import React from "react";
import { Loader2, Search, Trash2 } from "lucide-react";
import { Input } from "@/components/ui/input";
import { SearchClearButton } from "@/components/SearchClearButton";
import { Button } from "@/components/ui/button";
import { useSearchHits } from "@/hooks/useSearchHits";
import { formatFullNumber } from "@/lib/format";
import { sessionKey } from "@/lib/session-utils";
import { INPUT_NO_AUTO } from "@/lib/utils";
import { useT } from "@/i18n";
import type { AppSession } from "@/types";
import { EmptyState } from "@/components/EmptyState";
import { SearchResultCard } from "./SearchResultCard";

export function SearchPage({
  sessions,
  onOpenItem,
  recentSearches,
  onCommitSearch,
  onClearRecent,
  seed,
  onSeedConsumed
}: {
  sessions: AppSession[];
  onOpenItem: (item: AppSession, messageIndex?: number, query?: string) => void;
  recentSearches: string[];
  onCommitSearch: (query: string) => void;
  onClearRecent: () => void;
  /** Query handed over by the ⌘K palette's "view all results" bridge.
   * Nonce so bridging the same query twice still re-seeds. */
  seed?: { query: string; nonce: number } | null;
  /** Called once a seed has been applied — the owner clears it, so a
   * LATER normal visit to this page can't resurrect the stale query
   * on remount (the page is route-gated and remounts every visit). */
  onSeedConsumed?: () => void;
}) {
  const t = useT();
  const [query, setQuery] = React.useState("");
  // Recording a recent search lives in the hook (fires when a search
  // returns results), so the Search page and the ⌘K palette behave the same.
  const { hits, loading, committedQuery, error } = useSearchHits(
    query,
    onCommitSearch
  );
  const inputRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Consume-once bridge seed: covers both fresh-mount (bridge set the
  // seed, then routed here) and already-mounted re-bridge.
  React.useEffect(() => {
    if (!seed) return;
    setQuery(seed.query);
    onSeedConsumed?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [seed]);

  const handleOpen = React.useCallback(
    (item: AppSession, messageIndex?: number) => {
      const committed = committedQuery || query;
      // The recent search was already saved when this query returned
      // results (in useSearchHits) — opening just navigates.
      // Pass the query along so Records opens with the in-detail find
      // bar pre-filled — the search page only scrolls to the FIRST
      // match; the find bar carries the user to the rest.
      onOpenItem(item, messageIndex, committed);
    },
    [committedQuery, onOpenItem, query]
  );

  const trimmed = query.trim();
  const settled = committedQuery === trimmed && trimmed.length >= 1;
  const noResults = settled && !loading && hits.length === 0;

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      <div className="flex flex-col gap-2 p-3">
        <div className="relative flex items-center rounded-md bg-muted">
          {loading ? (
            <Loader2 className="absolute left-3 size-4 animate-spin text-muted-foreground" />
          ) : (
            <Search className="absolute left-3 size-4 text-muted-foreground pointer-events-none" />
          )}
          <Input {...INPUT_NO_AUTO}
            ref={inputRef}
            placeholder={t("search.placeholder")}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            autoFocus
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            className="h-11 pl-9 pr-8 border-0 bg-transparent shadow-none focus-visible:ring-0"
          />
          {query.length > 0 && (
            <SearchClearButton onClear={() => setQuery("")} />
          )}
        </div>
      </div>
      <div className="flex-1 min-h-0 overflow-auto px-3 flex flex-col gap-5">
        {error && (
          <div className="rounded-md border border-destructive/30 bg-destructive/10 text-destructive text-sm px-3 py-2">
            {error}
          </div>
        )}
        {trimmed.length < 1 && !loading && (
          <div className="flex flex-col items-center justify-center text-center gap-3 py-12 text-muted-foreground">
            <Search className="size-7" />
            <p className="text-sm">{t("search.hint")}</p>
            <p className="flex items-center gap-1 text-xs">
              <span>{t("search.press")}</span>
              <kbd className="inline-flex h-5 items-center justify-center rounded bg-muted px-1.5 text-[10px] font-medium font-mono">⌘</kbd>
              <kbd className="inline-flex h-5 items-center justify-center rounded bg-muted px-1.5 text-[10px] font-medium font-mono">K</kbd>
              <span>{t("search.summon")}</span>
            </p>
            <p className="text-xs">{t("search.indexed", { n: formatFullNumber(sessions.length) })}</p>
            {recentSearches.length > 0 && (
              <div className="w-full max-w-md mt-4 flex flex-col gap-3 items-center">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={onClearRecent}
                  className="text-foreground"
                >
                  <Trash2 className="size-4" />
                  {t("search.clear")}
                </Button>
                <div className="flex flex-wrap justify-center gap-1.5">
                  {recentSearches.map((entry) => (
                    <button
                      key={entry}
                      type="button"
                      onClick={() => setQuery(entry)}
                      className="inline-flex items-center gap-1 rounded-full bg-muted px-3 py-0.5 text-[10px] hover:bg-accent"
                    >
                      {entry}
                    </button>
                  ))}
                </div>
              </div>
            )}
          </div>
        )}
        {noResults && (
          <EmptyState icon={<Search />} title={t("search.noMatch", { query: trimmed })} />
        )}
        {hits.length > 0 && (
          <div className="flex flex-col gap-1.5">
            {hits.slice(0, 200).map((hit) => (
              <SearchResultCard
                key={sessionKey(hit.session)}
                hit={hit}
                query={committedQuery}
                onOpen={() => handleOpen(hit.session, hit.first_match_index)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
