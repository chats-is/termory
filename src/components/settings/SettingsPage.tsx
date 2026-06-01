import React from "react";
import { useTheme } from "next-themes";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { homeDir, join } from "@tauri-apps/api/path";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { toast } from "sonner";
import { Folder, FolderOpen, Loader2, Monitor, Moon, RefreshCw, Sun, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";

type ThemeChoice = "system" | "light" | "dark";

const THEME_OPTIONS: { value: ThemeChoice; label: string; icon: React.ReactNode }[] = [
  { value: "system", label: "System", icon: <Monitor className="size-6" /> },
  { value: "light", label: "Light", icon: <Sun className="size-6" /> },
  { value: "dark", label: "Dark", icon: <Moon className="size-6" /> }
];

const SHORTCUTS: { keys: string[]; label: string }[] = [
  { keys: ["⌘", "K"], label: "Open search palette" },
  { keys: ["⌘", "F"], label: "Open search palette (alias)" },
  { keys: ["⌘", "1..6"], label: "Switch rail route" },
  { keys: ["Esc"], label: "Close palette / dropdown" }
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
  const { theme, setTheme } = useTheme();
  const [mounted, setMounted] = React.useState(false);
  const [termoryDir, setTermoryDir] = React.useState<string | null>(null);
  const [checking, setChecking] = React.useState(false);

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
  }, []);

  const handleCheckUpdate = async () => {
    setChecking(true);
    try {
      const result = await check();
      if (result) {
        onUpdateFound(result);
      } else {
        toast.success("You're on the latest version.");
      }
    } catch (err) {
      toast.error(`Update check failed: ${String(err)}`);
    } finally {
      setChecking(false);
    }
  };

  const current = (mounted ? (theme as ThemeChoice) : "system") ?? "system";

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      <div className="flex-1 min-h-0 overflow-auto px-3 pt-3 pb-0">
        <div className="flex flex-col gap-2">
          <SettingsSection title="Appearance">
              <div className="grid grid-cols-3 gap-2">
                {THEME_OPTIONS.map((opt) => {
                  const active = current === opt.value;
                  return (
                    <button
                      key={opt.value}
                      type="button"
                      onClick={() => setTheme(opt.value)}
                      className={cn(
                        "flex flex-col items-center gap-1.5 rounded-md px-3 py-3 text-sm transition-colors outline outline-1",
                        active
                          ? "bg-primary text-primary-foreground outline-transparent"
                          : "outline-foreground/5 hover:bg-accent hover:text-accent-foreground"
                      )}
                    >
                      <span className="inline-flex items-center justify-center size-8">
                        {opt.icon}
                      </span>
                      <span>{opt.label}</span>
                    </button>
                  );
                })}
              </div>
          </SettingsSection>

          <SettingsSection title="Storage">
            <div className="flex flex-col gap-2">
              <div className="text-xs text-muted-foreground">Termory data directory</div>
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
                  Open
                </Button>
              </div>
              <p className="text-xs text-muted-foreground leading-relaxed">
                UI preferences live in <span className="font-mono">config.json</span>, provider library in{" "}
                <span className="font-mono">providers.json</span>. Both files are <span className="font-mono">chmod 0600</span> on Unix.
              </p>
            </div>
          </SettingsSection>

          <SettingsSection title="Search history">
            <div className="flex items-center justify-between gap-3">
              <div className="flex flex-col gap-0.5">
                <div className="text-sm">Recent searches</div>
                <div className="text-xs text-muted-foreground">
                  {recentSearches.length} stored {recentSearches.length === 1 ? "entry" : "entries"}
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
                Clear
              </Button>
            </div>
          </SettingsSection>

          <SettingsSection title="Keyboard shortcuts">
            <ul className="flex flex-col gap-2">
              {SHORTCUTS.map((sc) => (
                <li key={sc.label} className="flex items-center justify-between gap-3 text-sm">
                  <span className="text-muted-foreground">{sc.label}</span>
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

          <SettingsSection title="About">
            <div className="flex flex-col gap-3">
              <div className="flex items-center justify-between gap-3">
                <div className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm min-w-0">
                  <span className="text-muted-foreground">App</span>
                  <span>Termory</span>
                  <span className="text-muted-foreground">Version</span>
                  <span className="font-mono">{appVersion ? `v${appVersion}` : "—"}</span>
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={checking}
                  onClick={() => void handleCheckUpdate()}
                >
                  {checking ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    <RefreshCw className="size-4" />
                  )}
                  {checking ? "Checking…" : "Check for updates"}
                </Button>
              </div>
              <div className="flex items-center justify-between gap-3 pt-1">
                <div className="flex flex-col gap-0.5 min-w-0">
                  <div className="text-sm">Check for updates automatically</div>
                  <div className="text-xs text-muted-foreground">
                    Runs once a few seconds after the app launches.
                  </div>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={autoCheckUpdates}
                  onClick={() => onAutoCheckUpdatesChange(!autoCheckUpdates)}
                  className={cn(
                    "relative inline-flex h-5 w-9 shrink-0 rounded-full transition-colors",
                    autoCheckUpdates ? "bg-primary" : "bg-muted"
                  )}
                >
                  <span
                    className={cn(
                      "absolute top-0.5 inline-block size-4 rounded-full bg-background shadow-sm transition-transform",
                      autoCheckUpdates ? "translate-x-[18px]" : "translate-x-0.5"
                    )}
                  />
                </button>
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
