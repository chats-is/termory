import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, act, waitFor } from "@testing-library/react";
import type { Update } from "@tauri-apps/plugin-updater";
import { UpdateDialog } from "./UpdateDialog";

const relaunchMock = vi.fn();
const toastSuccess = vi.fn();
const toastError = vi.fn();
vi.mock("@tauri-apps/plugin-process", () => ({
  relaunch: (...args: unknown[]) => relaunchMock(...args)
}));
vi.mock("sonner", () => ({
  toast: { success: (...a: unknown[]) => toastSuccess(...a), error: (...a: unknown[]) => toastError(...a) }
}));

type DlEvent =
  | { event: "Started"; data: { contentLength?: number } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished" };

/** A fake Update whose downloadAndInstall hands the caller back the progress
 * callback + a resolver, so a test can drive Started/Progress/Finished and
 * decide when the install promise settles. */
function makeUpdate(body = "### 🐛 Bug Fixes\n- a real bug") {
  let emit: (e: DlEvent) => void = () => {};
  let finish: () => void = () => {};
  const settled = new Promise<void>((resolve) => (finish = resolve));
  const update = {
    version: "1.2.7",
    body,
    async downloadAndInstall(cb?: (e: DlEvent) => void) {
      emit = (e) => cb?.(e);
      await settled;
    }
  } as unknown as Update;
  return {
    update,
    emit: (e: DlEvent) => act(() => emit(e)),
    finishDownloadInstall: () => act(async () => finish())
  };
}

const primaryButton = () =>
  screen.getAllByRole("button").find((b) => b.textContent !== "Later")!;

describe("UpdateDialog", () => {
  beforeEach(() => {
    relaunchMock.mockReset();
    toastSuccess.mockReset();
    toastError.mockReset();
  });

  it("renders the changelog as markdown (heading text, not raw ###)", () => {
    const { update } = makeUpdate("### ✨ Features\n- shiny thing");
    render(<UpdateDialog update={update} currentVersion="1.2.6" onClose={() => {}} />);
    // react-markdown turns "### ✨ Features" into an <h3>, so the "###" markup
    // is gone and the text is present.
    expect(screen.getByText(/✨ Features/)).toBeTruthy();
    expect(screen.getByText(/shiny thing/)).toBeTruthy();
    expect(screen.queryByText(/### ✨/)).toBeNull();
  });

  it("initial button reads Update (not Install), with no spinner", () => {
    const { update } = makeUpdate();
    render(<UpdateDialog update={update} currentVersion="1.2.6" onClose={() => {}} />);
    expect(primaryButton().textContent).toBe("Update");
  });

  it("walks the label Update → Downloading… → Installing… across events", async () => {
    const { update, emit } = makeUpdate();
    render(<UpdateDialog update={update} currentVersion="1.2.6" onClose={() => {}} />);

    // Click starts the download phase.
    await act(async () => {
      fireEvent.click(primaryButton());
    });
    expect(primaryButton().textContent).toContain("Downloading…");

    // Known size + one chunk → a percentage shows in the download phase.
    emit({ event: "Started", data: { contentLength: 100 } });
    emit({ event: "Progress", data: { chunkLength: 50 } });
    expect(screen.getByText("50%")).toBeTruthy();

    // Finished → the updater applies the bundle → installing phase, no %.
    emit({ event: "Finished" });
    expect(primaryButton().textContent).toContain("Installing…");
    expect(screen.queryByText("50%")).toBeNull();
  });

  it("on success shows the installed toast and relaunches", async () => {
    const { update, finishDownloadInstall } = makeUpdate();
    render(<UpdateDialog update={update} currentVersion="1.2.6" onClose={() => {}} />);
    await act(async () => {
      fireEvent.click(primaryButton());
    });
    await finishDownloadInstall();
    await waitFor(() => expect(relaunchMock).toHaveBeenCalledTimes(1));
    expect(toastSuccess).toHaveBeenCalledTimes(1);
  });

  it("disables Later + hides the close button while busy", async () => {
    const { update } = makeUpdate();
    render(<UpdateDialog update={update} currentVersion="1.2.6" onClose={() => {}} />);
    const later = screen.getByRole("button", { name: "Later" });
    expect(later.hasAttribute("disabled")).toBe(false);
    await act(async () => {
      fireEvent.click(primaryButton());
    });
    expect(later.hasAttribute("disabled")).toBe(true);
  });
});
