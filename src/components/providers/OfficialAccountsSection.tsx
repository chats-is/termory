import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask } from "@tauri-apps/plugin-dialog";
import {
  Circle,
  CircleCheck,
  Loader2,
  RefreshCw,
  SavePlus,
  Trash2
} from "lucide-react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { QUOTA_CHANGED_EVENT } from "@/constants";
import { formatResetTime } from "@/lib/format";
import { quotaLevel, type QuotaLevel } from "@/lib/quota-utils";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type {
  AccountsState,
  CliApp,
  CurrentAccount,
  SavedAccount,
  SubscriptionQuota
} from "@/types";

// ─── Quota ring components (moved from ProviderOfficialCard) ─────────────────

const TIER_LABELS: Record<string, { short: MessageKey; full: MessageKey }> = {
  five_hour: {
    short: "providers.quotaFiveHourShort",
    full: "providers.quotaFiveHour"
  },
  seven_day: {
    short: "providers.quotaSevenDayShort",
    full: "providers.quotaSevenDay"
  },
  seven_day_opus: {
    short: "providers.quotaSevenDayOpusShort",
    full: "providers.quotaSevenDayOpus"
  },
  seven_day_sonnet: {
    short: "providers.quotaSevenDaySonnetShort",
    full: "providers.quotaSevenDaySonnet"
  },
  "30_day": {
    short: "providers.quotaMonthlyShort",
    full: "providers.quotaMonthly"
  },
  gemini_pro: {
    short: "providers.quotaGeminiProShort",
    full: "providers.quotaGeminiPro"
  },
  gemini_flash: {
    short: "providers.quotaGeminiFlashShort",
    full: "providers.quotaGeminiFlash"
  },
  gemini_flash_lite: {
    short: "providers.quotaGeminiFlashLiteShort",
    full: "providers.quotaGeminiFlashLite"
  }
};

type Translate = (
  key: MessageKey,
  params?: Record<string, string | number>
) => string;

function formatReset(iso: string, t: Translate, withZone: boolean): string | null {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return null;
  return t("providers.quotaResets", {
    time: formatResetTime(date, new Date(), withZone)
  });
}

const NOT_USED_TIERS = new Set(["seven_day_opus", "seven_day_sonnet"]);
const RING_CLASS: Record<QuotaLevel, string> = {
  ok: "stroke-primary",
  warn: "stroke-amber-500",
  crit: "stroke-destructive"
};

function tierLabels(name: string, t: Translate): { short: string; full: string } {
  const known = TIER_LABELS[name];
  if (known) return { short: t(known.short), full: t(known.full) };
  const hours = /^(\d+)_hour$/.exec(name);
  if (hours) {
    return { short: `${hours[1]}h`, full: t("providers.quotaHourWindow", { n: hours[1] }) };
  }
  const days = /^(\d+)_day$/.exec(name);
  if (days) {
    return { short: `${days[1]}d`, full: t("providers.quotaDayWindow", { n: days[1] }) };
  }
  return { short: name, full: name };
}

function QuotaRing({ utilization }: { utilization: number }) {
  const pct = Math.min(100, Math.max(0, utilization));
  const r = 16;
  const c = 2 * Math.PI * r;
  return (
    <span className="relative inline-flex size-[32px] shrink-0">
      <svg viewBox="0 0 36 36" className="size-[32px] -rotate-90" aria-hidden>
        <circle
          cx="18" cy="18" r={r} fill="none"
          strokeWidth="3" className="stroke-muted-foreground/25"
        />
        {pct > 0 && (
          <circle
            cx="18" cy="18" r={r} fill="none"
            strokeWidth="3" strokeLinecap="round"
            strokeDasharray={`${(pct / 100) * c} ${c}`}
            className={RING_CLASS[quotaLevel(pct)]}
          />
        )}
      </svg>
      <span className="absolute inset-0 flex items-center justify-center text-[8px] leading-none font-mono tabular-nums">
        {Math.round(utilization)}%
      </span>
    </span>
  );
}

function QuotaTierItem({
  name,
  utilization,
  resetsAt
}: {
  name: string;
  utilization: number;
  resetsAt?: string;
}) {
  const t = useT();
  const labels = tierLabels(name, t);
  // Visible subline stays compact (no timezone); the hover tooltip
  // carries the full form with the IANA zone name.
  const notUsed =
    utilization <= 0 && NOT_USED_TIERS.has(name)
      ? t("providers.quotaNotUsedYet", { model: labels.short })
      : null;
  const subline = notUsed ?? (resetsAt ? formatReset(resetsAt, t, false) : null);
  const hoverReset = notUsed ?? (resetsAt ? formatReset(resetsAt, t, true) : null);
  const detail = [labels.full, `${Math.round(utilization)}%`, hoverReset]
    .filter(Boolean)
    .join(" · ");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex items-center gap-1.5 text-xs leading-none">
          <QuotaRing utilization={utilization} />
          <span className="flex flex-col gap-1 min-w-0">
            <span className="max-w-32 truncate text-foreground">{labels.short}</span>
            {subline && (
              <span className="whitespace-nowrap text-[10px] text-muted-foreground/70">
                {subline}
              </span>
            )}
          </span>
        </span>
      </TooltipTrigger>
      <TooltipContent>{detail}</TooltipContent>
    </Tooltip>
  );
}

// ─── Main component ───────────────────────────────────────────────────────────

/** Official-account section rendered directly under the Official card.
 *
 * Codex  — full multi-account management (save / switch / delete).
 * Claude / Gemini — display-only: shows the live account name + email.
 * All    — quota rings rendered in the active account row.
 */
export function OfficialAccountsSection({
  app,
  onSwitched,
  quota,
  quotaLoading = false,
  quotaCooldown = false,
  onRefreshQuota,
  externalTrigger,
  loginInProgress = false,
  activeReloginId = null,
  onRelogin
}: {
  app: CliApp;
  onSwitched?: () => void;
  quota?: SubscriptionQuota | null;
  quotaLoading?: boolean;
  quotaCooldown?: boolean;
  onRefreshQuota?: () => void;
  /** Increment to trigger an account list reload (e.g. after Add Account). */
  externalTrigger?: number;
  /** True while any codex login is in progress (Add Account or Re-login). */
  loginInProgress?: boolean;
  /** ID of the saved account currently being re-logged in (for per-row spinner). */
  activeReloginId?: string | null;
  /** Called when the user clicks Re-login on a row. ProvidersPage handles the flow. */
  onRelogin?: (id: string) => void;
}) {
  const t = useT();
  const [state, setState] = React.useState<AccountsState | null>(null);
  const [busy, setBusy] = React.useState<string | null>(null);

  const reload = React.useCallback(async () => {
    try {
      setState(await invoke<AccountsState>("list_accounts", { app }));
    } catch (err) {
      toast.error(String(err));
    }
  }, [app]);

  React.useEffect(() => {
    void reload();
  }, [reload]);

  // Reload when the parent signals that a new account was added externally.
  React.useEffect(() => {
    if ((externalTrigger ?? 0) > 0) void reload();
  }, [externalTrigger, reload]);

  React.useEffect(() => {
    const cleanups: Array<() => void> = [];
    let live = true;
    const track = (p: Promise<() => void>) =>
      void p.then((un) => (live ? cleanups.push(un) : un()));
    track(
      listen<{ app?: string }>(QUOTA_CHANGED_EVENT, (e) => {
        if (e.payload?.app === app) void reload();
      })
    );
    track(
      getCurrentWindow().onFocusChanged(({ payload: focused }) => {
        if (focused) void reload();
      })
    );
    return () => {
      live = false;
      cleanups.forEach((fn) => fn());
    };
  }, [app, reload]);

  const current: CurrentAccount | null = state?.current ?? null;
  const accounts = state?.accounts ?? [];

  const isManaged = app === "codex";

  // Nothing to show until state loads; for display-only apps also bail out
  // when there is no current account.
  if (!state) return null;
  if (!isManaged && !current) return null;

  // ── handlers (Codex-only) ──────────────────────────────────────────────────

  const saveCurrent = async () => {
    try {
      await invoke("save_account", { app });
      toast.success(t("toast.accountSaved"));
      await reload();
    } catch (err) {
      toast.error(String(err));
    }
  };

  const switchTo = async (account: SavedAccount) => {
    if (account.active) return;
    const warn =
      current && !current.saved
        ? `\n\n${t("providers.accountSwitchWarnUnsaved")}`
        : "";
    const ok = await ask(
      `${t("providers.accountSwitchConfirm", { name: account.name })}${warn}`,
      { title: t("providers.accountSwitchTitle"), kind: "warning" }
    );
    if (!ok) return;
    setBusy(account.id);
    try {
      await invoke("switch_account", { id: account.id });
      await invoke("mark_account_relogin", { id: account.id, needed: false });
      toast.success(t("toast.accountSwitched", { name: account.name }));
      await reload();
      onSwitched?.();
    } catch (err) {
      await invoke("mark_account_relogin", { id: account.id, needed: true }).catch(() => {});
      toast.warning(t("toast.accountTokenExpired"));
      await reload();
    } finally {
      setBusy(null);
    }
  };

  const remove = async (account: SavedAccount) => {
    const ok = await ask(
      t("providers.accountDeleteConfirm", { name: account.name }),
      { title: t("providers.accountDeleteTitle"), kind: "warning" }
    );
    if (!ok) return;
    setBusy(account.id);
    try {
      await invoke("delete_account", { id: account.id });
      toast.success(t("toast.accountDeleted"));
      await reload();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(null);
    }
  };

  // ── row model ──────────────────────────────────────────────────────────────

  type Row = {
    key: string;
    name: string;
    email?: string | null;
    plan?: string | null;
    active: boolean;
    needsRelogin?: boolean;
    account: SavedAccount | null;
    isCurrentUnsaved: boolean;
  };

  const rows: Row[] = [];
  if (isManaged) {
    if (current && !current.saved) {
      rows.push({
        key: "__current__",
        name: current.name ?? current.email ?? "Codex",
        email: current.email,
        plan: current.plan,
        active: true,
        isCurrentUnsaved: true,
        account: null
      });
    }
    for (const acc of accounts) {
      rows.push({
        key: acc.id,
        name: acc.name,
        email: acc.email,
        plan: acc.plan,
        active: acc.active,
        needsRelogin: acc.needsRelogin,
        isCurrentUnsaved: false,
        account: acc
      });
    }
  } else if (current) {
    // Display-only: one row for the current account
    rows.push({
      key: "__current__",
      name: current.name ?? current.email ?? app,
      email: current.email,
      plan: current.plan ?? quota?.plan ?? null,
      active: true,
      isCurrentUnsaved: false,
      account: null
    });
  }

  const showQuota =
    !!onRefreshQuota && quota?.credentialStatus !== "not_found";

  return (
    <div className="-mt-2 flex flex-col rounded-b-xl bg-card pt-2 shadow-sm">
      {state.storageWarning && (
        <p className="px-3 py-2 text-[11px] leading-relaxed text-amber-600 dark:text-amber-400">
          {t("providers.accountKeyringWarning", { mode: state.storageWarning })}
        </p>
      )}

      {rows.length === 0 && !!current && (
        <p className="px-3 py-2 text-xs text-muted-foreground">
          {t("providers.accountsEmpty")}
        </p>
      )}

      {rows.map((row) => {
        const acc = row.account;
        const rowBusy = !!acc && busy === acc.id;
        const isActiveRow = row.active;

        return (
          <div
            key={row.key}
            className="flex items-center gap-3 border-t border-border/50 px-3 py-2 hover:bg-accent/40 first:border-t-0 last:rounded-b-xl"
          >
            {/* Switch button — managed only */}
            {isManaged && (
              <button
                type="button"
                disabled={row.active || row.needsRelogin || rowBusy}
                onClick={acc ? () => void switchTo(acc) : undefined}
                aria-label={
                  row.active
                    ? t("providers.accountActive")
                    : t("providers.accountSwitch")
                }
                className={`group/sw flex size-10 shrink-0 items-center justify-center text-primary${(row.needsRelogin || rowBusy) ? " opacity-30 cursor-not-allowed" : ""}`}
              >
                {rowBusy ? (
                  <Loader2 className="size-6 animate-spin" />
                ) : row.active ? (
                  <CircleCheck className="size-6" />
                ) : (
                  <Circle className="size-6 text-muted-foreground/40 transition-colors group-hover/sw:enabled:text-primary" />
                )}
              </button>
            )}

            {/* Display-only active indicator */}
            {!isManaged && (
              <span className="flex size-10 shrink-0 items-center justify-center text-primary">
                <CircleCheck className="size-6" />
              </span>
            )}

            {/* Account info */}
            <div className="min-w-0 flex-1 flex flex-col gap-0.5">
              <div className="flex items-center gap-2">
                <span className="shrink-0 max-w-[60%] truncate text-sm">
                  {row.name}
                </span>
                {row.plan && (
                  <Badge className="shrink-0 uppercase text-[9px] tracking-wide px-1.5 py-0 bg-primary/15 text-primary">
                    {row.plan}
                  </Badge>
                )}
                {row.needsRelogin && (
                  <Badge className="shrink-0 text-[10px] px-1.5 py-0 bg-destructive/15 text-destructive border-0">
                    {t("providers.accountNeedsRelogin")}
                  </Badge>
                )}
              </div>
              {row.email && (
                <span className="truncate text-[11px] text-muted-foreground">
                  {row.email}
                </span>
              )}
            </div>

            {/* Quota rings — active row only */}
            {isActiveRow && showQuota && (
              <div className="shrink-0 flex items-center gap-3 ml-1">
                {/* Gate on tiers, not success: a merged transient
                    failure keeps the last good tiers visible. */}
                {(quota?.tiers.length ?? 0) > 0 &&
                  quota!.tiers.map((tier) => (
                    <QuotaTierItem
                      key={tier.name}
                      name={tier.name}
                      utilization={tier.utilization}
                      resetsAt={tier.resetsAt}
                    />
                  ))}
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="shrink-0 -ml-2 inline-flex">
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-sm"
                        onClick={onRefreshQuota}
                        disabled={quotaLoading || quotaCooldown}
                        aria-label={t("providers.quotaRefresh")}
                      >
                        <RefreshCw
                          className={cn("size-4", quotaLoading && "animate-spin")}
                        />
                      </Button>
                    </span>
                  </TooltipTrigger>
                  <TooltipContent>
                    {quotaCooldown && !quotaLoading
                      ? t("providers.quotaCooldownHint")
                      : t("providers.quotaRefresh")}
                  </TooltipContent>
                </Tooltip>
              </div>
            )}

            {/* Actions — managed Codex only */}
            {isManaged && (
              <>
                {row.needsRelogin && (
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    disabled={loginInProgress || rowBusy}
                    onClick={() => onRelogin?.(row.key)}
                    className="shrink-0 gap-1.5 text-destructive border-destructive/40 hover:bg-destructive/10"
                  >
                    {activeReloginId === row.key ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : null}
                    {t("providers.accountRelogin")}
                  </Button>
                )}
                {row.isCurrentUnsaved && (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <span className="inline-flex">
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => void saveCurrent()}
                          aria-label={t("providers.accountSaveCurrent")}
                        >
                          <SavePlus className="size-4" />
                        </Button>
                      </span>
                    </TooltipTrigger>
                    <TooltipContent>
                      {t("providers.accountSaveCurrentHint")}
                    </TooltipContent>
                  </Tooltip>
                )}
                {acc && (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-sm"
                        className="text-destructive hover:bg-destructive/10"
                        disabled={rowBusy}
                        onClick={() => void remove(acc)}
                        aria-label={t("providers.accountDelete")}
                      >
                        <Trash2 className="size-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>{t("providers.accountDelete")}</TooltipContent>
                  </Tooltip>
                )}
              </>
            )}
          </div>
        );
      })}
    </div>
  );
}
