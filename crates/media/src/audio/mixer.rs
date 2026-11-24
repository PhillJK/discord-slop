//! Mixing of decoded speaker frames.

/// Adds `src` into `acc`.
pub fn accumulate(acc: &mut [f32], src: &[f32]) {
    for (a, s) in acc.iter_mut().zip(src) {
        *a += s;
    }
}

/// Soft clipper: linear below 0.8, smoothly saturating to ±1 above.
pub fn soft_clip(buf: &mut [f32]) {
    for s in buf {
        let x = *s;
        let a = x.abs();
        if a > 0.8 {
            let over = a - 0.8;
            let y = 0.8 + 0.2 * (over / 0.2).tanh();
            *s = y.copysign(x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_and_clipping() {
        let mut acc = vec![0.0f32; 4];
        accumulate(&mut acc, &[0.5, 0.5, -0.9, 0.1]);
        accumulate(&mut acc, &[0.6, 0.1, -0.9, 0.1]);
        soft_clip(&mut acc);
        assert!(acc[0] < 1.0 && acc[0] > 0.8);
        assert!((acc[1] - 0.6).abs() < 1e-6);
        assert!(acc[2] > -1.0 && acc[2] < -0.8);
        assert!((acc[3] - 0.2).abs() < 1e-6);
    }
}
