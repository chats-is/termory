import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { SubscriptionQuota } from "@/types";
import { QUOTA_CHANGED_EVENT, QUOTA_INVALIDATED_EVENT } from "@/constants";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

// Captured per event name so a test can push a backend event by hand.
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
vi.mock("sonner", () => ({ toast: { error: vi.fn() } }));

import { useQuotas, __resetQuotaCacheForTests } from "./useQuotas";

function quota(over: Partial<SubscriptionQuota> = {}): SubscriptionQuota {
  return {
    app: "claude",
    credentialStatus: "valid",
    success: true,
    tiers: [{ name: "five_hour", utilization: 12 }],
    queriedAt: Date.now(),
    ...over
  } as SubscriptionQuota;
}

/** Push a backend event into the hook's listener. */
function emit(name: string, payload: unknown) {
  const cb = listeners.get(name);
  if (!cb) throw new Error(`no listener for ${name}`);
  act(() => cb({ payload }));
}

beforeEach(() => {
  invokeMock.mockReset();
  listeners.clear();
  __resetQuotaCacheForTests();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useQuotas — fetching", () => {
  it("stores a successful result and clears loading", async () => {
    const result = quota();
    invokeMock.mockResolvedValue(result);
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(invokeMock).toHaveBeenCalledWith("fetch_subscription_quota", {
      app: "claude"
    });
    expect(r.current.quotas.claude?.tiers).toEqual(result.tiers);
    expect(r.current.quotaLoading).toBeNull();
  });

  it("ignores a CLI with no quota support", async () => {
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("opencode");
    });
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("refuses a second fetch inside the rate-limit floor", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });
});

// The rules that make an account switch show the NEW account's usage
// rather than the previous one's. See quota-utils `quotaResultIsStale`
// and CLAUDE.md's "a quota belongs to ONE login".
describe("useQuotas — account switch", () => {
  it("resetQuota drops the entry so a later failure has nothing to keep", async () => {
    invokeMock.mockResolvedValue(quota({ tiers: [{ name: "five_hour", utilization: 80 }] }));
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(r.current.quotas.claude).toBeDefined();

    act(() => r.current.resetQuota("claude"));
    expect(r.current.quotas.claude).toBeUndefined();

    // A failed post-switch fetch: mergeQuotaResult has no `prev`, so the
    // previous account's 80% cannot come back.
    invokeMock.mockResolvedValue(
      quota({ success: false, error: "HTTP 500", tiers: [], queriedAt: Date.now() })
    );
    await act(async () => {
      await r.current.refreshQuota("claude", false, true);
    });
    expect(r.current.quotas.claude?.tiers).toEqual([]);
  });

  // resetQuota clears the MODULE cache too, not just the rendered state —
  // which is what lets the re-fetch through, since the floor is measured
  // off that cache. (So the switch path would work even without `force`;
  // see the next test for what force is actually load-bearing for.)
  it("resetQuota clears the cache the rate-limit floor reads", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    act(() => r.current.resetQuota("claude"));
    await act(async () => {
      await r.current.refreshQuota("claude"); // no force
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("force bypasses the floor with no reset in front of it", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    await act(async () => {
      await r.current.refreshQuota("claude", false, true);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  // What `force` is really for on the switch path: a fetch started for
  // the PREVIOUS account can still be in flight, and the in-flight guard
  // would otherwise swallow the re-fetch for the new one.
  it("force starts a fetch even while one is in flight", async () => {
    let release!: (q: SubscriptionQuota) => void;
    invokeMock.mockReturnValueOnce(
      new Promise<SubscriptionQuota>((resolve) => {
        release = resolve;
      })
    );
    const { r } = await renderQuotas();
    let pending!: Promise<void>;
    act(() => {
      pending = r.current.refreshQuota("claude");
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    // Plain refresh: refused, a fetch is already running.
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    invokeMock.mockResolvedValue(quota());
    await act(async () => {
      await r.current.refreshQuota("claude", false, true);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    await act(async () => {
      release(quota());
      await pending;
    });
  });

  it("drops an in-flight result that was fetched before the switch", async () => {
    // A fetch that resolves only after the switch has landed.
    let release!: (q: SubscriptionQuota) => void;
    invokeMock.mockReturnValue(
      new Promise<SubscriptionQuota>((resolve) => {
        release = resolve;
      })
    );
    const { r } = await renderQuotas();
    let pending!: Promise<void>;
    act(() => {
      pending = r.current.refreshQuota("claude");
    });

    act(() => r.current.resetQuota("claude"));
    // Stamped BEFORE the reset → describes the account switched away from.
    await act(async () => {
      release(quota({ queriedAt: Date.now() - 5_000 }));
      await pending;
    });
    expect(r.current.quotas.claude).toBeUndefined();
  });

  it("keeps a result fetched after the switch", async () => {
    // Stamp `queriedAt` when the call is MADE, not when the mock is set
    // up — the backend stamps it as the fetch completes, and a value
    // frozen before the reset would read as the old account's.
    invokeMock.mockImplementation(() => Promise.resolve(quota()));
    const { r } = await renderQuotas();
    act(() => r.current.resetQuota("claude"));
    await act(async () => {
      await r.current.refreshQuota("claude", false, true);
    });
    expect(r.current.quotas.claude).toBeDefined();
  });
});

describe("useQuotas — backend events", () => {
  it("stores a pushed quota result", async () => {
    const { r } = await renderQuotas();
    emit(QUOTA_CHANGED_EVENT, quota({ plan: "Max" }));
    await waitFor(() => expect(r.current.quotas.claude?.plan).toBe("Max"));
  });

  it("never rolls back to an older snapshot", async () => {
    const { r } = await renderQuotas();
    emit(QUOTA_CHANGED_EVENT, quota({ plan: "Max", queriedAt: 2000 }));
    await waitFor(() => expect(r.current.quotas.claude?.plan).toBe("Max"));
    emit(QUOTA_CHANGED_EVENT, quota({ plan: "Pro", queriedAt: 1000 }));
    expect(r.current.quotas.claude?.plan).toBe("Max");
  });

  // A tray-initiated account switch: the page has no other way to learn
  // the login changed.
  it("clears the entry on the invalidated event", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(r.current.quotas.claude).toBeDefined();
    emit(QUOTA_INVALIDATED_EVENT, "claude");
    await waitFor(() => expect(r.current.quotas.claude).toBeUndefined());
  });

  it("ignores an invalidated event naming something that isn't a CLI", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    emit(QUOTA_INVALIDATED_EVENT, "not-a-cli");
    expect(r.current.quotas.claude).toBeDefined();
  });

  // The event beat the in-flight fetch it was announcing.
  it("drops a pushed result predating the last invalidation", async () => {
    const { r } = await renderQuotas();
    act(() => r.current.resetQuota("claude"));
    emit(QUOTA_CHANGED_EVENT, quota({ plan: "Max", queriedAt: Date.now() - 5_000 }));
    expect(r.current.quotas.claude).toBeUndefined();
  });
});

describe("useQuotas — cooldown", () => {
  it("is in cooldown right after a successful fetch and out of it later", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(r.current.quotaInCooldown).toBe(true);

    // 2 minutes on (the success floor), the Refresh button re-enables.
    const later = Date.now() + 121_000;
    vi.spyOn(Date, "now").mockReturnValue(later);
    const { r: r2 } = await renderQuotas();
    expect(r2.current.quotaInCooldown).toBe(false);
    vi.restoreAllMocks();
  });

  it("only reports cooldown for the CLI whose tab is open", async () => {
    invokeMock.mockResolvedValue(quota({ app: "codex" }));
    const { r } = await renderQuotas("codex");
    await act(async () => {
      await r.current.refreshQuota("codex");
    });
    expect(r.current.quotaInCooldown).toBe(true);
    // The same hook mounted on a CLI with no result of its own.
    const { r: claudeTab } = await renderQuotas("claude");
    expect(claudeTab.current.quotaInCooldown).toBe(false);
  });
});

describe("useQuotas — tab-entry freshness", () => {
  it("reports a just-fetched success as fresh and a failure as not", async () => {
    invokeMock.mockResolvedValue(quota());
    const { r } = await renderQuotas();
    expect(r.current.quotaIsFresh("claude")).toBe(false); // nothing cached yet
    await act(async () => {
      await r.current.refreshQuota("claude");
    });
    expect(r.current.quotaIsFresh("claude")).toBe(true);

    // A failure only holds for the short error-retry window, so 90s on it
    // is already stale while a success would still be fresh.
    __resetQuotaCacheForTests();
    invokeMock.mockResolvedValue(quota({ success: false, error: "boom" }));
    const { r: r2 } = await renderQuotas();
    await act(async () => {
      await r2.current.refreshQuota("claude");
    });
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + 90_000);
    expect(r2.current.quotaIsFresh("claude")).toBe(false);
    vi.restoreAllMocks();
  });
});

async function renderQuotas(app: "claude" | "codex" = "claude") {
  const { result: r } = renderHook(() => useQuotas(app));
  // The two listen() calls resolve on a microtask; wait so `emit` finds them.
  await waitFor(() => expect(listeners.size).toBe(2));
  return { r };
}
