<div align="center">

# Termory

**One place to browse your AI coding-CLI history — and switch API providers in a click.**

[![Release](https://img.shields.io/github/v/release/copilot-is/termory?include_prereleases&sort=semver)](https://github.com/copilot-is/termory/releases)
[![License](https://img.shields.io/github/license/copilot-is/termory?color=blue)](LICENSE)
![Platforms](https://img.shields.io/badge/platform-macOS%20·%20Linux%20·%20Windows-555)
![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%20v2-24C8DB)

</div>

---

Termory is a local-first desktop app for **Codex**, **Claude Code**, **Gemini CLI**, and **OpenCode**. See every session, memory file, and skill across all your tools in one window, keep the messages worth saving, search across everything, and switch any CLI between API providers without touching a config file. It runs natively on macOS, Linux, and Windows, and everything stays on your machine.

<div align="center">
  <img src="docs/screenshots/providers.png" alt="Termory — switch any CLI between API providers" width="820">
  <br><br>
  <img src="docs/screenshots/stats.png" alt="Termory — usage stats across every tool" width="820">
</div>

## Features

| | |
|---|---|
| **Providers** | Keep named API profiles per CLI and switch the active one with a click — or from the macOS menu bar. Your native login is never touched, so it survives the switch. |
| **Records** | All your sessions, memory files, and skills from every tool, rendered the way each tool shows them. |
| **Favorites** | Star any message; it's saved as a snapshot that stays readable even after the original session is gone. |
| **Search** | Instant search across all your history, with a `⌘K` command palette. |
| **Stats** | Tokens, messages, projects, and an activity heatmap over any date range. |
| **Local-first** | No servers, no telemetry. Termory reads your history in place and never modifies it. |

## Supported tools

| Tool | Sessions | Memory | Skills | Provider switch |
|------|:--------:|:------:|:------:|:---------------:|
| Codex | ✅ | ✅ | ✅ | ✅ |
| Claude Code | ✅ | ✅ | ✅ | ✅ |
| Gemini CLI | ✅ | ✅ | ✅ | ✅ |
| OpenCode | ✅ | ✅ | ✅ | ✅ |

## Download

Grab the installer for your platform from the [**Releases**](https://github.com/copilot-is/termory/releases) page:

| Platform | File |
|----------|------|
| macOS (Apple Silicon / Intel) | `.dmg` |
| Linux | `.AppImage` · `.deb` · `.rpm` |
| Windows | `.msi` · `.exe` |

> [!NOTE]
> Current builds are unsigned, so the OS may warn on first launch. On macOS, right-click the app and choose **Open**.

## Providers

A **provider** is a named snapshot of a CLI's API settings (`base URL`, `API key`, `model`, …). Each CLI keeps its own library, and switching one materializes its settings into that CLI's live config so the next launch picks it up:

| CLI | Live config written |
|-----|---------------------|
| Claude Code | `~/.claude/settings.json` |
| Codex | `~/.codex/auth.json` + `~/.codex/config.toml` |
| Gemini CLI | `~/.gemini/.env` |
| OpenCode | `~/.config/opencode/opencode.json` |

Switching back to **Official** only clears the fields Termory injected — your native credentials (OAuth tokens, `.credentials.json`, …) are never written. **Advanced settings** let you merge extra per-provider options that are removed again when you switch away.

## Build from source

Requires [Node.js](https://nodejs.org/) 20+, the [Rust toolchain](https://rustup.rs/), and the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
npm install
npm run tauri:dev     # run the desktop app
npm run tauri:build   # build installers
```

Tests:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib   # Rust
npm test                                                # frontend
```

## License

[MIT](LICENSE) © 2026 John Ma
