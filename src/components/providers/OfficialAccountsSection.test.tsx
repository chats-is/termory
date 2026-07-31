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
import {
  OfficialAccountsSection,
  waitingOnTierName
} from "./OfficialAccountsSection";

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

  it("manages grok saved accounts like codex (switch + delete rows)", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="grok" />);
    await screen.findByText("Work");
    expect(screen.getByRole("button", { name: "Switch" })).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "Delete" }).length).toBeGreaterThan(0);
  });

  // Grok flags needsRelogin BACKEND-side (switch_grok knows a 4xx refresh from
  // a lock/write error; a string Err here does not), so the frontend must not
  // flag it — the reload picks up whatever the backend set. Same split as
  // claude.
  it("does not flag needs-relogin from the frontend when a grok switch fails", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve(makeState());
      if (cmd === "switch_account")
        return Promise.reject(new Error("Grok token refresh failed (400)"));
      return Promise.resolve(null);
    });
    render(<OfficialAccountsSection app="grok" />);
    await screen.findByText("Work");
    await userEvent.click(screen.getByRole("button", { name: "Switch" }));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("switch_account", { id: "acct-b" })
    );
    expect(invokeMock).not.toHaveBeenCalledWith("mark_account_relogin", {
      id: "acct-b",
      needed: true
    });
  });

  // Gemini is the one account-capable CLI that stays display-only: reading a
  // login is not the same as being able to restore one.
  it("keeps gemini display-only (no switch or delete rows)", async () => {
    mockList(makeState());
    render(<OfficialAccountsSection app="gemini" />);
    await screen.findByText("Jane");
    expect(screen.queryByRole("button", { name: "Switch" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
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

  // A failed fetch keeps the previous good numbers on screen
  // (mergeQuotaResult) — the label turns red so they don't read as live.
  it("marks the tier names when the last fetch failed", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="codex"
        quota={makeQuota({ success: false, error: "HTTP 500" })}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(await screen.findByText("5h")).toHaveClass("text-destructive");
    expect(screen.getByText("Weekly")).toHaveClass("text-destructive");
    // The RING keeps the pressure scale — a red ring would read as
    // "nearly spent", which is a different statement.
    expect(screen.getByText("12%")).not.toHaveClass("text-destructive");
  });

  it("leaves the tier names alone on a successful fetch", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection app="codex" quota={makeQuota()} onRefreshQuota={vi.fn()} />
    );
    expect(await screen.findByText("5h")).not.toHaveClass("text-destructive");
  });

  // The mark describes the DATA, not the request. A refresh in flight
  // hasn't replaced the failed numbers yet, so dropping the mark for its
  // duration would flash red → normal → red and present stale data as live.
  it("keeps the names marked while a retry is in flight", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="codex"
        quota={makeQuota({ success: false })}
        quotaLoading
        onRefreshQuota={vi.fn()}
      />
    );
    expect(await screen.findByText("5h")).toHaveClass("text-destructive");
  });

  // grok's billing endpoint serves no usage percentage for some accounts
  // (Free / unified billing), so the backend emits no tier rather than a
  // fake 0%. Nothing is shown in its place — no rings, no placeholder copy
  // (user decision 2026-07-31): an empty result is simply absent.
  // A disabled element fires no pointer events, so the tooltip trigger has
  // to sit on a wrapper — otherwise the cooldown hint, which exists only
  // for the state that disables the button, is unreadable by construction.
  it("keeps the refresh tooltip reachable while the button is disabled", async () => {
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
    const button = screen.getByLabelText("Refresh usage");
    expect(button).toBeDisabled();
    // Walk UP from the button — the card has several tooltips (the rings),
    // so the first trigger in the DOM is not this one. The trigger must be
    // an ANCESTOR, never the disabled button itself.
    expect(button.getAttribute("data-slot")).not.toBe("tooltip-trigger");
    expect(button.closest('[data-slot="tooltip-trigger"]')).not.toBeNull();
  });

  it("renders no quota content for an empty result", async () => {
    mockList(makeState());
    const { container } = render(
      <OfficialAccountsSection
        app="grok"
        quota={makeQuota({ app: "grok", tiers: [] })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Jane");
    // No ring — matched on QuotaRing's own viewBox, since the row's
    // lucide status icon is an svg with a <circle> too.
    expect(container.querySelector('svg[viewBox="0 0 36 36"]')).toBeNull();
    expect(screen.queryByText(/%/)).not.toBeInTheDocument();
    // The row is just the account; only the Refresh control remains.
    expect(screen.getByLabelText("Refresh usage")).toBeInTheDocument();
  });

  // grok's unified-billing model: bought credits arrive as a BALANCE with
  // no limit, and such an account has no on-demand cap — so without this
  // it showed nothing at all.
  it("shows a grok prepaid balance with no ring", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="grok"
        quota={makeQuota({
          app: "grok",
          tiers: [{ name: "seven_day", utilization: 30 }],
          prepaidBalance: 12.5
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    expect(await screen.findByText("Balance")).toBeInTheDocument();
    // Currency symbol varies by the runner's resolved locale (same reason
    // as the Credits test below) — match the amount.
    expect(screen.getByText(/12\.50/)).toBeInTheDocument();
    // One ring only — the window's. A balance has no limit to divide by,
    // so it must not render a percentage of its own.
    expect(screen.getAllByText(/^\d+%$/)).toHaveLength(1);
  });

  it("omits the balance when the account has none", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="grok"
        quota={makeQuota({ app: "grok", tiers: [{ name: "seven_day", utilization: 30 }] })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Weekly");
    expect(screen.queryByText("Balance")).not.toBeInTheDocument();
  });

  it("labels a model-scoped window with its period, not a bare model name", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({
          tiers: [
            { name: "five_hour", utilization: 9 },
            // What the live API sends for a per-model weekly: the name is
            // the MODEL, the period comes from `group`.
            { name: "Fable", group: "weekly", utilization: 4 }
          ]
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("5h");
    expect(screen.getByText("Weekly · Fable")).toBeInTheDocument();
    // The bare model name must NOT be a row of its own.
    expect(screen.queryByText("Fable")).not.toBeInTheDocument();
  });

  it("shows the countdown on exactly one window when two share a name", async () => {
    mockList(makeState());
    const soon = new Date(Date.now() + 3 * 3_600_000).toISOString();
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({
          // A model whose display name happens to equal a window id: the
          // countdown is resolved to ONE row, not matched by name per row.
          tiers: [
            { name: "seven_day", utilization: 40, resetsAt: soon },
            { name: "seven_day", group: "weekly", utilization: 4, resetsAt: soon }
          ]
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Weekly · seven_day");
    expect(screen.getAllByText(/in \d/)).toHaveLength(1);
  });

  it("renders an unrecognized period group verbatim", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({
          tiers: [{ name: "Fable", group: "fortnightly", utilization: 4 }]
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    // A period this build doesn't know still surfaces — same rule as
    // unknown window ids, so a new API grouping needs no release.
    expect(await screen.findByText("fortnightly · Fable")).toBeInTheDocument();
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

  it("renders quota in the single current-account row for display-only apps (Gemini)", async () => {
    // Gemini is the remaining display-only quota app — Claude gained full
    // management (its `saved: true` now always has a matching accounts entry).
    mockList(
      makeState({
        current: { name: "John", email: "john@example.com", plan: null, saved: true },
        accounts: []
      })
    );
    render(
      <OfficialAccountsSection
        app="gemini"
        quota={makeQuota({ app: "gemini" })}
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

  it("renders the Usage credits item, scaling minor units by decimalPlaces", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({
          app: "claude",
          extraUsage: {
            isEnabled: true,
            monthlyLimit: 5000,
            usedCredits: 1944,
            utilization: 38.88,
            currency: "USD",
            decimalPlaces: 2
          }
        })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Jane");
    expect(screen.getByText("Credits")).toBeInTheDocument();
    // 1944 / 10^2 → 19.44 used of 5000 / 10^2 → 50.00 (currency symbol
    // varies by the test runner's resolved locale, so match the amounts).
    expect(screen.getByText(/19\.44 \/ .*50\.00/)).toBeInTheDocument();
  });

  it("hides the Usage credits item when extraUsage is disabled", async () => {
    mockList(makeState());
    render(
      <OfficialAccountsSection
        app="claude"
        quota={makeQuota({ app: "claude", extraUsage: { isEnabled: false } })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("Jane");
    expect(screen.queryByText("Credits")).toBeNull();
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
        app="gemini"
        quota={makeQuota({ plan: "Max" })}
        onRefreshQuota={vi.fn()}
      />
    );
    await screen.findByText("John");
    // CSS `uppercase` is visual-only; the DOM contains the raw plan string.
    expect(screen.getByText("Max")).toBeInTheDocument();
  });

  it("treats Claude as managed: unsaved current row offers Save current", async () => {
    mockList(
      makeState({
        current: { name: "John", email: "john@example.com", plan: "Max", saved: false },
        accounts: []
      })
    );
    render(<OfficialAccountsSection app="claude" />);
    await screen.findByText("John");
    // Managed UI: the unsaved live login gets the snapshot affordance the
    // display-only rendering never shows.
    expect(screen.getByRole("button", { name: "Save current" })).toBeInTheDocument();
  });
});

describe("waitingOnTierName", () => {
  const soon = "2026-07-25T14:00:00Z";
  const later = "2026-07-31T00:00:00Z";

  it("returns null for no tiers or all-zero utilization", () => {
    expect(waitingOnTierName([])).toBeNull();
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 0, resetsAt: soon },
        { name: "seven_day", utilization: 0, resetsAt: later }
      ])
    ).toBeNull();
  });

  it("picks the soonest-resetting window while nothing is spent", () => {
    // 5h at 29% resets this afternoon while the weekly sits at 46% five days
    // out — neither blocks, so the 5h top-up is the actionable wait.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 29, resetsAt: soon },
        { name: "seven_day", utilization: 46, resetsAt: later }
      ])
    ).toBe("five_hour");
  });

  it("switches to the spent window once one is exhausted", () => {
    // 5h spent, weekly still has room → waiting on the 5h reset.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 100, resetsAt: soon },
        { name: "seven_day", utilization: 46, resetsAt: later }
      ])
    ).toBe("five_hour");
    // Weekly spent while the 5h still has room → the 5h reset frees nothing,
    // so the wait is the weekly, even though it resets much later.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 30, resetsAt: soon },
        { name: "seven_day", utilization: 100, resetsAt: later }
      ])
    ).toBe("seven_day");
  });

  it("ignores a spent MODEL-scoped window while other models are usable", () => {
    // Claude Max with the Fable weekly burned: Sonnet still works, so nothing
    // is blocked and the countdown stays on the next top-up. Counting Fable
    // would answer "wait 5 days" to someone who can work right now.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 30, resetsAt: soon },
        { name: "seven_day", utilization: 40, resetsAt: later },
        { name: "Fable", utilization: 100, resetsAt: later }
      ])
    ).toBe("five_hour");
    // Gemini's windows are model-scoped throughout — one spent bucket just
    // means using another model.
    expect(
      waitingOnTierName([
        { name: "gemini_pro", utilization: 100, resetsAt: later },
        { name: "gemini_flash", utilization: 20, resetsAt: soon }
      ])
    ).toBe("gemini_flash");
  });

  it("treats a window carrying an API group as model-scoped", () => {
    // The API says this window is scoped to one model, so a spent one
    // blocks nothing — settled by the `group` field, not by whether the
    // name happens to be missing from the account-wide id list. A model
    // named like a window id ("seven_day") must not read as account-wide.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 30, resetsAt: soon },
        { name: "seven_day", group: "weekly", utilization: 100, resetsAt: later }
      ])
    ).toBe("five_hour");
  });

  it("blocks on model-scoped windows once EVERY one is spent", () => {
    // No model left to switch to, so the last of them is the real wait.
    expect(
      waitingOnTierName([
        { name: "gemini_flash", utilization: 100, resetsAt: soon },
        { name: "gemini_pro", utilization: 100, resetsAt: later }
      ])
    ).toBe("gemini_pro");
  });

  it("treats generated {n}_hour / {n}_day ids as account-wide", () => {
    // Codex derives these from the window seconds; they are account-wide, so a
    // spent one blocks even though it is not in the literal id set.
    expect(
      waitingOnTierName([
        { name: "3_hour", utilization: 100, resetsAt: soon },
        { name: "14_day", utilization: 20, resetsAt: later }
      ])
    ).toBe("3_hour");
  });

  it("picks the LAST of several spent windows", () => {
    // Both spent: the 5h clears this afternoon but the weekly still blocks
    // until the 31st, so that is when work resumes.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 100, resetsAt: soon },
        { name: "seven_day", utilization: 100, resetsAt: later }
      ])
    ).toBe("seven_day");
  });

  it("still picks the soonest when the usage sits on another window", () => {
    // Fresh 5h window, weekly already used — the countdown belongs on the 5h
    // boundary, and the all-zero guard must look across every tier.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 0, resetsAt: soon },
        { name: "seven_day", utilization: 12, resetsAt: later }
      ])
    ).toBe("five_hour");
  });

  it("ignores windows with a missing or unparseable reset time", () => {
    expect(
      waitingOnTierName([
        { name: "Fable", utilization: 5 },
        { name: "five_hour", utilization: 40, resetsAt: "not-a-date" },
        { name: "seven_day", utilization: 88, resetsAt: later }
      ])
    ).toBe("seven_day");
    // No window has a usable reset time at all.
    expect(
      waitingOnTierName([{ name: "five_hour", utilization: 40 }])
    ).toBeNull();
  });

  it("keeps the first window on an identical reset time", () => {
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 50, resetsAt: soon },
        { name: "seven_day", utilization: 90, resetsAt: soon }
      ])
    ).toBe("five_hour");
    // Same tie-break on the spent branch.
    expect(
      waitingOnTierName([
        { name: "five_hour", utilization: 100, resetsAt: soon },
        { name: "seven_day", utilization: 100, resetsAt: soon }
      ])
    ).toBe("five_hour");
  });
});
