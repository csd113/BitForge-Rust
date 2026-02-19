// src/main.rs — BitForge entry point.

mod app;
mod compiler;
mod deps;
mod env_setup;
mod github;
mod messages;
mod process;

use std::sync::Arc;

use app::BitForgeApp;
use env_setup::{brew_prefix, find_brew, setup_build_environment};

fn main() -> eframe::Result<()> {
    // ── 0. Widen PATH for child processes ─────────────────────────────────────
    // SAFETY: single-threaded at this point; no other threads yet.
    {
        let brew = find_brew();
        let pfx  = brew.as_deref().map(brew_prefix);
        let env  = setup_build_environment(pfx.as_deref());
        if let Some(path) = env.get("PATH") {
            std::env::set_var("PATH", path);
        }
    }

    // ── 1. Tokio runtime ──────────────────────────────────────────────────────
    let worker_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(worker_threads)
            .build()
            .expect("Failed to create tokio runtime"),
    );

    // ── 2. Channels ───────────────────────────────────────────────────────────
    let (msg_tx,     msg_rx)     = std::sync::mpsc::channel::<messages::AppMessage>();
    let (confirm_tx, confirm_rx) = std::sync::mpsc::channel::<messages::ConfirmRequest>();

    // ── 3. Window options ─────────────────────────────────────────────────────
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("BitForge")
            .with_inner_size([960.0, 840.0])
            .with_min_inner_size([720.0, 620.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    // ── 4. Run on main thread ─────────────────────────────────────────────────
    eframe::run_native(
        "BitForge",
        native_options,
        Box::new(move |cc| {
            // Light mode with macOS-tuned visuals.
            let mut visuals = egui::Visuals::light();

            // Slightly warm widget backgrounds and selection colour.
            visuals.selection.bg_fill = egui::Color32::from_rgb(0, 122, 255);
            visuals.selection.stroke  = egui::Stroke::NONE;
            visuals.hyperlink_color   = egui::Color32::from_rgb(0, 122, 255);

            // Softer window/popup shadow for a macOS look.
            visuals.popup_shadow  = egui::Shadow::NONE;
            visuals.window_shadow = egui::Shadow {
                offset: egui::Vec2::new(0.0, 4.0),
                blur:   16.0,
                spread: 0.0,
                color:  egui::Color32::from_black_alpha(40),
            };

            cc.egui_ctx.set_visuals(visuals);

            Ok(Box::new(BitForgeApp::new(
                cc,
                runtime,
                msg_rx,
                msg_tx,
                confirm_rx,
                confirm_tx,
            )))
        }),
    )
}
