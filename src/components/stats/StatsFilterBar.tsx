import React from "react";
import { Calendar, ChevronDown, Layers, RefreshCw } from "lucide-react";
import { BrandIcon } from "@/components/BrandIcon";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { CLI_APP_SOURCE_BADGE } from "@/constants";
import { cn } from "@/lib/utils";
import type { DateRange, DateRangePreset, SourceFilter } from "@/lib/stats-utils";
import type { CliApp } from "@/types";

type PresetOption = { id: Exclude<DateRangePreset, "custom">; label: string };
const PRESETS: PresetOption[] = [
  { id: "7d", label: "Last 7 days" },
  { id: "30d", label: "Last 30 days" },
  { id: "90d", label: "Last 90 days" },
  { id: "365d", label: "Last 365 days" },
  { id: "all", label: "All time" }
];

const SOURCES: SourceFilter[] = ["All", "claude", "codex", "gemini", "opencode"];

function sourceLabel(s: SourceFilter): string {
  if (s === "All") return "All";
  return CLI_APP_SOURCE_BADGE[s as CliApp] ?? s;
}

export function StatsFilterBar({
  range,
  onRangeChange,
  source,
  onSourceChange,
  refreshing,
  onRefresh
}: {
  range: DateRange;
  onRangeChange: (next: DateRange) => void;
  source: SourceFilter;
  onSourceChange: (next: SourceFilter) => void;
  refreshing: boolean;
  onRefresh: () => void;
}) {
  const [presetOpen, setPresetOpen] = React.useState(false);
  const wrapperRef = React.useRef<HTMLDivElement>(null);

  // Click-outside / Esc dismiss for the preset menu. Custom controlled
  // implementation rather than Radix DropdownMenu — the latter has a
  // documented freeze in Tauri's WebKit when used alongside Dialogs
  // (see commit cb86276 and the radix-ui/primitives#3317 thread).
  React.useEffect(() => {
    if (!presetOpen) return;
    const handleDown = (event: MouseEvent) => {
      if (!wrapperRef.current?.contains(event.target as Node)) {
        setPresetOpen(false);
      }
    };
    const handleEsc = (event: KeyboardEvent) => {
      if (event.key === "Escape") setPresetOpen(false);
    };
    document.addEventListener("mousedown", handleDown);
    document.addEventListener("keydown", handleEsc);
    return () => {
      document.removeEventListener("mousedown", handleDown);
      document.removeEventListener("keydown", handleEsc);
    };
  }, [presetOpen]);

  const currentLabel =
    range.preset === "custom"
      ? "Custom range"
      : PRESETS.find((p) => p.id === range.preset)?.label ?? "Range";

  return (
    <div className="flex items-center gap-1 rounded-md bg-muted p-3 flex-wrap">
      {/* Source filter — shadcn Tabs (Radix-backed), styled to match
          the Providers page tab bar (flat, transparent inside a muted
          container) so the two filter shells look identical. */}
      <div className="flex-1 min-w-0 overflow-x-auto">
        <Tabs
          value={source}
          onValueChange={(v) => onSourceChange(v as SourceFilter)}
          aria-label="Source filter"
        >
          <TabsList className="w-full justify-start gap-1 bg-transparent p-0 [&>button]:flex-none [&>button]:rounded-md [&>button]:px-3">
            {SOURCES.map((s) => (
              <TabsTrigger key={s} value={s}>
                {s === "All" ? (
                  <Layers className="size-4 shrink-0" aria-hidden />
                ) : (
                  <BrandIcon source={sourceLabel(s)} />
                )}
                <span>{sourceLabel(s)}</span>
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
      </div>

      {/* Date range dropdown — anchored to the right side of its
          trigger so the menu doesn't overflow the panel when sitting
          next to the refresh button. */}
      <div ref={wrapperRef} className="relative">
        <button
          type="button"
          onClick={() => setPresetOpen((p) => !p)}
          aria-haspopup="menu"
          aria-expanded={presetOpen}
          className={cn(
            "h-9 px-3 rounded-md border bg-card shadow-sm",
            "inline-flex items-center gap-2 text-sm",
            "hover:bg-accent hover:text-accent-foreground transition-colors"
          )}
        >
          <Calendar className="size-4 text-muted-foreground" />
          <span>{currentLabel}</span>
          <ChevronDown className="size-3.5 text-muted-foreground" />
        </button>
        {presetOpen && (
          <div
            role="menu"
            className={cn(
              "absolute top-full right-0 mt-1 min-w-[180px] z-50",
              "rounded-md border bg-popover p-1 text-popover-foreground shadow-md"
            )}
          >
            {PRESETS.map((p) => (
              <button
                key={p.id}
                type="button"
                role="menuitem"
                onClick={() => {
                  onRangeChange({ preset: p.id });
                  setPresetOpen(false);
                }}
                className={cn(
                  "w-full text-left px-2 py-1.5 rounded-sm text-sm cursor-pointer",
                  "hover:bg-accent hover:text-accent-foreground outline-none transition-colors",
                  range.preset === p.id && "bg-accent/50"
                )}
              >
                {p.label}
              </button>
            ))}
          </div>
        )}
      </div>

      <button
        type="button"
        onClick={onRefresh}
        disabled={refreshing}
        aria-label="Refresh stats"
        title="Refresh"
        className={cn(
          "h-9 w-9 rounded-md border bg-card shadow-sm",
          "inline-flex items-center justify-center transition-colors",
          "hover:bg-accent hover:text-accent-foreground",
          "disabled:opacity-50 disabled:pointer-events-none"
        )}
      >
        <RefreshCw
          className={cn("size-4", refreshing && "animate-spin")}
        />
      </button>
    </div>
  );
}
