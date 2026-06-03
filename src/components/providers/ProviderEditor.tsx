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
  isManagedOptionKey
} from "@/lib/provider-utils";
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
  // the config-key encoding). Example keys still hint at what to type.
  const overrideHelp = {
    claude:
      "Map Claude Code's Sonnet / Opus / Haiku sizes to specific upstream models (the three rows below) — handy when your provider doesn't use Claude's native model names. Add [1m] after a model to use its 1M-token context. Other Claude Code preferences work here too.",
    codex:
      "Tune how Codex behaves with this provider — for example its reasoning effort or approval policy.",
    gemini:
      "Add environment variables Gemini CLI reads — for example to target a Google Cloud project or use Vertex AI instead of the public API.",
    opencode:
      "Tune this provider's connection in OpenCode — these go into its official `options` (e.g. timeout, setCacheKey, headers.X-Token) for request timeouts, prompt caching, or custom request headers."
  }[draft.app];
  const modelDatalistId = React.useId();
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
            <DialogTitle>{isNew ? "Add provider" : "Edit provider"}</DialogTitle>
            <DialogDescription>{CLI_APP_LABEL[draft.app]}</DialogDescription>
          </DialogHeader>

          <div className="grid grid-cols-1 sm:grid-cols-2 gap-x-4 gap-y-4 items-start -mx-6 max-h-[65vh] overflow-y-auto px-6 py-1">
            <div className="grid gap-2">
              <Label htmlFor="provider-name">Name *</Label>
              <Input
                id="provider-name"
                ref={firstFieldRef}
                type="text"
                placeholder="Display name for this provider"
                value={draft.name}
                onChange={(e) => update("name", e.target.value)}
                autoComplete="off"
                autoCorrect="off"
                autoCapitalize="off"
                spellCheck={false}
                required
              />
            </div>

            <div className="grid gap-2">
              <Label htmlFor="provider-baseurl">Base URL *</Label>
              <Input
                id="provider-baseurl"
                type="text"
                className="font-mono"
                placeholder={baseUrlPlaceholder(draft.app)}
                value={draft.baseUrl ?? ""}
                onChange={(e) => update("baseUrl", e.target.value)}
                required
              />
              <p className="text-xs text-muted-foreground">{baseUrlHelp(draft.app)}</p>
            </div>

            <div className={`grid gap-2 ${isOpencode ? "" : "sm:col-span-2"}`}>
              <Label htmlFor="provider-apikey">API key</Label>
              <div className="flex gap-1.5">
                <Input
                  id="provider-apikey"
                  type={revealKey ? "text" : "password"}
                  className="font-mono"
                  placeholder="sk-… (leave blank to fill in later)"
                  value={draft.apiKey ?? ""}
                  onChange={(e) => update("apiKey", e.target.value)}
                  autoComplete="off"
                  spellCheck={false}
                />
                <Button
                  type="button"
                  variant="outline"
                  size="icon"
                  onClick={() => setRevealKey((c) => !c)}
                  aria-label={revealKey ? "Hide API key" : "Show API key"}
                >
                  {revealKey ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
                </Button>
              </div>
              <p className="text-xs text-muted-foreground">{apiKeyHelp(draft.app)}</p>
            </div>

            {isOpencode && (
              <div className="grid gap-2">
                <Label>AI SDK *</Label>
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
                <p className="text-xs text-muted-foreground">
                  {
                    OPENCODE_NPM_OPTIONS.find(
                      (o) => o.value === (draft.npm ?? OPENCODE_DEFAULT_NPM)
                    )?.hint
                  }
                </p>
              </div>
            )}

            <div className="grid gap-2 sm:col-span-2">
              <Label htmlFor="provider-model">{`Model${modelRequired ? " *" : ""}`}</Label>
              <div className="flex gap-1.5">
                <Input
                  id="provider-model"
                  type="text"
                  className="font-mono"
                  placeholder={
                    modelRequired
                      ? "Enter the model id (e.g. claude-opus-4-7)"
                      : "Leave blank to use the default"
                  }
                  value={draft.model ?? ""}
                  onChange={(e) => update("model", e.target.value)}
                  list={modelOptions.length > 0 ? modelDatalistId : undefined}
                  autoComplete="off"
                  required={modelRequired}
                />
                <Button
                  type="button"
                  variant="outline"
                  size="icon"
                  onClick={() => void fetchModels()}
                  disabled={!canFetchModels}
                  aria-label="Fetch available models from API"
                >
                  {fetchingModels ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    <RefreshCw className="size-4" />
                  )}
                </Button>
              </div>
              {modelOptions.length > 0 && (
                <datalist id={modelDatalistId}>
                  {modelOptions.map((m) => (
                    <option key={m} value={m} />
                  ))}
                </datalist>
              )}
              {modelError && (
                <p className="text-xs text-destructive">{modelError}</p>
              )}
              {!modelError && modelOptions.length > 0 && (
                <p className="text-xs text-muted-foreground">
                  {modelOptions.length} models available — start typing to pick
                </p>
              )}
            </div>

            {isOpencode && (
              <div className="grid gap-2 sm:col-span-2">
                <Label>Additional models</Label>
                <p className="text-xs text-muted-foreground">
                  Extra models surfaced in OpenCode's picker (the primary
                  "Model" above is always included). ID is the model id; name
                  is the display label (defaults to the id if left blank).
                </p>
                <div className="flex flex-col gap-2">
                  {modelRows.map((m, i) => (
                    <div key={i} className="flex items-center gap-1.5">
                      <Input
                        aria-label="Model ID"
                        className="font-mono flex-1"
                        placeholder="model id"
                        value={m.id}
                        onChange={(e) =>
                          setModelList(
                            modelRows.map((r, j) =>
                              j === i ? { ...r, id: e.target.value } : r
                            )
                          )
                        }
                        autoComplete="off"
                        spellCheck={false}
                      />
                      <Input
                        aria-label="Model display name"
                        className="flex-1"
                        placeholder="Display name (optional)"
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
                        aria-label="Remove model"
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
                  Add model
                </Button>
              </div>
            )}

            <Collapsible
              open={overridesOpen}
              onOpenChange={setOverridesOpen}
              className="grid gap-2 sm:col-span-2"
            >
              <CollapsibleTrigger className="flex w-full items-center justify-between gap-1.5 text-sm font-medium select-none [&[data-state=open]>svg]:rotate-90">
                Advanced settings
                <ChevronRight className="size-3.5 text-muted-foreground transition-transform" />
              </CollapsibleTrigger>
              <CollapsibleContent className="-mx-1.5 overflow-hidden data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down">
                <div className="grid gap-3 px-1.5 py-1.5">
                  <p className="text-xs text-muted-foreground">
                    Extra settings merged into {CLI_APP_LABEL[draft.app]}'s config
                    while this provider is active, and removed when you switch away.{" "}
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
                          <Input
                            aria-label="Override key"
                            aria-invalid={isDup || isManaged || undefined}
                            className={`font-mono flex-1${
                              isProtected ? " text-muted-foreground" : ""
                            }`}
                            placeholder="key"
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
                          <Input
                            aria-label="Override value"
                            className="font-mono flex-1"
                            placeholder={
                              isProtected ? "model id (append [1m] for 1M)" : "value"
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
                              aria-label="Remove override"
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
                      Duplicate key{duplicateKeys.length > 1 ? "s" : ""}:{" "}
                      {duplicateKeys.map((k) => `"${k}"`).join(", ")} — each key
                      must be unique.
                    </p>
                  )}
                  {managedKeys.length > 0 && (
                    <p className="text-xs text-destructive">
                      {managedKeys.map((k) => `"${k}"`).join(", ")}{" "}
                      {managedKeys.length > 1 ? "are" : "is"} already managed by
                      the fields above (Base URL / API key / Model
                      {isOpencode ? " / AI SDK" : ""}) — set{" "}
                      {managedKeys.length > 1 ? "them" : "it"} there instead, not
                      here.
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
                    Add override
                  </Button>
                </div>
              </CollapsibleContent>
            </Collapsible>
          </div>

          <DialogFooter>
            <Button type="button" variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!canSave || saving}>
              {saving ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  {isNew ? "Creating…" : "Saving…"}
                </>
              ) : isNew ? (
                "Create"
              ) : (
                "Save"
              )}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
