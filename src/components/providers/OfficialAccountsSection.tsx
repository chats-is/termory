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
import { CLI_APP_LABEL, QUOTA_CHANGED_EVENT } from "@/constants";
import { formatCountdown, formatCurrency, formatResetTime } from "@/lib/format";
import { quotaLevel, type QuotaLevel } from "@/lib/quota-utils";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type {
  AccountsState,
  CliApp,
  CurrentAccount,
  ExtraUsage,
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

// "in 4 hr 42 min" / "in 6 days" — the live countdown appended after the
// absolute reset time. Shown on ONE window only (see waitingOnTierName): the
// one you're actually waiting on. null once the boundary is past (stale data)
// so the caller keeps just the absolute form.
function formatResetCountdown(iso: string, t: Translate): string | null {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return null;
  if (date.getTime() <= Date.now()) return null;
  const dur = formatCountdown(date, new Date(), t);
  return dur ? t("providers.quotaResetsCountdown", { duration: dur }) : null;
}

// A window counts as spent once utilization reaches 100 — the API reports a
// 0-100 percent, and at 100 that window grants nothing until it resets.
const SPENT_PCT = 100;

// ACCOUNT-WIDE windows: spending one stops all work until it resets. Everything
// else is MODEL-SCOPED (Claude's `weekly_scoped` brand-name weeklies — "Fable",
// "Opus" — the legacy `seven_day_opus`/`seven_day_sonnet`, and Gemini's
// per-model buckets), where spending one only rules out THAT model.
//
// Listed from this side on purpose: model names are dynamic (any model Anthropic
// ships surfaces as a tier under its brand name, with no per-model code), so
// they can't be enumerated — while the account-wide ids are a fixed set plus the
// generated `{n}_hour` / `{n}_day` forms `tierLabels` already humanizes. A new
// model therefore lands on the model-scoped side by default, which is correct.
//
// A new ACCOUNT-WIDE window does need adding here (Anthropic spells its ids in
// words, so expect the `thirty_day` shape rather than `30_day`). That upkeep is
// the intended workflow — `tierLabels` needs the same edit to give it a name.
const ACCOUNT_WIDE_TIERS = new Set(["five_hour", "seven_day", "30_day"]);
const GENERATED_WINDOW = /^\d+_(hour|day)$/;

// A `group` means the API itself said this window is scoped to one model
// (it's the period the model's window groups under), so it settles the
// question with no list to maintain. The id list stays for the windows
// that carry no group: the account-wide ones, the legacy flat
// `seven_day_opus`/`seven_day_sonnet`, and Gemini's per-model buckets.
function isAccountWide(name: string, group?: string): boolean {
  if (group) return false;
  return ACCOUNT_WIDE_TIERS.has(name) || GENERATED_WINDOW.test(name);
}

// The ONE window that shows the live countdown: the one you're actually
// WAITING ON. Every other window keeps just its absolute reset time.
//
// Two cases, because "how long until I can work again" has two different
// answers depending on whether anything is spent (user decision 2026-07-25):
//
//   * Nothing blocking → the SOONEST-resetting window. You aren't blocked, so
//     the meaningful number is when capacity next tops up — a 5h window
//     resetting at 2pm, not a weekly five days out that merely sits at a higher
//     percent.
//   * Something blocking → the LATEST-resetting BLOCKING window. Every one of
//     them has to clear before you can work again, so the last is when you're
//     actually free. A spent weekly keeps blocking through several 5h resets,
//     and the 5h countdown would promise capacity that isn't coming.
//
// "Blocking" is not the same as "spent": a spent MODEL-SCOPED window (see
// isAccountWide) leaves every other model usable, so it blocks nothing on its
// own — counting it would answer "wait 5 days" to someone who can switch model
// and work right now.
//
// So whenever account-wide windows exist, THEY decide, and model-scoped ones
// never do. Model-scoped windows take over only when there are no account-wide
// windows at all AND every one of them is spent — the Gemini shape, whose
// per-model buckets are the whole picture, so all-spent really does mean no
// model is left. (A "every model-scoped window is spent" rule without the
// account-wide check is WRONG for Claude: its scoped weeklies cover only some
// models — Opus, Fable — so Sonnet stays usable with no window of its own.)
//
// Both earlier rules got this wrong in one direction each: "highest
// utilization" put a 5-day weekly countdown above a 5h window resetting the
// same afternoon, and plain "soonest reset" pointed at a 5h window while a
// spent weekly was the real wait.
//
// Returns null until the account has been used at all (any tier above 0) —
// nothing to wait for on a completely fresh account. That check spans all
// tiers, since the usage may sit on a different tier than the chosen one.
// Windows without a usable reset time can't carry a countdown and are skipped.
export function waitingOnTierName(
  tiers: {
    name: string;
    group?: string;
    utilization: number;
    resetsAt?: string | null;
  }[]
): string | null {
  if (!tiers.some((tier) => tier.utilization > 0)) return null;
  const usable = tiers.flatMap((tier) => {
    if (!tier.resetsAt) return [];
    const at = new Date(tier.resetsAt).getTime();
    return Number.isNaN(at)
      ? []
      : [{ name: tier.name, group: tier.group, at, pct: tier.utilization }];
  });
  if (usable.length === 0) return null;
  const isSpent = (tier: { pct: number }) => tier.pct >= SPENT_PCT;
  const accountWide = usable.filter((t) => isAccountWide(t.name, t.group));
  const modelScoped = usable.filter((t) => !isAccountWide(t.name, t.group));
  const spentAccountWide = accountWide.filter(isSpent);
  const blocking =
    spentAccountWide.length > 0
      ? spentAccountWide
      : accountWide.length === 0 && modelScoped.every(isSpent)
        ? modelScoped
        : [];
  // Strict comparisons keep the FIRST window on a tie, in both branches.
  return blocking.length > 0
    ? blocking.reduce((best, tier) => (tier.at > best.at ? tier : best)).name
    : usable.reduce((best, tier) => (tier.at < best.at ? tier : best)).name;
}

const NOT_USED_TIERS = new Set(["seven_day_opus", "seven_day_sonnet"]);
const RING_CLASS: Record<QuotaLevel, string> = {
  ok: "stroke-primary",
  warn: "stroke-amber-500",
  crit: "stroke-destructive"
};

// The API's period groups, mapped to the SAME labels their account-wide
// counterparts use so "Weekly · Fable" reads consistently next to a plain
// "Weekly". Unrecognized groups render verbatim — a new period surfaces
// without a release, exactly like an unknown window id.
// One label per period (no short/full split): the composed
// "Weekly · Fable" is already the whole story, unlike an account-wide
// window whose full form adds scope ("Weekly (all models)").
const GROUP_LABELS: Record<string, MessageKey> = {
  session: "providers.quotaFiveHourShort",
  weekly: "providers.quotaSevenDayShort",
  monthly: "providers.quotaMonthlyShort"
};

// `group` is set only for MODEL-SCOPED windows, whose `name` is a bare
// model name ("Fable") that alone wouldn't say which window it is sitting
// next to "5h". Compose `{period} · {model}` — both halves come from the
// API, so any model/period pair works with no per-model code (which
// models get their own window is dynamic and differs per account).
function tierLabels(
  name: string,
  t: Translate,
  group?: string
): { short: string; full: string } {
  if (group) {
    const period = GROUP_LABELS[group];
    const composed = `${period ? t(period) : group} · ${name}`;
    return { short: composed, full: composed };
  }
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
  app,
  name,
  group,
  utilization,
  resetsAt,
  showCountdown,
  stale
}: {
  app: CliApp;
  name: string;
  group?: string;
  utilization: number;
  resetsAt?: string;
  // Only the window you're waiting on (see waitingOnTierName) gets the
  // countdown; the rest show just the absolute reset time.
  showCountdown: boolean;
  // The last fetch failed, so these numbers are the previous successful
  // result (mergeQuotaResult keeps them rather than blanking the card).
  stale?: boolean;
}) {
  const t = useT();
  let labels = tierLabels(name, t, group);
  // Grok reuses the seven_day/30_day window ids (so the tray "W"/"M"
  // abbreviations and ring shorts apply), but the Claude-specific FULL
  // labels ("Weekly (all models)") misdescribe grok's CREDIT window —
  // the tooltip uses the plain short form there.
  if (app === "grok") {
    labels = { short: labels.short, full: labels.short };
  }
  // Visible subline stays compact (no timezone); the hover tooltip
  // carries the full form with the IANA zone name.
  const notUsed =
    utilization <= 0 && NOT_USED_TIERS.has(name)
      ? t("providers.quotaNotUsedYet", { model: labels.short })
      : null;
  // Absolute reset time (existing) with the live countdown appended:
  // "Resets 11:09pm · in 4 hr 42 min".
  const countdown =
    showCountdown && resetsAt ? formatResetCountdown(resetsAt, t) : null;
  const withCountdown = (base: string | null): string | null =>
    base ? (countdown ? `${base} · ${countdown}` : base) : null;
  const subline = notUsed ?? withCountdown(resetsAt ? formatReset(resetsAt, t, false) : null);
  const hoverReset = notUsed ?? withCountdown(resetsAt ? formatReset(resetsAt, t, true) : null);
  const detail = [labels.full, `${Math.round(utilization)}%`, hoverReset]
    .filter(Boolean)
    .join(" · ");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex items-center gap-1.5 text-xs leading-none">
          <QuotaRing utilization={utilization} />
          <span className="flex flex-col gap-1 min-w-0">
            {/* Red NAME, not a red ring: the ring's colors are the
                pressure scale (amber ≥75%, red ≥90%), so tinting it
                would read as "this window is nearly spent". The label
                is the one part of the item that carries no other
                meaning, so it can say "these numbers are stale". */}
            <span
              className={cn(
                "max-w-32 truncate",
                stale ? "text-destructive" : "text-foreground"
              )}
            >
              {labels.short}
            </span>
            {subline && (
              <span className="whitespace-nowrap text-[10px] text-muted-foreground/70">
                {subline}
              </span>
            )}
          </span>
        </span>
      </TooltipTrigger>
      <TooltipContent>
        {detail}
        {/* A bare red label says something is wrong but not what — the
            hover carries the reason. */}
        {stale && (
          <>
            <br />
            {t("providers.quotaStale")}
          </>
        )}
      </TooltipContent>
    </Tooltip>
  );
}

/** "Usage credits" — Claude's pay-as-you-go overflow (and grok on-demand
 * credits). Rendered next to the quota rings when enabled: a pressure ring
 * on the used-credit utilization + the currency amounts as the subline. */
function ExtraUsageItem({ extra, stale }: { extra: ExtraUsage; stale?: boolean }) {
  const t = useT();
  const used = formatCurrency(
    extra.usedCredits ?? 0,
    extra.currency,
    extra.decimalPlaces
  );
  const limit =
    extra.monthlyLimit != null
      ? formatCurrency(extra.monthlyLimit, extra.currency, extra.decimalPlaces)
      : null;
  const subline = limit ? `${used} / ${limit}` : used;
  const detail = limit
    ? t("providers.usageCreditsHint", { used, limit })
    : t("providers.usageCredits");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex items-center gap-1.5 text-xs leading-none">
          <QuotaRing utilization={extra.utilization ?? 0} />
          <span className="flex flex-col gap-1 min-w-0">
            <span
              className={cn(
                "max-w-32 truncate",
                stale ? "text-destructive" : "text-foreground"
              )}
            >
              {t("providers.usageCredits")}
            </span>
            <span className="whitespace-nowrap text-[10px] text-muted-foreground/70">
              {subline}
            </span>
          </span>
        </span>
      </TooltipTrigger>
      <TooltipContent>
        {detail}
        {stale && (
          <>
            <br />
            {t("providers.quotaStale")}
          </>
        )}
      </TooltipContent>
    </Tooltip>
  );
}

/** Prepaid ("bought") credit balance — grok's unified-billing model.
 *
 * Deliberately has NO ring, unlike every sibling item: a ring encodes a
 * used/limit ratio and a balance has no limit to divide by, so any arc
 * would be invented. Also NO low-balance colouring: grok's own warning
 * (credit_bar.rs:219) fires only once the included allowance is fully
 * spent AND is gated on the auto-top-up rule — a separate endpoint we
 * don't call — so a bare "$10 is low" tint would cry wolf at users whose
 * balance tops itself up. Just the amount. */
function PrepaidBalanceItem({ amount, stale }: { amount: number; stale?: boolean }) {
  const t = useT();
  // grok stores these as USD (billing.rs `Cent`), already in major units.
  const value = formatCurrency(amount, "USD");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex flex-col gap-1 min-w-0 text-xs leading-none">
          <span
            className={cn(
              "max-w-32 truncate",
              stale ? "text-destructive" : "text-foreground"
            )}
          >
            {t("providers.creditBalance")}
          </span>
          <span className="whitespace-nowrap text-[10px] text-muted-foreground/70">
            {value}
          </span>
        </span>
      </TooltipTrigger>
      <TooltipContent>
        {t("providers.creditBalanceHint", { amount: value })}
        {stale && (
          <>
            <br />
            {t("providers.quotaStale")}
          </>
        )}
      </TooltipContent>
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
  onRelogin,
  reloginUnavailable = false
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
  /** True when re-login can't run (Codex desktop-app-only install —
   * no `codex` binary to spawn). Disables the Re-login buttons. */
  reloginUnavailable?: boolean;
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

  // Full snapshot management (save / switch / delete). Codex + Claude + Grok;
  // Gemini stays display-only. The "Add account" / re-login buttons are gated
  // separately by the page (their props are only passed for CLIs whose login
  // Termory can spawn), so this flag governs the rows, not those buttons.
  const isManaged = app === "codex" || app === "claude" || app === "grok";

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
    // A RUNNING claude holds ~/.claude.json in memory and writes the whole
    // document back on any config change — its stale `oauthAccount` would
    // silently undo the identity half of the switch (and later snapshots
    // would then pair the new credentials with the old identity). There is
    // no lock to detect it by (unlike the sqlite CLIs' `map_db_locked`), so
    // the confirm carries the hint instead.
    const quitHint =
      app === "claude" ? `\n\n${t("providers.accountSwitchQuitClaudeHint")}` : "";
    const ok = await ask(
      `${t("providers.accountSwitchConfirm", { name: account.name, cli: CLI_APP_LABEL[app] })}${warn}${quitHint}`,
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
      if (app === "codex") {
        // Codex validates the tokens over the network before writing, so a
        // failure means the snapshot's refresh token is dead → flag it.
        await invoke("mark_account_relogin", { id: account.id, needed: true }).catch(() => {});
        toast.warning(t("toast.accountTokenExpired"));
      } else {
        // Claude validates too, but flags needsRelogin BACKEND-side on
        // AuthFailure only — a string Err here can't tell a dead token from
        // a locked-Keychain write error, and flagging a write error would
        // trap a healthy account. Just surface the message; reload picks up
        // any backend-set flag.
        toast.error(String(err));
      }
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
        name: current.name ?? current.email ?? CLI_APP_LABEL[app],
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

  // The last fetch FAILED while there is still something to show, which
  // only happens because mergeQuotaResult keeps the previous successful
  // result rather than blanking the card (see quota-utils). That retention
  // is deliberate — but silently, so the numbers looked live while they
  // could be arbitrarily old. Mark the labels instead of hiding the data.
  // `not_found` never reaches here (showQuota already hides the section).
  //
  // Deliberately NOT gated on `quotaLoading`: this marks the DATA, not the
  // request. A refresh in flight hasn't replaced the numbers yet, so
  // clearing the mark while it runs would flash red → normal → red and
  // call stale data live for the duration.
  const quotaStale = !!quota && !quota.success;

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
            {/* Switch button — managed only. The Tooltip is added ONLY on a
                switchable (inactive, actionable) button; the active current
                account and disabled rows render a plain button with no tip. */}
            {isManaged &&
              (() => {
                const switchable =
                  !row.active && !row.needsRelogin && !rowBusy && !!acc;
                const btn = (
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
                );
                return switchable ? (
                  <Tooltip>
                    <TooltipTrigger asChild>{btn}</TooltipTrigger>
                    <TooltipContent side="top">
                      {t("providers.accountUse")}
                    </TooltipContent>
                  </Tooltip>
                ) : (
                  btn
                );
              })()}

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
                  (() => {
                    const waitingOn = waitingOnTierName(quota!.tiers);
                    // Resolve the name to ONE row up front: a model-scoped
                    // window's name is a model, so it could in principle
                    // equal an account-wide window id, and comparing by
                    // name per row would then light the countdown twice.
                    // First match wins, so exactly one row carries it.
                    const waitingIndex = quota!.tiers.findIndex(
                      (tier) => tier.name === waitingOn
                    );
                    return quota!.tiers.map((tier, index) => (
                      // Same reason for the composite key: name alone is
                      // not guaranteed unique across the two kinds, and a
                      // duplicate React key is an error in its own right.
                      <QuotaTierItem
                        key={`${tier.group ?? ""}:${tier.name}`}
                        app={app}
                        name={tier.name}
                        group={tier.group}
                        utilization={tier.utilization}
                        resetsAt={tier.resetsAt}
                        showCountdown={index === waitingIndex}
                        stale={quotaStale}
                      />
                    ));
                  })()}
                {quota?.extraUsage?.isEnabled && (
                  <ExtraUsageItem extra={quota.extraUsage} stale={quotaStale} />
                )}
                {quota?.prepaidBalance != null && (
                  <PrepaidBalanceItem
                    amount={quota.prepaidBalance}
                    stale={quotaStale}
                  />
                )}
                <Tooltip>
                  {/* The trigger is the WRAPPER, not the button: a disabled
                      element dispatches no hover events, and this button is
                      disabled exactly when it has something to explain —
                      "refreshed just now, try again in a moment" could never
                      be read while the cooldown that produces it was on. */}
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
                  <TooltipContent side="top">
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
                    disabled={loginInProgress || rowBusy || reloginUnavailable}
                    onClick={() => onRelogin?.(row.key)}
                    className="shrink-0 gap-1.5 text-destructive border-destructive/40 hover:bg-destructive/10"
                  >
                    {activeReloginId === row.key ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : null}
                    {t("providers.accountRelogin")}
                  </Button>
                )}
                {row.isCurrentUnsaved && (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-sm"
                        onClick={() => void saveCurrent()}
                        aria-label={t("providers.accountSaveCurrent")}
                      >
                        <SavePlus className="size-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent side="top">
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
                    <TooltipContent side="top">
                      {t("providers.accountDelete")}
                    </TooltipContent>
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
