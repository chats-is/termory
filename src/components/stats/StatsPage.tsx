import React from "react";
import { Layers, RefreshCw } from "lucide-react";
import { BrandIcon } from "@/components/BrandIcon";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { CLI_APP_LABEL, CLI_APP_SOURCE_BADGE } from "@/constants";
import { isSourceEnabled, visibleSources } from "@/lib/provider-utils";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type { AppSession, CliApp } from "@/types";
import {
  type DateRange,
  type DateRangePreset,
  type SourceFilter,
  dailyActivity,
  dailyModelTokens,
  dailyTokens,
  filterSessions,
  overviewKpis,
  resolveRange,
} from "@/lib/stats-utils";
import { OverviewSection } from "./OverviewSection";
import { TokensChart, type TokensGroupBy } from "./TokensChart";

// Reference order: All | 30d | 7d.
const RANGES: { id: DateRangePreset; labelKey: MessageKey }[] = [
  { id: "all", labelKey: "stats.range.all" },
  { id: "30d", labelKey: "stats.range.30d" },
  { id: "7d", labelKey: "stats.range.7d" },
];

const SOURCES: SourceFilter[] = [
  "All",
  "claude",
  "codex",
  "gemini",
  "opencode",
  "grok",
];

/**
 * Stats dashboard — one continuous page over the same filtered window:
 * 8 KPI cards + calendar heatmap, followed by one Tokens chart toggled
 * between type and model breakdown (see TokensChart — both
 * breakdowns sum to the same per-day total). Shared controls: source
 * filter, All/30d/7d range, refresh.
 */
export function StatsPage({
  sessions,
  onRefresh,
  refreshing,
  sourceToggles = {},
  sourceOrder,
}: {
  sessions: AppSession[];
  onRefresh: () => void;
  refreshing: boolean;
  /** Settings → Tools map (absent key = enabled); disabled sources
   *  lose their filter pill (their data is already gone backend-side). */
  sourceToggles?: Partial<Record<CliApp, boolean>>;
  /** Pill order = the Settings → Tools drag order (App-resolved). */
  sourceOrder?: readonly CliApp[];
}) {
  const t = useT();
  const [range, setRange] = React.useState<DateRange>({ preset: "30d" });
  const [source, setSource] = React.useState<SourceFilter>("All");
  // Pill order follows the Settings → Tools drag order; Claude Desktop
  // has no session source, and SOURCES pins the toggle-less fallback.
  const orderedKeys = (sourceOrder ?? SOURCES.filter((s) => s !== "All")) as CliApp[];
  // Shares the `visibleSources` selector with the Records sidebar.
  const sourceFilters: SourceFilter[] = [
    "All",
    ...visibleSources(orderedKeys, sourceToggles, { recordsOnly: true }),
  ];
  // A source disabled while selected falls back to All.
  React.useEffect(() => {
    if (source !== "All" && !isSourceEnabled(sourceToggles, source)) setSource("All");
  }, [source, sourceToggles]);
  const [groupBy, setGroupBy] = React.useState<TokensGroupBy>("type");

  const resolved = React.useMemo(
    () => resolveRange(range, sessions),
    [range, sessions],
  );
  const filtered = React.useMemo(
    () => filterSessions(sessions, resolved, source),
    [sessions, resolved, source],
  );
  // Module 1 — every KPI card value (sessions/messages/tokens + streaks +
  // peak hour + favorite model) from one function, range-scoped.
  const kpis = React.useMemo(
    () => overviewKpis(filtered, resolved),
    [filtered, resolved],
  );
  // Module 2 — the activity heatmap follows the source filter but NOT the
  // All/30d/7d range: it's always the full history, so it's fed by
  // source-filtered sessions and builds its own fixed window internally.
  const sourceFiltered = React.useMemo(
    () =>
      source === "All"
        ? sessions
        : sessions.filter((s) => s.source.toLowerCase() === source),
    [sessions, source],
  );
  const activity = React.useMemo(
    () => dailyActivity(sourceFiltered),
    [sourceFiltered],
  );
  // Module 3 — per-day tokens by type (input/output/cached/reasoning),
  // range-scoped.
  const tokensDaily = React.useMemo(
    () => dailyTokens(filtered, resolved),
    [filtered, resolved],
  );
  // Module 4 — per-day tokens by model (stacked series + legend),
  // range-scoped.
  const modelDaily = React.useMemo(
    () => dailyModelTokens(filtered, resolved),
    [filtered, resolved],
  );
  // Stable model color rank: all-time usage WITHIN the current source
  // filter, ignoring the range preset — so a model's TokensChart color
  // doesn't repaint just because the range toggle changed the window's
  // relative ranking (only a source-filter change can reorder it). Reuses
  // module 4's aggregator over an all-time window; `models` is already
  // sorted by token total desc.
  const globalModelRank = React.useMemo(() => {
    const allTime = resolveRange({ preset: "all" }, sourceFiltered);
    return dailyModelTokens(sourceFiltered, allTime).models.filter(
      (m) => m !== "Unknown",
    );
  }, [sourceFiltered]);
  return (
    <div className="flex-1 min-h-0 flex flex-col">
      {/* Fixed header (outside the scroll area, like the Providers page):
          one card holding the source filter, range pills, and refresh. */}
      <div className="px-3 mt-3 mb-3 flex flex-col gap-3">
        <div className="rounded-md bg-muted p-3">
          {/* Source filter (fills) + range pills + refresh. */}
          <div className="flex items-center gap-2">
            <Tabs
              value={source}
              onValueChange={(v) => setSource(v as SourceFilter)}
              className="flex-1 min-w-0"
            >
              <TabsList
                aria-label={t("stats.sourceFilter")}
                className="w-full justify-start gap-1 bg-transparent p-0 [&>button]:flex-none [&>button]:rounded-md [&>button]:px-3"
              >
                {sourceFilters.map((s) => (
                  <TabsTrigger key={s} value={s}>
                    {s === "All" ? (
                      <Layers className="size-4 shrink-0" aria-hidden />
                    ) : (
                      <BrandIcon source={CLI_APP_SOURCE_BADGE[s as CliApp]} />
                    )}
                    <span>
                      {s === "All"
                        ? t("stats.source.all")
                        : CLI_APP_LABEL[s as CliApp]}
                    </span>
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
            {/* Range pills — to the right of the source filter. */}
            <Tabs
              value={range.preset}
              onValueChange={(v) => setRange({ preset: v as DateRangePreset })}
              className="shrink-0"
            >
              <TabsList aria-label={t("stats.range.label")}>
                {RANGES.map((r) => (
                  <TabsTrigger key={r.id} value={r.id}>
                    {t(r.labelKey)}
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  onClick={onRefresh}
                  disabled={refreshing}
                  aria-label={t("stats.refresh")}
                  className={cn(
                    "h-9 w-9 shrink-0 rounded-md bg-background shadow-sm inline-flex items-center justify-center",
                    "hover:bg-accent hover:text-accent-foreground transition-colors",
                    refreshing && "opacity-60",
                  )}
                >
                  <RefreshCw
                    className={cn("size-4", refreshing && "animate-spin")}
                  />
                </button>
              </TooltipTrigger>
              <TooltipContent side="left">{t("stats.refresh")}</TooltipContent>
            </Tooltip>
          </div>
        </div>
      </div>
      <div className="flex-1 min-h-0 overflow-auto px-3 pb-0">
        <div className="flex flex-col gap-4">
          <OverviewSection kpis={kpis} activity={activity} />
          {/* Tokens — one chart, toggled between token-type and model
              breakdown (both sum to the same per-day total). "Tokens"
              title on the left, the type/model toggle on the right. */}
          <div className="rounded-lg bg-muted p-4 flex flex-col gap-3">
            <div className="flex items-center justify-between">
              <div className="text-xs text-muted-foreground">
                {t("stats.tokensChart")}
              </div>
              <Tabs
                value={groupBy}
                onValueChange={(v) => setGroupBy(v as TokensGroupBy)}
              >
                <TabsList aria-label={t("stats.tokensChart")}>
                  <TabsTrigger value="type">
                    {t("stats.groupByType")}
                  </TabsTrigger>
                  <TabsTrigger value="model">
                    {t("stats.groupByModel")}
                  </TabsTrigger>
                </TabsList>
              </Tabs>
            </div>
            <TokensChart
              groupBy={groupBy}
              daily={tokensDaily}
              modelData={modelDaily}
              globalModelRank={globalModelRank}
            />
          </div>
        </div>
      </div>
    </div>
  );
}
