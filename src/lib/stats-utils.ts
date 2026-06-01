// Pure aggregation helpers used by the Stats page. Everything in here
// is side-effect free so it can be unit-tested without a DOM, and the
// Stats page renders the results via `useMemo`.
//
// The input shape is always `AppSession[]` — the same data
// `scan_all_sessions` returns. No Rust IPC changes; Stats is purely a
// view over already-collected metadata.

import type { AppSession, CliApp, TokenStats } from "../types";
import { isSessionItem } from "./session-utils";

export type DateRangePreset = "7d" | "30d" | "90d" | "365d" | "all" | "custom";

export type DateRange =
  | { preset: Exclude<DateRangePreset, "custom"> }
  | { preset: "custom"; from: Date; to: Date };

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
  now: Date = new Date()
): { from: Date; to: Date } {
  if (range.preset === "custom") {
    return { from: range.from, to: range.to };
  }
  const to = new Date(now);
  to.setHours(23, 59, 59, 999);
  if (range.preset === "all") {
    // 50 years back covers any conceivable AppSession; cleaner than
    // dealing with `null`/`undefined` `from` in callers.
    const from = new Date(now);
    from.setFullYear(from.getFullYear() - 50);
    return { from, to };
  }
  const days =
    range.preset === "7d"
      ? 7
      : range.preset === "30d"
        ? 30
        : range.preset === "90d"
          ? 90
          : 365;
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
 * Per-hour-per-date matrices for the workload heatmap. All three
 * matrices share the same `dates` axis (column index) so the hover
 * card can read corresponding hour/date data from any of them.
 *
 *   messages[hour][date] — count of AI interactions
 *   tokens[hour][date]   — total tokens
 *   sessions[hour][date] — sessions whose `started_at` fell on
 *                          (date, hour). Sparse — most cells are 0.
 */
export type DailyActivities = {
  dates: string[];
  messages: number[][];
  tokens: number[][];
  sessions: number[][];
};

/** Build the 24-hour × N-date matrices for messages / tokens /
 * sessions-started. */
export function dailyActivities(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): DailyActivities {
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
  const messages: number[][] = Array.from({ length: 24 }, () =>
    dates.map(() => 0)
  );
  const tokens: number[][] = Array.from({ length: 24 }, () =>
    dates.map(() => 0)
  );
  const sessionsM: number[][] = Array.from({ length: 24 }, () =>
    dates.map(() => 0)
  );
  for (const s of sessions) {
    if (!isSessionItem(s)) continue;

    // Session-creation row: a session's started_at lands at exactly
    // one (date, hour) cell. Used for the "Sessions" hover row.
    if (s.started_at) {
      const startTs = new Date(s.started_at);
      if (!Number.isNaN(startTs.getTime())) {
        const di = dateIndex.get(localDateKey(startTs));
        if (di != null) sessionsM[startTs.getHours()][di] += 1;
      }
    }

    if (!s.daily_tokens) continue;
    for (const entry of s.daily_tokens) {
      const di = dateIndex.get(entry.date);
      if (di == null) continue;
      if (entry.hours && entry.hours.length === 24) {
        for (let h = 0; h < 24; h++) {
          messages[h][di] += entry.hours[h];
        }
      }
      if (entry.hour_tokens && entry.hour_tokens.length === 24) {
        for (let h = 0; h < 24; h++) {
          tokens[h][di] += entry.hour_tokens[h];
        }
      }
    }
  }
  return { dates, messages, tokens, sessions: sessionsM };
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
