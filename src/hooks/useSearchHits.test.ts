import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useSearchHits } from "./useSearchHits";
import type { SearchHit } from "../types";

// Stub the IPC the hook calls.
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

const hit = (id: string): SearchHit =>
  ({ id, source: "Claude" } as unknown as SearchHit);

// Advance past the debounce AND flush the invoke promise.
async function runDebounce() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(300);
  });
}

describe("useSearchHits — requirement verification", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("debounces: no search while typing, then ONE search for the final query", async () => {
    invokeMock.mockResolvedValue([]);
    const { rerender } = renderHook(({ q }) => useSearchHits(q), {
      initialProps: { q: "r" }
    });
    rerender({ q: "re" });
    rerender({ q: "rea" });
    rerender({ q: "react" });
    // Still typing → nothing has run yet.
    expect(invokeMock).not.toHaveBeenCalled();
    // Stopped typing → exactly one search, for the final query only.
    await runDebounce();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("search_all_sessions", {
      query: "react"
    });
  });

  it("loading stays false while typing (only the debounced search turns it on)", () => {
    invokeMock.mockResolvedValue([hit("a")]);
    const { result, rerender } = renderHook(({ q }) => useSearchHits(q), {
      initialProps: { q: "r" }
    });
    expect(result.current.loading).toBe(false);
    rerender({ q: "re" });
    expect(result.current.loading).toBe(false);
    rerender({ q: "react" });
    expect(result.current.loading).toBe(false);
  });

  it("SAVES the query when the search returns results", async () => {
    invokeMock.mockResolvedValue([hit("a"), hit("b")]);
    const onResults = vi.fn();
    renderHook(() => useSearchHits("react", onResults));
    await runDebounce();
    expect(onResults).toHaveBeenCalledTimes(1);
    expect(onResults).toHaveBeenCalledWith("react");
  });

  it("does NOT save when the search returns no results", async () => {
    invokeMock.mockResolvedValue([]);
    const onResults = vi.fn();
    renderHook(() => useSearchHits("react", onResults));
    await runDebounce();
    expect(onResults).not.toHaveBeenCalled();
  });
});
