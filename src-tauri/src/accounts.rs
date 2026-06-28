//! Multi-account management for a CLI's **official** OAuth login.
//!
//! Unlike provider switching (which points a CLI at a third-party API and
//! never touches the native OAuth credential), this feature snapshots the
//! credential itself so a user can keep several official accounts and swap
//! between them. It is therefore a deliberate, user-triggered exception to
//! the "never write the native credential" invariant — only ever invoked
//! from an explicit button.
//!
//! ## Phase 1 — Codex (file-based)
//!
//! Codex's default credential store is the `~/.codex/auth.json` FILE
//! (verified: `config_defaults_to_file_cli_auth_store_mode`,
//! codex-rs `core/src/config/config_tests.rs:5105` → `File` mode →
//! `create_auth_storage` returns a pure `FileAuthStorage`,
//! `login/src/auth/storage.rs:514`). So reading / writing that one file is
//! the complete, correct switch on every OS — no Keychain involved. The
//! only exception is a user who explicitly set
//! `cli_auth_credentials_store = "keyring"` (or `"auto"`) in
//! `config.toml`; we detect that and surface a warning (Keychain writes
//! are a later phase).
//!
//! A saved account stores the **full `auth.json` verbatim** (the OAuth
//! `tokens` block — id/access/refresh — plus `last_refresh` / `account_id`
//! must travel together for Codex's refresh to keep working), labelled by
//! the email + plan decoded from the `tokens.id_token` JWT.
//!
//! Claude (which on macOS keeps its credential in the Keychain, not a
//! file) is a later phase; the `app` field on each stored account leaves
//! room for it.

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::error::Error;

use crate::providers::{codex_auth_path, codex_root, CliApp};

// ===================================================================
// Wire types (camelCase → frontend)
// ===================================================================

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsState {
    /// The account currently logged into the live CLI config, if any.
    current: Option<CurrentAccount>,
    /// Saved snapshots for this app (no token payload is ever sent out).
    accounts: Vec<SavedAccountView>,
    /// Set to the configured store mode (`"keyring"` / `"auto"`) when the
    /// CLI is NOT using the file store this feature writes — a hint that a
    /// file switch may not take effect. `None` = file store (the default).
    storage_warning: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentAccount {
    /// Display name from the id_token `name` claim, if present.
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// RFC 3339 timestamp from `last_refresh` in auth.json.
    last_refresh: Option<String>,
    /// Whether the live login is already captured in `accounts` — when
    /// false the UI can warn that switching away would lose it.
    saved: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedAccountView {
    id: String,
    label: String,
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    saved_at: String,
    /// RFC 3339 timestamp from `last_refresh` inside the saved payload.
    last_refresh: Option<String>,
    /// True when this snapshot matches the live login.
    active: bool,
}

// ===================================================================
// IPC entry points
// ===================================================================

/// List the live + saved official accounts for `app`. Phase 1 implements
/// Codex; other apps return an empty state so the UI degrades cleanly.
pub fn list_accounts(app: CliApp) -> Result<AccountsState, Box<dyn Error>> {
    match app {
        CliApp::Codex => list_codex_accounts(),
        _ => Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        }),
    }
}

/// Snapshot the CLI's current official login into the store. Upserts by
/// account fingerprint, so re-saving the same account refreshes its token
/// payload (and label, when one is supplied). `label` defaults to the
/// account email.
pub fn save_current_account(app: CliApp, label: Option<String>) -> Result<(), Box<dyn Error>> {
    match app {
        CliApp::Codex => save_codex_account(label),
        _ => Err(unsupported(app)),
    }
}

/// Result of `refresh_codex_tokens` — always `Ok`; failures land in
/// `warning` so the frontend can show a non-blocking hint.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRefreshResult {
    pub refreshed: bool,
    pub warning: Option<String>,
}

/// Attempt to refresh the Codex access/id tokens using the saved
/// refresh_token. Called by the frontend immediately after `switch_account`
/// to avoid the 401 that comes from restoring expired tokens.
pub async fn refresh_codex_tokens() -> TokenRefreshResult {
    let path = match codex_auth_path() {
        Ok(p) => p,
        Err(e) => {
            return TokenRefreshResult {
                refreshed: false,
                warning: Some(format!("Cannot locate auth.json: {e}")),
            }
        }
    };
    match try_refresh_codex_tokens(&path).await {
        Ok(()) => TokenRefreshResult { refreshed: true, warning: None },
        Err(e) => TokenRefreshResult {
            refreshed: false,
            warning: Some(e.to_string()),
        },
    }
}

/// Restore a saved snapshot into the live CLI credential.
pub fn switch_account(id: String) -> Result<(), Box<dyn Error>> {
    let store = read_store()?;
    let entry = store
        .iter()
        .find(|e| str_field(e, "id") == Some(id.as_str()))
        .ok_or("Account not found")?;
    let payload = str_field(entry, "payload").ok_or("Saved account has no credential payload")?;
    match str_field(entry, "app") {
        Some("codex") => switch_codex(payload),
        other => Err(format!("Unsupported account app: {}", other.unwrap_or("?")).into()),
    }
}

/// Rename a saved snapshot (live credential untouched).
pub fn rename_account(id: String, label: String) -> Result<(), Box<dyn Error>> {
    let mut store = read_store()?;
    let slot = store
        .iter_mut()
        .find(|e| str_field(e, "id") == Some(id.as_str()))
        .ok_or("Account not found")?;
    if let JsonValue::Object(o) = slot {
        o.insert("label".into(), JsonValue::String(label));
    }
    write_store(store)
}

/// Spawn `codex login`, wait for completion, then save the resulting
/// credential into the accounts store. Returns the new account's id.
///
/// Auth.json is cleared to `{}` **before** spawning so codex's own
/// `clear_existing_auth_before_login → logout_with_revoke` finds nothing to
/// revoke on the server, keeping any previously-saved accounts' tokens valid.
/// If the current live login is not yet in the store it is auto-saved first
/// so it can be restored afterwards.
///
/// On any failure the original auth.json is put back (rollback).
pub async fn login_and_save_codex_account() -> Result<String, String> {
    let auth_path = codex_auth_path().map_err(|e| e.to_string())?;
    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // If there is a live Codex login that is not yet saved, auto-save it so
    // we can restore it after the new login completes.
    let prev_active_id: Option<String> = auto_save_unsaved_live_codex_account()
        .map_err(|e| e.to_string())?;

    // Snapshot original auth.json for rollback.
    let original_auth: Option<Vec<u8>> = std::fs::read(&auth_path).ok();

    // Clear auth.json — codex's logout_with_revoke will find nothing to revoke.
    atomic_write_0600(&auth_path, b"{}").map_err(|e| e.to_string())?;

    // Spawn `codex login` (non-interactive; the browser handles the UI).
    let mut child = match tokio::process::Command::new("codex")
        .arg("login")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            restore_auth(&auth_path, original_auth.as_deref());
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "codex is not installed or not in PATH".to_string()
            } else {
                format!("Failed to launch codex: {e}")
            };
            return Err(msg);
        }
    };

    let mut stderr_pipe = child.stderr.take();

    // Wait up to 5 minutes for the browser login to complete.
    let status = tokio::select! {
        r = child.wait() => match r {
            Ok(s) => s,
            Err(e) => {
                restore_auth(&auth_path, original_auth.as_deref());
                return Err(format!("codex login error: {e}"));
            }
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            let _ = child.kill().await;
            restore_auth(&auth_path, original_auth.as_deref());
            return Err("codex login timed out after 5 minutes".into());
        }
    };

    if !status.success() {
        let stderr_msg = if let Some(ref mut se) = stderr_pipe {
            use tokio::io::AsyncReadExt as _;
            let mut buf = String::new();
            let _ = se.read_to_string(&mut buf).await;
            buf
        } else {
            String::new()
        };
        restore_auth(&auth_path, original_auth.as_deref());
        return Err(format!(
            "codex login failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_msg.trim()
        ));
    }

    // Save the new account (reads the fresh auth.json codex just wrote).
    if let Err(e) = save_codex_account(None) {
        restore_auth(&auth_path, original_auth.as_deref());
        return Err(format!("Login succeeded but could not save account: {e}"));
    }

    // Capture the new account's id from the live fingerprint before we restore.
    let new_id = read_codex_live()
        .ok()
        .flatten()
        .and_then(|live| {
            let fp = live.fingerprint;
            read_store().ok()?.into_iter().find(|e| {
                str_field(e, "app") == Some("codex")
                    && str_field(e, "fingerprint") == Some(fp.as_str())
            })
            .and_then(|e| str_field(&e, "id").map(String::from))
        })
        .unwrap_or_default();

    // Restore the previously active account (writes its snapshot back to auth.json).
    if let Some(prev_id) = prev_active_id {
        let _ = switch_account(prev_id);
    }

    Ok(new_id)
}

/// If there is a live Codex login that is not yet recorded in the store,
/// auto-save it and return its store id. Returns `None` when the live login
/// is already saved or there is no live login.
fn auto_save_unsaved_live_codex_account() -> Result<Option<String>, Box<dyn Error>> {
    let Some(live) = read_codex_live()? else {
        return Ok(None);
    };
    let mut store = read_store()?;
    let fp = live.fingerprint.as_str();
    if store.iter().any(|e| {
        str_field(e, "app") == Some("codex") && str_field(e, "fingerprint") == Some(fp)
    }) {
        // Already saved — just return its id.
        return Ok(store
            .iter()
            .find(|e| str_field(e, "fingerprint") == Some(fp))
            .and_then(|e| str_field(e, "id"))
            .map(String::from));
    }
    let id = format!("codex:{fp}");
    let label = live
        .name
        .clone()
        .or_else(|| live.email.clone())
        .unwrap_or_else(|| "Codex account".to_string());
    let entry = serde_json::json!({
        "id": id, "app": "codex", "label": label,
        "name": live.name, "email": live.email, "plan": live.plan,
        "fingerprint": fp, "payload": live.raw, "savedAt": now_rfc3339(),
    });
    store.push(entry);
    write_store(store)?;
    Ok(Some(id))
}

/// Restore auth.json from a snapshot, or remove it if there was none.
fn restore_auth(path: &std::path::Path, original: Option<&[u8]>) {
    match original {
        Some(bytes) => { let _ = atomic_write_0600(path, bytes); }
        None => { let _ = std::fs::remove_file(path); }
    }
}

/// Delete a saved snapshot. NEVER touches the live credential.
pub fn delete_account(id: String) -> Result<(), Box<dyn Error>> {
    let mut store = read_store()?;
    let before = store.len();
    store.retain(|e| str_field(e, "id") != Some(id.as_str()));
    if store.len() == before {
        return Err("Account not found".into());
    }
    write_store(store)
}

// ===================================================================
// Store (~/.termory/accounts.json) helpers
// ===================================================================

fn read_store() -> Result<Vec<JsonValue>, Box<dyn Error>> {
    match crate::config::read_accounts()? {
        JsonValue::Array(a) => Ok(a),
        _ => Ok(Vec::new()),
    }
}

fn write_store(entries: Vec<JsonValue>) -> Result<(), Box<dyn Error>> {
    crate::config::write_accounts(&JsonValue::Array(entries))
}

fn str_field<'a>(v: &'a JsonValue, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn unsupported(app: CliApp) -> Box<dyn Error> {
    format!("Account management is not implemented for {app:?} yet").into()
}

// ===================================================================
// Codex
// ===================================================================

/// Parsed view of the live `~/.codex/auth.json`.
struct CodexLive {
    /// Stable per-account identity (chatgpt_user_id → account_id → email
    /// → content hash) used to upsert / match the active snapshot.
    fingerprint: String,
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// RFC 3339 timestamp from the `last_refresh` field in auth.json.
    last_refresh: Option<String>,
    /// Full auth.json content, stored verbatim as the snapshot payload.
    raw: String,
}

fn read_codex_live() -> Result<Option<CodexLive>, Box<dyn Error>> {
    let path = codex_auth_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    let doc: JsonValue = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse ~/.codex/auth.json: {e}"))?;

    // No `tokens` block = no ChatGPT login (pure API-key login or logged
    // out) — nothing to save. Mirrors quota.rs `parse_codex_credentials`.
    let Some(tokens) = doc.get("tokens").filter(|t| !t.is_null()) else {
        return Ok(None);
    };

    let id_info = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .and_then(|jwt| parse_codex_id_token(jwt).ok());
    let account_id = tokens.get("account_id").and_then(|v| v.as_str());

    let name = id_info.as_ref().and_then(|i| i.name.clone());
    let email = id_info.as_ref().and_then(|i| i.email.clone());
    let plan = id_info.as_ref().and_then(|i| i.plan.clone());
    let user_id = id_info.as_ref().and_then(|i| i.user_id.clone());

    let fingerprint = user_id
        .or_else(|| account_id.map(String::from))
        .or_else(|| email.clone())
        .unwrap_or_else(|| format!("hash:{}", stable_hash(&raw)));

    let last_refresh = doc.get("last_refresh").and_then(|v| v.as_str()).map(String::from);

    Ok(Some(CodexLive {
        fingerprint,
        name,
        email,
        plan,
        last_refresh,
        raw,
    }))
}

fn list_codex_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let live = read_codex_live()?;
    let current_fp = live.as_ref().map(|l| l.fingerprint.clone());
    let store = read_store()?;

    let mut accounts = Vec::new();
    let mut current_saved = false;
    for e in &store {
        if str_field(e, "app") != Some("codex") {
            continue;
        }
        let fp = str_field(e, "fingerprint");
        let active = current_fp.is_some() && current_fp.as_deref() == fp;
        if active {
            current_saved = true;
        }
        let last_refresh = str_field(e, "payload")
            .and_then(|p| serde_json::from_str::<JsonValue>(p).ok())
            .and_then(|doc| doc.get("last_refresh").and_then(|v| v.as_str()).map(String::from));
        accounts.push(SavedAccountView {
            id: str_field(e, "id").unwrap_or_default().to_string(),
            label: str_field(e, "label").unwrap_or_default().to_string(),
            name: str_field(e, "name").map(String::from),
            email: str_field(e, "email").map(String::from),
            plan: str_field(e, "plan").map(String::from),
            saved_at: str_field(e, "savedAt").unwrap_or_default().to_string(),
            last_refresh,
            active,
        });
    }

    let current = live.map(|l| CurrentAccount {
        name: l.name,
        email: l.email,
        plan: l.plan,
        last_refresh: l.last_refresh,
        saved: current_saved,
    });

    Ok(AccountsState {
        current,
        accounts,
        storage_warning: codex_storage_warning(),
    })
}

fn save_codex_account(label: Option<String>) -> Result<(), Box<dyn Error>> {
    let live = read_codex_live()?
        .ok_or("No Codex ChatGPT login found to save (run `codex login` first)")?;

    let id = format!("codex:{}", live.fingerprint);
    let mut store = read_store()?;

    // Keep the user's existing custom label when re-saving (token refresh)
    // unless they explicitly passed a new one.
    let existing_label = store
        .iter()
        .find(|e| str_field(e, "id") == Some(id.as_str()))
        .and_then(|e| str_field(e, "label"))
        .map(String::from);
    let label = label
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or(existing_label)
        .or_else(|| live.name.clone())
        .or_else(|| live.email.clone())
        .unwrap_or_else(|| "Codex account".to_string());

    let entry = json!({
        "id": id,
        "app": "codex",
        "label": label,
        "name": live.name,
        "email": live.email,
        "plan": live.plan,
        "fingerprint": live.fingerprint,
        "payload": live.raw,
        "savedAt": now_rfc3339(),
    });

    match store
        .iter_mut()
        .find(|e| str_field(e, "id") == Some(id.as_str()))
    {
        Some(slot) => *slot = entry,
        None => store.push(entry),
    }
    write_store(store)
}

/// POST the saved refresh_token to `https://auth.openai.com/oauth/token`,
/// merge the new id/access/refresh tokens back into auth.json, and update
/// `last_refresh`. Endpoint + client_id from codex-rs
/// `login/src/auth/manager.rs:186,CLIENT_ID`.
async fn try_refresh_codex_tokens(path: &std::path::Path) -> Result<(), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut doc: JsonValue = serde_json::from_str(&raw).map_err(|e| e.to_string())?;

    let refresh_token = doc
        .pointer("/tokens/refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "No refresh_token in saved credential".to_string())?
        .to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .post("https://auth.openai.com/oauth/token")
        .json(&json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<JsonValue>(&body)
            .ok()
            .and_then(|v| {
                v.get("error_description")
                    .or_else(|| v.get("message"))
                    .and_then(|m| m.as_str())
                    .map(String::from)
            })
            .unwrap_or(body);
        return Err(format!("Token refresh failed ({status}): {msg}"));
    }

    let body: JsonValue = resp.json().await.map_err(|e| e.to_string())?;

    // Merge new tokens into the existing doc so all other fields survive.
    if let Some(tokens) = doc.get_mut("tokens").and_then(|v| v.as_object_mut()) {
        for key in ["id_token", "access_token", "refresh_token"] {
            if let Some(v) = body.get(key).filter(|v| v.is_string()) {
                tokens.insert(key.into(), v.clone());
            }
        }
    }
    doc["last_refresh"] = JsonValue::String(chrono::Utc::now().to_rfc3339());

    let updated = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    atomic_write_0600(path, updated.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

fn switch_codex(payload: &str) -> Result<(), Box<dyn Error>> {
    // Validate the snapshot still parses before clobbering the live file —
    // never overwrite a good login with garbage.
    let _: JsonValue = serde_json::from_str(payload)
        .map_err(|e| format!("Saved Codex credential is corrupt: {e}"))?;
    let path = codex_auth_path()?;
    // Single atomic temp+rename: on any failure the original auth.json is
    // left intact, so no separate rollback snapshot is needed.
    atomic_write_0600(&path, payload.as_bytes())
}

/// Decode the useful claims from a Codex `id_token` JWT (no signature
/// verification — display only). Claim layout per codex-rs
/// `login/src/token_data.rs:73-160`.
struct CodexIdInfo {
    /// The OIDC `name` claim — the account's display name.
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    user_id: Option<String>,
}

fn parse_codex_id_token(jwt: &str) -> Result<CodexIdInfo, Box<dyn Error>> {
    let payload_b64 = jwt.split('.').nth(1).ok_or("invalid JWT: no payload")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)?;
    let claims: JsonValue = serde_json::from_slice(&bytes)?;

    let auth = claims.get("https://api.openai.com/auth");
    let profile = claims.get("https://api.openai.com/profile");

    let name = claims
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .or_else(|| {
            profile
                .and_then(|p| p.get("email"))
                .and_then(|v| v.as_str())
        })
        .map(String::from);

    let plan = auth
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str())
        .map(title_case_plan);

    let user_id = auth
        .and_then(|a| a.get("chatgpt_user_id").or_else(|| a.get("user_id")))
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(CodexIdInfo {
        name,
        email,
        plan,
        user_id,
    })
}

/// `"pro"` → `"Pro"`. Codex's `PlanType::display_name` title-cases the
/// known plans; a plain capitalize is close enough for a label.
fn title_case_plan(raw: &str) -> String {
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Return the configured `cli_auth_credentials_store` mode when it is NOT
/// the default file store (i.e. `"keyring"` / `"auto"`), so the UI can
/// warn that a file-only switch may not take effect. Default (`"file"` /
/// unset / unreadable) → `None`. Reads the same `CODEX_HOME`-aware path
/// that the credential writers use.
fn codex_storage_warning() -> Option<String> {
    let home = dirs::home_dir()?;
    let text = std::fs::read_to_string(codex_root(&home).join("config.toml")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("cli_auth_credentials_store") else {
            continue;
        };
        let val = rest
            .trim_start()
            .strip_prefix('=')
            .unwrap_or(rest)
            .trim()
            .trim_matches('"')
            .to_ascii_lowercase();
        if val == "keyring" || val == "auto" {
            return Some(val);
        }
        return None;
    }
    None
}

// ===================================================================
// Small utilities
// ===================================================================

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn stable_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Atomic temp+rename write with mode 0600 on Unix — credentials must not
/// land world-readable. Mirrors `config::write_json_atomic_0600` but for
/// raw bytes (the verbatim auth.json payload).
fn atomic_write_0600(path: &std::path::Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.file_name().ok_or("invalid path")?.to_owned();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    tmp_name.push(format!(".tmp.{nanos}"));
    let tmp_path = path.with_file_name(tmp_name);

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{EnvVarGuard, HOME_LOCK};
    use std::path::{Path, PathBuf};

    fn tempdir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("termory-accounts-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a fake (unsigned) JWT carrying the given payload — mirrors
    /// codex-rs `token_data_tests::fake_jwt`.
    fn fake_jwt(payload: JsonValue) -> String {
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = b64(br#"{"alg":"none","typ":"JWT"}"#);
        let body = b64(&serde_json::to_vec(&payload).unwrap());
        let sig = b64(b"sig");
        format!("{header}.{body}.{sig}")
    }

    fn write_codex_auth(home: &Path, email: &str, plan: &str, account_id: &str) {
        let jwt = fake_jwt(json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": plan,
                "chatgpt_user_id": format!("user-{email}"),
            },
        }));
        let doc = json!({
            "tokens": {
                "id_token": jwt,
                "access_token": "access-xyz",
                "refresh_token": "refresh-xyz",
                "account_id": account_id,
            },
            "last_refresh": "2026-06-27T00:00:00Z",
        });
        let dir = home.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn parse_id_token_extracts_name_email_plan_and_user() {
        let jwt = fake_jwt(json!({
            "name": "Jane Doe",
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro",
                "chatgpt_user_id": "u-123",
            },
        }));
        let info = parse_codex_id_token(&jwt).unwrap();
        assert_eq!(info.name.as_deref(), Some("Jane Doe"));
        assert_eq!(info.email.as_deref(), Some("user@example.com"));
        assert_eq!(info.plan.as_deref(), Some("Pro"));
        assert_eq!(info.user_id.as_deref(), Some("u-123"));
    }

    #[test]
    fn save_defaults_label_to_name_and_surfaces_it() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("name-label");
        let _h = EnvVarGuard::set("HOME", &tmp);

        // auth.json whose id_token carries a display `name`.
        let jwt = fake_jwt(json!({
            "name": "Jane Doe",
            "email": "jane@example.com",
            "https://api.openai.com/auth": { "chatgpt_user_id": "u-jane" },
        }));
        let doc =
            json!({ "tokens": { "id_token": jwt, "access_token": "ax", "account_id": "acct" } });
        let dir = tmp.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), serde_json::to_string(&doc).unwrap()).unwrap();

        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(
            state.current.as_ref().unwrap().name.as_deref(),
            Some("Jane Doe")
        );

        // Default label prefers the name over the email.
        save_current_account(CliApp::Codex, None).unwrap();
        let saved = &list_accounts(CliApp::Codex).unwrap().accounts[0];
        assert_eq!(saved.label, "Jane Doe");
        assert_eq!(saved.name.as_deref(), Some("Jane Doe"));
        assert_eq!(saved.email.as_deref(), Some("jane@example.com"));
    }

    #[test]
    fn parse_id_token_reads_profile_email_fallback() {
        let jwt = fake_jwt(json!({
            "https://api.openai.com/profile": { "email": "p@example.com" },
        }));
        let info = parse_codex_id_token(&jwt).unwrap();
        assert_eq!(info.email.as_deref(), Some("p@example.com"));
    }

    #[test]
    fn list_is_empty_when_no_codex_login() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("no-login");
        let _h = EnvVarGuard::set("HOME", &tmp);
        let state = list_accounts(CliApp::Codex).unwrap();
        assert!(state.current.is_none());
        assert!(state.accounts.is_empty());
    }

    #[test]
    fn save_then_switch_roundtrip_restores_exact_auth_json() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("roundtrip");
        let _h = EnvVarGuard::set("HOME", &tmp);

        // Account A logged in → save it.
        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        let a_bytes = std::fs::read(tmp.join(".codex/auth.json")).unwrap();
        save_current_account(CliApp::Codex, None).unwrap();

        // Account A shows current + saved + active, label defaulted to email.
        let state = list_accounts(CliApp::Codex).unwrap();
        let cur = state.current.as_ref().unwrap();
        assert_eq!(cur.email.as_deref(), Some("a@example.com"));
        assert_eq!(cur.plan.as_deref(), Some("Pro"));
        assert!(cur.saved);
        assert_eq!(state.accounts.len(), 1);
        assert_eq!(state.accounts[0].label, "a@example.com");
        assert!(state.accounts[0].active);
        let a_id = state.accounts[0].id.clone();

        // Account B logs in (different file) and is saved too.
        write_codex_auth(&tmp, "b@example.com", "plus", "acct-b");
        save_current_account(CliApp::Codex, Some("Work".into())).unwrap();
        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(state.accounts.len(), 2);
        // Now A is no longer active (B is live).
        let a_view = state.accounts.iter().find(|x| x.id == a_id).unwrap();
        assert!(!a_view.active);

        // Switch back to A → live auth.json is byte-identical to A's snapshot.
        switch_account(a_id.clone()).unwrap();
        let live_now = std::fs::read(tmp.join(".codex/auth.json")).unwrap();
        assert_eq!(live_now, a_bytes, "switch must restore A's exact auth.json");

        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(
            state.current.as_ref().unwrap().email.as_deref(),
            Some("a@example.com")
        );
        assert!(state.accounts.iter().find(|x| x.id == a_id).unwrap().active);
    }

    #[test]
    fn resave_same_account_upserts_and_keeps_custom_label() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("upsert");
        let _h = EnvVarGuard::set("HOME", &tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex, Some("Personal".into())).unwrap();
        // Re-save with no label (e.g. token refresh) keeps "Personal".
        save_current_account(CliApp::Codex, None).unwrap();

        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(state.accounts.len(), 1, "must upsert, not duplicate");
        assert_eq!(state.accounts[0].label, "Personal");
    }

    #[test]
    fn rename_and_delete_only_touch_the_store() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("rename-delete");
        let _h = EnvVarGuard::set("HOME", &tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex, None).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();

        rename_account(id.clone(), "Renamed".into()).unwrap();
        assert_eq!(
            list_accounts(CliApp::Codex).unwrap().accounts[0].label,
            "Renamed"
        );

        // Delete leaves the live auth.json intact.
        let live_before = std::fs::read(tmp.join(".codex/auth.json")).unwrap();
        delete_account(id).unwrap();
        assert!(list_accounts(CliApp::Codex).unwrap().accounts.is_empty());
        let live_after = std::fs::read(tmp.join(".codex/auth.json")).unwrap();
        assert_eq!(
            live_before, live_after,
            "delete must not touch live credential"
        );
    }

    #[test]
    fn switch_unknown_id_errors() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("switch-unknown");
        let _h = EnvVarGuard::set("HOME", &tmp);
        assert!(switch_account("codex:nope".into()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn switched_auth_json_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("perms");
        let _h = EnvVarGuard::set("HOME", &tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex, None).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();
        switch_account(id).unwrap();
        let mode = std::fs::metadata(tmp.join(".codex/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "restored auth.json must be 0600");
    }

    #[test]
    fn codex_home_env_redirects_credential_path() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("codex-home-env");
        let _h = EnvVarGuard::set("HOME", &tmp);
        let custom = tmp.join("relocated-codex");
        std::fs::create_dir_all(&custom).unwrap();
        let _ch = EnvVarGuard::set("CODEX_HOME", &custom);

        // The live login lives under CODEX_HOME, NOT ~/.codex.
        let jwt = fake_jwt(json!({
            "email": "moved@example.com",
            "https://api.openai.com/auth": { "chatgpt_user_id": "u-moved" },
        }));
        let doc = json!({
            "tokens": { "id_token": jwt, "access_token": "ax", "account_id": "acct-moved" },
        });
        std::fs::write(
            custom.join("auth.json"),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();
        assert!(
            !tmp.join(".codex/auth.json").exists(),
            "~/.codex must be untouched when CODEX_HOME is set"
        );

        // Read + save resolve through CODEX_HOME.
        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(
            state.current.as_ref().unwrap().email.as_deref(),
            Some("moved@example.com")
        );
        save_current_account(CliApp::Codex, None).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();

        // Switch writes back under CODEX_HOME, never creating ~/.codex.
        std::fs::write(custom.join("auth.json"), "{}").unwrap();
        switch_account(id).unwrap();
        let restored: JsonValue =
            serde_json::from_slice(&std::fs::read(custom.join("auth.json")).unwrap()).unwrap();
        assert_eq!(
            restored
                .pointer("/tokens/account_id")
                .and_then(|v| v.as_str()),
            Some("acct-moved")
        );
        assert!(!tmp.join(".codex/auth.json").exists());
    }

    #[test]
    fn storage_warning_fires_only_for_keyring_or_auto() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("store-mode");
        let _h = EnvVarGuard::set("HOME", &tmp);
        let cfg = tmp.join(".codex/config.toml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();

        std::fs::write(&cfg, "cli_auth_credentials_store = \"keyring\"\n").unwrap();
        assert_eq!(codex_storage_warning().as_deref(), Some("keyring"));

        std::fs::write(&cfg, "cli_auth_credentials_store = \"file\"\n").unwrap();
        assert_eq!(codex_storage_warning(), None);

        std::fs::write(&cfg, "# cli_auth_credentials_store = \"auto\"\n").unwrap();
        assert_eq!(codex_storage_warning(), None, "commented line is ignored");
    }
}
