import React from "react";
import {
  Calendar as CalendarIcon,
  ChevronDown,
  Layers,
  RefreshCw
} from "lucide-react";
import type { DateRange as CalendarRange } from "react-day-picker";
import { BrandIcon } from "@/components/BrandIcon";
import { Calendar } from "@/components/ui/calendar";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { CLI_APP_LABEL, CLI_APP_SOURCE_BADGE } from "@/constants";
import { cn } from "@/lib/utils";
import { useT, type MessageKey } from "@/i18n";
import type { DateRange, DateRangePreset, SourceFilter } from "@/lib/stats-utils";
import type { CliApp } from "@/types";

type PresetOption = { id: Exclude<DateRangePreset, "custom">; labelKey: MessageKey };
const PRESETS: PresetOption[] = [
  { id: "today", labelKey: "stats.range.today" },
  { id: "7d", labelKey: "stats.range.7d" },
  { id: "30d", labelKey: "stats.range.30d" },
  { id: "90d", labelKey: "stats.range.90d" }
];

/** `Date` → `YYYY-MM-DD` in local time (used for the trigger label). */
function toInputDate(d: Date): string {
  const y = d.getFullYear();
  const m = `${d.getMonth() + 1}`.padStart(2, "0");
  const day = `${d.getDate()}`.padStart(2, "0");
  return `${y}-${m}-${day}`;
}

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
  const t = useT();
  const [presetOpen, setPresetOpen] = React.useState(false);
  const wrapperRef = React.useRef<HTMLDivElement>(null);

  // Draft state for the shadcn Calendar range picker. Seeded from the
  // active range on open so editing an existing custom window starts
  // from its current bounds rather than blank.
  const [draft, setDraft] = React.useState<CalendarRange | undefined>();
  React.useEffect(() => {
    if (!presetOpen) return;
    setDraft(
      range.preset === "custom"
        ? { from: range.from, to: range.to }
        : undefined
    );
    // Seed once per open — only re-run when the menu toggles.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [presetOpen]);

  const customValid = !!draft?.from && !!draft?.to;

  const applyCustom = () => {
    if (!draft?.from || !draft?.to) return;
    const from = new Date(draft.from);
    const to = new Date(draft.to);
    from.setHours(0, 0, 0, 0);
    to.setHours(23, 59, 59, 999);
    onRangeChange({ preset: "custom", from, to });
    setPresetOpen(false);
  };

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

  const currentPreset = PRESETS.find((p) => p.id === range.preset);
  const currentLabel =
    range.preset === "custom"
      ? `${toInputDate(range.from)} → ${toInputDate(range.to)}`
      : currentPreset
        ? t(currentPreset.labelKey)
        : t("stats.range.label");

  return (
    <div className="flex items-center gap-1 rounded-md bg-muted p-3 flex-wrap">
      {/* Source filter — shadcn Tabs (Radix-backed), styled to match
          the Providers page tab bar (flat, transparent inside a muted
          container) so the two filter shells look identical. */}
      <div className="flex-1 min-w-0">
        <Tabs
          value={source}
          onValueChange={(v) => onSourceChange(v as SourceFilter)}
          aria-label={t("stats.sourceFilter")}
        >
          <TabsList className="w-full justify-start gap-1 bg-transparent p-0 [&>button]:flex-none [&>button]:rounded-md [&>button]:px-3">
            {SOURCES.map((s) => (
              <TabsTrigger key={s} value={s}>
                {s === "All" ? (
                  <Layers className="size-4 shrink-0" aria-hidden />
                ) : (
                  <BrandIcon source={sourceLabel(s)} />
                )}
                <span>
                  {s === "All"
                    ? t("stats.source.all")
                    : CLI_APP_LABEL[s as CliApp]}
                </span>
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
          <CalendarIcon className="size-4 text-muted-foreground" />
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
                {t(p.labelKey)}
              </button>
            ))}

            {/* Custom range — shadcn Calendar in range mode. Embedded in
                the existing custom-controlled dropdown (not a Radix
                Popover) to stay clear of the Tauri WebKit overlay freeze
                noted above. */}
            <div className="mt-1 border-t pt-1.5">
              <div
                className={cn(
                  "text-xs font-medium px-2 mb-0.5",
                  range.preset === "custom"
                    ? "text-foreground"
                    : "text-muted-foreground"
                )}
              >
                {t("stats.range.custom")}
              </div>
              <Calendar
                mode="range"
                selected={draft}
                onSelect={setDraft}
                numberOfMonths={2}
                defaultMonth={draft?.from}
                className="p-2"
              />
              <button
                type="button"
                onClick={applyCustom}
                disabled={!customValid}
                className={cn(
                  "mx-2 mb-1 w-[calc(100%-1rem)] h-8 rounded-sm text-sm font-medium transition-colors",
                  "bg-primary text-primary-foreground hover:bg-primary/90",
                  "disabled:opacity-50 disabled:pointer-events-none"
                )}
              >
                {t("stats.range.apply")}
              </button>
            </div>
          </div>
        )}
      </div>

      <button
        type="button"
        onClick={onRefresh}
        disabled={refreshing}
        aria-label={t("stats.refresh")}
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
