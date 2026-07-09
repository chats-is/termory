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

// ─── Module 1 · KPI cards ─────────────────────────────────────────────────────
//
// One statistics function per Stats UI module (4 modules → 4 functions).
// `overviewKpis` produces EVERY value the 8 KPI cards show, in one pass —
// it folds together what used to be three separate helpers (window totals,
// streak/peak KPIs, and the top-model pick). EVERY field is window-accurate:
//  - sessions: count where `started_at` falls in the window
//  - messages / tokens: sum of in-window `daily_tokens` (no lifetime totals,
//    no even-distribution smearing — sessions without `daily_tokens` add zero)
//  - activeDays / streaks: consecutive in-window days with any activity
//  - peakHour: local hour (0-23) with the most in-window messages
//  - favoriteModel: the model with the most in-window tokens (skipping the
//    "Unknown" bucket); session-level attribution (one model per session)

export type OverviewKpis = {
  sessions: number;
  messages: number;
  tokens: TokenStats;
  /** Days in the window with any activity (messages or tokens). */
  activeDays: number;
  /** Consecutive active days ending at the window's last day — with a
   * one-day grace: a not-yet-active today counts back from yesterday. */
  currentStreak: number;
  /** Longest run of consecutive active days in the window. */
  longestStreak: number;
  /** Local hour (0-23) with the most messages; null with no activity. */
  peakHour: number | null;
  /** Model id with the most in-window tokens, skipping "Unknown"; null
   * when nothing datable is in the window. */
  favoriteModel: string | null;
};

export function overviewKpis(
  sessions: AppSession[],
  range: { from: Date; to: Date }
): OverviewKpis {
  const fromKey = localDateKey(range.from);
  const toKey = localDateKey(range.to);
  const inRange = (date: string) => date >= fromKey && date <= toKey;

  // Per-day activity across the window drives activeDays + the streaks.
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

  // Single session pass for the totals, per-hour messages, and per-model
  // tokens (favorite model).
  let sessionCount = 0;
  let messages = 0;
  const tokens: TokenStats = {
    input: 0,
    output: 0,
    cached: 0,
    reasoning: 0,
    total: 0
  };
  const hourMessages = new Array<number>(24).fill(0);
  const modelTokens = new Map<string, number>();
  for (const s of sessions) {
    if (!isSessionItem(s)) continue;
    const model = s.model && s.model.trim() ? s.model : "Unknown";

    if (s.started_at) {
      const startTs = new Date(s.started_at);
      if (!Number.isNaN(startTs.getTime()) && inRange(localDateKey(startTs))) {
        sessionCount += 1;
      }
    }
    if (!s.daily_tokens) continue;
    for (const entry of s.daily_tokens) {
      if (!inRange(entry.date)) continue;
      tokens.input += entry.tokens.input;
      tokens.output += entry.tokens.output;
      tokens.cached += entry.tokens.cached;
      tokens.reasoning += entry.tokens.reasoning;
      tokens.total += entry.tokens.total;
      messages += entry.messages ?? 0;
      if (model !== "Unknown") {
        modelTokens.set(model, (modelTokens.get(model) ?? 0) + entry.tokens.total);
      }
      if (entry.hours && entry.hours.length === 24) {
        for (let h = 0; h < 24; h++) hourMessages[h] += entry.hours[h];
      }
    }
  }

  let peakHour: number | null = null;
  let bestHour = 0;
  for (let h = 0; h < 24; h++) {
    if (hourMessages[h] > bestHour) {
      bestHour = hourMessages[h];
      peakHour = h;
    }
  }

  let favoriteModel: string | null = null;
  let bestModel = 0;
  for (const [model, tok] of modelTokens) {
    if (tok > bestModel) {
      bestModel = tok;
      favoriteModel = model;
    }
  }

  return {
    sessions: sessionCount,
    messages,
    tokens,
    activeDays,
    currentStreak,
    longestStreak,
    peakHour,
    favoriteModel
  };
}

// ─── Module 2 · Activity calendar heatmap ─────────────────────────────────────
//
// The heatmap follows the SOURCE filter but NOT the All/30d/7d range — it is
// always the full history (a GitHub-style contribution graph). So this
// function takes source-filtered sessions and builds its OWN fixed window
// (365 days back → today) internally; it never sees `resolveRange`'s output.
// That "no date-range condition" is exactly why it's a separate module from
// the range-scoped Tokens chart.

export type DailyActivity = { date: string; messages: number; tokens: number };

export function dailyActivity(sessions: AppSession[]): DailyActivity[] {
  const to = new Date();
  to.setHours(0, 0, 0, 0);
  const cursor = new Date();
  cursor.setDate(cursor.getDate() - 364); // fixed 365-day window
  cursor.setHours(0, 0, 0, 0);
  const buckets = new Map<string, DailyActivity>();
  while (cursor.getTime() <= to.getTime()) {
    const key = localDateKey(cursor);
    buckets.set(key, { date: key, messages: 0, tokens: 0 });
    cursor.setDate(cursor.getDate() + 1);
  }
  for (const s of sessions) {
    if (!isSessionItem(s) || !s.daily_tokens) continue;
    for (const entry of s.daily_tokens) {
      const bucket = buckets.get(entry.date);
      if (!bucket) continue;
      bucket.messages += entry.messages ?? 0;
      bucket.tokens += entry.tokens.total;
    }
  }
  return Array.from(buckets.values());
}

// ─── Module 3 · Tokens by type ────────────────────────────────────────────────

export type DailyTokens = {
  date: string; // YYYY-MM-DD (local)
  /** Number of AI interactions on this date (sum of
   * `daily_tokens[date].messages`). */
  messages: number;
  /** Total tokens that day. Named `total` (not `tokens`) so it aligns
   * with `TokenStats.total` and avoids collision with the TokenStats
   * *object* called `tokens` on AppSession. */
  total: number;
  input: number;
  output: number;
  cached: number;
  reasoning: number;
};

/**
 * Per-day token rollups split by kind (input/output/cached/reasoning),
 * scoped to the given range — the "Type" mode of the Tokens chart.
 *
 * Source: each session's `daily_tokens` array (produced by the four
 * scanners when underlying records carry timestamps). Sessions without
 * `daily_tokens` contribute zero — Termory does NOT smear lifetime
 * totals across the date range, because that would fabricate per-day
 * numbers that look identical to real data. Days outside the range are
 * dropped.
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

// ─── Module 4 · Tokens by model ───────────────────────────────────────────────

/** One legend row for the model chart: window totals for a single model
 * (input/output split + total). `model` is `"Unknown"` for sessions whose
 * source didn't record one. */
export type ModelLegendRow = {
  model: string;
  tokens: number;
  input: number;
  output: number;
};

export type DailyModelTokens = {
  /** Every date in the window (same axis as `dailyTokens`). */
  dates: string[];
  /** Models sorted by window token total desc (then name); includes
   * "Unknown". */
  models: string[];
  /** Per-model daily totals, aligned with `dates`. */
  series: Record<string, number[]>;
  /** Per-model window totals for the legend, same order as `models`
   * (folds in what used to be a separate `modelBreakdown`). */
  legend: ModelLegendRow[];
};

/**
 * The "Model" mode of the Tokens chart: per-day token totals split by
 * model (stacked series) PLUS the per-model legend totals — one function
 * for the whole module. Attribution is SESSION-level (one best-guess model
 * per session), so summing every model's series for a date equals that
 * date's `dailyTokens[].total`, and the legend totals reconcile with the
 * Type mode's window total.
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
  const legendMap = new Map<string, ModelLegendRow>();
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
      let leg = legendMap.get(model);
      if (!leg) {
        leg = { model, tokens: 0, input: 0, output: 0 };
        legendMap.set(model, leg);
      }
      leg.tokens += entry.tokens.total;
      leg.input += entry.tokens.input;
      leg.output += entry.tokens.output;
    }
  }
  const legend = Array.from(legendMap.values()).sort(
    (a, b) => b.tokens - a.tokens || a.model.localeCompare(b.model)
  );
  const models = legend.map((l) => l.model);
  return { dates, models, series, legend };
}
