//! Voice pipeline.
//!
//! Capture: device → mono f32 → resample to 48 kHz → RNNoise (480-sample frames)
//! → VAD / push-to-talk gate → Opus (960-sample / 20 ms frames) → datagram.
//!
//! Playback: datagram → per-speaker jitter buffer → Opus decode (FEC/PLC on
//! loss) → mix → resample to device rate → device.

pub mod denoise;
pub mod jitter;
pub mod mixer;
pub mod resample;
pub mod vad;

#[cfg(feature = "opus")]
pub mod codec;
#[cfg(feature = "audio-io")]
pub mod device;
#[cfg(all(feature = "audio-io", feature = "opus"))]
pub mod engine;

/// Internal sample rate of the voice pipeline.
pub const SAMPLE_RATE: u32 = 48_000;
/// One Opus packet: 20 ms at 48 kHz, mono.
pub const FRAME_SAMPLES: usize = 960;
/// One RNNoise frame: 10 ms at 48 kHz.
pub const DENOISE_FRAME: usize = nnnoiseless::FRAME_SIZE;

const _: () = assert!(FRAME_SAMPLES == 2 * DENOISE_FRAME);

/// RMS level of a frame in dBFS (samples in [-1, 1]).
pub fn level_dbfs(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -120.0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    let rms = (sum / frame.len() as f32).sqrt();
    if rms <= 1e-6 {
        -120.0
    } else {
        20.0 * rms.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_of_full_scale_square_is_zero_db() {
        let f = vec![1.0f32; 480];
        assert!(level_dbfs(&f).abs() < 0.01);
        assert!(level_dbfs(&[0.0; 10]) <= -119.0);
        let half = vec![0.5f32; 480];
        assert!((level_dbfs(&half) + 6.02).abs() < 0.05);
    }
}
