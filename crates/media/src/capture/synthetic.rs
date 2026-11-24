use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::{CaptureSource, CaptureStream, SYNTHETIC_ID, ScreenCapturer};
use crate::{MediaResult, video::synthetic::pattern};

/// Produces a moving test pattern at the requested fps.
pub struct SyntheticCapturer {
    pub width: u32,
    pub height: u32,
}

impl Default for SyntheticCapturer {
    fn default() -> Self {
        Self {
            width: 2560,
            height: 1440,
        }
    }
}

struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn sources(&self) -> Vec<CaptureSource> {
        vec![CaptureSource {
            id: SYNTHETIC_ID.into(),
            name: format!("Test pattern ({}×{})", self.width, self.height),
        }]
    }

    fn start(&self, _source_id: &str, fps: u32) -> MediaResult<CaptureStream> {
        let (tx, rx) = crossbeam_channel::bounded(2);
        let stop = Arc::new(AtomicBool::new(false));
        let (w, h) = (self.width, self.height);
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("capture-synthetic".into())
            .spawn(move || {
                let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
                let mut next = Instant::now();
                let mut n = 0u32;
                while !stop2.load(Ordering::Relaxed) {
                    // Drop when the consumer lags, like a real capture source.
                    let _ = tx.try_send(pattern(w, h, n));
                    n = n.wrapping_add(1);
                    next += interval;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            })
            .map_err(|e| crate::MediaError::Capture(e.to_string()))?;
        Ok(CaptureStream::new(rx, Box::new(StopOnDrop(stop))))
    }
}
