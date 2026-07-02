import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  screen,
  waitFor,
  type RenderOptions
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { AccountsState, SubscriptionQuota } from "@/types";
import { OfficialAccountsSection } from "./OfficialAccountsSection";

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver;
Element.prototype.scrollIntoView ??= () => {};
(globalThis as unknown as { PointerEvent: typeof MouseEvent }).PointerEvent ??=
  MouseEvent as unknown as typeof PointerEvent;
HTMLElement.prototype.hasPointerCapture ??= () => false;
HTMLElement.prototype.setPointerCapture ??= () => {};
HTMLElement.prototype.releasePointerCapture ??= () => {};

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {})
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ onFocusChanged: vi.fn().mockResolvedValue(() => {}) })
}));
const { askMock } = vi.hoisted(() => ({ askMock: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ ask: askMock }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn(), warning: vi.fn() } }));

function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

function makeState(over: Partial<AccountsState> = {}): AccountsState {
  return {
    current: { name: "Jane", email: "jane@example.com", plan: "Max", saved: true },
    accounts: [
      {
        id: "acct-a",
        name: "Jane",
        email: "jane@example.com",
        plan: "Max",
        savedAt: "2026-06-27T00:00:00Z",
        active: true
      },
      {
        id: "acct-b",
        name: "Work",
        email: "work@example.com",
        plan: "Plus",
        savedAt: "2026-06-26T00:00:00Z",
        active: false
      }
    ],
    storageWarning: null,
    ...over
  };
}

function makeQuota(overrides: Partial<SubscriptionQuota> = {}): SubscriptionQuota {
  return {
    app: "claude",
    credentialStatus: "valid",
    success: true,
    tiers: [
      { name: "five_hour", utilization: 12.4, resetsAt: "2099-01-01T00:00:00Z" },
      { name: "seven_day", utilization: 41 }
    ],
    queriedAt: Date.now(),
    ...overrides
  };
}

function mockList(state: AccountsState) {
  invokeMock.mockImplementation((cmd: string) =>
    cmd === "list_accounts" ? Promise.resolve(state) : Promise.resolve(null)
  );
}

beforeEach(() => {
  invokeMock.mockReset();
  askMock.mockReset();
  askMock.mockResolvedValue(true);
});

describe("OfficialAccountsSection — Codex management", () => {
  it("lists saved accounts with the active one marked", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="codex" />);
    expect(await screen.findByText("Jane")).toBeInTheDocument();
    expect(screen.getByText("Work")).toBeInTheDocument();
    expect(screen.getByText(/jane@example\.com/)).toBeInTheDocument();
    // Only acct-b (non-active) gets a Switch button
    expect(screen.getAllByRole("button", { name: "Switch" })).toHaveLength(1);
    expect(invokeMock).toHaveBeenCalledWith("list_accounts", { app: "codex" });
  });

  it("renders without error when there is no current login (not logged in)", async () => {
    mockList(makeState({ current: null, accounts: [] }));
    const { container } = render(<OfficialAccountsSection app="codex" />);
    // State loaded but no rows and no current → empty section, no hint
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("list_accounts", { app: "codex" })
    );
    expect(screen.queryByText(/No saved accounts/i)).toBeNull();
    expect(container.querySelector("[class]")).toBeTruthy(); // section wrapper is in DOM
  });

  it("shows the empty hint when logged in but nothing saved yet", async () => {
    mockList(
      makeState({
        current: { name: "Jane", email: "jane@example.com", plan: "Max", saved: true },
        accounts: []
      })
    );
    render(<OfficialAccountsSection app="codex" />);
    expect(await screen.findByText(/No saved accounts/i)).toBeInTheDocument();
  });

  it("switches a non-active account after confirm and notifies the parent", async () => {
    mockList(makeState());
    const onSwitched = vi.fn();
    render(<OfficialAccountsSection app="codex" onSwitched={onSwitched} />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("switch_account", { id: "acct-b" })
    );
    await waitFor(() => expect(onSwitched).toHaveBeenCalled());
  });

  it("marks account needs-relogin when refresh fails, shows badge and Re-login button", async () => {
    const stateWithExpired: AccountsState = {
      ...makeState(),
      accounts: [
        {
          id: "acct-a",
          name: "Jane",
          email: "jane@example.com",
          plan: "Max",
          savedAt: "2026-06-27T00:00:00Z",
          active: true,
          needsRelogin: false
        },
        {
          id: "acct-b",
          name: "Work",
          email: "work@example.com",
          plan: "Plus",
          savedAt: "2026-06-26T00:00:00Z",
          active: false,
          needsRelogin: true
        }
      ]
    };
    mockList(stateWithExpired);
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    expect(screen.getByText("Needs re-login")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /re-login/i })).toBeInTheDocument();
  });

  it("calls mark_account_relogin with needed=true when switch_account fails", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve(makeState());
      if (cmd === "switch_account") return Promise.reject(new Error("Token refresh failed (401)"));
      return Promise.resolve(null);
    });
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("mark_account_relogin", {
        id: "acct-b",
        needed: true
      })
    );
  });

  it("calls mark_account_relogin with needed=false when switch_account succeeds", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve(makeState());
      if (cmd === "switch_account") return Promise.resolve(null);
      return Promise.resolve(null);
    });
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("mark_account_relogin", {
        id: "acct-b",
        needed: false
      })
    );
  });

  it("does not switch when cancelled", async () => {
    mockList(makeState());
    askMock.mockResolvedValue(false);
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    expect(invokeMock).not.toHaveBeenCalledWith("switch_account", expect.anything());
  });

  it("deletes after confirm", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getAllByRole("button", { name: "Delete" })[0]);
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_account", { id: "acct-a" })
    );
  });

  it("shows unsaved current login with a Bookmark save button", async () => {
    mockList(
      makeState({
        current: { name: "Jane", email: "a@example.com", plan: "Pro", saved: false },
        accounts: []
      })
    );
    render(<OfficialAccountsSection app="codex" />);
    expect(await screen.findByText("Jane")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Switch" })).toBeNull();
    // "Save current" is now an icon button; accessible name = aria-label
    await userEvent.click(screen.getByRole("button", { name: "Save current" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("save_account", { app: "codex" })
    );
  });

  it("shows no Save button once the current login is already saved", async () => {
    mockList(makeState()); // current.saved === true
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    expect(screen.queryByRole("button", { name: "Save current" })).toBeNull();
  });

  it("shows the keyring warning when present", async () => {
    mockList(makeState({ storageWarning: "keyring" }));
    render(<OfficialAccountsSection app="codex" />);
    expect(await screen.findByText(/keyring/i)).toBeInTheDocument();
  });

  it("reloads accounts when externalTrigger changes", async () => {
    let callCount = 0;
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") {
        callCount++;
        return Promise.resolve(makeState());
      }
      return Promise.resolve(null);
    });
    const { rerender } = render(
      <OfficialAccountsSection app="codex" externalTrigger={0} />
    );
    await screen.findByText("Jane");
    const afterMount = callCount;
    // Increment externalTrigger → should trigger a reload
    rerender(
      <TooltipProvider>
        <OfficialAccountsSection app="codex" externalTrigger={1} />
      </TooltipProvider>
    );
    await waitFor(() => expect(callCount).toBeGreaterThan(afterMount));
  });
});

describe("OfficialAccountsSection — quota rings in active row", () => {
  it("renders quota tier rings in the active account row when quota is passed", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="codex"
        quota={makeQuota()}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Jane");
    // Quota rings for both tiers
    expect(screen.getByText("5h")).toBeInTheDocument();
    expect(screen.getByText("Weekly")).toBeInTheDocument();
    expect(screen.getByText("12%")).toBeInTheDocument();
    expect(screen.getByText("41%")).toBeInTheDocument();
  });

  it("shows the Refresh button and calls onRefreshQuota when clicked", async () => {
    const user = userEvent.setup();
    const onRefresh = vi.fn();
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="codex"
        quota={makeQuota()}
        onRefreshQuota={onRefresh}
      />
    );
    await screen.findByText("Jane");
    await user.click(screen.getByLabelText("Refresh usage"));
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });

  it("disables the Refresh button when in cooldown", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="codex"
        quota={makeQuota()}
        quotaCooldown
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Jane");
    expect(screen.getByLabelText("Refresh usage")).toBeDisabled();
  });

  it("hides quota section when there is no OAuth login (not_found)", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="codex"
        quota={makeQuota({ credentialStatus: "not_found", success: false, tiers: [] })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Jane");
    expect(screen.queryByText("5h")).toBeNull();
    expect(screen.queryByLabelText("Refresh usage")).toBeNull();
  });

  it("renders quota in the single current-account row for display-only apps (Claude)", async () => {
    mockList(
      makeState({
        current: { name: "John", email: "john@example.com", plan: null, saved: true },
        accounts: []
      })
    );
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({ app: "claude" })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("John");
    expect(screen.getByText("5h")).toBeInTheDocument();
    expect(screen.getByText("12%")).toBeInTheDocument();
  });

  it("renders nothing when there is no current account for a display-only app", async () => {
    mockList(makeState({ current: null, accounts: [] }));
    const { container } = render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({ app: "claude" })}
        onRefreshQuota={vi.fn()}
      />
    );
    // Component returns null → nothing in the DOM
    expect(container.firstChild).toBeNull();
  });

  it("uses quota.plan in the display-only row when current.plan is null", async () => {
    mockList(
      makeState({
        current: { name: "John", email: "john@example.com", plan: null, saved: true },
        accounts: []
      })
    );
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({ plan: "Max" })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("John");
    // CSS `uppercase` is visual-only; the DOM contains the raw plan string.
    expect(screen.getByText("Max")).toBeInTheDocument();
  });
});
