import React from "react";
import { Search } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList
} from "@/components/ui/command";
import { useSearchHits } from "@/hooks/useSearchHits";
import {
  projectDisplayName,
  sessionKey,
  sourceDisplayName,
  typeLabelOf
} from "@/lib/session-utils";
import { INPUT_NO_AUTO } from "@/lib/utils";
import type { AppSession, SearchHit } from "@/types";

export function CommandPalette({
  sessions,
  onOpenItem,
  recentSearches,
  onCommitSearch
}: {
  sessions: AppSession[];
  onOpenItem: (item: AppSession, messageIndex?: number) => void;
  recentSearches: string[];
  onCommitSearch: (query: string) => void;
  onClearRecent: () => void;
}) {
  const [open, setOpen] = React.useState(false);
  const [query, setQuery] = React.useState("");
  const { hits, loading, committedQuery } = useSearchHits(query);

  // Global ⌘K / ⌘F (or Ctrl variants) toggle.
  React.useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      const isToggle =
        (event.metaKey || event.ctrlKey) && (key === "k" || key === "f");
      if (isToggle) {
        event.preventDefault();
        setOpen((current) => !current);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  React.useEffect(() => {
    if (!open) setQuery("");
  }, [open]);

  const handleOpen = (item: AppSession, messageIndex?: number) => {
    onCommitSearch(committedQuery || query);
    onOpenItem(item, messageIndex);
    setOpen(false);
  };

  // Cheap metadata-only fallback so the palette feels live before the
  // backend debounce settles (or when there are 1-char queries the
  // backend rejects).
  const fallbackHits = React.useMemo<SearchHit[]>(() => {
    const trimmed = query.trim();
    if (committedQuery === trimmed) return [];
    if (trimmed.length === 0) return [];
    const needle = trimmed.toLowerCase();
    const matches: SearchHit[] = [];
    for (const session of sessions) {
      const haystack = `${session.title}\n${session.project}\n${session.source}`.toLowerCase();
      if (haystack.includes(needle)) {
        matches.push({ session, snippet: "", role: "", match_count: 0, truncated: false });
        if (matches.length >= 16) break;
      }
    }
    return matches;
  }, [query, committedQuery, sessions]);

  const rows = hits.length > 0 ? hits.slice(0, 8) : fallbackHits.slice(0, 8);
  const trimmed = query.trim();
  const settled = committedQuery === trimmed && trimmed.length >= 2;
  const showRecents = trimmed.length === 0 && recentSearches.length > 0;
  const showEmpty = trimmed.length > 0 && rows.length === 0 && (settled || !loading);

  return (
    <CommandDialog
      open={open}
      onOpenChange={setOpen}
      title="Quick search"
      description="Find sessions, memories, skills"
      shouldFilter={false}
    >
      <CommandInput {...INPUT_NO_AUTO}
        placeholder="Find sessions, memories, skills…"
        value={query}
        onValueChange={setQuery}
      />
      <CommandList>
        {trimmed.length === 0 && recentSearches.length === 0 && (
          <CommandEmpty>Type to search across all records.</CommandEmpty>
        )}
        {showEmpty && <CommandEmpty>No matches.</CommandEmpty>}
        {showRecents && (
          <CommandGroup heading="Recent searches">
            {recentSearches.map((entry) => (
              <CommandItem
                key={`recent:${entry}`}
                value={`recent:${entry}`}
                onSelect={() => setQuery(entry)}
              >
                <Search size={13} className="text-muted-foreground" />
                <span className="truncate">{entry}</span>
              </CommandItem>
            ))}
          </CommandGroup>
        )}
        {rows.length > 0 && (
          <CommandGroup heading={hits.length > 0 ? "Results" : "Matching"}>
            {rows.map((row) => {
              const session = row.session;
              const typeLabel = typeLabelOf(session);
              return (
                <CommandItem
                  key={sessionKey(session)}
                  value={sessionKey(session)}
                  onSelect={() => handleOpen(session, row.first_match_index)}
                  className="items-start gap-2.5"
                >
                  <Badge
                    variant="secondary"
                    className="text-[10px] uppercase tracking-wide shrink-0"
                  >
                    {typeLabel}
                  </Badge>
                  <div className="flex-1 min-w-0 flex flex-col gap-0.5">
                    <span className="text-sm font-medium leading-tight truncate">
                      {session.title || "(untitled)"}
                    </span>
                    <span className="flex items-center gap-1.5 text-xs text-muted-foreground min-w-0">
                      <span>{sourceDisplayName(session.source)}</span>
                      <span className="text-border">·</span>
                      <span className="truncate">
                        {projectDisplayName(session.project)}
                      </span>
                    </span>
                  </div>
                </CommandItem>
              );
            })}
          </CommandGroup>
        )}
      </CommandList>
    </CommandDialog>
  );
}
