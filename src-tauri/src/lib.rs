mod claude_desktop;
mod codex_follow;
mod config;
mod providers;
mod quota;
mod sessions;
mod terminal;
mod tray;
mod watcher;

/// Shared test infrastructure. The `HOME_LOCK` mutex serializes tests
/// that mutate the `HOME` env var across both `config` and
/// `providers` modules — without a single shared lock, parallel test
/// execution lets one module clobber another's HOME override.
#[cfg(test)]
pub(crate) mod testutils {
    use std::sync::Mutex;
    pub static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Panic-safe override of a process env var, restored on drop. Hold
    /// `HOME_LOCK` while using it so concurrent tests never observe the
    /// temporary value (the var resolvers read process-global state).
    pub struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvVarGuard {
        pub fn set(key: &'static str, val: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, val);
            EnvVarGuard { key, prev }
        }
        pub fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            EnvVarGuard { key, prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

use providers::{
    activate, deactivate, delete_provider_traces, detect_cli_versions,
    detect_gateway_apis as providers_detect_gateway_apis, detect_installed_clis,
    fetch_favicon as providers_fetch_favicon, fetch_models, read_active_state,
    set_opencode_default, test_provider, ActiveState, CliApp, GatewayCapabilities, ModelListResult,
    Provider, TestResult,
};
use sessions::{get_session, scan_sessions, search_sessions, SearchHit, SessionDetail};
use tauri::Manager;

#[tauri::command]
async fn scan_all_sessions(
    app: tauri::AppHandle,
    watcher: tauri::State<'_, watcher::WatcherHandle>,
) -> Result<sessions::ScanResult, String> {
    let handle = watcher.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let result = scan_sessions().map_err(|err| err.to_string())?;
        // Tell the watcher about the project cwds we just discovered so
        // it can dynamically watch them (catching per-project
        // CLAUDE.md / AGENTS.md / .claude/skills/ edits without
        // recursively watching every cwd the user might be in).
        let cwds = watcher::dynamic_paths_from_sessions(
            result.projects.iter().map(|p| p.project.as_str()),
        );
        handle.reconfigure_dynamic(cwds);
        Ok::<_, String>(result)
    })
    .await
    .map_err(|err| err.to_string())??;
    // Refresh the tray's "recent sessions" list from this scan.
    tray::refresh_recent(&app, &result.records);
    Ok(result)
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

/// Mainstream terminals installed on this OS (+ "auto"), for the
/// Settings → Terminal dropdown.
#[tauri::command]
async fn detect_terminals() -> Result<Vec<terminal::TerminalOption>, String> {
    tauri::async_runtime::spawn_blocking(terminal::detect)
        .await
        .map_err(|err| err.to_string())
}

/// Open a recorded session in the user's chosen terminal and resume it in its
/// CLI (Settings → Terminal). Driven by the Records / Favorites right-click
/// menu — same path the tray's recent-session click uses.
#[tauri::command]
fn resume_session_in_terminal(
    source: String,
    id: String,
    project: Option<String>,
) -> Result<(), String> {
    terminal::resume_session(&source, &id, project.as_deref())
}

/// Open the user's chosen terminal in a project's cwd and launch a FRESH session
/// of `source`'s CLI there (no resume). Driven by the Records sidebar project-row
/// "New session" action — same `terminal::new_session` the tray's New Session uses.
#[tauri::command]
fn new_session_in_terminal(source: String, project: Option<String>) -> Result<(), String> {
    terminal::new_session(&source, project.as_deref())
}

/// Migrate a renamed Claude project's sessions + memory into the new path's
/// slug dir so the CLI lists/resumes them again. Copies by default (the old
/// dir stays as a backup); `delete_old` drops the source only after a clean
/// copy. Driven by the Records right-click "migrate" action.
#[tauri::command]
async fn migrate_claude_project(
    old_path: String,
    new_path: String,
    delete_old: bool,
) -> Result<sessions::ClaudeMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_claude_project(&old_path, &new_path, delete_old)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Migrate one Claude session into the new path's slug dir, located by
/// project + the record's path relative to its slug dir (no filesystem path).
#[tauri::command]
async fn migrate_claude_session(
    project: String,
    rel: String,
    new_path: String,
    delete_old: bool,
) -> Result<sessions::ClaudeMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_claude_session(&project, &rel, &new_path, delete_old)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Migrate one Claude auto-memory into the new path's slug dir, located by
/// project + the record's path relative to its slug dir.
#[tauri::command]
async fn migrate_claude_memory(
    project: String,
    rel: String,
    new_path: String,
    delete_old: bool,
) -> Result<sessions::ClaudeMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_claude_memory(&project, &rel, &new_path, delete_old)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Permanently delete a whole Claude project (slug dir).
#[tauri::command]
async fn delete_claude_project(project: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_claude_project(&project))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete one Claude session (by project + slug-relative path).
#[tauri::command]
async fn delete_claude_session(project: String, rel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_claude_session(&project, &rel))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete one Claude auto-memory (by project + memory-relative path).
#[tauri::command]
async fn delete_claude_memory(project: String, rel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_claude_memory(&project, &rel))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete a Gemini project (its `~/.gemini/tmp/<id>/` dir).
#[tauri::command]
async fn delete_gemini_project(project: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_gemini_project(&project))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete one Gemini session (by project + tmp-dir-relative path).
#[tauri::command]
async fn delete_gemini_session(project: String, rel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_gemini_session(&project, &rel))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete one Gemini auto-memory (by project + memory-relative path).
#[tauri::command]
async fn delete_gemini_memory(project: String, rel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_gemini_memory(&project, &rel))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete one Codex session by thread id (row + rollout file).
#[tauri::command]
async fn delete_codex_session(id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_codex_session(&id))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete every Codex session under a project cwd (rows + files).
#[tauri::command]
async fn delete_codex_project(project: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_codex_project(&project))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete one Codex auto-memory (.md under ~/.codex/memories/).
#[tauri::command]
async fn delete_codex_memory(rel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_codex_memory(&rel))
        .await
        .map_err(|e| e.to_string())?
}

/// Migrate one Codex session (by thread id) to a new cwd — rewrites the rollout
/// file's payload.cwd + the threads row. Regrouping convenience; resume-by-id
/// already works across renames.
#[tauri::command]
async fn migrate_codex_session(
    id: String,
    new_path: String,
) -> Result<sessions::CodexMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::migrate_codex_session(&id, &new_path))
        .await
        .map_err(|e| e.to_string())?
}

/// Migrate every Codex session under a project cwd to a new cwd (incl archived).
#[tauri::command]
async fn migrate_codex_project(
    project: String,
    new_path: String,
) -> Result<sessions::CodexMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_codex_project(&project, &new_path)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Permanently delete one OpenCode session by id (cascades to message/part).
#[tauri::command]
async fn delete_opencode_session(id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_opencode_session(&id))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete an OpenCode project (keyed by worktree; cascades).
#[tauri::command]
async fn delete_opencode_project(project: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_opencode_project(&project))
        .await
        .map_err(|e| e.to_string())?
}

/// Read-only: is `path` registered in Claude's `~/.claude.json`? After a Claude
/// migration the frontend uses this to warn that the moved sessions won't show
/// in `claude --resume` until the user opens Claude in the new dir once. Termory
/// never writes that file.
#[tauri::command]
async fn claude_project_registered(path: String) -> Result<bool, String> {
    Ok(sessions::claude_project_registered(&path))
}

/// Push the app-language strings for the tray's static rows (Open / Official /
/// Exit) and rebuild the menu. The frontend calls this on language load/change
/// so the menu bar follows the app language (CLI / provider names stay as-is).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn set_tray_labels(
    app: tauri::AppHandle,
    open: String,
    official: String,
    exit: String,
    five_hour: String,
    weekly: String,
    monthly: String,
    new_session: String,
    choose_folder: String,
    status_busy: String,
    status_waiting: String,
) -> Result<(), String> {
    tray::set_labels(
        open,
        official,
        exit,
        five_hour,
        weekly,
        monthly,
        new_session,
        choose_folder,
        status_busy,
        status_waiting,
    );
    let _ = tray::rebuild_menu(&app);
    Ok(())
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
        CliApp::ClaudeDesktop => "claude-desktop",
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
        let mut out = Vec::with_capacity(CliApp::all().len());
        for app in CliApp::all() {
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
    app: tauri::AppHandle,
    provider: Provider,
    providers_for_app: Vec<Provider>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        activate(&provider, &providers_for_app).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())??;
    let _ = tray::rebuild_menu(&app);
    Ok(())
}

/// List the most recent distinct Codex project cwds (read-only) for the
/// switch-time "follow sessions" picker. Newest first, capped at `limit`.
#[tauri::command]
async fn recent_codex_projects(
    limit: usize,
) -> Result<Vec<codex_follow::RecentCodexProject>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        codex_follow::recent_projects(limit).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Re-tag the selected Codex projects' live threads to `target_provider_id`
/// so `codex resume` lists them under the now-active provider. Backs up
/// state_5.sqlite first, records originals for reversibility, and refuses to
/// write while Codex holds the DB lock.
#[tauri::command]
async fn follow_codex_sessions(
    projects: Vec<String>,
    target_provider_id: String,
) -> Result<codex_follow::FollowResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        codex_follow::follow_projects(&projects, &target_provider_id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Promote a Termory OpenCode provider to OpenCode's startup default
/// by writing `model = "<termory-id>/<primary>"` at the top of
/// opencode.json. The provider must already be activated.
#[tauri::command]
async fn set_opencode_default_provider(
    app: tauri::AppHandle,
    provider: Provider,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        set_opencode_default(&provider).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())??;
    let _ = tray::rebuild_menu(&app);
    Ok(())
}

/// Surgical per-provider cleanup before delete. For Claude/Codex/Gemini
/// this is a no-op (single-slot — the delete flow runs deactivate
/// when the provider is in use). For OpenCode it removes only this
/// provider's `termory-<id>` slot from opencode.json (plus clears the
/// top-level `model` if it pointed here); sibling Termory slots and
/// any /connect entries in `auth.json` stay untouched.
#[tauri::command]
async fn delete_provider(app: tauri::AppHandle, provider: Provider) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        delete_provider_traces(&provider).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())??;
    let _ = tray::rebuild_menu(&app);
    Ok(())
}

/// Restore a CLI to its native auth flow by clearing all
/// Termory-injected fields.
#[tauri::command]
async fn deactivate_provider(
    handle: tauri::AppHandle,
    app: String,
    providers_for_app: Vec<Provider>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        deactivate(cli, &providers_for_app).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())??;
    let _ = tray::rebuild_menu(&handle);
    Ok(())
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

/// Official-account subscription quota (5-hour / weekly rate-limit
/// windows) for one CLI, read from its existing OAuth login. Claude
/// implemented; other apps report `not_found`. Never returns Err —
/// credential / network problems surface inside the result so the
/// frontend can render a per-state message instead of a raw toast.
#[tauri::command]
async fn fetch_subscription_quota(
    handle: tauri::AppHandle,
    app: String,
) -> Result<quota::SubscriptionQuota, String> {
    let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
    let result = quota::fetch_quota(cli).await;
    // Keep the tray's quota row in sync with what the Providers page
    // just fetched (no extra network hit).
    tray::refresh_quota(&handle, &result);
    Ok(result)
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
async fn write_app_providers(
    app: tauri::AppHandle,
    value: serde_json::Value,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::write_providers(&value).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = tray::rebuild_menu(&app);
    Ok(())
}

/// Probe a gateway's `{baseUrl, apiKey}` and report which API modes it
/// supports (OpenAI / OpenAI-Responses / Anthropic / Gemini), so the
/// gateway tab can offer only the matching CLIs for binding.
#[tauri::command]
async fn detect_gateway_apis(
    base_url: String,
    api_key: String,
) -> Result<GatewayCapabilities, String> {
    Ok(providers_detect_gateway_apis(&base_url, &api_key).await)
}

/// Read the `gateways` array from ~/.termory/providers.json (
/// entries with `bindings`). Returns `[]` if missing. Same file as the
/// per-CLI providers, separate array.
#[tauri::command]
async fn read_app_gateways() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| config::read_gateways().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// Atomically write the `gateways` array (preserving the sibling
/// `providers` array), file mode 0600 on Unix.
#[tauri::command]
async fn write_app_gateways(value: serde_json::Value) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::write_gateways(&value).map_err(|e| e.to_string())
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

    // `mut` is only needed for the macOS pre-run activation-policy
    // call below.
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut app = tauri::Builder::default()
        .plugin(log_plugin)
        .plugin(tauri_plugin_dialog::init())
        // Launch-at-login (Settings → Startup). macOS uses a
        // LaunchAgent plist; Windows the Run registry key; Linux an
        // XDG autostart .desktop entry — all handled by the plugin.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // The login item launches with this flag so setup() can
            // start tray-only (window + Dock hidden); a manual open
            // has no flag and shows the window as usual.
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            scan_all_sessions,
            load_session,
            search_all_sessions,
            detect_clis,
            detect_terminals,
            resume_session_in_terminal,
            new_session_in_terminal,
            migrate_claude_project,
            migrate_claude_session,
            migrate_claude_memory,
            delete_claude_project,
            delete_claude_session,
            delete_claude_memory,
            delete_gemini_project,
            delete_gemini_session,
            delete_gemini_memory,
            delete_codex_session,
            delete_codex_project,
            delete_codex_memory,
            migrate_codex_session,
            migrate_codex_project,
            delete_opencode_session,
            delete_opencode_project,
            claude_project_registered,
            set_tray_labels,
            detect_cli_versions_cmd,
            provider_active_state,
            provider_active_states,
            activate_provider,
            deactivate_provider,
            delete_provider,
            set_opencode_default_provider,
            recent_codex_projects,
            follow_codex_sessions,
            test_provider_api,
            fetch_subscription_quota,
            fetch_provider_models,
            fetch_provider_favicon,
            detect_gateway_apis,
            read_app_config,
            write_app_config,
            read_app_providers,
            write_app_providers,
            read_app_gateways,
            write_app_gateways,
            read_app_favorites,
            write_app_favorites,
        ])
        .on_window_event(|window, event| {
            // Closing the window hides it instead of quitting, so the
            // app keeps running in the menu-bar tray (switch providers,
            // reopen via the tray's "Open"). Real quit goes through the
            // tray's "Exit" → app.exit(0).
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // No window on screen → hide the Dock icon too, so
                // Termory lives only in the menu bar. The tray's "Open"
                // brings both back (set_dock_visibility inside the
                // helper is the purpose-built API — it handles the
                // macOS activation quirks a raw activation-policy
                // toggle does not).
                tray::hide_main_window(window.app_handle());
                api.prevent_close();
            }
        })
        .setup(|app| {
            // The window is declared `visible: false` so a login-item
            // launch never flashes it. Launched by the login item
            // (--autostart, see the autostart plugin init): stay
            // tray-only — same end state as closing the window; the
            // tray's "Open" (or a Dock/Finder reopen) brings it back.
            // A normal launch shows the window here instead.
            if std::env::args().any(|a| a == "--autostart") {
                tray::hide_main_window(app.handle());
            } else {
                tray::show_main_window(app.handle());
            }
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
            // System tray (macOS menu bar) — one click → menu listing
            // all providers per CLI for one-tap switching. Tray failure
            // is non-fatal; the app window still works.
            if let Err(err) = tray::install(app.handle()) {
                log::error!("tray install failed: {err}");
            }
            // One-shot warm-up of the tray's Claude quota row (5h /
            // Weekly) so the FIRST menu open already shows numbers.
            // After this there is NO polling — the row refreshes on
            // tray click (menu open, rate-limited in trigger_quota_refresh)
            // and whenever the Providers page fetches via IPC.
            tray::trigger_quota_refresh(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // --autostart (login item): suppress the macOS Dock icon BEFORE
    // the event loop starts. The setup()-time set_dock_visibility
    // runs after NSApp already registered with the Dock, so the icon
    // flashed briefly; ActivationPolicy::Accessory set pre-run keeps
    // it from ever appearing. The tray's "Open" restores it later via
    // set_dock_visibility(true) (the reliable restore API — see the
    // window-event comment above).
    #[cfg(target_os = "macos")]
    if std::env::args().any(|a| a == "--autostart") {
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    }

    app.run(|app_handle, event| {
        // macOS: re-launching the app from Finder / the Dock when it's
        // already running (the window was closed → hidden in the menu
        // bar, Dock icon gone) fires a Reopen event instead of a fresh
        // launch. Without handling it the click does nothing and the app
        // looks stuck. Re-show the window, same as the tray's "Open".
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            tray::show_main_window(app_handle);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (app_handle, event);
        }
    });
}
