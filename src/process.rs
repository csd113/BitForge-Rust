// src/process.rs
//
// `run_command`: spawn a child via `sh -c`, stream stdout+stderr to the UI.
// `probe`:       run a command and capture its output (no UI logging).

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::messages::AppMessage;

/// Execute `cmd` in a shell, streaming every output line to `log_tx`.
///
/// * `cwd` – optional working directory for the child process.
/// * `env` – complete environment (replaces the child's inherited env).
///
/// Returns `Ok(())` on exit code 0; `Err` on non-zero exit or spawn failure.
pub async fn run_command(
    cmd: &str,
    cwd: Option<&Path>,
    env: &HashMap<String, String>,
    log_tx: &Sender<AppMessage>,
) -> Result<()> {
    log_tx
        .send(AppMessage::Log(format!("\n$ {cmd}\n")))
        .ok();

    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(cmd)
        .env_clear()
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Ensures no zombie processes if this task is cancelled.
        .kill_on_drop(true);

    if let Some(dir) = cwd {
        builder.current_dir(dir);
    }

    let mut child = builder
        .spawn()
        .with_context(|| format!("Failed to spawn: {cmd}"))?;

    let stdout = child.stdout.take().context("stdout not captured")?;
    let stderr = child.stderr.take().context("stderr not captured")?;

    // Drain stdout and stderr concurrently to avoid OS pipe-buffer deadlocks.
    let tx_out = log_tx.clone();
    let tx_err = log_tx.clone();

    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tx_out.send(AppMessage::Log(format!("{line}\n"))).ok();
        }
    });

    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tx_err.send(AppMessage::Log(format!("{line}\n"))).ok();
        }
    });

    // Wait for the child to exit (closes its pipe ends → EOF in reader tasks).
    let status = child
        .wait()
        .await
        .with_context(|| format!("Failed to wait for: {cmd}"))?;

    // Drain remaining buffered output.
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_owned());
        bail!("Command failed (exit {code}): {cmd}");
    }

    Ok(())
}

/// Run a command and capture its trimmed stdout, returning `None` on failure.
///
/// Uses `tokio::process::Command` so callers inside async tasks do not block
/// the tokio thread pool.  Returns `None` on empty `cmd`.
pub async fn probe(cmd: &[&str], env: &HashMap<String, String>) -> Option<String> {
    let (prog, args) = cmd.split_first()?;

    let output = Command::new(prog)
        .args(args)
        .env_clear()
        .envs(env)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
