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
use tauri::{AppHandle, Emitter};

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
/// SQLite); 500ms collects the burst without making the UI feel laggy.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

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
            if events
                .iter()
                .any(|e| event_touches_install(e, &install_targets))
            {
                if let Err(err) = app_handle.emit(CLI_INSTALL_CHANGED_EVENT, ()) {
                    log::warn!("watcher install-changed emit failed: {err}");
                }
            }

            // If every event in the burst touched only noise files
            // (SQLite WAL/SHM, OS metadata), there's nothing to re-scan
            // for. Skip without rescanning — otherwise we'd churn on
            // database internals after every read.
            if !events.iter().any(event_has_relevant_path) {
                continue;
            }

            match scan_sessions() {
                Ok(sessions) => {
                    // Reconfigure dynamic watches based on the project
                    // cwds discovered in this scan. Sessions that have
                    // been opened in new projects pick up coverage;
                    // disappeared projects get unwatched.
                    let new_cwds =
                        dynamic_paths_from_sessions(sessions.iter().map(|s| s.project.as_str()));
                    let handle = WatcherHandle {
                        inner: inner_for_thread.clone(),
                    };
                    handle.reconfigure_dynamic(new_cwds);

                    // Keep the tray's "recent sessions" list current.
                    crate::tray::refresh_recent(&app_handle, &sessions);

                    if let Err(err) = app_handle.emit(SOURCES_CHANGED_EVENT, sessions) {
                        log::warn!("watcher sources-changed emit failed: {err}");
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
fn event_has_relevant_path(event: &notify::Event) -> bool {
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
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let claude_config = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));

    vec![
        home.join(".codex"),
        claude_config,
        home.join(".gemini"),
        home.join(".config").join("opencode"),
        home.join(".local").join("share").join("opencode"),
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
    let Some(home) = dirs::home_dir() else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_paths_keeps_absolute_dedups_and_drops_empty_or_relative() {
        let paths = vec![
            "/abs/one",
            "/abs/two",
            "/abs/one",      // duplicate → deduped by the HashSet
            "",              // empty → dropped
            "relative/path", // not absolute → dropped
        ];
        let result = dynamic_paths_from_sessions(paths);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&PathBuf::from("/abs/one")));
        assert!(result.contains(&PathBuf::from("/abs/two")));
        assert!(!result.contains(&PathBuf::from("relative/path")));
    }
}
