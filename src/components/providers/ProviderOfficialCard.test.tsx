import React from "react";
import { describe, expect, it, vi } from "vitest";
import { render as rtlRender, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ProviderOfficialCard } from "./ProviderOfficialCard";

const baseProps = {
  app: "claude" as const,
  isInUse: false,
  settingDefault: false,
  versions: [{ text: "v2.0.0" }],
  onSetDefault: vi.fn()
};

function render(ui: React.ReactElement) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>);
}

describe("ProviderOfficialCard", () => {
  it("shows Official label and version", () => {
    render(<ProviderOfficialCard {...baseProps} />);
    expect(screen.getByText("Official")).toBeInTheDocument();
    expect(screen.getByText(/v2\.0\.0/)).toBeInTheDocument();
  });

  it("shows a loading pulse while version is loading", () => {
    const { container } = render(
      <ProviderOfficialCard {...baseProps} versions={[]} versionLoading />
    );
    expect(container.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("shows '—' when there are no version segments and not loading", () => {
    render(<ProviderOfficialCard {...baseProps} versions={[]} />);
    expect(screen.getByText("—")).toBeInTheDocument();
  });

  it("shows Activate button when not in use and calls onSetDefault", async () => {
    const user = userEvent.setup();
    const onSetDefault = vi.fn();
    render(<ProviderOfficialCard {...baseProps} isInUse={false} onSetDefault={onSetDefault} />);
    const btn = screen.getByRole("button", { name: "Activate" });
    await user.click(btn);
    expect(onSetDefault).toHaveBeenCalledTimes(1);
  });

  it("hides Activate button when in use", () => {
    render(<ProviderOfficialCard {...baseProps} isInUse />);
    expect(screen.queryByRole("button", { name: "Activate" })).toBeNull();
  });

  it("shows In use badge when active", () => {
    render(<ProviderOfficialCard {...baseProps} isInUse />);
    expect(screen.getByText("In use")).toBeInTheDocument();
  });

  it("disables Activate while settingDefault", () => {
    render(<ProviderOfficialCard {...baseProps} settingDefault />);
    expect(screen.getByRole("button", { name: "Activating…" })).toBeDisabled();
  });

  it("shows the update badge with the new version when a segment has latest", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[{ text: "v2.0.0", latest: "2.1.216" }]}
      />
    );
    expect(screen.getByText("New v2.1.216")).toBeInTheDocument();
  });

  it("shows no update badge when no segment has latest", () => {
    render(
      <ProviderOfficialCard {...baseProps} versions={[{ text: "v2.0.0", latest: null }]} />
    );
    expect(screen.queryByText(/^New /)).toBeNull();
  });

  it("hides the update badge while the version is still loading", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        versionLoading
        versions={[{ text: "v2.0.0", latest: "2.1.216" }]}
      />
    );
    expect(screen.queryByText("New v2.1.216")).toBeNull();
  });

  it("puts the badge after ITS OWN segment, not at the end of the line", () => {
    // Codex's shape: the npm check is the CLI's, so a badge trailing the
    // whole line would read as the desktop App being out of date.
    const { container } = render(
      <ProviderOfficialCard
        {...baseProps}
        app="codex"
        versions={[
          { text: "v0.142.5", label: "CLI", latest: "0.143.0" },
          { text: "v26.707.31428", label: "App" }
        ]}
      />
    );
    const line = container.querySelector("p")!;
    const text = line.textContent ?? "";
    expect(text.indexOf("New v0.143.0")).toBeGreaterThan(text.indexOf("(CLI)"));
    expect(text.indexOf("New v0.143.0")).toBeLessThan(text.indexOf("(App)"));
  });

  it("renders the actions slot before the Activate button", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        actions={<button type="button">Add account</button>}
      />
    );
    const addBtn = screen.getByRole("button", { name: "Add account" });
    const activateBtn = screen.getByRole("button", { name: "Activate" });
    // compareDocumentPosition: FOLLOWING = 4 means addBtn comes before activateBtn in DOM
    expect(
      addBtn.compareDocumentPosition(activateBtn) & Node.DOCUMENT_POSITION_FOLLOWING
    ).toBeTruthy();
  });
});
