import React from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SearchHit } from "../types";

/** Debounce so the search — and its loading state — only start AFTER the
 * user stops typing, not on every keystroke. */
const SEARCH_DEBOUNCE_MS = 300;

export function useSearchHits(
  query: string,
  /** Called with the query the moment a search returns results (> 0). This
   * is the SINGLE place a recent search is recorded, so the Search page and
   * the ⌘K palette save on the exact same trigger — no results, no save. */
  onResults?: (query: string) => void
) {
  const [hits, setHits] = React.useState<SearchHit[]>([]);
  const [loading, setLoading] = React.useState(false);
  const [committedQuery, setCommittedQuery] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  // Hold the latest callback without re-triggering the debounce effect.
  const onResultsRef = React.useRef(onResults);
  onResultsRef.current = onResults;

  React.useEffect(() => {
    const trimmed = query.trim();
    // While the user is still typing, no search is running yet — clear any
    // stale loading so the spinner only appears once the debounced search
    // actually fires (i.e. after the user has stopped typing).
    setLoading(false);
    // 1-char minimum (aligned with the backend): a single CJK character
    // is a meaningful query.
    if (trimmed.length === 0) {
      setHits([]);
      setCommittedQuery("");
      setError(null);
      return;
    }
    let cancelled = false;
    const handle = window.setTimeout(() => {
      setLoading(true);
      invoke<SearchHit[]>("search_all_sessions", { query: trimmed })
        .then((result) => {
          if (cancelled) return;
          setHits(result);
          setCommittedQuery(trimmed);
          setError(null);
          if (result.length > 0) onResultsRef.current?.(trimmed);
        })
        .catch((err) => {
          if (!cancelled) setError(String(err));
        })
        .finally(() => {
          if (!cancelled) setLoading(false);
        });
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
  }, [query]);

  return { hits, loading, committedQuery, error };
}
