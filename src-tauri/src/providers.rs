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
use std::time::Duration;

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
}

impl CliApp {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(CliApp::Claude),
            "codex" => Some(CliApp::Codex),
            "gemini" => Some(CliApp::Gemini),
            "opencode" => Some(CliApp::Opencode),
            _ => None,
        }
    }

    /// Name of the binary this CLI ships as, looked up on `$PATH`.
    pub fn bin_name(self) -> &'static str {
        match self {
            CliApp::Claude => "claude",
            CliApp::Codex => "codex",
            CliApp::Gemini => "gemini",
            CliApp::Opencode => "opencode",
        }
    }

    pub fn all() -> [CliApp; 4] {
        [
            CliApp::Claude,
            CliApp::Codex,
            CliApp::Gemini,
            CliApp::Opencode,
        ]
    }
}

/// Build the ordered list of directories to scan when looking for
/// `tool`'s binary. Modeled after cc-switch's `scan_cli_version`
/// (`commands/misc.rs:584`) — covers every common installation method
/// instead of relying on the inherited process `$PATH`.
///
/// Order matters: shorter / user-level paths come first so we prefer
/// per-user installs over system ones. Per-tool sections (opencode
/// env vars + bun/go/XDG) only contribute when the tool requests them.
fn cli_search_paths(tool: &str) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut paths: Vec<PathBuf> = Vec::new();
    let home = dirs::home_dir().unwrap_or_default();

    // Cross-platform user-level dir — `~/.npm-global/bin` resolves to
    // `%USERPROFILE%\.npm-global\bin` on Windows; uncommon but valid
    // when a user sets a custom npm prefix anywhere.
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join(".npm-global/bin"));
    }

    // Unix-only user-level dirs (XDG `~/.local/bin`, n version manager,
    // Unix-style Volta layout). On Windows these are non-existent paths,
    // so gating them avoids dead stat() calls and clarifies intent.
    #[cfg(unix)]
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join(".local/bin"));
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
        // NVM: every installed node version gets its own bin dir.
        let nvm_base = home.join(".nvm/versions/node");
        if nvm_base.exists() {
            if let Ok(entries) = std::fs::read_dir(&nvm_base) {
                for entry in entries.flatten() {
                    let bin = entry.path().join("bin");
                    if bin.exists() {
                        push_unique(&mut paths, bin);
                    }
                }
            }
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

    // opencode's curl installer respects $OPENCODE_INSTALL_DIR then
    // $XDG_BIN_DIR, and defaults to ~/.opencode/bin (per the upstream
    // install script). Also reachable via bun and go installs.
    if tool == "opencode" {
        if let Some(val) = std::env::var_os("OPENCODE_INSTALL_DIR") {
            push_unique(&mut paths, PathBuf::from(val));
        }
        if let Some(val) = std::env::var_os("XDG_BIN_DIR") {
            push_unique(&mut paths, PathBuf::from(val));
        }
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

    // Cross-platform per-user installer fallbacks (bun / cargo / codex
    // curl). Bun and cargo on Windows install under %USERPROFILE%
    // matching the same `~/.bun/bin`, `~/.cargo/bin` joins.
    if !home.as_os_str().is_empty() {
        push_unique(&mut paths, home.join(".bun/bin"));
        push_unique(&mut paths, home.join(".cargo/bin"));
        push_unique(&mut paths, home.join(".codex/bin"));
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
const SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Spawn `cmd` and read its full output, killing the child if it
/// hasn't exited within [`SUBPROCESS_TIMEOUT`]. Returns `None` on
/// spawn failure, non-zero exit, or timeout. Polls `try_wait` every
/// 50ms — accurate enough for a 5s budget and avoids a watchdog
/// thread.
fn output_with_timeout(mut cmd: std::process::Command) -> Option<std::process::Output> {
    use std::process::Stdio;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > SUBPROCESS_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Run `--version` on the resolved binary, with PATH augmented to
/// include the binary's directory (so the CLI's own runtime — node,
/// dyld dependencies, etc. — resolves). Mirrors cc-switch's per-path
/// `Command::new(tool_path).env("PATH", new_path)` pattern.
fn query_version_at(binary: &std::path::Path) -> Option<String> {
    let dir = binary.parent()?;
    let current_path = std::env::var("PATH").unwrap_or_default();
    #[cfg(unix)]
    let augmented = format!("{}:{}", dir.display(), current_path);
    #[cfg(windows)]
    let augmented = format!("{};{}", dir.display(), current_path);

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
    cmd.args(["-l", "-i", "-c", &format!("{tool} --version 2>&1")]);
    let output = output_with_timeout(cmd)?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    if raw.trim().is_empty() {
        return None;
    }
    parse_version(&raw)
}

#[cfg(not(unix))]
fn shell_version_fallback(_tool: &str) -> Option<String> {
    None
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
pub fn detect_installed_clis() -> std::collections::HashMap<CliApp, bool> {
    let mut out = std::collections::HashMap::new();
    for app in CliApp::all() {
        let installed = find_cli_binary(app.bin_name()).is_some()
            || shell_version_fallback(app.bin_name()).is_some();
        out.insert(app, installed);
    }
    out
}

/// Run each installed CLI with `--version` and return the parsed
/// version string. Spawns one subprocess per CLI so the frontend
/// calls this on page-load + Recheck only, never inside hot paths.
pub fn detect_cli_versions() -> std::collections::HashMap<CliApp, Option<String>> {
    let mut out = std::collections::HashMap::new();
    for app in CliApp::all() {
        let v = find_cli_binary(app.bin_name())
            .and_then(|p| query_version_at(&p))
            .or_else(|| shell_version_fallback(app.bin_name()));
        out.insert(app, v);
    }
    out
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

/// Which env-var name Claude Code looks up the auth string under.
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
    /// Claude-only options nested under `claude` in the JSON so the
    /// editor can group them visually and Termory doesn't pollute the
    /// top-level shape for every app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeOptions>,
    /// OpenCode-only options nested under `opencode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<OpencodeOptions>,
    /// User-defined config overrides merged into the CLI's live config
    /// on activation and stripped on switch/deactivate. `key` is a
    /// dot-path (`env.FOO`, `tools.web.enabled`); `value` is type-
    /// inferred (`true`/`false` → bool, numeric → number, else string)
    /// for JSON/TOML targets and kept verbatim for Gemini's `.env`.
    /// See `apply_overrides` / `override_keys`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<ProviderOverride>,
    /// Cached favicon as a `data:image/...;base64,...` URL. Populated
    /// at create / edit time via `fetch_favicon` so the ProviderCard
    /// can render the brand mark locally without making a network
    /// request on every render (and without leaking the provider's
    /// hostname to Google's s2 service the way the legacy live-fetch
    /// did). `None` ⇒ fall back to the letter avatar in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
}

/// Claude Code's `/model` menu reads three env vars
/// (ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL — `modelOptions.ts:167`)
/// to route per-size picks at the upstream provider. Empty fields fall
/// back to the provider's top-level `model`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOptions {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub haiku_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sonnet_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub opus_model: String,
    /// Declare the 1M context window per route via the `[1m]` model-id
    /// suffix on ANTHROPIC_DEFAULT_{SONNET,OPUS}_MODEL (cc-switch's
    /// per-route `supports1m`). Haiku has no 1M variant, so there's no
    /// Haiku flag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sonnet_1m: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opus_1m: bool,
}

/// OpenCode catalog binding. `provider_id` selects which AI SDK npm
/// package OpenCode loads — the dropdown shows catalog ids
/// (`anthropic` / `openai` / `openai-compatible` / …) which map to
/// `@ai-sdk/<id>` npm packages in `opencode_npm_for_catalog`.
/// Empty/missing → defaults to `openai-compatible`. `models` are
/// extra model ids surfaced in OpenCode's picker alongside the
/// provider's primary `model` (top-level Provider field).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Extra model ids to expose in OpenCode's model picker alongside
    /// the top-level `model` field (which acts as the primary/default).
    /// OpenCode supports multiple models per provider — they all get
    /// written as `models: { <id>: { name: "<id>" } }` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// One user-defined config override. `key` is a dot-path into the
/// CLI's config (`env.FOO`, `tools.web.enabled`); `value` is the raw
/// string the user typed (type-inferred per target format at write
/// time).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderOverride {
    pub key: String,
    pub value: String,
}

impl Provider {
    fn claude_haiku_model(&self) -> &str {
        self.claude
            .as_ref()
            .map(|c| c.haiku_model.as_str())
            .unwrap_or("")
    }
    fn claude_sonnet_model(&self) -> &str {
        self.claude
            .as_ref()
            .map(|c| c.sonnet_model.as_str())
            .unwrap_or("")
    }
    fn claude_opus_model(&self) -> &str {
        self.claude
            .as_ref()
            .map(|c| c.opus_model.as_str())
            .unwrap_or("")
    }
    fn opencode_catalog_id_raw(&self) -> Option<&str> {
        self.opencode
            .as_ref()
            .and_then(|o| o.provider_id.as_deref())
    }
    fn opencode_extra_models(&self) -> &[String] {
        self.opencode
            .as_ref()
            .map(|o| o.models.as_slice())
            .unwrap_or(&[])
    }
}

const OPENCODE_DEFAULT_PROVIDER_ID: &str = "openai-compatible";

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
    match provider.app {
        CliApp::Claude => activate_claude(provider, providers_for_app),
        CliApp::Codex => activate_codex(provider, providers_for_app),
        CliApp::Gemini => activate_gemini(provider, providers_for_app),
        CliApp::Opencode => activate_opencode(provider, providers_for_app),
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
        CliApp::Claude | CliApp::Codex | CliApp::Gemini => Ok(()),
        CliApp::Opencode => delete_opencode_provider_entry(provider),
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
    Ok(home()?.join(".claude").join("settings.json"))
}

/// Append Claude Code's `[1m]` 1M-context suffix to a model id when
/// `on` is set — skips empty ids and ones already suffixed.
fn with_claude_1m(model: &str, on: bool) -> String {
    if on && !model.is_empty() && !model.ends_with("[1m]") {
        format!("{model}[1m]")
    } else {
        model.to_string()
    }
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
    // Per-size routing: when the user picks Haiku / Sonnet / Opus
    // from Claude Code's `/model` menu in 3P mode, Claude reads
    // these env vars to decide the actual model id to send (see
    // `modelOptions.ts:77-89, 109, 167` — `is3P && customXxxModel`
    // branch). Empty string in the provider means "don't override
    // this size"; we strip the corresponding env var so Claude falls
    // back to its default Anthropic-side resolution.
    //
    // 1M context is declared per-route via the `[1m]` model-id suffix
    // (cc-switch `claude_desktop_config.rs` ONE_M_CONTEXT_MARKER) on
    // the ANTHROPIC_DEFAULT_{SONNET,OPUS}_MODEL values. Haiku has no
    // 1M variant, so it never gets the suffix.
    let (sonnet_1m, opus_1m) = p
        .claude
        .as_ref()
        .map(|c| (c.sonnet_1m, c.opus_1m))
        .unwrap_or((false, false));
    for (env_key, val, one_m) in [
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            p.claude_haiku_model(),
            false,
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            p.claude_sonnet_model(),
            sonnet_1m,
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            p.claude_opus_model(),
            opus_1m,
        ),
    ] {
        if val.is_empty() {
            env.remove(env_key);
        } else {
            env.insert(
                env_key.into(),
                JsonValue::String(with_claude_1m(val, one_m)),
            );
        }
    }
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

fn codex_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".codex"))
}

fn codex_auth_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(codex_dir()?.join("auth.json"))
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
        apply_toml_overrides(&mut doc, p);
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
    for o in &p.overrides {
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
    // xdg-basedir uses ~/.config on every platform (verified at
    // .audit-sources/opencode/packages/core/src/global.ts:12 +
    // xdg-basedir source). Don't use dirs::config_dir() — it returns
    // ~/Library/Application Support on macOS, which is wrong.
    Ok(home()?
        .join(".config")
        .join("opencode")
        .join("opencode.json"))
}

fn opencode_catalog_id(p: &Provider) -> String {
    p.opencode_catalog_id_raw()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(OPENCODE_DEFAULT_PROVIDER_ID)
        .to_string()
}

/// Stable, per-Termory-provider id used in OpenCode's auth.json and
/// opencode.json. Termory writes its providers under this id so they
/// don't share the catalog id namespace with the user's `/connect`
/// entries.
fn opencode_termory_id(p: &Provider) -> String {
    format!("termory-{}", p.id)
}

/// Map the user-facing catalog id (anthropic / openai-compatible /
/// google / …) to the npm package name OpenCode loads for that
/// provider. This is what goes into opencode.json `provider.<id>.npm`
/// so OpenCode knows which AI SDK to instantiate.
fn opencode_npm_for_catalog(catalog_id: &str) -> &'static str {
    match catalog_id {
        "anthropic" => "@ai-sdk/anthropic",
        "openai" => "@ai-sdk/openai",
        "openai-compatible" => "@ai-sdk/openai-compatible",
        "google" => "@ai-sdk/google",
        "azure" => "@ai-sdk/azure",
        "amazon-bedrock" => "@ai-sdk/amazon-bedrock",
        "google-vertex" => "@ai-sdk/google-vertex",
        _ => "@ai-sdk/openai-compatible",
    }
}

fn activate_opencode(p: &Provider, all: &[Provider]) -> Result<(), Box<dyn Error>> {
    // Primary model is required — without an entry in `models` map
    // OpenCode's picker can't surface this provider. API key is
    // optional (OpenCode supports env-var references and some
    // gateways don't require auth) — we just omit options.apiKey
    // when the user left it blank.
    if p.model.trim().is_empty() {
        return Err("OpenCode provider requires a primary model id.".into());
    }

    let termory_id = opencode_termory_id(p);
    let catalog = opencode_catalog_id(p);
    let npm = opencode_npm_for_catalog(&catalog);

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
    if !opts.is_empty() {
        block.insert("options".into(), JsonValue::Object(opts));
    }

    // models map: primary first, then any extras the user added in the
    // editor. Dedup so the primary isn't repeated. cc-switch writes
    // each model with `{name: "<id>"}` so OpenCode's picker has a label.
    let mut models = serde_json::Map::new();
    let mut seen = std::collections::HashSet::new();
    for m in std::iter::once(p.model.trim()).chain(
        p.opencode_extra_models()
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty()),
    ) {
        if !seen.insert(m.to_string()) {
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("name".into(), JsonValue::String(m.to_string()));
        models.insert(m.to_string(), JsonValue::Object(entry));
    }
    block.insert("models".into(), JsonValue::Object(models));

    // NOTE: Termory does NOT write the top-level `model` field on
    // activate. Activate only registers this provider's slot.
    // Setting it as OpenCode's startup default is a separate explicit
    // action via `set_opencode_default`. Multi-provider coexistence
    // is intentional — OpenCode picks at runtime via `/model`.
    //
    // Clear every provider's override keys, then apply this one's.
    strip_json_overrides(&mut root, &override_keys(all, CliApp::Opencode));
    apply_json_overrides(&mut root, p, CliApp::Opencode);
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
    if p.model.trim().is_empty() {
        return Err("Provider needs a primary model id to be set as default.".into());
    }
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
        JsonValue::String(format!("{termory_id}/{}", p.model.trim())),
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

    // Strip every provider's override keys.
    let keys = override_keys(providers, CliApp::Opencode);
    let had_overrides = !keys.is_empty();
    strip_json_overrides(&mut root, &keys);

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

    // Only touch the file when something Termory-owned changed.
    if had_overrides || model_removed {
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
// Helpers
// ===================================================================

fn home() -> Result<PathBuf, Box<dyn Error>> {
    dirs::home_dir().ok_or_else(|| "home directory not available".into())
}

fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
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

fn load_json_object(path: &Path) -> Result<Map<String, JsonValue>, Box<dyn Error>> {
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

fn write_json_object(path: &Path, root: &Map<String, JsonValue>) -> Result<(), Box<dyn Error>> {
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
// User-defined config overrides (Provider.overrides)
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
fn override_key_is_managed(app: CliApp, key: &str) -> bool {
    let k = key.trim();
    match app {
        CliApp::Claude => matches!(
            k,
            "env.ANTHROPIC_BASE_URL"
                | "env.ANTHROPIC_AUTH_TOKEN"
                | "env.ANTHROPIC_API_KEY"
                | "env.ANTHROPIC_MODEL"
                | "env.ANTHROPIC_DEFAULT_HAIKU_MODEL"
                | "env.ANTHROPIC_DEFAULT_SONNET_MODEL"
                | "env.ANTHROPIC_DEFAULT_OPUS_MODEL"
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
        // OpenCode: top-level model + the whole provider.* tree
        // (Termory's provider.<termory-id> slot).
        CliApp::Opencode => k == "model" || k.starts_with("provider."),
    }
}

/// Union of every override key declared by any provider in the list,
/// excluding managed keys — the set Termory must clear before
/// (re)writing the active provider's overrides.
fn override_keys(providers: &[Provider], app: CliApp) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for p in providers {
        for o in &p.overrides {
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
fn infer_json_value(raw: &str) -> JsonValue {
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
fn json_set_path(root: &mut Map<String, JsonValue>, key: &str, value: JsonValue) {
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

/// Apply a provider's overrides into a JSON config root (Claude
/// settings.json, opencode.json).
fn apply_json_overrides(root: &mut Map<String, JsonValue>, p: &Provider, app: CliApp) {
    for o in &p.overrides {
        let key = o.key.trim();
        if key.is_empty() || override_key_is_managed(app, key) {
            continue;
        }
        json_set_path(root, key, infer_json_value(&o.value));
    }
}

/// Claude variant: `settings.json`'s `env` object is
/// `Record<string, string>`, so override values under `env.*` are kept
/// as strings (never type-inferred to bool/number); everything else is
/// inferred like normal JSON. Managed keys (base url / token / model /
/// routing) are skipped — the dedicated fields own them.
fn apply_claude_overrides(root: &mut Map<String, JsonValue>, p: &Provider) {
    for o in &p.overrides {
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

/// Apply a provider's overrides into a TOML document (Codex config.toml).
fn apply_toml_overrides(doc: &mut DocumentMut, p: &Provider) {
    for o in &p.overrides {
        if override_key_is_managed(CliApp::Codex, o.key.trim()) {
            continue;
        }
        let segs = dot_path(&o.key);
        let Some((last, parents)) = segs.split_last() else {
            continue;
        };
        let mut tbl = doc.as_table_mut();
        for seg in parents {
            let entry = tbl.entry(seg).or_insert(toml_edit::table());
            if !entry.is_table() {
                *entry = toml_edit::table();
            }
            tbl = entry.as_table_mut().expect("ensured table");
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

fn mask_secret(value: &str) -> String {
    if value.len() <= 8 {
        "•".repeat(value.len())
    } else {
        format!(
            "{}{}{}",
            &value[..4],
            "•".repeat(value.len() - 8),
            &value[value.len() - 4..]
        )
    }
}

fn string_match(provider_value: &str, live_value: Option<&str>) -> bool {
    let live = live_value.unwrap_or("");
    provider_value.trim() == live.trim()
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::HOME_LOCK;

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

    /// Verify [`output_with_timeout`] kills hung children. We can't
    /// wait the full 5s in a unit test, so this exercises the
    /// happy-path return shape instead — `sleep 0` exits immediately
    /// and the watchdog never trips. Pair with a slower integration
    /// test if hanging behavior matters more.
    #[cfg(unix)]
    #[test]
    fn output_with_timeout_returns_on_immediate_exit() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        let out = output_with_timeout(cmd).expect("shell should exit");
        assert!(out.status.success());
    }

    /// Slow integration test — only runs when explicitly opted in via
    /// `--ignored`, since it sleeps for the full SUBPROCESS_TIMEOUT
    /// window. Confirms a child that exceeds the budget gets killed
    /// and the function returns `None`.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn output_with_timeout_kills_hung_child() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let start = std::time::Instant::now();
        let result = output_with_timeout(cmd);
        let elapsed = start.elapsed();
        assert!(result.is_none(), "should have given up");
        assert!(
            elapsed < SUBPROCESS_TIMEOUT + std::time::Duration::from_secs(2),
            "should have killed near the timeout window, took {elapsed:?}"
        );
    }

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

    struct HomeOverride {
        prev: Option<std::ffi::OsString>,
    }
    impl HomeOverride {
        fn new(p: &Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", p);
            HomeOverride { prev }
        }
    }
    impl Drop for HomeOverride {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
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
            claude: None,
            opencode: None,
            favicon: None,
            overrides: Vec::new(),
        }
    }

    #[test]
    fn provider_overrides_apply_strip_and_infer_types_json() {
        // Claude (JSON) overrides: dot-path nesting, type inference,
        // clean removal of the previous provider's keys on switch
        // (incl. empty-parent pruning), and full strip on deactivate.
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("ovr-json");
        let _home = HomeOverride::new(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let mut a = make_provider(CliApp::Claude, "A", "https://a.x.io", "sk-a");
        a.overrides = vec![
            ProviderOverride {
                key: "env.FOO".into(),
                value: "bar".into(),
            },
            ProviderOverride {
                key: "permissions.defaultMode".into(),
                value: "acceptEdits".into(),
            },
            // Managed key — MUST be ignored, never clobbers the real
            // baseUrl from the dedicated field.
            ProviderOverride {
                key: "env.ANTHROPIC_BASE_URL".into(),
                value: "https://evil.example".into(),
            },
        ];
        let mut b = make_provider(CliApp::Claude, "B", "https://b.x.io", "sk-b");
        b.overrides = vec![
            // Looks like a bool, but env values must stay strings.
            ProviderOverride {
                key: "env.FLAG".into(),
                value: "true".into(),
            },
            // Non-env key → type-inferred to a number.
            ProviderOverride {
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("ovr-toml");
        let _home = HomeOverride::new(&tmp);
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        let mut p = make_provider(CliApp::Codex, "C", "https://c.x.io/v1", "sk-c");
        p.overrides = vec![
            ProviderOverride {
                key: "model_reasoning_effort".into(),
                value: "high".into(),
            },
            ProviderOverride {
                key: "tools.web_search".into(),
                value: "true".into(),
            },
        ];
        let all = vec![p.clone()];

        activate(&p, &all).unwrap();
        let txt = fs::read_to_string(tmp.join(".codex/config.toml")).unwrap();
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-rt");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-oauth-keep");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-legacy-model");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-multi-model");
        let _home = HomeOverride::new(&tmp);

        let mut p = make_provider(
            CliApp::Claude,
            "multi-route",
            "https://api.x.io",
            "sk-multi",
        );
        p.model = "gpt-5".into();
        p.claude = Some(ClaudeOptions {
            sonnet_model: "gpt-5".into(),
            opus_model: "claude-opus-4-7".into(),
            haiku_model: "deepseek-chat".into(),
            ..Default::default()
        });
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
    fn claude_activate_appends_1m_suffix_per_route_only() {
        // Declaring 1M on Sonnet (but not Opus) must append `[1m]` to
        // ANTHROPIC_DEFAULT_SONNET_MODEL only. Opus stays bare, Haiku
        // never gets it (no 1M variant), and the main ANTHROPIC_MODEL
        // is untouched.
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-1m");
        let _home = HomeOverride::new(&tmp);

        let mut p = make_provider(CliApp::Claude, "ctx-1m", "https://api.x.io", "sk-1m");
        p.model = "claude-sonnet-4-5".into();
        p.claude = Some(ClaudeOptions {
            sonnet_model: "claude-sonnet-4-5".into(),
            opus_model: "claude-opus-4-7".into(),
            haiku_model: "claude-haiku-4-5".into(),
            sonnet_1m: true,
            opus_1m: false,
        });
        activate(&p, &[p.clone()]).unwrap();

        let after: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        let env_str = |k: &str| {
            after
                .pointer(&format!("/env/{k}"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        assert_eq!(
            env_str("ANTHROPIC_DEFAULT_SONNET_MODEL").as_deref(),
            Some("claude-sonnet-4-5[1m]")
        );
        assert_eq!(
            env_str("ANTHROPIC_DEFAULT_OPUS_MODEL").as_deref(),
            Some("claude-opus-4-7")
        );
        assert_eq!(
            env_str("ANTHROPIC_DEFAULT_HAIKU_MODEL").as_deref(),
            Some("claude-haiku-4-5")
        );
        // Main model never gets the suffix.
        assert_eq!(
            env_str("ANTHROPIC_MODEL").as_deref(),
            Some("claude-sonnet-4-5")
        );
    }

    #[test]
    fn claude_activate_skips_empty_sub_models_and_strips_stale_ones() {
        // User clears the Opus override on an existing provider and
        // saves. We must remove ANTHROPIC_DEFAULT_OPUS_MODEL from
        // settings.json — leaving a stale env var would silently
        // override Claude's default Opus next session.
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-clear-sub-model");
        let _home = HomeOverride::new(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(
            tmp.join(".claude/settings.json"),
            r#"{
              "env": {
                "ANTHROPIC_BASE_URL": "https://api.x.io",
                "ANTHROPIC_AUTH_TOKEN": "sk-stale",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "stale-opus-route"
              }
            }"#,
        )
        .unwrap();

        let mut p = make_provider(CliApp::Claude, "clear-opus", "https://api.x.io", "sk-stale");
        p.model = "gpt-5".into();
        // claude_opus_model stays empty; we expect the stale value to be removed.
        activate(&p, &[p.clone()]).unwrap();

        let after: JsonValue =
            serde_json::from_str(&fs::read_to_string(tmp.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert!(
            after.pointer("/env/ANTHROPIC_DEFAULT_OPUS_MODEL").is_none(),
            "empty claude_opus_model must strip the stale env var"
        );
        assert_eq!(
            after
                .pointer("/env/ANTHROPIC_MODEL")
                .and_then(|v| v.as_str()),
            Some("gpt-5")
        );
    }

    #[test]
    fn claude_unmanaged_when_external_edit_does_not_match_any_provider() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("claude-unmanaged");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("codex-rt");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("codex-stable-id");
        let _home = HomeOverride::new(&tmp);
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

    #[test]
    fn codex_oauth_login_then_activate_api_then_deactivate_keeps_oauth() {
        // Three-stage round-trip: user logged into ChatGPT, swaps to
        // a Custom API provider via Termory, then swaps back to
        // Official. The OAuth tokens must survive all three stages so
        // the user doesn't have to re-run `codex login`.
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("codex-three-stage");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("codex-preserve-oauth");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("codex-builtin");
        let _home = HomeOverride::new(&tmp);
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
    fn gemini_activate_writes_dotenv_and_reverses_with_0600() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("gemini-rt");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("gemini-preserve");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("gemini-oauth-keep");
        let _home = HomeOverride::new(&tmp);
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-cc-mode");
        let _home = HomeOverride::new(&tmp);

        let mut p = make_provider(
            CliApp::Opencode,
            "packycode",
            "https://api.packy.example",
            "sk-packy",
        );
        p.model = "claude-opus-4-7".into();
        p.opencode = Some(OpencodeOptions {
            provider_id: Some("anthropic".into()),
            models: vec!["claude-sonnet-4-5".into(), "claude-haiku-4-5".into()],
        });
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
        // models contains primary + extras, each as {name: "<id>"}.
        for m in ["claude-opus-4-7", "claude-sonnet-4-5", "claude-haiku-4-5"] {
            assert_eq!(
                config
                    .pointer(&format!("{block_ptr}/models/{m}/name"))
                    .and_then(|v| v.as_str()),
                Some(m)
            );
        }
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
    fn opencode_activate_dedupes_primary_in_extra_models() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-dedup");
        let _home = HomeOverride::new(&tmp);

        let mut p = make_provider(CliApp::Opencode, "dedup", "", "sk-dedup");
        p.model = "gpt-5".into();
        p.opencode = Some(OpencodeOptions {
            provider_id: None,
            // Primary repeated + an actual extra
            models: vec!["gpt-5".into(), "gpt-5-mini".into()],
        });
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
    }

    #[test]
    fn opencode_set_default_picks_one_among_multi_activated() {
        // A and B both activated → both slots in opencode.json,
        // but only the one passed to set_opencode_default ends up as
        // the top-level model. Switching default just overwrites the
        // top-level field, slots stay put.
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-set-default");
        let _home = HomeOverride::new(&tmp);

        let mut a = make_provider(CliApp::Opencode, "a", "", "sk-a");
        a.id = "aaa".into();
        a.model = "model-a".into();
        let mut b = make_provider(CliApp::Opencode, "b", "", "sk-b");
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-default-rejects");
        let _home = HomeOverride::new(&tmp);
        let p = make_provider(CliApp::Opencode, "p", "", "sk-p");
        // Never activated.
        let result = set_opencode_default(&p);
        assert!(result.is_err());
        assert!(!tmp.join(".config/opencode/opencode.json").exists());
    }

    #[test]
    fn opencode_activate_rejects_empty_model() {
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-no-model");
        let _home = HomeOverride::new(&tmp);
        let mut p = make_provider(CliApp::Opencode, "no-model", "https://x.io", "sk-x");
        p.model = String::new();
        let result = activate(&p, &[p.clone()]);
        assert!(result.is_err());
        assert!(
            !tmp.join(".config/opencode/opencode.json").exists(),
            "no partial opencode.json on model-missing failure"
        );
    }

    #[test]
    fn opencode_activate_preserves_unrelated_provider_blocks() {
        // Pre-existing user `provider.<...>` blocks (manually edited or
        // from /connect baseURL overlays) must survive Termory's activate.
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-preserve");
        let _home = HomeOverride::new(&tmp);
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

        let mut p = make_provider(CliApp::Opencode, "termory-one", "", "sk-termory");
        p.model = "claude-opus-4-7".into();
        p.opencode = Some(OpencodeOptions {
            provider_id: Some("anthropic".into()),
            models: vec![],
        });
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-deactivate");
        let _home = HomeOverride::new(&tmp);

        let mut p = make_provider(
            CliApp::Opencode,
            "termory-one",
            "https://gateway.example.com",
            "sk-termory",
        );
        p.model = "model-a".into();
        p.opencode = Some(OpencodeOptions {
            provider_id: Some("anthropic".into()),
            models: vec![],
        });
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-deactivate-user-model");
        let _home = HomeOverride::new(&tmp);

        let mut p = make_provider(CliApp::Opencode, "t", "", "sk-t");
        p.model = "m".into();
        p.opencode = Some(OpencodeOptions {
            provider_id: None,
            models: vec![],
        });
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-delete-inactive");
        let _home = HomeOverride::new(&tmp);

        let mut a = make_provider(CliApp::Opencode, "a", "", "sk-a");
        a.id = "aaa".into();
        a.model = "model-a".into();
        let mut b = make_provider(CliApp::Opencode, "b", "", "sk-b");
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
        let _g = HOME_LOCK.lock().unwrap();
        let tmp = tempdir("opencode-empty-key");
        let _home = HomeOverride::new(&tmp);
        let mut p = make_provider(CliApp::Opencode, "no-key", "https://example.com", "");
        p.model = "gpt-5".into();
        p.opencode = Some(OpencodeOptions {
            provider_id: Some("openai-compatible".into()),
            models: vec![],
        });
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
