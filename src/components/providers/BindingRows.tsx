import React from "react";
import { ChevronRight, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger
} from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { BrandIcon } from "@/components/BrandIcon";
import { ModelCombobox } from "@/components/ModelCombobox";
import {
  CLI_APPS,
  CLI_APP_LABEL,
  CLI_APP_SOURCE_BADGE,
  OPENCODE_NPM_OPTIONS
} from "@/constants";
import {
  isClaudeSafeModelId,
  isManagedOptionKey,
  newGatewayId,
  npmForProtocol,
  overrideHelpFor
} from "@/lib/provider-utils";
import { cn, INPUT_NO_AUTO } from "@/lib/utils";
import type { CliApp, Gateway, GatewayBinding, GatewayProtocol } from "@/types";
import { useT } from "@/i18n";

/**
 * The per-CLI binding rows of a gateway — drafts, validation, the
 * materialization back to `GatewayBinding[]`, and the collapsible row UI.
 * Shared by the AI Gateway editor (dialog, checkbox rows) and the Router
 * page (inline, switch rows), so the two surfaces cannot drift.
 */

// Claude per-size routing keys, seeded as an options template for a
// Claude binding (mirrors ProviderEditor's CLAUDE_OVERRIDE_TEMPLATE).
export const CLAUDE_ROUTING_KEYS = [
  "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
  "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
  "env.ANTHROPIC_DEFAULT_HAIKU_MODEL"
] as const;

// Sentinel value for the multi-model "Default model" Select's "no default"
// option (Radix Select forbids an empty-string item value). Maps to "".
const NO_DEFAULT_MODEL = "__no_default__";

export type KV = { key: string; value: string };
export type ModelRow = { id: string; name: string };

// One editable binding row's draft state, keyed by CLI. No `protocol` —
// it's derived from app/npm wherever needed (`protocolForBinding`). `id`
// is the binding's own stable id.
export type BindDraft = {
  id: string;
  checked: boolean;
  model: string;
  npm: string; // OpenCode AI SDK package ("" → default for supported mode)
  models: ModelRow[]; // OpenCode extra models
  options: KV[]; // advanced settings (Claude: per-size routing keys)
  apiBackend: string; // grok wire API ("" → omitted; grok's own default applies)
};

export type Drafts = Record<CliApp, BindDraft>;

/** Per-CLI drafts seeded from a gateway's existing bindings. */
export function draftsFromGateway(gateway: Gateway): Drafts {
  const out = {} as Drafts;
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
}

// OpenCode + Grok are MULTI-model (unified): a checked binding needs a
// models LIST (each row → one picker entry); the `model` field is only the
// OPTIONAL default, chosen FROM the list (mirrors ProviderEditor).
export const isMultiModel = (app: CliApp): boolean => app === "opencode" || app === "grok";

/** For a Claude binding, surface the 3 per-size routing keys as a fixed
 * template (key read-only, fill the value) followed by any extra options
 * the user added — matching the per-CLI Claude editor. */
export function optionRowsFor(app: CliApp, draft: BindDraft): KV[] {
  const opts = draft.options;
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
}

/** Every save-blocking condition, derived from the drafts and the per-app
 * bindable protocols. `canSave` is the conjunction. */
export function draftChecks(drafts: Drafts, protocols: Record<CliApp, GatewayProtocol[]>) {
  const boundMulti = (app: CliApp) => drafts[app].checked && protocols[app].length > 0;
  const modelIdsFor = (app: CliApp) =>
    drafts[app].models.map((m) => m.id.trim()).filter(Boolean);
  const missingModels = (app: CliApp) =>
    isMultiModel(app) && boundMulti(app) && modelIdsFor(app).length === 0;
  const defaultInvalid = (app: CliApp) => {
    const d = drafts[app].model.trim();
    return isMultiModel(app) && boundMulti(app) && d.length > 0 && !modelIdsFor(app).includes(d);
  };
  // Claude Desktop rejects non-Claude model names, so a checked Claude
  // Desktop binding can't carry an invalid (non-blank) model id.
  const cdInvalidModel =
    boundMulti("claude-desktop") &&
    drafts["claude-desktop"].models.some((m) => !isClaudeSafeModelId(m.id));
  // No duplicate model ids within ANY binding's models list (a repeat would
  // silently override — grok's `[model."<id>-<model>"]`, OpenCode's `models`
  // map, Claude Desktop's `inferenceModels`).
  const modelsDup = (app: CliApp) => {
    if (!boundMulti(app)) return false;
    const ids = modelIdsFor(app);
    return new Set(ids).size !== ids.length;
  };
  // An Advanced-settings option key that a dedicated field already owns is
  // silently skipped by the backend at write time, so block the save. Uses
  // the rendered rows so Claude's protected routing template — which is NOT
  // managed — passes.
  const managedKey = (app: CliApp) => {
    if (!boundMulti(app)) return false;
    return optionRowsFor(app, drafts[app]).some((o) => {
      const k = o.key.trim();
      return !!k && isManagedOptionKey(app, k);
    });
  };
  // Duplicate non-blank option keys silently overwrite each other on write
  // (last wins), so block the save.
  const dupKeys = (app: CliApp): string[] => {
    if (!boundMulti(app)) return [];
    const seen = new Set<string>();
    const dups = new Set<string>();
    for (const o of optionRowsFor(app, drafts[app])) {
      const k = o.key.trim();
      if (!k) continue;
      if (seen.has(k)) dups.add(k);
      else seen.add(k);
    }
    return [...dups];
  };
  /** Whether THIS app's draft alone blocks a save (a checked, bindable row). */
  const appBlocked = (app: CliApp) =>
    missingModels(app) ||
    defaultInvalid(app) ||
    modelsDup(app) ||
    managedKey(app) ||
    dupKeys(app).length > 0 ||
    (app === "claude-desktop" && cdInvalidModel);
  const canSave =
    !CLI_APPS.some(missingModels) &&
    !CLI_APPS.some(defaultInvalid) &&
    !CLI_APPS.some(modelsDup) &&
    !CLI_APPS.some(managedKey) &&
    !CLI_APPS.some((a) => dupKeys(a).length > 0) &&
    !cdInvalidModel;
  return {
    canSave,
    appBlocked,
    modelIdsFor,
    missingModels,
    defaultInvalid,
    modelsDup,
    managedKey,
    dupKeys
  };
}

/** Materialize the checked, bindable drafts among `apps` into bindings. A
 * binding is a provider minus the gateway's common fields, WITH its own id;
 * protocol is NOT stored — derived from app/npm. */
export function bindingsFromDrafts(
  drafts: Drafts,
  protocols: Record<CliApp, GatewayProtocol[]>,
  apps: readonly CliApp[]
): GatewayBinding[] {
  return CLI_APPS.filter(
    (app) => apps.includes(app) && drafts[app].checked && protocols[app].length > 0
  ).map((app) => {
    const d = drafts[app];
    const b: GatewayBinding = { id: d.id, app, model: d.model.trim() || undefined };
    // Advanced options — drop blank-key or blank-value rows.
    const options = d.options
      .map((o) => ({ key: o.key.trim(), value: o.value.trim() }))
      .filter((o) => o.key && o.value);
    if (options.length) b.options = options;
    // OpenCode-only: the AI SDK package (store the EFFECTIVE one so the
    // derived protocol stays correct).
    if (app === "opencode") {
      b.npm = d.npm.trim() || npmForProtocol(protocols[app][0] ?? "openai");
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
}

/** The bindings a one-click bind switch commits: the SAVED bindings with
 * only `app` changed — dropped when unbound, or materialized from its draft
 * when bound. Unsaved field edits in other rows stay drafts (they wait for
 * Save). Visible bindings come out in `CLI_APPS` order followed by the rest
 * (hidden by Settings → Tools) in saved order — the same shape the drafts
 * materialize to, so a committed toggle does not read as dirty. */
export function withBindToggled(
  saved: GatewayBinding[],
  drafts: Drafts,
  app: CliApp,
  checked: boolean,
  protocols: Record<CliApp, GatewayProtocol[]>,
  visibleApps: readonly CliApp[]
): GatewayBinding[] {
  const others = saved.filter((b) => b.app !== app);
  const added = checked
    ? bindingsFromDrafts({ ...drafts, [app]: { ...drafts[app], checked: true } }, protocols, [app])
    : [];
  const all = [...others, ...added];
  const visible = CLI_APPS.flatMap((a) =>
    visibleApps.includes(a) ? all.filter((b) => b.app === a) : []
  );
  const hidden = all.filter((b) => !visibleApps.includes(b.app));
  return [...visible, ...hidden];
}

export function BindingRows({
  drafts,
  setBind,
  protocols,
  models: modelsProp,
  visibleApps,
  detecting = false,
  unavailableHint,
  control = "checkbox",
  beforeChevron,
  trailing
}: {
  drafts: Drafts;
  setBind: (app: CliApp, patch: Partial<BindDraft>) => void;
  /** Per-app bindable protocols; an empty list disables the row. */
  protocols: Record<CliApp, GatewayProtocol[]>;
  /** Model catalog for autocomplete — one flat list, or one per tool. */
  models: string[] | ((app: CliApp) => string[]);
  visibleApps: readonly CliApp[];
  detecting?: boolean;
  /** Text shown beside a non-bindable row's name (why it is disabled). */
  unavailableHint?: (app: CliApp) => string;
  /** The bind toggle: a checkbox (dialog form) or a switch (inline page). */
  control?: "checkbox" | "switch";
  /** Content at the right end of the trigger, just before the chevron
   * (e.g. an "in use" badge). */
  beforeChevron?: (app: CliApp) => React.ReactNode;
  /** Extra content at the right end of a row's header (e.g. an Activate
   * button); rendered outside the collapse trigger. */
  trailing?: (app: CliApp) => React.ReactNode;
}) {
  const t = useT();
  const checks = draftChecks(drafts, protocols);
  return (
    <>
      {visibleApps.map((app) => {
        const allowed = protocols[app];
        const bindable = allowed.length > 0;
        const draft = drafts[app];
        // OpenCode: effective AI SDK package (falls back to the first
        // detected mode's package). Protocol is derived, never stored.
        const effectiveNpm =
          app === "opencode" ? draft.npm || npmForProtocol(allowed[0] ?? "openai") : "";
        // Always show one blank row to start, mirroring ProviderEditor.
        const modelRows = draft.models.length ? draft.models : [{ id: "", name: "" }];
        const optionRows = optionRowsFor(app, draft);
        const models = typeof modelsProp === "function" ? modelsProp(app) : modelsProp;
        const setModels = (models: ModelRow[]) => setBind(app, { models });
        const modelList = (invalid?: (m: ModelRow) => boolean) => (
          <>
            {modelRows.map((m, i) => (
              <div key={i} className="flex items-center gap-1.5">
                <ModelCombobox
                  ariaLabel={t("providers.modelId")}
                  placeholder={app === "claude-desktop" ? t("providers.cdModelIdPlaceholder") : undefined}
                  value={m.id}
                  onValueChange={(v) =>
                    setModels(modelRows.map((r, j) => (j === i ? { ...r, id: v } : r)))
                  }
                  options={models}
                  loading={detecting}
                  ariaInvalid={invalid ? invalid(m) : undefined}
                  className="flex-1"
                />
                <Input
                  {...INPUT_NO_AUTO}
                  aria-label={t("providers.modelDisplayName")}
                  className="flex-1 h-8"
                  placeholder={t("providers.displayNameOptional")}
                  value={m.name}
                  onChange={(e) =>
                    setModels(
                      modelRows.map((r, j) => (j === i ? { ...r, name: e.target.value } : r))
                    )
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
                      onClick={() => setModels(modelRows.filter((_, j) => j !== i))}
                    >
                      <Trash2 className="size-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">{t("providers.removeModel")}</TooltipContent>
                </Tooltip>
              </div>
            ))}
            {checks.modelsDup(app) && (
              <p className="text-xs text-destructive">
                {t("help.duplicateModel", {
                  id:
                    modelRows
                      .map((m) => m.id.trim())
                      .filter(Boolean)
                      .find((id, i, a) => a.indexOf(id) !== i) ?? ""
                })}
              </p>
            )}
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="self-start"
              onClick={() => setModels([...draft.models, { id: "", name: "" }])}
            >
              {t("providers.add")}
            </Button>
          </>
        );
        return (
          <Collapsible
            key={app}
            defaultOpen={control === "checkbox" && bindable && draft.checked}
            className={cn(
              "rounded-md border p-2 flex flex-col gap-2",
              !bindable && "opacity-50"
            )}
          >
            <div className="flex items-center gap-2 text-sm">
              {/* The toggle is standalone — it binds/unbinds only. */}
              {control === "switch" ? (
                <Switch
                  checked={bindable && draft.checked}
                  disabled={!bindable}
                  aria-label={t("providers.bindTo", { app: CLI_APP_LABEL[app] })}
                  onCheckedChange={(v) => setBind(app, { checked: v })}
                />
              ) : (
                <input
                  type="checkbox"
                  checked={bindable && draft.checked}
                  disabled={!bindable}
                  aria-label={t("providers.bindTo", { app: CLI_APP_LABEL[app] })}
                  onChange={(e) => setBind(app, { checked: e.target.checked })}
                  className="cursor-pointer disabled:cursor-not-allowed"
                />
              )}
              {/* The whole label + chevron row toggles collapse. */}
              <CollapsibleTrigger
                disabled={!bindable}
                aria-label={t("providers.toggleSettingsFor", { app: CLI_APP_LABEL[app] })}
                className="group flex flex-1 items-center gap-2 rounded-sm text-left disabled:opacity-50 disabled:pointer-events-none min-w-0"
              >
                <BrandIcon source={CLI_APP_SOURCE_BADGE[app]} />
                <span className="font-medium">{CLI_APP_LABEL[app]}</span>
                {!bindable && unavailableHint && (
                  <span className="text-xs text-muted-foreground font-normal">
                    {unavailableHint(app)}
                  </span>
                )}
                {bindable && draft.checked && draft.model.trim() && (
                  <span className="text-xs text-muted-foreground font-mono truncate">
                    {draft.model.trim()}
                  </span>
                )}
                <span className="ml-auto flex items-center gap-2 shrink-0">
                  {beforeChevron?.(app)}
                  <ChevronRight className="size-4 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-90" />
                </span>
              </CollapsibleTrigger>
              {trailing && <div className="shrink-0 flex items-center gap-1.5">{trailing(app)}</div>}
            </div>

            <CollapsibleContent className="overflow-hidden data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down">
              <div className="flex flex-col gap-2 pl-6 pt-2">
                {/* OpenCode: AI SDK package first — it selects the
                    SDK/protocol before the model. */}
                {app === "opencode" && (
                  <>
                    <Label className="text-xs">{t("providers.aiSdk")}</Label>
                    <Select value={effectiveNpm} onValueChange={(v) => setBind(app, { npm: v })}>
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
                {/* Grok: api_backend — before the model. "" = default (field
                    omitted; grok applies its own default, chat_completions). */}
                {app === "grok" && (
                  <>
                    <Label className="text-xs">{t("providers.apiBackend")}</Label>
                    <Select
                      value={draft.apiBackend || "default"}
                      onValueChange={(v) => setBind(app, { apiBackend: v === "default" ? "" : v })}
                    >
                      <SelectTrigger className="w-full">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="default">{t("providers.apiBackendDefault")}</SelectItem>
                        <SelectItem value="responses">Responses</SelectItem>
                        <SelectItem value="chat_completions">Chat Completions</SelectItem>
                        <SelectItem value="messages">Messages</SelectItem>
                      </SelectContent>
                    </Select>
                  </>
                )}

                {/* Single-model apps (Claude / Codex / Gemini): the primary
                    Model field. Multi-model (OpenCode + Grok) render their
                    OPTIONAL default AFTER the models list below; Claude
                    Desktop's picker is the list only. */}
                {!isMultiModel(app) && app !== "claude-desktop" && (
                  <>
                    <Label className="text-xs">{t("providers.model")}</Label>
                    <ModelCombobox
                      ariaLabel={t("providers.model")}
                      value={draft.model}
                      onValueChange={(v) => setBind(app, { model: v })}
                      options={models}
                      loading={detecting}
                    />
                  </>
                )}

                {/* Claude Desktop: the inferenceModels list (Model ID +
                    optional display name; append [1m] for 1M). */}
                {app === "claude-desktop" && (
                  <>
                    <Label className="text-xs">{t("providers.modelList")}</Label>
                    <p className="text-xs text-muted-foreground">{t("help.cdModels")}</p>
                    {modelList((m) => !isClaudeSafeModelId(m.id))}
                    {modelRows.some((m) => !isClaudeSafeModelId(m.id)) && (
                      <p className="text-xs text-destructive">{t("help.cdModelInvalid")}</p>
                    )}
                  </>
                )}

                {/* OpenCode + Grok: the REQUIRED model list (each row → one
                    picker entry), then the OPTIONAL default chosen FROM it. */}
                {isMultiModel(app) && (
                  <>
                    <Label className="text-xs mt-1">{`${t("providers.modelList")} *`}</Label>
                    <p className="text-xs text-muted-foreground">
                      {t(app === "grok" ? "help.grokModels" : "help.extraModels")}
                    </p>
                    {modelList()}
                    <Label className="text-xs mt-1">{t("providers.defaultModel")}</Label>
                    <Select
                      value={draft.model.trim() ? draft.model.trim() : NO_DEFAULT_MODEL}
                      onValueChange={(v) => setBind(app, { model: v === NO_DEFAULT_MODEL ? "" : v })}
                    >
                      <SelectTrigger
                        className="w-full h-8"
                        aria-label={t("providers.defaultModel")}
                        aria-invalid={checks.defaultInvalid(app) || undefined}
                      >
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value={NO_DEFAULT_MODEL}>{t("providers.selectModel")}</SelectItem>
                        {[...new Set(checks.modelIdsFor(app))].map((id) => (
                          <SelectItem key={id} value={id}>
                            {id}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    {checks.defaultInvalid(app) ? (
                      <p className="text-xs text-destructive">{t("help.grokDefaultInvalid")}</p>
                    ) : (
                      <p className="text-xs text-muted-foreground">
                        {t(app === "grok" ? "help.grokDefault" : "help.opencodeDefault")}
                      </p>
                    )}
                  </>
                )}

                {/* Advanced settings — same wording as ProviderEditor. For
                    grok these are its GLOBAL config.toml keys, applied when
                    the binding is set as DEFAULT. Shown for every app. */}
                <Label className="text-xs mt-1">{t("providers.advancedSettings")}</Label>
                <p className="text-xs text-muted-foreground">
                  {t("help.overrideIntro", { app: CLI_APP_LABEL[app] })} {overrideHelpFor(app, t)}
                </p>
                {optionRows.map((o, i) => {
                  const isTemplate = app === "claude" && i < CLAUDE_ROUTING_KEYS.length;
                  return (
                    <div key={i} className="flex items-center gap-1.5">
                      <Input
                        {...INPUT_NO_AUTO}
                        value={o.key}
                        readOnly={isTemplate}
                        placeholder={t("providers.keyUpper")}
                        className={cn("font-mono flex-1 h-8", isTemplate && "text-muted-foreground")}
                        onChange={(e) =>
                          setBind(app, {
                            options: optionRows.map((x, j) =>
                              j === i ? { ...x, key: e.target.value } : x
                            )
                          })
                        }
                      />
                      <Input
                        {...INPUT_NO_AUTO}
                        value={o.value}
                        placeholder={t("providers.valueUpper")}
                        className="flex-1 h-8"
                        onChange={(e) =>
                          setBind(app, {
                            options: optionRows.map((x, j) =>
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
                                setBind(app, { options: optionRows.filter((_, j) => j !== i) })
                              }
                            >
                              <Trash2 className="size-4" />
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent side="top">{t("providers.removeOverride")}</TooltipContent>
                        </Tooltip>
                      )}
                    </div>
                  );
                })}
                {checks.managedKey(app) && (
                  <p className="text-xs text-destructive">
                    {t("errors.managedKeys", {
                      keys: optionRows
                        .map((o) => o.key.trim())
                        .filter((k) => k && isManagedOptionKey(app, k))
                        .map((k) => `"${k}"`)
                        .join(", ")
                    })}
                  </p>
                )}
                {checks.dupKeys(app).length > 0 && (
                  <p className="text-xs text-destructive">
                    {t("errors.duplicateKeys", {
                      keys: checks
                        .dupKeys(app)
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
                  onClick={() => setBind(app, { options: [...optionRows, { key: "", value: "" }] })}
                >
                  {t("providers.add")}
                </Button>
              </div>
            </CollapsibleContent>
          </Collapsible>
        );
      })}
    </>
  );
}
