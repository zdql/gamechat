use base64::Engine;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::convert::i16_to_f32;
use super::resample::resample_i16;

pub(crate) struct PlaybackBuffer {
    pub(super) samples: VecDeque<f32>,
    pub(super) started: bool,
    pub(super) start_threshold_samples: usize,
    pub(super) underruns: u64,
}

impl PlaybackBuffer {
    pub(crate) fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            started: false,
            start_threshold_samples: 4_800,
            underruns: 0,
        }
    }
}

pub(crate) fn enqueue_audio_delta(
    delta: &str,
    queue: Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Result<(), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(delta)
        .map_err(|e| format!("invalid audio delta base64: {e}"))?;
    let mut pcm = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let resampled = resample_i16(&pcm, 24_000, output_rate);
    let mut guard = queue.lock().map_err(|_| "playback queue poisoned")?;
    guard
        .samples
        .extend(resampled.into_iter().map(i16_to_f32));
    Ok(())
}

pub(crate) fn playback_depth_ms(
    queue: &Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Option<usize> {
    if output_rate == 0 {
        return None;
    }
    let guard = queue.lock().ok()?;
    Some(guard.samples.len() * 1_000 / output_rate as usize)
}

pub(super) fn fill_output_samples(
    data: &mut [f32],
    channels: usize,
    queue: &Arc<Mutex<PlaybackBuffer>>,
) {
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
            *out = sample;
        }
    }
}
