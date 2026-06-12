import React from "react";
import { Check, Copy } from "lucide-react";
import { copyToClipboard } from "@/lib/clipboard";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";

/** Per-message copy affordance shared by Records (MessageList rows) and
 * the Favorites detail pane. Copies the message's raw markdown text and
 * swaps the icon to a check for a short confirmation window. */
export function MessageCopyButton({
  text,
  className
}: {
  text: string;
  className?: string;
}) {
  const t = useT();
  const [copied, setCopied] = React.useState(false);
  const timerRef = React.useRef<number | undefined>(undefined);
  React.useEffect(() => () => window.clearTimeout(timerRef.current), []);

  const handleCopy = async (event: React.MouseEvent) => {
    event.stopPropagation();
    await copyToClipboard(text);
    setCopied(true);
    window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => setCopied(false), 1200);
  };

  const label = copied ? t("common.copied") : t("common.copy");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={(event) => void handleCopy(event)}
          aria-label={label}
          className={cn(
            "p-1 rounded transition-colors",
            copied
              ? "text-primary"
              : "text-muted-foreground/40 hover:text-foreground",
            className
          )}
        >
          {copied ? (
            <Check className="size-3.5" aria-hidden />
          ) : (
            <Copy className="size-3.5" aria-hidden />
          )}
        </button>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}
