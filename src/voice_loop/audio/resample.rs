pub(crate) fn i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

pub(crate) fn resample_i16(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
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
