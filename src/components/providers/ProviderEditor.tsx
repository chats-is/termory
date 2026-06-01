import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { Eye, EyeOff, Loader2, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
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
      haikuModel: draft.claude?.haikuModel?.trim() || undefined
    };
    const claudeHasAny = !!(claude.sonnetModel || claude.opusModel || claude.haikuModel);
    const extraModels = (draft.opencode?.models ?? [])
      .map((m) => m.trim())
      .filter((m) => m.length > 0);
    const opencode = {
      providerId: draft.opencode?.providerId?.trim() || undefined,
      models: extraModels.length > 0 ? extraModels : undefined
    };
    const opencodeHasAny = !!(opencode.providerId || opencode.models);
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
      <DialogContent className="sm:max-w-lg max-h-[88vh] overflow-y-auto">
        <form onSubmit={handleSubmit} className="contents">
          <DialogHeader className="flex-row items-baseline gap-2">
            <DialogTitle>{isNew ? "Add provider" : "Edit provider"}</DialogTitle>
            <DialogDescription>{CLI_APP_LABEL[draft.app]}</DialogDescription>
          </DialogHeader>

          <div className="flex flex-col gap-4 py-2">
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

            <div className="grid gap-2">
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

            <div className="grid gap-2">
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
              <div className="grid gap-2">
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
              <details className="group rounded-md bg-muted/40 px-3 py-2">
                <summary className="cursor-pointer text-xs font-medium select-none">
                  Advanced — per-size routing (Sonnet / Opus / Haiku)
                </summary>
                <p className="text-xs text-muted-foreground mt-2">
                  When Claude Code's <code className="font-mono text-[11px]">/model</code> menu picks a size,
                  it sends the model id below to your provider. Leave blank to fall back to the main model.
                </p>
                <div className="flex flex-col gap-3 mt-3">
                  {(
                    [
                      ["sonnetModel", "Sonnet route", "e.g. gpt-5"],
                      ["opusModel", "Opus route", "e.g. claude-opus-4-7"],
                      ["haikuModel", "Haiku route", "e.g. deepseek-chat"]
                    ] as const
                  ).map(([key, label, ph]) => (
                    <div key={key} className="grid gap-1.5">
                      <Label htmlFor={`claude-${key}`} className="text-xs">
                        {label}
                      </Label>
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
              </details>
            )}
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
