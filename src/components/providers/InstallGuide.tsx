import React from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Check, Copy, ExternalLink, Loader2, RefreshCw, Terminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { BrandIcon } from "@/components/BrandIcon";
import { CLI_APP_LABEL, CLI_APP_SOURCE_BADGE, CLI_INSTALL } from "@/constants";
import { IS_MAC, IS_WINDOWS } from "@/lib/platform";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";
import type { CliApp } from "@/types";

export function InstallGuide({
  app,
  rechecking,
  onRecheck
}: {
  app: CliApp;
  rechecking: boolean;
  onRecheck: () => void;
}) {
  const t = useT();
  const info = CLI_INSTALL[app];
  const label = CLI_APP_LABEL[app];
  const platform = IS_WINDOWS ? "windows" : IS_MAC ? "mac" : "linux";
  // Each installer declares its supported OSes where needed: e.g. Homebrew
  // is macOS-only, while Codex has distinct Unix and PowerShell installers.
  const methods = React.useMemo(
    () =>
      info.methods.filter(
        (m) => !m.platforms || m.platforms.includes(platform)
      ),
    [info.methods, platform]
  );
  const [methodId, setMethodId] = React.useState(methods[0]?.id ?? "");
  const [copied, setCopied] = React.useState(false);

  // Reset to the first method when switching apps — different apps
  // expose different installer sets (npm/curl/brew/bun/paru).
  React.useEffect(() => {
    setMethodId(methods[0]?.id ?? "");
    setCopied(false);
  }, [app, methods]);

  const method = methods.find((m) => m.id === methodId) ?? methods[0];
  const isDownloadUrl = !!method && /^https?:\/\//.test(method.command);

  const copy = async () => {
    if (!method) return;
    try {
      await navigator.clipboard.writeText(method.command);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard blocked — ignore */
    }
  };

  return (
    <div className="flex-1 min-h-0 overflow-auto px-6 pt-10 pb-6">
      <div className="mx-auto flex flex-col items-center gap-4 max-w-md text-center">
        <BrandIcon source={CLI_APP_SOURCE_BADGE[app]} className="size-12" />
        <div className="flex flex-col gap-1.5">
          <h2 className="text-lg font-medium">{t("install.notInstalled", { name: label })}</h2>
          <p className="text-sm text-muted-foreground leading-relaxed">
            {t("install.desc", { name: label })}
          </p>
        </div>

        <div className="w-full flex flex-col gap-2">
          {methods.length > 1 && (
            <div className="flex flex-wrap items-center gap-1 rounded-md bg-muted p-1">
              {methods.map((m) => (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => {
                    setMethodId(m.id);
                    setCopied(false);
                  }}
                  className={cn(
                    "h-7 px-2.5 rounded text-xs font-medium transition-colors",
                    m.id === methodId
                      ? "bg-background shadow-sm text-foreground"
                      : "text-muted-foreground hover:text-foreground"
                  )}
                >
                  {m.label}
                </button>
              ))}
            </div>
          )}
          {method && <div className="flex items-center gap-2 rounded-md outline outline-1 outline-foreground/10 bg-muted px-3 py-2">
            <Terminal className="size-4 shrink-0 text-muted-foreground" />
            {/* Single line, horizontal scroll on overflow — install
                commands/URLs must never soft-wrap mid-token. */}
            <code className="flex-1 min-w-0 overflow-x-auto whitespace-nowrap text-left text-xs font-mono">
              {method.command}
            </code>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  type="button"
                  onClick={() =>
                    isDownloadUrl ? void openUrl(method.command) : void copy()
                  }
                  aria-label={
                    isDownloadUrl ? t("install.openDownload") : t("install.copyCommand")
                  }
                  className="size-7"
                >
                  {isDownloadUrl ? (
                    <ExternalLink className="size-4" />
                  ) : copied ? (
                    <Check className="size-4 text-green-600" />
                  ) : (
                    <Copy className="size-4" />
                  )}
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">
                {isDownloadUrl ? t("install.openDownload") : t("install.copyCommand")}
              </TooltipContent>
            </Tooltip>
          </div>}
        </div>

        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => void openUrl(info.url)}
          >
            <ExternalLink className="size-4" />
            {t("install.docs")}
          </Button>
          <Button
            type="button"
            size="sm"
            disabled={rechecking}
            onClick={onRecheck}
          >
            {rechecking ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <RefreshCw className="size-4" />
            )}
            {rechecking ? t("install.checking") : t("providers.recheck")}
          </Button>
        </div>
      </div>
    </div>
  );
}
