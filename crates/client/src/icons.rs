//! Phosphor icon font (MIT, https://phosphoricons.com), vendored in
//! `assets/Phosphor.ttf` because egui's default fonts don't cover these glyphs.
//! Codepoints from the Phosphor "regular" variant.

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

pub const ARROW_CLOCKWISE: &str = "\u{E036}";
pub const ARROW_SQUARE_OUT: &str = "\u{E5DE}";
pub const CHAT_CIRCLE_TEXT: &str = "\u{E16E}";
pub const GEAR: &str = "\u{E270}";
pub const HEADPHONES: &str = "\u{E2A6}";
pub const MICROPHONE: &str = "\u{E326}";
pub const MICROPHONE_SLASH: &str = "\u{E328}";
pub const MONITOR: &str = "\u{E32E}";
pub const PHONE_X: &str = "\u{E3C4}";
pub const RECORD: &str = "\u{E3EE}";
pub const SPEAKER_HIGH: &str = "\u{E44A}";
pub const SPEAKER_SLASH: &str = "\u{E45A}";
pub const STOP: &str = "\u{E46C}";
pub const WARNING: &str = "\u{E4E0}";
pub const X: &str = "\u{E4F6}";

/// Registers the icon font as a fallback for the proportional and monospace
/// families, so icons can be mixed freely with text.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/Phosphor.ttf"
        ))),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("phosphor".into());
    }
    ctx.set_fonts(fonts);
}
