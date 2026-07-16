import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { InstallGuide } from "./InstallGuide";
import { IS_MAC } from "@/lib/platform";

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

describe("InstallGuide", () => {
  it("shows the method tabs for a multi-method app", () => {
    render(<InstallGuide app="claude" rechecking={false} onRecheck={() => {}} />);
    expect(screen.getByRole("button", { name: "npm" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "curl" })).toBeTruthy();
  });

  it("shows the brew tab on macOS only", () => {
    // IS_MAC reflects the HOST platform (jsdom UA), so this runs both ways:
    // brew visible on the mac dev machine, hidden on ubuntu CI.
    render(<InstallGuide app="codex" rechecking={false} onRecheck={() => {}} />);
    const brew = screen.queryByRole("button", { name: "brew" });
    if (IS_MAC) {
      expect(brew).toBeTruthy();
    } else {
      expect(brew).toBeNull();
    }
    // The canonical tab order keeps npm first either way.
    expect(screen.getByRole("button", { name: "npm" })).toBeTruthy();
  });

  it("hides the method tabs when there is only one install method", () => {
    render(
      <InstallGuide app="claude-desktop" rechecking={false} onRecheck={() => {}} />
    );
    // No tab button — the single method's command still renders.
    expect(screen.queryByRole("button", { name: "Download" })).toBeNull();
    expect(screen.getByText("https://claude.ai/download")).toBeTruthy();
  });
});
