import { Folder, MessageSquare } from "lucide-react";
import type { SearchHit } from "@/types";
import { formatRelativeDate } from "@/lib/format";
import { useT } from "@/i18n";
import {
  isSessionItem,
  memoryToolsOf,
  projectDisplayName,
  sourceDisplayName
} from "@/lib/session-utils";
import { BrandIcon } from "@/components/BrandIcon";
import { SnippetLine } from "@/components/SnippetLine";

/** Presentational body of ONE search hit — title/date row, meta row
 * (project · message count · source icons), highlighted snippet.
 * Shared by the Search page (`SearchResultCard`'s <button>) and the
 * ⌘K palette (`CommandItem`), so the row structure exists exactly
 * once and the two result lists can't drift. The interactive wrapper
 * (button / CommandItem with `flex flex-col gap-1`) belongs to the
 * caller.
 *
 * Icon sizes are CLASS-based (size-3 / size-[11px]) rather than the
 * lucide `size` attribute: inside a cmdk CommandItem the stock
 * `svg:not([class*='size-'])` rule would otherwise force
 * attribute-sized icons to 16px, rendering the same row differently
 * in the two hosts. */
export function SearchResultRow({
  hit,
  query
}: {
  hit: SearchHit;
  query: string;
}) {
  const t = useT();
  const session = hit.session;
  const isMemoryOrSkill = !isSessionItem(session);
  const tools = memoryToolsOf(session);
  return (
    <>
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-medium leading-snug line-clamp-2 flex-1 min-w-0">
          {session.title || t("records.untitled")}
        </h2>
        <span className="text-xs text-muted-foreground shrink-0">
          {formatRelativeDate(session.updated_at ?? session.started_at, t)}
        </span>
      </div>
      <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
        <span className="flex items-center gap-1 min-w-0">
          <Folder className="size-3 shrink-0" />
          <span className="truncate">{projectDisplayName(session.project)}</span>
        </span>
        <span className="flex items-center gap-2 shrink-0">
          {isSessionItem(session) && (
            <span className="flex items-center gap-1">
              <MessageSquare className="size-[11px]" />
              <span className="tabular-nums">{session.message_count}</span>
            </span>
          )}
          {isMemoryOrSkill ? (
            <span className="flex items-center gap-1">
              {tools.map((tool) => {
                const label = tool === "Other" ? "Memory" : sourceDisplayName(tool);
                return (
                  <span key={tool} aria-label={label}>
                    <BrandIcon source={tool === "Other" ? "Memory" : tool} />
                  </span>
                );
              })}
            </span>
          ) : (
            <span aria-label={sourceDisplayName(session.source)}>
              <BrandIcon source={session.source} />
            </span>
          )}
        </span>
      </div>
      {hit.snippet && (
        /* The ! sizes SnippetLine's internal icon (no size-* class of
           its own) consistently in BOTH hosts — inside a CommandItem
           the stock :not([class*='size-']) rule would win otherwise. */
        <div className="[&_svg]:!size-[11px]">
          <SnippetLine
            snippet={hit.snippet}
            query={query}
            role={hit.role}
            matchCount={hit.match_count}
            truncated={hit.truncated}
          />
        </div>
      )}
    </>
  );
}
