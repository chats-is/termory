import { RefreshCw } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { BrandIcon } from "@/components/BrandIcon";
import { CLI_APP_SOURCE_BADGE } from "@/constants";
import { formatWeekdayTime } from "@/lib/format";
import { quotaLevel, type QuotaLevel } from "@/lib/quota-utils";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type { CliApp, SubscriptionQuota } from "@/types";

/** Known rate-limit window ids → short inline label + full tooltip
 * label. Generated ids from non-standard window lengths (Codex maps
 * them to `{n}_hour` / `{n}_day` — quota.rs window_seconds_to_tier_name)
 * are humanized by `tierLabels`; anything else falls back to the raw
 * id so a brand-new window type still surfaces. */
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
  // Codex free plan's 30-day window (generated id, promoted to a
  // proper label — "30d" read poorly).
  "30_day": {
    short: "providers.quotaMonthlyShort",
    full: "providers.quotaMonthly"
  }
};

type Translate = (
  key: MessageKey,
  params?: Record<string, string | number>
) => string;

/** Reset-time copy, matching Claude's own /usage display:
 *  - within 24h  → "Resets in 1 hr 5 min" (relative)
 *  - further out → "Resets Fri 12:00 AM" (weekday + time)
 * Returns null for an unparseable timestamp. */
function formatReset(iso: string, t: Translate): string | null {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return null;
  const diffMs = date.getTime() - Date.now();
  if (diffMs > 0 && diffMs < 24 * 60 * 60 * 1000) {
    const totalMin = Math.round(diffMs / 60_000);
    const h = Math.floor(totalMin / 60);
    const m = totalMin % 60;
    if (h > 0 && m > 0) return t("providers.quotaResetsInHrMin", { h, m });
    if (h > 0) return t("providers.quotaResetsInHr", { h });
    return t("providers.quotaResetsInMin", { m: Math.max(1, m) });
  }
  return t("providers.quotaResets", { time: formatWeekdayTime(date) });
}

/** The model-specific weekly windows show "You haven't used X yet"
 * instead of a reset time while untouched — same as Claude's /usage. */
const NOT_USED_TIERS = new Set(["seven_day_opus", "seven_day_sonnet"]);

/** Display labels for a window id: known ids via TIER_LABELS,
 * generated `{n}_hour` / `{n}_day` ids humanized ("30_day" → short
 * "30d", full "30-day" / "30 天"), anything else raw. */
function tierLabels(
  name: string,
  t: Translate
): { short: string; full: string } {
  const known = TIER_LABELS[name];
  if (known) return { short: t(known.short), full: t(known.full) };
  const hours = /^(\d+)_hour$/.exec(name);
  if (hours) {
    return {
      short: `${hours[1]}h`,
      full: t("providers.quotaHourWindow", { n: hours[1] })
    };
  }
  const days = /^(\d+)_day$/.exec(name);
  if (days) {
    return {
      short: `${days[1]}d`,
      full: t("providers.quotaDayWindow", { n: days[1] })
    };
  }
  return { short: name, full: name };
}

/** Ring color steps with pressure so a nearly-exhausted window reads
 * at a glance (thresholds shared with the tray glyph via quota-utils):
 * primary → amber from 75% → destructive from 90%. */
const RING_CLASS: Record<QuotaLevel, string> = {
  ok: "stroke-primary",
  warn: "stroke-amber-500",
  crit: "stroke-destructive"
};

function ringColor(utilization: number): string {
  return RING_CLASS[quotaLevel(utilization)];
}

/** Donut showing used percentage as an arc, with the rounded percent
 * centered inside the ring. */
function QuotaRing({ utilization }: { utilization: number }) {
  const pct = Math.min(100, Math.max(0, utilization));
  const r = 16;
  const c = 2 * Math.PI * r;
  return (
    <span className="relative inline-flex size-[35px] shrink-0">
      <svg viewBox="0 0 36 36" className="size-[35px] -rotate-90" aria-hidden>
        <circle
          cx="18"
          cy="18"
          r={r}
          fill="none"
          strokeWidth="3"
          className="stroke-muted-foreground/25"
        />
        {pct > 0 && (
          <circle
            cx="18"
            cy="18"
            r={r}
            fill="none"
            strokeWidth="3"
            strokeLinecap="round"
            strokeDasharray={`${(pct / 100) * c} ${c}`}
            className={ringColor(pct)}
          />
        )}
      </svg>
      <span className="absolute inset-0 flex items-center justify-center text-[9px] leading-none font-mono tabular-nums">
        {Math.round(utilization)}%
      </span>
    </span>
  );
}

/** One window as a compact inline group: ring + percent + short
 * label. Full label / exact percentage / reset time ride in the
 * Tooltip to keep the line clean. */
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
  // Second line under the label: "You haven't used X yet" for an
  // untouched model window, else the reset-time copy.
  const subline =
    utilization <= 0 && NOT_USED_TIERS.has(name)
      ? t("providers.quotaNotUsedYet", { model: labels.short })
      : resetsAt
        ? formatReset(resetsAt, t)
        : null;
  const detail = [labels.full, `${Math.round(utilization)}%`, subline]
    .filter(Boolean)
    .join(" · ");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex items-center gap-1.5 text-xs leading-none">
          <QuotaRing utilization={utilization} />
          <span className="flex flex-col gap-1 min-w-0">
            <span className="max-w-32 truncate text-foreground">
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
      <TooltipContent>{detail}</TooltipContent>
    </Tooltip>
  );
}

function QuotaSection({
  quota,
  loading,
  cooldown,
  onRefresh
}: {
  quota: SubscriptionQuota | null;
  loading: boolean;
  cooldown: boolean;
  onRefresh: () => void;
}) {
  const t = useT();

  // Middle of the card row: one line of ring+percent groups plus the
  // refresh button, sitting between the Official/version block and
  // the action button. Failures show NO inline text — background
  // fetches fail silently, and a MANUAL refresh surfaces its error as
  // a toast (ProvidersPage.refreshQuota).
  return (
    <div className="flex-1 min-w-0 flex items-center justify-end gap-4 px-2">
      {quota?.success &&
        quota.tiers.map((tier) => (
          <QuotaTierItem
            key={tier.name}
            name={tier.name}
            utilization={tier.utilization}
            resetsAt={tier.resetsAt}
          />
        ))}
      <Tooltip>
        <TooltipTrigger asChild>
          {/* A disabled shadcn Button has pointer-events: none, so the
              hover that opens the Tooltip lands on this wrapper span. */}
          <span className="shrink-0 -ml-2 inline-flex">
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              onClick={onRefresh}
              disabled={loading || cooldown}
              aria-label={t("providers.quotaRefresh")}
            >
              <RefreshCw className={cn("size-4", loading && "animate-spin")} />
            </Button>
          </span>
        </TooltipTrigger>
        <TooltipContent>
          {cooldown && !loading
            ? t("providers.quotaCooldownHint")
            : t("providers.quotaRefresh")}
        </TooltipContent>
      </Tooltip>
    </div>
  );
}

export function ProviderOfficialCard({
  app,
  isInUse,
  settingDefault,
  version,
  versionLoading = false,
  quota,
  quotaLoading = false,
  quotaCooldown = false,
  onRefreshQuota,
  onSetDefault
}: {
  app: CliApp;
  isInUse: boolean;
  settingDefault: boolean;
  // Installed CLI version parsed from `<bin> --version`. Null AFTER
  // detection ran but `--version` returned nothing parseable.
  version?: string | null;
  // True while the version detection is in flight (covers initial
  // mount and Recheck). When true the card renders a loading
  // placeholder instead of "—".
  versionLoading?: boolean;
  // Official-account rate-limit usage (5-hour / weekly windows). The
  // section renders only when the parent wires these — CLIs without a
  // backend quota implementation simply omit them.
  quota?: SubscriptionQuota | null;
  quotaLoading?: boolean;
  // True while the manual-refresh rate limit is in effect (the parent
  // tracks `queriedAt`); the Refresh button renders disabled.
  quotaCooldown?: boolean;
  onRefreshQuota?: () => void;
  onSetDefault: () => void;
}) {
  const t = useT();
  // No OAuth login at all (`not_found`) is not a transient state —
  // retrying can't succeed, so the whole quota section (refresh
  // button included) disappears. Transient failures (expired token,
  // network error) keep the button so the user can retry; while the
  // FIRST fetch is still in flight (quota == null) it stays too.
  const showQuotaSection =
    !!onRefreshQuota && quota?.credentialStatus !== "not_found";
  return (
    <Card
      className={cn(
        "p-3 gap-0 outline outline-1 outline-transparent shadow-sm",
        isInUse
          ? // Active accent stripe drawn as an overlay (::before) so it adds
            // NO box width — content stays aligned with inactive cards.
            "relative overflow-hidden bg-primary/5 before:content-[''] before:absolute before:inset-y-0 before:left-0 before:w-1 before:bg-primary"
          : "bg-card hover:bg-accent/40 transition-colors"
      )}
    >
      <CardContent className="px-0 flex items-center gap-3 min-h-7">
        <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-background shadow-sm [&_svg]:size-5">
          <BrandIcon source={CLI_APP_SOURCE_BADGE[app]} />
        </span>
        <div
          className={cn(
            "min-w-0 flex flex-col",
            // With a quota line the title block hugs its content and
            // the quota takes the flexible middle; without one it keeps
            // the original full-width layout.
            showQuotaSection ? "shrink-0" : "flex-1"
          )}
        >
          <div className="flex items-center gap-2">
            <h3 className="text-lg font-medium">{t("providers.official")}</h3>
            {isInUse && (
              <Badge className="uppercase text-[9px] tracking-wide px-1.5 py-0">
                {t("providers.inUse")}
              </Badge>
            )}
          </div>
          <p className="text-xs text-muted-foreground leading-snug">
            Version{" "}
            {versionLoading ? (
              <span className="inline-block w-12 h-3 align-middle rounded bg-muted-foreground/15 animate-pulse" />
            ) : version ? (
              <span className="font-mono">v{version}</span>
            ) : (
              <span className="font-mono">—</span>
            )}
          </p>
        </div>
        {showQuotaSection && (
          <QuotaSection
            quota={quota ?? null}
            loading={quotaLoading}
            cooldown={quotaCooldown}
            onRefresh={onRefreshQuota!}
          />
        )}
        {!isInUse && (
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={onSetDefault}
            disabled={settingDefault}
          >
            {settingDefault ? t("providers.activating") : t("providers.activate")}
          </Button>
        )}
      </CardContent>
    </Card>
  );
}
