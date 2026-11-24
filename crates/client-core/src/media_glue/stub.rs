use std::sync::Arc;

use bytes::Bytes;
use qchat_proto::{SessionId, VideoHeader, VoiceHeader};

use crate::{
    Session,
    runtime::Waker,
    state::{Shared, VideoPreset, VideoSlot},
};

const UNAVAILABLE: &str = "built without media support";

pub(crate) struct Media;

impl Media {
    pub fn available() -> bool {
        false
    }
    pub fn new(_state: Shared, _waker: Waker) -> Self {
        Self
    }
    pub fn refresh_devices(&mut self) {}
    pub fn is_active(&self) -> bool {
        false
    }
    pub fn start_voice(&mut self, _session: Arc<Session>) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
    pub fn stop_voice(&mut self) {}
    pub fn voice_packet(&mut self, _header: VoiceHeader, _payload: Bytes) {}
    pub fn remove_speaker(&mut self, _id: SessionId) {}
    pub fn apply_audio_settings(&mut self) {}
    pub fn start_share(
        &mut self,
        _session: Arc<Session>,
        _source_id: &str,
        _preset: VideoPreset,
    ) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
    pub fn stop_share(&mut self) {}
    pub fn keyframe_needed(&mut self) {}
    pub fn watch(&mut self, _session: Arc<Session>, _publisher: SessionId, _slot: Arc<VideoSlot>) {}
    pub fn unwatch(&mut self, _publisher: SessionId) {}
    pub fn video_frame(&mut self, _header: VideoHeader, _payload: Bytes) {}
    pub fn shutdown(&mut self) {}
}
