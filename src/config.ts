// Local store split across three files under ~/.termory/:
//   * config.json    — UI prefs (default_pane, recent_searches, …).
//   * providers.json — Provider library (contains API keys, chmod 0600).
//   * favorites.json — Saved message snapshots (chmod 0600).
//
// Frontend callers use the unchanged getConfig/setConfig interface;
// this module routes `providers` / `favorites` keys to their own
// files and every other key to config.json. Splitting keeps
// per-feature data (and any embedded secrets/PII) out of files that
// users might back up or share.

import { invoke } from "@tauri-apps/api/core";

type ConfigObject = Record<string, unknown>;

// Log only in dev. In production these warnings ship to the renderer's
// devtools console where nobody opens them anyway, so they're either
// noise or — for the API-key edge cases — leaked detail. Vite inlines
// `import.meta.env.DEV` at build time, so this collapses to a no-op
// in release bundles.
const warn = (...args: unknown[]) => {
  if (import.meta.env.DEV) {
    // eslint-disable-next-line no-console
    console.warn(...args);
  }
};

const PROVIDERS_KEY = "providers";
const GATEWAYS_KEY = "gateways";
const FAVORITES_KEY = "favorites";

let configPromise: Promise<ConfigObject> | null = null;
let providersPromise: Promise<unknown[]> | null = null;
let gatewaysPromise: Promise<unknown[]> | null = null;
let favoritesPromise: Promise<unknown[]> | null = null;

function loadConfig(): Promise<ConfigObject> {
  if (!configPromise) {
    configPromise = invoke<unknown>("read_app_config")
      .then((value) => {
        if (value && typeof value === "object" && !Array.isArray(value)) {
          return value as ConfigObject;
        }
        return {};
      })
      .catch((err) => {
        warn("config: read_app_config failed", err);
        return {} as ConfigObject;
      });
  }
  return configPromise;
}

function loadProviders(): Promise<unknown[]> {
  if (!providersPromise) {
    providersPromise = invoke<unknown>("read_app_providers")
      .then((value) => (Array.isArray(value) ? value : []))
      .catch((err) => {
        warn("config: read_app_providers failed", err);
        return [];
      });
  }
  return providersPromise;
}

async function flushConfig(next: ConfigObject): Promise<void> {
  configPromise = Promise.resolve(next);
  try {
    await invoke("write_app_config", { value: next });
  } catch (err) {
    warn("config: write_app_config failed", err);
  }
}

/// Strip "", null, undefined leaf values from a Provider record so
/// providers.json only persists the fields the user actually filled.
/// Non-string falsy values (0, false) are kept — they only matter if a
/// future Provider field uses them.
function stripEmpty(item: unknown): unknown {
  if (!item || typeof item !== "object" || Array.isArray(item)) return item;
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(item as Record<string, unknown>)) {
    if (value === "" || value === null || value === undefined) continue;
    out[key] = value;
  }
  return out;
}

async function flushProviders(next: unknown[]): Promise<void> {
  const cleaned = next.map(stripEmpty);
  providersPromise = Promise.resolve(cleaned);
  try {
    await invoke("write_app_providers", { value: cleaned });
  } catch (err) {
    warn("config: write_app_providers failed", err);
  }
}

function loadGateways(): Promise<unknown[]> {
  if (!gatewaysPromise) {
    gatewaysPromise = invoke<unknown>("read_app_gateways")
      .then((value) => (Array.isArray(value) ? value : []))
      .catch((err) => {
        warn("config: read_app_gateways failed", err);
        return [];
      });
  }
  return gatewaysPromise;
}

async function flushGateways(next: unknown[]): Promise<void> {
  const cleaned = next.map(stripEmpty);
  gatewaysPromise = Promise.resolve(cleaned);
  try {
    await invoke("write_app_gateways", { value: cleaned });
  } catch (err) {
    warn("config: write_app_gateways failed", err);
  }
}

function loadFavorites(): Promise<unknown[]> {
  if (!favoritesPromise) {
    favoritesPromise = invoke<unknown>("read_app_favorites")
      .then((value) => (Array.isArray(value) ? value : []))
      .catch((err) => {
        warn("config: read_app_favorites failed", err);
        return [];
      });
  }
  return favoritesPromise;
}

async function flushFavorites(next: unknown[]): Promise<void> {
  favoritesPromise = Promise.resolve(next);
  try {
    await invoke("write_app_favorites", { value: next });
  } catch (err) {
    warn("config: write_app_favorites failed", err);
  }
}

export async function getConfig<T>(key: string): Promise<T | null> {
  if (key === PROVIDERS_KEY) {
    const arr = await loadProviders();
    return arr as unknown as T;
  }
  if (key === GATEWAYS_KEY) {
    const arr = await loadGateways();
    return arr as unknown as T;
  }
  if (key === FAVORITES_KEY) {
    const arr = await loadFavorites();
    return arr as unknown as T;
  }
  const config = await loadConfig();
  const raw = config[key];
  if (raw === undefined || raw === null) return null;
  return raw as T;
}

export async function setConfig<T>(key: string, value: T): Promise<void> {
  if (key === PROVIDERS_KEY) {
    if (!Array.isArray(value)) {
      warn("config: providers value must be an array, ignoring set");
      return;
    }
    await flushProviders(value as unknown[]);
    return;
  }
  if (key === GATEWAYS_KEY) {
    if (!Array.isArray(value)) {
      warn("config: gateways value must be an array, ignoring set");
      return;
    }
    await flushGateways(value as unknown[]);
    return;
  }
  if (key === FAVORITES_KEY) {
    if (!Array.isArray(value)) {
      warn("config: favorites value must be an array, ignoring set");
      return;
    }
    await flushFavorites(value as unknown[]);
    return;
  }
  const current = await loadConfig();
  const next: ConfigObject = { ...current, [key]: value as unknown };
  await flushConfig(next);
}
