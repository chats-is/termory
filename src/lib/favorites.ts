// Favorites — saved message snapshots, persisted via IPC to
// `~/.termory/favorites.json`. Each star button toggles an entry by
// the (source, sessionId, messageIndex) tuple key.

import type {
  AppSession,
  Favorite,
  SessionMessage,
  SessionSource
} from "@/types";

const KNOWN_SOURCES: SessionSource[] = [
  "Claude",
  "Codex",
  "Gemini",
  "OpenCode"
];

/** Narrow `AppSession.source` (loose `string` for backend-compat) to
 * the `SessionSource` union the favorite needs. Unknown values fall
 * back to "Claude" — defensive, should never happen for sessions that
 * have a render-able message list. */
function asSessionSource(raw: string): SessionSource {
  return (KNOWN_SOURCES as string[]).includes(raw)
    ? (raw as SessionSource)
    : "Claude";
}

/** Composite key used to dedupe favorites + answer
 * "is this message currently favorited?" in O(1). */
export function favoriteKey(
  source: string,
  sessionId: string,
  index: number
): string {
  return `${source}::${sessionId}::${index}`;
}

/** Read-only Set of keys derived from a Favorite[]. */
export function favoriteKeySet(favorites: Favorite[]): Set<string> {
  const out = new Set<string>();
  for (const f of favorites) {
    out.add(favoriteKey(f.source, f.source_session_id, f.source_message_index));
  }
  return out;
}

/** Build a Favorite from the components that the click site has. The
 * source session metadata is snapshotted so the favorite survives
 * even if the session is later deleted or renamed. */
export function makeFavorite(
  session: AppSession,
  message: SessionMessage,
  index: number
): Favorite {
  return {
    id: cryptoRandomId(),
    favorited_at: new Date().toISOString(),
    message,
    source: asSessionSource(session.source),
    source_session_id: session.id,
    source_session_path: session.path,
    source_session_title: session.title ?? "",
    source_session_project: session.project ?? "",
    source_message_index: index
  };
}

/** Toggle a favorite — returns the new array. Pure (no IPC), so the
 * caller can decide when to persist. */
export function toggleFavoriteEntry(
  current: Favorite[],
  session: AppSession,
  message: SessionMessage,
  index: number
): Favorite[] {
  const key = favoriteKey(session.source, session.id, index);
  const existing = current.find(
    (f) =>
      favoriteKey(f.source, f.source_session_id, f.source_message_index) === key
  );
  if (existing) {
    return current.filter((f) => f.id !== existing.id);
  }
  return [...current, makeFavorite(session, message, index)];
}

/** Sort newest first by `favorited_at` (ISO string sort works as date
 * sort for RFC 3339 with consistent TZ). */
export function sortFavoritesDesc(favorites: Favorite[]): Favorite[] {
  return [...favorites].sort((a, b) =>
    a.favorited_at < b.favorited_at ? 1 : a.favorited_at > b.favorited_at ? -1 : 0
  );
}

/** Validator for usePersistentState — defensive against hand-edited
 * favorites.json with malformed entries. */
export function isFavoriteList(raw: unknown): raw is Favorite[] {
  if (!Array.isArray(raw)) return false;
  return raw.every(
    (x) =>
      !!x &&
      typeof x === "object" &&
      "id" in x &&
      "message" in x &&
      "source_session_id" in x
  );
}

function cryptoRandomId(): string {
  // crypto.randomUUID exists in modern browsers/Tauri; fall back to a
  // Math.random-based id if not available (tests / non-WebCrypto env).
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `fav-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
