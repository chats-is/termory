import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  fireEvent,
  render as rtlRender,
  screen,
  waitFor,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { setFormatLocale } from "@/lib/format";
import { blankGateway, maskKey, providerFromBinding } from "@/lib/provider-utils";
import type { ActiveState, CliApp, Gateway, ProviderBalance } from "@/types";
import { toast } from "sonner";
import { __resetBalanceCacheForTests } from "@/hooks/useBalances";
import { GatewaysPage } from "./GatewaysPage";

// GatewaysPage calls invoke() for `provider_active_states` on mount (and after
// each activate/deactivate). Stub it; per-test we steer the Codex state.
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// Toasts fire on success/failure — keep them out of jsdom.
vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn() }
}));
// deleteGateway confirms via the native dialog — auto-confirm it.
vi.mock("@tauri-apps/plugin-dialog", () => ({
  ask: vi.fn().mockResolvedValue(true)
}));
// The page reads each gateway's wallet through `useBalances`, which
// subscribes to the backend's balance-changed event. Unmocked, the real
// `listen` rejects against a missing Tauri host and vitest reports it as
// an unhandled error.
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {})
}));

beforeEach(() => {
  // The balance cache is module-level (it survives route remounts by
  // design), so one test's reading would otherwise seed the next one's.
  __resetBalanceCacheForTests();
  invokeMock.mockReset();
  vi.mocked(toast.error).mockClear();
  vi.mocked(toast.success).mockClear();
});

function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

const ALL_INSTALLED: Record<CliApp, boolean> = {
  claude: true,
  "claude-desktop": true,
  codex: true,
  gemini: true,
  opencode: true,
  grok: true
};

// A gateway with a single Codex binding. Base URL + key are fixed so the
// derived synth creds are deterministic (used to build a matching live
// snapshot for the "active" case).
function codexGateway(): Gateway {
  return {
    ...blankGateway(),
    name: "GW",
    baseUrl: "https://gw.example.com",
    apiKey: "sk-test-key",
    bindings: [{ id: "bind-codex", app: "codex" }]
  };
}

// invoke returns the given Codex active state for `provider_active_states`;
// commands listed in `reject` fail with that message; anything else resolves null.
function mockInvoke(
  codex: ActiveState | null,
  reject: Record<string, string> = {}
) {
  invokeMock.mockImplementation((cmd: string) => {
    if (reject[cmd]) return Promise.reject(new Error(reject[cmd]));
    return Promise.resolve(
      cmd === "provider_active_states" ? (codex ? [codex] : []) : null
    );
  });
}

function setup(
  codexState: ActiveState | null,
  activeProviderIds: Record<string, string> = {},
  reject: Record<string, string> = {}
) {
  const codexFollowForBinding = vi.fn().mockResolvedValue(undefined);
  const markActive = vi.fn();
  const setGateways = vi.fn();
  mockInvoke(codexState, reject);
  render(
    <GatewaysPage
      gateways={[codexGateway()]}
      setGateways={setGateways}
      addSignal={0}
      markActive={markActive}
      activeProviderIds={activeProviderIds}
      installed={ALL_INSTALLED}
      codexFollowForBinding={codexFollowForBinding}
    />
  );
  return { codexFollowForBinding, markActive, setGateways };
}

describe("GatewaysPage — Codex follow prompt", () => {
  it("routes a Codex binding ACTIVATION through the follow prompt (official→custom)", async () => {
    // Codex on Official; the binding isn't active → an "Activate" button.
    const { codexFollowForBinding } = setup(
      { app: "codex", kind: "official", livePath: "" },
      {}
    );
    fireEvent.click(await screen.findByRole("button", { name: "Activate" }));
    await waitFor(() =>
      expect(codexFollowForBinding).toHaveBeenCalledWith(
        "toCustom",
        expect.any(String),
        expect.any(Function)
      )
    );
  });

  it("routes a Codex binding DEACTIVATION through the follow prompt (custom→official)", async () => {
    // Make the binding read as the active one: marker points at it AND the
    // live snapshot creds match the synth.
    const synth = providerFromBinding(codexGateway(), codexGateway().bindings[0]);
    const { codexFollowForBinding } = setup(
      {
        app: "codex",
        kind: "custom",
        matchedProviderId: "bind-codex",
        liveSnapshot: {
          baseUrl: synth.baseUrl,
          apiKeyMasked: maskKey(synth.apiKey ?? "")
        },
        livePath: ""
      },
      { codex: "bind-codex" }
    );
    fireEvent.click(await screen.findByRole("button", { name: /turn off/i }));
    await waitFor(() =>
      expect(codexFollowForBinding).toHaveBeenCalledWith(
        "toOfficial",
        expect.any(String),
        expect.any(Function)
      )
    );
  });

  it("does NOT prompt when Codex is already on a custom provider (custom→custom)", async () => {
    // Codex is custom, but a DIFFERENT provider is active → this binding shows
    // "Activate"; activating it is custom→custom, so no follow prompt.
    const { codexFollowForBinding } = setup(
      { app: "codex", kind: "custom", matchedProviderId: "other-id", livePath: "" },
      { codex: "other-id" }
    );
    fireEvent.click(await screen.findByRole("button", { name: "Activate" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith(
        "activate_provider",
        expect.anything()
      )
    );
    expect(codexFollowForBinding).not.toHaveBeenCalled();
  });
});

// An active Codex binding: marker points at it + live snapshot creds match the
// synth, so isBindingActive is true and delete attempts a deactivate.
function activeCodexState(): ActiveState {
  const synth = providerFromBinding(codexGateway(), codexGateway().bindings[0]);
  return {
    app: "codex",
    kind: "custom",
    matchedProviderId: "bind-codex",
    liveSnapshot: {
      baseUrl: synth.baseUrl,
      apiKeyMasked: maskKey(synth.apiKey ?? "")
    },
    livePath: ""
  };
}

describe("GatewaysPage — delete cleanup", () => {
  it("keeps the gateway + toasts when a binding's live config can't be cleared", async () => {
    const { setGateways } = setup(
      activeCodexState(),
      { codex: "bind-codex" },
      { deactivate_provider: "database is locked — quit the running Codex" }
    );
    // Wait until the binding reads active (so the delete attempts cleanup).
    await screen.findByRole("button", { name: /turn off/i });
    fireEvent.click(screen.getByRole("button", { name: "Delete AI Gateway" }));
    await waitFor(() => expect(toast.error).toHaveBeenCalled());
    // Cleanup failed → the gateway is NOT removed from the list (retryable).
    expect(setGateways).not.toHaveBeenCalled();
  });

  it("removes the gateway when cleanup succeeds", async () => {
    const { setGateways } = setup(activeCodexState(), { codex: "bind-codex" });
    await screen.findByRole("button", { name: /turn off/i });
    fireEvent.click(screen.getByRole("button", { name: "Delete AI Gateway" }));
    await waitFor(() => expect(setGateways).toHaveBeenCalled());
    expect(toast.error).not.toHaveBeenCalled();
  });
});

describe("GatewaysPage — gateway balance", () => {
  // A gateway is ONE {baseUrl, apiKey} = one wallet, however many CLIs it
  // binds — so the reading is fetched under the GATEWAY's id and rendered
  // once on its card, not repeated per binding row.
  function balance(over: Partial<ProviderBalance> = {}): ProviderBalance {
    return {
      providerId: "gw1",
      status: "ok",
      entries: [{ currency: "USD", remaining: 89.42, depleted: false }],
      queriedAt: Date.now(),
      ...over
    };
  }

  it("queries with the gateway's own id + creds and shows the amount", async () => {
    setFormatLocale("en-US");
    const seen: { id: string; baseUrl?: string; apiKey?: string }[] = [];
    invokeMock.mockImplementation((cmd: string, args: Record<string, unknown>) => {
      if (cmd === "fetch_provider_balance") {
        const subject = args.subject as { id: string; baseUrl?: string; apiKey?: string };
        seen.push(subject);
        return Promise.resolve(balance({ providerId: subject.id }));
      }
      return Promise.resolve(cmd === "provider_active_states" ? [] : null);
    });

    render(
      <GatewaysPage
        gateways={[{ ...codexGateway(), id: "gw1" }]}
        setGateways={vi.fn()}
        addSignal={0}
        markActive={vi.fn()}
        activeProviderIds={{}}
        installed={ALL_INSTALLED}
        codexFollowForBinding={vi.fn().mockResolvedValue(undefined)}
      />
    );

    expect(await screen.findByText("$89.42")).toBeInTheDocument();
    // ONE query, for the gateway itself — not one per binding, and not
    // under a binding's id (which would be a second wallet lookup for the
    // same credentials).
    expect(seen).toEqual([
      { id: "gw1", baseUrl: "https://gw.example.com", apiKey: "sk-test-key" }
    ]);
  });

  it("renders no balance row for a gateway whose host has no wallet API", async () => {
    invokeMock.mockImplementation((cmd: string) =>
      Promise.resolve(
        cmd === "fetch_provider_balance"
          ? balance({ status: "unsupported", entries: [] })
          : cmd === "provider_active_states"
            ? []
            : null
      )
    );
    render(
      <GatewaysPage
        gateways={[{ ...codexGateway(), id: "gw1" }]}
        setGateways={vi.fn()}
        addSignal={0}
        markActive={vi.fn()}
        activeProviderIds={{}}
        installed={ALL_INSTALLED}
        codexFollowForBinding={vi.fn().mockResolvedValue(undefined)}
      />
    );
    await screen.findByRole("button", { name: "Edit AI Gateway" });
    // The value slot holds a balance or nothing at all — never a status
    // word. Most gateways are exactly this case.
    expect(screen.queryByLabelText("Refresh balance")).not.toBeInTheDocument();
  });
});
