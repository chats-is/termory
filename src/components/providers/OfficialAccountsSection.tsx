import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask } from "@tauri-apps/plugin-dialog";
import { Circle, CircleCheck, Loader2, Pencil, Trash2, UserPlus } from "lucide-react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { QUOTA_CHANGED_EVENT } from "@/constants";
import { useT } from "@/i18n";
import type { AccountsState, CliApp, CurrentAccount, SavedAccount } from "@/types";

/** Modal collecting / editing an account label. */
function LabelDialog({
  open,
  title,
  initial,
  onClose,
  onSubmit
}: {
  open: boolean;
  title: string;
  initial: string;
  onClose: () => void;
  onSubmit: (label: string) => Promise<void>;
}) {
  const t = useT();
  const [label, setLabel] = React.useState(initial);
  const [saving, setSaving] = React.useState(false);
  const inputRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    if (open) {
      setLabel(initial);
      const id = window.setTimeout(() => {
        inputRef.current?.focus();
        inputRef.current?.select();
      }, 0);
      return () => window.clearTimeout(id);
    }
  }, [open, initial]);

  const submit = async () => {
    setSaving(true);
    try {
      await onSubmit(label.trim());
      onClose();
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(o) => !o && !saving && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
        </DialogHeader>
        <div className="space-y-1.5">
          <Label htmlFor="account-label">{t("providers.accountName")}</Label>
          <Input
            ref={inputRef}
            id="account-label"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            placeholder={t("providers.accountNamePlaceholder")}
            disabled={saving}
            onKeyDown={(e) => {
              if (e.key === "Enter" && label.trim()) void submit();
            }}
          />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose} disabled={saving}>
            {t("common.cancel")}
          </Button>
          <Button onClick={() => void submit()} disabled={saving || !label.trim()}>
            {saving && <Loader2 className="size-4 animate-spin" />}
            {saving ? t("providers.saving") : t("providers.save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Saved official-account list for one CLI (phase 1: Codex), rendered as
 * a self-contained section directly under the Official card. Lists the
 * saved logins as plain rows, lets the user snapshot the current login,
 * switch between them, rename, and delete. */
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
  const [dialog, setDialog] = React.useState<
    { mode: "save" } | { mode: "rename"; account: SavedAccount } | null
  >(null);

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

  // Auto-refresh when the user logs in / out of the CLI elsewhere: the
  // window regaining focus, or the watcher's credential-change event
  // (a written `auth.json` fires `termory:quota-changed` for this app).
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

  const saveCurrent = async (label: string) => {
    try {
      await invoke("save_account", { app, label: label || null });
      toast.success(t("toast.accountSaved"));
      await reload();
    } catch (err) {
      toast.error(String(err));
      throw err;
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

  const rename = async (account: SavedAccount, label: string) => {
    try {
      await invoke("rename_account", { id: account.id, label });
      await reload();
    } catch (err) {
      toast.error(String(err));
      throw err;
    }
  };

  const switchTo = async (account: SavedAccount) => {
    if (account.active) return;
    const warn =
      current && !current.saved
        ? `\n\n${t("providers.accountSwitchWarnUnsaved")}`
        : "";
    const ok = await ask(
      `${t("providers.accountSwitchConfirm", { name: account.label })}${warn}`,
      { title: t("providers.accountSwitchTitle"), kind: "warning" }
    );
    if (!ok) return;
    setBusy(account.id);
    try {
      await invoke("switch_account", { id: account.id });
      // Attempt to refresh the restored tokens so the CLI doesn't get a 401.
      const refresh = await invoke<{ refreshed: boolean; warning: string | null }>(
        "refresh_codex_tokens"
      );
      if (refresh?.warning) {
        toast.warning(t("toast.accountTokenExpired"));
      }
      toast.success(t("toast.accountSwitched", { name: account.label }));
      await reload();
      onSwitched?.();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setBusy(null);
    }
  };

  const remove = async (account: SavedAccount) => {
    const ok = await ask(
      t("providers.accountDeleteConfirm", { name: account.label }),
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

  // The current login is ALWAYS shown (marked active), even before it's
  // saved — an unsaved one is a synthetic row (account: null) that offers
  // a Save action; once saved it merges into `accounts` and gains
  // rename/delete. Saved accounts follow.
  type Row = {
    key: string;
    label: string;
    email?: string | null;
    plan?: string | null;
    active: boolean;
    account: SavedAccount | null;
  };
  const rows: Row[] = [];
  if (current && !current.saved) {
    rows.push({
      key: "__current__",
      label: current.name ?? current.email ?? "Codex",
      email: current.email,
      plan: current.plan,
      active: true,
      account: null
    });
  }
  for (const acc of accounts) {
    rows.push({
      key: acc.id,
      label: acc.label,
      email: acc.email,
      plan: acc.plan,
      active: acc.active,
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

      {rows.length === 0 && (
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
            className="flex items-center gap-2 px-3 py-2 hover:bg-accent/40 last:rounded-b-xl"
          >
            <button
              type="button"
              disabled={row.active || rowBusy}
              onClick={acc ? () => void switchTo(acc) : undefined}
              aria-label={
                row.active
                  ? t("providers.accountActive")
                  : t("providers.accountSwitch")
              }
              className="group/sw flex size-5 shrink-0 items-center justify-center text-primary"
            >
              {rowBusy ? (
                <Loader2 className="size-4 animate-spin" />
              ) : row.active ? (
                <CircleCheck className="size-4" />
              ) : (
                <Circle className="size-4 text-muted-foreground/40 transition-colors group-hover/sw:text-primary" />
              )}
            </button>
            <div className="min-w-0 flex-1 flex items-center gap-2">
              <span className="shrink-0 max-w-[45%] truncate text-sm">
                {row.label}
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
            </div>
            {acc ? (
              <>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      disabled={rowBusy}
                      onClick={() =>
                        setDialog({ mode: "rename", account: acc })
                      }
                      aria-label={t("providers.accountRename")}
                    >
                      <Pencil className="size-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>{t("providers.accountRename")}</TooltipContent>
                </Tooltip>
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
                onClick={() => setDialog({ mode: "save" })}
              >
                {t("providers.accountSaveCurrent")}
              </Button>
            )}
          </div>
        );
      })}

      {app === "codex" && (
        <div className="flex items-center px-3 py-2 last:rounded-b-xl">
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

      <LabelDialog
        open={dialog !== null}
        title={
          dialog?.mode === "rename"
            ? t("providers.accountRename")
            : t("providers.accountSaveCurrent")
        }
        initial={
          dialog?.mode === "rename"
            ? dialog.account.label
            : current?.name ?? current?.email ?? ""
        }
        onClose={() => setDialog(null)}
        onSubmit={(label) =>
          dialog?.mode === "rename"
            ? rename(dialog.account, label)
            : saveCurrent(label)
        }
      />
    </div>
  );
}
