# CLAUDE.md

## Scope

Rules for working in this repository — constraints the code cannot tell you, and things that must or must not be done. Implementation detail belongs in the code; history belongs in git.

## The app

Tauri v2 + React desktop app. Browses the local history (sessions, memory, skills) that **Codex / Claude Code / Gemini CLI / OpenCode / Grok Build** store on disk, and switches each tool's API provider. **Claude Desktop** is provider-switchable too (GUI app, no history). User-facing docs are `README.md` and `docs/GUIDE.md` — do not duplicate them here.

Modules are named for what they do (`ls src-tauri/src/`, `ls src/lib/`). Rust tests are inline at the bottom of each module. The IPC surface is `generate_handler!` in `lib.rs`.

Features spanning several files:

- **i18n** — `src/i18n/` (`<I18nProvider>` + `useT()`)
- **Local store** — `src/config.ts` routes over IPC to `~/.termory/{config,providers,favorites}.json` (Rust `config.rs`)
- **Multi-account** — `accounts.rs` + `claude_auth.rs` + `OfficialAccountsSection.tsx`
- **Quota** — `quota.rs` + `useQuotas.ts`
- **Balance** — `balance.rs` + `useBalances.ts` + `balance-utils.ts`
- **Stats** — `stats-utils.ts` (pure) + `src/components/stats/`
- **Favorites** — `favorites.ts` (pure) + `src/components/favorites/`
- **Find (⌘F)** — `TranscriptFindBar.tsx` + `DocDetailView.tsx` + `highlight-term.ts`
- **Migrate/delete** — `sessions.rs` (`*_in` fns) + `migrate.ts` + `records.ts`

## Security boundary — the frontend never sends a path (LOCKED)

**A filesystem path is never accepted as an IPC argument.** The frontend sends `(source, id)` or `(project, rel)`; the backend resolves the real path itself and refuses anything outside what it already knows.

- `load_session` looks the path up in `SESSION_PATH_INDEX` and **refuses anything that was not part of the most recent scan**, so a path the renderer does not know about cannot reach `parse_doc_file` / `parse_codex_session` at all. This inverts an earlier design where the path came from the frontend and was validated against an allow-list — do not go back to it.
- Migrate and delete take `(project, rel)` or an id and build the path under a bounded root, guarded by `is_safe_rel` (every component Normal; rejects `..` and absolute).
- The session id is charset-guarded (`[A-Za-z0-9._-]`) before it is interpolated into a resume command — injection defence.

**A read path NEVER returns a raw API key to the frontend.** Every reverse-derivation hands back `api_key_masked` (`mask_secret`: first and last four characters, the rest dotted; eight characters or fewer are dotted entirely). The page only needs to show which credential is live, and a raw key in an IPC payload lands wherever that payload lands. `mask_secret` slices by CHARS, not bytes — it reads untrusted on-disk content, and byte slicing panics on a multibyte boundary.

**Two files under `~/.termory/` hold sensitive data** (`0700` dir / `0600` files on Unix):

- `providers.json` — API keys.
- `favorites.json` — **verbatim message snapshots, so it may contain PII or secrets the user pasted into a conversation.**

**Nothing sensitive goes into the log** (`~/.termory/logs/`). Log the failure, never the credential, the payload or the message body.

## Implementation rules

- **NEVER commit automatically (LOCKED).** No `git commit` / `push` / `tag` / PR unless asked in that turn. Finishing work and going green are not commit triggers. Approval to commit once does not carry forward. Branching is fine when asked for.
- **Never hide recorded content** — see tool-format rule 7.
- **Do not add title or message fallbacks the official tool does not have.**
- **Format tool output the way the official tool renders it in its TUI** — not its docs. Grep `.audit-sources/<repo>/` for the render function and cite file + symbol next to the branch.
- **Treat UI behaviour separately from official data behaviour.**
- **Keep changes scoped.** No unrelated refactors.
- **Add or update tests when changing a parser or formatter.** Tool-rendering tests assert VERBATIM strings, never regex.
- **When adding a scan location, verify against that tool's official source** — never infer from naming.
- **Green tests are not runtime verification.** State what still needs a real device or live service. To actually run the app — launch it, screenshot it, click through it, confirm a rendering change — use the repo's `run-termory` skill; **there is no CDP/DevTools handle** on Tauri v2 macOS, so every Playwright / chromium-cli recipe is a dead end and accessibility plus screenshots is the whole surface.

## Verification

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib
cd src-tauri && cargo fmt
npm test
npm run build
```

Parser and formatter changes need tests covering the real storage shape, title extraction, visible messages, hidden metadata, and preview formatting. Skill and memory changes need tests covering the scan paths and the per-tool tag string.

## Cross-platform is a hard requirement (macOS / Linux / Windows)

CI builds all four targets on every `v*` tag. Never let a single-platform API into a shared path.

- Gate OS-specific calls behind `#[cfg(...)]` with a fallback arm.
- **Do not remove `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`** from `main.rs` — without it the Windows release opens a console beside the app.
- **Resolve each CLI's data path to what THAT tool uses per-OS.** Claude/Codex/Gemini use home-relative dotdirs everywhere. **OpenCode uses xdg paths on EVERY OS including Windows**, so build them from `home()`; `dirs::config_dir()` / `dirs::data_dir()` are wrong for these tools on both macOS and Windows.
- Build paths with `PathBuf::join`; keep shell and `.exe` assumptions inside a `cfg`.
- **Never spawn a CLI by bare name on Windows — always resolve the real binary path first.** `Command::new("codex")` appends `.exe` and never `.cmd`, so an npm-installed `codex.cmd` shim makes detection report "installed" while the spawn fails NotFound. Resolve through the same scan `detect_clis` uses; an explicit path ending in `.cmd` then goes through the runtime's hardened `cmd.exe` routing, and PATH must be augmented with the binary's own dir so the shim can find its node.
- **Never call `dirs::home_dir()` — use `crate::home_dir()`.** It honours the `HOME` override in test builds on every OS; the plain form ignores the environment on Windows and writes the runner's real profile.
- **Windows tests must exist for anything path-shaped.** Derive the expected value the way production does and PARSE the result — canonicalize adds `\\?\` and JSON escapes backslashes, so substring asserts cannot hold.
- **Windows file locks are MANDATORY, not advisory**: while a guard is held any other handle reading that range fails, same process included. Mirror the upstream protocol exactly rather than inventing a Windows-only divergence.

## Release

**`test.yml` is the release gate** — it exposes `workflow_call` and `release.yml` runs it first, with build and publish gated on it. **Do not duplicate the suite in release.yml.**

A `v*` tag builds macOS arm64 + x64, Linux x64 and Windows x64, attaches `latest.json`, and publishes automatically once all four succeed.

- **Never choose a MAJOR or MINOR bump on your own (LOCKED).** Default to PATCH and ASK when it looks like more — the version line is a product decision. **Pushing a tag is irreversible**: auto-updaters pick it up within minutes.
- **Bump the version in ALL FIVE files to the same value**: `package.json`, `package-lock.json` (run `npm install`), `src-tauri/Cargo.toml`, `Cargo.lock` (`cargo update -p termory`), `tauri.conf.json`.
- **Never change the macOS bundle id `is.chats.termory` after a public release** — macOS treats a different id as a different app, breaking user data and the updater.
- **The signing key at `~/.tauri/termory.key` must be backed up.** Losing it means no more signed updates, and the locked bundle id makes that unrecoverable.
- `tauri::generate_context!` needs `dist/` at compile time, so a bare Rust job must stub `dist/index.html`.

## Upstream references

`.audit-sources/` (gitignored) holds clones of `openai/codex`, `google-gemini/gemini-cli`, `anomalyco/opencode`, `xai-org/grok-build`, `videcoding/cli`, `farion1231/cc-switch`. **Grep there rather than fetching doc sites** — docs lag the actual UI.

**Cite the file and symbol, never a line number** — the clones are re-fetched and line numbers drift.

**The `videcoding/cli` Claude Code clone is stale** and lacks every tool added since April 2026. Claude Code is closed source, so the authoritative reference for anything newer is the shipped binary — a Bun executable with its JS strings in the clear:

```sh
python3 - <<'EOF'
import mmap; m=mmap.mmap(open('/Users/…/.local/bin/claude','rb').fileno(),0,access=mmap.ACCESS_READ)
i=m.find(b'userFacingName(){return"Monitor"}'); print(m[i:i+400].decode('utf-8','replace'))
EOF
```

Grep `"ToolName"` to find the block, then read `userFacingName` / `renderToolUseMessage` verbatim. **Do not assume the verb equals the tool name**, and check whether `renderToolUseMessage` exists at all — its absence means the TUI shows the verb with no arguments. **Record the binary version next to anything recovered this way.**

**When behaviour differs by version, match the locally installed one** and cover it with a focused test.

## Data sources

**Read source history in place. Never modify the CLIs' original files or databases** outside the explicit migrate/delete features.

### Session list filters

Each mirrors that tool's own list logic; verify against its source before changing one.

- **Codex** — `~/.codex/state_5.sqlite` table `threads`: `archived = 0`, `preview <> ''`, `source` in the four interactive sources. Messages from each row's `rollout_path` JSONL.
- **Claude** — `CLAUDE_CONFIG_DIR/projects/**/*.jsonl` (else `~/.claude/projects/`). Filename must be a UUID, first line must NOT be `isSidechain`, and the session needs one of customTitle / aiTitle / lastPrompt / summary / firstPrompt.
- **Gemini** — `~/.gemini/tmp/*/chats/session-*.{json,jsonl}`, gated on non-empty `sessionId`, `hasResumableContent`, and `kind !== 'subagent'`. `hasResumableContent` mirrors the CLI: a `user` turn counts unless its content is ignored (empty / slash command / shell passthrough / `<session_context>` / `<hook_context>`); a `gemini` turn counts with text, tool calls or thoughts. Fall back to file mtime when the record has no timestamps.
- **OpenCode** — `opencode.db` table `session`, `parent_id IS NULL` and not archived. Messages from `message` + `part`; `session_message` is a compat path for OLDER databases only and must never be preferred.
- **Grok** — `$GROK_HOME`-aware. Read `summary.json` verbatim; title is the official `display_title()`, hidden sessions skipped per `is_hidden()`.

Grok specifics:

- **The encoded-cwd dir name is never decoded** — read `info.cwd`. Short cwds are percent-encoded and long ones use a slug+hash, so decoding breaks on the second form.
- **ALL empty sessions are DROPPED.** Grok opens a new session dir on every CLI entry, so blank shells abound and a cwd holding only those would surface as a noise project. This diverges from grok's own picker on purpose — surfacing one empty per cwd is a picker affordance, not history.
- **`num_messages == 0` alone is NOT sufficient to call a session empty.** It counts turns grok injects on entry, and how many depends on the DIRECTORY. Decide in three tiers so the common cases never read a file: zero messages → empty; non-empty title → real; otherwise stream the transcript for a real user turn.
- **A real user turn carries no `synthetic_reason` AND does not unwrap to nothing but envelopes.** Both halves are load-bearing — plain-text synthetics carry no envelope. **ONE definition shared with the renderer**, or a session gets listed whose every message renders as "system".
- Sibling noise to skip: `session_search.sqlite`, per-cwd `prompt_history.jsonl`, `.lock` files.

### Grok has TWO on-disk message formats

Dispatch by the scanned file's name. **`chat_history.jsonl` is CURRENT** and preferred; **`updates.jsonl` is the ACP stream**, used only when chat_history is absent.

- **Envelopes are never shown raw**: `<user_query>` unwraps to the message text; every other envelope becomes an italic `*[tag]*` notice; a synthetic or all-envelope message renders with the SYSTEM role.
- **Tokens come from `updates.jsonl`**, riding each `turn_completed`. **Usage is PER-TURN, so turns SUM** — the lifetime sibling is a different ledger, and `numTurns` counts loop rounds within one prompt.
- **Grok nests its token subsets** (cached reads inside input, reasoning inside output) while `TokenStats` wants four DISJOINT values — subtract both out or you double-count. `signals.json` context figures are window occupancy, not consumption, and are deliberately unused.

### Memory

- **Claude** — `~/.claude/projects/<canonical-git-root-slug>/memory/**/*.md` (keyed off the canonical git root so worktrees share one dir), `~/.claude/rules/**/*.md`, `<cwd>/.claude/rules/**/*.md`.
- **Codex** — `~/.codex/memories/**/*.md`, skipping `skills/`.
- **Grok** — `<grok-home>/memory/**/*.md`; global at the root, workspace-scoped under `{slug}-{hash}/`.
- **Gemini** — `~/.gemini/GEMINI.md` AND `~/.gemini/MEMORY.md`, plus `~/.gemini/tmp/<id>/memory/`, skipping `skills/`.

**Per-project instruction files**, scanned at cwd and — **only when a `.git` exists at or above cwd** — every ancestor up to the git root:

| File | Tag |
|---|---|
| `CLAUDE.md` | `claude,opencode` |
| `CLAUDE.local.md` | `claude` |
| `AGENTS.md` | `codex,opencode` |
| `AGENTS.override.md` | `codex` |
| `GEMINI.md` / `MEMORY.md` | `gemini` |
| `<cwd>/.claude/CLAUDE.md` | `claude` — cwd ONLY, a project-root convention |

Global: `~/.claude/CLAUDE.md` → `claude,opencode`; `~/.codex/AGENTS.md` + `AGENTS.override.md` → `codex`; `~/.config/opencode/AGENTS.md` → `opencode`.

**The ancestor walk gates on `.git`** because all three open-source tools refuse to ascend without a project-root marker. Claude Code is the outlier — it walks to the filesystem root — and we deliberately apply the stricter rule.

**Deliberately NOT scanned**: `AGENTS.local.md` anywhere, `~/.codex/instructions.md`, `~/.claude/CLAUDE.local.md`, `CONTEXT.md`, and `project_doc_fallback_filenames` from the user's codex config.

### Skills

| Tool | Global | Project | Tag |
|---|---|---|---|
| Claude | `~/.claude/skills/` | `<cwd>/.claude/skills/` | `claude,opencode,grok` |
| Codex | `~/.codex/skills/` (NOT under `memories/`) | `<cwd>/.codex/skills/` | `codex` |
| Gemini | `~/.gemini/skills/` | `~/.gemini/tmp/<id>/memory/skills/` + `<cwd>/.gemini/skills/` | `gemini` |
| OpenCode | `~/.config/opencode/skills/` | `<cwd>/.opencode/skills/` | `opencode` |
| Grok | `<grok-home>/skills/` | `<cwd>/.grok/skills/` | `grok` |
| Tool-neutral | `~/.agents/skills/` | `<cwd>/.agents/skills/` | `codex,gemini,opencode,grok` at BOTH levels |

- **A memory/skill record's `id` is the file's FULL PATH**, never the filename or frontmatter `name` — those collide across projects, and the id keys both the backend path index and the frontend key.
- **Claude's project-dir slug is LOSSY** (`/`, `.`, `_`, `-` all become `-`) and cannot be reversed. Read the real cwd from the first records of a sibling session JSONL — only the first few lines, since these run tens of MB — and probe the filesystem before falling back to a naive decode.

## Reading and scanning

- **Cross-source record sorting is by PARSED instant, not string compare (LOCKED).** Sources emit different UTC offsets, and mixed-offset RFC3339 strings do not order lexicographically. Use `record_instant` (missing/unparseable sorts last). Within-source dedup sorts may stay string-based.
- **Scan-time extraction is mtime-cached and single-pass (LOCKED).** The Claude/Codex list scans get tokens + model + daily_tokens + message_count from ONE streamed pass per file, cached by `(path, mtime)`. **Do NOT reintroduce a per-scan full-file parse** — it pegs a core whenever a CLI is writing. Counting reuses the real message builders; a parallel "cheap predicate" would drift from the detail count.

### `read_record` elides over-long strings (LOCKED)

Used by the Claude/Codex JSONL reads, not every JSONL read. A record has no size bound — Codex writes a tool's output verbatim into one, and a runaway command produces a single line of hundreds of MB. It reads one newline-terminated record into a REUSED buffer and hands it to `from_slice`, never `from_str` on a fresh `String`.

- The LIST scan passes `SCAN_MAX_STRING_BYTES` (64 KB); the DETAIL parsers pass `DETAIL_MAX_STRING_BYTES` (1 MiB). **That split is the whole point**: the scan never keeps message text, so eliding a tail leaves the JSON structure untouched and the message builders reach identical decisions and an identical count.
- **1 MiB is CODEX'S OWN NUMBER (LOCKED)** — `DEFAULT_OUTPUT_BYTES_CAP`, which Codex applies to everything it retains from a command. This is alignment, not divergence.
- **What the cap buys is SEARCH, not "that session opens" (LOCKED — do not "simplify" it away as a fix for one old session).** `search_sessions` calls `get_session` for EVERY record, so one giant message was materialised on every query.
- **A cut may only land where it cannot produce invalid JSON — four hazards.** Getting any wrong is silent: `from_slice` fails, the record is skipped, its count and tokens vanish.
  1. **UTF-8 boundary** — measure the back-off against the OUTPUT, not the current run: a run that fits the budget can still end mid-character at a chunk boundary.
  2. **`\uXXXX`** — the four hex digits are ordinary bytes inside a bulk-copied run, so a cut between them must be forbidden byte-wise.
  3. **Surrogate pairs** — a lone high surrogate is rejected, so a cut after one must wait for its partner.
  4. **All-escape strings** — escape bytes are emitted without consulting the budget, so a string with no plain runs never reaches the eliding path. **BOTH escape forms need the guard**: a body of `\n` otherwise sails past the cap.
- **Write those escape tests with doubled backslashes in ORDINARY strings** (`"\\uD83D\\uDE00"`) so the body really holds the six ASCII bytes. A raw string containing real emoji never reaches the escape path and passes with the guard deleted.
- **Every newline ends a record, `in_string` or not.** For malformed input this is the difference between losing one record and losing the rest of the file.
- **Unlike Codex, which truncates silently, Termory says so.** A record that lost bytes emits an elided notice as its OWN message, pushed after that record's messages. Separate rather than appended because tool output is folded into a code fence where an italic notice renders as literal asterisks, and because its `kind::ELIDED` keeps it out of `kind::TEXT`, which is what the list scan counts.
- **The notice fires only when the record actually rendered something.** Codex persists a turn as both a `response_item` and an `event_msg` and one copy renders nothing, so an ungated notice reports the same loss twice.
- **The cap has DOWNSTREAM consequences — think twice before raising it.** Search matches the CAPPED text, so content past the cap is not findable; favourites snapshot what the list rendered. Checked and NOT affected: `first_match_index`, `message_count`, `merge_tool_outputs`.
- The elided-byte accounting is **signed** — the UTF-8 back-off can reach below where the run started writing, so an unsigned subtraction underflows.

- **`SearchCache` is bounded on BYTES, not entry count (LOCKED).** Entries are whole transcripts whose sizes differ by four orders of magnitude, so an entry limit bounds nothing. An outsized transcript is served but never retained. The budget is deliberately generous: search re-parses on a miss.
- **Do NOT add a macOS `malloc_zone_pressure_relief` reclaim.** `malloc_default_zone()` returns the nano zone, not the scalable zone the large allocations live in. If the retained-spike question returns, swap the global allocator instead.

## Records page

- The detail-load effect depends on narrow per-field deps (`source` / `path` / `id` / `updated_at` / `message_count`), not the whole `selected` object — a watcher rescan hands back a new object with the same identity and would re-trigger `load_session`. A ref distinguishes "new selection" (spinner) from "content advanced" (silent swap in a transition).
- Records opens with an EMPTY detail pane; boot does not parse the most recent session.
- `setRoute` runs in a transition so leaving a long detail is not blocked by the next route's render.
- Messages render through a virtualized `MessageList` with `measureElement`.

**Display**

- `AppSession.preview` carries comma-separated tool tags; the card renders one brand badge per tag.
- **Project-level `AGENTS.md` / `AGENTS.override.md` are ALWAYS tagged both `codex` and `opencode`** — the spec is tool-neutral, so Termory reports which tools CAN read the file.
- The source filter applies to all three panes; multi-tagged files appear under each tool.
- **Empty official titles stay empty.** No scanner may derive a title the official tool would not show. **Rendering is a separate layer and DOES substitute** — the four title sites fall back to `records.untitled`. **Do NOT push that fallback into the backend.**
- Brand identity lives in `<BrandIcon>`, colours baked into the SVG `fill`. **Do NOT redraw brand marks by hand — fetch the official asset.** Callers resize via `className`.

## Unified tool-message format — LOCKED RULE

Every tool message uses the same markdown shape, regardless of platform. Any new tool or structured-result formatter MUST follow it.

`````
{status} **{Verb}**({args})

⎿ {summary}              ← when the tool has a structured summary

```{lang}                ← optional fence for diff / source / structured output
{body}
```

or

````                     ← 4-backtick fence for unstructured text output
{body}
````
`````

1. `{status}`: `⏺` success, `✗` failure. Applied to every tool card on every platform.
2. `{Verb}` is platform-native; the wrapper `**Verb**(args)` is identical everywhere.
3. `{args}` always passes through `wrap_inline_code`. **This applies to EVERY line carrying user payload**, not just the header — the secondary `↳` lines are just as exposed.
4. **`⎿ ` prefix is REQUIRED on every summary line**, one trailing space before the content. Tools without a structured summary skip the line. NEVER put `⎿` inside a code fence — browser monospace fonts render U+23BF inconsistently.
5. Summary content matches the per-tool Claude TUI component verbatim, including count bolding and pluralization (`⎿ Read **N** lines`, `⎿ Added **N** lines, removed **M** lines`, `⎿ Error: Exit code N` for Codex / `⎿ Error: {message}` for Claude).
6. Reasoning on all platforms: `format_reasoning_body` → `> *line*` italic blockquote.
7. **No content is dropped to match official TUI behavior.** Suppressed tools and synthetic wrappers all surface, usually as italic `*[wrapper-name]*` notices. Termory is a history browser; hiding things misleads the user.

**What breaks in markdown, verified through the real `MessageBody` pipeline:**

| bare text | renders as | |
|---|---|---|
| ``echo `date` `` | `echo date` | ✗ backticks eaten |
| `cp *a* dir/` | `cp a dir/` | ✗ `*x*` pair → italic |
| `/a/ _tmp_ /b` | `/a/ tmp /b` | ✗ `_x_` at WORD BOUNDARIES → italic |
| `/a[1](b)/c` | `/a1/c` | ✗ `[x](y)` → link |
| `my_project/a_b.md` | unchanged | ✓ INTRAWORD `_` is not emphasis |
| `ls *.md` | unchanged | ✓ a lone `*` needs a partner |
| `/a[1]/c` | unchanged | ✓ `[…]` with no `(…)` is not a link |

**Bold is NOT a hazard by itself** — intraword `_` does not emphasize, so underscore-heavy values inside bold survive. Do not "fix" those; it only adds noise.

**Render it, don't reason about it.** `MessageBody` is cheap to drive from a scratch vitest file that dumps `container.textContent`; anything unequal to the source string is a bug.

**Failure detection** (`SessionMessage.exit_code` carries the parsed value through `merge_tool_outputs`):

| Platform | Signal |
|---|---|
| Codex | `Process exited with code N` / `Exit code: N` in the `function_call_output` wrapper; populates `exit_code` |
| Claude | `tool_result.is_error: true`; no exit code field |
| OpenCode | `state.status === "error"` on a tool part; `assistant.error` for whole-message failures |
| Gemini | `status` on each `toolCalls[]` entry; anything other than `'success'` is an error |

### Per-platform verb mapping

**Every branch must cite the source file that produces the verb in that TUI.**

| Platform | Verb source |
|---|---|
| Codex | `tui/src/exec_cell/render.rs`; shell names in `rollout-trace/src/tool_dispatch.rs` |
| Claude | `src/tools/<Tool>/UI.tsx` — `userFacingName` + `renderToolUseMessage`; wrapper `AssistantToolUseMessage.tsx` |
| OpenCode | `packages/tui/src/routes/session/index.tsx` — one named function per tool |
| Gemini | `packages/cli/src/ui/components/messages/ToolShared.tsx` |
| Grok | per-block headers under `xai-grok-pager/src/scrollback/blocks/tool/` + `acp/tracker.rs` |

**Do NOT assume the verb equals the tool name.** Claude's `ReportFindings` renders as `Code review`; `ReadMcpResource` as the literal camelCase `readMcpResource`; `McpAuth`'s whole label IS the verb. Grok's TUI header is not the taxonomy `presentation_name` — `web_fetch` shows as **Fetch**. Check whether `renderToolUseMessage` exists at all.

**Traps not visible in the tables:**

- **Codex Limited vs Extended mode.** The CLI default is Limited, so most rollouts carry only `FunctionCall` + `FunctionCallOutput` for shell tools. The function-call-output path is authoritative. Do not also dispatch `exec_command_end` without call_id dedup — it duplicates the card.
- **Codex Code Mode (`exec`): the script is the ONLY record (LOCKED).** Inner `tools.*` calls produce no rollout entries, so the script plus its output is the entire history. Its `input` is script SOURCE, not JSON args, so the generic arg-compacting path flattens a multi-line script to one truncated line.
- **`custom_tool_call_output` has TWO shapes** — a JSON envelope string, and an ARRAY of content items (Code Mode). Handling only the envelope drops the entire output message.
- **The generic fallback's label comes from the official GUI**: per-tool label else `lowerCase(tool)`, then `upperFirst`. **The word split is load-bearing** — Codex tool names are snake_case, so printing the raw name shows the internal symbol where the GUI shows a phrase.
- **OpenCode's two error sites have DIFFERENT wire shapes (LOCKED).** A failed tool part carries `state.error` as a plain STRING; a whole-message assistant failure carries a NamedError OBJECT (`{name, data:{message}}`). Reading `error.message` — which NEITHER has — renders every failed card as a bare `Error` and surfaces assistant failures as nothing. One shared helper mirrors the TUI's `errorMessage()`.
- **Claude's newest attachment types come from the shipped binary**: `agent_listing_delta` (label counts `addedTypes`; the noun is agent TYPES), `read_truncation_notice`, `context_tip`. For `context_tip`, **both `tip` and `action` are surfaced** — `action` is the command the tip tells you to run, and dropping it is what rule 7 forbids.
- **Align the DATA TRANSFORMATION, not the presentation (LOCKED).** Upstream is authoritative for which recorded field becomes which piece of display content. It is NOT authoritative for whether Termory shows something (rule 7) or how it is styled (rules 1–6). "Official hides this, so there is nothing to align" is the reasoning that leaves fields unrendered.
- **OpenCode `Execute`: always fence the output**, though the TUI shows it only on error. A rule-7 visibility call, NOT up for "alignment" — the script's return value is summarised nowhere else.
- **Gemini's `<session_context>`** is the FIRST user record of every session. Surface it as a `role: "system"` notice with the tag unwrapped and the directory tree fenced. **Never skip it** — skipping breaks the session title and cascades.
- **`isMeta` user records have no official mapping** — Claude drops them outright, so the `*[meta]*` prefix and its don't-stack rule are Termory's own presentation under rule 7.
- **Claude's suppressed tools are RENDERED here** — `TodoWrite`, `AskUserQuestion`, `EnterPlanMode`, `TaskCreate`, `ToolSearch` and the rest, with a per-tool body where one exists and the generic fallback otherwise.

Body decorations stay platform-native (BlockTool titles, `$ cmd` bash fences, ```diff fences, `↳ Loaded` lists, todo icons); only the header is reshaped.

**OpenCode's TUI lives in `packages/tui`**: rendering in `routes/session/index.tsx`, message DATA in `context/data.tsx`, with `synthetic` and `compaction` as PART types rather than message types.

### Shared helpers

- **`wrap_inline_code`** picks a backtick delimiter longer than the longest run inside the content and pads when it starts or ends with one. Use it wherever an unsafe payload becomes inline code.
- **`format_reasoning_body`** escapes stray `*` / `_` so an italic span cannot break mid-line.
- **`merge_tool_outputs`** folds a matching result into the leading tool card, switching the marker to `✗` and prefixing the fence with `Error:` on failure. Orphan results keep their text and still get a status prefix.
- **`strip_ansi`** is required for OpenCode Bash output and shell captures — an inline state machine, no regex crate.
- **Provider-native combined formats skip `merge_tool_outputs`** — they arrive complete with their own fence.
- `SessionMessage`'s `tool_use_id` and `exit_code` are parse-time only (`#[serde(skip)]`).

### Markdown rendering

- **No DOMPurify / rehype-sanitize is needed**: react-markdown emits React elements, not HTML strings, so raw `<tag>` is auto-escaped and displays as literal text.
- **No raw/rendered toggle** — every message goes through one pipeline; "open original file" covers inspecting the record.
- No syntax-highlight pass; code blocks are plain monospace until a per-language renderer is added deliberately.
- Layout is TUI-style continuous scrollback, no card borders. Role colours are CSS vars read by inline styles, so the palette stays single-sourced.
- **`.message-body` is the only meaningful CSS class.** Its rules are load-bearing: left padding aligns body text under the role label, `word-break: break-all` on inline code lets long paths wrap, and the fence margins keep a diff flush under its verb header. If the summary above an Edit diff sits flush-left, that alignment regressed.
- Unordered lists use a `- ` text marker to match Codex's TUI.

## History and preview behaviour

- **Session lists come from the same stored records the official tool uses**, with official values for title, project, timestamps, id and path.
- **Show everything that was recorded.** Termory surfaces what the official TUIs hide. **List-time filters deciding WHICH sessions appear are separate and still apply.**
- **Compatibility readers are for real older layouts only** and must never override the current official path.
- **App-only UI features are not evidence for official data behaviour.**

## Subprocess management (`process.rs`)

**Every subprocess goes through this module — never call `.spawn()` / `.output()` directly (LOCKED).** A grep for a bare spawn outside it should come back empty.

**Three kinds of child, deliberately different — do NOT unify them:**

| Entry point | Used for | Killed on app exit? |
|---|---|---|
| `probe` / `probe_with_stdin` | `--version`, `security(1)`, `which` | n/a (synchronous) |
| `spawn_managed` | login flows, `claude update` | **yes** |
| `spawn_detached` | the user's terminal | **never** |

The third row is load-bearing: quitting Termory must not take down a terminal the user is working in, so detached children are absent from the shutdown registry and handed to the reaper for the zombie fix ONLY. `probe_with_stdin` exists for `security -i`, the form Claude Code itself uses so a credential never appears in `argv`.

- **Killing must reach grandchildren.** `Child::kill` hits the wrong process whenever what we spawned is a wrapper (an npm shim, `$SHELL -l -i -c`) — a killed wrapper leaves the real binary re-parented and still holding its port. Each managed child gets a container the OS tears down as a unit: a process group on unix, a Job Object with kill-on-close on Windows.
- **Both `wait()` and `terminate()` sweep the group — a NORMAL exit is not a free pass.** "The wrapper exited" says nothing about the process doing the work. Sweeping at reap time is also what makes it safe: the group id is reserved while the group is non-empty, so the signal can only reach our own descendants. **Leaving it to `Drop` does not work** — `Drop` skips the group once the child is marked finished.
- **The grace period is the CALLER's choice, not one constant (LOCKED).** A child that ignores SIGTERM costs the full wait every time, so a value picked for the slowest case is paid by every other. Long grace for an installer that should clean up a partial download, short for a cancel the user is watching. **A timed-out probe gets NO grace.** `Drop` sends only SIGKILL — a TERM with an uncatchable KILL behind it is a no-op.
- **Probes poll with a BACKOFF, not a fixed interval.** A `--version` exits in about a millisecond, so a flat interval puts that floor under every probe and costs hundreds of ms per tray-menu open across five CLIs.
- **Probes drain both pipes on their own threads for the whole run.** A pipe blocks the writer once full, so polling `try_wait` without reading deadlocks the child against its own output; the only way out is the timeout, which returns nothing for a command that succeeded. Reading afterwards is too late. Failure paths deliberately do NOT join the reader threads, or a grandchild still holding the pipe reintroduces the hang.
- **A successful probe also kills its group** — whatever the child left running has outlived the command. Consequence: the shell fallback sources the user's rc, so a non-detaching background process started there is taken down with the probe.
- **Every SILENT spawn hides its console window on Windows** — a property of `probe` and `spawn_managed`, not a rule each call site must remember. A GUI-subsystem parent flashes a console for every console child, and `.cmd` shims route through `cmd.exe`. `hide_console` stays public for the one case that is neither: the Windows terminal launch, where **the outer helper is hidden and the terminal never is** — which is why `spawn_detached` does not apply it.
- **Shutdown sweeps only MANAGED children**, with a short grace so a wedged child cannot hold the quit. The registry holds a shared owner because the sweep and a live handle can each outlive the other, and on Windows releasing twice is a double `CloseHandle`.

**Known limits — do not read the module as airtight.** Unix has no kill-on-close equivalent, so a SIGKILLed Termory leaves managed children running; Windows cannot assign the job until `CreateProcess` returns, so a grandchild forked in that window escapes; a child that deliberately breaks away leaves the container by design. The upgrade and version-probe kill paths are UNMEASURED against a real CLI.

**Testing traps:** a test that spawns and kills immediately proves NOTHING — the parent dies before it forks, so wait on a marker the child writes first. And assert by IDENTITY, never by registry length or a global count — the suite is parallel, so tests holding a live managed child must take the shared sweep lock.

## CLI detection

**Detection does NOT trust the inherited `$PATH`** (`providers::cli_search_paths`, mirrored by `watcher::install_watch_targets`): it stats a fixed ORDERED dir list — installer env var, per-user dirs, system dirs, then `$PATH` as a catch-all. `find_cli_binary` returns the FIRST hit, so **order is behaviour**. Verify each rule against the installer, never infer.

- **`~/.local/bin` is cross-platform, NOT unix-only.** Claude Code's launcher uses `.local\bin\claude.exe` on Windows and codex's installer defaults `BIN_DIR` there. A `#[cfg(unix)]` gate makes the InstallGuide's own recommended Windows install undetectable.
- **Windows has NO shell fallback.** `shell_version_fallback` is `#[cfg(unix)]`, so Windows detection is entirely the dir list plus the process `$PATH`, and an installer's freshly-appended PATH entry is invisible to a running process. A missing dir there means "undetectable until re-login".
- **The shell fallback is CACHED (`shell_installed_cached`, TTL 10 min) because it is on a hot path.** The `||` only short-circuits on a HIT, so the ~1s interactive-shell spawn happens for every CLI the user does NOT have, on every tray-menu open. Staleness is harmless — a real install is found by the dir scan, and both the watcher and Recheck call `clear_shell_probe_cache()`.
- **Anything run through the interactive shell is MARKER-SCOPED (`SHELL_PROBE_MARKER`).** An interactive shell sources rc files and many print a banner onto the same stdout, ahead of the real output; `parse_version` takes the FIRST version-shaped token, so the banner wins. Build such commands with `marked_shell_command` and read through `after_shell_marker`, which returns only what follows the marker and **`None` when the marker never appeared** — falling back to the whole text is the bug. It splits by POSITION, never by matching banner content.
- **Claude Code's legacy local install** (`claude migrate-installer` → `<claude-config>/local`, honoring `$CLAUDE_CONFIG_DIR`) stays in the list: it is deliberately NOT on PATH, so only the slow shell fallback could find it, and on Windows nothing could.
- **Windows-only landings**: Claude via winget → `%LOCALAPPDATA%\Microsoft\WinGet\Links` (user) / `%ProgramFiles%\WinGet\Links` (machine); Codex standalone → `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`, a DIFFERENT path from its Unix `~/.local/bin`.
- **Every entry must trace to a real installer; cite the script next to it.** An unjustified entry costs a stat AND manufactures false provenance for the next reader.
- **Installer env overrides are probed FIRST** (`$CODEX_INSTALL_DIR` / `$GROK_BIN_DIR` / `$OPENCODE_INSTALL_DIR` / `$XDG_BIN_DIR`). An explicitly chosen dir must outrank every package-manager dir, else a stale npm copy wins the first-hit race.
- The watcher mirrors the list with two deliberate omissions — write-hot dirs where unrelated events would each cost a re-probe: Linux `/usr/bin`, and `~/go/bin` + `$GOPATH/bin`.

**The settle drain must NOT swallow install events (LOCKED).** After each burst the watcher drains ~300 ms (`SETTLE_WINDOW`), because reading the session SQLite files touches their `-wal`/`-shm` sidecars and FSEvents reports those straight back. A package manager installs by deleting the old binary and writing the new one, and that write lands inside the drain window — so bin-dir events (`event_touches_install`) must set `install_settled_late` rather than being dropped, and the watcher re-probes after the drain. The re-probe reads only, so it cannot feed itself new events; session rescans are NOT re-run.

**Codex counts as installed when EITHER the CLI binary OR the desktop app is present** — they share `~/.codex/` auth and config.

- **Detect the macOS app by bundle id `com.openai.codex`, NEVER by `.app` name** — the Codex app IS the unified ChatGPT desktop app, so existing installs are named `ChatGPT.app`.
- **Detect the Windows app by package NAME (`OpenAI.Codex` / `OpenAI.CodexBeta`), never the DisplayName** — Windows brands it "ChatGPT" while the identity stays `OpenAI.Codex`. Distribution there is Store-only, so MSIX is the only form to check. Prefix-match the publisher-hash suffix; never hardcode a hash.
- **Presence and version are split by cost.** Presence is a pure `read_dir` of `%LOCALAPPDATA%\Packages\` and may run on the hot path (every tray-menu open, on the main-event thread). The PowerShell `Get-AppxPackage` version fetch loads the Appx module and **must stay on the cold path** or it freezes the tray on every click.
- **`bundledCli`** is the codex CLI shipped inside the desktop app (`<bundle>/Contents/Resources/codex`, macOS only). `codex_binary()` is standalone CLI first, else bundled, and every codex spawn routes through it. **There is no Windows bundled fallback** — `codex_binary()` is standalone-only there.
- **"Which version is installed" and "can a codex be run" are SEPARATE questions (LOCKED).** `codex_standalone_cli_installed()` answers the VERSION side; `codex_cli_installed()` (that OR the shell probe) answers EXECUTION. Do not merge them with `||`: the shell probe only proves "some codex answered", never WHICH, so using it on the version side makes the CLI segment describe a binary the user never installed.

**Latest-version sources — each matches what that tool's OWN installer reads:** Claude Code → `downloads.claude.ai/claude-code-releases/latest` (npm is not its primary install and can lag); Codex CLI → npm `@openai/codex`, with `~/.codex/version.json` as an offline fallback; Gemini → npm `@google/gemini-cli`; OpenCode → npm `opencode-ai`; Grok → `x.ai/cli/stable`; Codex desktop app (macOS) → its Sparkle appcast, read from `sparkle:shortVersionString`, **never `sparkle:version`**, filtered by `hardwareRequirements` and `minimumSystemVersion` before taking the max. That static feed can be ahead of what a given user's app is offered, so the badge may appear early.

- **Claude Desktop has NO update check, deliberately.** Its updater requires a per-device `device_id` telemetry token and there is no public alternative. Querying a vendor's identified update API from a third-party app is a different proposition from the public distribution endpoints.
- A single fetch failure yields `None` for that tool and must never error the whole call. The frontend fetches this INDEPENDENTLY of the installed-version probe — the badge must never block the installed-version render.

**`ProviderOfficialCard.versions` is a LIST of `VersionSegment {text, label?, latest?}`, not a preformatted string.** A string cannot say WHERE the update badge belongs, so a Codex CLI update renders at the end of the line and reads as the desktop App being out of date.

## CLI upgrade (`upgrade.rs`)

- **Four CLIs ship their own upgrade command, used VERBATIM** — `claude update`, `codex update`, `opencode upgrade`, `grok update`. Each detects its own install method using signals unreadable from outside. **Do NOT replace these with a Termory-inferred package-manager command.**
- **Codex's is the BARE `codex update`, never `codex_shell_invocation()`.** That helper falls back to the desktop app's bundled binary by absolute path — right for LAUNCHING, wrong for upgrading, since codex classifies the bundled copy as `InstallMethod::Other` and refuses. No app's upgrade command may contain an absolute path or a `.app/` segment.
- **Gemini is the only app needing inference** — it has no update subcommand. `install_method_from_path` reads the CANONICALIZED path: `/Cellar/` or a macOS `/opt/homebrew`|`/usr/local` prefix → Brew; `/.bun/` → Bun and `/pnpm/` → Pnpm, both tested BEFORE the generic `/node_modules/` → Npm arm because their global roots contain that segment too. Command forms follow Codex's own (`install -g <pkg>`, no `@latest` pin) except pnpm, whose global add is `add -g`.
- **The run goes through `$SHELL -l -i -c` (LOCKED).** A GUI process inherits launchd's bare PATH, where `npm`/`brew`/every CLI is absent — and `codex update` itself spawns `npm install -g`, with that npm living in nvm/volta. **`-l` alone is NOT enough**: zsh reads `.zshrc` only when INTERACTIVE.
- **stdin AND stderr are both `Stdio::null()` (LOCKED).** stdin: `opencode upgrade` prompts when it cannot identify the install method and `claude update` can want a sudo password — with no stdin those exit non-zero instead of hanging. stderr: the command's own stderr is folded in by `2>&1`, and **a piped stream with no reader deadlocks** once the buffer fills.
- **The start marker strips rc noise (don't remove).** The shell echoes it before the command and the reader drops everything up to it; without it a FAILURE is reported as the rc banner instead of the real reason.
- **The output reader takes BYTES and decodes LOSSILY — never `lines()` (LOCKED).** `lines()` returns `Err` for a line that is not valid UTF-8, which the consuming loop reads as end-of-stream. Not an edge case on Windows: a console child writes in the system's OEM code page, so on any non-English install the CLI's own error text is not UTF-8. The cost is everything AFTER that line, plus a drain that stops while the child keeps writing.
- **The Windows child is a DIFFERENT shell form**: `cmd /C "echo <marker>& <cmd> 2>&1"`. cmd.exe has no rc file, but the marker is echoed anyway so the reader's filter is identical on both paths.

### Update badge

- **One pure function decides the badge, the component only renders it.** `upgradeBadgeState(segment, {upgrading, error, canUpgrade})` returns `{label, tone, clickable, disabled, tooltip}`. Four states: amber idle (tooltip: the command) · amber `↑ Upgrading` with the label swapped and `disabled` so a second click cannot start a parallel run · red after a failure, still clickable to retry, tooltip = reason + command · amber non-button for a segment with no `upgradeCommand`.
- **While an upgrade runs the badge renders no tooltip element at all.** A disabled button dispatches no hover events, and a tooltip left open from before the click keeps rendering the command — which is re-probed mid-run, and mid-reinstall Codex briefly resolves to the desktop app's absolute path.
- **The card's layout NEVER changes (LOCKED)** — no progress row, no error row; every state rides on the badge. **The terminal is only ever TEXT in the failure tooltip, never a button** (LOCKED). The red state is deliberately redundant with the failure toast — it persists after the toast dismisses.
- **`upgrading`/`upgradeError` are per-APP, the badge is per-SEGMENT.** A segment without a command is display-only and ignores app state — Codex renders CLI + App and only the CLI is upgradable.
- **`cli_upgrade_commands` must NOT join `refreshVersions`'s `Promise.all` (LOCKED).** A secondary fetch must never block or blank the installed-version render.
- `onUpgrade` takes **no arguments** — the backend derives what to run from the app alone.
- The success toast carries **no version number**: for Codex "updated to vX" is ambiguous across its two segments.

## Terminal picker (`terminal.rs`)

**Three steps and nothing else (LOCKED — resist re-growing it).** `detect()` lists what is installed, the pick comes from the `terminal` config key, `open()` launches it; an empty / `auto` / unrecognized id is the Default row. A picker that opens a terminal needs no state and no second probe.

| | Rows are | Command ends up in |
|---|---|---|
| macOS / Linux | terminal APPS | the user's `$SHELL` (`exec $SHELL`) |
| Windows | **SHELLS** | that shell itself |

Windows lists shells because it has **no `$SHELL` equivalent** — `%COMSPEC%` is a constant, not a preference. The terminal comes from Windows' own "default terminal application" setting, which `start` honours, so **no Windows row may name a terminal**. A handed-off console therefore shows Windows Terminal's DEFAULT profile; that is not a bug, and making the tab say otherwise would mean naming a terminal again.

- **Each platform's argv construction is a PURE function**, so per-terminal flags are unit-testable on any dev machine; only the spawn is `#[cfg]`-gated.
- **ONE definition per list** drives both detection AND launch, so an added row cannot miss a launch arm.
- **`xfce4-terminal` and `tilix` take ONE string, not an argv vector** — Tilix registers `-e` as a STRING option and never rejoins leftover argv. Its own man page says the opposite and Claude Code's launcher passes a vector; **both are wrong, the source wins.**
- **Anything TYPED into a shell leads with a SPACE (LOCKED).** Terminal.app's `do script` and iTerm's cold-launch `write text` deliver the command as KEYSTROKES, so it lands in the tty buffer while the shell is still sourcing rc files — and **an rc file that reads a keystroke eats the first character** (oh-my-zsh's update prompt does `read -r -k 1`). A leading space is harmless either way: the `read` falls to its default arm, and with no prompt the shell ignores a blank. **Control characters do NOT work** — `^U` is the tty's kill character, swallowed by the line discipline. **Never add the prefix to the argv-based launchers**, where the command is an ARGUMENT nothing can consume.
- **macOS cold launch opens TWO windows** unless the AppleScript reuses the window the launch itself creates.
- **On Windows the project dir rides as the SPAWN cwd**, never inside the command string — a nested `cd /d` hits the MSVCRT-vs-cmd quoting mismatch and mangles real paths.
- **Git Bash is found by absolute path, not PATH.** `bash` on PATH is WSL's entry point, so probing for it can run against a different OS entirely; `git-bash.exe` is not on PATH at all. Walk only the dirs the installer can produce. Its launch also needs an empty title argument first, because `start` reads a quoted first token as the window title.
- **There is deliberately no macOS CLI-probe branch**: a Dock-launched process inherits launchd's bare PATH, so such a fallback could only fire in dev — the branch under test would never be the branch users get.
- **A saved id no longer on offer renders as Default**, and the config is deliberately NOT rewritten: reinstalling that terminal restores the choice.

## Providers

A Provider is a named snapshot of `{baseUrl, apiKey, model, …}`; each CLI has its own list. Activate materializes it into the CLI's live config. Switching back to Official clears only the Termory-injected fields — **the CLI's own OAuth/credentials file is never touched**, so logins survive a round-trip.

**Termory stores no "active provider" pointer.** Active state is reverse-derived on every read from the CLI's live config, which keeps it correct when cc-switch, a hand-edit or the CLI's own login changes the same files.

**`CliApp::key()` is the single app→string mapping** — the exact inverse of `CliApp::parse` and the same literal the frontend union uses. It keys config maps and tray menu ids, so a second copy would silently orphan a tool's settings.

**Local storage** — `~/.termory/`, `0700` dir / `0600` files on Unix, atomic write (tmp + rename).

- `config.json` — UI prefs, no secrets. Two keys are read by BOTH sides and each needs a Rust const beside a frontend mirror: `active_provider_ids` (per-CLI record of the last switch, used by both reverse-derivations to disambiguate identical-creds entries) and `codex_keep_all_sessions` (read PER SWITCH on both sides).
- `providers.json` — one UNIFIED `providers` array holding per-CLI providers (`kind: "official"|"custom"`) AND gateways (`kind: "gateway"`), split by `kind` on read; **each writer must preserve the other kind**. Contains API keys.

**Lenient config parsing (LOCKED — valid JSON must never error).** Syntactically valid JSON must never make the code fail, even when it holds a value this build does not recognize. The canonical case is a DOWNGRADE: an older binary reading a providers.json a newer version wrote. That is Termory's own data, so an unrecognized entry is **skipped**, not fatal; only a real syntax error may fail.

- Parse the array entry-by-entry and drop failures; `Gateway.bindings` uses a lenient deserializer so an unknown-app binding drops just that binding.
- **IPC commands taking a provider LIST use the `ProviderList` newtype**, or one unknown entry fails the whole call at Tauri's arg-binding layer before the handler runs. Single-provider args stay strict.

**Editing a live provider re-applies it.** Saving the editor re-runs `activate_provider` when that provider is currently live, or the edit silently does nothing until a manual re-activate. Pass the PREVIOUS copy in the strip set too, so option keys the edit REMOVED get cleaned. If it was also the startup default, re-run `set_default`.

Empty top-level `Provider` fields are stripped on write. `model` is the single model for single-model apps and the OPTIONAL default for the multi-model ones (OpenCode / Grok), chosen FROM `models`. **The models map is built purely from the list, in list order** — `model` is never injected into it. List order survives because `serde_json`'s `preserve_order` is on, which also means every Termory JSON write preserves the user's existing key order.

**Claude has no dedicated block**: its per-size `/model` routing rides `options` as `env.ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL`. Those keys are deliberately NOT managed, so they pass through; `[1m]` appended to a value selects the 1M context window. **Reverse-derivation matches on base URL + api key only**, so a `[1m]` suffix never affects active-state matching. Both editors block save on duplicate or managed keys — `isManagedOptionKey` mirrors `override_key_is_managed`, **keep the two in sync**.

### Per-CLI materialization

Cite the official source next to each branch; do not infer from docs.

| CLI | Writes | Never touched |
|---|---|---|
| **Claude Code** | `~/.claude/settings.json` (merge): `env.ANTHROPIC_BASE_URL` / `_AUTH_TOKEN` / `_MODEL` + optional sub-routing keys. **Always writes `AUTH_TOKEN` and CLEARS `ANTHROPIC_API_KEY`** — Claude reads AUTH_TOKEN first, so a leftover API_KEY would outlive the switch | `.credentials.json` / Keychain |
| **Codex** | `~/.codex/auth.json` (merge) + `config.toml` (merge): `auth_mode = "apikey"`, `OPENAI_API_KEY`; top-level `model_provider` / `model`; a `[model_providers.termory]` block with `wire_api = "responses"` and `requires_openai_auth = true` | the `tokens` / `last_refresh` / `agent_identity` fields INSIDE auth.json |
| **Gemini** | `~/.gemini/.env` (dotenv merge, 0600): `GOOGLE_GEMINI_BASE_URL` (this triggers gateway mode), `GEMINI_API_KEY`, `GEMINI_MODEL`; other vars preserved | `oauth_creds.json`, `google_accounts.json` |
| **OpenCode** | `~/.config/opencode/opencode.json` only, under `provider.termory-<id>` | **`auth.json` is never written** — it belongs to `providers login` / `/connect` |
| **Grok** | `~/.grok/config.toml` (merge, atomic): one `[model."<pid>-<mid>"]` entry per picker model | `config.json` |

- **Never set Codex's `env_key`** — it forces a hard env-var path with no fallback. `requires_openai_auth = true` is what gates the auth.json load.
- **Codex's provider id is pinned to `termory`**, and its reserved ids must never be overwritten. Codex keys session history by `model_provider`, so a per-provider id would visually drop history on every switch.
- **OpenCode: enabling rebuilds the whole `provider.<id>` block**, so options are scoped per-provider and removed keys vanish on re-enable. **Sibling providers' blocks and options are never touched** — no top-level strip. The top-level `model` default is set only by `set_default`.
- **Grok is MULTI-SLOT and MULTI-MODEL.** `[model.*]` is a FLAT list, one entry per picker row, owned per provider by the `<pid>-` key prefix so siblings coexist. Write `[model]` IMPLICIT so no bare empty table is emitted. Enable and set-default are SEPARATE: enable writes entries only; set-default writes `models.default`.
- **Grok's `options` are GLOBAL config.toml keys and apply ON SET-DEFAULT, not on enable.** Everything Termory writes lands in the one config.toml shared with the user's settings, and grok has no per-provider scope — applying on enable makes multiple enabled providers fight over the same top-level keys. Only the ONE default provider's options are live: strip the union of all grok providers' keys, then apply the current one's. Managed keys (`models.default`, `model.*.<field>`) are skipped.
- **`apply_toml_overrides` takes the app** — hardcoding one CLI lets another's managed key through.
- **Do NOT touch `endpoints.xai_api_base_url`** — grok's internal endpoint override, not the custom-model mechanism.
- **All grok paths honor `$GROK_HOME`** (config, session scan, skills, watcher target), like `CLAUDE_CONFIG_DIR`.
- **Factory grok config has no `models.default` and no `[model]` entries**, so deleting every Termory provider must return to exactly that shape.
- `reject_duplicate_model_ids` is shared by grok / OpenCode / Claude Desktop activate and both editors.
- **Base URL is REQUIRED on every provider (LOCKED)** — the per-CLI writers treat an empty string as "CLEAR this field", so activating with a blank one strips the endpoint while STILL writing the API key, leaving the CLI pointed at the OFFICIAL endpoint holding a third party's token. Guard it at the single dispatch, not per writer.

### Claude Desktop (3P gateway profile)

Managed as a **provider**, not a CLI: `is_cli()` is false, so it is excluded from terminal flows and `$PATH` probes, detected by config-dir existence, and platform-gated to macOS/Windows. **Its tab always shows**; install state gates activation and gateway binding, never tab visibility.

**Direct mode only.** cc-switch's proxy mode is out of scope — it needs a local HTTP proxy Termory does not have.

- Both config files get `deploymentMode: "3p"` (merged), plus a profile with `inferenceProvider: "gateway"` + the `inferenceGateway*` fields and an optional model list. "Restore official" flips them back to `1p`, deletes the profile and strips the gateway keys.
- **NEVER degrade a read/parse failure to an empty document.** A mid-write Claude Desktop leaves torn JSON, and truncating the file to just our keys would destroy the whole config. All writes are snapshot + rollback / atomic.
- **Managed keys are checked by BOTH the exact key AND its dot-path ROOT**, so an option like `inferenceGatewayBaseUrl.x` cannot clobber a managed scalar.
- **Model ids are hard-validated** against Claude Desktop's own rule (`claude-*` / `anthropic/claude-*` plus a known role); a non-Anthropic id blocks save, matching its `invalid_config` rejection.
- **Windows config is MSIX-VIRTUALIZED**: the app writes `%APPDATA%\Claude`, but an unpackaged reader must use `%LOCALAPPDATA%\Packages\Claude_<publisherhash>\LocalCache\Roaming\Claude`. Scan by the `Claude_*` prefix — **never hardcode the publisher hash**. A newer Squirrel install form uses an un-virtualized `%APPDATA%\Claude`, so both must resolve, and the `-3p` dir is resolved INDEPENDENTLY because the MSIX form pre-creates it before any normal config dir exists.

### Model inputs (`ModelCombobox`)

- **It renders INLINE — no portal, no Radix Popover (LOCKED).** Both editors live inside a Radix Dialog, where a portaled popup is blocked by the Dialog's `pointer-events: none` + `react-remove-scroll` lock; separately, Radix Popover freezes the Tauri WebKit overlay.
- **cmdk's `CommandInput` does not forward `id`**, so each field is labelled via `aria-label`, not `<Label htmlFor>`.
- **The multi-model "Default model" field is a RESTRICTED Radix `Select`**, not free text: its items are exactly the ids added in the list above plus a sentinel mapping back to no-default, so the default can only ever be one of that provider's own models.

### Codex "follow sessions" on provider switch (Codex-ONLY)

**Codex tags every thread with the `model_provider` active at creation, and `codex resume` lists ONLY threads matching the CURRENT config provider.** After a switch to a custom platform, a project's prior official-era sessions vanish from `codex resume`. **No other CLI has this** — the others list resume history by project/path.

At switch time the user picks which projects follow into the now-active bucket. For each selected project's non-target sessions:

1. **Rewrite the rollout JSONL's first-line `payload.model_provider` FIRST** — it is authoritative. The `threads` table is a CACHE Codex rebuilds from the rollout's `session_meta` on backfill, so **editing the table alone is silently reverted the moment Codex runs**. Stream the remainder unchanged, atomic temp + rename.
2. Update `threads.model_provider` for the same rows, for visibility before the next backfill.
3. **Restore the original file mtime.** The resume picker shows each session's time from the rollout file's MTIME, so a naive rewrite makes every session read "just now".

**No journal, no per-session bookkeeping (deliberate).** The official bucket is ALWAYS `openai` and Termory's is ALWAYS `termory`, so the fold target is fully determined by switch direction and reversal is the symmetric switch-back. **Do not add a change-journal without a new requirement.**

**Safety.** Open RW with a busy timeout and surface "database is locked — quit Codex" rather than writing. Only successfully-rewritten files get their row updated, so a partial failure self-heals via Codex's backfill. No DB backup: the table is a self-healing cache.

**Only an official↔custom transition changes the bucket**, so the prompt fires only then — custom→custom stays `termory`. The Gateways tab must route through the same prompt in BOTH directions. **A tray-handed switch prompts UNCONDITIONALLY and must NOT re-derive the direction**: the tray already established that the bucket changes, and the marker map is still empty right after the window opens, which reads as "already Official" and skips the prompt.

**Settings → "Keep all sessions on a Codex switch"** replaces the prompt with silent all-projects following on BOTH entry points, read PER SWITCH so it applies immediately. It is what lets a menu-bar switch complete without opening the window.

**"Activate & keep" is DISABLED until at least one project is checked** — with nothing checked it would be identical to "Activate only". On confirm the order is activate → toast → migrate → toast; migration only runs if activation landed.

**Memories do NOT need this.** Codex memories have no `model_provider` column, so switching never hides them. Injection is local and un-gated; only GENERATION is auth-gated, which is a Codex limitation.

### Gateways

A gateway is ONE `{baseUrl, apiKey}` that may speak several API modes; add it once, detect which modes respond, then BIND it to the CLIs whose required mode matches. Additive — it does not touch per-CLI provider management.

- **Storage**: gateways live in the same unified `providers` array, discriminated by `kind: "gateway"`. Each writer preserves the other kind.
- **Mode ↔ CLI**: Claude → Anthropic Messages; **Codex → OpenAI Responses specifically** (`wire_api="responses"`, so the `/v1/responses` probe is the one that matters); Gemini → Gemini API; OpenCode → any mode; Claude Desktop → Anthropic. On top of capability, the editor **install-gates every app**.
- **A binding is a provider minus the gateway's common fields, WITH its own id.** There is **no `protocol` field** — it is derived. The own id is what lets one CLI hold several bindings, and it is the activation-marker key.
- **Probe each capability at its OWN real API endpoint** (POST, empty body, route-exists). **A GET `/models` list NEVER gates a capability** — every compatible gateway answers it, so it only produces false positives. **Gemini is the exception**: its `/v1beta/models?key=` path is Gemini-specific, so data implies support.
- **The auto-detect dedup memo must be gated on the RESULT still existing (LOCKED).** Editing the base URL or key CLEARS the capabilities, and a memo that outlives the result makes that unrecoverable: clearing the key and pasting the SAME key back matches the signature, runs no probe, and leaves the editor with no capabilities.
- **Gateway base URL is stored PATH-LESS.** The editor strips a pasted `/v1` or `/v1beta`, and each CLI's real URL is derived per protocol. Otherwise a base entered with a path breaks the Gemini one.
- **One root does not always serve every mode — the Anthropic mode gets a SECOND probe under `/anthropic`.** A family of vendors (DeepSeek, Moonshot) keeps the OpenAI-compatible API at the root while mounting the Anthropic one under a prefix, which otherwise leaves Claude Code and Claude Desktop unbindable on a vendor that supports them. The sub-path probe runs CONCURRENTLY with the root one, and **the ROOT wins whenever it answers**, so the recorded path is a fallback, never a default. Anthropic protocol ONLY. A base already ending in the prefix is left alone. **Only this one prefix is probed** — every candidate costs a request on every detect.
- The Rust `Gateway` needs a read-only `capabilities` field because the tray activates through the same binding synth; without it a menu-driven switch derives a different URL than the page.
- **Activation reuses the existing path** via a synthesized `Provider` (id = the binding's id), so it flows through activate / deactivate / reverse-derivation unchanged.
- **A delete that cannot clear a binding's live config KEEPS the gateway** rather than losing it silently while a CLI still points at its endpoint; the retry is idempotent.
- When a standalone provider and a binding share identical creds they are indistinguishable on disk, so the per-CLI marker disambiguates which is "in use" — honored only while its creds still match the live snapshot.
- **Gateway bindings appear on the per-CLI Providers list as view + activate only, with NO Edit/Delete** — bindings are managed exclusively from the Gateways tab.
- **Known gap:** detection is best-effort; a permissive gateway can answer several modes on the same route.

## Official-account quota (`quota.rs`)

Reads each CLI's EXISTING OAuth login. **It never writes credentials.** Claude + Codex + Gemini + Grok are implemented; OpenCode has no official subscription quota.

- **While an add-account flow is running for that CLI, BOTH quota entry points bail out early (LOCKED)** — the IPC and the watcher's `force_quota_refresh`, gated on `login_in_progress`. A login flow OWNS the credential while it runs, and what it leaves on disk reads as LOGGED OUT. A `not_found` is treated as definitive everywhere, so it would clear the tray row and hide the quota section while the user is off in the browser.
  - Return a **PLAIN FAILURE, not `not_found`**: both the page merge and the tray keep the last good numbers for a failure and clear for a logout, and those numbers stay CORRECT because every flow ends on the pre-login account.
  - Return **WITHOUT stamping the rate-limit marker**, or the genuine refresh that follows gets refused by a floor this non-fetch earned.
- **Every credential read is TIMED and wrapped in `spawn_blocking`.** A locked Keychain makes `security` block on its unlock dialog, and `fetch_quota` is async on both paths, so an untimed read parks a Tokio worker indefinitely.
- **Claude** — the Keychain service name is DERIVED (`Claude Code-credentials` + a sha256(config-dir) suffix when `CLAUDE_CONFIG_DIR` is set), falling back to the credentials file. `parse_claude_usage` reads the flat top-level windows through the fixed allowlist `CLAUDE_KNOWN_TIERS`: **an unrecognized window key is DROPPED, never passed through** — Anthropic uses that namespace for internal codenames. Adding an id there also needs its label in `tierLabels` and `tray_tier_label`, or it renders as a raw id.
  - The newer `limits` array carries MODEL-scoped weeklies named by `scope.model.display_name`, each with its own `group`. **Take the group from the API, never infer it**: which models get a window, and which periods exist, differ per account. The label composes `{period} · {model}`, and an unrecognized group renders VERBATIM so a new period surfaces without a release.
  - **Credit amounts are in MINOR units** (`decimal_places`), so the frontend divides by `10^decimal_places`; grok already stores dollars.
  - The endpoint's newer `spend` object and per-window `*_dollars` fields are **deliberately NOT parsed** — that billing data is not served to the CLI OAuth token.
- **Codex** — gated on `tokens` PRESENCE (the ChatGPT login), **NOT on `auth_mode`**: a Termory custom→Official round-trip leaves tokens with no auth_mode and the quota must survive it. A pure API-key login has no tokens → `not_found`.
- **Gemini** — an expired access token is re-minted from the refresh token via the public installed-app client and **never persisted**.
- **The GROK quota path NEVER refreshes the token (LOCKED).** auth.x.ai rotates refresh tokens WITH reuse detection, and grok persists the rotated one to auth.json under its own file lock. The quota path is a read-only background fetch bound by the never-write-credentials rule, so refreshing here spends a token it has nowhere to put and leaves grok holding a dead one — which LOGS THE USER OUT. Use the stored access token while valid, report Expired otherwise ("run grok once"). **The scope split is the rule**: `switch_grok` DOES refresh, because it already holds grok's lock and writes auth.json. **Gemini's re-mint above is NOT a precedent** — that token has no reuse detection and no on-disk owner to invalidate.
- **Grok is CREDIT-based, and a missing percentage is NOT 0% (LOCKED).** Utilization follows grok's own order: an explicit percent, else the deprecated used/limit pair, else **unknown** — and unknown emits NO tier. The endpoint serves no usage percentage at all for some accounts, so defaulting to 0 gives those users a confident 0% ring that never moves. Grok reports exhaustion through a 429 on the CHAT request, never here. **An empty result shows NOTHING** — no rings, no placeholder copy; a ring reads as authoritative where a line of text does not.
- **Grok has TWO mutually exclusive credit models and both must surface**: legacy accounts get an on-demand cap (the "Credits" ring, `$used / $limit`), unified-billing accounts buy PREPAID credits (a balance, stored as NEGATIVE cents by accounting convention). A unified subscriber has `onDemandCap: 0`, so without the prepaid field a bought balance renders as nothing. The prepaid item has **no ring and no low-balance tint** — a balance has no limit to divide by.
- **A `410 Gone` from any usage endpoint maps to `not_found`** — the account no longer has this resource, so the card hides cleanly instead of toasting.
- **Reset countdown — exactly ONE window gets it**, chosen by `waitingOnTierName`: nothing spent → the SOONEST-resetting window; something spent → the LATEST-resetting SPENT window (every spent window must clear before work resumes).
  - **"Blocking" ≠ "spent".** A spent MODEL-SCOPED window leaves other models usable. `isAccountWide(name, group)` splits them: a `group` settles it immediately (model-scoped); otherwise the account-wide id list decides. **The list is written from the ACCOUNT-WIDE side deliberately** — model names are dynamic and cannot be enumerated, so a new model correctly lands on the model-scoped side. A new ACCOUNT-WIDE window arrives with an unknown id and must be ADDED there; that upkeep is the intended workflow.
  - Account-wide windows decide whenever any exist. Model-scoped ones take over only when there are NO account-wide windows and every model window is spent. A plain "every model window is spent" rule is wrong for Claude, whose scoped weeklies cover only some models.
  - Resolve the chosen name to ONE row INDEX before rendering. A model-scoped window's `name` is a MODEL name and could equal an account-wide id, and a per-row comparison then lights the countdown on both — the same reason the row key is `{group}:{name}`.
- **A quota result belongs to ONE login, so an account switch INVALIDATES it (LOCKED).** Every caching rule assumes the identity is unchanged; otherwise the 2-min floor makes the post-switch re-fetch a no-op, and "failures keep the last good numbers" pins the previous account's tiers there indefinitely. Both sides drop the entry before re-fetching, including the frontend module cache.
  - **Dropping the entry must also drop the rate-limit marker**, which records when the PREVIOUS account was fetched.
  - Clearing is what makes the failure path correct: with nothing retained, a failed post-switch fetch shows NOTHING rather than someone else's usage.
  - The post-switch fetch passes `force` to bypass the **in-flight guard** — a fetch started for the previous account can still be running and would swallow the re-fetch.
  - Results already in flight when the switch landed are dropped by comparing `queriedAt` against the invalidation instant; `queried_at` is stamped when a fetch COMPLETES, so a re-fetch started after the switch always counts as newer.
- **The whole quota block lives in `useQuotas.ts`, not in ProvidersPage** — fetch, both floors, the module cache, the two backend events, the cooldown clock. The page takes a dozen props and drives 18 IPCs, so none of these rules could be asserted through it.

## Wallet balance (`balance.rs`)

The account behind ONE `{baseUrl, apiKey}`. The IPC arg is a `BalanceSubject { id, baseUrl, apiKey }`, **not a `Provider` (LOCKED)**: a Gateway satisfies it structurally, so nothing has to invent a fake CLI to satisfy a field the query never reads. It never returns `Err` — unrecognised host, missing key and failed request are all `status` values.

- **`base_url` IDENTIFIES the vendor; it never builds the request URL.** `detect_vendor` matches it, then the request goes to a hardcoded constant on the vendor's own domain. That decoupling IS the feature's boundary: a relay resolves to `Unsupported` rather than having its key sent somewhere. **Relays are out of scope** — they have no common balance API.
- **Detection is HOST-scoped, not whole-URL `contains` (LOCKED).** Routing a vendor behind a path prefix is a real relay convention, so `https://relay.example.com/openrouter.ai/v1` matches a substring check — and then sends the relay's key to openrouter. Strip scheme/userinfo/port/path first; substring matching WITHIN the host is kept so regional subdomains resolve. `.cn` is tested before `.com` for SiliconFlow, so arm order is behaviour.
- **A missing amount is an ERROR, never 0** — the one deliberate divergence from cc-switch, which `unwrap_or(0.0)`s every field and so renders a confident "$0.00" that never changes when a response shape moves. Each parser returns NO entry when its anchor field is absent, and an empty vec becomes `status: Error`. `depleted` uses the vendor's OWN flag where one exists.
- **Novita reports 0.0001 USD minor units** (÷10 000); skipping that reports 10 000× too much. DeepSeek reports per CURRENCY, so `entries` is a list.
- **Adding a vendor needs a verified source for its endpoint** — one `detect_vendor` arm, one `endpoint` arm, one parser. Only DeepSeek's shape is confirmed against a live API; the other five are unverified.

**Balance UI**

- **Every ENTRY the user added is its own independent subject — add N, get N readings (LOCKED).** A standalone provider queries under the provider id, a gateway binding under the binding id, a gateway under the gateway id. One vendor account reached through several entries is read several times, and that is correct. **Do NOT dedupe by credentials** to save a request: it second-guesses how the user set things up and reworks the result tag, the event matching and the tray rows.
- A gateway is ONE `{baseUrl, apiKey}` = one wallet however many CLIs it binds, so the reading belongs on the **gateway card**, not repeated on each binding row.
- **The balance sits on the TITLE row, never in the `<dl>` below (LOCKED).** A balance is live account state; the `<dl>` holds stored settings. Sharing that row brings two layout rules: the title takes `truncate` + `min-w-0` (**the NAME yields** — a clipped amount is a wrong number), and the row takes `min-h-8`, the action cluster's own height, so both columns centre on one line. `min-h-8` is on `ProviderCard`, so EVERY card gets it. **Do NOT "fix" the alignment with a negative margin on the button**: a 32px control squeezed into the 28px `text-lg` line centres on the WRONG axis. Fix the container, not the child.
- **The value slot holds a balance and NOTHING else (LOCKED).** No status word, no error text; a number once read STAYS, and a failed refresh changes only the button. It renders whatever entries survived and hides the row only when no balance was ever read. The one tooltip exception is the tinted `depleted` state — red is the only thing the amount cannot explain itself.
- **The refresh button carries every other state.** Tooltip precedence is **loading → error → cooldown → idle**: a failure selects the SHORT retry floor, so the button sits in cooldown right after failing, and letting the cooldown text win would hide the reason exactly when it exists. Trigger on a wrapper `<span>` — a disabled element fires no hover events.
- **Floors match the quota's** (120 s success / 60 s failure, on the automatic pass, the manual click and the tray alike) so "when does this number update" has one answer. They stay SEPARATE constants and separate throttle markers: sharing a marker would let a quota fetch suppress a balance fetch. **`unsupported`/`no_key` never expire** — decided with no request, they can only change with the base URL or key.
- **Cache is keyed by the id we ASKED for, never `result.providerId`**, and validated against `balanceCredsKey` (`baseUrl` + NUL + `apiKey`). **A BACKEND-pushed result must also check `credsChangedAt`**: the payload carries no record of which credentials produced it, so one already in flight when the user edited the provider would be stamped with the CURRENT ones, read as fresh, and leave the old account's balance on the new card.
- Failures toast only on a MANUAL refresh. `balanceErrorToast` is deliberately a LOCAL copy of the shape `useQuotas` uses — hoisting it would couple two separate features.

## Multi-account (Codex + Claude Code + Grok Build)

Login snapshots in `~/.termory/accounts.json`. Termory writes the CLI's own credential file ONLY on switch/login.

- Each entry's `payload` is the **parsed credential as a JSON object**, not a JSON-encoded string. `migrate_account_entries` applies its arms IN SEQUENCE, each gated on the version that introduced it — **never `return` out of an arm**, or the oldest data skips the newest cleanup — and `read_store` persists the upgrade on first read.
- **Every switch re-snapshots the OUTGOING login first.** All three CLIs rotate refresh tokens, so a days-old entry holds a rotated-away token and the switch back becomes a `needsRelogin`.

**Account snapshots vs. provider switching share `auth.json` — field ownership is LOCKED.** `activate_codex` writes `auth_mode` + `OPENAI_API_KEY` and deliberately leaves the OAuth `tokens` beside them, which is what makes an Official↔custom round-trip lossless — and also means a live login still reads as a valid ACCOUNT while a custom provider is active. So the two features must not write each other's fields:

- **`read_codex_live` strips the provider-owned fields at a SINGLE point**, so no consumer has to remember to. The id's last-resort hash must hash the STRIPPED doc, never the file text — hashing the raw file splits ONE login into two accounts the moment a provider is activated.
- **`switch_codex` carries those fields over from the CURRENT auth.json**, not from the snapshot (absent there ⇒ removed here). Writing no key is always safe (Codex falls back to `tokens`); writing a stale one from an unrelated snapshot is the bug this prevents. Net: **switching accounts never changes `OPENAI_API_KEY`**, in either direction, from either entry point.

**Claude**

- Credentials go through the two-tier store: macOS Keychain via `/usr/bin/security` — the SAME binary Claude spawns, so the entry's ACL matches and no auth prompt appears — else a file. The Keychain tier is `not(test)`-gated so unit tests never touch a real keychain.
- The `oauthAccount` identity object is restored into `~/.claude.json` — **the one narrow exception to "Termory never writes `~/.claude.json`"**. Only that key is replaced, atomically, with other keys and the file mode preserved.
- **`~/.claude.json` writes NEVER degrade a read/parse failure to an empty doc.** A mid-write claude leaves torn JSON, and truncating the file would destroy the whole global config.
- The switch confirm carries a quit-running-claude hint: a running instance write-through-caches `~/.claude.json` and would roll the identity back, and no lock exists to detect it.
- **The tray pays ZERO for claude accounts until one is saved** — the live read spawns `security(1)` and `build_menu` runs on the MAIN thread, so it gates on the store having claude entries first.

**Grok**

- **The payload is SCOPE-SCOPED (`{scope, auth}`), not the whole document.** auth.json can also hold a plain API key or an enterprise OIDC login under another issuer; a whole-document restore would wipe whichever the snapshot predates. The API-key entry is excluded by requiring a non-empty `user_id`, not by hardcoding its key.
- A corrupt auth.json is renamed to `auth.json.corrupt.<millis>` before a fresh write, mirroring grok's own recovery.
- **The refresh token is the whole hazard.** auth.x.ai rotates it and runs reuse detection: spending one twice revokes the token family. The blast radius is one `grok login`, but it is SILENT and arrives hours later. Three rules follow:
  1. **Refresh the snapshot before committing it.** Restoring a spent token "succeeds" and then logs the user out at grok's next refresh; validating first turns that into an error at click time. **429/5xx/network degrade to writing the snapshot verbatim** — the server said "not now", which says nothing about the token, and flagging those would trap a healthy account behind a disabled button.
  2. **Re-snapshot the outgoing login before overwriting it.**
  3. **Hold grok's own `auth.json.lock` across all of it, INCLUDING the IdP call**, and write the `PID:TIMESTAMP` holder info — stale holder info is what makes a grok waiter break a lock it should have waited on. Never delete the lock FILE; unlinking is grok's break-a-stuck-holder signal, not a release.
- **The refresh patches exactly the four fields grok's own refresh replaces**, carrying every other field over in place. **Two rules are copied, not invented:** a response WITHOUT a `refresh_token` keeps the OLD one (the IdP rotates only sometimes, and treating the missing field as a value blanks the only way back into the account), and a missing `expires_in` REMOVES `expires_at` rather than leaving the stale one. Persist to the STORE before the live file — the refresh may have spent the old token, so a crash between the two writes must leave the durable record holding the live one.
- **`grok login --device-auth` is REQUIRED, not a preference**: the loopback branch clears the credential up front, so abandoning that login logs the user out; the device branch skips the clear and is also what prints the verification URL. Its prompt goes to **stderr**; the code is read positionally (the next non-empty line after a heading ending in `code:`) so its format is never assumed.

**Automatic sync of the live account** — the CLI keeps rewriting its own credential underneath us, so the entry for the account in use goes stale with nothing the user did, and a stale entry is a switch-back that fails. This re-derives that ONE entry through the same builder the Save button uses, so `name`/`email`/`plan` are parsed from the very document that becomes the payload.

- **The watcher is the main path.** Claude's credential lives in the Keychain and emits nothing, but two files beside the config dir move when it changes, and they are routed SEPARATELY because only one is a credential: the `<config-dir>.lock` (taken for exactly one thing — refreshing the OAuth token) and `.claude.json` (the only file a LOGIN touches).
- **`.claude.json` must NOT join the credential route (LOCKED).** That map also feeds `force_quota_refresh`, which bypasses the quota's own floor, and `.claude.json` is Claude's whole global config, written from 159 places in its source. Routing it there turns a three-trigger feature into an API call every ten seconds while Claude is in use. The sync can afford it where the quota cannot: a pass that finds nothing costs one credential read and writes nothing.
- Both signals sit one level ABOVE the watched tree and the lock exists only while held, so their PARENT is watched non-recursively with a name filter. That parent is `$HOME` in the default layout, which churns constantly, so it must also join the rescan gate's ignore list.
- **The only other trigger is ONE catch-up pass at launch** — for whatever changed while Termory was closed. **There is deliberately NO periodic pass**; it would be the only recurring task in the process.
- **`syncedAt` is this flow's OWN field, `savedAt` is not (LOCKED).** `savedAt` is written by USER actions, `syncedAt` only here. Each writer replaces the whole entry, so each must carry the other's field over. **Both are excluded from the content comparison**, or every later pass differs and you get a store write and a "synced" report every minute, forever.
- **Only ever REPLACES an existing entry, matched on `app` + `id`.** Never creates — auto-saving every login is a different, opt-in feature. `app` is part of the match because an id is NOT globally unique: both derivations fall back to the EMAIL, so one person signed into two CLIs with the same address holds two entries under one id.
- **The lookup runs on the copy about to be written**, not the one read at the top: the credential read between them can take a hundred milliseconds, and a DELETE landing in that window would otherwise be resurrected forever. A mutex does not fix this — it closes the wrong window, and a plain `Mutex` is not reentrant.
- Gated on `login_in_progress` and on disabled sources. **Grok's auth.json is read WITHOUT taking grok's lock** — acquiring stamps the lock file, which lives in the recursively-watched grok home, so every pass would trigger a full session rescan; a torn read is caught by the parse instead.
- **A credential that cannot be READ is not a failure to report.** The watcher fires ON the write, so a read landing mid-write parses as garbage; reporting it would paint the footer red for a CLI that is working fine. Only a failure to WRITE the store is reported.
- The change event is emitted ONLY on a real update. The page re-reads on success only and **silently**; a failed pass changed nothing, and re-reading risks replacing good rows with an error. It deliberately does NOT move `lastSyncedAt`, which dates the session scan.

**`rateLimitTier` → the Max multiplier in the plan label.** `subscriptionType` alone cannot tell Max 5x from Max 20x. **Only Max takes a multiplier** — the same tier string appears under a TEAM subscription, so appending it elsewhere invents a "Team 5x" that means something else. Unrecognized or absent tiers fall back to the bare plan; the value set is not closed. **Both plan derivations go through one helper**, or an account shows `Max` on the quota card and `Max 20x` on its account row.

Gemini returns a DISPLAY-ONLY `current` with an empty `accounts` list — no snapshot management.

**`switch_account` is refused while an add-account flow owns that CLI's credential (LOCKED), at BOTH entry points** — the IPC and the tray. A switch WRITES auth.json and the flow overwrites it when it ends, so the user's choice is silently undone after a success toast. The tray needs its own check because its row is reachable with the window closed. **The guard must NOT move into `accounts::switch_account`**: the login flow calls that to restore the previous account while still holding its own slot. **`delete_account` is deliberately NOT guarded** — it only touches the store, so the worst case is a live login with no saved entry, which is recoverable and surfaced.

**Cancelling a codex login must ASK the server to stop, not kill the child (LOCKED).** An npm-installed codex is a node WRAPPER that spawns the real binary, and the wrapper is the only handle we hold. `Child::kill` sends SIGKILL, which is uncatchable, so the wrapper dies before it can forward the signal and the real binary is re-parented and keeps running — a live OAuth server still holding the port. The browser tab still has the real state, so finishing that login writes credentials AFTER the rollback restored the previous account. Send codex's own `GET /cancel`, wait up to 2 s for the child to exit, then fall back to a group-wide terminate.

- **Only `DEFAULT_PORT` (1455), never the fallback port beside it** — codex gates its own `/cancel` the same way. Sweeping both fires at whatever else occupies a port we did not bind.
- **The send result is checked; the RESPONSE is not.** Nothing listening means nothing to wait for, so kill immediately instead of sitting out the grace period.
- **The CONNECT timeout (300 ms) is separate from the reply timeout (2 s).** Unix answers a closed loopback port with an instant ECONNREFUSED; **Windows sends no RST** and sits out the full timeout, so one shared constant makes every serverless cancel a 2 s stall. Defence in depth — there is no known everyday trigger.
- **The verdict is whether the CHILD EXITS**, not a response code.
- Scope is codex only. `claude auth login` and `grok login` were each observed to be single processes that cancel cleanly.

**The tray is rebuilt by the LOGIN ipc, on BOTH outcomes, and never by the cancel ipc (LOCKED — all three CLIs).** Firing the cancel is not the rollback: the flow still has to stop the child before restoring auth.json, so a rebuild in the cancel ipc reads the blanked file and every saved account compares as inactive. Bind the result, rebuild, then return it — a rebuild behind `?` is skipped on every failure path.

**The pre-overwrite snapshot is split across TWO helpers:**

- **`resnapshot_live_before_login` — UNCONDITIONAL**, used by all three login flows. A login DESTROYS the outgoing credential and all three CLIs rotate refresh tokens, so the copy on disk is the only one that still works. An "already saved, nothing to do" shortcut leaves the restore at the end of the flow reaching for a rotated-away token.
- **`auto_save_unsaved_live_account` — ONLY-IF-MISSING**, used by the tray's switch, where the question is "would this unattended switch lose a login the user never saved?" Freshness is already handled by the switch's own outgoing re-snapshot.

Every mutating account command calls `tray::rebuild_menu` after its write — the tray lists saved logins under each CLI's Official row.

## Tool toggles (Settings → Tools)

Per-app on/off switches plus a drag-sortable order. Stored in config.json as `sources` (`Partial<Record<CliApp, bool>>`, **absent key = enabled**) and `source_order` (`CliApp[]`, frontend-only).

- **Gemini is OFF by default** because Gemini CLI stopped serving individual accounts (HTTP 410); enterprise Code Assist still works. It needs an explicit `true` to appear. **Do not "fix" the default-off list as a bug.**
- **`DEFAULT_OFF_KEYS` (config.rs) ⟷ `DEFAULT_OFF_SOURCES` (provider-utils.ts) is a keep-in-sync mirror pair.**
- **The frontend gate is ALWAYS `isSourceEnabled()`** — never inline truthiness, since an absent key means enabled.
- **ONE backend filter point**: `apply_source_toggles`, a post-pass at the end of `scan_sessions`, so Records / Search / Stats / the tray recent list all follow. Memory/Skill records drop only the disabled tags from `preview` and vanish when none remain.
- **`refresh_session_path_index` runs BEFORE that filter**, so `load_session` by `(source, id)` still works for a disabled source — otherwise Favorites snapshots of a disabled tool become unopenable.
- **`write_app_config` is a PER-KEY merge**: the frontend sends only the one changed `{key, value}`. A whole-object write round-trips the startup cache and resurrects renamed/orphan keys forever. When `sources` changed it also rescans, emits `SOURCES_CHANGED_EVENT`, and rebuilds the tray.
- **The reorder uses `@dnd-kit/sortable`; do NOT hand-roll drag.** WKWebView's native `dragDropEnabled` interception swallows HTML5 drag events and drop-on-target cannot insert after the last row; raw pointermove oscillates at the midpoint. Grip-button-only listeners keep the Switch clickable.
- **`GatewayEditor.handleSave` carries a hidden app's existing bindings over VERBATIM** — rebuilding them from drafts silently drops a disabled tool's binding on any save.
- The LAST enabled tool's switch is disabled, so all-off is unreachable.
- **The disabled set rides in `InstallSnapshot.disabled`** so `build_menu` / `terminal_clis` do ZERO config I/O on the main thread. Quota and account-sync triggers skip disabled CLIs.

## Menu-bar tray

**Click model per OS**: macOS left click opens the menu; Windows/Linux put the menu on RIGHT click and open the window on left click. Linux appindicator trays deliver no click events at all.

**Recent sessions**

- A record with neither a title nor a snippet is DROPPED rather than rendered as a placeholder — several such rows are indistinguishable, and this menu exists to click the conversation you mean. Filter on the recent-sessions mapping, not the shared picked vector (it also feeds the project list, and a project whose sessions are untitled is still somewhere you work), and filter BEFORE the take so the list stays at full length.
- Click ids are **content-addressed and looked up by identity**, so a watcher rebuild while the menu is open cannot resume the WRONG session; a missing id is a silent no-op.
- The New Session id carries the full project path, fully decoupled from the recent list. **The `tray:newpick:` dispatch check must stay FIRST**, since it shares a prefix with `tray:new:`.
- The session id is charset-guarded before interpolation into the resume command — injection defence.
- The tray **never scans on its own**: it is refreshed from the existing scans.

**Menu mutation**

- **The refresh splices the dynamic region IN PLACE, never `set_menu`** — on macOS a full rebuild CLOSES an open menu, and refreshes must apply immediately without dismissing it. Any splice failure falls back to a full rebuild.
- ONE deliberate exception: an installed-set change forces a full rebuild even from the menu-open path, because the splice cannot refresh the per-CLI submenus.
- **ALL menu mutations are QUEUED on the main thread.** The queue serialises concurrent refreshers that would otherwise interleave remove/insert ops or write back a stale menu handle. A mutex cannot replace it: worker-holds-lock-waits-main against main-waits-lock deadlocks.

**Recent-session live work status (Claude-only)**

- Reads Claude's OWN runtime file (`~/.claude/sessions/<pid>.json`, the snapshot it writes for `claude ps`) — not the transcript, not a hook.
- **Plain text, no glyph**: a native macOS menu is an all-text list, so a coloured dot means either an emoji or a far-left icon column that breaks the otherwise icon-less menu.
- Filter by Claude's exact filename guard (all-ASCII-digit stem), a **PID-liveness probe**, and a 24 h mtime backstop against PID reuse. mtime alone cannot tell a long-busy live session from a crashed one.
- **Refresh is event-driven, no timer.** But a CRASH leaves the file untouched and emits no event, so the tray CLICK also re-runs the liveness probe against the CACHED list (no session scan) and re-splices.
- **Claude-only is a DECISION, not a gap — do not re-derive it (verified against the codex clone 2026-08-24).** Codex can be told it is MID-TURN but never that a session is merely open, and the mid-turn half cannot be made trustworthy:
  - `should_persist_event_msg` (`rollout/src/policy.rs`) persists `TurnStarted` / `TurnComplete` / `TurnAborted` unconditionally, in BOTH history modes, and the recorder flushes every record — so the rollout tail carries turn boundaries. **On disk they are named `task_started` / `task_complete`** (serde `rename`, with the `turn_*` spelling only an alias) plus `turn_aborted`; codex's own mid-turn rule is `snapshot_turn_state` → `ends_mid_turn` (`core/src/thread_manager.rs`), and `rollout/src/reverse_jsonl_scanner.rs` is its tail reader.
  - **`ends_mid_turn` is NOT "working".** A killed or crashed codex writes no `turn_aborted`, so its `task_started` dangles forever. Liveness is required, and the ONLY thread→pid link on disk is `logs_2.sqlite` (`logs.thread_id` + `logs.process_uuid`, formatted `pid:{pid}:{uuid}` in `state/src/log_db.rs`). **Its coverage is incidental**: only events whose tracing span carries `thread_id` are tagged, so a real session can hold zero such rows — the feature would light up for some running sessions and not others. It is also a write-hot multi-hundred-MB db whose `-wal`/`-shm` reads feed the watcher's settle window.
  - **`threads` in `state_5.sqlite` has NO runtime status column** (`state/migrations/0001_threads.sql`; the only status-bearing table, `thread_goals`, was added and dropped again in `0033`/`0034`), rollout mtime reads identical for an idle-but-running session and a closed one, there is no `codex ps`, and no per-session pid or lock file exists anywhere.
  - **Two look-alikes that are NOT session state**: `~/.codex/ipc/ipc.sock` is the IDE EXTENSION's socket, which the TUI only connects to as a client (`tui/src/ide_context/ipc.rs`); and the `app-server daemon` pidfile is opt-in remote-control plumbing for SSH hosts, Unix-only, absent from an ordinary TUI install.
  - Gemini and OpenCode write no such file either.

**Quota inline title**

- Rendered from the API's own fields (`TrayTier { name, group, used }`), **not a pre-rendered label**, so a language push relabels the cached quota instead of freezing the locale it was fetched under.
- A pressure emoji per window (🟢 <75%, 🟡 ≥75%, 🔴 ≥90%) because macOS menu text cannot be coloured. **Credits and the prepaid balance carry NO glyph** — every other segment's glyph reports a used/limit ratio, and a balance has no limit to divide by.
- **Suppressed while a custom provider is active** — the quota belongs to the official login, so gluing it onto a custom provider's name misreads as that provider's usage.
- **THREE triggers, no periodic polling**: a startup warm-up, every tray click (rate-limited per CLI, sharing one marker with the page), and every completed quota IPC. Plus the watcher's credential-file branch, which bypasses the normal floors so login/logout reflects near-instantly.
- Changed numbers are applied via `Submenu::set_text` on cached row handles. **A full rebuild here would close the open menu** — and the fetch is triggered by opening it.
- A `not_found` (logged out — definitive, unlike a transient failure) CLEARS that CLI's entry rather than keeping stale numbers, as does a successful fetch with no usable windows. **UNLESS credits or a prepaid balance are present**, in which case the entry stays.

**Balance in the tray**

- The CLI row's suffix is **mutually exclusive with the quota's by construction**: quota needs Official active, balance needs a custom provider. `Codex · Official · 🟢 12% 5h` and `Codex · DeepSeek · ¥89.44` are the two shapes of one slot.
- **No pressure glyph** — 🟢🟡🔴 encode how much of a LIMIT is spent, and a balance has no limit to divide by.
- `CliRow` holds the whole `Provider`, not its id: the triggers run on the MAIN thread, where re-reading providers.json is the cost the cached-row design exists to avoid. The title suppresses a figure cached under a DIFFERENT provider — showing the old number under the new name is the one wrong answer.
- Cache the API's own values, not a rendered string. Update via `Submenu::set_text`, **never `set_menu`**.
- **`balance_supersedes` gates every store.** Two fetches for one CLI can overlap and arrival order is not reading order; and `set_text` on an open menu is not free. It compares the READING (`provider_id` + entries), **not the record** — `queried_at` differs on every fetch, so folding it in makes "unchanged" unreachable.
- **Five triggers, no polling**: startup warm-up · tray click · every completed page fetch · every provider mutation · the page's tab entry. The provider-mutation trigger is the moment a row's SUBJECT can change; it clears EVERY CLI, since one providers.json write can move any number of rows. **There is deliberately NO watcher path**: a wallet moves when the user spends elsewhere, which leaves no local signal.
- **A PAGE fetch feeds the tray too (LOCKED)**, or the tray re-queries a wallet the page just read. It cannot be the quota's direct `(cli, result)` handoff: a quota belongs to a CLI by nature, while the page queries per CARD. The IPC looks up the row that names this provider and refreshes only that one; a provider active nowhere leaves the tray untouched, **marker included** — stamping one for a wallet no row shows would mute a row that never got the reading. This covers the Providers tab only by construction, since a gateway-bound row names the BINDING's id while the Gateways tab queries the GATEWAY's id.

**The submenu MIRRORS the Providers page, single-sourced through one resolver (LOCKED)** — the page is the reference for both what is listed and what a switch writes. The three lists are distinct and must not be conflated: the listed rows, the option-key STRIP SET handed to activate (**every kind** — narrowing it leaves a sibling's keys in the live config), and the full set handed to deactivate and set-default.

**Active state is reverse-derived**, with the marker record only disambiguating identical-creds entries. `id.is_none()` is NOT the same question as "Official": an Unmanaged live config also has no id, and reading that as Official ticks the Official row and hangs the official account's QUOTA off someone else's endpoint.

**The tray WRITES the marker too**, so both entry points leave identical state. The config-write IPC must rebuild when that key changes, because the page writes the marker AFTER the activate that already rebuilt.

**Codex switch handover** — a bucket-changing Codex click needs the follow prompt, which a native menu cannot host. Unless the keep-all setting is on, the tray parks the request, rebuilds (to discard the native self-toggle), shows the window and emits. **It first asks the same question the page asks** — does any project actually hold off-target sessions — and applies the switch directly when none do, so no window opens. A candidate-listing FAILURE prompts: switching silently could drop history from `codex resume`, while a needless prompt costs one dialog. That check runs off the menu-event thread because it opens sqlite. **`App.tsx` claims the parked request, not ProvidersPage** — that page only mounts on its own route, so a window sitting elsewhere would drop the switch. The claim is take-once, and the direction comes from the resolved active id, not the raw marker.

**Saved account rows** sit directly under Official with no title row — they are that login's accounts, not more providers. A row is DISABLED for `needsRelogin` or while a login flow runs for that CLI; **both conditions live in ONE place read by the build path AND the in-place refresh**, since a row enabled by one and disabled by the other is a reachable, sticky state. The LIVE row stays enabled so the account in use is not greyed out beside its own checkmark; clicking it is a guarded no-op.

- **A switch updates the rows IN PLACE, never by rebuilding** — the switch lands seconds after the click, and reopening the menu to see whether it worked is exactly what a user does next. Fall back to a full rebuild only when the account SET changed, or when the row title would be stale.
- Before switching, the tray auto-saves an unsaved live login: the page warns in a confirm dialog, and a native menu row has nowhere to warn.
- **Starting a login rebuilds nothing**, so the tray click must also refresh the account rows.

**Whenever an account or provider rule changes, change it on BOTH surfaces (LOCKED)** — the page and the tray are two front ends over one state, and a guard added to only one leaves the other as a silent way around it.

**Only INSTALLED CLIs get a submenu.** The install snapshot the VISIBLE menu reflects is cached and stored only AFTER a successful rebuild — storing it earlier lets a failed rebuild claim the new set and permanently kill the staleness check. Claude Desktop's marker is its config dir, which cannot be watched before it exists, so the watcher watches the PARENT non-recursively with a name filter; those busy shared dirs must also be excluded from the session-rescan gate.

**Static row labels are pushed from the frontend** (the tray is built before the frontend loads, so it cannot read the i18n dicts) as ONE object with named fields — a positional parameter list silently mislabels the menu on any reorder. The push is gated on the i18n provider being ready, or the tray briefly shows the navigator-guessed locale. **`cli_label` in tray.rs mirrors `CLI_APP_LABEL`** — keep them in sync; the label is "Gemini", not "Gemini CLI". CLI and provider names stay untranslated.

**Open / Exit are plain items**, not predefined ones, so macOS attaches no native icon. The separator before Exit is only added when there is middle content, or an empty middle renders two adjacent rules.

**Icon**: a template image on macOS so the bar themes it light/dark; the COLOURED app icon on Windows/Linux, where a monochrome template reads as a black blob.

**Known gaps (by design for a quick switcher)**: no multi-slot enable-vs-default split, no delete, no edit/test/add-account — all page-only. Failures are logged, not toasted. Provider checkmarks refresh only on Termory-initiated changes, so an external switch can leave them stale until the next action; the recent list does follow the watcher.

## Window / Dock behaviour

**Single instance:** `tauri-plugin-single-instance` is registered FIRST in the plugin chain — a second launch surfaces the running instance's window and exits.

Closing the window does **not** quit: `CloseRequested` is intercepted, the window hides, the Dock icon goes away on macOS, and `prevent_close()` keeps the app in the menu bar. The tray's **Open** restores the Dock icon then shows / unminimizes / focuses.

- The window is declared `visible: false` in tauri.conf.json — that is what stops it flashing before `setup()` runs. A normal launch shows it in `setup()`; a login-item launch (`--autostart`) stays tray-only.
- **The Dock icon is suppressed PRE-RUN** via `set_activation_policy(Accessory)` before `app.run()` — doing it in `setup()` is too late and flashes the icon.
- **`set_dock_visibility` is the purpose-built API** — a raw `set_activation_policy` toggle does NOT reliably restore the Dock icon when re-showing the window.
- The title-bar **text is hidden** (`hiddenTitle: true`) while the traffic lights and draggable bar stay; `title` is still "Termory" so Mission Control identifies it.

## Stats

Every figure reads from ONE shared filtered window.

### Accuracy rules (LOCKED — do not weaken)

All values are **window-accurate**: they reflect what happened in the chosen range, never lifetime totals.

| Metric | Rule |
|---|---|
| Sessions | `started_at ∈ window` — an old session reused today does NOT count |
| Messages / Tokens | summed from the per-day buckets inside the window |
| Active days | window days with any messages or tokens |
| Streaks | consecutive active days; current runs to the window's last day with a one-day grace |
| Peak hour | argmax of the in-window hourly sums; null when there is no hourly data |
| Favorite model | top model by in-window tokens, skipping "Unknown" |

- **No fallback smearing.** A session with lifetime totals but no per-day buckets contributes ZERO — even-distributing them would fabricate per-day numbers indistinguishable from real ones.
- **Filtering uses interval OVERLAP** (`[started_at, updated_at] ∩ window`). Filtering on `updated_at` alone drops sessions whose window contribution predates their last update.
- **The `all` range** starts at the earliest datable activity and degrades to 30d when there is none; `to` is always end-of-today so sessions still being written stay in the filter.
- **Cross-source consistency (test-pinned)**: the stacked model series must sum to the same per-day total as the daily rollup. The "Unknown" bucket rides inside "Others" in the chart precisely so this holds while the legend still hides Unknown.
- **The heatmap follows the source filter but NOT the range** — it always spans full history, like a contribution graph.

**Model attribution is session-level and approximate**: one best-guess model id per session, so a session that switched models mid-run lands entirely under its main one.

**Heatmap intensity (LOCKED)** is a weighted geometric mean of messages and tokens (`m^0.6 · t^0.4`, normalized to the cell set's max), degrading to whichever dimension exists. Messages alone under-light "one big request" days; tokens alone under-light "many short messages".

**Chart colours**: the four token KINDS are nominal, so they get four DISTINCT hues, not an ordinal ramp. Models get **provider-family ramps** — every model of a vendor is a shade of that vendor's hue, so a Claude model never looks "custom" — with the boldest step going to the most-used model. Colour tracks each model's ALL-TIME rank, so a filter change never repaints the survivors. With nine vendor anchors the palette is past the CVD-safe ceiling; distinctness is the hard requirement and brand a lean, and it is legal only because every segment carries a legend label.

**One statistics function per UI module (LOCKED)** in `stats-utils.ts` — KPIs, heatmap, tokens-by-type, tokens-by-model — plus the shared range/filter setup. Each iterates its sessions once. **Rendering and format helpers are NOT statistics functions** and live with the component that uses them. **Model ids render as-is**; there is no name-prettifying helper.

`stats.tokens.*` i18n keys are also used by the Records token tooltip — they are not Stats-private.

## Favorites

### Snapshot rule (LOCKED — do not weaken)

**A Favorite is a SELF-CONTAINED snapshot of the parsed message** — the full `SessionMessage` stored verbatim alongside its source-session metadata. When the source session is later deleted, renamed or re-parsed differently, the Favorite stays readable and renders identically, through the same markdown pipeline as the Records detail.

Do NOT:

- Replace the snapshot with a `(session_id, message_index)` reference resolved at render time. "Open original" uses that tuple as a NAVIGATION HINT only; if the index has drifted, that is accepted.
- Strip it to "just the markdown text" — `role` / `kind` / `timestamp` are read by the role bar and the chip.
- Lazy-fetch session metadata — the title and project are stored at favorite time so the card still shows something when the original is gone.

**Identity key** is `(source, session_id, message_index)`. Index drift across re-parses is the documented trade-off: a content hash cannot replace it — a hash mismatch is not "wasn't favorited", which breaks "click the star to remove" — and all four scanners are append-only.

**Layout parity with Records is deliberate (LOCKED)** — the list column and detail header reuse the same class sets, so the two pages read as one shell. Action icons sit on the meta row, not the title row.

**Scroll-to-message navigation is ONE path** shared by Favorites "Open original", search results, the Cmd-K palette and the in-session find bar: a pending-scroll request carrying `{sessionKey, index, nonce}`, cleared by a stable callback once the virtualizer has scrolled. The nonce is what lets the same target be re-triggered; the stable callback identity is what keeps the effect from oscillating.

## Projects, migration & deletion

### Project model (LOCKED)

**A project is a folder/entity each CLI keeps, enumerated DIRECTLY and independently of records** — a project shows because its entity exists, not because a record groups under it. The scan returns `{ projects, records }` separately.

- Projects come from three sources: real folders (Claude's slug dirs, Gemini's `.project_root`-marked tmp dirs), OpenCode's `project` table rows with no live session, and — for CLIs with **no folder entity** (Codex, OpenCode-with-sessions) — each record's `(source, cwd)`.
- There is **no project `id`** and **no empty-project placeholder records**.
- Consequence: **deleting a record never removes its project.** The project stays until its folder is deleted.

### Memory terminology — do NOT conflate (safety-critical)

- **Auto-memory** lives INSIDE the slug/hash folder (`~/.claude/projects/<slug>/memory/`, Gemini's `tmp/<id>/memory/`), so it goes when the project folder is deleted.
- **The project's own files** (`<cwd>/CLAUDE.md`, `AGENTS.md`, `<cwd>/.claude/…`) live in the USER's working directory, OUTSIDE that folder. **The backend NEVER deletes them** — Termory only ever writes or deletes under `~/.claude/projects` and `~/.gemini/tmp`.

### Delete and migrate

The frontend never sends a raw filesystem path — see the security boundary above.

**Delete-project drops everything shown under that project from the UI** — sessions, auto-memory, and the cwd's own instruction files. The cwd's own files SURVIVE on disk; they simply stop being listed.

Migration is MOVE mode, and the mechanism differs per CLI:

- **Claude** — copy the whole slug subtree (skipping the regenerable index and `.DS_Store`), rewrite only the top-level `cwd` of each JSONL line, then remove the old dir. History content mentioning the old path is left intact on purpose. **Preserve mtime on every migrated file** — `fs::write`/`fs::copy` reset it, and Claude's `/resume` list orders by it.
- **Codex** — no project folder exists; a project is just `threads.cwd`. **Rewrite the rollout `payload.cwd` FIRST** (authoritative, atomic, mtime restored), then `UPDATE threads.cwd` — the table is a backfill cache, so file-first means a crash self-heals to the NEW cwd. Project form includes archived.
- **Gemini** — rewrite the `.project_root` markers only; gemini self-heals its registry from them, so there is **no file move and no registry write** (which also avoids racing its lockfile). Refuse when the destination already owns a Gemini dir. Project-level only.
- **OpenCode** — `UPDATE session SET directory = ?, path = NULL`. **Leave `project_id` intact**: the session stays attached to its project, so it never resurfaces as an empty project at the old cwd, and Termory never hand-writes OpenCode's registry tables.
- **Grok** — files must physically MOVE into the destination's encoded-cwd dir plus a `summary.json` rewrite, because grok resumes per-cwd by scanning that dir. Paths change, so the result carries the old and new dirs for the frontend remap. **The long-path form is REFUSED** rather than reproducing grok's slug+hash fallback.

**The destination is CANONICALIZED (symlinks resolved) for grok, gemini and opencode** — all three derive their stored cwd from the OS `current_dir`, which is already resolved. Gemini is the non-obvious one: its own comparison does not resolve symlinks, but the value it compares against is `process.cwd()`, so a non-canonical marker makes it mint a fresh slug instead of adopting the migrated one.

**MOVE, not copy (LOCKED)**: copying leaves the same session UUID in two slug dirs, the scan dedups by UUID, and the old project vanishes anyway while leaving hidden duplicates.

**Codex/OpenCode remaps must match on `(source, project)`, not on the folder** — their records share one directory, so a folder match hits unrelated sessions.

**The per-ROW local update keys by `sessionKey` (`source:path:id`), never by path** — a DB-backed source shares ONE db-file path across every session, so a path-keyed removal or remap hits the whole list. Applies to delete and migrate alike.

**`claude --resume` needs the project REGISTERED.** The moved files land in the right slug dir, but the command-line flag only lists a project once it appears in `~/.claude.json`, which happens when the user opens Claude in that dir. `/resume` inside a running Claude shows them regardless. **Termory never writes `~/.claude.json`**; it only READS it so the migrate confirm can say so.

**Local list updates happen only on SUCCESS**, with tombstones (record paths and `(source, cwd)`) hiding just-removed entries until a fresh scan confirms them gone.

## UI conventions

### Internationalization

Three bundled locales (en / zh-Hans / zh-Hant) via an in-house type-safe i18n.

- **`en.ts` is the SOURCE dictionary**; its keys form the `MessageKey` union, and the two zh dicts are `Record<MessageKey, string>` — **omitting a key is a compile error**. Add every new key to all three.
- **Pure (non-React) helpers that produce UI copy take a translator param** — they cannot call `useT()`.
- **Locale-aware date and number formatting follows the APP language, not the OS locale.** `setFormatLocale` is called in the provider's RENDER BODY, not an effect, so it is set before children format in the same pass.
- **Brand and product names are NOT translated** — CLI labels, `AI Gateway`, `Base URL`, `AI SDK`, `Tokens`, and literals like `config.json`. `BrandIcon source` must stay a source literal, so translate the display text separately from the icon's prop.

### Never hand-edit `src/components/ui/*` (LOCKED)

Everything in that directory is stock shadcn output — Tooltip, Command, Button and the rest. **Do not hand-edit any of it.** Add new primitives with `npx shadcn@latest add <name>`; put ALL customization at the usage site via `className` / props; take animation keyframes from the already-imported `tw-animate-css` rather than adding `@keyframes` to a component file. `git diff --stat src/components/ui/` should be empty before committing.

**One deliberate committed deviation exists**: `dialog.tsx`'s overlay is a light blur instead of the stock black. A blanket CLI `--overwrite` WILL clobber it — restore that line if an update touches the file.

### Using those components

- **Use the shadcn `Button` for standard action buttons.** A raw `<button>` is correct ONLY for things that are not standard buttons: clickable list rows and cards, the activity rail, segmented toggles and tabs, filter pills, and icons positioned inside an input.
- **Every icon button on the Providers route is `icon-sm` (32px) with a `size-4` glyph — do NOT re-introduce a `size-N` className override (LOCKED).** A className that contradicts the `size` prop is the shape to grep for, since it reads as correct. The only deliberate exceptions are the page header's primary `+` and the account row's selector (a hit area around a circle, a raw `<button>` by convention).
- **Use the shadcn `Tooltip`, never a raw `title=` attribute** — the native one has inconsistent styling, no touch support, fights focus-visible and ignores the project's colour tokens.
- **Icon-only buttons still need an `aria-label`** regardless of any tooltip; screen-reader users do not hover.
- When a button opens a popover anchored on the SAME element, suppress the tooltip while it is open so the two surfaces do not stack.
- **A tooltip that EXPLAINS a disabled state must trigger on a WRAPPER, not the button (LOCKED).** A disabled element dispatches no pointer events, so a tooltip on the button itself is reachable only while the button is ENABLED — exactly when it has nothing to explain. **This is not a blanket rule**: where the tooltip only repeats the button's own name, losing it while disabled costs nothing.
- The freshness footer shows a tooltip on FAILURE only. Every success state is a bare label on purpose — it already answers the question.

### Search and find

- **`⌘K` is the palette's ONLY trigger; `⌘F` is in-session find and does nothing elsewhere.** The two keys must never both open the palette.
- **The palette's `open` is CONTROLLED by `App`**, so App's ⌘F handler can no-op while it is open — a window keydown listener still fires underneath a Radix dialog.
- The palette shows BACKEND results only — no instant metadata fallback rows, which flash title-only rows that then swap.
- **The palette and the Search page must keep sharing ONE result-row component**, or the two lists drift.
- The Search-page seed is CONSUME-ONCE, or a later normal visit resurrects a stale query.
- **Highlighting is one component** used by both the snippet lines and the message body. The rehype pass emits BARE `<mark>` nodes with no styling in the tree.
- Highlight colour is the YELLOW convention with SOLID colours so it reads the same over cards, code and selected rows. Primary-tinted highlights collide with the selection colour and go invisible. The text utilities need `!` to beat the selected row's equal-specificity cascade. **The highlight must not change layout** — no margins or padding on the wrapper.
- Sessions navigate per-MESSAGE because the list is virtualized and rows unmount; docs navigate per-OCCURRENCE because the whole doc is in the DOM.
- **ONE effect owns the find bar's reaction to a selection change** — a two-effect split relies silently on declaration order.

### Right-click menus

Rows carry `select-none` so a right-click does not text-select the row. The Favorites menu drops session-management actions (they act on the SOURCE session, and delete would remove it) and, when the source session is gone, also drops reveal and resume.

**No manual "re-scan" entries.** The watcher covers everything Termory surfaces: the static CLI data dirs plus a recursive watch on each project cwd, debounced into one re-scan. **The sources-changed emit is gated on window visibility** — while hidden the re-scan still runs for the tray, but the full result is not serialized to a frontend nobody is looking at. The frontend re-scans on window focus, SILENTLY: focus fires on every activation, so toggling the global loading flag would flash the refresh indicator.

## Decided against — do not propose these again

- **Arrow-key navigation inside the Records/Favorites lists and the sidebar** — a two-step highlight is unintuitive. `⌘1`–`⌘6`, `⌘K`, `⌘F` and Esc are the shortcuts that exist.
- **Scan-path overrides in Settings** — users with non-default CLI locations rely on each tool's own env var.
- **A "Clear filters" action on the empty states** — the only filters are the sidebar's source and project rows, which are always visible and directly clickable.
- **A watcher toggle** — the watcher runs unconditionally.
- **Live work status for Codex sessions** — the rollout tail says only whether a thread ENDS mid-turn, never whether a process is still alive to be working on it; see the tray's live-status rules for what was checked and why the one liveness link on disk is not usable.
