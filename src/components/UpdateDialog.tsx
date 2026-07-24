import React from "react";
import { relaunch } from "@tauri-apps/plugin-process";
import type { Update } from "@tauri-apps/plugin-updater";
import { Loader2, Sparkles } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { toast } from "sonner";
import { useT } from "@/i18n";
import { MessageBody } from "@/components/MessageBody";

export function UpdateDialog({
  update,
  currentVersion,
  onClose
}: {
  update: Update | null;
  currentVersion: string;
  onClose: () => void;
}) {
  const t = useT();
  const [phase, setPhase] = React.useState<"downloading" | "installing" | null>(null);
  const [progress, setProgress] = React.useState<{ downloaded: number; total: number | null } | null>(
    null
  );

  const busy = phase !== null;
  const open = update !== null;

  const handleInstall = async () => {
    if (!update) return;
    setPhase("downloading");
    setProgress({ downloaded: 0, total: null });
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") {
          setProgress({ downloaded: 0, total: event.data.contentLength ?? null });
        } else if (event.event === "Progress") {
          setProgress((prev) =>
            prev
              ? { downloaded: prev.downloaded + event.data.chunkLength, total: prev.total }
              : null
          );
        } else if (event.event === "Finished") {
          // Download complete — the updater now applies the bundle.
          setProgress((prev) => (prev ? { ...prev, downloaded: prev.total ?? prev.downloaded } : null));
          setPhase("installing");
        }
      });
      toast.success(t("update.installed"));
      await relaunch();
    } catch (err) {
      toast.error(t("update.installFailed", { error: String(err) }));
      setPhase(null);
      setProgress(null);
    }
  };

  const handleOpenChange = (next: boolean) => {
    if (!next && !busy) onClose();
  };

  const progressPct = (() => {
    if (!progress) return null;
    if (!progress.total || progress.total === 0) return null;
    return Math.min(100, Math.round((progress.downloaded / progress.total) * 100));
  })();

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        showCloseButton={!busy}
        className="sm:max-w-md"
        onPointerDownOutside={(event) => event.preventDefault()}
        onEscapeKeyDown={(event) => event.preventDefault()}
      >
        <DialogHeader>
          <div className="flex items-center gap-3 mb-2">
            <span className="inline-flex items-center justify-center size-10 rounded-md bg-primary/10 text-primary shadow-sm">
              <Sparkles className="size-5" />
            </span>
            <div className="flex flex-col">
              <DialogTitle>{t("update.available")}</DialogTitle>
              <DialogDescription className="font-mono">
                v{currentVersion || "?"} → v{update?.version ?? "?"}
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        {update?.body && (
          <div className="text-xs text-muted-foreground leading-relaxed max-h-56 overflow-auto rounded-md outline outline-1 outline-foreground/5 p-3">
            <MessageBody text={update.body} className="[&_h3]:mt-3 [&_h3]:mb-1 [&_h3:first-child]:mt-0 [&_h3]:text-sm [&_h3]:font-semibold [&_h3]:text-foreground [&_ul]:my-1 [&_li]:ml-3" />
          </div>
        )}

        {busy && (
          <div className="flex flex-col gap-2">
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>{phase === "installing" ? t("update.installing") : t("update.downloading")}</span>
              {phase === "downloading" && progressPct != null && (
                <span className="tabular-nums">{progressPct}%</span>
              )}
            </div>
            <div className="h-1.5 w-full rounded-full bg-muted overflow-hidden">
              {/* Always start at 0% and only grow — never a fake mid-track
                  position (or a segment that sweeps through the middle) that
                  then snaps back to 0. Unknown total → stays 0% until the
                  first chunk lands, which for the Tauri updater is ~immediate. */}
              <div
                className="h-full bg-primary transition-[width] duration-150"
                style={{ width: `${progressPct ?? 0}%` }}
              />
            </div>
          </div>
        )}

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            disabled={busy}
            onClick={onClose}
          >{t("update.later")}</Button>
          <Button
            type="button"
            disabled={busy}
            onClick={() => void handleInstall()}
          >
            {busy && <Loader2 className="size-4 animate-spin" />}
            {phase === "downloading"
              ? t("update.downloading")
              : phase === "installing"
                ? t("update.installing")
                : t("update.downloadInstall")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
