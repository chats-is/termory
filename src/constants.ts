import type { CliApp, MemoryTool, Route } from "./types";

export const MEMORY_SOURCE = "Memory";
export const SKILL_SOURCE = "Skill";

export const MEMORY_TOOL_ORDER: MemoryTool[] = [
  "Claude",
  "Codex",
  "Gemini",
  "OpenCode",
  "Other"
];

export const CLI_APPS: CliApp[] = ["claude", "codex", "gemini", "opencode"];

export const CLI_APP_LABEL: Record<CliApp, string> = {
  claude: "Claude Code",
  codex: "Codex",
  gemini: "Gemini",
  opencode: "OpenCode"
};

export const CLI_APP_SOURCE_BADGE: Record<CliApp, string> = {
  claude: "Claude",
  codex: "Codex",
  gemini: "Gemini",
  opencode: "OpenCode"
};

// OpenCode `provider.<id>.npm` options — the AI SDK package OpenCode
// loads for the provider. `value` is the npm package written verbatim to
// opencode.json (the official config field); `label`/`hint` are display
// only. Default is the OpenAI-compatible adapter (covers most gateways).
export const OPENCODE_NPM_OPTIONS: {
  value: string;
  label: string;
  hint: string;
}[] = [
  {
    value: "@ai-sdk/openai-compatible",
    label: "OpenAI-compatible (default)",
    hint: "Generic OpenAI-shaped REST. Use for PackyCode, DMXAPI, OpenRouter, and most gateways."
  },
  {
    value: "@ai-sdk/anthropic",
    label: "Anthropic",
    hint: "Anthropic Claude API. Use for endpoints that mimic api.anthropic.com."
  },
  {
    value: "@ai-sdk/openai",
    label: "OpenAI (Responses)",
    hint: "OpenAI Responses API (/v1/responses) — api.openai.com or a gateway that implements it."
  },
  {
    value: "@ai-sdk/google",
    label: "Google",
    hint: "Google Gemini API."
  },
  {
    value: "@ai-sdk/azure",
    label: "Azure OpenAI",
    hint: "Azure-hosted OpenAI."
  },
  {
    value: "@ai-sdk/amazon-bedrock",
    label: "Amazon Bedrock",
    hint: "AWS Bedrock."
  },
  {
    value: "@ai-sdk/google-vertex",
    label: "Google Vertex",
    hint: "Google Vertex AI."
  }
];

export const OPENCODE_DEFAULT_NPM = "@ai-sdk/openai-compatible";

export const ACTIVE_STATE_REFRESH_EVENT = "termory:providers-refresh";

// Install instructions surfaced in the Providers page InstallGuide when
// the corresponding CLI binary is missing from PATH. Commands are
// pulled from each tool's official README (.audit-sources/<tool>/).
export type InstallMethod = { id: string; label: string; command: string };

export const CLI_INSTALL: Record<
  CliApp,
  { binary: string; url: string; methods: InstallMethod[] }
> = {
  claude: {
    binary: "claude",
    url: "https://code.claude.com/docs",
    methods: [
      {
        id: "curl",
        label: "curl",
        command: "curl -fsSL https://claude.ai/install.sh | bash"
      },
      {
        id: "npm",
        label: "npm",
        command: "npm install -g @anthropic-ai/claude-code"
      }
    ]
  },
  codex: {
    binary: "codex",
    url: "https://github.com/openai/codex",
    methods: [
      { id: "npm", label: "npm", command: "npm install -g @openai/codex" },
      { id: "brew", label: "brew", command: "brew install --cask codex" }
    ]
  },
  gemini: {
    binary: "gemini",
    url: "https://github.com/google-gemini/gemini-cli",
    methods: [
      {
        id: "npm",
        label: "npm",
        command: "npm install -g @google/gemini-cli"
      },
      { id: "brew", label: "brew", command: "brew install gemini-cli" },
      { id: "npx", label: "npx", command: "npx @google/gemini-cli" }
    ]
  },
  opencode: {
    binary: "opencode",
    url: "https://opencode.ai/docs",
    methods: [
      {
        id: "curl",
        label: "curl",
        command: "curl -fsSL https://opencode.ai/install | bash"
      },
      { id: "npm", label: "npm", command: "npm i -g opencode-ai@latest" },
      { id: "bun", label: "bun", command: "bun install -g opencode-ai" },
      {
        id: "brew",
        label: "brew",
        command: "brew install anomalyco/tap/opencode"
      },
      { id: "paru", label: "paru", command: "paru -S opencode-bin" }
    ]
  }
};

export const ROUTES: Route[] = [
  "records",
  "search",
  "stats",
  "favorites",
  "providers",
  "settings"
];

// Order matches the rail's visual order (Providers / Records /
// Favorites / Search / Stats / Settings) and ⌘1..6 bindings.
export const RAIL_ROUTE_ORDER: Route[] = [
  "providers",
  "records",
  "favorites",
  "search",
  "stats",
  "settings"
];

