import React from "react";
import { AlertTriangle, Check, RefreshCw } from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { formatTimeAgo } from "@/lib/format";
import { useT } from "@/i18n";

export function FreshnessFooter({
  syncing,
  lastSyncedAt,
  error
}: {
  syncing: boolean;
  lastSyncedAt: number | null;
  error: string | null;
}) {
  const t = useT();
  // Bump every 30s so "Synced 2m ago" stays accurate without
  // re-rendering the rest of the app. tick is intentionally unused —
  // its only job is to invalidate the rendered label.
  const [, setTick] = React.useState(0);
  React.useEffect(() => {
    const id = window.setInterval(() => setTick((t) => t + 1), 30_000);
    return () => window.clearInterval(id);
  }, []);

  // Brief "just synced" pulse after a successful sync — gives the user
  // a passive cue that the background actually did something. After
  // ~1.8s the footer falls back to the idle "Synced 2m ago" state.
  // Triggers on any `lastSyncedAt` advance, so both launch-time scans
  // and watcher-driven re-scans get the cue.
  const justSyncedWindow = 1800;
  const [justSynced, setJustSynced] = React.useState(false);
  const prevSyncedAt = React.useRef(lastSyncedAt);
  React.useEffect(() => {
    if (
      lastSyncedAt != null &&
      prevSyncedAt.current !== lastSyncedAt &&
      !error
    ) {
      setJustSynced(true);
      const timer = window.setTimeout(() => setJustSynced(false), justSyncedWindow);
      prevSyncedAt.current = lastSyncedAt;
      return () => window.clearTimeout(timer);
    }
    prevSyncedAt.current = lastSyncedAt;
  }, [lastSyncedAt, error]);

  let state: "idle" | "syncing" | "done" | "error" = "idle";
  let icon: React.ReactNode = null;
  let label = "";
  // ONLY a failure gets a tooltip. The label already answers the question
  // the footer exists for ("how fresh is this?") in the form anyone
  // actually wants — "2m ago" — and an exact timestamp on hover adds a
  // precision nobody decides anything with. A failure is the opposite: the
  // label is just "Sync failed", and the reason has nowhere else to go.
  const tooltip = error ?? undefined;
  if (error) {
    state = "error";
    icon = <AlertTriangle size={12} strokeWidth={2.25} />;
    label = t("footer.syncFailed");
  } else if (syncing) {
    state = "syncing";
    icon = <RefreshCw size={12} strokeWidth={2.25} className="animate-spin" />;
    label = t("footer.syncing");
  } else if (justSynced) {
    state = "done";
    icon = <Check size={12} strokeWidth={2.25} />;
    label = t("footer.syncedJustNow");
  } else if (lastSyncedAt != null) {
    state = "idle";
    icon = <Check size={12} strokeWidth={2.25} />;
    label = t("footer.synced", { ago: formatTimeAgo(lastSyncedAt, t) });
  }

  const stateClass = {
    idle: "text-muted-foreground",
    syncing: "text-muted-foreground",
    done: "text-primary",
    error: "text-destructive"
  }[state];

  const footer = (
    <footer
      aria-label={label || t("footer.status")}
      className={cn(
        "absolute bottom-0 right-0 flex items-center gap-1.5 rounded-tl-md bg-sidebar px-3 py-1 text-[11px]",
        stateClass
      )}
    >
      <span className="shrink-0">{icon}</span>
      <span>{label}</span>
    </footer>
  );

  // The label is relative ("Synced 2m ago") or a one-line failure; the
  // hover carries what it can't: the exact local timestamp, or the full
  // error. Nothing to add in the syncing / never-synced states, which
  // render the bare footer.
  if (!tooltip) return footer;
  return (
    <Tooltip>
      <TooltipTrigger asChild>{footer}</TooltipTrigger>
      <TooltipContent side="top">{tooltip}</TooltipContent>
    </Tooltip>
  );
}
