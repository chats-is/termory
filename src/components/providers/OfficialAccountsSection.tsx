import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask } from "@tauri-apps/plugin-dialog";
import { Circle, CircleCheck, Loader2, Trash2, UserPlus } from "lucide-react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { QUOTA_CHANGED_EVENT } from "@/constants";
import { useT } from "@/i18n";
import type { AccountsState, CliApp, CurrentAccount, SavedAccount } from "@/types";

/** Saved official-account list for one CLI (phase 1: Codex), rendered as
 * a self-contained section directly under the Official card. Lists the
 * saved logins as plain rows, lets the user snapshot the current login,
 * switch between them, and delete. */
export function OfficialAccountsSection({
  app,
  onSwitched
}: {
  app: CliApp;
  onSwitched?: () => void;
}) {
  const t = useT();
  const [state, setState] = React.useState<AccountsState | null>(null);
  const [busy, setBusy] = React.useState<string | null>(null);
  const [loggingIn, setLoggingIn] = React.useState(false);

  const reload = React.useCallback(async () => {
    try {
      setState(await invoke<AccountsState>("list_accounts", { app }));
    } catch (err) {
      toast.error(String(err));
    }
  }, [app]);

  React.useEffect(() => {
    void reload();
  }, [reload]);

  React.useEffect(() => {
    const cleanups: Array<() => void> = [];
    let live = true;
    const track = (p: Promise<() => void>) =>
      void p.then((un) => (live ? cleanups.push(un) : un()));
    track(
      listen<{ app?: string }>(QUOTA_CHANGED_EVENT, (e) => {
        if (e.payload?.app === app) void reload();
      })
    );
    track(
      getCurrentWindow().onFocusChanged(({ payload: focused }) => {
        if (focused) void reload();
      })
    );
    return () => {
      live = false;
      cleanups.forEach((fn) => fn());
    };
  }, [app, reload]);

  const current: CurrentAccount | null = state?.current ?? null;
  const accounts = state?.accounts ?? [];

  const saveCurrent = async () => {
    try {
      await invoke("save_account", { app });
      toast.success(t("toast.accountSaved"));
      await reload();
    } catch (err) {
      toast.error(String(err));
    }
  };

  const loginAndSave = async () => {
    setLoggingIn(true);
    try {
      await invoke<string>("login_and_save_codex_account");
      toast.success(t("toast.accountAdded"));
      await reload();
      onSwitched?.();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setLoggingIn(false);
    }
  };

  const switchTo = async (account: SavedAccount) => {
    if (account.active) return;
    const warn =
      current && !current.saved
        ? `\n\n${t("providers.accountSwitchWarnUnsaved")}`
        : "";
    const ok = await ask(
      `${t("providers.accountSwitchConfirm", { name: account.name })}${warn}`,
      { title: t("providers.accountSwitchTitle"), kind: "warning" }
    );
    if (!ok) return;
    setBusy(account.id);
    try {
      await invoke("switch_account", { id: account.id });
      await invoke("mark_account_relogin", { id: account.id, needed: false });
      toast.success(t("toast.accountSwitched", { name: account.name }));
      await reload();
      onSwitched?.();
    } catch (err) {
      await invoke("mark_account_relogin", { id: account.id, needed: true }).catch(() => {});
      toast.warning(t("toast.accountTokenExpired"));
      await reload();
    } finally {
      setBusy(null);
    }
  };

  const remove = async (account: SavedAccount) => {
    const ok = await ask(
      t("providers.accountDeleteConfirm", { name: account.name }),
      { title: t("providers.accountDeleteTitle"), kind: "warning" }
    );
    if (!ok) return;
    setBusy(account.id);
    try {
      await invoke("delete_account", { id: account.id });
      toast.success(t("toast.accountDeleted"));
      await reload();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(null);
    }
  };

  if (!state) return null;

  type Row = {
    key: string;
    name: string;
    email?: string | null;
    plan?: string | null;
    active: boolean;
    needsRelogin?: boolean;
    account: SavedAccount | null;
  };
  const rows: Row[] = [];
  if (current && !current.saved) {
    rows.push({
      key: "__current__",
      name: current.name ?? current.email ?? "Codex",
      email: current.email,
      plan: current.plan,
      active: true,
      account: null
    });
  }
  for (const acc of accounts) {
    rows.push({
      key: acc.id,
      name: acc.name,
      email: acc.email,
      plan: acc.plan,
      active: acc.active,
      needsRelogin: acc.needsRelogin,
      account: acc
    });
  }

  return (
    <div className="-mt-2 flex flex-col rounded-b-xl bg-card pt-2 shadow-sm">
      {state.storageWarning && (
        <p className="px-3 py-2 text-[11px] leading-relaxed text-amber-600 dark:text-amber-400">
          {t("providers.accountKeyringWarning", { mode: state.storageWarning })}
        </p>
      )}

      {rows.length === 0 && !!current && (
        <p className="px-3 py-2 text-xs text-muted-foreground">
          {t("providers.accountsEmpty")}
        </p>
      )}

      {rows.map((row) => {
        const acc = row.account;
        const rowBusy = !!acc && busy === acc.id;
        return (
          <div
            key={row.key}
            className="flex items-center gap-2 border-t border-border/50 px-3 py-2 hover:bg-accent/40 first:border-t-0 last:rounded-b-xl"
          >
            <button
              type="button"
              disabled={row.active || row.needsRelogin || rowBusy}
              onClick={acc ? () => void switchTo(acc) : undefined}
              aria-label={
                row.active
                  ? t("providers.accountActive")
                  : t("providers.accountSwitch")
              }
              className={`group/sw flex size-6 shrink-0 items-center justify-center text-primary${(row.needsRelogin || rowBusy) ? " opacity-30 cursor-not-allowed" : ""}`}
            >
              {rowBusy ? (
                <Loader2 className="size-5 animate-spin" />
              ) : row.active ? (
                <CircleCheck className="size-5" />
              ) : (
                <Circle className="size-5 text-muted-foreground/40 transition-colors group-hover/sw:enabled:text-primary" />
              )}
            </button>
            <div className="min-w-0 flex-1 flex items-center gap-2">
              <span className="shrink-0 max-w-[45%] truncate text-sm">
                {row.name}
              </span>
              {row.plan && (
                <Badge className="shrink-0 uppercase text-[9px] tracking-wide px-1.5 py-0 bg-primary/15 text-primary">
                  {row.plan}
                </Badge>
              )}
              {row.email && (
                <span className="truncate text-[11px] text-muted-foreground">
                  {row.email}
                </span>
              )}
              {row.needsRelogin && (
                <Badge className="shrink-0 text-[10px] px-1.5 py-0 bg-destructive/15 text-destructive border-0">
                  {t("providers.accountNeedsRelogin")}
                </Badge>
              )}
            </div>
            {acc ? (
              <>
                {row.needsRelogin && (
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    disabled={loggingIn || rowBusy}
                    onClick={() => void loginAndSave()}
                    className="shrink-0 gap-1.5 text-destructive border-destructive/40 hover:bg-destructive/10"
                  >
                    {loggingIn ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : null}
                    {t("providers.accountRelogin")}
                  </Button>
                )}
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      className="text-destructive hover:bg-destructive/10"
                      disabled={rowBusy}
                      onClick={() => void remove(acc)}
                      aria-label={t("providers.accountDelete")}
                    >
                      <Trash2 className="size-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>{t("providers.accountDelete")}</TooltipContent>
                </Tooltip>
              </>
            ) : (
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => void saveCurrent()}
              >
                {t("providers.accountSaveCurrent")}
              </Button>
            )}
          </div>
        );
      })}

      {app === "codex" && (
        <div className={`flex items-center px-3 py-2 last:rounded-b-xl${rows.length > 0 ? " border-t border-border/50" : ""}`}>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={loggingIn}
            onClick={() => void loginAndSave()}
            className="gap-1.5"
          >
            {loggingIn ? (
              <>
                <Loader2 className="size-3.5 animate-spin" />
                {t("providers.accountAdding")}
              </>
            ) : (
              <>
                <UserPlus className="size-3.5" />
                {t("providers.accountAdd")}
              </>
            )}
          </Button>
        </div>
      )}
    </div>
  );
}
