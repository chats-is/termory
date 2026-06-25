import React from "react";
import { describe, it, expect, vi } from "vitest";
import { render, waitFor } from "@testing-library/react";
import { resolveLocale, LOCALES, I18nProvider, useI18n } from "./index";
import { en } from "./locales/en";
import { zhHans } from "./locales/zh-Hans";
import { zhHant } from "./locales/zh-Hant";

// `index.tsx` loads the saved language via `@/config` (Tauri invoke).
// jsdom has no Tauri, so mock it; tests drive when `getConfig` resolves to
// observe the `ready` gating. Vitest hoists vi.mock above the imports; the
// factory only captures `getConfigMock` (called later, in an effect).
const getConfigMock = vi.fn();
vi.mock("@/config", () => ({
  getConfig: (key: string) => getConfigMock(key),
  setConfig: vi.fn()
}));

describe("resolveLocale", () => {
  it("maps a system locale to one of ours, defaulting to English", () => {
    expect(resolveLocale("en-US")).toBe("en");
    expect(resolveLocale("fr")).toBe("en");
    expect(resolveLocale(null)).toBe("en");
    expect(resolveLocale("zh-CN")).toBe("zh-Hans");
    expect(resolveLocale("zh")).toBe("zh-Hans");
    expect(resolveLocale("zh-Hans")).toBe("zh-Hans");
    // Traditional variants
    expect(resolveLocale("zh-TW")).toBe("zh-Hant");
    expect(resolveLocale("zh-HK")).toBe("zh-Hant");
    expect(resolveLocale("zh-Hant")).toBe("zh-Hant");
  });
});

describe("dictionaries", () => {
  it("every locale defines exactly the English key set", () => {
    const keys = Object.keys(en).sort();
    expect(Object.keys(zhHans).sort()).toEqual(keys);
    expect(Object.keys(zhHant).sort()).toEqual(keys);
  });

  it("offers the three selectable languages", () => {
    expect(LOCALES.map((l) => l.value)).toEqual(["en", "zh-Hans", "zh-Hant"]);
  });
});

describe("I18nProvider ready gating", () => {
  function probe() {
    let ready: boolean | undefined;
    function Probe() {
      ready = useI18n().ready;
      return null;
    }
    return { Probe, get: () => ready };
  }

  it("starts not-ready, then flips ready once the saved language loads", async () => {
    let resolveConfig!: (v: unknown) => void;
    getConfigMock.mockReturnValueOnce(
      new Promise((r) => {
        resolveConfig = r;
      })
    );
    const { Probe, get } = probe();
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>
    );
    // Before the config load resolves, the tray-label push must hold off.
    expect(get()).toBe(false);
    resolveConfig("zh-Hans");
    await waitFor(() => expect(get()).toBe(true));
  });

  it("flips ready even when loading the saved language fails", async () => {
    getConfigMock.mockReturnValueOnce(Promise.reject(new Error("no config")));
    const { Probe, get } = probe();
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>
    );
    await waitFor(() => expect(get()).toBe(true));
  });
});
