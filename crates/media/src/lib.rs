//! qchat media pipelines.
//!
//! Pure-Rust building blocks (always available):
//! * [`audio::denoise`]: RNNoise noise suppression (`nnnoiseless`)
//! * [`audio::vad`], [`audio::jitter`], [`audio::resample`], [`audio::mixer`]
//! * [`video::scale`], [`video::synthetic`], [`capture::synthetic`]
//!
//! Native backends behind features:
//! * `audio-io` + `opus`: [`audio::engine::VoiceEngine`]
//! * `h264`: [`video::h264`] encoder/decoder and the publish/view pipelines
//! * `capture`: [`capture::xcap_backend`] (Wayland/PipeWire, X11, Windows, macOS)

pub mod audio;
pub mod capture;
pub mod video;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("audio device error: {0}")]
    Device(String),
    #[error("codec error: {0}")]
    Codec(String),
    #[error("capture error: {0}")]
    Capture(String),
    #[error("{0} support was not compiled in")]
    Unsupported(&'static str),
}

pub type MediaResult<T> = Result<T, MediaError>;
