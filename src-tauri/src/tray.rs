// macOS / Windows / Linux system tray (menu bar on macOS).
//
// One click → menu with a submenu per CLI listing every Provider in
// the user's library, plus "Official" as a sibling row. The currently
// active option is marked with a checkmark, reverse-derived through
// `providers::read_active_state` the same way the Providers page does
// — Termory still stores no "active provider" pointer anywhere.
//
// Menu shape (each CLI row shows its active choice inline):
//
//   Open
//   ─────────────────────
//   Fix the flaky stats test          ← up to 5 most-recent session titles
//   Refactor the gateway editor          (newest first; click opens a terminal
//   …                                     and resumes it in its CLI).
//   ─────────────────────
//   Claude Code · Official  ▸  ☑ Official
//                              ─────────
//                              ☐ Anthropic
//                              ☐ OpenRouter
//   Codex · OpenRouter      ▸  …
//   Gemini · …              ▸  …
//   OpenCode · Official     ▸  …
//   ─────────────────────
//   Exit
//
// Click handler dispatches by id `tray:{app}:official` /
// `tray:{app}:custom:{provider_id}` and calls the same activate /
// deactivate helpers the IPC commands use, so the on-disk write
// path stays single-sourced.

use crate::config;
use crate::providers::{
    activate, deactivate, detect_installed_clis, gateway_providers, read_active_state,
    set_opencode_default, CliApp, Provider,
};
use crate::sessions::{AppSession, ClaudeWorkStatus};
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{
    menu::{
        CheckMenuItemBuilder, IsMenuItem, Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem,
        SubmenuBuilder,
    },
    tray::TrayIconBuilder,
    AppHandle, Manager, Wry,
};

const TRAY_ID: &str = "termory-main";

/// How many recent session titles to surface under "Open", and how
/// many recent projects the "New session" submenu offers.
const RECENT_LIMIT: usize = 5;
const NEW_SESSION_PROJECT_LIMIT: usize = 5;

/// One recent-session row. A click opens a terminal in `project` and
/// resumes the session in its CLI (`source` + `id`).
#[derive(Clone, PartialEq)]
struct RecentSession {
    source: String,
    project: String,
    id: String,
    label: String,
    /// Live work status for a currently-running Claude session (Busy /
    /// Waiting), joined on `id == sessionId`. `None` for non-Claude
    /// rows and for sessions that aren't actively running.
    status: Option<ClaudeWorkStatus>,
}

/// One "New session" group: a recent project dir. The submenu shows
/// its basename as a (disabled) group header with one row per
/// INSTALLED CLI underneath — any CLI can be launched fresh in any
/// recent project, not just the (cwd, CLI) pairs seen in history.
#[derive(Clone, PartialEq)]
struct NewSessionProject {
    project: String,
    label: String,
    /// CLIs seen in this project's history, most recent first — the
    /// submenu lists these before the CLIs never used here, so the
    /// most likely choice is the first row.
    cli_recency: Vec<CliApp>,
}

/// Recent sessions + new-session targets shown under "Open", refreshed
/// from each scan (watcher + the `scan_all_sessions` IPC) — the tray
/// never scans on its own.
#[derive(Clone, PartialEq, Default)]
struct RecentState {
    sessions: Vec<RecentSession>,
    targets: Vec<NewSessionProject>,
}

static RECENT: Mutex<RecentState> = Mutex::new(RecentState {
    sessions: Vec::new(),
    targets: Vec::new(),
});

/// Claude's per-model weekly windows stay app-only — the menu row
/// shows the main session + weekly windows (user decision).
const TRAY_HIDDEN_TIERS: &[&str] = &["seven_day_opus", "seven_day_sonnet"];

/// Cached official-account quota shown on a CLI's first-level row.
/// One entry per quota-capable CLI (`quota::SUPPORTED`). Refreshed by
/// `refresh_quota` from the tray's click-triggered fetch AND from
/// every `fetch_subscription_quota` IPC, so a manual refresh in the
/// Providers page updates the tray too.
#[derive(Clone, PartialEq)]
struct TrayQuota {
    /// `(window id, used %)` — already filtered to displayable
    /// windows (TRAY_HIDDEN_TIERS dropped), API order preserved.
    tiers: Vec<(String, f64)>,
    /// Subscription plan display name ("Max" / "Plus" / "Free" …).
    plan: Option<String>,
}

static QUOTA: Mutex<Vec<(CliApp, TrayQuota)>> = Mutex::new(Vec::new());

/// Handles to the per-CLI first-level submenus, refreshed on every
/// build_menu. A quota change updates the row TITLE in place
/// (`Submenu::set_text` dispatches to the main thread) — a full
/// rebuild (`set_menu`) CLOSES an open menu on macOS, which users hit
/// every time the click-triggered quota fetch landed while the menu
/// was still up.
struct CliRow {
    cli: CliApp,
    submenu: tauri::menu::Submenu<Wry>,
    /// The part before the quota suffix, e.g. "Claude Code · Official".
    base_title: String,
    /// Whether this row carries the quota suffix (CLI supported AND
    /// Official active).
    shows_quota: bool,
}

static CLI_ROWS: Mutex<Vec<CliRow>> = Mutex::new(Vec::new());

/// The dynamic "recent" region (session rows + separators + the "New
/// Session" submenu) starts at this fixed index — right after the
/// always-present "Open" row and its separator.
const REGION_START: usize = 2;

/// Root menu handle + the dynamic region's current item count, so
/// `refresh_recent` can splice the region IN PLACE (`remove_at` +
/// `insert_items`) — a full `set_menu` rebuild CLOSES an open menu on
/// macOS, while in-place splicing updates it live.
static RECENT_REGION: Mutex<Option<(Menu<Wry>, usize)>> = Mutex::new(None);

/// The installed-CLI map the VISIBLE menu reflects. Stored only AFTER
/// a successful `set_menu` (`do_rebuild_menu_with`) / tray `install` —
/// storing earlier would let a failed rebuild claim the new set and
/// permanently defeat the staleness check below. Refresh paths compare
/// a fresh probe against it: a difference means a CLI was
/// installed/removed since the menu was built, so the per-CLI provider
/// submenus (and the New Session rows) are stale — only a full rebuild
/// refreshes those (the in-place splice covers just the recent
/// region). `None` until the first build.
static INSTALLED: Mutex<Option<HashMap<CliApp, bool>>> = Mutex::new(None);

/// Whether the visible menu was built with a DIFFERENT installed set
/// than `current`. Main-thread only (reads the cache the main-thread
/// builds).
fn menu_installed_stale(current: &HashMap<CliApp, bool>) -> bool {
    INSTALLED
        .lock()
        .map(|g| g.as_ref() != Some(current))
        .unwrap_or(false)
}

/// Compare `installed` against what the visible menu was built with and
/// do a full rebuild when they differ. Returns true when the stale path
/// ran (rebuild attempted — on failure INSTALLED stays stale, so the
/// next refresh retries). MUST run on the main thread. The single
/// owner of the "menu reflects the installed set" invariant — both
/// `commit_recent` and `refresh_installed_with` route through here.
fn rebuild_if_installed_stale(app: &AppHandle, installed: &HashMap<CliApp, bool>) -> bool {
    if !menu_installed_stale(installed) {
        return false;
    }
    if let Err(err) = do_rebuild_menu_with(app, installed.clone()) {
        log::error!("tray installed-set rebuild failed: {err}");
    }
    true
}

/// Compose a CLI row title from its base + (when shown) the cached
/// plan/quota suffix: "Claude Code · Official (Max) · 🟢 12% 5h · …".
fn cli_row_title(base: &str, shows_quota: bool, cli: CliApp, labels: &TrayLabels) -> String {
    if !shows_quota {
        return base.to_string();
    }
    let Some(q) = QUOTA
        .lock()
        .ok()
        .and_then(|g| g.iter().find(|(c, _)| *c == cli).map(|(_, q)| q.clone()))
    else {
        return base.to_string();
    };
    let mut title = base.to_string();
    if let Some(plan) = &q.plan {
        title = format!("{title} ({plan})");
    }
    if let Some(label) = quota_label(&q, labels) {
        title = format!("{title} · {label}");
    }
    title
}

/// Localized labels for the menu's static rows (Open / Official / Exit). The
/// frontend pushes the translated strings via the `set_tray_labels` IPC when the
/// app language loads or changes; until then English is used. CLI and provider
/// names are brand / user data and stay untranslated.
#[derive(Clone)]
struct TrayLabels {
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
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            open: "Open".to_string(),
            official: "Official".to_string(),
            exit: "Exit".to_string(),
            five_hour: "5h".to_string(),
            weekly: "Weekly".to_string(),
            monthly: "Monthly".to_string(),
            new_session: "New Session".to_string(),
            choose_folder: "Choose Folder…".to_string(),
            status_busy: "Working".to_string(),
            status_waiting: "Needs input".to_string(),
        }
    }
}

static TRAY_LABELS: Mutex<Option<TrayLabels>> = Mutex::new(None);

fn tray_labels() -> TrayLabels {
    TRAY_LABELS
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// Store the localized static labels (called from the `set_tray_labels` IPC).
/// The caller rebuilds the menu so the new labels take effect.
#[allow(clippy::too_many_arguments)]
pub fn set_labels(
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
) {
    if let Ok(mut g) = TRAY_LABELS.lock() {
        *g = Some(TrayLabels {
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
        });
    }
}

/// Menu-bar glyph: the three-card terminal "chip" from the app icon,
/// pure black on transparent so macOS renders it as a template image
/// and themes it for light / dark menu bars. Embedded so it ships in
/// the binary regardless of bundle layout.
const TRAY_ICON_PNG: &[u8] = include_bytes!("../icons/tray-icon.png");

/// Install the tray icon. Called once from `setup()`.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let installed = detect_installed_clis();
    let menu = build_menu(app, &installed)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("Termory");
    // Prefer the dedicated monochrome menu-bar glyph; fall back to the
    // app window icon if it somehow fails to decode.
    match tauri::image::Image::from_bytes(TRAY_ICON_PNG) {
        Ok(icon) => {
            builder = builder.icon(icon);
            #[cfg(target_os = "macos")]
            {
                builder = builder.icon_as_template(true);
            }
        }
        Err(err) => {
            log::error!("tray icon decode failed, using window icon: {err}");
            if let Some(icon) = app.default_window_icon() {
                builder = builder.icon(icon.clone());
            }
        }
    }
    builder
        .on_menu_event(|app, event| {
            handle_menu_event(app, event.id.as_ref());
        })
        // A click on the tray icon is also "the menu is opening" —
        // kick off a (rate-limited) quota refresh so the quota info
        // rows stay current without any background polling. The fetch
        // lands after the menu is already on screen, so the updated
        // numbers show from the NEXT open (floors: 2 min after a
        // success, 60s after a failure — QUOTA_TRAY_MIN_INTERVAL /
        // QUOTA_TRAY_ERROR_RETRY).
        .on_tray_icon_event(|tray, event| {
            if matches!(event, tauri::tray::TrayIconEvent::Click { .. }) {
                trigger_quota_refresh(tray.app_handle());
                // Also re-check recent-session work status on open, so a
                // crashed session's stale status clears even without a
                // filesystem event (it splices live into the open menu).
                refresh_work_status(tray.app_handle());
            }
        })
        .build(app)?;
    // The tray (and its menu) is live — only now may the cache claim the
    // visible menu reflects `installed` (see the INSTALLED invariant).
    if let Ok(mut g) = INSTALLED.lock() {
        *g = Some(installed);
    }
    Ok(())
}

/// Minimum spacing between quota fetches per CLI — same window as the
/// Providers page's auto-refresh cache (QUOTA_STALE_MS).
const QUOTA_TRAY_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);
/// Retry floor after a FAILED fetch — much shorter, so a transient
/// network error doesn't mute the tray row for the full window
/// (frontend mirror: QUOTA_ERROR_RETRY_MS in ProvidersPage.tsx).
const QUOTA_TRAY_ERROR_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// Per-CLI marker: when the last quota fetch COMPLETED + whether it
/// succeeded (failures earn the shorter retry floor). Updated by
/// `refresh_quota` for EVERY completed fetch — tray-triggered or the
/// Providers page's IPC — so the two paths share one rate limit and
/// can't double-fetch within each other's windows.
static QUOTA_LAST_FETCH: Mutex<Vec<(CliApp, std::time::Instant, bool)>> = Mutex::new(Vec::new());

fn set_quota_marker(cli: CliApp, ok: bool) {
    if let Ok(mut guard) = QUOTA_LAST_FETCH.lock() {
        let now = std::time::Instant::now();
        match guard.iter_mut().find(|(c, _, _)| *c == cli) {
            Some(entry) => {
                entry.1 = now;
                entry.2 = ok;
            }
            None => guard.push((cli, now, ok)),
        }
    }
}

fn spawn_quota_fetch(app: &AppHandle, cli: CliApp) {
    // Mark pre-flight as failed-shape so an in-flight / errored
    // attempt only blocks the short window; the completed fetch
    // overwrites this via refresh_quota's marker update.
    set_quota_marker(cli, false);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let quota = crate::quota::fetch_quota(cli).await;
        refresh_quota(&handle, &quota);
    });
}

/// Async, rate-limited quota fetch + tray update for every CLI in
/// `quota::SUPPORTED`. Used by the menu-open (tray click) hook and the
/// one-shot warm-up at startup.
pub fn trigger_quota_refresh(app: &AppHandle) {
    for &cli in crate::quota::SUPPORTED {
        {
            let Ok(guard) = QUOTA_LAST_FETCH.lock() else {
                continue;
            };
            let now = std::time::Instant::now();
            if let Some((_, prev, ok)) = guard.iter().find(|(c, _, _)| *c == cli) {
                let floor = if *ok {
                    QUOTA_TRAY_MIN_INTERVAL
                } else {
                    QUOTA_TRAY_ERROR_RETRY
                };
                if now.duration_since(*prev) < floor {
                    continue;
                }
            }
        }
        spawn_quota_fetch(app, cli);
    }
}

/// Floor for credential-change-driven refreshes — just burst dedup
/// (Codex rewrites auth.json on every token refresh), NOT a cache
/// window: a login/logout means the cached state is wrong by
/// definition, so the normal floors don't apply.
const QUOTA_FORCE_FLOOR: std::time::Duration = std::time::Duration::from_secs(10);

/// A CLI's credential file just changed (login / logout / token
/// refresh) — re-fetch its quota now, bypassing the regular rate
/// limits. Called from the filesystem watcher.
pub fn force_quota_refresh(app: &AppHandle, cli: CliApp) {
    if !crate::quota::supports_quota(cli) {
        return;
    }
    {
        let Ok(guard) = QUOTA_LAST_FETCH.lock() else {
            return;
        };
        let now = std::time::Instant::now();
        if let Some((_, prev, _)) = guard.iter().find(|(c, _, _)| *c == cli) {
            if now.duration_since(*prev) < QUOTA_FORCE_FLOOR {
                return;
            }
        }
    }
    spawn_quota_fetch(app, cli);
}

/// Rebuild the tray menu so checkmarks reflect the current active
/// state. Called after any IPC command that mutates provider state.
/// Queue a full menu rebuild on the MAIN thread. ALL menu mutations
/// (this, the recent-region splice, the quota title updates) run as
/// queued main-thread tasks, so they are inherently serialized — two
/// concurrent refreshers can't interleave their remove/insert ops or
/// write a stale menu handle back into RECENT_REGION. (A mutex would
/// risk deadlock instead: a worker holding it while its menu ops wait
/// for the main thread, while the main thread waits for the mutex.)
pub fn rebuild_menu(app: &AppHandle) -> tauri::Result<()> {
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(err) = do_rebuild_menu(&handle) {
            log::error!("tray menu rebuild failed: {err}");
        }
    })
}

fn do_rebuild_menu(app: &AppHandle) -> tauri::Result<()> {
    do_rebuild_menu_with(app, detect_installed_clis())
}

/// Full rebuild from an already-probed installed map (callers that just
/// paid for the probe pass it in, so the main thread doesn't re-probe —
/// `detect_installed_clis` can spawn a shell per missing CLI). INSTALLED
/// is stored only after `set_menu` succeeds: on any failure the cache
/// keeps the OLD set, `menu_installed_stale` stays true, and the next
/// refresh retries the rebuild instead of believing a menu that was
/// never shown.
fn do_rebuild_menu_with(app: &AppHandle, installed: HashMap<CliApp, bool>) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let menu = build_menu(app, &installed)?;
        tray.set_menu(Some(menu))?;
        if let Ok(mut g) = INSTALLED.lock() {
            *g = Some(installed);
        }
    }
    Ok(())
}

/// Recompute the recent cache from a fresh scan and rebuild the
/// menu so the entries under "Open" stay current. Reuses the caller's scan
/// (watcher / `scan_all_sessions`); skips the rebuild when nothing changed
/// so active CLI use doesn't churn the menu on every file event.
pub fn refresh_recent(app: &AppHandle, sessions: &[AppSession]) {
    refresh_recent_with(app, sessions, detect_installed_clis());
}

/// `refresh_recent` from an already-probed installed map — the watcher's
/// install branch just ran the probe via `refresh_installed`, so its
/// fall-through rescan hands the result here instead of probing the
/// same PATH (and possibly spawning the same fallback shells) twice in
/// one burst.
pub fn refresh_recent_with(
    app: &AppHandle,
    sessions: &[AppSession],
    installed: HashMap<CliApp, bool>,
) {
    let mut recent = select_recent_state(sessions);
    // Join live Claude work-status onto the recent rows. The FS read
    // stays on the CALLER's thread (watcher / async runtime), like the
    // CLI-install probe below — the queued main-thread task stays lean.
    attach_work_statuses(&mut recent, &crate::sessions::claude_work_statuses());
    // The whole compare→store→splice sequence runs as ONE queued
    // main-thread task: concurrent refreshers (watcher thread + the
    // scan_all_sessions IPC) serialize in queue order, so the RECENT
    // cache and the visible menu can't diverge (a caller-side store
    // with a later queue slot could otherwise leave the menu showing
    // an older state than the cache until the next change).
    //
    // The splice itself updates the dynamic region IN PLACE — a full
    // set_menu rebuild CLOSES an open menu on macOS, and this refresh
    // fires constantly (watcher) while the user may be looking at the
    // menu.
    // Filesystem PATH probing stays on the CALLER's thread (watcher /
    // async runtime) so the queued main-thread task is lean.
    let handle = app.clone();
    let queued = app.run_on_main_thread(move || commit_recent(&handle, &installed, recent));
    if let Err(err) = queued {
        log::error!("tray recent update queue failed: {err}");
    }
}

/// The terminal-launchable subset of an installed map, for the recent
/// region (recent-session resume, New Session) — excludes the Claude
/// Desktop GUI app, which isn't terminal-launchable.
fn terminal_clis(installed: &HashMap<CliApp, bool>) -> Vec<CliApp> {
    CliApp::all()
        .into_iter()
        .filter(|c| c.is_cli() && installed.get(c).copied().unwrap_or(false))
        .collect()
}

/// Store `recent` (when changed) and splice the dynamic region in place,
/// falling back to a full rebuild. When the installed-CLI set differs
/// from what the menu was built with (a CLI was installed/removed), do
/// a FULL rebuild instead — the splice can't refresh the per-CLI
/// provider submenus. MUST run on the main thread (queued like every
/// other menu mutation, so concurrent refreshers serialize).
fn commit_recent(app: &AppHandle, installed: &HashMap<CliApp, bool>, recent: RecentState) {
    let recent_changed = match RECENT.lock() {
        Ok(mut guard) if *guard != recent => {
            *guard = recent.clone();
            true
        }
        Ok(_) => false,
        Err(_) => return, // poisoned → no update
    };
    if rebuild_if_installed_stale(app, installed) {
        return; // build_menu reads the just-stored RECENT — no splice needed
    }
    if !recent_changed {
        return;
    }
    if !update_recent_region(app, &terminal_clis(installed), &recent) {
        if let Err(err) = do_rebuild_menu(app) {
            log::error!("tray recent rebuild failed: {err}");
        }
    }
}

/// A CLI binary dir (or Claude Desktop's config dir) just changed —
/// re-probe the installed set and, when it no longer matches what the
/// menu was built with, do a full rebuild so the new CLI's provider
/// submenu (and its New Session rows) appear without waiting for a
/// provider mutation. Called from the filesystem watcher's
/// install-detection branch; the PATH probe runs on the caller's
/// thread, the compare + rebuild are queued on the main thread.
/// Returns the probed map so the caller can reuse it (the watcher
/// hands it to `refresh_recent_with` for the same burst's rescan).
pub fn refresh_installed(app: &AppHandle) -> HashMap<CliApp, bool> {
    let installed = detect_installed_clis();
    refresh_installed_with(app, installed.clone());
    installed
}

/// Compare an already-probed installed map against the menu and rebuild
/// when stale — the entry point for callers that ran the probe
/// themselves (the `detect_clis` IPC hands its result over so a
/// Providers-page Recheck also refreshes the tray, at no extra probe
/// cost).
pub fn refresh_installed_with(app: &AppHandle, installed: HashMap<CliApp, bool>) {
    let handle = app.clone();
    let queued = app.run_on_main_thread(move || {
        rebuild_if_installed_stale(&handle, &installed);
    });
    if let Err(err) = queued {
        log::error!("tray installed refresh queue failed: {err}");
    }
}

/// Re-evaluate live work status against the CACHED recent list and
/// re-splice if it changed — without a session scan. Called on menu
/// open (tray click) so a CRASHED Claude session's stale "Working"
/// clears promptly: a crash leaves the `<pid>.json` untouched (no
/// filesystem event), so the normal watcher-driven refresh never fires
/// for it; this re-runs the liveness probe on demand. Cheap — only the
/// small `~/.claude/sessions/` dir is re-read (on the caller thread);
/// the cached recent list is re-read + re-attached on the main thread,
/// so the base list is never stale.
pub fn refresh_work_status(app: &AppHandle) {
    let statuses = crate::sessions::claude_work_statuses();
    let installed = detect_installed_clis();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let mut recent = match RECENT.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        attach_work_statuses(&mut recent, &statuses);
        commit_recent(&handle, &installed, recent);
    });
}

/// Replace the dynamic region's items in the existing root menu.
/// Returns false when the splice isn't possible (menu not built yet /
/// any menu-API failure) — the caller falls back to a full rebuild.
fn update_recent_region(app: &AppHandle, installed_clis: &[CliApp], recent: &RecentState) -> bool {
    let Some((menu, old_len)) = RECENT_REGION.lock().ok().and_then(|g| g.clone()) else {
        return false;
    };
    let labels = tray_labels();
    let Ok(region) = build_recent_region(app, &labels, installed_clis, recent) else {
        return false;
    };
    for _ in 0..old_len {
        if menu.remove_at(REGION_START).is_err() {
            return false; // partial state → caller's full rebuild repairs
        }
    }
    let refs: Vec<&dyn IsMenuItem<Wry>> = region.iter().map(|b| b.as_ref()).collect();
    if menu.insert_items(&refs, REGION_START).is_err() {
        return false;
    }
    if let Ok(mut g) = RECENT_REGION.lock() {
        *g = Some((menu, region.len()));
    }
    true
}

/// Record a completed quota fetch (any source) and rebuild the menu
/// when the displayed numbers changed. Failed fetches only refresh the
/// rate-limit marker — the menu keeps the last good numbers instead of
/// flickering empty.
/// Drop a CLI's cached tray quota and refresh its row title (only when
/// something was actually removed). Shared by the two "no longer any
/// quota to show" paths: a definitive logout (`not_found`) and a
/// successful fetch with no usable windows (logged in but unsubscribed).
fn clear_quota_entry(app: &AppHandle, cli: CliApp) {
    let removed = QUOTA
        .lock()
        .map(|mut guard| {
            let before = guard.len();
            guard.retain(|(c, _)| *c != cli);
            guard.len() != before
        })
        .unwrap_or(false);
    if removed {
        update_cli_row_title(app, cli);
    }
}

pub fn refresh_quota(app: &AppHandle, quota: &crate::quota::SubscriptionQuota) {
    let Some(cli) = CliApp::parse(&quota.app) else {
        return;
    };
    if !crate::quota::supports_quota(cli) {
        return;
    }
    // Shared rate limit: an IPC fetch from the Providers page counts
    // exactly like a tray-triggered one.
    set_quota_marker(cli, quota.success);
    // Push every completed result to the frontend so an open Providers
    // page reflects backend-initiated fetches (tray click, watcher
    // credential-change) without its own request. Harmless echo for
    // IPC-initiated fetches — same data the page already received.
    {
        use tauri::Emitter;
        let _ = app.emit(crate::quota::QUOTA_CHANGED_EVENT, quota);
    }
    if !quota.success {
        // Logged out (`not_found`) is a definitive state, not a
        // transient failure — drop the stale numbers from the menu.
        // Other failures keep the last good data (no flickering).
        if quota.credential_status == crate::quota::CredentialStatus::NotFound {
            clear_quota_entry(app, cli);
        }
        return;
    }
    let next = TrayQuota {
        tiers: quota
            .tiers
            .iter()
            .filter(|t| !TRAY_HIDDEN_TIERS.contains(&t.name.as_str()))
            .map(|t| (t.name.clone(), t.utilization))
            .collect(),
        plan: quota.plan.clone(),
    };
    // A successful fetch that carries no usable windows means the
    // official account has no active subscription quota (e.g. logged in
    // but unsubscribed — the usage endpoint returns windows with no
    // `utilization`). Drop any stale numbers instead of leaving them on
    // the menu, same as a definitive logout (`not_found`).
    if next.tiers.is_empty() {
        clear_quota_entry(app, cli);
        return;
    }
    match QUOTA.lock() {
        Ok(mut guard) => match guard.iter_mut().find(|(c, _)| *c == cli) {
            Some((_, cur)) if *cur == next => return, // unchanged → no update
            Some((_, cur)) => *cur = next,
            None => guard.push((cli, next)),
        },
        _ => return,
    }
    update_cli_row_title(app, cli);
}

/// Reflect a quota change in the CLI's first-level row title IN PLACE.
/// A full `rebuild_menu` (`set_menu`) closes an open menu on macOS —
/// and the quota fetch is triggered by OPENING the menu, so it used to
/// slam the menu shut in the user's face whenever the numbers changed.
/// `Submenu::set_text` updates the visible title without dismissing.
/// Falls back to a full rebuild when no row handle exists (menu not
/// built yet / CLI not installed).
fn update_cli_row_title(app: &AppHandle, cli: CliApp) {
    // Queued on the main thread like every other menu mutation (see
    // rebuild_menu) so it serializes with rebuilds/splices.
    let handle = app.clone();
    let queued = app.run_on_main_thread(move || {
        let updated = CLI_ROWS
            .lock()
            .ok()
            .map(|rows| {
                if let Some(row) = rows.iter().find(|r| r.cli == cli) {
                    let labels = tray_labels();
                    let title = cli_row_title(&row.base_title, row.shows_quota, cli, &labels);
                    row.submenu.set_text(title).is_ok()
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if !updated {
            if let Err(err) = do_rebuild_menu(&handle) {
                log::error!("tray quota rebuild failed: {err}");
            }
        }
    });
    if let Err(err) = queued {
        log::error!("tray quota update queue failed: {err}");
    }
}

/// Pressure glyph per window — thresholds from quota.rs (shared with
/// the in-app ring via the quota-utils.ts mirror): <75% green,
/// ≥75% amber, ≥90% red. Emoji because macOS menu text can't be
/// colored — these are the only color carrier a menu title allows.
fn quota_glyph(utilization: f64) -> &'static str {
    if utilization >= crate::quota::CRIT_PCT {
        "🔴"
    } else if utilization >= crate::quota::WARN_PCT {
        "🟡"
    } else {
        "🟢"
    }
}

/// Short menu label for a window id: the localized labels for the
/// standard windows, "{n}h" / "{n}d" for generated `{n}_hour` /
/// `{n}_day` ids (Codex non-standard window lengths, e.g. the free
/// plan's 30-day window — mirrors `tierLabels` in
/// ProviderOfficialCard.tsx), raw id otherwise.
fn tray_tier_label(name: &str, labels: &TrayLabels) -> String {
    match name {
        "five_hour" => labels.five_hour.clone(),
        "seven_day" => labels.weekly.clone(),
        "30_day" => labels.monthly.clone(),
        // Gemini buckets are per-MODEL classes, not time windows —
        // brand names, untranslated by convention.
        "gemini_pro" => "Pro".to_string(),
        "gemini_flash" => "Flash".to_string(),
        "gemini_flash_lite" => "Lite".to_string(),
        other => {
            if let Some(n) = other.strip_suffix("_hour") {
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                    return format!("{n}h");
                }
            }
            if let Some(n) = other.strip_suffix("_day") {
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                    return format!("{n}d");
                }
            }
            other.to_string()
        }
    }
}

/// "🟢 12% 5h · 🟡 78% Weekly" (or "🟢 9% 30d" on a Codex free plan)
/// — appended to the CLI's first-level row title (percent right after
/// the pressure glyph). None when no window is known.
fn quota_label(q: &TrayQuota, labels: &TrayLabels) -> Option<String> {
    if q.tiers.is_empty() {
        return None;
    }
    Some(
        q.tiers
            .iter()
            .map(|(name, used)| {
                format!(
                    "{} {:.0}% {}",
                    quota_glyph(*used),
                    used,
                    tray_tier_label(name, labels)
                )
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

/// The (pure) selection. One shared filtered+sorted candidate list
/// feeds both halves:
///  * `sessions` — the 5 newest sessions, flat (single click → resume);
///  * `targets`  — the first 5 distinct `(source, cwd)` pairs in the
///    same recency order, for the "New session" submenu.
fn select_recent_state(sessions: &[AppSession]) -> RecentState {
    let mut picked: Vec<&AppSession> = sessions
        .iter()
        .filter(|s| !matches!(s.source.as_str(), "Memory" | "Skill"))
        // Drop empty-project placeholders (id == "") — they're sidebar-only.
        .filter(|s| !s.id.is_empty())
        // Drop "sessions" that are really just a CLI slash command
        // (`/model`, `/clear`, …) — they're system commands, not chats.
        .filter(|s| !is_slash_command(label_text(&s.title, &s.snippet)))
        .collect();
    // Newest first. ISO-8601 `updated_at` sorts lexicographically =
    // chronologically; `None` sorts last under descending order.
    picked.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let recent_sessions: Vec<RecentSession> = picked
        .iter()
        .take(RECENT_LIMIT)
        .map(|s| RecentSession {
            source: s.source.clone(),
            project: s.project.clone(),
            id: s.id.clone(),
            label: recent_label(&s.title, &s.snippet),
            status: None,
        })
        .collect();

    let mut targets: Vec<NewSessionProject> = Vec::new();
    for s in &picked {
        // "New session" needs a cwd to land in.
        if s.project.is_empty() {
            continue;
        }
        // Dedup by cwd ACROSS CLIs; keep scanning past the project cap
        // so later (older) sessions still feed cli_recency for the
        // projects already chosen.
        let entry = targets.iter_mut().find(|t| t.project == s.project);
        let entry = match entry {
            Some(e) => e,
            None => {
                if targets.len() >= NEW_SESSION_PROJECT_LIMIT {
                    continue;
                }
                targets.push(NewSessionProject {
                    project: s.project.clone(),
                    label: project_dir_label(&s.project),
                    cli_recency: Vec::new(),
                });
                targets.last_mut().expect("just pushed")
            }
        };
        if let Some(cli) = source_cli(&s.source) {
            if !entry.cli_recency.contains(&cli) {
                entry.cli_recency.push(cli);
            }
        }
    }

    RecentState {
        sessions: recent_sessions,
        targets,
    }
}

/// "New session" group header: the cwd's basename ("termory").
fn project_dir_label(project: &str) -> String {
    let name = std::path::Path::new(project)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(project);
    let name: String = name.chars().take(32).collect();
    if name.is_empty() {
        "(unknown)".to_string()
    } else {
        name
    }
}

/// Inverse of `cli_source`: parse an `AppSession.source` string.
fn source_cli(source: &str) -> Option<CliApp> {
    match source {
        "Claude" => Some(CliApp::Claude),
        "Codex" => Some(CliApp::Codex),
        "Gemini" => Some(CliApp::Gemini),
        "OpenCode" => Some(CliApp::Opencode),
        _ => None,
    }
}

/// `AppSession.source`-style string for a CliApp — what
/// `terminal::new_session` / `resume_session` dispatch on.
fn cli_source(cli: CliApp) -> &'static str {
    match cli {
        CliApp::Claude => "Claude",
        CliApp::Codex => "Codex",
        CliApp::Gemini => "Gemini",
        CliApp::Opencode => "OpenCode",
        // Claude Desktop has no terminal sessions, so this is never used
        // for resume/new dispatch — present only to keep the match total.
        CliApp::ClaudeDesktop => "ClaudeDesktop",
    }
}

/// The text a recent row shows before truncation: title, else snippet, else
/// empty (the caller renders "(untitled)").
fn label_text<'a>(title: &'a str, snippet: &'a str) -> &'a str {
    if !title.trim().is_empty() {
        title.trim()
    } else if !snippet.trim().is_empty() {
        snippet.trim()
    } else {
        ""
    }
}

/// Is `text` a bare CLI slash command (`/model`, `/clear`, `/user:cmd`)? True
/// when the first whitespace token is `/` + a command word (alphanumeric /
/// `-_:`, no inner `/` — so file paths like `/Users/...` are NOT commands).
fn is_slash_command(text: &str) -> bool {
    let Some(first) = text.split_whitespace().next() else {
        return false;
    };
    first.len() > 1
        && first.starts_with('/')
        && first[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

/// Fill each Claude recent row's live work status by matching
/// `id == sessionId`. Non-Claude rows and sessions that aren't running
/// stay `None`. Pure (statuses passed in) so it's unit-testable; the
/// filesystem read happens in `refresh_recent`.
fn attach_work_statuses(
    recent: &mut RecentState,
    statuses: &std::collections::HashMap<String, ClaudeWorkStatus>,
) {
    if statuses.is_empty() {
        return;
    }
    for s in recent.sessions.iter_mut() {
        if s.source == "Claude" {
            s.status = statuses.get(&s.id).copied();
        }
    }
}

/// Localized live work status shown after a recent row's title
/// ("Working" / "Needs input"); `None` for idle / not-running sessions,
/// which get no suffix. Plain text — a native macOS menu is an all-text
/// list, so a colored dot would mean either an emoji (cheap) or a
/// far-left icon column (breaks an otherwise icon-less menu); the word
/// alone stays clean and consistent.
fn work_status_label(status: Option<ClaudeWorkStatus>, labels: &TrayLabels) -> Option<&str> {
    match status {
        Some(ClaudeWorkStatus::Busy) => Some(&labels.status_busy),
        Some(ClaudeWorkStatus::Waiting) => Some(&labels.status_waiting),
        None => None,
    }
}

/// Menu label for a recent session: title, else snippet, else "(untitled)",
/// truncated so the menu stays narrow.
fn recent_label(title: &str, snippet: &str) -> String {
    let raw = label_text(title, snippet);
    let raw = if raw.is_empty() { "(untitled)" } else { raw };
    let mut out: String = raw.chars().take(44).collect();
    if raw.chars().count() > 44 {
        out.push('…');
    }
    out
}

/// Build the dynamic "recent" region — recent-session rows (+ their
/// separator) and the "New Session" submenu (+ its separator) — as
/// free-standing items. Used by build_menu for the initial layout AND
/// by refresh_recent's in-place splice.
fn build_recent_region(
    app: &AppHandle,
    labels: &TrayLabels,
    installed_clis: &[CliApp],
    recent: &RecentState,
) -> tauri::Result<Vec<Box<dyn IsMenuItem<Wry>>>> {
    let mut region: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();

    if !recent.sessions.is_empty() {
        for r in recent.sessions.iter() {
            // Content-addressed id (source + session id, neither
            // contains ':') — a click looks the session up by identity,
            // so a refresh between display and click can't resume the
            // WRONG session; worst case is a silent no-op.
            let id = format!("tray:session:{}:{}", r.source, r.id);
            // Live work status (Claude only) shown after the title:
            // "Fix the test · Working". Idle / not-running rows show
            // just the title.
            let label = match work_status_label(r.status, labels) {
                Some(status) => format!("{} · {}", r.label, status),
                None => r.label.clone(),
            };
            let item = MenuItemBuilder::with_id(id, &label).build(app)?;
            region.push(Box::new(item));
        }
        region.push(Box::new(PredefinedMenuItem::separator(app)?));
    }

    // "New Session": recent project dirs as nested submenus (project
    // ▸ CLI; flat header+rows grew too long) plus a "Choose Folder…"
    // tail (▸ CLI → native dir picker) so a session can start in a
    // BRAND-NEW directory — and the submenu still exists with zero
    // history. Skipped only when no CLI is installed at all.
    if !installed_clis.is_empty() {
        let mut sub = SubmenuBuilder::new(app, &labels.new_session);
        for target in recent.targets.iter() {
            let mut project_sub = SubmenuBuilder::new(app, &target.label);
            // CLIs last used in THIS project first (most likely
            // choice on top), then the rest; installed only.
            let mut clis: Vec<CliApp> = target
                .cli_recency
                .iter()
                .copied()
                .filter(|c| installed_clis.contains(c))
                .collect();
            for cli in installed_clis {
                if !clis.contains(cli) {
                    clis.push(*cli);
                }
            }
            for cli in clis {
                // The full project path rides in the id (after the
                // ':'-free cli key) so the click is decoupled from
                // RECENT entirely — no stale-index hazard.
                let item = MenuItemBuilder::with_id(
                    format!("tray:new:{}:{}", cli_key(cli), target.project),
                    cli_label(cli),
                )
                .build(app)?;
                project_sub = project_sub.item(&item);
            }
            sub = sub.item(&project_sub.build()?);
        }
        if !recent.targets.is_empty() {
            sub = sub.item(&PredefinedMenuItem::separator(app)?);
        }
        let mut pick_sub = SubmenuBuilder::new(app, &labels.choose_folder);
        for cli in installed_clis {
            let item = MenuItemBuilder::with_id(
                format!("tray:newpick:{}", cli_key(*cli)),
                cli_label(*cli),
            )
            .build(app)?;
            pick_sub = pick_sub.item(&item);
        }
        sub = sub.item(&pick_sub.build()?);
        region.push(Box::new(sub.build()?));
        region.push(Box::new(PredefinedMenuItem::separator(app)?));
    }
    Ok(region)
}

fn build_menu(app: &AppHandle, installed: &HashMap<CliApp, bool>) -> tauri::Result<Menu<Wry>> {
    let providers: Vec<Provider> = config::read_providers()
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    let labels = tray_labels();
    let mut menu = MenuBuilder::new(app);

    // "Open" sits at the very top of the menu, the "New session"
    // submenu (recent (cwd, CLI) targets — picking one opens the
    // chosen terminal there and launches the CLI fresh) directly
    // under it, then the recent sessions (newest first; single click
    // resumes in a terminal).
    let open = MenuItemBuilder::with_id("tray:open", &labels.open).build(app)?;
    menu = menu.item(&open).item(&PredefinedMenuItem::separator(app)?);

    // Only surface CLIs actually installed on this machine (`installed` is
    // the caller's `detect_installed_clis()` probe — same one the Providers
    // page uses, so the two agree). The CALLER stores it into INSTALLED
    // after `set_menu` succeeds; build_menu itself never touches the cache
    // (a build that fails or is never shown must not claim the new set).
    //
    // Terminal flows (recent region + New Session) take only real CLIs;
    // the provider-switch submenu loop below uses the full CliApp::all()
    // (including Claude Desktop) with its own per-CLI install gate.
    let installed_clis = terminal_clis(installed);

    let recent = RECENT.lock().map(|g| g.clone()).unwrap_or_default();
    let region = build_recent_region(app, &labels, &installed_clis, &recent)?;
    let region_len = region.len();
    for item in &region {
        menu = menu.item(item.as_ref());
    }

    // Read the gateway bindings ONCE, then group per CLI in the loop.
    // (Uninstalled CLIs get no provider submenu — nothing to switch.)
    let gateways = gateway_providers();

    let mut cli_rows: Vec<CliRow> = Vec::new();
    for cli in CliApp::all() {
        if !installed.get(&cli).copied().unwrap_or(false) {
            continue;
        }
        // The user's standalone providers PLUS this CLI's gateway bindings
        // (synthesized into the same Provider shape), so both appear as
        // switchable choices and the active checkmark lands on whichever is
        // live.
        let mut providers_for_app: Vec<Provider> =
            providers.iter().filter(|p| p.app == cli).cloned().collect();
        providers_for_app.extend(gateways.iter().filter(|p| p.app == cli).cloned());
        // Reverse-derive the active provider id. Anything other than
        // `Some(matching id)` (None, or matched-by-config-but-not-in-list)
        // falls back to "Official is the active row".
        let active_id = read_active_state(cli, &providers_for_app)
            .ok()
            .and_then(|s| s.matched_provider_id);

        // First-level title shows the currently-active choice inline
        // (e.g. "Claude Code · Official"), using the same rule as the
        // checkmarks below: matched provider name, else "Official".
        let active_name = active_id
            .as_deref()
            .and_then(|id| providers_for_app.iter().find(|p| p.id == id))
            .map(|p| p.name.as_str())
            .unwrap_or(labels.official.as_str());
        // Quota-capable CLI with Official active: the title carries
        // the official-account plan + quota suffix. Suppressed while a
        // custom provider is active — the quota belongs to the
        // official login, and gluing it onto a custom provider's name
        // would read as that provider's usage.
        let base_title = format!("{} · {}", cli_label(cli), active_name);
        let shows_quota = crate::quota::supports_quota(cli) && active_id.is_none();
        let title = cli_row_title(&base_title, shows_quota, cli, &labels);
        let mut sub = SubmenuBuilder::new(app, title);

        let official = CheckMenuItemBuilder::with_id(
            format!("tray:{}:official", cli_key(cli)),
            &labels.official,
        )
        .checked(active_id.is_none())
        .build(app)?;
        sub = sub.item(&official);

        if !providers_for_app.is_empty() {
            let sep = PredefinedMenuItem::separator(app)?;
            sub = sub.item(&sep);
        }

        for p in &providers_for_app {
            let is_active = active_id.as_deref() == Some(p.id.as_str());
            let item = CheckMenuItemBuilder::with_id(
                format!("tray:{}:custom:{}", cli_key(cli), p.id),
                &p.name,
            )
            .checked(is_active)
            .build(app)?;
            sub = sub.item(&item);
        }

        let sub = sub.build()?;
        menu = menu.item(&sub);
        cli_rows.push(CliRow {
            cli,
            submenu: sub,
            base_title,
            shows_quota,
        });
    }
    if let Ok(mut rows) = CLI_ROWS.lock() {
        *rows = cli_rows;
    }

    let sep = PredefinedMenuItem::separator(app)?;
    // Plain MenuItem (not PredefinedMenuItem::quit) so macOS doesn't
    // attach the native quit-item icon — keeps the menu icon-free.
    let quit = MenuItemBuilder::with_id("tray:quit", &labels.exit).build(app)?;
    menu = menu.item(&sep).item(&quit);

    let menu = menu.build()?;
    // Record the root handle + region length so refresh_recent can
    // splice in place.
    if let Ok(mut g) = RECENT_REGION.lock() {
        *g = Some((menu.clone(), region_len));
    }
    Ok(menu)
}

/// Hide the window + (macOS) the Dock icon — the tray-only end state
/// used both when the user closes the window and on `--autostart`
/// login launches. Inverse of `show_main_window`.
pub(crate) fn hide_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_dock_visibility(false);
}

pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        // A window is going on screen → restore the Dock icon.
        #[cfg(target_os = "macos")]
        let _ = app.set_dock_visibility(true);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn handle_menu_event(app: &AppHandle, id: &str) {
    if id == "tray:open" {
        show_main_window(app);
        return;
    }
    if id == "tray:quit" {
        app.exit(0);
        return;
    }
    // A recent-session row → open a fresh OS terminal in that session's
    // project and resume it in its CLI (`claude --resume <id>` etc.).
    // Looked up by (source, session id) — see build_menu on why not by
    // index. Fire-and-forget from the tray; errors logged.
    if let Some(rest) = id.strip_prefix("tray:session:") {
        let mut parts = rest.splitn(2, ':');
        if let (Some(source), Some(sid)) = (parts.next(), parts.next()) {
            if let Some(r) = RECENT.lock().ok().and_then(|g| {
                g.sessions
                    .iter()
                    .find(|r| r.source == source && r.id == sid)
                    .cloned()
            }) {
                let _ = crate::terminal::resume_session(&r.source, &r.id, Some(&r.project));
            }
        }
        return;
    }
    // "Choose Folder…" (`tray:newpick:{cli}`) → native directory
    // picker, then launch that CLI fresh in the picked dir. The picker
    // callback is async — fire-and-forget, errors logged.
    if let Some(cli) = id.strip_prefix("tray:newpick:").and_then(CliApp::parse) {
        use tauri_plugin_dialog::DialogExt;
        app.dialog().file().pick_folder(move |folder| {
            let Some(dir) = folder.and_then(|f| f.into_path().ok()) else {
                return; // cancelled
            };
            let _ = crate::terminal::new_session(cli_source(cli), Some(&dir.to_string_lossy()));
        });
        return;
    }
    // A "New session" row (`tray:new:{cli}:{project path}`) → open the
    // chosen terminal in that dir and launch that CLI fresh. The path
    // is carried in the id verbatim (it may itself contain ':'), so
    // the click is independent of RECENT's current state. NOTE: the
    // `tray:newpick:` check above must stay FIRST — it shares this
    // prefix.
    if let Some(rest) = id.strip_prefix("tray:new:") {
        let mut parts = rest.splitn(2, ':');
        if let (Some(cli), Some(project)) = (parts.next().and_then(CliApp::parse), parts.next()) {
            let _ = crate::terminal::new_session(cli_source(cli), Some(project));
        }
        return;
    }
    let Some(rest) = id.strip_prefix("tray:") else {
        return;
    };
    let mut parts = rest.splitn(3, ':');
    let (Some(app_key), Some(kind)) = (parts.next(), parts.next()) else {
        return;
    };
    let provider_id = parts.next();
    let Some(cli) = CliApp::parse(app_key) else {
        return;
    };

    let providers_value = config::read_providers().unwrap_or_default();
    let providers: Vec<Provider> = serde_json::from_value(providers_value).unwrap_or_default();
    // Standalone providers + this CLI's gateway bindings, so a click on a
    // gateway row resolves to its synthesized provider and activates via the
    // same path.
    let mut providers_for_app: Vec<Provider> =
        providers.iter().filter(|p| p.app == cli).cloned().collect();
    providers_for_app.extend(gateway_providers().into_iter().filter(|p| p.app == cli));

    let result = match (kind, provider_id) {
        ("official", _) => deactivate(cli, &providers_for_app),
        ("custom", Some(pid)) => match providers_for_app.iter().find(|p| p.id == pid) {
            Some(p) => {
                // For OpenCode, `activate` only adds the provider's slot
                // to opencode.json — it does NOT make it the startup
                // default, which is what the checkmark / inline title
                // track. So also promote it to default (mirrors the
                // Providers page flow). Single-slot CLIs (Claude / Codex
                // / Gemini) need only `activate`, which writes their live
                // config directly.
                let activated = activate(p, &providers_for_app);
                if activated.is_ok() && cli == CliApp::Opencode {
                    set_opencode_default(p)
                } else {
                    activated
                }
            }
            None => return,
        },
        _ => return,
    };

    if let Err(err) = result {
        log::error!("tray activation failed for {app_key}: {err}");
    }

    if let Err(err) = rebuild_menu(app) {
        log::error!("tray menu rebuild failed: {err}");
    }

    // Tell any open Providers page to re-derive active state from disk.
    use tauri::Emitter;
    let _ = app.emit("termory:providers-changed", ());
}

/// CLI display names — kept in sync with the Providers page tabs
/// (`CLI_APP_LABEL` in src/constants.ts) so the tray and the UI use
/// identical source names.
fn cli_label(cli: CliApp) -> &'static str {
    match cli {
        CliApp::Claude => "Claude Code",
        CliApp::Codex => "Codex",
        CliApp::Gemini => "Gemini",
        CliApp::Opencode => "OpenCode",
        CliApp::ClaudeDesktop => "Claude Desktop",
    }
}

fn cli_key(cli: CliApp) -> &'static str {
    match cli {
        CliApp::Claude => "claude",
        CliApp::Codex => "codex",
        CliApp::Gemini => "gemini",
        CliApp::Opencode => "opencode",
        CliApp::ClaudeDesktop => "claude-desktop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_label_formats_known_generated_and_missing_windows() {
        let labels = TrayLabels::default();
        let both = TrayQuota {
            tiers: vec![("five_hour".into(), 12.4), ("seven_day".into(), 78.0)],
            plan: None,
        };
        assert_eq!(
            quota_label(&both, &labels).as_deref(),
            Some("🟢 12% 5h · 🟡 78% Weekly")
        );
        // Codex free plan: a single generated 30-day window → "30d".
        let monthly = TrayQuota {
            tiers: vec![("30_day".into(), 9.0)],
            plan: None,
        };
        assert_eq!(
            quota_label(&monthly, &labels).as_deref(),
            Some("🟢 9% Monthly")
        );
        // Truly unknown ids pass through raw.
        let odd = TrayQuota {
            tiers: vec![("mystery_window".into(), 99.6)],
            plan: None,
        };
        assert_eq!(
            quota_label(&odd, &labels).as_deref(),
            Some("🔴 100% mystery_window")
        );
        let none = TrayQuota {
            tiers: vec![],
            plan: None,
        };
        assert_eq!(quota_label(&none, &labels), None);
    }

    #[test]
    fn tray_tier_label_humanizes_generated_ids() {
        let labels = TrayLabels::default();
        assert_eq!(tray_tier_label("five_hour", &labels), "5h");
        assert_eq!(tray_tier_label("seven_day", &labels), "Weekly");
        assert_eq!(tray_tier_label("3_hour", &labels), "3h");
        assert_eq!(tray_tier_label("30_day", &labels), "Monthly");
        assert_eq!(tray_tier_label("14_day", &labels), "14d");
        assert_eq!(tray_tier_label("gemini_pro", &labels), "Pro");
        assert_eq!(tray_tier_label("gemini_flash_lite", &labels), "Lite");
        assert_eq!(tray_tier_label("_day", &labels), "_day"); // no digits → raw
        assert_eq!(tray_tier_label("weekly_limit", &labels), "weekly_limit");
    }

    #[test]
    fn quota_glyph_thresholds_match_the_app_ring() {
        assert_eq!(quota_glyph(0.0), "🟢");
        assert_eq!(quota_glyph(74.9), "🟢");
        assert_eq!(quota_glyph(75.0), "🟡");
        assert_eq!(quota_glyph(89.9), "🟡");
        assert_eq!(quota_glyph(90.0), "🔴");
        assert_eq!(quota_glyph(100.0), "🔴");
    }

    #[test]
    fn recent_label_fallbacks_and_truncation() {
        assert_eq!(recent_label("Fix bug", "snip"), "Fix bug");
        // Blank title falls back to the snippet (trimmed).
        assert_eq!(recent_label("   ", "  the snippet  "), "the snippet");
        // Both blank → placeholder.
        assert_eq!(recent_label("", ""), "(untitled)");
        // Over-long titles truncate to 44 chars + an ellipsis.
        let out = recent_label(&"x".repeat(60), "");
        assert_eq!(out.chars().count(), 45);
        assert!(out.ends_with('…'));
    }

    fn sess(source: &str, id: &str, updated: &str) -> AppSession {
        AppSession {
            source: source.into(),
            id: id.into(),
            project: "/p".into(),
            updated_at: Some(updated.into()),
            ..Default::default()
        }
    }

    fn sess_in(source: &str, project: &str, id: &str, updated: &str) -> AppSession {
        AppSession {
            project: project.into(),
            ..sess(source, id, updated)
        }
    }

    #[test]
    fn select_recent_state_sessions_flat_newest_first() {
        let sessions = vec![
            sess_in("Claude", "/work/termory", "c-old", "2026-01-01T00:00:00Z"),
            sess_in("Codex", "/work/chats", "x1", "2026-06-01T00:00:00Z"),
            sess_in("Claude", "/work/termory", "c-new", "2026-07-01T00:00:00Z"),
            sess("Memory", "m1", "2026-12-01T00:00:00Z"), // excluded
            sess("Skill", "s1", "2026-12-01T00:00:00Z"),  // excluded
            sess_in("Gemini", "/work/termory", "g1", "2026-03-01T00:00:00Z"),
        ];
        let state = select_recent_state(&sessions);
        let ids: Vec<&str> = state.sessions.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["c-new", "x1", "g1", "c-old"]);
    }

    #[test]
    fn select_recent_state_targets_dedup_by_cwd_across_clis() {
        let sessions = vec![
            sess_in("Claude", "/work/termory", "c-old", "2026-01-01T00:00:00Z"),
            sess_in("Codex", "/work/chats", "x1", "2026-06-01T00:00:00Z"),
            sess_in("Claude", "/work/termory", "c-new", "2026-07-01T00:00:00Z"),
            // Same cwd under another CLI merges into one group — the
            // submenu lists every installed CLI per group anyway.
            sess_in("Gemini", "/work/termory", "g1", "2026-03-01T00:00:00Z"),
        ];
        let state = select_recent_state(&sessions);
        let labels: Vec<&str> = state.targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["termory", "chats"]);
        // CLI recency per project, newest first: termory's latest
        // session is Claude, then Gemini (the older Claude session
        // doesn't duplicate the entry).
        assert_eq!(
            state.targets[0].cli_recency,
            vec![CliApp::Claude, CliApp::Gemini]
        );
        assert_eq!(state.targets[1].cli_recency, vec![CliApp::Codex]);
    }

    #[test]
    fn select_recent_state_targets_skip_records_without_a_cwd() {
        let mut no_cwd = sess("Claude", "c1", "2026-06-01T00:00:00Z");
        no_cwd.project = "".into();
        let state = select_recent_state(&[no_cwd]);
        // Still resumable as a flat session (resume falls back to no cd)…
        assert_eq!(state.sessions.len(), 1);
        // …but not offered as a New-session target.
        assert!(state.targets.is_empty());
    }

    #[test]
    fn select_recent_state_drops_slash_command_sessions() {
        let mut cmd = sess("Claude", "cmd1", "2026-12-01T00:00:00Z");
        cmd.title = "/clear".into(); // newest, but a bare command → dropped
        let mut chat = sess("Codex", "chat1", "2026-06-01T00:00:00Z");
        chat.title = "Fix the flaky test".into();
        let state = select_recent_state(&vec![cmd, chat]);
        let ids: Vec<&str> = state.sessions.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["chat1"]);
    }

    #[test]
    fn select_recent_state_caps_sessions_and_targets() {
        let mut sessions: Vec<AppSession> = Vec::new();
        for p in 0..7 {
            for i in 0..3 {
                sessions.push(sess_in(
                    "Claude",
                    &format!("/work/p{p}"),
                    &format!("p{p}-s{i}"),
                    &format!("2026-0{}-{:02}T00:00:00Z", 7 - p, 28 - i),
                ));
            }
        }
        let state = select_recent_state(&sessions);
        assert_eq!(state.sessions.len(), RECENT_LIMIT);
        assert_eq!(state.targets.len(), NEW_SESSION_PROJECT_LIMIT);
        assert_eq!(state.targets[0].label, "p0");
    }

    #[test]
    fn attach_work_statuses_joins_claude_rows_by_id_only() {
        let sessions = vec![
            sess_in("Claude", "/work/a", "busy-id", "2026-07-01T00:00:00Z"),
            sess_in("Claude", "/work/b", "wait-id", "2026-06-01T00:00:00Z"),
            sess_in("Claude", "/work/c", "idle-id", "2026-05-01T00:00:00Z"),
            sess_in("Codex", "/work/d", "busy-id", "2026-04-01T00:00:00Z"), // same id, wrong source
        ];
        let mut state = select_recent_state(&sessions);
        let mut statuses = std::collections::HashMap::new();
        statuses.insert("busy-id".to_string(), ClaudeWorkStatus::Busy);
        statuses.insert("wait-id".to_string(), ClaudeWorkStatus::Waiting);
        attach_work_statuses(&mut state, &statuses);

        let by_id = |id: &str| state.sessions.iter().find(|r| r.id == id).unwrap().status;
        assert_eq!(by_id("busy-id"), Some(ClaudeWorkStatus::Busy));
        assert_eq!(by_id("wait-id"), Some(ClaudeWorkStatus::Waiting));
        assert_eq!(by_id("idle-id"), None); // not in the map
                                            // Codex row sharing the id must NOT pick up Claude's status.
        let codex = state.sessions.iter().find(|r| r.source == "Codex").unwrap();
        assert_eq!(codex.status, None);
    }

    #[test]
    fn work_status_label_maps_states() {
        let labels = TrayLabels::default();
        assert_eq!(
            work_status_label(Some(ClaudeWorkStatus::Busy), &labels),
            Some("Working")
        );
        assert_eq!(
            work_status_label(Some(ClaudeWorkStatus::Waiting), &labels),
            Some("Needs input")
        );
        assert_eq!(work_status_label(None, &labels), None);
    }

    #[test]
    fn project_dir_label_uses_basename() {
        assert_eq!(project_dir_label("/Users/x/work/termory"), "termory");
        assert_eq!(project_dir_label("/work/chats"), "chats");
        // Pathological root cwd falls back to the raw path.
        assert_eq!(project_dir_label("/"), "/");
    }

    #[test]
    fn cli_source_matches_session_source_strings() {
        assert_eq!(cli_source(CliApp::Claude), "Claude");
        assert_eq!(cli_source(CliApp::Opencode), "OpenCode");
    }

    #[test]
    fn is_slash_command_detects_bare_commands_not_paths() {
        assert!(is_slash_command("/model"));
        assert!(is_slash_command("/clear"));
        assert!(is_slash_command("/user:cmd"));
        assert!(is_slash_command("/compact some args")); // still a command invocation
                                                         // Real prompts / paths are NOT commands.
        assert!(!is_slash_command("fix the bug"));
        assert!(!is_slash_command("/Users/john/project")); // inner "/" → path
        assert!(!is_slash_command(""));
        assert!(!is_slash_command("/"));
    }
}
