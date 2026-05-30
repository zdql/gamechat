mod buffer;
mod convert;
mod device;
mod resample;

pub(super) use buffer::{enqueue_audio_delta, playback_depth_ms, PlaybackBuffer};
pub(super) use device::{start_input_stream, start_output_stream};
pub(super) use resample::{i16_to_le_bytes, resample_i16};

pub(super) struct AudioChunk {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}
