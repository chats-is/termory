import type { AppSession, Project } from "@/types";
import { recordRel } from "./session-utils";

/** Stable key for a project (source + cwd) — the join/tombstone key. */
export function projectKey(p: { source: string; project: string }): string {
  return `${p.source}\n${p.project}`;
}

/**
 * The CLI storage folder a record sits in (its path with the within-project
 * `rel` stripped): `…/projects/-slug/x.jsonl` → `…/projects/-slug`. A project's
 * sessions AND its auto-memory share this folder, so it identifies every record
 * the CLI stores for that project — used by delete/migrate-project to act on
 * all of them, not just the sessions.
 */
export function projectDirOf(path: string): string {
  const rel = recordRel(path);
  return path.length > rel.length ? path.slice(0, path.length - rel.length - 1) : path;
}

/** Predicate: a record stored under `folder` (its project's CLI storage dir). */
export function recordUnderFolder(
  folder: string
): (s: { path: string }) => boolean {
  return (s) => s.path === folder || s.path.startsWith(folder + "/");
}

/**
 * New path for a migrated record: swap the old slug-dir prefix for the new one
 * (e.g. `…/projects/-old/x.jsonl` → `…/projects/-new/x.jsonl`). Records whose
 * path isn't under `oldDir` are returned unchanged.
 */
export function remappedPath(
  path: string,
  oldDir: string,
  newDir: string
): string {
  return path.startsWith(oldDir) ? newDir + path.slice(oldDir.length) : path;
}

/**
 * Reconcile a fresh scan against tombstoned (locally deleted/moved) keys: drop
 * tombstones the scan confirms are gone, and hide any still present (a stale,
 * in-flight scan that predates the local change — without this it would re-add
 * the row and clobber the optimistic update). Mutates `tombstones`. Generic
 * over the key so it serves both records (by path) and projects (by
 * source+cwd).
 */
export function reconcileTombstones<T>(
  incoming: T[],
  tombstones: Set<string>,
  key: (item: T) => string
): T[] {
  if (tombstones.size === 0) return incoming;
  const present = new Set(incoming.map(key));
  for (const k of [...tombstones]) if (!present.has(k)) tombstones.delete(k);
  return tombstones.size === 0
    ? incoming
    : incoming.filter((item) => !tombstones.has(key(item)));
}

/** Add a project to a list if not already present (by source+cwd). */
export function withProject(projects: Project[], project: Project): Project[] {
  const k = projectKey(project);
  return projects.some((p) => projectKey(p) === k)
    ? projects
    : [...projects, project];
}
