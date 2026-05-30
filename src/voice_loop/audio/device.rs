use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use super::buffer::{fill_output_samples, PlaybackBuffer};
use super::convert::f32_to_i16;
use super::AudioChunk;

pub(crate) fn start_input_stream(
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
    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!(
            "unsupported input sample format: {:?} (only F32 is supported)",
            config.sample_format()
        ));
    }
    let err_fn = |err| eprintln!("input stream error: {err}");

    let stream = device
        .build_input_stream(
            &config.into(),
            move |data: &[f32], _| send_input_samples(data, channels, sample_rate, &tx),
            err_fn,
            None,
        )
        .map_err(|e| format!("failed to build input stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("failed to start input stream: {e}"))?;
    Ok(stream)
}

pub(crate) fn start_output_stream(
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
    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!(
            "unsupported output sample format: {:?} (only F32 is supported)",
            config.sample_format()
        ));
    }
    let err_fn = |err| eprintln!("output stream error: {err}");

    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _| fill_output_samples(data, channels, &queue),
            err_fn,
            None,
        )
        .map_err(|e| format!("failed to build output stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("failed to start output stream: {e}"))?;
    Ok((stream, sample_rate))
}

fn send_input_samples(
    data: &[f32],
    channels: usize,
    sample_rate: u32,
    tx: &mpsc::UnboundedSender<AudioChunk>,
) {
    let mut mono = Vec::with_capacity(data.len() / channels.max(1));
    for frame in data.chunks(channels.max(1)) {
        mono.push(f32_to_i16(frame[0]));
    }
    let _ = tx.send(AudioChunk {
        samples: mono,
        sample_rate,
    });
}
