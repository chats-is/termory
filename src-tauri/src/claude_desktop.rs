//! Claude **Desktop** third-party ("3P") provider management — DIRECT mode.
//!
//! Unlike Claude Code (which reads `~/.claude/settings.json` env vars),
//! Claude Desktop switches to a third-party endpoint via its own "3P"
//! mechanism (verified against cc-switch's `claude_desktop_config.rs`):
//!
//!   1. Flip `deploymentMode` to `"3p"` in BOTH config files:
//!        macOS:   ~/Library/Application Support/Claude/claude_desktop_config.json
//!                 ~/Library/Application Support/Claude-3p/claude_desktop_config.json
//!        Windows: the claude.ai installer ships an MSIX-packaged app whose
//!                 AppData is VIRTUALIZED — the real config dir is
//!                 %LOCALAPPDATA%\Packages\Claude_<hash>\LocalCache\Roaming\Claude
//!                 (raw %APPDATA% / %LOCALAPPDATA% remain as unpackaged
//!                 fallbacks; the installer pre-creates a raw
//!                 %LOCALAPPDATA%\Claude-3p, resolved independently)
//!   2. Write a **gateway profile** to
//!        <Claude-3p>/configLibrary/<PROFILE_ID>.json
//!      pointing at the provider's Anthropic-compatible base URL + bearer key.
//!   3. Register it in <Claude-3p>/configLibrary/_meta.json (`appliedId` +
//!      `entries`).
//!
//! Restoring "Official" flips `deploymentMode` back to `"1p"`, deletes the
//! profile, and clears the meta entry — Claude Desktop's own login is never
//! touched, so a round-trip is lossless.
//!
//! SCOPE: only cc-switch's **direct** mode is implemented (gateway → an
//! Anthropic-format upstream). cc-switch's **proxy** mode (routing
//! OpenAI/Gemini models through a local HTTP proxy with model-mapping +
//! OAuth) is intentionally out of scope — Termory has no proxy server.
//!
//! PLATFORM: macOS / Windows only. There is no Linux Claude Desktop, so
//! `is_supported()` is false there and `apply` / `restore_official` error.

use serde_json::{json, Map, Value as JsonValue};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use crate::providers::{
    atomic_write, infer_json_value, json_set_path, load_json_object, mask_secret,
    override_key_is_managed, string_match, write_json_object, ActiveKind, ActiveState, CliApp,
    LiveSnapshot, Provider, ProviderKind,
};

/// 1M-context marker — Claude Desktop natively parses a `[1m]` suffix on a
/// model name and sets `supports1m: true` (verified in the app's own
/// bundle: `name.match(/^(.+?)\[1m\]$/i)`). Termory's editor uses the same
/// `[1m]` convention as Claude Code, so a model id like
/// `claude-sonnet-4-6[1m]` round-trips through the picker unchanged.
const ONE_M_MARKER: &str = "[1m]";

/// Stable profile id Termory writes into Claude Desktop's config library
/// (the profile filename is `<PROFILE_ID>.json`). UUID-shaped, distinct
/// from cc-switch's id so the two tools never collide on the same entry.
const PROFILE_ID: &str = "7465726d-6f72-4079-8000-000000000001";
const PROFILE_NAME: &str = "Termory";
const CONFIG_FILE: &str = "claude_desktop_config.json";
const CONFIG_LIBRARY_DIR: &str = "configLibrary";

/// The five files an apply / restore touches, snapshotted for rollback.
#[derive(Debug, Clone)]
struct Paths {
    normal_config: PathBuf,
    threep_config: PathBuf,
    profile: PathBuf,
    meta: PathBuf,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

/// macOS / Windows only — there is no Linux Claude Desktop.
pub fn is_supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// Whether Claude Desktop looks installed: supported platform AND its
/// config dir exists (Claude Desktop creates `…/Claude/` on first run).
pub fn is_installed() -> bool {
    if !is_supported() {
        return false;
    }
    match current_paths() {
        // The normal config dir (parent of claude_desktop_config.json) is
        // the durable marker — present whether or not 3P was ever used.
        Ok(paths) => paths
            .normal_config
            .parent()
            .map(Path::is_dir)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// The installed Claude Desktop app version (it's a GUI app with no
/// `--version` CLI). macOS: `CFBundleShortVersionString` from the app
/// bundle's `Info.plist` (plain XML, scanned without a plist crate).
/// Returns None when not found / on platforms we don't read it from.
pub fn version() -> Option<String> {
    // macOS: prefer the authoritative current version from the app bundle's
    // Info.plist.
    #[cfg(target_os = "macos")]
    {
        for bundle in macos_app_bundles() {
            let plist = bundle.join("Contents").join("Info.plist");
            if let Ok(xml) = fs::read_to_string(&plist) {
                if let Some(v) = plist_short_version(&xml) {
                    return Some(v);
                }
            }
        }
    }
    // Cross-platform fallback (and the primary source on Windows): the
    // updater's last-seen version cached in `<config-dir>/config.json`.
    // `<config-dir>` is the same per-OS path `current_paths` already
    // resolves, so this works on macOS and Windows without guessing the
    // Windows app-install layout.
    config_json_version()
}

fn config_json_version() -> Option<String> {
    let paths = current_paths().ok()?;
    let cfg = paths.normal_config.parent()?.join("config.json");
    parse_config_version(&fs::read_to_string(cfg).ok()?)
}

/// Pull `updaterLastSeenVersion` out of Claude Desktop's `config.json`.
fn parse_config_version(json_text: &str) -> Option<String> {
    serde_json::from_str::<JsonValue>(json_text)
        .ok()?
        .get("updaterLastSeenVersion")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(target_os = "macos")]
fn macos_app_bundles() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/Applications/Claude.app")];
    if let Ok(home) = home() {
        out.push(home.join("Applications").join("Claude.app"));
    }
    out
}

/// Pull `CFBundleShortVersionString`'s value out of an XML Info.plist —
/// the `<string>` immediately following its `<key>`.
#[cfg(target_os = "macos")]
fn plist_short_version(xml: &str) -> Option<String> {
    const KEY: &str = "<key>CFBundleShortVersionString</key>";
    let rest = &xml[xml.find(KEY)? + KEY.len()..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")?;
    let v = rest[start..start + end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Activate a Custom Claude Desktop provider (direct mode).
pub fn apply(provider: &Provider) -> Result<(), Box<dyn Error>> {
    let base_url = provider.base_url.trim();
    // The API key may be blank (filled in later) — Claude Desktop just gets an
    // empty bearer token until it's set, like the other providers.
    if base_url.is_empty() {
        return Err("Claude Desktop provider is missing a Base URL".into());
    }
    let paths = current_paths()?;
    let profile = build_profile(provider);
    with_rollback(&paths, |paths| apply_to_paths(paths, &profile))
}

/// Restore Claude Desktop to its official (first-party) login.
pub fn restore_official() -> Result<(), Box<dyn Error>> {
    let paths = current_paths()?;
    with_rollback(&paths, restore_at_paths)
}

/// Reverse-derive the active Claude Desktop state from the live profile.
pub fn read_active(providers: &[Provider]) -> Result<ActiveState, Box<dyn Error>> {
    if !is_supported() {
        return Ok(official_state(String::new()));
    }
    let paths = current_paths()?;
    let live_path = paths.profile.display().to_string();

    // Lenient on the READ path: a corrupt profile (our own file) reads as
    // "no profile" → Official, rather than failing the whole active-states
    // call. The WRITE paths use the strict `read_json_or_empty` instead.
    let profile = read_json_or_empty(&paths.profile).unwrap_or_default();
    let base_url = profile
        .get("inferenceGatewayBaseUrl")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let api_key = profile
        .get("inferenceGatewayApiKey")
        .and_then(JsonValue::as_str)
        .map(str::to_string);

    // No gateway profile → official.
    if base_url.is_none() && api_key.is_none() {
        return Ok(official_state(live_path));
    }

    let snapshot = LiveSnapshot {
        base_url: base_url.clone(),
        api_key_masked: api_key.as_deref().map(mask_secret),
        model: None,
    };
    let matched = providers.iter().find(|p| {
        p.app == CliApp::ClaudeDesktop
            && p.kind == ProviderKind::Custom
            && string_match(&p.base_url, base_url.as_deref())
            && string_match(&p.api_key, api_key.as_deref())
    });

    Ok(ActiveState {
        app: CliApp::ClaudeDesktop,
        kind: if matched.is_some() {
            ActiveKind::Custom
        } else {
            ActiveKind::Unmanaged
        },
        matched_provider_id: matched.map(|p| p.id.clone()),
        live_snapshot: Some(snapshot),
        live_path,
        configured_provider_ids: Vec::new(),
    })
}

fn official_state(live_path: String) -> ActiveState {
    ActiveState {
        app: CliApp::ClaudeDesktop,
        kind: ActiveKind::Official,
        matched_provider_id: None,
        live_snapshot: None,
        live_path,
        configured_provider_ids: Vec::new(),
    }
}

// -------------------------------------------------------------------
// Path-injected core (tested directly, platform-independent)
// -------------------------------------------------------------------

fn paths_from_dirs(normal_dir: &Path, threep_dir: &Path) -> Paths {
    let config_library = threep_dir.join(CONFIG_LIBRARY_DIR);
    Paths {
        normal_config: normal_dir.join(CONFIG_FILE),
        threep_config: threep_dir.join(CONFIG_FILE),
        profile: config_library.join(format!("{PROFILE_ID}.json")),
        meta: config_library.join("_meta.json"),
    }
}

/// Translate Termory's flat `Provider` into the 3P gateway profile JSON —
/// the same "dedicated fields + generic `options` escape hatch" model every
/// other CLI uses. `baseUrl`/`apiKey` → the gateway keys; `model`+`models`
/// → `inferenceModels`; the provider's `options` merge into ANY other
/// `inference*` profile key (managed keys are protected, like the CLIs).
fn build_profile(provider: &Provider) -> Map<String, JsonValue> {
    let mut profile = json!({
        "coworkEgressAllowedHosts": ["*"],
        "disableDeploymentModeChooser": true,
        "inferenceGatewayApiKey": provider.api_key.trim(),
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceGatewayBaseUrl": provider.base_url.trim(),
        "inferenceProvider": "gateway",
    })
    .as_object()
    .cloned()
    .expect("profile literal is an object");

    let models = inference_models(provider);
    if !models.is_empty() {
        profile.insert("inferenceModels".to_string(), JsonValue::Array(models));
    }

    // Generic escape hatch: merge any other (non-managed) profile key the
    // user set in Advanced settings into the profile JSON. A managed key is
    // skipped — checking BOTH the exact key AND its dot-path ROOT segment, so
    // a nested key like `inferenceGatewayBaseUrl.x` can't sneak past the
    // exact-match guard and clobber the managed scalar/array via json_set_path.
    for o in &provider.options {
        let key = o.key.trim();
        let root = key.split('.').next().unwrap_or(key).trim();
        if key.is_empty()
            || override_key_is_managed(CliApp::ClaudeDesktop, key)
            || override_key_is_managed(CliApp::ClaudeDesktop, root)
        {
            continue;
        }
        json_set_path(&mut profile, key, infer_json_value(&o.value));
    }

    profile
}

/// Build the `inferenceModels` array from the provider's `models` list.
/// Claude Desktop has NO single primary model (the editor hides that field),
/// so the top-level `provider.model` is intentionally ignored here — models
/// come only from the list. Each entry is a plain string id, or an object
/// `{ name, labelOverride?, supports1m? }` when it carries a display label or
/// a trailing `[1m]` (1M context) marker. Deduped by id.
fn inference_models(provider: &Provider) -> Vec<JsonValue> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for m in &provider.models {
        entries.push((m.id.clone(), m.name.clone()));
    }

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (raw_id, label) in entries {
        let (id, supports_1m) = strip_one_m(&raw_id);
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let label = label.trim();
        if supports_1m || !label.is_empty() {
            let mut item = json!({ "name": id });
            if !label.is_empty() {
                item["labelOverride"] = json!(label);
            }
            if supports_1m {
                item["supports1m"] = json!(true);
            }
            out.push(item);
        } else {
            out.push(JsonValue::String(id));
        }
    }
    out
}

/// Strip a trailing `[1m]` (case-insensitive) marker from a model id,
/// returning the bare id and whether the marker was present.
fn strip_one_m(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    let marker = ONE_M_MARKER.as_bytes();
    if bytes.len() >= marker.len()
        && bytes[bytes.len() - marker.len()..].eq_ignore_ascii_case(marker)
    {
        (
            trimmed[..trimmed.len() - marker.len()]
                .trim_end()
                .to_string(),
            true,
        )
    } else {
        (trimmed.to_string(), false)
    }
}

fn apply_to_paths(paths: &Paths, profile: &Map<String, JsonValue>) -> Result<(), Box<dyn Error>> {
    write_deployment_mode(&paths.normal_config, "3p")?;
    write_deployment_mode(&paths.threep_config, "3p")?;
    write_json_object(&paths.profile, profile)?;
    write_meta(&paths.meta, Some(PROFILE_ID))?;
    Ok(())
}

fn restore_at_paths(paths: &Paths) -> Result<(), Box<dyn Error>> {
    write_deployment_mode(&paths.normal_config, "1p")?;
    write_deployment_mode(&paths.threep_config, "1p")?;
    remove_enterprise_gateway_keys(&paths.threep_config)?;
    if paths.profile.exists() {
        fs::remove_file(&paths.profile)?;
    }
    write_meta(&paths.meta, None)?;
    Ok(())
}

/// Read a JSON object from `path`, returning an empty map when the file
/// is absent / empty / not an object (so a hand-mangled config never
/// hard-errors the read path).
fn read_json_or_empty(path: &Path) -> Result<Map<String, JsonValue>, Box<dyn Error>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    // Propagate a parse error (corrupt / non-object config) instead of
    // swallowing it — the write paths (`write_deployment_mode` / `write_meta`
    // / `remove_enterprise_gateway_keys`) must abort so `with_rollback`
    // restores the snapshot, never overwriting a user's config with a
    // truncated `{ "deploymentMode": "3p" }`. (An empty / absent file still
    // yields an empty map via `load_json_object`.)
    load_json_object(path)
}

/// Merge `deploymentMode = mode` into a config file, preserving every
/// other key the user / Claude Desktop wrote.
fn write_deployment_mode(path: &Path, mode: &str) -> Result<(), Box<dyn Error>> {
    let mut root = read_json_or_empty(path)?;
    root.insert(
        "deploymentMode".to_string(),
        JsonValue::String(mode.to_string()),
    );
    write_json_object(path, &root)
}

/// Upsert / clear Termory's entry in `_meta.json`. `Some(id)` registers
/// the profile and points `appliedId` at it; `None` removes our entry and
/// clears `appliedId` if it pointed here (falling back to another entry).
fn write_meta(path: &Path, applied_profile_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut root = read_json_or_empty(path)?;
    let mut entries = root
        .get("entries")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    entries.retain(|e| e.get("id").and_then(JsonValue::as_str) != Some(PROFILE_ID));

    match applied_profile_id {
        Some(id) => {
            entries.push(json!({ "id": PROFILE_ID, "name": PROFILE_NAME }));
            root.insert("appliedId".to_string(), JsonValue::String(id.to_string()));
        }
        None => {
            let was_ours = root
                .get("appliedId")
                .and_then(JsonValue::as_str)
                .is_some_and(|id| id == PROFILE_ID);
            if was_ours {
                match entries
                    .iter()
                    .find_map(|e| e.get("id").and_then(JsonValue::as_str))
                {
                    Some(next) => {
                        root.insert("appliedId".to_string(), JsonValue::String(next.to_string()));
                    }
                    None => {
                        root.remove("appliedId");
                    }
                }
            }
        }
    }

    root.insert("entries".to_string(), JsonValue::Array(entries));
    write_json_object(path, &root)
}

/// Strip the gateway keys from an `enterpriseConfig` block, in case a
/// previous 3P apply (Termory's or another tool's) left them there.
/// Mirrors cc-switch's `remove_cc_switch_enterprise_config`.
fn remove_enterprise_gateway_keys(path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json_or_empty(path)?;
    let Some(enterprise) = root
        .get_mut("enterpriseConfig")
        .and_then(JsonValue::as_object_mut)
    else {
        return Ok(());
    };
    for key in [
        "disableDeploymentModeChooser",
        "inferenceGatewayApiKey",
        "inferenceGatewayAuthScheme",
        "inferenceGatewayBaseUrl",
        "inferenceProvider",
    ] {
        enterprise.remove(key);
    }
    if enterprise.is_empty() {
        root.remove("enterpriseConfig");
    }
    write_json_object(path, &root)
}

// -------------------------------------------------------------------
// Snapshot / rollback
// -------------------------------------------------------------------

fn with_rollback<F>(paths: &Paths, op: F) -> Result<(), Box<dyn Error>>
where
    F: FnOnce(&Paths) -> Result<(), Box<dyn Error>>,
{
    let snapshots = snapshot_files(paths)?;
    match op(paths) {
        Ok(()) => Ok(()),
        Err(err) => match restore_snapshots(&snapshots) {
            Ok(()) => Err(err),
            Err(rollback_err) => Err(format!("{err}; rollback also failed: {rollback_err}").into()),
        },
    }
}

fn snapshot_files(paths: &Paths) -> Result<Vec<FileSnapshot>, Box<dyn Error>> {
    [
        &paths.normal_config,
        &paths.threep_config,
        &paths.profile,
        &paths.meta,
    ]
    .into_iter()
    .map(|path| {
        let content = if path.exists() {
            Some(fs::read(path)?)
        } else {
            None
        };
        Ok(FileSnapshot {
            path: path.clone(),
            content,
        })
    })
    .collect()
}

fn restore_snapshots(snapshots: &[FileSnapshot]) -> Result<(), Box<dyn Error>> {
    for snap in snapshots {
        match &snap.content {
            Some(content) => atomic_write(&snap.path, content)?,
            None => {
                if snap.path.exists() {
                    fs::remove_file(&snap.path)?;
                }
            }
        }
    }
    Ok(())
}

// -------------------------------------------------------------------
// Platform paths
// -------------------------------------------------------------------

fn current_paths() -> Result<Paths, Box<dyn Error>> {
    #[cfg(target_os = "macos")]
    {
        let root = config_parent_dir()?;
        Ok(paths_from_dirs(
            &root.join("Claude"),
            &root.join("Claude-3p"),
        ))
    }
    #[cfg(windows)]
    {
        let candidates = windows_config_parents();
        let parent = select_config_parent(&candidates);
        // Resolve the real install folders — the Windows dir isn't always
        // exactly `Claude` / `Claude-3p` (cc-switch fuzzy-matches `Claude*`),
        // so pick the actual folder when the exact name is absent. The 3p
        // dir is resolved INDEPENDENTLY: a fresh Windows install
        // pre-creates `%LOCALAPPDATA%\Claude-3p` while the normal config
        // dir may land under Roaming, so prefer wherever a `*-3p` dir
        // already exists (see `select_threep_dir`).
        Ok(paths_from_dirs(
            &pick_windows_claude_dir(&parent, false),
            &select_threep_dir(&candidates, &parent),
        ))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err("Claude Desktop 3P configuration is only supported on macOS and Windows".into())
    }
}

/// The per-OS parent dir Claude Desktop's config dir(s) live directly
/// under (`Claude` / `Claude-3p`). THE single source for that root —
/// `current_paths` resolves the config dirs from it and
/// `install_watch_parents` hands it to the filesystem watcher, so the
/// dir the watcher watches is by construction the dir `is_installed`
/// checks.
#[cfg(target_os = "macos")]
fn config_parent_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join("Library").join("Application Support"))
}

/// Ordered Windows candidates for the parent dir holding Claude
/// Desktop's config dir(s). Claude Desktop is an Electron app — its
/// userData, and so `claude_desktop_config.json`, lives under ROAMING
/// AppData (`%APPDATA%\Claude`, per the official MCP docs at
/// modelcontextprotocol.io/quickstart/user). `%LOCALAPPDATA%` is kept
/// as a FALLBACK only because cc-switch resolves there; by itself it
/// holds the app BINARY dir (`AnthropicClaude`, which doesn't match
/// the `Claude*` config rule) — looking only there is why Windows
/// installs were never detected.
#[cfg(windows)]
fn windows_config_parents() -> Vec<PathBuf> {
    let fallback = |sub: &str| home().unwrap_or_default().join("AppData").join(sub);
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback("Roaming"));
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback("Local"));
    // The claude.ai Windows installer ships an MSIX-PACKAGED app, whose
    // AppData is VIRTUALIZED: the app writes %APPDATA%\Claude but the
    // real on-disk location (the one an unpackaged app like Termory can
    // see) is Packages\Claude_<publisherhash>\LocalCache\Roaming\Claude
    // (verified on real hardware). Those parents come FIRST — when a
    // package exists it IS the install; the raw Roaming/Local parents
    // remain for unpackaged installs.
    let mut parents = msix_package_roaming_parents(&local);
    parents.push(roaming);
    parents.push(local);
    parents
}

/// `<local>\Packages\Claude_*\LocalCache\Roaming` for every installed
/// Claude MSIX package (the publisher-hash suffix varies per signing
/// identity, so scan by the `Claude_` prefix instead of hardcoding).
/// Pure / path-injected — unit-tested off-Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn msix_package_roaming_parents(local_app_data: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(local_app_data.join("Packages"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            e.path().is_dir()
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("Claude_"))
        })
        .map(|e| e.path().join("LocalCache").join("Roaming"))
        .collect();
    out.sort();
    out
}

#[cfg(windows)]
fn config_parent_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(select_config_parent(&windows_config_parents()))
}

/// First candidate that already CONTAINS a Claude Desktop config dir
/// (a `Claude*` non-3p child); else the first candidate, so fresh
/// writes target the official Roaming location. Pure / path-injected —
/// unit-tested off-Windows like the rest of this module.
#[cfg_attr(not(windows), allow(dead_code))]
fn select_config_parent(candidates: &[PathBuf]) -> PathBuf {
    candidates
        .iter()
        .find(|parent| parent_has_claude_dir(parent))
        .cloned()
        .unwrap_or_else(|| candidates.first().cloned().unwrap_or_default())
}

/// The `Claude-3p` dir, resolved independently of the normal config
/// dir: the first candidate parent where a `*-3p` dir already EXISTS
/// wins (a fresh Windows install pre-creates `%LOCALAPPDATA%\Claude-3p`
/// even before the normal dir appears), else `<normal parent>\Claude-3p`
/// so both dirs stay siblings for fresh writes.
#[cfg_attr(not(windows), allow(dead_code))]
fn select_threep_dir(candidates: &[PathBuf], normal_parent: &Path) -> PathBuf {
    candidates
        .iter()
        .map(|c| pick_windows_claude_dir(c, true))
        .find(|p| p.exists())
        .unwrap_or_else(|| pick_windows_claude_dir(normal_parent, true))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parent_has_claude_dir(parent: &Path) -> bool {
    std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|e| {
            e.path().is_dir()
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| windows_claude_dir_matches(n, false))
        })
}

#[cfg(any(target_os = "macos", windows))]
fn home() -> Result<PathBuf, Box<dyn Error>> {
    crate::home_dir().ok_or_else(|| "home directory not available".into())
}

/// Parent dir(s) whose direct `Claude*` child marks a Claude Desktop
/// install (`is_installed` checks the config dir's existence, and the
/// app creates it on first run). The filesystem watcher watches these
/// non-recursively so an install/uninstall — the config dir appearing
/// or disappearing — triggers install re-detection; the dir itself
/// can't be watched before it exists. Empty on unsupported platforms.
pub(crate) fn install_watch_parents() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        config_parent_dir().map(|p| vec![p]).unwrap_or_default()
    }
    // Watch every Windows candidate parent (package LocalCache\Roaming
    // dirs + raw Roaming + raw Local), PLUS the Packages dir itself — a
    // `Claude_*` package dir appearing there is the MSIX install event
    // (its LocalCache\Roaming can't be watched before the package
    // exists; a first-run that happens later self-heals via the tray's
    // menu-open / Recheck probes).
    #[cfg(windows)]
    {
        let mut parents = windows_config_parents();
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            parents.push(local.join("Packages"));
        }
        parents
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Vec::new()
    }
}

/// Whether a direct-child NAME under `install_watch_parents()` is a
/// Claude Desktop config dir (`Claude`, `Claude-3p`, versioned
/// `Claude*` variants). Owned here so the watcher's install routing
/// (`event_touches_claude_desktop`) and the Windows dir resolution
/// (`windows_claude_dir_matches`) share ONE rule and can't drift —
/// same ownership pattern as `quota::credential_cli_for_path`.
pub(crate) fn is_install_marker_name(name: &str) -> bool {
    name.starts_with("Claude")
}

/// Whether a `%LOCALAPPDATA%` child folder name is Claude Desktop's config
/// dir for the requested deployment (`threep` = the `-3p` variant). The
/// `-3p` substring distinguishes the two (so `Claude-3p` is 3P, `Claude`
/// — or a versioned `Claude*` variant — is normal). Mirror of cc-switch's
/// `pick_windows_claude_dir` filter. Pure (so it's unit-tested off-Windows).
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_claude_dir_matches(name: &str, threep: bool) -> bool {
    is_install_marker_name(name) && name.contains("-3p") == threep
}

/// Resolve Claude Desktop's Windows config dir for `threep`: the exact
/// `Claude` / `Claude-3p` folder when present, else the first (sorted)
/// `Claude*` folder matching the `-3p` flag — the install folder isn't
/// always the exact name. Falls back to the exact name (even if absent) so
/// the caller always has a deterministic path. The fuzzy scan runs ONLY
/// when the exact folder is missing (a single shallow `read_dir`).
#[cfg_attr(not(windows), allow(dead_code))]
fn pick_windows_claude_dir(local_app_data: &Path, threep: bool) -> PathBuf {
    let exact = local_app_data.join(if threep { "Claude-3p" } else { "Claude" });
    if exact.exists() {
        return exact;
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(local_app_data)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| windows_claude_dir_matches(n, threep))
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next().unwrap_or(exact)
}

// -------------------------------------------------------------------
// Tests (path-injected — platform-independent)
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("termory-cd-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_paths(root: &Path) -> Paths {
        paths_from_dirs(&root.join("Claude"), &root.join("Claude-3p"))
    }

    #[test]
    fn select_config_parent_picks_the_parent_holding_a_claude_dir() {
        let base = tempdir("parent-select");
        let roaming = base.join("Roaming");
        let local = base.join("Local");
        fs::create_dir_all(&roaming).unwrap();
        fs::create_dir_all(&local).unwrap();

        // Neither has a Claude dir → first candidate (Roaming, the
        // official location) so fresh writes land there.
        assert_eq!(
            select_config_parent(&[roaming.clone(), local.clone()]),
            roaming
        );

        // The app BINARY dir (`AnthropicClaude`) must NOT count as a
        // config dir — it fails the `Claude*` prefix rule.
        fs::create_dir_all(local.join("AnthropicClaude")).unwrap();
        assert_eq!(
            select_config_parent(&[roaming.clone(), local.clone()]),
            roaming
        );

        // Only Local has a real Claude config dir → Local wins even
        // though Roaming is listed first (cc-switch-style installs).
        fs::create_dir_all(local.join("Claude")).unwrap();
        assert_eq!(
            select_config_parent(&[roaming.clone(), local.clone()]),
            local
        );

        // Both have one → the official Roaming location wins.
        fs::create_dir_all(roaming.join("Claude")).unwrap();
        assert_eq!(
            select_config_parent(&[roaming.clone(), local.clone()]),
            roaming
        );
    }

    #[test]
    fn msix_package_roaming_parents_finds_claude_packages() {
        let local = tempdir("msix-scan");
        // No Packages dir at all → empty.
        assert!(msix_package_roaming_parents(&local).is_empty());
        // Real-world shape: Packages\Claude_<publisherhash> (hash varies
        // per signing identity, so the scan matches the prefix).
        fs::create_dir_all(local.join("Packages").join("Claude_pzs8sxrjxfjjc")).unwrap();
        fs::create_dir_all(local.join("Packages").join("Microsoft.Edge_8wekyb")).unwrap();
        let got = msix_package_roaming_parents(&local);
        assert_eq!(
            got,
            vec![local
                .join("Packages")
                .join("Claude_pzs8sxrjxfjjc")
                .join("LocalCache")
                .join("Roaming")]
        );
    }

    #[test]
    fn select_threep_dir_prefers_an_existing_3p_dir_across_parents() {
        let base = tempdir("threep-select");
        let roaming = base.join("Roaming");
        let local = base.join("Local");
        fs::create_dir_all(&roaming).unwrap();
        fs::create_dir_all(&local).unwrap();
        let candidates = vec![roaming.clone(), local.clone()];

        // Nothing exists yet → sibling of the normal parent (fresh writes).
        assert_eq!(
            select_threep_dir(&candidates, &roaming),
            roaming.join("Claude-3p")
        );

        // The app pre-created %LOCALAPPDATA%\Claude-3p (observed on a
        // fresh Windows install) → it wins even when the normal config
        // parent is Roaming (split layout).
        fs::create_dir_all(local.join("Claude-3p")).unwrap();
        assert_eq!(
            select_threep_dir(&candidates, &roaming),
            local.join("Claude-3p")
        );

        // A Roaming 3p dir (all-Roaming layout) takes precedence by
        // candidate order.
        fs::create_dir_all(roaming.join("Claude-3p")).unwrap();
        assert_eq!(
            select_threep_dir(&candidates, &roaming),
            roaming.join("Claude-3p")
        );
    }

    fn provider(id: &str, base: &str, key: &str) -> Provider {
        Provider {
            id: id.into(),
            app: CliApp::ClaudeDesktop,
            kind: ProviderKind::Custom,
            name: id.into(),
            base_url: base.into(),
            api_key: key.into(),
            model: String::new(),
            npm: None,
            models: Vec::new(),
            options: Vec::new(),
            favicon: None,
        }
    }

    fn read(path: &Path) -> JsonValue {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_short_version_extracts_value() {
        let xml = r#"<?xml version="1.0"?><plist><dict>
            <key>CFBundleName</key><string>Claude</string>
            <key>CFBundleShortVersionString</key><string>1.15962.0</string>
            <key>CFBundleVersion</key><string>9.9.9</string>
        </dict></plist>"#;
        assert_eq!(plist_short_version(xml), Some("1.15962.0".to_string()));
        assert_eq!(plist_short_version("<plist></plist>"), None);
    }

    #[test]
    fn windows_claude_dir_matches_distinguishes_3p_and_variants() {
        assert!(windows_claude_dir_matches("Claude", false));
        assert!(!windows_claude_dir_matches("Claude", true));
        assert!(windows_claude_dir_matches("Claude-3p", true));
        assert!(!windows_claude_dir_matches("Claude-3p", false));
        // Versioned / variant install folder names still match.
        assert!(windows_claude_dir_matches("Claude-1.15962.0", false));
        assert!(windows_claude_dir_matches("Claude-3p-1.15962.0", true));
        // Non-Claude folders never match.
        assert!(!windows_claude_dir_matches("NotClaude", false));
        assert!(!windows_claude_dir_matches("Cursor", false));
    }

    #[test]
    fn parse_config_version_reads_updater_last_seen() {
        let json = r#"{"locale":"en-US","updaterLastSeenVersion":"1.15962.0","x":1}"#;
        assert_eq!(parse_config_version(json), Some("1.15962.0".to_string()));
        assert_eq!(parse_config_version("{}"), None);
        assert_eq!(parse_config_version("not json"), None);
        assert_eq!(
            parse_config_version(r#"{"updaterLastSeenVersion":""}"#),
            None
        );
    }

    #[test]
    fn apply_writes_3p_profile_and_meta() {
        let root = tempdir("apply");
        let paths = test_paths(&root);
        // Pre-existing unrelated key must survive the deploymentMode merge.
        fs::create_dir_all(root.join("Claude")).unwrap();
        fs::write(
            &paths.normal_config,
            r#"{"locale":"en","deploymentMode":"1p"}"#,
        )
        .unwrap();

        apply_to_paths(
            &paths,
            &build_profile(&provider("p", "https://gw.example.com", "sk-secret-123456")),
        )
        .unwrap();

        let normal = read(&paths.normal_config);
        let threep = read(&paths.threep_config);
        let profile = read(&paths.profile);
        let meta = read(&paths.meta);

        assert_eq!(normal["deploymentMode"], json!("3p"));
        assert_eq!(normal["locale"], json!("en"), "unrelated key preserved");
        assert_eq!(threep["deploymentMode"], json!("3p"));
        assert_eq!(profile["inferenceProvider"], json!("gateway"));
        assert_eq!(
            profile["inferenceGatewayBaseUrl"],
            json!("https://gw.example.com")
        );
        assert_eq!(profile["inferenceGatewayApiKey"], json!("sk-secret-123456"));
        assert_eq!(profile["inferenceGatewayAuthScheme"], json!("bearer"));
        assert_eq!(profile["disableDeploymentModeChooser"], json!(true));
        assert_eq!(profile["coworkEgressAllowedHosts"], json!(["*"]));
        assert!(
            profile.get("inferenceModels").is_none(),
            "no models in the list → inferenceModels omitted (Claude Desktop auto-discovers from the gateway)"
        );
        assert_eq!(meta["appliedId"], json!(PROFILE_ID));
        assert!(meta["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == json!(PROFILE_ID) && e["name"] == json!(PROFILE_NAME)));
    }

    #[test]
    fn apply_aborts_and_restores_on_corrupt_config_instead_of_truncating() {
        let root = tempdir("corrupt");
        let paths = test_paths(&root);
        fs::create_dir_all(root.join("Claude")).unwrap();
        // A config serde_json can't parse (trailing comma) — must NOT be
        // silently truncated to `{ "deploymentMode": "3p" }`.
        let corrupt = r#"{ "mcpServers": { "x": {} }, }"#;
        fs::write(&paths.normal_config, corrupt).unwrap();

        let profile = build_profile(&provider("p", "https://gw.example.com", "sk-key"));
        let res = with_rollback(&paths, |p| apply_to_paths(p, &profile));

        assert!(res.is_err(), "corrupt config must abort the apply");
        // The user's config is preserved byte-for-byte (the bug truncated it).
        assert_eq!(fs::read_to_string(&paths.normal_config).unwrap(), corrupt);
    }

    #[test]
    fn build_profile_maps_models_and_merges_options() {
        use crate::providers::{ProviderModel, ProviderOption};
        let mut p = provider("p", "https://gw.example.com", "sk-key");
        // Claude Desktop has no primary model — `provider.model` is ignored.
        p.model = "should-be-ignored".into();
        p.models = vec![
            ProviderModel {
                id: "claude-sonnet-4-6[1m]".into(), // [1m] → supports1m
                name: String::new(),
            },
            ProviderModel {
                id: "claude-opus-4-8".into(),
                name: "Opus".into(), // label → labelOverride
            },
            ProviderModel {
                id: "claude-haiku-4-5".into(),
                name: String::new(), // plain string id
            },
        ];
        p.options = vec![
            ProviderOption {
                key: "inferenceGatewayHeaders.X-Foo".into(), // unmanaged → merges in
                value: "bar".into(),
            },
            ProviderOption {
                key: "inferenceProvider".into(), // exact managed → skipped
                value: "hacked".into(),
            },
            ProviderOption {
                // ROOT-managed (dot-suffixed) → must be skipped, else it would
                // clobber the gateway URL string with an object.
                key: "inferenceGatewayBaseUrl.evil".into(),
                value: "pwn".into(),
            },
        ];

        let profile = build_profile(&p);
        let v = JsonValue::Object(profile);

        // `provider.model` ("should-be-ignored") does NOT appear.
        assert_eq!(
            v["inferenceModels"],
            json!([
                { "name": "claude-sonnet-4-6", "supports1m": true },
                { "name": "claude-opus-4-8", "labelOverride": "Opus" },
                "claude-haiku-4-5"
            ])
        );
        // Unmanaged option merged into the profile JSON (dot-path).
        assert_eq!(v["inferenceGatewayHeaders"]["X-Foo"], json!("bar"));
        // Exact managed key can't be clobbered by an option.
        assert_eq!(v["inferenceProvider"], json!("gateway"));
        // Dot-suffixed managed key is also skipped — the gateway URL stays a
        // string, not clobbered into an object by `inferenceGatewayBaseUrl.evil`.
        assert_eq!(
            v["inferenceGatewayBaseUrl"],
            json!("https://gw.example.com")
        );
    }

    #[test]
    fn restore_official_clears_profile_and_meta() {
        let root = tempdir("restore");
        let paths = test_paths(&root);
        // Another tool's profile + entry must survive our restore.
        apply_to_paths(
            &paths,
            &build_profile(&provider("p", "https://gw.example.com", "sk-secret-123456")),
        )
        .unwrap();
        {
            let mut meta = read(&paths.meta);
            let entries = meta["entries"].as_array_mut().unwrap();
            entries.push(json!({ "id": "other-tool", "name": "Other" }));
            fs::write(&paths.meta, serde_json::to_string(&meta).unwrap()).unwrap();
        }

        restore_at_paths(&paths).unwrap();

        assert_eq!(read(&paths.normal_config)["deploymentMode"], json!("1p"));
        assert_eq!(read(&paths.threep_config)["deploymentMode"], json!("1p"));
        assert!(!paths.profile.exists(), "our profile file deleted");
        let meta = read(&paths.meta);
        assert!(
            !meta["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["id"] == json!(PROFILE_ID)),
            "our meta entry removed"
        );
        // appliedId fell back to the surviving entry, not left dangling on us.
        assert_eq!(meta["appliedId"], json!("other-tool"));
    }

    #[test]
    fn read_active_matches_provider_then_reverts() {
        let root = tempdir("read");
        let paths = test_paths(&root);
        let p = provider("cd-1", "https://gw.example.com", "sk-secret-123456");

        // Before apply: official.
        assert_eq!(
            read_active_at(&paths, &[p.clone()]).kind,
            ActiveKind::Official
        );

        apply_to_paths(&paths, &build_profile(&p)).unwrap();
        let st = read_active_at(&paths, &[p.clone()]);
        assert_eq!(st.kind, ActiveKind::Custom);
        assert_eq!(st.matched_provider_id.as_deref(), Some("cd-1"));

        // A gateway profile we don't have in the list → Unmanaged.
        let st_unmanaged = read_active_at(&paths, &[]);
        assert_eq!(st_unmanaged.kind, ActiveKind::Unmanaged);

        restore_at_paths(&paths).unwrap();
        assert_eq!(read_active_at(&paths, &[p]).kind, ActiveKind::Official);
    }

    /// Path-injected mirror of `read_active`, so the reverse-derivation is
    /// testable without touching real `~/Library` paths.
    fn read_active_at(paths: &Paths, providers: &[Provider]) -> ActiveState {
        let profile = read_json_or_empty(&paths.profile).unwrap();
        let base_url = profile
            .get("inferenceGatewayBaseUrl")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        let api_key = profile
            .get("inferenceGatewayApiKey")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        if base_url.is_none() && api_key.is_none() {
            return official_state(paths.profile.display().to_string());
        }
        let matched = providers.iter().find(|p| {
            p.app == CliApp::ClaudeDesktop
                && p.kind == ProviderKind::Custom
                && string_match(&p.base_url, base_url.as_deref())
                && string_match(&p.api_key, api_key.as_deref())
        });
        ActiveState {
            app: CliApp::ClaudeDesktop,
            kind: if matched.is_some() {
                ActiveKind::Custom
            } else {
                ActiveKind::Unmanaged
            },
            matched_provider_id: matched.map(|p| p.id.clone()),
            live_snapshot: Some(LiveSnapshot {
                base_url,
                api_key_masked: api_key_masked_opt(&profile),
                model: None,
            }),
            live_path: paths.profile.display().to_string(),
            configured_provider_ids: Vec::new(),
        }
    }

    fn api_key_masked_opt(profile: &Map<String, JsonValue>) -> Option<String> {
        profile
            .get("inferenceGatewayApiKey")
            .and_then(JsonValue::as_str)
            .map(mask_secret)
    }
}
