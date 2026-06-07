import React from "react";
import { describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { DateRange, SourceFilter } from "@/lib/stats-utils";
import { StatsFilterBar } from "./StatsFilterBar";

// Default props — each test overrides the bits it asserts on. The bar
// takes value + onChange props, so we assert on the vi.fn() callbacks
// rather than mocking internals. No I18nProvider: useT() falls back to
// English, so we assert the en.ts strings directly.
function setup(overrides: Partial<React.ComponentProps<typeof StatsFilterBar>> = {}) {
  const onRangeChange = vi.fn();
  const onSourceChange = vi.fn();
  const onRefresh = vi.fn();
  const range: DateRange = overrides.range ?? { preset: "7d" };
  const source: SourceFilter = overrides.source ?? "All";
  render(
    <StatsFilterBar
      range={range}
      onRangeChange={onRangeChange}
      source={source}
      onSourceChange={onSourceChange}
      refreshing={overrides.refreshing ?? false}
      onRefresh={onRefresh}
      {...overrides}
    />
  );
  return { onRangeChange, onSourceChange, onRefresh };
}

/** Open the date-range dropdown (the trigger shows the current label +
 * chevron). */
async function openRangeMenu(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole("button", { name: /last 7 days/i }));
  return screen.getByRole("menu");
}

describe("StatsFilterBar — date range presets", () => {
  it("fires onRangeChange with { preset } when a preset is clicked", async () => {
    const user = userEvent.setup();
    const { onRangeChange } = setup({ range: { preset: "7d" } });

    const menu = await openRangeMenu(user);
    await user.click(within(menu).getByRole("menuitem", { name: "Today" }));

    expect(onRangeChange).toHaveBeenCalledTimes(1);
    expect(onRangeChange).toHaveBeenCalledWith({ preset: "today" });
  });

  it.each([
    ["Last 30 days", "30d"],
    ["Last 90 days", "90d"]
  ])("maps the %s row to preset %s", async (label, preset) => {
    const user = userEvent.setup();
    const { onRangeChange } = setup({ range: { preset: "7d" } });
    const menu = await openRangeMenu(user);
    await user.click(within(menu).getByRole("menuitem", { name: label }));
    expect(onRangeChange).toHaveBeenCalledWith({ preset });
  });

  it("closes the menu after picking a preset", async () => {
    const user = userEvent.setup();
    setup({ range: { preset: "7d" } });

    await openRangeMenu(user);
    await user.click(screen.getByRole("menuitem", { name: "Today" }));

    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  });
});

describe("StatsFilterBar — source filter", () => {
  it("fires onSourceChange when a source tab is clicked", async () => {
    const user = userEvent.setup();
    const { onSourceChange } = setup({ source: "All" });

    await user.click(screen.getByRole("tab", { name: /codex/i }));

    expect(onSourceChange).toHaveBeenCalledWith("codex");
  });

  it("renders all five source tabs (All + 4 CLIs)", () => {
    setup({ source: "All" });
    expect(screen.getByRole("tab", { name: /all/i })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /codex/i })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /claude/i })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /gemini/i })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /opencode/i })).toBeInTheDocument();
  });
});

describe("StatsFilterBar — custom range dropdown", () => {
  it("renders the Custom range section + calendar inside the open menu", async () => {
    const user = userEvent.setup();
    setup({ range: { preset: "7d" } });

    const menu = await openRangeMenu(user);

    // The custom-controlled dropdown (NOT a Radix Popover) holds the
    // Custom range label, a shadcn Calendar (grid), and an Apply button.
    expect(within(menu).getByText("Custom range")).toBeInTheDocument();
    // numberOfMonths={2} → two calendar grids.
    expect(within(menu).getAllByRole("grid").length).toBeGreaterThanOrEqual(1);
    expect(
      within(menu).getByRole("button", { name: /^apply$/i })
    ).toBeInTheDocument();
  });

  it("disables Apply until a day range is drafted", async () => {
    const user = userEvent.setup();
    setup({ range: { preset: "7d" } });

    const menu = await openRangeMenu(user);

    // No draft seeded for a non-custom range → Apply is disabled and
    // clicking it does nothing (no onRangeChange custom fire).
    expect(
      within(menu).getByRole("button", { name: /^apply$/i })
    ).toBeDisabled();
  });
});

describe("StatsFilterBar — refresh", () => {
  it("fires onRefresh when the refresh button is clicked", async () => {
    const user = userEvent.setup();
    const { onRefresh } = setup({ refreshing: false });

    await user.click(screen.getByRole("button", { name: /refresh stats/i }));
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });

  it("disables the refresh button while refreshing", () => {
    setup({ refreshing: true });
    expect(
      screen.getByRole("button", { name: /refresh stats/i })
    ).toBeDisabled();
  });
});
