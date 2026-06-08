import React from "react";
import { relaunch } from "@tauri-apps/plugin-process";
import type { Update } from "@tauri-apps/plugin-updater";
import { Download, Loader2, Sparkles } from "lucide-react";
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
  const [installing, setInstalling] = React.useState(false);
  const [progress, setProgress] = React.useState<{ downloaded: number; total: number | null } | null>(
    null
  );

  const open = update !== null;

  const handleInstall = async () => {
    if (!update) return;
    setInstalling(true);
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
          setProgress((prev) => (prev ? { ...prev, downloaded: prev.total ?? prev.downloaded } : null));
        }
      });
      toast.success(t("update.installed"));
      await relaunch();
    } catch (err) {
      toast.error(t("update.installFailed", { error: String(err) }));
      setInstalling(false);
      setProgress(null);
    }
  };

  const handleOpenChange = (next: boolean) => {
    if (!next && !installing) onClose();
  };

  const progressPct = (() => {
    if (!progress) return null;
    if (!progress.total || progress.total === 0) return null;
    return Math.min(100, Math.round((progress.downloaded / progress.total) * 100));
  })();

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        showCloseButton={!installing}
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
          <div className="text-xs text-muted-foreground leading-relaxed whitespace-pre-wrap max-h-56 overflow-auto rounded-md outline outline-1 outline-foreground/5 p-3">
            {update.body}
          </div>
        )}

        {installing && (
          <div className="flex flex-col gap-2">
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>{t("update.downloading")}</span>
              {progressPct != null && <span className="tabular-nums">{progressPct}%</span>}
            </div>
            <div className="h-1.5 w-full rounded-full bg-muted overflow-hidden">
              {progressPct != null ? (
                // Known total → real progress, starting at 0%.
                <div
                  className="h-full bg-primary transition-[width] duration-150"
                  style={{ width: `${progressPct}%` }}
                />
              ) : (
                // Total / first chunk not known yet → indeterminate slide
                // (never sits at a fake position that then resets to 0).
                <div className="h-full w-2/5 rounded-full bg-primary animate-[progress-indeterminate_1.1s_ease-in-out_infinite]" />
              )}
            </div>
          </div>
        )}

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            disabled={installing}
            onClick={onClose}
          >{t("update.later")}</Button>
          <Button
            type="button"
            disabled={installing}
            onClick={() => void handleInstall()}
          >
            {installing ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <Download className="size-4" />
            )}
            {installing ? t("update.installing") : t("update.installNow")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
