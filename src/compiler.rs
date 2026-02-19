// src/compiler.rs
//
// `compile_bitcoin` and `compile_electrs`: async functions that clone/update
// the source repository and drive the appropriate build tool, streaming all
// output to the UI log in real time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;

use crate::messages::{log_msg, AppMessage};
use crate::process::{probe, run_command};

const BITCOIN_REPO: &str = "https://github.com/bitcoin/bitcoin.git";
const ELECTRS_REPO: &str = "https://github.com/romanz/electrs.git";

const SEP: &str = "============================================================";

// ─── Static regex — compiled once at first use ────────────────────────────────

static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+)\.(\d+)").expect("VERSION_RE is a valid static pattern")
});

// ─── Public compile functions ─────────────────────────────────────────────────

/// Compile Bitcoin Core from source.  Returns the output binaries directory.
pub async fn compile_bitcoin(
    version: &str,
    build_dir: &Path,
    cores: usize,
    env: &HashMap<String, String>,
    tx: &Sender<AppMessage>,
) -> Result<PathBuf> {
    log_msg(tx, &format!("\n{SEP}\nCOMPILING BITCOIN CORE {version}\n{SEP}\n"));

    let version_clean = version.trim_start_matches('v');
    let src_dir = build_dir.join(format!("bitcoin-{version_clean}"));

    std::fs::create_dir_all(build_dir).context("Failed to create build directory")?;

    clone_or_update(&src_dir, build_dir, version, BITCOIN_REPO, tx, env).await?;

    if let Some(path_val) = env.get("PATH") {
        let preview = truncate_str(path_val, 150);
        log_msg(
            tx,
            &format!(
                "\nEnvironment setup:\n  PATH: {preview}...\n  Building node-only (wallet support disabled)\n"
            ),
        );
    }

    tx.send(AppMessage::Progress(0.3)).ok();

    let binaries = if use_cmake(version) {
        build_bitcoin_cmake(&src_dir, cores, env, tx).await?
    } else {
        build_bitcoin_autotools(&src_dir, cores, env, tx).await?
    };

    tx.send(AppMessage::Progress(0.9)).ok();

    let output_dir = build_dir
        .join("binaries")
        .join(format!("bitcoin-{version_clean}"));
    let copied = copy_binaries(&output_dir, &binaries, tx)?;

    if copied.is_empty() {
        log_msg(tx, "⚠️  Warning: No binaries were copied. Checking what exists...\n");
        for binary in &binaries {
            let mark = if binary.exists() { "✓" } else { "❌" };
            log_msg(tx, &format!("  {mark} {}\n", binary.display()));
        }
    }

    log_msg(
        tx,
        &format!(
            "\n{SEP}\n✅ BITCOIN CORE {version} COMPILED SUCCESSFULLY!\n{SEP}\n\n\
             📍 Binaries location: {}\n   Found {} binaries\n\n",
            output_dir.display(),
            copied.len()
        ),
    );

    Ok(output_dir)
}

/// Compile Electrs from source.  Returns the output binaries directory.
pub async fn compile_electrs(
    version: &str,
    build_dir: &Path,
    cores: usize,
    env: &HashMap<String, String>,
    tx: &Sender<AppMessage>,
) -> Result<PathBuf> {
    log_msg(tx, &format!("\n{SEP}\nCOMPILING ELECTRS {version}\n{SEP}\n"));

    log_msg(tx, "\n🔍 Verifying Rust installation...\n");
    match probe(&["cargo", "--version"], env).await {
        Some(v) => log_msg(tx, &format!("✓ Cargo found: {v}\n")),
        None => {
            let msg = "❌ Cargo not found in PATH!\n\nElectrs requires Rust/Cargo to compile.\n\nPlease:\n1. Click 'Check & Install Dependencies' button\n2. Ensure Rust is installed\n3. Restart this application";
            log_msg(tx, msg);
            tx.send(AppMessage::ShowDialog {
                title:    "Rust Not Found".into(),
                message:  msg.into(),
                is_error: true,
            })
            .ok();
            return Err(anyhow::anyhow!("Cargo not found — cannot compile Electrs"));
        }
    }

    if let Some(v) = probe(&["rustc", "--version"], env).await {
        log_msg(tx, &format!("✓ Rustc found: {v}\n"));
    } else {
        log_msg(tx, "⚠️  Warning: rustc check failed, but cargo found. Proceeding...\n");
    }

    let version_clean = version.trim_start_matches('v');
    let src_dir = build_dir.join(format!("electrs-{version_clean}"));

    std::fs::create_dir_all(build_dir).context("Failed to create build directory")?;

    clone_or_update(&src_dir, build_dir, version, ELECTRS_REPO, tx, env).await?;

    log_msg(tx, &format!("\n🔧 Building with Cargo ({cores} jobs)...\n"));

    if let Some(path_val) = env.get("PATH") {
        log_msg(
            tx,
            &format!("Environment details:\n  PATH: {}...\n", truncate_str(path_val, 150)),
        );
    }
    if let Some(lcp) = env.get("LIBCLANG_PATH") {
        log_msg(tx, &format!("  LIBCLANG_PATH: {lcp}\n"));
    }

    tx.send(AppMessage::Progress(0.3)).ok();

    run_command(
        &format!("cargo build --release --jobs {cores}"),
        Some(&src_dir),
        env,
        tx,
    )
    .await
    .context("cargo build --release failed")?;

    tx.send(AppMessage::Progress(0.85)).ok();

    log_msg(tx, "\n📋 Collecting binaries...\n");
    let binary = src_dir.join("target/release/electrs");
    if !binary.exists() {
        return Err(anyhow::anyhow!(
            "Electrs binary not found at expected location: {}",
            binary.display()
        ));
    }

    let output_dir = build_dir
        .join("binaries")
        .join(format!("electrs-{version_clean}"));
    copy_binaries(&output_dir, &[binary], tx)?;

    log_msg(
        tx,
        &format!(
            "\n{SEP}\n✅ ELECTRS {version} COMPILED SUCCESSFULLY!\n{SEP}\n\n\
             📍 Binary location: {}/electrs\n\n",
            output_dir.display()
        ),
    );

    Ok(output_dir)
}

// ─── CMake build (Bitcoin Core v25+) ─────────────────────────────────────────

async fn build_bitcoin_cmake(
    src_dir: &Path,
    cores: usize,
    env: &HashMap<String, String>,
    tx: &Sender<AppMessage>,
) -> Result<Vec<PathBuf>> {
    log_msg(tx, "\n🔨 Building with CMake...\n");
    log_msg(tx, "\n⚙️  Configuring (wallet support disabled)...\n");

    run_command(
        "cmake -B build -DENABLE_WALLET=OFF -DENABLE_IPC=OFF",
        Some(src_dir),
        env,
        tx,
    )
    .await
    .context("cmake configure failed")?;

    tx.send(AppMessage::Progress(0.5)).ok();
    log_msg(tx, &format!("\n🔧 Compiling with {cores} cores...\n"));

    run_command(
        &format!("cmake --build build -j{cores}"),
        Some(src_dir),
        env,
        tx,
    )
    .await
    .context("cmake build failed")?;

    let bin_dir = src_dir.join("build/bin");
    Ok(vec![
        bin_dir.join("bitcoind"),
        bin_dir.join("bitcoin-cli"),
        bin_dir.join("bitcoin-tx"),
        bin_dir.join("bitcoin-wallet"),
        bin_dir.join("bitcoin-util"),
    ])
}

// ─── Autotools build (Bitcoin Core < v25) ────────────────────────────────────

async fn build_bitcoin_autotools(
    src_dir: &Path,
    cores: usize,
    env: &HashMap<String, String>,
    tx: &Sender<AppMessage>,
) -> Result<Vec<PathBuf>> {
    log_msg(tx, "\n🔨 Building with Autotools...\n");
    log_msg(tx, "\n⚙️  Running autogen.sh...\n");

    run_command("./autogen.sh", Some(src_dir), env, tx)
        .await
        .context("autogen.sh failed")?;

    log_msg(tx, "\n⚙️  Configuring (wallet support disabled)...\n");
    run_command(
        "./configure --disable-wallet --disable-gui",
        Some(src_dir),
        env,
        tx,
    )
    .await
    .context("./configure failed")?;

    tx.send(AppMessage::Progress(0.5)).ok();
    log_msg(tx, &format!("\n🔧 Compiling with {cores} cores...\n"));

    run_command(&format!("make -j{cores}"), Some(src_dir), env, tx)
        .await
        .context("make failed")?;

    let bin_dir = src_dir.join("bin");
    Ok(vec![
        bin_dir.join("bitcoind"),
        bin_dir.join("bitcoin-cli"),
        bin_dir.join("bitcoin-tx"),
        bin_dir.join("bitcoin-wallet"),
    ])
}

// ─── Binary copy ─────────────────────────────────────────────────────────────

fn copy_binaries(
    dest_dir: &Path,
    binary_files: &[PathBuf],
    tx: &Sender<AppMessage>,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_dir).context("Failed to create output directory")?;
    log_msg(tx, &format!("Copying binaries to: {}\n", dest_dir.display()));

    let mut copied = Vec::new();
    for binary in binary_files {
        if !binary.exists() {
            log_msg(
                tx,
                &format!("⚠️  Binary not found (skipping): {}\n", binary.display()),
            );
            continue;
        }

        // Guard: a path like `/` has no file name.
        let name = match binary.file_name() {
            Some(n) => n,
            None => {
                log_msg(
                    tx,
                    &format!("⚠️  Skipping path with no file name: {}\n", binary.display()),
                );
                continue;
            }
        };

        let dest = dest_dir.join(name);
        match std::fs::copy(binary, &dest) {
            Ok(_) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &dest,
                        std::fs::Permissions::from_mode(0o755),
                    );
                }
                log_msg(
                    tx,
                    &format!(
                        "✓ Copied: {} → {}\n",
                        name.to_string_lossy(),
                        dest.display()
                    ),
                );
                copied.push(dest);
            }
            Err(e) => {
                log_msg(
                    tx,
                    &format!("⚠️  Failed to copy {}: {e}\n", name.to_string_lossy()),
                );
            }
        }
    }

    if copied.is_empty() {
        log_msg(tx, "❌ WARNING: No binaries were copied!\n");
    }

    Ok(copied)
}

// ─── Version helpers ──────────────────────────────────────────────────────────

/// Parse a version tag into `(major, minor)`.  Strips any leading `v`.
///
/// Uses a process-global compiled regex — no per-call allocation.
pub fn parse_version(tag: &str) -> (u32, u32) {
    let tag = tag.trim_start_matches('v');
    VERSION_RE
        .captures(tag)
        .and_then(|c: regex::Captures<'_>| {
            let major: u32 = c.get(1)?.as_str().parse().ok()?;
            let minor: u32 = c.get(2)?.as_str().parse().ok()?;
            Some((major, minor))
        })
        .unwrap_or((0, 0))
}

/// `true` when Bitcoin Core v25+ (uses CMake); older versions use Autotools.
pub fn use_cmake(version: &str) -> bool {
    let (major, _) = parse_version(version);
    major >= 25
}

// ─── Clone / update helper ────────────────────────────────────────────────────

/// Clone the repo at `version` into `src_dir`, or fetch+checkout if it exists.
///
/// Uses `tokio::process::Command` directly for git operations to avoid shell
/// injection: `version` comes from the GitHub API and `src_dir` from user input.
async fn clone_or_update(
    src_dir: &Path,
    build_dir: &Path,
    version: &str,
    repo_url: &str,
    tx: &Sender<AppMessage>,
    env: &HashMap<String, String>,
) -> Result<()> {
    if !src_dir.exists() {
        log_msg(tx, &format!("\n📥 Cloning repository from {repo_url}...\n"));

        // Use run_command with the shell for consistency with the rest of the
        // build pipeline; version tags from GitHub are expected to match
        // [v][0-9]+\.[0-9]+.* — validate before interpolating.
        validate_version_tag(version)?;

        run_command(
            &format!(
                "git clone --depth 1 --branch {} {} {}",
                shell_quote(version),
                shell_quote(repo_url),
                shell_quote(&src_dir.to_string_lossy()),
            ),
            Some(build_dir),
            env,
            tx,
        )
        .await
        .context("git clone failed")?;

        log_msg(tx, &format!("✓ Source cloned to {}\n", src_dir.display()));
    } else {
        log_msg(
            tx,
            &format!("✓ Source directory exists: {}\n", src_dir.display()),
        );
        log_msg(tx, &format!("📥 Updating to {version}...\n"));

        validate_version_tag(version)?;

        run_command(
            &format!("git fetch --depth 1 origin tag {}", shell_quote(version)),
            Some(src_dir),
            env,
            tx,
        )
        .await
        .context("git fetch failed")?;

        run_command(
            &format!("git checkout {}", shell_quote(version)),
            Some(src_dir),
            env,
            tx,
        )
        .await
        .context("git checkout failed")?;

        log_msg(tx, &format!("✓ Updated to {version}\n"));
    }
    Ok(())
}

// ─── Utilities ────────────────────────────────────────────────────────────────

/// Validate that a version tag contains only safe characters.
/// GitHub tags for Bitcoin/Electrs follow `v\d+\.\d+[.\d]*(-rc\d+)?`.
fn validate_version_tag(tag: &str) -> Result<()> {
    if tag.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_')) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Version tag contains unexpected characters: {tag:?}"
        ))
    }
}

/// Wrap a string in single quotes for POSIX sh, escaping any `'` inside.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Return the first `max_chars` characters of `s`, never splitting a codepoint.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_pos, _)) => &s[..byte_pos],
        None => s,
    }
}
