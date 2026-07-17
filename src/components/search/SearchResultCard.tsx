import type { SearchHit } from "@/types";
import { SearchResultRow } from "@/components/search/SearchResultRow";

export function SearchResultCard({
  hit,
  query,
  onOpen
}: {
  hit: SearchHit;
  query: string;
  onOpen: () => void;
}) {
  return (
    <button
      onClick={onOpen}
      className="w-full text-left rounded-md bg-card px-2 py-2 transition-colors flex flex-col gap-1 hover:bg-accent/40"
    >
      <SearchResultRow hit={hit} query={query} />
    </button>
  );
}
