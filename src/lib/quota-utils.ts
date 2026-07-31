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

/** True for a result that was fetched BEFORE the entry was invalidated,
 * i.e. one describing a login that is no longer live.
 *
 * A quota result is only meaningful for the account that was live when it
 * was fetched, and an account switch invalidates the entry at a known
 * moment (`resetAt`). Anything already in flight at that point — the
 * page's own IPC call, or a backend-initiated QUOTA_CHANGED_EVENT from
 * the tray / credential watcher — still lands afterwards and would
 * otherwise be stored under the NEW account. Comparing against the
 * invalidation timestamp (both are epoch ms; `queried_at` is stamped by
 * the same machine's clock in quota.rs) drops exactly those.
 *
 * A result with no `queriedAt` counts as stale once invalidated: it
 * carries no evidence of being newer, and showing the previous account's
 * usage is the failure this guards against. */
export function quotaResultIsStale(
  result: SubscriptionQuota,
  resetAt: number | undefined
): boolean {
  if (!resetAt) return false;
  return (result.queriedAt ?? 0) < resetAt;
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
 * off the card.
 *
 * The retained data belongs to the account it was fetched for, so this
 * only holds while the login is unchanged: a caller switching accounts
 * must DROP the entry first (ProvidersPage `resetQuota`), otherwise a
 * failed post-switch fetch keeps the previous account's usage on screen
 * under the new account's row. */
export function mergeQuotaResult(
  prev: SubscriptionQuota | undefined,
  next: SubscriptionQuota
): SubscriptionQuota {
  if (next.success || next.credentialStatus === "not_found") return next;
  if (!prev || prev.tiers.length === 0) return next;
  return { ...next, tiers: prev.tiers, plan: prev.plan, extraUsage: prev.extraUsage };
}
