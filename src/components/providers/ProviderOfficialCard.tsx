import React from "react";
import { ArrowUp } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { BrandIcon } from "@/components/BrandIcon";
import { CLI_APP_SOURCE_BADGE } from "@/constants";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";
import type { VersionSegment } from "@/lib/provider-utils";
import type { CliApp } from "@/types";

export function ProviderOfficialCard({
  app,
  isInUse,
  settingDefault,
  versions,
  versionLoading = false,
  actions,
  onSetDefault
}: {
  app: CliApp;
  isInUse: boolean;
  settingDefault: boolean;
  /** One entry per installed component, joined with " · ". Usually a
   *  single unlabeled version; Codex has two (CLI + App). Each carries
   *  its own `latest`, so the update badge renders after the component
   *  it actually applies to. Empty = nothing installed/known ("—"). */
  versions?: VersionSegment[];
  versionLoading?: boolean;
  /** Optional slot rendered between the info block and the Activate button. */
  actions?: React.ReactNode;
  onSetDefault: () => void;
}) {
  const t = useT();
  return (
    <Card
      className={cn(
        "p-3 gap-0 outline outline-1 outline-transparent shadow-sm",
        isInUse
          ? "relative overflow-hidden bg-primary/5 before:content-[''] before:absolute before:inset-y-0 before:left-0 before:w-1 before:bg-primary"
          : "bg-card hover:bg-accent/40 transition-colors"
      )}
    >
      <CardContent className="px-0 flex items-center gap-3 min-h-7">
        <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-background shadow-sm [&_svg]:size-5">
          <BrandIcon source={CLI_APP_SOURCE_BADGE[app]} />
        </span>
        <div className="min-w-0 flex-1 flex flex-col">
          <div className="flex items-center gap-2">
            <h3 className="text-lg font-medium">{t("providers.official")}</h3>
            {isInUse && (
              <Badge className="uppercase text-[9px] tracking-wide px-1.5 py-0">
                {t("providers.inUse")}
              </Badge>
            )}
          </div>
          <p className="text-xs text-muted-foreground leading-snug flex items-center gap-1.5 flex-wrap">
            <span>Version</span>
            {versionLoading ? (
              <span className="inline-block w-12 h-3 align-middle rounded bg-muted-foreground/15 animate-pulse" />
            ) : versions && versions.length > 0 ? (
              versions.map((seg, i) => (
                <React.Fragment key={`${seg.label ?? ""}-${seg.text}`}>
                  {i > 0 && <span aria-hidden>·</span>}
                  <span className="font-mono">
                    {seg.text}
                    {seg.label ? ` (${seg.label})` : ""}
                  </span>
                  {/* Badge belongs to THIS component, not the line — a
                      Codex CLI update must not read as an App update. */}
                  {seg.latest && (
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Badge
                          variant="outline"
                          className="gap-0.5 px-1.5 py-0 border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-400"
                        >
                          <ArrowUp className="size-3" />
                          <span className="font-mono">
                            {t("providers.updateAvailable", {
                              version: `v${seg.latest}`
                            })}
                          </span>
                        </Badge>
                      </TooltipTrigger>
                      <TooltipContent side="top">
                        {t("providers.updateAvailableHint")}
                      </TooltipContent>
                    </Tooltip>
                  )}
                </React.Fragment>
              ))
            ) : (
              <span className="font-mono">—</span>
            )}
          </p>
        </div>
        {actions}
        {!isInUse && (
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={onSetDefault}
            disabled={settingDefault}
          >
            {settingDefault ? t("providers.activating") : t("providers.activate")}
          </Button>
        )}
      </CardContent>
    </Card>
  );
}
