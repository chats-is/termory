//! Official subscription quota — reads the CLI's existing OAuth
//! credentials (never writes them) and queries the official usage
//! endpoint for the rate-limit windows (5-hour session, 7-day weekly,
//! per-model weekly, …).
//!
//! Claude Code, Codex, and Gemini CLI are implemented; OpenCode has
//! no official subscription quota. The entry point is keyed by
//! `CliApp` behind one IPC shape.
//!
//! Reference: cc-switch `src-tauri/src/services/subscription.rs`
//! (credential sources + endpoint + response shape, cross-checked).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::providers::CliApp;

/// CLIs with a quota implementation — the single backend list every
/// consumer (tray submenu, tray refresh trigger) keys off. Add the
/// CLI here when its `fetch_quota` arm lands. Frontend mirror:
/// `QUOTA_SUPPORTED` in ProvidersPage.tsx.
pub const SUPPORTED: &[CliApp] = &[CliApp::Claude, CliApp::Codex, CliApp::Gemini];

pub fn supports_quota(app: CliApp) -> bool {
    SUPPORTED.contains(&app)
}

/// Emitted with a `SubscriptionQuota` payload after every completed
/// quota fetch (any trigger), so the Providers page stays in sync with
/// backend-initiated fetches. Lives here (quota state, not tray UI);
/// the emit happens in tray::refresh_quota, the central result sink.
/// Frontend mirror: QUOTA_CHANGED_EVENT in src/constants.ts.
pub const QUOTA_CHANGED_EVENT: &str = "termory:quota-changed";

/// Which CLI a credential-file path belongs to — the single list the
/// filesystem watcher matches to force a quota refresh on login /
/// logout (the readers below own the same paths). Keychain-backed
/// credentials produce no file event; the 60s not_found retry is the
/// fallback there.
pub fn credential_cli_for_path(path: &std::path::Path) -> Option<CliApp> {
    let parent_is = |dir: &str| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some(dir)
    };
    match path.file_name().and_then(|n| n.to_str())? {
        ".credentials.json" => Some(CliApp::Claude),
        // The parent check excludes OpenCode's unrelated
        // ~/.local/share/opencode/auth.json.
        "auth.json" if parent_is(".codex") => Some(CliApp::Codex),
        "oauth_creds.json" if parent_is(".gemini") => Some(CliApp::Gemini),
        _ => None,
    }
}

/// Pressure thresholds (used %) for the tray glyph color. Frontend
/// mirror: `QUOTA_WARN_PCT` / `QUOTA_CRIT_PCT` in src/lib/quota-utils.ts
/// (the in-app ring) — keep the two in sync.
pub const WARN_PCT: f64 = 75.0;
pub const CRIT_PCT: f64 = 90.0;

// ===================================================================
// Wire types (camelCase to the frontend)
// ===================================================================

/// State of the on-disk OAuth credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Valid,
    Expired,
    NotFound,
    ParseError,
}

/// One rate-limit window (e.g. the 5-hour session or 7-day week).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaTier {
    /// Window id as returned by the API: `five_hour`, `seven_day`,
    /// `seven_day_opus`, `seven_day_sonnet`, … Unknown ids pass
    /// through verbatim so new windows surface without a release.
    pub name: String,
    /// Used percentage 0–100.
    pub utilization: f64,
    /// ISO 8601 reset time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// Pay-as-you-go overflow usage (Claude "extra usage").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraUsage {
    pub is_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_credits: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

/// Result of one quota query for one CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionQuota {
    /// CLI key, matching the frontend `CliApp` literals ("claude", …).
    pub app: String,
    pub credential_status: CredentialStatus,
    pub success: bool,
    pub tiers: Vec<QuotaTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_usage: Option<ExtraUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix millis of the query (frontend staleness display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queried_at: Option<i64>,
}

impl SubscriptionQuota {
    fn not_found(app: &str) -> Self {
        Self {
            app: app.to_string(),
            credential_status: CredentialStatus::NotFound,
            success: false,
            tiers: vec![],
            extra_usage: None,
            error: None,
            queried_at: Some(now_millis()),
        }
    }

    fn success(app: &str, tiers: Vec<QuotaTier>, extra_usage: Option<ExtraUsage>) -> Self {
        Self {
            app: app.to_string(),
            credential_status: CredentialStatus::Valid,
            success: true,
            tiers,
            extra_usage,
            error: None,
            queried_at: Some(now_millis()),
        }
    }

    fn error(app: &str, status: CredentialStatus, message: String) -> Self {
        Self {
            app: app.to_string(),
            credential_status: status,
            success: false,
            tiers: vec![],
            extra_usage: None,
            error: Some(message),
            queried_at: Some(now_millis()),
        }
    }
}

// ===================================================================
// Claude credentials
// ===================================================================

/// Parsed credential: token (may be present even when expired — the
/// caller still tries it) + status + diagnostic message.
type Credential = (Option<String>, CredentialStatus, Option<String>);

/// Claude Code's config dir — `sessions::claude_config_root` (the
/// scanners' single source: `CLAUDE_CONFIG_DIR`, else `~/.claude`).
fn claude_config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| crate::sessions::claude_config_root(&h))
}

/// Read the Claude Code OAuth credential. Source priority mirrors
/// Claude Code itself (auth.ts:1323 — Keychain on macOS, else file):
///  1. macOS Keychain, service "Claude Code-credentials"
///  2. `CLAUDE_CONFIG_DIR`/.credentials.json (default ~/.claude/)
fn read_claude_credentials() -> Credential {
    #[cfg(target_os = "macos")]
    {
        if let Some(found) = read_claude_credentials_from_keychain() {
            return found;
        }
    }
    read_claude_credentials_from_file()
}

#[cfg(target_os = "macos")]
fn read_claude_credentials_from_keychain() -> Option<Credential> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None; // no Keychain entry — fall back to the file
    }
    let json = String::from_utf8(output.stdout).ok()?;
    let json = json.trim();
    if json.is_empty() {
        return None;
    }
    Some(parse_claude_credentials(json))
}

fn read_claude_credentials_from_file() -> Credential {
    let Some(path) = claude_config_dir().map(|d| d.join(".credentials.json")) else {
        return (None, CredentialStatus::NotFound, None);
    };
    if !path.exists() {
        return (None, CredentialStatus::NotFound, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_claude_credentials(&content),
        Err(err) => (
            None,
            CredentialStatus::ParseError,
            Some(format!("Failed to read credentials file: {err}")),
        ),
    }
}

/// Parse the credentials JSON (shared by Keychain and file — both hold
/// the same document):
/// `{"claudeAiOauth": {"accessToken": "...", "expiresAt": ...}}`
/// (legacy key `"claude.ai_oauth"` also accepted).
fn parse_claude_credentials(content: &str) -> Credential {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(err) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse credentials JSON: {err}")),
            );
        }
    };

    let Some(entry) = parsed
        .get("claudeAiOauth")
        .or_else(|| parsed.get("claude.ai_oauth"))
    else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("No OAuth entry found in credentials".to_string()),
        );
    };

    let access_token = match entry.get("accessToken").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return (
                None,
                CredentialStatus::ParseError,
                Some("accessToken is empty or missing".to_string()),
            );
        }
    };

    if let Some(expires_at) = entry.get("expiresAt") {
        if is_token_expired(expires_at) {
            return (
                Some(access_token),
                CredentialStatus::Expired,
                Some("OAuth token has expired".to_string()),
            );
        }
    }

    (Some(access_token), CredentialStatus::Valid, None)
}

/// `expiresAt` appears as a Unix timestamp (seconds or millis) or an
/// ISO 8601 string depending on the Claude Code version.
fn is_token_expired(expires_at: &serde_json::Value) -> bool {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match expires_at {
        serde_json::Value::Number(n) => match n.as_u64() {
            // millis-scale timestamps are > 1e12
            Some(ts) if ts > 1_000_000_000_000 => ts / 1000 < now_secs,
            Some(ts) => ts < now_secs,
            None => false,
        },
        serde_json::Value::String(s) => {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                (dt.timestamp() as u64) < now_secs
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            {
                (dt.and_utc().timestamp() as u64) < now_secs
            } else {
                false // unparseable → don't assume expired
            }
        }
        _ => false,
    }
}

// ===================================================================
// Claude usage API
// ===================================================================

/// Known window ids, in display order. Unknown ids are appended after.
const CLAUDE_KNOWN_TIERS: &[&str] = &[
    "five_hour",
    "seven_day",
    "seven_day_opus",
    "seven_day_sonnet",
];

/// Parse the `GET /api/oauth/usage` response body into tiers +
/// extra usage. Top-level keys are window objects
/// `{ utilization, resets_at }` plus an `extra_usage` object;
/// windows without a `utilization` are skipped (not active for this
/// plan), and unknown window keys pass through with their raw name.
fn parse_claude_usage(body: &serde_json::Value) -> (Vec<QuotaTier>, Option<ExtraUsage>) {
    fn window_tier(name: &str, value: &serde_json::Value) -> Option<QuotaTier> {
        let utilization = value.get("utilization")?.as_f64()?;
        Some(QuotaTier {
            name: name.to_string(),
            utilization,
            resets_at: value
                .get("resets_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    let mut tiers = Vec::new();
    for &name in CLAUDE_KNOWN_TIERS {
        if let Some(tier) = body.get(name).and_then(|v| window_tier(name, v)) {
            tiers.push(tier);
        }
    }
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if key == "extra_usage" || CLAUDE_KNOWN_TIERS.contains(&key.as_str()) {
                continue;
            }
            if let Some(tier) = window_tier(key, value) {
                tiers.push(tier);
            }
        }
    }

    let extra_usage = body.get("extra_usage").map(|v| ExtraUsage {
        is_enabled: v
            .get("is_enabled")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        monthly_limit: v.get("monthly_limit").and_then(|n| n.as_f64()),
        used_credits: v.get("used_credits").and_then(|n| n.as_f64()),
        utilization: v.get("utilization").and_then(|n| n.as_f64()),
        currency: v.get("currency").and_then(|s| s.as_str()).map(String::from),
    });

    (tiers, extra_usage)
}

/// Human-readable error line for a non-2xx response: `HTTP {status}: {message}`.
/// The message is the API's own error text, extracted from the
/// Anthropic error envelope `{"error": {"type": ..., "message": ...}}`
/// (a top-level `message` is also accepted). Non-JSON / unexpected
/// bodies fall back to the raw text, truncated.
fn http_error_message(status: reqwest::StatusCode, body: &str) -> String {
    match api_error_detail(body) {
        Some(detail) => format!("HTTP {status}: {detail}"),
        None => format!("HTTP {status}"),
    }
}

/// The API's error `message` out of a response body; None when the
/// body is empty / has no usable text. Accepts the Anthropic envelope
/// (`error.message`), a top-level `message`, and the ChatGPT backend's
/// `detail` field.
fn api_error_detail(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = v
            .pointer("/error/message")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .and_then(|m| m.as_str())
        {
            let msg = msg.trim();
            if !msg.is_empty() {
                return Some(msg.to_string());
            }
        }
    }
    let raw = body.trim();
    if raw.is_empty() {
        return None;
    }
    let mut out: String = raw.chars().take(200).collect();
    if raw.chars().count() > 200 {
        out.push('…');
    }
    Some(out)
}

// ===================================================================
// Shared query plumbing
// ===================================================================

fn network_error(app_key: &str, err: impl std::fmt::Display) -> SubscriptionQuota {
    SubscriptionQuota::error(
        app_key,
        CredentialStatus::Valid,
        format!("Network error: {err}"),
    )
}

/// The 10s-timeout client every quota query uses.
fn quota_http_client(app_key: &str) -> Result<reqwest::Client, SubscriptionQuota> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|err| {
            SubscriptionQuota::error(
                app_key,
                CredentialStatus::Valid,
                format!("HTTP client init failed: {err}"),
            )
        })
}

/// Shared response handling: non-2xx → error result (401/403 marks the
/// credential Expired; message via `http_error_message` so the card
/// shows the API's own error text), 2xx → parsed JSON body.
async fn read_json_or_error(
    app_key: &str,
    resp: reqwest::Response,
) -> Result<serde_json::Value, SubscriptionQuota> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let cred = if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            CredentialStatus::Expired
        } else {
            CredentialStatus::Valid
        };
        return Err(SubscriptionQuota::error(
            app_key,
            cred,
            http_error_message(status, &body),
        ));
    }
    resp.json().await.map_err(|err| {
        SubscriptionQuota::error(
            app_key,
            CredentialStatus::Valid,
            format!("Failed to parse API response: {err}"),
        )
    })
}

/// Shared credential-status scaffold for every CLI's fetch arm:
/// NotFound / ParseError short-circuit; Expired still TRIES the query
/// (local staleness heuristics can be wrong) and only reports Expired
/// when the API also rejects; Valid queries directly.
async fn quota_for_credential<Q, Fut>(
    app_key: &'static str,
    token: Option<String>,
    status: CredentialStatus,
    message: Option<String>,
    query: Q,
) -> SubscriptionQuota
where
    Q: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = SubscriptionQuota>,
{
    match status {
        CredentialStatus::NotFound => SubscriptionQuota::not_found(app_key),
        CredentialStatus::ParseError => SubscriptionQuota::error(
            app_key,
            CredentialStatus::ParseError,
            message.unwrap_or_else(|| "Failed to parse credentials".to_string()),
        ),
        CredentialStatus::Expired => {
            if let Some(token) = token {
                let result = query(token).await;
                if result.success {
                    return result;
                }
            }
            SubscriptionQuota::error(
                app_key,
                CredentialStatus::Expired,
                message.unwrap_or_else(|| "OAuth token has expired".to_string()),
            )
        }
        CredentialStatus::Valid => {
            let token = token.expect("token present when status is Valid");
            query(token).await
        }
    }
}

/// Query the official usage endpoint with the OAuth access token.
/// Endpoint + `anthropic-beta` header per cc-switch
/// `subscription.rs:321-323` (the same call Claude Code's `/usage`
/// command makes).
async fn query_claude_quota(access_token: &str) -> SubscriptionQuota {
    let client = match quota_http_client("claude") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let resp = match client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => return network_error("claude", err),
    };
    let body = match read_json_or_error("claude", resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (tiers, extra_usage) = parse_claude_usage(&body);
    SubscriptionQuota::success("claude", tiers, extra_usage)
}

// ===================================================================
// Codex credentials
// ===================================================================

/// Parsed Codex credential: token + ChatGPT account id (sent as the
/// `ChatGPT-Account-Id` header — codex-rs backend-client/src/client.rs:214)
/// + status + diagnostic message.
type CodexCredential = (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
);

/// Read the Codex OAuth credential. Source priority mirrors cc-switch
/// (`subscription.rs:459-467`):
///  1. macOS Keychain, service "Codex Auth"
///  2. `~/.codex/auth.json`
/// Only `auth_mode == "chatgpt"` (OAuth) carries subscription usage —
/// API-key mode has no rate-limit windows to show.
fn read_codex_credentials() -> CodexCredential {
    #[cfg(target_os = "macos")]
    {
        if let Some(found) = read_codex_credentials_from_keychain() {
            return found;
        }
    }
    read_codex_credentials_from_file()
}

#[cfg(target_os = "macos")]
fn read_codex_credentials_from_keychain() -> Option<CodexCredential> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "Codex Auth", "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None; // no Keychain entry — fall back to the file
    }
    let json = String::from_utf8(output.stdout).ok()?;
    let json = json.trim();
    if json.is_empty() {
        return None;
    }
    Some(parse_codex_credentials(json))
}

fn read_codex_credentials_from_file() -> CodexCredential {
    let Ok(path) = crate::providers::codex_auth_path() else {
        return (None, None, CredentialStatus::NotFound, None);
    };
    if !path.exists() {
        return (None, None, CredentialStatus::NotFound, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_codex_credentials(&content),
        Err(err) => (
            None,
            None,
            CredentialStatus::ParseError,
            Some(format!("Failed to read Codex auth file: {err}")),
        ),
    }
}

/// Parse the auth.json document (shared by Keychain and file):
/// `{"auth_mode": "chatgpt", "tokens": {"access_token": ..., "account_id": ...},
///   "last_refresh": "..."}` — shape per codex-rs `codex_login::AuthDotJson`.
fn parse_codex_credentials(content: &str) -> CodexCredential {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(err) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Codex auth JSON: {err}")),
            );
        }
    };

    if parsed.get("auth_mode").and_then(|v| v.as_str()) != Some("chatgpt") {
        // API-key login (or no login) — no subscription usage to query.
        return (
            None,
            None,
            CredentialStatus::NotFound,
            Some("Codex not using OAuth mode".to_string()),
        );
    }

    let Some(tokens) = parsed.get("tokens") else {
        return (
            None,
            None,
            CredentialStatus::ParseError,
            Some("No tokens in Codex auth".to_string()),
        );
    };
    let access_token = match tokens.get("access_token").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some("access_token is empty or missing".to_string()),
            );
        }
    };
    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Codex CLI auto-refreshes tokens older than ~8 days; a stale
    // last_refresh usually means the access token no longer works.
    if let Some(last_refresh) = parsed.get("last_refresh").and_then(|v| v.as_str()) {
        if is_codex_token_stale(last_refresh) {
            return (
                Some(access_token),
                account_id,
                CredentialStatus::Expired,
                Some("Codex token may be stale (>8 days since last refresh)".to_string()),
            );
        }
    }

    (
        Some(access_token),
        account_id,
        CredentialStatus::Valid,
        None,
    )
}

/// Stale when `last_refresh` (RFC 3339) is more than 8 days old —
/// the window after which Codex CLI itself forces a token refresh.
fn is_codex_token_stale(last_refresh: &str) -> bool {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match chrono::DateTime::parse_from_rfc3339(last_refresh) {
        Ok(dt) => now_secs.saturating_sub(dt.timestamp() as u64) > 8 * 24 * 3600,
        Err(_) => false, // unparseable → don't assume stale
    }
}

// ===================================================================
// Codex usage API
// ===================================================================

/// Tier name for a rate-limit window length, aligned with Claude's
/// naming so the frontend labels / tray glyphs apply unchanged:
/// 18000s → `five_hour`, 604800s → `seven_day`; other lengths become
/// `{n}_hour` / `{n}_day` and pass through with their raw name.
fn window_seconds_to_tier_name(secs: i64) -> String {
    match secs {
        18_000 => "five_hour".to_string(),
        604_800 => "seven_day".to_string(),
        s => {
            let hours = s / 3600;
            if hours >= 24 {
                format!("{}_day", hours / 24)
            } else {
                format!("{hours}_hour")
            }
        }
    }
}

fn unix_ts_to_iso(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339())
}

/// Parse the `GET /wham/usage` response. Shape per codex-rs
/// `RateLimitStatusPayload` (codex-backend-openapi-models): top-level
/// `rate_limit.{primary_window,secondary_window}`, each a
/// `RateLimitWindowSnapshot { used_percent, limit_window_seconds,
/// reset_at }` (rate_limit_window_snapshot.rs:14-23). Primary is the
/// 5-hour session window, secondary the weekly one.
fn parse_codex_usage(body: &serde_json::Value) -> Vec<QuotaTier> {
    let mut tiers = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = body.pointer(&format!("/rate_limit/{key}")) else {
            continue;
        };
        let Some(used) = window.get("used_percent").and_then(|v| v.as_f64()) else {
            continue;
        };
        tiers.push(QuotaTier {
            name: window
                .get("limit_window_seconds")
                .and_then(|v| v.as_i64())
                .map(window_seconds_to_tier_name)
                .unwrap_or_else(|| "unknown".to_string()),
            utilization: used,
            resets_at: window
                .get("reset_at")
                .and_then(|v| v.as_i64())
                .and_then(unix_ts_to_iso),
        });
    }
    tiers
}

/// Query the ChatGPT backend usage endpoint with the Codex OAuth
/// token. Endpoint per codex-rs `backend-client/src/client.rs:296`
/// (`{base}/wham/usage`, ChatGptApi path style); the
/// `ChatGPT-Account-Id` header per client.rs:214.
async fn query_codex_quota(access_token: &str, account_id: Option<&str>) -> SubscriptionQuota {
    let client = match quota_http_client("codex") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "codex-cli")
        .header("Accept", "application/json");
    if let Some(id) = account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(err) => return network_error("codex", err),
    };
    let body = match read_json_or_error("codex", resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    SubscriptionQuota::success("codex", parse_codex_usage(&body), None)
}

// ===================================================================
// Gemini credentials
// ===================================================================

/// Parsed Gemini credential: access token + refresh token + status +
/// diagnostic message. Google access tokens last ~1h, so Expired is
/// common — the refresh token (which doesn't expire unless revoked)
/// mints a fresh one at query time.
type GeminiCredential = (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
);

/// `~/.gemini/oauth_creds.json` — gemini-cli `storage.ts:22`
/// (OAUTH_FILE under the global gemini dir).
fn gemini_oauth_creds_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("oauth_creds.json"))
}

/// Read the Gemini CLI OAuth credential. Source priority mirrors
/// gemini-cli itself (`oauth-credential-storage.ts:16-17` — Keychain
/// service "gemini-cli-oauth", account "main-account"; file fallback):
///  1. macOS Keychain (keytar JSON)
///  2. `~/.gemini/oauth_creds.json` (legacy flat format)
fn read_gemini_credentials() -> GeminiCredential {
    #[cfg(target_os = "macos")]
    {
        if let Some(found) = read_gemini_credentials_from_keychain() {
            return found;
        }
    }
    read_gemini_credentials_from_file()
}

#[cfg(target_os = "macos")]
fn read_gemini_credentials_from_keychain() -> Option<GeminiCredential> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "gemini-cli-oauth",
            "-a",
            "main-account",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None; // no Keychain entry — fall back to the file
    }
    let json = String::from_utf8(output.stdout).ok()?;
    let json = json.trim();
    if json.is_empty() {
        return None;
    }
    Some(parse_gemini_keychain_json(json))
}

fn read_gemini_credentials_from_file() -> GeminiCredential {
    let Some(path) = gemini_oauth_creds_path() else {
        return (None, None, CredentialStatus::NotFound, None);
    };
    if !path.exists() {
        return (None, None, CredentialStatus::NotFound, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_gemini_file_json(&content),
        Err(err) => (
            None,
            None,
            CredentialStatus::ParseError,
            Some(format!("Failed to read Gemini credentials: {err}")),
        ),
    }
}

/// Keychain (keytar) document:
/// `{"token": {"accessToken": ..., "refreshToken": ..., "expiresAt": <ms>}, "updatedAt": ...}`.
/// A flat document (no `token` wrapper) falls through to the file parser.
#[cfg(any(target_os = "macos", test))]
fn parse_gemini_keychain_json(content: &str) -> GeminiCredential {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(err) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Gemini keychain JSON: {err}")),
            );
        }
    };
    let Some(token) = parsed.get("token") else {
        return parse_gemini_file_json(content);
    };
    let access = token
        .get("accessToken")
        .and_then(|v| v.as_str())
        .map(String::from);
    let refresh = token
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_ms = token.get("expiresAt").and_then(|v| v.as_i64());
    finish_gemini_credential(access, refresh, expires_ms)
}

/// File (oauth_creds.json) document:
/// `{"access_token": ..., "refresh_token": ..., "expiry_date": <ms>}`.
fn parse_gemini_file_json(content: &str) -> GeminiCredential {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(err) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Gemini credentials: {err}")),
            );
        }
    };
    let access = parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let refresh = parsed
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_ms = parsed.get("expiry_date").and_then(|v| v.as_i64());
    finish_gemini_credential(access, refresh, expires_ms)
}

/// Shared tail of both Gemini parsers: empty token → ParseError (the
/// refresh token is still returned for the mint-a-fresh-one path);
/// millisecond expiry in the past → Expired, keeping the token.
fn finish_gemini_credential(
    access: Option<String>,
    refresh: Option<String>,
    expires_ms: Option<i64>,
) -> GeminiCredential {
    let access = match access {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                None,
                refresh,
                CredentialStatus::ParseError,
                Some("access token is empty or missing".to_string()),
            );
        }
    };
    if let Some(ms) = expires_ms {
        if ms < now_millis() {
            return (
                Some(access),
                refresh,
                CredentialStatus::Expired,
                Some("Gemini access token has expired".to_string()),
            );
        }
    }
    (Some(access), refresh, CredentialStatus::Valid, None)
}

// ===================================================================
// Gemini usage API
// ===================================================================

/// Gemini CLI's public installed-app OAuth client (`oauth2.ts:76-85`
/// — the comment there documents the "secret" as non-secret for
/// installed applications).
const GEMINI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GEMINI_OAUTH_CLIENT_SECRET: &str = "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";

/// Mint a fresh ~1h access token from the refresh token (standard
/// Google token endpoint). The result is used in-memory only —
/// Termory never writes credentials back.
async fn refresh_gemini_token(refresh_token: &str) -> Option<String> {
    let client = quota_http_client("gemini").ok()?;
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", GEMINI_OAUTH_CLIENT_ID),
            ("client_secret", GEMINI_OAUTH_CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("access_token")?.as_str().map(String::from)
}

/// Bucket → display class. Gemini quotas are per-MODEL buckets, not
/// time windows (`types.ts:255-262` BucketInfo.modelId); group them as
/// Pro / Flash / Flash-Lite (cc-switch `classify_gemini_model`).
fn classify_gemini_model(model_id: &str) -> &str {
    if model_id.contains("flash-lite") {
        "gemini_flash_lite"
    } else if model_id.contains("flash") {
        "gemini_flash"
    } else if model_id.contains("pro") {
        "gemini_pro"
    } else {
        model_id
    }
}

/// `cloudaicompanionProject` arrives as a plain string or an object
/// with `id` / `projectId` depending on tier.
fn extract_project_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(obj) => obj
            .get("id")
            .or_else(|| obj.get("projectId"))
            .and_then(|v| v.as_str())
            .map(String::from),
        _ => None,
    }
}

/// Parse retrieveUserQuota's `buckets[]` (`types.ts:255-265`
/// BucketInfo { remainingFraction, resetTime, modelId }) into tiers:
/// one per model class, keeping the class's LOWEST remaining fraction
/// (the binding limit), converted to used %. Pro → Flash → Flash-Lite
/// order.
fn parse_gemini_quota(body: &serde_json::Value) -> Vec<QuotaTier> {
    // (class, lowest remaining fraction, its reset time)
    let mut classes: Vec<(String, f64, Option<String>)> = Vec::new();
    if let Some(buckets) = body.get("buckets").and_then(|b| b.as_array()) {
        for bucket in buckets {
            let model = bucket
                .get("modelId")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            let name = classify_gemini_model(model).to_string();
            let remaining = bucket
                .get("remainingFraction")
                .and_then(|f| f.as_f64())
                .unwrap_or(1.0)
                .clamp(0.0, 1.0);
            let reset = bucket
                .get("resetTime")
                .and_then(|r| r.as_str())
                .map(String::from);
            match classes.iter_mut().find(|(n, _, _)| *n == name) {
                Some(entry) => {
                    if remaining < entry.1 {
                        entry.1 = remaining;
                        if reset.is_some() {
                            entry.2 = reset;
                        }
                    }
                }
                None => classes.push((name, remaining, reset)),
            }
        }
    }
    let order = |n: &str| match n {
        "gemini_pro" => 0,
        "gemini_flash" => 1,
        "gemini_flash_lite" => 2,
        _ => 3,
    };
    classes.sort_by_key(|(n, _, _)| order(n));
    classes
        .into_iter()
        .map(|(name, remaining, reset)| QuotaTier {
            name,
            utilization: (1.0 - remaining) * 100.0,
            resets_at: reset,
        })
        .collect()
}

/// Query the Code Assist quota. Two POSTs (gemini-cli
/// `server.ts:263/363`): `v1internal:loadCodeAssist` for the
/// cloudaicompanion project id, then `v1internal:retrieveUserQuota`
/// for the per-model buckets.
async fn query_gemini_quota(access_token: &str) -> SubscriptionQuota {
    let client = match quota_http_client("gemini") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let resp = match client
        .post("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
        .header("Authorization", format!("Bearer {access_token}"))
        .json(&serde_json::json!({
            "metadata": { "ideType": "GEMINI_CLI", "pluginType": "GEMINI" }
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => return network_error("gemini", err),
    };
    let load_body = match read_json_or_error("gemini", resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let project = load_body
        .get("cloudaicompanionProject")
        .and_then(extract_project_id);

    let mut quota_req = serde_json::json!({});
    if let Some(ref pid) = project {
        quota_req["project"] = serde_json::Value::String(pid.clone());
    }
    let resp = match client
        .post("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota")
        .header("Authorization", format!("Bearer {access_token}"))
        .json(&quota_req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => return network_error("gemini", err),
    };
    let body = match read_json_or_error("gemini", resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    SubscriptionQuota::success("gemini", parse_gemini_quota(&body), None)
}

// ===================================================================
// Entry point
// ===================================================================

/// Fetch the official-account quota for one CLI (see `SUPPORTED`).
pub async fn fetch_quota(app: CliApp) -> SubscriptionQuota {
    match app {
        CliApp::Claude => {
            let (token, status, message) = read_claude_credentials();
            quota_for_credential("claude", token, status, message, |t| async move {
                query_claude_quota(&t).await
            })
            .await
        }
        CliApp::Codex => {
            let (token, account_id, status, message) = read_codex_credentials();
            quota_for_credential("codex", token, status, message, move |t| async move {
                query_codex_quota(&t, account_id.as_deref()).await
            })
            .await
        }
        CliApp::Gemini => {
            let (mut token, refresh_token, mut status, message) = read_gemini_credentials();
            // Google access tokens last ~1h — when stale, mint a fresh
            // one from the refresh token before the shared scaffold
            // runs (used in-memory only; Termory never writes
            // credentials).
            if status == CredentialStatus::Expired {
                if let Some(rt) = refresh_token.as_deref() {
                    if let Some(fresh) = refresh_gemini_token(rt).await {
                        token = Some(fresh);
                        status = CredentialStatus::Valid;
                    }
                }
            }
            quota_for_credential("gemini", token, status, message, |t| async move {
                query_gemini_quota(&t).await
            })
            .await
        }
        // OpenCode has no official subscription quota. bin_name
        // doubles as the frontend CliApp key.
        CliApp::Opencode => SubscriptionQuota::not_found(app.bin_name()),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn far_future_ms() -> i64 {
        (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64)
            + 3_600_000
    }

    #[test]
    fn parse_claude_credentials_valid_token() {
        let content = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-abc",
                "expiresAt": far_future_ms()
            }
        })
        .to_string();
        let (token, status, message) = parse_claude_credentials(&content);
        assert_eq!(token.as_deref(), Some("sk-ant-oat01-abc"));
        assert_eq!(status, CredentialStatus::Valid);
        assert!(message.is_none());
    }

    #[test]
    fn parse_claude_credentials_accepts_legacy_key() {
        let content = json!({
            "claude.ai_oauth": { "accessToken": "tok" }
        })
        .to_string();
        let (token, status, _) = parse_claude_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(status, CredentialStatus::Valid);
    }

    #[test]
    fn parse_claude_credentials_expired_keeps_token() {
        // Expired tokens are still returned — fetch_quota tries the
        // API anyway and only reports Expired when it rejects.
        let content = json!({
            "claudeAiOauth": {
                "accessToken": "tok",
                "expiresAt": 1_700_000_000_000_i64 // 2023, millis
            }
        })
        .to_string();
        let (token, status, _) = parse_claude_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(status, CredentialStatus::Expired);
    }

    #[test]
    fn parse_claude_credentials_missing_entry_is_parse_error() {
        let (token, status, _) = parse_claude_credentials("{}");
        assert!(token.is_none());
        assert_eq!(status, CredentialStatus::ParseError);
    }

    #[test]
    fn parse_claude_credentials_empty_token_is_parse_error() {
        let content = json!({ "claudeAiOauth": { "accessToken": "" } }).to_string();
        let (token, status, _) = parse_claude_credentials(&content);
        assert!(token.is_none());
        assert_eq!(status, CredentialStatus::ParseError);
    }

    #[test]
    fn token_expiry_handles_seconds_millis_and_iso() {
        // seconds-scale, past
        assert!(is_token_expired(&json!(1_700_000_000_u64)));
        // millis-scale, past
        assert!(is_token_expired(&json!(1_700_000_000_000_u64)));
        // millis-scale, future
        assert!(!is_token_expired(&json!(far_future_ms())));
        // ISO past / future
        assert!(is_token_expired(&json!("2023-01-01T00:00:00Z")));
        assert!(!is_token_expired(&json!("2099-01-01T00:00:00Z")));
        // unparseable → not expired
        assert!(!is_token_expired(&json!("soonish")));
    }

    #[test]
    fn parse_claude_usage_extracts_known_tiers_in_order() {
        let body = json!({
            "seven_day": { "utilization": 41.0, "resets_at": "2026-06-15T07:00:00Z" },
            "five_hour": { "utilization": 12.5, "resets_at": "2026-06-10T12:00:00Z" },
            "seven_day_opus": { "utilization": 3.0, "resets_at": null }
        });
        let (tiers, extra) = parse_claude_usage(&body);
        let names: Vec<&str> = tiers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["five_hour", "seven_day", "seven_day_opus"]);
        assert_eq!(tiers[0].utilization, 12.5);
        assert_eq!(tiers[0].resets_at.as_deref(), Some("2026-06-10T12:00:00Z"));
        assert!(tiers[2].resets_at.is_none());
        assert!(extra.is_none());
    }

    #[test]
    fn parse_claude_usage_passes_unknown_windows_through() {
        let body = json!({
            "five_hour": { "utilization": 1.0 },
            "thirty_day": { "utilization": 9.0, "resets_at": "2026-07-01T00:00:00Z" }
        });
        let (tiers, _) = parse_claude_usage(&body);
        let names: Vec<&str> = tiers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["five_hour", "thirty_day"]);
    }

    #[test]
    fn parse_claude_usage_skips_windows_without_utilization() {
        let body = json!({
            "five_hour": { "resets_at": "2026-06-10T12:00:00Z" },
            "seven_day": { "utilization": 2.0 }
        });
        let (tiers, _) = parse_claude_usage(&body);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].name, "seven_day");
    }

    #[test]
    fn parse_claude_usage_reads_extra_usage() {
        let body = json!({
            "five_hour": { "utilization": 50.0 },
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 100.0,
                "used_credits": 12.34,
                "utilization": 12.34,
                "currency": "USD"
            }
        });
        let (_, extra) = parse_claude_usage(&body);
        let extra = extra.expect("extra_usage parsed");
        assert!(extra.is_enabled);
        assert_eq!(extra.monthly_limit, Some(100.0));
        assert_eq!(extra.used_credits, Some(12.34));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn parse_codex_credentials_chatgpt_mode_yields_token_and_account() {
        let content = json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "eyJtok", "account_id": "acc-1" },
            "last_refresh": chrono::Utc::now().to_rfc3339()
        })
        .to_string();
        let (token, account, status, message) = parse_codex_credentials(&content);
        assert_eq!(token.as_deref(), Some("eyJtok"));
        assert_eq!(account.as_deref(), Some("acc-1"));
        assert_eq!(status, CredentialStatus::Valid);
        assert!(message.is_none());
    }

    #[test]
    fn parse_codex_credentials_apikey_mode_is_not_found() {
        // API-key login has no subscription windows — treated like
        // "no OAuth login" so the UI shows nothing.
        let content = json!({
            "auth_mode": "apikey",
            "OPENAI_API_KEY": "sk-..."
        })
        .to_string();
        let (token, _, status, _) = parse_codex_credentials(&content);
        assert!(token.is_none());
        assert_eq!(status, CredentialStatus::NotFound);
    }

    #[test]
    fn parse_codex_credentials_stale_refresh_keeps_token() {
        let content = json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "tok", "account_id": "acc" },
            "last_refresh": "2020-01-01T00:00:00Z" // way past 8 days
        })
        .to_string();
        let (token, account, status, _) = parse_codex_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(account.as_deref(), Some("acc"));
        assert_eq!(status, CredentialStatus::Expired);
    }

    #[test]
    fn window_seconds_map_to_claude_compatible_tier_names() {
        assert_eq!(window_seconds_to_tier_name(18_000), "five_hour");
        assert_eq!(window_seconds_to_tier_name(604_800), "seven_day");
        assert_eq!(window_seconds_to_tier_name(3 * 3600), "3_hour");
        assert_eq!(window_seconds_to_tier_name(30 * 24 * 3600), "30_day");
    }

    #[test]
    fn parse_codex_usage_maps_primary_and_secondary_windows() {
        // Shape per codex-rs RateLimitStatusPayload / RateLimitWindowSnapshot.
        let body = json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 42.0,
                    "limit_window_seconds": 18_000,
                    "reset_after_seconds": 120,
                    "reset_at": 1_780_000_000_i64
                },
                "secondary_window": {
                    "used_percent": 84,
                    "limit_window_seconds": 604_800,
                    "reset_after_seconds": 0,
                    "reset_at": 1_780_600_000_i64
                }
            }
        });
        let tiers = parse_codex_usage(&body);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].name, "five_hour");
        assert_eq!(tiers[0].utilization, 42.0);
        assert_eq!(
            tiers[0].resets_at.as_deref(),
            unix_ts_to_iso(1_780_000_000).as_deref()
        );
        assert_eq!(tiers[1].name, "seven_day");
        assert_eq!(tiers[1].utilization, 84.0);
    }

    #[test]
    fn parse_codex_usage_skips_missing_windows() {
        let body = json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 10.0,
                    "limit_window_seconds": 18_000
                }
            }
        });
        let tiers = parse_codex_usage(&body);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].name, "five_hour");
        assert!(tiers[0].resets_at.is_none());

        assert!(parse_codex_usage(&json!({})).is_empty());
    }

    #[test]
    fn api_error_detail_reads_chatgpt_detail_field() {
        // ChatGPT backend errors use {"detail": "..."} instead of the
        // Anthropic envelope.
        let status = reqwest::StatusCode::UNAUTHORIZED;
        assert_eq!(
            http_error_message(
                status,
                r#"{"detail": "Could not parse your authentication token"}"#
            ),
            "HTTP 401 Unauthorized: Could not parse your authentication token"
        );
    }

    #[test]
    fn http_error_message_extracts_the_api_message() {
        let status = reqwest::StatusCode::TOO_MANY_REQUESTS;
        // Anthropic error envelope → just the message.
        let body = r#"{ "error": { "type": "rate_limit_error", "message": "Rate limited. Please try again later." } }"#;
        assert_eq!(
            http_error_message(status, body),
            "HTTP 429 Too Many Requests: Rate limited. Please try again later."
        );
        // Top-level `message` is accepted too.
        assert_eq!(
            http_error_message(status, r#"{"message": "slow down"}"#),
            "HTTP 429 Too Many Requests: slow down"
        );
        // Non-JSON body falls back to the raw text.
        assert_eq!(
            http_error_message(status, "service unavailable"),
            "HTTP 429 Too Many Requests: service unavailable"
        );
        // Empty body → status only.
        assert_eq!(http_error_message(status, ""), "HTTP 429 Too Many Requests");
    }

    #[test]
    fn api_error_detail_truncates_long_raw_bodies() {
        let long = "x".repeat(300);
        let out = api_error_detail(&long).unwrap();
        assert_eq!(out.chars().count(), 201);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn quota_serializes_camel_case() {
        let quota = SubscriptionQuota {
            app: "claude".into(),
            credential_status: CredentialStatus::Valid,
            success: true,
            tiers: vec![QuotaTier {
                name: "five_hour".into(),
                utilization: 12.5,
                resets_at: Some("2026-06-10T12:00:00Z".into()),
            }],
            extra_usage: None,
            error: None,
            queried_at: Some(1),
        };
        let v = serde_json::to_value(&quota).unwrap();
        assert_eq!(v["credentialStatus"], "valid");
        assert_eq!(v["tiers"][0]["resetsAt"], "2026-06-10T12:00:00Z");
        assert_eq!(v["queriedAt"], 1);
    }

    #[test]
    fn parse_gemini_file_json_valid_and_expired() {
        let valid = json!({
            "access_token": "ya29.tok",
            "refresh_token": "1//refresh",
            "expiry_date": far_future_ms()
        })
        .to_string();
        let (token, refresh, status, _) = parse_gemini_file_json(&valid);
        assert_eq!(token.as_deref(), Some("ya29.tok"));
        assert_eq!(refresh.as_deref(), Some("1//refresh"));
        assert_eq!(status, CredentialStatus::Valid);

        // Expired keeps BOTH tokens — fetch_quota refreshes with the
        // refresh token, else still tries the stale access token.
        let expired = json!({
            "access_token": "ya29.old",
            "refresh_token": "1//refresh",
            "expiry_date": 1_700_000_000_000_i64
        })
        .to_string();
        let (token, refresh, status, _) = parse_gemini_file_json(&expired);
        assert_eq!(token.as_deref(), Some("ya29.old"));
        assert_eq!(refresh.as_deref(), Some("1//refresh"));
        assert_eq!(status, CredentialStatus::Expired);

        // Missing access token → ParseError, refresh still surfaced.
        let no_token = json!({ "refresh_token": "1//r" }).to_string();
        let (token, refresh, status, _) = parse_gemini_file_json(&no_token);
        assert!(token.is_none());
        assert_eq!(refresh.as_deref(), Some("1//r"));
        assert_eq!(status, CredentialStatus::ParseError);
    }

    #[test]
    fn parse_gemini_keychain_json_nested_and_flat() {
        // keytar document (nested under `token`, camelCase, ms expiry).
        let nested = json!({
            "token": {
                "accessToken": "ya29.kc",
                "refreshToken": "1//kc",
                "expiresAt": far_future_ms()
            },
            "updatedAt": 1
        })
        .to_string();
        let (token, refresh, status, _) = parse_gemini_keychain_json(&nested);
        assert_eq!(token.as_deref(), Some("ya29.kc"));
        assert_eq!(refresh.as_deref(), Some("1//kc"));
        assert_eq!(status, CredentialStatus::Valid);

        // A flat document falls through to the file-format parser.
        let flat = json!({
            "access_token": "ya29.flat",
            "refresh_token": "1//flat",
            "expiry_date": far_future_ms()
        })
        .to_string();
        let (token, _, status, _) = parse_gemini_keychain_json(&flat);
        assert_eq!(token.as_deref(), Some("ya29.flat"));
        assert_eq!(status, CredentialStatus::Valid);
    }

    #[test]
    fn classify_gemini_model_groups_by_class() {
        assert_eq!(classify_gemini_model("gemini-2.5-pro"), "gemini_pro");
        assert_eq!(classify_gemini_model("gemini-2.5-flash"), "gemini_flash");
        assert_eq!(
            classify_gemini_model("gemini-2.5-flash-lite"),
            "gemini_flash_lite"
        );
        // Unknown model ids pass through raw.
        assert_eq!(classify_gemini_model("imagen-3"), "imagen-3");
    }

    #[test]
    fn parse_gemini_quota_keeps_lowest_remaining_per_class_and_sorts() {
        let body = json!({
            "buckets": [
                { "modelId": "gemini-2.5-flash", "remainingFraction": 0.9,
                  "resetTime": "2026-06-12T00:00:00Z" },
                { "modelId": "gemini-2.5-pro", "remainingFraction": 0.4,
                  "resetTime": "2026-06-12T00:00:00Z" },
                // Second pro bucket with LESS remaining — it wins.
                { "modelId": "gemini-2.5-pro-preview", "remainingFraction": 0.25,
                  "resetTime": "2026-06-13T00:00:00Z" }
            ]
        });
        let tiers = parse_gemini_quota(&body);
        let names: Vec<&str> = tiers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["gemini_pro", "gemini_flash"]);
        assert_eq!(tiers[0].utilization, 75.0); // 1 - 0.25
        assert_eq!(tiers[0].resets_at.as_deref(), Some("2026-06-13T00:00:00Z"));
        assert_eq!(tiers[1].utilization.round(), 10.0); // 1 - 0.9, fp-rounded

        assert!(parse_gemini_quota(&json!({})).is_empty());
    }

    #[test]
    fn extract_project_id_handles_string_and_object() {
        assert_eq!(
            extract_project_id(&json!("proj-1")).as_deref(),
            Some("proj-1")
        );
        assert_eq!(
            extract_project_id(&json!({"id": "proj-2"})).as_deref(),
            Some("proj-2")
        );
        assert_eq!(
            extract_project_id(&json!({"projectId": "proj-3"})).as_deref(),
            Some("proj-3")
        );
        assert!(extract_project_id(&json!(42)).is_none());
    }

    #[test]
    fn credential_cli_for_path_maps_all_three_clis() {
        use std::path::Path;
        assert_eq!(
            credential_cli_for_path(Path::new("/u/x/.claude/.credentials.json")),
            Some(CliApp::Claude)
        );
        assert_eq!(
            credential_cli_for_path(Path::new("/u/x/.codex/auth.json")),
            Some(CliApp::Codex)
        );
        assert_eq!(
            credential_cli_for_path(Path::new("/u/x/.gemini/oauth_creds.json")),
            Some(CliApp::Gemini)
        );
        // OpenCode's unrelated auth.json must NOT match.
        assert_eq!(
            credential_cli_for_path(Path::new("/u/x/.local/share/opencode/auth.json")),
            None
        );
        assert_eq!(
            credential_cli_for_path(Path::new("/u/x/.claude/projects/p/s.jsonl")),
            None
        );
    }

    /// The scaffold's four credential-status branches, exercised with
    /// stub queries (no network).
    #[test]
    fn quota_for_credential_scaffold_branches() {
        fn ok(app: &'static str) -> SubscriptionQuota {
            SubscriptionQuota::success(app, vec![], None)
        }
        fn fail(app: &'static str) -> SubscriptionQuota {
            SubscriptionQuota::error(app, CredentialStatus::Valid, "boom".into())
        }
        // NotFound short-circuits — the query must not run.
        let r = tauri::async_runtime::block_on(quota_for_credential(
            "claude",
            None,
            CredentialStatus::NotFound,
            None,
            |_t| async { panic!("query must not run for NotFound") },
        ));
        assert!(!r.success);
        assert_eq!(r.credential_status, CredentialStatus::NotFound);

        // ParseError carries the parser's message through.
        let r = tauri::async_runtime::block_on(quota_for_credential(
            "claude",
            None,
            CredentialStatus::ParseError,
            Some("bad json".into()),
            |_t| async { panic!("query must not run for ParseError") },
        ));
        assert_eq!(r.credential_status, CredentialStatus::ParseError);
        assert_eq!(r.error.as_deref(), Some("bad json"));

        // Expired still TRIES the query; a success wins outright.
        let r = tauri::async_runtime::block_on(quota_for_credential(
            "claude",
            Some("stale-tok".into()),
            CredentialStatus::Expired,
            Some("token expired".into()),
            |t| async move {
                assert_eq!(t, "stale-tok");
                ok("claude")
            },
        ));
        assert!(r.success);

        // Expired + query rejected → Expired error with the ORIGINAL
        // parser message (not the query's).
        let r = tauri::async_runtime::block_on(quota_for_credential(
            "claude",
            Some("stale-tok".into()),
            CredentialStatus::Expired,
            Some("token expired".into()),
            |_t| async { fail("claude") },
        ));
        assert!(!r.success);
        assert_eq!(r.credential_status, CredentialStatus::Expired);
        assert_eq!(r.error.as_deref(), Some("token expired"));

        // Valid queries directly.
        let r = tauri::async_runtime::block_on(quota_for_credential(
            "claude",
            Some("tok".into()),
            CredentialStatus::Valid,
            None,
            |t| async move {
                assert_eq!(t, "tok");
                ok("claude")
            },
        ));
        assert!(r.success);
    }
}
