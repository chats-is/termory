import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  formatCompact,
  formatDate,
  formatFullNumber,
  formatRelativeDate,
  formatResetTime,
  formatTimeAgo,
  setFormatLocale
} from "./format";

describe("setFormatLocale", () => {
  afterEach(() => setFormatLocale(undefined)); // reset to OS locale for other tests

  it("formats dates in the app's selected language, not the OS locale", () => {
    const d = "2026-06-10T08:00:00Z"; // mid-June → month stable across TZs
    setFormatLocale("en");
    expect(formatDate(d)).toMatch(/Jun/);
    setFormatLocale("zh-Hans");
    expect(formatDate(d)).toMatch(/月/);
  });

  it("compacts large numbers with 万/亿 in Chinese, K/M/B otherwise", () => {
    setFormatLocale("en");
    expect(formatCompact(21_500_000_000)).toBe("21.5B");
    expect(formatCompact(1_200_000)).toBe("1.2M");
    setFormatLocale("zh-Hans");
    expect(formatCompact(21_500_000_000)).toBe("215亿");
    expect(formatCompact(1_200_000)).toBe("120万");
    expect(formatCompact(5_000)).toBe("5000");
    setFormatLocale("zh-Hant");
    expect(formatCompact(120_000)).toBe("12万");
  });
});

describe("formatDate", () => {
  it("returns 'Unknown time' for null / undefined / empty", () => {
    expect(formatDate(null)).toBe("Unknown time");
    expect(formatDate(undefined)).toBe("Unknown time");
    expect(formatDate("")).toBe("Unknown time");
  });
  it("returns the input verbatim for an unparseable string", () => {
    expect(formatDate("not-a-date")).toBe("not-a-date");
  });
  it("formats a valid ISO string into a locale-aware short label", () => {
    // We don't assert exact characters because Intl output depends on
    // the host locale. Instead assert non-empty + contains a digit.
    const out = formatDate("2026-05-29T10:30:00Z");
    expect(out).toMatch(/\d/);
    expect(out).not.toBe("Unknown time");
  });
});

describe("formatRelativeDate", () => {
  // Pin "now" so the multi-branch logic is deterministic. The fake is
  // applied per-test so each assertion picks its own reference point.
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-05-29T12:00:00Z"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("returns em-dash for null", () => {
    expect(formatRelativeDate(null)).toBe("—");
  });
  it("returns the input for unparseable strings", () => {
    expect(formatRelativeDate("garbage")).toBe("garbage");
  });
  it("returns 'just now' for <60s old", () => {
    expect(formatRelativeDate("2026-05-29T11:59:30Z")).toBe("just now");
  });
  it("returns minutes for 1–59m old", () => {
    expect(formatRelativeDate("2026-05-29T11:55:00Z")).toBe("5m ago");
  });
  it("returns hours when same calendar day", () => {
    expect(formatRelativeDate("2026-05-29T09:00:00Z")).toBe("3h ago");
  });
  it("returns 'Yesterday' for the previous calendar day (>24h diff)", () => {
    // The function returns "Nh ago" while absolute diff < 24h, so to
    // hit the "Yesterday" branch we need both: absolute age ≥ 24h AND
    // calendar dayDiff === 1. Target = 26h before "now".
    expect(formatRelativeDate("2026-05-28T10:00:00Z")).toBe("Yesterday");
  });
  it("returns 'Nd ago' for 2–6 days back", () => {
    expect(formatRelativeDate("2026-05-26T12:00:00Z")).toBe("3d ago");
  });
  it("returns same-year short date when older than a week", () => {
    // Intl format depends on locale; just check it doesn't include "ago"
    // or fall through to year-prefixed format.
    const out = formatRelativeDate("2026-01-05T12:00:00Z");
    expect(out).not.toMatch(/ago/);
    expect(out).not.toMatch(/2026/);
  });
  it("returns year-prefixed date for prior years", () => {
    const out = formatRelativeDate("2024-06-01T12:00:00Z");
    expect(out).toMatch(/2024/);
  });
});

describe("formatTimeAgo", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-05-29T12:00:00Z"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("returns 'just now' for <5s old", () => {
    expect(formatTimeAgo(Date.now() - 1000)).toBe("just now");
  });
  it("returns seconds when <60s old", () => {
    expect(formatTimeAgo(Date.now() - 30_000)).toBe("30s ago");
  });
  it("returns minutes when <60m old", () => {
    expect(formatTimeAgo(Date.now() - 5 * 60_000)).toBe("5m ago");
  });
  it("returns hours when <24h old", () => {
    expect(formatTimeAgo(Date.now() - 3 * 3_600_000)).toBe("3h ago");
  });
  it("returns days otherwise", () => {
    expect(formatTimeAgo(Date.now() - 2 * 86_400_000)).toBe("2d ago");
  });
});

describe("formatResetTime", () => {
  // Mirrors Claude Code's own /usage rules (claude-code
  // utils/format.ts:238 formatResetTime). TZ-independent: local wall
  // times in/out, so the assertions hold in any zone. Locale pinned to
  // en — the OS locale on the dev machine is Chinese.
  beforeEach(() => setFormatLocale("en"));
  afterEach(() => setFormatLocale(undefined));
  it("renders a reset within 24h as compact time + IANA zone", () => {
    const now = new Date("2026-07-02T08:00:00");
    const reset = new Date("2026-07-02T23:50:00");
    // e.g. "11:50pm (Asia/Shanghai)"
    expect(formatResetTime(reset, now)).toBe(
      `11:50pm (${Intl.DateTimeFormat().resolvedOptions().timeZone})`
    );
  });
  it("stays time-only across midnight while within 24h", () => {
    const now = new Date("2026-07-02T23:00:00");
    const reset = new Date("2026-07-03T03:30:00");
    expect(formatResetTime(reset, now)).toMatch(/^3:30am \(.+\)$/);
  });
  it("drops the minute on the exact hour, like the official CLI", () => {
    const now = new Date("2026-07-02T08:00:00");
    const reset = new Date("2026-07-02T23:00:00");
    expect(formatResetTime(reset, now)).toMatch(/^11pm \(.+\)$/);
  });
  it("shows month + day (with comma, no weekday) past 24h", () => {
    const now = new Date("2026-07-02T08:00:00");
    const reset = new Date("2026-07-05T23:09:00");
    // e.g. "Jul 5, 11:09pm (Asia/Shanghai)"
    expect(formatResetTime(reset, now)).toMatch(/^Jul 5, 11:09pm \(.+\)$/);
  });
  it("adds the year when the reset lands in a different year", () => {
    const now = new Date("2026-07-02T08:00:00");
    const reset = new Date("2027-01-01T10:30:00");
    expect(formatResetTime(reset, now)).toContain("2027");
  });
  it("omits the zone suffix when withZone is false (compact inline form)", () => {
    const now = new Date("2026-07-02T08:00:00");
    const reset = new Date("2026-07-02T23:50:00");
    expect(formatResetTime(reset, now, false)).toBe("11:50pm");
  });
  it("rounds to the nearest minute so API jitter can't flip the display", () => {
    const now = new Date("2026-07-02T08:00:00");
    // The same minute-aligned boundary reported from both sides of the
    // minute (measured server jitter: 23:09:59.733 vs 23:10:00.379).
    const early = new Date("2026-07-02T23:09:59.733");
    const late = new Date("2026-07-02T23:10:00.379");
    expect(formatResetTime(early, now)).toBe(formatResetTime(late, now));
    expect(formatResetTime(early, now)).toMatch(/^11:10pm \(.+\)$/);
  });
});

describe("formatFullNumber", () => {
  it("returns locale-grouped decimal numbers", () => {
    // The exact thousands separator depends on locale ("," in en-US,
    // "." in de-DE). We just check that the number renders and that
    // four-digit values include a separator of some kind.
    expect(formatFullNumber(0)).toBe("0");
    expect(formatFullNumber(42)).toBe("42");
    const big = formatFullNumber(1234);
    expect(big.replace(/[^\d]/g, "")).toBe("1234");
    expect(big).not.toBe("1234");
  });
});
