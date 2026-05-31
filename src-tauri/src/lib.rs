mod config;
mod providers;
mod sessions;
mod watcher;

/// Shared test infrastructure. The `HOME_LOCK` mutex serializes tests
/// that mutate the `HOME` env var across both `config` and
/// `providers` modules — without a single shared lock, parallel test
/// execution lets one module clobber another's HOME override.
#[cfg(test)]
pub(crate) mod testutils {
    use std::sync::Mutex;
    pub static HOME_LOCK: Mutex<()> = Mutex::new(());
}

use providers::{
    activate, deactivate, delete_provider_traces, detect_cli_versions, detect_installed_clis,
    fetch_favicon as providers_fetch_favicon, fetch_models, read_active_state,
    set_opencode_default, test_provider, ActiveState, CliApp, ModelListResult, Provider,
    TestResult,
};
use sessions::{get_session, scan_sessions, search_sessions, AppSession, SearchHit, SessionDetail};
use tauri::Manager;

#[tauri::command]
async fn scan_all_sessions(
    watcher: tauri::State<'_, watcher::WatcherHandle>,
) -> Result<Vec<AppSession>, String> {
    let handle = watcher.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let sessions = scan_sessions().map_err(|err| err.to_string())?;
        // Tell the watcher about the project cwds we just discovered so
        // it can dynamically watch them (catching per-project
        // CLAUDE.md / AGENTS.md / .claude/skills/ edits without
        // recursively watching every cwd the user might be in).
        let cwds =
            watcher::dynamic_paths_from_sessions(sessions.iter().map(|s| s.project.as_str()));
        handle.reconfigure_dynamic(cwds);
        Ok(sessions)
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Open one record by `(source, id)`. The Rust side looks up the
/// path from the index populated by the most recent `scan_sessions`
/// — `path` is never accepted from the frontend, so a hypothetical
/// renderer-side injection vector can't ask Termory to open
/// `/etc/passwd` (or anything else not in the current scan set).
#[tauri::command]
async fn load_session(source: String, id: String) -> Result<SessionDetail, String> {
    tauri::async_runtime::spawn_blocking(move || {
        get_session(&source, &id).map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
async fn search_all_sessions(query: String) -> Result<Vec<SearchHit>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        search_sessions(&query).map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Detect which CLI binaries are reachable on `$PATH`. Result is
/// fresh per call — the frontend re-checks on Providers page mount
/// and before every action so newly-installed CLIs surface without
/// an app restart.
#[tauri::command]
async fn detect_clis() -> Result<std::collections::HashMap<String, bool>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let map = detect_installed_clis();
        let serialized = map
            .into_iter()
            .map(|(app, installed)| (cli_app_key(app).to_string(), installed))
            .collect();
        Ok(serialized)
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Spawn each installed CLI with `--version` and return the parsed
/// version. Heavier than [`detect_clis`] (4 subprocesses), so the
/// frontend calls this only on page mount / Recheck.
#[tauri::command]
async fn detect_cli_versions_cmd(
) -> Result<std::collections::HashMap<String, Option<String>>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let map = detect_cli_versions();
        let serialized = map
            .into_iter()
            .map(|(app, version)| (cli_app_key(app).to_string(), version))
            .collect();
        Ok(serialized)
    })
    .await
    .map_err(|err| err.to_string())?
}

fn cli_app_key(app: CliApp) -> &'static str {
    match app {
        CliApp::Claude => "claude",
        CliApp::Codex => "codex",
        CliApp::Gemini => "gemini",
        CliApp::Opencode => "opencode",
    }
}

/// Reverse-derive the active provider state for one CLI. The frontend
/// passes its current Provider list so we can match against it; nothing
/// is stored backend-side.
#[tauri::command]
async fn provider_active_state(
    app: String,
    providers: Vec<Provider>,
) -> Result<ActiveState, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        read_active_state(cli, &providers).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Reverse-derive active state for all four CLIs in one call.
#[tauri::command]
async fn provider_active_states(providers: Vec<Provider>) -> Result<Vec<ActiveState>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut out = Vec::with_capacity(4);
        for app in [
            CliApp::Claude,
            CliApp::Codex,
            CliApp::Gemini,
            CliApp::Opencode,
        ] {
            out.push(read_active_state(app, &providers).map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Activate a Custom provider — materializes it into the matching
/// CLI's live config. Single-slot CLIs (Claude/Codex/Gemini) write
/// directly into the CLI's primary slot (overwriting any previous
/// Termory write). For OpenCode this only adds the provider's slot to
/// opencode.json; promoting it to startup default is a separate call
/// (`set_opencode_default_provider`).
#[tauri::command]
async fn activate_provider(
    provider: Provider,
    providers_for_app: Vec<Provider>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        activate(&provider, &providers_for_app).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Promote a Termory OpenCode provider to OpenCode's startup default
/// by writing `model = "<termory-id>/<primary>"` at the top of
/// opencode.json. The provider must already be activated.
#[tauri::command]
async fn set_opencode_default_provider(provider: Provider) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        set_opencode_default(&provider).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Surgical per-provider cleanup before delete. For Claude/Codex/Gemini
/// this is a no-op (single-slot — the delete flow runs deactivate
/// when the provider is in use). For OpenCode it removes only this
/// provider's `termory-<id>` slot from opencode.json (plus clears the
/// top-level `model` if it pointed here); sibling Termory slots and
/// any /connect entries in `auth.json` stay untouched.
#[tauri::command]
async fn delete_provider(provider: Provider) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        delete_provider_traces(&provider).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Restore a CLI to its native auth flow by clearing all
/// Termory-injected fields.
#[tauri::command]
async fn deactivate_provider(app: String, providers_for_app: Vec<Provider>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        deactivate(cli, &providers_for_app).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Send a connectivity probe to the provider's base URL.
#[tauri::command]
async fn test_provider_api(provider: Provider) -> Result<TestResult, String> {
    Ok(test_provider(&provider).await)
}

/// Hit the provider's models endpoint and return the available model
/// ids. Used to populate the Model field autocomplete suggestions.
#[tauri::command]
async fn fetch_provider_models(provider: Provider) -> Result<ModelListResult, String> {
    Ok(fetch_models(&provider).await)
}

/// Best-effort fetch of a `data:image/...;base64,...` favicon for the
/// given URL. Called from the editor's save path so the favicon is
/// cached into providers.json once and the renderer never has to make
/// a third-party request to display it.
#[tauri::command]
async fn fetch_provider_favicon(url: String) -> Result<Option<String>, String> {
    Ok(providers_fetch_favicon(&url).await)
}

/// Read ~/.termory/config.json. Returns an empty `{}` if missing.
#[tauri::command]
async fn read_app_config() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| config::read_config().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// Atomically write ~/.termory/config.json with file mode 0600 (Unix).
#[tauri::command]
async fn write_app_config(value: serde_json::Value) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::write_config(&value).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read ~/.termory/providers.json. Returns an empty `[]` if missing.
/// Separate file because it holds API keys — file is chmod 0600.
#[tauri::command]
async fn read_app_providers() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| config::read_providers().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// Atomically write ~/.termory/providers.json with file mode 0600 (Unix).
#[tauri::command]
async fn write_app_providers(value: serde_json::Value) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::write_providers(&value).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read ~/.termory/favorites.json. Returns an empty `[]` if missing.
/// Stored separately because favorites can contain user-typed prompts
/// with PII or accidentally-pasted secrets — file is chmod 0600.
#[tauri::command]
async fn read_app_favorites() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| config::read_favorites().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// Atomically write ~/.termory/favorites.json with file mode 0600 (Unix).
#[tauri::command]
async fn write_app_favorites(value: serde_json::Value) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::write_favorites(&value).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

pub fn run() {
    // Log target directory: `~/.termory/logs/`. Falls back to the
    // OS-default log location if HOME isn't readable so app launch
    // never depends on logging setup. Mirrors where `config.json` and
    // `providers.json` live so users have one place to look at when
    // reporting a bug.
    let logs_dir = dirs::home_dir().map(|h| h.join(".termory").join("logs"));

    let log_plugin = {
        let mut builder = tauri_plugin_log::Builder::new()
            .max_file_size(5 * 1024 * 1024) // 5 MB
            .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepAll)
            .level(log::LevelFilter::Info);
        if let Some(path) = logs_dir {
            // Ensure the dir exists so the plugin's open() doesn't
            // race on first launch.
            let _ = std::fs::create_dir_all(&path);
            builder = builder.targets([
                tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Folder {
                    path,
                    file_name: Some("termory".into()),
                }),
                tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
            ]);
        }
        builder.build()
    };

    tauri::Builder::default()
        .plugin(log_plugin)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            scan_all_sessions,
            load_session,
            search_all_sessions,
            detect_clis,
            detect_cli_versions_cmd,
            provider_active_state,
            provider_active_states,
            activate_provider,
            deactivate_provider,
            delete_provider,
            set_opencode_default_provider,
            test_provider_api,
            fetch_provider_models,
            fetch_provider_favicon,
            read_app_config,
            write_app_config,
            read_app_providers,
            write_app_providers,
            read_app_favorites,
            write_app_favorites,
        ])
        .setup(|app| {
            // Background filesystem watcher: pushes a fresh
            // `Vec<AppSession>` to the frontend via
            // `termory:sources-changed` whenever a watched source
            // directory mutates. Failure is non-fatal — the app still
            // works with only the launch-time scan.
            let handle = app.handle().clone();
            match watcher::start(handle) {
                Ok(watcher_handle) => {
                    app.manage(watcher_handle);
                }
                Err(err) => {
                    log::error!("watcher init failed: {err}");
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
