//! Fixed binary headers for voice datagrams and video frame streams.
//!
//! These are hand-encoded (big-endian) rather than postcard so they have a fixed,
//! tiny size and can be rewritten in place by the relay.

use crate::SessionId;

/// Datagram type tag (first byte of every datagram).
pub const DATAGRAM_VOICE: u8 = 0x01;

/// Header of a voice datagram. Followed by one Opus packet (20 ms @ 48 kHz).
///
/// Layout: `kind u8 | sender u32 | seq u16 | timestamp u32` (11 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceHeader {
    /// Filled in by the relay; clients may send anything (it is overwritten).
    pub sender: SessionId,
    /// Wrapping sequence number, +1 per packet.
    pub seq: u16,
    /// Sample timestamp at 48 kHz, wrapping.
    pub timestamp: u32,
}

impl VoiceHeader {
    pub const LEN: usize = 11;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(DATAGRAM_VOICE);
        out.extend_from_slice(&self.sender.0.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
    }

    /// Builds a complete datagram.
    pub fn to_datagram(&self, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(Self::LEN + payload.len());
        self.encode(&mut v);
        v.extend_from_slice(payload);
        v
    }

    /// Parses a datagram into header and Opus payload.
    pub fn decode(buf: &[u8]) -> Option<(Self, &[u8])> {
        if buf.len() < Self::LEN || buf[0] != DATAGRAM_VOICE {
            return None;
        }
        let sender = u32::from_be_bytes(buf[1..5].try_into().ok()?);
        let seq = u16::from_be_bytes(buf[5..7].try_into().ok()?);
        let timestamp = u32::from_be_bytes(buf[7..11].try_into().ok()?);
        Some((
            Self {
                sender: SessionId(sender),
                seq,
                timestamp,
            },
            &buf[Self::LEN..],
        ))
    }

    /// Overwrites the sender field of an encoded datagram in place.
    pub fn rewrite_sender(buf: &mut [u8], sender: SessionId) -> bool {
        if buf.len() < Self::LEN || buf[0] != DATAGRAM_VOICE {
            return false;
        }
        buf[1..5].copy_from_slice(&sender.0.to_be_bytes());
        true
    }
}

/// Header of a video frame stream. Written right after the [`crate::StreamKind`]
/// byte, followed by the encoded frame (H.264 Annex-B) until end of stream.
///
/// Layout: `publisher u32 | frame_seq u32 | timestamp_ms u32 | flags u8 |
/// width u16 | height u16` (17 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// Filled in by the relay; ignored when sent by a client.
    pub publisher: SessionId,
    pub frame_seq: u32,
    /// Capture timestamp in milliseconds since the stream started, wrapping.
    pub timestamp_ms: u32,
    pub keyframe: bool,
    pub width: u16,
    pub height: u16,
}

impl VideoHeader {
    pub const LEN: usize = 17;
    const FLAG_KEYFRAME: u8 = 0x01;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&self.publisher.0.to_be_bytes());
        b[4..8].copy_from_slice(&self.frame_seq.to_be_bytes());
        b[8..12].copy_from_slice(&self.timestamp_ms.to_be_bytes());
        b[12] = if self.keyframe {
            Self::FLAG_KEYFRAME
        } else {
            0
        };
        b[13..15].copy_from_slice(&self.width.to_be_bytes());
        b[15..17].copy_from_slice(&self.height.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            publisher: SessionId(u32::from_be_bytes(b[0..4].try_into().ok()?)),
            frame_seq: u32::from_be_bytes(b[4..8].try_into().ok()?),
            timestamp_ms: u32::from_be_bytes(b[8..12].try_into().ok()?),
            keyframe: b[12] & Self::FLAG_KEYFRAME != 0,
            width: u16::from_be_bytes(b[13..15].try_into().ok()?),
            height: u16::from_be_bytes(b[15..17].try_into().ok()?),
        })
    }
}

/// QUIC stream priority for a video frame: newer groups of pictures first, and
/// within a group, earlier frames first (a delta frame is useless without its
/// predecessors). Higher value = sent first. `gop` increments at each keyframe.
pub fn frame_priority(gop: u32, index_in_gop: u32) -> i32 {
    let gop = (gop & 0x7fff) as i32;
    let idx = index_in_gop.min(0xffff) as i32;
    (gop << 16) | (0xffff - idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_orders_newer_gops_first_then_earlier_frames() {
        assert!(frame_priority(2, 0) > frame_priority(1, 0));
        assert!(frame_priority(2, 0) > frame_priority(1, 50));
        assert!(frame_priority(1, 1) > frame_priority(1, 2));
        assert!(frame_priority(2, 60) > frame_priority(1, 0));
    }

    #[test]
    fn voice_roundtrip_and_rewrite() {
        let h = VoiceHeader {
            sender: SessionId(0xdead_beef),
            seq: 65535,
            timestamp: 123_456,
        };
        let mut d = h.to_datagram(&[1, 2, 3]);
        let (back, payload) = VoiceHeader::decode(&d).expect("decode");
        assert_eq!(back, h);
        assert_eq!(payload, &[1, 2, 3]);

        assert!(VoiceHeader::rewrite_sender(&mut d, SessionId(5)));
        assert_eq!(
            VoiceHeader::decode(&d).expect("decode").0.sender,
            SessionId(5)
        );
    }

    #[test]
    fn voice_rejects_short_or_wrong_kind() {
        assert!(VoiceHeader::decode(&[DATAGRAM_VOICE; 5]).is_none());
        let mut d = VoiceHeader {
            sender: SessionId(1),
            seq: 1,
            timestamp: 1,
        }
        .to_datagram(&[]);
        d[0] = 0x7f;
        assert!(VoiceHeader::decode(&d).is_none());
        assert!(!VoiceHeader::rewrite_sender(&mut d, SessionId(2)));
    }

    #[test]
    fn video_roundtrip() {
        for keyframe in [true, false] {
            let h = VideoHeader {
                publisher: SessionId(9),
                frame_seq: 42,
                timestamp_ms: 99_999,
                keyframe,
                width: 2560,
                height: 1440,
            };
            assert_eq!(VideoHeader::decode(&h.encode()), Some(h));
        }
        assert!(VideoHeader::decode(&[0; 16]).is_none());
    }
}
