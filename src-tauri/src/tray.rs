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
    activate, deactivate, detect_install_snapshot, gateway_providers, read_active_state,
    set_default, CliApp, InstallSnapshot, Provider, ProviderKind,
};
use crate::sessions::{AppSession, ClaudeWorkStatus};
use std::collections::HashMap;
use std::error::Error;
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

/// LEGACY per-model weekly windows (the flat `seven_day_opus` /
/// `seven_day_sonnet` ids) stay app-only. The API has since moved
/// model-scoped limits into the `limits` array, named by
/// `scope.model.display_name` (e.g. "Fable") — those DO show on the
/// tray (brand name, falls through `tray_tier_label` verbatim), so
/// this hide-set only drops the legacy ids if they ever return
/// non-null again.
const TRAY_HIDDEN_TIERS: &[&str] = &["seven_day_opus", "seven_day_sonnet"];

/// Cached official-account quota shown on a CLI's first-level row.
/// One entry per quota-capable CLI (`quota::SUPPORTED`). Refreshed by
/// `refresh_quota` from the tray's click-triggered fetch AND from
/// every `fetch_subscription_quota` IPC, so a manual refresh in the
/// Providers page updates the tray too.
#[derive(Clone, PartialEq)]
struct TrayQuota {
    /// Displayable windows (TRAY_HIDDEN_TIERS dropped), API order
    /// preserved.
    tiers: Vec<TrayTier>,
    /// Subscription plan display name ("Max" / "Plus" / "Free" …).
    plan: Option<String>,
    /// Pay-as-you-go credits (Claude "extra usage" / grok on-demand),
    /// present only when enabled — appended as "🟢 $used / $limit Credits".
    credits: Option<TrayCredits>,
}

/// One window on the tray row. Keeps the API's own fields rather than
/// a pre-rendered label: the label is composed at RENDER time, so a
/// language switch relabels the cached quota instead of freezing the
/// locale it was fetched under.
#[derive(Clone, PartialEq)]
struct TrayTier {
    /// Window id, or the model display name for a model-scoped window.
    name: String,
    /// Period a model-scoped window groups under — see
    /// [`crate::quota::QuotaTier::group`].
    group: Option<String>,
    used: f64,
}

/// The pieces of an enabled `ExtraUsage` the tray row renders: a
/// pressure glyph off `utilization` + the `$used / $limit` amounts.
#[derive(Clone, PartialEq)]
struct TrayCredits {
    utilization: f64,
    used: f64,
    limit: Option<f64>,
    currency: Option<String>,
    decimal_places: Option<u32>,
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

/// A saved-login row's live handle, kept so an account switch can move the
/// checkmark IN PLACE. A full `set_menu` closes an open menu on macOS, and the
/// switch lands seconds late (it refreshes tokens over the network first) —
/// which is exactly when the user has reopened the menu to see whether it
/// worked. Rebuilding there would shut the menu in their face on every switch.
struct AccountRow {
    cli: CliApp,
    id: String,
    item: tauri::menu::CheckMenuItem<Wry>,
}

static ACCOUNT_ROWS: Mutex<Vec<AccountRow>> = Mutex::new(Vec::new());

/// A provider switch the tray started but deliberately did NOT perform,
/// because it needs the Providers page's prompt (currently only Codex's
/// official↔custom switch, which asks which projects' sessions should follow).
/// The page picks it up via the `take_pending_tray_switch` IPC — on the
/// `termory:tray-switch-request` event, and again on mount so a request made
/// while the frontend was still loading isn't lost.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingSwitch {
    /// CliApp key ("codex").
    pub app: String,
    /// The provider id to switch to, or `None` for Official.
    pub provider_id: Option<String>,
}

static PENDING_SWITCH: Mutex<Option<PendingSwitch>> = Mutex::new(None);

/// Emitted when a switch is waiting for the page — see [`PendingSwitch`].
/// Mirrored in src/constants.ts.
pub const TRAY_SWITCH_REQUEST_EVENT: &str = "termory:tray-switch-request";

/// Hand the pending switch to the prompt window, clearing it (take-once).
pub fn take_pending_switch() -> Option<PendingSwitch> {
    PENDING_SWITCH.lock().ok().and_then(|mut s| s.take())
}

/// The dynamic "recent" region (session rows + separators + the "New
/// Session" submenu) starts at this fixed index — right after the
/// always-present "Open" row and its separator.
const REGION_START: usize = 2;

/// Root menu handle + the dynamic region's current item count, so
/// `refresh_recent` can splice the region IN PLACE (`remove_at` +
/// `insert_items`) — a full `set_menu` rebuild CLOSES an open menu on
/// macOS, while in-place splicing updates it live.
static RECENT_REGION: Mutex<Option<(Menu<Wry>, usize)>> = Mutex::new(None);

/// The install snapshot the VISIBLE menu reflects (per-app installed
/// map + the codex-terminal bool — see `providers::InstallSnapshot`).
/// Stored only AFTER a successful `set_menu` (`do_rebuild_menu_with`) /
/// tray `install` — storing earlier would let a failed rebuild claim
/// the new set and permanently defeat the staleness check below.
/// Refresh paths compare a fresh probe against it: a difference means
/// a CLI was installed/removed (or codex's terminal capability
/// flipped, e.g. the standalone CLI was installed while the desktop
/// app was already present) since the menu was built, so the per-CLI
/// provider submenus / New Session rows are stale — only a full
/// rebuild refreshes those (the in-place splice covers just the recent
/// region). `None` until the first build.
static INSTALLED: Mutex<Option<InstallSnapshot>> = Mutex::new(None);

/// Whether the visible menu was built with a DIFFERENT install
/// snapshot than `current`. Main-thread only (reads the cache the
/// main-thread builds).
fn menu_installed_stale(current: &InstallSnapshot) -> bool {
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
fn rebuild_if_installed_stale(app: &AppHandle, installed: &InstallSnapshot) -> bool {
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
///
/// Deserialized straight from the IPC payload as ONE object: passing a dozen
/// positional `String`s meant any reordering silently mislabelled the menu, and
/// a struct makes each label name-matched instead.
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayLabels {
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
    credits: String,
    /// Stand-in for a provider saved without a name — mirrors the Providers
    /// page's `p.name || t("providers.unnamed")`, so a nameless row is a
    /// readable placeholder here instead of a blank, unclickable-looking line.
    unnamed: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            open: "Open".to_string(),
            official: "Official".to_string(),
            exit: "Exit".to_string(),
            five_hour: "5h".to_string(),
            weekly: "W".to_string(),
            monthly: "M".to_string(),
            new_session: "New Session".to_string(),
            choose_folder: "Choose Folder…".to_string(),
            status_busy: "Working".to_string(),
            status_waiting: "Needs input".to_string(),
            credits: "Credits".to_string(),
            unnamed: "(unnamed)".to_string(),
        }
    }
}

impl TrayLabels {
    /// A provider's menu label: its name, else the localized placeholder —
    /// the Providers page's `p.name || t("providers.unnamed")`.
    fn provider_name<'a>(&'a self, p: &'a Provider) -> &'a str {
        let name = p.name.trim();
        if name.is_empty() {
            &self.unnamed
        } else {
            name
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
pub fn set_labels(labels: TrayLabels) {
    if let Ok(mut g) = TRAY_LABELS.lock() {
        *g = Some(labels);
    }
}

/// Menu-bar glyph: the three-card terminal "chip" from the app icon,
/// pure black on transparent so macOS renders it as a template image
/// and themes it for light / dark menu bars. Embedded so it ships in
/// the binary regardless of bundle layout.
const TRAY_ICON_PNG: &[u8] = include_bytes!("../icons/tray-icon.png");

/// Install the tray icon. Called once from `setup()`.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let installed = detect_install_snapshot();
    let menu = build_menu(app, &installed)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("Termory");
    // Windows/Linux convention: LEFT click opens the app window, the
    // menu lives on RIGHT click (handled below). macOS keeps the
    // menu-bar behavior where left click IS the menu.
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.show_menu_on_left_click(false);
    }
    // macOS menu bar: the dedicated MONOCHROME glyph, marked as a template
    // so macOS themes it for light / dark bars. Fall back to the window icon
    // if it somehow fails to decode.
    #[cfg(target_os = "macos")]
    match tauri::image::Image::from_bytes(TRAY_ICON_PNG) {
        Ok(icon) => {
            builder = builder.icon(icon).icon_as_template(true);
        }
        Err(err) => {
            log::error!("tray icon decode failed, using window icon: {err}");
            if let Some(icon) = app.default_window_icon() {
                builder = builder.icon(icon.clone());
            }
        }
    }
    // Windows / Linux system tray: the COLORED app icon (like most apps
    // there). The macOS monochrome template would render as a black blob —
    // those trays don't do menu-bar theming. Fall back to the embedded
    // glyph only if the window icon is somehow unavailable.
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        } else if let Ok(icon) = tauri::image::Image::from_bytes(TRAY_ICON_PNG) {
            builder = builder.icon(icon);
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
            if let tauri::tray::TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                trigger_quota_refresh(tray.app_handle());
                // Also re-check recent-session work status on open, so a
                // crashed session's stale status clears even without a
                // filesystem event (it splices live into the open menu).
                refresh_work_status(tray.app_handle());
                // Windows/Linux: LEFT click (release) opens the app
                // window — the menu is on right click, see
                // show_menu_on_left_click(false) above. (Linux
                // appindicator trays don't deliver click events at
                // all, so in practice this is the Windows path.)
                #[cfg(not(target_os = "macos"))]
                if button == tauri::tray::MouseButton::Left
                    && button_state == tauri::tray::MouseButtonState::Up
                {
                    show_main_window(tray.app_handle());
                }
                #[cfg(target_os = "macos")]
                let _ = (button, button_state);
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

/// A CLI row's title before the quota suffix: the in-use choice inline, e.g.
/// "Claude Code · Official" / "Codex · OpenRouter". An UNMANAGED config names
/// neither — the CLI points somewhere Termory doesn't know — so the row carries
/// no suffix at all rather than claiming a choice the user didn't make here.
fn cli_base_title(
    cli: CliApp,
    set: &CliProviders,
    active: &ActiveChoice,
    labels: &TrayLabels,
) -> String {
    let name = active
        .id
        .as_deref()
        .and_then(|id| set.row(id))
        .map(|r| labels.provider_name(&r.provider))
        .or(active.official.then_some(labels.official.as_str()));
    match name {
        Some(name) => format!("{} · {}", cli_label(cli), name),
        None => cli_label(cli).to_string(),
    }
}

/// Would `cli`'s row title differ from what the visible menu shows? Used to
/// decide whether an in-place refresh is enough or the menu must be rebuilt.
///
/// An account switch REWRITES auth.json, which Codex's active-state derivation
/// reads (`auth_mode` / `OPENAI_API_KEY` live there) — so restoring a snapshot
/// taken while a custom provider was active moves the Official/provider
/// checkmarks too, not just the account row. Reads config from disk, so keep it
/// off the main thread.
fn cli_row_title_is_stale(cli: CliApp) -> bool {
    let providers =
        crate::providers::providers_from_json(config::read_providers().unwrap_or_default());
    let set = CliProviders::resolve(cli, &providers, &gateway_providers());
    let active = set.active_choice(cli, &config::active_provider_markers());
    let fresh = cli_base_title(cli, &set, &active, &tray_labels());
    CLI_ROWS
        .lock()
        .ok()
        .map(|rows| {
            rows.iter()
                .find(|r| r.cli == cli)
                // No cached row for this CLI: nothing to update in place.
                .is_none_or(|r| r.base_title != fresh)
        })
        .unwrap_or(true)
}

/// A saved login's menu label: the display label, plus a ⚠ when its refresh
/// token was revoked. Shared by the build and the in-place refresh so the two
/// can't render the same account differently.
fn account_row_label(a: &crate::accounts::TrayAccount) -> String {
    if a.needs_relogin {
        format!("{} ⚠", a.label)
    } else {
        a.label.clone()
    }
}

/// Push the current saved-login state onto the cached row handles. Returns
/// false when the cache can't express it — no rows, or the SET of accounts
/// changed (added / removed / reordered) — leaving the caller to rebuild.
fn apply_account_rows(rows: &[AccountRow]) -> bool {
    if rows.is_empty() {
        return false;
    }
    let mut clis: Vec<CliApp> = Vec::new();
    for row in rows {
        if !clis.contains(&row.cli) {
            clis.push(row.cli);
        }
    }
    for cli in clis {
        let live = crate::accounts::tray_accounts(cli);
        let cached: Vec<&AccountRow> = rows.iter().filter(|r| r.cli == cli).collect();
        if live.len() != cached.len() || live.iter().zip(&cached).any(|(a, r)| a.id != r.id) {
            return false;
        }
        for (a, row) in live.iter().zip(&cached) {
            if row.item.set_text(account_row_label(a)).is_err()
                || row.item.set_checked(a.active).is_err()
                || row.item.set_enabled(!a.needs_relogin).is_err()
            {
                return false;
            }
        }
    }
    true
}

/// Move the account checkmark (and the ⚠ / disabled state) WITHOUT rebuilding.
/// `set_menu` closes an open menu on macOS, and an account switch completes
/// seconds after the click — reopening the menu to check the result is the
/// normal next action, so a rebuild there would dismiss it every time. Same
/// technique the quota suffix uses (`update_cli_row_title`). Falls back to a
/// full rebuild when the account SET changed, which the handles can't express.
fn refresh_accounts(app: &AppHandle) {
    let handle = app.clone();
    let queued = app.run_on_main_thread(move || {
        let updated = ACCOUNT_ROWS
            .lock()
            .ok()
            .map(|rows| apply_account_rows(&rows))
            .unwrap_or(false);
        if !updated {
            if let Err(err) = do_rebuild_menu(&handle) {
                log::error!("tray account rebuild failed: {err}");
            }
        }
    });
    if let Err(err) = queued {
        log::error!("tray account refresh could not reach the main thread: {err}");
    }
}

/// Reflect a landed account switch in the menu, preferring the in-place update
/// so a menu the user reopened to check the result isn't dismissed.
///
/// An account switch no longer moves the row's title on its own: it rewrites
/// auth.json, which the provider derivation also reads, but
/// `accounts::switch_codex` carries the provider-owned fields over from the
/// live file (see `accounts::PROVIDER_OWNED_AUTH_FIELDS`), so the derived
/// provider — and hence the title — is unchanged. This check therefore almost
/// always takes the in-place branch now. It is kept as the guard for the case
/// it cannot rule out: something else moved the provider between the menu
/// being built and the switch landing, where only a rebuild can express the
/// change and showing the truth beats keeping the menu open.
fn settle_account_change(app: &AppHandle, cli: CliApp) {
    if cli_row_title_is_stale(cli) {
        if let Err(err) = rebuild_menu(app) {
            log::error!("tray menu rebuild after account switch failed: {err}");
        }
    } else {
        refresh_accounts(app);
    }
}

/// Restore a saved official login (Codex / Claude) from the tray. Async
/// because a switch can do network work before writing (Codex refreshes the
/// tokens first) — a failure leaves the live credential untouched, and for
/// Codex we then mirror the Providers page by flagging the entry as needing
/// re-login (its refresh token was revoked), which renders as the ⚠ suffix on
/// the next build. Claude ALSO validates (switch_claude refreshes first) but
/// flags needsRelogin BACKEND-side on AuthFailure only — a string Err here
/// can't tell a dead token from a locked-Keychain write error, and flagging
/// a write error would misdiagnose, so only Codex flags from this side.
///
/// On success the quota belongs to a DIFFERENT account, so force a refetch:
/// that also emits `quota-changed`, which an open Providers page already
/// listens to for reloading its account list — no extra event needed.
fn spawn_account_switch(app: &AppHandle, cli: CliApp, id: String) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Switching OVERWRITES the live credential, so a live login that was
        // never snapshotted would be gone for good. The Providers page guards
        // this by warning in its confirm dialog ("the current login isn't
        // saved") — a native menu row has no dialog to warn in, so snapshot it
        // instead: the account stays recoverable either way, which is what the
        // warning is for. Idempotent, and the same call the `codex login` flow
        // makes.
        if let Err(err) = crate::accounts::auto_save_unsaved_live_account(cli) {
            log::warn!("tray account switch: snapshotting the live login failed: {err}");
        }
        match crate::accounts::switch_account(id.clone()).await {
            Ok(()) => {
                let _ = crate::accounts::mark_account_relogin(&id, false);
                settle_account_change(&handle, cli);
                force_quota_refresh(&handle, cli);
            }
            Err(err) => {
                log::error!("tray account switch failed for {id}: {err}");
                if cli == CliApp::Codex {
                    let _ = crate::accounts::mark_account_relogin(&id, true);
                }
                // The failure only flags the entry — the live credential was
                // left untouched, so nothing but the account row can have moved.
                refresh_accounts(&handle);
            }
        }
    });
}

/// Codex-only, and ONLY when the user turned on Settings → "follow all
/// projects silently" (`config::codex_keep_all_sessions`): after a bucket-CHANGING
/// switch, re-tag EVERY project holding off-target sessions so `codex resume`
/// still lists them. Without that setting this never runs — the switch is handed
/// to the page's `CodexFollowDialog` instead, which asks first.
///
/// Both buckets are fixed (Official is always `openai`, a Termory-written custom
/// provider is always `termory`), so only official↔custom changes anything.
///
/// Runs on a blocking worker: it opens sqlite and rewrites rollout JSONL files
/// that routinely run 100+ MB — never on the menu-event (main) thread.
///
/// The caller has already established that the bucket changes; `to_official`
/// names the side just switched TO.
fn spawn_codex_follow_all(to_official: bool) {
    let target = codex_bucket(to_official);
    tauri::async_runtime::spawn_blocking(move || {
        let projects = match codex_follow_candidates(target) {
            Ok(list) => list,
            Err(err) => {
                log::warn!("tray codex follow: listing projects failed: {err}");
                return;
            }
        };
        if projects.is_empty() {
            return;
        }
        match crate::codex_follow::follow_projects(&projects, target) {
            // Nothing to refresh: the re-tag changes which threads `codex
            // resume` lists, and Termory's own Codex scan never filters on
            // `model_provider` — so no menu rebuild and no re-scan.
            Ok(res) => log::info!(
                "tray codex follow: re-tagged {} thread(s) into {target}",
                res.moved
            ),
            // A running Codex holds the DB lock — the switch itself already
            // landed, and the user can re-switch after quitting Codex.
            Err(err) => log::warn!("tray codex follow failed: {err}"),
        }
    });
}

/// Codex's OFFICIAL thread bucket — both official logins default to this
/// built-in provider id, so switching back to Official always lands here.
/// Mirror of `CODEX_OFFICIAL_PROVIDER_ID` in ProvidersPage.tsx.
const CODEX_OFFICIAL_BUCKET: &str = "openai";

/// The `model_provider` bucket a switch lands in. Both are fixed — every
/// Termory-written custom provider shares the one `termory` id — so the target
/// follows from the direction alone.
fn codex_bucket(to_official: bool) -> &'static str {
    if to_official {
        CODEX_OFFICIAL_BUCKET
    } else {
        crate::providers::TERMORY_PROVIDER_ID
    }
}

/// Projects holding at least one session OUTSIDE `target` — i.e. the ones a
/// switch would hide from `codex resume` unless they follow. Same filter as the
/// page's `maybePromptThenActivate`; limit 0 = every project, no cap.
///
/// Opens sqlite, so callers must be on a blocking worker.
fn codex_follow_candidates(target: &str) -> Result<Vec<String>, Box<dyn Error>> {
    Ok(crate::codex_follow::recent_projects(0)?
        .into_iter()
        .filter(|p| p.providers.iter().any(|id| id != target))
        .map(|p| p.project)
        .collect())
}

/// A bucket-CHANGING Codex switch with the silent-follow setting OFF.
///
/// The page only prompts when some project actually HAS off-target sessions —
/// with none there is nothing to follow, so it just switches. Matching that
/// means answering "are there candidates?", which opens sqlite and therefore
/// cannot run on the menu-event (main) thread; hence this worker:
///   * candidates → park the request, show the app, let the page prompt;
///   * none → apply the switch right here, exactly as a non-Codex click would.
///
/// A listing FAILURE prompts: switching silently could drop a project's
/// history from `codex resume`, while a needless prompt costs one dialog.
fn spawn_codex_bucket_switch(
    app: &AppHandle,
    cli: CliApp,
    set: CliProviders,
    provider_id: Option<String>,
) {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let to_official = provider_id.is_none();
        let needs_prompt = match codex_follow_candidates(codex_bucket(to_official)) {
            Ok(candidates) => !candidates.is_empty(),
            Err(err) => {
                log::warn!("tray codex switch: listing projects failed: {err}");
                true
            }
        };
        if !needs_prompt {
            apply_switch(&handle, cli, &set, provider_id.as_deref());
            return;
        }
        if let Ok(mut slot) = PENDING_SWITCH.lock() {
            *slot = Some(PendingSwitch {
                app: cli.key().to_string(),
                provider_id,
            });
        }
        // A native check item TOGGLES ITSELF when clicked (muda/NSMenuItem
        // checkbox behaviour); the writing path discards that by rebuilding
        // right after. This branch writes nothing, so without a rebuild the
        // clicked row stays lit ON TOP of the row that's really active —
        // "Official and a provider both checked" — and cancelling the prompt
        // leaves it that way.
        if let Err(err) = rebuild_menu(&handle) {
            log::error!("tray menu rebuild failed: {err}");
        }
        let win = handle.clone();
        let _ = handle.run_on_main_thread(move || show_main_window(&win));
        use tauri::Emitter;
        let _ = handle.emit(TRAY_SWITCH_REQUEST_EVENT, ());
    });
}

/// The write half of a provider switch: activate / deactivate, record the
/// activation marker, rebuild the menu, and tell an open Providers page to
/// re-derive. Shared by the direct click and the deferred Codex path above so
/// both leave identical state. `provider_id = None` → Official.
fn apply_switch(app: &AppHandle, cli: CliApp, set: &CliProviders, provider_id: Option<&str>) {
    let target = provider_id.and_then(|pid| set.row(pid));
    if provider_id.is_some() && target.is_none() {
        // The provider vanished from the library between click and write.
        let _ = rebuild_menu(app);
        return;
    }
    if let Err(err) = set.switch_to(cli, target) {
        log::error!("tray activation failed for {}: {err}", cli.key());
    } else if let Err(err) = config::set_active_provider_marker(cli.key(), provider_id) {
        // Mirror of the Providers page's `markActive`: records which provider is
        // now in use so the creds-collision disambiguation resolves to the one
        // the user just clicked. Official clears it.
        log::warn!("tray marker write failed for {}: {err}", cli.key());
    }
    if let Err(err) = rebuild_menu(app) {
        log::error!("tray menu rebuild failed: {err}");
    }
    // Tell any open Providers page to re-derive active state from disk.
    use tauri::Emitter;
    let _ = app.emit("termory:providers-changed", ());
}

/// One switchable row in a CLI's submenu.
#[derive(Clone)]
struct ProviderRow {
    provider: Provider,
    /// A gateway binding's synthesized provider rather than a standalone one.
    /// It decides which strip set the activation gets — see [`CliProviders`].
    from_gateway: bool,
}

/// Everything the tray needs to RENDER one CLI's provider submenu and to RUN a
/// click on it, resolved once. Deliberately shaped after the Providers page's
/// per-CLI memos so the two surfaces can't drift — that page is the reference
/// for both what is listed and what a switch writes:
///
/// | here | ProvidersPage.tsx |
/// |---|---|
/// | `rows` | `customProviders` ++ `gatewayBoundForApp` (the listed choices, and the candidate set `effectiveActiveId` matches against) |
/// | `standalone` | `providersForApp` (the strip set `activate_provider` gets for a standalone provider) |
/// | `all` | `allProvidersForApp` (what `deactivate_provider` and Grok's `set_default_provider` get) |
///
/// Note `rows` keeps only `kind == Custom` standalone entries (an Official-kind
/// record is not a switch target) while `standalone` keeps every kind, because
/// the strip set is a union of option KEYS to clean — narrowing it would leave
/// a sibling's keys behind in the live config.
#[derive(Clone)]
struct CliProviders {
    rows: Vec<ProviderRow>,
    standalone: Vec<Provider>,
    all: Vec<Provider>,
}

/// What a CLI's live config currently points at.
#[derive(Default)]
struct ActiveChoice {
    /// The in-use provider's id — the page's `effectiveActiveId`. `None` for
    /// Official AND for unmanaged.
    id: Option<String>,
    /// The live config is Termory's Official state (nothing injected) — the
    /// page's `activeState.kind === "official"`, which is what drives ITS
    /// Official card's "in use" badge.
    ///
    /// **Not the same as `id.is_none()`**: an UNMANAGED config — a third-party
    /// endpoint matching no provider in the library (cc-switch, a hand-edited
    /// config) — also yields no id, and reading that as Official would tick
    /// the Official row, title the CLI `· Official`, and hang the official
    /// account's QUOTA off someone else's endpoint. Under unmanaged the page
    /// marks nothing as in use, and so does the tray.
    official: bool,
}

impl CliProviders {
    /// `providers` = the whole library, `gateways` = every binding synth (both
    /// read once per menu build / click, then split per CLI here).
    fn resolve(cli: CliApp, providers: &[Provider], gateways: &[Provider]) -> Self {
        let standalone: Vec<Provider> =
            providers.iter().filter(|p| p.app == cli).cloned().collect();
        let bindings: Vec<Provider> = gateways.iter().filter(|p| p.app == cli).cloned().collect();
        let rows = standalone
            .iter()
            .filter(|p| p.kind == ProviderKind::Custom)
            .map(|p| ProviderRow {
                provider: p.clone(),
                from_gateway: false,
            })
            .chain(bindings.iter().map(|p| ProviderRow {
                provider: p.clone(),
                from_gateway: true,
            }))
            .collect();
        let mut all = standalone.clone();
        all.extend(bindings);
        Self {
            rows,
            standalone,
            all,
        }
    }

    fn row(&self, id: &str) -> Option<&ProviderRow> {
        self.rows.iter().find(|r| r.provider.id == id)
    }

    /// What's in use for this CLI — the SINGLE place the tray answers it, so
    /// the menu it renders and the decisions it makes off that state can't
    /// diverge. One `read_active_state` call per CLI per build.
    ///
    /// `id` follows the page's `effectiveActiveId`: the `active_provider_ids`
    /// marker wins only while the marked provider still matches the live config
    /// snapshot, else the reverse-derived match. Multi-slot (OpenCode/Grok)
    /// skip the marker — their `matched_provider_id` comes from the live
    /// default pointer, which carries the id already. NOTE this deliberately
    /// does NOT read the marker raw: the marker is a RECORD of Termory's last
    /// switch, so on its own it goes stale the moment the live config is
    /// changed by anything else.
    ///
    /// A read failure claims nothing (no id, not official) rather than
    /// asserting a state we couldn't determine.
    fn active_choice(&self, cli: CliApp, markers: &HashMap<String, String>) -> ActiveChoice {
        let candidates: Vec<Provider> = self.rows.iter().map(|r| r.provider.clone()).collect();
        let Ok(state) = read_active_state(cli, &candidates) else {
            return ActiveChoice::default();
        };
        let id = if matches!(cli, CliApp::Opencode | CliApp::Grok) {
            state.matched_provider_id.clone()
        } else {
            crate::providers::resolve_active_provider_id(
                &state,
                markers.get(cli.key()).map(String::as_str),
                &candidates,
            )
        };
        ActiveChoice {
            id,
            official: state.kind == crate::providers::ActiveKind::Official,
        }
    }

    /// Run the switch the clicked row asks for, writing exactly what the
    /// Providers page's `performOfficial` / `performSetAsDefault` /
    /// `performActivateGateway` write for the same choice.
    fn switch_to(&self, cli: CliApp, target: Option<&ProviderRow>) -> Result<(), Box<dyn Error>> {
        let Some(row) = target else {
            // "Official" — clear Termory's writes. Gets `all` so OpenCode /
            // Grok can also recognise (and clear) a default pointing at a
            // gateway binding's slot.
            return deactivate(cli, &self.all);
        };
        // A gateway binding activates with itself as the whole strip set, the
        // convention every GatewaysPage call uses; a standalone provider gets
        // the app's standalone list, so keys dropped from a sibling are cleaned.
        let strip_set: &[Provider] = if row.from_gateway {
            std::slice::from_ref(&row.provider)
        } else {
            &self.standalone
        };
        activate(&row.provider, strip_set)?;
        // Multi-slot (OpenCode + Grok): `activate` only adds the slot/entries —
        // being the STARTUP DEFAULT (what the checkmark and inline title track)
        // is a second write, exactly as the page does it. Single-slot CLIs set
        // their default implicitly on activate.
        if matches!(cli, CliApp::Opencode | CliApp::Grok) {
            set_default(&row.provider, &self.all)?;
        }
        Ok(())
    }
}

/// Does `cli`'s cached row carry the quota suffix? False when the row is
/// absent (uninstalled / disabled) or a non-Official choice is live.
fn cli_row_shows_quota(cli: CliApp) -> bool {
    CLI_ROWS
        .lock()
        .ok()
        .map(|rows| {
            rows.iter()
                .find(|r| r.cli == cli)
                .is_some_and(|r| r.shows_quota)
        })
        .unwrap_or(false)
}

/// Async, rate-limited quota fetch + tray update for every CLI in
/// `quota::SUPPORTED`. Used by the menu-open (tray click) hook and the
/// one-shot warm-up at startup.
pub fn trigger_quota_refresh(app: &AppHandle) {
    // Settings → Tools: don't fetch quota for a disabled tool — its CLI
    // row (where the numbers would show) is hidden anyway.
    let disabled = crate::config::disabled_sources();
    // Nor for a CLI whose row won't display it: the quota belongs to the
    // OFFICIAL login, so a row showing a custom provider (or an unmanaged
    // config) suppresses it — fetching would spend a network round-trip on a
    // number nothing renders. Read off the cached `CliRow.shows_quota`, the
    // same flag `build_menu` computed, so this stays free of disk I/O: the
    // callers are the tray-click handler and startup, both on the main thread.
    // An EMPTY cache means the menu hasn't been built yet (startup warm-up) —
    // fetch then, so the first build has numbers to show.
    let rows_built = CLI_ROWS.lock().map(|r| !r.is_empty()).unwrap_or(false);
    for &cli in crate::quota::SUPPORTED {
        if disabled.contains(cli.key()) {
            continue;
        }
        if rows_built && !cli_row_shows_quota(cli) {
            continue;
        }
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
    // Settings → Tools: a disabled tool's credential churn shouldn't
    // trigger fetches (its tray row is hidden).
    if crate::config::disabled_sources().contains(cli.key()) {
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
    do_rebuild_menu_with(app, detect_install_snapshot())
}

/// Full rebuild from an already-probed install snapshot (callers that
/// just paid for the probe pass it in, so the main thread doesn't
/// re-probe — `detect_install_snapshot` can spawn a shell per missing
/// CLI). INSTALLED is stored only after `set_menu` succeeds: on any
/// failure the cache keeps the OLD set, `menu_installed_stale` stays
/// true, and the next refresh retries the rebuild instead of believing
/// a menu that was never shown.
fn do_rebuild_menu_with(app: &AppHandle, installed: InstallSnapshot) -> tauri::Result<()> {
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
    refresh_recent_with(app, sessions, detect_install_snapshot());
}

/// `refresh_recent` from an already-probed install snapshot — the
/// watcher's install branch just ran the probe via `refresh_installed`,
/// so its fall-through rescan hands the result here instead of probing
/// the same PATH (and possibly spawning the same fallback shells) twice
/// in one burst.
pub fn refresh_recent_with(app: &AppHandle, sessions: &[AppSession], installed: InstallSnapshot) {
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

/// The terminal-launchable subset of an install snapshot, for the
/// recent region (recent-session resume, New Session) — excludes the
/// Claude Desktop GUI app, which isn't terminal-launchable.
///
/// Codex needs an extra gate: its installed-map entry is true when
/// EITHER the CLI or the desktop app is present (they share `~/.codex`,
/// so provider switching works with just the app), but terminal flows
/// spawn a `codex` binary — the snapshot's `codex_terminal` (standalone
/// CLI incl. the shell fallback, or the app's bundled binary) was
/// probed on the CALLER's thread, so no filesystem work happens here
/// on the main thread.
fn terminal_clis(installed: &InstallSnapshot) -> Vec<CliApp> {
    // Settings → Tools: a disabled tool gets no terminal rows either.
    // The disabled set rides in the snapshot (probed on the caller's
    // thread) — no config-file I/O here on the main thread; the recent
    // SESSION rows are already filtered upstream by scan_sessions.
    let mut clis = terminal_clis_in(&installed.map, installed.codex_terminal);
    clis.retain(|c| !installed.disabled.contains(c.key()));
    clis
}

fn terminal_clis_in(installed: &HashMap<CliApp, bool>, codex_cli: bool) -> Vec<CliApp> {
    CliApp::all()
        .into_iter()
        .filter(|c| c.is_cli() && installed.get(c).copied().unwrap_or(false))
        .filter(|c| *c != CliApp::Codex || codex_cli)
        .collect()
}

/// Store `recent` (when changed) and splice the dynamic region in place,
/// falling back to a full rebuild. When the installed-CLI set differs
/// from what the menu was built with (a CLI was installed/removed), do
/// a FULL rebuild instead — the splice can't refresh the per-CLI
/// provider submenus. MUST run on the main thread (queued like every
/// other menu mutation, so concurrent refreshers serialize).
fn commit_recent(app: &AppHandle, installed: &InstallSnapshot, recent: RecentState) {
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
pub fn refresh_installed(app: &AppHandle) -> InstallSnapshot {
    let installed = detect_install_snapshot();
    refresh_installed_with(app, installed.clone());
    installed
}

/// Compare an already-probed install snapshot against the menu and
/// rebuild when stale — the entry point for callers that ran the probe
/// themselves (the `detect_clis` IPC hands its result over so a
/// Providers-page Recheck also refreshes the tray, at no extra probe
/// cost).
pub fn refresh_installed_with(app: &AppHandle, installed: InstallSnapshot) {
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
    let installed = detect_install_snapshot();
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
            .map(|t| TrayTier {
                name: t.name.clone(),
                group: t.group.clone(),
                used: t.utilization,
            })
            .collect(),
        plan: quota.plan.clone(),
        credits: quota
            .extra_usage
            .as_ref()
            .filter(|e| e.is_enabled)
            .map(|e| TrayCredits {
                utilization: e.utilization.unwrap_or(0.0),
                used: e.used_credits.unwrap_or(0.0),
                limit: e.monthly_limit,
                currency: e.currency.clone(),
                decimal_places: e.decimal_places,
            }),
    };
    // A successful fetch that carries no usable windows AND no credits
    // means the official account has no active subscription quota (e.g.
    // logged in but unsubscribed — the usage endpoint returns windows
    // with no `utilization`). Drop any stale numbers instead of leaving
    // them on the menu, same as a definitive logout (`not_found`). When
    // pay-as-you-go credits ARE enabled we keep the entry so the tray
    // shows them even with no windows — matching the in-app card, which
    // renders credits off `extraUsage.isEnabled` regardless of tiers.
    if next.tiers.is_empty() && next.credits.is_none() {
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

/// Short menu label for a window: the localized labels for the
/// standard windows, "{n}h" / "{n}d" for generated `{n}_hour` /
/// `{n}_day` ids (Codex non-standard window lengths, e.g. the free
/// plan's 30-day window — mirrors `tierLabels` in
/// OfficialAccountsSection.tsx), raw id otherwise.
///
/// A MODEL-SCOPED window (`group` set — Claude's per-model weeklies)
/// renders as `{period} · {model}`, e.g. "Weekly · Fable": its `name`
/// is a bare model name, which alone wouldn't say WHICH window it is
/// sitting next to "5h". The period label comes from the API's own
/// grouping so a new period/model pair needs no code here.
fn tray_tier_label(name: &str, group: Option<&str>, labels: &TrayLabels) -> String {
    if let Some(group) = group {
        return format!("{} · {}", tray_group_label(group, labels), name);
    }
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

/// The API's period group (`session` / `weekly` / `monthly`) as the
/// same short label its account-wide counterpart uses, so "Weekly ·
/// Fable" reads consistently with the plain "Weekly" window. An
/// unrecognized group renders verbatim — new periods surface without
/// a release, exactly like unknown window ids.
fn tray_group_label(group: &str, labels: &TrayLabels) -> String {
    match group {
        "session" => labels.five_hour.clone(),
        "weekly" => labels.weekly.clone(),
        "monthly" => labels.monthly.clone(),
        other => other.to_string(),
    }
}

/// Render a credit amount in MINOR units (Claude sends cents +
/// `decimal_places`) or major units (grok, no decimal places) as a
/// short currency string — "$19.44" for USD, "19.44 EUR" otherwise.
/// Whole amounts drop the ".00" so "$3.00" reads "$3" (matches the
/// user-requested "$3 / $10" style). Rust has no Intl, so this is a
/// deliberately simple mirror of `formatCurrency` in format.ts.
fn format_credit_amount(value: f64, currency: Option<&str>, decimal_places: Option<u32>) -> String {
    let amount = match decimal_places {
        Some(dp) if dp > 0 => value / 10f64.powi(dp as i32),
        _ => value,
    };
    let num = if (amount.fract()).abs() < f64::EPSILON {
        format!("{:.0}", amount)
    } else {
        format!("{:.2}", amount)
    };
    match currency.unwrap_or("USD") {
        "USD" => format!("${num}"),
        code => format!("{num} {code}"),
    }
}

/// "🟢 $3 / $10 Credits" (or "🟢 $3 Credits" when no cap is set) —
/// the credits suffix appended after the quota windows.
fn credits_label(c: &TrayCredits, labels: &TrayLabels) -> String {
    let used = format_credit_amount(c.used, c.currency.as_deref(), c.decimal_places);
    let amounts = match c.limit {
        Some(limit) => {
            let limit = format_credit_amount(limit, c.currency.as_deref(), c.decimal_places);
            format!("{used} / {limit}")
        }
        None => used,
    };
    format!(
        "{} {} {}",
        quota_glyph(c.utilization),
        amounts,
        labels.credits
    )
}

/// "🟢 12% 5h · 🟡 78% Weekly · 🟢 $3 / $10 Credits" (or "🟢 9% 30d"
/// on a Codex free plan) — appended to the CLI's first-level row title
/// (percent right after the pressure glyph). None when no window and no
/// credits are known.
fn quota_label(q: &TrayQuota, labels: &TrayLabels) -> Option<String> {
    let mut parts: Vec<String> = q
        .tiers
        .iter()
        .map(|t| {
            format!(
                "{} {:.0}% {}",
                quota_glyph(t.used),
                t.used,
                tray_tier_label(&t.name, t.group.as_deref(), labels)
            )
        })
        .collect();
    if let Some(credits) = &q.credits {
        parts.push(credits_label(credits, labels));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" · "))
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
    // Newest first, by PARSED instant — this list mixes all sources, whose
    // RFC3339 offsets differ (`+00:00` / `Z`), so a lexicographic string
    // compare misorders them (same bug as the Records list). `None` /
    // unparseable sorts last under descending order (`record_instant` → MIN).
    picked.sort_by(|a, b| {
        crate::sessions::record_instant(&b.updated_at)
            .cmp(&crate::sessions::record_instant(&a.updated_at))
    });

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
        CliApp::Grok => "Grok",
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

/// Max chars for a recent-session menu label before it's truncated with an
/// ellipsis — keeps the menu bar narrow.
const RECENT_LABEL_MAX: usize = 24;

/// Menu label for a recent session: title, else snippet, else "(untitled)",
/// truncated so the menu stays narrow.
fn recent_label(title: &str, snippet: &str) -> String {
    let raw = label_text(title, snippet);
    let raw = if raw.is_empty() { "(untitled)" } else { raw };
    let mut out: String = raw.chars().take(RECENT_LABEL_MAX).collect();
    if raw.chars().count() > RECENT_LABEL_MAX {
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
                    format!("tray:new:{}:{}", cli.key(), target.project),
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
            let item =
                MenuItemBuilder::with_id(format!("tray:newpick:{}", cli.key()), cli_label(*cli))
                    .build(app)?;
            pick_sub = pick_sub.item(&item);
        }
        sub = sub.item(&pick_sub.build()?);
        region.push(Box::new(sub.build()?));
        region.push(Box::new(PredefinedMenuItem::separator(app)?));
    }
    Ok(region)
}

fn build_menu(app: &AppHandle, installed: &InstallSnapshot) -> tauri::Result<Menu<Wry>> {
    let providers: Vec<Provider> =
        crate::providers::providers_from_json(config::read_providers().unwrap_or_default());

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
    // the caller's `detect_install_snapshot()` probe — same one the
    // Providers page uses, so the two agree). The CALLER stores it into INSTALLED
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

    // Per-CLI activation markers (config.json `active_provider_ids`, written
    // by the Providers page). Used to disambiguate a standalone provider and
    // a gateway binding that share identical creds — the SAME rule the
    // Providers page applies (`resolveActiveProviderId`); without it the tray
    // reverse-derives to whichever matches first and checkmarks the wrong one.
    let markers = config::active_provider_markers();

    // Settings → Tools: disabled tools get no provider-switch submenu
    // (the disabled set rides in the snapshot — no config I/O here).
    let mut cli_rows: Vec<CliRow> = Vec::new();
    let mut account_rows: Vec<AccountRow> = Vec::new();
    for cli in CliApp::all() {
        if !installed.map.get(&cli).copied().unwrap_or(false)
            || installed.disabled.contains(cli.key())
        {
            continue;
        }
        // The user's standalone providers PLUS this CLI's gateway bindings
        // (synthesized into the same Provider shape), so both appear as
        // switchable choices and the active checkmark lands on whichever is
        // live — the same list, in the same order, as the Providers page.
        let set = CliProviders::resolve(cli, &providers, &gateways);
        // The menu bar previously used `matched_provider_id` alone, i.e. field
        // matching only, so a standalone provider and a gateway binding sharing
        // one endpoint were indistinguishable and it checkmarked whichever came
        // first — disagreeing with the page.
        let active = set.active_choice(cli, &markers);
        let active_id = active.id.clone();

        // First-level title shows the currently-active choice inline
        // (e.g. "Claude Code · Official"): the in-use provider's name, else
        // "Official". An UNMANAGED config names neither — the CLI points
        // somewhere Termory doesn't know — so the row carries no suffix at
        // all rather than claiming a choice the user didn't make here.
        let base_title = cli_base_title(cli, &set, &active, &labels);
        // Quota-capable CLI with Official active: the title carries
        // the official-account plan + quota suffix. Suppressed while
        // anything else is live — the quota belongs to the official login,
        // and gluing it onto a custom (or unknown) endpoint's row would
        // read as that endpoint's usage.
        let shows_quota = crate::quota::supports_quota(cli) && active.official;
        let title = cli_row_title(&base_title, shows_quota, cli, &labels);
        let mut sub = SubmenuBuilder::new(app, title);

        let official =
            CheckMenuItemBuilder::with_id(format!("tray:{}:official", cli.key()), &labels.official)
                // Same rule as the page's Official card (`activeState.kind ===
                // "official"`), NOT "no provider matched" — see `ActiveChoice`.
                .checked(active.official)
                .build(app)?;
        sub = sub.item(&official);

        // Saved official logins (Codex only — the one CLI with snapshot
        // management), directly under Official: they are that login's accounts,
        // not more providers, and the Providers page likewise carries them on
        // the Official card, above the provider list. The separator alone marks
        // the group — no title row, so the menu stays a plain list of things you
        // can click. A ⚠ suffix marks an entry whose refresh token was revoked
        // (the page's "needs re-login" badge — switching to it will fail until
        // the user re-authenticates there).
        //
        // Each group opens with its own separator, so a CLI with no accounts,
        // no providers, or neither still gets exactly the rules it needs.
        let accounts = crate::accounts::tray_accounts(cli);
        if !accounts.is_empty() {
            sub = sub.item(&PredefinedMenuItem::separator(app)?);
            for a in &accounts {
                let item = CheckMenuItemBuilder::with_id(
                    format!("tray:{}:account:{}", cli.key(), a.id),
                    account_row_label(a),
                )
                .checked(a.active)
                // Only a revoked refresh token disables a row — it can be fixed
                // only by re-authenticating on the Providers page, so the click
                // would just fail. The LIVE row stays enabled: greying out the
                // account in use reads as "unavailable" next to its own
                // checkmark, and it's the one row that should look normal.
                // Clicking it is a no-op, guarded in the handler the same way
                // the page guards it (`if (account.active) return`).
                .enabled(!a.needs_relogin)
                .build(app)?;
                sub = sub.item(&item);
                // Keep the handle so a switch can update this row without a
                // menu-closing rebuild — see AccountRow.
                account_rows.push(AccountRow {
                    cli,
                    id: a.id.clone(),
                    item,
                });
            }
        }

        if !set.rows.is_empty() {
            sub = sub.item(&PredefinedMenuItem::separator(app)?);
            for row in &set.rows {
                let p = &row.provider;
                let is_active = active_id.as_deref() == Some(p.id.as_str());
                let item = CheckMenuItemBuilder::with_id(
                    format!("tray:{}:custom:{}", cli.key(), p.id),
                    labels.provider_name(p),
                )
                .checked(is_active)
                .build(app)?;
                sub = sub.item(&item);
            }
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
    // Whether anything sits between "Open" and "Exit": the recent region
    // (sessions / New Session) or any CLI provider submenu. Captured BEFORE
    // cli_rows is moved into the cache below.
    let has_middle = region_len > 0 || !cli_rows.is_empty();

    if let Ok(mut rows) = CLI_ROWS.lock() {
        *rows = cli_rows;
    }
    if let Ok(mut rows) = ACCOUNT_ROWS.lock() {
        *rows = account_rows;
    }

    // Separate Exit from the content above ONLY when there IS content. With
    // an empty middle (no tools AND no records) the "Open" separator and
    // this one would be adjacent — a double rule between Open and Exit.
    // Skipping it leaves a single "Open ─── Exit" divider.
    if has_middle {
        menu = menu.item(&PredefinedMenuItem::separator(app)?);
    }
    // Plain MenuItem (not PredefinedMenuItem::quit) so macOS doesn't
    // attach the native quit-item icon — keeps the menu icon-free.
    let quit = MenuItemBuilder::with_id("tray:quit", &labels.exit).build(app)?;
    menu = menu.item(&quit);

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

    // Switching the official LOGIN, not the provider — a different axis, so it
    // returns before any provider state is read or written.
    if kind == "account" {
        // Rebuild FIRST: the click already toggled the row's native checkmark,
        // and the switch below starts with a network token refresh that can take
        // seconds — until it lands, two account rows would show as checked.
        if let Err(err) = rebuild_menu(app) {
            log::error!("tray menu rebuild failed: {err}");
        }
        if let Some(id) = provider_id {
            // Clicking the account that's ALREADY live does nothing — the same
            // guard the page applies (`switchTo`: `if (account.active) return`).
            // Without it the row would run a full token refresh + auth.json
            // rewrite to land exactly where it already was. The rebuild above
            // has already restored its checkmark after the native toggle.
            let already_live = crate::accounts::tray_accounts(cli)
                .iter()
                .any(|a| a.id == id && a.active);
            if !already_live {
                spawn_account_switch(app, cli, id.to_string());
            }
        }
        return;
    }

    // The clicked row → the same list the menu was built from, resolved once:
    // every branch below (park-direction check, the write, the marker) reads
    // this one answer, so they cannot disagree with each other or with what the
    // user saw. `None` target = the "Official" row.
    let providers: Vec<Provider> =
        crate::providers::providers_from_json(config::read_providers().unwrap_or_default());
    let set = CliProviders::resolve(cli, &providers, &gateway_providers());
    let target = match (kind, provider_id) {
        ("official", _) => None,
        ("custom", Some(pid)) => match set.row(pid) {
            Some(row) => Some(row),
            // Row names a provider that's gone from the library: nothing to
            // switch to, but the click lit its checkmark — rebuild to drop it.
            None => {
                let _ = rebuild_menu(app);
                return;
            }
        },
        _ => {
            let _ = rebuild_menu(app);
            return;
        }
    };
    // Direction for the Codex bucket check below. Uses the in-use ID, matching
    // the page's own prompt conditions (`effectiveActiveId === null` ⇒ treat as
    // official) — so an unmanaged config prompts on the way OUT to a custom
    // provider, exactly as the page does. This is a different question from the
    // Official ROW's checkmark, which follows `ActiveChoice::official`.
    let was_official = set
        .active_choice(cli, &config::active_provider_markers())
        .id
        .is_none();
    let to_official = target.is_none();

    // Codex tags every thread with the `model_provider` active at creation and
    // `codex resume` only lists threads matching the CURRENT one — so an
    // official↔custom switch hides a project's prior sessions unless they're
    // re-tagged, and the Providers page ASKS which projects should follow
    // (`CodexFollowDialog`) before switching. Unless the user opted into
    // following everything silently (Settings → `codex_keep_all_sessions`), such a
    // switch is deferred to `spawn_codex_bucket_switch`, which decides between
    // handing it to the page and just doing it — see there.
    //
    // The direction comes from the SAME validated answer the menu renders — NOT
    // the raw marker. The marker records Termory's last switch, so after an
    // external change (cc-switch / a hand-edited config.toml) it can claim
    // "custom" while the live config is Official: the bucket would then really
    // change, yet `was_official != to_official` would be false and we'd switch
    // with no prompt and no follow, dropping the project's earlier sessions
    // from `codex resume`.
    let bucket_changes = cli == CliApp::Codex && was_official != to_official;
    if bucket_changes && !config::codex_keep_all_sessions() {
        let provider_id = target.map(|r| r.provider.id.clone());
        spawn_codex_bucket_switch(app, cli, set.clone(), provider_id);
        return;
    }

    apply_switch(app, cli, &set, target.map(|r| r.provider.id.as_str()));
    // Only reachable with the silent-follow setting ON (the deferred path
    // returned above), which is the one case the tray re-tags by itself.
    if bucket_changes {
        spawn_codex_follow_all(to_official);
    }
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
        CliApp::Grok => "Grok Build",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, app: &str, kind: &str, name: &str) -> Provider {
        serde_json::from_value(serde_json::json!({
            "id": id, "app": app, "kind": kind, "name": name,
            "baseUrl": "https://api.example.com", "apiKey": "sk-test"
        }))
        .expect("provider fixture")
    }

    #[test]
    fn cli_providers_mirrors_the_page_lists() {
        let library = vec![
            provider("c1", "claude", "custom", "Anthropic Proxy"),
            // Official-kind + other-app entries are not switch targets.
            provider("legacy", "claude", "official", "Legacy"),
            provider("x1", "codex", "custom", "Other CLI"),
            provider("c2", "claude", "custom", "OpenRouter"),
        ];
        let gateways = vec![
            provider("bind-claude", "claude", "custom", "My Gateway"),
            provider("bind-codex", "codex", "custom", "My Gateway"),
        ];
        let set = CliProviders::resolve(CliApp::Claude, &library, &gateways);

        // Rows = the page's `customProviders` ++ `gatewayBoundForApp`: this
        // CLI's CUSTOM standalone providers in library order, then its gateway
        // bindings — which is also the order the submenu renders.
        let rows: Vec<(&str, bool)> = set
            .rows
            .iter()
            .map(|r| (r.provider.id.as_str(), r.from_gateway))
            .collect();
        assert_eq!(
            rows,
            vec![("c1", false), ("c2", false), ("bind-claude", true)]
        );

        // `standalone` (the page's `providersForApp`) keeps EVERY kind — it is
        // the option-key strip set, so narrowing it would leave a sibling's
        // keys behind in the live config.
        let standalone: Vec<&str> = set.standalone.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(standalone, vec!["c1", "legacy", "c2"]);

        // `all` (the page's `allProvidersForApp`) adds the gateway synths.
        let all: Vec<&str> = set.all.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(all, vec!["c1", "legacy", "c2", "bind-claude"]);

        // A gateway row is flagged so its activation gets itself as the whole
        // strip set, the convention every GatewaysPage call uses.
        assert!(set.row("bind-claude").expect("gateway row").from_gateway);
        assert!(!set.row("c2").expect("standalone row").from_gateway);
        // An Official-kind entry is never a clickable row.
        assert!(set.row("legacy").is_none());
    }

    #[test]
    fn provider_name_falls_back_to_the_unnamed_placeholder() {
        let labels = TrayLabels::default();
        assert_eq!(
            labels.provider_name(&provider("a", "claude", "custom", "Kimi")),
            "Kimi"
        );
        // Blank / whitespace-only names would otherwise render an empty menu
        // row, so they show the placeholder — same as the Providers page.
        assert_eq!(
            labels.provider_name(&provider("b", "claude", "custom", "   ")),
            "(unnamed)"
        );
    }

    #[test]
    fn cli_base_title_names_the_choice_and_stays_bare_when_unmanaged() {
        let labels = TrayLabels::default();
        let library = vec![provider("p1", "codex", "custom", "OpenRouter")];
        let set = CliProviders::resolve(CliApp::Codex, &library, &[]);

        // Official: the localized "Official" suffix.
        let official = ActiveChoice {
            id: None,
            official: true,
        };
        assert_eq!(
            cli_base_title(CliApp::Codex, &set, &official, &labels),
            "Codex · Official"
        );

        // A custom provider in use: its name.
        let custom = ActiveChoice {
            id: Some("p1".into()),
            official: false,
        };
        assert_eq!(
            cli_base_title(CliApp::Codex, &set, &custom, &labels),
            "Codex · OpenRouter"
        );

        // UNMANAGED — the live config points somewhere Termory doesn't know, so
        // the row names NEITHER choice rather than claiming "Official".
        let unmanaged = ActiveChoice {
            id: None,
            official: false,
        };
        assert_eq!(
            cli_base_title(CliApp::Codex, &set, &unmanaged, &labels),
            "Codex"
        );

        // A marker naming a provider that's gone from the library falls back
        // the same way — there is no row to take the name from.
        let stale = ActiveChoice {
            id: Some("deleted".into()),
            official: false,
        };
        assert_eq!(
            cli_base_title(CliApp::Codex, &set, &stale, &labels),
            "Codex"
        );
    }

    #[test]
    fn account_row_label_marks_a_revoked_token() {
        let base = crate::accounts::TrayAccount {
            id: "acct".into(),
            label: "a@example.com".into(),
            active: false,
            needs_relogin: false,
        };
        assert_eq!(account_row_label(&base), "a@example.com");
        let revoked = crate::accounts::TrayAccount {
            needs_relogin: true,
            ..base
        };
        assert_eq!(account_row_label(&revoked), "a@example.com ⚠");
    }

    /// Account-wide window (its name IS the period, so no group).
    fn wide_tier(name: &str, used: f64) -> TrayTier {
        TrayTier {
            name: name.into(),
            group: None,
            used,
        }
    }

    #[test]
    fn quota_label_formats_known_generated_and_missing_windows() {
        let labels = TrayLabels::default();
        let both = TrayQuota {
            tiers: vec![wide_tier("five_hour", 12.4), wide_tier("seven_day", 78.0)],
            plan: None,
            credits: None,
        };
        assert_eq!(
            quota_label(&both, &labels).as_deref(),
            Some("🟢 12% 5h · 🟡 78% W")
        );
        // Codex free plan: a single 30-day window → the "M" default label.
        let monthly = TrayQuota {
            tiers: vec![wide_tier("30_day", 9.0)],
            plan: None,
            credits: None,
        };
        assert_eq!(quota_label(&monthly, &labels).as_deref(), Some("🟢 9% M"));
        // Truly unknown ids pass through raw.
        let odd = TrayQuota {
            tiers: vec![wide_tier("mystery_window", 99.6)],
            plan: None,
            credits: None,
        };
        assert_eq!(
            quota_label(&odd, &labels).as_deref(),
            Some("🔴 100% mystery_window")
        );
        let none = TrayQuota {
            tiers: vec![],
            plan: None,
            credits: None,
        };
        assert_eq!(quota_label(&none, &labels), None);
    }

    #[test]
    fn quota_label_appends_credits_after_windows() {
        let labels = TrayLabels::default();
        // Claude sends cents + decimal_places=2 → "$19.44 / $50 Credits"
        // ($50.00 trims to $50; $19.44 keeps its cents).
        let q = TrayQuota {
            tiers: vec![wide_tier("five_hour", 12.0)],
            plan: None,
            credits: Some(TrayCredits {
                utilization: 38.88,
                used: 1944.0,
                limit: Some(5000.0),
                currency: Some("USD".into()),
                decimal_places: Some(2),
            }),
        };
        assert_eq!(
            quota_label(&q, &labels).as_deref(),
            Some("🟢 12% 5h · 🟢 $19.44 / $50 Credits")
        );
        // grok stores major units (no decimal_places) and may have no cap.
        let grok = TrayQuota {
            tiers: vec![],
            plan: None,
            credits: Some(TrayCredits {
                utilization: 95.0,
                used: 3.0,
                limit: None,
                currency: Some("USD".into()),
                decimal_places: None,
            }),
        };
        assert_eq!(
            quota_label(&grok, &labels).as_deref(),
            Some("🔴 $3 Credits")
        );
    }

    #[test]
    fn tray_tier_label_humanizes_generated_ids() {
        let labels = TrayLabels::default();
        assert_eq!(tray_tier_label("five_hour", None, &labels), "5h");
        assert_eq!(tray_tier_label("seven_day", None, &labels), "W");
        assert_eq!(tray_tier_label("3_hour", None, &labels), "3h");
        assert_eq!(tray_tier_label("30_day", None, &labels), "M");
        assert_eq!(tray_tier_label("14_day", None, &labels), "14d");
        assert_eq!(tray_tier_label("gemini_pro", None, &labels), "Pro");
        assert_eq!(tray_tier_label("gemini_flash_lite", None, &labels), "Lite");
        assert_eq!(tray_tier_label("_day", None, &labels), "_day"); // no digits → raw
        assert_eq!(
            tray_tier_label("weekly_limit", None, &labels),
            "weekly_limit"
        );
    }

    #[test]
    fn tray_tier_label_composes_model_scoped_windows_with_their_period() {
        let labels = TrayLabels::default();
        // A model-scoped weekly reads as its period + the model, so it
        // can't be mistaken for a window of its own next to "5h".
        assert_eq!(
            tray_tier_label("Fable", Some("weekly"), &labels),
            "W · Fable"
        );
        assert_eq!(
            tray_tier_label("Opus", Some("session"), &labels),
            "5h · Opus"
        );
        assert_eq!(
            tray_tier_label("Haiku", Some("monthly"), &labels),
            "M · Haiku"
        );
        // An unrecognized period renders verbatim rather than vanishing —
        // same "unknown surfaces without a release" rule as window ids.
        assert_eq!(
            tray_tier_label("Fable", Some("fortnightly"), &labels),
            "fortnightly · Fable"
        );
        // The model name never falls through the id-humanizing arms: a
        // model literally called "3_hour" stays itself.
        assert_eq!(
            tray_tier_label("3_hour", Some("weekly"), &labels),
            "W · 3_hour"
        );
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
        // Over-long titles truncate to RECENT_LABEL_MAX chars + an ellipsis.
        let out = recent_label(&"x".repeat(60), "");
        assert_eq!(out.chars().count(), RECENT_LABEL_MAX + 1);
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
    fn select_recent_state_sorts_mixed_offsets_by_instant() {
        // The recent list mixes sources with different UTC offsets. A Codex
        // record carrying a local `+08:00` offset (its historical shape) is an
        // EARLIER instant (12:00Z) than the Claude `Z` record (13:00Z), yet its
        // larger wall-clock digits sort it ABOVE under a lexicographic string
        // compare. The parsed-instant sort must put the newer Claude one first.
        let sessions = vec![
            sess_in("Codex", "/work/a", "x-earlier", "2026-07-21T20:00:00+08:00"),
            sess_in("Claude", "/work/b", "c-later", "2026-07-21T13:00:00Z"),
        ];
        let state = select_recent_state(&sessions);
        let ids: Vec<&str> = state.sessions.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["c-later", "x-earlier"]);
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
    fn terminal_clis_filters_to_installed_real_clis() {
        let mut installed: HashMap<CliApp, bool> = HashMap::new();
        installed.insert(CliApp::Claude, true);
        installed.insert(CliApp::Codex, false);
        // Claude Desktop is installed but NOT a CLI — terminal flows
        // (recent-session resume, New Session) must exclude it.
        installed.insert(CliApp::ClaudeDesktop, true);
        let got = terminal_clis_in(&installed, true);
        assert_eq!(got, vec![CliApp::Claude]);
        // Order follows CliApp::all(), not map iteration order.
        installed.insert(CliApp::Codex, true);
        installed.insert(CliApp::Opencode, true);
        let got = terminal_clis_in(&installed, true);
        assert_eq!(got, vec![CliApp::Claude, CliApp::Codex, CliApp::Opencode]);
        // Codex installed via the DESKTOP APP only (map says true, no
        // CLI binary) — terminal flows can't spawn `codex`, so it must
        // be excluded even though the installed map includes it.
        let got = terminal_clis_in(&installed, false);
        assert_eq!(got, vec![CliApp::Claude, CliApp::Opencode]);
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
