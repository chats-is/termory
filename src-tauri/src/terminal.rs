//! Terminal launching for the tray's "recent sessions → resume" action.
//!
//! Three steps, and nothing else: `detect()` lists what's installed on
//! this OS (plus a "Default" row), the user's pick is read from the
//! `terminal` config key, and `open(id, project, cmd)` launches it,
//! `cd`-ing into `project` and running `cmd`. An unrecognized or empty
//! id — including "auto" — is the Default row and falls back to the OS's
//! own choice.
//!
//! Per-OS shape differs in what the list MEANS: on macOS and Linux the
//! rows are terminal apps (the shell is always the user's `$SHELL`), on
//! Windows they are SHELLS, because Windows has no `$SHELL` equivalent
//! and the terminal app comes from its own OS setting instead.
//!
//! Each platform's argv construction is a pure function
//! (`mac_open_args` / `linux_open_command` / `windows_open_command`) so
//! the per-terminal flags are unit-testable on any dev machine; only the
//! spawn is `#[cfg]`-gated. Only macOS is verified on real hardware —
//! Linux / Windows are best-effort.

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

/// Is `bin` resolvable on PATH? Linux only — macOS detects by `.app`
/// (see [`mac_app_installed`]) because a Dock-launched process inherits
/// launchd's bare PATH, where none of these terminals appear.
#[cfg(target_os = "linux")]
fn which(bin: &str) -> bool {
    let mut c = Command::new("which");
    c.arg(bin);
    crate::process::probe(c, crate::process::PROBE_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Launch a terminal for the user.
///
/// Goes through [`crate::process::spawn_detached`], which reaps the child
/// but never kills it. Both halves matter here:
///
/// - **Reaped** — this used to drop the `Child` on the spot, and
///   `std::process::Child::drop` does not wait, so every "Resume in
///   terminal" / "New session" click left a zombie for the lifetime of
///   the app.
/// - **Never killed** — the user is about to work in this terminal, so it
///   must outlive Termory. It is deliberately absent from the shutdown
///   registry; do not "fix" that by making it managed.
fn spawn(c: Command) -> Result<(), String> {
    crate::process::spawn_detached(c)
        .map(|_| ())
        .map_err(|err| {
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
        "Grok" => "grok".to_string(),
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

/// A macOS terminal the picker can offer: the `.app` name (both the
/// detection check and what `open` launches) and the flags that precede
/// the command. One definition drives detection and launch alike — the
/// app name used to be written here AND again as a literal in `open()`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct MacTerminal {
    id: &'static str,
    label: &'static str,
    app: &'static str,
    pre_args: &'static [&'static str],
}

/// App names from Claude Code's own launcher
/// (deepLink/terminalLauncher.ts `MACOS_TERMINALS`) — note kitty's app
/// bundle is lowercase `kitty.app`. Order is the picker's display order.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MAC_TERMINALS: &[MacTerminal] = &[
    // iTerm has no argv interface — it's driven by AppleScript, so its
    // pre-args are unused and `open()` handles it by id.
    MacTerminal {
        id: "iterm",
        label: "iTerm",
        app: "iTerm",
        pre_args: &[],
    },
    MacTerminal {
        id: "ghostty",
        label: "Ghostty",
        app: "Ghostty",
        pre_args: &["-e"],
    },
    MacTerminal {
        id: "alacritty",
        label: "Alacritty",
        app: "Alacritty",
        pre_args: &["-e"],
    },
    MacTerminal {
        id: "kitty",
        label: "Kitty",
        app: "kitty",
        pre_args: &[],
    },
    MacTerminal {
        id: "wezterm",
        label: "WezTerm",
        app: "WezTerm",
        pre_args: &["start", "--"],
    },
];

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_terminal(id: &str) -> Option<&'static MacTerminal> {
    MAC_TERMINALS.iter().find(|t| t.id == id)
}

/// Is this terminal's `.app` installed? Two `exists()` checks, no
/// subprocess.
///
/// This replaced a `which` probe, which missed a GUI-only install:
/// Kitty, Alacritty and WezTerm keep their CLI inside the bundle and only
/// land on PATH if the user links it, so those three were undetectable
/// however plainly they sat in `/Applications`. A `which` probe is also
/// useless in the shipped app — a Dock-launched process inherits
/// launchd's bare PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), the measured
/// fact recorded in CLAUDE.md — so nothing is lost by dropping it, and
/// detection now matches how `open()` launches: by the bundle.
#[cfg(target_os = "macos")]
fn mac_app_installed(app: &str) -> bool {
    use std::path::Path;
    let name = format!("{app}.app");
    if Path::new("/Applications").join(&name).exists() {
        return true;
    }
    crate::home_dir().is_some_and(|home| home.join("Applications").join(&name).exists())
}

#[cfg(target_os = "macos")]
pub fn detect() -> Vec<TerminalOption> {
    // "auto" is the system default, which on macOS IS Terminal.app —
    // `open`'s fallback branch lands there, so don't list it again.
    let mut v = vec![opt("auto", "Default")];
    for t in MAC_TERMINALS {
        if mac_app_installed(t.app) {
            v.push(opt(t.id, t.label));
        }
    }
    v
}

#[cfg(target_os = "macos")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    let shell_cmd = with_cd(project, cmd);
    match mac_terminal(id) {
        // AppleScript has no argv interface, so iTerm can't go through
        // the table-driven path below.
        Some(t) if t.id == "iterm" => {
            let esc = applescript_escape(&shell_cmd);
            spawn_args("osascript", &["-e", &iterm_script(&esc)])
        }
        // Everything else launches from its own row — no app name is
        // written a second time here.
        Some(t) => open_app(t, &shell_cmd),
        // "auto" / unknown → Terminal.app.
        None => {
            let esc = applescript_escape(&shell_cmd);
            spawn_args("osascript", &["-e", &terminal_app_script(&esc)])
        }
    }
}

#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A leading SPACE on every command we TYPE into a shell — Terminal.app's
/// `do script` and iTerm's `write text`. Both deliver the command as
/// KEYSTROKES, so it lands in the tty buffer while the shell is still
/// sourcing its rc files, and **an rc file that reads a keystroke eats the
/// first character of our command**.
///
/// The reported case is oh-my-zsh's update prompt (`tools/check_for_upgrade.sh:235`,
/// the DEFAULT `prompt` mode, fired every 13 days): `read -r -k 1 option` takes
/// the `c` of `cd '<project>' && claude`, leaving the shell to run
/// `d '<project>' && claude` → `zsh: command not found: d`, and the CLI never
/// starts. omz's own `has_typed_input` guard only covers text that already
/// arrived BEFORE the check, which is a race we lose whenever the window is
/// created slightly ahead of the keystrokes.
///
/// A space is the one prefix that is harmless in BOTH outcomes: omz's `read`
/// sees it and falls to the `*)` arm (anything but `y`/`Y`/Enter means don't
/// update), so our command survives intact; with no prompt the shell just
/// ignores a leading blank — and under `HIST_IGNORE_SPACE` it also keeps this
/// injected command out of the user's history, which is if anything an
/// improvement. **Control characters do NOT work here** — `^U` was tried first
/// and is the tty's kill character, swallowed by the line discipline before any
/// reader sees it (measured under a pty: the prompt still ate the `c`).
///
/// The argv-based launchers need no prefix and must not get one: Ghostty /
/// Alacritty / Kitty / WezTerm and every Linux and Windows branch pass the
/// command as an ARGUMENT, where nothing can consume it.
#[cfg(target_os = "macos")]
fn typed(esc: &str) -> String {
    format!(" {esc}")
}

/// AppleScript that runs `esc` (an already-escaped command) in Terminal.app.
/// When Terminal isn't already running, `activate` opens a default empty
/// window AND a bare `do script` opens a SECOND — two windows, one unused.
/// Running it `in window 1` reuses the window the launch creates, so we get
/// exactly one. When Terminal is already running, a fresh `do script` opens a
/// new window without disturbing the user's existing ones.
#[cfg(target_os = "macos")]
fn terminal_app_script(esc: &str) -> String {
    // `do script` TYPES the text — see `typed` for why it carries a space.
    let esc = typed(esc);
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
///
/// Only the `write text` branch carries the [`typed`] space: it is the one that
/// sends KEYSTROKES to a shell. `create window … command` hands iTerm the
/// command to RUN as the window's process, so no rc file is between us and it.
#[cfg(target_os = "macos")]
fn iterm_script(esc: &str) -> String {
    let keys = typed(esc);
    format!(
        "tell application \"iTerm\"\n  if it is running then\n    create window with default profile command \"{esc}\"\n  else\n    activate\n    delay 0.3\n    if (count of windows) is 0 then\n      create window with default profile command \"{esc}\"\n    else\n      tell current session of current window to write text \"{keys}\"\n    end if\n  end if\nend tell"
    )
}

// ===================================================================
// Linux
// ===================================================================

/// The Linux terminals the picker can offer: `(binary, id, label)`.
///
/// A const rather than an inline array so the list has ONE definition:
/// `detect()` builds the picker from it and
/// `every_linux_list_entry_has_a_launch_arm` walks the same rows, so an
/// entry added here without a `linux_open_command` arm fails the test.
/// It used to be inline, with the test hand-copying the ids — a third
/// copy that could agree with neither.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_TERMINALS: &[(&str, &str, &str)] = &[
    ("gnome-terminal", "gnome-terminal", "GNOME Terminal"),
    ("konsole", "konsole", "Konsole"),
    ("xfce4-terminal", "xfce4-terminal", "XFCE Terminal"),
    ("mate-terminal", "mate-terminal", "MATE Terminal"),
    ("tilix", "tilix", "Tilix"),
    ("ghostty", "ghostty", "Ghostty"),
    ("alacritty", "alacritty", "Alacritty"),
    ("kitty", "kitty", "Kitty"),
    ("wezterm", "wezterm", "WezTerm"),
    ("xterm", "xterm", "xterm"),
];

#[cfg(target_os = "linux")]
pub fn detect() -> Vec<TerminalOption> {
    let mut v = vec![opt("auto", "Default")];
    for &(bin, id, label) in LINUX_TERMINALS {
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
        // mate-terminal's `-x` takes the rest of the line, which is what
        // our bash argv vector needs (Claude Code's own launcher uses the
        // same flag, deepLink/terminalLauncher.ts `launchLinuxTerminal`).
        "mate-terminal" => launch("mate-terminal", &["-x"]),
        // Tilix takes ONE string, like xfce4-terminal above — its `-e` is
        // registered `GOptionArg.STRING` (application.d) and `cmdparams.d`
        // never joins leftover argv back onto it, so `-e bash -lc <script>`
        // gives it just `bash` and leaves `-lc` as an unknown option. Its
        // man page says the opposite ("all text after this parameter"),
        // and the reference launcher passes an argv vector here, so BOTH
        // are wrong; the source wins. Not run against a real tilix — but
        // the single-string form is correct either way, which is why it's
        // safe without one.
        "tilix" => LinuxLaunch {
            program: "tilix",
            args: vec![
                "-e".to_string(),
                format!(
                    "bash -lc '{}; exec $SHELL'",
                    shell_cmd.replace('\'', "'\\''")
                ),
            ],
        },
        "ghostty" => launch("ghostty", &["-e"]),
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
        // Through `spawn` like every other branch — spawning directly
        // here is what left a zombie behind for each emulator tried.
        if spawn(c).is_ok() {
            return Ok(());
        }
    }
    Err("no terminal emulator found".to_string())
}

/// Launch a terminal from its table row: `open -na <App> --args
/// <pre_args> bash -lc '<cmd>; exec $SHELL'` — the shape the Ghostty arm
/// has always used, and the one Claude Code's own launcher uses for all
/// four of these (`launchMacosTerminal`, which likewise passes `-e` /
/// `start` inside `--args`).
///
/// There is deliberately no CLI branch: a Dock-launched process inherits
/// launchd's bare PATH (`/usr/bin:/bin:/usr/sbin:/sbin`, the measured
/// fact in CLAUDE.md), so a `which` fallback could only ever fire in
/// `npm run tauri:dev` — i.e. the branch under test would never be the
/// branch users get.
#[cfg(target_os = "macos")]
fn open_app(t: &MacTerminal, shell_cmd: &str) -> Result<(), String> {
    let mut c = Command::new("open");
    c.args(mac_open_args(t, shell_cmd));
    spawn(c)
}

/// The argv for that launch — cfg-free so the four per-terminal forms are
/// unit-testable on any dev OS, the same reason `linux_open_command` and
/// `windows_open_command` are pure. Getting a terminal's flags wrong is
/// exactly how the Tilix row shipped broken.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_open_args(t: &MacTerminal, shell_cmd: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["-na".into(), t.app.into(), "--args".into()];
    args.extend(bash_lc_args(t.pre_args, shell_cmd));
    args
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

/// Is `bin` resolvable on PATH? (Windows counterpart of `which`.)
#[cfg(target_os = "windows")]
fn where_(bin: &str) -> bool {
    let mut c = Command::new("where");
    c.arg(bin);
    // Timed + silent: `process::probe` applies the no-console-window
    // flag, without which one flashes for every `where` invocation
    // (GUI-subsystem parent).
    crate::process::probe(c, crate::process::PROBE_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The shell a row that does NOT name one hosts — `Default` and
/// `Terminal`. Windows has no `$SHELL` equivalent to read (`%COMSPEC%`
/// is the constant `cmd.exe`, not a preference), so unlike macOS/Linux
/// this cannot follow the user's configuration and has to be chosen.
/// Newest PowerShell first, `cmd` only on a box that has neither.
#[cfg(target_os = "windows")]
fn default_shell() -> WinShell {
    if where_("pwsh") {
        WinShell::Pwsh
    } else if where_("powershell") {
        WinShell::PowerShell
    } else {
        WinShell::Cmd
    }
}

#[cfg(target_os = "windows")]
pub fn detect() -> Vec<TerminalOption> {
    // This list is SHELLS, not terminal apps — the terminal app is the
    // OS's own "default terminal application" setting, which `start`
    // honors for every row (see `windows_open_command`). Naming a
    // terminal here would override the one thing the user configured for
    // himself, so no row does.
    let mut v = vec![opt("auto", "Default")];
    // `pwsh` (PowerShell 7+) and `powershell` (the built-in 5.1) are
    // SEPARATE binaries that can both be installed, so both are probed;
    // the newer one is listed first. Only 7+ carries a version in its
    // label — the bare name is the one the user knows.
    if where_("pwsh") {
        v.push(opt("pwsh", "PowerShell 7"));
    }
    if where_("powershell") {
        v.push(opt("powershell", "PowerShell"));
    }
    // Unconditional: cmd.exe is part of Windows.
    v.push(opt("cmd", "CMD"));
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

/// A shell for the rows that don't name one. WT's positional commandline
/// REPLACES the profile's shell rather than running inside it (Microsoft's
/// `--appendCommandLine` is documented as appending "instead of replacing
/// it"), and passing NO commandline means our resume command never runs —
/// so one has to be named. `-NoExit` / `/K` keep the prompt afterwards, the
/// Windows counterpart of the `exec $SHELL` the unix branches append.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
enum WinShell {
    Pwsh,
    PowerShell,
    Cmd,
}

impl WinShell {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn args(self, cmd: &str) -> Vec<String> {
        let argv: &[&str] = match self {
            WinShell::Pwsh => &["pwsh", "-NoExit", "-Command"],
            WinShell::PowerShell => &["powershell", "-NoExit", "-Command"],
            WinShell::Cmd => &["cmd", "/K"],
        };
        argv.iter()
            .map(|a| (*a).to_string())
            .chain([cmd.to_string()])
            .collect()
    }
}

/// Pure command construction for the Windows terminal launch — cfg-free
/// so it unit-tests on every dev OS (mirror of the
/// `windows_claude_dir_matches` precedent; the Windows runtime is the
/// path that never runs locally). The spawn site (`open`) stays gated.
///
/// EVERY row goes through `cmd /C start <shell> …`, and that is the
/// point: `start` creates a console, and Windows hosts a new console in
/// whatever the user picked under "default terminal application" — so the
/// terminal APP follows their setting and is never named here. What the
/// rows pick is the SHELL, which Windows has no configured default for;
/// `probe` supplies it for `Default` and for an id we don't recognize.
///
/// The project dir rides as the spawn cwd (see [`WindowsLaunch::cwd`]),
/// never inside the command string.
///
/// `probe` is a CLOSURE, not a value: a row that names its own shell
/// never reads it, and evaluating it eagerly cost those clicks up to two
/// `where` spawns on the tray's main thread for a result thrown away.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_open_command(
    id: &str,
    project: Option<&str>,
    cmd: &str,
    probe: impl FnOnce() -> WinShell,
) -> WindowsLaunch {
    let shell = match id {
        "pwsh" => WinShell::Pwsh,
        "powershell" => WinShell::PowerShell,
        "cmd" => WinShell::Cmd,
        // "auto", and any id this build doesn't know (a stale config, or
        // one written by a newer version) — the probe decides.
        _ => probe(),
    };
    let mut args = vec!["/C".to_string(), "start".to_string()];
    args.extend(shell.args(cmd));
    WindowsLaunch {
        program: "cmd",
        args,
        cwd: project.map(str::to_string),
        hide_helper_console: true,
    }
}

#[cfg(target_os = "windows")]
pub fn open(id: &str, project: Option<&str>, cmd: &str) -> Result<(), String> {
    let launch = windows_open_command(id, project, cmd, default_shell);
    let mut c = Command::new(launch.program);
    c.args(&launch.args);
    if let Some(dir) = &launch.cwd {
        c.current_dir(dir);
    }
    if launch.hide_helper_console {
        crate::process::hide_console(&mut c);
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

    // A list entry with no launch arm is worse than no entry at all:
    // `linux_open_command` returns None for an unknown id, so picking
    // Tilix would silently open whatever the fallback chain finds first.
    // The macOS launch forms were the only ones no test could see — and
    // a wrong flag is exactly how the Tilix row shipped broken. Verbatim
    // argv, one per shape: `-e`, no flag, `start --`.
    #[test]
    fn mac_open_args_build_each_terminals_launch() {
        let args = |id| mac_open_args(mac_terminal(id).unwrap(), "cd '/p' && claude");
        assert_eq!(
            args("alacritty"),
            vec![
                "-na",
                "Alacritty",
                "--args",
                "-e",
                "bash",
                "-lc",
                "cd '/p' && claude; exec $SHELL"
            ]
        );
        // kitty takes the command with no flag before it, and its bundle
        // is lowercase.
        assert_eq!(
            args("kitty"),
            vec![
                "-na",
                "kitty",
                "--args",
                "bash",
                "-lc",
                "cd '/p' && claude; exec $SHELL"
            ]
        );
        assert_eq!(&args("wezterm")[3..5], ["start", "--"]);
        assert_eq!(&args("ghostty")[..4], ["-na", "Ghostty", "--args", "-e"]);
    }

    #[test]
    fn every_linux_list_entry_has_a_launch_arm() {
        assert!(!LINUX_TERMINALS.is_empty());
        for &(_, id, _) in LINUX_TERMINALS {
            assert!(
                linux_open_command(id, "claude").is_some(),
                "no launch arm for {id}"
            );
        }
    }

    #[test]
    fn linux_open_command_mate_takes_the_rest_of_the_line() {
        // -x, not -e: mate-terminal's `-e` wants a single string.
        let l = linux_open_command("mate-terminal", "claude").unwrap();
        assert_eq!(l.program, "mate-terminal");
        assert_eq!(l.args, vec!["-x", "bash", "-lc", "claude; exec $SHELL"]);
    }

    // Tilix's `-e` is a single-string GOption, so the command must arrive
    // as ONE argument — an argv vector would hand it `bash` and leave
    // `-lc` to be rejected as an unknown option.
    #[test]
    fn linux_open_command_tilix_passes_one_string_not_an_argv_vector() {
        let l = linux_open_command("tilix", "echo 'hi'").unwrap();
        assert_eq!(l.program, "tilix");
        assert_eq!(
            l.args,
            vec!["-e", "bash -lc 'echo '\\''hi'\\''; exec $SHELL'"]
        );
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
    // Only the `wt` row may name a terminal app: `start` hands the new
    // console to whatever the user set as their default terminal
    // application, and naming one overrides that setting — which is the
    // whole reason picking a SHELL must not drag a terminal along.
    #[test]
    fn no_windows_row_names_a_terminal_every_one_goes_through_start() {
        for id in [
            "auto",
            "pwsh",
            "powershell",
            "cmd",
            // A `wt` left in an older config, and anything a newer build
            // might write: both are just unknown ids now.
            "wt",
            "an-id-from-a-newer-build",
        ] {
            let l = windows_open_command(id, None, "claude", || WinShell::PowerShell);
            assert_eq!(l.program, "cmd", "{id}");
            assert_eq!(&l.args[..2], ["/C", "start"], "{id}");
            assert!(!l.args.iter().any(|a| a == "wt"), "{id} named a terminal");
            // The OUTER cmd /C helper's console must be hidden — `start`
            // opens the real terminal window.
            assert!(l.hide_helper_console, "{id}");
        }
    }

    // The list picks a SHELL. An explicit row is exactly what it says;
    // only Default (and an id this build doesn't know) takes the probe.
    #[test]
    fn windows_rows_select_their_own_shell_and_default_takes_the_probe() {
        let args = |id, probe| windows_open_command(id, None, "claude", move || probe).args;
        assert_eq!(
            args("pwsh", WinShell::Cmd),
            vec!["/C", "start", "pwsh", "-NoExit", "-Command", "claude"]
        );
        assert_eq!(
            args("powershell", WinShell::Cmd),
            vec!["/C", "start", "powershell", "-NoExit", "-Command", "claude"]
        );
        assert_eq!(
            args("cmd", WinShell::Pwsh),
            vec!["/C", "start", "cmd", "/K", "claude"]
        );
        // Default follows the probe, whatever it found.
        assert_eq!(
            args("auto", WinShell::Pwsh),
            vec!["/C", "start", "pwsh", "-NoExit", "-Command", "claude"]
        );
        assert_eq!(
            args("auto", WinShell::PowerShell),
            vec!["/C", "start", "powershell", "-NoExit", "-Command", "claude"]
        );
    }

    #[test]
    fn windows_open_command_keeps_the_project_dir_out_of_the_command_string() {
        // The project dir must NOT appear inside the command string — a
        // nested `cd /d "…"` hits the MSVCRT-vs-cmd quoting mismatch
        // and mangled real paths (the original Windows bug). It rides
        // as the spawn cwd; `start`'s new console inherits it. This now
        // holds for the PowerShell rows too, which used to build their
        // own `cd '<p>';` prefix.
        for id in ["auto", "pwsh", "powershell", "cmd"] {
            let l = windows_open_command(id, Some("C:\\My Docs\\o'brien"), "claude", || {
                WinShell::Pwsh
            });
            assert_eq!(l.cwd.as_deref(), Some("C:\\My Docs\\o'brien"), "{id}");
            assert!(
                !l.args.iter().any(|a| a.contains("My Docs")),
                "{id} put the path in the command"
            );
        }
        let l = windows_open_command("auto", None, "claude", || WinShell::Pwsh);
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
        assert!(s.contains("do script \" echo hi\" in window 1"), "{s}");
        // Already running → a fresh `do script` (its own new window).
        assert!(s.contains("    do script \" echo hi\"\n  else"), "{s}");
    }

    // Everything TYPED into a shell leads with a space, so an rc file that
    // reads a keystroke (oh-my-zsh's update prompt: `read -r -k 1`) eats the
    // space instead of the first character of the command — see `typed`.
    // Without it, `cd '/p' && claude` was left as `d '/p' && claude`
    // ("zsh: command not found: d") whenever that prompt fired.
    #[cfg(target_os = "macos")]
    #[test]
    fn typed_commands_lead_with_a_space_so_an_rc_prompt_cannot_eat_them() {
        // Terminal.app types on BOTH branches (already running / cold launch).
        let s = terminal_app_script("cd '/p' && claude");
        assert_eq!(
            s.matches("do script \" cd '/p' && claude\"").count(),
            2,
            "{s}"
        );
        assert!(!s.contains("do script \"cd '/p' && claude\""), "{s}");

        let s = iterm_script("cd '/p' && claude");
        assert!(
            s.contains("write text \" cd '/p' && claude\""),
            "the typed branch must lead with a space: {s}"
        );
        // iTerm's `create window … command` is NOT typed — it is the process
        // iTerm runs, so it stays exactly as built.
        assert_eq!(
            s.matches("create window with default profile command \"cd '/p' && claude\"")
                .count(),
            2,
            "{s}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn iterm_script_reuses_launch_window_on_cold_start() {
        let s = iterm_script("echo hi");
        assert!(s.contains("if it is running then"), "{s}");
        // Cold launch → reuse the launch window instead of a second one.
        assert!(s.contains("(count of windows) is 0"), "{s}");
        assert!(
            s.contains("tell current session of current window to write text \" echo hi\""),
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
        assert_eq!(
            new_session_command_with("Grok", "codex").as_deref(),
            Some("grok")
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
