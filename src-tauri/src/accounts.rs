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
    /// Whether the live login is already captured in `accounts` — when
    /// false the UI can warn that switching away would lose it.
    saved: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedAccountView {
    id: String,
    name: String,
    email: Option<String>,
    plan: Option<String>,
    saved_at: String,
    /// True when this snapshot matches the live login.
    active: bool,
    /// Set when the last `refresh_codex_tokens` call after switching to this
    /// account failed — the refresh_token has been revoked and the user must
    /// re-authenticate.
    needs_relogin: bool,
}

// ===================================================================
// IPC entry points
// ===================================================================

/// List the live + saved official accounts for `app`.
/// Codex — full multi-account management.
/// Claude / Gemini — display-only: reads the CLI's live credential file for
///   current name / email (no saved accounts, no switching).
/// Other apps — empty state.
pub fn list_accounts(app: CliApp) -> Result<AccountsState, Box<dyn Error>> {
    match app {
        CliApp::Codex => list_codex_accounts(),
        CliApp::Claude => list_claude_accounts(),
        CliApp::Gemini => list_gemini_accounts(),
        _ => Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        }),
    }
}

/// Snapshot the CLI's current official login into the store.
/// Upserts by id so re-saving refreshes the token payload.
pub fn save_current_account(app: CliApp) -> Result<(), Box<dyn Error>> {
    match app {
        CliApp::Codex => save_codex_account(),
        _ => Err(unsupported(app)),
    }
}

/// Restore a saved snapshot into the live CLI credential.
/// Refreshes tokens in memory BEFORE writing to auth.json — if refresh fails
/// the auth.json is left untouched and the caller should mark needsRelogin.
pub async fn switch_account(id: String) -> Result<(), Box<dyn Error>> {
    let store = read_store()?;
    let entry = store
        .iter()
        .find(|e| str_field(e, "id") == Some(id.as_str()))
        .ok_or("Account not found")?;
    let payload = str_field(entry, "payload").ok_or("Saved account has no credential payload")?;
    match str_field(entry, "app") {
        Some("codex") => switch_codex(payload).await,
        other => Err(format!("Unsupported account app: {}", other.unwrap_or("?")).into()),
    }
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

/// Tauri-managed state allowing `cancel_codex_login` to abort an in-flight
/// `login_and_save_codex_account` call.
pub struct CodexLoginCancel(pub std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>);

pub const CODEX_LOGIN_URL_EVENT: &str = "codex:login-url";

pub async fn login_and_save_codex_account(
    app: tauri::AppHandle,
    cancel_state: &CodexLoginCancel,
) -> Result<String, String> {
    let auth_path = codex_auth_path().map_err(|e| e.to_string())?;
    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // If there is a live Codex login that is not yet saved, auto-save it so
    // we can restore it after the new login completes.
    let prev_active_id: Option<String> =
        auto_save_unsaved_live_codex_account().map_err(|e| e.to_string())?;

    // Snapshot original auth.json for rollback.
    let original_auth: Option<Vec<u8>> = std::fs::read(&auth_path).ok();

    // Clear auth.json — codex's logout_with_revoke will find nothing to revoke.
    atomic_write_0600(&auth_path, b"{}").map_err(|e| e.to_string())?;

    // Set up a cancel token so `cancel_codex_login` can interrupt the select!.
    let cancel_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    *cancel_state.0.lock().unwrap() = Some(cancel_notify.clone());

    // Spawn `codex login` (non-interactive; the browser handles the UI).
    // Resolve the real binary via the same scan `detect_clis` uses: a bare
    // `Command::new("codex")` only finds `codex.exe` on Windows (the
    // runtime appends .exe, never .cmd), so the npm-installed `codex.cmd`
    // shim made detection say "installed" while this spawn failed
    // NotFound. An explicit path ending in `.cmd` goes through the
    // runtime's hardened cmd.exe routing instead; PATH is augmented with
    // the binary's dir so the shim finds its node (same as the version
    // probes).
    let resolved = crate::providers::find_cli_binary("codex");
    let program: std::ffi::OsString = resolved
        .as_deref()
        .map(|p| p.as_os_str().to_os_string())
        .unwrap_or_else(|| "codex".into());
    let mut cmd = tokio::process::Command::new(&program);
    if let Some(path) = resolved
        .as_deref()
        .and_then(crate::providers::augmented_path_for)
    {
        cmd.env("PATH", path);
    }
    cmd.arg("login")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // Silent helper (output is captured, the browser is the UI) — don't
    // flash a console window on Windows (see providers::hide_console).
    #[cfg(windows)]
    cmd.creation_flags(crate::providers::CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
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

    // Read stderr line-by-line; emit the auth URL as soon as we see it so
    // the frontend can show a dialog without waiting for login to finish.
    // The URL appears as a bare `https://…` line (per codex's
    // `print_login_server_start` in cli/src/login.rs:113).
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let app_clone = app.clone();
    let stderr_task: tokio::task::JoinHandle<String> = tokio::spawn(async move {
        use tauri::Emitter as _;
        use tokio::io::AsyncBufReadExt as _;
        let reader = tokio::io::BufReader::new(stderr_pipe);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.starts_with("https://") {
                let _ = app_clone.emit(CODEX_LOGIN_URL_EVENT, trimmed.to_string());
            }
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(trimmed);
        }
        collected
    });

    // Wait up to 5 minutes for the browser login to complete.
    let status = tokio::select! {
        r = child.wait() => match r {
            Ok(s) => s,
            Err(e) => {
                stderr_task.abort();
                restore_auth(&auth_path, original_auth.as_deref());
                *cancel_state.0.lock().unwrap() = None;
                return Err(format!("codex login error: {e}"));
            }
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            let _ = child.kill().await;
            stderr_task.abort();
            restore_auth(&auth_path, original_auth.as_deref());
            *cancel_state.0.lock().unwrap() = None;
            return Err("codex login timed out after 5 minutes".into());
        },
        _ = cancel_notify.notified() => {
            let _ = child.kill().await;
            stderr_task.abort();
            restore_auth(&auth_path, original_auth.as_deref());
            *cancel_state.0.lock().unwrap() = None;
            return Err("Login cancelled".into());
        },
    };
    *cancel_state.0.lock().unwrap() = None;

    let stderr_msg = stderr_task.await.unwrap_or_default();

    if !status.success() {
        restore_auth(&auth_path, original_auth.as_deref());
        return Err(format!(
            "codex login failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_msg.trim()
        ));
    }

    // Save the new account (reads the fresh auth.json codex just wrote).
    if let Err(e) = save_codex_account() {
        restore_auth(&auth_path, original_auth.as_deref());
        return Err(format!("Login succeeded but could not save account: {e}"));
    }

    // Capture the new account's id before we restore the previous account.
    let new_id = read_codex_live()
        .ok()
        .flatten()
        .map(|live| live.id)
        .unwrap_or_default();

    // Restore the previously active account (writes its snapshot back to auth.json).
    if let Some(prev_id) = prev_active_id {
        if let Err(e) = switch_account(prev_id.clone()).await {
            // Don't fail the overall operation — the new account was saved successfully.
            // Mark the previous account as needing re-login so the UI reflects the issue.
            log::warn!("Failed to restore previous account {prev_id}: {e}");
            let _ = mark_account_relogin(&prev_id, true);
        }
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
    let id = live.id.as_str();
    if store
        .iter()
        .any(|e| str_field(e, "app") == Some("codex") && str_field(e, "id") == Some(id))
    {
        return Ok(Some(id.to_string()));
    }
    let entry = serde_json::json!({
        "id": id, "app": "codex",
        "name": live.name, "email": live.email, "plan": live.plan,
        "payload": live.raw, "savedAt": now_rfc3339(),
    });
    store.push(entry);
    write_store(store)?;
    Ok(Some(id.to_string()))
}

/// Abort an in-flight `login_and_save_codex_account` call by signalling its
/// cancel token.  No-op when no login is in progress.
pub async fn cancel_codex_login(cancel_state: &CodexLoginCancel) -> Result<(), String> {
    if let Some(notify) = cancel_state.0.lock().unwrap().as_ref() {
        notify.notify_one();
    }
    Ok(())
}

/// Restore auth.json from a snapshot, or remove it if there was none.
fn restore_auth(path: &std::path::Path, original: Option<&[u8]>) {
    match original {
        Some(bytes) => {
            let _ = atomic_write_0600(path, bytes);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
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

/// Schema version stamped into accounts.json. Bump when the on-disk shape
/// changes in a backward-incompatible way and add a migration arm to
/// `migrate_account_entries`. v1 is the current baseline.
pub const ACCOUNTS_SCHEMA_VERSION: u64 = 1;

fn read_store() -> Result<Vec<JsonValue>, Box<dyn Error>> {
    let raw = crate::config::read_accounts()?;
    let map = match raw {
        JsonValue::Object(m) => m,
        _ => return Ok(Vec::new()),
    };
    let version = map
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(ACCOUNTS_SCHEMA_VERSION);
    let entries = match map.get("accounts") {
        Some(JsonValue::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    Ok(migrate_account_entries(version, entries))
}

/// Upgrade account entries written by an older schema version to the
/// current shape. Add an arm when bumping `ACCOUNTS_SCHEMA_VERSION`.
fn migrate_account_entries(_version: u64, entries: Vec<JsonValue>) -> Vec<JsonValue> {
    // e.g. `if _version < 2 { entries = entries.into_iter().map(...).collect() }`
    entries
}

fn write_store(entries: Vec<JsonValue>) -> Result<(), Box<dyn Error>> {
    let mut env = serde_json::Map::new();
    env.insert("version".into(), JsonValue::from(ACCOUNTS_SCHEMA_VERSION));
    env.insert("accounts".into(), JsonValue::Array(entries));
    crate::config::write_accounts(&JsonValue::Object(env))
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
    /// Stable per-account identity: tokens.account_id → chatgpt_account_id
    /// → email → stable_hash. Mirrors official local_chatgpt_auth.rs:37-40.
    id: String,
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// Full auth.json content, stored verbatim as the snapshot payload.
    raw: String,
}

// ===================================================================
// Claude account (display-only — reads ~/.claude.json `oauthAccount`)
// ===================================================================

fn list_claude_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let home = dirs::home_dir().ok_or("home directory not available")?;
    let path = home.join(".claude.json");
    if !path.exists() {
        return Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        });
    }
    let content = std::fs::read_to_string(&path)?;
    let doc: serde_json::Value = serde_json::from_str(&content).unwrap_or(serde_json::Value::Null);
    let oauth = doc.get("oauthAccount");
    let email = oauth
        .and_then(|o| o.get("emailAddress"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let name = oauth
        .and_then(|o| o.get("displayName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    if email.is_none() && name.is_none() {
        return Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        });
    }
    Ok(AccountsState {
        current: Some(CurrentAccount {
            name,
            email,
            plan: None,
            saved: true,
        }),
        accounts: Vec::new(),
        storage_warning: None,
    })
}

// ===================================================================
// Gemini account (display-only — decodes id_token from oauth_creds.json)
// ===================================================================

fn list_gemini_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let home = dirs::home_dir().ok_or("home directory not available")?;
    let path = home.join(".gemini").join("oauth_creds.json");
    if !path.exists() {
        return Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        });
    }
    let content = std::fs::read_to_string(&path)?;
    let doc: serde_json::Value = serde_json::from_str(&content).unwrap_or(serde_json::Value::Null);
    let id_token = doc.get("id_token").and_then(|v| v.as_str()).unwrap_or("");
    let email = jwt_claim(id_token, "email");
    let name = jwt_claim(id_token, "name");
    if email.is_none() && name.is_none() {
        return Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        });
    }
    Ok(AccountsState {
        current: Some(CurrentAccount {
            name,
            email,
            plan: None,
            saved: true,
        }),
        accounts: Vec::new(),
        storage_warning: None,
    })
}

/// Decode one string claim from a JWT payload (no signature verification —
/// display only). Returns `None` if the JWT is malformed or the claim absent.
fn jwt_claim(jwt: &str, claim: &str) -> Option<String> {
    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get(claim)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// ===================================================================
// Codex
// ===================================================================

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
    // tokens.account_id is the top-level field in auth.json (set by the
    // login server alongside the token bundle). Mirrors the official priority
    // in local_chatgpt_auth.rs:37-40: tokens.account_id → chatgpt_account_id.
    let account_id = tokens.get("account_id").and_then(|v| v.as_str());

    let name = id_info.as_ref().and_then(|i| i.name.clone());
    let email = id_info.as_ref().and_then(|i| i.email.clone());
    let plan = id_info.as_ref().and_then(|i| i.plan.clone());
    let chatgpt_account_id = id_info.as_ref().and_then(|i| i.chatgpt_account_id.clone());

    let id = account_id
        .map(String::from)
        .or(chatgpt_account_id)
        .or_else(|| email.clone())
        .unwrap_or_else(|| stable_hash(&raw).to_string());

    Ok(Some(CodexLive {
        id,
        name,
        email,
        plan,
        raw,
    }))
}

fn list_codex_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let live = read_codex_live()?;
    let current_id = live.as_ref().map(|l| l.id.as_str());
    let store = read_store()?;

    let mut accounts = Vec::new();
    let mut current_saved = false;
    for e in &store {
        if str_field(e, "app") != Some("codex") {
            continue;
        }
        let active = current_id.is_some() && current_id == str_field(e, "id");
        if active {
            current_saved = true;
        }
        let needs_relogin = e
            .get("needsRelogin")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        accounts.push(SavedAccountView {
            id: str_field(e, "id").unwrap_or_default().to_string(),
            name: str_field(e, "name").unwrap_or_default().to_string(),
            email: str_field(e, "email").map(String::from),
            plan: str_field(e, "plan").map(String::from),
            saved_at: str_field(e, "savedAt").unwrap_or_default().to_string(),
            active,
            needs_relogin,
        });
    }

    let current = live.map(|l| CurrentAccount {
        name: l.name,
        email: l.email,
        plan: l.plan,
        saved: current_saved,
    });

    Ok(AccountsState {
        current,
        accounts,
        storage_warning: codex_storage_warning(),
    })
}

fn save_codex_account() -> Result<(), Box<dyn Error>> {
    let live = read_codex_live()?
        .ok_or("No Codex ChatGPT login found to save (run `codex login` first)")?;

    let id = live.id.clone();
    let mut store = read_store()?;

    let entry = json!({
        "id": id,
        "app": "codex",
        "name": live.name,
        "email": live.email,
        "plan": live.plan,
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

/// Set or clear the `needsRelogin` flag on a saved account. Called by the
/// frontend after `refresh_codex_tokens`: on failure the flag is set (the
/// refresh_token has been revoked; the user must re-authenticate); on success
/// it is cleared. `save_codex_account` also clears it implicitly because it
/// replaces the whole store entry without the flag.
pub fn mark_account_relogin(id: &str, needed: bool) -> Result<(), Box<dyn Error>> {
    let mut store = read_store()?;
    let slot = store
        .iter_mut()
        .find(|e| str_field(e, "id") == Some(id))
        .ok_or_else(|| format!("account {id} not found"))?;
    if let JsonValue::Object(o) = slot {
        if needed {
            o.insert("needsRelogin".into(), JsonValue::Bool(true));
        } else {
            o.remove("needsRelogin");
        }
    }
    write_store(store)
}

#[allow(dead_code)]
enum RefreshError {
    /// Token is permanently invalid (4xx from OAuth server, excluding 429).
    /// The user must re-authenticate.
    AuthFailure(String),
    /// Transient error: rate-limited (429), server error (5xx), network
    /// failure, or missing refresh_token. Safe to fall back to the snapshot.
    Transient(String),
}

/// POST the saved refresh_token to `https://auth.openai.com/oauth/token`,
/// merge the new id/access/refresh tokens back into auth.json, and update
/// `last_refresh`. Endpoint + client_id from codex-rs
/// `login/src/auth/manager.rs:186,CLIENT_ID`.
///
/// On success the doc is mutated with the new tokens.
/// On failure returns `RefreshError` — the doc is left unchanged.
async fn refresh_doc_tokens(doc: &mut JsonValue) -> Result<(), RefreshError> {
    let refresh_token = doc
        .pointer("/tokens/refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RefreshError::Transient("No refresh_token in saved credential".into()))?
        .to_string();

    // Mirror codex-rs default_client.rs `get_codex_user_agent()` +
    // `default_headers()` (default_client.rs:138-161, 289-304):
    //   User-Agent = "codex_cli_rs/{ver} ({OS type} {OS ver}; {arch}) {terminal}"
    //   originator = "codex_cli_rs"  (DEFAULT_ORIGINATOR, manager.rs:203)
    // Primary: `codex --version`. Fallback: `~/.codex/version.json` latest_version
    // (written by Codex itself — a real version string). Both are real Codex
    // version numbers so the User-Agent is never fabricated.
    let codex_version = crate::providers::detect_cli_versions()
        .remove(&crate::providers::CliApp::Codex)
        .flatten()
        .or_else(|| crate::providers::codex_latest_known_version())
        .unwrap_or_else(|| "unknown".to_string());
    let os = os_info::get();
    let user_agent_str = format!(
        "codex_cli_rs/{codex_version} ({} {}; {}) unknown",
        os.os_type(),
        os.version(),
        os.architecture().unwrap_or("unknown"),
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                "originator",
                reqwest::header::HeaderValue::from_static("codex_cli_rs"),
            );
            if let Ok(ua) = reqwest::header::HeaderValue::from_str(&user_agent_str) {
                h.insert(reqwest::header::USER_AGENT, ua);
            }
            h
        })
        .build()
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

    let resp = client
        .post("https://auth.openai.com/oauth/token")
        .json(&json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

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
        // 4xx (except 429) = permanent auth failure; 429 and 5xx = transient.
        return if status.is_client_error() && status.as_u16() != 429 {
            Err(RefreshError::AuthFailure(format!(
                "Token refresh failed ({status}): {msg}"
            )))
        } else {
            Err(RefreshError::Transient(format!(
                "Token refresh failed ({status}): {msg}"
            )))
        };
    }

    let body: JsonValue = resp
        .json()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    if let Some(tokens) = doc.get_mut("tokens").and_then(|v| v.as_object_mut()) {
        for key in ["id_token", "access_token", "refresh_token"] {
            if let Some(v) = body.get(key).filter(|v| v.is_string()) {
                tokens.insert(key.into(), v.clone());
            }
        }
    }
    doc["last_refresh"] = JsonValue::String(chrono::Utc::now().to_rfc3339());
    Ok(())
}

/// Validate/refresh tokens in memory first, then write to auth.json.
/// If refresh fails auth.json is left untouched and Err is returned.
async fn switch_codex(payload: &str) -> Result<(), Box<dyn Error>> {
    let mut doc: JsonValue = serde_json::from_str(payload)
        .map_err(|e| format!("Saved Codex credential is corrupt: {e}"))?;

    // Always attempt a token refresh to validate the credential is still valid
    // server-side. A 401 (token_revoked — user logged out of Codex since the
    // snapshot was taken) surfaces as Err so the caller marks needsRelogin
    // immediately. Network / connectivity failures are non-fatal: we fall back
    // to writing the snapshot as-is so a temporary outage doesn't block switching.
    match refresh_doc_tokens(&mut doc).await {
        Ok(()) => {}
        Err(RefreshError::AuthFailure(e)) => return Err(e.into()),
        Err(RefreshError::Transient(_)) => {} // rate-limit / network / no token — proceed with snapshot
    }

    let path = codex_auth_path()?;
    let updated = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    atomic_write_0600(&path, updated.as_bytes())?;
    save_codex_account()?;
    Ok(())
}

/// Decode the useful claims from a Codex `id_token` JWT (no signature
/// verification — display only). Claim layout per codex-rs
/// `login/src/token_data.rs:73-160`.
struct CodexIdInfo {
    /// The OIDC `name` claim — the account's display name.
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// `chatgpt_account_id` from the JWT auth claims — workspace/org ID.
    /// Present for enterprise/team accounts, absent for personal accounts.
    chatgpt_account_id: Option<String>,
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

    let chatgpt_account_id = auth
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(CodexIdInfo {
        name,
        email,
        plan,
        chatgpt_account_id,
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
    use crate::testutils::{lock_home, override_home, EnvVarGuard};
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
            },
        }));
        let doc = json!({
            "tokens": {
                "id_token": jwt,
                "access_token": "access-xyz",
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
    fn parse_id_token_extracts_name_email_plan_and_account_id() {
        let jwt = fake_jwt(json!({
            "name": "Jane Doe",
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro",
                "chatgpt_account_id": "account-123",
            },
        }));
        let info = parse_codex_id_token(&jwt).unwrap();
        assert_eq!(info.name.as_deref(), Some("Jane Doe"));
        assert_eq!(info.email.as_deref(), Some("user@example.com"));
        assert_eq!(info.plan.as_deref(), Some("Pro"));
        assert_eq!(info.chatgpt_account_id.as_deref(), Some("account-123"));
    }

    #[test]
    fn save_uses_name_from_id_token() {
        let _g = lock_home();
        let tmp = tempdir("name-label");
        let _h = override_home(&tmp);

        // auth.json whose id_token carries a display `name`.
        let jwt = fake_jwt(json!({
            "name": "Jane Doe",
            "email": "jane@example.com",
        }));
        let doc = json!({ "tokens": { "id_token": jwt, "access_token": "ax", "account_id": "acct-jane" } });
        let dir = tmp.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), serde_json::to_string(&doc).unwrap()).unwrap();

        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(
            state.current.as_ref().unwrap().name.as_deref(),
            Some("Jane Doe")
        );

        save_current_account(CliApp::Codex).unwrap();
        let saved = &list_accounts(CliApp::Codex).unwrap().accounts[0];
        assert_eq!(saved.name, "Jane Doe");
        assert_eq!(saved.email.as_deref(), Some("jane@example.com"));
    }

    #[test]
    fn accounts_file_is_versioned_object_envelope() {
        let _g = lock_home();
        let tmp = tempdir("versioned-envelope");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();

        // On disk: `{ "version": N, "accounts": [...] }` — never a bare array.
        let path = tmp.join(".termory/accounts.json");
        let raw: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw.pointer("/version").and_then(|v| v.as_u64()),
            Some(ACCOUNTS_SCHEMA_VERSION),
            "accounts.json must carry a version stamp"
        );
        assert!(
            raw.pointer("/accounts")
                .and_then(|v| v.as_array())
                .is_some(),
            "accounts.json must have an 'accounts' array"
        );
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
        let _g = lock_home();
        let tmp = tempdir("no-login");
        let _h = override_home(&tmp);
        let state = list_accounts(CliApp::Codex).unwrap();
        assert!(state.current.is_none());
        assert!(state.accounts.is_empty());
    }

    #[tokio::test]
    async fn save_then_switch_roundtrip_restores_exact_auth_json() {
        let _g = lock_home();
        let tmp = tempdir("roundtrip");
        let _h = override_home(&tmp);

        // Account A logged in → save it.
        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        let a_bytes = std::fs::read(tmp.join(".codex/auth.json")).unwrap();
        save_current_account(CliApp::Codex).unwrap();

        let state = list_accounts(CliApp::Codex).unwrap();
        let cur = state.current.as_ref().unwrap();
        assert_eq!(cur.email.as_deref(), Some("a@example.com"));
        assert_eq!(cur.plan.as_deref(), Some("Pro"));
        assert!(cur.saved);
        assert_eq!(state.accounts.len(), 1);
        assert!(state.accounts[0].active);
        let a_id = state.accounts[0].id.clone();

        // Account B logs in (different file) and is saved too.
        write_codex_auth(&tmp, "b@example.com", "plus", "acct-b");
        save_current_account(CliApp::Codex).unwrap();
        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(state.accounts.len(), 2);
        // Now A is no longer active (B is live).
        let a_view = state.accounts.iter().find(|x| x.id == a_id).unwrap();
        assert!(!a_view.active);

        // Switch back to A → live auth.json is byte-identical to A's snapshot.
        switch_account(a_id.clone()).await.unwrap();
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
    fn resave_same_account_upserts_not_duplicates() {
        let _g = lock_home();
        let tmp = tempdir("upsert");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        save_current_account(CliApp::Codex).unwrap();

        let state = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(state.accounts.len(), 1, "must upsert, not duplicate");
    }

    #[test]
    fn delete_only_touches_the_store() {
        let _g = lock_home();
        let tmp = tempdir("delete-store");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();

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

    #[tokio::test]
    async fn switch_unknown_id_errors() {
        let _g = lock_home();
        let tmp = tempdir("switch-unknown");
        let _h = override_home(&tmp);
        assert!(switch_account("no-such-id".into()).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn switched_auth_json_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_home();
        let tmp = tempdir("perms");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();
        switch_account(id).await.unwrap();
        let mode = std::fs::metadata(tmp.join(".codex/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "restored auth.json must be 0600");
    }

    #[tokio::test]
    async fn codex_home_env_redirects_credential_path() {
        let _g = lock_home();
        let tmp = tempdir("codex-home-env");
        let _h = override_home(&tmp);
        let custom = tmp.join("relocated-codex");
        std::fs::create_dir_all(&custom).unwrap();
        let _ch = EnvVarGuard::set("CODEX_HOME", &custom);

        // The live login lives under CODEX_HOME, NOT ~/.codex.
        let jwt = fake_jwt(json!({ "email": "moved@example.com" }));
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
        save_current_account(CliApp::Codex).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();

        // Switch writes back under CODEX_HOME, never creating ~/.codex.
        std::fs::write(custom.join("auth.json"), "{}").unwrap();
        switch_account(id).await.unwrap();
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
        let _g = lock_home();
        let tmp = tempdir("store-mode");
        let _h = override_home(&tmp);
        let cfg = tmp.join(".codex/config.toml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();

        std::fs::write(&cfg, "cli_auth_credentials_store = \"keyring\"\n").unwrap();
        assert_eq!(codex_storage_warning().as_deref(), Some("keyring"));

        std::fs::write(&cfg, "cli_auth_credentials_store = \"file\"\n").unwrap();
        assert_eq!(codex_storage_warning(), None);

        std::fs::write(&cfg, "# cli_auth_credentials_store = \"auto\"\n").unwrap();
        assert_eq!(codex_storage_warning(), None, "commented line is ignored");
    }

    #[test]
    fn mark_relogin_sets_and_clears_flag() {
        let _g = lock_home();
        let tmp = tempdir("mark-relogin");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        let id = list_accounts(CliApp::Codex).unwrap().accounts[0].id.clone();

        // Set needsRelogin = true.
        mark_account_relogin(&id, true).unwrap();
        let state = list_accounts(CliApp::Codex).unwrap();
        assert!(
            state
                .accounts
                .iter()
                .find(|a| a.id == id)
                .unwrap()
                .needs_relogin,
            "flag should be set"
        );

        // Clear needsRelogin = false.
        mark_account_relogin(&id, false).unwrap();
        let state = list_accounts(CliApp::Codex).unwrap();
        assert!(
            !state
                .accounts
                .iter()
                .find(|a| a.id == id)
                .unwrap()
                .needs_relogin,
            "flag should be cleared"
        );
    }
}
