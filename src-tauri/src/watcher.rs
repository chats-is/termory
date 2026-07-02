//! Filesystem watcher — keeps Termory's session / memory / skill view
//! in sync with on-disk changes so the user never has to click
//! "refresh". When a watched path changes, we coalesce a burst of
//! events into a single re-scan and emit the result to the frontend
//! via a Tauri event.
//!
//! Two watch tiers:
//!   * Static — each platform's top-level config dir under HOME
//!     (`~/.codex/`, `~/.claude/`, …). Set up once at startup.
//!   * Dynamic — project cwds discovered from session metadata, plus
//!     their git-root ancestors. Reconfigured after every scan (both
//!     watcher-triggered and `scan_all_sessions` IPC). Lets us catch
//!     edits to `<cwd>/CLAUDE.md`, `<cwd>/.claude/skills/...`, etc.
//!     without recursively watching every cwd the user might be in.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter, Manager};

use crate::sessions::scan_sessions;

/// Event name fired at the frontend after a successful re-scan.
/// Payload is `Vec<AppSession>` (same shape as `scan_all_sessions`).
pub const SOURCES_CHANGED_EVENT: &str = "termory:sources-changed";

/// Event name fired when something in a CLI install dir changes —
/// binary appearing (install) or disappearing (uninstall). Payload is
/// empty `()`; the frontend re-runs `detect_clis` / `detect_cli_versions`
/// to read the current state. Replaces polling for install detection.
pub const CLI_INSTALL_CHANGED_EVENT: &str = "termory:cli-install-changed";

/// Coalesce changes that arrive within this window before triggering
/// a re-scan. Many editors / DB engines emit a flurry of intermediate
/// events on save (temp file → rename → mtime touch + WAL writes for
/// SQLite); 600ms collects the burst without making the UI feel laggy,
/// and trims re-scan frequency during continuous CLI activity.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(600);

/// After we've finished a re-scan, drain any events that arrive within
/// this settle window. Reading the SQLite databases (Codex's
/// `state_5.sqlite`, OpenCode's `opencode.db`) touches the `-wal` and
/// `-shm` sidecar files even on a pure read, which the watcher sees as
/// new modifications. Without this drain we'd immediately re-trigger
/// ourselves and loop indefinitely.
const SETTLE_WINDOW: Duration = Duration::from_millis(300);

struct WatcherInner {
    watcher: RecommendedWatcher,
    /// Project cwds we're currently dynamically watching. Diffed
    /// against the new set on every reconfigure so we only add/remove
    /// the delta — avoids tearing down and rebuilding the whole tree.
    dynamic_paths: HashSet<PathBuf>,
}

/// Handle to the running watcher. Cheap to clone (Arc).
#[derive(Clone)]
pub struct WatcherHandle {
    inner: Arc<Mutex<WatcherInner>>,
}

impl WatcherHandle {
    /// Update the set of dynamically-watched project cwds. Paths in
    /// `new_paths` that aren't already watched get added; paths that
    /// disappeared from `new_paths` get removed. Paths that overlap a
    /// static target (e.g. someone has `~/.codex/foo` as a session
    /// project — vanishingly rare) are skipped to avoid double events.
    pub fn reconfigure_dynamic(&self, new_paths: HashSet<PathBuf>) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            // Worker thread panicked while holding the lock; recover
            // the inner state so we can still mutate the watcher.
            Err(p) => p.into_inner(),
        };

        // Remove watches no longer present.
        let to_remove: Vec<PathBuf> = inner
            .dynamic_paths
            .iter()
            .filter(|p| !new_paths.contains(*p))
            .cloned()
            .collect();
        for path in &to_remove {
            let _ = inner.watcher.unwatch(path);
        }

        // Add new watches. Skip non-existent paths (project was deleted)
        // and paths already covered by a static target.
        let static_targets = watch_targets();
        let to_add: Vec<PathBuf> = new_paths
            .iter()
            .filter(|p| !inner.dynamic_paths.contains(*p))
            .cloned()
            .collect();
        for path in &to_add {
            if !path.exists() {
                continue;
            }
            if static_targets.iter().any(|t| path.starts_with(t)) {
                continue;
            }
            if let Err(err) = inner.watcher.watch(path, RecursiveMode::Recursive) {
                log::warn!("watcher skip dynamic {path:?}: {err}");
            }
        }

        inner.dynamic_paths = new_paths;
    }
}

/// Compute the project-cwd set Termory should be dynamically watching,
/// from a freshly-scanned `Vec<AppSession>`.
pub fn dynamic_paths_from_sessions<S: AsRef<str>>(
    project_paths: impl IntoIterator<Item = S>,
) -> HashSet<PathBuf> {
    project_paths
        .into_iter()
        .filter_map(|p| {
            let s = p.as_ref();
            if s.is_empty() {
                return None;
            }
            let path = PathBuf::from(s);
            if path.is_absolute() {
                Some(path)
            } else {
                None
            }
        })
        .collect()
}

/// Start the filesystem watcher in a background thread. Returns the
/// handle once static watches are registered; the event loop runs
/// forever in the spawned thread.
pub fn start(app_handle: AppHandle) -> notify::Result<WatcherHandle> {
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();

    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        // Send may fail if the receiver thread has died; that's fine,
        // we'll silently stop forwarding.
        let _ = tx.send(res);
    })?;

    for path in watch_targets() {
        if !path.exists() {
            continue;
        }
        // Per-path failures are non-fatal — partial coverage beats no
        // coverage. A user might not have every CLI installed.
        if let Err(err) = watcher.watch(&path, RecursiveMode::Recursive) {
            log::warn!("watcher skip session target {path:?}: {err}");
        }
    }

    // Install-detection watches: known CLI binary dirs + node version
    // manager roots. Replaces 3s frontend polling — events here fire
    // the `cli-install-changed` event so the Providers page can
    // refresh just the install status (no session re-scan needed).
    let install_targets = install_watch_targets();
    for (path, mode) in &install_targets {
        if !path.exists() {
            continue;
        }
        if let Err(err) = watcher.watch(path, *mode) {
            log::warn!("watcher skip install target {path:?}: {err}");
        }
    }

    // Claude Desktop install detection: its marker is the config dir
    // itself (created on the app's first run), which can't be watched
    // before it exists — so watch the PARENT dir non-recursively and
    // name-filter events to `Claude*` direct children in the routing
    // (`event_touches_claude_desktop`). These parents (`~/Library/
    // Application Support` / `%LOCALAPPDATA%`) are busy shared dirs, so
    // they deliberately do NOT go through the generic
    // `event_touches_install` path-prefix match.
    let claude_desktop_parents = crate::claude_desktop::install_watch_parents();
    for path in &claude_desktop_parents {
        if !path.exists() {
            continue;
        }
        if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
            log::warn!("watcher skip claude-desktop target {path:?}: {err}");
        }
    }

    let inner = Arc::new(Mutex::new(WatcherInner {
        watcher,
        dynamic_paths: HashSet::new(),
    }));
    let inner_for_thread = inner.clone();

    thread::spawn(move || {
        loop {
            // Block until the first event of a burst arrives.
            let mut events: Vec<notify::Event> = Vec::new();
            match rx.recv() {
                Ok(Ok(event)) => events.push(event),
                Ok(Err(_)) => {}  // watcher-level error, ignore
                Err(_) => return, // channel closed → shutdown
            }
            // Then drain everything that lands within the debounce
            // window. Once we hit Timeout we know the burst is done.
            let deadline = Instant::now() + DEBOUNCE_WINDOW;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(Ok(event)) => events.push(event),
                    Ok(Err(_)) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }

            // Install-detection routing: if any event touched a CLI
            // binary dir or shell rc file, fire the install-changed
            // event so the frontend re-runs detect_clis. Independent
            // of the session rescan below — these paths usually don't
            // overlap with session storage.
            let installed_probe = if events.iter().any(|e| {
                event_touches_install(e, &install_targets)
                    || event_touches_claude_desktop(e, &claude_desktop_parents)
            }) {
                if let Err(err) = app_handle.emit(CLI_INSTALL_CHANGED_EVENT, ()) {
                    log::warn!("watcher install-changed emit failed: {err}");
                }
                // The tray gates its per-CLI submenus on the installed
                // set too — re-probe and rebuild it when the set
                // changed, so a fresh install shows up without waiting
                // for a provider mutation. (The probe runs here on the
                // watcher thread; only the compare + rebuild queue on
                // the main thread.) Keep the probed map: the rescan
                // below hands it to `refresh_recent_with` so the same
                // burst doesn't probe PATH twice.
                Some(crate::tray::refresh_installed(&app_handle))
            } else {
                None
            };

            // Credential routing: a change to a CLI's OAuth credential
            // file means a login / logout / token refresh just happened
            // — force-refresh that CLI's quota (bypasses the normal
            // rate limits; tray + Providers page update via
            // QUOTA_CHANGED_EVENT). Keychain-backed credentials (macOS
            // Claude / Codex variants) produce no file event — the
            // 60s not_found retry remains the fallback there.
            {
                let mut credential_clis: Vec<crate::providers::CliApp> =
                    events.iter().flat_map(event_credential_clis).collect();
                credential_clis.dedup();
                for cli in credential_clis {
                    crate::tray::force_quota_refresh(&app_handle, cli);
                }
            }

            // If every event in the burst touched only noise files
            // (SQLite WAL/SHM, OS metadata), there's nothing to re-scan
            // for. Skip without rescanning — otherwise we'd churn on
            // database internals after every read. Direct children of
            // the Claude Desktop watch parents are excluded too: they
            // are install signals only (handled above), and without the
            // exclusion every other app's activity in ~/Library/
            // Application Support / %LOCALAPPDATA% would trigger a full
            // session rescan.
            if !events
                .iter()
                .any(|e| event_has_relevant_path(e, &claude_desktop_parents))
            {
                continue;
            }

            match scan_sessions() {
                Ok(result) => {
                    // Reconfigure dynamic watches based on the project
                    // cwds discovered in this scan. Sessions that have
                    // been opened in new projects pick up coverage;
                    // disappeared projects get unwatched.
                    let new_cwds = dynamic_paths_from_sessions(
                        result.projects.iter().map(|p| p.project.as_str()),
                    );
                    let handle = WatcherHandle {
                        inner: inner_for_thread.clone(),
                    };
                    handle.reconfigure_dynamic(new_cwds);

                    // Keep the tray's "recent sessions" list current — the
                    // tray is the always-visible surface, so this runs
                    // unconditionally. Reuse the install branch's probe
                    // when it ran (one PATH probe per burst, not two).
                    match installed_probe {
                        Some(installed) => crate::tray::refresh_recent_with(
                            &app_handle,
                            &result.records,
                            installed,
                        ),
                        None => crate::tray::refresh_recent(&app_handle, &result.records),
                    }

                    // Skip the frontend emit when the main window is hidden
                    // (close-to-tray): nobody's looking, so serializing the
                    // full ScanResult + re-rendering is wasted. The frontend
                    // re-scans on window focus (App.tsx) when shown again, and
                    // the next emit once visible is the fallback. Default to
                    // emitting when visibility can't be determined.
                    let window_visible = app_handle
                        .get_webview_window("main")
                        .and_then(|w| w.is_visible().ok())
                        .unwrap_or(true);
                    if window_visible {
                        if let Err(err) = app_handle.emit(SOURCES_CHANGED_EVENT, result) {
                            log::warn!("watcher sources-changed emit failed: {err}");
                        }
                    }
                }
                Err(err) => {
                    log::warn!("watcher rescan failed: {err}");
                }
            }

            // Drain self-induced events so they don't immediately
            // trigger another rescan. The SQLite reads we just did
            // touch `-wal` / `-shm`; FSEvents reports those back to us.
            let settle_until = Instant::now() + SETTLE_WINDOW;
            loop {
                let now = Instant::now();
                if now >= settle_until {
                    break;
                }
                match rx.recv_timeout(settle_until - now) {
                    Ok(_) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });

    Ok(WatcherHandle { inner })
}

/// True if `event` touches at least one path that would actually
/// affect our scan output. Filters out SQLite's `-wal` / `-shm` /
/// `-journal` sidecars (they churn on every read, including our own)
/// and OS metadata noise (`.DS_Store`). If only filtered files
/// changed, the data we'd surface is identical to last scan, so a
/// re-scan would be pure cost.
/// CLIs whose OAuth credential file this event touched. The
/// path→CLI mapping is owned by quota.rs (`credential_cli_for_path`,
/// next to the credential readers) so the watcher can't drift from
/// the actual credential locations.
fn event_credential_clis(event: &notify::Event) -> Vec<crate::providers::CliApp> {
    event
        .paths
        .iter()
        .filter_map(|p| crate::quota::credential_cli_for_path(p))
        .collect()
}

fn event_has_relevant_path(event: &notify::Event, ignore_children_of: &[PathBuf]) -> bool {
    event.paths.iter().any(|path| {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        if name.ends_with("-wal")
            || name.ends_with("-shm")
            || name.ends_with("-journal")
            || name == ".DS_Store"
        {
            return false;
        }
        // The Claude Desktop watch parents (and their direct children)
        // carry no session data — those events exist only to feed the
        // install-detection branch, never a rescan.
        if ignore_children_of
            .iter()
            .any(|p| path == p || path.parent() == Some(p.as_path()))
        {
            return false;
        }
        true
    })
}

/// The list of top-level directories we watch statically. Each is the
/// canonical root for one platform's records:
///   * `~/.codex/` — sessions DB, memories, skills, AGENTS.md
///   * `$CLAUDE_CONFIG_DIR` or `~/.claude/` — projects, rules, skills,
///     global CLAUDE.md
///   * `~/.gemini/` — chats / memory / skills under tmp/
///   * `~/.config/opencode/` — AGENTS.md, skills
///   * `~/.local/share/opencode/` — sqlite DB, storage compat layout
///   * `~/.agents/` — tool-neutral global skills
///
/// Dynamic watches (project cwds derived from session metadata) are
/// layered on top via `WatcherHandle::reconfigure_dynamic`.
fn watch_targets() -> Vec<PathBuf> {
    let Some(home) = crate::home_dir() else {
        return Vec::new();
    };

    vec![
        crate::providers::codex_root(&home),
        crate::sessions::claude_config_root(&home),
        home.join(".gemini"),
        crate::sessions::opencode_config_dir(&home),
        crate::sessions::opencode_data_dir(&home),
        home.join(".agents"),
    ]
}

/// Dirs to watch for CLI binary install/uninstall events.
///
/// Each entry is `(path, mode)`:
///   * `NonRecursive` for leaf bin dirs (`~/.opencode/bin`,
///     `/opt/homebrew/bin`, …) — we only care about direct children
///     (the binary itself appearing/disappearing).
///   * `Recursive` for node-version-manager roots
///     (`~/.nvm/versions/node`, fnm, mise) — each child is a version
///     with its own bin/ subdir where the CLI gets installed.
///
/// Non-existent paths are silently skipped at registration time.
/// Mirrors the install-side of `cli_search_paths` in `providers.rs`.
fn install_watch_targets() -> Vec<(PathBuf, RecursiveMode)> {
    let Some(home) = crate::home_dir() else {
        return Vec::new();
    };
    let mut targets: Vec<(PathBuf, RecursiveMode)> = Vec::new();

    // Cross-platform per-user bin dirs (these resolve correctly on
    // both Unix and Windows via `home.join()` — e.g. `.bun/bin` becomes
    // `%USERPROFILE%\.bun\bin` on Windows, where bun does install).
    for sub in [
        ".opencode/bin",
        ".bun/bin",
        ".cargo/bin",
        ".npm-global/bin",
        ".codex/bin",
    ] {
        targets.push((home.join(sub), RecursiveMode::NonRecursive));
    }

    // Unix-only per-user dirs (XDG, n, Unix Volta layout).
    #[cfg(unix)]
    {
        for sub in [".local/bin", "n/bin", ".volta/bin"] {
            targets.push((home.join(sub), RecursiveMode::NonRecursive));
        }
    }

    // pnpm — different default per platform.
    #[cfg(target_os = "macos")]
    targets.push((home.join("Library/pnpm"), RecursiveMode::NonRecursive));
    #[cfg(target_os = "linux")]
    targets.push((home.join(".local/share/pnpm"), RecursiveMode::NonRecursive));

    #[cfg(target_os = "macos")]
    {
        targets.push((
            PathBuf::from("/opt/homebrew/bin"),
            RecursiveMode::NonRecursive,
        ));
        targets.push((PathBuf::from("/usr/local/bin"), RecursiveMode::NonRecursive));
    }
    #[cfg(target_os = "linux")]
    {
        targets.push((PathBuf::from("/usr/local/bin"), RecursiveMode::NonRecursive));
    }
    #[cfg(target_os = "windows")]
    {
        // npm global default: %APPDATA%\npm
        if let Some(appdata) = dirs::data_dir() {
            targets.push((appdata.join("npm"), RecursiveMode::NonRecursive));
        }
        // Node.js MSI installer
        targets.push((
            PathBuf::from("C:\\Program Files\\nodejs"),
            RecursiveMode::NonRecursive,
        ));
        // Scoop shims (opencode README's recommended install method)
        targets.push((
            home.join("scoop").join("shims"),
            RecursiveMode::NonRecursive,
        ));
        // Chocolatey bin (opencode README's other recommended method)
        targets.push((
            PathBuf::from("C:\\ProgramData\\chocolatey\\bin"),
            RecursiveMode::NonRecursive,
        ));
        // Volta on Windows: %LOCALAPPDATA%\Volta\bin
        if let Some(localdata) = dirs::data_local_dir() {
            targets.push((
                localdata.join("Volta").join("bin"),
                RecursiveMode::NonRecursive,
            ));
            // pnpm on Windows: %LOCALAPPDATA%\pnpm
            targets.push((localdata.join("pnpm"), RecursiveMode::NonRecursive));
            // fnm on Windows: %LOCALAPPDATA%\fnm\node-versions
            targets.push((
                localdata.join("fnm").join("node-versions"),
                RecursiveMode::Recursive,
            ));
        }
        // NVM-Windows: $NVM_HOME or C:\nvm; recursive because each
        // version is a sibling dir holding node.exe + npm.
        let nvm_home = std::env::var_os("NVM_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\nvm"));
        targets.push((nvm_home, RecursiveMode::Recursive));
    }

    // Unix node version managers — recursive enumeration of per-version bin dirs.
    #[cfg(unix)]
    {
        targets.push((home.join(".nvm/versions/node"), RecursiveMode::Recursive));
        targets.push((
            home.join(".local/state/fnm_multishells"),
            RecursiveMode::Recursive,
        ));
        targets.push((
            home.join(".local/share/mise/installs/node"),
            RecursiveMode::Recursive,
        ));
    }

    targets
}

/// True if `event` touches any path under one of the install-watch
/// targets. We don't filter by file extension or name — any change
/// inside a known bin dir is grounds to re-detect (binary added,
/// removed, replaced, or even mtime-touched by a package manager).
fn event_touches_install(event: &notify::Event, targets: &[(PathBuf, RecursiveMode)]) -> bool {
    event
        .paths
        .iter()
        .any(|path| targets.iter().any(|(target, _)| path.starts_with(target)))
}

/// True if `event` marks Claude Desktop's config dir appearing or
/// disappearing: a DIRECT `Claude*` child of one of the watched parent
/// dirs (`claude_desktop::install_watch_parents`). Unlike the bin-dir
/// targets this is name-filtered — the parents are busy shared dirs
/// (`~/Library/Application Support` / `%LOCALAPPDATA%`), and a bare
/// path-prefix match would re-run install detection (PATH probes,
/// possible shell spawns) on every other app's activity there.
fn event_touches_claude_desktop(event: &notify::Event, parents: &[PathBuf]) -> bool {
    event.paths.iter().any(|path| {
        parents.iter().any(|parent| {
            path.parent() == Some(parent.as_path())
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    // The `Claude*` marker rule is owned by claude_desktop
                    // (shared with its Windows dir resolution) so it can't
                    // drift from what `is_installed` actually checks.
                    .is_some_and(crate::claude_desktop::is_install_marker_name)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_paths_keeps_absolute_dedups_and_drops_empty_or_relative() {
        // Build absolute paths from temp_dir so the test holds on
        // Windows too ("/abs/one" isn't absolute there — no drive).
        let one = std::env::temp_dir().join("one");
        let two = std::env::temp_dir().join("two");
        let one_s = one.to_string_lossy().into_owned();
        let two_s = two.to_string_lossy().into_owned();
        let paths = vec![
            one_s.as_str(),
            two_s.as_str(),
            one_s.as_str(),  // duplicate → deduped by the HashSet
            "",              // empty → dropped
            "relative/path", // not absolute → dropped
        ];
        let result = dynamic_paths_from_sessions(paths);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&one));
        assert!(result.contains(&two));
        assert!(!result.contains(&PathBuf::from("relative/path")));
    }

    #[test]
    fn event_has_relevant_path_ignores_claude_desktop_parent_children() {
        fn ev(paths: &[&str]) -> notify::Event {
            let mut e = notify::Event::default();
            e.paths = paths.iter().map(PathBuf::from).collect();
            e
        }
        let parents = vec![PathBuf::from("/Users/x/Library/Application Support")];
        // Unrelated apps' direct children under the busy shared parent
        // must NOT trigger a session rescan (they're install-signal
        // territory only).
        assert!(!event_has_relevant_path(
            &ev(&["/Users/x/Library/Application Support/Slack"]),
            &parents
        ));
        // Neither the Claude marker itself (install branch handles it)…
        assert!(!event_has_relevant_path(
            &ev(&["/Users/x/Library/Application Support/Claude"]),
            &parents
        ));
        // …nor an event on the watched parent dir itself.
        assert!(!event_has_relevant_path(
            &ev(&["/Users/x/Library/Application Support"]),
            &parents
        ));
        // Real session data stays relevant.
        assert!(event_has_relevant_path(
            &ev(&["/Users/x/.claude/projects/p/s.jsonl"]),
            &parents
        ));
        // Noise names stay filtered regardless.
        assert!(!event_has_relevant_path(
            &ev(&["/Users/x/.codex/state_5.sqlite-wal"]),
            &parents
        ));
    }

    #[test]
    fn event_touches_claude_desktop_matches_direct_claude_children_only() {
        fn ev(paths: &[&str]) -> notify::Event {
            let mut e = notify::Event::default();
            e.paths = paths.iter().map(PathBuf::from).collect();
            e
        }
        let parents = vec![PathBuf::from("/Users/x/Library/Application Support")];
        // The config dir appearing / disappearing is the install marker.
        assert!(event_touches_claude_desktop(
            &ev(&["/Users/x/Library/Application Support/Claude"]),
            &parents
        ));
        assert!(event_touches_claude_desktop(
            &ev(&["/Users/x/Library/Application Support/Claude-3p"]),
            &parents
        ));
        // Other apps' dirs in the same busy parent must NOT re-trigger
        // install detection.
        assert!(!event_touches_claude_desktop(
            &ev(&["/Users/x/Library/Application Support/Slack"]),
            &parents
        ));
        // Deep events inside the Claude dir aren't the marker (and the
        // non-recursive watch filters them out anyway).
        assert!(!event_touches_claude_desktop(
            &ev(&["/Users/x/Library/Application Support/Claude/claude_desktop_config.json"]),
            &parents
        ));
        // Unrelated roots never match.
        assert!(!event_touches_claude_desktop(
            &ev(&["/Users/x/Claude"]),
            &parents
        ));
    }

    #[test]
    fn event_credential_clis_matches_quota_cli_credential_files_only() {
        use crate::providers::CliApp;
        fn ev(paths: &[&str]) -> notify::Event {
            let mut e = notify::Event::default();
            e.paths = paths.iter().map(PathBuf::from).collect();
            e
        }
        assert_eq!(
            event_credential_clis(&ev(&["/Users/x/.claude/.credentials.json"])),
            vec![CliApp::Claude]
        );
        assert_eq!(
            event_credential_clis(&ev(&["/Users/x/.codex/auth.json"])),
            vec![CliApp::Codex]
        );
        assert_eq!(
            event_credential_clis(&ev(&["/Users/x/.gemini/oauth_creds.json"])),
            vec![CliApp::Gemini]
        );
        // OpenCode's unrelated auth.json must NOT match (parent isn't `.codex`).
        assert!(
            event_credential_clis(&ev(&["/Users/x/.local/share/opencode/auth.json"])).is_empty()
        );
        // Ordinary session files don't match.
        assert!(event_credential_clis(&ev(&["/Users/x/.claude/projects/p/s.jsonl"])).is_empty());

        // A relocated CODEX_HOME puts auth.json outside any `.codex` dir;
        // the CODEX_HOME env var must be consulted so the watcher can still
        // trigger a quota refresh when that file changes.
        let _g = crate::testutils::lock_home();
        let _e = crate::testutils::EnvVarGuard::set("CODEX_HOME", "/custom/cdx");
        assert_eq!(
            event_credential_clis(&ev(&["/custom/cdx/auth.json"])),
            vec![CliApp::Codex]
        );
    }
}
