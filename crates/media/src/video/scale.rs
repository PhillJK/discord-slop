//! RGBA downscaling with `fast_image_resize` (SIMD).

use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image};

use super::RawFrame;

pub struct Scaler {
    resizer: Resizer,
    options: ResizeOptions,
}

impl Default for Scaler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scaler {
    pub fn new() -> Self {
        Self {
            resizer: Resizer::new(),
            options: ResizeOptions::new()
                .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
                .use_alpha(false),
        }
    }

    /// Returns `frame` resized to exactly `w`×`h` (moved through unchanged if
    /// it already has that size).
    pub fn scale(&mut self, frame: RawFrame, w: u32, h: u32) -> Result<RawFrame, String> {
        if frame.width == w && frame.height == h {
            return Ok(frame);
        }
        let src = Image::from_vec_u8(frame.width, frame.height, frame.rgba, PixelType::U8x4)
            .map_err(|e| e.to_string())?;
        let mut dst = Image::new(w, h, PixelType::U8x4);
        self.resizer
            .resize(&src, &mut dst, &self.options)
            .map_err(|e| e.to_string())?;
        Ok(RawFrame {
            width: w,
            height: h,
            rgba: dst.into_vec(),
            captured_at: frame.captured_at,
        })
    }

    /// Crops one column/row if needed so both dimensions are even.
    pub fn make_even(frame: RawFrame) -> RawFrame {
        let (w, h) = (frame.width & !1, frame.height & !1);
        if w == frame.width && h == frame.height {
            return frame;
        }
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        let stride = frame.width as usize * 4;
        for row in frame.rgba.chunks_exact(stride).take(h as usize) {
            out.extend_from_slice(&row[..w as usize * 4]);
        }
        RawFrame {
            width: w,
            height: h,
            rgba: out,
            captured_at: frame.captured_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RawFrame {
        RawFrame {
            width: w,
            height: h,
            rgba: px
                .iter()
                .copied()
                .cycle()
                .take((w * h * 4) as usize)
                .collect(),
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn downscales_4k_to_1440p() {
        let mut s = Scaler::new();
        let out = s
            .scale(solid(3840, 2160, [10, 200, 30, 255]), 2560, 1440)
            .expect("scale");
        assert_eq!((out.width, out.height), (2560, 1440));
        assert_eq!(out.rgba.len(), 2560 * 1440 * 4);
        assert_eq!(&out.rgba[..4], &[10, 200, 30, 255]);
    }

    #[test]
    fn make_even_crops() {
        let f = Scaler::make_even(solid(5, 3, [1, 2, 3, 4]));
        assert_eq!((f.width, f.height), (4, 2));
        assert_eq!(f.rgba.len(), 4 * 2 * 4);
    }
}
