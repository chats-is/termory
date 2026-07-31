import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { CodexFollowDialog, type CodexFollowTarget } from "./CodexFollowDialog";

// The dialog calls invoke() (follow_codex_sessions) + toast on the keep path;
// stub both so it renders without a Tauri host.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

function makeTarget(): CodexFollowTarget {
  return {
    providerId: "termory",
    label: "My Gateway",
    activate: vi.fn().mockResolvedValue(true),
    projects: [
      {
        project: "/Users/me/proj-a",
        updated_at: 0,
        session_count: 3,
        providers: ["openai"]
      }
    ]
  };
}

describe("CodexFollowDialog — keep requires a selection", () => {
  it("disables 'Activate & keep' until a project is checked (Skip stays enabled)", () => {
    render(<CodexFollowDialog target={makeTarget()} onClose={vi.fn()} />);
    const keep = screen.getByRole("button", { name: "Activate & keep" });
    // Nothing checked → keep is disabled (it would otherwise be identical to
    // "Activate only" — switch the provider without migrating sessions).
    expect(keep).toBeDisabled();
    expect(screen.getByRole("button", { name: "Activate only" })).toBeEnabled();
    // Check the project → keep enables.
    fireEvent.click(screen.getByRole("button", { name: /proj-a/i }));
    expect(keep).toBeEnabled();
    // Uncheck → disabled again.
    fireEvent.click(screen.getByRole("button", { name: /proj-a/i }));
    expect(keep).toBeDisabled();
  });
});
