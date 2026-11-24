//! Dark theme loosely inspired by modern chat apps.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};

pub const BG_DARKEST: Color32 = Color32::from_rgb(30, 31, 34);
pub const BG_DARK: Color32 = Color32::from_rgb(43, 45, 49);
pub const BG_MAIN: Color32 = Color32::from_rgb(49, 51, 56);
pub const BG_INPUT: Color32 = Color32::from_rgb(56, 58, 64);
pub const TEXT: Color32 = Color32::from_rgb(219, 222, 225);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(148, 155, 164);
pub const ACCENT: Color32 = Color32::from_rgb(88, 101, 242);
pub const GREEN: Color32 = Color32::from_rgb(35, 165, 90);
pub const RED: Color32 = Color32::from_rgb(218, 55, 60);
pub const LIVE: Color32 = Color32::from_rgb(237, 66, 69);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut v = Visuals::dark();
    v.panel_fill = BG_MAIN;
    v.window_fill = BG_DARK;
    v.extreme_bg_color = BG_INPUT;
    v.faint_bg_color = BG_DARK;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = Color32::from_rgb(0, 168, 252);
    v.selection.bg_fill = ACCENT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BG_DARKEST);
    v.widgets.inactive.weak_bg_fill = BG_INPUT;
    v.widgets.inactive.bg_fill = BG_INPUT;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(64, 66, 73);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(78, 80, 88);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(4);
    }
    v.window_corner_radius = CornerRadius::same(8);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
    });
}

/// Stable per-name color for usernames.
pub fn name_color(name: &str) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(242, 63, 67),
        Color32::from_rgb(240, 178, 50),
        Color32::from_rgb(35, 165, 90),
        Color32::from_rgb(0, 168, 252),
        Color32::from_rgb(165, 124, 255),
        Color32::from_rgb(235, 69, 158),
        Color32::from_rgb(26, 188, 156),
        Color32::from_rgb(230, 126, 34),
    ];
    let h = name
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    PALETTE[(h as usize) % PALETTE.len()]
}
