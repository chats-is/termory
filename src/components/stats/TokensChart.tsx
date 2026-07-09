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
import { type DailyModelTokens, type DailyTokens } from "@/lib/stats-utils";

/**
 * Round a value up to a clean axis bound (1 / 2 / 2.5 / 5 × 10ⁿ) — the
 * Tokens chart's Y-axis domain, kept fixed across the Type/Model toggle so
 * it never jumps. A layout helper for this chart, not a statistic.
 */
function niceMax(v: number): number {
  if (v <= 0) return 1;
  const base = 10 ** Math.floor(Math.log10(v));
  const f = v / base;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * base;
}

export type TokensGroupBy = "type" | "model";

// Token-type series, stacked bottom → top. Cached usually dwarfs the
// rest (prompt-cache hits), so it sits at the base and the smaller
// categories ride on top where their thin slices stay visible.
// input/output/cached/reasoning are NOMINAL kinds — no magnitude order —
// so each gets its OWN hue (blue/green/yellow/magenta) rather than a
// shade of one blue (the old 4-blue ramp collapsed under color-blindness,
// tritan ΔE 7.6). Colors are mode-aware CSS vars (`--stat-tok-*`, defined
// + validated per surface in globals.css); recharts renders a `var(...)`
// fill fine (same as OTHERS_COLOR below). `input` keeps the brand-blue
// slot. Validated as a set (validate_palette.js --pairs all): worst
// adjacent CVD ΔE 24.2 light / 10.3 dark, cleared by the legend labels.
const TYPE_SERIES: {
  key: keyof DailyTokens;
  labelKey: MessageKey;
  color: string;
}[] = [
  { key: "cached", labelKey: "stats.tokens.cached", color: "var(--stat-tok-cached)" },
  { key: "input", labelKey: "stats.tokens.input", color: "var(--stat-tok-input)" },
  { key: "output", labelKey: "stats.tokens.output", color: "var(--stat-tok-output)" },
  { key: "reasoning", labelKey: "stats.tokens.reasoning", color: "var(--stat-tok-reasoning)" }
];

// Model identity colors — grouped by PROVIDER FAMILY. Every model of a
// provider wears a SHADE of that provider's hue, so a model always reads as
// its provider (a Claude model never looks "custom"). Same-provider models
// are told apart by lightness, assigned by all-time usage rank: step 0 is
// the brand anchor (the most-used model, boldest), then progressively
// LIGHTER shades as usage drops. The ramps are CSS vars validated as
// ordinal scales, and the seven anchors validated as a categorical set
// (see globals.css `--stat-<provider>-N` for the full rationale + the CVD
// numbers). Recognized vendors + their hue: claude=clay, gemini=blue,
// openai=teal, mistral=gold, deepseek=violet, qwen=magenta, grok=crimson,
// glm=indigo, minimax=rose.
type Provider =
  | "claude"
  | "gemini"
  | "openai"
  | "mistral"
  | "deepseek"
  | "qwen"
  | "grok"
  | "glm"
  | "minimax"
  | "other";
function inferProvider(model: string): Provider {
  // Strip a leading `vendor/` prefix (e.g. `anthropic/claude-fable-5` from a
  // gateway) before matching the model family.
  const m = model.toLowerCase().replace(/^[a-z0-9._-]+\//, "");
  if (m.startsWith("claude")) return "claude";
  if (m.startsWith("gemini")) return "gemini";
  if (/^(gpt|o1|o3|o4|chatgpt)/.test(m)) return "openai";
  // Mistral's whole model family (le Chat / API): mistral, mixtral, and the
  // codestral / magistral / ministral / pixtral / devstral lines.
  if (/^(mistral|mixtral|codestral|magistral|ministral|pixtral|devstral)/.test(m))
    return "mistral";
  if (m.startsWith("deepseek")) return "deepseek";
  // Alibaba Qwen family: qwen / qwen2 / qwen3 / qwq / qvq.
  if (/^(qwen|qwq|qvq)/.test(m)) return "qwen";
  if (m.startsWith("grok")) return "grok";
  // Zhipu GLM family: glm-4 / glm-4.6 / chatglm / codegeex.
  if (/^(glm|chatglm|codegeex)/.test(m)) return "glm";
  // MiniMax: the MiniMax-* line + the older `abab` models (e.g. abab6.5s).
  if (/^(minimax|abab)/.test(m)) return "minimax";
  return "other";
}
// Provider brand-family ramps (resolved from globals.css — one set of
// values for both themes). A model gets the step at its rank WITHIN its
// provider, clamped to the last.
const RAMP_STEPS = 5;
const providerRamp = (
  p: Exclude<Provider, "other">,
  rank: number
): string => `var(--stat-${p}-${Math.min(rank, RAMP_STEPS - 1)})`;
// Neutral pool for models whose provider can't be inferred (an exotic custom
// / gateway id — now that the seven mainstream vendors above are recognized,
// this is a rare fallback, not the common case). The seven brand families
// fill most of the CVD-distinct hue space, leaving only these two hues that
// stay ≥ ΔE 8 from every anchor (validated --pairs all) — so a pool model
// still can't be mistaken for a branded one. Beyond two distinct customs
// they share the Others grey; identity leans on the legend's direct labels.
const EXTRA_MODEL_COLORS = [
  "#0891b2", // cyan-600
  "#64748b" // slate-500
] as const;
// Neutral "Others" bucket (models beyond the extra-pool slots above, or
// beyond the top-6-in-window cap) — reuses the app's own muted-foreground
// token (already proven to work as a raw SVG/CSS attribute value
// elsewhere in this file, e.g. the axis `stroke`), so it's legible and
// light/dark-aware with no extra code.
const OTHERS_COLOR = "var(--muted-foreground)";
const MAX_STACKS = 6;

/** Y-axis tick left-anchored at the plot's left edge, so the numbers
 * line up with the card title above (recharts injects `y` + `payload`;
 * we ignore the injected `x` and render at x=0). */
function LeftYTick({
  y,
  payload
}: {
  y?: number;
  payload?: { value: number };
}) {
  return (
    <text
      x={0}
      y={y}
      dy={3}
      textAnchor="start"
      fontSize={11}
      fill="var(--muted-foreground)"
    >
      {payload ? formatCompact(payload.value) : ""}
    </text>
  );
}

type Series = { key: string; color: string; label: string };

/**
 * One chart, one toggle: "Tokens" broken down either by type
 * (cached/input/output/reasoning) or by model. Both breakdowns sum to
 * the SAME per-day total (`Σ modelData.series[*][d] === daily[d].total`,
 * the locked cross-consistency rule) — this used to be two nearly
 * identical chart components (DailyTokensChart + ModelUsageChart) with
 * duplicated axis/tooltip/tick code that drifted out of sync repeatedly.
 * `groupBy` is controlled by the parent so the toggle can live in the
 * shared card header, matching how CalendarHeatmap's card title works.
 */
export function TokensChart({
  groupBy,
  daily,
  modelData,
  globalModelRank
}: {
  groupBy: TokensGroupBy;
  daily: DailyTokens[];
  modelData: DailyModelTokens;
  /** Model names ranked by ALL-TIME usage (not the current window) —
   * drives color assignment so a model's color is stable across
   * source/range filter changes. See StatsPage's `globalModelRank`. */
  globalModelRank: string[];
}) {
  const t = useT();

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

  // "Unknown" stays out of the legend (UI-wide convention) but its
  // tokens still count — in the chart it folds into "Others" so the
  // stacked totals keep matching dailyTokens. WHICH models get their own
  // stacked segment is still based on the CURRENT window's ranking (what's
  // actually significant in the window you're looking at); only the
  // COLOR each one gets is looked up from the stable all-time rank below.
  const ranked = modelData.models.filter((m) => m !== "Unknown");
  const stackedModels = ranked.slice(0, MAX_STACKS);
  const foldModels = [
    ...ranked.slice(MAX_STACKS),
    ...(modelData.models.includes("Unknown") ? ["Unknown"] : [])
  ];
  const othersKey = "__others__";

  // Stable identity color, ranked by ALL-TIME usage (not the model's
  // position in `stackedModels`, which is window-relative and would repaint
  // on every filter change). A known provider's models take that provider's
  // brand-family ramp, stepped by rank WITHIN the provider (its #1 all-time
  // model = the anchor, step 0). Unknown-provider models draw from the
  // neutral pool, ranked among all such models together.
  const otherRank = globalModelRank.filter((m) => inferProvider(m) === "other");
  const colorOfModel = (model: string): string => {
    const provider = inferProvider(model);
    if (provider !== "other") {
      const rank = globalModelRank
        .filter((m) => inferProvider(m) === provider)
        .indexOf(model);
      return providerRamp(provider, rank < 0 ? 0 : rank);
    }
    const rank = otherRank.indexOf(model);
    return rank >= 0 && rank < EXTRA_MODEL_COLORS.length
      ? EXTRA_MODEL_COLORS[rank]
      : OTHERS_COLOR;
  };

  const modelChartData = React.useMemo(() => {
    return modelData.dates.map((date, i) => {
      const row: Record<string, number | string> = { date };
      for (const m of stackedModels) row[m] = modelData.series[m]?.[i] ?? 0;
      if (foldModels.length > 0) {
        row[othersKey] = foldModels.reduce(
          (sum, m) => sum + (modelData.series[m]?.[i] ?? 0),
          0
        );
      }
      return row;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [modelData]);

  const typeSeries: Series[] = TYPE_SERIES.map((s) => ({
    key: s.key,
    color: s.color,
    label: t(s.labelKey)
  }));
  const modelSeries: Series[] = [
    ...stackedModels.map((m) => ({
      key: m,
      color: colorOfModel(m),
      label: m
    })),
    ...(foldModels.length > 0
      ? [{ key: othersKey, color: OTHERS_COLOR, label: t("stats.othersBucket") }]
      : [])
  ];
  const activeSeries = groupBy === "type" ? typeSeries : modelSeries;
  const chartData = groupBy === "type" ? daily : modelChartData;
  const labelFor = (key: string): string =>
    activeSeries.find((s) => s.key === key)?.label ?? key;

  // Computed from `daily` regardless of mode — both breakdowns share the
  // exact same per-day total, so the Y-axis domain never jumps when the
  // user switches the toggle.
  const yMax = React.useMemo(
    () => niceMax(daily.reduce((max, d) => Math.max(max, d.total), 0)),
    [daily]
  );
  const grandTotal = daily.reduce((sum, d) => sum + d.total, 0);
  // Per-model window totals for the legend (in/out/share) — module 4's
  // aggregator returns them alongside the stacked series. Includes Unknown:
  // the Others legend row below folds it in, so the shown percentages
  // always sum to exactly 100%.
  const usage = modelData.legend;
  const totalTokens = usage.reduce((sum, u) => sum + u.tokens, 0);
  const usageByModel = new Map(usage.map((u) => [u.model, u]));
  // The Others row's aggregate in/out/tokens (models beyond the top 6 +
  // Unknown) — same set as the chart's Others bar segment.
  const othersAgg = foldModels.reduce(
    (acc, m) => {
      const u = usageByModel.get(m);
      if (u) {
        acc.input += u.input;
        acc.output += u.output;
        acc.tokens += u.tokens;
      }
      return acc;
    },
    { input: 0, output: 0, tokens: 0 }
  );

  if (daily.length === 0 || grandTotal === 0) {
    return (
      <div className="h-64 flex items-center justify-center text-sm text-muted-foreground">
        {t("stats.noData")}
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="h-72">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart
            data={chartData}
            margin={{ top: 8, right: 8, left: 0, bottom: 0 }}
          >
            <CartesianGrid
              strokeDasharray="3 3"
              vertical={false}
              stroke="var(--border)"
            />
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
              width={52}
              tickLine={false}
              axisLine={false}
              // Left-anchored so the numbers line up with the card
              // title above (matches the calendar heatmap).
              tick={<LeftYTick />}
            />
            <RechartsTooltip
              cursor={{ fill: "var(--muted)" }}
              content={({ active, payload, label }) => {
                if (!active || !payload || payload.length === 0) return null;
                const rows = payload
                  .filter((p) => (p.value as number) > 0)
                  .reverse(); // stack order top-down
                if (rows.length === 0) return null;
                const total = payload.reduce(
                  (sum, p) => sum + (p.value as number),
                  0
                );
                return (
                  <div className="rounded-md border bg-popover px-3 py-2 text-xs shadow-md">
                    <div className="font-medium mb-1">
                      {tickLabel(String(label))}
                    </div>
                    {rows.map((p) => (
                      <div
                        key={String(p.dataKey)}
                        className="flex items-center gap-2"
                      >
                        <span
                          aria-hidden
                          className="inline-block size-2 rounded-[2px]"
                          style={{ background: p.color }}
                        />
                        <span className="text-muted-foreground">
                          {labelFor(String(p.dataKey))}
                        </span>
                        <span className="ml-auto pl-3 font-mono tabular-nums">
                          {formatCompact(p.value as number)}
                        </span>
                      </div>
                    ))}
                    <div className="mt-1 flex items-center gap-2 border-t pt-1">
                      <span className="text-muted-foreground">
                        {t("stats.tokens.total")}
                      </span>
                      <span className="ml-auto pl-3 font-mono tabular-nums font-medium">
                        {formatCompact(total)}
                      </span>
                    </div>
                  </div>
                );
              }}
            />
            {activeSeries.map((s) => (
              <Bar
                key={s.key}
                dataKey={s.key}
                stackId="tokens"
                fill={s.color}
                isAnimationActive={false}
              />
            ))}
          </BarChart>
        </ResponsiveContainer>
      </div>

      {groupBy === "type" ? (
        // Legend: dot · type · window total.
        <div className="flex flex-wrap gap-x-4 gap-y-1.5">
          {TYPE_SERIES.map((s) => (
            <div key={s.key} className="flex items-center gap-2 text-sm">
              <span
                aria-hidden
                className="inline-block size-2.5 rounded-[3px] shrink-0"
                style={{ background: s.color }}
              />
              <span className="text-muted-foreground">{t(s.labelKey)}</span>
              <span className="font-medium tabular-nums">
                {formatCompact(
                  daily.reduce((sum, d) => sum + (d[s.key] as number), 0)
                )}
              </span>
            </div>
          ))}
        </div>
      ) : (
        // Legend: dot · name · "{in} in · {out} out" · share%. Mirrors the
        // chart's series 1:1 — the stacked models plus ONE Others row (never
        // one row per folded model) — so colors never collide and the
        // percentages always sum to 100%.
        <div className="flex flex-col gap-1.5">
          {stackedModels.map((m) => {
            const u = usageByModel.get(m);
            return (
              <div key={m} className="flex items-center gap-2 text-sm">
                <span
                  aria-hidden
                  className="inline-block size-2.5 rounded-[3px] shrink-0"
                  style={{ background: colorOfModel(m) }}
                />
                <span className="font-medium truncate">{m}</span>
                <span className="ml-auto pl-3 text-muted-foreground whitespace-nowrap">
                  {t("stats.inOut", {
                    in: formatCompact(u?.input ?? 0),
                    out: formatCompact(u?.output ?? 0)
                  })}
                </span>
                <span className="w-14 text-right font-medium tabular-nums">
                  {totalTokens > 0
                    ? `${(((u?.tokens ?? 0) / totalTokens) * 100).toFixed(1)}%`
                    : "—"}
                </span>
              </div>
            );
          })}
          {foldModels.length > 0 && (
            <div className="flex items-center gap-2 text-sm">
              <span
                aria-hidden
                className="inline-block size-2.5 rounded-[3px] shrink-0"
                style={{ background: OTHERS_COLOR }}
              />
              <span className="font-medium truncate">
                {t("stats.othersBucket")}
              </span>
              <span className="ml-auto pl-3 text-muted-foreground whitespace-nowrap">
                {t("stats.inOut", {
                  in: formatCompact(othersAgg.input),
                  out: formatCompact(othersAgg.output)
                })}
              </span>
              <span className="w-14 text-right font-medium tabular-nums">
                {totalTokens > 0
                  ? `${((othersAgg.tokens / totalTokens) * 100).toFixed(1)}%`
                  : "—"}
              </span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
