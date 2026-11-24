//! Settings persisted between runs via eframe storage.

use qchat_client_core::state::{AudioSettings, VideoPreset, VoiceMode};
use serde::{Deserialize, Serialize};

pub const KEY: &str = "qchat-prefs";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub address: String,
    pub username: String,
    /// Only stored when the user explicitly opts in.
    pub remember_password: bool,
    pub password: String,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub noise_suppression: bool,
    pub push_to_talk: bool,
    pub ptt_key: String,
    pub vad_threshold_db: f32,
    pub preset: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            address: "localhost".into(),
            username: String::new(),
            remember_password: false,
            password: String::new(),
            input_device: None,
            output_device: None,
            noise_suppression: true,
            push_to_talk: false,
            ptt_key: "F8".into(),
            vad_threshold_db: -50.0,
            preset: "1440p".into(),
        }
    }
}

impl Prefs {
    pub fn audio(&self) -> AudioSettings {
        AudioSettings {
            input_device: self.input_device.clone(),
            output_device: self.output_device.clone(),
            noise_suppression: self.noise_suppression,
            mode: if self.push_to_talk {
                VoiceMode::PushToTalk
            } else {
                VoiceMode::VoiceActivity
            },
            vad_threshold_db: self.vad_threshold_db,
        }
    }

    pub fn set_audio(&mut self, a: &AudioSettings) {
        self.input_device.clone_from(&a.input_device);
        self.output_device.clone_from(&a.output_device);
        self.noise_suppression = a.noise_suppression;
        self.push_to_talk = a.mode == VoiceMode::PushToTalk;
        self.vad_threshold_db = a.vad_threshold_db;
    }

    pub fn preset(&self) -> VideoPreset {
        match self.preset.as_str() {
            "720p" => VideoPreset::P720,
            "1080p" => VideoPreset::P1080,
            _ => VideoPreset::P1440,
        }
    }

    pub fn set_preset(&mut self, p: VideoPreset) {
        self.preset = match p {
            VideoPreset::P1440 => "1440p",
            VideoPreset::P1080 => "1080p",
            VideoPreset::P720 => "720p",
        }
        .into();
    }
}
