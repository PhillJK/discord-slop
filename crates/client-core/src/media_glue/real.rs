//! Wires the media crate's engines to the session and shared state.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use bytes::Bytes;
use parking_lot::Mutex;
use qchat_media::{
    audio::{
        engine::{VoiceCallbacks, VoiceConfig, VoiceEngine},
        vad::GateMode,
    },
    capture,
    video::{
        EncodeSettings,
        pipeline::{PublishCallbacks, Publisher, ViewCallbacks, Viewer},
    },
};
use qchat_proto::{SessionId, VideoHeader, VoiceHeader};

use crate::{
    Session,
    runtime::Waker,
    state::{
        CaptureSourceInfo, DecodedFrame, ShareStatus, Shared, VideoPreset, VideoSlot, VoiceMode,
    },
};

pub(crate) struct Media {
    state: Shared,
    waker: Waker,
    voice: Option<(VoiceEngine, Weak<Session>)>,
    publisher: Arc<Mutex<Option<Publisher>>>,
    viewers: HashMap<SessionId, Viewer>,
}

fn settings(p: VideoPreset) -> EncodeSettings {
    match p {
        VideoPreset::P1440 => EncodeSettings::P1440,
        VideoPreset::P1080 => EncodeSettings::P1080,
        VideoPreset::P720 => EncodeSettings::P720,
    }
}

/// "Speaking until further notice" marker for the local user.
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(3600 * 24)
}

impl Media {
    pub fn available() -> bool {
        true
    }

    pub fn new(state: Shared, waker: Waker) -> Self {
        Self {
            state,
            waker,
            voice: None,
            publisher: Arc::default(),
            viewers: HashMap::new(),
        }
    }

    /// Enumerates audio devices and capture sources on a helper thread (this
    /// can take a while and must not stall the network loop).
    pub fn refresh_devices(&mut self) {
        let state = self.state.clone();
        let waker = self.waker.clone();
        let _ = std::thread::Builder::new()
            .name("enumerate-devices".into())
            .spawn(move || {
                let inputs = qchat_media::audio::device::input_device_names();
                let outputs = qchat_media::audio::device::output_device_names();
                let sources: Vec<CaptureSourceInfo> = capture::sources()
                    .into_iter()
                    .map(|s| CaptureSourceInfo {
                        id: s.id,
                        name: s.name,
                    })
                    .collect();
                let mut st = state.write();
                st.audio_inputs = inputs;
                st.audio_outputs = outputs;
                st.capture_sources = sources;
                drop(st);
                waker();
            });
    }

    pub fn is_active(&self) -> bool {
        self.voice.is_some() || self.publisher.lock().is_some() || !self.viewers.is_empty()
    }

    fn voice_config(&self) -> VoiceConfig {
        let st = self.state.read();
        VoiceConfig {
            input_device: st.audio.input_device.clone(),
            output_device: st.audio.output_device.clone(),
            noise_suppression: st.audio.noise_suppression,
            gate: match st.audio.mode {
                VoiceMode::VoiceActivity => GateMode::VoiceActivity {
                    threshold_db: st.audio.vad_threshold_db,
                },
                VoiceMode::PushToTalk => GateMode::PushToTalk {
                    pressed: st.ptt_pressed,
                },
            },
            muted: st.muted,
            deafened: st.deafened,
        }
    }

    pub fn start_voice(&mut self, session: Arc<Session>) -> Result<(), String> {
        self.stop_voice();
        let me = session.id();
        let weak = Arc::downgrade(&session);
        let send_session = weak.clone();
        let state = self.state.clone();
        let waker = self.waker.clone();
        let engine = VoiceEngine::start(
            self.voice_config(),
            VoiceCallbacks {
                send: Box::new(move |seq, ts, payload| {
                    if let Some(s) = send_session.upgrade() {
                        s.send_voice(seq, ts, payload);
                    }
                }),
                local_speaking: Box::new(move |speaking| {
                    let mut st = state.write();
                    if speaking {
                        st.speaking.insert(me, far_future());
                    } else {
                        st.speaking.remove(&me);
                    }
                    drop(st);
                    waker();
                }),
            },
        )
        .map_err(|e| e.to_string())?;
        self.voice = Some((engine, weak));
        Ok(())
    }

    pub fn stop_voice(&mut self) {
        if self.voice.take().is_some() {
            let me = self.state.read().me;
            if let Some(me) = me {
                self.state.write().speaking.remove(&me);
            }
        }
    }

    pub fn voice_packet(&mut self, header: VoiceHeader, payload: Bytes) {
        if let Some((v, _)) = &self.voice {
            v.push_packet(header.sender.0, header.seq, header.timestamp, payload);
        }
    }

    pub fn remove_speaker(&mut self, id: SessionId) {
        if let Some((v, _)) = &self.voice {
            v.remove_speaker(id.0);
        }
    }

    pub fn apply_audio_settings(&mut self) {
        let new = self.voice_config();
        let Some((engine, session)) = &self.voice else {
            return;
        };
        let old = engine.config();
        if old.input_device != new.input_device || old.output_device != new.output_device {
            // Device change: restart the engine.
            if let Some(s) = session.upgrade()
                && let Err(e) = self.start_voice(s)
            {
                self.state
                    .write()
                    .notice(format!("audio restart failed: {e}"));
                (self.waker)();
            }
        } else {
            engine.set_config(new);
        }
    }

    pub fn start_share(
        &mut self,
        session: Arc<Session>,
        source_id: &str,
        preset: VideoPreset,
    ) -> Result<(), String> {
        self.stop_share();
        let handle = tokio::runtime::Handle::current();
        let source_id = source_id.to_owned();
        let source_name = self
            .state
            .read()
            .capture_sources
            .iter()
            .find(|s| s.id == source_id)
            .map_or_else(|| "screen".to_owned(), |s| s.name.clone());
        let slot = self.publisher.clone();
        let state = self.state.clone();
        let waker = self.waker.clone();
        let weak = Arc::downgrade(&session);
        let enc = settings(preset);

        // Starting a capture may block on a system dialog (Wayland portal
        // picker, macOS permission prompt), so do it off the network thread.
        std::thread::Builder::new()
            .name("share-setup".into())
            .spawn(move || {
                let fail = |msg: String| {
                    state
                        .write()
                        .notice(format!("screen capture failed: {msg}"));
                    waker();
                    if let Some(s) = weak.upgrade() {
                        handle.spawn(async move {
                            let _ = s.stop_screen_share().await;
                        });
                    }
                };
                let stream = match capture::start(&source_id, enc.fps) {
                    Ok(s) => s,
                    Err(e) => return fail(e.to_string()),
                };
                let send_weak = weak.clone();
                let in_flight_weak = weak.clone();
                let send_handle = handle.clone();
                let stats_state = state.clone();
                let stats_waker = waker.clone();
                let publisher = Publisher::start(
                    stream,
                    enc,
                    PublishCallbacks {
                        send: Box::new(move |meta, payload| {
                            if let Some(s) = send_weak.upgrade() {
                                let _guard = send_handle.enter();
                                s.send_video_frame(meta.header, payload, meta.priority);
                            }
                        }),
                        in_flight: Box::new(move || {
                            in_flight_weak.upgrade().map_or(0, |s| s.video_in_flight())
                        }),
                        stats: Box::new(move |st| {
                            if let Some(sh) = stats_state.write().sharing.as_mut() {
                                sh.encode_fps = st.encode_fps;
                                sh.kbps = st.kbps;
                                sh.skipped = st.skipped;
                                sh.warning = st.warning;
                            }
                            stats_waker();
                        }),
                    },
                );
                match publisher {
                    Ok(p) => {
                        *slot.lock() = Some(p);
                        state.write().sharing = Some(ShareStatus {
                            preset,
                            source_name,
                            encode_fps: 0.0,
                            kbps: 0.0,
                            skipped: 0,
                            warning: None,
                        });
                        waker();
                    }
                    Err(e) => fail(e.to_string()),
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn stop_share(&mut self) {
        self.publisher.lock().take();
    }

    pub fn keyframe_needed(&mut self) {
        if let Some(p) = self.publisher.lock().as_ref() {
            p.request_keyframe();
        }
    }

    pub fn watch(&mut self, session: Arc<Session>, publisher: SessionId, slot: Arc<VideoSlot>) {
        let handle = tokio::runtime::Handle::current();
        let weak = Arc::downgrade(&session);
        let waker = self.waker.clone();
        let frame_slot = slot.clone();
        let generation = std::sync::atomic::AtomicU64::new(0);
        let viewer = Viewer::start(ViewCallbacks {
            frame: Box::new(move |f| {
                let g = generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                *frame_slot.frame.lock() = Some(DecodedFrame {
                    width: f.width,
                    height: f.height,
                    rgba: Arc::new(f.rgba),
                    generation: g,
                });
                waker();
            }),
            request_keyframe: Box::new(move || {
                if let Some(s) = weak.upgrade() {
                    handle.spawn(async move {
                        let _ = s.request_keyframe(publisher).await;
                    });
                }
            }),
            stats: Box::new(move |st| {
                let mut s = slot.stats.lock();
                s.fps = st.fps;
                s.kbps = st.kbps;
                s.width = st.width;
                s.height = st.height;
                s.dropped = st.dropped;
            }),
        });
        match viewer {
            Ok(v) => {
                self.viewers.insert(publisher, v);
            }
            Err(e) => {
                self.state
                    .write()
                    .notice(format!("cannot start video decoder: {e}"));
                (self.waker)();
            }
        }
    }

    pub fn unwatch(&mut self, publisher: SessionId) {
        self.viewers.remove(&publisher);
    }

    pub fn video_frame(&mut self, header: VideoHeader, payload: Bytes) {
        if let Some(v) = self.viewers.get(&header.publisher)
            && !v.push(header, payload)
        {
            tracing::debug!("decoder queue full, frame dropped");
        }
    }

    pub fn shutdown(&mut self) {
        self.stop_voice();
        self.stop_share();
        self.viewers.clear();
    }
}
