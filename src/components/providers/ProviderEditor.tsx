import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronRight, Eye, EyeOff, Loader2, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ModelCombobox } from "@/components/ModelCombobox";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import {
  CLI_APP_LABEL,
  OPENCODE_DEFAULT_NPM,
  OPENCODE_NPM_OPTIONS
} from "@/constants";
import {
  apiKeyHelp,
  baseUrlHelp,
  baseUrlPlaceholder,
  isManagedOptionKey,
  overrideHelpFor
} from "@/lib/provider-utils";
import { INPUT_NO_AUTO } from "@/lib/utils";
import { useT } from "@/i18n";
import type { Provider } from "@/types";

// Default override rows seeded for a fresh Claude provider — the per-size
// `/model` routing keys. Shown as editable templates; blank ones are
// dropped on save. Append `[1m]` to a value to request the 1M context
// window for that route (e.g. `claude-sonnet-4-6[1m]`).
const CLAUDE_OVERRIDE_TEMPLATE: { key: string; value: string }[] = [
  { key: "env.ANTHROPIC_DEFAULT_SONNET_MODEL", value: "" },
  { key: "env.ANTHROPIC_DEFAULT_OPUS_MODEL", value: "" },
  { key: "env.ANTHROPIC_DEFAULT_HAIKU_MODEL", value: "" }
];

export function ProviderEditor({
  provider,
  isNew,
  onSave,
  onClose
}: {
  provider: Provider;
  isNew: boolean;
  onSave: (p: Provider) => void;
  onClose: () => void;
}) {
  const t = useT();
  const [draft, setDraft] = React.useState<Provider>(provider);
  const [revealKey, setRevealKey] = React.useState(false);
  const firstFieldRef = React.useRef<HTMLInputElement>(null);
  const [modelOptions, setModelOptions] = React.useState<string[]>([]);
  const [fetchingModels, setFetchingModels] = React.useState(false);
  const [modelError, setModelError] = React.useState<string | null>(null);
  const [saving, setSaving] = React.useState(false);
  // Custom config overrides — open by default when the provider already
  // has some, or for Claude (where the seeded per-size routing template
  // replaces the old Model Mapping section).
  const [overridesOpen, setOverridesOpen] = React.useState(
    () => (provider.options?.length ?? 0) > 0 || provider.app === "claude"
  );
  const overrides = draft.options ?? [];
  const setOverrides = (next: { key: string; value: string }[]) =>
    update("options", next);
  // For Claude, the three per-size routing keys are ALWAYS present as a
  // protected template: their key is read-only and they can't be deleted
  // — the user only fills the value (blank ones drop on submit). Any
  // other override rows the user adds sit after them and stay freely
  // editable / removable. Other apps just show one blank row to start.
  const protectedKeys =
    draft.app === "claude" ? CLAUDE_OVERRIDE_TEMPLATE.map((t) => t.key) : [];
  const overrideRows: { key: string; value: string }[] = (() => {
    if (draft.app === "claude") {
      const byKey = new Map(overrides.map((o) => [o.key, o.value]));
      const template = CLAUDE_OVERRIDE_TEMPLATE.map((t) => ({
        key: t.key,
        value: byKey.get(t.key) ?? ""
      }));
      const extras = overrides.filter((o) => !protectedKeys.includes(o.key));
      return [...template, ...extras];
    }
    return overrides.length > 0 ? overrides : [{ key: "", value: "" }];
  })();
  // Duplicate non-blank keys would silently overwrite each other on
  // activation (last wins), so block the save until they're resolved.
  const duplicateKeys = (() => {
    const seen = new Set<string>();
    const dups = new Set<string>();
    for (const o of overrideRows) {
      const k = o.key.trim();
      if (!k) continue;
      if (seen.has(k)) dups.add(k);
      else seen.add(k);
    }
    return [...dups];
  })();
  // Keys already managed by the provider's own fields (Base URL / API key
  // / Model / AI SDK). The backend silently skips them, so block the save
  // and tell the user to use the dedicated field instead. Protected
  // Claude template keys are not managed, so they never trip this.
  const managedKeys = [
    ...new Set(
      overrideRows
        .map((o) => o.key.trim())
        .filter((k) => k && isManagedOptionKey(draft.app, k))
    )
  ];
  // OpenCode extra-models editor rows (Model ID + display name). Same
  // add/delete UX as Advanced settings; always show one row to start.
  const modelList = draft.models ?? [];
  const setModelList = (next: { id: string; name: string }[]) =>
    update("models", next);
  const modelRows =
    modelList.length > 0 ? modelList : [{ id: "", name: "" }];
  // Per-CLI help — what these settings let you DO (plain language, not
  // the config-key encoding). Shared with the gateway binding editor.
  const overrideHelp = overrideHelpFor(draft.app, t);
  // Snapshot the originally-loaded URL so we can decide whether to
  // refetch the favicon on save. Captured once at mount — re-rendering
  // with a new `provider` prop happens only on `isNew` flips.
  const originalBaseUrlRef = React.useRef(provider.baseUrl ?? "");

  React.useEffect(() => {
    firstFieldRef.current?.focus();
  }, []);
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const update = <K extends keyof Provider>(key: K, value: Provider[K]) => {
    setDraft((cur) => ({ ...cur, [key]: value }));
  };

  // Universal required fields: name + baseUrl. apiKey is always
  // optional (OpenCode supports env-var references; empty is allowed
  // and Termory just leaves the field unset). OpenCode additionally
  // needs a primary model — without it OpenCode's picker can't surface
  // the provider.
  const isOpencode = draft.app === "opencode";
  const modelRequired = isOpencode;
  const canSave =
    draft.name.trim().length > 0 &&
    (draft.baseUrl ?? "").trim().length > 0 &&
    (!modelRequired || (draft.model ?? "").trim().length > 0) &&
    duplicateKeys.length === 0 &&
    managedKeys.length === 0;

  const canFetchModels = (draft.baseUrl ?? "").trim().length > 0 && !fetchingModels;

  const fetchModels = async () => {
    if (!canFetchModels) return;
    setFetchingModels(true);
    setModelError(null);
    try {
      const result = await invoke<{
        ok: boolean;
        models: string[];
        status: number | null;
        message: string;
      }>("fetch_provider_models", { provider: draft });
      setModelOptions(result.models);
      if (!result.ok) {
        setModelError(
          result.status ? `${result.status} ${result.message}` : result.message
        );
      }
    } catch (err) {
      setModelError(String(err));
    } finally {
      setFetchingModels(false);
    }
  };

  // Auto-fetch the model list (debounced) once base URL + key are present,
  // deduped per (base, key) so it fires once per unique pair.
  const lastFetched = React.useRef("");
  React.useEffect(() => {
    const base = (draft.baseUrl ?? "").trim();
    const key = (draft.apiKey ?? "").trim();
    if (!base || !key) return;
    const sig = `${base}\n${key}`;
    if (lastFetched.current === sig) return;
    const t = setTimeout(() => {
      lastFetched.current = sig;
      void fetchModels();
    }, 600);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft.baseUrl, draft.apiKey]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canSave || saving) return;
    // OpenCode-only top-level fields. `npm` is dropped when it equals
    // the default (the backend already falls back to it). `models` drops
    // rows with a blank id; a blank name is kept "" (backend defaults it
    // to the id at write time).
    const models = (draft.models ?? [])
      .map((m) => ({ id: m.id.trim(), name: m.name.trim() }))
      .filter((m) => m.id.length > 0);
    const npm =
      isOpencode && draft.npm && draft.npm.trim() !== OPENCODE_DEFAULT_NPM
        ? draft.npm.trim()
        : undefined;
    // Drop override rows missing a key OR a value — a seeded template row
    // the user never filled isn't a real override. Values kept verbatim.
    const cleanedOverrides = (draft.options ?? [])
      .map((o) => ({ key: o.key.trim(), value: o.value.trim() }))
      .filter((o) => o.key.length > 0 && o.value.length > 0);
    const trimmedBaseUrl = draft.baseUrl?.trim() || undefined;

    // Refetch the favicon when the URL is new OR has just changed.
    // Skip the network when the user is editing other fields and the
    // host hasn't moved — the cached base64 in `draft.favicon` is
    // still valid. Fetch failure is silent (favicon stays whatever it
    // was) so a slow / 404 / offline upstream never blocks the save.
    let favicon = draft.favicon;
    const urlChanged =
      (trimmedBaseUrl ?? "") !== (originalBaseUrlRef.current ?? "");
    if (trimmedBaseUrl && (urlChanged || !favicon)) {
      setSaving(true);
      try {
        const fetched = await invoke<string | null>(
          "fetch_provider_favicon",
          { url: trimmedBaseUrl }
        );
        if (fetched) favicon = fetched;
        else if (urlChanged) favicon = undefined; // moved host → drop stale
      } catch {
        /* leave favicon as-is */
      } finally {
        setSaving(false);
      }
    }
    onSave({
      ...draft,
      name: draft.name.trim(),
      baseUrl: trimmedBaseUrl,
      apiKey: draft.apiKey?.trim() || undefined,
      model: draft.model?.trim() || undefined,
      npm,
      models: models.length > 0 ? models : undefined,
      options: cleanedOverrides.length > 0 ? cleanedOverrides : undefined,
      favicon
    });
  };

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent className="sm:max-w-2xl">
        <form onSubmit={handleSubmit} className="contents">
          <DialogHeader className="flex-row items-baseline gap-2">
            <DialogTitle>{isNew ? t("providers.addProvider") : t("providers.editProvider")}</DialogTitle>
            <DialogDescription>{CLI_APP_LABEL[draft.app]}</DialogDescription>
          </DialogHeader>

          <div className="grid grid-cols-1 sm:grid-cols-2 gap-x-4 gap-y-4 items-start -mx-6 max-h-[65vh] overflow-y-auto px-6 py-1">
            <div className="grid gap-2 sm:col-span-2">
              <Label htmlFor="provider-name">{t("providers.name")} *</Label>
              <Input {...INPUT_NO_AUTO}
                id="provider-name"
                ref={firstFieldRef}
                type="text"
                placeholder={t("providers.displayNameForProvider")}
                value={draft.name}
                onChange={(e) => update("name", e.target.value)}
                autoComplete="off"
                autoCorrect="off"
                autoCapitalize="off"
                spellCheck={false}
                required
              />
            </div>

            <div className="grid gap-2 sm:col-span-2">
              <Label htmlFor="provider-baseurl">{t("providers.baseUrl")} *</Label>
              <Input {...INPUT_NO_AUTO}
                id="provider-baseurl"
                type="text"
                className="font-mono"
                placeholder={baseUrlPlaceholder(draft.app, draft.npm)}
                value={draft.baseUrl ?? ""}
                onChange={(e) => update("baseUrl", e.target.value)}
                required
              />
              <p className="text-xs text-muted-foreground">
                {baseUrlHelp(draft.app, draft.npm, t)}
              </p>
            </div>

            <div className="grid gap-2 sm:col-span-2">
              <Label htmlFor="provider-apikey">{t("providers.apiKey")}</Label>
              <div className="relative">
                <Input {...INPUT_NO_AUTO}
                  id="provider-apikey"
                  type={revealKey ? "text" : "password"}
                  className="font-mono pr-9"
                  placeholder={t("providers.apiKeyBlank")}
                  value={draft.apiKey ?? ""}
                  onChange={(e) => update("apiKey", e.target.value)}
                />
                <button
                  type="button"
                  onClick={() => setRevealKey((c) => !c)}
                  aria-label={revealKey ? t("providers.hideApiKey") : t("providers.showApiKey")}
                  className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                >
                  {revealKey ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
                </button>
              </div>
              <p className="text-xs text-muted-foreground">{apiKeyHelp(draft.app, t)}</p>
            </div>

            {isOpencode && (
              <div className="grid gap-2">
                <Label>{t("providers.aiSdk")} *</Label>
                <Select
                  value={draft.npm ?? OPENCODE_DEFAULT_NPM}
                  onValueChange={(v) => update("npm", v)}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {OPENCODE_NPM_OPTIONS.map((opt) => (
                      <SelectItem key={opt.value} value={opt.value}>
                        {opt.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            )}

            <div className="grid gap-2 sm:col-span-2">
              <Label>{`${t("providers.model")}${modelRequired ? " *" : ""}`}</Label>
              <ModelCombobox
                ariaLabel={`Model${modelRequired ? " *" : ""}`}
                value={draft.model ?? ""}
                onValueChange={(v) => update("model", v)}
                options={modelOptions}
                loading={fetchingModels}
                ariaInvalid={modelRequired && !(draft.model ?? "").trim()}
              />
              {modelError && (
                <p className="text-xs text-destructive">{modelError}</p>
              )}
              {!modelError && (fetchingModels || modelOptions.length > 0) && (
                <p className="text-xs text-muted-foreground">
                  {fetchingModels
                    ? t("help.fetchingModels")
                    : t("help.modelsAvailable", { n: modelOptions.length })}
                </p>
              )}
            </div>

            {isOpencode && (
              <div className="grid gap-2 sm:col-span-2">
                <Label>{t("providers.additionalModels")}</Label>
                <p className="text-xs text-muted-foreground">
                  {t("help.extraModels")}
                </p>
                <div className="flex flex-col gap-2">
                  {modelRows.map((m, i) => (
                    <div key={i} className="flex items-center gap-1.5">
                      <ModelCombobox
                        ariaLabel={t("providers.modelId")}
                        value={m.id}
                        onValueChange={(v) =>
                          setModelList(
                            modelRows.map((r, j) =>
                              j === i ? { ...r, id: v } : r
                            )
                          )
                        }
                        options={modelOptions}
                        loading={fetchingModels}
                        className="flex-1"
                      />
                      <Input {...INPUT_NO_AUTO}
                        aria-label={t("providers.modelDisplayName")}
                        className="flex-1"
                        placeholder={t("providers.displayNameOptional")}
                        value={m.name}
                        onChange={(e) =>
                          setModelList(
                            modelRows.map((r, j) =>
                              j === i ? { ...r, name: e.target.value } : r
                            )
                          )
                        }
                        autoComplete="off"
                        spellCheck={false}
                      />
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        aria-label={t("providers.removeModel")}
                        onClick={() =>
                          setModelList(modelRows.filter((_, j) => j !== i))
                        }
                      >
                        <Trash2 className="size-4" />
                      </Button>
                    </div>
                  ))}
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="justify-self-start"
                  onClick={() =>
                    setModelList([...modelList, { id: "", name: "" }])
                  }
                >
                  <Plus className="size-4" />
                  {t("providers.add")}
                </Button>
              </div>
            )}

            <Collapsible
              open={overridesOpen}
              onOpenChange={setOverridesOpen}
              className="grid gap-2 sm:col-span-2"
            >
              <CollapsibleTrigger className="flex w-full items-center justify-between gap-1.5 text-sm font-medium select-none [&[data-state=open]>svg]:rotate-90">{t("providers.advancedSettings")}<ChevronRight className="size-3.5 text-muted-foreground transition-transform" />
              </CollapsibleTrigger>
              <CollapsibleContent className="-mx-1.5 overflow-hidden data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down">
                <div className="grid gap-3 px-1.5 py-1.5">
                  <p className="text-xs text-muted-foreground">
                    {t("help.overrideIntro", { app: CLI_APP_LABEL[draft.app] })}{" "}
                    {overrideHelp}
                  </p>
                  <div className="flex flex-col gap-2">
                    {overrideRows.map((o, i) => {
                      // Seeded Claude routing keys are protected: read-only
                      // key, no delete. Only the value is editable.
                      const isProtected = protectedKeys.includes(o.key);
                      const isDup =
                        !isProtected && duplicateKeys.includes(o.key.trim());
                      const isManaged =
                        !isProtected && managedKeys.includes(o.key.trim());
                      return (
                        <div key={i} className="flex items-center gap-1.5">
                          <Input {...INPUT_NO_AUTO}
                            aria-label={t("providers.overrideKey")}
                            aria-invalid={isDup || isManaged || undefined}
                            className={`font-mono flex-1${
                              isProtected ? " text-muted-foreground" : ""
                            }`}
                            placeholder={t("providers.keyUpper")}
                            value={o.key}
                            readOnly={isProtected}
                            tabIndex={isProtected ? -1 : undefined}
                            onChange={(e) =>
                              setOverrides(
                                overrideRows.map((r, j) =>
                                  j === i ? { ...r, key: e.target.value } : r
                                )
                              )
                            }
                            autoComplete="off"
                            spellCheck={false}
                          />
                          <Input {...INPUT_NO_AUTO}
                            aria-label={t("providers.overrideValue")}
                            className="font-mono flex-1"
                            placeholder={
                              isProtected ? t("providers.modelIdHint") : t("providers.valueUpper")
                            }
                            value={o.value}
                            onChange={(e) =>
                              setOverrides(
                                overrideRows.map((r, j) =>
                                  j === i ? { ...r, value: e.target.value } : r
                                )
                              )
                            }
                            autoComplete="off"
                            spellCheck={false}
                          />
                          {isProtected ? (
                            // Protected default route — no delete icon, but
                            // keep its footprint so inputs stay aligned.
                            <span className="size-9 shrink-0" aria-hidden />
                          ) : (
                            <Button
                              type="button"
                              variant="ghost"
                              size="icon"
                              aria-label={t("providers.removeOverride")}
                              onClick={() =>
                                setOverrides(
                                  overrideRows.filter((_, j) => j !== i)
                                )
                              }
                            >
                              <Trash2 className="size-4" />
                            </Button>
                          )}
                        </div>
                      );
                    })}
                  </div>
                  {duplicateKeys.length > 0 && (
                    <p className="text-xs text-destructive">
                      {t("errors.duplicateKeys", {
                        keys: duplicateKeys.map((k) => `"${k}"`).join(", ")
                      })}
                    </p>
                  )}
                  {managedKeys.length > 0 && (
                    <p className="text-xs text-destructive">
                      {t("errors.managedKeys", {
                        keys: managedKeys.map((k) => `"${k}"`).join(", ")
                      })}
                    </p>
                  )}
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="justify-self-start"
                    onClick={() =>
                      setOverrides([...overrides, { key: "", value: "" }])
                    }
                  >
                    <Plus className="size-4" />
                    {t("providers.add")}
                  </Button>
                </div>
              </CollapsibleContent>
            </Collapsible>
          </div>

          <DialogFooter>
            <Button type="button" variant="ghost" onClick={onClose}>{t("providers.cancel")}</Button>
            <Button type="submit" disabled={!canSave || saving}>
              {saving ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  {isNew ? t("providers.creating") : t("providers.saving")}
                </>
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
