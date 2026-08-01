//! Provider management for Claude Code / Codex / Gemini CLI / OpenCode.
//!
//! Design principle (per user instruction): **Termory does NOT store an
//! "active provider" pointer.** The active state is always re-derived
//! by reading each CLI's live configuration file and matching it
//! against the saved provider list. This keeps Termory consistent
//! when:
//!   - users edit the CLI config by hand
//!   - other tools (cc-switch, scripts) change the config
//!   - the CLI itself rewrites the config via OAuth flows
//!
//! What Termory DOES store (frontend prefs.json):
//!   - The Provider list (user-defined named snapshots)
//!   - UI prefs (which tab was last viewed, etc.)
//!
//! What this module owns (backend):
//!   - per-CLI activate / deactivate functions that write live configs
//!   - per-CLI read_active function that reverse-derives state
//!   - test_provider function that pings the API
//!
//! Provider data model (intentionally a flat user-facing shape, not
//! the per-CLI raw `settings_config` value cc-switch uses):
//!   { id, app, kind, name, base_url, api_key, model, ... }
//! The activate functions translate this into the right shape for
//! each CLI's config file format.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use toml_edit::{value as toml_value, DocumentMut, Item};

/// Stable provider id Termory uses inside Codex's `[model_providers.X]`
/// table and OpenCode's `provider.X` map. This avoids reserving any
/// of the CLI's built-in provider ids (codex: openai/amazon-bedrock/
/// ollama/lmstudio; opencode picks model id by `provider/model`) and
/// — for Codex — prevents session-history drift across switches
/// because Codex groups history by model_provider id.
pub const TERMORY_PROVIDER_ID: &str = "termory";

/// Codex's reserved built-in provider ids — writing to one of these
/// names doesn't actually take effect (built-ins win in
/// `merge_configured_model_providers` via `or_insert`).
const CODEX_RESERVED_IDS: &[&str] = &[
    "amazon-bedrock",
    "openai",
    "ollama",
    "lmstudio",
    "oss",
    "ollama-chat",
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum CliApp {
    Claude,
    Codex,
    Gemini,
    Opencode,
    /// Claude **Desktop** (the GUI app) — managed via its 3P gateway
    /// profile, NOT a terminal CLI. Provider-switchable but never
    /// terminal-launchable, and only supported on macOS / Windows.
    #[serde(rename = "claude-desktop")]
    ClaudeDesktop,
    /// xAI's Grok Build CLI. Provider switching via the OFFICIAL
    /// custom-model mechanism in `~/.grok/config.toml` — a
    /// `[model."<model-id>"]` entry (model/base_url/name/description/
    /// api_key; the section key IS the model id, `description` carries
    /// the provider name) + `models.default = "<model-id>"`; see
    /// docs.x.ai/build/settings#example-configtoml, verified against the
    /// real TUI. OAuth session lives in `~/.grok/auth.json`, never
    /// touched.
    Grok,
}

impl CliApp {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(CliApp::Claude),
            "codex" => Some(CliApp::Codex),
            "gemini" => Some(CliApp::Gemini),
            "opencode" => Some(CliApp::Opencode),
            "claude-desktop" => Some(CliApp::ClaudeDesktop),
            "grok" => Some(CliApp::Grok),
            _ => None,
        }
    }

    /// The wire key for this app — the exact inverse of [`CliApp::parse`], and
    /// the same string the frontend's `CliApp` union uses. Keys config maps
    /// (`sources`, `active_provider_ids`) and tray menu ids.
    pub fn key(self) -> &'static str {
        match self {
            CliApp::Claude => "claude",
            CliApp::Codex => "codex",
            CliApp::Gemini => "gemini",
            CliApp::Opencode => "opencode",
            CliApp::ClaudeDesktop => "claude-desktop",
            CliApp::Grok => "grok",
        }
    }

    /// Name of the binary this CLI ships as, looked up on `$PATH`.
    /// Claude Desktop has no CLI binary — installation is detected via
    /// its config dir (`claude_desktop::is_installed`), so this name is
    /// never used for a PATH probe (the detection paths special-case it).
    pub fn bin_name(self) -> &'static str {
        match self {
            CliApp::Claude => "claude",
            CliApp::Codex => "codex",
            CliApp::Gemini => "gemini",
            CliApp::Opencode => "opencode",
            CliApp::ClaudeDesktop => "claude-desktop",
            CliApp::Grok => "grok",
        }
    }

    /// Whether this entry is a terminal-launchable CLI. False only for
    /// Claude Desktop (a GUI app) — used to keep it out of the tray's
    /// recent-session / new-session terminal flows and version probes
    /// while still surfacing it in the provider-switch submenu.
    pub fn is_cli(self) -> bool {
        !matches!(self, CliApp::ClaudeDesktop)
    }

    pub fn all() -> [CliApp; 6] {
        // Order drives the tray submenu list — Claude Desktop sits right
        // after Claude Code, matching the Providers page tab order
        // (`CLI_APPS` in constants.ts). Grok Build sits last (newest).
        [
            CliApp::Claude,
            CliApp::ClaudeDesktop,
            CliApp::Codex,
            CliApp::Gemini,
            CliApp::Opencode,
            CliApp::Grok,
        ]
    }
}

/// Build the ordered list of directories to scan when looking for
/// `tool`'s binary. Modeled after cc-switch's `scan_cli_version`
/// (`commands/misc.rs:584`) — covers every common installation method
/// instead of relying on the inherited process `$PATH`.
///
/// Order matters — `find_cli_binary` takes the first stat hit:
///   1. the installer env var `tool`'s own install script honors, if set
///      (most user-specific signal there is, so it outranks everything)
///   2. per-user dirs, then system dirs (prefer a per-user install)
///   3. the process `$PATH` last, as a catch-all
/// Per-tool sections (opencode's bun/go dirs, codex's Windows standalone
/// bin dir, grok's `~/.grok/bin`) only contribute when `tool` asks.
///
/// EVERY entry must trace to a real installer — cite the script + line
/// next to it. A `~/.codex/bin` once sat here for months purely because
/// it LOOKED like `~/.grok/bin` and `~/.opencode/bin`; no codex
/// installer has ever written there, and a later reader (reasonably)
/// assumed the unexplained path was a legacy layout and wrote that
/// guess down as fact. An unjustified entry is worse than a missing
/// one: it costs a stat AND it manufactures false provenance.
fn cli_search_paths(tool: &str) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut paths: Vec<PathBuf> = Vec::new();
    let home = crate::home_dir().unwrap_or_default();

    // Installer-honored custom install dirs go FIRST. Each var is the
    // one THAT tool's own install script reads, so if it's set the
    // binary really is there — and an explicitly chosen dir has to beat
    // every package-manager dir below, since `find_cli_binary` returns
    // the first stat hit. Relative order within a tool follows its
    // installer's own precedence (opencode: OPENCODE_INSTALL_DIR then
    // XDG_BIN_DIR then its default). Unset vars contribute nothing, so
    // this is inert for the common case.
    let install_dir_vars: &[&str] = match tool {
        "opencode" => &["OPENCODE_INSTALL_DIR", "XDG_BIN_DIR"],
        "codex" => &["CODEX_INSTALL_DIR"],
        "grok" => &["GROK_BIN_DIR"],
        _ => &[],
    };
    for var in install_dir_vars {
        if let Some(val) = std::env::var_os(var) {
            push_unique(&mut paths, PathBuf::from(val));
        }
    }

    // Cross-platform user-level dirs.
    //   - `~/.npm-global/bin` resolves to `%USERPROFILE%\.npm-global\bin`
    //     on Windows; uncommon but valid when a user sets a custom npm
    //     prefix anywhere.
    //   - `~/.local/bin` is NOT unix-only. Claude Code's own launcher-path
    //     helper reads, verbatim from the shipped binary (2.1.218):
    //         if (platform === "win32")
    //             return join(homedir(), ".local", "bin", "claude.exe")
    //         return "~/.local/bin/claude"
    //     i.e. the native installer (`claude.ai/install.ps1` → bootstrap.ps1
    //     → `claude.exe install`, the powershell command our own InstallGuide
    //     shows) lands in `%USERPROFILE%\.local\bin` on Windows too. It
    //     appends that to the user PATH, but Windows doesn't propagate PATH
    //     to running processes, so the PATH fallback alone leaves a fresh
    //     install undetected.
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join(".npm-global/bin"));
        push_unique(&mut paths, home.join(".local/bin"));
    }

    // Unix-only user-level dirs (n version manager, Unix-style Volta
    // layout). On Windows these are non-existent paths, so gating them
    // avoids dead stat() calls and clarifies intent.
    #[cfg(unix)]
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join("n/bin"));
        push_unique(&mut paths, home.join(".volta/bin"));
        extend_mise_node_search_paths(&mut paths, &home);
    }

    // System dirs per platform.
    #[cfg(target_os = "macos")]
    {
        push_unique(&mut paths, PathBuf::from("/opt/homebrew/bin"));
        push_unique(&mut paths, PathBuf::from("/usr/local/bin"));
    }
    #[cfg(target_os = "linux")]
    {
        push_unique(&mut paths, PathBuf::from("/usr/local/bin"));
        push_unique(&mut paths, PathBuf::from("/usr/bin"));
    }
    #[cfg(target_os = "windows")]
    {
        // npm global default: %APPDATA%\npm
        if let Some(appdata) = dirs::data_dir() {
            push_unique(&mut paths, appdata.join("npm"));
        }
        // Node.js MSI installer
        push_unique(&mut paths, PathBuf::from("C:\\Program Files\\nodejs"));
        // Scoop — opencode README's recommended Windows install method
        if !home.as_os_str().is_empty() {
            push_unique(&mut paths, home.join("scoop").join("shims"));
        }
        // Chocolatey — opencode README's other recommended Windows method
        push_unique(
            &mut paths,
            PathBuf::from("C:\\ProgramData\\chocolatey\\bin"),
        );
        // Volta on Windows lives at %LOCALAPPDATA%\Volta\bin (NOT ~/.volta)
        if let Some(localdata) = dirs::data_local_dir() {
            push_unique(&mut paths, localdata.join("Volta").join("bin"));
            // pnpm on Windows: %LOCALAPPDATA%\pnpm
            push_unique(&mut paths, localdata.join("pnpm"));
            // winget `portable` packages (Claude Code's winget manifest is
            // `InstallerType: portable`) keep the real payload under
            // `WinGet\Packages\<id>\` and expose a shim from a Links dir
            // that winget appends to PATH. Both dirs per winget-cli
            // `AppInstallerCommonCore/Runtime.cpp:223-234`:
            //   PortableLinksUserLocation    = LocalAppData\Microsoft\WinGet\Links
            //   PortableLinksMachineLocation = ProgramFiles\WinGet\Links
            // User scope first (the default for `winget install`).
            push_unique(
                &mut paths,
                localdata.join("Microsoft").join("WinGet").join("Links"),
            );
        }
        // %ProgramFiles% is FOLDERID_ProgramFiles for an x64 process, which
        // Termory is (winget resolves the known folder; the env var is the
        // equivalent without pulling in a Windows-API dependency).
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            push_unique(
                &mut paths,
                PathBuf::from(program_files).join("WinGet").join("Links"),
            );
        }
    }

    // Unix node version managers — recursive enumeration.
    #[cfg(unix)]
    {
        // FNM (Fast Node Manager): each shell session gets its own
        // `~/.local/state/fnm_multishells/<pid>/bin`.
        let fnm_base = home.join(".local/state/fnm_multishells");
        if fnm_base.exists() {
            if let Ok(entries) = std::fs::read_dir(&fnm_base) {
                for entry in entries.flatten() {
                    let bin = entry.path().join("bin");
                    if bin.exists() {
                        push_unique(&mut paths, bin);
                    }
                }
            }
        }
        // NVM: every installed node version gets its own bin dir, each
        // potentially holding a stale copy of an npm-installed CLI.
        let default_alias = std::fs::read_to_string(home.join(".nvm/alias/default"))
            .ok()
            .map(|s| s.trim().to_string());
        for bin in nvm_node_bin_dirs(&home.join(".nvm/versions/node"), default_alias.as_deref()) {
            push_unique(&mut paths, bin);
        }
    }

    // Windows node version managers — different layouts from Unix:
    //   - NVM-Windows (coreybutler/nvm-windows): %NVM_HOME% (default
    //     C:\nvm); each child dir IS the version's bin (binaries live
    //     directly in <version_dir>, not <version_dir>\bin).
    //   - fnm on Windows: %LOCALAPPDATA%\fnm\node-versions\<v>\installation
    #[cfg(target_os = "windows")]
    {
        let nvm_home = std::env::var_os("NVM_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\nvm"));
        if nvm_home.exists() {
            if let Ok(entries) = std::fs::read_dir(&nvm_home) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        push_unique(&mut paths, p);
                    }
                }
            }
        }
        if let Some(localdata) = dirs::data_local_dir() {
            let fnm_base = localdata.join("fnm").join("node-versions");
            if fnm_base.exists() {
                if let Ok(entries) = std::fs::read_dir(&fnm_base) {
                    for entry in entries.flatten() {
                        let installation = entry.path().join("installation");
                        if installation.exists() {
                            push_unique(&mut paths, installation);
                        }
                    }
                }
            }
        }
    }

    // opencode's curl installer defaults to ~/.opencode/bin when neither
    // $OPENCODE_INSTALL_DIR nor $XDG_BIN_DIR is set (both handled at the
    // top). Also reachable via bun and go installs.
    if tool == "opencode" {
        if !home.as_os_str().is_empty() {
            push_unique(&mut paths, home.join("bin"));
            push_unique(&mut paths, home.join(".opencode/bin"));
            // ~/go/bin is the Unix Go install convention; on Windows
            // Go uses %USERPROFILE%\go which `home.join("go/bin")` does
            // produce correctly, so we can keep it cross-platform.
            push_unique(&mut paths, home.join("go/bin"));
        }
        if let Some(raw) = std::env::var_os("GOPATH") {
            for p in std::env::split_paths(&raw) {
                push_unique(&mut paths, p.join("bin"));
            }
        }
    }

    // Codex's standalone installer, with $CODEX_INSTALL_DIR unset (that
    // one's handled at the top), splits by platform: `~/.local/bin` on
    // Unix (`.audit-sources/codex/scripts/install/install.sh:8`, in the
    // cross-platform section above) but
    // `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` on Windows
    // (`.../install.ps1:741`) — a path nothing else in this list covers.
    // The payload itself lives in `~/.codex/packages/standalone/`; only
    // the visible bin dir named here is ever on PATH.
    #[cfg(target_os = "windows")]
    if tool == "codex" {
        if let Some(localdata) = dirs::data_local_dir() {
            push_unique(
                &mut paths,
                localdata
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin"),
            );
        }
    }

    // Claude Code's LEGACY "local installer" form: `claude
    // migrate-installer` moved the npm global install to
    // `<claude-config>/local` (superseded by today's native
    // `~/.local/bin`, but still live on machines that migrated and
    // never re-installed). It is deliberately NOT on PATH — the
    // installer exposes it through a shell alias instead, per the
    // doctor's own repair text in the shipped binary (2.1.218):
    //     Create alias: alias claude="~/.claude/local/claude"
    // so neither the dir list nor the `$PATH` catch-all can see it and
    // only the ~1s interactive-shell fallback would (aliases expand in
    // `-i` shells). One stat is much cheaper than that spawn, and on
    // Windows there IS no shell fallback at all. Honors
    // `$CLAUDE_CONFIG_DIR` like every other Claude state path.
    if tool == "claude" && !home.as_os_str().is_empty() {
        push_unique(
            &mut paths,
            crate::sessions::claude_config_root(&home).join("local"),
        );
    }

    // Grok Build's installer defaults to ~/.grok/bin on BOTH platforms
    // when $GROK_BIN_DIR is unset, and confirmed against a real install.
    // Cited from the LIVE scripts (`x.ai/cli/install.sh:157` /
    // `install.ps1:153`) because `.audit-sources/grok-build` ships no
    // installer — so unlike the other tools these line numbers can drift.
    if tool == "grok" {
        if !home.as_os_str().is_empty() {
            push_unique(&mut paths, home.join(".grok/bin"));
        }
    }

    // Cross-platform per-user installer fallbacks. Both are generic
    // (any `bun add -g` / `cargo install` binary lands there, whichever
    // tool), and both resolve under %USERPROFILE% on Windows with the
    // same joins. A third entry here, `~/.codex/bin`, was REMOVED: no
    // codex installer or version ever used it — see the note on
    // `cli_search_paths` about not inventing paths.
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join(".bun/bin"));
        push_unique(&mut paths, home.join(".cargo/bin"));
    }
    // pnpm default locations are platform-specific.
    #[cfg(target_os = "macos")]
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join("Library/pnpm"));
    }
    #[cfg(target_os = "linux")]
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join(".local/share/pnpm"));
    }

    // The process's own PATH last — catches anything our hardcoded
    // list misses (e.g. truly custom user setups). On macOS GUI launches
    // this is just the launchd minimal PATH; on terminal launches it's
    // the user's shell PATH.
    if let Some(raw) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&raw) {
            push_unique(&mut paths, p);
        }
    }

    paths
}

fn push_unique(paths: &mut Vec<std::path::PathBuf>, p: std::path::PathBuf) {
    if p.as_os_str().is_empty() {
        return;
    }
    if !paths.iter().any(|existing| existing == &p) {
        paths.push(p);
    }
}

/// mise (`https://mise.jdx.dev`) stores its shims at
/// `~/.local/share/mise/shims/` and its node installs at
/// `~/.local/share/mise/installs/node/<version>/bin`. Mirrors
/// cc-switch's `extend_mise_node_search_paths`. Unix-only — mise is
/// primarily a Unix tool and even its Windows builds use different
/// layouts that don't match this structure.
#[cfg(unix)]
fn extend_mise_node_search_paths(paths: &mut Vec<std::path::PathBuf>, home: &std::path::Path) {
    let mise = home.join(".local/share/mise");
    push_unique(paths, mise.join("shims"));
    let node_installs = mise.join("installs/node");
    if node_installs.exists() {
        if let Ok(entries) = std::fs::read_dir(&node_installs) {
            for entry in entries.flatten() {
                let bin = entry.path().join("bin");
                if bin.exists() {
                    push_unique(paths, bin);
                }
            }
        }
    }
}

/// Enumerate NVM's `<base>/<version>/bin` dirs in probe order:
/// the `~/.nvm/alias/default` version first (what a fresh shell's PATH
/// resolves to), then the rest newest-first. `read_dir` order is
/// arbitrary, so without this a stale CLI copy left in an OLD node
/// version could win [`find_cli_binary`]'s first-match probe over the
/// binary the user's terminal actually runs. Alias matching tolerates
/// a missing/extra `v` prefix (`22.21.1` vs `v22.21.1`); chained
/// aliases (`lts/*`, `node`) don't name a version dir and fall through
/// to the newest-first order.
#[cfg(unix)]
fn nvm_node_bin_dirs(
    base: &std::path::Path,
    default_alias: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut versions: Vec<(String, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let bin = entry.path().join("bin");
            if !bin.exists() {
                return None;
            }
            Some((entry.file_name().to_string_lossy().into_owned(), bin))
        })
        .collect();
    versions.sort_by(|a, b| node_version_sort_key(&b.0).cmp(&node_version_sort_key(&a.0)));
    if let Some(alias) = default_alias {
        let alias = alias.trim_start_matches('v');
        if let Some(idx) = versions
            .iter()
            .position(|(name, _)| name.trim_start_matches('v') == alias)
        {
            let default = versions.remove(idx);
            versions.insert(0, default);
        }
    }
    versions.into_iter().map(|(_, bin)| bin).collect()
}

/// Numeric sort key for a `vX.Y.Z` node version dir name, so `v9.1.0`
/// orders below `v22.21.1` (lexicographic comparison would not).
#[cfg(unix)]
fn node_version_sort_key(name: &str) -> Vec<u64> {
    name.trim_start_matches('v')
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

/// Per-platform executable-name candidates for `tool` inside `dir`.
/// On Windows we have to consider `.cmd` (npm shims) and `.exe`
/// (native installers) in addition to the bare name.
fn executable_candidates(tool: &str, dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        vec![
            dir.join(format!("{tool}.cmd")),
            dir.join(format!("{tool}.exe")),
            dir.join(tool),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![dir.join(tool)]
    }
}

/// Walk [`cli_search_paths`] and return the first `<dir>/<tool>` that
/// resolves to a real file. No subprocess, no `which::which` — pure
/// stat()-based lookup.
pub fn find_cli_binary(tool: &str) -> Option<std::path::PathBuf> {
    for dir in cli_search_paths(tool) {
        for candidate in executable_candidates(tool, &dir) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Wallclock cap for a single `<bin> --version` (or shell-fallback)
/// invocation. Real CLIs exit in tens of ms; anything past this
/// boundary is almost certainly a hang (stuck-on-network sync, broken
/// shebang stuck in interpreter prompt, runaway `.zshrc` user code via
/// `shell_version_fallback`, etc.). Kill + give up so we don't pin a
/// Tokio blocking-pool slot indefinitely.
const SUBPROCESS_TIMEOUT: std::time::Duration = crate::process::PROBE_TIMEOUT;

/// Run a silent probe to completion, killing it (and anything it
/// spawned) after [`SUBPROCESS_TIMEOUT`].
///
/// A thin alias over [`crate::process::probe`] — the module doc there
/// covers the timeout, the console-hiding flag and why the kill has to
/// reach the whole process group rather than just the direct child.
/// Returns `None` on spawn failure or timeout; a NON-ZERO exit still
/// returns `Some` (callers check `status` — the Keychain paths rely on
/// reading exit 44/36).
fn output_with_timeout(cmd: std::process::Command) -> Option<std::process::Output> {
    crate::process::probe(cmd, SUBPROCESS_TIMEOUT)
}

/// PATH with `binary`'s own directory prepended, so the CLI's runtime
/// (node for `.cmd`/shebang shims, dyld deps, …) resolves even when
/// the dir isn't on the parent process's PATH. Mirrors cc-switch's
/// per-path `Command::new(tool_path).env("PATH", new_path)` pattern.
/// Shared by the version probes and the `codex login` spawn.
pub(crate) fn augmented_path_for(binary: &std::path::Path) -> Option<String> {
    let dir = binary.parent()?;
    let current_path = std::env::var("PATH").unwrap_or_default();
    #[cfg(unix)]
    let sep = ':';
    #[cfg(not(unix))]
    let sep = ';';
    Some(format!("{}{}{}", dir.display(), sep, current_path))
}

/// Run `--version` on the resolved binary, with PATH augmented to
/// include the binary's directory (see `augmented_path_for`).
fn query_version_at(binary: &std::path::Path) -> Option<String> {
    let augmented = augmented_path_for(binary)?;
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--version").env("PATH", &augmented);
    let output = output_with_timeout(cmd)?;

    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    if raw.trim().is_empty() {
        return None;
    }
    parse_version(&raw)
}

/// Last-resort fallback when the hardcoded search list misses the
/// install (user has it in some truly custom dir reachable only via
/// `.zshrc`). Spawns the user's interactive shell so PATH-affecting
/// rc files are sourced. cc-switch's `try_get_version` uses the same
/// strategy as its primary path; we use it only as a fallback to keep
/// the hot-path detection fast.
///
/// Higher-risk timeout site: `-l -i` sources `~/.zshrc` which can run
/// arbitrary user code. [`output_with_timeout`] kills the child after
/// [`SUBPROCESS_TIMEOUT`] so a misbehaving rc file can't pin us.
#[cfg(unix)]
fn shell_version_fallback(tool: &str) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut cmd = std::process::Command::new(&shell);
    // The marker is echoed BEFORE the probe so rc output can be split
    // off — see `version_after_probe_marker`. Exit status still belongs
    // to `{tool} --version`: in a `;` sequence the shell reports the
    // last command's code.
    let probe = marked_shell_command(&format!("{tool} --version"));
    cmd.args(["-l", "-i", "-c", &probe]);
    let output = output_with_timeout(cmd)?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    parse_version(after_shell_marker(&raw)?)
}

/// Echoed immediately before ANY command we run through the user's
/// interactive login shell, so that command's real output can be told
/// apart from whatever the rc files printed.
///
/// An INTERACTIVE shell sources `.zshrc`, and plenty of setups print a
/// banner there (fastfetch/neofetch, greetings, MOTDs). All of it lands
/// on the same stdout we read, AHEAD of the real output. Two places are
/// affected and both use this marker:
///
/// * [`shell_version_fallback`] — [`parse_version`] returns the FIRST
///   version-shaped token it sees. Measured on a real machine: a
///   fastfetch banner made `codex --version` report `16.00`, the host's
///   RAM size, because `Memory  11.56 GiB / 16.00 GiB` precedes
///   `codex-cli 0.144.6` by eleven lines.
/// * `upgrade::run_upgrade` — a failed upgrade would otherwise be
///   reported with a banner line as its reason.
pub(crate) const SHELL_PROBE_MARKER: &str = "__termory_shell_probe__";

/// The `-c` payload for running `cmd` in an interactive login shell:
/// marker first, then the command with stderr folded into stdout.
///
/// Exit status still belongs to `cmd` — in a `;` sequence the shell
/// reports the last command's code.
///
/// Both callers (`shell_version_fallback`, `upgrade::upgrade_child`) are
/// unix-only, so off unix this is dead in a RELEASE build — the test build
/// keeps it alive through its own callers, which is why only release warned.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn marked_shell_command(cmd: &str) -> String {
    format!("echo {SHELL_PROBE_MARKER}; {cmd} 2>&1")
}

/// Everything after the marker — the command's own output. `None` when
/// the marker never appeared: a shell that didn't reach the `echo`
/// produced nothing trustworthy, so callers must not fall back to
/// parsing the whole text (that is exactly the bug this prevents).
///
/// Its only caller is the unix-gated `shell_version_fallback`; see
/// [`marked_shell_command`] for why that means a release-only warning.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn after_shell_marker(raw: &str) -> Option<&str> {
    // `rsplit` so a banner that happens to contain the marker text
    // can't shadow the real one — the last occurrence is ours.
    let after = raw.rsplit(SHELL_PROBE_MARKER).next()?;
    // No marker at all → rsplit yields the input unchanged.
    if after.len() == raw.len() {
        return None;
    }
    Some(after)
}

#[cfg(not(unix))]
fn shell_version_fallback(_tool: &str) -> Option<String> {
    None
}

/// How long a shell-fallback verdict stays good. See
/// [`shell_installed_cached`] for why staleness is harmless.
const SHELL_PROBE_TTL: Duration = Duration::from_secs(600);

static SHELL_PROBE: Mutex<Vec<(&'static str, Instant, bool)>> = Mutex::new(Vec::new());

/// Cached "does the shell know this tool" verdict for the HOT path.
///
/// [`shell_version_fallback`] costs ~1s per call — it spawns an
/// interactive login shell so the user's rc files get sourced. The
/// catch is `find_cli_binary(..).is_some() || shell_version_fallback(..)`:
/// the `||` only short-circuits on a HIT, so the spawn happens for
/// every CLI the directory scan missed — i.e. for every tool the user
/// does NOT have installed. [`detect_install_snapshot`] runs that on a
/// hot path (every tray-menu open, every watcher burst, on the caller
/// thread), so a user with two of the five CLIs was paying ~3s of shell
/// spawns each time they opened the menu. Measured on a stock macOS
/// zsh: 0.9-1.7s per miss.
///
/// Staleness is bounded where it matters: an install landing in ANY
/// watched dir is found by `find_cli_binary` and never reaches this
/// cache, so a fresh normal install still appears immediately. The
/// cache can only delay an install in a dir we neither scan nor watch —
/// which could not have been noticed promptly anyway — and the
/// Providers-page Recheck clears it outright
/// ([`clear_shell_probe_cache`]).
fn shell_installed_cached(tool: &'static str) -> bool {
    let now = Instant::now();
    if let Ok(cache) = SHELL_PROBE.lock() {
        if let Some((_, at, found)) = cache.iter().find(|(t, ..)| *t == tool) {
            if now.duration_since(*at) < SHELL_PROBE_TTL {
                return *found;
            }
        }
    }
    // Probe OUTSIDE the lock: this blocks for up to SUBPROCESS_TIMEOUT,
    // and holding the mutex across it would serialize every other
    // caller behind the slowest shell.
    let found = shell_version_fallback(tool).is_some();
    if let Ok(mut cache) = SHELL_PROBE.lock() {
        cache.retain(|(t, ..)| *t != tool);
        cache.push((tool, now, found));
    }
    found
}

/// Drop every cached shell-fallback verdict so the next probe re-runs
/// for real. The explicit escape hatch behind the Providers page's
/// Recheck (and the watcher's install-changed branch, where a bin dir
/// we do NOT scan may have just gained a binary).
pub fn clear_shell_probe_cache() {
    if let Ok(mut cache) = SHELL_PROBE.lock() {
        cache.clear();
    }
}

/// Whether the Codex CLI binary is installed (path scan + shell
/// fallback). Split out from [`detect_install_snapshot`] because Codex
/// alone has TWO install forms — this CLI and the desktop app — and
/// some features (account add via `codex login`, terminal resume /
/// New Session) need the binary specifically.
/// Can a `codex` CLI be RUN here — standalone binary on disk, or one
/// the user's interactive shell resolves (custom dir, alias)?
///
/// The EXECUTION-side counterpart to
/// [`codex_standalone_cli_installed`]. Use this to gate features that
/// just need to run codex; use the standalone one for anything that
/// describes codex as an installed product (version, upgrade).
pub fn codex_cli_installed() -> bool {
    codex_standalone_cli_installed() || shell_installed_cached("codex")
}

/// Is a STANDALONE codex CLI installed — i.e. is there a real binary on
/// disk that the user installed and can upgrade themselves?
///
/// This is the VERSION-side answer, and it is deliberately narrower than
/// [`codex_cli_installed`]. Codex ships in two forms and only this one
/// is a product the user manages: the desktop app carries its own copy
/// at `<bundle>/Contents/Resources/codex` (an alpha-channel build that
/// updates with the app via Sparkle, not on its own).
///
/// The interactive-shell probe is NOT consulted here. It answers "can a
/// `codex` be run in your shell", which is an EXECUTION question — it
/// cannot say WHICH codex answered, so it must not decide whether a
/// standalone install exists, what version to display, or whether an
/// upgrade can be offered. Mixing the two once made the CLI version
/// segment describe a binary the user never installed.
pub fn codex_standalone_cli_installed() -> bool {
    find_cli_binary("codex").is_some()
}

/// The Codex DESKTOP app's bundle id. Since 2026-07-09 the Codex app
/// IS the new unified ChatGPT desktop app (Chat / Work / Codex modes;
/// the old ChatGPT app became "ChatGPT Classic") — existing installs
/// updated IN PLACE, so the `.app` NAME is unreliable (`Codex.app` or
/// `ChatGPT.app` depending on install date). The bundle id is the
/// stable identity; detect by it, never by name.
#[cfg(target_os = "macos")]
const CODEX_APP_BUNDLE_ID: &str = "com.openai.codex";

/// Everything Termory reads off the installed Codex desktop app,
/// resolved in ONE pass: locating the bundle already reads its
/// Info.plist (to match `CFBundleIdentifier`), so the version and the
/// bundled CLI are derived from that same read instead of re-resolving
/// per fact.
///
/// `bundled_cli` is the codex CLI shipped INSIDE the app —
/// `<bundle>/Contents/Resources/codex`, a self-contained native binary
/// (verified runnable: `--version` reports `codex-cli <ver>`; the
/// app's Codex mode runs on it). Fallback seam for app-only installs.
/// Caveats: it rides the app's release channel (alpha builds) and the
/// path is an app implementation detail — the standalone CLI always
/// wins when present ([`codex_binary`]).
#[cfg(target_os = "macos")]
struct CodexAppInfo {
    version: Option<String>,
    bundled_cli: Option<std::path::PathBuf>,
}

/// Locate the Codex desktop app under `parents` by bundle id and read
/// its facts. Known names (`Codex.app`, `ChatGPT.app`) are checked
/// first so the common case is one or two plist reads; a full `*.app`
/// scan of each parent is the fallback for renamed bundles.
#[cfg(target_os = "macos")]
fn codex_app_info_in(parents: &[std::path::PathBuf]) -> Option<CodexAppInfo> {
    let info_of = |bundle: &std::path::Path| -> Option<CodexAppInfo> {
        let xml = std::fs::read_to_string(bundle.join("Contents").join("Info.plist")).ok()?;
        if crate::claude_desktop::plist_string_value(&xml, "CFBundleIdentifier")?
            != CODEX_APP_BUNDLE_ID
        {
            return None;
        }
        let bin = bundle.join("Contents").join("Resources").join("codex");
        Some(CodexAppInfo {
            version: crate::claude_desktop::plist_string_value(&xml, "CFBundleShortVersionString"),
            bundled_cli: bin.is_file().then_some(bin),
        })
    };
    for parent in parents {
        for name in ["Codex.app", "ChatGPT.app"] {
            if let Some(info) = info_of(&parent.join(name)) {
                return Some(info);
            }
        }
    }
    for parent in parents {
        let entries = match std::fs::read_dir(parent) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let bundle = entry.path();
            if bundle.extension().is_some_and(|ext| ext == "app") {
                if let Some(info) = info_of(&bundle) {
                    return Some(info);
                }
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn codex_app_parents() -> Vec<std::path::PathBuf> {
    let mut parents = vec![std::path::PathBuf::from("/Applications")];
    if let Some(home) = crate::home_dir() {
        parents.push(home.join("Applications"));
    }
    parents
}

#[cfg(target_os = "macos")]
fn codex_app_info() -> Option<CodexAppInfo> {
    codex_app_info_in(&codex_app_parents())
}

/// Windows distribution of the Codex/ChatGPT desktop app is
/// **Microsoft-Store-only** — there is no direct `.exe`/`.msi`
/// installer on openai.com/download, so an MSIX package is the ONLY
/// install form to check (unlike macOS, no separate `/Applications`
/// scan is needed). Windows renders the taskbar/Start DisplayName as
/// "ChatGPT" (shared branding with the unified app), but the package
/// identity stays `OpenAI.Codex` — confirmed on a real installed
/// package name, `OpenAI.Codex_26.707.9564.0_x64__2p2nqsd0c76g0`
/// (openai/codex#32772). Detect by package name, never by the
/// DisplayName.
///
/// **Presence** is detected by a filesystem scan (see
/// [`codex_appx_installed_in`]) — NOT PowerShell — because
/// [`probe_codex_installs`] runs on [`detect_install_snapshot`]'s hot
/// path (every tray-menu open, every watcher rescan, on the caller /
/// main-event thread). A `Get-AppxPackage` spawn there loads the Appx
/// module (0.5–1.5s cold) and would freeze the tray on every click,
/// whereas macOS's equivalent is a cheap plist read. The PowerShell
/// query is reserved for the app VERSION ([`codex_appx_version`]),
/// which only the cold-path `detect_codex_installs` IPC (Providers
/// page load + Recheck) consumes.
///
/// `bundled_cli` is `None` on Windows BY DECISION, not impossibility
/// (2026-07-26, real-hardware probe): the package DOES ship the full
/// CLI at `<InstallLocation>\app\resources\codex.exe` (337 MB, alpha
/// channel), and a normal non-elevated process CAN read/copy it — the
/// WindowsApps ACL denies only ROOT enumeration and IN-PLACE execution
/// (running it directly fails Access denied; a copied-out binary runs).
/// Using it would need a per-app-version managed copy, which was
/// deliberately not built — `codex_binary()` stays standalone-CLI-only
/// on this platform.
///
/// Package-family prefixes for the Codex/ChatGPT desktop app, STABLE
/// FIRST — the order is the preference order everywhere both could
/// match. The Store ships two listings sharing the publisher: "ChatGPT"
/// (identity `OpenAI.Codex`) and "ChatGPT (Beta)" (identity
/// `OpenAI.CodexBeta`, a SEPARATE package family — verified in the
/// msstore catalog 2026-07-26). A Beta-only install is still the Codex
/// desktop app, so both count as installed. The trailing `_` keeps the
/// match anchored to the family-name shape (`<Name>_<publisherhash>`)
/// so `OpenAI.Codex_` can never accidentally swallow `OpenAI.CodexBeta_`
/// or an unrelated `OpenAI.CodexSomething` identity.
#[cfg_attr(not(windows), allow(dead_code))]
const CODEX_APPX_PACKAGE_PREFIXES: [&str; 2] = ["OpenAI.Codex_", "OpenAI.CodexBeta_"];

/// Whether the Codex/ChatGPT MSIX package (stable or Beta) is installed
/// for the current user, by scanning `<local_app_data>\Packages\` for an
/// `OpenAI.Codex_<publisherhash>` / `OpenAI.CodexBeta_<publisherhash>`
/// dir (the PackageFamilyName — the publisher-hash suffix varies per
/// signing identity, so match the prefix, never a hardcoded hash). This
/// per-user Packages dir is readable (unlike `WindowsApps\`, whose
/// ROOT blocks enumeration) and is the SAME mechanism Claude
/// Desktop's Windows detection uses
/// (`msix_package_roaming_parents`). Path-injected + compiled
/// off-Windows so it's unit-testable on any host (mirrors
/// claude_desktop.rs). Verified on real Windows hardware 2026-07-26
/// (stable package `OpenAI.Codex_2p2nqsd0c76g0`).
#[cfg_attr(not(windows), allow(dead_code))]
fn codex_appx_installed_in(local_app_data: &std::path::Path) -> bool {
    std::fs::read_dir(local_app_data.join("Packages"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            // Name-prefix check FIRST (cheap, no syscall): the Packages
            // dir holds hundreds of MSIX entries, so only the rare
            // prefix match pays for the `is_dir` stat.
            e.file_name()
                .to_str()
                .is_some_and(|n| CODEX_APPX_PACKAGE_PREFIXES.iter().any(|p| n.starts_with(p)))
                && e.path().is_dir()
        })
}

#[cfg(windows)]
fn codex_appx_installed() -> bool {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) if !local.is_empty() => codex_appx_installed_in(std::path::Path::new(&local)),
        _ => false,
    }
}

/// Extract the version segment (`26.707.9564.0`) from an AppX
/// `PackageFullName` (`<Name>_<Version>_<Architecture>__<PublisherId>`,
/// the `__` marking an empty ResourceId segment). Split out from
/// [`codex_appx_version`] so the parsing is testable without a real
/// Windows install. Compiled off-Windows for that test.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_appx_package_version(full_name: &str) -> Option<String> {
    full_name.split('_').nth(1).map(|s| s.to_string())
}

/// Choose which `PackageFullName` line to report from a
/// `Get-AppxPackage -Name 'OpenAI.Codex*'` wildcard query: the STABLE
/// package when present, else the Beta — the prefix array's order.
/// Lines matching neither identity (the wildcard could catch an
/// unrelated future `OpenAI.CodexSomething`) are ignored rather than
/// mis-parsed. Split out so the preference is testable off-Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn pick_appx_full_name(stdout: &str) -> Option<String> {
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    CODEX_APPX_PACKAGE_PREFIXES
        .iter()
        .find_map(|prefix| lines.iter().find(|l| l.starts_with(prefix)))
        .map(|l| (*l).to_string())
}

/// The installed Codex/ChatGPT MSIX package version via PowerShell's
/// `Get-AppxPackage` — the supported way to read package identity
/// without touching the ACL-restricted `WindowsApps` directory. Queries
/// the CURRENT USER's packages only (no `-AllUsers`, so no elevation),
/// with the `OpenAI.Codex*` wildcard so the Beta package
/// (`OpenAI.CodexBeta`) is found too; `pick_appx_full_name` prefers
/// stable when both are installed. COLD PATH ONLY
/// (`detect_codex_installs` IPC) — never call from
/// `detect_install_snapshot` (see [`codex_appx_installed_in`] for why).
/// Best-effort: any failure (PowerShell missing, package absent,
/// timeout) is just `None`, never an error. Verified on real Windows
/// hardware 2026-07-26 (returned `OpenAI.Codex_26.721.4979.0_x64__…`).
#[cfg(windows)]
fn codex_appx_version() -> Option<String> {
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "(Get-AppxPackage -Name 'OpenAI.Codex*' | Select-Object -ExpandProperty PackageFullName)",
    ]);
    let output = output_with_timeout(cmd)?;
    if !output.status.success() {
        return None;
    }
    let full_name = pick_appx_full_name(&String::from_utf8_lossy(&output.stdout))?;
    parse_appx_package_version(&full_name)
}

/// The desktop app's bundled codex CLI, when the app is installed and
/// ships one. macOS-only (like the app detection itself).
pub fn codex_bundled_cli() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        codex_app_info().and_then(|info| info.bundled_cli)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Resolve a RUNNABLE codex binary: the standalone CLI first, else the
/// desktop app's bundled copy. Every codex spawn/launch seam (account
/// login, terminal resume / new-session, tray terminal gating) routes
/// through this, so an app-only install still gets CLI-backed features.
/// Stat + plist reads only — no shell fallback (callers that also want
/// the interactive-shell probe use [`codex_cli_installed`] for the
/// standalone half).
pub fn codex_binary() -> Option<std::path::PathBuf> {
    find_cli_binary("codex").or_else(codex_bundled_cli)
}

/// Codex's install forms, probed in ONE pass (a single bundle
/// resolution feeds `app` / `app_version` / `bundled_cli`).
///
/// `cli` is the VERSION-side answer — a standalone binary found on disk
/// ([`codex_standalone_cli_installed`]), NOT "some codex is runnable".
/// It drives the CLI version segment, its version number, and whether
/// an upgrade is offered, all of which describe that one product. The
/// execution side (login / resume / new session / tray gating) reads
/// `codex_binary()` or `InstallSnapshot::codex_terminal` instead, and
/// those DO fall back to the bundled copy.
/// Serialized camelCase — this is the `detect_codex_installs` IPC's
/// wire shape (frontend `CodexInstalls` in src/types.ts).
#[derive(Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInstalls {
    pub cli: bool,
    pub app: bool,
    pub app_version: Option<String>,
    pub bundled_cli: bool,
}

pub fn probe_codex_installs() -> CodexInstalls {
    let cli = codex_standalone_cli_installed();
    #[cfg(target_os = "macos")]
    {
        let info = codex_app_info();
        CodexInstalls {
            cli,
            app: info.is_some(),
            app_version: info.as_ref().and_then(|i| i.version.clone()),
            bundled_cli: info.as_ref().is_some_and(|i| i.bundled_cli.is_some()),
        }
    }
    #[cfg(windows)]
    {
        // Presence only (cheap filesystem scan) — the version needs a
        // slow PowerShell spawn and is filled in by
        // `probe_codex_installs_detailed` on the cold path.
        CodexInstalls {
            cli,
            app: codex_appx_installed(),
            app_version: None,
            bundled_cli: false,
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        CodexInstalls {
            cli,
            app: false,
            app_version: None,
            bundled_cli: false,
        }
    }
}

/// Full Codex install probe INCLUDING the Windows app version — for the
/// `detect_codex_installs` IPC ONLY (Providers page load + Recheck),
/// never the hot path. On Windows the version costs a PowerShell
/// `Get-AppxPackage` spawn, so it's fetched here rather than in
/// [`probe_codex_installs`]; macOS already has the version from the
/// plist read the base probe does, so this is a passthrough there.
pub fn probe_codex_installs_detailed() -> CodexInstalls {
    let base = probe_codex_installs();
    #[cfg(windows)]
    if base.app && base.app_version.is_none() {
        return CodexInstalls {
            app_version: codex_appx_version(),
            ..base
        };
    }
    base
}

/// One probe pass for everything the tray and the `detect_clis` IPC
/// need: the per-app installed map PLUS whether a terminal-runnable
/// codex exists. The two answer different questions for Codex — the
/// map says "provider management usable" (CLI or desktop app, shared
/// `~/.codex/`), `codex_terminal` says "a `codex` invocation can run
/// in a terminal" (standalone CLI — including the shell fallback,
/// since the user's login shell resolves what our fixed-dir scan may
/// miss — or the app's bundled binary). Probing them together keeps
/// one pass over the disk, and the tray stores the WHOLE snapshot in
/// its staleness compare so a codex-terminal flip (CLI installed
/// while the app was already present) rebuilds the menu.
#[derive(Clone, PartialEq)]
pub struct InstallSnapshot {
    pub map: std::collections::HashMap<CliApp, bool>,
    pub codex_terminal: bool,
    /// Settings → Tools: cli keys toggled OFF, read once per probe pass
    /// (caller's thread) so the tray's main-thread consumers
    /// (`build_menu`, `terminal_clis`) do zero config-file I/O. Also
    /// participates in the tray's staleness compare, so a toggle change
    /// self-heals on the next menu open even without the explicit
    /// rebuild the toggle write triggers.
    pub disabled: std::collections::HashSet<String>,
}

/// Report whether each supported CLI is installed. Path-only scan
/// when the binary lives anywhere reachable via [`cli_search_paths`];
/// falls back to spawning an interactive shell only when the scan
/// fails (so the hot-path stays fast for users whose CLIs are in
/// well-known locations).
///
/// We check the binary, not config files — every CLI creates its
/// config dir lazily (only after first run / login), so config-file
/// presence is a poor proxy for "is this installed".
///
/// Codex counts as installed when EITHER the CLI binary OR the desktop
/// app is present (see [`InstallSnapshot`] for the codex_terminal
/// split); account add / re-login gate on the `detect_codex_installs`
/// IPC frontend-side.
pub fn detect_install_snapshot() -> InstallSnapshot {
    let codex = probe_codex_installs();
    let mut map = std::collections::HashMap::new();
    for app in CliApp::all() {
        let installed = match app {
            // EXECUTION side — "can codex be managed here at all". Uses
            // the wide answer (incl. the shell probe) so a CLI installed
            // somewhere the fixed-dir scan misses still gets its tab,
            // not the InstallGuide. The narrow `codex.cli` is for the
            // version segment only.
            CliApp::Codex => codex_cli_installed() || codex.app,
            // Claude Desktop is a GUI app with no CLI binary — detect it
            // by its on-disk config dir (and platform support) instead.
            CliApp::ClaudeDesktop => crate::claude_desktop::is_installed(),
            _ => {
                find_cli_binary(app.bin_name()).is_some() || shell_installed_cached(app.bin_name())
            }
        };
        map.insert(app, installed);
    }
    InstallSnapshot {
        map,
        // EXECUTION side: `codex.cli` is standalone-only, so the shell
        // probe is added back here — the user's terminal resolves what
        // the fixed-dir scan misses, and any runnable codex will do.
        codex_terminal: codex.cli || codex.bundled_cli || shell_installed_cached("codex"),
        disabled: crate::config::disabled_sources(),
    }
}

/// One CLI's installed version. Costs a subprocess (`--version`, plus
/// the interactive-shell fallback when the binary isn't in the search
/// list), so callers that need a SINGLE tool must use this rather than
/// running [`detect_cli_versions`] and indexing the result — that would
/// probe all six apps to read one.
///
/// The Codex value is the CLI binary's version ONLY (None when just the
/// desktop app is installed) — the app's bundle version rides in the
/// `detect_codex_installs` IPC, and the frontend composes the
/// "CLI vX · App vY" display from both.
pub fn detect_cli_version(app: CliApp) -> Option<String> {
    // Claude Desktop is a GUI app with no `--version` — read its app
    // bundle version instead of probing a binary.
    if !app.is_cli() {
        return crate::claude_desktop::version();
    }
    find_cli_binary(app.bin_name())
        .and_then(|p| query_version_at(&p))
        .or_else(|| shell_version_fallback(app.bin_name()))
}

/// Every installed CLI's version. Spawns one subprocess per CLI, so the
/// frontend calls this on page-load + Recheck only, never inside hot
/// paths. For a single tool use [`detect_cli_version`].
pub fn detect_cli_versions() -> std::collections::HashMap<CliApp, Option<String>> {
    CliApp::all()
        .into_iter()
        .map(|app| (app, detect_cli_version(app)))
        .collect()
}

/// Pull the first `MAJOR.MINOR[.PATCH][-suffix]` token out of free-form
/// `--version` output. Tolerates leading `v`, trailing parenthetical
/// build info, ANSI escapes, etc.
fn parse_version(text: &str) -> Option<String> {
    for token in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
        let candidate = token.trim_start_matches('v').trim();
        if candidate.is_empty() || !candidate.contains('.') {
            continue;
        }
        let mut chars = candidate.chars().peekable();
        if !matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
            continue;
        }
        // Walk: digits, dots, then optional -prerelease.
        let mut end = 0;
        let mut seen_dot = false;
        for (i, c) in candidate.char_indices() {
            if c.is_ascii_digit() {
                end = i + c.len_utf8();
            } else if c == '.' {
                seen_dot = true;
                end = i + c.len_utf8();
            } else if (c == '-' || c.is_ascii_alphabetic()) && seen_dot {
                // Accept SemVer prerelease / build metadata.
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        if !seen_dot {
            continue;
        }
        let trimmed = candidate[..end].trim_end_matches('.');
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Use the CLI's native account/OAuth login. Activating means
    /// clearing the Termory-injected fields from the live config so
    /// the CLI falls back to its native auth flow.
    Official,
    /// Third-party API platform. Activating writes base_url + api_key
    /// + model into the live config in the per-CLI shape.
    Custom,
}

/// A stored third-party API platform for one CLI — a named snapshot of
/// `{base_url, api_key, model, …}` the user can activate. One library
/// per CLI, persisted as an array in `~/.termory/providers.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub app: CliApp,
    pub kind: ProviderKind,
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    /// OpenCode-only: the AI SDK npm package OpenCode loads for this
    /// provider — written verbatim to opencode.json `provider.<id>.npm`
    /// (the official config field, e.g. "@ai-sdk/openai-compatible").
    /// Empty/None → defaults to "@ai-sdk/openai-compatible".
    /// Read ONLY by `activate_opencode`; `activate()` dispatches by
    /// `app`, so for a non-OpenCode provider this is inert storage and
    /// is never consulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// OpenCode-only: extra models surfaced in OpenCode's picker
    /// alongside the primary top-level `model`. Each is written as a
    /// `models: { <id>: { name: <name> } }` entry (name defaults to the
    /// id when blank). Like `npm`, read ONLY by `activate_opencode` —
    /// inert for every other app.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ProviderModel>,
    /// User-defined provider options ("Advanced settings" in the editor)
    /// merged into the CLI's live config on activation and stripped on
    /// switch/deactivate. `key` is a dot-path (`env.FOO`,
    /// `tools.web.enabled`); `value` is type-inferred (`true`/`false` →
    /// bool, numeric → number, else string) for JSON/TOML targets and
    /// kept verbatim for Gemini's `.env`. Serialized as `options`.
    /// See `apply_*_overrides` / `override_keys`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ProviderOption>,
    /// Cached favicon as a `data:image/...;base64,...` URL. Populated
    /// at create / edit time via `fetch_favicon` so the ProviderCard
    /// can render the brand mark locally without making a network
    /// request on every render (and without leaking the provider's
    /// hostname to Google's s2 service the way the legacy live-fetch
    /// did). `None` ⇒ fall back to the letter avatar in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    /// Grok-only: the wire API each `[model.*]` entry declares
    /// (`api_backend` = chat_completions | responses | messages). When
    /// unset/blank, `activate_grok` omits the field so Grok applies its own
    /// default (`chat_completions`). Inert storage for every other app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
}

/// One user-defined provider option ("Advanced settings" entry). `key`
/// is a dot-path into the CLI's config (`env.FOO`, `tools.web.enabled`);
/// `value` is the raw string the user typed (type-inferred per target
/// format at write time).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderOption {
    pub key: String,
    pub value: String,
}

/// One extra OpenCode model: `id` is the model id (the key in OpenCode's
/// `models` map), `name` its display label. Blank `name` falls back to
/// `id` at write time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

impl Provider {
    /// The npm package OpenCode should load for this provider — the
    /// official `provider.<id>.npm` field. Empty/None falls back to the
    /// OpenAI Responses adapter (`@ai-sdk/openai`).
    fn opencode_npm(&self) -> &str {
        self.npm
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(OPENCODE_DEFAULT_NPM)
    }
}

const OPENCODE_DEFAULT_NPM: &str = "@ai-sdk/openai";

// ===================================================================
// Gateway → Provider synthesis. Mirrors the frontend
// `providerFromBinding` / `gatewayBaseForProtocol` / `protocolForBinding`
// (src/lib/provider-utils.ts) — keep the two in sync. Lets the tray
// surface gateway bindings as activatable providers via the SAME
// `activate` / `read_active_state` path standalone providers use.
// ===================================================================

/// One gateway binding: a CLI target of a gateway, minus the gateway's
/// shared base/key, with its own id. Protocol is derived from app / npm.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayBinding {
    pub id: String,
    pub app: CliApp,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    #[serde(default)]
    pub options: Vec<ProviderOption>,
    /// Grok-only: `api_backend` for each model entry (see `Provider`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
}

/// A gateway: one `{baseUrl, apiKey}` fanned out to several CLIs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gateway {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    // Per-binding lenient: a binding for an app this build doesn't know (e.g. a
    // `grok` binding read by an older version) is dropped, but the gateway and
    // its other bindings survive — the unknown feature is just absent from the
    // UI. See the "Lenient config parsing" note above `providers_from_json`.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub bindings: Vec<GatewayBinding>,
    #[serde(default)]
    pub favicon: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum GatewayProtocol {
    OpenaiCompatible,
    Openai,
    Anthropic,
    Gemini,
}

fn protocol_for_npm(npm: &str) -> GatewayProtocol {
    if npm.contains("anthropic") || npm.contains("bedrock") {
        GatewayProtocol::Anthropic
    } else if npm.contains("google") {
        GatewayProtocol::Gemini
    } else if npm.contains("openai-compatible") {
        GatewayProtocol::OpenaiCompatible
    } else if npm.contains("openai") {
        GatewayProtocol::Openai
    } else {
        GatewayProtocol::OpenaiCompatible
    }
}

fn protocol_for_binding(b: &GatewayBinding) -> GatewayProtocol {
    match b.app {
        CliApp::Claude => GatewayProtocol::Anthropic,
        CliApp::Codex => GatewayProtocol::Openai,
        CliApp::Gemini => GatewayProtocol::Gemini,
        CliApp::Opencode => protocol_for_npm(b.npm.as_deref().unwrap_or("")),
        // xAI's API is OpenAI-compatible chat completions.
        CliApp::Grok => GatewayProtocol::OpenaiCompatible,
        // Claude Desktop binds Anthropic-capable gateways (its 3P gateway
        // speaks the Anthropic Messages format) — same as Claude Code.
        CliApp::ClaudeDesktop => GatewayProtocol::Anthropic,
    }
}

fn npm_for_protocol(p: GatewayProtocol) -> &'static str {
    match p {
        GatewayProtocol::OpenaiCompatible => "@ai-sdk/openai-compatible",
        GatewayProtocol::Openai => "@ai-sdk/openai",
        GatewayProtocol::Anthropic => "@ai-sdk/anthropic",
        GatewayProtocol::Gemini => "@ai-sdk/google",
    }
}

/// Derive a CLI's real base URL from the gateway's path-less root: strip a
/// trailing `/v1beta` or `/v1`, then re-add `/v1` for the OpenAI flavors
/// (Anthropic / Gemini keep the bare root and append their own path).
fn gateway_base_for_protocol(base: &str, p: GatewayProtocol) -> String {
    let mut b = base.trim().trim_end_matches('/');
    b = b.strip_suffix("/v1beta").unwrap_or(b);
    b = b.strip_suffix("/v1").unwrap_or(b);
    match p {
        GatewayProtocol::OpenaiCompatible | GatewayProtocol::Openai => format!("{b}/v1"),
        GatewayProtocol::Anthropic | GatewayProtocol::Gemini => b.to_string(),
    }
}

fn provider_from_binding(g: &Gateway, b: &GatewayBinding) -> Provider {
    let protocol = protocol_for_binding(b);
    let is_opencode = b.app == CliApp::Opencode;
    Provider {
        id: b.id.clone(),
        app: b.app,
        kind: ProviderKind::Custom,
        name: g.name.clone(),
        base_url: gateway_base_for_protocol(&g.base_url, protocol),
        api_key: g.api_key.clone(),
        model: b.model.clone(),
        npm: if is_opencode {
            Some(
                b.npm
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| npm_for_protocol(protocol).to_string()),
            )
        } else {
            None
        },
        // Models list — OpenCode extra models, Claude Desktop inferenceModels,
        // AND grok's required model list.
        models: if matches!(
            b.app,
            CliApp::Opencode | CliApp::ClaudeDesktop | CliApp::Grok
        ) {
            b.models.clone()
        } else {
            Vec::new()
        },
        options: b.options.clone(),
        favicon: g.favicon.clone(),
        api_backend: b.api_backend.clone(),
    }
}

/// Every gateway binding synthesized as an activatable Provider (id = the
/// binding's own id, `app` = its target CLI). Reads the gateways from
/// `~/.termory/providers.json` ONCE; returns `[]` on any read/parse failure.
/// Callers filter by `app` (the tray reads this once and groups per CLI
/// rather than re-reading the file per CLI).
pub fn gateway_providers() -> Vec<Provider> {
    gateways_from_json(crate::config::read_gateways().unwrap_or_default())
        .iter()
        .flat_map(|g| g.bindings.iter().map(|b| provider_from_binding(g, b)))
        .collect()
}

// ── Lenient config parsing (LOCKED) ──────────────────────────────────────────
// Config content that is valid JSON must NEVER make the code error. An entry
// this build doesn't recognize — the canonical case is an OLDER binary reading
// a NEWER version's file after a downgrade, e.g. a `grok` provider/binding for
// an app this build has no feature for — is Termory's OWN legitimate data from
// another version, so it is simply SKIPPED (not shown in this version's UI),
// and everything else keeps working. Only a real JSON *syntax* error may fail.
// This is NOT a license to accept arbitrary foreign junk — it's version-skew
// tolerance for the project's own data.

/// Parse a JSON array of provider entries into typed `Provider`s, skipping any
/// single entry the build can't deserialize (unknown `app`/`kind`, corrupt
/// row). One bad entry never empties the whole list.
pub fn providers_from_json(value: JsonValue) -> Vec<Provider> {
    let JsonValue::Array(arr) = value else {
        return Vec::new();
    };
    arr.into_iter()
        .filter_map(|e| match serde_json::from_value::<Provider>(e) {
            Ok(p) => Some(p),
            Err(err) => {
                log::warn!("Skipping unrecognized provider entry: {err}");
                None
            }
        })
        .collect()
}

/// Same per-entry tolerance for the gateway list. Note a gateway's *bindings*
/// are ALSO parsed leniently (see `lenient_vec` on `Gateway.bindings`), so an
/// unknown-app binding drops just that binding, not the whole gateway.
pub fn gateways_from_json(value: JsonValue) -> Vec<Gateway> {
    let JsonValue::Array(arr) = value else {
        return Vec::new();
    };
    arr.into_iter()
        .filter_map(|e| match serde_json::from_value::<Gateway>(e) {
            Ok(g) => Some(g),
            Err(err) => {
                log::warn!("Skipping unrecognized gateway entry: {err}");
                None
            }
        })
        .collect()
}

/// `#[serde(deserialize_with)]` helper: deserialize a `Vec<T>` element-by-
/// element, dropping (with a warn) any element that fails to parse — so one
/// unrecognized item in a list FIELD (e.g. a gateway binding for an app this
/// build doesn't know) doesn't fail the whole containing struct.
fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw = Vec::<JsonValue>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| match serde_json::from_value::<T>(v) {
            Ok(x) => Some(x),
            Err(err) => {
                log::warn!("Skipping unrecognized list entry: {err}");
                None
            }
        })
        .collect())
}

/// Tauri command-argument wrapper for a providers array that deserializes
/// LENIENTLY (via `providers_from_json`): an entry the build can't parse is
/// dropped instead of failing the whole command at Tauri's arg-binding layer,
/// BEFORE the handler runs. Without it, one unknown provider in providers.json
/// makes `provider_active_states` (and the other list-arg commands) fail
/// outright — the `unknown variant 'grok'` bug.
#[derive(Debug, Default, Clone)]
pub struct ProviderList(pub Vec<Provider>);

impl<'de> Deserialize<'de> for ProviderList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(ProviderList(providers_from_json(JsonValue::deserialize(
            deserializer,
        )?)))
    }
}

/// Reverse-derived active state for a single CLI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveState {
    pub app: CliApp,
    pub kind: ActiveKind,
    /// When kind=Custom, the id of the matched Provider from the
    /// user's list (or None when no Provider matches and the state
    /// is "Unmanaged"). For OpenCode this means "the one set as
    /// default" (top-level `model` points at it).
    pub matched_provider_id: Option<String>,
    /// Reverse-derived snapshot of what's actually in live config.
    /// Always populated when kind != Official (used for the
    /// Unmanaged banner).
    pub live_snapshot: Option<LiveSnapshot>,
    /// Path of the file(s) consulted, for "open in finder" UX.
    pub live_path: String,
    /// OpenCode-only: ids of Termory providers whose slots are
    /// currently in opencode.json (i.e. "activated"). Activated and
    /// default are distinct concepts for OpenCode — multiple slots
    /// can coexist, only one can be the default. Empty for other CLIs
    /// (which are single-slot).
    #[serde(default)]
    pub configured_provider_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActiveKind {
    Official,
    Custom,
    Unmanaged,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    pub base_url: Option<String>,
    pub api_key_masked: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: u128,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResult {
    pub ok: bool,
    pub models: Vec<String>,
    pub status: Option<u16>,
    pub message: String,
}

// ===================================================================
// Gateway API-mode detection
// ===================================================================
//
// A gateway is one `{baseUrl, apiKey}` that may speak several API modes.
// Each mode is probed at ITS OWN real API endpoint (a list endpoint never
// gates a capability — it only proves an unrelated list route exists, and
// gives false positives). Naming mirrors the AI SDK packages OpenCode uses:
//   - openaiCompatible → POST /v1/chat/completions   (@ai-sdk/openai-compatible) → OpenCode
//   - openai           → POST /v1/responses          (@ai-sdk/openai)            → Codex + OpenCode
//   - anthropic        → POST /v1/messages           (@ai-sdk/anthropic)         → Claude + OpenCode
//   - gemini           → GET  /v1beta/models?key= returns data (@ai-sdk/google)  → Gemini + OpenCode
//     (Gemini-SPECIFIC path — a non-Gemini gateway 404s it, so data = support;
//     contrast OpenAI's generic /v1/models which every compatible gateway answers)
// `models` is a single flat catalog (union of the two GET /models lists)
// used ONLY as autocomplete candidates — the gateway routes by model id,
// so there's no reliable per-mode model split. Probes never spend tokens.

/// Which API modes a gateway supports + a flat model-id catalog for the
/// binding editor's autocomplete.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayCapabilities {
    /// OpenAI Chat Completions (`/v1/chat/completions`) — `@ai-sdk/openai-compatible`.
    pub openai_compatible: bool,
    /// OpenAI Responses (`/v1/responses`) — `@ai-sdk/openai`; Codex requires it.
    pub openai: bool,
    pub anthropic: bool,
    pub gemini: bool,
    #[serde(default)]
    pub models: Vec<String>,
}

// ===================================================================
// Activation entry point
// ===================================================================

/// Dispatch activate to the per-CLI write functions. Only Custom
/// providers reach here — the frontend's Official card has its own
/// path through `deactivate` directly, so we don't accept Official
/// kind here.
pub fn activate(provider: &Provider, providers_for_app: &[Provider]) -> Result<(), Box<dyn Error>> {
    if provider.kind == ProviderKind::Official {
        return Err("activate() does not accept Official kind — call deactivate() instead.".into());
    }
    // A provider's Base URL is its whole point, and BOTH editors already refuse
    // to save without one (`ProviderEditor.canSave` / `GatewayEditor.canSave`),
    // so an empty one means hand-edited providers.json or a bug — never a user
    // choice. Reject it HERE, at the one dispatch every caller goes through.
    //
    // Without this the per-CLI writers took the empty string as "clear this
    // field" (their write-when-non-empty / strip-when-empty rule, which exists
    // so a CLEARED field doesn't leave a stale value behind): activating such a
    // provider stripped the base URL while still writing the API key, leaving
    // the CLI pointed at the OFFICIAL endpoint holding a third party's token —
    // a half-switched state that reads as "activated" everywhere in the UI.
    // Grok and Claude Desktop already errored; the other four did not.
    if provider.base_url.trim().is_empty() {
        return Err(format!(
            "Provider \"{}\" is missing a Base URL",
            provider.name.trim()
        )
        .into());
    }
    match provider.app {
        CliApp::Claude => activate_claude(provider, providers_for_app),
        CliApp::Codex => activate_codex(provider, providers_for_app),
        CliApp::Gemini => activate_gemini(provider, providers_for_app),
        CliApp::Opencode => activate_opencode(provider, providers_for_app),
        CliApp::Grok => activate_grok(provider, providers_for_app),
        CliApp::ClaudeDesktop => crate::claude_desktop::apply(provider),
    }
}

/// Clear all Termory-injected fields from the live config so the CLI
/// falls back to its native auth flow.
pub fn deactivate(app: CliApp, providers_for_app: &[Provider]) -> Result<(), Box<dyn Error>> {
    match app {
        CliApp::Claude => deactivate_claude(providers_for_app),
        CliApp::Codex => deactivate_codex(providers_for_app),
        CliApp::Gemini => deactivate_gemini(providers_for_app),
        CliApp::Opencode => deactivate_opencode(providers_for_app),
        CliApp::Grok => deactivate_grok(providers_for_app),
        CliApp::ClaudeDesktop => crate::claude_desktop::restore_official(),
    }
}

/// Surgical per-provider cleanup, used when deleting a single
/// provider so we don't accidentally wipe siblings. For
/// Claude / Codex / Gemini this is a no-op (single-slot CLIs —
/// the delete flow runs `deactivate` when the provider is in use).
/// For OpenCode it strips this provider's `termory-<id>` slot from
/// opencode.json and clears the top-level `model` if it pointed
/// here; sibling Termory slots stay configured.
pub fn delete_provider_traces(provider: &Provider) -> Result<(), Box<dyn Error>> {
    if provider.kind == ProviderKind::Official {
        return Ok(());
    }
    match provider.app {
        // Single-slot CLIs (and Claude Desktop's single 3P profile) need
        // no surgical cleanup here — the delete flow runs `deactivate`
        // when the provider being removed is the one in use.
        CliApp::Claude | CliApp::Codex | CliApp::Gemini | CliApp::ClaudeDesktop => Ok(()),
        CliApp::Opencode => delete_opencode_provider_entry(provider),
        // Multi-slot (like OpenCode): remove only this provider's entries +
        // clear the default if it pointed here; sibling slots survive.
        CliApp::Grok => delete_grok_provider_entry(provider),
    }
}

/// Promote a multi-slot provider to its CLI's startup default. OpenCode and
/// Grok are the multi-slot CLIs (separate enable vs. set-default steps); the
/// single-slot CLIs set their default implicitly on activate, so this only
/// dispatches those two. `all` = this app's providers, needed by Grok to
/// strip the previous default's global Advanced settings before applying the
/// new one's (OpenCode's options are per-block, so it ignores `all`).
pub fn set_default(p: &Provider, all: &[Provider]) -> Result<(), Box<dyn Error>> {
    match p.app {
        CliApp::Opencode => set_opencode_default(p),
        CliApp::Grok => set_grok_default(p, all),
        other => Err(format!("set_default is only for multi-slot CLIs, not {other:?}").into()),
    }
}

// ===================================================================
// Read active state (per-CLI reverse derivation)
// ===================================================================

pub fn read_active_state(
    app: CliApp,
    providers_for_app: &[Provider],
) -> Result<ActiveState, Box<dyn Error>> {
    match app {
        CliApp::Claude => read_active_claude(providers_for_app),
        CliApp::Codex => read_active_codex(providers_for_app),
        CliApp::Gemini => read_active_gemini(providers_for_app),
        CliApp::Opencode => read_active_opencode(providers_for_app),
        CliApp::Grok => read_active_grok(providers_for_app),
        CliApp::ClaudeDesktop => crate::claude_desktop::read_active(providers_for_app),
    }
}

// ===================================================================
// Claude Code
// ===================================================================
//
// File: ~/.claude/settings.json
// Custom: writes env.ANTHROPIC_BASE_URL + env.ANTHROPIC_AUTH_TOKEN
//         + env.ANTHROPIC_MODEL (when set)
// Official: removes those env keys
// Reverse: read env block, compare to provider list
//
// OAuth credentials live in a separate file (~/.claude/.credentials.json
// or the macOS Keychain — see auth.ts:1323) which we never touch, so
// switching to a Custom provider and back leaves the OAuth login
// intact automatically.

fn claude_settings_path() -> Result<PathBuf, Box<dyn Error>> {
    // User settings live under Claude's config home — `$CLAUDE_CONFIG_DIR`
    // when set, else `~/.claude` (official: `getSettingsRootPathForSource`
    // → `getClaudeConfigHomeDir`, settings.ts:285). Writing the hardcoded
    // `~/.claude` would silently miss for a relocated config dir.
    Ok(crate::sessions::claude_config_root(&home()?).join("settings.json"))
}

fn activate_claude(p: &Provider, all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let path = claude_settings_path()?;
    let mut root = load_json_object(&path)?;
    let env = ensure_json_object(&mut root, "env")?;
    if !p.base_url.is_empty() {
        env.insert(
            "ANTHROPIC_BASE_URL".into(),
            JsonValue::String(p.base_url.clone()),
        );
    } else {
        env.remove("ANTHROPIC_BASE_URL");
    }
    // Claude reads ANTHROPIC_AUTH_TOKEN first (treated as OAuth-style
    // bearer in `src/utils/auth.ts:164`), and falls back to
    // ANTHROPIC_API_KEY. We always write AUTH_TOKEN and clear API_KEY
    // — covers ~all known third-party gateways. Users who hit a
    // platform that requires API_KEY can edit settings.json directly.
    env.remove("ANTHROPIC_API_KEY");
    if !p.api_key.is_empty() {
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".into(),
            JsonValue::String(p.api_key.clone()),
        );
    } else {
        env.remove("ANTHROPIC_AUTH_TOKEN");
    }
    // Main model goes into env.ANTHROPIC_MODEL — matches cc-switch
    // and Claude Code's priority chain (model.ts:69:
    // `process.env.ANTHROPIC_MODEL || settings.model`).
    if !p.model.is_empty() {
        env.insert("ANTHROPIC_MODEL".into(), JsonValue::String(p.model.clone()));
    } else {
        env.remove("ANTHROPIC_MODEL");
    }
    // Per-size routing (Haiku / Sonnet / Opus picks in Claude Code's
    // `/model` menu) is no longer a dedicated field — users express it
    // through overrides as `env.ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL`
    // (those keys are NOT in `override_key_is_managed`, so they pass
    // through). 1M context is declared by appending `[1m]` directly to
    // the override value (e.g. `claude-sonnet-4-6[1m]`).
    //
    // Clear every provider's override keys, then apply this one's
    // (env.* kept as strings — Claude's `env` is Record<string,string>).
    strip_json_overrides(&mut root, &override_keys(all, CliApp::Claude));
    apply_claude_overrides(&mut root, p);
    write_json_object(&path, &root)
}

fn deactivate_claude(all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(());
    }
    let mut root = load_json_object(&path)?;
    if let Some(JsonValue::Object(env)) = root.get_mut("env") {
        env.remove("ANTHROPIC_BASE_URL");
        env.remove("ANTHROPIC_AUTH_TOKEN");
        env.remove("ANTHROPIC_API_KEY");
        env.remove("ANTHROPIC_MODEL");
        env.remove("ANTHROPIC_DEFAULT_HAIKU_MODEL");
        env.remove("ANTHROPIC_DEFAULT_SONNET_MODEL");
        env.remove("ANTHROPIC_DEFAULT_OPUS_MODEL");
        if env.is_empty() {
            root.remove("env");
        }
    }
    strip_json_overrides(&mut root, &override_keys(all, CliApp::Claude));
    write_json_object(&path, &root)
}

fn read_active_claude(providers: &[Provider]) -> Result<ActiveState, Box<dyn Error>> {
    let path = claude_settings_path()?;
    let live_path = path.display().to_string();
    if !path.exists() {
        return Ok(ActiveState {
            app: CliApp::Claude,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }
    let root = load_json_object(&path)?;
    let env = root.get("env").and_then(|v| v.as_object());
    let base_url = env
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let auth_token = env
        .and_then(|e| e.get("ANTHROPIC_AUTH_TOKEN"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let api_key = env
        .and_then(|e| e.get("ANTHROPIC_API_KEY"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let chosen_key = auth_token.clone().or(api_key.clone());
    // Read model from env.ANTHROPIC_MODEL first (where we now write
    // it). Fall back to the top-level `model` for settings.json files
    // produced by older Termory versions — keeps the reverse match
    // working during the transition.
    let model = env
        .and_then(|e| e.get("ANTHROPIC_MODEL"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            root.get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

    // No injection → Official
    if base_url.is_none() && chosen_key.is_none() {
        return Ok(ActiveState {
            app: CliApp::Claude,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }

    let snapshot = LiveSnapshot {
        base_url: base_url.clone(),
        api_key_masked: chosen_key.as_deref().map(mask_secret),
        model: model.clone(),
    };

    // Match against the user's provider list.
    let matched = providers.iter().find(|p| {
        p.app == CliApp::Claude
            && p.kind == ProviderKind::Custom
            && string_match(&p.base_url, base_url.as_deref())
            && string_match(&p.api_key, chosen_key.as_deref())
    });

    Ok(ActiveState {
        app: CliApp::Claude,
        kind: if matched.is_some() {
            ActiveKind::Custom
        } else {
            ActiveKind::Unmanaged
        },
        matched_provider_id: matched.map(|p| p.id.clone()),
        live_snapshot: Some(snapshot),
        live_path,
        configured_provider_ids: Vec::new(),
    })
}

// ===================================================================
// Codex
// ===================================================================
//
// Files: ~/.codex/auth.json + ~/.codex/config.toml
// Custom: writes auth.json's OPENAI_API_KEY + config.toml's
//         model_provider + [model_providers.termory] block + model
// Official: removes model_provider, removes [model_providers.termory],
//           removes OPENAI_API_KEY from auth.json
// Reverse: read config.toml's model_provider. If "termory" (or another
//          non-reserved id we wrote), read its base_url + model and
//          match. Otherwise Official.

/// Resolve Codex's home directory the way the CLI itself does
/// (`codex-rs/utils/home-dir/src/lib.rs:14-59`): the `CODEX_HOME`
/// environment variable when set and non-empty (an absolute override),
/// otherwise `~/.codex`. Used for the credential / config files Termory
/// reads & writes (auth.json / config.toml) so a user who relocated
/// `CODEX_HOME` still gets provider-switching, quota, and account
/// management against the right files. Takes `home` so the env-free
/// tests (HOME override) stay deterministic.
pub(crate) fn codex_root(home: &Path) -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".codex"),
    }
}

fn codex_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(codex_root(&home()?))
}

pub(crate) fn codex_auth_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(codex_dir()?.join("auth.json"))
}

/// Read the `latest_version` field from `~/.codex/version.json`.
/// Codex writes this file when checking for updates — it is always a real
/// Codex version string. Returns `None` when the file is absent or unreadable.
pub(crate) fn codex_latest_known_version() -> Option<String> {
    let path = codex_dir().ok()?.join("version.json");
    let bytes = std::fs::read(path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("latest_version")?.as_str().map(String::from)
}

fn codex_config_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(codex_dir()?.join("config.toml"))
}

fn codex_provider_id_or_default(_p: &Provider) -> String {
    // Internal stable id, never user-configurable. Avoids Codex's
    // reserved built-in names (openai/amazon-bedrock/ollama/lmstudio)
    // so the merge in `merge_configured_model_providers` actually
    // takes our block (see model-provider-info/src/lib.rs:442-473).
    TERMORY_PROVIDER_ID.to_string()
}

fn activate_codex(p: &Provider, all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let provider_id = codex_provider_id_or_default(p);

    // Step 1: write auth.json.
    //
    // We MERGE — do NOT overwrite — so a previously run `codex login`
    // (OAuth) survives a round-trip through Custom Provider mode.
    // Concretely: we set `auth_mode = "apikey"` (Codex `resolved_mode()`
    // checks this first per login/src/auth/manager.rs:980-988, so it
    // takes precedence over any existing OAuth `tokens` field) and
    // write `OPENAI_API_KEY`, but leave `tokens / last_refresh /
    // agent_identity` untouched. When the user later deactivates back
    // to Official, deactivate_codex removes only auth_mode +
    // OPENAI_API_KEY; the preserved tokens then make resolved_mode()
    // fall back to ChatGPT mode → user stays logged in.
    //
    // This deliberately differs from the official `login_with_api_key`
    // (which nulls tokens) because Termory's "switch to API platform"
    // is a temporary swap, not a permanent OAuth abandonment.
    //
    // Saved with rollback so a failure on step 2 (config.toml) doesn't
    // strand auth.json in a half-written state.
    let auth_path = codex_auth_path()?;
    let prev_auth_bytes = if auth_path.exists() {
        Some(fs::read(&auth_path)?)
    } else {
        None
    };
    let mut auth_root = load_json_object(&auth_path)?;
    auth_root.insert("auth_mode".into(), JsonValue::String("apikey".into()));
    if !p.api_key.is_empty() {
        auth_root.insert(
            "OPENAI_API_KEY".into(),
            JsonValue::String(p.api_key.clone()),
        );
    } else {
        auth_root.remove("OPENAI_API_KEY");
    }
    write_json_object(&auth_path, &auth_root)?;

    // Step 2: write config.toml.
    //
    // Field choices verified against Codex source + cc-switch:
    //   - `requires_openai_auth = true` makes Codex load
    //     auth.json via AuthManager. Without this, TUI returns
    //     LoginStatus::NotAuthenticated (see Codex tui/src/lib.rs:1817).
    //   - `wire_api = "responses"` — "chat" is removed in current Codex
    //     (CHAT_WIRE_API_REMOVED_ERROR, model-provider-info/src/lib.rs:45).
    //   - We DO NOT set `env_key`. If `env_key` is set and the
    //     environment variable is missing, Codex errors out before
    //     falling back to auth.json (model-provider/src/auth.rs:92-103
    //     + model-provider-info/src/lib.rs:272-288).
    let config_result = (|| -> Result<(), Box<dyn Error>> {
        let config_path = codex_config_path()?;
        let mut doc = load_toml_document(&config_path)?;
        doc["model_provider"] = toml_value(provider_id.as_str());
        if !p.model.is_empty() {
            doc["model"] = toml_value(p.model.as_str());
        }
        if doc.get("model_providers").is_none() {
            doc["model_providers"] = toml_edit::table();
        }
        let providers_table = doc["model_providers"]
            .as_table_mut()
            .ok_or("model_providers must be a TOML table")?;
        // `[model_providers]` only holds `[model_providers.<id>]` sub-tables —
        // implicit so toml_edit never emits a bare empty `[model_providers]`
        // header above them (same fix as grok's `[model]`).
        providers_table.set_implicit(true);
        if !providers_table.contains_key(&provider_id) {
            providers_table[&provider_id] = toml_edit::table();
        }
        let block = providers_table[&provider_id]
            .as_table_mut()
            .ok_or("model_providers.<id> must be a table")?;
        if !p.name.is_empty() {
            block["name"] = toml_value(p.name.as_str());
        }
        if !p.base_url.is_empty() {
            block["base_url"] = toml_value(p.base_url.as_str());
        }
        block["wire_api"] = toml_value("responses");
        block["requires_openai_auth"] = toml_value(true);
        // Defensive: scrub any pre-existing env_key on this block to
        // avoid Codex preferring an empty env var over auth.json.
        block.remove("env_key");
        // Clear every provider's override keys, then apply this one's.
        strip_toml_overrides(&mut doc, &override_keys(all, CliApp::Codex));
        apply_toml_overrides(&mut doc, p, CliApp::Codex);
        write_text_file(&config_path, &doc.to_string())
    })();

    if let Err(err) = config_result {
        // Rollback auth.json to previous state.
        if let Some(bytes) = prev_auth_bytes {
            let _ = fs::write(&auth_path, bytes);
        } else {
            let _ = fs::remove_file(&auth_path);
        }
        return Err(err);
    }
    Ok(())
}

fn deactivate_codex(all: &[Provider]) -> Result<(), Box<dyn Error>> {
    // Clear ApiKey-mode fields from auth.json, but preserve any
    // ChatGPT OAuth credentials. We only touch the API key path —
    // if the user previously ran `codex login` and has tokens in
    // auth.json, those keep working after we deactivate.
    let auth_path = codex_auth_path()?;
    if auth_path.exists() {
        let mut auth_root = load_json_object(&auth_path)?;
        let was_apikey_mode = auth_root
            .get("auth_mode")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("apikey"))
            .unwrap_or(false);
        let has_tokens = matches!(auth_root.get("tokens"), Some(JsonValue::Object(_)));
        auth_root.remove("OPENAI_API_KEY");
        if was_apikey_mode {
            // Remove the explicit ApiKey marker. If a ChatGPT token
            // is also present (rare but possible), `resolved_mode()`
            // will fall back to ChatGPT mode via the presence of
            // `tokens`. Otherwise Codex falls through to
            // "NotAuthenticated" and the user runs `codex login`.
            auth_root.remove("auth_mode");
        }
        // If the file is now effectively empty, delete it so Codex
        // starts cleanly. "Effectively empty" = only null fields left.
        let effectively_empty =
            !has_tokens && auth_root.iter().all(|(_, v)| matches!(v, JsonValue::Null));
        if effectively_empty {
            let _ = fs::remove_file(&auth_path);
        } else {
            write_json_object(&auth_path, &auth_root)?;
        }
    }

    // Strip model_provider + matching provider block from config.toml.
    // Only remove provider blocks Termory could have written
    // (non-reserved id); never touch the user's openai/bedrock/ollama
    // blocks even if they happen to be the current selection.
    // Scorched-earth: also unconditionally drop the stable
    // `[model_providers.termory]` block so leftovers from a failed
    // delete (or a previous Termory version) get swept, not just the
    // currently-selected one.
    let config_path = codex_config_path()?;
    if !config_path.exists() {
        return Ok(());
    }
    let mut doc = load_toml_document(&config_path)?;
    let provider_id = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::to_string);
    if let Some(id) = provider_id.as_deref() {
        let is_built_in = CODEX_RESERVED_IDS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(id));
        if !is_built_in {
            doc.as_table_mut().remove("model_provider");
            doc.as_table_mut().remove("model");
            if let Some(providers) = doc
                .get_mut("model_providers")
                .and_then(|i| i.as_table_mut())
            {
                providers.remove(id);
                if providers.is_empty() {
                    doc.as_table_mut().remove("model_providers");
                }
            }
        }
    }
    // Always purge the stable Termory provider block, even if
    // `model_provider` was already pointing elsewhere — that leftover
    // is exactly the "failed delete" footprint the Restore-default
    // button is meant to clean.
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(|i| i.as_table_mut())
    {
        providers.remove(TERMORY_PROVIDER_ID);
        if providers.is_empty() {
            doc.as_table_mut().remove("model_providers");
        }
    }
    strip_toml_overrides(&mut doc, &override_keys(all, CliApp::Codex));
    write_text_file(&config_path, &doc.to_string())
}

fn read_active_codex(providers: &[Provider]) -> Result<ActiveState, Box<dyn Error>> {
    let config_path = codex_config_path()?;
    let live_path = config_path.display().to_string();
    if !config_path.exists() {
        return Ok(ActiveState {
            app: CliApp::Codex,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }
    let text = fs::read_to_string(&config_path)?;
    let doc = text.parse::<DocumentMut>()?;
    let active_id = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::to_string);
    let model = doc
        .get("model")
        .and_then(|item| item.as_str())
        .map(str::to_string);

    // No model_provider, or it points to a built-in id → Official.
    let Some(active_id) = active_id else {
        return Ok(ActiveState {
            app: CliApp::Codex,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    };
    if CODEX_RESERVED_IDS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&active_id))
    {
        return Ok(ActiveState {
            app: CliApp::Codex,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }

    let base_url = doc
        .get("model_providers")
        .and_then(|i| i.as_table())
        .and_then(|t| t.get(active_id.as_str()))
        .and_then(|i| i.as_table())
        .and_then(|t| t.get("base_url"))
        .and_then(Item::as_str)
        .map(str::to_string);
    let api_key = read_codex_auth_key()?;
    let snapshot = LiveSnapshot {
        base_url: base_url.clone(),
        api_key_masked: api_key.as_deref().map(mask_secret),
        model: model.clone(),
    };

    let matched = providers.iter().find(|p| {
        p.app == CliApp::Codex
            && p.kind == ProviderKind::Custom
            && string_match(&p.base_url, base_url.as_deref())
            && string_match(&p.api_key, api_key.as_deref())
    });

    Ok(ActiveState {
        app: CliApp::Codex,
        kind: if matched.is_some() {
            ActiveKind::Custom
        } else {
            ActiveKind::Unmanaged
        },
        matched_provider_id: matched.map(|p| p.id.clone()),
        live_snapshot: Some(snapshot),
        live_path,
        configured_provider_ids: Vec::new(),
    })
}

fn read_codex_auth_key() -> Result<Option<String>, Box<dyn Error>> {
    let path = codex_auth_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let root = load_json_object(&path)?;
    Ok(root
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

// ===================================================================
// Grok Build (xAI)
// ===================================================================
//
// File: ~/.grok/config.toml (merge, atomic write). Grok's `[model.*]` is a
// FLAT model list — each `[model.<key>]` is one entry in the TUI picker
// (docs.x.ai/build/settings#example-configtoml). MULTI-SLOT, like OpenCode:
// a Termory provider carries a `models` list (each → one entry) + an
// optional default (`p.model`). Each model is written as:
//
//   [model."<provider-id>-<model-id>"]   ← key = provider id + model id
//   model = "<model-id>"                 ← id sent to the API
//   base_url = "https://…/v1"            ← provider endpoint
//   name = "<model name | id>"           ← shown in the model picker
//   description = "<provider>"           ← Termory writes the provider NAME
//   api_key = "…"                        ← direct key (docs prefer env_key,
//                                          but an env var can't be
//                                          guaranteed — same call as Codex)
//   [models] default = "<provider-id>-<model-id>"   ← OPTIONAL
//
// The provider-id prefix keeps entries unique so several providers' models
// coexist in one flat list. Activate rewrites ONLY the activating
// provider's entries; "Set Official" clears just `models.default`; deleting
// a provider removes only its entries. ~/.grok/auth.json is NEVER touched.

/// The per-entry fields Termory writes — the dynamic managed-key rule
/// (`override_key_is_managed`) recognizes `model.<key>.<field>` for these.
const GROK_ENTRY_FIELDS: [&str; 6] = [
    "model",
    "base_url",
    "name",
    "description",
    "api_key",
    "api_backend",
];

fn grok_config_path() -> Result<PathBuf, Box<dyn Error>> {
    grok_home_dir()
        .map(|d| d.join("config.toml"))
        .ok_or_else(|| "home directory not available".into())
}

/// Grok Build's data dir — honors `$GROK_HOME` (documented relocation
/// knob, docs.x.ai/build/settings), else `~/.grok`. Mirrors how the
/// Claude scanner honors CLAUDE_CONFIG_DIR.
pub(crate) fn grok_home_dir() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("GROK_HOME") {
        if !custom.is_empty() {
            return Some(PathBuf::from(custom));
        }
    }
    crate::home_dir().map(|h| h.join(".grok"))
}

/// The flat model-list key for one of a provider's models:
/// `<provider-id>-<model-id>` (the provider-id prefix keeps a provider's
/// own models distinct; grok TUI-quotes keys that contain a `.`).
fn grok_entry_key(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}-{model_id}")
}

/// True when `key` is a `[model.<key>]` entry Termory wrote for provider
/// `pid` — entries are keyed `<pid>-<model-id>`, so ownership is the
/// `<pid>-` prefix. Provider ids are stable uuids, so a different provider's
/// id being a prefix of this one is not a practical collision (same
/// assumption OpenCode makes with its `<termory-id>/` model ref).
fn grok_key_owned_by(key: &str, pid: &str) -> bool {
    key.starts_with(&format!("{pid}-"))
}

/// Remove every `[model.<key>]` entry belonging to provider `pid`
/// (prefix-owned), returning the removed keys. Multi-slot: only THIS
/// provider's entries are touched — siblings coexist. Also catches stale
/// entries left by a previous (shorter) model list on re-activate.
fn remove_grok_provider_entries(doc: &mut DocumentMut, pid: &str) -> Vec<String> {
    let Some(entries) = doc.get_mut("model").and_then(|i| i.as_table_mut()) else {
        return Vec::new();
    };
    let ours: Vec<String> = entries
        .iter()
        .filter(|(k, _)| grok_key_owned_by(k, pid))
        .map(|(k, _)| k.to_string())
        .collect();
    for key in &ours {
        entries.remove(key);
    }
    if entries.is_empty() {
        doc.remove("model");
    }
    ours
}

/// The current `models.default` value, if any.
fn grok_default_key(doc: &DocumentMut) -> Option<String> {
    doc.get("models")
        .and_then(|i| i.get("default"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

/// Clear `models.default`, pruning an emptied `[models]` table.
fn clear_grok_default(doc: &mut DocumentMut) {
    if let Some(models) = doc.get_mut("models").and_then(|i| i.as_table_mut()) {
        models.remove("default");
        if models.is_empty() {
            doc.remove("models");
        }
    }
}

/// Enable a grok provider (add its models to grok's flat picker list).
/// MULTI-SLOT, like OpenCode: only THIS provider's `[model.<pid>-*]` entries
/// are (re)written — sibling providers' entries coexist. Does NOT set the
/// startup default; promoting a slot to default is a separate action
/// (`set_grok_default`), mirroring OpenCode's enable-vs-default split.
fn activate_grok(p: &Provider, _all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let base = p.base_url.trim();
    if base.is_empty() {
        return Err("Grok provider is missing a Base URL".into());
    }
    let pid = p.id.trim();
    if pid.is_empty() {
        return Err("Grok provider is missing an id".into());
    }
    let models: Vec<(&str, &str)> = p
        .models
        .iter()
        .map(|m| (m.id.trim(), m.name.trim()))
        .filter(|(id, _)| !id.is_empty())
        .collect();
    if models.is_empty() {
        return Err("Grok provider requires at least one model.".into());
    }
    reject_duplicate_model_ids(&p.models, "Grok")?;
    let default = p.model.trim();
    if !default.is_empty() && !models.iter().any(|(id, _)| *id == default) {
        return Err("Grok default model must be one of the provider's models.".into());
    }

    let path = grok_config_path()?;
    let mut doc = load_toml_document(&path)?;
    // Rebuild ONLY this provider's entries (drop its stale ones first);
    // sibling providers' entries are left in place.
    remove_grok_provider_entries(&mut doc, pid);

    if doc.get("model").is_none() {
        doc["model"] = toml_edit::table();
    }
    let entries = doc["model"]
        .as_table_mut()
        .ok_or("`model` must be a TOML table")?;
    // `[model]` only holds `[model.<key>]` sub-tables — implicit so toml_edit
    // never emits a bare empty `[model]` line above them.
    entries.set_implicit(true);
    for &(mid, mname) in &models {
        let key = grok_entry_key(pid, mid);
        entries.insert(&key, toml_edit::table());
        let entry = &mut entries[&key];
        entry["model"] = toml_value(mid);
        entry["base_url"] = toml_value(base);
        // `name` = picker display (model's name, else its id); `description`
        // = the PROVIDER platform name.
        entry["name"] = toml_value(if mname.is_empty() { mid } else { mname });
        if p.name.trim().is_empty() {
            if let Some(tbl) = entry.as_table_mut() {
                tbl.remove("description");
            }
        } else {
            entry["description"] = toml_value(p.name.trim());
        }
        entry["api_key"] = toml_value(p.api_key.as_str());
        // Wire API for this entry. When unset/blank the field is OMITTED so
        // grok applies its OWN default — `ApiBackend::default()` =
        // chat_completions (xai-grok-sampling-types/types.rs, applied via
        // `ModelInfo::fallback`) — and keeps following grok if that default
        // ever changes. Only an explicit editor choice is written.
        match p
            .api_backend
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(backend) => entry["api_backend"] = toml_value(backend),
            None => {
                if let Some(tbl) = entry.as_table_mut() {
                    tbl.remove("api_backend");
                }
            }
        }
    }

    // NOTE: no `models.default` here — enabling only registers the slot.
    // Promoting it to grok's startup default is `set_grok_default`.
    //
    // Grok has NO per-provider Advanced settings: its overrides would be
    // GLOBAL config.toml keys (not scoped to a provider), so a per-provider
    // box is misleading and multiple providers would clash on the same
    // top-level namespace. The editor hides Advanced settings for grok and
    // never sends `options`, so nothing to apply/strip here.
    atomic_write(&path, doc.to_string().as_bytes())
}

/// Promote an already-enabled grok provider to grok's startup default by
/// writing `models.default = "<pid>-<model>"`. The provider's entries must
/// already exist (enable first). When the provider has no explicit default
/// model, the first listed model is promoted (mirrors `set_opencode_default`).
///
/// This is ALSO where grok's Advanced settings (`options`) are materialized:
/// grok's overrides are GLOBAL config.toml keys (`ui.*`, `models.temperature`,
/// `[session]`, … — everything shares one file with the user's own settings),
/// so writing them on *enable* would let multiple enabled providers clash on
/// the same top-level keys. Writing them here — only for the ONE default
/// provider — sidesteps that: strip the union of all grok providers' option
/// keys, then apply this provider's. `deactivate_grok` strips them on switch.
pub fn set_grok_default(p: &Provider, all: &[Provider]) -> Result<(), Box<dyn Error>> {
    if p.app != CliApp::Grok || p.kind != ProviderKind::Custom {
        return Err("set_grok_default only applies to Grok Custom providers.".into());
    }
    let pid = p.id.trim();
    let default_model = if !p.model.trim().is_empty() {
        p.model.trim()
    } else {
        p.models
            .iter()
            .map(|m| m.id.trim())
            .find(|id| !id.is_empty())
            .ok_or("Provider needs at least one model to be set as default.")?
    };
    let key = grok_entry_key(pid, default_model);
    let path = grok_config_path()?;
    let mut doc = load_toml_document(&path)?;
    if doc.get("model").and_then(|i| i.get(&key)).is_none() {
        return Err("Provider isn't activated yet — activate it first.".into());
    }
    if doc.get("models").is_none() {
        doc["models"] = toml_edit::table();
    }
    doc["models"]["default"] = toml_value(key.as_str());
    // Global Advanced settings: only the default provider's are live. Strip the
    // union (drops the previous default's), then apply this one's.
    strip_toml_overrides(&mut doc, &override_keys(all, CliApp::Grok));
    apply_toml_overrides(&mut doc, p, CliApp::Grok);
    atomic_write(&path, doc.to_string().as_bytes())
}

/// "Set Official" — clear a `models.default` that points at one of our grok
/// providers' entries, and strip that (default) provider's global Advanced
/// settings. Enabled slots STAY (like OpenCode's "Set Official"), so they
/// remain selectable in grok's picker; a hand-written default is left alone.
fn deactivate_grok(all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let path = grok_config_path()?;
    if !path.exists() {
        return Ok(());
    }
    let mut doc = load_toml_document(&path)?;
    let default_is_ours = grok_default_key(&doc)
        .map(|d| {
            all.iter()
                .filter(|p| p.app == CliApp::Grok && p.kind == ProviderKind::Custom)
                .any(|p| grok_key_owned_by(&d, p.id.trim()))
        })
        .unwrap_or(false);
    if default_is_ours {
        clear_grok_default(&mut doc);
        // Our global Advanced settings are live ONLY while our provider is the
        // default, so strip them here too (union covers whichever was live).
        strip_toml_overrides(&mut doc, &override_keys(all, CliApp::Grok));
        atomic_write(&path, doc.to_string().as_bytes())?;
    }
    Ok(())
}

/// Surgical per-provider cleanup: remove just this grok provider's entries,
/// clear the startup default if it pointed here, and strip its own override
/// keys (Advanced settings, live only while it was the default). Sibling
/// slots survive.
fn delete_grok_provider_entry(p: &Provider) -> Result<(), Box<dyn Error>> {
    if p.app != CliApp::Grok || p.kind != ProviderKind::Custom {
        return Ok(());
    }
    let path = grok_config_path()?;
    if !path.exists() {
        return Ok(());
    }
    let pid = p.id.trim();
    let mut doc = load_toml_document(&path)?;
    let removed = remove_grok_provider_entries(&mut doc, pid);
    let mut changed = !removed.is_empty();
    let default_ours = grok_default_key(&doc)
        .map(|d| grok_key_owned_by(&d, pid))
        .unwrap_or(false);
    if default_ours {
        clear_grok_default(&mut doc);
        changed = true;
        // This provider was the default → its global Advanced settings are
        // live in config.toml; strip them. (If it wasn't the default, its
        // options were never written, so this is skipped.)
        let ovr = override_keys(std::slice::from_ref(p), CliApp::Grok);
        if !ovr.is_empty() {
            strip_toml_overrides(&mut doc, &ovr);
        }
    }
    if changed {
        atomic_write(&path, doc.to_string().as_bytes())?;
    }
    Ok(())
}

fn read_active_grok(providers: &[Provider]) -> Result<ActiveState, Box<dyn Error>> {
    let path = grok_config_path()?;
    let live_path = path.display().to_string();
    let official = |live_path: String, ids: Vec<String>| ActiveState {
        app: CliApp::Grok,
        kind: ActiveKind::Official,
        matched_provider_id: None,
        live_snapshot: None,
        live_path,
        configured_provider_ids: ids,
    };
    if !path.exists() {
        return Ok(official(live_path, Vec::new()));
    }
    let doc = fs::read_to_string(&path)?.parse::<DocumentMut>()?;

    // "Enabled" = the provider has ≥1 `[model.<pid>-*]` entry; "default" =
    // `models.default` points at one of its entries. Independent, like
    // OpenCode.
    let has_entry = |pid: &str| -> bool {
        doc.get("model")
            .and_then(|i| i.as_table())
            .map(|t| t.iter().any(|(k, _)| grok_key_owned_by(k, pid)))
            .unwrap_or(false)
    };
    let configured_provider_ids: Vec<String> = providers
        .iter()
        .filter(|p| p.app == CliApp::Grok && p.kind == ProviderKind::Custom)
        .filter(|p| has_entry(p.id.trim()))
        .map(|p| p.id.clone())
        .collect();

    let Some(default) = grok_default_key(&doc) else {
        return Ok(official(live_path, configured_provider_ids));
    };
    let Some(entry) = doc.get("model").and_then(|i| i.get(&default)) else {
        return Ok(official(live_path, configured_provider_ids));
    };
    // Which grok provider owns the default entry? By `<pid>-` prefix — this
    // is unambiguous (the entry key IS `<pid>-<model>`) and scoped to grok,
    // so a same-creds provider on another CLI is never matched.
    let matched = providers.iter().find(|p| {
        p.app == CliApp::Grok
            && p.kind == ProviderKind::Custom
            && grok_key_owned_by(&default, p.id.trim())
    });
    let Some(p) = matched else {
        // Default points somewhere we don't own → Official (enabled slots
        // stay exposed via configured_provider_ids).
        return Ok(official(live_path, configured_provider_ids));
    };
    let get = |k: &str| entry.get(k).and_then(|i| i.as_str()).unwrap_or("");
    let base_url = get("base_url").to_string();
    let api_key = get("api_key");
    let model = get("model");
    Ok(ActiveState {
        app: CliApp::Grok,
        kind: ActiveKind::Custom,
        matched_provider_id: Some(p.id.clone()),
        live_snapshot: Some(LiveSnapshot {
            base_url: (!base_url.is_empty()).then_some(base_url),
            api_key_masked: (!api_key.is_empty()).then(|| mask_secret(api_key)),
            model: (!model.is_empty()).then(|| model.to_string()),
        }),
        live_path,
        configured_provider_ids,
    })
}

// ===================================================================
// Gemini CLI
// ===================================================================
//
// File: ~/.gemini/.env  (dotenv; Gemini auto-loads on startup)
// Custom: writes GOOGLE_GEMINI_BASE_URL + GEMINI_API_KEY + GEMINI_MODEL
// Official: removes those three
// Reverse: parse .env; if any of them present → Custom-ish.
//
// OAuth credentials live in separate files (`~/.gemini/oauth_creds.json`
// and `~/.gemini/google_accounts.json`, see `storage.ts:22, 87`) which
// we never touch — switching to a Custom provider and back leaves
// `gemini auth` login intact automatically.

fn gemini_env_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".gemini").join(".env"))
}

fn activate_gemini(p: &Provider, all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let path = gemini_env_path()?;
    let mut map = parse_dotenv(&path)?;
    // Each field: write when non-empty, strip when empty. Empty-string
    // strip prevents stale values from a prior Custom provider from
    // sticking around after the user clears the field.
    for (key, value) in [
        ("GOOGLE_GEMINI_BASE_URL", &p.base_url),
        ("GEMINI_API_KEY", &p.api_key),
        // Gemini CLI reads GEMINI_MODEL with priority just below
        // `--model` (see `cli/src/config/config.ts:836-837`:
        // `argv.model || process.env['GEMINI_MODEL'] || settings.model?.name`).
        // Matches cc-switch's preset shape (provider.rs:653-658).
        ("GEMINI_MODEL", &p.model),
    ] {
        if value.is_empty() {
            map.remove(key);
        } else {
            map.insert(key.into(), value.clone());
        }
    }
    // Overrides: `.env` is flat, so each override key is the literal
    // env var name and the value is verbatim. Clear every provider's
    // override keys first, then apply this one's.
    for k in override_keys(all, CliApp::Gemini) {
        map.remove(&k);
    }
    for o in &p.options {
        let key = o.key.trim();
        if !key.is_empty() && !override_key_is_managed(CliApp::Gemini, key) {
            map.insert(key.to_string(), o.value.clone());
        }
    }
    write_dotenv(&path, &map)
}

fn deactivate_gemini(all: &[Provider]) -> Result<(), Box<dyn Error>> {
    let path = gemini_env_path()?;
    if !path.exists() {
        return Ok(());
    }
    let mut map = parse_dotenv(&path)?;
    map.remove("GOOGLE_GEMINI_BASE_URL");
    map.remove("GEMINI_API_KEY");
    map.remove("GEMINI_MODEL");
    for k in override_keys(all, CliApp::Gemini) {
        map.remove(&k);
    }
    write_dotenv(&path, &map)
}

fn read_active_gemini(providers: &[Provider]) -> Result<ActiveState, Box<dyn Error>> {
    let path = gemini_env_path()?;
    let live_path = path.display().to_string();
    if !path.exists() {
        return Ok(ActiveState {
            app: CliApp::Gemini,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }
    let map = parse_dotenv(&path)?;
    let base_url = map.get("GOOGLE_GEMINI_BASE_URL").cloned();
    let api_key = map.get("GEMINI_API_KEY").cloned();
    let model = map.get("GEMINI_MODEL").cloned();
    if base_url.is_none() && api_key.is_none() && model.is_none() {
        return Ok(ActiveState {
            app: CliApp::Gemini,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }
    let snapshot = LiveSnapshot {
        base_url: base_url.clone(),
        api_key_masked: api_key.as_deref().map(mask_secret),
        model,
    };
    let matched = providers.iter().find(|p| {
        p.app == CliApp::Gemini
            && p.kind == ProviderKind::Custom
            && string_match(&p.base_url, base_url.as_deref())
            && string_match(&p.api_key, api_key.as_deref())
    });
    Ok(ActiveState {
        app: CliApp::Gemini,
        kind: if matched.is_some() {
            ActiveKind::Custom
        } else {
            ActiveKind::Unmanaged
        },
        matched_provider_id: matched.map(|p| p.id.clone()),
        live_snapshot: Some(snapshot),
        live_path,
        configured_provider_ids: Vec::new(),
    })
}

// ===================================================================
// OpenCode
// ===================================================================
//
// cc-switch mode: Termory writes EVERYTHING into one file —
// `~/.config/opencode/opencode.json`. `auth.json` is never touched
// (that file stays reserved for `/connect`).
//
// Per Termory provider P with id <pid> (stored in `providers.json`):
//   * Slot in opencode.json:
//       provider.termory-<pid>.{
//         name, npm,
//         options.{baseURL, apiKey},
//         models: { <id>: {name: "<id>"}, ... }   // primary + extras
//       }
//   * "In use" pointer (top-level): model = "termory-<pid>/<primary>"
//
// Two independent states reverse-derived from opencode.json alone:
//   * Enabled — `provider.termory-<pid>` exists.
//   * In use — top-level `model` starts with `termory-<pid>/` AND
//              the slot's apiKey matches the stored provider's key.
//
// Activate writes the slot only. Set-as-default writes the top-level
// model (requires slot to exist). Delete removes the slot and clears
// top-level model if it pointed at this slot.

fn opencode_config_path() -> Result<PathBuf, Box<dyn Error>> {
    // xdg-basedir resolves the config dir as `$XDG_CONFIG_HOME` (when set)
    // else `~/.config` on every platform (verified at
    // .audit-sources/opencode/packages/core/src/global.ts:12 +
    // xdg-basedir source). `opencode_config_dir` honors the env override;
    // don't use dirs::config_dir() — it returns ~/Library/Application
    // Support on macOS, which is wrong.
    Ok(crate::sessions::opencode_config_dir(&home()?).join("opencode.json"))
}

/// Stable, per-Termory-provider id used as the key in OpenCode's
/// opencode.json `provider` map. Termory writes its providers under this
/// id so they don't collide with the user's `/connect` entries.
fn opencode_termory_id(p: &Provider) -> String {
    format!("termory-{}", p.id)
}

fn activate_opencode(p: &Provider, _all: &[Provider]) -> Result<(), Box<dyn Error>> {
    // OpenCode is multi-model (like grok): the `models` LIST is what
    // populates its picker, and the top-level `model` is the OPTIONAL
    // default (set separately via `set_opencode_default`). At least one
    // model must exist — either an entry in the list OR a bare default
    // (a gateway binding may carry only `model` with no list). When both
    // are present the default must be one of the listed models. API key
    // is optional (OpenCode supports env-var references and some gateways
    // don't require auth) — we just omit options.apiKey when blank.
    reject_duplicate_model_ids(&p.models, "OpenCode")?;
    let default_model = p.model.trim();
    if default_model.is_empty() && p.models.is_empty() {
        return Err("OpenCode provider requires at least one model.".into());
    }
    if !default_model.is_empty()
        && !p.models.is_empty()
        && !p.models.iter().any(|m| m.id.trim() == default_model)
    {
        return Err("OpenCode default model must be one of the provider's models.".into());
    }

    let termory_id = opencode_termory_id(p);
    let npm = p.opencode_npm();

    // Everything lives in opencode.json under provider.<termory-id>:
    //   npm, name, options.{baseURL, apiKey}, models.{<id>: {name}}
    // Matches cc-switch's pattern (opencode_config.rs:89-104,
    // provider.rs:695-742). auth.json is untouched — that file is
    // reserved for `/connect` flows.
    let path = opencode_config_path()?;
    let mut root = load_json_object(&path)?;
    let provider_map = ensure_json_object(&mut root, "provider")?;
    let block = ensure_object_at(provider_map, &termory_id);
    block.clear();
    if !p.name.is_empty() {
        block.insert("name".into(), JsonValue::String(p.name.clone()));
    }
    block.insert("npm".into(), JsonValue::String(npm.to_string()));

    let mut opts = serde_json::Map::new();
    if !p.base_url.trim().is_empty() {
        opts.insert("baseURL".into(), JsonValue::String(p.base_url.clone()));
    }
    if !p.api_key.trim().is_empty() {
        opts.insert("apiKey".into(), JsonValue::String(p.api_key.clone()));
    }
    // User "Advanced settings" options live INSIDE this provider's
    // `options` bag (OpenCode's open-ended AI-SDK options object). This
    // scopes them to the provider automatically: `block.clear()` above
    // rebuilds them on every enable (so removed keys vanish with no
    // separate strip), deleting the slot drops them, and sibling
    // providers' options are never touched. Keys are relative to
    // `options`; baseURL / apiKey are managed by the dedicated fields.
    for o in &p.options {
        let key = o.key.trim();
        if key.is_empty() || override_key_is_managed(CliApp::Opencode, key) {
            continue;
        }
        json_set_path(&mut opts, key, infer_json_value(&o.value));
    }
    if !opts.is_empty() {
        block.insert("options".into(), JsonValue::Object(opts));
    }

    // models map: built PURELY from the `models` list, in list order —
    // `p.model` is only the optional default pointer now (chosen FROM the
    // list, written separately by set_opencode_default), so it does NOT get
    // injected here. cc-switch writes each as `{name: "<label>"}` (blank name
    // falls back to id) so the picker has a label. Keyed by id, so a repeated
    // id just overrides (dedup already rejects real duplicates).
    let mut models = serde_json::Map::new();
    let put = |models: &mut serde_json::Map<String, JsonValue>, id: &str, name: &str| {
        if id.is_empty() {
            return;
        }
        let label = if name.is_empty() { id } else { name };
        let mut entry = serde_json::Map::new();
        entry.insert("name".into(), JsonValue::String(label.to_string()));
        models.insert(id.to_string(), JsonValue::Object(entry));
    };
    for m in &p.models {
        put(&mut models, m.id.trim(), m.name.trim());
    }
    // Fallback for a gateway binding that carries only a `model` with no list
    // (the model-only case) — write that single model so the slot isn't empty.
    if models.is_empty() {
        put(&mut models, p.model.trim(), "");
    }
    block.insert("models".into(), JsonValue::Object(models));

    // NOTE: Termory does NOT write the top-level `model` field here.
    // Enabling only registers this provider's slot; promoting it to the
    // OpenCode-startup default is a separate explicit action via
    // `set_opencode_default`. Multiple enabled providers coexist —
    // OpenCode picks at runtime via `/model`. Options need no top-level
    // strip/apply anymore: they're rebuilt inside the block above.
    write_json_object(&path, &root)
}

/// Promote a Termory provider to OpenCode's startup default by writing
/// `model = "<termory-id>/<primary>"` at the top of opencode.json.
/// Per `provider.ts:1775-1807` this short-circuits OpenCode's default
/// model resolution at startup. Requires the provider to be activated
/// already (slot must exist) — callers should activate first.
pub fn set_opencode_default(p: &Provider) -> Result<(), Box<dyn Error>> {
    if p.app != CliApp::Opencode || p.kind != ProviderKind::Custom {
        return Err("set_opencode_default only applies to OpenCode Custom providers.".into());
    }
    // The default model is optional in the editor now — when the user left
    // it blank, promote the first listed model so "Set as default" still
    // gives OpenCode a concrete startup pointer.
    let default_model = if !p.model.trim().is_empty() {
        p.model.trim()
    } else {
        p.models
            .iter()
            .map(|m| m.id.trim())
            .find(|id| !id.is_empty())
            .ok_or("Provider needs at least one model to be set as default.")?
    };
    let termory_id = opencode_termory_id(p);
    let path = opencode_config_path()?;
    let mut root = load_json_object(&path)?;
    let slot_exists = root
        .get("provider")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key(&termory_id))
        .unwrap_or(false);
    if !slot_exists {
        return Err("Provider isn't activated yet — activate it first.".into());
    }
    root.insert(
        "model".into(),
        JsonValue::String(format!("{termory_id}/{default_model}")),
    );
    write_json_object(&path, &root)
}

/// Remove a single Termory OpenCode provider's slot from opencode.json.
/// auth.json is not touched (Termory doesn't write there in cc-switch
/// mode). If the top-level `model` pointed at this provider, clear it
/// too — that ref is dead now.
fn delete_opencode_provider_entry(p: &Provider) -> Result<(), Box<dyn Error>> {
    if p.app != CliApp::Opencode || p.kind != ProviderKind::Custom {
        return Ok(());
    }
    let termory_id = opencode_termory_id(p);

    let config_path = opencode_config_path()?;
    if !config_path.exists() {
        return Ok(());
    }
    let mut root = load_json_object(&config_path)?;
    let mut changed = false;
    if let Some(JsonValue::Object(provider_map)) = root.get_mut("provider") {
        if provider_map.remove(&termory_id).is_some() {
            changed = true;
        }
        if provider_map.is_empty() {
            root.remove("provider");
        }
    }
    // Drop top-level `model` only when it refers to this provider —
    // user's choice of another provider as default stays untouched.
    let model_ref_prefix = format!("{termory_id}/");
    let drop_top_model = root
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.starts_with(&model_ref_prefix))
        .unwrap_or(false);
    if drop_top_model {
        root.remove("model");
        changed = true;
    }
    if changed {
        if root.is_empty() {
            let _ = fs::remove_file(&config_path);
        } else {
            write_json_object(&config_path, &root)?;
        }
    }
    Ok(())
}

fn deactivate_opencode(providers: &[Provider]) -> Result<(), Box<dyn Error>> {
    // For OpenCode, "Set Official as default" means *no Termory
    // provider is the startup default* — but the user's Enabled
    // Termory slots stay in opencode.json so they remain selectable
    // via OpenCode's `/model` command. We only clear the top-level
    // `model` field, and only when it points at one of the user's
    // Termory providers (don't touch a hand-written choice).
    let config_path = opencode_config_path()?;
    if !config_path.exists() {
        return Ok(());
    }
    let mut root = load_json_object(&config_path)?;

    // No option-stripping here: options live inside each provider's block
    // (`provider.<id>.options`), and enabled slots stay on "Set Official"
    // — so their options stay too. We only clear the startup-default.
    let user_termory_ids: std::collections::HashSet<String> = providers
        .iter()
        .filter(|p| p.app == CliApp::Opencode && p.kind == ProviderKind::Custom)
        .map(opencode_termory_id)
        .collect();
    let active_termory_id = root
        .get("model")
        .and_then(|v| v.as_str())
        .and_then(|s| s.split_once('/').map(|(pid, _)| pid.to_string()));

    let mut model_removed = false;
    if let Some(id) = active_termory_id {
        if user_termory_ids.contains(&id) {
            root.remove("model");
            model_removed = true;
        }
    }

    // Only touch the file when the default actually pointed at us.
    if model_removed {
        if root.is_empty() {
            let _ = fs::remove_file(&config_path);
        } else {
            write_json_object(&config_path, &root)?;
        }
    }
    Ok(())
}

fn read_active_opencode(providers: &[Provider]) -> Result<ActiveState, Box<dyn Error>> {
    let config_path = opencode_config_path()?;
    let live_path = config_path.display().to_string();

    if !config_path.exists() {
        return Ok(ActiveState {
            app: CliApp::Opencode,
            kind: ActiveKind::Official,
            matched_provider_id: None,
            live_snapshot: None,
            live_path,
            configured_provider_ids: Vec::new(),
        });
    }
    let config_root = load_json_object(&config_path)?;

    // Build the list of Termory provider ids whose slots exist in
    // opencode.json. "Activated" = slot exists; "default" = top-level
    // `model` points at it. They're independent for OpenCode.
    let provider_map = config_root.get("provider").and_then(|v| v.as_object());
    let configured_provider_ids: Vec<String> = providers
        .iter()
        .filter(|p| p.app == CliApp::Opencode && p.kind == ProviderKind::Custom)
        .filter(|p| {
            provider_map
                .map(|m| m.contains_key(&opencode_termory_id(p)))
                .unwrap_or(false)
        })
        .map(|p| p.id.clone())
        .collect();

    // The top-level `model` field decides which provider is the
    // OpenCode-startup default. Parse it as `<providerId>/<modelId>`
    // and match the providerId against our Termory providers.
    let top_model_ref = config_root
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let active_termory_id = top_model_ref
        .as_deref()
        .and_then(|s| s.split_once('/').map(|(pid, _)| pid.to_string()));

    if let Some(active_id) = active_termory_id {
        for p in providers {
            if p.app != CliApp::Opencode || p.kind != ProviderKind::Custom {
                continue;
            }
            if opencode_termory_id(p) != active_id {
                continue;
            }
            // Sanity check the api key in the live block matches what
            // Termory stored — guards against a stale top-level model
            // pointing at a slot the user edited. Treat missing
            // options.apiKey and an empty stored key as equivalent
            // (both mean "no key configured here").
            let block = config_root
                .get("provider")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get(&active_id))
                .and_then(|v| v.as_object());
            let live_key = block
                .and_then(|b| b.get("options"))
                .and_then(|v| v.as_object())
                .and_then(|o| o.get("apiKey"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if live_key != p.api_key.trim() {
                continue;
            }
            let live_base = block
                .and_then(|b| b.get("options"))
                .and_then(|v| v.as_object())
                .and_then(|o| o.get("baseURL"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            return Ok(ActiveState {
                app: CliApp::Opencode,
                kind: ActiveKind::Custom,
                matched_provider_id: Some(p.id.clone()),
                live_snapshot: Some(LiveSnapshot {
                    base_url: live_base,
                    api_key_masked: if live_key.is_empty() {
                        None
                    } else {
                        Some(mask_secret(live_key))
                    },
                    model: top_model_ref,
                }),
                live_path,
                configured_provider_ids,
            });
        }
    }

    // Top-level `model` either missing or pointing somewhere we don't
    // own → Official, even if some Termory providers are still
    // activated (their slots stay in opencode.json, exposed via
    // `configured_provider_ids` for the UI).
    Ok(ActiveState {
        app: CliApp::Opencode,
        kind: ActiveKind::Official,
        matched_provider_id: None,
        live_snapshot: None,
        live_path,
        configured_provider_ids,
    })
}

// ===================================================================
// Test API
// ===================================================================

/// Strip subdomains down to the brand domain — `api.openai.com` →
/// `openai.com`, `chat.deepseek.com` → `deepseek.com`. Used as a
/// favicon-fetch heuristic because API subdomains usually 404 on
/// `/favicon.ico` while the brand root almost always serves one.
///
/// Heuristic only: takes the last two labels. Fails for `.co.uk` /
/// `.com.cn` style multi-label TLDs (would yield `example.co.uk` →
/// `co.uk`), but none of the upstream AI providers we support use
/// those. IP literals are returned as `None` so we skip the strip and
/// fall back to the literal host. Proper handling would need the
/// Public Suffix List — overkill for the favicon nicety here.
fn registrable_domain(host: &str) -> Option<String> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let n = parts.len();
    Some(format!("{}.{}", parts[n - 2], parts[n - 1]))
}

/// Fetch the favicon for `url` (anything that parses as a URL — we
/// only care about its host) and return a `data:image/...;base64,...`
/// URL the renderer can stick straight into an `<img src>`. Returns
/// `None` on parse failure, network error, non-2xx, non-image
/// Content-Type, or empty body.
///
/// Tries the brand-root domain first (`api.openai.com` →
/// `openai.com/favicon.ico`), falls back to the literal host on miss.
/// Per-attempt 4-second budget so a slow upstream caps the worst-case
/// editor-save delay at ~8 seconds even when both probes have to
/// time out.
///
/// No Google s2 fallback (that's the privacy leak we just fixed), no
/// HTML `<link rel=icon>` scrape (too much work for a UI nicety). If
/// neither candidate yields, the UI falls back to the letter avatar.
pub async fn fetch_favicon(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .ok()?;
    let mut candidates: Vec<String> = Vec::new();
    if let Some(root) = registrable_domain(host) {
        candidates.push(root);
    }
    if !candidates.iter().any(|h| h == host) {
        candidates.push(host.to_string());
    }
    for h in &candidates {
        let favicon_url = format!("https://{h}/favicon.ico");
        if let Some(data_url) = try_fetch_favicon(&client, &favicon_url).await {
            return Some(data_url);
        }
    }
    None
}

/// Single-attempt favicon fetch + base64 wrap. Caller picks which
/// host to probe; this helper enforces the MIME / size / status
/// validation.
async fn try_fetch_favicon(client: &reqwest::Client, url: &str) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    // Most CDNs serve image/x-icon, image/vnd.microsoft.icon, or
    // image/png/svg+xml. Anything that isn't an `image/...` MIME is
    // almost certainly an HTML 404 page or a redirect we shouldn't
    // base64-embed.
    let mime = content_type.split(';').next().map(str::trim).unwrap_or("");
    if !mime.starts_with("image/") {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    // Cap at ~256 KB raw — favicons are usually 2-30 KB. Anything past
    // this is either misconfigured or maliciously large, and bloats
    // providers.json with a base64 payload we don't want in there.
    if bytes.len() > 256 * 1024 {
        return None;
    }
    let encoded = STANDARD.encode(&bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

/// Lightweight connectivity check — calls `GET {base_url}/models` (or
/// the Gemini variant) with the provider's API key. Counts any 2xx
/// as success.
pub async fn test_provider(p: &Provider) -> TestResult {
    let start = std::time::Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            return TestResult {
                ok: false,
                status: None,
                latency_ms: start.elapsed().as_millis(),
                message: format!("HTTP client init failed: {err}"),
            };
        }
    };

    let (url, gemini_query) = match p.app {
        CliApp::Gemini => {
            let base = p
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1beta")
                .trim_end_matches("/v1")
                .to_string();
            (
                format!("{}/v1beta/models", base),
                Some(("key", p.api_key.clone())),
            )
        }
        _ => {
            let base = p.base_url.trim_end_matches('/');
            // Most providers expose /v1/models. Allow base URLs that
            // already include /v1 (e.g. https://api.openai.com/v1).
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            (url, None)
        }
    };

    let mut req = client.get(&url);
    if matches!(p.app, CliApp::Gemini) {
        if let Some((k, v)) = gemini_query {
            req = req.query(&[(k, v)]);
        }
    } else if !p.api_key.is_empty() {
        req = req.bearer_auth(&p.api_key);
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(err) => {
            return TestResult {
                ok: false,
                status: None,
                latency_ms: start.elapsed().as_millis(),
                message: format!("Request failed: {err}"),
            };
        }
    };
    let status = response.status();
    TestResult {
        ok: status.is_success(),
        status: Some(status.as_u16()),
        latency_ms: start.elapsed().as_millis(),
        message: if status.is_success() {
            "OK".into()
        } else {
            status.canonical_reason().unwrap_or("HTTP error").into()
        },
    }
}

/// Hit the provider's models endpoint and return the model id list.
/// Same routing as `test_provider`:
///   * Gemini  → `GET {base}/v1beta/models?key={apiKey}`,response
///               `{ models: [{ name: "models/gemini-2.5-pro", ... }] }`
///               → strip the `models/` prefix to get the bare id.
///   * others  → `GET {base}/v1/models` with Bearer auth, response
///               `{ data: [{ id: "gpt-5", ... }, ...] }`.
/// Gracefully returns `ok=false` + diagnostic message on any failure
/// (network, HTTP error, JSON shape mismatch) — the frontend treats
/// that as "fetch failed, user can still type manually".
pub async fn fetch_models(p: &Provider) -> ModelListResult {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            return ModelListResult {
                ok: false,
                models: Vec::new(),
                status: None,
                message: format!("HTTP client init failed: {err}"),
            };
        }
    };

    let (url, gemini_query) = match p.app {
        CliApp::Gemini => {
            let base = p
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1beta")
                .trim_end_matches("/v1")
                .to_string();
            (
                format!("{}/v1beta/models", base),
                Some(("key", p.api_key.clone())),
            )
        }
        _ => {
            let base = p.base_url.trim_end_matches('/');
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            (url, None)
        }
    };

    let mut req = client.get(&url);
    if matches!(p.app, CliApp::Gemini) {
        if let Some((k, v)) = gemini_query {
            req = req.query(&[(k, v)]);
        }
    } else if !p.api_key.is_empty() {
        req = req.bearer_auth(&p.api_key);
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(err) => {
            return ModelListResult {
                ok: false,
                models: Vec::new(),
                status: None,
                message: format!("Request failed: {err}"),
            };
        }
    };
    let status = response.status();
    let status_u16 = status.as_u16();
    if !status.is_success() {
        return ModelListResult {
            ok: false,
            models: Vec::new(),
            status: Some(status_u16),
            message: status
                .canonical_reason()
                .unwrap_or("HTTP error")
                .to_string(),
        };
    }
    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(err) => {
            return ModelListResult {
                ok: false,
                models: Vec::new(),
                status: Some(status_u16),
                message: format!("Response is not JSON: {err}"),
            };
        }
    };

    let mut models = match p.app {
        CliApp::Gemini => body
            .get("models")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                    .map(|name| name.trim_start_matches("models/").to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        _ => body
            .get("data")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|n| n.as_str()))
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    };
    models.sort();
    models.dedup();

    if models.is_empty() {
        return ModelListResult {
            ok: false,
            models,
            status: Some(status_u16),
            message: "Endpoint returned no models".into(),
        };
    }
    ModelListResult {
        ok: true,
        models,
        status: Some(status_u16),
        message: "OK".into(),
    }
}

// ===================================================================
// Gateway API-mode detection
// ===================================================================

/// Extract model ids from a list-endpoint body. OpenAI/Anthropic shape
/// is `{ data: [{ id }] }`; Gemini is `{ models: [{ name }] }` with a
/// `models/` prefix to strip.
fn parse_model_ids(body: &serde_json::Value, gemini: bool) -> Vec<String> {
    let mut ids: Vec<String> = if gemini {
        body.get("models")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                    .map(|name| name.trim_start_matches("models/").to_string())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        body.get("data")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|n| n.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    ids.sort();
    ids.dedup();
    ids
}

/// Does a POST route EXIST? We send a deliberately empty body and read
/// only the status — present unless it answers 404/405 (a 400/401/422/503
/// means the route is there but rejected/couldn't serve our empty request).
/// Never spends tokens. `req` must already carry the right auth (Bearer /
/// x-api-key). Used for the OpenAI/Anthropic gates, whose own list endpoint
/// (`/v1/models`) is GENERIC (every openai-compatible gateway answers it,
/// so it can't gate a capability).
async fn route_exists(req: reqwest::RequestBuilder) -> bool {
    match req.json(&serde_json::json!({})).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            code != 404 && code != 405
        }
        Err(_) => false,
    }
}

/// GET a `/models`-style list. Returns `(got_data, models)`. Used both for
/// the Gemini GATE (its `/v1beta/models` is Gemini-SPECIFIC — a non-Gemini
/// gateway 404s it — so returning data DOES prove support) and the plain
/// catalog (where the bool is ignored).
async fn fetch_models_list(req: reqwest::RequestBuilder, gemini: bool) -> (bool, Vec<String>) {
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let models = resp
                .json::<serde_json::Value>()
                .await
                .map(|b| parse_model_ids(&b, gemini))
                .unwrap_or_default();
            (!models.is_empty(), models)
        }
        _ => (false, Vec::new()),
    }
}

fn with_bearer(req: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        req
    } else {
        req.bearer_auth(api_key)
    }
}

fn with_anthropic(req: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        req
    } else {
        req.header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    }
}

/// Detect which API modes a gateway's `{base_url, api_key}` supports.
/// OpenAI/Anthropic are probed at their real POST endpoints (their
/// `/v1/models` list is generic). Gemini is gated on its Gemini-SPECIFIC
/// `GET /v1beta/models` returning data (a non-Gemini gateway 404s that
/// path), and that same response also feeds the catalog. `models` is the
/// union of the two list endpoints, for autocomplete. All concurrent.
pub async fn detect_gateway_apis(base_url: &str, api_key: &str) -> GatewayCapabilities {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return GatewayCapabilities::default(),
    };

    // The gateway base is the bare ROOT (no API-version path); every
    // per-mode URL is derived from it. Strip any version suffix the user
    // may have pasted (`/v1` or `/v1beta`) so we don't double it.
    let root = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1beta")
        .trim_end_matches("/v1");
    let chat_url = format!("{root}/v1/chat/completions");
    let responses_url = format!("{root}/v1/responses");
    let messages_url = format!("{root}/v1/messages");
    let oai_models_url = format!("{root}/v1/models");
    let gemini_models_url = format!("{root}/v1beta/models");

    let (openai_compatible, openai, anthropic, gemini_list, oai_catalog) = tokio::join!(
        // OpenAI / Anthropic gates — POST the real API endpoint.
        route_exists(with_bearer(client.post(&chat_url), api_key)),
        route_exists(with_bearer(client.post(&responses_url), api_key)),
        route_exists(with_anthropic(client.post(&messages_url), api_key)),
        // Gemini gate + its catalog in one GET (Gemini-specific path).
        fetch_models_list(
            client.get(&gemini_models_url).query(&[("key", api_key)]),
            true
        ),
        // OpenAI catalog (data only — generic list, never a gate).
        fetch_models_list(with_bearer(client.get(&oai_models_url), api_key), false),
    );

    let (gemini, gemini_catalog) = gemini_list;
    let mut models = oai_catalog.1;
    models.extend(gemini_catalog);
    models.sort();
    models.dedup();

    GatewayCapabilities {
        openai_compatible,
        openai,
        anthropic,
        gemini,
        models,
    }
}

// ===================================================================
// Helpers
// ===================================================================

fn home() -> Result<PathBuf, Box<dyn Error>> {
    crate::home_dir().ok_or_else(|| "home directory not available".into())
}

fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    ensure_parent_dir(path)?;
    let mut tmp_name = path.file_name().ok_or("invalid path")?.to_owned();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    tmp_name.push(format!(".tmp.{nanos}"));
    let tmp_path = path.with_file_name(tmp_name);
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn write_text_file(path: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    atomic_write(path, contents.as_bytes())
}

pub(crate) fn load_json_object(path: &Path) -> Result<Map<String, JsonValue>, Box<dyn Error>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let parsed: JsonValue = serde_json::from_str(&text)?;
    match parsed {
        JsonValue::Object(map) => Ok(map),
        _ => Err(format!("{}: root must be a JSON object", path.display()).into()),
    }
}

pub(crate) fn write_json_object(
    path: &Path,
    root: &Map<String, JsonValue>,
) -> Result<(), Box<dyn Error>> {
    let serialized = serde_json::to_string_pretty(&JsonValue::Object(root.clone()))?;
    atomic_write(path, serialized.as_bytes())
}

fn ensure_json_object<'a>(
    root: &'a mut Map<String, JsonValue>,
    key: &str,
) -> Result<&'a mut Map<String, JsonValue>, Box<dyn Error>> {
    if !root.contains_key(key) {
        root.insert(key.into(), JsonValue::Object(Map::new()));
    }
    match root.get_mut(key) {
        Some(JsonValue::Object(map)) => Ok(map),
        _ => Err(format!("`{key}` is not a JSON object").into()),
    }
}

fn ensure_object_at<'a>(
    parent: &'a mut Map<String, JsonValue>,
    key: &str,
) -> &'a mut Map<String, JsonValue> {
    if !parent.contains_key(key) || !matches!(parent.get(key), Some(JsonValue::Object(_))) {
        parent.insert(key.into(), JsonValue::Object(Map::new()));
    }
    match parent.get_mut(key) {
        Some(JsonValue::Object(map)) => map,
        _ => unreachable!("just inserted an object"),
    }
}

// ===================================================================
// User-defined config overrides (Provider.options / "options")
//
// A provider can carry extra `{ key, value }` pairs that Termory merges
// into the CLI's live config on activation and strips on switch /
// deactivate. `key` is a dot-path; `value` is type-inferred for
// JSON/TOML targets, kept verbatim for Gemini's `.env`. Clean removal
// uses `override_keys` (the UNION of every provider-of-this-CLI's keys)
// so switching A → B never leaves A's keys behind.
// ===================================================================

/// Split a dot-path key into non-empty segments.
fn dot_path(key: &str) -> Vec<&str> {
    key.split('.')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Keys that Termory's dedicated fields (`baseUrl` / `apiKey` / `model`
/// / per-app routing) own. Overrides must NEVER touch these — they're
/// an escape hatch for *additional* config, not a way to clobber the
/// core credential/endpoint/model the user set in their own fields.
/// Checked at both apply and strip time so a managed key in `overrides`
/// is silently ignored, and the dedicated field always wins.
pub(crate) fn override_key_is_managed(app: CliApp, key: &str) -> bool {
    let k = key.trim();
    match app {
        // The DEFAULT_{HAIKU,SONNET,OPUS}_MODEL routing keys are
        // intentionally NOT managed — users set per-size routing through
        // overrides (with an optional `[1m]` suffix for 1M context).
        CliApp::Claude => matches!(
            k,
            "env.ANTHROPIC_BASE_URL"
                | "env.ANTHROPIC_AUTH_TOKEN"
                | "env.ANTHROPIC_API_KEY"
                | "env.ANTHROPIC_MODEL"
        ),
        // Codex: top-level model_provider/model + the whole
        // model_providers.* table (Termory's [model_providers.termory]).
        CliApp::Codex => k == "model_provider" || k == "model" || k.starts_with("model_providers."),
        CliApp::Gemini => {
            matches!(
                k,
                "GOOGLE_GEMINI_BASE_URL" | "GEMINI_API_KEY" | "GEMINI_MODEL"
            )
        }
        // OpenCode options are written INSIDE the provider's own block
        // under `provider.<id>.options`, so keys are relative to that
        // `options` bag. The two keys Termory fills from dedicated fields
        // (baseURL / apiKey) are managed and must not be clobbered.
        CliApp::Opencode => k == "baseURL" || k == "apiKey",
        // Claude Desktop: dedicated fields (baseUrl / apiKey / models)
        // own these profile keys; Advanced-settings options merge into the
        // SAME profile JSON for any OTHER `inference*` key, so these are
        // managed and must not be clobbered. (`claude_desktop::apply`
        // reuses this check + `json_set_path` to merge the rest.)
        CliApp::ClaudeDesktop => matches!(
            k,
            "inferenceProvider"
                | "inferenceGatewayBaseUrl"
                | "inferenceGatewayApiKey"
                | "inferenceGatewayAuthScheme"
                | "inferenceModels"
                | "disableDeploymentModeChooser"
                | "coworkEgressAllowedHosts"
        ),
        CliApp::Grok => {
            // Owned by the dedicated fields: the default pointer and the
            // five entry fields Termory writes (entry key = model id).
            // Other per-entry keys (api_backend, context_window,
            // extra_headers, … — docs.x.ai/build/settings/reference)
            // pass through as Advanced settings.
            k == "models.default"
                || (k.starts_with("model.")
                    && GROK_ENTRY_FIELDS
                        .iter()
                        .any(|f| k.ends_with(&format!(".{f}"))))
        }
    }
}

/// Union of every override key declared by any provider in the list,
/// excluding managed keys — the set Termory must clear before
/// (re)writing the active provider's overrides.
fn override_keys(providers: &[Provider], app: CliApp) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for p in providers {
        for o in &p.options {
            let k = o.key.trim();
            if k.is_empty() || override_key_is_managed(app, k) {
                continue;
            }
            if !keys.iter().any(|e| e == k) {
                keys.push(k.to_string());
            }
        }
    }
    keys
}

/// Infer a JSON scalar from a user-typed string: bool / integer /
/// finite float / else string. No arrays/objects in v1.
pub(crate) fn infer_json_value(raw: &str) -> JsonValue {
    let t = raw.trim();
    match t {
        "true" => return JsonValue::Bool(true),
        "false" => return JsonValue::Bool(false),
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        return JsonValue::from(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() {
            return JsonValue::from(f);
        }
    }
    JsonValue::String(raw.to_string())
}

/// Set a dot-path in a JSON object, creating intermediate objects.
pub(crate) fn json_set_path(root: &mut Map<String, JsonValue>, key: &str, value: JsonValue) {
    let segs = dot_path(key);
    let Some((last, parents)) = segs.split_last() else {
        return;
    };
    let mut cur = root;
    for seg in parents {
        let entry = cur
            .entry(seg.to_string())
            .or_insert_with(|| JsonValue::Object(Map::new()));
        if !entry.is_object() {
            *entry = JsonValue::Object(Map::new());
        }
        cur = entry.as_object_mut().expect("ensured object");
    }
    cur.insert(last.to_string(), value);
}

/// Remove a dot-path from a JSON object, pruning now-empty parents.
fn json_remove_path(root: &mut Map<String, JsonValue>, key: &str) {
    let segs = dot_path(key);
    json_remove_path_inner(root, &segs);
}
fn json_remove_path_inner(map: &mut Map<String, JsonValue>, segs: &[&str]) {
    match segs {
        [] => {}
        [last] => {
            map.remove(*last);
        }
        [head, rest @ ..] => {
            if let Some(child) = map.get_mut(*head).and_then(|v| v.as_object_mut()) {
                json_remove_path_inner(child, rest);
                if child.is_empty() {
                    map.remove(*head);
                }
            }
        }
    }
}

/// Claude variant: `settings.json`'s `env` object is
/// `Record<string, string>`, so override values under `env.*` are kept
/// as strings (never type-inferred to bool/number); everything else is
/// inferred like normal JSON. Managed keys (base url / token / model /
/// routing) are skipped — the dedicated fields own them.
fn apply_claude_overrides(root: &mut Map<String, JsonValue>, p: &Provider) {
    for o in &p.options {
        let key = o.key.trim();
        if key.is_empty() || override_key_is_managed(CliApp::Claude, key) {
            continue;
        }
        let value = if dot_path(key).first() == Some(&"env") {
            JsonValue::String(o.value.clone())
        } else {
            infer_json_value(&o.value)
        };
        json_set_path(root, key, value);
    }
}

/// Strip the given override keys from a JSON config root.
fn strip_json_overrides(root: &mut Map<String, JsonValue>, keys: &[String]) {
    for k in keys {
        json_remove_path(root, k);
    }
}

/// Infer a toml_edit Item (scalar) from a user-typed string.
fn infer_toml_item(raw: &str) -> Item {
    let t = raw.trim();
    if t == "true" {
        return toml_value(true);
    }
    if t == "false" {
        return toml_value(false);
    }
    if let Ok(i) = t.parse::<i64>() {
        return toml_value(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() {
            return toml_value(f);
        }
    }
    toml_value(raw)
}

/// Apply a provider's overrides into a TOML document (Codex config.toml /
/// Grok config.toml). `app` selects the managed-key set so the correct
/// dedicated-field keys are skipped (Codex's `model_provider`/… vs Grok's
/// `models.default`/`model.*`).
fn apply_toml_overrides(doc: &mut DocumentMut, p: &Provider, app: CliApp) {
    for o in &p.options {
        if override_key_is_managed(app, o.key.trim()) {
            continue;
        }
        let segs = dot_path(&o.key);
        let Some((last, parents)) = segs.split_last() else {
            continue;
        };
        let mut tbl = doc.as_table_mut();
        let n = parents.len();
        for (i, seg) in parents.iter().enumerate() {
            let existed = tbl.get(seg).map(|e| e.is_table()).unwrap_or(false);
            let entry = tbl.entry(seg).or_insert(toml_edit::table());
            if !entry.is_table() {
                *entry = toml_edit::table();
            }
            let t = entry.as_table_mut().expect("ensured table");
            // A NEWLY-created intermediate parent (not the one that holds
            // the leaf key) contains only a sub-table → mark it implicit so
            // toml_edit emits no bare `[parent]` header above the sub-table.
            if !existed && i + 1 < n {
                t.set_implicit(true);
            }
            tbl = t;
        }
        tbl.insert(last, infer_toml_item(&o.value));
    }
}

/// Strip override keys from a TOML document, pruning now-empty tables.
fn strip_toml_overrides(doc: &mut DocumentMut, keys: &[String]) {
    for k in keys {
        let segs = dot_path(k);
        toml_remove_path_inner(doc.as_table_mut(), &segs);
    }
}
fn toml_remove_path_inner(tbl: &mut toml_edit::Table, segs: &[&str]) {
    match segs {
        [] => {}
        [last] => {
            tbl.remove(*last);
        }
        [head, rest @ ..] => {
            if let Some(child) = tbl.get_mut(*head).and_then(|i| i.as_table_mut()) {
                toml_remove_path_inner(child, rest);
                if child.is_empty() {
                    tbl.remove(*head);
                }
            }
        }
    }
}

fn load_toml_document(path: &Path) -> Result<DocumentMut, Box<dyn Error>> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let text = fs::read_to_string(path)?;
    Ok(text.parse::<DocumentMut>()?)
}

fn parse_dotenv(path: &Path) -> Result<std::collections::BTreeMap<String, String>, Box<dyn Error>> {
    use std::collections::BTreeMap;
    let mut map = BTreeMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let text = fs::read_to_string(path)?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim().to_string();
            let val = v.trim().to_string();
            if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                map.insert(key, val);
            }
        }
    }
    Ok(map)
}

fn write_dotenv(
    path: &Path,
    map: &std::collections::BTreeMap<String, String>,
) -> Result<(), Box<dyn Error>> {
    let body = map
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let final_body = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    };
    ensure_parent_dir(path)?;
    atomic_write(path, final_body.as_bytes())?;
    // Restrict permissions on Unix: API keys live in this file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
        if let Some(parent) = path.parent() {
            let mut dperm = fs::metadata(parent)?.permissions();
            dperm.set_mode(0o700);
            fs::set_permissions(parent, dperm)?;
        }
    }
    Ok(())
}

pub(crate) fn mask_secret(value: &str) -> String {
    // Count / slice by CHARS, not bytes — byte slicing (`&value[..4]`) panics
    // when a multibyte char straddles the boundary (read paths feed this
    // untrusted on-disk content, e.g. a Claude Desktop profile api key).
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        "•".repeat(chars.len())
    } else {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}{}{}", head, "•".repeat(chars.len() - 8), tail)
    }
}

pub(crate) fn string_match(provider_value: &str, live_value: Option<&str>) -> bool {
    let live = live_value.unwrap_or("");
    provider_value.trim() == live.trim()
}

/// Rust mirror of the frontend `resolveActiveProviderId` — the SAME rule, not
/// shared code: prefer the per-CLI activation marker (`active_provider_ids` in
/// config.json, written by `markActive` after a successful switch) when the
/// marked provider's `base_url` / `api_key` still match the live snapshot;
/// otherwise fall back to the reverse-derived `matched_provider_id`.
///
/// The marker is what tells identical-endpoint entries apart — a standalone
/// provider and a gateway binding can carry the same `base_url` + `api_key`, so
/// field matching alone picks whichever comes first. The live-snapshot check is
/// what keeps a STALE marker (the live config changed since) from lying.
///
/// Single-slot CLIs only. OpenCode/Grok already resolve by id: theirs is in the
/// live config's startup-default pointer.
pub(crate) fn resolve_active_provider_id(
    state: &ActiveState,
    marker: Option<&str>,
    candidates: &[Provider],
) -> Option<String> {
    if let (Some(marker), Some(live)) = (marker, state.live_snapshot.as_ref()) {
        if let Some(m) = candidates.iter().find(|c| c.id == marker) {
            let base_ok = m.base_url == live.base_url.clone().unwrap_or_default();
            let key_ok = mask_secret(&m.api_key) == live.api_key_masked.clone().unwrap_or_default();
            if base_ok && key_ok {
                return Some(marker.to_string());
            }
        }
    }
    state.matched_provider_id.clone()
}

/// Reject a models LIST that repeats a model id. A duplicate would silently
/// override wherever the target keys by id — grok's
/// `[model."<pid>-<id>"]`, OpenCode's `models` map, Claude Desktop's
/// `inferenceModels`. Blank ids are ignored (dropped at write time).
pub(crate) fn reject_duplicate_model_ids(
    models: &[ProviderModel],
    app: &str,
) -> Result<(), Box<dyn Error>> {
    let mut seen = std::collections::HashSet::new();
    for m in models {
        let id = m.id.trim();
        if !id.is_empty() && !seen.insert(id) {
            return Err(format!("Duplicate {app} model id: {id}").into());
        }
    }
    Ok(())
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{lock_home, override_home, EnvVarGuard};

    #[test]
    fn cli_app_key_round_trips_through_parse() {
        // `key()` is the inverse of `parse()`, and both mirror the frontend's
        // `CliApp` union. They key config maps (`sources`,
        // `active_provider_ids`) and tray menu ids, so a drift between them
        // would silently orphan a tool's settings.
        for app in CliApp::all() {
            assert_eq!(
                CliApp::parse(app.key()),
                Some(app),
                "key() must parse back to itself for {app:?}"
            );
        }
        // The one non-lowercase key, spelled out so a rename can't slip by.
        assert_eq!(CliApp::ClaudeDesktop.key(), "claude-desktop");
    }

    #[test]
    fn codex_root_honors_codex_home_env() {
        let _g = lock_home();
        let home = Path::new("/tmp/fake-home");

        {
            let _e = EnvVarGuard::unset("CODEX_HOME");
            assert_eq!(codex_root(home), home.join(".codex"));
        }
        {
            let _e = EnvVarGuard::set("CODEX_HOME", "/custom/codex");
            assert_eq!(codex_root(home), PathBuf::from("/custom/codex"));
        }
        // Empty value is ignored (falls back to ~/.codex).
        {
            let _e = EnvVarGuard::set("CODEX_HOME", "");
            assert_eq!(codex_root(home), home.join(".codex"));
        }
    }

    #[test]
    fn claude_activate_honors_claude_config_dir() {
        let _g = lock_home();
        let tmp = tempdir("claude-cfg-dir");
        let _home = override_home(&tmp);
        let cfg = tmp.join("relocated-claude");
        fs::create_dir_all(&cfg).unwrap();
        let _cfg_env = EnvVarGuard::set("CLAUDE_CONFIG_DIR", &cfg);

        let p = make_provider(CliApp::Claude, "X", "https://api.x.io", "sk-secret");
        activate(&p, &[p.clone()]).unwrap();

        // settings.json must land under CLAUDE_CONFIG_DIR, never ~/.claude.
        assert!(
            cfg.join("settings.json").exists(),
            "settings.json must be written under CLAUDE_CONFIG_DIR"
        );
        assert!(!tmp.join(".claude/settings.json").exists());
        // Reverse-derivation reads the same relocated file.
        let state = read_active_state(CliApp::Claude, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Custom);
    }

    fn binding(app: CliApp, npm: Option<&str>) -> GatewayBinding {
        GatewayBinding {
            id: "b1".into(),
            app,
            model: "m".into(),
            npm: npm.map(str::to_string),
            models: vec![],
            options: vec![],
            api_backend: None,
        }
    }

    #[test]
    fn gateway_base_for_protocol_derives_per_protocol() {
        // Path-less root + OpenAI flavors get /v1; Anthropic/Gemini stay bare.
        assert_eq!(
            gateway_base_for_protocol("https://r.x", GatewayProtocol::Openai),
            "https://r.x/v1"
        );
        assert_eq!(
            gateway_base_for_protocol("https://r.x/", GatewayProtocol::OpenaiCompatible),
            "https://r.x/v1"
        );
        // A pasted /v1 or /v1beta is stripped before re-deriving.
        assert_eq!(
            gateway_base_for_protocol("https://r.x/v1", GatewayProtocol::Gemini),
            "https://r.x"
        );
        assert_eq!(
            gateway_base_for_protocol("https://r.x/v1beta", GatewayProtocol::Anthropic),
            "https://r.x"
        );
    }

    #[test]
    fn protocol_for_binding_maps_app_and_npm() {
        assert!(matches!(
            protocol_for_binding(&binding(CliApp::Claude, None)),
            GatewayProtocol::Anthropic
        ));
        assert!(matches!(
            protocol_for_binding(&binding(CliApp::Codex, None)),
            GatewayProtocol::Openai
        ));
        assert!(matches!(
            protocol_for_binding(&binding(CliApp::Gemini, None)),
            GatewayProtocol::Gemini
        ));
        // OpenCode derives from npm; openai-compatible matched before openai.
        assert!(matches!(
            protocol_for_binding(&binding(
                CliApp::Opencode,
                Some("@ai-sdk/openai-compatible")
            )),
            GatewayProtocol::OpenaiCompatible
        ));
        assert!(matches!(
            protocol_for_binding(&binding(CliApp::Opencode, Some("@ai-sdk/openai"))),
            GatewayProtocol::Openai
        ));
        assert!(matches!(
            protocol_for_binding(&binding(CliApp::Opencode, None)),
            GatewayProtocol::OpenaiCompatible
        ));
    }

    #[test]
    fn provider_from_binding_synthesizes_provider() {
        let g = Gateway {
            name: "Router".into(),
            base_url: "https://gw.x".into(),
            api_key: "sk-1".into(),
            bindings: vec![],
            favicon: Some("data:img".into()),
        };
        // Claude binding → Anthropic base (bare root), no npm/models.
        let claude = provider_from_binding(&g, &binding(CliApp::Claude, None));
        assert_eq!(claude.id, "b1");
        assert_eq!(claude.app, CliApp::Claude);
        assert_eq!(claude.name, "Router");
        assert_eq!(claude.base_url, "https://gw.x");
        assert_eq!(claude.api_key, "sk-1");
        assert_eq!(claude.favicon.as_deref(), Some("data:img"));
        assert!(claude.npm.is_none());

        // OpenCode binding → /v1 base + npm filled (defaulted from protocol).
        let oc = provider_from_binding(&g, &binding(CliApp::Opencode, None));
        assert_eq!(oc.base_url, "https://gw.x/v1");
        assert_eq!(oc.npm.as_deref(), Some("@ai-sdk/openai-compatible"));
    }

    #[test]
    fn parse_model_ids_openai_shape() {
        let body = serde_json::json!({ "data": [{ "id": "gpt-5" }, { "id": "gpt-4o" }] });
        assert_eq!(parse_model_ids(&body, false), vec!["gpt-4o", "gpt-5"]); // sorted+deduped
    }

    #[test]
    fn parse_model_ids_gemini_strips_models_prefix() {
        let body = serde_json::json!({
            "models": [{ "name": "models/gemini-2.5-pro" }, { "name": "models/gemini-2.5-flash" }]
        });
        assert_eq!(
            parse_model_ids(&body, true),
            vec!["gemini-2.5-flash", "gemini-2.5-pro"]
        );
    }

    #[test]
    fn parse_model_ids_handles_missing_or_wrong_shape() {
        assert!(parse_model_ids(&serde_json::json!({}), false).is_empty());
        assert!(parse_model_ids(&serde_json::json!({ "models": [] }), true).is_empty());
    }

    #[test]
    fn cli_search_paths_includes_opencode_specific_dirs() {
        let paths = cli_search_paths("opencode");
        let has = |needle: &str| {
            paths
                .iter()
                .any(|p| p.to_string_lossy().replace('\\', "/").contains(needle))
        };
        assert!(has(".opencode/bin"), "missing ~/.opencode/bin: {paths:?}");
        assert!(has(".bun/bin"), "missing ~/.bun/bin: {paths:?}");
        assert!(has("go/bin"), "missing ~/go/bin: {paths:?}");
    }

    #[test]
    fn cli_search_paths_skips_opencode_only_dirs_for_other_tools() {
        // Path::ends_with matches whole components, so `~/go/bin` matches
        // but `~/.cargo/bin` does NOT (because the parent component is
        // `.cargo`, not `go`).
        let paths = cli_search_paths("claude");
        let matching: Vec<_> = paths.iter().filter(|p| p.ends_with("go/bin")).collect();
        assert!(
            matching.is_empty(),
            "claude search list contains ~/go/bin entries: {matching:?}"
        );
    }

    #[test]
    fn cli_search_paths_includes_local_bin_on_every_platform() {
        // Claude Code's native installer targets `~/.local/bin` on ALL
        // three platforms — on Windows that's
        // `%USERPROFILE%\.local\bin\claude.exe` (claude.ai/install.ps1 →
        // bootstrap.ps1 → `claude.exe install`). It used to be gated
        // `#[cfg(unix)]`, which left the InstallGuide's own recommended
        // Windows install undetected until the user re-logged in (the
        // PATH fallback can't see a just-appended user PATH entry).
        let paths = cli_search_paths("claude");
        let has_local_bin = paths
            .iter()
            .any(|p| p.ends_with(std::path::Path::new(".local").join("bin")));
        assert!(has_local_bin, "search list missing ~/.local/bin: {paths:?}");
    }

    #[test]
    fn cli_search_paths_honors_installer_custom_dirs() {
        // codex's install.sh:8 / install.ps1:744 read $CODEX_INSTALL_DIR
        // and grok's install.sh:157 / install.ps1:153 read $GROK_BIN_DIR,
        // exactly like opencode's $OPENCODE_INSTALL_DIR (already honored).
        // A user who sets one gets the binary somewhere none of the fixed
        // paths can see, on every platform.
        let _lock = crate::testutils::lock_home();
        let codex_dir = std::env::temp_dir().join("termory-test-codex-install-dir");
        let grok_dir = std::env::temp_dir().join("termory-test-grok-bin-dir");
        let _codex_var = crate::testutils::EnvVarGuard::set("CODEX_INSTALL_DIR", &codex_dir);
        let _grok_var = crate::testutils::EnvVarGuard::set("GROK_BIN_DIR", &grok_dir);

        let codex_paths = cli_search_paths("codex");
        let grok_paths = cli_search_paths("grok");
        // Per-tool sections stay scoped — codex's var must not leak into
        // another tool's list.
        let claude_paths = cli_search_paths("claude");

        assert!(
            codex_paths.contains(&codex_dir),
            "CODEX_INSTALL_DIR ignored: {codex_paths:?}"
        );
        assert!(
            grok_paths.contains(&grok_dir),
            "GROK_BIN_DIR ignored: {grok_paths:?}"
        );
        assert!(
            !claude_paths.contains(&codex_dir),
            "CODEX_INSTALL_DIR leaked into claude's list: {claude_paths:?}"
        );
        // An explicitly set install dir must OUTRANK every package-manager
        // dir — `find_cli_binary` returns the first hit, so a stale npm
        // copy would otherwise win over the dir the user chose.
        assert_eq!(
            codex_paths.first(),
            Some(&codex_dir),
            "CODEX_INSTALL_DIR must be probed first: {codex_paths:?}"
        );
        assert_eq!(
            grok_paths.first(),
            Some(&grok_dir),
            "GROK_BIN_DIR must be probed first: {grok_paths:?}"
        );
    }

    /// The hot path (`detect_install_snapshot`: every tray-menu open,
    /// every watcher burst) calls this for every CLI the dir scan
    /// missed, and each miss is a ~1s interactive-shell spawn. Pin both
    /// halves of the contract: a verdict is reused within the TTL, and
    /// `clear_shell_probe_cache` really drops it.
    #[test]
    fn shell_probe_cache_reuses_verdict_until_cleared() {
        let _lock = crate::testutils::lock_home();
        // A name no shell can resolve → a stable `false` verdict, and
        // the probe stays hermetic (nothing real is executed).
        let tool = "termory-nonexistent-tool-probe";
        let stamp = || {
            SHELL_PROBE
                .lock()
                .ok()
                .and_then(|c| c.iter().find(|(t, ..)| *t == tool).map(|(_, at, _)| *at))
        };
        clear_shell_probe_cache();

        assert!(!shell_installed_cached(tool));
        let first = stamp().expect("first call must record a verdict");

        assert!(!shell_installed_cached(tool));
        // A re-probe would overwrite the entry with a fresh Instant, so
        // an unchanged stamp is proof the shell was NOT spawned again.
        // (Deterministic — no wall-clock comparison to go flaky in CI.)
        assert_eq!(
            stamp(),
            Some(first),
            "second call re-probed instead of reusing the cached verdict"
        );

        clear_shell_probe_cache();
        assert!(
            stamp().is_none(),
            "clear_shell_probe_cache must drop every verdict"
        );
    }

    #[test]
    fn cli_search_paths_includes_claude_legacy_local_install() {
        // `claude migrate-installer`'s target. It's alias-exposed, never
        // on PATH, so without this entry the ONLY thing that could find
        // it is the ~1s interactive-shell fallback (and on Windows,
        // nothing at all — there is no shell fallback there).
        let _lock = crate::testutils::lock_home();
        let home = tempdir("claude-local");
        let _home_env = crate::testutils::override_home(&home);

        let expected = home.join(".claude").join("local");
        let paths = cli_search_paths("claude");
        assert!(
            paths.contains(&expected),
            "claude search list missing the legacy local install: {paths:?}"
        );
        // Claude-scoped, like every other per-tool section.
        let codex_paths = cli_search_paths("codex");
        assert!(
            !codex_paths.contains(&expected),
            "claude-only dir leaked into codex's list: {codex_paths:?}"
        );

        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn cli_search_paths_claude_local_follows_config_dir_override() {
        // Every other Claude state path honors $CLAUDE_CONFIG_DIR; the
        // local install lives under that same root, so it must too.
        let _lock = crate::testutils::lock_home();
        let home = tempdir("claude-local-cfg");
        let _home_env = crate::testutils::override_home(&home);
        let cfg = home.join("custom-claude-home");
        let _cfg_env = crate::testutils::EnvVarGuard::set("CLAUDE_CONFIG_DIR", &cfg);

        let paths = cli_search_paths("claude");
        assert!(
            paths.contains(&cfg.join("local")),
            "legacy local install must follow CLAUDE_CONFIG_DIR: {paths:?}"
        );

        fs::remove_dir_all(&home).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cli_search_paths_windows_includes_codex_and_winget_dirs() {
        // Two Windows-only install landings that no other entry covers:
        // codex's standalone installer (install.ps1:743) and winget's
        // `portable` shim dir (Claude Code's manifest is that type).
        let joined = |paths: Vec<std::path::PathBuf>| -> String {
            paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("|")
        };
        let codex = joined(cli_search_paths("codex"));
        assert!(
            codex.contains("Programs\\OpenAI\\Codex\\bin"),
            "Windows codex search list missing the standalone bin dir: {codex}"
        );
        let claude = joined(cli_search_paths("claude"));
        assert!(
            claude.contains("Microsoft\\WinGet\\Links"),
            "Windows search list missing the winget Links dir: {claude}"
        );
        assert!(
            !claude.contains("Programs\\OpenAI\\Codex\\bin"),
            "codex-only dir leaked into claude's list: {claude}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cli_search_paths_windows_includes_scoop_and_choco() {
        let paths = cli_search_paths("opencode");
        let joined: String = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined.contains("scoop\\shims"),
            "Windows search list missing scoop shims: {joined}"
        );
        assert!(
            joined.contains("chocolatey\\bin"),
            "Windows search list missing chocolatey bin: {joined}"
        );
        assert!(
            joined.contains("\\Program Files\\nodejs"),
            "Windows search list missing Node.js MSI dir: {joined}"
        );
    }

    #[test]
    fn cli_search_paths_dedupes() {
        let paths = cli_search_paths("opencode");
        let mut sorted: Vec<_> = paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        sorted.sort();
        let len_before = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            len_before,
            "cli_search_paths returned duplicates: {paths:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_candidates_unix_returns_bare_name_only() {
        let dir = std::path::Path::new("/opt/bin");
        let got = executable_candidates("claude", dir);
        assert_eq!(got, vec![dir.join("claude")]);
    }

    #[test]
    fn augmented_path_for_prepends_the_binary_dir() {
        let sep = if cfg!(unix) { ':' } else { ';' };
        let bin = std::env::temp_dir().join("some-bin-dir").join("codex");
        let got = augmented_path_for(&bin).expect("binary has a parent dir");
        let prefix = format!("{}{}", bin.parent().unwrap().display(), sep);
        assert!(
            got.starts_with(&prefix),
            "PATH must start with the binary's dir: {got}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn executable_candidates_windows_returns_cmd_exe_and_bare() {
        let dir = std::path::Path::new("C:\\bin");
        let got = executable_candidates("claude", dir);
        assert_eq!(
            got,
            vec![
                dir.join("claude.cmd"),
                dir.join("claude.exe"),
                dir.join("claude"),
            ]
        );
    }

    // The timeout / kill / reap behaviour behind `output_with_timeout`
    // is tested where it lives, in `process::tests` — including the
    // grandchild case this file's own tests never covered.

    #[test]
    fn registrable_domain_strips_subdomains_and_skips_ips() {
        assert_eq!(
            registrable_domain("api.openai.com"),
            Some("openai.com".to_string())
        );
        assert_eq!(
            registrable_domain("chat.deepseek.com"),
            Some("deepseek.com".to_string())
        );
        // Already a brand root — returns itself.
        assert_eq!(
            registrable_domain("openai.com"),
            Some("openai.com".to_string())
        );
        // Deep subdomain — still collapses to last two labels.
        assert_eq!(
            registrable_domain("v1.api.openai.com"),
            Some("openai.com".to_string())
        );
        // IP literals: skip the strip so the caller falls back to the
        // literal host (the only sane probe for IP-addressed servers).
        assert_eq!(registrable_domain("127.0.0.1"), None);
        assert_eq!(registrable_domain("[::1]"), None);
        // Bare hostnames (intranet) — no `.` → can't be stripped.
        assert_eq!(registrable_domain("localhost"), None);
    }

    /// Captured VERBATIM from a real interactive zsh on this project's
    /// dev machine (fastfetch banner in `.zshrc`), with the `\x1b[18G`
    /// column escapes intact — those are what make the banner's own
    /// numbers survive tokenization.
    const REAL_BANNER_PROBE: &str = concat!(
        "  \u{f08c7}  OS \u{1b}[18GmacOS\n",
        "  \u{f033d}  Kernel \u{1b}[18G25.5.0\n",
        "  \u{f035b}  Memory \u{1b}[18G11.56 GiB / 16.00 GiB 72%\n",
        "  \u{f02ca}  Disk \u{1b}[18G331.18 GiB / 460.43 GiB 72%\n",
        "__termory_shell_probe__\n",
        "codex-cli 0.144.6\n"
    );

    /// The bug this marker exists for: `parse_version` returns the FIRST
    /// version-shaped token, so an rc banner ahead of the real output
    /// wins. On this machine that reported the host's RAM (`16.00`) as
    /// the codex version.
    #[test]
    fn after_shell_marker_ignores_rc_banner_output() {
        // Whole text: the banner wins — this is the broken behaviour.
        assert_eq!(
            parse_version(REAL_BANNER_PROBE),
            Some("16.00".to_string()),
            "precondition: the banner really does shadow the real version"
        );
        // Marker-scoped: only the tool's own output is considered.
        assert_eq!(
            after_shell_marker(REAL_BANNER_PROBE).and_then(parse_version),
            Some("0.144.6".to_string())
        );
    }

    /// The marker does POSITIONAL splitting, not content matching — it
    /// drops everything before itself without knowing what a banner
    /// looks like. These cover shapes far apart from the fastfetch one
    /// above so a future "smarter" parse can't quietly narrow that.
    #[test]
    fn after_shell_marker_survives_any_banner_shape() {
        let probe = "codex-cli 0.144.6";
        for (name, banner) in [
            (
                "plain greeting",
                "Welcome back! You have 3.14 unread items.",
            ),
            (
                "motd with a version",
                "Last login: 2026-07-26  System 15.2.1 build 24C101",
            ),
            (
                "package-manager notice",
                "npm notice New major version available! 10.9.2 -> 11.1.0",
            ),
            (
                "column escapes (fastfetch)",
                "  Memory \u{1b}[18G11.56 GiB / 16.00 GiB 72%",
            ),
            // No trailing newline: the marker lands mid-line.
            ("unterminated line", "loading... 9.99"),
            // A banner that happens to print the marker text itself —
            // `rsplit` keeps the LAST occurrence, which is ours.
            (
                "banner containing the marker",
                concat!("debug: ", "__termory_shell_probe__", " was here 7.77"),
            ),
        ] {
            let sep = if banner.ends_with("9.99") { "" } else { "\n" };
            let raw = format!("{banner}{sep}{SHELL_PROBE_MARKER}\n{probe}\n");
            assert_eq!(
                after_shell_marker(&raw).and_then(parse_version),
                Some("0.144.6".to_string()),
                "banner shape: {name}"
            );
        }
    }

    /// Many version-shaped tokens before the marker, none of which may
    /// win — the failure mode is "first match wins", so volume matters.
    #[test]
    fn after_shell_marker_ignores_a_banner_full_of_versions() {
        let noisy: String = (0..20).map(|i| format!("tool-{i} 1.{i}.0\n")).collect();
        let raw = format!("{noisy}{SHELL_PROBE_MARKER}\ncodex-cli 0.144.6\n");
        assert_eq!(
            after_shell_marker(&raw).and_then(parse_version),
            Some("0.144.6".to_string())
        );
    }

    #[test]
    fn after_shell_marker_rejects_output_without_a_marker() {
        // A shell that never reached the `echo` produced nothing we can
        // trust — don't fall back to parsing the banner.
        assert_eq!(after_shell_marker("codex-cli 0.144.6"), None);
        assert_eq!(after_shell_marker(""), None);
        // Marker present but nothing after it → empty, not the banner.
        let probe = format!("banner\n{SHELL_PROBE_MARKER}\n");
        assert_eq!(after_shell_marker(&probe).map(str::trim), Some(""));
    }

    #[test]
    fn marked_shell_command_puts_the_marker_first_and_folds_stderr() {
        let built = marked_shell_command("codex --version");
        assert_eq!(
            built,
            format!("echo {SHELL_PROBE_MARKER}; codex --version 2>&1")
        );
    }

    #[test]
    fn parse_version_handles_common_cli_outputs() {
        // Plain "X.Y.Z" output
        assert_eq!(parse_version("0.5.7"), Some("0.5.7".to_string()));
        // Leading "v" prefix
        assert_eq!(parse_version("v1.2.3"), Some("1.2.3".to_string()));
        // Tool name + version (Codex / opencode style)
        assert_eq!(
            parse_version("codex-cli 0.46.0"),
            Some("0.46.0".to_string())
        );
        // npm @scope/name@1.2.3 isn't a version flag's normal output,
        // but the parser shouldn't trip on the leading "@" digits.
        assert_eq!(
            parse_version("@anthropic-ai/claude-code 0.0.32"),
            Some("0.0.32".to_string())
        );
        // Parenthesized build metadata in output
        assert_eq!(
            parse_version("opencode 0.5.7 (build abc123)"),
            Some("0.5.7".to_string())
        );
        // SemVer prerelease with hyphen
        assert_eq!(
            parse_version("gemini 0.10.0-preview.1"),
            Some("0.10.0-preview.1".to_string())
        );
        // Major.minor only
        assert_eq!(parse_version("v1.2"), Some("1.2".to_string()));
        // No version → None
        assert_eq!(parse_version("something with no version"), None);
        // Empty
        assert_eq!(parse_version(""), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_app_info_in_matches_by_bundle_id_not_name() {
        let parent = tempdir("codex-app");
        // Distinct versions identify WHICH bundle matched (the info
        // struct no longer carries the bundle path).
        let write_app = |name: &str, bundle_id: &str, version: &str| {
            let contents = parent.join(name).join("Contents");
            fs::create_dir_all(&contents).unwrap();
            fs::write(
                contents.join("Info.plist"),
                format!(
                    r#"<?xml version="1.0"?><plist><dict>
                        <key>CFBundleIdentifier</key><string>{bundle_id}</string>
                        <key>CFBundleShortVersionString</key><string>{version}</string>
                    </dict></plist>"#
                ),
            )
            .unwrap();
        };

        // No apps at all → None.
        assert!(codex_app_info_in(&[parent.clone()]).is_none());

        // An unrelated app whose NAME doesn't match and id isn't codex.
        write_app("Slack.app", "com.tinyspeck.slackmacgap", "1.0.0");
        assert!(codex_app_info_in(&[parent.clone()]).is_none());

        // The merged desktop app kept bundle id com.openai.codex but is
        // NAMED ChatGPT.app (in-place update from the old Codex app) —
        // the id, not the name, must decide. Version comes from the
        // SAME plist read that matched the id.
        write_app("ChatGPT.app", CODEX_APP_BUNDLE_ID, "26.707.31428");
        let info = codex_app_info_in(&[parent.clone()]).unwrap();
        assert_eq!(info.version.as_deref(), Some("26.707.31428"));
        // No Contents/Resources/codex file yet → no bundled CLI.
        assert_eq!(info.bundled_cli, None);

        // A renamed bundle (neither known name) is still found by the
        // full scan.
        fs::rename(parent.join("ChatGPT.app"), parent.join("Renamed.app")).unwrap();
        let info = codex_app_info_in(&[parent.clone()]).unwrap();
        assert_eq!(info.version.as_deref(), Some("26.707.31428"));

        // ChatGPT Classic (the old chat app, id com.openai.chat) must
        // NOT count as the Codex app even under the ChatGPT.app name.
        fs::remove_dir_all(parent.join("Renamed.app")).unwrap();
        write_app("ChatGPT.app", "com.openai.chat", "1.2026.100");
        assert!(codex_app_info_in(&[parent.clone()]).is_none());

        // Bundled CLI: reported only when the app ships an actual
        // Contents/Resources/codex file.
        fs::remove_dir_all(parent.join("ChatGPT.app")).unwrap();
        write_app("ChatGPT.app", CODEX_APP_BUNDLE_ID, "26.707.31428");
        let resources = parent.join("ChatGPT.app/Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("codex"), b"#!/bin/sh\n").unwrap();
        let info = codex_app_info_in(&[parent.clone()]).unwrap();
        assert_eq!(info.bundled_cli, Some(resources.join("codex")));

        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn parse_appx_package_version_reads_second_underscore_segment() {
        // Real package name reported for the Codex/ChatGPT app
        // (openai/codex#32772) — Name_Version_Arch__PublisherId, the
        // `__` marking an empty ResourceId segment.
        assert_eq!(
            parse_appx_package_version("OpenAI.Codex_26.707.9564.0_x64__2p2nqsd0c76g0"),
            Some("26.707.9564.0".to_string())
        );
        // Malformed / no underscores at all.
        assert_eq!(parse_appx_package_version("OpenAIcodex"), None);
        assert_eq!(parse_appx_package_version(""), None);
    }

    #[test]
    fn codex_appx_installed_in_matches_package_family_prefix() {
        let local = tempdir("codex-appx");
        // No Packages dir at all → not installed (no panic).
        assert!(!codex_appx_installed_in(&local));

        let packages = local.join("Packages");
        fs::create_dir_all(&packages).unwrap();
        // Unrelated MSIX packages must not count.
        fs::create_dir_all(packages.join("Microsoft.Edge_8wekyb3d8bbwe")).unwrap();
        fs::create_dir_all(packages.join("Claude_pzs8sxrjxfjjc")).unwrap();
        assert!(!codex_appx_installed_in(&local));

        // A Beta-only install counts too — "ChatGPT (Beta)" is the
        // separate `OpenAI.CodexBeta` package family (msstore catalog).
        fs::create_dir_all(packages.join("OpenAI.CodexBeta_2p2nqsd0c76g0")).unwrap();
        assert!(codex_appx_installed_in(&local));

        // The real PackageFamilyName shape: OpenAI.Codex_<publisherhash>
        // (hash varies per signing identity → prefix match).
        fs::create_dir_all(packages.join("OpenAI.Codex_2p2nqsd0c76g0")).unwrap();
        assert!(codex_appx_installed_in(&local));

        fs::remove_dir_all(&local).unwrap();
    }

    #[test]
    fn pick_appx_full_name_prefers_stable_over_beta_and_rejects_strays() {
        // Both installed → stable wins regardless of line order.
        assert_eq!(
            pick_appx_full_name(
                "OpenAI.CodexBeta_26.800.1.0_x64__2p2nqsd0c76g0\r\nOpenAI.Codex_26.721.4979.0_x64__2p2nqsd0c76g0\r\n"
            ),
            Some("OpenAI.Codex_26.721.4979.0_x64__2p2nqsd0c76g0".to_string())
        );
        // Beta alone is still reported.
        assert_eq!(
            pick_appx_full_name("OpenAI.CodexBeta_26.800.1.0_x64__2p2nqsd0c76g0"),
            Some("OpenAI.CodexBeta_26.800.1.0_x64__2p2nqsd0c76g0".to_string())
        );
        // Wildcard strays matching neither identity are ignored, not
        // mis-parsed; empty output is None.
        assert_eq!(
            pick_appx_full_name("OpenAI.CodexSomething_1.0.0.0_x64__hash"),
            None
        );
        assert_eq!(pick_appx_full_name("  \r\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn nvm_node_bin_dirs_orders_default_alias_first_then_newest() {
        let base = tempdir("nvm-order");
        // v9 pins the numeric (not lexicographic) sort; a bin-less dir
        // and a stray file must both be skipped.
        for v in ["v9.1.0", "v20.19.4", "v22.21.1"] {
            fs::create_dir_all(base.join(v).join("bin")).unwrap();
        }
        fs::create_dir_all(base.join("v18.0.0")).unwrap(); // no bin/
        fs::write(base.join(".DS_Store"), b"").unwrap();

        // No alias → newest first.
        let dirs = nvm_node_bin_dirs(&base, None);
        assert_eq!(
            dirs,
            vec![
                base.join("v22.21.1/bin"),
                base.join("v20.19.4/bin"),
                base.join("v9.1.0/bin"),
            ]
        );

        // Default alias promotes its version to the front; a missing
        // `v` prefix in the alias file still matches.
        let dirs = nvm_node_bin_dirs(&base, Some("20.19.4"));
        assert_eq!(
            dirs,
            vec![
                base.join("v20.19.4/bin"),
                base.join("v22.21.1/bin"),
                base.join("v9.1.0/bin"),
            ]
        );

        // A chained alias (`lts/*`) names no version dir → newest first.
        let dirs = nvm_node_bin_dirs(&base, Some("lts/*"));
        assert_eq!(dirs[0], base.join("v22.21.1/bin"));

        fs::remove_dir_all(&base).unwrap();
    }

    fn tempdir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("termory-providers-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_provider(app: CliApp, name: &str, base: &str, key: &str) -> Provider {
        Provider {
            id: format!("test-{name}"),
            app,
            kind: ProviderKind::Custom,
            name: name.into(),
            base_url: base.into(),
            api_key: key.into(),
            model: "test-model".into(),
            npm: None,
            models: Vec::new(),
            favicon: None,
            options: Vec::new(),
            api_backend: None,
        }
    }

    #[test]
    fn activate_rejects_a_provider_without_a_base_url() {
        let _g = lock_home();
        let tmp = tempdir("no-base-url");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        // Both editors refuse to save without a Base URL, so this only comes
        // from a hand-edited providers.json — and it must not half-switch the
        // CLI. Before the guard the per-CLI writers read "" as "clear this
        // field": the base URL was stripped while the API key was still
        // written, leaving Claude pointed at the OFFICIAL endpoint holding a
        // third party's token.
        let mut p = make_provider(CliApp::Claude, "no-base", "", "sk-live");
        let err = activate(&p, &[p.clone()]).expect_err("empty base URL is rejected");
        assert!(
            err.to_string().contains("missing a Base URL"),
            "unexpected error: {err}"
        );
        // Whitespace is not a Base URL either.
        p.base_url = "   ".into();
        assert!(activate(&p, &[p.clone()]).is_err());
        // Nothing was written to the CLI's live config.
        assert!(
            !tmp.join(".claude/settings.json").exists(),
            "a rejected activation must not touch settings.json"
        );

        // The same provider with a Base URL activates normally.
        p.base_url = "https://api.example.com".into();
        activate(&p, &[p.clone()]).unwrap();
        let settings: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings
                .pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(|v| v.as_str()),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn provider_overrides_apply_strip_and_infer_types_json() {
        // Claude (JSON) overrides: dot-path nesting, type inference,
        // clean removal of the previous provider's keys on switch
        // (incl. empty-parent pruning), and full strip on deactivate.
        let _g = lock_home();
        let tmp = tempdir("ovr-json");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let mut a = make_provider(CliApp::Claude, "A", "https://a.x.io", "sk-a");
        a.options = vec![
            ProviderOption {
                key: "env.FOO".into(),
                value: "bar".into(),
            },
            ProviderOption {
                key: "permissions.defaultMode".into(),
                value: "acceptEdits".into(),
            },
            // Managed key — MUST be ignored, never clobbers the real
            // baseUrl from the dedicated field.
            ProviderOption {
                key: "env.ANTHROPIC_BASE_URL".into(),
                value: "https://evil.example".into(),
            },
        ];
        let mut b = make_provider(CliApp::Claude, "B", "https://b.x.io", "sk-b");
        b.options = vec![
            // Looks like a bool, but env values must stay strings.
            ProviderOption {
                key: "env.FLAG".into(),
                value: "true".into(),
            },
            // Non-env key → type-inferred to a number.
            ProviderOption {
                key: "cleanupPeriodDays".into(),
                value: "30".into(),
            },
        ];
        let all = vec![a.clone(), b.clone()];
        let read = || -> JsonValue {
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap()
        };

        activate(&a, &all).unwrap();
        let cfg = read();
        assert_eq!(
            cfg.pointer("/env/FOO").and_then(|v| v.as_str()),
            Some("bar")
        );
        assert_eq!(
            cfg.pointer("/permissions/defaultMode")
                .and_then(|v| v.as_str()),
            Some("acceptEdits")
        );
        // Managed key override is ignored: the dedicated baseUrl wins.
        assert_eq!(
            cfg.pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(|v| v.as_str()),
            Some("https://a.x.io"),
            "overrides must never clobber the managed baseUrl"
        );

        // Switch to B → A's keys gone (empty `permissions` pruned), B's
        // applied with numeric type inference.
        activate(&b, &all).unwrap();
        let cfg = read();
        assert!(
            cfg.pointer("/env/FOO").is_none(),
            "previous provider's override must be stripped on switch"
        );
        assert!(
            cfg.pointer("/permissions").is_none(),
            "emptied parent object must be pruned"
        );
        // env.* stays a string even when it looks like a bool.
        assert_eq!(
            cfg.pointer("/env/FLAG").and_then(|v| v.as_str()),
            Some("true")
        );
        assert!(cfg.pointer("/env/FLAG").unwrap().as_bool().is_none());
        // non-env key is type-inferred to a real number.
        assert_eq!(
            cfg.pointer("/cleanupPeriodDays").and_then(|v| v.as_i64()),
            Some(30)
        );

        // Deactivate → B's overrides gone too.
        deactivate(CliApp::Claude, &all).unwrap();
        let cfg = read();
        assert!(cfg.pointer("/env/FLAG").is_none());
        assert!(cfg.pointer("/cleanupPeriodDays").is_none());
    }

    #[test]
    fn provider_overrides_apply_strip_nested_toml() {
        // Codex (TOML) overrides: nested table path + bool inference,
        // pruned cleanly on deactivate.
        let _g = lock_home();
        let tmp = tempdir("ovr-toml");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        let mut p = make_provider(CliApp::Codex, "C", "https://c.x.io/v1", "sk-c");
        p.options = vec![
            ProviderOption {
                key: "model_reasoning_effort".into(),
                value: "high".into(),
            },
            ProviderOption {
                key: "tools.web_search".into(),
                value: "true".into(),
            },
            // A 3-level dotted key: the intermediate parent `[sandbox]`
            // must NOT be emitted as a bare empty header.
            ProviderOption {
                key: "sandbox.network.enabled".into(),
                value: "true".into(),
            },
        ];
        let all = vec![p.clone()];

        activate(&p, &all).unwrap();
        let txt = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
        assert!(
            !txt.contains("[sandbox]\n"),
            "no bare `[sandbox]` header: {txt}"
        );
        let doc: toml::Value = toml::from_str(&txt).unwrap();
        assert_eq!(
            doc.get("model_reasoning_effort").and_then(|v| v.as_str()),
            Some("high")
        );
        assert_eq!(
            doc.get("tools")
                .and_then(|t| t.get("web_search"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "nested tools.web_search must be a real bool, not the string \"true\""
        );
        assert_eq!(
            doc.get("sandbox")
                .and_then(|s| s.get("network"))
                .and_then(|n| n.get("enabled"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "deep dotted key must round-trip"
        );

        deactivate(CliApp::Codex, &all).unwrap();
        let txt = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
        let doc: toml::Value = toml::from_str(&txt).unwrap();
        assert!(doc.get("model_reasoning_effort").is_none());
        assert!(
            doc.get("tools").is_none(),
            "emptied [tools] table must be pruned"
        );
    }

    #[test]
    fn claude_activate_and_reverse_roundtrip() {
        let _g = lock_home();
        let tmp = tempdir("claude-rt");
        let _home = override_home(&tmp);
        // Existing unrelated settings preserved.
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(
            tmp.join(".claude/settings.json"),
            r#"{"permissions": {"foo": true}, "env": {"OTHER": "x"}}"#,
        )
        .unwrap();
        let p = make_provider(
            CliApp::Claude,
            "Anthropic-thirdparty",
            "https://api.x.io",
            "sk-secret",
        );
        activate(&p, &[p.clone()]).unwrap();
        let state = read_active_state(CliApp::Claude, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Custom);
        assert_eq!(
            state.matched_provider_id.as_deref(),
            Some("test-Anthropic-thirdparty")
        );
        // Unrelated keys preserved
        let after: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            after.pointer("/permissions/foo").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            after.pointer("/env/OTHER").and_then(|v| v.as_str()),
            Some("x")
        );
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(|v| v.as_str()),
            Some("https://api.x.io")
        );

        // Model written into env.ANTHROPIC_MODEL (matches cc-switch
        // preset shape and Claude's auth priority chain). Top-level
        // `model` must NOT be set — env wins anyway, and writing both
        // forks the source of truth.
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_MODEL")
                .and_then(|v| v.as_str()),
            Some("test-model")
        );
        assert!(
            after.get("model").is_none(),
            "top-level model must not be set"
        );

        // Deactivate restores Official + leaves unrelated env keys.
        deactivate(CliApp::Claude, &[p.clone()]).unwrap();
        let state2 = read_active_state(CliApp::Claude, &[p.clone()]).unwrap();
        assert_eq!(state2.kind, ActiveKind::Official);
        let after2: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            after2.pointer("/permissions/foo").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            after2.pointer("/env/OTHER").and_then(|v| v.as_str()),
            Some("x")
        );
        assert!(after2.pointer("/env/ANTHROPIC_BASE_URL").is_none());
        assert!(after2.pointer("/env/ANTHROPIC_MODEL").is_none());
    }

    #[test]
    fn claude_oauth_login_then_activate_api_then_deactivate_keeps_credentials() {
        // Claude stores OAuth credentials in a separate file
        // (`~/.claude/.credentials.json`, see `src/utils/auth.ts:1323`)
        // that we never touch. Confirm activate → deactivate leaves
        // that file byte-identical, so the user stays logged in.
        let _g = lock_home();
        let tmp = tempdir("claude-oauth-keep");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        // Stage 1: user ran `claude login` — OAuth tokens persisted.
        let creds_contents = r#"{
          "claudeAiOauth": {
            "accessToken": "at-original",
            "refreshToken": "rt-original",
            "expiresAt": 9999999999000
          }
        }"#;
        fs::write(tmp.join(".claude/.credentials.json"), creds_contents).unwrap();

        // Stage 2: activate Custom provider via Termory.
        let p = make_provider(CliApp::Claude, "api-temp", "https://temp.api", "sk-temp");
        activate(&p, &[p.clone()]).unwrap();

        // settings.json now has env injection.
        let settings_after_activate: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings_after_activate
                .pointer("/env/ANTHROPIC_AUTH_TOKEN")
                .and_then(|v| v.as_str()),
            Some("sk-temp")
        );

        // OAuth credentials file untouched.
        let creds_after_activate =
            fs::read_to_string(tmp.join(".claude/.credentials.json")).unwrap();
        assert_eq!(
            creds_after_activate, creds_contents,
            "OAuth credentials file must survive activate byte-for-byte"
        );

        // Stage 3: deactivate.
        deactivate(CliApp::Claude, &[p.clone()]).unwrap();
        let creds_after_deactivate =
            fs::read_to_string(tmp.join(".claude/.credentials.json")).unwrap();
        assert_eq!(
            creds_after_deactivate, creds_contents,
            "OAuth credentials file must survive deactivate byte-for-byte"
        );

        // settings.json env stripped.
        let settings_after_deactivate: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert!(settings_after_deactivate
            .pointer("/env/ANTHROPIC_AUTH_TOKEN")
            .is_none());
        assert!(settings_after_deactivate
            .pointer("/env/ANTHROPIC_BASE_URL")
            .is_none());
        assert!(settings_after_deactivate
            .pointer("/env/ANTHROPIC_MODEL")
            .is_none());
    }

    #[test]
    fn claude_reverse_falls_back_to_top_level_model_for_legacy_settings() {
        // settings.json written by older Termory versions put the
        // model at the top level. The active-state reader should
        // still match these correctly.
        let _g = lock_home();
        let tmp = tempdir("claude-legacy-model");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(
            tmp.join(".claude/settings.json"),
            r#"{
              "env": {
                "ANTHROPIC_BASE_URL": "https://legacy.example",
                "ANTHROPIC_AUTH_TOKEN": "sk-legacy"
              },
              "model": "legacy-model-id"
            }"#,
        )
        .unwrap();
        let p = make_provider(
            CliApp::Claude,
            "legacy",
            "https://legacy.example",
            "sk-legacy",
        );
        let state = read_active_state(CliApp::Claude, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Custom);
        assert_eq!(
            state.live_snapshot.unwrap().model.as_deref(),
            Some("legacy-model-id")
        );
    }

    #[test]
    fn claude_activate_writes_per_size_model_envs_when_set() {
        // Routing GPT-5 to Sonnet, Claude-Opus to Opus, and DeepSeek
        // to Haiku — a non-trivial 3P setup. Confirm Termory writes
        // the three ANTHROPIC_DEFAULT_* env vars exactly as Claude
        // Code's `modelOptions.ts` expects.
        let _g = lock_home();
        let tmp = tempdir("claude-multi-model");
        let _home = override_home(&tmp);

        let mut p = make_provider(
            CliApp::Claude,
            "multi-route",
            "https://api.x.io",
            "sk-multi",
        );
        p.model = "gpt-5".into();
        // Per-size routing now flows through overrides — the
        // DEFAULT_{HAIKU,SONNET,OPUS}_MODEL keys are unmanaged, so they
        // pass straight through into env.
        p.options = vec![
            ProviderOption {
                key: "env.ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
                value: "gpt-5".into(),
            },
            ProviderOption {
                key: "env.ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
                value: "claude-opus-4-7".into(),
            },
            ProviderOption {
                key: "env.ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
                value: "deepseek-chat".into(),
            },
        ];
        activate(&p, &[p.clone()]).unwrap();

        let after: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_MODEL")
                .and_then(|v| v.as_str()),
            Some("gpt-5")
        );
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_DEFAULT_SONNET_MODEL")
                .and_then(|v| v.as_str()),
            Some("gpt-5")
        );
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_DEFAULT_OPUS_MODEL")
                .and_then(|v| v.as_str()),
            Some("claude-opus-4-7")
        );
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_DEFAULT_HAIKU_MODEL")
                .and_then(|v| v.as_str()),
            Some("deepseek-chat")
        );

        // Deactivate clears all four.
        deactivate(CliApp::Claude, &[p.clone()]).unwrap();
        let after2: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        for var in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
        ] {
            assert!(
                after2.pointer(&format!("/env/{var}")).is_none(),
                "{var} must be cleared after deactivate"
            );
        }
    }

    #[test]
    fn claude_switch_strips_other_providers_sub_model_overrides() {
        // Provider A routes Opus to a custom model via an override.
        // Switching to Provider B (which declares no such override) must
        // strip ANTHROPIC_DEFAULT_OPUS_MODEL — the override union cleans
        // up keys any known provider manages, even when the activated
        // provider doesn't set them.
        let _g = lock_home();
        let tmp = tempdir("claude-switch-strip");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let mut a = make_provider(CliApp::Claude, "route-opus", "https://a.example", "sk-a");
        a.model = "gpt-5".into();
        a.options = vec![ProviderOption {
            key: "env.ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
            value: "claude-opus-4-7".into(),
        }];
        let mut b = make_provider(CliApp::Claude, "plain", "https://b.example", "sk-b");
        b.model = "gpt-5-mini".into();
        let all = vec![a.clone(), b.clone()];

        activate(&a, &all).unwrap();
        let after_a: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            after_a
                .pointer("/env/ANTHROPIC_DEFAULT_OPUS_MODEL")
                .and_then(|v| v.as_str()),
            Some("claude-opus-4-7")
        );

        // Switch to B — its base_url/token replace A's, and A's opus
        // override is stripped via the union.
        activate(&b, &all).unwrap();
        let after_b: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert!(
            after_b
                .pointer("/env/ANTHROPIC_DEFAULT_OPUS_MODEL")
                .is_none(),
            "switching to a provider without the opus override must strip it"
        );
        assert_eq!(
            after_b
                .pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(|v| v.as_str()),
            Some("https://b.example")
        );
    }

    #[test]
    fn claude_unmanaged_when_external_edit_does_not_match_any_provider() {
        let _g = lock_home();
        let tmp = tempdir("claude-unmanaged");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        // Outside party set an unknown base URL.
        fs::write(
            tmp.join(".claude/settings.json"),
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://unknown.example", "ANTHROPIC_AUTH_TOKEN": "sk-unknown"}}"#,
        )
        .unwrap();
        let known = make_provider(CliApp::Claude, "Other", "https://api.known", "sk-known");
        let state = read_active_state(CliApp::Claude, &[known.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Unmanaged);
        assert!(state.matched_provider_id.is_none());
        let snap = state.live_snapshot.unwrap();
        assert_eq!(snap.base_url.as_deref(), Some("https://unknown.example"));
    }

    #[test]
    fn codex_activate_and_reverse_roundtrip_preserves_unrelated_blocks() {
        let _g = lock_home();
        let tmp = tempdir("codex-rt");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();
        // Pre-existing config with unrelated mcp_servers block + an
        // unrelated provider block.
        fs::write(
            tmp.join(".codex/config.toml"),
            r#"approval_policy = "untrusted"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com/v1"

[mcp_servers.context7]
command = "npx"
"#,
        )
        .unwrap();
        let p = make_provider(
            CliApp::Codex,
            "Custom-codex",
            "https://codex.x.io/v1",
            "sk-codex",
        );
        activate(&p, &[p.clone()]).unwrap();
        let txt = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
        // Unrelated stuff preserved.
        assert!(txt.contains("approval_policy"));
        assert!(txt.contains("mcp_servers.context7"));
        assert!(txt.contains("model_providers.openai"));
        // termory block written.
        assert!(txt.contains("[model_providers.termory]"));
        // ...with NO bare empty `[model_providers]` header above the blocks.
        assert!(!txt.contains("[model_providers]\n"));
        assert!(txt.contains("https://codex.x.io/v1"));

        let state = read_active_state(CliApp::Codex, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Custom);
        assert_eq!(
            state.matched_provider_id.as_deref(),
            Some("test-Custom-codex")
        );

        // config.toml: termory block has the verified shape
        // (wire_api=responses + requires_openai_auth=true, NO env_key).
        assert!(txt.contains(r#"wire_api = "responses""#));
        assert!(txt.contains("requires_openai_auth = true"));
        assert!(
            !txt.contains("env_key"),
            "env_key would force Codex to use env var only; we must not set it"
        );

        // Auth file: explicit auth_mode=apikey + OPENAI_API_KEY. We
        // do NOT null tokens/last_refresh (unlike official
        // login_with_api_key) — see merge rationale in activate_codex.
        let auth: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".codex/auth.json")).unwrap())
                .unwrap();
        assert_eq!(
            auth.get("auth_mode").and_then(|v| v.as_str()),
            Some("apikey")
        );
        assert_eq!(
            auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
            Some("sk-codex")
        );

        // Deactivate: model_provider removed, termory block removed,
        // unrelated openai block preserved, mcp_servers preserved.
        // auth.json is effectively empty → file deleted.
        deactivate(CliApp::Codex, &[p.clone()]).unwrap();
        let txt2 = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
        assert!(!txt2.contains("[model_providers.termory]"));
        assert!(txt2.contains("model_providers.openai"));
        assert!(txt2.contains("mcp_servers.context7"));
        assert!(!txt2.contains("model_provider ="));
        assert!(
            !tmp.join(".codex/auth.json").exists(),
            "auth.json should be removed when it contained only ApiKey-mode fields"
        );

        let state2 = read_active_state(CliApp::Codex, &[p.clone()]).unwrap();
        assert_eq!(state2.kind, ActiveKind::Official);
    }

    #[test]
    fn codex_switching_between_custom_providers_keeps_stable_model_provider_id() {
        // Codex's `codex resume` picker filters sessions by the active
        // `model_provider` (resume_picker.rs MatchDefault). If every
        // Termory custom provider used a distinct id, switching would
        // scope resume to the new id and hide the old provider's
        // history — the cc-switch complaint. Termory pins ALL custom
        // providers to the single stable id `TERMORY_PROVIDER_ID`, so
        // switching among them keeps `model_provider` constant and the
        // whole resume history stays visible. Lock that in here.
        let _g = lock_home();
        let tmp = tempdir("codex-stable-id");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        let read_model_provider = || {
            let txt = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
            let toml: toml::Value = toml::from_str(&txt).unwrap();
            toml.get("model_provider")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        // Two distinct custom providers (different endpoints/keys).
        let a = make_provider(CliApp::Codex, "Provider A", "https://a.x.io/v1", "sk-a");
        let b = make_provider(CliApp::Codex, "Provider B", "https://b.x.io/v1", "sk-b");

        activate(&a, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(read_model_provider().as_deref(), Some(TERMORY_PROVIDER_ID));
        assert!(fs::read_to_string(tmp.join(".codex/config.toml"))
            .unwrap()
            .contains("https://a.x.io/v1"));

        // Switch to B — model_provider must NOT change; only the
        // termory block's base_url is swapped.
        activate(&b, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(
            read_model_provider().as_deref(),
            Some(TERMORY_PROVIDER_ID),
            "switching custom providers must keep model_provider stable so codex resume keeps history"
        );
        let txt = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
        assert!(txt.contains("https://b.x.io/v1"));
        // Exactly one termory provider block — not one per provider.
        assert_eq!(txt.matches("[model_providers.termory]").count(), 1);
    }

    /// End-to-end guard for the interaction between the two features that
    /// share `~/.codex/auth.json`: with a custom provider active, switching
    /// the official ACCOUNT must leave the provider active.
    ///
    /// `read_active_codex` matches on config.toml's `model_provider` +
    /// `base_url` AND auth.json's `OPENAI_API_KEY` — so a switch that dropped
    /// the key (restoring a snapshot taken while Official was in use) left the
    /// state `Unmanaged`, which reads in the UI as the provider having been
    /// deactivated. Asserting the auth.json FIELDS alone (as accounts.rs does)
    /// would not have caught that, since the damage shows up in the derived
    /// state.
    #[tokio::test]
    async fn codex_account_switch_keeps_the_active_provider() {
        let _g = lock_home();
        let tmp = tempdir("codex-acct-switch");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        // A ChatGPT login is in place (account A), then account B. No
        // `refresh_token`: `switch_codex` always attempts a refresh first, and
        // one here would make this test hit the network (a real 401).
        let write_login = |account: &str| {
            fs::write(
                tmp.join(".codex/auth.json"),
                format!(
                    r#"{{
                      "auth_mode": "chatgpt",
                      "tokens": {{
                        "access_token": "at-{account}",
                        "account_id": "{account}"
                      }},
                      "last_refresh": "2025-01-01T00:00:00Z"
                    }}"#
                ),
            )
            .unwrap();
        };
        write_login("acct-a");
        crate::accounts::save_current_account(CliApp::Codex).unwrap();
        write_login("acct-b");
        crate::accounts::save_current_account(CliApp::Codex).unwrap();

        // Now activate a custom provider on top of the live login.
        let p = make_provider(CliApp::Codex, "gw", "https://gw.x.io/v1", "sk-gw");
        activate(&p, &[p.clone()]).unwrap();
        assert_eq!(
            read_active_state(CliApp::Codex, &[p.clone()]).unwrap().kind,
            ActiveKind::Custom,
            "precondition: the provider is active"
        );

        // Switch the official account. Both snapshots were taken BEFORE the
        // provider existed, so neither payload carries its key.
        crate::accounts::switch_account("acct-a".to_string())
            .await
            .unwrap();

        let state = read_active_state(CliApp::Codex, &[p.clone()]).unwrap();
        assert_eq!(
            state.kind,
            ActiveKind::Custom,
            "switching accounts must not deactivate the provider"
        );
        assert_eq!(state.matched_provider_id.as_deref(), Some("test-gw"));

        let auth: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".codex/auth.json")).unwrap())
                .unwrap();
        assert_eq!(
            auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
            Some("sk-gw"),
        );
        assert_eq!(
            auth.pointer("/tokens/account_id").and_then(|v| v.as_str()),
            Some("acct-a"),
            "…while the login really did switch"
        );
    }

    #[test]
    fn codex_oauth_login_then_activate_api_then_deactivate_keeps_oauth() {
        // Three-stage round-trip: user logged into ChatGPT, swaps to
        // a Custom API provider via Termory, then swaps back to
        // Official. The OAuth tokens must survive all three stages so
        // the user doesn't have to re-run `codex login`.
        let _g = lock_home();
        let tmp = tempdir("codex-three-stage");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        // Stage 1: user ran `codex login` — auth.json has OAuth tokens.
        fs::write(
            tmp.join(".codex/auth.json"),
            r#"{
              "auth_mode": "chatgpt",
              "OPENAI_API_KEY": null,
              "tokens": {
                "refresh_token": "rt-original",
                "access_token": "at-original",
                "id_token": "id-original",
                "account_id": "acc-1"
              },
              "last_refresh": "2025-01-01T00:00:00Z"
            }"#,
        )
        .unwrap();

        // Stage 2: activate Custom API provider via Termory.
        let p = make_provider(CliApp::Codex, "api-temp", "https://temp.api/v1", "sk-temp");
        activate(&p, &[p.clone()]).unwrap();

        let auth_after_activate: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".codex/auth.json")).unwrap())
                .unwrap();
        // Switched to apikey, BUT tokens still there.
        assert_eq!(
            auth_after_activate
                .get("auth_mode")
                .and_then(|v| v.as_str()),
            Some("apikey")
        );
        assert_eq!(
            auth_after_activate
                .get("OPENAI_API_KEY")
                .and_then(|v| v.as_str()),
            Some("sk-temp")
        );
        assert_eq!(
            auth_after_activate
                .pointer("/tokens/refresh_token")
                .and_then(|v| v.as_str()),
            Some("rt-original"),
            "OAuth refresh token must survive activate"
        );
        assert_eq!(
            auth_after_activate
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("at-original")
        );

        // Stage 3: deactivate → back to ChatGPT mode.
        deactivate(CliApp::Codex, &[p.clone()]).unwrap();

        let auth_after_deactivate: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".codex/auth.json")).unwrap())
                .unwrap();
        // ApiKey-mode fields cleared.
        assert!(
            auth_after_deactivate.get("OPENAI_API_KEY").is_none()
                || matches!(
                    auth_after_deactivate.get("OPENAI_API_KEY"),
                    Some(JsonValue::Null)
                )
        );
        // auth_mode removed → Codex resolved_mode() falls back to ChatGPT
        // because tokens is present.
        assert!(auth_after_deactivate.get("auth_mode").is_none());
        // OAuth tokens still intact — user does NOT have to log in again.
        assert_eq!(
            auth_after_deactivate
                .pointer("/tokens/refresh_token")
                .and_then(|v| v.as_str()),
            Some("rt-original")
        );
        assert_eq!(
            auth_after_deactivate
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("at-original")
        );
        assert_eq!(
            auth_after_deactivate
                .pointer("/tokens/account_id")
                .and_then(|v| v.as_str()),
            Some("acc-1")
        );
        assert_eq!(
            auth_after_deactivate
                .get("last_refresh")
                .and_then(|v| v.as_str()),
            Some("2025-01-01T00:00:00Z")
        );
    }

    #[test]
    fn codex_deactivate_preserves_existing_oauth_tokens() {
        let _g = lock_home();
        let tmp = tempdir("codex-preserve-oauth");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();
        // User previously ran `codex login` → OAuth tokens in auth.json.
        // Then activated a Termory Custom provider (auth_mode=apikey,
        // OPENAI_API_KEY set). Now deactivate → tokens MUST survive.
        fs::write(
            tmp.join(".codex/auth.json"),
            r#"{
              "auth_mode": "apikey",
              "OPENAI_API_KEY": "sk-temp",
              "tokens": { "refresh_token": "rt-keep", "access_token": "at-keep" },
              "last_refresh": "2025-01-01T00:00:00Z"
            }"#,
        )
        .unwrap();
        fs::write(
            tmp.join(".codex/config.toml"),
            r#"model_provider = "termory"

[model_providers.termory]
base_url = "https://x.io/v1"
"#,
        )
        .unwrap();

        deactivate(CliApp::Codex, &[]).unwrap();

        let auth: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".codex/auth.json")).unwrap())
                .unwrap();
        // ApiKey fields gone.
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert!(auth.get("auth_mode").is_none());
        // OAuth tokens preserved.
        assert_eq!(
            auth.pointer("/tokens/refresh_token")
                .and_then(|v| v.as_str()),
            Some("rt-keep")
        );
        assert_eq!(
            auth.pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("at-keep")
        );
    }

    #[test]
    fn codex_reverse_returns_official_when_model_provider_points_to_builtin() {
        let _g = lock_home();
        let tmp = tempdir("codex-builtin");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();
        fs::write(
            tmp.join(".codex/config.toml"),
            r#"model_provider = "openai"
"#,
        )
        .unwrap();
        let state = read_active_state(CliApp::Codex, &[]).unwrap();
        assert_eq!(state.kind, ActiveKind::Official);
    }

    #[test]
    fn grok_activate_roundtrip_preserves_config_and_auth() {
        let _g = lock_home();
        let tmp = tempdir("grok-rt");
        let _home = override_home(&tmp);
        let grok = tmp.join(".grok");
        fs::create_dir_all(&grok).unwrap();
        // Prior OAuth login + a FACTORY config: the official baseline has NO
        // `models.default` and NO `[model]` entries (verified on a real
        // install) — just grok's own settings.
        fs::write(
            grok.join("auth.json"),
            r#"{"https://auth.x.ai::abc":{"token":"secret"}}"#,
        )
        .unwrap();
        fs::write(grok.join("config.toml"), "[ui]\nyolo = false\n").unwrap();
        let auth_before = fs::read(grok.join("auth.json")).unwrap();

        // A grok provider carries a `models` LIST (required) + an optional
        // default (`model`, must be one of the models). id = "test-x-third".
        let mut p = make_provider(CliApp::Grok, "x-third", "https://gw.example/v1", "xai-sk");
        p.model = "grok-4.5".into();
        p.models = vec![
            ProviderModel {
                id: "grok-4.5".into(),
                name: String::new(),
            },
            ProviderModel {
                id: "grok-3".into(),
                name: "Grok 3".into(),
            },
        ];
        // Multi-slot: activate writes this provider's model entries but does
        // NOT set the startup default (that's a separate set_grok_default).
        activate(&p, &[p.clone()]).unwrap();
        let text = fs::read_to_string(grok.join("config.toml")).unwrap();
        // One flat entry per model, keyed by `<provider-id>-<model-id>` (a
        // key with a `.` like grok-4.5 gets TOML-quoted, so match the key
        // substring rather than the exact header form).
        assert!(text.contains("test-x-third-grok-4.5"));
        assert!(text.contains("test-x-third-grok-3"));
        // ...and NO bare empty `[model]` header above the sub-tables.
        assert!(!text.contains("[model]\n"));
        // `model` field = upstream id; `name` = display (id when blank, else
        // the model's name); `description` = provider name.
        assert!(text.contains("model = \"grok-4.5\""));
        assert!(text.contains("model = \"grok-3\""));
        assert!(text.contains("name = \"grok-4.5\""));
        assert!(text.contains("name = \"Grok 3\""));
        assert!(text.contains("description = \"x-third\""));
        assert!(text.contains("api_key = \"xai-sk\""));
        // Unset api_backend is OMITTED — grok applies its own default
        // (chat_completions per `ApiBackend::default()`); only an explicit
        // editor choice is written.
        assert!(!text.contains("api_backend"));
        // Activate alone does NOT set the default (multi-slot split).
        assert!(!text.contains("default ="));
        assert!(!text.contains("termory"));
        assert!(text.contains("yolo = false"));

        // Promote to grok's startup default separately.
        set_grok_default(&p, &[p.clone()]).unwrap();
        let text_def = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(text_def.contains("default = \"test-x-third-grok-4.5\""));

        let state = read_active_state(CliApp::Grok, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Custom);
        assert_eq!(state.matched_provider_id.as_deref(), Some("test-x-third"));
        assert_eq!(
            state
                .live_snapshot
                .as_ref()
                .and_then(|s| s.model.as_deref()),
            Some("grok-4.5")
        );

        // An EXPLICIT api_backend choice IS written into every entry.
        let mut p_explicit = p.clone();
        p_explicit.api_backend = Some("responses".into());
        activate(&p_explicit, &[p_explicit.clone()]).unwrap();
        let text_explicit = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(text_explicit.contains("api_backend = \"responses\""));

        // "Set Official" clears the default but KEEPS the slot's entries
        // (multi-slot — they stay selectable in grok's picker).
        deactivate(CliApp::Grok, &[p.clone()]).unwrap();
        let text2 = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(text2.contains("test-x-third-grok-4.5"), "slot entries stay");
        assert!(!text2.contains("default ="), "default cleared");
        assert!(text2.contains("yolo = false"));
        let state2 = read_active_state(CliApp::Grok, &[p.clone()]).unwrap();
        assert_eq!(state2.kind, ActiveKind::Official);
        // ...but the slot is still exposed as configured.
        assert_eq!(
            state2.configured_provider_ids,
            vec!["test-x-third".to_string()]
        );

        // Deleting the provider removes its entries entirely → factory shape.
        delete_provider_traces(&p).unwrap();
        let text3 = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(!text3.contains("test-x-third"));
        assert!(!text3.contains("[model]"));
        assert!(text3.contains("yolo = false"));

        // The OAuth session file is byte-identical across the round-trip.
        assert_eq!(fs::read(grok.join("auth.json")).unwrap(), auth_before);
    }

    #[test]
    fn grok_multi_slot_providers_coexist_and_default_picks_one() {
        let _g = lock_home();
        let tmp = tempdir("grok-multi-slot");
        let _home = override_home(&tmp);
        let grok = tmp.join(".grok");
        fs::create_dir_all(&grok).unwrap();
        fs::write(grok.join("config.toml"), "[ui]\nyolo = false\n").unwrap();

        let mut a = make_provider(CliApp::Grok, "aaa", "https://a.example/v1", "key-a");
        a.model = "grok-4.5".into();
        a.models = vec![ProviderModel {
            id: "grok-4.5".into(),
            name: String::new(),
        }];
        let mut b = make_provider(CliApp::Grok, "bbb", "https://b.example/v1", "key-b");
        b.model = "grok-3".into();
        b.models = vec![ProviderModel {
            id: "grok-3".into(),
            name: String::new(),
        }];
        let all = [a.clone(), b.clone()];

        // Multi-slot: enabling BOTH keeps both providers' entries in config.
        activate(&a, &all).unwrap();
        activate(&b, &all).unwrap();
        let text = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(text.contains("test-aaa-grok-4.5"), "A's entry coexists");
        assert!(text.contains("test-bbb-grok-3"), "B's entry coexists");
        // Neither is the default until set explicitly.
        assert!(!text.contains("default ="));

        // Both show as configured; none in use yet.
        let state0 = read_active_state(CliApp::Grok, &all).unwrap();
        assert_eq!(state0.kind, ActiveKind::Official);
        let mut ids = state0.configured_provider_ids.clone();
        ids.sort();
        assert_eq!(ids, vec!["test-aaa".to_string(), "test-bbb".to_string()]);

        // Set B as default → B is in use, A still enabled.
        set_grok_default(&b, &all).unwrap();
        let text2 = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(text2.contains("default = \"test-bbb-grok-3\""));
        assert!(text2.contains("test-aaa-grok-4.5"), "A's slot survives");
        let state = read_active_state(CliApp::Grok, &all).unwrap();
        assert_eq!(state.matched_provider_id.as_deref(), Some("test-bbb"));

        // Deleting A removes ONLY A's entries; B (the default) is untouched.
        delete_provider_traces(&a).unwrap();
        let text3 = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(!text3.contains("test-aaa"));
        assert!(text3.contains("test-bbb-grok-3"));
        assert!(text3.contains("default = \"test-bbb-grok-3\""));

        // Duplicate model ids in ONE provider's list are still rejected.
        let mut dup = make_provider(CliApp::Grok, "dup", "https://d.example/v1", "key-d");
        dup.model = String::new();
        dup.models = vec![
            ProviderModel {
                id: "grok-4.5".into(),
                name: String::new(),
            },
            ProviderModel {
                id: "grok-4.5".into(),
                name: "again".into(),
            },
        ];
        assert!(activate(&dup, &[dup.clone()]).is_err());
    }

    #[test]
    fn grok_set_default_falls_back_to_first_model_when_no_default() {
        let _g = lock_home();
        let tmp = tempdir("grok-default-fallback");
        let _home = override_home(&tmp);
        let grok = tmp.join(".grok");
        fs::create_dir_all(&grok).unwrap();

        let mut p = make_provider(CliApp::Grok, "nd", "https://x.example/v1", "key-x");
        p.model = String::new(); // no explicit default
        p.models = vec![
            ProviderModel {
                id: "grok-4.5".into(),
                name: String::new(),
            },
            ProviderModel {
                id: "grok-3".into(),
                name: String::new(),
            },
        ];
        activate(&p, &[p.clone()]).unwrap();
        // No default set yet → set_grok_default promotes the FIRST listed.
        set_grok_default(&p, &[p.clone()]).unwrap();
        let text = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(text.contains("default = \"test-nd-grok-4.5\""));

        // set_grok_default on a provider with no activated entries → error.
        let other = make_provider(CliApp::Grok, "gone", "https://y.example/v1", "key-y");
        assert!(set_grok_default(&other, &[other.clone()]).is_err());
    }

    #[test]
    fn grok_options_applied_on_set_default_and_stripped_on_switch() {
        // Grok's Advanced settings are GLOBAL config.toml keys, applied ONLY
        // when the provider is the default (set_grok_default) — NOT on enable —
        // and stripped on "Set Official". A managed key (models.default) is
        // ignored.
        let _g = lock_home();
        let tmp = tempdir("grok-global-opts");
        let _home = override_home(&tmp);
        let grok = tmp.join(".grok");
        fs::create_dir_all(&grok).unwrap();
        fs::write(grok.join("config.toml"), "[ui]\nyolo = false\n").unwrap();

        let mut p = make_provider(CliApp::Grok, "g", "https://x.example/v1", "key-x");
        p.model = "grok-4.5".into();
        p.models = vec![ProviderModel {
            id: "grok-4.5".into(),
            name: String::new(),
        }];
        p.options = vec![
            ProviderOption {
                key: "ui.compact_mode".into(),
                value: "true".into(),
            },
            ProviderOption {
                key: "models.temperature".into(),
                value: "0.7".into(),
            },
            // managed → ignored (Termory owns the default pointer)
            ProviderOption {
                key: "models.default".into(),
                value: "hacked".into(),
            },
        ];

        // Enable: entries written, options NOT applied yet.
        activate(&p, &[p.clone()]).unwrap();
        let after_enable = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(!after_enable.contains("compact_mode = true"));
        assert!(!after_enable.contains("temperature"));

        // Set default: options now applied to config.toml top-level.
        set_grok_default(&p, &[p.clone()]).unwrap();
        let after_default = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(after_default.contains("compact_mode = true"));
        assert!(after_default.contains("temperature = 0.7"));
        // managed key ignored: models.default points at the model entry, not "hacked".
        assert!(after_default.contains("default = \"test-g-grok-4.5\""));
        assert!(!after_default.contains("hacked"));

        // Set Official: options stripped, slot entries stay.
        deactivate(CliApp::Grok, &[p.clone()]).unwrap();
        let after_official = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(!after_official.contains("compact_mode"));
        assert!(!after_official.contains("temperature"));
        assert!(
            after_official.contains("g-grok-4.5"),
            "slot entries survive"
        );
        assert!(
            after_official.contains("yolo = false"),
            "user config untouched"
        );
    }

    #[test]
    fn grok_switch_default_strips_previous_default_options() {
        // Switching the default from A to B strips A's global Advanced settings
        // and applies B's — only the CURRENT default provider's options are live.
        let _g = lock_home();
        let tmp = tempdir("grok-switch-opts");
        let _home = override_home(&tmp);
        let grok = tmp.join(".grok");
        fs::create_dir_all(&grok).unwrap();

        let mut a = make_provider(CliApp::Grok, "aaa", "https://a.example/v1", "key-a");
        a.model = "grok-4.5".into();
        a.models = vec![ProviderModel {
            id: "grok-4.5".into(),
            name: String::new(),
        }];
        a.options = vec![ProviderOption {
            key: "ui.compact_mode".into(),
            value: "true".into(),
        }];

        let mut b = make_provider(CliApp::Grok, "bbb", "https://b.example/v1", "key-b");
        b.model = "grok-3".into();
        b.models = vec![ProviderModel {
            id: "grok-3".into(),
            name: String::new(),
        }];
        b.options = vec![ProviderOption {
            key: "models.temperature".into(),
            value: "0.7".into(),
        }];

        let all = [a.clone(), b.clone()];
        activate(&a, &all).unwrap();
        set_grok_default(&a, &all).unwrap();
        let after_a = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(
            after_a.contains("compact_mode = true"),
            "A's options applied"
        );

        // Switch the default to B.
        activate(&b, &all).unwrap();
        set_grok_default(&b, &all).unwrap();
        let after_b = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(!after_b.contains("compact_mode"), "A's options stripped");
        assert!(after_b.contains("temperature = 0.7"), "B's options applied");
        assert!(after_b.contains("default = \"test-bbb-grok-3\""));
    }

    #[test]
    fn grok_read_active_matches_grok_provider_not_a_same_creds_other_cli() {
        let _g = lock_home();
        let tmp = tempdir("grok-cross-app");
        let _home = override_home(&tmp);
        let grok = tmp.join(".grok");
        fs::create_dir_all(&grok).unwrap();

        // The SAME gateway key reused across CLIs (common). Activate the grok
        // provider; read_active must return the GROK provider's id — a codex
        // provider sharing the creds (listed FIRST) must not be matched, or
        // the frontend's `isLive` check fails and editing never re-applies.
        let mut g = make_provider(CliApp::Grok, "g", "https://gw.example/v1", "shared-key");
        g.model = "grok-4.5".into();
        g.models = vec![ProviderModel {
            id: "grok-4.5".into(),
            name: String::new(),
        }];
        let codex_same = make_provider(CliApp::Codex, "c", "https://gw.example/v1", "shared-key");

        activate(&g, &[g.clone()]).unwrap();
        set_grok_default(&g, &[g.clone()]).unwrap();
        // Codex provider listed BEFORE the grok one (would win without the
        // app filter).
        let state = read_active_state(CliApp::Grok, &[codex_same, g.clone()]).unwrap();
        assert_eq!(state.matched_provider_id.as_deref(), Some("test-g"));
    }

    #[test]
    fn gemini_activate_writes_dotenv_and_reverses_with_0600() {
        let _g = lock_home();
        let tmp = tempdir("gemini-rt");
        let _home = override_home(&tmp);
        let p = make_provider(CliApp::Gemini, "g-third", "https://g.example", "g-sk");
        activate(&p, &[p.clone()]).unwrap();
        let env_text = fs::read_to_string(tmp.join(".gemini/.env")).unwrap();
        assert!(env_text.contains("GOOGLE_GEMINI_BASE_URL=https://g.example"));
        assert!(env_text.contains("GEMINI_API_KEY=g-sk"));
        // make_provider sets model="test-model" → GEMINI_MODEL must
        // also be written, matching cc-switch's preset shape.
        assert!(env_text.contains("GEMINI_MODEL=test-model"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(tmp.join(".gemini/.env"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, ".env must be 0600");
        }
        let state = read_active_state(CliApp::Gemini, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Custom);
        assert_eq!(state.matched_provider_id.as_deref(), Some("test-g-third"));
        assert_eq!(
            state
                .live_snapshot
                .as_ref()
                .and_then(|s| s.model.as_deref()),
            Some("test-model")
        );

        deactivate(CliApp::Gemini, &[p.clone()]).unwrap();
        let env_text2 = fs::read_to_string(tmp.join(".gemini/.env")).unwrap();
        // All three Termory-managed env vars cleared.
        for var in ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY", "GEMINI_MODEL"] {
            assert!(
                !env_text2.contains(&format!("{var}=")),
                "{var} must be cleared after deactivate"
            );
        }
        let state2 = read_active_state(CliApp::Gemini, &[p.clone()]).unwrap();
        assert_eq!(state2.kind, ActiveKind::Official);
    }

    #[test]
    fn gemini_activate_preserves_unrelated_env_vars() {
        // User's `~/.gemini/.env` may already contain other variables
        // (DEBUG_MODE, custom tooling, etc.). We must merge — never
        // overwrite — and only touch the three Termory-managed keys.
        let _g = lock_home();
        let tmp = tempdir("gemini-preserve");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".gemini")).unwrap();
        fs::write(
            tmp.join(".gemini/.env"),
            "DEBUG=true\nMY_TOOL_PATH=/opt/x\nGEMINI_MODEL=stale-model\n",
        )
        .unwrap();

        let p = make_provider(CliApp::Gemini, "g", "https://g.example", "g-sk");
        activate(&p, &[p.clone()]).unwrap();

        let env_text = fs::read_to_string(tmp.join(".gemini/.env")).unwrap();
        assert!(
            env_text.contains("DEBUG=true"),
            "unrelated DEBUG var must survive"
        );
        assert!(
            env_text.contains("MY_TOOL_PATH=/opt/x"),
            "unrelated MY_TOOL_PATH must survive"
        );
        assert!(env_text.contains("GEMINI_MODEL=test-model"));
        assert!(!env_text.contains("stale-model"));
    }

    #[test]
    fn gemini_oauth_credentials_survive_activate_deactivate_cycle() {
        // `gemini auth` persists OAuth tokens to oauth_creds.json (see
        // `core/src/config/storage.ts:22`). Termory only writes `.env`,
        // so activate → deactivate must leave the credentials file
        // byte-identical and the user stays logged in.
        let _g = lock_home();
        let tmp = tempdir("gemini-oauth-keep");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".gemini")).unwrap();
        let creds_contents = r#"{
          "access_token": "at-original",
          "refresh_token": "rt-original",
          "expiry_date": 9999999999000
        }"#;
        fs::write(tmp.join(".gemini/oauth_creds.json"), creds_contents).unwrap();
        let accounts_contents = r#"{ "active": "user@example.com" }"#;
        fs::write(tmp.join(".gemini/google_accounts.json"), accounts_contents).unwrap();

        let p = make_provider(CliApp::Gemini, "g-temp", "https://temp.g", "g-temp");
        activate(&p, &[p.clone()]).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.join(".gemini/oauth_creds.json")).unwrap(),
            creds_contents,
            "oauth_creds.json must survive activate byte-for-byte"
        );
        assert_eq!(
            fs::read_to_string(tmp.join(".gemini/google_accounts.json")).unwrap(),
            accounts_contents,
        );

        deactivate(CliApp::Gemini, &[p.clone()]).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.join(".gemini/oauth_creds.json")).unwrap(),
            creds_contents,
            "oauth_creds.json must survive deactivate byte-for-byte"
        );
        assert_eq!(
            fs::read_to_string(tmp.join(".gemini/google_accounts.json")).unwrap(),
            accounts_contents,
        );
    }

    #[test]
    fn opencode_activate_writes_full_provider_block_in_opencode_json() {
        // cc-switch mode: everything Termory writes lives in
        // ~/.config/opencode/opencode.json. auth.json is never touched.
        let _g = lock_home();
        let tmp = tempdir("opencode-cc-mode");
        let _home = override_home(&tmp);

        let mut p = make_provider(
            CliApp::Opencode,
            "packycode",
            "https://api.packy.example",
            "sk-packy",
        );
        // Grok-style shape: the default (`model`) is ONE OF the listed
        // models, and the list is what populates OpenCode's picker.
        p.model = "claude-opus-4-7".into();
        p.npm = Some("@ai-sdk/anthropic".into());
        p.models = vec![
            ProviderModel {
                id: "claude-opus-4-7".into(),
                ..Default::default()
            },
            ProviderModel {
                id: "claude-sonnet-4-5".into(),
                ..Default::default()
            },
            ProviderModel {
                id: "claude-haiku-4-5".into(),
                ..Default::default()
            },
        ];
        activate(&p, &[p.clone()]).unwrap();

        // auth.json must NOT be created — that file is reserved for /connect.
        assert!(
            !tmp.join(".local/share/opencode/auth.json").exists(),
            "auth.json must not be created in cc-switch mode"
        );

        let termory_id = format!("termory-{}", p.id);
        let config_path = tmp.join(".config/opencode/opencode.json");
        let config: JsonValue =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let block_ptr = format!("/provider/{termory_id}");
        assert_eq!(
            config
                .pointer(&format!("{block_ptr}/name"))
                .and_then(|v| v.as_str()),
            Some("packycode")
        );
        assert_eq!(
            config
                .pointer(&format!("{block_ptr}/npm"))
                .and_then(|v| v.as_str()),
            Some("@ai-sdk/anthropic")
        );
        assert_eq!(
            config
                .pointer(&format!("{block_ptr}/options/baseURL"))
                .and_then(|v| v.as_str()),
            Some("https://api.packy.example")
        );
        assert_eq!(
            config
                .pointer(&format!("{block_ptr}/options/apiKey"))
                .and_then(|v| v.as_str()),
            Some("sk-packy")
        );
        // models map is built purely from the LIST, each as {name: "<id>"}.
        for m in ["claude-opus-4-7", "claude-sonnet-4-5", "claude-haiku-4-5"] {
            assert_eq!(
                config
                    .pointer(&format!("{block_ptr}/models/{m}/name"))
                    .and_then(|v| v.as_str()),
                Some(m)
            );
        }
        // Map order == the `models` LIST order (list[0] first) — the default
        // `model` is NOT injected/reordered anymore, it's just the default
        // pointer. Here the list starts with opus. Relies on serde_json's
        // preserve_order feature keeping insertion order.
        assert_eq!(
            config
                .pointer(&format!("{block_ptr}/models"))
                .and_then(|v| v.as_object())
                .and_then(|m| m.keys().next())
                .map(String::as_str),
            Some("claude-opus-4-7")
        );
        // Activate alone does NOT set as default — top-level `model`
        // is untouched.
        assert!(config.get("model").is_none());

        // Activated provider shows up in configured_provider_ids but
        // kind stays Official until set_opencode_default is called.
        let state = read_active_state(CliApp::Opencode, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Official);
        assert_eq!(state.configured_provider_ids, vec![p.id.clone()]);

        // After explicit set_default, kind flips to Custom.
        set_opencode_default(&p).unwrap();
        let state2 = read_active_state(CliApp::Opencode, &[p.clone()]).unwrap();
        assert_eq!(state2.kind, ActiveKind::Custom);
        assert_eq!(state2.matched_provider_id.as_deref(), Some(p.id.as_str()));
        let config2: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config2.get("model").and_then(|v| v.as_str()),
            Some(format!("{termory_id}/claude-opus-4-7").as_str())
        );
    }

    #[test]
    fn opencode_activate_dedupes_primary_in_models() {
        let _g = lock_home();
        let tmp = tempdir("opencode-dedup");
        let _home = override_home(&tmp);

        let mut p = make_provider(
            CliApp::Opencode,
            "dedup",
            "https://api.example.com",
            "sk-dedup",
        );
        p.model = "gpt-5".into();
        // Primary repeated + an actual extra (with a custom display name).
        p.models = vec![
            ProviderModel {
                id: "gpt-5".into(),
                ..Default::default()
            },
            ProviderModel {
                id: "gpt-5-mini".into(),
                name: "GPT-5 Mini".into(),
            },
        ];
        activate(&p, &[p.clone()]).unwrap();

        let termory_id = format!("termory-{}", p.id);
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        let models = config
            .pointer(&format!("/provider/{termory_id}/models"))
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(models.len(), 2);
        assert!(models.contains_key("gpt-5"));
        assert!(models.contains_key("gpt-5-mini"));
        // Primary's name defaults to its id; the extra uses its label.
        assert_eq!(
            config
                .pointer(&format!("/provider/{termory_id}/models/gpt-5/name"))
                .and_then(|v| v.as_str()),
            Some("gpt-5")
        );
        assert_eq!(
            config
                .pointer(&format!("/provider/{termory_id}/models/gpt-5-mini/name"))
                .and_then(|v| v.as_str()),
            Some("GPT-5 Mini")
        );
    }

    #[test]
    fn opencode_models_map_follows_list_order_not_default() {
        // The default `model` is only the default POINTER — it must NOT be
        // reordered to the front of the models map. Map order == list order.
        let _g = lock_home();
        let tmp = tempdir("opencode-list-order");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "order", "https://x.io", "sk-x");
        // Default is the SECOND list entry — it must still land second.
        p.model = "b".into();
        p.models = vec![
            ProviderModel {
                id: "a".into(),
                name: String::new(),
            },
            ProviderModel {
                id: "b".into(),
                name: String::new(),
            },
            ProviderModel {
                id: "c".into(),
                name: String::new(),
            },
        ];
        activate(&p, &[p.clone()]).unwrap();
        let termory_id = format!("termory-{}", p.id);
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        let keys: Vec<&str> = config
            .pointer(&format!("/provider/{termory_id}/models"))
            .and_then(|v| v.as_object())
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["a", "b", "c"], "map keeps list order");
    }

    #[test]
    fn opencode_model_only_binding_writes_single_model() {
        // A gateway binding may carry only `model` with no list — the
        // fallback writes that single model so the slot isn't empty.
        let _g = lock_home();
        let tmp = tempdir("opencode-model-only");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "mo", "https://x.io", "sk-x");
        p.model = "solo".into();
        p.models = Vec::new();
        activate(&p, &[p.clone()]).unwrap();
        let termory_id = format!("termory-{}", p.id);
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(config
            .pointer(&format!("/provider/{termory_id}/models/solo"))
            .is_some());
    }

    #[test]
    fn opencode_options_nest_under_provider_block_and_skip_managed() {
        let _g = lock_home();
        let tmp = tempdir("opencode-options");
        let _home = override_home(&tmp);

        let mut p = make_provider(CliApp::Opencode, "p", "https://api.x.io", "sk-p");
        p.model = "m1".into();
        p.options = vec![
            // → number, nested inside the provider's options bag
            ProviderOption {
                key: "timeout".into(),
                value: "600000".into(),
            },
            // → nested string under options.headers
            ProviderOption {
                key: "headers.X-Token".into(),
                value: "abc".into(),
            },
            // managed (set by the dedicated API key field) → ignored
            ProviderOption {
                key: "apiKey".into(),
                value: "evil".into(),
            },
        ];
        let read = || -> JsonValue {
            serde_json::from_str(
                &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
            )
            .unwrap()
        };
        activate(&p, &[p.clone()]).unwrap();
        let id = format!("termory-{}", p.id);
        let cfg = read();
        assert_eq!(
            cfg.pointer(&format!("/provider/{id}/options/timeout"))
                .and_then(|v| v.as_i64()),
            Some(600000)
        );
        assert_eq!(
            cfg.pointer(&format!("/provider/{id}/options/headers/X-Token"))
                .and_then(|v| v.as_str()),
            Some("abc")
        );
        // apiKey stays the dedicated field's value, not the override.
        assert_eq!(
            cfg.pointer(&format!("/provider/{id}/options/apiKey"))
                .and_then(|v| v.as_str()),
            Some("sk-p")
        );
        // Nothing leaked to the top level.
        assert!(cfg.get("timeout").is_none());

        // Removing the options and re-enabling drops them (block rebuilt),
        // while baseURL/apiKey from the dedicated fields survive.
        p.options = vec![];
        activate(&p, &[p.clone()]).unwrap();
        let cfg2 = read();
        assert!(cfg2
            .pointer(&format!("/provider/{id}/options/timeout"))
            .is_none());
        assert_eq!(
            cfg2.pointer(&format!("/provider/{id}/options/baseURL"))
                .and_then(|v| v.as_str()),
            Some("https://api.x.io")
        );
    }

    #[test]
    fn opencode_enabling_one_provider_keeps_siblings_options() {
        // Core multi-slot guarantee: enabling B must not wipe A's options,
        // because each provider's options live inside its own block.
        let _g = lock_home();
        let tmp = tempdir("opencode-sibling-options");
        let _home = override_home(&tmp);

        let mut a = make_provider(CliApp::Opencode, "a", "https://a.io", "sk-a");
        a.id = "aaa".into();
        a.model = "ma".into();
        a.options = vec![ProviderOption {
            key: "timeout".into(),
            value: "111".into(),
        }];
        let mut b = make_provider(CliApp::Opencode, "b", "https://b.io", "sk-b");
        b.id = "bbb".into();
        b.model = "mb".into();
        b.options = vec![ProviderOption {
            key: "timeout".into(),
            value: "222".into(),
        }];
        let all = vec![a.clone(), b.clone()];
        activate(&a, &all).unwrap();
        activate(&b, &all).unwrap();

        let cfg: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        // A's option survived B being enabled.
        assert_eq!(
            cfg.pointer("/provider/termory-aaa/options/timeout")
                .and_then(|v| v.as_i64()),
            Some(111)
        );
        assert_eq!(
            cfg.pointer("/provider/termory-bbb/options/timeout")
                .and_then(|v| v.as_i64()),
            Some(222)
        );
    }

    #[test]
    fn opencode_set_default_picks_one_among_multi_activated() {
        // A and B both activated → both slots in opencode.json,
        // but only the one passed to set_opencode_default ends up as
        // the top-level model. Switching default just overwrites the
        // top-level field, slots stay put.
        let _g = lock_home();
        let tmp = tempdir("opencode-set-default");
        let _home = override_home(&tmp);

        let mut a = make_provider(CliApp::Opencode, "a", "https://api.example.com", "sk-a");
        a.id = "aaa".into();
        a.model = "model-a".into();
        let mut b = make_provider(CliApp::Opencode, "b", "https://api.example.com", "sk-b");
        b.id = "bbb".into();
        b.model = "model-b".into();

        activate(&a, &[a.clone(), b.clone()]).unwrap();
        activate(&b, &[a.clone(), b.clone()]).unwrap();

        // No top-level model yet — neither was set as default.
        let config0: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(config0.pointer("/provider/termory-aaa").is_some());
        assert!(config0.pointer("/provider/termory-bbb").is_some());
        assert!(config0.get("model").is_none());

        let state0 = read_active_state(CliApp::Opencode, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(state0.kind, ActiveKind::Official);
        let mut configured = state0.configured_provider_ids.clone();
        configured.sort();
        assert_eq!(configured, vec!["aaa".to_string(), "bbb".to_string()]);

        // Set A as default.
        set_opencode_default(&a).unwrap();
        let state1 = read_active_state(CliApp::Opencode, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(state1.kind, ActiveKind::Custom);
        assert_eq!(state1.matched_provider_id.as_deref(), Some("aaa"));

        // Switch default to B — A's slot stays.
        set_opencode_default(&b).unwrap();
        let config2: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(config2.pointer("/provider/termory-aaa").is_some());
        assert_eq!(
            config2.get("model").and_then(|v| v.as_str()),
            Some("termory-bbb/model-b")
        );
        let state2 = read_active_state(CliApp::Opencode, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(state2.matched_provider_id.as_deref(), Some("bbb"));
    }

    #[test]
    fn opencode_set_default_rejects_inactive_provider() {
        let _g = lock_home();
        let tmp = tempdir("opencode-default-rejects");
        let _home = override_home(&tmp);
        let p = make_provider(CliApp::Opencode, "p", "https://api.example.com", "sk-p");
        // Never activated.
        let result = set_opencode_default(&p);
        assert!(result.is_err());
        assert!(!tmp.join(".config/opencode/opencode.json").exists());
    }

    #[test]
    fn opencode_activate_rejects_when_no_model_at_all() {
        // OpenCode is multi-model now: a blank default is fine, but with NO
        // default AND an EMPTY models list there's nothing to put in the
        // picker, so activation must still fail cleanly.
        let _g = lock_home();
        let tmp = tempdir("opencode-no-model");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "no-model", "https://x.io", "sk-x");
        p.model = String::new();
        p.models = Vec::new();
        let result = activate(&p, &[p.clone()]);
        assert!(result.is_err());
        assert!(
            !tmp.join(".config/opencode/opencode.json").exists(),
            "no partial opencode.json on model-missing failure"
        );
    }

    #[test]
    fn opencode_activate_allows_models_list_without_a_default() {
        // The new grok-style shape: a REQUIRED models list, OPTIONAL default.
        // With a list but blank `model`, activation writes every model into
        // the picker and no error.
        let _g = lock_home();
        let tmp = tempdir("opencode-list-no-default");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "list-only", "https://x.io", "sk-x");
        p.model = String::new();
        p.models = vec![
            ProviderModel {
                id: "gpt-5".into(),
                name: "GPT-5".into(),
            },
            ProviderModel {
                id: "gpt-5-mini".into(),
                name: String::new(),
            },
        ];
        activate(&p, &[p.clone()]).unwrap();

        let termory_id = format!("termory-{}", p.id);
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(
            config
                .pointer(&format!("/provider/{termory_id}/models/gpt-5"))
                .is_some(),
            "listed model surfaces in the picker"
        );
        assert!(config
            .pointer(&format!("/provider/{termory_id}/models/gpt-5-mini"))
            .is_some());
        // No default chosen → set_opencode_default falls back to the first
        // listed model rather than erroring.
        set_opencode_default(&p).unwrap();
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config.get("model").and_then(|v| v.as_str()),
            Some(format!("{termory_id}/gpt-5").as_str()),
            "blank default promotes the first listed model"
        );
    }

    #[test]
    fn opencode_activate_rejects_default_not_in_models_list() {
        // When both a default and a non-empty list are present, the default
        // must be one of them (mirrors grok).
        let _g = lock_home();
        let tmp = tempdir("opencode-bad-default");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "bad-default", "https://x.io", "sk-x");
        p.model = "not-listed".into();
        p.models = vec![ProviderModel {
            id: "gpt-5".into(),
            name: String::new(),
        }];
        assert!(activate(&p, &[p.clone()]).is_err());
        assert!(!tmp.join(".config/opencode/opencode.json").exists());
    }

    #[test]
    fn reject_duplicate_model_ids_flags_repeats_ignores_blanks() {
        let m = |id: &str, name: &str| ProviderModel {
            id: id.into(),
            name: name.into(),
        };
        // Unique + blanks → ok.
        assert!(reject_duplicate_model_ids(&[m("a", ""), m("b", ""), m("  ", "")], "X").is_ok());
        // A repeated id (whitespace-normalized) → error.
        assert!(reject_duplicate_model_ids(&[m("a", ""), m(" a ", "x")], "X").is_err());
    }

    #[test]
    fn providers_from_json_skips_unrecognized_entries_keeps_the_rest() {
        // The downgrade case: an OLDER binary reads a providers.json that a
        // NEWER version wrote, containing a provider for an app this build has
        // no feature for (`future-cli` stands in for what `grok` WAS to a
        // pre-grok build — using a real-but-newer name here would be a known
        // variant and wouldn't exercise the skip) plus a corrupt row. The
        // unknown/corrupt entries are skipped (absent from this version's UI);
        // the entries this build DOES know survive. One bad entry must NEVER
        // empty or fail the whole list.
        let value = serde_json::json!([
            { "id": "a", "app": "claude",     "kind": "custom", "name": "A" },
            { "id": "b", "app": "future-cli", "kind": "custom", "name": "unknown app" },
            { "id": "c", "app": "codex",      "kind": "custom", "name": "C" },
            { "id": "d", "app": 12345,        "kind": "custom", "name": "corrupt app type" },
        ]);
        let parsed = providers_from_json(value.clone());
        let ids: Vec<&str> = parsed.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"], "keep only what this build understands");

        // The Tauri-arg newtype deserializes with the same tolerance, so an IPC
        // call carrying an unknown provider doesn't fail at arg-binding.
        let list: ProviderList = serde_json::from_value(value).unwrap();
        assert_eq!(list.0.len(), 2);
    }

    #[test]
    fn gateway_with_unknown_binding_survives_keeping_known_bindings() {
        // A gateway whose bindings include one for an app this build doesn't
        // know (`future-cli` stands in for what `grok` was to a pre-grok
        // build) must still load — the gateway and its recognized bindings
        // survive; only the unknown binding is dropped (its feature is just
        // absent from this version's UI). It must NOT take down the whole
        // gateway.
        let value = serde_json::json!([{
            "kind": "gateway", "id": "gw1", "name": "GW",
            "baseUrl": "https://gw.example", "apiKey": "sk-x",
            "bindings": [
                { "id": "x", "app": "claude" },
                { "id": "y", "app": "future-cli" },
                { "id": "z", "app": "codex" },
            ],
        }]);
        let gateways = gateways_from_json(value);
        assert_eq!(gateways.len(), 1, "the gateway itself survives");
        let apps: Vec<CliApp> = gateways[0].bindings.iter().map(|b| b.app).collect();
        assert_eq!(
            apps,
            vec![CliApp::Claude, CliApp::Codex],
            "unknown-app binding dropped, known ones kept"
        );
    }

    #[test]
    fn opencode_activate_rejects_duplicate_models() {
        let _g = lock_home();
        let tmp = tempdir("opencode-dup");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "dup", "https://x.io", "sk-x");
        // TWO identical entries in the LIST → a real duplicate, rejected.
        p.models = vec![
            ProviderModel {
                id: "gpt-5".into(),
                name: String::new(),
            },
            ProviderModel {
                id: "gpt-5".into(),
                name: "again".into(),
            },
        ];
        assert!(activate(&p, &[p.clone()]).is_err());
        assert!(!tmp.join(".config/opencode/opencode.json").exists());
    }

    #[test]
    fn opencode_dedup_allows_primary_redeclared_in_list() {
        // BOUNDARY: the dedup guard checks the models LIST internally, NOT
        // against the primary. So re-declaring the primary ONCE in the list
        // (OpenCode's intentional "give the primary a custom name" case) is
        // allowed even though `model` and a list id coincide.
        let _g = lock_home();
        let tmp = tempdir("opencode-primary-redeclare");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "redeclare", "https://x.io", "sk-x");
        p.model = "gpt-5".into();
        p.models = vec![
            ProviderModel {
                id: "gpt-5".into(),
                name: "GPT-5 Custom".into(),
            },
            ProviderModel {
                id: "gpt-5-mini".into(),
                name: String::new(),
            },
        ];
        // Not a LIST-internal duplicate → activation succeeds.
        assert!(activate(&p, &[p.clone()]).is_ok());
    }

    #[test]
    fn opencode_activate_preserves_unrelated_provider_blocks() {
        // Pre-existing user `provider.<...>` blocks (manually edited or
        // from /connect baseURL overlays) must survive Termory's activate.
        let _g = lock_home();
        let tmp = tempdir("opencode-preserve");
        let _home = override_home(&tmp);
        fs::create_dir_all(tmp.join(".config/opencode")).unwrap();
        fs::write(
            tmp.join(".config/opencode/opencode.json"),
            r#"{
              "$schema": "https://opencode.ai/config.json",
              "provider": {
                "anthropic": { "options": { "baseURL": "https://user.example.com" } }
              }
            }"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.join(".local/share/opencode")).unwrap();
        let prior_auth = r#"{"github-copilot":{"type":"oauth","refresh":"rt"}}"#;
        fs::write(tmp.join(".local/share/opencode/auth.json"), prior_auth).unwrap();

        let mut p = make_provider(
            CliApp::Opencode,
            "termory-one",
            "https://api.example.com",
            "sk-termory",
        );
        p.model = "claude-opus-4-7".into();
        p.npm = Some("@ai-sdk/anthropic".into());
        activate(&p, &[p.clone()]).unwrap();

        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        let termory_id = format!("termory-{}", p.id);
        assert!(config.pointer(&format!("/provider/{termory_id}")).is_some());
        // User's manual anthropic block untouched.
        assert_eq!(
            config
                .pointer("/provider/anthropic/options/baseURL")
                .and_then(|v| v.as_str()),
            Some("https://user.example.com")
        );
        assert_eq!(
            config.get("$schema").and_then(|v| v.as_str()),
            Some("https://opencode.ai/config.json")
        );
        // auth.json must be byte-identical — Termory never touches it.
        assert_eq!(
            fs::read_to_string(tmp.join(".local/share/opencode/auth.json")).unwrap(),
            prior_auth
        );
    }

    #[test]
    fn opencode_deactivate_clears_only_top_model_keeps_slots() {
        // For OpenCode, "Set Official as default" clears the top-level
        // `model` (so no Termory provider is the startup default) but
        // keeps the Enabled slots so they remain selectable via
        // OpenCode's `/model` command.
        let _g = lock_home();
        let tmp = tempdir("opencode-deactivate");
        let _home = override_home(&tmp);

        let mut p = make_provider(
            CliApp::Opencode,
            "termory-one",
            "https://gateway.example.com",
            "sk-termory",
        );
        p.model = "model-a".into();
        p.npm = Some("@ai-sdk/anthropic".into());
        activate(&p, &[p.clone()]).unwrap();
        set_opencode_default(&p).unwrap();

        // Inject unrelated $schema.
        let config_path = tmp.join(".config/opencode/opencode.json");
        let mut config_root: serde_json::Map<String, JsonValue> =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        config_root.insert(
            "$schema".into(),
            JsonValue::String("https://opencode.ai/config.json".into()),
        );
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&JsonValue::Object(config_root)).unwrap(),
        )
        .unwrap();

        deactivate(CliApp::Opencode, &[p.clone()]).unwrap();

        let termory_id = format!("termory-{}", p.id);
        let config_after: JsonValue =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            config_after
                .pointer(&format!("/provider/{termory_id}"))
                .is_some(),
            "termory provider slot must SURVIVE deactivate (still Enabled)"
        );
        assert!(
            config_after.get("model").is_none(),
            "top-level model pointing at us is cleared"
        );
        assert_eq!(
            config_after.get("$schema").and_then(|v| v.as_str()),
            Some("https://opencode.ai/config.json"),
            "unrelated $schema field survived"
        );

        // kind=Official because top-level model is gone, but the
        // provider remains in `configured_provider_ids`.
        let state = read_active_state(CliApp::Opencode, &[p.clone()]).unwrap();
        assert_eq!(state.kind, ActiveKind::Official);
        assert_eq!(state.configured_provider_ids, vec![p.id.clone()]);
    }

    #[test]
    fn opencode_deactivate_preserves_user_set_top_model() {
        // If the user manually pointed top-level `model` at a
        // non-Termory provider, our deactivate must NOT clear it.
        let _g = lock_home();
        let tmp = tempdir("opencode-deactivate-user-model");
        let _home = override_home(&tmp);

        let mut p = make_provider(CliApp::Opencode, "t", "https://api.example.com", "sk-t");
        p.model = "m".into();
        activate(&p, &[p.clone()]).unwrap();

        // User points top-level model at a non-Termory provider.
        let config_path = tmp.join(".config/opencode/opencode.json");
        let mut config_root: serde_json::Map<String, JsonValue> =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        config_root.insert(
            "model".into(),
            JsonValue::String("anthropic/claude-opus-4-7".into()),
        );
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&JsonValue::Object(config_root)).unwrap(),
        )
        .unwrap();

        deactivate(CliApp::Opencode, &[p.clone()]).unwrap();
        let config_after: JsonValue =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            config_after.get("model").and_then(|v| v.as_str()),
            Some("anthropic/claude-opus-4-7"),
            "non-termory user choice for default must survive"
        );
    }

    #[test]
    fn opencode_delete_only_clears_top_model_when_it_points_at_self() {
        // Deleting an inactive provider must NOT touch top-level model
        // (which points at a different Termory provider).
        let _g = lock_home();
        let tmp = tempdir("opencode-delete-inactive");
        let _home = override_home(&tmp);

        let mut a = make_provider(CliApp::Opencode, "a", "https://api.example.com", "sk-a");
        a.id = "aaa".into();
        a.model = "model-a".into();
        let mut b = make_provider(CliApp::Opencode, "b", "https://api.example.com", "sk-b");
        b.id = "bbb".into();
        b.model = "model-b".into();
        activate(&a, &[a.clone(), b.clone()]).unwrap();
        activate(&b, &[a.clone(), b.clone()]).unwrap();
        // Promote B as the default.
        set_opencode_default(&b).unwrap();

        // Delete A (not the default) — top-level model must still point at B.
        delete_provider_traces(&a).unwrap();
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(config.pointer("/provider/termory-aaa").is_none());
        assert!(config.pointer("/provider/termory-bbb").is_some());
        assert_eq!(
            config.get("model").and_then(|v| v.as_str()),
            Some("termory-bbb/model-b")
        );
    }

    #[test]
    fn opencode_activate_allows_empty_api_key() {
        // OpenCode's options.apiKey is optional in the schema. Termory
        // should write the slot without options.apiKey when the user
        // left it blank (some gateways don't need auth; or user
        // intends to fill via env var / `/connect` later).
        let _g = lock_home();
        let tmp = tempdir("opencode-empty-key");
        let _home = override_home(&tmp);
        let mut p = make_provider(CliApp::Opencode, "no-key", "https://example.com", "");
        p.model = "gpt-5".into();
        p.npm = Some("@ai-sdk/openai-compatible".into());
        activate(&p, &[p.clone()]).unwrap();

        let termory_id = format!("termory-{}", p.id);
        let config: JsonValue = serde_json::from_str(
            &fs::read_to_string(tmp.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        let block_ptr = format!("/provider/{termory_id}");
        // options.apiKey omitted, options.baseURL still written.
        assert_eq!(
            config
                .pointer(&format!("{block_ptr}/options/baseURL"))
                .and_then(|v| v.as_str()),
            Some("https://example.com")
        );
        assert!(config
            .pointer(&format!("{block_ptr}/options/apiKey"))
            .is_none());
    }

    #[test]
    fn mask_secret_format() {
        assert_eq!(mask_secret("short"), "•••••");
        // "sk-1234567890abcd" is 17 chars; mask = head(4) + dots(17-8=9) + tail(4)
        assert_eq!(mask_secret("sk-1234567890abcd"), "sk-1•••••••••abcd");
    }
}
