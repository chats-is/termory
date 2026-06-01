import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  fireEvent,
  render as rtlRender,
  screen,
  within,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { FavoritesPage } from "./FavoritesPage";
import type { AppSession, Favorite, SessionMessage } from "../../types";

/** FavoritesPage's detail-header action buttons are wrapped in shadcn
 * Tooltip; Radix throws without a TooltipProvider in the tree (main.tsx
 * mounts one at the root). */
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

function mkMessage(partial: Partial<SessionMessage> = {}): SessionMessage {
  return {
    role: "assistant",
    text: "hello there",
    timestamp: "2026-05-30T10:00:00Z",
    kind: "text",
    ...partial
  };
}

function mkFavorite(partial: Partial<Favorite> = {}): Favorite {
  return {
    id: "f-default",
    favorited_at: "2026-05-30T10:00:00Z",
    message: mkMessage(),
    source: "Claude",
    source_session_id: "s-default",
    source_session_path: "/p/default.jsonl",
    source_session_title: "Default session",
    source_session_project: "/repo/default",
    source_message_index: 0,
    ...partial
  };
}

function mkSession(partial: Partial<AppSession> = {}): AppSession {
  return {
    id: "s-default",
    source: "Claude",
    title: "Default session",
    project: "/repo/default",
    path: "/p/default.jsonl",
    started_at: null,
    updated_at: null,
    message_count: 0,
    preview: "",
    snippet: "",
    message_previews: [],
    ...partial
  };
}

describe("FavoritesPage — empty", () => {
  it("renders the empty state when there are no favorites", () => {
    render(
      <FavoritesPage
        favorites={[]}
        sessions={[]}
        onOpenSource={() => {}}
        onRemove={() => {}}
      />
    );
    expect(screen.getByText(/No favorites yet/i)).toBeInTheDocument();
    // The recommended action describes how to add a favorite — make
    // sure it stays touch-friendly ("click" not "hover").
    expect(
      screen.getByText(/Click the star next to any message/i)
    ).toBeInTheDocument();
  });
});

describe("FavoritesPage — list", () => {
  it("shows every favorite, newest first by favorited_at", () => {
    const favorites: Favorite[] = [
      mkFavorite({
        id: "old",
        favorited_at: "2026-05-01T00:00:00Z",
        source_session_title: "OLD session"
      }),
      mkFavorite({
        id: "new",
        favorited_at: "2026-06-01T00:00:00Z",
        source_session_title: "NEW session"
      }),
      mkFavorite({
        id: "mid",
        favorited_at: "2026-05-15T00:00:00Z",
        source_session_title: "MID session"
      })
    ];
    render(
      <FavoritesPage
        favorites={favorites}
        sessions={[]}
        onOpenSource={() => {}}
        onRemove={() => {}}
      />
    );
    // All three titles appear in the list
    const newCard = screen.getAllByText("NEW session");
    const midCard = screen.getAllByText("MID session");
    const oldCard = screen.getAllByText("OLD session");
    expect(newCard.length).toBeGreaterThanOrEqual(1);
    expect(midCard.length).toBeGreaterThanOrEqual(1);
    expect(oldCard.length).toBeGreaterThanOrEqual(1);

    // The DOM order of the title nodes inside the <aside> list column
    // proves the sort.
    const aside = document.querySelector("aside") as HTMLElement;
    const titles = within(aside)
      .getAllByText(/session$/i)
      .map((el) => el.textContent);
    expect(titles).toEqual(["NEW session", "MID session", "OLD session"]);
  });
});

describe("FavoritesPage — auto-select", () => {
  it("auto-selects the newest favorite on mount", () => {
    const favorites: Favorite[] = [
      mkFavorite({
        id: "old",
        favorited_at: "2026-05-01T00:00:00Z",
        source_session_title: "OLD",
        message: mkMessage({ text: "old-body-marker" })
      }),
      mkFavorite({
        id: "new",
        favorited_at: "2026-06-01T00:00:00Z",
        source_session_title: "NEW",
        message: mkMessage({ text: "new-body-marker" })
      })
    ];
    render(
      <FavoritesPage
        favorites={favorites}
        sessions={[]}
        onOpenSource={() => {}}
        onRemove={() => {}}
      />
    );
    // The detail pane (<section>) holds the rendered MessageBody for
    // the selected favorite. Scope the query there so we don't match
    // the same text echoed in the list-card snippet.
    const detail = document.querySelector("section") as HTMLElement;
    expect(within(detail).getByText("new-body-marker")).toBeInTheDocument();
    expect(
      within(detail).queryByText("old-body-marker")
    ).not.toBeInTheDocument();
  });

  it("clicking a list card swaps the detail pane", () => {
    const favorites: Favorite[] = [
      mkFavorite({
        id: "a",
        favorited_at: "2026-06-01T00:00:00Z",
        source_session_title: "Alpha",
        message: mkMessage({ text: "alpha-body-marker" })
      }),
      mkFavorite({
        id: "b",
        favorited_at: "2026-05-01T00:00:00Z",
        source_session_title: "Beta",
        message: mkMessage({ text: "beta-body-marker" })
      })
    ];
    render(
      <FavoritesPage
        favorites={favorites}
        sessions={[]}
        onOpenSource={() => {}}
        onRemove={() => {}}
      />
    );
    const detail = document.querySelector("section") as HTMLElement;
    // Alpha is newest → auto-selected
    expect(within(detail).getByText("alpha-body-marker")).toBeInTheDocument();

    // Click the Beta card in the list aside (use the first match — the
    // <span> with the title text — and walk up to its <button>).
    const aside = document.querySelector("aside") as HTMLElement;
    const betaCard = within(aside).getByText("Beta").closest("button");
    fireEvent.click(betaCard as HTMLButtonElement);

    expect(within(detail).getByText("beta-body-marker")).toBeInTheDocument();
    expect(
      within(detail).queryByText("alpha-body-marker")
    ).not.toBeInTheDocument();
  });
});

describe("FavoritesPage — Open original / archived", () => {
  it("renders the Open original button when the source session is still present", () => {
    const fav = mkFavorite({
      source_session_id: "live-session",
      source_message_index: 7
    });
    const live = mkSession({ id: "live-session" });
    const onOpenSource = vi.fn();
    render(
      <FavoritesPage
        favorites={[fav]}
        sessions={[live]}
        onOpenSource={onOpenSource}
        onRemove={() => {}}
      />
    );
    const openButton = screen.getByRole("button", {
      name: /open original session/i
    });
    fireEvent.click(openButton);
    expect(onOpenSource).toHaveBeenCalledTimes(1);
    expect(onOpenSource).toHaveBeenCalledWith(live, 7);
  });

  it("renders the 'archived' label when the source session is missing from scans", () => {
    const fav = mkFavorite({ source_session_id: "deleted-session" });
    render(
      <FavoritesPage
        favorites={[fav]}
        sessions={[]} // no source session anymore
        onOpenSource={() => {}}
        onRemove={() => {}}
      />
    );
    expect(screen.getByText(/archived/i)).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /open original session/i })
    ).not.toBeInTheDocument();
  });
});

describe("FavoritesPage — remove", () => {
  it("calls onRemove(id) when the trash button is clicked", () => {
    const fav = mkFavorite({ id: "to-remove" });
    const onRemove = vi.fn();
    render(
      <FavoritesPage
        favorites={[fav]}
        sessions={[]}
        onOpenSource={() => {}}
        onRemove={onRemove}
      />
    );
    const removeBtn = screen.getByRole("button", { name: /remove favorite/i });
    fireEvent.click(removeBtn);
    expect(onRemove).toHaveBeenCalledWith("to-remove");
  });
});

describe("FavoritesPage — message rendering", () => {
  it("renders the message role and runs the full markdown body through react-markdown", () => {
    const fav = mkFavorite({
      message: mkMessage({
        role: "assistant",
        text: "## Detail-Heading-Marker\n\nSome **detail-bold-marker** content."
      })
    });
    render(
      <FavoritesPage
        favorites={[fav]}
        sessions={[]}
        onOpenSource={() => {}}
        onRemove={() => {}}
      />
    );
    const detail = document.querySelector("section") as HTMLElement;
    // Role chip — only inside the detail section (the list card uses
    // a colored bar, not a text role label).
    expect(within(detail).getByText(/^assistant$/i)).toBeInTheDocument();
    // Markdown was rendered, not raw-printed: the bold word is inside
    // a <strong> tag, and the heading is an <h2>.
    const heading = within(detail).getByText(/Detail-Heading-Marker/);
    expect(heading.tagName).toBe("H2");
    const bold = within(detail).getByText(/detail-bold-marker/);
    expect(bold.tagName).toBe("STRONG");
  });
});
