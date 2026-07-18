//! App config + provider-library + favorites KV stored under `~/.termory/`.
//!
//! Three files, separated by sensitivity / purpose:
//!   * `config.json`    — UI preferences (default_pane, recent_searches,
//!                       providers_app, …). No secrets.
//!   * `providers.json` — Provider library (contains API keys).
//!   * `favorites.json` — Saved message snapshots (may contain whatever
//!                       the user pasted into a CLI; treat as private).
//!
//! All three files write atomically (tmp + rename) and on Unix get
//! mode 0600 (parent dir 0700). Pattern matches Codex auth.json
//! (`login/src/auth/storage.rs:147`), OpenCode auth.json
//! (`packages/opencode/src/auth/index.ts:78,87`), and cc-switch's
//! settings store (`settings.rs:469-475`).

use serde_json::{Map, Value as JsonValue};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = ".termory";
const CONFIG_FILE_NAME: &str = "config.json";
const PROVIDERS_FILE_NAME: &str = "providers.json";
const FAVORITES_FILE_NAME: &str = "favorites.json";
const ACCOUNTS_FILE_NAME: &str = "accounts.json";

fn app_dir() -> Result<PathBuf, Box<dyn Error>> {
    let home = crate::home_dir().ok_or("home directory not available")?;
    Ok(home.join(APP_DIR_NAME))
}

fn config_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(app_dir()?.join(CONFIG_FILE_NAME))
}

fn providers_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(app_dir()?.join(PROVIDERS_FILE_NAME))
}

fn favorites_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(app_dir()?.join(FAVORITES_FILE_NAME))
}

fn accounts_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(app_dir()?.join(ACCOUNTS_FILE_NAME))
}

// ===================================================================
// Generic helpers
// ===================================================================

fn read_json(path: &Path, default: JsonValue) -> Result<JsonValue, Box<dyn Error>> {
    if !path.exists() {
        return Ok(default);
    }
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(default);
    }
    let parsed: JsonValue = serde_json::from_str(&text)?;
    Ok(parsed)
}

fn write_json_atomic_0600(path: &Path, value: &JsonValue) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(parent)?.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(parent, perms)?;
        }
    }

    let serialized = serde_json::to_string_pretty(value)?;

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
        f.write_all(serialized.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(serialized.as_bytes())?;
        f.sync_all()?;
    }

    fs::rename(&tmp_path, path)?;
    Ok(())
}

// ===================================================================
// config.json — UI preferences
// ===================================================================

/// Read `~/.termory/config.json`. Returns `{}` if missing.
pub fn read_config() -> Result<JsonValue, Box<dyn Error>> {
    read_json(&config_path()?, JsonValue::Object(Map::new()))
}

/// Tools OFF BY DEFAULT — hidden until the user flips their Settings →
/// Tools switch to an explicit `true`. Gemini CLI stopped serving
/// individual accounts on 2026-06-18 (HTTP 410; enterprise Code Assist
/// still works, Google steers individuals to Antigravity CLI), so it no
/// longer earns a default slot. MIRROR of `DEFAULT_OFF_SOURCES` in
/// src/lib/provider-utils.ts — keep in sync.
const DEFAULT_OFF_KEYS: &[&str] = &["gemini"];

/// CLI keys ("codex" / "claude" / …) the user has switched OFF in
/// Settings → Tools, read from config.json's `sources` map. An absent
/// key means ENABLED — except the `DEFAULT_OFF_KEYS` tools, which need
/// an explicit `true` to show.
pub fn disabled_sources() -> std::collections::HashSet<String> {
    let map = read_config()
        .ok()
        .and_then(|c| c.get("sources").cloned())
        .and_then(|v| match v {
            JsonValue::Object(map) => Some(map),
            _ => None,
        });
    disabled_sources_from(map.as_ref())
}

fn disabled_sources_from(
    map: Option<&Map<String, JsonValue>>,
) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> = map
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.as_bool() == Some(false))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    for key in DEFAULT_OFF_KEYS {
        let explicitly_on = map.and_then(|m| m.get(*key)).and_then(|v| v.as_bool()) == Some(true);
        if !explicitly_on {
            out.insert((*key).to_string());
        }
    }
    out
}

/// Atomically write `~/.termory/config.json` (chmod 0600 on Unix).
pub fn write_config(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    write_json_atomic_0600(&config_path()?, value)
}

// ===================================================================
// providers.json — Provider library (contains API keys)
// ===================================================================

/// Schema version stamped into providers.json. Bump when the on-disk shape
/// changes in a breaking way so a future load can detect the old format and
/// migrate it forward (see `migrate_entries`). v1 is the current baseline.
pub const PROVIDERS_SCHEMA_VERSION: u64 = 1;

/// Discriminator value for a gateway entry in the unified `providers` list.
const GATEWAY_KIND: &str = "gateway";

fn entry_is_gateway(v: &JsonValue) -> bool {
    v.get("kind").and_then(|k| k.as_str()) == Some(GATEWAY_KIND)
}

/// Read every entry in providers.json (`{ "version": N, "providers": [...] }`,
/// where `providers` is the UNIFIED list of per-CLI providers
/// `kind: "official"|"custom"` and gateways `kind: "gateway"`), running it
/// through `migrate_entries` so an older on-disk version is upgraded to the
/// current shape. A missing / empty / non-object file yields `[]`.
fn read_all_entries() -> Result<Vec<JsonValue>, Box<dyn Error>> {
    let raw = read_json(&providers_path()?, JsonValue::Object(Map::new()))?;
    let mut map = match raw {
        JsonValue::Object(map) => map,
        _ => return Ok(Vec::new()),
    };
    let version = map
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(PROVIDERS_SCHEMA_VERSION);
    let entries = match map.remove("providers") {
        Some(JsonValue::Array(a)) => a,
        _ => Vec::new(),
    };
    Ok(migrate_entries(version, entries))
}

/// Upgrade `providers` entries written by an older schema version to the
/// current shape. Add an arm when bumping `PROVIDERS_SCHEMA_VERSION`; v1 is
/// the baseline so there is nothing to migrate yet.
fn migrate_entries(_version: u64, entries: Vec<JsonValue>) -> Vec<JsonValue> {
    // e.g. `if _version < 2 { entries = entries.into_iter().map(...).collect() }`
    entries
}

/// Persist the unified `providers` array as `{ "version": N, "providers": [...] }`.
fn write_all_entries(entries: Vec<JsonValue>) -> Result<(), Box<dyn Error>> {
    let mut env = Map::new();
    env.insert("version".into(), JsonValue::from(PROVIDERS_SCHEMA_VERSION));
    env.insert("providers".into(), JsonValue::Array(entries));
    write_json_atomic_0600(&providers_path()?, &JsonValue::Object(env))
}

/// Read the per-CLI providers (everything `kind != "gateway"`).
pub fn read_providers() -> Result<JsonValue, Box<dyn Error>> {
    Ok(JsonValue::Array(
        read_all_entries()?
            .into_iter()
            .filter(|v| !entry_is_gateway(v))
            .collect(),
    ))
}

/// Write the per-CLI providers, **preserving** the gateway entries already
/// in the unified list (the two kinds share one array, discriminated by
/// `kind`, so each writer must keep the other kind intact).
pub fn write_providers(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    let mut next: Vec<JsonValue> = match value {
        JsonValue::Array(a) => a.clone(),
        _ => Vec::new(),
    };
    next.extend(read_all_entries()?.into_iter().filter(entry_is_gateway));
    write_all_entries(next)
}

/// Read the gateways (everything `kind == "gateway"`).
pub fn read_gateways() -> Result<JsonValue, Box<dyn Error>> {
    Ok(JsonValue::Array(
        read_all_entries()?
            .into_iter()
            .filter(entry_is_gateway)
            .collect(),
    ))
}

/// Write the gateways (each tagged `kind: "gateway"`), preserving the
/// per-CLI providers in the unified list.
pub fn write_gateways(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    let mut next: Vec<JsonValue> = read_all_entries()?
        .into_iter()
        .filter(|v| !entry_is_gateway(v))
        .collect();
    if let JsonValue::Array(arr) = value {
        for g in arr {
            let mut g = g.clone();
            if let JsonValue::Object(ref mut o) = g {
                o.insert("kind".into(), JsonValue::from(GATEWAY_KIND));
            }
            next.push(g);
        }
    }
    write_all_entries(next)
}

// ===================================================================
// favorites.json — saved message snapshots
// ===================================================================

/// Read `~/.termory/favorites.json`. Returns `[]` if missing.
pub fn read_favorites() -> Result<JsonValue, Box<dyn Error>> {
    read_json(&favorites_path()?, JsonValue::Array(Vec::new()))
}

/// Atomically write `~/.termory/favorites.json` (chmod 0600 on Unix).
/// Favorites can contain sensitive snippets the user pasted into a
/// CLI (API keys, prompts with PII, …) — hence the same 0600 mode as
/// `providers.json`.
pub fn write_favorites(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    write_json_atomic_0600(&favorites_path()?, value)
}

// ===================================================================
// accounts.json — saved official-login snapshots (contains OAuth tokens)
// ===================================================================

/// Read `~/.termory/accounts.json`. Returns `[]` if missing.
pub fn read_accounts() -> Result<JsonValue, Box<dyn Error>> {
    read_json(&accounts_path()?, JsonValue::Array(Vec::new()))
}

/// Atomically write `~/.termory/accounts.json` (chmod 0600 on Unix).
/// Each entry snapshots a CLI's official OAuth login (Codex `auth.json`,
/// …) so the user can switch between multiple accounts — it holds live
/// access/refresh tokens, hence the same 0600 mode as `providers.json`.
pub fn write_accounts(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    write_json_atomic_0600(&accounts_path()?, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{lock_home, override_home};

    fn tempdir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("termory-appconfig-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_missing_config_returns_empty_object() {
        let _g = lock_home();
        let tmp = tempdir("config-empty");
        let _h = override_home(&tmp);
        let value = read_config().unwrap();
        assert!(value.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_missing_providers_returns_empty_array() {
        let _g = lock_home();
        let tmp = tempdir("providers-empty");
        let _h = override_home(&tmp);
        let value = read_providers().unwrap();
        assert!(value.as_array().unwrap().is_empty());
    }

    #[test]
    fn config_and_providers_live_in_separate_files() {
        let _g = lock_home();
        let tmp = tempdir("two-files");
        let _h = override_home(&tmp);
        write_config(&serde_json::json!({"default_pane": "memory"})).unwrap();
        write_providers(&serde_json::json!([{"id": "p1", "name": "Test"}])).unwrap();

        let cfg_text = fs::read_to_string(tmp.join(".termory/config.json")).unwrap();
        let prov_text = fs::read_to_string(tmp.join(".termory/providers.json")).unwrap();
        // config.json must not contain provider data, and vice versa.
        assert!(cfg_text.contains("default_pane"));
        assert!(!cfg_text.contains("\"id\""));
        assert!(prov_text.contains("\"id\""));
        assert!(!prov_text.contains("default_pane"));
    }

    #[cfg(unix)]
    #[test]
    fn both_files_get_0600_and_dir_0700() {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_home();
        let tmp = tempdir("perms");
        let _h = override_home(&tmp);
        write_config(&serde_json::json!({"k": "v"})).unwrap();
        write_providers(&serde_json::json!([])).unwrap();
        let dir_mode = fs::metadata(tmp.join(".termory"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let cfg_mode = fs::metadata(tmp.join(".termory/config.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let prov_mode = fs::metadata(tmp.join(".termory/providers.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "~/.termory must be 0700");
        assert_eq!(cfg_mode, 0o600, "config.json must be 0600");
        assert_eq!(prov_mode, 0o600, "providers.json must be 0600");
    }

    #[test]
    fn providers_roundtrip_preserves_array_order() {
        let _g = lock_home();
        let tmp = tempdir("rt-providers");
        let _h = override_home(&tmp);
        let payload = serde_json::json!([
            {"id": "a", "name": "First"},
            {"id": "b", "name": "Second"},
            {"id": "c", "name": "Third"},
        ]);
        write_providers(&payload).unwrap();
        let back = read_providers().unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn providers_file_is_a_versioned_object_envelope() {
        let _g = lock_home();
        let tmp = tempdir("providers-versioned");
        let _h = override_home(&tmp);
        write_providers(&serde_json::json!([{"id": "a", "name": "A"}])).unwrap();

        // On disk: `{ "version": N, "providers": [...] }` — an object, never
        // a bare array.
        let raw: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".termory/providers.json")).unwrap())
                .unwrap();
        assert_eq!(
            raw.pointer("/version").and_then(|v| v.as_u64()),
            Some(PROVIDERS_SCHEMA_VERSION)
        );
        assert!(raw.pointer("/providers").unwrap().is_array());
        let back = read_providers().unwrap();
        assert_eq!(back.pointer("/0/id").and_then(|v| v.as_str()), Some("a"));
    }

    #[test]
    fn read_missing_gateways_returns_empty_array() {
        let _g = lock_home();
        let tmp = tempdir("gateways-empty");
        let _h = override_home(&tmp);
        assert!(read_gateways().unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn providers_and_gateways_coexist_without_clobbering() {
        let _g = lock_home();
        let tmp = tempdir("prov-gateways");
        let _h = override_home(&tmp);

        write_providers(&serde_json::json!([{"id": "p1", "app": "claude", "name": "P"}])).unwrap();
        write_gateways(&serde_json::json!([
            {"id": "r1", "name": "Gateway", "baseUrl": "https://x", "bindings": [{"app": "codex"}]}
        ]))
        .unwrap();

        // Both arrays survive in the one file.
        let providers = read_providers().unwrap();
        let gateways = read_gateways().unwrap();
        assert_eq!(
            providers.pointer("/0/id").and_then(|v| v.as_str()),
            Some("p1")
        );
        assert_eq!(
            gateways.pointer("/0/id").and_then(|v| v.as_str()),
            Some("r1")
        );
        assert_eq!(
            gateways
                .pointer("/0/bindings/0/app")
                .and_then(|v| v.as_str()),
            Some("codex")
        );

        // Rewriting providers must NOT drop gateways, and vice versa.
        write_providers(&serde_json::json!([{"id": "p2", "app": "gemini", "name": "P2"}])).unwrap();
        assert_eq!(
            read_gateways()
                .unwrap()
                .pointer("/0/id")
                .and_then(|v| v.as_str()),
            Some("r1")
        );
        write_gateways(&serde_json::json!([{"id": "r2", "name": "R2"}])).unwrap();
        assert_eq!(
            read_providers()
                .unwrap()
                .pointer("/0/id")
                .and_then(|v| v.as_str()),
            Some("p2")
        );

        // Single file on disk: ONE unified `providers` array, no separate
        // `gateways` key; the gateway entry is tagged `kind: "gateway"`.
        let raw: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".termory/providers.json")).unwrap())
                .unwrap();
        assert_eq!(
            raw.pointer("/version").and_then(|v| v.as_u64()),
            Some(PROVIDERS_SCHEMA_VERSION)
        );
        assert!(raw.pointer("/gateways").is_none());
        let entries = raw.pointer("/providers").unwrap().as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("gateway"))
                .count(),
            1
        );
    }

    #[test]
    fn read_missing_favorites_returns_empty_array() {
        let _g = lock_home();
        let tmp = tempdir("favorites-empty");
        let _h = override_home(&tmp);
        let value = read_favorites().unwrap();
        assert!(value.as_array().unwrap().is_empty());
    }

    #[test]
    fn favorites_roundtrip_preserves_array_order() {
        let _g = lock_home();
        let tmp = tempdir("rt-favorites");
        let _h = override_home(&tmp);
        let payload = serde_json::json!([
            {"id": "f-1", "message": {"role": "user", "text": "hi", "kind": "text"}},
            {"id": "f-2", "message": {"role": "assistant", "text": "hey", "kind": "text"}},
        ]);
        write_favorites(&payload).unwrap();
        let back = read_favorites().unwrap();
        assert_eq!(back, payload);
    }

    #[cfg(unix)]
    #[test]
    fn favorites_file_gets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_home();
        let tmp = tempdir("favorites-perms");
        let _h = override_home(&tmp);
        write_favorites(&serde_json::json!([])).unwrap();
        let mode = fs::metadata(tmp.join(".termory/favorites.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "favorites.json must be 0600");
    }

    #[test]
    fn config_overwrite_is_atomic_no_stray_tmp() {
        let _g = lock_home();
        let tmp = tempdir("atomic");
        let _h = override_home(&tmp);
        write_config(&serde_json::json!({"a": 1})).unwrap();
        write_providers(&serde_json::json!([])).unwrap();
        write_config(&serde_json::json!({"b": 2})).unwrap();
        let names: Vec<_> = fs::read_dir(tmp.join(".termory"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "no .tmp leftovers: {names:?}");
        assert!(names.contains(&"config.json".to_string()));
        assert!(names.contains(&"providers.json".to_string()));
    }

    #[test]
    fn disabled_sources_from_defaults_gemini_off_until_explicit_true() {
        use serde_json::json;
        let obj = |v: serde_json::Value| v.as_object().cloned().unwrap();

        // No config / no sources key → only the default-off tools.
        let d = disabled_sources_from(None);
        assert!(d.contains("gemini"));
        assert_eq!(d.len(), 1);

        // Explicit false stays disabled; unrelated keys unaffected.
        let m = obj(json!({ "codex": false }));
        let d = disabled_sources_from(Some(&m));
        assert!(d.contains("codex"));
        assert!(d.contains("gemini"));

        // Explicit true OVERRIDES the default-off.
        let m = obj(json!({ "gemini": true }));
        let d = disabled_sources_from(Some(&m));
        assert!(!d.contains("gemini"));

        // Explicit false for a default-off tool is still disabled.
        let m = obj(json!({ "gemini": false }));
        assert!(disabled_sources_from(Some(&m)).contains("gemini"));
    }
}
