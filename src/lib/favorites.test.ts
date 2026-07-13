import { describe, expect, it } from "vitest";
import type { AppSession, Favorite, SessionMessage } from "../types";
import {
  favoriteKey,
  favoriteKeySet,
  isFavoriteList,
  makeFavorite,
  sortFavoritesDesc,
  toggleFavoriteEntry
} from "./favorites";

function mkSession(partial: Partial<AppSession> = {}): AppSession {
  return {
    id: "s1",
    source: "Claude",
    title: "Session A",
    project: "/repo/a",
    path: "/p",
    started_at: null,
    updated_at: null,
    message_count: 0,
    preview: "",
    snippet: "",
    message_previews: [],
    ...partial
  };
}

function mkMessage(partial: Partial<SessionMessage> = {}): SessionMessage {
  return {
    role: "assistant",
    text: "hello",
    timestamp: "2026-05-30T10:00:00Z",
    kind: "text",
    ...partial
  };
}

function mkFavorite(partial: Partial<Favorite> = {}): Favorite {
  return {
    id: "f1",
    favorited_at: "2026-05-30T10:00:00Z",
    message: mkMessage(),
    source: "Claude",
    source_session_id: "s1",
    source_session_path: "/p",
    source_session_title: "Session A",
    source_session_project: "/repo/a",
    source_message_index: 0,
    ...partial
  };
}

describe("favoriteKey", () => {
  it("composes (source, sessionId, index) into a deterministic key", () => {
    expect(favoriteKey("Claude", "abc", 3)).toBe("Claude::abc::3");
  });
  it("differs across sources even with the same session id", () => {
    // Defensive: session ids collide across CLIs in the wild — the
    // tuple key must distinguish them.
    expect(favoriteKey("Claude", "x", 0)).not.toBe(
      favoriteKey("Codex", "x", 0)
    );
  });
});

describe("favoriteKeySet", () => {
  it("returns a Set of keys for every favorite", () => {
    const set = favoriteKeySet([
      mkFavorite({ id: "f1", source_session_id: "s1", source_message_index: 0 }),
      mkFavorite({ id: "f2", source_session_id: "s2", source_message_index: 5 })
    ]);
    expect(set.has("Claude::s1::0")).toBe(true);
    expect(set.has("Claude::s2::5")).toBe(true);
    expect(set.has("Claude::s3::0")).toBe(false);
  });
  it("handles an empty input", () => {
    expect(favoriteKeySet([]).size).toBe(0);
  });
});

describe("makeFavorite", () => {
  it("snapshots full session metadata + message", () => {
    const session = mkSession({
      id: "s9",
      title: "Migration plan",
      project: "/repo/x"
    });
    const message = mkMessage({ text: "Apply migration 0042" });
    const fav = makeFavorite(session, message, 7);
    expect(fav.message).toEqual(message);
    expect(fav.source).toBe("Claude");
    expect(fav.source_session_id).toBe("s9");
    expect(fav.source_session_title).toBe("Migration plan");
    expect(fav.source_session_project).toBe("/repo/x");
    expect(fav.source_message_index).toBe(7);
  });
  it("generates a unique id per favorite", () => {
    const a = makeFavorite(mkSession(), mkMessage(), 0);
    const b = makeFavorite(mkSession(), mkMessage(), 0);
    expect(a.id).not.toBe(b.id);
  });
  it("narrows unknown AppSession.source to a SessionSource", () => {
    // Defensive: AppSession.source is `string` on the wire. If a stray
    // value comes through we fall back to a known source rather than
    // breaking the Favorite type contract downstream.
    const fav = makeFavorite(
      mkSession({ source: "Bogus" as never }),
      mkMessage(),
      0
    );
    expect(["Claude", "Codex", "Gemini", "OpenCode", "Grok"]).toContain(fav.source);
  });
  it("populates favorited_at with a parseable ISO timestamp", () => {
    const before = Date.now();
    const fav = makeFavorite(mkSession(), mkMessage(), 0);
    const ts = new Date(fav.favorited_at).getTime();
    expect(Number.isNaN(ts)).toBe(false);
    expect(ts).toBeGreaterThanOrEqual(before);
  });
});

describe("toggleFavoriteEntry", () => {
  it("adds a new favorite when the message isn't already starred", () => {
    const next = toggleFavoriteEntry([], mkSession(), mkMessage(), 0);
    expect(next).toHaveLength(1);
    expect(next[0].source_message_index).toBe(0);
  });
  it("removes the favorite when the same (source, session, index) is toggled again", () => {
    const session = mkSession({ id: "s9" });
    const message = mkMessage();
    const after_add = toggleFavoriteEntry([], session, message, 3);
    const after_remove = toggleFavoriteEntry(after_add, session, message, 3);
    expect(after_remove).toHaveLength(0);
  });
  it("preserves other entries when toggling one off", () => {
    const session = mkSession();
    const a = mkFavorite({
      id: "a",
      source_session_id: "s1",
      source_message_index: 0
    });
    const b = mkFavorite({
      id: "b",
      source_session_id: "s1",
      source_message_index: 5
    });
    const next = toggleFavoriteEntry([a, b], session, mkMessage(), 0);
    expect(next.map((f) => f.id)).toEqual(["b"]);
  });
  it("does not mutate the input array", () => {
    const input = [
      mkFavorite({ id: "a", source_session_id: "s1", source_message_index: 0 })
    ];
    const snapshot = [...input];
    toggleFavoriteEntry(input, mkSession(), mkMessage(), 5);
    expect(input).toEqual(snapshot);
  });
});

describe("sortFavoritesDesc", () => {
  it("returns newest first by favorited_at", () => {
    const oldest = mkFavorite({ id: "old", favorited_at: "2026-05-01T00:00:00Z" });
    const newest = mkFavorite({ id: "new", favorited_at: "2026-06-01T00:00:00Z" });
    const mid = mkFavorite({ id: "mid", favorited_at: "2026-05-15T00:00:00Z" });
    expect(sortFavoritesDesc([oldest, newest, mid]).map((f) => f.id)).toEqual([
      "new",
      "mid",
      "old"
    ]);
  });
  it("does not mutate the input array", () => {
    const input = [
      mkFavorite({ id: "a", favorited_at: "2026-05-01T00:00:00Z" }),
      mkFavorite({ id: "b", favorited_at: "2026-06-01T00:00:00Z" })
    ];
    const snapshot = [...input];
    sortFavoritesDesc(input);
    expect(input).toEqual(snapshot);
  });
});

describe("isFavoriteList", () => {
  it("accepts a valid favorite array", () => {
    expect(isFavoriteList([mkFavorite()])).toBe(true);
  });
  it("accepts an empty array", () => {
    expect(isFavoriteList([])).toBe(true);
  });
  it("rejects non-arrays", () => {
    expect(isFavoriteList(null)).toBe(false);
    expect(isFavoriteList({})).toBe(false);
    expect(isFavoriteList("[]")).toBe(false);
  });
  it("rejects entries missing required fields", () => {
    // Defensive against hand-edited favorites.json: any entry without
    // the core (id / message / source_session_id) triplet is dropped,
    // not the whole list.
    expect(isFavoriteList([{ id: "f1" }])).toBe(false);
    expect(isFavoriteList([{ id: "f1", message: mkMessage() }])).toBe(false);
    expect(isFavoriteList([{ message: mkMessage(), source_session_id: "s1" }])).toBe(
      false
    );
  });
});
