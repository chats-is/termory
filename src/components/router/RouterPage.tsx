import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  List,
  Play,
  RefreshCw,
  RotateCcw,
  RotateCw,
  Save,
  Square,
  RadioTower
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { BrandIcon } from "@/components/BrandIcon";
import { CLI_APPS, CLI_APP_LABEL, CLI_APP_SOURCE_BADGE } from "@/constants";
import { getConfig, invalidateConfigCache, invalidateGatewaysCache, writeGateways } from "@/config";
import { copyToClipboard } from "@/lib/clipboard";
import {
  appProtocols,
  isGatewayList,
  isMultiSlot,
  isRouterGateway,
  isSourceEnabled
} from "@/lib/provider-utils";
import { useCodexFollow } from "@/hooks/useCodexFollow";
import { useGatewayBindings } from "@/hooks/useGatewayBindings";
import { CodexFollowDialog } from "@/components/providers/CodexFollowDialog";
import { RouterModelsDialog } from "@/components/router/RouterModelsDialog";
import {
  BindingRows,
  bindingsFromDrafts,
  draftChecks,
  draftsFromGateway,
  withBindToggled,
  type BindDraft,
  type Drafts
} from "@/components/providers/BindingRows";
import {
  PROTOCOL_LABEL,
  canMoveCandidate,
  candidateReason,
  formatCountdown,
  healthFor,
  moveCandidate,
  parsePort,
  hostMode,
  prefsFromCandidates,
  sameBindings,
  sameStoredList,
  toggleCandidate,
  upstreamState,
  type UpstreamState
} from "@/lib/router-utils";
import { useT, type MessageKey } from "@/i18n";
import type {
  CliApp,
  Gateway,
  RouterCandidate,
  RouterConfigPatch,
  RouterConfigView,
  RouterPageState,
  RouterStatus,
  RouterStrategy
} from "@/types";

const ROUTER_CHANGED_EVENT = "termory:router-changed";

// Module-level cache of the last page state so re-entering the route paints
// the last-known truth immediately instead of a blank frame while the IPC
// (credential reads, providers.json sync) resolves — the same pattern the
// Providers page uses for its install snapshot.
let cachedPageState: RouterPageState | null = null;



const KIND_LABEL: Record<RouterCandidate["kind"], MessageKey> = {
  live: "router.kind.live",
  account: "router.kind.account",
  provider: "router.kind.provider",
  gateway: "router.kind.gateway"
};

const STATE_LABEL: Record<UpstreamState, MessageKey> = {
  disabled: "router.state.disabled",
  unavailable: "router.state.unavailable",
  cooldown: "router.state.cooldown",
  error: "router.state.error",
  ok: "router.state.ok",
  idle: "router.state.idle"
};

const STATE_TONE: Record<UpstreamState, string> = {
  disabled: "text-muted-foreground",
  unavailable: "text-muted-foreground",
  cooldown: "text-amber-600 dark:text-amber-400",
  error: "text-destructive",
  ok: "text-emerald-600 dark:text-emerald-400",
  idle: "text-muted-foreground"
};

function Section({
  title,
  hint,
  actions,
  children
}: {
  /** Omitted for a card whose rows speak for themselves (the settings). */
  title?: string;
  hint?: string;
  actions?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <Card className="p-3 gap-0 outline outline-1 outline-transparent bg-card shadow-sm">
      <CardContent className="px-0 flex flex-col gap-3">
        {(title || actions) && (
          <div className="flex items-center justify-between gap-3">
            <div className="min-w-0">
              {title && <h2 className="text-lg font-medium">{title}</h2>}
              {hint && <p className="text-xs text-muted-foreground mt-0.5">{hint}</p>}
            </div>
            {actions && <div className="shrink-0">{actions}</div>}
          </div>
        )}
        {children}
      </CardContent>
    </Card>
  );
}

/** Wraps a connection setting that is locked while the router runs; the
 * tooltip sits on the wrapper because a disabled control fires no hover
 * events. Unlocked, it renders the control as-is. */
function LockedWhileRunning({
  locked,
  reason,
  children
}: {
  locked: boolean;
  reason: string;
  children: React.ReactNode;
}) {
  if (!locked) return <>{children}</>;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex shrink-0">{children}</span>
      </TooltipTrigger>
      <TooltipContent side="top">{reason}</TooltipContent>
    </Tooltip>
  );
}

/** The address the running router is reached at: the LAN address when it is
 * open to the network (all interfaces, or one interface IP), else loopback. */
function activeUrl(status: RouterStatus): string {
  return status.lanUrl ?? status.baseUrl;
}

export function RouterPage({
  gateways,
  setGateways,
  activeProviderIds,
  setActiveProviderIds,
  sourceToggles,
  sourceOrder
}: {
  /** App-owned gateways list (providers.json) — the router's own entry lives
   * in it, so "Use in tools" is the same binding model as the Gateways tab. */
  gateways: Gateway[];
  setGateways: React.Dispatch<React.SetStateAction<Gateway[]>>;
  activeProviderIds: Record<string, string>;
  setActiveProviderIds: React.Dispatch<React.SetStateAction<Record<string, string>>>;
  sourceToggles?: Partial<Record<CliApp, boolean>>;
  /** Settings → Tools drag order; the binding rows follow it. */
  sourceOrder?: readonly CliApp[];
}) {
  const t = useT();
  const [state, setState] = React.useState<RouterPageState | null>(cachedPageState);
  const [busy, setBusy] = React.useState(false);
  const [modelsOpen, setModelsOpen] = React.useState(false);
  const [portDraft, setPortDraft] = React.useState<string>(
    cachedPageState ? String(cachedPageState.config.port) : ""
  );
  const [now, setNow] = React.useState(() => Date.now());
  // Inline binding drafts (the same draft model the AI Gateway editor uses).
  const [drafts, setDrafts] = React.useState<Drafts | null>(null);
  // Per-tool model catalog: what the ENABLED providers serving that tool's
  // API support (live listings, cached by the backend).
  const [bindingModels, setBindingModels] = React.useState<Partial<Record<CliApp, string[]>>>({});
  const [modelsLoading, setModelsLoading] = React.useState(false);
  const loadBindingModels = React.useCallback(async (force = false) => {
    setModelsLoading(true);
    try {
      const entries = await Promise.all(
        CLI_APPS.map(async (app) => {
          const models = await invoke<string[]>("router_binding_models", { app, force }).catch(
            () => [] as string[]
          );
          return [app, models] as const;
        })
      );
      setBindingModels(Object.fromEntries(entries));
    } finally {
      setModelsLoading(false);
    }
  }, []);

  // The backend re-syncs the router's gateway entry on every page-state read
  // and config write, so the App-owned list is re-read afterwards to pick
  // the synced connection fields up.
  const refreshGateways = React.useCallback(async () => {
    // The GATEWAYS cache, not config.json's: the backend just wrote the
    // router entry into providers.json behind the module cache.
    invalidateGatewaysCache();
    const fresh = await getConfig<unknown>("gateways").catch(() => null);
    if (!isGatewayList(fresh)) return;
    // Only a real change is written back — every setGateways persists
    // (write_app_gateways + tray rebuild + balance invalidation).
    setGateways((cur) => (sameStoredList(cur, fresh) ? cur : fresh));
  }, [setGateways]);

  const reload = React.useCallback(async () => {
    try {
      const next = await invoke<RouterPageState>("router_page_state");
      cachedPageState = next;
      setState(next);
      setPortDraft(String(next.config.port));
      await refreshGateways();
    } catch (err) {
      toast.error(String(err));
    }
  }, [refreshGateways]);


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
  const { followTarget, setFollowTarget, codexFollowForBinding } = useCodexFollow();
  const {
    activeStates,
    isBindingActive,
    reconcileAfterEdit,
    refreshActive
  } = useGatewayBindings({ gateways, markActive, activeProviderIds, codexFollowForBinding });

  // Start and Stop switch the tools' bindings in the backend (restore /
  // suspend) and rewrite the activation markers there, so the "In use"
  // badges re-read both afterwards — the same marker re-read the Providers
  // page does (`refreshMarkers`).
  const refreshInUse = React.useCallback(async () => {
    invalidateConfigCache();
    try {
      const stored = (await getConfig<Record<string, string>>("active_provider_ids")) ?? {};
      setActiveProviderIds((cur) =>
        JSON.stringify(cur) === JSON.stringify(stored) ? cur : stored
      );
    } catch {
      // keep the markers we have
    }
    await refreshActive();
  }, [setActiveProviderIds, refreshActive]);
  const routerGateway = gateways.find(isRouterGateway) ?? null;
  const visibleApps = (sourceOrder ?? CLI_APPS).filter((app) => isSourceEnabled(sourceToggles, app));

  // Binding and configuring are never gated here: every tool can be bound
  // and set up ahead of time, whether or not it is installed yet or a
  // source for its API is enabled yet — the router answers 503 for an API
  // nobody serves, which is the right moment to say so, not a reason to
  // lock the row. So every app gets its natural protocol(s).
  const protocols = React.useMemo(
    () =>
      appProtocols({
        anthropic: true,
        openai: true,
        openaiCompatible: true,
        gemini: true,
        models: []
      }),
    []
  );

  // Seed / re-seed the drafts from the saved entry whenever it changes and
  // there is nothing unsaved; a dirty draft survives a background re-sync.
  const savedKey = routerGateway ? JSON.stringify(routerGateway.bindings) : "";
  const draftsDirty =
    !!drafts &&
    !!routerGateway &&
    !sameBindings(
      [
        ...bindingsFromDrafts(drafts, protocols, visibleApps),
        ...routerGateway.bindings.filter((b) => !visibleApps.includes(b.app))
      ],
      routerGateway.bindings
    );
  React.useEffect(() => {
    if (!routerGateway) return;
    if (drafts && draftsDirty) return;
    setDrafts(draftsFromGateway(routerGateway));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [savedKey, routerGateway?.id]);

  const checks = drafts ? draftChecks(drafts, protocols) : null;

  const saveBindings = async (next: Gateway) => {
    const prev = routerGateway;
    const list = gateways.some((g) => g.id === next.id)
      ? gateways.map((g) => (g.id === next.id ? next : g))
      : [...gateways, next];
    // Written to disk BEFORE the page state changes and before
    // re-activating: a router binding is activated from providers.json
    // (`providers::activate` re-derives it there so a stale page copy can
    // never write an old port or key), and a write that fails must leave
    // the page showing what IS on disk — not a binding that never landed.
    try {
      await writeGateways(list);
    } catch (err) {
      // Not on disk: re-activating now would apply the OLD binding.
      invalidateGatewaysCache();
      toast.error(t("router.saveFailed", { error: String(err) }));
      return;
    }
    setGateways(list);
    if (prev) await reconcileAfterEdit(prev, next);
  };

  const commitDrafts = (d: Drafts) => {
    if (!routerGateway) return;
    if (!draftChecks(d, protocols).canSave) return;
    // Rows hidden by Settings → Tools were never rendered, so their bindings
    // carry over verbatim (mirrors the AI Gateway editor).
    const hidden = routerGateway.bindings.filter((b) => !visibleApps.includes(b.app));
    void saveBindings({
      ...routerGateway,
      bindings: [...bindingsFromDrafts(d, protocols, visibleApps), ...hidden]
    });
  };

  // The bind switch commits at once (it is the row's one-click action);
  // field edits inside an expanded row wait for the Save button so a
  // half-typed model id never reaches the CLI's live config.
  // The switch commits ONLY that app's bind/unbind, applied onto the SAVED
  // bindings — unsaved field edits in other rows stay drafts until Save.
  // The commit runs outside any state updater (updaters must stay pure).
  const setBind = (app: CliApp, patch: Partial<BindDraft>) => {
    if (patch.checked === undefined) {
      setDrafts((cur) => (cur ? { ...cur, [app]: { ...cur[app], ...patch } } : cur));
      return;
    }
    if (!drafts) return;
    const next = { ...drafts, [app]: { ...drafts[app], ...patch } };
    setDrafts(next);
    if (!routerGateway) return;
    // A tool that cannot be bound as drafted (e.g. OpenCode / Grok with no
    // model yet) stays a checked, unsaved draft: say why instead of
    // silently not binding it. Save applies it once the row is complete.
    if (patch.checked && draftChecks(next, protocols).appBlocked(app)) {
      toast.error(t("router.bindIncomplete", { app: CLI_APP_LABEL[app] }));
      return;
    }
    void saveBindings({
      ...routerGateway,
      bindings: withBindToggled(
        routerGateway.bindings,
        next,
        app,
        patch.checked,
        protocols,
        visibleApps
      )
    });
  };

  React.useEffect(() => {
    void reload();
  }, [reload]);

  // The enabled provider set decides the catalog: ONE unforced load once it
  // is known, and again when it changes. Unforced is enough — a provider
  // just enabled has no listing yet and is fetched; the rest are served
  // from the backend's cache until their TTL runs out. (Forcing here cost
  // one full `/models` sweep of every provider per tool per visit.)
  const enabledKey = state?.candidates
    .filter((c) => c.enabled)
    .map((c) => c.key)
    .join("\n");
  React.useEffect(() => {
    if (enabledKey === undefined) return;
    void loadBindingModels();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enabledKey]);

  // A tray switch, a Start that restored bindings or a Stop that suspended
  // them all emit this after writing the CLIs' live configs: re-read the
  // "In use" state and the router entry, like the Providers page does.
  React.useEffect(() => {
    const unlisten = listen("termory:providers-changed", () => {
      void refreshInUse();
      void refreshGateways();
    });
    return () => {
      void unlisten.then((fn) => fn()).catch(() => {});
    };
  }, [refreshInUse, refreshGateways]);

  // Status changes (a request completed, start/stop) push an event; the
  // status read is cheap, the full candidate read is not, so only status
  // is refreshed here.
  React.useEffect(() => {
    const unlisten = listen(ROUTER_CHANGED_EVENT, () => {
      void invoke<RouterStatus>("router_status")
        .then((status) => setState((prev) => (prev ? { ...prev, status } : prev)))
        .catch(() => {});
    });
    return () => {
      void unlisten.then((fn) => fn()).catch(() => {});
    };
  }, []);

  // Cooldown countdowns tick once a second only while one is showing.
  const anyCooldown =
    state?.status.upstreams.some((h) => h.cooldownUntil != null && h.cooldownUntil > now) ?? false;
  React.useEffect(() => {
    if (!anyCooldown) return;
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [anyCooldown]);

  const patch = React.useCallback(
    async (p: RouterConfigPatch) => {
      try {
        const next = await invoke<RouterPageState>("router_write_config", { patch: p });
        cachedPageState = next;
        setState(next);
        setPortDraft(String(next.config.port));
        await refreshGateways();
      } catch (err) {
        toast.error(t("router.saveFailed", { error: String(err) }));
        void reload();
      }
    },
    [reload, refreshGateways, t]
  );

  const applyCandidates = React.useCallback(
    (candidates: RouterCandidate[]) => {
      setState((prev) => (prev ? { ...prev, candidates } : prev));
      void patch({ upstreams: prefsFromCandidates(candidates) });
    },
    [patch]
  );

  const start = async () => {
    setBusy(true);
    try {
      const status = await invoke<RouterStatus>("router_start");
      setState((prev) => (prev ? { ...prev, status } : prev));
      toast.success(t("router.started", { url: activeUrl(status) }));
      await refreshInUse();
    } catch (err) {
      toast.error(t("router.startFailed", { error: String(err) }));
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    try {
      const status = await invoke<RouterStatus>("router_stop");
      setState((prev) => (prev ? { ...prev, status } : prev));
      toast.success(t("router.stoppedToast"));
      await refreshInUse();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(false);
    }
  };

  const commitPort = () => {
    if (!state) return;
    const port = parsePort(portDraft);
    if (port == null) {
      toast.error(t("router.portInvalid"));
      setPortDraft(String(state.config.port));
      return;
    }
    if (port !== state.config.port) void patch({ port });
  };

  // Listen address: loopback, every interface, or one interface's IP. An
  // address open to the network is refused without a key (the backend
  // enforces it too); say so here instead of a raw error.
  const chooseHost = (host: string) => {
    if (!state || host === state.config.host) return;
    if (hostMode(host) !== "local" && !state.config.hasApiKey) {
      toast.error(t("router.hostNeedsKey"));
      return;
    }
    void patch({ host });
  };

  const regenerateKey = async () => {
    try {
      const next = await invoke<RouterPageState>("router_generate_key");
      cachedPageState = next;
      setState(next);
      await refreshGateways();
    } catch (err) {
      toast.error(String(err));
    }
  };

  // The raw key is fetched only on this click; the page state carries the
  // masked form (security boundary: no read path hands out a raw key).
  const copyKey = async () => {
    try {
      const key = await invoke<string>("router_reveal_key");
      await copyToClipboard(key);
      toast.success(t("common.copied"));
    } catch (err) {
      toast.error(String(err));
    }
  };

  const copyText = async (text: string) => {
    await copyToClipboard(text);
    toast.success(t("common.copied"));
  };

  const resetUpstream = async (key: string) => {
    try {
      await invoke("router_reset_upstream", { key });
    } catch (err) {
      toast.error(String(err));
    }
  };

  // The shell (header band, the three cards) paints at once; only the
  // data-bearing parts wait for the first page-state read.
  const loading = state === null;
  const config: RouterConfigView = state?.config ?? {
    autostart: false,
    host: "127.0.0.1",
    port: 8317,
    hasApiKey: false,
    apiKeyMasked: "",
    strategy: "failover",
    upstreams: []
  };
  const candidates = state?.candidates ?? [];
  const status: RouterStatus = state?.status ?? {
    running: false,
    host: config.host,
    port: config.port,
    startedAt: null,
    baseUrl: "",
    lanUrl: null,
    upstreams: []
  };
  // Listen address, port and key change only while stopped (the backend
  // refuses them while running too).
  const connectionLocked = status.running;
  const listenAddresses = state?.listenAddresses ?? [
    { address: config.host, interface: null }
  ];
  const addressHint = (a: { address: string; interface: string | null }) =>
    a.interface ??
    (hostMode(a.address) === "local"
      ? t("router.host.local")
      : hostMode(a.address) === "lan"
        ? t("router.host.lan")
        : "");

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      {/* Header band — the same muted band the Providers page opens with:
          identity on the left, the page's primary control on the right. */}
      <div className="px-3 pt-3 pb-3">
        <div className="flex items-center justify-between gap-3 rounded-md bg-muted p-3">
          <div className="min-w-0">
            <div className="flex items-center gap-2 min-h-8">
              <h1 className="text-lg font-medium truncate">{t("router.title")}</h1>
              <Badge variant={status.running ? "default" : "secondary"}>
                {status.running ? t("router.running") : t("router.stopped")}
              </Badge>
            </div>
            <div className="text-xs text-muted-foreground truncate">{t("router.desc")}</div>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            {status.running && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    className="hidden md:inline-flex items-center gap-2 rounded-md bg-background px-2.5 h-8 text-xs font-mono shadow-sm hover:bg-accent"
                    onClick={() => void copyText(activeUrl(status))}
                    aria-label={t("common.copy")}
                  >
                    {activeUrl(status)}
                    <Copy className="size-3.5 text-muted-foreground" aria-hidden />
                  </button>
                </TooltipTrigger>
                <TooltipContent side="bottom">{t("common.copy")}</TooltipContent>
              </Tooltip>
            )}
            {status.running ? (
              <Button variant="outline" size="sm" disabled={busy || loading} onClick={() => void stop()}>
                <Square className="size-4" />
                {t("router.stop")}
              </Button>
            ) : (
              <Button size="sm" disabled={busy || loading} onClick={() => void start()}>
                <Play className="size-4" />
                {t("router.start")}
              </Button>
            )}
          </div>
        </div>
      </div>

      <div className="flex-1 min-h-0 overflow-auto px-3 pb-0">
        <div className="flex flex-col gap-2">
          {/* Settings — the Settings page's own row pattern. */}
          <Section title={t("router.settings")}>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5 min-w-0">
                <div className="text-sm">{t("router.port")}</div>
                <div className="text-xs text-muted-foreground">{t("router.portDesc")}</div>
              </div>
              <LockedWhileRunning locked={connectionLocked} reason={t("router.stopToEdit")}>
                <Input
                  className="w-20 text-right font-mono"
                  inputMode="numeric"
                  value={portDraft}
                  disabled={connectionLocked}
                  aria-label={t("router.port")}
                  onChange={(e) => setPortDraft(e.target.value)}
                  onBlur={commitPort}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                  }}
                />
              </LockedWhileRunning>
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5 min-w-0">
                <div className="text-sm">{t("router.host")}</div>
                <div className="text-xs text-muted-foreground">{t("router.hostDesc")}</div>
              </div>
              <LockedWhileRunning locked={connectionLocked} reason={t("router.stopToEdit")}>
                <Select value={config.host} onValueChange={chooseHost} disabled={connectionLocked}>
                  <SelectTrigger className="w-56 shrink-0" aria-label={t("router.host")}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {listenAddresses.map((a) => (
                      <SelectItem key={a.address} value={a.address}>
                        <span className="font-mono">{a.address}</span>
                        {addressHint(a) && (
                          <span className="ml-2 text-muted-foreground">{addressHint(a)}</span>
                        )}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </LockedWhileRunning>
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5 min-w-0">
                <div className="text-sm">{t("router.strategy")}</div>
                <div className="text-xs text-muted-foreground">{t("router.strategyDesc")}</div>
              </div>
              <Select
                value={config.strategy}
                onValueChange={(v) => void patch({ strategy: v as RouterStrategy })}
              >
                <SelectTrigger className="w-32" aria-label={t("router.strategy")}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="failover">{t("router.strategy.failover")}</SelectItem>
                  <SelectItem value="round-robin">{t("router.strategy.roundRobin")}</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5 min-w-0">
                <div className="text-sm">{t("router.autostart")}</div>
                <div className="text-xs text-muted-foreground">{t("router.autostartDesc")}</div>
              </div>
              <Switch
                checked={config.autostart}
                onCheckedChange={(v) => void patch({ autostart: v })}
                aria-label={t("router.autostart")}
              />
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5 min-w-0">
                <div className="text-sm">{t("router.apiKey")}</div>
                <div className="text-xs text-muted-foreground">{t("router.apiKeyDesc")}</div>
              </div>
              <div className="flex items-center gap-2 shrink-0">
                <code className="text-xs text-muted-foreground">
                  {config.hasApiKey ? config.apiKeyMasked : t("router.apiKeyNone")}
                </code>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label={t("common.copy")}
                      disabled={!config.hasApiKey}
                      onClick={() => void copyKey()}
                    >
                      <Copy className="size-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">{t("common.copy")}</TooltipContent>
                </Tooltip>
                {/* The tooltip explains the DISABLED state too, so it triggers
                    on a wrapper — a disabled button fires no hover events. */}
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="inline-flex">
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={t("router.regenerate")}
                        disabled={connectionLocked}
                        onClick={() => void regenerateKey()}
                      >
                        <RotateCw className="size-4" />
                      </Button>
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="top">
                    {connectionLocked ? t("router.stopToEdit") : t("router.regenerate")}
                  </TooltipContent>
                </Tooltip>
              </div>
            </div>
          </Section>
          {/* Upstreams — a Settings-style section; each row is laid out
              like a gateway-binding row (icon box · title/subtitle · actions). */}
          <Section
            title={t("router.upstreams")}
            hint={t("router.upstreamsDesc")}
            actions={
              <div className="flex items-center gap-1">
                <Button variant="ghost" size="sm" onClick={() => setModelsOpen(true)}>
                  <List className="size-4" />
                  {t("router.models.open")}
                </Button>
                <Button variant="ghost" size="sm" onClick={() => void reload()}>
                  <RefreshCw className="size-4" />
                  {t("router.refresh")}
                </Button>
              </div>
            }
          >
            {loading ? (
              <div className="flex flex-col gap-2 pt-1">
                {[0, 1, 2].map((i) => (
                  <Skeleton key={i} className="h-12 w-full rounded-md" />
                ))}
              </div>
            ) : candidates.length === 0 ? (
              <p className="text-xs text-muted-foreground">{t("router.noCandidates")}</p>
            ) : (
              <div className="flex flex-col gap-2 pt-1">
                {candidates.map((c) => {
                  const h = healthFor(status, c.key);
                  const { state: st, until } = upstreamState(c, h, now);
                  const stateText =
                    st === "cooldown" && until != null
                      ? t("router.state.cooldown", { time: formatCountdown(until, now) })
                      : t(STATE_LABEL[st]);
                  const subtitle = [
                    c.detail,
                    !c.available ? candidateReason(c, t) : null,
                    c.protocols.map((p) => PROTOCOL_LABEL[p]).join(" / ")
                  ]
                    .filter(Boolean)
                    .join(" · ");
                  // Same row shape as the tool-binding rows below (BindingRows):
                  // bordered row, switch first, inline brand icon, name, then
                  // muted detail; actions at the right end.
                  return (
                    <div
                      key={c.key}
                      className="rounded-md border p-2 flex items-center gap-2 text-sm min-h-12"
                    >
                      <Switch
                        checked={c.enabled}
                        onCheckedChange={(v) => applyCandidates(toggleCandidate(candidates, c.key, v))}
                        aria-label={c.label}
                      />
                      <span className="shrink-0 inline-flex [&_svg]:size-4">
                        {c.app ? (
                          <BrandIcon source={CLI_APP_SOURCE_BADGE[c.app]} />
                        ) : (
                          <RadioTower className="size-4" aria-hidden />
                        )}
                      </span>
                      <span className="font-medium truncate">{c.label}</span>
                      <Badge variant="outline" className="text-[9px] tracking-wide px-1.5 py-0 shrink-0">
                        {t(KIND_LABEL[c.kind])}
                      </Badge>
                      <span className="text-xs text-muted-foreground truncate min-w-0">{subtitle}</span>
                      <div className="ml-auto flex items-center gap-1 shrink-0">
                        <div className="text-right mr-1">
                          <div className={cn("text-xs", STATE_TONE[st])}>{stateText}</div>
                          {h && h.requests > 0 && (
                            <div className="text-xs text-muted-foreground">
                              {t("router.stats", { ok: h.requests - h.failures, fail: h.failures })}
                            </div>
                          )}
                        </div>
                        {(st === "cooldown" || st === "error") && (
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <Button
                                variant="ghost"
                                size="icon-sm"
                                aria-label={t("router.retryNow")}
                                onClick={() => void resetUpstream(c.key)}
                              >
                                <RotateCcw className="size-4" />
                              </Button>
                            </TooltipTrigger>
                            <TooltipContent side="top">{t("router.retryNow")}</TooltipContent>
                          </Tooltip>
                        )}
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          aria-label={t("router.moveUp")}
                          disabled={!canMoveCandidate(candidates, c.key, "up")}
                          onClick={() => applyCandidates(moveCandidate(candidates, c.key, "up"))}
                        >
                          <ArrowUp className="size-4" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          aria-label={t("router.moveDown")}
                          disabled={!canMoveCandidate(candidates, c.key, "down")}
                          onClick={() => applyCandidates(moveCandidate(candidates, c.key, "down"))}
                        >
                          <ArrowDown className="size-4" />
                        </Button>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </Section>

          {/* Tool bindings — the router's own gateway entry, edited INLINE with
              the same rows the AI Gateway editor uses: a switch binds, the
              row expands to the tool's model / options, Save applies. */}
          <Section
            title={t("router.useIn")}
            hint={t("router.useInDesc")}
            actions={
              // Always shown; enabled only while there is an unsaved,
              // valid edit.
              <Button
                size="sm"
                disabled={!draftsDirty || !checks?.canSave}
                onClick={() => drafts && commitDrafts(drafts)}
              >
                <Save className="size-4" />
                {t("router.saveBindings")}
              </Button>
            }
          >
            {routerGateway && drafts && (
              <div className="flex flex-col gap-2 pt-1">
                <BindingRows
                  drafts={drafts}
                  setBind={setBind}
                  protocols={protocols}
                  models={(app) => bindingModels[app] ?? []}
                  detecting={modelsLoading}
                  visibleApps={visibleApps}
                  control="switch"
                  beforeChevron={(app) => {
                    // Configuration only: activation lives in the tool's own
                    // provider list (Providers page), and needs the router up.
                    const b = routerGateway.bindings.find((x) => x.app === app);
                    if (!b || !drafts[app].checked) return null;
                    const stateFor = activeStates[app];
                    const inUse = isMultiSlot(app)
                      ? stateFor?.matchedProviderId === b.id
                      : isBindingActive(routerGateway, b);
                    return inUse ? (
                      <Badge className="uppercase text-[9px] tracking-wide px-1.5 py-0">
                        {t("providers.inUse")}
                      </Badge>
                    ) : null;
                  }}
                />
              </div>
            )}
          </Section>
        </div>
      </div>

      <CodexFollowDialog target={followTarget} onClose={() => setFollowTarget(null)} />
      <RouterModelsDialog open={modelsOpen} onOpenChange={setModelsOpen} />
    </div>
  );
}
