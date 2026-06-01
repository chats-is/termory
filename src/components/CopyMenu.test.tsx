import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  render as rtlRender,
  screen,
  type RenderOptions
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TooltipProvider } from "@/components/ui/tooltip";
import { CopyMenu } from "./CopyMenu";

/** CopyMenu's trigger is wrapped in a shadcn Tooltip; Radix throws if
 * no TooltipProvider is in the tree. main.tsx provides one at the app
 * root — these tests provide the same. */
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

// `copyToClipboard` goes through Tauri's clipboard plugin in production;
// in jsdom there's no plugin. Mock to a spy so we can assert it was
// called with the right value.
const copyMock = vi.fn();
vi.mock("@/lib/clipboard", () => ({
  copyToClipboard: (value: string) => {
    copyMock(value);
    return Promise.resolve();
  }
}));

beforeEach(() => {
  copyMock.mockReset();
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
});

const items = [
  { label: "Copy ID", value: "id-123" },
  { label: "Copy path", value: "/p/q" }
];

describe("CopyMenu", () => {
  it("renders the trigger button collapsed by default", () => {
    render(<CopyMenu items={items} />);
    const trigger = screen.getByRole("button", { name: "Copy" });
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("toggles the menu open on trigger click", async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<CopyMenu items={items} />);
    const trigger = screen.getByRole("button", { name: "Copy" });
    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("menu")).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: /Copy ID/ })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: /Copy path/ })).toBeInTheDocument();
  });

  it("copies the selected item's value and closes the menu", async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<CopyMenu items={items} />);
    await user.click(screen.getByRole("button", { name: "Copy" }));
    await user.click(screen.getByRole("menuitem", { name: /Copy ID/ }));
    expect(copyMock).toHaveBeenCalledWith("id-123");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("shows the ✓ confirmation briefly then clears it", async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<CopyMenu items={items} />);
    await user.click(screen.getByRole("button", { name: "Copy" }));
    await user.click(screen.getByRole("menuitem", { name: /Copy ID/ }));
    // Re-open the menu to inspect the ✓ marker (we just closed it
    // when copying, so the marker only shows on the next open).
    await user.click(screen.getByRole("button", { name: "Copy" }));
    const idItem = screen.getByRole("menuitem", { name: /Copy ID/ });
    expect(idItem.querySelector("svg")).not.toBeNull();
    // After 1.2s the `copied` state clears. Wrap in act() so the
    // resulting setState lands a re-render before we assert.
    act(() => {
      vi.advanceTimersByTime(1300);
    });
    expect(
      screen.getByRole("menuitem", { name: /Copy ID/ }).querySelector("svg")
    ).toBeNull();
  });

  it("closes on Escape key", async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<CopyMenu items={items} />);
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(screen.getByRole("menu")).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes on outside mousedown", async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    const { container } = render(
      <div>
        <CopyMenu items={items} />
        <button data-testid="outside">elsewhere</button>
      </div>
    );
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(screen.getByRole("menu")).toBeInTheDocument();
    // Use pointer event sequence so the doc-level mousedown listener
    // fires the same way it does in the browser.
    await user.pointer({
      keys: "[MouseLeft]",
      target: container.querySelector("[data-testid=outside]") as Element
    });
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
