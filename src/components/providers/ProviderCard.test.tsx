import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  screen,
  type RenderOptions
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TooltipProvider } from "@/components/ui/tooltip";
import { blankProvider } from "@/lib/provider-utils";
import type { Provider } from "@/types";
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
});
