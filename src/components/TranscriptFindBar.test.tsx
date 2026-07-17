import React from "react";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { TranscriptFindBar } from "./TranscriptFindBar";

const baseProps = () => ({
  query: "",
  onQueryChange: vi.fn(),
  position: 0,
  total: 0,
  onNext: vi.fn(),
  onPrev: vi.fn(),
  onClose: vi.fn(),
  focusNonce: 0
});

describe("TranscriptFindBar", () => {
  it("focuses the input on mount (focusNonce effect)", () => {
    render(<TranscriptFindBar {...baseProps()} />);
    expect(document.activeElement).toBe(
      screen.getByLabelText("Find in session")
    );
  });

  it("re-focuses and selects the input when focusNonce changes", () => {
    const props = baseProps();
    const { rerender } = render(
      <TranscriptFindBar {...props} query="todo" />
    );
    const input = screen.getByLabelText("Find in session") as HTMLInputElement;
    input.blur();
    expect(document.activeElement).not.toBe(input);
    rerender(<TranscriptFindBar {...props} query="todo" focusNonce={1} />);
    expect(document.activeElement).toBe(input);
    // The previous query comes back selected, browser-find style.
    expect(input.selectionStart).toBe(0);
    expect(input.selectionEnd).toBe(4);
  });

  it("reports typed text through onQueryChange", () => {
    const props = baseProps();
    render(<TranscriptFindBar {...props} />);
    fireEvent.change(screen.getByLabelText("Find in session"), {
      target: { value: "abc" }
    });
    expect(props.onQueryChange).toHaveBeenCalledWith("abc");
  });

  it("shows the position counter when there are matches", () => {
    render(
      <TranscriptFindBar {...baseProps()} query="x" position={2} total={17} />
    );
    expect(screen.getByText("3/17")).toBeInTheDocument();
  });

  it("shows 'No matches' when a non-empty query has zero matches", () => {
    render(<TranscriptFindBar {...baseProps()} query="zzz" total={0} />);
    expect(screen.getByText("No matches")).toBeInTheDocument();
  });

  it("shows neither counter nor 'No matches' while the query is blank", () => {
    render(<TranscriptFindBar {...baseProps()} query="  " total={0} />);
    expect(screen.queryByText("No matches")).toBeNull();
  });

  it("Enter → onNext, Shift+Enter → onPrev, Escape → onClose", () => {
    const props = baseProps();
    render(<TranscriptFindBar {...props} query="x" total={3} />);
    const input = screen.getByLabelText("Find in session");
    fireEvent.keyDown(input, { key: "Enter" });
    expect(props.onNext).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
    expect(props.onPrev).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(input, { key: "Escape" });
    expect(props.onClose).toHaveBeenCalledTimes(1);
  });

  it("navigation buttons call onPrev/onNext and close calls onClose", () => {
    const props = baseProps();
    render(<TranscriptFindBar {...props} query="x" position={1} total={3} />);
    fireEvent.click(screen.getByRole("button", { name: "Previous match" }));
    expect(props.onPrev).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Next match" }));
    expect(props.onNext).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Close find" }));
    expect(props.onClose).toHaveBeenCalledTimes(1);
  });

  it("disables prev/next (but not close) when there are no matches", () => {
    render(<TranscriptFindBar {...baseProps()} query="zzz" total={0} />);
    expect(
      screen.getByRole("button", { name: "Previous match" })
    ).toBeDisabled();
    expect(screen.getByRole("button", { name: "Next match" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Close find" })).toBeEnabled();
  });
});
