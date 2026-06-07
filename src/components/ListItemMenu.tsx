import React from "react";
import { invoke } from "@tauri-apps/api/core";
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
import { basename, resumeCommandFor } from "@/lib/session-utils";
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

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>{children}</ContextMenuTrigger>
      <ContextMenuContent className="w-52">
        <ContextMenuItem onSelect={() => void revealItemInDir(path)}>
          {t("menu.revealInFinder")}
        </ContextMenuItem>
        <ContextMenuSeparator />
        {resumeCmd && (
          <>
            <ContextMenuItem onSelect={resumeInTerminal}>
              {t("menu.resumeInTerminal")}
            </ContextMenuItem>
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
      </ContextMenuContent>
    </ContextMenu>
  );
}
