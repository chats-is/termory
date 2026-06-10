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
