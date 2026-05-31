import React from "react";
import { Check, Copy } from "lucide-react";
import { copyToClipboard } from "@/lib/clipboard";
import { cn } from "@/lib/utils";

export function CopyMenu({ items }: { items: { label: string; value: string }[] }) {
  const [open, setOpen] = React.useState(false);
  const [copied, setCopied] = React.useState<string | null>(null);
  const wrapperRef = React.useRef<HTMLDivElement>(null);

  React.useEffect(() => {
    if (!open) return;
    const handleDocClick = (event: MouseEvent) => {
      if (!wrapperRef.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const handleEsc = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", handleDocClick);
    document.addEventListener("keydown", handleEsc);
    return () => {
      document.removeEventListener("mousedown", handleDocClick);
      document.removeEventListener("keydown", handleEsc);
    };
  }, [open]);

  const handleCopy = async (label: string, value: string) => {
    await copyToClipboard(value);
    setCopied(label);
    setOpen(false);
    window.setTimeout(() => setCopied(null), 1200);
  };

  return (
    <div ref={wrapperRef} className="relative inline-flex">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label="Copy"
        className="inline-flex shrink-0 text-muted-foreground hover:text-foreground transition-colors"
      >
        <Copy size={12} />
      </button>
      {open && (
        <div
          role="menu"
          className="absolute top-full right-0 mt-1 min-w-[180px] z-50 rounded-md border bg-popover p-1 text-popover-foreground shadow-md"
        >
          {items.map((item) => (
            <button
              key={item.label}
              type="button"
              role="menuitem"
              onClick={() => void handleCopy(item.label, item.value)}
              className={cn(
                "w-full flex items-center justify-between gap-3 text-left px-2 py-1.5 rounded-sm text-sm cursor-pointer",
                "hover:bg-accent hover:text-accent-foreground focus:bg-accent focus:text-accent-foreground outline-none"
              )}
            >
              <span>{item.label}</span>
              {copied === item.label && <Check className="size-3.5 text-primary" />}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
