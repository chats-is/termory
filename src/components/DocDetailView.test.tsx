import React from "react";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { DocDetailView } from "./DocDetailView";

// jsdom has no layout, so Element.scrollIntoView is undefined; the
// occurrence navigation calls it on the current <mark>.
Element.prototype.scrollIntoView ??= function scrollIntoView() {};

const baseProps = () => ({
  text: "alpha beta alpha gamma alpha",
  findOpen: true,
  findQuery: "alpha",
  onQueryChange: vi.fn(),
  onClose: vi.fn(),
  focusNonce: 0
});

function currentMarks(container: HTMLElement) {
  return container.querySelectorAll("mark[data-current]");
}

describe("DocDetailView", () => {
  it("renders the doc without a find bar when findOpen is false", () => {
    const { container } = render(
      <DocDetailView {...baseProps()} findOpen={false} />
    );
    expect(screen.queryByLabelText("Find in session")).toBeNull();
    expect(container.querySelectorAll("mark")).toHaveLength(0);
    expect(container.textContent).toContain("alpha beta alpha gamma alpha");
  });

  it("highlights every occurrence and counts them in the find bar", () => {
    const { container } = render(<DocDetailView {...baseProps()} />);
    expect(container.querySelectorAll("mark")).toHaveLength(3);
    expect(screen.getByText("1/3")).toBeInTheDocument();
    // The first occurrence starts as the current one.
    const current = currentMarks(container);
    expect(current).toHaveLength(1);
    expect(current[0]).toBe(container.querySelectorAll("mark")[0]);
  });

  it("next/prev move data-current across occurrences and wrap around", () => {
    const { container } = render(<DocDetailView {...baseProps()} />);
    const marks = () => container.querySelectorAll("mark");
    fireEvent.click(screen.getByRole("button", { name: "Next match" }));
    expect(screen.getByText("2/3")).toBeInTheDocument();
    expect(currentMarks(container)[0]).toBe(marks()[1]);
    fireEvent.click(screen.getByRole("button", { name: "Next match" }));
    fireEvent.click(screen.getByRole("button", { name: "Next match" }));
    // 3 → wraps back to 1.
    expect(screen.getByText("1/3")).toBeInTheDocument();
    expect(currentMarks(container)[0]).toBe(marks()[0]);
    fireEvent.click(screen.getByRole("button", { name: "Previous match" }));
    expect(screen.getByText("3/3")).toBeInTheDocument();
    expect(currentMarks(container)[0]).toBe(marks()[2]);
  });

  it("shows 'No matches' and no marks for a query that isn't in the doc", () => {
    const { container } = render(
      <DocDetailView {...baseProps()} findQuery="zzz" />
    );
    expect(container.querySelectorAll("mark")).toHaveLength(0);
    expect(screen.getByText("No matches")).toBeInTheDocument();
  });

  it("Enter in the input advances the occurrence", () => {
    render(<DocDetailView {...baseProps()} />);
    fireEvent.keyDown(screen.getByLabelText("Find in session"), {
      key: "Enter"
    });
    expect(screen.getByText("2/3")).toBeInTheDocument();
  });
});
