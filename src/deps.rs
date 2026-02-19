// src/deps.rs
//
// Background task: check and optionally install all build dependencies.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use anyhow::Result;
use tokio::sync::oneshot;

use crate::messages::{log_msg, AppMessage, ConfirmRequest};
use crate::process::{probe, run_command};

// Homebrew packages required for Bitcoin Core (autotools + cmake) and Electrs.
const BREW_PACKAGES: &[&str] = &[
    "automake", "libtool", "pkg-config", "boost",
    "miniupnpc", "zeromq", "sqlite", "python", "cmake",
    "llvm", "libevent", "rocksdb", "rust", "git",
];

// ─── Public entry point ───────────────────────────────────────────────────────

/// Background task: check and (optionally) install all dependencies.
///
/// Returns `true` when everything — including the Rust toolchain — is ready.
pub async fn check_dependencies_task(
    brew: String,
    env: HashMap<String, String>,
    log_tx: Sender<AppMessage>,
    confirm_tx: Sender<ConfirmRequest>,
) -> Result<bool> {
    log_msg(&log_tx, "\n=== Checking System Dependencies ===\n");
    log_msg(&log_tx, &format!("✓ Homebrew found at: {brew}\n"));

    // ── Check Homebrew packages ───────────────────────────────────────────────
    log_msg(&log_tx, "\nChecking Homebrew packages...\n");

    let mut missing: Vec<&str> = Vec::new();
    for &pkg in BREW_PACKAGES {
        // Use tokio::process::Command to avoid blocking a thread pool thread.
        let ok = tokio::process::Command::new(&brew)
            .args(["list", pkg])
            .env_clear()
            .envs(&env)
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if ok {
            log_msg(&log_tx, &format!("  ✓ {pkg}\n"));
        } else {
            log_msg(&log_tx, &format!("  ❌ {pkg} - not installed\n"));
            missing.push(pkg);
        }
    }

    // ── Offer to install missing packages ─────────────────────────────────────
    if !missing.is_empty() {
        log_msg(
            &log_tx,
            &format!(
                "\n⚠️  Missing Homebrew packages: {}\n",
                missing.join(", ")
            ),
        );

        let count = missing.len();
        let preview = missing
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let extra = if count > 5 {
            format!(", and {} more", count - 5)
        } else {
            String::new()
        };

        let message = format!(
            "Found {count} missing package{}:\n\n{preview}{extra}\n\nInstall all missing packages now?",
            if count == 1 { "" } else { "s" }
        );

        let should_install =
            ask_confirm(&confirm_tx, "Install Missing Dependencies", &message).await;

        if should_install {
            for pkg in &missing {
                log_msg(&log_tx, &format!("\n📦 Installing {pkg}...\n"));
                // Pass brew path and pkg as separate shell words; neither
                // should contain spaces but quoting makes it explicit.
                let cmd = format!("{brew:?} install {pkg}");
                match run_command(&cmd, None, &env, &log_tx).await {
                    Ok(()) => log_msg(&log_tx, &format!("✓ {pkg} installed successfully\n")),
                    Err(e) => {
                        log_msg(&log_tx, &format!("❌ Failed to install {pkg}: {e}\n"));
                        log_tx
                            .send(AppMessage::ShowDialog {
                                title:    "Installation Failed".into(),
                                message:  format!("Failed to install {pkg}:\n{e}"),
                                is_error: true,
                            })
                            .ok();
                    }
                }
            }
        } else {
            log_msg(
                &log_tx,
                "\n⚠️  Dependencies not installed. Compilation may fail.\n",
            );
        }
    } else {
        log_msg(&log_tx, "\n✓ All Homebrew packages are installed!\n");
    }

    // ── Check Rust toolchain ──────────────────────────────────────────────────
    let rust_ok = check_rust_installation(&brew, &env, &log_tx).await;

    log_msg(&log_tx, "\n=== Dependency Check Complete ===\n");

    if rust_ok {
        log_msg(&log_tx, "\n✓ Rust toolchain is ready!\n");
        log_tx
            .send(AppMessage::ShowDialog {
                title:    "Dependency Check".into(),
                message:  "✅ All dependencies are installed and ready!\n\nYou can now proceed with compilation.".into(),
                is_error: false,
            })
            .ok();
    } else {
        log_msg(
            &log_tx,
            "\n⚠️  Rust toolchain needs attention (see messages above)\n",
        );
        log_tx
            .send(AppMessage::ShowDialog {
                title:    "Dependency Check".into(),
                message:  "⚠️  Some dependencies need attention.\n\nCheck the log for details.\nYou may need to restart the app after installing Rust.".into(),
                is_error: false,
            })
            .ok();
    }

    Ok(rust_ok)
}

// ─── Rust toolchain check ─────────────────────────────────────────────────────

async fn check_rust_installation(
    brew: &str,
    env: &HashMap<String, String>,
    log_tx: &Sender<AppMessage>,
) -> bool {
    log_msg(log_tx, "\n=== Checking Rust Toolchain ===\n");

    let rustc_ok = if let Some(v) = probe(&["rustc", "--version"], env).await {
        log_msg(log_tx, &format!("✓ rustc found: {v}\n"));
        true
    } else {
        log_msg(log_tx, "❌ rustc not found in PATH\n");
        false
    };

    let cargo_ok = if let Some(v) = probe(&["cargo", "--version"], env).await {
        log_msg(log_tx, &format!("✓ cargo found: {v}\n"));
        true
    } else {
        log_msg(log_tx, "❌ cargo not found in PATH\n");
        false
    };

    if rustc_ok && cargo_ok {
        return true;
    }

    // ── Try installing via Homebrew ───────────────────────────────────────────
    log_msg(log_tx, "\n❌ Rust toolchain not found or incomplete!\n");
    log_msg(log_tx, "Installing Rust via Homebrew...\n");

    // Non-blocking check that brew knows the rust formula.
    let brew_knows_rust = tokio::process::Command::new(brew)
        .args(["info", "rust"])
        .env_clear()
        .envs(env)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !brew_knows_rust {
        log_msg(log_tx, "❌ Rust formula not found in Homebrew\n");
        log_tx
            .send(AppMessage::ShowDialog {
                title:    "Rust Installation Failed".into(),
                message:  "Could not install Rust via Homebrew.\n\nPlease install manually:\n1. Visit https://rustup.rs\n2. Run: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh\n3. Restart this app".into(),
                is_error: true,
            })
            .ok();
        return false;
    }

    log_msg(log_tx, "📦 Installing rust from Homebrew...\n");
    let brew_cmd = format!("{brew:?} install rust");
    match run_command(&brew_cmd, None, env, log_tx).await {
        Err(e) => {
            log_msg(log_tx, &format!("❌ Failed to install Rust: {e}\n"));
            log_tx
                .send(AppMessage::ShowDialog {
                    title:    "Installation Error".into(),
                    message:  format!("Failed to install Rust: {e}\n\nPlease install manually from https://rustup.rs"),
                    is_error: true,
                })
                .ok();
            return false;
        }
        Ok(()) => {
            log_msg(log_tx, "\nVerifying Rust installation...\n");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    // Re-check after installation.
    match (
        probe(&["rustc", "--version"], env).await,
        probe(&["cargo", "--version"], env).await,
    ) {
        (Some(r), Some(c)) => {
            log_msg(log_tx, &format!("✓ rustc installed: {r}\n"));
            log_msg(log_tx, &format!("✓ cargo installed: {c}\n"));
            true
        }
        _ => {
            log_msg(
                log_tx,
                "⚠️  Rust installed but binaries not yet in PATH. Restart the app.\n",
            );
            log_tx
                .send(AppMessage::ShowDialog {
                    title:    "Rust Installation".into(),
                    message:  "Rust was installed but may not be in PATH.\n\nPlease:\n1. Close and reopen this app\n2. OR manually add ~/.cargo/bin to your PATH".into(),
                    is_error: false,
                })
                .ok();
            false
        }
    }
}

// ─── Confirmation helper ──────────────────────────────────────────────────────

/// Send a `ConfirmRequest` to the UI, then suspend until the UI replies.
async fn ask_confirm(
    tx: &Sender<ConfirmRequest>,
    title: &str,
    message: &str,
) -> bool {
    let (response_tx, response_rx) = oneshot::channel::<bool>();
    tx.send(ConfirmRequest {
        title:       title.to_owned(),
        message:     message.to_owned(),
        response_tx,
    })
    .ok();
    response_rx.await.unwrap_or(false)
}
