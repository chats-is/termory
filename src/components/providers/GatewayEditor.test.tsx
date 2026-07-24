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
import { blankGateway } from "@/lib/provider-utils";
import type { CliApp, Gateway, GatewayCapabilities } from "@/types";
import { GatewayEditor } from "./GatewayEditor";

// GatewayEditor calls invoke() for `detect_gateway_apis` (auto-detect once
// base URL + key are entered) and `fetch_provider_favicon` (on save). Stub
// invoke so it runs without a Tauri host; per-test we steer the return value
// by command name.
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

// A capabilities object that makes every CLI bindable (all four modes ok).
const ALL_CAPS: GatewayCapabilities = {
  openaiCompatible: true,
  openai: true,
  anthropic: true,
  gemini: true,
  models: ["gpt-5", "claude-sonnet-4-6"]
};

// Default IPC behavior: detection returns ALL_CAPS, favicon returns null.
function stubInvoke(caps: GatewayCapabilities | Error = ALL_CAPS) {
  invokeMock.mockImplementation((cmd: string) => {
    if (cmd === "detect_gateway_apis") {
      return caps instanceof Error
        ? Promise.reject(caps)
        : Promise.resolve(caps);
    }
    if (cmd === "fetch_provider_favicon") return Promise.resolve(null);
    return Promise.resolve(null);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  vi.useRealTimers();
  stubInvoke();
});

// GatewayEditor itself doesn't mount a Tooltip, but the project convention is
// to wrap standalone renders in TooltipProvider so a future affordance can't
// break the suite.
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

function setup(
  overrides?: Partial<Gateway>,
  isNew = true,
  installed: Record<CliApp, boolean> = ALL_INSTALLED,
  visibleApps?: readonly CliApp[]
) {
  const onSave = vi.fn();
  const onClose = vi.fn();
  render(
    <GatewayEditor
      gateway={{ ...blankGateway(), ...overrides }}
      isNew={isNew}
      installed={installed}
      visibleApps={visibleApps}
      onSave={onSave}
      onClose={onClose}
    />
  );
  return { onSave, onClose };
}

const createBtn = () => screen.getByRole("button", { name: /^create$/i });

// Fill name + base URL + key, which triggers the debounced auto-detect, then
// wait for at least one CLI bind checkbox to become enabled (caps arrived).
async function fillCredsAndDetect() {
  fireEvent.change(screen.getByLabelText("Name"), { target: { value: "GW" } });
  fireEvent.change(screen.getByLabelText("Base URL"), {
    target: { value: "https://gw.example.com" }
  });
  fireEvent.change(screen.getByLabelText("API key"), {
    target: { value: "sk-test-key" }
  });
  await waitFor(
    () => expect(invokeMock).toHaveBeenCalledWith(
      "detect_gateway_apis",
      expect.anything()
    ),
    { timeout: 2000 }
  );
  await waitFor(() =>
    expect(screen.getByLabelText("Apply Claude Code")).not.toBeDisabled()
  );
}

describe("GatewayEditor — rendering", () => {
  it("renders the add dialog with the gateway name", () => {
    setup();
    expect(
      screen.getByRole("heading", { name: /add provider/i })
    ).toBeInTheDocument();
    expect(screen.getByText("AI Gateway")).toBeInTheDocument();
    expect(screen.getByLabelText("Name")).toBeInTheDocument();
    expect(screen.getByLabelText("Base URL")).toBeInTheDocument();
    expect(screen.getByLabelText("API key")).toBeInTheDocument();
  });

  it("shows the edit title when not new", () => {
    setup({ name: "Existing" }, false);
    expect(
      screen.getByRole("heading", { name: /edit provider/i })
    ).toBeInTheDocument();
  });

  it("disables save until a name and base URL are entered", () => {
    setup();
    expect(createBtn()).toBeDisabled();
    fireEvent.change(screen.getByLabelText("Name"), {
      target: { value: "GW" }
    });
    expect(createBtn()).toBeDisabled();
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://gw.example.com" }
    });
    expect(createBtn()).not.toBeDisabled();
  });
});

describe("GatewayEditor — detection + binding", () => {
  it("leaves bind checkboxes disabled before detection runs", () => {
    setup();
    // No creds entered → no caps → every CLI checkbox disabled.
    expect(screen.getByLabelText("Apply Claude Code")).toBeDisabled();
    expect(screen.getByLabelText("Apply Codex")).toBeDisabled();
  });

  it("auto-detects once base URL + key are entered and enables binding", async () => {
    setup();
    await fillCredsAndDetect();
    expect(screen.getByLabelText("Apply Claude Code")).not.toBeDisabled();
    expect(screen.getByLabelText("Apply Codex")).not.toBeDisabled();
    expect(screen.getByLabelText("Apply Gemini")).not.toBeDisabled();
    expect(screen.getByLabelText("Apply OpenCode")).not.toBeDisabled();
  });

  it("only offers a binding for an INSTALLED app, even when the gateway supports it", async () => {
    // Claude Desktop + Codex not installed; Claude Code + the rest installed.
    setup({}, true, {
      ...ALL_INSTALLED,
      "claude-desktop": false,
      codex: false
    });
    await fillCredsAndDetect();
    // Capable gateway, but the not-installed apps can't be bound…
    expect(screen.getByLabelText("Apply Claude Desktop")).toBeDisabled();
    expect(screen.getByLabelText("Apply Codex")).toBeDisabled();
    // …while installed ones stay bindable.
    expect(screen.getByLabelText("Apply Claude Code")).not.toBeDisabled();
    expect(screen.getByLabelText("Apply Gemini")).not.toBeDisabled();
  });
});

describe("GatewayEditor — save callback", () => {
  it("saves a gateway with a checked binding in the expected shape", async () => {
    const { onSave } = setup();
    await fillCredsAndDetect();

    // Bind to Claude (single-mode, no extra required fields).
    fireEvent.click(screen.getByLabelText("Apply Claude Code"));
    fireEvent.click(createBtn());

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved: Gateway = onSave.mock.calls[0][0];
    expect(saved.kind).toBe("gateway");
    expect(saved.name).toBe("GW");
    // Base URL stored path-less.
    expect(saved.baseUrl).toBe("https://gw.example.com");
    expect(saved.apiKey).toBe("sk-test-key");
    expect(saved.capabilities).toEqual(ALL_CAPS);
    expect(saved.bindings).toHaveLength(1);
    const binding = saved.bindings[0];
    expect(binding.app).toBe("claude");
    expect(typeof binding.id).toBe("string");
    expect(binding.id.length).toBeGreaterThan(0);
  });

  it("strips a pasted /v1 suffix from the saved base URL", async () => {
    const { onSave } = setup();
    fireEvent.change(screen.getByLabelText("Name"), {
      target: { value: "GW" }
    });
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://gw.example.com/v1" }
    });
    // No binding needed — a gateway with zero bindings is allowed.
    fireEvent.click(createBtn());

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved: Gateway = onSave.mock.calls[0][0];
    expect(saved.baseUrl).toBe("https://gw.example.com");
    expect(saved.bindings).toEqual([]);
  });

  it("blocks save when a checked OpenCode binding has no models", async () => {
    const { onSave } = setup();
    await fillCredsAndDetect();

    fireEvent.click(screen.getByLabelText("Apply OpenCode"));
    // OpenCode now requires a models LIST (like Grok) → save is blocked with
    // an empty list even though the checkbox is checked. (The inline error
    // lives inside the per-CLI collapsible, which isn't necessarily expanded;
    // the disabled save button is the load-bearing assertion.)
    expect(createBtn()).toBeDisabled();
    fireEvent.click(createBtn());
    expect(onSave).not.toHaveBeenCalled();
  });
});

describe("GatewayEditor — close", () => {
  it("calls onClose when Cancel is clicked", () => {
    const { onClose } = setup();
    fireEvent.click(screen.getByRole("button", { name: /^cancel$/i }));
    expect(onClose).toHaveBeenCalled();
  });

  it("hides a disabled tool's binding row entirely (visibleApps)", async () => {
    setup(undefined, true, ALL_INSTALLED, [
      "claude",
      "claude-desktop",
      "gemini",
      "opencode"
    ]);
    await fillCredsAndDetect();
    expect(screen.getByLabelText("Apply Claude Code")).toBeInTheDocument();
    expect(screen.queryByLabelText("Apply Codex")).toBeNull();
  });

  it("REPRO: editing an existing gateway, checking Grok with empty model blocks save", async () => {
    setup(
      {
        name: "GW",
        baseUrl: "https://gw.example.com",
        apiKey: "sk-x",
        capabilities: ALL_CAPS
      },
      false
    );
    // Saved caps render immediately; check Grok before any re-detect.
    await waitFor(() =>
      expect(screen.getByLabelText("Apply Grok Build")).not.toBeDisabled()
    );
    fireEvent.click(screen.getByLabelText("Apply Grok Build"));
    expect(
      screen.getByRole("button", { name: /^save$/i })
    ).toBeDisabled();
  });

  it("a checked Grok Build binding requires a model before save", async () => {
    setup();
    await fillCredsAndDetect();
    // ALL_CAPS includes openaiCompatible → grok is bindable.
    fireEvent.click(screen.getByLabelText("Apply Grok Build"));
    // No model yet → save blocked (grok's api_key is stored per-model).
    expect(createBtn()).toBeDisabled();
  });

  it("save preserves a hidden tool's existing binding verbatim", async () => {
    // A gateway already bound to Codex, edited while Codex is toggled off
    // (visibleApps excludes it). Saving must carry the binding over — the
    // regression was handleSave dropping it (protocols zeroed for
    // non-visible apps), which then deactivated the live config too.
    const { onSave } = setup(
      {
        name: "GW",
        baseUrl: "https://gw.example.com",
        apiKey: "sk-x",
        capabilities: ALL_CAPS,
        bindings: [{ id: "b-codex", app: "codex", model: "gpt-5" }]
      },
      true,
      ALL_INSTALLED,
      ["claude", "claude-desktop", "gemini", "opencode"]
    );
    await waitFor(() =>
      expect(screen.getByLabelText("Apply Claude Code")).not.toBeDisabled()
    );
    fireEvent.click(createBtn());
    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved: Gateway = onSave.mock.calls[0][0];
    expect(saved.bindings).toEqual([
      { id: "b-codex", app: "codex", model: "gpt-5" }
    ]);
  });
});
