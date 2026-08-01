//! Upgrading a managed CLI to its latest version, in-app.
//!
//! Two things live here: WHAT command upgrades each CLI
//! ([`upgrade_commands`], shown in the Providers card's update-badge
//! tooltip) and RUNNING it ([`run_upgrade`], driven by clicking that
//! badge).
//!
//! **Four CLIs ship their own upgrade command, used verbatim.** Each
//! detects its own install method, and does so with signals we cannot
//! read from outside — Codex keys off the `CODEX_MANAGED_BY_NPM` env
//! var its npm shim sets (`install-context/src/lib.rs:110-112`), which
//! lives in the codex process's environment, not ours. Second-guessing
//! that from a path would be strictly worse.
//!
//! **Gemini CLI is the exception**: it has no update subcommand
//! (verified against `gemini --help` — mcp / extensions / skills /
//! hooks / gemma only), so there we DO infer the install method from
//! the resolved binary path.
//!
//! **The run goes through `$SHELL -l -i -c`** — the same form as
//! [`crate::providers::shell_version_fallback`] and for the same
//! reason: a GUI process inherits launchd's bare PATH
//! (`/usr/bin:/bin:/usr/sbin:/sbin`) with no `npm`, `brew`, or
//! nvm/volta shims. `find_cli_binary` locates the CLI itself, but not
//! what the CLI shells out to — `codex update` spawns
//! `npm install -g @openai/codex`, and that npm exists only in an
//! interactive login shell. `-l` alone is not enough: zsh reads
//! `.zshrc` only when interactive, and that is where nvm is set up.

use crate::providers::CliApp;

/// How a Gemini CLI install was performed, inferred from the resolved
/// binary path. Only Gemini needs this — see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    Npm,
    Bun,
    Pnpm,
    Brew,
}

impl InstallMethod {
    /// The upgrade command for `package` under this install method.
    fn command(self, package: &str, brew_formula: &str) -> String {
        match self {
            // Forms follow Codex's own UpdateAction
            // (tui/src/update_action.rs:41-43) — the one upstream that
            // spells these out: bare `install -g <pkg>` with no
            // `@latest` pin (that IS latest), `upgrade` for brew.
            InstallMethod::Npm => format!("npm install -g {package}"),
            InstallMethod::Bun => format!("bun install -g {package}"),
            // pnpm has no global `install -g <pkg>` form — global adds
            // are `pnpm add -g` (`pnpm install` only reads a lockfile).
            InstallMethod::Pnpm => format!("pnpm add -g {package}"),
            InstallMethod::Brew => format!("brew upgrade {brew_formula}"),
        }
    }
}

/// Infer the install method from a CLI's fully-resolved (symlinks
/// followed) binary path.
///
/// Verified against real installs on macOS:
/// - npm: `~/.nvm/versions/node/<v>/bin/gemini` resolves to
///   `…/lib/node_modules/@google/gemini-cli/bundle/gemini.js`
/// - Homebrew: `/opt/homebrew/bin/<x>` resolves to
///   `../Cellar/<formula>/<v>/bin/<x>`
///
/// bun and pnpm global roots ALSO contain a `node_modules` segment, so
/// they must be tested before the generic npm arm.
///
/// `is_macos` mirrors Codex's own prefix check
/// (`install-context/src/lib.rs:220`): `/usr/local` implies Homebrew
/// only on macOS; on Linux it is an ordinary system prefix.
fn install_method_from_path(real: &std::path::Path, is_macos: bool) -> Option<InstallMethod> {
    // Slash-normalize so the same segment tests work on Windows paths.
    let p = real.to_string_lossy().replace('\\', "/");
    let has = |seg: &str| p.contains(seg);

    if has("/Cellar/")
        || has("/homebrew/")
        || (is_macos && (p.starts_with("/opt/homebrew/") || p.starts_with("/usr/local/")))
    {
        return Some(InstallMethod::Brew);
    }
    if has("/.bun/") {
        return Some(InstallMethod::Bun);
    }
    // pnpm global roots: `~/Library/pnpm`, `~/.local/share/pnpm`,
    // `%LOCALAPPDATA%/pnpm` — all carry a `pnpm` path segment.
    if has("/pnpm/") {
        return Some(InstallMethod::Pnpm);
    }
    if has("/node_modules/") {
        return Some(InstallMethod::Npm);
    }
    None
}

/// Gemini CLI's upgrade command, inferred from its install. Falls back
/// to npm when the binary isn't found or the path matches nothing known
/// — npm is Gemini's primary distribution (its InstallGuide leads with
/// it), so it's the best guess rather than showing nothing.
fn gemini_command() -> String {
    let method = crate::providers::find_cli_binary("gemini")
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|real| install_method_from_path(&real, cfg!(target_os = "macos")))
        .unwrap_or(InstallMethod::Npm);
    // Package + formula names mirror this app's own InstallGuide entry
    // (`CLI_INSTALL.gemini` in src/constants.ts).
    method.command("@google/gemini-cli", "gemini-cli")
}

/// The shell command that upgrades `app`, or `None` when it has no
/// command-line upgrade path.
///
/// `None` covers Claude Desktop — a self-updating GUI app (Squirrel)
/// with no CLI entry point. The Codex DESKTOP app is the same story
/// (Sparkle), which is why only Codex's *CLI* segment gets a command.
pub fn upgrade_command(app: CliApp) -> Option<String> {
    Some(match app {
        // Built-in upgrade subcommands, verified in each upstream:
        //   claude   — main.tsx:6160 (`.command('update').alias('upgrade')`)
        //   codex    — cli/src/main.rs:799 `run_update_command`
        //   opencode — cli/cmd/upgrade.ts `UpgradeCommand`
        //   grok     — `grok update`, per the shipped `--help` (0.2.111)
        CliApp::Claude => "claude update".to_string(),
        // Bare `codex`, NOT `terminal::codex_shell_invocation()`. That
        // helper falls back to the desktop app's bundled binary when no
        // standalone CLI is found, which is right for LAUNCHING a
        // session (any codex will do) and wrong here: upgrading is a
        // version-side action and only a self-managed install can
        // upgrade itself. The bundled copy ships with the app via
        // Sparkle, and codex's own updater classifies it as
        // `InstallMethod::Other` and bails with "Could not detect the
        // Codex installation method" (cli/src/main.rs:809). Borrowing
        // the helper once put `'/Applications/ChatGPT.app/Contents/
        // Resources/codex' update` in the badge tooltip — a command
        // that always fails.
        CliApp::Codex => "codex update".to_string(),
        CliApp::Opencode => "opencode upgrade".to_string(),
        CliApp::Grok => "grok update".to_string(),
        CliApp::Gemini => gemini_command(),
        CliApp::ClaudeDesktop => return None,
    })
}

/// Every app's upgrade command, keyed by the `CliApp` serde string —
/// the same map shape as `detect_cli_versions_cmd`. Apps with no
/// command-line upgrade are ABSENT from the map.
pub fn upgrade_commands() -> std::collections::HashMap<String, String> {
    CliApp::all()
        .iter()
        .filter_map(|&app| upgrade_command(app).map(|cmd| (app.key().to_string(), cmd)))
        .collect()
}

/// An upgrade legitimately takes minutes (npm resolving a global
/// install, an installer downloading a release), so this is far more
/// generous than the 5s probe budget. It exists only so a wedged child
/// can't pin a task slot forever.
const UPGRADE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How many trailing output lines to keep for the failure message.
const TAIL_LINES: usize = 20;

/// Build the child process. See the module doc for why this is an
/// interactive login shell rather than a bare spawn.
#[cfg(unix)]
fn upgrade_child(cmd: &str) -> tokio::process::Command {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut c = tokio::process::Command::new(shell);
    // TERM=dumb keeps spinner libraries from emitting cursor-control
    // escapes we'd only have to strip back out.
    c.env("TERM", "dumb");
    // Shared with `shell_version_fallback`: marker first so rc-file
    // banners can be split off, then the command with its stderr folded
    // in. The SHELL's own stderr is discarded in `run_upgrade`.
    let script = crate::providers::marked_shell_command(cmd);
    c.args(["-l", "-i", "-c", &script]);
    c
}

#[cfg(windows)]
fn upgrade_child(cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    // cmd.exe has no rc file, so nothing precedes the marker — it's
    // echoed anyway so the reader's filter is identical on both paths.
    c.args([
        "/C",
        &format!("echo {}& {cmd} 2>&1", crate::providers::SHELL_PROBE_MARKER),
    ]);
    // No console window: applied by `process::spawn_managed`.
    c
}

/// Run `app`'s upgrade and resolve when it finishes. The card's badge
/// reads "Updating" and is disabled meanwhile; on failure the error
/// returned here becomes the badge's tooltip.
///
/// **stdin is `null` deliberately.** `opencode upgrade` prompts for a
/// choice when it can't identify the install method
/// (`cli/cmd/upgrade.ts:33-45`), and `claude update` can land in its
/// `no_permissions` branch (`utils/autoUpdater.ts:498`) wanting a sudo
/// password. With no stdin those hit EOF and exit non-zero instead of
/// hanging forever; the UI then names the command for the user to run
/// in their own terminal.
pub async fn run_upgrade(app: CliApp) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some(cmd) = upgrade_command(app) else {
        return Err("this app has no command-line upgrade".to_string());
    };

    let mut spawn_cmd = upgrade_child(&cmd);
    spawn_cmd
        .stdout(std::process::Stdio::piped())
        // DISCARD the shell's own stderr. The command's stderr is
        // already folded into stdout by `2>&1`; what remains on fd 2 is
        // the interactive shell's rc-time noise. Piping a stream nobody
        // reads is a DEADLOCK — once the pipe buffer fills (~64 KB) the
        // child blocks forever. Reproduced directly: a child writing
        // 400 KB to an unread piped stderr never returns its stdout.
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    // Managed, so a timeout kills the whole tree: the thing doing the
    // work here is a GRANDCHILD (`$SHELL -l -i -c` → `codex update` →
    // `npm install -g`), which a signal to the shell alone would leave
    // running — mid-install, with the package half-written.
    let mut child = crate::process::spawn_managed(spawn_cmd)
        .map_err(|err| format!("couldn't start the upgrade: {err}"))?;

    let stdout = child
        .stdout()
        .ok_or_else(|| "couldn't read the upgrade output".to_string())?;

    // The stream must be DRAINED regardless (an unread pipe blocks the
    // child); we keep the tail so a failure reports the reason the
    // command actually printed rather than a bare exit code.
    let mut tail: Vec<String> = Vec::new();
    let mut lines = BufReader::new(stdout).lines();
    let mut started = false;

    let stream = async {
        while let Ok(Some(raw)) = lines.next_line().await {
            let line = crate::sessions::strip_ansi(&raw).trim_end().to_string();
            if !started {
                // `contains`, not equality: a prompt or banner fragment
                // can share the line the echo lands on.
                started = line.contains(crate::providers::SHELL_PROBE_MARKER);
                continue;
            }
            if line.is_empty() {
                continue;
            }
            if tail.len() == TAIL_LINES {
                tail.remove(0);
            }
            tail.push(line);
        }
        child.wait().await
    };

    let status = match tokio::time::timeout(UPGRADE_TIMEOUT, stream).await {
        Ok(status) => status.map_err(|err| format!("the upgrade failed to run: {err}"))?,
        Err(_) => {
            // Stop the whole tree explicitly rather than leaving it to
            // `Drop`, which can't wait: an installer given a moment to
            // handle SIGTERM cleans up its partial download, where an
            // immediate SIGKILL leaves it on disk.
            child.terminate(crate::process::INSTALL_GRACE).await;
            return Err("the upgrade timed out".to_string());
        }
    };

    if status.success() {
        return Ok(());
    }
    Err(tail.last().cloned().unwrap_or_else(|| match status.code() {
        Some(code) => format!("exited with code {code}"),
        None => "the upgrade was interrupted".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn built_in_upgrade_commands_are_used_verbatim() {
        assert_eq!(
            upgrade_command(CliApp::Claude).as_deref(),
            Some("claude update")
        );
        assert_eq!(
            upgrade_command(CliApp::Opencode).as_deref(),
            Some("opencode upgrade")
        );
        assert_eq!(
            upgrade_command(CliApp::Grok).as_deref(),
            Some("grok update")
        );
        // Never the bundled binary's path — see the Codex arm.
        assert_eq!(
            upgrade_command(CliApp::Codex).as_deref(),
            Some("codex update")
        );
    }

    /// Upgrade commands must never carry an absolute path. Codex is the
    /// one at risk: a second, bundled binary exists inside the desktop
    /// app, and the session-launch helper falls back to it by path.
    /// That fallback belongs to execution, not to upgrading.
    #[test]
    fn upgrade_commands_never_reference_a_bundled_binary() {
        for (app, cmd) in upgrade_commands() {
            assert!(
                !cmd.contains(".app/"),
                "{app} upgrade command points inside an app bundle: {cmd}"
            );
            assert!(
                !cmd.starts_with('/') && !cmd.starts_with('\''),
                "{app} upgrade command is an absolute path: {cmd}"
            );
        }
    }

    #[test]
    fn claude_desktop_has_no_command_line_upgrade() {
        assert_eq!(upgrade_command(CliApp::ClaudeDesktop), None);
        assert!(!upgrade_commands().contains_key("claude-desktop"));
    }

    #[test]
    fn upgrade_commands_map_is_keyed_by_the_cli_app_string() {
        let map = upgrade_commands();
        assert_eq!(map.get("claude").map(String::as_str), Some("claude update"));
        assert_eq!(
            map.get("opencode").map(String::as_str),
            Some("opencode upgrade")
        );
        assert_eq!(map.get("grok").map(String::as_str), Some("grok update"));
        assert!(map.contains_key("codex"));
        assert!(map.contains_key("gemini"));
    }

    #[test]
    fn gemini_always_yields_a_command_even_when_uninstalled() {
        let cmd = upgrade_command(CliApp::Gemini).expect("gemini falls back to npm");
        assert!(cmd.contains("@google/gemini-cli"), "unexpected: {cmd}");
    }

    #[test]
    fn install_method_reads_real_world_resolved_paths() {
        // Verbatim from a real macOS install (nvm-managed npm global).
        assert_eq!(
            install_method_from_path(
                Path::new(
                    "/Users/j/.nvm/versions/node/v22.21.1/lib/node_modules/@google/gemini-cli/bundle/gemini.js"
                ),
                true
            ),
            Some(InstallMethod::Npm)
        );
        assert_eq!(
            install_method_from_path(
                Path::new("/opt/homebrew/Cellar/gemini-cli/0.16.2/bin/gemini"),
                true
            ),
            Some(InstallMethod::Brew)
        );
        // bun / pnpm global roots also contain `node_modules`, so they
        // must win over the npm arm.
        assert_eq!(
            install_method_from_path(
                Path::new("/Users/j/.bun/install/global/node_modules/@google/gemini-cli/x.js"),
                true
            ),
            Some(InstallMethod::Bun)
        );
        assert_eq!(
            install_method_from_path(
                Path::new("/Users/j/Library/pnpm/global/5/node_modules/@google/gemini-cli/x.js"),
                true
            ),
            Some(InstallMethod::Pnpm)
        );
        // Windows-shaped path, backslashes normalized.
        assert_eq!(
            install_method_from_path(
                Path::new(
                    "C:\\Users\\j\\AppData\\Roaming\\npm\\node_modules\\@google\\gemini-cli\\x.js"
                ),
                false
            ),
            Some(InstallMethod::Npm)
        );
    }

    #[test]
    fn usr_local_implies_homebrew_only_on_macos() {
        let p = Path::new("/usr/local/bin/gemini");
        assert_eq!(install_method_from_path(p, true), Some(InstallMethod::Brew));
        // On Linux `/usr/local` is an ordinary prefix, not Homebrew.
        assert_eq!(install_method_from_path(p, false), None);
    }

    #[test]
    fn unknown_paths_yield_no_method() {
        assert_eq!(
            install_method_from_path(Path::new("/opt/custom/bin/gemini"), true),
            None
        );
    }

    #[test]
    fn method_commands_match_upstream_forms() {
        assert_eq!(
            InstallMethod::Npm.command("@google/gemini-cli", "gemini-cli"),
            "npm install -g @google/gemini-cli"
        );
        assert_eq!(
            InstallMethod::Bun.command("@google/gemini-cli", "gemini-cli"),
            "bun install -g @google/gemini-cli"
        );
        // pnpm global adds are `add -g`, not `install -g`.
        assert_eq!(
            InstallMethod::Pnpm.command("@google/gemini-cli", "gemini-cli"),
            "pnpm add -g @google/gemini-cli"
        );
        assert_eq!(
            InstallMethod::Brew.command("@google/gemini-cli", "gemini-cli"),
            "brew upgrade gemini-cli"
        );
    }
}
