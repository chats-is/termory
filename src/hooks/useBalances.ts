import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { BALANCE_CHANGED_EVENT } from "@/constants";
import { balanceCredsKey, mergeBalanceResult } from "@/lib/balance-utils";
import type { BalanceSubject, ProviderBalance } from "@/types";

/** Manual-refresh failure toast, split over two lines: the part before
 * the first ": " (e.g. "HTTP 429 Too Many Requests") as the title, the
 * rest (the vendor's own message) as the description.
 *
 * A local copy of the shape `useQuotas` uses rather than a shared
 * helper: balance and quota are separate features that happen to present
 * an error the same way today, and hoisting this would couple them so a
 * change wanted by one silently moves the other. */
function balanceErrorToast(error: string) {
  const idx = error.indexOf(": ");
  if (idx > 0) {
    toast.error(error.slice(0, idx), { description: error.slice(idx + 2) });
  } else {
    toast.error(error);
  }
}

/** A successful balance holds this long before the AUTOMATIC pass
 * re-queries. **Deliberately the same window as the quota's
 * `QUOTA_STALE_MS`** (user decision 2026-08-10) — the two features
 * refresh on one schedule, so "when does this number update" has a
 * single answer wherever it is shown. It was 5 min at first, reasoning
 * that a wallet moves only when the user spends; the shorter window is
 * still bounded by the same manual floor below and by the states that
 * never re-query at all. Keep the two constants in step. */
const BALANCE_STALE_MS = 2 * 60_000;
/** A FAILED query retries much sooner, so a transient blip doesn't mute
 * the amount for the full stale window. Applies to both the automatic
 * pass and the manual cooldown. */
const BALANCE_ERROR_RETRY_MS = 60_000;
/** Manual-refresh floor: the button shows disabled this long after a
 * success so a click cannot hammer the vendor's billing endpoint. Same
 * value as the quota's `QUOTA_MIN_INTERVAL_MS`, and now the same as the
 * automatic window too — still a SEPARATE constant, because the two
 * answer different questions (what a background pass may do vs what a
 * click may do) and one moving is not a reason for the other to. */
const BALANCE_MIN_INTERVAL_MS = 120_000;

/** Results survive route remounts. Keyed by provider id; each entry
 * remembers the `{baseUrl, apiKey}` it came from so an edit invalidates
 * it. */
type CacheEntry = { result: ProviderBalance; credsKey: string };
let cachedBalances: Record<string, CacheEntry> = {};

/** Test seam only — nothing in the app calls this. A real remount is
 * SUPPOSED to reuse the cache; that is why it lives outside the hook. */
export function __resetBalanceCacheForTests() {
  cachedBalances = {};
}

/**
 * How long a result stays fresh for the AUTOMATIC pass.
 *
 * `unsupported` and `no_key` never expire: both are decided WITHOUT a
 * network request (the backend inspects the base URL and the key before
 * building one), so the answer can only change when those inputs do —
 * and `credsKey` already catches that. On the typical page most cards
 * are exactly this case, so the whole page costs one round of local
 * calls, once.
 */
function freshnessMs(result: ProviderBalance): number {
  if (result.status === "unsupported" || result.status === "no_key") {
    return Number.POSITIVE_INFINITY;
  }
  return result.status === "ok" ? BALANCE_STALE_MS : BALANCE_ERROR_RETRY_MS;
}

/** Per-result MANUAL floor: the full cooldown after a success, the short
 * retry window after a failure. Module-private — nothing outside reads it. */
function balanceMinIntervalMs(result?: ProviderBalance): number {
  return result?.status === "ok"
    ? BALANCE_MIN_INTERVAL_MS
    : BALANCE_ERROR_RETRY_MS;
}

function isFresh(
  entry: CacheEntry | undefined,
  credsKey: string,
  manual: boolean
): boolean {
  if (!entry || entry.credsKey !== credsKey) return false;
  const floor = manual
    ? balanceMinIntervalMs(entry.result)
    : freshnessMs(entry.result);
  return Date.now() - entry.result.queriedAt < floor;
}

/**
 * Balance state for the wallets on screen: the per-subject fetch with
 * its two floors, the module cache that survives route remounts, the
 * automatic pass, the manual-refresh cooldown clock, and the backend's
 * own results arriving as events.
 *
 * A SUBJECT is anything carrying `{id, baseUrl, apiKey}` — a custom
 * provider on the Providers tab, or a gateway on the Gateways tab. The
 * hook never reads anything else, so a gateway needs no stand-in
 * provider object (and no invented `app`) to be queried.
 *
 * One IPC per subject, cheap by construction: an unrecognised base URL
 * returns `unsupported` having made no request at all, and that verdict
 * is cached indefinitely.
 */
export function useBalances(subjects: BalanceSubject[]) {
  const [balances, setBalances] = React.useState<
    Record<string, ProviderBalance>
  >(() =>
    Object.fromEntries(
      Object.entries(cachedBalances).map(([id, e]) => [id, e.result])
    )
  );
  const [loading, setLoading] = React.useState<ReadonlySet<string>>(new Set());
  // In-flight ids. A ref, not the state above: the guard has to be exact
  // at call time, and two providers can start in the same tick.
  const inFlight = React.useRef(new Set<string>());

  const store = React.useCallback((id: string, credsKey: string, result: ProviderBalance) => {
    const prev =
      cachedBalances[id]?.credsKey === credsKey
        ? cachedBalances[id].result
        : undefined;
    const merged = mergeBalanceResult(prev, result);
    cachedBalances = { ...cachedBalances, [id]: { result: merged, credsKey } };
    setBalances((cur) => ({ ...cur, [id]: merged }));
  }, []);

  const refreshBalance = React.useCallback(
    async (subject: BalanceSubject, manual = false) => {
      const id = subject.id;
      const credsKey = balanceCredsKey(subject);
      if (inFlight.current.has(id)) return;
      // Manual clicks measure against the cooldown floor, the automatic
      // pass against the longer freshness window.
      if (isFresh(cachedBalances[id], credsKey, manual)) return;

      inFlight.current.add(id);
      setLoading((cur) => new Set(cur).add(id));
      try {
        // Send exactly the declared subject, not whatever object the
        // caller happened to hold: a Provider carries its `options` and a
        // base64 favicon, a Gateway its whole binding list, and none of it
        // is read by the query.
        const result = await invoke<ProviderBalance>("fetch_provider_balance", {
          subject: { id, baseUrl: subject.baseUrl, apiKey: subject.apiKey }
        });
        // Store under the id WE asked for, never `result.providerId`: a
        // backend that ever echoed a different id would otherwise write
        // one provider's wallet onto another provider's card.
        store(id, credsKey, result);
        if (manual && result.status !== "ok" && result.error) {
          balanceErrorToast(result.error);
        }
      } catch (err) {
        // The IPC never rejects on a query failure (those come back as a
        // result with a status), so reaching here means the call itself
        // could not be made — keep whatever is shown.
        if (manual) balanceErrorToast(String(err));
      } finally {
        inFlight.current.delete(id);
        setLoading((cur) => {
          if (!cur.has(id)) return cur;
          const next = new Set(cur);
          next.delete(id);
          return next;
        });
      }
    },
    [store]
  );

  // Automatic pass over whatever is on screen. Depends on the identity
  // of each provider's balance INPUTS rather than the array: the page
  // rebuilds its provider list on every render, and depending on the
  // array itself would re-run this each time.
  const credsFingerprint = subjects
    .map((p) => `${p.id} ${balanceCredsKey(p)}`)
    .join("");
  // When a provider's credentials change, note the moment. A result the
  // BACKEND pushes carries no record of which credentials produced it, so
  // this timestamp is the only way to recognise one that was already in
  // flight when the user edited the provider — see the listener below.
  const credsChangedAt = React.useRef<Record<string, number>>({});
  React.useEffect(() => {
    const onScreen = new Set(subjects.map((p) => p.id));
    for (const p of subjects) {
      const cached = cachedBalances[p.id];
      if (cached && cached.credsKey !== balanceCredsKey(p)) {
        credsChangedAt.current[p.id] = Date.now();
      }
      void refreshBalance(p);
    }
    // Drop marks for providers no longer listed — a pushed result can
    // only be accepted for one that is, so keeping them is pure growth.
    for (const id of Object.keys(credsChangedAt.current)) {
      if (!onScreen.has(id)) delete credsChangedAt.current[id];
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [credsFingerprint, refreshBalance]);

  // Backend-initiated results (the tray fetches on menu open, and after
  // a provider switch) arrive as events, so the page reflects them with
  // no request of its own. Only for a provider currently on screen —
  // the payload carries the id it was fetched for.
  const subjectsRef = React.useRef(subjects);
  subjectsRef.current = subjects;
  React.useEffect(() => {
    const unlisten = listen<ProviderBalance>(BALANCE_CHANGED_EVENT, (event) => {
      const result = event.payload;
      if (!result?.providerId) return;
      const subject = subjectsRef.current.find(
        (p) => p.id === result.providerId
      );
      // Not on screen — the page isn't asking this question.
      if (!subject) return;
      // Fetched BEFORE the user edited this provider, so it describes the
      // previous {baseUrl, apiKey}. The payload carries no record of the
      // credentials it used, and storing it would stamp it with the
      // CURRENT ones — where it then reads as fresh and suppresses the
      // re-fetch, leaving the old account's balance on the new card.
      const changedAt = credsChangedAt.current[result.providerId];
      if (changedAt && result.queriedAt < changedAt) return;
      const credsKey = balanceCredsKey(subject);
      const prev = cachedBalances[result.providerId];
      // Out-of-order guard: with the page and the tray both fetching, a
      // slower result can arrive after a newer one — never roll back.
      if (prev?.credsKey === credsKey && prev.result.queriedAt > result.queriedAt) {
        return;
      }
      store(result.providerId, credsKey, result);
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [store]);

  // Cooldown clock: re-render once the SOONEST cooldown on screen lapses
  // so its button re-enables without the user touching anything. ONE
  // timer for the whole list — a timer per provider would be N timers
  // re-armed on every render.
  const [, tick] = React.useReducer((n: number) => n + 1, 0);
  const soonestExpiry = subjects.reduce((soonest, p) => {
    const entry = balances[p.id];
    if (!entry) return soonest;
    const expiry = entry.queriedAt + balanceMinIntervalMs(entry);
    return expiry > Date.now() && expiry < soonest ? expiry : soonest;
  }, Number.POSITIVE_INFINITY);
  React.useEffect(() => {
    if (!Number.isFinite(soonestExpiry)) return;
    const id = setTimeout(tick, soonestExpiry - Date.now() + 50);
    return () => clearTimeout(id);
  }, [soonestExpiry]);

  /** Whether a manual refresh would currently be refused — drives the
   * button's disabled state, so the floor is VISIBLE instead of a click
   * silently doing nothing. */
  const balanceInCooldown = React.useCallback(
    (id: string) => {
      const entry = balances[id];
      if (!entry) return false;
      return Date.now() - entry.queriedAt < balanceMinIntervalMs(entry);
    },
    [balances]
  );

  return { balances, balanceLoading: loading, balanceInCooldown, refreshBalance };
}
