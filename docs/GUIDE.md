# Termory — Guide

> 中文版见 [GUIDE.zh-CN.md](GUIDE.zh-CN.md)

Termory brings the history your terminal AI coding tools — **Codex**, **Claude Code**, **Gemini CLI**, **OpenCode**, and **Grok Build** — already store on your machine (sessions, memory, skills) into one window to browse, with no import or setup (it just reads the tools' existing local data). On top of browsing history, it also lets you manage and switch each tool's API providers in a click — including **Claude Desktop**, the GUI app: it keeps no terminal history, but Termory switches it between its official login and third-party Anthropic-compatible providers the same way (macOS / Windows). For installation and downloads, see the [README](../README.md).

## Feature map

Termory has six destinations on the left activity rail (`⌘1`–`⌘6`), plus a macOS menu-bar tray:

| # | Destination | What it does |
|---|-------------|--------------|
| 1 | **[Providers](#1-providers)** | Manage each CLI's API providers and switch the active one; manage AI Gateways; view official quota and account balances; save and switch Codex / Claude Code / Grok Build accounts. |
| 2 | **[Records](#2-records)** | Browse every session, memory file, and skill; resume, migrate, or delete them. |
| 3 | **[Favorites](#3-favorites)** | Messages you've starred, saved as snapshots. |
| 4 | **[Search](#4-search)** | Full-text search across all history, plus a `⌘K` quick-search palette. |
| 5 | **[Stats](#5-stats)** | Overview KPIs + calendar heatmap and a token chart split by type / model over All / 30d / 7d. |
| 6 | **[Settings](#6-settings)** | Appearance, tool toggles, startup, language, terminal, storage, search history, shortcuts, updates. |
| — | **[Menu-bar tray](#menu-bar-tray-macos)** | Resume a session, start a new one, or switch providers without opening the window. |

Two cross-cutting topics — **[Privacy & your data](#privacy--your-data)** and **[Installation & updates](#installation--updates)** — are covered at the end.

---

## 1. Providers

**What it is.** Termory keeps a library of **providers** for each CLI — named configurations for different API platforms, each holding a base URL, API key, and model. A CLI's providers are independent: for Claude Code you might keep an OpenRouter config, a local model, and the official login, and switch which one is active at any time. The **Providers** page (the default landing page) is where you manage them.

### Switching the active provider

1. Open **Providers** and pick the app's tab (Claude Code / Codex / Gemini / OpenCode / Grok Build / Claude Desktop).
2. Click **Activate** (or **Set as default**) on the provider you want.
3. The next time you launch that CLI, it uses the new provider — no manual config editing.

Click **Official** to go back to your native login at any time.

Two app-scoping notes: switching **Codex** applies to the Codex CLI and the Codex desktop app together — since mid-2026 that app is the unified **ChatGPT** desktop app (Chat / Work / Codex), and the two share one config and login. Either install alone is enough: the Codex tab works with just the desktop app, and the Official card's version line shows what's present (e.g. `v0.144.6 (CLI) · v26.715.31925 (App)`), with a "new version" badge on whichever of the two — CLI or app — has an update (the CLI's is clickable to upgrade; see [Updating a CLI](#updating-a-cli)). **Claude Desktop** is its own tab with its own provider library — managed entirely separately from Claude Code.

### Adding or editing a provider

1. Click **Add provider**, give it a **Name**, and fill in **Base URL**, **API key**, and **Model**.
2. The model field auto-suggests available models once the base URL and key are set (you can also type one in).
3. **Test** checks connectivity to the base URL before you rely on it.
4. **Save**. Editing a provider that's currently active re-applies it immediately.

### DeepSeek on Claude Code

Providers → **Claude Code** → **Add provider**:

| Field | Value |
|-------|-------|
| Name | `DeepSeek` |
| Base URL | `https://api.deepseek.com/anthropic` |
| API key | your DeepSeek key |
| Model | `deepseek-v4-pro[1m]` |

Expand **Advanced settings** and point Claude's three model sizes at DeepSeek:

| Key | Value |
|-----|-------|
| `env.ANTHROPIC_DEFAULT_OPUS_MODEL` | `deepseek-v4-pro[1m]` |
| `env.ANTHROPIC_DEFAULT_SONNET_MODEL` | `deepseek-v4-pro[1m]` |
| `env.ANTHROPIC_DEFAULT_HAIKU_MODEL` | `deepseek-v4-flash` |

`[1m]` after a model name asks for the 1M-token context window.

Add any of these if you want them:

| Key | Value | Effect |
|-----|-------|--------|
| `env.CLAUDE_CODE_AUTO_COMPACT_WINDOW` | `786432` | token count at which it auto-compacts |
| `env.CLAUDE_CODE_SUBAGENT_MODEL` | `deepseek-v4-flash` | model used for subagents |
| `env.CLAUDE_CODE_EFFORT_LEVEL` | `max` | reasoning effort |

**Save**, then **Activate**. Start `claude` and you're on DeepSeek; click **Official** to switch back.

### DeepSeek on Codex

Providers → **Codex** → **Add provider**:

| Field | Value |
|-------|-------|
| Name | `DeepSeek` |
| Base URL | `https://api.deepseek.com` |
| API key | your DeepSeek key |
| Model | `deepseek-v4-pro` |

You can also add this under **Advanced settings**:

| Key | Value | Effect |
|-----|-------|--------|
| `model_reasoning_effort` | `high` | reasoning effort — `none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max` / `ultra`, and each provider can have its own |

**Save**, then **Activate**. Start `codex` and you're on DeepSeek; click **Official** to switch back.

### One DeepSeek gateway bound to Claude Code and Codex

One key for both CLIs, instead of adding it twice.

Providers → **AI Gateways** → **Add provider**:

| Field | Value |
|-------|-------|
| Name | `DeepSeek` |
| Base URL | `https://api.deepseek.com` |
| API key | your DeepSeek key |

The base URL is the bare host — no `/v1`, no `/anthropic`; each binding adds the path its own CLI needs.

Under **Apply to tools**, tick **Claude Code** and expand it:

| Field | Value |
|-------|-------|
| Model | `deepseek-v4-pro[1m]` |

In its **Advanced settings**, point Claude's three model sizes at DeepSeek:

| Key | Value |
|-----|-------|
| `env.ANTHROPIC_DEFAULT_OPUS_MODEL` | `deepseek-v4-pro[1m]` |
| `env.ANTHROPIC_DEFAULT_SONNET_MODEL` | `deepseek-v4-pro[1m]` |
| `env.ANTHROPIC_DEFAULT_HAIKU_MODEL` | `deepseek-v4-flash` |

Add any of these if you want them:

| Key | Value | Effect |
|-----|-------|--------|
| `env.CLAUDE_CODE_AUTO_COMPACT_WINDOW` | `786432` | token count at which it auto-compacts |
| `env.CLAUDE_CODE_SUBAGENT_MODEL` | `deepseek-v4-flash` | model used for subagents |
| `env.CLAUDE_CODE_EFFORT_LEVEL` | `max` | reasoning effort |

Then tick **Codex** and expand it:

| Field | Value |
|-------|-------|
| Model | `deepseek-v4-pro` |

Its **Advanced settings** can take this too:

| Key | Value | Effect |
|-----|-------|--------|
| `model_reasoning_effort` | `high` | reasoning effort |

**Create**, then **Activate** each of the two rows. Start `claude` or `codex` and you're on DeepSeek; click **Official** to switch back.

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
| Grok Build | `~/.grok/config.toml` | `~/.grok/auth.json` (the auth.x.ai login) |
| Claude Desktop | `…/Claude{,-3p}/claude_desktop_config.json` + a `Claude-3p/configLibrary/` profile | Claude Desktop's own claude.ai login |

(Codex is the one case where config and a credential share a file — there Termory still *merges*: it sets `auth_mode` + the API key but leaves your `tokens` intact, so the ChatGPT login survives.)

**Switching back to Official is symmetric:** Termory removes the override fields it added and leaves everything else as-is. Because your native login was never overwritten, the CLI resumes using it immediately — no re-login.

### Claude Desktop (the GUI app)

Besides the five CLIs, Termory can switch **Claude Desktop** — the desktop app — to a third-party provider, on **macOS and Windows** (its tab shows everywhere but only activates where the app can run). Claude Desktop has no terminal history, so it appears only on the **Providers** page and **AI Gateways**, never in Sessions/Memory/Skills.

It uses Claude Desktop's own **third-party ("3P")** mechanism, not env vars: Termory flips `deploymentMode` to `3p` and writes a provider profile (your base URL + API key) into Claude Desktop's config library; "Official" flips it back to `1p` and removes the profile. Two specifics differ from the CLIs:

- **The endpoint must be Anthropic-compatible**, and **model IDs must be Claude names** (`claude-sonnet-4-6`, `anthropic/claude-…`; append `[1m]` for the 1M-context variant). Claude Desktop rejects non-Claude model names, so the editor blocks saving them. The model list is optional — leave it empty and Claude Desktop auto-discovers from the endpoint.
- There's **no per-size routing** and no primary "Model" field — just the optional model list plus the generic Advanced settings, which merge into the provider profile.

(This is unrelated to the **AI Gateways** feature below — Claude Desktop is just a provider; "3P" is Claude Desktop's own term for its third-party config.)

### Advanced settings (per provider)

Beyond the basic fields, each provider has an **Advanced settings** section where you add your own config entries — any setting the CLI supports that Termory doesn't have a dedicated field for. Each entry you add is merged into that CLI's config when you activate the provider (for Grok, when you **set it as default** — see below), and removed again when you switch away.

**How to add one:**

1. In the provider editor, expand **Advanced settings**.
2. Click **Add** to get a new row, then fill in the **KEY** and **VALUE**.
3. Add as many rows as you need; use a row's remove button to drop one. **Save** the provider.

**An AI Gateway binding works the same way**: expand a tool's row in the gateway editor and it has its own **Advanced settings**, with the same keys and rules as that CLI's section below (a Claude Code binding is pre-seeded with the same three size-routing rows). A binding's settings belong to that binding alone — the gateway's other tools are unaffected.

The tables below are just common examples — you can add any key/value the CLI accepts. The rules:

- The **key** is a dot-path into the CLI's config — `a.b.c` creates nested structure.
- The **value** is type-inferred for JSON/TOML targets: `true`/`false` → boolean, whole numbers → integer, decimals → float, anything else → string. Two exceptions keep values as literal strings: Gemini's `.env`, and **any Claude Code key under `env.`** — Claude's `env` block is a string-to-string map, so a numeric value like `786432` is written as `"786432"` rather than a JSON number.
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

**OpenCode** → `~/.config/opencode/opencode.json`. OpenCode has two extra dedicated fields above the options — **AI SDK** (the npm package it loads, `@ai-sdk/openai-compatible` by default) and a **Models** list (the model IDs for its picker; at least one is required) with an optional **Default model** chosen from that list. Advanced option keys are relative to the provider's `options` bag:

| Key | Example value | Effect |
|-----|---------------|--------|
| `timeout` | `600000` | request timeout (ms) |
| `headers.X-Token` | `abc123` | custom request header |

*Managed (blocked):* `baseURL`, `apiKey`.

**Grok Build** → `~/.grok/config.toml`. Grok's Advanced settings are grok's own **global** config keys — `ui.compact_mode`, `models.temperature`, `[session]` settings, etc. Because they're global (grok has no per-provider config section), they're applied **only when you set the provider as default** (and removed when you switch away or set Official), so several enabled grok providers never fight over the same keys. *Managed (blocked):* `models.default` and the `[model.*]` entry fields Termory owns.

| Key | Example value | Effect |
|-----|---------------|--------|
| `models.temperature` | `0.7` | sampling temperature for all models |
| `ui.compact_mode` | `true` | compact TUI layout |

### Codex: keeping sessions after a switch

Codex tags every session with the provider active when it was created, and `codex resume` only lists sessions matching the *current* provider — so switching can hide a project's earlier sessions. When you switch Codex between Official and a custom provider, Termory offers to **"Keep earlier sessions?"**: pick the projects whose sessions should follow into the new provider, and Termory re-tags them so `codex resume` keeps listing them. If no project has sessions on the other side, there's nothing to move and it just switches. The other CLIs list resume history by project path and don't need this.

Don't want to be asked? **Settings → Provider switching → Keep all sessions on a Codex switch** makes every project follow automatically, with no prompt — and that's also what lets a Codex switch complete straight from the menu bar without opening the window.

### Account balance

When a provider or gateway points straight at a vendor Termory recognises — DeepSeek, SiliconFlow, StepFun, OpenRouter, Novita — its card shows that account's wallet next to the buttons, e.g. **Balance ¥89.42**, with a refresh button beside it. It is read with the API key you already entered; nothing else is sent, and the request goes to that vendor's own domain.

The row simply isn't there when there is nothing to show — a relay or any host Termory doesn't recognise, a card with no key yet, or a vendor with no balance API — which is most cards. A number that was read once stays put: if a refresh fails, only the button changes. It re-reads at most every 2 minutes, and the menu-bar row carries the same figure for whichever provider a CLI is currently on.

### AI Gateways

A **gateway** is a single `{base URL, API key}` that may speak several API formats at once (OpenAI, Anthropic, Gemini…). Instead of adding the same key separately to each CLI, add the gateway once:

1. Open the **AI Gateways** tab → **Add provider**, and fill in the base URL and API key.
2. Detection runs on its own a moment later — Termory probes which API formats the gateway answers. Under **Apply to tools**, a tool whose format wasn't found stays greyed out; the refresh button re-probes.
3. Tick the tools you want, give each a model, and **Create**. Then **Activate** each row on the gateway's card (one gateway → many CLIs, one key).

For a worked example, see **[One DeepSeek gateway bound to Claude Code and Codex](#one-deepseek-gateway-bound-to-claude-code-and-codex)** above.

Bound gateways also appear in each CLI's provider list (view/activate only — edit them from the Gateways tab).

### Updating a CLI

When a newer version of a CLI is published, an amber **↑ New v0.145.0** badge appears after its version on the Official card. **Click the badge and Termory upgrades that CLI in place** — the badge reads **↑ Upgrading** while it runs, then the version line refreshes and the badge disappears.

Termory runs each CLI's own upgrade command, so it respects however you installed it (npm, Homebrew, the official install script, …):

| CLI | What runs |
|---|---|
| Claude Code | `claude update` |
| Codex | `codex update` |
| OpenCode | `opencode upgrade` |
| Grok Build | `grok update` |
| Gemini | Gemini has no update subcommand, so Termory picks the command matching your install — e.g. `npm install -g @google/gemini-cli` or `brew upgrade gemini-cli` |

The upgrade runs through a login shell, so tools that live in a version manager (nvm, Volta, Homebrew) resolve the same way they do in your terminal.

If it fails, the badge turns **red** and stays that way after the toast disappears, so the card still shows something went wrong. Hover it for the reason plus the exact command, which you can run in your own terminal — useful for the cases Termory can't handle unattended, like a global npm directory that needs `sudo`. Clicking a red badge retries.

**On the Codex tab only the CLI segment is clickable.** The desktop app updates itself (and has no command-line entry point), so its badge is informational — upgrading the CLI never touches it.

### Official quota

For a CLI logged in with an official subscription, the card shows your usage as donut rings (e.g. **5-hour** and **Weekly** windows), color-coded by pressure (🟢 < 75%, 🟡 ≥ 75%, 🔴 ≥ 90%). **Refresh usage** re-fetches it (with a short cooldown). Quota reads your existing official login only — it's hidden while a custom provider is active.

When pay-as-you-go **Usage credits** are enabled on the account (Claude's overflow after you hit a plan limit, or grok on-demand credits), the card adds a **Usage credits** ring showing how much of the spending limit you've used (e.g. `$19.44 / $50.00`). Only the fields the official usage endpoint exposes are shown — the account's promotional-credit balance and auto-reload setting live behind the web billing login and aren't available to the CLI token.

Grok's other billing model shows up as **Balance** — the prepaid credits you bought, drawn down only once your plan allowance is spent. It has no ring: a balance has no limit to divide by, so there's no percentage to draw. It rides along on the menu-bar row too, after the windows and credits.

**When there's nothing to show, nothing is shown.** Some accounts get no usage figures from the endpoint at all — a grok free account is the common case, because its allowance is a rolling 24-hour token window that no API exposes; it's only reported when you hit it, as an error in the CLI itself. In that case the card shows no rings rather than an invented 0%.

**If a refresh fails, the window names turn red** and the last known numbers stay on screen — hover for the reason. Switching accounts clears the figures immediately and re-fetches, so what you see always belongs to the account you're looking at.

### Official accounts (Codex, Claude Code & Grok Build multi-account)

On the **Codex**, **Claude Code** and **Grok Build** tabs, the Official card also manages multiple logins. **Save current** snapshots the live login; **Add account** starts a fresh login in your browser (`codex login` / `claude auth login` / `grok login` — cancellable, and if you cancel or it fails, your previous login is restored). For Codex this works with just the desktop app installed too — Termory runs the CLI bundled inside the app for the login. Grok signs in with a device code: the dialog shows the code alongside the link, so you can check it matches what the browser asks for. Each saved row shows the account's email, plan, and when its tokens were last refreshed. **Switch** restores a snapshot — Termory refreshes its tokens first, and an account whose login has expired is flagged **Re-login** instead of being written broken. Snapshots live in `~/.termory/accounts.json` (owner-only file permissions); the CLI's own credential store is only touched when you switch or add.

Grok Build keeps its login in a plain file, but xAI retires a refresh token every time it is used — so a snapshot taken a while ago can hold one that no longer works. Termory checks with xAI before writing anything: if the saved login has expired you get an error there and then, with the row flagged **Re-login** and your current login untouched, rather than a switch that looks fine and signs you out a few hours later. If xAI simply can't be reached, the snapshot is restored as-is and grok refreshes it itself on next launch. Termory used to have a gap here — switching grok accounts from the terminal rather than from Termory left the snapshot for the account you walked away from out of date, and switching back asked you to log in again. Saved accounts now keep themselves current (below), so that no longer happens.

Claude Code keeps its credential in the macOS Keychain (with `.credentials.json` as the fallback, and the only store on Linux / Windows). Termory reads and writes it through the same `security` command Claude itself uses, so no authorization prompt appears. One practical note: quit any running `claude` sessions before switching — a running instance keeps its config in memory and can write the old identity back over the switch.

Accounts and providers are independent, even for Codex, which keeps both in the same file: switching accounts changes only which official login is active, and never the provider you have selected or its API key. Saving an account records the login alone — a third-party key is never copied into a snapshot, so restoring one later can't bring someone else's key back. This holds the same way whether you switch from the app or the menu bar.

**Saved accounts keep themselves current.** The account a CLI is signed into is not a fixed thing: it refreshes its tokens in the background, you might re-authenticate in a terminal, and your plan can change under it. Termory follows all of that on its own — the copy it holds of the live account is re-read from the CLI's own credential whenever it changes, so switching back to an account later just works, and the plan and email you see are the ones in effect now rather than the ones from the day you saved it. It happens quietly: nothing is announced unless something actually changed, and then only as a brief note in the status corner at the bottom right. Nothing you saved is ever created, removed, or reordered by it — only the account currently in use is touched.

For **Gemini** the card just shows which official account is currently logged in; saving and switching covers Codex, Claude Code and Grok Build.

---

## 2. Records

**What it is.** Records is the history browser. Three panes — **Sessions**, **Memories**, **Skills** — list everything Termory found across all supported tools.

- **Sessions** — chat transcripts from each CLI.
- **Memories** — on-disk memory / instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, per-project memory folders, etc.).
- **Skills** — `SKILL.md` files under each tool's skills directory.

### Browsing

- **Source filter** (Codex / Claude / Gemini / OpenCode / Grok Build / All) in the sidebar narrows all three panes at once.
- The sidebar groups records by **project** (working directory).
- Click any record to open it; the detail pane renders each message the way its own tool shows it (tool calls, diffs, reasoning, etc.).
- Each message has a **copy** button (raw markdown) and a **star** (add to Favorites).
- The detail header's **Open in Finder** reveals the underlying file.

### Right-click actions

Right-click any session, memory, or skill:

- **Reveal in Finder** — open the underlying file.
- **Resume in terminal** / **Copy resume command** — see [resuming](#resuming-a-session) below.
- **Copy path / filename / session ID**.
- **Migrate** — re-point a session or its whole project to a new path (all five CLIs; Gemini is project-level only). Useful after you rename or move a repo, so its history regroups under the new location. (Project-level migrate is also on the sidebar project row.)
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

> One bound worth knowing: a single message longer than 1 MB is truncated when a record is read (you'll see an `… elided` notice in the transcript), and search only sees the part that was kept. In practice this affects a handful of old sessions where a runaway command dumped its entire output into one message — the CLIs themselves now cap that at write time.

**Two ways in:**

- The **Search** page (`⌘4`) — type a query; results are grouped by source with the matching snippet highlighted. Clicking a result opens the record scrolled to the first match.
- The **`⌘K` quick-search palette** — summon it from anywhere; the same search and the same result rows as the Search page, keyboard-driven (↑/↓/Enter), capped to the top 8. A **"View all results"** row at the bottom (or plain Enter when nothing is listed) jumps to the full Search page with your query carried over.

Queries can be as short as **one character** (a single CJK character is a valid query). Opening a result also pre-fills the record's **in-record find** (below) with your query, so you can walk every match, not just the first.

**Find in the open record (`⌘F`)** — with a session or memory/skill doc open in Records, `⌘F` opens a find bar over that record: every occurrence is highlighted, `Enter` / `Shift+Enter` (or ↑/↓ buttons) jump between matches, `Esc` closes. Sessions navigate match-by-message; docs navigate occurrence-by-occurrence.

Recent searches are remembered (shown on the Search page); clear them under **Settings → Search history**.

---

## 5. Stats

**What it is.** Usage analytics over a date range you choose, for the selected source(s). All values are **window-accurate** — they reflect what happened in the chosen range, not lifetime totals.

**How to use:** pick a range — **All / 30d / 7d** — and optionally filter by source. The page shows two stacked blocks:

**Overview** —

- **8 KPI cards** — Sessions, Messages, Total tokens, Active days, Current streak, Longest streak, Peak hour, Favorite model.
- **Calendar heatmap** — one square per day (GitHub-style week columns); intensity blends messages and tokens. Hover a day for its date, messages, and tokens. It always spans your full history, regardless of the range pills.

**Tokens** — one stacked bar chart of daily tokens, with a **Type / Model** toggle (top-left):

- **Type** — split by token kind: Input / Output / Cached / Reasoning.
- **Model** — split by model, colored by provider family: every model of a vendor is a shade of that vendor's hue — Claude clay, Gemini blue, OpenAI teal, Mistral gold, DeepSeek violet, Qwen magenta, xAI/Grok crimson, GLM indigo, MiniMax rose — so a model always reads as its provider (the most-used model is the boldest shade). Models from other (custom / gateway) providers get their own distinct colors. The top models each get a bar, the rest fold into "Others"; the legend lists each model with its input/output tokens and share of the window's usage.

> Both breakdowns add up to the same daily total. Model attribution is per-session (each session is attributed to its one recorded model); sessions with no recorded model are hidden from the KPIs/legend but still counted in the totals.

---

## 6. Settings

The Settings page (`⌘6`) has these sections:

| Section | What it does |
|---------|--------------|
| **Appearance** | Theme — System / Light / Dark. |
| **Tools** | Turn each tool (Claude Code / Claude Desktop / Codex / Gemini / OpenCode / Grok Build) on or off. A disabled tool disappears everywhere — provider tabs, records, search, stats, and the menu bar — the moment you toggle it; its data on disk is untouched and everything comes back when you re-enable it. The last enabled tool can't be turned off. **Gemini is off by default** (Google deprecated Gemini CLI for individual accounts in June 2026) — flip it on if you still use it or want to browse its history. |
| **Provider switching** | **Keep all sessions on a Codex switch** — Codex lists resumable sessions per provider, so moving between Official and a third-party API hides the other side's. Off (default), Termory asks which projects should follow. On, every project follows automatically with no prompt — which also lets a switch from the menu bar finish without opening the window. |
| **Startup** | **Launch at login** — start Termory automatically when you log in (tray only, no window). |
| **Language** | English / 简体中文 / 繁體中文. Changes apply immediately. |
| **Terminal** | Which terminal opens when you resume a session. Only terminals found on this machine are listed; "auto" uses your OS default. |
| **Storage** | Shows the `~/.termory/` data directory with an **Open** button. |
| **Search history** | How many recent searches are stored, with a **Clear** button. |
| **Keyboard shortcuts** | A reference list (see below). |
| **About** | App version, **Check for updates**, and an auto-check toggle. |

### Keyboard shortcuts

| Shortcut | Action |
|----------|--------|
| `⌘1`–`⌘6` | Switch rail destination |
| `⌘K` | Open the quick-search palette |
| `⌘F` | Find in the open record (Records) |
| `Esc` | Close the palette / a dropdown |

---

## Menu-bar tray (macOS)

The tray lets you act without opening the window:

- **Recent sessions** — up to 5 most recent; click to resume in your terminal. A Claude session that's currently running shows its live status next to the title (**· Working** / **· Needs input**).
- **New Session** — open a CLI fresh in a recent project folder, or pick a new folder (**Choose Folder…**).
- **Per-CLI submenus** — switch each CLI's active provider; the official quota shows inline (🟢/🟡/🔴 by pressure) while Official is the active choice.
- **Codex / Claude Code / Grok Build accounts** — your saved logins sit right under **Official** in that CLI's submenu; click one to switch. The live one is checkmarked, and a login whose tokens expired shows **⚠** and can't be picked — re-authenticate on the Providers page first. Switching from here snapshots the current login first, so nothing is lost even if you never pressed **Save current**.
- Switching **Codex** between Official and a third-party provider needs the "which projects should follow?" question, which a menu can't ask — so Termory opens the window for it. Turn on **Settings → Provider switching → Keep all sessions on a Codex switch** and it finishes in the menu bar instead.

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
| **Delete** a session / project / memory | Removes that record | File-based CLIs (Claude, Gemini, Grok): deletes the files. DB-based CLIs (Codex, OpenCode): deletes the rows (Codex also deletes the rollout files, since the row alone would be rebuilt). |
| **Migrate** a project | Re-points it at a new path | Claude & Grok move the on-disk history (the project folder / session dir) and rewrite the stored `cwd`. Codex, Gemini & OpenCode are metadata-only rewrites (rollout `cwd` / `.project_root` marker / DB `directory`) — no files moved. |
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
