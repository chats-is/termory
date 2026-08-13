import type { CliApp, MemoryTool, Route } from "./types";

export const MEMORY_SOURCE = "Memory";
export const SKILL_SOURCE = "Skill";

export const MEMORY_TOOL_ORDER: MemoryTool[] = [
  "Claude",
  "Codex",
  "Gemini",
  "OpenCode",
  "Grok",
  "Other"
];

export const CLI_APPS: CliApp[] = [
  "claude",
  "claude-desktop",
  "codex",
  "gemini",
  "opencode",
  "grok"
];

export const CLI_APP_LABEL: Record<CliApp, string> = {
  claude: "Claude Code",
  "claude-desktop": "Claude Desktop",
  codex: "Codex",
  gemini: "Gemini",
  opencode: "OpenCode",
  grok: "Grok Build"
};


export const CLI_APP_SOURCE_BADGE: Record<CliApp, string> = {
  claude: "Claude",
  // Claude Desktop reuses the Claude brand icon (same BrandIcon branch).
  "claude-desktop": "Claude",
  codex: "Codex",
  gemini: "Gemini",
  opencode: "OpenCode",
  grok: "Grok"
};



// OpenCode `provider.<id>.npm` options — the AI SDK package OpenCode
// loads for the provider. `value` is the npm package written verbatim to
// opencode.json (the official config field); `label`/`hint` are display
// only. Default is the OpenAI Responses adapter.
export const OPENCODE_NPM_OPTIONS: {
  value: string;
  label: string;
  hint: string;
}[] = [
  {
    value: "@ai-sdk/openai",
    label: "OpenAI (Responses)",
    hint: "OpenAI Responses API (/v1/responses) — api.openai.com or a gateway that implements it."
  },
  {
    value: "@ai-sdk/openai-compatible",
    label: "OpenAI-compatible",
    hint: "Generic OpenAI-shaped REST. Use for PackyCode, DMXAPI, OpenRouter, and most gateways."
  },
  {
    value: "@ai-sdk/anthropic",
    label: "Anthropic",
    hint: "Anthropic Claude API. Use for endpoints that mimic api.anthropic.com."
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

export const OPENCODE_DEFAULT_NPM = "@ai-sdk/openai";

export const ACTIVE_STATE_REFRESH_EVENT = "termory:providers-refresh";

// Backend pushes a `SubscriptionQuota` payload after every completed
// quota fetch (tray click, watcher credential-change, IPC), so the
// Providers page stays in sync without its own request. Rust mirror:
// QUOTA_CHANGED_EVENT in src-tauri/src/quota.rs.
export const QUOTA_CHANGED_EVENT = "termory:quota-changed";

// Backend pushes a `ProviderBalance` after every balance fetch it made
// itself (the tray fetches on menu open, at startup, and after a provider
// switch or edit), so the Providers page reflects it without its own
// request and both share one throttle marker. Rust mirror:
// BALANCE_CHANGED_EVENT in src-tauri/src/balance.rs.
export const BALANCE_CHANGED_EVENT = "termory:balance-changed";

// Backend pushes `{ app, ok, error? }` when its account auto-sync reached an
// outcome worth reporting for that CLI: it rewrote the saved entry for the
// account currently logged in (a token rotation, a re-login run in the
// terminal, a plan change), or it failed trying. A pass that found nothing
// to change emits NOTHING — that is nearly every pass, and only a real
// update counts as having synced. Nothing the user did in Termory starts
// it, so this is the only way the UI learns of it. Rust mirror:
// ACCOUNTS_CHANGED_EVENT / AccountSyncEvent in src-tauri/src/accounts.rs.
export const ACCOUNTS_CHANGED_EVENT = "termory:accounts-changed";

// Backend pushes a CLI key ("codex", …) when that CLI's cached quota was
// dropped because the LOGIN changed (an account switch), so the page can
// discard the previous account's numbers — it has no other way to learn
// of a switch made from the menu-bar tray. Rust mirror:
// QUOTA_INVALIDATED_EVENT in src-tauri/src/quota.rs.
export const QUOTA_INVALIDATED_EVENT = "termory:quota-invalidated";

// A provider switch the TRAY started but handed to the page because it needs
// the "follow sessions?" prompt the tray can't show (Codex official↔custom).
// The page fetches the request with the `take_pending_tray_switch` IPC.
// Mirror of tray::TRAY_SWITCH_REQUEST_EVENT in src-tauri/src/tray.rs.
export const TRAY_SWITCH_REQUEST_EVENT = "termory:tray-switch-request";

// config.json key: follow ALL projects silently on a Codex bucket switch,
// instead of asking which ones (Settings toggle, default off). Mirror of
// `config::CODEX_KEEP_ALL_SESSIONS_KEY` in src-tauri/src/config.rs.
export const CODEX_KEEP_ALL_SESSIONS_KEY = "codex_keep_all_sessions";

// Install instructions surfaced in the Providers page InstallGuide when
// the corresponding CLI binary is missing from PATH. Commands are
// pulled from each tool's official README (.audit-sources/<tool>/).
export type InstallMethod = {
  id: string;
  label: string;
  command: string;
  platforms?: ("mac" | "linux" | "windows")[];
};

export const CLI_INSTALL: Record<
  CliApp,
  { binary: string; url: string; methods: InstallMethod[] }
> = {
  claude: {
    binary: "claude",
    url: "https://code.claude.com/docs",
    methods: [
      {
        id: "npm",
        label: "npm",
        command: "npm install -g @anthropic-ai/claude-code"
      },
      {
        id: "native",
        label: "curl",
        command: "curl -fsSL https://claude.ai/install.sh | bash"
      },
      {
        id: "brew",
        label: "brew",
        command: "brew install --cask claude-code",
        platforms: ["mac"]
      },
      {
        id: "powershell",
        label: "powershell",
        command: "irm https://claude.ai/install.ps1 | iex",
        platforms: ["windows"]
      },
      {
        id: "cmd",
        label: "cmd",
        command: "curl -fsSL https://claude.ai/install.cmd -o install.cmd && install.cmd && del install.cmd",
        platforms: ["windows"]
      },
      {
        id: "winget",
        label: "winget",
        command: "winget install Anthropic.ClaudeCode",
        platforms: ["windows"]
      }
    ]
  },
  // Claude Desktop is a downloaded GUI app — no package manager, just one
  // Download entry (its InstallGuide shows when the app isn't installed;
  // the method tabs are hidden for single-method apps).
  "claude-desktop": {
    binary: "Claude",
    url: "https://claude.ai/download",
    methods: [
      {
        id: "download",
        label: "download",
        command: "https://claude.ai/download"
      }
    ]
  },
  // Method tabs across every CLI share ONE canonical order. Labels are all
  // lowercase and there is no separate "Native" entry — claude's native
  // installer IS its `curl` tab — so npm always leads where it exists:
  // npm → curl → brew → bun → paru → powershell → cmd → winget → app/download.
  codex: {
    binary: "codex",
    url: "https://developers.openai.com/codex",
    methods: [
      { id: "npm", label: "npm", command: "npm install -g @openai/codex" },
      // Official standalone installer (chatgpt.com domain, redirects to
      // releases.openai.com) — installs a native CLI to ~/.local/bin/codex
      // on Unix (install.sh:8) but %LOCALAPPDATA%\Programs\OpenAI\Codex\bin
      // on Windows (install.ps1:743); both are covered by cli_search_paths,
      // as is the $CODEX_INSTALL_DIR override both scripts honor.
      {
        id: "curl",
        label: "curl",
        command: "curl -fsSL https://chatgpt.com/codex/install.sh | sh"
      },
      {
        id: "brew",
        label: "brew",
        command: "brew install --cask codex",
        platforms: ["mac"]
      },
      {
        id: "powershell",
        label: "powershell",
        command: "powershell -ExecutionPolicy ByPass -c \"irm https://chatgpt.com/codex/install.ps1 | iex\"",
        platforms: ["windows"]
      },
      // The merged ChatGPT/Codex desktop app (2026-07: the Codex app IS
      // the new unified ChatGPT app) — an app-only install is fully
      // supported (shared ~/.codex/, bundled-CLI fallback), so it's a
      // legitimate install method here, same pattern as claude-desktop.
      {
        id: "app",
        label: "app",
        command: "https://chatgpt.com/download",
        platforms: ["mac", "windows"]
      }
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
      {
        id: "brew",
        label: "brew",
        command: "brew install gemini-cli",
        platforms: ["mac"]
      },
      { id: "npx", label: "npx", command: "npx @google/gemini-cli" }
    ]
  },
  opencode: {
    binary: "opencode",
    url: "https://opencode.ai/docs",
    methods: [
      { id: "npm", label: "npm", command: "npm i -g opencode-ai" },
      {
        id: "curl",
        label: "curl",
        command: "curl -fsSL https://opencode.ai/install | bash"
      },
      {
        id: "brew",
        label: "brew",
        command: "brew install anomalyco/tap/opencode",
        platforms: ["mac"]
      },
      {
        id: "bun",
        label: "bun",
        command: "bun add -g opencode-ai"
      },
      {
        id: "paru",
        label: "paru",
        command: "paru -S opencode",
        platforms: ["linux"]
      }
    ]
  },
  grok: {
    binary: "grok",
    url: "https://docs.x.ai/build/overview",
    methods: [
      // `@xai-official/grok` verified on the registry (latest 1.0.3, bin
      // `grok`, os darwin/linux/win32). Its postinstall drops the real
      // binary into `$GROK_HOME/bin` — which `cli_search_paths` already
      // probes for grok — so an npm install is detected like any other.
      { id: "npm", label: "npm", command: "npm install -g @xai-official/grok" },
      {
        id: "curl",
        label: "curl",
        command: "curl -fsSL https://x.ai/cli/install.sh | bash"
      },
      {
        id: "powershell",
        label: "powershell",
        command: "irm https://x.ai/cli/install.ps1 | iex",
        platforms: ["windows"]
      }
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
