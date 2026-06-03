import { describe, expect, it } from "vitest";
import {
  blankProvider,
  isManagedOptionKey,
  isProviderList,
  maskKey,
  newProviderId
} from "./provider-utils";

// `isManagedOptionKey` MUST mirror `override_key_is_managed` in
// `src-tauri/src/providers.rs`. These cases pin the frontend side; if the
// Rust list changes, update both (the two can't auto-detect each other's
// drift, so this test is the guardrail on the TS half).
describe("isManagedOptionKey", () => {
  it("claude — base/auth/model env vars are managed, size routing is NOT", () => {
    expect(isManagedOptionKey("claude", "env.ANTHROPIC_BASE_URL")).toBe(true);
    expect(isManagedOptionKey("claude", "env.ANTHROPIC_AUTH_TOKEN")).toBe(true);
    expect(isManagedOptionKey("claude", "env.ANTHROPIC_API_KEY")).toBe(true);
    expect(isManagedOptionKey("claude", "env.ANTHROPIC_MODEL")).toBe(true);
    // The /model size-routing keys pass through (set via Advanced settings).
    expect(isManagedOptionKey("claude", "env.ANTHROPIC_DEFAULT_SONNET_MODEL")).toBe(
      false
    );
    expect(isManagedOptionKey("claude", "cleanupPeriodDays")).toBe(false);
  });

  it("codex — model_provider / model / model_providers.*", () => {
    expect(isManagedOptionKey("codex", "model_provider")).toBe(true);
    expect(isManagedOptionKey("codex", "model")).toBe(true);
    expect(isManagedOptionKey("codex", "model_providers.termory.base_url")).toBe(true);
    expect(isManagedOptionKey("codex", "model_reasoning_effort")).toBe(false);
  });

  it("gemini — base url / api key / model env vars", () => {
    expect(isManagedOptionKey("gemini", "GOOGLE_GEMINI_BASE_URL")).toBe(true);
    expect(isManagedOptionKey("gemini", "GEMINI_API_KEY")).toBe(true);
    expect(isManagedOptionKey("gemini", "GEMINI_MODEL")).toBe(true);
    expect(isManagedOptionKey("gemini", "GOOGLE_CLOUD_PROJECT")).toBe(false);
  });

  it("opencode — baseURL / apiKey only (keys are relative to `options`)", () => {
    expect(isManagedOptionKey("opencode", "baseURL")).toBe(true);
    expect(isManagedOptionKey("opencode", "apiKey")).toBe(true);
    expect(isManagedOptionKey("opencode", "timeout")).toBe(false);
    expect(isManagedOptionKey("opencode", "headers.X-Token")).toBe(false);
  });

  it("trims the key before matching", () => {
    expect(isManagedOptionKey("opencode", "  apiKey  ")).toBe(true);
    expect(isManagedOptionKey("codex", "  model  ")).toBe(true);
  });
});

describe("blankProvider", () => {
  it("is a custom provider seeded with the per-CLI default base URL", () => {
    expect(blankProvider("claude")).toMatchObject({
      app: "claude",
      kind: "custom",
      name: "",
      baseUrl: "https://api.anthropic.com"
    });
    expect(blankProvider("codex").baseUrl).toBe("https://api.openai.com/v1");
    expect(blankProvider("gemini").baseUrl).toBe(
      "https://generativelanguage.googleapis.com"
    );
    expect(blankProvider("opencode").baseUrl).toBe("https://api.anthropic.com");
  });

  it("generates a fresh id each call", () => {
    expect(blankProvider("codex").id).not.toBe(blankProvider("codex").id);
  });
});

describe("maskKey", () => {
  it("returns empty for an empty key", () => {
    expect(maskKey("")).toBe("");
  });

  it("fully masks keys of 8 chars or fewer", () => {
    expect(maskKey("sk-12345")).toBe("•".repeat(8));
  });

  it("shows the first and last 4 chars of a long key", () => {
    const masked = maskKey("sk-abcdefghijklmnop");
    expect(masked.startsWith("sk-a")).toBe(true);
    expect(masked.endsWith("mnop")).toBe(true);
    expect(masked).toContain("•");
  });
});

describe("isProviderList", () => {
  it("accepts a valid array (including empty)", () => {
    expect(isProviderList([])).toBe(true);
    expect(
      isProviderList([{ id: "a", name: "A", app: "claude", kind: "custom" }])
    ).toBe(true);
  });

  it("rejects non-arrays and malformed entries", () => {
    expect(isProviderList({})).toBe(false);
    expect(isProviderList(null)).toBe(false);
    expect(
      isProviderList([{ id: "a", name: "A", app: "nope", kind: "custom" }])
    ).toBe(false);
    expect(
      isProviderList([{ id: "a", name: "A", app: "claude", kind: "bad" }])
    ).toBe(false);
    expect(isProviderList([{ name: "A", app: "claude", kind: "custom" }])).toBe(
      false
    );
  });
});

describe("newProviderId", () => {
  it("returns a non-empty, unique string", () => {
    const a = newProviderId();
    const b = newProviderId();
    expect(a).toBeTruthy();
    expect(a).not.toBe(b);
  });
});
