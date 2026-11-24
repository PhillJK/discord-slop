//! Shared server state: connected peers, voice rooms, active screen shares.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::RwLock;
use qchat_proto::{
    Channel, ChannelId, ChannelKind, Event, Member, RejectReason, ServerMsg, SessionId, StreamInfo,
    VoiceState, Welcome, error_code,
};
use tokio::sync::mpsc;

use crate::{config::Config, store::Store, video::ViewerPipe};

/// Outbound control-message queue depth per peer. A peer that falls this far
/// behind is disconnected rather than allowed to grow server memory.
const PEER_QUEUE: usize = 4096;

/// Minimum interval between keyframe requests forwarded to one publisher.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(300);

pub struct Peer {
    pub id: SessionId,
    pub username: String,
    pub conn: quinn::Connection,
    events: mpsc::Sender<ServerMsg>,
}

impl Peer {
    /// Queues a control message; disconnects the peer if its queue is full.
    pub fn send(&self, msg: ServerMsg) {
        match self.events.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(session = %self.id, "control queue full, disconnecting");
                self.conn
                    .close(error_code::SLOW_CONSUMER.into(), b"control queue overflow");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

struct StreamEntry {
    info: StreamInfo,
    viewers: HashMap<SessionId, ViewerPipe>,
    last_keyframe_request: Option<Instant>,
}

#[derive(Default)]
struct State {
    peers: HashMap<SessionId, Arc<Peer>>,
    channels: Vec<Channel>,
    voice: HashMap<SessionId, VoiceState>,
    streams: HashMap<SessionId, StreamEntry>,
}

impl State {
    fn broadcast(&self, msg: &ServerMsg, except: Option<SessionId>) {
        for (id, p) in &self.peers {
            if Some(*id) != except {
                p.send(msg.clone());
            }
        }
    }

    fn channel(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }
}

pub struct Hub {
    pub cfg: Config,
    pub store: Store,
    state: RwLock<State>,
    next_session: AtomicU32,
}

impl Hub {
    pub async fn new(cfg: Config, store: Store) -> anyhow::Result<Arc<Self>> {
        store.seed_defaults().await?;
        let channels = store.list_channels().await?;
        Ok(Arc::new(Self {
            cfg,
            store,
            state: RwLock::new(State {
                channels,
                ..Default::default()
            }),
            next_session: AtomicU32::new(1),
        }))
    }

    /// Registers a new peer and returns its handle, outbound queue and welcome
    /// snapshot. The snapshot and the `MemberJoined` broadcast happen under one
    /// lock so no event can be missed or duplicated.
    pub fn register(
        &self,
        username: String,
        conn: quinn::Connection,
    ) -> Result<(Arc<Peer>, mpsc::Receiver<ServerMsg>, Welcome), RejectReason> {
        let mut st = self.state.write();
        if st.peers.len() >= self.cfg.limits.max_sessions {
            return Err(RejectReason::ServerFull);
        }
        if st
            .peers
            .values()
            .any(|p| p.username.to_lowercase() == username.to_lowercase())
        {
            return Err(RejectReason::UsernameTaken);
        }
        let id = SessionId(self.next_session.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel(PEER_QUEUE);
        let peer = Arc::new(Peer {
            id,
            username: username.clone(),
            conn,
            events: tx,
        });
        st.peers.insert(id, peer.clone());
        let mut members: Vec<Member> = st
            .peers
            .values()
            .map(|p| Member {
                session: p.id,
                username: p.username.clone(),
            })
            .collect();
        members.sort_by_key(|m| m.session);
        let welcome = Welcome {
            session: id,
            server_name: self.cfg.server_name.clone(),
            channels: st.channels.clone(),
            members,
            voice_states: st.voice.values().copied().collect(),
            streams: st.streams.values().map(|s| s.info).collect(),
        };
        st.broadcast(
            &ServerMsg::Event(Event::MemberJoined(Member {
                session: id,
                username,
            })),
            Some(id),
        );
        Ok((peer, rx, welcome))
    }

    /// Removes a peer and everything it owned (voice state, stream, viewer pipes).
    pub fn unregister(&self, id: SessionId) {
        let mut st = self.state.write();
        if st.peers.remove(&id).is_none() {
            return;
        }
        if st.voice.remove(&id).is_some() {
            st.broadcast(
                &ServerMsg::Event(Event::VoiceStateChanged {
                    session: id,
                    state: None,
                }),
                None,
            );
        }
        if st.streams.remove(&id).is_some() {
            st.broadcast(
                &ServerMsg::Event(Event::StreamEnded { publisher: id }),
                None,
            );
        }
        for s in st.streams.values_mut() {
            s.viewers.remove(&id);
        }
        st.broadcast(&ServerMsg::Event(Event::MemberLeft { session: id }), None);
    }

    pub fn broadcast(&self, msg: ServerMsg) {
        self.state.read().broadcast(&msg, None);
    }

    pub fn channel(&self, id: ChannelId) -> Option<Channel> {
        self.state.read().channel(id).cloned()
    }

    pub fn add_channel(&self, ch: Channel) {
        let mut st = self.state.write();
        st.channels.push(ch.clone());
        st.broadcast(&ServerMsg::Event(Event::ChannelCreated(ch)), None);
    }

    // ---- voice -------------------------------------------------------------

    pub fn join_voice(
        &self,
        id: SessionId,
        channel: ChannelId,
    ) -> Result<(), qchat_proto::ErrorKind> {
        let mut st = self.state.write();
        match st.channel(channel) {
            None => return Err(qchat_proto::ErrorKind::UnknownChannel),
            Some(c) if c.kind != ChannelKind::Voice => {
                return Err(qchat_proto::ErrorKind::WrongChannelKind);
            }
            Some(_) => {}
        }
        let (muted, deafened) = st
            .voice
            .get(&id)
            .map_or((false, false), |v| (v.muted, v.deafened));
        let state = VoiceState {
            session: id,
            channel,
            muted,
            deafened,
        };
        st.voice.insert(id, state);
        st.broadcast(
            &ServerMsg::Event(Event::VoiceStateChanged {
                session: id,
                state: Some(state),
            }),
            None,
        );
        Ok(())
    }

    pub fn leave_voice(&self, id: SessionId) {
        let mut st = self.state.write();
        if st.voice.remove(&id).is_some() {
            st.broadcast(
                &ServerMsg::Event(Event::VoiceStateChanged {
                    session: id,
                    state: None,
                }),
                None,
            );
        }
    }

    pub fn set_voice_state(
        &self,
        id: SessionId,
        muted: bool,
        deafened: bool,
    ) -> Result<(), qchat_proto::ErrorKind> {
        let mut st = self.state.write();
        let Some(v) = st.voice.get_mut(&id) else {
            return Err(qchat_proto::ErrorKind::NotInVoice);
        };
        v.muted = muted;
        v.deafened = deafened;
        let state = *v;
        st.broadcast(
            &ServerMsg::Event(Event::VoiceStateChanged {
                session: id,
                state: Some(state),
            }),
            None,
        );
        Ok(())
    }

    /// Connections that should receive a voice packet from `sender`. Empty if the
    /// sender is not in voice or is muted (or deafened, which implies muted).
    pub fn voice_recipients(&self, sender: SessionId, out: &mut Vec<quinn::Connection>) {
        out.clear();
        let st = self.state.read();
        let Some(me) = st.voice.get(&sender) else {
            return;
        };
        if me.muted || me.deafened {
            return;
        }
        for v in st.voice.values() {
            if v.session != sender
                && v.channel == me.channel
                && !v.deafened
                && let Some(p) = st.peers.get(&v.session)
            {
                out.push(p.conn.clone());
            }
        }
    }

    // ---- screen share --------------------------------------------------------

    pub fn start_stream(&self, info: StreamInfo) -> Result<(), qchat_proto::ErrorKind> {
        let mut st = self.state.write();
        if st.streams.contains_key(&info.publisher) {
            return Err(qchat_proto::ErrorKind::AlreadySharing);
        }
        st.streams.insert(
            info.publisher,
            StreamEntry {
                info,
                viewers: HashMap::new(),
                last_keyframe_request: None,
            },
        );
        st.broadcast(&ServerMsg::Event(Event::StreamStarted(info)), None);
        Ok(())
    }

    pub fn stop_stream(&self, publisher: SessionId) -> bool {
        let mut st = self.state.write();
        if st.streams.remove(&publisher).is_some() {
            st.broadcast(&ServerMsg::Event(Event::StreamEnded { publisher }), None);
            true
        } else {
            false
        }
    }

    pub fn watch(
        self: &Arc<Self>,
        viewer: SessionId,
        publisher: SessionId,
    ) -> Result<(), qchat_proto::ErrorKind> {
        let mut st = self.state.write();
        let conn = st
            .peers
            .get(&viewer)
            .map(|p| p.conn.clone())
            .ok_or(qchat_proto::ErrorKind::Internal)?;
        let entry = st
            .streams
            .get_mut(&publisher)
            .ok_or(qchat_proto::ErrorKind::UnknownStream)?;
        if let std::collections::hash_map::Entry::Vacant(slot) = entry.viewers.entry(viewer) {
            let hub = Arc::downgrade(self);
            slot.insert(ViewerPipe::spawn(
                conn,
                self.cfg.limits.video_max_in_flight,
                Box::new(move || {
                    if let Some(h) = hub.upgrade() {
                        h.request_keyframe(publisher);
                    }
                }),
            ));
        }
        drop(st);
        // A new viewer can only start decoding at a keyframe.
        self.request_keyframe(publisher);
        Ok(())
    }

    pub fn unwatch(&self, viewer: SessionId, publisher: SessionId) {
        if let Some(e) = self.state.write().streams.get_mut(&publisher) {
            e.viewers.remove(&viewer);
        }
    }

    /// Asks a publisher for a keyframe, rate limited.
    pub fn request_keyframe(&self, publisher: SessionId) {
        let mut st = self.state.write();
        let Some(entry) = st.streams.get_mut(&publisher) else {
            return;
        };
        let now = Instant::now();
        if entry
            .last_keyframe_request
            .is_some_and(|t| now.duration_since(t) < KEYFRAME_REQUEST_INTERVAL)
        {
            return;
        }
        entry.last_keyframe_request = Some(now);
        if let Some(p) = st.peers.get(&publisher) {
            p.send(ServerMsg::Event(Event::KeyframeNeeded));
        }
    }

    /// Fans an encoded frame out to all viewers of `publisher`. Returns false if
    /// the session is not currently sharing.
    pub fn forward_frame(&self, publisher: SessionId, frame: &crate::video::Frame) -> bool {
        let st = self.state.read();
        let Some(entry) = st.streams.get(&publisher) else {
            return false;
        };
        for pipe in entry.viewers.values() {
            pipe.offer(frame.clone());
        }
        true
    }
}
