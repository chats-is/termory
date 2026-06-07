import React from "react";
import { ChevronsUpDown } from "lucide-react";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList
} from "@/components/ui/command";

/**
 * Inline model combobox built on cmdk's `Command` — type a model id freely
 * OR pick from the fetched suggestions. Rendered INLINE (no portal, no Radix
 * Popover) so it works inside a Radix Dialog: mouse hover / click / wheel
 * scroll all behave (a portaled popup gets blocked by the Dialog's
 * `pointer-events:none` + scroll lock). cmdk filters the list by what's typed
 * (plain substring, not fuzzy).
 */
export function ModelCombobox({
  id,
  value,
  onValueChange,
  options,
  loading,
  placeholder,
  ariaInvalid,
  ariaLabel,
  className
}: {
  id?: string;
  value: string;
  onValueChange: (v: string) => void;
  options: string[];
  loading?: boolean;
  placeholder?: string;
  ariaInvalid?: boolean;
  ariaLabel?: string;
  className?: string;
}) {
  const t = useT();
  const placeholderText = placeholder ?? t("providers.modelPlaceholder");
  const [open, setOpen] = React.useState(false);
  // The fetched catalog can be a union of two `/models` lists → dedupe so
  // duplicate ids don't collide on the `key` / drop a row.
  const opts = React.useMemo(() => Array.from(new Set(options)), [options]);
  return (
    <Command
      // The input value IS the model (free text); cmdk filters items by it.
      filter={(itemValue, search) =>
        itemValue.toLowerCase().includes(search.trim().toLowerCase()) ? 1 : 0
      }
      className={cn(
        "relative overflow-visible rounded-md border border-input bg-transparent [&_[data-slot=command-input-wrapper]]:border-b-0 [&_[data-slot=command-input-wrapper]>svg]:hidden [&_[data-slot=command-input-wrapper]]:pl-3",
        className
      )}
    >
      <CommandInput
        id={id}
        value={value}
        onValueChange={onValueChange}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        placeholder={placeholderText}
        aria-invalid={ariaInvalid}
        aria-label={ariaLabel}
        className="font-mono pr-8"
      />
      <ChevronsUpDown className="pointer-events-none absolute top-2.5 right-3 size-4 shrink-0 opacity-50" />
      {open && (
        <div
          className="absolute top-full right-0 left-0 z-50 mt-1 rounded-md border bg-popover text-popover-foreground shadow-md"
          // Keep the input focused so a click commits the item before blur.
          onMouseDown={(e) => e.preventDefault()}
        >
          <CommandList className="max-h-[168px]">
            <CommandEmpty>
              {loading ? "Fetching models…" : "No models found."}
            </CommandEmpty>
            <CommandGroup>
              {opts.map((m) => (
                <CommandItem
                  key={m}
                  value={m}
                  onSelect={() => {
                    onValueChange(m);
                    setOpen(false);
                  }}
                >
                  {m}
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </div>
      )}
    </Command>
  );
}
