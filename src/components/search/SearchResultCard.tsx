import { Folder, MessageSquare } from "lucide-react";
import type { SearchHit } from "@/types";
import { formatDate, formatRelativeDate } from "@/lib/format";
import { useT } from "@/i18n";
import {
  isSessionItem,
  memoryToolsOf,
  projectDisplayName,
  sourceDisplayName
} from "@/lib/session-utils";
import { BrandIcon } from "@/components/BrandIcon";
import { SnippetLine } from "@/components/SnippetLine";

export function SearchResultCard({
  hit,
  query,
  onOpen
}: {
  hit: SearchHit;
  query: string;
  onOpen: () => void;
}) {
  const t = useT();
  const session = hit.session;
  const isMemoryOrSkill = !isSessionItem(session);
  const tools = memoryToolsOf(session);
  return (
    <button
      onClick={onOpen}
      className="w-full text-left rounded-md bg-card px-2 py-2 transition-colors flex flex-col gap-1 hover:bg-accent/40"
    >
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-medium leading-snug line-clamp-2 flex-1 min-w-0">
          {session.title || "(untitled)"}
        </h2>
        <span className="text-xs text-muted-foreground shrink-0">
          {formatRelativeDate(session.updated_at ?? session.started_at, t)}
        </span>
      </div>
      <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
        <span className="flex items-center gap-1 min-w-0">
          <Folder size={12} className="shrink-0" />
          <span className="truncate">{projectDisplayName(session.project)}</span>
        </span>
        <span className="flex items-center gap-2 shrink-0">
          {isSessionItem(session) && (
            <span className="flex items-center gap-1">
              <MessageSquare size={11} />
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
      <SnippetLine
        snippet={hit.snippet}
        query={query}
        role={hit.role}
        matchCount={hit.match_count}
        truncated={hit.truncated}
      />
    </button>
  );
}
