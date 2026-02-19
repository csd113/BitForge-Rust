// src/app.rs
//
// BitForge — main application state and egui render loop.

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
/// Fixed pixel height for the build log terminal panel.
const TERMINAL_HEIGHT: f32 = 260.0;
/// Max width for the centred content column.
const CONTENT_WIDTH: f32 = 860.0;

// ─── Colour palette (macOS light mode) ───────────────────────────────────────

mod pal {
    use egui::Color32;
    pub const ACCENT:        Color32 = Color32::from_rgb(0, 122, 255);    // macOS blue
    pub const ACCENT_TEXT:   Color32 = Color32::WHITE;
    pub const SURFACE:       Color32 = Color32::from_rgb(250, 250, 252);  // card bg
    pub const BORDER:        Color32 = Color32::from_rgb(212, 212, 218);
    pub const LABEL_MUTED:   Color32 = Color32::from_rgb(128, 128, 138);
    pub const TEXT_PRIMARY:  Color32 = Color32::from_rgb(20,  20,  25);
    pub const SUCCESS:       Color32 = Color32::from_rgb(52,  199, 89);   // macOS green
    pub const DANGER:        Color32 = Color32::from_rgb(255, 59,  48);   // macOS red
    pub const PAGE_BG:       Color32 = Color32::from_rgb(236, 236, 240);  // window bg
    pub const STATUS_BG:     Color32 = Color32::from_rgb(242, 242, 246);

    // Terminal stays dark
    pub const TERM_BG:     Color32 = Color32::from_rgb(18, 18, 18);
    pub const TERM_TEXT:   Color32 = Color32::from_rgb(0, 215, 0);
    pub const TERM_BORDER: Color32 = Color32::from_rgb(55, 55, 55);
}

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

pub struct BitForgeApp {
    // Configuration
    target:    String,
    cores:     usize,
    max_cores: usize,
    build_dir: String,

    // Version lists
    bitcoin_versions: Vec<String>,
    selected_bitcoin: String,
    electrs_versions: Vec<String>,
    selected_electrs: String,

    // UI state
    log_buffer:     String,
    log_line_count: usize,
    progress:       f32,
    is_busy:        bool,
    status_bar:     String,

    // Modal
    modal: Option<Modal>,

    // Channels
    msg_rx:     Receiver<AppMessage>,
    msg_tx:     Sender<AppMessage>,
    confirm_rx: Receiver<ConfirmRequest>,
    confirm_tx: Sender<ConfirmRequest>,

    // Runtime
    runtime: Arc<Runtime>,

    // Environment
    brew:     Option<String>,
    brew_pfx: Option<String>,
}

impl BitForgeApp {
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
        let macos    = macos_version();

        let status_bar = format!(
            "macOS {}   ·   Homebrew: {}   ·   {} CPUs",
            macos,
            brew_pfx.as_deref().unwrap_or("not found"),
            max_cores,
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

        // Splash — borrow ends before first append_log call
        let sep      = "=".repeat(60);
        let brew_str = app.brew_pfx.as_deref().unwrap_or("Not Found").to_owned();
        let cpus     = app.max_cores;

        app.append_log(&format!("{sep}\nBitForge — Bitcoin Core & Electrs Compiler\n{sep}\n"));
        app.append_log(&format!("System: macOS {macos}\n"));
        app.append_log(&format!("Homebrew: {brew_str}\n"));
        app.append_log(&format!("CPU Cores: {cpus}\n"));
        app.append_log(&format!("{sep}\n\n"));
        app.append_log("👉 Click \"Check & Install Dependencies\" to begin.\n\n");
        app.append_log("📝 Bitcoin Core and Electrs are compiled from source via GitHub.\n\n");

        app.spawn_refresh_all_versions();
        app
    }

    // ─── Log helpers ──────────────────────────────────────────────────────────

    fn append_log(&mut self, msg: &str) {
        let new_lines = msg.chars().filter(|&c| c == '\n').count();
        self.log_buffer.push_str(msg);
        self.log_line_count += new_lines;

        if self.log_line_count > MAX_LOG_LINES {
            let drop_count = self.log_line_count.saturating_sub(TRIM_TO_LINES);
            let mut remaining = drop_count;
            if let Some(split_pos) = self.log_buffer.char_indices().find_map(|(i, c)| {
                if c == '\n' {
                    if remaining == 0 { return Some(i); }
                    remaining -= 1;
                }
                None
            }) {
                self.log_buffer     = self.log_buffer[split_pos + 1..].to_owned();
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
                    self.is_busy  = false;
                    self.progress = 0.0;
                }
            }
        }

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
                    title:    "Homebrew Not Found".into(),
                    message:  "Homebrew is required.\nInstall it from https://brew.sh then restart BitForge.".into(),
                    is_error: true,
                });
                return;
            }
        };

        let env        = setup_build_environment(self.brew_pfx.as_deref());
        let tx         = self.msg_tx.clone();
        let confirm_tx = self.confirm_tx.clone();

        self.is_busy = true;
        self.append_log("\n>>> Starting dependency check...\n");

        self.runtime.spawn(async move {
            match check_dependencies_task(brew, env, tx.clone(), confirm_tx).await {
                Ok(_) => {}
                Err(e) => {
                    tx.send(AppMessage::ShowDialog {
                        title:    "Error".into(),
                        message:  format!("Dependency check failed:\n{e}"),
                        is_error: true,
                    }).ok();
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
                    }).ok();
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
                    }).ok();
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

        let loading = |s: &str| s.is_empty() || s == "Loading...";
        if (target == "Bitcoin" || target == "Both") && loading(&bitcoin_ver) {
            self.modal = Some(Modal::Alert {
                title:    "Not Ready".into(),
                message:  "Please wait for Bitcoin versions to load, or click Refresh.".into(),
                is_error: true,
            });
            return;
        }
        if (target == "Electrs" || target == "Both") && loading(&electrs_ver) {
            self.modal = Some(Modal::Alert {
                title:    "Not Ready".into(),
                message:  "Please wait for Electrs versions to load, or click Refresh.".into(),
                is_error: true,
            });
            return;
        }

        let env = setup_build_environment(self.brew_pfx.as_deref());
        let tx  = self.msg_tx.clone();

        self.is_busy  = true;
        self.progress = 0.0;

        self.runtime.spawn(async move {
            tx.send(AppMessage::Progress(0.05)).ok();
            let mut output_dirs: Vec<String> = Vec::new();
            let mut error_occurred = false;

            if target == "Bitcoin" || target == "Both" {
                tx.send(AppMessage::Progress(0.1)).ok();
                match compile_bitcoin(&bitcoin_ver, &build_dir, cores, &env, &tx).await {
                    Ok(dir) => {
                        output_dirs.push(dir.to_string_lossy().into_owned());
                        tx.send(AppMessage::Progress(if target == "Both" { 0.5 } else { 0.95 })).ok();
                    }
                    Err(e) => {
                        log_msg(&tx, &format!("\n❌ Compilation failed: {e}\n"));
                        tx.send(AppMessage::ShowDialog {
                            title: "Compilation Failed".into(),
                            message: e.to_string(),
                            is_error: true,
                        }).ok();
                        error_occurred = true;
                    }
                }
            }

            if !error_occurred && (target == "Electrs" || target == "Both") {
                tx.send(AppMessage::Progress(if target == "Both" { 0.55 } else { 0.1 })).ok();
                match compile_electrs(&electrs_ver, &build_dir, cores, &env, &tx).await {
                    Ok(dir) => {
                        output_dirs.push(dir.to_string_lossy().into_owned());
                        tx.send(AppMessage::Progress(1.0)).ok();
                    }
                    Err(e) => {
                        log_msg(&tx, &format!("\n❌ Compilation failed: {e}\n"));
                        tx.send(AppMessage::ShowDialog {
                            title: "Compilation Failed".into(),
                            message: e.to_string(),
                            is_error: true,
                        }).ok();
                        error_occurred = true;
                    }
                }
            }

            if !error_occurred {
                tx.send(AppMessage::Progress(1.0)).ok();
                let dirs_list = output_dirs.iter()
                    .map(|d| format!("• {d}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                tx.send(AppMessage::ShowDialog {
                    title:    "Compilation Complete".into(),
                    message:  format!("✅ {target} compiled successfully!\n\nBinaries saved to:\n{dirs_list}"),
                    is_error: false,
                }).ok();
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
                    .min_width(360.0)
                    .max_width(480.0)
                    .show(ctx, |ui| {
                        ui.add_space(2.0);
                        let (icon, color) = if err {
                            ("⛔  Error", pal::DANGER)
                        } else {
                            ("✅  Success", pal::SUCCESS)
                        };
                        ui.colored_label(color, egui::RichText::new(icon).strong().size(14.0));
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.label(msg_str.as_str());
                        ui.add_space(12.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                            if ui.add(accent_button("OK")).clicked() {
                                close = true;
                            }
                        });
                        ui.add_space(2.0);
                    });

                if close { Some(ModalAction::Close) } else { None }
            }

            Some(Modal::Confirm { title, message, .. }) => {
                let title_str = title.clone();
                let msg_str   = message.clone();
                let mut answer: Option<bool> = None;

                egui::Window::new(title_str.as_str())
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .collapsible(false)
                    .resizable(false)
                    .min_width(380.0)
                    .max_width(500.0)
                    .show(ctx, |ui| {
                        ui.add_space(6.0);
                        ui.label(msg_str.as_str());
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                            if ui.add(accent_button("Install")).clicked() {
                                answer = Some(true);
                            }
                            ui.add_space(6.0);
                            if ui.button(egui::RichText::new("Cancel").size(13.0)).clicked() {
                                answer = Some(false);
                            }
                        });
                        ui.add_space(2.0);
                    });

                answer.map(ModalAction::Confirm)
            }
        };

        match action {
            None => {}
            Some(ModalAction::Close) => { self.modal = None; }
            Some(ModalAction::Confirm(answer)) => {
                if let Some(Modal::Confirm { response_tx, .. }) = self.modal.take() {
                    response_tx.send(answer).ok();
                }
            }
        }
    }

    // ─── Content renderer (called inside centred column) ──────────────────────

    fn render_content(&mut self, ui: &mut egui::Ui) {
        // Header
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("⚙  BitForge")
                    .size(26.0)
                    .strong()
                    .color(pal::TEXT_PRIMARY),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Bitcoin Core & Electrs Compiler for macOS")
                    .size(13.0)
                    .color(pal::LABEL_MUTED),
            );
        });

        ui.add_space(20.0);

        // ── Step 1 ────────────────────────────────────────────────────────────
        section_card(ui, "Step 1 — Check & Install Dependencies", |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Scans for required Homebrew packages and the Rust toolchain.",
                    )
                    .size(12.5)
                    .color(pal::LABEL_MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!self.is_busy, accent_button("Check & Install")).clicked() {
                        self.spawn_check_deps();
                    }
                });
            });
        });

        ui.add_space(10.0);

        // ── Step 2 ────────────────────────────────────────────────────────────
        section_card(ui, "Step 2 — Configure Build", |ui| {
            egui::Grid::new("settings_grid")
                .num_columns(4)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    // Row 1: Target + Cores
                    ui.label(egui::RichText::new("Target").color(pal::LABEL_MUTED));
                    egui::ComboBox::from_id_source("target_combo")
                        .selected_text(&self.target)
                        .width(140.0)
                        .show_ui(ui, |ui: &mut egui::Ui| {
                            for opt in &["Bitcoin", "Electrs", "Both"] {
                                ui.selectable_value(&mut self.target, opt.to_string(), *opt);
                            }
                        });

                    ui.label(egui::RichText::new("CPU Cores").color(pal::LABEL_MUTED));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.cores)
                                .range(1..=self.max_cores)
                                .speed(1.0),
                        );
                        ui.label(
                            egui::RichText::new(format!("of {}", self.max_cores))
                                .small()
                                .color(pal::LABEL_MUTED),
                        );
                    });
                    ui.end_row();

                    // Row 2: Build directory
                    ui.label(egui::RichText::new("Output Dir").color(pal::LABEL_MUTED));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.build_dir)
                            .desired_width(440.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    ui.label(""); // spacer
                    if ui.button("Browse…").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            self.build_dir = folder.to_string_lossy().into_owned();
                        }
                    }
                    ui.end_row();
                });
        });

        ui.add_space(10.0);

        // ── Step 3 ────────────────────────────────────────────────────────────
        section_card(ui, "Step 3 — Select Versions", |ui| {
            egui::Grid::new("versions_grid")
                .num_columns(4)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Bitcoin Core").color(pal::LABEL_MUTED));
                    egui::ComboBox::from_id_source("bitcoin_combo")
                        .selected_text(&self.selected_bitcoin)
                        .width(200.0)
                        .show_ui(ui, |ui: &mut egui::Ui| {
                            for v in &self.bitcoin_versions {
                                ui.selectable_value(
                                    &mut self.selected_bitcoin,
                                    v.clone(),
                                    v.as_str(),
                                );
                            }
                        });
                    if ui.button("↻  Refresh").clicked() {
                        self.spawn_refresh_bitcoin_versions();
                    }
                    ui.label("");
                    ui.end_row();

                    ui.label(egui::RichText::new("Electrs").color(pal::LABEL_MUTED));
                    egui::ComboBox::from_id_source("electrs_combo")
                        .selected_text(&self.selected_electrs)
                        .width(200.0)
                        .show_ui(ui, |ui: &mut egui::Ui| {
                            for v in &self.electrs_versions {
                                ui.selectable_value(
                                    &mut self.selected_electrs,
                                    v.clone(),
                                    v.as_str(),
                                );
                            }
                        });
                    if ui.button("↻  Refresh").clicked() {
                        self.spawn_refresh_electrs_versions();
                    }
                    ui.label("");
                    ui.end_row();
                });
        });

        ui.add_space(10.0);

        // ── Progress ──────────────────────────────────────────────────────────
        section_card(ui, "Build Progress", |ui| {
            let label = if self.is_busy {
                format!("{:.0}%", self.progress * 100.0)
            } else if self.progress >= 1.0 {
                "Complete".to_owned()
            } else {
                "Idle".to_owned()
            };

            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(self.progress)
                        .desired_width(ui.available_width() - 56.0)
                        .animate(self.is_busy)
                        .text(""),
                );
                ui.add_space(6.0);
                ui.label(egui::RichText::new(label).small().color(pal::LABEL_MUTED));
            });
        });

        ui.add_space(10.0);

        // ── Build log terminal — FIXED HEIGHT, never resizes ──────────────────
        ui.label(egui::RichText::new("Build Log").strong().color(pal::TEXT_PRIMARY));
        ui.add_space(4.0);

        egui::Frame {
            fill:          pal::TERM_BG,
            stroke:        egui::Stroke::new(1.0, pal::TERM_BORDER),
            inner_margin:  egui::Margin::same(10.0),
            rounding: egui::Rounding::same(8.0),
            outer_margin:  egui::Margin::ZERO,
            ..Default::default()
        }
        .show(ui, |ui| {
            // Hard-pin both min and max to the same value so egui never
            // allocates more or less space as log content grows.
            ui.set_min_height(TERMINAL_HEIGHT);
            ui.set_max_height(TERMINAL_HEIGHT);

            egui::ScrollArea::vertical()
                .id_source("build_log")
                .stick_to_bottom(true)
                .max_height(TERMINAL_HEIGHT)
                .min_scrolled_height(TERMINAL_HEIGHT)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(&self.log_buffer)
                            .color(pal::TERM_TEXT)
                            .monospace()
                            .size(11.5),
                    );
                });
        });

        ui.add_space(18.0);

        // ── Compile button ────────────────────────────────────────────────────
        ui.vertical_centered(|ui| {
            let label = if self.is_busy { "⏳  Compiling…" } else { "🚀  Start Compilation" };
            if ui
                .add_enabled(
                    !self.is_busy,
                    egui::Button::new(
                        egui::RichText::new(label)
                            .size(15.0)
                            .color(pal::ACCENT_TEXT)
                            .strong(),
                    )
                    .fill(pal::ACCENT)
                    .stroke(egui::Stroke::NONE)
                    .min_size(egui::vec2(220.0, 40.0)),
                )
                .clicked()
            {
                self.spawn_compile();
            }
        });
    }
}

// ─── UI helpers ───────────────────────────────────────────────────────────────

/// macOS-style filled accent button.
fn accent_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(label)
            .color(pal::ACCENT_TEXT)
            .strong()
            .size(13.0),
    )
    .fill(pal::ACCENT)
    .stroke(egui::Stroke::NONE)
    .min_size(egui::vec2(100.0, 28.0))
}

/// Render a titled card section.
fn section_card(ui: &mut egui::Ui, heading: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame {
        fill:          pal::SURFACE,
        stroke:        egui::Stroke::new(1.0, pal::BORDER),
        rounding: egui::Rounding::same(10.0),
        inner_margin:  egui::Margin::symmetric(16.0, 12.0),
        outer_margin:  egui::Margin::ZERO,
        ..Default::default()
    }
    .show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            egui::RichText::new(heading)
                .strong()
                .size(13.0)
                .color(pal::TEXT_PRIMARY),
        );
        ui.add_space(8.0);
        body(ui);
    });
}

// ─── eframe::App ──────────────────────────────────────────────────────────────

impl eframe::App for BitForgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();
        self.render_modal(ctx);

        // ── Status bar ────────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar")
            .frame(egui::Frame {
                fill:         pal::STATUS_BG,
                stroke:       egui::Stroke::new(1.0, pal::BORDER),
                inner_margin: egui::Margin::symmetric(16.0, 5.0),
                ..Default::default()
            })
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(&self.status_bar)
                        .small()
                        .color(pal::LABEL_MUTED),
                );
            });

        // ── Main window ───────────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame {
                fill:         pal::PAGE_BG,
                inner_margin: egui::Margin::ZERO,
                ..Default::default()
            })
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        // Horizontal centering: equal padding on both sides.
                        let total = ui.available_width();
                        let pad   = ((total - CONTENT_WIDTH) / 2.0).max(16.0);

                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            ui.add_space(pad);
                            ui.vertical(|ui| {
                                ui.set_width(CONTENT_WIDTH.min(total - pad * 2.0));
                                self.render_content(ui);
                            });
                        });
                        ui.add_space(28.0);
                    });
            });

        ctx.request_repaint_after(if self.is_busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        });
    }
}
