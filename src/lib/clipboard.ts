/// Write `text` to the clipboard. REJECTS when the write fails, so a caller
/// never reports "Copied" for something the user cannot paste — every
/// caller shows the failure instead.
export async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch (err) {
    console.warn("clipboard write failed", err);
    throw err;
  }
}
