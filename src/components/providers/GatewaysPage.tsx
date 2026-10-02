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
import { BalanceInline } from "./BalanceInline";
import { BrandIcon } from "@/components/BrandIcon";
import { EmptyState } from "@/components/EmptyState";
import { CLI_APP_LABEL, CLI_APP_SOURCE_BADGE } from "@/constants";
import { blankGateway, isMultiSlot, providerFromBinding } from "@/lib/provider-utils";
import { useGatewayBindings } from "@/hooks/useGatewayBindings";
import { useBalances } from "@/hooks/useBalances";
import type { CliApp, Gateway } from "@/types";
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
  allGateways,
  setGateways,
  addSignal,
  markActive,
  activeProviderIds,
  installed,
  visibleApps,
  codexFollowForBinding
}: {
  /** The gateways LISTED here (the AI Gateways — the router entry is not). */
  gateways: Gateway[];
  /** EVERY gateway-shaped entry, router included — the binding hook's
   * reverse-derivation and set-default strip set must see the router's
   * bindings too, or making a gateway binding the Grok/OpenCode default
   * leaves the router binding's options live. Defaults to `gateways`. */
  allGateways?: Gateway[];
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
  /** Per-CLI install map — an app is only offered as a gateway-binding
   * target when it's installed (mirrors the Providers tab's install gate). */
  installed: Record<CliApp, boolean>;
  /** Apps listed as binding rows in the editor (Settings → Tools hides
   * disabled ones entirely). */
  visibleApps?: readonly CliApp[];
  /** Run a Codex binding switch through the "follow sessions?" prompt (owned by
   * ProvidersPage, which renders the dialog). `direction` is the bucket the
   * switch lands on: `"toCustom"` (activate, official→custom) or `"toOfficial"`
   * (deactivate, custom→official). */
  codexFollowForBinding: (
    direction: "toCustom" | "toOfficial",
    label: string,
    activate: () => Promise<boolean>,
    opts?: { required?: boolean }
  ) => Promise<void>;
}) {
  const t = useT();
  const [editing, setEditing] = React.useState<Gateway | null>(null);
  const [editingIsNew, setEditingIsNew] = React.useState(false);
  const {
    activeStates,
    busy,
    refreshActive,
    isBindingActive,
    activateBinding,
    deactivateBinding,
    toggleBindingEnabled,
    clearBindingDefault,
    reconcileAfterEdit
  } = useGatewayBindings({
    gateways: allGateways ?? gateways,
    markActive,
    activeProviderIds,
    codexFollowForBinding
  });

  // Wallet balance per GATEWAY, keyed by the gateway's own id. A gateway
  // is one {baseUrl, apiKey}, i.e. exactly one wallet, however many CLIs
  // it is bound to — so the reading belongs on the gateway card, not
  // repeated on each binding row. `useBalances` takes the narrow
  // {id, baseUrl, apiKey} subject, which a Gateway already satisfies; no
  // stand-in Provider is synthesized for it.
  const { balances, balanceLoading, balanceInCooldown, refreshBalance } =
    useBalances(gateways);

  // Settings → Tools: binding rows LISTED per gateway card (a disabled
  // tool's binding is hidden; the binding itself survives in
  // providers.json and delete-cleanup still iterates the full list).
  // Computed once here so the empty-state check and the row render
  // can't drift.
  const visibleBindingsByGateway = React.useMemo(
    () =>
      new Map(
        gateways.map((g) => [
          g.id,
          g.bindings
            .filter((b) => !visibleApps || visibleApps.includes(b.app))
            // Rows in the Settings → Tools order, not providers.json order.
            .sort((x, y) =>
              visibleApps ? visibleApps.indexOf(x.app) - visibleApps.indexOf(y.app) : 0
            )
        ])
      ),
    [gateways, visibleApps]
  );

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

    if (!prev) return; // a brand-new gateway has no active bindings
    await reconcileAfterEdit(prev, next);
  };

  const deleteGateway = async (gateway: Gateway) => {
    const confirmed = await ask(
      t("providers.deleteGatewayMsg", { name: gateway.name || t("providers.thisGateway") }),
      { title: t("providers.deleteGateway"), kind: "warning", okLabel: t("providers.delete"), cancelLabel: t("providers.cancel") }
    );
    if (!confirmed) return;
    // Clear any live config this AI Gateway's bindings injected — but only
    // the ones THIS gateway actually activated (marker), so we don't wipe a
    // standalone provider that happens to share the endpoint. If a cleanup
    // fails (CLI running → DB locked, etc.) surface it and KEEP the gateway —
    // mirrors `ProvidersPage.deleteProvider`, so the user can quit the CLI and
    // retry (already-cleaned bindings skip on the retry, so it's idempotent).
    let failed = false;
    for (const b of gateway.bindings) {
      const synth = providerFromBinding(gateway, b);
      try {
        if (isMultiSlot(b.app)) {
          await invoke("delete_provider", { provider: synth });
        } else if (isBindingActive(gateway, b)) {
          await invoke("deactivate_provider", {
            app: b.app,
            providersForApp: [synth]
          });
        }
        if (activeProviderIds[b.app] === b.id) markActive(b.app, null);
      } catch (err) {
        failed = true;
        toast.error(t("toast.clearFailed", { app: CLI_APP_LABEL[b.app], error: String(err) }));
      }
    }
    await refreshActive();
    // Keep the gateway when any binding's live config couldn't be cleared, so
    // it isn't silently lost while a CLI still points at its endpoint.
    if (failed) return;
    setGateways((cur) => cur.filter((r) => r.id !== gateway.id));
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
                    {/* The gateway's own wallet. Inside the action cluster
                        rather than under the base URL: it is live account
                        state, not a stored setting, and the cluster is the
                        row's `items-center` column — the name block beside
                        it is two lines of text with a different baseline.
                        Self-hiding, so a relay gateway looks as before. */}
                    <BalanceInline
                      balance={balances[gateway.id]}
                      loading={balanceLoading.has(gateway.id)}
                      cooldown={balanceInCooldown(gateway.id)}
                      onRefresh={() => void refreshBalance(gateway, true)}
                    />
                    <Tooltip>
                      <TooltipTrigger asChild>
                        {/* `icon-sm` (32px), NOT the 28px override this pair
                            used to carry: it is the size every other icon
                            button in the app uses — the binding rows below,
                            every ProviderCard action, the quota and balance
                            refreshes — and the balance item now sits in this
                            same cluster, so a 28px neighbour left it 4px
                            taller than the icons beside it. */}
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          aria-label={t("providers.editGateway")}
                          onClick={() => startEdit(gateway)}
                        >
                          <Pencil className="size-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">
                        {t("providers.editGateway")}
                      </TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          className="text-destructive hover:text-destructive hover:bg-destructive/10"
                          aria-label={t("providers.deleteGateway")}
                          onClick={() => void deleteGateway(gateway)}
                        >
                          <Trash2 className="size-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">
                        {t("providers.deleteGateway")}
                      </TooltipContent>
                    </Tooltip>
                  </div>
                </div>

                {/* Bindings (visibleBindingsByGateway — Tools-filtered). */}
                {(visibleBindingsByGateway.get(gateway.id) ?? []).length === 0 ? (
                  <p className="text-xs text-muted-foreground">
                    {t("providers.noBindings")}
                  </p>
                ) : (
                  <div className="flex flex-col gap-1.5 pt-1">
                    {(visibleBindingsByGateway.get(gateway.id) ?? [])
                      .map((b) => {
                      const state = activeStates[b.app];
                      const isMulti = isMultiSlot(b.app);
                      // Multi-slot (OpenCode/Grok) has two independent states:
                      // the slot is configured AND it's the startup default
                      // (in use). Single-slot CLIs collapse to one.
                      const configured =
                        isMulti &&
                        (state?.configuredProviderIds ?? []).includes(b.id);
                      const isDefault = isMulti
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
                          {isMulti ? (
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
                                        ? t("providers.disable")
                                        : t("providers.enable")
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
                                <TooltipContent side="top">
                                  {configured
                                    ? t("providers.disable")
                                    : t("providers.enable")}
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
            installed={installed}
            visibleApps={visibleApps}
            onSave={saveGateway}
            onClose={closeEditor}
          />
        </React.Suspense>
      )}
    </div>
  );
}
