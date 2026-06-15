# Termory — Guide

> 中文版见 [GUIDE.zh-CN.md](GUIDE.zh-CN.md)

Termory brings the history your terminal AI coding tools — **Codex**, **Claude Code**, **Gemini CLI**, and **OpenCode** — already store on your machine (sessions, memory, skills) into one window to browse, with no import or setup (it just reads the tools' existing local data). On top of browsing history, it also lets you manage and switch each tool's API providers in a click. For installation and downloads, see the [README](../README.md).

## Feature map

Termory has six destinations on the left activity rail (`⌘1`–`⌘6`), plus a macOS menu-bar tray:

| # | Destination | What it does |
|---|-------------|--------------|
| 1 | **[Providers](#1-providers)** | Manage each CLI's API providers and switch the active one; manage AI Gateways; view official quota. |
| 2 | **[Records](#2-records)** | Browse every session, memory file, and skill; resume, migrate, or delete them. |
| 3 | **[Favorites](#3-favorites)** | Messages you've starred, saved as snapshots. |
| 4 | **[Search](#4-search)** | Full-text search across all history, plus a `⌘K` quick-search palette. |
| 5 | **[Stats](#5-stats)** | Tokens, messages, models, and an activity heatmap over any date range. |
| 6 | **[Settings](#6-settings)** | Appearance, language, startup, terminal, storage, search history, shortcuts, updates. |
| — | **[Menu-bar tray](#menu-bar-tray-macos)** | Resume a session, start a new one, or switch providers without opening the window. |

Two cross-cutting topics — **[Privacy & your data](#privacy--your-data)** and **[Installation & updates](#installation--updates)** — are covered at the end.

---

## 1. Providers

**What it is.** Termory keeps a library of **providers** for each CLI — named configurations for different API platforms, each holding a base URL, API key, and model. A CLI's providers are independent: for Claude Code you might keep an OpenRouter config, a local model, and the official login, and switch which one is active at any time. The **Providers** page (the default landing page) is where you manage them.

### Switching the active provider

1. Open **Providers** and pick the CLI's tab (Claude Code / Codex / Gemini / OpenCode).
2. Click **Activate** (or **Set as default**) on the provider you want.
3. The next time you launch that CLI, it uses the new provider — no manual config editing.

Click **Official** to go back to your native login at any time.

### Adding or editing a provider

1. Click **Add provider**, give it a **Name**, and fill in **Base URL**, **API key**, and **Model**.
2. The model field auto-suggests available models once the base URL and key are set (you can also type one in).
3. **Test** checks connectivity to the base URL before you rely on it.
4. **Save**. Editing a provider that's currently active re-applies it immediately.

### Termory never fully overwrites your CLI config (the technical principle)

This is the core guarantee: activating a provider **merges a few fields into your existing config — it never replaces the whole file.**

**Activation is a field-level merge.** Termory reads the CLI's current config, adds *only* the fields needed to point it at your chosen provider (base URL, key, model, plus a few CLI-specific routing keys), and writes the result back. Everything else in the file — your own customizations, unrelated settings, other tools' entries — is preserved untouched.

**And your login credentials sit in a separate file Termory never writes at all.** On top of the merge above, each CLI keeps its OAuth tokens / credentials apart from the config Termory edits, so your login is doubly safe:

| CLI | Config Termory merges into | Credential file it never touches |
|-----|----------------------------|----------------------------------|
| Claude Code | `~/.claude/settings.json` | `~/.claude/.credentials.json` (or macOS Keychain) |
| Codex | `~/.codex/auth.json` + `~/.codex/config.toml` | the `tokens` inside `auth.json` (the ChatGPT login) |
| Gemini CLI | `~/.gemini/.env` | `~/.gemini/oauth_creds.json` + `google_accounts.json` |
| OpenCode | `~/.config/opencode/opencode.json` | `~/.local/share/opencode/auth.json` |

(Codex is the one case where config and a credential share a file — there Termory still *merges*: it sets `auth_mode` + the API key but leaves your `tokens` intact, so the ChatGPT login survives.)

**Switching back to Official is symmetric:** Termory removes the override fields it added and leaves everything else as-is. Because your native login was never overwritten, the CLI resumes using it immediately — no re-login.

### Advanced settings (per-provider options)

Beyond the basic fields, each provider has an **Advanced settings** section where you add your own config entries — any setting the CLI supports that Termory doesn't have a dedicated field for. Each entry you add is merged into that CLI's config when you activate the provider, and removed again when you switch away.

**How to add one:**

1. In the provider editor, expand **Advanced settings**.
2. Click **Add** to get a new row, then fill in the **KEY** and **VALUE**.
3. Add as many rows as you need; **Remove** drops one. **Save** the provider.

The tables below are just common examples — you can add any key/value the CLI accepts. The rules:

- The **key** is a dot-path into the CLI's config — `a.b.c` creates nested structure.
- The **value** is type-inferred for JSON/TOML targets: `true`/`false` → boolean, whole numbers → integer, decimals → float, anything else → string. (Gemini's `.env` keeps every value as a literal string.)
- Keys owned by the dedicated fields (base URL / key / model) are **managed** — the editor blocks them, because those fields already control them.

**Claude Code** → `~/.claude/settings.json`. The headline use is mapping Claude's Sonnet / Opus / Haiku sizes to specific upstream models (a new Claude provider is pre-seeded with these three rows):

| Key | Example value |
|-----|---------------|
| `env.ANTHROPIC_DEFAULT_SONNET_MODEL` | `gpt-5` |
| `env.ANTHROPIC_DEFAULT_OPUS_MODEL` | `claude-opus-4-8` |
| `env.ANTHROPIC_DEFAULT_HAIKU_MODEL` | `claude-haiku-4-5` |

Append `[1m]` to a value for its 1M-token context window, e.g. `claude-sonnet-4-6[1m]`. *Managed (blocked):* `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_AUTH_TOKEN`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_MODEL`.

**Codex** → `~/.codex/config.toml`. The dot-path becomes a nested TOML table:

| Key | Example value | Result |
|-----|---------------|--------|
| `model_reasoning_effort` | `high` | `model_reasoning_effort = "high"` |
| `approval_policy` | `on-request` | a string value |
| `tools.web_search` | `true` | a boolean (type-inferred) |

*Managed (blocked):* `model`, `model_provider`, the whole `model_providers.*` table.

**Gemini CLI** → `~/.gemini/.env`. Any environment variable Gemini reads — e.g. target a Google Cloud project or use Vertex AI:

| Key | Example value |
|-----|---------------|
| `GOOGLE_CLOUD_PROJECT` | `my-project-id` |
| `GOOGLE_GENAI_USE_VERTEXAI` | `true` |

*Managed (blocked):* `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY`, `GEMINI_MODEL`.

**OpenCode** → `~/.config/opencode/opencode.json`. OpenCode has two extra dedicated fields above the options — **AI SDK** (the npm package it loads, `@ai-sdk/openai-compatible` by default) and **Additional models** (extra model IDs for its picker). Advanced option keys are relative to the provider's `options` bag:

| Key | Example value | Effect |
|-----|---------------|--------|
| `timeout` | `600000` | request timeout (ms) |
| `headers.X-Token` | `abc123` | custom request header |

*Managed (blocked):* `baseURL`, `apiKey`.

### Codex: keeping sessions after a switch

Codex tags every session with the provider active when it was created, and `codex resume` only lists sessions matching the *current* provider — so switching can hide a project's earlier sessions. When you switch Codex between Official and a custom provider, Termory offers to **"Keep earlier sessions?"**: pick the projects whose sessions should follow into the new provider, and Termory re-tags them so `codex resume` keeps listing them. The other CLIs list resume history by project path and don't need this.

### AI Gateways

A **gateway** is a single `{base URL, API key}` that may speak several API formats at once (OpenAI, Anthropic, Gemini…). Instead of adding the same key separately to each CLI, add the gateway once:

1. Open the **AI Gateways** tab → **Add** a gateway with its base URL and key.
2. Click **Detect APIs** — Termory probes which API formats the gateway answers.
3. **Apply** it to each CLI whose format matches (one gateway → many CLIs, one key).

Bound gateways also appear in each CLI's provider list (view/activate only — edit them from the Gateways tab).

### Official quota

For a CLI logged in with an official subscription, the card shows your usage as donut rings (e.g. **5-hour** and **Weekly** windows), color-coded by pressure (🟢 < 75%, 🟡 ≥ 75%, 🔴 ≥ 90%). **Refresh usage** re-fetches it (with a short cooldown). Quota reads your existing official login only — it's hidden while a custom provider is active.

---

## 2. Records

**What it is.** Records is the history browser. Three panes — **Sessions**, **Memories**, **Skills** — list everything Termory found across all four tools.

- **Sessions** — chat transcripts from each CLI.
- **Memories** — on-disk memory / instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, per-project memory folders, etc.).
- **Skills** — `SKILL.md` files under each tool's skills directory.

### Browsing

- **Source filter** (Codex / Claude / Gemini / OpenCode / All) in the sidebar narrows all three panes at once.
- The sidebar groups records by **project** (working directory).
- Click any record to open it; the detail pane renders each message the way its own tool shows it (tool calls, diffs, reasoning, etc.).
- Each message has a **copy** button (raw markdown) and a **star** (add to Favorites).
- The detail header's **Open in Finder** reveals the underlying file.

### Right-click actions

Right-click any session, memory, or skill:

- **Reveal in Finder** — open the underlying file.
- **Resume in terminal** / **Copy resume command** — see [resuming](#resuming-a-session) below.
- **Copy path / filename / session ID**.
- **Migrate** — re-point a session or its whole project to a new path (Claude Code & Codex). Useful after you rename or move a repo, so its history regroups under the new location. (Project-level migrate is also on the sidebar project row.)
- **Delete session / project / memory** — with a confirmation step.

> **Delete is permanent**, and deleting **changes the CLI's own data**. If a CLI is running it may hold a database lock — Termory will tell you to quit it first. Deleting a *project* removes its stored history only; the files in your actual project folder (`CLAUDE.md`, `AGENTS.md`, …) are never touched.

### Resuming a session

Resume a session from the **tray** (click a recent session) or in Records via **right-click → Resume in terminal**. Termory opens your terminal, `cd`s into the session's working directory, and runs the CLI's own resume command (`claude --resume <id>`, `codex resume <id>`, …). Choose which terminal under **Settings → Terminal**.

The sidebar project row also has **Open in terminal** — opens a terminal in that folder and launches the CLI fresh (a new session, not a resume).

---

## 3. Favorites

**What it is.** A place for individual messages worth keeping. Click the **star** next to any message in Records and it's saved here as a **self-contained snapshot** — full text, role, and timestamp — so it stays readable even if you later delete or change the original session.

**How to use:**

- Star a message in Records → it appears under **Favorites**.
- In the Favorites detail, **Open original session** jumps back to it in Records (if it still exists); **Remove from favorites** deletes the snapshot.
- Favorites are stored locally in `~/.termory/favorites.json`.

---

## 4. Search

**What it is.** Full-text search across the body of every session, memory, and skill Termory has scanned.

**Two ways in:**

- The **Search** page (`⌘4`) — type a query; results are grouped by source with the matching snippet highlighted. Clicking a result opens the record scrolled to the first match.
- The **`⌘K` quick-search palette** — summon it from anywhere (also `⌘F`); same index, keyboard-driven (↑/↓/Enter), with your recent searches listed.

Recent searches are remembered; clear them under **Settings → Search history**.

---

## 5. Stats

**What it is.** Usage analytics over a date range you choose, for the selected source(s). All values are **window-accurate** — they reflect what happened in the chosen range, not lifetime totals.

**How to use:**

1. Pick a range — **Today / Last 7 / 30 / 90 days**, or a **Custom range** (two-month calendar picker).
2. Optionally filter by source.

**What you see:**

- **KPI strip** — Sessions, Messages, Tokens, Models, Projects. Hover **Tokens** for an Input/Output/Reasoning/Cached/Total breakdown; hover **Models** for per-model token usage.
- **Daily tokens** — a trend chart with Input / Output / Cached / Reasoning lines; per-day breakdown on hover.
- **Daily activities** — a 24-hour × date heatmap; cell intensity blends messages and tokens. Hover a cell for that hour's Sessions / Messages / Tokens and per-model usage.

> Model attribution is per-session (each session is attributed to its one recorded model); sessions with no recorded model are hidden.

---

## 6. Settings

The Settings page (`⌘6`) has these sections:

| Section | What it does |
|---------|--------------|
| **Appearance** | Theme — System / Light / Dark. |
| **Language** | English / 简体中文 / 繁體中文. Changes apply immediately. |
| **Startup** | **Launch at login** — start Termory automatically when you log in (tray only, no window). |
| **Terminal** | Which terminal opens when you resume a session. Only terminals found on this machine are listed; "auto" uses your OS default. |
| **Storage** | Shows the `~/.termory/` data directory with an **Open** button. |
| **Search history** | How many recent searches are stored, with a **Clear** button. |
| **Keyboard shortcuts** | A reference list (see below). |
| **About** | App version, **Check for updates**, and an auto-check toggle. |

### Keyboard shortcuts

| Shortcut | Action |
|----------|--------|
| `⌘1`–`⌘6` | Switch rail destination |
| `⌘K` / `⌘F` | Open the quick-search palette |
| `Esc` | Close the palette / a dropdown |

---

## Menu-bar tray (macOS)

The tray lets you act without opening the window:

- **Recent sessions** — up to 5 most recent; click to resume in your terminal.
- **New Session** — open a CLI fresh in a recent project folder, or pick a new folder (**Choose Folder…**).
- **Per-CLI submenus** — switch each CLI's active provider; the official quota shows inline (🟢/🟡/🔴 by pressure).

Closing the window doesn't quit Termory — it keeps running in the tray. Use **Open** to bring the window back, **Exit** to quit fully.

---

## Privacy & your data

**Termory has no servers, no accounts, and no telemetry.** Your history never leaves your machine as part of using the app.

### Where Termory stores its own data

Only `~/.termory/` (on macOS/Linux the directory is `0700`, files `0600` — only your user can read them). Open it from **Settings → Storage**.

| File | Contents |
|------|----------|
| `config.json` | UI preferences. No secrets. |
| `providers.json` | Saved providers and gateways — **contains API keys**. |
| `favorites.json` | Snapshots of starred messages. |

### Does Termory modify my history?

It **reads your history in place** and never changes it — *except* for operations you explicitly trigger, which write to the CLI's own data store:

| Operation | What it changes | Mechanism |
|-----------|-----------------|-----------|
| **Delete** a session / project / memory | Removes that record | File-based CLIs (Claude, Gemini): deletes the files. DB-based CLIs (Codex, OpenCode): deletes the rows (Codex also deletes the rollout files, since the row alone would be rebuilt). |
| **Migrate** a project | Re-points it at a new path | Claude: moves the history folder and rewrites each session's top-level `cwd`. Codex: a metadata rewrite of `cwd` in the rollout file and `threads` table — no files moved. |
| **Keep sessions** on a Codex switch | Re-tags sessions with the new provider | Rewrites `model_provider` in the rollout file and `threads` table. |

**Two things these never touch:** (1) your OAuth login / credential files, and (2) the files in your project working directory (`CLAUDE.md`, `AGENTS.md`, …) — those live in your repo, not the CLI's history store.

### Does anything go over the network?

Only when you ask, and only to endpoints **you** chose:

| Action | Where it connects |
|--------|-------------------|
| Test a provider / fetch its model list | The provider's own base URL |
| Detect a gateway's API modes | The gateway's base URL |
| Show official subscription quota | The official endpoint your CLI already logs in to |
| Check for app updates | GitHub releases |

No analytics, no crash reporting, no background phoning home.

---

## Installation & updates

### macOS: "Termory is damaged and can't be opened"

Builds aren't Apple-notarized, so macOS quarantines the download (common on Apple Silicon). The app is fine — drag it into **Applications**, then clear the quarantine flag once:

```bash
xattr -dr com.apple.quarantine /Applications/Termory.app
```

### Windows / Linux

Windows SmartScreen: **More info → Run anyway**. Linux: use the `.AppImage`, `.deb`, or `.rpm` from [Releases](https://github.com/chats-is/termory/releases).

### Updates

Termory updates in-app: **Settings → About → Check for updates** → **Install now** (it relaunches with the new version). Auto-update only works for installs on a version that shipped the signing key; very old installs need one manual download from the [Releases](https://github.com/chats-is/termory/releases) page, after which in-app updates work.
