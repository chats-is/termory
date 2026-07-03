import { describe, expect, it } from "vitest";
import type { SubscriptionQuota } from "@/types";
import {
  QUOTA_CRIT_PCT,
  QUOTA_WARN_PCT,
  mergeQuotaResult,
  quotaLevel
} from "./quota-utils";

describe("quotaLevel", () => {
  // Mirrors src-tauri/src/quota.rs WARN_PCT/CRIT_PCT (asserted there by
  // tray::tests::quota_glyph_thresholds_match_the_app_ring).
  it("thresholds match the Rust tray glyph (75 / 90)", () => {
    expect(QUOTA_WARN_PCT).toBe(75);
    expect(QUOTA_CRIT_PCT).toBe(90);
    expect(quotaLevel(0)).toBe("ok");
    expect(quotaLevel(74.9)).toBe("ok");
    expect(quotaLevel(75)).toBe("warn");
    expect(quotaLevel(89.9)).toBe("warn");
    expect(quotaLevel(90)).toBe("crit");
    expect(quotaLevel(100)).toBe("crit");
  });
});

describe("mergeQuotaResult", () => {
  const q = (over: Partial<SubscriptionQuota>): SubscriptionQuota =>
    ({
      app: "claude",
      credentialStatus: "valid",
      success: true,
      tiers: [],
      queriedAt: 1000,
      ...over
    }) as SubscriptionQuota;
  const goodTiers = [{ name: "five_hour", utilization: 12, resetsAt: "2026-07-03T04:10:00Z" }];

  it("a success replaces the entry outright", () => {
    const prev = q({ tiers: goodTiers });
    const next = q({ tiers: [], queriedAt: 2000 });
    expect(mergeQuotaResult(prev, next)).toBe(next);
  });

  it("a definitive not_found replaces (logout clears the display)", () => {
    const prev = q({ tiers: goodTiers });
    const next = q({ success: false, credentialStatus: "not_found", queriedAt: 2000 });
    expect(mergeQuotaResult(prev, next)).toBe(next);
  });

  it("a transient failure keeps the last good tiers + plan but carries the failure bookkeeping", () => {
    const prev = q({ tiers: goodTiers, plan: "Max" });
    const next = q({ success: false, error: "boom", tiers: [], queriedAt: 2000 });
    const merged = mergeQuotaResult(prev, next);
    expect(merged.tiers).toEqual(goodTiers); // reset times survive
    expect(merged.plan).toBe("Max");
    expect(merged.success).toBe(false); // shorter retry floor
    expect(merged.error).toBe("boom");
    expect(merged.queriedAt).toBe(2000); // cooldown clock advances
  });

  it("repeated failures keep merging the surviving tiers", () => {
    const prev = q({ tiers: goodTiers });
    const fail1 = mergeQuotaResult(prev, q({ success: false, tiers: [], queriedAt: 2000 }));
    const fail2 = mergeQuotaResult(fail1, q({ success: false, tiers: [], queriedAt: 3000 }));
    expect(fail2.tiers).toEqual(goodTiers);
  });

  it("a failure with nothing to preserve passes through", () => {
    expect(mergeQuotaResult(undefined, q({ success: false, tiers: [] })).tiers).toEqual([]);
  });
});
