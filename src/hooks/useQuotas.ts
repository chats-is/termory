import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { CLI_APPS, QUOTA_CHANGED_EVENT, QUOTA_INVALIDATED_EVENT } from "@/constants";
import { mergeQuotaResult, quotaResultIsStale } from "@/lib/quota-utils";
import type { CliApp, SubscriptionQuota } from "@/types";

/** CLIs whose Official card shows the subscription quota (5-hour /
 * weekly rate-limit windows; grok: the weekly/monthly credit window).
 * MIRROR of the backend list `quota::SUPPORTED` in src-tauri/src/quota.rs
 * (which drives the tray) — when a CLI's fetch_quota arm lands, add it
 * in BOTH places. */
export const QUOTA_SUPPORTED: ReadonlySet<CliApp> = new Set([
  "claude",
  "codex",
  "gemini",
  "grok"
]);

/** Quota results survive route remounts (like cachedVersions). An entry
 * older than QUOTA_STALE_MS is silently re-fetched on the next entry to
 * the tab. Manual Refresh bypasses the stale window but is still rate
 * limited by QUOTA_MIN_INTERVAL_MS (the button shows disabled during
 * the cooldown) so it can't hammer the official endpoint. */
let cachedQuotas: Partial<Record<CliApp, SubscriptionQuota>> = {};
const QUOTA_STALE_MS = 2 * 60_000;
const QUOTA_MIN_INTERVAL_MS = 120_000;
/** A FAILED fetch caches for much less so a transient network error
 * doesn't mute the quota display for the full stale window. Applies to
 * the auto path, the manual cooldown, and the tray (Rust mirror:
 * QUOTA_TRAY_ERROR_RETRY in tray.rs). */
const QUOTA_ERROR_RETRY_MS = 60_000;

/** Per-result refresh floor: full cooldown after a success, the short
 * error-retry window after a failure (or no result yet). */
function quotaMinIntervalMs(q?: SubscriptionQuota): number {
  return q?.success ? QUOTA_MIN_INTERVAL_MS : QUOTA_ERROR_RETRY_MS;
}

/** Test seam only: drop the module-level cache that survives remounts.
 * Nothing in the app calls this — a real remount is supposed to reuse
 * the cache, which is the whole point of it living outside the hook. */
export function __resetQuotaCacheForTests() {
  cachedQuotas = {};
}

/** Manual-refresh failure toast, split over two lines: the part before
 * the first ": " (e.g. "HTTP 429 Too Many Requests") as the title, the
 * rest (the API's message) as the description. Errors without that
 * shape show as a single line. */
function quotaErrorToast(error: string) {
  const idx = error.indexOf(": ");
  if (idx > 0) {
    toast.error(error.slice(0, idx), { description: error.slice(idx + 2) });
  } else {
    toast.error(error);
  }
}

/** Official-account quota state for every supported CLI: the fetch with
 * its rate limits, the module-level cache that survives route remounts,
 * both backend-pushed events, and the manual-refresh cooldown clock.
 *
 * Lives outside ProvidersPage so the account-switch rules below are
 * reachable from tests — the page itself takes a dozen props and drives
 * 18 IPCs, which is not a seam anything can be asserted through. */
export function useQuotas(app: CliApp) {
  const [quotas, setQuotas] = React.useState<
    Partial<Record<CliApp, SubscriptionQuota>>
  >(cachedQuotas);
  const [quotaLoading, setQuotaLoading] = React.useState<CliApp | null>(null);
  const quotaLoadingRef = React.useRef<CliApp | null>(null);
  // When a CLI's displayed quota was last invalidated because the
  // IDENTITY behind it changed (account switch). Any result whose
  // `queriedAt` predates that moment was fetched for the PREVIOUS
  // account, so it is dropped instead of being displayed / cached —
  // covers both an in-flight IPC fetch and a backend-initiated
  // QUOTA_CHANGED_EVENT that was already on its way.
  const quotaResetAtRef = React.useRef<Partial<Record<CliApp, number>>>({});

  /** Drop a CLI's quota entry. The numbers on screen belong to the
   * account that was live when they were fetched, so an account switch
   * must clear them rather than leave them under the new account's row
   * until a refresh lands. Clearing also disarms mergeQuotaResult's
   * failure fallback (no `prev` to keep), so a FAILED post-switch fetch
   * shows nothing instead of the previous account's usage. */
  const resetQuota = React.useCallback((target: CliApp) => {
    quotaResetAtRef.current = {
      ...quotaResetAtRef.current,
      [target]: Date.now()
    };
    setQuotas((cur) => {
      if (!(target in cur)) return cur;
      const next = { ...cur };
      delete next[target];
      cachedQuotas = next;
      return next;
    });
  }, []);

  /** True for a result fetched before the entry was last invalidated —
   * i.e. one describing the previous account. */
  const quotaIsStale = React.useCallback(
    (result: SubscriptionQuota, target: CliApp) =>
      quotaResultIsStale(result, quotaResetAtRef.current[target]),
    []
  );

  // `manual: true` = the user clicked Refresh — a failure surfaces as
  // a toast with the backend's raw error. Background fetches (tab
  // entry, stale re-fetch) fail silently and just keep the old data.
  // `force: true` bypasses the rate limit AND the in-flight guard —
  // only for an identity change (account switch), where the cached
  // result describes a DIFFERENT account. The IN-FLIGHT half is the
  // load-bearing one there: a fetch started for the previous account can
  // still be running, and the guard would swallow the re-fetch for the
  // new one. (The rate-limit half is belt-and-braces on that path, since
  // the caller resets first and `resetQuota` drops the cached entry the
  // floor is measured from — but it keeps `force` honest for any caller
  // that doesn't reset.)
  const refreshQuota = React.useCallback(
    async (target: CliApp, manual = false, force = false) => {
      if (!QUOTA_SUPPORTED.has(target)) return;
      if (!force && quotaLoadingRef.current === target) return; // fetch in flight
      // Rate limit: never query the official endpoint more often than
      // the per-result floor (shorter after a failure), regardless of
      // which path called us.
      const lastResult = cachedQuotas[target];
      if (
        !force &&
        lastResult?.queriedAt &&
        Date.now() - lastResult.queriedAt < quotaMinIntervalMs(lastResult)
      ) {
        return;
      }
      quotaLoadingRef.current = target;
      setQuotaLoading(target);
      try {
        const result = await invoke<SubscriptionQuota>(
          "fetch_subscription_quota",
          { app: target }
        );
        if (quotaIsStale(result, target)) return;
        setQuotas((cur) => {
          // Failures merge with the previous entry instead of wiping the
          // displayed tiers/reset times — see quota-utils mergeQuotaResult.
          const next = { ...cur, [target]: mergeQuotaResult(cur[target], result) };
          cachedQuotas = next;
          return next;
        });
        if (manual && !result.success && result.error) {
          quotaErrorToast(result.error);
        }
      } catch (err) {
        // unknown-app guard only — leave the previous result on screen
        if (manual) toast.error(String(err));
      } finally {
        if (quotaLoadingRef.current === target) quotaLoadingRef.current = null;
        setQuotaLoading((cur) => (cur === target ? null : cur));
      }
    },
    [quotaIsStale]
  );

  // The backend dropped a CLI's quota because its LOGIN changed — a
  // switch made from the menu-bar tray, which the page has no other way
  // to learn about. Mirror the drop here (the backend's own re-fetch
  // arrives as a quota-changed event), so the rings don't keep showing
  // the account that was switched away from.
  React.useEffect(() => {
    const unlisten = listen<string>(QUOTA_INVALIDATED_EVENT, (event) => {
      const cli = event.payload;
      if (cli && CLI_APPS.includes(cli as CliApp)) resetQuota(cli as CliApp);
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [resetQuota]);

  // Backend-initiated quota results (tray click, watcher
  // credential-change — e.g. the user just ran `claude login`) arrive
  // as events; store them so the page reflects a login/logout without
  // its own request. IPC-initiated fetches echo here too (same data).
  React.useEffect(() => {
    const unlisten = listen<SubscriptionQuota>(QUOTA_CHANGED_EVENT, (event) => {
      const result = event.payload;
      if (!result?.app) return;
      // Fetched for the account that was live before a switch — the
      // numbers no longer describe the current login.
      if (quotaIsStale(result, result.app)) return;
      setQuotas((cur) => {
        // Out-of-order guard: with concurrent fetches (page + tray),
        // a slower fetch's payload can arrive after a newer result —
        // never roll the entry back to an older snapshot.
        const prev = cur[result.app];
        if (
          prev?.queriedAt &&
          result.queriedAt &&
          result.queriedAt < prev.queriedAt
        ) {
          return cur;
        }
        const next = {
          ...cur,
          [result.app]: mergeQuotaResult(prev, result)
        };
        cachedQuotas = next;
        return next;
      });
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [quotaIsStale]);

  // Manual-refresh cooldown for the CURRENT tab's quota (shorter
  // window after a failed fetch). Derived from `queriedAt`; the
  // timeout re-renders once the cooldown lapses so the Refresh button
  // re-enables without user interaction.
  const quotaEntry = quotas[app];
  const quotaQueriedAt = quotaEntry?.queriedAt;
  const quotaCooldownMs = quotaMinIntervalMs(quotaEntry);
  const [, quotaTick] = React.useReducer((n: number) => n + 1, 0);
  React.useEffect(() => {
    if (!quotaQueriedAt) return;
    const remain = quotaCooldownMs - (Date.now() - quotaQueriedAt);
    if (remain <= 0) return;
    const id = setTimeout(quotaTick, remain + 50);
    return () => clearTimeout(id);
  }, [quotaQueriedAt, quotaCooldownMs]);
  const quotaInCooldown =
    !!quotaQueriedAt && Date.now() - quotaQueriedAt < quotaCooldownMs;

  /** Whether the cached entry is old enough for the tab-entry refresh to
   * bother. Successful results hold for the full stale window; failures
   * only for the short error-retry one. */
  const quotaIsFresh = React.useCallback((target: CliApp) => {
    const cached = cachedQuotas[target];
    const staleMs = cached?.success ? QUOTA_STALE_MS : QUOTA_ERROR_RETRY_MS;
    return !!cached?.queriedAt && Date.now() - cached.queriedAt < staleMs;
  }, []);

  return {
    quotas,
    quotaLoading,
    quotaInCooldown,
    refreshQuota,
    resetQuota,
    quotaIsFresh
  };
}
