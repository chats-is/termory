import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AppSession } from "../types";
import {
  niceMax,
  dailyTokens,
  filterSessions,
  dailyActivities,
  modelBreakdown,
  resolveRange,
  sessionTimestamp,
  windowTotals
} from "./stats-utils";

function mk(partial: Partial<AppSession>): AppSession {
  return {
    id: "x",
    source: "Codex",
    title: "t",
    project: "",
    path: "/p",
    started_at: null,
    updated_at: null,
    message_count: 0,
    preview: "",
    snippet: "",
    message_previews: [],
    ...partial
  };
}

function withTokens(
  base: Partial<AppSession>,
  tokens: { input: number; output: number; cached?: number; reasoning?: number }
): AppSession {
  const cached = tokens.cached ?? 0;
  const reasoning = tokens.reasoning ?? 0;
  return mk({
    ...base,
    tokens: {
      input: tokens.input,
      output: tokens.output,
      cached,
      reasoning,
      total: tokens.input + tokens.output + cached + reasoning
    }
  });
}

describe("sessionTimestamp", () => {
  it("prefers updated_at over started_at", () => {
    const s = mk({
      started_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-02-01T00:00:00Z"
    });
    expect(sessionTimestamp(s)?.toISOString()).toBe("2026-02-01T00:00:00.000Z");
  });
  it("falls back to started_at when updated_at is null", () => {
    const s = mk({ started_at: "2026-03-15T10:00:00Z" });
    expect(sessionTimestamp(s)?.toISOString()).toBe("2026-03-15T10:00:00.000Z");
  });
  it("returns null for unparseable timestamps", () => {
    expect(sessionTimestamp(mk({ updated_at: "not-a-date" }))).toBeNull();
  });
  it("returns null when both fields are missing", () => {
    expect(sessionTimestamp(mk({}))).toBeNull();
  });
});

describe("resolveRange", () => {
  const now = new Date("2026-05-29T12:00:00Z");
  it("returns a single local day (00:00 → 23:59:59.999) for 'today'", () => {
    const r = resolveRange({ preset: "today" }, now);
    expect(r.from.getHours()).toBe(0);
    expect(r.from.getMinutes()).toBe(0);
    expect(r.to.getHours()).toBe(23);
    expect(r.to.getMinutes()).toBe(59);
    // Same calendar day, just start-of-day → end-of-day.
    expect(r.from.getFullYear()).toBe(r.to.getFullYear());
    expect(r.from.getMonth()).toBe(r.to.getMonth());
    expect(r.from.getDate()).toBe(r.to.getDate());
  });
  it("returns 7-day window for '7d'", () => {
    const r = resolveRange({ preset: "7d" }, now);
    const diffDays = (r.to.getTime() - r.from.getTime()) / 86_400_000;
    expect(Math.round(diffDays)).toBeGreaterThanOrEqual(6);
    expect(Math.round(diffDays)).toBeLessThanOrEqual(7);
  });
  it("returns the literal range for 'custom'", () => {
    const from = new Date("2026-01-01T00:00:00Z");
    const to = new Date("2026-02-01T00:00:00Z");
    const r = resolveRange({ preset: "custom", from, to }, now);
    expect(r.from).toEqual(from);
    expect(r.to).toEqual(to);
  });
  it("extends `to` to end-of-today so sessions written later in the day still pass the filter", () => {
    // Regression: when `to` was `now`, any session updated AFTER the
    // Stats page loaded (e.g. claude --continue mid-page) would drop
    // out of the chart on the next watcher rescan, silently losing
    // today's data.
    const pageOpenTime = new Date("2026-05-29T12:00:00");
    const r = resolveRange({ preset: "30d" }, pageOpenTime);
    expect(r.to.getHours()).toBe(23);
    expect(r.to.getMinutes()).toBe(59);
    expect(r.to.getSeconds()).toBe(59);
    const laterToday = new Date("2026-05-29T22:00:00");
    expect(laterToday.getTime()).toBeLessThan(r.to.getTime());
  });
});

describe("filterSessions", () => {
  const range = {
    from: new Date("2026-05-01T00:00:00Z"),
    to: new Date("2026-05-31T23:59:59Z")
  };
  it("drops sessions outside the range", () => {
    const inside = mk({ updated_at: "2026-05-15T10:00:00Z", source: "Claude" });
    const outside = mk({ updated_at: "2026-04-30T23:00:00Z", source: "Claude" });
    expect(filterSessions([inside, outside], range, "All")).toHaveLength(1);
  });
  it("filters by source when source !== 'All' (case-insensitive)", () => {
    const codex = mk({ updated_at: "2026-05-10T00:00:00Z", source: "Codex" });
    const claude = mk({ updated_at: "2026-05-10T00:00:00Z", source: "Claude" });
    expect(filterSessions([codex, claude], range, "claude")).toEqual([claude]);
  });
  it("drops sessions with no usable timestamp", () => {
    const noTs = mk({ source: "Claude" });
    expect(filterSessions([noTs], range, "All")).toHaveLength(0);
  });
  it("keeps a session whose interval OVERLAPS the window even if updated_at is outside", () => {
    // Regression: filter used to look only at `updated_at`. A session
    // created BEFORE the window with messages IN the window (and
    // updated_at AFTER the window) was silently dropped, leaving its
    // in-window daily_tokens uncounted in Messages / Tokens.
    const pastWindow = {
      from: new Date("2025-12-20T00:00:00"),
      to: new Date("2025-12-25T23:59:59")
    };
    const session = mk({
      source: "Claude",
      started_at: "2025-12-15T10:00:00",
      updated_at: "2025-12-30T10:00:00"
    });
    expect(filterSessions([session], pastWindow, "All")).toHaveLength(1);
  });
  it("drops a session whose interval is entirely outside the window", () => {
    const window = {
      from: new Date("2026-05-01T00:00:00"),
      to: new Date("2026-05-31T23:59:59")
    };
    const before = mk({
      source: "Claude",
      started_at: "2026-04-01T10:00:00",
      updated_at: "2026-04-15T10:00:00"
    });
    const after = mk({
      source: "Claude",
      started_at: "2026-06-01T10:00:00",
      updated_at: "2026-06-15T10:00:00"
    });
    expect(filterSessions([before, after], window, "All")).toHaveLength(0);
  });
});

describe("windowTotals", () => {
  const range = {
    from: new Date("2026-05-28T00:00:00"),
    to: new Date("2026-05-30T23:59:59")
  };

  it("counts sessions started in window and aggregates in-range daily_tokens", () => {
    const sessions: AppSession[] = [
      mk({
        source: "Claude",
        project: "/a",
        started_at: "2026-05-28T10:00:00",
        updated_at: "2026-05-29T11:00:00",
        daily_tokens: [
          {
            date: "2026-05-28",
            tokens: { input: 100, output: 50, cached: 0, reasoning: 0, total: 150 },
            messages: 3
          },
          {
            date: "2026-05-29",
            tokens: { input: 200, output: 80, cached: 0, reasoning: 0, total: 280 },
            messages: 5
          }
        ]
      }),
      mk({
        source: "Codex",
        project: "/b",
        started_at: "2026-05-29T08:00:00",
        updated_at: "2026-05-29T09:00:00",
        daily_tokens: [
          {
            date: "2026-05-29",
            tokens: { input: 40, output: 10, cached: 0, reasoning: 0, total: 50 },
            messages: 1
          }
        ]
      })
    ];
    const t = windowTotals(sessions, range);
    expect(t.sessions).toBe(2);
    expect(t.messages).toBe(9);
    expect(t.tokens).toEqual({
      input: 340,
      output: 140,
      cached: 0,
      reasoning: 0,
      total: 480
    });
    expect(t.projects).toBe(2);
  });

  it("excludes daily_tokens entries OUTSIDE the window", () => {
    const sessions: AppSession[] = [
      mk({
        source: "Claude",
        project: "/old",
        started_at: "2026-05-01T10:00:00",  // before window
        updated_at: "2026-05-29T11:00:00",
        daily_tokens: [
          // before window — must NOT count
          {
            date: "2026-05-01",
            tokens: { input: 1000, output: 500, cached: 0, reasoning: 0, total: 1500 },
            messages: 50
          },
          // in window — counts
          {
            date: "2026-05-29",
            tokens: { input: 10, output: 5, cached: 0, reasoning: 0, total: 15 },
            messages: 1
          }
        ]
      })
    ];
    const t = windowTotals(sessions, range);
    expect(t.sessions).toBe(0); // started_at before window
    expect(t.messages).toBe(1); // only the in-window entry
    expect(t.tokens.total).toBe(15);
    expect(t.projects).toBe(1); // contributed via in-window daily_tokens
  });

  it("ignores sessions without daily_tokens AND without started_at in window", () => {
    const sessions: AppSession[] = [
      // No daily_tokens, started before window — should contribute zero.
      withTokens(
        {
          source: "Claude",
          project: "/lifetime",
          started_at: "2026-05-01T10:00:00",
          updated_at: "2026-05-29T10:00:00"
        },
        { input: 9_999_999, output: 9_999_999 }
      )
    ];
    const t = windowTotals(sessions, range);
    expect(t.sessions).toBe(0);
    expect(t.messages).toBe(0);
    expect(t.tokens.total).toBe(0);
    expect(t.projects).toBe(0);
  });

  it("ignores Memory / Skill items", () => {
    const t = windowTotals(
      [
        mk({ source: "Memory", started_at: "2026-05-29T10:00:00" }),
        mk({ source: "Skill", started_at: "2026-05-29T10:00:00" })
      ],
      range
    );
    expect(t).toEqual({
      sessions: 0,
      messages: 0,
      tokens: { input: 0, output: 0, cached: 0, reasoning: 0, total: 0 },
      projects: 0
    });
  });
});

describe("modelBreakdown", () => {
  const range = {
    from: new Date("2026-05-28T00:00:00"),
    to: new Date("2026-05-30T23:59:59")
  };

  const tk = (total: number) => ({
    input: total,
    output: 0,
    cached: 0,
    reasoning: 0,
    total
  });

  it("groups in-window sessions + tokens by model, sorted by tokens desc", () => {
    const sessions: AppSession[] = [
      mk({
        source: "Claude",
        model: "claude-opus-4-7",
        started_at: "2026-05-28T10:00:00",
        daily_tokens: [{ date: "2026-05-28", tokens: tk(100), messages: 2 }]
      }),
      mk({
        source: "Claude",
        model: "claude-opus-4-7",
        started_at: "2026-05-29T10:00:00",
        daily_tokens: [{ date: "2026-05-29", tokens: tk(50), messages: 1 }]
      }),
      mk({
        source: "Codex",
        model: "gpt-5",
        started_at: "2026-05-29T11:00:00",
        daily_tokens: [{ date: "2026-05-29", tokens: tk(500), messages: 3 }]
      })
    ];
    const b = modelBreakdown(sessions, range);
    expect(b).toEqual([
      { model: "gpt-5", sessions: 1, tokens: 500 },
      { model: "claude-opus-4-7", sessions: 2, tokens: 150 }
    ]);
  });

  it("buckets sessions with no model under 'Unknown'", () => {
    const b = modelBreakdown(
      [
        mk({
          source: "Gemini",
          started_at: "2026-05-29T10:00:00",
          daily_tokens: [{ date: "2026-05-29", tokens: tk(30), messages: 1 }]
        })
      ],
      range
    );
    expect(b).toEqual([{ model: "Unknown", sessions: 1, tokens: 30 }]);
  });

  it("uses windowTotals accounting: out-of-window tokens excluded, started_at gates the session count", () => {
    const b = modelBreakdown(
      [
        mk({
          source: "Claude",
          model: "claude-opus-4-7",
          started_at: "2026-05-01T10:00:00", // before window → not counted as a session
          daily_tokens: [
            { date: "2026-05-01", tokens: tk(9999), messages: 9 }, // out of window
            { date: "2026-05-29", tokens: tk(20), messages: 1 } // in window
          ]
        })
      ],
      range
    );
    expect(b).toEqual([{ model: "claude-opus-4-7", sessions: 0, tokens: 20 }]);
  });

  it("ignores Memory / Skill items", () => {
    expect(
      modelBreakdown(
        [
          mk({
            source: "Memory",
            model: "x",
            started_at: "2026-05-29T10:00:00"
          })
        ],
        range
      )
    ).toEqual([]);
  });
});

describe("dailyTokens", () => {
  const range = {
    from: new Date("2026-05-28T00:00:00"),
    to: new Date("2026-05-29T23:59:59")
  };

  it("prefers backend daily_tokens over even-distribution fallback", () => {
    // When the backend provides per-message-bucketed daily_tokens, the
    // chart uses those exact numbers — NOT a smeared even-split.
    const wide = {
      from: new Date("2026-05-26T00:00:00"),
      to: new Date("2026-05-29T23:59:59")
    };
    const sessions: AppSession[] = [
      mk({
        started_at: "2026-05-26T08:00:00",
        updated_at: "2026-05-29T10:00:00",
        tokens: {
          input: 800,
          output: 200,
          cached: 0,
          reasoning: 0,
          total: 1000
        },
        daily_tokens: [
          {
            date: "2026-05-26",
            tokens: { input: 700, output: 100, cached: 0, reasoning: 0, total: 800 }
          },
          {
            date: "2026-05-28",
            tokens: { input: 100, output: 100, cached: 0, reasoning: 0, total: 200 }
          }
          // Note: May 27 and May 29 NOT in daily_tokens → they remain
          // at zero, NOT given an even-split share.
        ]
      })
    ];
    const out = dailyTokens(sessions, wide);
    expect(out.find((d) => d.date === "2026-05-26")!.total).toBe(800);
    expect(out.find((d) => d.date === "2026-05-27")!.total).toBe(0);
    expect(out.find((d) => d.date === "2026-05-28")!.total).toBe(200);
    expect(out.find((d) => d.date === "2026-05-29")!.total).toBe(0);
  });

  it("ignores sessions that have no daily_tokens (no smearing of lifetime totals)", () => {
    const wide = {
      from: new Date("2026-05-26T00:00:00"),
      to: new Date("2026-05-30T23:59:59")
    };
    const sessions = [
      // Has tokens but NO daily_tokens — should contribute zero, NOT
      // be smeared across [started_at, updated_at].
      withTokens(
        {
          started_at: "2026-05-26T20:00:00",
          updated_at: "2026-05-30T20:00:00"
        },
        { input: 600_000, output: 200_000, cached: 200_000 }
      )
    ];
    const out = dailyTokens(sessions, wide);
    for (const d of out) expect(d.total).toBe(0);
  });

  it("emits a dense series across the range, with zero rows for empty days", () => {
    const out = dailyTokens([], range);
    expect(out.map((d) => d.date)).toEqual(["2026-05-28", "2026-05-29"]);
    for (const d of out) {
      expect(d.total).toBe(0);
    }
  });
});

describe("dailyActivities", () => {
  const range = {
    from: new Date("2026-05-28T00:00:00"),
    to: new Date("2026-05-29T23:59:59")
  };

  it("builds a 24-row × N-date messages grid from daily_tokens.hours", () => {
    const sessions: AppSession[] = [
      mk({
        started_at: "2026-05-28T10:00:00",
        updated_at: "2026-05-28T18:00:00",
        daily_tokens: [
          {
            date: "2026-05-28",
            tokens: { input: 0, output: 0, cached: 0, reasoning: 0, total: 0 },
            hours: hoursWith({ 10: 3, 14: 2 }),
            hour_tokens: hoursWith({ 10: 1500, 14: 800 })
          }
        ]
      })
    ];
    const out = dailyActivities(sessions, range);
    expect(out.dates).toEqual(["2026-05-28", "2026-05-29"]);
    expect(out.messages[10][0]).toBe(3);
    expect(out.messages[14][0]).toBe(2);
    expect(out.messages[11][0]).toBe(0);
    expect(out.tokens[10][0]).toBe(1500);
    expect(out.tokens[14][0]).toBe(800);
  });

  it("places a session's started_at in exactly one (date, hour) cell", () => {
    const sessions = [
      mk({
        started_at: "2026-05-28T14:30:00",
        updated_at: "2026-05-28T15:00:00"
      })
    ];
    const out = dailyActivities(sessions, range);
    expect(out.sessions[14][0]).toBe(1);
    // No spill into any other cell.
    let totalSessions = 0;
    for (let h = 0; h < 24; h++) {
      for (let d = 0; d < out.dates.length; d++) totalSessions += out.sessions[h][d];
    }
    expect(totalSessions).toBe(1);
  });

  it("counts a session only when started_at is set (no updated_at fallback)", () => {
    const sessions = [
      // Has started_at → counted on the 28th, hour 10.
      mk({
        started_at: "2026-05-28T10:00:00",
        updated_at: "2026-05-29T18:00:00"
      }),
      // No started_at — should NOT be counted (old session touched today
      // shouldn't look "created today").
      mk({ updated_at: "2026-05-29T10:00:00" })
    ];
    const out = dailyActivities(sessions, range);
    expect(out.sessions[10][0]).toBe(1);
    let total = 0;
    for (let h = 0; h < 24; h++) {
      for (let d = 0; d < out.dates.length; d++) total += out.sessions[h][d];
    }
    expect(total).toBe(1);
  });

  it("ignores Memory / Skill items", () => {
    const out = dailyActivities(
      [
        mk({ source: "Memory", started_at: "2026-05-28T10:00:00" }),
        mk({ source: "Skill", started_at: "2026-05-29T10:00:00" })
      ],
      range
    );
    let total = 0;
    for (let h = 0; h < 24; h++) {
      for (let d = 0; d < out.dates.length; d++) total += out.sessions[h][d];
    }
    expect(total).toBe(0);
  });

  it("attributes per-cell messages/tokens to each session's model", () => {
    const sessions: AppSession[] = [
      mk({
        source: "Claude",
        model: "claude-opus-4-7",
        started_at: "2026-05-28T10:00:00",
        daily_tokens: [
          {
            date: "2026-05-28",
            tokens: { input: 0, output: 0, cached: 0, reasoning: 0, total: 0 },
            hours: hoursWith({ 10: 2 }),
            hour_tokens: hoursWith({ 10: 1000 })
          }
        ]
      }),
      mk({
        source: "Codex",
        model: "gpt-5",
        started_at: "2026-05-28T10:00:00",
        daily_tokens: [
          {
            date: "2026-05-28",
            tokens: { input: 0, output: 0, cached: 0, reasoning: 0, total: 0 },
            hours: hoursWith({ 10: 1 }),
            hour_tokens: hoursWith({ 10: 4000 })
          }
        ]
      })
    ];
    const out = dailyActivities(sessions, range);
    // Cell (hour 10, date index 0) — both models, sorted by tokens desc.
    expect(out.models[10][0]).toEqual([
      { model: "gpt-5", messages: 1, tokens: 4000 },
      { model: "claude-opus-4-7", messages: 2, tokens: 1000 }
    ]);
    // A cell with no activity is an empty list.
    expect(out.models[11][0]).toEqual([]);
  });

  it("buckets model-less sessions under 'Unknown' in the cell", () => {
    const out = dailyActivities(
      [
        mk({
          started_at: "2026-05-28T09:00:00",
          daily_tokens: [
            {
              date: "2026-05-28",
              tokens: { input: 0, output: 0, cached: 0, reasoning: 0, total: 0 },
              hours: hoursWith({ 9: 4 }),
              hour_tokens: hoursWith({ 9: 200 })
            }
          ]
        })
      ],
      range
    );
    expect(out.models[9][0]).toEqual([
      { model: "Unknown", messages: 4, tokens: 200 }
    ]);
  });
});

/** Build a 24-slot hours array with the given hour→count overrides. */
function hoursWith(overrides: Record<number, number>): number[] {
  const out = new Array(24).fill(0) as number[];
  for (const [h, v] of Object.entries(overrides)) out[Number(h)] = v;
  return out;
}

describe("integration: filter → bucket", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-05-29T12:00:00Z"));
  });
  afterEach(() => vi.useRealTimers());

  it("30-day filter passes recent sessions and drops older ones", () => {
    const sessions = [
      mk({ source: "Claude", updated_at: "2026-05-10T10:00:00Z" }),
      mk({ source: "Claude", updated_at: "2026-05-20T10:00:00Z" }),
      mk({ source: "Codex", updated_at: "2026-05-25T10:00:00Z" }),
      mk({ source: "Claude", updated_at: "2026-03-01T10:00:00Z" }) // outside
    ];
    const range = resolveRange({ preset: "30d" });
    const filtered = filterSessions(sessions, range, "All");
    expect(filtered).toHaveLength(3);
  });
});

describe("niceMax", () => {
  it("rounds up to a clean 1 / 2 / 2.5 / 5 × 10ⁿ bound", () => {
    expect(niceMax(1)).toBe(1);
    expect(niceMax(1.1)).toBe(2);
    expect(niceMax(2)).toBe(2);
    expect(niceMax(2.1)).toBe(2.5);
    expect(niceMax(2.5)).toBe(2.5);
    expect(niceMax(3)).toBe(5);
    expect(niceMax(5)).toBe(5);
    expect(niceMax(6)).toBe(10);
  });

  it("scales across magnitudes (K → B)", () => {
    expect(niceMax(50_000)).toBe(50_000);
    expect(niceMax(1_800_000_000)).toBe(2_000_000_000);
    expect(niceMax(2_100_000_000)).toBe(2_500_000_000);
  });

  it("returns 1 for non-positive input (empty axis)", () => {
    expect(niceMax(0)).toBe(1);
    expect(niceMax(-5)).toBe(1);
  });
});
