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
    session_launch_command_with(source, id, &codex_shell_invocation())
}

fn session_launch_command_with(source: &str, id: &str, codex: &str) -> Option<String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    Some(match source {
        "Claude" => format!("claude --resume {id}"),
        "Codex" => format!("{codex} resume {id}"),
        "OpenCode" => format!("opencode --session {id}"),
        "Gemini" => format!("gemini --resume {id}"),
        // Verified against `grok --help` (0.2.93): `-r, --resume [<SESSION_ID>]`.
        "Grok" => format!("grok --resume {id}"),
        _ => return None,
    })
}

/// The shell text that invokes the codex CLI in the user's terminal:
/// bare `codex` when the standalone CLI is installed (the login shell
/// resolves it from PATH), else the desktop app's bundled binary by
/// single-quoted absolute path (app-only installs have nothing on
/// PATH). Neither present → bare `codex` (the shell will report the
/// real "command not found" instead of us guessing). The bundled
/// branch is macOS-only because `codex_bundled_cli` statically returns
/// None everywhere else — no Windows quoting path exists (or is
/// needed) here.
fn codex_shell_invocation() -> String {
    if crate::providers::find_cli_binary("codex").is_some() {
        return "codex".to_string();
    }
    #[cfg(not(target_os = "windows"))]
    if let Some(path) = crate::providers::codex_bundled_cli() {
        return shell_quote(&path.to_string_lossy());
    }
    "codex".to_string()
}

/// The user's Settings → Terminal choice (`terminal` config key);
/// empty / "auto" → the OS default terminal.
fn configured_terminal() -> String {
    crate::config::read_config()
        .ok()
        .and_then(|c| {
            c.get("terminal")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Open `cmd` in the chosen terminal, `cd`-ed into `project` when that
/// dir exists.
fn open_in_project(project: Option<&str>, cmd: &str) -> Result<(), String> {
    let project = project.filter(|p| !p.is_empty() && std::path::Path::new(p).is_dir());
    open(&configured_terminal(), project, cmd)
}

/// Resume a recorded session in the user's chosen terminal: build the
/// CLI resume command, then open a terminal `cd`-ed into the session's project
/// dir (when it exists). Shared by the tray click + the `resume_session_in_terminal` IPC.
pub fn resume_session(source: &str, id: &str, project: Option<&str>) -> Result<(), String> {
    let Some(cmd) = session_launch_command(source, id) else {
        return Err("this session can't be resumed by id".to_string());
    };
    open_in_project(project, &cmd)
}

/// The bare CLI invocation that starts a NEW session — the binary name
/// (or, for an app-only Codex install, the bundled binary's quoted
/// path). `None` for unknown sources.
fn new_session_command(source: &str) -> Option<String> {
    new_session_command_with(source, &codex_shell_invocation())
}

fn new_session_command_with(source: &str, codex: &str) -> Option<String> {
    Some(match source {
        "Claude" => "claude".to_string(),
        "Codex" => codex.to_string(),
        "OpenCode" => "opencode".to_string(),
        "Gemini" => "gemini".to_string(),
        _ => return None,
    })
}

/// Start a NEW session for `source` in the user's chosen terminal,
/// `cd`-ed into the project dir. Driven by the tray's per-project
/// "New session" entry.
pub fn new_session(source: &str, project: Option<&str>) -> Result<(), String> {
    let Some(cmd) = new_session_command(source) else {
        return Err("unknown source".to_string());
    };
    open_in_project(project, &cmd)
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
            spawn_args("osascript", &["-e", &iterm_script(&esc)])
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
            spawn_args("osascript", &["-e", &terminal_app_script(&esc)])
        }
    }
}

#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// AppleScript that runs `esc` (an already-escaped command) in Terminal.app.
/// When Terminal isn't already running, `activate` opens a default empty
/// window AND a bare `do script` opens a SECOND — two windows, one unused.
/// Running it `in window 1` reuses the window the launch creates, so we get
/// exactly one. When Terminal is already running, a fresh `do script` opens a
/// new window without disturbing the user's existing ones.
#[cfg(target_os = "macos")]
fn terminal_app_script(esc: &str) -> String {
    format!(
        "tell application \"Terminal\"\n  if it is running then\n    do script \"{esc}\"\n  else\n    do script \"{esc}\" in window 1\n  end if\n  activate\nend tell"
    )
}

/// AppleScript that runs `esc` in iTerm. Same cold-launch double-window issue
/// as Terminal.app: launching iTerm opens a default window AND `create window`
/// opens a second. Already running → a fresh window (don't disturb existing);
/// cold launch → reuse the window the launch creates (the `delay` lets it
/// appear so the count check doesn't race; a new window is created if the
/// user's startup preference opens none).
#[cfg(target_os = "macos")]
fn iterm_script(esc: &str) -> String {
    format!(
        "tell application \"iTerm\"\n  if it is running then\n    create window with default profile command \"{esc}\"\n  else\n    activate\n    delay 0.3\n    if (count of windows) is 0 then\n      create window with default profile command \"{esc}\"\n    else\n      tell current session of current window to write text \"{esc}\"\n    end if\n  end if\nend tell"
    )
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

/// A (program, args) pair for a Linux terminal launch — cfg-free so
/// the construction is unit-testable off-Linux (same rationale as
/// `WindowsLaunch` below; the Linux runtime never runs on the dev
/// machine).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct LinuxLaunch {
    program: &'static str,
    args: Vec<String>,
}

/// `<pre_args…> bash -lc '<shell_cmd>; exec $SHELL'` — the arg vector
/// every bash-launching terminal takes (they differ only in pre_args).
/// `exec $SHELL` keeps the window open after the CLI exits.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn bash_lc_args(pre_args: &[&str], shell_cmd: &str) -> Vec<String> {
    let run = format!("{shell_cmd}; exec $SHELL");
    let mut args: Vec<String> = pre_args.iter().map(|s| s.to_string()).collect();
    args.extend(["bash".to_string(), "-lc".to_string(), run]);
    args
}

/// Pure command construction for a KNOWN Linux terminal id; `None` for
/// "auto"/unknown (the caller walks `linux_fallback_commands`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_open_command(id: &str, shell_cmd: &str) -> Option<LinuxLaunch> {
    let launch = |program: &'static str, pre: &[&str]| LinuxLaunch {
        program,
        args: bash_lc_args(pre, shell_cmd),
    };
    Some(match id {
        "gnome-terminal" => launch("gnome-terminal", &["--"]),
        "konsole" => launch("konsole", &["-e"]),
        // xfce4-terminal wants the whole command as one `--command`
        // string; single quotes in the payload are POSIX-escaped.
        "xfce4-terminal" => LinuxLaunch {
            program: "xfce4-terminal",
            args: vec![
                "--command".to_string(),
                format!(
                    "bash -lc '{}; exec $SHELL'",
                    shell_cmd.replace('\'', "'\\''")
                ),
            ],
        },
        "alacritty" => launch("alacritty", &["-e"]),
        "kitty" => launch("kitty", &[]),
        "wezterm" => launch("wezterm", &["start", "--"]),
        "xterm" => launch("xterm", &["-e"]),
        _ => return None,
    })
}

/// The "auto"/unknown fallback candidates, tried in order until one
/// spawns.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_fallback_commands(shell_cmd: &str) -> Vec<LinuxLaunch> {
    ["x-terminal-emulator", "gnome-terminal", "xterm"]
        .into_iter()
        .map(|program| LinuxLaunch {
            program,
            args: bash_lc_args(
                if program == "gnome-terminal" {
                    &["--"]
                } else {
                    &["-e"]
                },
                shell_cmd,
            ),
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    let shell_cmd = with_cd(project, cmd);
    if let Some(launch) = linux_open_command(id, &shell_cmd) {
        let mut c = Command::new(launch.program);
        c.args(&launch.args);
        return spawn(c);
    }
    // "auto" / unknown → first available emulator.
    for launch in linux_fallback_commands(&shell_cmd) {
        let mut c = Command::new(launch.program);
        c.args(&launch.args);
        if c.spawn().is_ok() {
            return Ok(());
        }
    }
    Err("no terminal emulator found".to_string())
}

// ===================================================================
// Shared POSIX CLI launcher (macOS)
// ===================================================================

#[cfg(target_os = "macos")]
fn cli(bin: &str, pre_args: &[&str], shell_cmd: &str) -> Result<(), String> {
    let mut c = Command::new(bin);
    c.args(bash_lc_args(pre_args, shell_cmd));
    spawn(c)
}

#[cfg(target_os = "macos")]
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
        let mut c = Command::new("where");
        c.arg(bin);
        // Silent probe — without this a console window flashes for
        // every `where` invocation (GUI-subsystem parent).
        crate::providers::hide_console(&mut c);
        c.output().map(|o| o.status.success()).unwrap_or(false)
    }
    // "auto" IS cmd on Windows — don't list Command Prompt again separately.
    let mut v = vec![opt("auto", "Default (cmd)")];
    if where_("wt") {
        v.push(opt("wt", "Windows Terminal"));
    }
    if where_("powershell") {
        v.push(opt("powershell", "PowerShell"));
    }
    v
}

/// The (program, args) for a Windows terminal launch, plus whether the
/// spawned HELPER process's console must be hidden (the `cmd /C start`
/// branch — `start` opens the real terminal window; the outer helper's
/// own console would otherwise flash first).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct WindowsLaunch {
    program: &'static str,
    args: Vec<String>,
    /// Working directory for the SPAWN (the `cmd /C start` branch —
    /// `start`'s new console inherits it). The directory deliberately
    /// does NOT travel inside the command string: a nested
    /// `cd /d "path"` inside `cmd /C start cmd /K "…"` hits the
    /// MSVCRT-vs-cmd quoting mismatch (Rust escapes inner quotes as
    /// `\"`, cmd.exe doesn't honor backslash escapes), which mangled
    /// every picked path on real Windows hardware.
    cwd: Option<String>,
    hide_helper_console: bool,
}

/// Pure command construction for the Windows terminal launch — cfg-free
/// so it unit-tests on every dev OS (mirror of the
/// `windows_claude_dir_matches` precedent; the Windows runtime is the
/// path that never runs locally). The spawn site (`open`) stays gated.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_open_command(id: &str, project: Option<&str>, cmd: &str) -> WindowsLaunch {
    match id {
        "wt" => {
            // Windows Terminal: -d sets the start dir, then the command.
            let mut args: Vec<String> = Vec::new();
            if let Some(p) = project {
                args.extend(["-d".to_string(), p.to_string()]);
            }
            args.extend(["cmd".to_string(), "/k".to_string(), cmd.to_string()]);
            WindowsLaunch {
                program: "wt",
                args,
                cwd: None,
                hide_helper_console: false,
            }
        }
        "powershell" => {
            let full = match project {
                Some(p) => format!("cd '{}'; {}", p.replace('\'', "''"), cmd),
                None => cmd.to_string(),
            };
            WindowsLaunch {
                program: "powershell",
                args: vec!["-NoExit".to_string(), "-Command".to_string(), full],
                cwd: None,
                hide_helper_console: false,
            }
        }
        // "auto" / "cmd" / unknown → Command Prompt. The project dir
        // rides as the spawn cwd (see WindowsLaunch.cwd), NOT as a
        // `cd /d` inside the command string.
        _ => WindowsLaunch {
            program: "cmd",
            args: vec![
                "/C".to_string(),
                "start".to_string(),
                "cmd".to_string(),
                "/K".to_string(),
                cmd.to_string(),
            ],
            cwd: project.map(str::to_string),
            hide_helper_console: true,
        },
    }
}

#[cfg(target_os = "windows")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    let launch = windows_open_command(id, project, cmd);
    let mut c = Command::new(launch.program);
    c.args(&launch.args);
    if let Some(dir) = &launch.cwd {
        c.current_dir(dir);
    }
    if launch.hide_helper_console {
        crate::providers::hide_console(&mut c);
    }
    spawn(c)
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

    // linux_open_command / linux_fallback_commands are cfg-free so
    // these run on every dev OS — the Linux launch path otherwise only
    // executes on real Linux hardware.
    #[test]
    fn linux_open_command_builds_known_terminal_launches() {
        let l = linux_open_command("gnome-terminal", "cd '/p' && claude").unwrap();
        assert_eq!(l.program, "gnome-terminal");
        assert_eq!(
            l.args,
            vec!["--", "bash", "-lc", "cd '/p' && claude; exec $SHELL"]
        );
        // kitty takes no pre-args; wezterm needs `start --`.
        let l = linux_open_command("kitty", "claude").unwrap();
        assert_eq!(l.args, vec!["bash", "-lc", "claude; exec $SHELL"]);
        let l = linux_open_command("wezterm", "claude").unwrap();
        assert_eq!(
            l.args,
            vec!["start", "--", "bash", "-lc", "claude; exec $SHELL"]
        );
        // auto / unknown → fallback list, not a single launch.
        assert!(linux_open_command("auto", "claude").is_none());
    }

    #[test]
    fn linux_open_command_xfce_wraps_the_whole_command_and_escapes_quotes() {
        let l = linux_open_command("xfce4-terminal", "echo 'hi'").unwrap();
        assert_eq!(l.program, "xfce4-terminal");
        assert_eq!(
            l.args,
            vec!["--command", "bash -lc 'echo '\\''hi'\\''; exec $SHELL'"]
        );
    }

    #[test]
    fn linux_fallback_commands_try_three_emulators_in_order() {
        let cands = linux_fallback_commands("claude");
        assert_eq!(
            cands.iter().map(|c| c.program).collect::<Vec<_>>(),
            vec!["x-terminal-emulator", "gnome-terminal", "xterm"]
        );
        assert_eq!(
            cands[0].args,
            vec!["-e", "bash", "-lc", "claude; exec $SHELL"]
        );
        // gnome-terminal separates its command with `--`, not `-e`.
        assert_eq!(cands[1].args[0], "--");
        assert_eq!(cands[2].args[0], "-e");
    }

    // windows_open_command is deliberately cfg-free so these run on
    // every dev OS — the Windows launch path otherwise only executes
    // on real Windows hardware.
    #[test]
    fn windows_open_command_wt_sets_start_dir_then_command() {
        let l = windows_open_command("wt", Some("C:\\proj"), "codex resume x");
        assert_eq!(l.program, "wt");
        assert_eq!(
            l.args,
            vec!["-d", "C:\\proj", "cmd", "/k", "codex resume x"]
        );
        assert!(!l.hide_helper_console);
        // No project → no -d pair.
        let l = windows_open_command("wt", None, "codex");
        assert_eq!(l.args, vec!["cmd", "/k", "codex"]);
    }

    #[test]
    fn windows_open_command_powershell_escapes_single_quotes() {
        let l = windows_open_command("powershell", Some("C:\\o'brien"), "claude");
        assert_eq!(l.program, "powershell");
        assert_eq!(
            l.args,
            vec!["-NoExit", "-Command", "cd 'C:\\o''brien'; claude"]
        );
        assert!(!l.hide_helper_console);
    }

    #[test]
    fn windows_open_command_default_uses_start_with_spawn_cwd() {
        let l = windows_open_command("auto", Some("C:\\My Docs\\proj"), "claude");
        assert_eq!(l.program, "cmd");
        // The project dir must NOT appear inside the command string — a
        // nested `cd /d "…"` hits the MSVCRT-vs-cmd quoting mismatch
        // and mangled real paths (the original Windows bug). It rides
        // as the spawn cwd; `start`'s new console inherits it.
        assert_eq!(l.args, vec!["/C", "start", "cmd", "/K", "claude"]);
        assert_eq!(l.cwd.as_deref(), Some("C:\\My Docs\\proj"));
        // The OUTER cmd /C helper's console must be hidden — `start`
        // opens the real terminal window.
        assert!(l.hide_helper_console);
        let l = windows_open_command("cmd", None, "claude");
        assert_eq!(l.args, vec!["/C", "start", "cmd", "/K", "claude"]);
        assert_eq!(l.cwd, None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn with_cd_quotes_the_project() {
        assert_eq!(with_cd(Some("/a b"), "run"), "cd '/a b' && run");
        assert_eq!(with_cd(None, "run"), "run");
        // A single quote in the path is escaped, not broken out of.
        assert_eq!(with_cd(Some("/a'b"), "run"), "cd '/a'\\''b' && run");
    }

    // The double-window fix lives in the AppleScript: on a cold launch the
    // command must reuse the window the launch creates, not open a second.
    #[cfg(target_os = "macos")]
    #[test]
    fn terminal_app_script_reuses_launch_window_on_cold_start() {
        let s = terminal_app_script("echo hi");
        assert!(s.contains("if it is running then"), "{s}");
        // Cold launch → run in window 1 (reuse the launch's default window).
        assert!(s.contains("do script \"echo hi\" in window 1"), "{s}");
        // Already running → a fresh `do script` (its own new window).
        assert!(s.contains("    do script \"echo hi\"\n  else"), "{s}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn iterm_script_reuses_launch_window_on_cold_start() {
        let s = iterm_script("echo hi");
        assert!(s.contains("if it is running then"), "{s}");
        // Cold launch → reuse the launch window instead of a second one.
        assert!(s.contains("(count of windows) is 0"), "{s}");
        assert!(
            s.contains("tell current session of current window to write text \"echo hi\""),
            "{s}"
        );
    }

    // The `_with` variants take the codex invocation explicitly so the
    // asserts don't depend on what's installed on the test machine
    // (the public fns resolve it live via `codex_shell_invocation`).
    #[test]
    fn session_launch_command_per_source() {
        assert_eq!(
            session_launch_command_with("Claude", "u-1", "codex").as_deref(),
            Some("claude --resume u-1")
        );
        assert_eq!(
            session_launch_command_with("Codex", "t-2", "codex").as_deref(),
            Some("codex resume t-2")
        );
        assert_eq!(
            session_launch_command_with("OpenCode", "s-3", "codex").as_deref(),
            Some("opencode --session s-3")
        );
        assert_eq!(
            session_launch_command_with("Gemini", "g-4", "codex").as_deref(),
            Some("gemini --resume g-4")
        );
        assert_eq!(session_launch_command_with("Memory", "x", "codex"), None);
        assert_eq!(session_launch_command_with("whatever", "x", "codex"), None);
        // App-only Codex install — the bundled binary's quoted path is
        // the invocation; only the Codex arm uses it.
        let bundled = "'/Applications/ChatGPT.app/Contents/Resources/codex'";
        assert_eq!(
            session_launch_command_with("Codex", "t-2", bundled).as_deref(),
            Some("'/Applications/ChatGPT.app/Contents/Resources/codex' resume t-2")
        );
        assert_eq!(
            session_launch_command_with("Claude", "u-1", bundled).as_deref(),
            Some("claude --resume u-1")
        );
    }

    #[test]
    fn new_session_command_per_source() {
        assert_eq!(
            new_session_command_with("Claude", "codex").as_deref(),
            Some("claude")
        );
        assert_eq!(
            new_session_command_with("Codex", "codex").as_deref(),
            Some("codex")
        );
        assert_eq!(
            new_session_command_with("OpenCode", "codex").as_deref(),
            Some("opencode")
        );
        assert_eq!(
            new_session_command_with("Gemini", "codex").as_deref(),
            Some("gemini")
        );
        assert_eq!(new_session_command_with("Memory", "codex"), None);
        assert_eq!(
            new_session_command_with("Codex", "'/x/y z/codex'").as_deref(),
            Some("'/x/y z/codex'")
        );
    }

    #[test]
    fn session_launch_command_rejects_unsafe_ids() {
        for bad in ["a; rm -rf ~", "a b", "$(whoami)", "a`id`", "a|b", ""] {
            assert_eq!(
                session_launch_command_with("Claude", bad, "codex"),
                None,
                "id={bad:?}"
            );
        }
        assert!(session_launch_command_with(
            "Claude",
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "codex"
        )
        .is_some());
    }
}
