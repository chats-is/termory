//! Official subscription quota — reads the CLI's existing OAuth
//! credentials (never writes them) and queries the official usage
//! endpoint for the rate-limit windows (5-hour session, 7-day weekly,
//! per-model weekly, …).
//!
//! Claude Code is implemented; the entry point is keyed by `CliApp`
//! so Codex / Gemini can be added later behind the same IPC shape.
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
pub const SUPPORTED: &[CliApp] = &[CliApp::Claude];

pub fn supports_quota(app: CliApp) -> bool {
    SUPPORTED.contains(&app)
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
/// body is empty / has no usable text.
fn api_error_detail(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = v
            .pointer("/error/message")
            .or_else(|| v.get("message"))
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

/// Query the official usage endpoint with the OAuth access token.
/// Endpoint + `anthropic-beta` header per cc-switch
/// `subscription.rs:321-323` (the same call Claude Code's `/usage`
/// command makes).
async fn query_claude_quota(access_token: &str) -> SubscriptionQuota {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            return SubscriptionQuota::error(
                "claude",
                CredentialStatus::Valid,
                format!("HTTP client init failed: {err}"),
            );
        }
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
        Err(err) => {
            return SubscriptionQuota::error(
                "claude",
                CredentialStatus::Valid,
                format!("Network error: {err}"),
            );
        }
    };

    let status = resp.status();
    if !status.is_success() {
        // Surface the API's own error `message` (not the raw JSON
        // envelope) next to the status — that's what the card shows.
        let body = resp.text().await.unwrap_or_default();
        let cred = if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            CredentialStatus::Expired
        } else {
            CredentialStatus::Valid
        };
        return SubscriptionQuota::error("claude", cred, http_error_message(status, &body));
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(err) => {
            return SubscriptionQuota::error(
                "claude",
                CredentialStatus::Valid,
                format!("Failed to parse API response: {err}"),
            );
        }
    };

    let (tiers, extra_usage) = parse_claude_usage(&body);
    SubscriptionQuota {
        app: "claude".to_string(),
        credential_status: CredentialStatus::Valid,
        success: true,
        tiers,
        extra_usage,
        error: None,
        queried_at: Some(now_millis()),
    }
}

// ===================================================================
// Entry point
// ===================================================================

/// Fetch the official-account quota for one CLI. Claude only for now;
/// the others report `not_found` so the frontend shows nothing.
pub async fn fetch_quota(app: CliApp) -> SubscriptionQuota {
    match app {
        CliApp::Claude => {
            let (token, status, message) = read_claude_credentials();
            match status {
                CredentialStatus::NotFound => SubscriptionQuota::not_found("claude"),
                CredentialStatus::ParseError => SubscriptionQuota::error(
                    "claude",
                    CredentialStatus::ParseError,
                    message.unwrap_or_else(|| "Failed to parse credentials".to_string()),
                ),
                CredentialStatus::Expired => {
                    // The file timestamp can lag a Keychain refresh —
                    // still try the API; only report Expired when it
                    // actually rejects the token.
                    if let Some(token) = token {
                        let result = query_claude_quota(&token).await;
                        if result.success {
                            return result;
                        }
                    }
                    SubscriptionQuota::error(
                        "claude",
                        CredentialStatus::Expired,
                        message.unwrap_or_else(|| "OAuth token has expired".to_string()),
                    )
                }
                CredentialStatus::Valid => {
                    let token = token.expect("token present when status is Valid");
                    query_claude_quota(&token).await
                }
            }
        }
        // bin_name doubles as the frontend CliApp key ("codex", …).
        _ => SubscriptionQuota::not_found(app.bin_name()),
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
}
