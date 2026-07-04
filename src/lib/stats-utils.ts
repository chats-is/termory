// Pure aggregation helpers used by the Stats page. Everything in here
// is side-effect free so it can be unit-tested without a DOM, and the
// Stats page renders the results via `useMemo`.
//
// The input shape is always `AppSession[]` — the same data
// `scan_all_sessions` returns. No Rust IPC changes; Stats is purely a
// view over already-collected metadata.

import type { AppSession, CliApp, TokenStats } from "../types";
import { isSessionItem } from "./session-utils";

export type DateRangePreset = "7d" | "30d" | "all";

export type DateRange = { preset: DateRangePreset };

export type SourceFilter = "All" | CliApp;

/**
 * Parse `updated_at` (preferred) or `started_at`. Sessions where
 * neither field is a valid ISO timestamp get dropped — they can't be
 * placed on any time chart.
 */
export function sessionTimestamp(session: AppSession): Date | null {
  const raw = session.updated_at ?? session.started_at;
  if (!raw) return null;
  const d = new Date(raw);
  if (Number.isNaN(d.getTime())) return null;
  return d;
}

/**
 * Resolve a date range preset to a concrete `{from, to}` window.
 *
 * `from` is the start of the day N days before now in the user's
 * local time. `to` is the END of today (23:59:59.999 local). This
 * matters because sessions being actively written (e.g. the Claude
 * session driving the current conversation) keep advancing their
 * `updated_at` past "now-at-page-load"; using end-of-today as the
 * upper bound keeps them in the filter regardless of when the user
 * opened the page.
 */
export function resolveRange(
  range: DateRange,
  sessions: AppSession[],
  now: Date = new Date()
): { from: Date; to: Date } {
  const to = new Date(now);
  to.setHours(23, 59, 59, 999);
  if (range.preset === "all") {
    // Earliest activity day: min daily_tokens date, falling back to
    // started_at. Date keys are YYYY-MM-DD, so lexical compare works.
    let earliest: string | null = null;
    for (const s of sessions) {
      if (!isSessionItem(s)) continue;
      if (s.daily_tokens) {
        for (const e of s.daily_tokens) {
          if (!earliest || e.date < earliest) earliest = e.date;
        }
      }
      if (s.started_at) {
        const d = new Date(s.started_at);
        if (!Number.isNaN(d.getTime())) {
          const k = localDateKey(d);
          if (!earliest || k < earliest) earliest = k;
        }
      }
    }
    if (earliest) {
      const [y, m, d] = earliest.split("-").map(Number);
      const from = new Date(y, m - 1, d);
      from.setHours(0, 0, 0, 0);
      return { from, to };
    }
    // No datable activity → fall through to the 30d window.
  }
  const days = range.preset === "7d" ? 7 : 30;
  const from = new Date(now);
  from.setDate(from.getDate() - days + 1);
  from.setHours(0, 0, 0, 0);
  return { from, to };
}

/**
 * Filter sessions whose lifetime interval [started_at, updated_at]
 * OVERLAPS the given window, AND match the source filter.
 *
 * Why interval overlap instead of a single timestamp:
 *   The stats rule is "Sessions = started_at ∈ window; Messages /
 *   Tokens = daily_tokens.date ∈ window". A session created BEFORE
 *   window with messages IN window must still be considered (for
 *   Messages / Tokens). Filtering on `updated_at ∈ window` would
 *   silently drop it. Interval overlap catches every session with
 *   any possible in-window contribution; aggregators then apply the
 *   per-entry date check.
 *
 * Sessions without any usable timestamp are dropped — they can't be
 * placed on any chart.
 */
export function filterSessions(
  sessions: AppSession[],
  range: { from: Date; to: Date },
  source: SourceFilter
): AppSession[] {
  const fromMs = range.from.getTime();
  const toMs = range.to.getTime();
  return sessions.filter((s) => {
    if (source !== "All") {
      // CliApp values are lowercase; AppSession.source uses capitalized
      // values ("Claude", "Codex", "Gemini", "OpenCode"). Map both
      // sides to a canonical lowercase for the comparison.
      const canonical = s.source.toLowerCase();
      const want = source.toLowerCase();
      if (canonical !== want) return false;
    }
    const startStr = s.started_at ?? s.updated_at;
    const endStr = s.updated_at ?? s.started_at;
    if (!startStr || !endStr) return false;
    const start = new Date(startStr).getTime();
    const end = new Date(endStr).getTime();
    if (Number.isNaN(start) || Number.isNaN(end)) return false;
    // Guard against rare "started_at > updated_at" data with min/max.
    const lo = Math.min(start, end);
    const hi = Math.max(start, end);
    return hi >= fromMs && lo <= toMs;
  });
}

/** YYYY-MM-DD key in the local timezone. */
export function localDateKey(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

/**
 * Headline KPI numbers for the window. EVERY field is window-accurate:
 *  - sessions: count where `started_at` falls in window
 *  - messages: sum of `daily_tokens[date in window].messages`
 *  - tokens:   sum of `daily_tokens[date in window].tokens`
 *  - projects: unique projects of sessions that contributed any of
 *              the above
 *
 * No lifetime-of-touched-session totals (which would over-report) and
 * no even-distribution estimates (which would fabricate per-day
 * numbers). Sessions without recoverable `daily_tokens` contribute
 * zero to messages / tokens — Termory shows the real activity in the
 * window, nothing else.
 */
export type WindowTotals = {
  sessions: number;
  messages: number;
  tokens: TokenStats;
  projects: number;
};

export function windowTotals(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): WindowTotals {
  const fromKey = localDateKey(range.from);
  const toKey = localDateKey(range.to);
  const inRange = (date: string) => date >= fromKey && date <= toKey;

  let sessionCount = 0;
  let messageCount = 0;
  const tokens: TokenStats = {
    input: 0,
    output: 0,
    cached: 0,
    reasoning: 0,
    total: 0
  };
  const projects = new Set<string>();
  for (const s of sessions) {
    if (!isSessionItem(s)) continue;

    let contributed = false;

    // Sessions: started_at in window.
    if (s.started_at) {
      const startTs = new Date(s.started_at);
      if (!Number.isNaN(startTs.getTime()) && inRange(localDateKey(startTs))) {
        sessionCount += 1;
        contributed = true;
      }
    }

    // Messages + tokens: only count in-range daily_tokens entries.
    if (s.daily_tokens) {
      for (const entry of s.daily_tokens) {
        if (!inRange(entry.date)) continue;
        tokens.input += entry.tokens.input;
        tokens.output += entry.tokens.output;
        tokens.cached += entry.tokens.cached;
        tokens.reasoning += entry.tokens.reasoning;
        tokens.total += entry.tokens.total;
        messageCount += entry.messages ?? 0;
        contributed = true;
      }
    }

    if (contributed && s.project && s.project.trim()) {
      projects.add(s.project);
    }
  }
  return {
    sessions: sessionCount,
    messages: messageCount,
    tokens,
    projects: projects.size
  };
}

/** Per-model rollup row. `model` is `"Unknown"` for sessions whose
 * source didn't record one. */
export type ModelUsage = {
  model: string;
  sessions: number;
  tokens: number;
  /** In-window input / output token sums (same per-entry date check as
   * `tokens`) — the Models-tab legend renders "{in} in · {out} out". */
  input: number;
  output: number;
};

/**
 * Group the in-window usage by `session.model`, window-accurate with
 * the SAME accounting as `windowTotals`:
 *   - `sessions` counts sessions whose `started_at` falls in the window
 *   - `tokens`   sums only in-window `daily_tokens` totals
 *
 * `model` is session-level (one best-guess id per session), so a
 * session that switched models mid-run lands entirely under its single
 * recorded model — this is an approximation, not a per-message split.
 * Sessions with no recorded model bucket under `"Unknown"`. Rows are
 * sorted by tokens desc, then sessions desc, then name.
 */
export function modelBreakdown(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): ModelUsage[] {
  const fromKey = localDateKey(range.from);
  const toKey = localDateKey(range.to);
  const inRange = (date: string) => date >= fromKey && date <= toKey;

  const map = new Map<
    string,
    { sessions: number; tokens: number; input: number; output: number }
  >();
  const bucket = (model: string) => {
    let b = map.get(model);
    if (!b) {
      b = { sessions: 0, tokens: 0, input: 0, output: 0 };
      map.set(model, b);
    }
    return b;
  };

  for (const s of sessions) {
    if (!isSessionItem(s)) continue;
    const model = s.model && s.model.trim() ? s.model : "Unknown";

    if (s.started_at) {
      const startTs = new Date(s.started_at);
      if (!Number.isNaN(startTs.getTime()) && inRange(localDateKey(startTs))) {
        bucket(model).sessions += 1;
      }
    }

    if (s.daily_tokens) {
      for (const entry of s.daily_tokens) {
        if (!inRange(entry.date)) continue;
        const b = bucket(model);
        b.tokens += entry.tokens.total;
        b.input += entry.tokens.input;
        b.output += entry.tokens.output;
      }
    }
  }

  return Array.from(map, ([model, v]) => ({ model, ...v })).sort(
    (a, b) =>
      b.tokens - a.tokens ||
      b.sessions - a.sessions ||
      a.model.localeCompare(b.model)
  );
}

export type DailyTokens = {
  date: string; // YYYY-MM-DD (local)
  /** Number of AI interactions on this date (sum of
   * `daily_tokens[date].messages`). Drives the in-range messages KPI
   * via `windowTotals`. */
  messages: number;
  /** Total tokens that day. Matches the "Total" row in the
   * DailyTokensChart tooltip. Named `total` (not `tokens`) so it
   * aligns with `TokenStats.total` and avoids collision with the
   * TokenStats *object* called `tokens` on AppSession. */
  total: number;
  input: number;
  output: number;
  cached: number;
  reasoning: number;
};

/**
 * Round a value up to a clean axis bound (1 / 2 / 2.5 / 5 × 10ⁿ). Used by the
 * DailyTokensChart to pin its sticky Y-axis chart and its scrollable plot to
 * the SAME domain — the axis-only chart has no data series, so it can't derive
 * `"auto"` and needs an explicit shared max.
 */
export function niceMax(v: number): number {
  if (v <= 0) return 1;
  const base = 10 ** Math.floor(Math.log10(v));
  const f = v / base;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * base;
}

/**
 * Per-day token rollups for the daily-usage chart.
 *
 * Source: each session's `daily_tokens` array (produced by the four
 * scanners when underlying records carry timestamps). Sessions
 * without `daily_tokens` contribute zero — Termory does NOT smear
 * lifetime totals across the date range, because that would
 * fabricate per-day numbers that look identical to real data.
 *
 * Days outside the chart's range are silently dropped. Session counts
 * live on `DailyActivities` (heatmap matrix), not here.
 */
export function dailyTokens(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): DailyTokens[] {
  const buckets = new Map<string, DailyTokens>();
  const cursor = new Date(range.from);
  cursor.setHours(0, 0, 0, 0);
  const end = new Date(range.to);
  end.setHours(0, 0, 0, 0);
  while (cursor.getTime() <= end.getTime()) {
    const key = localDateKey(cursor);
    buckets.set(key, {
      date: key,
      messages: 0,
      total: 0,
      input: 0,
      output: 0,
      cached: 0,
      reasoning: 0
    });
    cursor.setDate(cursor.getDate() + 1);
  }
  for (const s of sessions) {
    if (!isSessionItem(s)) continue;
    if (!s.daily_tokens || s.daily_tokens.length === 0) continue;
    for (const entry of s.daily_tokens) {
      const bucket = buckets.get(entry.date);
      if (!bucket) continue;
      bucket.messages += entry.messages ?? 0;
      bucket.total += entry.tokens.total;
      bucket.input += entry.tokens.input;
      bucket.output += entry.tokens.output;
      bucket.cached += entry.tokens.cached;
      bucket.reasoning += entry.tokens.reasoning;
    }
  }
  return Array.from(buckets.values());
}

// ─── Overview KPIs ────────────────────────────────────────────────────────────

export type OverviewKpis = {
  /** Days in the window with any activity (messages or tokens). */
  activeDays: number;
  /** Consecutive active days ending at the window\'s last day — with a
   * one-day grace: a not-yet-active today counts back from yesterday. */
  currentStreak: number;
  /** Longest run of consecutive active days in the window. */
  longestStreak: number;
  /** Local hour (0-23) with the most messages; null with no activity. */
  peakHour: number | null;
};

/** Streak / activity KPIs — window-accurate with the same accounting
 * as `windowTotals` (per-entry date checks, no smearing). */
export function overviewKpis(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): OverviewKpis {
  const daily = dailyTokens(sessions, range);
  const active = daily.map((d) => d.messages > 0 || d.total > 0);

  let activeDays = 0;
  let longestStreak = 0;
  let run = 0;
  for (const a of active) {
    if (a) {
      activeDays += 1;
      run += 1;
      if (run > longestStreak) longestStreak = run;
    } else {
      run = 0;
    }
  }

  let currentStreak = 0;
  let i = active.length - 1;
  if (i >= 0 && !active[i]) i -= 1; // today not active yet → from yesterday
  for (; i >= 0 && active[i]; i -= 1) currentStreak += 1;

  // Per-hour message totals across the window (same in-range per-entry
  // check the other aggregators use).
  const fromKey = localDateKey(range.from);
  const toKey = localDateKey(range.to);
  const hourMessages = new Array<number>(24).fill(0);
  for (const s of sessions) {
    if (!isSessionItem(s) || !s.daily_tokens) continue;
    for (const entry of s.daily_tokens) {
      if (entry.date < fromKey || entry.date > toKey) continue;
      if (!entry.hours || entry.hours.length !== 24) continue;
      for (let h = 0; h < 24; h++) hourMessages[h] += entry.hours[h];
    }
  }
  let peakHour: number | null = null;
  let best = 0;
  for (let h = 0; h < 24; h++) {
    if (hourMessages[h] > best) {
      best = hourMessages[h];
      peakHour = h;
    }
  }

  return { activeDays, currentStreak, longestStreak, peakHour };
}

// ─── Per-day per-model tokens (Models tab) ────────────────────────────────────

export type DailyModelTokens = {
  /** Every date in the window (same axis as `dailyTokens`). */
  dates: string[];
  /** Models sorted by window token total desc (then name). */
  models: string[];
  /** Per-model daily totals, aligned with `dates`. */
  series: Record<string, number[]>;
};

/**
 * Stacked-bar data: per-day token totals split by model. Attribution
 * is SESSION-level (one best-guess model per session — the documented
 * approximation shared with `modelBreakdown`), so summing every
 * model\'s series for a date equals that date\'s `dailyTokens[].total`.
 */
export function dailyModelTokens(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): DailyModelTokens {
  const dates: string[] = [];
  const cursor = new Date(range.from);
  cursor.setHours(0, 0, 0, 0);
  const end = new Date(range.to);
  end.setHours(0, 0, 0, 0);
  while (cursor.getTime() <= end.getTime()) {
    dates.push(localDateKey(cursor));
    cursor.setDate(cursor.getDate() + 1);
  }
  const dateIndex = new Map<string, number>();
  dates.forEach((d, i) => dateIndex.set(d, i));

  const series: Record<string, number[]> = {};
  const totals = new Map<string, number>();
  for (const s of sessions) {
    if (!isSessionItem(s) || !s.daily_tokens) continue;
    const model = s.model && s.model.trim() ? s.model : "Unknown";
    for (const entry of s.daily_tokens) {
      const di = dateIndex.get(entry.date);
      if (di == null) continue;
      let row = series[model];
      if (!row) {
        row = dates.map(() => 0);
        series[model] = row;
      }
      row[di] += entry.tokens.total;
      totals.set(model, (totals.get(model) ?? 0) + entry.tokens.total);
    }
  }
  const models = Array.from(totals.keys()).sort(
    (a, b) => (totals.get(b) ?? 0) - (totals.get(a) ?? 0) || a.localeCompare(b)
  );
  return { dates, models, series };
}

// ─── Model display names ──────────────────────────────────────────────────────

/**
 * Friendly model name for the Claude family — "claude-opus-4-8" →
 * "Opus 4.8", "claude-3-5-haiku-20241022" → "Haiku 3.5",
 * "anthropic/claude-fable-5" → "Fable 5". Non-Claude ids (gpt-5,
 * gemini-2.5-pro, …) pass through unchanged, as does "Unknown".
 */
export function displayModelName(id: string): string {
  const m =
    /(?:^|\/)claude-(?:(\d+)-(\d+)-)?(opus|sonnet|haiku|fable)(?:-(\d+(?:-\d+)*))?/i.exec(
      id
    );
  if (!m) return id;
  const role = m[3][0].toUpperCase() + m[3].slice(1).toLowerCase();
  // Version prefix form ("claude-3-5-sonnet-…") or suffix form
  // ("claude-opus-4-8"); a trailing 8-digit segment is a DATE, not a
  // version ("…-haiku-20241022").
  let version = "";
  if (m[1]) {
    version = `${m[1]}.${m[2]}`;
  } else if (m[4]) {
    const parts = m[4].split("-").filter((p) => p.length < 8);
    version = parts.join(".");
  }
  return version ? `${role} ${version}` : role;
}

// ─── Calendar heatmap grid ────────────────────────────────────────────────────

export type DayCell = { date: string; messages: number; tokens: number };

/**
 * GitHub-style week columns for the Overview heatmap: each column is
 * one week (rows Sun→Sat), the first/last columns padded with null so
 * every column is exactly 7 cells. Input is `dailyTokens` output (one
 * entry per day, already date-ordered).
 */
export function calendarWeeks(daily: DailyTokens[]): (DayCell | null)[][] {
  if (daily.length === 0) return [];
  const cells: (DayCell | null)[] = daily.map((d) => ({
    date: d.date,
    messages: d.messages,
    tokens: d.total
  }));
  // Parse the first date key as LOCAL (new Date("YYYY-MM-DD") is UTC).
  const [y, m, d] = daily[0].date.split("-").map(Number);
  const firstWeekday = new Date(y, m - 1, d).getDay();
  const padded: (DayCell | null)[] = [
    ...Array.from({ length: firstWeekday }, () => null),
    ...cells
  ];
  while (padded.length % 7 !== 0) padded.push(null);
  const weeks: (DayCell | null)[][] = [];
  for (let i = 0; i < padded.length; i += 7) weeks.push(padded.slice(i, i + 7));
  return weeks;
}

// ─── Hourly activity (Overview day view) ──────────────────────────────────────

export type HourlyActivity = {
  /** Per-hour (0-23, local) message counts for one day. */
  messages: number[];
  /** Per-hour total tokens for the same day. */
  tokens: number[];
};

/**
 * The 24-hour message/token distribution for a single day (`YYYY-MM-DD`
 * local key) — sums every session's `daily_tokens[date].hours` /
 * `hour_tokens` for that date. Same session-set the caller already
 * source-filtered; days with no hourly data return all-zero arrays.
 */
export function hourlyActivity(
  sessions: AppSession[],
  dateKey: string
): HourlyActivity {
  const messages = new Array<number>(24).fill(0);
  const tokens = new Array<number>(24).fill(0);
  for (const s of sessions) {
    if (!isSessionItem(s) || !s.daily_tokens) continue;
    for (const e of s.daily_tokens) {
      if (e.date !== dateKey) continue;
      if (e.hours && e.hours.length === 24) {
        for (let h = 0; h < 24; h++) messages[h] += e.hours[h];
      }
      if (e.hour_tokens && e.hour_tokens.length === 24) {
        for (let h = 0; h < 24; h++) tokens[h] += e.hour_tokens[h];
      }
    }
  }
  return { messages, tokens };
}
