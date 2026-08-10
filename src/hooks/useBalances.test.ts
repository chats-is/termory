import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import { blankProvider } from "@/lib/provider-utils";
import type { BalanceSubject, Provider, ProviderBalance } from "@/types";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

const { toastError } = vi.hoisted(() => ({ toastError: vi.fn() }));
vi.mock("sonner", () => ({ toast: { error: toastError } }));

// Captured so a test can push a backend-initiated result by hand.
const { listeners, listenMock } = vi.hoisted(() => {
  const listeners = new Map<string, (e: { payload: unknown }) => void>();
  return {
    listeners,
    listenMock: vi.fn((name: string, cb: (e: { payload: unknown }) => void) => {
      listeners.set(name, cb);
      return Promise.resolve(() => listeners.delete(name));
    })
  };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

import { useBalances, __resetBalanceCacheForTests } from "./useBalances";

function provider(over: Partial<Provider> = {}): Provider {
  return {
    ...blankProvider("codex"),
    id: "p1",
    baseUrl: "https://api.deepseek.com",
    apiKey: "sk-a",
    ...over
  };
}

function result(over: Partial<ProviderBalance> = {}): ProviderBalance {
  return {
    providerId: "p1",
    status: "ok",
    entries: [{ currency: "CNY", remaining: 48.2, depleted: false }],
    queriedAt: Date.now(),
    ...over
  };
}

beforeEach(() => {
  __resetBalanceCacheForTests();
  invokeMock.mockReset();
  toastError.mockReset();
  listeners.clear();
});
afterEach(() => vi.useRealTimers());

describe("useBalances", () => {
  it("fetches every provider on screen and keys results by provider id", async () => {
    const a = provider({ id: "a" });
    const b = provider({ id: "b", baseUrl: "https://openrouter.ai/api/v1" });
    invokeMock.mockImplementation((_cmd, args: { subject: BalanceSubject }) =>
      Promise.resolve(result({ providerId: args.subject.id }))
    );

    const { result: hook } = renderHook(() => useBalances([a, b]));
    await waitFor(() => {
      expect(Object.keys(hook.current.balances).sort()).toEqual(["a", "b"]);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("stores under the id it ASKED for, not the one the reply carries", async () => {
    // A reply echoing another provider's id must never paint that
    // provider's card with this wallet.
    invokeMock.mockResolvedValue(result({ providerId: "somebody-else" }));
    const { result: hook } = renderHook(() => useBalances([provider({ id: "a" })]));
    await waitFor(() => expect(hook.current.balances.a).toBeDefined());
    expect(hook.current.balances["somebody-else"]).toBeUndefined();
  });

  it("re-uses a fresh result across remounts instead of re-querying", async () => {
    invokeMock.mockResolvedValue(result());
    const first = renderHook(() => useBalances([provider()]));
    await waitFor(() => expect(first.result.current.balances.p1).toBeDefined());
    first.unmount();

    // The module cache surviving the remount is the whole point of it
    // living outside the hook.
    const second = renderHook(() => useBalances([provider()]));
    expect(second.result.current.balances.p1).toBeDefined();
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("never re-queries an unsupported provider, however long it sits", async () => {
    // `unsupported` is decided with NO network request and can only
    // change when the base URL does — which credsKey already catches. A
    // page of relays must therefore cost exactly one round of calls.
    invokeMock.mockResolvedValue(
      result({ status: "unsupported", entries: undefined })
    );
    const p = provider({ baseUrl: "https://my-relay.example.com/v1" });
    const first = renderHook(() => useBalances([p]));
    await waitFor(() => expect(first.result.current.balances.p1).toBeDefined());
    first.unmount();

    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 24 * 60 * 60_000); // a day later
    renderHook(() => useBalances([p]));
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("re-queries when the base URL or the key is edited", async () => {
    invokeMock.mockResolvedValue(result({ status: "unsupported" }));
    const { rerender } = renderHook(({ p }) => useBalances([p]), {
      initialProps: { p: provider({ baseUrl: "https://typo.example.com" }) }
    });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(1));

    // Fixing the URL must not leave the previous host's verdict on the
    // card — and `unsupported` never expires on its own, so without the
    // creds check the correction would never be picked up at all.
    rerender({ p: provider({ baseUrl: "https://api.deepseek.com" }) });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));

    // Same vendor, different account → a different wallet.
    rerender({
      p: provider({ baseUrl: "https://api.deepseek.com", apiKey: "sk-b" })
    });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(3));
  });

  it("does not re-fetch when an unrelated provider field changes", async () => {
    invokeMock.mockResolvedValue(result());
    const { rerender } = renderHook(({ p }) => useBalances([p]), {
      initialProps: { p: provider({ name: "before" }) }
    });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(1));
    rerender({ p: provider({ name: "after", model: "x" }) });
    // Give a stray effect a chance to fire before asserting it did not.
    await new Promise((r) => setTimeout(r, 0));
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("collapses concurrent fetches for the same provider", async () => {
    let release: (v: ProviderBalance) => void = () => {};
    invokeMock.mockReturnValue(
      new Promise<ProviderBalance>((res) => {
        release = res;
      })
    );
    const { result: hook } = renderHook(() => useBalances([provider()]));
    await waitFor(() => expect(hook.current.balanceLoading.has("p1")).toBe(true));

    // A click on the amount while the automatic pass is still running
    // must not start a second request — this is what lets the button
    // stay enabled (and its tooltip reachable) during a fetch.
    await act(async () => {
      await hook.current.refreshBalance(provider(), true);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    await act(async () => {
      release(result());
      await Promise.resolve();
    });
    await waitFor(() => expect(hook.current.balanceLoading.has("p1")).toBe(false));
  });

  it("holds a click and the automatic pass to the same window, and shows it", async () => {
    // The two floors are separate constants that currently carry the SAME
    // value (they track the quota's, user decision 2026-08-10): inside the
    // window neither the click nor the automatic pass fetches, past it
    // both do. `balanceInCooldown` is what makes the refused click
    // visible — a disabled button rather than a click that silently does
    // nothing.
    invokeMock.mockResolvedValue(result({ queriedAt: Date.now() }));
    const p = provider();
    const { result: hook } = renderHook(() => useBalances([p]));
    // Wait for the RESULT to land, not merely for the call: the cooldown
    // is derived from the stored entry, so waiting on the invoke count
    // races the state update (flaked only under full-suite load).
    await waitFor(() => expect(hook.current.balances.p1).toBeDefined());
    expect(invokeMock).toHaveBeenCalledTimes(1);

    expect(hook.current.balanceInCooldown("p1")).toBe(true);
    await act(async () => {
      await hook.current.refreshBalance(p, true);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    // Past the window: the button re-enables and BOTH paths fetch.
    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 150_000);
    expect(hook.current.balanceInCooldown("p1")).toBe(false);
    await act(async () => {
      await hook.current.refreshBalance(p, true);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    // …and so does the automatic pass, which is the point: with the two
    // windows equal, whatever the click may do the background pass may do
    // too. (The stub answers with a fixed `queriedAt`, so the stored entry
    // is still the original one and both floors measure from it.)
    await act(async () => {
      await hook.current.refreshBalance(p);
    });
    expect(invokeMock).toHaveBeenCalledTimes(3);
  });

  it("keeps the previous amount when a refresh fails", async () => {
    invokeMock.mockResolvedValueOnce(result());
    const p = provider();
    const { result: hook } = renderHook(() => useBalances([p]));
    await waitFor(() => expect(hook.current.balances.p1?.status).toBe("ok"));

    invokeMock.mockResolvedValueOnce(
      result({ status: "error", error: "Network error", entries: undefined })
    );
    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 150_000);
    await act(async () => {
      await hook.current.refreshBalance(p, true);
    });
    expect(hook.current.balances.p1?.entries).toHaveLength(1);
    expect(hook.current.balances.p1?.status).toBe("error");
  });

  it("toasts a failure ONLY when the user asked for it", async () => {
    invokeMock.mockResolvedValue(
      result({ status: "error", error: "HTTP 500: upstream down" })
    );
    const p = provider();
    const { result: hook } = renderHook(() => useBalances([p]));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(1));
    expect(toastError).not.toHaveBeenCalled();

    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 150_000);
    await act(async () => {
      await hook.current.refreshBalance(p, true);
    });
    expect(toastError).toHaveBeenCalledWith("HTTP 500", {
      description: "upstream down"
    });
  });

  it("takes the backend's own results without a request of its own", async () => {
    // The tray fetches on menu open and after a provider switch; the page
    // reflects that through the event rather than re-querying.
    invokeMock.mockResolvedValue(result());
    const { result: hook } = renderHook(() => useBalances([provider()]));
    await waitFor(() => expect(hook.current.balances.p1).toBeDefined());

    act(() => {
      listeners.get("termory:balance-changed")?.({
        payload: result({
          queriedAt: Date.now() + 1,
          entries: [{ currency: "CNY", remaining: 5, depleted: false }]
        })
      });
    });
    expect(hook.current.balances.p1?.entries?.[0].remaining).toBe(5);
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("ignores a pushed result for a provider not on screen", async () => {
    invokeMock.mockResolvedValue(result());
    const { result: hook } = renderHook(() => useBalances([provider()]));
    await waitFor(() => expect(hook.current.balances.p1).toBeDefined());

    act(() => {
      listeners.get("termory:balance-changed")?.({
        payload: result({ providerId: "somebody-else" })
      });
    });
    expect(hook.current.balances["somebody-else"]).toBeUndefined();
  });

  it("never rolls a pushed result back to an older one", async () => {
    // The page and the tray both fetch; a slower result can land after a
    // newer one.
    invokeMock.mockResolvedValue(result({ queriedAt: 2_000 }));
    const { result: hook } = renderHook(() => useBalances([provider()]));
    await waitFor(() => expect(hook.current.balances.p1).toBeDefined());

    act(() => {
      listeners.get("termory:balance-changed")?.({
        payload: result({
          queriedAt: 1_000,
          entries: [{ currency: "USD", remaining: 999, depleted: false }]
        })
      });
    });
    expect(hook.current.balances.p1?.entries?.[0].remaining).toBe(48.2);
  });

  it("drops a pushed result that predates a credential edit", async () => {
    // The payload carries no record of which credentials produced it, so
    // an in-flight backend fetch that started before the edit would
    // otherwise be stamped with the NEW ones — where it reads as fresh
    // and suppresses the re-fetch, leaving the old account's balance on
    // the new card.
    invokeMock.mockResolvedValue(result({ queriedAt: 1_000 }));
    const { result: hook, rerender } = renderHook(({ p }) => useBalances([p]), {
      initialProps: { p: provider({ apiKey: "sk-old" }) }
    });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(1));

    rerender({ p: provider({ apiKey: "sk-new" }) });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));

    // The OLD key's fetch finally lands.
    act(() => {
      listeners.get("termory:balance-changed")?.({
        payload: result({
          queriedAt: 1_500,
          entries: [{ currency: "CNY", remaining: 999, depleted: false }]
        })
      });
    });
    expect(hook.current.balances.p1?.entries?.[0].remaining).not.toBe(999);
  });

  it("reports no cooldown for a provider with no result yet", () => {
    invokeMock.mockReturnValue(new Promise(() => {}));
    const { result: hook } = renderHook(() => useBalances([provider()]));
    expect(hook.current.balanceInCooldown("p1")).toBe(false);
  });

  it("keeps the previous amount when the IPC itself throws", async () => {
    invokeMock.mockResolvedValueOnce(result());
    const p = provider();
    const { result: hook } = renderHook(() => useBalances([p]));
    await waitFor(() => expect(hook.current.balances.p1).toBeDefined());

    invokeMock.mockRejectedValueOnce(new Error("ipc down"));
    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 150_000);
    await act(async () => {
      await hook.current.refreshBalance(p, true);
    });
    expect(hook.current.balances.p1?.status).toBe("ok");
    expect(hook.current.balanceLoading.has("p1")).toBe(false);
  });
});
