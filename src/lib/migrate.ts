import { invoke } from "@tauri-apps/api/core";
import { open, ask } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import type { MessageKey } from "@/i18n";

type Translate = (
  key: MessageKey,
  params?: Record<string, string | number>
) => string;

/** Backend `ClaudeMigrationResult` — counts + the source/destination dirs the
 *  frontend uses to re-point moved records in memory (no re-scan). */
export type MigrateResult = {
  sessions: number;
  memory_files: number;
  old_dir: string;
  new_dir: string;
  new_project: string;
};

type MigrateCommand =
  | "migrate_claude_project"
  | "migrate_claude_session"
  | "migrate_claude_memory";

/**
 * Shared flow for the three Claude migration entry points (whole project /
 * single session / single memory): pick the renamed folder, confirm, then
 * invoke the matching backend command. MOVE semantics (`deleteOld: true`):
 * the backend removes the old location only after a complete copy, so a
 * session's UUID lives in exactly one project dir (no dedup-hidden leftovers).
 * `args` carries the command's own source key (`oldPath` / `sessionPath` /
 * `memoryPath`). On success `onMigrated(res)` lets the caller re-point the
 * moved records locally (instead of a full re-scan).
 */
export async function runClaudeMigration(
  command: MigrateCommand,
  args: Record<string, string>,
  t: Translate,
  onMigrated?: (res: MigrateResult) => void
): Promise<void> {
  const picked = await open({
    directory: true,
    title: t("menu.migratePickTitle")
  });
  if (typeof picked !== "string") return; // cancelled

  // `claude --resume` (command line) lists a project's sessions only once the
  // project is registered in ~/.claude.json — which Claude does when you OPEN it
  // (run `claude` in the dir), not from the migrated files alone. So for a
  // project never opened in Claude, the moved sessions won't show via the
  // `--resume` flag until you open Claude there once (afterwards `--resume` works
  // too); `/resume` inside a running Claude shows them regardless. Termory never
  // writes ~/.claude.json, so we just surface this in the confirm when the target
  // isn't registered yet. Memory-only moves don't affect resume → skip the check.
  let registered = true;
  if (command !== "migrate_claude_memory") {
    try {
      registered = await invoke<boolean>("claude_project_registered", {
        path: picked
      });
    } catch {
      registered = true; // probe failed → don't block the migration
    }
  }
  const baseMsg = registered
    ? t("menu.migrateConfirm", { to: picked })
    : t("menu.migrateConfirmUnregistered", { to: picked });
  const confirmed = await ask(`${baseMsg}\n\n${t("menu.exitCliHint", { app: "Claude Code" })}`, {
    title: t("menu.migrate"),
    kind: "warning",
    okLabel: t("menu.migrate"),
    cancelLabel: t("common.cancel")
  });
  if (!confirmed) return;
  try {
    const res = await invoke<MigrateResult>(command, {
      ...args,
      newPath: picked,
      deleteOld: true
    });
    toast.success(
      t("menu.migrateSuccess", {
        sessions: res.sessions,
        memory: res.memory_files
      })
    );
    onMigrated?.(res);
  } catch (err) {
    toast.error(t("menu.migrateError", { error: String(err) }));
  }
}

/**
 * Codex migration (session or whole project). Unlike Claude, Codex has no
 * project folder and the rollout transcripts never move — migrating is a pure
 * metadata rewrite (the rollout file's payload.cwd + the threads row), so
 * `codex resume` regroups the sessions under the new folder. There's no
 * old_dir/new_dir relocation, so the result is adapted to the shared
 * `MigrateResult` shape with empty dirs (the record's path stays put; only its
 * `project` is re-pointed in `remapRecordsLocally`).
 */
export async function runCodexMigration(
  command: "migrate_codex_session" | "migrate_codex_project",
  args: Record<string, string>,
  t: Translate,
  onMigrated?: (res: MigrateResult) => void
): Promise<void> {
  const picked = await open({
    directory: true,
    title: t("menu.migratePickTitle")
  });
  if (typeof picked !== "string") return; // cancelled
  const confirmed = await ask(
    `${t("menu.migrateConfirmCodex", { to: picked })}\n\n${t("menu.exitCliHint", { app: "Codex" })}`,
    {
      title: t("menu.migrate"),
      kind: "warning",
      okLabel: t("menu.migrate"),
      cancelLabel: t("common.cancel")
    }
  );
  if (!confirmed) return;
  try {
    const res = await invoke<{ sessions: number; new_project: string }>(command, {
      ...args,
      newPath: picked
    });
    toast.success(t("menu.migrateSuccessCodex", { sessions: res.sessions }));
    onMigrated?.({
      sessions: res.sessions,
      memory_files: 0,
      old_dir: "",
      new_dir: "",
      new_project: res.new_project
    });
  } catch (err) {
    toast.error(t("menu.migrateError", { error: String(err) }));
  }
}

type DeleteCommand =
  | "delete_claude_project"
  | "delete_claude_session"
  | "delete_claude_memory"
  | "delete_gemini_project"
  | "delete_gemini_session"
  | "delete_gemini_memory"
  | "delete_codex_session"
  | "delete_codex_project"
  | "delete_codex_memory"
  | "delete_opencode_session"
  | "delete_opencode_project";

/**
 * Shared flow for the delete entry points (whole project / single session /
 * single memory, Claude or Gemini): confirm (destructive, no undo), then
 * invoke the matching backend command. `name` is shown in the confirm + the
 * success toast so the user sees exactly what was removed. The backend guards
 * every path to stay inside the CLI's data dir. On success `onDeleted()` lets
 * the caller drop the removed records from the list locally.
 */
export async function runRecordDelete(
  command: DeleteCommand,
  args: Record<string, string>,
  name: string,
  t: Translate,
  onDeleted?: () => void
): Promise<void> {
  const confirmKey: MessageKey = command.endsWith("_project")
    ? "menu.deleteProjectConfirm"
    : command.endsWith("_session")
      ? "menu.deleteSessionConfirm"
      : "menu.deleteMemoryConfirm";
  // Name the CLI that actually owns this record so the hint matches the action.
  const app = command.startsWith("delete_gemini")
    ? "Gemini"
    : command.startsWith("delete_codex")
      ? "Codex"
      : command.startsWith("delete_opencode")
        ? "OpenCode"
        : "Claude Code";
  const confirmed = await ask(
    `${t(confirmKey, { name })}\n\n${t("menu.exitCliHint", { app })}`,
    {
      title: t("menu.delete"),
      kind: "warning",
      okLabel: t("menu.delete"),
      cancelLabel: t("common.cancel")
    }
  );
  if (!confirmed) return;
  try {
    await invoke(command, args);
    toast.success(t("menu.deleteSuccess", { name }));
    onDeleted?.();
  } catch (err) {
    toast.error(t("menu.deleteError", { error: String(err) }));
  }
}
