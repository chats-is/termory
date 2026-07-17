import React from "react";
import { MessageBody } from "@/components/MessageBody";
import { TranscriptFindBar } from "@/components/TranscriptFindBar";

/** Detail body for Memory / Skill documents — a single markdown doc,
 * not a virtualized transcript. ⌘F find works at OCCURRENCE level
 * here: the whole doc is in the DOM (no virtualization), so every
 * rendered `<mark>` from MessageBody's highlight pass is countable;
 * prev/next moves a `data-current` attribute across them (orange via
 * styles.css) and scrolls the current one into view. Sessions can't
 * do this — their rows mount/unmount with the virtualizer — which is
 * why MessageList navigates per-message instead. */
export function DocDetailView({
  text,
  findOpen,
  findQuery,
  onQueryChange,
  onClose,
  focusNonce
}: {
  text: string;
  findOpen: boolean;
  findQuery: string;
  onQueryChange: (query: string) => void;
  onClose: () => void;
  focusNonce: number;
}) {
  const containerRef = React.useRef<HTMLDivElement>(null);
  const [pos, setPos] = React.useState(0);
  const [total, setTotal] = React.useState(0);
  const query = findOpen ? findQuery.trim() : "";

  // Restart at the first occurrence whenever the query or doc changes.
  React.useLayoutEffect(() => {
    setPos(0);
  }, [query, text]);

  // ONE effect, ONE DOM snapshot: count the rendered marks, clamp the
  // position against that same snapshot, then tag/scroll the current
  // mark. Counting and applying from a single querySelectorAll means
  // the pos+1/total counter can never disagree with which mark is
  // actually highlighted (the previous split-effect version could
  // desync when the doc text changed between the two effects).
  React.useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const marks = container.querySelectorAll("mark");
    setTotal(marks.length);
    const clamped = marks.length ? Math.min(pos, marks.length - 1) : 0;
    if (clamped !== pos) {
      setPos(clamped); // re-runs this effect with a valid position
      return;
    }
    marks.forEach((mark) => mark.removeAttribute("data-current"));
    const current = marks[clamped];
    if (current) {
      current.setAttribute("data-current", "");
      current.scrollIntoView({ block: "center" });
    }
  }, [pos, query, text]);

  const next = React.useCallback(() => {
    if (total > 0) setPos((p) => (p + 1) % total);
  }, [total]);
  const prev = React.useCallback(() => {
    if (total > 0) setPos((p) => (p - 1 + total) % total);
  }, [total]);

  return (
    <>
      {findOpen && (
        <TranscriptFindBar
          query={findQuery}
          onQueryChange={onQueryChange}
          position={pos}
          total={total}
          onNext={next}
          onPrev={prev}
          onClose={onClose}
          focusNonce={focusNonce}
        />
      )}
      <div ref={containerRef} className="flex-1 overflow-auto px-4 py-2">
        <div className="rounded-lg bg-card text-card-foreground px-5 py-4">
          <MessageBody text={text} highlight={query || undefined} />
        </div>
      </div>
    </>
  );
}
