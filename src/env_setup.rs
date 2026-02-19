// src/env_setup.rs
//
// Homebrew discovery and build environment construction.

use std::collections::{HashMap, HashSet};

// ─── Homebrew discovery ───────────────────────────────────────────────────────

/// Return the path to the `brew` binary, checking Apple Silicon first.
#[must_use]
pub fn find_brew() -> Option<String> {
    const CANDIDATES: [&str; 2] = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"];
    CANDIDATES
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).is_file())
        .map(str::to_owned)
}

/// Derive the Homebrew prefix from the brew binary path.
#[must_use]
pub fn brew_prefix(brew: &str) -> String {
    if brew.contains("/opt/homebrew") {
        "/opt/homebrew".to_owned()
    } else {
        "/usr/local".to_owned()
    }
}

// ─── Build environment ────────────────────────────────────────────────────────

/// Build a complete process environment suitable for spawning compilation
/// children.  Prepends Homebrew, Cargo, and LLVM paths to `PATH`, sets
/// `LIBCLANG_PATH` / `DYLD_LIBRARY_PATH` for RocksDB bindgen, and inherits
/// everything else from the parent process.
#[must_use]
pub fn setup_build_environment(brew_pfx: Option<&str>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();

    let home = env
        .get("HOME")
        .map(|s| s.as_str())
        .unwrap_or("/Users/user")
        .to_owned();

    // ── Build ordered PATH components ────────────────────────────────────────
    // Capacity estimate: prefix bin + 2 homebrew locations + cargo + llvm +
    // existing PATH split + 4 system dirs.
    let mut path_parts: Vec<&str> = Vec::with_capacity(16);

    // Declare owned strings that need to live long enough.
    let pfx_bin;
    let cargo_bin;
    let llvm_bin_owned;

    if let Some(pfx) = brew_pfx {
        pfx_bin = format!("{pfx}/bin");
        path_parts.push(&pfx_bin);
    }
    path_parts.push("/opt/homebrew/bin");
    path_parts.push("/usr/local/bin");

    cargo_bin = format!("{home}/.cargo/bin");
    if std::path::Path::new(&cargo_bin).is_dir() {
        path_parts.push(&cargo_bin);
    }

    // LLVM: find first present candidate.
    let llvm_candidates = build_llvm_candidates(brew_pfx);
    let mut llvm_prefix_found: Option<&str> = None;
    let mut llvm_bin_buf = String::new();

    for candidate in &llvm_candidates {
        llvm_bin_buf.clear();
        llvm_bin_buf.push_str(candidate);
        llvm_bin_buf.push_str("/bin");
        if std::path::Path::new(&llvm_bin_buf).is_dir() {
            // Keep the bin path we found; derive lib path from it later.
            llvm_bin_owned = llvm_bin_buf.clone();
            path_parts.push(&llvm_bin_owned);
            llvm_prefix_found = Some(candidate.as_str());
            break;
        }
    }

    // Existing PATH entries and system fallbacks.
    let existing_path_owned;
    if let Some(existing) = env.get("PATH") {
        existing_path_owned = existing.clone();
        // existing PATH may contain many ':'-separated entries; push them
        // individually so dedup can eliminate duplicates.
        for part in existing_path_owned.split(':') {
            path_parts.push(part);
        }
    }
    path_parts.extend_from_slice(&["/usr/bin", "/bin", "/usr/sbin", "/sbin"]);

    // Deduplicate while preserving first-occurrence order.
    // Use HashSet<&str> — no allocation per entry.
    let mut seen: HashSet<&str> = HashSet::with_capacity(path_parts.len());
    let deduped: Vec<&str> = path_parts
        .into_iter()
        .filter(|p| !p.is_empty() && seen.insert(p))
        .collect();

    env.insert("PATH".to_owned(), deduped.join(":"));

    // ── LLVM library paths ────────────────────────────────────────────────────
    if let Some(pfx) = llvm_prefix_found {
        let lib = format!("{pfx}/lib");
        env.insert("LIBCLANG_PATH".to_owned(), lib.clone());
        env.insert("DYLD_LIBRARY_PATH".to_owned(), lib);
    }

    env
}

// ─── LLVM prefix candidates ───────────────────────────────────────────────────

fn build_llvm_candidates(brew_pfx: Option<&str>) -> Vec<String> {
    let mut v = Vec::with_capacity(3);
    if let Some(pfx) = brew_pfx {
        v.push(format!("{pfx}/opt/llvm"));
    }
    v.push("/opt/homebrew/opt/llvm".to_owned());
    v.push("/usr/local/opt/llvm".to_owned());
    v
}

// ─── macOS version ────────────────────────────────────────────────────────────

/// Return the macOS product version string, e.g. `"14.4.1"`.
/// Falls back to `"unknown"` when `sw_vers` is unavailable.
#[must_use]
pub fn macos_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}
