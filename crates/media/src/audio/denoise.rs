//! RNNoise noise suppression via `nnnoiseless`.

use nnnoiseless::DenoiseState;

use super::DENOISE_FRAME;

/// Wraps an RNNoise state. Feed 48 kHz mono frames of [`DENOISE_FRAME`]
/// samples in [-1, 1].
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
    /// RNNoise's first output frame contains a fade-in artifact; skip it.
    warmed_up: bool,
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            scratch_in: vec![0.0; DENOISE_FRAME],
            scratch_out: vec![0.0; DENOISE_FRAME],
            warmed_up: false,
        }
    }

    /// Denoises `frame` in place and returns RNNoise's voice probability (0..1).
    ///
    /// # Panics
    /// If `frame.len() != DENOISE_FRAME`.
    pub fn process(&mut self, frame: &mut [f32]) -> f32 {
        assert_eq!(frame.len(), DENOISE_FRAME, "denoise frame size");
        // nnnoiseless works on i16-range floats.
        for (d, s) in self.scratch_in.iter_mut().zip(frame.iter()) {
            *d = s * 32768.0;
        }
        let vad = self
            .state
            .process_frame(&mut self.scratch_out, &self.scratch_in);
        if self.warmed_up {
            for (d, s) in frame.iter_mut().zip(self.scratch_out.iter()) {
                *d = (s / 32768.0).clamp(-1.0, 1.0);
            }
        } else {
            frame.fill(0.0);
            self.warmed_up = true;
        }
        vad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic low-frequency "fan/rumble" noise (low-passed white noise).
    fn fan_noise(n: usize, seed: &mut u32, lp: &mut f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 17;
                *seed ^= *seed << 5;
                let w = *seed as f32 / u32::MAX as f32 * 2.0 - 1.0;
                *lp += 0.02 * (w - *lp);
                *lp * 0.6
            })
            .collect()
    }

    fn energy(x: &[f32]) -> f64 {
        x.iter().map(|s| f64::from(*s).powi(2)).sum()
    }

    #[test]
    fn suppresses_fan_noise() {
        let mut d = Denoiser::new();
        let (mut seed, mut lp) = (0x1234_5678, 0.0);
        let (mut in_energy, mut out_energy) = (0.0, 0.0);
        let mut vad_sum = 0.0;
        // 3 seconds of noise; measure the last 1.5 s (after adaptation).
        for i in 0..300 {
            let mut f = fan_noise(DENOISE_FRAME, &mut seed, &mut lp);
            let e_in = energy(&f);
            let vad = d.process(&mut f);
            if i >= 150 {
                in_energy += e_in;
                out_energy += energy(&f);
                vad_sum += vad;
            }
        }
        let reduction_db = 10.0 * (in_energy / out_energy.max(1e-12)).log10();
        assert!(
            reduction_db > 20.0,
            "only {reduction_db:.1} dB of noise reduction"
        );
        assert!(
            vad_sum / 150.0 < 0.2,
            "noise must not be classified as speech"
        );
    }
}
