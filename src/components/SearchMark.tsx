import React from "react";

/** THE search/find highlight — the single component every highlighted
 * match in the app renders through:
 *  - SnippetLine (Search page + ⌘K palette snippets) renders it
 *    directly;
 *  - MessageBody maps react-markdown's `mark` elements (injected by
 *    rehypeHighlightTerm for the in-session/doc find) onto it.
 * There is deliberately no styling anywhere else — change the look
 * here and every highlight follows. Framework palette only; solid
 * colors so it reads identically over cards/code/selected rows; the
 * trailing `!` on the text utilities beats the selected list row's
 * equal-specificity `[&_*]:text-primary-foreground` cascade. */
export function SearchMark({ children }: { children?: React.ReactNode }) {
  return (
    <mark className="bg-yellow-300 text-yellow-950! dark:bg-yellow-800 dark:text-amber-100!">
      {children}
    </mark>
  );
}
