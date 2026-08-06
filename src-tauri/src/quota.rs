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
pub const SUPPORTED: &[CliApp] = &[CliApp::Claude, CliApp::Codex, CliApp::Gemini, CliApp::Grok];

pub fn supports_quota(app: CliApp) -> bool {
    SUPPORTED.contains(&app)
}

/// Emitted with a `SubscriptionQuota` payload after every completed
/// quota fetch (any trigger), so the Providers page stays in sync with
/// backend-initiated fetches. Lives here (quota state, not tray UI);
/// the emit happens in tray::refresh_quota, the central result sink.
/// Frontend mirror: QUOTA_CHANGED_EVENT in src/constants.ts.
pub const QUOTA_CHANGED_EVENT: &str = "termory:quota-changed";

/// Emitted with a CLI key payload (`"codex"`, …) when that CLI's cached
/// quota was dropped because the LOGIN behind it changed — not because a
/// fetch reported something. Lets an open Providers page discard the
/// previous account's numbers on a switch made from the menu-bar tray,
/// where the page never learns of the switch otherwise (its own switch
/// path clears the entry directly). Emitted from tray::invalidate_quota;
/// frontend mirror: QUOTA_INVALIDATED_EVENT in src/constants.ts.
pub const QUOTA_INVALIDATED_EVENT: &str = "termory:quota-invalidated";

/// Claude's credential is in the macOS **Keychain** and emits no
/// filesystem event of its own — but `<config-dir>.lock` does, and it
/// means exactly one thing: the OAuth token was refreshed.
///
/// `proper-lockfile` names a lock `` `${file}.lock` `` and creates it with
/// `mkdir` (lib/lockfile.js:11,:29), and Claude locks the CONFIG DIR
/// ITSELF only in `checkAndRefreshOAuthTokenIfNeededImpl` (auth.ts:1485-
/// 1491). Every other `lockfile.lock` call in its source locks some other
/// path — a mailbox, a marker, a task — so this name is an exclusive
/// signal, and a plan change rides the same refresh.
///
/// A credential change is what the quota cares about too, which is why
/// this one is routed through `credential_cli_for_path` while the login
/// signal below is NOT.
pub fn claude_credential_signal_path() -> Option<PathBuf> {
    let dir = crate::claude_auth::config_dir()?;
    // Sibling of the config dir, not a child — `~/.claude` → `~/.claude.lock`.
    let mut lock = dir.into_os_string();
    lock.push(".lock");
    Some(PathBuf::from(lock))
}

/// `.claude.json` — the only file a LOGIN touches (`storeOAuthAccountInfo`,
/// cli/handlers/auth.ts:58/72; the tokens themselves go to the Keychain),
/// and the same file a logout clears.
///
/// **Deliberately NOT part of `credential_cli_for_path`.** This is Claude's
/// whole global config, written from 159 places in its source — startup
/// counters, changelog fetch times, skill-usage tracking. Routing it there
/// would hand every one of those writes to `force_quota_refresh`, which
/// bypasses the normal two-minute floor, and turn a feature documented as
/// having "THREE triggers (NO periodic polling)" into an Anthropic API call
/// every ten seconds while Claude is in use. The account sync consumes it
/// alone: a pass that finds nothing costs one credential read and writes
/// nothing, and the watcher's debounce collapses a flurry of config writes
/// into a single one.
pub fn claude_identity_signal_path() -> Option<PathBuf> {
    crate::accounts::claude_json_path().ok()
}

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
    // A relocated `CODEX_HOME` puts auth.json outside any `.codex`-named
    // dir, so the basename check above misses it — match the resolved
    // override dir directly. (Default / unset → CODEX_HOME absent, and
    // `parent_is(".codex")` already covers `~/.codex/auth.json`.)
    let parent_is_codex_home = || match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => path.parent() == Some(std::path::Path::new(&v)),
        _ => false,
    };
    // A relocated `GROK_HOME` — same story as CODEX_HOME above.
    let parent_is_grok_home = || match std::env::var_os("GROK_HOME") {
        Some(v) if !v.is_empty() => path.parent() == Some(std::path::Path::new(&v)),
        _ => false,
    };
    match path.file_name().and_then(|n| n.to_str())? {
        ".credentials.json" => Some(CliApp::Claude),
        // The parent check excludes OpenCode's unrelated
        // ~/.local/share/opencode/auth.json.
        "auth.json" if parent_is(".codex") || parent_is_codex_home() => Some(CliApp::Codex),
        "auth.json" if parent_is(".grok") || parent_is_grok_home() => Some(CliApp::Grok),
        "oauth_creds.json" if parent_is(".gemini") => Some(CliApp::Gemini),
        // Matched by FULL path, not basename: `.claude.lock` is an
        // ordinary-looking name and one anywhere else is not Claude's.
        _ if claude_credential_signal_path().as_deref() == Some(path) => Some(CliApp::Claude),
        _ => None,
    }
}

/// Pressure thresholds (used %) for the tray glyph color. Frontend
/// mirror: `QUOTA_WARN_PCT` / `QUOTA_CRIT_PCT` in src/lib/quota-utils.ts
/// (the in-app ring) — keep the two in sync.
pub const WARN_PCT: f64 = 75.0;
pub const CRIT_PCT: f64 = 90.0;

/// Tidy a raw plan identifier for display: strip Gemini's "-tier"
/// suffix and uppercase the first letter ("free" → "Free", "max" →
/// "Max", "standard-tier" → "Standard"). Unknown values pass through
/// tidied; empty input yields None.
fn display_plan(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches("-tier").trim();
    let mut chars = raw.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().collect::<String>() + chars.as_str())
}

/// Claude's plan label, with the Max multiplier folded in: `"Max 20x"`.
///
/// `subscriptionType` alone cannot tell Max 5x from Max 20x — both are
/// `"max"` — and the difference is the whole shape of the account's limits.
/// The multiplier lives beside it in the same credential block as
/// `rateLimitTier` (auth.ts:1227), written by the same profile fetch.
///
/// **Only Max takes a multiplier.** Claude Code itself never reads the tier
/// on its own: every use is `isMax && rateLimitTier == …` (upgrade.tsx:25,
/// rate-limit-options.tsx:52, planModeV2.ts:18). The reason is visible in
/// `isTeamPremiumSubscriber` (auth.ts:1687), where `default_claude_max_5x`
/// appears under a **team** subscription — so the tier string does not name
/// a Max level on its own, and appending it to any other plan would invent
/// a "Team 5x" that means something else entirely.
///
/// Unrecognized or absent tiers fall back to the bare plan rather than
/// surfacing a raw identifier: the value set is not closed (only these two
/// appear anywhere in Claude's source), so a new one must degrade to
/// today's correct-but-less-specific label, never to `Max default_claude_…`.
pub(crate) fn claude_plan_label(
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Option<String> {
    let raw = subscription_type?;
    let plan = display_plan(raw)?;
    if !raw.trim().eq_ignore_ascii_case("max") {
        return Some(plan);
    }
    let multiplier = match rate_limit_tier.map(str::trim) {
        Some("default_claude_max_20x") => "20x",
        Some("default_claude_max_5x") => "5x",
        _ => return Some(plan),
    };
    Some(format!("{plan} {multiplier}"))
}

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
    /// For a MODEL-SCOPED window this is the model's display name
    /// ("Fable") — see `group` for the period it belongs to.
    pub name: String,
    /// The PERIOD a model-scoped window covers, taken VERBATIM from the
    /// API's own grouping (`limits[].group`: `session` / `weekly` /
    /// `monthly`). Present only for model-scoped windows, whose `name`
    /// is a bare model name that says nothing about the period — the
    /// label renders `Weekly · Fable` from the two. `None` for the flat
    /// account-wide windows, whose `name` already IS the period, and
    /// for sources with no such grouping (Codex / Gemini / grok).
    ///
    /// Read from the API instead of inferred because this data is
    /// DYNAMIC: which models have their own window, and which periods
    /// exist, differ per account and change without notice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Decimal places of `monthly_limit` / `used_credits` — Claude
    /// reports these in MINOR units (cents), so the frontend divides by
    /// `10^decimal_places` to render currency (`1944` + `2` → `$19.44`).
    /// Absent (grok, which already stores major units) → treat as 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimal_places: Option<u32>,
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
    /// Display name of the account's subscription plan ("Max", "Pro",
    /// "Plus", "Free", …) — per-CLI source documented at each
    /// extraction site. Brand-ish raw value, not translated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_usage: Option<ExtraUsage>,
    /// Remaining PREPAID ("bought") credit balance in major currency units
    /// — grok only, from `config.prepaidBalance` (billing.rs:94).
    ///
    /// A BALANCE, not usage: unlike `extra_usage` there is no limit to
    /// divide by, so it carries no utilization and renders as a bare
    /// amount. grok's two billing models are mutually exclusive
    /// (`credit_limit_upsell_mode`, dispatch/billing.rs:54) — unified users
    /// buy prepaid credits, legacy users get an on-demand cap — so a
    /// unified subscriber has `onDemandCap: 0` and would show NOTHING
    /// without this field, which is exactly what the on-demand-only
    /// `extra_usage` mapping did. Drawn down only once the included
    /// allowance hits 100% (credit_bar.rs:219), i.e. a reserve tank.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepaid_balance: Option<f64>,
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
            plan: None,
            extra_usage: None,
            prepaid_balance: None,
            error: None,
            queried_at: Some(now_millis()),
        }
    }

    fn success(
        app: &str,
        tiers: Vec<QuotaTier>,
        plan: Option<String>,
        extra_usage: Option<ExtraUsage>,
    ) -> Self {
        Self {
            app: app.to_string(),
            credential_status: CredentialStatus::Valid,
            success: true,
            tiers,
            plan,
            extra_usage,
            prepaid_balance: None,
            error: None,
            queried_at: Some(now_millis()),
        }
    }

    /// grok-only rider on `success` — every other CLI leaves it `None`, so
    /// it stays off the shared constructor's signature.
    fn with_prepaid_balance(mut self, balance: Option<f64>) -> Self {
        self.prepaid_balance = balance;
        self
    }

    fn error(app: &str, status: CredentialStatus, message: String) -> Self {
        Self {
            app: app.to_string(),
            credential_status: status,
            success: false,
            tiers: vec![],
            plan: None,
            extra_usage: None,
            prepaid_balance: None,
            error: Some(message),
            queried_at: Some(now_millis()),
        }
    }
}

/// The result to hand back for a CLI whose credential is mid-login.
///
/// **Deliberately a plain failure, NOT `not_found`.** While an add-account
/// flow runs, the CLI's credential on disk is not the user's account (the
/// codex flow blanks `auth.json`, claude logs out locally, grok overwrites
/// its scope entry), so a real fetch reports "logged out" — and both the
/// tray and `mergeQuotaResult` (quota-utils.ts:64) treat that as
/// DEFINITIVE and clear the display, taking the whole quota section and
/// its refresh button with it. A plain failure takes the other branch in
/// both places: keep the last good numbers. That is the honest answer
/// here, because the user is adding a SECOND account and the one those
/// numbers describe is still logged in — and every flow ends by restoring
/// it (a cancel rolls back; a success saves the new account to the store
/// and switches the live login back).
///
/// `Valid` because the stored credential IS valid; what is momentarily
/// unavailable is our ability to read it, which `error` conveys.
pub fn quota_during_login(cli: CliApp) -> SubscriptionQuota {
    SubscriptionQuota::error(
        cli.bin_name(),
        CredentialStatus::Valid,
        "an add-account flow is using this CLI's credential".to_string(),
    )
}

// ===================================================================
// Claude credentials
// ===================================================================

/// Parsed credential: token (may be present even when expired — the
/// caller still tries it) + subscription plan + status + diagnostic
/// message.
type Credential = (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
);

/// Read the Claude Code OAuth credential through the shared storage layer
/// (`claude_auth::read_credentials` — Keychain-first on macOS with the
/// official service-name derivation incl. the `CLAUDE_CONFIG_DIR` hash
/// suffix, `-a` account arg, and a timeout so a locked Keychain can't hang
/// the caller; then `.credentials.json`). Replaces an earlier local pair
/// that hardcoded the service name and spawned `security` untimed.
///
/// `None` from the store (no entry / unreadable / corrupt) maps to
/// `NotFound` — the card hides, matching a logged-out state.
fn read_claude_credentials() -> Credential {
    match crate::claude_auth::read_credentials() {
        Some(doc) => parse_claude_credentials(&doc.to_string()),
        None => (None, None, CredentialStatus::NotFound, None),
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
            None,
            CredentialStatus::ParseError,
            Some("No OAuth entry found in credentials".to_string()),
        );
    };

    // Subscription plan, stored at login by Claude Code itself
    // (auth.ts:1225 `subscriptionType` — "free" / "pro" / "max"), plus the
    // Max multiplier from its neighbour `rateLimitTier` (auth.ts:1227).
    let field = |k: &str| entry.get(k).and_then(|v| v.as_str());
    let plan = claude_plan_label(field("subscriptionType"), field("rateLimitTier"));

    let access_token = match entry.get("accessToken").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return (
                None,
                plan,
                CredentialStatus::ParseError,
                Some("accessToken is empty or missing".to_string()),
            );
        }
    };

    if let Some(expires_at) = entry.get("expiresAt") {
        if is_token_expired(expires_at) {
            return (
                Some(access_token),
                plan,
                CredentialStatus::Expired,
                Some("OAuth token has expired".to_string()),
            );
        }
    }

    (Some(access_token), plan, CredentialStatus::Valid, None)
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
            // Flat top-level windows ARE their own period.
            group: None,
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
            // `limits` is the new structured array (handled below); the
            // reserved keys and the known tiers are already covered.
            if key == "extra_usage" || key == "limits" || CLAUDE_KNOWN_TIERS.contains(&key.as_str())
            {
                continue;
            }
            if let Some(tier) = window_tier(key, value) {
                tiers.push(tier);
            }
        }
    }

    // Newer API shape: a `limits` array carries model-SCOPED weekly
    // windows (e.g. Fable) that the flat top-level fields don't — the
    // top-level `seven_day_opus`/`seven_day_sonnet` are now null. Each
    // `weekly_scoped` entry names its model in `scope.model.display_name`
    // (a brand name, so it becomes the tier `name` verbatim). Any model
    // surfaces dynamically, no per-model code. `session`/`weekly_all`
    // duplicate the flat `five_hour`/`seven_day` already added, so only
    // scoped entries are taken here.
    for entry in body
        .get("limits")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if entry.get("kind").and_then(|k| k.as_str()) != Some("weekly_scoped") {
            continue;
        }
        let Some(percent) = entry.get("percent").and_then(|p| p.as_f64()) else {
            continue;
        };
        let Some(model) = entry
            .pointer("/scope/model/display_name")
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        // De-dup by model name in case the array repeats one.
        if tiers.iter().any(|tier| tier.name == model) {
            continue;
        }
        tiers.push(QuotaTier {
            name: model.to_string(),
            // The period this scoped window belongs to, verbatim from
            // the API ("weekly" for `weekly_scoped`) so the label can
            // read "Weekly · Fable" rather than a bare model name.
            // Absent (or empty) stays None — the period is never
            // inferred from `kind` or from the model, so a shape we
            // don't recognize degrades to today's bare-name label
            // instead of inventing a period.
            group: entry
                .get("group")
                .and_then(|g| g.as_str())
                .filter(|g| !g.is_empty())
                .map(String::from),
            utilization: percent,
            resets_at: entry
                .get("resets_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
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
        decimal_places: v
            .get("decimal_places")
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
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

/// Map a non-2xx quota response to a result:
/// - **410 Gone → NotFound.** The usage endpoint no longer serves this
///   account (e.g. Gemini CLI stopped serving individual/free logins on
///   2026-06-18). Treated like a logout: the card hides cleanly instead
///   of surfacing a confusing error toast on manual refresh. Enterprise
///   logins keep serving quota, so they are unaffected.
/// - **401/403 → error with an Expired credential** (re-login prompt).
/// - **everything else → error** carrying the API's own message text.
fn quota_error_for_status(
    app_key: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> SubscriptionQuota {
    if status == reqwest::StatusCode::GONE {
        return SubscriptionQuota::not_found(app_key);
    }
    let cred = if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        CredentialStatus::Expired
    } else {
        CredentialStatus::Valid
    };
    SubscriptionQuota::error(app_key, cred, http_error_message(status, body))
}

/// Shared response handling: non-2xx → error result (see
/// `quota_error_for_status`), 2xx → parsed JSON body.
async fn read_json_or_error(
    app_key: &str,
    resp: reqwest::Response,
) -> Result<serde_json::Value, SubscriptionQuota> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(quota_error_for_status(app_key, status, &body));
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
async fn query_claude_quota(access_token: &str, plan: Option<String>) -> SubscriptionQuota {
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
    SubscriptionQuota::success("claude", tiers, plan, extra_usage)
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
/// Gated on the presence of OAuth `tokens` — that's what "ChatGPT
/// account is logged in" means (same semantic as Claude's credential
/// file). The `auth_mode` field is deliberately NOT consulted: it only
/// selects what the CLI uses for requests, and a Termory round-trip
/// (activate custom provider → back to Official) leaves `tokens` with
/// NO `auth_mode` key — which Codex itself resolves to ChatGPT mode
/// (`login/src/auth/manager.rs:980-988` `resolved_mode`). A pure
/// API-key login has no `tokens` → `not_found`.
fn read_codex_credentials() -> CodexCredential {
    #[cfg(target_os = "macos")]
    {
        if let Some(found) = read_codex_credentials_from_keychain() {
            return found;
        }
    }
    read_codex_credentials_from_file()
}

/// Read the Codex Keychain entry.
///
/// Goes through [`crate::process::probe`] for the TIMEOUT: a locked login
/// keychain makes `security` put up an unlock dialog and block until it is
/// answered, and this runs on a Tokio worker (the quota fetch is async), so
/// an untimed call parks that worker for as long as the dialog is ignored.
/// `claude_auth` has always been timed for this reason; the Codex and
/// Gemini readers were the two that were not.
#[cfg(target_os = "macos")]
fn read_codex_credentials_from_keychain() -> Option<CodexCredential> {
    let mut cmd = std::process::Command::new("security");
    cmd.args(["find-generic-password", "-s", "Codex Auth", "-w"]);
    let output = crate::process::probe(cmd, crate::process::PROBE_TIMEOUT)?;
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
/// `{"auth_mode"?: ..., "tokens": {"access_token": ..., "account_id": ...},
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

    // No `tokens` = no ChatGPT login (pure API-key login or logged
    // out) — nothing to query. See read_codex_credentials' doc for
    // why `auth_mode` is deliberately ignored here.
    let Some(tokens) = parsed.get("tokens").filter(|t| !t.is_null()) else {
        return (
            None,
            None,
            CredentialStatus::NotFound,
            Some("Codex has no ChatGPT login".to_string()),
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
            // Codex windows are account-wide time windows — the name IS
            // the period, so there is nothing to group under.
            group: None,
            utilization: used,
            resets_at: window
                .get("reset_at")
                .and_then(|v| v.as_i64())
                .and_then(unix_ts_to_iso),
        });
    }
    tiers
}

/// Account plan from the usage response's `plan_type`
/// (`RateLimitStatusPayload.plan_type` — KnownPlan: free / go / plus /
/// pro / business / edu / enterprise, codex-rs protocol/src/account.rs).
fn codex_plan(body: &serde_json::Value) -> Option<String> {
    body.get("plan_type")
        .and_then(|v| v.as_str())
        .and_then(display_plan)
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
    SubscriptionQuota::success("codex", parse_codex_usage(&body), codex_plan(&body), None)
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
    crate::home_dir().map(|h| h.join(".gemini").join("oauth_creds.json"))
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

/// Read the Gemini Keychain entry. Timed for the same reason as
/// [`read_codex_credentials_from_keychain`].
#[cfg(target_os = "macos")]
fn read_gemini_credentials_from_keychain() -> Option<GeminiCredential> {
    let mut cmd = std::process::Command::new("security");
    cmd.args([
        "find-generic-password",
        "-s",
        "gemini-cli-oauth",
        "-a",
        "main-account",
        "-w",
    ]);
    let output = crate::process::probe(cmd, crate::process::PROBE_TIMEOUT)?;
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

/// Account tier from the loadCodeAssist response. Mirrors the
/// official CLI exactly (`setup.ts:221`): **`paidTier` wins over
/// `currentTier`** — a Google One AI Pro account reports
/// `currentTier: standard-tier` but carries the real plan in
/// `paidTier.name`. The `name` is the FULL marketing string
/// ("Gemini Code Assist in Google One AI Pro" — what the official
/// CLI's `Tier:` row prints verbatim), far too long for a badge — so
/// a plan keyword is extracted from it ("Pro"); fall back to the id
/// ("standard-tier" → "Standard"), then to the tidied name.
fn gemini_plan(load_body: &serde_json::Value) -> Option<String> {
    let tier = load_body
        .get("paidTier")
        .filter(|t| t.is_object())
        .or_else(|| load_body.get("currentTier"))?;
    let name = tier.get("name").and_then(|v| v.as_str());
    if let Some(short) = name.and_then(gemini_tier_keyword) {
        return Some(short);
    }
    tier.get("id")
        .and_then(|v| v.as_str())
        .and_then(display_plan)
        .or_else(|| name.and_then(display_plan))
}

/// Whole-word plan keyword inside a Gemini tier marketing name.
/// Order matters only for documentation — the words are mutually
/// exclusive in real tier names.
fn gemini_tier_keyword(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    for keyword in ["ultra", "pro", "enterprise", "standard", "legacy", "free"] {
        if lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| word == keyword)
        {
            return display_plan(keyword);
        }
    }
    None
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
            // Gemini's buckets are per-MODEL CLASSES, already named as
            // such (`gemini_pro` → "Gemini Pro") and carrying no period
            // from the API — nothing to compose, so no group.
            group: None,
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
    let plan = gemini_plan(&load_body);

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
    SubscriptionQuota::success("gemini", parse_gemini_quota(&body), plan, None)
}

// ===================================================================
// Grok Build credentials + billing
// ===================================================================
//
// Grok Build is CREDIT-based. The TUI's `/usage` data comes from the
// `x.ai/billing` extension (grok-build
// `xai-grok-shell/src/extensions/billing.rs` — open source), whose
// `handle_get_billing` fetches `GET {proxy}/billing?format=credits` off the
// CLI chat proxy (`CLI_CHAT_PROXY_BASE_URL_DEFAULT =
// "https://cli-chat-proxy.grok.com/v1"`, agent/config.rs:46) with the
// auth.json bearer + `X-XAI-Token-Auth: xai-grok-cli` (auth/config.rs:288)
// + `x-userid`. VERIFIED live against a real account (2026-07-16): 200 with
// `config.currentPeriod` (weekly window) / `creditUsagePercent` /
// on-demand / prepaid fields.
//
// The stored `key` is a SHORT-LIVED (~1 day) OIDC access token.
// **Termory deliberately does NOT refresh it** (LOCKED — learned the hard
// way, 2026-07-16): auth.x.ai issues ROTATING refresh tokens with reuse
// detection — a refresh both rotates the RT server-side and, on reuse of a
// stale RT, revokes the whole token family. grok itself persists the
// rotated RT back to auth.json under a file lock (manager.rs:64 "hold the
// file lock across the IdP call to prevent refresh-token races"); a second
// client that refreshes WITHOUT persisting (Termory's never-write-
// credentials rule) leaves grok holding a dead RT and logs the user out —
// this was verified by breaking a real login during development. (An
// earlier probe's "the old RT stays valid — reusable" observation was a
// rotation GRACE WINDOW, not reusability.) So: use the stored access token
// while it's valid; once expired, report Expired — running grok refreshes
// its own login and rewrites auth.json, and the quota comes back.

/// The fields Termory needs from one grok auth.json login entry.
struct GrokAuthEntry {
    /// The stored access token (`key`) — may be expired.
    key: String,
    user_id: String,
    /// Plain `email` on the auth entry — the official `/v1/settings`
    /// request attaches it as `x-email` when present
    /// (remote/client.rs:28-30).
    email: Option<String>,
}

fn grok_auth_path() -> Option<PathBuf> {
    crate::providers::grok_home_dir().map(|h| h.join("auth.json"))
}

/// Decode a JWT's payload segment into its claims (no signature
/// verification — local reads only, the API remains the authority).
/// Shared by every JWT-claim reader below so the decode step (base64
/// alphabet, padding, error handling) can't drift between them.
fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Unix-seconds `exp` claim from a JWT.
fn jwt_exp_seconds(jwt: &str) -> Option<i64> {
    jwt_claims(jwt)?.get("exp")?.as_i64()
}

/// Subscription plan from the access token's numeric `tier` claim — the
/// official enum map (grok-build mvp_agent/mod.rs `jwt_tier_claim`:
/// 0 free, 1 supergrok, 2 x_basic, 3 x_premium, 4 x_premium_plus,
/// 5 supergrok_heavy, 6 supergrok_lite), rendered in the CCP display
/// spelling ("SuperGrok", "X Premium+", … — config-types/lib.rs
/// `subscription_tier_display` doc). Fallback only: the claim can be
/// STALE right after an upgrade (the "stale JWT tier" note in
/// mvp_agent/mod.rs) — `/v1/settings` is the authoritative channel.
fn grok_jwt_tier_plan(jwt: &str) -> Option<String> {
    let tier = jwt_claims(jwt)?.get("tier")?.as_u64()?;
    let raw = match tier {
        0 => "Free",
        1 => "SuperGrok",
        2 => "X Basic",
        3 => "X Premium",
        4 => "X Premium+",
        5 => "SuperGrok Heavy",
        6 => "SuperGrok Lite",
        n => return display_plan(&n.to_string()),
    };
    display_plan(raw)
}

fn read_grok_credentials() -> (Option<GrokAuthEntry>, CredentialStatus, Option<String>) {
    let Some(path) = grok_auth_path() else {
        return (None, CredentialStatus::NotFound, None);
    };
    if !path.exists() {
        return (None, CredentialStatus::NotFound, None);
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to read Grok credentials: {err}")),
            );
        }
    };
    parse_grok_credentials(&content)
}

/// auth.json is a map keyed by auth scope (`{issuer}::{client_id}`,
/// auth/config.rs:213); each value is a `GrokAuth` (auth/model.rs:40).
/// Take the first entry that carries a `key` + `user_id`.
fn parse_grok_credentials(
    content: &str,
) -> (Option<GrokAuthEntry>, CredentialStatus, Option<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(err) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Grok credentials: {err}")),
            );
        }
    };
    let Some(map) = parsed.as_object() else {
        return (None, CredentialStatus::NotFound, None);
    };
    let s = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(String::from)
    };
    for entry in map.values() {
        let (Some(key), Some(user_id)) = (s(entry, "key"), s(entry, "user_id")) else {
            continue;
        };
        // Local staleness check on the JWT exp (60s slack). The scaffold
        // still tries an Expired token against the API; the refresh in the
        // fetch arm normally replaces it first.
        let status = match jwt_exp_seconds(&key) {
            Some(exp)
                if exp
                    <= (SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64)
                        + 60 =>
            {
                CredentialStatus::Expired
            }
            _ => CredentialStatus::Valid,
        };
        let email = s(entry, "email");
        return (
            Some(GrokAuthEntry {
                key,
                user_id,
                email,
            }),
            status,
            None,
        );
    }
    (None, CredentialStatus::NotFound, None)
}

/// `x-grok-client-version` the billing proxy expects — a known-good CLI
/// version (the live probe was verified with it).
const GROK_CLIENT_VERSION: &str = "0.2.99";

/// `x-grok-client-identifier` — official default `"grok-shell"`
/// (`process_client_identifier()`, xai-grok-http/lib.rs:240; the
/// `GROK_CLIENT_NAME` env override is a grok-process concern, not ours).
const GROK_CLIENT_IDENTIFIER: &str = "grok-shell";

/// `x-grok-client-mode` — official default `"interactive"`
/// (`process_client_mode()` one-way latch, xai-grok-http/lib.rs:260;
/// `"headless"` is only set by the `grok -p` entry points).
const GROK_CLIENT_MODE: &str = "interactive";

/// Map the billing response to quota tiers + extra usage.
///
/// Utilization follows grok's OWN resolution order (pager
/// `credit_balance_from_config`, effects/helpers.rs:1256-1266): the
/// credits-config `creditUsagePercent` first, else derived from the
/// DEPRECATED `used`/`monthlyLimit` pair, else **unknown** — and unknown
/// yields NO tier at all.
///
/// **A missing percent is NOT 0% (LOCKED).** This code used to read
/// `creditUsagePercent` alone and `unwrap_or(0.0)`, reasoning that proto3
/// JSON omits zero-valued scalars so absence must mean zero. That is true
/// of a `Cent` (`{"val": 0}` → `{}`), but NOT of this response: the
/// endpoint serves NO usage percentage at all for some accounts — verified
/// live against a Free / `isUnifiedBillingUser` login, whose raw body
/// carries `currentPeriod`, the on-demand Cents and `prepaidBalance` but
/// neither `creditUsagePercent` nor the deprecated `used`/`monthlyLimit`.
/// Those users got a confident "0%" ring that never moved, including after
/// their allowance was exhausted (grok itself reports exhaustion through a
/// 429 on the CHAT request — `subscription:free-usage-exhausted`,
/// sampling/error.rs:290 — never through this endpoint). Showing nothing
/// is the honest rendering; inventing a zero is what made the card lie.
/// (grok's own `/usage` prints `Weekly limit: 0%` for these accounts, so
/// this is a divergence from the TUI on purpose — the UI here is a ring
/// that reads as authoritative, not a line of text.)
///
/// The window is `config.currentPeriod`
/// (`USAGE_PERIOD_TYPE_WEEKLY`/`_MONTHLY`, billing.rs:37) with the
/// deprecated `billingPeriodEnd` as fallback — mapped onto the existing
/// `seven_day`/`30_day` tier ids so the tray ("W"/"M") and card labels
/// apply. On-demand credits surface as `ExtraUsage` when a cap is
/// configured; that is independent of the percent, so a no-percent account
/// with a cap still shows its credits.
fn parse_grok_billing(body: &serde_json::Value) -> (Vec<QuotaTier>, Option<ExtraUsage>) {
    let config = body.get("config").cloned().unwrap_or(serde_json::json!({}));
    // Cent values (USD cents) — proto3 omits `val` when 0, so absent ⇒ 0.
    let cents = |k: &str| {
        config
            .get(k)
            .and_then(|v| v.get("val"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    let utilization = config
        .get("creditUsagePercent")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            // Legacy `GetGrokBuildBillingConfig` shape, same fallback the
            // pager applies (a zero limit is no denominator, not 0%).
            let limit = cents("monthlyLimit");
            (limit > 0).then(|| cents("used") as f64 / limit as f64 * 100.0)
        })
        // The pager clamps too — the backend has been seen reporting past
        // 100 (its own test covers `credit_usage_percent: Some(150.0)`).
        // DELIBERATELY stricter than the pager on the low end: it clamps
        // only the served percent and leaves the derived one at `.min(100)`
        // (effects/helpers.rs:1264), so a negative `used` would yield a
        // negative percent there. Unreachable in practice — negative Cents
        // are the accounting convention for a BALANCE, not for usage — but
        // a ring cannot render one, so both branches get a floor.
        .map(|pct| pct.clamp(0.0, 100.0));
    let period_type = config
        .pointer("/currentPeriod/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = if period_type.contains("MONTHLY") {
        "30_day"
    } else {
        // Weekly is the observed consumer default; unspecified enums are
        // omitted by proto3 JSON, so missing ⇒ weekly.
        "seven_day"
    };
    let resets_at = config
        .pointer("/currentPeriod/end")
        .or_else(|| config.get("billingPeriodEnd"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let tiers = utilization
        .map(|utilization| QuotaTier {
            name: name.to_string(),
            // Grok reports ONE account-wide credit window whose name is
            // already the period.
            group: None,
            utilization,
            resets_at,
        })
        .into_iter()
        .collect();
    // On-demand (pay-per-use overflow).
    let cap = cents("onDemandCap");
    let used = cents("onDemandUsed");
    let extra = (cap > 0).then(|| ExtraUsage {
        is_enabled: true,
        monthly_limit: Some(cap as f64 / 100.0),
        used_credits: Some(used as f64 / 100.0),
        utilization: Some(used as f64 / cap as f64 * 100.0),
        currency: Some("USD".to_string()),
        // grok values are already major units (dollars) — no scaling.
        decimal_places: None,
    });
    (tiers, extra)
}

/// Remaining prepaid ("bought") credit balance in DOLLARS, or `None` when
/// there is none to show.
///
/// `config.prepaidBalance` is a `Cent` (USD cents — billing.rs:23), and
/// billing stores credit amounts as NEGATIVE cents by accounting
/// convention: grok's own renderers take `i64::abs` before display
/// (credit_bar.rs:121 and :188), so a `-1250` balance is $12.50 of credit
/// REMAINING, not a debt. Zero (including the proto3-omitted `{}` form)
/// means no credits bought → nothing to render.
fn parse_grok_prepaid_balance(body: &serde_json::Value) -> Option<f64> {
    let cents = body
        .pointer("/config/prepaidBalance/val")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .abs();
    (cents > 0).then(|| cents as f64 / 100.0)
}

/// `subscription_tier_display` from a `/v1/settings` response body.
/// Field names are the Rust field names verbatim (RemoteSettings has no
/// serde rename_all — config-types/lib.rs:211).
fn parse_grok_settings_plan(body: &serde_json::Value) -> Option<String> {
    body.get("subscription_tier_display")
        .and_then(|v| v.as_str())
        .and_then(display_plan)
}

/// Subscription tier display name ("SuperGrok", "X Premium+", "Free",
/// "API Key") from cli-chat-proxy `GET /v1/settings`
/// (`RemoteSettings.subscription_tier_display`, grok-build
/// config-types/lib.rs:718) — the same channel the TUI's billing
/// extension shows (billing.rs:274 enriches its response from
/// RemoteSettings). Headers mirror the official settings fetch
/// (remote/client.rs:17-41 `add_cli_chat_proxy_headers_blocking`:
/// bearer + token-auth + userid + version + optional `x-email` +
/// identifier + mode).
/// Best-effort: any failure → None (the JWT claim fallback applies).
async fn fetch_grok_plan(
    client: &reqwest::Client,
    access_token: &str,
    user_id: &str,
    email: Option<&str>,
) -> Option<String> {
    let mut req = client
        .get("https://cli-chat-proxy.grok.com/v1/settings")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("x-userid", user_id)
        .header("x-grok-client-version", GROK_CLIENT_VERSION);
    if let Some(email) = email {
        req = req.header("x-email", email);
    }
    let resp = req
        .header("x-grok-client-identifier", GROK_CLIENT_IDENTIFIER)
        .header("x-grok-client-mode", GROK_CLIENT_MODE)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    parse_grok_settings_plan(&body)
}

/// Query the billing endpoint the TUI's own `/usage` uses (see the
/// module note above for the full source trail + live verification).
/// Headers mirror the official billing fetch (billing.rs
/// `handle_get_billing`: bearer + token-auth + userid + version +
/// client-mode — NO `x-email`/identifier there, unlike `/settings`).
/// The billing GET and the `/v1/settings` plan lookup are independent
/// (neither's result feeds the other), so they run concurrently — each
/// has its own 10s timeout and sequencing them would let a slow
/// connection double the worst-case wait for a single quota refresh.
async fn query_grok_quota(
    access_token: &str,
    user_id: &str,
    email: Option<&str>,
) -> SubscriptionQuota {
    let client = match quota_http_client("grok") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let billing_send = client
        .get("https://cli-chat-proxy.grok.com/v1/billing?format=credits")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("x-userid", user_id)
        .header("x-grok-client-version", GROK_CLIENT_VERSION)
        .header("x-grok-client-mode", GROK_CLIENT_MODE)
        .send();
    let plan_fetch = fetch_grok_plan(&client, access_token, user_id, email);
    let (billing_result, plan_from_settings) = tokio::join!(billing_send, plan_fetch);
    let resp = match billing_result {
        Ok(r) => r,
        Err(err) => return network_error("grok", err),
    };
    let body = match read_json_or_error("grok", resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (tiers, extra) = parse_grok_billing(&body);
    let prepaid = parse_grok_prepaid_balance(&body);
    // The tier name is NOT in the billing response (billing.rs enriches it
    // from RemoteSettings client-side) — official precedence per
    // mvp_agent/mod.rs `resolve_subscription_tier_for_telemetry`:
    // `/settings` display name first, JWT `tier` claim fallback.
    let plan = plan_from_settings.or_else(|| grok_jwt_tier_plan(access_token));
    SubscriptionQuota::success("grok", tiers, plan, extra).with_prepaid_balance(prepaid)
}

// ===================================================================
// Entry point
// ===================================================================

/// Run a credential read on a blocking thread.
///
/// Every one of them touches the filesystem and, on macOS, spawns
/// `security(1)` — up to [`crate::process::PROBE_TIMEOUT`] of blocking if
/// the login keychain is locked and the unlock dialog goes unanswered.
/// `fetch_quota` is called from an async context on both of its paths (the
/// `fetch_subscription_quota` IPC and the tray's `spawn_quota_fetch`), so
/// doing that inline parks a Tokio worker for the duration.
///
/// A `JoinError` means the read panicked; degrade to "no credential"
/// rather than taking the whole quota fetch down with it.
async fn read_credential<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
    on_panic: T,
) -> T {
    tokio::task::spawn_blocking(read).await.unwrap_or(on_panic)
}

/// Fetch the official-account quota for one CLI (see `SUPPORTED`).
pub async fn fetch_quota(app: CliApp) -> SubscriptionQuota {
    match app {
        CliApp::Claude => {
            let (token, plan, status, message) = read_credential(
                read_claude_credentials,
                (None, None, CredentialStatus::NotFound, None),
            )
            .await;
            quota_for_credential("claude", token, status, message, move |t| async move {
                query_claude_quota(&t, plan).await
            })
            .await
        }
        CliApp::Codex => {
            let (token, account_id, status, message) = read_credential(
                read_codex_credentials,
                (None, None, CredentialStatus::NotFound, None),
            )
            .await;
            quota_for_credential("codex", token, status, message, move |t| async move {
                query_codex_quota(&t, account_id.as_deref()).await
            })
            .await
        }
        CliApp::Gemini => {
            let (mut token, refresh_token, mut status, message) = read_credential(
                read_gemini_credentials,
                (None, None, CredentialStatus::NotFound, None),
            )
            .await;
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
        CliApp::Grok => {
            let (entry, status, message) = read_credential(
                read_grok_credentials,
                (None, CredentialStatus::NotFound, None),
            )
            .await;
            let token = entry.as_ref().map(|e| e.key.clone());
            // NO refresh — deliberate (see the module note: auth.x.ai
            // rotates refresh tokens with reuse detection; refreshing
            // without persisting the rotated RT kills grok's own login).
            // Expired ⇒ the scaffold still tries the stale token once, then
            // reports Expired; running grok refreshes its login and the
            // quota comes back on the next fetch.
            let message = message.or_else(|| {
                (status == CredentialStatus::Expired)
                    .then(|| "Grok token expired — run grok once to refresh its login".to_string())
            });
            let user_id = entry
                .as_ref()
                .map(|e| e.user_id.clone())
                .unwrap_or_default();
            let email = entry.as_ref().and_then(|e| e.email.clone());
            quota_for_credential("grok", token, status, message, move |t| async move {
                query_grok_quota(&t, &user_id, email.as_deref()).await
            })
            .await
        }
        // Claude Desktop's quota would belong to its own claude.ai login,
        // which Termory doesn't read here — surface nothing.
        CliApp::ClaudeDesktop => SubscriptionQuota::not_found(app.bin_name()),
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
    /// The mid-login placeholder must NOT look like a logout.
    ///
    /// `not_found` is the definitive logged-out state: the tray clears its
    /// row for it and `mergeQuotaResult` (quota-utils.ts:64) replaces the
    /// entry outright, which hides the whole quota section — refresh button
    /// included. That is the reported bug this placeholder exists to avoid,
    /// so returning one here would silently restore it. A plain failure
    /// takes the other branch in both places and keeps the last good
    /// numbers, which stay correct because every login flow ends by
    /// restoring the account they describe.
    #[test]
    fn quota_during_login_is_a_transient_failure_not_a_logout() {
        let q = quota_during_login(CliApp::Codex);
        assert_ne!(
            q.credential_status,
            CredentialStatus::NotFound,
            "must not be not_found — that clears the card"
        );
        assert!(
            !q.success,
            "must be a failure so the retry floor stays short"
        );
        assert!(q.error.is_some(), "the manual-refresh toast needs a reason");
        assert!(q.tiers.is_empty(), "it carries no numbers of its own");
    }

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
        let (token, plan, status, message) = parse_claude_credentials(&content);
        assert_eq!(token.as_deref(), Some("sk-ant-oat01-abc"));
        assert!(plan.is_none()); // no subscriptionType in this fixture
        assert_eq!(status, CredentialStatus::Valid);
        assert!(message.is_none());
    }

    #[test]
    fn parse_claude_credentials_accepts_legacy_key() {
        let content = json!({
            "claude.ai_oauth": { "accessToken": "tok" }
        })
        .to_string();
        let (token, _, status, _) = parse_claude_credentials(&content);
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
        let (token, _, status, _) = parse_claude_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(status, CredentialStatus::Expired);
    }

    #[test]
    fn parse_claude_credentials_missing_entry_is_parse_error() {
        let (token, _, status, _) = parse_claude_credentials("{}");
        assert!(token.is_none());
        assert_eq!(status, CredentialStatus::ParseError);
    }

    #[test]
    fn parse_claude_credentials_empty_token_is_parse_error() {
        let content = json!({ "claudeAiOauth": { "accessToken": "" } }).to_string();
        let (token, _, status, _) = parse_claude_credentials(&content);
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
    fn parse_claude_usage_extracts_model_scoped_limits_from_the_array() {
        // Real-shape body (2026-07): flat five_hour/seven_day still
        // carry utilization, the legacy per-model fields are null, and
        // model-scoped weekly limits live in the `limits` array.
        let body = json!({
            "five_hour": { "utilization": 53.0, "resets_at": "2026-07-04T09:29:59Z" },
            "seven_day": { "utilization": 63.0, "resets_at": "2026-07-09T15:59:59Z" },
            "seven_day_opus": null,
            "seven_day_sonnet": null,
            "limits": [
                { "kind": "session", "percent": 53, "resets_at": "2026-07-04T09:29:59Z", "scope": null },
                { "kind": "weekly_all", "group": "weekly", "percent": 63, "resets_at": "2026-07-09T15:59:59Z", "scope": null },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 100, "resets_at": "2026-07-09T15:59:59Z",
                  "scope": { "model": { "id": null, "display_name": "Fable" } }, "is_active": true }
            ]
        });
        let (tiers, _) = parse_claude_usage(&body);
        let names: Vec<&str> = tiers.iter().map(|t| t.name.as_str()).collect();
        // Flat five_hour/seven_day first (session/weekly_all in the
        // array are NOT re-added), then the scoped model window verbatim.
        assert_eq!(names, vec!["five_hour", "seven_day", "Fable"]);
        let fable = tiers.iter().find(|t| t.name == "Fable").unwrap();
        assert_eq!(fable.utilization, 100.0);
        assert_eq!(fable.resets_at.as_deref(), Some("2026-07-09T15:59:59Z"));
        // The scoped window carries the API's own period grouping, so
        // the UI can label it "Weekly · Fable" instead of a bare model
        // name; the flat account-wide windows carry none (their name IS
        // the period).
        assert_eq!(fable.group.as_deref(), Some("weekly"));
        assert!(tiers
            .iter()
            .filter(|t| t.name != "Fable")
            .all(|t| t.group.is_none()));
    }

    #[test]
    fn parse_claude_usage_scoped_limit_without_a_group_keeps_the_model_name() {
        // `group` is what the live API sends, but it must not be
        // REQUIRED: a body without it still surfaces the window, just
        // with no period to compose a label from.
        let body = json!({
            "limits": [
                { "kind": "weekly_scoped", "percent": 7,
                  "scope": { "model": { "display_name": "Fable" } } },
                { "kind": "weekly_scoped", "group": "", "percent": 8,
                  "scope": { "model": { "display_name": "Opus" } } }
            ]
        });
        let (tiers, _) = parse_claude_usage(&body);
        assert_eq!(
            tiers.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["Fable", "Opus"]
        );
        // Missing and empty both mean "no period", never an empty label.
        assert!(tiers.iter().all(|t| t.group.is_none()));
    }

    #[test]
    fn parse_claude_usage_skips_scoped_limits_without_a_model_name() {
        let body = json!({
            "five_hour": { "utilization": 10.0 },
            "limits": [
                { "kind": "weekly_scoped", "percent": 20, "scope": { "model": { "display_name": "" } } },
                { "kind": "weekly_scoped", "percent": 30, "scope": null }
            ]
        });
        let (tiers, _) = parse_claude_usage(&body);
        // Only the flat five_hour — both scoped entries lack a usable name.
        assert_eq!(
            tiers.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["five_hour"]
        );
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
        // Real-shape body (2026-07): credit amounts are MINOR units with
        // `decimal_places: 2` (1944 cents + 5000 cents ⇒ $19.44 / $50.00).
        let body = json!({
            "five_hour": { "utilization": 50.0 },
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 5000.0,
                "used_credits": 1944.0,
                "utilization": 38.88,
                "currency": "USD",
                "decimal_places": 2
            }
        });
        let (_, extra) = parse_claude_usage(&body);
        let extra = extra.expect("extra_usage parsed");
        assert!(extra.is_enabled);
        assert_eq!(extra.monthly_limit, Some(5000.0));
        assert_eq!(extra.used_credits, Some(1944.0));
        assert_eq!(extra.utilization, Some(38.88));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
        assert_eq!(extra.decimal_places, Some(2));
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
    fn parse_codex_credentials_apikey_without_tokens_is_not_found() {
        // Pure API-key login (no ChatGPT tokens) has no subscription
        // windows — treated like "no OAuth login" so the UI shows
        // nothing.
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
    fn parse_codex_credentials_tokens_without_auth_mode_is_valid() {
        // The Termory round-trip shape: activate custom provider →
        // back to Official removes auth_mode + OPENAI_API_KEY but
        // keeps tokens. Codex itself resolves this to ChatGPT mode
        // (resolved_mode, manager.rs:980-988) — the quota must show.
        let content = json!({
            "tokens": { "access_token": "tok", "account_id": "acc" },
            "last_refresh": chrono::Utc::now().to_rfc3339()
        })
        .to_string();
        let (token, account, status, _) = parse_codex_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(account.as_deref(), Some("acc"));
        assert_eq!(status, CredentialStatus::Valid);
    }

    #[test]
    fn parse_codex_credentials_apikey_with_tokens_still_reads_the_account() {
        // A custom provider is ACTIVE (Termory wrote auth_mode=apikey)
        // but the ChatGPT account is still logged in (tokens merged,
        // not overwritten) — the official-account quota stays
        // readable, same semantic as Claude's credential file.
        let content = json!({
            "auth_mode": "apikey",
            "OPENAI_API_KEY": "sk-...",
            "tokens": { "access_token": "tok", "account_id": "acc" },
            "last_refresh": chrono::Utc::now().to_rfc3339()
        })
        .to_string();
        let (token, _, status, _) = parse_codex_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(status, CredentialStatus::Valid);
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
    fn quota_error_for_status_maps_gone_to_not_found() {
        use reqwest::StatusCode;
        // 410 Gone (individual Gemini login cut off) → clean NotFound,
        // no error text — the card hides instead of toasting.
        let gone = quota_error_for_status("gemini", StatusCode::GONE, "gone");
        assert_eq!(gone.credential_status, CredentialStatus::NotFound);
        assert!(!gone.success);
        assert!(gone.error.is_none());
        assert!(gone.tiers.is_empty());

        // 401/403 → Expired credential (re-login), with a message.
        let unauth = quota_error_for_status("gemini", StatusCode::UNAUTHORIZED, "");
        assert_eq!(unauth.credential_status, CredentialStatus::Expired);
        assert!(unauth.error.is_some());
        let forbidden = quota_error_for_status("gemini", StatusCode::FORBIDDEN, "");
        assert_eq!(forbidden.credential_status, CredentialStatus::Expired);

        // Other failures keep a Valid credential + surface the message.
        let server = quota_error_for_status(
            "gemini",
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":{"message":"boom"}}"#,
        );
        assert_eq!(server.credential_status, CredentialStatus::Valid);
        assert!(server.error.unwrap().contains("boom"));
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
            tiers: vec![
                QuotaTier {
                    name: "five_hour".into(),
                    group: None,
                    utilization: 12.5,
                    resets_at: Some("2026-06-10T12:00:00Z".into()),
                },
                QuotaTier {
                    name: "Fable".into(),
                    group: Some("weekly".into()),
                    utilization: 4.0,
                    resets_at: None,
                },
            ],
            plan: Some("Max".into()),
            extra_usage: None,
            prepaid_balance: None,
            error: None,
            queried_at: Some(1),
        };
        let v = serde_json::to_value(&quota).unwrap();
        assert_eq!(v["credentialStatus"], "valid");
        assert_eq!(v["tiers"][0]["resetsAt"], "2026-06-10T12:00:00Z");
        // An absent group is OMITTED, not null — the frontend's optional
        // field stays undefined for account-wide windows.
        assert!(v["tiers"][0].get("group").is_none());
        assert_eq!(v["tiers"][1]["group"], "weekly");
        assert_eq!(v["plan"], "Max");
        assert_eq!(v["queriedAt"], 1);
        // Same omit-when-absent rule for the grok-only balance.
        assert!(v.get("prepaidBalance").is_none());
        let with_balance = SubscriptionQuota {
            prepaid_balance: Some(12.5),
            ..quota
        };
        assert_eq!(
            serde_json::to_value(&with_balance).unwrap()["prepaidBalance"],
            12.5
        );
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

    /// Claude's credential is in the macOS Keychain and emits no event of
    /// its own; these two files beside the config dir are what does move.
    /// Matched by FULL path — `.claude.json` is an ordinary-looking name
    /// and a file called that anywhere else is not Claude's config.
    #[test]
    fn credential_cli_for_path_matches_claudes_keychain_signal_files() {
        use crate::testutils::{lock_home, override_home, EnvVarGuard};
        use std::path::Path;
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("termory-claude-signal");
        std::fs::create_dir_all(&tmp).unwrap();
        let _h = override_home(&tmp);
        let _e = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        // The token-refresh lock: `proper-lockfile` appends `.lock` to the
        // locked path, and Claude locks the config DIR itself.
        assert_eq!(
            credential_cli_for_path(&tmp.join(".claude.lock")),
            Some(CliApp::Claude)
        );
        // The LOGIN signal must NOT be here. `.claude.json` is Claude's
        // whole global config, written from 159 places in its source, and
        // this map feeds `force_quota_refresh` — which bypasses its own
        // rate floor. Routing it here turned a feature documented as
        // having no periodic polling into an API call every ten seconds
        // while Claude was in use. The account sync consumes it directly.
        assert_eq!(credential_cli_for_path(&tmp.join(".claude.json")), None);
        assert_eq!(
            claude_identity_signal_path().as_deref(),
            Some(tmp.join(".claude.json").as_path())
        );
        // Same basename somewhere else is not Claude's lock.
        assert_eq!(
            credential_cli_for_path(Path::new("/somewhere/else/.claude.lock")),
            None
        );
        // A lock on a DIFFERENT path — Claude locks mailboxes, markers and
        // tasks too, and none of those mean the credential moved.
        assert_eq!(
            credential_cli_for_path(&tmp.join(".claude/tasks.lock")),
            None
        );
    }

    #[test]
    fn credential_cli_for_path_matches_relocated_codex_home() {
        use crate::testutils::{lock_home, EnvVarGuard};
        use std::path::Path;
        let _g = lock_home();
        let _e = EnvVarGuard::set("CODEX_HOME", "/custom/cdx");

        // auth.json directly under the relocated CODEX_HOME maps to Codex
        // even though the dir isn't named ".codex".
        assert_eq!(
            credential_cli_for_path(Path::new("/custom/cdx/auth.json")),
            Some(CliApp::Codex)
        );
        // An unrelated auth.json still doesn't match.
        assert_eq!(
            credential_cli_for_path(Path::new("/u/x/.local/share/opencode/auth.json")),
            None
        );
    }

    /// The scaffold's four credential-status branches, exercised with
    /// stub queries (no network).
    #[test]
    fn quota_for_credential_scaffold_branches() {
        fn ok(app: &'static str) -> SubscriptionQuota {
            SubscriptionQuota::success(app, vec![], None, None)
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

    #[test]
    fn display_plan_tidies_raw_identifiers() {
        assert_eq!(display_plan("max").as_deref(), Some("Max"));
        assert_eq!(display_plan("pro").as_deref(), Some("Pro"));
        assert_eq!(display_plan("plus").as_deref(), Some("Plus"));
        assert_eq!(display_plan("free-tier").as_deref(), Some("Free"));
        assert_eq!(display_plan("standard-tier").as_deref(), Some("Standard"));
        assert_eq!(display_plan("  enterprise ").as_deref(), Some("Enterprise"));
        assert_eq!(display_plan(""), None);
    }

    /// Only Max carries a multiplier, and only for the two tier strings
    /// Claude's own source names. Everything else degrades to the bare
    /// plan — never to a raw identifier.
    #[test]
    fn claude_plan_label_appends_the_multiplier_to_max_only() {
        let label = |sub: Option<&str>, tier: Option<&str>| claude_plan_label(sub, tier);

        assert_eq!(
            label(Some("max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            label(Some("max"), Some("default_claude_max_5x")).as_deref(),
            Some("Max 5x")
        );
        // Max with no tier recorded, or one nobody has seen before: the
        // plan is still correct, just less specific.
        assert_eq!(label(Some("max"), None).as_deref(), Some("Max"));
        assert_eq!(
            label(Some("max"), Some("default_claude_max_50x")).as_deref(),
            Some("Max"),
            "an unknown tier must not leak a raw identifier into the badge"
        );
        // NOT Max: the tier string does not name a Max level on its own —
        // `isTeamPremiumSubscriber` pairs the 5x tier with a TEAM
        // subscription (auth.ts:1687), so appending it here would invent a
        // plan that means something else.
        assert_eq!(
            label(Some("team"), Some("default_claude_max_5x")).as_deref(),
            Some("Team")
        );
        assert_eq!(
            label(Some("pro"), Some("default_claude_max_20x")).as_deref(),
            Some("Pro")
        );
        // No subscription at all → no label, tier or not.
        assert_eq!(label(None, Some("default_claude_max_20x")), None);
        assert_eq!(label(Some(""), None), None);
    }

    #[test]
    fn parse_claude_credentials_extracts_subscription_type() {
        let content = json!({
            "claudeAiOauth": {
                "accessToken": "tok",
                "subscriptionType": "max",
                "expiresAt": far_future_ms()
            }
        })
        .to_string();
        let (token, plan, status, _) = parse_claude_credentials(&content);
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(plan.as_deref(), Some("Max"));
        assert_eq!(status, CredentialStatus::Valid);
    }

    #[test]
    fn codex_plan_reads_plan_type() {
        assert_eq!(
            codex_plan(&json!({"plan_type": "plus", "rate_limit": {}})).as_deref(),
            Some("Plus")
        );
        assert!(codex_plan(&json!({})).is_none());
    }

    #[test]
    fn parse_grok_billing_reports_no_tier_when_the_endpoint_serves_no_percent() {
        // The REAL response for a Free / unified-billing login, captured
        // live from the raw endpoint (2026-07-31): a period and the Cent
        // fields, but NO usage percentage in any form. This body used to
        // produce a 0% ring that never moved even once the allowance was
        // exhausted — the absence means "not served", not "nothing used",
        // so it must produce no tier at all.
        let body = json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-29T00:00:00+00:00",
                    "end": "2026-08-05T00:00:00+00:00"
                },
                "onDemandCap": {"val": 0},
                "onDemandUsed": {"val": 0},
                "isUnifiedBillingUser": true,
                "prepaidBalance": {"val": 0},
                "topUpMethod": "TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
                "billingPeriodStart": "2026-07-29T00:00:00+00:00",
                "billingPeriodEnd": "2026-08-05T00:00:00+00:00"
            }
        });
        let (tiers, extra) = parse_grok_billing(&body);
        assert!(tiers.is_empty(), "no usage data → no ring, not a 0% ring");
        assert!(extra.is_none(), "no on-demand cap → no extra usage");
    }

    #[test]
    fn parse_grok_prepaid_balance_reads_the_signed_cent_value() {
        // Billing stores credits as NEGATIVE cents; grok's own renderers
        // abs() them, so this is $12.50 REMAINING.
        let body = json!({"config": {"prepaidBalance": {"val": -1250}}});
        assert_eq!(parse_grok_prepaid_balance(&body), Some(12.5));
        // A positive value means the same amount.
        let body = json!({"config": {"prepaidBalance": {"val": 1250}}});
        assert_eq!(parse_grok_prepaid_balance(&body), Some(12.5));
    }

    #[test]
    fn parse_grok_prepaid_balance_is_absent_when_zero_or_missing() {
        // proto3 omits a zero Cent's `val` (and the whole field), and a
        // zero balance is nothing to show either way.
        assert_eq!(
            parse_grok_prepaid_balance(&json!({"config": {"prepaidBalance": {"val": 0}}})),
            None
        );
        assert_eq!(
            parse_grok_prepaid_balance(&json!({"config": {"prepaidBalance": {}}})),
            None
        );
        assert_eq!(parse_grok_prepaid_balance(&json!({"config": {}})), None);
    }

    #[test]
    fn parse_grok_billing_falls_back_to_the_deprecated_used_over_limit() {
        // Legacy `GetGrokBuildBillingConfig` shape — the pager's own second
        // resolution step (effects/helpers.rs:1264), which this code
        // previously skipped entirely.
        let body = json!({
            "config": {
                "monthlyLimit": {"val": 2000},
                "used": {"val": 500},
                "billingPeriodEnd": "2026-08-05T00:00:00+00:00"
            }
        });
        let (tiers, _) = parse_grok_billing(&body);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].utilization, 25.0);
        assert_eq!(
            tiers[0].resets_at.as_deref(),
            Some("2026-08-05T00:00:00+00:00")
        );
    }

    #[test]
    fn parse_grok_billing_zero_percent_is_still_a_tier() {
        // An explicit zero IS data — the distinction the missing-field case
        // above turns on. It must still render (a genuinely unused window).
        let body = json!({"config": {"creditUsagePercent": 0.0}});
        let (tiers, _) = parse_grok_billing(&body);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].utilization, 0.0);
    }

    #[test]
    fn parse_grok_billing_clamps_an_over_100_percent() {
        // The pager clamps; its own tests cover a 150% response.
        let body = json!({"config": {"creditUsagePercent": 150.0}});
        let (tiers, _) = parse_grok_billing(&body);
        assert_eq!(tiers[0].utilization, 100.0);
    }

    #[test]
    fn parse_grok_billing_keeps_on_demand_credits_without_a_percent() {
        // The credits ring is independent of the window percent, so an
        // account with a cap still shows its spend even when the endpoint
        // serves no utilization.
        let body = json!({
            "config": {
                "onDemandCap": {"val": 5000},
                "onDemandUsed": {"val": 1250}
            }
        });
        let (tiers, extra) = parse_grok_billing(&body);
        assert!(tiers.is_empty());
        let extra = extra.expect("cap configured");
        assert_eq!(extra.used_credits, Some(12.5));
    }

    #[test]
    fn parse_grok_billing_monthly_percent_and_on_demand() {
        let body = json!({
            "config": {
                "creditUsagePercent": 42.5,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "end": "2026-08-01T00:00:00+00:00"
                },
                "onDemandCap": {"val": 5000},
                "onDemandUsed": {"val": 1250}
            }
        });
        let (tiers, extra) = parse_grok_billing(&body);
        assert_eq!(tiers[0].name, "30_day");
        assert_eq!(tiers[0].utilization, 42.5);
        let extra = extra.expect("on-demand cap configured");
        assert!(extra.is_enabled);
        assert_eq!(extra.monthly_limit, Some(50.0));
        assert_eq!(extra.used_credits, Some(12.5));
        assert_eq!(extra.utilization, Some(25.0));
    }

    #[test]
    fn grok_jwt_tier_plan_maps_claims_to_display_names() {
        let jwt = |claims: &str| {
            use base64::Engine;
            let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
            format!(
                "{}.{}.sig",
                b64(br#"{"alg":"none"}"#),
                b64(claims.as_bytes())
            )
        };
        // The official enum map (mvp_agent/mod.rs jwt_tier_claim), CCP
        // display spelling.
        assert_eq!(
            grok_jwt_tier_plan(&jwt(r#"{"tier":0}"#)).as_deref(),
            Some("Free")
        );
        assert_eq!(
            grok_jwt_tier_plan(&jwt(r#"{"tier":1}"#)).as_deref(),
            Some("SuperGrok")
        );
        assert_eq!(
            grok_jwt_tier_plan(&jwt(r#"{"tier":4}"#)).as_deref(),
            Some("X Premium+")
        );
        assert_eq!(
            grok_jwt_tier_plan(&jwt(r#"{"tier":5}"#)).as_deref(),
            Some("SuperGrok Heavy")
        );
        // Unknown future tier passes through as the raw number (fail-open,
        // mirrors the official `_ => tier.to_string()` arm).
        assert_eq!(
            grok_jwt_tier_plan(&jwt(r#"{"tier":9}"#)).as_deref(),
            Some("9")
        );
        // No tier claim / not a JWT → None.
        assert_eq!(grok_jwt_tier_plan(&jwt(r#"{"exp":1}"#)), None);
        assert_eq!(grok_jwt_tier_plan("not-a-jwt"), None);
    }

    #[test]
    fn parse_grok_settings_plan_reads_display_name() {
        assert_eq!(
            parse_grok_settings_plan(&json!({"subscription_tier_display": "SuperGrok Heavy"}))
                .as_deref(),
            Some("SuperGrok Heavy")
        );
        // Empty / whitespace-only display → None so the JWT fallback runs
        // (mirrors the official `.filter(|s| !s.trim().is_empty())`).
        assert_eq!(
            parse_grok_settings_plan(&json!({"subscription_tier_display": "  "})),
            None
        );
        assert_eq!(parse_grok_settings_plan(&json!({})), None);
    }

    #[test]
    fn parse_grok_credentials_reads_entry_and_expiry() {
        // Far-future exp → Valid.
        let exp = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64)
            + 86_400;
        let jwt = |exp: i64| {
            use base64::Engine;
            let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
            format!(
                "{}.{}.sig",
                b64(br#"{"alg":"none"}"#),
                b64(format!(r#"{{"exp":{exp}}}"#).as_bytes())
            )
        };
        let content = json!({
            "https://auth.x.ai::client-123": {
                "key": jwt(exp),
                "auth_mode": "oidc",
                "user_id": "user-1",
                "email": "u@example.com",
                "refresh_token": "rt-1",
                "oidc_issuer": "https://auth.x.ai",
                "oidc_client_id": "client-123",
                "principal_type": "User",
                "principal_id": "user-1"
            }
        })
        .to_string();
        let (entry, status, msg) = parse_grok_credentials(&content);
        let entry = entry.expect("entry parsed");
        assert_eq!(status, CredentialStatus::Valid);
        assert!(msg.is_none());
        assert_eq!(entry.user_id, "user-1");
        assert_eq!(entry.email.as_deref(), Some("u@example.com"));
        assert!(entry.key.starts_with("eyJ"), "key is the stored JWT");

        // Expired exp → Expired (refresh path takes over in the fetch arm).
        let content = json!({
            "https://auth.x.ai::client-123": {
                "key": jwt(1_000_000),
                "user_id": "user-1"
            }
        })
        .to_string();
        let (_, status, _) = parse_grok_credentials(&content);
        assert_eq!(status, CredentialStatus::Expired);

        // No usable entry → NotFound (logged out).
        let (entry, status, _) = parse_grok_credentials(r#"{"other": {}}"#);
        assert!(entry.is_none());
        assert_eq!(status, CredentialStatus::NotFound);
    }

    #[test]
    fn credential_path_matches_grok_auth_json() {
        assert_eq!(
            credential_cli_for_path(std::path::Path::new("/home/u/.grok/auth.json")),
            Some(CliApp::Grok)
        );
        // OpenCode's unrelated auth.json still doesn't match.
        assert_eq!(
            credential_cli_for_path(std::path::Path::new(
                "/home/u/.local/share/opencode/auth.json"
            )),
            None
        );
    }

    #[test]
    fn gemini_plan_extracts_keyword_from_marketing_name() {
        // Google One AI Pro — currentTier reports standard-tier, the
        // real plan rides in paidTier (official precedence,
        // setup.ts:221). The badge wants just "Pro".
        let body = json!({
            "currentTier": { "id": "standard-tier" },
            "paidTier": {
                "id": "g1-pro-tier",
                "name": "Gemini Code Assist in Google One AI Pro"
            }
        });
        assert_eq!(gemini_plan(&body).as_deref(), Some("Pro"));
        let body = json!({
            "currentTier": { "id": "free-tier", "name": "Free", "isDefault": true }
        });
        assert_eq!(gemini_plan(&body).as_deref(), Some("Free"));
        // No keyword in the name → fall back to the id, "-tier" stripped.
        let body = json!({ "currentTier": { "id": "standard-tier" } });
        assert_eq!(gemini_plan(&body).as_deref(), Some("Standard"));
        // "Ultra" wins over an unrelated id.
        let body = json!({
            "currentTier": { "id": "whatever", "name": "Google One AI Ultra" }
        });
        assert_eq!(gemini_plan(&body).as_deref(), Some("Ultra"));
        assert!(gemini_plan(&json!({})).is_none());
    }
}
