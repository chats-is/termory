import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { BrandIcon } from "@/components/BrandIcon";
import { CLI_APP_SOURCE_BADGE } from "@/constants";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";
import type { CliApp } from "@/types";

export function ProviderOfficialCard({
  app,
  isInUse,
  settingDefault,
  version,
  versionLoading = false,
  onSetDefault
}: {
  app: CliApp;
  isInUse: boolean;
  settingDefault: boolean;
  // Installed CLI version parsed from `<bin> --version`. Null AFTER
  // detection ran but `--version` returned nothing parseable.
  version?: string | null;
  // True while the version detection is in flight (covers initial
  // mount and Recheck). When true the card renders a loading
  // placeholder instead of "—".
  versionLoading?: boolean;
  onSetDefault: () => void;
}) {
  const t = useT();
  return (
    <Card
      className={cn(
        "p-3 gap-0 outline outline-1 outline-transparent shadow-sm",
        isInUse
          ? // Active accent stripe drawn as an overlay (::before) so it adds
            // NO box width — content stays aligned with inactive cards.
            "relative overflow-hidden bg-primary/5 before:content-[''] before:absolute before:inset-y-0 before:left-0 before:w-1 before:bg-primary"
          : "bg-card hover:bg-accent/40 transition-colors"
      )}
    >
      <CardContent className="px-0 flex items-center gap-3 min-h-7">
        <span className="shrink-0 inline-flex items-center justify-center size-10 rounded-md bg-background shadow-sm [&_svg]:size-5">
          <BrandIcon source={CLI_APP_SOURCE_BADGE[app]} />
        </span>
        <div className="flex-1 min-w-0 flex flex-col">
          <div className="flex items-center gap-2">
            <h3 className="text-lg font-medium">{t("providers.official")}</h3>
            {isInUse && (
              <Badge className="uppercase text-[9px] tracking-wide px-1.5 py-0">
                {t("providers.inUse")}
              </Badge>
            )}
          </div>
          <p className="text-xs text-muted-foreground leading-snug">
            Version{" "}
            {versionLoading ? (
              <span className="inline-block w-12 h-3 align-middle rounded bg-muted-foreground/15 animate-pulse" />
            ) : version ? (
              <span className="font-mono">v{version}</span>
            ) : (
              <span className="font-mono">—</span>
            )}
          </p>
        </div>
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
