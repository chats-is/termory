import type { MessageKey } from "@/i18n";
import type {
  RouterCandidate,
  RouterModel,
  RouterProtocol,
  RouterStatus,
  RouterUpstreamHealth,
  RouterUpstreamPref
} from "@/types";

/** Pure helpers for the Router page. Everything that produces UI copy takes
 * the translator as a parameter — these run outside React. */

/** Whether two binding lists hold the same bindings, whatever their
 * order. The page builds its list visible-rows-first (CLI order) and then
 * the rows Settings → Tools hides, so the same bindings can come back in
 * another order than they were saved in — an order-sensitive compare then
 * reads "unsaved changes" forever. Keyed by app: a list holds at most one
 * binding per tool. */
export function sameBindings<T extends { app: string }>(a: T[], b: T[]): boolean {
  if (a.length !== b.length) return false;
  const key = (list: T[]) =>
    JSON.stringify([...list].sort((x, y) => (x.app < y.app ? -1 : x.app > y.app ? 1 : 0)));
  return key(a) === key(b);
}

/** The model window's search: every whitespace-separated word must appear
 * in the model id or one of its source names (case-insensitive). */
export function filterRouterModels(models: RouterModel[], query: string): RouterModel[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return models;
  return models.filter((m) => {
    const hay = [m.id, ...m.sources].join("\n").toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

export type UpstreamState =
  | "disabled"
  | "unavailable"
  | "cooldown"
  | "error"
  | "ok"
  | "idle";

/** API names are product terms and stay untranslated. */
export const PROTOCOL_LABEL: Record<RouterProtocol, string> = {
  anthropic: "Anthropic Messages",
  "openai-responses": "OpenAI Responses",
  "openai-chat": "OpenAI Chat",
  gemini: "Gemini"
};

export function healthFor(
  status: RouterStatus | null | undefined,
  key: string
): RouterUpstreamHealth | undefined {
  return status?.upstreams.find((h) => h.key === key);
}

/** One state per row, in precedence order: an upstream the user turned off
 * says nothing else; one with no usable credential is unavailable whatever
 * its history; a live cooldown beats a stale error; a recorded error with
 * no cooldown left is a failure the next request will retry; a success
 * streak is healthy; never used is idle. */
export function upstreamState(
  candidate: RouterCandidate,
  health: RouterUpstreamHealth | undefined,
  now: number
): { state: UpstreamState; until?: number } {
  if (!candidate.enabled) return { state: "disabled" };
  if (!candidate.available) return { state: "unavailable" };
  if (health?.cooldownUntil != null && health.cooldownUntil > now) {
    return { state: "cooldown", until: health.cooldownUntil };
  }
  if (health?.lastError) return { state: "error" };
  if (health && health.requests > 0) return { state: "ok" };
  return { state: "idle" };
}

/** "45s" / "4m 10s" — never negative. */
export function formatCountdown(untilMillis: number, now: number): string {
  const secs = Math.max(0, Math.ceil((untilMillis - now) / 1000));
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s === 0 ? `${m}m` : `${m}m ${s}s`;
}

/** The preference list the backend expects, derived from the candidates in
 * the order the page shows them (the backend already applied the saved
 * order and appended unknowns). */
export function prefsFromCandidates(candidates: RouterCandidate[]): RouterUpstreamPref[] {
  return candidates.map((c) => ({ key: c.key, enabled: c.enabled }));
}

export function toggleCandidate(
  candidates: RouterCandidate[],
  key: string,
  enabled: boolean
): RouterCandidate[] {
  return candidates.map((c) => (c.key === key ? { ...c, enabled } : c));
}

/** Whether a move in `direction` stays inside the row's tool group — the
 * list is grouped by tool (backend order), so a move across groups would
 * just snap back on the next read. */
export function canMoveCandidate(
  candidates: RouterCandidate[],
  key: string,
  direction: "up" | "down"
): boolean {
  const index = candidates.findIndex((c) => c.key === key);
  if (index < 0) return false;
  const target = direction === "up" ? index - 1 : index + 1;
  if (target < 0 || target >= candidates.length) return false;
  return candidates[target].app === candidates[index].app;
}

/** Swap with the neighbour inside the same tool group; a no-op otherwise. */
export function moveCandidate(
  candidates: RouterCandidate[],
  key: string,
  direction: "up" | "down"
): RouterCandidate[] {
  if (!canMoveCandidate(candidates, key, direction)) return candidates;
  const index = candidates.findIndex((c) => c.key === key);
  const target = direction === "up" ? index - 1 : index + 1;
  const next = candidates.slice();
  [next[index], next[target]] = [next[target], next[index]];
  return next;
}

/** A port typed into the field: an integer in 1–65535, else null. */
/** The listen-address choice a stored host maps to: loopback only, every
 * interface (the LAN), or one specific interface IP. */
export type RouterHostMode = "local" | "lan" | "interface";

export function hostMode(host: string): RouterHostMode {
  const h = host.trim();
  if (h === "" || h === "127.0.0.1" || h === "::1") return "local";
  if (h === "0.0.0.0" || h === "::") return "lan";
  return "interface";
}

export function parsePort(raw: string): number | null {
  if (!/^\d{1,5}$/.test(raw.trim())) return null;
  const n = Number(raw.trim());
  return n >= 1 && n <= 65535 ? n : null;
}

/** Canonical form for comparing stored lists: object keys sorted, and
 * "" / null / undefined values dropped (the store strips empty leaves on
 * write, and the backend's key order is not the frontend's). */
function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const key of Object.keys(value as Record<string, unknown>).sort()) {
      const v = (value as Record<string, unknown>)[key];
      if (v === "" || v === null || v === undefined) continue;
      out[key] = canonical(v);
    }
    return out;
  }
  return value;
}

/** Whether two gateway lists hold the same data — a re-read that changed
 * nothing must not be written back (a write rebuilds the tray and
 * invalidates balances). */
export function sameStoredList(a: unknown, b: unknown): boolean {
  return JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));
}

/** Backend `reasonCode` → message key. An unknown or null code falls back to
 * the backend's own free-form `reason`. */
export const REASON_CODE_LABEL: Record<string, MessageKey> = {
  not_logged_in: "router.reason.notLoggedIn",
  needs_relogin: "router.reason.needsRelogin",
  no_payload: "router.reason.noPayload",
  no_base_url: "router.reason.noBaseUrl",
  no_api_key: "router.reason.noApiKey",
  no_detected_api: "router.reason.noDetectedApi",
  token_expired: "router.reason.tokenExpired",
  tool_disabled: "router.reason.toolDisabled"
};

/** The unavailable-reason text for a candidate: translated when the backend
 * sent a known code, else its raw `reason` (or null). */
export function candidateReason(
  candidate: Pick<RouterCandidate, "reason" | "reasonCode">,
  t: (key: MessageKey) => string
): string | null {
  const code = candidate.reasonCode;
  const key =
    code && Object.prototype.hasOwnProperty.call(REASON_CODE_LABEL, code)
      ? REASON_CODE_LABEL[code]
      : undefined;
  if (key) return t(key);
  return candidate.reason ?? null;
}
