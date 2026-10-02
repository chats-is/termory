import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { getConfig, invalidateConfigCache } from "@/config";
import { CODEX_KEEP_ALL_SESSIONS_KEY } from "@/constants";
import type {
  CodexFollowTarget,
  RecentCodexProject
} from "@/components/providers/CodexFollowDialog";
import { useT } from "@/i18n";

/** Stable Codex `model_provider` ids: Termory writes "termory" for any custom
 * provider, and Official is the built-in "openai" bucket. */
export const CODEX_CUSTOM_PROVIDER_ID = "termory";
export const CODEX_OFFICIAL_PROVIDER_ID = "openai";

/**
 * The Codex "follow sessions?" prompt, shared by every surface that can
 * switch Codex between the Official and the custom bucket (the Providers
 * tab, the Gateways tab and the Router page). The caller renders
 * `<CodexFollowDialog target={followTarget} onClose=… />` itself.
 */
export function useCodexFollow() {
  const t = useT();
  const [followTarget, setFollowTarget] = React.useState<CodexFollowTarget | null>(null);

  const maybePromptThenActivate = React.useCallback(
    async (base: Omit<CodexFollowTarget, "projects">) => {
      let projects: RecentCodexProject[] = [];
      try {
        // limit 0 = no cap — return every project, newest first; the dialog
        // scrolls. So a project the user wants is never pushed out by a hard cap.
        projects = await invoke<RecentCodexProject[]>("recent_codex_projects", {
          limit: 0
        });
      } catch {
        projects = [];
      }
      // Only projects with at least one session whose provider differs from the
      // target are migration candidates.
      const candidates = projects.filter((p) =>
        p.providers.some((id) => id !== base.providerId)
      );
      if (candidates.length === 0) {
        await base.activate();
        return;
      }
      // Settings → "follow all projects silently": the user opted out of being
      // asked, so switch and re-tag everything. Read per switch (not cached in
      // state) so a change made while this page is open takes effect
      // immediately — the Rust side reads the same key per switch.
      invalidateConfigCache();
      const keepSessions =
        (await getConfig<boolean>(CODEX_KEEP_ALL_SESSIONS_KEY).catch(() => false)) === true;
      if (keepSessions) {
        const activated = await base.activate();
        if (!activated) return;
        try {
          const moved = await invoke<{ moved: number }>("follow_codex_sessions", {
            projects: candidates.map((p) => p.project),
            targetProviderId: base.providerId
          });
          toast.success(t("providers.followDone", { count: String(moved.moved) }));
        } catch (err) {
          toast.error(String(err));
        }
        return;
      }
      setFollowTarget({ ...base, projects: candidates });
    },
    [t]
  );

  /** Bridge for binding surfaces: `"toCustom"` (activate, official→custom)
   * or `"toOfficial"` (deactivate, custom→official). */
  const codexFollowForBinding = React.useCallback(
    (
      direction: "toCustom" | "toOfficial",
      label: string,
      activate: () => Promise<boolean>,
      opts?: { required?: boolean }
    ) =>
      maybePromptThenActivate({
        providerId:
          direction === "toOfficial" ? CODEX_OFFICIAL_PROVIDER_ID : CODEX_CUSTOM_PROVIDER_ID,
        label,
        activate,
        onDismiss: opts?.required ? () => void activate() : undefined
      }),
    [maybePromptThenActivate]
  );

  return { followTarget, setFollowTarget, maybePromptThenActivate, codexFollowForBinding };
}
