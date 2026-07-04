import React from "react";
import { Layers, RefreshCw } from "lucide-react";
import { BrandIcon } from "@/components/BrandIcon";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { CLI_APP_LABEL, CLI_APP_SOURCE_BADGE } from "@/constants";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type { AppSession, CliApp } from "@/types";
import {
  type DateRange,
  type DateRangePreset,
  type SourceFilter,
  dailyModelTokens,
  dailyTokens,
  filterSessions,
  modelBreakdown,
  overviewKpis,
  resolveRange,
  windowTotals,
} from "@/lib/stats-utils";
import { OverviewTab } from "./OverviewTab";
import { TrendsTab } from "./TrendsTab";
import { ModelsTab } from "./ModelsTab";

type StatsView = "overview" | "trends" | "models";

const VIEWS: { id: StatsView; labelKey: MessageKey }[] = [
  { id: "overview", labelKey: "stats.tab.overview" },
  { id: "trends", labelKey: "stats.tab.trends" },
  { id: "models", labelKey: "stats.tab.models" },
];

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
];

/**
 * Stats dashboard — two views over the same filtered window:
 *   Overview — 8 KPI cards + calendar heatmap
 *   Trends   — per-day token trend lines (Input/Output/Cached/Reasoning)
 *   Models   — per-day stacked token bars split by model + legend
 * Shared controls: view tabs, source filter, All/30d/7d range, refresh.
 */
export function StatsPage({
  sessions,
  onRefresh,
  refreshing,
}: {
  sessions: AppSession[];
  onRefresh: () => void;
  refreshing: boolean;
}) {
  const t = useT();
  const [view, setView] = React.useState<StatsView>("overview");
  const [range, setRange] = React.useState<DateRange>({ preset: "30d" });
  const [source, setSource] = React.useState<SourceFilter>("All");

  const resolved = React.useMemo(
    () => resolveRange(range, sessions),
    [range, sessions],
  );
  const filtered = React.useMemo(
    () => filterSessions(sessions, resolved, source),
    [sessions, resolved, source],
  );
  const totals = React.useMemo(
    () => windowTotals(filtered, resolved),
    [filtered, resolved],
  );
  const models = React.useMemo(
    () => modelBreakdown(filtered, resolved),
    [filtered, resolved],
  );
  // The calendar heatmap is a GitHub-style all-history view: ALWAYS the
  // earliest activity day → today, independent of the All/30d/7d range
  // (which only scopes the KPIs and the Models chart). It still follows
  // the source filter. Source-filter the sessions, then resolve the
  // full "all" window over that subset for the day grid.
  const sourceFiltered = React.useMemo(
    () =>
      source === "All"
        ? sessions
        : sessions.filter((s) => s.source.toLowerCase() === source),
    [sessions, source],
  );
  const heatmapDaily = React.useMemo(() => {
    const allRange = resolveRange({ preset: "all" }, sourceFiltered);
    // dailyTokens already skips non-session items, so no extra filter.
    return dailyTokens(sourceFiltered, allRange);
  }, [sourceFiltered]);
  const kpis = React.useMemo(
    () => overviewKpis(filtered, resolved),
    [filtered, resolved],
  );
  const modelDaily = React.useMemo(
    () => dailyModelTokens(filtered, resolved),
    [filtered, resolved],
  );
  // Windowed per-day tokens for the Trends chart (follows the range).
  const windowDaily = React.useMemo(
    () => dailyTokens(filtered, resolved),
    [filtered, resolved],
  );

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      {/* Fixed header (outside the scroll area, like the Providers page).
          Two SEPARATE cards: source filter + refresh, then view/range. */}
      <div className="px-3 mt-3 mb-3 flex flex-col gap-3">
        <div className="rounded-md bg-muted p-3">
          {/* Card 1: source filter (fills) + refresh. */}
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
                {SOURCES.map((s) => (
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
            <button
              type="button"
              onClick={onRefresh}
              disabled={refreshing}
              aria-label={t("stats.refresh")}
              className={cn(
                "h-9 w-9 shrink-0 rounded-md border bg-card shadow-sm inline-flex items-center justify-center",
                "hover:bg-accent hover:text-accent-foreground transition-colors",
                refreshing && "opacity-60",
              )}
            >
              <RefreshCw
                className={cn("size-4", refreshing && "animate-spin")}
              />
            </button>
          </div>
        </div>
        {/* Row 2 (no card): view tabs (left) + range pills (right). */}
        <div className="flex items-center gap-2 flex-wrap">
          <Tabs value={view} onValueChange={(v) => setView(v as StatsView)}>
            <TabsList>
              {VIEWS.map((v) => (
                <TabsTrigger key={v.id} value={v.id}>
                  {t(v.labelKey)}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
          <div className="flex-1" />
          <Tabs
            value={range.preset}
            onValueChange={(v) => setRange({ preset: v as DateRangePreset })}
          >
            <TabsList aria-label={t("stats.range.label")}>
              {RANGES.map((r) => (
                <TabsTrigger key={r.id} value={r.id}>
                  {t(r.labelKey)}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        </div>
      </div>
      <div className="flex-1 min-h-0 overflow-auto px-3 pb-3">
        {view === "overview" ? (
          <OverviewTab
            totals={totals}
            kpis={kpis}
            favoriteModel={
              models.find((m) => m.model !== "Unknown")?.model ?? null
            }
            daily={heatmapDaily}
            sessions={sourceFiltered}
          />
        ) : view === "trends" ? (
          <TrendsTab data={windowDaily} />
        ) : (
          <ModelsTab data={modelDaily} usage={models} />
        )}
      </div>
    </div>
  );
}
