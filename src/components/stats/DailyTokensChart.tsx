import React from "react";
import {
  CartesianGrid,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip as RechartsTooltip,
  XAxis,
  YAxis
} from "recharts";
import { Card, CardContent } from "@/components/ui/card";
import { formatCompact, formatFullNumber } from "@/lib/format";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import type { DailyTokens } from "@/lib/stats-utils";
import { useT } from "@/i18n";
import {
  BreakdownRow,
  TOKEN_COLORS,
  formatDateLong,
  formatDateShort
} from "./shared";

/**
 * Daily tokens — 4 trend lines on one linear-scale chart.
 *
 *   Input    — blue
 *   Output   — emerald
 *   Cached   — amber
 *   Reasoning— purple
 *
 * All four series share the same Y axis. Cached typically dominates
 * by 1-3 orders of magnitude (Claude prompt cache hits are huge), so
 * the smaller series sit close to the x-axis. That's an honest
 * reflection of the data — the tooltip surfaces the exact numbers
 * per day so the user can read off Input / Output / Reasoning values
 * even when their lines hug the baseline.
 */

function CustomTooltip({
  active,
  payload,
  label
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: DailyTokens }>;
  label?: string;
}) {
  const t = useT();
  if (!active || !payload || payload.length === 0) return null;
  const row = payload[0]?.payload;
  if (!row) return null;
  // Skip tooltip entirely for empty days — no data = no card.
  if (row.total === 0) return null;
  return (
    <div
      className="rounded-md border bg-popover text-popover-foreground text-xs shadow-md px-2.5 py-2 leading-tight"
      style={{ borderColor: "var(--border)" }}
    >
      <div className="font-medium pb-1.5 mb-1.5 border-b border-border/40">
        {formatDateLong(String(label ?? ""))}
      </div>
      <div className="space-y-0.5 tabular-nums">
        <BreakdownRow color={TOKEN_COLORS.input} label={t("stats.tokens.input")} value={row.input} />
        <BreakdownRow color={TOKEN_COLORS.output} label={t("stats.tokens.output")} value={row.output} />
        <BreakdownRow
          color={TOKEN_COLORS.reasoning}
          label={t("stats.tokens.reasoning")}
          value={row.reasoning}
        />
        <BreakdownRow color={TOKEN_COLORS.cached} label={t("stats.tokens.cached")} value={row.cached} />
      </div>
      <div className="border-t border-border/40 mt-1.5 pt-1">
        <div className="flex items-center gap-2 tabular-nums">
          <span aria-hidden className="inline-block w-3 shrink-0" />
          <span className="text-muted-foreground w-20">{t("stats.tokens.total")}</span>
          <span className="font-medium">{formatCompact(row.total)}</span>
        </div>
      </div>
    </div>
  );
}

export function DailyTokensChart({ data }: { data: DailyTokens[] }) {
  const t = useT();
  const total = React.useMemo(
    () => data.reduce((acc, b) => acc + b.total, 0),
    [data]
  );
  // Anchor on the LAST bucket and walk backwards by 2 days. First
  // bucket only ends up labeled when n is odd (29 → 0 lands cleanly);
  // for even n the first tick lands on index 1, and the unlabeled
  // index 0 is acceptable per spec. Every visible gap is exactly 2.
  const xTicks = React.useMemo(() => {
    const n = data.length;
    if (n === 0) return [];
    const indices: number[] = [];
    for (let i = n - 1; i >= 0; i -= 2) indices.push(i);
    indices.reverse();
    return indices.map((i) => data[i].date);
  }, [data]);
  return (
    <Card className="p-3 gap-2 outline outline-1 outline-transparent bg-card shadow-sm">
      <CardContent className="px-0 flex flex-col gap-3">
        <div className="flex items-baseline justify-between gap-2 flex-wrap">
          <h3 className="text-sm font-medium text-muted-foreground uppercase tracking-wide">
            {t("stats.dailyTokens")}
          </h3>
          {total > 0 && (
            <Tooltip>
              <TooltipTrigger asChild>
                <span className="text-[11px] text-muted-foreground tabular-nums cursor-default">
                  {formatCompact(total)} {t("stats.kpi.tokens")}
                </span>
              </TooltipTrigger>
              <TooltipContent>{formatFullNumber(total)} tokens</TooltipContent>
            </Tooltip>
          )}
        </div>
        {total === 0 ? (
          <div className="h-[220px] flex items-center justify-center text-sm text-muted-foreground">
            No token data in this range.
          </div>
        ) : (
          <div className="h-[220px] w-full">
            <ResponsiveContainer>
              <LineChart
                data={data}
                margin={{ top: 6, right: 0, bottom: 0, left: 0 }}
              >
                <CartesianGrid
                  strokeDasharray="3 3"
                  stroke="currentColor"
                  opacity={0.08}
                  vertical={false}
                />
                <XAxis
                  dataKey="date"
                  tick={{ fontSize: 10, fill: "currentColor", opacity: 0.7 }}
                  axisLine={false}
                  tickLine={false}
                  tickFormatter={formatDateShort}
                  ticks={xTicks}
                  interval={0}
                  padding={{ left: 0, right: 16 }}
                />
                <YAxis
                  tick={{ fontSize: 11, fill: "currentColor", opacity: 0.7 }}
                  axisLine={false}
                  tickLine={false}
                  width={42}
                  tickFormatter={formatCompact}
                  domain={[0, "auto"]}
                />
                <RechartsTooltip content={<CustomTooltip />} />
                <Line
                  type="monotone"
                  dataKey="cached"
                  stroke={TOKEN_COLORS.cached}
                  strokeWidth={2}
                  dot={false}
                  activeDot={{ r: 4, strokeWidth: 0 }}
                  isAnimationActive={false}
                />
                <Line
                  type="monotone"
                  dataKey="output"
                  stroke={TOKEN_COLORS.output}
                  strokeWidth={2}
                  dot={false}
                  activeDot={{ r: 4, strokeWidth: 0 }}
                  isAnimationActive={false}
                />
                <Line
                  type="monotone"
                  dataKey="input"
                  stroke={TOKEN_COLORS.input}
                  strokeWidth={2}
                  dot={false}
                  activeDot={{ r: 4, strokeWidth: 0 }}
                  isAnimationActive={false}
                />
                <Line
                  type="monotone"
                  dataKey="reasoning"
                  stroke={TOKEN_COLORS.reasoning}
                  strokeWidth={2}
                  dot={false}
                  activeDot={{ r: 4, strokeWidth: 0 }}
                  isAnimationActive={false}
                />
              </LineChart>
            </ResponsiveContainer>
          </div>
        )}
        <div className="flex justify-end items-center gap-3 text-[10px] text-muted-foreground">
          <Legend color={TOKEN_COLORS.input} label={t("stats.tokens.input")} />
          <Legend color={TOKEN_COLORS.output} label={t("stats.tokens.output")} />
          <Legend color={TOKEN_COLORS.reasoning} label={t("stats.tokens.reasoning")} />
          <Legend color={TOKEN_COLORS.cached} label={t("stats.tokens.cached")} />
        </div>
      </CardContent>
    </Card>
  );
}

function Legend({ color, label }: { color: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1">
      <span
        aria-hidden
        className="inline-block w-3 h-[2px] rounded-full"
        style={{ background: color }}
      />
      <span>{label}</span>
    </span>
  );
}
