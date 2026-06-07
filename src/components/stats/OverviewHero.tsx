import { Card, CardContent } from "@/components/ui/card";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger
} from "@/components/ui/hover-card";
import { formatCompact, formatFullNumber } from "@/lib/format";
import type { ModelUsage } from "@/lib/stats-utils";
import type { TokenStats } from "@/types";
import { useT } from "@/i18n";
import { TOKEN_COLORS, BreakdownRow, ModelUsageList } from "./shared";

/**
 * One-row KPI strip — basic totals only:
 *   Sessions · Messages · Tokens · Models · Projects
 *
 * The Tokens cell reveals an input/output/cached/reasoning breakdown
 * on hover; the Models cell reveals a per-model token breakdown on
 * hover (both shadcn HoverCard).
 */

export function OverviewHero({
  sessions,
  messages,
  tokens,
  models,
  projects
}: {
  sessions: number;
  messages: number;
  tokens: TokenStats;
  models: ModelUsage[];
  projects: number;
}) {
  const t = useT();
  return (
    <Card className="p-3 gap-0 outline outline-1 outline-transparent bg-card shadow-sm">
      <CardContent className="px-0">
        <div className="grid grid-cols-2 md:grid-cols-5 gap-x-6 gap-y-3">
          <Kpi label={t("stats.kpi.sessions")} value={sessions} />
          <Kpi label={t("stats.kpi.messages")} value={messages} />
          <TokensKpi tokens={tokens} />
          <ModelsKpi models={models} />
          <Kpi label={t("stats.kpi.projects")} value={projects} />
        </div>
      </CardContent>
    </Card>
  );
}

function Kpi({
  label,
  value,
  compact
}: {
  label: string;
  value: number;
  compact?: boolean;
}) {
  const display = compact ? formatCompact(value) : formatFullNumber(value);
  return (
    <div className="flex flex-col gap-1">
      <span className="text-xs uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      <span className="text-3xl font-semibold tabular-nums leading-none">
        {display}
      </span>
    </div>
  );
}

function TokensKpi({ tokens }: { tokens: TokenStats }) {
  const t = useT();
  const hasBreakdown =
    tokens.input + tokens.output + tokens.cached + tokens.reasoning > 0;
  const valueNode = (
    <span className="text-3xl font-semibold tabular-nums leading-none cursor-default">
      {formatCompact(tokens.total)}
    </span>
  );
  return (
    <div className="flex flex-col gap-1">
      <span className="text-xs uppercase tracking-wide text-muted-foreground">
        {t("stats.kpi.tokens")}
      </span>
      {hasBreakdown ? (
        <HoverCard openDelay={80} closeDelay={80}>
          <HoverCardTrigger asChild>{valueNode}</HoverCardTrigger>
          <HoverCardContent
            className="w-auto p-3 text-xs leading-tight"
            side="bottom"
            align="start"
          >
            <div className="space-y-0.5 tabular-nums">
              <BreakdownRow color={TOKEN_COLORS.input} label={t("stats.tokens.input")} value={tokens.input} />
              <BreakdownRow color={TOKEN_COLORS.output} label={t("stats.tokens.output")} value={tokens.output} />
              <BreakdownRow
                color={TOKEN_COLORS.reasoning}
                label={t("stats.tokens.reasoning")}
                value={tokens.reasoning}
              />
              <BreakdownRow color={TOKEN_COLORS.cached} label={t("stats.tokens.cached")} value={tokens.cached} />
            </div>
            <div className="border-t border-border/40 mt-1.5 pt-1">
              <div className="flex items-center gap-2 tabular-nums">
                <span aria-hidden className="inline-block w-3 shrink-0" />
                <span className="text-muted-foreground w-20">{t("stats.tokens.total")}</span>
                <span className="font-medium">{formatCompact(tokens.total)}</span>
              </div>
            </div>
          </HoverCardContent>
        </HoverCard>
      ) : (
        valueNode
      )}
    </div>
  );
}

/** Models cell — main number is the count of distinct *named* models in
 * the window; hovering reveals each named model's approximate token
 * total. The "Unknown" bucket (sessions with no recorded model) is
 * excluded from both the count and the hover. */
function ModelsKpi({ models }: { models: ModelUsage[] }) {
  const t = useT();
  // Drop the "Unknown" bucket — sessions whose source recorded no model
  // (no assistant reply / older sessions). It's noise in a model-usage
  // breakdown, and the main count already ignores it.
  const named = models.filter((m) => m.model !== "Unknown");
  const valueNode = (
    <span className="text-3xl font-semibold tabular-nums leading-none cursor-default">
      {formatFullNumber(named.length)}
    </span>
  );
  return (
    <div className="flex flex-col gap-1">
      <span className="text-xs uppercase tracking-wide text-muted-foreground">
        {t("stats.kpi.models")}
      </span>
      {named.length > 0 ? (
        <HoverCard openDelay={80} closeDelay={80}>
          <HoverCardTrigger asChild>{valueNode}</HoverCardTrigger>
          <HoverCardContent
            className="w-auto min-w-[220px] p-3 text-xs leading-tight"
            side="bottom"
            align="start"
          >
            <ModelUsageList models={named} />
          </HoverCardContent>
        </HoverCard>
      ) : (
        valueNode
      )}
    </div>
  );
}
