import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
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
  onOpenItem: vi.fn(),
  onCommitSearch: vi.fn()
});

/** The palette is a CONTROLLED component (App owns `open` so its ⌘F
 * handler can see palette state); this harness supplies that state so
 * tests keep exercising the real ⌘K listener end-to-end. */
function PaletteHarness(
  props: Omit<React.ComponentProps<typeof CommandPalette>, "open" | "onOpenChange">
) {
  const [open, setOpen] = React.useState(false);
  return <CommandPalette open={open} onOpenChange={setOpen} {...props} />;
}

beforeEach(() => {
  invokeMock.mockReset();
  // Default: backend yields no hits, so the rows come from `fallbackHits`.
  invokeMock.mockResolvedValue([] as SearchHit[]);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("CommandPalette — shortcut ownership (revised plan)", () => {
  it("⌘F does NOT toggle the palette — it belongs to the in-session find", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);
    await user.keyboard("{Meta>}f{/Meta}");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("⌘⇧K does not toggle the palette (shift excluded)", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);
    await user.keyboard("{Meta>}{Shift>}k{/Shift}{/Meta}");
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

describe("CommandPalette — view-all-results bridge", () => {
  it("shows the bridge for a typed query and fires onOpenSearchPage", async () => {
    const user = userEvent.setup();
    const onOpenSearchPage = vi.fn();
    const onCommitSearch = vi.fn();
    render(
      <PaletteHarness
        {...baseProps()}
        onCommitSearch={onCommitSearch}
        onOpenSearchPage={onOpenSearchPage}
      />
    );
    await openPalette(user);
    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "gateway"
    );
    const bridge = screen.getByText('View all results for “gateway” in Search');
    await user.click(bridge);
    expect(onOpenSearchPage).toHaveBeenCalledWith("gateway");
    // Bridging no longer records a recent search — that happens in
    // useSearchHits only when a query actually returns results.
    expect(onCommitSearch).not.toHaveBeenCalled();
    // Palette closes after bridging.
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("hides the bridge when no onOpenSearchPage handler is given", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);
    await openPalette(user);
    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "gateway"
    );
    expect(screen.queryByText(/View all results/)).toBeNull();
  });
});

describe("CommandPalette", () => {
  it("is closed until ⌘K is pressed", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);

    expect(screen.queryByRole("dialog")).toBeNull();

    await openPalette(user);
    expect(
      screen.getByPlaceholderText("Find sessions, memories, skills…")
    ).toBeInTheDocument();
  });

  it("shows backend hits ONLY (no instant metadata rows) and passes first_match_index through", async () => {
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
    const onCommitSearch = vi.fn();
    render(
      <PaletteHarness
        {...baseProps()}
        onOpenItem={onOpenItem}
        onCommitSearch={onCommitSearch}
      />
    );
    await openPalette(user);
    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "needle"
    );

    // Nothing shows until the debounced backend search settles — the
    // palette runs in lockstep with the Search page: no title-only
    // fallback rows flashing in first, and CRUCIALLY no premature
    // "No matches." during the debounce window (the flash bug).
    expect(screen.queryByText("Backend hit session")).toBeNull();
    expect(screen.queryByText("No matches.")).toBeNull();
    const hitRow = await screen.findByText("Backend hit session", {}, { timeout: 2000 });

    await user.click(hitRow);
    expect(onOpenItem).toHaveBeenCalledWith(session, 7, "needle");
    expect(onCommitSearch).toHaveBeenCalledWith("needle");
    // Selecting closes the dialog.
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("renders Records-card-style rows: highlighted snippet + source icon", async () => {
    const user = userEvent.setup();
    const session = mkSession({ id: "a", title: "Layout row" });
    invokeMock.mockResolvedValue([
      {
        session,
        snippet: "before needle after",
        role: "assistant",
        match_count: 3,
        first_match_index: 1
      }
    ] satisfies SearchHit[]);
    render(<PaletteHarness {...baseProps()} />);
    await openPalette(user);
    await user.type(
      screen.getByPlaceholderText("Find sessions, memories, skills…"),
      "needle"
    );
    await screen.findByText("Layout row", {}, { timeout: 2000 });
    // Snippet line renders with the matched term wrapped in <mark>.
    expect(
      await screen.findByText("needle", { selector: "mark" })
    ).toBeInTheDocument();
    // Source is an icon (aria-labelled span), not a text name.
    expect(screen.getByLabelText("Claude Code")).toBeInTheDocument();
    expect(screen.queryByText("Claude Code")).toBeNull();
  });

  it("clear-X in the input empties the query and keeps the dialog open", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);
    await openPalette(user);
    const input = screen.getByPlaceholderText<HTMLInputElement>(
      "Find sessions, memories, skills…"
    );
    await user.type(input, "gateway");
    expect(input.value).toBe("gateway");
    await user.click(screen.getByRole("button", { name: "Clear" }));
    expect(input.value).toBe("");
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("shows the empty-state hint when there is no query", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);
    await openPalette(user);

    expect(
      screen.getByText("Type to search across all records.")
    ).toBeInTheDocument();
  });

  it("closes on ⌘K toggle when already open", async () => {
    const user = userEvent.setup();
    render(<PaletteHarness {...baseProps()} />);

    await openPalette(user);
    expect(screen.getByRole("dialog")).toBeInTheDocument();

    await user.keyboard("{Meta>}k{/Meta}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });
});
