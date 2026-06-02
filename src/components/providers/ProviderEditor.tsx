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
import { CLI_APP_LABEL, OPENCODE_PROVIDER_ID_OPTIONS } from "@/constants";
import { apiKeyHelp, baseUrlHelp, baseUrlPlaceholder } from "@/lib/provider-utils";
import type { Provider } from "@/types";

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
  // Model Mapping collapsible — opens by default when the provider
  // already has any mapping / 1M flag set, otherwise stays folded.
  const [mappingOpen, setMappingOpen] = React.useState(
    () =>
      !!(
        provider.claude?.sonnetModel ||
        provider.claude?.opusModel ||
        provider.claude?.haikuModel ||
        provider.claude?.sonnet1m ||
        provider.claude?.opus1m
      )
  );
  // Custom config overrides — open by default when the provider
  // already has some.
  const [overridesOpen, setOverridesOpen] = React.useState(
    () => (provider.overrides?.length ?? 0) > 0
  );
  const overrides = draft.overrides ?? [];
  const setOverrides = (next: { key: string; value: string }[]) =>
    update("overrides", next);
  // Always show at least one row so there's an input ready without
  // clicking "Add" first. The blank row is virtual until typed into
  // (empty-key rows are dropped on submit).
  const overrideRows =
    overrides.length > 0 ? overrides : [{ key: "", value: "" }];
  // Per-CLI help with REAL config keys (verified against each tool's
  // source): Claude settings.json / Codex config.toml / OpenCode
  // opencode.json take dot-path + typed values; Gemini's .env takes
  // env var names with verbatim string values.
  const overrideHelp = {
    claude:
      "e.g. cleanupPeriodDays, outputStyle, env.CLAUDE_CODE_USE_BEDROCK — dot-path keys; values typed automatically (env.* kept as strings).",
    codex:
      "e.g. model_reasoning_effort, approval_policy — dot-path keys; values typed automatically.",
    gemini:
      "e.g. GOOGLE_CLOUD_PROJECT, GOOGLE_GENAI_USE_VERTEXAI — each key is a .env variable name; values written verbatim.",
    opencode:
      "e.g. theme, autoupdate — dot-path keys; values typed automatically."
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
    (!modelRequired || (draft.model ?? "").trim().length > 0);

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
    // Trim every string field; collapse nested option objects to
    // undefined when nothing inside survived the trim, so providers.json
    // doesn't carry empty {claude: {}} / {opencode: {}} blocks.
    const claude = {
      sonnetModel: draft.claude?.sonnetModel?.trim() || undefined,
      opusModel: draft.claude?.opusModel?.trim() || undefined,
      haikuModel: draft.claude?.haikuModel?.trim() || undefined,
      sonnet1m: draft.claude?.sonnet1m || undefined,
      opus1m: draft.claude?.opus1m || undefined
    };
    const claudeHasAny = !!(
      claude.sonnetModel ||
      claude.opusModel ||
      claude.haikuModel ||
      claude.sonnet1m ||
      claude.opus1m
    );
    const extraModels = (draft.opencode?.models ?? [])
      .map((m) => m.trim())
      .filter((m) => m.length > 0);
    const opencode = {
      providerId: draft.opencode?.providerId?.trim() || undefined,
      models: extraModels.length > 0 ? extraModels : undefined
    };
    const opencodeHasAny = !!(opencode.providerId || opencode.models);
    // Drop override rows with a blank key; keep values verbatim.
    const cleanedOverrides = (draft.overrides ?? [])
      .map((o) => ({ key: o.key.trim(), value: o.value }))
      .filter((o) => o.key.length > 0);
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
      claude: claudeHasAny ? claude : undefined,
      opencode: opencodeHasAny ? opencode : undefined,
      overrides: cleanedOverrides.length > 0 ? cleanedOverrides : undefined,
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
                  value={draft.opencode?.providerId ?? "openai-compatible"}
                  onValueChange={(v) =>
                    update("opencode", {
                      ...(draft.opencode ?? {}),
                      providerId: v
                    })
                  }
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {OPENCODE_PROVIDER_ID_OPTIONS.map((opt) => (
                      <SelectItem key={opt.value} value={opt.value}>
                        {opt.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {
                    OPENCODE_PROVIDER_ID_OPTIONS.find(
                      (o) =>
                        o.value === (draft.opencode?.providerId ?? "openai-compatible")
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
                <Label htmlFor="provider-extra-models">Additional models</Label>
                <Input
                  id="provider-extra-models"
                  type="text"
                  className="font-mono"
                  placeholder="e.g. claude-sonnet-4-5, gpt-5-mini"
                  value={(draft.opencode?.models ?? []).join(", ")}
                  onChange={(e) =>
                    update("opencode", {
                      ...(draft.opencode ?? {}),
                      models: e.target.value
                        .split(",")
                        .map((s) => s.trim())
                        .filter((s) => s.length > 0)
                    })
                  }
                  autoComplete="off"
                  spellCheck={false}
                />
                <p className="text-xs text-muted-foreground">
                  Comma-separated extra model ids surfaced in OpenCode's picker. The primary "Model" above is always included.
                </p>
              </div>
            )}

            {draft.app === "claude" && (
              <Collapsible
                open={mappingOpen}
                onOpenChange={setMappingOpen}
                className="grid gap-2 sm:col-span-2"
              >
                <CollapsibleTrigger className="flex w-full items-center justify-between gap-1.5 text-sm font-medium select-none [&[data-state=open]>svg]:rotate-90">
                  Model Mapping
                  <ChevronRight className="size-3.5 text-muted-foreground transition-transform" />
                </CollapsibleTrigger>
                <CollapsibleContent className="-mx-1.5 overflow-hidden data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down">
                <div className="grid gap-3 px-1.5 py-1.5">
                <p className="text-xs text-muted-foreground">
                  When Claude Code's <code className="font-mono text-[11px]">/model</code> menu picks a size,
                  it sends the model id below to your provider. Leave blank to fall back to the main model.
                  Tick <span className="font-medium">1M</span> to append <code className="font-mono text-[11px]">[1m]</code> so that route requests the 1M context window (Haiku has no 1M variant).
                </p>
                <div className="flex flex-col gap-3">
                  {(
                    [
                      ["sonnetModel", "Sonnet", "e.g. gpt-5", "sonnet1m"],
                      ["opusModel", "Opus", "e.g. claude-opus-4-7", "opus1m"],
                      ["haikuModel", "Haiku", "e.g. deepseek-chat", null]
                    ] as const
                  ).map(([key, label, ph, oneMKey]) => (
                    <div key={key} className="grid gap-1.5">
                      <div className="flex items-center justify-between gap-2">
                        <Label htmlFor={`claude-${key}`} className="text-xs">
                          {label}
                        </Label>
                        {oneMKey && (
                          <label className="flex items-center gap-1 text-xs text-muted-foreground cursor-pointer select-none">
                            <input
                              type="checkbox"
                              className="size-3 accent-primary"
                              checked={draft.claude?.[oneMKey] ?? false}
                              onChange={(e) =>
                                update("claude", {
                                  ...(draft.claude ?? {}),
                                  [oneMKey]: e.target.checked
                                })
                              }
                            />
                            1M
                          </label>
                        )}
                      </div>
                      <Input
                        id={`claude-${key}`}
                        type="text"
                        className="font-mono"
                        placeholder={ph}
                        value={draft.claude?.[key] ?? ""}
                        onChange={(e) =>
                          update("claude", {
                            ...(draft.claude ?? {}),
                            [key]: e.target.value
                          })
                        }
                      />
                    </div>
                  ))}
                </div>
                </div>
                </CollapsibleContent>
              </Collapsible>
            )}

            <Collapsible
              open={overridesOpen}
              onOpenChange={setOverridesOpen}
              className="grid gap-2 sm:col-span-2"
            >
              <CollapsibleTrigger className="flex w-full items-center justify-between gap-1.5 text-sm font-medium select-none [&[data-state=open]>svg]:rotate-90">
                Custom config overrides
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
                    {overrideRows.map((o, i) => (
                      <div key={i} className="flex items-center gap-1.5">
                        <Input
                          aria-label="Override key"
                          className="font-mono flex-1"
                          placeholder="key"
                          value={o.key}
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
                          placeholder="value"
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
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          aria-label="Remove override"
                          onClick={() =>
                            setOverrides(overrideRows.filter((_, j) => j !== i))
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
