import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { blankProvider } from "@/lib/provider-utils";
import { ProviderEditor } from "./ProviderEditor";

// ProviderEditor calls invoke() on save (fetch_provider_favicon) and on the
// "fetch models" button. Stub it so saving resolves without a Tauri host.
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(null);
});

function openAdvanced() {
  fireEvent.click(screen.getByRole("button", { name: /advanced settings/i }));
}

const saveBtn = () => screen.getByRole("button", { name: /^create$/i });

describe("ProviderEditor — Advanced settings validation", () => {
  it("grok requires a models LIST before save (multi-model, flat picker list)", () => {
    render(
      <ProviderEditor
        provider={blankProvider("grok")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "GW" }
    });
    // Name + Base URL (seeded by blankProvider) present, models list empty →
    // save must stay disabled (grok's `models` is required; the primary
    // `model` is only the OPTIONAL default).
    expect(saveBtn()).toBeDisabled();
    // Fill the first model-list row's id → the list is now non-empty.
    fireEvent.change(screen.getAllByLabelText("Model ID")[0], {
      target: { value: "grok-4.5" }
    });
    expect(saveBtn()).not.toBeDisabled();
  });

  it("blocks save on duplicate option keys", () => {
    render(
      <ProviderEditor
        provider={blankProvider("codex")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "C" }
    });
    openAdvanced();
    fireEvent.change(screen.getAllByLabelText("Override key")[0], {
      target: { value: "foo" }
    });
    fireEvent.click(screen.getByRole("button", { name: /^add$/i }));
    fireEvent.change(screen.getAllByLabelText("Override key")[1], {
      target: { value: "foo" }
    });

    expect(screen.getByText(/duplicate key/i)).toBeInTheDocument();
    expect(saveBtn()).toBeDisabled();
  });

  it("blocks save on a managed option key", () => {
    render(
      <ProviderEditor
        provider={blankProvider("codex")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "C" }
    });
    openAdvanced();
    fireEvent.change(screen.getAllByLabelText("Override key")[0], {
      target: { value: "model" } // managed for Codex
    });

    expect(screen.getByText(/already managed/i)).toBeInTheDocument();
    expect(saveBtn()).toBeDisabled();
  });
});

describe("ProviderEditor — save normalization", () => {
  it("trims option values and drops blank rows", async () => {
    const onSave = vi.fn();
    render(
      <ProviderEditor
        provider={blankProvider("codex")}
        isNew
        onSave={onSave}
        onClose={vi.fn()}
      />
    );
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "My Codex" }
    });
    openAdvanced();
    fireEvent.change(screen.getAllByLabelText("Override key")[0], {
      target: { value: "approval_policy" }
    });
    fireEvent.change(screen.getAllByLabelText("Override value")[0], {
      target: { value: "  on-request  " }
    });
    fireEvent.click(saveBtn());

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved = onSave.mock.calls[0][0];
    expect(saved.name).toBe("My Codex");
    expect(saved.options).toEqual([
      { key: "approval_policy", value: "on-request" }
    ]);
  });

  it("saves OpenCode extra models as {id,name} (trimmed)", async () => {
    const onSave = vi.fn();
    render(
      <ProviderEditor
        provider={blankProvider("opencode")}
        isNew
        onSave={onSave}
        onClose={vi.fn()}
      />
    );
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "OC" }
    });
    fireEvent.change(screen.getByLabelText("Model *"), {
      target: { value: "gpt-5" }
    });
    fireEvent.change(screen.getByLabelText("Model ID"), {
      target: { value: "  gpt-5-mini  " }
    });
    fireEvent.change(screen.getByLabelText("Model display name"), {
      target: { value: "  Mini  " }
    });
    fireEvent.click(saveBtn());

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved = onSave.mock.calls[0][0];
    expect(saved.models).toEqual([{ id: "gpt-5-mini", name: "Mini" }]);
  });
});

describe("ProviderEditor — Claude routing template", () => {
  it("seeds three protected (read-only, non-deletable) routing rows", () => {
    render(
      <ProviderEditor
        provider={blankProvider("claude")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    const keys = screen.getAllByLabelText("Override key") as HTMLInputElement[];
    expect(keys).toHaveLength(3);
    expect(keys.map((k) => k.value)).toEqual([
      "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
      "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
      "env.ANTHROPIC_DEFAULT_HAIKU_MODEL"
    ]);
    keys.forEach((k) => expect(k.readOnly).toBe(true));
    // Protected rows render no delete button.
    expect(screen.queryAllByLabelText("Remove override")).toHaveLength(0);
  });
});

describe("ProviderEditor — Claude Desktop (direct gateway profile)", () => {
  it("shows the models list + Advanced settings, hides the single primary Model field", () => {
    render(
      <ProviderEditor
        provider={blankProvider("claude-desktop")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    // No single primary Model combobox (Claude Desktop has no primary model)…
    expect(screen.queryByLabelText(/^Model \*?$/)).not.toBeInTheDocument();
    // …but the models list (→ inferenceModels) and the generic options
    // escape hatch (→ merged into the 3P profile JSON) are both available.
    expect(screen.getByLabelText("Model ID")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /advanced settings/i })
    ).toBeInTheDocument();
  });

  it("treats the API key as optional (savable blank, filled in later)", () => {
    render(
      <ProviderEditor
        provider={blankProvider("claude-desktop")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    // Base URL is pre-seeded; only Name is required — NOT the API key.
    expect(saveBtn()).toBeDisabled(); // name still blank
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "MyGateway" }
    });
    // No API key entered, yet save enables (the key can be filled in later).
    expect(saveBtn()).toBeEnabled();
  });

  it("blocks save when a model id isn't a Claude model name", () => {
    render(
      <ProviderEditor
        provider={blankProvider("claude-desktop")}
        isNew
        onSave={vi.fn()}
        onClose={vi.fn()}
      />
    );
    fireEvent.change(screen.getByLabelText("Name *"), {
      target: { value: "G" }
    });
    expect(saveBtn()).toBeEnabled(); // blank model row is fine (base URL pre-seeded)
    // A non-Claude model id blocks the save (Claude Desktop would reject it).
    fireEvent.change(screen.getByLabelText("Model ID"), {
      target: { value: "gpt-4" }
    });
    expect(saveBtn()).toBeDisabled();
    // Fix it → save re-enables.
    fireEvent.change(screen.getByLabelText("Model ID"), {
      target: { value: "claude-sonnet-4-6" }
    });
    expect(saveBtn()).toBeEnabled();
  });
});
