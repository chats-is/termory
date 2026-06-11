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
use crate::sessions::AppSession;
use std::sync::Mutex;
use tauri::{
    menu::{
        CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem,
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
}

/// One "New session" group: a recent project dir. The submenu shows
/// its basename as a (disabled) group header with one row per
/// INSTALLED CLI underneath — any CLI can be launched fresh in any
/// recent project, not just the (cwd, CLI) pairs seen in history.
#[derive(Clone, PartialEq)]
struct NewSessionProject {
    project: String,
    label: String,
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
    let menu = build_menu(app)?;
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
        // numbers show from the NEXT open (floors: 10 min after a
        // success, 60s after a failure — QUOTA_TRAY_MIN_INTERVAL /
        // QUOTA_TRAY_ERROR_RETRY).
        .on_tray_icon_event(|tray, event| {
            if matches!(event, tauri::tray::TrayIconEvent::Click { .. }) {
                trigger_quota_refresh(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Minimum spacing between quota fetches per CLI — same window as the
/// Providers page's auto-refresh cache (QUOTA_STALE_MS).
const QUOTA_TRAY_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
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
pub fn rebuild_menu(app: &AppHandle) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let menu = build_menu(app)?;
        tray.set_menu(Some(menu))?;
    }
    Ok(())
}

/// Recompute the recent cache from a fresh scan and rebuild the
/// menu so the entries under "Open" stay current. Reuses the caller's scan
/// (watcher / `scan_all_sessions`); skips the rebuild when nothing changed
/// so active CLI use doesn't churn the menu on every file event.
pub fn refresh_recent(app: &AppHandle, sessions: &[AppSession]) {
    let recent = select_recent_state(sessions);
    match RECENT.lock() {
        Ok(mut guard) if *guard != recent => *guard = recent,
        _ => return, // unchanged (or poisoned) → no rebuild
    }
    if let Err(err) = rebuild_menu(app) {
        log::error!("tray recent rebuild failed: {err}");
    }
}

/// Record a completed quota fetch (any source) and rebuild the menu
/// when the displayed numbers changed. Failed fetches only refresh the
/// rate-limit marker — the menu keeps the last good numbers instead of
/// flickering empty.
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
            let removed = QUOTA
                .lock()
                .map(|mut guard| {
                    let before = guard.len();
                    guard.retain(|(c, _)| *c != cli);
                    guard.len() != before
                })
                .unwrap_or(false);
            if removed {
                if let Err(err) = rebuild_menu(app) {
                    log::error!("tray quota rebuild failed: {err}");
                }
            }
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
    match QUOTA.lock() {
        Ok(mut guard) => match guard.iter_mut().find(|(c, _)| *c == cli) {
            Some((_, cur)) if *cur == next => return, // unchanged → no rebuild
            Some((_, cur)) => *cur = next,
            None => guard.push((cli, next)),
        },
        _ => return,
    }
    if let Err(err) = rebuild_menu(app) {
        log::error!("tray quota rebuild failed: {err}");
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
        })
        .collect();

    let mut targets: Vec<NewSessionProject> = Vec::new();
    for s in &picked {
        // "New session" needs a cwd to land in.
        if s.project.is_empty() {
            continue;
        }
        // Dedup by cwd ACROSS CLIs — the group lists every installed
        // CLI anyway, so which CLI the history came from is irrelevant.
        if targets.iter().any(|t| t.project == s.project) {
            continue;
        }
        targets.push(NewSessionProject {
            project: s.project.clone(),
            label: project_dir_label(&s.project),
        });
        if targets.len() >= NEW_SESSION_PROJECT_LIMIT {
            break;
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

/// `AppSession.source`-style string for a CliApp — what
/// `terminal::new_session` / `resume_session` dispatch on.
fn cli_source(cli: CliApp) -> &'static str {
    match cli {
        CliApp::Claude => "Claude",
        CliApp::Codex => "Codex",
        CliApp::Gemini => "Gemini",
        CliApp::Opencode => "OpenCode",
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

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
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

    // Only surface CLIs actually installed on this machine — same probe the
    // Providers page uses (`detect_clis`), so the two agree. Consumed by both
    // the "New session" groups below and the per-CLI provider submenus.
    let installed = detect_installed_clis();

    let recent = RECENT.lock().map(|g| g.clone()).unwrap_or_default();
    if !recent.targets.is_empty() {
        let mut sub = SubmenuBuilder::new(app, &labels.new_session);
        for (pidx, target) in recent.targets.iter().enumerate() {
            if pidx > 0 {
                sub = sub.item(&PredefinedMenuItem::separator(app)?);
            }
            // Group header: the project dir, display-only.
            let header = MenuItemBuilder::with_id(format!("tray:newhdr:{pidx}"), &target.label)
                .enabled(false)
                .build(app)?;
            sub = sub.item(&header);
            for cli in CliApp::all() {
                if !installed.get(&cli).copied().unwrap_or(false) {
                    continue;
                }
                let item = MenuItemBuilder::with_id(
                    format!("tray:new:{pidx}:{}", cli_key(cli)),
                    cli_label(cli),
                )
                .build(app)?;
                sub = sub.item(&item);
            }
        }
        menu = menu.item(&sub.build()?);
    }
    menu = menu.item(&PredefinedMenuItem::separator(app)?);

    if !recent.sessions.is_empty() {
        for (idx, r) in recent.sessions.iter().enumerate() {
            let item =
                MenuItemBuilder::with_id(format!("tray:session:{idx}"), &r.label).build(app)?;
            menu = menu.item(&item);
        }
        menu = menu.item(&PredefinedMenuItem::separator(app)?);
    }

    // Read the gateway bindings ONCE, then group per CLI in the loop.
    // (Uninstalled CLIs get no provider submenu — nothing to switch.)
    let gateways = gateway_providers();

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
        let mut title = format!("{} · {}", cli_label(cli), active_name);
        // Quota-capable CLI with Official active: append the
        // official-account quota inline, e.g.
        // "Claude Code · Official · 🟢 12% 5h · 🟡 78% Weekly".
        // Suppressed while a custom provider is active — the quota
        // belongs to the official login, and gluing it onto a custom
        // provider's name would read as that provider's usage.
        if crate::quota::supports_quota(cli) && active_id.is_none() {
            if let Some(q) = QUOTA
                .lock()
                .ok()
                .and_then(|g| g.iter().find(|(c, _)| *c == cli).map(|(_, q)| q.clone()))
            {
                // "Claude Code · Official (Max) · 🟢 12% 5h · …"
                if let Some(plan) = &q.plan {
                    title = format!("{title} ({plan})");
                }
                if let Some(label) = quota_label(&q, &labels) {
                    title = format!("{title} · {label}");
                }
            }
        }
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
    }

    let sep = PredefinedMenuItem::separator(app)?;
    // Plain MenuItem (not PredefinedMenuItem::quit) so macOS doesn't
    // attach the native quit-item icon — keeps the menu icon-free.
    let quit = MenuItemBuilder::with_id("tray:quit", &labels.exit).build(app)?;
    menu = menu.item(&sep).item(&quit);

    menu.build()
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
    // Fire-and-forget from the tray (no toast surface); errors logged.
    if let Some(idx) = id
        .strip_prefix("tray:session:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        if let Some(r) = RECENT
            .lock()
            .ok()
            .and_then(|g| g.sessions.get(idx).cloned())
        {
            let _ = crate::terminal::resume_session(&r.source, &r.id, Some(&r.project));
        }
        return;
    }
    // A "New session" row (`tray:new:{project}:{cli}`) → open the
    // chosen terminal in that project's cwd and launch that CLI fresh.
    if let Some(rest) = id.strip_prefix("tray:new:") {
        let mut parts = rest.splitn(2, ':');
        let (Some(pidx), Some(cli)) = (
            parts.next().and_then(|n| n.parse::<usize>().ok()),
            parts.next().and_then(CliApp::parse),
        ) else {
            return;
        };
        if let Some(target) = RECENT
            .lock()
            .ok()
            .and_then(|g| g.targets.get(pidx).cloned())
        {
            let _ = crate::terminal::new_session(cli_source(cli), Some(&target.project));
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
    }
}

fn cli_key(cli: CliApp) -> &'static str {
    match cli {
        CliApp::Claude => "claude",
        CliApp::Codex => "codex",
        CliApp::Gemini => "gemini",
        CliApp::Opencode => "opencode",
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
