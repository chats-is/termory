import type { CliApp, Provider } from "../types";

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

export function baseUrlPlaceholder(app: CliApp): string {
  switch (app) {
    case "claude":
      return "https://api.anthropic.com";
    case "codex":
      return "https://api.openai.com/v1";
    case "gemini":
      return "https://generativelanguage.googleapis.com";
    case "opencode":
      return "https://api.anthropic.com";
  }
}

export function baseUrlHelp(app: CliApp): string {
  switch (app) {
    case "claude":
      return "Don't include /v1 — Claude appends it.";
    case "codex":
      return "Include /v1 at the end of the URL.";
    case "gemini":
      return "The base URL of your provider's API.";
    case "opencode":
      return "Use the provider's OpenAI/Anthropic-compatible root URL.";
  }
}

export function apiKeyHelp(_app: CliApp): string {
  return "Stored locally on this machine and only sent to the provider you choose.";
}

/**
 * Whether an "Advanced settings" option key is managed by one of the
 * provider's own dedicated fields (Base URL / API key / Model / AI SDK).
 * Managed keys are silently skipped by the backend at write time, so the
 * editor blocks them up front. MUST mirror `override_key_is_managed` in
 * `src-tauri/src/providers.rs` — keep the two in sync.
 */
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
