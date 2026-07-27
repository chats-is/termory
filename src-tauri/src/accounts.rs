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
//! A saved account stores `auth.json` (the OAuth `tokens` block —
//! id/access/refresh — plus `last_refresh` / `account_id` must travel
//! together for Codex's refresh to keep working), labelled by the email +
//! plan decoded from the `tokens.id_token` JWT — **minus the fields
//! provider management owns**, see `PROVIDER_OWNED_AUTH_FIELDS`.
//!
//! ## Phase 2 — Claude Code (Keychain-first)
//!
//! Claude's credential store is two-tier (macOS Keychain over
//! `.credentials.json`), handled by the `claude_auth` module; the account
//! IDENTITY lives separately in `~/.claude.json` `oauthAccount`. See the
//! "Claude Code — full multi-account management" section below for the
//! snapshot shape and the deliberate `~/.claude.json` write exception.

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::error::Error;

use crate::providers::{codex_auth_path, codex_root, CliApp};

/// The `auth.json` fields owned by PROVIDER management, not by an account
/// snapshot: `providers::activate_codex` writes them (`auth_mode = "apikey"`
/// plus the third-party `OPENAI_API_KEY`) and `deactivate_codex` removes
/// them. Both deliberately leave the OAuth `tokens` alongside, which is why
/// a live login still reads as a valid account while a provider is active.
///
/// Account switching swaps the official login ONLY, so these must survive it
/// untouched — and must never be captured into a snapshot, which would copy
/// a third-party API key into `accounts.json` and re-apply it on some later
/// switch to a completely different account.
const PROVIDER_OWNED_AUTH_FIELDS: &[&str] = &["auth_mode", "OPENAI_API_KEY"];

/// Drop the provider-owned fields from a credential document about to be
/// stored, so a snapshot taken while a custom provider was active carries
/// only the official login.
fn strip_provider_fields(doc: &mut JsonValue) {
    if let JsonValue::Object(o) = doc {
        for key in PROVIDER_OWNED_AUTH_FIELDS {
            o.remove(*key);
        }
    }
}

/// Overwrite `doc`'s provider-owned fields with whatever the LIVE `auth.json`
/// currently holds (absent there ⇒ absent here), so writing `doc` leaves the
/// active provider exactly as it was found.
///
/// Deliberately infallible: an unreadable or corrupt live file is treated as
/// "no provider fields", which strips them from `doc`. Writing no key is
/// always safe (Codex falls back to the OAuth `tokens`); writing a stale one
/// from an unrelated snapshot is what this whole seam exists to prevent.
fn carry_over_provider_fields(path: &std::path::Path, doc: &mut JsonValue) {
    let live: Option<JsonValue> = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let JsonValue::Object(target) = doc else {
        return;
    };
    for key in PROVIDER_OWNED_AUTH_FIELDS {
        match live.as_ref().and_then(|l| l.get(*key)) {
            Some(v) => {
                target.insert((*key).to_string(), v.clone());
            }
            None => {
                target.remove(*key);
            }
        }
    }
}

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
        CliApp::Grok => list_grok_accounts(),
        _ => Ok(AccountsState {
            current: None,
            accounts: Vec::new(),
            storage_warning: None,
        }),
    }
}

/// One saved login as the menu-bar tray renders it: a checkable row.
pub struct TrayAccount {
    pub id: String,
    /// Single-line display label (`name · email`, whichever parts exist).
    pub label: String,
    /// Matches the live login — the tray checkmarks this row.
    pub active: bool,
    pub needs_relogin: bool,
}

/// Saved logins for the tray's account submenu. Codex + Claude — the CLIs
/// with snapshot management (`list_accounts` is display-only for the others),
/// so every other app returns `[]` and the tray renders no account section.
/// A read failure also yields `[]`: no accounts.json just means "nothing saved".
pub fn tray_accounts(app: CliApp) -> Vec<TrayAccount> {
    let state = match app {
        CliApp::Codex => list_codex_accounts(),
        CliApp::Claude => {
            // Perf gate for the tray HOT PATH (`build_menu` runs on the main
            // thread): resolving the live Claude login spawns `security(1)`
            // on macOS. With nothing saved there are no rows to render, so
            // skip the whole live read — a user who never touches Claude
            // multi-account pays zero for it (same rule as the Windows
            // `Get-AppxPackage` split in providers.rs).
            let has_saved = read_store()
                .map(|s| s.iter().any(|e| str_field(e, "app") == Some("claude")))
                .unwrap_or(false);
            if !has_saved {
                return Vec::new();
            }
            list_claude_accounts()
        }
        _ => return Vec::new(),
    };
    let Ok(state) = state else {
        return Vec::new();
    };
    state
        .accounts
        .into_iter()
        .map(|a| {
            // Same primary label as the Providers page card (name, else email),
            // plus the email as the disambiguator when both exist — two logins
            // can share a display name, and the email is what identifies them.
            let name = a.name.trim();
            let email = a.email.as_deref().map(str::trim).unwrap_or("");
            let label = match (name.is_empty(), email.is_empty()) {
                (false, false) if name != email => format!("{name} · {email}"),
                (false, _) => name.to_string(),
                (true, false) => email.to_string(),
                (true, true) => a.id.clone(),
            };
            TrayAccount {
                id: a.id,
                label,
                active: a.active,
                needs_relogin: a.needs_relogin,
            }
        })
        .collect()
}

/// Snapshot the CLI's current official login into the store.
/// Upserts by id so re-saving refreshes the token payload.
pub fn save_current_account(app: CliApp) -> Result<(), Box<dyn Error>> {
    match app {
        CliApp::Codex => save_codex_account(),
        CliApp::Claude => save_claude_account(),
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
    let payload = entry
        .get("payload")
        .cloned()
        .ok_or("Saved account has no credential payload")?;
    let app = str_field(entry, "app").map(String::from);
    match app.as_deref() {
        Some("codex") => switch_codex(&payload).await,
        Some("claude") => switch_claude(&payload).await,
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

/// RAII reservation of a login-cancel slot. `reserve` fails when a login is
/// already in flight — the backend re-entrancy guard the frontend's
/// `loggingIn` flags can't fully provide (two clicks in one React batch both
/// read the stale flag) — and the slot self-clears on Drop, so no early-exit
/// path can leak a stale notify. A leaked notify used to be merely a dead
/// `cancel_*_login`; with this guard it would block every future login, so
/// the Drop-based clear is load-bearing, not tidiness. (The codex flow's
/// spawn-failure path really did leak the slot before this.)
struct LoginSlot<'a> {
    slot: &'a std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>,
}
impl<'a> LoginSlot<'a> {
    fn reserve(
        slot: &'a std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>,
    ) -> Result<(Self, std::sync::Arc<tokio::sync::Notify>), String> {
        let mut s = slot.lock().unwrap();
        if s.is_some() {
            return Err("A login is already in progress".into());
        }
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        *s = Some(notify.clone());
        Ok((LoginSlot { slot }, notify))
    }
}
impl Drop for LoginSlot<'_> {
    fn drop(&mut self) {
        *self.slot.lock().unwrap() = None;
    }
}

pub const CODEX_LOGIN_URL_EVENT: &str = "codex:login-url";
pub const CLAUDE_LOGIN_URL_EVENT: &str = "claude:login-url";

pub async fn login_and_save_codex_account(
    app: tauri::AppHandle,
    cancel_state: &CodexLoginCancel,
) -> Result<String, String> {
    let auth_path = codex_auth_path().map_err(|e| e.to_string())?;
    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Reserve the cancel slot (also the re-entrancy guard); self-clears on
    // every exit path via Drop.
    let (_login_slot, cancel_notify) = LoginSlot::reserve(&cancel_state.0)?;

    // If there is a live Codex login that is not yet saved, auto-save it so
    // we can restore it after the new login completes.
    let prev_active_id: Option<String> =
        auto_save_unsaved_live_account(CliApp::Codex).map_err(|e| e.to_string())?;

    // Snapshot original auth.json for rollback.
    let original_auth: Option<Vec<u8>> = std::fs::read(&auth_path).ok();

    // Clear auth.json — codex's logout_with_revoke will find nothing to revoke.
    atomic_write_0600(&auth_path, b"{}").map_err(|e| e.to_string())?;

    // Spawn `codex login` (non-interactive; the browser handles the UI).
    // Resolve the real binary via the same scan `detect_clis` uses: a bare
    // `Command::new("codex")` only finds `codex.exe` on Windows (the
    // runtime appends .exe, never .cmd), so the npm-installed `codex.cmd`
    // shim made detection say "installed" while this spawn failed
    // NotFound. An explicit path ending in `.cmd` goes through the
    // runtime's hardened cmd.exe routing instead; PATH is augmented with
    // the binary's dir so the shim finds its node (same as the version
    // probes). `codex_binary` falls back to the desktop app's bundled
    // CLI, so an app-only install can still add accounts.
    let resolved = crate::providers::codex_binary();
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
                return Err(format!("codex login error: {e}"));
            }
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            let _ = child.kill().await;
            stderr_task.abort();
            restore_auth(&auth_path, original_auth.as_deref());
            return Err("codex login timed out after 5 minutes".into());
        },
        _ = cancel_notify.notified() => {
            let _ = child.kill().await;
            stderr_task.abort();
            restore_auth(&auth_path, original_auth.as_deref());
            return Err("Login cancelled".into());
        },
    };

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

/// Snapshot `app`'s live login if it isn't in the store yet, so a flow that
/// is about to overwrite the credential can't destroy it. Used by the
/// `codex login` flow and by the tray's account switch (which, unlike the
/// Providers page, has no dialog to warn in). Non-managed apps are a no-op.
pub(crate) fn auto_save_unsaved_live_account(
    app: CliApp,
) -> Result<Option<String>, Box<dyn Error>> {
    match app {
        CliApp::Codex => auto_save_unsaved_live_codex_account(),
        CliApp::Claude => {
            let Some(live) = read_claude_live()? else {
                return Ok(None);
            };
            let id = live.id;
            let store = read_store()?;
            if !store.iter().any(|e| {
                str_field(e, "app") == Some("claude") && str_field(e, "id") == Some(id.as_str())
            }) {
                save_claude_account()?;
            }
            Ok(Some(id))
        }
        _ => Ok(None),
    }
}

/// If there is a live Codex login that is not yet recorded in the store,
/// auto-save it and return its store id. Returns `None` when the live login
/// is already saved or there is no live login.
fn auto_save_unsaved_live_codex_account() -> Result<Option<String>, Box<dyn Error>> {
    let Some(live) = read_codex_live()? else {
        return Ok(None);
    };
    let mut store = read_store()?;
    let id = live.id.clone();
    if store
        .iter()
        .any(|e| str_field(e, "app") == Some("codex") && str_field(e, "id") == Some(id.as_str()))
    {
        return Ok(Some(id));
    }
    let entry = serde_json::json!({
        "id": id, "app": "codex",
        "name": live.name, "email": live.email, "plan": live.plan,
        "payload": live.doc, "savedAt": now_rfc3339(),
    });
    store.push(entry);
    write_store(store)?;
    Ok(Some(id))
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
/// changes, or when historical data needs a one-time cleanup, and add an arm
/// to `migrate_account_entries`. v1 is the original baseline.
pub const ACCOUNTS_SCHEMA_VERSION: u64 = 3;

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
    let migrated = migrate_account_entries(version, entries);
    // Persist the upgrade once so historical data on disk is actually rewritten
    // to the new shape, not just fixed in memory on every read. After the first
    // write the file carries the current version, so this stops firing.
    // BEST-EFFORT: a read must still return its data if the write-back fails
    // (disk full / read-only fs) — the in-memory migration already made the
    // data correct, and the next successful write will re-attempt the upgrade.
    if version < ACCOUNTS_SCHEMA_VERSION {
        if let Err(e) = write_store(migrated.clone()) {
            log::warn!("Failed to persist accounts.json schema upgrade: {e}");
        }
    }
    Ok(migrated)
}

/// Upgrade account entries written by an older schema version to the
/// current shape. Add an arm when bumping `ACCOUNTS_SCHEMA_VERSION`.
///
/// Arms apply in sequence, each gated on the version that introduced it, so a
/// v1 file walks through every later arm too. Don't `return` out of one — that
/// would make the oldest data skip the newest cleanups (each arm is written to
/// be idempotent, so falling through them all is safe).
fn migrate_account_entries(version: u64, mut entries: Vec<JsonValue>) -> Vec<JsonValue> {
    if version < 2 {
        // v1 stored `payload` as a JSON-ENCODED STRING; v2 stores the parsed
        // object. Convert each historical entry in place (leaving already-object
        // payloads untouched so the pass is idempotent).
        entries = entries
            .into_iter()
            .map(|mut e| {
                let parsed = e
                    .get("payload")
                    .and_then(|p| p.as_str())
                    .and_then(|s| serde_json::from_str::<JsonValue>(s).ok());
                if let Some(obj) = parsed {
                    e["payload"] = obj;
                }
                e
            })
            .collect();
    }
    if version < 3 {
        // Snapshots taken before the provider/account split captured the whole
        // auth.json, so one taken while a custom provider was active still
        // holds that provider's `OPENAI_API_KEY`. `switch_codex` no longer
        // writes it out (it carries the live file's fields over instead), but
        // the key itself shouldn't keep sitting in accounts.json — drop it.
        entries = entries
            .into_iter()
            .map(|mut e| {
                if let Some(payload) = e.get_mut("payload") {
                    strip_provider_fields(payload);
                }
                e
            })
            .collect();
    }
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
    /// The parsed auth.json, stored as the snapshot payload (a JSON object,
    /// NOT a JSON-encoded string) — already MINUS `PROVIDER_OWNED_AUTH_FIELDS`,
    /// stripped once in `read_codex_live` so no consumer has to remember to.
    doc: JsonValue,
}

// ===================================================================
// Claude Code — full multi-account management (Phase 2)
// ===================================================================
//
// The credential itself lives behind `claude_auth` (Keychain-first on
// macOS, `.credentials.json` elsewhere — see that module for why writing
// only the file cannot work on macOS). The account's IDENTITY (email /
// display name / uuid) is NOT in the credential: Claude keeps it in
// `~/.claude.json` under `oauthAccount`, populated by `/api/oauth/profile`
// at login and never re-derived from the token. A snapshot therefore
// carries BOTH — `payload: { credentials, oauthAccount }` — and a switch
// restores both, otherwise the CLI (and Termory's own account list) would
// keep showing the previous login's name.
//
// Writing `oauthAccount` back is a deliberate, narrow exception to the
// "Termory never writes ~/.claude.json" rule: the switch is exactly the
// user-triggered credential overwrite this feature exists for, and leaving
// the identity stale is the worse outcome. Only that ONE key is replaced;
// everything else in the document is preserved byte-for-byte at the JSON
// level (preserve_order keeps the key order), and the write is atomic.
//
// Like Codex, a switch validates the snapshot by refreshing its tokens in
// memory first (`refresh_claude_doc_tokens` — official endpoint/body per
// services/oauth/client.ts:146). An `AuthFailure` flags the entry
// `needsRelogin` BACKEND-side (Codex leaves that to the frontend; here a
// string Err can't distinguish a dead token from a locked-Keychain write
// error, and flagging the latter would trap the row) and leaves the live
// credential untouched; transient failures fall back to the snapshot
// verbatim. Anthropic ROTATES the refresh token on every refresh (the
// #30337 comment in fallbackStorage.ts), so a successful refresh persists
// to the STORE before the live write — see
// `persist_refreshed_claude_snapshot`.

/// Parsed view of the live Claude login: the credential document plus the
/// identity read from `~/.claude.json`.
struct ClaudeLive {
    /// Stable per-account identity: `oauthAccount.accountUuid` → email.
    /// No hash fallback — the credential doc rotates on every token refresh,
    /// so hashing it would mint a new "account" per refresh. A login without
    /// identity (no oauthAccount) is treated as not listable/savable.
    id: String,
    name: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// `{"claudeAiOauth": {...}}` as read from the store.
    credentials: JsonValue,
    /// The `oauthAccount` object, verbatim.
    oauth_account: JsonValue,
}

/// The global config file holding `oauthAccount` — official resolution
/// (`getGlobalClaudeFile`, env.ts:14-26): a legacy `<config-dir>/.config.json`
/// wins when it exists; else `$CLAUDE_CONFIG_DIR/.claude.json` when the var
/// is set, else `~/.claude.json`. Hardcoding the home form paired one
/// profile's credentials with another profile's identity under a custom
/// `CLAUDE_CONFIG_DIR` — and worse, the switch then WROTE the identity into
/// a file that profile's claude never reads.
fn claude_json_path() -> Result<std::path::PathBuf, Box<dyn Error>> {
    let config_dir = crate::claude_auth::config_dir().ok_or("home directory not available")?;
    let legacy = config_dir.join(".config.json");
    if legacy.exists() {
        return Ok(legacy);
    }
    match std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(_) => Ok(config_dir.join(".claude.json")),
        None => {
            let home = crate::home_dir().ok_or("home directory not available")?;
            Ok(home.join(".claude.json"))
        }
    }
}

/// Display-quality read (30s Keychain cache) — the tray / account list /
/// quota path. Anything that PERSISTS the result must use
/// [`read_claude_live_uncached`]: an external writer (a running claude
/// rotating tokens, the `claude auth login` child) can change the Keychain
/// under the cache, and snapshotting a stale doc stores a dead refresh
/// token.
fn read_claude_live() -> Result<Option<ClaudeLive>, Box<dyn Error>> {
    claude_live_from(crate::claude_auth::read_credentials())
}

/// Snapshot-quality read — bypasses the Keychain cache. See
/// `claude_auth::read_credentials_uncached` for why every persisted read
/// must pay the spawn.
fn read_claude_live_uncached() -> Result<Option<ClaudeLive>, Box<dyn Error>> {
    claude_live_from(crate::claude_auth::read_credentials_uncached())
}

fn claude_live_from(credentials: Option<JsonValue>) -> Result<Option<ClaudeLive>, Box<dyn Error>> {
    let Some(credentials) = credentials else {
        return Ok(None);
    };
    // A credential without an OAuth block is an API-key-era file — not a
    // login this feature manages.
    let Some(oauth) = credentials
        .get("claudeAiOauth")
        .or_else(|| credentials.get("claude.ai_oauth"))
        .filter(|v| !v.is_null())
    else {
        return Ok(None);
    };
    let plan = oauth
        .get("subscriptionType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(title_case_plan);

    let path = claude_json_path()?;
    let account = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<JsonValue>(&raw).ok())
        .and_then(|doc| doc.get("oauthAccount").cloned())
        .filter(|v| v.is_object());
    let Some(account) = account else {
        // Logged in but no profile record (very old login) — no stable
        // identity to key a snapshot on.
        return Ok(None);
    };
    let field = |k: &str| {
        account
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let email = field("emailAddress");
    let Some(id) = field("accountUuid").or_else(|| email.clone()) else {
        return Ok(None);
    };
    Ok(Some(ClaudeLive {
        id,
        name: field("displayName"),
        email,
        plan,
        credentials,
        oauth_account: account,
    }))
}

fn list_claude_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let live = read_claude_live()?;
    let current_id = live.as_ref().map(|l| l.id.as_str());
    let store = read_store()?;

    let mut accounts = Vec::new();
    let mut current_saved = false;
    for e in &store {
        if str_field(e, "app") != Some("claude") {
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
        storage_warning: None,
    })
}

fn save_claude_account() -> Result<(), Box<dyn Error>> {
    // UNCACHED: this is the one function that persists the live credential
    // into the store. Reading it through the 30s cache once snapshotted a
    // pre-login doc after a fast (<30s) browser login — pairing the NEW
    // account's identity with the OLD account's tokens.
    let live = read_claude_live_uncached()?
        .ok_or("No Claude login with account info found to save (run `claude` and log in first)")?;

    let id = live.id.clone();
    let mut store = read_store()?;

    let entry = json!({
        "id": id,
        "app": "claude",
        // The list row needs a non-empty primary label; email is the
        // reliable one (displayName is optional on AccountInfo).
        "name": live.name.clone().or_else(|| live.email.clone()).unwrap_or_else(|| "Claude".into()),
        "email": live.email,
        "plan": live.plan,
        "payload": {
            "credentials": live.credentials,
            "oauthAccount": live.oauth_account,
        },
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

/// Set (`Some`) or remove (`None`) ONLY the `oauthAccount` key of
/// `~/.claude.json`, leaving every other key untouched. Atomic (tmp +
/// rename); the file's existing permission bits are preserved — it is
/// Claude's own file (typically 0644), not a Termory 0600 store. A missing
/// file with `None` is a no-op (don't mint an empty `{}` config).
fn update_claude_oauth_account(account: Option<&JsonValue>) -> Result<(), Box<dyn Error>> {
    let path = claude_json_path()?;
    // A read or PARSE failure must abort, never degrade to an empty object:
    // this file is Claude's whole global config (projects, permissions,
    // onboarding), a running claude can leave it mid-write, and writing the
    // degraded doc back would TRUNCATE it to just `oauthAccount`. Same rule
    // as `claude_desktop::read_json_or_empty` ("PROPAGATES a parse error …
    // never truncates"). Only a genuinely missing file starts fresh.
    let mut doc = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<JsonValue>(&raw).map_err(|e| {
            format!(
                "{} is not valid JSON — refusing to rewrite it (is claude mid-write?): {e}",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if account.is_none() {
                return Ok(()); // nothing to remove, don't mint an empty config
            }
            JsonValue::Object(serde_json::Map::new())
        }
        Err(e) => return Err(format!("Failed to read {}: {e}", path.display()).into()),
    };
    if !doc.is_object() {
        return Err(format!(
            "{} does not hold a JSON object — refusing to rewrite it",
            path.display()
        )
        .into());
    }
    #[cfg(unix)]
    let prior_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&path)
            .ok()
            .map(|m| m.permissions().mode())
    };
    if let JsonValue::Object(o) = &mut doc {
        match account {
            Some(a) => {
                o.insert("oauthAccount".into(), a.clone());
            }
            None => {
                o.remove("oauthAccount");
            }
        }
    }
    atomic_write_0600(&path, serde_json::to_string(&doc)?.as_bytes())?;
    #[cfg(unix)]
    if let Some(mode) = prior_mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
    }
    Ok(())
}

fn write_claude_oauth_account(account: &JsonValue) -> Result<(), Box<dyn Error>> {
    update_claude_oauth_account(Some(account))
}

/// The identity a Claude snapshot payload keys on — same precedence as
/// `read_claude_live` (`accountUuid` → email).
fn claude_payload_id(payload: &JsonValue) -> Option<String> {
    let account = payload.get("oauthAccount")?;
    ["accountUuid", "emailAddress"]
        .iter()
        .find_map(|k| account.get(k).and_then(|v| v.as_str()))
        .map(String::from)
}

/// POST the saved refresh_token to Claude's token endpoint and merge the
/// refreshed tokens into the credential doc's `claudeAiOauth` block — the
/// Claude mirror of `refresh_doc_tokens`. Endpoint + client_id + body shape
/// per the official `refreshOAuthToken` (services/oauth/client.ts:146-169,
/// constants/oauth.ts:91,99): plain JSON POST, 15s timeout, `scope` = the
/// full claude.ai set when the snapshot is a claude.ai login (the backend
/// allows scope expansion on refresh — official comment), else the
/// snapshot's own scopes.
///
/// On success the doc is mutated: accessToken / refreshToken (the server
/// ROTATES it; absent in the response = keep the old one, official default
/// `refresh_token: newRefreshToken = refreshToken`) / `expiresAt` =
/// now + expires_in seconds, in MILLIS (official `Date.now() + expiresIn *
/// 1000`) / scopes reparsed. `subscriptionType` etc. stay untouched.
/// On failure the doc is unchanged; 4xx-except-429 = AuthFailure.
async fn refresh_claude_doc_tokens(doc: &mut JsonValue) -> Result<(), RefreshError> {
    const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
    const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    const CLAUDE_AI_OAUTH_SCOPES: &[&str] = &[
        "user:profile",
        "user:inference",
        "user:sessions:claude_code",
        "user:mcp_servers",
        "user:file_upload",
    ];

    let oauth = doc
        .get("claudeAiOauth")
        .ok_or_else(|| RefreshError::Transient("No claudeAiOauth block in credential".into()))?;
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RefreshError::Transient("No refreshToken in saved credential".into()))?
        .to_string();
    let stored_scopes: Vec<String> = oauth
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // `shouldUseClaudeAIAuth` = scopes include user:inference
    // (services/oauth/client.ts:38); for those the official sends the full
    // default set so scope expansion applies, otherwise the token's own.
    let scope = if stored_scopes.iter().any(|s| s == "user:inference") || stored_scopes.is_empty() {
        CLAUDE_AI_OAUTH_SCOPES.join(" ")
    } else {
        stored_scopes.join(" ")
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    let resp = client
        .post(CLAUDE_TOKEN_URL)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_CLIENT_ID,
            "scope": scope,
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
        // Same split as the Codex arm: 4xx (except 429) = the refresh token
        // is dead (revoked / rotated away) → permanent; else transient.
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
    let access = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RefreshError::Transient("Refresh response missing access_token".into()))?
        .to_string();
    let expires_in = body.get("expires_in").and_then(|v| v.as_i64());
    let new_rt = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let new_scopes = body.get("scope").and_then(|v| v.as_str()).map(|s| {
        JsonValue::Array(
            s.split(' ')
                .filter(|p| !p.is_empty())
                .map(|p| JsonValue::String(p.into()))
                .collect(),
        )
    });

    if let Some(oauth) = doc.get_mut("claudeAiOauth").and_then(|v| v.as_object_mut()) {
        oauth.insert("accessToken".into(), JsonValue::String(access));
        if let Some(rt) = new_rt {
            oauth.insert("refreshToken".into(), JsonValue::String(rt));
        }
        if let Some(secs) = expires_in {
            let now_ms = chrono::Utc::now().timestamp_millis();
            oauth.insert("expiresAt".into(), JsonValue::from(now_ms + secs * 1000));
        }
        if let Some(scopes) = new_scopes {
            oauth.insert("scopes".into(), scopes);
        }
    }
    Ok(())
}

/// Overwrite the STORED snapshot's credential payload for `id` (app claude).
/// Called right after a successful refresh, BEFORE the live write: the
/// refresh rotated the refresh_token server-side, so from that moment the
/// in-memory doc holds the only working copy — parking it in the store first
/// means a failed live write (fs error) loses nothing; the retry just works.
fn persist_refreshed_claude_snapshot(id: &str, cred: &JsonValue) -> Result<(), Box<dyn Error>> {
    let mut store = read_store()?;
    if let Some(entry) = store
        .iter_mut()
        .find(|e| str_field(e, "app") == Some("claude") && str_field(e, "id") == Some(id))
    {
        if let Some(payload) = entry.get_mut("payload") {
            payload["credentials"] = cred.clone();
        }
        write_store(store)?;
    }
    Ok(())
}

/// Restore a saved Claude snapshot: validate/refresh the tokens in memory
/// first (mirrors `switch_codex` — an `AuthFailure` flags the entry
/// `needsRelogin` and leaves the live credential untouched; a transient
/// failure falls back to the snapshot as-is so an outage doesn't block
/// switching), then write the credential (Keychain/file via `claude_auth`)
/// and the matching `oauthAccount` identity.
async fn switch_claude(payload: &JsonValue) -> Result<(), Box<dyn Error>> {
    let credentials = payload
        .get("credentials")
        .filter(|v| v.is_object())
        .ok_or("Saved Claude account has no credential payload")?;
    let account = payload
        .get("oauthAccount")
        .filter(|v| v.is_object())
        .ok_or("Saved Claude account has no oauthAccount payload")?;

    // Fail fast on a locked Keychain BEFORE the refresh: refreshing rotates
    // the snapshot's refresh_token server-side, so a refresh followed by an
    // un-writable Keychain would burn the only working copy.
    #[cfg(all(target_os = "macos", not(test)))]
    if crate::claude_auth::keychain_locked() {
        return Err(
            "macOS keychain is locked — unlock it (open Keychain Access) and try again".into(),
        );
    }

    // Re-snapshot the OUTGOING login first when it's already in the store:
    // Claude rotates the refresh token on every refresh, so the entry saved
    // days ago likely holds a dead RT — the live credential is the only copy
    // that still works. Only refreshes an EXISTING entry (never adds one; an
    // unsaved live login stays the user's explicit choice, guarded by the
    // page's confirm warning / the tray's auto-save). UNCACHED read: the
    // rotation this step defends against is exactly what makes a ≤30s-stale
    // cached doc wrong here.
    if let Ok(Some(live)) = read_claude_live_uncached() {
        let store = read_store()?;
        if store
            .iter()
            .any(|e| str_field(e, "id") == Some(live.id.as_str()))
        {
            save_claude_account()?;
        }
    }

    let mut cred = credentials.clone();
    match refresh_claude_doc_tokens(&mut cred).await {
        Ok(()) => {
            // Rotated tokens exist only in memory — park them in the store
            // before anything else can fail.
            if let Some(id) = claude_payload_id(payload) {
                persist_refreshed_claude_snapshot(&id, &cred)?;
            }
        }
        Err(RefreshError::AuthFailure(e)) => {
            // The snapshot's refresh token is dead. Flag the entry HERE
            // (unlike Codex, where the frontend flags): the frontend can't
            // tell an auth failure from a write failure through the string
            // Err, and flagging a locked-keychain victim would trap the row
            // behind a disabled Switch button.
            if let Some(id) = claude_payload_id(payload) {
                let _ = mark_account_relogin(&id, true);
            }
            return Err(format!("Account needs re-login (refresh token revoked): {e}").into());
        }
        Err(RefreshError::Transient(_)) => {} // outage / rate-limit / no RT — proceed with snapshot
    }

    crate::claude_auth::write_credentials(&cred)?;
    // Credential landed; a failure past this point leaves creds=B with
    // identity=A on display. Surface it rather than pretending success.
    write_claude_oauth_account(account)
        .map_err(|e| format!("Credentials switched, but updating ~/.claude.json failed: {e}"))?;
    // Upsert the snapshot so `savedAt` reflects this switch (mirrors
    // switch_codex's tail).
    save_claude_account()?;
    Ok(())
}

/// Tauri-managed state allowing `cancel_claude_login` to abort an in-flight
/// `login_and_save_claude_account` — the Claude sibling of `CodexLoginCancel`.
pub struct ClaudeLoginCancel(pub std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>);

/// Restore the pre-login Claude state after a failed / cancelled add-account
/// flow. `None` originals mean "was absent" and clear the tier.
fn restore_claude_auth(original_cred: Option<&JsonValue>, original_oauth: Option<&JsonValue>) {
    match original_cred {
        Some(doc) => {
            if let Err(e) = crate::claude_auth::write_credentials(doc) {
                log::error!("claude login rollback: restoring credentials failed: {e}");
            }
        }
        None => {
            let _ = crate::claude_auth::delete_credentials();
        }
    }
    if let Err(e) = update_claude_oauth_account(original_oauth) {
        log::error!("claude login rollback: restoring oauthAccount failed: {e}");
    }
}

/// Add a Claude account — the same shape as `login_and_save_codex_account`:
/// `claude auth login` is a HEADLESS subcommand (main.tsx:5747 →
/// cli/handlers/auth.ts `authLogin`): it opens the browser, prints the
/// fallback URL on STDOUT ("If the browser didn't open, visit: {url}"),
/// captures the OAuth callback on a local server, installs the new
/// credential + `oauthAccount` (`installOAuthTokens`), and EXITS. So the
/// flow is: auto-save the live login → snapshot originals → spawn → wait
/// (5-min timeout, cancellable) → save the new account → restore the
/// previous one. Returns the new account's store id.
///
/// Unlike Codex there is NO pre-clear of the live credential:
/// `installOAuthTokens` starts with `performLogout` (logout.tsx:20), which
/// is PURELY LOCAL — `removeApiKey` + `secureStorage.delete()` + cache/
/// config clears, no server-side token revocation — so saved snapshots'
/// tokens survive the new login untouched (the pre-clear Codex needs to
/// dodge its `logout_with_revoke` has nothing to dodge here). The rollback
/// originals still matter: a login killed AFTER the exchange may have
/// already wiped/replaced the local state.
pub async fn login_and_save_claude_account(
    app: tauri::AppHandle,
    cancel_state: &ClaudeLoginCancel,
) -> Result<String, String> {
    // Fail fast on a locked Keychain — claude's own login and the restore
    // both write it.
    #[cfg(all(target_os = "macos", not(test)))]
    if crate::claude_auth::keychain_locked() {
        return Err(
            "macOS keychain is locked — unlock it (open Keychain Access) and try again".into(),
        );
    }

    // Reserve the cancel slot (also the re-entrancy guard); self-clears on
    // every exit path via Drop.
    let (_login_slot, cancel_notify) = LoginSlot::reserve(&cancel_state.0)?;

    // Snapshot the live login UNCONDITIONALLY (its id is what gets restored
    // afterwards) — `claude auth login` wipes local storage (`performLogout`)
    // before installing the new one, and that wipe destroys the only FRESH
    // copy of the outgoing tokens: Claude rotates the refresh token on every
    // refresh, so an entry saved days ago holds a dead RT and the restore
    // below would AuthFailure into needsRelogin. Same rule, same reason as
    // `switch_claude`'s re-snapshot-outgoing step — an only-if-missing
    // auto-save is NOT enough here.
    let prev_active_id = match read_claude_live_uncached().map_err(|e| e.to_string())? {
        Some(live) => {
            save_claude_account().map_err(|e| e.to_string())?;
            Some(live.id)
        }
        None => None,
    };

    // Originals for rollback (cancel / timeout / failure after the wipe).
    let original_cred = crate::claude_auth::read_credentials();
    let original_oauth = std::fs::read_to_string(claude_json_path().map_err(|e| e.to_string())?)
        .ok()
        .and_then(|raw| serde_json::from_str::<JsonValue>(&raw).ok())
        .and_then(|doc| doc.get("oauthAccount").cloned())
        .filter(|v| v.is_object());

    // Spawn `claude auth login`. Resolve the real binary via the same scan
    // detection uses (a bare name misses `.cmd` shims on Windows — see the
    // codex spawn above), PATH augmented so the shim finds its runtime.
    let resolved = crate::providers::find_cli_binary("claude");
    let program: std::ffi::OsString = resolved
        .as_deref()
        .map(|p| p.as_os_str().to_os_string())
        .unwrap_or_else(|| "claude".into());
    let mut cmd = tokio::process::Command::new(&program);
    if let Some(path) = resolved
        .as_deref()
        .and_then(crate::providers::augmented_path_for)
    {
        cmd.env("PATH", path);
    }
    cmd.args(["auth", "login"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(crate::providers::CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "claude is not installed or not in PATH".to_string()
            } else {
                format!("Failed to launch claude: {e}")
            };
            return Err(msg);
        }
    };

    // stdout: the auth URL rides a "…visit: https://…" line
    // (cli/handlers/auth.ts:199) — emit it as soon as it appears so the
    // frontend can show the copyable-URL dialog without waiting.
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let app_clone = app.clone();
    let stdout_task: tokio::task::JoinHandle<String> = tokio::spawn(async move {
        use tauri::Emitter as _;
        use tokio::io::AsyncBufReadExt as _;
        let reader = tokio::io::BufReader::new(stdout_pipe);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if let Some(url) = trimmed
                .split_once("visit:")
                .map(|(_, rest)| rest.trim())
                .filter(|u| u.starts_with("https://"))
            {
                let _ = app_clone.emit(CLAUDE_LOGIN_URL_EVENT, url.to_string());
            }
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(trimmed);
        }
        collected
    });
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_task: tokio::task::JoinHandle<String> = tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut buf = String::new();
        let mut reader = stderr_pipe;
        let _ = reader.read_to_string(&mut buf).await;
        buf
    });

    // Wait up to 5 minutes for the browser roundtrip to complete.
    let status = tokio::select! {
        r = child.wait() => match r {
            Ok(s) => s,
            Err(e) => {
                stdout_task.abort();
                stderr_task.abort();
                restore_claude_auth(original_cred.as_ref(), original_oauth.as_ref());
                return Err(format!("claude auth login error: {e}"));
            }
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            restore_claude_auth(original_cred.as_ref(), original_oauth.as_ref());
            return Err("claude login timed out after 5 minutes".into());
        },
        _ = cancel_notify.notified() => {
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            restore_claude_auth(original_cred.as_ref(), original_oauth.as_ref());
            return Err("Login cancelled".into());
        },
    };

    let stdout_msg = stdout_task.await.unwrap_or_default();
    let stderr_msg = stderr_task.await.unwrap_or_default();

    if !status.success() {
        restore_claude_auth(original_cred.as_ref(), original_oauth.as_ref());
        let detail = if stderr_msg.trim().is_empty() {
            stdout_msg
        } else {
            stderr_msg
        };
        return Err(format!(
            "claude auth login failed (exit {}): {}",
            status.code().unwrap_or(-1),
            detail.trim()
        ));
    }

    // The child WROTE the Keychain from outside this process — no Termory
    // write path ran, so no invalidation happened. Drop the cache before
    // anything reads: with a fast (<30s) browser login the cache still
    // holds the PRE-login doc, and the save below would snapshot it (on a
    // first login it held None, and the "could not save" rollback then
    // deleted the fresh login outright).
    crate::claude_auth::invalidate_credentials_cache();

    // Save the new account (reads the fresh credential claude just wrote).
    if let Err(e) = save_claude_account() {
        restore_claude_auth(original_cred.as_ref(), original_oauth.as_ref());
        return Err(format!("Login succeeded but could not save account: {e}"));
    }

    // Capture the new account's id before restoring the previous login.
    let new_id = read_claude_live()
        .ok()
        .flatten()
        .map(|live| live.id)
        .unwrap_or_default();

    // Restore the previously active account (same tail as the codex flow —
    // the login process has exited, so nothing is running on the new login).
    if let Some(prev_id) = prev_active_id {
        if let Err(e) = switch_account(prev_id.clone()).await {
            // Don't fail the overall operation — the new account was saved.
            // NO unconditional needsRelogin here: `switch_claude` already
            // flags the entry itself when the failure is an AuthFailure, and
            // everything else (locked Keychain mid-flow, fs error) is a
            // WRITE failure — flagging those would trap a healthy account
            // behind a disabled Switch button (the same misdiagnosis rule
            // as the frontend's per-app catch split).
            log::warn!("Failed to restore previous claude account {prev_id}: {e}");
        }
    }

    Ok(new_id)
}

/// Fire the cancel notify for an in-flight `login_and_save_claude_account`.
pub async fn cancel_claude_login(cancel_state: &ClaudeLoginCancel) -> Result<(), String> {
    // `as_ref`, NOT `take` (mirrors cancel_codex_login): the slot doubles as
    // the re-entrancy guard, and it must stay occupied until the CANCELLED
    // flow finishes its kill + rollback and its LoginSlot Drop clears it. A
    // take() here opened a window where a new login could reserve the slot
    // while the old flow was still rolling back — and the old flow's Drop
    // then wiped the NEW login's notify, leaving it uncancellable.
    match cancel_state.0.lock().unwrap().as_ref() {
        Some(n) => {
            n.notify_one();
            Ok(())
        }
        None => Err("No claude login in progress".into()),
    }
}

// ===================================================================
// Gemini account (display-only — decodes id_token from oauth_creds.json)
// ===================================================================

// ===================================================================
// Grok Build account (display-only — plain fields in ~/.grok/auth.json)
// ===================================================================

/// Grok Build's auth.json stores the login as a scope-keyed object
/// (`https://auth.x.ai::<uuid>`) with PLAIN `email` / `first_name` /
/// `last_name` fields (verified on the real 0.2.93 install) — no JWT
/// decode needed. Display-only, like Claude / Gemini.
fn list_grok_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let empty = || AccountsState {
        current: None,
        accounts: Vec::new(),
        storage_warning: None,
    };
    let Some(path) = crate::providers::grok_home_dir().map(|d| d.join("auth.json")) else {
        return Ok(empty());
    };
    if !path.exists() {
        return Ok(empty());
    }
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or(serde_json::Value::Null);
    let Some(entries) = doc.as_object() else {
        return Ok(empty());
    };
    // First auth.x.ai-scoped entry = the live login.
    let Some(entry) = entries
        .iter()
        .find(|(k, _)| k.starts_with("https://auth.x.ai::"))
        .map(|(_, v)| v)
    else {
        return Ok(empty());
    };
    let field = |k: &str| {
        entry
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let email = field("email");
    let name = match (field("first_name"), field("last_name")) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (Some(f), None) => Some(f),
        (None, Some(l)) => Some(l),
        (None, None) => None,
    };
    if email.is_none() && name.is_none() {
        return Ok(empty());
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

fn list_gemini_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let home = crate::home_dir().ok_or("home directory not available")?;
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
    let mut doc: JsonValue = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse ~/.codex/auth.json: {e}"))?;

    // Scoped so the borrow of `doc` ends before it is stripped below.
    let (id_info, account_id) = {
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
        // login server alongside the token bundle). Mirrors the official
        // priority in local_chatgpt_auth.rs:37-40: tokens.account_id →
        // chatgpt_account_id.
        let account_id = tokens
            .get("account_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        (id_info, account_id)
    };

    let name = id_info.as_ref().and_then(|i| i.name.clone());
    let email = id_info.as_ref().and_then(|i| i.email.clone());
    let plan = id_info.as_ref().and_then(|i| i.plan.clone());
    let chatgpt_account_id = id_info.as_ref().and_then(|i| i.chatgpt_account_id.clone());

    // Strip HERE, the single point every consumer of `doc` goes through, so no
    // caller has to remember to — and so nothing derived from the document as
    // a whole (the id fallback right below) can vary with the active provider.
    strip_provider_fields(&mut doc);

    let id = account_id
        .or(chatgpt_account_id)
        .or_else(|| email.clone())
        // Last resort, only when the login carries no identifying claim at
        // all. Hashes the STRIPPED document, never the file text: otherwise
        // activating a provider would change the hash and split one login
        // into two accounts.
        .unwrap_or_else(|| stable_hash(&doc.to_string()).to_string());

    Ok(Some(CodexLive {
        id,
        name,
        email,
        plan,
        doc,
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
        "payload": live.doc,
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
    let codex_version = crate::providers::detect_cli_version(crate::providers::CliApp::Codex)
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
async fn switch_codex(payload: &JsonValue) -> Result<(), Box<dyn Error>> {
    // Payload is the parsed auth.json object (current schema). Tolerate the
    // legacy JSON-encoded-string form too, in case an un-migrated entry slips
    // through — the on-load migration normally converts it first.
    let mut doc: JsonValue = match payload {
        JsonValue::String(s) => serde_json::from_str(s)
            .map_err(|e| format!("Saved Codex credential is corrupt: {e}"))?,
        obj => obj.clone(),
    };

    // Re-snapshot the OUTGOING login first when it's already in the store —
    // same protection as switch_claude's: OpenAI can rotate the refresh
    // token on refresh, so an entry saved days ago may hold a dead RT while
    // the live auth.json has the only working copy; overwriting it without
    // recapturing turns the next switch-back into a needsRelogin. Only
    // refreshes an EXISTING entry (never adds one — an unsaved live login
    // stays guarded by the page's confirm warning / the tray's auto-save).
    // No cache concern here: auth.json is read fresh every time.
    if let Ok(Some(live)) = read_codex_live() {
        let store = read_store()?;
        if store.iter().any(|e| {
            str_field(e, "app") == Some("codex") && str_field(e, "id") == Some(live.id.as_str())
        }) {
            save_codex_account()?;
        }
    }

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
    // Switching the account must not move the provider. `auth_mode` /
    // `OPENAI_API_KEY` share this file but belong to provider management, so
    // they are taken from the CURRENT auth.json, never from the snapshot —
    // otherwise switching accounts would re-apply whichever third-party key
    // happened to be live when that snapshot was taken (or wipe an active
    // provider, when it was taken while Official was in use).
    carry_over_provider_fields(&path, &mut doc);
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
    let home = crate::home_dir()?;
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
pub(crate) fn atomic_write_0600(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
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
    fn saved_payload_is_a_json_object_not_a_string() {
        let _g = lock_home();
        let tmp = tempdir("payload-object");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();

        let path = tmp.join(".termory/accounts.json");
        let raw: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let payload = raw.pointer("/accounts/0/payload").unwrap();
        assert!(
            payload.is_object(),
            "payload must be a JSON object, not a stringified one: {payload:?}"
        );
        // The auth.json contents live inside the object verbatim.
        assert_eq!(
            payload
                .pointer("/tokens/account_id")
                .and_then(|v| v.as_str()),
            Some("acct-a")
        );
    }

    #[tokio::test]
    async fn migrates_v1_string_payload_to_object_and_persists() {
        let _g = lock_home();
        let tmp = tempdir("migrate-payload");
        let _h = override_home(&tmp);

        // Hand-write a v1 store whose `payload` is a JSON-ENCODED STRING (the
        // old shape) so the on-load migration has something to upgrade.
        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        let auth = std::fs::read_to_string(tmp.join(".codex/auth.json")).unwrap();
        let store_dir = tmp.join(".termory");
        std::fs::create_dir_all(&store_dir).unwrap();
        let v1 = json!({
            "version": 1,
            "accounts": [{
                "id": "acct-a", "app": "codex",
                "name": "A", "email": "a@example.com", "plan": "Pro",
                "payload": auth,            // <-- a STRING, the legacy shape
                "savedAt": "2026-06-27T00:00:00Z",
            }],
        });
        let store_path = store_dir.join("accounts.json");
        std::fs::write(&store_path, serde_json::to_string_pretty(&v1).unwrap()).unwrap();

        // A plain read migrates in memory AND rewrites the file to the current
        // version + an object payload.
        let entries = read_store().unwrap();
        assert!(
            entries[0]["payload"].is_object(),
            "in-memory payload upgraded"
        );

        let on_disk: JsonValue =
            serde_json::from_slice(&std::fs::read(&store_path).unwrap()).unwrap();
        assert_eq!(
            on_disk.pointer("/version").and_then(|v| v.as_u64()),
            Some(ACCOUNTS_SCHEMA_VERSION),
            "historical file stamped to the current version"
        );
        assert!(
            on_disk.pointer("/accounts/0/payload").unwrap().is_object(),
            "historical payload rewritten as an object on disk"
        );

        // The migrated snapshot still switches correctly (writes auth.json back).
        std::fs::remove_file(tmp.join(".codex/auth.json")).unwrap();
        switch_account("acct-a".to_string()).await.unwrap();
        let restored = std::fs::read_to_string(tmp.join(".codex/auth.json")).unwrap();
        assert_eq!(restored, auth, "switch restores the exact auth.json");
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

    /// Simulate `providers::activate_codex`: it merges the custom provider's
    /// credentials into the SAME auth.json and deliberately leaves the OAuth
    /// tokens beside them, so the login still reads as a valid account.
    fn activate_custom_provider(home: &Path, key: &str) {
        let path = home.join(".codex/auth.json");
        let mut doc: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["auth_mode"] = json!("apikey");
        doc["OPENAI_API_KEY"] = json!(key);
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn switching_accounts_leaves_the_active_provider_credentials_alone() {
        let _g = lock_home();
        let tmp = tempdir("provider-untouched");
        let _h = override_home(&tmp);
        let path = tmp.join(".codex/auth.json");

        // A and B both saved while Official was in use (clean snapshots).
        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        write_codex_auth(&tmp, "b@example.com", "plus", "acct-b");
        save_current_account(CliApp::Codex).unwrap();

        // The user then points Codex at a third-party provider.
        activate_custom_provider(&tmp, "sk-third-party");

        switch_account("acct-a".to_string()).await.unwrap();

        let after: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            after.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
            Some("sk-third-party"),
            "an account switch must not move the active provider's key"
        );
        assert_eq!(
            after.get("auth_mode").and_then(|v| v.as_str()),
            Some("apikey"),
            "nor its mode marker"
        );
        assert_eq!(
            after.pointer("/tokens/account_id").and_then(|v| v.as_str()),
            Some("acct-a"),
            "while the official login really did switch"
        );
    }

    #[tokio::test]
    async fn switching_to_a_snapshot_holding_a_key_does_not_write_it_back() {
        let _g = lock_home();
        let tmp = tempdir("dirty-snapshot");
        let _h = override_home(&tmp);
        let path = tmp.join(".codex/auth.json");

        // A snapshot from before this fix: taken while a provider was active,
        // so its payload still carries that key.
        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        activate_custom_provider(&tmp, "sk-stale");
        let dirty: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        write_store(vec![json!({
            "id": "acct-a", "app": "codex",
            "name": JsonValue::Null, "email": "a@example.com", "plan": "Pro",
            "payload": dirty,
            "savedAt": "2026-06-27T00:00:00Z",
        })])
        .unwrap();

        // Live is Official again (a plain login, no provider fields).
        write_codex_auth(&tmp, "b@example.com", "plus", "acct-b");

        switch_account("acct-a".to_string()).await.unwrap();

        let after: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            after.get("OPENAI_API_KEY").is_none(),
            "a stale key inside an old snapshot must not be resurrected"
        );
        assert!(after.get("auth_mode").is_none());
        assert_eq!(
            after.pointer("/tokens/account_id").and_then(|v| v.as_str()),
            Some("acct-a"),
        );
    }

    /// One login must stay ONE account when a provider is activated on top of
    /// it. Exercised on the LAST-RESORT id branch — a login carrying no
    /// identifying claim, whose id is a hash of the document — because that is
    /// the only branch the provider's fields can reach. (The normal branch
    /// reads `tokens.account_id`, which they can't touch, so asserting this
    /// there passes no matter how the id is derived.)
    #[test]
    fn a_hashed_login_id_does_not_change_when_a_provider_is_activated() {
        let _g = lock_home();
        let tmp = tempdir("identity-fallback");
        let _h = override_home(&tmp);

        // `tokens` present (so this reads as a login) but with no account_id
        // and no id_token to decode.
        let dir = tmp.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        let doc = json!({
            "tokens": { "access_token": "access-xyz" },
            "last_refresh": "2026-06-27T00:00:00Z",
        });
        std::fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();

        save_current_account(CliApp::Codex).unwrap();
        let before = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(before.accounts.len(), 1);
        assert!(before.current.as_ref().unwrap().saved);

        activate_custom_provider(&tmp, "sk-third-party");

        let after = list_accounts(CliApp::Codex).unwrap();
        assert_eq!(
            after.accounts[0].id, before.accounts[0].id,
            "the id must not depend on provider fields sharing the file"
        );
        assert_eq!(
            after.accounts.len(),
            1,
            "activating a provider must not fork the account list"
        );
        assert!(
            after.accounts[0].active && after.current.as_ref().unwrap().saved,
            "the live login must still read as this saved account"
        );

        // The tray's unattended snapshot therefore has nothing to add.
        assert_eq!(
            auto_save_unsaved_live_account(CliApp::Codex)
                .unwrap()
                .as_deref(),
            Some(before.accounts[0].id.as_str()),
        );
        assert_eq!(list_accounts(CliApp::Codex).unwrap().accounts.len(), 1);
    }

    #[test]
    fn reading_the_store_scrubs_provider_keys_from_historical_snapshots() {
        let _g = lock_home();
        let tmp = tempdir("scrub-store");
        let _h = override_home(&tmp);
        let store_dir = tmp.join(".termory");
        std::fs::create_dir_all(&store_dir).unwrap();

        // A v2 file written before the provider/account split: the snapshot
        // captured the whole auth.json, third-party key included.
        let store = json!({
            "version": 2,
            "accounts": [{
                "id": "acct-a", "app": "codex",
                "name": "Jane", "email": "a@example.com", "plan": "Pro",
                "payload": {
                    "tokens": { "access_token": "access-xyz", "account_id": "acct-a" },
                    "last_refresh": "2026-06-27T00:00:00Z",
                    "auth_mode": "apikey",
                    "OPENAI_API_KEY": "sk-leaked",
                },
                "savedAt": "2026-06-27T00:00:00Z",
            }],
        });
        let store_path = store_dir.join("accounts.json");
        std::fs::write(&store_path, serde_json::to_string_pretty(&store).unwrap()).unwrap();

        let entries = read_store().unwrap();
        assert!(entries[0]["payload"].get("OPENAI_API_KEY").is_none());

        // …and the cleanup is persisted, not re-done on every read.
        let on_disk: JsonValue =
            serde_json::from_slice(&std::fs::read(&store_path).unwrap()).unwrap();
        let payload = on_disk.pointer("/accounts/0/payload").unwrap();
        assert!(
            payload.get("OPENAI_API_KEY").is_none(),
            "the leaked key must be gone from disk"
        );
        assert!(payload.get("auth_mode").is_none());
        assert_eq!(
            payload
                .pointer("/tokens/account_id")
                .and_then(|v| v.as_str()),
            Some("acct-a"),
            "the login itself survives"
        );
        assert_eq!(
            on_disk
                .pointer("/accounts/0/email")
                .and_then(|v| v.as_str()),
            Some("a@example.com"),
            "and so does the rest of the entry"
        );
        assert_eq!(
            on_disk.pointer("/version").and_then(|v| v.as_u64()),
            Some(ACCOUNTS_SCHEMA_VERSION),
        );
    }

    #[test]
    fn v1_data_reaches_every_later_migration_arm() {
        let _g = lock_home();
        let tmp = tempdir("migrate-chain");
        let _h = override_home(&tmp);
        let store_dir = tmp.join(".termory");
        std::fs::create_dir_all(&store_dir).unwrap();

        // The oldest shape: payload as a JSON-encoded STRING, and it carries a
        // provider key. Both the v2 arm (parse) and the v3 arm (scrub) apply.
        let payload_str = serde_json::to_string(&json!({
            "tokens": { "account_id": "acct-a" },
            "OPENAI_API_KEY": "sk-leaked",
        }))
        .unwrap();
        let store = json!({
            "version": 1,
            "accounts": [{
                "id": "acct-a", "app": "codex",
                "payload": payload_str,
                "savedAt": "2026-06-27T00:00:00Z",
            }],
        });
        std::fs::write(
            store_dir.join("accounts.json"),
            serde_json::to_string_pretty(&store).unwrap(),
        )
        .unwrap();

        let entries = read_store().unwrap();
        let payload = &entries[0]["payload"];
        assert!(payload.is_object(), "v2 arm ran (string → object)");
        assert!(
            payload.get("OPENAI_API_KEY").is_none(),
            "v3 arm ran too — an early return here would skip it"
        );
    }

    #[test]
    fn snapshots_never_capture_the_active_provider_credentials() {
        let _g = lock_home();
        let tmp = tempdir("snapshot-clean");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        activate_custom_provider(&tmp, "sk-third-party");

        // Both entry points: the page's explicit save…
        save_current_account(CliApp::Codex).unwrap();
        // …and the tray's unattended one (idempotent on an already-saved id,
        // so clear the store to exercise it).
        let payload_from_save = read_store().unwrap()[0]["payload"].clone();
        write_store(Vec::new()).unwrap();
        auto_save_unsaved_live_account(CliApp::Codex).unwrap();
        let payload_from_auto_save = read_store().unwrap()[0]["payload"].clone();

        for (label, payload) in [
            ("save_current_account", &payload_from_save),
            ("auto_save_unsaved_live_account", &payload_from_auto_save),
        ] {
            assert!(
                payload.get("OPENAI_API_KEY").is_none(),
                "{label} must not copy a third-party key into accounts.json"
            );
            assert!(payload.get("auth_mode").is_none(), "{label}");
            assert!(
                payload.pointer("/tokens/account_id").is_some(),
                "{label} still stores the login itself"
            );
        }
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
    fn tray_accounts_labels_rows_and_marks_the_live_one() {
        let _g = lock_home();
        let tmp = tempdir("tray-accounts");
        let _h = override_home(&tmp);

        // Non-Codex CLIs have no snapshot management → no account rows.
        assert!(tray_accounts(CliApp::Claude).is_empty());
        // No accounts.json yet.
        assert!(tray_accounts(CliApp::Codex).is_empty());

        // Save an email-only login, then a second one that is the LIVE login.
        write_codex_auth(&tmp, "first@example.com", "pro", "acct-first");
        save_current_account(CliApp::Codex).unwrap();
        write_codex_auth(&tmp, "second@example.com", "plus", "acct-second");
        save_current_account(CliApp::Codex).unwrap();

        let rows = tray_accounts(CliApp::Codex);
        assert_eq!(rows.len(), 2);
        // The fake token carries no `name` claim → label falls back to email.
        let live = rows.iter().find(|r| r.active).expect("one row is live");
        assert_eq!(live.label, "second@example.com");
        assert!(!live.needs_relogin);
        let other = rows.iter().find(|r| !r.active).unwrap();
        assert_eq!(other.label, "first@example.com");

        // A revoked refresh token flags the row (rendered with a ⚠ suffix).
        mark_account_relogin(&other.id, true).unwrap();
        let rows = tray_accounts(CliApp::Codex);
        assert!(
            rows.iter()
                .find(|r| r.id == other.id)
                .unwrap()
                .needs_relogin
        );
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

    // ================================================================
    // Claude
    // ================================================================
    //
    // In test builds `claude_auth` is file-only (the Keychain tier is
    // compiled out — see that module), so these exercise the full
    // save/switch flow against `.credentials.json` + `~/.claude.json`
    // under an overridden HOME. That covers every OS's file path and the
    // whole accounts layer; the macOS Keychain read/write itself is the
    // one seam left to real-machine verification.

    /// Write a Claude login: the credential file plus the `oauthAccount`
    /// identity in `~/.claude.json` (which also carries unrelated keys the
    /// switch must preserve).
    fn write_claude_login(home: &Path, token: &str, uuid: &str, email: &str, name: &str) {
        // Deliberately NO refreshToken: the switch's refresh attempt then
        // short-circuits as Transient and proceeds offline — the same trick
        // `write_codex_auth` uses to keep the roundtrip tests network-free.
        let cred = json!({
            "claudeAiOauth": {
                "accessToken": token,
                "expiresAt": 1_800_000_000_000_i64,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
            }
        });
        let dir = home.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".credentials.json"), cred.to_string()).unwrap();

        let claude_json = json!({
            "firstStartTime": "2026-01-01T00:00:00Z",
            "projects": { "/Users/me/proj": { "allowedTools": [] } },
            "oauthAccount": {
                "accountUuid": uuid,
                "emailAddress": email,
                "displayName": name,
                "organizationUuid": "org-1",
            },
        });
        std::fs::write(home.join(".claude.json"), claude_json.to_string()).unwrap();
    }

    #[test]
    fn claude_live_reads_identity_and_requires_oauth_account() {
        let _g = lock_home();
        let tmp = tempdir("claude-live");
        let _h = override_home(&tmp);
        let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        // No login at all → None, and save errors.
        assert!(read_claude_live().unwrap().is_none());
        assert!(save_claude_account().is_err());

        write_claude_login(&tmp, "at-1", "uuid-a", "a@example.com", "Alice");
        let live = read_claude_live().unwrap().expect("live login");
        assert_eq!(live.id, "uuid-a");
        assert_eq!(live.email.as_deref(), Some("a@example.com"));
        assert_eq!(live.name.as_deref(), Some("Alice"));
        assert_eq!(live.plan.as_deref(), Some("Max"));

        // Credential present but identity gone (no oauthAccount) → not
        // listable/savable: the id would have no stable source.
        std::fs::write(tmp.join(".claude.json"), "{}").unwrap();
        assert!(read_claude_live().unwrap().is_none());
        assert!(save_claude_account().is_err());
    }

    #[tokio::test]
    async fn claude_save_switch_roundtrip_restores_credentials_and_identity() {
        let _g = lock_home();
        let tmp = tempdir("claude-roundtrip");
        let _h = override_home(&tmp);
        let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        // Login A, save. Capture A's on-disk shapes for the final compare.
        write_claude_login(&tmp, "at-a", "uuid-a", "a@example.com", "Alice");
        save_current_account(CliApp::Claude).unwrap();
        let cred_a = std::fs::read_to_string(tmp.join(".claude/.credentials.json")).unwrap();

        // Login B (simulates `claude /login` with another account), save.
        write_claude_login(&tmp, "at-b", "uuid-b", "b@example.com", "Bob");
        save_current_account(CliApp::Claude).unwrap();

        let state = list_accounts(CliApp::Claude).unwrap();
        assert_eq!(state.accounts.len(), 2);
        assert!(
            state
                .accounts
                .iter()
                .any(|a| a.id == "uuid-b" && a.active && a.plan.as_deref() == Some("Max")),
            "B is the live login"
        );
        assert!(state.current.as_ref().unwrap().saved);

        // Switch back to A.
        switch_account("uuid-a".into()).await.unwrap();

        // Credential restored byte-equivalent at the JSON level.
        let cred_now: JsonValue = serde_json::from_str(
            &std::fs::read_to_string(tmp.join(".claude/.credentials.json")).unwrap(),
        )
        .unwrap();
        let cred_a: JsonValue = serde_json::from_str(&cred_a).unwrap();
        assert_eq!(cred_now, cred_a, "A's credential document restored");

        // Identity followed, and the unrelated ~/.claude.json keys survived.
        let cj: JsonValue =
            serde_json::from_str(&std::fs::read_to_string(tmp.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            cj.pointer("/oauthAccount/emailAddress")
                .and_then(|v| v.as_str()),
            Some("a@example.com")
        );
        assert_eq!(
            cj.pointer("/oauthAccount/displayName")
                .and_then(|v| v.as_str()),
            Some("Alice")
        );
        assert_eq!(
            cj.pointer("/firstStartTime").and_then(|v| v.as_str()),
            Some("2026-01-01T00:00:00Z"),
            "unrelated top-level keys must be preserved"
        );
        assert!(
            cj.pointer("/projects/~1Users~1me~1proj").is_some(),
            "projects map must be preserved"
        );

        let state = list_accounts(CliApp::Claude).unwrap();
        assert!(
            state.accounts.iter().any(|a| a.id == "uuid-a" && a.active),
            "A is active after the switch"
        );
    }

    /// The outgoing login's snapshot is refreshed before the overwrite:
    /// Claude rotates the refresh token, so the store's copy from save time
    /// may be dead while the live file holds the only working one.
    #[tokio::test]
    async fn claude_switch_resnapshots_the_outgoing_login_first() {
        let _g = lock_home();
        let tmp = tempdir("claude-resnap");
        let _h = override_home(&tmp);
        let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        // A saved, then B saved while B is live.
        write_claude_login(&tmp, "at-a", "uuid-a", "a@example.com", "Alice");
        save_current_account(CliApp::Claude).unwrap();
        write_claude_login(&tmp, "at-b-old", "uuid-b", "b@example.com", "Bob");
        save_current_account(CliApp::Claude).unwrap();

        // Claude itself refreshes B's tokens after our save — the live file
        // moves ahead of the snapshot.
        write_claude_login(&tmp, "at-b-fresh", "uuid-b", "b@example.com", "Bob");

        switch_account("uuid-a".into()).await.unwrap();

        let store = read_store().unwrap();
        let b = store
            .iter()
            .find(|e| str_field(e, "id") == Some("uuid-b"))
            .expect("B still in store");
        assert_eq!(
            b.pointer("/payload/credentials/claudeAiOauth/accessToken")
                .and_then(|v| v.as_str()),
            Some("at-b-fresh"),
            "B's snapshot must hold the live (freshest) tokens, not the save-time ones"
        );
    }

    /// The codex switch recaptures the outgoing login's live tokens into
    /// its existing store entry before overwriting auth.json — the stored
    /// copy may hold a rotated-away (dead) refresh token while the live
    /// file has the only working one.
    #[tokio::test]
    async fn codex_switch_resnapshots_the_outgoing_login_first() {
        let _g = lock_home();
        let tmp = tempdir("codex-resnap");
        let _h = override_home(&tmp);

        // A saved, then B saved while B is live.
        write_codex_auth(&tmp, "a@example.com", "plus", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        write_codex_auth(&tmp, "b@example.com", "pro", "acct-b");
        save_current_account(CliApp::Codex).unwrap();

        // Codex itself refreshes B's tokens after our save — the live file
        // moves ahead of the snapshot.
        let auth_path = tmp.join(".codex/auth.json");
        let mut doc: JsonValue =
            serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
        doc["tokens"]["access_token"] = JsonValue::String("access-fresh".into());
        std::fs::write(&auth_path, doc.to_string()).unwrap();

        switch_account("acct-a".into()).await.unwrap();

        let store = read_store().unwrap();
        let b = store
            .iter()
            .find(|e| str_field(e, "id") == Some("acct-b"))
            .expect("B still in store");
        assert_eq!(
            b.pointer("/payload/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("access-fresh"),
            "B's snapshot must hold the live (freshest) tokens, not the save-time ones"
        );
    }

    /// Cancelling must NOT vacate the slot — it stays occupied until the
    /// cancelled flow's own Drop, so no new login can start while the old
    /// one is still rolling back (a `take()` here once opened exactly that
    /// window, and the old flow's Drop then wiped the new login's notify).
    #[tokio::test]
    async fn cancel_claude_login_leaves_the_slot_reserved() {
        let state = ClaudeLoginCancel(std::sync::Mutex::new(None));
        let (_guard, _notify) = LoginSlot::reserve(&state.0).unwrap();
        cancel_claude_login(&state).await.expect("cancel fires");
        assert!(
            state.0.lock().unwrap().is_some(),
            "slot must stay reserved until the cancelled flow's Drop"
        );
        assert!(
            LoginSlot::reserve(&state.0).is_err(),
            "no new login may start during the rollback window"
        );
    }

    /// The login-cancel slot doubles as the backend re-entrancy guard, and
    /// its Drop-based clear is what keeps an early-exit path (the codex
    /// flow's spawn failure used to leak it) from blocking every future
    /// login.
    #[test]
    fn login_slot_blocks_reentry_and_clears_on_drop() {
        let slot: std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>> =
            std::sync::Mutex::new(None);
        let (guard, _notify) = LoginSlot::reserve(&slot).expect("first reserve");
        assert!(slot.lock().unwrap().is_some(), "slot reserved");
        assert!(
            LoginSlot::reserve(&slot).is_err(),
            "a second concurrent login must be refused"
        );
        drop(guard);
        assert!(slot.lock().unwrap().is_none(), "drop must clear the slot");
        let (_g, _n) = LoginSlot::reserve(&slot).expect("cleared slot accepts a new login");
    }

    /// A corrupt (or mid-write) `~/.claude.json` must ABORT the identity
    /// write, never degrade to an empty doc — writing that back would
    /// truncate Claude's whole global config to just `oauthAccount` (the
    /// same never-truncate rule as `claude_desktop::read_json_or_empty`).
    #[test]
    fn claude_json_rewrite_refuses_a_corrupt_file() {
        let _g = lock_home();
        let tmp = tempdir("claude-corrupt-cj");
        let _h = override_home(&tmp);
        let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        let path = tmp.join(".claude.json");
        // A torn write: valid prefix, truncated mid-object.
        std::fs::write(&path, r#"{"projects": {"/a": {"allowed"#).unwrap();
        let account = json!({ "accountUuid": "u", "emailAddress": "a@b.c" });
        let err = update_claude_oauth_account(Some(&account))
            .expect_err("corrupt file must refuse the rewrite");
        assert!(
            err.to_string().contains("refusing"),
            "actionable message, got: {err}"
        );
        // And a non-object document refuses too.
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert!(update_claude_oauth_account(Some(&account)).is_err());
        // The file is untouched in both cases.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1,2,3]");
    }

    /// `~/.claude.json` resolution follows the official `getGlobalClaudeFile`
    /// (env.ts:14-26): `<config-dir>/.config.json` wins when present, else
    /// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
    #[test]
    fn claude_json_path_honors_config_dir_and_legacy_file() {
        let _g = lock_home();
        let tmp = tempdir("claude-cj-path");
        let _h = override_home(&tmp);
        {
            let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");
            assert_eq!(claude_json_path().unwrap(), tmp.join(".claude.json"));
        }
        let custom = tmp.join("custom-cc");
        std::fs::create_dir_all(&custom).unwrap();
        let _cfg = EnvVarGuard::set("CLAUDE_CONFIG_DIR", &custom);
        assert_eq!(claude_json_path().unwrap(), custom.join(".claude.json"));
        // Legacy .config.json inside the config dir takes precedence.
        std::fs::write(custom.join(".config.json"), "{}").unwrap();
        assert_eq!(claude_json_path().unwrap(), custom.join(".config.json"));
    }

    /// The add-account flow's logout + rollback rides on
    /// `update_claude_oauth_account(None)` — pin its two contracts: removing
    /// ONLY the key (other keys survive), and not minting a `{}` config
    /// where none existed.
    #[test]
    fn claude_oauth_account_clear_is_narrow_and_no_ops_on_missing_file() {
        let _g = lock_home();
        let tmp = tempdir("claude-oauth-clear");
        let _h = override_home(&tmp);
        let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        // Missing file + None → no file appears.
        update_claude_oauth_account(None).unwrap();
        assert!(!tmp.join(".claude.json").exists());

        write_claude_login(&tmp, "at-a", "uuid-a", "a@example.com", "Alice");
        update_claude_oauth_account(None).unwrap();
        let cj: JsonValue =
            serde_json::from_str(&std::fs::read_to_string(tmp.join(".claude.json")).unwrap())
                .unwrap();
        assert!(cj.get("oauthAccount").is_none(), "key removed");
        assert!(
            cj.get("firstStartTime").is_some() && cj.get("projects").is_some(),
            "unrelated keys survive the clear"
        );
    }

    #[test]
    fn tray_accounts_includes_claude_rows() {
        let _g = lock_home();
        let tmp = tempdir("claude-tray");
        let _h = override_home(&tmp);
        let _cfg = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");

        write_claude_login(&tmp, "at-a", "uuid-a", "a@example.com", "Alice");
        save_current_account(CliApp::Claude).unwrap();

        let rows = tray_accounts(CliApp::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Alice · a@example.com");
        assert!(rows[0].active);
        // Display-only apps still contribute no rows.
        assert!(tray_accounts(CliApp::Gemini).is_empty());
    }
}
