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

fn app_dir() -> Result<PathBuf, Box<dyn Error>> {
    let home = dirs::home_dir().ok_or("home directory not available")?;
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

/// Atomically write `~/.termory/config.json` (chmod 0600 on Unix).
pub fn write_config(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    write_json_atomic_0600(&config_path()?, value)
}

// ===================================================================
// providers.json — Provider library (contains API keys)
// ===================================================================

/// Schema version stamped into providers.json. Bump when the on-disk
/// `Provider` shape changes in a breaking way so a future load can detect
/// the old format and migrate. The wrap/unwrap lives entirely here —
/// callers (frontend `config.ts`, `tray.rs`) only ever see the bare
/// providers array.
pub const PROVIDERS_SCHEMA_VERSION: u64 = 1;

/// Read the providers array from `~/.termory/providers.json`. Returns
/// `[]` if missing. The file is `{ "version": N, "providers": [...] }`;
/// this unwraps to the inner array (tolerating a bare-array file from
/// older dev builds / hand-edits).
pub fn read_providers() -> Result<JsonValue, Box<dyn Error>> {
    let raw = read_json(&providers_path()?, JsonValue::Array(Vec::new()))?;
    Ok(match raw {
        JsonValue::Object(mut map) => map
            .remove("providers")
            .unwrap_or(JsonValue::Array(Vec::new())),
        other => other, // bare array (legacy) or the empty default
    })
}

/// Atomically write `~/.termory/providers.json` (chmod 0600 on Unix).
/// `value` is the bare providers array; it's wrapped in the versioned
/// envelope here.
pub fn write_providers(value: &JsonValue) -> Result<(), Box<dyn Error>> {
    let mut envelope = serde_json::Map::new();
    envelope.insert("version".into(), JsonValue::from(PROVIDERS_SCHEMA_VERSION));
    envelope.insert("providers".into(), value.clone());
    write_json_atomic_0600(&providers_path()?, &JsonValue::Object(envelope))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::HOME_LOCK;

    struct HomeOverride {
        prev: Option<std::ffi::OsString>,
    }
    impl HomeOverride {
        fn new(p: &Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", p);
            HomeOverride { prev }
        }
    }
    impl Drop for HomeOverride {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("config-empty");
        let _h = HomeOverride::new(&tmp);
        let value = read_config().unwrap();
        assert!(value.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_missing_providers_returns_empty_array() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("providers-empty");
        let _h = HomeOverride::new(&tmp);
        let value = read_providers().unwrap();
        assert!(value.as_array().unwrap().is_empty());
    }

    #[test]
    fn config_and_providers_live_in_separate_files() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("two-files");
        let _h = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("perms");
        let _h = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("rt-providers");
        let _h = HomeOverride::new(&tmp);
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
    fn providers_file_carries_schema_version_and_unwraps_on_read() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("providers-versioned");
        let _h = HomeOverride::new(&tmp);
        write_providers(&serde_json::json!([{"id": "a", "name": "A"}])).unwrap();

        // On disk: a versioned envelope, not a bare array.
        let raw: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".termory/providers.json")).unwrap())
                .unwrap();
        assert_eq!(
            raw.pointer("/version").and_then(|v| v.as_u64()),
            Some(PROVIDERS_SCHEMA_VERSION)
        );
        assert!(raw.pointer("/providers").unwrap().is_array());

        // A legacy bare-array file still reads back as the array.
        fs::write(
            tmp.join(".termory/providers.json"),
            r#"[{"id":"legacy","name":"Old"}]"#,
        )
        .unwrap();
        let back = read_providers().unwrap();
        assert_eq!(back.as_array().unwrap().len(), 1);
        assert_eq!(
            back.pointer("/0/id").and_then(|v| v.as_str()),
            Some("legacy")
        );
    }

    #[test]
    fn read_missing_favorites_returns_empty_array() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("favorites-empty");
        let _h = HomeOverride::new(&tmp);
        let value = read_favorites().unwrap();
        assert!(value.as_array().unwrap().is_empty());
    }

    #[test]
    fn favorites_roundtrip_preserves_array_order() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("rt-favorites");
        let _h = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("favorites-perms");
        let _h = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("atomic");
        let _h = HomeOverride::new(&tmp);
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
}
