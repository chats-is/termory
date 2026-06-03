import React from "react";
import { StatsFilterBar } from "./StatsFilterBar";
import { OverviewHero } from "./OverviewHero";
import { DailyTokensChart } from "./DailyTokensChart";
import { DailyActivitiesHeatmap } from "./DailyActivitiesHeatmap";
import type { AppSession } from "@/types";
import {
  type DateRange,
  type SourceFilter,
  dailyActivities,
  dailyTokens,
  filterSessions,
  resolveRange,
  windowTotals
} from "@/lib/stats-utils";

/**
 * Stats dashboard:
 *   1. StatsFilterBar    — date range + source
 *   2. OverviewHero      — KPI strip (Sessions / Messages / Tokens / Projects)
 *   3. DailyTokensChart — per-day token breakdown (line chart)
 *   4. DailyActivitiesHeatmap — 24-hour × N-date heatmap
 */
export function StatsPage({
  sessions,
  onRefresh,
  refreshing
}: {
  sessions: AppSession[];
  onRefresh: () => void;
  refreshing: boolean;
}) {
  const [range, setRange] = React.useState<DateRange>({ preset: "30d" });
  const [source, setSource] = React.useState<SourceFilter>("All");

  const resolved = React.useMemo(() => resolveRange(range), [range]);

  const filtered = React.useMemo(
    () => filterSessions(sessions, resolved, source),
    [sessions, resolved, source]
  );

  const totals = React.useMemo(
    () => windowTotals(filtered, resolved),
    [filtered, resolved]
  );

  const tokenData = React.useMemo(
    () => dailyTokens(filtered, resolved),
    [filtered, resolved]
  );
  const activities = React.useMemo(
    () => dailyActivities(filtered, resolved),
    [filtered, resolved]
  );

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      {/* Fixed header (sibling, outside the scroll area) — the filter bar
          stays put while the cards scroll, matching the Providers page. */}
      <div className="px-3 mt-3 mb-3">
        <StatsFilterBar
          range={range}
          onRangeChange={setRange}
          source={source}
          onSourceChange={setSource}
          refreshing={refreshing}
          onRefresh={onRefresh}
        />
      </div>
      <div className="flex-1 min-h-0 overflow-auto px-3 pb-1">
        <div className="flex flex-col gap-3">
          <OverviewHero
            sessions={totals.sessions}
            messages={totals.messages}
            tokens={totals.tokens}
            projects={totals.projects}
          />
          <DailyTokensChart data={tokenData} />
          <DailyActivitiesHeatmap activities={activities} totals={totals} />
        </div>
      </div>
    </div>
  );
}
