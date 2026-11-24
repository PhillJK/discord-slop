//! Background runtime for GUI clients.
//!
//! The UI owns a [`ClientHandle`] and a [`Shared`] state. Commands go in through
//! the handle; the runtime thread (its own tokio runtime) applies network events
//! to the state and calls the waker so the UI repaints.

use std::{sync::Arc, time::Instant};

use parking_lot::RwLock;
use qchat_proto::{ChannelId, ChannelKind, Event, SessionId};
use tokio::sync::mpsc;

use crate::{
    ConnectError, ConnectParams, KnownHosts, Session, SessionEvent,
    media_glue::Media,
    state::{AppState, CertPrompt, ConnStatus, Shared, VideoPreset, VideoSlot},
};

pub type Waker = Arc<dyn Fn() + Send + Sync>;

const HISTORY_PAGE: u16 = 50;

#[derive(Debug, Clone)]
pub enum Command {
    Connect(ConnectParams),
    /// Pin the certificate from the current prompt and reconnect.
    TrustCertificate,
    DismissCertificate,
    Disconnect,
    SendMessage {
        channel: ChannelId,
        content: String,
    },
    /// Load the newest page if nothing is loaded, otherwise the next older page.
    LoadHistory {
        channel: ChannelId,
    },
    CreateChannel {
        name: String,
        kind: ChannelKind,
    },
    JoinVoice(ChannelId),
    LeaveVoice,
    SetMuted(bool),
    SetDeafened(bool),
    SetPushToTalkPressed(bool),
    /// Re-read `AppState::audio` and apply it to the running voice engine.
    ApplyAudioSettings,
    RefreshDevices,
    StartShare {
        source_id: String,
        preset: VideoPreset,
    },
    StopShare,
    Watch(SessionId),
    Unwatch(SessionId),
    Shutdown,
}

#[derive(Clone)]
pub struct ClientHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl ClientHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

/// Starts the runtime thread.
pub fn spawn(known_hosts: KnownHosts, waker: Waker) -> std::io::Result<(ClientHandle, Shared)> {
    let state: Shared = Arc::new(RwLock::new(AppState::new(Media::available())));
    let (tx, rx) = mpsc::unbounded_channel();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("qchat-net")
        .enable_all()
        .build()?;
    let st = state.clone();
    std::thread::Builder::new()
        .name("qchat-runtime".into())
        .spawn(move || {
            rt.block_on(async move {
                let media = Media::new(st.clone(), waker.clone());
                let mut r = Runtime {
                    state: st,
                    waker,
                    known_hosts: Arc::new(known_hosts),
                    session: None,
                    events: None,
                    media,
                    internal_tx: None,
                };
                r.run(rx).await;
            });
        })?;
    Ok((ClientHandle { tx }, state))
}

enum Internal {
    Connected(Session, mpsc::UnboundedReceiver<SessionEvent>),
    ConnectFailed(ConnectError, ConnectParams),
    VoiceJoined,
    ShareAccepted {
        source_id: String,
        preset: VideoPreset,
    },
    WatchAccepted(SessionId),
}

struct Runtime {
    state: Shared,
    waker: Waker,
    known_hosts: Arc<KnownHosts>,
    session: Option<Arc<Session>>,
    events: Option<mpsc::UnboundedReceiver<SessionEvent>>,
    media: Media,
    internal_tx: Option<mpsc::UnboundedSender<Internal>>,
}

impl Runtime {
    fn wake(&self) {
        (self.waker)();
    }

    fn update(&self, f: impl FnOnce(&mut AppState)) {
        f(&mut self.state.write());
        self.wake();
    }

    async fn run(&mut self, mut cmds: mpsc::UnboundedReceiver<Command>) {
        let (itx, mut irx) = mpsc::unbounded_channel();
        self.internal_tx = Some(itx);
        self.media.refresh_devices();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    None | Some(Command::Shutdown) => break,
                    Some(cmd) => self.command(cmd),
                },
                Some(i) = irx.recv() => self.internal(i),
                ev = recv_opt(&mut self.events) => match ev {
                    Some(ev) => self.session_event(ev),
                    None => self.disconnected("connection lost".into()),
                },
                _ = tick.tick() => self.tick(),
            }
        }
        self.media.shutdown();
        if let Some(s) = self.session.take() {
            s.close().await;
        }
    }

    fn spawn_req<F>(&self, fut: F)
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let state = self.state.clone();
        let waker = self.waker.clone();
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                state.write().notice(e);
                waker();
            }
        });
    }

    fn internal_tx(&self) -> mpsc::UnboundedSender<Internal> {
        // Always set in `run` before any command is processed.
        self.internal_tx
            .clone()
            .unwrap_or_else(|| mpsc::unbounded_channel().0)
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Connect(params) => self.connect(params),
            Command::TrustCertificate => {
                let prompt = self.state.write().cert_prompt.take();
                if let Some(p) = prompt {
                    if let Err(e) = self.known_hosts.trust(&p.host, &p.fingerprint) {
                        self.update(|s| s.notice(format!("could not save certificate pin: {e}")));
                    }
                    self.connect(p.params);
                }
            }
            Command::DismissCertificate => self.update(|s| {
                s.cert_prompt = None;
                s.status = ConnStatus::Disconnected;
            }),
            Command::Disconnect => {
                self.media.shutdown();
                if let Some(s) = self.session.take() {
                    tokio::spawn(async move { s.close().await });
                }
                self.events = None;
                self.update(|s| {
                    s.reset_session();
                    s.status = ConnStatus::Disconnected;
                });
            }
            Command::Shutdown => {}
            other => {
                let Some(session) = self.session.clone() else {
                    self.update(|s| s.notice("not connected"));
                    return;
                };
                self.session_command(session, other);
            }
        }
    }

    fn session_command(&mut self, session: Arc<Session>, cmd: Command) {
        match cmd {
            Command::SendMessage { channel, content } => {
                let state = self.state.clone();
                let waker = self.waker.clone();
                self.spawn_req(async move {
                    let m = session
                        .send_message(channel, content)
                        .await
                        .map_err(|e| format!("message not sent: {e}"))?;
                    state.write().logs.entry(m.channel).or_default().insert(m);
                    waker();
                    Ok(())
                });
            }
            Command::LoadHistory { channel } => {
                let before = {
                    let mut st = self.state.write();
                    let log = st.logs.entry(channel).or_default();
                    if log.loading || log.reached_start {
                        return;
                    }
                    log.loading = true;
                    // First load: newest page. Later: page before the oldest we have.
                    if log.loaded_once {
                        log.oldest_id()
                    } else {
                        None
                    }
                };
                let state = self.state.clone();
                let waker = self.waker.clone();
                self.spawn_req(async move {
                    let res = session.fetch_history(channel, before, HISTORY_PAGE).await;
                    let mut st = state.write();
                    let log = st.logs.entry(channel).or_default();
                    log.loading = false;
                    log.loaded_once = true;
                    let msgs = res.map_err(|e| format!("could not load history: {e}"))?;
                    if msgs.len() < HISTORY_PAGE as usize {
                        log.reached_start = true;
                    }
                    for m in msgs {
                        log.insert(m);
                    }
                    drop(st);
                    waker();
                    Ok(())
                });
            }
            Command::CreateChannel { name, kind } => self.spawn_req(async move {
                session
                    .create_channel(name, kind)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("could not create channel: {e}"))
            }),
            Command::JoinVoice(channel) => {
                let (muted, deafened) = {
                    let st = self.state.read();
                    (st.muted, st.deafened)
                };
                let itx = self.internal_tx();
                self.spawn_req(async move {
                    session
                        .join_voice(channel)
                        .await
                        .map_err(|e| format!("could not join voice: {e}"))?;
                    if muted || deafened {
                        let _ = session.set_voice_state(muted, deafened).await;
                    }
                    let _ = itx.send(Internal::VoiceJoined);
                    Ok(())
                });
            }
            Command::LeaveVoice => {
                self.media.stop_voice();
                self.spawn_req(
                    async move { session.leave_voice().await.map_err(|e| e.to_string()) },
                );
            }
            Command::SetMuted(m) => {
                let (muted, deafened, in_voice) = {
                    let mut st = self.state.write();
                    st.muted = m;
                    if !m {
                        st.deafened = false;
                    }
                    (st.muted, st.deafened, st.my_voice().is_some())
                };
                self.media.apply_audio_settings();
                self.wake();
                if in_voice {
                    self.spawn_req(async move {
                        session
                            .set_voice_state(muted, deafened)
                            .await
                            .map_err(|e| e.to_string())
                    });
                }
            }
            Command::SetDeafened(d) => {
                let (muted, deafened, in_voice) = {
                    let mut st = self.state.write();
                    st.deafened = d;
                    if d {
                        st.muted = true;
                    }
                    (st.muted, st.deafened, st.my_voice().is_some())
                };
                self.media.apply_audio_settings();
                self.wake();
                if in_voice {
                    self.spawn_req(async move {
                        session
                            .set_voice_state(muted, deafened)
                            .await
                            .map_err(|e| e.to_string())
                    });
                }
            }
            Command::SetPushToTalkPressed(p) => {
                self.state.write().ptt_pressed = p;
                self.media.apply_audio_settings();
            }
            Command::ApplyAudioSettings => self.media.apply_audio_settings(),
            Command::RefreshDevices => {
                self.media.refresh_devices();
                self.wake();
            }
            Command::StartShare { source_id, preset } => {
                if !Media::available() {
                    self.update(|s| s.notice("this build has no media support"));
                    return;
                }
                let (w, h) = preset.size();
                let itx = self.internal_tx();
                self.spawn_req(async move {
                    session
                        .start_screen_share(
                            u16::try_from(w).unwrap_or(u16::MAX),
                            u16::try_from(h).unwrap_or(u16::MAX),
                            u8::try_from(preset.fps()).unwrap_or(30),
                        )
                        .await
                        .map_err(|e| format!("could not start sharing: {e}"))?;
                    let _ = itx.send(Internal::ShareAccepted { source_id, preset });
                    Ok(())
                });
            }
            Command::StopShare => {
                self.media.stop_share();
                self.update(|s| s.sharing = None);
                self.spawn_req(async move {
                    session.stop_screen_share().await.map_err(|e| e.to_string())
                });
            }
            Command::Watch(publisher) => {
                if !Media::available() {
                    self.update(|s| s.notice("this build has no media support"));
                    return;
                }
                let itx = self.internal_tx();
                self.spawn_req(async move {
                    session
                        .watch_stream(publisher)
                        .await
                        .map_err(|e| format!("could not watch stream: {e}"))?;
                    let _ = itx.send(Internal::WatchAccepted(publisher));
                    Ok(())
                });
            }
            Command::Unwatch(publisher) => {
                self.media.unwatch(publisher);
                self.update(|s| {
                    s.watching.remove(&publisher);
                });
                self.spawn_req(async move {
                    session
                        .unwatch_stream(publisher)
                        .await
                        .map_err(|e| e.to_string())
                });
            }
            Command::Connect(_)
            | Command::TrustCertificate
            | Command::DismissCertificate
            | Command::Disconnect
            | Command::Shutdown => {}
        }
    }

    fn connect(&mut self, params: ConnectParams) {
        self.media.shutdown();
        if let Some(s) = self.session.take() {
            tokio::spawn(async move { s.close().await });
        }
        self.events = None;
        self.update(|s| {
            s.reset_session();
            s.status = ConnStatus::Connecting;
            s.last_error = None;
            s.cert_prompt = None;
        });
        let kh = self.known_hosts.clone();
        let itx = self.internal_tx();
        tokio::spawn(async move {
            let msg = match Session::connect(&params, &kh).await {
                Ok((s, ev)) => Internal::Connected(s, ev),
                Err(e) => Internal::ConnectFailed(e, params),
            };
            let _ = itx.send(msg);
        });
    }

    fn internal(&mut self, i: Internal) {
        match i {
            Internal::Connected(session, events) => {
                if self.state.read().status != ConnStatus::Connecting {
                    // User disconnected meanwhile.
                    return;
                }
                let welcome = session.welcome.clone();
                let first_text = welcome
                    .channels
                    .iter()
                    .find(|c| c.kind == ChannelKind::Text)
                    .map(|c| c.id);
                self.session = Some(Arc::new(session));
                self.events = Some(events);
                self.update(|s| {
                    s.apply_welcome(welcome);
                    s.status = ConnStatus::Connected;
                });
                if let Some(ch) = first_text {
                    self.command(Command::LoadHistory { channel: ch });
                }
            }
            Internal::ConnectFailed(err, params) => self.update(|s| {
                s.status = ConnStatus::Disconnected;
                match err {
                    ConnectError::UnknownCertificate { host, fingerprint } => {
                        s.cert_prompt = Some(CertPrompt {
                            host,
                            fingerprint,
                            previous: None,
                            params,
                        });
                    }
                    ConnectError::CertificateChanged {
                        host,
                        expected,
                        fingerprint,
                    } => {
                        s.cert_prompt = Some(CertPrompt {
                            host,
                            fingerprint,
                            previous: Some(expected),
                            params,
                        });
                    }
                    other => s.last_error = Some(other.to_string()),
                }
            }),
            Internal::VoiceJoined => {
                if let Some(session) = self.session.clone()
                    && let Err(e) = self.media.start_voice(session)
                {
                    self.update(|s| s.notice(format!("audio unavailable: {e}")));
                }
            }
            Internal::ShareAccepted { source_id, preset } => {
                if let Some(session) = self.session.clone()
                    && let Err(e) = self.media.start_share(session.clone(), &source_id, preset)
                {
                    self.update(|s| s.notice(format!("screen capture failed: {e}")));
                    tokio::spawn(async move {
                        let _ = session.stop_screen_share().await;
                    });
                }
            }
            Internal::WatchAccepted(publisher) => {
                let Some(session) = self.session.clone() else {
                    return;
                };
                let slot = Arc::new(VideoSlot::default());
                self.state.write().watching.insert(publisher, slot.clone());
                self.media.watch(session, publisher, slot);
                self.wake();
            }
        }
    }

    fn session_event(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::Server(e) => {
                match &e {
                    Event::KeyframeNeeded => self.media.keyframe_needed(),
                    Event::StreamEnded { publisher } => {
                        self.media.unwatch(*publisher);
                        if self.state.read().me == Some(*publisher) {
                            self.media.stop_share();
                        }
                    }
                    Event::VoiceStateChanged {
                        session,
                        state: None,
                    } if self.state.read().me == Some(*session) => {
                        self.media.stop_voice();
                    }
                    Event::MemberLeft { session } => {
                        self.media.unwatch(*session);
                        self.media.remove_speaker(*session);
                    }
                    _ => {}
                }
                self.update(|s| s.apply_event(&e));
            }
            SessionEvent::Voice { header, payload } => {
                let newly = {
                    let mut st = self.state.write();
                    let was = st.is_speaking(header.sender);
                    st.speaking.insert(header.sender, Instant::now());
                    !was
                };
                if newly {
                    self.wake();
                }
                self.media.voice_packet(header, payload);
            }
            SessionEvent::Video { header, payload } => self.media.video_frame(header, payload),
            SessionEvent::Closed(reason) => self.disconnected(reason),
        }
    }

    fn disconnected(&mut self, reason: String) {
        if self.session.is_none() {
            self.events = None;
            return;
        }
        self.media.shutdown();
        self.session = None;
        self.events = None;
        self.update(|s| {
            s.reset_session();
            s.status = ConnStatus::Disconnected;
            s.last_error = Some(reason);
        });
    }

    fn tick(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let rtt = u32::try_from(session.rtt().as_millis()).unwrap_or(u32::MAX);
        let mut st = self.state.write();
        st.rtt_ms = rtt;
        // Expire speaking indicators so the UI updates when someone stops.
        let before = st.speaking.len();
        st.speaking
            .retain(|_, t| t.elapsed() < crate::state::SPEAKING_HOLD * 4);
        let changed = before != st.speaking.len() || !st.speaking.is_empty();
        drop(st);
        if changed || self.media.is_active() {
            self.wake();
        }
    }
}

async fn recv_opt(rx: &mut Option<mpsc::UnboundedReceiver<SessionEvent>>) -> Option<SessionEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}
