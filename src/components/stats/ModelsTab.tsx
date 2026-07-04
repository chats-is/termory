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
import { useT } from "@/i18n";
import {
  type DailyModelTokens,
  type ModelUsage,
  displayModelName,
  niceMax
} from "@/lib/stats-utils";

/** Blue monochrome ladder, rank 0 darkest (fixed hexes like the old
 * chart's series colors — readable on light and dark). The last entry
 * is the "Others" bucket. */
const MODEL_COLORS = [
  "#2563eb", // blue-600
  "#3b82f6", // blue-500
  "#60a5fa", // blue-400
  "#93c5fd", // blue-300
  "#bfdbfe", // blue-200
  "#dbeafe" // blue-100
] as const;
const OTHERS_COLOR = "#cbd5e1"; // slate-300
/** Stacked series cap — ranks beyond this fold into "Others". */
const MAX_STACKS = MODEL_COLORS.length;

export function ModelsTab({
  data,
  usage
}: {
  data: DailyModelTokens;
  usage: ModelUsage[];
}) {
  const t = useT();

  // "Unknown" stays out of the legend (UI-wide convention) but its
  // tokens still count — in the chart it folds into "Others" so the
  // stacked totals keep matching dailyTokens.
  const ranked = data.models.filter((m) => m !== "Unknown");
  const stacked = ranked.slice(0, MAX_STACKS);
  const foldModels = [
    ...ranked.slice(MAX_STACKS),
    ...(data.models.includes("Unknown") ? ["Unknown"] : [])
  ];
  const othersKey = "__others__";

  const chartData = React.useMemo(() => {
    return data.dates.map((date, i) => {
      const row: Record<string, number | string> = { date };
      for (const m of stacked) row[m] = data.series[m]?.[i] ?? 0;
      if (foldModels.length > 0) {
        row[othersKey] = foldModels.reduce(
          (sum, m) => sum + (data.series[m]?.[i] ?? 0),
          0
        );
      }
      return row;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data]);

  const yMax = React.useMemo(() => {
    let max = 0;
    for (const row of chartData) {
      let total = 0;
      for (const key of Object.keys(row)) {
        if (key === "date") continue;
        total += row[key] as number;
      }
      if (total > max) max = total;
    }
    return niceMax(max);
  }, [chartData]);

  const dateFmt = React.useMemo(
    () =>
      new Intl.DateTimeFormat(getFormatLocale(), {
        month: "short",
        day: "numeric"
      }),
    []
  );
  const tickLabel = (key: string): string => {
    const [y, m, d] = key.split("-").map(Number);
    return dateFmt.format(new Date(y, m - 1, d));
  };

  const totalTokens = usage.reduce((sum, u) => sum + u.tokens, 0);
  const legendRows = usage.filter((u) => u.model !== "Unknown");
  const colorOf = (model: string): string => {
    const rank = stacked.indexOf(model);
    return rank >= 0 ? MODEL_COLORS[rank] : OTHERS_COLOR;
  };

  return (
    <div className="flex flex-col gap-4">
      <div className="h-72">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={chartData} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
            <CartesianGrid strokeDasharray="3 3" vertical={false} stroke="var(--border)" />
            <XAxis
              dataKey="date"
              tickFormatter={tickLabel}
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
                const rows = payload
                  .filter((p) => (p.value as number) > 0)
                  .reverse(); // stack order top-down
                if (rows.length === 0) return null;
                return (
                  <div className="rounded-md border bg-popover px-3 py-2 text-xs shadow-md">
                    <div className="font-medium mb-1">
                      {tickLabel(String(label))}
                    </div>
                    {rows.map((p) => (
                      <div key={String(p.dataKey)} className="flex items-center gap-2">
                        <span
                          aria-hidden
                          className="inline-block size-2 rounded-[2px]"
                          style={{ background: p.color }}
                        />
                        <span className="text-muted-foreground">
                          {p.dataKey === "__others__"
                            ? t("stats.othersBucket")
                            : displayModelName(String(p.dataKey))}
                        </span>
                        <span className="ml-auto pl-3 font-mono tabular-nums">
                          {formatCompact(p.value as number)}
                        </span>
                      </div>
                    ))}
                  </div>
                );
              }}
            />
            {stacked.map((m, i) => (
              <Bar
                key={m}
                dataKey={m}
                stackId="tokens"
                fill={MODEL_COLORS[i]}
                radius={i === 0 ? [2, 2, 0, 0] : undefined}
                isAnimationActive={false}
              />
            ))}
            {foldModels.length > 0 && (
              <Bar
                dataKey={othersKey}
                stackId="tokens"
                fill={OTHERS_COLOR}
                isAnimationActive={false}
              />
            )}
          </BarChart>
        </ResponsiveContainer>
      </div>

      {/* Legend: dot · name · "{in} in · {out} out" · share% */}
      <div className="flex flex-col gap-1.5">
        {legendRows.map((u) => (
          <div key={u.model} className="flex items-center gap-2 text-sm">
            <span
              aria-hidden
              className="inline-block size-2.5 rounded-[3px] shrink-0"
              style={{ background: colorOf(u.model) }}
            />
            <span className="font-medium truncate">
              {displayModelName(u.model)}
            </span>
            <span className="ml-auto pl-3 text-muted-foreground whitespace-nowrap">
              {t("stats.inOut", {
                in: formatCompact(u.input),
                out: formatCompact(u.output)
              })}
            </span>
            <span className="w-14 text-right font-medium tabular-nums">
              {totalTokens > 0
                ? `${((u.tokens / totalTokens) * 100).toFixed(1)}%`
                : "—"}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
