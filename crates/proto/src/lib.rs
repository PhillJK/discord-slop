//! qchat wire protocol.
//!
//! One QUIC connection per client carries three kinds of traffic:
//!
//! * **Control**: a single bidirectional stream opened by the client. It carries
//!   length-prefixed [`postcard`] frames of [`ClientMsg`] / [`ServerMsg`].
//! * **Voice**: QUIC datagrams with a fixed binary [`VoiceHeader`] + Opus payload.
//! * **Video**: one unidirectional stream per encoded frame, starting with
//!   [`StreamKind::VideoFrame`] and a fixed binary [`VideoHeader`].
//!
//! Every stream begins with a one-byte [`StreamKind`] tag. Tags are stable; never
//! renumber them. Any change to the wire format must bump [`PROTOCOL_VERSION`].

pub mod framing;
pub mod media;
pub mod messages;

pub use framing::{FrameError, read_frame, write_frame};
pub use media::{VideoHeader, VoiceHeader};
pub use messages::*;

/// Bumped on every incompatible wire change.
pub const PROTOCOL_VERSION: u32 = 1;

/// ALPN identifier negotiated during the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"qchat/1";

/// Default UDP port of a relay server.
pub const DEFAULT_PORT: u16 = 7443;

/// Maximum size of a single control frame (bytes, excluding the length prefix).
pub const MAX_CONTROL_FRAME: usize = 1024 * 1024;

/// Maximum size of a single encoded video frame payload.
pub const MAX_VIDEO_FRAME: usize = 8 * 1024 * 1024;

/// Maximum chat message length in characters.
pub const MAX_MESSAGE_CHARS: usize = 4000;

/// Maximum username length in characters.
pub const MAX_USERNAME_CHARS: usize = 32;

/// Maximum channel name length in characters.
pub const MAX_CHANNEL_NAME_CHARS: usize = 64;

/// Maximum number of messages returned by one history request.
pub const MAX_HISTORY_PAGE: u16 = 200;

/// First byte of every QUIC stream, identifying what the stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    /// Bidirectional control stream (client-opened, exactly one per connection).
    Control = 0x01,
    /// Unidirectional stream carrying exactly one encoded video frame.
    VideoFrame = 0x02,
    /// Reserved for on-demand file streaming (not implemented yet).
    File = 0x03,
}

impl StreamKind {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Control),
            0x02 => Some(Self::VideoFrame),
            0x03 => Some(Self::File),
            _ => None,
        }
    }
}

/// QUIC application error codes used when closing connections or resetting streams.
pub mod error_code {
    /// Normal shutdown.
    pub const NORMAL: u32 = 0;
    /// Handshake rejected (bad password, name taken, version mismatch...).
    pub const REJECTED: u32 = 1;
    /// Peer violated the protocol.
    pub const PROTOCOL: u32 = 2;
    /// Peer is too slow to keep up with control traffic.
    pub const SLOW_CONSUMER: u32 = 3;
    /// Server is shutting down.
    pub const SHUTDOWN: u32 = 4;
    /// A video frame stream was cancelled because a newer frame superseded it.
    pub const STALE_FRAME: u32 = 0x10;
}
