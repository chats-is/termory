//! "New version available" detection for the managed CLIs.
//!
//! Each tool publishes its latest release somewhere queryable:
//! - Codex / Gemini / OpenCode → the npm registry
//!   (`https://registry.npmjs.org/<pkg>/latest`, `.version`).
//! - Claude Code → its native release channel
//!   (`https://downloads.claude.ai/claude-code-releases/latest`, a bare
//!   version string). npm (`@anthropic-ai/claude-code`) is NO LONGER the
//!   recommended install (native installer / brew / winget are), so it can
//!   lag — we read the same endpoint `claude.ai/install.sh` uses.
//! - Grok Build → its installer's channel endpoint
//!   (`https://x.ai/cli/stable`, a bare version string — the same URL
//!   `x.ai/cli/install.sh` reads to discover the latest release).
//! - Claude Desktop → NO simple public "latest" endpoint, so it is not
//!   checked (the Providers card keeps showing installed-only).
//!
//! The frontend compares these against the installed versions
//! (`detect_cli_versions`) and shows an update badge when behind. Results
//! are cached for [`CACHE_TTL`] so opening the Providers page repeatedly
//! doesn't re-hit the network; the Recheck button forces a refresh.
//!
//! Network parsing helpers are unit-tested; the live fetches are not
//! (same policy as `quota.rs`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::providers::CliApp;

/// How long a fetched result stays fresh before the next call re-hits
/// the network. Version releases are infrequent; 6h keeps the page snappy.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// npm registry base — `/<pkg>/latest` returns the `latest` dist-tag's
/// packument, whose `.version` is the newest stable release.
const NPM_REGISTRY: &str = "https://registry.npmjs.org";

/// Claude Code's native release channel — a bare version string, the same
/// endpoint the official `claude.ai/install.sh` bootstrap reads.
const CLAUDE_LATEST_URL: &str = "https://downloads.claude.ai/claude-code-releases/latest";

/// Grok's installer channel endpoint — a bare version string.
const GROK_STABLE_URL: &str = "https://x.ai/cli/stable";

static LATEST: Mutex<Option<(Instant, HashMap<CliApp, Option<String>>)>> = Mutex::new(None);

/// The npm package each CLI publishes under, or `None` when the tool's
/// latest version comes from elsewhere: Claude Code reads its native
/// release channel (npm no longer recommended), Grok its installer
/// endpoint, and Claude Desktop is unsupported.
pub fn npm_package(app: CliApp) -> Option<&'static str> {
    match app {
        CliApp::Codex => Some("@openai/codex"),
        CliApp::Gemini => Some("@google/gemini-cli"),
        CliApp::Opencode => Some("opencode-ai"),
        CliApp::Claude | CliApp::Grok | CliApp::ClaudeDesktop => None,
    }
}

/// `MAJOR.MINOR.PATCH` with an optional `-prerelease` suffix — the shape
/// every managed CLI reports and the exact pattern grok's own installer
/// validates against.
pub fn valid_version(s: &str) -> bool {
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (s, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    match pre {
        None => true,
        Some(p) => {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
        }
    }
}

/// Pull `.version` out of an npm `/<pkg>/latest` response body.
pub fn parse_npm_version(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let version = v.get("version")?.as_str()?.trim();
    valid_version(version).then(|| version.to_string())
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("Termory/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
}

async fn fetch_npm_latest(client: &reqwest::Client, pkg: &str) -> Option<String> {
    let url = format!("{NPM_REGISTRY}/{pkg}/latest");
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse_npm_version(&body)
}

/// Fetch a bare `MAJOR.MINOR.PATCH` version string from a plain-text
/// endpoint (Claude Code's release channel, Grok's installer channel).
async fn fetch_bare_version(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let version = body.trim();
    valid_version(version).then(|| version.to_string())
}

/// Latest known version for each managed CLI (keyed by [`CliApp`]).
/// `None` for a tool means "couldn't determine" (network failure, or
/// unsupported like Claude Desktop) — the frontend simply shows no badge.
///
/// Cached for [`CACHE_TTL`]; pass `force` to bypass the cache (Recheck).
pub async fn detect_latest_versions(force: bool) -> HashMap<CliApp, Option<String>> {
    if !force {
        if let Ok(guard) = LATEST.lock() {
            if let Some((at, cached)) = guard.as_ref() {
                if at.elapsed() < CACHE_TTL {
                    return cached.clone();
                }
            }
        }
    }

    let mut out: HashMap<CliApp, Option<String>> = HashMap::new();

    if let Some(client) = http_client() {
        // Fetch every source concurrently — independent requests, each
        // with its own 10s timeout.
        let (claude, codex_npm, gemini, opencode, grok) = tokio::join!(
            fetch_bare_version(&client, CLAUDE_LATEST_URL),
            fetch_npm_latest(&client, npm_package(CliApp::Codex).unwrap()),
            fetch_npm_latest(&client, npm_package(CliApp::Gemini).unwrap()),
            fetch_npm_latest(&client, npm_package(CliApp::Opencode).unwrap()),
            fetch_bare_version(&client, GROK_STABLE_URL),
        );

        out.insert(CliApp::Claude, claude);
        // Codex also writes `~/.codex/version.json` when it self-checks —
        // use that as an offline fallback when npm is unreachable.
        out.insert(
            CliApp::Codex,
            codex_npm.or_else(crate::providers::codex_latest_known_version),
        );
        out.insert(CliApp::Gemini, gemini);
        out.insert(CliApp::Opencode, opencode);
        out.insert(CliApp::Grok, grok);
    } else {
        // Client init failed — still surface Codex's local fallback.
        out.insert(
            CliApp::Codex,
            crate::providers::codex_latest_known_version(),
        );
    }

    out.entry(CliApp::ClaudeDesktop).or_insert(None);

    if let Ok(mut guard) = LATEST.lock() {
        *guard = Some((Instant::now(), out.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_package_maps_the_three_npm_clis() {
        assert_eq!(npm_package(CliApp::Codex), Some("@openai/codex"));
        assert_eq!(npm_package(CliApp::Gemini), Some("@google/gemini-cli"));
        assert_eq!(npm_package(CliApp::Opencode), Some("opencode-ai"));
        // Claude Code reads its native release channel, not npm.
        assert_eq!(npm_package(CliApp::Claude), None);
        assert_eq!(npm_package(CliApp::Grok), None);
        assert_eq!(npm_package(CliApp::ClaudeDesktop), None);
    }

    #[test]
    fn valid_version_accepts_semver_and_prerelease() {
        assert!(valid_version("2.1.216"));
        assert!(valid_version("0.144.6"));
        assert!(valid_version("0.1.42-beta"));
        assert!(valid_version("1.0.0-rc.1"));
    }

    #[test]
    fn valid_version_rejects_junk() {
        assert!(!valid_version(""));
        assert!(!valid_version("2.1"));
        assert!(!valid_version("2.1.216.3"));
        assert!(!valid_version("v2.1.216"));
        assert!(!valid_version("latest"));
        assert!(!valid_version("2.1.x"));
        assert!(!valid_version("2.1.216-"));
        // An HTML error page must never parse as a version.
        assert!(!valid_version("<!doctype html>"));
    }

    #[test]
    fn parse_npm_version_reads_version_field() {
        assert_eq!(
            parse_npm_version(r#"{"name":"@openai/codex","version":"0.144.6"}"#),
            Some("0.144.6".to_string())
        );
    }

    #[test]
    fn parse_npm_version_rejects_missing_or_invalid() {
        assert_eq!(parse_npm_version(r#"{"name":"x"}"#), None);
        assert_eq!(parse_npm_version(r#"{"version":"not-a-version"}"#), None);
        assert_eq!(parse_npm_version("not json"), None);
    }
}
