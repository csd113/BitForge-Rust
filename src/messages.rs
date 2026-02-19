// src/messages.rs
//
// All message types that flow between background tokio tasks and the main
// egui render thread.  Using typed enums keeps the contract explicit and
// compiler-checked.
//
// Also provides `log_msg`, the single shared helper used by every module
// to push a line into the UI terminal, eliminating the per-module duplicate.

use std::sync::mpsc::Sender;
use tokio::sync::oneshot;

// ─── AppMessage ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AppMessage {
    /// Append text to the dark terminal log widget.
    Log(String),

    /// Set the progress bar value (0.0 – 1.0).
    Progress(f32),

    /// Populate the Bitcoin version combobox.
    BitcoinVersionsLoaded(Vec<String>),

    /// Populate the Electrs version combobox.
    ElectrsVersionsLoaded(Vec<String>),

    /// Show an informational / error overlay (no reply needed).
    ShowDialog {
        title:    String,
        message:  String,
        is_error: bool,
    },

    /// A background task completed — re-enable the Compile button.
    TaskDone,
}

// ─── ConfirmRequest ───────────────────────────────────────────────────────────

pub struct ConfirmRequest {
    pub title:       String,
    pub message:     String,
    /// UI sends `true` (Yes) or `false` (No) back through this channel.
    pub response_tx: oneshot::Sender<bool>,
}

// ─── Shared log helper ────────────────────────────────────────────────────────

/// Push a log line to the UI terminal.
/// Errors are silently ignored — the UI may be shutting down.
#[inline]
pub fn log_msg(tx: &Sender<AppMessage>, msg: &str) {
    tx.send(AppMessage::Log(msg.to_owned())).ok();
}
