//! H.264 encode/decode via openh264 (Cisco), compiled from source.

use openh264::{
    OpenH264API,
    decoder::{Decoder, DecoderConfig},
    formats::{RgbaSliceU8, YUVBuffer, YUVSource},
};

use super::RawFrame;
use super::openh264_ffi::{EncoderParams, Planes, RawEncoder};
use crate::{MediaError, MediaResult};

/// Keyframe interval (also forced on request, e.g. when a viewer joins).
pub const KEYFRAME_INTERVAL_SECS: u32 = 3;

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
}

pub struct H264Encoder {
    encoder: RawEncoder,
    width: u32,
    height: u32,
    yuv: YUVBuffer,
    started: std::time::Instant,
}

fn codec_err(e: impl std::fmt::Display) -> MediaError {
    MediaError::Codec(e.to_string())
}

impl H264Encoder {
    /// Creates an encoder for frames of exactly `width`×`height` (even).
    /// Uses up to 8 threads, one slice per thread.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> MediaResult<Self> {
        let threads = std::thread::available_parallelism()
            .map_or(2, |n| n.get())
            .clamp(1, 8) as u16;
        let encoder = RawEncoder::new(&EncoderParams {
            width,
            height,
            fps,
            bitrate_bps,
            intra_period_frames: fps * KEYFRAME_INTERVAL_SECS,
            threads,
        })
        .map_err(MediaError::Codec)?;
        Ok(Self {
            encoder,
            width,
            height,
            yuv: YUVBuffer::new(width as usize, height as usize),
            started: std::time::Instant::now(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Makes the next encoded frame an IDR keyframe.
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Encodes one frame (must match the encoder size). Returns `None` if the
    /// rate controller skipped the frame.
    pub fn encode(&mut self, frame: &RawFrame) -> MediaResult<Option<EncodedFrame>> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(MediaError::Codec(format!(
                "frame is {}x{}, encoder expects {}x{}",
                frame.width, frame.height, self.width, self.height
            )));
        }
        self.yuv.read_rgba8(RgbaSliceU8::new(
            &frame.rgba,
            (self.width as usize, self.height as usize),
        ));
        let planes = Planes {
            y: self.yuv.y(),
            u: self.yuv.u(),
            v: self.yuv.v(),
            strides: self.yuv.strides(),
        };
        let ts = self.started.elapsed().as_millis() as i64;
        Ok(self
            .encoder
            .encode(&planes, ts)
            .map_err(MediaError::Codec)?
            .map(|(data, keyframe)| EncodedFrame {
                data,
                keyframe,
                width: self.width,
                height: self.height,
            }))
    }
}

pub struct H264Decoder {
    decoder: Decoder,
}

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl H264Decoder {
    pub fn new() -> MediaResult<Self> {
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(codec_err)?;
        Ok(Self { decoder })
    }

    /// Decodes one access unit (Annex-B). `Ok(None)` means no picture yet.
    pub fn decode(&mut self, data: &[u8]) -> MediaResult<Option<DecodedFrame>> {
        let Some(yuv) = self.decoder.decode(data).map_err(codec_err)? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let mut rgba = vec![0u8; w * h * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(DecodedFrame {
            width: w as u32,
            height: h as u32,
            rgba,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::synthetic::pattern;

    #[test]
    fn encode_decode_1440p_roundtrip() {
        let (w, h) = (2560, 1440);
        let mut enc = H264Encoder::new(w, h, 30, 8_000_000).expect("encoder");
        let mut dec = H264Decoder::new().expect("decoder");
        let mut decoded = 0;
        let mut keyframes = 0;
        for n in 0..10 {
            if n == 5 {
                enc.force_keyframe();
            }
            let f = pattern(w, h, n);
            let Some(e) = enc.encode(&f).expect("encode") else {
                continue;
            };
            if e.keyframe {
                keyframes += 1;
            }
            if let Some(d) = dec.decode(&e.data).expect("decode") {
                assert_eq!((d.width, d.height), (w, h));
                // Compare a pixel in the top-left colour bar (lossy codec).
                let src = &f.rgba[(100 * w as usize + 50) * 4..][..3];
                let got = &d.rgba[(100 * w as usize + 50) * 4..][..3];
                for (a, b) in src.iter().zip(got) {
                    assert!(a.abs_diff(*b) < 24, "pixel {src:?} vs {got:?}");
                }
                decoded += 1;
            }
        }
        assert!(decoded >= 8, "decoded only {decoded} frames");
        assert!(
            keyframes >= 2,
            "first frame and forced frame must be keyframes"
        );
    }
}
