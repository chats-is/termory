import React from "react";
import { beforeAll, describe, expect, it, vi } from "vitest";
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

// Clicking a badge hovers it first, which opens its Radix Tooltip —
// and Radix's Popper needs a ResizeObserver that jsdom doesn't ship.
// Without this the click throws instead of firing onUpgrade. (Local to
// this file, per the project's shim convention.)
beforeAll(() => {
  if (!("ResizeObserver" in globalThis)) {
    globalThis.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof ResizeObserver;
  }
});

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

  // Upgrade STATE rules (which segment reacts, tone, disabled) live in
  // `upgradeBadgeState` and are table-tested in provider-utils.test.ts.
  // These cover only that the card renders that state correctly.

  it("renders an idle update badge as a button that upgrades on click", async () => {
    const user = userEvent.setup();
    const onUpgrade = vi.fn();
    render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[
          { text: "v2.0.0", latest: "2.1.216", upgradeCommand: "claude update" }
        ]}
        onUpgrade={onUpgrade}
      />
    );
    await user.click(screen.getByRole("button", { name: /New v2\.1\.216/ }));
    // No argument: the backend derives what to run from the app alone.
    expect(onUpgrade).toHaveBeenCalledWith();
  });

  it("renders a segment with no upgrade command as a plain badge", () => {
    render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[{ text: "v2.0.0", latest: "2.1.216" }]}
        onUpgrade={vi.fn()}
      />
    );
    expect(screen.getByText("New v2.1.216")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /New v2\.1\.216/ })).toBeNull();
  });

  it("reads 'Upgrading' and goes disabled while an upgrade runs", async () => {
    const user = userEvent.setup();
    const onUpgrade = vi.fn();
    render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[
          { text: "v2.0.0", latest: "2.1.216", upgradeCommand: "claude update" }
        ]}
        onUpgrade={onUpgrade}
        upgrading
      />
    );
    // The label swaps rather than showing a spinner.
    expect(screen.queryByText("New v2.1.216")).toBeNull();
    const badge = screen.getByRole("button", { name: /Upgrading/ });
    expect(badge).toBeDisabled();
    await user.click(badge);
    expect(onUpgrade).not.toHaveBeenCalled();
  });

  it("renders no tooltip while an upgrade runs", () => {
    // A tooltip left open from before the click would keep rendering the
    // command, which is re-probed mid-run — Codex briefly resolves to the
    // desktop app's absolute path while npm replaces the binary.
    const { container } = render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[
          { text: "v2.0.0", latest: "2.1.216", upgradeCommand: "claude update" }
        ]}
        onUpgrade={vi.fn()}
        upgrading
      />
    );
    expect(container.querySelector("[data-slot=tooltip-trigger]")).toBeNull();
    expect(screen.getByRole("button", { name: /Upgrading/ })).toBeInTheDocument();
  });

  it("renders a failed badge red and still clickable", async () => {
    const user = userEvent.setup();
    const onUpgrade = vi.fn();
    render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[
          { text: "v2.0.0", latest: "2.1.216", upgradeCommand: "claude update" }
        ]}
        onUpgrade={onUpgrade}
        upgradeError="EACCES: permission denied"
      />
    );
    const badge = screen.getByRole("button", { name: /New v2\.1\.216/ });
    expect(badge.className).toContain("text-destructive");
    await user.click(badge);
    expect(onUpgrade).toHaveBeenCalledTimes(1);
  });

  it("never grows a row for upgrade state", () => {
    // All of it rides on the badge; the card's layout must not shift.
    const versions = [
      { text: "v2.0.0", latest: "2.1.216", upgradeCommand: "claude update" }
    ];
    const rows = (ui: React.ReactElement) =>
      render(ui).container.querySelectorAll("p").length;
    const idle = rows(
      <ProviderOfficialCard {...baseProps} versions={versions} onUpgrade={vi.fn()} />
    );
    expect(
      rows(
        <ProviderOfficialCard
          {...baseProps}
          versions={versions}
          onUpgrade={vi.fn()}
          upgrading
        />
      )
    ).toBe(idle);
    expect(
      rows(
        <ProviderOfficialCard
          {...baseProps}
          versions={versions}
          onUpgrade={vi.fn()}
          upgradeError="EACCES: permission denied"
        />
      )
    ).toBe(idle);
  });

  it("confines upgrade state to Codex's upgradable segment", () => {
    // Codex renders CLI + App; only the CLI is upgradable. Upgrading it
    // must not make the App segment read "Updating".
    render(
      <ProviderOfficialCard
        {...baseProps}
        app="codex"
        versions={[
          {
            text: "v0.144.6",
            label: "CLI",
            latest: "0.145.0",
            upgradeCommand: "codex update"
          },
          { text: "v26.707.31428", label: "App", latest: "26.721.30844" }
        ]}
        onUpgrade={vi.fn()}
        upgrading
      />
    );
    expect(screen.getAllByText("Upgrading")).toHaveLength(1);
    expect(screen.getByText("New v26.721.30844")).toBeInTheDocument();
  });

  it("offers no terminal button anywhere on a failed card", () => {
    // The terminal is only ever TEXT inside the tooltip (Radix's
    // floating content doesn't render under jsdom, so no test here
    // inspects tooltip contents).
    render(
      <ProviderOfficialCard
        {...baseProps}
        versions={[
          { text: "v2.0.0", latest: "2.1.216", upgradeCommand: "claude update" }
        ]}
        onUpgrade={vi.fn()}
        upgradeError="EACCES: permission denied"
      />
    );
    expect(screen.queryByRole("button", { name: /terminal/i })).toBeNull();
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
