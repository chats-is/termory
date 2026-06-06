import React from "react";
import { AlertTriangle, Check, CircleCheckBig, CircleOff, Loader2, Pencil, Trash2, Zap } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { maskKey } from "@/lib/provider-utils";
import { OPENCODE_DEFAULT_NPM } from "@/constants";
import type { Provider, TestResult } from "@/types";

function ProviderFavicon({
  favicon,
  name
}: {
  favicon?: string;
  name?: string;
}) {
  // The editor caches the favicon as a `data:image/...;base64,...`
  // URL into providers.json when the user creates or edits the entry.
  // Rendering from that cache means: no live network request per
  // mount, no hostname disclosure to any third party, works offline.
  // Empty / undefined → letter avatar fallback.
  const [errored, setErrored] = React.useState(false);
  if (favicon && !errored) {
    return (
      <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-background shadow-sm">
        <img
          src={favicon}
          alt=""
          className="size-5 rounded-sm"
          onError={() => setErrored(true)}
        />
      </span>
    );
  }
  const letter = (name?.trim()[0] ?? "?").toUpperCase();
  return (
    <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-primary/15 text-primary text-base font-medium shadow-sm">
      {letter}
    </span>
  );
}

export function ProviderCard({
  provider,
  isConfigured,
  isInUse,
  toggling,
  settingDefault,
  testing,
  testResult,
  activatable = true,
  gatewayBadge,
  onToggleEnabled,
  onSetDefault,
  onEdit,
  onDelete,
  onTest
}: {
  provider: Provider;
  // When set, this card represents a gateway binding rather than a
  // standalone provider — shows an "AI Gateway" badge so the user can tell
  // where it comes from (managed in the Gateways tab). The value is the
  // gateway name, used only as a truthy flag (the name already shows as
  // the card title).
  gatewayBadge?: string;
  // OpenCode: slot exists in opencode.json. Other CLIs: same as isInUse
  // (single-slot — Enabled ≡ In use, so the Enable concept doesn't
  // surface separately).
  isConfigured: boolean;
  // Universal: CLI is currently using this provider.
  isInUse: boolean;
  // OpenCode-only pending state for the Enable/Disable toggle.
  toggling: boolean;
  settingDefault: boolean;
  testing: boolean;
  testResult: TestResult | undefined;
  // False when the underlying CLI binary is missing from PATH — Set as
  // default / Enable toggle are hard-disabled because writing the live
  // config has no effect without a CLI to consume it. Edit / Delete /
  // Test stay enabled (data management, not activation).
  activatable?: boolean;
  // OpenCode-only: toggle the slot in opencode.json. Undefined for
  // other CLIs (their Enabled state isn't separately controllable).
  onToggleEnabled?: () => void;
  onSetDefault: () => void;
  // Edit / Delete are omitted for gateway-binding cards — those are
  // managed from the Gateways tab, not from the per-CLI source list.
  onEdit?: () => void;
  onDelete?: () => void;
  onTest: () => void;
}) {
  const isOpencode = provider.app === "opencode";
  return (
    <Card
      className={cn(
        "p-3 gap-0 outline outline-1 outline-transparent shadow-sm",
        // Active: left accent stripe (Mac Finder selected-row style)
        //   + faint primary tint. Avoids dominating the whole card
        //   with solid primary the way smaller surfaces (sidebar rows
        //   etc.) can get away with.
        // Inactive: standard card surface with hover affordance.
        isInUse
          ? // Active accent stripe drawn as an overlay (::before) so it
            // adds NO box width — content stays aligned with inactive cards.
            "relative overflow-hidden bg-primary/5 before:content-[''] before:absolute before:inset-y-0 before:left-0 before:w-1 before:bg-primary"
          : "bg-card hover:bg-accent/40 transition-colors"
      )}
    >
      <CardContent className="px-0 flex flex-col gap-2">
        <div className="flex items-start justify-between gap-3 flex-wrap min-h-7">
          <ProviderFavicon favicon={provider.favicon} name={provider.name} />
          <div className="flex-1 min-w-0 flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <h3 className="text-lg font-medium">
                {provider.name || "(unnamed)"}
              </h3>
              {isInUse && (
                <Badge className="uppercase text-[9px] tracking-wide px-1.5 py-0">
                  In use
                </Badge>
              )}
              {gatewayBadge && (
                <Badge
                  variant="outline"
                  className="text-[9px] tracking-wide px-1.5 py-0"
                >
                  AI Gateway
                </Badge>
              )}
            </div>
            {(provider.baseUrl ||
              provider.apiKey ||
              provider.model ||
              provider.app === "opencode") && (
              <dl className="grid grid-cols-[max-content_1fr] gap-x-3.5 gap-y-1 text-xs">
                {provider.baseUrl && (
                  <>
                    <dt className="text-muted-foreground">Base URL</dt>
                    <dd className="font-mono break-all">{provider.baseUrl}</dd>
                  </>
                )}
                {provider.apiKey && (
                  <>
                    <dt className="text-muted-foreground">API key</dt>
                    <dd className="font-mono break-all">{maskKey(provider.apiKey)}</dd>
                  </>
                )}
                {provider.model && (
                  <>
                    <dt className="text-muted-foreground">Model</dt>
                    <dd className="font-mono break-all">{provider.model}</dd>
                  </>
                )}
                {provider.app === "opencode" && (
                  <>
                    <dt className="text-muted-foreground">AI SDK</dt>
                    <dd className="font-mono break-all">
                      {provider.npm || OPENCODE_DEFAULT_NPM}
                    </dd>
                  </>
                )}
              </dl>
            )}
          </div>
          <div className="flex flex-col items-end gap-1.5 shrink-0">
            <div className="inline-flex items-center gap-1.5">
            {onToggleEnabled && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={onToggleEnabled}
                    disabled={toggling || !activatable}
                    aria-label={isConfigured ? "Disable" : "Enable"}
                    className="inline-flex items-center justify-center size-8 rounded-md hover:bg-accent hover:text-accent-foreground disabled:opacity-50 disabled:pointer-events-none transition-colors"
                  >
                    {toggling ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : isConfigured ? (
                      <CircleCheckBig className="size-4 text-green-600" />
                    ) : (
                      <CircleOff className="size-4 text-red-600" />
                    )}
                  </button>
                </TooltipTrigger>
                <TooltipContent side="bottom">
                  {!activatable
                    ? "Install it first."
                    : isConfigured
                      ? "Disable"
                      : "Enable"}
                </TooltipContent>
              </Tooltip>
            )}
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  onClick={onTest}
                  disabled={testing}
                  aria-label="Test"
                  className="inline-flex items-center justify-center size-8 rounded-md hover:bg-accent hover:text-accent-foreground disabled:opacity-50 disabled:pointer-events-none transition-colors"
                >
                  {testing ? <Loader2 className="size-4 animate-spin" /> : <Zap className="size-4" />}
                </button>
              </TooltipTrigger>
              <TooltipContent side="bottom">Test</TooltipContent>
            </Tooltip>
            {onEdit && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={onEdit}
                    aria-label="Edit"
                    className="inline-flex items-center justify-center size-8 rounded-md hover:bg-accent hover:text-accent-foreground transition-colors"
                  >
                    <Pencil className="size-4" />
                  </button>
                </TooltipTrigger>
                <TooltipContent side="bottom">Edit</TooltipContent>
              </Tooltip>
            )}
            {onDelete && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={onDelete}
                    aria-label="Delete"
                    className="inline-flex items-center justify-center size-8 rounded-md text-destructive hover:text-destructive hover:bg-destructive/10 transition-colors"
                  >
                    <Trash2 className="size-4" />
                  </button>
                </TooltipTrigger>
                <TooltipContent side="bottom">Delete</TooltipContent>
              </Tooltip>
            )}
            </div>
            {!isInUse && (
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={onSetDefault}
                disabled={settingDefault || !activatable}
              >
                {isOpencode
                  ? settingDefault
                    ? "Setting…"
                    : "Set as default"
                  : settingDefault
                    ? "Activating…"
                    : "Activate"}
              </Button>
            )}
          </div>
        </div>
        {testResult && (
          <div
            className={cn(
              "flex items-center gap-2 text-xs px-2.5 py-1.5 rounded-md",
              testResult.ok
                ? "bg-primary/10 text-primary"
                : "bg-destructive/10 text-destructive"
            )}
          >
            {testResult.ok ? <Check className="size-3.5" /> : <AlertTriangle className="size-3.5" />}
            <span>
              {testResult.status ? `HTTP ${testResult.status}` : "no response"} ·{" "}
              {testResult.latencyMs}ms · {testResult.message}
            </span>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
