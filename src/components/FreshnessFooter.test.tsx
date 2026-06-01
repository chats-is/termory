import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { FreshnessFooter } from "./FreshnessFooter";

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-05-29T12:00:00Z"));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("FreshnessFooter", () => {
  it("renders nothing visible when idle and never synced", () => {
    // No icon + no label when lastSyncedAt is null & not syncing & no error
    const { container } = render(
      <FreshnessFooter syncing={false} lastSyncedAt={null} error={null} />
    );
    const footer = container.querySelector("footer");
    expect(footer).not.toBeNull();
    expect(footer?.textContent?.trim()).toBe("");
  });

  it("renders 'Syncing…' label while syncing", () => {
    render(
      <FreshnessFooter syncing={true} lastSyncedAt={null} error={null} />
    );
    expect(screen.getByText("Syncing…")).toBeInTheDocument();
  });

  it("renders 'Sync failed' when error is set", () => {
    render(
      <FreshnessFooter
        syncing={false}
        lastSyncedAt={null}
        error="boom: ENOENT"
      />
    );
    expect(screen.getByText("Sync failed")).toBeInTheDocument();
  });

  it("flashes 'Synced just now' when lastSyncedAt advances, then falls back", () => {
    const first = Date.now() - 60_000;
    const { rerender } = render(
      <FreshnessFooter syncing={false} lastSyncedAt={first} error={null} />
    );
    // Initial mount: lastSyncedAt is the first value but prevSyncedAt.current
    // starts equal to it (initialized in useRef), so the just-synced effect
    // sees no advance. Falls through to the idle "Synced Nm ago" branch.
    expect(screen.getByText(/Synced/)).toBeInTheDocument();
    expect(screen.queryByText("Synced just now")).toBeNull();

    // Bump → effect detects advance → enters "Synced just now" pulse
    act(() => {
      rerender(
        <FreshnessFooter
          syncing={false}
          lastSyncedAt={Date.now()}
          error={null}
        />
      );
    });
    expect(screen.getByText("Synced just now")).toBeInTheDocument();

    // Advance past the pulse window AND past formatTimeAgo's "just now"
    // threshold (5s) so we can distinguish "pulse ended" from "still
    // showing just now via the idle fallback".
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(screen.queryByText("Synced just now")).toBeNull();
    expect(screen.getByText(/Synced 10s ago/)).toBeInTheDocument();
  });

  it("ignores lastSyncedAt advance when error is present", () => {
    const { rerender } = render(
      <FreshnessFooter
        syncing={false}
        lastSyncedAt={null}
        error="initial"
      />
    );
    expect(screen.getByText("Sync failed")).toBeInTheDocument();
    act(() => {
      rerender(
        <FreshnessFooter
          syncing={false}
          lastSyncedAt={Date.now()}
          error="still failing"
        />
      );
    });
    expect(screen.queryByText("Synced just now")).toBeNull();
    expect(screen.getByText("Sync failed")).toBeInTheDocument();
  });
});
