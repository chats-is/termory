import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  render as rtlRender,
  screen,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { FreshnessFooter } from "./FreshnessFooter";

// The footer carries a Tooltip (exact timestamp / full error), and Radix
// requires the provider the real app mounts at its root. Passed as RTL's
// `wrapper` rather than wrapped inline, so `rerender` keeps it too —
// wrapping the JSX by hand loses the provider on every rerender.
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(ui, { wrapper: TooltipProvider, ...options });
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-05-29T12:00:00Z"));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("FreshnessFooter — tooltip", () => {
  // ONLY a failure gets one. The success label ("Synced 1m ago") already
  // says everything the footer is for, so an exact timestamp on hover was
  // dropped as noise (user decision 2026-07-31); a failure's label is just
  // "Sync failed" and the reason has nowhere else to live. Radix's asChild
  // trigger stamps data-slot onto the footer, so that attribute tells a
  // wrapped footer from a bare one.
  const trigger = (el: Element | null) => el?.getAttribute("data-slot");
  const synced = Date.now() - 60_000;

  it("wraps the footer when a sync failed, so the reason is reachable", () => {
    const { container } = render(
      <FreshnessFooter syncing={false} lastSyncedAt={synced} error={"scan failed: EACCES"} />
    );
    expect(trigger(container.querySelector("footer"))).toBe("tooltip-trigger");
  });

  it("wraps a failure that happened before any successful sync", () => {
    const { container } = render(
      <FreshnessFooter syncing={false} lastSyncedAt={null} error={"scan failed: EACCES"} />
    );
    expect(trigger(container.querySelector("footer"))).toBe("tooltip-trigger");
  });

  it("leaves every success state bare — the label is the whole story", () => {
    for (const props of [
      { syncing: false, lastSyncedAt: synced, error: null }, // idle
      { syncing: true, lastSyncedAt: synced, error: null }, // rescanning
      { syncing: true, lastSyncedAt: null, error: null }, // first scan
      { syncing: false, lastSyncedAt: null, error: null } // never synced
    ]) {
      const { container, unmount } = render(<FreshnessFooter {...props} />);
      expect(trigger(container.querySelector("footer"))).toBeNull();
      unmount();
    }
  });
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
