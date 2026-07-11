import { describe, expect, it } from "vitest";
import {
  appProtocols,
  blankProvider,
  codexVersionText,
  isClaudeSafeModelId,
  isManagedOptionKey,
  isProviderList,
  isGatewayList,
  maskKey,
  newProviderId,
  npmForProtocol,
  protocolForBinding,
  protocolForNpm,
  providerFromBinding,
  resolveActiveProviderId,
  gatewayBaseForProtocol
} from "./provider-utils";
import type {
  ActiveState,
  CodexInstalls,
  Gateway,
  GatewayCapabilities
} from "../types";
import type { MessageKey } from "@/i18n";
import { en } from "@/i18n/locales/en";

const tEn = (key: MessageKey) => en[key];

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

  it("claude-desktop — the 3P profile keys owned by dedicated fields are managed", () => {
    // Filled from Base URL / API key / models — options must not clobber them.
    expect(isManagedOptionKey("claude-desktop", "inferenceGatewayBaseUrl")).toBe(
      true
    );
    expect(isManagedOptionKey("claude-desktop", "inferenceGatewayApiKey")).toBe(
      true
    );
    expect(isManagedOptionKey("claude-desktop", "inferenceModels")).toBe(true);
    // Any other inference* key passes through (the escape hatch), e.g. headers.
    expect(
      isManagedOptionKey("claude-desktop", "inferenceGatewayHeaders.X-Foo")
    ).toBe(false);
  });
});

describe("isClaudeSafeModelId", () => {
  it("accepts claude-* / anthropic/claude-* role names (and [1m])", () => {
    expect(isClaudeSafeModelId("claude-sonnet-4-6")).toBe(true);
    expect(isClaudeSafeModelId("claude-opus-4-8")).toBe(true);
    expect(isClaudeSafeModelId("claude-haiku-4-5")).toBe(true);
    expect(isClaudeSafeModelId("claude-fable-5")).toBe(true);
    expect(isClaudeSafeModelId("anthropic/claude-sonnet-4.6")).toBe(true);
    expect(isClaudeSafeModelId("claude-sonnet-4-6[1m]")).toBe(true);
    expect(isClaudeSafeModelId("  CLAUDE-OPUS-4-8  ")).toBe(true);
  });
  it("blank is treated as ok (dropped on save, not flagged)", () => {
    expect(isClaudeSafeModelId("")).toBe(true);
    expect(isClaudeSafeModelId("   ")).toBe(true);
  });
  it("rejects non-Claude names and degenerate role-only ids", () => {
    expect(isClaudeSafeModelId("gpt-4")).toBe(false);
    expect(isClaudeSafeModelId("glm-4.6")).toBe(false);
    expect(isClaudeSafeModelId("claude-3-5-sonnet-20241022")).toBe(false);
    expect(isClaudeSafeModelId("claude-sonnet-")).toBe(false); // role but nothing after
    expect(isClaudeSafeModelId("claude-")).toBe(false);
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
    // Claude Desktop is Anthropic-format, same default endpoint as Claude Code.
    expect(blankProvider("claude-desktop").baseUrl).toBe(
      "https://api.anthropic.com"
    );
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

  it("accepts claude-desktop providers (else they wipe the whole list on load)", () => {
    expect(
      isProviderList([
        { id: "a", name: "A", app: "claude-desktop", kind: "custom" }
      ])
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

// ── Gateway helpers ─────────────────────────────────────────

function caps(partial: Partial<GatewayCapabilities>): GatewayCapabilities {
  return {
    openaiCompatible: false,
    openai: false,
    anthropic: false,
    gemini: false,
    models: [],
    ...partial
  };
}

describe("gatewayBaseForProtocol", () => {
  it("openai gets a trailing /v1 (idempotent, any pasted suffix)", () => {
    expect(gatewayBaseForProtocol("https://r.x", "openai")).toBe("https://r.x/v1");
    expect(gatewayBaseForProtocol("https://r.x/", "openai")).toBe("https://r.x/v1");
    expect(gatewayBaseForProtocol("https://r.x/v1", "openai")).toBe("https://r.x/v1");
    expect(gatewayBaseForProtocol("https://r.x/v1beta", "openai")).toBe(
      "https://r.x/v1"
    );
    expect(gatewayBaseForProtocol("https://r.x", "openai-compatible")).toBe(
      "https://r.x/v1"
    );
  });
  it("anthropic → bare root regardless of pasted suffix", () => {
    expect(gatewayBaseForProtocol("https://r.x/v1", "anthropic")).toBe("https://r.x");
    expect(gatewayBaseForProtocol("https://r.x", "anthropic")).toBe("https://r.x");
  });
  it("gemini → bare root, stripping /v1beta OR /v1", () => {
    expect(gatewayBaseForProtocol("https://r.x/v1beta", "gemini")).toBe("https://r.x");
    // The bug fix: a base pasted with the OpenAI-style /v1 must still
    // reduce to the root so Gemini gets /v1beta, not /v1/v1beta.
    expect(gatewayBaseForProtocol("https://r.x/v1", "gemini")).toBe("https://r.x");
    expect(gatewayBaseForProtocol("https://r.x", "gemini")).toBe("https://r.x");
  });
  it("preserves a non-version sub-path", () => {
    expect(gatewayBaseForProtocol("https://r.x/gw/v1", "gemini")).toBe(
      "https://r.x/gw"
    );
    expect(gatewayBaseForProtocol("https://r.x/gw", "openai")).toBe(
      "https://r.x/gw/v1"
    );
  });
});

describe("appProtocols", () => {
  it("maps capabilities to bindable CLIs (Codex needs Responses)", () => {
    const p = appProtocols(
      caps({ openaiCompatible: true, openai: false, anthropic: true })
    );
    expect(p.claude).toEqual(["anthropic"]);
    expect(p.codex).toEqual([]); // chat ok but no Responses route
    expect(p.gemini).toEqual([]);
    // OpenCode is flexible — every supported mode (anthropic + chat here).
    expect(p.opencode).toEqual(["anthropic", "openai-compatible"]);
  });
  it("Codex bindable only when the Responses (openai) route exists", () => {
    const p = appProtocols(caps({ openai: true, openaiCompatible: true }));
    expect(p.codex).toEqual(["openai"]);
    // OpenAI Responses is preferred over openai-compatible (default).
    expect(p.opencode).toEqual(["openai", "openai-compatible"]);
  });
  it("undefined capabilities → nothing bindable", () => {
    const p = appProtocols(undefined);
    expect(p.claude).toEqual([]);
    expect(p.opencode).toEqual([]);
  });
  it("claude-desktop binds an Anthropic-capable gateway (like Claude Code)", () => {
    expect(appProtocols(caps({ anthropic: true }))["claude-desktop"]).toEqual([
      "anthropic"
    ]);
    expect(appProtocols(caps({ openai: true }))["claude-desktop"]).toEqual([]);
  });
});

describe("providerFromBinding", () => {
  const gateway: Gateway = {
    kind: "gateway",
    id: "rel1",
    name: "My Gateway",
    baseUrl: "https://r.x",
    apiKey: "sk-1",
    bindings: []
  };
  it("uses the binding's own id; derives the protocol (Claude → anthropic base)", () => {
    const claude = providerFromBinding(gateway, {
      id: "b-claude",
      app: "claude",
      model: "claude-x"
    });
    expect(claude.id).toBe("b-claude");
    expect(claude.app).toBe("claude");
    expect(claude.kind).toBe("custom");
    expect(claude.baseUrl).toBe("https://r.x"); // anthropic → bare host
    expect(claude.apiKey).toBe("sk-1");
    expect(claude.model).toBe("claude-x");
    expect(claude.npm).toBeUndefined();
  });
  it("Codex binding gets a /v1 base (derived openai protocol)", () => {
    const codex = providerFromBinding(gateway, { id: "b-codex", app: "codex" });
    expect(codex.baseUrl).toBe("https://r.x/v1");
  });
  it("Claude Desktop binding: anthropic base, no npm, but carries its models", () => {
    const cd = providerFromBinding(gateway, {
      id: "b-cd",
      app: "claude-desktop",
      models: [{ id: "claude-sonnet-4-6[1m]", name: "Sonnet" }]
    });
    expect(cd.app).toBe("claude-desktop");
    expect(cd.baseUrl).toBe("https://r.x"); // anthropic → bare host
    expect(cd.apiKey).toBe("sk-1");
    expect(cd.npm).toBeUndefined();
    // The models list must reach the synth provider (→ inferenceModels).
    expect(cd.models).toEqual([{ id: "claude-sonnet-4-6[1m]", name: "Sonnet" }]);
  });
  it("OpenCode protocol is derived from its npm package", () => {
    const oc = providerFromBinding(gateway, {
      id: "b-oc",
      app: "opencode",
      npm: "@ai-sdk/anthropic"
    });
    expect(oc.npm).toBe("@ai-sdk/anthropic");
    expect(oc.baseUrl).toBe("https://r.x"); // anthropic → bare host
    // No npm → defaults to @ai-sdk/openai-compatible (Chat Completions).
    const oc2 = providerFromBinding(gateway, { id: "b-oc2", app: "opencode" });
    expect(oc2.npm).toBe(npmForProtocol("openai-compatible"));
    expect(oc2.npm).toBe("@ai-sdk/openai-compatible");
    expect(oc2.baseUrl).toBe("https://r.x/v1");
  });
  it("passes advanced options through to the synthesized provider", () => {
    const p = providerFromBinding(gateway, {
      id: "b1",
      app: "claude",
      options: [{ key: "env.ANTHROPIC_DEFAULT_OPUS_MODEL", value: "claude-opus" }]
    });
    expect(p.options).toEqual([
      { key: "env.ANTHROPIC_DEFAULT_OPUS_MODEL", value: "claude-opus" }
    ]);
  });
  it("passes OpenCode extra models through; non-OpenCode bindings don't", () => {
    const oc = providerFromBinding(gateway, {
      id: "b-oc",
      app: "opencode",
      models: [{ id: "m1", name: "M1" }]
    });
    expect(oc.models).toEqual([{ id: "m1", name: "M1" }]);
    const claude = providerFromBinding(gateway, {
      id: "b-claude",
      app: "claude",
      models: [{ id: "m1", name: "M1" }]
    });
    expect(claude.models).toBeUndefined();
  });
});

describe("protocolForBinding", () => {
  it("fixes Claude/Codex/Gemini; derives OpenCode from npm", () => {
    expect(protocolForBinding({ app: "claude" })).toBe("anthropic");
    expect(protocolForBinding({ app: "codex" })).toBe("openai");
    expect(protocolForBinding({ app: "gemini" })).toBe("gemini");
    expect(protocolForBinding({ app: "opencode", npm: "@ai-sdk/anthropic" })).toBe(
      "anthropic"
    );
    expect(protocolForBinding({ app: "opencode", npm: "@ai-sdk/google" })).toBe(
      "gemini"
    );
    // @ai-sdk/openai → Responses; @ai-sdk/openai-compatible → Chat (matched
    // first since its name also contains "openai"); no npm → compatible.
    expect(protocolForBinding({ app: "opencode", npm: "@ai-sdk/openai" })).toBe(
      "openai"
    );
    expect(
      protocolForBinding({ app: "opencode", npm: "@ai-sdk/openai-compatible" })
    ).toBe("openai-compatible");
    expect(protocolForBinding({ app: "opencode" })).toBe("openai-compatible");
  });
});

describe("isGatewayList", () => {
  it("accepts gateway-shaped entries, rejects junk", () => {
    const g = (extra: object) => ({ kind: "gateway", id: "a", name: "A", ...extra });
    expect(isGatewayList([g({ bindings: [] })])).toBe(true);
    expect(isGatewayList([g({})])).toBe(true); // bindings optional on read
    expect(isGatewayList([g({ bindings: [{ id: "b1", app: "claude" }] })])).toBe(
      true
    );
    // Missing the `kind: "gateway"` discriminant → not a gateway entry.
    expect(isGatewayList([{ id: "a", name: "A", bindings: [] }])).toBe(false);
    expect(isGatewayList([g({ bindings: "no" })])).toBe(false);
    // Pre-refactor binding shape (no `id`) is rejected → stale data dropped.
    expect(
      isGatewayList([g({ bindings: [{ app: "claude", protocol: "anthropic" }] })])
    ).toBe(false);
    expect(isGatewayList([{ kind: "gateway", name: "A" }])).toBe(false);
    expect(isGatewayList("nope")).toBe(false);
  });
});

describe("protocolForNpm", () => {
  it("maps each AI SDK package; matches openai-compatible BEFORE openai", () => {
    expect(protocolForNpm("@ai-sdk/openai-compatible")).toBe("openai-compatible");
    expect(protocolForNpm("@ai-sdk/openai")).toBe("openai"); // Responses
    expect(protocolForNpm("@ai-sdk/anthropic")).toBe("anthropic");
    expect(protocolForNpm("@ai-sdk/amazon-bedrock")).toBe("anthropic");
    expect(protocolForNpm("@ai-sdk/google")).toBe("gemini");
    // Unknown / azure / empty → default to the OpenAI-compatible package.
    expect(protocolForNpm("@ai-sdk/azure")).toBe("openai-compatible");
    expect(protocolForNpm("")).toBe("openai-compatible");
  });
});

describe("resolveActiveProviderId", () => {
  const candidates = [
    { id: "a", baseUrl: "https://x", apiKey: "sk-aaaaaaaaaaaa" },
    { id: "b", baseUrl: "https://y", apiKey: "sk-bbbbbbbbbbbb" }
  ];
  const stateFor = (
    matched: string | null,
    snapBase: string,
    snapKey: string
  ): ActiveState => ({
    app: "opencode",
    kind: "custom",
    matchedProviderId: matched,
    liveSnapshot: { baseUrl: snapBase, apiKeyMasked: maskKey(snapKey) },
    livePath: "/x"
  });

  it("returns null without a state", () => {
    expect(resolveActiveProviderId(null, "a", candidates)).toBeNull();
    expect(resolveActiveProviderId(undefined, "a", candidates)).toBeNull();
  });

  it("honors the marker when its creds still match the live snapshot", () => {
    // live config = a's creds; backend matched b; marker points at a.
    const state = stateFor("b", "https://x", "sk-aaaaaaaaaaaa");
    expect(resolveActiveProviderId(state, "a", candidates)).toBe("a");
  });

  it("ignores a stale marker (creds no longer match) → matchedProviderId", () => {
    // live config = b's creds; marker still points at a → fall back to b.
    const state = stateFor("b", "https://y", "sk-bbbbbbbbbbbb");
    expect(resolveActiveProviderId(state, "a", candidates)).toBe("b");
  });

  it("falls back to matchedProviderId with no marker", () => {
    const state = stateFor("b", "https://y", "sk-bbbbbbbbbbbb");
    expect(resolveActiveProviderId(state, undefined, candidates)).toBe("b");
  });

  it("falls back when the marker id isn't among the candidates", () => {
    const state = stateFor("b", "https://x", "sk-aaaaaaaaaaaa");
    expect(resolveActiveProviderId(state, "zzz", candidates)).toBe("b");
  });
});

describe("codexVersionText", () => {
  const installs = (
    cli: boolean,
    app: boolean,
    appVersion?: string | null
  ): CodexInstalls => ({ cli, app, appVersion });

  it("shows both forms labeled when CLI and app are installed", () => {
    expect(
      codexVersionText("0.142.5", installs(true, true, "26.707.31428"), tEn)
    ).toBe("v0.142.5 (CLI) · v26.707.31428 (App)");
  });

  it("shows only the installed form", () => {
    expect(codexVersionText("0.142.5", installs(true, false), tEn)).toBe(
      "v0.142.5 (CLI)"
    );
    expect(
      codexVersionText(null, installs(false, true, "26.707.31428"), tEn)
    ).toBe("v26.707.31428 (App)");
  });

  it("dashes a form whose version probe failed", () => {
    expect(codexVersionText(null, installs(true, false), tEn)).toBe("— (CLI)");
    expect(codexVersionText(null, installs(false, true, null), tEn)).toBe(
      "— (App)"
    );
  });

  it("returns null when neither is installed", () => {
    expect(codexVersionText(null, installs(false, false), tEn)).toBeNull();
  });

  it("falls back to the plain CLI form before detection resolves", () => {
    expect(codexVersionText("0.142.5", null, tEn)).toBe("v0.142.5");
    expect(codexVersionText(null, null, tEn)).toBeNull();
  });
});
