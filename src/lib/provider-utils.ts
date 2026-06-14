import type {
  ActiveState,
  CliApp,
  Provider,
  Gateway,
  GatewayBinding,
  GatewayCapabilities,
  GatewayProtocol
} from "../types";
import type { MessageKey } from "@/i18n";

/** Translator passed into the per-CLI help builders so their copy
 * renders in the active locale (the functions are pure / outside React). */
type Translate = (key: MessageKey, params?: Record<string, string | number>) => string;

export function newProviderId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function blankProvider(app: CliApp): Provider {
  const base: Provider = {
    id: newProviderId(),
    app,
    kind: "custom",
    name: "",
    baseUrl: "",
    apiKey: "",
    model: ""
  };
  if (app === "claude") {
    base.baseUrl = "https://api.anthropic.com";
  } else if (app === "codex") {
    base.baseUrl = "https://api.openai.com/v1";
  } else if (app === "gemini") {
    base.baseUrl = "https://generativelanguage.googleapis.com";
  } else if (app === "opencode") {
    base.baseUrl = "https://api.anthropic.com";
  }
  return base;
}

/**
 * Resolve which provider/gateway-binding id is "in use" for a single-slot
 * CLI, disambiguating the case where several entries share the exact same
 * base+key (indistinguishable on disk — e.g. a standalone provider and a
 * gateway binding pointing at the same endpoint).
 *
 * Termory records `markerId` = the id the user last activated for this CLI.
 * The marker is honored ONLY when its candidate's creds still match the
 * live config snapshot — so an external change (cc-switch / manual edit /
 * OAuth) makes the stale marker fall back to the backend's reverse-derived
 * match, preserving the "no blind active pointer" guarantee.
 */
export function resolveActiveProviderId(
  state: ActiveState | null | undefined,
  markerId: string | undefined,
  candidates: { id: string; baseUrl?: string; apiKey?: string }[]
): string | null {
  if (!state) return null;
  const live = state.liveSnapshot;
  if (markerId && live) {
    const marked = candidates.find((c) => c.id === markerId);
    if (
      marked &&
      (marked.baseUrl ?? "") === (live.baseUrl ?? "") &&
      maskKey(marked.apiKey ?? "") === (live.apiKeyMasked ?? "")
    ) {
      return markerId;
    }
  }
  return state.matchedProviderId ?? null;
}

export function maskKey(key: string): string {
  if (!key) return "";
  if (key.length <= 8) return "•".repeat(key.length);
  return `${key.slice(0, 4)}${"•".repeat(key.length - 8)}${key.slice(-4)}`;
}

export function isProviderList(raw: unknown): raw is Provider[] {
  if (!Array.isArray(raw)) return false;
  for (const item of raw) {
    if (!item || typeof item !== "object") return false;
    const p = item as Record<string, unknown>;
    if (typeof p.id !== "string") return false;
    if (typeof p.name !== "string") return false;
    if (
      p.app !== "claude" &&
      p.app !== "codex" &&
      p.app !== "gemini" &&
      p.app !== "opencode"
    ) {
      return false;
    }
    if (p.kind !== "official" && p.kind !== "custom") return false;
  }
  return true;
}

export function baseUrlPlaceholder(app: CliApp, npm?: string): string {
  switch (app) {
    case "claude":
      return "https://api.anthropic.com";
    case "codex":
      return "https://api.openai.com/v1";
    case "gemini":
      return "https://generativelanguage.googleapis.com";
    case "opencode":
      // Depends on the chosen AI SDK package (same path conventions as
      // the per-CLI rules above).
      switch (protocolForNpm(npm ?? "")) {
        case "openai-compatible":
        case "openai":
          return "https://api.example.com/v1";
        case "anthropic":
          return "https://api.anthropic.com";
        case "gemini":
          return "https://generativelanguage.googleapis.com";
      }
  }
}

export function baseUrlHelp(app: CliApp, npm: string | undefined, t: Translate): string {
  switch (app) {
    case "claude":
      return t("help.baseUrl.claudeNoV1");
    case "codex":
      return t("help.baseUrl.includeV1");
    case "gemini":
      return t("help.baseUrl.generic");
    case "opencode":
      // The required path depends on the selected AI SDK package.
      switch (protocolForNpm(npm ?? "")) {
        case "openai-compatible":
        case "openai":
          return t("help.baseUrl.openaiV1");
        case "anthropic":
          return t("help.baseUrl.anthropicNoV1");
        case "gemini":
          return t("help.baseUrl.googleBare");
      }
  }
}

export function apiKeyHelp(_app: CliApp, t: Translate): string {
  return t("help.apiKey");
}

/** Per-CLI help for the "Advanced settings" / overrides section. Shared
 * by ProviderEditor and the gateway binding editor so the wording stays
 * identical. */
export function overrideHelpFor(app: CliApp, t: Translate): string {
  switch (app) {
    case "claude":
      return t("help.override.claude");
    case "codex":
      return t("help.override.codex");
    case "gemini":
      return t("help.override.gemini");
    case "opencode":
      return t("help.override.opencode");
  }
}

/**
 * Whether an "Advanced settings" option key is managed by one of the
 * provider's own dedicated fields (Base URL / API key / Model / AI SDK).
 * Managed keys are silently skipped by the backend at write time, so the
 * editor blocks them up front. MUST mirror `override_key_is_managed` in
 * `src-tauri/src/providers.rs` — keep the two in sync.
 */
// ── Gateway helpers ─────────────────────────────────────────

export function newGatewayId(): string {
  return newProviderId();
}

export function blankGateway(): Gateway {
  return {
    kind: "gateway",
    id: newGatewayId(),
    name: "",
    baseUrl: "",
    apiKey: "",
    bindings: []
  };
}

const OPENCODE_NPM_BY_PROTOCOL: Record<GatewayProtocol, string> = {
  "openai-compatible": "@ai-sdk/openai-compatible", // Chat Completions
  openai: "@ai-sdk/openai", // Responses
  anthropic: "@ai-sdk/anthropic",
  gemini: "@ai-sdk/google"
};

/** OpenCode AI-SDK package for a gateway protocol. */
export function npmForProtocol(protocol: GatewayProtocol): string {
  return OPENCODE_NPM_BY_PROTOCOL[protocol];
}

/** Inverse of `npmForProtocol`: which wire protocol an OpenCode AI-SDK
 * package speaks. For OpenCode the binding's protocol is DERIVED from
 * the chosen package. NOTE order: `@ai-sdk/openai-compatible` (Chat
 * Completions) must be matched BEFORE `@ai-sdk/openai` (Responses), since
 * the former's name also contains "openai". */
export function protocolForNpm(npm: string): GatewayProtocol {
  if (npm.includes("anthropic") || npm.includes("bedrock")) return "anthropic";
  if (npm.includes("google")) return "gemini";
  if (npm.includes("openai-compatible")) return "openai-compatible";
  if (npm.includes("openai")) return "openai"; // Responses
  return "openai-compatible"; // azure / unknown → Chat Completions
}

/** Which protocols each CLI can bind to, given detected capabilities.
 * Claude→Anthropic, Codex→OpenAI **Responses** specifically, Gemini→Gemini;
 * OpenCode is flexible (npm SDK) so it lists every supported mode — both
 * OpenAI flavors when present. OpenAI **Responses** (`openai`) is listed
 * before `openai-compatible` so it's OpenCode's default when the gateway
 * supports it. Empty list ⇒ that CLI can't bind. */
export function appProtocols(
  caps: GatewayCapabilities | undefined
): Record<CliApp, GatewayProtocol[]> {
  const opencode: GatewayProtocol[] = [];
  if (caps?.anthropic) opencode.push("anthropic");
  if (caps?.openai) opencode.push("openai");
  if (caps?.openaiCompatible) opencode.push("openai-compatible");
  if (caps?.gemini) opencode.push("gemini");
  return {
    claude: caps?.anthropic ? ["anthropic"] : [],
    codex: caps?.openai ? ["openai"] : [], // Codex needs Responses
    gemini: caps?.gemini ? ["gemini"] : [],
    opencode
  };
}

/** Derive the per-CLI base URL from the gateway's path-less ROOT. The
 * gateway stores no API-version path; each protocol's CLI gets the path it
 * expects: OpenAI a trailing `/v1`, Anthropic the bare root (Claude appends
 * `/v1` itself), Gemini the bare root (it appends `/v1beta`). Any version
 * suffix pasted into the root is stripped first so it's never doubled. */
export function gatewayBaseForProtocol(
  base: string,
  protocol: GatewayProtocol
): string {
  // Reduce to the gateway's bare ROOT (strip any API-version suffix the
  // user may have pasted), then apply each protocol's path convention.
  const b = (base ?? "")
    .trim()
    .replace(/\/+$/, "")
    .replace(/\/(v1beta|v1)$/, "");
  switch (protocol) {
    case "openai-compatible":
    case "openai":
      // Both OpenAI flavors live under /v1 (chat/completions vs responses).
      return `${b}/v1`;
    case "anthropic":
      return b; // bare root — Claude Code appends /v1 itself
    case "gemini":
      return b; // bare root — Gemini appends /v1beta itself
  }
}

/** The wire protocol a binding uses — DERIVED, not stored. Claude/Codex/
 * Gemini each have exactly one mode; OpenCode's comes from its AI SDK
 * package. */
export function protocolForBinding(binding: {
  app: CliApp;
  npm?: string;
}): GatewayProtocol {
  switch (binding.app) {
    case "claude":
      return "anthropic";
    case "codex":
      return "openai";
    case "gemini":
      return "gemini";
    case "opencode":
      return protocolForNpm(binding.npm ?? "");
  }
}

/** Materialize a gateway+binding into the existing `Provider` shape so the
 * normal `activate_provider` / reverse-derive path can be reused. The
 * synthesized provider's id IS the binding's own id (stable, unique). */
export function providerFromBinding(gateway: Gateway, binding: GatewayBinding): Provider {
  const protocol = protocolForBinding(binding);
  const provider: Provider = {
    id: binding.id,
    app: binding.app,
    kind: "custom",
    name: gateway.name,
    baseUrl: gatewayBaseForProtocol(gateway.baseUrl ?? "", protocol),
    apiKey: gateway.apiKey ?? "",
    model: binding.model ?? "",
    favicon: gateway.favicon
  };
  if (binding.app === "opencode") {
    provider.npm = binding.npm ?? npmForProtocol(protocol);
    if (binding.models?.length) provider.models = binding.models;
  }
  if (binding.options?.length) provider.options = binding.options;
  return provider;
}

/** Validate a loaded `gateways` array (defensive — same spirit as
 * `isProviderList`). */
export function isGatewayList(raw: unknown): raw is Gateway[] {
  if (!Array.isArray(raw)) return false;
  for (const item of raw) {
    if (!item || typeof item !== "object") return false;
    const r = item as Record<string, unknown>;
    // Unified providers.json discriminant — a gateway is `kind: "gateway"`.
    if (r.kind !== "gateway") return false;
    if (typeof r.id !== "string" || typeof r.name !== "string") return false;
    if (r.bindings === undefined) continue;
    if (!Array.isArray(r.bindings)) return false;
    // Each binding must carry its OWN id (the new shape). This rejects
    // pre-refactor data (bindings had `protocol` but no `id`) so it's
    // dropped rather than half-loaded.
    for (const b of r.bindings) {
      if (!b || typeof b !== "object") return false;
      const bb = b as Record<string, unknown>;
      if (typeof bb.id !== "string" || typeof bb.app !== "string") return false;
    }
  }
  return true;
}

export function isManagedOptionKey(app: CliApp, key: string): boolean {
  const k = key.trim();
  switch (app) {
    case "claude":
      return [
        "env.ANTHROPIC_BASE_URL",
        "env.ANTHROPIC_AUTH_TOKEN",
        "env.ANTHROPIC_API_KEY",
        "env.ANTHROPIC_MODEL"
      ].includes(k);
    case "codex":
      return (
        k === "model_provider" ||
        k === "model" ||
        k.startsWith("model_providers.")
      );
    case "gemini":
      return ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY", "GEMINI_MODEL"].includes(
        k
      );
    case "opencode":
      // Options nest under the provider's own `options` bag; keys are
      // relative to it, and baseURL/apiKey come from the dedicated fields.
      return k === "baseURL" || k === "apiKey";
  }
}
