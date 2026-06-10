// Constants and small atoms shared between Stats sub-components.

import { formatCompact, getFormatLocale } from "@/lib/format";
import { useT } from "@/i18n";

/** Series colors — used in both the DailyTokensChart lines/tooltip
 * and the OverviewHero Tokens hover-card breakdown. Single source of
 * truth so the two surfaces never drift. */
export const TOKEN_COLORS = {
  input: "#3b82f6", // blue-500
  output: "#10b981", // emerald-500
  cached: "#f59e0b", // amber-500
  reasoning: "#a855f7" // purple-500
} as const;

/** One labeled value row used inside hover-cards / tooltips. Renders
 * `─ Label    value` with a colored leader bar. Zero values become an
 * em-dash so the row layout stays fixed. */
export function BreakdownRow({
  color,
  label,
  value
}: {
  color: string;
  label: string;
  value: number;
}) {
  return (
    <div className="flex items-center gap-2">
      <span
        aria-hidden
        className="inline-block w-3 h-[2px] rounded-full shrink-0"
        style={{ background: color }}
      />
      <span className="text-muted-foreground w-20">{label}</span>
      <span>{value === 0 ? "—" : formatCompact(value)}</span>
    </div>
  );
}

/** Per-model rows for a hover card: `model    {tokens}`, sorted as
 * given (callers pass `modelBreakdown` output, already tokens-desc).
 * Shared by the OverviewHero Models cell and the DailyActivities
 * summary so the two surfaces never drift. Session counts are
 * intentionally NOT shown here — per-model session counting doesn't
 * reconcile with the window's "sessions created" headline (sessions
 * with no recorded model would have to surface as an "Unknown" row), so
 * the breakdown is a pure token-usage view. Accepts any row with a
 * `model` + `tokens` (window-level `ModelUsage` or per-cell
 * `ModelCellUsage`). */
export function ModelUsageList({
  models
}: {
  models: { model: string; tokens: number }[];
}) {
  const t = useT();
  return (
    <>
      <div className="flex items-center justify-between gap-6 text-[10px] uppercase tracking-wide text-muted-foreground mb-1">
        <span>{t("stats.col.model")}</span>
        <span>{t("stats.col.tokens")}</span>
      </div>
      <div className="space-y-0.5 tabular-nums">
        {models.map((m) => (
          <div
            key={m.model}
            className="flex items-center justify-between gap-6"
          >
            <span className="truncate max-w-[200px]" title={m.model}>
              {m.model}
            </span>
            <span className="shrink-0 font-medium">
              {m.tokens === 0 ? "—" : formatCompact(m.tokens)}
            </span>
          </div>
        ))}
      </div>
    </>
  );
}

/** Compact `M/D` date label for chart x-axis ticks. */
export function formatDateShort(date: unknown): string {
  const str = typeof date === "string" ? date : String(date ?? "");
  const parsed = new Date(`${str}T00:00:00`);
  if (Number.isNaN(parsed.getTime())) return str;
  return `${parsed.getMonth() + 1}/${parsed.getDate()}`;
}

/** `Mon, Jan 5` date label for tooltip / hover headers. */
export function formatDateLong(date: string): string {
  const parsed = new Date(`${date}T00:00:00`);
  if (Number.isNaN(parsed.getTime())) return date;
  return parsed.toLocaleDateString(getFormatLocale(), {
    weekday: "short",
    month: "short",
    day: "numeric"
  });
}
