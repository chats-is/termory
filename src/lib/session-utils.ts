import type { AppSession, MemoryTool, Route } from "../types";
import { CLI_APP_LABEL, MEMORY_SOURCE, MEMORY_TOOL_ORDER, ROUTES, SKILL_SOURCE } from "../constants";

export function sessionKey(session: { source: string; path: string; id: string }): string {
  return `${session.source}:${session.path}:${session.id}`;
}

// Pretty label for a tool/source identifier. Internal source values
// stay short ("Claude", "Gemini"); the display layer always goes
// through this helper so Records and Providers show the same official
// tool name ("Claude Code", "Gemini").
export function sourceDisplayName(source: string): string {
  // Session source names lowercase to their CliApp key (test-pinned
  // convention), so the display name single-sources from CLI_APP_LABEL —
  // Records / Search / Favorites always read the same as the Providers
  // tab ("Claude" → "Claude Code", "Grok" → "Grok Build").
  const label = (CLI_APP_LABEL as Record<string, string>)[source.toLowerCase()];
  return label ?? source;
}

export function isMemoryItem(session: AppSession): boolean {
  return session.source === MEMORY_SOURCE;
}

export function isSkillItem(session: AppSession): boolean {
  return session.source === SKILL_SOURCE;
}

export function isSessionItem(session: AppSession): boolean {
  return !isMemoryItem(session) && !isSkillItem(session);
}

export function typeLabelOf(session: AppSession): "Session" | "Memory" | "Skill" {
  if (isMemoryItem(session)) return "Memory";
  if (isSkillItem(session)) return "Skill";
  return "Session";
}

export function memoryToolsOf(session: AppSession): MemoryTool[] {
  const set = new Set<MemoryTool>();
  for (const raw of (session.preview ?? "").split(",")) {
    const tag = raw.trim().toLowerCase();
    if (tag === "claude") set.add("Claude");
    else if (tag === "codex") set.add("Codex");
    else if (tag === "gemini") set.add("Gemini");
    else if (tag === "opencode") set.add("OpenCode");
    else if (tag === "grok") set.add("Grok");
  }
  if (set.size === 0) return ["Other"];
  return MEMORY_TOOL_ORDER.filter((tool) => set.has(tool));
}

export function roleClass(role: string): "user" | "assistant" | "tool" | "event" {
  const lowered = role.toLowerCase();
  if (lowered.includes("user")) return "user";
  if (lowered.includes("assistant")) return "assistant";
  if (lowered.includes("tool")) return "tool";
  return "event";
}

export function projectDisplayName(project: string): string {
  // Tool config "projects" (`~/.codex`, `~/.claude/skills`, `~/.gemini`,
  // etc.) keep their full label — basenaming them yields useless
  // strings like `.codex` or `skills` that don't tell the user which
  // platform or scope they're looking at. Only real filesystem paths
  // (cwd / git repo roots) get shortened to their leaf folder.
  if (project.startsWith("~/") || project.startsWith("~\\")) return project;
  return project.split(/[\\/]+/).filter(Boolean).pop() ?? project;
}

export function basename(path: string): string {
  // Cross-platform basename: split on / or \, drop trailing empty
  // segments, return the last piece. Falls back to the whole path if
  // nothing splits.
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : path;
}

/**
 * A record file's path relative to its project dir — the single identifier
 * delete/migrate pass so the backend can rebuild the path under the bounded
 * project dir without ever receiving a full filesystem path. Detect the CLI's
 * data-root marker from the path, then drop it AND the project-dir segment:
 *   `…/projects/<slug>/<rel>`        → `<rel>` (e.g. `<id>.jsonl`, `memory/x.md`)
 *   `…/.gemini/tmp/<hash>/<rel>`     → `<rel>` (e.g. `chats/s.json`, `memory/x.md`)
 * Falls back to the basename if no marker is found.
 */
export function recordRel(path: string): string {
  // Windows paths arrive `\`-separated — match either separator. The
  // returned rel keeps its NATIVE separators (the backend joins it
  // under the project dir, so it must match the on-disk shape).
  const gemini = /[\\/]\.gemini[\\/]tmp[\\/]/.exec(path);
  const marker = gemini ?? /[\\/]projects[\\/]/.exec(path);
  if (!marker) return basename(path);
  const after = path.slice(marker.index + marker[0].length); // "<dir><sep><rel>"
  const sep = after.search(/[\\/]/);
  return sep < 0 ? after : after.slice(sep + 1);
}

// Per-platform "resume this session" shell command. Returns `null`
// for sources whose CLI doesn't expose a direct resume-by-id flag
// (Gemini's `/resume` is in-TUI only; OpenCode's `--session` depends
// on additional flags we don't know here).
export function resumeCommandFor(source: string, id: string): string | null {
  switch (source) {
    case "Claude":
      return `claude --resume ${id}`;
    case "Codex":
      return `codex resume ${id}`;
    case "OpenCode":
      return `opencode --session ${id}`;
    case "Gemini":
      return `gemini --resume ${id}`;
    case "Grok":
      return `grok --resume ${id}`;
    default:
      return null;
  }
}

export function isRoute(value: string): value is Route {
  return (ROUTES as string[]).includes(value);
}

export function readRouteFromHash(): Route {
  const raw = window.location.hash.replace(/^#/, "");
  return isRoute(raw) ? raw : "providers";
}
