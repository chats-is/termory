#!/usr/bin/env node
// Termory driver — launch and drive the macOS Tauri app programmatically.
//
// Termory is a NATIVE Tauri v2 app (WKWebView), not Electron: there is no
// remote-debugging port, no Playwright `_electron`, and no CDP. The only
// handle macOS gives an outside process is the accessibility API, so this
// driver talks to the app through `osascript` (System Events) and captures
// it with `screencapture`.
//
// Usage:
//   node .claude/skills/run-termory/driver.mjs <command> [args]
//
//   launch            start `npm run tauri:dev`, wait for the window + first scan
//   settle [ms]       extra wait for the session scan (default 8000)
//   bounds            print the window's {x,y,w,h} in screen points
//   shot <file>       raise the app, screenshot its window to <file>
//   paste <text>      IME-safe text entry (clipboard + Cmd-V)
//   key <spec>...     key presses, e.g. `key cmd+k down down return`
//   search [--wait ms] <query>
//                     Cmd-K palette, type <query>, wait for results.
//                     First search after a launch waits 25s (see below).
//   route <1-6>       Cmd-1..6 — providers/records/favorites/search/stats/settings
//   quit              quit the app (leaves `npm run tauri:dev` to exit)
//
// Every command raises the app first. That is not politeness: screencapture
// grabs a SCREEN REGION, so any window sitting on top of Termory ends up in
// the screenshot (see Gotchas in SKILL.md).

import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";

const APP = "Termory";
const REPO = resolve(import.meta.dirname, "../../..");

const KEY_CODES = {
  return: 36, enter: 36, esc: 53, escape: 53, tab: 48, space: 49,
  delete: 51, down: 125, up: 126, left: 123, right: 124,
  f: 3, k: 40, // only the letters this driver needs by code
};

function osa(script) {
  return execFileSync("osascript", ["-e", script], { encoding: "utf8" }).trim();
}

function sh(cmd, args, opts = {}) {
  return execFileSync(cmd, args, { encoding: "utf8", ...opts });
}

const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

function isRunning() {
  try {
    sh("pgrep", ["-x", APP]);
    return true;
  } catch {
    return false;
  }
}

function raise() {
  if (!isRunning()) throw new Error(`${APP} is not running — run \`launch\` first.`);
  osa(`tell application "System Events" to tell process "${APP}" to set frontmost to true`);
  sleep(400);
}

// Window geometry is QUERIED, never hardcoded: the user can move or resize
// the window between runs, and a stale rectangle silently screenshots the
// desktop instead of the app.
function bounds() {
  const raw = osa(
    `tell application "System Events" to tell process "${APP}" ` +
      `to get {position, size} of window 1`,
  );
  const [x, y, w, h] = raw.split(",").map((n) => parseInt(n.trim(), 10));
  if ([x, y, w, h].some(Number.isNaN)) throw new Error(`could not parse window bounds: ${raw}`);
  return { x, y, w, h };
}

function launch() {
  if (isRunning()) {
    console.log("already running");
    return;
  }
  const log = "/tmp/termory-dev.log";
  const out = spawn("sh", ["-c", `npm run tauri:dev > ${log} 2>&1`], {
    cwd: REPO,
    detached: true,
    stdio: "ignore",
  });
  out.unref();
  try { sh("rm", ["-f", WARM_MARKER]); } catch { /* nothing to clear */ }
  console.log(`building… (first run compiles Rust; log: ${log})`);
  // Cargo builds the binary before the window exists. Poll for the process,
  // and bail out on a compile error rather than sitting out the timeout.
  const deadline = Date.now() + 15 * 60_000;
  while (Date.now() < deadline) {
    if (isRunning()) {
      sleep(2500); // window + webview need a moment after the process appears
      const b = bounds();
      // THEN wait out the first session scan. The window is interactive
      // before the scan finishes, and a `search` fired into that gap finds
      // NOTHING — the palette shows no rows, so Enter lands on its
      // "view all results" footer button and silently navigates to the
      // Search page instead of opening a session. Observed, not theorised.
      // The scan is not observable from outside (see `settle`), so this is
      // a timer.
      settle();
      console.log(`up: ${JSON.stringify(b)}`);
      return;
    }
    try {
      const tail = sh("grep", ["-cE", "^error(\\[E[0-9]+\\])?:|could not compile", log]).trim();
      if (tail !== "0") {
        console.error(sh("grep", ["-E", "^error", log]));
        throw new Error("build failed — see the log above");
      }
    } catch (e) {
      if (e.message?.startsWith("build failed")) throw e; // grep exits 1 on no match
    }
    sleep(2000);
  }
  throw new Error("timed out waiting for the window");
}

// There is NO observable readiness signal for the first session scan, so this
// is a plain timer and is deliberately generous. Things that were tried and do
// not work: the accessibility tree (Tauri's WKWebView exposes only an opaque
// `AXGroup` — `get value of every static text of window 1` returns nothing, so
// the footer's synced/syncing state is unreadable), and process CPU (macOS
// `ps -o %cpu` is a decaying average, and the dev build spawns short-lived
// helpers whose pids `pgrep` also matches, so it reads as busy long after the
// app is idle). Raise it for a machine with a very large history.
function settle(ms = 8000) {
  sleep(Number(ms) || 8000);
}

function shot(file) {
  if (!file) throw new Error("usage: shot <file>");
  raise();
  sleep(600); // let the raise animation settle before capturing
  const { x, y, w, h } = bounds();
  mkdirSync(dirname(resolve(file)), { recursive: true });
  sh("screencapture", ["-x", `-R${x},${y},${w},${h}`, resolve(file)]);
  if (!existsSync(resolve(file))) throw new Error("screencapture produced no file");
  console.log(`${file} (${w}x${h} pt, 2x on retina)`);
}

// TEXT ENTRY GOES THROUGH THE CLIPBOARD, ALWAYS.
// `keystroke "Wait agent"` is routed through the ACTIVE INPUT METHOD. On a
// machine with a Chinese IME selected this arrives as pinyin composition —
// the literal observed failure was "Wait agent" turning into a candidate
// list reading `爱他a W gen t`. Cmd-V bypasses the IME entirely.
function paste(text) {
  raise();
  sh("pbcopy", [], { input: text });
  osa(`tell application "System Events" to keystroke "v" using command down`);
  sleep(300);
}

function key(specs) {
  raise();
  for (const spec of specs) {
    const parts = spec.toLowerCase().split("+");
    const base = parts.pop();
    const mods = parts.map((m) => ({ cmd: "command", shift: "shift", opt: "option", ctrl: "control" })[m]);
    if (mods.some((m) => !m)) throw new Error(`unknown modifier in ${spec}`);
    const using = mods.length ? ` using {${mods.map((m) => `${m} down`).join(", ")}}` : "";
    if (KEY_CODES[base] !== undefined && !mods.length) {
      osa(`tell application "System Events" to key code ${KEY_CODES[base]}`);
    } else if (base.length === 1) {
      osa(`tell application "System Events" to keystroke "${base}"${using}`);
    } else if (KEY_CODES[base] !== undefined) {
      osa(`tell application "System Events" to key code ${KEY_CODES[base]}${using}`);
    } else {
      throw new Error(`unknown key: ${base}`);
    }
    sleep(400);
  }
}

// THE FIRST SEARCH AFTER A COLD LAUNCH IS ~4x SLOWER THAN THE REST.
// `search_all_sessions` parses every session on a cache miss, so the first
// query walks the entire history (measured here: results appeared between 4s
// and 16s; every later query landed within ~3s). Waiting the short time on a
// cold app is the failure this driver was written around: the palette is still
// empty, so a following `key … return` hits its "view all results" FOOTER
// BUTTON and silently navigates to the Search page instead of opening a
// session — it looks like the keys went to the wrong place, but the keys were
// fine and the list was empty.
//
// The warm/cold state lives in a marker file because each driver invocation is
// a fresh node process and cannot otherwise remember. `launch` clears it.
const WARM_MARKER = "/tmp/termory-search-warm";

function search(argv) {
  const args = [...argv];
  let wait = null;
  const i = args.indexOf("--wait");
  if (i !== -1) {
    wait = Number(args[i + 1]);
    args.splice(i, 2);
  }
  const query = args.join(" ");
  if (!query) throw new Error("usage: search [--wait <ms>] <query>");
  if (wait == null) wait = existsSync(WARM_MARKER) ? 4000 : 25000;
  key(["esc"]);
  key(["cmd+k"]);
  sleep(600);
  paste(query);
  sleep(wait);
  try {
    sh("touch", [WARM_MARKER]);
  } catch {
    /* marker is an optimisation; a failure here only costs time */
  }
}

function route(n) {
  const i = Number(n);
  if (!(i >= 1 && i <= 6)) throw new Error("usage: route <1-6>");
  key([`cmd+${i}`]);
}

const [cmd, ...rest] = process.argv.slice(2);
try {
  switch (cmd) {
    case "launch": launch(); break;
    case "settle": settle(rest[0]); break;
    case "bounds": console.log(JSON.stringify(bounds())); break;
    case "shot": shot(rest[0]); break;
    case "paste": paste(rest.join(" ")); break;
    case "key": key(rest); break;
    case "search": search(rest); break;
    case "route": route(rest[0]); break;
    case "quit":
      if (isRunning()) osa(`tell application "System Events" to tell process "${APP}" to click menu item "Quit Termory" of menu 1 of menu bar item "Termory" of menu bar 1`);
      break;
    default:
      console.error("commands: launch | settle [ms] | bounds | shot <file> | paste <text> | key <spec>… | search <q> | route <1-6> | quit");
      process.exit(2);
  }
} catch (e) {
  console.error(String(e.message ?? e));
  process.exit(1);
}
