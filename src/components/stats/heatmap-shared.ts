// Intensity + tier colors for the Overview calendar heatmap (the
// full-history, GitHub-style day grid in OverviewSection). Kept in its
// own module so the intensity rule and tier ramp are unit-testable apart
// from the component.

/** Messages weigh 60%, tokens 40% in the combined intensity. */
export const MSG_WEIGHT = 0.6;

/** Empty-cell grey — no-activity days AND leading/trailing padding, so
 * a grid reads as one complete rectangle. */
export const EMPTY_CELL = "bg-foreground/[0.08]";

/** Ratio (0..1) → Tailwind tier class. */
export function tierClass(ratio: number): string {
  if (ratio <= 0) return EMPTY_CELL;
  if (ratio < 0.08) return "bg-primary/25";
  if (ratio < 0.18) return "bg-primary/40";
  if (ratio < 0.35) return "bg-primary/55";
  if (ratio < 0.55) return "bg-primary/70";
  if (ratio < 0.75) return "bg-primary/85";
  return "bg-primary";
}

/** Weighted-geometric-mean intensity (messages^0.6 · tokens^0.4),
 * normalized to the cell-set maxima, with single-dimension degradation
 * (no tokens anywhere → messages-only; no messages → tokens-only). */
export function intensity(
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
