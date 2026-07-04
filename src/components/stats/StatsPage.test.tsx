import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  screen,
  fireEvent,
  within,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { StatsPage } from "./StatsPage";
import type { AppSession } from "@/types";

// jsdom lacks ResizeObserver (recharts ResponsiveContainer wants it).
// Kept local to the test file per the project convention.
class RO {
  observe() {}
  unobserve() {}
  disconnect() {}
}
(globalThis as { ResizeObserver?: unknown }).ResizeObserver ??= RO;

function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

function mk(partial: Partial<AppSession>): AppSession {
  return {
    id: "x",
    source: "Claude",
    title: "t",
    project: "",
    path: "/p",
    started_at: "2026-07-01T10:00:00",
    updated_at: "2026-07-01T11:00:00",
    message_count: 0,
    preview: "",
    snippet: "",
    message_previews: [],
    model: "claude-opus-4-8",
    daily_tokens: [
      {
        date: "2026-07-01",
        tokens: { input: 100, output: 50, cached: 0, reasoning: 0, total: 150 },
        messages: 3
      }
    ],
    ...partial
  };
}

describe("StatsPage", () => {
  it("shows the Overview KPI cards by default and switches to Models", () => {
    render(
      <StatsPage sessions={[mk({})]} onRefresh={() => {}} refreshing={false} />
    );
    // Overview tab content: the new KPI labels.
    expect(screen.getByText("Active days")).toBeTruthy();
    expect(screen.getByText("Favorite model")).toBeTruthy();
    // Switch to Models (Radix Tabs activate on mousedown, not click).
    fireEvent.mouseDown(screen.getByRole("tab", { name: "Models" }), {
      button: 0
    });
    // Models-only artifact: the legend's in/out line.
    expect(screen.getByText("100 in · 50 out")).toBeTruthy();
    expect(screen.queryByText("Active days")).toBeNull();
  });

  it("offers the All / 30d / 7d range pills", () => {
    render(<StatsPage sessions={[]} onRefresh={() => {}} refreshing={false} />);
    // Scope to the Range tablist — the source filter also has an "All".
    const rangeList = within(screen.getByRole("tablist", { name: "Range" }));
    for (const label of ["All", "30d", "7d"]) {
      expect(rangeList.getByRole("tab", { name: label })).toBeTruthy();
    }
    const seven = rangeList.getByRole("tab", { name: "7d" });
    fireEvent.mouseDown(seven, { button: 0 });
    expect(seven.getAttribute("data-state")).toBe("active");
  });

  it("filters by source via the brand pills", () => {
    render(
      <StatsPage
        sessions={[mk({}), mk({ source: "Codex", model: "gpt-5", id: "y" })]}
        onRefresh={() => {}}
        refreshing={false}
      />
    );
    const codex = screen.getByRole("tab", { name: /Codex/ });
    fireEvent.mouseDown(codex, { button: 0 });
    expect(codex.getAttribute("data-state")).toBe("active");
  });

  it("wires the refresh button", () => {
    const onRefresh = vi.fn();
    render(
      <StatsPage sessions={[]} onRefresh={onRefresh} refreshing={false} />
    );
    fireEvent.click(screen.getByRole("button", { name: "Refresh stats" }));
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });
});
