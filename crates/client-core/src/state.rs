//! Client-side view of the relay, shared with the UI.
//!
//! The runtime thread mutates [`AppState`] and then calls the UI waker; the UI
//! reads it every frame. Keep lock hold times short on both sides.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::{Mutex, RwLock};
use qchat_proto::{
    Channel, ChannelId, ChannelKind, ChatMessage, Event, SessionId, StreamInfo, VoiceState, Welcome,
};

use crate::ConnectParams;

pub type Shared = Arc<RwLock<AppState>>;

/// How long a speaker stays "speaking" after their last voice packet.
pub const SPEAKING_HOLD: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone)]
pub struct CertPrompt {
    pub host: String,
    pub fingerprint: String,
    /// Set when a previously pinned fingerprint differs.
    pub previous: Option<String>,
    pub params: ConnectParams,
}

#[derive(Debug, Default)]
pub struct ChannelLog {
    /// Sorted by id, deduplicated.
    pub messages: Vec<ChatMessage>,
    /// True once the beginning of the channel's history has been loaded.
    pub reached_start: bool,
    pub loading: bool,
    /// At least one history page has been requested.
    pub loaded_once: bool,
}

impl ChannelLog {
    pub fn insert(&mut self, msg: ChatMessage) {
        match self.messages.binary_search_by_key(&msg.id, |m| m.id) {
            Ok(_) => {}
            Err(pos) => self.messages.insert(pos, msg),
        }
    }

    pub fn oldest_id(&self) -> Option<i64> {
        self.messages.first().map(|m| m.id)
    }
}

/// A decoded RGBA video frame for display.
#[derive(Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
    /// Incremented on every new frame so the UI knows when to re-upload.
    pub generation: u64,
}

/// Latest-frame slot for one watched stream (never a queue: the UI always shows
/// the newest frame and stale ones are simply overwritten).
#[derive(Default)]
pub struct VideoSlot {
    pub frame: Mutex<Option<DecodedFrame>>,
    pub stats: Mutex<VideoStats>,
}

#[derive(Debug, Default, Clone)]
pub struct VideoStats {
    pub fps: f32,
    pub kbps: f32,
    pub width: u32,
    pub height: u32,
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPreset {
    P1440,
    P1080,
    P720,
}

impl VideoPreset {
    pub const ALL: [VideoPreset; 3] = [Self::P1440, Self::P1080, Self::P720];

    pub fn size(self) -> (u32, u32) {
        match self {
            Self::P1440 => (2560, 1440),
            Self::P1080 => (1920, 1080),
            Self::P720 => (1280, 720),
        }
    }

    pub fn fps(self) -> u32 {
        30
    }

    pub fn bitrate_bps(self) -> u32 {
        match self {
            Self::P1440 => 8_000_000,
            Self::P1080 => 5_000_000,
            Self::P720 => 2_500_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::P1440 => "1440p 30fps",
            Self::P1080 => "1080p 30fps",
            Self::P720 => "720p 30fps",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSourceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceMode {
    #[default]
    VoiceActivity,
    PushToTalk,
}

#[derive(Debug, Clone)]
pub struct AudioSettings {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub noise_suppression: bool,
    pub mode: VoiceMode,
    /// Voice activity threshold in dBFS (after noise suppression).
    pub vad_threshold_db: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            noise_suppression: true,
            mode: VoiceMode::VoiceActivity,
            vad_threshold_db: -50.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShareStatus {
    pub preset: VideoPreset,
    pub source_name: String,
    pub encode_fps: f32,
    pub kbps: f32,
    /// Frames skipped because the encoder or uplink could not keep up.
    pub skipped: u64,
    pub warning: Option<String>,
}

#[derive(Default)]
pub struct AppState {
    pub status: ConnStatus,
    pub last_error: Option<String>,
    pub cert_prompt: Option<CertPrompt>,
    pub me: Option<SessionId>,
    pub server_name: String,
    pub channels: Vec<Channel>,
    pub members: BTreeMap<SessionId, String>,
    pub voice: HashMap<SessionId, VoiceState>,
    pub streams: BTreeMap<SessionId, StreamInfo>,
    pub logs: HashMap<ChannelId, ChannelLog>,
    pub speaking: HashMap<SessionId, Instant>,
    /// Local intent; mirrored to the relay while in voice.
    pub muted: bool,
    pub deafened: bool,
    pub ptt_pressed: bool,
    pub audio: AudioSettings,
    pub audio_inputs: Vec<String>,
    pub audio_outputs: Vec<String>,
    pub capture_sources: Vec<CaptureSourceInfo>,
    pub sharing: Option<ShareStatus>,
    pub watching: BTreeMap<SessionId, Arc<VideoSlot>>,
    /// Transient notices for the UI (oldest first).
    pub notices: VecDeque<String>,
    pub rtt_ms: u32,
    pub media_available: bool,
}

impl AppState {
    pub fn new(media_available: bool) -> Self {
        Self {
            media_available,
            ..Default::default()
        }
    }

    pub fn notice(&mut self, msg: impl Into<String>) {
        self.notices.push_back(msg.into());
        while self.notices.len() > 5 {
            self.notices.pop_front();
        }
    }

    pub fn reset_session(&mut self) {
        self.me = None;
        self.server_name.clear();
        self.channels.clear();
        self.members.clear();
        self.voice.clear();
        self.streams.clear();
        self.logs.clear();
        self.speaking.clear();
        self.sharing = None;
        self.watching.clear();
        self.rtt_ms = 0;
    }

    pub fn apply_welcome(&mut self, w: Welcome) {
        self.reset_session();
        self.me = Some(w.session);
        self.server_name = w.server_name;
        self.channels = w.channels;
        self.channels.sort_by_key(|c| c.position);
        self.members = w
            .members
            .into_iter()
            .map(|m| (m.session, m.username))
            .collect();
        self.voice = w.voice_states.into_iter().map(|v| (v.session, v)).collect();
        self.streams = w.streams.into_iter().map(|s| (s.publisher, s)).collect();
    }

    pub fn apply_event(&mut self, e: &Event) {
        match e {
            Event::MessageCreated(m) => self.logs.entry(m.channel).or_default().insert(m.clone()),
            Event::ChannelCreated(c) => {
                if !self.channels.iter().any(|x| x.id == c.id) {
                    self.channels.push(c.clone());
                    self.channels.sort_by_key(|c| c.position);
                }
            }
            Event::MemberJoined(m) => {
                self.members.insert(m.session, m.username.clone());
            }
            Event::MemberLeft { session } => {
                self.members.remove(session);
                self.voice.remove(session);
                self.streams.remove(session);
                self.watching.remove(session);
                self.speaking.remove(session);
            }
            Event::VoiceStateChanged { session, state } => match state {
                Some(s) => {
                    self.voice.insert(*session, *s);
                }
                None => {
                    self.voice.remove(session);
                    self.speaking.remove(session);
                }
            },
            Event::StreamStarted(info) => {
                self.streams.insert(info.publisher, *info);
            }
            Event::StreamEnded { publisher } => {
                self.streams.remove(publisher);
                self.watching.remove(publisher);
                if Some(*publisher) == self.me {
                    self.sharing = None;
                }
            }
            Event::KeyframeNeeded => {}
        }
    }

    pub fn my_voice(&self) -> Option<&VoiceState> {
        self.me.and_then(|me| self.voice.get(&me))
    }

    pub fn username(&self, id: SessionId) -> &str {
        self.members.get(&id).map_or("unknown", String::as_str)
    }

    pub fn is_speaking(&self, id: SessionId) -> bool {
        self.speaking
            .get(&id)
            .is_some_and(|t| t.elapsed() < SPEAKING_HOLD)
    }

    pub fn channel(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn first_text_channel(&self) -> Option<ChannelId> {
        self.channels
            .iter()
            .find(|c| c.kind == ChannelKind::Text)
            .map(|c| c.id)
    }
}
