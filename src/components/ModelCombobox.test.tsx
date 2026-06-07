import React from "react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ModelCombobox } from "./ModelCombobox";

// cmdk uses ResizeObserver internally and calls scrollIntoView on the active
// item; jsdom ships neither. Polyfill both so the inline combobox can mount /
// navigate. (Local to this file — the shared setup doesn't provide them.)
beforeAll(() => {
  if (!("ResizeObserver" in globalThis)) {
    globalThis.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof ResizeObserver;
  }
  if (!HTMLElement.prototype.scrollIntoView) {
    HTMLElement.prototype.scrollIntoView = () => {};
  }
});

// ModelCombobox takes its suggestion list via the `options` prop (the caller
// fetches `fetch_provider_models` and passes the result in) — no Tauri IPC of
// its own, so nothing to mock here. It has no Tooltip either, so no
// TooltipProvider wrapper is needed. i18n falls back to English with no
// provider, so the default placeholder is the English string.

// A tiny controlled harness: the combobox is controlled, so without re-rendering
// on `onValueChange` a typed value never appears in the input. This mirrors how
// the editors use it (state up, value down) and lets us assert the committed value.
function Harness({
  initial = "",
  options = [],
  loading,
  onValueChange
}: {
  initial?: string;
  options?: string[];
  loading?: boolean;
  onValueChange?: (v: string) => void;
}) {
  const [value, setValue] = React.useState(initial);
  return (
    <ModelCombobox
      value={value}
      onValueChange={(v) => {
        setValue(v);
        onValueChange?.(v);
      }}
      options={options}
      loading={loading}
      ariaLabel="Model *"
    />
  );
}

describe("ModelCombobox", () => {
  it("is reachable by its aria-label (cmdk does not forward id)", () => {
    render(<Harness />);
    // The documented gotcha: cmdk's CommandInput drops `id`, so the field is
    // only findable via aria-label — getByLabelText must resolve it.
    const input = screen.getByLabelText("Model *");
    expect(input.tagName).toBe("INPUT");
    expect(input).toHaveAttribute(
      "placeholder",
      "Select or type a model id"
    );
  });

  it("calls onValueChange with the free-typed text", async () => {
    const user = userEvent.setup();
    const onValueChange = vi.fn();
    render(<Harness onValueChange={onValueChange} />);
    const input = screen.getByLabelText("Model *");
    await user.type(input, "gpt-5");
    // Last call carries the fully-typed value; the field reflects it (controlled).
    expect(onValueChange).toHaveBeenLastCalledWith("gpt-5");
    expect(input).toHaveValue("gpt-5");
  });

  it("renders fetched suggestions once the input is focused", async () => {
    const user = userEvent.setup();
    render(<Harness options={["gpt-5", "gpt-5-mini"]} />);
    const input = screen.getByLabelText("Model *");
    // The dropdown only mounts on focus (open state).
    expect(screen.queryByRole("option")).toBeNull();
    await user.click(input);
    const options = screen.getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["gpt-5", "gpt-5-mini"]);
  });

  it("selecting a suggestion calls onValueChange with that id and closes the list", async () => {
    const user = userEvent.setup();
    const onValueChange = vi.fn();
    render(
      <Harness options={["gpt-5", "gpt-5-mini"]} onValueChange={onValueChange} />
    );
    const input = screen.getByLabelText("Model *");
    await user.click(input);
    await user.click(screen.getByRole("option", { name: "gpt-5-mini" }));
    expect(onValueChange).toHaveBeenLastCalledWith("gpt-5-mini");
    expect(input).toHaveValue("gpt-5-mini");
    // onSelect sets open=false → the list unmounts.
    expect(screen.queryByRole("option")).toBeNull();
  });

  it("dedupes duplicate ids in the options list", async () => {
    const user = userEvent.setup();
    render(<Harness options={["gpt-5", "gpt-5", "gpt-5-mini"]} />);
    await user.click(screen.getByLabelText("Model *"));
    const options = screen.getAllByRole("option");
    expect(options).toHaveLength(2);
    expect(options.map((o) => o.textContent)).toEqual(["gpt-5", "gpt-5-mini"]);
  });

  it("filters suggestions by substring as the user types", async () => {
    const user = userEvent.setup();
    render(<Harness options={["gpt-5", "gpt-5-mini", "claude-opus"]} />);
    const input = screen.getByLabelText("Model *");
    await user.click(input);
    await user.type(input, "mini");
    const options = screen.getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["gpt-5-mini"]);
  });

  it("shows the loading empty-state copy while fetching with no matches", async () => {
    const user = userEvent.setup();
    render(<Harness options={[]} loading />);
    const input = screen.getByLabelText("Model *");
    await user.click(input);
    expect(screen.getByText("Fetching models…")).toBeInTheDocument();
  });
});
