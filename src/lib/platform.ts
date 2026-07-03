import type { MessageKey } from "@/i18n";

// Platform detection for UI wording. Matches both real browser UAs
// ("Macintosh" / "Windows NT") and jsdom's process.platform-based UA
// ("darwin" / "win32"), so component tests see the host platform.
const UA = typeof navigator !== "undefined" ? navigator.userAgent : "";
export const IS_MAC = /Mac|darwin/i.test(UA);
export const IS_WINDOWS = /Windows|win32/i.test(UA);

/** The "reveal this file in the file manager" label for this OS —
 * Finder / File Explorer / generic file manager. Shared by the
 * right-click menu AND the detail-header button: both call the same
 * revealItemInDir(), so they carry the same wording. */
export function revealLabelKey(): MessageKey {
  return IS_MAC
    ? "menu.revealInFinder"
    : IS_WINDOWS
      ? "menu.revealInExplorer"
      : "menu.revealInFiles";
}
