import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  fireEvent,
  screen,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { openUrl } from "@tauri-apps/plugin-opener";
import { InstallGuide } from "./InstallGuide";
import { IS_MAC, IS_WINDOWS } from "@/lib/platform";

// The copy-command button uses shadcn Tooltip → needs a TooltipProvider.
function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(ui, { wrapper: TooltipProvider, ...options });
}

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

describe("InstallGuide", () => {
  it("shows the method tabs for a multi-method app", () => {
    render(<InstallGuide app="claude" rechecking={false} onRecheck={() => {}} />);
    expect(screen.getByRole("button", { name: "Native" })).toBeTruthy();
    if (IS_MAC) expect(screen.getByRole("button", { name: "Homebrew" })).toBeTruthy();
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

  it("shows Codex's official installer for the current platform", () => {
    render(<InstallGuide app="codex" rechecking={false} onRecheck={() => {}} />);

    if (IS_WINDOWS) {
      fireEvent.click(screen.getByRole("button", { name: "PowerShell" }));
      expect(
        screen.getByText(
          'powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"'
        )
      ).toBeTruthy();
      expect(
        screen.queryByText("curl -fsSL https://chatgpt.com/codex/install.sh | sh")
      ).toBeNull();
    } else {
      fireEvent.click(screen.getByRole("button", { name: "curl" }));
      expect(
        screen.getByText("curl -fsSL https://chatgpt.com/codex/install.sh | sh")
      ).toBeTruthy();
    }
  });

  it("hides the method tabs when there is only one install method", () => {
    render(
      <InstallGuide app="claude-desktop" rechecking={false} onRecheck={() => {}} />
    );
    // No tab button — the single method's URL only renders on supported OSes.
    expect(screen.queryByRole("button", { name: "download" })).toBeNull();
    if (IS_MAC || IS_WINDOWS) {
      expect(screen.getByText("https://claude.ai/download")).toBeTruthy();
    } else {
      expect(screen.queryByText("https://claude.ai/download")).toBeNull();
    }
  });

  it("opens an app download URL instead of copying it", () => {
    render(
      <InstallGuide app="claude-desktop" rechecking={false} onRecheck={() => {}} />
    );
    if (IS_MAC || IS_WINDOWS) {
      fireEvent.click(screen.getByRole("button", { name: "Open download page" }));
      expect(openUrl).toHaveBeenCalledWith("https://claude.ai/download");
    } else {
      expect(screen.queryByRole("button", { name: "Open download page" })).toBeNull();
    }
  });

  it("shows Grok's installer for the current platform", () => {
    render(<InstallGuide app="grok" rechecking={false} onRecheck={() => {}} />);

    if (IS_WINDOWS) {
      expect(screen.getByText("irm https://x.ai/cli/install.ps1 | iex")).toBeTruthy();
      expect(screen.queryByText("curl -fsSL https://x.ai/cli/install.sh | bash")).toBeNull();
    } else {
      expect(screen.getByText("curl -fsSL https://x.ai/cli/install.sh | bash")).toBeTruthy();
      expect(screen.queryByText("irm https://x.ai/cli/install.ps1 | iex")).toBeNull();
    }
  });
});
