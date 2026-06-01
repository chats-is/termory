import { Card, CardContent } from "@/components/ui/card";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger
} from "@/components/ui/hover-card";
import { formatCompact, formatFullNumber } from "@/lib/format";
import type { TokenStats } from "@/types";
import { TOKEN_COLORS, BreakdownRow } from "./shared";

/**
 * One-row KPI strip — basic totals only:
 *   Sessions · Messages · Tokens · Projects
 *
 * The Tokens cell reveals an input/output/cached/reasoning breakdown
 * on hover (shadcn HoverCard), matching the DailyTokensChart tooltip.
 */

export function OverviewHero({
  sessions,
  messages,
  tokens,
  projects
}: {
  sessions: number;
  messages: number;
  tokens: TokenStats;
  projects: number;
}) {
  return (
    <Card className="p-3 gap-0 outline outline-1 outline-transparent bg-card shadow-sm">
      <CardContent className="px-0">
        <div className="grid grid-cols-2 md:grid-cols-4 gap-x-6 gap-y-3">
          <Kpi label="Sessions" value={sessions} />
          <Kpi label="Messages" value={messages} />
          <TokensKpi tokens={tokens} />
          <Kpi label="Projects" value={projects} />
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
        Tokens
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
              <BreakdownRow color={TOKEN_COLORS.input} label="Input" value={tokens.input} />
              <BreakdownRow color={TOKEN_COLORS.output} label="Output" value={tokens.output} />
              <BreakdownRow
                color={TOKEN_COLORS.reasoning}
                label="Reasoning"
                value={tokens.reasoning}
              />
              <BreakdownRow color={TOKEN_COLORS.cached} label="Cached" value={tokens.cached} />
            </div>
            <div className="border-t border-border/40 mt-1.5 pt-1">
              <div className="flex items-center gap-2 tabular-nums">
                <span aria-hidden className="inline-block w-3 shrink-0" />
                <span className="text-muted-foreground w-20">Total</span>
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
