import * as React from "react";
import { invoke } from "@tauri-apps/api/core";
import { Check, Loader2 } from "lucide-react";
import { toast } from "sonner";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useT } from "@/i18n";
import { cn } from "@/lib/utils";

/** One recent Codex project from the `recent_codex_projects` IPC. */
export type RecentCodexProject = {
  project: string;
  updated_at: number;
  session_count: number;
  providers: string[];
};

type FollowResult = { moved: number };

export type CodexFollowTarget = {
  /** model_provider id the selected projects' sessions should follow to. */
  providerId: string;
  /** Human label for the destination ("Official" or the provider name). */
  label: string;
  /** Recent projects (with sessions), prefetched by the caller. */
  projects: RecentCodexProject[];
  /**
   * Activates the provider. On "Migrate" we ACTIVATE FIRST (this), surface the
   * activation toast, THEN migrate the selected projects and surface the
   * migration toast. "Skip" activates without migrating; closing aborts.
   * Returns true on success — migration only runs when activation landed.
   */
  activate: () => Promise<boolean>;
};

function basename(path: string): string {
  const parts = path.replace(/[/\\]+$/, "").split(/[/\\]/);
  return parts[parts.length - 1] || path;
}

/**
 * Switch-time picker: after Codex is switched to a new provider, let the user
 * choose which recent projects' sessions should "follow" the switch — i.e.
 * have their model_provider re-tagged so `codex resume` lists them under the
 * now-active provider. The caller (`maybePromptThenActivate`) already filters
 * to projects with at least one off-target session, so every listed project
 * has something to move; all rows start unchecked.
 */
export function CodexFollowDialog({
  target,
  onClose
}: {
  target: CodexFollowTarget | null;
  onClose: () => void;
}) {
  const t = useT();
  const open = target !== null;
  const [running, setRunning] = React.useState(false);
  const [selected, setSelected] = React.useState<Set<string>>(new Set());
  const projects = target?.projects ?? [];

  React.useEffect(() => {
    // Nothing selected by default — the user opts each project in explicitly.
    if (target) setSelected(new Set());
  }, [target]);

  const toggle = (project: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(project)) next.delete(project);
      else next.add(project);
      return next;
    });
  };

  // "Activate": ACTIVATE FIRST (surfaces the activation toast), THEN keep the
  // selected projects' sessions in the now-active bucket (surfaces the "kept"
  // toast). If keeping fails (e.g. Codex is running and holds the DB lock)
  // activation has already happened — the error toast tells the user to quit
  // Codex and retry.
  const handleKeep = async () => {
    if (!target) return;
    const chosen = projects
      .map((p) => p.project)
      .filter((p) => selected.has(p));
    setRunning(true);
    try {
      const activated = await target.activate();
      // Only re-tag if activation actually landed — otherwise it would hide the
      // sessions under a bucket that isn't active.
      if (activated && chosen.length > 0) {
        const result = await invoke<FollowResult>("follow_codex_sessions", {
          projects: chosen,
          targetProviderId: target.providerId
        });
        toast.success(
          t("providers.followDone", { count: String(result.moved) })
        );
      }
      onClose();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setRunning(false);
    }
  };

  // "Skip": activate the provider without migrating any sessions.
  const handleSkip = async () => {
    if (!target) return;
    setRunning(true);
    try {
      await target.activate();
      onClose();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setRunning(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(o) => !o && !running && onClose()}>
      <DialogContent
        className="sm:max-w-2xl"
        onPointerDownOutside={(e) => running && e.preventDefault()}
        onEscapeKeyDown={(e) => running && e.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>{t("providers.followTitle")}</DialogTitle>
          <DialogDescription>
            {t("providers.followDesc", { label: target?.label ?? "" })}
          </DialogDescription>
          <p className="text-sm text-muted-foreground">
            {t("menu.exitCliHint", { app: "Codex" })}
          </p>
        </DialogHeader>

        <div className="-mx-6 max-h-[45vh] overflow-y-auto px-6 py-1">
          {projects.length === 0 ? (
            <p className="py-6 text-center text-sm text-muted-foreground">
              {t("providers.followEmpty")}
            </p>
          ) : (
            <ul className="flex flex-col gap-1">
              {projects.map((p) => {
                const checked = selected.has(p.project);
                return (
                  <li key={p.project}>
                    <button
                      type="button"
                      onClick={() => toggle(p.project)}
                      className={cn(
                        "flex w-full items-center gap-3 rounded-md px-2 py-2 text-left select-none",
                        "hover:bg-accent/60 transition-colors"
                      )}
                    >
                      <span
                        className={cn(
                          "flex size-4 shrink-0 items-center justify-center rounded border",
                          checked
                            ? "bg-primary border-primary text-primary-foreground"
                            : "border-input"
                        )}
                      >
                        {checked && <Check className="size-3" />}
                      </span>
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-sm font-medium">
                          {basename(p.project)}
                        </span>
                        <span className="block truncate text-xs text-muted-foreground">
                          {p.project}
                        </span>
                      </span>
                      <span className="shrink-0 text-xs text-muted-foreground">
                        {t("providers.followCount", {
                          count: String(p.session_count)
                        })}
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>

        <DialogFooter>
          <Button
            variant="ghost"
            onClick={() => void handleSkip()}
            disabled={running}
          >
            {t("providers.followSkip")}
          </Button>
          <Button
            onClick={() => void handleKeep()}
            disabled={running || selected.size === 0}
          >
            {running && <Loader2 className="size-4 animate-spin" />}
            {t("providers.followApply")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
