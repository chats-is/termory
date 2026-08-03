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
  error,
  accountSync = null
}: {
  syncing: boolean;
  lastSyncedAt: number | null;
  error: string | null;
  /**
   * Last reportable outcome of the backend's account auto-sync, which
   * nothing the user did started. `at` advances per outcome, so a repeat
   * of the same one still shows.
   *
   * It rides the states this footer already has — the success pulse and
   * the failure state, same icons, colours and wording — because it
   * answers the same question ("is what I'm looking at current?"), not a
   * second kind of status. What it must NOT do is move `lastSyncedAt`:
   * that clock dates the session scan, driven by the filesystem watcher,
   * and an account sync neither re-scans sessions nor shares its timing.
   */
  accountSync?: { at: number; error: string | null } | null;
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

  // The account sync's outcome, shown for a beat and then dropped. A
  // failure lingers longer: it carries a reason worth hovering for. Neither
  // parks in the footer, which exists to report the session scan — and the
  // next pass fixes a failure anyway.
  const accountFailWindow = 5000;
  const [accountShown, setAccountShown] = React.useState<{
    error: string | null;
  } | null>(null);
  const syncAt = accountSync?.at;
  const syncError = accountSync?.error ?? null;
  // Keyed on the PRIMITIVES: React clears a pending timeout before re-running
  // an effect, so depending on the object would let any parent re-render that
  // rebuilt an equal one cut the pulse short mid-show.
  const prevAccountAt = React.useRef(syncAt);
  React.useEffect(() => {
    if (syncAt == null || prevAccountAt.current === syncAt) {
      prevAccountAt.current = syncAt;
      return;
    }
    prevAccountAt.current = syncAt;
    setAccountShown({ error: syncError });
    const timer = window.setTimeout(
      () => setAccountShown(null),
      syncError ? accountFailWindow : justSyncedWindow
    );
    return () => window.clearTimeout(timer);
  }, [syncAt, syncError]);

  let state: "idle" | "syncing" | "done" | "error" = "idle";
  let icon: React.ReactNode = null;
  let label = "";
  // ONLY a failure gets a tooltip. The label already answers the question
  // the footer exists for ("how fresh is this?") in the form anyone
  // actually wants — "2m ago" — and an exact timestamp on hover adds a
  // precision nobody decides anything with. A failure is the opposite: the
  // label is just "Sync failed", and the reason has nowhere else to go.
  const tooltip = error ?? accountShown?.error ?? undefined;
  if (error) {
    state = "error";
    icon = <AlertTriangle size={12} strokeWidth={2.25} />;
    label = t("footer.syncFailed");
  } else if (syncing) {
    state = "syncing";
    icon = <RefreshCw size={12} strokeWidth={2.25} className="animate-spin" />;
    label = t("footer.syncing");
  } else if (accountShown) {
    // Under the session scan's own error and in-flight states — those
    // describe the data on screen right now — but over the generic success
    // pulse, which the idle label repeats a second later anyway.
    const failed = accountShown.error != null;
    state = failed ? "error" : "done";
    icon = failed ? (
      <AlertTriangle size={12} strokeWidth={2.25} />
    ) : (
      <Check size={12} strokeWidth={2.25} />
    );
    label = failed ? t("footer.syncFailed") : t("footer.syncedJustNow");
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
