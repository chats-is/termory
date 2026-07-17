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
import { copyToClipboard } from "@/lib/clipboard";
import type { AppSession, SessionMessage } from "../types";

vi.mock("@/lib/clipboard", () => ({ copyToClipboard: vi.fn() }));

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

describe("MessageList — find highlight", () => {
  it("marks matching rows with data-find and the current one as 'current'", () => {
    const { container } = render(
      <MessageList
        messages={[
          mkMessage({ text: "alpha" }),
          mkMessage({ text: "beta" }),
          mkMessage({ text: "gamma" })
        ]}
        find={{ query: "a", indices: new Set([0, 2]), current: 2 }}
      />
    );
    const articles = container.querySelectorAll("article");
    expect(articles[0].getAttribute("data-find")).toBe("match");
    expect(articles[1].hasAttribute("data-find")).toBe(false);
    expect(articles[2].getAttribute("data-find")).toBe("current");
  });

  it("wraps the matched term in <mark> inside matching messages only", () => {
    const { container } = render(
      <MessageList
        messages={[
          mkMessage({ text: "say hello world" }),
          mkMessage({ text: "no match here" })
        ]}
        find={{ query: "hello", indices: new Set([0]), current: 0 }}
      />
    );
    const marks = container.querySelectorAll("article mark");
    expect(marks).toHaveLength(1);
    expect(marks[0].textContent).toBe("hello");
    // The non-matching message body has no marks.
    expect(
      container.querySelectorAll("article")[1].querySelectorAll("mark")
    ).toHaveLength(0);
  });

  it("marks nothing when the find prop is omitted", () => {
    const { container } = render(
      <MessageList messages={[mkMessage(), mkMessage()]} />
    );
    expect(container.querySelectorAll("article[data-find]")).toHaveLength(0);
    expect(container.querySelectorAll("mark")).toHaveLength(0);
  });
});

describe("MessageList — per-message copy button", () => {
  it("renders one copy button per message even without the favorites prop", () => {
    render(
      <MessageList messages={[mkMessage(), mkMessage({ role: "user" })]} />
    );
    expect(screen.getAllByRole("button", { name: "Copy" })).toHaveLength(2);
  });

  it("copies the clicked message's text and flips to the copied state", async () => {
    const messages = [
      mkMessage({ text: "first body" }),
      mkMessage({ text: "second body" })
    ];
    render(<MessageList messages={messages} />);
    const buttons = screen.getAllByRole("button", { name: "Copy" });
    fireEvent.click(buttons[1]);
    expect(copyToClipboard).toHaveBeenCalledWith("second body");
    // After the (mocked) clipboard write resolves, the button shows
    // the transient confirmation label.
    expect(await screen.findByRole("button", { name: "Copied" })).toBeTruthy();
  });

  it("stops click propagation so the surrounding row isn't activated", () => {
    const onArticleClick = vi.fn();
    const { container } = render(
      <div onClick={onArticleClick}>
        <MessageList messages={[mkMessage()]} />
      </div>
    );
    const copy = container.querySelector(
      "button[aria-label='Copy']"
    ) as HTMLButtonElement;
    fireEvent.click(copy);
    expect(onArticleClick).not.toHaveBeenCalled();
  });
});

