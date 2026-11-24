//! Control-stream message types.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Server-assigned id of a connected session. Unique for the lifetime of the server
/// process; never reused while the server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u32);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Persistent channel id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Persistent message id (monotonic per server).
pub type MessageId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelKind {
    Text,
    Voice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    pub kind: ChannelKind,
    pub position: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub session: SessionId,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub channel: ChannelId,
    pub author: String,
    pub content: String,
    /// Unix timestamp in milliseconds (server clock).
    pub sent_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceState {
    pub session: SessionId,
    pub channel: ChannelId,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    /// Each session can publish at most one screen share, so the publisher's
    /// session id doubles as the stream id.
    pub publisher: SessionId,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
}

/// Messages sent from client to server on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Must be the first message on the control stream.
    Hello {
        version: u32,
        password: String,
        username: String,
    },
    Request {
        id: u32,
        body: Request,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    SendMessage {
        channel: ChannelId,
        content: String,
    },
    FetchHistory {
        channel: ChannelId,
        /// Return messages with `id < before`; `None` means newest.
        before: Option<MessageId>,
        limit: u16,
    },
    CreateChannel {
        name: String,
        kind: ChannelKind,
    },
    JoinVoice {
        channel: ChannelId,
    },
    LeaveVoice,
    SetVoiceState {
        muted: bool,
        deafened: bool,
    },
    StartScreenShare {
        width: u16,
        height: u16,
        fps: u8,
    },
    StopScreenShare,
    WatchStream {
        publisher: SessionId,
    },
    UnwatchStream {
        publisher: SessionId,
    },
    /// Viewer asks the server to get a fresh keyframe from the publisher
    /// (e.g. after a decode error).
    RequestKeyframe {
        publisher: SessionId,
    },
    Ping,
}

/// Messages sent from server to client on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome(Welcome),
    Rejected(RejectReason),
    Response {
        id: u32,
        result: Result<Response, ErrorKind>,
    },
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub session: SessionId,
    pub server_name: String,
    pub channels: Vec<Channel>,
    pub members: Vec<Member>,
    pub voice_states: Vec<VoiceState>,
    pub streams: Vec<StreamInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum RejectReason {
    #[error("protocol version mismatch (server speaks v{server})")]
    VersionMismatch { server: u32 },
    #[error("wrong password")]
    BadPassword,
    #[error("username is already in use")]
    UsernameTaken,
    #[error("invalid username: {0}")]
    InvalidUsername(String),
    #[error("server is full")]
    ServerFull,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Message(ChatMessage),
    History(Vec<ChatMessage>),
    Channel(Channel),
    Pong,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ErrorKind {
    #[error("no such channel")]
    UnknownChannel,
    #[error("wrong channel type for this operation")]
    WrongChannelKind,
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("rate limited, slow down")]
    RateLimited,
    #[error("not in a voice channel")]
    NotInVoice,
    #[error("no such stream")]
    UnknownStream,
    #[error("already sharing a screen")]
    AlreadySharing,
    #[error("internal server error")]
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    MessageCreated(ChatMessage),
    ChannelCreated(Channel),
    MemberJoined(Member),
    MemberLeft {
        session: SessionId,
    },
    /// `state == None` means the session left voice.
    VoiceStateChanged {
        session: SessionId,
        state: Option<VoiceState>,
    },
    StreamStarted(StreamInfo),
    StreamEnded {
        publisher: SessionId,
    },
    /// Sent to a publisher: encode the next frame as a keyframe.
    KeyframeNeeded,
}

/// Validates a username, returning the trimmed form.
pub fn validate_username(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("must not be empty".into());
    }
    if name.chars().count() > crate::MAX_USERNAME_CHARS {
        return Err(format!(
            "must be at most {} characters",
            crate::MAX_USERNAME_CHARS
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("must not contain control characters".into());
    }
    Ok(name.to_owned())
}

/// Validates a channel name, returning the normalized form.
pub fn validate_channel_name(name: &str) -> Result<String, String> {
    let name = name.trim().trim_start_matches('#').trim();
    if name.is_empty() {
        return Err("must not be empty".into());
    }
    if name.chars().count() > crate::MAX_CHANNEL_NAME_CHARS {
        return Err(format!(
            "must be at most {} characters",
            crate::MAX_CHANNEL_NAME_CHARS
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("must not contain control characters".into());
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation() {
        assert_eq!(validate_username("  bob ").as_deref(), Ok("bob"));
        assert!(validate_username("   ").is_err());
        assert!(validate_username(&"x".repeat(33)).is_err());
        assert!(validate_username("a\nb").is_err());
    }

    #[test]
    fn channel_name_validation() {
        assert_eq!(validate_channel_name("#general").as_deref(), Ok("general"));
        assert!(validate_channel_name("#").is_err());
    }

    #[test]
    fn postcard_roundtrip_all_variants() {
        let ch = ChannelId(Uuid::new_v4());
        let msgs = vec![
            ServerMsg::Welcome(Welcome {
                session: SessionId(7),
                server_name: "x".into(),
                channels: vec![Channel {
                    id: ch,
                    name: "general".into(),
                    kind: ChannelKind::Text,
                    position: 0,
                }],
                members: vec![Member {
                    session: SessionId(7),
                    username: "a".into(),
                }],
                voice_states: vec![VoiceState {
                    session: SessionId(7),
                    channel: ch,
                    muted: true,
                    deafened: false,
                }],
                streams: vec![StreamInfo {
                    publisher: SessionId(7),
                    width: 2560,
                    height: 1440,
                    fps: 30,
                }],
            }),
            ServerMsg::Rejected(RejectReason::BadPassword),
            ServerMsg::Response {
                id: 3,
                result: Err(ErrorKind::Invalid("nope".into())),
            },
            ServerMsg::Response {
                id: 4,
                result: Ok(Response::History(vec![ChatMessage {
                    id: 1,
                    channel: ch,
                    author: "a".into(),
                    content: "héllo 👋".into(),
                    sent_at_ms: 1_700_000_000_000,
                }])),
            },
            ServerMsg::Event(Event::VoiceStateChanged {
                session: SessionId(1),
                state: None,
            }),
            ServerMsg::Event(Event::KeyframeNeeded),
        ];
        for m in msgs {
            let bytes = postcard::to_stdvec(&m).expect("encode");
            let back: ServerMsg = postcard::from_bytes(&bytes).expect("decode");
            assert_eq!(m, back);
        }

        let c = ClientMsg::Request {
            id: 9,
            body: Request::FetchHistory {
                channel: ch,
                before: Some(10),
                limit: 50,
            },
        };
        let bytes = postcard::to_stdvec(&c).expect("encode");
        assert_eq!(
            c,
            postcard::from_bytes::<ClientMsg>(&bytes).expect("decode")
        );
    }
}
