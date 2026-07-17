import { describe, expect, it } from "vitest";
import { markTermInTree } from "./highlight-term";

type Node = {
  type: string;
  value?: string;
  tagName?: string;
  properties?: Record<string, unknown>;
  children?: Node[];
};

const text = (value: string): Node => ({ type: "text", value });
const el = (tagName: string, ...children: Node[]): Node => ({
  type: "element",
  tagName,
  properties: {},
  children
});
/** Generated marks are BARE — styling is applied by the SearchMark
 * component MessageBody maps mark elements onto. */
const markEl = (...children: Node[]): Node => el("mark", ...children);
const root = (...children: Node[]): Node => ({ type: "root", children });

describe("markTermInTree", () => {
  it("wraps a single occurrence in a <mark> element", () => {
    const tree = root(el("p", text("say hello world")));
    markTermInTree(tree, "hello");
    const p = tree.children![0];
    expect(p.children).toEqual([
      text("say "),
      markEl(text("hello")),
      text(" world")
    ]);
  });

  it("matches case-insensitively but keeps the original casing", () => {
    const tree = root(el("p", text("Hello HELLO")));
    markTermInTree(tree, "hello");
    expect(p0(tree).children).toEqual([
      markEl(text("Hello")),
      text(" "),
      markEl(text("HELLO"))
    ]);
  });

  it("wraps every occurrence within one text node", () => {
    const tree = root(el("p", text("ab ab ab")));
    markTermInTree(tree, "ab");
    expect(p0(tree).children!.filter((c) => c.tagName === "mark")).toHaveLength(3);
  });

  it("recurses into nested elements (code spans, bold, fences)", () => {
    const tree = root(el("p", el("strong", text("Bash")), el("code", text("bash -c"))));
    markTermInTree(tree, "bash");
    const [strong, code] = p0(tree).children!;
    expect(strong.children).toEqual([markEl(text("Bash"))]);
    expect(code.children).toEqual([markEl(text("bash")), text(" -c")]);
  });

  it("leaves non-matching text nodes untouched", () => {
    const tree = root(el("p", text("nothing here")));
    markTermInTree(tree, "zzz");
    expect(p0(tree).children).toEqual([text("nothing here")]);
  });
});

function p0(tree: Node): Node {
  return tree.children![0];
}
