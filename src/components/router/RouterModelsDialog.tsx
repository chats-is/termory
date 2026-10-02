import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { Copy, RefreshCw, Search } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { copyToClipboard } from "@/lib/clipboard";
import { filterRouterModels } from "@/lib/router-utils";
import { useT } from "@/i18n";
import type { RouterModel } from "@/types";

/**
 * The models the router offers — what `/v1/models` serves, grouped per model
 * with the enabled sources that list it (`router_models`). A model every
 * listing source is cooling down for is shown, marked, rather than hidden.
 */
export function RouterModelsDialog({
  open,
  onOpenChange
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const t = useT();
  const [models, setModels] = React.useState<RouterModel[] | null>(null);
  const [loading, setLoading] = React.useState(false);
  const [query, setQuery] = React.useState("");

  const load = React.useCallback(async (force: boolean) => {
    setLoading(true);
    try {
      setModels(await invoke<RouterModel[]>("router_models", { force }));
    } catch (err) {
      toast.error(String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  React.useEffect(() => {
    if (open) {
      setQuery("");
      void load(false);
    }
  }, [open, load]);

  const shown = filterRouterModels(models ?? [], query);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{t("router.models.title")}</DialogTitle>
          <DialogDescription>{t("router.models.desc")}</DialogDescription>
        </DialogHeader>
        <div className="relative">
          <Search className="size-4 absolute left-2.5 top-1/2 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("router.models.search")}
            aria-label={t("router.models.search")}
            className="pl-8"
          />
        </div>
        {/* The count bar sits right on top of the list it counts. */}
        <div className="flex flex-col gap-1">
          <div className="flex items-center justify-between gap-2 rounded-md bg-muted px-2 py-0.5 text-xs text-muted-foreground">
            <span>
              {models === null
                ? " "
                : t("router.models.count", {
                    shown: String(shown.length),
                    total: String(models.length)
                  })}
            </span>
            <Tooltip>
              <TooltipTrigger asChild>
                <span className="inline-flex">
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    disabled={loading}
                    aria-label={t("router.refresh")}
                    onClick={() => void load(true)}
                  >
                    <RefreshCw className={loading ? "size-4 animate-spin" : "size-4"} />
                  </Button>
                </span>
              </TooltipTrigger>
              <TooltipContent side="left">{t("router.refresh")}</TooltipContent>
            </Tooltip>
          </div>
          <div className="max-h-[50vh] overflow-y-auto -mx-1 px-1 flex flex-col gap-1">
            {models === null ? (
              [0, 1, 2, 3].map((i) => <Skeleton key={i} className="h-10 w-full rounded-md" />)
            ) : shown.length === 0 ? (
              <p className="text-xs text-muted-foreground py-4 text-center">
                {models.length === 0 ? t("router.models.empty") : t("router.models.noMatch")}
              </p>
            ) : (
              shown.map((m) => (
                <div
                  key={m.id}
                  className="rounded-md border px-2 py-1.5 flex items-center gap-2 text-sm"
                >
                  <div className="flex flex-col min-w-0 flex-1">
                    <span className="font-mono truncate">{m.id}</span>
                    <span className="text-xs text-muted-foreground truncate">
                      {m.sources.join(" · ")}
                    </span>
                  </div>
                  {!m.available && (
                    <Badge variant="outline" className="shrink-0 text-[10px]">
                      {t("router.models.cooling")}
                    </Badge>
                  )}
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={t("router.models.copy")}
                        onClick={() =>
                          void copyToClipboard(m.id).then(
                            () => toast.success(t("common.copied")),
                            (err) => toast.error(String(err))
                          )
                        }
                      >
                        <Copy className="size-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent side="left">{t("router.models.copy")}</TooltipContent>
                  </Tooltip>
                </div>
              ))
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
