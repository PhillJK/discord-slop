//! Cross-platform capture via `xcap`'s monitor video recorder:
//! Wayland (xdg-desktop-portal ScreenCast + PipeWire; the system picker lets the
//! user choose a monitor or window), X11 (XCB), Windows (WGC/DXGI) and macOS.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use xcap::Monitor;

use super::{CaptureSource, CaptureStream, ScreenCapturer};
use crate::{MediaError, MediaResult, video::RawFrame};

const PREFIX: &str = "xcap:";

pub struct XcapCapturer;

fn cap_err(e: impl std::fmt::Display) -> MediaError {
    MediaError::Capture(e.to_string())
}

/// Stops the capture thread when the stream is dropped. The recorder itself
/// lives on that thread: on macOS it wraps an `AVCaptureSession`, which is
/// neither `Send` nor `Sync`, so it must be created, run and stopped there.
struct Guard {
    stop: Arc<AtomicBool>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl ScreenCapturer for XcapCapturer {
    fn sources(&self) -> Vec<CaptureSource> {
        let Ok(monitors) = Monitor::all() else {
            return Vec::new();
        };
        monitors
            .iter()
            .filter_map(|m| {
                let id = m.id().ok()?;
                let name = m.name().unwrap_or_else(|_| format!("Monitor {id}"));
                let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
                let primary = if m.is_primary().unwrap_or(false) {
                    " — primary"
                } else {
                    ""
                };
                Some(CaptureSource {
                    id: format!("{PREFIX}{id}"),
                    name: format!("{name} ({w}×{h}){primary}"),
                })
            })
            .collect()
    }

    fn start(&self, source_id: &str, _fps: u32) -> MediaResult<CaptureStream> {
        let wanted: Option<u32> = source_id.strip_prefix(PREFIX).and_then(|s| s.parse().ok());
        let (tx, rx) = crossbeam_channel::bounded(2);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<MediaResult<()>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("capture-xcap".into())
            .spawn(move || {
                let (recorder, frames) = match open_recorder(wanted) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                while !stop2.load(Ordering::Relaxed) {
                    let f = match frames.recv_timeout(Duration::from_millis(200)) {
                        Ok(f) => f,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if let Some(frame) = to_raw(f) {
                        // Consumer is behind: drop this frame (it'll take the next).
                        let _ = tx.try_send(frame);
                    }
                }
                if let Err(e) = recorder.stop() {
                    tracing::debug!(error = %e, "stopping recorder");
                }
            })
            .map_err(cap_err)?;
        ready_rx
            .recv()
            .map_err(|_| MediaError::Capture("capture thread exited during startup".into()))??;
        Ok(CaptureStream::new(rx, Box::new(Guard { stop })))
    }
}

/// Finds the requested monitor (falling back to the primary, then the first)
/// and starts recording it. Runs on the capture thread.
fn open_recorder(
    wanted: Option<u32>,
) -> MediaResult<(xcap::VideoRecorder, std::sync::mpsc::Receiver<xcap::Frame>)> {
    let monitors = Monitor::all().map_err(cap_err)?;
    let monitor = monitors
        .iter()
        .find(|m| wanted.is_some() && m.id().ok() == wanted)
        .or_else(|| monitors.iter().find(|m| m.is_primary().unwrap_or(false)))
        .or_else(|| monitors.first())
        .ok_or_else(|| MediaError::Capture("no monitor found".into()))?;
    let (recorder, frames) = monitor.video_recorder().map_err(cap_err)?;
    recorder.start().map_err(cap_err)?;
    Ok((recorder, frames))
}

/// Converts an xcap frame into tightly packed RGBA, removing any row padding.
fn to_raw(f: xcap::Frame) -> Option<RawFrame> {
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let packed = w * h * 4;
    let rgba = if f.raw.len() == packed {
        f.raw
    } else {
        let stride = f.raw.len() / h;
        if stride < w * 4 {
            return None;
        }
        let mut out = Vec::with_capacity(packed);
        for row in f.raw.chunks_exact(stride).take(h) {
            out.extend_from_slice(&row[..w * 4]);
        }
        out
    };
    Some(RawFrame {
        width: f.width,
        height: f.height,
        rgba,
        captured_at: Instant::now(),
    })
}
