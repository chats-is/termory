import React from "react";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  screen,
  type RenderOptions
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TooltipProvider } from "@/components/ui/tooltip";
import { blankProvider } from "@/lib/provider-utils";
import { setFormatLocale } from "@/lib/format";
import type { Provider, ProviderBalance } from "@/types";
import { ProviderCard } from "./ProviderCard";

// jsdom lacks ResizeObserver, which Radix Tooltip touches when a tooltip
// opens (e.g. via userEvent hover/focus). Provide a no-op shim.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver;

// ProviderCard uses shadcn Tooltip, which needs a TooltipProvider in the tree.
// i18n falls back to English with no I18nProvider, so we assert English copy.
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

function makeProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    ...blankProvider("codex"),
    name: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    apiKey: "sk-or-v1-abcdef",
    model: "gpt-5",
    ...overrides
  };
}

// Required-but-irrelevant props collapsed into one spread for readability.
const baseProps = {
  isConfigured: false,
  isInUse: false,
  toggling: false,
  settingDefault: false,
  testing: false
};

describe("ProviderCard — Edit/Delete visibility", () => {
  it("shows Edit and Delete for a normal provider (onEdit/onDelete provided)", () => {
    render(
      <ProviderCard
        provider={makeProvider()}
        {...baseProps}
        onSetDefault={vi.fn()}
        onTest={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
      />
    );
    expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete" })).toBeInTheDocument();
  });

  it("HIDES Edit and Delete for a gateway-binding card (no onEdit/onDelete)", () => {
    render(
      <ProviderCard
        provider={makeProvider({ name: "Gateway Binding" })}
        {...baseProps}
        gatewayBadge="My Gateway"
        onSetDefault={vi.fn()}
        onTest={vi.fn()}
      />
    );
    // Bindings are managed only from the Gateways tab — no Edit/Delete here.
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
    // The AI Gateway badge marks the card's origin.
    expect(screen.getByText("AI Gateway")).toBeInTheDocument();
    // Test stays available even for bindings.
    expect(screen.getByRole("button", { name: "Test" })).toBeInTheDocument();
  });
});

describe("ProviderCard — activation", () => {
  it("renders the Activate button when not in use and fires onSetDefault", async () => {
    const user = userEvent.setup();
    const onSetDefault = vi.fn();
    render(
      <ProviderCard
        provider={makeProvider()}
        {...baseProps}
        onSetDefault={onSetDefault}
        onTest={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
      />
    );
    const activate = screen.getByRole("button", { name: "Activate" });
    await user.click(activate);
    expect(onSetDefault).toHaveBeenCalledTimes(1);
  });

  it("renders the In use badge and hides Activate when in use", () => {
    render(
      <ProviderCard
        provider={makeProvider()}
        {...baseProps}
        isInUse
        onSetDefault={vi.fn()}
        onTest={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
      />
    );
    expect(screen.getByText("In use")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Activate" })).toBeNull();
  });
});

describe("ProviderCard — test/connectivity", () => {
  it("fires onTest when the Test button is clicked", async () => {
    const user = userEvent.setup();
    const onTest = vi.fn();
    render(
      <ProviderCard
        provider={makeProvider()}
        {...baseProps}
        onSetDefault={vi.fn()}
        onTest={onTest}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
      />
    );
    await user.click(screen.getByRole("button", { name: "Test" }));
    expect(onTest).toHaveBeenCalledTimes(1);
  });

  // Same rule as the quota Refresh button: "install it first" only applies
  // while the button is disabled, so the trigger must be the wrapper.
  it("keeps the install hint reachable while the toggle is disabled", () => {
    render(
      <ProviderCard
        provider={makeProvider()}
        {...baseProps}
        activatable={false}
        onToggleEnabled={vi.fn()}
        onSetDefault={vi.fn()}
        onTest={vi.fn()}
      />
    );
    const toggle = screen.getByLabelText("Enable");
    expect(toggle).toBeDisabled();
    expect(toggle.getAttribute("data-slot")).not.toBe("tooltip-trigger");
    expect(toggle.closest('[data-slot="tooltip-trigger"]')).not.toBeNull();
  });
});

describe("ProviderCard — balance row", () => {
  // formatCurrency follows the app locale, which is the OS locale with no
  // I18nProvider mounted; pin it so the assertions hold off this machine.
  beforeAll(() => setFormatLocale("en-US"));
  afterAll(() => setFormatLocale(undefined));

  function balance(over: Partial<ProviderBalance> = {}): ProviderBalance {
    return {
      providerId: "p1",
      status: "ok",
      entries: [{ currency: "USD", remaining: 20.75, depleted: false }],
      queriedAt: Date.now(),
      ...over
    };
  }

  function renderCard(props: Record<string, unknown> = {}) {
    return render(
      <ProviderCard
        provider={makeProvider()}
        {...baseProps}
        onSetDefault={vi.fn()}
        onTest={vi.fn()}
        {...props}
      />
    );
  }

  it("renders NOTHING when no balance was ever read", () => {
    // The commonest card by far — a relay or gateway base URL, which the
    // backend answers `unsupported` without making any request.
    for (const b of [
      undefined,
      balance({ status: "unsupported", entries: undefined }),
      balance({ status: "no_key", entries: undefined })
    ]) {
      const { unmount } = renderCard({ balance: b });
      expect(screen.queryByText("Balance")).not.toBeInTheDocument();
      unmount();
    }
  });

  it("shows the amount, with the granted total where there is one", () => {
    renderCard({ balance: balance() });
    expect(screen.getByText("Balance")).toBeInTheDocument();
    expect(screen.getByText("$20.75")).toBeInTheDocument();

    const { unmount } = renderCard({
      balance: balance({
        entries: [{ currency: "USD", remaining: 20.75, total: 25, depleted: false }]
      })
    });
    expect(screen.getByText("$20.75 / $25.00")).toBeInTheDocument();
    unmount();
  });

  it("joins one amount per currency", () => {
    // DeepSeek reports balance_infos PER CURRENCY and can return several.
    renderCard({
      balance: balance({
        entries: [
          { currency: "CNY", remaining: 48.2, depleted: false },
          { currency: "USD", remaining: 6.5, depleted: false }
        ]
      })
    });
    expect(screen.getByText("CN¥48.20 · $6.50")).toBeInTheDocument();
  });

  it("KEEPS the amount on screen when the last refresh failed", () => {
    // The value slot holds a balance and nothing else — a failure changes
    // the button, never the number the user was reading.
    renderCard({
      balance: balance({ status: "error", error: "Network error" })
    });
    expect(screen.getByText("$20.75")).toBeInTheDocument();
  });

  it("explains the tint, and ONLY the tint", () => {
    // Red is the one thing the amount cannot explain itself: a vendor can
    // report an account as unable to spend while its balance is non-zero
    // (DeepSeek's is_available), so a red ¥10.00 is otherwise a colour
    // with no reason. Every other state leaves the value bare — it is a
    // value, not a status.
    const depleted = renderCard({
      balance: balance({
        entries: [{ currency: "USD", remaining: 10, depleted: true }]
      })
    });
    const red = screen.getByText("$10.00");
    expect(red).toHaveClass("text-destructive");
    expect(red.closest('[data-slot="tooltip-trigger"]')).not.toBeNull();
    depleted.unmount();

    const normal = renderCard({ balance: balance() });
    const plain = screen.getByText("$20.75");
    expect(plain).not.toHaveClass("text-destructive");
    expect(plain.closest('[data-slot="tooltip-trigger"]')).toBeNull();
    normal.unmount();
  });

  it("refreshes from its own button", async () => {
    const onRefreshBalance = vi.fn();
    renderCard({ balance: balance(), onRefreshBalance });
    await userEvent.click(
      screen.getByRole("button", { name: "Refresh balance" })
    );
    expect(onRefreshBalance).toHaveBeenCalledTimes(1);
  });

  it("has no refresh button when the card cannot refresh", () => {
    renderCard({ balance: balance() });
    expect(
      screen.queryByRole("button", { name: "Refresh balance" })
    ).not.toBeInTheDocument();
  });

  it("disables refresh while a fetch runs and during the cooldown", () => {
    for (const props of [{ balanceLoading: true }, { balanceCooldown: true }]) {
      const { unmount } = renderCard({
        balance: balance(),
        onRefreshBalance: vi.fn(),
        ...props
      });
      expect(
        screen.getByRole("button", { name: "Refresh balance" })
      ).toBeDisabled();
      unmount();
    }
  });

  // Same rule as the quota Refresh button: the hints exist only for the
  // states that disable the button, so a disabled element — which fires
  // no hover events — can never be the trigger.
  it("keeps the button's hints reachable while it is disabled", () => {
    for (const props of [
      { balanceCooldown: true },
      { balance: balance({ status: "error", error: "boom" }), balanceCooldown: true }
    ]) {
      const { unmount } = renderCard({
        balance: balance(),
        onRefreshBalance: vi.fn(),
        ...props
      });
      const button = screen.getByRole("button", { name: "Refresh balance" });
      expect(button).toBeDisabled();
      expect(button.getAttribute("data-slot")).not.toBe("tooltip-trigger");
      expect(button.closest('[data-slot="tooltip-trigger"]')).not.toBeNull();
      unmount();
    }
  });
});
