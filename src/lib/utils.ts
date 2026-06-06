import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * Spread onto text inputs to turn OFF browser / IME autofill and
 * auto-conversion: autocomplete (autofill suggestions), autocorrect,
 * autocapitalize, and spellcheck. Applied at each call site because the
 * stock shadcn `ui/input.tsx` stays unmodified (LOCKED rule). Spread it
 * FIRST so any explicit per-input prop (e.g. `type`) still wins.
 */
export const INPUT_NO_AUTO = {
  autoComplete: "off",
  autoCorrect: "off",
  autoCapitalize: "off",
  spellCheck: false
} as const;
