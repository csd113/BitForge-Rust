// src/main.rs
//
// Entry point for the Bitcoin & Electrs Compiler.
//
// Responsibilities:
//   1. Widen PATH in the process environment for all child spawns.
//   2. Create the tokio multi-thread runtime.
//   3. Create the std::sync::mpsc channels.
//   4. Launch the eframe event loop on the main thread.

mod app;
mod compiler;
mod deps;
mod env_setup;
mod github;
mod messages;
mod process;

use std::sync::Arc;

use app::BitcoinCompilerApp;
use env_setup::{brew_prefix, find_brew, setup_build_environment};

fn main() -> eframe::Result<()> {
    // ── 0. Widen PATH ─────────────────────────────────────────────────────────
    // set_var is safe here: no other threads are running yet.
    // Note: std::env::set_var will require `unsafe` in future Rust editions.
    {
        let brew = find_brew();
        let pfx  = brew.as_deref().map(brew_prefix);
        let env  = setup_build_environment(pfx.as_deref());
        if let Some(path) = env.get("PATH") {
            // SAFETY: single-threaded at this point.
            std::env::set_var("PATH", path);
        }
    }

    // ── 1. Tokio runtime ──────────────────────────────────────────────────────
    // Scale worker threads to available CPUs, capped at 8.
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
    let (msg_tx, msg_rx)         = std::sync::mpsc::channel::<messages::AppMessage>();
    let (confirm_tx, confirm_rx) = std::sync::mpsc::channel::<messages::ConfirmRequest>();

    // ── 3. eframe native window options ──────────────────────────────────────
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Bitcoin & Electrs Compiler for macOS")
            .with_inner_size([920.0, 820.0])
            .with_min_inner_size([700.0, 600.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    // ── 4. Run eframe on the main thread ──────────────────────────────────────
    eframe::run_native(
        "Bitcoin & Electrs Compiler for macOS",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(BitcoinCompilerApp::new(
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
