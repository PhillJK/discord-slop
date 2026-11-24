use std::{
    collections::{BTreeSet, HashMap},
    time::{Duration, Instant},
};

use eframe::egui::{self, RichText};
use qchat_client_core::{
    ClientHandle, Command, ConnectParams,
    proto::{ChannelId, SessionId},
    state::{ConnStatus, Shared},
};

use crate::{dialogs, prefs::Prefs, theme, views};

pub struct App {
    pub handle: ClientHandle,
    pub state: Shared,
    pub prefs: Prefs,
    pub password: String,
    pub selected_text: Option<ChannelId>,
    pub composer: String,
    pub new_channel: Option<dialogs::NewChannel>,
    pub share_dialog: Option<dialogs::ShareDialog>,
    pub settings_open: bool,
    pub textures: HashMap<SessionId, (egui::TextureHandle, u64)>,
    /// Stream shown in the central area instead of chat.
    pub focused_stream: Option<SessionId>,
    /// Streams shown in their own OS window.
    pub popped_out: BTreeSet<SessionId>,
    pub toasts: Vec<(String, Instant)>,
    ptt_down: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, handle: ClientHandle, state: Shared) -> Self {
        let prefs: Prefs = cc
            .storage
            .and_then(|s| eframe::get_value(s, crate::prefs::KEY))
            .unwrap_or_default();
        state.write().audio = prefs.audio();
        handle.send(Command::ApplyAudioSettings);
        let password = if prefs.remember_password {
            prefs.password.clone()
        } else {
            String::new()
        };
        Self {
            handle,
            state,
            prefs,
            password,
            selected_text: None,
            composer: String::new(),
            new_channel: None,
            share_dialog: None,
            settings_open: false,
            textures: HashMap::new(),
            focused_stream: None,
            popped_out: BTreeSet::new(),
            toasts: Vec::new(),
            ptt_down: false,
        }
    }

    pub fn connect(&mut self) {
        self.prefs.address = self.prefs.address.trim().to_owned();
        self.prefs.username = self.prefs.username.trim().to_owned();
        if self.prefs.remember_password {
            self.prefs.password.clone_from(&self.password);
        } else {
            self.prefs.password.clear();
        }
        self.selected_text = None;
        self.focused_stream = None;
        self.popped_out.clear();
        self.textures.clear();
        self.handle.send(Command::Connect(ConnectParams {
            address: self.prefs.address.clone(),
            password: self.password.clone(),
            username: self.prefs.username.clone(),
        }));
    }

    pub fn select_text_channel(&mut self, id: ChannelId) {
        self.selected_text = Some(id);
        self.focused_stream = None;
        let needs_load = self
            .state
            .read()
            .logs
            .get(&id)
            .is_none_or(|l| !l.loaded_once);
        if needs_load {
            self.handle.send(Command::LoadHistory { channel: id });
        }
    }

    fn handle_push_to_talk(&mut self, ctx: &egui::Context) {
        let ptt_mode = self.prefs.push_to_talk;
        let key = egui::Key::from_name(&self.prefs.ptt_key);
        let down = ptt_mode && key.is_some_and(|k| ctx.input(|i| i.key_down(k)));
        if down != self.ptt_down {
            self.ptt_down = down;
            self.handle.send(Command::SetPushToTalkPressed(down));
        }
    }

    fn collect_toasts(&mut self) {
        let mut st = self.state.write();
        let now = Instant::now();
        while let Some(n) = st.notices.pop_front() {
            self.toasts.push((n, now));
        }
        drop(st);
        self.toasts
            .retain(|(_, t)| t.elapsed() < Duration::from_secs(6));
    }

    fn show_toasts(&self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (msg, _) in &self.toasts {
                    egui::Frame::new()
                        .fill(theme::BG_DARKEST)
                        .corner_radius(6)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.label(msg);
                        });
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn login_screen(&mut self, ui: &mut egui::Ui) {
        let (status, error) = {
            let st = self.state.read();
            (st.status, st.last_error.clone())
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG_DARKEST))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.18).max(20.0));
                    egui::Frame::new()
                        .fill(theme::BG_MAIN)
                        .corner_radius(10)
                        .inner_margin(28)
                        .show(ui, |ui| {
                            ui.set_width(380.0);
                            ui.heading(RichText::new("Welcome to qchat").strong());
                            ui.label(
                                RichText::new("Connect to a relay server").color(theme::TEXT_MUTED),
                            );
                            ui.add_space(14.0);
                            let busy = status == ConnStatus::Connecting;
                            let mut submit = false;
                            ui.add_enabled_ui(!busy, |ui| {
                                field(ui, "RELAY ADDRESS", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.prefs.address)
                                            .hint_text("host:7443")
                                            .desired_width(f32::INFINITY),
                                    )
                                });
                                let r = field(ui, "PASSWORD", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.password)
                                            .password(true)
                                            .desired_width(f32::INFINITY),
                                    )
                                });
                                submit |= enter_pressed(ui, &r);
                                let r = field(ui, "DISPLAY NAME", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.prefs.username)
                                            .char_limit(
                                                qchat_client_core::proto::MAX_USERNAME_CHARS,
                                            )
                                            .desired_width(f32::INFINITY),
                                    )
                                });
                                submit |= enter_pressed(ui, &r);
                                ui.checkbox(
                                    &mut self.prefs.remember_password,
                                    "Remember password on this device",
                                );
                                ui.add_space(8.0);
                                let can = !self.prefs.address.trim().is_empty()
                                    && !self.prefs.username.trim().is_empty();
                                let btn = egui::Button::new(
                                    RichText::new(if busy { "Connecting…" } else { "Connect" })
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(theme::ACCENT)
                                .min_size(egui::vec2(ui.available_width(), 36.0));
                                if ui.add_enabled(can, btn).clicked() || (submit && can) {
                                    self.connect();
                                }
                            });
                            if busy {
                                ui.add_space(6.0);
                                ui.spinner();
                            }
                            if let Some(e) = error {
                                ui.add_space(8.0);
                                ui.colored_label(theme::RED, e);
                            }
                        });
                });
            });
    }

    fn main_screen(&mut self, ui: &mut egui::Ui) {
        // Keep selection valid.
        if self
            .selected_text
            .is_none_or(|id| self.state.read().channel(id).is_none())
        {
            let first = self.state.read().first_text_channel();
            if let Some(id) = first {
                self.select_text_channel(id);
            }
        }
        if let Some(p) = self.focused_stream
            && !self.state.read().watching.contains_key(&p)
        {
            self.focused_stream = None;
        }

        egui::Panel::left("channels")
            .exact_size(250.0)
            .frame(egui::Frame::new().fill(theme::BG_DARK).inner_margin(0))
            .show(ui, |ui| views::sidebar(self, ui));
        egui::Panel::right("members")
            .exact_size(230.0)
            .frame(egui::Frame::new().fill(theme::BG_DARK).inner_margin(12))
            .show(ui, |ui| views::members(self, ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG_MAIN).inner_margin(0))
            .show(ui, |ui| {
                if let Some(p) = self.focused_stream {
                    views::stream_view(self, ui, p);
                } else {
                    views::chat(self, ui);
                }
            });

        views::popouts(self, ui.ctx());
    }
}

fn field<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.label(
        RichText::new(label)
            .small()
            .strong()
            .color(theme::TEXT_MUTED),
    );
    let r = add(ui);
    ui.add_space(6.0);
    r
}

fn enter_pressed(ui: &egui::Ui, r: &egui::Response) -> bool {
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.collect_toasts();
        self.handle_push_to_talk(&ctx);

        let (status, prompt) = {
            let st = self.state.read();
            (st.status, st.cert_prompt.clone())
        };
        if status == ConnStatus::Connected {
            self.main_screen(ui);
        } else {
            self.login_screen(ui);
        }

        if let Some(p) = prompt {
            dialogs::cert_prompt(self, &ctx, &p);
        }
        if self.settings_open {
            dialogs::settings(self, &ctx);
        }
        if self.share_dialog.is_some() {
            dialogs::share(self, &ctx);
        }
        if self.new_channel.is_some() {
            dialogs::new_channel(self, &ctx);
        }
        self.show_toasts(&ctx);

        // Keep speaking rings / stats fresh while anything live is going on.
        let live = {
            let st = self.state.read();
            !st.speaking.is_empty() || !st.watching.is_empty() || st.sharing.is_some()
        };
        if live {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let audio = self.state.read().audio.clone();
        self.prefs.set_audio(&audio);
        if !self.prefs.remember_password {
            self.prefs.password.clear();
        }
        eframe::set_value(storage, crate::prefs::KEY, &self.prefs);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.handle.send(Command::Shutdown);
    }
}
