import { describe, expect, it } from "vitest";
import { QUOTA_CRIT_PCT, QUOTA_WARN_PCT, quotaLevel } from "./quota-utils";

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
