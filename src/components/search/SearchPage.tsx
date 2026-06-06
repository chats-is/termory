import React from "react";
import { Loader2, Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { useSearchHits } from "@/hooks/useSearchHits";
import { formatFullNumber } from "@/lib/format";
import { sessionKey } from "@/lib/session-utils";
import { INPUT_NO_AUTO } from "@/lib/utils";
import type { AppSession } from "@/types";
import { EmptyState } from "@/components/EmptyState";
import { SearchResultCard } from "./SearchResultCard";

export function SearchPage({
  sessions,
  onOpenItem,
  recentSearches,
  onCommitSearch,
  onClearRecent
}: {
  sessions: AppSession[];
  onOpenItem: (item: AppSession, messageIndex?: number) => void;
  recentSearches: string[];
  onCommitSearch: (query: string) => void;
  onClearRecent: () => void;
}) {
  const [query, setQuery] = React.useState("");
  const { hits, loading, committedQuery, error } = useSearchHits(query);
  const inputRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const handleOpen = React.useCallback(
    (item: AppSession, messageIndex?: number) => {
      onCommitSearch(committedQuery || query);
      onOpenItem(item, messageIndex);
    },
    [committedQuery, onCommitSearch, onOpenItem, query]
  );

  const trimmed = query.trim();
  const settled = committedQuery === trimmed && trimmed.length >= 2;
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
            type="search"
            placeholder="Search across sessions, memories, skills…"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            autoFocus
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            className="h-11 pl-9 pr-3 border-0 bg-transparent shadow-none focus-visible:ring-0"
          />
        </div>
      </div>
      <div className="flex-1 min-h-0 overflow-auto px-3 flex flex-col gap-5">
        {error && (
          <div className="rounded-md border border-destructive/30 bg-destructive/10 text-destructive text-sm px-3 py-2">
            {error}
          </div>
        )}
        {trimmed.length < 2 && !loading && (
          <div className="flex flex-col items-center justify-center text-center gap-3 py-12 text-muted-foreground">
            <Search className="size-7" />
            <p className="text-sm">Search inside every session, memory, and skill Termory scans.</p>
            <p className="flex items-center gap-1 text-xs">
              <span>Press</span>
              <kbd className="inline-flex h-5 items-center justify-center rounded bg-muted px-1.5 text-[10px] font-medium font-mono">⌘</kbd>
              <kbd className="inline-flex h-5 items-center justify-center rounded bg-muted px-1.5 text-[10px] font-medium font-mono">K</kbd>
              <span>to summon search from anywhere.</span>
            </p>
            <p className="text-xs">{formatFullNumber(sessions.length)} records indexed.</p>
            {recentSearches.length > 0 && (
              <div className="w-full max-w-md mt-4 flex flex-col gap-2 items-center">
                <div className="flex items-center gap-3 text-xs">
                  <span>Recent</span>
                  <button
                    type="button"
                    onClick={onClearRecent}
                    className="text-muted-foreground hover:text-foreground"
                  >
                    Clear
                  </button>
                </div>
                <div className="flex flex-wrap justify-center gap-1.5">
                  {recentSearches.map((entry) => (
                    <button
                      key={entry}
                      type="button"
                      onClick={() => setQuery(entry)}
                      className="inline-flex items-center gap-1 rounded-full bg-muted px-2.5 py-1 text-xs hover:bg-accent"
                    >
                      {entry}
                    </button>
                  ))}
                </div>
              </div>
            )}
          </div>
        )}
        {noResults && <EmptyState icon={<Search />} title={`No matches for "${trimmed}"`} />}
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
