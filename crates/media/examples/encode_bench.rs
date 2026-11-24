//! Measures software H.264 encode speed at each preset.
//!
//!     cargo run --release -p qchat-media --example encode_bench
//!
//! 1440p30 needs < 33 ms per frame (encode + RGBA→YUV conversion).

use std::time::Instant;

use qchat_media::video::{EncodeSettings, h264::H264Encoder, synthetic::pattern};

fn main() {
    for (name, s) in [
        ("1440p30", EncodeSettings::P1440),
        ("1080p30", EncodeSettings::P1080),
        ("720p30", EncodeSettings::P720),
    ] {
        let mut enc =
            H264Encoder::new(s.max_width, s.max_height, s.fps, s.bitrate_bps).expect("encoder");
        let frames: Vec<_> = (0..30)
            .map(|n| pattern(s.max_width, s.max_height, n))
            .collect();
        // Warm up.
        for f in frames.iter().take(5) {
            let _ = enc.encode(f);
        }
        let n = 90;
        let mut bytes = 0usize;
        let t0 = Instant::now();
        for i in 0..n {
            if let Ok(Some(e)) = enc.encode(&frames[i % frames.len()]) {
                bytes += e.data.len();
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let verdict = if ms < 1000.0 / f64::from(s.fps) {
            "OK"
        } else {
            "TOO SLOW"
        };
        println!(
            "{name}: {ms:6.1} ms/frame ({:5.1} fps max), {:5.2} Mb/s  [{verdict}]",
            1000.0 / ms,
            bytes as f64 * 8.0 / (n as f64 / f64::from(s.fps)) / 1e6
        );
    }
}
