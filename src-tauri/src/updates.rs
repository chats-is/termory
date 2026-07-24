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

/// The Codex DESKTOP app's Sparkle appcast — the app's own updater feed,
/// read verbatim from its bundled config key `codexSparkleFeedUrl`. A
/// static XML file on a CDN: public, unauthenticated, and carrying no
/// device/installation identifier, unlike Claude Desktop's update API
/// (which is why that one stays unchecked — see the module header).
///
/// macOS ONLY. The Windows Codex app ships as an MSIX through the
/// Microsoft Store, which owns its updates and publishes no appcast.
const CODEX_APP_APPCAST_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/appcast.xml";

/// Key the Codex desktop app's latest version rides under in the
/// `detect_latest_versions_cmd` map. Not a `CliApp` — mirrored by
/// `CODEX_APP_LATEST_KEY` in `ProvidersPage.tsx`.
pub const CODEX_APP_KEY: &str = "codex-app";

/// Latest known versions, as one cache entry.
#[derive(Clone, Default)]
pub struct LatestVersions {
    /// Per managed CLI. `None` = couldn't determine (network failure, or
    /// unsupported like Claude Desktop).
    pub clis: HashMap<CliApp, Option<String>>,
    /// The Codex DESKTOP app. Deliberately NOT `clis[Codex]`: that one is
    /// npm's `@openai/codex`, which publishes the CLI only. The two are
    /// separately versioned products (`v0.144.6` vs `v26.721.30844`), so
    /// comparing one against the other would be meaningless.
    pub codex_app: Option<String>,
}

static LATEST: Mutex<Option<(Instant, LatestVersions)>> = Mutex::new(None);

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

/// One `<item>` of a Sparkle appcast, reduced to the fields that decide
/// whether it applies to this machine.
#[derive(Debug, PartialEq)]
pub(crate) struct AppcastItem {
    /// `sparkle:shortVersionString` — the MARKETING version, which is
    /// what the app reports as its own version and therefore the only
    /// one comparable with what we display. NOT `sparkle:version`,
    /// which is an opaque build number (`5813`).
    pub version: String,
    /// `sparkle:hardwareRequirements`; `None` = applies to any arch.
    pub arch: Option<String>,
    /// `sparkle:minimumSystemVersion`; `None` = no floor.
    pub min_os: Option<String>,
}

/// Pull the fields we need out of a Sparkle appcast. Hand-rolled rather
/// than pulling in an XML crate — same call as `claude_desktop::version`
/// makes for Info.plist, and the feed is machine-generated with a fixed
/// shape.
pub(crate) fn parse_appcast(xml: &str) -> Vec<AppcastItem> {
    let mut items = Vec::new();
    for chunk in xml.split("<item>").skip(1) {
        // Stop at the item's own end so a malformed feed can't let one
        // item borrow the next one's fields.
        let body = chunk.split("</item>").next().unwrap_or(chunk);
        let Some(version) = tag_text(body, "sparkle:shortVersionString") else {
            continue;
        };
        items.push(AppcastItem {
            version,
            arch: tag_text(body, "sparkle:hardwareRequirements"),
            min_os: tag_text(body, "sparkle:minimumSystemVersion"),
        });
    }
    items
}

/// Text content of the first `<tag>…</tag>` in `body`, trimmed.
fn tag_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let rest = body.split_once(&open)?.1;
    let text = rest.split_once(&format!("</{tag}>"))?.0.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// True when an appcast item's `hardwareRequirements` covers `host_arch`
/// (a `std::env::consts::ARCH` value). Sparkle spells architectures the
/// Apple way (`arm64` / `x86_64`), Rust the LLVM way (`aarch64`), and
/// some feeds use `x64` — accept all spellings. An item with no
/// requirement is universal.
fn arch_matches(item_arch: Option<&str>, host_arch: &str) -> bool {
    let Some(item_arch) = item_arch else {
        return true;
    };
    let item_arch = item_arch.trim().to_ascii_lowercase();
    match host_arch {
        "aarch64" | "arm64" => matches!(item_arch.as_str(), "arm64" | "aarch64"),
        "x86_64" => matches!(item_arch.as_str(), "x86_64" | "x64" | "amd64"),
        other => item_arch == other,
    }
}

/// Numeric segments of a dotted version, for ordering (`26.715.31925` <
/// `26.721.30844` — a lexicographic compare gets this wrong).
fn version_key(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

/// Newest appcast version this machine could actually install.
///
/// Filtering matters on both axes: the feed is ONE arch-agnostic URL
/// (Sparkle itself selects per item), so an Intel Mac must not be told
/// about an arm64-only build; and an item whose `minimumSystemVersion`
/// is above the host's macOS would be an update the user can't take.
/// Items are not assumed to be ordered — we take the max.
pub(crate) fn pick_appcast_latest(
    items: &[AppcastItem],
    host_arch: &str,
    host_os: Option<&str>,
) -> Option<String> {
    items
        .iter()
        .filter(|item| arch_matches(item.arch.as_deref(), host_arch))
        .filter(|item| match (&item.min_os, host_os) {
            // No floor, or no host version to compare → don't exclude.
            (None, _) | (_, None) => true,
            (Some(min), Some(host)) => version_key(host) >= version_key(min),
        })
        .filter(|item| valid_version(&item.version))
        .max_by_key(|item| version_key(&item.version))
        .map(|item| item.version.clone())
}

/// A host OS version string usable for the `minimumSystemVersion`
/// comparison, or `None` when it isn't numeric.
///
/// `os_info` returns the literal `"Unknown"` when it can't determine the
/// version. Passed through as `Some("Unknown")` that parses to `[0]`,
/// which is BELOW every real `minimumSystemVersion` — so it would filter
/// out every item and hide the badge, the exact opposite of the "can't
/// tell → don't exclude" rule `pick_appcast_latest` intends. Anything
/// not starting with a digit is therefore treated as unknown.
fn parseable_host_os(raw: &str) -> Option<String> {
    let raw = raw.trim();
    raw.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| raw.to_string())
}

/// Fetch + resolve the Codex desktop app's latest version for THIS
/// machine. macOS-only; `None` everywhere else (see
/// [`CODEX_APP_APPCAST_URL`]).
async fn fetch_codex_app_latest(client: &reqwest::Client) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let resp = client.get(CODEX_APP_APPCAST_URL).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let host_os = os_info::get().version().to_string();
    pick_appcast_latest(
        &parse_appcast(&body),
        std::env::consts::ARCH,
        parseable_host_os(&host_os).as_deref(),
    )
}

/// Latest known version for each managed CLI (keyed by [`CliApp`]).
/// `None` for a tool means "couldn't determine" (network failure, or
/// unsupported like Claude Desktop) — the frontend simply shows no badge.
///
/// Cached for [`CACHE_TTL`]; pass `force` to bypass the cache (Recheck).
pub async fn detect_latest_versions(force: bool) -> LatestVersions {
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
    let mut codex_app = None;

    if let Some(client) = http_client() {
        // Fetch every source concurrently — independent requests, each
        // with its own 10s timeout.
        let (claude, codex_npm, gemini, opencode, grok, codex_app_latest) = tokio::join!(
            fetch_bare_version(&client, CLAUDE_LATEST_URL),
            fetch_npm_latest(&client, npm_package(CliApp::Codex).unwrap()),
            fetch_npm_latest(&client, npm_package(CliApp::Gemini).unwrap()),
            fetch_npm_latest(&client, npm_package(CliApp::Opencode).unwrap()),
            fetch_bare_version(&client, GROK_STABLE_URL),
            fetch_codex_app_latest(&client),
        );

        codex_app = codex_app_latest;
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

    let result = LatestVersions {
        clis: out,
        codex_app,
    };
    if let Ok(mut guard) = LATEST.lock() {
        *guard = Some((Instant::now(), result.clone()));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed shape of the real feed (fields in the order OpenAI emits
    /// them), plus an x86_64 item the live feed doesn't currently carry
    /// — the URL is arch-agnostic, so both must be handled.
    const APPCAST: &str = r#"
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
  <channel>
    <title>Codex</title>
    <item>
      <title>26.721.30844</title>
      <sparkle:version>5813</sparkle:version>
      <sparkle:shortVersionString>26.721.30844</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>12.0</sparkle:minimumSystemVersion>
      <sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements>
      <enclosure url="https://example.test/a.zip" />
    </item>
    <item>
      <title>26.715.72359</title>
      <sparkle:version>5718</sparkle:version>
      <sparkle:shortVersionString>26.715.72359</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>12.0</sparkle:minimumSystemVersion>
      <sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements>
    </item>
    <item>
      <title>26.700.10000</title>
      <sparkle:shortVersionString>26.700.10000</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>12.0</sparkle:minimumSystemVersion>
      <sparkle:hardwareRequirements>x86_64</sparkle:hardwareRequirements>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parse_appcast_reads_marketing_version_arch_and_min_os() {
        let items = parse_appcast(APPCAST);
        assert_eq!(items.len(), 3);
        // The MARKETING version, never the opaque `sparkle:version` build
        // number (5813) — only the former is comparable with what the app
        // reports as its own version.
        assert_eq!(items[0].version, "26.721.30844");
        assert_eq!(items[0].arch.as_deref(), Some("arm64"));
        assert_eq!(items[0].min_os.as_deref(), Some("12.0"));
    }

    #[test]
    fn pick_appcast_latest_filters_by_arch() {
        let items = parse_appcast(APPCAST);
        // One arch-agnostic feed serves every Mac, so an Intel host must
        // not be offered an arm64-only build (and vice versa).
        assert_eq!(
            pick_appcast_latest(&items, "aarch64", Some("26.0")).as_deref(),
            Some("26.721.30844")
        );
        assert_eq!(
            pick_appcast_latest(&items, "x86_64", Some("26.0")).as_deref(),
            Some("26.700.10000")
        );
    }

    #[test]
    fn pick_appcast_latest_respects_minimum_system_version() {
        let items = parse_appcast(APPCAST);
        // macOS 11 is below every item's 12.0 floor — badging an update
        // the user cannot install would be worse than no badge.
        assert!(pick_appcast_latest(&items, "aarch64", Some("11.7.1")).is_none());
        // Unknown host version → don't exclude anything.
        assert_eq!(
            pick_appcast_latest(&items, "aarch64", None).as_deref(),
            Some("26.721.30844")
        );
    }

    #[test]
    fn parseable_host_os_rejects_non_numeric() {
        // os_info yields the literal "Unknown" when it can't tell — that
        // must become None, not a Some that parses to [0] and filters
        // out every item (hiding the badge instead of showing it).
        assert_eq!(parseable_host_os("Unknown"), None);
        assert_eq!(parseable_host_os(""), None);
        assert_eq!(parseable_host_os("15.5.0"), Some("15.5.0".to_string()));
    }

    #[test]
    fn pick_appcast_latest_shows_update_when_host_os_is_unknown() {
        // The bug this guards: an unknown host version (→ None) must
        // NOT be treated as "older than every floor". With None the
        // min_os axis is skipped and the newest arch-matching item wins.
        let items = parse_appcast(APPCAST);
        assert_eq!(
            pick_appcast_latest(&items, "aarch64", parseable_host_os("Unknown").as_deref())
                .as_deref(),
            Some("26.721.30844")
        );
    }

    #[test]
    fn pick_appcast_latest_takes_the_max_not_the_first() {
        // Feed order is not a contract, and a lexicographic compare would
        // rank 26.715.31925 above 26.721.30844 ("715" > "721" is false,
        // but "9" > "3" at the patch level is the trap).
        let xml = r#"
<rss><channel>
  <item><sparkle:shortVersionString>26.721.30844</sparkle:shortVersionString></item>
  <item><sparkle:shortVersionString>26.715.99999</sparkle:shortVersionString></item>
</channel></rss>"#;
        assert_eq!(
            pick_appcast_latest(&parse_appcast(xml), "aarch64", None).as_deref(),
            Some("26.721.30844")
        );
    }

    #[test]
    fn parse_appcast_tolerates_junk_and_missing_fields() {
        assert!(parse_appcast("").is_empty());
        assert!(parse_appcast("<html>not an appcast</html>").is_empty());
        // An item without a version is skipped, not defaulted.
        assert!(parse_appcast("<item><title>x</title></item>").is_empty());
        // A truncated feed must not let one item borrow the next's fields.
        let split = r#"<item><sparkle:shortVersionString>1.2.3</sparkle:shortVersionString>
          </item><item><sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements></item>"#;
        let items = parse_appcast(split);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].arch, None);
    }

    #[test]
    fn arch_matches_accepts_every_spelling() {
        // Sparkle says arm64/x86_64, Rust says aarch64, some feeds x64.
        assert!(arch_matches(Some("arm64"), "aarch64"));
        assert!(arch_matches(Some("x64"), "x86_64"));
        assert!(arch_matches(Some("x86_64"), "x86_64"));
        assert!(!arch_matches(Some("arm64"), "x86_64"));
        // No requirement = universal build.
        assert!(arch_matches(None, "x86_64"));
    }

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
