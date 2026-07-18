import { describe, expect, it, beforeEach, afterEach } from "vitest";
import type { AppSession } from "../types";
import {
  basename,
  isMemoryItem,
  isSessionItem,
  isSkillItem,
  memoryToolsOf,
  projectDisplayName,
  readRouteFromHash,
  recordRel,
  relUnderRoot,
  resumeCommandFor,
  roleClass,
  sessionKey,
  sourceDisplayName,
  typeLabelOf
} from "./session-utils";

// Compact factory — only the few fields the helpers actually inspect.
function mkSession(partial: Partial<AppSession>): AppSession {
  return {
    id: "x",
    source: "Codex",
    title: "t",
    project: "",
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

describe("sessionKey", () => {
  it("joins source/path/id with colons", () => {
    expect(sessionKey({ source: "Claude", path: "/a/b", id: "uuid" })).toBe(
      "Claude:/a/b:uuid"
    );
  });
});

describe("sourceDisplayName", () => {
  it("expands Claude → Claude Code", () => {
    expect(sourceDisplayName("Claude")).toBe("Claude Code");
  });

  it("expands Grok → Grok Build (mirror of CLI_APP_LABEL)", () => {
    expect(sourceDisplayName("Grok")).toBe("Grok Build");
  });
  it("passes other sources through verbatim", () => {
    // Gemini explicitly stays "Gemini" — we dropped "Gemini CLI".
    expect(sourceDisplayName("Gemini")).toBe("Gemini");
    expect(sourceDisplayName("Codex")).toBe("Codex");
    expect(sourceDisplayName("OpenCode")).toBe("OpenCode");
  });
});

describe("isMemoryItem / isSkillItem / isSessionItem", () => {
  it("recognizes memory by source", () => {
    const m = mkSession({ source: "Memory" });
    expect(isMemoryItem(m)).toBe(true);
    expect(isSkillItem(m)).toBe(false);
    expect(isSessionItem(m)).toBe(false);
  });
  it("recognizes skill by source", () => {
    const s = mkSession({ source: "Skill" });
    expect(isSkillItem(s)).toBe(true);
    expect(isMemoryItem(s)).toBe(false);
    expect(isSessionItem(s)).toBe(false);
  });
  it("treats anything else as session", () => {
    for (const src of ["Codex", "Claude", "Gemini", "OpenCode"]) {
      expect(isSessionItem(mkSession({ source: src }))).toBe(true);
    }
  });
});

describe("typeLabelOf", () => {
  it("returns Memory / Skill / Session", () => {
    expect(typeLabelOf(mkSession({ source: "Memory" }))).toBe("Memory");
    expect(typeLabelOf(mkSession({ source: "Skill" }))).toBe("Skill");
    expect(typeLabelOf(mkSession({ source: "Codex" }))).toBe("Session");
  });
});

describe("memoryToolsOf", () => {
  it("parses comma-separated preview into ordered MemoryTool list", () => {
    expect(memoryToolsOf(mkSession({ preview: "codex,opencode" }))).toEqual([
      "Codex",
      "OpenCode"
    ]);
  });
  it("preserves canonical order regardless of input order", () => {
    // MEMORY_TOOL_ORDER is ["Claude", "Codex", "Gemini", "OpenCode", "Other"].
    expect(
      memoryToolsOf(mkSession({ preview: "opencode,codex,claude" }))
    ).toEqual(["Claude", "Codex", "OpenCode"]);
  });
  it("dedupes repeats", () => {
    expect(
      memoryToolsOf(mkSession({ preview: "claude,claude,codex" }))
    ).toEqual(["Claude", "Codex"]);
  });
  it("falls back to [Other] when no recognized tags", () => {
    expect(memoryToolsOf(mkSession({ preview: "" }))).toEqual(["Other"]);
    expect(memoryToolsOf(mkSession({ preview: "random" }))).toEqual(["Other"]);
  });
  it("is case-insensitive", () => {
    expect(memoryToolsOf(mkSession({ preview: "CODEX,Claude" }))).toEqual([
      "Claude",
      "Codex"
    ]);
  });
});

describe("roleClass", () => {
  it("buckets by substring, case-insensitive", () => {
    expect(roleClass("user")).toBe("user");
    expect(roleClass("USER")).toBe("user");
    expect(roleClass("assistant")).toBe("assistant");
    expect(roleClass("AI assistant")).toBe("assistant");
    expect(roleClass("tool_use")).toBe("tool");
    expect(roleClass("function")).toBe("event");
    expect(roleClass("")).toBe("event");
  });
});

describe("projectDisplayName", () => {
  it("keeps `~/…` paths verbatim", () => {
    expect(projectDisplayName("~/.codex")).toBe("~/.codex");
    expect(projectDisplayName("~\\.codex")).toBe("~\\.codex");
  });
  it("returns the last path segment for absolute paths", () => {
    expect(projectDisplayName("/Users/john/Documents/termory")).toBe("termory");
    expect(projectDisplayName("C:\\Users\\john\\foo")).toBe("foo");
  });
  it("returns input unchanged when no separator", () => {
    expect(projectDisplayName("standalone")).toBe("standalone");
  });
  it("ignores trailing slashes", () => {
    expect(projectDisplayName("/Users/john/termory/")).toBe("termory");
  });
});

describe("recordRel", () => {
  it("returns a Claude session path relative to its slug dir", () => {
    expect(recordRel("/Users/me/.claude/projects/-Users-me-app/abc.jsonl")).toBe(
      "abc.jsonl"
    );
  });
  it("returns a Claude memory path under memory/", () => {
    expect(
      recordRel("/Users/me/.claude/projects/-Users-me-app/memory/sub/N.md")
    ).toBe("memory/sub/N.md");
  });
  it("returns a Gemini session path under chats/", () => {
    expect(
      recordRel("/Users/me/.gemini/tmp/abc123/chats/session-2026-01.json")
    ).toBe("chats/session-2026-01.json");
  });
  it("returns a Gemini memory path under memory/", () => {
    expect(recordRel("/Users/me/.gemini/tmp/abc123/memory/N.md")).toBe(
      "memory/N.md"
    );
  });
  it("falls back to the basename when no data-root marker is present", () => {
    expect(recordRel("/somewhere/else/file.md")).toBe("file.md");
  });
});

describe("relUnderRoot", () => {
  it("strips the root dir and separator", () => {
    expect(relUnderRoot("/u/.grok/memory", "/u/.grok/memory/sub/N.md")).toBe(
      "sub/N.md"
    );
  });
  it("works with a custom root location ($GROK_HOME)", () => {
    expect(relUnderRoot("/custom/home/memory", "/custom/home/memory/N.md")).toBe(
      "N.md"
    );
  });
  it("tolerates a trailing separator on the root", () => {
    expect(relUnderRoot("/u/mem/", "/u/mem/a/b.md")).toBe("a/b.md");
  });
  it("handles windows separators", () => {
    expect(relUnderRoot("C:\\g\\memory", "C:\\g\\memory\\p\\x.md")).toBe("p\\x.md");
  });
  it("falls back to the basename when full is not under root", () => {
    expect(relUnderRoot("/u/mem", "/other/place/x.md")).toBe("x.md");
  });
  it("falls back to the basename when full equals root", () => {
    expect(relUnderRoot("/u/mem", "/u/mem")).toBe("mem");
  });
});

describe("basename", () => {
  it("handles unix paths", () => {
    expect(basename("/a/b/c.txt")).toBe("c.txt");
  });
  it("handles windows paths", () => {
    expect(basename("C:\\Users\\john\\file.md")).toBe("file.md");
  });
  it("handles mixed separators", () => {
    expect(basename("/a/b\\c/d.json")).toBe("d.json");
  });
  it("ignores trailing separators", () => {
    expect(basename("/a/b/c/")).toBe("c");
  });
  it("returns input when no separator", () => {
    expect(basename("naked")).toBe("naked");
  });
});

describe("resumeCommandFor", () => {
  it("returns the right CLI invocation for each session source", () => {
    expect(resumeCommandFor("Claude", "uuid-1")).toBe("claude --resume uuid-1");
    expect(resumeCommandFor("Codex", "thread-2")).toBe("codex resume thread-2");
    expect(resumeCommandFor("OpenCode", "ses-3")).toBe(
      "opencode --session ses-3"
    );
    expect(resumeCommandFor("Gemini", "g-4")).toBe("gemini --resume g-4");
  });
  it("returns null for non-session sources", () => {
    expect(resumeCommandFor("Memory", "x")).toBeNull();
    expect(resumeCommandFor("Skill", "x")).toBeNull();
  });
});

describe("readRouteFromHash", () => {
  const originalHash = window.location.hash;
  beforeEach(() => {
    window.location.hash = "";
  });
  afterEach(() => {
    window.location.hash = originalHash;
  });

  it("returns the route when the hash matches", () => {
    window.location.hash = "#records";
    expect(readRouteFromHash()).toBe("records");
    window.location.hash = "#stats";
    expect(readRouteFromHash()).toBe("stats");
  });
  it("falls back to 'providers' for unknown hashes", () => {
    window.location.hash = "#bogus";
    expect(readRouteFromHash()).toBe("providers");
  });
  it("falls back to 'providers' for empty hash", () => {
    window.location.hash = "";
    expect(readRouteFromHash()).toBe("providers");
  });
});

describe("recordRel (windows paths)", () => {
  it("extracts the within-project rel from backslash paths", () => {
    expect(
      recordRel("C:\\Users\\x\\.claude\\projects\\C--proj\\memory\\sub\\NOTE.md")
    ).toBe("memory\\sub\\NOTE.md");
    expect(
      recordRel("C:\\Users\\x\\.gemini\\tmp\\hash1\\chats\\session-a.json")
    ).toBe("chats\\session-a.json");
    // Flat session file directly in the slug dir.
    expect(
      recordRel("C:\\Users\\x\\.claude\\projects\\C--proj\\abc.jsonl")
    ).toBe("abc.jsonl");
  });
});
