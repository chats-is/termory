import React from "react";
import { Folder } from "lucide-react";
import { cn } from "@/lib/utils";
import { formatRelativeDate } from "@/lib/format";
import { useT } from "@/i18n";
import {
  memoryToolsOf,
  projectDisplayName,
  sourceDisplayName
} from "@/lib/session-utils";
import type { AppSession, SearchHit } from "@/types";
import { BrandIcon } from "./BrandIcon";
import { SnippetLine } from "./SnippetLine";

// forwardRef + ...rest so the call site can make it a context-menu
// `asChild` trigger (Radix passes a ref + event handlers to the child).
export const MemoryCard = React.forwardRef<
  HTMLButtonElement,
  {
    item: AppSession;
    selected: AppSession | null;
    onClick: () => void;
    query: string;
    contentQuery: string;
    hit: SearchHit | undefined;
    showSource: boolean;
  } & Omit<React.ComponentPropsWithoutRef<"button">, "onClick">
>(function MemoryCard(
  {
    item,
    selected,
    onClick,
    query,
    contentQuery,
    hit,
    showSource,
    className,
    ...rest
  },
  ref
) {
  const t = useT();
  const showSnippet = !!hit && query.toLowerCase() === contentQuery.toLowerCase();
  const isActive = selected?.path === item.path && selected?.id === item.id;
  const tools = memoryToolsOf(item);
  return (
    <button
      ref={ref}
      onClick={onClick}
      {...rest}
      className={cn(
        "w-full text-left rounded-lg px-2 py-2 flex flex-col gap-1 select-none",
        isActive
          ? "bg-primary text-primary-foreground [&_*]:text-primary-foreground"
          : "hover:bg-accent/60 data-[state=open]:bg-accent/60",
        className
      )}
    >
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-medium leading-snug line-clamp-2 flex-1 min-w-0">
          {item.title}
        </h2>
        <span className="text-xs text-muted-foreground shrink-0">
          {formatRelativeDate(item.updated_at ?? item.started_at, t)}
        </span>
      </div>
      <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
        <span className="flex items-center gap-1 min-w-0">
          <Folder size={12} className="shrink-0" />
          <span className="truncate">{projectDisplayName(item.project)}</span>
        </span>
        {showSource && (
          <span className="flex items-center gap-1 shrink-0">
            {tools.map((tool) => {
              const label = tool === "Other" ? t("command.typeMemory") : sourceDisplayName(tool);
              return (
                <span key={tool} aria-label={label}>
                  <BrandIcon source={tool === "Other" ? "Memory" : tool} />
                </span>
              );
            })}
          </span>
        )}
      </div>
      {showSnippet && hit && (
        <SnippetLine
          snippet={hit.snippet}
          query={query}
          role={hit.role}
          matchCount={hit.match_count}
          truncated={hit.truncated}
        />
      )}
    </button>
  );
});
