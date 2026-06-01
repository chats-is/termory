// Constants and small atoms shared between Stats sub-components.

import { formatCompact } from "@/lib/format";

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
  return parsed.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric"
  });
}
