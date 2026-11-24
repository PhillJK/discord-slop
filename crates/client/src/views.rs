//! Main-screen panels.

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, Sense};
use qchat_client_core::{
    Command,
    proto::{ChannelKind, ChatMessage, MAX_MESSAGE_CHARS, SessionId},
    state::{AppState, VideoSlot},
};

use crate::{app::App, dialogs, icons, theme};

// ---------------------------------------------------------------- sidebar ---

pub fn sidebar(app: &mut App, ui: &mut egui::Ui) {
    let st = app.state.read();
    let server_name = st.server_name.clone();
    let rtt = st.rtt_ms;
    drop(st);

    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(14, 12))
        .fill(theme::BG_DARK)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&server_name).strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{rtt} ms"))
                            .small()
                            .color(theme::TEXT_MUTED),
                    )
                    .on_hover_text("round-trip time to the relay");
                });
            });
        });
    ui.separator();

    // Bottom user panel first so the channel list gets the remaining height.
    egui::Panel::bottom("user-panel")
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DARKEST)
                .inner_margin(egui::Margin::symmetric(10, 8)),
        )
        .show(ui, |ui| user_panel(app, ui));

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(6.0);
            channel_section(app, ui, ChannelKind::Text);
            ui.add_space(10.0);
            channel_section(app, ui, ChannelKind::Voice);
        });
}

fn channel_section(app: &mut App, ui: &mut egui::Ui, kind: ChannelKind) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        let title = match kind {
            ChannelKind::Text => "TEXT CHANNELS",
            ChannelKind::Voice => "VOICE CHANNELS",
        };
        ui.label(
            RichText::new(title)
                .small()
                .strong()
                .color(theme::TEXT_MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if ui
                .add(egui::Button::new(RichText::new("+").color(theme::TEXT_MUTED)).frame(false))
                .on_hover_text("Create channel")
                .clicked()
            {
                app.new_channel = Some(dialogs::NewChannel {
                    name: String::new(),
                    kind,
                });
            }
        });
    });

    let st = app.state.read();
    let channels: Vec<_> = st
        .channels
        .iter()
        .filter(|c| c.kind == kind)
        .cloned()
        .collect();
    let my_voice = st.my_voice().map(|v| v.channel);
    drop(st);

    for ch in channels {
        let selected = match kind {
            ChannelKind::Text => app.selected_text == Some(ch.id) && app.focused_stream.is_none(),
            ChannelKind::Voice => my_voice == Some(ch.id),
        };
        let icon = match kind {
            ChannelKind::Text => "#",
            ChannelKind::Voice => icons::SPEAKER_HIGH,
        };
        let resp = channel_row(ui, icon, &ch.name, selected);
        if resp.clicked() {
            match kind {
                ChannelKind::Text => app.select_text_channel(ch.id),
                ChannelKind::Voice => {
                    if my_voice != Some(ch.id) {
                        app.handle.send(Command::JoinVoice(ch.id));
                    }
                }
            }
        }
        if kind == ChannelKind::Voice {
            voice_members(app, ui, ch.id);
        }
    }
}

fn channel_row(ui: &mut egui::Ui, icon: &str, name: &str, selected: bool) -> egui::Response {
    let width = ui.available_width() - 16.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width + 16.0, 30.0), Sense::click());
    let rect = rect.shrink2(egui::vec2(8.0, 1.0));
    let bg = if selected {
        Color32::from_rgb(64, 66, 73)
    } else if resp.hovered() {
        Color32::from_rgb(53, 55, 60)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 4.0, bg);
    let color = if selected || resp.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_MUTED
    };
    ui.painter().text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        format!("{icon}  {name}"),
        egui::FontId::proportional(15.0),
        color,
    );
    resp
}

fn voice_members(app: &mut App, ui: &mut egui::Ui, channel: qchat_client_core::proto::ChannelId) {
    let st = app.state.read();
    let mut people: Vec<_> = st
        .voice
        .values()
        .filter(|v| v.channel == channel)
        .map(|v| {
            (
                v.session,
                st.username(v.session).to_owned(),
                st.is_speaking(v.session) && !v.muted,
                v.muted,
                v.deafened,
                st.streams.contains_key(&v.session),
            )
        })
        .collect();
    let me = st.me;
    drop(st);
    people.sort_by_key(|p| p.1.to_lowercase());

    for (session, name, speaking, muted, deafened, live) in people {
        ui.horizontal(|ui| {
            ui.add_space(34.0);
            avatar(ui, &name, 20.0, speaking);
            ui.label(RichText::new(&name).color(if speaking {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            }));
            if live {
                let watch = ui
                    .add(
                        egui::Button::new(
                            RichText::new("LIVE").small().strong().color(Color32::WHITE),
                        )
                        .fill(theme::LIVE)
                        .corner_radius(3),
                    )
                    .on_hover_text("Watch stream");
                if watch.clicked() && Some(session) != me {
                    watch_stream(app, session);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(10.0);
                if deafened {
                    ui.label(RichText::new(icons::SPEAKER_SLASH).color(theme::RED))
                        .on_hover_text("Deafened");
                } else if muted {
                    ui.label(RichText::new(icons::MICROPHONE_SLASH).color(theme::RED))
                        .on_hover_text("Muted");
                }
            });
        });
    }
}

pub fn watch_stream(app: &mut App, publisher: SessionId) {
    let watching = app.state.read().watching.contains_key(&publisher);
    if !watching {
        app.handle.send(Command::Watch(publisher));
    }
    app.focused_stream = Some(publisher);
}

fn user_panel(app: &mut App, ui: &mut egui::Ui) {
    let st = app.state.read();
    let me = st.me;
    let name = me.map(|m| st.username(m).to_owned()).unwrap_or_default();
    let in_voice = st.my_voice().map(|v| v.channel);
    let voice_name = in_voice
        .and_then(|c| st.channel(c))
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let (muted, deafened) = (st.muted, st.deafened);
    let sharing = st.sharing.clone();
    let speaking = me.is_some_and(|m| st.is_speaking(m));
    let media = st.media_available;
    drop(st);

    if in_voice.is_some() {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Voice Connected")
                        .strong()
                        .color(theme::GREEN),
                );
                ui.label(RichText::new(&voice_name).small().color(theme::TEXT_MUTED));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new(icons::PHONE_X).color(Color32::WHITE))
                            .fill(theme::RED),
                    )
                    .on_hover_text("Disconnect from voice")
                    .clicked()
                {
                    app.handle.send(Command::LeaveVoice);
                }
            });
        });
        ui.horizontal(|ui| {
            if let Some(s) = &sharing {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("{} Stop sharing", icons::STOP))
                                .color(Color32::WHITE),
                        )
                        .fill(theme::LIVE),
                    )
                    .clicked()
                {
                    app.handle.send(Command::StopShare);
                }
                let mut info = format!(
                    "{} · {:.0} fps · {:.1} Mb/s",
                    s.preset.label(),
                    s.encode_fps,
                    s.kbps / 1000.0
                );
                if let Some(w) = &s.warning {
                    info.push_str(" · ");
                    info.push_str(w);
                }
                ui.label(RichText::new(info).small().color(theme::TEXT_MUTED));
            } else if ui
                .add_enabled(
                    media,
                    egui::Button::new(format!("{} Share screen", icons::MONITOR)),
                )
                .on_disabled_hover_text("this build has no media support")
                .clicked()
            {
                app.handle.send(Command::RefreshDevices);
                app.share_dialog = Some(dialogs::ShareDialog {
                    source: None,
                    preset: app.prefs.preset(),
                });
            }
        });
        ui.separator();
    }

    ui.horizontal(|ui| {
        avatar(ui, &name, 30.0, speaking && !muted);
        ui.vertical(|ui| {
            ui.label(RichText::new(&name).strong());
            ui.label(RichText::new("online").small().color(theme::TEXT_MUTED));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(icons::GEAR).on_hover_text("Settings").clicked() {
                app.handle.send(Command::RefreshDevices);
                app.settings_open = true;
            }
            let deaf_btn = egui::Button::new(
                RichText::new(if deafened {
                    icons::SPEAKER_SLASH
                } else {
                    icons::HEADPHONES
                })
                .color(if deafened { theme::RED } else { theme::TEXT }),
            );
            if ui
                .add(deaf_btn)
                .on_hover_text(if deafened { "Undeafen" } else { "Deafen" })
                .clicked()
            {
                app.handle.send(Command::SetDeafened(!deafened));
            }
            let mic_btn = egui::Button::new(
                RichText::new(if muted {
                    icons::MICROPHONE_SLASH
                } else {
                    icons::MICROPHONE
                })
                .color(if muted { theme::RED } else { theme::TEXT }),
            );
            if ui
                .add(mic_btn)
                .on_hover_text(if muted { "Unmute" } else { "Mute" })
                .clicked()
            {
                app.handle.send(Command::SetMuted(!muted));
            }
        });
    });
    ui.horizontal(|ui| {
        if ui.small_button("Log out").clicked() {
            app.handle.send(Command::Disconnect);
        }
    });
}

pub fn avatar(ui: &mut egui::Ui, name: &str, size: f32, speaking: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    let c = rect.center();
    let r = size / 2.0;
    ui.painter().circle_filled(c, r, theme::name_color(name));
    if speaking {
        ui.painter()
            .circle_stroke(c, r + 1.0, egui::Stroke::new(2.0, theme::GREEN));
    }
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    ui.painter().text(
        c,
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(size * 0.5),
        Color32::WHITE,
    );
}

// ---------------------------------------------------------------- members ---

pub fn members(app: &mut App, ui: &mut egui::Ui) {
    let st = app.state.read();
    let mut people: Vec<(SessionId, String, bool)> = st
        .members
        .iter()
        .map(|(id, name)| (*id, name.clone(), st.streams.contains_key(id)))
        .collect();
    let me = st.me;
    drop(st);
    people.sort_by_key(|p| p.1.to_lowercase());

    ui.label(
        RichText::new(format!("ONLINE — {}", people.len()))
            .small()
            .strong()
            .color(theme::TEXT_MUTED),
    );
    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (id, name, live) in people {
                ui.horizontal(|ui| {
                    avatar(ui, &name, 30.0, false);
                    ui.label(&name);
                    if Some(id) == me {
                        ui.label(RichText::new("(you)").small().color(theme::TEXT_MUTED));
                    }
                    if live && Some(id) != me {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Watch").small().color(Color32::WHITE),
                                    )
                                    .fill(theme::LIVE),
                                )
                                .clicked()
                            {
                                watch_stream(app, id);
                            }
                        });
                    } else if live {
                        ui.label(RichText::new("LIVE").small().strong().color(theme::LIVE));
                    }
                });
            }
        });
}

// ------------------------------------------------------------------- chat ---

pub fn chat(app: &mut App, ui: &mut egui::Ui) {
    let Some(channel) = app.selected_text else {
        ui.centered_and_justified(|ui| ui.label("No text channel"));
        return;
    };
    let st = app.state.read();
    let name = st
        .channel(channel)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    drop(st);

    egui::Panel::top("chat-header")
        .frame(
            egui::Frame::new()
                .fill(theme::BG_MAIN)
                .inner_margin(egui::Margin::symmetric(16, 12)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("#").size(20.0).color(theme::TEXT_MUTED));
                ui.label(RichText::new(&name).strong().size(16.0));
                live_strip(app, ui);
            });
        });

    egui::Panel::bottom("composer")
        .frame(
            egui::Frame::new()
                .fill(theme::BG_MAIN)
                .inner_margin(egui::Margin {
                    left: 16,
                    right: 16,
                    top: 4,
                    bottom: 16,
                }),
        )
        .show(ui, |ui| composer(app, ui, channel, &name));

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG_MAIN)
                .inner_margin(egui::Margin::symmetric(16, 0)),
        )
        .show(ui, |ui| message_list(app, ui, channel, &name));
}

/// Small "X is live" buttons in the chat header.
fn live_strip(app: &mut App, ui: &mut egui::Ui) {
    let st = app.state.read();
    let me = st.me;
    let live: Vec<(SessionId, String)> = st
        .streams
        .keys()
        .filter(|p| Some(**p) != me)
        .map(|p| (*p, st.username(*p).to_owned()))
        .collect();
    drop(st);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        for (p, name) in live {
            if ui
                .add(egui::Button::new(
                    RichText::new(format!("{} {name} is live", icons::RECORD)).small(),
                ))
                .clicked()
            {
                watch_stream(app, p);
            }
        }
    });
}

fn composer(
    app: &mut App,
    ui: &mut egui::Ui,
    channel: qchat_client_core::proto::ChannelId,
    name: &str,
) {
    egui::Frame::new()
        .fill(theme::BG_INPUT)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let edit = egui::TextEdit::multiline(&mut app.composer)
                .hint_text(format!("Message #{name}"))
                .desired_rows(1)
                .desired_width(f32::INFINITY)
                .char_limit(MAX_MESSAGE_CHARS)
                .frame(egui::Frame::NONE)
                .return_key(Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::SHIFT,
                    egui::Key::Enter,
                )));
            let resp = ui.add(edit);
            let send = resp.has_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            if send {
                let text = app.composer.trim().to_owned();
                if !text.is_empty() {
                    app.handle.send(Command::SendMessage {
                        channel,
                        content: text,
                    });
                }
                app.composer.clear();
                resp.request_focus();
            }
        });
}

fn message_list(
    app: &mut App,
    ui: &mut egui::Ui,
    channel: qchat_client_core::proto::ChannelId,
    name: &str,
) {
    let st = app.state.read();
    let (msgs, reached_start, loading) = st.logs.get(&channel).map_or_else(
        || (Vec::new(), false, false),
        |l| (l.messages.clone(), l.reached_start, l.loading),
    );
    drop(st);

    let out = egui::ScrollArea::vertical()
        .id_salt(("messages", channel))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(12.0);
            if reached_start {
                ui.label(
                    RichText::new(format!("Welcome to #{name}!"))
                        .size(24.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(format!("This is the start of the #{name} channel."))
                        .color(theme::TEXT_MUTED),
                );
                ui.add_space(12.0);
            } else if loading {
                ui.spinner();
            }
            let mut prev: Option<&ChatMessage> = None;
            for m in &msgs {
                let grouped = prev.is_some_and(|p| {
                    p.author == m.author && m.sent_at_ms - p.sent_at_ms < 5 * 60 * 1000
                });
                message_row(ui, m, grouped);
                prev = Some(m);
            }
            ui.add_space(8.0);
        });
    // Scrolled to the very top: load older messages.
    if out.state.clamped_offset().y <= 1.0 && !reached_start && !loading && !msgs.is_empty() {
        app.handle.send(Command::LoadHistory { channel });
    }
}

fn message_row(ui: &mut egui::Ui, m: &ChatMessage, grouped: bool) {
    let ts = chrono::DateTime::from_timestamp_millis(m.sent_at_ms)
        .map(|t| t.with_timezone(&chrono::Local))
        .unwrap_or_default();
    if grouped {
        ui.horizontal_top(|ui| {
            ui.add_space(48.0);
            ui.add(egui::Label::new(&m.content).wrap().selectable(true));
        });
        return;
    }
    ui.add_space(10.0);
    ui.horizontal_top(|ui| {
        avatar(ui, &m.author, 38.0, false);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&m.author)
                        .strong()
                        .color(theme::name_color(&m.author)),
                );
                let today = chrono::Local::now().date_naive() == ts.date_naive();
                let stamp = if today {
                    format!("Today at {}", ts.format("%H:%M"))
                } else {
                    ts.format("%Y-%m-%d %H:%M").to_string()
                };
                ui.label(RichText::new(stamp).small().color(theme::TEXT_MUTED));
            });
            ui.add(egui::Label::new(&m.content).wrap().selectable(true));
        });
    });
}

// ----------------------------------------------------------------- video ---

/// Uploads the newest decoded frame of `publisher` to a texture, if changed.
pub fn texture_for(
    app: &mut App,
    ctx: &egui::Context,
    publisher: SessionId,
    slot: &Arc<VideoSlot>,
) -> Option<egui::TextureHandle> {
    let frame = slot.frame.lock().clone();
    let Some(frame) = frame else {
        return app.textures.get(&publisher).map(|(t, _)| t.clone());
    };
    let needs_upload = app
        .textures
        .get(&publisher)
        .is_none_or(|(_, g)| *g != frame.generation);
    if needs_upload {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match app.textures.get_mut(&publisher) {
            Some((tex, g)) => {
                tex.set(image, egui::TextureOptions::LINEAR);
                *g = frame.generation;
            }
            None => {
                let tex = ctx.load_texture(
                    format!("stream-{}", publisher.0),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                app.textures.insert(publisher, (tex, frame.generation));
            }
        }
    }
    app.textures.get(&publisher).map(|(t, _)| t.clone())
}

fn draw_video(ui: &mut egui::Ui, tex: Option<&egui::TextureHandle>) {
    let avail = ui.available_size();
    match tex {
        Some(tex) => {
            let [w, h] = tex.size();
            let aspect = w as f32 / h.max(1) as f32;
            let mut size = egui::vec2(avail.x, avail.x / aspect);
            if size.y > avail.y {
                size = egui::vec2(avail.y * aspect, avail.y);
            }
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new(tex).fit_to_exact_size(size));
            });
        }
        None => {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Waiting for video…").color(theme::TEXT_MUTED));
            });
        }
    }
}

fn stats_line(st: &AppState, publisher: SessionId, slot: &VideoSlot) -> String {
    let s = slot.stats.lock().clone();
    format!(
        "{} · {}×{} · {:.0} fps · {:.1} Mb/s · dropped {}",
        st.username(publisher),
        s.width,
        s.height,
        s.fps,
        s.kbps / 1000.0,
        s.dropped
    )
}

pub fn stream_view(app: &mut App, ui: &mut egui::Ui, publisher: SessionId) {
    let st = app.state.read();
    let Some(slot) = st.watching.get(&publisher).cloned() else {
        return;
    };
    let stats = stats_line(&st, publisher, &slot);
    drop(st);
    let ctx = ui.ctx().clone();

    egui::Panel::top("stream-header")
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DARKEST)
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("LIVE").strong().color(theme::LIVE));
                ui.label(stats);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(format!("{} Stop watching", icons::X)).clicked() {
                        app.handle.send(Command::Unwatch(publisher));
                        app.focused_stream = None;
                        app.textures.remove(&publisher);
                        app.popped_out.remove(&publisher);
                    }
                    if ui
                        .button(format!("{} Pop out", icons::ARROW_SQUARE_OUT))
                        .clicked()
                    {
                        app.popped_out.insert(publisher);
                        app.focused_stream = None;
                    }
                    if ui
                        .button(format!("{} Back to chat", icons::CHAT_CIRCLE_TEXT))
                        .clicked()
                    {
                        app.focused_stream = None;
                    }
                });
            });
        });
    let tex = texture_for(app, &ctx, publisher, &slot);
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Color32::BLACK))
        .show(ui, |ui| draw_video(ui, tex.as_ref()));
}

/// Streams popped out into their own OS windows.
pub fn popouts(app: &mut App, ctx: &egui::Context) {
    let ids: Vec<SessionId> = app.popped_out.iter().copied().collect();
    for publisher in ids {
        let st = app.state.read();
        let Some(slot) = st.watching.get(&publisher).cloned() else {
            drop(st);
            app.popped_out.remove(&publisher);
            continue;
        };
        let title = format!("{} — qchat stream", st.username(publisher));
        let stats = stats_line(&st, publisher, &slot);
        drop(st);
        let tex = texture_for(app, ctx, publisher, &slot);
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("stream", publisher.0)),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([1280.0, 760.0]),
            |ui, _class| {
                egui::Panel::bottom("stats").show(ui, |ui| {
                    ui.label(RichText::new(&stats).small());
                });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(Color32::BLACK))
                    .show(ui, |ui| draw_video(ui, tex.as_ref()));
                if ui.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
            },
        );
        if close {
            app.popped_out.remove(&publisher);
            app.focused_stream = Some(publisher);
        }
    }
}
