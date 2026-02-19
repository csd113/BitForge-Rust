// src/github.rs
//
// Fetches latest stable release tags from the GitHub Releases API.
// Versions are sorted newest-first by semver (major.minor.patch) so that
// index 0 is always the most recent stable release, regardless of the
// order GitHub returns them in.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::LazyLock;

const BITCOIN_API: &str =
    "https://api.github.com/repos/bitcoin/bitcoin/releases?per_page=30";
const ELECTRS_API: &str =
    "https://api.github.com/repos/romanz/electrs/releases?per_page=30";
const MAX_VERSIONS: usize = 10;

// ─── Shared HTTP client ───────────────────────────────────────────────────────

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .expect("Failed to build shared reqwest client")
});

// ─── GitHub API response shape ────────────────────────────────────────────────

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name:   String,
    /// GitHub's own pre-release flag — more reliable than string matching alone.
    prerelease: bool,
}

// ─── Public fetch functions ───────────────────────────────────────────────────

/// Fetch up to 10 stable Bitcoin Core release tags, newest first.
pub async fn fetch_bitcoin_versions() -> Result<Vec<String>> {
    fetch_versions(BITCOIN_API, "Bitcoin Core").await
}

/// Fetch up to 10 stable Electrs release tags, newest first.
pub async fn fetch_electrs_versions() -> Result<Vec<String>> {
    fetch_versions(ELECTRS_API, "Electrs").await
}

// ─── Shared implementation ────────────────────────────────────────────────────

async fn fetch_versions(url: &str, project: &str) -> Result<Vec<String>> {
    let releases: Vec<GitHubRelease> = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .with_context(|| format!("HTTP GET failed for {project} releases"))?
        .error_for_status()
        .with_context(|| format!("GitHub API returned error status for {project}"))?
        .json()
        .await
        .with_context(|| format!("Failed to parse {project} release JSON"))?;

    let mut versions: Vec<String> = releases
        .into_iter()
        // Filter out pre-releases via both the API flag and "rc" in the tag.
        .filter(|r| !r.prerelease && !r.tag_name.to_ascii_lowercase().contains("rc"))
        .map(|r| r.tag_name)
        .collect();

    // Sort newest-first by semver tuple (major, minor, patch).
    // This guarantees the correct ordering even when the API returns
    // releases out of order (e.g. patch releases interleaved with majors).
    versions.sort_by(|a, b| parse_semver(b).cmp(&parse_semver(a)));
    versions.truncate(MAX_VERSIONS);

    Ok(versions)
}

// ─── Semver parser ────────────────────────────────────────────────────────────

/// Parse a version tag into a `(major, minor, patch)` tuple for sorting.
/// Strips any leading `v`.  Unknown / malformed tags sort as `(0, 0, 0)`.
fn parse_semver(tag: &str) -> (u32, u32, u32) {
    let s = tag.trim_start_matches('v');
    let mut parts = s.splitn(4, '.').map(|p| p.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    (major, minor, patch)
}
