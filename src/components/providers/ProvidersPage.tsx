import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask } from "@tauri-apps/plugin-dialog";
import { Check, Copy, Loader2, Plug, Plus, RadioTower, UserPlus } from "lucide-react";
import { toast } from "sonner";
import { getConfig, invalidateConfigCache } from "@/config";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import {
  ACTIVE_STATE_REFRESH_EVENT,
  CLI_APPS,
  CLI_APP_LABEL,
  CLI_APP_SOURCE_BADGE,
  CODEX_KEEP_ALL_SESSIONS_KEY,
  QUOTA_CHANGED_EVENT
} from "@/constants";
import {
  blankProvider,
  codexVersionSegments,
  hasUpdate,
  isMultiSlot,
  isSourceEnabled,
  providerFromBinding,
  resolveActiveProviderId
} from "@/lib/provider-utils";
import { mergeQuotaResult } from "@/lib/quota-utils";
import type {
  ActiveState,
  CliApp,
  CodexInstalls,
  Provider,
  Gateway,
  SubscriptionQuota,
  TestResult
} from "@/types";
import { BrandIcon } from "@/components/BrandIcon";
import { EmptyState } from "@/components/EmptyState";
import { useT } from "@/i18n";
import { ProviderCard } from "./ProviderCard";
import { ProviderOfficialCard } from "./ProviderOfficialCard";
import { OfficialAccountsSection } from "./OfficialAccountsSection";
import {
  CodexFollowDialog,
  type CodexFollowTarget,
  type RecentCodexProject
} from "./CodexFollowDialog";

const GatewaysPage = React.lazy(() =>
  import("./GatewaysPage").then((m) => ({ default: m.GatewaysPage }))
);

// The tab row mixes the four per-CLI tabs with a separate "Gateways"
// tab. `"gateways"` is not a CliApp, so the active CLI (`app`)
// and the selected view are tracked separately.
const GATEWAYS_TAB = "gateways";

// InstallGuide and ProviderEditor are conditionally rendered (CLI
// missing / editor open), so lazy-load to keep them out of the main
// Providers chunk. Editor is the heavier of the two — it pulls in
// the AI-SDK provider-id catalog, datalist autocomplete, the
// invoke-based test/fetch-models actions, etc.
const InstallGuide = React.lazy(() =>
  import("./InstallGuide").then((m) => ({ default: m.InstallGuide }))
);
const ProviderEditor = React.lazy(() =>
  import("./ProviderEditor").then((m) => ({ default: m.ProviderEditor }))
);

// Stable Codex model_provider ids: Termory writes "termory" for any custom
// provider, and Official is the built-in "openai" bucket.
const CODEX_CUSTOM_PROVIDER_ID = "termory";
const CODEX_OFFICIAL_PROVIDER_ID = "openai";

// Module-level cache for CLI detection results so the OpenCode tab
// doesn't flash "Official → InstallGuide" every time the user
// switches away from Providers and back. ProvidersPage is gated by
// `route === "providers"` in App.tsx and so unmounts on every route
// change; without this cache each remount would briefly render the
// Official card with the optimistic `installed[opencode] = true`
// default before the async detect_clis returned `false`.
//
// Updated by the page itself whenever a fresh detect result lands;
// stays alive across mount/unmount cycles until the window reloads.
// Build a full Record<CliApp,…> from a backend `detect_clis` /
// `detect_cli_versions_cmd` map keyed by CliApp string — keeps the
// per-CLI literals from drifting as new apps are added.
function cliBoolRecord(map: Record<string, boolean>): Record<CliApp, boolean> {
  return Object.fromEntries(CLI_APPS.map((c) => [c, !!map[c]])) as Record<
    CliApp,
    boolean
  >;
}
function cliVersionRecord(
  map: Record<string, string | null>
): Record<CliApp, string | null> {
  return Object.fromEntries(
    CLI_APPS.map((c) => [c, map[c] ?? null])
  ) as Record<CliApp, string | null>;
}

/** Key the Codex DESKTOP app's latest version rides under in the
 * `detect_latest_versions_cmd` map — deliberately not a `CliApp` (it's a
 * second product under the codex tab, versioned independently of the npm
 * CLI), so `cliVersionRecord` passes over it and we read it separately.
 * Mirrors `updates::CODEX_APP_KEY`. */
const CODEX_APP_LATEST_KEY = "codex-app";

let cachedInstalled: Record<CliApp, boolean> = {
  claude: true,
  // Optimistic default (like the other CLIs) — its tab always shows; detect
  // flips this to false on unsupported platforms / no install, which only
  // drives the InstallGuide-vs-provider-list choice, not the tab's visibility.
  "claude-desktop": true,
  codex: true,
  gemini: true,
  opencode: true,
  grok: true
};
let cachedVersions: Record<CliApp, string | null> = {
  claude: null,
  "claude-desktop": null,
  codex: null,
  gemini: null,
  opencode: null,
  grok: null
};
let cachedVersionsLoading = true;
// Latest available versions (from the npm registry / grok's channel
// endpoint via `detect_latest_versions_cmd`). Cached like the installed
// versions so route remounts don't refetch the network; the backend
// additionally caches for 6h.
let cachedLatestVersions: Record<CliApp, string | null> = {
  claude: null,
  "claude-desktop": null,
  codex: null,
  gemini: null,
  opencode: null,
  grok: null
};
let cachedCodexAppLatest: string | null = null;
// Codex's two install forms (CLI binary vs the merged ChatGPT/Codex
// desktop app). Null until the first `detect_codex_installs` resolves;
// cached like the maps above so route remounts don't flash the
// Add-account gate / version line.
let cachedCodexInstalls: CodexInstalls | null = null;
// True once the first `refreshVersions()` of the app lifetime has
// resolved. Used to keep the version skeleton from flashing on every
// route remount — after the first detect, subsequent route entries
// render the cached values silently and only watcher events / Recheck
// trigger a re-fetch (visible flash there is correct: the user
// actually changed something).
let versionsEverResolved = false;

// CLIs whose Official card shows the subscription quota (5-hour /
// weekly rate-limit windows; grok: the weekly/monthly credit window).
// MIRROR of the backend list `quota::SUPPORTED` in src-tauri/src/quota.rs
// (which drives the tray) — when a CLI's fetch_quota arm lands, add it
// in BOTH places.
const QUOTA_SUPPORTED: ReadonlySet<CliApp> = new Set([
  "claude",
  "codex",
  "gemini",
  "grok"
]);

// CLIs whose Official card shows the logged-in account (grok's
// auth.json also carries plain email/first_name/last_name —
// display-only account info via backend `list_grok_accounts`).
const ACCOUNT_SUPPORTED: ReadonlySet<CliApp> = new Set([...QUOTA_SUPPORTED]);
// Quota results survive route remounts (like cachedVersions). An entry
// older than QUOTA_STALE_MS is silently re-fetched on the next entry
// to the tab. Manual Refresh bypasses the stale window but is still
// rate-limited by QUOTA_MIN_INTERVAL_MS (the button shows disabled
// during the cooldown) so it can't hammer the official endpoint.
let cachedQuotas: Partial<Record<CliApp, SubscriptionQuota>> = {};
const QUOTA_STALE_MS = 2 * 60_000;
const QUOTA_MIN_INTERVAL_MS = 120_000;
// A FAILED fetch caches for much less so a transient network error
// doesn't mute the quota display for the full stale window. Applies
// to the auto path, the manual cooldown, and the tray (Rust mirror:
// QUOTA_TRAY_ERROR_RETRY in tray.rs).
const QUOTA_ERROR_RETRY_MS = 60_000;

/** Per-result refresh floor: full cooldown after a success, the short
 * error-retry window after a failure (or no result yet). */
function quotaMinIntervalMs(q?: SubscriptionQuota): number {
  return q?.success ? QUOTA_MIN_INTERVAL_MS : QUOTA_ERROR_RETRY_MS;
}

/** Manual-refresh failure toast, split over two lines: the part before
 * the first ": " (e.g. "HTTP 429 Too Many Requests") as the title, the
 * rest (the API's message) as the description. Errors without that
 * shape show as a single line. */
function quotaErrorToast(error: string) {
  const idx = error.indexOf(": ");
  if (idx > 0) {
    toast.error(error.slice(0, idx), { description: error.slice(idx + 2) });
  } else {
    toast.error(error);
  }
}

// isMultiSlot lives in provider-utils (shared with GatewaysPage + the backend
// set_default dispatch): OpenCode + Grok are multi-slot.

export function ProvidersPage({
  providers,
  setProviders,
  gateways,
  setGateways,
  activeProviderIds,
  setActiveProviderIds,
  app,
  setApp,
  sourceToggles = {},
  sourceOrder,
  traySwitch,
  onTraySwitchDone
}: {
  providers: Provider[];
  setProviders: React.Dispatch<React.SetStateAction<Provider[]>>;
  gateways: Gateway[];
  setGateways: React.Dispatch<React.SetStateAction<Gateway[]>>;
  activeProviderIds: Record<string, string>;
  setActiveProviderIds: React.Dispatch<
    React.SetStateAction<Record<string, string>>
  >;
  app: CliApp;
  setApp: (next: CliApp) => void;
  /** Settings → Tools map (absent key = enabled); disabled apps lose
   *  their tab (App.tsx guards the active tab back to an enabled one). */
  sourceToggles?: Partial<Record<CliApp, boolean>>;
  /** Tab order = the Settings → Tools drag order (App-resolved). */
  sourceOrder?: readonly CliApp[];
  /** A switch the tray parked for us, claimed by App.tsx (always mounted,
   * unlike this route-gated page). Null when nothing is pending. */
  traySwitch?: { app: CliApp; providerId: string | null } | null;
  /** Clear it once we've started running it (take-once). */
  onTraySwitchDone?: () => void;
}) {
  const t = useT();
  // Record / clear the "last activated" marker for a CLI (see
  // resolveActiveProviderId — used to disambiguate identical-creds entries).
  const markActive = React.useCallback(
    (target: CliApp, id: string | null) => {
      setActiveProviderIds((cur) => {
        if (id) return { ...cur, [target]: id };
        if (!(target in cur)) return cur;
        const next = { ...cur };
        delete next[target];
        return next;
      });
    },
    [setActiveProviderIds]
  );
  // Which tab is showing: a per-CLI provider view, or the Gateways view.
  const [view, setView] = React.useState<"providers" | "gateways">("providers");
  // Bumped when the header "+" is clicked in the Gateways view — the
  // header lives here, but the gateway editor state lives in GatewaysPage,
  // so this signal tells it to open a fresh "add gateway" form.
  const [gatewayAddSignal, setGatewayAddSignal] = React.useState(0);
  const [editing, setEditing] = React.useState<Provider | null>(null);
  const [editingIsNew, setEditingIsNew] = React.useState(false);
  const [activeStates, setActiveStates] = React.useState<Record<CliApp, ActiveState | null>>({
    claude: null,
    "claude-desktop": null,
    codex: null,
    gemini: null,
    opencode: null,
    grok: null
  });
  // Initialize from the module-level cache so a remount (route
  // switch) renders with the last-known truth, not the optimistic
  // default. The cache is written back from inside the refresh
  // helpers below.
  const [installed, setInstalled] =
    React.useState<Record<CliApp, boolean>>(cachedInstalled);
  const [versions, setVersions] =
    React.useState<Record<CliApp, string | null>>(cachedVersions);
  const [versionsLoading, setVersionsLoading] = React.useState(
    cachedVersionsLoading
  );
  const [codexInstalls, setCodexInstalls] = React.useState<CodexInstalls | null>(
    cachedCodexInstalls
  );
  const [latestVersions, setLatestVersions] =
    React.useState<Record<CliApp, string | null>>(cachedLatestVersions);
  const [codexAppLatest, setCodexAppLatest] = React.useState<string | null>(
    cachedCodexAppLatest
  );

  // Mirror state into the module-level cache on every change so the
  // next mount has the fresh truth as its initial value.
  React.useEffect(() => {
    cachedInstalled = installed;
  }, [installed]);
  React.useEffect(() => {
    cachedVersions = versions;
  }, [versions]);
  React.useEffect(() => {
    cachedVersionsLoading = versionsLoading;
  }, [versionsLoading]);
  React.useEffect(() => {
    cachedCodexInstalls = codexInstalls;
  }, [codexInstalls]);
  React.useEffect(() => {
    cachedLatestVersions = latestVersions;
  }, [latestVersions]);
  React.useEffect(() => {
    cachedCodexAppLatest = codexAppLatest;
  }, [codexAppLatest]);
  const [toggling, setToggling] = React.useState<string | null>(null);
  const [testing, setTesting] = React.useState<string | null>(null);
  const [settingDefault, setSettingDefault] = React.useState<string | null>(null);
  // Codex "Add account" — state lives here so the button can live in the card.
  const [codexLoggingIn, setCodexLoggingIn] = React.useState(false);
  const [codexAccountTrigger, setCodexAccountTrigger] = React.useState(0);
  const [activeReloginId, setActiveReloginId] = React.useState<string | null>(null);
  const [codexLoginUrl, setCodexLoginUrl] = React.useState<string | null>(null);
  const [codexLoginUrlCopied, setCodexLoginUrlCopied] = React.useState(false);
  // When set, the switch-time Codex "bring sessions along?" picker is open.
  // The prompt appears BEFORE activation; `activate` runs only after the user
  // decides (bring-along moves the selected projects first, then activates).
  const [followTarget, setFollowTarget] = React.useState<CodexFollowTarget | null>(null);
  const [rechecking, setRechecking] = React.useState(false);

  // Official-account quota (5h / weekly windows), per CLI. Seeded from
  // the module cache so a route remount shows the last result without
  // a skeleton flash; refreshed when stale or on manual Refresh.
  const [quotas, setQuotas] = React.useState<
    Partial<Record<CliApp, SubscriptionQuota>>
  >(cachedQuotas);
  const [quotaLoading, setQuotaLoading] = React.useState<CliApp | null>(null);
  const quotaLoadingRef = React.useRef<CliApp | null>(null);

  // `manual: true` = the user clicked Refresh — a failure surfaces as
  // a toast with the backend's raw error. Background fetches (tab
  // entry, stale re-fetch) fail silently and just keep the old data.
  const refreshQuota = React.useCallback(async (target: CliApp, manual = false) => {
    if (!QUOTA_SUPPORTED.has(target)) return;
    if (quotaLoadingRef.current === target) return; // fetch in flight
    // Rate limit: never query the official endpoint more often than
    // the per-result floor (shorter after a failure), regardless of
    // which path called us.
    const lastResult = cachedQuotas[target];
    if (
      lastResult?.queriedAt &&
      Date.now() - lastResult.queriedAt < quotaMinIntervalMs(lastResult)
    ) {
      return;
    }
    quotaLoadingRef.current = target;
    setQuotaLoading(target);
    try {
      const result = await invoke<SubscriptionQuota>(
        "fetch_subscription_quota",
        { app: target }
      );
      setQuotas((cur) => {
        // Failures merge with the previous entry instead of wiping the
        // displayed tiers/reset times — see quota-utils mergeQuotaResult.
        const next = { ...cur, [target]: mergeQuotaResult(cur[target], result) };
        cachedQuotas = next;
        return next;
      });
      if (manual && !result.success && result.error) {
        quotaErrorToast(result.error);
      }
    } catch (err) {
      // unknown-app guard only — leave the previous result on screen
      if (manual) toast.error(String(err));
    } finally {
      quotaLoadingRef.current = null;
      setQuotaLoading(null);
    }
  }, []);

  // Backend-initiated quota results (tray click, watcher
  // credential-change — e.g. the user just ran `claude login`) arrive
  // as events; store them so the page reflects a login/logout without
  // its own request. IPC-initiated fetches echo here too (same data).
  React.useEffect(() => {
    const unlisten = listen<SubscriptionQuota>(QUOTA_CHANGED_EVENT, (event) => {
      const result = event.payload;
      if (!result?.app) return;
      setQuotas((cur) => {
        // Out-of-order guard: with concurrent fetches (page + tray),
        // a slower fetch's payload can arrive after a newer result —
        // never roll the entry back to an older snapshot.
        const prev = cur[result.app];
        if (
          prev?.queriedAt &&
          result.queriedAt &&
          result.queriedAt < prev.queriedAt
        ) {
          return cur;
        }
        const next = {
          ...cur,
          [result.app]: mergeQuotaResult(prev, result)
        };
        cachedQuotas = next;
        return next;
      });
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  // Manual-refresh cooldown for the CURRENT tab's quota (shorter
  // window after a failed fetch). Derived from `queriedAt`; the
  // timeout re-renders once the cooldown lapses so the Refresh button
  // re-enables without user interaction.
  const quotaEntry = quotas[app];
  const quotaQueriedAt = quotaEntry?.queriedAt;
  const quotaCooldownMs = quotaMinIntervalMs(quotaEntry);
  const [, quotaTick] = React.useReducer((n: number) => n + 1, 0);
  React.useEffect(() => {
    if (!quotaQueriedAt) return;
    const remain = quotaCooldownMs - (Date.now() - quotaQueriedAt);
    if (remain <= 0) return;
    const id = setTimeout(quotaTick, remain + 50);
    return () => clearTimeout(id);
  }, [quotaQueriedAt, quotaCooldownMs]);
  const quotaInCooldown =
    !!quotaQueriedAt && Date.now() - quotaQueriedAt < quotaCooldownMs;

  // Gateway-binding gate = installed AND enabled (Settings → Tools): a
  // disabled tool must not be offered as a binding target, mirroring its
  // hidden provider tab. GatewaysPage threads this into GatewayEditor.
  const bindableInstalled = React.useMemo(
    () =>
      Object.fromEntries(
        CLI_APPS.map((a) => [a, installed[a] && isSourceEnabled(sourceToggles, a)])
      ) as Record<CliApp, boolean>,
    [installed, sourceToggles]
  );
  // Binding rows LISTED in the GatewayEditor — a disabled tool's row is
  // hidden entirely (vs installed-gating, which only dims it).
  const enabledApps = React.useMemo(
    () => CLI_APPS.filter((a) => isSourceEnabled(sourceToggles, a)),
    [sourceToggles]
  );

  // Codex is "installed" with just the desktop app (shared ~/.codex).
  // Account add / re-login spawn `codex login` — they need EITHER the
  // standalone CLI or the app's bundled copy (the backend spawn falls
  // back to it). Null (not yet detected) stays permissive; the login
  // handler re-checks and toasts.
  const codexCliMissing =
    codexInstalls != null && !codexInstalls.cli && !codexInstalls.bundledCli;

  const refreshInstalled = React.useCallback(async () => {
    try {
      const map = await invoke<Record<string, boolean>>("detect_clis");
      setInstalled(cliBoolRecord(map));
    } catch {
      /* leave previous state on error */
    }
  }, []);

  // Heavier than refreshInstalled — spawns 4 `<bin> --version`
  // subprocesses. Called on first cold-start mount, on Recheck, and
  // when the watcher reports an install/uninstall. Subsequent route
  // remounts read straight from the module-level cache without
  // re-firing this.
  //
  // Only flips `versionsLoading` to `true` on the very first attempt
  // of the app lifetime. After that, refreshes update the value
  // silently — no skeleton flash — because the user already has
  // accurate numbers on screen.
  const refreshVersions = React.useCallback(async () => {
    if (!versionsEverResolved) {
      setVersionsLoading(true);
    }
    try {
      // The codex cli/app split rides along — same triggers (cold
      // start, Recheck, install change), one skeleton cycle.
      const [map, codex] = await Promise.all([
        invoke<Record<string, string | null>>("detect_cli_versions_cmd"),
        invoke<CodexInstalls>("detect_codex_installs")
      ]);
      setVersions(cliVersionRecord(map));
      setCodexInstalls(codex);
    } catch {
      /* leave previous state on error */
    } finally {
      versionsEverResolved = true;
      setVersionsLoading(false);
    }
  }, []);

  // Latest available versions (network). Runs independently of
  // `refreshVersions` so the slower fetch never blocks the installed-
  // version render; the update badge simply appears once it resolves.
  // `force` bypasses the backend's 6h cache (Recheck).
  const refreshLatestVersions = React.useCallback(async (force = false) => {
    try {
      const map = await invoke<Record<string, string | null>>(
        "detect_latest_versions_cmd",
        { force }
      );
      setLatestVersions(cliVersionRecord(map));
      setCodexAppLatest(map[CODEX_APP_LATEST_KEY] ?? null);
    } catch {
      /* network best-effort — leave previous state on error */
    }
  }, []);

  const handleRecheckInstall = async () => {
    setRechecking(true);
    try {
      const map = await invoke<Record<string, boolean>>("detect_clis");
      const next = cliBoolRecord(map);
      setInstalled(next);
      if (next[app]) {
        toast.success(t("toast.detected", { app: CLI_APP_LABEL[app] }));
        void refreshVersions();
        void refreshLatestVersions(true);
      } else {
        toast.error(t("toast.notInstalled", { app: CLI_APP_LABEL[app] }));
      }
    } catch (err) {
      toast.error(t("toast.detectionFailed", { error: String(err) }));
    } finally {
      setRechecking(false);
    }
  };

  // Pre-action gate: re-check whether the target CLI is installed
  // right now. Returns true to proceed, false to abort (toast already
  // shown). Used by the action handlers that actually need the CLI
  // binary to consume the live config we're about to mutate.
  const ensureCliInstalled = async (target: CliApp): Promise<boolean> => {
    try {
      const map = await invoke<Record<string, boolean>>("detect_clis");
      setInstalled(cliBoolRecord(map));
      if (!map[target]) {
        toast.error(
          t("toast.notInstalledFull", { app: CLI_APP_LABEL[target] })
        );
        return false;
      }
      return true;
    } catch {
      return true; // detection failed — don't block the user
    }
  };

  // All gateway bindings materialized as providers (one per binding). They
  // share the per-CLI active-state derivation so a gateway-activated CLI
  // matches the synthesized id instead of reading as "Unmanaged".
  const gatewaySynth = React.useMemo(
    () => gateways.flatMap((r) => r.bindings.map((b) => providerFromBinding(r, b))),
    [gateways]
  );

  const refreshActive = React.useCallback(async () => {
    try {
      // Pass standalone providers + gateway-binding synths; the per-CLI
      // "in use" highlight is disambiguated client-side via the activation
      // marker (resolveActiveProviderId) when several share identical creds.
      const states = await invoke<ActiveState[]>("provider_active_states", {
        providers: [...providers, ...gatewaySynth]
      });
      const next: Record<CliApp, ActiveState | null> = {
        claude: null,
        "claude-desktop": null,
        codex: null,
        gemini: null,
        opencode: null,
    grok: null
      };
      for (const s of states) next[s.app] = s;
      setActiveStates(next);
    } catch (err) {
      toast.error(t("toast.readStateFailed", { error: String(err) }));
    }
  }, [providers, gatewaySynth]);

  // Re-read the per-CLI activation markers from disk. The TRAY writes them
  // itself when switching (mirroring `markActive`), and a backend write can't
  // invalidate our module cache — so without this the page keeps a stale
  // marker and, when a standalone provider and a gateway binding share creds,
  // labels the wrong one "in use". Bails out when nothing changed so the
  // write-back effect in usePersistentState doesn't fire a pointless write.
  const refreshMarkers = React.useCallback(async () => {
    invalidateConfigCache();
    let stored: Record<string, string> = {};
    try {
      stored = (await getConfig<Record<string, string>>("active_provider_ids")) ?? {};
    } catch {
      return;
    }
    setActiveProviderIds((cur) =>
      JSON.stringify(cur) === JSON.stringify(stored) ? cur : stored
    );
  }, [setActiveProviderIds]);

  // Re-derive on mount AND whenever the visible tab changes — switching
  // from the Gateways tab (where a binding may have just been activated)
  // back to a CLI tab must pick up that change in the per-CLI list.
  React.useEffect(() => {
    void refreshActive();
  }, [refreshActive, view, app]);

  // Fetch the official quota when entering a supported CLI's tab.
  // Cached results younger than QUOTA_STALE_MS render as-is (no
  // network hit on every tab flip); the card's Refresh button calls
  // `refreshQuota` directly for an unconditional re-fetch.
  React.useEffect(() => {
    if (view !== "providers" || !QUOTA_SUPPORTED.has(app)) return;
    if (!installed[app]) return;
    const cached = cachedQuotas[app];
    // Successful results cache for the full stale window; failures
    // only for the short error-retry window.
    const staleMs = cached?.success ? QUOTA_STALE_MS : QUOTA_ERROR_RETRY_MS;
    if (cached?.queriedAt && Date.now() - cached.queriedAt < staleMs) {
      return;
    }
    void refreshQuota(app);
  }, [view, app, installed, refreshQuota]);

  React.useEffect(() => {
    void refreshInstalled();
    // First-mount-of-app-lifetime gate. Route remounts read the
    // cached versions instantly; only the first cold start (or a
    // page reload) pays the subprocess cost. After that, watcher
    // events and manual Recheck are the only paths that re-fire
    // `refreshVersions`.
    if (!versionsEverResolved) {
      void refreshVersions();
      // Latest-version check rides the same cold-start gate. Network,
      // backend-cached 6h; route remounts skip it.
      void refreshLatestVersions();
    }
  }, [refreshInstalled, refreshVersions, refreshLatestVersions]);

  // Event-driven install + version refresh — no polling. Triggers:
  //   1. Rust watcher fires `cli-install-changed` when any CLI binary
  //      dir / node-version-manager root mutates — install, uninstall,
  //      OR an in-place UPGRADE (the watcher matches on path, not event
  //      kind, so rewriting an existing binary fires it too).
  //   2. Tauri window gains focus — a cheap bool re-check, covering an
  //      install/uninstall the OS didn't emit an FS event for.
  //   3. (Already wired above) Page mount + manual Recheck.
  //
  // `detect_clis` is pure stat (~10ms), so it runs on every trigger. The
  // heavier version probe (`detect_cli_versions`, 4 `--version` spawns)
  // runs when the installed bool FLIPS (install/uninstall) OR on a watcher
  // event — the watcher path is what makes an in-place upgrade (bool
  // unchanged, version changed) update the number + clear the badge, the
  // same way install/uninstall already do. Each trigger probes versions
  // EXACTLY ONCE (no double-fetch); on focus the probe runs only if the
  // bool actually flipped, so frequent window switches don't spawn.
  const installedRef = React.useRef(installed);
  installedRef.current = installed;
  React.useEffect(() => {
    // Update the install bool; returns whether it flipped so each caller
    // decides on its own whether a version re-probe is warranted (no
    // version fetch happens in here — that keeps it to exactly one).
    const refresh = async (): Promise<boolean> => {
      try {
        const map = await invoke<Record<string, boolean>>("detect_clis");
        const next = cliBoolRecord(map);
        const prev = installedRef.current;
        const changed = CLI_APPS.some((c) => prev[c] !== next[c]);
        if (changed) setInstalled(next);
        return changed;
      } catch {
        /* leave previous state on transient error */
        return false;
      }
    };
    const unlistenPromise = listen("termory:cli-install-changed", () => {
      // A watched binary dir changed — install, uninstall, OR an in-place
      // upgrade. Update the bool and re-probe the INSTALLED version once
      // (unconditional because an upgrade doesn't flip the bool). The
      // latest-version (upstream) value is unaffected by a local (un)install
      // /upgrade, so it's NOT refetched here — it's owned by cold-start +
      // Recheck, keeping unrelated bin-dir churn off the network.
      void refresh();
      void refreshVersions();
    });
    const win = getCurrentWindow();
    const focusPromise = win.onFocusChanged(({ payload: focused }) => {
      // Cheap bool re-check on focus (covers an install/uninstall the OS
      // didn't emit an FS event for); re-probe versions ONLY if it truly
      // flipped. An in-place upgrade is already handled by the watcher.
      if (focused) {
        void refresh().then((changed) => {
          if (changed) void refreshVersions();
        });
      }
    });
    return () => {
      void unlistenPromise.then((fn) => fn()).catch(() => {});
      void focusPromise.then((fn) => fn()).catch(() => {});
    };
  }, [refreshVersions]);

  // Auto refresh when the Rust watcher detects any change in the
  // CLI dirs (live config files live inside those dirs). Reuse the
  // existing `termory:sources-changed` event — payload is ignored.
  React.useEffect(() => {
    const unlistenPromise = listen("termory:sources-changed", () => {
      void refreshActive();
    });
    // The menu-bar tray switches providers via its own handler and
    // emits this after writing the CLI's live config — re-derive so an
    // open Providers page reflects a tray switch even when unfocused. The
    // tray also wrote the activation marker, so re-read that too (the
    // creds-collision label depends on it).
    const unlistenTrayPromise = listen("termory:providers-changed", () => {
      void refreshMarkers();
      void refreshActive();
    });
    const peerHandler = () => void refreshActive();
    window.addEventListener(ACTIVE_STATE_REFRESH_EVENT, peerHandler);
    return () => {
      void unlistenPromise.then((fn) => fn()).catch(() => {});
      void unlistenTrayPromise.then((fn) => fn()).catch(() => {});
      window.removeEventListener(ACTIVE_STATE_REFRESH_EVENT, peerHandler);
    };
  }, [refreshActive, refreshMarkers]);

  const providersForApp = React.useMemo(
    () => providers.filter((p) => p.app === app),
    [providers, app]
  );
  const customProviders = React.useMemo(
    () => providersForApp.filter((p) => p.kind === "custom"),
    [providersForApp]
  );
  // Gateway bindings targeting the current CLI, surfaced in its provider
  // list (requirement: a bound gateway shows up under the CLI). Managed
  // from the Gateways tab — Edit jumps there; Delete unbinds.
  const gatewayBoundForApp = React.useMemo(
    () =>
      gateways.flatMap((gateway) =>
        gateway.bindings
          .filter((b) => b.app === app)
          .map((binding) => ({
            gateway,
            binding,
            synth: providerFromBinding(gateway, binding)
          }))
      ),
    [gateways, app]
  );
  // Standalone providers + gateway-binding synths for this app — the full set
  // Grok's set_grok_default needs to strip the previous default's global
  // Advanced settings before applying the new default's.
  const allProvidersForApp = React.useMemo(
    () => [...providersForApp, ...gatewayBoundForApp.map((g) => g.synth)],
    [providersForApp, gatewayBoundForApp]
  );
  const activeState = activeStates[app];
  // The "in use" id. OpenCode's matchedProviderId is resolved by the live
  // default-slot id (not by creds), so it's already unambiguous — running it
  // through the creds-collision marker only adds risk there (a stale marker
  // + two identical-creds slots could mismatch). Single-slot CLIs reverse-
  // derive by creds, so they genuinely need the marker to disambiguate a
  // standalone provider and a gateway binding that share creds.
  const effectiveActiveId = React.useMemo(
    () =>
      isMultiSlot(app)
        ? (activeState?.matchedProviderId ?? null)
        : resolveActiveProviderId(activeState, activeProviderIds[app], [
            ...customProviders,
            ...gatewayBoundForApp.map((g) => g.synth)
          ]),
    [activeState, activeProviderIds, app, customProviders, gatewayBoundForApp]
  );

  // Activate a gateway binding's synthesized provider via the normal path.
  // Returns true on success (the Codex follow dialog migrates only after a
  // landed activation).
  const performActivateGateway = async (synth: Provider): Promise<boolean> => {
    if (!(await ensureCliInstalled(synth.app))) return false;
    setSettingDefault(synth.id);
    try {
      await invoke("activate_provider", {
        provider: synth,
        providersForApp: [synth]
      });
      if (isMultiSlot(synth.app)) {
        await invoke("set_default_provider", {
          provider: synth,
          providersForApp: allProvidersForApp
        });
      }
      markActive(synth.app, synth.id);
      toast.success(t("toast.nowInUse", { name: synth.name || t("providers.unnamed") }));
      await refreshActive();
      return true;
    } catch (err) {
      toast.error(String(err));
      return false;
    } finally {
      setSettingDefault(null);
    }
  };

  // A Codex gateway binding activates into the same custom "termory" bucket as
  // a standalone custom provider, so an official→gateway switch hides resume
  // history too — prompt first, same as setAsDefault.
  const activateGateway = async (synth: Provider) => {
    if (synth.app === "codex" && effectiveActiveId === null) {
      await maybePromptThenActivate({
        providerId: CODEX_CUSTOM_PROVIDER_ID,
        label: synth.name || t("providers.unnamed"),
        activate: () => performActivateGateway(synth)
      });
      return;
    }
    await performActivateGateway(synth);
  };

  const toggleGatewayEnabled = async (synth: Provider) => {
    if (!isMultiSlot(synth.app)) return;
    if (!(await ensureCliInstalled(synth.app))) return;
    const enabled = (activeStates[synth.app]?.configuredProviderIds ?? []).includes(
      synth.id
    );
    setToggling(synth.id);
    try {
      if (enabled) {
        await invoke("delete_provider", { provider: synth });
      } else {
        await invoke("activate_provider", {
          provider: synth,
          providersForApp: [synth]
        });
      }
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setToggling(null);
    }
  };

  const startNew = () => {
    setEditing(blankProvider(app));
    setEditingIsNew(true);
  };
  const startEdit = (p: Provider) => {
    setEditing({ ...p });
    setEditingIsNew(false);
  };
  const closeEditor = () => {
    setEditing(null);
    setEditingIsNew(false);
  };

  const saveProvider = async (next: Provider) => {
    const prev = providers.find((p) => p.id === next.id);
    const updated = prev
      ? providers.map((p) => (p.id === next.id ? next : p))
      : [...providers, next];
    setProviders(updated);
    closeEditor();

    // Saving only updates providers.json. If the edited provider is the
    // one currently live for its CLI, re-activate so the change (model /
    // base URL / key / options / …) actually reaches the live config —
    // otherwise the edit silently doesn't take effect until the user
    // manually re-activates. New providers and edits to an inactive
    // provider don't touch any live config.
    const state = activeStates[next.app];
    const isLive =
      isMultiSlot(next.app)
        ? (state?.configuredProviderIds ?? []).includes(next.id)
        : state?.matchedProviderId === next.id;
    if (next.kind !== "custom" || !isLive) return;

    // The backend strips the union of all providers' CURRENT option keys
    // before re-applying — so a key the edit REMOVED is no longer in the
    // union and would be orphaned in the live config. Fold the previous
    // version of this provider into the strip set (backend only reads its
    // option keys; a duplicate id is harmless) so removed keys get
    // cleaned. The applied provider (`next`) carries only current keys,
    // so removed ones are stripped and not written back.
    const stripSet = updated.filter((p) => p.app === next.app);
    if (prev) stripSet.push(prev);
    try {
      await invoke("activate_provider", {
        provider: next,
        providersForApp: stripSet
      });
      if (isMultiSlot(next.app)) {
        // Re-affirm the picker default ONLY if it was ALREADY the default.
        // Saving an enabled-but-not-default slot just re-applies its entries —
        // it must NOT be promoted to default (and the marker stays put).
        if (state?.matchedProviderId === next.id) {
          await invoke("set_default_provider", {
          provider: next,
          providersForApp: allProvidersForApp
        });
          markActive(next.app, next.id);
        }
      } else {
        // Single-slot: re-activating IS the live/default, so mark it.
        markActive(next.app, next.id);
      }
      await refreshActive();
    } catch (err) {
      toast.error(
        t("toast.savedButFailedApp", { app: CLI_APP_LABEL[next.app], error: String(err) })
      );
    }
  };

  const deleteProvider = async (id: string) => {
    const target = providers.find((p) => p.id === id);
    if (!target) return;
    // Tauri's native confirm dialog — feels at home on each OS
    // (macOS sheet, Windows MessageBox, Linux GTK) and the Delete
    // button is highlighted as the destructive one. Replaces the
    // browser `window.confirm` which used to render an out-of-place
    // generic "OK / Cancel" alert.
    const confirmed = await ask(
      t("providers.deleteProviderMsg", { name: target.name || t("providers.thisProvider") }),
      {
        title: t("providers.deleteProvider"),
        kind: "warning",
        okLabel: t("providers.delete"),
        cancelLabel: t("providers.cancel")
      }
    );
    if (!confirmed) return;
    const isInUse = activeStates[target.app]?.matchedProviderId === id;
    try {
      if (isMultiSlot(target.app)) {
        // Multi-slot (OpenCode / Grok) — delete only THIS provider's
        // entries (and the default pointer if it referenced them).
        // Other Termory slots stay intact.
        await invoke("delete_provider", { provider: target });
      } else if (isInUse) {
        // Single-slot CLIs — when the deleted one is the live record,
        // full deactivate clears Termory's writes so the CLI falls
        // back to its native auth.
        await invoke("deactivate_provider", {
          app: target.app,
          providersForApp: providers.filter((p) => p.app === target.app)
        });
      }
    } catch (err) {
      toast.error(t("toast.clearFailed", { app: CLI_APP_LABEL[target.app], error: String(err) }));
      return;
    }
    setProviders((cur) => cur.filter((p) => p.id !== id));
    if (activeProviderIds[target.app] === id) markActive(target.app, null);
    await refreshActive();
  };

  // OpenCode-only: toggle the provider's slot in opencode.json.
  // Enabled means the slot exists (multi-slot coexist). Other CLIs
  // don't have this concept — they only have "Set as default".
  const toggleEnabled = async (target: Provider) => {
    if (!isMultiSlot(target.app)) return;
    if (!(await ensureCliInstalled(target.app))) return;
    const state = activeStates[target.app];
    const enabled = (state?.configuredProviderIds ?? []).includes(target.id);
    setToggling(target.id);
    try {
      if (enabled) {
        await invoke("delete_provider", { provider: target });
        toast.success(t("toast.disabled", { name: target.name || t("providers.unnamed") }));
      } else {
        await invoke("activate_provider", {
          provider: target,
          providersForApp
        });
        toast.success(t("toast.enabled", { name: target.name || t("providers.unnamed") }));
      }
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setToggling(null);
    }
  };

  // The actual activation — runs AFTER the Codex follow prompt is resolved
  // (or immediately for non-Codex CLIs).
  // Returns true on success so the Codex follow dialog only migrates after the
  // activation actually landed (migrating to a bucket that never activated
  // would hide the sessions instead of revealing them).
  const performSetAsDefault = async (target: Provider): Promise<boolean> => {
    if (!(await ensureCliInstalled(target.app))) return false;
    setSettingDefault(target.id);
    try {
      // OpenCode: ensure the slot exists first (auto-enable) — the user can
      // hit "Set as default" on a not-yet-enabled provider, and
      // set_opencode_default errors on a missing slot. Single-slot CLIs:
      // activating IS setting the default.
      if (isMultiSlot(target.app)) {
        await invoke("activate_provider", {
          provider: target,
          providersForApp
        });
        await invoke("set_default_provider", {
          provider: target,
          providersForApp: allProvidersForApp
        });
      } else {
        await invoke("activate_provider", {
          provider: target,
          providersForApp
        });
      }
      markActive(target.app, target.id);
      toast.success(t("toast.nowInUse", { name: target.name || t("providers.unnamed") }));
      await refreshActive();
      return true;
    } catch (err) {
      toast.error(String(err));
      return false;
    } finally {
      setSettingDefault(null);
    }
  };

  // Decide whether the Codex follow prompt is warranted, then either prompt or
  // activate directly. Only an official↔custom transition changes the
  // model_provider bucket (all custom providers share the "termory" id), so
  // only those transitions can hide resume history. We then keep only the
  // recent projects whose sessions are NOT already on the target bucket — a
  // project already tagged with the target provider has nothing to migrate. If
  // none remain, activate directly without prompting. Order on confirm:
  // activate first, then migrate (handled in the dialog).
  const maybePromptThenActivate = async (
    base: Omit<CodexFollowTarget, "projects">
  ) => {
    let projects: RecentCodexProject[] = [];
    try {
      // limit 0 = no cap — return every project, newest first; the dialog
      // scrolls. So a project the user wants is never pushed out by a hard cap.
      projects = await invoke<RecentCodexProject[]>("recent_codex_projects", {
        limit: 0
      });
    } catch {
      projects = [];
    }
    // Only projects with at least one session whose provider differs from the
    // target are migration candidates.
    const candidates = projects.filter((p) =>
      p.providers.some((id) => id !== base.providerId)
    );
    if (candidates.length === 0) {
      await base.activate();
      return;
    }
    // Settings → "follow all projects silently": the user opted out of being
    // asked, so switch and re-tag everything. Applies here too, not just to the
    // tray, so the setting means the same thing wherever you switch from. Read
    // per switch (not cached in state) so a change made while this page is open
    // takes effect immediately — the Rust side reads the same key per switch.
    invalidateConfigCache();
    const keepSessions =
      (await getConfig<boolean>(CODEX_KEEP_ALL_SESSIONS_KEY).catch(() => false)) === true;
    if (keepSessions) {
      const activated = await base.activate();
      if (!activated) return;
      try {
        const moved = await invoke<{ moved: number }>("follow_codex_sessions", {
          projects: candidates.map((p) => p.project),
          targetProviderId: base.providerId
        });
        toast.success(t("providers.followDone", { count: String(moved.moved) }));
      } catch (err) {
        toast.error(String(err));
      }
      return;
    }
    setFollowTarget({ ...base, projects: candidates });
  };

  // Bridge for the Gateways tab: activate/deactivate a binding there calls the
  // CLI write directly and would skip the Codex follow prompt the Providers tab
  // applies. GatewaysPage calls this for a Codex binding on a bucket-changing
  // switch — `"toCustom"` (official→custom, activate) or `"toOfficial"`
  // (custom→official, deactivate) — so the prompt fires there too (the dialog
  // is rendered here, regardless of which tab is open).
  const codexFollowForBinding = (
    direction: "toCustom" | "toOfficial",
    label: string,
    activate: () => Promise<boolean>
  ) =>
    maybePromptThenActivate({
      providerId:
        direction === "toOfficial"
          ? CODEX_OFFICIAL_PROVIDER_ID
          : CODEX_CUSTOM_PROVIDER_ID,
      label,
      activate
    });

  // Universal "Set as default" — promotes a provider to "In use". For a Codex
  // official→custom switch we prompt first (see maybePromptThenActivate).
  const setAsDefault = async (target: Provider) => {
    if (target.app === "codex" && effectiveActiveId === null) {
      await maybePromptThenActivate({
        providerId: CODEX_CUSTOM_PROVIDER_ID,
        label: target.name || t("providers.unnamed"),
        activate: () => performSetAsDefault(target)
      });
      return;
    }
    await performSetAsDefault(target);
  };

  // The actual Official switch — clears Termory writes so the CLI falls back
  // to its native auth flow. Runs after the Codex prompt resolves.
  const performOfficial = async (): Promise<boolean> => {
    if (!(await ensureCliInstalled(app))) return false;
    setSettingDefault("__official__");
    try {
      await invoke("deactivate_provider", {
        app,
        // Include gateway-binding synths so OpenCode's deactivate can
        // recognize (and clear) a top-level default that points at a
        // gateway binding's slot — otherwise switching back to Official
        // silently no-ops when a gateway binding was the default.
        providersForApp: [
          ...providersForApp,
          ...gatewayBoundForApp.map((g) => g.synth)
        ]
      });
      markActive(app, null);
      toast.success(t("toast.officialInUse", { app: CLI_APP_LABEL[app] }));
      await refreshActive();
      return true;
    } catch (err) {
      toast.error(String(err));
      return false;
    } finally {
      setSettingDefault(null);
    }
  };

  // Official "Set as default". For a Codex custom→official switch we prompt
  // first — switching back to the "openai" bucket can hide sessions that were
  // moved to a custom provider, so offer to bring them back.
  const setOfficialAsDefault = async () => {
    if (app === "codex" && effectiveActiveId !== null) {
      await maybePromptThenActivate({
        providerId: CODEX_OFFICIAL_PROVIDER_ID,
        label: t("providers.official"),
        activate: () => performOfficial()
      });
      return;
    }
    await performOfficial();
  };

  // ── Switch handed over by the tray ──────────────────────────────────────
  // A native menu can't host the Codex "follow sessions?" dialog, so for an
  // official↔custom Codex switch the tray writes NOTHING and parks the request,
  // then shows this app (even if its window was closed). App.tsx claims it — it
  // is always mounted, whereas this page only exists on the Providers route —
  // and hands it down.
  //
  // We prompt UNCONDITIONALLY and do NOT re-derive the direction: the tray
  // already established that the bucket changes (the only case it parks), and
  // re-deriving read `effectiveActiveId`, whose marker map is still `{}` right
  // after the window opened, as "already Official" — skipping the prompt.
  //
  // `startedRef` guards against running the same request twice: React
  // StrictMode invokes effect bodies twice in dev, and the queued
  // `onTraySwitchDone` is not yet visible to that second pass — without it a
  // `codex_keep_all_sessions` switch would activate and re-tag twice. It holds the
  // request OBJECT, not a value token: App.tsx mints a fresh object per claim,
  // so repeating the very same switch (click Official, cancel the dialog, click
  // Official again) is a new object and runs, while a re-render of the one
  // in-flight request is skipped. Keying on `app:providerId` instead silently
  // dropped that second, identical click.
  const traySwitchStartedRef = React.useRef<object | null>(null);
  React.useEffect(() => {
    if (!traySwitch) return;
    if (app !== traySwitch.app) {
      setView("providers");
      setApp(traySwitch.app);
      return;
    }
    const { providerId } = traySwitch;
    if (traySwitchStartedRef.current === traySwitch) return;

    if (providerId === null) {
      traySwitchStartedRef.current = traySwitch;
      onTraySwitchDone?.();
      void maybePromptThenActivate({
        providerId: CODEX_OFFICIAL_PROVIDER_ID,
        label: t("providers.official"),
        activate: () => performOfficial()
      });
      return;
    }
    // The library arrives via its own async read, so on a cold start (tray-only
    // launch) this list can still be empty. Consume the request only once the
    // target is actually resolved — clearing it first would discard the switch
    // permanently, with no activation and no error.
    const target = [...customProviders, ...gatewayBoundForApp.map((g) => g.synth)].find(
      (p) => p.id === providerId
    );
    if (!target) return;
    traySwitchStartedRef.current = traySwitch;
    onTraySwitchDone?.();
    void maybePromptThenActivate({
      providerId: CODEX_CUSTOM_PROVIDER_ID,
      label: target.name || t("providers.unnamed"),
      activate: () => performSetAsDefault(target)
    });
    // The handlers are re-created each render; the ref + take-once above make
    // this run exactly once per parked request.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [traySwitch, app, customProviders, gatewayBoundForApp, setApp]);

  // Unified handler for both "Add Account" (reloginId=undefined) and
  // "Re-login" (reloginId=the saved account's id). Mutual exclusion is
  // enforced by the codexLoggingIn guard so both buttons share one lock.
  const handleCodexLogin = async (reloginId?: string) => {
    if (codexLoggingIn) return;
    // Login spawns a codex binary (standalone CLI or the desktop app's
    // bundled copy) — bail only when neither exists.
    if (codexCliMissing) {
      toast.error(t("providers.accountAddNeedsCli"));
      return;
    }
    setCodexLoggingIn(true);
    setActiveReloginId(reloginId ?? null);
    setCodexLoginUrl(null);
    setCodexLoginUrlCopied(false);
    const unlisten = await listen<string>("codex:login-url", (event) => {
      setCodexLoginUrl(event.payload);
    });
    try {
      await invoke<string>("login_and_save_codex_account");
      toast.success(t("toast.accountAdded"));
      void refreshQuota("codex", true);
    } catch (err) {
      const msg = String(err);
      if (!msg.includes("Login cancelled")) {
        toast.error(msg);
      }
    } finally {
      unlisten();
      setCodexLoggingIn(false);
      setActiveReloginId(null);
      setCodexLoginUrl(null);
      setCodexLoginUrlCopied(false);
      setCodexAccountTrigger((n) => n + 1);
      void refreshActive();
    }
  };

  const testOne = async (target: Provider) => {
    setTesting(target.id);
    try {
      const result = await invoke<TestResult>("test_provider_api", { provider: target });
      const detail = `${result.status ? `HTTP ${result.status}` : t("providers.noResponse")} · ${result.latencyMs}ms · ${result.message}`;
      const msg = `${target.name} · ${detail}`;
      if (result.ok) toast.success(msg);
      else toast.error(msg);
    } catch (err) {
      toast.error(`${target.name} · ${String(err)}`);
    } finally {
      setTesting(null);
    }
  };

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      <div className="px-3 pt-3 pb-3">
        <div className="flex items-center gap-1 rounded-md bg-muted p-3">
          <div className="flex-1 min-w-0">
            <Tabs
              value={view === "gateways" ? GATEWAYS_TAB : app}
              onValueChange={(v) => {
                if (v === GATEWAYS_TAB) {
                  setView("gateways");
                } else {
                  setApp(v as CliApp);
                  setView("providers");
                }
              }}
            >
              <TabsList className="w-full justify-start gap-1 bg-transparent p-0 [&>button]:flex-none [&>button]:rounded-md [&>button]:px-3">
                {(sourceOrder ?? CLI_APPS).filter((id) => isSourceEnabled(sourceToggles, id)).map((id) => (
                  <TabsTrigger key={id} value={id}>
                    <BrandIcon source={CLI_APP_SOURCE_BADGE[id]} />
                    <span>{CLI_APP_LABEL[id]}</span>
                  </TabsTrigger>
                ))}
                <TabsTrigger value={GATEWAYS_TAB}>
                  <RadioTower className="size-4 shrink-0" aria-hidden />
                  <span>{t("providers.aiGateways")}</span>
                </TabsTrigger>
              </TabsList>
            </Tabs>
          </div>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="default"
                size="icon"
                onClick={() =>
                  view === "gateways"
                    ? setGatewayAddSignal((n) => n + 1)
                    : startNew()
                }
                disabled={view === "providers" && !installed[app]}
                aria-label={t("providers.addProvider")}
                className="shrink-0 rounded-md shadow-sm"
              >
                <Plus className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="left">
              {t("providers.addProvider")}
            </TooltipContent>
          </Tooltip>
        </div>
      </div>

      {view === "gateways" && (
        <React.Suspense fallback={null}>
          <GatewaysPage
            gateways={gateways}
            setGateways={setGateways}
            addSignal={gatewayAddSignal}
            markActive={markActive}
            activeProviderIds={activeProviderIds}
            installed={bindableInstalled}
            visibleApps={enabledApps}
            codexFollowForBinding={codexFollowForBinding}
          />
        </React.Suspense>
      )}

      {view === "providers" &&
        // A missing CLI always shows its install guide. Provider records and
        // gateway bindings are configuration for an executable that cannot
        // currently use them, so neither should leak through based on the
        // presence (or deletion) of an unrelated custom-provider record.
        (!installed[app] ? (
          <React.Suspense fallback={null}>
            <InstallGuide
              app={app}
              rechecking={rechecking}
              onRecheck={() => void handleRecheckInstall()}
            />
          </React.Suspense>
        ) : (
        <div className="flex-1 min-h-0 overflow-auto px-3 pb-0">
          <div className="flex flex-col gap-3">
            {installed[app] && (
              <div className="flex flex-col">
                <div className="relative z-10 rounded-xl bg-card">
                  <ProviderOfficialCard
                    app={app}
                    isInUse={activeState?.kind === "official"}
                    settingDefault={settingDefault === "__official__"}
                    versions={(() => {
                      // `versions[app]` is the CLI version for every app
                      // (Codex's desktop-app version rides in
                      // `codexInstalls`), so the update check is the same
                      // either way — it just lands on Codex's CLI segment.
                      const latest = hasUpdate(versions[app], latestVersions[app])
                        ? latestVersions[app]
                        : null;
                      if (app === "codex") {
                        return codexVersionSegments(
                          versions.codex,
                          codexInstalls,
                          t,
                          latest,
                          codexAppLatest
                        );
                      }
                      return versions[app]
                        ? [{ text: `v${versions[app]}`, latest }]
                        : [];
                    })()}
                    versionLoading={versionsLoading}
                    actions={app === "codex" ? (
                      codexLoggingIn ? (
                        <div className="flex items-center gap-2 shrink-0">
                          <Loader2 className="size-4 animate-spin text-muted-foreground" />
                          <span className="text-sm text-muted-foreground">{t("providers.accountAdding")}</span>
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            onClick={() => void invoke("cancel_codex_login")}
                          >
                            {t("common.cancel")}
                          </Button>
                        </div>
                      ) : codexCliMissing ? (
                        // App-only install — `codex login` can't spawn.
                        // Disabled buttons don't fire hover events, so
                        // the Tooltip anchors on a wrapping span.
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <span className="inline-flex shrink-0">
                              <Button
                                type="button"
                                variant="outline"
                                size="sm"
                                disabled
                                className="gap-1.5"
                              >
                                <UserPlus className="size-4" />
                                {t("providers.accountAdd")}
                              </Button>
                            </span>
                          </TooltipTrigger>
                          <TooltipContent>
                            {t("providers.accountAddNeedsCli")}
                          </TooltipContent>
                        </Tooltip>
                      ) : (
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          onClick={() => void handleCodexLogin()}
                          className="shrink-0 gap-1.5"
                        >
                          <UserPlus className="size-4" />
                          {t("providers.accountAdd")}
                        </Button>
                      )
                    ) : undefined}
                    onSetDefault={() => void setOfficialAsDefault()}
                  />
                </div>
                {ACCOUNT_SUPPORTED.has(app) && (
                  <OfficialAccountsSection
                    app={app}
                    onSwitched={() => {
                      void refreshActive();
                      void refreshQuota(app, true);
                    }}
                    quota={quotas[app] ?? null}
                    quotaLoading={quotaLoading === app}
                    quotaCooldown={quotaInCooldown}
                    onRefreshQuota={
                      QUOTA_SUPPORTED.has(app)
                        ? () => void refreshQuota(app, true)
                        : undefined
                    }
                    externalTrigger={app === "codex" ? codexAccountTrigger : undefined}
                    loginInProgress={app === "codex" ? codexLoggingIn : undefined}
                    activeReloginId={app === "codex" ? activeReloginId : undefined}
                    onRelogin={app === "codex" ? (id) => void handleCodexLogin(id) : undefined}
                    reloginUnavailable={app === "codex" ? codexCliMissing : undefined}
                  />
                )}
              </div>
            )}

            {customProviders.map((p) => {
              const configuredIds = activeState?.configuredProviderIds ?? [];
              const matchedId = effectiveActiveId;
              const isMulti = isMultiSlot(p.app);
              const isConfigured = isMulti
                ? configuredIds.includes(p.id)
                : matchedId === p.id;
              const isInUse = matchedId === p.id;
              return (
                <ProviderCard
                  key={p.id}
                  provider={p}
                  isConfigured={isConfigured}
                  isInUse={isInUse}
                  toggling={toggling === p.id}
                  settingDefault={settingDefault === p.id}
                  testing={testing === p.id}
                  activatable={installed[app]}
                  onToggleEnabled={isMulti ? () => void toggleEnabled(p) : undefined}
                  onSetDefault={() => void setAsDefault(p)}
                  onEdit={() => startEdit(p)}
                  onDelete={() => deleteProvider(p.id)}
                  onTest={() => void testOne(p)}
                />
              );
            })}

            {gatewayBoundForApp.map(({ gateway, synth }) => {
              const configuredIds = activeState?.configuredProviderIds ?? [];
              const matchedId = effectiveActiveId;
              const isMulti = isMultiSlot(synth.app);
              const isConfigured = isMulti
                ? configuredIds.includes(synth.id)
                : matchedId === synth.id;
              const isInUse = matchedId === synth.id;
              return (
                <ProviderCard
                  key={synth.id}
                  provider={synth}
                  gatewayBadge={gateway.name || "gateway"}
                  isConfigured={isConfigured}
                  isInUse={isInUse}
                  toggling={toggling === synth.id}
                  settingDefault={settingDefault === synth.id}
                  testing={testing === synth.id}
                  activatable={installed[app]}
                  onToggleEnabled={
                    isMulti ? () => void toggleGatewayEnabled(synth) : undefined
                  }
                  onSetDefault={() => void activateGateway(synth)}
                  onTest={() => void testOne(synth)}
                />
              );
            })}

            {customProviders.length === 0 && gatewayBoundForApp.length === 0 && (
              <EmptyState
                icon={<Plug size={32} />}
                title={t("providers.noCustomProviders")}
                description={`Add a third-party API platform for ${CLI_APP_LABEL[app]} and switch to it with one click.`}
                action={{ label: t("providers.addProvider"), onClick: startNew }}
              />
            )}
          </div>
        </div>
        ))}

      {editing && (
        <React.Suspense fallback={null}>
          <ProviderEditor
            provider={editing}
            isNew={editingIsNew}
            onSave={saveProvider}
            onClose={closeEditor}
          />
        </React.Suspense>
      )}

      <CodexFollowDialog
        target={followTarget}
        onClose={() => setFollowTarget(null)}
      />

      {/* Codex login URL dialog — shown while codex login is in progress and the auth URL has been emitted */}
      <Dialog open={codexLoginUrl !== null} onOpenChange={(open) => { if (!open) setCodexLoginUrl(null); }}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("providers.codexLoginDialogTitle")}</DialogTitle>
            <DialogDescription>{t("providers.codexLoginDialogDesc")}</DialogDescription>
          </DialogHeader>
          <div className="relative rounded-md border bg-muted/50 px-3 py-2 pr-10">
            <span className="break-all font-mono text-xs select-all">
              {codexLoginUrl}
            </span>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  className="absolute right-1 top-1"
                  aria-label={t("common.copy")}
                  onClick={() => {
                    if (codexLoginUrl) {
                      void navigator.clipboard.writeText(codexLoginUrl).then(() => {
                        setCodexLoginUrlCopied(true);
                        setTimeout(() => setCodexLoginUrlCopied(false), 1500);
                      });
                    }
                  }}
                >
                  {codexLoginUrlCopied ? <Check className="size-4" /> : <Copy className="size-4" />}
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">{t("common.copy")}</TooltipContent>
            </Tooltip>
          </div>
          <DialogFooter className="flex-row items-center gap-3">
            <p className="flex-1 text-xs text-muted-foreground">{t("providers.codexLoginDialogWaiting")}</p>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => void invoke("cancel_codex_login")}
            >
              {t("common.cancel")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
