import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  screen,
  type RenderOptions
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { SubscriptionQuota } from "@/types";
import { ProviderOfficialCard } from "./ProviderOfficialCard";

// jsdom lacks ResizeObserver, which Radix Tooltip touches when a tooltip
// opens (e.g. via userEvent hover/focus). Provide a no-op shim.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver;

// Tooltip needs a TooltipProvider in the tree. i18n falls back to
// English with no I18nProvider, so we assert English copy.
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

const baseProps = {
  app: "claude" as const,
  isInUse: true,
  settingDefault: false,
  version: "2.0.0",
  onSetDefault: vi.fn()
};

function makeQuota(overrides: Partial<SubscriptionQuota> = {}): SubscriptionQuota {
  return {
    app: "claude",
    credentialStatus: "valid",
    success: true,
    tiers: [
      {
        name: "five_hour",
        utilization: 12.4,
        // Far future → deterministic "Resets {weekday time}" branch
        // (the relative "Resets in X hr" branch is time-of-run dependent).
        resetsAt: "2099-01-01T00:00:00Z"
      },
      { name: "seven_day", utilization: 41 },
      { name: "seven_day_sonnet", utilization: 0 },
      // Codex free plan's 30-day window — labeled "Monthly".
      { name: "30_day", utilization: 9 },
      // Generated id with no dedicated label — humanized to "14d".
      { name: "14_day", utilization: 3 },
      // Truly unknown id — passes through raw.
      { name: "mystery_window", utilization: 7 }
    ],
    queriedAt: Date.now(),
    ...overrides
  };
}

describe("ProviderOfficialCard — quota section", () => {
  it("renders nothing quota-related when onRefreshQuota is omitted (unsupported CLI)", () => {
    render(<ProviderOfficialCard {...baseProps} />);
    expect(screen.queryByLabelText("Refresh usage")).toBeNull();
    expect(screen.queryByText("5h")).toBeNull();
  });

  it("renders one ring group per tier with short label + rounded percent; unknown tier names pass through", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.getByText("5h")).toBeInTheDocument();
    expect(screen.getByText("Weekly")).toBeInTheDocument();
    expect(screen.getByText("12%")).toBeInTheDocument();
    expect(screen.getByText("41%")).toBeInTheDocument();
    // The Codex 30-day window gets its proper label…
    expect(screen.getByText("Monthly")).toBeInTheDocument();
    expect(screen.queryByText("30_day")).toBeNull();
    // …other generated `{n}_day` ids are humanized…
    expect(screen.getByText("14d")).toBeInTheDocument();
    // …while truly unknown ids surface verbatim instead of being dropped.
    expect(screen.getByText("mystery_window")).toBeInTheDocument();
  });

  it("shows the reset time under the label, and the full detail in the tooltip", async () => {
    const user = userEvent.setup();
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        onRefreshQuota={vi.fn()}
      />
    );
    // Inline subline: "Resets {weekday time}" (far-future timestamp).
    expect(screen.getByText(/^Resets /)).toBeInTheDocument();
    await user.hover(screen.getByText("5h"));
    // Radix renders the open tooltip's text twice (visible content +
    // a visually-hidden copy for the aria-describedby announcement).
    const tips = await screen.findAllByText(/^5-hour · 12% · Resets /);
    expect(tips.length).toBeGreaterThan(0);
  });

  it("shows 'You haven't used X yet' for an untouched model window", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(
      screen.getByText("You haven't used Sonnet yet")
    ).toBeInTheDocument();
  });

  it("shows the subscription plan badge next to Official", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota({ plan: "Max" })}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.getByText("Max")).toBeInTheDocument();
  });

  it("renders no plan badge when the quota carries none", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.queryByText("Max")).toBeNull();
  });

  it("clicking Refresh calls onRefreshQuota", async () => {
    const user = userEvent.setup();
    const onRefresh = vi.fn();
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        onRefreshQuota={onRefresh}
      />
    );
    await user.click(screen.getByLabelText("Refresh usage"));
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });

  it("disables Refresh during the rate-limit cooldown", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        quotaCooldown
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.getByLabelText("Refresh usage")).toBeDisabled();
  });

  it("disables Refresh while loading", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota()}
        quotaLoading
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.getByLabelText("Refresh usage")).toBeDisabled();
  });

  it("renders no inline error after a failure — just the refresh button", () => {
    // Failure feedback is toast-only (manual refresh path in
    // ProvidersPage); background failures are fully silent.
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota({
          success: false,
          credentialStatus: "valid",
          tiers: [],
          error: "HTTP 429 Too Many Requests: Rate limited."
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.queryByText("5h")).toBeNull();
    expect(screen.queryByText(/HTTP 429/)).toBeNull();
    expect(screen.getByLabelText("Refresh usage")).toBeEnabled();
  });

  it("hides the whole section (refresh button too) when there is no OAuth login", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={makeQuota({
          success: false,
          credentialStatus: "not_found",
          tiers: []
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.queryByText("5h")).toBeNull();
    expect(screen.queryByLabelText("Refresh usage")).toBeNull();
  });

  it("shows only the (disabled) refresh button while the first fetch loads", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        quota={null}
        quotaLoading
        onRefreshQuota={vi.fn()}
      />
    );
    expect(screen.queryByText("5h")).toBeNull();
    expect(screen.getByLabelText("Refresh usage")).toBeDisabled();
  });
});
