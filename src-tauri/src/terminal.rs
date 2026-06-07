//! Terminal launching for the tray's "recent sessions → resume" action.
//!
//! `detect()` lists the mainstream terminals actually installed on this OS
//! (the Settings dropdown shows them + "Auto"); `open(id, project, cmd)`
//! launches the chosen one, `cd`-ing into `project` and running `cmd`.
//! Only macOS is verified on the dev machine — Linux / Windows are
//! best-effort. An unknown / "auto" id falls back to the OS default.

use serde::Serialize;
use std::process::Command;

/// One selectable terminal for the Settings dropdown.
#[derive(Serialize)]
pub struct TerminalOption {
    pub id: String,
    pub label: String,
}

fn opt(id: &str, label: &str) -> TerminalOption {
    TerminalOption {
        id: id.into(),
        label: label.into(),
    }
}

/// POSIX single-quote a path for safe embedding in a shell command.
#[cfg(not(target_os = "windows"))]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `cd '<project>' && <cmd>` when a project dir is given, else just `<cmd>`.
#[cfg(not(target_os = "windows"))]
fn with_cd(project: Option<&str>, cmd: &str) -> String {
    match project {
        Some(p) => format!("cd {} && {}", shell_quote(p), cmd),
        None => cmd.to_string(),
    }
}

/// Is `bin` resolvable on PATH?
#[cfg(not(target_os = "windows"))]
fn which(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn spawn(mut c: Command) -> Result<(), String> {
    c.spawn().map(|_| ()).map_err(|err| {
        let msg = format!("couldn't launch the terminal: {err}");
        log::error!("terminal: {msg}");
        msg
    })
}

/// The CLI invocation that resumes session `id` for `source`. Mirrors the
/// frontend `resumeCommandFor` (src/lib/session-utils.ts) — keep in sync. The
/// session id is charset-guarded (`[A-Za-z0-9._-]`) before being interpolated
/// into the shell command (injection defense). `None` for unknown sources.
fn session_launch_command(source: &str, id: &str) -> Option<String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    Some(match source {
        "Claude" => format!("claude --resume {id}"),
        "Codex" => format!("codex resume {id}"),
        "OpenCode" => format!("opencode --session {id}"),
        "Gemini" => format!("gemini --resume {id}"),
        _ => return None,
    })
}

/// Resume a recorded session in the user's chosen terminal (Settings →
/// Terminal, `terminal` config key; empty / "auto" → OS default): build the
/// CLI resume command, then open a terminal `cd`-ed into the session's project
/// dir (when it exists). Shared by the tray click + the `resume_session_in_terminal` IPC.
pub fn resume_session(source: &str, id: &str, project: Option<&str>) -> Result<(), String> {
    let Some(cmd) = session_launch_command(source, id) else {
        return Err("this session can't be resumed by id".to_string());
    };
    let project = project.filter(|p| !p.is_empty() && std::path::Path::new(p).is_dir());
    let choice = crate::config::read_config()
        .ok()
        .and_then(|c| {
            c.get("terminal")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    open(&choice, project, &cmd)
}

// ===================================================================
// macOS
// ===================================================================

#[cfg(target_os = "macos")]
pub fn detect() -> Vec<TerminalOption> {
    use std::path::Path;
    // "auto" IS Terminal.app on macOS — don't list Terminal again separately.
    let mut v = vec![opt("auto", "Default (Terminal)")];
    if Path::new("/Applications/iTerm.app").exists() {
        v.push(opt("iterm", "iTerm"));
    }
    if which("ghostty") || Path::new("/Applications/Ghostty.app").exists() {
        v.push(opt("ghostty", "Ghostty"));
    }
    if which("alacritty") {
        v.push(opt("alacritty", "Alacritty"));
    }
    if which("kitty") {
        v.push(opt("kitty", "Kitty"));
    }
    if which("wezterm") {
        v.push(opt("wezterm", "WezTerm"));
    }
    v
}

#[cfg(target_os = "macos")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    let shell_cmd = with_cd(project, cmd);
    match id {
        "iterm" => {
            let esc = applescript_escape(&shell_cmd);
            // Same cold-launch double-window issue as Terminal.app: launching
            // iTerm opens a default window AND `create window` opens a second.
            // Already running → open a fresh window (don't disturb existing).
            // Cold launch → reuse the window the launch creates; the `delay`
            // lets it appear so the count check doesn't race, and a new window
            // is created if the user's startup pref opens none.
            let script = format!(
                "tell application \"iTerm\"\n  if it is running then\n    create window with default profile command \"{esc}\"\n  else\n    activate\n    delay 0.3\n    if (count of windows) is 0 then\n      create window with default profile command \"{esc}\"\n    else\n      tell current session of current window to write text \"{esc}\"\n    end if\n  end if\nend tell"
            );
            spawn_args("osascript", &["-e", &script])
        }
        "ghostty" => {
            // App-based launch works whether or not the CLI is on PATH.
            let run = format!("{shell_cmd}; exec $SHELL");
            spawn_args(
                "open",
                &["-na", "Ghostty", "--args", "-e", "bash", "-lc", &run],
            )
        }
        "alacritty" => cli("alacritty", &["-e"], &shell_cmd),
        "kitty" => cli("kitty", &[], &shell_cmd),
        "wezterm" => cli("wezterm", &["start", "--"], &shell_cmd),
        // "auto" / "terminal" / unknown → Terminal.app.
        _ => {
            let esc = applescript_escape(&shell_cmd);
            // When Terminal isn't already running, `activate` opens a default
            // empty window AND `do script` (with no target) opens a second —
            // two windows. Launching via `do script … in window 1` reuses the
            // window the launch creates, so we get exactly one. When Terminal
            // is already running, a fresh `do script` opens a new window
            // without disturbing the user's existing ones.
            let script = format!(
                "tell application \"Terminal\"\n  if it is running then\n    do script \"{esc}\"\n  else\n    do script \"{esc}\" in window 1\n  end if\n  activate\nend tell"
            );
            spawn_args("osascript", &["-e", &script])
        }
    }
}

#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ===================================================================
// Linux
// ===================================================================

#[cfg(target_os = "linux")]
pub fn detect() -> Vec<TerminalOption> {
    let mut v = vec![opt("auto", "Default")];
    for (bin, id, label) in [
        ("gnome-terminal", "gnome-terminal", "GNOME Terminal"),
        ("konsole", "konsole", "Konsole"),
        ("xfce4-terminal", "xfce4-terminal", "XFCE Terminal"),
        ("alacritty", "alacritty", "Alacritty"),
        ("kitty", "kitty", "Kitty"),
        ("wezterm", "wezterm", "WezTerm"),
        ("xterm", "xterm", "xterm"),
    ] {
        if which(bin) {
            v.push(opt(id, label));
        }
    }
    v
}

#[cfg(target_os = "linux")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    let shell_cmd = with_cd(project, cmd);
    match id {
        "gnome-terminal" => cli("gnome-terminal", &["--"], &shell_cmd),
        "konsole" => cli("konsole", &["-e"], &shell_cmd),
        "xfce4-terminal" => xfce(&shell_cmd),
        "alacritty" => cli("alacritty", &["-e"], &shell_cmd),
        "kitty" => cli("kitty", &[], &shell_cmd),
        "wezterm" => cli("wezterm", &["start", "--"], &shell_cmd),
        "xterm" => cli("xterm", &["-e"], &shell_cmd),
        // "auto" / unknown → first available emulator.
        _ => {
            let run = format!("{shell_cmd}; exec $SHELL");
            for bin in ["x-terminal-emulator", "gnome-terminal", "xterm"] {
                let mut c = Command::new(bin);
                if bin == "gnome-terminal" {
                    c.args(["--", "bash", "-lc", &run]);
                } else {
                    c.args(["-e", "bash", "-lc", &run]);
                }
                if c.spawn().is_ok() {
                    return Ok(());
                }
            }
            Err("no terminal emulator found".to_string())
        }
    }
}

#[cfg(target_os = "linux")]
fn xfce(shell_cmd: &str) -> Result<(), String> {
    // xfce4-terminal wants the whole command as one `-x`/`--command` string.
    let run = format!(
        "bash -lc '{}; exec $SHELL'",
        shell_cmd.replace('\'', "'\\''")
    );
    spawn_args("xfce4-terminal", &["--command", &run])
}

// ===================================================================
// Shared POSIX CLI launcher (macOS + Linux)
// ===================================================================

#[cfg(not(target_os = "windows"))]
fn cli(bin: &str, pre_args: &[&str], shell_cmd: &str) -> Result<(), String> {
    // Keep the window open after the CLI exits.
    let run = format!("{shell_cmd}; exec $SHELL");
    let mut c = Command::new(bin);
    c.args(pre_args).args(["bash", "-lc", &run]);
    spawn(c)
}

#[cfg(not(target_os = "windows"))]
fn spawn_args(bin: &str, args: &[&str]) -> Result<(), String> {
    let mut c = Command::new(bin);
    c.args(args);
    spawn(c)
}

// ===================================================================
// Windows
// ===================================================================

#[cfg(target_os = "windows")]
pub fn detect() -> Vec<TerminalOption> {
    fn where_(bin: &str) -> bool {
        Command::new("where")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    // "auto" IS cmd on Windows — don't list Command Prompt again separately.
    let mut v = vec![opt("auto", "Default (Command Prompt)")];
    if where_("wt") {
        v.push(opt("wt", "Windows Terminal"));
    }
    if where_("powershell") {
        v.push(opt("powershell", "PowerShell"));
    }
    v
}

#[cfg(target_os = "windows")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    match id {
        "wt" => {
            // Windows Terminal: -d sets the start dir, then the command.
            let mut c = Command::new("wt");
            if let Some(p) = project {
                c.args(["-d", p]);
            }
            c.args(["cmd", "/k", cmd]);
            spawn(c)
        }
        "powershell" => {
            let full = match project {
                Some(p) => format!("cd '{}'; {}", p.replace('\'', "''"), cmd),
                None => cmd.to_string(),
            };
            let mut c = Command::new("powershell");
            c.args(["-NoExit", "-Command", &full]);
            spawn(c)
        }
        // "auto" / "cmd" / unknown → Command Prompt.
        _ => {
            let full = match project {
                Some(p) => format!("cd /d \"{p}\" && {cmd}"),
                None => cmd.to_string(),
            };
            let mut c = Command::new("cmd");
            c.args(["/C", "start", "cmd", "/K", &full]);
            spawn(c)
        }
    }
}

// ===================================================================
// Fallback (unsupported OS)
// ===================================================================

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn detect() -> Vec<TerminalOption> {
    vec![opt("auto", "Default")]
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn open(_id: &str, _project: Option<&str>, _cmd: &str) -> Result<(), String> {
    Err("opening a terminal is unsupported on this OS".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_always_offers_auto() {
        let opts = detect();
        assert_eq!(opts.first().map(|o| o.id.as_str()), Some("auto"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn with_cd_quotes_the_project() {
        assert_eq!(with_cd(Some("/a b"), "run"), "cd '/a b' && run");
        assert_eq!(with_cd(None, "run"), "run");
        // A single quote in the path is escaped, not broken out of.
        assert_eq!(with_cd(Some("/a'b"), "run"), "cd '/a'\\''b' && run");
    }

    #[test]
    fn session_launch_command_per_source() {
        assert_eq!(
            session_launch_command("Claude", "u-1").as_deref(),
            Some("claude --resume u-1")
        );
        assert_eq!(
            session_launch_command("Codex", "t-2").as_deref(),
            Some("codex resume t-2")
        );
        assert_eq!(
            session_launch_command("OpenCode", "s-3").as_deref(),
            Some("opencode --session s-3")
        );
        assert_eq!(
            session_launch_command("Gemini", "g-4").as_deref(),
            Some("gemini --resume g-4")
        );
        assert_eq!(session_launch_command("Memory", "x"), None);
        assert_eq!(session_launch_command("whatever", "x"), None);
    }

    #[test]
    fn session_launch_command_rejects_unsafe_ids() {
        for bad in ["a; rm -rf ~", "a b", "$(whoami)", "a`id`", "a|b", ""] {
            assert_eq!(session_launch_command("Claude", bad), None, "id={bad:?}");
        }
        assert!(session_launch_command("Claude", "a1b2c3d4-e5f6-7890-abcd-ef1234567890").is_some());
    }
}
