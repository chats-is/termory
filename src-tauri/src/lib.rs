mod accounts;
mod claude_auth;
mod claude_desktop;
mod codex_follow;
mod config;
mod process;
mod providers;
mod quota;
mod sessions;
mod terminal;
mod tray;
mod updates;
mod upgrade;
mod watcher;

/// Shared test infrastructure. The `HOME_LOCK` mutex serializes every
/// env-mutating test across ALL modules (config / providers / accounts /
/// sessions / quota / watcher) — without a single shared lock, parallel
/// test execution lets one module clobber another's HOME override.
#[cfg(test)]
pub(crate) mod testutils {
    use std::sync::Mutex;
    pub static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Serialize env-mutating tests, tolerating poison: a test that
    /// panicked already failed on its own — letting the poison cascade
    /// turns ONE failure into dozens of PoisonError noise for every
    /// later test in the queue (seen on the first CI run).
    pub fn lock_home() -> std::sync::MutexGuard<'static, ()> {
        HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `\` → `/` so path assertions written with unix separators hold
    /// on Windows too (scan output uses the OS-native separator).
    pub fn norm(p: &str) -> String {
        p.replace('\\', "/")
    }

    /// Redirect every home-derived path to `dir` for the guard's
    /// lifetime: sets HOME — which `crate::home_dir()` honors in TEST
    /// builds on EVERY OS (plain `dirs::home_dir()` on Windows resolves
    /// via SHGetKnownFolderPath and ignores the environment entirely) —
    /// and UNSETS XDG_CONFIG_HOME / XDG_DATA_HOME (GitHub's ubuntu
    /// runners preset them, which leaks the runner's real ~/.config
    /// into XDG-honoring resolvers while HOME points at the tempdir).
    /// Hold `lock_home()` while using it.
    pub struct HomeOverride {
        _guards: Vec<EnvVarGuard>,
    }
    pub fn override_home(dir: impl AsRef<std::ffi::OsStr>) -> HomeOverride {
        let dir = dir.as_ref();
        HomeOverride {
            _guards: vec![
                EnvVarGuard::set("HOME", dir),
                EnvVarGuard::unset("XDG_CONFIG_HOME"),
                EnvVarGuard::unset("XDG_DATA_HOME"),
            ],
        }
    }

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

/// Crate-wide home resolution — every path derivation routes through
/// here instead of calling `dirs::home_dir()` directly.
///
/// In TEST builds a set `HOME` env var wins on EVERY OS: plain
/// `dirs::home_dir()` on Windows resolves via
/// `SHGetKnownFolderPath(FOLDERID_Profile)` and ignores the
/// environment entirely, so without this override the HOME-redirect
/// test infrastructure silently read/wrote the REAL user profile on
/// Windows (cross-test contamination — found by the first Windows CI
/// runs). Release builds compile without `cfg(test)`, so production
/// behavior is exactly `dirs::home_dir()` everywhere.
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return Some(std::path::PathBuf::from(h));
        }
    }
    dirs::home_dir()
}

use providers::{
    activate, deactivate, delete_provider_traces, detect_cli_versions,
    detect_gateway_apis as providers_detect_gateway_apis, detect_install_snapshot,
    fetch_favicon as providers_fetch_favicon, fetch_models, read_active_state, set_default,
    test_provider, ActiveState, CliApp, GatewayCapabilities, ModelListResult, Provider,
    ProviderList, TestResult,
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
/// fully fresh per call — the frontend re-checks on Providers page
/// mount and before every action so newly-installed CLIs surface
/// without an app restart. This is the COLD path (a user-visible
/// Recheck, off the main thread), so it clears the hot path's
/// shell-probe cache first and eats the real ~1s-per-missing-CLI
/// spawn: it's the explicit escape hatch for an install in a dir
/// Termory neither scans nor watches.
#[tauri::command]
async fn detect_clis(
    app: tauri::AppHandle,
) -> Result<std::collections::HashMap<String, bool>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        providers::clear_shell_probe_cache();
        let snapshot = detect_install_snapshot();
        // The page just paid for a fresh probe — hand the WHOLE snapshot
        // to the tray so a Providers-page Recheck also updates the menu
        // (compare + rebuild only when the install state actually
        // changed, including codex's terminal capability).
        tray::refresh_installed_with(&app, snapshot.clone());
        let serialized = snapshot
            .map
            .into_iter()
            .map(|(cli, installed)| (cli_app_key(cli).to_string(), installed))
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

/// Every app's upgrade command, keyed by CliApp string, for the update
/// badge's tooltip. Apps with no command-line upgrade (the self-updating
/// GUI apps) are absent. Cheap: constants, plus one path stat +
/// canonicalize for Gemini.
#[tauri::command]
async fn cli_upgrade_commands() -> Result<std::collections::HashMap<String, String>, String> {
    tauri::async_runtime::spawn_blocking(upgrade::upgrade_commands)
        .await
        .map_err(|err| err.to_string())
}

/// Run `app`'s upgrade in-app, resolving when it finishes. Runs through
/// an interactive login shell so it resolves the nvm / brew shims a GUI
/// process's bare PATH lacks (see the `upgrade` module docs).
#[tauri::command]
async fn run_cli_upgrade(app: providers::CliApp) -> Result<(), String> {
    upgrade::run_upgrade(app).await
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

/// Permanently delete one Grok session (its `<grok-home>/sessions/<cwd>/<id>/`
/// dir). Migration of grok sessions is intentionally not built yet.
#[tauri::command]
async fn delete_grok_session(id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_grok_session(&id))
        .await
        .map_err(|e| e.to_string())?
}

/// Permanently delete every Grok session under a project cwd (its encoded-cwd
/// session dirs).
#[tauri::command]
async fn delete_grok_project(project: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_grok_project(&project))
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

/// Permanently delete one Grok auto-memory (.md under <grok-home>/memory/).
#[tauri::command]
async fn delete_grok_memory(rel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || sessions::delete_grok_memory(&rel))
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

/// Re-point a Gemini project's history to a new cwd (rewrites its `.project_root`
/// ownership markers; gemini self-heals its registry from them on next run).
#[tauri::command]
async fn migrate_gemini_project(
    project: String,
    new_path: String,
) -> Result<sessions::GeminiMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_gemini_project(&project, &new_path)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Re-point one OpenCode session to a new cwd (UPDATE session.directory by id).
#[tauri::command]
async fn migrate_opencode_session(
    id: String,
    new_path: String,
) -> Result<sessions::OpencodeMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::migrate_opencode_session(&id, &new_path))
        .await
        .map_err(|e| e.to_string())?
}

/// Re-point a whole OpenCode project's sessions to a new cwd (by directory).
#[tauri::command]
async fn migrate_opencode_project(
    project: String,
    new_path: String,
) -> Result<sessions::OpencodeMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_opencode_project(&project, &new_path)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Move one Grok session's dir to a new cwd's encoded-cwd dir + rewrite its
/// summary.json info.cwd (grok resumes per-cwd by scanning the encoded dir).
#[tauri::command]
async fn migrate_grok_session(
    id: String,
    new_path: String,
) -> Result<sessions::GrokMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::migrate_grok_session(&id, &new_path))
        .await
        .map_err(|e| e.to_string())?
}

/// Move a whole Grok project's session dirs to a new cwd's encoded-cwd dir.
#[tauri::command]
async fn migrate_grok_project(
    project: String,
    new_path: String,
) -> Result<sessions::GrokMigrationResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::migrate_grok_project(&project, &new_path)
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
async fn set_tray_labels(app: tauri::AppHandle, labels: tray::TrayLabels) -> Result<(), String> {
    tray::set_labels(labels);
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

/// Codex's install forms, split for the Providers page: `cli` is the
/// standalone binary; `app` is the desktop app (macOS bundle id
/// `com.openai.codex` — the merged ChatGPT/Codex desktop app);
/// `bundled_cli` is the runnable codex CLI shipped INSIDE the app
/// (`Contents/Resources/codex`) — the fallback that keeps account add /
/// re-login working on app-only installs. Provider management needs
/// any of them (shared `~/.codex/`), which is what `detect_clis`'
/// codex entry says; the frontend disables login flows only when BOTH
/// `cli` and `bundled_cli` are false. One probe pass backend-side
/// (`providers::probe_codex_installs_detailed` — the cold-path variant
/// that also fetches the Windows app version via PowerShell; this IPC
/// runs on page load + Recheck, off the tray/watcher hot path).
#[tauri::command]
async fn detect_codex_installs() -> Result<providers::CodexInstalls, String> {
    tauri::async_runtime::spawn_blocking(|| Ok(providers::probe_codex_installs_detailed()))
        .await
        .map_err(|err| err.to_string())?
}

/// Latest available version for each managed CLI (see `updates.rs`),
/// keyed by the same CliApp string as `detect_cli_versions_cmd`. The
/// frontend compares these to the installed versions and shows an
/// update badge when behind. `force` bypasses the 6h cache (Recheck).
#[tauri::command]
async fn detect_latest_versions_cmd(
    force: Option<bool>,
) -> Result<std::collections::HashMap<String, Option<String>>, String> {
    let latest = updates::detect_latest_versions(force.unwrap_or(false)).await;
    let mut out: std::collections::HashMap<String, Option<String>> = latest
        .clis
        .into_iter()
        .map(|(app, version)| (cli_app_key(app).to_string(), version))
        .collect();
    // The Codex DESKTOP app rides in the same map under a key that is
    // deliberately NOT a CliApp: it's a second product under the codex
    // tab, versioned independently of the npm CLI. The frontend's
    // `cliVersionRecord` only picks CliApp keys, so this one passes
    // through it untouched and is read separately.
    out.insert(updates::CODEX_APP_KEY.to_string(), latest.codex_app);
    Ok(out)
}

fn cli_app_key(app: CliApp) -> &'static str {
    match app {
        CliApp::Claude => "claude",
        CliApp::Codex => "codex",
        CliApp::Gemini => "gemini",
        CliApp::Opencode => "opencode",
        CliApp::ClaudeDesktop => "claude-desktop",
        CliApp::Grok => "grok",
    }
}

/// Reverse-derive the active provider state for one CLI. The frontend
/// passes its current Provider list so we can match against it; nothing
/// is stored backend-side.
#[tauri::command]
async fn provider_active_state(
    app: String,
    providers: ProviderList,
) -> Result<ActiveState, String> {
    let providers = providers.0;
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        read_active_state(cli, &providers).map_err(|e| e.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Reverse-derive active state for all four CLIs in one call.
#[tauri::command]
async fn provider_active_states(providers: ProviderList) -> Result<Vec<ActiveState>, String> {
    let providers = providers.0;
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
/// Termory write). For the multi-slot CLIs (OpenCode + Grok) this only adds
/// the provider's slot/entries; promoting it to startup default is a
/// separate call (`set_default_provider`).
#[tauri::command]
async fn activate_provider(
    app: tauri::AppHandle,
    provider: Provider,
    providers_for_app: ProviderList,
) -> Result<(), String> {
    let providers_for_app = providers_for_app.0;
    tauri::async_runtime::spawn_blocking(move || {
        let cli = provider.app;
        let id = provider.id.clone();
        activate(&provider, &providers_for_app).map_err(|e| e.to_string())?;
        // Record the activation marker HERE, before the rebuild below, so the
        // menu is built with it. The page also writes it (`markActive`), but
        // only AFTER this call returns — leaving the freshly-built menu
        // resolving with the PREVIOUS marker, which checkmarks the wrong row
        // whenever two entries share one endpoint (the case the marker exists
        // for). Writing it first also makes the page's write a no-op, so
        // `write_app_config` sees no change and skips its own rebuild.
        config::set_active_provider_marker(cli.key(), Some(&id)).map_err(|e| e.to_string())
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

/// Promote an already-activated multi-slot provider to its CLI's startup
/// default. OpenCode writes `model = "<termory-id>/<primary>"` at the top of
/// opencode.json; Grok writes `models.default = "<pid>-<model>"` in
/// config.toml. The provider must already be activated (its slot/entries
/// exist). Multi-slot CLIs (OpenCode + Grok) have a separate enable vs.
/// set-default step; single-slot CLIs set their default implicitly on
/// activate.
#[tauri::command]
async fn set_default_provider(
    app: tauri::AppHandle,
    provider: Provider,
    providers_for_app: ProviderList,
) -> Result<(), String> {
    let providers_for_app = providers_for_app.0;
    tauri::async_runtime::spawn_blocking(move || {
        set_default(&provider, &providers_for_app).map_err(|e| e.to_string())
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
    providers_for_app: ProviderList,
) -> Result<(), String> {
    let providers_for_app = providers_for_app.0;
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        deactivate(cli, &providers_for_app).map_err(|e| e.to_string())?;
        // Official has no provider id, so the marker is REMOVED. Written here
        // for the same reason as in `activate_provider`: the rebuild below must
        // see it, and it makes the page's own `markActive(app, null)` a no-op.
        config::set_active_provider_marker(cli.key(), None).map_err(|e| e.to_string())
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
    // The page pulls on its own (tab entry, cache expiry, manual refresh),
    // so it needs the same guard as the credential watcher: mid-login, the
    // file on disk is not the user's account and reading it reports a
    // logout that clears the card. Returning early — WITHOUT
    // `refresh_quota` — is the point: that call would stamp the rate-limit
    // marker, and the real refresh triggered when the flow restores the
    // credential would then be refused by a floor this non-fetch earned.
    if accounts::login_in_progress(&handle, cli) {
        return Ok(quota::quota_during_login(cli));
    }
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

/// Persist ONE config key: read the current ~/.termory/config.json, change
/// only `key`, and write it back (mode 0600 on Unix). Every other key on
/// disk — including keys the app doesn't recognize and backend-written
/// `grok_prev_*` — is left exactly as it is. This is a per-key merge, never
/// a whole-object overwrite of a stale in-memory cache, so orphaned/renamed
/// keys don't ride along and an emptied file doesn't resurrect them.
#[tauri::command]
async fn write_app_config(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut cfg = config::read_config().map_err(|e| e.to_string())?;
        let obj = cfg
            .as_object_mut()
            .ok_or_else(|| "config.json is not a JSON object".to_string())?;
        // Did `sources` actually change? Compare the on-disk value to the
        // incoming one before we overwrite it.
        let sources_changed = key == "sources" && obj.get("sources") != Some(&value);
        // Same question for the per-CLI activation markers. The tray READS
        // these to disambiguate a standalone provider from a gateway binding
        // that share creds, so a marker change is a tray-visible change — and
        // the Providers page always writes it AFTER the activate/deactivate
        // IPC that rebuilt the menu (`markActive` follows `activate_provider`),
        // so without a rebuild here the tray would keep resolving with the
        // PREVIOUS marker until some unrelated rebuild happened to fire.
        let markers_changed = key == config::ACTIVE_PROVIDER_IDS_KEY
            && obj.get(config::ACTIVE_PROVIDER_IDS_KEY) != Some(&value);
        obj.insert(key, value);
        config::write_config(&cfg).map_err(|e| e.to_string())?;

        // A change to the `sources` key (Settings → Tools toggles) must
        // propagate everywhere scan output flows: re-scan (the filter lives
        // in scan_sessions), push the result to the frontend via the same
        // event the watcher uses, refresh the tray's recent rows, and do a
        // full menu rebuild (per-CLI provider submenus gate on the setting).
        if sources_changed {
            use tauri::Emitter as _;
            match scan_sessions() {
                Ok(result) => {
                    tray::refresh_recent(&app, &result.records);
                    if let Err(err) = app.emit(watcher::SOURCES_CHANGED_EVENT, &result) {
                        log::warn!("sources-toggle emit failed: {err}");
                    }
                }
                Err(err) => log::warn!("sources-toggle rescan failed: {err}"),
            }
            let _ = tray::rebuild_menu(&app);
        }
        if markers_changed {
            let _ = tray::rebuild_menu(&app);
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Take the provider switch the tray deferred to the page (Codex
/// official↔custom, which needs the "follow sessions?" prompt the tray can't
/// show). Returns `None` when nothing is pending; the request is cleared by
/// this call, so the page can poll it on mount AND on the event without
/// running it twice.
#[tauri::command]
async fn take_pending_tray_switch() -> Result<Option<tray::PendingSwitch>, String> {
    Ok(tray::take_pending_switch())
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
async fn write_app_gateways(app: tauri::AppHandle, value: serde_json::Value) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::write_gateways(&value).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    // The tray lists each CLI's gateway BINDINGS alongside its standalone
    // providers, so adding / editing / unbinding a gateway changes the menu
    // exactly like `write_app_providers` does — this is its sibling and needs
    // the same rebuild.
    let _ = tray::rebuild_menu(&app);
    Ok(())
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

// ===================================================================
// Official-account management (save / switch / delete the CLI's own
// OAuth login — see accounts.rs). Phase 1: Codex.
// ===================================================================

/// List the live + saved official accounts for one CLI.
#[tauri::command]
async fn list_accounts(app: String) -> Result<accounts::AccountsState, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        accounts::list_accounts(cli).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Snapshot the CLI's current official login into the store (upsert by account).
#[tauri::command]
async fn save_account(handle: tauri::AppHandle, app: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cli = CliApp::parse(&app).ok_or_else(|| format!("unknown app: {app}"))?;
        accounts::save_current_account(cli).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    // The tray lists the saved logins, so any change to the store must
    // rebuild it — otherwise the menu keeps the pre-change rows.
    let _ = tray::rebuild_menu(&handle);
    Ok(())
}

/// Restore a saved snapshot into the live CLI credential.
/// Validates/refreshes tokens in memory before writing auth.json.
#[tauri::command]
async fn switch_account(handle: tauri::AppHandle, id: String) -> Result<(), String> {
    // Refuse while an add-account flow owns this CLI's credential. The
    // switch would WRITE auth.json, and the flow overwrites it again when
    // it ends — `switch_account(prev_id)` on success, `restore_auth` on
    // cancel — so the user's choice is silently undone after a success
    // toast, having spent a token refresh on the way. The frontend greys
    // the button out; this covers the tray, which has its own row and no
    // idea a login is running.
    //
    // Note the guard belongs HERE and not in `accounts::switch_account`:
    // the login flow calls that to restore the previous account while
    // still holding its slot, so a guard inside would block its own
    // cleanup.
    if let Some(cli) = accounts::account_cli(&id) {
        if accounts::login_in_progress(&handle, cli) {
            return Err("finish or cancel the add-account flow first".into());
        }
    }
    let cli = accounts::switch_account(id)
        .await
        .map_err(|e| e.to_string())?;
    // The cached quota describes the account we just switched AWAY from;
    // keeping it would leave the previous login's usage on the tray row
    // (and, on a failed re-fetch, leave it there indefinitely). The page
    // re-fetches right after this call and that result flows back to the
    // tray via `fetch_subscription_quota` → `tray::refresh_quota`.
    tray::invalidate_quota(&handle, cli);
    // Moves the tray's account checkmark (and can change which provider
    // state reverse-derives, since auth.json is part of it).
    let _ = tray::rebuild_menu(&handle);
    Ok(())
}

/// Delete a saved snapshot. Never touches the live credential.
#[tauri::command]
async fn delete_account(handle: tauri::AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        accounts::delete_account(id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = tray::rebuild_menu(&handle);
    Ok(())
}

/// Spawn `codex login`, wait for the browser login to complete, then
/// auto-save the resulting credential. Auth.json is cleared beforehand so
/// the existing session is not revoked. Returns the new account's store id.
#[tauri::command]
async fn login_and_save_codex_account(
    app: tauri::AppHandle,
    cancel_state: tauri::State<'_, accounts::CodexLoginCancel>,
) -> Result<String, String> {
    let result = accounts::login_and_save_codex_account(app.clone(), &cancel_state).await;
    // Rebuild on BOTH outcomes, before propagating the error. Success adds a
    // row and moves the live login; a failure — cancel, the 5-minute
    // timeout, a save error — has just rolled auth.json back, which the tray
    // must follow too. This sat after a `?`, so the rollback never reached
    // the menu: a cancelled login left the account checkmark missing until
    // the app was restarted.
    let _ = tray::rebuild_menu(&app);
    // Every quota refresh during the flow was skipped by the login guard,
    // and on SUCCESS the live credential may now be a different account:
    // with no previous account to restore (`resnapshot_live_before_login`
    // returns None), auth.json is left holding the NEW login. That write
    // landed while the guard was still up, and nothing writes the file
    // again afterwards — so without this the card would keep showing the
    // pre-login state, and a first-ever account would show none at all
    // with its section hidden, refresh button and all. Cheap when
    // redundant: `force_quota_refresh` has its own burst floor.
    if result.is_ok() {
        tray::force_quota_refresh(&app, CliApp::Codex);
    }
    result
}

#[tauri::command]
async fn cancel_codex_login(
    cancel_state: tauri::State<'_, accounts::CodexLoginCancel>,
) -> Result<(), String> {
    accounts::cancel_codex_login(&cancel_state).await?;
    // Deliberately NO `rebuild_menu` here. This only FIRES the cancel; the
    // rollback happens inside the login flow, after it has stopped the
    // child (an HTTP `/cancel` plus up to 2s waiting for it to exit).
    // Rebuilding now reads the auth.json the flow blanked at the start, so
    // every saved account compares as inactive and the tray loses its
    // checkmark — with nothing to correct it, because the login IPC's own
    // rebuild sat behind a `?` and a cancel returns Err. That flow rebuilds
    // when it finishes, the only point at which the file has settled.
    Ok(())
}

/// Add a Claude account via the headless `claude auth login` (browser
/// roundtrip, fallback URL emitted as `claude:login-url`), then auto-save
/// the resulting credential and restore the previous login — the same flow
/// shape as `login_and_save_codex_account`.
#[tauri::command]
async fn login_and_save_claude_account(
    app: tauri::AppHandle,
    cancel_state: tauri::State<'_, accounts::ClaudeLoginCancel>,
) -> Result<String, String> {
    let result = accounts::login_and_save_claude_account(app.clone(), &cancel_state).await;
    // Rebuild on BOTH outcomes, before propagating the error. Success adds a
    // row and moves the live login; a failure — cancel, the 5-minute
    // timeout, a save error — has just rolled auth.json back, which the tray
    // must follow too. This sat after a `?`, so the rollback never reached
    // the menu: a cancelled login left the account checkmark missing until
    // the app was restarted.
    let _ = tray::rebuild_menu(&app);
    // Every quota refresh during the flow was skipped by the login guard,
    // and on SUCCESS the live credential may now be a different account:
    // with no previous account to restore (`resnapshot_live_before_login`
    // returns None), auth.json is left holding the NEW login. That write
    // landed while the guard was still up, and nothing writes the file
    // again afterwards — so without this the card would keep showing the
    // pre-login state, and a first-ever account would show none at all
    // with its section hidden, refresh button and all. Cheap when
    // redundant: `force_quota_refresh` has its own burst floor.
    if result.is_ok() {
        tray::force_quota_refresh(&app, CliApp::Claude);
    }
    result
}

#[tauri::command]
async fn cancel_claude_login(
    cancel_state: tauri::State<'_, accounts::ClaudeLoginCancel>,
) -> Result<(), String> {
    accounts::cancel_claude_login(&cancel_state).await?;
    // Deliberately NO `rebuild_menu` here. This only FIRES the cancel; the
    // rollback happens inside the login flow, after it has stopped the
    // child (an HTTP `/cancel` plus up to 2s waiting for it to exit).
    // Rebuilding now reads the auth.json the flow blanked at the start, so
    // every saved account compares as inactive and the tray loses its
    // checkmark — with nothing to correct it, because the login IPC's own
    // rebuild sat behind a `?` and a cancel returns Err. That flow rebuilds
    // when it finishes, the only point at which the file has settled.
    Ok(())
}

/// Add a Grok account via the headless `grok login --device-auth` (device
/// flow, verification URL emitted as `grok:login-url` and the user code as
/// `grok:login-code`), then auto-save the resulting credential and restore the
/// previous login — the same flow shape as the codex / claude commands.
#[tauri::command]
async fn login_and_save_grok_account(
    app: tauri::AppHandle,
    cancel_state: tauri::State<'_, accounts::GrokLoginCancel>,
) -> Result<String, String> {
    let result = accounts::login_and_save_grok_account(app.clone(), &cancel_state).await;
    // Rebuild on BOTH outcomes, before propagating the error. Success adds a
    // row and moves the live login; a failure — cancel, the 5-minute
    // timeout, a save error — has just rolled auth.json back, which the tray
    // must follow too. This sat after a `?`, so the rollback never reached
    // the menu: a cancelled login left the account checkmark missing until
    // the app was restarted.
    let _ = tray::rebuild_menu(&app);
    // Every quota refresh during the flow was skipped by the login guard,
    // and on SUCCESS the live credential may now be a different account:
    // with no previous account to restore (`resnapshot_live_before_login`
    // returns None), auth.json is left holding the NEW login. That write
    // landed while the guard was still up, and nothing writes the file
    // again afterwards — so without this the card would keep showing the
    // pre-login state, and a first-ever account would show none at all
    // with its section hidden, refresh button and all. Cheap when
    // redundant: `force_quota_refresh` has its own burst floor.
    if result.is_ok() {
        tray::force_quota_refresh(&app, CliApp::Grok);
    }
    result
}

#[tauri::command]
async fn cancel_grok_login(
    cancel_state: tauri::State<'_, accounts::GrokLoginCancel>,
) -> Result<(), String> {
    accounts::cancel_grok_login(&cancel_state).await?;
    // Deliberately NO `rebuild_menu` here. This only FIRES the cancel; the
    // rollback happens inside the login flow, after it has stopped the
    // child (an HTTP `/cancel` plus up to 2s waiting for it to exit).
    // Rebuilding now reads the auth.json the flow blanked at the start, so
    // every saved account compares as inactive and the tray loses its
    // checkmark — with nothing to correct it, because the login IPC's own
    // rebuild sat behind a `?` and a cancel returns Err. That flow rebuilds
    // when it finishes, the only point at which the file has settled.
    Ok(())
}

#[tauri::command]
fn mark_account_relogin(handle: tauri::AppHandle, id: String, needed: bool) -> Result<(), String> {
    accounts::mark_account_relogin(&id, needed).map_err(|e| e.to_string())?;
    // Drives the ⚠ suffix on the tray's account rows.
    let _ = tray::rebuild_menu(&handle);
    Ok(())
}

pub fn run() {
    // Log target directory: `~/.termory/logs/`. Falls back to the
    // OS-default log location if HOME isn't readable so app launch
    // never depends on logging setup. Mirrors where `config.json` and
    // `providers.json` live so users have one place to look at when
    // reporting a bug.
    let logs_dir = crate::home_dir().map(|h| h.join(".termory").join("logs"));

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
        // MUST be the first registered plugin (per its docs). A second
        // launch (double-clicked exe on Windows — macOS app bundles are
        // single-instance natively) just surfaces the running
        // instance's window and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main_window(app);
        }))
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
            cli_upgrade_commands,
            run_cli_upgrade,
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
            delete_grok_session,
            delete_grok_project,
            delete_codex_memory,
            delete_grok_memory,
            migrate_codex_session,
            migrate_codex_project,
            migrate_gemini_project,
            migrate_opencode_session,
            migrate_opencode_project,
            migrate_grok_session,
            migrate_grok_project,
            delete_opencode_session,
            delete_opencode_project,
            claude_project_registered,
            set_tray_labels,
            detect_cli_versions_cmd,
            detect_latest_versions_cmd,
            detect_codex_installs,
            provider_active_state,
            provider_active_states,
            take_pending_tray_switch,
            activate_provider,
            deactivate_provider,
            delete_provider,
            set_default_provider,
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
            list_accounts,
            save_account,
            switch_account,
            delete_account,
            login_and_save_codex_account,
            cancel_codex_login,
            login_and_save_claude_account,
            cancel_claude_login,
            login_and_save_grok_account,
            cancel_grok_login,
            mark_account_relogin,
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
            app.manage(accounts::CodexLoginCancel(std::sync::Mutex::new(None)));
            app.manage(accounts::ClaudeLoginCancel(std::sync::Mutex::new(None)));
            app.manage(accounts::GrokLoginCancel(std::sync::Mutex::new(None)));
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
            // Keep each CLI's saved entry for the account it is currently
            // logged into in step with that CLI's own credential — token
            // rotation, a re-login run in the terminal, a plan change. The
            // watcher handles the file-backed cases the moment they land;
            // this covers what emits no filesystem event (macOS Keychain).
            accounts::start_auto_sync(app.handle().clone());
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
        // Quitting must not leave a login or an upgrade running behind
        // us. Only MANAGED children are affected — a terminal the user
        // opened is deliberately outside this (see process.rs).
        if let tauri::RunEvent::Exit = event {
            process::shutdown_all();
        }
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
