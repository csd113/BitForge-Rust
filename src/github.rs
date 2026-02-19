// src/github.rs
//
// Fetches the latest stable release tags for Bitcoin Core and Electrs from
// the GitHub Releases API.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::LazyLock;

const BITCOIN_API: &str =
    "https://api.github.com/repos/bitcoin/bitcoin/releases?per_page=30";
const ELECTRS_API: &str =
    "https://api.github.com/repos/romanz/electrs/releases?per_page=30";
const MAX_VERSIONS: usize = 10;

// ─── Shared HTTP client ───────────────────────────────────────────────────────
// reqwest::Client is designed to be cloned and shared — it manages an internal
// connection pool.  Building one per request wastes resources and bypasses the
// pool entirely.

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
    /// GitHub's own pre-release flag — more reliable than string matching.
    prerelease: bool,
}

// ─── Public fetch functions ───────────────────────────────────────────────────

/// Fetch up to 10 stable Bitcoin Core release tags from GitHub.
pub async fn fetch_bitcoin_versions() -> Result<Vec<String>> {
    fetch_versions(BITCOIN_API, "Bitcoin Core").await
}

/// Fetch up to 10 stable Electrs release tags from GitHub.
pub async fn fetch_electrs_versions() -> Result<Vec<String>> {
    fetch_versions(ELECTRS_API, "Electrs").await
}

// ─── Shared implementation ────────────────────────────────────────────────────

async fn fetch_versions(url: &str, project: &str) -> Result<Vec<String>> {
    let response = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .with_context(|| format!("HTTP GET failed for {project} releases"))?
        .error_for_status()
        .with_context(|| format!("GitHub API returned error status for {project}"))?;

    let releases: Vec<GitHubRelease> = response
        .json()
        .await
        .with_context(|| format!("Failed to parse {project} release JSON"))?;

    let versions: Vec<String> = releases
        .into_iter()
        // Filter pre-releases via both the API flag and "rc" in the name.
        .filter(|r| !r.prerelease && !r.tag_name.to_ascii_lowercase().contains("rc"))
        .map(|r| r.tag_name)
        .take(MAX_VERSIONS)
        .collect();

    Ok(versions)
}
