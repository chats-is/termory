import React from "react";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger
} from "@/components/ui/hover-card";
import { formatCompact, formatFullNumber, getFormatLocale } from "@/lib/format";
import { useT, type MessageKey } from "@/i18n";
import { type DailyActivity, type OverviewKpis } from "@/lib/stats-utils";
import { EMPTY_CELL, intensity, tierClass } from "./heatmap-shared";

/**
 * Reshape the flat per-day activity list into GitHub-style week columns
 * (each column one week, rows Sun→Sat), padding the first/last columns
 * with null so every column is exactly 7 cells. This is a rendering
 * layout for the heatmap — not a statistic — so it lives with the
 * component, not in stats-utils.
 */
function calendarWeeks(
  activity: DailyActivity[]
): (DailyActivity | null)[][] {
  if (activity.length === 0) return [];
  // Parse the first date key as LOCAL (new Date("YYYY-MM-DD") is UTC).
  const [y, m, d] = activity[0].date.split("-").map(Number);
  const firstWeekday = new Date(y, m - 1, d).getDay();
  const padded: (DailyActivity | null)[] = [
    ...Array.from({ length: firstWeekday }, () => null),
    ...activity
  ];
  while (padded.length % 7 !== 0) padded.push(null);
  const weeks: (DailyActivity | null)[][] = [];
  for (let i = 0; i < padded.length; i += 7) weeks.push(padded.slice(i, i + 7));
  return weeks;
}

function KpiCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg bg-muted px-4 py-3 min-w-0">
      <div className="text-sm text-muted-foreground truncate">{label}</div>
      <div className="text-lg font-semibold truncate">{value}</div>
    </div>
  );
}

const parseDateKey = (key: string): Date => {
  const [y, m, d] = key.split("-").map(Number);
  return new Date(y, m - 1, d);
};

function CalendarHeatmap({ activity }: { activity: DailyActivity[] }) {
  const t = useT();
  const weeks = React.useMemo(() => calendarWeeks(activity), [activity]);
  const { maxMsg, maxTok } = React.useMemo(() => {
    let maxMsg = 0;
    let maxTok = 0;
    for (const d of activity) {
      if (d.messages > maxMsg) maxMsg = d.messages;
      if (d.tokens > maxTok) maxTok = d.tokens;
    }
    return { maxMsg, maxTok };
  }, [activity]);
  const dateFmt = React.useMemo(
    () =>
      new Intl.DateTimeFormat(getFormatLocale(), {
        month: "short",
        day: "numeric",
        weekday: "short"
      }),
    []
  );
  const cellDate = parseDateKey;
  // Localized weekday labels (rows are Sun→Sat). All seven are shown.
  const weekdayLabels = React.useMemo(() => {
    const fmt = new Intl.DateTimeFormat(getFormatLocale(), {
      weekday: "short"
    });
    // 2024-01-07 is a Sunday.
    return [0, 1, 2, 3, 4, 5, 6].map((dow) =>
      fmt.format(new Date(2024, 0, 7 + dow))
    );
  }, []);
  // Month label per week column — shown on the first column of each new
  // month (like a GitHub contribution graph's top axis).
  const monthLabels = React.useMemo(() => {
    const fmt = new Intl.DateTimeFormat(getFormatLocale(), { month: "short" });
    let prev = -1;
    return weeks.map((week) => {
      const first = week.find((c) => c !== null);
      if (!first) return "";
      const d = cellDate(first.date);
      if (d.getMonth() !== prev) {
        prev = d.getMonth();
        return fmt.format(d);
      }
      return "";
    });
  }, [weeks]);
  const ratioOf = (messages: number, tokens: number): number =>
    intensity(messages, tokens, maxMsg, maxTok);
  return (
    <div className="flex gap-1 text-[10px] text-muted-foreground">
      {/* Left weekday axis — all seven days. The label rows are `flex-1`
          so they split the same stretched height as the cell rows and
          stay aligned regardless of the responsive (aspect-square) cell
          size. */}
      <div className="flex flex-col gap-[3px] shrink-0 self-stretch">
        <span className="h-3" aria-hidden /> {/* aligns with month row */}
        {weekdayLabels.map((label, i) => (
          <span
            key={i}
            className="flex-1 flex items-center justify-start leading-none text-left whitespace-nowrap"
          >
            {label}
          </span>
        ))}
      </div>
      {/* Fixed 52-week grid, 100% width — columns stretch to fill the row.
          Padding cells share the empty-day grey so the grid is one
          complete rectangle. */}
      <div className="flex-1 min-w-0">
        <div className="flex flex-col gap-[3px]">
          {/* Top month axis, aligned to week columns. */}
          <div className="flex gap-[3px]">
            {monthLabels.map((label, wi) => (
              <span
                key={wi}
                className="flex-1 min-w-0 h-3 leading-3 whitespace-nowrap"
              >
                {label}
              </span>
            ))}
          </div>
          <div className="flex gap-[3px]">
            {weeks.map((week, wi) => (
              <div key={wi} className="flex-1 min-w-0 flex flex-col gap-[3px]">
                {week.map((cell, di) =>
                  cell === null ? (
                    <span
                      key={di}
                      className={`w-full aspect-square rounded-[2px] ${EMPTY_CELL}`}
                    />
                  ) : (
                    <HoverCard key={di} openDelay={50} closeDelay={80}>
                      <HoverCardTrigger asChild>
                        <span
                          className={`w-full aspect-square rounded-[2px] ${tierClass(
                            cell.messages === 0 && cell.tokens === 0
                              ? 0
                              : Math.max(
                                  ratioOf(cell.messages, cell.tokens),
                                  0.001
                                )
                          )}`}
                        />
                      </HoverCardTrigger>
                      {/* HoverCard's default is a light popover with border +
                          shadow and NO arrow — same look as the bar-chart
                          recharts tooltips, no restyling needed. */}
                      <HoverCardContent side="top" className="w-fit px-3 py-2 text-xs">
                        <div className="font-medium mb-1">
                          {dateFmt.format(cellDate(cell.date))}
                        </div>
                        <div className="flex items-center gap-2">
                          <span className="text-muted-foreground">
                            {t("stats.kpi.messages")}
                          </span>
                          <span className="ml-auto pl-3 font-mono tabular-nums">
                            {formatFullNumber(cell.messages)}
                          </span>
                        </div>
                        <div className="flex items-center gap-2">
                          <span className="text-muted-foreground">Tokens</span>
                          <span className="ml-auto pl-3 font-mono tabular-nums">
                            {formatCompact(cell.tokens)}
                          </span>
                        </div>
                      </HoverCardContent>
                    </HoverCard>
                  )
                )}
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}

export function OverviewSection({
  kpis,
  activity
}: {
  kpis: OverviewKpis;
  activity: DailyActivity[];
}) {
  const t = useT();
  const hourFmt = React.useMemo(
    () => new Intl.DateTimeFormat(getFormatLocale(), { hour: "numeric" }),
    []
  );
  const peakHour =
    kpis.peakHour === null
      ? "—"
      : hourFmt.format(new Date(2000, 0, 1, kpis.peakHour));

  const cards: { labelKey: MessageKey; value: string }[] = [
    { labelKey: "stats.kpi.sessions", value: formatFullNumber(kpis.sessions) },
    { labelKey: "stats.kpi.messages", value: formatFullNumber(kpis.messages) },
    { labelKey: "stats.kpi.totalTokens", value: formatCompact(kpis.tokens.total) },
    { labelKey: "stats.kpi.activeDays", value: formatFullNumber(kpis.activeDays) },
    {
      labelKey: "stats.kpi.currentStreak",
      value: t("stats.streakDays", { n: kpis.currentStreak })
    },
    {
      labelKey: "stats.kpi.longestStreak",
      value: t("stats.streakDays", { n: kpis.longestStreak })
    },
    { labelKey: "stats.kpi.peakHour", value: peakHour },
    {
      labelKey: "stats.kpi.favoriteModel",
      value: kpis.favoriteModel ?? "—"
    }
  ];

  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
        {cards.map((c) => (
          <KpiCard key={c.labelKey} label={t(c.labelKey)} value={c.value} />
        ))}
      </div>
      {/* Full-history calendar heatmap (per day). */}
      <div className="rounded-lg bg-muted p-4 flex flex-col gap-2">
        <div className="text-xs text-muted-foreground">
          {t("stats.activity")}
        </div>
        <CalendarHeatmap activity={activity} />
      </div>
    </div>
  );
}
