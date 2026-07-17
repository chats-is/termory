export type SnippetSegment = { text: string; match: boolean };

/** In-session find matcher: indices of already-LOWERCASED texts
 * containing `query` (matched case-insensitively — the same semantics
 * as the backend `search_sessions`, which lowercases both sides; a
 * blank / whitespace-only query matches nothing). Takes pre-lowered
 * texts so hot paths lowercase the transcript ONCE per opened find
 * (App.tsx `loweredTexts` memo) instead of on every keystroke —
 * sessions run to 100+MB of text, and re-allocating lowercased copies
 * per keystroke caused GC-visible jank. */
export function matchingLoweredIndices(
  loweredTexts: ReadonlyArray<string>,
  query: string
): number[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];
  const out: number[] = [];
  for (let i = 0; i < loweredTexts.length; i++) {
    if (loweredTexts[i].includes(needle)) out.push(i);
  }
  return out;
}


export function splitSnippet(snippet: string, query: string): SnippetSegment[] {
  if (!query) return [{ text: snippet, match: false }];
  const lowerSnippet = snippet.toLowerCase();
  const lowerQuery = query.toLowerCase();
  const out: SnippetSegment[] = [];
  let cursor = 0;
  while (cursor < snippet.length) {
    const idx = lowerSnippet.indexOf(lowerQuery, cursor);
    if (idx === -1) {
      out.push({ text: snippet.slice(cursor), match: false });
      break;
    }
    if (idx > cursor) out.push({ text: snippet.slice(cursor, idx), match: false });
    out.push({ text: snippet.slice(idx, idx + lowerQuery.length), match: true });
    cursor = idx + lowerQuery.length;
  }
  return out;
}
