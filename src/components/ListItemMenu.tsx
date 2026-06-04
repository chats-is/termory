import React from "react";
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
  children
}: {
  path: string;
  /** Session GUID — omit for memory/skill rows. */
  id?: string;
  /** Per-message id (the favorite's own id) — adds "Copy message ID". */
  messageId?: string;
  /** Session source (Claude / Codex / …) — enables the resume command. */
  source?: string;
  children: React.ReactNode;
}) {
  const resumeCmd = source && id ? resumeCommandFor(source, id) : null;

  const copy = (value: string, what: string) => {
    void copyToClipboard(value);
    toast.success(`${what} copied`);
  };

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>{children}</ContextMenuTrigger>
      <ContextMenuContent className="w-52">
        <ContextMenuItem onSelect={() => void revealItemInDir(path)}>
          Reveal in Finder
        </ContextMenuItem>
        <ContextMenuSeparator />
        {resumeCmd && (
          <ContextMenuItem onSelect={() => copy(resumeCmd, "Resume command")}>
            Copy resume command
          </ContextMenuItem>
        )}
        <ContextMenuItem onSelect={() => copy(path, "Path")}>
          Copy path
        </ContextMenuItem>
        <ContextMenuItem onSelect={() => copy(basename(path), "Filename")}>
          Copy filename
        </ContextMenuItem>
        {id && (
          <ContextMenuItem onSelect={() => copy(id, "Session ID")}>
            Copy session ID
          </ContextMenuItem>
        )}
        {messageId && (
          <ContextMenuItem onSelect={() => copy(messageId, "Message ID")}>
            Copy message ID
          </ContextMenuItem>
        )}
      </ContextMenuContent>
    </ContextMenu>
  );
}
