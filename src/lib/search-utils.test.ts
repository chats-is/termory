import { describe, expect, it } from "vitest";
import { matchingLoweredIndices, splitSnippet } from "./search-utils";

describe("matchingLoweredIndices", () => {
  // Production callers pre-lowercase once per transcript (App.tsx memo).
  const msgs = (...texts: string[]) => texts.map((text) => text.toLowerCase());

  it("returns [] for an empty or whitespace-only query", () => {
    expect(matchingLoweredIndices(msgs("a", "b"), "")).toEqual([]);
    expect(matchingLoweredIndices(msgs("a", "b"), "   ")).toEqual([]);
  });

  it("matches case-insensitively as a substring", () => {
    expect(
      matchingLoweredIndices(msgs("Hello World", "nothing", "say hello"), "HELLO")
    ).toEqual([0, 2]);
  });

  it("trims the query before matching", () => {
    expect(matchingLoweredIndices(msgs("a todo b"), " todo ")).toEqual([0]);
  });

  it("returns [] when nothing matches", () => {
    expect(matchingLoweredIndices(msgs("aaa", "bbb"), "zzz")).toEqual([]);
  });

  it("matches markdown-formatted tool text verbatim (no stripping)", () => {
    // Message bodies are the formatted markdown — `**Bash**(...)` etc. —
    // so the marker characters themselves are matchable, same as the
    // backend search.
    expect(matchingLoweredIndices(msgs("⏺ **Bash**(`ls -la`)"), "**bash**")).toEqual([0]);
  });

  it("handles an empty message list", () => {
    expect(matchingLoweredIndices([], "x")).toEqual([]);
  });
});

describe("splitSnippet", () => {
  it("returns the whole snippet as a non-match when the query is empty", () => {
    expect(splitSnippet("hello world", "")).toEqual([
      { text: "hello world", match: false }
    ]);
  });

  it("returns a single non-match segment when the query isn't found", () => {
    expect(splitSnippet("hello", "xyz")).toEqual([
      { text: "hello", match: false }
    ]);
  });

  it("splits around a single match, keeping the surrounding text", () => {
    expect(splitSnippet("a TODO b", "todo")).toEqual([
      { text: "a ", match: false },
      { text: "TODO", match: true },
      { text: " b", match: false }
    ]);
  });

  it("matches case-insensitively but preserves the snippet's original casing", () => {
    expect(splitSnippet("Hello", "hello")).toEqual([
      { text: "Hello", match: true }
    ]);
  });

  it("highlights every occurrence", () => {
    expect(splitSnippet("ab ab", "ab")).toEqual([
      { text: "ab", match: true },
      { text: " ", match: false },
      { text: "ab", match: true }
    ]);
  });

  it("handles a match that spans the whole snippet", () => {
    expect(splitSnippet("xx", "xx")).toEqual([{ text: "xx", match: true }]);
  });
});
