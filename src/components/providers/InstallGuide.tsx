import React from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Check, Copy, ExternalLink, Loader2, RefreshCw, Terminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import { BrandIcon } from "@/components/BrandIcon";
import { CLI_APP_LABEL, CLI_APP_SOURCE_BADGE, CLI_INSTALL } from "@/constants";
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
  const [methodId, setMethodId] = React.useState(info.methods[0].id);
  const [copied, setCopied] = React.useState(false);

  // Reset to the first method when switching apps — different apps
  // expose different installer sets (npm/curl/brew/bun/paru).
  React.useEffect(() => {
    setMethodId(info.methods[0].id);
    setCopied(false);
  }, [app, info.methods]);

  const method =
    info.methods.find((m) => m.id === methodId) ?? info.methods[0];

  const copy = async () => {
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
          <div className="flex flex-wrap items-center gap-1 rounded-md bg-muted p-1">
            {info.methods.map((m) => (
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
          <div className="flex items-center gap-2 rounded-md outline outline-1 outline-foreground/10 bg-muted px-3 py-2">
            <Terminal className="size-3.5 shrink-0 text-muted-foreground" />
            <code className="flex-1 text-left text-xs font-mono break-all">
              {method.command}
            </code>
            <Button
              variant="ghost"
              size="icon-sm"
              type="button"
              onClick={() => void copy()}
              aria-label={t("install.copyCommand")}
              className="size-7"
            >
              {copied ? (
                <Check className="size-3.5 text-green-600" />
              ) : (
                <Copy className="size-3.5" />
              )}
            </Button>
          </div>
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
