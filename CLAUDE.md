# CLAUDE.md

## Scope

This file is for Claude Code working in this repository. Keep it limited to current codebase facts, implementation rules, and verification steps.

## Current App

Tauri v2 desktop app for browsing local history records from supported AI tool clients, plus their on-disk memory and skill files.

Supported sources in the current code:

- Codex
- Claude Code
- Gemini CLI
- OpenCode

Top-level UI shell — six activity-rail routes (⌘1..6 in order):

1. **Providers** — third-party API platform switcher (per CLI). Default landing route.
2. **Records** — chat / transcript history. Three internal panes:
   - *Sessions* — per-tool transcripts
   - *Memories* — on-disk memory files (CLAUDE.md, AGENTS.md, GEMINI.md, `~/.codex/memories/`, `~/.claude/projects/<slug>/memory/`, etc.)
   - *Skills* — `SKILL.md` files under each tool's skills directory plus the cross-tool `.agents/skills/` location
3. **Favorites** — saved per-message snapshots
4. **Search** — substring search + Cmd-K palette
5. **Stats** — KPI strip + Daily tokens chart + Daily activities heatmap
6. **Settings** — Appearance / Storage / Search history / Keyboard shortcuts / About

Plus a **system-level macOS menu-bar tray** (outside the in-app rail) for quick provider switching — see "Menu-bar tray" below.

Current alignment target: data acquisition and message preview formatting should follow the official tools. UI layout, source filters, project grouping, search, stats, cross-source sorting, and the Memory/Skills views are app behavior and do not need official UI parity.

## Tech Stack

- Desktop framework: Tauri v2
- Frontend: React 18, TypeScript, Vite
- UI / styling: Tailwind CSS v4 + shadcn/ui (Radix primitives); charts via `recharts`; virtualized lists via `@tanstack/react-virtual`; theming via `next-themes`; toasts via `sonner`
- Backend: Rust 2021
- Database access: `rusqlite` with bundled SQLite
- JSON parsing: `serde`, `serde_json`
- Filesystem scanning: `walkdir`, `dirs`
- Time handling: `chrono`
- Icons: `lucide-react` plus inline SVG brand icons

## Code Map

- Frontend: `src/main.tsx`
- Styles: `src/styles.css`
- Frontend local-store wrapper: `src/config.ts` (routes `getConfig`/`setConfig` to the three backing files via IPC — `providers` → providers.json, `favorites` → favorites.json, everything else → config.json)
- Tauri IPC commands: `src-tauri/src/lib.rs`
- Session/Memory/Skill scanning, parsing, and formatting: `src-tauri/src/sessions.rs`
- Provider switching (activate / deactivate / reverse-derive / test / fetch-models): `src-tauri/src/providers.rs`
- macOS menu-bar tray (build menu / rebuild / click handler): `src-tauri/src/tray.rs`; icon assets `src-tauri/icons/tray-icon.png` (36×36 template) + `tray-template.svg` (vector source)
- Local KV store (config.json + providers.json + favorites.json under `~/.termory/`, chmod 0600): `src-tauri/src/config.rs`
- Stats aggregations (pure, window-accurate): `src/lib/stats-utils.ts` (+ `stats-utils.test.ts`)
- Stats UI: `src/components/stats/{StatsPage,StatsFilterBar,OverviewHero,DailyTokensChart,DailyActivitiesHeatmap,shared}.tsx`
- Favorites helpers (pure, snapshot-based): `src/lib/favorites.ts` (+ `favorites.test.ts`)
- Favorites UI: `src/components/favorites/FavoritesPage.tsx` (+ `FavoritesPage.test.tsx`); star button + scroll-to-message hook live on `src/components/MessageList.tsx`
- Tauri config: `src-tauri/tauri.conf.json`
- Rust parser/formatter tests: inline tests at the bottom of `src-tauri/src/sessions.rs`
- Rust provider/store tests: inline tests at the bottom of `src-tauri/src/providers.rs` and `src-tauri/src/config.rs`

Current Tauri IPC commands (17), called from the frontend with `invoke(...)`:

**Scan & detail**
- `scan_all_sessions` — returns Sessions + Memory + Skill entries as `AppSession[]`
- `load_session` — loads one entry by `{ source, path, id }`
- `search_all_sessions(query)` — substring search across all loaded session/memory/skill bodies; each `SearchHit` carries `first_match_index` for scroll-to-message

**CLI detection**
- `detect_clis()` — runs `which`-style probes for `claude` / `codex` / `gemini` / `opencode` binaries; returns `{ [app]: boolean }` for the Providers page InstallGuide gate
- `detect_cli_versions_cmd()` — invokes each detected CLI with `--version` so the Providers page can show actual installed versions

**Providers — switch CLI to a third-party platform**
- `provider_active_state(app, providers)` / `provider_active_states(providers)` — reverse-derive which Provider is currently active by reading the CLI's live config files; nothing about "active" is stored backend-side
- `activate_provider(provider, providersForApp)` / `deactivate_provider(app, providersForApp)` — materialize a Provider into the CLI's live config (or clear Termory-injected fields)
- `delete_provider(provider)` — remove a stored Provider AND deactivate it if it was the active one
- `set_opencode_default_provider(provider)` — OpenCode-specific: pick which of multiple configured `auth.json` entries opencode uses by default
- `test_provider_api(provider)` — connectivity probe to the provider's base URL (returns `{ ok, status, latencyMs, message }`)
- `fetch_provider_models(provider)` — hits `/v1/models` (or `/v1beta/models?key=` for Gemini) and returns the available model ids for the editor's autocomplete
- `fetch_provider_favicon(url)` — proxies a one-shot `<host>/favicon.*` fetch through the backend (avoids leaking the hostname to a third-party favicon service); returns a `data:image/...;base64,...` URL cached on the Provider record

**App-local KV stores** (all `chmod 0600` on Unix)
- `read_app_config` / `write_app_config` — `~/.termory/config.json` (UI prefs)
- `read_app_providers` / `write_app_providers` — `~/.termory/providers.json` (provider library, contains API keys)
- `read_app_favorites` / `write_app_favorites` — `~/.termory/favorites.json` (saved message snapshots, may contain PII / pasted secrets)

## Project Commands

- Package manager: npm (`package-lock.json` is present).
- Web dev server: `npm run dev`
- Tauri dev app: `npm run tauri:dev`
- Frontend production build: `npm run build`
- Tauri bundle build: `npm run tauri:build`
- Rust tests: `cargo test --manifest-path src-tauri/Cargo.toml --lib`
- Rust format: `cd src-tauri && cargo fmt`

The Tauri binary is renamed via `[[bin]] name = "Termory"` in `src-tauri/Cargo.toml` plus `mainBinaryName: "Termory"` in `tauri.conf.json`, so the macOS menu bar shows "Termory" rather than the lowercase Cargo package name.

## Release & Updater

GitHub Actions workflow at `.github/workflows/release.yml` is triggered when a `v*` tag is pushed (e.g. `git tag v0.2.0 && git push --tags`) or manually via `workflow_dispatch`. It uses `tauri-apps/tauri-action@v0` and builds installers for:

- macOS Apple Silicon (`aarch64-apple-darwin`)
- macOS Intel (`x86_64-apple-darwin`)
- Linux x86_64 (`x86_64-unknown-linux-gnu`) — with apt deps for webkit2gtk-4.1, soup-3.0, javascriptcoregtk-4.1, etc.
- Windows x86_64 (`x86_64-pc-windows-msvc`)

A draft GitHub Release is created with the platform installers attached plus `latest.json` (the updater manifest). Review & publish the draft.

### In-app updater (`tauri-plugin-updater`)

- Plugin registered in `lib.rs`: `tauri_plugin_updater::Builder::new().build()` + `tauri_plugin_process::init()` for `relaunch()`.
- `tauri.conf.json` declares `bundle.createUpdaterArtifacts: true` and `plugins.updater.endpoints` pointing at `https://github.com/copilot-is/termory/releases/latest/download/latest.json`.
- `capabilities/default.json` grants `updater:default` + `process:default`.
- Frontend: `Settings` page exposes "Check for updates" → `@tauri-apps/plugin-updater::check()` → "Download and install" → `update.downloadAndInstall()` → `@tauri-apps/plugin-process::relaunch()`.

### One-time signing key setup (required before first signed release)

The updater only installs artifacts whose signature matches a pubkey baked into the binary. Without a keypair, in-app updates won't work (but the GitHub Actions builds still produce installers — users can download manually).

```sh
# Generates ~/.tauri/termory.key (private, password-protected) + termory.key.pub
npx @tauri-apps/cli signer generate -w ~/.tauri/termory.key
```

Then:

1. Paste the contents of `~/.tauri/termory.key.pub` into `tauri.conf.json` `plugins.updater.pubkey`.
2. Add GitHub repo secrets:
   - `TAURI_SIGNING_PRIVATE_KEY` = contents of `~/.tauri/termory.key` (the private file)
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` = the password you set during generation
3. Tag and push: `git tag v0.2.0 && git push --tags` — the workflow builds, signs, and publishes a draft release.

Bump `src-tauri/tauri.conf.json` `version` and `src-tauri/Cargo.toml` `version` together before tagging so the binary self-reports the right version.

macOS bundle identifier: `is.chats.termory` (reverse DNS of the `chats.is` domain the project ships under). Do NOT change this after a public release — macOS treats a different identifier as a different app, so existing user data and the Tauri updater would break.

The repo also contains `.audit-sources/` (gitignored) with shallow clones of `openai/codex`, `google-gemini/gemini-cli`, `sst/opencode`, `videcoding/cli` (legacy Claude Code reference), and `farion1231/cc-switch` (reference for provider-switcher behavior). This is the source-of-truth for path/behavior verification when official docs disagree with implementation — grep here instead of WebFetching docs.

## Upstream References

Use upstream implementations as the reference for history data and message preview behavior:

- Codex official source: https://github.com/openai/codex
- Claude Code referenced CLI implementation: https://github.com/videcoding/cli
- Gemini CLI official source: https://github.com/google-gemini/gemini-cli
- OpenCode official source: https://github.com/anomalyco/opencode

For Memory paths:

- Claude Code memory: https://code.claude.com/docs/en/memory
- Codex AGENTS.md guide: https://developers.openai.com/codex/guides/agents-md
- Codex memories: https://developers.openai.com/codex/memories
- Gemini GEMINI.md: https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/gemini-md.md
- Gemini memory tool: https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/memory.md
- OpenCode rules: https://opencode.ai/docs/rules/

For Skills paths:

- Codex skills docs: https://developers.openai.com/codex/skills
- Gemini CLI skills docs: https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/skills.md
- OpenCode skills docs: https://opencode.ai/docs/skills/

For TUI tool-message rendering (every Termory branch cites a line here):

- Codex exec/shell render: `.audit-sources/codex/codex-rs/tui/src/exec_cell/render.rs`, bash highlight alias at `codex-rs/tui/src/render/highlight.rs:533`
- Claude tool-use wrapper: `.audit-sources/claude-code/src/components/messages/AssistantToolUseMessage.tsx:152` (assembles `<bold>{userFacingName}</bold>({renderToolUseMessage})`); per-tool: `src/tools/<Tool>/UI.tsx` (`userFacingName` + `renderToolUseMessage`)
- Gemini ToolInfo render: `.audit-sources/gemini-cli/packages/cli/src/ui/components/messages/ToolShared.tsx:202`; type at `packages/cli/src/ui/types.ts:119` `IndividualToolCallDisplay`
- OpenCode tool components: `.audit-sources/opencode/packages/opencode/src/cli/cmd/tui/feature-plugins/system/session-v2.tsx` (Bash l.707, Glob l.748, Read l.764, Grep l.794, WebFetch l.810, WebSearch l.818, Write l.828, Edit l.857, ApplyPatch l.891, TodoWrite l.964, Question l.991, Skill l.1022, Task l.1030, generic l.522, BlockTool helper l.659, InlineTool helper l.559)

When behavior differs by version, match the locally installed or explicitly requested target version and cover it with a focused test. Tool-message rendering should reference the TUI source files above, not the doc sites — docs lag behind the actual UI for many of these tools.

## Current Data Sources

### Sessions

- Codex
  - List: `~/.codex/state_5.sqlite`, table `threads`, rows where `archived = 0` AND `preview <> ''` AND `source IN ('cli', 'vscode', 'atlas', 'chatgpt')`. The four sources match `INTERACTIVE_SESSION_SOURCES` in `codex-rs/rollout/src/lib.rs`; the `preview <> ''` clause matches `push_thread_filters` in `codex-rs/state/src/runtime/threads.rs`. Same filter is applied when loading a single thread by id.
  - Messages: each selected thread's `threads.rollout_path` JSONL file.
- Claude Code
  - List: `CLAUDE_CONFIG_DIR/projects/**/*.jsonl` when `CLAUDE_CONFIG_DIR` is set, otherwise `~/.claude/projects/**/*.jsonl`. Filename must be a UUID (`is_uuid_like`), first line must NOT contain `"isSidechain":true`, and the session must have at least one of customTitle/aiTitle/lastPrompt/summary/firstPrompt — same filter as videcoding/cli `parseSessionInfoFromLite`.
  - Messages: the selected project JSONL file.
- Gemini CLI
  - List: `~/.gemini/tmp/*/chats/session-*.jsonl` and `~/.gemini/tmp/*/chats/session-*.json`. Sessions must have a non-empty `sessionId`, `hasUserOrAssistantMessage`, and `kind !== 'subagent'`. When `startTime`/`lastUpdated` are missing on the record, Termory falls back to the file's mtime (then to `Utc::now()`) — mirrors `getAllSessionFiles` in `packages/cli/src/utils/sessionUtils.ts`.
  - Project path: sibling/related `.project_root` file under the Gemini temp project directory.
  - Messages: the selected session JSONL/JSON chat file.
- OpenCode
  - List: `~/.local/share/opencode/opencode.db`, table `session`, rows where `parent_id IS NULL` and `time_archived IS NULL`, ordered by `time_updated DESC, id DESC`. Mirrors `listByProject` in `packages/opencode/src/session/session.ts`.
  - Messages: `~/.local/share/opencode/opencode.db`, tables `message` and `part`; `session_message` is only a fallback when `message`/`part` are unavailable (a real compat path for older databases — `session_message` is otherwise the projections table per `projectors-next.ts`).
  - Compatibility storage: JSON files under `~/.local/share/opencode/**/storage`; use only for older/alternate local layouts and never before the current SQLite path.

Read source history in place. Do not modify original history files or databases.

### Memory

Verified against each tool's open-source implementation (not just docs). When docs and source disagree, source is authoritative. See Upstream References for source URLs.

- Claude Code: `~/.claude/projects/<sanitized-canonical-git-root>/memory/**/*.md` (auto-memory per project — `src/memdir/paths.ts` uses `findCanonicalGitRoot` so worktrees of the same repo share one dir), `~/.claude/rules/**/*.md` (global rules), `<cwd>/.claude/rules/**/*.md` (project rules — both recursive, all `.md`)
- Codex: `~/.codex/memories/**/*.md` — `scan_codex_memory` skips the `skills/` subdir for backward compatibility (current Codex source stores skills at `~/.codex/skills/`)
- Gemini CLI:
  - Global: `~/.gemini/GEMINI.md` (legacy) AND `~/.gemini/MEMORY.md` (modern alias — `getAllGeminiMdFilenames()` returns both)
  - Per-project: `~/.gemini/tmp/<id>/memory/{MEMORY.md preferred, GEMINI.md legacy}` — confirmed at `packages/core/src/config/storage.ts getProjectMemoryDir()` → `getProjectMemoryTempDir() = path.join(globalTempDir, projectIdentifier, 'memory')`. Termory recursively reads .md inside, skipping the `skills/` subdir which is surfaced under Skills.
- Per-project instruction files — scanned at the cwd AND, **only when a `.git` directory exists at or above cwd**, every ancestor up to and including the git root (stopping before `$HOME`):
  - `CLAUDE.md` → tag `claude,opencode` (OpenCode officially falls back to it)
  - `CLAUDE.local.md` → tag `claude`
  - `AGENTS.md` → tag `codex,opencode`
  - `AGENTS.override.md` → tag `codex` (Codex's official override file)
  - `GEMINI.md` → tag `gemini`
  - `MEMORY.md` → tag `gemini`
  - `<cwd>/.claude/CLAUDE.md` → tag `claude` (only at cwd, not at ancestors — `.claude/CLAUDE.md` is a project-root convention)
- Global instruction files:
  - `~/.claude/CLAUDE.md` → tag `claude,opencode`
  - `~/.codex/AGENTS.md`, `~/.codex/AGENTS.override.md` → tag `codex`
  - `~/.config/opencode/AGENTS.md` → tag `opencode`

Paths intentionally NOT scanned (no current source reads them):

- `AGENTS.local.md` (any location) — not in any tool's source; Codex uses `AGENTS.override.md`
- `~/.codex/instructions.md` — legacy
- `~/.claude/CLAUDE.local.md` — not documented at user level
- `CONTEXT.md` — OpenCode deprecated, intentionally skipped
- `project_doc_fallback_filenames` from `~/.codex/config.toml` — Termory does not read user config

### Why ancestor walk gates on `.git`

All three open-source tools refuse to ascend without a project-root marker:

- **Codex** (`codex-rs/core/src/agents_md.rs`): `DEFAULT_PROJECT_ROOT_MARKERS = &[".git"]`. The doc-comment on the loader: *"If no marker is found, only the current working directory is considered."*
- **Gemini** (`packages/core/src/utils/memoryDiscovery.ts findProjectRoot`): defaults to `['.git']`. When no marker is found, returns null → caller sets `ceiling = startDir` → `findUpwardGeminiFiles` breaks immediately on the start dir.
- **OpenCode** (`packages/opencode/src/project/project.ts`): `worktree` is resolved via `git rev-parse --git-common-dir`; without git the fallback sets `worktree: sandbox` (= cwd), so `Filesystem.findUp(start=cwd, stop=cwd)` collects only cwd.

Claude Code (the only one NOT gating on `.git` — its `attachments.ts` walks to fs root) is the outlier; for simplicity we apply the stricter (more common) rule. This is a known minor mismatch documented in [`codex-ancestor-walk-rule`](memory).

The implementation lives in `scan_memory`:

1. `push_project_root_instruction_files(cwd, ...)` always runs (cwd-level files including `.claude/CLAUDE.md`).
2. `find_git_root(cwd, home)` walks up looking for `.git`. Returns `Some(dir)` or `None`.
3. If `Some(git_root)` and `git_root != cwd`, walk from `cwd.parent()` up to and including `git_root`, calling `push_ancestor_instruction_files` at each level (omits `.claude/CLAUDE.md`).
4. The final dedup-by-path keeps each file's first label; ancestor files get their own ancestor dir as the project label.

### Skills

Source-verified locations:

| Tool | Global | Project | Tag |
|---|---|---|---|
| Claude Code | `~/.claude/skills/` | `<cwd>/.claude/skills/` | `claude,opencode` (OpenCode officially also reads `.claude/skills/`) |
| Codex | `~/.codex/skills/` (NOT `~/.codex/memories/skills/`) | `<cwd>/.codex/skills/` | `codex` |
| Gemini CLI | `~/.gemini/skills/` (`Storage.getUserSkillsDir`) | `~/.gemini/tmp/<id>/memory/skills/` (`Storage.getProjectSkillsMemoryDir`) + `<cwd>/.gemini/skills/` | `gemini` |
| OpenCode | `~/.config/opencode/skills/` | `<cwd>/.opencode/skills/` | `opencode` |
| Tool-neutral | `~/.agents/skills/` (`Storage.getUserAgentSkillsDir`) | `<cwd>/.agents/skills/` | `codex,gemini,opencode` (officially supported by all three) |

Implementation notes:

- All skill scanners route through `push_doc_files_recursive(dir, base, project, tag, source="Skill", skip_dirs=&[], out)`.
- `doc_session_from_file(path, project, source)` is shared between Memory and Skill scanners; the `source` field on `AppSession` is `"Memory"` or `"Skill"` accordingly.
- `parse_doc_file(path, source)` handles loading either kind in `get_session`.
- `derive_memory_project_label` recognizes both memory and skill on-disk paths (including `.agents/skills/`) so loading a single file by absolute path produces a sensible project label.

## Current Implementation Notes

- `scan_all_sessions` calls Rust scanning on a blocking worker and returns sessions, memories, and skills in one list (distinguished by `source`).
- `load_session` loads one selected record by `source`, `path`, and `id`.
- **Records page perf** (`App.tsx` detail effect + `src/components/MessageList.tsx`):
  - Detail load `useEffect` depends on `selected.source / path / id / updated_at / message_count` — narrow per-field deps instead of the whole `selected` object, so watcher-driven `applyScanResult` rebuilds that hand back a new `AppSession` reference with the same identity don't re-trigger `load_session`. A `prevSelectedKeyRef` distinguishes "new selection" (show `Loader2`) from "same selection, content advanced" (silent swap via `React.startTransition(setDetail)`).
  - `applyScanResult` no longer auto-selects `result[0]` on first launch — Records opens with an empty detail pane so app boot doesn't pay the cost of parsing the most recent session up front.
  - `setRoute` is wrapped in `React.useTransition` so clicking a rail icon (especially leaving a long Records detail) doesn't get blocked by the heavy unmount + re-render of the next route.
  - The detail pane renders messages through `<MessageList>` (`@tanstack/react-virtual`) which keeps roughly the visible window + 6-row overscan in the DOM regardless of session length. `measureElement` reports actual rendered heights so scrolling and the scrollbar stay accurate across variable-length message cards.
  - The `!selected && sessions.length === 0 && loading` "Scanning…" empty state was removed — initial boot just shows "Nothing to view yet" until data arrives, no transient spinner flash.
- `AppSession.preview` carries comma-separated tool tags (e.g. `"codex,opencode"` for AGENTS.md). The list-card `MemoryCard` renders one brand badge per tag via `memoryToolsOf()`; the detail-header badge renders a single type label (`Session` / `Memory` / `Skill`) via `typeLabelOf()`.
- For session-type entries the detail header shows the GUID (`selected.id`) on its own line below the project path, rendered as inline monospace via Tailwind (`font-mono` / `text-xs` / `text-muted-foreground`). Memory/Skill entries omit the GUID line.
- Project-level `AGENTS.md` and `AGENTS.override.md` are always tagged with both `codex` and `opencode` regardless of which tool actually has sessions in the cwd. Rationale: the AGENTS.md spec is tool-neutral — Termory reports which tools CAN read the file, not which tool happened to run there. Verified by `scan_memory_always_tags_project_agents_md_with_both_codex_and_opencode` test.
- Sidebar source filter (Codex/Claude/Gemini/OpenCode/All) applies to **all three** panes (Sessions, Memory, Skills). Memory and Skill filtering goes through `memoryToolsOf(item).includes(source as MemoryTool)`, so multi-tagged files (AGENTS.md with `codex,opencode`) appear under both Codex and OpenCode filters.
- Session list cards currently show source, date, title, project, and message count.
- `message_count` is an app-derived visible parsed message count when the official list does not provide the same count directly.
- Empty or missing official titles should stay empty unless the official tool has the same fallback.
- Brand identity lives in the `<BrandIcon>` SVG component (`src/components/BrandIcon.tsx`), one branch per `source` literal. Colors are baked into the SVG `fill` attribute (no CSS class indirection after the Tailwind v4 migration). Current colors:
  - `Codex` and `OpenCode` use `#111827` (slate-900 — both render the same dark glyph; cards distinguish them via the icon shape, not color)
  - `Claude` uses `#d97745` (Anthropic Clay)
  - `Gemini` uses an inline `<linearGradient>` defined in the same SVG: `#4285f4` → `#a142f4` → `#34a853` (Google blue/purple/green)
- There is no per-CLI CSS class (no `.badge.codex` etc.) — list-card "badges" are just `<BrandIcon source="…">` instances. To resize, callers pass `className="size-12"` etc., merged into the SVG via `cn()` inside `BrandIcon`. The legacy `Session` / `Memory` / `Skill` color pills are gone from the detail header (the source / type is implied by the rest of the card content); `typeLabelOf()` is still used by the Cmd-K command palette to label results.

### Unified tool-message format — LOCKED RULE

Every tool message — regardless of source platform — uses the same markdown shape. This is a hard rule: any new tool / structured-result formatter MUST follow it without exception.

**Shape:**

`````
{status} **{Verb}**({args})

⎿ {summary}              ← present when the tool has a structured summary
                           (line counts, status codes, settings, etc.)

```{lang}                ← optional fence for diff / source / structured output
{body}
```

or

````                     ← 4-backtick fence for unstructured text output
{body}                     (avoids collision with content containing ```)
````
`````

**Rules:**

1. `{status}` glyph: `⏺` success, `✗` failure (Claude `constants/figures.ts:4` + Codex `exec_cell/render.rs:236`). Cross-platform — applied to every tool card.
2. `{Verb}` text is platform-native (Claude `userFacingName`, OpenCode `session-v2.tsx`, Codex `exec_cell/render.rs`, Gemini `displayName`); the wrapper shape `**Verb**(args)` is identical across platforms.
3. `{args}` always passes through `wrap_inline_code` (sessions.rs:48) so embedded backticks / `*` / `()` don't break markdown.
4. **`⎿ ` prefix is REQUIRED on every summary line**, with one trailing space before the content. Tools without a structured summary skip the line entirely (Bash, generic MCP, etc.). NEVER put `⎿` inside a code fence — browser monospace fonts render U+23BF inconsistently, breaking column alignment.
5. Summary content matches the per-tool Claude TUI component verbatim (count bolding, label pluralization). Examples:
   - `⎿ Read **N** lines` — `FileReadTool/UI.tsx:138-139`
   - `⎿ Wrote **N** lines to **{file}**` — `FileWriteTool/UI.tsx:58`
   - `⎿ Found **N** {label}` — `GrepTool/UI.tsx:32-33`
   - `⎿ Added **N** lines, removed **M** lines` — `FileEditToolUpdatedMessage.tsx:42-50`
   - `⎿ Received **{size}** ({code} {codeText})` — `WebFetchTool/UI.tsx:31-58`
   - `⎿ Error: Exit code N` (Codex) / `⎿ Error: {message}` (Claude — no exit code)
6. Reasoning across all platforms: `format_reasoning_body(content)` → `> *line*` italic blockquote.
7. **No content is dropped to match official TUI behavior.** Suppressed tools (Claude `TodoWrite`, `AskUserQuestion`, `EnterPlanMode`, etc.) and synthetic / contextual wrappers (Codex `<environment_context>`, OpenCode `synthetic` parts, Claude `<tick>` / `<local-command-caveat>` / NULL_RENDERING attachments) all surface in Termory — usually as italic `*[wrapper-name]*` notices when there's no structured representation. Termory is a history browser; hiding things misleads the user.

**Failure detection per platform** (`SessionMessage.exit_code: Option<i64>` carries the parsed value through `merge_tool_outputs`):

| Platform | Signal source | Notes |
|---|---|---|
| Codex | `Process exited with code N` / `Exit code: N` in the `function_call_output.output` wrapper (`ExecCommandToolOutput.response_text()` — context.rs:409) parsed by `codex_parse_exec_output` | Limited mode default; populates `exit_code` |
| Claude | `tool_result.is_error: true` content block | No exit code field — `Error:` prefix has no `Exit code N` part |
| OpenCode | `state.status === "error"` on a tool part (and `state.error.message` for the text); `assistant.error` for whole-message failures (`SessionErrorUnknown` shape per types.gen.ts:2905) | Body gets `✗ ` marker + 4-backtick `Error: {message}` fence; kind = `tool_error` so future UI can colour the card |
| Gemini | `status` field on each `toolCalls[]` entry; per `sessionUtils.ts:654-657` anything other than `'success'` (e.g. `'error'`, `'cancelled'`) maps to `CoreToolCallStatus.Error` | No exit code; body gets an `Error:` prefix |

### Per-platform verb mapping

Every Termory branch cites the exact source file that produces the verb in each TUI. Survey under `.audit-sources/{codex,gemini-cli,opencode,claude-code}/`.

**Codex** (`codex_function_call_message` for ResponseItem::FunctionCall, `codex_custom_tool_call_message` for ResponseItem::CustomToolCall) — `.audit-sources/codex/codex-rs/tui/src/exec_cell/render.rs:381-385`:

- `exec_command` / `shell` / `shell_command` / `local_shell` (all 4 names per `rollout-trace/src/tool_dispatch.rs:263`) → `**Bash**({wrap_inline_code(cmd)})` — verb unified with Claude per design call (was `**Ran** \`cmd\`` before unification).
- `apply_patch` → `**{Verb}**({wrap_inline_code(path)})\n\n```diff\n{patch}\n```` ` — `codex_parse_patch_actions` scans `*** Add File:` / `*** Delete File:` / `*** Update File:` markers, picks `Added` / `Deleted` / `Edited` per `diff_render.rs:421-436`. Multi-file patches collapse to `**Edited**({N} files)`. Modern Codex stores apply_patch as `payload.type = "custom_tool_call"` with `input` field (raw patch text); legacy form is `function_call` with `arguments`. Both shapes route to the same patch-header builder.
- `update_plan` → `**Updated Plan**` + optional `*explanation*` + GFM task list `- [x]/[~]/[ ]` (matches PlanUpdateCell at `history_cell/plans.rs:138-194` — TUI uses ✔/□ symbols with crossed-out / bold / dim styling; Termory stays on GFM markers so checkboxes render natively in react-markdown)
- `view_image` → `**Viewed Image**({wrap_inline_code(path)})` (patches.rs:63-72 — capital `I` per TUI)
- other → `**{name}**({compact args})` fallback

**Codex EventMsg dispatch** (`codex_event_msg_to_message`) — `RolloutItem::EventMsg` records are the canonical replay source for Codex; the wrapper `codex_message_from_value` routes `event_msg` records here. Handled variants:

- `user_message` / `agent_message` / `agent_reasoning` / `agent_reasoning_raw_content`
- `web_search_end` → `**Searched**({wrap_inline_code(detail)})` where `detail` follows Codex's `web_search_action_detail` (search.rs:13-38): `query` for `Search` (or first of `queries` with ` ...` suffix when multiple), `url` for `OpenPage`, `'pattern' in url` / `'pattern'` / `url` for `FindInPage`
- `mcp_tool_call_end` → `**MCP**({server}.{tool})` (dot separator per Codex `format_mcp_invocation` mcp.rs:761-780); when `arguments` is a non-empty / non-`null` JSON value, appends `, {compact_json}` inside the parens
- `image_generation_end` → `**Generated Image**({wrap_inline_code(prompt)})` + saved path (capital `I` per TUI patches.rs:74-93)
- `view_image_tool_call` → same shape as the function_call variant
- `plan_update` → same as the function_call `update_plan` (payload IS the UpdatePlanArgs)
- `patch_apply_end` (Extended mode) → per-file `**Verb**({path})` lines; on failure appends stderr fence + `**Error**`
- `context_compacted` → `*Context compacted*` system notice
- `error` → `**Error**: {message}` system notice
- `turn_aborted` → `*Turn interrupted by user*` / `*Turn stopped — budget limit reached*`
- `thread_rolled_back` → `*Rolled back N turn(s)*`
- `entered_review_mode` / `exited_review_mode` → italic notices

**Codex `custom_tool_call` / `custom_tool_call_output`** (`codex_custom_tool_call_message` / `codex_custom_tool_call_output_message`) — modern shape for apply_patch and similar tools, differs from `function_call`:
* input arrives in an `input` field (raw text) instead of `arguments` (JSON-encoded args)
* output is wrapped in a JSON envelope `{"output":"..."}` — the message handler unwraps `output` / `text` / `result` keys, falling back to raw on parse failure

Without these handlers, modern apply_patch was silently dropped and no ```diff fence was emitted.

`exec_command_end` (Extended-mode shell) is intentionally NOT dispatched yet — it would duplicate the ResponseItem-derived card. Need call_id-based dedup before enabling.

`Limited` vs `Extended` mode (per `codex-rs/rollout/src/policy.rs:135-153`): the CLI default is Limited (`tui/src/app_server_session.rs: persist_extended_history: false`), so most rollouts only carry `ResponseItem::FunctionCall` + `FunctionCallOutput` for shell tools — NOT `EventMsg::ExecCommandEnd`. Termory's `codex_function_call_output_message` is the authoritative path for shell output in that mode; `codex_parse_exec_output` strips the wrapper to recover `aggregated_output`.

**Claude Code** (`claude_tool_use_text`) — `.audit-sources/claude-code/src/components/messages/AssistantToolUseMessage.tsx:152` wraps `<bold>{userFacingName}</bold>({renderToolUseMessage})`. Each Tool's `UI.tsx` provides both pieces. All argument values pass through `wrap_inline_code` so markdown-special chars in user payloads can't leak.

`claude_tool_use_text` returns `Option<String>` so tools that Claude TUI explicitly suppresses (`userFacingName: ''` AND `renderToolUseMessage: () => null`) can return `None` and the entire tool card is skipped — matching the TUI which renders nothing for them:

| Raw name | userFacingName source | Termory output |
|---|---|---|
| `Bash` | BashTool/UI.tsx | `**Bash**({command})` (empty cmd → just `**Bash**`) |
| `Read` / `View` | FileReadTool/UI.tsx:179 → "Read" / "Read agent output" (path matches `/tasks/{taskId}.output` per `getAgentOutputTaskId`); `getPlansDirectory` "Reading Plan" variant is intentionally skipped (depends on session config) | `**Read**({path} · lines X-Y / · pages N / · limit N)` / `**Read agent output**({taskId})` |
| `Write` | FileWriteTool/UI.tsx → "Write" | `**Write**({path})` |
| `Edit` / `MultiEdit` / `str_replace*` | FileEditTool/UI.tsx:28-87 → "Update" by default, "Create" when `old_string === ''` (or first edit's `old_string === ''` for MultiEdit) | `**Update**({path})` / `**Create**({path})` |
| `Grep` | GrepTool.ts:170 → "Search" | `**Search**(pattern: ..., path: ...)` |
| `Glob` | GlobTool/UI.tsx:13 → "Search" | `**Search**(pattern: ..., path: ...)` |
| `WebFetch` | WebFetchTool.ts:81 → "Fetch" | `**Fetch**({url})` |
| `WebSearch` | WebSearchTool.ts:160 → "Web Search" (space) | `**Web Search**({query})` |
| `NotebookEdit` | NotebookEditTool.ts → "Edit Notebook" | `**Edit Notebook**({notebook_path})` |
| `Task` / `Agent` | AgentTool/UI.tsx `userFacingName` — "Agent" for `worker`/`general-purpose`/missing subagent_type, else the subagent_type verbatim | `**{verb}**({description})` (`verb` = "Agent" or `subagent_type`) |
| `Skill` | SkillTool/UI.tsx | `**Skill**({name})` |
| `ReadMcpResource` | ReadMcpResourceTool/UI.tsx → literal **`readMcpResource`** (camelCase, NOT title-cased) | `**readMcpResource**({uri})` |
| `ListMcpResources` | literal **`listMcpResources`** | `**listMcpResources**({server})` |
| `McpAuth` | McpAuthTool.ts → literal `'{server} - authenticate (MCP)'` (the whole label IS the verb) | `**{server} - authenticate (MCP)**` |
| `mcp__{server}__{tool}` (generic MCP) | — | `**MCP**({server}/{tool})` (matches Codex MCP) |
| **SUPPRESSED in Claude TUI** — `TodoWrite` / `AskUserQuestion` / `EnterPlanMode` / `ExitPlanMode` / `ExitPlanModeV2` / `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList` / `TaskStop` / `TaskOutput` / `ToolSearch` | userFacingName `''` AND renderToolUseMessage returns null | `claude_tool_use_text` returns `None` → no tool card emitted at all |

**Claude content blocks** beyond `text` / `tool_use`:

- `thinking` and `redacted_thinking` → reasoning message via `claude_thinking_blocks` + `format_reasoning_body`. Claude TUI renders `∴ Thinking…` (AssistantThinkingMessage.tsx); Termory emits the unified `> *content*` blockquote instead.
- `image` (`{source: {type: "base64"|"url", media_type, ...}}`) → italic `*Image ({mime})*` or `*Image: {url}*` notice via `claude_image_part_label`.
- `tool_result.content` may be `Value::String` or `Value::Array` of `text` blocks. For `Edit` / `MultiEdit` / `Write` tools, Termory prefers the structured diff over the brief tool_result text — `claude_format_structured_patch` reads the JSONL line's sibling `toolUseResult.structuredPatch` field (the same data Claude TUI's `StructuredDiff.tsx` consumes) and emits a `**Added N lines, removed M lines**` summary header on its own line, then a ```diff fence with the actual hunks. NO `@@ -X,N +Y,M @@` text in the fence — Claude's `formatDiff` (StructuredDiff/Fallback.tsx:373-440) conveys hunk boundaries via gutter line-number jumps, not the unified-diff header. Multi-hunk patches get a blank line between hunks instead.

`claude_display_text` strips / rewrites the following Claude-internal text wrappers and constants (per `UserTextMessage.tsx:40-197` dispatch chain and `constants/messages.ts`):

| Wrapper / signal | Claude TUI | Termory output |
|---|---|---|
| `(no content)` (NO_CONTENT_MESSAGE) | null (UserTextMessage.tsx:48) | drop |
| `[Request interrupted by user]` / `[Request interrupted by user for tool use]` | `<InterruptedByUser>` italic (l.83-92) | `*[Interrupted by user]*` |
| `<tick>...</tick>` | null (l.57-59) | drop |
| `<local-command-caveat>...` | null (l.61-64) | drop |
| `<bash-stdout>...` / `<bash-stderr>...` | `<UserBashOutputMessage>` → stdout + stderr (l.66-71) | unwrapped + concatenated (stdout then `\n\n` + stderr); inner `<persisted-output>` also stripped |
| `<local-command-stdout>` / `<local-command-stderr>` | `<UserLocalCommandOutputMessage>` indented w/ Markdown (l.74-79) | inner text passed through |
| `<bash-input>...` | `<UserBashInputMessage>` `! {input}` (l.110-113) | `! {input}` |
| `<command-message>...` | `<UserCommandMessage>` `❯ /cmd args` (l.115-118) | `/cmd args` |
| `<user-memory-input>...` | `<UserMemoryInputMessage>` `# {content}` chip (l.120-122) | `\# {content}` (H1-escape so markdown doesn't render as heading) |
| `<task-notification>...<summary>...</summary>...` | `<UserAgentNotificationMessage>` `⏺ {summary}` (l.139-141) | `⏺ {summary}` |
| `<tool_use_error>...` (inside tool_result.content only) | stripped to inner text | inner error text only |
| `({tool} completed with no output)` (toolResultStorage.ts:293 placeholder) | `(No output)` summary via `BashToolResultMessage.tsx:107-121` | `(No output)` |

Feature-gated wrappers not handled: `<github-webhook-activity>` (KAIROS_GITHUB_WEBHOOKS), `<teammate-message>` (swarms), `<fork-boilerplate>` (FORK_SUBAGENT), `<cross-session-message>`, `<channel source=...>`, `<mcp-resource-update>` / `<mcp-polling-update>`. All are dropped silently via the generic `strip_display_tags` fallback.

**Claude top-level record types** (per Message.tsx:103-281 dispatch):

- `user` / `assistant` — message containers (see content-block handling above)
- `attachment` — dispatched per `attachment.type` by `claude_attachment_messages` (sessions.rs). Subtypes that emit a notice line: `directory`, `file` / `already_read_file` (with `numLines` / `cells` / `unchanged` / `bytes` detail), `compact_file_reference`, `pdf_reference`, `selected_lines_in_ide`, `nested_memory`, `skill_listing` (non-initial only), `queued_command` (prompt text run through `claude_display_text` so embedded `<task-notification>` etc. dispatch correctly), `plan_file_reference`, `invoked_skills`, `mcp_resource`. NULL_RENDERING subtypes (`task_reminder`, `deferred_tools_delta`, `command_permissions`, `date_change`, `hook_success`, `async_hook_response`, `agent_setting`, `relevant_memories`, `dynamic_skill`, `agent_listing_delta`) drop silently — matches `nullRenderingAttachments.ts:14-49`.
- `system` — dispatched by `subtype` via `claude_system_message`:
  - `local_command` → strips `<command-message>`/`<command-args>` to `/cmd args` (kind=LOCAL_COMMAND)
  - `turn_duration` → `*※ Worked for {duration}*` italic dim (matches SystemTextMessage.tsx:342-401). Duration formatted via `format_duration_short` (e.g. `45269ms` → `45.3s`).
  - `away_summary` → `*※ {content}*` italic dim (l.70-84)
  - `agents_killed` → `**Error** All background agents stopped` (l.87-101)
  - `compact_boundary` → `---\n\n*{content}*\n\n---` GFM divider notice (Message.tsx:195-203 `CompactBoundaryMessage`)
  - `microcompact_boundary` / `api_error` / other → silent drop (matches verbose-only or null fallthrough)

**OpenCode** (`opencode_v2_tool_part_text`) — each tool header uses the unified `**Verb**(args)` shape but the verb text + body content stay platform-native (matching `session-v2.tsx` lines cited below). Body decorations (`\# description` BlockTool title, bash fence with `$ cmd` prefix, ```diff diff fence, `↳ Loaded` instruction-file list, `{✓/~/✕/☐}` todo icons) are preserved verbatim — only the header line was reshaped:

- `Bash` / `Shell` (l.707): header `**Shell**({wrap_inline_code(cmd)})`. With output → followed by `\# {description ?? "Shell"}\n\n```bash\n$ {cmd}\n{output}\n```` (original BlockTool body). Without output → header alone (original InlineTool was `$ {cmd}`). Output resolution mirrors TUI l.710: `metadata.output ?? state.content` (Bash-specific override — other tools just use `state.content`), then `strip_ansi` to drop terminal colour codes.
- `Glob` (l.748): `**Glob**(pattern: {wrap_inline_code(pattern)}, path: {wrap_inline_code(path)} — {N} match[es])` (singular/plural matched).
- `Read` (l.764): `**Read**({wrap_inline_code(filePath)} [other=...])` + per-entry `↳ Loaded {path}` lines using CommonMark hard breaks (`\` line terminator). `metadata.loaded` is the `instruction.resolve` array from `read.ts:264` — the auto-loaded instruction files (AGENTS.md / CLAUDE.md / etc.) the Read tool fetched alongside the requested file; surfaced because it's data, not decoration.
- `Grep` (l.794): `**Grep**(pattern: {pattern}, path: {path} — {N} match[es])`.
- `WebFetch` (l.810): `**WebFetch**({wrap_inline_code(url)})`.
- `WebSearch` (l.818): `**{provider label}**({wrap_inline_code(query)} — {N} results)`. Verb is provider-derived per `webSearchProviderLabel` (`tool/websearch.ts:39-43`): `"parallel"` → `Parallel Web Search`, `"exa"` → `Exa Web Search`, otherwise → `Web Search` (default, with space — matches Claude's verb).
- `Write` (l.828): `**Write**({wrap_inline_code(filePath)})` + ```{lang from ext}\n{content}\n``` body when completed.
- `Edit` (l.857): `**Edit**({wrap_inline_code(filePath)})` + ```diff\n{diff}\n``` body when diff present.
- `ApplyPatch` (l.891): per-file header → `**Deleted**({path})` / `**Created**({path})` / `**Moved**({old → new})` / `**Patched**({path})` + ```diff fence (matches FileChange tags in fileTitle()). When a file has no `patch` text, body falls back to `-N line` / `-N lines` (pluralized per TUI l.923).
- `TodoWrite` (l.964): `**Todos**\n\n{✓/~/✕/☐} {content}` per todo (verb is "Todos" — matches the original BlockTool title `\# Todos`; icons from todoIcon helper).
- `Question` (l.991): `**Questions**\n\n{Q}\n{A}` per Q/A pair (verb "Questions" matches `\# Questions` title).
- `Skill` (l.1022): `**Skill**({wrap_inline_code(name)})`.
- `Task` (l.1030): `**{Titlecase(subagent_type ?? "General")} Task**({wrap_inline_code(description)})` — verb includes the agent name prefix, matching the original `{Agent} Task — description` heading.
- generic (l.522): `**{name}**({input})` header + 4-backtick output fence when present.
- `reasoning` part → `format_reasoning_body` (unified italic blockquote — replaces the old `_Thinking:_` inline prefix).

All tool cards emit a `⏺` / `✗` leading marker (`status_marker` per session-v2.tsx:572 + l.669 — error state flips the InlineTool/BlockTool color). Failed parts append a 4-backtick `Error: {message}` body from `state.error.message`, mirroring Codex / Claude / Gemini failure formatting.

Top-level `SessionMessage` types beyond the tool parts (session-v2.tsx Match arms l.92-122):

- `user` (l.159 UserMessage) → `text` body + attachment row built by `opencode_v2_user_attachments`. Files surface as `` `{mime}` `` `` `{name ?? uri}` `` code-span pairs (l.176-185); agents as `` `agent` `` `` `{name}` `` (l.186-193). `references` (PromptReferenceAttachment) are persisted but TUI skips them, so Termory does too.
- `assistant` (l.296 AssistantMessage) → text from parts; if `message.error.message` is set (l.339-353), append `*✕ {message}*` italic notice on its own line.
- `synthetic` (l.105-107) → TUI renders `<></>`; Termory returns `None` so they don't appear in the transcript.
- `shell` (l.200 ShellMessage) → `$ {command}` + `strip_ansi(output)` on a second line.
- `compaction` (l.231) → bold header `**Auto Compaction**` (when `reason === "auto"`) or `**Compaction**`, followed by the `summary` body.
- `agent-switched` (l.261) → `▣ Switched agent to {Titlecase(agent)}` (prefix matches the TUI agent-color glyph at l.267).
- `model-switched` (l.275) → `◇ Switched model to {provider}/{id}[/{variant}]` (prefix matches l.284 secondary-color glyph).

Audit reference is OpenCode `1.15.5` (commit `9324ef0`). Compared against `v1.15.7`: only cosmetic reasoning collapse-icon change in session-v2.tsx (`▼/▶` → `-/+`), no structural / schema diffs. No re-audit needed.

**Gemini CLI** (`gemini_tool_messages_from_value` + `gemini_thought_messages_from_value` + `gemini_part_to_string`) — `.audit-sources/gemini-cli/packages/cli/src/ui/components/messages/`:

- `toolCalls[]` entries (ToolShared.tsx:202 `ToolInfo`) → `{status_marker} **{displayName}**({description})` with status-aware body. `status === 'success'` → `⏺` marker, body fenced verbatim. Otherwise `✗` marker + `Error: ` prefix inside the fence (per sessionUtils.ts:654-657 only `'success'` is success). `resultDisplay` body shapes per `ToolResultDisplay.tsx` are dispatched in `gemini_result_display_to_text`:
  - `string` → as-is (markdown / plain text)
  - `Array<AnsiLine>` (each line is `Array<AnsiToken {text, ...}>`, detected via `isAnsiOutput`) → join token `text` fields, trim per-line trailing whitespace (xterm-headless pads to terminal width)
  - `{todos: ...}` → drop body (TUI hides it; TodoTray renders todos separately, ToolResultDisplay.tsx:84-87)
  - `{isSubagentProgress: true, ...}` → drop body (live-progress widget, no useful static representation)
  - `{fileDiff, fileName?}` → `gemini_format_file_diff` (DiffRenderer.tsx:204-214 `isNewFile`): when every non-header line is an addition, emit ```{lang}\n{added lines}\n``` (lang inferred from the filename extension); otherwise ```diff\n{full diff}\n```
  - `{summary, ...}` (StructuredToolResult / GrepResult / ListDirectoryResult / ReadManyFilesResult) → emit the `summary` string
  - other object → `serde_json::to_string_pretty` fallback (matches TUI's `JSON.stringify(obj, null, 2)`)
- `thoughts[{subject, description}]` array (ThinkingMessage.tsx:22 `normalizeThoughtLines`) → one reasoning message per entry. Subject is wrapped in `**...**` so `format_reasoning_body` keeps it as a bold blockquote header line (mirrors the TUI's bold-italic subject + italic body at l.84-93); description lines render italic. `gemini_normalize_escaped_newlines` applies the same `\\n` / `\\r\\n` → real-newline pass as `textUtils.ts:168` so persisted escaped literals split into multiple lines. Noise filtering matches the source (skip whitespace-only or `...` runs)
- System-notice records (`type: 'info' | 'error' | 'warning'`) → `format_gemini_system_notice` wraps the body in an italic span with the TUI icon prefix (`ℹ` per InfoMessage.tsx:30 / `✕` per ErrorMessage.tsx:16 / `⚠` per WarningMessage.tsx:17). Multi-line bodies use CommonMark hard breaks (`  \n`) so the italic span survives across visual lines without a paragraph break terminating it
- Parts with `executableCode: {code, language}` → ```{lang}\n{code}\n``` fence
- Parts with `codeExecutionResult: {outcome, output}` → 4-backtick output fence + italic `*Outcome: OUTCOME_FAILED*` footer when non-OK
- Parts with `inlineData: {mimeType, ...}` → `*Inline data ({mime})*` italic notice
- Parts with `fileData: {fileUri}` → `*File: {uri}*` italic notice
- Parts with `functionCall: {name}` → `*Tool call: {name}*` (inline marker; the structured card comes from `toolCalls[]`)
- Parts with `functionResponse: {name}` → `*Tool response: {name}*`

### Helpers used across all four platforms

- `wrap_inline_code(content)` (sessions.rs:48) — CommonMark §6.1: pick a backtick delimiter longer than the longest run inside the content; pad with spaces when content starts or ends with a backtick. Used everywhere an unsafe user payload (path, command, URL, query, pattern) becomes inline `\`code\`` in markdown.
- `format_reasoning_body(content)` (sessions.rs:71) — line-by-line `> *...*` italic blockquote, escapes stray `*` / `_` so italic spans can't break mid-line.
- `merge_tool_outputs(messages)` (sessions.rs runs in `parse_claude_session` and `parse_codex_session`): folds matching `tool_result` / `tool_error` into the leading `tool_use` card. On a matched failure it prefixes the leading line with `✗ ` (instead of `⏺ `) and prepends the fence body with `Error:` (plus `Exit code N` when `SessionMessage.exit_code` is set). Orphan results (no matching tool_use) keep their text but also get a `⏺` / `✗` status prefix.
- `codex_parse_exec_output(text)` returns `CodexExecOutput { raw, exit_code }` — strips Codex's `Chunk ID: ... Output:` wrapper line-by-line so the visible body is just `aggregated_output`, AND extracts the exit code for the `Error: Exit code N` line.
- `codex_parse_patch_actions(patch_text)` scans `*** Add/Delete/Update File:` markers and returns `Vec<CodexPatchAction>` for the apply_patch header builder.
- `strip_ansi(text)` — drop ANSI escapes (CSI colour / cursor codes, OSC title-set sequences, and lone `ESC + letter` escapes). Used for OpenCode Bash output (session-v2.tsx:710) and `type: "shell"` message captures (session-v2.tsx:203). No regex crate — small inline state machine, leaves non-ESC content untouched.

### Tool message metadata + UI

- `SessionMessage` carries two `#[serde(skip)]` fields used only during parsing/merging:
  - `tool_use_id: Option<String>` — links `tool_use` ↔ `tool_result` by provider id (Claude `tool_use.id` / Codex `function_call.call_id`).
  - `exit_code: Option<i64>` — Codex shell exit code parsed from `function_call_output` metadata; surfaced in the `Error: Exit code N` fence line.
- Provider-native combined formats (OpenCode parts, Gemini toolCalls, Codex EventMsg-derived cards) skip `merge_tool_outputs` — they already arrive complete with their own fence and add the `⏺` / `✗` prefix at emission time.

### Markdown rendering (frontend)

- The detail-pane body uses `react-markdown` + `remark-gfm` (tables / task lists / strikethrough). No syntax-highlight pass: code blocks render as plain monospace until a per-language renderer is added intentionally.
- No DOMPurify / rehype-sanitize: react-markdown emits React elements (not HTML strings), so raw `<tag>` in session content is auto-escaped by React's text node rendering and displays as literal text — same characters the CLI shows.
- No raw / rendered toggle and no `viewMode` state — every message renders through the same react-markdown pipeline. The "open original file" affordance in the detail header still lets the user inspect the underlying JSONL / db row outside Termory.
- **TUI-style scrollback layout**: messages render as continuous vertical flow without card borders. The role chip + per-role color bar live as inline Tailwind utilities in `MessageList.tsx` (no `.roleBar` / `.roleLabel` CSS classes after the v4 migration). Role color tokens are still in `src/styles.css` via CSS vars (`--role-user`, `--role-assistant`, `--role-tool`, `--role-event`); the bar/text inline styles read those vars so the color palette stays single-sourced.
- The only meaningful CSS class still in `src/styles.css` is `.message-body` (note hyphen, **not** the old `.messageBody`). It wraps the react-markdown output and carries:
  - `padding-left: 11px` so body text aligns under the role label (bar 3px + 8px gap = 11px) — overridden to `padding: 0` for the memory/skill single-doc view via a Tailwind variant on the wrapping element
  - `word-break: break-all` on inline `<code>` so long paths inside `**Read**(\`/very/long/path\`)` wrap with the surrounding paragraph
  - `.message-body pre { margin: 0; padding: 0 0 0 1em }` — fences sit flush with the verb header above, 1em left indent so fence content lines up under the `⏺` marker
  - `.message-body p + pre { margin-top: -0.4em }` — pulls a fence visually onto the preceding paragraph (summary + diff pair). CommonMark-required blank line stays in the source
- The `.message.tool .messageBody p + p { padding-left: 1em }` rule that used to indent second-paragraph summaries was removed when the role-chip layout moved to inline Tailwind; the visual still works because of the verb-header layout's intrinsic indent. If you regress this, the `⎿ Added N lines, removed M lines` summary above an Edit diff will sit flush-left instead of aligning under the verb.
- Unordered lists render with the `- ` text marker via `list-style-type: "- "` (matching Codex TUI's `start_item` output at `codex-rs/tui/src/markdown_render.rs:754-760`).
- Tool detail-pane loading state shows only the spinner icon (no `Loading transcript` label) so the brief delay between session select and detail load is unobtrusive.

## History and Preview Behavior

- Session lists should come from the same stored records the official tool uses for its history/resume list.
- Session list fields should use official values when available: title, project/cwd, timestamps, source id, and original path.
- Loading a session should parse the underlying transcript/messages for that exact selected record.
- Message previews in the detail pane should show the same user-visible content style as the official tool, including command/tool output formatting.
- **Show everything that was recorded.** Termory deliberately surfaces content that the official TUIs hide (Claude's suppressed tools, `<tick>` / `<local-command-caveat>` / NULL_RENDERING attachments / `isMeta` user messages; Codex's `<environment_context>` / `<user_instructions>` / `<skill_instructions>` / etc.; OpenCode's `synthetic` parts). The transcript is the source of truth — hiding things makes the history misleading. List-time filters (`isSidechain`, `kind === "subagent"`) that decide *which sessions* appear in the list are separate and still apply.
- Compatibility readers are allowed only for real older/alternate storage layouts and should not override the current official path.
- App-only UI features such as source filters, project grouping, search, stats, cross-source sorting, and the Memory/Skills pane organization must not be used as evidence for official data behavior.

## Implementation Rules

- Keep data acquisition and message preview formatting aligned with the official tool behavior, but **never hide content** — Termory is a history browser, so anything recorded must be surfaced (usually as an italic `*[wrapper-name]*` notice when there's no nicer representation). See the "Unified tool-message format — LOCKED RULE" rule 7.
- Do not add custom title/message fallbacks unless the official tool does the same.
- Format command and tool output the way the official tool **actually renders it in its TUI** — not what its docs say, and not what feels right. Always grep `.audit-sources/<repo>/` for the real render function and put a `// path/to/file.tsx:LINE` citation next to the matching Termory branch. Earlier rounds of this codebase had ~600 lines of tool-formatting guesswork that diverged from every TUI; those have been replaced and the rule exists to prevent regressing.
- Treat UI behavior separately from official data behavior. Source filters, project grouping, search, stats, cross-source sorting, and the Memory/Skills views are app UI behavior.
- Keep changes scoped. Avoid unrelated refactors.
- Add or update tests when changing a parser or formatter. Skill/memory scanners have parallel tests at the bottom of `sessions.rs` — extend that block when adding scan paths. Tool-rendering tests should assert verbatim strings (e.g. `"**Search**(pattern: \"TODO\", path: \"src\")"`), not regex matches, so renames are caught.
- When adding a new scan location for an existing tool, verify against the tool's official source first (then docs as a secondary reference); do not infer from naming conventions alone.

## Verification

Run when practical:

```sh
cd src-tauri
cargo fmt
```

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib
```

```sh
npm run build
```

Parser/formatter tests should cover the relevant official storage shape, title extraction, visible messages, hidden metadata, and command/tool preview formatting. Skill/memory tests should cover the actual scan paths and the per-tool tag string (e.g. `claude,opencode` for `.claude/skills/`).

## Providers (switch CLI to a third-party API platform)

User flow: a Provider is a named snapshot of `{baseUrl, apiKey, model, ...}`. Each CLI has its own list. Activate = materialize into the CLI's live config so the next launch picks it up. Switching back to Official = clear the Termory-injected fields; the CLI's native OAuth/credentials file is **never touched**, so logins survive a round-trip.

**Local storage** — `~/.termory/` (same path on macOS / Linux / Windows), permissions `0700` dir / `0600` files on Unix. Atomic write (tmp + rename):

- `~/.termory/config.json` — UI prefs (`default_pane`, `providers_app`, `recent_searches`). No secrets.
- `~/.termory/providers.json` — provider library (`Provider[]`). Contains API keys, `0600`.

Termory does **not** store an "active provider" pointer anywhere. `provider_active_state` reverse-derives the active state on every read by parsing the CLI's live config files and matching against the in-memory provider list — this keeps Termory consistent when other tools (`cc-switch`, manual `vim`, the CLI's own OAuth flow) change the same files.

`Provider` schema (in `providers.json`, fields default to omitted when empty):

```
{ id, app: "claude"|"codex"|"gemini"|"opencode", kind: "custom"|"official",
  name, baseUrl?, apiKey?, model?,
  claude?: { haikuModel?, sonnetModel?, opusModel? },
  opencode?: { providerId? } }
```

App-specific options live under their app key so the JSON is grouped and `Provider` doesn't grow a wide column of `xxx_yyy_zzz` flat fields. The editor submit pass collapses an empty nested block to `undefined` so providers.json never carries `claude: {}` / `opencode: {}`; `config.ts` strips `""`/`null`/`undefined` top-level fields the same way. `opencode.providerId` is the AI SDK catalog id (`anthropic` / `openai` / `openai-compatible` / …) Termory writes the key under in `~/.local/share/opencode/auth.json`.

### Per-CLI materialization (source-of-truth: cite official source AND cc-switch when both verified)

All four were cross-verified against the upstream CLI source (`.audit-sources/{codex,claude-code,gemini-cli,opencode}/`) AND cc-switch's implementation (`.audit-sources/cc-switch/src-tauri/`). Cite `file:line` next to each branch — don't infer from docs.

| CLI | Files written | Key fields | OAuth credential file (untouched) |
|---|---|---|---|
| **Claude Code** | `~/.claude/settings.json` (merge) | `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_AUTH_TOKEN`, `env.ANTHROPIC_MODEL`, plus optional sub-routing `env.ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL` for Claude Code's `/model` size picker (see `model.ts:69, 105-138`) | `~/.claude/.credentials.json` (or macOS Keychain — `auth.ts:1323`). Independent file, isolated by design. |
| **Codex** | `~/.codex/auth.json` (merge) + `~/.codex/config.toml` (merge) | auth.json: `auth_mode = "apikey"`, `OPENAI_API_KEY` (matches `login_with_api_key` shape `manager.rs:529-542`; we **merge** instead of nulling `tokens` so ChatGPT OAuth survives a swap). config.toml: top-level `model_provider`, `model`; `[model_providers.termory]` block with `name`, `base_url`, `wire_api = "responses"`, `requires_openai_auth = true` (gates `AuthManager`'s auth.json load — `tui/src/lib.rs:1817`). **Never set `env_key`** — it forces Codex onto a hard env-var path with no fallback (`model-provider/src/auth.rs:92-103`). | The `tokens` / `last_refresh` / `agent_identity` fields **inside** auth.json. Termory never overwrites them; deactivate only removes `auth_mode` + `OPENAI_API_KEY`. |
| **Gemini CLI** | `~/.gemini/.env` (dotenv merge, `chmod 0600`) | `GOOGLE_GEMINI_BASE_URL` (triggers GATEWAY mode `contentGenerator.ts:85-87`), `GEMINI_API_KEY`, `GEMINI_MODEL` (`config.ts:836-837`). Other env vars in the file are preserved. | `~/.gemini/oauth_creds.json` + `~/.gemini/google_accounts.json` (`storage.ts:22, 87`). Independent files, isolated. |
| **OpenCode** | `~/.local/share/opencode/auth.json` (merge, additive) + optional `~/.config/opencode/opencode.json` overlay | auth.json: `{<provider_id>: {type: "api", key: "..."}}` under the user-chosen AI SDK catalog id (`anthropic` / `openai` / `openai-compatible` / `google` / `azure` / `amazon-bedrock` / `google-vertex`). Same shape `/connect` writes (`auth/index.ts:22-26, 72-80`) so OpenCode's built-in npm + model directory is reused — Termory does NOT re-declare `npm`/`name`/`models`/top-level `model`. Optional overlay: `provider.<id>.options.baseURL` in opencode.json — the official "custom base URL" pattern from `providers.mdx:36-48`. All other `provider.*` blocks (and other auth.json entries, including OAuth-type entries from `providers login`) are preserved on activate AND deactivate. | Other auth.json entries with `type ≠ "api"` — or any entry under a different catalog id — are not touched. OpenCode's `providers login` writes `{type: "oauth", ...}` entries under its own id; Termory only adds/removes its own `{type: "api"}` entry under the user-chosen id. |

**Codex "stable provider id" rationale:** Codex stores session history keyed by `model_provider`. If we used a different id per provider, switching would visually "drop" history. We pin all Termory-written Codex provider blocks to id `"termory"` (`TERMORY_PROVIDER_ID` constant) and refuse to overwrite Codex's built-in reserved ids (`openai`/`amazon-bedrock`/`ollama`/`lmstudio` — `CODEX_RESERVED_IDS`).

**Claude sub-model routing (Advanced section):** Claude Code's `/model` menu in 3P mode reads `process.env.ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL` via `getDefaultXxxModel()` (`model.ts:105-138`). Users who route different sizes to different upstream models (e.g. Sonnet → `gpt-5`, Opus → `claude-opus-4-7`) fill the Advanced fields; empty fields strip the corresponding env var.

**Test coverage:** Each CLI has an activate/reverse roundtrip test, an unrelated-fields-preserved test, and an OAuth-credentials-isolated three-stage test (Stage 1 simulates a prior CLI login, Stage 2 activates a Custom provider via Termory, Stage 3 deactivates — credentials file must be byte-identical at the end). See `providers::tests::*` in `src-tauri/src/providers.rs`.

## Menu-bar tray (macOS system tray)

A native system-tray icon (the macOS menu bar) for quick provider switching, separate from the in-app Providers rail route. Implemented in `src-tauri/src/tray.rs` and installed from `lib.rs` `setup()`. Requires the `tauri` features `tray-icon` + `image-png` (Cargo.toml).

**Menu shape** — one submenu per CLI; the active choice is shown inline on the first-level row and the submenu lists Official + the user's providers with the active one checkmarked:

```
Claude Code · Official  ▸  ☑ Official
                           ─────────
                           ☐ Anthropic
                           ☐ OpenRouter
Codex · OpenRouter      ▸  …
Gemini · …              ▸  …
OpenCode · Official     ▸  …
─────────────────────
Open
Exit
```

- **Active state is reverse-derived** via `providers::read_active_state` (same as the Providers page) — Termory still stores no "active provider" pointer. Checkmark + inline `· name` use the matched provider name, else "Official".
- **CLI display names** match the Providers page tabs (`CLI_APP_LABEL` in `src/constants.ts`) — keep `cli_label` in `tray.rs` in sync (it's "Gemini", not "Gemini CLI").
- **Click handler** reuses the same write helpers as the IPC commands (`activate` / `deactivate`), so the on-disk path is single-sourced. For **OpenCode** a click does `activate` **+** `set_opencode_default` (the slot must exist before it can be the default — `set_opencode_default` errors otherwise), so one click both enables the slot and sets it as default, matching the Providers page end state. Single-slot CLIs (Claude / Codex / Gemini) need only `activate`.
- **Menu is rebuilt** (`tray::rebuild_menu`) after every tray click and after the five provider-mutating IPC commands (`activate_provider` / `deactivate_provider` / `delete_provider` / `set_opencode_default_provider` / `write_app_providers`) so the checkmarks / inline titles stay in sync.
- After a tray switch the handler emits **`termory:providers-changed`**; `ProvidersPage` listens for it and re-derives active state, so an open Providers page reflects a tray switch even when unfocused.
- **Open / Exit** are plain `MenuItem`s (not `PredefinedMenuItem::quit`) so macOS attaches no native item icon. Exit calls `app.exit(0)`.
- **Icon**: embedded `icons/tray-icon.png` (36×36 = 18pt @2x, the macOS standard), a pure-black template image of the three-card terminal "chip" from the app icon, optically centered. `icon_as_template(true)` lets macOS theme it for light / dark bars. `icons/tray-template.svg` is the vector source; the menu-bar size is governed by the whole image (tray-icon crate scales it to a fixed 18pt height), so the artwork's transparent margin controls how large it reads.

**Known minor gaps (by design for a quick-switcher):** the tray doesn't run the Providers page's `ensureCliInstalled` gate; it doesn't expose OpenCode's multi-slot enable/disable or delete; failures are `log::error!`-only (no toast). Menu only refreshes on Termory-initiated changes, so an external switch (cc-switch / manual edit) can leave it stale until the next Termory action.

### Window / Dock behavior (menu-bar app)

Closing the window does **not** quit — `lib.rs` `on_window_event` intercepts `CloseRequested`, calls `window.hide()` + `set_dock_visibility(false)` (macOS) + `api.prevent_close()`, so the app keeps running in the menu bar with no Dock icon. The tray's **Open** restores the Dock icon (`set_dock_visibility(true)`) then shows / unminimizes / focuses the window. Launch starts with the window + Dock icon visible (default). `set_dock_visibility` is the purpose-built API — a raw `set_activation_policy(Accessory)`/`Regular` toggle did NOT reliably restore the Dock icon when re-showing the window.

## Stats

Three cards stacked under a source filter + date range bar:

1. **OverviewHero** — KPI strip: Sessions / Messages / Tokens / Projects. The Tokens cell hovers a 4-row breakdown (Input / Output / Reasoning / Cached + Total).
2. **DailyTokensChart** — 4 trend lines (Input / Output / Cached / Reasoning) on a single linear-scale chart. Tooltip per-day shows fixed 5-row breakdown.
3. **DailyActivitiesHeatmap** — 24-hour × N-date heatmap. Cell intensity blends per-cell messages + tokens via a weighted geometric mean (see "Heatmap intensity rule" below); hover reveals `Sessions / Messages / Tokens` for that exact `(date, hour)` bucket. Hour labels: hand-picked 14 rows, work band 09:00–18:00 highlighted with `text-foreground`.

### Accuracy rules (LOCKED — do not weaken)

All Stats values are **window-accurate** — they reflect what actually happened in the chosen date range, NOT lifetime totals.

The rules `windowTotals` (and the two visualization functions) follow:

| Metric | Source | Notes |
|---|---|---|
| Sessions | `started_at ∈ window` count | Old session reused today does NOT count |
| Messages | `Σ daily_tokens[date ∈ window].messages` | Per-message timestamps from backend |
| Tokens | `Σ daily_tokens[date ∈ window].tokens` | Same |
| Projects | unique projects of sessions with any of the above | Window-bounded |

**No fallback smearing**: if a session has `s.tokens` but no `s.daily_tokens`, it contributes ZERO to Messages/Tokens. Termory does not even-distribute lifetime totals across `[started_at, updated_at]` — that would fabricate per-day numbers indistinguishable from real ones. (`sessionActiveDays` and the fallback branch were removed.)

**filter uses interval overlap**: `filterSessions` keeps a session if `[started_at, updated_at] ∩ window ≠ ∅`. Filtering on `updated_at ∈ window` alone would drop sessions whose `updated_at` is after a custom past window even when their `daily_tokens` fall inside it.

**Cross-source consistency** (backend-guaranteed):
- `entry.messages === Σ entry.hours[h]` per `daily_tokens` entry
- `entry.tokens.total === Σ entry.hour_tokens[h]` per entry (Gemini edge case: when some records have explicit `total` and others don't, slight drift)
- Therefore `Σ windowTotals.messages === Σ DailyActivities.messages[h][d]` and `Σ windowTotals.tokens.total === Σ DailyTokens[].total === Σ DailyActivities.tokens[h][d]`

### Heatmap intensity rule (LOCKED — do not weaken)

`DailyActivitiesHeatmap` colors each `(h, d)` cell by a combined intensity ratio. The formula is a **weighted geometric mean** with messages tilted at 60%, tokens at 40%:

```
ratio = m^0.6 * t^0.4    where m = msgCount / maxMsg, t = tokCount / maxTok
```

Computed per cell in a `useMemo` that pre-scans `maxMsg` / `maxTok` and fills a 24×N ratio matrix once — the render loop only reads `ratios[h][d]`. Do NOT inline the `Math.pow` calls into the JSX map (720+ calls per frame).

**Single-dimension degradation** (don't remove this):
- `maxMsg === 0 && maxTok === 0` → `ratio = 0` (truly inert)
- `maxTok === 0` → fall back to `msgCount / maxMsg` (old session users with no `daily_tokens`)
- `maxMsg === 0` → fall back to `tokCount / maxTok` (degenerate)
- Otherwise → geometric mean as above

**Tier mapping** (6 buckets, thresholds skewed toward the low end so mid-activity cells don't get stuck in the lightest tier):

| ratio | tier | Tailwind class |
|---|---|---|
| `≤ 0` | inert | `bg-foreground/[0.04]` |
| `< 0.08` | 1 (lightest) | `bg-primary/15` |
| `< 0.18` | 2 | `bg-primary/30` |
| `< 0.35` | 3 | `bg-primary/45` |
| `< 0.55` | 4 | `bg-primary/60` |
| `< 0.75` | 5 | `bg-primary/75` |
| `≥ 0.75` | 6 (darkest) | `bg-primary/90` |

**Special cells**:
- `msg === 0 && sess === 0` → inert color, NO HoverCard (no work to show)
- `msg === 0 && sess > 0` (a session was created in this hour but its first message landed later) → floor color `bg-primary/10` — deliberately ONE notch below tier 1 so it's distinguishable from a real-but-low activity cell. Hoverable; HoverCard shows `Sessions: N`.

Why weighted geometric mean (not arithmetic, not single-dimension):
- Pure messages → "one big request" hour reads cold (user was actively waiting on a 100K-token answer)
- Pure tokens → "many short messages" hour reads cold (user was clearly engaged)
- Arithmetic mean → "many msg + low tok" cells stay too bright (high m dominates)
- Geometric mean with `m^0.6 * t^0.4` → messages are the primary signal but tokens still pull ~40%; equal-magnitude cells (`m === t`) collapse cleanly to that shared value

Whoever later tunes the weight: change `MSG_WEIGHT` in `DailyActivitiesHeatmap.tsx` (default `0.6`). Don't change the geometric-mean shape itself unless you have a UX reason as strong as the one above.

### Backend wire shape

`DailyTokenBreakdown` (per-day, per-session):

```rust
{
  date: "YYYY-MM-DD",            // scanning machine's local TZ
  tokens: TokenStats { input, output, cached, reasoning, total },
  messages: u64,                 // count of AI interactions that day
  hours: [u64; 24],              // per-hour message count, local hour 0..23
  hour_tokens: [u64; 24],        // per-hour tokens.total
}
```

Populated by all four scanners when underlying records carry timestamps:
- **Codex** — `event_msg.token_count` events (delta between cumulative usages); per-event `timestamp` → local date+hour.
- **Claude** — per-`message.usage` JSONL line; line `timestamp` → local date+hour.
- **Gemini** — per-`record.tokens` entry; record `timestamp` → local date+hour.
- **OpenCode** — per `step-finish` part; `time.created` (epoch ms) → local date+hour.

All four are gated on a successful local-time parse — records without a timestamp don't appear in `daily_tokens`, the session shows up but contributes zero to time-bucketed widgets.

### Frontend aggregation

`src/lib/stats-utils.ts` exports the pure helpers (`windowTotals` / `dailyTokens` / `dailyActivities` / `filterSessions`). Each one iterates `filtered` sessions once; the Stats page memoizes them per `(filtered, resolved)`. 117 unit tests cover the window-overlap regression, no-fallback enforcement, per-source attribution, and cross-consistency between aggregator outputs.

Naming alignment (UI label ↔ data field ↔ file ↔ component) is intentional:
- "DAILY TOKENS" card → `DailyTokensChart.tsx` (component) ← `DailyTokens[]` (type) ← `dailyTokens()` (function)
- "DAILY ACTIVITIES" card → `DailyActivitiesHeatmap.tsx` ← `DailyActivities` ← `dailyActivities()`
- KPI labels (`Sessions` / `Messages` / `Tokens` / `Projects`) map 1:1 to `WindowTotals` fields

## Favorites

Per-message starring + dedicated Favorites route. Stars live next to every message in the Records detail pane; the Favorites route lists them in a Records-style 2-column shell (list / detail) and survives source-file deletion via local snapshots.

### Snapshot rule (LOCKED — do not weaken)

A `Favorite` is a **self-contained snapshot** of the parsed message. The full `SessionMessage` (role + complete markdown text + timestamp + kind) gets stored verbatim alongside source-session metadata (title, project, path, message index). When the source session is later deleted / renamed / re-parsed differently, the Favorite **stays readable and renders identically** — the detail pane uses the same `<MessageBody>` (react-markdown + remark-gfm) pipeline as the Records detail, so what you see in Favorites matches what you saw when you starred it.

Do not:
- Replace the snapshot with a `(source_session_id, message_index)` reference that re-fetches the message at render time. The "Open original" button still uses that tuple, but as a navigation hint, not the source of truth. If the index has drifted by the time the user clicks it, that's accepted — the snapshot in the favorite is authoritative.
- Strip the snapshot to "just the markdown text" — `role` / `kind` / `timestamp` are read by the role-bar color, the lowercase chip, and possible future tooltips.
- Lazy-fetch session metadata. `source_session_title` / `source_session_project` are stored at favorite time so the list card still has something to show when the original session is gone.

### Wire shape

`Favorite` (per favorite, in `~/.termory/favorites.json` as a JSON array):

```ts
{
  id: string;                     // UUID v4 (frontend-generated)
  favorited_at: string;           // ISO 8601 UTC
  message: SessionMessage;        // full snapshot
  source: SessionSource;          // narrowed: "Claude" | "Codex" | "Gemini" | "OpenCode"
  source_session_id: string;
  source_session_path: string;
  source_session_title: string;
  source_session_project: string;
  source_message_index: number;
}
```

Same `SessionMessage` struct used by `scan_all_sessions` / Records — no schema duplication.

### Identity key

The `(source, source_session_id, source_message_index)` tuple is the "is this message currently favorited?" key (`favoriteKey()` in `src/lib/favorites.ts`). Used by:
- The star button to render its fill state in O(1)
- `toggleFavoriteEntry()` to flip between add / remove
- The `[&_*]:text-primary-foreground` highlight cascade in the list cards

Index drift across re-parses is the documented trade-off — if a session's `merge_tool_outputs` produces a different message at the same index later, the Records star may light up on the wrong message. The Favorites page itself is unaffected because it renders the snapshot. We deliberately did NOT pick a content hash because (a) it complicates "click star to remove" UX (hash mismatch ≠ "wasn't favorited") and (b) index drift is rare; sessions are append-only in all four scanners.

### Frontend layout (LOCKED for visual parity with Records)

FavoritesPage is the same shell as Records' middle + right columns:
- List column: `bg-sidebar` aside, 240–300px, `text-sidebar-foreground` default text, `bg-primary text-primary-foreground [&_*]:text-primary-foreground` active state — identical class set to Records' session list buttons. Newest first by `favorited_at`. Auto-selects the first card on mount and after the previously-selected card is removed.
- Detail column: same `<header>` shape as Records detail (`text-lg font-semibold` title, `text-xs leading-none` meta row with `·` chips, size=12 lucide icons). Action icons (`ExternalLink` / `Trash2`) cluster top-right of the meta row, NOT the title row — keeps the title clean.

`MessageList` (used in Records, optionally pluggable into anywhere else) accepts an optional `favorites: FavoriteContext` bundle (session + keys + onToggle). Caller that doesn't want the affordance simply omits the bundle — the three-field grouping prevents "forgot to pass one" bugs.

### Scroll-to-message navigation (shared by Favorites + Search + Cmd-K)

Three sites navigate "into a specific message" — Favorites "Open original", Search results, Cmd-K palette results. All converge on the same `openItem(item, messageIndex?)` → `pendingScroll` → `MessageList scrollRequest` → `onScrolled` path:

1. **Backend** — `SearchHit` carries `first_match_index: Option<usize>` (the 0-based index of the message containing the first query hit). `serde(default, skip_serializing_if = "Option::is_none")` so old hits without it still deserialize.
2. **`openItem(item, messageIndex?)`** in App.tsx — sets `selected` AND a `pendingScroll = {sessionKey, index, nonce}` state. Nonce is `Date.now()`; same target clicked twice still re-triggers.
3. **Records renders**; once `detail.messages` loads, `MessageList` receives `scrollRequest` (only when `pendingScroll.sessionKey === selected`'s key).
4. **`MessageList`'s effect** fires a `requestAnimationFrame` → `virtualizer.scrollToIndex(idx, {align: "start"})` → `onScrolled()` callback.
5. **`clearPendingScroll = useCallback(() => setPendingScroll(null), [])`** in App.tsx nulls the state — required so the scroll doesn't fire again when the user selects another session and comes back.

The callback identity is stable (`useCallback` with empty deps) so the MessageList effect's dep array doesn't oscillate. SearchPage / CommandPalette pass `hit.first_match_index` through to `onOpenItem`; Favorites passes `source_message_index`.

## UI conventions

### Tooltips (LOCKED)

Termory uses **shadcn `Tooltip`** (Radix-backed, mounted via `TooltipProvider` at root in `main.tsx`) — not raw HTML `title=` attributes. Native `title` was removed project-wide and replaced where the affordance is needed:

| Place | Trigger |
|---|---|
| Records detail "Open in Finder" | shadcn `Tooltip` |
| `CopyMenu` trigger | shadcn `Tooltip` (suppressed while menu open via `open={menuOpen ? false : undefined}`) |
| `MessageList` star button | shadcn `Tooltip` (dynamic label: Add / Remove from favorites) |
| Favorites detail "Open original session" + "Remove favorite" | shadcn `Tooltip` |
| Stats header compact-number chips (e.g. "1.2B tokens") | shadcn `Tooltip` showing `formatFullNumber(...)` |

Do NOT add `title="..."` to JSX. The reasons: (1) inconsistent styling vs the rest of the app, (2) no touch / long-press support, (3) browser-native tooltip can fight with focus-visible / aria-label, (4) shadcn version respects the project's color tokens.

Icon-only buttons still need an `aria-label` regardless of whether they have a Tooltip — screen-reader users don't trigger hover.

When a button opens a popover or dropdown that anchors on the SAME element (CopyMenu's case), suppress the Tooltip while the popover is open: pass `open={isOpen ? false : undefined}` to `<Tooltip>` so the two surfaces don't stack on the same anchor.

Tests that mount a component using `<Tooltip>` standalone (without the app's root `TooltipProvider`) must wrap their `render(...)` in a `TooltipProvider`. Pattern:

```tsx
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}
```

## Pending feature work

The current UI shell is settled: activity rail (Providers / Records / Favorites / Search / Stats / Settings, in that order — Providers is the default landing route via `readRouteFromHash` → `"providers"` fallback), routed via URL hash, with a passive bottom freshness footer fed by the Rust filesystem watcher. **All six rail destinations are real implementations** — no route renders a placeholder anymore (`RoutePlaceholder` was removed when Favorites shipped).

Roadmap below is grouped by priority. Pick top-down within a tier.

### P0 — core capabilities for v1 ✅

All P0 items have shipped:

- ~~Search page + Cmd-K palette~~ — done. Backend `search_all_sessions` IPC + frontend `SearchPage` (grouped by source with snippet highlight) + global Cmd-K palette reusing the same store + recent-search history persisted via `config.ts`.
- ~~Empty states per route~~ — done. Distinguishes first-launch (no data) from filtered-empty (with "Clear filters" action) across Sessions / Memories / Skills panes.
- Message content rendering polish — deferred. `renderMessages` / `TimeSeparator` helpers stay staged. Revisit only with a concrete pain point.

### P1 — new pages & persistence

- ~~`tauri-plugin-store`~~ — replaced with custom `config.rs` module (`~/.termory/{config,providers,favorites}.json` with `chmod 0600`). The plugin couldn't control file location or Unix permissions; rolling our own KV gives both.
- ~~Providers page~~ — done. See the "Providers" section above. Cross-verified against `.audit-sources/cc-switch/` for the per-CLI write shapes; 4 CLIs supported with per-CLI tests. OpenCode adapter was re-done in a second pass: instead of self-declaring `provider.termory.{npm,name,models}` (which fights OpenCode's catalog at runtime), Termory now writes the API key under one of OpenCode's built-in catalog ids (`anthropic`/`openai`/`openai-compatible`/…) into `~/.local/share/opencode/auth.json` — same shape `/connect` produces — and optionally overlays a `baseURL` in `opencode.json`. The AI SDK dropdown in the editor is the catalog id picker.
- ~~Stats page~~ — done. See the "Stats" section below. KPI strip (Sessions/Messages/Tokens/Projects) + DAILY TOKENS line chart + DAILY ACTIVITIES heatmap. All values window-accurate from each session's `daily_tokens[]` — no fallback smearing, no lifetime-of-touched-session totals.
- ~~App Settings page~~ — done. 5 sections: Appearance (next-themes System/Light/Dark), Storage (`~/.termory/` path + "Open in Finder"), Search history (count + Clear), Keyboard shortcuts (display-only reference), About (version + manual / auto update check). The only spec item NOT in the page is **scan-path overrides** — users with non-default CLI install locations still rely on the per-CLI env vars (`CLAUDE_CONFIG_DIR` etc.). Add a "Sources" section if that ever becomes a real ask. The originally-listed "watcher toggle" was dropped along with the watcher-completion work in P2.

### P2 — quality of life

- **Right-click context menus** — on list items ("Re-read this file", "Reveal in Finder", "Copy ID") and on sidebar source rows ("Re-scan this source").
- **Keyboard navigation** — partial. ✅ `⌘1..6` switch rail routes (App.tsx:235), `⌘K` / `⌘F` summon Cmd-K search palette (CommandPalette.tsx), `Esc` closes palette / dropdowns. ❌ Still TODO: arrow-key navigation inside lists (Records sidebar, session list, Favorites list) and `Enter` to open the focused item.
- ~~Watcher completion~~ — intentionally not pursued. Per-project files (`<cwd>/CLAUDE.md`, `AGENTS.md`, `.claude/skills/`, etc.) are only read at launch; if a user edits them mid-session the change isn't reflected until next launch / manual refresh, and that's acceptable. Recursive cwd watching would pull in `node_modules` / build noise and isn't worth the complexity.
- ~~Frontend test baseline~~ — done. 117 Vitest tests covering `session-utils`, `format`, `usePersistentState`, `CopyMenu`, `FreshnessFooter`, `stats-utils`, `favorites` helpers, `FavoritesPage`, and `MessageList` (star wiring). `@tanstack/react-virtual` is `vi.mock`'d in MessageList tests to bypass jsdom layout limits.

### P3 — nice to have

- ~~New-item badges~~ — explicitly excluded; the freshness footer ("Synced 2m ago") is enough passive feedback, an unread/red-dot system isn't desired here.
- ~~Starred messages~~ — done. See the "Favorites" section above. Per-message star (not per-session) with full-snapshot storage in `~/.termory/favorites.json` and a dedicated rail destination.
- **Export session** — single session → markdown / PDF, surfaced via the detail header's existing action row.
- **Starred sessions / tags** — virtual "Starred" source in sidebar; custom labels per record (orthogonal to per-message favorites — favorites snapshot a message body, tags label the session as a whole).
