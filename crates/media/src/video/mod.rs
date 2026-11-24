//! Video: scaling, synthetic test source, H.264 codec and the publish/view
//! pipelines.

pub mod scale;
pub mod synthetic;

#[cfg(feature = "h264")]
pub mod h264;
#[cfg(feature = "h264")]
mod openh264_ffi;
#[cfg(feature = "h264")]
pub mod pipeline;

use std::time::Instant;

/// A captured frame, tightly packed RGBA8.
#[derive(Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub captured_at: Instant,
}

/// Target encode parameters (from a quality preset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeSettings {
    /// Bounding box: frames are scaled down (never up) to fit, keeping aspect.
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

impl EncodeSettings {
    pub const P1440: Self = Self {
        max_width: 2560,
        max_height: 1440,
        fps: 30,
        bitrate_bps: 8_000_000,
    };
    pub const P1080: Self = Self {
        max_width: 1920,
        max_height: 1080,
        fps: 30,
        bitrate_bps: 5_000_000,
    };
    pub const P720: Self = Self {
        max_width: 1280,
        max_height: 720,
        fps: 30,
        bitrate_bps: 2_500_000,
    };
}

/// Largest size that fits in `max_w`×`max_h`, keeps the aspect ratio of
/// `src_w`×`src_h`, never upscales, and has even dimensions (required by I420).
pub fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (2, 2);
    }
    let scale = (f64::from(max_w) / f64::from(src_w))
        .min(f64::from(max_h) / f64::from(src_h))
        .min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (
        even(f64::from(src_w) * scale),
        even(f64::from(src_h) * scale),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_even_dims() {
        assert_eq!(fit_within(3840, 2160, 2560, 1440), (2560, 1440));
        assert_eq!(fit_within(2560, 1440, 2560, 1440), (2560, 1440));
        assert_eq!(
            fit_within(1920, 1080, 2560, 1440),
            (1920, 1080),
            "no upscale"
        );
        assert_eq!(
            fit_within(3440, 1440, 2560, 1440),
            (2560, 1072),
            "ultrawide"
        );
        assert_eq!(fit_within(1001, 701, 4000, 4000), (1000, 700), "odd source");
        assert_eq!(fit_within(5120, 2880, 1280, 720), (1280, 720));
    }
}
