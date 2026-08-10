import React from "react";
import { RefreshCw } from "lucide-react";
import {
  balanceButtonState,
  balanceDisplay,
  formatBalanceAmount
} from "@/lib/balance-utils";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useT } from "@/i18n";
import type { ProviderBalance } from "@/types";

/**
 * The wallet behind a direct-to-vendor endpoint — a custom provider on
 * the Providers tab, or an AI GATEWAY on the Gateways tab. One component
 * for both, so the two surfaces cannot drift into rendering the same
 * reading differently.
 *
 * **The value slot holds a balance and nothing else.** It never shows a
 * status word, never an error, and a number once read stays there —
 * everything about the refresh OPERATION (failed, cooling down) lives on
 * the button beside it, the only element the user can act on. With no
 * balance ever read, the row does not render at all; on most cards
 * (every relay, every provider or gateway pointed at one) that is the
 * permanent state.
 *
 * The value carries no tooltip: unlike a quota ring, which shows a bare
 * percentage and hides its label and reset time, the amount here IS the
 * whole content. Nothing is hidden, so there is nothing to reveal.
 */
export function BalanceInline({
  balance,
  loading,
  cooldown,
  onRefresh
}: {
  balance?: ProviderBalance;
  loading?: boolean;
  cooldown?: boolean;
  onRefresh?: () => void;
}) {
  const t = useT();
  const display = balanceDisplay(balance);
  if (display.kind === "hidden") return null;
  const button = balanceButtonState(balance, { loading, cooldown });

  return (
    // `ml-auto` parks it at the right edge of the title row, clear of the
    // name and its badges; `shrink-0` makes the NAME give way instead
    // (the h3 truncates) — a clipped amount would be a wrong number.
    // Typography follows the quota items on the account row above
    // (`OfficialAccountsSection` QuotaTierItem / PrepaidBalanceItem):
    // `text-xs leading-none`, label in `text-foreground`, numbers in
    // `font-mono tabular-nums`. `leading-none` is the load-bearing part —
    // text-xs's default 16px line-height was helping push this row past
    // the title's 28px line and out of alignment with its neighbours.
    <span className="ml-auto shrink-0 flex items-center gap-1.5 text-xs leading-none">
      <span className="text-muted-foreground">{t("providers.balance")}</span>
      {display.depleted ? (
        <Tooltip>
          <TooltipTrigger asChild>
            {/* The value carries a tooltip in EXACTLY one case: when it
                is tinted. Red is the only thing here the amount cannot
                explain itself — a vendor can report an account as unable
                to spend while its balance is non-zero (DeepSeek's
                `is_available`), so a red `¥10.00` is otherwise a colour
                with no reason. This is information ABOUT the value, not
                about the refresh operation, hence not on the button. */}
            <span className="font-mono tabular-nums text-destructive">
              {formatBalanceAmount(display.entries)}
            </span>
          </TooltipTrigger>
          <TooltipContent side="top">
            {t("providers.balanceDepleted")}
          </TooltipContent>
        </Tooltip>
      ) : (
        <span className="font-mono tabular-nums">
          {formatBalanceAmount(display.entries)}
        </span>
      )}
      {onRefresh && (
        <Tooltip>
          {/* Trigger on the WRAPPER: a disabled element dispatches no
              hover events, and this button is disabled exactly when it
              has something to explain — the cooldown text and the
              failure reason could never be read otherwise. */}
          <TooltipTrigger asChild>
            {/* Same size as the quota's refresh button one row up, and as
                the card's own action buttons beside it — this app already
                had a refresh-next-to-a-metric control and it is `icon-sm`
                + `size-4`. A smaller one reads as an inconsistency, not as
                a grouping; the grouping comes from spacing.
                No negative margin: the row is 32px now, so the button
                fits natively. Squeezing it into a 28px line was what put
                it 2px above the cluster it sits beside. */}
            <span className="shrink-0 inline-flex">
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                onClick={onRefresh}
                disabled={button.disabled}
                aria-label={t("providers.balanceRefresh")}
              >
                <RefreshCw
                  className={cn("size-4", button.spinning && "animate-spin")}
                />
              </Button>
            </span>
          </TooltipTrigger>
          <TooltipContent side="top" className="max-w-72">
            {button.tooltip.kind === "error" ? (
              <span className="break-words">{button.tooltip.message}</span>
            ) : button.tooltip.kind === "cooldown" ? (
              t("providers.balanceCooldownHint")
            ) : (
              t("providers.balanceRefresh")
            )}
          </TooltipContent>
        </Tooltip>
      )}
    </span>
  );
}
