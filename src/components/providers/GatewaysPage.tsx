import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import {
  CircleCheckBig,
  CircleOff,
  Loader2,
  Pencil,
  RadioTower,
  Trash2
} from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { BrandIcon } from "@/components/BrandIcon";
import { EmptyState } from "@/components/EmptyState";
import {
  ACTIVE_STATE_REFRESH_EVENT,
  CLI_APP_LABEL,
  CLI_APP_SOURCE_BADGE
} from "@/constants";
import {
  blankGateway,
  maskKey,
  providerFromBinding
} from "@/lib/provider-utils";
import type { ActiveState, CliApp, Gateway, GatewayBinding } from "@/types";
import { useT } from "@/i18n";

const GatewayEditor = React.lazy(() =>
  import("./GatewayEditor").then((m) => ({ default: m.GatewayEditor }))
);


/**
 * AI Gateway management — a second, independent kind of provider
 * management. Each gateway is one {baseUrl, apiKey} bound to one or more
 * CLIs; binding activation reuses the per-CLI `activate_provider` path
 * via a synthesized Provider, so active state reverse-derives the same
 * way as the Providers tab.
 */
export function GatewaysPage({
  gateways,
  setGateways,
  addSignal,
  markActive,
  activeProviderIds
}: {
  gateways: Gateway[];
  setGateways: React.Dispatch<React.SetStateAction<Gateway[]>>;
  /** Bumped by the ProvidersPage header "+" to open a fresh editor. */
  addSignal: number;
  /** Record / clear the per-CLI activation marker (shared with the
   * per-CLI provider list so identical-creds entries disambiguate). */
  markActive: (app: CliApp, id: string | null) => void;
  /** Per-CLI activation marker map — a binding is "in use" only when the
   * marker points at it (creds-matching alone is ambiguous when a
   * standalone provider shares the same endpoint). */
  activeProviderIds: Record<string, string>;
}) {
  const t = useT();
  const [editing, setEditing] = React.useState<Gateway | null>(null);
  const [editingIsNew, setEditingIsNew] = React.useState(false);
  const [activeStates, setActiveStates] = React.useState<
    Record<CliApp, ActiveState | null>
  >({ claude: null, codex: null, gemini: null, opencode: null });
  const [busy, setBusy] = React.useState<string | null>(null);

  // All gateway bindings materialized as providers — passed to the
  // reverse-derivation so a binding's synthesized id can be matched.
  const synthProviders = React.useMemo(
    () => gateways.flatMap((r) => r.bindings.map((b) => providerFromBinding(r, b))),
    [gateways]
  );

  const refreshActive = React.useCallback(async () => {
    try {
      const states = await invoke<ActiveState[]>("provider_active_states", {
        providers: synthProviders
      });
      const next: Record<CliApp, ActiveState | null> = {
        claude: null,
        codex: null,
        gemini: null,
        opencode: null
      };
      for (const s of states) next[s.app] = s;
      setActiveStates(next);
      // Nudge the parent ProvidersPage (mounted alongside us) to re-derive
      // too, so the per-CLI source list reflects a binding we just
      // activated/deactivated here.
      window.dispatchEvent(new Event(ACTIVE_STATE_REFRESH_EVENT));
    } catch (err) {
      toast.error(t("toast.readStateFailed", { error: String(err) }));
    }
  }, [synthProviders]);

  React.useEffect(() => {
    void refreshActive();
  }, [refreshActive]);

  const startNew = () => {
    setEditing(blankGateway());
    setEditingIsNew(true);
  };
  // Open a fresh editor when the ProvidersPage header "+" fires. Compare
  // against the value captured at mount (NOT `> 0`) so re-entering the
  // tab — which remounts this component while `addSignal` stays whatever
  // it was — doesn't re-open the dialog. Only an actual bump while
  // mounted triggers it.
  const lastSignal = React.useRef(addSignal);
  React.useEffect(() => {
    if (addSignal !== lastSignal.current) {
      lastSignal.current = addSignal;
      startNew();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [addSignal]);
  const startEdit = (r: Gateway) => {
    setEditing({ ...r });
    setEditingIsNew(false);
  };
  const closeEditor = () => {
    setEditing(null);
    setEditingIsNew(false);
  };

  const saveGateway = async (next: Gateway) => {
    const prev = gateways.find((r) => r.id === next.id);
    setGateways((cur) => {
      const exists = cur.some((r) => r.id === next.id);
      return exists ? cur.map((r) => (r.id === next.id ? next : r)) : [...cur, next];
    });
    closeEditor();

    // Reconcile live config for bindings that were ACTIVE before this
    // edit (a brand-new gateway has none, so nothing to do):
    //   - a binding dropped from the gateway → deactivate it
    //   - a binding kept → re-activate so base/key/model edits reach the
    //     CLI's live config (mirrors ProvidersPage.saveProvider). The
    //     binding's own id is stable across the edit, so "was active" is
    //     read off the pre-edit active state, and the kept binding's
    //     marker stays valid without re-marking.
    if (!prev) return;
    try {
      for (const pb of prev.bindings) {
        if (!isBindingActive(prev, pb)) continue;
        const stillBound = next.bindings.find((nb) => nb.app === pb.app);
        const oldSynth = providerFromBinding(prev, pb);
        if (!stillBound) {
          if (pb.app === "opencode") {
            await invoke("delete_provider", { provider: oldSynth });
          } else {
            await invoke("deactivate_provider", {
              app: pb.app,
              providersForApp: [oldSynth]
            });
          }
          markActive(pb.app, null); // binding gone → drop its stale marker
        } else {
          const newSynth = providerFromBinding(next, stillBound);
          await invoke("activate_provider", {
            provider: newSynth,
            providersForApp: [newSynth]
          });
          if (pb.app === "opencode") {
            await invoke("set_opencode_default_provider", { provider: newSynth });
          }
        }
      }
      await refreshActive();
    } catch (err) {
      toast.error(t("toast.savedButFailed", { error: String(err) }));
    }
  };

  const deleteGateway = async (gateway: Gateway) => {
    const confirmed = await ask(
      t("providers.deleteGatewayMsg", { name: gateway.name || t("providers.thisGateway") }),
      { title: t("providers.deleteGateway"), kind: "warning", okLabel: t("providers.delete"), cancelLabel: t("providers.cancel") }
    );
    if (!confirmed) return;
    // Clear any live config this AI Gateway's bindings injected — but only
    // the ones THIS gateway actually activated (marker), so we don't wipe
    // a standalone provider that happens to share the endpoint.
    for (const b of gateway.bindings) {
      const synth = providerFromBinding(gateway, b);
      try {
        if (b.app === "opencode") {
          await invoke("delete_provider", { provider: synth });
        } else if (isBindingActive(gateway, b)) {
          await invoke("deactivate_provider", {
            app: b.app,
            providersForApp: [synth]
          });
        }
        if (activeProviderIds[b.app] === b.id) markActive(b.app, null);
      } catch {
        /* best-effort cleanup */
      }
    }
    setGateways((cur) => cur.filter((r) => r.id !== gateway.id));
    await refreshActive();
  };

  const isBindingActive = (gateway: Gateway, b: GatewayBinding): boolean => {
    const state = activeStates[b.app];
    if (b.app === "opencode") {
      // OpenCode slots are keyed by id — no collision, reliable.
      return (state?.configuredProviderIds ?? []).includes(b.id);
    }
    // Single-slot: "in use" only when Termory's marker points at THIS
    // binding AND its creds still match the live config — so a coincidental
    // standalone provider with the same endpoint (whose activation wrote
    // the identical live config) doesn't make a just-added binding look
    // active.
    if (activeProviderIds[b.app] !== b.id) return false;
    const synth = providerFromBinding(gateway, b);
    const live = state?.liveSnapshot;
    return (
      !!live &&
      (live.baseUrl ?? "") === (synth.baseUrl ?? "") &&
      (live.apiKeyMasked ?? "") === maskKey(synth.apiKey ?? "")
    );
  };

  const activateBinding = async (gateway: Gateway, b: GatewayBinding) => {
    const synth = providerFromBinding(gateway, b);
    setBusy(synth.id);
    try {
      await invoke("activate_provider", {
        provider: synth,
        providersForApp: [synth]
      });
      if (b.app === "opencode") {
        await invoke("set_opencode_default_provider", { provider: synth });
      }
      markActive(b.app, synth.id);
      toast.success(t("toast.bindingActivated", { name: gateway.name, app: CLI_APP_LABEL[b.app] }));
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(null);
    }
  };

  const deactivateBinding = async (gateway: Gateway, b: GatewayBinding) => {
    const synth = providerFromBinding(gateway, b);
    setBusy(synth.id);
    try {
      if (b.app === "opencode") {
        await invoke("delete_provider", { provider: synth });
      } else {
        await invoke("deactivate_provider", {
          app: b.app,
          providersForApp: [synth]
        });
      }
      markActive(b.app, null);
      toast.success(t("toast.bindingDeactivated", { name: gateway.name, app: CLI_APP_LABEL[b.app] }));
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(null);
    }
  };

  // OpenCode-only: add / remove the provider slot in opencode.json WITHOUT
  // touching the top-level default (the two states are independent — see
  // `read_active_opencode`). Mirrors ProvidersPage.toggleGatewayEnabled.
  const toggleBindingEnabled = async (gateway: Gateway, b: GatewayBinding) => {
    if (b.app !== "opencode") return;
    const synth = providerFromBinding(gateway, b);
    const enabled = (activeStates.opencode?.configuredProviderIds ?? []).includes(
      b.id
    );
    setBusy(synth.id);
    try {
      if (enabled) {
        await invoke("delete_provider", { provider: synth });
        markActive(b.app, null);
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
      setBusy(null);
    }
  };

  // OpenCode-only: turn off the "in use" (default) state WITHOUT removing
  // the slot — clears the top-level `model` pointer only (deactivate_opencode
  // leaves enabled slots), so the binding stays configured but is no longer
  // the startup default. Mirrors the single-slot "In use — turn off".
  const clearBindingDefault = async (gateway: Gateway, b: GatewayBinding) => {
    if (b.app !== "opencode") return;
    const synth = providerFromBinding(gateway, b);
    setBusy(synth.id);
    try {
      await invoke("deactivate_provider", {
        app: b.app,
        providersForApp: [synth]
      });
      markActive(b.app, null);
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      <div className="flex-1 min-h-0 overflow-auto px-3 pb-0">
        <div className="flex flex-col gap-3">
          {gateways.length === 0 ? (
            <EmptyState
              icon={<RadioTower size={32} />}
              title={t("providers.noGateways")}
              description={t("providers.gwEmptyDesc")}
              action={{ label: t("providers.addProvider"), onClick: startNew }}
            />
          ) : (
            gateways.map((gateway) => (
              <Card
                key={gateway.id}
                className="p-3 gap-0 outline outline-1 outline-transparent shadow-sm bg-card hover:bg-accent/40 transition-colors"
              >
                <CardContent className="px-0 flex flex-col gap-2">
                <div className="flex items-start justify-between gap-3">
                  <div className="flex items-start gap-3 min-w-0">
                    {gateway.favicon ? (
                      <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-background shadow-sm">
                        <img
                          src={gateway.favicon}
                          alt=""
                          className="size-5 rounded-sm"
                        />
                      </span>
                    ) : (
                      <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-primary/15 text-primary">
                        <RadioTower size={20} />
                      </span>
                    )}
                    <div className="min-w-0">
                      <div className="text-lg font-medium truncate">
                        {gateway.name || t("providers.unnamedGateway")}
                      </div>
                      <div className="text-xs text-muted-foreground truncate">
                        {gateway.baseUrl || t("providers.noBaseUrl")}
                      </div>
                    </div>
                  </div>
                  <div className="flex items-center gap-1 shrink-0">
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      className="size-7"
                      aria-label={t("providers.editGateway")}
                      onClick={() => startEdit(gateway)}
                    >
                      <Pencil className="size-4" />
                    </Button>
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      className="size-7 text-muted-foreground hover:text-destructive"
                      aria-label={t("providers.deleteGateway")}
                      onClick={() => void deleteGateway(gateway)}
                    >
                      <Trash2 className="size-4" />
                    </Button>
                  </div>
                </div>

                {/* Bindings. */}
                {gateway.bindings.length === 0 ? (
                  <p className="text-xs text-muted-foreground">
                    {t("providers.noBindings")}
                  </p>
                ) : (
                  <div className="flex flex-col gap-1.5 pt-1">
                    {gateway.bindings.map((b) => {
                      const state = activeStates[b.app];
                      const isOpencode = b.app === "opencode";
                      // OpenCode has two independent states: the slot exists
                      // in opencode.json (configured) AND it's the top-level
                      // default (in use). Single-slot CLIs collapse to one.
                      const configured =
                        isOpencode &&
                        (state?.configuredProviderIds ?? []).includes(b.id);
                      const isDefault = isOpencode
                        ? state?.matchedProviderId === b.id
                        : isBindingActive(gateway, b);
                      return (
                        <div
                          key={b.id}
                          className="flex items-center justify-between gap-2"
                        >
                          {/* Boxed brand icon (under the gateway favicon) +
                              label/model (under the gateway name) — mirrors
                              the standalone provider card layout. */}
                          <div className="flex items-center gap-3 min-w-0">
                            <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-background shadow-sm [&_svg]:size-5">
                              <BrandIcon source={CLI_APP_SOURCE_BADGE[b.app]} />
                            </span>
                            <div className="min-w-0">
                              <div className="text-sm truncate">
                                {CLI_APP_LABEL[b.app]}
                              </div>
                              {b.model && (
                                <div className="text-xs text-muted-foreground font-mono truncate">
                                  {b.model}
                                </div>
                              )}
                            </div>
                          </div>
                          {isOpencode ? (
                            // Two-state: Set-as-default (or "In use" badge)
                            // + an enable/disable toggle for the slot.
                            <div className="flex items-center gap-1.5 shrink-0">
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Button
                                    variant="ghost"
                                    size="icon-sm"
                                    type="button"
                                    onClick={() =>
                                      void toggleBindingEnabled(gateway, b)
                                    }
                                    disabled={busy === b.id}
                                    aria-label={
                                      configured
                                        ? t("providers.removeFromOpencode")
                                        : t("providers.addToOpencode")
                                    }
                                  >
                                    {busy === b.id ? (
                                      <Loader2 className="size-4 animate-spin" />
                                    ) : configured ? (
                                      <CircleCheckBig className="size-4 text-green-600" />
                                    ) : (
                                      <CircleOff className="size-4 text-red-600" />
                                    )}
                                  </Button>
                                </TooltipTrigger>
                                <TooltipContent side="bottom">
                                  {configured
                                    ? t("providers.addedToOpencode")
                                    : t("providers.notInOpencode")}
                                </TooltipContent>
                              </Tooltip>
                              {isDefault ? (
                                <Button
                                  type="button"
                                  size="sm"
                                  variant="secondary"
                                  disabled={busy === b.id}
                                  onClick={() =>
                                    void clearBindingDefault(gateway, b)
                                  }
                                >{t("providers.inUseTurnOff")}</Button>
                              ) : (
                                <Button
                                  type="button"
                                  size="sm"
                                  variant="outline"
                                  disabled={busy === b.id}
                                  onClick={() => void activateBinding(gateway, b)}
                                >{t("providers.setDefault")}</Button>
                              )}
                            </div>
                          ) : (
                            <Button
                              type="button"
                              size="sm"
                              variant={isDefault ? "secondary" : "outline"}
                              disabled={busy === b.id}
                              onClick={() =>
                                isDefault
                                  ? void deactivateBinding(gateway, b)
                                  : void activateBinding(gateway, b)
                              }
                            >
                              {isDefault ? t("providers.inUseTurnOff") : t("providers.activate")}
                            </Button>
                          )}
                        </div>
                      );
                    })}
                  </div>
                )}
                </CardContent>
              </Card>
            ))
          )}
        </div>
      </div>

      {editing && (
        <React.Suspense fallback={null}>
          <GatewayEditor
            gateway={editing}
            isNew={editingIsNew}
            onSave={saveGateway}
            onClose={closeEditor}
          />
        </React.Suspense>
      )}
    </div>
  );
}
