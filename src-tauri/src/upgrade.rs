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

/// Drain the child's stdout to EOF, returning the last [`TAIL_LINES`]
/// lines that followed the start marker.
///
/// The stream must be drained REGARDLESS of whether anyone wants the
/// text — an unread pipe blocks the child once it fills (~64 KB) — so
/// this never stops early on anything but EOF or a real pipe error.
///
/// **Bytes in, lossy `String` out — never `lines()`.** That helper hands
/// back `io::Result<String>` and yields `Err` for a line that isn't valid
/// UTF-8, which the `while let Ok(..)` reading it took for end-of-stream.
/// Not an edge case on Windows: a console child writes in the system's
/// OEM code page, so on any non-English install the CLI's own error text
/// is not UTF-8. Measured on a zh-CN Win11 box — `'x' is not recognized`
/// arrived as CP936 (`178 187 202 199 …` = 不是内部或外部命令) and
/// decoding stopped dead on it.
///
/// It cost more than mangled characters. Everything after that line was
/// dropped too, so a failing upgrade reported a bare exit code with an
/// EMPTY tooltip despite having explained itself perfectly well — and the
/// drain stopped while the child kept writing, which is exactly the pipe
/// deadlock described above, turning a chatty failure into a hang until
/// [`UPGRADE_TIMEOUT`] (10 minutes).
///
/// Lossy decoding cannot fail, so the drain always reaches EOF, and the
/// ASCII skeleton that makes the tail actionable (command names, paths,
/// versions, error codes) survives whatever the code page was.
///
/// Split out of `run_upgrade` so the tests drive the REAL reader. The
/// Windows shell form is the one that produces non-UTF-8 output, and a
/// test copy of this loop would have reproduced the bug rather than
/// caught it.
async fn drain_marked_output<R>(stdout: R) -> Vec<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut tail: Vec<String> = Vec::new();
    let mut reader = BufReader::new(stdout);
    let mut started = false;
    let mut raw: Vec<u8> = Vec::new();

    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break, // a real pipe error — nothing left to read
        }
        let decoded = String::from_utf8_lossy(&raw);
        let line = crate::sessions::strip_ansi(&decoded).trim_end().to_string();
        if !started {
            // `contains`, not equality: a prompt or banner fragment can
            // share the line the echo lands on.
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
    tail
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

    let stream = async {
        let tail = drain_marked_output(stdout).await;
        (tail, child.wait().await)
    };

    let (tail, status) = match tokio::time::timeout(UPGRADE_TIMEOUT, stream).await {
        Ok((tail, status)) => (
            tail,
            status.map_err(|err| format!("the upgrade failed to run: {err}"))?,
        ),
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

    /// Drive a real [`upgrade_child`] through the REAL reader, and hand
    /// back its exit success plus the tail `run_upgrade` would report.
    ///
    /// Windows-only because that branch is a DIFFERENT shell form
    /// (`cmd /C "echo <marker>& …"` rather than `$SHELL -l -i -c`) whose
    /// output contract nothing checked — the unix side's marker handling
    /// is covered by `providers::tests::after_shell_marker_*`.
    #[cfg(windows)]
    async fn drive_upgrade_child(cmd: &str) -> (bool, Vec<String>) {
        let mut spawn_cmd = upgrade_child(cmd);
        spawn_cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null());
        let mut child = crate::process::spawn_managed(spawn_cmd).expect("spawn");
        let stdout = child.stdout().expect("stdout");
        let tail = drain_marked_output(stdout).await;
        let status = child.wait().await.expect("wait");
        (status.success(), tail)
    }

    /// The marker must actually reach stdout, and the command's own output
    /// must follow it — every line before the marker is discarded, so a
    /// form that never printed one would swallow the whole upgrade log and
    /// leave a bare exit code. A non-empty tail proves both halves.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_upgrade_child_emits_the_marker_before_the_output() {
        let _hold = crate::process::hold_children();
        let (ok, tail) = drive_upgrade_child("echo hello").await;

        assert!(ok, "a plain echo should succeed: {tail:?}");
        assert!(
            tail.iter().any(|l| l.contains("hello")),
            "the output never survived the marker filter: {tail:?}"
        );
    }

    /// **A failure reason in the system code page must still come back.**
    ///
    /// Two things have to hold at once here, and each broke on real
    /// Windows: `2>&1` folds the reason onto stdout (the shell's own fd 2
    /// is wired to `null`, so anything left there is gone), and the reader
    /// survives decoding it. On a zh-CN box this exact command answers in
    /// CP936, which is not valid UTF-8 — the `lines()` reader this
    /// replaced treated that as end-of-stream and returned an empty tail.
    ///
    /// Asserting on the ECHOED COMMAND NAME rather than the message: it is
    /// ASCII, so it survives lossy decoding on every locale, which keeps
    /// this test meaningful on an English install too.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_upgrade_child_reports_a_failure_in_any_code_page() {
        let _hold = crate::process::hold_children();
        // cmd.exe reports an unknown command on stderr and exits non-zero:
        // the same shape as a real failing upgrade.
        let (ok, tail) = drive_upgrade_child("termory_no_such_command_xyz").await;

        assert!(!ok, "an unknown command must fail: {tail:?}");
        assert!(
            tail.iter()
                .any(|l| l.contains("termory_no_such_command_xyz")),
            "the reason never survived, so the failure tooltip is empty: {tail:?}"
        );
    }

    /// The drain must not stop at a line that isn't valid UTF-8 — the
    /// lines AFTER it are exactly the ones a tail keeps, and stopping
    /// early also stops draining a pipe the child is still writing to.
    ///
    /// Fed straight into the real reader rather than through a shell, so
    /// it pins the rule on every platform instead of only where a console
    /// happens to emit non-UTF-8.
    #[tokio::test]
    async fn the_drain_survives_a_line_that_is_not_utf8() {
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(crate::providers::SHELL_PROBE_MARKER.as_bytes());
        raw.extend_from_slice(b"\n");
        // CP936 for the tail of "is not an internal or external command".
        raw.extend_from_slice(&[178, 187, 202, 199, 196, 218, 178, 191]);
        raw.extend_from_slice(b"\nlast-line\n");

        let tail = drain_marked_output(std::io::Cursor::new(raw)).await;

        assert_eq!(
            tail.last().map(String::as_str),
            Some("last-line"),
            "decoding stopped at the non-UTF-8 line: {tail:?}"
        );
    }

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
