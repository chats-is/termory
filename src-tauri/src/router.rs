//! Local unified API router.
//!
//! One loopback HTTP server that pools every credential Termory already
//! knows about — the live Claude Code / Codex OAuth logins, the saved
//! account snapshots (`accounts.json`), the custom providers and the
//! gateways (`providers.json`) — behind the standard vendor endpoints
//! (`/v1/messages`, `/v1/responses`, `/v1/chat/completions`, `/v1beta/*`),
//! with automatic failover between them. A CLI is pointed at it the same
//! way it is pointed at any third-party API: as a custom provider whose
//! base URL is `http://127.0.0.1:<port>`.
//!
//! Design rules:
//!
//! - **Pass-through, per protocol.** A request is forwarded VERBATIM to an
//!   upstream that speaks the same API; the router never translates between
//!   Anthropic / OpenAI / Gemini shapes. That is what makes a Claude Code
//!   request against a Claude OAuth token indistinguishable from the CLI's
//!   own — the body, including the system prompt the token is gated on, is
//!   whatever the client sent.
//! - **Credentials are resolved at REQUEST time**, from the same stores the
//!   rest of Termory reads, so a re-login, an account switch or a provider
//!   edit is picked up on the next request with no restart.
//! - **The router NEVER refreshes an OAuth token.** It is a read-only consumer
//!   bound by the same never-write-credentials rule as `quota.rs`: refreshing
//!   spends a rotating refresh token it has nowhere to persist, which logs the
//!   CLI out. An expired token makes that upstream *unavailable* until the
//!   CLI itself refreshes it.
//! - **Failover happens before the first byte only.** Once an upstream has
//!   answered 2xx its body is streamed straight through; a stream that dies
//!   mid-way is the client's to retry, as with any direct API call.
//! - **Nothing sensitive is logged**: upstream key, status code and error
//!   class only — never a header, a token or a body.

use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use tauri::Emitter;

use crate::providers::{CliApp, ProviderKind};

const ROUTER_FILE_NAME: &str = "router.json";
pub const DEFAULT_PORT: u16 = 8317;
/// Emitted whenever the runtime status changes (start / stop / a request
/// completed) so an open Router page can re-read `status()`.
pub const ROUTER_CHANGED_EVENT: &str = "termory:router-changed";
/// Largest request body the router buffers. Buffering is what makes retry
/// possible — the same bytes go to the next upstream — and Anthropic's own
/// request cap is 32 MB, so nothing legitimate exceeds this.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle gap between two streamed chunks before the upstream is abandoned.
/// A thinking model can sit silent for minutes, so this is generous.
const READ_TIMEOUT: Duration = Duration::from_secs(600);

const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Grok Build's own backend for an OAuth login (`xai-grok-shell-base/src/util`
/// `is_cli_chat_proxy_url`): a standard-shaped API serving Chat Completions,
/// Responses AND Anthropic Messages (`ApiBackend` in `xai-grok-sampling-types`).
const GROK_CLI_CHAT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
/// The three wire APIs a Grok login serves.
const GROK_PROTOCOLS: [Protocol; 3] = [
    Protocol::OpenaiChat,
    Protocol::OpenaiResponses,
    Protocol::Anthropic,
];

// ===================================================================
// Config
// ===================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Always the first healthy upstream in list order; the rest are spares.
    Failover,
    /// Rotate the starting point per request; failover still applies.
    RoundRobin,
}

impl Default for Strategy {
    fn default() -> Self {
        Strategy::Failover
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamPref {
    pub key: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterConfig {
    /// Start the server with the app.
    #[serde(default)]
    pub autostart: bool,
    /// The address the listener binds. `127.0.0.1` (the default) keeps the
    /// router on this machine; `0.0.0.0` / `::` or one interface's IP opens
    /// it to the LAN — which REQUIRES an API key (`validate_exposure`).
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Key clients must present (`Authorization: Bearer` / `x-api-key` /
    /// `x-goog-api-key` / `?key=`). Empty = nobody can use the router.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub strategy: Strategy,
    /// Ordered preferences. Order IS priority; candidates missing from this
    /// list are appended disabled.
    #[serde(default)]
    pub upstreams: Vec<UpstreamPref>,
    /// Binding ids that were IN USE when the router was stopped and were
    /// deactivated then; `start()` puts them back in use. A binding is only
    /// ever live while the listener is (the activation guard), so stopping
    /// must hand the CLIs back to Official rather than leave them pointed
    /// at a closed port.
    #[serde(default)]
    pub suspended_bindings: Vec<String>,
    /// The multi-slot (OpenCode / Grok) ids among `suspended_bindings` that
    /// were the CLI's DEFAULT when suspended; restore re-sets only those.
    #[serde(default)]
    pub suspended_defaults: Vec<String>,
    /// A quit switched Codex to Official without re-tagging its sessions
    /// (`CodexFollow::Defer`); the next launch settles it.
    #[serde(default)]
    pub pending_codex_follow: bool,
    /// Ports the router listened on before the current one (newest first,
    /// a few). A CLI left on an old port by an unclean exit is still
    /// recognised as the router's (`is_router_url`); any other local proxy
    /// is not.
    #[serde(default)]
    pub former_ports: Vec<u16>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_host() -> String {
    DEFAULT_HOST.to_string()
}

pub const DEFAULT_HOST: &str = "127.0.0.1";

/// The configured bind address; an unparsable value (hand-edited file) falls
/// back to loopback — never to an open address.
pub fn bind_ip(cfg: &RouterConfig) -> std::net::IpAddr {
    cfg.host
        .trim()
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// Whether a bind address reaches beyond this machine.
fn is_exposed(ip: std::net::IpAddr) -> bool {
    !ip.is_loopback()
}

/// The host THIS machine's CLIs use to reach the router: loopback whenever
/// the listener accepts it (a loopback or wildcard bind), else the one
/// interface it is bound to. IPv6 is bracketed for URLs.
fn client_host(ip: std::net::IpAddr) -> String {
    if ip.is_loopback() || ip.is_unspecified() {
        return DEFAULT_HOST.to_string();
    }
    match ip {
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    }
}

/// The router's base URL for this machine's CLIs.
pub fn router_base_url(ip: std::net::IpAddr, port: u16) -> String {
    format!("http://{}:{port}", client_host(ip))
}

/// The URL another device on the LAN uses, when the router is exposed: the
/// bound interface's IP, or — for a wildcard bind — this machine's primary
/// LAN address. Found by "connecting" a UDP socket (which sends nothing) and
/// reading the local address the OS picked; `None` when offline.
fn lan_url(ip: std::net::IpAddr, port: u16) -> Option<String> {
    if !is_exposed(ip) {
        return None;
    }
    let lan = if ip.is_unspecified() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.connect("192.0.2.1:9").ok()?; // TEST-NET-1: never routed, nothing sent
        sock.local_addr().ok()?.ip()
    } else {
        ip
    };
    if lan.is_loopback() || lan.is_unspecified() {
        return None;
    }
    Some(match lan {
        std::net::IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        v4 => format!("http://{v4}:{port}"),
    })
}

/// An address open beyond this machine must not serve without a key: every
/// pooled login and API key would be usable by anyone on the network.
fn validate_exposure(cfg: &RouterConfig) -> Result<(), String> {
    if is_exposed(bind_ip(cfg)) && cfg.api_key.trim().is_empty() {
        return Err(
            "A listen address other than 127.0.0.1 requires an API key — generate one first."
                .to_string(),
        );
    }
    Ok(())
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            autostart: false,
            host: default_host(),
            port: DEFAULT_PORT,
            api_key: String::new(),
            strategy: Strategy::Failover,
            upstreams: Vec::new(),
            suspended_bindings: Vec::new(),
            suspended_defaults: Vec::new(),
            pending_codex_follow: false,
            former_ports: Vec::new(),
        }
    }
}

fn config_path() -> Result<std::path::PathBuf, Box<dyn Error>> {
    let home = crate::home_dir().ok_or("home directory not available")?;
    Ok(home.join(".termory").join(ROUTER_FILE_NAME))
}

/// Read `~/.termory/router.json`, defaulting every missing field. A
/// syntactically valid document never errors (same lenient rule as the
/// providers file) — an unknown `strategy` from a newer build falls back
/// to the default rather than failing the read.
pub fn read_config() -> Result<RouterConfig, Box<dyn Error>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(RouterConfig::default());
    }
    let text = std::fs::read_to_string(&path)?;
    if text.trim().is_empty() {
        return Ok(RouterConfig::default());
    }
    let raw: JsonValue = serde_json::from_str(&text)?;
    Ok(config_from_json(raw))
}

fn config_from_json(raw: JsonValue) -> RouterConfig {
    let mut cfg = RouterConfig::default();
    let JsonValue::Object(map) = raw else {
        return cfg;
    };
    if let Some(b) = map.get("autostart").and_then(|v| v.as_bool()) {
        cfg.autostart = b;
    }
    if let Some(h) = map.get("host").and_then(|v| v.as_str()) {
        // Unparsable → the loopback default (never an open address).
        if h.trim().parse::<std::net::IpAddr>().is_ok() {
            cfg.host = h.trim().to_string();
        }
    }
    if let Some(p) = map.get("port").and_then(|v| v.as_u64()) {
        if (1..=65535).contains(&p) {
            cfg.port = p as u16;
        }
    }
    if let Some(k) = map.get("apiKey").and_then(|v| v.as_str()) {
        cfg.api_key = k.to_string();
    }
    if let Some(s) = map.get("strategy") {
        if let Ok(s) = serde_json::from_value::<Strategy>(s.clone()) {
            cfg.strategy = s;
        }
    }
    if let Some(JsonValue::Array(list)) = map.get("upstreams") {
        cfg.upstreams = list
            .iter()
            .filter_map(|v| serde_json::from_value::<UpstreamPref>(v.clone()).ok())
            .filter(|p| !p.key.is_empty())
            .collect();
    }
    let strings = |key: &str| -> Vec<String> {
        match map.get(key) {
            Some(JsonValue::Array(list)) => list
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    };
    cfg.suspended_bindings = strings("suspendedBindings");
    cfg.suspended_defaults = strings("suspendedDefaults");
    if let Some(b) = map.get("pendingCodexFollow").and_then(|v| v.as_bool()) {
        cfg.pending_codex_follow = b;
    }
    if let Some(JsonValue::Array(list)) = map.get("formerPorts") {
        cfg.former_ports = list
            .iter()
            .filter_map(|v| v.as_u64())
            .filter(|p| (1..=65535).contains(p))
            .map(|p| p as u16)
            .collect();
    }
    cfg
}

pub fn write_config(cfg: &RouterConfig) -> Result<(), Box<dyn Error>> {
    let path = config_path()?;
    // A file that exists but does not parse is NOT replaced: it may be a
    // torn write with the user's key and order still inside. Lenient
    // parsing covers valid JSON with unknown values, not this.
    if path.exists() {
        read_config().map_err(|e| format!("router.json is unreadable — fix or delete it: {e}"))?;
    }
    let value = serde_json::to_value(cfg)?;
    crate::accounts::atomic_write_0600(&path, serde_json::to_string_pretty(&value)?.as_bytes())?;
    *config_cache() = Some(cfg.clone());
    Ok(())
}

/// Read-modify-write router.json under ONE process-wide lock, starting from
/// the FILE (not the cache), so two writers — the page's patch, a suspend,
/// a restore, a key generation — never overwrite each other's fields with a
/// stale copy. `f` may refuse with an error; nothing is written then.
pub fn update_config(
    f: impl FnOnce(&mut RouterConfig) -> Result<(), String>,
) -> Result<RouterConfig, Box<dyn Error>> {
    static RMW: Mutex<()> = Mutex::new(());
    let _g = RMW.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = read_config()?;
    f(&mut cfg)?;
    write_config(&cfg)?;
    Ok(cfg)
}

/// Serializes the router's lifecycle transitions — start, stop, port/key
/// changes, suspend and restore — so they never interleave: a restore racing
/// a quick Stop, or two starts each replacing the other's listener.
fn lifecycle() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    &LOCK
}

/// The server reads the config per request from here, so an enable toggle
/// or a reorder applies to the very next request without a restart.
fn config_cache() -> std::sync::MutexGuard<'static, Option<RouterConfig>> {
    static CACHE: Mutex<Option<RouterConfig>> = Mutex::new(None);
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

fn current_config() -> RouterConfig {
    if let Some(c) = config_cache().as_ref() {
        return c.clone();
    }
    match read_config() {
        Ok(cfg) => {
            *config_cache() = Some(cfg.clone());
            cfg
        }
        // Serve defaults for this call but do NOT cache them: caching would
        // let the next write persist defaults over the unreadable file.
        Err(err) => {
            log::warn!("router config unreadable, using defaults: {err}");
            RouterConfig::default()
        }
    }
}

/// 32 bytes from the OS's secure random source (`getrandom`: `getentropy`
/// on macOS, `getrandom(2)` on Linux, `ProcessPrng` on Windows). A failing
/// source is not papered over with a weaker one: these bytes are a router
/// key, the only guard once the router is open to the LAN.
fn os_random_32() -> [u8; 32] {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("the OS random number generator is unavailable");
    buf
}

/// A fresh client key: 32 random bytes as hex under an `sk-` prefix (the
/// form every OpenAI-style client accepts as-is).
pub fn generate_api_key() -> String {
    let hex: String = os_random_32().iter().map(|b| format!("{b:02x}")).collect();
    format!("sk-{hex}")
}

/// A random UUID v4, from the same OS source.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&os_random_32()[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

// ===================================================================
// Protocols and upstream candidates
// ===================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// Anthropic Messages — `/v1/messages`.
    Anthropic,
    /// OpenAI Responses — `/v1/responses`. What Codex speaks.
    OpenaiResponses,
    /// OpenAI Chat Completions — `/v1/chat/completions`.
    OpenaiChat,
    /// Gemini API — `/v1beta/models/...`.
    Gemini,
}

/// Which protocol a client path belongs to. `None` = not a routed API
/// path (answered locally or 404).
pub fn classify_path(path: &str) -> Option<Protocol> {
    if path.starts_with("/v1/messages") {
        return Some(Protocol::Anthropic);
    }
    if path.starts_with("/v1/responses") {
        return Some(Protocol::OpenaiResponses);
    }
    if path.starts_with("/v1/chat/completions")
        || path.starts_with("/v1/completions")
        || path.starts_with("/v1/embeddings")
    {
        return Some(Protocol::OpenaiChat);
    }
    if path.starts_with("/v1beta/") || (path.starts_with("/v1/models/") && path.contains(':')) {
        return Some(Protocol::Gemini);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateKind {
    /// The CLI's current login, read from its own credential store.
    Live,
    /// A saved snapshot in `accounts.json`.
    Account,
    /// A custom provider in `providers.json`.
    Provider,
    /// A gateway in `providers.json`.
    Gateway,
}

/// One pool member as the page lists it. `key` is the stable identity the
/// config refers to (`live:codex`, `account:claude:<id>`, `provider:<id>`,
/// `gateway:<id>`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub key: String,
    pub kind: CandidateKind,
    pub app: Option<CliApp>,
    pub label: String,
    /// Secondary line: an email for a login, a masked host for an API.
    pub detail: String,
    pub protocols: Vec<Protocol>,
    /// Whether a request could be sent through it right now (a credential
    /// exists and is not expired). `reason` says why not.
    pub available: bool,
    pub reason: Option<String>,
    /// A stable code for `reason` when it is one of the known cases, so the
    /// page can translate it (the raw text is the fallback).
    pub reason_code: Option<&'static str>,
    pub enabled: bool,
}

/// How an upstream authenticates, resolved at request time.
#[derive(Debug, Clone)]
pub enum Auth {
    /// `Authorization: Bearer` + `chatgpt-account-id`.
    ChatGpt {
        token: String,
        account_id: Option<String>,
    },
    /// `Authorization: Bearer` + Grok Build's proxy headers
    /// (`X-XAI-Token-Auth`, user id, client version/identifier/mode).
    GrokOauth {
        token: String,
        user_id: String,
        email: Option<String>,
    },
    /// A plain API key: `Authorization: Bearer` and `x-api-key` for the
    /// Anthropic/OpenAI shapes, `x-goog-api-key` for Gemini.
    ApiKey(String),
}

#[derive(Debug, Clone)]
pub struct Upstream {
    /// The API this upstream is spoken to in for THIS request: the client's
    /// own when the upstream serves it (pass-through), else a format a
    /// translator connects (`resolve_for_client`).
    protocol: Protocol,
    base_url: String,
    auth: Auth,
    /// The OAuth login behind this upstream when the router can refresh it
    /// (Codex, Grok); `None` for API keys and for Claude logins.
    login: Option<crate::accounts::RouterLogin>,
    /// Access-token expiry (unix seconds) when known.
    expires_at: Option<i64>,
}

/// Enumerate every pool member, ordered by the config's preference list
/// (config order = priority), then unknown candidates disabled.
pub fn list_candidates(cfg: &RouterConfig) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    found.extend(live_candidates());
    found.extend(account_candidates());
    found.extend(provider_candidates());
    found.extend(gateway_candidates());
    // Settings → Tools: a switched-off tool's logins, accounts and providers
    // are hidden here AND refused by routing (`resolve_upstream_with`), the
    // same one-rule-both-sides shape the session scan applies. Tool-neutral
    // gateways have no switch.
    let disabled = crate::config::disabled_sources();
    found.retain(|c| c.app.is_none_or(|a| !disabled.contains(a.key())));
    for c in &mut found {
        c.reason_code = c.reason.as_deref().and_then(reason_code);
    }
    order_candidates(found, cfg)
}

/// The known unavailability reasons as stable codes the page translates
/// (`router.reason.*`); anything else is shown as the raw text.
fn reason_code(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "not logged in" => "not_logged_in",
        "needs re-login" => "needs_relogin",
        "snapshot has no payload" => "no_payload",
        "no base URL" => "no_base_url",
        "no API key" => "no_api_key",
        "no detected API" => "no_detected_api",
        TOOL_DISABLED => "tool_disabled",
        r if r.starts_with("token expired") => "token_expired",
        _ => return None,
    })
}

fn order_candidates(found: Vec<Candidate>, cfg: &RouterConfig) -> Vec<Candidate> {
    order_candidates_with(found, cfg, &tool_order())
}

/// The list is GROUPED BY TOOL, in the Settings → Tools order (tool-neutral
/// gateways last). Inside a group the user's saved order holds; entries the
/// user has not placed yet follow, in the order the source lists them
/// (live login, then saved accounts, then providers.json order) — the same
/// order the Providers page shows. Priority only competes within a
/// protocol, so ordering across tools carries no meaning to lose.
fn order_candidates_with(
    found: Vec<Candidate>,
    cfg: &RouterConfig,
    tools: &[CliApp],
) -> Vec<Candidate> {
    // Enumeration index = the source list's own order.
    let mut by_key: HashMap<String, (usize, Candidate)> = found
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c.key.clone(), (i, c)))
        .collect();
    let mut out: Vec<Candidate> = Vec::with_capacity(by_key.len());
    for pref in &cfg.upstreams {
        if let Some((_, mut c)) = by_key.remove(&pref.key) {
            c.enabled = pref.enabled;
            out.push(c);
        }
    }
    let mut rest: Vec<(usize, Candidate)> = by_key
        .into_values()
        .map(|(i, mut c)| {
            c.enabled = false;
            (i, c)
        })
        .collect();
    rest.sort_by_key(|(i, _)| *i);
    out.extend(rest.into_iter().map(|(_, c)| c));
    // Stable: the within-group order just built survives the grouping.
    out.sort_by_key(|c| tool_rank(c.app, tools));
    out
}

fn tool_rank(app: Option<CliApp>, tools: &[CliApp]) -> usize {
    match app {
        Some(a) => tools.iter().position(|t| *t == a).unwrap_or(tools.len()),
        None => tools.len() + 1,
    }
}

/// Settings → Tools order (`source_order` in config.json, a frontend-owned
/// key mirrored here read-only), falling back to the built-in order.
fn tool_order() -> Vec<CliApp> {
    const DEFAULT: [CliApp; 6] = [
        CliApp::Claude,
        CliApp::ClaudeDesktop,
        CliApp::Codex,
        CliApp::Gemini,
        CliApp::Opencode,
        CliApp::Grok,
    ];
    let mut order: Vec<CliApp> = crate::config::read_config()
        .ok()
        .and_then(|v| v.get("source_order").cloned())
        .and_then(|v| v.as_array().cloned())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().and_then(CliApp::parse))
                .collect()
        })
        .unwrap_or_default();
    for app in DEFAULT {
        if !order.contains(&app) {
            order.push(app);
        }
    }
    order
}

fn live_candidates() -> Vec<Candidate> {
    let mut out = Vec::new();
    // Codex
    {
        let (available, reason, email) = match read_codex_live_doc() {
            Some(doc) => {
                let (ok, why) = codex_oauth_status(&doc);
                (ok, why, codex_doc_email(&doc))
            }
            None => (false, Some("not logged in".to_string()), None),
        };
        out.push(Candidate {
            key: "live:codex".into(),
            kind: CandidateKind::Live,
            app: Some(CliApp::Codex),
            label: "Codex".into(),
            detail: email.unwrap_or_default(),
            protocols: vec![Protocol::OpenaiResponses],
            available,
            reason,
            reason_code: None,
            enabled: false,
        });
    }
    // Grok Build
    {
        let (available, reason, email) = match read_grok_live_entry() {
            Some(entry) => {
                let email = entry
                    .get("email")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let (ok, why) = grok_oauth_status(&entry);
                (ok, why, email)
            }
            None => (false, Some("not logged in".to_string()), None),
        };
        out.push(Candidate {
            key: "live:grok".into(),
            kind: CandidateKind::Live,
            app: Some(CliApp::Grok),
            label: "Grok Build".into(),
            detail: email.unwrap_or_default(),
            protocols: GROK_PROTOCOLS.to_vec(),
            available,
            reason,
            reason_code: None,
            enabled: false,
        });
    }
    out
}

fn account_candidates() -> Vec<Candidate> {
    let entries = crate::accounts::read_store().unwrap_or_default();
    let mut out = Vec::new();
    for e in &entries {
        let (Some(app), Some(id)) = (
            e.get("app")
                .and_then(|v| v.as_str())
                .and_then(CliApp::parse),
            e.get("id").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let protocols: Vec<Protocol> = match app {
            CliApp::Codex => vec![Protocol::OpenaiResponses],
            CliApp::Grok => GROK_PROTOCOLS.to_vec(),
            _ => continue,
        };
        let needs_relogin = e
            .get("needsRelogin")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let (available, reason) = if needs_relogin {
            (false, Some("needs re-login".to_string()))
        } else {
            let payload = e.get("payload");
            match app {
                CliApp::Codex => payload
                    .map(codex_oauth_status)
                    .unwrap_or((false, Some("snapshot has no payload".into()))),
                CliApp::Grok => payload
                    .and_then(|p| p.get("auth"))
                    .map(grok_oauth_status)
                    .unwrap_or((false, Some("snapshot has no payload".into()))),
                _ => match account_auth(app, e) {
                    Ok(_) => (true, None),
                    Err(why) => (false, Some(why)),
                },
            }
        };
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        let email = e.get("email").and_then(|v| v.as_str()).unwrap_or("");
        out.push(Candidate {
            key: format!("account:{}:{}", app.key(), id),
            kind: CandidateKind::Account,
            app: Some(app),
            label: if name.is_empty() {
                email.to_string()
            } else {
                name.to_string()
            },
            detail: if name.is_empty() {
                String::new()
            } else {
                email.to_string()
            },
            protocols,
            available,
            reason,
            reason_code: None,
            enabled: false,
        });
    }
    out
}

/// Which protocol a per-CLI custom provider speaks — the wire API that CLI
/// uses against a third-party base URL.
fn provider_protocol(app: CliApp) -> Protocol {
    match app {
        CliApp::Claude | CliApp::ClaudeDesktop => Protocol::Anthropic,
        CliApp::Codex => Protocol::OpenaiResponses,
        CliApp::Gemini => Protocol::Gemini,
        CliApp::Opencode | CliApp::Grok => Protocol::OpenaiChat,
    }
}

/// The API a custom provider is actually spoken to in — the same rule
/// Termory applies when it writes the provider into its CLI: an OpenCode
/// provider's AI-SDK package (`npm`) decides it, a Grok provider's
/// `api_backend` does; every other tool's provider speaks its tool's API.
fn provider_wire_protocol(p: &crate::providers::Provider) -> Protocol {
    use crate::providers::GatewayProtocol;
    match p.app {
        CliApp::Opencode => {
            match crate::providers::protocol_for_npm(p.npm.as_deref().unwrap_or("")) {
                GatewayProtocol::Anthropic => Protocol::Anthropic,
                GatewayProtocol::Gemini => Protocol::Gemini,
                GatewayProtocol::Openai => Protocol::OpenaiResponses,
                GatewayProtocol::OpenaiCompatible => Protocol::OpenaiChat,
            }
        }
        CliApp::Grok => match p.api_backend.as_deref().map(str::trim) {
            Some("responses") => Protocol::OpenaiResponses,
            Some("messages") => Protocol::Anthropic,
            _ => Protocol::OpenaiChat,
        },
        app => provider_protocol(app),
    }
}

fn provider_candidates() -> Vec<Candidate> {
    let providers =
        crate::providers::providers_from_json(crate::config::read_providers().unwrap_or_default());
    providers
        .into_iter()
        .filter(|p| p.kind == ProviderKind::Custom)
        .map(|p| {
            let (available, reason) = if p.base_url.trim().is_empty() {
                (false, Some("no base URL".to_string()))
            } else if p.api_key.trim().is_empty() {
                (false, Some("no API key".to_string()))
            } else {
                (true, None)
            };
            Candidate {
                key: format!("provider:{}", p.id),
                kind: CandidateKind::Provider,
                app: Some(p.app),
                label: p.name.clone(),
                detail: host_of(&p.base_url),
                protocols: vec![provider_wire_protocol(&p)],
                available,
                reason,
                reason_code: None,
                enabled: false,
            }
        })
        .collect()
}

fn gateway_protocols(caps: Option<&JsonValue>) -> Vec<Protocol> {
    let mut out = Vec::new();
    let Some(caps) = caps else {
        return out;
    };
    let flag = |k: &str| caps.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    if flag("anthropic") {
        out.push(Protocol::Anthropic);
    }
    if flag("openai") {
        out.push(Protocol::OpenaiResponses);
    }
    if flag("openaiCompatible") {
        out.push(Protocol::OpenaiChat);
    }
    if flag("gemini") {
        out.push(Protocol::Gemini);
    }
    out
}

fn gateway_candidates() -> Vec<Candidate> {
    let raw = crate::config::read_gateways().unwrap_or_default();
    let JsonValue::Array(list) = raw else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|g| {
            // The router's own entry would be a loop.
            if is_router_entry(g) {
                return None;
            }
            let id = g.get("id").and_then(|v| v.as_str())?;
            let name = g.get("name").and_then(|v| v.as_str()).unwrap_or(id);
            let base = g.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("");
            let key = g.get("apiKey").and_then(|v| v.as_str()).unwrap_or("");
            let protocols = gateway_protocols(g.get("capabilities"));
            let (available, reason) = if base.trim().is_empty() {
                (false, Some("no base URL".to_string()))
            } else if key.trim().is_empty() {
                (false, Some("no API key".to_string()))
            } else if protocols.is_empty() {
                (false, Some("no detected API".to_string()))
            } else {
                (true, None)
            };
            Some(Candidate {
                key: format!("gateway:{id}"),
                kind: CandidateKind::Gateway,
                app: None,
                label: name.to_string(),
                detail: host_of(base),
                protocols,
                available,
                reason,
                reason_code: None,
                enabled: false,
            })
        })
        .collect()
}

fn host_of(url: &str) -> String {
    let s = url.trim();
    let s = s.split("://").nth(1).unwrap_or(s);
    s.split('/').next().unwrap_or("").to_string()
}

// ---- credential readers (read-only, never refresh) ----------------

/// Unix seconds now.
/// Whether a login's access token is already past its expiry (unknown
/// expiry counts as usable — the upstream will say otherwise).
fn token_expired(expires_at: Option<i64>) -> bool {
    expires_at.is_some_and(|e| e <= now_secs() as i64)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The live Codex `auth.json` document — the FILE, exactly what
/// `accounts::read_codex_live` (the Accounts page's notion of "live")
/// reads. Deliberately no Keychain probe: Codex's keyring entry is keyed
/// per `CODEX_HOME` (`login/src/auth/storage.rs` `compute_store_key`),
/// a plain `-s "Codex Auth"` lookup can return a login from another
/// store, and the two surfaces must agree on WHICH login is live.
fn read_codex_live_doc() -> Option<JsonValue> {
    let path = crate::providers::codex_auth_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Decode a JWT's payload without verifying it — only to read `exp` and
/// `email`, both informational here.
fn jwt_claims(token: &str) -> Option<JsonValue> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn codex_oauth_status(doc: &JsonValue) -> (bool, Option<String>) {
    match codex_oauth_auth(doc) {
        Ok(_) => refreshable_status(
            codex_access_exp(doc),
            doc.pointer("/tokens/refresh_token")
                .and_then(|v| v.as_str())
                .is_some_and(|t| !t.is_empty()),
            "codex",
        ),
        Err(e) => (false, Some(e)),
    }
}

fn codex_access_exp(doc: &JsonValue) -> Option<i64> {
    jwt_claims(doc.pointer("/tokens/access_token")?.as_str()?)?
        .get("exp")?
        .as_i64()
}

fn grok_key_exp(entry: &JsonValue) -> Option<i64> {
    jwt_claims(entry.get("key")?.as_str()?)?
        .get("exp")?
        .as_i64()
}

fn grok_oauth_status(entry: &JsonValue) -> (bool, Option<String>) {
    match grok_oauth_auth(entry) {
        Ok(_) => refreshable_status(
            grok_key_exp(entry),
            ["refresh_token", "oidc_client_id"].iter().all(|k| {
                entry
                    .get(*k)
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.is_empty())
            }),
            "grok",
        ),
        Err(e) => (false, Some(e)),
    }
}

/// A login the router can refresh stays usable while it has a refresh
/// token, even with its access token expired.
fn refreshable_status(exp: Option<i64>, can_refresh: bool, cli: &str) -> (bool, Option<String>) {
    let expired = exp.is_some_and(|e| e <= now_secs() as i64);
    if expired && !can_refresh {
        (false, Some(format!("token expired — run {cli} once")))
    } else {
        (true, None)
    }
}

fn codex_oauth_auth(doc: &JsonValue) -> Result<Auth, String> {
    let tokens = doc
        .get("tokens")
        .filter(|t| !t.is_null())
        .ok_or_else(|| "no ChatGPT login".to_string())?;
    let token = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "no ChatGPT login".to_string())?;
    // An expired access token is NOT an error here: the router refreshes a
    // Codex login before use (`accounts::refresh_login_for_router`).
    // `get_account_id` (codex-rs `login/src/auth/manager.rs`): the token
    // data's `account_id`, else the id_token's `chatgpt_account_id` claim.
    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            tokens
                .get("id_token")
                .and_then(|v| v.as_str())
                .and_then(jwt_claims)
                .and_then(|c| {
                    c.get("https://api.openai.com/auth")
                        .and_then(|a| a.get("chatgpt_account_id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
        });
    Ok(Auth::ChatGpt {
        token: token.to_string(),
        account_id,
    })
}

fn codex_doc_email(doc: &JsonValue) -> Option<String> {
    let id_token = doc.get("tokens")?.get("id_token")?.as_str()?;
    jwt_claims(id_token)?
        .get("email")?
        .as_str()
        .map(str::to_string)
}

/// The live Grok Build login: the first `auth.json` entry (a map keyed by
/// `{issuer}::{client_id}`) holding both `key` and `user_id` — the same rule
/// `quota.rs` applies. Read WITHOUT grok's lock (a read cannot tear the
/// JSON into something that parses wrong, and taking the lock stamps a
/// file inside the watched grok home).
fn read_grok_live_entry() -> Option<JsonValue> {
    let path = crate::providers::grok_home_dir()?.join("auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let doc: JsonValue = serde_json::from_str(&text).ok()?;
    grok_live_entry_in(&doc)
}

/// Grok's scope prefix for an xAI login — `accounts::GROK_XAI_SCOPE_PREFIX`.
const GROK_XAI_SCOPE: &str = "https://auth.x.ai::";

/// The live xAI login in a grok auth.json: the SAME rule the account code
/// uses (`accounts::grok_live_from_doc` — xAI scope prefix + `user_id`), so
/// the token the router SENDS is the one its refresh ROTATES. auth.json can
/// also hold an enterprise-OIDC login under another issuer, whose token
/// cli-chat-proxy.grok.com would reject and which no refresh here renews.
fn grok_live_entry_in(doc: &JsonValue) -> Option<JsonValue> {
    let nonempty = |v: &JsonValue, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .is_some_and(|x| !x.trim().is_empty())
    };
    doc.as_object()?
        .iter()
        .find(|(k, v)| {
            k.starts_with(GROK_XAI_SCOPE) && nonempty(v, "user_id") && nonempty(v, "key")
        })
        .map(|(_, v)| v.clone())
}

/// A Grok auth entry (`{key, user_id, email?, …}`) as an upstream auth. The
/// access token is a JWT; its `exp` decides (60 s slack, as grok itself).
/// Reading it never refreshes: that happens only through
/// `accounts::refresh_login_for_router`, which writes the rotated token back
/// under grok's lock (auth.x.ai has reuse detection, so a refresh with
/// nowhere to persist it logs grok out).
fn grok_oauth_auth(entry: &JsonValue) -> Result<Auth, String> {
    let token = entry
        .get("key")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "not logged in".to_string())?;
    let user_id = entry
        .get("user_id")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "not logged in".to_string())?;
    // Expiry is handled by the router's refresh, not rejected here.
    Ok(Auth::GrokOauth {
        token: token.to_string(),
        user_id: user_id.to_string(),
        email: entry
            .get("email")
            .and_then(|v| v.as_str())
            .filter(|e| !e.is_empty())
            .map(str::to_string),
    })
}

/// The auth an account snapshot yields, or why it cannot.
fn account_auth(app: CliApp, entry: &JsonValue) -> Result<Auth, String> {
    let payload = entry
        .get("payload")
        .ok_or_else(|| "snapshot has no payload".to_string())?;
    match app {
        CliApp::Codex => codex_oauth_auth(payload),
        // The snapshot is scope-scoped: `{scope, auth}`, `auth` being the
        // auth.json entry.
        CliApp::Grok => grok_oauth_auth(payload.get("auth").unwrap_or(payload)),
        _ => Err("unsupported".to_string()),
    }
}

const PROTOCOL_MISMATCH: &str = "protocol mismatch";

/// The stores a routing pass reads, loaded ONCE per request.
struct Stores {
    accounts: Vec<JsonValue>,
    providers: Vec<crate::providers::Provider>,
    gateways: Vec<JsonValue>,
    /// Settings → Tools switched-off keys.
    disabled: std::collections::HashSet<String>,
}

const TOOL_DISABLED: &str = "tool switched off in Settings";

impl Stores {
    fn load() -> Self {
        Stores {
            disabled: crate::config::disabled_sources(),
            accounts: crate::accounts::read_store().unwrap_or_default(),
            providers: crate::providers::providers_from_json(
                crate::config::read_providers().unwrap_or_default(),
            ),
            gateways: crate::config::read_gateways()
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
fn resolve_upstream(key: &str, protocol: Protocol) -> Result<Upstream, String> {
    resolve_upstream_with(&Stores::load(), key, protocol)
}

/// Resolve a pool key to a sendable upstream for `protocol`. `Err` = skip
/// this one (with the reason recorded in the health table).
fn resolve_upstream_with(
    stores: &Stores,
    key: &str,
    protocol: Protocol,
) -> Result<Upstream, String> {
    let mut parts = key.splitn(3, ':');
    let kind = parts.next().unwrap_or("");
    let tool_on = |app: CliApp| -> Result<(), String> {
        if stores.disabled.contains(app.key()) {
            Err(TOOL_DISABLED.to_string())
        } else {
            Ok(())
        }
    };
    match kind {
        "live" => match parts.next() {
            Some("codex") if protocol == Protocol::OpenaiResponses => {
                tool_on(CliApp::Codex)?;
                let doc = read_codex_live_doc().ok_or_else(|| "not logged in".to_string())?;
                Ok(Upstream {
                    protocol,
                    base_url: CHATGPT_CODEX_BASE_URL.to_string(),
                    auth: codex_oauth_auth(&doc)?,
                    login: Some(crate::accounts::RouterLogin::Live(CliApp::Codex)),
                    expires_at: codex_access_exp(&doc),
                })
            }
            Some("grok") if GROK_PROTOCOLS.contains(&protocol) => {
                tool_on(CliApp::Grok)?;
                let entry = read_grok_live_entry().ok_or_else(|| "not logged in".to_string())?;
                Ok(Upstream {
                    protocol,
                    base_url: GROK_CLI_CHAT_BASE_URL.to_string(),
                    auth: grok_oauth_auth(&entry)?,
                    login: Some(crate::accounts::RouterLogin::Live(CliApp::Grok)),
                    expires_at: grok_key_exp(&entry),
                })
            }
            _ => Err(PROTOCOL_MISMATCH.to_string()),
        },
        "account" => {
            let app = parts
                .next()
                .and_then(CliApp::parse)
                .ok_or_else(|| "unknown app".to_string())?;
            let id = parts.next().ok_or_else(|| "missing id".to_string())?;
            tool_on(app)?;
            let entry = stores
                .accounts
                .iter()
                .find(|e| {
                    e.get("id").and_then(|v| v.as_str()) == Some(id)
                        && e.get("app").and_then(|v| v.as_str()) == Some(app.key())
                })
                .ok_or_else(|| "account deleted".to_string())?;
            if entry
                .get("needsRelogin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Err("needs re-login".to_string());
            }
            let base_url = match (app, protocol) {
                (CliApp::Codex, Protocol::OpenaiResponses) => CHATGPT_CODEX_BASE_URL,
                (CliApp::Grok, p) if GROK_PROTOCOLS.contains(&p) => GROK_CLI_CHAT_BASE_URL,
                _ => return Err(PROTOCOL_MISMATCH.to_string()),
            };
            let refreshable = matches!(app, CliApp::Codex | CliApp::Grok);
            let payload = entry.get("payload");
            let expires_at = match app {
                CliApp::Codex => payload.and_then(codex_access_exp),
                CliApp::Grok => payload.and_then(|p| p.get("auth")).and_then(grok_key_exp),
                _ => None,
            };
            Ok(Upstream {
                protocol,
                base_url: base_url.to_string(),
                auth: account_auth(app, entry)?,
                login: refreshable.then(|| crate::accounts::RouterLogin::Saved {
                    app,
                    id: id.to_string(),
                }),
                expires_at,
            })
        }
        "provider" => {
            let id = parts.next().ok_or_else(|| "missing id".to_string())?;
            let p = stores
                .providers
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| "provider deleted".to_string())?;
            if provider_wire_protocol(p) != protocol {
                return Err(PROTOCOL_MISMATCH.to_string());
            }
            tool_on(p.app)?;
            if p.base_url.trim().is_empty() || p.api_key.trim().is_empty() {
                return Err("incomplete provider".to_string());
            }
            Ok(Upstream {
                protocol,
                base_url: p.base_url.trim().to_string(),
                auth: Auth::ApiKey(p.api_key.clone()),
                login: None,
                expires_at: None,
            })
        }
        "gateway" => {
            let id = parts.next().ok_or_else(|| "missing id".to_string())?;
            let g = stores
                .gateways
                .iter()
                .find(|g| g.get("id").and_then(|v| v.as_str()) == Some(id))
                .ok_or_else(|| "gateway deleted".to_string())?;
            if is_router_entry(g) {
                return Err("the router cannot be its own upstream".to_string());
            }
            if !gateway_protocols(g.get("capabilities")).contains(&protocol) {
                return Err(PROTOCOL_MISMATCH.to_string());
            }
            let base = g.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("");
            let api_key = g.get("apiKey").and_then(|v| v.as_str()).unwrap_or("");
            if base.trim().is_empty() || api_key.trim().is_empty() {
                return Err("incomplete gateway".to_string());
            }
            let anthropic_path = g
                .get("capabilities")
                .and_then(|c| c.get("anthropicPath"))
                .and_then(|v| v.as_str());
            Ok(Upstream {
                protocol,
                base_url: gateway_base(base, protocol, anthropic_path),
                auth: Auth::ApiKey(api_key.to_string()),
                login: None,
                expires_at: None,
            })
        }
        _ => Err("unknown upstream".to_string()),
    }
}

/// Resolve a pool key for a CLIENT speaking `client` — the CLIProxyAPI
/// model: any member that serves the model can serve any client. The
/// client's own format is used when the member speaks it (byte-for-byte
/// pass-through, the most faithful path); otherwise the first format in
/// `translate::targets_for(client)` the member speaks, and the request and
/// response are translated. `PROTOCOL_MISMATCH` only when no translator
/// connects the two.
fn resolve_for_client(stores: &Stores, key: &str, client: Protocol) -> Result<Upstream, String> {
    match resolve_upstream_with(stores, key, client) {
        Err(why) if why == PROTOCOL_MISMATCH => {}
        other => return other,
    }
    for &target in crate::translate::targets_for(client) {
        match resolve_upstream_with(stores, key, target) {
            Err(why) if why == PROTOCOL_MISMATCH => continue,
            other => return other,
        }
    }
    Err(PROTOCOL_MISMATCH.to_string())
}

/// A gateway's per-protocol root. Mirrors `providers::gateway_base_for_protocol`;
/// keep the two in sync.
fn gateway_base(base: &str, protocol: Protocol, anthropic_path: Option<&str>) -> String {
    let mut b = base.trim().trim_end_matches('/');
    b = b.strip_suffix("/v1beta").unwrap_or(b);
    b = b.strip_suffix("/v1").unwrap_or(b);
    match protocol {
        Protocol::Anthropic => {
            let sub = anthropic_path.unwrap_or("").trim_end_matches('/');
            if sub.is_empty() || b.ends_with(sub) {
                b.to_string()
            } else {
                format!("{b}{sub}")
            }
        }
        // `upstream_url` strips the client's `/v1` for these, so the base
        // must carry it — same as the CLI's own binding does.
        Protocol::OpenaiResponses | Protocol::OpenaiChat => format!("{b}/v1"),
        Protocol::Gemini => b.to_string(),
    }
}

/// Join a stored base URL with the client's request path the way that
/// CLI would: an OpenAI-flavoured base already carries `/v1` (Codex and
/// OpenCode append `/responses` / `/chat/completions` to it), while the
/// Anthropic and Gemini bases are bare roots the CLI appends `/v1/...` or
/// `/v1beta/...` to. ChatGPT's Codex backend has no `/v1` at all, so the
/// versioned prefix is dropped there as well.
pub fn upstream_url(base: &str, protocol: Protocol, path_and_query: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    match protocol {
        Protocol::Anthropic => {
            let b = base.strip_suffix("/v1").unwrap_or(base);
            format!("{b}{path_and_query}")
        }
        Protocol::Gemini => {
            let b = base.strip_suffix("/v1beta").unwrap_or(base);
            let b = b.strip_suffix("/v1").unwrap_or(b);
            format!("{b}{path_and_query}")
        }
        Protocol::OpenaiResponses | Protocol::OpenaiChat => {
            let rest = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
            format!("{base}{rest}")
        }
    }
}

// ===================================================================
// Health / failover state
// ===================================================================

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamHealth {
    pub key: String,
    pub requests: u64,
    pub failures: u64,
    /// Consecutive failures; reset by a success. Drives the cooldown length.
    pub streak: u32,
    pub last_error: Option<String>,
    /// Unix millis until which this upstream is skipped.
    pub cooldown_until: Option<u64>,
    pub last_used_at: Option<u64>,
}

fn health_table() -> std::sync::MutexGuard<'static, HashMap<String, UpstreamHealth>> {
    static TABLE: std::sync::LazyLock<Mutex<HashMap<String, UpstreamHealth>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A `Retry-After` header in delta-seconds form (the HTTP-date form is rare
/// on these APIs and ignored).
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|secs| Duration::from_secs(secs.min(REST_MAX.as_secs())))
}

/// The longest any upstream-supplied wait is honoured (magpie caps quota
/// rests at eight days: a weekly window plus slack). Beyond it a value is
/// noise — and an unbounded one overflows the deadline arithmetic.
const REST_MAX: Duration = Duration::from_secs(8 * 24 * 60 * 60);

/// The form of a pool key that may appear in the log or in a response
/// body. An account id can fall back to the EMAIL, so the id segment is
/// masked; every other key is a Termory-generated id.
pub fn public_key(key: &str) -> String {
    match key.strip_prefix("account:") {
        Some(rest) => match rest.split_once(':') {
            Some((app, id)) => format!("account:{app}:{}", crate::providers::mask_secret(id)),
            None => "account".to_string(),
        },
        None => key.to_string(),
    }
}

// ===================================================================
// Per-(member, model) state — CLIProxyAPI `MarkResult` / `isAuthBlockedForModel`
// (sdk/cliproxy/auth/conductor_cooldown.go, selector.go). A failure cools
// THAT model on THAT member; the member's other models stay usable.
// ===================================================================

#[derive(Debug, Clone, Default)]
struct ModelState {
    /// Unix ms until which the member is not tried for this model (0 = free).
    next_retry_after: u64,
    /// The block is a QUOTA one (a 429): counted as "cooldown" when every
    /// candidate is blocked (→ 429 `model_cooldown` instead of 503).
    quota: bool,
    /// 429 exponential backoff level (1 s · 2^level, capped at 30 min).
    backoff_level: u32,
}

fn model_states() -> std::sync::MutexGuard<'static, HashMap<(String, String), ModelState>> {
    static STATES: std::sync::LazyLock<Mutex<HashMap<(String, String), ModelState>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    STATES.lock().unwrap_or_else(|e| e.into_inner())
}

/// `Some((until_ms, quota))` while (member, model) is blocked.
fn model_blocked(key: &str, model: &str) -> Option<(u64, bool)> {
    let now = now_millis();
    let states = model_states();
    // A whole-credential block (usage limit) covers every model of the
    // member (`credential_quota`).
    [model, CREDENTIAL_SCOPE]
        .iter()
        .filter_map(|m| states.get(&(key.to_string(), m.to_string())))
        .filter(|s| s.next_retry_after > now)
        .map(|s| (s.next_retry_after, s.quota))
        .max_by_key(|(u, _)| *u)
}

/// The model key of a whole-credential block.
const CREDENTIAL_SCOPE: &str = "\u{0}credential";

const COOLDOWN_AUTH: Duration = Duration::from_secs(30 * 60);
const COOLDOWN_MODEL_SUPPORT: Duration = Duration::from_secs(12 * 60 * 60);
const COOLDOWN_TRANSIENT: Duration = Duration::from_secs(60);
const QUOTA_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
const QUOTA_FLOOR: Duration = Duration::from_secs(10);

/// One failed attempt, as `MarkResult` classifies it.
#[derive(Debug, Clone)]
struct Failure {
    status: Option<u16>,
    retry_after: Option<Duration>,
    text: String,
    /// DNS / connect / TLS / reset / timeout: never cools the member
    /// (`shouldSkipCredentialCooldown`), but may start a new round.
    transport: bool,
    /// A usage-limit exhaustion of the whole credential (Codex
    /// `IsCredentialScoped`): every model of the member is blocked.
    credential_scoped: bool,
    /// The failure belongs to THIS request (Codex `IsRequestScoped`): it is
    /// handed back to the client, nothing is cooled.
    request_scoped: bool,
}

impl Failure {
    /// The Codex executor's classified error (`codex_exec::CodexError`).
    fn from_codex(e: crate::codex_exec::CodexError) -> Self {
        Failure {
            status: (e.status != 0).then_some(e.status),
            retry_after: e.retry_after,
            text: e.message,
            transport: false,
            credential_scoped: e.credential_scoped,
            request_scoped: e.request_scoped,
        }
    }
}

/// Port of `MarkResult`'s failure arm for one (member, model): the status
/// table of conductor_cooldown.go. A new deadline never shortens a live one.
fn mark_model_failure(key: &str, model: &str, f: &Failure) {
    if f.transport {
        record_attempt_failure(key, &f.text, None);
        return;
    }
    let now = now_millis();
    let mut states = model_states();
    let st = states
        .entry((key.to_string(), model.to_string()))
        .or_default();
    // The quota flag describes the CURRENT block: once a 429 window has
    // run out, a later non-429 failure is not a quota block (an all-blocked
    // answer would otherwise still say 429 `model_cooldown`).
    if f.status != Some(429) && st.next_retry_after <= now {
        st.quota = false;
    }
    let ra = f
        .retry_after
        .filter(|d| !d.is_zero())
        .map(|d| d.min(REST_MAX));
    let lower = f.text.to_ascii_lowercase();
    // A Retry-After is a quota's reset time only on a 429; elsewhere it is
    // held to the status's own ceiling, so a stray huge value cannot park a
    // member for days.
    let held = |cap: Duration| ra.map(|d| d.min(cap));
    let wait: Duration = if is_model_support_error(f.status, &f.text) {
        held(COOLDOWN_MODEL_SUPPORT).unwrap_or(COOLDOWN_MODEL_SUPPORT)
    } else if lower.contains("invalid_grant") {
        COOLDOWN_AUTH
    } else {
        match f.status {
            Some(401) | Some(402) | Some(403) => COOLDOWN_AUTH,
            Some(404) => held(COOLDOWN_MODEL_SUPPORT).unwrap_or(COOLDOWN_MODEL_SUPPORT),
            Some(429) => {
                st.quota = true;
                if let Some(d) = ra {
                    d.max(QUOTA_FLOOR)
                } else if st.next_retry_after > now {
                    // A live quota window is reused, not escalated.
                    Duration::from_millis(st.next_retry_after - now)
                } else {
                    let d = Duration::from_secs(1u64 << st.backoff_level.min(20));
                    if d >= QUOTA_BACKOFF_MAX {
                        QUOTA_BACKOFF_MAX
                    } else {
                        st.backoff_level += 1;
                        d
                    }
                }
            }
            Some(408) | Some(500) | Some(502) | Some(503) | Some(504) | Some(520..=526) => {
                held(QUOTA_BACKOFF_MAX).unwrap_or(COOLDOWN_TRANSIENT)
            }
            _ => COOLDOWN_TRANSIENT,
        }
    };
    let deadline = now.saturating_add(wait.min(REST_MAX).as_millis() as u64);
    if deadline > st.next_retry_after {
        st.next_retry_after = deadline;
    }
    let until = st.next_retry_after;
    let quota = st.quota;
    if f.credential_scoped {
        let cred = states
            .entry((key.to_string(), CREDENTIAL_SCOPE.to_string()))
            .or_default();
        cred.quota = quota;
        if until > cred.next_retry_after {
            cred.next_retry_after = until;
        }
    }
    drop(states);
    // The page shows the status; the body may be long (it stays in the
    // client's error when this was the last member).
    let shown = f
        .status
        .map_or_else(|| f.text.clone(), |s| format!("HTTP {s}"));
    record_attempt_failure(key, &shown, Some(until));
}

/// Success resets that model's state (and its backoff level).
fn mark_model_success(key: &str, model: &str) {
    model_states().remove(&(key.to_string(), model.to_string()));
    record_success(key);
}

/// The per-member health row the page shows (counts, last error, the
/// latest cooldown deadline of any of its models).
fn record_attempt_failure(key: &str, why: &str, until: Option<u64>) {
    let mut table = health_table();
    let h = table
        .entry(key.to_string())
        .or_insert_with(|| UpstreamHealth {
            key: key.to_string(),
            ..Default::default()
        });
    h.requests += 1;
    h.failures += 1;
    h.streak += 1;
    h.last_error = Some(why.chars().take(300).collect());
    h.last_used_at = Some(now_millis());
    if let Some(u) = until {
        if h.cooldown_until.is_none_or(|c| c < u) {
            h.cooldown_until = Some(u);
        }
    }
}

/// A body's `error.code` / `code` / `error.type` / `type` (the paths
/// `clienterror` reads).
fn error_fields(body: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<JsonValue>(body) else {
        return (None, None);
    };
    let pick = |name: &str| -> Option<String> {
        for path in [
            format!("/error/{name}"),
            format!("/{name}"),
            format!("/response/error/{name}"),
            format!("/body/error/{name}"),
        ] {
            if let Some(s) = v.pointer(&path).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
        }
        None
    };
    (pick("code"), pick("type"))
}

/// "This member does not serve that model" — an explicit `model_not_found`
/// code, or a 400/404/422 whose text says the model is not supported.
fn is_model_support_error(status: Option<u16>, body: &str) -> bool {
    let (code, _) = error_fields(body);
    if code
        .as_deref()
        .is_some_and(|c| c == "model_not_found" || c == "model_not_found_error")
    {
        return true;
    }
    if !matches!(status, Some(400) | Some(404) | Some(422)) {
        return false;
    }
    let l = body.to_ascii_lowercase();
    l.contains("model")
        && [
            "not supported",
            "unsupported model",
            "model_not_supported",
            "does not exist",
            "not found",
            "unknown model",
            "invalid model",
            "no such model",
            "not available",
            "model names are",
        ]
        .iter()
        .any(|p| l.contains(p))
}

/// Port of `isRequestInvalidError` (conductor_cooldown.go): the REQUEST is
/// at fault, so it goes straight back to the client — no cooldown, no
/// other member tried.
fn is_request_invalid(status: u16, body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if status < 500 && lower.contains("cloudflare") && lower.contains("challenge") {
        return false;
    }
    if lower.contains("invalid_grant") && matches!(status, 400 | 401) {
        return false;
    }
    if is_model_support_error(Some(status), body) {
        return false;
    }
    if matches!(status, 402 | 429) {
        return false;
    }
    let (code, kind) = error_fields(body);
    if status == 401 && kind.as_deref() == Some("authentication_error") {
        return false;
    }
    if let Some(c) = code.as_deref() {
        if c == "model_not_found" || c == "model_not_found_error" {
            return false;
        }
        if [
            "cyber_policy",
            "context_length_exceeded",
            "message_too_big",
            "string_above_max_length",
            "invalid_prompt",
            "invalid_value",
            "unsupported_value",
            "invalid_request_error",
            "previous_response_not_found",
        ]
        .contains(&c)
        {
            return true;
        }
    }
    if let Some(k) = kind.as_deref() {
        if [
            "invalid_request",
            "invalid_request_error",
            "bad_request_error",
            "invalid_prompt",
        ]
        .contains(&k)
        {
            return true;
        }
    }
    if lower.contains("item with id") && lower.contains("not found") && lower.contains("store") {
        return true;
    }
    matches!(status, 400 | 409 | 413 | 422)
}

/// Statuses after which a new retry ROUND may start
/// (`isCredentialRetryRoundStatus`).
fn is_retry_round_status(status: u16) -> bool {
    matches!(status, 403 | 408 | 429 | 500 | 502 | 503 | 504)
}

fn record_skip(key: &str, why: String) {
    let mut table = health_table();
    let h = table
        .entry(key.to_string())
        .or_insert_with(|| UpstreamHealth {
            key: key.to_string(),
            ..Default::default()
        });
    h.last_error = Some(why);
}

fn record_success(key: &str) {
    let mut table = health_table();
    let h = table
        .entry(key.to_string())
        .or_insert_with(|| UpstreamHealth {
            key: key.to_string(),
            ..Default::default()
        });
    h.requests += 1;
    h.streak = 0;
    h.last_error = None;
    h.cooldown_until = None;
    h.last_used_at = Some(now_millis());
}

#[cfg(test)]
fn in_cooldown(key: &str) -> bool {
    health_table()
        .get(key)
        .and_then(|h| h.cooldown_until)
        .is_some_and(|until| until > now_millis())
}

/// Clear one upstream's cooldown and error (the page's "retry now").
pub fn reset_health(key: &str) {
    model_states().retain(|(k, _), _| k != key);
    if let Some(h) = health_table().get_mut(key) {
        h.streak = 0;
        h.cooldown_until = None;
        h.last_error = None;
    }
}

/// The ENABLED keys in saved order (unknown candidates are appended
/// DISABLED to `cfg.upstreams`, so routing never needs the page's full
/// enumeration).
pub fn enabled_keys(cfg: &RouterConfig) -> Vec<String> {
    cfg.upstreams
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.key.clone())
        .collect()
}

/// The members a request may use — CLIProxyAPI's candidate set
/// (`registry.GetModelProviders` + `authSupportsRouteModel`): only the
/// enabled members whose model list INCLUDES the requested model, in the
/// page's priority order (Settings → Tools grouping, saved order within a
/// group). A member that does not list the model is never tried.
///
/// One deviation, forced by our LIVE model lists (CLIProxyAPI's catalog is
/// static and always known): when no member lists the model, members whose
/// listing could not be fetched (unknown) are used instead, rather than
/// refusing a model the vendor may well serve. `model_unknown` = nobody
/// lists it and nobody is unknown → the client gets `model_not_found`.
pub struct Plan {
    pub members: Vec<(String, Result<Upstream, String>)>,
    pub model_unknown: bool,
}

pub fn routing_plan(cfg: &RouterConfig, protocol: Protocol, model: Option<&str>) -> Plan {
    let keys = enabled_keys(cfg);
    let stores = Stores::load();
    let mut members: Vec<(String, Result<Upstream, String>)> = keys
        .into_iter()
        .filter_map(|key| match resolve_for_client(&stores, &key, protocol) {
            Err(why) if why == PROTOCOL_MISMATCH => None,
            other => Some((key, other)),
        })
        .collect();
    let tools = tool_order();
    members.sort_by_key(|(key, _)| tool_rank(key_app(&stores, key), &tools));
    let Some(m) = model else {
        return Plan {
            members,
            model_unknown: false,
        };
    };
    let ranks: HashMap<String, u8> = members
        .iter()
        .map(|(key, _)| (key.clone(), model_rank(&upstream_models(&stores, key), m)))
        .collect();
    let has_listed = ranks.values().any(|r| *r == 0);
    let wanted = if has_listed { 0 } else { 1 };
    members.retain(|(key, _)| ranks.get(key) == Some(&wanted));
    Plan {
        model_unknown: members.is_empty(),
        members,
    }
}

// ---- Quota-aware order (magpie `weigh`, internal/gateway/routing.go) ----
//
// A login whose subscription windows are nearly used up goes to the back:
// fine (< 90 %) keeps the configured order, then low (90–98 %), then spent
// (≥ 98 %), each of those ascending by use. Nobody is skipped — only
// reordered. Unknown use counts as fine. magpie's smart mode also orders
// the fine ones by the soonest reset; Termory keeps the configured order
// there (the Router page's priority list is the user's choice).
const QUOTA_TTL: Duration = Duration::from_secs(60);
const QUOTA_KEEP: Duration = Duration::from_secs(10 * 60);
const QUOTA_LOW: f64 = 90.0;
const QUOTA_SPENT: f64 = 98.0;

#[derive(Clone)]
struct MemberQuota {
    at: std::time::Instant,
    used: Option<f64>,
}

fn member_quotas() -> std::sync::MutexGuard<'static, HashMap<String, MemberQuota>> {
    static M: std::sync::LazyLock<Mutex<HashMap<String, MemberQuota>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    M.lock().unwrap_or_else(|e| e.into_inner())
}

/// The most-used window of a member's subscription, when known lately.
fn quota_used(key: &str) -> Option<f64> {
    member_quotas()
        .get(key)
        .filter(|q| q.at.elapsed() <= QUOTA_KEEP)
        .and_then(|q| q.used)
}

fn quota_rank(used: Option<f64>) -> (u8, f64) {
    match used {
        Some(u) if u >= QUOTA_SPENT => (2, u),
        Some(u) if u >= QUOTA_LOW => (1, u),
        _ => (0, 0.0),
    }
}

fn order_by_quota(members: &mut [(String, Result<Upstream, String>)]) {
    let ranks: HashMap<String, (u8, f64)> = members
        .iter()
        .map(|(k, _)| (k.clone(), quota_rank(quota_used(k))))
        .collect();
    members.sort_by(|(a, _), (b, _)| {
        ranks[a]
            .partial_cmp(&ranks[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Re-read the subscription use of the plan's logins whose reading is
/// older than a minute (magpie caches allowances for one), in the
/// background: the request in hand routes on what is known now.
fn spawn_quota_refresh(members: &[(String, Result<Upstream, String>)]) {
    for (key, r) in members {
        let Ok(u) = r else { continue };
        let auth = match &u.auth {
            a @ (Auth::ChatGpt { .. } | Auth::GrokOauth { .. }) => a.clone(),
            _ => continue,
        };
        {
            let mut m = member_quotas();
            let prev = m.get(key).cloned();
            if prev.as_ref().is_some_and(|q| q.at.elapsed() < QUOTA_TTL) {
                continue;
            }
            // Claim it (and keep the last reading) so concurrent requests
            // do not each fetch.
            m.insert(
                key.clone(),
                MemberQuota {
                    at: std::time::Instant::now(),
                    used: prev.and_then(|q| q.used),
                },
            );
        }
        let key = key.clone();
        tokio::spawn(async move {
            let q = match &auth {
                Auth::ChatGpt { token, account_id } => {
                    crate::quota::query_codex_quota(token, account_id.as_deref()).await
                }
                Auth::GrokOauth {
                    token,
                    user_id,
                    email,
                } => crate::quota::query_grok_quota(token, user_id, email.as_deref()).await,
                Auth::ApiKey(_) => return,
            };
            let used = q
                .success
                .then(|| {
                    q.tiers
                        .iter()
                        .map(|t| t.utilization)
                        .fold(None, |m: Option<f64>, u| Some(m.map_or(u, |m| m.max(u))))
                })
                .flatten();
            member_quotas().insert(
                key,
                MemberQuota {
                    at: std::time::Instant::now(),
                    used,
                },
            );
        });
    }
}

// ---- Keeping Codex on an account with room (magpie `codex_switch.go`) ----
//
// The Codex app may stop sending at all once it knows the account it is
// signed in to is out — so no request comes to move on. While the router
// runs, the login
// Codex is signed in to is looked at every few minutes and, once it is
// spent (`QUOTA_SPENT`, the share quota ordering counts an account spent
// at, so the sign-in and the routing agree), Codex is signed in to the
// first saved Codex account ENABLED on the Router page, in page order, that
// is not flagged for re-login and whose use is known and not spent — through
// the tray's own switch (`spawn_account_switch`: the add-account guard, the
// snapshot of an unsaved live login, the quota and menu refresh).
const LOGIN_SWITCH_FIRST: Duration = Duration::from_secs(60);
const LOGIN_SWITCH_EVERY: Duration = Duration::from_secs(5 * 60);

/// magpie `spent`: an account-wide window at `QUOTA_SPENT` or past it.
/// Unknown (a failed read, no windows) is never spent.
fn quota_spent(q: &crate::quota::SubscriptionQuota) -> Option<bool> {
    if !q.success {
        return None;
    }
    Some(
        q.tiers
            .iter()
            .any(|t| t.group.is_none() && t.utilization >= QUOTA_SPENT),
    )
}

/// magpie `NextLogin`: the saved account Codex should move to, or `None`
/// when it should stay.
async fn codex_next_login() -> Option<String> {
    let cfg = current_config();
    let spare_keys: Vec<String> = enabled_keys(&cfg)
        .into_iter()
        .filter(|k| k.starts_with("account:codex:"))
        .collect();
    if spare_keys.is_empty() {
        return None;
    }
    let resolved = tokio::task::spawn_blocking(move || {
        let stores = Stores::load();
        let saved: HashMap<String, crate::accounts::TrayAccount> =
            crate::accounts::tray_accounts(CliApp::Codex)
                .into_iter()
                .map(|a| (a.id.clone(), a))
                .collect();
        let live = resolve_upstream_with(&stores, "live:codex", Protocol::OpenaiResponses).ok();
        let spares: Vec<(String, Upstream)> = spare_keys
            .iter()
            .filter_map(|k| {
                let id = k.strip_prefix("account:codex:")?;
                let a = saved.get(id)?;
                if a.active || a.needs_relogin {
                    return None;
                }
                let u = resolve_upstream_with(&stores, k, Protocol::OpenaiResponses).ok()?;
                Some((id.to_string(), u))
            })
            .collect();
        (live, spares)
    })
    .await
    .ok()?;
    let (Some(live), spares) = resolved else {
        return None;
    };
    if spares.is_empty() {
        return None;
    }
    let read = |u: &Upstream| match &u.auth {
        Auth::ChatGpt { token, account_id } => Some((token.clone(), account_id.clone())),
        _ => None,
    };
    let (token, account_id) = read(&live)?;
    let q = crate::quota::query_codex_quota(&token, account_id.as_deref()).await;
    if quota_spent(&q) != Some(true) {
        return None;
    }
    for (id, u) in spares {
        let Some((token, account_id)) = read(&u) else {
            continue;
        };
        let q = crate::quota::query_codex_quota(&token, account_id.as_deref()).await;
        if quota_spent(&q) == Some(false) {
            return Some(id);
        }
    }
    None
}

// ---- Session affinity (magpie `affine`, internal/gateway/affinity.go) ----
//
// A conversation stays with the member that answered it, so the vendor's
// prompt cache is read again instead of paid for afresh elsewhere. Kept
// while that member is in the plan, not resting for the model and not
// spent; within a turn (the client handing tool results back) always;
// across turns only while the last answer is under five minutes old (the
// shortest vendor cache life) — and never across turns under round-robin,
// where a new turn takes the next member. magpie also drops the stick when
// the last answer read under 1024 tokens from the cache; the router does
// not parse usage out of relayed streams, so that check is not made.
const STICK_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_COLD: Duration = Duration::from_secs(5 * 60);
const STICKS_MAX: usize = 4096;

/// magpie `sessionHeaders`.
const SESSION_HEADERS: [&str; 6] = [
    "x-opencode-session",
    "x-session-affinity",
    "x-session-id",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
];

struct Stick {
    member: String,
    at: std::time::Instant,
}

fn sticks() -> std::sync::MutexGuard<'static, HashMap<String, Stick>> {
    static M: std::sync::LazyLock<Mutex<HashMap<String, Stick>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    M.lock().unwrap_or_else(|e| e.into_inner())
}

/// magpie `conversationID`: a session header, else a hash of the first
/// user message (the whole body when there is none).
pub fn conversation_id(headers: &hyper::HeaderMap, body: &[u8]) -> String {
    use sha2::Digest;
    for h in SESSION_HEADERS {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    let parsed: Option<JsonValue> = serde_json::from_slice(body).ok();
    let first: Option<JsonValue> = parsed.as_ref().and_then(|v| {
        let (items, gemini) = match v.get("messages").and_then(|m| m.as_array()) {
            Some(m) if !m.is_empty() => (m.clone(), false),
            _ => match v.get("contents").and_then(|c| c.as_array()) {
                Some(c) if !c.is_empty() => (c.clone(), true),
                _ => match v.get("input") {
                    Some(JsonValue::Array(a)) => (a.clone(), false),
                    Some(other) => return Some(other.clone()),
                    None => (Vec::new(), false),
                },
            },
        };
        items
            .iter()
            .find(|it| {
                let role = it.get("role").and_then(|r| r.as_str());
                role == Some("user") || (gemini && role.is_none_or(str::is_empty))
            })
            .or(items.first())
            .cloned()
    });
    let bytes = match first {
        Some(v) => serde_json::to_vec(&v).unwrap_or_default(),
        None => body.to_vec(),
    };
    let sum = sha2::Sha256::digest(&bytes);
    let hex: String = sum[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("magpie-{hex}")
}

/// magpie `turnIn`'s `within`: the request hands tool results back, so the
/// user's turn is still going on.
fn within_turn(protocol: Protocol, body: Option<&JsonValue>) -> bool {
    let Some(b) = body else { return false };
    let last_of = |field: &str| {
        b.get(field)
            .and_then(|v| v.as_array())
            .and_then(|a| a.last())
    };
    match protocol {
        Protocol::Anthropic => last_of("messages")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .is_some_and(|c| {
                c.iter()
                    .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            }),
        Protocol::OpenaiChat => {
            last_of("messages")
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str())
                == Some("tool")
        }
        Protocol::OpenaiResponses => last_of("input")
            .and_then(|m| m.get("type"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.ends_with("call_output")),
        Protocol::Gemini => last_of("contents")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .is_some_and(|p| p.iter().any(|x| x.get("functionResponse").is_some())),
    }
}

/// The member to put first for this conversation, if the stick holds.
fn sticky_member(
    stick_key: &str,
    plan: &[(String, Result<Upstream, String>)],
    model: &str,
    within: bool,
    rotate: bool,
) -> Option<String> {
    let (member, age) = {
        let m = sticks();
        let st = m.get(stick_key)?;
        (st.member.clone(), st.at.elapsed())
    };
    if age > STICK_KEEP
        || !plan.iter().any(|(k, r)| *k == member && r.is_ok())
        || model_blocked(&member, model).is_some()
        || quota_used(&member).is_some_and(|u| u >= QUOTA_SPENT)
    {
        return None;
    }
    if within {
        return Some(member);
    }
    if rotate || age > CACHE_COLD {
        return None;
    }
    Some(member)
}

fn remember_stick(stick_key: &str, member: &str) {
    let mut m = sticks();
    m.insert(
        stick_key.to_string(),
        Stick {
            member: member.to_string(),
            at: std::time::Instant::now(),
        },
    );
    if m.len() > STICKS_MAX {
        m.retain(|_, st| st.at.elapsed() <= STICK_KEEP);
    }
    // Still over (more than STICKS_MAX conversations within a day): drop
    // the oldest down to the cap, so the map — and this pass — stay bounded.
    if m.len() > STICKS_MAX {
        let mut ages: Vec<(std::time::Instant, String)> =
            m.iter().map(|(k, st)| (st.at, k.clone())).collect();
        ages.sort();
        let excess = m.len() - STICKS_MAX;
        for (_, k) in ages.into_iter().take(excess) {
            m.remove(&k);
        }
    }
}

/// Round-robin cursor per model (`RoundRobinSelector`: the member AFTER the
/// last one picked for this model, wrapping). Failover is CLIProxyAPI's
/// fill-first: the first eligible member in priority order.
fn rr_last() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    static LAST: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    LAST.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pick the next member from `eligible` (in priority order).
fn pick_member<'a>(strategy: Strategy, model: &str, eligible: &[&'a str]) -> Option<&'a str> {
    let first = *eligible.first()?;
    if strategy != Strategy::RoundRobin {
        return Some(first);
    }
    let mut last = rr_last();
    let next = match last.get(model) {
        Some(prev) => match eligible.iter().position(|k| *k == prev) {
            Some(i) => eligible[(i + 1) % eligible.len()],
            // The last pick is not eligible now: the first one after it in
            // the full order is unknown here, so start from the top.
            None => first,
        },
        None => first,
    };
    last.insert(model.to_string(), next.to_string());
    Some(next)
}

/// The tool a pool key belongs to (`None` for a gateway).
fn key_app(stores: &Stores, key: &str) -> Option<CliApp> {
    let mut parts = key.splitn(3, ':');
    match parts.next()? {
        "live" | "account" => parts.next().and_then(CliApp::parse),
        "provider" => {
            let id = parts.next()?;
            stores.providers.iter().find(|p| p.id == id).map(|p| p.app)
        }
        _ => None,
    }
}

/// 0 = listed, 1 = catalog unknown, 2 = catalog known and model absent.
fn model_rank(models: &Option<Vec<String>>, model: &str) -> u8 {
    match models {
        Some(list) if list.iter().any(|m| m == model) => 0,
        None => 1,
        Some(_) => 2,
    }
}

/// What a pool member is known to serve: its fetched listing (`CATALOG`),
/// a provider's configured models, a gateway's detected catalog. `None`
/// when nothing is known.
fn upstream_models(stores: &Stores, key: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = CATALOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .map(|(_, m)| m.clone())
        .unwrap_or_default();
    if let Some(id) = key.strip_prefix("provider:") {
        if let Some(p) = stores.providers.iter().find(|p| p.id == id) {
            if !p.model.trim().is_empty() {
                out.push(p.model.trim().to_string());
            }
            out.extend(p.models.iter().map(|m| m.id.clone()));
        }
    } else if let Some(id) = key.strip_prefix("gateway:") {
        // The live listing (in `out` already) wins; the list saved at
        // detection time is only the fallback before one was fetched.
        if out.is_empty() {
            if let Some(g) = stores
                .gateways
                .iter()
                .find(|g| g.get("id").and_then(|v| v.as_str()) == Some(id))
            {
                out = detected_gateway_models(g);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The model list a gateway's entry saved at DETECTION time — stale by
/// nature (vendors rename and retire models), so only a fallback until
/// `refresh_catalog` has the live one.
fn detected_gateway_models(g: &JsonValue) -> Vec<String> {
    match g.get("capabilities").and_then(|c| c.get("models")) {
        Some(JsonValue::Array(models)) => models
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The model a client request names: the body's `model`, or for Gemini the
/// `models/<id>:<method>` path segment.
pub fn requested_model(path: &str, body: &[u8]) -> Option<String> {
    if let Some(rest) = path.split("/models/").nth(1) {
        if let Some(id) = rest.split(':').next().filter(|s| !s.is_empty()) {
            return Some(id.to_string());
        }
    }
    serde_json::from_slice::<JsonValue>(body)
        .ok()?
        .get("model")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ===================================================================
// Server
// ===================================================================

struct ServerHandle {
    host: std::net::IpAddr,
    port: u16,
    started_at: u64,
    /// Flipped to `true` on stop. Watched by the accept loop AND every open
    /// connection, so a keep-alive client cannot keep routing after Stop.
    shutdown: tokio::sync::watch::Sender<bool>,
}

fn server_slot() -> std::sync::MutexGuard<'static, Option<ServerHandle>> {
    static SLOT: Mutex<Option<ServerHandle>> = Mutex::new(None);
    SLOT.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterStatus {
    pub running: bool,
    /// The bind address (`127.0.0.1`, `0.0.0.0`, an interface IP).
    pub host: String,
    pub port: u16,
    pub started_at: Option<u64>,
    /// The URL this machine's CLIs use (`router_base_url`).
    pub base_url: String,
    /// The URL for other devices, when the router is exposed to the LAN.
    pub lan_url: Option<String>,
    pub upstreams: Vec<UpstreamHealth>,
}

/// Whether the listener is up. Activation of a router binding is refused
/// while it is not (see `providers::activate`).
pub fn is_running() -> bool {
    server_slot().is_some()
}

/// Whether `id` is one of the router gateway's binding ids — i.e. a provider
/// that routes through the local router.
pub fn is_router_binding_id(id: &str) -> bool {
    let Ok(JsonValue::Array(gws)) = crate::config::read_gateways() else {
        return false;
    };
    gws.iter()
        .filter(|g| is_router_entry(g))
        .flat_map(|g| {
            g.get("bindings")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .any(|b| b.get("id").and_then(|v| v.as_str()) == Some(id))
}

pub fn status() -> RouterStatus {
    let cfg = current_config();
    let slot = server_slot();
    let (running, host, port, started_at) = match slot.as_ref() {
        Some(h) => (true, h.host, h.port, Some(h.started_at)),
        None => (false, bind_ip(&cfg), cfg.port, None),
    };
    drop(slot);
    let base_url = router_base_url(host, port);
    let lan_url = lan_url(host, port);
    let mut upstreams: Vec<UpstreamHealth> = health_table().values().cloned().collect();
    upstreams.sort_by(|a, b| a.key.cmp(&b.key));
    RouterStatus {
        running,
        host: host.to_string(),
        port,
        started_at,
        base_url,
        lan_url,
        upstreams,
    }
}

/// Start the listener on the configured port. Idempotent when already
/// running there.
pub async fn start(app: tauri::AppHandle) -> Result<RouterStatus, String> {
    let _g = lifecycle().lock().await;
    let cfg = current_config();
    // A hand-edited file cannot open the router to the LAN without a key.
    validate_exposure(&cfg)?;
    listen(&app, bind_ip(&cfg), cfg.port).await
}

/// Bring the listener up on `host:port` (caller holds `lifecycle()`). The
/// address is BOUND FIRST, so a failure changes nothing. The page never
/// re-binds a running listener (connection settings are edited only while
/// stopped); should the saved address differ from a running one anyway (a
/// hand-edited file), the listeners are swapped without suspending. A FRESH
/// start re-applies and restores bindings before returning.
async fn listen(
    app: &tauri::AppHandle,
    host: std::net::IpAddr,
    port: u16,
) -> Result<RouterStatus, String> {
    let running = server_slot().as_ref().map(|h| (h.host, h.port));
    if running == Some((host, port)) {
        return Ok(status());
    }
    let addr = SocketAddr::new(host, port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    // Only now retire the old listener (a port change): bindings stay in use.
    stop_listener(app);
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let ctx = Arc::new(Ctx {
        client,
        app: Some(app.clone()),
    });
    *server_slot() = Some(ServerHandle {
        host,
        port,
        started_at: now_millis(),
        shutdown: tx,
    });
    log::info!("router listening on {addr}");

    // Keep the pooled logins fresh while the router runs, not only when a
    // request happens to need one: a pass now, then every
    // `BACKGROUND_REFRESH_EVERY`, ending with the listener.
    {
        let bg_ctx = ctx.clone();
        let mut bg_rx = rx.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                background_refresh_pass(&bg_ctx).await;
                tokio::select! {
                    _ = bg_rx.changed() => break,
                    _ = tokio::time::sleep(BACKGROUND_REFRESH_EVERY) => {}
                }
            }
        });
    }
    // Keep Codex signed in to an account with room (magpie
    // `KeepOnAnAccountWithRoom`): a minute after start, then every
    // `LOGIN_SWITCH_EVERY`, ending with the listener.
    if let Some(app_handle) = ctx.app.clone() {
        let mut sw_rx = rx.clone();
        tauri::async_runtime::spawn(async move {
            let mut wait = LOGIN_SWITCH_FIRST;
            loop {
                tokio::select! {
                    _ = sw_rx.changed() => break,
                    _ = tokio::time::sleep(wait) => {}
                }
                if let Some(id) = codex_next_login().await {
                    log::info!(
                        "router: the Codex login has used {QUOTA_SPENT}% or more of its allowance; switching it to a saved account with room"
                    );
                    crate::tray::spawn_account_switch(&app_handle, CliApp::Codex, id);
                }
                wait = LOGIN_SWITCH_EVERY;
            }
        });
    }

    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = rx.changed() => break,
                accepted = listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(pair) => pair,
                        // e.g. EMFILE: accept() fails at once on every call,
                        // so back off instead of spinning a core.
                        Err(err) => {
                            log::debug!("router: accept failed: {err}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let ctx = ctx.clone();
                    let mut conn_rx = rx.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req| handle(req, ctx.clone()));
                        let conn = http1::Builder::new()
                            .keep_alive(true)
                            .serve_connection(io, svc);
                        let mut conn = std::pin::pin!(conn);
                        tokio::select! {
                            res = conn.as_mut() => {
                                if let Err(err) = res {
                                    log::debug!("router connection ended: {err}");
                                }
                            }
                            _ = conn_rx.changed() => {
                                // Finish the in-flight response, then close.
                                conn.as_mut().graceful_shutdown();
                                let _ = conn.await;
                            }
                        }
                    });
                }
            }
        }
        log::info!("router stopped");
    });
    let _ = app.emit(ROUTER_CHANGED_EVENT, ());
    if running.is_some() {
        return Ok(status());
    }
    // Fetch every enabled member's model list right away, so the first
    // requests route by real lists (CLIProxyAPI loads its registry at start).
    tauri::async_runtime::spawn(refresh_catalog(false));
    // A fresh start: a binding still live from before (an unclean exit on
    // an older port/key) is brought up to date, and the bindings the last
    // stop suspended go back in use. Awaited, under the lifecycle lock, so
    // a Stop cannot land in the middle of it.
    let app2 = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        match live_router_bindings() {
            Ok(live) => {
                if let Err(err) = reapply_bindings(&live) {
                    log::warn!("re-applying router bindings after start failed: {err}");
                }
            }
            Err(err) => log::warn!("reading router bindings after start failed: {err}"),
        }
        match restore_suspended_bindings() {
            Ok(n) if n > 0 => notify_bindings_changed(&app2),
            Ok(_) => rebuild_tray(&app2),
            Err(err) => {
                log::warn!("restoring router bindings after start failed: {err}");
                rebuild_tray(&app2);
            }
        }
        settle_pending_codex_follow();
    })
    .await;
    Ok(status())
}

/// Tear the listener down. Open keep-alive connections are closed too.
fn stop_listener(app: &tauri::AppHandle) -> bool {
    let handle = server_slot().take();
    match handle {
        Some(h) => {
            let _ = h.shutdown.send(true);
            let _ = app.emit(ROUTER_CHANGED_EVENT, ());
            true
        }
        None => false,
    }
}

/// The user's Stop: tear the listener down AND hand every CLI that was
/// using a router binding back to Official, remembering which so `start()`
/// can restore them. Leaving the bindings active would keep each CLI
/// pointed at a closed port while every list still said "in use".
pub async fn stop(app: &tauri::AppHandle) {
    let _g = lifecycle().lock().await;
    if !stop_listener(app) {
        return;
    }
    let app2 = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        match suspend_live_bindings(CodexFollow::Now) {
            Ok(n) if n > 0 => notify_bindings_changed(&app2),
            Ok(_) => rebuild_tray(&app2),
            Err(err) => {
                log::warn!("suspending router bindings on stop failed: {err}");
                rebuild_tray(&app2);
            }
        }
    })
    .await;
}

/// The menu bar greys the router's binding rows while it is stopped, so
/// every start and stop rebuilds it — even one that switched no binding.
fn rebuild_tray(app: &tauri::AppHandle) {
    if let Err(err) = crate::tray::rebuild_menu(app) {
        log::warn!("tray rebuild after router start/stop failed: {err}");
    }
}

/// The Providers page and the tray both list what a binding switch changed.
fn notify_bindings_changed(app: &tauri::AppHandle) {
    let _ = app.emit("termory:providers-changed", ());
    rebuild_tray(app);
}

struct Ctx {
    client: reqwest::Client,
    /// `None` only in tests, which drive `handle_inner` with no app.
    app: Option<tauri::AppHandle>,
}

impl Ctx {
    fn emit_changed(&self) {
        if let Some(app) = &self.app {
            let _ = app.emit(ROUTER_CHANGED_EVENT, ());
        }
    }
}

type OutBody = BoxBody<Bytes, std::io::Error>;

fn full(body: impl Into<Bytes>) -> OutBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed()
}

fn json_response(status: StatusCode, body: JsonValue) -> Response<OutBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(body.to_string()))
        .unwrap_or_else(|_| Response::new(full("")))
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response<OutBody> {
    json_response(
        status,
        json!({ "error": { "type": kind, "message": message } }),
    )
}

/// Constant-time string equality, so the key check's timing says nothing
/// about how much of a guess matched. Length is not secret (every key is
/// `sk-` + 64 hex).
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The client-side key check. Accepts every header the four API shapes use
/// for their own key, plus Gemini's `?key=` query form.
pub fn client_key_matches(
    expected: &str,
    authorization: Option<&str>,
    x_api_key: Option<&str>,
    x_goog_api_key: Option<&str>,
    query: Option<&str>,
) -> bool {
    // No key, no use: a router without a key serves nobody.
    if expected.is_empty() {
        return false;
    }
    if let Some(a) = authorization {
        let t = a.trim();
        let t = t
            .strip_prefix("Bearer ")
            .or_else(|| t.strip_prefix("bearer "))
            .unwrap_or(t);
        if ct_eq(t.trim(), expected) {
            return true;
        }
    }
    if x_api_key.is_some_and(|k| ct_eq(k.trim(), expected)) {
        return true;
    }
    if x_goog_api_key.is_some_and(|k| ct_eq(k.trim(), expected)) {
        return true;
    }
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("key=") {
                if ct_eq(v, expected) {
                    return true;
                }
            }
        }
    }
    false
}

/// `?key=` is the client's OWN key for the router; it must not travel
/// upstream. Every other parameter (`alt=sse`) is forwarded as-is.
pub fn strip_key_param(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("key="))
        .collect::<Vec<_>>()
        .join("&")
}

/// Headers that never cross the router: hop-by-hop, the host, the framing
/// (reqwest / hyper recompute them), compression negotiation (a body is
/// relayed byte-for-byte, so the client's `accept-encoding` must not make
/// the upstream compress something the router will not decompress) and
/// every credential header (replaced by the upstream's own).
fn is_dropped_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-connection"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "content-length"
            | "accept-encoding"
            | "authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "chatgpt-account-id"
            | "x-xai-token-auth"
    )
}

fn is_dropped_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "trailer"
    )
}

fn apply_auth(
    mut req: reqwest::RequestBuilder,
    auth: &Auth,
    protocol: Protocol,
) -> reqwest::RequestBuilder {
    match auth {
        Auth::ChatGpt { token, account_id } => {
            req = req.header("authorization", format!("Bearer {token}"));
            if let Some(id) = account_id {
                req = req.header("chatgpt-account-id", id.as_str());
            }
        }
        Auth::GrokOauth {
            token,
            user_id,
            email,
        } => {
            req = grok_headers(req, token, user_id, email.as_deref());
        }
        Auth::ApiKey(key) => match protocol {
            Protocol::Gemini => {
                req = req.header("x-goog-api-key", key.as_str());
            }
            Protocol::Anthropic => {
                // Claude Code sends `Authorization: Bearer` for a custom
                // endpoint (ANTHROPIC_AUTH_TOKEN, which Termory writes) and the
                // Anthropic API proper wants `x-api-key`; every compatible
                // vendor accepts either, so both go out.
                req = req
                    .header("authorization", format!("Bearer {key}"))
                    .header("x-api-key", key.as_str());
            }
            Protocol::OpenaiResponses | Protocol::OpenaiChat => {
                req = req.header("authorization", format!("Bearer {key}"));
            }
        },
    }
    req
}

/// Grok Build's request headers against its cli-chat-proxy, as the shell
/// sends them (`mvp_agent/mod.rs` `inject_proxy_headers`, sampler
/// `client.rs`, `extensions/billing.rs`): the bearer, `X-XAI-Token-Auth`
/// for a user token, the user id in both spellings the code base uses, the
/// client version the proxy version-gates on, identifier and mode.
fn grok_headers(
    mut req: reqwest::RequestBuilder,
    token: &str,
    user_id: &str,
    email: Option<&str>,
) -> reqwest::RequestBuilder {
    req = req
        .header("authorization", format!("Bearer {token}"))
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-authenticateresponse", "authenticate-response")
        .header("x-userid", user_id)
        .header("x-grok-user-id", user_id)
        .header("x-grok-client-version", grok_client_version())
        .header("x-grok-client-identifier", "grok-shell")
        .header("x-grok-client-mode", "interactive");
    if let Some(e) = email {
        req = req.header("x-email", e);
    }
    req
}

/// The installed Grok Build version (`grok 1.0.30 (hash)` → `1.0.30`),
/// else the known-good fallback the quota probe uses.
fn grok_client_version() -> String {
    static VERSION: std::sync::LazyLock<Mutex<Option<String>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    if let Some(v) = VERSION.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return v.clone();
    }
    let v = crate::providers::detect_cli_version(CliApp::Grok)
        .and_then(|raw| {
            raw.split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .find(|t| t.contains('.') && t.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "0.2.99".to_string());
    *VERSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.clone());
    v
}

/// Refresh the login behind `key` (single-flight per key) and resolve the
/// upstream again from the stores. `stale_token` is the access token that
/// just failed (the 401/403 path): when the stored one already differs,
/// another request refreshed it and it is used without a second refresh.
async fn refresh_and_resolve(
    ctx: &Ctx,
    key: &str,
    login: &crate::accounts::RouterLogin,
    protocol: Protocol,
    stale_token: Option<&str>,
) -> Result<Upstream, String> {
    static GATES: std::sync::LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    let gate = GATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _held = gate.lock().await;
    let resolve = |k: String| {
        tokio::task::spawn_blocking(move || resolve_upstream_with(&Stores::load(), &k, protocol))
    };
    if let Some(stale) = stale_token {
        if let Ok(Ok(u)) = resolve(key.to_string()).await {
            if auth_token(&u.auth) != Some(stale) {
                return Ok(u);
            }
        }
    }
    // A login flow OWNS the credential while it runs (see accounts.rs).
    let cli = match login {
        crate::accounts::RouterLogin::Live(app) => *app,
        crate::accounts::RouterLogin::Saved { app, .. } => *app,
    };
    if let Some(app) = &ctx.app {
        if crate::accounts::login_in_progress(app, cli) {
            return Err("a login is in progress for this tool".into());
        }
    }
    crate::accounts::refresh_login_for_router(login, stale_token.is_some()).await?;
    resolve(key.to_string()).await.map_err(|e| e.to_string())?
}

fn auth_token(auth: &Auth) -> Option<&str> {
    match auth {
        Auth::ChatGpt { token, .. } => Some(token),
        Auth::GrokOauth { token, .. } => Some(token),
        Auth::ApiKey(k) => Some(k),
    }
}

/// Everything about the client request a send needs, borrowed.
struct Outgoing<'a> {
    method: &'a hyper::Method,
    /// The API the UPSTREAM is spoken to in for this send.
    protocol: Protocol,
    path_and_query: &'a str,
    body: &'a Bytes,
    headers: &'a hyper::HeaderMap,
    /// The client asked for a stream (before any translation).
    client_stream: bool,
    /// The body is the client's own Responses request, untranslated.
    passthrough: bool,
    /// The model as the client named it, thinking suffix included
    /// (`gpt-5.6-terra(high)`), and without it.
    requested_model: &'a str,
    base_model: &'a str,
    /// The client's own API and body (the thinking config's source).
    client_protocol: Protocol,
    source_body: Option<&'a JsonValue>,
    /// Repairs in force for this member (`autofix`), applied last.
    fixes: &'a crate::autofix::BodyFixes,
}

/// CLIProxyAPI's format names, as `thinking` takes them.
fn thinking_format(p: Protocol) -> &'static str {
    match p {
        Protocol::Anthropic => "claude",
        Protocol::OpenaiChat => "openai",
        Protocol::OpenaiResponses => "openai-response",
        Protocol::Gemini => "gemini",
    }
}

/// The executor step after translation (CLIProxyAPI `ApplyRequestThinking`
/// then `SetStringIfDifferent(body, "model", baseModel)`): the thinking
/// config — the model-name suffix first, else the source body's own
/// reasoning settings — is written in the UPSTREAM's format, and the body's
/// model becomes the base name.
fn apply_request_thinking(body: &mut JsonValue, out: &Outgoing<'_>, to_format: &str) {
    if !body.is_object() {
        return;
    }
    match crate::thinking::apply_thinking(
        body,
        out.requested_model,
        thinking_format(out.client_protocol),
        to_format,
        to_format,
        out.source_body,
    ) {
        Ok(v) => *body = v,
        Err(err) => log::warn!("router: thinking config not applied: {err}"),
    }
    if let Some(o) = body.as_object_mut() {
        if o.contains_key("model") && !out.base_model.is_empty() {
            o.insert("model".into(), JsonValue::from(out.base_model));
        }
    }
}

/// Optional fields each member refused (magpie `markUnfit`), per API: left
/// out of later requests up front. Remembered only once a send without
/// them succeeded.
type RefusedFields = HashMap<(String, Protocol), Vec<String>>;

fn refused_fields() -> std::sync::MutexGuard<'static, RefusedFields> {
    static M: std::sync::LazyLock<Mutex<RefusedFields>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    M.lock().unwrap_or_else(|e| e.into_inner())
}

fn remembered_fixes(key: &str, protocol: Protocol) -> crate::autofix::BodyFixes {
    crate::autofix::BodyFixes {
        drop_fields: refused_fields()
            .get(&(key.to_string(), protocol))
            .cloned()
            .unwrap_or_default(),
        ..Default::default()
    }
}

fn remember_fixes(key: &str, protocol: Protocol, fixes: &crate::autofix::BodyFixes) {
    if fixes.drop_fields.is_empty() {
        return;
    }
    let mut m = refused_fields();
    let e = m.entry((key.to_string(), protocol)).or_default();
    for f in &fixes.drop_fields {
        if !e.contains(f) {
            e.push(f.clone());
        }
    }
}

fn next_fix_for(
    sent: Option<&JsonValue>,
    status: u16,
    text: &str,
    cur: &crate::autofix::BodyFixes,
    anthropic: bool,
) -> Option<crate::autofix::BodyFixes> {
    crate::autofix::next_fix(status, text, sent?, cur, !anthropic)
}

/// What the response side needs to know about a Codex-backend send.
pub struct CodexRequest {
    /// The client did not ask for a stream; the backend always streams, so
    /// the SSE is assembled into one Response object for it.
    pub unwrap_stream: bool,
}

/// Build the upstream request for one upstream: URL, body, headers,
/// credential. A ChatGPT-login upstream goes through the port of
/// CLIProxyAPI's Codex executor (`codex_exec::build_request`, after the
/// Responses→Codex request translator for an untranslated Responses body);
/// every other upstream gets the body as-is plus its credential.
fn build_upstream_request(
    ctx: &Ctx,
    out: &Outgoing<'_>,
    upstream: &Upstream,
) -> (
    reqwest::RequestBuilder,
    Option<CodexRequest>,
    Option<JsonValue>,
) {
    let protocol = out.protocol;
    if let (Auth::ChatGpt { token, account_id }, Protocol::OpenaiResponses) =
        (&upstream.auth, protocol)
    {
        let mut body: JsonValue = serde_json::from_slice(out.body).unwrap_or(JsonValue::Null);
        if out.passthrough {
            let model = crate::codex_exec::request_model_name(&body);
            body = crate::translate::responses_to_codex::translate_request(
                &model,
                &body,
                out.client_stream,
            );
        }
        apply_request_thinking(&mut body, out, "codex");
        out.fixes.apply(&mut body);
        let client_headers: Vec<(String, String)> = out
            .headers
            .iter()
            .filter_map(|(n, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (n.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        let (url, headers, bytes) = crate::codex_exec::build_request(
            &upstream.base_url,
            &body,
            &crate::codex_exec::CodexCredential {
                access_token: token,
                account_id: account_id.as_deref(),
            },
            &client_headers,
            "",
        );
        let mut builder = ctx.client.post(url).body(bytes);
        for (k, v) in headers {
            // reqwest negotiates the connection itself (and HTTP/2 forbids it).
            if k.eq_ignore_ascii_case("connection") {
                continue;
            }
            builder = builder.header(k, v);
        }
        return (
            builder,
            Some(CodexRequest {
                unwrap_stream: !out.client_stream,
            }),
            Some(body),
        );
    }
    // A Gemini API-key upstream goes through the port of CLIProxyAPI's
    // Gemini executor (`gemini_exec::build_request`): its own URL
    // (`/v1beta/models/{model}:{generateContent|streamGenerateContent?alt=sse}`),
    // key header and body steps, after the thinking config.
    if let (Auth::ApiKey(key), Protocol::Gemini) = (&upstream.auth, protocol) {
        let mut body: JsonValue = serde_json::from_slice(out.body).unwrap_or(JsonValue::Null);
        apply_request_thinking(&mut body, out, "gemini");
        out.fixes.apply(&mut body);
        let base = upstream.base_url.trim().trim_end_matches('/');
        let base = base.strip_suffix("/v1beta").unwrap_or(base);
        let base = base.strip_suffix("/v1").unwrap_or(base);
        let client_headers: Vec<(String, String)> = out
            .headers
            .iter()
            .filter_map(|(n, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (n.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        let (url, headers, bytes) = crate::gemini_exec::build_request(
            base,
            out.base_model,
            out.client_stream,
            &body,
            key,
            &client_headers,
        );
        let mut builder = ctx.client.post(url).body(bytes);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        return (builder, None, Some(body));
    }
    let url = upstream_url(&upstream.base_url, protocol, out.path_and_query);
    let grok_responses =
        matches!(upstream.auth, Auth::GrokOauth { .. }) && protocol == Protocol::OpenaiResponses;
    let mut sent_json = None;
    let shaped = match serde_json::from_slice::<JsonValue>(out.body) {
        Ok(mut body) if body.is_object() && protocol != Protocol::Gemini => {
            apply_request_thinking(&mut body, out, thinking_format(protocol));
            out.fixes.apply(&mut body);
            let bytes = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
            sent_json = Some(body);
            bytes
        }
        Ok(mut body) if body.is_object() && !out.fixes.is_empty() => {
            out.fixes.apply(&mut body);
            let bytes = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
            sent_json = Some(body);
            bytes
        }
        _ => out.body.clone(),
    };
    let send_body = if grok_responses {
        strip_unsupported_tools(&shaped)
    } else {
        shaped
    };
    let mut builder = ctx.client.request(out.method.clone(), &url).body(send_body);
    for (name, value) in out.headers.iter() {
        if is_dropped_request_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = apply_auth(builder, &upstream.auth, protocol);
    (builder, None, sent_json)
}

async fn handle(req: Request<Incoming>, ctx: Arc<Ctx>) -> Result<Response<OutBody>, hyper::Error> {
    Ok(handle_inner(req, ctx).await)
}

async fn handle_inner<B>(req: Request<B>, ctx: Arc<Ctx>) -> Response<OutBody>
where
    B: hyper::body::Body,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let cfg = current_config();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    if req.method() == hyper::Method::GET && (path == "/" || path == "/health") {
        return json_response(StatusCode::OK, json!({ "ok": true, "port": cfg.port }));
    }

    let headers = req.headers().clone();
    let hdr = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    if !client_key_matches(
        &cfg.api_key,
        hdr("authorization"),
        hdr("x-api-key"),
        hdr("x-goog-api-key"),
        query.as_deref(),
    ) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid router API key",
        );
    }

    // Gemini clients list models at `/v1beta/models` (CLIProxyAPI
    // `convertModelToMap` gemini shape).
    if req.method() == hyper::Method::GET && path.trim_end_matches('/') == "/v1beta/models" {
        catalog_for_request().await;
        let listing = tokio::task::spawn_blocking(move || {
            let ids = available_model_ids(&cfg);
            json!({ "models": ids.iter().map(|(id, _)| json!({
                "name": format!("models/{id}"),
                "displayName": id,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })).collect::<Vec<_>>() })
        })
        .await
        .unwrap_or_else(|_| json!({ "models": [] }));
        return json_response(StatusCode::OK, listing);
    }

    if req.method() == hyper::Method::GET && path.trim_end_matches('/') == "/v1/models" {
        catalog_for_request().await;
        // Anthropic shape for Anthropic clients (CLIProxyAPI's
        // `isAnthropicModelsRequest`: an `anthropic-version` header or a
        // claude-cli user agent), the OpenAI shape for everyone else.
        let anthropic = hdr("anthropic-version").is_some()
            || hdr("user-agent").is_some_and(|ua| ua.contains("claude-cli"));
        let listing = tokio::task::spawn_blocking(move || models_listing(&cfg, anthropic))
            .await
            .unwrap_or_else(|_| json!({ "object": "list", "data": [] }));
        return json_response(StatusCode::OK, listing);
    }

    // Token counting (CLIProxyAPI: `POST /v1/messages/count_tokens` and
    // Gemini `:countTokens` — there is no OpenAI one).
    let count_client = if path.trim_end_matches('/') == "/v1/messages/count_tokens" {
        Some(Protocol::Anthropic)
    } else if path.starts_with("/v1beta/models/") && path.ends_with(":countTokens") {
        Some(Protocol::Gemini)
    } else {
        None
    };
    if let (Some(client), &hyper::Method::POST) = (count_client, req.method()) {
        let body = match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
            .collect()
            .await
        {
            Ok(c) => c.to_bytes(),
            Err(_) => {
                return error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    "request body exceeds the router limit",
                )
            }
        };
        return handle_count(&ctx, &cfg, client, &path, &body, &headers).await;
    }

    let Some(protocol) = classify_path(&path) else {
        return error_response(StatusCode::NOT_FOUND, "not_found_error", "unknown API path");
    };

    let method = req.method().clone();
    let body = match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body exceeds the router limit",
            );
        }
    };
    serve_api(ctx, cfg, protocol, method, path, query, headers, body).await
}

/// Route one API request (body collected) across the pool.
#[allow(clippy::too_many_arguments)]
async fn serve_api(
    ctx: Arc<Ctx>,
    cfg: RouterConfig,
    protocol: Protocol,
    method: hyper::Method,
    path: String,
    query: Option<String>,
    headers: hyper::HeaderMap,
    body: Bytes,
) -> Response<OutBody> {
    let forwarded_query = query
        .as_deref()
        .map(strip_key_param)
        .filter(|q| !q.is_empty());
    let path_and_query = match &forwarded_query {
        Some(q) => format!("{path}?{q}"),
        None => path.clone(),
    };

    // Credential resolution reads files and, for Claude, may spawn
    // `security(1)` on a cache miss — blocking work that must not park a
    // Tokio worker (a locked Keychain holds it for the whole probe timeout).
    let plan_cfg = cfg.clone();
    // `gpt-5.6-terra(high)`: the suffix is a thinking setting, not part of
    // the model — routing, cooldowns and the catalog use the base name
    // (CLIProxyAPI `ParseSuffix(...).ModelName`).
    let requested_full = requested_model(&path, &body).unwrap_or_default();
    let model = (!requested_full.is_empty())
        .then(|| crate::thinking::parse_suffix(&requested_full).model_name);
    // Routing picks members by their model LISTS (CLIProxyAPI's registry is
    // populated at start-up; ours is fetched): make sure every enabled
    // member's listing has been fetched — a no-op while they are fresh, and
    // a failed one is not retried for a minute.
    if model.is_some() {
        catalog_for_request().await;
    }
    let plan_model = model.clone();
    let plan = match tokio::task::spawn_blocking(move || {
        routing_plan(&plan_cfg, protocol, plan_model.as_deref())
    })
    .await
    {
        Ok(plan) => plan,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "routing failed",
            )
        }
    };
    let mut plan = plan;
    order_by_quota(&mut plan.members);
    spawn_quota_refresh(&plan.members);
    let model_key = model.clone().unwrap_or_default();
    if plan.members.is_empty() {
        if plan.model_unknown {
            // CLIProxyAPI `getRequestDetailsWithOptions`: an unknown model.
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({ "error": {
                    "message": format!("unknown provider for model {model_key}"),
                    "type": "invalid_request_error",
                    "code": "model_not_found",
                    "param": "model",
                }}),
            );
        }
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_upstream",
            "no enabled upstream speaks this API — add one on the Router page",
        );
    }

    // The client body as JSON, for upstreams that need it TRANSLATED into
    // their own API (parsed once; a pass-through never needs it).
    let client_json: Option<JsonValue> = serde_json::from_slice(&body).ok();
    // A Gemini client streams by METHOD (`:streamGenerateContent`), every
    // other API by the body's `stream` flag.
    let client_stream = if protocol == Protocol::Gemini {
        path.contains(":streamGenerateContent")
    } else {
        client_json
            .as_ref()
            .and_then(|b| b.get("stream"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };

    // CLIProxyAPI `Manager.Execute`: up to REQUEST_RETRY extra rounds; each
    // round walks the eligible members once (fill-first or round-robin),
    // skipping members blocked FOR THIS MODEL. A request-fault answer goes
    // straight back to the client; any other failure cools (member, model)
    // and moves on. A new round starts only after a round-retry status or a
    // transport error, after waiting for the soonest member to free up —
    // never longer than MAX_RETRY_INTERVAL.
    let stick_key = format!("{model_key}|{}", conversation_id(&headers, &body));
    let mut sticky = sticky_member(
        &stick_key,
        &plan.members,
        &model_key,
        within_turn(protocol, client_json.as_ref()),
        cfg.strategy == Strategy::RoundRobin,
    );
    let mut last_upstream: Option<Response<OutBody>> = None;
    let mut last_error: Option<String> = None;
    let mut attempted_any = false;
    // Members that failed WITHOUT a cooldown — in transport (connect,
    // timeout, a stream broken before its first payload), a shape refusal,
    // an unresolvable credential, a failed pre-use refresh, no translator —
    // would be picked again at once by a later round (and their zero wait
    // would make the rounds skip waiting for the members that ARE cooling
    // down). A member that failed is never re-sent within one request
    // (CLAUDE.md); rounds retry only members cooling down.
    let mut failed_without_cooldown: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for round in 0..=REQUEST_RETRY {
        let mut tried: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut round_retryable = false;
        let mut round_429: std::collections::HashSet<String> = std::collections::HashSet::new();
        'member: loop {
            let eligible: Vec<&str> = plan
                .members
                .iter()
                .map(|(k, _)| k.as_str())
                .filter(|k| {
                    !tried.contains(*k)
                        && !failed_without_cooldown.contains(*k)
                        && model_blocked(k, &model_key).is_none()
                })
                .collect();
            let kept = sticky
                .take()
                .and_then(|k| eligible.iter().copied().find(|e| *e == k));
            let Some(key) = kept.or_else(|| pick_member(cfg.strategy, &model_key, &eligible))
            else {
                break;
            };
            tried.insert(key.to_string());
            let resolved = &plan
                .members
                .iter()
                .find(|(k, _)| k == key)
                .expect("picked from the plan")
                .1;
            let mut upstream: Upstream = match resolved {
                Ok(u) => u.clone(),
                Err(why) => {
                    record_skip(key, why.clone());
                    last_error = Some(format!("{}: {why}", public_key(key)));
                    failed_without_cooldown.insert(key.to_string());
                    continue;
                }
            };
            // A login close to expiry is refreshed BEFORE use.
            if let Some(login) = upstream.login.clone() {
                let due = upstream.expires_at.is_some_and(|e| {
                    e <= now_secs() as i64 + crate::accounts::router_refresh_margin_secs()
                });
                if due {
                    match refresh_and_resolve(&ctx, key, &login, upstream.protocol, None).await {
                        Ok(u) => upstream = u,
                        // The margin fires minutes BEFORE expiry: a refresh
                        // that failed leaves a token that still works.
                        Err(_) if !token_expired(upstream.expires_at) => {
                            log::warn!(
                                "router: refresh of {} failed, using the current token",
                                public_key(key)
                            );
                        }
                        Err(why) => {
                            record_skip(key, why.clone());
                            last_error = Some(format!("{}: {why}", public_key(key)));
                            failed_without_cooldown.insert(key.to_string());
                            continue;
                        }
                    }
                }
            }
            // An upstream speaking another API gets the request TRANSLATED
            // into it (and the answer translated back).
            let translated = upstream.protocol != protocol;
            // `/v1/completions` and `/v1/embeddings` ride the Chat protocol
            // only to reach OpenAI-compatible members AS THEY ARE: no
            // translator speaks them (they would be rewritten as a chat
            // request and fail). A member that would need translating is
            // passed over, without a cooldown.
            if translated && untranslatable_path(&path) {
                failed_without_cooldown.insert(key.to_string());
                last_error = Some(format!(
                    "{}: does not serve {path} natively",
                    public_key(key)
                ));
                continue;
            }
            let (send_path, send_body, send_headers) = if translated {
                let Some(client_body) = client_json.as_ref() else {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "the request body is not JSON",
                    );
                };
                let Some(t) = crate::translate::request(
                    protocol,
                    upstream.protocol,
                    &model_key,
                    client_body,
                    client_stream,
                ) else {
                    record_skip(key, "no translator for this API pair".to_string());
                    failed_without_cooldown.insert(key.to_string());
                    continue;
                };
                let mut h = headers.clone();
                // The client's own API headers mean nothing to another API.
                for n in ["anthropic-version", "anthropic-beta", "content-length"] {
                    h.remove(n);
                }
                h.insert(
                    hyper::header::CONTENT_TYPE,
                    hyper::header::HeaderValue::from_static("application/json"),
                );
                (
                    crate::translate::upstream_path(upstream.protocol).to_string(),
                    Bytes::from(serde_json::to_vec(&t).unwrap_or_default()),
                    h,
                )
            } else {
                (path_and_query.clone(), body.clone(), headers.clone())
            };
            let out = Outgoing {
                method: &method,
                protocol: upstream.protocol,
                path_and_query: &send_path,
                body: &send_body,
                headers: &send_headers,
                client_stream,
                passthrough: !translated,
                requested_model: &requested_full,
                base_model: &model_key,
                client_protocol: protocol,
                source_body: client_json.as_ref(),
                fixes: &crate::autofix::BodyFixes::default(),
            };
            attempted_any = true;
            // magpie's repairs: a refusal that names something the router
            // can fix resends the SAME member a corrected body; fields a
            // member refused before are left out up front.
            let mut fixes = remembered_fixes(key, upstream.protocol);
            let failure = 'send: loop {
                let out = Outgoing {
                    fixes: &fixes,
                    ..out
                };
                let (builder, mut codex_req, mut sent_json) =
                    build_upstream_request(&ctx, &out, &upstream);
                let mut sent = builder.send().await;
                // CLIProxyAPI `tryRefreshAfterUnauthorized`: a 401 from a
                // refreshable login refreshes once and re-sends to the SAME
                // member.
                if let (Ok(resp), Some(login)) = (&sent, upstream.login.clone()) {
                    if resp.status().as_u16() == 401 {
                        let stale = auth_token(&upstream.auth).map(str::to_string);
                        if let Ok(u) = refresh_and_resolve(
                            &ctx,
                            key,
                            &login,
                            upstream.protocol,
                            stale.as_deref(),
                        )
                        .await
                        {
                            upstream = u;
                            let (b, c, j) = build_upstream_request(&ctx, &out, &upstream);
                            codex_req = c;
                            sent_json = j;
                            sent = b.send().await;
                        }
                    }
                }
                let anthropic = upstream.protocol == Protocol::Anthropic;
                break 'send match sent {
                    Ok(resp) if resp.status().is_success() => {
                        let delivery = Delivery {
                            client: protocol,
                            upstream: upstream.protocol,
                            translated,
                            original: client_json.as_ref(),
                            client_stream,
                            codex: codex_req.is_some(),
                            unwrap_codex: codex_req.as_ref().is_some_and(|r| r.unwrap_stream),
                            model: &model_key,
                            fix_grok_anthropic: protocol == Protocol::Anthropic
                                && !translated
                                && matches!(upstream.auth, Auth::GrokOauth { .. }),
                        };
                        match deliver(resp, &delivery).await {
                            Ok(out) => {
                                mark_model_success(key, &model_key);
                                remember_fixes(key, upstream.protocol, &fixes);
                                remember_stick(&stick_key, key);
                                ctx.emit_changed();
                                return out;
                            }
                            // A stream that failed before its first payload
                            // (xAI reports foreign reasoning that way).
                            Err(f) => match f.status.and_then(|st| {
                                next_fix_for(sent_json.as_ref(), st, &f.text, &fixes, anthropic)
                            }) {
                                Some(n) => {
                                    log::info!(
                                        "router: repairing the request for {}",
                                        public_key(key)
                                    );
                                    fixes = n;
                                    continue 'send;
                                }
                                // The REQUEST is at fault (a 400-class error
                                // delivered inside the stream, e.g. context
                                // too long): hand it back like the same error
                                // as an ordinary answer — no cooldown, no
                                // other member, which would fail alike.
                                None if f.request_scoped
                                    || f.status.is_some_and(|st| {
                                        is_request_invalid(st, &f.text)
                                            && !crate::autofix::is_shape_refusal(st, &f.text)
                                    }) =>
                                {
                                    ctx.emit_changed();
                                    let status = f.status.unwrap_or(400);
                                    let body = if serde_json::from_str::<JsonValue>(&f.text).is_ok()
                                    {
                                        f.text
                                    } else {
                                        json!({ "error": {
                                            "type": "invalid_request_error",
                                            "message": f.text,
                                        }})
                                        .to_string()
                                    };
                                    let mut h = reqwest::header::HeaderMap::new();
                                    h.insert(
                                        reqwest::header::CONTENT_TYPE,
                                        reqwest::header::HeaderValue::from_static(
                                            "application/json",
                                        ),
                                    );
                                    return buffered_response(status, &h, body);
                                }
                                None => f,
                            },
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let headers_out = resp.headers().clone();
                        let retry = retry_after(&headers_out);
                        let text = resp.text().await.unwrap_or_default();
                        if let Some(n) =
                            next_fix_for(sent_json.as_ref(), status, &text, &fixes, anthropic)
                        {
                            log::info!("router: repairing the request for {}", public_key(key));
                            fixes = n;
                            continue 'send;
                        }
                        // magpie `shapeWords` / `wrongEndpoint`: this member
                        // does not take the request's shape — another may.
                        // Next member, no cooldown.
                        if crate::autofix::is_shape_refusal(status, &text) {
                            last_error = Some(format!("{}: HTTP {status}", public_key(key)));
                            last_upstream = Some(buffered_response(status, &headers_out, text));
                            failed_without_cooldown.insert(key.to_string());
                            continue 'member;
                        }
                        if codex_req.is_some() {
                            // The Codex executor's own status classification
                            // (usage limits → credential-wide 429, capacity, …).
                            let ce = crate::codex_exec::codex_status_error(status, &text);
                            if ce.request_scoped || is_request_invalid(ce.status, &ce.message) {
                                ctx.emit_changed();
                                return buffered_response(status, &headers_out, text);
                            }
                            last_upstream =
                                Some(buffered_response(status, &headers_out, text.clone()));
                            let mut f = Failure::from_codex(ce);
                            if f.retry_after.is_none() {
                                f.retry_after = retry;
                            }
                            f
                        } else {
                            if is_request_invalid(status, &text) {
                                // The request is at fault: relay it, no
                                // cooldown, no other member (it would fail
                                // the same way).
                                ctx.emit_changed();
                                return buffered_response(status, &headers_out, text);
                            }
                            last_upstream =
                                Some(buffered_response(status, &headers_out, text.clone()));
                            Failure {
                                status: Some(status),
                                retry_after: retry,
                                text: if text.trim().is_empty() {
                                    format!("HTTP {status}")
                                } else {
                                    text
                                },
                                transport: false,
                                credential_scoped: false,
                                request_scoped: false,
                            }
                        }
                    }
                    Err(err) => {
                        let class = if err.is_connect() {
                            "connect failed"
                        } else if err.is_timeout() {
                            "timed out"
                        } else {
                            "request failed"
                        };
                        Failure {
                            status: None,
                            retry_after: None,
                            text: class.to_string(),
                            transport: true,
                            credential_scoped: false,
                            request_scoped: false,
                        }
                    }
                };
            };
            log::warn!(
                "router: upstream {} failed ({}), trying the next",
                public_key(key),
                failure
                    .status
                    .map_or_else(|| "no response".to_string(), |s| format!("HTTP {s}"))
            );
            if failure.transport || failure.status.is_some_and(is_retry_round_status) {
                round_retryable = true;
            }
            if failure.status == Some(429) {
                round_429.insert(key.to_string());
            }
            if failure.transport {
                failed_without_cooldown.insert(key.to_string());
            }
            mark_model_failure(key, &model_key, &failure);
            last_error = Some(format!("{}: {}", public_key(key), failure.text));
        }
        ctx.emit_changed();
        if !round_retryable || round == REQUEST_RETRY {
            break;
        }
        // Nobody left to retry: every member failed in transport.
        if plan
            .members
            .iter()
            .all(|(k, r)| r.is_err() || failed_without_cooldown.contains(k))
        {
            break;
        }
        // `closestCooldownWaitWithAttempted`: the soonest any member frees
        // up (a member that just answered 429 waits at least 10 s).
        let now = now_millis();
        let wait_ms = plan
            .members
            .iter()
            .filter(|(k, r)| r.is_ok() && !failed_without_cooldown.contains(k))
            .map(|(k, _)| {
                let until = model_blocked(k, &model_key).map_or(now, |(u, _)| u);
                let mut w = until.saturating_sub(now);
                if round_429.contains(k) {
                    w = w.max(QUOTA_FLOOR.as_millis() as u64);
                }
                w
            })
            .min()
            .unwrap_or(0);
        if wait_ms > MAX_RETRY_INTERVAL.as_millis() as u64 {
            break;
        }
        // Jitter: up to min(wait/4, 2 s) on top.
        let jitter = (now % 1000).min(wait_ms / 4).min(2000);
        tokio::time::sleep(Duration::from_millis(wait_ms + jitter)).await;
    }

    if let Some(resp) = last_upstream {
        return resp;
    }
    if !attempted_any {
        // Nothing was sent: every candidate is blocked for this model.
        let now = now_millis();
        let blocks: Vec<(u64, bool)> = plan
            .members
            .iter()
            .filter_map(|(k, _)| model_blocked(k, &model_key))
            .collect();
        if !blocks.is_empty() && blocks.len() == plan.members.len() {
            let soonest = blocks.iter().map(|(u, _)| *u).min().unwrap_or(now);
            let secs = soonest.saturating_sub(now).div_ceil(1000);
            if blocks.iter().all(|(_, quota)| *quota) {
                // CLIProxyAPI `modelCooldownError`.
                let mut resp = json_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({ "error": {
                        "code": "model_cooldown",
                        "message": format!("All credentials for model {model_key} are cooling down"),
                        "model": model_key,
                        "reset_seconds": secs,
                    }}),
                );
                if let Ok(v) = hyper::header::HeaderValue::from_str(&secs.to_string()) {
                    resp.headers_mut().insert("retry-after", v);
                }
                return resp;
            }
            let mut resp = error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_unavailable",
                &format!("no auth available (model={model_key})"),
            );
            if let Ok(v) = hyper::header::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert("retry-after", v);
            }
            return resp;
        }
    }
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "auth_unavailable",
        &last_error.unwrap_or_else(|| "no auth available".to_string()),
    )
}

/// A token-count request (`token_count.rs`, port of the executors'
/// `CountTokens` + `TranslateTokenCount`). The same candidates and order as a
/// real request for that model; the first member that can answer does:
/// Codex / OpenAI-compatible / Grok count LOCALLY (tiktoken) on the body
/// translated into their format; Claude counts locally too unless it is
/// Anthropic's own API with a key, which — like Gemini — is asked through
/// its own count endpoint. The number is answered in the client's shape.
async fn handle_count(
    ctx: &Ctx,
    cfg: &RouterConfig,
    client: Protocol,
    path: &str,
    body: &Bytes,
    headers: &hyper::HeaderMap,
) -> Response<OutBody> {
    use crate::token_count::{LocalCounter, RemoteCounter};
    let Ok(client_body) = serde_json::from_slice::<JsonValue>(body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request body is not JSON",
        );
    };
    let full = requested_model(path, body).unwrap_or_default();
    let model = crate::thinking::parse_suffix(&full).model_name;
    if !model.is_empty() {
        catalog_for_request().await;
    }
    let plan_cfg = cfg.clone();
    let plan_model = model.clone();
    let plan = match tokio::task::spawn_blocking(move || {
        routing_plan(
            &plan_cfg,
            client,
            (!plan_model.is_empty()).then_some(plan_model.as_str()),
        )
    })
    .await
    {
        Ok(p) => p,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "routing failed",
            )
        }
    };
    if plan.members.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": {
                "message": format!("unknown provider for model {model}"),
                "type": "invalid_request_error",
                "code": "model_not_found",
                "param": "model",
            }}),
        );
    }
    let client_format = thinking_format(client);
    let mut last_error = "no member could count tokens".to_string();
    for (key, resolved) in &plan.members {
        if model_blocked(key, &model).is_some() {
            continue;
        }
        let Ok(upstream) = resolved else { continue };
        // The body in the upstream's own format.
        let up_body = if upstream.protocol == client {
            client_body.clone()
        } else {
            match crate::translate::request(client, upstream.protocol, &model, &client_body, false)
            {
                Some(b) => b,
                None => continue,
            }
        };
        let local = match (&upstream.auth, upstream.protocol) {
            (Auth::ChatGpt { .. }, _) => Some(LocalCounter::Codex),
            (Auth::GrokOauth { .. }, _) => Some(LocalCounter::Xai),
            (Auth::ApiKey(_), Protocol::OpenaiResponses) => Some(LocalCounter::Codex),
            (Auth::ApiKey(_), Protocol::OpenaiChat) => Some(LocalCounter::OpenAICompat),
            (Auth::ApiKey(k), Protocol::Anthropic) => {
                (!crate::token_count::should_use_claude_upstream_token_count(k, &upstream.base_url))
                    .then_some(LocalCounter::Claude)
            }
            (Auth::ApiKey(_), Protocol::Gemini) => None,
        };
        if let Some(counter) = local {
            // The counter wants the body in ITS format.
            let want = counter.body_format();
            let body_for = if want == thinking_format(upstream.protocol)
                || (want == "codex" && upstream.protocol == Protocol::OpenaiResponses)
            {
                up_body.clone()
            } else {
                let target = match want {
                    "openai" => Protocol::OpenaiChat,
                    "claude" => Protocol::Anthropic,
                    _ => Protocol::OpenaiResponses,
                };
                if target == client {
                    client_body.clone()
                } else {
                    match crate::translate::request(client, target, &model, &client_body, false) {
                        Some(b) => b,
                        None => continue,
                    }
                }
            };
            match crate::token_count::count_locally(counter, &model, &body_for) {
                Ok(n) => {
                    let raw = crate::token_count::local_usage_json(counter, n);
                    return json_response(
                        StatusCode::OK,
                        crate::token_count::client_count_response(want, client_format, n, &raw),
                    );
                }
                Err(e) => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("{e:?}"),
                    );
                }
            }
        }
        // The upstream counts itself.
        let (counter, Auth::ApiKey(api_key)) = (
            if upstream.protocol == Protocol::Gemini {
                RemoteCounter::Gemini
            } else {
                RemoteCounter::Claude
            },
            &upstream.auth,
        ) else {
            continue;
        };
        let base = upstream.base_url.trim().trim_end_matches('/');
        let base = base.strip_suffix("/v1beta").unwrap_or(base);
        let base = base.strip_suffix("/v1").unwrap_or(base);
        let req = crate::token_count::upstream_count_request(counter, base, &model, &up_body);
        let mut builder = ctx.client.post(&req.url).json(&req.body);
        builder = match counter {
            RemoteCounter::Gemini => builder.header("x-goog-api-key", api_key.as_str()),
            RemoteCounter::Claude => {
                let mut betas: Vec<String> = headers
                    .get("anthropic-beta")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.split(',').map(|b| b.trim().to_string()).collect())
                    .unwrap_or_default();
                betas.extend(req.extra_betas.iter().cloned());
                betas.retain(|b| !b.is_empty());
                betas.dedup();
                builder
                    .header("x-api-key", api_key.as_str())
                    .header(
                        "anthropic-version",
                        headers
                            .get("anthropic-version")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("2023-06-01"),
                    )
                    .header("anthropic-beta", betas.join(","))
            }
        };
        match builder.send().await {
            Ok(resp) if resp.status().is_success() => {
                let answer: JsonValue = resp.json().await.unwrap_or(JsonValue::Null);
                let n = crate::token_count::parse_upstream_count(counter, &answer);
                return json_response(
                    StatusCode::OK,
                    crate::token_count::client_count_response(
                        counter.format(),
                        client_format,
                        n,
                        &answer,
                    ),
                );
            }
            Ok(resp) => {
                last_error = format!("{}: HTTP {}", public_key(key), resp.status().as_u16())
            }
            Err(_) => last_error = format!("{}: request failed", public_key(key)),
        }
    }
    error_response(StatusCode::BAD_GATEWAY, "upstream_unavailable", &last_error)
}

/// CLIProxyAPI `request-retry` / `max-retry-interval` (config.example.yaml).
const REQUEST_RETRY: usize = 3;
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// How a successful upstream answer reaches the client.
struct Delivery<'a> {
    client: Protocol,
    upstream: Protocol,
    translated: bool,
    original: Option<&'a JsonValue>,
    client_stream: bool,
    /// The ChatGPT Codex backend (always streams).
    codex: bool,
    /// The client did not ask to stream but the Codex backend does.
    unwrap_codex: bool,
    /// The requested model (the Codex relay fills it into response events).
    model: &'a str,
    fix_grok_anthropic: bool,
}

/// Turn a 2xx answer into the client response. A STREAM is only handed
/// over once its first payload has arrived and is not an error
/// (`readStreamBootstrap`): until then the member can still fail over. A
/// folded non-stream answer is checked the same way.
async fn deliver(
    mut resp: reqwest::Response,
    d: &Delivery<'_>,
) -> Result<Response<OutBody>, Failure> {
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let json_body = ct.starts_with("application/json");
    let streamed = ct.starts_with("text/event-stream") || (d.codex && !json_body);
    let whole = |text: String| -> Result<JsonValue, Failure> {
        if d.codex {
            // CLIProxyAPI Codex executor: the non-stream answer is assembled
            // from the SSE, with its own terminal/error classification.
            return crate::codex_exec::assemble_non_stream_detailed(&text)
                .map_err(Failure::from_codex);
        }
        if streamed && d.upstream == Protocol::OpenaiResponses {
            let (status, folded) = fold_responses_sse(&text);
            if status != StatusCode::OK {
                return Err(Failure {
                    status: in_stream_error_status(&folded),
                    retry_after: None,
                    text: folded.to_string(),
                    transport: false,
                    credential_scoped: false,
                    request_scoped: false,
                });
            }
            Ok(folded)
        } else {
            serde_json::from_str(&text).map_err(|_| Failure {
                status: None,
                retry_after: None,
                text: "the upstream answered with something that is not JSON".into(),
                transport: false,
                credential_scoped: false,
                request_scoped: false,
            })
        }
    };
    let read_all = |r: reqwest::Response| async move {
        r.text().await.map_err(|_| Failure {
            status: None,
            retry_after: None,
            text: "the upstream response could not be read".into(),
            transport: true,
            credential_scoped: false,
            request_scoped: false,
        })
    };
    if d.translated {
        let original = d.original.cloned().unwrap_or(JsonValue::Null);
        if d.client_stream && streamed {
            let codex_model = d.codex.then_some(d.model);
            let prefix = bootstrap(&mut resp, codex_model).await?;
            return Ok(translated_stream(
                prefix,
                resp,
                d.client,
                d.upstream,
                &original,
                codex_model,
            ));
        }
        let obj = whole(read_all(resp).await?)?;
        let out =
            crate::translate::non_stream(d.client, d.upstream, &obj, &original).unwrap_or(obj);
        return Ok(json_response(StatusCode::OK, out));
    }
    if d.unwrap_codex && streamed {
        let obj = whole(read_all(resp).await?)?;
        return Ok(json_response(StatusCode::OK, obj));
    }
    if streamed && d.codex {
        let prefix = bootstrap(&mut resp, Some(d.model)).await?;
        return Ok(codex_relay_stream(prefix, resp, d.model));
    }
    if streamed && d.upstream == Protocol::Gemini {
        let prefix = bootstrap(&mut resp, None).await?;
        return Ok(gemini_relay_stream(prefix, resp));
    }
    if streamed {
        let prefix = bootstrap(&mut resp, None).await?;
        return Ok(if d.fix_grok_anthropic {
            relay_fixing_anthropic_indexes(prefix, resp)
        } else {
            relay_prefixed(prefix, resp)
        });
    }
    Ok(relay(resp))
}

/// `readStreamBootstrap`: read until the first complete SSE event that
/// carries a payload. A stream that ends first, or whose first payload is
/// an error event, is a FAILURE (the member is cooled and the next tried);
/// the bytes read are handed back to be sent ahead of the rest.
async fn bootstrap(
    resp: &mut reqwest::Response,
    codex_model: Option<&str>,
) -> Result<Bytes, Failure> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                if let Some(first) = first_sse_payload(&buf) {
                    // A Codex stream's first event is judged the executor's
                    // way (terminal failures, usage limits, …).
                    if let Some(m) = codex_model {
                        let mut relay = crate::codex_exec::CodexStreamRelay::new(m, false);
                        if let crate::codex_exec::RelayStep::Failed(e) = relay.on_event(&first) {
                            return Err(Failure::from_codex(e));
                        }
                        return Ok(Bytes::from(buf));
                    }
                    let kind = first.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if kind == "error" || kind == "response.failed" {
                        return Err(Failure {
                            status: in_stream_error_status(&first),
                            retry_after: None,
                            text: first.to_string(),
                            transport: false,
                            credential_scoped: false,
                            request_scoped: false,
                        });
                    }
                    return Ok(Bytes::from(buf));
                }
            }
            Ok(None) => {
                return Err(Failure {
                    status: None,
                    retry_after: None,
                    text: "empty_stream: the upstream stream ended before any payload".into(),
                    transport: false,
                    credential_scoped: false,
                    request_scoped: false,
                });
            }
            Err(_) => {
                return Err(Failure {
                    status: None,
                    retry_after: None,
                    text: "the upstream stream broke before any payload".into(),
                    transport: true,
                    credential_scoped: false,
                    request_scoped: false,
                });
            }
        }
    }
}

/// OpenAI paths no translator covers: only a member speaking the client's
/// API natively may serve them.
fn untranslatable_path(path: &str) -> bool {
    path.starts_with("/v1/completions") || path.starts_with("/v1/embeddings")
}

/// The HTTP status an error delivered INSIDE a 200 stream stands for, so
/// the repairs (`autofix`) and the cooldown table can read it like an
/// ordinary error answer: an explicit `status` when the event carries one,
/// else what its error type/code names. `None` when it names nothing known.
/// xAI reports a foreign sealed reasoning item this way (magpie
/// `foreignReasoning`: "as the stream's error, which may be read as another
/// status").
fn in_stream_error_status(event: &JsonValue) -> Option<u16> {
    let err = event
        .get("error")
        .or_else(|| event.pointer("/response/error"))
        .unwrap_or(event);
    for v in [event.get("status"), err.get("status"), err.get("code")] {
        if let Some(n) = v.and_then(|v| v.as_u64()) {
            if (400..600).contains(&n) {
                return Some(n as u16);
            }
        }
    }
    let name = [err.get("type"), err.get("code")]
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if name.contains("invalid_request")
        || name.contains("invalid_encrypted_content")
        || name.contains("invalid_prompt")
        || name.contains("context_length")
    {
        Some(400)
    } else if name.contains("authentication") {
        Some(401)
    } else if name.contains("permission") {
        Some(403)
    } else if name.contains("not_found") {
        Some(404)
    } else if name.contains("rate_limit") || name.contains("usage_limit") {
        Some(429)
    } else if name.contains("overloaded") {
        Some(529)
    } else if name.contains("server_error") || name.contains("api_error") {
        Some(500)
    } else {
        None
    }
}

/// The first COMPLETE SSE event's JSON payload, if the buffer holds one.
fn first_sse_payload(buf: &[u8]) -> Option<JsonValue> {
    let text = String::from_utf8_lossy(buf).replace("\r\n", "\n");
    // Only the events already terminated by a blank line are complete.
    let complete = &text[..text.rfind("\n\n")?];
    for block in complete.split("\n\n") {
        let data: Vec<&str> = block
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|v| v.strip_prefix(' ').unwrap_or(v))
            .collect();
        if data.is_empty() {
            continue;
        }
        let payload = data.join("\n");
        if payload.trim() == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<JsonValue>(&payload) {
            return Some(v);
        }
    }
    None
}

/// Tool types xAI's Responses endpoint accepts (its own 422 lists them:
/// `function`, `web_search`, `x_search`, `image_generation`, `co…`). Codex
/// sends its MCP servers as `namespace` tools, which xAI rejects outright —
/// so for a Grok upstream those are dropped: the request goes through
/// without the MCP tools instead of failing entirely. Only `namespace` is
/// removed; everything else reaches xAI unchanged.
pub fn strip_unsupported_tools(raw: &[u8]) -> Bytes {
    let Ok(mut doc) = serde_json::from_slice::<JsonValue>(raw) else {
        return Bytes::copy_from_slice(raw);
    };
    let Some(JsonValue::Array(tools)) = doc.get_mut("tools") else {
        return Bytes::copy_from_slice(raw);
    };
    let before = tools.len();
    tools.retain(|t| t.get("type").and_then(|v| v.as_str()) != Some("namespace"));
    if tools.len() == before {
        return Bytes::copy_from_slice(raw);
    }
    log::info!(
        "router: dropped {} namespace tool(s) the Grok upstream does not accept",
        before - tools.len()
    );
    Bytes::from(doc.to_string())
}

/// Add the `index` Anthropic's stream grammar requires on
/// `content_block_delta` / `content_block_stop` when an upstream omits it —
/// Grok's backend does (seen live: `content_block_start` carries it, the
/// deltas do not), which breaks every spec-following client. The index is
/// the one the latest `content_block_start` announced. Lines are processed
/// whole; a partial line waits for the rest of it.
pub struct AnthropicIndexFixer {
    pending: Vec<u8>,
    current: i64,
}

impl AnthropicIndexFixer {
    pub fn new() -> Self {
        AnthropicIndexFixer {
            pending: Vec::new(),
            current: 0,
        }
    }

    /// Feed a chunk; returns the complete lines (fixed) it finished.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let Some(last_nl) = self.pending.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let rest = self.pending.split_off(last_nl + 1);
        let complete = std::mem::replace(&mut self.pending, rest);
        let mut out = Vec::with_capacity(complete.len() + 32);
        for line in complete.split_inclusive(|b| *b == b'\n') {
            out.extend_from_slice(&self.fix_line(line));
        }
        out
    }

    /// Whatever is left at end of stream, as-is.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn fix_line(&mut self, line: &[u8]) -> Vec<u8> {
        let Some(data) = line.strip_prefix(b"data:") else {
            return line.to_vec();
        };
        let text = String::from_utf8_lossy(data);
        let trimmed = text.trim();
        let Ok(mut v) = serde_json::from_str::<JsonValue>(trimmed) else {
            return line.to_vec();
        };
        let kind = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if kind == "content_block_start" {
            if let Some(i) = v.get("index").and_then(|i| i.as_i64()) {
                self.current = i;
            }
            return line.to_vec();
        }
        if (kind == "content_block_delta" || kind == "content_block_stop")
            && v.get("index").is_none()
        {
            if let Some(o) = v.as_object_mut() {
                o.insert("index".into(), JsonValue::from(self.current));
            }
            let mut fixed = b"data: ".to_vec();
            fixed.extend_from_slice(v.to_string().as_bytes());
            fixed.push(b'\n');
            return fixed;
        }
        line.to_vec()
    }
}

impl Default for AnthropicIndexFixer {
    fn default() -> Self {
        Self::new()
    }
}

/// `relay`, with the Anthropic stream repaired on the way through.
fn relay_fixing_anthropic_indexes(prefix: Bytes, resp: reqwest::Response) -> Response<OutBody> {
    use futures_util::StreamExt;
    let status = resp.status();
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in resp.headers().iter() {
        if is_dropped_response_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let fixer = Arc::new(Mutex::new(AnthropicIndexFixer::new()));
    let tail_fixer = fixer.clone();
    let fixed = futures_util::stream::once(async move { Ok::<Bytes, reqwest::Error>(prefix) })
        .chain(resp.bytes_stream())
        .map(move |chunk| {
            chunk
                .map(|b| Bytes::from(fixer.lock().unwrap_or_else(|e| e.into_inner()).feed(&b)))
                .map_err(std::io::Error::other)
        })
        .chain(futures_util::stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from(
                tail_fixer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .finish(),
            ))
        }))
        .filter(|r| std::future::ready(!matches!(r, Ok(b) if b.is_empty())))
        .map(|r| r.map(Frame::data));
    let body: OutBody = BodyExt::boxed(StreamBody::new(fixed));
    builder.body(body).unwrap_or_else(|_| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "relay_error",
            "bad upstream headers",
        )
    })
}

/// An upstream response already read into memory (small error bodies).
fn buffered_response(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: String,
) -> Response<OutBody> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if is_dropped_response_header(name.as_str()) || name.as_str() == "content-encoding" {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder.body(full(body)).unwrap_or_else(|_| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "relay_error",
            "bad upstream headers",
        )
    })
}

/// Stream an upstream response back as-is: status, headers (minus framing),
/// body chunk by chunk so SSE arrives live.
/// A streamed answer from an upstream spoken to in ANOTHER API, run through
/// the pair's stream translator event by event (`prefix` = the bytes the
/// bootstrap already read).
fn translated_stream(
    prefix: Bytes,
    resp: reqwest::Response,
    client: Protocol,
    upstream: Protocol,
    original: &JsonValue,
    codex_model: Option<&str>,
) -> Response<OutBody> {
    let Some(tx) = crate::translate::StreamTx::new(client, upstream, original) else {
        return relay_prefixed(prefix, resp);
    };
    // Generic over the CONCRETE upstream stream: erasing it to a
    // `dyn Stream + Send` would drop the `Sync` the response body needs.
    struct State<S> {
        upstream: std::pin::Pin<Box<S>>,
        prefix: Option<Bytes>,
        parser: crate::translate::SseParser,
        tx: crate::translate::StreamTx,
        /// A Codex upstream's events first go through the executor's relay
        /// (terminal / error handling), as CLIProxyAPI does.
        relay: Option<crate::codex_exec::CodexStreamRelay>,
        gemini: bool,
        finished: bool,
        ended: bool,
    }
    fn emit<S>(st: &mut State<S>, events: Vec<crate::translate::SseEvent>, out: &mut String) {
        for ev in events {
            match ev {
                crate::translate::SseEvent::Data(e, v) if !st.finished => {
                    // A Gemini upstream's chunks pass the executor's stream
                    // filter first (`FilterSSEUsageMetadata`); Null = drop.
                    let v = if st.gemini {
                        let v = crate::gemini_exec::rewrite_stream_event(&v);
                        if v.is_null() {
                            continue;
                        }
                        v
                    } else {
                        v
                    };
                    let step = match st.relay.as_mut() {
                        Some(r) => r.on_event(&v),
                        None => crate::codex_exec::RelayStep::Forward(v),
                    };
                    match step {
                        crate::codex_exec::RelayStep::Forward(v) => {
                            out.extend(st.tx.push(e.as_deref(), &v));
                        }
                        crate::codex_exec::RelayStep::Terminal(v) => {
                            out.extend(st.tx.push(e.as_deref(), &v));
                            st.finished = true;
                            out.extend(st.tx.finish());
                        }
                        crate::codex_exec::RelayStep::Failed(err) => {
                            out.extend(st.tx.push(Some("error"), &codex_error_event(&err)));
                            st.finished = true;
                            out.extend(st.tx.finish());
                        }
                    }
                }
                crate::translate::SseEvent::Done if !st.finished => {
                    st.finished = true;
                    out.extend(st.tx.finish());
                }
                _ => {}
            }
        }
    }
    let state = State {
        upstream: Box::pin(resp.bytes_stream()),
        prefix: Some(prefix),
        parser: crate::translate::SseParser::default(),
        tx,
        relay: codex_model.map(|m| crate::codex_exec::CodexStreamRelay::new(m, false)),
        gemini: upstream == Protocol::Gemini,
        finished: false,
        ended: false,
    };
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        use futures_util::StreamExt;
        loop {
            if st.ended {
                return None;
            }
            let mut out = String::new();
            let next = match st.prefix.take() {
                Some(p) => Some(Ok(p)),
                None => st.upstream.next().await,
            };
            match next {
                Some(Ok(chunk)) => {
                    let events = st.parser.feed(&chunk);
                    emit(&mut st, events, &mut out);
                }
                Some(Err(err)) => {
                    st.ended = true;
                    return Some((Err(std::io::Error::other(err)), st));
                }
                None => {
                    let events = st.parser.finish();
                    emit(&mut st, events, &mut out);
                    if !st.finished {
                        // A Codex stream that never reached a terminal event
                        // is an error (the executor's incomplete-stream 408).
                        if let Some(err) = st.relay.as_ref().and_then(|r| r.finish()) {
                            out.extend(st.tx.push(Some("error"), &codex_error_event(&err)));
                        }
                        st.finished = true;
                        out.extend(st.tx.finish());
                    }
                    st.ended = true;
                }
            }
            if st.finished && !st.ended {
                // Go stops reading at the terminal event.
                st.ended = true;
            }
            if !out.is_empty() {
                return Some((Ok(Frame::data(Bytes::from(out))), st));
            }
        }
    });
    let body: OutBody = StreamBody::new(stream).boxed();
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "relay_error", "bad stream"))
}

/// A Codex executor error as the backend's own `error` stream event, for a
/// translator (or a Responses client) to report.
fn codex_error_event(err: &crate::codex_exec::CodexError) -> JsonValue {
    let inner = serde_json::from_str::<JsonValue>(&err.message)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .unwrap_or_else(|| json!({ "message": err.message }));
    json!({ "type": "error", "error": inner, "status": err.status })
}

/// A Codex stream relayed to a Responses client through the port of the
/// executor's stream goroutine (`CodexStreamRelay`): each data event is
/// re-emitted (model filled in, output rebuilt at completion), the stream
/// ends at the terminal event, and an in-stream failure or a missing
/// terminal event is delivered as an `error` event.
fn codex_relay_stream(prefix: Bytes, resp: reqwest::Response, model: &str) -> Response<OutBody> {
    struct State<S> {
        upstream: std::pin::Pin<Box<S>>,
        prefix: Option<Bytes>,
        parser: crate::translate::SseParser,
        relay: crate::codex_exec::CodexStreamRelay,
        done: bool,
        ended: bool,
    }
    fn frame(event: Option<&str>, data: &JsonValue) -> String {
        let name = event.map(str::to_string).or_else(|| {
            data.get("type")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        });
        match name {
            Some(n) => format!("event: {n}\ndata: {data}\n\n"),
            None => format!("data: {data}\n\n"),
        }
    }
    let state = State {
        upstream: Box::pin(resp.bytes_stream()),
        prefix: Some(prefix),
        parser: crate::translate::SseParser::default(),
        relay: crate::codex_exec::CodexStreamRelay::new(model, false),
        done: false,
        ended: false,
    };
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        use futures_util::StreamExt;
        loop {
            if st.ended {
                return None;
            }
            let mut out = String::new();
            let next = match st.prefix.take() {
                Some(p) => Some(Ok(p)),
                None => st.upstream.next().await,
            };
            let events = match next {
                Some(Ok(chunk)) => st.parser.feed(&chunk),
                Some(Err(err)) => {
                    st.ended = true;
                    return Some((Err(std::io::Error::other(err)), st));
                }
                None => {
                    st.ended = true;
                    let mut ev = st.parser.finish();
                    if !st.done {
                        if let Some(err) = st.relay.finish() {
                            ev.push(crate::translate::SseEvent::Data(
                                Some("error".into()),
                                codex_error_event(&err),
                            ));
                        }
                    }
                    ev
                }
            };
            for ev in events {
                if st.done {
                    break;
                }
                match ev {
                    crate::translate::SseEvent::Data(e, v) => match st.relay.on_event(&v) {
                        crate::codex_exec::RelayStep::Forward(v) => {
                            out.push_str(&frame(e.as_deref(), &v))
                        }
                        crate::codex_exec::RelayStep::Terminal(v) => {
                            out.push_str(&frame(e.as_deref(), &v));
                            st.done = true;
                        }
                        crate::codex_exec::RelayStep::Failed(err) => {
                            out.push_str(&frame(Some("error"), &codex_error_event(&err)));
                            st.done = true;
                        }
                    },
                    crate::translate::SseEvent::Done => {
                        out.push_str("data: [DONE]\n\n");
                    }
                }
            }
            if st.done {
                st.ended = true;
            }
            if !out.is_empty() {
                return Some((Ok(Frame::data(Bytes::from(out))), st));
            }
        }
    });
    let body: OutBody = BodyExt::boxed(StreamBody::new(stream));
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "relay_error", "bad stream"))
}

/// A Gemini stream relayed to a Gemini client through the executor's stream
/// filter (`gemini_exec::rewrite_stream_event`, CLIProxyAPI
/// `FilterSSEUsageMetadata`): each chunk re-emitted as `data:`, dropped
/// when the filter says so.
fn gemini_relay_stream(prefix: Bytes, resp: reqwest::Response) -> Response<OutBody> {
    struct State<S> {
        upstream: std::pin::Pin<Box<S>>,
        prefix: Option<Bytes>,
        parser: crate::translate::SseParser,
        ended: bool,
    }
    let state = State {
        upstream: Box::pin(resp.bytes_stream()),
        prefix: Some(prefix),
        parser: crate::translate::SseParser::default(),
        ended: false,
    };
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        use futures_util::StreamExt;
        loop {
            if st.ended {
                return None;
            }
            let next = match st.prefix.take() {
                Some(p) => Some(Ok(p)),
                None => st.upstream.next().await,
            };
            let events = match next {
                Some(Ok(chunk)) => st.parser.feed(&chunk),
                Some(Err(err)) => {
                    st.ended = true;
                    return Some((Err(std::io::Error::other(err)), st));
                }
                None => {
                    st.ended = true;
                    st.parser.finish()
                }
            };
            let mut out = String::new();
            for ev in events {
                if let crate::translate::SseEvent::Data(_, v) = ev {
                    let v = crate::gemini_exec::rewrite_stream_event(&v);
                    if !v.is_null() {
                        out.push_str(&format!("data: {v}\n\n"));
                    }
                }
            }
            if !out.is_empty() {
                return Some((Ok(Frame::data(Bytes::from(out))), st));
            }
        }
    });
    let body: OutBody = BodyExt::boxed(StreamBody::new(stream));
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "relay_error", "bad stream"))
}

/// Relay a stream whose first bytes the bootstrap already read.
fn relay_prefixed(prefix: Bytes, resp: reqwest::Response) -> Response<OutBody> {
    use futures_util::StreamExt;
    let status = resp.status();
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in resp.headers().iter() {
        if is_dropped_response_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let stream = futures_util::stream::once(async move { Ok::<Bytes, reqwest::Error>(prefix) })
        .chain(resp.bytes_stream())
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    let body: OutBody = BodyExt::boxed(StreamBody::new(stream));
    builder.body(body).unwrap_or_else(|_| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "relay_error",
            "bad upstream headers",
        )
    })
}

fn relay(resp: reqwest::Response) -> Response<OutBody> {
    let status = resp.status();
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in resp.headers().iter() {
        if is_dropped_response_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let stream = resp
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    let body: OutBody = StreamBody::new(stream).boxed();
    builder.body(body).unwrap_or_else(|_| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "relay_error",
            "bad upstream headers",
        )
    })
}

/// `GET /v1/models`: the union of model ids the enabled upstreams declare
/// (a provider's `model` plus its `models` list, a gateway's detected
/// list). Only what the user configured — nothing is invented for the
/// OAuth logins, whose model set the vendor decides.
fn models_listing(cfg: &RouterConfig, anthropic: bool) -> JsonValue {
    let ids = available_model_ids(cfg);
    models_listing_shape(ids, anthropic)
}

/// CLIProxyAPI `GetAvailableModels`: a model whose every serving member is
/// cooling down / out of quota for it is not offered.
fn available_model_ids(cfg: &RouterConfig) -> Vec<(String, String)> {
    let stores = Stores::load();
    let keys = enabled_keys(cfg);
    let lists: Vec<(String, Vec<String>)> = keys
        .iter()
        .map(|k| (k.clone(), upstream_models(&stores, k).unwrap_or_default()))
        .collect();
    enabled_model_ids(cfg)
        .into_iter()
        .filter(|(id, _)| {
            let mut listers = lists.iter().filter(|(_, l)| l.contains(id)).peekable();
            listers.peek().is_none() || listers.any(|(k, _)| model_blocked(k, id).is_none())
        })
        .collect()
}

fn models_listing_shape(ids: Vec<(String, String)>, anthropic: bool) -> JsonValue {
    if anthropic {
        // CLIProxyAPI `convertModelToMap` (claude): Anthropic's list shape.
        let data: Vec<JsonValue> = ids
            .iter()
            .map(|(id, _)| {
                json!({ "id": id, "type": "model", "display_name": id,
                        "created_at": "1970-01-01T00:00:00Z" })
            })
            .collect();
        return json!({
            "data": data,
            "has_more": false,
            "first_id": ids.first().map(|(id, _)| id.clone()),
            "last_id": ids.last().map(|(id, _)| id.clone()),
        });
    }
    let data: Vec<JsonValue> = ids
        .into_iter()
        .map(|(id, owner)| json!({ "id": id, "object": "model", "owned_by": owner }))
        .collect();
    json!({ "object": "list", "data": data })
}

// ===================================================================
// "Use in tools": the router as a managed AI Gateway
// ===================================================================

/// The router is exposed to the CLIs the way a gateway is: ONE
/// `{baseUrl, apiKey}` entry in `providers.json` (`kind: "router"`, this
/// fixed id) whose per-CLI BINDINGS carry each tool's model / options and
/// activate through the ordinary gateway path. Termory owns the entry's
/// connection fields and capabilities; the user owns its bindings.
/// The entry is IDENTIFIED by `kind: "router"` (`is_router_entry`), never by
/// id: its id is an ordinary UUID minted when the entry is first created
/// (the gateway shape needs one). Mirror: frontend `isRouterGateway`.

/// Whether a providers.json entry is the router's own.
pub fn is_router_entry(g: &JsonValue) -> bool {
    g.get("kind").and_then(|v| v.as_str()) == Some(crate::config::ROUTER_KIND)
}
/// Stored name of the entry — a fixed English constant; every surface shows
/// the localized `router.title` for it instead (page and tray alike).
const ROUTER_GATEWAY_NAME: &str = "Local Router";

/// Model ids the enabled providers/gateways declare — what `/v1/models`
/// lists and what the gateway entry's `capabilities.models` autocompletes.
fn enabled_model_ids(cfg: &RouterConfig) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    enabled_model_entries(cfg)
        .into_iter()
        .filter(|(id, _)| seen.insert(id.clone()))
        .collect()
}

/// Every (model, source label) pair the enabled members declare, unmerged.
fn enabled_model_entries(cfg: &RouterConfig) -> Vec<(String, String)> {
    let enabled: std::collections::HashSet<&str> = cfg
        .upstreams
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.key.as_str())
        .collect();
    let mut ids: Vec<(String, String)> = Vec::new();
    let providers =
        crate::providers::providers_from_json(crate::config::read_providers().unwrap_or_default());
    // Settings → Tools: a switched-off tool's members are refused by routing
    // (`TOOL_DISABLED`), so their models are not advertised either.
    let disabled = crate::config::disabled_sources();
    let catalog = CATALOG.lock().unwrap_or_else(|e| e.into_inner());
    for key in &enabled {
        let (owner, app) = if key.starts_with("live:codex") || key.starts_with("account:codex:") {
            ("Codex", CliApp::Codex)
        } else if key.starts_with("live:grok") || key.starts_with("account:grok:") {
            ("Grok Build", CliApp::Grok)
        } else {
            continue;
        };
        if disabled.contains(app.key()) {
            continue;
        }
        if let Some((_, live)) = catalog.get(*key) {
            ids.extend(live.iter().map(|m| (m.clone(), owner.to_string())));
        }
    }
    for p in &providers {
        let key = format!("provider:{}", p.id);
        if !enabled.contains(key.as_str()) || disabled.contains(p.app.key()) {
            continue;
        }
        if !p.model.trim().is_empty() {
            ids.push((p.model.trim().to_string(), p.name.clone()));
        }
        for m in &p.models {
            ids.push((m.id.clone(), p.name.clone()));
        }
        // Everything the provider's own `/models` listing reported, when it
        // has been fetched (`refresh_catalog`) — the FULL supported set, not
        // just what the user typed into the provider.
        if let Some((_, live)) = catalog.get(&key) {
            ids.extend(live.iter().map(|m| (m.clone(), p.name.clone())));
        }
    }
    drop(catalog);
    if let Ok(JsonValue::Array(gws)) = crate::config::read_gateways() {
        for g in &gws {
            let Some(id) = g.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            if !enabled.contains(format!("gateway:{id}").as_str()) {
                continue;
            }
            let name = g.get("name").and_then(|v| v.as_str()).unwrap_or(id);
            let live = CATALOG
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&format!("gateway:{id}"))
                .map(|(_, m)| m.clone());
            let models = live.unwrap_or_else(|| detected_gateway_models(g));
            ids.extend(models.into_iter().map(|m| (m, name.to_string())));
        }
    }
    ids
}

/// One model the router offers, for the Router page's model list.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouterModel {
    pub id: String,
    /// The enabled sources that list it, in page order, without repeats.
    pub sources: Vec<String>,
    /// `false` while every member listing it is cooling down for it — the
    /// rule `/v1/models` hides it by (`available_model_ids`).
    pub available: bool,
}

/// What `/v1/models` serves, with each model's sources: the same members,
/// the same catalog, the same cooldown rule — only grouped per model.
fn router_models_list(cfg: &RouterConfig) -> Vec<RouterModel> {
    let available: std::collections::HashSet<String> = available_model_ids(cfg)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut out: Vec<RouterModel> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (id, source) in enabled_model_entries(cfg) {
        match at.get(&id) {
            Some(&i) => {
                if !out[i].sources.contains(&source) {
                    out[i].sources.push(source);
                }
            }
            None => {
                at.insert(id.clone(), out.len());
                out.push(RouterModel {
                    available: available.contains(&id),
                    id,
                    sources: vec![source],
                });
            }
        }
    }
    out
}

/// The Router page's model window: the live listings first (fetched when
/// stale, as a `/v1/models` request would), then the grouped list.
#[tauri::command]
pub async fn router_models(force: bool) -> Result<Vec<RouterModel>, String> {
    refresh_catalog(force).await;
    let cfg = current_config();
    tauri::async_runtime::spawn_blocking(move || router_models_list(&cfg))
        .await
        .map_err(|e| e.to_string())
}

/// The gateway's capabilities = the union of the ENABLED upstreams'
/// protocols (a tool can only bind a mode something in the pool serves),
/// plus the declared model ids for autocomplete.
fn router_capabilities(cfg: &RouterConfig) -> JsonValue {
    let enabled: std::collections::HashSet<&str> = cfg
        .upstreams
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.key.as_str())
        .collect();
    let mut protocols: std::collections::HashSet<Protocol> = std::collections::HashSet::new();
    for c in list_candidates(cfg) {
        if enabled.contains(c.key.as_str()) {
            protocols.extend(c.protocols.iter().copied());
        }
    }
    let models: Vec<String> = enabled_model_ids(cfg)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    // A client API is offered when an enabled member serves it natively OR
    // through a translator — that is what lets every tool bind the router.
    let served: Vec<Protocol> = protocols.into_iter().collect();
    json!({
        "anthropic": reachable(Protocol::Anthropic, &served),
        "openai": reachable(Protocol::OpenaiResponses, &served),
        "openaiCompatible": reachable(Protocol::OpenaiChat, &served),
        "gemini": reachable(Protocol::Gemini, &served),
        "models": models,
    })
}

/// Upsert the router's gateway entry from the current config: connection
/// fields, capabilities and name are Termory's; `bindings` and `favicon`
/// are preserved verbatim. Generates the client key first when none is
/// set, so the entry never holds an empty key. Returns the entry.
pub fn sync_router_gateway() -> Result<JsonValue, Box<dyn Error>> {
    let mut list = match crate::config::read_gateways()? {
        JsonValue::Array(a) => a,
        _ => Vec::new(),
    };
    let mut cfg = current_config();
    // A key is minted ONCE, when the router's entry is first created and
    // the router is stopped. Never afterwards: a key the user cleared stays
    // cleared, and a running router's key never changes under the CLIs
    // bound to it (connection settings change only while stopped).
    if cfg.api_key.trim().is_empty() && !list.iter().any(is_router_entry) && !is_running() {
        cfg = update_config(|c| {
            if c.api_key.trim().is_empty() {
                c.api_key = generate_api_key();
            }
            Ok(())
        })?;
    }
    let capabilities = router_capabilities(&cfg);
    let existing = list.iter_mut().find(|g| is_router_entry(g));
    let entry = match existing {
        Some(JsonValue::Object(o)) => {
            o.insert("kind".into(), JsonValue::from(crate::config::ROUTER_KIND));
            o.insert("name".into(), JsonValue::from(ROUTER_GATEWAY_NAME));
            o.insert(
                "baseUrl".into(),
                JsonValue::from(router_base_url(bind_ip(&cfg), cfg.port)),
            );
            o.insert("apiKey".into(), JsonValue::from(cfg.api_key.clone()));
            o.insert("capabilities".into(), capabilities);
            if !o.contains_key("bindings") {
                o.insert("bindings".into(), json!([]));
            }
            JsonValue::Object(o.clone())
        }
        _ => {
            let entry = json!({
                "kind": crate::config::ROUTER_KIND,
                "id": uuid_v4(),
                "name": ROUTER_GATEWAY_NAME,
                "baseUrl": router_base_url(bind_ip(&cfg), cfg.port),
                "apiKey": cfg.api_key,
                "capabilities": capabilities,
                "bindings": [],
            });
            entry
        }
    };
    // NOT `write_gateways`: that keeps the on-disk connection fields of this
    // entry against a stale frontend copy, which would drop this very update.
    crate::config::upsert_router_entry(&entry)?;
    Ok(entry)
}

/// After the port or key changed: re-apply every router binding that is the
/// CLI's LIVE provider, so the CLI's materialized config follows (CLAUDE.md:
/// editing a live provider re-applies it). Without this the CLI keeps
/// sending the OLD key to the OLD port with nothing on the page saying why.
/// The router's binding ids, from providers.json.
fn router_binding_ids() -> Result<std::collections::HashSet<String>, Box<dyn Error>> {
    let gateways = crate::config::read_gateways()?;
    Ok(gateways
        .as_array()
        .into_iter()
        .flatten()
        .filter(|g| is_router_entry(g))
        .flat_map(|g| {
            g.get("bindings")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|b| b.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect())
}

/// One router binding that is IN USE by its CLI right now.
struct LiveBinding {
    id: String,
    app: CliApp,
    /// Multi-slot (OpenCode / Grok) only: the slot is also the CLI's default.
    default: bool,
}

fn multi_slot(app: CliApp) -> bool {
    matches!(app, CliApp::Opencode | CliApp::Grok)
}

/// The router bindings in use, decided against the providers.json entry AS
/// IT IS NOW — so a caller about to change the port or key must ask BEFORE
/// `sync_router_gateway` writes the new values (after it, the live config,
/// still on the old port/key, no longer matches any synthesized binding).
///
/// - multi-slot: the slot is configured (enabled), default or not;
/// - single-slot: the reverse-derivation matches it, OR Termory's marker
///   names it while the live base URL is a loopback router URL — the case
///   of a CLI left on an OLD port/key (an unclean exit, a failed re-apply),
///   which the match alone can no longer see.
fn live_router_bindings() -> Result<Vec<LiveBinding>, Box<dyn Error>> {
    let ids = router_binding_ids()?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let synths = crate::providers::gateway_providers();
    let markers = crate::config::active_provider_markers();
    let mut apps: Vec<CliApp> = Vec::new();
    for p in synths.iter().filter(|p| ids.contains(&p.id)) {
        if !apps.contains(&p.app) {
            apps.push(p.app);
        }
    }
    let mut out = Vec::new();
    for app in apps {
        let for_app: Vec<_> = synths.iter().filter(|q| q.app == app).cloned().collect();
        let Ok(state) = crate::providers::read_active_state(app, &for_app) else {
            continue;
        };
        for p in for_app.iter().filter(|p| ids.contains(&p.id)) {
            let matched = state.matched_provider_id.as_deref() == Some(p.id.as_str());
            let in_use = if multi_slot(app) {
                matched || state.configured_provider_ids.contains(&p.id)
            } else {
                matched
                    || (markers.get(app.key()) == Some(&p.id)
                        && state
                            .live_snapshot
                            .as_ref()
                            .and_then(|s| s.base_url.as_deref())
                            .is_some_and(is_router_url))
            };
            if in_use {
                out.push(LiveBinding {
                    id: p.id.clone(),
                    app,
                    default: multi_slot(app) && matched,
                });
            }
        }
    }
    Ok(out)
}

/// Former ports remembered for `is_router_url`.
const FORMER_PORTS_KEPT: usize = 5;

/// Whether a live base URL points at THIS router: loopback or the host the
/// router is reached at, AND the router's port — the current one or one it
/// used before. Another local proxy (cc-switch, LiteLLM, Ollama …) on any
/// other port is the user's own setup and never counts.
fn is_router_url(url: &str) -> bool {
    is_router_url_for(url, &current_config())
}

fn is_router_url_for(url: &str, cfg: &RouterConfig) -> bool {
    let Some(rest) = url.trim().strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let own = client_host(bind_ip(cfg));
    (host == "127.0.0.1" || host == own) && (port == cfg.port || cfg.former_ports.contains(&port))
}

/// When a suspend moves Codex out of the router, what happens to the
/// sessions ("Keep all sessions on a Codex switch").
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CodexFollow {
    /// Re-tag now (Stop, the unclean-exit recovery at launch).
    Now,
    /// The app is quitting: re-tagging every rollout can take long, and an
    /// autostart would move them straight back. Record it instead
    /// (`pending_codex_follow`); the next launch settles it.
    Defer,
}

/// Stop's half of suspend/restore: deactivate every router binding that is
/// in use (single-slot → Official; multi-slot → the slot is removed), and
/// record which — plus which multi-slot one was the default — so `start()`
/// can restore them. The ids are recorded BEFORE anything is switched, so a
/// failing record never leaves CLIs switched with nothing to restore; ids
/// that then fail to switch are dropped again. Returns how many switched.
pub fn suspend_live_bindings(follow: CodexFollow) -> Result<usize, Box<dyn Error>> {
    let live = live_router_bindings()?;
    if live.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = live.iter().map(|b| b.id.clone()).collect();
    let defaults: Vec<String> = live
        .iter()
        .filter(|b| b.default)
        .map(|b| b.id.clone())
        .collect();
    if let Err(err) = update_config(|c| {
        c.suspended_bindings = merge_suspended(&c.suspended_bindings, ids.clone());
        c.suspended_defaults = merge_suspended(&c.suspended_defaults, defaults.clone());
        Ok(())
    }) {
        // Switching anyway: a CLI left on a closed port is the worse outcome.
        log::warn!("recording suspended router bindings failed (Start cannot restore them): {err}");
    }
    let synths = crate::providers::gateway_providers();
    let mut failed: Vec<String> = Vec::new();
    let mut codex_to_official = false;
    for b in &live {
        let Some(p) = synths.iter().find(|p| p.id == b.id) else {
            failed.push(b.id.clone());
            continue;
        };
        let for_app: Vec<_> = synths.iter().filter(|q| q.app == b.app).cloned().collect();
        let result = if multi_slot(b.app) {
            crate::providers::delete_provider_traces(p)
        } else {
            crate::providers::deactivate(b.app, &for_app)
        };
        if let Err(err) = result {
            log::warn!(
                "suspending the router binding for {} failed: {err}",
                b.app.key()
            );
            failed.push(b.id.clone());
            continue;
        }
        let _ = crate::config::set_active_provider_marker(b.app.key(), None);
        codex_to_official |= b.app == CliApp::Codex;
    }
    if !failed.is_empty() {
        let _ = update_config(|c| {
            c.suspended_bindings.retain(|id| !failed.contains(id));
            c.suspended_defaults.retain(|id| !failed.contains(id));
            Ok(())
        });
    }
    // Router → Official moves Codex into the `openai` bucket. With "Keep all
    // sessions on a Codex switch" on, sessions follow silently — the same
    // rule the tray applies; with it off there is no window to ask from, and
    // the sessions reappear when Start switches Codex back.
    if codex_to_official && crate::config::codex_keep_all_sessions() {
        match follow {
            CodexFollow::Now => crate::tray::codex_follow_all_blocking(true),
            CodexFollow::Defer => {
                let _ = update_config(|c| {
                    c.pending_codex_follow = true;
                    Ok(())
                });
            }
        }
    }
    Ok(live.len() - failed.len())
}

/// Adds newly suspended ids to those an earlier pass recorded, so a second
/// suspend before the next start (a quit after a Stop, a launch after a
/// crash) never forgets what the first one switched off. Order kept, no
/// duplicates.
fn merge_suspended(existing: &[String], new: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = existing.to_vec();
    for id in new {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// App quit: close the listener and suspend every binding in use, exactly as
/// Stop does, so no CLI is left pointed at a port nobody serves while
/// Termory is closed; the next `start()` restores them. Synchronous — the
/// process is about to end. Waits (bounded) for a start/stop in flight so
/// the two cannot interleave; Codex's session re-tag is DEFERRED to the
/// next launch rather than run here. A Termory that dies without a clean
/// quit is covered at the next launch by `start_if_configured`.
pub fn stop_for_exit() {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let _guard = loop {
        if let Ok(g) = lifecycle().try_lock() {
            break Some(g);
        }
        if std::time::Instant::now() >= deadline {
            log::warn!("router: quitting while a start/stop is still running");
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let Some(h) = server_slot().take() else {
        return;
    };
    let _ = h.shutdown.send(true);
    match suspend_live_bindings(CodexFollow::Defer) {
        Ok(n) if n > 0 => log::info!("router: suspended {n} binding(s) on quit"),
        Ok(_) => {}
        Err(err) => log::warn!("suspending router bindings on quit failed: {err}"),
    }
}

/// Start's half: put the bindings the last stop suspended back in use and
/// forget them — but only where the CLI is still where the suspend LEFT it
/// (Official for a single-slot CLI; for a multi-slot one, the slot absent).
/// A CLI the user moved to something else meanwhile keeps that choice. A
/// multi-slot slot is made default again only if it was, and no other
/// default was chosen meanwhile. Returns how many were restored.
pub fn restore_suspended_bindings() -> Result<usize, Box<dyn Error>> {
    let mut taken = (Vec::new(), Vec::new(), false);
    update_config(|c| {
        taken = (
            std::mem::take(&mut c.suspended_bindings),
            std::mem::take(&mut c.suspended_defaults),
            c.pending_codex_follow,
        );
        Ok(())
    })?;
    let (ids, defaults, pending_follow) = taken;
    if ids.is_empty() {
        return Ok(0);
    }
    let synths = crate::providers::gateway_providers();
    let mut n = 0;
    // Ids that could not be restored for a passing reason (the CLI's config
    // unreadable — torn mid-write —, an activation that failed): kept for
    // the next Start instead of being forgotten. Ids dropped on purpose (the
    // binding is gone, the user moved the CLI elsewhere) are not.
    let mut retry: Vec<String> = Vec::new();
    for id in &ids {
        let Some(p) = synths.iter().find(|p| &p.id == id) else {
            continue;
        };
        let for_app: Vec<_> = synths.iter().filter(|q| q.app == p.app).cloned().collect();
        let Ok(state) = crate::providers::read_active_state(p.app, &for_app) else {
            retry.push(id.clone());
            continue;
        };
        if multi_slot(p.app) {
            if state.configured_provider_ids.contains(&p.id) {
                continue;
            }
        } else if crate::providers::read_active_state(p.app, &[])
            .map(|s| s.kind != crate::providers::ActiveKind::Official)
            .unwrap_or(true)
        {
            log::info!(
                "router: {} was switched elsewhere while stopped; not restoring its binding",
                p.app.key()
            );
            continue;
        }
        if let Err(err) = crate::providers::activate(p, &for_app) {
            log::warn!(
                "restoring the router binding for {} failed: {err}",
                p.app.key()
            );
            retry.push(id.clone());
            continue;
        }
        if multi_slot(p.app) {
            if defaults.contains(&p.id) && state.matched_provider_id.is_none() {
                if let Err(err) = crate::providers::set_default(p, &for_app) {
                    log::warn!(
                        "restoring the router default for {} failed: {err}",
                        p.app.key()
                    );
                }
            }
        } else {
            let _ = crate::config::set_active_provider_marker(p.app.key(), Some(&p.id));
        }
        // Official → router: the sessions follow into `termory` when the
        // user chose "Keep all sessions on a Codex switch" — unless a quit
        // DEFERRED their move to `openai`, in which case they never left.
        if p.app == CliApp::Codex {
            if pending_follow {
                let _ = update_config(|c| {
                    c.pending_codex_follow = false;
                    Ok(())
                });
            } else if crate::config::codex_keep_all_sessions() {
                crate::tray::codex_follow_all_blocking(false);
            }
        }
        n += 1;
    }
    if !retry.is_empty() {
        let retry_defaults: Vec<String> = defaults
            .iter()
            .filter(|d| retry.contains(d))
            .cloned()
            .collect();
        update_config(|c| {
            c.suspended_bindings = merge_suspended(&c.suspended_bindings, retry.clone());
            c.suspended_defaults = merge_suspended(&c.suspended_defaults, retry_defaults.clone());
            Ok(())
        })?;
    }
    Ok(n)
}

/// A quit deferred Codex's session re-tag (`CodexFollow::Defer`). When the
/// launch did NOT put Codex back on the router, finish it now: Codex is on
/// Official, so the sessions follow to `openai`.
fn settle_pending_codex_follow() {
    if !current_config().pending_codex_follow {
        return;
    }
    let official = crate::providers::read_active_state(CliApp::Codex, &[])
        .is_ok_and(|s| s.kind == crate::providers::ActiveKind::Official);
    if official && crate::config::codex_keep_all_sessions() {
        crate::tray::codex_follow_all_blocking(true);
    }
    let _ = update_config(|c| {
        c.pending_codex_follow = false;
        Ok(())
    });
}

/// Re-apply the given in-use bindings from the entry as it is NOW (after a
/// port/key sync), so each CLI follows. `live` must be captured BEFORE the
/// sync — see `live_router_bindings`.
fn reapply_bindings(live: &[LiveBinding]) -> Result<(), Box<dyn Error>> {
    if live.is_empty() {
        return Ok(());
    }
    let synths = crate::providers::gateway_providers();
    for b in live {
        let Some(p) = synths.iter().find(|p| p.id == b.id) else {
            continue;
        };
        let for_app: Vec<_> = synths.iter().filter(|q| q.app == b.app).cloned().collect();
        if let Err(err) = crate::providers::activate(p, &for_app) {
            log::warn!(
                "re-applying the router binding for {} failed: {err}",
                b.app.key()
            );
            continue;
        }
        if b.default {
            if let Err(err) = crate::providers::set_default(p, &for_app) {
                log::warn!(
                    "re-setting the router default for {} failed: {err}",
                    b.app.key()
                );
            }
        }
    }
    Ok(())
}

// ===================================================================
// Codex emulation for the ChatGPT backend
// ===================================================================
//
// When the upstream is a ChatGPT login, the request is sent the way Codex
// itself sends it to `chatgpt.com/backend-api/codex` — a client that is NOT
// Codex (or Codex talking to a custom provider, which drops a few things)
// still reaches the backend with the headers and body shape it expects.
// Sources: `core/src/client.rs` (`build_routing_hint_header`, request
// fields), `login/src/auth/default_client.rs` (`get_codex_user_agent`),
// `codex-api/src/endpoint/responses.rs` (`Accept: text/event-stream`),
// `core/src/installation_id.rs`.
//
// What a client already sends is kept; only MISSING pieces are filled in,
// with one exception: a non-Codex `User-Agent` is replaced, because the
// backend reads it as the product talking to it.

/// Fold a Responses SSE stream into the single JSON document a non-streaming
/// client expects: the `response.completed` event's `response`; a
/// `response.failed` / `response.incomplete` event's `response` is returned
/// as-is (it carries the error), and an `error` event becomes a 502.
pub fn fold_responses_sse(text: &str) -> (StatusCode, JsonValue) {
    // With `store: false` the backend's terminal event carries an EMPTY
    // `output` (measured live 2026-09-29); the items arrive one by one as
    // `response.output_item.done`, which is what Codex's own reader collects
    // (`codex-api/src/sse/responses.rs`) and what cc-switch folds in
    // (`proxy/handlers.rs`). They replace `output` when any arrived.
    let mut items: Vec<JsonValue> = Vec::new();
    let with_items = |mut r: JsonValue, items: &Vec<JsonValue>| {
        if !items.is_empty() {
            if let Some(obj) = r.as_object_mut() {
                obj.insert("output".into(), JsonValue::Array(items.clone()));
            }
        }
        r
    };
    let text = text.replace("\r\n", "\n");
    let mut last: Option<JsonValue> = None;
    for event in text.split("\n\n") {
        let data: String = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<JsonValue>(&data) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response.output_item.done") => {
                if let Some(item) = v.get("item") {
                    items.push(item.clone());
                }
            }
            Some("response.completed") => {
                if let Some(r) = v.get("response") {
                    return (StatusCode::OK, with_items(r.clone(), &items));
                }
            }
            Some("response.failed") | Some("response.incomplete") => {
                if let Some(r) = v.get("response") {
                    last = Some(r.clone());
                }
            }
            Some("error") => {
                return (
                    StatusCode::BAD_GATEWAY,
                    json!({ "error": v.get("error").cloned().unwrap_or(v.clone()) }),
                );
            }
            _ => {}
        }
    }
    match last {
        Some(r) => (StatusCode::OK, with_items(r, &items)),
        None => (
            StatusCode::BAD_GATEWAY,
            json!({ "error": { "type": "upstream_stream", "message": "the stream ended without a completed response" } }),
        ),
    }
}

// ===================================================================
// Binding models
// ===================================================================

/// Live `/models` listings per ENABLED provider key, as the vendor reports
/// them — the FULL set a provider supports. Refreshed by `refresh_catalog`
/// (page open, upstream toggles, `/v1/models`), cached a few minutes so
/// expanding a binding row does not re-query every provider. Gateways are
/// not fetched here: their detected catalog already lives in
/// `capabilities.models`.
static CATALOG: std::sync::LazyLock<Mutex<HashMap<String, (std::time::Instant, Vec<String>)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
const CATALOG_TTL: Duration = Duration::from_secs(300);

fn catalog_is_fresh(key: &str) -> bool {
    let listed = CATALOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .is_some_and(|(at, _)| at.elapsed() < CATALOG_TTL);
    // A listing that FAILED is not re-fetched for a minute either, so one
    // unreachable member cannot add its timeout to every request.
    listed
        || catalog_tried()
            .get(key)
            .is_some_and(|at| at.elapsed() < CATALOG_RETRY_FAILED)
}

/// The catalog step on a REQUEST's path. Only a listing never fetched yet
/// (right after start) is waited for — routing needs it. A listing that is
/// merely past its TTL is used as it is and re-fetched in the background:
/// waiting on it held one request every few minutes for as long as the
/// slowest member took to answer its `/models`.
async fn catalog_for_request() {
    static IN_BACKGROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let cfg = current_config();
    let enabled = || cfg.upstreams.iter().filter(|p| p.enabled);
    if enabled().any(|p| catalog_never_fetched(&p.key)) {
        refresh_catalog(false).await;
        return;
    }
    if enabled().any(|p| !catalog_is_fresh(&p.key))
        && !IN_BACKGROUND.swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        tokio::spawn(async {
            refresh_catalog(false).await;
            IN_BACKGROUND.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }
}

/// No listing was ever attempted for this member.
fn catalog_never_fetched(key: &str) -> bool {
    !CATALOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(key)
        && !catalog_tried().contains_key(key)
}

/// When each member's listing was last attempted (success or failure).
fn catalog_tried() -> std::sync::MutexGuard<'static, HashMap<String, std::time::Instant>> {
    static TRIED: std::sync::LazyLock<Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    TRIED.lock().unwrap_or_else(|e| e.into_inner())
}

const CATALOG_RETRY_FAILED: Duration = Duration::from_secs(60);

/// Store a listing; returns whether the model SET changed (not just its age).
fn catalog_store(key: &str, models: Vec<String>) -> bool {
    let mut catalog = CATALOG.lock().unwrap_or_else(|e| e.into_inner());
    let changed = catalog.get(key).is_none_or(|(_, old)| *old != models);
    catalog.insert(key.to_string(), (std::time::Instant::now(), models));
    changed
}

/// Fetch (or re-use, unless `force`) every enabled upstream's model listing:
/// a provider's own `/models`, and for the OAuth logins the vendor's listing
/// queried with that login — Anthropic's `/v1/models` under the OAuth beta
/// flag, and the ChatGPT Codex backend's `/models` (picker-visible entries
/// only, as Codex's own picker shows them) — and a gateway's own live
/// listing, which REPLACES the list its entry saved at detection time.
pub async fn refresh_catalog(force: bool) {
    let cfg = current_config();
    let enabled: Vec<String> = cfg
        .upstreams
        .iter()
        .filter(|p| p.enabled)
        .filter(|p| force || !catalog_is_fresh(&p.key))
        .map(|p| p.key.clone())
        .collect();
    if enabled.is_empty() {
        return;
    }
    let keys = enabled.clone();
    // Resolution reads credential stores (blocking) — off the async worker.
    let resolved: Vec<(
        String,
        Result<(Upstream, Option<crate::providers::Provider>), String>,
    )> = tokio::task::spawn_blocking(move || {
        let stores = Stores::load();
        keys.into_iter()
            .map(|key| {
                let provider = key
                    .strip_prefix("provider:")
                    .and_then(|id| stores.providers.iter().find(|p| p.id == id).cloned());
                let protocol = match provider.as_ref() {
                    Some(p) => provider_wire_protocol(p),
                    None if key.starts_with("live:codex") || key.starts_with("account:codex:") => {
                        Protocol::OpenaiResponses
                    }
                    None if key.starts_with("live:grok") || key.starts_with("account:grok:") => {
                        Protocol::OpenaiChat
                    }
                    None => return (key, Err("no listing".to_string())),
                };
                let r = resolve_upstream_with(&stores, &key, protocol).map(|u| (u, provider));
                (key, r)
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    let codex_version = tokio::task::spawn_blocking(|| {
        crate::providers::detect_cli_version(CliApp::Codex).unwrap_or_else(|| "0.0.0".to_string())
    })
    .await
    .unwrap_or_else(|_| "0.0.0".to_string());
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let fetches = resolved.into_iter().filter_map(|(key, r)| {
        let (upstream, provider) = r.ok()?;
        let client = client.clone();
        let codex_version = codex_version.clone();
        Some(async move {
            let models = match provider {
                Some(p) => {
                    let r = crate::providers::fetch_models(&p).await;
                    r.ok.then_some(r.models)
                }
                None => {
                    // A listing is READ-ONLY and never touches the credential
                    // (CLAUDE.md): no refresh here — that spends a rotating
                    // token, possibly while the router is stopped or while a
                    // login flow owns the credential. An access token already
                    // expired is simply not listed now; while the router
                    // runs, the background refresh keeps it fresh.
                    if token_expired(upstream.expires_at) {
                        return (key, None);
                    }
                    fetch_oauth_models(&client, &upstream, &codex_version).await
                }
            };
            (key, models)
        })
    });
    // Gateways: their LIVE listing, not the one saved at detection time.
    let gateway_targets: Vec<(String, String, String)> = {
        let keys: Vec<String> = enabled
            .iter()
            .filter(|k| k.starts_with("gateway:"))
            .cloned()
            .collect();
        if keys.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || {
                let gws = match crate::config::read_gateways() {
                    Ok(JsonValue::Array(a)) => a,
                    _ => Vec::new(),
                };
                keys.into_iter()
                    .filter_map(|k| {
                        let id = k.strip_prefix("gateway:")?;
                        let g = gws
                            .iter()
                            .find(|g| g.get("id").and_then(|v| v.as_str()) == Some(id))?;
                        let base = g.get("baseUrl").and_then(|v| v.as_str())?.to_string();
                        let key = g
                            .get("apiKey")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some((k, base, key))
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        }
    };
    let gateway_fetches = gateway_targets
        .into_iter()
        .map(|(key, base, api_key)| async move {
            let models = crate::providers::list_gateway_models(&base, &api_key).await;
            (key, models)
        });
    let (mut results, gateway_results) = futures_util::future::join(
        futures_util::future::join_all(fetches),
        futures_util::future::join_all(gateway_fetches),
    )
    .await;
    results.extend(gateway_results);
    let mut changed = false;
    {
        let mut tried = catalog_tried();
        for (key, _) in &results {
            tried.insert(key.clone(), std::time::Instant::now());
        }
    }
    for (key, models) in results {
        // A failed listing keeps whatever was cached.
        if let Some(models) = models {
            changed |= catalog_store(&key, models);
        }
    }
    // The router's gateway entry carries the model list the binding rows and
    // the Providers page offer; it was synced before these listings landed,
    // so re-sync it when one of them actually changed.
    if changed {
        let _ = tokio::task::spawn_blocking(|| {
            if let Err(err) = sync_router_gateway() {
                log::warn!("router gateway sync after a catalog change failed: {err}");
            }
        })
        .await;
    }
}

/// How often the running router checks its logins. Well inside the refresh
/// margin, so a token is renewed before any request could find it expired.
const BACKGROUND_REFRESH_EVERY: Duration = Duration::from_secs(60);

/// Pool keys whose credential the router can refresh (Codex / Grok logins).
pub fn is_refreshable_login_key(key: &str) -> bool {
    ["live:codex", "live:grok"].contains(&key)
        || key.starts_with("account:codex:")
        || key.starts_with("account:grok:")
}

/// One background pass: every ENABLED refreshable login whose access token
/// is within the margin is refreshed through the same path a request takes
/// (single-flight gate, login-in-progress gate, write-back rules). Disabled
/// members are left alone — a refresh spends a rotating token, which is not
/// worth doing for a login the router will not use.
async fn background_refresh_pass(ctx: &Ctx) {
    let cfg = current_config();
    let keys: Vec<String> = cfg
        .upstreams
        .iter()
        .filter(|p| p.enabled && is_refreshable_login_key(&p.key))
        .map(|p| p.key.clone())
        .collect();
    if keys.is_empty() {
        return;
    }
    let resolved = tokio::task::spawn_blocking(move || {
        let stores = Stores::load();
        keys.into_iter()
            .map(|k| {
                let r = resolve_upstream_with(&stores, &k, protocol_for_listing(&k));
                (k, r)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let margin = crate::accounts::router_refresh_margin_secs();
    for (key, r) in resolved {
        let Ok(upstream) = r else { continue };
        let Some(login) = upstream.login.clone() else {
            continue;
        };
        let due = upstream
            .expires_at
            .is_some_and(|e| e <= now_secs() as i64 + margin);
        if !due {
            continue;
        }
        match refresh_and_resolve(ctx, &key, &login, protocol_for_listing(&key), None).await {
            Ok(_) => {
                log::info!("router: background refresh kept {} fresh", public_key(&key));
                ctx.emit_changed();
            }
            Err(why) => {
                record_skip(&key, why);
                ctx.emit_changed();
            }
        }
    }
}

/// The protocol a login key is listed under (its first served one).
fn protocol_for_listing(key: &str) -> Protocol {
    if key.contains("grok") {
        Protocol::OpenaiChat
    } else {
        Protocol::OpenaiResponses
    }
}

/// The ChatGPT Codex backend's `/models` is what Codex's own picker
/// reads (`codex-api/src/endpoint/models.rs` `ModelsClient`), keyed by the
/// installed CLI's version; only `visibility == "list"` entries are offered,
/// the picker's own rule.
async fn fetch_oauth_models(
    client: &reqwest::Client,
    upstream: &Upstream,
    codex_version: &str,
) -> Option<Vec<String>> {
    match &upstream.auth {
        Auth::ChatGpt { token, account_id } => {
            let mut req = client
                .get(format!(
                    "{}/models?client_version={codex_version}",
                    upstream.base_url.trim_end_matches('/')
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("originator", "codex_cli_rs")
                .header("user-agent", format!("codex_cli_rs/{codex_version}"));
            if let Some(id) = account_id {
                req = req.header("chatgpt-account-id", id.as_str());
            }
            let resp = req.send().await.ok()?;
            if !resp.status().is_success() {
                log::warn!(
                    "router: model listing for a Codex login answered {}",
                    resp.status().as_u16()
                );
                return None;
            }
            let body: JsonValue = resp.json().await.ok()?;
            Some(
                body.get("models")?
                    .as_array()?
                    .iter()
                    .filter(|m| m.get("visibility").and_then(|v| v.as_str()) == Some("list"))
                    .filter_map(|m| m.get("slug").and_then(|v| v.as_str()).map(str::to_string))
                    .collect(),
            )
        }
        Auth::GrokOauth {
            token,
            user_id,
            email,
        } => {
            // The shell's own catalog request (`GET /v1/models`, OpenAI shape;
            // `xai-grok-pager-pty-harness/src/content.rs`).
            let resp = grok_headers(
                client.get(format!(
                    "{}/models",
                    upstream.base_url.trim_end_matches('/')
                )),
                token,
                user_id,
                email.as_deref(),
            )
            .send()
            .await
            .ok()?;
            if !resp.status().is_success() {
                log::warn!(
                    "router: model listing for a Grok login answered {}",
                    resp.status().as_u16()
                );
                return None;
            }
            let body: JsonValue = resp.json().await.ok()?;
            let list = body
                .get("data")
                .or_else(|| body.get("models"))
                .and_then(|v| v.as_array())?;
            Some(
                list.iter()
                    .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
                    .collect(),
            )
        }
        Auth::ApiKey(_) => None,
    }
}

/// The model ids a binding for `app` can pick from: the full supported set
/// of every ENABLED login, provider and gateway that serves this tool's
/// API — each from its LIVE listing when one was fetched (a gateway falls
/// back to the list saved at detection time until then).
/// Whether a client speaking `client` can use a member serving `served`:
/// natively, or through a translator (`translate::targets_for`).
fn reachable(client: Protocol, served: &[Protocol]) -> bool {
    served.contains(&client)
        || crate::translate::targets_for(client)
            .iter()
            .any(|t| served.contains(t))
}

pub async fn binding_models(app: CliApp, force: bool) -> Vec<String> {
    refresh_catalog(force).await;
    let protocol = provider_protocol(app);
    let cfg = current_config();
    let stores = tokio::task::spawn_blocking(Stores::load)
        .await
        .unwrap_or_else(|_| Stores {
            accounts: Vec::new(),
            providers: Vec::new(),
            gateways: Vec::new(),
            disabled: std::collections::HashSet::new(),
        });
    let catalog = CATALOG.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<String> = Vec::new();
    for pref in cfg.upstreams.iter().filter(|p| p.enabled) {
        let key = pref.key.as_str();
        let login_protocols: Option<Vec<Protocol>> =
            if key.starts_with("live:codex") || key.starts_with("account:codex:") {
                Some(vec![Protocol::OpenaiResponses])
            } else if key.starts_with("live:grok") || key.starts_with("account:grok:") {
                Some(GROK_PROTOCOLS.to_vec())
            } else {
                None
            };
        if let Some(lp) = login_protocols {
            if reachable(protocol, &lp) {
                if let Some((_, live)) = catalog.get(key) {
                    out.extend(live.iter().cloned());
                }
            }
            continue;
        }
        if let Some(id) = key.strip_prefix("provider:") {
            if let Some(p) = stores.providers.iter().find(|p| p.id == id) {
                if !reachable(protocol, &[provider_wire_protocol(p)]) {
                    continue;
                }
                if !p.model.trim().is_empty() {
                    out.push(p.model.trim().to_string());
                }
                out.extend(p.models.iter().map(|m| m.id.clone()));
                if let Some((_, live)) = catalog.get(key) {
                    out.extend(live.iter().cloned());
                }
            }
        } else if let Some(id) = key.strip_prefix("gateway:") {
            if let Some(g) = stores
                .gateways
                .iter()
                .find(|g| g.get("id").and_then(|v| v.as_str()) == Some(id))
            {
                if !reachable(protocol, &gateway_protocols(g.get("capabilities"))) {
                    continue;
                }
                match catalog.get(key) {
                    Some((_, live)) => out.extend(live.iter().cloned()),
                    None => out.extend(detected_gateway_models(g)),
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| !m.trim().is_empty() && seen.insert(m.clone()));
    out
}

#[tauri::command]
pub async fn router_binding_models(
    app: CliApp,
    #[allow(unused_variables)] force: Option<bool>,
) -> Result<Vec<String>, String> {
    Ok(binding_models(app, force.unwrap_or(false)).await)
}

// ===================================================================
// IPC
// ===================================================================

/// Config as the page sees it — the key masked.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterConfigView {
    pub autostart: bool,
    pub host: String,
    pub port: u16,
    pub has_api_key: bool,
    pub api_key_masked: String,
    pub strategy: Strategy,
    pub upstreams: Vec<UpstreamPref>,
}

impl From<RouterConfig> for RouterConfigView {
    fn from(c: RouterConfig) -> Self {
        RouterConfigView {
            autostart: c.autostart,
            host: c.host.clone(),
            port: c.port,
            // Masked like every other read path (security boundary). The raw
            // key leaves the backend only through `router_reveal_key`, on the
            // user's explicit copy click.
            api_key_masked: crate::providers::mask_secret(&c.api_key),
            has_api_key: !c.api_key.is_empty(),
            strategy: c.strategy,
            upstreams: c.upstreams,
        }
    }
}

/// What the page needs in one round trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterPageState {
    pub config: RouterConfigView,
    pub candidates: Vec<Candidate>,
    pub status: RouterStatus,
    /// What the listen-address picker offers (`listen_addresses`).
    pub listen_addresses: Vec<ListenAddress>,
}

/// One choice in the listen-address picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenAddress {
    pub address: String,
    /// The interface it belongs to (`en0`, `Ethernet`); `None` for the two
    /// fixed choices and for a saved address no interface carries any more.
    pub interface: Option<String>,
}

/// The listen-address choices: loopback, every interface, then each IPv4
/// address this machine's interfaces carry (loopback and link-local 169.254
/// left out — neither is reachable from the LAN). The SAVED host is always
/// offered, so a picker whose interface went away still shows the value in
/// effect. IPv6 is left out: LAN clients use the IPv4 address.
pub fn listen_addresses(current: &str) -> Vec<ListenAddress> {
    let mut ifaces: Vec<(String, std::net::Ipv4Addr)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                Some((i.name, v4))
            }
            _ => None,
        })
        .collect();
    ifaces.sort();
    listen_address_list(current, ifaces)
}

fn listen_address_list(
    current: &str,
    ifaces: Vec<(String, std::net::Ipv4Addr)>,
) -> Vec<ListenAddress> {
    let mut out = vec![
        ListenAddress {
            address: DEFAULT_HOST.to_string(),
            interface: None,
        },
        ListenAddress {
            address: "0.0.0.0".to_string(),
            interface: None,
        },
    ];
    for (name, ip) in ifaces {
        let address = ip.to_string();
        if !out.iter().any(|a| a.address == address) {
            out.push(ListenAddress {
                address,
                interface: Some(name),
            });
        }
    }
    let current = current.trim();
    if !current.is_empty() && !out.iter().any(|a| a.address == current) {
        out.push(ListenAddress {
            address: current.to_string(),
            interface: None,
        });
    }
    out
}

/// Also re-syncs the router's gateway entry in providers.json, so the page
/// can re-read the gateways list right after and find it current. The
/// entry itself is NOT returned here: the frontend already holds every
/// gateway (keys included) through the ordinary gateways read, and a
/// second copy would drift from it.
pub fn page_state() -> RouterPageState {
    if let Err(err) = sync_router_gateway() {
        log::warn!("router gateway sync failed: {err}");
    }
    let cfg = current_config();
    let candidates = list_candidates(&cfg);
    let listen_addresses = listen_addresses(&cfg.host);
    RouterPageState {
        config: cfg.into(),
        candidates,
        status: status(),
        listen_addresses,
    }
}

/// Incoming config edits; every field optional so the page sends only what
/// changed (same per-key spirit as `write_app_config`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterConfigPatch {
    pub autostart: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub strategy: Option<Strategy>,
    pub upstreams: Option<Vec<UpstreamPref>>,
}

pub fn apply_patch(
    mut cfg: RouterConfig,
    patch: RouterConfigPatch,
) -> Result<RouterConfig, String> {
    if let Some(a) = patch.autostart {
        cfg.autostart = a;
    }
    if let Some(h) = patch.host {
        let h = h.trim();
        h.parse::<std::net::IpAddr>()
            .map_err(|_| format!("\"{h}\" is not an IP address"))?;
        cfg.host = h.to_string();
    }
    if let Some(p) = patch.port {
        if p == 0 {
            return Err("port must be between 1 and 65535".to_string());
        }
        if p != cfg.port {
            let old = cfg.port;
            cfg.former_ports.retain(|x| *x != old && *x != p);
            cfg.former_ports.insert(0, old);
            cfg.former_ports.truncate(FORMER_PORTS_KEPT);
        }
        cfg.port = p;
    }
    if let Some(k) = patch.api_key {
        cfg.api_key = k.trim().to_string();
    }
    if let Some(s) = patch.strategy {
        cfg.strategy = s;
    }
    if let Some(u) = patch.upstreams {
        let mut seen = std::collections::HashSet::new();
        cfg.upstreams = u
            .into_iter()
            .filter(|p| !p.key.is_empty() && seen.insert(p.key.clone()))
            .collect();
    }
    // Checked on the RESULT, so clearing the key while exposed is refused
    // as well as exposing without one.
    validate_exposure(&cfg)?;
    Ok(cfg)
}

/// The refusal for a connection-setting change while the router runs.
const STOP_TO_EDIT: &str = "Stop the router before changing its listen address, port or API key.";

/// A connection-setting change (`changed`) is refused while the listener is up.
fn refuse_while_running(changed: bool) -> Result<(), String> {
    if changed && is_running() {
        return Err(STOP_TO_EDIT.to_string());
    }
    Ok(())
}

/// Autostart hook for `setup()`.
pub fn start_if_configured(app: tauri::AppHandle) {
    if current_config().autostart {
        tauri::async_runtime::spawn(async move {
            if let Err(err) = start(app).await {
                log::warn!("router autostart failed: {err}");
            }
        });
        return;
    }
    // Not coming back up: a binding still in use can only be left over from
    // a Termory that ended without a clean quit (killed, crashed, power
    // loss), and it points its CLI at a closed port. Suspend it now, as the
    // quit would have; Start restores it. Then finish a Codex session
    // re-tag the last quit deferred.
    tauri::async_runtime::spawn(async move {
        let _g = lifecycle().lock().await;
        let _ = tauri::async_runtime::spawn_blocking(move || {
            match suspend_live_bindings(CodexFollow::Now) {
                Ok(n) if n > 0 => {
                    log::info!("router: suspended {n} binding(s) left in use by an unclean exit");
                    notify_bindings_changed(&app);
                }
                Ok(_) => {}
                Err(err) => log::warn!("suspending leftover router bindings failed: {err}"),
            }
            settle_pending_codex_follow();
        })
        .await;
    });
}

#[tauri::command]
pub async fn router_page_state() -> Result<RouterPageState, String> {
    // The page must render from local state at once; the catalog refresh
    // is NETWORK (one `/models` request per enabled provider, 10 s timeout
    // each), so it runs in the background. The binding rows fetch their
    // model lists through `router_binding_models`, which waits on it, and
    // the gateway entry's `capabilities.models` catches up on the next sync.
    tauri::async_runtime::spawn(refresh_catalog(false));
    tauri::async_runtime::spawn_blocking(page_state)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn router_write_config(
    app: tauri::AppHandle,
    patch: RouterConfigPatch,
) -> Result<RouterPageState, String> {
    // The listen ADDRESS (host or port): a change can move the URL the CLIs
    // use. Like a key change it is refused while running (below).
    let port_changed = {
        let cur = current_config();
        patch.port.is_some_and(|p| p != cur.port)
            || patch
                .host
                .as_deref()
                .is_some_and(|h| h.trim().parse::<std::net::IpAddr>().ok() != Some(bind_ip(&cur)))
    };
    let creds_changed = {
        let cur = current_config();
        port_changed
            || patch
                .api_key
                .as_deref()
                .is_some_and(|k| k.trim() != cur.api_key)
    };
    let upstreams_changed = patch.upstreams.is_some();
    let _g = lifecycle().lock().await;
    // The connection settings (listen address, port, key) change only while
    // STOPPED: a running listener is never re-bound (an overlapping bind on
    // the same port is refused on Linux) and a running CLI session is never
    // left holding an address or key that just stopped working. Checked
    // under the lock, so a start racing this patch cannot slip between.
    refuse_while_running(creds_changed)?;
    // Which bindings are in use must be asked BEFORE the sync writes the new
    // port/key into the entry — afterwards nothing matches the live config.
    let live = if creds_changed {
        tauri::async_runtime::spawn_blocking(|| live_router_bindings().map_err(|e| e.to_string()))
            .await
            .map_err(|e| e.to_string())??
    } else {
        Vec::new()
    };
    tauri::async_runtime::spawn_blocking(move || {
        update_config(|c| {
            *c = apply_patch(c.clone(), patch)?;
            Ok(())
        })
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    // A newly enabled provider's listing has to be in before the gateway
    // entry's model catalog is synced from it.
    if upstreams_changed {
        refresh_catalog(false).await;
    }
    tauri::async_runtime::spawn_blocking(move || {
        // Every change re-syncs the gateway entry (the upstream set decides
        // its capabilities); only a connection change re-applies live bindings.
        sync_router_gateway().map_err(|e| e.to_string())?;
        reapply_bindings(&live).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = app.emit("termory:providers-changed", ());
    let _ = app.emit(ROUTER_CHANGED_EVENT, ());
    tauri::async_runtime::spawn_blocking(page_state)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn router_start(app: tauri::AppHandle) -> Result<RouterStatus, String> {
    start(app).await
}

#[tauri::command]
pub async fn router_stop(app: tauri::AppHandle) -> Result<RouterStatus, String> {
    stop(&app).await;
    Ok(status())
}

#[tauri::command]
pub async fn router_status() -> Result<RouterStatus, String> {
    Ok(status())
}

#[tauri::command]
pub async fn router_reset_upstream(app: tauri::AppHandle, key: String) -> Result<(), String> {
    reset_health(&key);
    let _ = app.emit(ROUTER_CHANGED_EVENT, ());
    Ok(())
}

/// The raw key, on demand only — for the page's copy button and for a
/// third-party tool the user configures by hand.
#[tauri::command]
pub async fn router_reveal_key() -> Result<String, String> {
    Ok(current_config().api_key)
}

#[tauri::command]
pub async fn router_generate_key(app: tauri::AppHandle) -> Result<RouterPageState, String> {
    let _g = lifecycle().lock().await;
    refuse_while_running(true)?;
    tauri::async_runtime::spawn_blocking(move || {
        // In use BEFORE the new key reaches the entry (see
        // `live_router_bindings`), then re-pointed with it.
        let live = live_router_bindings().map_err(|e| e.to_string())?;
        update_config(|c| {
            c.api_key = generate_api_key();
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        sync_router_gateway().map_err(|e| e.to_string())?;
        reapply_bindings(&live).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = app.emit("termory:providers-changed", ());
    let _ = app.emit(ROUTER_CHANGED_EVENT, ());
    tauri::async_runtime::spawn_blocking(page_state)
        .await
        .map_err(|e| e.to_string())
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{lock_home, override_home};

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termory-router-{tag}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cand(key: &str, protocols: &[Protocol], enabled: bool) -> Candidate {
        Candidate {
            key: key.to_string(),
            kind: CandidateKind::Provider,
            app: None,
            label: key.to_string(),
            detail: String::new(),
            protocols: protocols.to_vec(),
            available: true,
            reason: None,
            reason_code: None,
            enabled,
        }
    }

    #[test]
    fn classify_path_routes_each_api() {
        assert_eq!(classify_path("/v1/messages"), Some(Protocol::Anthropic));
        assert_eq!(
            classify_path("/v1/messages/count_tokens"),
            Some(Protocol::Anthropic)
        );
        assert_eq!(
            classify_path("/v1/responses"),
            Some(Protocol::OpenaiResponses)
        );
        assert_eq!(
            classify_path("/v1/chat/completions"),
            Some(Protocol::OpenaiChat)
        );
        assert_eq!(classify_path("/v1/embeddings"), Some(Protocol::OpenaiChat));
        assert_eq!(
            classify_path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"),
            Some(Protocol::Gemini)
        );
        assert_eq!(
            classify_path("/v1/models/gemini-2.5-pro:generateContent"),
            Some(Protocol::Gemini)
        );
        assert_eq!(classify_path("/v1/models"), None);
        assert_eq!(classify_path("/"), None);
    }

    #[test]
    fn upstream_url_follows_each_cli_convention() {
        // Anthropic: bare root + full path (Claude Code appends /v1/messages).
        assert_eq!(
            upstream_url(
                "https://api.anthropic.com",
                Protocol::Anthropic,
                "/v1/messages"
            ),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            upstream_url(
                "https://api.deepseek.com/anthropic/",
                Protocol::Anthropic,
                "/v1/messages?beta=true"
            ),
            "https://api.deepseek.com/anthropic/v1/messages?beta=true"
        );
        // OpenAI flavours: the stored base already ends in /v1.
        assert_eq!(
            upstream_url(
                "https://api.openai.com/v1",
                Protocol::OpenaiResponses,
                "/v1/responses"
            ),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            upstream_url(
                "https://relay.example.com/openai",
                Protocol::OpenaiChat,
                "/v1/chat/completions"
            ),
            "https://relay.example.com/openai/chat/completions"
        );
        // ChatGPT's Codex backend has no /v1 at all.
        assert_eq!(
            upstream_url(
                CHATGPT_CODEX_BASE_URL,
                Protocol::OpenaiResponses,
                "/v1/responses"
            ),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // Gemini: bare root, the client's /v1beta path rides along.
        assert_eq!(
            upstream_url(
                "https://gw.example.com/v1beta",
                Protocol::Gemini,
                "/v1beta/models/gemini-2.5-pro:generateContent?alt=sse"
            ),
            "https://gw.example.com/v1beta/models/gemini-2.5-pro:generateContent?alt=sse"
        );
    }

    #[test]
    fn gateway_base_applies_anthropic_subpath_only_to_anthropic() {
        assert_eq!(
            gateway_base(
                "https://api.moonshot.cn/v1",
                Protocol::Anthropic,
                Some("/anthropic")
            ),
            "https://api.moonshot.cn/anthropic"
        );
        assert_eq!(
            gateway_base(
                "https://api.moonshot.cn/anthropic",
                Protocol::Anthropic,
                Some("/anthropic")
            ),
            "https://api.moonshot.cn/anthropic"
        );
        assert_eq!(
            gateway_base(
                "https://api.moonshot.cn/v1",
                Protocol::OpenaiChat,
                Some("/anthropic")
            ),
            "https://api.moonshot.cn/v1"
        );
        assert_eq!(
            gateway_base("https://gw.example.com", Protocol::OpenaiResponses, None),
            "https://gw.example.com/v1"
        );
        assert_eq!(
            upstream_url(
                &gateway_base("https://gw.example.com", Protocol::OpenaiResponses, None),
                Protocol::OpenaiResponses,
                "/v1/responses"
            ),
            "https://gw.example.com/v1/responses"
        );
        assert_eq!(
            gateway_base("https://gw.example.com/v1", Protocol::Gemini, None),
            "https://gw.example.com"
        );
    }

    #[test]
    fn grok_live_entry_is_the_xai_login_not_another_issuer() {
        let doc = json!({
            "https://sso.corp.example::cli": { "key": "oidc", "user_id": "u0" },
            "https://auth.x.ai::cli": { "key": "xai", "user_id": "u1" },
            "api_key": { "key": "k" }
        });
        assert_eq!(grok_live_entry_in(&doc).unwrap()["key"], "xai");
        assert!(grok_live_entry_in(&json!({ "api_key": { "key": "k" } })).is_none());
    }

    /// The listen address: loopback by default, a LAN address only with a
    /// key, and this machine's CLIs keep using loopback unless the router is
    /// bound to one interface.
    #[test]
    fn listen_address_rules() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert_eq!(
            router_base_url(ip("127.0.0.1"), 8317),
            "http://127.0.0.1:8317"
        );
        assert_eq!(
            router_base_url(ip("0.0.0.0"), 8317),
            "http://127.0.0.1:8317"
        );
        assert_eq!(router_base_url(ip("::"), 8317), "http://127.0.0.1:8317");
        assert_eq!(
            router_base_url(ip("192.168.1.20"), 8317),
            "http://192.168.1.20:8317"
        );
        assert_eq!(router_base_url(ip("fe80::1"), 1), "http://[fe80::1]:1");
        assert_eq!(lan_url(ip("127.0.0.1"), 8317), None);
        assert_eq!(
            lan_url(ip("192.168.1.20"), 8317).as_deref(),
            Some("http://192.168.1.20:8317")
        );

        let patch = |host: Option<&str>, key: Option<&str>| RouterConfigPatch {
            autostart: None,
            host: host.map(str::to_string),
            port: None,
            api_key: key.map(str::to_string),
            strategy: None,
            upstreams: None,
        };
        let keyless = RouterConfig::default();
        // Exposing without a key is refused; with one it is accepted.
        assert!(apply_patch(keyless.clone(), patch(Some("0.0.0.0"), None)).is_err());
        let open = apply_patch(keyless.clone(), patch(Some("0.0.0.0"), Some("sk-x"))).unwrap();
        assert_eq!(open.host, "0.0.0.0");
        // Clearing the key while exposed is refused too.
        assert!(apply_patch(open.clone(), patch(None, Some(""))).is_err());
        // Not an IP → refused; loopback needs no key.
        assert!(apply_patch(keyless.clone(), patch(Some("my-host"), None)).is_err());
        assert!(apply_patch(keyless, patch(Some("::1"), None)).is_ok());
        // A hand-edited unparsable host reads as loopback, never open.
        assert_eq!(
            bind_ip(&config_from_json(json!({ "host": "nope" }))),
            ip("127.0.0.1")
        );
    }

    #[test]
    fn listen_address_choices_keep_the_fixed_two_first_and_the_saved_one() {
        let ifaces = vec![
            ("en0".to_string(), "192.168.1.20".parse().unwrap()),
            ("en1".to_string(), "10.0.0.5".parse().unwrap()),
        ];
        let list = listen_address_list("127.0.0.1", ifaces.clone());
        let addrs: Vec<&str> = list.iter().map(|a| a.address.as_str()).collect();
        assert_eq!(addrs, ["127.0.0.1", "0.0.0.0", "192.168.1.20", "10.0.0.5"]);
        assert_eq!(list[2].interface.as_deref(), Some("en0"));
        // A saved address no interface carries any more is still offered.
        let list = listen_address_list("192.168.9.9", ifaces);
        assert_eq!(list.last().unwrap().address, "192.168.9.9");
        assert!(list.last().unwrap().interface.is_none());
        // The real enumeration never offers loopback twice.
        let real = listen_addresses("127.0.0.1");
        assert_eq!(real.iter().filter(|a| a.address == "127.0.0.1").count(), 1);
    }

    #[test]
    fn connection_settings_are_locked_while_running() {
        let _g = lock_home(); // serializes the global listener slot
        assert!(refuse_while_running(true).is_ok());
        let (tx, _rx) = tokio::sync::watch::channel(false);
        *server_slot() = Some(ServerHandle {
            host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            started_at: 0,
            shutdown: tx,
        });
        let refused = refuse_while_running(true);
        let other_change = refuse_while_running(false);
        *server_slot() = None;
        assert_eq!(refused.unwrap_err(), STOP_TO_EDIT);
        // Strategy / autostart / upstream edits stay allowed while running.
        assert!(other_change.is_ok());
    }

    /// A gateway's live listing REPLACES the list saved at detection time
    /// (vendors rename models — DeepSeek's `deepseek-v4-flash` became
    /// `deepseek-flash`); the saved list is only the fallback.
    #[test]
    fn a_gateways_live_listing_replaces_its_detected_models() {
        let id = format!("gw-live-{}", now_millis());
        let stores = Stores {
            accounts: Vec::new(),
            providers: Vec::new(),
            gateways: vec![json!({
                "id": id, "kind": "gateway", "baseUrl": "https://g.example",
                "capabilities": { "openaiCompatible": true, "models": ["old-name", "kept"] }
            })],
            disabled: std::collections::HashSet::new(),
        };
        let key = format!("gateway:{id}");
        assert_eq!(
            upstream_models(&stores, &key),
            Some(vec!["old-name".to_string(), "kept".to_string()])
        );
        catalog_store(&key, vec!["new-name".into(), "kept".into()]);
        assert_eq!(
            upstream_models(&stores, &key),
            Some(vec!["new-name".to_string(), "kept".to_string()])
        );
        CATALOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }

    #[test]
    fn known_reasons_get_stable_codes() {
        assert_eq!(reason_code("not logged in"), Some("not_logged_in"));
        assert_eq!(
            reason_code("token expired — run codex once"),
            Some("token_expired")
        );
        assert_eq!(reason_code("no detected API"), Some("no_detected_api"));
        assert_eq!(reason_code("something else"), None);
    }

    #[test]
    fn enabled_keys_keep_saved_order() {
        let mut cfg = RouterConfig {
            upstreams: vec![
                UpstreamPref {
                    key: "a".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "c".into(),
                    enabled: false,
                },
                UpstreamPref {
                    key: "b".into(),
                    enabled: true,
                },
            ],
            ..Default::default()
        };
        assert_eq!(enabled_keys(&cfg), vec!["a", "b"]);
        cfg.strategy = Strategy::RoundRobin;
        assert_eq!(enabled_keys(&cfg), vec!["a", "b"]);
    }

    /// Fill-first takes the first eligible member every time; round-robin
    /// takes the one after the last pick FOR THAT MODEL, wrapping.
    #[test]
    fn member_selection_is_fill_first_or_round_robin_per_model() {
        let m = format!("rr-model-{}", now_millis());
        let all = ["a", "b", "c"];
        assert_eq!(pick_member(Strategy::Failover, &m, &all), Some("a"));
        assert_eq!(pick_member(Strategy::Failover, &m, &all), Some("a"));
        let picks: Vec<_> = (0..4)
            .map(|_| pick_member(Strategy::RoundRobin, &m, &all).unwrap())
            .collect();
        assert_eq!(picks, ["a", "b", "c", "a"]);
        // An excluded member is skipped without disturbing the rotation.
        assert_eq!(
            pick_member(Strategy::RoundRobin, &m, &["a", "c"]),
            Some("c")
        );
        assert_eq!(pick_member(Strategy::RoundRobin, &m, &[]), None);
    }

    #[test]
    fn public_key_masks_only_the_account_id() {
        assert_eq!(public_key("live:codex"), "live:codex");
        assert_eq!(public_key("provider:abc"), "provider:abc");
        assert_eq!(
            public_key("account:claude:someone@example.com"),
            format!(
                "account:claude:{}",
                crate::providers::mask_secret("someone@example.com")
            )
        );
    }

    #[test]
    fn quota_spent_counts_account_wide_windows_only() {
        let q = |success: bool, tiers: Vec<(&str, Option<&str>, f64)>| {
            crate::quota::SubscriptionQuota {
                app: "codex".into(),
                credential_status: crate::quota::CredentialStatus::Valid,
                success,
                tiers: tiers
                    .into_iter()
                    .map(|(n, g, u)| crate::quota::QuotaTier {
                        name: n.into(),
                        group: g.map(str::to_string),
                        utilization: u,
                        resets_at: None,
                    })
                    .collect(),
                plan: None,
                extra_usage: None,
                prepaid_balance: None,
                error: None,
                queried_at: None,
            }
        };
        assert_eq!(
            quota_spent(&q(true, vec![("five_hour", None, 98.0)])),
            Some(true)
        );
        assert_eq!(
            quota_spent(&q(true, vec![("seven_day", None, 97.9)])),
            Some(false)
        );
        assert_eq!(
            quota_spent(&q(true, vec![("Fable", Some("weekly"), 100.0)])),
            Some(false)
        );
        assert_eq!(
            quota_spent(&q(false, vec![("five_hour", None, 100.0)])),
            None
        );
    }

    #[test]
    fn conversation_id_prefers_session_headers_then_first_user_message() {
        let mut h = hyper::HeaderMap::new();
        h.insert("x-session-id", "abc".parse().unwrap());
        assert_eq!(conversation_id(&h, b"{}"), "abc");
        let empty = hyper::HeaderMap::new();
        let a = br#"{"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"}]}"#;
        let b = br#"{"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"},{"role":"assistant","content":"x"},{"role":"user","content":"more"}]}"#;
        let c = br#"{"messages":[{"role":"user","content":"other"}]}"#;
        assert_eq!(conversation_id(&empty, a), conversation_id(&empty, b));
        assert_ne!(conversation_id(&empty, a), conversation_id(&empty, c));
        assert!(conversation_id(&empty, a).starts_with("magpie-"));
    }

    #[test]
    fn within_turn_reads_tool_results_per_api() {
        let j = |s: &str| serde_json::from_str::<JsonValue>(s).unwrap();
        assert!(within_turn(
            Protocol::Anthropic,
            Some(&j(
                r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t"}]}]}"#
            ))
        ));
        assert!(!within_turn(
            Protocol::Anthropic,
            Some(&j(r#"{"messages":[{"role":"user","content":"hi"}]}"#))
        ));
        assert!(within_turn(
            Protocol::OpenaiChat,
            Some(&j(r#"{"messages":[{"role":"tool","content":"r"}]}"#))
        ));
        assert!(within_turn(
            Protocol::OpenaiResponses,
            Some(&j(
                r#"{"input":[{"type":"function_call_output","output":"r"}]}"#
            ))
        ));
        assert!(within_turn(
            Protocol::Gemini,
            Some(&j(
                r#"{"contents":[{"role":"user","parts":[{"functionResponse":{}}]}]}"#
            ))
        ));
    }

    fn quota_member(key: &str) -> (String, Result<Upstream, String>) {
        (
            key.to_string(),
            Ok(Upstream {
                protocol: Protocol::OpenaiResponses,
                base_url: String::new(),
                auth: Auth::ApiKey(String::new()),
                login: None,
                expires_at: None,
            }),
        )
    }

    #[test]
    fn quota_order_moves_low_and_spent_members_back() {
        let set = |k: &str, u: f64| {
            member_quotas().insert(
                k.to_string(),
                MemberQuota {
                    at: std::time::Instant::now(),
                    used: Some(u),
                },
            );
        };
        set("q:spent", 99.0);
        set("q:low", 95.0);
        set("q:fine", 10.0);
        let mut m = vec![
            quota_member("q:spent"),
            quota_member("q:low"),
            quota_member("q:unknown"),
            quota_member("q:fine"),
        ];
        order_by_quota(&mut m);
        let keys: Vec<&str> = m.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["q:unknown", "q:fine", "q:low", "q:spent"]);
    }

    #[test]
    fn router_url_means_this_routers_port_not_any_local_proxy() {
        let cfg = RouterConfig {
            port: 8317,
            former_ports: vec![9000],
            ..Default::default()
        };
        assert!(is_router_url_for("http://127.0.0.1:8317/v1", &cfg));
        assert!(is_router_url_for("http://127.0.0.1:8317", &cfg));
        // A port the router used before (left there by an unclean exit).
        assert!(is_router_url_for("http://127.0.0.1:9000/v1", &cfg));
        // Someone else's local proxy.
        assert!(!is_router_url_for("http://127.0.0.1:4000/v1", &cfg));
        assert!(!is_router_url_for("http://127.0.0.1:11434", &cfg));
        assert!(!is_router_url_for("https://api.example.com/v1", &cfg));
    }

    #[test]
    fn a_port_change_remembers_the_former_port() {
        let patch = |p: u16| RouterConfigPatch {
            port: Some(p),
            ..Default::default()
        };
        let cfg = apply_patch(RouterConfig::default(), patch(9000)).unwrap();
        assert_eq!(cfg.former_ports, vec![DEFAULT_PORT]);
        let cfg = apply_patch(cfg, patch(DEFAULT_PORT)).unwrap();
        assert_eq!(cfg.former_ports, vec![9000]);
        let mut cfg = cfg;
        for p in 1..=10u16 {
            cfg = apply_patch(cfg, patch(10_000 + p)).unwrap();
        }
        assert_eq!(cfg.former_ports.len(), FORMER_PORTS_KEPT);
    }

    #[test]
    fn in_stream_errors_carry_the_status_they_stand_for() {
        assert_eq!(
            in_stream_error_status(
                &json!({"type": "error", "error": {"type": "invalid_request_error",
                "message": "Could not decrypt the provided encrypted_content."}})
            ),
            Some(400)
        );
        assert_eq!(
            in_stream_error_status(&json!({"type": "response.failed", "response": {"error":
                {"code": "invalid_encrypted_content"}}})),
            Some(400)
        );
        assert_eq!(
            in_stream_error_status(
                &json!({"type": "error", "error": {"type": "overloaded_error"}})
            ),
            Some(529)
        );
        assert_eq!(
            in_stream_error_status(&json!({"type": "error", "status": 429})),
            Some(429)
        );
        assert_eq!(in_stream_error_status(&json!({"type": "error"})), None);
    }

    #[test]
    fn huge_retry_after_values_are_held_to_sane_ceilings() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "18446744073709551615".parse().unwrap());
        assert_eq!(retry_after(&h), Some(REST_MAX));
        let year = Some(Duration::from_secs(365 * 24 * 3600));
        let fail = |status: u16| Failure {
            status: Some(status),
            retry_after: year,
            text: String::new(),
            transport: false,
            credential_scoped: false,
            request_scoped: false,
        };
        let now = now_millis();
        let until = |k: &str| model_blocked(k, "m-cap").unwrap().0 - now;
        mark_model_failure("cap:503", "m-cap", &fail(503));
        assert!(until("cap:503") <= QUOTA_BACKOFF_MAX.as_millis() as u64 + 1000);
        mark_model_failure("cap:404", "m-cap", &fail(404));
        assert!(until("cap:404") <= COOLDOWN_MODEL_SUPPORT.as_millis() as u64 + 1000);
        mark_model_failure("cap:429", "m-cap", &fail(429));
        assert!(until("cap:429") <= REST_MAX.as_millis() as u64 + 1000);
    }

    #[test]
    fn an_expired_quota_block_does_not_label_a_later_failure() {
        let fail = |status: u16| Failure {
            status: Some(status),
            retry_after: None,
            text: String::new(),
            transport: false,
            credential_scoped: false,
            request_scoped: false,
        };
        mark_model_failure("q:flag", "m-flag", &fail(429));
        assert_eq!(
            model_blocked("q:flag", "m-flag").map(|(_, q)| q),
            Some(true)
        );
        // The 429 window runs out…
        model_states()
            .get_mut(&("q:flag".to_string(), "m-flag".to_string()))
            .unwrap()
            .next_retry_after = 0;
        // …and the next failure is a 503: no longer a quota block.
        mark_model_failure("q:flag", "m-flag", &fail(503));
        assert_eq!(
            model_blocked("q:flag", "m-flag").map(|(_, q)| q),
            Some(false)
        );
    }

    #[test]
    fn a_providers_api_follows_its_sdk_or_backend_not_just_its_tool() {
        let p = |v: JsonValue| -> crate::providers::Provider { serde_json::from_value(v).unwrap() };
        let base = json!({ "id": "x", "kind": "custom", "name": "X",
            "baseUrl": "https://api.example.com/v1", "apiKey": "k" });
        let with = |extra: JsonValue| {
            let mut v = base.clone();
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            p(v)
        };
        assert_eq!(
            provider_wire_protocol(&with(
                json!({ "app": "opencode", "npm": "@ai-sdk/anthropic" })
            )),
            Protocol::Anthropic
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "opencode", "npm": "@ai-sdk/openai" }))),
            Protocol::OpenaiResponses
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "opencode", "npm": "@ai-sdk/google" }))),
            Protocol::Gemini
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "opencode" }))),
            Protocol::OpenaiChat
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "grok", "apiBackend": "messages" }))),
            Protocol::Anthropic
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "grok", "apiBackend": "responses" }))),
            Protocol::OpenaiResponses
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "grok" }))),
            Protocol::OpenaiChat
        );
        assert_eq!(
            provider_wire_protocol(&with(json!({ "app": "claude" }))),
            Protocol::Anthropic
        );
    }

    #[test]
    fn only_a_listing_never_fetched_is_waited_for() {
        let listed = "cat:listed";
        let failed = "cat:failed";
        assert!(catalog_never_fetched("cat:never"));
        // Listed once — even if it goes stale it is not waited for again.
        CATALOG.lock().unwrap().insert(
            listed.to_string(),
            (
                std::time::Instant::now() - CATALOG_TTL * 2,
                vec!["m".into()],
            ),
        );
        assert!(!catalog_never_fetched(listed));
        assert!(!catalog_is_fresh(listed));
        // Attempted and failed: not waited for either.
        catalog_tried().insert(failed.to_string(), std::time::Instant::now());
        assert!(!catalog_never_fetched(failed));
    }

    #[test]
    fn stick_map_is_capped_dropping_the_oldest() {
        let base = sticks().len();
        for i in 0..(STICKS_MAX + 50) {
            remember_stick(&format!("cap-test|{i}"), "m");
        }
        let m = sticks();
        assert!(m.len() <= STICKS_MAX.max(base));
        // The newest survive.
        assert!(m.contains_key(&format!("cap-test|{}", STICKS_MAX + 49)));
        assert!(!m.contains_key("cap-test|0"));
    }

    #[test]
    fn router_models_groups_sources_per_model() {
        let _g = lock_home();
        let dir = tempdir("models-list");
        let _h = override_home(&dir);
        *config_cache() = None;
        crate::config::write_providers(&json!([
            { "id": "pa", "app": "claude", "kind": "custom", "name": "Alpha",
              "baseUrl": "http://127.0.0.1:1", "apiKey": "k",
              "model": "m-shared", "models": [{ "id": "m-alpha" }] },
            { "id": "pb", "app": "claude", "kind": "custom", "name": "Beta",
              "baseUrl": "http://127.0.0.1:1", "apiKey": "k",
              "model": "m-shared" },
            { "id": "pc", "app": "claude", "kind": "custom", "name": "Off",
              "baseUrl": "http://127.0.0.1:1", "apiKey": "k", "model": "m-off" }
        ]))
        .unwrap();
        let cfg = RouterConfig {
            upstreams: vec![
                UpstreamPref {
                    key: "provider:pa".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "provider:pb".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "provider:pc".into(),
                    enabled: false,
                },
            ],
            ..Default::default()
        };
        let list = router_models_list(&cfg);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m-shared", "m-alpha"]);
        assert_eq!(list[0].sources, ["Alpha", "Beta"]);
        assert_eq!(list[1].sources, ["Alpha"]);
        assert!(list.iter().all(|m| m.available));
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_is_minted_only_when_the_entry_is_first_created() {
        let _g = lock_home();
        let dir = tempdir("mint");
        let _h = override_home(&dir);
        *config_cache() = None;
        let first = sync_router_gateway().unwrap();
        let minted = current_config().api_key;
        assert!(!minted.is_empty());
        assert_eq!(first["apiKey"], minted.as_str());
        // The user clears it: a later sync leaves it cleared.
        update_config(|c| {
            c.api_key.clear();
            Ok(())
        })
        .unwrap();
        let again = sync_router_gateway().unwrap();
        assert_eq!(current_config().api_key, "");
        assert_eq!(again["apiKey"], "");
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn affinity_keeps_the_member_within_a_turn_and_while_warm() {
        let plan = vec![quota_member("s:a"), quota_member("s:b")];
        remember_stick("m|conv-1", "s:b");
        // Within a turn: kept, round-robin or not.
        assert_eq!(
            sticky_member("m|conv-1", &plan, "m", true, true).as_deref(),
            Some("s:b")
        );
        // A new turn while the cache is warm: kept on fill-first only.
        assert_eq!(
            sticky_member("m|conv-1", &plan, "m", false, false).as_deref(),
            Some("s:b")
        );
        assert_eq!(sticky_member("m|conv-1", &plan, "m", false, true), None);
        // Gone from the plan: not kept.
        assert_eq!(
            sticky_member("m|conv-1", &plan[..1], "m", true, false),
            None
        );
        // Spent: not kept.
        member_quotas().insert(
            "s:b".into(),
            MemberQuota {
                at: std::time::Instant::now(),
                used: Some(99.0),
            },
        );
        assert_eq!(sticky_member("m|conv-1", &plan, "m", true, false), None);
        member_quotas().remove("s:b");
    }

    #[test]
    fn write_config_refuses_to_replace_an_unreadable_file() {
        let _g = lock_home();
        let dir = tempdir("torn");
        let _h = override_home(&dir);
        *config_cache() = None;
        let path = config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"port\": 90").unwrap();
        // Defaults are served but NOT cached...
        assert_eq!(current_config(), RouterConfig::default());
        assert!(config_cache().is_none());
        // ...and a write does not clobber the torn file.
        assert!(write_config(&RouterConfig::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"port\": 90");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_group_by_tool_order_and_keep_source_order_inside() {
        let mk = |key: &str, app: Option<CliApp>, kind: CandidateKind| Candidate {
            key: key.into(),
            kind,
            app,
            label: key.into(),
            detail: String::new(),
            protocols: vec![],
            available: true,
            reason: None,
            reason_code: None,
            enabled: false,
        };
        // Enumeration order: live logins, then accounts, then providers.json.
        let found = vec![
            mk("live:claude", Some(CliApp::Claude), CandidateKind::Live),
            mk("live:codex", Some(CliApp::Codex), CandidateKind::Live),
            mk(
                "account:codex:1",
                Some(CliApp::Codex),
                CandidateKind::Account,
            ),
            mk("provider:c1", Some(CliApp::Claude), CandidateKind::Provider),
            mk("provider:x1", Some(CliApp::Codex), CandidateKind::Provider),
            mk("provider:c2", Some(CliApp::Claude), CandidateKind::Provider),
            mk("gateway:g", None, CandidateKind::Gateway),
        ];
        // The user placed one Claude provider above the live login.
        let cfg = RouterConfig {
            upstreams: vec![
                UpstreamPref {
                    key: "provider:c2".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "live:claude".into(),
                    enabled: true,
                },
            ],
            ..Default::default()
        };
        // Settings puts Codex before Claude.
        let tools = [CliApp::Codex, CliApp::Claude, CliApp::Gemini];
        let out = order_candidates_with(found, &cfg, &tools);
        let keys: Vec<&str> = out.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "live:codex",
                "account:codex:1",
                "provider:x1",
                "provider:c2",
                "live:claude",
                "provider:c1",
                "gateway:g"
            ]
        );
    }

    #[test]
    fn order_candidates_uses_config_order_then_appends_unknown_disabled() {
        let cfg = RouterConfig {
            upstreams: vec![
                UpstreamPref {
                    key: "provider:z".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "gone".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "live:codex".into(),
                    enabled: false,
                },
            ],
            ..Default::default()
        };
        let found = vec![
            cand("live:codex", &[Protocol::OpenaiResponses], true),
            cand("provider:a", &[Protocol::Anthropic], true),
            cand("provider:z", &[Protocol::Anthropic], false),
        ];
        let out = order_candidates_with(found, &cfg, &[CliApp::Claude, CliApp::Codex]);
        let keys: Vec<&str> = out.iter().map(|c| c.key.as_str()).collect();
        // `cand` builds tool-less candidates, so they all share one group:
        // saved order first, then the unplaced one.
        assert_eq!(keys, vec!["provider:z", "live:codex", "provider:a"]);
        assert!(out[0].enabled);
        assert!(!out[1].enabled);
        assert!(!out[2].enabled, "unknown candidates are appended disabled");
    }

    #[test]
    fn client_key_check_accepts_every_header_form() {
        // No key, no use.
        assert!(!client_key_matches("", None, None, None, None));
        assert!(!client_key_matches(
            "",
            Some("Bearer anything"),
            None,
            None,
            None
        ));
        assert!(client_key_matches(
            "k1",
            Some("Bearer k1"),
            None,
            None,
            None
        ));
        assert!(client_key_matches("k1", None, Some("k1"), None, None));
        assert!(client_key_matches("k1", None, None, Some("k1"), None));
        assert!(client_key_matches(
            "k1",
            None,
            None,
            None,
            Some("alt=sse&key=k1")
        ));
        assert!(!client_key_matches(
            "k1",
            Some("Bearer k2"),
            None,
            None,
            None
        ));
        assert!(!client_key_matches("k1", None, None, None, None));
    }

    #[test]
    fn request_header_drop_list_is_hop_by_hop_framing_and_credentials_only() {
        for h in [
            "host",
            "connection",
            "keep-alive",
            "proxy-connection",
            "transfer-encoding",
            "te",
            "trailer",
            "upgrade",
            "content-length",
            "accept-encoding",
            "authorization",
            "x-api-key",
            "x-goog-api-key",
            "chatgpt-account-id",
        ] {
            assert!(is_dropped_request_header(h), "{h}");
        }
        // Everything a CLI sends that the vendor reads goes through verbatim.
        for h in [
            "content-type",
            "anthropic-version",
            "anthropic-beta",
            "x-app",
            "user-agent",
            "openai-beta",
            "originator",
            "session_id",
            "x-codex-installation-id",
            "content-encoding",
            "accept",
        ] {
            assert!(!is_dropped_request_header(h), "{h}");
        }
    }

    /// The live backend's shape: items arrive as `output_item.done`, and the
    /// terminal event's `output` is empty.
    #[test]
    fn responses_sse_fold_restores_output_items() {
        let sse = "event: response.output_item.done\r\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs\"}}\r\n\r\nevent: response.output_item.done\r\ndata: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"pong\"}]}}\r\n\r\nevent: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[]}}\r\n\r\n";
        let (status, body) = fold_responses_sse(sse);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["output"].as_array().unwrap().len(), 2);
        assert_eq!(body["output"][1]["content"][0]["text"], "pong");
        assert_eq!(body["status"], "completed");
        // No items streamed → the terminal event's own output is kept.
        let (_, body) = fold_responses_sse(
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"x\"}]}}\n\n",
        );
        assert_eq!(body["output"], json!([{ "type": "x" }]));
    }

    #[test]
    fn responses_sse_folds_to_the_completed_response() {
        let sse = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\"}}\n\n";
        let (status, body) = fold_responses_sse(sse);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "r1");
        assert_eq!(body["status"], "completed");
        let (status, body) =
            fold_responses_sse("data: {\"type\":\"error\",\"error\":{\"message\":\"nope\"}}\n\n");
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["message"], "nope");
        let (status, _) = fold_responses_sse("data: {\"type\":\"response.created\"}\n\n");
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn requested_model_reads_the_body_or_the_gemini_path() {
        assert_eq!(
            requested_model("/v1/responses", br#"{"model":"gpt-5.5"}"#).as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(
            requested_model("/v1beta/models/gemini-2.5-pro:streamGenerateContent", b"{}")
                .as_deref(),
            Some("gemini-2.5-pro")
        );
        assert_eq!(requested_model("/v1/messages", b"not json"), None);
    }

    #[test]
    fn model_rank_prefers_listed_then_unknown_then_absent() {
        let listed = Some(vec!["gpt-5.5".to_string()]);
        let other = Some(vec!["deepseek-v4-pro".to_string()]);
        assert_eq!(model_rank(&listed, "gpt-5.5"), 0);
        assert_eq!(model_rank(&None, "gpt-5.5"), 1);
        assert_eq!(model_rank(&other, "gpt-5.5"), 2);
    }

    #[test]
    fn anthropic_index_fixer_fills_missing_indexes_across_chunk_boundaries() {
        let mut f = AnthropicIndexFixer::new();
        let stream = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"po\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\"}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":7,\"delta\":{}}\n"
        );
        // Split mid-line to prove partial lines wait.
        let (a, b) = stream.as_bytes().split_at(90);
        let mut out = f.feed(a);
        out.extend(f.feed(b));
        out.extend(f.finish());
        let text = String::from_utf8(out).unwrap();
        let datas: Vec<JsonValue> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| serde_json::from_str(d.trim()).unwrap())
            .collect();
        assert_eq!(datas[1]["index"], 1, "delta gets the started block's index");
        assert_eq!(datas[1]["delta"]["text"], "po");
        assert_eq!(datas[2]["index"], 1, "stop too");
        assert_eq!(datas[3]["index"], 7, "an index already present is kept");
        assert!(
            text.contains("event: content_block_delta\n"),
            "non-data lines pass through"
        );
    }

    #[test]
    fn namespace_tools_are_dropped_for_grok_and_nothing_else() {
        let body = br#"{"model":"grok-4.7","tools":[{"type":"function","name":"shell"},{"type":"namespace","name":"mcp__x","tools":[]},{"type":"web_search"}]}"#;
        let out: JsonValue = serde_json::from_slice(&strip_unsupported_tools(body)).unwrap();
        let types: Vec<&str> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["function", "web_search"]);
        // Nothing to drop → byte-for-byte untouched.
        let plain = br#"{"model":"m","tools":[{"type":"function","name":"a"}]}"#;
        assert_eq!(&strip_unsupported_tools(plain)[..], &plain[..]);
    }

    #[test]
    fn only_codex_and_grok_logins_are_background_refreshed() {
        for k in [
            "live:codex",
            "live:grok",
            "account:codex:x",
            "account:grok:y",
        ] {
            assert!(is_refreshable_login_key(k), "{k}");
        }
        for k in ["live:claude", "account:claude:z", "provider:p", "gateway:g"] {
            assert!(!is_refreshable_login_key(k), "{k}");
        }
    }

    #[test]
    fn strip_key_param_keeps_other_query_params() {
        assert_eq!(strip_key_param("alt=sse&key=abc"), "alt=sse");
        assert_eq!(strip_key_param("key=abc"), "");
        assert_eq!(strip_key_param("a=1&b=2"), "a=1&b=2");
    }

    #[test]
    fn grok_auth_reads_the_entry_and_serves_three_apis() {
        let live = json!({ "key": fake_jwt(json!({ "exp": now_secs() + 3600 })), "user_id": "u-1", "email": "g@example.com" });
        match grok_oauth_auth(&live).unwrap() {
            Auth::GrokOauth { user_id, email, .. } => {
                assert_eq!(user_id, "u-1");
                assert_eq!(email.as_deref(), Some("g@example.com"));
            }
            _ => panic!(),
        }
        let dead = json!({ "key": fake_jwt(json!({ "exp": 10 })), "user_id": "u-1" });
        assert!(!grok_oauth_status(&dead).0, "expired and not refreshable");
        let refreshable = json!({ "key": fake_jwt(json!({ "exp": 10 })), "user_id": "u-1",
            "refresh_token": "rt", "oidc_client_id": "c" });
        assert_eq!(grok_oauth_status(&refreshable), (true, None));
        assert_eq!(grok_key_exp(&refreshable), Some(10));
        assert!(
            grok_oauth_auth(&json!({ "key": "k" })).is_err(),
            "user_id required"
        );
        // A saved snapshot wraps the entry under `auth`.
        let snapshot = json!({ "payload": { "scope": "s", "auth": live } });
        assert!(account_auth(CliApp::Grok, &snapshot).is_ok());
        // The cli-chat-proxy base carries /v1; every protocol lands on it.
        assert_eq!(
            upstream_url(GROK_CLI_CHAT_BASE_URL, Protocol::Anthropic, "/v1/messages"),
            "https://cli-chat-proxy.grok.com/v1/messages"
        );
        assert_eq!(
            upstream_url(
                GROK_CLI_CHAT_BASE_URL,
                Protocol::OpenaiResponses,
                "/v1/responses"
            ),
            "https://cli-chat-proxy.grok.com/v1/responses"
        );
        assert_eq!(
            upstream_url(
                GROK_CLI_CHAT_BASE_URL,
                Protocol::OpenaiChat,
                "/v1/chat/completions"
            ),
            "https://cli-chat-proxy.grok.com/v1/chat/completions"
        );
    }

    fn fake_jwt(claims: JsonValue) -> String {
        use base64::Engine;
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            e.encode(r#"{"alg":"none"}"#),
            e.encode(claims.to_string())
        )
    }

    #[test]
    fn codex_auth_reads_account_id_from_tokens_or_id_token_claim() {
        let with_id = json!({ "tokens": { "access_token": "at", "account_id": "acc-1" } });
        match codex_oauth_auth(&with_id).unwrap() {
            Auth::ChatGpt { token, account_id } => {
                assert_eq!(token, "at");
                assert_eq!(account_id.as_deref(), Some("acc-1"));
            }
            _ => panic!(),
        }
        let id_token = fake_jwt(json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-2" }
        }));
        let from_claim = json!({ "tokens": { "access_token": "at", "id_token": id_token } });
        match codex_oauth_auth(&from_claim).unwrap() {
            Auth::ChatGpt { account_id, .. } => assert_eq!(account_id.as_deref(), Some("acc-2")),
            _ => panic!(),
        }
        assert_eq!(
            codex_doc_email(&from_claim).as_deref(),
            Some("me@example.com")
        );
        // Expired: usable while a refresh token exists (the router refreshes
        // it), unavailable without one.
        let expired = json!({ "tokens": { "access_token": fake_jwt(json!({ "exp": 10 })) } });
        assert!(codex_oauth_auth(&expired).is_ok());
        assert!(!codex_oauth_status(&expired).0);
        let refreshable = json!({ "tokens": { "access_token": fake_jwt(json!({ "exp": 10 })), "refresh_token": "rt" } });
        assert_eq!(codex_oauth_status(&refreshable), (true, None));
        assert_eq!(codex_access_exp(&refreshable), Some(10));
        let api_key_only = json!({ "OPENAI_API_KEY": "sk" });
        assert!(codex_oauth_auth(&api_key_only).is_err());
    }

    #[test]
    fn config_round_trips_and_tolerates_unknown_values() {
        let _g = lock_home();
        let dir = tempdir("cfg");
        let _h = override_home(&dir);
        assert_eq!(read_config().unwrap(), RouterConfig::default());
        let cfg = RouterConfig {
            autostart: true,
            host: "0.0.0.0".into(),
            port: 9000,
            api_key: "tm-abc".into(),
            strategy: Strategy::RoundRobin,
            upstreams: vec![UpstreamPref {
                key: "live:codex".into(),
                enabled: true,
            }],
            suspended_bindings: vec!["b1".into(), "b2".into()],
            suspended_defaults: vec!["b2".into()],
            pending_codex_follow: true,
            former_ports: vec![8300],
        };
        write_config(&cfg).unwrap();
        *config_cache() = None;
        assert_eq!(read_config().unwrap(), cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(config_path().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A newer build's unknown strategy / bad port degrade, never error.
        std::fs::write(
            config_path().unwrap(),
            r#"{"port": 70000, "strategy": "weighted", "upstreams": [{"key": ""}, {"key": "a", "enabled": true}]}"#,
        )
        .unwrap();
        let lenient = read_config().unwrap();
        assert_eq!(lenient.port, DEFAULT_PORT);
        assert_eq!(lenient.strategy, Strategy::Failover);
        assert_eq!(lenient.upstreams.len(), 1);
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_suspend_keeps_what_the_first_recorded() {
        let merged = merge_suspended(&["a".into(), "b".into()], vec!["b".into(), "c".into()]);
        assert_eq!(merged, vec!["a", "b", "c"]);
        assert_eq!(merge_suspended(&[], vec!["x".into()]), vec!["x"]);
    }

    #[test]
    fn suspended_bindings_round_trip_and_nothing_live_suspends_nothing() {
        let _g = lock_home();
        let dir = tempdir("suspend");
        let _h = override_home(&dir);
        *config_cache() = None;
        crate::config::write_gateways(&json!([{
            "kind": "router", "id": "r-1", "name": "Local Router", "baseUrl": "http://127.0.0.1:8317",
            "apiKey": "sk-x", "bindings": [{ "id": "pb-1", "app": "claude", "model": "m" }]
        }]))
        .unwrap();
        // No CLI config points at the binding → nothing to suspend, no write.
        assert_eq!(suspend_live_bindings(CodexFollow::Now).unwrap(), 0);
        assert!(read_config().unwrap().suspended_bindings.is_empty());
        // The field survives a write/read and lenient parse.
        write_config(&RouterConfig {
            suspended_bindings: vec!["pb-1".into()],
            ..Default::default()
        })
        .unwrap();
        *config_cache() = None;
        assert_eq!(read_config().unwrap().suspended_bindings, vec!["pb-1"]);
        // Restore while the listener is down is refused by the activation
        // guard — a passing reason, so the id is kept for the next Start
        // (restore runs once per Start; nothing loops).
        assert_eq!(restore_suspended_bindings().unwrap(), 0);
        assert_eq!(read_config().unwrap().suspended_bindings, vec!["pb-1"]);
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The review's lifecycle cases, end to end on a real Claude Code
    /// settings.json in a temp HOME: a key change reaches the CLI (the in-use
    /// set is taken BEFORE the sync), Stop still recognises the binding
    /// afterwards, restore puts it back, and a CLI the user moved elsewhere
    /// while stopped keeps that choice.
    #[test]
    fn key_change_reaches_the_cli_and_restore_respects_the_users_switch() {
        let _g = lock_home();
        let dir = tempdir("lifecycle");
        let _h = override_home(&dir);
        *config_cache() = None;
        write_config(&RouterConfig {
            api_key: "sk-old".into(),
            ..Default::default()
        })
        .unwrap();
        sync_router_gateway().unwrap();
        let mut gws = crate::config::read_gateways().unwrap();
        gws[0]["bindings"] = json!([{ "id": "b1", "app": "claude", "model": "m" }]);
        crate::config::write_gateways(&gws).unwrap();
        // Pretend the listener is up (the activation guard).
        let (tx, _rx) = tokio::sync::watch::channel(false);
        *server_slot() = Some(ServerHandle {
            host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            started_at: 0,
            shutdown: tx,
        });
        let token = || -> String {
            let text = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
            let v: JsonValue = serde_json::from_str(&text).unwrap();
            v["env"]["ANTHROPIC_AUTH_TOKEN"]
                .as_str()
                .unwrap_or("")
                .to_string()
        };
        let synth = || {
            crate::providers::gateway_providers()
                .into_iter()
                .find(|p| p.id == "b1")
                .unwrap()
        };
        // Clears the fake listener even when an assertion fails, so the
        // next test is not handed a "running" router.
        struct SlotReset;
        impl Drop for SlotReset {
            fn drop(&mut self) {
                *server_slot() = None;
            }
        }
        let _reset = SlotReset;
        crate::providers::activate(&synth(), &[synth()]).unwrap();
        crate::config::set_active_provider_marker("claude", Some("b1")).unwrap();
        assert_eq!(token(), "sk-old");

        // Regenerate the key: in use captured first, then synced + re-applied.
        let live = live_router_bindings().unwrap();
        assert_eq!(live.len(), 1);
        update_config(|c| {
            c.api_key = "sk-new".into();
            Ok(())
        })
        .unwrap();
        sync_router_gateway().unwrap();
        reapply_bindings(&live).unwrap();
        assert_eq!(token(), "sk-new");

        // Stop recognises it and hands Claude back; restore puts it back.
        assert_eq!(suspend_live_bindings(CodexFollow::Now).unwrap(), 1);
        assert_eq!(token(), "");
        assert_eq!(read_config().unwrap().suspended_bindings, vec!["b1"]);
        assert_eq!(restore_suspended_bindings().unwrap(), 1);
        assert_eq!(token(), "sk-new");

        // Suspend again, then the user picks their own provider while stopped:
        // restore must not take Claude back, and the id is consumed.
        assert_eq!(suspend_live_bindings(CodexFollow::Now).unwrap(), 1);
        let own: crate::providers::Provider = serde_json::from_value(json!({
            "id": "mine", "app": "claude", "kind": "custom", "name": "Mine",
            "baseUrl": "https://api.example.com", "apiKey": "sk-mine"
        }))
        .unwrap();
        crate::providers::activate(&own, &[own.clone()]).unwrap();
        assert_eq!(restore_suspended_bindings().unwrap(), 0);
        assert_eq!(token(), "sk-mine");
        assert!(read_config().unwrap().suspended_bindings.is_empty());

        // A restore that fails for a passing reason (Claude's settings.json
        // torn mid-write) KEEPS the id for the next Start.
        crate::providers::deactivate(CliApp::Claude, &[own.clone()]).unwrap();
        update_config(|c| {
            c.suspended_bindings = vec!["b1".into()];
            Ok(())
        })
        .unwrap();
        let settings = dir.join(".claude/settings.json");
        let good = std::fs::read_to_string(&settings).unwrap();
        std::fs::write(&settings, "{\"env\": {").unwrap();
        assert_eq!(restore_suspended_bindings().unwrap(), 0);
        assert_eq!(read_config().unwrap().suspended_bindings, vec!["b1"]);
        std::fs::write(&settings, good).unwrap();
        assert_eq!(restore_suspended_bindings().unwrap(), 1);
        assert_eq!(token(), "sk-new");
        assert!(read_config().unwrap().suspended_bindings.is_empty());

        *server_slot() = None;
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two writers from stale copies never erase each other's fields.
    #[test]
    fn update_config_starts_from_the_file_not_the_cache() {
        let _g = lock_home();
        let dir = tempdir("rmw");
        let _h = override_home(&dir);
        *config_cache() = None;
        write_config(&RouterConfig::default()).unwrap();
        // A stale cached copy (as the page's snapshot would be)…
        let stale = current_config();
        update_config(|c| {
            c.suspended_bindings = vec!["x".into()];
            Ok(())
        })
        .unwrap();
        // …then a patch built on the FILE keeps the other writer's field.
        let after = update_config(|c| {
            c.port = stale.port + 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(after.suspended_bindings, vec!["x"]);
        // A refusing closure writes nothing.
        assert!(update_config(|_| Err("no".into())).is_err());
        assert_eq!(read_config().unwrap().port, stale.port + 1);
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tool_switched_off_in_settings_is_hidden_and_not_routed() {
        let _g = lock_home();
        let dir = tempdir("toggles");
        let _h = override_home(&dir);
        *config_cache() = None;
        crate::config::write_providers(&json!([
            { "id": "g1", "app": "gemini", "kind": "custom", "name": "G",
              "baseUrl": "https://g.example", "apiKey": "k" },
            { "id": "c1", "app": "claude", "kind": "custom", "name": "C",
              "baseUrl": "https://c.example", "apiKey": "k" }
        ]))
        .unwrap();
        crate::config::write_config(&json!({ "sources": { "gemini": false } })).unwrap();
        let cfg = RouterConfig::default();
        let keys: Vec<String> = list_candidates(&cfg).into_iter().map(|c| c.key).collect();
        assert!(keys.contains(&"provider:c1".to_string()));
        assert!(
            !keys.contains(&"provider:g1".to_string()),
            "hidden: {keys:?}"
        );
        let err = resolve_upstream("provider:g1", Protocol::Gemini).unwrap_err();
        assert_eq!(err, TOOL_DISABLED);
        assert!(resolve_upstream("provider:c1", Protocol::Anthropic).is_ok());
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_validates_port_and_dedups_upstreams() {
        let cfg = RouterConfig::default();
        assert!(apply_patch(
            cfg.clone(),
            RouterConfigPatch {
                port: Some(0),
                ..Default::default()
            }
        )
        .is_err());
        let next = apply_patch(
            cfg,
            RouterConfigPatch {
                api_key: Some("  k  ".into()),
                upstreams: Some(vec![
                    UpstreamPref {
                        key: "a".into(),
                        enabled: true,
                    },
                    UpstreamPref {
                        key: "a".into(),
                        enabled: false,
                    },
                    UpstreamPref {
                        key: "".into(),
                        enabled: true,
                    },
                ]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(next.api_key, "k");
        assert_eq!(
            next.upstreams,
            vec![UpstreamPref {
                key: "a".into(),
                enabled: true
            }]
        );
    }

    #[test]
    fn uuid_v4_has_the_right_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "{u}");
        assert_ne!(u, uuid_v4());
    }

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert!(a.starts_with("sk-") && a.len() == 67);
        assert_ne!(a, b);
    }

    #[test]
    fn activating_a_router_binding_needs_the_router_running() {
        let _g = lock_home();
        let dir = tempdir("guard");
        let _h = override_home(&dir);
        crate::config::write_gateways(&json!([{
            "kind": "router", "id": "r-1", "name": "Termory Router", "baseUrl": "http://127.0.0.1:8317",
            "apiKey": "tm-x", "bindings": [{ "id": "pb-1", "app": "claude", "model": "m" }]
        }]))
        .unwrap();
        assert!(is_router_binding_id("pb-1"));
        assert!(!is_router_binding_id("other"));
        let synth = crate::providers::gateway_providers()
            .into_iter()
            .find(|p| p.id == "pb-1")
            .unwrap();
        assert!(!is_running());
        let err = crate::providers::activate(&synth, &[synth.clone()]).unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_router_gateway_upserts_and_keeps_bindings() {
        let _g = lock_home();
        let dir = tempdir("gwsync");
        let _h = override_home(&dir);
        *config_cache() = None;
        crate::config::write_gateways(&json!([{ "id": "gw1", "name": "GW", "bindings": [] }]))
            .unwrap();
        crate::config::write_providers(&json!([
            { "id": "p1", "app": "claude", "kind": "custom", "name": "P",
              "baseUrl": "https://x.example", "apiKey": "k", "model": "m-1" }
        ]))
        .unwrap();
        write_config(&RouterConfig {
            upstreams: vec![UpstreamPref {
                key: "provider:p1".into(),
                enabled: true,
            }],
            ..Default::default()
        })
        .unwrap();

        let entry = sync_router_gateway().unwrap();
        let cfg = read_config().unwrap();
        assert!(
            cfg.api_key.starts_with("sk-"),
            "a key is generated on first sync"
        );
        assert_eq!(entry["id"].as_str().unwrap().len(), 36, "an ordinary UUID");
        let router_id = entry["id"].as_str().unwrap().to_string();
        assert_eq!(entry["kind"], "router");
        // A provider write must not lose the router entry (it rides the
        // gateway paths), and a gateway write keeps its own kind.
        crate::config::write_providers(&json!([])).unwrap();
        let gws = crate::config::read_gateways().unwrap();
        assert!(gws
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == router_id.as_str() && g["kind"] == "router"));
        crate::config::write_gateways(&gws).unwrap();
        let gws = crate::config::read_gateways().unwrap();
        assert!(gws
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == router_id.as_str() && g["kind"] == "router"));
        assert!(gws
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == "gw1" && g["kind"] == "gateway"));
        // A stale frontend copy (old id / key) cannot clobber the backend-owned
        // fields; only its bindings are taken.
        let stale = json!([
            { "id": "gw1", "name": "GW", "bindings": [] },
            { "kind": "router", "id": "termory-router", "name": "old", "apiKey": "sk-old",
              "baseUrl": "http://127.0.0.1:1", "bindings": [{ "id": "b-new", "app": "claude" }] }
        ]);
        crate::config::write_gateways(&stale).unwrap();
        let gws = crate::config::read_gateways().unwrap();
        let r = gws
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["kind"] == "router")
            .unwrap();
        assert_eq!(r["id"], router_id.as_str());
        assert_eq!(r["apiKey"], JsonValue::from(cfg.api_key.clone()));
        assert_eq!(r["bindings"][0]["id"], "b-new");
        assert_eq!(entry["baseUrl"], "http://127.0.0.1:8317");
        assert_eq!(entry["apiKey"], JsonValue::from(cfg.api_key.clone()));
        assert_eq!(entry["capabilities"]["anthropic"], true);
        // An Anthropic-only pool still serves Responses and Chat clients,
        // through the translators into Anthropic.
        assert_eq!(entry["capabilities"]["openai"], true);
        assert_eq!(entry["capabilities"]["openaiCompatible"], true);
        assert_eq!(entry["capabilities"]["models"], json!(["m-1"]));
        let gws = crate::config::read_gateways().unwrap();
        assert_eq!(
            gws.as_array().unwrap().len(),
            2,
            "the user's gateway survives"
        );

        // A binding the user added survives a re-sync with a new port.
        let mut gws = gws.as_array().unwrap().clone();
        gws[1]["bindings"] = json!([{ "id": "b-claude", "app": "claude", "model": "m-1" }]);
        crate::config::write_gateways(&JsonValue::Array(gws)).unwrap();
        let mut cfg2 = cfg.clone();
        cfg2.port = 9001;
        write_config(&cfg2).unwrap();
        let entry = sync_router_gateway().unwrap();
        assert_eq!(entry["baseUrl"], "http://127.0.0.1:9001");
        assert_eq!(entry["bindings"][0]["id"], "b-claude");
        assert_eq!(
            crate::config::read_gateways()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // The router's own entry is never an upstream candidate, and a stale
        // pref pointing at it resolves to an error rather than a loop.
        let cands = gateway_candidates();
        assert!(cands
            .iter()
            .all(|c| c.key != format!("gateway:{router_id}")));
        assert!(cands.iter().any(|c| c.key == "gateway:gw1"));
        assert!(resolve_upstream(&format!("gateway:{router_id}"), Protocol::Anthropic).is_err());
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-shot HTTP upstream: reads one request (headers + body by
    /// content-length), records it, answers `status` with `body`.
    async fn mock_upstream(
        status: u16,
        body: &'static str,
    ) -> (u16, Arc<Mutex<Vec<(String, String)>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let (head_end, content_len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                            let mut len = 0usize;
                            for line in head.lines().skip(1) {
                                if let Some((k, v)) = line.split_once(':') {
                                    let k = k.trim().to_ascii_lowercase();
                                    let v = v.trim().to_string();
                                    if k == "content-length" {
                                        len = v.parse().unwrap_or(0);
                                    }
                                    seen.lock().unwrap().push((k, v));
                                }
                            }
                            let first = head.lines().next().unwrap_or("").to_string();
                            seen.lock().unwrap().push(("request-line".into(), first));
                            break (pos + 4, len);
                        }
                    };
                    while buf.len() < head_end + content_len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    // A real rate limit names its window; CLIProxyAPI cools for
                    // it (a bare 429 cools for only 1 s the first time).
                    let ra = if status == 429 {
                        "retry-after: 60\r\n"
                    } else {
                        ""
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\n{ra}content-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (port, seen)
    }

    #[tokio::test]
    async fn embeddings_are_never_translated_to_another_api() {
        let (port, seen) = mock_upstream(200, r#"{"id":"x"}"#).await;
        let _g = lock_home();
        let dir = tempdir("embeddings-native");
        let _h = override_home(&dir);
        *config_cache() = None;
        // An Anthropic-only member that lists the model.
        crate::config::write_providers(&json!([
            { "id": "p-anth", "app": "claude", "kind": "custom", "name": "Anth",
              "baseUrl": format!("http://127.0.0.1:{port}"), "apiKey": "k",
              "model": "m-emb" }
        ]))
        .unwrap();
        write_config(&RouterConfig {
            api_key: "local-key".into(),
            upstreams: vec![UpstreamPref {
                key: "provider:p-anth".into(),
                enabled: true,
            }],
            ..Default::default()
        })
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("authorization", "Bearer local-key")
            .body(Full::new(Bytes::from_static(
                br#"{"model":"m-emb","input":"hi"}"#,
            )))
            .unwrap();
        let resp = handle_test(req, test_ctx()).await;
        assert!(!resp.status().is_success());
        let posts = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, v)| k == "request-line" && v.starts_with("POST"))
            .count();
        assert_eq!(posts, 0, "not rewritten into an Anthropic request");
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_transport_failure_is_not_resent_in_later_rounds() {
        // An upstream that accepts and drops every connection: a transport
        // failure, which sets no cooldown.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                // Count the requests, not the model-list fetch made before
                // routing (`GET /v1/models`).
                let mut head = [0u8; 4];
                let _ = tokio::io::AsyncReadExt::read_exact(&mut sock, &mut head).await;
                if &head == b"POST" {
                    h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                drop(sock);
            }
        });
        let _g = lock_home();
        let dir = tempdir("transport-once");
        let _h = override_home(&dir);
        *config_cache() = None;
        crate::config::write_providers(&json!([
            { "id": "p-drop", "app": "claude", "kind": "custom", "name": "Drop",
              "baseUrl": format!("http://127.0.0.1:{port}"), "apiKey": "k",
              "model": "m-drop" }
        ]))
        .unwrap();
        write_config(&RouterConfig {
            api_key: "local-key".into(),
            upstreams: vec![UpstreamPref {
                key: "provider:p-drop".into(),
                enabled: true,
            }],
            ..Default::default()
        })
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", "local-key")
            .body(Full::new(Bytes::from_static(br#"{"model":"m-drop"}"#)))
            .unwrap();
        let resp = handle_test(req, test_ctx()).await;
        assert!(!resp.status().is_success());
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one send, no resend in a retry round"
        );
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn handle_test(req: Request<Full<Bytes>>, ctx: Arc<Ctx>) -> Response<OutBody> {
        handle_inner(req, ctx).await
    }

    fn test_ctx() -> Arc<Ctx> {
        Arc::new(Ctx {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            app: None,
        })
    }

    async fn body_text(resp: Response<OutBody>) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn failover_skips_a_rate_limited_upstream_and_streams_the_next() {
        let (bad_port, bad_seen) = mock_upstream(429, r#"{"error":"slow down"}"#).await;
        let (good_port, good_seen) = mock_upstream(200, "event: message_start\ndata: {}\n\n").await;

        // HOME-scoped providers.json + router.json for this test only.
        let _g = lock_home();
        let dir = tempdir("failover");
        let _h = override_home(&dir);
        crate::config::write_providers(&json!([
            { "id": "p-bad", "app": "claude", "kind": "custom", "name": "Bad",
              "baseUrl": format!("http://127.0.0.1:{bad_port}"), "apiKey": "key-bad" },
            { "id": "p-good", "app": "claude", "kind": "custom", "name": "Good",
              "baseUrl": format!("http://127.0.0.1:{good_port}"), "apiKey": "key-good" }
        ]))
        .unwrap();
        write_config(&RouterConfig {
            api_key: "local-key".into(),
            upstreams: vec![
                UpstreamPref {
                    key: "provider:p-bad".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "provider:p-good".into(),
                    enabled: true,
                },
            ],
            ..Default::default()
        })
        .unwrap();
        reset_health("provider:p-bad");
        reset_health("provider:p-good");
        // Their model listings count as fetched, so the request path does not
        // query the mocks' `/models` (that would add requests the header
        // assertions below would see).
        for k in ["provider:p-bad", "provider:p-good"] {
            catalog_tried().insert(k.to_string(), std::time::Instant::now());
        }

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages?beta=true")
            .header("authorization", "Bearer local-key")
            .header("anthropic-version", "2023-06-01")
            .header("accept-encoding", "gzip")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from_static(br#"{"model":"claude"}"#)))
            .unwrap();
        // `handle_inner` takes hyper's Incoming; go through the same body
        // collection path with a Full body via a tiny adapter.
        let resp = handle_test(req, test_ctx()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(body_text(resp).await, "event: message_start\ndata: {}\n\n");

        // The bad one was tried first (list order), failed over, and sits in cooldown.
        let bad = bad_seen.lock().unwrap().clone();
        assert!(bad
            .iter()
            .any(|(k, v)| k == "request-line" && v == "POST /v1/messages?beta=true HTTP/1.1"));
        assert!(in_cooldown("provider:p-bad"));
        let h = health_table().get("provider:p-bad").cloned().unwrap();
        assert_eq!(h.last_error.as_deref(), Some("HTTP 429"));

        // The good one got the upstream's OWN key in both header forms, the
        // client's headers minus the dropped set, and the body.
        let good = good_seen.lock().unwrap().clone();
        let get = |k: &str| {
            good.iter()
                .filter(|(kk, _)| kk == k)
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("authorization"), vec!["Bearer key-good"]);
        assert_eq!(get("x-api-key"), vec!["key-good"]);
        assert_eq!(get("anthropic-version"), vec!["2023-06-01"]);
        assert!(
            get("accept-encoding").is_empty(),
            "compression negotiation is not forwarded"
        );
        assert_eq!(get("content-length"), vec!["18"]);
        assert!(!in_cooldown("provider:p-good"));

        // A second request FOR THE SAME MODEL goes straight to the good one
        // (bad is cooling down for that model — cooldowns are per model).
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", "local-key")
            .body(Full::new(Bytes::from_static(br#"{"model":"claude"}"#)))
            .unwrap();
        let resp = handle_test(req, test_ctx()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            bad_seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k == "request-line")
                .count(),
            1
        );

        // Wrong client key → 401 before anything is sent upstream.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", "nope")
            .body(Full::new(Bytes::from_static(b"{}")))
            .unwrap();
        let resp = handle_test(req, test_ctx()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // A Chat client reaches these Anthropic-only members through the
        // Chat→Anthropic translator (CLIProxyAPI `claude/openai/chat-completions`)
        // — it is no longer "no upstream speaks this API".
        let before = bad_seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == "request-line")
            .count();
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer local-key")
            .body(Full::new(Bytes::from_static(br#"{"model":"other"}"#)))
            .unwrap();
        let resp = handle_test(req, test_ctx()).await;
        assert_ne!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let after = bad_seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == "request-line")
            .count();
        assert_eq!(
            after,
            before + 1,
            "the Anthropic member was sent the translated request"
        );

        health_table().remove("provider:p-bad");
        health_table().remove("provider:p-good");
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn every_upstream_failing_returns_the_last_upstream_error() {
        let (a, _) = mock_upstream(503, "a down").await;
        let (b, _) = mock_upstream(500, "b down").await;
        let _g = lock_home();
        let dir = tempdir("allfail");
        let _h = override_home(&dir);
        crate::config::write_providers(&json!([
            { "id": "q-a", "app": "codex", "kind": "custom", "name": "A",
              "baseUrl": format!("http://127.0.0.1:{a}/v1"), "apiKey": "ka" },
            { "id": "q-b", "app": "codex", "kind": "custom", "name": "B",
              "baseUrl": format!("http://127.0.0.1:{b}/v1"), "apiKey": "kb" }
        ]))
        .unwrap();
        write_config(&RouterConfig {
            upstreams: vec![
                UpstreamPref {
                    key: "provider:q-a".into(),
                    enabled: true,
                },
                UpstreamPref {
                    key: "provider:q-b".into(),
                    enabled: true,
                },
            ],
            api_key: "local-key".into(),
            ..Default::default()
        })
        .unwrap();
        reset_health("provider:q-a");
        reset_health("provider:q-b");
        let req = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", "Bearer local-key")
            .body(Full::new(Bytes::from_static(b"{}")))
            .unwrap();
        let resp = handle_test(req, test_ctx()).await;
        assert_eq!(
            resp.status().as_u16(),
            500,
            "the LAST upstream's own error is relayed"
        );
        assert_eq!(body_text(resp).await, "b down");
        assert!(in_cooldown("provider:q-a") && in_cooldown("provider:q-b"));
        // Each failed upstream was sent the request exactly ONCE — pass 1
        // never re-sends what pass 0 already tried.
        for k in ["provider:q-a", "provider:q-b"] {
            let h = health_table().get(k).cloned().unwrap();
            assert_eq!((h.requests, h.failures), (1, 1), "{k}");
        }
        health_table().remove("provider:q-a");
        health_table().remove("provider:q-b");
        *config_cache() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fail(status: Option<u16>, ra: Option<u64>, text: &str) -> Failure {
        Failure {
            status,
            retry_after: ra.map(Duration::from_secs),
            text: text.to_string(),
            transport: false,
            credential_scoped: false,
            request_scoped: false,
        }
    }

    fn cooled_secs(key: &str, model: &str) -> u64 {
        let (until, _) = model_blocked(key, model).expect("blocked");
        (until - now_millis() + 500) / 1000
    }

    /// CLIProxyAPI `MarkResult`: state is per (member, MODEL); the status
    /// table decides the duration; success resets that model only.
    #[test]
    fn failures_cool_one_model_on_one_member_per_cliproxyapi_table() {
        let key = format!("test:{}", now_millis());
        mark_model_failure(&key, "m1", &fail(Some(401), None, ""));
        assert_eq!(cooled_secs(&key, "m1"), 1800);
        assert!(
            model_blocked(&key, "m2").is_none(),
            "other models stay usable"
        );
        mark_model_failure(&key, "m2", &fail(Some(503), None, ""));
        assert_eq!(cooled_secs(&key, "m2"), 60);
        mark_model_failure(&key, "m3", &fail(Some(404), None, ""));
        assert_eq!(cooled_secs(&key, "m3"), 12 * 3600);
        mark_model_failure(
            &key,
            "m4",
            &fail(Some(400), None, r#"{"error":{"code":"model_not_found"}}"#),
        );
        assert_eq!(cooled_secs(&key, "m4"), 12 * 3600);
        // 429: Retry-After (floored at 10 s), else 1 s doubling.
        mark_model_failure(&key, "q1", &fail(Some(429), Some(3), ""));
        assert_eq!(cooled_secs(&key, "q1"), 10);
        assert!(
            model_blocked(&key, "q1").unwrap().1,
            "a 429 is a quota block"
        );
        mark_model_failure(&key, "q2", &fail(Some(429), None, ""));
        assert_eq!(cooled_secs(&key, "q2"), 1);
        // A transport error never cools.
        mark_model_failure(
            &key,
            "t",
            &Failure {
                status: None,
                retry_after: None,
                text: "connect failed".into(),
                transport: true,
                credential_scoped: false,
                request_scoped: false,
            },
        );
        assert!(model_blocked(&key, "t").is_none());
        // Success resets that model's state.
        mark_model_success(&key, "m1");
        assert!(model_blocked(&key, "m1").is_none());
        assert!(model_blocked(&key, "m2").is_some());
        model_states().retain(|(k, _), _| k != &key);
        health_table().remove(&key);
    }

    /// `isRequestInvalidError`: the request's own fault goes straight back.
    #[test]
    fn request_faults_are_classified_like_cliproxyapi() {
        assert!(is_request_invalid(
            400,
            r#"{"error":{"type":"invalid_request_error"}}"#
        ));
        assert!(is_request_invalid(413, "too big"));
        assert!(is_request_invalid(
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 8k"}}"#
        ));
        assert!(!is_request_invalid(429, "slow down"));
        assert!(!is_request_invalid(402, "pay"));
        assert!(!is_request_invalid(
            400,
            r#"{"error":{"code":"model_not_found"}}"#
        ));
        assert!(!is_request_invalid(400, "The model gpt-x is not supported"));
        assert!(!is_request_invalid(
            401,
            r#"{"error":{"type":"authentication_error"}}"#
        ));
        assert!(!is_request_invalid(400, "invalid_grant"));
        assert!(!is_request_invalid(503, "down"));
        assert!(is_retry_round_status(429) && is_retry_round_status(503));
        assert!(!is_retry_round_status(401) && !is_retry_round_status(404));
    }
}
