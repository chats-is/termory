import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  fireEvent,
  render as rtlRender,
  screen,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { MessageList } from "./MessageList";
import { favoriteKey } from "../lib/favorites";
import type { AppSession, SessionMessage } from "../types";

/** Star buttons are wrapped in shadcn Tooltip; Radix throws without a
 * TooltipProvider in the tree (main.tsx mounts one at the root). */
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

// `@tanstack/react-virtual` reports a 0-height viewport in jsdom (no
// layout engine), so its `getVirtualItems()` always returns []. We
// replace it with a passthrough that renders every item — virtualization
// is purely a performance optimization; the per-message rendering logic
// and star-button wiring is the same with or without it.
vi.mock("@tanstack/react-virtual", () => {
  return {
    useVirtualizer: (opts: { count: number; estimateSize: () => number }) => {
      const size = opts.estimateSize();
      const items = Array.from({ length: opts.count }, (_, i) => ({
        index: i,
        key: i,
        start: i * size,
        size,
        end: (i + 1) * size,
        lane: 0
      }));
      return {
        getVirtualItems: () => items,
        getTotalSize: () => opts.count * size,
        measureElement: () => 0,
        scrollToIndex: () => {}
      };
    }
  };
});

function mkMessage(partial: Partial<SessionMessage> = {}): SessionMessage {
  return {
    role: "assistant",
    text: "hello",
    timestamp: null,
    kind: "text",
    ...partial
  };
}

function mkSession(partial: Partial<AppSession> = {}): AppSession {
  return {
    id: "s1",
    source: "Claude",
    title: "t",
    project: "",
    path: "/p.jsonl",
    started_at: null,
    updated_at: null,
    message_count: 0,
    preview: "",
    snippet: "",
    message_previews: [],
    ...partial
  };
}

describe("MessageList — star button visibility", () => {
  it("does not render any star button when the `favorites` prop is omitted", () => {
    render(
      <MessageList messages={[mkMessage(), mkMessage({ role: "user" })]} />
    );
    expect(
      screen.queryAllByRole("button", { name: /favorite/i })
    ).toHaveLength(0);
  });

  it("renders one star button per message when `favorites` is provided", () => {
    const session = mkSession();
    render(
      <MessageList
        messages={[mkMessage(), mkMessage(), mkMessage()]}
        favorites={{
          session,
          keys: new Set(),
          onToggle: () => {}
        }}
      />
    );
    // Three messages → three star buttons.
    const buttons = screen.getAllByRole("button", {
      name: /favorites/i
    });
    expect(buttons.length).toBeGreaterThanOrEqual(3);
  });
});

describe("MessageList — star button state + toggle", () => {
  it("labels favorited messages with 'Remove from favorites' and others with 'Add to favorites'", () => {
    const session = mkSession({ id: "s1", source: "Claude" });
    // Mark only the message at index 1 as favorited.
    const keys = new Set<string>();
    keys.add(favoriteKey(session.source, session.id, 1));
    render(
      <MessageList
        messages={[mkMessage(), mkMessage(), mkMessage()]}
        favorites={{
          session,
          keys,
          onToggle: () => {}
        }}
      />
    );
    expect(
      screen.getAllByRole("button", { name: "Remove from favorites" })
    ).toHaveLength(1);
    // The other two are not favorited.
    expect(
      screen.getAllByRole("button", { name: "Add to favorites" }).length
    ).toBeGreaterThanOrEqual(2);
  });

  it("calls onToggle(message, index) when the star is clicked", () => {
    const session = mkSession();
    const onToggle = vi.fn();
    const messages = [
      mkMessage({ text: "first" }),
      mkMessage({ text: "second" })
    ];
    render(
      <MessageList
        messages={messages}
        favorites={{
          session,
          keys: new Set(),
          onToggle
        }}
      />
    );
    // Click the first star (Add to favorites). The virtualizer renders
    // at least the visible window in jsdom — both are within view.
    const addButtons = screen.getAllByRole("button", {
      name: "Add to favorites"
    });
    fireEvent.click(addButtons[0]);
    expect(onToggle).toHaveBeenCalledTimes(1);
    // First arg is the SessionMessage, second arg is the index.
    expect(onToggle.mock.calls[0][0]).toEqual(messages[0]);
    expect(onToggle.mock.calls[0][1]).toBe(0);
  });

  it("stops click propagation so the message row itself isn't activated", () => {
    const session = mkSession();
    const onToggle = vi.fn();
    const onArticleClick = vi.fn();
    const { container } = render(
      <div onClick={onArticleClick}>
        <MessageList
          messages={[mkMessage()]}
          favorites={{
            session,
            keys: new Set(),
            onToggle
          }}
        />
      </div>
    );
    const star = container.querySelector(
      "button[aria-label='Add to favorites']"
    ) as HTMLButtonElement;
    fireEvent.click(star);
    expect(onToggle).toHaveBeenCalledTimes(1);
    // The outer onClick should NOT fire — stopPropagation is used.
    expect(onArticleClick).not.toHaveBeenCalled();
  });
});

