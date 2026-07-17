import React from "react";
import { Loader2, Search } from "lucide-react";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList
} from "@/components/ui/command";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { SearchClearButton } from "@/components/SearchClearButton";
import { SearchResultRow } from "@/components/search/SearchResultRow";
import { useSearchHits } from "@/hooks/useSearchHits";
import { sessionKey } from "@/lib/session-utils";
import { cn, INPUT_NO_AUTO } from "@/lib/utils";
import { useT } from "@/i18n";
import type { AppSession } from "@/types";

/** Quick-jump palette (⌘K — its ONLY trigger; ⌘F belongs to the
 * Records in-session find, App.tsx). Shape specified item-by-item by
 * the user (2026-07-17): no group headings, no recent-searches
 * section, no dialog close button; rows mirror the Records list card
 * (title / meta / highlighted snippet); the search state is a spinner
 * in the input row; the "view all results" bridge is a FIXED footer
 * below the scrolling list. */
export function CommandPalette({
  open,
  onOpenChange,
  onOpenItem,
  onCommitSearch,
  onOpenSearchPage
}: {
  /** CONTROLLED open state, owned by App — the ⌘F handler there must
   * know whether the palette is open (⌘F does nothing while it is,
   * instead of opening the find bar underneath the modal). */
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onOpenItem: (item: AppSession, messageIndex?: number, query?: string) => void;
  onCommitSearch: (query: string) => void;
  /** "View all results" bridge — navigates to the full Search page
   * with the current query (the palette shows capped rows only). */
  onOpenSearchPage?: (query: string) => void;
}) {
  const t = useT();
  const [query, setQuery] = React.useState("");
  const { hits, loading, committedQuery } = useSearchHits(query);

  // Global ⌘K (or Ctrl+K) toggle — the palette's only shortcut.
  React.useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.shiftKey) return;
      if (event.key.toLowerCase() !== "k") return;
      event.preventDefault();
      onOpenChange(!open);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, onOpenChange]);

  React.useEffect(() => {
    if (!open) setQuery("");
  }, [open]);

  const handleOpen = (item: AppSession, messageIndex?: number) => {
    const committed = committedQuery || query;
    onCommitSearch(committed);
    // Carry the query into the Records in-detail find bar (same as
    // SearchPage) so every match is reachable, not just the first.
    onOpenItem(item, messageIndex, committed);
    onOpenChange(false);
  };

  // Backend results ONLY — no instant metadata fallback. The palette
  // shows exactly what the Search page would show for the same query
  // (user decision: the two surfaces must run in lockstep; the old
  // fallback made title-only rows appear first and then "flash" into
  // snippet rows when the backend settled).
  const rows = hits.slice(0, 8);
  const trimmed = query.trim();
  const settled = committedQuery === trimmed && trimmed.length >= 1;
  // "No matches" ONLY once the search has actually SETTLED for the
  // current query — identical to SearchPage's `noResults`. The old
  // `(settled || !loading)` treated the 300ms debounce window (loading
  // still false) as "confirmed empty", flashing "No matches" on every
  // keystroke before the results arrived.
  const showEmpty = settled && !loading && rows.length === 0;
  const highlightQuery = committedQuery || trimmed;
  // "Searching" covers the whole not-yet-settled window (debounce +
  // IPC) — the raw `loading` flag alone is true only for the few ms
  // of the actual invoke and would never be visible.
  const searching = trimmed.length >= 1 && (loading || committedQuery !== trimmed);

  const bridgeToSearch = () => {
    if (!onOpenSearchPage || trimmed.length === 0) return;
    onCommitSearch(committedQuery || trimmed);
    onOpenSearchPage(trimmed);
    onOpenChange(false);
  };

  return (
    /* Official composition (ui/command.tsx is the VERBATIM registry
       file, which offers no shouldFilter passthrough on CommandDialog
       — so the same Dialog + Command structure is composed here, with
       shouldFilter on the Command where cmdk accepts it. The Command
       className below is the official CommandDialog's, copied
       unchanged.) */
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogHeader className="sr-only">
        <DialogTitle>{t("command.title")}</DialogTitle>
        <DialogDescription>{t("command.description")}</DialogDescription>
      </DialogHeader>
      <DialogContent
        className="overflow-hidden p-0 sm:max-w-md rounded-2xl [&_[cmdk-item]]:rounded-lg"
        showCloseButton={false}
      >
        {/* Based on the official CommandDialog's Command className, with
            the blanket `[cmdk-item]` py-3 and svg h-5/w-5 clauses
            REMOVED — this copy is palette-owned, and those clauses only
            forced every custom row to fight back with !important sizes.
            Row elements now size themselves normally. */}
        <Command
          shouldFilter={false}
          className="**:data-[slot=command-input-wrapper]:h-11 [&_[cmdk-input-wrapper]_svg]:h-4 [&_[cmdk-input-wrapper]_svg]:w-4 [&_[cmdk-input]]:h-11"
        >
          {/* Input box CLONES the Search page's (SearchPage.tsx:51): a
              rounded-md bg-muted pill, h-11, size-4 icon, no border, no
              dialog close X (Esc / ⌘K / click-outside dismiss). While
              searching, the stock left icon is made invisible (not
              hidden — layout must not shift) and a spinner renders in
              its exact spot, mirroring SearchPage's icon swap
              (SearchPage.tsx:52). */}
          <div
            className={cn(
              "relative mx-3 mt-3 mb-2",
              "[&_[data-slot=command-input-wrapper]]:rounded-md [&_[data-slot=command-input-wrapper]]:bg-muted [&_[data-slot=command-input-wrapper]]:border-0",
              searching &&
                "[&_[data-slot=command-input-wrapper]>svg:first-child]:invisible"
            )}
          >
            {searching && (
              <Loader2
                aria-hidden
                className="absolute left-3 top-1/2 -translate-y-1/2 size-4 animate-spin text-muted-foreground"
              />
            )}
            <CommandInput {...INPUT_NO_AUTO}
              placeholder={t("command.placeholder")}
              value={query}
              onValueChange={setQuery}
              className="pr-8"
              onKeyDown={(event) => {
                // Enter with no result rows → straight to the Search page
                // (the footer bridge is not a cmdk item, so cmdk has
                // nothing to select in that state).
                if (event.key === "Enter" && rows.length === 0) {
                  event.preventDefault();
                  bridgeToSearch();
                }
              }}
            />
            {query.length > 0 && (
              <SearchClearButton onClear={() => setQuery("")} />
            )}
          </div>
          <CommandList>
            {trimmed.length === 0 && (
              <CommandEmpty>{t("command.typeToSearch")}</CommandEmpty>
            )}
            {showEmpty && <CommandEmpty>{t("command.noMatches")}</CommandEmpty>}
            {rows.length > 0 && (
              <CommandGroup className="px-3 [&_[cmdk-group-items]]:flex [&_[cmdk-group-items]]:flex-col [&_[cmdk-group-items]]:gap-1.5">
                {rows.map((row) => (
                  /* Row body is the SHARED SearchResultRow — the same
                     component the Search page's SearchResultCard wraps
                     — so the two result lists are one implementation. */
                  <CommandItem
                    key={sessionKey(row.session)}
                    value={sessionKey(row.session)}
                    onSelect={() => handleOpen(row.session, row.first_match_index)}
                    className="flex flex-col items-stretch gap-1 rounded-md bg-card px-2 py-2"
                  >
                    <SearchResultRow hit={row} query={highlightQuery} />
                  </CommandItem>
                ))}
              </CommandGroup>
            )}
          </CommandList>
          {/* Fixed footer bridge to the full Search page — OUTSIDE the
              scrolling CommandList, so it never moves however long the
              result list gets. Plain button by design (not a cmdk item);
              Enter reaches it via the input's no-rows handler above. */}
          {trimmed.length > 0 && onOpenSearchPage && (
            <button
              type="button"
              onClick={bridgeToSearch}
              className="flex items-center gap-2 rounded-md mx-3 mt-2 mb-3 px-3 py-2.5 text-sm text-muted-foreground hover:bg-accent hover:text-accent-foreground transition-colors"
            >
              <Search className="size-4 shrink-0" />
              <span className="truncate">
                {t("command.viewAllResults", { query: trimmed })}
              </span>
            </button>
          )}
        </Command>
      </DialogContent>
    </Dialog>
  );
}
