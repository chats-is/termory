import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { revealLabelKey } from "@/lib/platform";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { toast } from "sonner";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger
} from "@/components/ui/context-menu";
import { copyToClipboard } from "@/lib/clipboard";
import { basename, recordRel, resumeCommandFor } from "@/lib/session-utils";
import {
  runClaudeMigration,
  runCodexMigration,
  runRecordDelete,
  type MigrateResult
} from "@/lib/migrate";
import { useT } from "@/i18n";

/**
 * Right-click context menu for a list row (`children` becomes the
 * trigger). "Reveal in Finder" is its own group; the copy actions below
 * mirror the detail-pane CopyMenu (resume command / path / filename / ID).
 */
export function ListItemMenu({
  path,
  id,
  messageId,
  source,
  project,
  onLocalDelete,
  onLocalMigrate,
  hideSessionOps,
  sourceMissing,
  children
}: {
  path: string;
  /** Session GUID — omit for memory/skill rows. */
  id?: string;
  /** Per-message id (the favorite's own id) — adds "Copy message ID". */
  messageId?: string;
  /** Session source (Claude / Codex / …) — enables the resume command. */
  source?: string;
  /** Session project/cwd — the terminal `cd`s here before resuming. */
  project?: string;
  /** Drop this row from the list after a successful delete (local, no re-scan). */
  onLocalDelete?: () => void;
  /** Re-point this row after a successful migrate (local, no re-scan). */
  onLocalMigrate?: (res: MigrateResult) => void;
  /** Hide the session-management actions (resume-in-terminal, migrate, delete).
   *  Set on the Favorites list — those act on the SOURCE session, not the saved
   *  snapshot, so they don't belong there (delete would remove the real session). */
  hideSessionOps?: boolean;
  /** The source file/session is gone (deleted favorite). Hide the actions that
   *  need it on disk — Reveal in Finder + the resume command. */
  sourceMissing?: boolean;
  children: React.ReactNode;
}) {
  const t = useT();
  const resumeCmd = source && id ? resumeCommandFor(source, id) : null;

  const copy = (value: string) => {
    void copyToClipboard(value);
    toast.success(t("menu.copied"));
  };

  const resumeInTerminal = () => {
    void invoke("resume_session_in_terminal", { source, id, project }).catch(
      (err) => toast.error(t("menu.terminalError", { error: String(err) }))
    );
  };

  // Per-record migration: a Claude session row migrates that one session; a
  // Claude auto-memory row (a .md under ~/.claude/projects/<slug>/memory/)
  // migrates that one file. Whole-project migration lives on the sidebar
  // project row, not here.
  const isClaudeSession = source === "Claude" && !!id;
  const isClaudeAutoMemory =
    !id &&
    path.includes("/projects/") &&
    path.includes("/memory/") &&
    path.endsWith(".md");
  // Gemini: delete only (migration of Gemini is not implemented yet).
  const isGeminiSession = source === "Gemini" && !!id;
  const isGeminiAutoMemory =
    !id &&
    path.includes("/.gemini/") &&
    path.includes("/memory/") &&
    path.endsWith(".md");
  // Codex / OpenCode: delete only (sqlite rows, removed by id — see backend).
  const isCodexSession = source === "Codex" && !!id;
  const isOpencodeSession = source === "OpenCode" && !!id;
  const isCodexAutoMemory =
    !id && path.includes("/.codex/memories/") && path.endsWith(".md");
  // Codex memory rel = the path under ~/.codex/memories/ (backend bounds it).
  const codexMemoryRel = (() => {
    const marker = "/.codex/memories/";
    const i = path.indexOf(marker);
    return i < 0 ? basename(path) : path.slice(i + marker.length);
  })();

  // Migrate/delete locate every record by project + `rel` (the file's path
  // within its project dir) — uniform across Claude/Gemini, session/memory.
  // The backend rebuilds the path under the bounded project dir, so the
  // frontend never passes a raw filesystem path.
  const proj = project ?? "";
  const rel = recordRel(path);

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>{children}</ContextMenuTrigger>
      <ContextMenuContent className="w-52">
        {!sourceMissing && (
          <>
            <ContextMenuItem onSelect={() => void revealItemInDir(path)}>
              {t(revealLabelKey())}
            </ContextMenuItem>
            <ContextMenuSeparator />
          </>
        )}
        {resumeCmd && !sourceMissing && (
          <>
            {!hideSessionOps && (
              <ContextMenuItem onSelect={resumeInTerminal}>
                {t("menu.resumeInTerminal")}
              </ContextMenuItem>
            )}
            <ContextMenuItem onSelect={() => copy(resumeCmd)}>
              {t("menu.copyResumeCommand")}
            </ContextMenuItem>
          </>
        )}
        <ContextMenuItem onSelect={() => copy(path)}>
          {t("menu.copyPath")}
        </ContextMenuItem>
        <ContextMenuItem onSelect={() => copy(basename(path))}>
          {t("menu.copyFilename")}
        </ContextMenuItem>
        {id && (
          <ContextMenuItem onSelect={() => copy(id)}>
            {t("menu.copySessionId")}
          </ContextMenuItem>
        )}
        {messageId && (
          <ContextMenuItem onSelect={() => copy(messageId)}>
            {t("menu.copyMessageId")}
          </ContextMenuItem>
        )}
        {!hideSessionOps && isClaudeSession && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              onSelect={() =>
                void runClaudeMigration(
                  "migrate_claude_session",
                  { project: proj, rel },
                  t,
                  onLocalMigrate
                )
              }
            >
              {t("menu.migrateSession")}
            </ContextMenuItem>
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_claude_session",
                  { project: proj, rel },
                  id ?? basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteSession")}
            </ContextMenuItem>
          </>
        )}
        {isClaudeAutoMemory && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              onSelect={() =>
                void runClaudeMigration(
                  "migrate_claude_memory",
                  { project: proj, rel },
                  t,
                  onLocalMigrate
                )
              }
            >
              {t("menu.migrateMemory")}
            </ContextMenuItem>
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_claude_memory",
                  { project: proj, rel },
                  basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteMemory")}
            </ContextMenuItem>
          </>
        )}
        {!hideSessionOps && isGeminiSession && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_gemini_session",
                  { project: proj, rel },
                  id ?? basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteSession")}
            </ContextMenuItem>
          </>
        )}
        {isGeminiAutoMemory && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_gemini_memory",
                  { project: proj, rel },
                  basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteMemory")}
            </ContextMenuItem>
          </>
        )}
        {!hideSessionOps && isCodexSession && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              onSelect={() =>
                void runCodexMigration(
                  "migrate_codex_session",
                  { id: id! },
                  t,
                  onLocalMigrate
                )
              }
            >
              {t("menu.migrateSession")}
            </ContextMenuItem>
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_codex_session",
                  { id: id! },
                  id ?? basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteSession")}
            </ContextMenuItem>
          </>
        )}
        {!hideSessionOps && isOpencodeSession && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_opencode_session",
                  { id: id! },
                  id ?? basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteSession")}
            </ContextMenuItem>
          </>
        )}
        {isCodexAutoMemory && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem
              variant="destructive"
              onSelect={() =>
                void runRecordDelete(
                  "delete_codex_memory",
                  { rel: codexMemoryRel },
                  basename(path),
                  t,
                  onLocalDelete
                )
              }
            >
              {t("menu.deleteMemory")}
            </ContextMenuItem>
          </>
        )}
      </ContextMenuContent>
    </ContextMenu>
  );
}
