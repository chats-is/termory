import { describe, it, expect } from "vitest";
import type { AppSession, Project } from "@/types";
import {
  remappedPath,
  reconcileTombstones,
  projectKey,
  projectDirOf,
  recordUnderFolder,
  withProject
} from "./records";

function s(path: string, id = "x"): AppSession {
  return { id, source: "Claude", path } as AppSession;
}
function p(source: string, project: string): Project {
  return { source, project };
}

describe("projectKey", () => {
  it("combines source + cwd", () => {
    expect(projectKey({ source: "Claude", project: "/a" })).toBe("Claude\n/a");
    // Same cwd under different sources are distinct projects.
    expect(projectKey(p("Claude", "/a"))).not.toBe(projectKey(p("Codex", "/a")));
  });
});

describe("projectDirOf", () => {
  it("strips the within-project rel to the CLI storage folder", () => {
    expect(projectDirOf("/u/.claude/projects/-slug/abc.jsonl")).toBe(
      "/u/.claude/projects/-slug"
    );
    // A session and its auto-memory resolve to the SAME folder.
    expect(projectDirOf("/u/.claude/projects/-slug/memory/sub/N.md")).toBe(
      "/u/.claude/projects/-slug"
    );
    expect(projectDirOf("/u/.gemini/tmp/hash/chats/s.json")).toBe(
      "/u/.gemini/tmp/hash"
    );
  });
});

describe("recordUnderFolder", () => {
  it("matches the folder's records (sessions + auto-memory), nothing outside", () => {
    const under = recordUnderFolder("/u/.claude/projects/-slug");
    expect(under({ path: "/u/.claude/projects/-slug/abc.jsonl" })).toBe(true);
    expect(under({ path: "/u/.claude/projects/-slug/memory/N.md" })).toBe(true);
    // A sibling slug that shares a prefix must NOT match.
    expect(under({ path: "/u/.claude/projects/-slug-2/abc.jsonl" })).toBe(false);
    expect(under({ path: "/u/.codex/sessions/x.jsonl" })).toBe(false);
  });
});

describe("remappedPath", () => {
  it("swaps the old slug-dir prefix for the new one", () => {
    expect(
      remappedPath("/p/projects/-old/abc.jsonl", "/p/projects/-old", "/p/projects/-new")
    ).toBe("/p/projects/-new/abc.jsonl");
    expect(
      remappedPath("/p/projects/-old/memory/sub/N.md", "/p/projects/-old", "/p/projects/-new")
    ).toBe("/p/projects/-new/memory/sub/N.md");
  });
  it("leaves a path that isn't under oldDir unchanged", () => {
    expect(remappedPath("/other/x.jsonl", "/p/projects/-old", "/p/projects/-new")).toBe(
      "/other/x.jsonl"
    );
  });
});

describe("withProject", () => {
  it("adds a project when absent, dedupes by source+cwd", () => {
    const list = [p("Claude", "/a")];
    expect(withProject(list, p("Claude", "/b"))).toHaveLength(2);
    expect(withProject(list, p("Claude", "/a"))).toBe(list); // unchanged
    expect(withProject(list, p("Codex", "/a"))).toHaveLength(2); // diff source
  });
});

describe("reconcileTombstones (records by path)", () => {
  it("returns the scan unchanged when there are no tombstones", () => {
    const tomb = new Set<string>();
    const scan = [s("/a"), s("/b")];
    expect(reconcileTombstones(scan, tomb, (r) => r.path)).toEqual(scan);
  });
  it("hides a tombstoned path still present in a stale scan, keeping the tombstone", () => {
    const tomb = new Set<string>(["/a"]);
    const result = reconcileTombstones([s("/a"), s("/b")], tomb, (r) => r.path);
    expect(result.map((r) => r.path)).toEqual(["/b"]);
    expect(tomb.has("/a")).toBe(true);
  });
  it("clears a tombstone the scan confirms is gone", () => {
    const tomb = new Set<string>(["/a"]);
    const result = reconcileTombstones([s("/b")], tomb, (r) => r.path);
    expect(result.map((r) => r.path)).toEqual(["/b"]);
    expect(tomb.has("/a")).toBe(false);
  });
});

describe("reconcileTombstones (projects by source+cwd)", () => {
  it("hides a tombstoned project still present in a stale scan", () => {
    const tomb = new Set<string>([projectKey(p("Claude", "/a"))]);
    const result = reconcileTombstones(
      [p("Claude", "/a"), p("Claude", "/b")],
      tomb,
      projectKey
    );
    expect(result.map((x) => x.project)).toEqual(["/b"]);
  });
});
