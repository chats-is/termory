import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CommandPalette } from "./CommandPalette";
import type { AppSession, SearchHit } from "../types";

// The palette reads results through useSearchHits → invoke("search_all_sessions").
// Stub the IPC so the hook resolves (or, by default, returns nothing — then the
// synchronous metadata-only `fallbackHits` path drives the rows without waiting
// on the 300ms debounce).
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

// cmdk (the command palette primitive) instantiates a ResizeObserver, which
// jsdom doesn't provide. A no-op stub is enough — there's no layout to observe.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver;

// cmdk scrolls the active item into view on selection changes; jsdom has no
// layout so Element.scrollIntoView is undefined. Stub it.
Element.prototype.scrollIntoView ??= function scrollIntoView() {};

function mkSession(partial: Partial<AppSession> = {}): AppSession {
  return {
    id: "id-1",
    source: "Claude",
    title: "Fix the flaky stats test",
    project: "/Users/john/Documents/termory",
    path: "/Users/john/.claude/projects/x/session.jsonl",
    message_count: 3,
    preview: "",
    message_previews: [],
    ...partial
  };
}

/** Open the palette via the global ⌘K listener and wait for the dialog. */
async function openPalette(user: ReturnType<typeof userEvent.setup>) {
  await user.keyboard("{Meta>}k{/Meta}");
  return screen.findByRole("dialog");
}

const baseProps = () => ({
  sessions: [] as AppSession[],
  onOpenItem: vi.fn(),
  recentSearches: [] as string[],
  onCommitSearch: vi.fn(),
  onClearRecent: vi.fn()
});

beforeEach(() => {
  invokeMock.mockReset();
  // Default: backend yields no hits, so the rows come from `fallbackHits`.
  invokeMock.mockResolvedValue([] as SearchHit[]);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("CommandPalette", () => {
  it("is closed until ⌘K is pressed", async () => {
    const user = userEvent.setup();
    render(<CommandPalette {...baseProps()} />);

    expect(screen.queryByRole("dialog")).toBeNull();

    await openPalette(user);
    expect(
      screen.getByPlaceholderText("Find sessions, memories, skills…")
    ).toBeInTheDocument();
  });

  it("filters the metadata fallback rows by the typed query", async () => {
    const user = userEvent.setup();
    const sessions = [
      mkSession({ id: "a", title: "Refactor the gateway editor" }),
      mkSession({ id: "b", title: "Fix the flaky stats test" })
    ];
    render(<CommandPalette {...baseProps()} sessions={sessions} />);
    await openPalette(user);

    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "gateway"
    );

    // Only the matching session shows; the other is filtered out.
    expect(screen.getByText("Refactor the gateway editor")).toBeInTheDocument();
    expect(screen.queryByText("Fix the flaky stats test")).toBeNull();
    // Fallback rows live under the "Matching" group heading.
    expect(screen.getByText("Matching")).toBeInTheDocument();
  });

  it("fires onOpenItem with the selected session and no index for fallback rows", async () => {
    const user = userEvent.setup();
    const onOpenItem = vi.fn();
    const onCommitSearch = vi.fn();
    const session = mkSession({ id: "a", title: "Refactor the gateway editor" });
    render(
      <CommandPalette
        {...baseProps()}
        sessions={[session]}
        onOpenItem={onOpenItem}
        onCommitSearch={onCommitSearch}
      />
    );
    await openPalette(user);
    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "gateway"
    );

    await user.click(screen.getByText("Refactor the gateway editor"));

    expect(onOpenItem).toHaveBeenCalledTimes(1);
    // fallbackHits carry no first_match_index → messageIndex is undefined.
    expect(onOpenItem).toHaveBeenCalledWith(session, undefined);
    expect(onCommitSearch).toHaveBeenCalledWith("gateway");
    // Selecting closes the dialog.
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("passes first_match_index through to onOpenItem for backend hits", async () => {
    const user = userEvent.setup();
    const session = mkSession({ id: "a", title: "Backend hit session" });
    invokeMock.mockResolvedValue([
      {
        session,
        snippet: "…matched…",
        role: "assistant",
        match_count: 2,
        first_match_index: 7
      }
    ] satisfies SearchHit[]);

    const onOpenItem = vi.fn();
    render(
      <CommandPalette {...baseProps()} onOpenItem={onOpenItem} />
    );
    await openPalette(user);
    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "needle"
    );

    // useSearchHits debounces 300ms then invokes; the hit appears under "Results".
    const hitRow = await screen.findByText("Backend hit session", {}, { timeout: 2000 });
    await waitFor(() =>
      expect(screen.getByText("Results")).toBeInTheDocument()
    );

    await user.click(hitRow);
    expect(onOpenItem).toHaveBeenCalledWith(session, 7);
  });

  it("shows recent searches when the query is empty and seeds the input on click", async () => {
    const user = userEvent.setup();
    render(
      <CommandPalette
        {...baseProps()}
        recentSearches={["gateway", "stats heatmap"]}
      />
    );
    await openPalette(user);

    expect(screen.getByText("Recent searches")).toBeInTheDocument();
    const recent = screen.getByText("gateway");
    expect(recent).toBeInTheDocument();

    await user.click(recent);

    expect(
      screen.getByPlaceholderText<HTMLInputElement>(
        "Find sessions, memories, skills…"
      ).value
    ).toBe("gateway");
  });

  it("shows the empty-state hint when there is no query and no recents", async () => {
    const user = userEvent.setup();
    render(<CommandPalette {...baseProps()} />);
    await openPalette(user);

    expect(
      screen.getByText("Type to search across all records.")
    ).toBeInTheDocument();
  });

  it("closes on ⌘K toggle when already open", async () => {
    const user = userEvent.setup();
    render(<CommandPalette {...baseProps()} />);

    await openPalette(user);
    expect(screen.getByRole("dialog")).toBeInTheDocument();

    await user.keyboard("{Meta>}k{/Meta}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });
});
