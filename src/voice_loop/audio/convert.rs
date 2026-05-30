//! Sample conversion between the device's `f32` format and the API's `i16` PCM.
//!
//! The cpal device speaks `f32`; the OpenAI Realtime API speaks 16-bit PCM.
//! These are the only two points where those representations meet.

/// Device float (`-1.0..=1.0`) to 16-bit PCM, on the capture path.
pub(super) fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// 16-bit PCM to device float (`-1.0..=1.0`), on the playback path.
pub(super) fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / i16::MAX as f32
}
