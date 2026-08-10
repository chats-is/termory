//! Account balance for a CUSTOM provider.
//!
//! Termory's providers are all third-party endpoints the user typed in
//! themselves, so there is no login to read and no per-CLI credential
//! file: the only two things known about a provider are its `base_url`
//! and its `api_key`. That is exactly enough, because every vendor here
//! exposes its wallet under the SAME key used for inference — so the
//! whole feature is "recognise the host, then GET one endpoint".
//!
//! **`base_url` is used to IDENTIFY the vendor, never to build the
//! request URL** (`detect_vendor` + the hardcoded per-vendor endpoints).
//! The two are deliberately decoupled, and that decoupling is the
//! feature's boundary: the balance endpoint lives on the vendor's own
//! domain and is reachable only with a key that domain issued. A relay
//! that proxies DeepSeek has a relay `base_url` (no match → Unsupported)
//! and a relay key that `api.deepseek.com` would reject anyway. So this
//! module serves DIRECT-to-vendor providers only; relays have no common
//! balance API and are out of scope.
//!
//! Endpoints, response shapes and unit conversions are all taken from
//! cc-switch's `src-tauri/src/services/balance.rs` (audited at commit
//! `997be22`) — cited per arm. Adding a vendor is one `detect_vendor`
//! arm, one `endpoint` arm and one parser; do NOT add one without a
//! verified source for its endpoint.
//!
//! Divergence from cc-switch, deliberate: **a missing amount is an
//! ERROR, never 0.** cc-switch `unwrap_or(0.0)`s every field, so a
//! response whose shape moved renders a confident "$0.00" that never
//! changes. This codebase has already been bitten by exactly that
//! reasoning on grok's `creditUsagePercent` (see the grok quota notes in
//! CLAUDE.md): absent is not zero, and a wrong number reads as
//! authoritative where a missing one does not.

use crate::providers::Provider;
use crate::quota::{http_error_message, now_millis};

/// Emitted with a `ProviderBalance` payload after every fetch the BACKEND
/// made itself (the tray's menu-open pass, the startup warm-up, and the
/// re-fetch after a provider switch or edit), so an open Providers page
/// reflects it without a request of its own and both sides share one
/// throttle marker. Frontend mirror: `BALANCE_CHANGED_EVENT` in
/// src/constants.ts.
pub const BALANCE_CHANGED_EVENT: &str = "termory:balance-changed";
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

// ===================================================================
// Wire types (camelCase to the frontend)
// ===================================================================

/// Outcome of one balance query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceStatus {
    /// Amounts were read — `entries` is non-empty.
    Ok,
    /// `base_url` matched no vendor this build knows. Not a failure:
    /// the overwhelmingly common case (any relay, any gateway), so the
    /// UI should render NOTHING rather than an error.
    Unsupported,
    /// Known vendor, but the provider carries no API key to query with.
    NoKey,
    /// The vendor rejected the key (HTTP 401/403).
    AuthFailed,
    /// Network failure, non-2xx, or a response we could not read.
    Error,
}

/// One wallet amount. A `Vec` rather than flat fields because DeepSeek
/// reports `balance_infos` PER CURRENCY and can return more than one;
/// every other vendor here yields exactly one entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceEntry {
    /// ISO-ish currency code the amounts are in ("USD", "CNY"), taken
    /// from the response where the vendor reports one and from the
    /// endpoint's own documented unit otherwise.
    pub currency: String,
    /// Remaining spendable amount, in MAJOR units (dollars, not cents —
    /// Novita's 0.0001-USD minor unit is converted at parse time).
    pub remaining: f64,
    /// Granted total and amount spent — only OpenRouter reports the
    /// pair; the others expose a bare remaining balance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    /// The account can no longer spend. DeepSeek's own `is_available`
    /// flag where the vendor reports one (it can be false with a
    /// non-zero balance), else derived as `remaining <= 0`.
    pub depleted: bool,
}

/// Result of one provider's balance query. Never an `Err` at the IPC
/// boundary — every failure rides in `status` + `error` so the card can
/// render per-state instead of raising a toast.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderBalance {
    /// The `Provider.id` this result belongs to — the frontend keys its
    /// cache by it, so a result can never be shown under another card.
    pub provider_id: String,
    pub status: BalanceStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<BalanceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix millis of the query (frontend staleness display).
    pub queried_at: i64,
}

impl ProviderBalance {
    fn new(provider_id: &str, status: BalanceStatus) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            status,
            entries: vec![],
            error: None,
            queried_at: now_millis(),
        }
    }

    fn failed(provider_id: &str, status: BalanceStatus, error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::new(provider_id, status)
        }
    }
}

// ===================================================================
// Vendor detection — the whole "infer from base_url" step
// ===================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceVendor {
    DeepSeek,
    StepFun,
    /// SiliconFlow's two sites are separate accounts with separate
    /// wallets AND different currencies (`.cn` bills CNY, `.com` USD),
    /// so they are distinct vendors rather than one with a flag.
    SiliconFlowCn,
    SiliconFlowCom,
    OpenRouter,
    Novita,
}

impl BalanceVendor {
    /// The vendor's balance endpoint — a CONSTANT, never derived from
    /// the provider's `base_url` (see the module docs).
    fn endpoint(self) -> &'static str {
        match self {
            // cc-switch balance.rs:71
            Self::DeepSeek => "https://api.deepseek.com/user/balance",
            // cc-switch balance.rs:149. NOTE the asymmetry: `api.stepfun.ai`
            // is also recognised, but the query always goes to `.com`.
            Self::StepFun => "https://api.stepfun.com/v1/accounts",
            // cc-switch balance.rs:207
            Self::SiliconFlowCn => "https://api.siliconflow.cn/v1/user/info",
            Self::SiliconFlowCom => "https://api.siliconflow.com/v1/user/info",
            // cc-switch balance.rs:284
            Self::OpenRouter => "https://openrouter.ai/api/v1/credits",
            // cc-switch balance.rs:349
            Self::Novita => "https://api.novita.ai/v3/user/balance",
        }
    }
}

/// The lowercased host of a base URL — scheme, userinfo, port and path
/// stripped. Empty when there is nothing host-shaped to take.
fn host_of(base_url: &str) -> String {
    base_url
        .trim()
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url.trim())
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@') // drop any user:pass@
        .next()
        .unwrap_or("")
        .split(':') // drop any :port
        .next()
        .unwrap_or("")
        .to_lowercase()
}
/// Recognise the vendor from a provider's `base_url`. Matched against
/// the HOST only, so any path/version suffix the user typed (`/v1`,
/// `/v1/chat/completions`, a trailing slash) is irrelevant.
///
/// **Host-scoped, unlike cc-switch's `base_url.contains(...)`** — that
/// form also matches the PATH, so a relay at
/// `https://relay.example.com/openrouter.ai/v1` (vendor name as a route
/// prefix, a real relay convention) is taken for OpenRouter itself and
/// its key is then sent to `openrouter.ai`. Substring matching WITHIN
/// the host is kept, so regional subdomains still resolve.
///
/// `.cn` is tested before `.com` for SiliconFlow — with `contains` the
/// order is behaviour, and a `.cn`-first arm keeps a `.com` host from
/// being swallowed only because both share the brand segment.
pub fn detect_vendor(base_url: &str) -> Option<BalanceVendor> {
    let url = host_of(base_url);
    if url.contains("api.deepseek.com") {
        Some(BalanceVendor::DeepSeek)
    } else if url.contains("api.stepfun.ai") || url.contains("api.stepfun.com") {
        Some(BalanceVendor::StepFun)
    } else if url.contains("api.siliconflow.cn") {
        Some(BalanceVendor::SiliconFlowCn)
    } else if url.contains("api.siliconflow.com") {
        Some(BalanceVendor::SiliconFlowCom)
    } else if url.contains("openrouter.ai") {
        Some(BalanceVendor::OpenRouter)
    } else if url.contains("api.novita.ai") {
        Some(BalanceVendor::Novita)
    } else {
        None
    }
}

// ===================================================================
// Response parsing (pure — this is what the tests drive)
// ===================================================================

/// A JSON number that some of these APIs send as a string (`"12.5"`).
fn amount(obj: &Value, field: &str) -> Option<f64> {
    obj.get(field).and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn entry(currency: &str, remaining: f64) -> BalanceEntry {
    BalanceEntry {
        currency: currency.to_string(),
        remaining,
        total: None,
        used: None,
        depleted: remaining <= 0.0,
    }
}

/// `{ balance_infos: [{ currency, total_balance, … }], is_available }`
/// — cc-switch balance.rs:72. One entry per currency; `is_available` is
/// the vendor's own spendable flag and applies to every entry.
fn parse_deepseek(body: &Value) -> Vec<BalanceEntry> {
    let available = body
        .get("is_available")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let Some(infos) = body.get("balance_infos").and_then(Value::as_array) else {
        return vec![];
    };
    infos
        .iter()
        .filter_map(|info| {
            let currency = info
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("CNY");
            // No amount ⇒ drop the entry rather than report a 0 balance.
            let total = amount(info, "total_balance")?;
            Some(BalanceEntry {
                depleted: !available || total <= 0.0,
                ..entry(currency, total)
            })
        })
        .collect()
}

/// `{ balance, total_cash_balance, total_voucher_balance, … }` —
/// cc-switch balance.rs:150. `balance` is the spendable total; the cash
/// and voucher splits are deliberately not surfaced.
fn parse_stepfun(body: &Value) -> Vec<BalanceEntry> {
    amount(body, "balance")
        .map(|b| vec![entry("CNY", b)])
        .unwrap_or_default()
}

/// `{ code, data: { balance, chargeBalance, totalBalance, … } }` —
/// cc-switch balance.rs:208. `totalBalance` is gift + topped-up.
fn parse_siliconflow(body: &Value, currency: &str) -> Vec<BalanceEntry> {
    body.get("data")
        .and_then(|data| amount(data, "totalBalance"))
        .map(|b| vec![entry(currency, b)])
        .unwrap_or_default()
}

/// `{ data: { total_credits, total_usage } }` — cc-switch
/// balance.rs:285. Remaining is the difference; OpenRouter is the only
/// vendor here that reports the granted/spent pair.
///
/// `total_credits` is required (absent ⇒ no entry ⇒ the caller reports
/// a shape error); `total_usage` defaults to 0, which a brand-new
/// account genuinely is.
fn parse_openrouter(body: &Value) -> Vec<BalanceEntry> {
    let data = body.get("data").unwrap_or(body);
    let Some(total) = amount(data, "total_credits") else {
        return vec![];
    };
    let used = amount(data, "total_usage").unwrap_or(0.0);
    let remaining = total - used;
    vec![BalanceEntry {
        total: Some(total),
        used: Some(used),
        ..entry("USD", remaining)
    }]
}

/// `{ availableBalance, cashBalance, creditLimit, … }` — cc-switch
/// balance.rs:350. **Amounts are in 0.0001 USD**, so the raw integer is
/// divided by 10 000; skipping that reports a balance 10 000× too high.
fn parse_novita(body: &Value) -> Vec<BalanceEntry> {
    amount(body, "availableBalance")
        .map(|raw| vec![entry("USD", raw / 10_000.0)])
        .unwrap_or_default()
}

fn parse_body(vendor: BalanceVendor, body: &Value) -> Vec<BalanceEntry> {
    match vendor {
        BalanceVendor::DeepSeek => parse_deepseek(body),
        BalanceVendor::StepFun => parse_stepfun(body),
        BalanceVendor::SiliconFlowCn => parse_siliconflow(body, "CNY"),
        BalanceVendor::SiliconFlowCom => parse_siliconflow(body, "USD"),
        BalanceVendor::OpenRouter => parse_openrouter(body),
        BalanceVendor::Novita => parse_novita(body),
    }
}

// ===================================================================
// Query
// ===================================================================

/// Fetch the provider's balance. Never fails: an unrecognised host, a
/// missing key, a dead network and a moved response shape are all
/// states in the result.
pub async fn fetch_balance(p: &Provider) -> ProviderBalance {
    // Vendor first, key second: with an unknown host the key is
    // irrelevant, and `Unsupported` is the state that hides the UI.
    let Some(vendor) = detect_vendor(&p.base_url) else {
        return ProviderBalance::new(&p.id, BalanceStatus::Unsupported);
    };
    let api_key = p.api_key.trim();
    if api_key.is_empty() {
        return ProviderBalance::new(&p.id, BalanceStatus::NoKey);
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("Termory/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            return ProviderBalance::failed(
                &p.id,
                BalanceStatus::Error,
                format!("HTTP client init failed: {err}"),
            )
        }
    };

    let resp = match client
        .get(vendor.endpoint())
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            return ProviderBalance::failed(
                &p.id,
                BalanceStatus::Error,
                format!("Network error: {err}"),
            )
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        let body = resp.text().await.unwrap_or_default();
        return ProviderBalance::failed(
            &p.id,
            BalanceStatus::AuthFailed,
            http_error_message(status, &body),
        );
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return ProviderBalance::failed(
            &p.id,
            BalanceStatus::Error,
            http_error_message(status, &body),
        );
    }

    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(err) => {
            return ProviderBalance::failed(
                &p.id,
                BalanceStatus::Error,
                format!("Failed to parse API response: {err}"),
            )
        }
    };

    let entries = parse_body(vendor, &body);
    if entries.is_empty() {
        // 2xx with no readable amount: the response shape moved, or the
        // endpoint answered for a different kind of account. Reporting a
        // zero balance here would be a confident lie (see module docs).
        return ProviderBalance::failed(
            &p.id,
            BalanceStatus::Error,
            "No balance found in the response".to_string(),
        );
    }

    ProviderBalance {
        entries,
        ..ProviderBalance::new(&p.id, BalanceStatus::Ok)
    }
}

// ===================================================================
// Tests
// ===================================================================
//
// Parsing + detection only. The live fetches are untested by the same
// policy as quota.rs / updates.rs: they need a real key and a real
// vendor account, and a mock server would only assert our own stub.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_vendor_reads_the_host_through_any_path_suffix() {
        let cases = [
            ("https://api.deepseek.com", BalanceVendor::DeepSeek),
            ("https://api.deepseek.com/v1", BalanceVendor::DeepSeek),
            (
                "https://api.deepseek.com/v1/chat/completions",
                BalanceVendor::DeepSeek,
            ),
            ("HTTPS://API.DEEPSEEK.COM/v1/", BalanceVendor::DeepSeek),
            ("https://api.stepfun.com/v1", BalanceVendor::StepFun),
            // The `.ai` host is recognised even though the query goes to `.com`.
            ("https://api.stepfun.ai/v1", BalanceVendor::StepFun),
            (
                "https://api.siliconflow.cn/v1",
                BalanceVendor::SiliconFlowCn,
            ),
            (
                "https://api.siliconflow.com/v1",
                BalanceVendor::SiliconFlowCom,
            ),
            ("https://openrouter.ai/api/v1", BalanceVendor::OpenRouter),
            ("https://api.novita.ai/v3/openai", BalanceVendor::Novita),
        ];
        for (url, want) in cases {
            assert_eq!(detect_vendor(url), Some(want), "{url}");
        }
    }

    #[test]
    fn detect_vendor_declines_relays_and_gateways() {
        // The common case by far: a third-party relay or the user's own
        // gateway. It must read as "not supported", never as a vendor.
        for url in [
            "",
            "https://api.openai.com/v1",
            "https://my-relay.example.com/v1",
            // A relay that PROXIES deepseek but is not deepseek — its key
            // would be rejected by api.deepseek.com.
            "https://relay.example.com/deepseek/v1",
            "https://api.anthropic.com",
        ] {
            assert_eq!(detect_vendor(url), None, "{url}");
        }
    }

    #[test]
    fn detect_vendor_ignores_a_vendor_name_in_the_path() {
        // Routing a vendor behind a path prefix is a real relay
        // convention, and cc-switch's whole-URL `contains` takes each of
        // these for the vendor itself — then sends the relay's key to the
        // vendor's own domain. Matching the host only is what stops it.
        for url in [
            "https://relay.example.com/openrouter.ai/v1",
            "https://gw.example.com/api.deepseek.com/v1",
            "https://gw.example.com/v1?upstream=api.novita.ai",
            "https://gw.example.com/v1#api.siliconflow.cn",
        ] {
            assert_eq!(detect_vendor(url), None, "{url}");
        }
    }

    #[test]
    fn host_of_strips_scheme_userinfo_port_and_path() {
        assert_eq!(host_of("https://api.deepseek.com/v1"), "api.deepseek.com");
        assert_eq!(host_of("  HTTPS://API.Deepseek.com  "), "api.deepseek.com");
        assert_eq!(host_of("api.deepseek.com/v1"), "api.deepseek.com");
        assert_eq!(
            host_of("https://api.deepseek.com:8443/v1"),
            "api.deepseek.com"
        );
        assert_eq!(
            host_of("https://u:p@api.deepseek.com/v1"),
            "api.deepseek.com"
        );
        assert_eq!(host_of(""), "");
    }

    #[test]
    fn siliconflow_sites_are_separate_vendors_with_separate_currencies() {
        // Same brand, different account + different currency, so the
        // `.cn`-before-`.com` order in detect_vendor is behaviour.
        assert_eq!(
            detect_vendor("https://api.siliconflow.cn/v1"),
            Some(BalanceVendor::SiliconFlowCn)
        );
        let body = json!({ "code": 20000, "data": { "totalBalance": "12.5" } });
        assert_eq!(parse_siliconflow(&body, "CNY")[0].currency, "CNY");
        assert_eq!(parse_siliconflow(&body, "USD")[0].currency, "USD");
    }

    #[test]
    fn parse_deepseek_reads_every_currency_and_the_availability_flag() {
        let body = json!({
            "is_available": true,
            "balance_infos": [
                { "currency": "CNY", "total_balance": "48.20",
                  "granted_balance": "0.00", "topped_up_balance": "48.20" },
                { "currency": "USD", "total_balance": 6.5 },
            ]
        });
        let entries = parse_deepseek(&body);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].currency, "CNY");
        assert_eq!(entries[0].remaining, 48.20);
        assert!(!entries[0].depleted);
        assert_eq!(entries[1].currency, "USD");
        assert_eq!(entries[1].remaining, 6.5);
    }

    #[test]
    fn deepseek_is_available_false_marks_a_non_zero_balance_depleted() {
        // The vendor's own flag wins: an account can hold a balance and
        // still be blocked, which `remaining > 0` alone cannot express.
        let body = json!({
            "is_available": false,
            "balance_infos": [{ "currency": "CNY", "total_balance": 10.0 }]
        });
        let entries = parse_deepseek(&body);
        assert_eq!(entries[0].remaining, 10.0);
        assert!(entries[0].depleted);
    }

    #[test]
    fn parse_stepfun_reads_the_spendable_balance() {
        let body = json!({
            "object": "account", "balance": 128.0,
            "total_cash_balance": 100.0, "total_voucher_balance": 28.0
        });
        let entries = parse_stepfun(&body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].remaining, 128.0);
        assert_eq!(entries[0].currency, "CNY");
    }

    #[test]
    fn parse_openrouter_reports_remaining_as_granted_minus_spent() {
        let body = json!({ "data": { "total_credits": 25.0, "total_usage": 4.25 } });
        let entries = parse_openrouter(&body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].remaining, 20.75);
        assert_eq!(entries[0].total, Some(25.0));
        assert_eq!(entries[0].used, Some(4.25));
        assert!(!entries[0].depleted);

        // Spent past the grant → depleted, and the negative remaining is
        // reported verbatim rather than clamped.
        let over = json!({ "data": { "total_credits": 5.0, "total_usage": 5.5 } });
        let entries = parse_openrouter(&over);
        assert!((entries[0].remaining - -0.5).abs() < 1e-9);
        assert!(entries[0].depleted);
    }

    #[test]
    fn parse_openrouter_treats_a_missing_usage_as_zero_but_needs_the_grant() {
        // A fresh account really has no spend yet…
        let fresh = json!({ "data": { "total_credits": 10.0 } });
        assert_eq!(parse_openrouter(&fresh)[0].remaining, 10.0);
        // …but with no grant there is no anchor, so no entry (the caller
        // turns that into an error rather than a $0.00 balance).
        let shapeless = json!({ "data": { "something_else": 1 } });
        assert!(parse_openrouter(&shapeless).is_empty());
    }

    #[test]
    fn parse_novita_converts_the_minor_unit_to_usd() {
        // Novita reports 0.0001 USD units — 123_456 is $12.3456, and
        // skipping the divide would report $123,456.
        let body = json!({ "availableBalance": "123456", "creditLimit": 0 });
        let entries = parse_novita(&body);
        assert_eq!(entries.len(), 1);
        assert!((entries[0].remaining - 12.3456).abs() < 1e-9);
        assert_eq!(entries[0].currency, "USD");
    }

    #[test]
    fn a_missing_amount_yields_no_entry_rather_than_a_zero_balance() {
        // Every parser, driven with a 2xx body that carries no amount:
        // the caller turns an empty vec into an Error, so a moved
        // response shape can never render as "$0.00".
        assert!(parse_deepseek(&json!({ "is_available": true })).is_empty());
        assert!(parse_deepseek(&json!({ "balance_infos": [{ "currency": "CNY" }] })).is_empty());
        assert!(parse_stepfun(&json!({ "object": "account" })).is_empty());
        assert!(parse_siliconflow(&json!({ "code": 20000 }), "CNY").is_empty());
        assert!(parse_siliconflow(&json!({ "data": { "status": true } }), "CNY").is_empty());
        assert!(parse_openrouter(&json!({})).is_empty());
        assert!(parse_novita(&json!({ "cashBalance": 1 })).is_empty());
    }

    #[test]
    fn amounts_parse_from_both_the_number_and_the_string_form() {
        assert_eq!(amount(&json!({ "a": 1.5 }), "a"), Some(1.5));
        assert_eq!(amount(&json!({ "a": "1.5" }), "a"), Some(1.5));
        assert_eq!(amount(&json!({ "a": " 1.5 " }), "a"), Some(1.5));
        assert_eq!(amount(&json!({ "a": "" }), "a"), None);
        assert_eq!(amount(&json!({ "a": null }), "a"), None);
        assert_eq!(amount(&json!({}), "a"), None);
    }

    #[test]
    fn balance_serializes_camel_case_and_omits_absent_fields() {
        let result = ProviderBalance {
            entries: vec![entry("USD", 12.5)],
            ..ProviderBalance::new("p1", BalanceStatus::Ok)
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["providerId"], "p1");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["entries"][0]["remaining"], 12.5);
        assert_eq!(json["entries"][0]["depleted"], false);
        // Absent optionals stay out of the payload entirely.
        assert!(json.get("error").is_none());
        assert!(json["entries"][0].get("total").is_none());

        // An unsupported host carries no vendor and no entries.
        let none = ProviderBalance::new("p2", BalanceStatus::Unsupported);
        let json = serde_json::to_value(&none).unwrap();
        assert_eq!(json["status"], "unsupported");
        assert!(json.get("entries").is_none());
    }
}
