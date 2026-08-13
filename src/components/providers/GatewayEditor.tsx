import React from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ChevronRight,
  Eye,
  EyeOff,
  Loader2,
  RefreshCw,
  Trash2
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger
} from "@/components/ui/collapsible";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { BrandIcon } from "@/components/BrandIcon";
import { ModelCombobox } from "@/components/ModelCombobox";
import {
  CLI_APPS,
  CLI_APP_LABEL,
  CLI_APP_SOURCE_BADGE,
  OPENCODE_NPM_OPTIONS
} from "@/constants";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import {
  appProtocols,
  isClaudeSafeModelId,
  newGatewayId,
  isManagedOptionKey,
  npmForProtocol,
  overrideHelpFor
} from "@/lib/provider-utils";
import { cn, INPUT_NO_AUTO } from "@/lib/utils";
import type {
  CliApp,
  Gateway,
  GatewayBinding,
  GatewayCapabilities
} from "@/types";
import { useT } from "@/i18n";

// Claude per-size routing keys, seeded as an options template for a
// Claude binding (mirrors ProviderEditor's CLAUDE_OVERRIDE_TEMPLATE).
const CLAUDE_ROUTING_KEYS = [
  "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
  "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
  "env.ANTHROPIC_DEFAULT_HAIKU_MODEL"
] as const;

// Throttle window for the manual "Detect APIs" refresh — the button is
// disabled this long after a detection completes so it can't be spammed.
const DETECT_COOLDOWN_MS = 5000;

// Sentinel value for the multi-model "Default model" Select's "no default"
// option (Radix Select forbids an empty-string item value). Maps to "".
const NO_DEFAULT_MODEL = "__no_default__";

type KV = { key: string; value: string };
type ModelRow = { id: string; name: string };

// One editable binding row's draft state, keyed by CLI. No `protocol` —
// it's derived from app/npm wherever needed (`protocolForBinding`). `id`
// is the binding's own stable id.
type BindDraft = {
  id: string;
  checked: boolean;
  model: string;
  npm: string; // OpenCode AI SDK package ("" → default for supported mode)
  models: ModelRow[]; // OpenCode extra models
  options: KV[]; // advanced settings (Claude: per-size routing keys)
  apiBackend: string; // grok wire API ("" → omitted; grok's own default applies)
};

/**
 * Add / edit a gateway: one base URL + key, auto-detect which API
 * modes it speaks, then pick which CLIs to bind. Binding details are
 * materialized into per-CLI providers elsewhere (see `providerFromBinding`).
 */
export function GatewayEditor({
  gateway,
  isNew,
  installed,
  visibleApps = CLI_APPS,
  onSave,
  onClose
}: {
  gateway: Gateway;
  isNew: boolean;
  installed: Record<CliApp, boolean>;
  /** Apps to LIST as binding rows (Settings → Tools filters disabled
   *  ones out entirely; an installed-but-unbindable app still shows,
   *  dimmed — that's the install gate, not the tool toggle). */
  visibleApps?: readonly CliApp[];
  onSave: (r: Gateway) => void;
  onClose: () => void;
}) {
  const t = useT();
  const [name, setName] = React.useState(gateway.name);
  const [baseUrl, setBaseUrl] = React.useState(gateway.baseUrl ?? "");
  const [apiKey, setApiKey] = React.useState(gateway.apiKey ?? "");
  const [revealKey, setRevealKey] = React.useState(false);
  const [saving, setSaving] = React.useState(false);
  // Base URL at mount — only refetch the favicon when the host moves.
  const originalBaseUrlRef = React.useRef(gateway.baseUrl ?? "");
  const [caps, setCaps] = React.useState<GatewayCapabilities | undefined>(
    gateway.capabilities
  );
  const [detecting, setDetecting] = React.useState(false);
  const [detectError, setDetectError] = React.useState<string | null>(null);
  // Detection runs automatically (debounced) once base URL is entered;
  // the manual "Detect APIs" button only appears if that auto-attempt
  // failed. `lastTried` dedups so the effect fires once per unique
  // (baseUrl, apiKey) and never loops on a network error.
  const [detectAttempted, setDetectAttempted] = React.useState(
    !!gateway.capabilities
  );
  // Start `lastTried` empty so opening the editor auto-detects once (when
  // base URL + key are present) — mirrors ProviderEditor's model auto-fetch,
  // so editing always shows a fresh "N models available" and the saved
  // capabilities/models stay current. The saved caps still render
  // immediately (from `caps`) until the re-detect resolves.
  const lastTried = React.useRef<string>("");
  // Monotonic id so an earlier (e.g. typed-base-but-no-key-yet) probe
  // that resolves LATE can't overwrite a newer one's result.
  const detectSeq = React.useRef(0);
  // Timestamp (ms) until which the manual refresh is throttled, so it
  // can't be spam-clicked. Set after each detection completes; a timer
  // resets it to 0 at expiry to re-enable the button.
  const [cooldownUntil, setCooldownUntil] = React.useState(0);

  // Per-CLI binding drafts, seeded from the gateway's existing bindings.
  const [binds, setBinds] = React.useState<Record<CliApp, BindDraft>>(() => {
    const out = {} as Record<CliApp, BindDraft>;
    for (const app of CLI_APPS) {
      const existing = gateway.bindings.find((b) => b.app === app);
      out[app] = {
        id: existing?.id ?? newGatewayId(),
        checked: !!existing,
        model: existing?.model ?? "",
        npm: existing?.npm ?? "",
        models: existing?.models ?? [],
        options: existing?.options ?? [],
        apiBackend: existing?.apiBackend ?? ""
      };
    }
    return out;
  });

  // For a Claude binding, surface the 3 per-size routing keys as a fixed
  // template (key read-only, fill the value) followed by any extra
  // options the user added — matching the per-CLI Claude editor.
  const optionRows = (app: CliApp): KV[] => {
    const opts = binds[app].options;
    if (app !== "claude") return opts.length ? opts : [{ key: "", value: "" }];
    const byKey = new Map(opts.map((o) => [o.key, o.value]));
    const template = CLAUDE_ROUTING_KEYS.map((key) => ({
      key,
      value: byKey.get(key) ?? ""
    }));
    const extras = opts.filter(
      (o) => !CLAUDE_ROUTING_KEYS.includes(o.key as (typeof CLAUDE_ROUTING_KEYS)[number])
    );
    return [...template, ...extras];
  };

  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const protocols = appProtocols(caps);
  // Only offer a binding for an app that's actually installed — mirrors the
  // Providers tab's install gate, so you can't bind a tool you can't run.
  for (const app of CLI_APPS) {
    if (!installed[app]) protocols[app] = [];
  }
  const hasModes =
    !!caps &&
    (caps.openaiCompatible || caps.openai || caps.anthropic || caps.gemini);

  const detect = async () => {
    const base = baseUrl.trim();
    const key = apiKey.trim();
    // A key is REQUIRED: many gateways' auth layer answers 401 for every
    // path before routing, so a keyless probe can't tell an implemented
    // endpoint from an unknown one (false positives). With a valid key the
    // request reaches real routing (400 = exists, 404 = not).
    if (!base || !key) return;
    // Snapshot the inputs + claim a sequence number. Stale resolutions
    // (an in-flight probe started before the user finished typing the
    // key) are discarded so they can't clobber the latest result.
    const seq = ++detectSeq.current;
    lastTried.current = `${base}\n${key}`;
    setDetecting(true);
    setDetectError(null);
    try {
      const result = await invoke<GatewayCapabilities>("detect_gateway_apis", {
        baseUrl: base,
        apiKey: key
      });
      if (seq !== detectSeq.current) return; // superseded by a newer detect
      setCaps(result);
      const any =
        result.openaiCompatible ||
        result.openai ||
        result.anthropic ||
        result.gemini;
      if (!any) setDetectError(t("providers.noModesResponded"));
    } catch (err) {
      if (seq !== detectSeq.current) return;
      setDetectError(String(err));
    } finally {
      if (seq === detectSeq.current) {
        setDetecting(false);
        setDetectAttempted(true);
        setCooldownUntil(Date.now() + DETECT_COOLDOWN_MS);
      }
    }
  };

  // While throttled, schedule a re-render at expiry so the refresh button
  // re-enables itself without another user action.
  React.useEffect(() => {
    const remaining = cooldownUntil - Date.now();
    if (remaining <= 0) return;
    const t = setTimeout(() => setCooldownUntil(0), remaining);
    return () => clearTimeout(t);
  }, [cooldownUntil]);

  // Auto-detect (debounced) once BOTH base URL and API key are entered —
  // so detection doesn't fire (and bindings don't appear) on a half-
  // filled form. A keyless gateway can still be probed with the manual
  // "Detect APIs" button.
  //
  // The dedup memo only applies while the RESULT for those creds is still on
  // screen (`caps`). Editing either field throws the result away — correctly,
  // it described the old credentials — so a memo that outlived it made
  // restoring the same value unrecoverable: clear the API key (every binding
  // goes unavailable), paste the identical key back, and the sig matched, no
  // probe ran, and the editor sat with no capabilities and no way to get them
  // back short of the manual refresh button. Reported bug; gating on `caps`
  // keeps the memo's real job (don't re-probe creds we already have an answer
  // for) and drops the case where there is no answer left.
  React.useEffect(() => {
    const base = baseUrl.trim();
    const key = apiKey.trim();
    if (!base || !key) return;
    if (caps && lastTried.current === `${base}\n${key}`) return;
    const t = setTimeout(() => void detect(), 700);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [baseUrl, apiKey, caps]);

  const setBind = (app: CliApp, patch: Partial<BindDraft>) =>
    setBinds((cur) => ({ ...cur, [app]: { ...cur[app], ...patch } }));

  // OpenCode + Grok are MULTI-model (unified): a checked binding needs a
  // models LIST (each row → one picker entry); the `model` field is only the
  // OPTIONAL default, chosen FROM the list (mirrors ProviderEditor).
  const isMultiModel = (app: CliApp): boolean =>
    app === "opencode" || app === "grok";
  const boundMulti = (app: CliApp): boolean =>
    binds[app].checked && protocols[app].length > 0;
  const modelIdsFor = (app: CliApp): string[] =>
    binds[app].models.map((m) => m.id.trim()).filter(Boolean);
  const missingModelsFor = (app: CliApp): boolean =>
    boundMulti(app) && modelIdsFor(app).length === 0;
  const defaultInvalidFor = (app: CliApp): boolean => {
    const d = binds[app].model.trim();
    return boundMulti(app) && d.length > 0 && !modelIdsFor(app).includes(d);
  };
  const opencodeMissingModels = missingModelsFor("opencode");
  const opencodeDefaultInvalid = defaultInvalidFor("opencode");
  const grokMissingModels = missingModelsFor("grok");
  const grokDefaultInvalid = defaultInvalidFor("grok");

  // Claude Desktop rejects non-Claude model names, so a checked Claude
  // Desktop binding can't carry an invalid (non-blank) model id.
  const cdBindingInvalidModel =
    binds["claude-desktop"].checked &&
    protocols["claude-desktop"].length > 0 &&
    binds["claude-desktop"].models.some((m) => !isClaudeSafeModelId(m.id));
  // No duplicate model ids within ANY binding's models list (a repeat would
  // silently override — grok's `[model."<id>-<model>"]`, OpenCode's `models`
  // map, Claude Desktop's `inferenceModels`). Applies to every list-carrying
  // app, consistent with the per-provider editor.
  const bindingModelsDup = (app: CliApp): boolean => {
    if (!binds[app].checked || protocols[app].length === 0) return false;
    const ids = binds[app].models.map((m) => m.id.trim()).filter(Boolean);
    return new Set(ids).size !== ids.length;
  };
  const anyBindingDupModels = CLI_APPS.some(bindingModelsDup);

  // An Advanced-settings option key that a dedicated field already owns is
  // silently skipped by the backend at write time, so block the save and
  // tell the user (mirrors ProviderEditor's managed-key check). Uses the
  // rendered rows so Claude's protected routing template — which is NOT
  // managed — passes.
  const bindingManagedKey = (app: CliApp): boolean => {
    if (!binds[app].checked || protocols[app].length === 0) return false;
    return optionRows(app).some((o) => {
      const k = o.key.trim();
      return !!k && isManagedOptionKey(app, k);
    });
  };
  const anyBindingManagedKey = CLI_APPS.some(bindingManagedKey);

  // Duplicate non-blank option keys silently overwrite each other on write
  // (last wins), so block the save — mirrors ProviderEditor's `duplicateKeys`.
  const bindingDupKeys = (app: CliApp): string[] => {
    if (!binds[app].checked || protocols[app].length === 0) return [];
    const seen = new Set<string>();
    const dups = new Set<string>();
    for (const o of optionRows(app)) {
      const k = o.key.trim();
      if (!k) continue;
      if (seen.has(k)) dups.add(k);
      else seen.add(k);
    }
    return [...dups];
  };
  const anyBindingDupKeys = CLI_APPS.some((app) => bindingDupKeys(app).length > 0);

  const canSave =
    name.trim().length > 0 &&
    baseUrl.trim().length > 0 &&
    // A gateway with no bindings is allowed (detect now, bind later).
    !opencodeMissingModels &&
    !opencodeDefaultInvalid &&
    !grokMissingModels &&
    !grokDefaultInvalid &&
    !anyBindingDupModels &&
    !anyBindingManagedKey &&
    !anyBindingDupKeys &&
    !cdBindingInvalidModel;

  const handleSave = async () => {
    if (!canSave || saving) return;
    // Rows HIDDEN by Settings → Tools were never rendered, so their
    // drafts can't have been edited — carry the existing bindings over
    // VERBATIM. Rebuilding them from `binds`/`protocols` would drop
    // them (protocols is zeroed for non-bindable apps), silently
    // deleting a disabled tool's binding on any unrelated save.
    const hiddenBindings = gateway.bindings.filter(
      (b) => !visibleApps.includes(b.app)
    );
    const bindings: GatewayBinding[] = CLI_APPS.filter(
      (app) =>
        visibleApps.includes(app) &&
        binds[app].checked &&
        protocols[app].length > 0
    ).map((app) => {
      const d = binds[app];
      // A binding is a provider minus the gateway's common fields, WITH
      // its own id. Protocol is NOT stored — derived from app/npm.
      const b: GatewayBinding = {
        id: d.id,
        app,
        model: d.model.trim() || undefined
      };
      // Advanced options — drop blank-key or blank-value rows.
      const options = d.options
        .map((o) => ({ key: o.key.trim(), value: o.value.trim() }))
        .filter((o) => o.key && o.value);
      if (options.length) b.options = options;
      // OpenCode-only: the AI SDK package (store the EFFECTIVE one so the
      // derived protocol stays correct).
      if (app === "opencode") {
        b.npm =
          d.npm.trim() ||
          npmForProtocol(protocols[app][0] ?? "openai");
      }
      // Models list — OpenCode's extra models, Claude Desktop's
      // inferenceModels, AND grok's required model list (drop blank-id rows).
      if (app === "opencode" || app === "claude-desktop" || app === "grok") {
        const models = d.models
          .map((m) => ({ id: m.id.trim(), name: m.name.trim() }))
          .filter((m) => m.id);
        if (models.length) b.models = models;
      }
      if (app === "grok" && d.apiBackend.trim()) b.apiBackend = d.apiBackend.trim();
      return b;
    });

    // The gateway base is stored path-less (no API-version suffix) — each
    // CLI's real URL is derived per protocol. Strip a pasted /v1 or /v1beta.
    const trimmedBase = baseUrl
      .trim()
      .replace(/\/+$/, "")
      .replace(/\/(v1beta|v1)$/, "");
    // Fetch the brand favicon (same as ProviderEditor) when the gateway is
    // new or its host moved — otherwise keep the cached one. Silent on
    // failure so a slow / offline upstream never blocks the save.
    let favicon = gateway.favicon;
    const urlChanged = trimmedBase !== (originalBaseUrlRef.current ?? "");
    if (trimmedBase && (urlChanged || !favicon)) {
      setSaving(true);
      try {
        const fetched = await invoke<string | null>("fetch_provider_favicon", {
          url: trimmedBase
        });
        if (fetched) favicon = fetched;
        else if (urlChanged) favicon = undefined; // moved host → drop stale
      } catch {
        /* leave favicon as-is */
      } finally {
        setSaving(false);
      }
    }

    onSave({
      ...gateway,
      name: name.trim(),
      baseUrl: trimmedBase,
      apiKey: apiKey.trim(),
      // Persist the detected capabilities (4 booleans + a flat model
      // catalog) so reopening the gateway shows bindable sources +
      // autocomplete immediately without re-probing. Now small enough to
      // store (no per-mode model lists).
      capabilities: caps,
      favicon,
      bindings: [...bindings, ...hiddenBindings]
    });
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void handleSave();
          }}
          className="contents"
        >
        <DialogHeader className="flex-row items-baseline gap-2">
          <DialogTitle>{isNew ? t("providers.addProvider") : t("providers.editProvider")}</DialogTitle>
          <DialogDescription>{t("providers.aiGateway")}</DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-3 -mx-6 max-h-[65vh] overflow-y-auto px-6 py-1">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="gateway-name">{t("providers.name")} *</Label>
            <Input {...INPUT_NO_AUTO}
              id="gateway-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("providers.namePlaceholder")}
              autoFocus
              required
            />
          </div>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor="gateway-base">{t("providers.baseUrl")} *</Label>
            <Input {...INPUT_NO_AUTO}
              id="gateway-base"
              className="font-mono"
              value={baseUrl}
              onChange={(e) => {
                setBaseUrl(e.target.value);
                setCaps(undefined); // URL changed → re-detect (auto)
                setDetectAttempted(false);
                setDetectError(null);
              }}
              placeholder={t("providers.gwUrlPlaceholder")}
              required
            />
            <p className="text-xs text-muted-foreground">
              {t("help.gatewayHost")}
            </p>
          </div>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor="gateway-key">{t("providers.apiKey")}</Label>
            <div className="relative">
              <Input {...INPUT_NO_AUTO}
                id="gateway-key"
                type={revealKey ? "text" : "password"}
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  setCaps(undefined); // key changed → re-detect (auto)
                  setDetectAttempted(false);
                  setDetectError(null);
                }}
                placeholder={t("providers.apiKeyPlaceholder")}
                className="font-mono pr-9"
              />
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={() => setRevealKey((v) => !v)}
                    aria-label={revealKey ? t("providers.hideApiKey") : t("providers.showApiKey")}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                  >
                    {revealKey ? <EyeOff size={16} /> : <Eye size={16} />}
                  </button>
                </TooltipTrigger>
                <TooltipContent side="top">
                  {revealKey ? t("providers.hideApiKey") : t("providers.showApiKey")}
                </TooltipContent>
              </Tooltip>
            </div>
          </div>

          {/* Bind targets — ALWAYS shown so the user sees every source by
              default. A row whose required API mode wasn't detected stays
              disabled until detection enables it. */}
          <div className="flex flex-col gap-2 pt-2">
            <div className="flex items-center justify-between gap-2">
              <div className="flex items-baseline gap-2 min-w-0">
                <Label className="shrink-0">{t("providers.bindToSources")}</Label>
                {!detecting && detectAttempted && !hasModes && (
                  <span className="text-xs text-destructive truncate">
                    {detectError ??
                      t("providers.noSourcesDetected")}
                  </span>
                )}
                {(detecting || (caps?.models?.length ?? 0) > 0) && (
                  <span className="text-xs text-muted-foreground truncate">
                    {detecting
                      ? t("help.fetchingModels")
                      : t("help.modelsAvailable", { n: caps?.models?.length ?? 0 })}
                  </span>
                )}
                {/* The Anthropic mode was found under a sub-path, so a Claude
                    binding's real base URL is not the root the user typed —
                    say so rather than rewriting it invisibly. */}
                {!detecting && caps?.anthropicPath && (
                  <span className="text-xs text-muted-foreground truncate">
                    {t("help.anthropicSubpath", { path: caps.anthropicPath })}
                  </span>
                )}
              </div>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-sm"
                    className="shrink-0"
                    aria-label={t("providers.detectApis")}
                    disabled={
                      !baseUrl.trim() ||
                      !apiKey.trim() ||
                      detecting ||
                      Date.now() < cooldownUntil
                    }
                    onClick={() => void detect()}
                  >
                    <RefreshCw
                      className={cn("size-4", detecting && "animate-spin")}
                    />
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="top">
                  {t("providers.detectApis")}
                </TooltipContent>
              </Tooltip>
            </div>
            {!detectAttempted && !detecting && (
              <p className="text-xs text-muted-foreground">
                {t("help.detectNeedsKey")}
              </p>
            )}
            <div className="relative flex flex-col gap-2">
              {detecting && (
                <div className="absolute inset-0 z-10 flex items-center justify-center rounded-md bg-background/60">
                  <Loader2 className="size-5 animate-spin text-muted-foreground" />
                </div>
              )}
              {visibleApps.map((app) => {
                const allowed = protocols[app];
                const bindable = allowed.length > 0;
                const draft = binds[app];
                // OpenCode: effective AI SDK package (falls back to the
                // first detected mode's package). Protocol is derived, never
                // stored.
                const effectiveNpm =
                  app === "opencode"
                    ? draft.npm || npmForProtocol(allowed[0] ?? "openai")
                    : "";
                // One flat catalog for autocomplete — the gateway routes by
                // model id, so candidates aren't split by protocol.
                const models = caps?.models ?? [];
                // OpenCode extra-models rows — always show one blank row to
                // start, mirroring ProviderEditor's "Additional models".
                const modelRows = draft.models.length
                  ? draft.models
                  : [{ id: "", name: "" }];
                return (
                  <Collapsible
                    key={app}
                    defaultOpen={bindable && draft.checked}
                    className={cn(
                      "rounded-md border p-2 flex flex-col gap-2",
                      !bindable && "opacity-50"
                    )}
                  >
                    <div className="flex items-center gap-2 text-sm">
                      {/* Checkbox is standalone — toggles the binding only. */}
                      <input
                        type="checkbox"
                        checked={bindable && draft.checked}
                        disabled={!bindable}
                        aria-label={t("providers.bindTo", { app: CLI_APP_LABEL[app] })}
                        onChange={(e) =>
                          setBind(app, { checked: e.target.checked })
                        }
                        className="cursor-pointer disabled:cursor-not-allowed"
                      />
                      {/* The whole label + chevron row toggles collapse. */}
                      <CollapsibleTrigger
                        disabled={!bindable}
                        aria-label={t("providers.toggleSettings")}
                        className="group flex flex-1 items-center gap-2 rounded-sm text-left disabled:opacity-50 disabled:pointer-events-none"
                      >
                        <BrandIcon source={CLI_APP_SOURCE_BADGE[app]} />
                        <span className="font-medium">{CLI_APP_LABEL[app]}</span>
                        {!bindable && (
                          <span className="text-xs text-muted-foreground font-normal">
                            {caps ? "no matching API mode" : "detect to enable"}
                          </span>
                        )}
                        <ChevronRight className="ml-auto size-4 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-90" />
                      </CollapsibleTrigger>
                    </div>

                    <CollapsibleContent className="overflow-hidden data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down">
                      <div className="flex flex-col gap-2 pl-6 pt-2">
                        {/* OpenCode: AI SDK package first — it selects the
                            SDK/protocol before the model. */}
                        {app === "opencode" && (
                          <>
                            <Label className="text-xs">{t("providers.aiSdk")}</Label>
                            <Select
                              value={effectiveNpm}
                              onValueChange={(v) => setBind(app, { npm: v })}
                            >
                              <SelectTrigger className="w-full h-8">
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                {OPENCODE_NPM_OPTIONS.map((o) => (
                                  <SelectItem key={o.value} value={o.value}>
                                    {o.label}
                                  </SelectItem>
                                ))}
                              </SelectContent>
                            </Select>
                          </>
                        )}
                        {/* Grok: api_backend — before the model. "" = default
                            (field omitted; grok applies its own default,
                            chat_completions). */}
                        {app === "grok" && (
                          <>
                            <Label className="text-xs">
                              {t("providers.apiBackend")}
                            </Label>
                            <Select
                              value={draft.apiBackend || "default"}
                              onValueChange={(v) =>
                                setBind(app, {
                                  apiBackend: v === "default" ? "" : v
                                })
                              }
                            >
                              <SelectTrigger className="w-full">
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                <SelectItem value="default">
                                  {t("providers.apiBackendDefault")}
                                </SelectItem>
                                <SelectItem value="responses">
                                  Responses
                                </SelectItem>
                                <SelectItem value="chat_completions">
                                  Chat Completions
                                </SelectItem>
                                <SelectItem value="messages">Messages</SelectItem>
                              </SelectContent>
                            </Select>
                          </>
                        )}

                        {/* Single-model apps (Claude / Codex / Gemini): the
                            primary Model field. Multi-model (OpenCode + Grok)
                            render their OPTIONAL default AFTER the models list
                            below; Claude Desktop's picker is the list only. */}
                        {!isMultiModel(app) && app !== "claude-desktop" && (
                          <>
                            <Label className="text-xs">
                              {t("providers.model")}
                            </Label>
                            <ModelCombobox
                              ariaLabel={t("providers.model")}
                              value={draft.model}
                              onValueChange={(v) => setBind(app, { model: v })}
                              options={models}
                              loading={detecting}
                            />
                          </>
                        )}

                        {/* Claude Desktop: the inferenceModels list (Model ID
                            + optional display name; append [1m] for 1M). */}
                        {app === "claude-desktop" && (
                          <>
                            <Label className="text-xs">{t("providers.modelList")}</Label>
                            <p className="text-xs text-muted-foreground">
                              {t("help.cdModels")}
                            </p>
                            {modelRows.map((m, i) => (
                              <div key={i} className="flex items-center gap-1.5">
                                <ModelCombobox
                                  ariaLabel={t("providers.modelId")}
                                  placeholder={t("providers.cdModelIdPlaceholder")}
                                  value={m.id}
                                  onValueChange={(v) =>
                                    setBind(app, {
                                      models: modelRows.map((r, j) =>
                                        j === i ? { ...r, id: v } : r
                                      )
                                    })
                                  }
                                  options={models}
                                  loading={detecting}
                                  ariaInvalid={!isClaudeSafeModelId(m.id)}
                                  className="flex-1"
                                />
                                <Input {...INPUT_NO_AUTO}
                                  aria-label={t("providers.modelDisplayName")}
                                  className="flex-1 h-8"
                                  placeholder={t("providers.displayNameOptional")}
                                  value={m.name}
                                  onChange={(e) =>
                                    setBind(app, {
                                      models: modelRows.map((r, j) =>
                                        j === i ? { ...r, name: e.target.value } : r
                                      )
                                    })
                                  }
                                />
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <Button
                                      type="button"
                                      variant="ghost"
                                      size="icon-sm"
                                      className="shrink-0 text-destructive hover:text-destructive hover:bg-destructive/10"
                                      aria-label={t("providers.removeModel")}
                                      onClick={() =>
                                        setBind(app, {
                                          models: modelRows.filter((_, j) => j !== i)
                                        })
                                      }
                                    >
                                      <Trash2 className="size-4" />
                                    </Button>
                                  </TooltipTrigger>
                                  <TooltipContent side="top">
                                    {t("providers.removeModel")}
                                  </TooltipContent>
                                </Tooltip>
                              </div>
                            ))}
                            {modelRows.some(
                              (m) => !isClaudeSafeModelId(m.id)
                            ) && (
                              <p className="text-xs text-destructive">
                                {t("help.cdModelInvalid")}
                              </p>
                            )}
                            {bindingModelsDup(app) && (
                              <p className="text-xs text-destructive">
                                {t("help.duplicateModel", {
                                  id:
                                    modelRows
                                      .map((m) => m.id.trim())
                                      .filter(Boolean)
                                      .find(
                                        (id, i, a) => a.indexOf(id) !== i
                                      ) ?? ""
                                })}
                              </p>
                            )}
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              className="self-start"
                              onClick={() =>
                                setBind(app, {
                                  models: [...draft.models, { id: "", name: "" }]
                                })
                              }
                            >
                              {t("providers.add")}
                            </Button>
                          </>
                        )}

                        {/* OpenCode + Grok: the REQUIRED model list (each row →
                            one picker entry). */}
                        {isMultiModel(app) && (
                          <>
                            <Label className="text-xs mt-1">
                              {`${t("providers.modelList")} *`}
                            </Label>
                            <p className="text-xs text-muted-foreground">
                              {t(app === "grok" ? "help.grokModels" : "help.extraModels")}
                            </p>
                            {modelRows.map((m, i) => (
                              <div key={i} className="flex items-center gap-1.5">
                                <ModelCombobox
                                  ariaLabel={t("providers.modelId")}
                                  value={m.id}
                                  onValueChange={(v) =>
                                    setBind(app, {
                                      models: modelRows.map((r, j) =>
                                        j === i ? { ...r, id: v } : r
                                      )
                                    })
                                  }
                                  options={models}
                                  loading={detecting}
                                  className="flex-1"
                                />
                                <Input {...INPUT_NO_AUTO}
                                  aria-label={t("providers.modelDisplayName")}
                                  className="flex-1 h-8"
                                  placeholder={t("providers.displayNameOptional")}
                                  value={m.name}
                                  onChange={(e) =>
                                    setBind(app, {
                                      models: modelRows.map((r, j) =>
                                        j === i ? { ...r, name: e.target.value } : r
                                      )
                                    })
                                  }
                                />
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <Button
                                      type="button"
                                      variant="ghost"
                                      size="icon-sm"
                                      className="shrink-0 text-destructive hover:text-destructive hover:bg-destructive/10"
                                      aria-label={t("providers.removeModel")}
                                      onClick={() =>
                                        setBind(app, {
                                          models: modelRows.filter((_, j) => j !== i)
                                        })
                                      }
                                    >
                                      <Trash2 className="size-4" />
                                    </Button>
                                  </TooltipTrigger>
                                  <TooltipContent side="top">
                                    {t("providers.removeModel")}
                                  </TooltipContent>
                                </Tooltip>
                              </div>
                            ))}
                            {bindingModelsDup(app) && (
                              <p className="text-xs text-destructive">
                                {t("help.duplicateModel", {
                                  id:
                                    modelRows
                                      .map((m) => m.id.trim())
                                      .filter(Boolean)
                                      .find(
                                        (id, i, a) => a.indexOf(id) !== i
                                      ) ?? ""
                                })}
                              </p>
                            )}
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              className="self-start"
                              onClick={() =>
                                setBind(app, {
                                  models: [...draft.models, { id: "", name: "" }]
                                })
                              }
                            >
                              {t("providers.add")}
                            </Button>
                          </>
                        )}

                        {/* OpenCode + Grok: the OPTIONAL default model, chosen
                            FROM the list above. Radix Select; the first item
                            (sentinel value) is "no default" — Radix forbids an
                            empty-string item value, so it maps to undefined. */}
                        {isMultiModel(app) && (
                          <>
                            <Label className="text-xs mt-1">
                              {t("providers.defaultModel")}
                            </Label>
                            <Select
                              value={
                                draft.model.trim()
                                  ? draft.model.trim()
                                  : NO_DEFAULT_MODEL
                              }
                              onValueChange={(v) =>
                                setBind(app, {
                                  model: v === NO_DEFAULT_MODEL ? "" : v
                                })
                              }
                            >
                              <SelectTrigger
                                className="w-full h-8"
                                aria-label={t("providers.defaultModel")}
                                aria-invalid={defaultInvalidFor(app) || undefined}
                              >
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                <SelectItem value={NO_DEFAULT_MODEL}>
                                  {t("providers.selectModel")}
                                </SelectItem>
                                {[...new Set(modelIdsFor(app))].map((id) => (
                                  <SelectItem key={id} value={id}>
                                    {id}
                                  </SelectItem>
                                ))}
                              </SelectContent>
                            </Select>
                            {defaultInvalidFor(app) ? (
                              <p className="text-xs text-destructive">
                                {t("help.grokDefaultInvalid")}
                              </p>
                            ) : (
                              <p className="text-xs text-muted-foreground">
                                {t(
                                  app === "grok"
                                    ? "help.grokDefault"
                                    : "help.opencodeDefault"
                                )}
                              </p>
                            )}
                          </>
                        )}

                        {/* Advanced settings — same wording as ProviderEditor.
                            For grok these are its GLOBAL config.toml keys,
                            applied when the binding is set as DEFAULT
                            (`set_grok_default`). Shown for every app. */}
                        <Label className="text-xs mt-1">{t("providers.advancedSettings")}</Label>
                        <p className="text-xs text-muted-foreground">
                          {t("help.overrideIntro", { app: CLI_APP_LABEL[app] })}{" "}
                          {overrideHelpFor(app, t)}
                        </p>
                        {optionRows(app).map((o, i) => {
                          const rows = optionRows(app);
                          const isTemplate =
                            app === "claude" && i < CLAUDE_ROUTING_KEYS.length;
                          return (
                            <div key={i} className="flex items-center gap-1.5">
                              <Input {...INPUT_NO_AUTO}
                                value={o.key}
                                readOnly={isTemplate}
                                placeholder={t("providers.keyUpper")}
                                className={cn(
                                  "font-mono flex-1 h-8",
                                  isTemplate && "text-muted-foreground"
                                )}
                                onChange={(e) =>
                                  setBind(app, {
                                    options: rows.map((x, j) =>
                                      j === i ? { ...x, key: e.target.value } : x
                                    )
                                  })
                                }
                              />
                              <Input {...INPUT_NO_AUTO}
                                value={o.value}
                                placeholder={t("providers.valueUpper")}
                                className="flex-1 h-8"
                                onChange={(e) =>
                                  setBind(app, {
                                    options: rows.map((x, j) =>
                                      j === i ? { ...x, value: e.target.value } : x
                                    )
                                  })
                                }
                              />
                              {!isTemplate && (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <Button
                                      type="button"
                                      variant="ghost"
                                      size="icon-sm"
                                      className="shrink-0 text-destructive hover:text-destructive hover:bg-destructive/10"
                                      aria-label={t("providers.removeOverride")}
                                      onClick={() =>
                                        setBind(app, {
                                          options: rows.filter((_, j) => j !== i)
                                        })
                                      }
                                    >
                                      <Trash2 className="size-4" />
                                    </Button>
                                  </TooltipTrigger>
                                  <TooltipContent side="top">
                                    {t("providers.removeOverride")}
                                  </TooltipContent>
                                </Tooltip>
                              )}
                            </div>
                          );
                        })}
                        {bindingManagedKey(app) && (
                          <p className="text-xs text-destructive">
                            {t("errors.managedKeys", {
                              keys: optionRows(app)
                                .map((o) => o.key.trim())
                                .filter((k) => k && isManagedOptionKey(app, k))
                                .map((k) => `"${k}"`)
                                .join(", ")
                            })}
                          </p>
                        )}
                        {bindingDupKeys(app).length > 0 && (
                          <p className="text-xs text-destructive">
                            {t("errors.duplicateKeys", {
                              keys: bindingDupKeys(app)
                                .map((k) => `"${k}"`)
                                .join(", ")
                            })}
                          </p>
                        )}
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          className="self-start"
                          onClick={() =>
                            setBind(app, {
                              options: [...optionRows(app), { key: "", value: "" }]
                            })
                          }
                        >
                          {t("providers.add")}
                        </Button>
                      </div>
                    </CollapsibleContent>
                  </Collapsible>
                );
              })}
            </div>
          </div>
        </div>

        <DialogFooter>
          <Button type="button" variant="ghost" onClick={onClose}>
            {t("providers.cancel")}
          </Button>
          <Button type="submit" disabled={!canSave || saving}>
            {saving ? (
              <Loader2 className="size-4 animate-spin" />
            ) : isNew ? (
              t("providers.create")
            ) : (
              t("providers.save")
            )}
          </Button>
        </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
