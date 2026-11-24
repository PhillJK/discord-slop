//! Bridge between the runtime and the media crate. Without the `media`
//! feature a stub with the same API is used so the runtime stays cfg-free.

#[cfg(feature = "media")]
mod real;
#[cfg(feature = "media")]
pub(crate) use real::Media;

#[cfg(not(feature = "media"))]
mod stub;
#[cfg(not(feature = "media"))]
pub(crate) use stub::Media;
