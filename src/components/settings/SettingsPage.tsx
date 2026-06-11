import React from "react";
import { useTheme } from "next-themes";
import { invoke } from "@tauri-apps/api/core";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { homeDir, join } from "@tauri-apps/api/path";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { toast } from "sonner";
import {
  isEnabled as autostartIsEnabled,
  enable as autostartEnable,
  disable as autostartDisable
} from "@tauri-apps/plugin-autostart";
import { Folder, FolderOpen, Monitor, Moon, RefreshCw, Sun, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import { getConfig, setConfig } from "@/config";
import { useI18n, LOCALES, type MessageKey } from "@/i18n";

type ThemeChoice = "system" | "light" | "dark";

const THEME_OPTIONS: { value: ThemeChoice; labelKey: MessageKey; icon: React.ReactNode }[] = [
  { value: "system", labelKey: "settings.theme.system", icon: <Monitor className="size-6" /> },
  { value: "light", labelKey: "settings.theme.light", icon: <Sun className="size-6" /> },
  { value: "dark", labelKey: "settings.theme.dark", icon: <Moon className="size-6" /> }
];

const SHORTCUTS: { keys: string[]; labelKey: MessageKey }[] = [
  { keys: ["⌘", "K"], labelKey: "settings.shortcuts.searchPalette" },
  { keys: ["⌘", "F"], labelKey: "settings.shortcuts.searchPaletteAlias" },
  { keys: ["⌘", "1..6"], labelKey: "settings.shortcuts.switchRoute" },
  { keys: ["Esc"], labelKey: "settings.shortcuts.closePalette" }
];

export function SettingsPage({
  recentSearches,
  onClearRecent,
  autoCheckUpdates,
  onAutoCheckUpdatesChange,
  appVersion,
  onUpdateFound
}: {
  recentSearches: string[];
  onClearRecent: () => void;
  autoCheckUpdates: boolean;
  onAutoCheckUpdatesChange: (next: boolean) => void;
  appVersion: string;
  onUpdateFound: (update: Update) => void;
}) {
  const { t, locale, setLocale } = useI18n();
  const { theme, setTheme } = useTheme();
  const [mounted, setMounted] = React.useState(false);
  const [termoryDir, setTermoryDir] = React.useState<string | null>(null);
  const [checking, setChecking] = React.useState(false);
  const [terminal, setTerminal] = React.useState("auto");
  const [terminals, setTerminals] = React.useState<{ id: string; label: string }[]>([
    { id: "auto", label: "Default" }
  ]);

  React.useEffect(() => {
    setMounted(true);
    void (async () => {
      try {
        const home = await homeDir();
        setTermoryDir(await join(home, ".termory"));
      } catch {
        setTermoryDir(null);
      }
    })();
    void (async () => {
      try {
        const [saved, list] = await Promise.all([
          getConfig<string>("terminal"),
          invoke<{ id: string; label: string }[]>("detect_terminals")
        ]);
        if (list.length > 0) setTerminals(list);
        if (saved) setTerminal(saved);
      } catch {
        /* keep defaults */
      }
    })();
  }, []);

  const handleTerminalChange = (value: string) => {
    setTerminal(value);
    void setConfig("terminal", value).catch((err) =>
      toast.error(t("settings.terminal.saveError", { error: String(err) }))
    );
  };

  const handleCheckUpdate = async () => {
    setChecking(true);
    try {
      const result = await check();
      if (result) {
        onUpdateFound(result);
      } else {
        toast.success(t("settings.about.latest"));
      }
    } catch (err) {
      toast.error(t("settings.about.checkFailed", { error: String(err) }));
    } finally {
      setChecking(false);
    }
  };

  // Launch-at-login toggle (tauri-plugin-autostart). State seeds from
  // the OS-side truth on mount; toggling writes through immediately.
  const [autostart, setAutostart] = React.useState(false);
  React.useEffect(() => {
    autostartIsEnabled()
      .then(setAutostart)
      .catch(() => {});
  }, []);
  const toggleAutostart = async (next: boolean) => {
    try {
      if (next) {
        await autostartEnable();
      } else {
        await autostartDisable();
      }
      setAutostart(next);
    } catch (err) {
      toast.error(t("settings.startup.error", { error: String(err) }));
    }
  };

  const current = (mounted ? (theme as ThemeChoice) : "system") ?? "system";

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      <div className="flex-1 min-h-0 overflow-auto px-3 pt-3 pb-0">
        <div className="flex flex-col gap-2">
          <SettingsSection title={t("settings.appearance")}>
              <div className="grid grid-cols-3 gap-2">
                {THEME_OPTIONS.map((opt) => {
                  const active = current === opt.value;
                  return (
                    <Button
                      key={opt.value}
                      type="button"
                      variant={active ? "default" : "outline"}
                      onClick={() => setTheme(opt.value)}
                      className="flex-col h-auto gap-1.5 px-3 py-3 font-normal"
                    >
                      <span className="inline-flex items-center justify-center size-8">
                        {opt.icon}
                      </span>
                      <span>{t(opt.labelKey)}</span>
                    </Button>
                  );
                })}
              </div>
          </SettingsSection>

          <SettingsSection title={t("settings.language")}>
            <div className="flex flex-col gap-2">
              <div className="text-xs text-muted-foreground">
                {t("settings.language.desc")}
              </div>
              <Select value={locale} onValueChange={(v) => setLocale(v as typeof locale)}>
                <SelectTrigger className="w-full sm:w-72">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {LOCALES.map((l) => (
                    <SelectItem key={l.value} value={l.value}>
                      {l.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </SettingsSection>

          <SettingsSection title={t("settings.startup")}>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5 min-w-0">
                <div className="text-sm">{t("settings.startup.launchAtLogin")}</div>
                <div className="text-xs text-muted-foreground">
                  {t("settings.startup.desc")}
                </div>
              </div>
              <Switch
                checked={autostart}
                onCheckedChange={(v) => void toggleAutostart(v)}
                aria-label={t("settings.startup.launchAtLogin")}
              />
            </div>
          </SettingsSection>

          <SettingsSection title={t("settings.terminal")}>
            <div className="flex flex-col gap-2">
              <div className="text-xs text-muted-foreground">
                {t("settings.terminal.desc")}
              </div>
              <Select value={terminal} onValueChange={handleTerminalChange}>
                <SelectTrigger className="w-full sm:w-72">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {terminals.map((tm) => (
                    <SelectItem key={tm.id} value={tm.id}>
                      {tm.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </SettingsSection>

          <SettingsSection title={t("settings.storage")}>
            <div className="flex flex-col gap-2">
              <div className="text-xs text-muted-foreground">{t("settings.storage.dir")}</div>
              <div className="flex items-center gap-2">
                <Folder size={14} className="shrink-0 text-muted-foreground" />
                <span className="flex-1 min-w-0 truncate text-sm font-mono">
                  {termoryDir ?? "~/.termory/"}
                </span>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={!termoryDir}
                  onClick={() => termoryDir && void revealItemInDir(termoryDir)}
                >
                  <FolderOpen className="size-4" />
                  {t("settings.storage.open")}
                </Button>
              </div>
              <p className="text-xs text-muted-foreground leading-relaxed">
                {t("settings.storage.note")}
              </p>
            </div>
          </SettingsSection>

          <SettingsSection title={t("settings.search")}>
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5">
                <div className="text-sm">{t("settings.search.recent")}</div>
                <div className="text-xs text-muted-foreground">
                  {t(
                    recentSearches.length === 1
                      ? "settings.search.count_one"
                      : "settings.search.count_other",
                    { n: recentSearches.length }
                  )}
                </div>
              </div>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={recentSearches.length === 0}
                onClick={onClearRecent}
              >
                <Trash2 className="size-4" />
                {t("settings.search.clear")}
              </Button>
            </div>
          </SettingsSection>

          <SettingsSection title={t("settings.shortcuts")}>
            <ul className="flex flex-col gap-2">
              {SHORTCUTS.map((sc) => (
                <li key={sc.labelKey} className="flex items-center justify-between gap-3 text-sm">
                  <span className="text-muted-foreground">{t(sc.labelKey)}</span>
                  <span className="flex items-center gap-1 shrink-0">
                    {sc.keys.map((k) => (
                      <kbd
                        key={k}
                        className="inline-flex h-5 min-w-5 items-center justify-center rounded bg-muted px-1.5 text-[10px] font-medium font-mono"
                      >
                        {k}
                      </kbd>
                    ))}
                  </span>
                </li>
              ))}
            </ul>
          </SettingsSection>

          <SettingsSection title={t("settings.about")}>
            <div className="flex flex-col gap-3">
              <div className="flex items-center justify-between gap-3">
                <div className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm min-w-0">
                  <span className="text-muted-foreground">{t("settings.about.app")}</span>
                  <span>Termory</span>
                  <span className="text-muted-foreground">{t("settings.about.version")}</span>
                  <span className="font-mono">{appVersion ? `v${appVersion}` : "—"}</span>
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={checking}
                  onClick={() => void handleCheckUpdate()}
                >
                  <RefreshCw className="size-4" />
                  {t("settings.about.check")}
                </Button>
              </div>
              <div className="flex items-center justify-between gap-3 pt-1">
                <div className="flex flex-col gap-0.5 min-w-0">
                  <div className="text-sm">{t("settings.about.auto")}</div>
                  <div className="text-xs text-muted-foreground">
                    {t("settings.about.autoDesc")}
                  </div>
                </div>
                <Switch
                  checked={autoCheckUpdates}
                  onCheckedChange={onAutoCheckUpdatesChange}
                />
              </div>
            </div>
          </SettingsSection>
        </div>
      </div>
    </div>
  );
}

function SettingsSection({
  title,
  children
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <Card className="p-3 gap-0 outline outline-1 outline-transparent bg-card shadow-sm">
      <CardContent className="px-0 flex flex-col gap-3">
        <h2 className="text-lg font-medium">{title}</h2>
        {children}
      </CardContent>
    </Card>
  );
}
