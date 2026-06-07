import type { MessageKey } from "@/i18n";

// `Intl.DateTimeFormat` construction is expensive (loads locale data
// the first time per option set). Cache one instance per format and
// reuse — formatDate runs hundreds of times per App re-render across
// the session list + detail messages.

/** Optional translator. When omitted, the relative-time formatters fall
 * back to English literals (keeps pure-unit tests simple); when a `t` is
 * passed they render the localized `time.*` strings. */
type Translate = (key: MessageKey, params?: Record<string, string | number>) => string;

const dateFormatter = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit"
});

const shortDateFormatter = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric"
});

const yearDateFormatter = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "numeric"
});

const numberFormatter = new Intl.NumberFormat();

export function formatDate(value?: string | null): string {
  if (!value) return "Unknown time";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return dateFormatter.format(date);
}

// Compact relative timestamp used on list cards: `2h ago`,
// `Yesterday`, `May 23`, `2024`. Drops absolute precision in
// exchange for scannability — older absolute formats stay in
// places that still need them (detail header, etc.).
export function formatRelativeDate(value?: string | null, t?: Translate): string {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  const now = Date.now();
  const diffMs = now - date.getTime();
  const sec = Math.round(diffMs / 1000);
  if (sec < 60) return t ? t("time.justNow") : "just now";
  const min = Math.round(sec / 60);
  if (min < 60) return t ? t("time.minutesAgo", { n: min }) : `${min}m ago`;
  const hr = Math.round(min / 60);
  if (hr < 24) return t ? t("time.hoursAgo", { n: hr }) : `${hr}h ago`;
  // Compare calendar dates for "Yesterday" / "Today" so a 23h-old
  // session at 11pm yesterday doesn't show "23h ago" forever.
  const startOfToday = new Date();
  startOfToday.setHours(0, 0, 0, 0);
  const startOfDate = new Date(date);
  startOfDate.setHours(0, 0, 0, 0);
  const dayDiff = Math.round(
    (startOfToday.getTime() - startOfDate.getTime()) / 86_400_000
  );
  if (dayDiff === 0) return t ? t("time.hoursAgo", { n: hr }) : `${hr}h ago`;
  if (dayDiff === 1) return t ? t("time.yesterday") : "Yesterday";
  if (dayDiff < 7) return t ? t("time.daysAgo", { n: dayDiff }) : `${dayDiff}d ago`;
  if (date.getFullYear() === new Date().getFullYear()) {
    return shortDateFormatter.format(date);
  }
  return yearDateFormatter.format(date);
}

export function formatTimeAgo(timestamp: number, t?: Translate): string {
  const sec = Math.floor((Date.now() - timestamp) / 1000);
  if (sec < 5) return t ? t("time.justNow") : "just now";
  if (sec < 60) return t ? t("time.secondsAgo", { n: sec }) : `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return t ? t("time.minutesAgo", { n: min }) : `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return t ? t("time.hoursAgo", { n: hr }) : `${hr}h ago`;
  const day = Math.floor(hr / 24);
  return t ? t("time.daysAgo", { n: day }) : `${day}d ago`;
}

export function formatFullNumber(value: number): string {
  return numberFormatter.format(value);
}

/**
 * Compact format for dashboard widgets — token-tuned so M is the
 * dominant unit. Pair with `formatFullNumber` in the `title=` attr
 * when the precise value matters on hover.
 *
 *   < 1,000        →  raw integer        (e.g. 500)
 *   1K – <100K     →  K with 0-1 decimal (e.g. 5K, 5.5K, 50K)
 *   100K – <1M     →  M with 2 decimals  (e.g. 0.50M, 0.05M)
 *   1M – <1B       →  M with 1 decimal   (e.g. 4.2M, 800M)
 *   ≥ 1B           →  B with 1 decimal   (e.g. 1.2B, 4B)
 *
 * Whole-number values strip the trailing `.0` so "800.0M" becomes
 * "800M" — Y-axis ticks especially shouldn't carry meaningless
 * decimals.
 */
export function formatCompact(value: number): string {
  if (value < 1_000) return String(value);
  if (value < 100_000) {
    const k = value / 1_000;
    return `${stripTrailingZero(k.toFixed(value < 10_000 ? 1 : 0))}K`;
  }
  if (value < 1_000_000_000) {
    const m = value / 1_000_000;
    return `${stripTrailingZero(m.toFixed(m < 1 ? 2 : 1))}M`;
  }
  return `${stripTrailingZero((value / 1_000_000_000).toFixed(1))}B`;
}

/**
 * Drop a single trailing `.0` from a fixed-decimal string so whole
 * numbers render cleanly: "800.0" → "800", "0.50" → "0.50" (intact).
 * Multi-zero tails (e.g. "0.00") are also normalised.
 */
function stripTrailingZero(s: string): string {
  if (!s.includes(".")) return s;
  return s.replace(/\.0+$/, "");
}
