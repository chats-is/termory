import React from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Star } from "lucide-react";
import { MessageBody } from "@/components/MessageBody";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { favoriteKey } from "@/lib/favorites";
import { useT } from "@/i18n";
import type { AppSession, SessionMessage } from "@/types";
import { roleClass } from "@/lib/session-utils";

/** External scroll-to-message request. The `nonce` differentiates
 * back-to-back requests for the same index so a second click on the
 * same favorite (after the user scrolled away) still re-scrolls.
 * The parent owns this state and shouldn't try to clear it — the
 * effect inside MessageList is idempotent per nonce. */
export type ScrollRequest = { index: number; nonce: number };

/** All the wiring needed to render the per-message star button.
 * Bundled into one optional prop so a caller that doesn't want the
 * favorites affordance only has to omit one field (instead of
 * remembering to leave three blank). */
export type FavoriteContext = {
  session: AppSession;
  keys: Set<string>;
  onToggle: (message: SessionMessage, index: number) => void;
};

export function MessageList({
  messages,
  favorites,
  scrollRequest,
  onScrolled
}: {
  messages: SessionMessage[];
  favorites?: FavoriteContext;
  scrollRequest?: ScrollRequest | null;
  /** Called once a scrollRequest has been consumed (either dispatched
   * to the virtualizer or dropped because the index is out of range).
   * Lets the parent clear its pending state so the same request
   * doesn't re-fire when the user toggles selection away and back. */
  onScrolled?: () => void;
}) {
  const t = useT();
  const parentRef = React.useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: messages.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 140,
    overscan: 6,
    measureElement: (element) => element.getBoundingClientRect().height
  });

  // External scroll-to-index — used when a Favorite's "Open original"
  // navigates here. Waits for the next animation frame so the
  // virtualizer has measured at least the first viewport before
  // scrolling. `messages.length` is in deps so a re-scroll after
  // detail finishes loading still lands. `onScrolled` always fires
  // once per consumed nonce so the parent can null its pending state
  // — otherwise the same request would refire whenever the user
  // toggles selection away and back to this session.
  const scrollNonce = scrollRequest?.nonce;
  const scrollIndex = scrollRequest?.index;
  React.useEffect(() => {
    if (scrollIndex == null) return;
    if (scrollIndex < 0 || scrollIndex >= messages.length) {
      onScrolled?.();
      return;
    }
    const handle = requestAnimationFrame(() => {
      virtualizer.scrollToIndex(scrollIndex, { align: "start" });
      onScrolled?.();
    });
    return () => cancelAnimationFrame(handle);
  }, [scrollNonce, scrollIndex, messages.length, virtualizer, onScrolled]);

  return (
    <div ref={parentRef} className="flex-1 overflow-auto px-4 py-2">
      <div
        style={{
          height: `${virtualizer.getTotalSize()}px`,
          width: "100%",
          position: "relative"
        }}
      >
        {virtualizer.getVirtualItems().map((virtualRow) => {
          const message = messages[virtualRow.index];
          const idx = virtualRow.index;
          const isFavorited =
            favorites?.keys.has(
              favoriteKey(favorites.session.source, favorites.session.id, idx)
            ) ?? false;
          return (
            <article
              key={virtualRow.key}
              data-index={virtualRow.index}
              ref={virtualizer.measureElement}
              data-role={roleClass(message.role)}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${virtualRow.start}px)`,
                paddingBottom: "20px"
              }}
            >
              <header className="flex items-center gap-2 mb-1">
                <span
                  aria-hidden="true"
                  data-role={roleClass(message.role)}
                  className="w-[3px] h-[0.95em] rounded-sm shrink-0 bg-muted-foreground/60 data-[role=user]:bg-teal-500 data-[role=assistant]:bg-blue-400 data-[role=tool]:bg-amber-500"
                />
                <span className="text-xs font-medium text-muted-foreground lowercase tabular-nums">
                  {message.role || "event"}
                </span>
                <span className="flex-1" />
                {favorites && (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <button
                        type="button"
                        onClick={(e) => {
                          e.stopPropagation();
                          favorites.onToggle(message, idx);
                        }}
                        aria-label={
                          isFavorited ? t("favorites.remove") : t("favorites.add")
                        }
                        className={cn(
                          "p-1 -mr-1 rounded transition-colors",
                          isFavorited
                            ? "text-amber-500 hover:text-amber-600"
                            : "text-muted-foreground/40 hover:text-foreground"
                        )}
                      >
                        <Star
                          className={cn(
                            "size-3.5",
                            isFavorited && "fill-current"
                          )}
                          aria-hidden
                        />
                      </button>
                    </TooltipTrigger>
                    <TooltipContent>
                      {isFavorited ? t("favorites.remove") : t("favorites.add")}
                    </TooltipContent>
                  </Tooltip>
                )}
              </header>
              <MessageBody text={message.text} className="pl-[11px]" />
            </article>
          );
        })}
      </div>
    </div>
  );
}
