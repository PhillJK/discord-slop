//! Screen capture behind the [`ScreenCapturer`] trait.
//!
//! Backends (feature `capture`): on Wayland our own xdg-desktop-portal
//! ScreenCast + PipeWire backend ([`pipewire_backend`]); elsewhere `xcap`
//! (XCB on X11, Windows Graphics Capture / DXGI, AVFoundation on macOS). The synthetic
//! backend produces a test pattern and is always available.

#[cfg(all(feature = "capture", target_os = "linux"))]
pub mod pipewire_backend;
pub mod synthetic;
#[cfg(feature = "capture")]
pub mod xcap_backend;

use crossbeam_channel::Receiver;

use crate::{MediaResult, video::RawFrame};

/// Id of the built-in synthetic test source.
pub const SYNTHETIC_ID: &str = "synthetic";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSource {
    /// Stable id for [`start`].
    pub id: String,
    /// Human-readable name.
    pub name: String,
}

/// A running capture. Frames arrive on `frames` (bounded; when the consumer
/// is slow, frames are dropped at the source). Dropping the handle stops it.
pub struct CaptureStream {
    pub frames: Receiver<RawFrame>,
    _guard: Box<dyn Send>,
}

impl CaptureStream {
    pub fn new(frames: Receiver<RawFrame>, guard: Box<dyn Send>) -> Self {
        Self {
            frames,
            _guard: guard,
        }
    }
}

pub trait ScreenCapturer {
    fn sources(&self) -> Vec<CaptureSource>;
    fn start(&self, source_id: &str, fps: u32) -> MediaResult<CaptureStream>;
}

/// Lists all sources of all compiled-in backends. The synthetic source is
/// included when `QCHAT_SYNTHETIC_SOURCE` is set or no real source exists.
pub fn sources() -> Vec<CaptureSource> {
    #[allow(unused_mut)]
    let mut out: Vec<CaptureSource> = Vec::new();
    #[cfg(all(feature = "capture", target_os = "linux"))]
    if pipewire_backend::is_wayland() {
        // On Wayland the portal picker chooses the screen/window.
        out.extend(pipewire_backend::PortalCapturer.sources());
    } else {
        out.extend(xcap_backend::XcapCapturer.sources());
    }
    #[cfg(all(feature = "capture", not(target_os = "linux")))]
    out.extend(xcap_backend::XcapCapturer.sources());
    if out.is_empty() || std::env::var_os("QCHAT_SYNTHETIC_SOURCE").is_some() {
        out.extend(synthetic::SyntheticCapturer::default().sources());
    }
    out
}

/// Starts capturing `source_id` (an id from [`sources`]).
pub fn start(source_id: &str, fps: u32) -> MediaResult<CaptureStream> {
    if source_id == SYNTHETIC_ID {
        return synthetic::SyntheticCapturer::default().start(source_id, fps);
    }
    #[cfg(all(feature = "capture", target_os = "linux"))]
    if source_id == pipewire_backend::PORTAL_ID {
        return pipewire_backend::PortalCapturer.start(source_id, fps);
    }
    #[cfg(feature = "capture")]
    {
        xcap_backend::XcapCapturer.start(source_id, fps)
    }
    #[cfg(not(feature = "capture"))]
    {
        Err(crate::MediaError::Unsupported("screen capture"))
    }
}
