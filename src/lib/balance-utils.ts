import { formatCurrency } from "@/lib/format";
import type { BalanceEntry, Provider, ProviderBalance } from "@/types";

/**
 * What the VALUE slot shows. The slot holds a balance and nothing else —
 * never an error, never a status word. A number that was fetched once
 * stays there; everything about the refresh operation (failed, cooling
 * down, when it last ran) belongs to the button beside it, which is the
 * only element the user can act on.
 *
 * So there are exactly two outcomes: an amount, or no row at all.
 * `hidden` is the default and by far the most common — a relay or
 * gateway base URL (`unsupported`), a provider still missing its key
 * (`no_key`), and one not yet fetched. A "not supported" line on every
 * such card would be noise on the majority of them.
 */
export type BalanceDisplay =
  | { kind: "hidden" }
  | { kind: "amount"; entries: BalanceEntry[]; depleted: boolean };

export function balanceDisplay(result?: ProviderBalance): BalanceDisplay {
  // Entries survive a failed refresh (see `mergeBalanceResult`), so this
  // deliberately does NOT gate on status: whatever was last read is what
  // the slot shows, and a failure only ever changes the button.
  const entries = result?.entries ?? [];
  if (entries.length === 0) return { kind: "hidden" };
  return {
    kind: "amount",
    entries,
    depleted: entries.some((e) => e.depleted)
  };
}

/**
 * The value slot's text: the remaining amount, plus the granted total
 * where the vendor reports one (OpenRouter alone) — `$20.75 / $25.00`.
 * The pair belongs here rather than on a hover because it IS balance
 * information, and this slot is where balance information goes.
 *
 * Multiple entries only ever come from DeepSeek's per-currency
 * `balance_infos` ("¥48.20 · $6.50"); every other vendor renders one.
 */
export function formatBalanceAmount(entries: BalanceEntry[]): string {
  return entries
    .map((e) => {
      const remaining = formatCurrency(e.remaining, e.currency);
      return e.total === undefined
        ? remaining
        : `${remaining} / ${formatCurrency(e.total, e.currency)}`;
    })
    .join(" · ");
}

/**
 * Everything the refresh BUTTON needs, decided in one place so the
 * component only maps a verdict to markup (same shape as
 * `upgradeBadgeState`).
 *
 * Tooltip precedence is load-bearing: **an error outranks the cooldown.**
 * A failed fetch selects the SHORT retry floor, so the button is in
 * cooldown for the whole window right after failing — letting the
 * cooldown text win would make the reason unreadable exactly when it
 * exists. Loading outranks both: mid-fetch there is nothing to explain
 * yet, and the spinner is the message.
 */
export type BalanceButtonState = {
  disabled: boolean;
  spinning: boolean;
  tooltip:
    | { kind: "idle" }
    | { kind: "cooldown" }
    | { kind: "error"; message: string };
};

export function balanceButtonState(
  result: ProviderBalance | undefined,
  opts: { loading?: boolean; cooldown?: boolean } = {}
): BalanceButtonState {
  const loading = !!opts.loading;
  const cooldown = !!opts.cooldown;
  if (loading) {
    return { disabled: true, spinning: true, tooltip: { kind: "idle" } };
  }
  const failed = result && result.status !== "ok" && result.error;
  if (failed) {
    // Still disabled while cooling down — the state is real — but the
    // tooltip reports the reason rather than the floor.
    return {
      disabled: cooldown,
      spinning: false,
      tooltip: { kind: "error", message: result.error! }
    };
  }
  if (cooldown) {
    return { disabled: true, spinning: false, tooltip: { kind: "cooldown" } };
  }
  return { disabled: false, spinning: false, tooltip: { kind: "idle" } };
}

/**
 * Store rule for a completed fetch: **a failure never empties the value
 * slot.** The slot's whole contract is that it holds a balance, so a
 * flaky request must leave the number the user was reading in place and
 * change only the button.
 *
 * - A SUCCESS replaces the entry outright.
 * - `unsupported` / `no_key` also replace it: both are DEFINITIVE
 *   (decided locally, no request made) and both legitimately clear the
 *   row — the user just pointed the provider at a relay, or cleared the
 *   key, and the slot must stop claiming a balance.
 * - Any other failure (network, 401, a moved response shape) KEEPS the
 *   previous amounts while carrying the failure's own bookkeeping:
 *   `status` selects the shorter retry floor, `queriedAt` drives the
 *   cooldown clock, `error` feeds the button's tooltip.
 *
 * The retained amounts describe the wallet behind one {baseUrl, apiKey},
 * so this only holds while those are unchanged — the cache's `credsKey`
 * check drops the entry otherwise, which is what stops one account's
 * balance surviving under another's.
 */
export function mergeBalanceResult(
  prev: ProviderBalance | undefined,
  next: ProviderBalance
): ProviderBalance {
  if (next.status === "ok") return next;
  if (next.status === "unsupported" || next.status === "no_key") return next;
  if (!prev?.entries?.length) return next;
  return { ...next, entries: prev.entries };
}

/**
 * Identity of the inputs a balance result was derived from. A cached
 * result is only valid while BOTH are unchanged: `baseUrl` decides which
 * vendor (or none) was queried and `apiKey` decides whose wallet, so an
 * edit to either makes the cached answer describe a different question.
 *
 * Without this, fixing a typo'd base URL left the card showing the
 * previous host's verdict — and since `unsupported` is cached
 * indefinitely (it costs no network and can only change with the URL),
 * the correction would never have been picked up at all.
 *
 * NUL as the separator because it cannot occur in either field.
 */
export function balanceCredsKey(p: Provider): string {
  return `${p.baseUrl ?? ""}\u0000${p.apiKey ?? ""}`;
}
