// src/app.rs
//
// Main application state and egui render loop.
//
// Architecture
// ────────────
// • `BitcoinCompilerApp` lives on the main (UI) thread.
// • Background tasks run on the shared `Arc<tokio::runtime::Runtime>`.
// • Two `std::sync::mpsc` channels bridge the two worlds:
//     msg_rx     — background → UI  (AppMessage)
//     confirm_rx — background → UI  (ConfirmRequest, needs Yes/No)
// • `update()` drains both channels, renders modals, then the main UI.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use tokio::runtime::Runtime;

use crate::compiler::{compile_bitcoin, compile_electrs};
use crate::deps::check_dependencies_task;
use crate::env_setup::{brew_prefix, find_brew, macos_version, setup_build_environment};
use crate::github::{fetch_bitcoin_versions, fetch_electrs_versions};
use crate::messages::{log_msg, AppMessage, ConfirmRequest};

/// Maximum log lines retained in memory.
const MAX_LOG_LINES: usize = 4_000;
/// Drop to this many lines when the cap is hit.
const TRIM_TO_LINES: usize = MAX_LOG_LINES / 2;

// ─── Home directory ───────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ─── Modal ────────────────────────────────────────────────────────────────────

enum Modal {
    Alert {
        title:    String,
        message:  String,
        is_error: bool,
    },
    Confirm {
        title:       String,
        message:     String,
        response_tx: tokio::sync::oneshot::Sender<bool>,
    },
}

enum ModalAction {
    Close,
    Confirm(bool),
}

// ─── App state ────────────────────────────────────────────────────────────────

pub struct BitcoinCompilerApp {
    // Configuration
    target:         String, // "Bitcoin" | "Electrs" | "Both"
    cores:          usize,
    max_cores:      usize,
    build_dir:      String,

    // Version lists
    bitcoin_versions:  Vec<String>,
    selected_bitcoin:  String,
    electrs_versions:  Vec<String>,
    selected_electrs:  String,

    // UI state
    log_buffer:     String,
    log_line_count: usize, // maintained alongside log_buffer to avoid O(n) counting
    progress:       f32,
    is_busy:        bool,
    status_bar:     String,

    // Modal overlay
    modal: Option<Modal>,

    // Channels
    msg_rx:     Receiver<AppMessage>,
    msg_tx:     Sender<AppMessage>,
    confirm_rx: Receiver<ConfirmRequest>,
    confirm_tx: Sender<ConfirmRequest>,

    // Runtime
    runtime: Arc<Runtime>,

    // Detected environment
    brew:      Option<String>,
    brew_pfx:  Option<String>,
}

impl BitcoinCompilerApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        runtime: Arc<Runtime>,
        msg_rx: Receiver<AppMessage>,
        msg_tx: Sender<AppMessage>,
        confirm_rx: Receiver<ConfirmRequest>,
        confirm_tx: Sender<ConfirmRequest>,
    ) -> Self {
        let max_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let default_cores = max_cores.saturating_sub(1).max(1);

        let brew     = find_brew();
        let brew_pfx = brew.as_deref().map(brew_prefix);
        let macos    = macos_version(); // called once, reused below

        let status_bar = format!(
            "System: macOS {macos}  |  Homebrew: {}  |  CPUs: {max_cores}",
            brew_pfx.as_deref().unwrap_or("Not Found"),
        );

        let default_build_dir = home_dir()
            .map(|h| h.join("Downloads/bitcoin_builds").to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp/bitcoin_builds".to_owned());

        let mut app = Self {
            target:   "Bitcoin".to_owned(),
            cores:    default_cores,
            max_cores,
            build_dir: default_build_dir,

            bitcoin_versions: vec!["Loading...".to_owned()],
            selected_bitcoin: "Loading...".to_owned(),
            electrs_versions: vec!["Loading...".to_owned()],
            selected_electrs: "Loading...".to_owned(),

            log_buffer:     String::new(),
            log_line_count: 0,
            progress:       0.0,
            is_busy:        false,
            status_bar,

            modal: None,

            msg_rx,
            msg_tx,
            confirm_rx,
            confirm_tx,

            runtime,

            brew,
            brew_pfx,
        };

        // Splash
        let sep = "=".repeat(60);
        // `.to_owned()` ends the immutable borrow of `app.brew_pfx` before
        // the first `append_log` call takes `&mut self`.
        let brew_str = app.brew_pfx.as_deref().unwrap_or("Not Found").to_owned();
        let cpu_str  = app.max_cores.to_string();

        app.append_log(&format!("{sep}\nBitcoin Core & Electrs Compiler\n{sep}\n"));
        app.append_log(&format!("System: macOS {macos}\n"));
        app.append_log(&format!("Homebrew: {brew_str}\n"));
        app.append_log(&format!("CPU Cores: {cpu_str}\n"));
        app.append_log(&format!("{sep}\n\n"));
        app.append_log("👉 Click 'Check & Install Dependencies' to begin\n\n");
        app.append_log("📝 Note: Both Bitcoin and Electrs pull source from GitHub\n\n");

        app.spawn_refresh_all_versions();
        app
    }

    // ─── Log helpers ──────────────────────────────────────────────────────────

    fn append_log(&mut self, msg: &str) {
        // Count new lines in the incoming message.
        let new_lines = msg.chars().filter(|&c| c == '\n').count();
        self.log_buffer.push_str(msg);
        self.log_line_count += new_lines;

        // Trim when over cap — drop oldest half.
        if self.log_line_count > MAX_LOG_LINES {
            let drop_count = self.log_line_count.saturating_sub(TRIM_TO_LINES);
            let mut remaining = drop_count;
            if let Some(split_pos) = self.log_buffer.char_indices().find_map(|(i, c)| {
                if c == '\n' {
                    if remaining == 0 {
                        return Some(i);
                    }
                    remaining -= 1;
                }
                None
            }) {
                self.log_buffer = self.log_buffer[split_pos + 1..].to_owned();
                self.log_line_count = TRIM_TO_LINES;
            }
        }
    }

    // ─── Message drain ────────────────────────────────────────────────────────

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                AppMessage::Log(s) => self.append_log(&s),
                AppMessage::Progress(v) => self.progress = v.clamp(0.0, 1.0),
                AppMessage::BitcoinVersionsLoaded(versions) => {
                    if let Some(first) = versions.first() {
                        self.selected_bitcoin = first.clone();
                    }
                    self.bitcoin_versions = versions;
                }
                AppMessage::ElectrsVersionsLoaded(versions) => {
                    if let Some(first) = versions.first() {
                        self.selected_electrs = first.clone();
                    }
                    self.electrs_versions = versions;
                }
                AppMessage::ShowDialog { title, message, is_error } => {
                    self.modal = Some(Modal::Alert { title, message, is_error });
                }
                AppMessage::TaskDone => {
                    self.is_busy = false;
                    self.progress = 0.0;
                }
            }
        }

        // Only pop a confirm if no modal is already shown.
        if self.modal.is_none() {
            if let Ok(req) = self.confirm_rx.try_recv() {
                self.modal = Some(Modal::Confirm {
                    title:       req.title,
                    message:     req.message,
                    response_tx: req.response_tx,
                });
            }
        }
    }

    // ─── Background task spawners ─────────────────────────────────────────────

    fn spawn_check_deps(&mut self) {
        let brew = match self.brew.clone() {
            Some(b) => b,
            None => {
                self.modal = Some(Modal::Alert {
                    title:    "Missing Dependency".into(),
                    message:  "Homebrew not found!\nPlease install from https://brew.sh".into(),
                    is_error: true,
                });
                return;
            }
        };

        let env        = setup_build_environment(self.brew_pfx.as_deref());
        let tx         = self.msg_tx.clone(); // single clone serves both log and done
        let confirm_tx = self.confirm_tx.clone();

        self.is_busy = true;
        self.append_log("\n>>> Starting dependency check...\n");

        self.runtime.spawn(async move {
            match check_dependencies_task(brew, env, tx.clone(), confirm_tx).await {
                Ok(_) => {}
                Err(e) => {
                    tx.send(AppMessage::ShowDialog {
                        title:    "Error".into(),
                        message:  format!("Dependency check failed: {e}"),
                        is_error: true,
                    })
                    .ok();
                }
            }
            tx.send(AppMessage::TaskDone).ok();
        });
    }

    fn spawn_refresh_bitcoin_versions(&self) {
        let tx = self.msg_tx.clone();
        self.runtime.spawn(async move {
            log_msg(&tx, "\n📡 Fetching Bitcoin versions from GitHub...\n");
            match fetch_bitcoin_versions().await {
                Ok(versions) => {
                    log_msg(&tx, &format!("✓ Loaded {} Bitcoin versions\n", versions.len()));
                    tx.send(AppMessage::BitcoinVersionsLoaded(versions)).ok();
                }
                Err(e) => {
                    log_msg(&tx, &format!("⚠️  Could not fetch Bitcoin versions: {e}\n"));
                    tx.send(AppMessage::ShowDialog {
                        title:    "Network Error".into(),
                        message:  "Could not fetch Bitcoin versions.\nCheck your internet connection.".into(),
                        is_error: false,
                    })
                    .ok();
                }
            }
        });
    }

    fn spawn_refresh_electrs_versions(&self) {
        let tx = self.msg_tx.clone();
        self.runtime.spawn(async move {
            log_msg(&tx, "\n📡 Fetching Electrs versions from GitHub...\n");
            match fetch_electrs_versions().await {
                Ok(versions) => {
                    log_msg(&tx, &format!("✓ Loaded {} Electrs versions\n", versions.len()));
                    tx.send(AppMessage::ElectrsVersionsLoaded(versions)).ok();
                }
                Err(e) => {
                    log_msg(&tx, &format!("⚠️  Could not fetch Electrs versions: {e}\n"));
                    tx.send(AppMessage::ShowDialog {
                        title:    "Network Error".into(),
                        message:  "Could not fetch Electrs versions.\nCheck your internet connection.".into(),
                        is_error: false,
                    })
                    .ok();
                }
            }
        });
    }

    fn spawn_refresh_all_versions(&self) {
        self.spawn_refresh_bitcoin_versions();
        self.spawn_refresh_electrs_versions();
    }

    fn spawn_compile(&mut self) {
        let target      = self.target.clone();
        let cores       = self.cores;
        let build_dir   = PathBuf::from(&self.build_dir);
        let bitcoin_ver = self.selected_bitcoin.clone();
        let electrs_ver = self.selected_electrs.clone();

        // Validate versions before starting.
        let loading = |s: &str| s.is_empty() || s == "Loading...";
        if (target == "Bitcoin" || target == "Both") && loading(&bitcoin_ver) {
            self.modal = Some(Modal::Alert {
                title:    "Error".into(),
                message:  "Please wait for Bitcoin versions to load, or click Refresh".into(),
                is_error: true,
            });
            return;
        }
        if (target == "Electrs" || target == "Both") && loading(&electrs_ver) {
            self.modal = Some(Modal::Alert {
                title:    "Error".into(),
                message:  "Please wait for Electrs versions to load, or click Refresh".into(),
                is_error: true,
            });
            return;
        }

        let env = setup_build_environment(self.brew_pfx.as_deref());
        let tx  = self.msg_tx.clone(); // one clone; used for log, progress, done

        self.is_busy  = true;
        self.progress = 0.0;

        self.runtime.spawn(async move {
            tx.send(AppMessage::Progress(0.05)).ok();

            let mut output_dirs: Vec<String> = Vec::new();
            let mut error_occurred = false;

            // ── Bitcoin ────────────────────────────────────────────────────────
            if target == "Bitcoin" || target == "Both" {
                tx.send(AppMessage::Progress(0.1)).ok();
                match compile_bitcoin(&bitcoin_ver, &build_dir, cores, &env, &tx).await {
                    Ok(dir) => {
                        output_dirs.push(dir.to_string_lossy().into_owned());
                        tx.send(AppMessage::Progress(if target == "Both" {
                            0.5
                        } else {
                            0.95
                        }))
                        .ok();
                    }
                    Err(e) => {
                        log_msg(&tx, &format!("\n❌ Compilation failed: {e}\n"));
                        tx.send(AppMessage::ShowDialog {
                            title:    "Compilation Failed".into(),
                            message:  e.to_string(),
                            is_error: true,
                        })
                        .ok();
                        error_occurred = true;
                    }
                }
            }

            // ── Electrs ────────────────────────────────────────────────────────
            if !error_occurred && (target == "Electrs" || target == "Both") {
                tx.send(AppMessage::Progress(if target == "Both" { 0.55 } else { 0.1 }))
                    .ok();
                match compile_electrs(&electrs_ver, &build_dir, cores, &env, &tx).await {
                    Ok(dir) => {
                        output_dirs.push(dir.to_string_lossy().into_owned());
                        tx.send(AppMessage::Progress(1.0)).ok();
                    }
                    Err(e) => {
                        log_msg(&tx, &format!("\n❌ Compilation failed: {e}\n"));
                        tx.send(AppMessage::ShowDialog {
                            title:    "Compilation Failed".into(),
                            message:  e.to_string(),
                            is_error: true,
                        })
                        .ok();
                        error_occurred = true;
                    }
                }
            }

            // ── Success dialog ─────────────────────────────────────────────────
            if !error_occurred {
                tx.send(AppMessage::Progress(1.0)).ok();
                let dirs_list = output_dirs
                    .iter()
                    .map(|d| format!("• {d}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                tx.send(AppMessage::ShowDialog {
                    title:    "Compilation Complete".into(),
                    message:  format!(
                        "✅ {target} compilation completed successfully!\n\nBinaries saved to:\n{dirs_list}"
                    ),
                    is_error: false,
                })
                .ok();
            }

            tx.send(AppMessage::TaskDone).ok();
        });
    }

    // ─── Modal rendering ──────────────────────────────────────────────────────

    fn render_modal(&mut self, ctx: &egui::Context) {
        let action: Option<ModalAction> = match &self.modal {
            None => return,

            Some(Modal::Alert { title, message, is_error }) => {
                let title_str = title.clone();
                let msg_str   = message.clone();
                let err       = *is_error;
                let mut close = false;

                egui::Window::new(title_str.as_str())
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .collapsible(false)
                    .resizable(false)
                    .min_width(340.0)
                    .show(ctx, |ui| {
                        let color = if err {
                            egui::Color32::from_rgb(230, 90, 90)
                        } else {
                            egui::Color32::from_rgb(90, 190, 90)
                        };
                        ui.colored_label(color, if err { "⛔ Error" } else { "ℹ  Info" });
                        ui.separator();
                        ui.label(msg_str.as_str());
                        ui.add_space(8.0);
                        if ui.button("  OK  ").clicked() {
                            close = true;
                        }
                    });

                if close { Some(ModalAction::Close) } else { None }
            }

            Some(Modal::Confirm { title, message, .. }) => {
                let title_str   = title.clone();
                let msg_str     = message.clone();
                let mut answer: Option<bool> = None;

                egui::Window::new(title_str.as_str())
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .collapsible(false)
                    .resizable(false)
                    .min_width(360.0)
                    .show(ctx, |ui| {
                        ui.label(msg_str.as_str());
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("  Yes  ").clicked() { answer = Some(true); }
                            if ui.button("  No  ").clicked()  { answer = Some(false); }
                        });
                    });

                answer.map(ModalAction::Confirm)
            }
        };

        match action {
            None => {}
            Some(ModalAction::Close) => {
                self.modal = None;
            }
            Some(ModalAction::Confirm(answer)) => {
                if let Some(Modal::Confirm { response_tx, .. }) = self.modal.take() {
                    response_tx.send(answer).ok();
                }
            }
        }
    }
}

// ─── eframe::App ──────────────────────────────────────────────────────────────

impl eframe::App for BitcoinCompilerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();
        self.render_modal(ctx);

        // Status bar
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&self.status_bar).small().weak());
            });
        });

        // Main panel
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.set_min_width(800.0);

            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Bitcoin Core & Electrs Compiler")
                        .size(20.0)
                        .strong(),
                );
                ui.add_space(6.0);
            });

            // Step 1: Dependency check
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Step 1:").strong());
                if ui
                    .add_enabled(
                        !self.is_busy,
                        egui::Button::new("Check & Install Dependencies"),
                    )
                    .clicked()
                {
                    self.spawn_check_deps();
                }
            });

            ui.separator();

            // Step 2: Build settings
            ui.group(|ui| {
                ui.label(egui::RichText::new("Step 2: Select What to Compile").strong());
                ui.add_space(4.0);

                egui::Grid::new("settings_grid")
                    .num_columns(5)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Target:");
                        egui::ComboBox::from_id_source("target_combo")
                            .selected_text(&self.target)
                            .width(130.0)
                            .show_ui(ui, |ui: &mut egui::Ui| {
                                for opt in &["Bitcoin", "Electrs", "Both"] {
                                    ui.selectable_value(
                                        &mut self.target,
                                        opt.to_string(),
                                        *opt,
                                    );
                                }
                            });

                        ui.label("CPU Cores:");
                        ui.add(
                            egui::DragValue::new(&mut self.cores)
                                .range(1..=self.max_cores)
                                .speed(1.0),
                        );
                        ui.label(
                            egui::RichText::new(format!("(max: {})", self.max_cores))
                                .small()
                                .weak(),
                        );
                        ui.end_row();

                        ui.label("Build Directory:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.build_dir)
                                .desired_width(360.0),
                        );
                        ui.label(""); // spacer
                        ui.label(""); // spacer
                        if ui.button("Browse…").clicked() {
                            // rfd::FileDialog is synchronous; on macOS it runs
                            // NSOpenPanel which has its own event loop, so the
                            // egui frame is not blocked in the usual sense.
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                self.build_dir = folder.to_string_lossy().into_owned();
                            }
                        }
                        ui.end_row();
                    });
            });

            ui.add_space(4.0);

            // Step 3: Version selection
            ui.group(|ui| {
                ui.label(egui::RichText::new("Step 3: Select Versions").strong());
                ui.add_space(4.0);

                egui::Grid::new("versions_grid")
                    .num_columns(3)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        // Bitcoin — iterate by reference, no clone of Vec
                        ui.label("Bitcoin Version:");
                        egui::ComboBox::from_id_source("bitcoin_combo")
                            .selected_text(&self.selected_bitcoin)
                            .width(180.0)
                            .show_ui(ui, |ui: &mut egui::Ui| {
                                for v in &self.bitcoin_versions {
                                    ui.selectable_value(
                                        &mut self.selected_bitcoin,
                                        v.clone(),
                                        v.as_str(),
                                    );
                                }
                            });
                        if ui.button("Refresh").clicked() {
                            self.spawn_refresh_bitcoin_versions();
                        }
                        ui.end_row();

                        // Electrs — iterate by reference, no clone of Vec
                        ui.label("Electrs Version:");
                        egui::ComboBox::from_id_source("electrs_combo")
                            .selected_text(&self.selected_electrs)
                            .width(180.0)
                            .show_ui(ui, |ui: &mut egui::Ui| {
                                for v in &self.electrs_versions {
                                    ui.selectable_value(
                                        &mut self.selected_electrs,
                                        v.clone(),
                                        v.as_str(),
                                    );
                                }
                            });
                        if ui.button("Refresh").clicked() {
                            self.spawn_refresh_electrs_versions();
                        }
                        ui.end_row();
                    });
            });

            ui.add_space(6.0);

            // Progress bar
            ui.label("Progress:");
            ui.add(
                egui::ProgressBar::new(self.progress)
                    .desired_width(ui.available_width())
                    .animate(self.is_busy),
            );

            ui.add_space(6.0);

            // Build log
            ui.label(egui::RichText::new("Build Log").strong());

            let log_frame = egui::Frame {
                fill:         egui::Color32::from_rgb(18, 18, 18),
                inner_margin: egui::Margin::same(8.0),
                stroke:       egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
                ..Default::default()
            };

            let available_height = ui.available_height() - 56.0;

            log_frame.show(ui, |ui| {
                egui::ScrollArea::both()
                    .stick_to_bottom(true)
                    .max_height(available_height.max(120.0))
                    .min_scrolled_height(120.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(&self.log_buffer)
                                .color(egui::Color32::from_rgb(0, 215, 0))
                                .monospace()
                                .size(11.5),
                        );
                    });
            });

            ui.add_space(6.0);

            // Compile button
            ui.vertical_centered(|ui| {
                if ui
                    .add_enabled(
                        !self.is_busy,
                        egui::Button::new(
                            egui::RichText::new("🚀  Start Compilation").size(14.0),
                        )
                        .min_size(egui::vec2(210.0, 36.0)),
                    )
                    .clicked()
                {
                    self.spawn_compile();
                }
            });
        });

        // Repaint scheduling
        ctx.request_repaint_after(if self.is_busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        });
    }
}
