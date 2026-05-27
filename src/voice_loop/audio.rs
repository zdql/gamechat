use base64::Engine;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub(super) struct AudioChunk {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

pub(super) struct PlaybackBuffer {
    samples: VecDeque<f32>,
    started: bool,
    start_threshold_samples: usize,
    underruns: u64,
}

impl PlaybackBuffer {
    pub(super) fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            started: false,
            start_threshold_samples: 4_800,
            underruns: 0,
        }
    }
}

pub(super) fn start_input_stream(
    tx: mpsc::UnboundedSender<AudioChunk>,
) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no default input audio device")?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("failed to read input config: {e}"))?;
    let sample_rate = config.sample_rate().0;
    let channels = usize::from(config.channels());
    eprintln!(
        "input: {} @ {} Hz, {} channel(s)",
        device.name().unwrap_or_else(|_| "unknown".to_string()),
        sample_rate,
        channels
    );
    let err_fn = |err| eprintln!("input stream error: {err}");

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| send_input_samples(data, channels, sample_rate, &tx),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _| send_input_samples(data, channels, sample_rate, &tx),
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config.into(),
            move |data: &[u16], _| send_input_samples(data, channels, sample_rate, &tx),
            err_fn,
            None,
        ),
        other => return Err(format!("unsupported input sample format: {other:?}")),
    }
    .map_err(|e| format!("failed to build input stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("failed to start input stream: {e}"))?;
    Ok(stream)
}

pub(super) fn start_output_stream(
    queue: Arc<Mutex<PlaybackBuffer>>,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output audio device")?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("failed to read output config: {e}"))?;
    let sample_rate = config.sample_rate().0;
    let channels = usize::from(config.channels());
    eprintln!(
        "output: {} @ {} Hz, {} channel(s)",
        device.name().unwrap_or_else(|_| "unknown".to_string()),
        sample_rate,
        channels
    );
    if let Ok(mut guard) = queue.lock() {
        guard.start_threshold_samples = (sample_rate as usize * 3 / 10).max(1);
        eprintln!(
            "output jitter buffer threshold: {} ms",
            guard.start_threshold_samples * 1_000 / sample_rate as usize
        );
    }
    let err_fn = |err| eprintln!("output stream error: {err}");

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config.into(),
            move |data: &mut [f32], _| fill_output_samples(data, channels, &queue),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config.into(),
            move |data: &mut [i16], _| fill_output_samples(data, channels, &queue),
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &config.into(),
            move |data: &mut [u16], _| fill_output_samples(data, channels, &queue),
            err_fn,
            None,
        ),
        other => return Err(format!("unsupported output sample format: {other:?}")),
    }
    .map_err(|e| format!("failed to build output stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("failed to start output stream: {e}"))?;
    Ok((stream, sample_rate))
}

// Returns the number of source (24 kHz) PCM samples decoded from `delta`, so
// the caller can track how much assistant audio has been enqueued for
// conversation.item.truncate accounting on barge-in.
pub(super) fn enqueue_audio_delta(
    delta: &str,
    queue: Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Result<usize, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(delta)
        .map_err(|e| format!("invalid audio delta base64: {e}"))?;
    let mut pcm = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let source_samples = pcm.len();
    let resampled = resample_i16(&pcm, 24_000, output_rate);
    let mut guard = queue.lock().map_err(|_| "playback queue poisoned")?;
    guard
        .samples
        .extend(resampled.into_iter().map(|s| s as f32 / i16::MAX as f32));
    Ok(source_samples)
}

pub(super) fn playback_depth_ms(
    queue: &Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Option<usize> {
    if output_rate == 0 {
        return None;
    }
    let guard = queue.lock().ok()?;
    Some(guard.samples.len() * 1_000 / output_rate as usize)
}

// Drop any queued playback audio. Used on barge-in so we stop voicing the
// (now-cancelled) assistant turn the moment the user starts speaking.
pub(super) fn clear_playback(queue: &Arc<Mutex<PlaybackBuffer>>) {
    if let Ok(mut guard) = queue.lock() {
        guard.samples.clear();
        guard.started = false;
    }
}

// Attenuate mic samples by `gain` (0.0..=1.0). Used to duck the mic while the
// assistant is speaking so speaker leakage doesn't trip the server-side VAD,
// while leaving real user speech loud enough to barge in.
pub(super) fn duck_samples(samples: &[i16], gain: f32) -> Vec<i16> {
    let gain = gain.clamp(0.0, 1.0);
    samples
        .iter()
        .map(|s| (*s as f32 * gain).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect()
}

pub(super) fn i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

pub(super) fn resample_i16(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if samples.is_empty() || from_rate == 0 || to_rate == 0 {
        return Vec::new();
    }
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let out_len = (samples.len() as u64 * to_rate as u64 / from_rate as u64).max(1) as usize;
    let ratio = from_rate as f64 / to_rate as f64;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let lo = src.floor() as usize;
        let hi = (lo + 1).min(samples.len() - 1);
        let frac = src - lo as f64;
        let mixed = samples[lo] as f64 * (1.0 - frac) + samples[hi] as f64 * frac;
        out.push(mixed.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}

fn send_input_samples<T>(
    data: &[T],
    channels: usize,
    sample_rate: u32,
    tx: &mpsc::UnboundedSender<AudioChunk>,
) where
    T: Copy + IntoSampleI16,
{
    let mut mono = Vec::with_capacity(data.len() / channels.max(1));
    for frame in data.chunks(channels.max(1)) {
        mono.push(frame[0].into_i16());
    }
    let _ = tx.send(AudioChunk {
        samples: mono,
        sample_rate,
    });
}

fn fill_output_samples<T>(data: &mut [T], channels: usize, queue: &Arc<Mutex<PlaybackBuffer>>)
where
    T: FromF32,
{
    let mut guard = queue.lock().expect("playback queue poisoned");
    if !guard.started && guard.samples.len() >= guard.start_threshold_samples {
        guard.started = true;
    }

    for frame in data.chunks_mut(channels.max(1)) {
        let sample = if guard.started {
            match guard.samples.pop_front() {
                Some(sample) => sample,
                None => {
                    guard.started = false;
                    guard.underruns += 1;
                    if guard.underruns <= 5 || guard.underruns % 25 == 0 {
                        eprintln!("playback underrun count={}", guard.underruns);
                    }
                    0.0
                }
            }
        } else {
            0.0
        };
        for out in frame.iter_mut() {
            *out = T::from_f32(sample);
        }
    }
}

trait IntoSampleI16 {
    fn into_i16(self) -> i16;
}

impl IntoSampleI16 for i16 {
    fn into_i16(self) -> i16 {
        self
    }
}

impl IntoSampleI16 for u16 {
    fn into_i16(self) -> i16 {
        (self as i32 - 32768).clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }
}

impl IntoSampleI16 for f32 {
    fn into_i16(self) -> i16 {
        (self.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    }
}

trait FromF32 {
    fn from_f32(value: f32) -> Self;
}

impl FromF32 for f32 {
    fn from_f32(value: f32) -> Self {
        value
    }
}

impl FromF32 for i16 {
    fn from_f32(value: f32) -> Self {
        (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    }
}

impl FromF32 for u16 {
    fn from_f32(value: f32) -> Self {
        ((value.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duck_samples_zero_gain_silences() {
        let input = vec![1000_i16, -2000, 3000, -4000];
        let out = duck_samples(&input, 0.0);
        assert!(out.iter().all(|s| *s == 0));
    }

    #[test]
    fn duck_samples_unity_gain_passthrough() {
        let input = vec![1000_i16, -2000, 3000, -4000];
        let out = duck_samples(&input, 1.0);
        assert_eq!(out, input);
    }

    #[test]
    fn duck_samples_half_gain_attenuates() {
        let input = vec![10_000_i16, -10_000];
        let out = duck_samples(&input, 0.5);
        // Allow ±1 for f32 rounding.
        assert!((out[0] - 5000).abs() <= 1, "got {}", out[0]);
        assert!((out[1] + 5000).abs() <= 1, "got {}", out[1]);
    }

    #[test]
    fn duck_samples_clamps_gain_above_one() {
        // Negative gain or gain >1.0 must not panic or saturate weirdly.
        let input = vec![10_000_i16];
        let out_high = duck_samples(&input, 5.0);
        assert_eq!(out_high, input, "gain >1.0 should clamp to 1.0");
        let out_neg = duck_samples(&input, -1.0);
        assert_eq!(out_neg, vec![0], "negative gain should clamp to 0.0");
    }

    #[test]
    fn clear_playback_resets_buffer_and_start_flag() {
        let buf = Arc::new(Mutex::new(PlaybackBuffer::new()));
        {
            let mut guard = buf.lock().unwrap();
            guard.samples.extend([0.1_f32, 0.2, 0.3]);
            guard.started = true;
        }
        clear_playback(&buf);
        let guard = buf.lock().unwrap();
        assert!(guard.samples.is_empty());
        assert!(!guard.started);
    }
}
