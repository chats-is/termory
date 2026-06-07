import React from "react";
import { Calendar, ExternalLink, Folder, Star, Trash2 } from "lucide-react";
import { BrandIcon } from "@/components/BrandIcon";
import { EmptyState } from "@/components/EmptyState";
import { ListItemMenu } from "@/components/ListItemMenu";
import { MessageBody } from "@/components/MessageBody";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { formatDate, formatRelativeDate } from "@/lib/format";
import { sortFavoritesDesc } from "@/lib/favorites";
import { roleClass, sourceDisplayName, basename } from "@/lib/session-utils";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";
import type { AppSession, Favorite } from "@/types";

/**
 * Favorites page — two-column shell matching the Records layout:
 *   left  — list of favorite cards (compact summary + snippet)
 *   right — full message rendering of the selected favorite, using
 *           the same `.message-body` + role-bar pipeline as Records.
 *
 * Snapshots are stored full (raw `SessionMessage`) so the favorite
 * remains readable even when the source session is later deleted.
 */
export function FavoritesPage({
  favorites,
  sessions,
  onOpenSource,
  onRemove
}: {
  favorites: Favorite[];
  /** Current scan results — used to enable the "Open" button on
   * cards whose source session is still around. */
  sessions: AppSession[];
  /** Open the original session in the Records detail pane. */
  onOpenSource: (session: AppSession, messageIndex: number) => void;
  onRemove: (id: string) => void;
}) {
  const t = useT();
  const sorted = React.useMemo(() => sortFavoritesDesc(favorites), [favorites]);

  // `${source}::${id}` → AppSession (when still present).
  const sessionIndex = React.useMemo(() => {
    const map = new Map<string, AppSession>();
    for (const s of sessions) map.set(`${s.source}::${s.id}`, s);
    return map;
  }, [sessions]);

  const [selectedId, setSelectedId] = React.useState<string | null>(null);
  // Auto-select the newest favorite on first render / when the list
  // changes underneath us (e.g. a star added in Records). The selected
  // id might also be deleted out from under the page — fall back to
  // the first remaining card.
  React.useEffect(() => {
    if (sorted.length === 0) {
      if (selectedId !== null) setSelectedId(null);
      return;
    }
    if (!selectedId || !sorted.some((f) => f.id === selectedId)) {
      setSelectedId(sorted[0].id);
    }
  }, [sorted, selectedId]);

  if (sorted.length === 0) {
    return (
      <div className="flex-1 min-h-0 flex items-center justify-center px-6 py-10">
        <EmptyState
          icon={<Star size={32} />}
          title={t("favorites.emptyTitle")}
          description={t("favorites.emptyDesc")}
        />
      </div>
    );
  }

  const selected = sorted.find((f) => f.id === selectedId) ?? sorted[0];
  const selectedSession = sessionIndex.get(
    `${selected.source}::${selected.source_session_id}`
  );

  return (
    <main className="flex-1 min-h-0 grid grid-cols-[minmax(240px,300px)_1fr]">
      {/* ───────────────── List column ─────────────────── */}
      <aside className="flex flex-col min-h-0 bg-sidebar mt-3 ml-3 rounded-md">
        <div className="flex-1 min-h-0 overflow-auto p-3 flex flex-col gap-2">
          {sorted.map((fav) => {
            const active = fav.id === selected.id;
            return (
              <ListItemMenu
                key={fav.id}
                path={fav.source_session_path}
                id={fav.source_session_id}
                messageId={fav.id}
                source={fav.source}
                project={fav.source_session_project}
              >
              <button
                type="button"
                onClick={() => setSelectedId(fav.id)}
                aria-current={active ? "true" : undefined}
                className={cn(
                  "w-full text-left rounded-lg px-2 py-2 transition-colors flex flex-col gap-1 select-none",
                  active
                    ? "bg-primary text-primary-foreground [&_*]:text-primary-foreground"
                    : "text-sidebar-foreground hover:bg-accent/60 data-[state=open]:bg-accent/60"
                )}
              >
                <div className="flex items-baseline justify-between gap-2">
                  <span className="text-sm font-medium truncate flex-1 min-w-0">
                    {fav.source_session_title || "(untitled)"}
                  </span>
                  <span className="text-[10px] text-muted-foreground tabular-nums shrink-0">
                    {formatRelativeDate(fav.favorited_at)}
                  </span>
                </div>
                <div className="flex items-start gap-2 text-[11px] text-muted-foreground min-w-0">
                  <span
                    aria-hidden
                    data-role={roleClass(fav.message.role)}
                    className="w-[3px] h-3 rounded-sm shrink-0 mt-[2px] bg-muted-foreground/60 data-[role=user]:bg-teal-500 data-[role=assistant]:bg-blue-400 data-[role=tool]:bg-amber-500"
                  />
                  <span className="line-clamp-2 break-words leading-snug flex-1 min-w-0">
                    {snippetOf(fav.message.text)}
                  </span>
                  <span
                    aria-label={sourceDisplayName(fav.source)}
                    className="self-end shrink-0"
                  >
                    <BrandIcon source={fav.source} />
                  </span>
                </div>
              </button>
              </ListItemMenu>
            );
          })}
        </div>
      </aside>

      {/* ───────────────── Detail column ───────────────── */}
      <section className="flex flex-col min-h-0 mt-3 ml-3 bg-background">
        {/* Header — same layout / font scale as the Records detail
            pane: text-lg font-semibold title on a row of its own, meta
            row below in text-xs with `·`-separated chips. Actions
            cluster on the right of the title row. */}
        <header className="flex flex-col gap-2 p-3">
          <h2 className="text-lg font-semibold leading-snug truncate">
            {selected.source_session_title || "(untitled)"}
          </h2>

          <div className="flex items-center justify-between gap-2">
            <div className="flex items-center gap-2 text-xs leading-none text-muted-foreground flex-wrap min-w-0">
              <span className="inline-flex items-center gap-1 tabular-nums">
                <Calendar size={12} className="shrink-0" />
                {formatDate(selected.favorited_at)}
              </span>
              {selected.source_session_project && (
                <>
                  <span className="text-border">·</span>
                  <span className="inline-flex items-center gap-1 min-w-0">
                    <Folder size={12} className="shrink-0" />
                    <span className="truncate">
                      {basename(selected.source_session_project)}
                    </span>
                  </span>
                </>
              )}
            </div>
            <div className="inline-flex items-center gap-2 shrink-0">
              {selectedSession ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={() =>
                        onOpenSource(
                          selectedSession,
                          selected.source_message_index
                        )
                      }
                      aria-label={t("favorites.openOriginal")}
                      className="inline-flex shrink-0 text-muted-foreground hover:text-foreground transition-colors"
                    >
                      <ExternalLink size={12} />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent>{t("favorites.openOriginal")}</TooltipContent>
                </Tooltip>
              ) : (
                <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
                  {t("favorites.archived")}
                </span>
              )}
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={() => onRemove(selected.id)}
                    aria-label={t("favorites.remove")}
                    className="inline-flex shrink-0 text-muted-foreground hover:text-destructive transition-colors"
                  >
                    <Trash2 size={12} />
                  </button>
                </TooltipTrigger>
                <TooltipContent>{t("favorites.remove")}</TooltipContent>
              </Tooltip>
            </div>
          </div>
        </header>

        {/* Full-render body — same pipeline as Records detail. */}
        <div className="flex-1 min-h-0 overflow-auto px-4 py-3">
          <article data-role={roleClass(selected.message.role)}>
            <header className="flex items-center gap-2 mb-1">
              <span
                aria-hidden
                data-role={roleClass(selected.message.role)}
                className="w-[3px] h-[0.95em] rounded-sm shrink-0 bg-muted-foreground/60 data-[role=user]:bg-teal-500 data-[role=assistant]:bg-blue-400 data-[role=tool]:bg-amber-500"
              />
              <span className="text-xs font-medium text-muted-foreground lowercase tabular-nums">
                {selected.message.role || "event"}
              </span>
            </header>
            <MessageBody
              text={selected.message.text}
              className="pl-[11px]"
            />
          </article>
        </div>
      </section>
    </main>
  );
}

/** Up to ~240-char preview for the list card. Markdown markers are
 * kept (the user sees them in Records too); whitespace and newlines
 * are collapsed into single spaces so CSS `line-clamp-2` can do the
 * visual truncation without weird line breaks in fenced code. */
function snippetOf(text: string): string {
  const collapsed = text.replace(/\s+/g, " ").trim();
  return collapsed.length > 240
    ? `${collapsed.slice(0, 237)}…`
    : collapsed || "(empty)";
}
