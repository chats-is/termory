import { describe, it, expect } from "vitest";
import { resolveLocale, LOCALES } from "./index";
import { en } from "./locales/en";
import { zhHans } from "./locales/zh-Hans";
import { zhHant } from "./locales/zh-Hant";

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
