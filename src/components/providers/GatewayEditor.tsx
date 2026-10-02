import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { Eye, EyeOff, Loader2, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
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
import { CLI_APPS } from "@/constants";
import { appProtocols } from "@/lib/provider-utils";
import { cn, INPUT_NO_AUTO } from "@/lib/utils";
import {
  BindingRows,
  bindingsFromDrafts,
  draftChecks,
  draftsFromGateway,
  type BindDraft,
  type Drafts
} from "./BindingRows";
import type {
  CliApp,
  Gateway,
  GatewayCapabilities
} from "@/types";
import { useT } from "@/i18n";

// Throttle window for the manual "Detect APIs" refresh — the button is
// disabled this long after a detection completes so it can't be spammed.
const DETECT_COOLDOWN_MS = 5000;

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
  const [binds, setBinds] = React.useState<Drafts>(() => draftsFromGateway(gateway));

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

  const checks = draftChecks(binds, protocols);

  // A gateway with no bindings is allowed (detect now, bind later).
  const canSave = name.trim().length > 0 && baseUrl.trim().length > 0 && checks.canSave;

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
    const bindings = bindingsFromDrafts(binds, protocols, visibleApps);

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
              <BindingRows
                drafts={binds}
                setBind={setBind}
                protocols={protocols}
                models={caps?.models ?? []}
                visibleApps={visibleApps}
                detecting={detecting}
                unavailableHint={() => (caps ? "no matching API mode" : "detect to enable")}
              />
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
