import type { MessageKey } from "@/i18n";

// `Intl.DateTimeFormat` construction is expensive (loads locale data
// the first time per option set). Cache one instance per format and
// reuse — formatDate runs hundreds of times per App re-render across
// the session list + detail messages.

/** Optional translator. When omitted, the relative-time formatters fall
 * back to English literals (keeps pure-unit tests simple); when a `t` is
 * passed they render the localized `time.*` strings. */
type Translate = (key: MessageKey, params?: Record<string, string | number>) => string;

function makeDateTimeFormatter(locale?: string) {
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit"
  });
}
function makeShortDateFormatter(locale?: string) {
  return new Intl.DateTimeFormat(locale, { month: "short", day: "numeric" });
}
function makeYearDateFormatter(locale?: string) {
  return new Intl.DateTimeFormat(locale, {
    year: "numeric",
    month: "short",
    day: "numeric"
  });
}

// The app's formatting locale (drives BOTH dates and `formatCompact` numbers).
// `undefined` = the OS locale; the i18n provider calls `setFormatLocale` so
// formatting follows the app's selected language (English was otherwise showing
// dates/units in the OS's Chinese format).
let currentLocale: string | undefined;
let dateFormatter = makeDateTimeFormatter(currentLocale);
let shortDateFormatter = makeShortDateFormatter(currentLocale);
let yearDateFormatter = makeYearDateFormatter(currentLocale);

/** Set the app formatting locale (a BCP-47 tag — "en" / "zh-Hans" / "zh-Hant").
 * Rebuilds the date formatters only on change (Intl construction is expensive)
 * and is read by `formatCompact` for 万/亿 vs K/M/B. */
export function setFormatLocale(locale: string | undefined): void {
  if (locale === currentLocale) return;
  currentLocale = locale;
  dateFormatter = makeDateTimeFormatter(locale);
  shortDateFormatter = makeShortDateFormatter(locale);
  yearDateFormatter = makeYearDateFormatter(locale);
}

/** Quota reset moment, matching Claude Code's own /usage formatting
 * (claude-code `utils/format.ts:238` `formatResetTime`) rule for rule:
 * within 24 hours → time only (`11:09pm`; the minute is dropped on the
 * exact hour — `11pm`, not `11:00pm`); more than 24 hours out → date +
 * time (`Jul 5, 11:09pm`, plus the year when it differs from the
 * current one). Compact lowercase am/pm, IANA zone name appended.
 * Formatting follows the app locale — en output is verbatim what the
 * official en-US CLI prints; zh locales keep their native form. */
export function formatResetTime(
  rawDate: Date,
  now: Date = new Date(),
  withZone = true
): string {
  // Round to the NEAREST minute first (one deliberate deviation from the
  // official CLI, which truncates). The API recomputes resets_at per
  // request with sub-second jitter around a minute-aligned boundary
  // (measured 70s apart: 20:10:00.379 then 20:09:59.733 for the same
  // window), so truncation flips the displayed minute between requests
  // — rounding pins it to the true boundary instead.
  const date = new Date(Math.round(rawDate.getTime() / 60_000) * 60_000);
  const minute: Intl.DateTimeFormatOptions["minute"] =
    date.getMinutes() === 0 ? undefined : "2-digit";
  const moreThanADayOut = date.getTime() - now.getTime() > 24 * 3_600_000;
  const options: Intl.DateTimeFormatOptions = moreThanADayOut
    ? {
        year: date.getFullYear() === now.getFullYear() ? undefined : "numeric",
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute
      }
    : { hour: "numeric", minute };
  // Options vary per call (minute/year presence), so no cached formatter —
  // fine here: this renders a handful of quota tiers, not the hot list path.
  const base = new Intl.DateTimeFormat(currentLocale, options).format(date);
  const compact = base.replace(/\s?(AM|PM)\b/, (m) => m.trim().toLowerCase());
  if (!withZone) return compact; // compact inline form; the zone shows on hover
  const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return tz ? `${compact} (${tz})` : compact;
}

/** Human countdown until a future moment: `4 hr 42 min` / `42 min` /
 * `2 days 3 hr` (localized units; the wrapping copy adds "in …"/"剩 …").
 * Returns null when the moment is already past (a stale reset boundary) so
 * callers can fall back to the absolute time. Minutes are shown only under a
 * day so long horizons stay compact; a sub-minute remainder floors up to
 * `1 min` rather than `0 min`. `t` is optional — English units without it,
 * so the pure unit tests need no i18n provider. */
export function formatCountdown(
  date: Date,
  now: Date = new Date(),
  t?: Translate
): string | null {
  const diffMs = date.getTime() - now.getTime();
  if (diffMs <= 0) return null;
  let totalMin = Math.round(diffMs / 60_000);
  const day = Math.floor(totalMin / 1440);
  totalMin -= day * 1440;
  const hr = Math.floor(totalMin / 60);
  const min = totalMin - hr * 60;
  const seg: string[] = [];
  if (day > 0) {
    const key = day === 1 ? "time.durDayOne" : "time.durDayOther";
    seg.push(t ? t(key, { n: day }) : `${day} ${day === 1 ? "day" : "days"}`);
  }
  if (hr > 0) seg.push(t ? t("time.durHr", { n: hr }) : `${hr} hr`);
  // Minutes only when under a day, so a multi-day horizon reads "2 days 3 hr".
  if (min > 0 && day === 0) seg.push(t ? t("time.durMin", { n: min }) : `${min} min`);
  if (seg.length === 0) {
    const m = Math.max(1, min);
    seg.push(t ? t("time.durMin", { n: m }) : `${m} min`);
  }
  return seg.join(" ");
}

/** The app's formatting locale, for ad-hoc `toLocale*` calls outside this module
 * (FreshnessFooter tooltip, stats hover labels) so they also follow the app
 * language instead of the OS locale. `undefined` = OS locale. */
export function getFormatLocale(): string | undefined {
  return currentLocale;
}

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

/** Format a currency amount for the usage-credits display. `value` is in
 * MINOR units when `decimalPlaces` is set (Claude sends cents +
 * `decimalPlaces: 2`), else already a major-unit amount (grok). Follows
 * the app locale; falls back to a plain `{symbol}{amount}` string if the
 * currency code is unknown to Intl. */
export function formatCurrency(
  value: number,
  currency?: string,
  decimalPlaces?: number
): string {
  const amount = decimalPlaces ? value / 10 ** decimalPlaces : value;
  const code = currency || "USD";
  try {
    return new Intl.NumberFormat(currentLocale, {
      style: "currency",
      currency: code
    }).format(amount);
  } catch {
    return `${amount.toFixed(2)} ${code}`;
  }
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
  // Chinese reads large numbers grouped by 万 (10^4) / 亿 (10^8), not K/M/B —
  // follow the app language (set via `setFormatLocale`).
  if (currentLocale?.startsWith("zh")) return formatCompactZh(value);
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

/** Compact format with Chinese magnitude units: `< 1万` raw, then `万` (10^4),
 * then `亿` (10^8). e.g. 5000 → "5000", 1.2M → "120万", 21.5B → "215亿". */
function formatCompactZh(value: number): string {
  if (value < 10_000) return String(value);
  if (value < 100_000_000) {
    return `${stripTrailingZero((value / 10_000).toFixed(1))}万`;
  }
  return `${stripTrailingZero((value / 100_000_000).toFixed(1))}亿`;
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
