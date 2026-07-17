import { X } from "lucide-react";
import { useT } from "@/i18n";

/** THE clear-input button for search inputs — one component shared by
 * the Search page and the ⌘K palette (styled after the native
 * `type="search"` cancel control: grey circle + bold white ✕).
 * Caller renders it inside a `relative` container and shows it only
 * while the query is non-empty. mousedown is prevented so focus stays
 * in the input. */
export function SearchClearButton({ onClear }: { onClear: () => void }) {
  const t = useT();
  return (
    <button
      type="button"
      aria-label={t("search.clear")}
      onMouseDown={(event) => event.preventDefault()}
      onClick={onClear}
      className="absolute right-3 top-1/2 -translate-y-1/2 rounded-full bg-muted-foreground/45 p-[2px] text-background hover:bg-muted-foreground/65 transition-colors"
    >
      <X className="size-2.5" strokeWidth={3} aria-hidden />
    </button>
  );
}
