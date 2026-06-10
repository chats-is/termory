import React from "react";
import { getConfig, setConfig } from "@/config";
import { setFormatLocale } from "@/lib/format";
import { en } from "./locales/en";
import { zhHans } from "./locales/zh-Hans";
import { zhHant } from "./locales/zh-Hant";

export type Locale = "en" | "zh-Hans" | "zh-Hant";
/** Every translatable string id — derived from the English source dict. */
export type MessageKey = keyof typeof en;

const DICTIONARIES: Record<Locale, Record<MessageKey, string>> = {
  en,
  "zh-Hans": zhHans,
  "zh-Hant": zhHant
};

/** Selectable languages, each shown in its OWN name (not translated). */
export const LOCALES: { value: Locale; label: string }[] = [
  { value: "en", label: "English" },
  { value: "zh-Hans", label: "简体中文" },
  { value: "zh-Hant", label: "繁體中文" }
];

const CONFIG_KEY = "language";

function isLocale(v: unknown): v is Locale {
  return v === "en" || v === "zh-Hans" || v === "zh-Hant";
}

/** Map a browser/system locale string (`navigator.language`) to one of ours. */
export function resolveLocale(raw: string | null | undefined): Locale {
  const s = (raw ?? "").toLowerCase();
  if (s.startsWith("zh")) {
    return /hant|tw|hk|mo/.test(s) ? "zh-Hant" : "zh-Hans";
  }
  return "en";
}

function interpolate(
  template: string,
  params?: Record<string, string | number>
): string {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (_, k: string) =>
    k in params ? String(params[k]) : `{${k}}`
  );
}

type I18nValue = {
  locale: Locale;
  setLocale: (l: Locale) => void;
  t: (key: MessageKey, params?: Record<string, string | number>) => string;
};

/** Fallback when no <I18nProvider> is mounted (e.g. unit tests rendering a
 * component in isolation): renders English; `setLocale` is a no-op. */
const DEFAULT_VALUE: I18nValue = {
  locale: "en",
  setLocale: () => {},
  t: (key, params) => interpolate(en[key] ?? key, params)
};

const I18nContext = React.createContext<I18nValue>(DEFAULT_VALUE);

export function I18nProvider({ children }: { children: React.ReactNode }) {
  // Start from the system locale; the saved preference (loaded below) wins.
  const [locale, setLocaleState] = React.useState<Locale>(() =>
    resolveLocale(typeof navigator !== "undefined" ? navigator.language : "en")
  );

  React.useEffect(() => {
    void getConfig<unknown>(CONFIG_KEY)
      .then((saved) => {
        if (isLocale(saved)) setLocaleState(saved);
      })
      .catch(() => {});
  }, []);

  // Keep the date formatters on the app's language (not the OS locale). Done in
  // the render body — not an effect — so it's set BEFORE children format dates
  // in this same pass (an effect would lag one render). `setFormatLocale`
  // early-returns when unchanged, so this is cheap on every render.
  setFormatLocale(locale);

  React.useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);

  const setLocale = React.useCallback((l: Locale) => {
    setLocaleState(l);
    void setConfig(CONFIG_KEY, l).catch(() => {});
  }, []);

  const t = React.useCallback<I18nValue["t"]>(
    (key, params) => {
      const dict = DICTIONARIES[locale];
      // Fall back to English, then the raw key, so a missing translation
      // degrades gracefully instead of rendering blank.
      const template = dict[key] ?? en[key] ?? key;
      return interpolate(template, params);
    },
    [locale]
  );

  const value = React.useMemo<I18nValue>(
    () => ({ locale, setLocale, t }),
    [locale, setLocale, t]
  );
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nValue {
  return React.useContext(I18nContext);
}

/** Shorthand when only the `t` function is needed. */
export function useT() {
  return useI18n().t;
}
