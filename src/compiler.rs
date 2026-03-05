// src/compiler.rs
//
// compile_bitcoin  — clone, cmake configure, cmake build, copy binaries.
// compile_electrs  — clone, cargo build --release, copy binary.
//
// Bitcoin Core v29+ uses CMake exclusively (autotools removed upstream).
// The critical env requirement: PKG_CONFIG_PATH must point at Homebrew's
// pkgconfig directories so cmake can find libevent, sqlite, etc. via
// pkg-config. Without this, cmake falls back to exhaustive try_compile
// probes for every dependency, stalling with zero output for 10+ minutes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};

use crate::messages::{log_msg, AppMessage};
use crate::process::{probe, run_command};

const BITCOIN_REPO: &str = "https://github.com/bitcoin/bitcoin.git";
const ELECTRS_REPO: &str = "https://github.com/romanz/electrs.git";
const SEP: &str = "============================================================";

// ─── Public compile functions ─────────────────────────────────────────────────

pub async fn compile_bitcoin(
    version: &str,
    build_dir: &Path,
    cores: usize,
    env: &HashMap<String, String>,
    tx: &Sender<AppMessage>,
) -> Result<PathBuf> {
    log_msg(
        tx,
        &format!("\n{SEP}\nCOMPILING BITCOIN CORE {version}\n{SEP}\n"),
    );

    let version_clean = version.trim_start_matches('v');
    let src_dir = build_dir.join(format!("bitcoin-{version_clean}"));

    tokio::fs::create_dir_all(build_dir)
        .await
        .context("Failed to create build directory")?;

    // Build a bitcoin-specific environment. The two critical additions
    // over the base env are:
    //   PKG_CONFIG_PATH — lets cmake find Homebrew packages via pkg-config
    //                     instantly, rather than doing exhaustive try_compile
    //                     probes that stall the build for 10+ minutes.
    //   TERM unset      — cmake streams configure output in real time only
    //                     when TERM is not "dumb"; removing it lets cmake
    //                     auto-detect and use its normal output mode.
    let env = bitcoin_env(env);

    // ── Step 1: clone ─────────────────────────────────────────────────────────
    clone_or_update(&src_dir, build_dir, version, BITCOIN_REPO, tx, &env).await?;

    // ── Step 2: cmake configure ───────────────────────────────────────────────
    //
    // Flags used (matching the official build-osx.md for v29+):
    //   -DENABLE_WALLET=OFF   skip wallet (no Berkeley DB / SQLite needed)
    //   -DENABLE_IPC=OFF      skip IPC (no capnp needed)
    //   -DBUILD_TESTS=OFF     skip test suite compilation
    //   -DBUILD_BENCH=OFF     skip benchmarks
    //   -DBUILD_GUI=OFF       skip Qt GUI
    //   -DWITH_MINIUPNPC=OFF  skip optional UPnP dep
    //   -DWITH_NATPMP=OFF     skip optional NAT-PMP dep
    //   -DWITH_ZMQ=OFF        skip optional ZMQ dep
    //
    // With wallet/IPC/tests/bench/GUI/optional-deps all disabled, the only
    // required non-system dependency is libevent, which pkg-config finds
    // instantly once PKG_CONFIG_PATH is set correctly.

    log_msg(
        tx,
        "\n── Step 1/3: CMake configure ────────────────────────────────\n",
    );
    log_msg(
        tx,
        &format!(
            "PKG_CONFIG_PATH = {}\n\n",
            env.get("PKG_CONFIG_PATH")
                .map(|s| s.as_str())
                .unwrap_or("(not set)")
        ),
    );

    tx.send(AppMessage::Progress(0.2)).ok();

    run_command(
        "cmake -B build \
            -DENABLE_WALLET=OFF \
            -DENABLE_IPC=OFF \
            -DBUILD_TESTS=OFF \
            -DBUILD_BENCH=OFF \
            -DBUILD_GUI=OFF \
            -DWITH_MINIUPNPC=OFF \
            -DWITH_NATPMP=OFF \
            -DWITH_ZMQ=OFF",
        Some(&src_dir),
        &env,
        tx,
    )
    .await
    .context(
        "cmake configure failed.\n\
         Common causes:\n\
         - libevent not installed: brew install libevent\n\
         - cmake not installed:    brew install cmake\n\
         - Xcode CLI tools missing: xcode-select --install",
    )?;

    // ── Step 3: cmake build ───────────────────────────────────────────────────
    log_msg(
        tx,
        &format!("\n── Step 2/3: Build ({cores} cores) ──────────────────────────────\n\n"),
    );
    tx.send(AppMessage::Progress(0.45)).ok();

    // No --target flag: with BUILD_TESTS/BENCH/GUI/WALLET all OFF at configure
    // time, cmake builds only the node binaries (bitcoind, bitcoin-cli, etc.).
    // Listing targets explicitly breaks across versions — bitcoin-tx was
    // removed in v29 and the set may change further.
    run_command(
        &format!("cmake --build build -j {cores}"),
        Some(&src_dir),
        &env,
        tx,
    )
    .await
    .context("cmake build failed")?;

    tx.send(AppMessage::Progress(0.9)).ok();

    // ── Step 4: copy binaries ─────────────────────────────────────────────────
    log_msg(
        tx,
        "\n── Step 3/3: Copying binaries ───────────────────────────────\n",
    );

    // Scan the bin dir for whatever executables were actually produced.
    // The exact set varies by version so we copy everything present.
    let bin_dir = src_dir.join("build").join("bin");
    let candidates = collect_executables(&bin_dir).await;

    let output_dir = build_dir
        .join("binaries")
        .join(format!("bitcoin-{version_clean}"));

    let copied = copy_binaries(&output_dir, &candidates, tx).await?;

    if copied.is_empty() {
        return Err(anyhow::anyhow!(
            "Build appeared to succeed but no binaries were found in {}\n\
             Check the log above for linker errors.",
            bin_dir.display()
        ));
    }

    log_msg(
        tx,
        &format!(
            "\n{SEP}\n✅ BITCOIN CORE {version} COMPILED SUCCESSFULLY!\n{SEP}\n\n\
         📍 Binaries copied to: {}\n\
         📦 {} binaries: {}\n\n",
            output_dir.display(),
            copied.len(),
            copied
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    );

    Ok(output_dir)
}

pub async fn compile_electrs(
    version: &str,
    build_dir: &Path,
    cores: usize,
    env: &HashMap<String, String>,
    tx: &Sender<AppMessage>,
) -> Result<PathBuf> {
    log_msg(
        tx,
        &format!("\n{SEP}\nCOMPILING ELECTRS {version}\n{SEP}\n"),
    );

    let env = cargo_env(env);

    log_msg(tx, "\n🔍 Verifying Rust installation...\n");
    match probe(&["cargo", "--version"], &env).await {
        Some(v) => log_msg(tx, &format!("✓ Cargo: {v}\n")),
        None => {
            let msg = "❌ Cargo not found in PATH.\n\nPlease click 'Check & Install Dependencies', ensure Rust is installed, then restart.";
            log_msg(tx, msg);
            tx.send(AppMessage::ShowDialog {
                title: "Rust Not Found".into(),
                message: msg.into(),
                is_error: true,
            })
            .ok();
            return Err(anyhow::anyhow!("Cargo not found — cannot compile Electrs"));
        }
    }

    if let Some(v) = probe(&["rustc", "--version"], &env).await {
        log_msg(tx, &format!("✓ Rustc: {v}\n"));
    }

    let version_clean = version.trim_start_matches('v');
    let src_dir = build_dir.join(format!("electrs-{version_clean}"));

    tokio::fs::create_dir_all(build_dir)
        .await
        .context("Failed to create build directory")?;

    clone_or_update(&src_dir, build_dir, version, ELECTRS_REPO, tx, &env).await?;

    log_msg(
        tx,
        &format!("\n🔧 Building Electrs with Cargo ({cores} jobs)...\n"),
    );
    if let Some(lcp) = env.get("LIBCLANG_PATH") {
        log_msg(tx, &format!("  LIBCLANG_PATH: {lcp}\n"));
    }

    tx.send(AppMessage::Progress(0.3)).ok();

    run_command(
        &format!("cargo build --release --jobs {cores}"),
        Some(&src_dir),
        &env,
        tx,
    )
    .await
    .context("cargo build --release failed")?;

    tx.send(AppMessage::Progress(0.85)).ok();

    let binary = src_dir.join("target/release/electrs");
    if !binary.exists() {
        return Err(anyhow::anyhow!(
            "Electrs binary not found at: {}",
            binary.display()
        ));
    }

    let output_dir = build_dir
        .join("binaries")
        .join(format!("electrs-{version_clean}"));
    copy_binaries(&output_dir, &[binary], tx).await?;

    log_msg(
        tx,
        &format!(
            "\n{SEP}\n✅ ELECTRS {version} COMPILED SUCCESSFULLY!\n{SEP}\n\n\
         📍 Binary: {}/electrs\n\n",
            output_dir.display()
        ),
    );

    Ok(output_dir)
}

// ─── Environment builders ─────────────────────────────────────────────────────

/// Environment for Bitcoin Core cmake builds.
///
/// Critical differences from cargo_env:
/// - PKG_CONFIG_PATH set → cmake finds Homebrew deps via pkg-config instantly.
/// - TERM NOT set to "dumb" → cmake streams output in real time, not batched.
fn bitcoin_env(base: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = base.clone();

    // ── PKG_CONFIG_PATH ──────────────────────────────────────────────────────
    // Bitcoin Core cmake finds libevent and other Homebrew deps via pkg-config.
    // Without these paths cmake runs silent try_compile probes for every lib,
    // stalling the configure step for 10+ minutes with no visible output.
    let homebrew_dirs = [
        "/opt/homebrew/lib/pkgconfig",
        "/opt/homebrew/share/pkgconfig",
        "/usr/local/lib/pkgconfig",
        "/usr/local/share/pkgconfig",
    ];

    let mut pcp: Vec<String> = homebrew_dirs.iter().map(|s| s.to_string()).collect();
    if let Some(existing) = env.get("PKG_CONFIG_PATH") {
        for part in existing.split(':').filter(|p| !p.is_empty()) {
            if !pcp.contains(&part.to_string()) {
                pcp.push(part.to_string());
            }
        }
    }
    env.insert("PKG_CONFIG_PATH".to_owned(), pcp.join(":"));

    // Suppress colours but do NOT set TERM=dumb (cmake buffers output when dumb).
    env.remove("TERM");
    env.insert("NO_COLOR".to_owned(), "1".to_owned());
    env.insert("CLICOLOR".to_owned(), "0".to_owned());
    env.insert("CLICOLOR_FORCE".to_owned(), "0".to_owned());
    env.insert("GIT_PROGRESS_DELAY".to_owned(), "0".to_owned());

    env
}

/// Environment for Cargo / Rust builds (Electrs).
fn cargo_env(base: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = base.clone();
    env.insert("NO_COLOR".to_owned(), "1".to_owned());
    env.insert("TERM".to_owned(), "dumb".to_owned());
    env.insert("CLICOLOR".to_owned(), "0".to_owned());
    env.insert("CLICOLOR_FORCE".to_owned(), "0".to_owned());
    env.insert("GIT_PROGRESS_DELAY".to_owned(), "0".to_owned());
    env.insert("CARGO_TERM_COLOR".to_owned(), "never".to_owned());
    env.insert("CARGO_TERM_PROGRESS_WHEN".to_owned(), "always".to_owned());
    env.insert("CARGO_TERM_PROGRESS_WIDTH".to_owned(), "60".to_owned());
    env
}

// ─── Collect executables from a directory ────────────────────────────────────

/// Read every file in `dir` and return those that are executable.
/// Returns an empty Vec if the directory doesn't exist or can't be read.
async fn collect_executables(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return result,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.is_file() {
            // On Unix, check the executable bit.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&path) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        result.push(path);
                    }
                }
            }
            #[cfg(not(unix))]
            result.push(path);
        }
    }
    result.sort(); // deterministic order
    result
}

// ─── Binary copy ──────────────────────────────────────────────────────────────

async fn copy_binaries(
    dest_dir: &Path,
    binary_files: &[PathBuf],
    tx: &Sender<AppMessage>,
) -> Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .context("Failed to create output directory")?;
    log_msg(
        tx,
        &format!("📋 Output directory: {}\n", dest_dir.display()),
    );

    let mut copied = Vec::new();
    for binary in binary_files {
        if !binary.exists() {
            log_msg(
                tx,
                &format!("  ⚠  Not found (skipping): {}\n", binary.display()),
            );
            continue;
        }

        let name = match binary.file_name() {
            Some(n) => n,
            None => continue,
        };

        let dest = dest_dir.join(name);
        match tokio::fs::copy(binary, &dest).await {
            Ok(_) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
                }
                log_msg(tx, &format!("  ✓ {}\n", name.to_string_lossy()));
                copied.push(dest);
            }
            Err(e) => {
                log_msg(
                    tx,
                    &format!("  ✗ Failed to copy {}: {e}\n", name.to_string_lossy()),
                );
            }
        }
    }

    Ok(copied)
}

// ─── Clone / update ───────────────────────────────────────────────────────────

/// Shallow-clone `version` into `src_dir`, or verify an existing clone matches.
///
/// If the directory exists at a different tag, remove and re-clone.
/// do NOT add --filter=blob:none: a blobless clone defers file downloads
/// to first access, causing cmake/cargo to stall silently fetching blobs.
async fn clone_or_update(
    src_dir: &Path,
    build_dir: &Path,
    version: &str,
    repo_url: &str,
    tx: &Sender<AppMessage>,
    env: &HashMap<String, String>,
) -> Result<()> {
    validate_version_tag(version)?;

    if src_dir.exists() {
        let current_tag = probe(
            &[
                "git",
                "-C",
                &src_dir.to_string_lossy(),
                "describe",
                "--tags",
                "--exact-match",
            ],
            env,
        )
        .await
        .unwrap_or_default();

        if current_tag == version {
            log_msg(
                tx,
                &format!("✓ Source already at {version}: {}\n", src_dir.display()),
            );
            return Ok(());
        }

        log_msg(
            tx,
            &format!("📥 Existing clone is at '{current_tag}', need '{version}'. Re-cloning...\n"),
        );
        tokio::fs::remove_dir_all(src_dir)
            .await
            .with_context(|| format!("Failed to remove {}", src_dir.display()))?;
    }

    log_msg(
        tx,
        &format!("\n📥 Cloning {} at {}...\n", repo_url, version),
    );
    log_msg(
        tx,
        "   (shallow clone — may take a few minutes for Bitcoin Core)\n\n",
    );

    run_command(
        &format!(
            "git clone --progress --depth 1 --branch {} {} {}",
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

    log_msg(tx, &format!("✓ Cloned to {}\n", src_dir.display()));
    Ok(())
}

// ─── Utilities ────────────────────────────────────────────────────────────────

fn validate_version_tag(tag: &str) -> Result<()> {
    if tag
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Version tag contains unexpected characters: {tag:?}"
        ))
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
