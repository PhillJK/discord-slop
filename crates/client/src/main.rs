// Release builds on Windows are GUI apps (no console window); debug builds keep
// the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! qchat desktop client.

mod app;
mod dialogs;
mod icons;
mod prefs;
mod theme;
mod views;

use qchat_client_core::KnownHosts;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,qchat=info")),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("qchat")
            .with_app_id("qchat")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "qchat",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            icons::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let known =
                KnownHosts::default_path().map_or_else(KnownHosts::in_memory, KnownHosts::load);
            let (handle, state) = qchat_client_core::runtime::spawn(
                known,
                std::sync::Arc::new(move || ctx.request_repaint()),
            )?;
            Ok(Box::new(app::App::new(cc, handle, state)))
        }),
    )
}
