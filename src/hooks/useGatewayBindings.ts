import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { ACTIVE_STATE_REFRESH_EVENT, CLI_APP_LABEL } from "@/constants";
import { isMultiSlot, maskKey, providerFromBinding } from "@/lib/provider-utils";
import type { ActiveState, CliApp, Gateway, GatewayBinding } from "@/types";
import { useT } from "@/i18n";

const EMPTY_STATES: Record<CliApp, ActiveState | null> = {
  claude: null,
  "claude-desktop": null,
  codex: null,
  gemini: null,
  opencode: null,
  grok: null
};

/**
 * Gateway-binding activation, shared by the Gateways tab and the Router
 * page (whose "Use in tools" is the router's own gateway entry). A binding
 * activates through the per-CLI `activate_provider` path via a synthesized
 * Provider, so active state reverse-derives the same way as the Providers
 * tab. One implementation, or the two surfaces drift (CLAUDE.md: a guard
 * added to only one surface leaves the other as a way around it).
 */
export function useGatewayBindings({
  gateways,
  markActive,
  activeProviderIds,
  codexFollowForBinding
}: {
  gateways: Gateway[];
  /** Record / clear the per-CLI activation marker. */
  markActive: (app: CliApp, id: string | null) => void;
  /** Per-CLI activation marker map — a binding is "in use" only when the
   * marker points at it (creds-matching alone is ambiguous when a
   * standalone provider shares the same endpoint). */
  activeProviderIds: Record<string, string>;
  /** Run a Codex binding switch through the "follow sessions?" prompt. */
  codexFollowForBinding: (
    direction: "toCustom" | "toOfficial",
    label: string,
    activate: () => Promise<boolean>,
    opts?: { required?: boolean }
  ) => Promise<void>;
}) {
  const t = useT();
  const [activeStates, setActiveStates] =
    React.useState<Record<CliApp, ActiveState | null>>(EMPTY_STATES);
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
      const next: Record<CliApp, ActiveState | null> = { ...EMPTY_STATES };
      for (const s of states) next[s.app] = s;
      setActiveStates(next);
      // Nudge a ProvidersPage mounted alongside to re-derive too, so the
      // per-CLI source list reflects a binding just activated here.
      window.dispatchEvent(new Event(ACTIVE_STATE_REFRESH_EVENT));
    } catch (err) {
      toast.error(t("toast.readStateFailed", { error: String(err) }));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [synthProviders]);

  React.useEffect(() => {
    void refreshActive();
  }, [refreshActive]);

  const isBindingActive = React.useCallback(
    (gateway: Gateway, b: GatewayBinding): boolean => {
      const state = activeStates[b.app];
      if (isMultiSlot(b.app)) {
        // Multi-slot (OpenCode/Grok) slots are keyed by id — no collision.
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
    },
    [activeStates, activeProviderIds]
  );

  const activateBinding = async (gateway: Gateway, b: GatewayBinding) => {
    const synth = providerFromBinding(gateway, b);
    const doActivate = async (): Promise<boolean> => {
      setBusy(synth.id);
      try {
        await invoke("activate_provider", {
          provider: synth,
          providersForApp: [synth]
        });
        if (isMultiSlot(b.app)) {
          await invoke("set_default_provider", {
            provider: synth,
            providersForApp: synthProviders.filter((s) => s.app === b.app)
          });
        }
        markActive(b.app, synth.id);
        toast.success(t("toast.bindingActivated", { name: gateway.name, app: CLI_APP_LABEL[b.app] }));
        await refreshActive();
        return true;
      } catch (err) {
        toast.error(String(err));
        return false;
      } finally {
        setBusy(null);
      }
    };
    // Codex official→custom: switching the API endpoint hides a project's prior
    // sessions from `codex resume`, so prompt to follow them — same as the
    // Providers tab. Only on official→custom (custom→custom keeps the bucket).
    if (b.app === "codex" && activeStates.codex?.kind === "official") {
      await codexFollowForBinding("toCustom", synth.name || gateway.name, doActivate);
      return;
    }
    await doActivate();
  };

  const deactivateBinding = async (gateway: Gateway, b: GatewayBinding) => {
    const synth = providerFromBinding(gateway, b);
    const doDeactivate = async (): Promise<boolean> => {
      setBusy(synth.id);
      try {
        if (isMultiSlot(b.app)) {
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
        return true;
      } catch (err) {
        toast.error(String(err));
        return false;
      } finally {
        setBusy(null);
      }
    };
    // Codex custom→official: turning the binding off folds Codex back to the
    // openai bucket, which can hide sessions moved to a custom provider — prompt
    // to bring them back, same as the Providers tab's "Set Official". (Codex is
    // on a custom config now, so kind is custom/unmanaged, not official.)
    const codexKind = activeStates.codex?.kind;
    if (b.app === "codex" && (codexKind === "custom" || codexKind === "unmanaged")) {
      await codexFollowForBinding("toOfficial", t("providers.official"), doDeactivate);
      return;
    }
    await doDeactivate();
  };

  // Multi-slot only (OpenCode/Grok): add / remove the provider slot WITHOUT
  // touching the startup default (the two states are independent — see
  // `read_active_opencode` / `read_active_grok`). Mirrors
  // ProvidersPage.toggleGatewayEnabled.
  const toggleBindingEnabled = async (gateway: Gateway, b: GatewayBinding) => {
    if (!isMultiSlot(b.app)) return;
    const synth = providerFromBinding(gateway, b);
    const enabled = (activeStates[b.app]?.configuredProviderIds ?? []).includes(b.id);
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

  // Multi-slot only (OpenCode/Grok): turn off the "in use" (default) state
  // WITHOUT removing the slot — clears the startup-default pointer only
  // (deactivate leaves enabled slots), so the binding stays configured but is
  // no longer the default. Mirrors the single-slot "In use — turn off".
  const clearBindingDefault = async (gateway: Gateway, b: GatewayBinding) => {
    if (!isMultiSlot(b.app)) return;
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

  // Reconcile live config for bindings that were ACTIVE before an edit:
  //   - a binding dropped from the gateway → deactivate it
  //   - a binding kept → re-activate so base/key/model edits reach the
  //     CLI's live config (mirrors ProvidersPage.saveProvider). The
  //     binding's own id is stable across the edit, so "was active" is
  //     read off the pre-edit active state, and the kept binding's
  //     marker stays valid without re-marking.
  const reconcileAfterEdit = async (prev: Gateway, next: Gateway) => {
    try {
      for (const pb of prev.bindings) {
        if (!isBindingActive(prev, pb)) continue;
        const stillBound = next.bindings.find((nb) => nb.app === pb.app);
        const oldSynth = providerFromBinding(prev, pb);
        if (!stillBound) {
          if (pb.app === "codex") {
            // Unbinding the Codex binding in use folds Codex back to the
            // openai bucket — the same custom→official switch as the
            // Gateways tab's deactivate, so it goes through the same
            // follow-sessions prompt (or the keep-all setting).
            const doDeactivate = async (): Promise<boolean> => {
              try {
                await invoke("deactivate_provider", {
                  app: pb.app,
                  providersForApp: [oldSynth]
                });
                markActive(pb.app, null);
                await refreshActive();
                return true;
              } catch (err) {
                toast.error(t("toast.savedButFailed", { error: String(err) }));
                return false;
              }
            };
            // `required`: the binding is already gone from the saved entry,
            // so closing the prompt must still take Codex off it (only the
            // session follow is optional) — or Codex stays on a binding that
            // no longer exists and a later Stop cannot suspend it.
            await codexFollowForBinding("toOfficial", t("providers.official"), doDeactivate, {
              required: true
            });
            continue;
          }
          if (isMultiSlot(pb.app)) {
            // Multi-slot: remove just THIS binding's slot/entries (leaves
            // sibling slots). deactivate would only clear the default.
            await invoke("delete_provider", { provider: oldSynth });
          } else {
            await invoke("deactivate_provider", {
              app: pb.app,
              providersForApp: [oldSynth]
            });
          }
          markActive(pb.app, null); // binding gone → drop its stale marker
        } else {
          // Re-applied only when what the CLI gets actually changed — the
          // binding OR the gateway's own fields (Base URL, key, …): compare
          // the synthesized providers, not the bindings. A one-click toggle
          // of another tool must not rewrite every live config.
          const newSynth = providerFromBinding(next, stillBound);
          if (JSON.stringify(newSynth) === JSON.stringify(oldSynth)) continue;
          // Multi-slot: "active" only means the slot is enabled; it is made
          // the CLI's default again only if it WAS the default.
          const wasDefault = activeStates[pb.app]?.matchedProviderId === pb.id;
          await invoke("activate_provider", {
            provider: newSynth,
            providersForApp: [newSynth]
          });
          if (isMultiSlot(pb.app) && wasDefault) {
            await invoke("set_default_provider", {
              provider: newSynth,
              providersForApp: synthProviders.filter((s) => s.app === pb.app)
            });
          }
        }
      }
      await refreshActive();
    } catch (err) {
      toast.error(t("toast.savedButFailed", { error: String(err) }));
    }
  };

  return {
    activeStates,
    busy,
    reconcileAfterEdit,
    synthProviders,
    refreshActive,
    isBindingActive,
    activateBinding,
    deactivateBinding,
    toggleBindingEnabled,
    clearBindingDefault
  };
}
