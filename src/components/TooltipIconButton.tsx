import React from "react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

/**
 * An icon-only shadcn `Button` with a tooltip.
 *
 * Why the `<span>` wrapper: the shadcn `Button` is a plain function
 * component with no `forwardRef` (it targets React 19's ref-as-prop),
 * but this app runs React 18 — so Radix's `TooltipTrigger asChild` ref
 * can't reach the Button directly and the tooltip never anchors/shows.
 * Wrapping in a real DOM element (the span) gives Radix a ref target.
 * A disabled Button still surfaces the tooltip because the span (not the
 * button) receives the hover.
 *
 * Raw `<button>` trigger sites (eye toggles, etc.) forward their ref
 * natively, so they keep their own inline `<Tooltip>` and do NOT use this.
 *
 * All `Button` props pass through. `tooltip` is the tooltip content and,
 * when it's a string, the default `aria-label` (override with `aria-label`).
 */
export function TooltipIconButton({
  tooltip,
  side = "top",
  wrapperClassName,
  children,
  "aria-label": ariaLabel,
  ...props
}: React.ComponentProps<typeof Button> & {
  tooltip: React.ReactNode;
  side?: React.ComponentProps<typeof TooltipContent>["side"];
  /** Extra classes for the ref-target span (e.g. absolute positioning). */
  wrapperClassName?: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className={cn("inline-flex", wrapperClassName)}>
          <Button
            aria-label={
              ariaLabel ?? (typeof tooltip === "string" ? tooltip : undefined)
            }
            {...props}
          >
            {children}
          </Button>
        </span>
      </TooltipTrigger>
      <TooltipContent side={side}>{tooltip}</TooltipContent>
    </Tooltip>
  );
}
