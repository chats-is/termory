import { describe, expect, it } from "vitest";
import {
  appProtocols,
  orderSources,
  visibleSources,
  blankProvider,
  codexVersionSegments,
  hasUpdate,
  isClaudeSafeModelId,
  isManagedOptionKey,
  isSourceEnabled,
  isProviderList,
  isGatewayList,
  maskKey,
  newProviderId,
  npmForProtocol,
  protocolForBinding,
  protocolForNpm,
  providerFromBinding,
  resolveActiveProviderId,
  upgradeBadgeState,
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

  it("grok — models.default + any entry's owned fields are managed", () => {
    expect(isManagedOptionKey("grok", "models.default")).toBe(true);
    expect(isManagedOptionKey("grok", "model.grok-4.5.api_key")).toBe(true);
    expect(isManagedOptionKey("grok", "model.grok-4.5.base_url")).toBe(true);
    expect(isManagedOptionKey("grok", "model.grok-4.5.description")).toBe(true);
    // api_backend is owned by the dedicated dropdown, so it's managed too.
    expect(isManagedOptionKey("grok", "model.grok-4.5.api_backend")).toBe(true);
    // Other per-entry keys are legit Advanced settings
    // (docs.x.ai/build/settings/reference: context_window, extra_headers…).
    expect(isManagedOptionKey("grok", "model.grok-4.5.context_window")).toBe(false);
    // The rule is dynamic over ANY entry key (the key IS the model id),
    // so owned fields are managed regardless of which entry they're on.
    expect(isManagedOptionKey("grok", "model.my-own.api_key")).toBe(true);
    expect(isManagedOptionKey("grok", "ui.compact_mode")).toBe(false);
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
    expect(blankProvider("grok").baseUrl).toBe("https://api.x.ai/v1");
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
  it("appends the detected Anthropic sub-path, and only for Anthropic", () => {
    // DeepSeek's layout: OpenAI at the root, Anthropic under /anthropic.
    const sub = "/anthropic";
    expect(gatewayBaseForProtocol("https://api.deepseek.com", "anthropic", sub)).toBe(
      "https://api.deepseek.com/anthropic"
    );
    expect(gatewayBaseForProtocol("https://api.deepseek.com", "openai", sub)).toBe(
      "https://api.deepseek.com/v1"
    );
    expect(gatewayBaseForProtocol("https://api.deepseek.com", "gemini", sub)).toBe(
      "https://api.deepseek.com"
    );
    // A root that already IS the vendor's Anthropic URL must not double it.
    expect(
      gatewayBaseForProtocol("https://api.deepseek.com/anthropic", "anthropic", sub)
    ).toBe("https://api.deepseek.com/anthropic");
    // The version strip still runs underneath the prefix.
    expect(gatewayBaseForProtocol("https://r.x/v1", "anthropic", sub)).toBe(
      "https://r.x/anthropic"
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
  it("carries the gateway's detected Anthropic sub-path into Claude bindings", () => {
    // DeepSeek: one gateway root, Anthropic mounted under /anthropic.
    const deepseek: Gateway = {
      ...gateway,
      baseUrl: "https://api.deepseek.com",
      capabilities: caps({ openai: true, anthropic: true, anthropicPath: "/anthropic" })
    };
    expect(providerFromBinding(deepseek, { id: "b1", app: "claude" }).baseUrl).toBe(
      "https://api.deepseek.com/anthropic"
    );
    expect(
      providerFromBinding(deepseek, { id: "b2", app: "claude-desktop" }).baseUrl
    ).toBe("https://api.deepseek.com/anthropic");
    // Codex keeps the root it was probed at.
    expect(providerFromBinding(deepseek, { id: "b3", app: "codex" }).baseUrl).toBe(
      "https://api.deepseek.com/v1"
    );
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

describe("codexVersionSegments", () => {
  const installs = (
    cli: boolean,
    app: boolean,
    appVersion?: string | null
  ): CodexInstalls => ({ cli, app, appVersion });

  it("shows both forms labeled when CLI and app are installed", () => {
    expect(
      codexVersionSegments("0.142.5", installs(true, true, "26.707.31428"), tEn)
    ).toEqual([
      { text: "v0.142.5", label: "CLI", latest: undefined },
      { text: "v26.707.31428", label: "App", latest: null }
    ]);
  });

  it("shows only the installed form", () => {
    expect(codexVersionSegments("0.142.5", installs(true, false), tEn)).toEqual([
      { text: "v0.142.5", label: "CLI", latest: undefined }
    ]);
    expect(
      codexVersionSegments(null, installs(false, true, "26.707.31428"), tEn)
    ).toEqual([{ text: "v26.707.31428", label: "App", latest: null }]);
  });

  it("dashes a form whose version probe failed", () => {
    expect(codexVersionSegments(null, installs(true, false), tEn)).toEqual([
      { text: "—", label: "CLI", latest: undefined }
    ]);
    expect(codexVersionSegments(null, installs(false, true, null), tEn)).toEqual([
      { text: "—", label: "App", latest: null }
    ]);
  });

  it("returns nothing when neither is installed", () => {
    expect(codexVersionSegments(null, installs(false, false), tEn)).toEqual([]);
  });

  it("falls back to the plain CLI form before detection resolves", () => {
    expect(codexVersionSegments("0.142.5", null, tEn)).toEqual([
      { text: "v0.142.5", latest: undefined }
    ]);
    expect(codexVersionSegments(null, null, tEn)).toEqual([]);
  });

  it("keeps the CLI and app updates on their own segments", () => {
    // Separately versioned products: npm's @openai/codex publishes the
    // CLI, the Sparkle appcast the desktop app. Crossing them would
    // compare 0.142.5 against 26.721.30844.
    const segments = codexVersionSegments(
      "0.142.5",
      installs(true, true, "26.707.31428"),
      tEn,
      "0.143.0",
      "26.721.30844"
    );
    expect(segments[0]).toEqual({
      text: "v0.142.5",
      label: "CLI",
      latest: "0.143.0"
    });
    expect(segments[1]).toEqual({
      text: "v26.707.31428",
      label: "App",
      latest: "26.721.30844"
    });
  });

  it("shows no app badge when the app is already current", () => {
    const segments = codexVersionSegments(
      "0.142.5",
      installs(true, true, "26.721.30844"),
      tEn,
      null,
      "26.721.30844"
    );
    expect(segments[1].latest).toBeNull();
  });

  it("shows no app badge when the installed app version is unknown", () => {
    // No baseline to be behind of — the appcast's newest entry would
    // otherwise always look like an available update.
    const segments = codexVersionSegments(
      "0.142.5",
      installs(false, true, null),
      tEn,
      null,
      "26.721.30844"
    );
    expect(segments[0]).toEqual({ text: "—", label: "App", latest: null });
  });
});

describe("hasUpdate", () => {
  it("is true when latest is a higher release", () => {
    expect(hasUpdate("0.142.5", "0.144.6")).toBe(true);
    expect(hasUpdate("2.1.100", "2.1.216")).toBe(true);
    expect(hasUpdate("1.9.0", "2.0.0")).toBe(true);
  });

  it("is false when installed is equal or newer", () => {
    expect(hasUpdate("2.1.216", "2.1.216")).toBe(false);
    expect(hasUpdate("2.1.216", "2.1.100")).toBe(false);
    expect(hasUpdate("2.0.0", "1.9.9")).toBe(false);
  });

  it("treats an installed prerelease at the same core as behind the stable latest", () => {
    expect(hasUpdate("0.2.0-alpha.1", "0.2.0")).toBe(true);
    // But a stable install is not behind an equal-core prerelease.
    expect(hasUpdate("0.2.0", "0.2.0-alpha.1")).toBe(false);
  });

  it("zero-pads mismatched segment counts", () => {
    expect(hasUpdate("2.1", "2.1.1")).toBe(true);
    expect(hasUpdate("2.1.0", "2.1")).toBe(false);
  });

  it("is false when either side is missing or unparseable", () => {
    expect(hasUpdate(null, "2.1.216")).toBe(false);
    expect(hasUpdate("2.1.216", null)).toBe(false);
    expect(hasUpdate("2.1.216", "")).toBe(false);
    expect(hasUpdate("latest", "2.1.216")).toBe(false);
  });
});

describe("isSourceEnabled", () => {
  it("treats an absent key (or absent map) as ENABLED — only explicit false disables", () => {
    expect(isSourceEnabled(undefined, "codex")).toBe(true);
    expect(isSourceEnabled({}, "codex")).toBe(true);
    expect(isSourceEnabled({ codex: true }, "codex")).toBe(true);
    expect(isSourceEnabled({ codex: false }, "codex")).toBe(false);
    // Other apps' entries don't leak.
    expect(isSourceEnabled({ codex: false }, "claude")).toBe(true);
  });

  it("gemini is OFF by default (individual support stopped) — explicit true re-enables", () => {
    // MIRROR of DEFAULT_OFF_KEYS in src-tauri/src/config.rs.
    expect(isSourceEnabled(undefined, "gemini")).toBe(false);
    expect(isSourceEnabled({}, "gemini")).toBe(false);
    expect(isSourceEnabled({ gemini: true }, "gemini")).toBe(true);
    expect(isSourceEnabled({ gemini: false }, "gemini")).toBe(false);
  });
});

describe("orderSources", () => {
  it("returns the default order for absent/empty saved order", () => {
    expect(orderSources(undefined)).toEqual([
      "claude",
      "claude-desktop",
      "codex",
      "gemini",
      "opencode",
      "grok"
    ]);
    expect(orderSources([])).toEqual(orderSources(undefined));
  });

  it("puts saved entries first and appends unknown-to-the-save tools", () => {
    expect(orderSources(["grok", "codex"])).toEqual([
      "grok",
      "codex",
      "claude",
      "claude-desktop",
      "gemini",
      "opencode"
    ]);
  });

  it("drops duplicates and ids that are no longer tools", () => {
    expect(
      orderSources(["codex", "codex", "nope" as never, "claude"])[0]
    ).toBe("codex");
    expect(orderSources(["codex", "codex"]).length).toBe(6);
  });
});

describe("visibleSources / isGatewayBindableApp", () => {
  it("filters disabled tools and (recordsOnly) claude-desktop", () => {
    const order = orderSources(undefined);
    expect(visibleSources(order, { codex: false }, { recordsOnly: true })).toEqual(
      ["claude", "opencode", "grok"]
    );
    // Provider surfaces keep claude-desktop.
    expect(visibleSources(order, { codex: false })).toContain("claude-desktop");
  });

  it("grok binds via openai-compatible when the gateway speaks it", () => {
    expect(appProtocols(caps({ openaiCompatible: true })).grok).toEqual([
      "openai-compatible"
    ]);
    expect(appProtocols(caps({ openai: true })).grok).toEqual([]);
  });
});

describe("upgradeBadgeState", () => {
  const upgradable = { upgradeCommand: "codex update" };
  // Codex's desktop-app segment: a newer version exists, but there is no
  // way to upgrade it from here (Sparkle self-updates it).
  const displayOnly = { upgradeCommand: undefined };

  it("is idle and clickable with nothing going on", () => {
    expect(upgradeBadgeState(upgradable)).toEqual({
      label: "version",
      tone: "amber",
      clickable: true,
      disabled: false,
      tooltip: "command"
    });
  });

  it("swaps the label and disables itself while upgrading", () => {
    expect(upgradeBadgeState(upgradable, { upgrading: true })).toEqual({
      label: "updating",
      tone: "amber",
      clickable: true,
      disabled: true,
      // No tooltip at all mid-run: a stale one left open from before the
      // click would show a command that gets re-probed during the
      // upgrade (mid-reinstall Codex resolves to an absolute path).
      tooltip: "none"
    });
  });

  it("goes red and stays retryable after a failure", () => {
    expect(upgradeBadgeState(upgradable, { error: "EACCES" })).toEqual({
      label: "version",
      tone: "red",
      clickable: true,
      disabled: false,
      tooltip: "failed"
    });
  });

  it("prefers the running state over a previous failure", () => {
    // A retry is in flight — show it as running, not as still-failed.
    const state = upgradeBadgeState(upgradable, {
      upgrading: true,
      error: "EACCES"
    });
    expect(state.label).toBe("updating");
    expect(state.tone).toBe("amber");
  });

  // The rule Codex's two segments exist for: per-app state must never
  // leak onto a segment that cannot be upgraded.
  it.each([
    ["idle", {}],
    ["upgrading", { upgrading: true }],
    ["failed", { error: "EACCES" }],
    ["upgrading after a failure", { upgrading: true, error: "EACCES" }]
  ])("stays informational when %s", (_name, opts) => {
    expect(upgradeBadgeState(displayOnly, opts)).toEqual({
      label: "version",
      tone: "amber",
      clickable: false,
      disabled: false,
      tooltip: "info"
    });
  });

  it("is informational when the card wires up no upgrade handler", () => {
    expect(
      upgradeBadgeState(upgradable, { upgrading: true, canUpgrade: false })
    ).toEqual({
      label: "version",
      tone: "amber",
      clickable: false,
      disabled: false,
      tooltip: "info"
    });
  });
});

describe("codexVersionSegments — upgrade command", () => {
  const installs = (cli: boolean, app: boolean, appVersion?: string | null) =>
    ({ cli, app, appVersion }) as CodexInstalls;

  it("gives the command to the CLI segment only", () => {
    // `codex update` upgrades the npm CLI. The desktop app is a
    // separately versioned product that self-updates via Sparkle.
    const segments = codexVersionSegments(
      "0.144.6",
      installs(true, true, "26.707.31428"),
      tEn,
      "0.145.0",
      "26.721.30844",
      "codex update"
    );
    expect(segments[0].upgradeCommand).toBe("codex update");
    expect(segments[1].upgradeCommand).toBeUndefined();
  });

  it("carries the command on the pre-detection fallback segment", () => {
    expect(
      codexVersionSegments("0.144.6", null, tEn, "0.145.0", null, "codex update")
    ).toEqual([
      { text: "v0.144.6", latest: "0.145.0", upgradeCommand: "codex update" }
    ]);
  });
});
