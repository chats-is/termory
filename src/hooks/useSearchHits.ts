import React from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SearchHit } from "../types";

export function useSearchHits(query: string) {
  const [hits, setHits] = React.useState<SearchHit[]>([]);
  const [loading, setLoading] = React.useState(false);
  const [committedQuery, setCommittedQuery] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);

  React.useEffect(() => {
    const trimmed = query.trim();
    // 1-char minimum (aligned with the backend): a single CJK character
    // is a meaningful query.
    if (trimmed.length === 0) {
      setHits([]);
      setCommittedQuery("");
      setLoading(false);
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
        })
        .catch((err) => {
          if (!cancelled) setError(String(err));
        })
        .finally(() => {
          if (!cancelled) setLoading(false);
        });
    }, 300);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
  }, [query]);

  return { hits, loading, committedQuery, error };
}
