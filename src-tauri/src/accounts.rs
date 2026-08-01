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
//!
//! ## Phase 3 — Grok Build (file-based, never refreshes)
//!
//! Grok's store is the plain `<grok-home>/auth.json` file (0600) — no
//! Keychain on any OS — but its refresh token rotates under reuse detection,
//! so this is the one CLI whose switch does NO network validation. See the
//! "Grok Build — full multi-account management" section below.

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
/// Codex / Claude / Grok — full multi-account management.
/// Gemini — display-only: reads the CLI's live credential file for
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

/// Saved logins for the tray's account submenu. Codex + Claude + Grok — the
/// CLIs with snapshot management (`list_accounts` is display-only for the
/// others), so every other app returns `[]` and the tray renders no account
/// section. A read failure also yields `[]`: no accounts.json just means
/// "nothing saved".
pub fn tray_accounts(app: CliApp) -> Vec<TrayAccount> {
    let state = match app {
        CliApp::Codex => list_codex_accounts(),
        // No perf gate like Claude's below: grok's live login is a plain file
        // read, the same cost as codex's, not a `security(1)` spawn.
        CliApp::Grok => list_grok_accounts(),
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
        CliApp::Grok => save_grok_account(),
        _ => Err(unsupported(app)),
    }
}

/// Restore a saved snapshot into the live CLI credential.
/// Refreshes tokens in memory BEFORE writing to auth.json — if refresh fails
/// the auth.json is left untouched and the caller should mark needsRelogin.
///
/// Returns the CLI whose live login changed, so a caller holding only the
/// account id (the IPC command) can invalidate that CLI's cached quota —
/// the numbers belong to the account being switched away from.
pub async fn switch_account(id: String) -> Result<CliApp, Box<dyn Error>> {
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
        Some("codex") => {
            switch_codex(&payload).await?;
            Ok(CliApp::Codex)
        }
        Some("claude") => {
            switch_claude(&payload).await?;
            Ok(CliApp::Claude)
        }
        Some("grok") => {
            switch_grok(&id, &payload).await?;
            Ok(CliApp::Grok)
        }
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

/// Where codex's login server listens — its `DEFAULT_PORT`
/// (`login/src/server.rs:54`).
///
/// Only this one, never the `FALLBACK_PORT` beside it: codex's own
/// `/cancel` caller is gated on `!using_fallback_port` (server.rs:585), so
/// it addresses the default port and never sweeps the fallback. Sweeping
/// both would fire `/cancel` at whatever else occupies a port we did not
/// bind, while our own server (on the fallback) went untouched.
const CODEX_LOGIN_PORT: u16 = 1455;

/// `GET /cancel` against a running codex login server, which responds and
/// then shuts ITSELF down (`login/src/server.rs:445`,
/// `HandledRequest::ResponseAndExit`). Byte-for-byte the request codex
/// sends to clear a stale server before rebinding (`send_cancel_request`,
/// server.rs:545), std sockets included — tokio is built here without the
/// `net` feature.
///
/// Returns whether the request was CONNECTED AND SENT, mirroring codex's
/// `io::Result<()>`: `?` on connect and write, but only `let _ = read(...)`
/// for the reply. **The response is deliberately not inspected** — whether
/// the child exits is the real answer and the caller waits for that; a
/// status line would only add a weaker second signal that can disagree with
/// it. The caller does need the send result, though: nothing listening means
/// nothing to wait for, so it should go straight to `kill` rather than sit
/// out the grace period.
/// `port` is a parameter purely so the unit tests can point this at a stub
/// listener on an ephemeral port; production has exactly one caller and it
/// passes [`CODEX_LOGIN_PORT`].
fn send_codex_cancel(port: u16) -> bool {
    use std::io::{Read as _, Write as _};
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let Ok(addr) = format!("127.0.0.1:{port}").parse::<std::net::SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, TIMEOUT) else {
        return false; // nothing listening — nothing to cancel
    };
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let req =
        format!("GET /cancel HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    true
}

/// Stop a running `codex login` as cleanly as possible.
///
/// **Do NOT simplify this back to `child.kill()` (LOCKED).** codex installs
/// from npm as a node WRAPPER that spawns the real binary
/// (`bin/codex.js:195`), and the wrapper is the only process we hold a
/// handle to. `Child::kill` sends SIGKILL, which is uncatchable — the
/// wrapper dies instantly, its `forwardSignal` (codex.js:223) never runs,
/// and the real binary is re-parented and KEEPS SERVING. Measured on two
/// machines, both npm installs: macOS left pid 91035 holding 1455, Windows
/// left `codex.exe` 9712 doing the same.
///
/// That orphan is not inert — it still answered `/auth/callback` with
/// `400 State mismatch` after its parent was gone, i.e. a live OAuth server
/// whose state DOES match the browser tab still open on the user's screen.
/// Completing that login writes credentials AFTER `restore_auth` restored
/// the previous account, silently undoing the cancel the user asked for.
///
/// So ask the server to stop: it exits on its own, the wrapper follows it
/// (codex.js awaits the child, then mirrors its exit), and the port is
/// released properly rather than left behind by a hard kill. Being plain
/// HTTP this behaves identically on Windows.
///
/// **codex-only.** The other CLIs were measured and none leak: claude and
/// grok run their login as a SINGLE process that `child.kill()` takes down
/// cleanly — grok checked under both its official installer and
/// `npm install -g @xai-official/grok` — and gemini has no login command.
///
/// **The fallback is no longer wrapper-only.** It used to be a bare
/// `child.kill()`, which is exactly the signal that leaves the grandchild
/// running; it is now [`crate::process::Managed::terminate`], which takes
/// the whole process group / job down. `/cancel` is still tried FIRST —
/// a server that shuts itself down releases its port properly and lets the
/// wrapper mirror its exit, which a signal cannot do — so this stays the
/// preferred path rather than the only line of defence.
async fn stop_codex_login(child: &mut crate::process::Managed, port: u16) {
    let sent = tokio::task::spawn_blocking(move || send_codex_cancel(port))
        .await
        .unwrap_or(false);

    if sent
        && tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .is_ok()
    {
        return; // exited on its own — nothing to kill
    }
    child.terminate(crate::process::CANCEL_GRACE).await;
}

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

    // Capture the outgoing login BEFORE auth.json is cleared below — see
    // `resnapshot_live_before_login` for why this cannot be only-if-missing.
    let prev_active_id: Option<String> =
        resnapshot_live_before_login(CliApp::Codex).map_err(|e| e.to_string())?;

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
    // Managed: silent (no console window on Windows), killable as a whole
    // process group, and torn down if Termory quits mid-login.
    let mut child = match crate::process::spawn_managed(cmd) {
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
    let stderr_pipe = child.stderr().expect("stderr piped");
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
            stop_codex_login(&mut child, CODEX_LOGIN_PORT).await;
            stderr_task.abort();
            restore_auth(&auth_path, original_auth.as_deref());
            return Err("codex login timed out after 5 minutes".into());
        },
        _ = cancel_notify.notified() => {
            stop_codex_login(&mut child, CODEX_LOGIN_PORT).await;
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
/// Snapshot the live login into the store immediately before a login flow
/// destroys it, returning its id so the caller can restore it afterwards.
///
/// **UNCONDITIONAL — deliberately not `auto_save_unsaved_live_account`.** Every
/// login flow destroys the outgoing credential (claude's `performLogout` wipes
/// local storage; a grok login overwrites the one shared scope entry; the codex
/// flow blanks auth.json itself so codex's `logout_with_revoke` finds nothing to
/// revoke). All three CLIs rotate their refresh token, so the copy ON DISK is
/// the only one that still works — an entry saved days ago holds a rotated-away
/// token, and the restore at the end of the flow would turn into a
/// `needsRelogin` on an account the user never touched.
///
/// The only-if-missing helper below answers a DIFFERENT question ("would this
/// unattended switch lose a login the user never saved?"), which is why the tray
/// keeps using it: there, freshness is already handled by `switch_*`'s own
/// outgoing re-snapshot. Here there is no later step to fall back on.
fn resnapshot_live_before_login(app: CliApp) -> Result<Option<String>, Box<dyn Error>> {
    match app {
        CliApp::Codex => match read_codex_live()? {
            Some(live) => {
                save_codex_account()?;
                Ok(Some(live.id))
            }
            None => Ok(None),
        },
        // UNCACHED: the rotation this defends against is exactly what makes a
        // ≤30s-stale cached doc wrong here (same rule as switch_claude's).
        CliApp::Claude => match read_claude_live_uncached()? {
            Some(live) => {
                save_claude_account()?;
                Ok(Some(live.id))
            }
            None => Ok(None),
        },
        CliApp::Grok => match read_grok_live()? {
            Some(live) => {
                save_grok_live(&live)?;
                Ok(Some(live.id))
            }
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

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
        CliApp::Grok => {
            let Some(live) = read_grok_live()? else {
                return Ok(None);
            };
            let id = live.id.clone();
            let store = read_store()?;
            if !store.iter().any(|e| {
                str_field(e, "app") == Some("grok") && str_field(e, "id") == Some(id.as_str())
            }) {
                save_grok_live(&live)?;
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

    // Capture the outgoing login before `claude auth login`'s own
    // `performLogout` wipes local storage — see `resnapshot_live_before_login`.
    let prev_active_id = resnapshot_live_before_login(CliApp::Claude).map_err(|e| e.to_string())?;

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
    // Managed: silent (no console window on Windows), killable as a whole
    // process group, and torn down if Termory quits mid-login.
    let mut child = match crate::process::spawn_managed(cmd) {
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
    let stdout_pipe = child.stdout().expect("stdout piped");
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
    let stderr_pipe = child.stderr().expect("stderr piped");
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
            child.terminate(crate::process::CANCEL_GRACE).await;
            stdout_task.abort();
            stderr_task.abort();
            restore_claude_auth(original_cred.as_ref(), original_oauth.as_ref());
            return Err("claude login timed out after 5 minutes".into());
        },
        _ = cancel_notify.notified() => {
            child.terminate(crate::process::CANCEL_GRACE).await;
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
// Grok Build — full multi-account management
// ===================================================================
//
// Grok's credential store is the plain `<grok-home>/auth.json` file (0600),
// a scope-keyed map `{ "<issuer>::<client_id>": GrokAuth }` (xai-grok-shell
// `auth/model.rs:259` + `auth/config.rs:217`) holding PLAIN `email` /
// `first_name` / `last_name` — no Keychain on any OS and no JWT decode. The
// xAI client_id is a compile-time constant (`auth/config.rs:279`), so every
// first-party login lands on the SAME scope key: one account at a time on
// disk, which is exactly why this snapshot/switch feature exists (same shape
// as Codex / Claude).
//
// ## The refresh token is the whole hazard, and it shapes everything here
//
// auth.x.ai rotates the refresh token and runs reuse detection: spending one
// twice revokes the token family (`auth/manager.rs:1710` — "a straddled
// exchange loses the rotated token, which is what revokes the family";
// `:1823` — "the double-spend that trips IdP rotation reuse detection"). The
// blast radius is one `grok login`, not a lost account — but it is silent and
// arrives hours later, so the design is built to never hand the user that.
//
// Three rules, each load-bearing:
//
//  1. **Refresh the snapshot before committing it** (`switch_grok`). A stored
//     credential goes stale as grok rotates the on-disk one, and restoring a
//     spent token "succeeds" then logs the user out at grok's next refresh.
//     Validating first turns that into an error at click time plus a
//     `needsRelogin` flag — the same contract `switch_codex` /
//     `switch_claude` provide. A 429/5xx/network failure says nothing about
//     the token, so it degrades to writing the snapshot verbatim.
//  2. **Re-snapshot the outgoing login before overwriting it.** A grok running
//     in a terminal rotates the RT in place, so an entry saved days ago holds
//     a spent one. Recapturing at switch-out keeps the entry we may come back
//     to in step with disk.
//  3. **Hold grok's own `auth.json.lock` across all of it** (`GrokAuthLock`),
//     including the IdP call — exactly why grok holds it across its own.
//     Without it a concurrent grok refresh lands between our read and our
//     write and we clobber the RT it just rotated to, which is both a lost
//     token and the double-spend rule 1 exists to avoid.
//
// The endpoint and grant were verified live, not inferred — see
// `GROK_TOKEN_ENDPOINT`. The refresh itself replicates grok's own
// (`auth/oidc/refresh.rs`) field-for-field, including its keep-the-old-token
// rule when the IdP declines to rotate.

/// auth.json scope prefix for a first-party xAI login. Matched by PREFIX, not
/// in full: the client_id suffix is a constant in grok (`auth/config.rs:279`)
/// but `GROK_OAUTH2_CLIENT_ID` can override it (`auth/config.rs:233`).
const GROK_XAI_SCOPE_PREFIX: &str = "https://auth.x.ai::";

fn grok_auth_path() -> Result<std::path::PathBuf, Box<dyn Error>> {
    Ok(crate::providers::grok_home_dir()
        .ok_or("Grok home directory not available")?
        .join("auth.json"))
}

/// The live first-party login read out of `auth.json`.
struct GrokLive {
    /// `user_id` — the account identity, and the store key.
    id: String,
    /// `first_name last_name`, whichever parts exist.
    name: Option<String>,
    email: Option<String>,
    /// The scope key this login is filed under.
    scope: String,
    /// That scope's `GrokAuth` value, verbatim.
    auth: JsonValue,
}

fn read_grok_live() -> Result<Option<GrokLive>, Box<dyn Error>> {
    read_grok_live_at(&grok_auth_path()?)
}

fn read_grok_live_at(path: &std::path::Path) -> Result<Option<GrokLive>, Box<dyn Error>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let doc: JsonValue = serde_json::from_str(&raw).unwrap_or(JsonValue::Null);
    Ok(grok_live_from_doc(&doc))
}

/// Pick the first-party xAI login out of a parsed auth.json.
///
/// Gated on the scope prefix AND a non-empty `user_id`. The `user_id` check
/// is what keeps a plain API-key entry out: `store_api_key` writes the
/// `xai::api_key` scope from `GrokAuth::default()` (`auth/storage.rs:426`),
/// whose `user_id` is the empty string — so requiring one excludes it without
/// hardcoding its key, and excludes any future keyless scope for free.
fn grok_live_from_doc(doc: &JsonValue) -> Option<GrokLive> {
    let field = |v: &JsonValue, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    let (scope, auth) = doc
        .as_object()?
        .iter()
        .find(|(k, v)| k.starts_with(GROK_XAI_SCOPE_PREFIX) && field(v, "user_id").is_some())?;
    Some(GrokLive {
        id: field(auth, "user_id")?,
        name: match (field(auth, "first_name"), field(auth, "last_name")) {
            (Some(f), Some(l)) => Some(format!("{f} {l}")),
            (Some(f), None) => Some(f),
            (None, Some(l)) => Some(l),
            (None, None) => None,
        },
        email: field(auth, "email"),
        scope: scope.clone(),
        auth: auth.clone(),
    })
}

/// Exclusive hold on grok's own `auth.json.lock`, mirroring
/// `xai-grok-shell/src/auth/manager/lock.rs`: a whole-file advisory lock plus
/// `PID:TIMESTAMP` holder info.
///
/// Writing the holder info is NOT cosmetic. A grok waiter that finds holder
/// info older than its 60s stale timeout breaks the lock by unlinking the
/// file and retrying on a fresh inode, so leaving the PREVIOUS holder's stale
/// `PID:TS` in place invites grok to break ours mid-write.
///
/// **The hold spans an IdP round-trip** (`switch_grok` refreshes inside it, on
/// purpose — two processes spending a refresh token concurrently is the
/// double-spend rotation reuse detection punishes). It is nonetheless bounded
/// by the refresh client's 20s timeout, roughly a third of grok's 60s stale
/// threshold, so a single `PID:TS` stamp at acquire stays fresh for the whole
/// hold and no heartbeat thread is needed. grok's own holds DO need one
/// because its refreshes fire from a background daemon at arbitrary times and
/// can straddle a system sleep, where wall-clock staleness keeps counting;
/// ours run only when the user just clicked Switch, i.e. with the machine
/// awake.
///
/// The lock releases when the handle drops (the advisory lock lives on the
/// open file description). The lock FILE is deliberately never deleted:
/// unlinking it is grok's break-a-stuck-holder signal, not a release.
struct GrokAuthLock {
    _file: std::fs::File,
}

impl GrokAuthLock {
    /// One non-blocking attempt. `Ok(None)` = currently held by someone else.
    fn try_acquire(auth_path: &std::path::Path) -> Result<Option<Self>, Box<dyn Error>> {
        let lock_path = auth_path.with_file_name("auth.json.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // `truncate(false)`: the holder info is rewritten below under the
        // lock. Truncating before acquiring would blank a live holder's
        // `PID:TS` and make grok's waiters read IT as stale.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(Box::new(e)),
        }
        // Best-effort, exactly as grok treats its own heartbeat rewrite: we
        // hold the real lock either way, and the window is milliseconds.
        if let Err(e) = write_grok_lock_holder(&mut file) {
            log::warn!("Failed to stamp grok auth lock holder info: {e}");
        }
        Ok(Some(Self { _file: file }))
    }
}

/// Write `PID:UNIX_TIMESTAMP` into the lock file — the exact payload grok's
/// `write_holder_info` (`auth/manager/lock.rs`) writes and its waiters parse.
fn write_grok_lock_holder(file: &mut std::fs::File) -> std::io::Result<()> {
    use std::io::{Seek, Write};
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    write!(file, "{}:{}", std::process::id(), ts)?;
    file.sync_all()
}

/// Acquire the auth lock, retrying briefly before giving up.
///
/// A grok refresh holds the lock across an IdP round-trip, so failing on the
/// first miss would make switching flaky for the exact reason the lock exists.
/// The retry stays bounded so a genuinely busy grok fails fast with actionable
/// copy instead of hanging the switch.
async fn acquire_grok_lock(auth_path: &std::path::Path) -> Result<GrokAuthLock, Box<dyn Error>> {
    const ATTEMPTS: u32 = 10;
    for attempt in 0..ATTEMPTS {
        if let Some(lock) = GrokAuthLock::try_acquire(auth_path)? {
            return Ok(lock);
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }
    Err("Grok is using its credentials right now — quit any running grok and try again".into())
}

/// Read auth.json for a read-modify-write, mirroring grok's own
/// `read_auth_json_or_empty_recovering_corrupt` (`auth/storage.rs:181`):
/// missing or empty → a fresh map; valid → the parsed map; non-empty corrupt →
/// renamed aside to `auth.json.corrupt.<millis>` (grok's own backup name) and
/// then a fresh map.
///
/// The rename is what makes starting fresh safe: a switch must never silently
/// destroy credential bytes it could not parse.
fn grok_doc_for_write(path: &std::path::Path) -> Result<JsonValue, Box<dyn Error>> {
    let fresh = || JsonValue::Object(serde_json::Map::new());
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(fresh());
    };
    if raw.trim().is_empty() {
        return Ok(fresh());
    }
    match serde_json::from_str::<JsonValue>(&raw) {
        Ok(v) if v.is_object() => Ok(v),
        _ => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let backup = path.with_file_name(format!("auth.json.corrupt.{ts}"));
            std::fs::rename(path, &backup)?;
            log::warn!("Backed up corrupt grok auth.json to {}", backup.display());
            Ok(fresh())
        }
    }
}

fn list_grok_accounts() -> Result<AccountsState, Box<dyn Error>> {
    let live = read_grok_live()?;
    let current_id = live.as_ref().map(|l| l.id.as_str());
    let store = read_store()?;

    let mut accounts = Vec::new();
    let mut current_saved = false;
    for e in &store {
        if str_field(e, "app") != Some("grok") {
            continue;
        }
        let active = current_id.is_some() && current_id == str_field(e, "id");
        if active {
            current_saved = true;
        }
        accounts.push(SavedAccountView {
            id: str_field(e, "id").unwrap_or_default().to_string(),
            name: str_field(e, "name").unwrap_or_default().to_string(),
            email: str_field(e, "email").map(String::from),
            plan: str_field(e, "plan").map(String::from),
            saved_at: str_field(e, "savedAt").unwrap_or_default().to_string(),
            active,
            // Set by `switch_grok` when the refresh came back a definitive
            // 4xx; cleared by a successful refresh or a re-save.
            needs_relogin: e
                .get("needsRelogin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }

    let current = live.map(|l| CurrentAccount {
        name: l.name,
        email: l.email,
        // The plan is only served over the network (`/v1/settings`, see
        // quota.rs `fetch_grok_plan`); the quota card already renders it, so
        // the account row does not fetch it a second time.
        plan: None,
        saved: current_saved,
    });

    Ok(AccountsState {
        current,
        accounts,
        storage_warning: None,
    })
}

fn save_grok_account() -> Result<(), Box<dyn Error>> {
    let live = read_grok_live()?.ok_or("No Grok login found to save (run `grok login` first)")?;
    save_grok_live(&live)
}

/// Upsert `live` into the store.
///
/// Split from `save_grok_account` so the switch path can snapshot the document
/// it already read under the lock, rather than re-reading the file it is about
/// to overwrite (a second read is a second chance to race).
fn save_grok_live(live: &GrokLive) -> Result<(), Box<dyn Error>> {
    let mut store = read_store()?;
    let entry = json!({
        "id": live.id,
        "app": "grok",
        "name": live.name.clone().unwrap_or_default(),
        "email": live.email,
        // Payload is SCOPE-SCOPED, not the whole document — see switch_grok.
        "payload": { "scope": live.scope, "auth": live.auth },
        "savedAt": now_rfc3339(),
    });
    match store
        .iter_mut()
        .find(|e| str_field(e, "id") == Some(live.id.as_str()))
    {
        Some(slot) => *slot = entry,
        None => store.push(entry),
    }
    write_store(store)
}

/// xAI's OAuth2 token endpoint. Taken from the live discovery document
/// (`https://auth.x.ai/.well-known/openid-configuration`) rather than
/// rediscovered at runtime — grok resolves it per refresh, but the value is
/// stable and one fewer request is one fewer failure mode.
///
/// Verified live: `grant_types_supported` includes `refresh_token`, and
/// `token_endpoint_auth_methods_supported` includes `none` — a PUBLIC client,
/// so no secret is involved and the `oidc_client_id` in auth.json is the whole
/// credential. A probe with a junk token returned `invalid_grant` (not
/// `invalid_client`), i.e. the endpoint accepts this request shape from a
/// third-party caller.
const GROK_TOKEN_ENDPOINT: &str = "https://auth.x.ai/oauth2/token";

/// Test-only override for `refresh_grok_auth`, so switch tests exercise every
/// branch without a network call. Guarded by `lock_home()` like the rest of
/// the process-global test state.
#[cfg(test)]
static GROK_REFRESH_STUB: std::sync::Mutex<Option<GrokRefreshStub>> = std::sync::Mutex::new(None);

#[cfg(test)]
enum GrokRefreshStub {
    /// Apply this token-endpoint response body (the patching still runs).
    Response(JsonValue),
    AuthFailure,
    Transient,
}

/// Refresh a saved grok credential IN PLACE, patching exactly the fields
/// grok's own refresh replaces (`auth/oidc/refresh.rs:255-278` +
/// `oidc/protocol.rs:225` `build_grok_auth`): `key`, `create_time`,
/// `expires_at`, and `refresh_token`.
///
/// Two rules are copied from that code rather than invented:
///
///  1. **A response without a `refresh_token` keeps the OLD one**
///     (`refresh.rs:276-278`). The IdP rotates only sometimes; treating a
///     missing field as "no refresh token" would delete the user's only way
///     back into the account.
///  2. **The profile fields are carried over, not re-fetched.** grok rebuilds
///     them from the previous credential (`refresh.rs:255-272`) instead of
///     calling `/userinfo`, so a refresh costs exactly one request. Patching
///     the stored object in place gives that for free — everything we do not
///     touch is preserved by construction.
///
/// On any error the object is left EXACTLY as it was, so the caller can fall
/// back to writing the snapshot verbatim.
async fn refresh_grok_auth(auth: &mut JsonValue) -> Result<bool, RefreshError> {
    #[cfg(test)]
    return grok_refresh_stubbed(auth);
    #[cfg(not(test))]
    grok_refresh_over_http(auth).await
}

/// Test build of [`refresh_grok_auth`]: never opens a socket.
///
/// A missing stub PANICS rather than falling through to the network. Making it
/// merely documented is not enough — two switch tests really did send made-up
/// tokens to the live auth.x.ai before this seam existed, and the only signal
/// was a confusing `invalid_grant` failure. Now the mistake names itself.
#[cfg(test)]
fn grok_refresh_stubbed(auth: &mut JsonValue) -> Result<bool, RefreshError> {
    let guard = GROK_REFRESH_STUB.lock().unwrap();
    let Some(stub) = guard.as_ref() else {
        panic!(
            "refresh_grok_auth would hit the network: install a GrokRefreshGuard \
             in this test. A switch test covers the file/store choreography, not \
             the HTTP."
        );
    };
    match stub {
        GrokRefreshStub::Response(v) => apply_grok_token_response(auth, v),
        GrokRefreshStub::AuthFailure => {
            Err(RefreshError::AuthFailure("stubbed auth failure".into()))
        }
        GrokRefreshStub::Transient => {
            Err(RefreshError::Transient("stubbed transient failure".into()))
        }
    }
}

#[cfg(not(test))]
async fn grok_refresh_over_http(auth: &mut JsonValue) -> Result<bool, RefreshError> {
    let refresh_token = auth
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RefreshError::Transient("No refresh_token in saved credential".into()))?
        .to_string();
    let client_id = auth
        .get("oidc_client_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RefreshError::Transient("No oidc_client_id in saved credential".into()))?
        .to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    let resp = client
        .post(GROK_TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| RefreshError::Transient(format!("Grok token refresh failed: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Same split as the codex / claude refreshers: only a definitive 4xx
        // means the credential itself is dead. 429 and 5xx are the server
        // saying "not now", which must NOT be reported as a dead token — that
        // would flag a healthy account as needing re-login.
        let detail = serde_json::from_str::<JsonValue>(&body)
            .ok()
            .and_then(|v| {
                v.get("error_description")
                    .or_else(|| v.get("error"))
                    .and_then(|e| e.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| body.trim().to_string());
        let msg = format!("Grok token refresh failed ({status}): {detail}");
        return Err(if status.is_client_error() && status.as_u16() != 429 {
            RefreshError::AuthFailure(msg)
        } else {
            RefreshError::Transient(msg)
        });
    }

    let parsed: JsonValue = serde_json::from_str(&body)
        .map_err(|e| RefreshError::Transient(format!("Grok token response is not JSON: {e}")))?;
    apply_grok_token_response(auth, &parsed)
}

/// Patch a stored grok credential with a token-endpoint response.
///
/// Split from the HTTP call so the part that can DESTROY a credential is unit
/// tested: everything this function does not touch survives, and the one field
/// that must never be blanked is `refresh_token`.
///
/// Mirrors `build_grok_auth` (`oidc/protocol.rs:225`) for the fields a refresh
/// replaces, plus the keep-the-old-token rule at `oidc/refresh.rs:276-278`.
///
/// Returns whether the IdP ROTATED the refresh token (sent a new one). grok
/// tracks the same bit (`oidc/refresh.rs:274` `idp_rotated`); for us it is the
/// one thing about the IdP's behaviour a switch can report that is not
/// otherwise observable from disk afterwards.
fn apply_grok_token_response(
    auth: &mut JsonValue,
    parsed: &JsonValue,
) -> Result<bool, RefreshError> {
    let access_token = parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        // A 200 with no access_token is not something to guess about; treat it
        // as transient so the snapshot is written verbatim rather than blanked.
        .ok_or_else(|| RefreshError::Transient("Grok token response has no access_token".into()))?
        .to_string();

    let JsonValue::Object(map) = auth else {
        return Err(RefreshError::Transient(
            "Saved Grok credential is corrupt".into(),
        ));
    };
    let now = chrono::Utc::now();
    map.insert("key".into(), JsonValue::String(access_token));
    map.insert("create_time".into(), JsonValue::String(now.to_rfc3339()));
    match parsed.get("expires_in").and_then(|v| v.as_i64()) {
        Some(secs) => {
            let at = now + chrono::Duration::seconds(secs);
            map.insert("expires_at".into(), JsonValue::String(at.to_rfc3339()));
        }
        // `build_grok_auth` sets `expires_at: None` when the response omits
        // `expires_in`, which makes grok fall back to its 30-day TOKEN_TTL.
        // Leaving the PREVIOUS (already past) value would instead make every
        // read treat the fresh token as expired.
        None => {
            map.remove("expires_at");
        }
    }
    // Only overwrite when the IdP actually rotated — a missing field means
    // "keep using the one you have", NOT "you no longer have one".
    let rotated = parsed
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    if let Some(rt) = rotated {
        map.insert("refresh_token".into(), JsonValue::String(rt.into()));
    }
    Ok(rotated.is_some())
}

/// Persist a just-refreshed grok credential into its store entry, BEFORE the
/// live file is written.
///
/// Order is load-bearing (same rule as `persist_refreshed_claude_snapshot`):
/// the refresh may have spent the old refresh token, so the rotated one is now
/// the only way back into this account. Writing the store first means a crash
/// between the two writes leaves the *durable record* holding the live token —
/// recoverable by switching again — instead of leaving it only in a file we
/// never got to write.
fn persist_refreshed_grok_snapshot(
    id: &str,
    scope: &str,
    auth: &JsonValue,
) -> Result<(), Box<dyn Error>> {
    let mut store = read_store()?;
    // `id` is the entry `switch_account` already resolved, NOT one re-derived
    // from the payload: by the time this runs the refresh has SPENT the stored
    // token, so a lookup that misses would leave the rotated replacement only
    // in auth.json and the spent one in the store — silently recreating the
    // stale-snapshot failure this whole feature exists to prevent.
    let Some(slot) = store
        .iter_mut()
        .find(|e| str_field(e, "app") == Some("grok") && str_field(e, "id") == Some(id))
    else {
        return Err(format!("Saved Grok account {id} vanished mid-switch").into());
    };
    slot["payload"] = json!({ "scope": scope, "auth": auth });
    slot["savedAt"] = JsonValue::String(now_rfc3339());
    // A successful refresh proves the credential is live, so any stale
    // needs-relogin mark on it is wrong (same implicit clear as a re-save).
    if let JsonValue::Object(o) = slot {
        o.remove("needsRelogin");
    }
    write_store(store)
}

/// Restore a saved grok login.
///
/// Validates the snapshot by refreshing it BEFORE touching `auth.json`, the
/// same shape as `switch_codex` / `switch_claude`: a definitive 4xx means the
/// stored refresh token is dead, so the live login is left untouched and the
/// entry is flagged `needsRelogin` — the user sees the failure at click time
/// instead of being logged out hours later by grok's own refresh.
async fn switch_grok(id: &str, payload: &JsonValue) -> Result<(), Box<dyn Error>> {
    let scope = payload
        .get("scope")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("Saved Grok credential is missing its auth.json scope")?
        .to_string();
    let mut auth = payload
        .get("auth")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or("Saved Grok credential is corrupt")?;

    let path = grok_auth_path()?;
    // ONE lock across read-outgoing → re-snapshot → refresh → write-incoming.
    // The refresh is INSIDE it for the same reason grok holds this lock across
    // its own IdP call: two processes spending a refresh token concurrently is
    // the double-spend that trips rotation reuse detection.
    let _lock = acquire_grok_lock(&path).await?;

    // Re-snapshot the OUTGOING login before it is overwritten, so switching
    // back later restores the RT grok most recently rotated to instead of a
    // spent one. Only refreshes an EXISTING entry (never adds one — an unsaved
    // live login stays guarded by the page's confirm warning and the tray's
    // auto-save), mirroring switch_codex.
    if let Some(live) = read_grok_live_at(&path)? {
        let store = read_store()?;
        if store.iter().any(|e| {
            str_field(e, "app") == Some("grok") && str_field(e, "id") == Some(live.id.as_str())
        }) {
            save_grok_live(&live)?;
        }
    }

    // Short, non-identifying handle for the log lines below. NEVER log the
    // email or any token material — this file's whole subject is credentials.
    let who: String = auth
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(|id| id.chars().take(8).collect())
        .unwrap_or_else(|| "unknown".into());

    match refresh_grok_auth(&mut auth).await {
        Ok(idp_rotated) => {
            // Durable record first — see persist_refreshed_grok_snapshot.
            //
            // BEST-EFFORT on purpose: the refresh already spent the stored
            // token, so the rotated one in `auth` is the only working copy
            // left. Aborting here would throw it away and leave the account
            // needing a re-login; writing auth.json anyway puts it where grok
            // can use it, and the next switch AWAY recaptures it into the
            // store (the outgoing re-snapshot above). The store-before-live
            // ORDER still stands — it is about a crash, not a known failure.
            if let Err(e) = persist_refreshed_grok_snapshot(id, &scope, &auth) {
                log::warn!("grok switch: could not persist the refreshed snapshot: {e}");
            }
            // A switch SPENDS a refresh token, so it must leave a trace saying
            // so. `idp_rotated` is the only part of the exchange that cannot be
            // reconstructed from disk afterwards (a kept token and a rotated
            // one look identical once written).
            log::info!("grok switch: refreshed {who}, idp_rotated={idp_rotated}");
        }
        Err(RefreshError::AuthFailure(msg)) => {
            // Flagged BACKEND-side, like switch_claude: only this arm knows the
            // failure was a dead token rather than a lock or write error, and
            // the caller sees just a string.
            let _ = mark_account_relogin(id, true);
            log::warn!("grok switch: refresh rejected for {who}, flagged needsRelogin: {msg}");
            return Err(msg.into());
        }
        // 429 / 5xx / network: the server said "not now", which says nothing
        // about the token. Write the snapshot verbatim and let grok refresh it
        // itself — the pre-refresh behaviour, kept as the degraded path.
        Err(RefreshError::Transient(msg)) => {
            log::warn!("grok switch: refresh unavailable for {who}, writing snapshot as-is: {msg}");
        }
    }

    // Replace ONLY this scope key. auth.json is a scope-keyed map that can
    // also hold a plain API key (`xai::api_key`, `auth/storage.rs:426`) or an
    // enterprise OIDC login under a different issuer; restoring a whole
    // document would wipe whichever of those the snapshot predates.
    let mut doc = grok_doc_for_write(&path)?;
    if let JsonValue::Object(map) = &mut doc {
        map.insert(scope, auth);
    }
    let updated = serde_json::to_string_pretty(&doc)?;
    atomic_write_0600(&path, updated.as_bytes())?;
    Ok(())
}

/// Tauri-managed state allowing `cancel_grok_login` to abort an in-flight
/// `login_and_save_grok_account` call.
pub struct GrokLoginCancel(pub std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>);

pub const GROK_LOGIN_URL_EVENT: &str = "grok:login-url";
/// The device-flow user code, emitted alongside the URL. Grok's device login
/// shows a code the user confirms (or types) in the browser, and when the IdP
/// returns no `verification_uri_complete` the URL alone cannot complete the
/// login — so the code is surfaced rather than left in captured output.
pub const GROK_LOGIN_CODE_EVENT: &str = "grok:login-code";

/// What one line of `grok login --device-auth`'s prompt tells the frontend.
#[derive(Debug, PartialEq, Eq)]
enum GrokLoginPrompt {
    Url(String),
    Code(String),
}

/// Line-by-line reader for that prompt (`auth/device_code.rs:363-391`).
///
/// Extracted from the spawn closure ONLY so it can be tested: this runs on a
/// path that needs a real browser round-trip, so a mistake here is invisible
/// until a user hits it — which is exactly what happened. The first version
/// gated the code on `ends_with("code:")` and its own comment claimed "both
/// headings end in code:", which is false: the common one ends in `browser:`
/// ("Confirm this code in your browser:", printed whenever the IdP returns a
/// `verification_uri_complete`), so the code was never emitted in the case
/// that actually occurs.
#[derive(Default)]
struct GrokLoginPromptReader {
    expect_code: bool,
}

impl GrokLoginPromptReader {
    fn feed(&mut self, line: &str) -> Option<GrokLoginPrompt> {
        let trimmed = line.trim();
        let out = if trimmed.starts_with("https://") {
            Some(GrokLoginPrompt::Url(trimmed.to_string()))
        } else if self.expect_code && !trimmed.is_empty() {
            // The code is the next non-empty line after its heading, so its
            // own format is never assumed.
            self.expect_code = false;
            Some(GrokLoginPrompt::Code(trimmed.to_string()))
        } else {
            None
        };
        // THREE lines of this prompt end in a colon, so `ends_with(':')` alone
        // would arm on "To sign in, open this URL in your browser:" and then
        // report the browser-fallback notice as the code. Requiring "code"
        // narrows it to the two headings that really precede it — and both
        // spellings are covered, which is the bug above.
        if trimmed.ends_with(':') && trimmed.contains("code") {
            self.expect_code = true;
        }
        out
    }
}

pub async fn login_and_save_grok_account(
    app: tauri::AppHandle,
    cancel_state: &GrokLoginCancel,
) -> Result<String, String> {
    let path = grok_auth_path().map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Reserve the cancel slot (also the re-entrancy guard); self-clears on
    // every exit path via Drop.
    let (_login_slot, cancel_notify) = LoginSlot::reserve(&cancel_state.0)?;

    // Capture the outgoing login before a successful login overwrites this
    // scope entry — see `resnapshot_live_before_login`.
    let prev_active_id = resnapshot_live_before_login(CliApp::Grok).map_err(|e| e.to_string())?;

    // Original bytes for rollback (cancel / timeout / failure).
    let original_auth: Option<Vec<u8>> = std::fs::read(&path).ok();

    // NOTE: deliberately NO pre-clear. The codex flow blanks auth.json first
    // so codex's own `logout_with_revoke` finds nothing to revoke server-side;
    // grok's logout is purely LOCAL (`auth/flow.rs:992` `perform_logout` →
    // `remove_scope` / `clear`, and the crate has no revocation endpoint at
    // all), so saved snapshots' tokens are never at risk and there is nothing
    // to blank.
    //
    // `--device-auth` is REQUIRED, not a preference. The loopback branch runs
    // with `reauth=true`, which clears the credential UP FRONT, so abandoning
    // that login logs the user out (`auth/flow.rs:1093`); the device branch
    // passes `force_interactive`, which skips the clear, so a cancelled login
    // leaves the current account intact. It is also what prints the
    // verification URL, i.e. what makes the flow headless at all.
    let resolved = crate::providers::find_cli_binary("grok");
    let program: std::ffi::OsString = resolved
        .as_deref()
        .map(|p| p.as_os_str().to_os_string())
        .unwrap_or_else(|| "grok".into());
    let mut cmd = tokio::process::Command::new(&program);
    if let Some(path) = resolved
        .as_deref()
        .and_then(crate::providers::augmented_path_for)
    {
        cmd.env("PATH", path);
    }
    cmd.args(["login", "--device-auth"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Managed: silent (no console window on Windows), killable as a whole
    // process group, and torn down if Termory quits mid-login.
    let mut child = match crate::process::spawn_managed(cmd) {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "grok is not installed or not in PATH".to_string()
            } else {
                format!("Failed to launch grok: {e}")
            };
            return Err(msg);
        }
    };

    // The device prompt goes to STDERR (`auth/device_code.rs:365-391`), so
    // that is the stream read line-by-line; stdout only carries the trailing
    // blank line `run_cli_login` prints on success.
    let stderr_pipe = child.stderr().expect("stderr piped");
    let app_clone = app.clone();
    let stderr_task: tokio::task::JoinHandle<String> = tokio::spawn(async move {
        use tauri::Emitter as _;
        use tokio::io::AsyncBufReadExt as _;
        let reader = tokio::io::BufReader::new(stderr_pipe);
        let mut lines = reader.lines();
        let mut collected = String::new();
        let mut prompt = GrokLoginPromptReader::default();
        while let Ok(Some(line)) = lines.next_line().await {
            match prompt.feed(&line) {
                Some(GrokLoginPrompt::Url(url)) => {
                    let _ = app_clone.emit(GROK_LOGIN_URL_EVENT, url);
                }
                Some(GrokLoginPrompt::Code(code)) => {
                    let _ = app_clone.emit(GROK_LOGIN_CODE_EVENT, code);
                }
                None => {}
            }
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(line.trim());
        }
        collected
    });
    let stdout_pipe = child.stdout().expect("stdout piped");
    let stdout_task: tokio::task::JoinHandle<String> = tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut buf = String::new();
        let mut reader = stdout_pipe;
        let _ = reader.read_to_string(&mut buf).await;
        buf
    });

    // Wait up to 5 minutes for the browser roundtrip (same budget as the
    // codex / claude flows).
    let status = tokio::select! {
        r = child.wait() => match r {
            Ok(s) => s,
            Err(e) => {
                stderr_task.abort();
                stdout_task.abort();
                restore_auth(&path, original_auth.as_deref());
                return Err(format!("grok login error: {e}"));
            }
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            child.terminate(crate::process::CANCEL_GRACE).await;
            stderr_task.abort();
            stdout_task.abort();
            restore_auth(&path, original_auth.as_deref());
            return Err("grok login timed out after 5 minutes".into());
        },
        _ = cancel_notify.notified() => {
            child.terminate(crate::process::CANCEL_GRACE).await;
            stderr_task.abort();
            stdout_task.abort();
            restore_auth(&path, original_auth.as_deref());
            return Err("Login cancelled".into());
        },
    };

    let stderr_msg = stderr_task.await.unwrap_or_default();
    let stdout_msg = stdout_task.await.unwrap_or_default();

    if !status.success() {
        restore_auth(&path, original_auth.as_deref());
        let detail = if stderr_msg.trim().is_empty() {
            stdout_msg
        } else {
            stderr_msg
        };
        return Err(format!(
            "grok login failed (exit {}): {}",
            status.code().unwrap_or(-1),
            detail.trim()
        ));
    }

    // Save the new account (reads the credential grok just wrote).
    let Some(new_live) = read_grok_live_at(&path).map_err(|e| e.to_string())? else {
        restore_auth(&path, original_auth.as_deref());
        return Err("Login finished but no Grok credential was written".into());
    };
    let new_id = new_live.id.clone();
    if let Err(e) = save_grok_live(&new_live) {
        restore_auth(&path, original_auth.as_deref());
        return Err(format!("Login succeeded but could not save account: {e}"));
    }

    // Restore the previously active account (the login process has exited, so
    // nothing is running on the new login). Skipped when the user re-logged
    // into the SAME account — the restore would be a no-op write.
    if let Some(prev_id) = prev_active_id.filter(|id| *id != new_id) {
        if let Err(e) = switch_account(prev_id.clone()).await {
            // Don't fail the operation — the new account was saved. No
            // needsRelogin either: grok's switch never validates over the
            // network, so any failure here is a lock or write error, and
            // flagging one would trap a healthy account.
            log::warn!("Failed to restore previous grok account {prev_id}: {e}");
        }
    }

    Ok(new_id)
}

/// Fire the cancel notify for an in-flight `login_and_save_grok_account`.
pub async fn cancel_grok_login(cancel_state: &GrokLoginCancel) -> Result<(), String> {
    // `as_ref`, NOT `take` — see `cancel_claude_login` for why the slot must
    // stay occupied until the cancelled flow's own Drop clears it.
    match cancel_state.0.lock().unwrap().as_ref() {
        Some(n) => {
            n.notify_one();
            Ok(())
        }
        None => Err("No grok login in progress".into()),
    }
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
#[derive(Debug)]
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

    /// A one-shot stand-in for codex's login server: accepts a single
    /// connection, hands the raw request back over the channel, and answers
    /// 200 the way `/cancel` does. Bound to port 0 so the OS picks a free
    /// one — the tests must never touch the real `CODEX_LOGIN_PORT`, which
    /// may have an actual login on it.
    fn stub_cancel_server() -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let port = listener.local_addr().expect("addr").port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 256];
                let n = stream.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        (port, rx)
    }

    /// A port nothing is listening on: bind then immediately release it.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }

    /// The request must be exactly what codex's own `send_cancel_request`
    /// sends (server.rs:545) — a wrong path or verb would be answered with
    /// a 404 by the real server and cancel nothing.
    #[test]
    fn send_codex_cancel_issues_the_cancel_request() {
        let (port, rx) = stub_cancel_server();
        assert!(send_codex_cancel(port), "should report the request as sent");
        let req = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("stub received a request");
        assert!(
            req.starts_with("GET /cancel HTTP/1.1\r\n"),
            "unexpected request line: {req:?}"
        );
    }

    /// Nothing listening ⇒ `false`, which is what tells the caller to skip
    /// the grace period and kill straight away.
    #[test]
    fn send_codex_cancel_reports_false_when_nothing_listens() {
        assert!(!send_codex_cancel(closed_port()));
    }

    /// With a server answering, the child is given its grace period and is
    /// allowed to exit on its OWN — a successful exit status proves it was
    /// not killed.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_codex_login_lets_the_child_exit_on_its_own() {
        // Keep `process::shutdown_all` (exercised by a parallel test in
        // process.rs) from draining the global registry and killing this
        // child out from under us.
        let _hold = crate::process::hold_children();
        let (port, _rx) = stub_cancel_server();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 0.3");
        let mut child = crate::process::spawn_managed(cmd).expect("spawn probe");

        stop_codex_login(&mut child, port).await;

        let status = child.wait().await.expect("wait");
        assert!(
            status.success(),
            "child should have exited on its own, not been killed: {status:?}"
        );
    }

    /// With nothing listening there is no exit coming, so the CANCEL's
    /// grace period must be SKIPPED rather than sat out — it is a
    /// user-facing cancel.
    ///
    /// The `terminate` that follows has a grace period of its own, but
    /// that one is the SIGTERM→SIGKILL escalation and is near-instant for
    /// a process with default handlers (covered by
    /// `process::tests::terminate_escalates_to_kill_when_term_is_ignored`).
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_codex_login_kills_at_once_when_nothing_answers() {
        // Keep `process::shutdown_all` (exercised by a parallel test in
        // process.rs) from draining the global registry and killing this
        // child out from under us.
        let _hold = crate::process::hold_children();
        let port = closed_port();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");
        let mut child = crate::process::spawn_managed(cmd).expect("spawn probe");

        let started = std::time::Instant::now();
        stop_codex_login(&mut child, port).await;
        let elapsed = started.elapsed();

        let status = child.wait().await.expect("wait");
        assert!(
            !status.success(),
            "child should have been killed, not exited cleanly: {status:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "waited out the grace period with no server to wait for: {elapsed:?}"
        );
    }

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

    // ── Grok Build ────────────────────────────────────────────────────────

    const GROK_SCOPE: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";

    /// One `GrokAuth` value shaped like the real thing (field names verified
    /// against `auth/model.rs` and a live 0.2.99 install).
    fn grok_auth_value(user_id: &str, email: &str, refresh: &str) -> JsonValue {
        json!({
            "key": format!("access-{user_id}"),
            "auth_mode": "oidc",
            "create_time": "2026-07-31T00:55:48.732571Z",
            "user_id": user_id,
            "email": email,
            "first_name": "Jane",
            "last_name": "Doe",
            "refresh_token": refresh,
            "expires_at": "2026-07-31T06:55:48.732571Z",
            "oidc_issuer": "https://auth.x.ai",
            "oidc_client_id": "b1a00492-073a-47ea-816f-4c329264a828",
        })
    }

    /// Write a live grok auth.json holding one xAI login plus the plain
    /// API-key sibling scope a `grok login --api-key` user would also have.
    fn write_grok_auth(home: &Path, user_id: &str, email: &str, refresh: &str) {
        let doc = json!({
            GROK_SCOPE: grok_auth_value(user_id, email, refresh),
            "xai::api_key": {
                "key": "xai-user-own-key",
                "auth_mode": "api_key",
                "create_time": "2026-07-01T00:00:00Z",
                "user_id": "",
            },
        });
        let dir = home.join(".grok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    fn grok_live_doc(home: &Path) -> JsonValue {
        serde_json::from_slice(&std::fs::read(home.join(".grok/auth.json")).unwrap()).unwrap()
    }

    /// Installs a `refresh_grok_auth` stub for the test's lifetime. Held
    /// alongside `lock_home()`, which serializes it with every other test.
    struct GrokRefreshGuard;
    impl GrokRefreshGuard {
        fn set(stub: GrokRefreshStub) -> Self {
            *GROK_REFRESH_STUB.lock().unwrap() = Some(stub);
            GrokRefreshGuard
        }
        /// The IdP accepted the refresh and rotated the token.
        fn rotating() -> Self {
            Self::set(GrokRefreshStub::Response(
                json!({ "access_token": "at-fresh", "refresh_token": "rt-rotated", "expires_in": 21600 }),
            ))
        }
    }
    impl Drop for GrokRefreshGuard {
        fn drop(&mut self) {
            *GROK_REFRESH_STUB.lock().unwrap() = None;
        }
    }

    fn stored_grok_refresh(id: &str) -> Option<String> {
        read_store()
            .unwrap()
            .iter()
            .find(|e| str_field(e, "id") == Some(id))?
            .pointer("/payload/auth/refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    /// The API-key scope carries an empty `user_id`, which is what keeps it
    /// from being mistaken for a login — assert that rather than a hardcoded
    /// key name, since that is the rule the code actually applies.
    #[test]
    fn grok_live_ignores_the_api_key_scope_and_composes_a_name() {
        let _g = lock_home();
        let tmp = tempdir("grok-live");
        let _h = override_home(&tmp);
        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");

        let live = read_grok_live().unwrap().expect("a live login");
        assert_eq!(live.id, "user-a");
        assert_eq!(live.email.as_deref(), Some("a@example.com"));
        assert_eq!(live.name.as_deref(), Some("Jane Doe"));
        assert_eq!(live.scope, GROK_SCOPE);
    }

    /// An auth.json holding ONLY an API key is not a login — the account
    /// section must stay empty rather than show a nameless row.
    #[test]
    fn grok_api_key_only_auth_json_is_not_a_login() {
        let _g = lock_home();
        let tmp = tempdir("grok-apikey-only");
        let _h = override_home(&tmp);
        let dir = tmp.join(".grok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"xai::api_key":{"key":"xai-k","auth_mode":"api_key","user_id":""}}"#,
        )
        .unwrap();

        assert!(read_grok_live().unwrap().is_none());
        assert!(list_accounts(CliApp::Grok).unwrap().current.is_none());
    }

    /// The pre-login snapshot must be UNCONDITIONAL, for every CLI. A login
    /// destroys the outgoing credential, and all three rotate their refresh
    /// token — so an "already saved, nothing to do" shortcut leaves the restore
    /// at the end of the flow reaching for a rotated-away token and flags an
    /// account the user never touched. (`login_and_save_codex_account` really
    /// did take that shortcut until 2026-07-31: the rotation hazard was fixed
    /// in `switch_codex` but never carried back to the login path.)
    #[test]
    fn resnapshot_before_login_refreshes_an_already_saved_codex_entry() {
        let _g = lock_home();
        let tmp = tempdir("codex-prelogin");
        let _h = override_home(&tmp);

        // Saved while the live login held its first token…
        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        let saved_first = read_store().unwrap()[0]
            .pointer("/payload/tokens/access_token")
            .cloned();

        // …then codex ran and rotated it on disk.
        let path = tmp.join(".codex/auth.json");
        let mut doc: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["tokens"]["access_token"] = json!("access-rotated");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        let id = resnapshot_live_before_login(CliApp::Codex).unwrap();
        assert_eq!(id.as_deref(), Some("acct-a"), "returns the id to restore");
        assert_eq!(
            read_store().unwrap()[0].pointer("/payload/tokens/access_token"),
            Some(&json!("access-rotated")),
            "an already-saved entry must still be refreshed from disk"
        );
        assert_ne!(
            saved_first.as_ref(),
            Some(&json!("access-rotated")),
            "guard: the fixture must actually have changed"
        );
    }

    /// The tray's helper answers a different question and must KEEP its
    /// only-if-missing shortcut — there, freshness is already handled by
    /// `switch_*`'s own outgoing re-snapshot, and an extra write on every
    /// menu-driven switch buys nothing.
    #[test]
    fn auto_save_stays_only_if_missing() {
        let _g = lock_home();
        let tmp = tempdir("codex-autosave");
        let _h = override_home(&tmp);

        write_codex_auth(&tmp, "a@example.com", "pro", "acct-a");
        save_current_account(CliApp::Codex).unwrap();
        let saved_at = str_field(&read_store().unwrap()[0], "savedAt")
            .unwrap()
            .to_string();

        let path = tmp.join(".codex/auth.json");
        let mut doc: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["tokens"]["access_token"] = json!("access-rotated");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        auto_save_unsaved_live_account(CliApp::Codex).unwrap();
        assert_eq!(
            str_field(&read_store().unwrap()[0], "savedAt"),
            Some(saved_at.as_str()),
            "an already-saved entry must be left untouched by the tray helper"
        );
    }

    /// THE load-bearing rule. grok rotates its refresh token under IdP reuse
    /// detection, so switching away must recapture the LIVE token first:
    /// restoring a stale one later is not a soft failure, it revokes the whole
    /// token family. Here the store's copy of A is deliberately older than
    /// what a running grok left on disk.
    #[tokio::test]
    async fn grok_switch_resnapshots_the_outgoing_login_first() {
        let _g = lock_home();
        let tmp = tempdir("grok-resnapshot");
        let _h = override_home(&tmp);
        let _r = GrokRefreshGuard::rotating();

        // A is live and saved (store and disk agree at rt-a1).
        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");
        save_current_account(CliApp::Grok).unwrap();
        // B is saved too.
        write_grok_auth(&tmp, "user-b", "b@example.com", "rt-b1");
        save_current_account(CliApp::Grok).unwrap();
        // Back on A, and a grok run rotates A's refresh token on disk. The
        // store still holds rt-a1 — the stale copy.
        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a2");
        assert_eq!(stored_grok_refresh("user-a").as_deref(), Some("rt-a1"));

        switch_account("user-b".to_string()).await.unwrap();

        assert_eq!(
            stored_grok_refresh("user-a").as_deref(),
            Some("rt-a2"),
            "switching away must recapture the live refresh token, or the \
             switch back would spend a rotated-away one"
        );
        assert_eq!(
            grok_live_doc(&tmp).pointer(&format!("/{}/user_id", GROK_SCOPE.replace('/', "~1"))),
            Some(&json!("user-b")),
            "and the incoming account must actually be live"
        );
    }

    /// The switch replaces one scope key, not the document: a plain API key
    /// (or an enterprise OIDC login under another issuer) must survive it.
    #[tokio::test]
    async fn grok_switch_preserves_sibling_scopes_and_writes_verbatim() {
        let _g = lock_home();
        let tmp = tempdir("grok-siblings");
        let _h = override_home(&tmp);
        // Verbatim write is the TRANSIENT path — a successful refresh would
        // (correctly) change the token, which is covered by its own test.
        let _r = GrokRefreshGuard::set(GrokRefreshStub::Transient);

        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");
        save_current_account(CliApp::Grok).unwrap();
        write_grok_auth(&tmp, "user-b", "b@example.com", "rt-b1");
        save_current_account(CliApp::Grok).unwrap();

        switch_account("user-a".to_string()).await.unwrap();

        let live = grok_live_doc(&tmp);
        assert_eq!(
            live.get("xai::api_key").and_then(|v| v.get("key")),
            Some(&json!("xai-user-own-key")),
            "a whole-document restore would have wiped the user's API key"
        );
        assert_eq!(
            live.get(GROK_SCOPE),
            Some(&grok_auth_value("user-a", "a@example.com", "rt-a1")),
            "the snapshot must be written back verbatim — no refresh, no rewrite"
        );
    }

    /// The whole point of refreshing at switch time: a dead stored token must
    /// be caught BEFORE auth.json is touched, so the user gets an error at
    /// click time instead of being logged out hours later by grok's own
    /// refresh. The live login must be left exactly as it was.
    #[tokio::test]
    async fn grok_switch_on_a_dead_token_writes_nothing_and_flags_relogin() {
        let _g = lock_home();
        let tmp = tempdir("grok-dead-token");
        let _h = override_home(&tmp);
        let _r = GrokRefreshGuard::set(GrokRefreshStub::AuthFailure);

        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");
        save_current_account(CliApp::Grok).unwrap();
        write_grok_auth(&tmp, "user-b", "b@example.com", "rt-b1");
        save_current_account(CliApp::Grok).unwrap();
        let live_before = std::fs::read(tmp.join(".grok/auth.json")).unwrap();

        let err = switch_account("user-a".to_string()).await.unwrap_err();
        assert!(err.to_string().contains("stubbed auth failure"));
        assert_eq!(
            std::fs::read(tmp.join(".grok/auth.json")).unwrap(),
            live_before,
            "a failed validation must not touch the live credential"
        );
        let view = list_accounts(CliApp::Grok).unwrap();
        let a = view.accounts.iter().find(|x| x.id == "user-a").unwrap();
        assert!(
            a.needs_relogin,
            "the dead entry must be flagged for re-login"
        );
    }

    /// A successful refresh must land the rotated token in the STORE as well
    /// as the live file — the refresh may have spent the old one, so an entry
    /// left holding it would be dead on the next switch back.
    #[tokio::test]
    async fn grok_switch_persists_the_rotated_token_to_both_places() {
        let _g = lock_home();
        let tmp = tempdir("grok-rotated");
        let _h = override_home(&tmp);
        let _r = GrokRefreshGuard::rotating();

        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");
        save_current_account(CliApp::Grok).unwrap();
        write_grok_auth(&tmp, "user-b", "b@example.com", "rt-b1");
        save_current_account(CliApp::Grok).unwrap();

        switch_account("user-a".to_string()).await.unwrap();

        let scope_ptr = format!("/{}", GROK_SCOPE.replace('/', "~1"));
        let live = grok_live_doc(&tmp);
        assert_eq!(
            live.pointer(&format!("{scope_ptr}/refresh_token")),
            Some(&json!("rt-rotated")),
            "the live file must carry the rotated token"
        );
        assert_eq!(
            live.pointer(&format!("{scope_ptr}/key")),
            Some(&json!("at-fresh"))
        );
        assert_eq!(
            stored_grok_refresh("user-a").as_deref(),
            Some("rt-rotated"),
            "and so must the store, or switching back would spend a dead one"
        );
    }

    /// A refresh SPENDS the stored token, so once it succeeds the rotated
    /// replacement in memory is the only working copy. If the store write then
    /// fails, auth.json must still be written — aborting would discard it and
    /// leave the account needing a re-login, and the next switch away recaptures
    /// it into the store anyway. Here the store entry is deleted mid-switch
    /// (the switch already holds the payload), which is what makes the persist
    /// step fail.
    #[tokio::test]
    async fn grok_switch_still_writes_the_live_file_when_the_store_write_fails() {
        let _g = lock_home();
        let tmp = tempdir("grok-persist-fails");
        let _h = override_home(&tmp);
        let _r = GrokRefreshGuard::rotating();

        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");
        save_current_account(CliApp::Grok).unwrap();
        write_grok_auth(&tmp, "user-b", "b@example.com", "rt-b1");
        save_current_account(CliApp::Grok).unwrap();

        // Take the payload the way switch_account does, then drop the entry so
        // `persist_refreshed_grok_snapshot` cannot find it.
        let payload = read_store()
            .unwrap()
            .into_iter()
            .find(|e| str_field(e, "id") == Some("user-a"))
            .and_then(|e| e.get("payload").cloned())
            .unwrap();
        delete_account("user-a".to_string()).unwrap();

        switch_grok("user-a", &payload).await.unwrap();

        assert_eq!(
            grok_live_doc(&tmp)
                .pointer(&format!("/{}/refresh_token", GROK_SCOPE.replace('/', "~1"))),
            Some(&json!("rt-rotated")),
            "the rotated token must reach the live file even when the store write fails"
        );
    }

    /// 429 / 5xx / offline says nothing about the token. Switching must still
    /// work (verbatim snapshot, grok refreshes it itself later) and must NOT
    /// flag the account — that would trap a healthy login behind a disabled
    /// Switch button.
    #[tokio::test]
    async fn grok_switch_survives_a_transient_refresh_failure_without_flagging() {
        let _g = lock_home();
        let tmp = tempdir("grok-transient");
        let _h = override_home(&tmp);
        let _r = GrokRefreshGuard::set(GrokRefreshStub::Transient);

        write_grok_auth(&tmp, "user-a", "a@example.com", "rt-a1");
        save_current_account(CliApp::Grok).unwrap();
        write_grok_auth(&tmp, "user-b", "b@example.com", "rt-b1");
        save_current_account(CliApp::Grok).unwrap();

        switch_account("user-a".to_string()).await.unwrap();

        assert_eq!(
            grok_live_doc(&tmp)
                .pointer(&format!("/{}/refresh_token", GROK_SCOPE.replace('/', "~1"))),
            Some(&json!("rt-a1")),
            "the snapshot should be written as-is when the server said 'not now'"
        );
        let view = list_accounts(CliApp::Grok).unwrap();
        let a = view.accounts.iter().find(|x| x.id == "user-a").unwrap();
        assert!(
            !a.needs_relogin,
            "a transient failure must not flag the account"
        );
    }

    /// A second acquire must fail while the first guard is alive, and the
    /// holder info must be the `PID:TS` payload grok's waiters parse — stale
    /// holder info is what makes grok break a lock it should have waited on.
    #[test]
    fn grok_auth_lock_is_exclusive_and_stamps_holder_info() {
        let tmp = tempdir("grok-lock");
        let auth_path = tmp.join("auth.json");

        let held = GrokAuthLock::try_acquire(&auth_path).unwrap();
        assert!(held.is_some(), "first acquire should succeed");
        assert!(
            GrokAuthLock::try_acquire(&auth_path).unwrap().is_none(),
            "a second acquire must not get the lock while the first is held"
        );

        // Reading the stamp back is UNIX-ONLY, and not for want of trying on
        // Windows: `File::try_lock` is flock there and `LockFileEx` here, and
        // `LockFileEx` is MANDATORY — while the guard above holds the range,
        // any other handle reading it fails with os error 33, including this
        // one. (Caught by CI, which is the only place this runs on Windows.)
        //
        // The production path is unaffected: the stamp is written through the
        // handle that owns the lock. What a grok waiter can read on Windows is
        // grok's own question, not ours — its waiter reads the same way
        // (`auth/manager/lock.rs:234`, no platform branch) and every one of
        // its ~20 lock tests is `#[cfg(unix)]` too. Mirroring the protocol
        // exactly, platform quirks included, beats inventing a Windows-only
        // divergence with nothing to validate it against.
        #[cfg(unix)]
        {
            let info = std::fs::read_to_string(tmp.join("auth.json.lock")).unwrap();
            let (pid, ts) = info.split_once(':').expect("holder info is PID:TS");
            assert_eq!(pid.parse::<u32>().unwrap(), std::process::id());
            assert!(ts.parse::<u64>().unwrap() > 0);
        }

        drop(held);
        assert!(
            GrokAuthLock::try_acquire(&auth_path).unwrap().is_some(),
            "dropping the guard must release the lock"
        );
        assert!(
            tmp.join("auth.json.lock").exists(),
            "the lock FILE must survive a release — unlinking it is grok's \
             break-a-stuck-holder signal, not a release"
        );
    }

    /// THE destructive case. The IdP rotates only sometimes; a response with
    /// no `refresh_token` means "keep the one you have". Treating the missing
    /// field as a value would blank the only way back into the account —
    /// grok's own refresher guards this at `oidc/refresh.rs:276-278`.
    #[test]
    fn grok_refresh_without_rotation_keeps_the_existing_refresh_token() {
        let mut auth = grok_auth_value("user-a", "a@example.com", "rt-old");
        let rotated = apply_grok_token_response(
            &mut auth,
            &json!({ "access_token": "at-new", "expires_in": 21600 }),
        )
        .unwrap();

        assert_eq!(
            auth["refresh_token"],
            json!("rt-old"),
            "must not be blanked"
        );
        assert_eq!(auth["key"], json!("at-new"));
        assert!(!rotated, "no new token in the response means no rotation");
    }

    #[test]
    fn grok_refresh_stores_a_rotated_refresh_token() {
        let mut auth = grok_auth_value("user-a", "a@example.com", "rt-old");
        let rotated = apply_grok_token_response(
            &mut auth,
            &json!({ "access_token": "at-new", "refresh_token": "rt-new" }),
        )
        .unwrap();
        assert_eq!(auth["refresh_token"], json!("rt-new"));
        assert!(rotated, "a new token in the response IS a rotation");
    }

    /// A refresh replaces four fields and must leave the rest of the
    /// credential alone — grok carries the profile over rather than
    /// re-fetching it (`oidc/refresh.rs:255-272`), and patching in place is
    /// what gives us that for free.
    #[test]
    fn grok_refresh_preserves_every_other_credential_field() {
        let mut auth = grok_auth_value("user-a", "a@example.com", "rt-old");
        let before = auth.clone();
        apply_grok_token_response(
            &mut auth,
            &json!({ "access_token": "at-new", "expires_in": 21600 }),
        )
        .unwrap();

        for key in [
            "user_id",
            "email",
            "first_name",
            "last_name",
            "auth_mode",
            "oidc_issuer",
            "oidc_client_id",
        ] {
            assert_eq!(auth[key], before[key], "{key} must survive a refresh");
        }
        // And the ones that SHOULD move actually moved.
        assert_ne!(auth["key"], before["key"]);
        assert_ne!(auth["expires_at"], before["expires_at"]);
    }

    /// No `expires_in` → the stale `expires_at` must be REMOVED, not left in
    /// place. `build_grok_auth` sets it to None (grok then falls back to its
    /// 30-day TTL); keeping the old, already-past value would make every read
    /// treat the freshly minted token as expired.
    #[test]
    fn grok_refresh_without_expires_in_drops_the_stale_expiry() {
        let mut auth = grok_auth_value("user-a", "a@example.com", "rt-old");
        apply_grok_token_response(&mut auth, &json!({ "access_token": "at-new" })).unwrap();
        assert!(auth.get("expires_at").is_none());
    }

    /// A 200 with no access token is not something to guess about: transient,
    /// so the caller writes the snapshot verbatim instead of blanking it.
    #[test]
    fn grok_refresh_without_an_access_token_is_transient_and_changes_nothing() {
        let mut auth = grok_auth_value("user-a", "a@example.com", "rt-old");
        let before = auth.clone();
        let err =
            apply_grok_token_response(&mut auth, &json!({ "token_type": "Bearer" })).unwrap_err();
        assert!(matches!(err, RefreshError::Transient(_)));
        assert_eq!(
            auth, before,
            "a failed refresh must not mutate the credential"
        );
    }

    /// The endpoint is pinned rather than discovered at runtime, so pin it in
    /// a test too: the value came from auth.x.ai's live discovery document,
    /// and this is where refresh tokens get POSTed — a typo would send live
    /// credentials to whatever host the typo names.
    #[test]
    fn grok_token_endpoint_is_the_discovered_one() {
        assert_eq!(GROK_TOKEN_ENDPOINT, "https://auth.x.ai/oauth2/token");
    }

    /// Replays `grok login --device-auth`'s prompt VERBATIM
    /// (`auth/device_code.rs:363-391`), in the shape it takes when the IdP
    /// returns a `verification_uri_complete` — the common case, and the one
    /// the first version of this parser got wrong: its heading ends in
    /// `browser:`, not `code:`, so the code was never emitted.
    #[test]
    fn grok_login_prompt_reads_the_url_and_the_confirm_variant_code() {
        let mut r = GrokLoginPromptReader::default();
        let mut seen = Vec::new();
        for line in [
            "",
            "To sign in, open this URL in your browser:",
            "",
            "  https://accounts.x.ai/device?code=ABCD-EFGH",
            "",
            "Confirm this code in your browser:",
            "",
            "  ABCD-EFGH",
            "",
            "\u{1b}[90mOnly continue with a code you requested. Don't share it with anyone.\u{1b}[0m",
            "",
            "Waiting for authorization...",
        ] {
            if let Some(ev) = r.feed(line) {
                seen.push(ev);
            }
        }
        assert_eq!(
            seen,
            vec![
                GrokLoginPrompt::Url("https://accounts.x.ai/device?code=ABCD-EFGH".into()),
                GrokLoginPrompt::Code("ABCD-EFGH".into()),
            ]
        );
    }

    /// The other branch — no `verification_uri_complete`, so grok prints
    /// "Then enter this code:" and the user must type it. Also carries the
    /// browser-fallback notice, which sits between the URL and the heading.
    #[test]
    fn grok_login_prompt_reads_the_enter_variant_and_ignores_the_fallback_notice() {
        let mut r = GrokLoginPromptReader::default();
        let mut seen = Vec::new();
        for line in [
            "",
            "To sign in, open this URL in your browser:",
            "",
            "  https://accounts.x.ai/device",
            "",
            "  (Could not open browser automatically — open the URL above manually.)",
            "",
            "Then enter this code:",
            "",
            "  WXYZ-1234",
            "",
            "Waiting for authorization...",
        ] {
            if let Some(ev) = r.feed(line) {
                seen.push(ev);
            }
        }
        assert_eq!(
            seen,
            vec![
                GrokLoginPrompt::Url("https://accounts.x.ai/device".into()),
                GrokLoginPrompt::Code("WXYZ-1234".into()),
            ],
            "the browser-fallback notice must not be mistaken for the code"
        );
    }

    /// The URL heading also ends in a colon, so arming on the colon alone
    /// would report the very next non-empty line — the fallback notice — as
    /// the user's code. Pinning it separately because widening that condition
    /// is the obvious "fix" for the bug the tests above cover.
    #[test]
    fn grok_login_prompt_url_heading_never_arms_the_code_reader() {
        let mut r = GrokLoginPromptReader::default();
        assert_eq!(r.feed("To sign in, open this URL in your browser:"), None);
        assert!(
            !r.expect_code,
            "a heading without \"code\" must not arm the reader"
        );
        assert_eq!(r.feed("  (Could not open browser automatically.)"), None);
    }

    /// A corrupt auth.json is moved aside, never silently overwritten: those
    /// bytes are the user's only copy of a credential we could not parse.
    #[test]
    fn grok_corrupt_auth_json_is_backed_up_before_a_write() {
        let tmp = tempdir("grok-corrupt");
        let path = tmp.join("auth.json");
        std::fs::write(&path, b"{not json at all").unwrap();

        let doc = grok_doc_for_write(&path).unwrap();
        assert_eq!(doc, json!({}), "recovery starts from a fresh map");

        let backup = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("auth.json.corrupt.")
            })
            .expect("corrupt file must be renamed aside");
        assert_eq!(std::fs::read(backup.path()).unwrap(), b"{not json at all");
    }

    /// A missing or empty file is normal (grok tolerates both), so it must
    /// not be mistaken for corruption and generate a spurious backup.
    #[test]
    fn grok_missing_or_empty_auth_json_needs_no_backup() {
        let tmp = tempdir("grok-empty");
        let path = tmp.join("auth.json");
        assert_eq!(grok_doc_for_write(&path).unwrap(), json!({}));

        std::fs::write(&path, b"   \n").unwrap();
        assert_eq!(grok_doc_for_write(&path).unwrap(), json!({}));
        assert!(
            !std::fs::read_dir(&tmp)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "an empty file is valid recovery state, not corruption"
        );
    }

    /// `$GROK_HOME` moves the credential like it moves every other grok path.
    #[test]
    fn grok_home_env_redirects_credential_path() {
        let _g = lock_home();
        let tmp = tempdir("grok-home-env");
        let _h = override_home(&tmp);
        let custom = tmp.join("relocated-grok");
        std::fs::create_dir_all(&custom).unwrap();
        let _gh = EnvVarGuard::set("GROK_HOME", &custom);

        std::fs::write(
            custom.join("auth.json"),
            serde_json::to_string_pretty(&json!({
                GROK_SCOPE: grok_auth_value("user-moved", "moved@example.com", "rt-m"),
            }))
            .unwrap(),
        )
        .unwrap();

        let live = read_grok_live().unwrap().expect("login under GROK_HOME");
        assert_eq!(live.id, "user-moved");
        assert!(
            !tmp.join(".grok/auth.json").exists(),
            "nothing should have been read from or written to the default path"
        );
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
