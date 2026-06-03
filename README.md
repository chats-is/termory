# Termory

> One place to browse your AI coding-CLI history — and switch API providers in a click.

Termory is a desktop app for **Codex**, **Claude Code**, **Gemini CLI**, and **OpenCode**. See every session, memory file, and skill across all your tools in one window, keep the messages worth saving, search across everything, and switch any CLI between API providers without touching a config file. It runs natively on **macOS, Linux, and Windows**, and everything stays on your machine.

## Features

- **Providers** — keep named API profiles for each CLI and switch the active one with a click (or straight from the macOS menu bar). Your native login is never touched, so it always survives the switch.
- **Records** — all your sessions, memory files, and skills from every tool, rendered the way each tool shows them.
- **Favorites** — star any message; it's saved as a snapshot that stays readable even after the original session is gone.
- **Search** — instant search across all your history, with a ⌘K command palette.
- **Stats** — tokens, messages, projects, and an activity heatmap over any date range.
- **Local-first** — no servers, no telemetry. Termory reads your history in place and never modifies it.

## Supported tools

Codex · Claude Code · Gemini CLI · OpenCode

## Download

Grab the latest installer for your platform from the [Releases](https://github.com/copilot-is/termory/releases) page — macOS (Apple Silicon & Intel), Linux (AppImage / deb / rpm), and Windows.

> Current builds are unsigned, so the OS may warn on first launch. On macOS, right-click the app and choose **Open**.

## Build from source

Requires Node.js 20+, the Rust toolchain, and the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
npm install
npm run tauri:dev     # run the desktop app
npm run tauri:build   # build installers
```
