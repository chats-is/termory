import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ListItemMenu } from "./ListItemMenu";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ revealItemInDir: vi.fn() }));
vi.mock("@/lib/clipboard", () => ({ copyToClipboard: vi.fn() }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

describe("ListItemMenu — Resume in terminal", () => {
  beforeEach(() => invokeMock.mockReset());

  it("invokes resume_session_in_terminal with the row's source/id/project", async () => {
    invokeMock.mockResolvedValue(undefined);
    render(
      <ListItemMenu path="/p/s.jsonl" id="sess-1" source="Claude" project="/proj">
        <button>row</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("row"));
    await userEvent.click(await screen.findByText("Resume in terminal"));
    expect(invokeMock).toHaveBeenCalledWith("resume_session_in_terminal", {
      source: "Claude",
      id: "sess-1",
      project: "/proj"
    });
  });

  it("is hidden for non-session rows (no source/id → no resume command)", async () => {
    render(
      <ListItemMenu path="/p/CLAUDE.md">
        <button>memrow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("memrow"));
    // The menu opened (Reveal in Finder is always present) but the resume
    // entry is absent for a memory/skill row.
    expect(await screen.findByText("Reveal in Finder")).toBeTruthy();
    expect(screen.queryByText("Resume in terminal")).toBeNull();
  });
});
