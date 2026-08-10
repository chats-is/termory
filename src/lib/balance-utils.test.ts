import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { setFormatLocale } from "./format";
import {
  balanceButtonState,
  balanceCredsKey,
  balanceDisplay,
  formatBalanceAmount,
  mergeBalanceResult
} from "./balance-utils";
import { blankProvider } from "./provider-utils";
import type { BalanceEntry, Provider, ProviderBalance } from "@/types";

function result(over: Partial<ProviderBalance> = {}): ProviderBalance {
  return {
    providerId: "p1",
    status: "ok",
    queriedAt: 1_700_000_000_000,
    ...over
  };
}

function entry(over: Partial<BalanceEntry> = {}): BalanceEntry {
  return { currency: "USD", remaining: 12.5, depleted: false, ...over };
}

describe("balanceDisplay — the value slot holds a balance and nothing else", () => {
  it("renders nothing when no balance was ever read", () => {
    // The permanent state of most cards: a relay or gateway base URL,
    // which the backend answers `unsupported` without any request.
    expect(balanceDisplay(undefined)).toEqual({ kind: "hidden" });
    expect(balanceDisplay(result({ status: "unsupported" }))).toEqual({
      kind: "hidden"
    });
    expect(balanceDisplay(result({ status: "no_key" }))).toEqual({
      kind: "hidden"
    });
    expect(balanceDisplay(result({ entries: [] }))).toEqual({ kind: "hidden" });
  });

  it("KEEPS the number when the last refresh failed", () => {
    // The slot's whole contract. A failure changes the button, never the
    // value — the entries reach here retained by mergeBalanceResult.
    for (const status of ["error", "auth_failed"] as const) {
      expect(
        balanceDisplay(result({ status, error: "boom", entries: [entry()] }))
      ).toMatchObject({ kind: "amount", depleted: false });
    }
  });

  it("never puts a status word where the amount goes", () => {
    // A failure with nothing retained hides the row rather than filling
    // the slot with "Unavailable".
    expect(
      balanceDisplay(result({ status: "error", error: "boom" }))
    ).toEqual({ kind: "hidden" });
  });

  it("reports depletion when ANY entry is spent", () => {
    expect(balanceDisplay(result({ entries: [entry()] }))).toMatchObject({
      depleted: false
    });
    // DeepSeek returns one entry per currency; one empty wallet is
    // enough to colour the row.
    expect(
      balanceDisplay(
        result({ entries: [entry(), entry({ currency: "CNY", depleted: true })] })
      )
    ).toMatchObject({ depleted: true });
  });
});

describe("balanceButtonState — the button carries every state", () => {
  const failed = result({ status: "error", error: "HTTP 500: upstream down" });

  it("is idle and enabled with nothing to say", () => {
    expect(balanceButtonState(result())).toEqual({
      disabled: false,
      spinning: false,
      tooltip: { kind: "idle" }
    });
  });

  it("spins and disables while fetching", () => {
    expect(balanceButtonState(result(), { loading: true })).toEqual({
      disabled: true,
      spinning: true,
      tooltip: { kind: "idle" }
    });
  });

  it("explains the cooldown that disables it", () => {
    expect(balanceButtonState(result(), { cooldown: true })).toEqual({
      disabled: true,
      spinning: false,
      tooltip: { kind: "cooldown" }
    });
  });

  it("puts the FAILURE ahead of the cooldown", () => {
    // Load-bearing: a failure selects the short retry floor, so the
    // button is in cooldown for that whole window right after failing.
    // If the cooldown text won, the reason would be unreadable exactly
    // when it exists.
    expect(balanceButtonState(failed, { cooldown: true })).toEqual({
      disabled: true,
      spinning: false,
      tooltip: { kind: "error", message: "HTTP 500: upstream down" }
    });
    // Still reported once the cooldown lapses, now clickable to retry.
    expect(balanceButtonState(failed)).toMatchObject({
      disabled: false,
      tooltip: { kind: "error", message: "HTTP 500: upstream down" }
    });
  });

  it("says nothing about a failure it cannot describe", () => {
    // A failed status with no message has nothing to report, so it falls
    // back to the ordinary label rather than an empty tooltip.
    expect(balanceButtonState(result({ status: "error" }))).toMatchObject({
      tooltip: { kind: "idle" }
    });
  });

  it("lets the spinner outrank everything", () => {
    expect(
      balanceButtonState(failed, { loading: true, cooldown: true })
    ).toMatchObject({ spinning: true, tooltip: { kind: "idle" } });
  });
});

describe("mergeBalanceResult", () => {
  const prev = result({ entries: [entry()] });

  it("keeps the previous amounts through a transient failure", () => {
    const next = result({
      status: "error",
      error: "Network error",
      entries: undefined,
      queriedAt: 1_700_000_999_999
    });
    const merged = mergeBalanceResult(prev, next);
    expect(merged.entries).toEqual(prev.entries);
    // …while carrying the failure's own bookkeeping.
    expect(merged.status).toBe("error");
    expect(merged.error).toBe("Network error");
    expect(merged.queriedAt).toBe(1_700_000_999_999);
  });

  it("lets a definitive verdict clear the row", () => {
    // Decided locally with no request: the user pointed the provider at
    // a relay, or cleared the key. The slot must stop claiming a balance.
    for (const status of ["unsupported", "no_key"] as const) {
      expect(
        mergeBalanceResult(prev, result({ status, entries: undefined })).entries
      ).toBeUndefined();
    }
  });

  it("replaces outright on success, and keeps nothing it never had", () => {
    const fresh = result({ entries: [entry({ remaining: 1 })] });
    expect(mergeBalanceResult(prev, fresh)).toBe(fresh);
    const failure = result({ status: "error", entries: undefined });
    expect(mergeBalanceResult(undefined, failure)).toBe(failure);
  });
});

describe("balance formatting", () => {
  // formatCurrency follows the APP locale, which is the OS locale when no
  // I18nProvider is mounted — so an unpinned assertion here reads
  // "US$4.25" on a zh-CN machine and "$4.25" on CI. Pin it.
  beforeAll(() => setFormatLocale("en-US"));
  afterAll(() => setFormatLocale(undefined));

  it("joins one amount per currency", () => {
    expect(formatBalanceAmount([entry({ remaining: 12.5 })])).toBe("$12.50");
    expect(
      formatBalanceAmount([
        entry({ currency: "CNY", remaining: 48.2 }),
        entry({ currency: "USD", remaining: 6.5 })
      ])
    ).toBe("CN¥48.20 · $6.50");
  });

  it("carries the granted total where a vendor reports one", () => {
    // OpenRouter alone; it belongs in the value slot because it IS
    // balance information, and that slot is where balance goes.
    expect(formatBalanceAmount([entry({ remaining: 20.75, total: 25 })])).toBe(
      "$20.75 / $25.00"
    );
    // Everyone else shows a single amount, no empty separator.
    expect(formatBalanceAmount([entry({ remaining: 20.75 })])).toBe("$20.75");
  });
});

describe("balanceCredsKey", () => {
  it("changes when either input the result depends on changes", () => {
    const base = {
      ...blankProvider("codex"),
      baseUrl: "https://api.deepseek.com",
      apiKey: "sk-a"
    };
    const same = balanceCredsKey({ ...base });
    expect(balanceCredsKey(base)).toBe(same);
    // A different vendor is queried…
    expect(balanceCredsKey({ ...base, baseUrl: "https://openrouter.ai" })).not.toBe(same);
    // …or a different account's wallet at the same vendor.
    expect(balanceCredsKey({ ...base, apiKey: "sk-b" })).not.toBe(same);
    // Fields the answer does NOT depend on must not invalidate it. Passed
    // as a whole Provider (not an object literal) because the parameter is
    // now the narrow `BalanceSubject` — a provider still satisfies it
    // structurally, which is what lets a GATEWAY be a subject too.
    const renamed: Provider = { ...base, name: "renamed", model: "x" };
    expect(balanceCredsKey(renamed)).toBe(same);
    // …and a gateway, which carries no `app` at all, is a valid subject.
    expect(
      balanceCredsKey({
        id: "gw1",
        baseUrl: base.baseUrl,
        apiKey: base.apiKey
      })
    ).toBe(same);
  });

  it("cannot be forged by moving the split point", () => {
    // A plain separator that can occur in a field would let one
    // {url, key} pair collide with another.
    const a = balanceCredsKey({
      ...blankProvider("codex"),
      baseUrl: "https://x.test/a",
      apiKey: "b"
    });
    const b = balanceCredsKey({
      ...blankProvider("codex"),
      baseUrl: "https://x.test/a b",
      apiKey: ""
    });
    expect(a).not.toBe(b);
  });
});
