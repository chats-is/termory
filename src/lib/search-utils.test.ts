import { describe, expect, it } from "vitest";
import { splitSnippet } from "./search-utils";

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
