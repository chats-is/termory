import React from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ChevronRight,
  Eye,
  EyeOff,
  Loader2,
  Plus,
  RefreshCw,
  Trash2
} from "lucide-react";
import { Button } from "@/components/ui/button";
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
  newGatewayId,
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
};

/**
 * Add / edit a gateway: one base URL + key, auto-detect which API
 * modes it speaks, then pick which CLIs to bind. Binding details are
 * materialized into per-CLI providers elsewhere (see `providerFromBinding`).
 */
export function GatewayEditor({
  gateway,
  isNew,
  onSave,
  onClose
}: {
  gateway: Gateway;
  isNew: boolean;
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
  // Seed `lastTried` to the saved gateway's creds when it already carries
  // persisted capabilities — so opening it to edit shows the stored caps
  // immediately and does NOT auto-redetect (the user can refresh, and any
  // base/key change still triggers a fresh detect).
  const lastTried = React.useRef<string>(
    gateway.capabilities
      ? `${(gateway.baseUrl ?? "").trim()}\n${(gateway.apiKey ?? "").trim()}`
      : ""
  );
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
        options: existing?.options ?? []
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
      if (!any) setDetectError("No supported API modes responded at this URL.");
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
  // "Detect APIs" button. Fires once per unique (baseUrl, apiKey).
  React.useEffect(() => {
    const base = baseUrl.trim();
    const key = apiKey.trim();
    if (!base || !key) return;
    if (lastTried.current === `${base}\n${key}`) return;
    const t = setTimeout(() => void detect(), 700);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [baseUrl, apiKey]);

  const setBind = (app: CliApp, patch: Partial<BindDraft>) =>
    setBinds((cur) => ({ ...cur, [app]: { ...cur[app], ...patch } }));

  // OpenCode can't surface a provider without a primary model id, so a
  // checked OpenCode binding must have one (mirrors ProviderEditor).
  const opencodeMissingModel =
    binds.opencode.checked &&
    protocols.opencode.length > 0 &&
    binds.opencode.model.trim().length === 0;

  const canSave =
    name.trim().length > 0 &&
    baseUrl.trim().length > 0 &&
    // A gateway with no bindings is allowed (detect now, bind later).
    !opencodeMissingModel;

  const handleSave = async () => {
    if (!canSave || saving) return;
    const bindings: GatewayBinding[] = CLI_APPS.filter(
      (app) => binds[app].checked && protocols[app].length > 0
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
      // derived protocol stays correct) + extra models (drop blank-id).
      if (app === "opencode") {
        b.npm =
          d.npm.trim() ||
          npmForProtocol(protocols[app][0] ?? "openai-compatible");
        const models = d.models
          .map((m) => ({ id: m.id.trim(), name: m.name.trim() }))
          .filter((m) => m.id);
        if (models.length) b.models = models;
      }
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
      bindings
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
          <DialogTitle>{isNew ? t("providers.addGateway") : t("providers.editGateway")}</DialogTitle>
          <DialogDescription>AI Gateway</DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-3 -mx-6 max-h-[65vh] overflow-y-auto px-6 py-1">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="gateway-name">{t("providers.name")}</Label>
            <Input {...INPUT_NO_AUTO}
              id="gateway-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("providers.namePlaceholder")}
              autoFocus
            />
          </div>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor="gateway-base">{t("providers.baseUrl")}</Label>
            <Input {...INPUT_NO_AUTO}
              id="gateway-base"
              value={baseUrl}
              onChange={(e) => {
                setBaseUrl(e.target.value);
                setCaps(undefined); // URL changed → re-detect (auto)
                setDetectAttempted(false);
                setDetectError(null);
              }}
              placeholder={t("providers.gwUrlPlaceholder")}
            />
            <p className="text-xs text-muted-foreground">
              The gateway host. Termory appends the right path per CLI
              (`/v1`, `/v1beta`, …) when binding.
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
                className="pr-9"
              />
              <button
                type="button"
                onClick={() => setRevealKey((v) => !v)}
                aria-label={revealKey ? t("providers.hideApiKey") : t("providers.showApiKey")}
                className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
              >
                {revealKey ? <EyeOff size={16} /> : <Eye size={16} />}
              </button>
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
                      "No supported sources detected — refresh to retry."}
                  </span>
                )}
              </div>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="size-7 shrink-0"
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
            </div>
            {!detectAttempted && !detecting && (
              <p className="text-xs text-muted-foreground">
                Enter the base URL and API key — detection needs the key
                (auto, or via the refresh button).
              </p>
            )}
            <div className="relative flex flex-col gap-2">
              {detecting && (
                <div className="absolute inset-0 z-10 flex items-center justify-center rounded-md bg-background/60">
                  <Loader2 className="size-5 animate-spin text-muted-foreground" />
                </div>
              )}
              {CLI_APPS.map((app) => {
                const allowed = protocols[app];
                const bindable = allowed.length > 0;
                const draft = binds[app];
                // OpenCode: effective AI SDK package (falls back to the
                // first detected mode's package). Protocol is derived, never
                // stored.
                const effectiveNpm =
                  app === "opencode"
                    ? draft.npm ||
                      npmForProtocol(allowed[0] ?? "openai-compatible")
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
                        <Label className="text-xs">
                          {app === "opencode" ? `${t("providers.model")} *` : t("providers.model")}
                        </Label>
                        <ModelCombobox
                          ariaLabel={app === "opencode" ? `${t("providers.model")} *` : t("providers.model")}
                          value={draft.model}
                          onValueChange={(v) => setBind(app, { model: v })}
                          options={models}
                          loading={detecting}
                          ariaInvalid={
                            app === "opencode" && !draft.model.trim()
                          }
                        />
                        {app === "opencode" && !draft.model.trim() && (
                          <p className="text-xs text-destructive">
                            OpenCode requires a primary model id.
                          </p>
                        )}

                        {/* OpenCode: AI SDK package + extra models. */}
                        {app === "opencode" && (
                          <>
                            <Label className="text-xs mt-1">{t("providers.aiSdk")}</Label>
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
                            <Label className="text-xs mt-1">
                              {t("providers.additionalModels")}
                            </Label>
                            <p className="text-xs text-muted-foreground">
                              Extra models surfaced in OpenCode&apos;s picker
                              (the primary &quot;Model&quot; above is always
                              included). ID is the model id; name is the display
                              label (defaults to the id if left blank).
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
                                <Button
                                  type="button"
                                  variant="ghost"
                                  size="icon"
                                  className="size-8 shrink-0"
                                  aria-label={t("providers.removeModel")}
                                  onClick={() =>
                                    setBind(app, {
                                      models: modelRows.filter((_, j) => j !== i)
                                    })
                                  }
                                >
                                  <Trash2 className="size-4" />
                                </Button>
                              </div>
                            ))}
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

                        {/* Advanced settings — same wording as ProviderEditor;
                            Claude seeds the per-size routing keys. */}
                        <Label className="text-xs mt-1">{t("providers.advancedSettings")}</Label>
                        <p className="text-xs text-muted-foreground">
                          Extra settings merged into {CLI_APP_LABEL[app]}&apos;s
                          config while this provider is active, and removed when
                          you switch away. {overrideHelpFor(app)}
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
                                <Button
                                  type="button"
                                  variant="ghost"
                                  size="icon"
                                  className="size-8 shrink-0"
                                  aria-label={t("providers.removeOverride")}
                                  onClick={() =>
                                    setBind(app, {
                                      options: rows.filter((_, j) => j !== i)
                                    })
                                  }
                                >
                                  <Trash2 className="size-4" />
                                </Button>
                              )}
                            </div>
                          );
                        })}
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
