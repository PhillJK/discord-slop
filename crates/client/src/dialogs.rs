//! Modal dialogs and windows.

use eframe::egui::{self, Color32, RichText};
use qchat_client_core::{
    Command,
    proto::ChannelKind,
    state::{CertPrompt, VideoPreset, VoiceMode},
};

use crate::{app::App, icons, theme};

pub struct NewChannel {
    pub name: String,
    pub kind: ChannelKind,
}

pub struct ShareDialog {
    pub source: Option<String>,
    pub preset: VideoPreset,
}

pub fn cert_prompt(app: &mut App, ctx: &egui::Context, p: &CertPrompt) {
    egui::Modal::new(egui::Id::unique("cert-prompt")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        if let Some(prev) = &p.previous {
            ui.heading(RichText::new(format!("{} Certificate changed", icons::WARNING)).color(theme::RED));
            ui.label(format!(
                "The certificate presented by {} differs from the one you trusted before. \
                 If the relay operator did not replace it, someone may be intercepting your connection.",
                p.host
            ));
            ui.add_space(6.0);
            ui.label(RichText::new("Previously trusted:").small().color(theme::TEXT_MUTED));
            ui.monospace(group_hex(prev));
        } else {
            ui.heading("Trust this relay?");
            ui.label(format!(
                "{} uses a certificate that is not signed by a public authority (normal for self-hosted relays). \
                 Compare the fingerprint with the one printed in the server log before trusting it.",
                p.host
            ));
        }
        ui.add_space(6.0);
        ui.label(RichText::new("SHA-256 fingerprint:").small().color(theme::TEXT_MUTED));
        ui.monospace(group_hex(&p.fingerprint));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let label = if p.previous.is_some() { "Trust new certificate" } else { "Trust and connect" };
            if ui
                .add(egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(if p.previous.is_some() { theme::RED } else { theme::ACCENT }))
                .clicked()
            {
                app.handle.send(Command::TrustCertificate);
            }
            if ui.button("Cancel").clicked() {
                app.handle.send(Command::DismissCertificate);
            }
        });
    });
}

fn group_hex(s: &str) -> String {
    s.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .chunks(8)
        .map(|row| row.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn new_channel(app: &mut App, ctx: &egui::Context) {
    let Some(nc) = &mut app.new_channel else {
        return;
    };
    let mut close = false;
    let mut create = false;
    egui::Modal::new(egui::Id::unique("new-channel")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.heading("Create channel");
        ui.horizontal(|ui| {
            ui.radio_value(&mut nc.kind, ChannelKind::Text, "# Text");
            ui.radio_value(
                &mut nc.kind,
                ChannelKind::Voice,
                format!("{} Voice", icons::SPEAKER_HIGH),
            );
        });
        let r = ui.add(
            egui::TextEdit::singleline(&mut nc.name)
                .hint_text("new-channel")
                .char_limit(qchat_client_core::proto::MAX_CHANNEL_NAME_CHARS)
                .desired_width(f32::INFINITY),
        );
        r.request_focus();
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let ok = !nc.name.trim().trim_start_matches('#').is_empty();
            if ui
                .add_enabled(ok, egui::Button::new("Create").fill(theme::ACCENT))
                .clicked()
                || (enter && ok)
            {
                create = true;
            }
            if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });
    });
    if create {
        app.handle.send(Command::CreateChannel {
            name: nc.name.clone(),
            kind: nc.kind,
        });
        close = true;
    }
    if close {
        app.new_channel = None;
    }
}

pub fn share(app: &mut App, ctx: &egui::Context) {
    let sources = app.state.read().capture_sources.clone();
    let Some(d) = &mut app.share_dialog else {
        return;
    };
    if d.source.is_none() {
        d.source = sources.first().map(|s| s.id.clone());
    }
    let mut go = false;
    let mut close = false;
    egui::Modal::new(egui::Id::unique("share")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Share your screen");
        ui.add_space(6.0);
        ui.label(RichText::new("SOURCE").small().strong().color(theme::TEXT_MUTED));
        ui.horizontal(|ui| {
            let selected = sources
                .iter()
                .find(|s| Some(&s.id) == d.source.as_ref())
                .map_or("—".to_owned(), |s| s.name.clone());
            egui::ComboBox::from_id_salt("share-source")
                .width(320.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for s in &sources {
                        ui.selectable_value(&mut d.source, Some(s.id.clone()), &s.name);
                    }
                });
            if ui.button(icons::ARROW_CLOCKWISE).on_hover_text("Refresh sources").clicked() {
                app.handle.send(Command::RefreshDevices);
            }
        });
        if sources.is_empty() {
            ui.label(
                RichText::new("No capture sources found. On Wayland the system picker opens when you go live.")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        }
        ui.add_space(6.0);
        ui.label(RichText::new("QUALITY").small().strong().color(theme::TEXT_MUTED));
        ui.horizontal(|ui| {
            for p in VideoPreset::ALL {
                ui.selectable_value(&mut d.preset, p, p.label());
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(RichText::new("Go Live").strong().color(Color32::WHITE)).fill(theme::ACCENT))
                .clicked()
            {
                go = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if go {
        let source_id = d.source.clone().unwrap_or_default();
        let preset = d.preset;
        app.prefs.set_preset(preset);
        app.handle.send(Command::StartShare { source_id, preset });
        close = true;
    }
    if close {
        app.share_dialog = None;
    }
}

pub fn settings(app: &mut App, ctx: &egui::Context) {
    let mut open = true;
    let (inputs, outputs, mut audio, media) = {
        let st = app.state.read();
        (
            st.audio_inputs.clone(),
            st.audio_outputs.clone(),
            st.audio.clone(),
            st.media_available,
        )
    };
    let before = format!("{audio:?}");
    egui::Window::new("Voice & Video settings")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            if !media {
                ui.colored_label(theme::RED, "This build was compiled without media support.");
            }
            device_combo(
                ui,
                "INPUT DEVICE",
                "in-dev",
                &inputs,
                &mut audio.input_device,
            );
            device_combo(
                ui,
                "OUTPUT DEVICE",
                "out-dev",
                &outputs,
                &mut audio.output_device,
            );
            if ui
                .small_button(format!("{} Refresh devices", icons::ARROW_CLOCKWISE))
                .clicked()
            {
                app.handle.send(Command::RefreshDevices);
            }
            ui.separator();
            ui.checkbox(&mut audio.noise_suppression, "Noise suppression (RNNoise)");
            ui.separator();
            ui.label(
                RichText::new("INPUT MODE")
                    .small()
                    .strong()
                    .color(theme::TEXT_MUTED),
            );
            ui.radio_value(&mut audio.mode, VoiceMode::VoiceActivity, "Voice activity");
            ui.radio_value(&mut audio.mode, VoiceMode::PushToTalk, "Push to talk");
            match audio.mode {
                VoiceMode::VoiceActivity => {
                    ui.add(
                        egui::Slider::new(&mut audio.vad_threshold_db, -80.0..=-10.0)
                            .text("activation threshold (dBFS)"),
                    );
                }
                VoiceMode::PushToTalk => {
                    ui.horizontal(|ui| {
                        ui.label("Key:");
                        ui.add(
                            egui::TextEdit::singleline(&mut app.prefs.ptt_key).desired_width(80.0),
                        );
                        if egui::Key::from_name(&app.prefs.ptt_key).is_none() {
                            ui.colored_label(theme::RED, "unknown key name");
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Works while the qchat window is focused (e.g. F8, Space, V).",
                        )
                        .small()
                        .color(theme::TEXT_MUTED),
                    );
                }
            }
        });
    if format!("{audio:?}") != before {
        app.prefs.set_audio(&audio);
        app.state.write().audio = audio;
        app.handle.send(Command::ApplyAudioSettings);
    }
    app.settings_open = open;
}

fn device_combo(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    devices: &[String],
    selected: &mut Option<String>,
) {
    ui.label(
        RichText::new(label)
            .small()
            .strong()
            .color(theme::TEXT_MUTED),
    );
    egui::ComboBox::from_id_salt(id)
        .width(380.0)
        .selected_text(selected.clone().unwrap_or_else(|| "System default".into()))
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "System default");
            for d in devices {
                ui.selectable_value(selected, Some(d.clone()), d);
            }
        });
}
