import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ListItemMenu } from "./ListItemMenu";

const invokeMock = vi.fn();
const openMock = vi.fn();
const askMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ revealItemInDir: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openMock(...args),
  ask: (...args: unknown[]) => askMock(...args)
}));
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

describe("ListItemMenu — Migrate (single session / memory)", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    openMock.mockReset();
    askMock.mockReset();
  });

  it("migrates a single Claude session (sessionPath, copy, keep old)", async () => {
    openMock.mockResolvedValue("/new/proj");
    askMock.mockResolvedValue(true);
    invokeMock.mockResolvedValue({ sessions: 1, memory_files: 0 });
    render(
      <ListItemMenu
        path="/u/.claude/projects/-old/s1.jsonl"
        id="s1"
        source="Claude"
        project="/old/proj"
      >
        <button>row</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("row"));
    await userEvent.click(
      await screen.findByText("Migrate session…")
    );
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("migrate_claude_session", {
        project: "/old/proj",
        rel: "s1.jsonl",
        newPath: "/new/proj",
        deleteOld: true
      })
    );
  });

  it("migrates a single Claude auto-memory by project + rel", async () => {
    openMock.mockResolvedValue("/new/proj");
    askMock.mockResolvedValue(true);
    invokeMock.mockResolvedValue({ sessions: 0, memory_files: 1 });
    render(
      <ListItemMenu
        path="/u/.claude/projects/-old/memory/sub/NOTE.md"
        project="/Users/me/old"
      >
        <button>memrow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("memrow"));
    await userEvent.click(
      await screen.findByText("Migrate memory…")
    );
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("migrate_claude_memory", {
        project: "/Users/me/old",
        rel: "memory/sub/NOTE.md",
        newPath: "/new/proj",
        deleteOld: true
      })
    );
  });

  it("does not migrate when the folder pick is cancelled", async () => {
    openMock.mockResolvedValue(null); // cancelled
    render(
      <ListItemMenu path="/p/s.jsonl" id="s1" source="Claude" project="/old/proj">
        <button>row2</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("row2"));
    await userEvent.click(
      await screen.findByText("Migrate session…")
    );
    await waitFor(() => expect(openMock).toHaveBeenCalled());
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("offers no migrate item for non-Claude sessions or plain memory files", async () => {
    const { unmount } = render(
      <ListItemMenu path="/p/s.jsonl" id="s1" source="Codex" project="/old">
        <button>codexrow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("codexrow"));
    expect(await screen.findByText("Reveal in Finder")).toBeTruthy();
    expect(screen.queryByText("Migrate session…")).toBeNull();
    unmount();

    // A project-folder CLAUDE.md (not under projects/<slug>/memory/) → no item.
    render(
      <ListItemMenu path="/work/repo/CLAUDE.md">
        <button>plainmem</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("plainmem"));
    expect(await screen.findByText("Reveal in Finder")).toBeTruthy();
    expect(screen.queryByText("Migrate memory…")).toBeNull();
  });
});

describe("ListItemMenu — Delete (single session / memory)", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    askMock.mockReset();
  });

  it("deletes a single Claude session after confirmation", async () => {
    askMock.mockResolvedValue(true);
    invokeMock.mockResolvedValue(undefined);
    render(
      <ListItemMenu
        path="/u/.claude/projects/-p/s1.jsonl"
        id="s1"
        source="Claude"
        project="/proj"
      >
        <button>row</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("row"));
    await userEvent.click(await screen.findByText("Delete session…"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_claude_session", {
        project: "/proj",
        rel: "s1.jsonl"
      })
    );
  });

  it("deletes a single Claude auto-memory by project + rel", async () => {
    askMock.mockResolvedValue(true);
    invokeMock.mockResolvedValue(undefined);
    render(
      <ListItemMenu
        path="/u/.claude/projects/-old/memory/NOTE.md"
        project="/Users/me/old"
      >
        <button>memrow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("memrow"));
    await userEvent.click(await screen.findByText("Delete memory…"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_claude_memory", {
        project: "/Users/me/old",
        rel: "memory/NOTE.md"
      })
    );
  });

  it("routes a Gemini session to delete_gemini_session (project + rel)", async () => {
    askMock.mockResolvedValue(true);
    invokeMock.mockResolvedValue(undefined);
    render(
      <ListItemMenu
        path="/u/.gemini/tmp/h/chats/session-x.json"
        id="g1"
        source="Gemini"
        project="/Users/me/g"
      >
        <button>grow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("grow"));
    await userEvent.click(await screen.findByText("Delete session…"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_gemini_session", {
        project: "/Users/me/g",
        rel: "chats/session-x.json"
      })
    );
  });

  it("routes a Gemini auto-memory to delete_gemini_memory (project + rel)", async () => {
    askMock.mockResolvedValue(true);
    invokeMock.mockResolvedValue(undefined);
    render(
      <ListItemMenu
        path="/u/.gemini/tmp/h/memory/NOTE.md"
        project="/Users/me/g"
      >
        <button>gmem</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("gmem"));
    await userEvent.click(await screen.findByText("Delete memory…"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_gemini_memory", {
        project: "/Users/me/g",
        rel: "memory/NOTE.md"
      })
    );
  });

  it("does not delete when the confirmation is declined", async () => {
    askMock.mockResolvedValue(false);
    render(
      <ListItemMenu path="/p/s.jsonl" id="s1" source="Claude" project="/proj">
        <button>row2</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("row2"));
    await userEvent.click(await screen.findByText("Delete session…"));
    await waitFor(() => expect(askMock).toHaveBeenCalled());
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("ListItemMenu — hideSessionOps (Favorites)", () => {
  it("hides resume-in-terminal / migrate / delete, keeps the copy actions", async () => {
    render(
      <ListItemMenu
        path="/p/s.jsonl"
        id="sess-1"
        source="Claude"
        project="/proj"
        hideSessionOps
      >
        <button>favrow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("favrow"));
    await screen.findByText("Copy resume command"); // menu is open
    expect(screen.queryByText("Resume in terminal")).toBeNull();
    expect(screen.queryByText("Migrate session…")).toBeNull();
    expect(screen.queryByText("Delete session…")).toBeNull();
    // Copy / reveal stay available.
    expect(screen.getByText("Copy path")).toBeTruthy();
    expect(screen.getByText("Copy resume command")).toBeTruthy();
  });
});

describe("ListItemMenu — sourceMissing (deleted favorite)", () => {
  it("hides Reveal in Finder and Copy resume command when the source is gone", async () => {
    render(
      <ListItemMenu
        path="/p/s.jsonl"
        id="sess-1"
        messageId="fav-1"
        source="Claude"
        project="/proj"
        hideSessionOps
        sourceMissing
      >
        <button>delrow</button>
      </ListItemMenu>
    );
    fireEvent.contextMenu(screen.getByText("delrow"));
    await screen.findByText("Copy path"); // menu is open
    expect(screen.queryByText("Reveal in Finder")).toBeNull();
    expect(screen.queryByText("Copy resume command")).toBeNull();
    expect(screen.queryByText("Resume in terminal")).toBeNull();
    // Snapshot copy actions stay.
    expect(screen.getByText("Copy message ID")).toBeTruthy();
  });
});
