import React from "react";
import {
  BarChart3,
  History,
  Plug,
  Search,
  Settings as SettingsIcon,
  Star
} from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type { Route } from "@/types";

export function ActivityRail({
  route,
  onChange
}: {
  route: Route;
  onChange: (next: Route) => void;
}) {
  const t = useT();
  const items: { id: Route; icon: React.ReactNode; labelKey: MessageKey }[] = [
    { id: "providers", icon: <Plug size={20} />, labelKey: "nav.providers" },
    { id: "records", icon: <History size={20} />, labelKey: "nav.records" },
    { id: "favorites", icon: <Star size={20} />, labelKey: "nav.favorites" },
    { id: "search", icon: <Search size={20} />, labelKey: "nav.search" },
    { id: "stats", icon: <BarChart3 size={20} />, labelKey: "nav.stats" },
    { id: "settings", icon: <SettingsIcon size={20} />, labelKey: "nav.settings" }
  ];
  return (
    <nav
      aria-label="Primary"
      className="flex flex-col items-center gap-2 p-3 bg-sidebar my-3 ml-3 rounded-md"
    >
      {items.map((item) => {
        const isActive = route === item.id;
        return (
          <Tooltip key={item.id}>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={() => onChange(item.id)}
                aria-label={t(item.labelKey)}
                aria-current={isActive ? "page" : undefined}
                className={cn(
                  "inline-flex items-center justify-center rounded-md size-9 transition-colors",
                  isActive
                    ? "bg-background text-foreground shadow-sm"
                    : "text-muted-foreground hover:bg-accent/60 hover:text-foreground"
                )}
              >
                {item.icon}
              </button>
            </TooltipTrigger>
            <TooltipContent side="right">{t(item.labelKey)}</TooltipContent>
          </Tooltip>
        );
      })}
    </nav>
  );
}
