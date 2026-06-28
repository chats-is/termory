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
import type { AccountsState } from "@/types";
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
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

function makeState(over: Partial<AccountsState> = {}): AccountsState {
  return {
    current: { name: "Jane", email: "jane@example.com", plan: "Max", saved: true },
    accounts: [
      {
        id: "codex:a",
        label: "Jane",
        name: "Jane",
        email: "jane@example.com",
        plan: "Max",
        savedAt: "2026-06-27T00:00:00Z",
        active: true
      },
      {
        id: "codex:b",
        label: "Work",
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

describe("OfficialAccountsSection", () => {
  it("lists saved accounts with the active one marked", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="codex" />);
    expect(await screen.findByText("Jane")).toBeInTheDocument();
    expect(screen.getByText("Work")).toBeInTheDocument();
    expect(screen.getByText(/jane@example\.com/)).toBeInTheDocument();
    // The active account is marked by a checkmark only (no "Active" badge),
    // and has no Switch button — only the non-active "Work" does.
    expect(screen.queryByText("Active")).toBeNull();
    expect(screen.getAllByRole("button", { name: "Switch" })).toHaveLength(1);
    expect(invokeMock).toHaveBeenCalledWith("list_accounts", { app: "codex" });
  });

  it("shows the styled empty hint when there are no saved accounts", async () => {
    mockList(makeState({ current: null, accounts: [] }));
    render(<OfficialAccountsSection app="codex" />);
    // Always renders the card (even unused) with the empty hint — no rows.
    expect(await screen.findByText(/No saved accounts/i)).toBeInTheDocument();
    expect(screen.queryByText("Work")).toBeNull();
  });

  it("switches a non-active account after confirm and notifies the parent", async () => {
    mockList(makeState());
    const onSwitched = vi.fn();
    render(<OfficialAccountsSection app="codex" onSwitched={onSwitched} />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("switch_account", {
        id: "codex:b"
      })
    );
    await waitFor(() => expect(onSwitched).toHaveBeenCalled());
  });

  it("does not switch when cancelled", async () => {
    mockList(makeState());
    askMock.mockResolvedValue(false);
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    expect(invokeMock).not.toHaveBeenCalledWith(
      "switch_account",
      expect.anything()
    );
  });

  it("deletes after confirm", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getAllByRole("button", { name: "Delete" })[0]);
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_account", {
        id: "codex:a"
      })
    );
  });

  it("always shows the current login (active, with a Save button) even when unsaved", async () => {
    mockList(
      makeState({
        current: { name: "Jane", email: "a@example.com", plan: "Pro", saved: false },
        accounts: []
      })
    );
    render(<OfficialAccountsSection app="codex" />);
    // The live login appears without being saved first, marked active
    // (no Switch button), and offers a Save action.
    expect(await screen.findByText("Jane")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Switch" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Save current" }));
    const input = await screen.findByLabelText("Account name");
    await userEvent.clear(input);
    await userEvent.type(input, "My Codex");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("save_account", {
        app: "codex",
        label: "My Codex"
      })
    );
  });

  it("shows no Save button once the current login is already saved", async () => {
    mockList(makeState()); // current.saved === true
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Work");
    // The saved active account gets rename/delete instead of a Save button.
    expect(screen.queryByRole("button", { name: "Save current" })).toBeNull();
  });

  it("shows the keyring warning when present", async () => {
    mockList(makeState({ storageWarning: "keyring" }));
    render(<OfficialAccountsSection app="codex" />);
    expect(await screen.findByText(/keyring/i)).toBeInTheDocument();
  });

  it("shows Add account button for codex and calls login_and_save_codex_account", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve(makeState());
      if (cmd === "login_and_save_codex_account") return Promise.resolve("codex:c");
      return Promise.resolve(null);
    });
    const onSwitched = vi.fn();
    render(<OfficialAccountsSection app="codex" onSwitched={onSwitched} />);
    await screen.findByText("Jane");
    const addBtn = screen.getByRole("button", { name: /add account/i });
    expect(addBtn).toBeInTheDocument();
    await userEvent.click(addBtn);
    // Wait for the full async chain: IPC → reload → onSwitched
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("login_and_save_codex_account");
      expect(onSwitched).toHaveBeenCalled();
    });
  });

  it("shows loading state while logging in", async () => {
    let resolve!: (v: string) => void;
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve(makeState());
      if (cmd === "login_and_save_codex_account")
        return new Promise<string>((r) => { resolve = r; });
      return Promise.resolve(null);
    });
    render(<OfficialAccountsSection app="codex" />);
    await screen.findByText("Jane");
    await userEvent.click(screen.getByRole("button", { name: /add account/i }));
    await screen.findByText(/waiting for browser login/i);
    expect(screen.getByRole("button", { name: /waiting/i })).toBeDisabled();
    resolve("codex:c");
    await waitFor(() =>
      expect(screen.queryByText(/waiting for browser login/i)).toBeNull()
    );
  });

  it("does not show Add account button for non-codex apps", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="claude" />);
    await screen.findByText("Jane");
    expect(screen.queryByRole("button", { name: /add account/i })).toBeNull();
  });
});
