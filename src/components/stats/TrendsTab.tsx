import React from "react";
import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip as RechartsTooltip,
  XAxis,
  YAxis
} from "recharts";
import { formatCompact, getFormatLocale } from "@/lib/format";
import { useT, type MessageKey } from "@/i18n";
import { type DailyTokens, niceMax } from "@/lib/stats-utils";

/** The four token classes, stacked bottom→top. Blue / emerald / amber /
 * purple — the same palette the old daily-tokens chart used. */
const SERIES: {
  key: keyof Pick<DailyTokens, "input" | "output" | "cached" | "reasoning">;
  color: string;
  labelKey: MessageKey;
}[] = [
  { key: "input", color: "#3b82f6", labelKey: "stats.tokens.input" },
  { key: "output", color: "#10b981", labelKey: "stats.tokens.output" },
  { key: "cached", color: "#f59e0b", labelKey: "stats.tokens.cached" },
  { key: "reasoning", color: "#a855f7", labelKey: "stats.tokens.reasoning" }
];

/**
 * Trends — per-day tokens as a stacked bar chart (Input / Output /
 * Cached / Reasoning), following the All/30d/7d range. Stacking (vs the
 * old four overlaid lines) fixes the readability problem where Cached —
 * routinely 1-3 orders of magnitude larger, from prompt-cache hits —
 * flattened the other three lines onto the baseline: every class now
 * keeps a visible segment, the bar height is the day's total, and the
 * tooltip surfaces exact per-class numbers. Same `dailyTokens` data as
 * the KPI token total, and the same stacked look as the Models tab.
 */
export function TrendsTab({ data }: { data: DailyTokens[] }) {
  const t = useT();

  const yMax = React.useMemo(() => {
    let max = 0;
    for (const d of data) if (d.total > max) max = d.total;
    return niceMax(max);
  }, [data]);

  const dateFmt = React.useMemo(
    () =>
      new Intl.DateTimeFormat(getFormatLocale(), {
        month: "short",
        day: "numeric"
      }),
    []
  );
  const longFmt = React.useMemo(
    () =>
      new Intl.DateTimeFormat(getFormatLocale(), {
        year: "numeric",
        month: "short",
        day: "numeric"
      }),
    []
  );
  const parse = (key: string): Date => {
    const [y, m, d] = key.split("-").map(Number);
    return new Date(y, m - 1, d);
  };

  return (
    <div className="flex flex-col gap-3">
      <div className="h-80">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart
            data={data}
            margin={{ top: 8, right: 8, left: 0, bottom: 0 }}
          >
            <CartesianGrid
              strokeDasharray="3 3"
              vertical={false}
              stroke="var(--border)"
            />
            <XAxis
              dataKey="date"
              tickFormatter={(v: string) => dateFmt.format(parse(v))}
              minTickGap={48}
              tickLine={false}
              axisLine={false}
              tick={{ fontSize: 11, fill: "var(--muted-foreground)" }}
            />
            <YAxis
              domain={[0, yMax]}
              tickFormatter={(v: number) => formatCompact(v)}
              width={52}
              tickLine={false}
              axisLine={false}
              tick={{ fontSize: 11, fill: "var(--muted-foreground)" }}
            />
            <RechartsTooltip
              cursor={{ fill: "var(--muted)" }}
              content={({ active, payload, label }) => {
                if (!active || !payload || payload.length === 0) return null;
                const row = payload[0]?.payload as DailyTokens | undefined;
                if (!row || row.total === 0) return null; // empty day → no card
                return (
                  <div className="rounded-md border bg-popover px-3 py-2 text-xs shadow-md">
                    <div className="font-medium mb-1">
                      {longFmt.format(parse(String(label)))}
                    </div>
                    {SERIES.map((s) => (
                      <div key={s.key} className="flex items-center gap-2">
                        <span
                          aria-hidden
                          className="inline-block size-2 rounded-[2px]"
                          style={{ background: s.color }}
                        />
                        <span className="text-muted-foreground">
                          {t(s.labelKey)}
                        </span>
                        <span className="ml-auto pl-3 font-mono tabular-nums">
                          {row[s.key] === 0 ? "—" : formatCompact(row[s.key])}
                        </span>
                      </div>
                    ))}
                    <div className="mt-1 flex items-center gap-2 border-t border-border/40 pt-1">
                      <span aria-hidden className="inline-block size-2" />
                      <span className="text-muted-foreground">
                        {t("stats.tokens.total")}
                      </span>
                      <span className="ml-auto pl-3 font-mono font-medium tabular-nums">
                        {formatCompact(row.total)}
                      </span>
                    </div>
                  </div>
                );
              }}
            />
            {SERIES.map((s, i) => (
              <Bar
                key={s.key}
                dataKey={s.key}
                stackId="tokens"
                fill={s.color}
                radius={i === SERIES.length - 1 ? [2, 2, 0, 0] : undefined}
                isAnimationActive={false}
              />
            ))}
          </BarChart>
        </ResponsiveContainer>
      </div>
      {/* Legend — color dot · class name. */}
      <div className="flex flex-wrap gap-x-4 gap-y-1 text-sm">
        {SERIES.map((s) => (
          <div key={s.key} className="flex items-center gap-2">
            <span
              aria-hidden
              className="inline-block size-2.5 rounded-[3px]"
              style={{ background: s.color }}
            />
            <span>{t(s.labelKey)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
