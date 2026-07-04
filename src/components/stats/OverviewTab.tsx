import React from "react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { formatCompact, formatFullNumber, getFormatLocale } from "@/lib/format";
import { useT, type MessageKey } from "@/i18n";
import type { AppSession } from "@/types";
import {
  type DailyTokens,
  type OverviewKpis,
  type WindowTotals,
  calendarWeeks,
  displayModelName,
  hourlyActivity,
  localDateKey
} from "@/lib/stats-utils";

function KpiCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg bg-muted px-4 py-3 min-w-0">
      <div className="text-sm text-muted-foreground truncate">{label}</div>
      <div className="text-lg font-semibold truncate">{value}</div>
    </div>
  );
}

/**
 * Calendar heatmap intensity — the SAME weighted-geometric-mean rule
 * as the old 24h heatmap (messages^0.6 × tokens^0.4, see the LOCKED
 * rationale in CLAUDE.md), applied at day granularity. Single-dim
 * degradation preserved: no tokens anywhere → messages-only ratio and
 * vice versa.
 */
const MSG_WEIGHT = 0.6;

/** The empty-cell grey — used for both no-activity days AND the leading
 * / trailing week padding, so the grid reads as one full rectangle of
 * placeholders (matching the reference design) instead of a ragged
 * shape with transparent corners. */
const EMPTY_CELL = "bg-foreground/[0.08]";

function tierClass(ratio: number): string {
  if (ratio <= 0) return EMPTY_CELL;
  if (ratio < 0.08) return "bg-primary/25";
  if (ratio < 0.18) return "bg-primary/40";
  if (ratio < 0.35) return "bg-primary/55";
  if (ratio < 0.55) return "bg-primary/70";
  if (ratio < 0.75) return "bg-primary/85";
  return "bg-primary";
}

/** Weighted-geometric-mean intensity (messages 60% / tokens 40%),
 * normalized to the cell-set maxima, with single-dimension degradation
 * — shared by the calendar (per-day) and the hourly heatmap. */
function intensity(
  messages: number,
  tokens: number,
  maxMsg: number,
  maxTok: number
): number {
  if (maxMsg === 0 && maxTok === 0) return 0;
  const m = maxMsg > 0 ? messages / maxMsg : 0;
  const tk = maxTok > 0 ? tokens / maxTok : 0;
  if (maxTok === 0) return m;
  if (maxMsg === 0) return tk;
  return Math.pow(m, MSG_WEIGHT) * Math.pow(tk, 1 - MSG_WEIGHT);
}

const parseDateKey = (key: string): Date => {
  const [y, m, d] = key.split("-").map(Number);
  return new Date(y, m - 1, d);
};

/**
 * 24-hour activity heatmap for ONE day (the calendar's selected day, or
 * today by default) — same weighted-geometric-mean intensity + tier
 * colors as the calendar, laid out as a 4-row × 6-column grid (each row
 * a 6-hour band, left axis 00/06/12/18). Hovering a cell shows that
 * hour's messages / tokens.
 */
function HourlyHeatmap({
  sessions,
  dateKey
}: {
  sessions: AppSession[];
  dateKey: string;
}) {
  const t = useT();
  const { messages, tokens } = React.useMemo(
    () => hourlyActivity(sessions, dateKey),
    [sessions, dateKey]
  );
  const { maxMsg, maxTok } = React.useMemo(() => {
    let maxMsg = 0;
    let maxTok = 0;
    for (let h = 0; h < 24; h++) {
      if (messages[h] > maxMsg) maxMsg = messages[h];
      if (tokens[h] > maxTok) maxTok = tokens[h];
    }
    return { maxMsg, maxTok };
  }, [messages, tokens]);
  const hourFmt = React.useMemo(
    () => new Intl.DateTimeFormat(getFormatLocale(), { hour: "numeric" }),
    []
  );
  const dayFmt = React.useMemo(
    () =>
      new Intl.DateTimeFormat(getFormatLocale(), {
        weekday: "short",
        month: "short",
        day: "numeric"
      }),
    []
  );
  const rows = [0, 1, 2, 3]; // each row = a 6-hour band
  const cols = [0, 1, 2, 3, 4, 5];
  return (
    <div className="flex flex-col gap-1.5 shrink-0">
      <div className="text-xs font-medium">
        {dayFmt.format(parseDateKey(dateKey))}
      </div>
      <div className="flex gap-1 text-[10px] text-muted-foreground">
        {/* Left hour-band axis. */}
        <div className="flex flex-col gap-1 shrink-0">
          {rows.map((r) => (
            <span key={r} className="h-4 leading-4 text-right tabular-nums">
              {String(r * 6).padStart(2, "0")}
            </span>
          ))}
        </div>
        <div className="flex flex-col gap-1">
          {rows.map((r) => (
            <div key={r} className="flex gap-1">
              {cols.map((c) => {
                const hour = r * 6 + c;
                const m = messages[hour];
                const tk = tokens[hour];
                const ratio =
                  m === 0 && tk === 0
                    ? 0
                    : Math.max(intensity(m, tk, maxMsg, maxTok), 0.001);
                return (
                  <Tooltip key={c}>
                    <TooltipTrigger asChild>
                      <span
                        className={`size-4 rounded-[4px] ${tierClass(ratio)}`}
                      />
                    </TooltipTrigger>
                    <TooltipContent>
                      <div className="flex flex-col gap-0.5">
                        <span className="font-medium">
                          {hourFmt.format(new Date(2000, 0, 1, hour))}
                        </span>
                        <span>
                          {t("stats.summaryMessages", {
                            n: formatFullNumber(m)
                          })}
                          {" · "}
                          {formatCompact(tk)} Tokens
                        </span>
                      </div>
                    </TooltipContent>
                  </Tooltip>
                );
              })}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function CalendarHeatmap({
  daily,
  selected,
  onSelect
}: {
  daily: DailyTokens[];
  selected: string;
  onSelect: (date: string) => void;
}) {
  const t = useT();
  const weeks = React.useMemo(() => calendarWeeks(daily), [daily]);
  const { maxMsg, maxTok } = React.useMemo(() => {
    let maxMsg = 0;
    let maxTok = 0;
    for (const d of daily) {
      if (d.messages > maxMsg) maxMsg = d.messages;
      if (d.total > maxTok) maxTok = d.total;
    }
    return { maxMsg, maxTok };
  }, [daily]);
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
  // Localized weekday labels (rows are Sun→Sat). GitHub shows only
  // Mon/Wed/Fri to avoid crowding the small cells.
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
  // On overflow (long "All" windows) start scrolled to the newest week.
  const scrollRef = React.useRef<HTMLDivElement>(null);
  React.useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollLeft = el.scrollWidth;
  }, [weeks]);
  return (
    <div className="flex gap-1 text-[10px] text-muted-foreground">
      {/* Left weekday axis (fixed) — Mon/Wed/Fri, GitHub style. */}
      <div className="flex flex-col gap-1 shrink-0">
        <span className="h-4" aria-hidden /> {/* aligns with month row */}
        {weekdayLabels.map((label, i) => (
          <span
            key={i}
            className="h-4 leading-4 text-right whitespace-nowrap"
          >
            {i % 2 === 1 ? label : ""}
          </span>
        ))}
      </div>
      {/* Scrollable weeks — left→right oldest→newest (GitHub style).
          Padding cells share the empty-day grey so the grid is one
          complete rectangle. */}
      <div ref={scrollRef} className="overflow-x-auto">
        <div className="flex flex-col gap-1 w-max">
          {/* Top month axis, aligned to week columns. */}
          <div className="flex gap-1">
            {monthLabels.map((label, wi) => (
              <span key={wi} className="w-4 h-4 leading-4 whitespace-nowrap">
                {label}
              </span>
            ))}
          </div>
          <div className="flex gap-1">
            {weeks.map((week, wi) => (
              <div key={wi} className="flex flex-col gap-1">
                {week.map((cell, di) =>
                  cell === null ? (
                    <span
                      key={di}
                      className={`size-4 rounded-[4px] ${EMPTY_CELL}`}
                    />
                  ) : (
                    <Tooltip key={di}>
                      <TooltipTrigger asChild>
                        <button
                          type="button"
                          onClick={() => onSelect(cell.date)}
                          aria-label={dateFmt.format(cellDate(cell.date))}
                          className={`size-4 rounded-[4px] ${tierClass(
                            cell.messages === 0 && cell.tokens === 0
                              ? 0
                              : Math.max(
                                  ratioOf(cell.messages, cell.tokens),
                                  0.001
                                )
                          )} ${
                            cell.date === selected
                              ? "ring-2 ring-primary ring-offset-1 ring-offset-background"
                              : ""
                          }`}
                        />
                      </TooltipTrigger>
                      <TooltipContent>
                        <div className="flex flex-col gap-0.5">
                          <span className="font-medium">
                            {dateFmt.format(cellDate(cell.date))}
                          </span>
                          <span>
                            {t("stats.summaryMessages", {
                              n: formatFullNumber(cell.messages)
                            })}
                            {" · "}
                            {formatCompact(cell.tokens)} Tokens
                          </span>
                        </div>
                      </TooltipContent>
                    </Tooltip>
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

export function OverviewTab({
  totals,
  kpis,
  favoriteModel,
  daily,
  sessions
}: {
  totals: WindowTotals;
  kpis: OverviewKpis;
  favoriteModel: string | null;
  daily: DailyTokens[];
  /** Source-filtered sessions (all history) — for the hourly view. */
  sessions: AppSession[];
}) {
  const t = useT();
  // The calendar's selected day drives the hourly heatmap; defaults to
  // today.
  const [selectedDate, setSelectedDate] = React.useState(() =>
    localDateKey(new Date())
  );
  const hourFmt = React.useMemo(
    () => new Intl.DateTimeFormat(getFormatLocale(), { hour: "numeric" }),
    []
  );
  const peakHour =
    kpis.peakHour === null
      ? "—"
      : hourFmt.format(new Date(2000, 0, 1, kpis.peakHour));

  const cards: { labelKey: MessageKey; value: string }[] = [
    { labelKey: "stats.kpi.sessions", value: formatFullNumber(totals.sessions) },
    { labelKey: "stats.kpi.messages", value: formatFullNumber(totals.messages) },
    { labelKey: "stats.kpi.totalTokens", value: formatCompact(totals.tokens.total) },
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
      value: favoriteModel ? displayModelName(favoriteModel) : "—"
    }
  ];

  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
        {cards.map((c) => (
          <KpiCard key={c.labelKey} label={t(c.labelKey)} value={c.value} />
        ))}
      </div>
      {/* Left: 24-hour activity for the selected day. Right: full-history
          calendar — clicking a day drives the hourly view. */}
      <div className="flex gap-6 items-start flex-wrap">
        <HourlyHeatmap sessions={sessions} dateKey={selectedDate} />
        <CalendarHeatmap
          daily={daily}
          selected={selectedDate}
          onSelect={setSelectedDate}
        />
      </div>
    </div>
  );
}
