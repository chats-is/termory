import React from "react";
import { ChevronDown, ChevronUp, Search, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { Input } from "@/components/ui/input";
import { INPUT_NO_AUTO } from "@/lib/utils";
import { useT } from "@/i18n";

/** In-session find bar for the Records detail pane. Purely presentational —
 * the match list, current position, and open/close state live in App.tsx
 * (they have to coordinate with `pendingScroll` and the ⌘F shortcut).
 *
 * Renders as a full-width row between the detail header and the
 * message list (user decision — in-flow, 100% width, no floating
 * overlay), styled after the app's OWN search-input pattern (the
 * SearchPage `rounded-md bg-muted` pill with a left icon and a
 * borderless input — SearchPage.tsx:51): input growing left, counter
 * + nav/close inside the pill on the right.
 *
 * Keyboard: Enter → next match, Shift+Enter → previous, Esc → close.
 * `focusNonce` re-focuses + selects the input when it changes, so a ⌘F
 * while the bar is already open behaves like the browser's find (jump
 * back to the input with the query selected). */
export function TranscriptFindBar({
  query,
  onQueryChange,
  position,
  total,
  onNext,
  onPrev,
  onClose,
  focusNonce
}: {
  query: string;
  onQueryChange: (query: string) => void;
  /** 0-based position of the current match within the match list. */
  position: number;
  total: number;
  onNext: () => void;
  onPrev: () => void;
  onClose: () => void;
  focusNonce: number;
}) {
  const t = useT();
  // shadcn Input is a plain function component (React 18 — no forwardRef),
  // so focus goes through the wrapping element instead of an input ref.
  const wrapRef = React.useRef<HTMLDivElement>(null);
  React.useEffect(() => {
    const input = wrapRef.current?.querySelector("input");
    input?.focus();
    input?.select();
  }, [focusNonce]);

  const hasQuery = query.trim().length > 0;
  return (
    <div ref={wrapRef} className="px-4 pt-2 pb-2 shrink-0">
      <div className="relative flex items-center rounded-md bg-muted">
        <Search className="absolute left-3 size-4 text-muted-foreground pointer-events-none" />
        <Input
          {...INPUT_NO_AUTO}
          value={query}
          onChange={(event) => onQueryChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              if (event.shiftKey) onPrev();
              else onNext();
            } else if (event.key === "Escape") {
              event.preventDefault();
              onClose();
            }
          }}
          placeholder={t("records.findPlaceholder")}
          aria-label={t("records.findPlaceholder")}
          className="h-9 flex-1 pl-9 pr-2 border-0 bg-transparent shadow-none focus-visible:ring-0 dark:bg-transparent"
        />
        {hasQuery && (
          <span className="text-xs text-muted-foreground tabular-nums whitespace-nowrap px-1.5 shrink-0">
            {total > 0
              ? `${position + 1}/${total}`
              : t("records.findNoMatches")}
          </span>
        )}
        <div className="flex items-center gap-0.5 pr-1.5 shrink-0">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon-xs"
                onClick={onPrev}
                disabled={total === 0}
                aria-label={t("records.findPrev")}
              >
                <ChevronUp aria-hidden />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="top">{t("records.findPrev")}</TooltipContent>
          </Tooltip>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon-xs"
                onClick={onNext}
                disabled={total === 0}
                aria-label={t("records.findNext")}
              >
                <ChevronDown aria-hidden />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="top">{t("records.findNext")}</TooltipContent>
          </Tooltip>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon-xs"
                onClick={onClose}
                aria-label={t("records.findClose")}
              >
                <X aria-hidden />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="top">{t("records.findClose")}</TooltipContent>
          </Tooltip>
        </div>
      </div>
    </div>
  );
}
