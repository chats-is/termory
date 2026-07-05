import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AppSession } from "../types";
import {
  calendarWeeks,
  dailyModelTokens,
  dailyTokens,
  displayModelName,
  filterSessions,
  modelBreakdown,
  niceMax,
  overviewKpis,
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
  it("returns 7-day window for '7d'", () => {
    const r = resolveRange({ preset: "7d" }, [], now);
    const diffDays = (r.to.getTime() - r.from.getTime()) / 86_400_000;
    expect(Math.round(diffDays)).toBeGreaterThanOrEqual(6);
    expect(Math.round(diffDays)).toBeLessThanOrEqual(7);
  });
  it("extends `to` to end-of-today so sessions written later in the day still pass the filter", () => {
    // Regression: when `to` was `now`, any session updated AFTER the
    // Stats page loaded (e.g. claude --continue mid-page) would drop
    // out of the chart on the next watcher rescan, silently losing
    // today's data.
    const pageOpenTime = new Date("2026-05-29T12:00:00");
    const r = resolveRange({ preset: "30d" }, [], pageOpenTime);
    expect(r.to.getHours()).toBe(23);
    expect(r.to.getMinutes()).toBe(59);
    expect(r.to.getSeconds()).toBe(59);
    const laterToday = new Date("2026-05-29T22:00:00");
    expect(laterToday.getTime()).toBeLessThan(r.to.getTime());
  });
  it("'all' starts at the earliest daily_tokens date", () => {
    const sessions = [
      mk({
        started_at: "2026-04-10T10:00:00",
        daily_tokens: [
          {
            date: "2026-03-05",
            tokens: { input: 1, output: 0, cached: 0, reasoning: 0, total: 1 },
            messages: 1
          }
        ]
      })
    ];
    const r = resolveRange({ preset: "all" }, sessions, now);
    expect(r.from.getFullYear()).toBe(2026);
    expect(r.from.getMonth()).toBe(2); // March
    expect(r.from.getDate()).toBe(5);
    expect(r.to.getHours()).toBe(23);
  });
  it("'all' falls back to started_at when there are no daily_tokens", () => {
    const sessions = [mk({ started_at: "2026-02-01T08:00:00" })];
    const r = resolveRange({ preset: "all" }, sessions, now);
    expect(r.from.getMonth()).toBe(1); // February
    expect(r.from.getDate()).toBe(1);
  });
  it("'all' with no datable activity degrades to the 30d window", () => {
    const r = resolveRange({ preset: "all" }, [mk({})], now);
    const r30 = resolveRange({ preset: "30d" }, [], now);
    expect(r.from.getTime()).toBe(r30.from.getTime());
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
      { model: "gpt-5", sessions: 1, tokens: 500, input: 500, output: 0 },
      { model: "claude-opus-4-7", sessions: 2, tokens: 150, input: 150, output: 0 }
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
    expect(b).toEqual([{ model: "Unknown", sessions: 1, tokens: 30, input: 30, output: 0 }]);
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
    expect(b).toEqual([{ model: "claude-opus-4-7", sessions: 0, tokens: 20, input: 20, output: 0 }]);
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
    const range = resolveRange({ preset: "30d" }, sessions);
    const filtered = filterSessions(sessions, range, "All");
    expect(filtered).toHaveLength(3);
  });
});

describe("overviewKpis", () => {
  const range = {
    from: new Date("2026-05-25T00:00:00"),
    to: new Date("2026-05-29T23:59:59")
  };
  const tk = (total: number) => ({
    input: total,
    output: 0,
    cached: 0,
    reasoning: 0,
    total
  });
  const activeOn = (dates: string[]) =>
    mk({
      started_at: "2026-05-25T10:00:00",
      daily_tokens: dates.map((date) => ({
        date,
        tokens: tk(10),
        messages: 1
      }))
    });

  it("counts active days; a not-yet-active last day counts the streak from the day before", () => {
    // Window 05-25..05-29; active 25, 26, 28.
    const k = overviewKpis([activeOn(["2026-05-25", "2026-05-26", "2026-05-28"])], range);
    expect(k.activeDays).toBe(3);
    expect(k.longestStreak).toBe(2); // 25-26
    expect(k.currentStreak).toBe(1); // 29 inactive → count back from 28
  });

  it("current streak runs through the window's last day when it is active", () => {
    const k = overviewKpis(
      [activeOn(["2026-05-27", "2026-05-28", "2026-05-29"])],
      range
    );
    expect(k.currentStreak).toBe(3);
    expect(k.longestStreak).toBe(3);
  });

  it("current streak is zero when neither the last day nor the day before is active", () => {
    const k = overviewKpis([activeOn(["2026-05-25"])], range);
    expect(k.currentStreak).toBe(0);
    expect(k.longestStreak).toBe(1);
  });

  it("peak hour is the in-window hourly-message argmax; null with no data", () => {
    const s = mk({
      started_at: "2026-05-28T10:00:00",
      daily_tokens: [
        {
          date: "2026-05-28",
          tokens: tk(0),
          messages: 8,
          hours: hoursWith({ 22: 5, 9: 3 })
        }
      ]
    });
    expect(overviewKpis([s], range).peakHour).toBe(22);
    expect(overviewKpis([], range).peakHour).toBeNull();
  });
});

describe("dailyModelTokens", () => {
  const range = {
    from: new Date("2026-05-28T00:00:00"),
    to: new Date("2026-05-29T23:59:59")
  };
  const tk = (total: number) => ({
    input: total,
    output: 0,
    cached: 0,
    reasoning: 0,
    total
  });

  it("splits per-day totals by session model and cross-sums to dailyTokens", () => {
    const sessions = [
      mk({
        model: "claude-opus-4-8",
        started_at: "2026-05-28T10:00:00",
        daily_tokens: [
          { date: "2026-05-28", tokens: tk(100), messages: 1 },
          { date: "2026-05-29", tokens: tk(50), messages: 1 }
        ]
      }),
      mk({
        model: "gpt-5",
        started_at: "2026-05-28T11:00:00",
        daily_tokens: [{ date: "2026-05-28", tokens: tk(300), messages: 1 }]
      })
    ];
    const out = dailyModelTokens(sessions, range);
    expect(out.dates).toEqual(["2026-05-28", "2026-05-29"]);
    // Sorted by window total desc.
    expect(out.models).toEqual(["gpt-5", "claude-opus-4-8"]);
    expect(out.series["gpt-5"]).toEqual([300, 0]);
    expect(out.series["claude-opus-4-8"]).toEqual([100, 50]);
    // LOCKED cross-consistency: per-date model sums equal dailyTokens totals.
    const daily = dailyTokens(sessions, range);
    out.dates.forEach((_, i) => {
      const stackSum = out.models.reduce(
        (sum, m) => sum + (out.series[m]?.[i] ?? 0),
        0
      );
      expect(stackSum).toBe(daily[i].total);
    });
  });

  it("buckets a missing model under Unknown", () => {
    const out = dailyModelTokens(
      [
        mk({
          started_at: "2026-05-28T10:00:00",
          daily_tokens: [{ date: "2026-05-28", tokens: tk(30), messages: 1 }]
        })
      ],
      range
    );
    expect(out.models).toEqual(["Unknown"]);
    expect(out.series["Unknown"]).toEqual([30, 0]);
  });
});

describe("displayModelName", () => {
  it("prettifies the Claude family", () => {
    expect(displayModelName("claude-opus-4-8")).toBe("Opus 4.8");
    expect(displayModelName("claude-fable-5")).toBe("Fable 5");
    expect(displayModelName("claude-sonnet-4-6")).toBe("Sonnet 4.6");
    // Version-prefix form with a trailing DATE segment.
    expect(displayModelName("claude-3-5-haiku-20241022")).toBe("Haiku 3.5");
    // Vendor-prefixed ids.
    expect(displayModelName("anthropic/claude-sonnet-4-6")).toBe("Sonnet 4.6");
  });
  it("passes non-Claude ids and Unknown through unchanged", () => {
    expect(displayModelName("gpt-5")).toBe("gpt-5");
    expect(displayModelName("gemini-2.5-pro")).toBe("gemini-2.5-pro");
    expect(displayModelName("Unknown")).toBe("Unknown");
  });
});

describe("calendarWeeks", () => {
  const day = (date: string, messages: number, total: number) => ({
    date,
    messages,
    total,
    input: 0,
    output: 0,
    cached: 0,
    reasoning: 0
  });

  it("pads the first week to Sunday and chunks by 7", () => {
    // 2026-05-28 is a Thursday (weekday 4).
    const weeks = calendarWeeks([
      day("2026-05-28", 1, 10),
      day("2026-05-29", 0, 0),
      day("2026-05-30", 2, 20)
    ]);
    expect(weeks).toHaveLength(1);
    expect(weeks[0]).toHaveLength(7);
    expect(weeks[0].slice(0, 4)).toEqual([null, null, null, null]);
    expect(weeks[0][4]?.date).toBe("2026-05-28");
    expect(weeks[0][6]?.date).toBe("2026-05-30");
  });

  it("starts a new column when the range crosses into the next week", () => {
    const weeks = calendarWeeks([
      day("2026-05-30", 1, 10), // Saturday
      day("2026-05-31", 2, 20) // Sunday → next week column
    ]);
    expect(weeks).toHaveLength(2);
    expect(weeks[0][6]?.date).toBe("2026-05-30");
    expect(weeks[1][0]?.date).toBe("2026-05-31");
    expect(weeks[1].slice(1)).toEqual([null, null, null, null, null, null]);
  });

  it("returns an empty grid for no days", () => {
    expect(calendarWeeks([])).toEqual([]);
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
