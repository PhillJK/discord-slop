//! Small streaming resampler (linear interpolation with a one-pole
//! anti-aliasing low-pass when downsampling). Devices are opened at 48 kHz
//! whenever they support it, so this is only a fallback path.

pub struct Resampler {
    from: u32,
    to: u32,
    /// Position of the next output sample, in input samples, relative to `prev`.
    pos: f64,
    prev: f32,
    lp_state: f32,
    lp_alpha: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        // One-pole low-pass at ~0.45 * target Nyquist when downsampling.
        let lp_alpha = if to < from {
            let fc = 0.45 * to as f32;
            let dt = 1.0 / from as f32;
            let rc = 1.0 / (2.0 * std::f32::consts::PI * fc);
            dt / (rc + dt)
        } else {
            1.0
        };
        Self {
            from,
            to,
            pos: 0.0,
            prev: 0.0,
            lp_state: 0.0,
            lp_alpha,
        }
    }

    pub fn is_passthrough(&self) -> bool {
        self.from == self.to
    }

    /// Resamples `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        let step = f64::from(self.from) / f64::from(self.to);
        for &raw in input {
            self.lp_state += self.lp_alpha * (raw - self.lp_state);
            let cur = self.lp_state;
            // Emit all output samples that fall between prev (0.0) and cur (1.0).
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.prev + (cur - self.prev) * t);
                self.pos += step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn output_length_matches_ratio() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 48_000),
            (16_000, 48_000),
        ] {
            let mut r = Resampler::new(from, to);
            let mut out = Vec::new();
            let chunk = vec![0.0f32; 441];
            for _ in 0..100 {
                r.process(&chunk, &mut out);
            }
            let expected = 44_100.0 * f64::from(to) / f64::from(from);
            let got = out.len() as f64;
            assert!(
                (got - expected).abs() <= 2.0,
                "{from}->{to}: {got} vs {expected}"
            );
        }
    }

    #[test]
    fn preserves_low_frequency_sine() {
        let mut r = Resampler::new(44_100, 48_000);
        let input: Vec<f32> = (0..44_100)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44_100.0).sin() * 0.5)
            .collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let peak = out[1000..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.02, "peak {peak}");
    }
}
