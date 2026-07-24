import type {
  ActiveState,
  CliApp,
  CodexInstalls,
  Provider,
  Gateway,
  GatewayBinding,
  GatewayCapabilities,
  GatewayProtocol
} from "../types";
import type { MessageKey } from "@/i18n";
import { CLI_APPS } from "../constants";

/** Translator passed into the per-CLI help builders so their copy
 * renders in the active locale (the functions are pure / outside React). */
type Translate = (key: MessageKey, params?: Record<string, string | number>) => string;

/** Tools OFF BY DEFAULT — hidden until the user flips their Settings →
 * Tools switch to an explicit `true`. Gemini CLI stopped serving
 * individual accounts on 2026-06-18 (HTTP 410; enterprise Code Assist
 * still works, Google steers individuals to Antigravity CLI). MIRROR of
 * `DEFAULT_OFF_KEYS` in src-tauri/src/config.rs — keep the two in
 * sync. */
const DEFAULT_OFF_SOURCES: ReadonlySet<CliApp> = new Set(["gemini"]);

/** Resolve the user's Settings → Tools drag order (config `source_order`)
 * into the full tool list: saved entries first (unknown/duplicate ids
 * dropped), then any tool the saved list doesn't know yet (new tools
 * added in an update) in default order. Empty/absent → default order. */
export function orderSources(saved: readonly CliApp[] | undefined): CliApp[] {
  const seen = new Set<CliApp>();
  const out: CliApp[] = [];
  for (const key of saved ?? []) {
    if (CLI_APPS.includes(key) && !seen.has(key)) {
      seen.add(key);
      out.push(key);
    }
  }
  for (const key of CLI_APPS) {
    if (!seen.has(key)) out.push(key);
  }
  return out;
}

/** Settings → Tools: is `app` enabled under `toggles`? Absent key =
 * enabled (only an explicit `false` disables — a truthiness check would
 * wrongly treat never-persisted tools as off), EXCEPT the
 * `DEFAULT_OFF_SOURCES`, which need an explicit `true`. Every frontend
 * gate on the toggles must go through this helper. */
export function isSourceEnabled(
  toggles: Partial<Record<CliApp, boolean>> | undefined,
  app: CliApp
): boolean {
  const value = toggles?.[app];
  if (value === undefined) return !DEFAULT_OFF_SOURCES.has(app);
  return value !== false;
}

/** The single "which tools show on a record/pill surface" selector:
 * drag order ∩ enabled, optionally excluding tools with no records
 * source (Claude Desktop keeps no terminal history). App sidebar and
 * Stats pills share this so the two can't drift. */
export function visibleSources(
  ordered: readonly CliApp[],
  toggles: Partial<Record<CliApp, boolean>> | undefined,
  opts?: { recordsOnly?: boolean }
): CliApp[] {
  return ordered.filter(
    (k) =>
      (!opts?.recordsOnly || k !== "claude-desktop") && isSourceEnabled(toggles, k)
  );
}

/** One installed component on an Official card's version line.
 *
 * Codex is the reason this is a LIST rather than a string: it has two
 * independently-versioned install forms (the CLI and the desktop app),
 * and an update applies to ONE of them. Carrying `latest` per segment
 * lets the card put the badge directly after the component it refers
 * to — rendering it after the whole joined line made a CLI update read
 * as an App update. */
export type VersionSegment = {
  /** Preformatted version, e.g. `v0.142.5`, or `—` when the probe failed. */
  text: string;
  /** Component name shown in parens (`CLI` / `App`). Omitted for
   *  single-form apps, where there's nothing to disambiguate. */
  label?: string;
  /** Newer available version (bare, e.g. `0.143.0`) for THIS component,
   *  else null/absent — drives the badge that follows this segment. */
  latest?: string | null;
};

/** Compose the Codex Official card's version segments from its two
 * install forms — `v0.144.6 (CLI)` + `v26.715.31925 (App)` (whichever
 * are present).
 *
 * The two updates are tracked SEPARATELY because these are separately
 * versioned products: `cliLatest` comes from npm's `@openai/codex`
 * (which publishes the CLI only), `appLatest` from the desktop app's
 * Sparkle appcast. Each lands on its own segment — crossing them would
 * compare `0.144.6` against `26.721.30844`.
 *
 * Falls back to the plain CLI form while `detect_codex_installs` hasn't
 * resolved yet; empty when neither form is installed. */
export function codexVersionSegments(
  cliVersion: string | null | undefined,
  installs: CodexInstalls | null,
  t: Translate,
  cliLatest?: string | null,
  appLatest?: string | null
): VersionSegment[] {
  if (!installs) {
    return cliVersion ? [{ text: `v${cliVersion}`, latest: cliLatest }] : [];
  }
  const segments: VersionSegment[] = [];
  if (installs.cli) {
    segments.push({
      text: cliVersion ? `v${cliVersion}` : "—",
      label: t("providers.codexVersionCli"),
      latest: cliLatest
    });
  }
  if (installs.app) {
    segments.push({
      text: installs.appVersion ? `v${installs.appVersion}` : "—",
      label: t("providers.codexVersionApp"),
      // Gated on knowing the INSTALLED version too — with no baseline
      // there is nothing to be behind of, and the appcast's newest entry
      // would otherwise always look like an available update.
      latest:
        installs.appVersion && hasUpdate(installs.appVersion, appLatest)
          ? appLatest
          : null
    });
  }
  return segments;
}

/**
 * True when `latest` is a strictly newer release than `installed`.
 *
 * Both are `MAJOR.MINOR.PATCH[-prerelease]`. Cores are compared numerically
 * (shorter is zero-padded); on an equal core, a version WITH a prerelease is
 * treated as older than one without (so an installed Codex alpha reads as
 * behind the stable npm `latest`). A missing or unparseable side → `false`,
 * so we never show a spurious "update available".
 */
export function hasUpdate(
  installed: string | null | undefined,
  latest: string | null | undefined
): boolean {
  if (!installed || !latest) return false;
  const parse = (v: string) => {
    const [core, pre] = v.trim().split("-", 2);
    const nums = core.split(".").map((n) => Number.parseInt(n, 10));
    if (nums.length === 0 || nums.some((n) => Number.isNaN(n))) return null;
    return { nums, hasPre: pre !== undefined && pre !== "" };
  };
  const a = parse(installed);
  const b = parse(latest);
  if (!a || !b) return false;
  const len = Math.max(a.nums.length, b.nums.length);
  for (let i = 0; i < len; i++) {
    const ai = a.nums[i] ?? 0;
    const bi = b.nums[i] ?? 0;
    if (bi > ai) return true;
    if (bi < ai) return false;
  }
  // Equal cores: latest is newer only if installed is a prerelease and
  // latest is a stable release.
  return a.hasPre && !b.hasPre;
}

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
  if (app === "claude" || app === "claude-desktop") {
    base.baseUrl = "https://api.anthropic.com";
  } else if (app === "codex") {
    base.baseUrl = "https://api.openai.com/v1";
  } else if (app === "gemini") {
    base.baseUrl = "https://generativelanguage.googleapis.com";
  } else if (app === "opencode") {
    base.baseUrl = "https://api.anthropic.com";
  } else if (app === "grok") {
    // docs.x.ai custom-model example: base_url INCLUDES /v1.
    base.baseUrl = "https://api.x.ai/v1";
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
      p.app !== "claude-desktop" &&
      p.app !== "codex" &&
      p.app !== "gemini" &&
      p.app !== "opencode" &&
      p.app !== "grok"
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
    case "claude-desktop":
      return "https://api.anthropic.com";
    case "grok":
      return "https://api.x.ai/v1";
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
    case "grok":
      // The custom-model base_url includes /v1 (docs.x.ai example).
      return t("help.baseUrl.includeV1");
    case "claude-desktop":
      return t("help.baseUrl.claudeDesktop");
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
    case "claude-desktop":
      return t("help.override.claudeDesktop");
    case "grok":
      return t("help.override.grok");
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
    // Claude Desktop's 3P gateway speaks the Anthropic Messages format —
    // bindable whenever the gateway has the Anthropic capability, exactly
    // like Claude Code.
    "claude-desktop": caps?.anthropic ? ["anthropic"] : [],
    codex: caps?.openai ? ["openai"] : [], // Codex needs Responses
    gemini: caps?.gemini ? ["gemini"] : [],
    // xAI's API is OpenAI-compatible chat completions. Like every other
    // binding this is best-effort (the probe can't prove Grok Build's
    // full runtime works against the gateway — same documented caveat
    // as the rest); the binding additionally REQUIRES a model, since
    // grok's api_key is stored per-model (GatewayEditor validates).
    grok: caps?.openaiCompatible ? ["openai-compatible"] : [],
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
    // xAI's API is OpenAI-compatible chat completions.
    case "grok":
      return "openai-compatible";
    case "opencode":
      return protocolForNpm(binding.npm ?? "");
    // Claude Desktop binds Anthropic-capable gateways (see appProtocols);
    // its 3P gateway speaks the Anthropic Messages format.
    case "claude-desktop":
      return "anthropic";
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
  }
  // Models list — OpenCode's extra models, Claude Desktop's inferenceModels,
  // AND grok's required model list.
  if (
    binding.models?.length &&
    (binding.app === "opencode" ||
      binding.app === "claude-desktop" ||
      binding.app === "grok")
  ) {
    provider.models = binding.models;
  }
  if (binding.app === "grok" && binding.apiBackend)
    provider.apiBackend = binding.apiBackend;
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
    case "grok":
      // MIRROR of the Rust `override_key_is_managed` Grok arm: the
      // default pointer + the five owned fields of ANY `[model.<id>]`
      // entry (the entry key is the model id). Other per-entry keys
      // (api_backend, context_window, …) pass through as Advanced
      // settings.
      return (
        k === "models.default" ||
        (k.startsWith("model.") &&
          ["model", "base_url", "name", "description", "api_key", "api_backend"].some((f) =>
            k.endsWith(`.${f}`)
          ))
      );
    // Claude Desktop: Advanced-settings options merge into the 3P gateway
    // profile JSON; the keys filled from dedicated fields (Base URL / API
    // key / models) are managed. MIRROR of `override_key_is_managed`'s
    // ClaudeDesktop arm in providers.rs.
    case "claude-desktop":
      return [
        "inferenceProvider",
        "inferenceGatewayBaseUrl",
        "inferenceGatewayApiKey",
        "inferenceGatewayAuthScheme",
        "inferenceModels",
        "disableDeploymentModeChooser",
        "coworkEgressAllowedHosts"
      ].includes(k);
  }
}

/**
 * Whether a model id is one Claude Desktop will accept in `inferenceModels`.
 * Claude Desktop rejects non-Anthropic model names ("is not an Anthropic
 * model from the provider catalog") — they must be a `claude-*` /
 * `anthropic/claude-*` role name (sonnet / opus / haiku / fable). A trailing
 * `[1m]` marker is stripped first. Mirror of `is_claude_safe_model_id`
 * (cc-switch / the Claude Desktop bundle). Used to BLOCK save in both
 * editors (feeds `canSave`) and to flag the row — empty ids are treated as
 * "fine" (they're dropped on save, not written).
 */
export function isClaudeSafeModelId(id: string): boolean {
  let s = id.trim().toLowerCase();
  if (!s) return true; // blank rows are dropped, not flagged
  if (s.endsWith("[1m]")) s = s.slice(0, -4).trimEnd();
  const tail = s.startsWith("anthropic/claude-")
    ? s.slice("anthropic/claude-".length)
    : s.startsWith("claude-")
      ? s.slice("claude-".length)
      : null;
  if (tail === null) return false;
  return ["sonnet-", "opus-", "haiku-", "fable-"].some(
    (role) => tail.startsWith(role) && tail.length > role.length
  );
}
