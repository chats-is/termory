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

/// How many recent session titles to surface under "Open".
const RECENT_LIMIT: usize = 5;

/// One cached recent-session row. A click opens a terminal in `project` and
/// resumes the session in its CLI (`source` + `id`); `label` is the menu text.
#[derive(Clone, PartialEq)]
struct RecentSession {
    source: String,
    project: String,
    id: String,
    label: String,
}

/// Recent sessions shown under "Open", refreshed from each scan (watcher +
/// the `scan_all_sessions` IPC) — the tray never scans on its own.
static RECENT: Mutex<Vec<RecentSession>> = Mutex::new(Vec::new());

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
        .build(app)?;
    Ok(())
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

/// Recompute the recent-sessions cache from a fresh scan and rebuild the
/// menu so the titles under "Open" stay current. Reuses the caller's scan
/// (watcher / `scan_all_sessions`); skips the rebuild when nothing changed
/// so active CLI use doesn't churn the menu on every file event.
pub fn refresh_recent(app: &AppHandle, sessions: &[AppSession]) {
    let recent = select_recent(sessions);
    match RECENT.lock() {
        Ok(mut guard) if *guard != recent => *guard = recent,
        _ => return, // unchanged (or poisoned) → no rebuild
    }
    if let Err(err) = rebuild_menu(app) {
        log::error!("tray recent rebuild failed: {err}");
    }
}

/// The (pure) selection: drop Memory/Skill, newest first, cap at
/// `RECENT_LIMIT`, map to the cached row.
fn select_recent(sessions: &[AppSession]) -> Vec<RecentSession> {
    let mut picked: Vec<&AppSession> = sessions
        .iter()
        .filter(|s| !matches!(s.source.as_str(), "Memory" | "Skill"))
        // Drop "sessions" that are really just a CLI slash command
        // (`/model`, `/clear`, …) — they're system commands, not chats.
        .filter(|s| !is_slash_command(label_text(&s.title, &s.snippet)))
        .collect();
    // Newest first. ISO-8601 `updated_at` sorts lexicographically =
    // chronologically; `None` sorts last under descending order.
    picked.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    picked
        .into_iter()
        .take(RECENT_LIMIT)
        .map(|s| RecentSession {
            source: s.source.clone(),
            project: s.project.clone(),
            id: s.id.clone(),
            label: recent_label(&s.title, &s.snippet),
        })
        .collect()
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

    let mut menu = MenuBuilder::new(app);

    // "Open" sits at the very top of the menu.
    let open = MenuItemBuilder::with_id("tray:open", "Open").build(app)?;
    menu = menu.item(&open).item(&PredefinedMenuItem::separator(app)?);

    // Recent sessions (newest first) — each opens in Records on click.
    let recent = RECENT.lock().map(|g| g.clone()).unwrap_or_default();
    if !recent.is_empty() {
        for (idx, r) in recent.iter().enumerate() {
            let item =
                MenuItemBuilder::with_id(format!("tray:session:{idx}"), &r.label).build(app)?;
            menu = menu.item(&item);
        }
        menu = menu.item(&PredefinedMenuItem::separator(app)?);
    }

    // Only surface CLIs actually installed on this machine — same probe the
    // Providers page uses (`detect_clis`), so the two agree. Uninstalled CLIs
    // get no submenu (nothing to switch).
    let installed = detect_installed_clis();
    // Read the gateway bindings ONCE, then group per CLI in the loop.
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
            .unwrap_or("Official");
        let title = format!("{} · {}", cli_label(cli), active_name);
        let mut sub = SubmenuBuilder::new(app, title);

        let official =
            CheckMenuItemBuilder::with_id(format!("tray:{}:official", cli_key(cli)), "Official")
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
    let quit = MenuItemBuilder::with_id("tray:quit", "Exit").build(app)?;
    menu = menu.item(&sep).item(&quit);

    menu.build()
}

fn show_main_window(app: &AppHandle) {
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
    if let Some(idx) = id
        .strip_prefix("tray:session:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        if let Some(r) = RECENT.lock().ok().and_then(|g| g.get(idx).cloned()) {
            // Fire-and-forget from the tray (no toast surface); errors logged.
            let _ = crate::terminal::resume_session(&r.source, &r.id, Some(&r.project));
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

    #[test]
    fn select_recent_drops_docs_and_sorts_newest_first() {
        let sessions = vec![
            sess("Claude", "c1", "2026-01-01T00:00:00Z"),
            sess("Codex", "x1", "2026-06-01T00:00:00Z"),
            sess("Memory", "m1", "2026-12-01T00:00:00Z"), // excluded
            sess("Skill", "s1", "2026-12-01T00:00:00Z"),  // excluded
            sess("Gemini", "g1", "2026-03-01T00:00:00Z"),
        ];
        let recent = select_recent(&sessions);
        let ids: Vec<&str> = recent.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["x1", "g1", "c1"]);
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

    #[test]
    fn select_recent_drops_slash_command_sessions() {
        let mut cmd = sess("Claude", "cmd1", "2026-12-01T00:00:00Z");
        cmd.title = "/clear".into(); // newest, but a bare command → dropped
        let mut chat = sess("Codex", "chat1", "2026-06-01T00:00:00Z");
        chat.title = "Fix the flaky test".into();
        let recent = select_recent(&vec![cmd, chat]);
        let ids: Vec<&str> = recent.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["chat1"]);
    }

    #[test]
    fn select_recent_caps_at_limit() {
        let sessions: Vec<AppSession> = (0..10)
            .map(|i| {
                sess(
                    "Claude",
                    &format!("c{i}"),
                    &format!("2026-06-{:02}T00:00:00Z", i + 1),
                )
            })
            .collect();
        assert_eq!(select_recent(&sessions).len(), RECENT_LIMIT);
    }
}
