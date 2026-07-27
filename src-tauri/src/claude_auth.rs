//! Claude Code credential storage — the read/write half of Claude
//! multi-account management.
//!
//! Claude Code stores its OAuth credential through a two-tier
//! `SecureStorage` (`src/utils/secureStorage/`): on macOS the **Keychain**
//! with the plaintext file as a fallback, everywhere else the file alone.
//! This module mirrors that layer so Termory reads and writes exactly what
//! the CLI does.
//!
//! ## Why the Keychain half is mandatory on macOS
//!
//! `createFallbackStorage.read()` (fallbackStorage.ts) returns the Keychain
//! value whenever it is non-null and only then falls through to the file:
//!
//! ```text
//! const result = primary.read()
//! if (result !== null && result !== undefined) return result
//! return secondary.read() || {}
//! ```
//!
//! So on macOS writing only `.credentials.json` is a no-op — the stale
//! Keychain entry shadows it. Deleting the Keychain entry to force the file
//! path doesn't work either: the next `update()` sees `primaryDataBefore ===
//! null`, writes the Keychain again and **deletes the file** — so that
//! arrangement survives only until Claude's next token refresh.
//!
//! ## Why `security` and not the Security framework
//!
//! Claude writes the Keychain by spawning `/usr/bin/security`
//! (macOsKeychainStorage.ts:117), so the item's ACL trusts that binary.
//! Termory spawns the same binary, so the ACL matches and no authorization
//! prompt appears. Going through `Security.framework` instead would present
//! `Termory.app` as the caller — an identity the item's ACL does not know,
//! which prompts the user on every switch.

use serde_json::Value as JsonValue;
use std::error::Error;
use std::path::PathBuf;

#[cfg(all(target_os = "macos", not(test)))]
use crate::providers::output_with_timeout_stdin;

// The Keychain tier is gated `all(target_os = "macos", not(test))`
// throughout: unit tests must never touch the developer's (or CI runner's)
// REAL login keychain — `security add-generic-password` would create actual
// entries there, and a locked CI keychain would fail the write path. Test
// builds therefore run file-only on every OS; the Keychain primitives'
// behavior (exit codes, `-U` overwrite, stdin form) was verified live
// against `security(1)` and is re-verified on a real machine, not in
// `cargo test`.

/// `security(1)` exit code for `errSecItemNotFound` — the entry does not
/// exist. Both `find-generic-password` and `delete-generic-password` use it,
/// which is what makes a delete of a missing entry naturally idempotent.
#[cfg(all(target_os = "macos", not(test)))]
const SEC_ITEM_NOT_FOUND: i32 = 44;

/// `security show-keychain-info` exit code meaning the keychain is locked.
/// Same probe Claude Code uses (`isMacOsKeychainLocked`,
/// macOsKeychainStorage.ts:211).
#[cfg(all(target_os = "macos", not(test)))]
const SEC_KEYCHAIN_LOCKED: i32 = 36;

/// `security -i` reads stdin with a 4096-byte `fgets` buffer; a longer line is
/// truncated mid-argument, which fails the command while **leaving the
/// previous entry intact** — the exact shape of Claude's #30337. Claude keeps
/// 64 bytes of headroom (macOsKeychainStorage.ts:24) and falls back to argv
/// beyond it; we do the same.
#[cfg(all(target_os = "macos", not(test)))]
const SECURITY_STDIN_LINE_LIMIT: usize = 4096 - 64;

/// The config dir Claude Code reads, honoring `CLAUDE_CONFIG_DIR`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    crate::home_dir().map(|h| crate::sessions::claude_config_root(&h))
}

/// Path of the plaintext credential file (the macOS fallback, and the ONLY
/// store on Linux / Windows). `plainTextStorage.getStoragePath()`.
pub(crate) fn credentials_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join(".credentials.json"))
}

/// The Keychain service name, mirroring `getMacOsKeychainStorageServiceName`
/// (macOsKeychainHelpers.ts:29):
///
/// ```text
/// `Claude Code${OAUTH_FILE_SUFFIX}${serviceSuffix}${dirHash}`
/// ```
///
/// `OAUTH_FILE_SUFFIX` is `''` for the production build (constants/oauth.ts:101
/// — the non-empty variants are staging/local builds behind internal env
/// vars), `serviceSuffix` is `-credentials` for the OAuth entry, and `dirHash`
/// is present ONLY when `CLAUDE_CONFIG_DIR` is set, as `-<sha256(dir)[..8]>`.
///
/// Do NOT hardcode `"Claude Code-credentials"`: with a custom config dir that
/// name matches no entry, and the lookup silently falls through to the file.
///
/// Known limitation: the official hash input is `.normalize('NFC')`-ed. We
/// hash the raw value, so a custom config dir holding non-ASCII characters in
/// a non-NFC form derives a different name. That degrades safely — the lookup
/// finds nothing and the file fallback takes over — and adding a Unicode
/// normalization dependency for that three-way edge case isn't worth it.
pub(crate) fn keychain_service_name() -> String {
    use sha2::{Digest, Sha256};
    // `isDefaultDir = !process.env.CLAUDE_CONFIG_DIR` — keyed on the env var,
    // not on whether the path equals the default.
    let custom = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|v| !v.is_empty());
    let Some(dir) = custom else {
        return "Claude Code-credentials".to_string();
    };
    let digest = Sha256::digest(dir.as_bytes());
    let hash: String = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(8)
        .collect();
    format!("Claude Code-credentials-{hash}")
}

/// The `-a` account argument. `getUsername()` (macOsKeychainHelpers.ts:43) is
/// `process.env.USER || userInfo().username`, with a constant last resort.
#[cfg(all(target_os = "macos", not(test)))]
pub(crate) fn keychain_account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "claude-code-user".to_string())
}

// ===================================================================
// Keychain primitives (macOS only)
// ===================================================================

/// True when the login keychain is locked, in which case any `security`
/// call would put up an unlock dialog and block. Callers use it to fail
/// fast with an actionable message instead of hanging on the timeout.
#[cfg(all(target_os = "macos", not(test)))]
pub(crate) fn keychain_locked() -> bool {
    let mut cmd = std::process::Command::new("security");
    cmd.arg("show-keychain-info");
    match output_with_timeout_stdin(cmd, None) {
        Some(out) => out.status.code() == Some(SEC_KEYCHAIN_LOCKED),
        // A timeout or spawn failure is not evidence of a lock.
        None => false,
    }
}

/// Keychain read cache — same 30s TTL as Claude Code's own
/// (`KEYCHAIN_CACHE_TTL_MS`, macOsKeychainHelpers.ts:69), and for the same
/// reason: every read spawns `security(1)` (~30-500ms), and some of OUR
/// callers sit on the tray's MAIN-thread rebuild path. 30s of staleness is
/// fine — tokens live hours, and every Termory write goes through
/// `keychain_write`/`keychain_delete`, which invalidate. An external writer
/// (a running claude refreshing tokens) is out of date at most 30s, the
/// bound the official CLI itself accepts cross-process.
#[cfg(all(target_os = "macos", not(test)))]
#[allow(clippy::type_complexity)]
static KEYCHAIN_CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<JsonValue>)>> =
    std::sync::Mutex::new(None);

#[cfg(all(target_os = "macos", not(test)))]
const KEYCHAIN_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(all(target_os = "macos", not(test)))]
fn invalidate_keychain_cache() {
    *KEYCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Read the Keychain entry (30s-cached). `None` = no entry (or an unreadable
/// one), which is what makes the caller fall through to the file.
#[cfg(all(target_os = "macos", not(test)))]
fn keychain_read() -> Option<JsonValue> {
    {
        let cache = KEYCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < KEYCHAIN_CACHE_TTL {
                return value.clone();
            }
        }
    }
    let value = keychain_read_uncached();
    *KEYCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((std::time::Instant::now(), value.clone()));
    value
}

#[cfg(all(target_os = "macos", not(test)))]
fn keychain_read_uncached() -> Option<JsonValue> {
    let mut cmd = std::process::Command::new("security");
    cmd.args([
        "find-generic-password",
        "-a",
        &keychain_account(),
        "-w",
        "-s",
        &keychain_service_name(),
    ]);
    let out = output_with_timeout_stdin(cmd, None)?;
    if !out.status.success() {
        return None; // exit 44 = no entry
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// Write the Keychain entry, replacing any existing one (`-U`).
///
/// The payload travels as hex (`-X`) so no escaping is needed, and the
/// command goes through `security -i` on **stdin** so a process monitor sees
/// only `security -i`, never the credential (Claude's INC-3028 note). Only
/// when the line would overflow the stdin buffer do we fall back to argv —
/// same trade-off Claude makes, since the alternative is a silent no-write
/// that leaves the old entry shadowing everything.
#[cfg(all(target_os = "macos", not(test)))]
fn keychain_write(doc: &JsonValue) -> bool {
    let ok = keychain_write_inner(doc);
    // Cache maintenance AFTER the write settles — invalidating BEFORE left a
    // sub-second window where a concurrent read (tray build on the main
    // thread) re-cached the OLD entry mid-write and the switch tail then
    // snapshotted stale credentials. On success the cache is primed with the
    // exact document written, so the very next read needs no spawn at all.
    let mut cache = KEYCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = if ok {
        Some((std::time::Instant::now(), Some(doc.clone())))
    } else {
        None
    };
    ok
}

#[cfg(all(target_os = "macos", not(test)))]
fn keychain_write_inner(doc: &JsonValue) -> bool {
    let json = doc.to_string();
    let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
    let account = keychain_account();
    let service = keychain_service_name();
    let command =
        format!("add-generic-password -U -a \"{account}\" -s \"{service}\" -X \"{hex}\"\n");

    if command.len() <= SECURITY_STDIN_LINE_LIMIT {
        let mut cmd = std::process::Command::new("security");
        cmd.arg("-i");
        return output_with_timeout_stdin(cmd, Some(command.as_bytes()))
            .map(|o| o.status.success())
            .unwrap_or(false);
    }
    let mut cmd = std::process::Command::new("security");
    cmd.args([
        "add-generic-password",
        "-U",
        "-a",
        &account,
        "-s",
        &service,
        "-X",
        &hex,
    ]);
    output_with_timeout_stdin(cmd, None)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Delete the Keychain entry. A missing entry counts as success (exit 44),
/// mirroring `plainTextStorage.delete()`'s ENOENT handling.
#[cfg(all(target_os = "macos", not(test)))]
fn keychain_delete() -> bool {
    let ok = keychain_delete_inner();
    invalidate_keychain_cache(); // after the delete settles — see keychain_write
    ok
}

#[cfg(all(target_os = "macos", not(test)))]
fn keychain_delete_inner() -> bool {
    let mut cmd = std::process::Command::new("security");
    cmd.args([
        "delete-generic-password",
        "-a",
        &keychain_account(),
        "-s",
        &keychain_service_name(),
    ]);
    match output_with_timeout_stdin(cmd, None) {
        Some(out) => out.status.success() || out.status.code() == Some(SEC_ITEM_NOT_FOUND),
        None => false,
    }
}

// ===================================================================
// File primitives (all platforms)
// ===================================================================

fn file_read() -> Option<JsonValue> {
    let path = credentials_file()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn file_write(doc: &JsonValue) -> Result<(), Box<dyn Error>> {
    let path = credentials_file().ok_or("home directory not available")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 0600, like `plainTextStorage.update`'s chmodSync.
    crate::accounts::atomic_write_0600(&path, doc.to_string().as_bytes())
}

fn file_delete() -> bool {
    let Some(path) = credentials_file() else {
        return false;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

// ===================================================================
// Public API — the SecureStorage equivalent
// ===================================================================

/// Read the live credential document, `{"claudeAiOauth": {...}}`.
///
/// Keychain first on macOS, then the file — the order `createFallbackStorage`
/// uses, so we see exactly what the CLI would. Served through the 30s cache;
/// fine for DISPLAY paths (tray, account list, quota), but every
/// snapshot-quality read must use [`read_credentials_uncached`] instead.
pub(crate) fn read_credentials() -> Option<JsonValue> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        if let Some(found) = keychain_read() {
            return Some(found);
        }
    }
    file_read()
}

/// [`read_credentials`] bypassing the 30s cache. REQUIRED at every point
/// whose result gets PERSISTED (account snapshots) or decides a write:
/// external writers — the `claude auth login` child, a running claude
/// rotating its refresh token — change the Keychain without any Termory
/// invalidation, and a ≤30s-stale answer at a snapshot point stores a dead
/// refresh token (or, on a first login, reads None and rolls the fresh
/// login back). Display paths keep the cached form; correctness wins here.
pub(crate) fn read_credentials_uncached() -> Option<JsonValue> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let value = keychain_read_uncached();
        // Re-prime so display readers benefit from the fresh answer too.
        *KEYCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now(), value.clone()));
        if let Some(found) = value {
            return Some(found);
        }
    }
    file_read()
}

/// Drop the Keychain read cache. Called when an EXTERNAL writer is known to
/// have changed the credential (the `claude auth login` child exiting) so
/// even display reads pick up the new state immediately. No-op off macOS /
/// in tests (no cache exists there).
pub(crate) fn invalidate_credentials_cache() {
    #[cfg(all(target_os = "macos", not(test)))]
    invalidate_keychain_cache();
}

/// Write the credential document back, mirroring `createFallbackStorage.update`
/// including its two cleanup rules — both exist to stop one tier from
/// shadowing a fresher value in the other:
///
///  * Keychain write succeeded and there was **no** prior Keychain entry ⇒
///    delete the file (the first-time migration case).
///  * Keychain write failed and there **was** a prior entry ⇒ delete it,
///    otherwise `read()` keeps preferring that stale entry over the file we
///    just wrote — the /login loop of #30337.
pub(crate) fn write_credentials(doc: &JsonValue) -> Result<(), Box<dyn Error>> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        if keychain_locked() {
            return Err(
                "macOS keychain is locked — unlock it (open Keychain Access) and try again".into(),
            );
        }
        // UNCACHED probe: the migrate/cleanup decisions below hinge on
        // whether an entry exists RIGHT NOW — a 30s-stale answer could
        // delete the file tier on a false "first write" or skip the
        // stale-entry cleanup. Writes are rare; correctness wins.
        let had_keychain_entry = keychain_read_uncached().is_some();
        if keychain_write(doc) {
            if !had_keychain_entry {
                let _ = file_delete();
            }
            return Ok(());
        }
        // Keychain write failed — fall back to the file, then make sure no
        // older Keychain entry can shadow it.
        file_write(doc)?;
        if had_keychain_entry && !keychain_delete() {
            return Err("wrote the credentials file but could not clear the stale keychain entry — Claude Code may keep using the previous account".into());
        }
        return Ok(());
    }
    #[cfg(any(not(target_os = "macos"), test))]
    {
        file_write(doc)
    }
}

/// Remove the credential from every tier (`createFallbackStorage.delete`).
/// Used by the add-account rollback when there was no prior login; returns
/// true when at least one tier reported success.
pub(crate) fn delete_credentials() -> bool {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let primary = keychain_delete();
        let secondary = file_delete();
        primary || secondary
    }
    #[cfg(any(not(target_os = "macos"), test))]
    {
        file_delete()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{lock_home, EnvVarGuard};
    use serde_json::json;

    #[test]
    fn service_name_is_bare_for_the_default_config_dir() {
        let _lock = lock_home();
        {
            let _unset = EnvVarGuard::unset("CLAUDE_CONFIG_DIR");
            assert_eq!(keychain_service_name(), "Claude Code-credentials");
        }
        {
            // Empty is treated as unset, matching `!process.env.CLAUDE_CONFIG_DIR`.
            let _empty = EnvVarGuard::set("CLAUDE_CONFIG_DIR", "");
            assert_eq!(keychain_service_name(), "Claude Code-credentials");
        }
    }

    /// The suffix is the first 8 hex chars of sha256(dir) — pinned against a
    /// value computed independently (`printf %s /tmp/cc | shasum -a 256`), so
    /// a refactor that changes the digest or the truncation is caught.
    #[test]
    fn service_name_appends_sha256_prefix_for_a_custom_config_dir() {
        let _lock = lock_home();
        let _cfg = EnvVarGuard::set("CLAUDE_CONFIG_DIR", "/tmp/cc");
        let name = keychain_service_name();
        assert!(
            name.starts_with("Claude Code-credentials-"),
            "unexpected shape: {name}"
        );
        let hash = name.trim_start_matches("Claude Code-credentials-");
        assert_eq!(hash.len(), 8, "hash segment must be 8 chars: {name}");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash segment must be hex: {name}"
        );
        // Independently computed: `printf %s /tmp/cc | shasum -a 256`.
        assert_eq!(name, "Claude Code-credentials-aa3d8c96");
    }

    #[test]
    fn credentials_file_follows_the_config_dir_override() {
        let _lock = lock_home();
        let _cfg = EnvVarGuard::set("CLAUDE_CONFIG_DIR", "/tmp/cc-probe");
        assert_eq!(
            credentials_file().unwrap(),
            PathBuf::from("/tmp/cc-probe/.credentials.json")
        );
    }

    /// The file tier round-trips through 0600 atomic writes. Uses a scratch
    /// CLAUDE_CONFIG_DIR so the real credential is never touched.
    #[test]
    fn file_tier_round_trips_and_delete_is_idempotent() {
        let _lock = lock_home();
        let dir = std::env::temp_dir().join(format!("termory-cauth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _cfg = EnvVarGuard::set("CLAUDE_CONFIG_DIR", &dir);

        assert!(file_read().is_none(), "no file yet");
        let doc = json!({ "claudeAiOauth": { "accessToken": "a-1" } });
        file_write(&doc).expect("write");
        assert_eq!(file_read().unwrap(), doc);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(credentials_file().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
        }

        assert!(file_delete(), "delete existing");
        assert!(file_delete(), "delete missing is still success");
        assert!(file_read().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
