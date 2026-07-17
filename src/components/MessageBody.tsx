import React from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "@/lib/utils";
import { rehypeHighlightTerm } from "@/lib/highlight-term";
import { SearchMark } from "@/components/SearchMark";

const messageRemarkPlugins = [remarkGfm];

/** Every `<mark>` in rendered markdown (only rehypeHighlightTerm
 * injects them — raw HTML in session text is never parsed) renders
 * through the ONE shared SearchMark component, the same one
 * SnippetLine uses, so all highlights in the app are a single
 * implementation. Module-level so the mapping identity is stable. */
const messageComponents: Components = {
  mark: ({ children }) => <SearchMark>{children}</SearchMark>
};

export const MessageBody = React.memo(function MessageBody({
  text,
  className,
  highlight
}: {
  text: string;
  className?: string;
  /** In-session find term — every case-insensitive occurrence in the
   * rendered output gets wrapped in a SearchMark. */
  highlight?: string;
}) {
  const rehypePlugins = React.useMemo(
    () => (highlight?.trim() ? [rehypeHighlightTerm(highlight)] : undefined),
    [highlight]
  );
  return (
    <div className={cn("message-body", className)}>
      <ReactMarkdown
        remarkPlugins={messageRemarkPlugins}
        rehypePlugins={rehypePlugins}
        components={messageComponents}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
