---
name: run-termory
description: Build, run, and drive the Termory desktop app on macOS. Use when asked to start Termory, launch the dev app, take a screenshot of its UI, click through or interact with the running app, verify a rendering change in the real app, or run its Rust/frontend tests.
---

Termory is a **native Tauri v2 app (WKWebView) on macOS** — not Electron. Drive it
with `.claude/skills/run-termory/driver.mjs`, which talks to it through macOS
accessibility (`osascript`) and captures it with `screencapture`. Paths below are
relative to the repo root.

**There is no CDP/DevTools handle.** Tauri v2 on macOS exposes no remote-debugging
port, so Playwright's `_electron`, `chromium-cli`, and every CDP-based recipe are
dead ends here. Accessibility + screenshots is the whole surface. (`xvfb` is
likewise irrelevant — this is a real macOS window server, not headless Linux.)

## Prerequisites

Already satisfied on this machine; no `apt-get`/`brew` step was needed. What must
be true:

- Node (v22.21.1 here) and a Rust toolchain — `npm run tauri:dev` builds both.
- **Accessibility permission for the terminal app running the driver**
  (System Settings → Privacy & Security → Accessibility). Without it every
  `osascript` System Events call fails. This was already granted here, so the
  first-grant prompt is unverified.
- Screen Recording permission for `screencapture`.

## Run (agent path)

```bash
node .claude/skills/run-termory/driver.mjs launch          # idempotent; prints window bounds
node .claude/skills/run-termory/driver.mjs route 2         # Cmd-1..6 rail: providers/records/favorites/search/stats/settings
node .claude/skills/run-termory/driver.mjs search "Wait agent"
node .claude/skills/run-termory/driver.mjs shot /tmp/x.png     # look before selecting
node .claude/skills/run-termory/driver.mjs key down down return
node .claude/skills/run-termory/driver.mjs quit
```

`launch` spawns `npm run tauri:dev` detached, logs to `/tmp/termory-dev.log`, polls
for the process, and fails fast on a compile error instead of sitting out its
15-minute timeout. It then waits out the first session scan (`settle`, 8s), so
with Rust already built it returns in ~15s (measured); a cold Rust build is
minutes.

Other commands: `bounds` (window rect as JSON), `paste <text>` (IME-safe text
entry — see Gotchas), `key <spec>…` (`cmd+k`, `down`, `return`, `esc`, …).

**Then look at the screenshot.** A frame showing another app's window means the
raise didn't land, not that the app is broken.

A verified end-to-end flow — find a rendered tool card and land on it.
**Screenshot the results before selecting one**; see the ordering gotcha below.

```bash
D=.claude/skills/run-termory/driver.mjs
node $D launch
node $D route 2
node $D search "Wait agent"     # Cmd-K palette; first search is slow, see below
node $D shot /tmp/hits.png      # LOOK: which row is the one you want?
node $D key down down return    # move N rows, open it
sleep 3
node $D key return              # in-session find jumps to the match
node $D shot /tmp/card.png
```

Opening a search hit also seeds the in-session find bar with the query, so the
second `return` jumps to the matching message — that is the app's
search→find linkage, not a driver trick.

## Run (human path)

`npm run tauri:dev` — blocks the terminal, opens the window, Ctrl-C to stop. Fine
for a human; useless for an agent, which is why `launch` backgrounds it.

## Test

```bash
cd src-tauri && cargo fmt && cargo test --lib   # 552 tests
npx vitest run                                  # 492 tests, 33 files — REPO ROOT ONLY
npm run build                                   # also the type-check
```

## Verifying a rendering change

The app renders backend-produced markdown, so a rendering bug can live in either
layer. Three probes, all used successfully:

**Backend → real data.** Add a `#[ignore]` test in `sessions.rs` that walks real
history and prints what the parser emits, run it, then delete it:

```bash
cargo test --lib probe_real_attachment_render -- --ignored --nocapture
```

**Frontend → real markdown.** A scratch vitest file that drives the real
`MessageBody` and dumps `container.textContent`, compared against the source
string. Anything unequal is a markdown-escaping bug.

```bash
npx vitest run src/__render_probe.test.tsx     # from REPO ROOT
```

**On screen → a UI you cannot otherwise reach.** Some surfaces only appear under
a condition you cannot manufacture: the update dialog needs a genuinely newer
release, and on a machine already running the latest there is no way to see it.
Temporarily mount the component with a scripted fake, screenshot it, then
`git checkout` the file. For the update dialog that meant a fake `Update` whose
`downloadAndInstall` emits `Started` + `Progress` and **parks** — never
`Finished`, never resolving, because resolving makes the real dialog call
`relaunch()`:

```tsx
// TEMPORARY — revert. Mounted in place of the real `pendingUpdate`.
const __TEMP_FAKE_UPDATE__ = {
  version: "1.4.3",
  body: "### 🐛 Bug Fixes\n- something",
  async downloadAndInstall(cb?: (e: unknown) => void) {
    const total = 48_500_000;
    cb?.({ event: "Started", data: { contentLength: total } });
    for (let i = 0; i < 42; i++) {              // park at a FIXED percentage:
      await new Promise((r) => setTimeout(r, 120));
      cb?.({ event: "Progress", data: { chunkLength: total / 100 } });
    }
    await new Promise(() => {});                // …so the screenshot is deterministic
  }
} as never;
```

Park at a fixed value rather than letting it run: a fake that completes leaves
the dialog in its terminal state, and a screenshot timed against a moving number
races the shutter. Two traps around this: a parked promise means the component
is now stuck in that state, so editing the fake and relying on HMR shows you
nothing (see the HMR gotcha below — restart instead); and `git checkout` the
scaffolding the moment you have the pixels, before it rides along in a commit.

Then confirm in the app with the driver. All three layers disagreeing is possible:
a string can be correct in Rust, correct through react-markdown, and still land on
a screen the user never navigates to.

## Gotchas

- **The first search after a launch takes ~4x longer than the rest.**
  `search_all_sessions` parses every session on a cache miss, so the first query
  walks the whole history — measured here between 4s and 16s, with every later
  query landing in ~3s. `driver.mjs search` handles this with a `/tmp/termory-
  search-warm` marker (25s cold, 4s warm; `launch` clears it, `--wait <ms>`
  overrides). **The failure it prevents is silent and misleading**: with the
  palette still empty, a following `key … return` hits its "view all results"
  FOOTER button and navigates to the Search page instead of opening a session.
  It reads as "my keystrokes went somewhere wrong" — they didn't; the list was
  empty.
- **Search-result POSITION is not stable, so never hardcode "the 3rd result".**
  Results are recency-ordered across sessions, memories AND skills, so
  `CLAUDE.md`, your own notes, and the transcript of the session you are
  working in all rank alongside real hits — and every message you write changes
  the ordering. The same `search` + `key down down return` opened a Codex
  session on one run and a `CLAUDE.md` memory on the next. `shot` the palette
  and count rows before selecting.
- **Never `keystroke "some text"` — always the clipboard.** `keystroke` is routed
  through the **active input method**. With a Chinese IME selected, `keystroke
  "Wait agent"` arrived as a pinyin candidate list reading `爱他a W gen t` and the
  search returned nothing. `driver.mjs paste` does `pbcopy` + Cmd-V, which bypasses
  the IME. This is the single highest-value line in this skill.
- **Never `click at {x, y}`.** System Events clicks **absolute screen
  coordinates**, so whatever window happens to be topmost there receives the click
  — observed live: a click meant for the app's find bar activated the terminal
  behind it. Navigate by keyboard (`cmd+k`, arrows, `return`, `esc`). The driver
  deliberately exposes no click command.
- **`screencapture -R` grabs a screen REGION, not a window.** Any window on top of
  Termory lands in the PNG. Every driver command raises the app first; if you call
  `screencapture` yourself, raise first or you will screenshot your own terminal
  (this happened twice before the driver existed).
- **Query the window rect, never hardcode it.** `bounds` reads it from System
  Events each time. A hardcoded rectangle silently captures the desktop after the
  user moves or resizes the window.
- **Retina:** `-R` takes points; the PNG is 2x those numbers (1080x720 pt →
  2160x1440 px). Map image coords back with `screen = origin + image/2`.
- **HMR swaps the module but does NOT reset React state — restart the app to
  verify anything stateful.** Vite hot-reloads the webview in place, so a
  component keeps the state it was already in. Cost an entire round here: the
  update dialog was parked in its `installing` phase, the source was edited to
  park at 42% of *downloading* instead, HMR applied the edit — and the screen
  did not change, because `phase` had survived. It reads as "my edit didn't
  take" and sends you back to re-check code that was already correct. The tell
  is an edit that is visibly live in one respect (an icon or class changed)
  while the state-dependent part is stale. `driver.mjs quit` then `launch`.
- **`MessageBody`'s prop is `text`, not `content`.** Passing `content` renders an
  empty container and a probe silently reports nothing rendered.
- **vitest only runs from the repo root.** From `src-tauri/` it resolves the config
  relative to the cwd and dies on `Cannot find module '.../src/test/setup.ts'`.
- **vitest swallows `console.log` here.** Have probe tests write to a file
  (`fs.writeFileSync('/tmp/out.txt', …)`) and `cat` it.
- **The process is `Termory`, capital T** (`[[bin]] name = "Termory"`), so
  `pgrep -x Termory` is the readiness signal — `pgrep termory` finds nothing.
- **Closing the window does not quit** — Termory is a menu-bar app and hides to the
  tray. `driver.mjs quit` clicks the real Quit menu item; `pkill` also works.
- The window opens at 1080x720 and the app reads the developer's REAL `~/.claude`,
  `~/.codex`, … history. Expect the live machine's sessions, not fixtures.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Termory is not running — run \`launch\` first.` | The app quit or never started; check `/tmp/termory-dev.log`. |
| Search box fills with Chinese characters | The IME ate `keystroke`. Use `paste`. |
| Screenshot shows the terminal, not the app | Something raised itself after the driver's raise. Re-run `shot`. |
| A keyboard command does nothing | Accessibility permission missing for the terminal app, or focus went elsewhere — `shot` first to see actual state. |
| `Cannot find module '.../src-tauri/src/test/setup.ts'` | You ran `npx vitest` from `src-tauri/`. Run it from the repo root. |
| `launch` returns "build failed" | Read `/tmp/termory-dev.log`; it greps for `error:` / `could not compile`. |
| Footer shows 同步失败 / "Sync failed" | Seen once after rapid quit/relaunch cycles, with nothing in the dev log and one healthy process. It cleared itself on the next scan — switch routes (`route 1; route 2`) to re-trigger one. Only chase it if it repeats. |
| Palette empty / Enter lands on the Search page | The first search hadn't finished. See the cold-search gotcha; `search --wait 30000`. |
