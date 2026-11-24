//! Transmit gate: voice activity detection with hangover, or push-to-talk.

/// How many 20 ms frames to keep transmitting after voice stops (avoids
/// clipping word endings).
const HANGOVER_FRAMES: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GateMode {
    /// Transmit when the level exceeds `threshold_db` (dBFS) or RNNoise is
    /// confident there is speech.
    VoiceActivity { threshold_db: f32 },
    /// Transmit only while the key is held.
    PushToTalk { pressed: bool },
}

pub struct Gate {
    pub mode: GateMode,
    pub muted: bool,
    hangover: u32,
}

impl Gate {
    pub fn new(mode: GateMode) -> Self {
        Self {
            mode,
            muted: false,
            hangover: 0,
        }
    }

    /// Decides whether to transmit a 20 ms frame.
    ///
    /// `level_db` is the frame level after noise suppression and
    /// `voice_prob` RNNoise's speech probability (use 0.0 when disabled).
    pub fn transmit(&mut self, level_db: f32, voice_prob: f32) -> bool {
        if self.muted {
            self.hangover = 0;
            return false;
        }
        match self.mode {
            GateMode::PushToTalk { pressed } => {
                if pressed {
                    self.hangover = 3; // short tail after release
                    true
                } else if self.hangover > 0 {
                    self.hangover -= 1;
                    true
                } else {
                    false
                }
            }
            GateMode::VoiceActivity { threshold_db } => {
                let active =
                    level_db > threshold_db && (voice_prob > 0.5 || level_db > threshold_db + 15.0);
                if active {
                    self.hangover = HANGOVER_FRAMES;
                    true
                } else if self.hangover > 0 {
                    self.hangover -= 1;
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vad_with_hangover() {
        let mut g = Gate::new(GateMode::VoiceActivity {
            threshold_db: -50.0,
        });
        assert!(!g.transmit(-70.0, 0.0));
        assert!(g.transmit(-30.0, 0.9));
        for _ in 0..HANGOVER_FRAMES {
            assert!(g.transmit(-80.0, 0.0));
        }
        assert!(!g.transmit(-80.0, 0.0));
        // Loud but non-speech (e.g. noise burst) below +15 dB margin: no.
        assert!(!g.transmit(-45.0, 0.1));
    }

    #[test]
    fn mute_wins() {
        let mut g = Gate::new(GateMode::PushToTalk { pressed: true });
        g.muted = true;
        assert!(!g.transmit(0.0, 1.0));
    }

    #[test]
    fn ptt_short_tail() {
        let mut g = Gate::new(GateMode::PushToTalk { pressed: true });
        assert!(g.transmit(-90.0, 0.0));
        g.mode = GateMode::PushToTalk { pressed: false };
        assert!(g.transmit(-90.0, 0.0));
        assert!(g.transmit(-90.0, 0.0));
        assert!(g.transmit(-90.0, 0.0));
        assert!(!g.transmit(-90.0, 0.0));
    }
}
