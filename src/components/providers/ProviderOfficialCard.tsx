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
import { updateBadgeState } from "@/lib/provider-utils";
import type { UpdateBadgeState, VersionSegment } from "@/lib/provider-utils";
import type { CliApp } from "@/types";

const BADGE_TONE = {
  amber: "border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-400",
  red: "border-destructive/40 bg-destructive/10 text-destructive"
} as const;

const BADGE_HOVER = {
  amber: "hover:bg-amber-500/20",
  red: "hover:bg-destructive/20"
} as const;

/** The update badge — the ONLY thing on the card that reflects an
 *  upgrade, so the layout never shifts. Pure rendering: every decision
 *  comes from `updateBadgeState`, where the rules and their tests live.
 *
 *    amber `↑ New v1.2.3`  idle — click upgrades (tooltip: the command)
 *    amber `↑ Updating`    running — same badge, label swapped, disabled
 *    red   `↑ New v1.2.3`  last attempt failed — click retries; tooltip
 *                          gives the reason and names the command to run
 *                          by hand. That is TEXT, never a button: the
 *                          failures needing a human (sudo, opencode's
 *                          install-method prompt) belong in the user's
 *                          own terminal.
 */
function UpdateBadge({
  latest,
  command,
  state,
  error,
  onUpgrade
}: {
  latest: string;
  command?: string | null;
  state: UpdateBadgeState;
  error?: string;
  onUpgrade?: () => void;
}) {
  const t = useT();
  const className = cn(
    "gap-0.5 px-1.5 py-0",
    BADGE_TONE[state.tone],
    state.disabled && "opacity-60",
    state.clickable && !state.disabled && ["cursor-pointer", BADGE_HOVER[state.tone]]
  );
  const content = (
    <>
      <ArrowUp className="size-3" />
      {state.label === "updating" ? (
        <span>{t("providers.updateRunning")}</span>
      ) : (
        <span className="font-mono">
          {t("providers.updateAvailable", { version: `v${latest}` })}
        </span>
      )}
    </>
  );
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        {state.clickable ? (
          <Badge asChild variant="outline" className={className}>
            <button
              type="button"
              disabled={state.disabled}
              onClick={() => onUpgrade?.()}
            >
              {content}
            </button>
          </Badge>
        ) : (
          <Badge variant="outline" className={className}>
            {content}
          </Badge>
        )}
      </TooltipTrigger>
      <TooltipContent side="top">
        {state.tooltip === "failed" ? (
          <span className="flex flex-col gap-0.5">
            <span>{error}</span>
            <span className="opacity-80">{t("providers.updateRunItYourself")}</span>
            <code className="font-mono opacity-80">{command}</code>
          </span>
        ) : state.tooltip === "command" ? (
          <span className="flex flex-col gap-0.5">
            <span>{t("providers.updateClickToRun")}</span>
            <code className="font-mono opacity-80">{command}</code>
          </span>
        ) : (
          t("providers.updateAvailableHint")
        )}
      </TooltipContent>
    </Tooltip>
  );
}

export function ProviderOfficialCard({
  app,
  isInUse,
  settingDefault,
  versions,
  versionLoading = false,
  actions,
  onSetDefault,
  onUpgrade,
  upgrading = false,
  upgradeError
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
  /** Run this app's upgrade in-app. Takes no command: the backend
   *  decides what to run from the app alone, so passing one here would
   *  imply a choice the caller doesn't have. Omitted → badges stay
   *  informational. */
  onUpgrade?: () => void;
  /** An upgrade for THIS app is running. Only a segment that actually
   *  has an upgrade command reacts (see `updateBadgeState`). */
  upgrading?: boolean;
  /** Why the last attempt failed — turns that segment's badge red and
   *  becomes its tooltip. Never renders as its own row: the card's
   *  layout must not shift. */
  upgradeError?: string;
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
                  {/* Badge belongs to THIS segment, not the line — a
                      Codex CLI update must not read as an App update. */}
                  {seg.latest && (
                    <UpdateBadge
                      latest={seg.latest}
                      command={seg.upgradeCommand}
                      error={upgradeError}
                      onUpgrade={onUpgrade}
                      state={updateBadgeState(seg, {
                        upgrading,
                        error: upgradeError,
                        canUpgrade: !!onUpgrade
                      })}
                    />
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
