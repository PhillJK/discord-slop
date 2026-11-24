//! Times each stage of the share pipeline on a 2560x1600 frame
//! (GNOME fractional-scaling size) scaled into the 1440p box.
use std::time::Instant;

use openh264::formats::{RgbaSliceU8, YUVBuffer};
use qchat_media::video::{fit_within, h264::H264Encoder, scale::Scaler, synthetic::pattern};

fn main() {
    let (sw, sh) = (2560, 1600);
    let (w, h) = fit_within(sw, sh, 2560, 1440);
    let mut scaler = Scaler::new();
    let mut yuv = YUVBuffer::new(w as usize, h as usize);
    let mut enc = H264Encoder::new(w, h, 30, 8_000_000).expect("enc");
    let n = 40;
    let frames: Vec<_> = (0..n).map(|i| pattern(sw, sh, i)).collect();
    let (mut ts, mut ty, mut te) = (0.0, 0.0, 0.0);
    for f in frames {
        let t = Instant::now();
        let s = scaler.scale(f, w, h).expect("scale");
        ts += t.elapsed().as_secs_f64();
        let t = Instant::now();
        yuv.read_rgba8(RgbaSliceU8::new(&s.rgba, (w as usize, h as usize)));
        ty += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let _ = enc.encode(&s);
        te += t.elapsed().as_secs_f64();
    }
    let ms = |x: f64| x * 1000.0 / f64::from(n);
    println!(
        "{sw}x{sh} -> {w}x{h}: scale {:.1} ms, rgba->yuv {:.1} ms, encode(incl. yuv) {:.1} ms",
        ms(ts),
        ms(ty),
        ms(te)
    );
}
