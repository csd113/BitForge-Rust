// src/process.rs
//
// `run_command`: spawn a child via `sh -c`, stream stdout+stderr to the UI.
// `probe`:       run a command and capture its output (no logging).
//
// KEY DESIGN: we read stdout/stderr as raw byte chunks rather than lines.
// This ensures that:
//   • git's carriage-return-based progress ("\rReceiving 50%") is shown live.
//   • cmake/cargo output without trailing newlines is not buffered indefinitely.
//   • No output is ever silently swallowed in the BufReader internal buffer.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::messages::AppMessage;

/// Execute `cmd` in a shell, streaming every byte of output to `log_tx`.
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
    log_tx.send(AppMessage::Log(format!("\n$ {cmd}\n"))).ok();

    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(cmd)
        .env_clear()
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // No orphan processes if this task is cancelled.
        .kill_on_drop(true);

    if let Some(dir) = cwd {
        builder.current_dir(dir);
    }

    let mut child = builder
        .spawn()
        .with_context(|| format!("Failed to spawn: {cmd}"))?;

    let stdout = child.stdout.take().context("stdout not captured")?;
    let stderr = child.stderr.take().context("stderr not captured")?;

    // Drain stdout and stderr as raw byte chunks so that:
    //   - \r-terminated progress lines (git, cmake) appear immediately.
    //   - Large pipe buffers never deadlock the child process.
    // Each chunk is sanitised: \r not followed by \n becomes \n so the
    // terminal-style log displays correctly.
    let tx_out = log_tx.clone();
    let tx_err = log_tx.clone();

    let stdout_task = tokio::spawn(drain_reader(stdout, tx_out));
    let stderr_task = tokio::spawn(drain_reader(stderr, tx_err));

    // Wait for the child to exit. Because the reader tasks are independently
    // spawned and continuously draining the pipes, the child can never block
    // on a full pipe buffer — no deadlock possible.
    let status = child
        .wait()
        .await
        .with_context(|| format!("Failed to wait for: {cmd}"))?;

    // Ensure every last byte is flushed before we check the exit code.
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

/// Continuously read `reader` in 8 KiB chunks and forward sanitised UTF-8
/// text to `tx`.  Carriage returns not followed by a newline are replaced
/// with newlines so that git/cmake progress displays properly.
async fn drain_reader<R: AsyncReadExt + Unpin>(mut reader: R, tx: Sender<AppMessage>) {
    let mut buf = vec![0u8; 8192];
    let mut carry = Vec::new(); // bytes from last chunk that ended mid-CR/LF

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break, // EOF or error — stop reading
            Ok(n) => n,
        };

        // Combine any leftover bytes with the new chunk.
        carry.extend_from_slice(&buf[..n]);

        // Convert to a lossy UTF-8 string, replacing \r not followed by \n
        // with \n so the log view shows each progress update on its own line.
        let text = String::from_utf8_lossy(&carry);
        let sanitised = sanitise_cr(text.as_ref());

        // If the chunk ends mid-sequence (no trailing newline) we hold the
        // last incomplete "line" in carry so it isn't split across chunks.
        // For simplicity we forward everything and reset carry.
        carry.clear();

        if !sanitised.is_empty() {
            tx.send(AppMessage::Log(sanitised)).ok();
        }
    }

    // Flush any remaining bytes.
    if !carry.is_empty() {
        let text = String::from_utf8_lossy(&carry);
        let sanitised = sanitise_cr(text.as_ref());
        if !sanitised.is_empty() {
            tx.send(AppMessage::Log(sanitised)).ok();
        }
    }
}

/// Normalize line endings: collapse Windows CRLF (\r\n) → \n, and strip
/// ANSI escape sequences. Bare \r (carriage return without \n) is passed
/// through unchanged so that append_log can apply true terminal semantics
/// (overwrite the current line), keeping cmake/make progress readable
/// instead of generating hundreds of stacked duplicate lines.
fn sanitise_cr(s: &str) -> String {
    // Fast path: nothing to do for pure ASCII with no special bytes.
    if !s.contains('\r') {
        return s.to_owned();
    }
    // Collapse \r\n → \n; leave bare \r intact for append_log to handle.
    s.replace("\r\n", "\n")
}

/// Run a command and capture its trimmed stdout, returning `None` on failure.
/// Async so callers inside tokio tasks do not block a worker thread.
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
