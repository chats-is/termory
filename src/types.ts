// Cross-file type definitions for Termory.

// `tokens` and `model` map 1:1 onto the Rust-side TokenStats /
// Option<String> on AppSession. Both are `undefined`/absent when the
// source platform didn't record token data for that session — the
// Stats page renders coverage explicitly ("N of M sessions") so
// users aren't misled by partial totals.
export type TokenStats = {
  input: number;
  output: number;
  cached: number;
  reasoning: number;
  total: number;
};

/** One entry per local date a session was active — populated by all
 * four scanners (Claude / Codex / Gemini / OpenCode) when the
 * underlying records carry per-message / per-part timestamps. Used by
 * the Stats page to place spend on the day it actually happened.
 * Absent only when no timestamped token data could be recovered. */
export type DailyTokenBreakdown = {
  date: string; // YYYY-MM-DD (local)
  tokens: TokenStats;
  /** Number of AI interactions counted on this date. Absent on older
   * sessions parsed before the field existed; default to 0. */
  messages?: number;
  /** Per-hour message counts, indexed by local hour 0..23. Empty when
   * timestamps weren't available. */
  hours?: number[];
  /** Per-hour token totals, indexed by local hour 0..23. Parallel to
   * `hours` but accumulates tokens.total instead of an interaction
   * count. Empty for sources/sessions that didn't carry per-message
   * timestamps. */
  hour_tokens?: number[];
};

export type AppSession = {
  id: string;
  source: string;
  title: string;
  project: string;
  path: string;
  started_at?: string | null;
  updated_at?: string | null;
  message_count: number;
  /** True when the source CLI archived this session. Only Codex
   * (`threads.archived`) and OpenCode (`session.time_archived`) have a
   * local archive flag; always false for Claude / Gemini. Rendered as an
   * "Archived" badge on the list card and detail header. */
  archived?: boolean;
  preview: string;
  snippet?: string;
  message_previews: SessionMessage[];
  tokens?: TokenStats;
  model?: string;
  daily_tokens?: DailyTokenBreakdown[];
};

/**
 * A first-class project (a working directory a CLI keeps records for). Shown
 * in the sidebar as long as it exists, with or without records. `project` is
 * the cwd string records carry in their own `project` field — the join key.
 * An empty project = one in the list that no record points at.
 */
export type Project = {
  source: string;
  project: string;
};

/** What `scan_all_sessions` (and the watcher event) returns. */
export type ScanResult = {
  projects: Project[];
  records: AppSession[];
};

export type SessionMessage = {
  role: string;
  text: string;
  timestamp?: string | null;
  kind: string;
};

export type SessionDetail = {
  session: AppSession;
  messages: SessionMessage[];
};

/** A user-saved message snapshot. Stored in `~/.termory/favorites.json`
 * and persists across CLI re-scans even if the original session file
 * is renamed / deleted (we keep the full content + enough source ref
 * to attempt re-opening the original transcript). */
export type Favorite = {
  /** Unique id for the favorite itself (not the source message). */
  id: string;
  /** ISO timestamp when the user starred this message. */
  favorited_at: string;
  /** Full message snapshot — survives source-file changes. */
  message: SessionMessage;
  /** Source platform tag. Narrowed to the 4 session sources because
   * Memory / Skill items don't have a starrable message list. */
  source: SessionSource;
  /** Best-effort reference back to the original session. The source
   * file might have been deleted by the time the favorite is opened;
   * the snapshot above is the authoritative copy. */
  source_session_id: string;
  source_session_path: string;
  source_session_title: string;
  source_session_project: string;
  /** Index of this message within the session's parsed message list
   * AT TIME OF FAVORITE. Indices can drift if the parser changes
   * shape, so don't rely on this for content — use it only as a
   * fast equality key for "is this message currently favorited?". */
  source_message_index: number;
};

export type SearchHit = {
  session: AppSession;
  snippet: string;
  role: string;
  match_count: number;
  // True when the per-session search loop stopped after hitting the
  // backend's 500-match cap — render `×500+` instead of `×500` so
  // the user knows there were probably way more.
  truncated?: boolean;
  // 0-based index of the first matched message in `detail.messages`.
  // The frontend feeds this to `openItem(session, messageIndex)` so
  // the Records detail pane scrolls to the match on jump.
  first_match_index?: number;
};

/** Sources of parsed session transcripts. Capitalized form lives on
 * `AppSession.source` (Rust enum → string). Currently the same 4
 * values are also the legal `Favorite.source`. */
export type SessionSource = "Claude" | "Codex" | "Gemini" | "OpenCode" | "Grok";



export type MemoryTool = SessionSource | "Other";

export type CliApp = "claude" | "claude-desktop" | "codex" | "gemini" | "opencode" | "grok";

export type ProviderKind = "official" | "custom";

export type Provider = {
  id: string;
  app: CliApp;
  kind: ProviderKind;
  name: string;
  // All string fields below are optional in storage — config.ts strips
  // ""/null/undefined when writing providers.json, so a freshly-loaded
  // Provider may omit any of them. React inputs that bind to these
  // must use `?? ""` to stay controlled.
  baseUrl?: string;
  apiKey?: string;
  model?: string;
  // OpenCode-only: the AI SDK npm package OpenCode loads for this
  // provider — written verbatim to opencode.json `provider.<id>.npm`
  // (the official config field, e.g. "@ai-sdk/openai-compatible").
  // Only the OpenCode editor branch sets it and only the OpenCode
  // backend reads it — inert for other apps.
  npm?: string;
  // OpenCode-only: extra models surfaced in OpenCode's picker alongside
  // the primary top-level `model`. Each is `{ id, name }` → written as
  // `models: { <id>: { name } }` (name defaults to id when blank). Like
  // `npm`, OpenCode-only.
  models?: { id: string; name: string }[];
  // User-defined provider options ("Advanced settings" in the editor)
  // merged into the CLI's live config on activation and stripped on
  // switch/deactivate. `key` is a dot-path (`env.FOO`, `tools.web_search`);
  // `value` is type-inferred (bool/number/else string) for JSON/TOML
  // targets, verbatim for Gemini's `.env`.
  options?: { key: string; value: string }[];
  // Grok-only: the wire API each `[model.*]` entry declares
  // (api_backend = chat_completions | responses | messages). Defaults to
  // "responses" when unset. Read ONLY by the grok activate path.
  apiBackend?: string;
  // Cached favicon as a `data:image/...;base64,...` URL. Populated at
  // create / edit time by `invoke('fetch_provider_favicon')` so the
  // ProviderCard renders the brand mark locally without making a
  // network request on every render or leaking the hostname to a
  // third-party favicon proxy. Absent → fall back to the letter avatar.
  favicon?: string;
};

// ── Gateway (a SECOND, independent kind of provider management) ──
// A gateway is ONE {baseUrl, apiKey} that may speak several API modes. It
// lives in the same providers.json file but in a separate `gateways` array
// (see config.rs) so it never collides with the per-CLI Provider parse.

/** API mode a binding speaks. Derived per binding from app/npm
 * (`protocolForBinding`). Mirrors the AI SDK packages: `openai-compatible`
 * = Chat Completions (`@ai-sdk/openai-compatible`), `openai` = Responses
 * (`@ai-sdk/openai`). */
export type GatewayProtocol =
  | "openai-compatible"
  | "openai"
  | "anthropic"
  | "gemini";

/** Which API modes a gateway supports (backend `GatewayCapabilities`).
 * Each is probed at its OWN real API endpoint (POST), never a list:
 *   openaiCompatible → POST /v1/chat/completions   (@ai-sdk/openai-compatible)
 *   openai           → POST /v1/responses          (@ai-sdk/openai, Codex)
 *   anthropic        → POST /v1/messages           (@ai-sdk/anthropic)
 *   gemini           → GET  /v1beta/models?key= returns data (@ai-sdk/google)
 * `models` is a single flat catalog (union of the two GET /models lists)
 * used only as autocomplete candidates — the gateway routes by model id,
 * so there's no reliable per-mode model split. */
export type GatewayCapabilities = {
  openaiCompatible: boolean;
  openai: boolean;
  anthropic: boolean;
  gemini: boolean;
  models: string[];
};

/** One gateway→CLI binding. The gateway supplies baseUrl/apiKey; the binding
 * picks the protocol + model (+ OpenCode npm package / options) used to
 * materialize a per-CLI provider via the existing activate path. */
export type GatewayBinding = {
  /** Own stable id — a binding is a full provider minus the gateway's
   * common fields (name/baseUrl/apiKey). Used as the synthesized
   * provider id + the activation marker; lets one CLI hold several
   * bindings without an id collision. */
  id: string;
  app: CliApp;
  model?: string;
  /** OpenCode-only: AI SDK package. The wire protocol is DERIVED from it
   * (see `protocolForBinding`), so there's no separate `protocol` field. */
  npm?: string;
  /** OpenCode-only: extra models for the picker (`{ id, name }`). */
  models?: { id: string; name: string }[];
  /** Advanced settings merged into the CLI config (same as a Provider's
   * `options`). For Claude this is also where the per-size routing keys
   * `ANTHROPIC_DEFAULT_{SONNET,OPUS,HAIKU}_MODEL` live. */
  options?: { key: string; value: string }[];
  /** Grok-only: `api_backend` for each model entry (see Provider). */
  apiBackend?: string;
};

/** A gateway entry, stored in the unified `providers` array of
 * providers.json discriminated by `kind: "gateway"` (alongside per-CLI
 * providers whose `kind` is "official"/"custom"). A gateway is a kind of
 * provider that fans one `{baseUrl, apiKey}` out to several CLIs. */
export type Gateway = {
  kind: "gateway";
  id: string;
  name: string;
  baseUrl?: string;
  apiKey?: string;
  /** Last auto-detection result (which API modes responded). */
  capabilities?: GatewayCapabilities;
  /** CLIs this gateway is bound to (one entry per CLI). */
  bindings: GatewayBinding[];
  favicon?: string;
};

// ── Official-account subscription quota (backend quota.rs) ──

/** State of the CLI's on-disk OAuth credential (backend `CredentialStatus`). */
export type CredentialStatus =
  | "valid"
  | "expired"
  | "not_found"
  | "parse_error";

/** One rate-limit window. `name` is the API's window id (`five_hour`,
 * `seven_day`, `seven_day_opus`, `seven_day_sonnet`, …) — unknown ids
 * pass through verbatim. `utilization` is the used percentage 0–100. */
export type QuotaTier = {
  /** Window id (`five_hour`, `seven_day`, …) or, for a MODEL-SCOPED
   * window, the model's display name ("Fable"). */
  name: string;
  /** Period a model-scoped window groups under, verbatim from the API
   * (`session` / `weekly` / `monthly`) — set only when `name` is a bare
   * model name, so the label can read "Weekly · Fable". Absent for the
   * account-wide windows, whose `name` already IS the period. */
  group?: string;
  utilization: number;
  resetsAt?: string;
};

/** Pay-as-you-go overflow usage (Claude "usage credits" / grok on-demand). */
export type ExtraUsage = {
  isEnabled: boolean;
  monthlyLimit?: number;
  usedCredits?: number;
  utilization?: number;
  currency?: string;
  /** Decimal places of `monthlyLimit` / `usedCredits`. Claude reports them
   * in MINOR units (cents) with `decimalPlaces: 2`; divide by
   * `10^decimalPlaces` to get the currency amount. Absent → treat as 0. */
  decimalPlaces?: number;
};

/** Result of `fetch_subscription_quota` for one CLI. Credential and
 * network problems land in `credentialStatus` / `error` with
 * `success: false` — the IPC itself only rejects on an unknown app. */
export type SubscriptionQuota = {
  app: CliApp;
  credentialStatus: CredentialStatus;
  success: boolean;
  tiers: QuotaTier[];
  /** Subscription plan display name ("Max" / "Pro" / "Plus" / "Free" …) —
   * brand-ish raw value, not translated. */
  plan?: string;
  extraUsage?: ExtraUsage;
  error?: string;
  queriedAt?: number;
};

/** One saved official-login snapshot (see backend `accounts.rs`). The
 * token payload never leaves the backend; this is the display view. */
export type SavedAccount = {
  id: string;
  name: string;
  email?: string | null;
  plan?: string | null;
  savedAt: string;
  /** True when this snapshot matches the live login. */
  active: boolean;
  /** Set when the last switch+refresh for this account failed — the
   * refresh_token has been revoked and the user must re-authenticate. */
  needsRelogin?: boolean;
};

/** The CLI's current official login, parsed from its live credential. */
export type CurrentAccount = {
  name?: string | null;
  email?: string | null;
  plan?: string | null;
  /** Whether the live login is already captured in `accounts`. */
  saved: boolean;
};

/** Result of `detect_codex_installs` — Codex's two install forms.
 * `cli` gates account add / re-login (they spawn `codex login`);
 * `app` is the desktop app (the merged ChatGPT/Codex app, detected by
 * bundle id `com.openai.codex` — the .app name is unreliable since the
 * 2026-07 merge). Provider management works with EITHER (shared
 * `~/.codex/`), which is what `detect_clis`' codex entry reports. */
export type CodexInstalls = {
  cli: boolean;
  app: boolean;
  appVersion?: string | null;
  /** The runnable codex CLI bundled inside the desktop app
   * (`Contents/Resources/codex`) — login/terminal fallback for
   * app-only installs. Login flows disable only when BOTH `cli` and
   * this are false. */
  bundledCli?: boolean;
};

/** Result of `list_accounts` for one CLI. */
export type AccountsState = {
  current?: CurrentAccount | null;
  accounts: SavedAccount[];
  /** Non-null (the store mode, e.g. `"keyring"`) when the CLI isn't using
   * the file store this feature writes — a file switch may not take
   * effect. Null = default file store. */
  storageWarning?: string | null;
};

export type ActiveKind = "official" | "custom" | "unmanaged";

export type LiveSnapshot = {
  baseUrl?: string | null;
  apiKeyMasked?: string | null;
  model?: string | null;
};

export type ActiveState = {
  app: CliApp;
  kind: ActiveKind;
  matchedProviderId?: string | null;
  liveSnapshot?: LiveSnapshot | null;
  livePath: string;
  // OpenCode-only: ids of Termory providers whose slots exist in
  // opencode.json (i.e. activated). Activated vs default are distinct
  // for OpenCode — multiple slots coexist, only one can be default.
  configuredProviderIds?: string[];
};

export type TestResult = {
  ok: boolean;
  status?: number | null;
  latencyMs: number;
  message: string;
};

export type Route =
  | "records"
  | "search"
  | "stats"
  | "favorites"
  | "providers"
  | "settings";

export type Pane = "sessions" | "memory" | "skills";
