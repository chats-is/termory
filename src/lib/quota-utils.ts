import type { SubscriptionQuota } from "@/types";

// Official-account quota display helpers.
//
// Pressure thresholds (used %) for the quota ring color. Rust mirror:
// `WARN_PCT` / `CRIT_PCT` in src-tauri/src/quota.rs (drives the tray's
// emoji glyph) — keep the two in sync.
export const QUOTA_WARN_PCT = 75;
export const QUOTA_CRIT_PCT = 90;

export type QuotaLevel = "ok" | "warn" | "crit";

/** Pressure level for a used percentage: <75% ok, ≥75% warn, ≥90% crit. */
export function quotaLevel(utilization: number): QuotaLevel {
  if (utilization >= QUOTA_CRIT_PCT) return "crit";
  if (utilization >= QUOTA_WARN_PCT) return "warn";
  return "ok";
}

/** Store rule for a completed quota fetch — the frontend mirror of the
 * tray's "failures keep the last good numbers":
 * - a SUCCESS or a definitive `not_found` (logged out) replaces the
 *   entry outright (both may legitimately clear the display);
 * - any other failure (network error, 401, transient API error) keeps
 *   the previous entry's DISPLAY data — tiers with their reset times,
 *   plan, extra usage — while carrying the failure's bookkeeping
 *   (`success: false` selects the shorter retry floor, `queriedAt`
 *   drives the cooldown clock, `error` feeds the manual toast).
 * Without this, one failed background refresh wiped the reset times
 * off the card. */
export function mergeQuotaResult(
  prev: SubscriptionQuota | undefined,
  next: SubscriptionQuota
): SubscriptionQuota {
  if (next.success || next.credentialStatus === "not_found") return next;
  if (!prev || prev.tiers.length === 0) return next;
  return { ...next, tiers: prev.tiers, plan: prev.plan, extraUsage: prev.extraUsage };
}
