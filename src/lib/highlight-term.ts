import { splitSnippet } from "./search-utils";

/** Minimal structural hast types — enough for the tree walk below
 * without pulling in @types/hast. */
type HastText = { type: "text"; value: string };
type HastElement = {
  type: "element";
  tagName: string;
  properties?: Record<string, unknown>;
  children: HastNode[];
};
type HastNode = HastText | HastElement | { type: string; children?: HastNode[] };

function isText(node: HastNode): node is HastText {
  return node.type === "text" && typeof (node as HastText).value === "string";
}

function hasChildren(node: HastNode): node is HastNode & { children: HastNode[] } {
  return Array.isArray((node as { children?: unknown }).children);
}

/** Split one text node into text/<mark> pieces around every
 * case-insensitive occurrence of `needle` (already lowercased).
 * The occurrence walk itself is `splitSnippet` — the SAME tokenizer
 * the Records/Search snippet highlight uses — so the two highlight
 * surfaces can never drift; this function only maps its segments
 * onto hast nodes. */
function splitTextNode(node: HastText, needle: string): HastNode[] {
  const segments = splitSnippet(node.value, needle);
  if (segments.length === 1 && !segments[0].match) return [node];
  return segments.map((segment) =>
    segment.match
      ? {
          // Bare <mark> — styling lives in the SearchMark component,
          // which MessageBody maps every mark element onto.
          type: "element" as const,
          tagName: "mark",
          properties: {},
          children: [{ type: "text" as const, value: segment.text }]
        }
      : { type: "text" as const, value: segment.text }
  );
}

/** Walk a hast tree, wrapping every case-insensitive occurrence of
 * `needle` inside text nodes with a `<mark>` element. Mutates the tree
 * (it's the per-render tree react-markdown just built — never shared).
 * Exported for unit tests. */
export function markTermInTree(tree: HastNode, needle: string): void {
  if (!hasChildren(tree)) return;
  const next: HastNode[] = [];
  for (const child of tree.children) {
    if (isText(child)) {
      next.push(...splitTextNode(child, needle));
    } else {
      markTermInTree(child, needle);
      next.push(child);
    }
  }
  tree.children = next;
}

/** rehype plugin factory: highlights every occurrence of `term`
 * (case-insensitive substring — the same semantics as the in-session
 * find's `matchingLoweredIndices`) in the rendered output by wrapping
 * it in `<mark>`. Matching happens on the RENDERED text nodes, so a
 * query that only matches markdown syntax (e.g. `**`) simply produces
 * no visible mark — the match/count semantics stay source-based. */
export function rehypeHighlightTerm(term: string) {
  const needle = term.trim().toLowerCase();
  return () => (tree: HastNode) => {
    if (!needle) return;
    markTermInTree(tree, needle);
  };
}
