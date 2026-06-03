import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, Plug, Plus } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  ACTIVE_STATE_REFRESH_EVENT,
  CLI_APPS,
  CLI_APP_LABEL,
  CLI_APP_SOURCE_BADGE
} from "@/constants";
import { blankProvider } from "@/lib/provider-utils";
import type { ActiveState, CliApp, Provider, TestResult } from "@/types";
import { BrandIcon } from "@/components/BrandIcon";
import { EmptyState } from "@/components/EmptyState";
import { ProviderCard } from "./ProviderCard";
import { ProviderOfficialCard } from "./ProviderOfficialCard";

// InstallGuide and ProviderEditor are conditionally rendered (CLI
// missing / editor open), so lazy-load to keep them out of the main
// Providers chunk. Editor is the heavier of the two — it pulls in
// the AI-SDK provider-id catalog, datalist autocomplete, the
// invoke-based test/fetch-models actions, etc.
const InstallGuide = React.lazy(() =>
  import("./InstallGuide").then((m) => ({ default: m.InstallGuide }))
);
const ProviderEditor = React.lazy(() =>
  import("./ProviderEditor").then((m) => ({ default: m.ProviderEditor }))
);

// Module-level cache for CLI detection results so the OpenCode tab
// doesn't flash "Official → InstallGuide" every time the user
// switches away from Providers and back. ProvidersPage is gated by
// `route === "providers"` in App.tsx and so unmounts on every route
// change; without this cache each remount would briefly render the
// Official card with the optimistic `installed[opencode] = true`
// default before the async detect_clis returned `false`.
//
// Updated by the page itself whenever a fresh detect result lands;
// stays alive across mount/unmount cycles until the window reloads.
let cachedInstalled: Record<CliApp, boolean> = {
  claude: true,
  codex: true,
  gemini: true,
  opencode: true
};
let cachedVersions: Record<CliApp, string | null> = {
  claude: null,
  codex: null,
  gemini: null,
  opencode: null
};
let cachedVersionsLoading = true;
// True once the first `refreshVersions()` of the app lifetime has
// resolved. Used to keep the version skeleton from flashing on every
// route remount — after the first detect, subsequent route entries
// render the cached values silently and only watcher events / Recheck
// trigger a re-fetch (visible flash there is correct: the user
// actually changed something).
let versionsEverResolved = false;

export function ProvidersPage({
  providers,
  setProviders,
  app,
  setApp
}: {
  providers: Provider[];
  setProviders: React.Dispatch<React.SetStateAction<Provider[]>>;
  app: CliApp;
  setApp: (next: CliApp) => void;
}) {
  const [editing, setEditing] = React.useState<Provider | null>(null);
  const [editingIsNew, setEditingIsNew] = React.useState(false);
  const [activeStates, setActiveStates] = React.useState<Record<CliApp, ActiveState | null>>({
    claude: null,
    codex: null,
    gemini: null,
    opencode: null
  });
  // Initialize from the module-level cache so a remount (route
  // switch) renders with the last-known truth, not the optimistic
  // default. The cache is written back from inside the refresh
  // helpers below.
  const [installed, setInstalled] =
    React.useState<Record<CliApp, boolean>>(cachedInstalled);
  const [versions, setVersions] =
    React.useState<Record<CliApp, string | null>>(cachedVersions);
  const [versionsLoading, setVersionsLoading] = React.useState(
    cachedVersionsLoading
  );

  // Mirror state into the module-level cache on every change so the
  // next mount has the fresh truth as its initial value.
  React.useEffect(() => {
    cachedInstalled = installed;
  }, [installed]);
  React.useEffect(() => {
    cachedVersions = versions;
  }, [versions]);
  React.useEffect(() => {
    cachedVersionsLoading = versionsLoading;
  }, [versionsLoading]);
  const [toggling, setToggling] = React.useState<string | null>(null);
  const [testing, setTesting] = React.useState<string | null>(null);
  const [testResults, setTestResults] = React.useState<Record<string, TestResult>>({});
  const [settingDefault, setSettingDefault] = React.useState<string | null>(null);
  const [rechecking, setRechecking] = React.useState(false);

  const refreshInstalled = React.useCallback(async () => {
    try {
      const map = await invoke<Record<string, boolean>>("detect_clis");
      setInstalled({
        claude: !!map.claude,
        codex: !!map.codex,
        gemini: !!map.gemini,
        opencode: !!map.opencode
      });
    } catch {
      /* leave previous state on error */
    }
  }, []);

  // Heavier than refreshInstalled — spawns 4 `<bin> --version`
  // subprocesses. Called on first cold-start mount, on Recheck, and
  // when the watcher reports an install/uninstall. Subsequent route
  // remounts read straight from the module-level cache without
  // re-firing this.
  //
  // Only flips `versionsLoading` to `true` on the very first attempt
  // of the app lifetime. After that, refreshes update the value
  // silently — no skeleton flash — because the user already has
  // accurate numbers on screen.
  const refreshVersions = React.useCallback(async () => {
    if (!versionsEverResolved) {
      setVersionsLoading(true);
    }
    try {
      const map = await invoke<Record<string, string | null>>(
        "detect_cli_versions_cmd"
      );
      setVersions({
        claude: map.claude ?? null,
        codex: map.codex ?? null,
        gemini: map.gemini ?? null,
        opencode: map.opencode ?? null
      });
    } catch {
      /* leave previous state on error */
    } finally {
      versionsEverResolved = true;
      setVersionsLoading(false);
    }
  }, []);

  const handleRecheckInstall = async () => {
    setRechecking(true);
    try {
      const map = await invoke<Record<string, boolean>>("detect_clis");
      const next = {
        claude: !!map.claude,
        codex: !!map.codex,
        gemini: !!map.gemini,
        opencode: !!map.opencode
      };
      setInstalled(next);
      if (next[app]) {
        toast.success(`${CLI_APP_LABEL[app]} detected.`);
        void refreshVersions();
      } else {
        toast.error(`${CLI_APP_LABEL[app]} still not installed.`);
      }
    } catch (err) {
      toast.error(`Detection failed: ${String(err)}`);
    } finally {
      setRechecking(false);
    }
  };

  // Pre-action gate: re-check whether the target CLI is installed
  // right now. Returns true to proceed, false to abort (toast already
  // shown). Used by the action handlers that actually need the CLI
  // binary to consume the live config we're about to mutate.
  const ensureCliInstalled = async (target: CliApp): Promise<boolean> => {
    try {
      const map = await invoke<Record<string, boolean>>("detect_clis");
      setInstalled({
        claude: !!map.claude,
        codex: !!map.codex,
        gemini: !!map.gemini,
        opencode: !!map.opencode
      });
      if (!map[target]) {
        toast.error(
          `${CLI_APP_LABEL[target]} is not installed. Install it first.`
        );
        return false;
      }
      return true;
    } catch {
      return true; // detection failed — don't block the user
    }
  };

  const refreshActive = React.useCallback(async () => {
    try {
      const states = await invoke<ActiveState[]>("provider_active_states", { providers });
      const next: Record<CliApp, ActiveState | null> = {
        claude: null,
        codex: null,
        gemini: null,
        opencode: null
      };
      for (const s of states) next[s.app] = s;
      setActiveStates(next);
    } catch (err) {
      toast.error(`Read live state failed: ${String(err)}`);
    }
  }, [providers]);

  React.useEffect(() => {
    void refreshActive();
  }, [refreshActive]);

  React.useEffect(() => {
    void refreshInstalled();
    // First-mount-of-app-lifetime gate. Route remounts read the
    // cached versions instantly; only the first cold start (or a
    // page reload) pays the subprocess cost. After that, watcher
    // events and manual Recheck are the only paths that re-fire
    // `refreshVersions`.
    if (!versionsEverResolved) {
      void refreshVersions();
    }
  }, [refreshInstalled, refreshVersions]);

  // Event-driven install detection — no polling. Three triggers:
  //   1. Rust watcher fires `cli-install-changed` when any CLI binary
  //      dir or node-version-manager root mutates (install / uninstall).
  //   2. Tauri window gains focus — covers the case where the OS
  //      didn't propagate an FS event (e.g. uninstall script left the
  //      binary in place but stripped PATH; user came back from
  //      terminal and we re-check just in case).
  //   3. (Already wired above) Page mount + manual Recheck.
  //
  // Dedup: only refetch versions when the installed map actually flips.
  // `detect_clis` is pure stat (~10ms), `detect_cli_versions` spawns 4
  // subprocesses (~hundreds of ms) — without this guard, every focus
  // event would flash the Version skeleton even when nothing changed.
  const installedRef = React.useRef(installed);
  installedRef.current = installed;
  React.useEffect(() => {
    const refresh = async () => {
      try {
        const map = await invoke<Record<string, boolean>>("detect_clis");
        const next = {
          claude: !!map.claude,
          codex: !!map.codex,
          gemini: !!map.gemini,
          opencode: !!map.opencode
        };
        const prev = installedRef.current;
        const changed =
          prev.claude !== next.claude ||
          prev.codex !== next.codex ||
          prev.gemini !== next.gemini ||
          prev.opencode !== next.opencode;
        if (changed) {
          setInstalled(next);
          void refreshVersions();
        }
      } catch {
        /* leave previous state on transient error */
      }
    };
    const unlistenPromise = listen("termory:cli-install-changed", () => {
      void refresh();
    });
    const win = getCurrentWindow();
    const focusPromise = win.onFocusChanged(({ payload: focused }) => {
      if (focused) void refresh();
    });
    return () => {
      void unlistenPromise.then((fn) => fn()).catch(() => {});
      void focusPromise.then((fn) => fn()).catch(() => {});
    };
  }, [refreshVersions]);

  // Auto refresh when the Rust watcher detects any change in the
  // CLI dirs (live config files live inside those dirs). Reuse the
  // existing `termory:sources-changed` event — payload is ignored.
  React.useEffect(() => {
    const unlistenPromise = listen("termory:sources-changed", () => {
      void refreshActive();
    });
    // The menu-bar tray switches providers via its own handler and
    // emits this after writing the CLI's live config — re-derive so an
    // open Providers page reflects a tray switch even when unfocused.
    const unlistenTrayPromise = listen("termory:providers-changed", () => {
      void refreshActive();
    });
    const peerHandler = () => void refreshActive();
    window.addEventListener(ACTIVE_STATE_REFRESH_EVENT, peerHandler);
    return () => {
      void unlistenPromise.then((fn) => fn()).catch(() => {});
      void unlistenTrayPromise.then((fn) => fn()).catch(() => {});
      window.removeEventListener(ACTIVE_STATE_REFRESH_EVENT, peerHandler);
    };
  }, [refreshActive]);

  const providersForApp = React.useMemo(
    () => providers.filter((p) => p.app === app),
    [providers, app]
  );
  const customProviders = React.useMemo(
    () => providersForApp.filter((p) => p.kind === "custom"),
    [providersForApp]
  );
  const activeState = activeStates[app];

  const startNew = () => {
    setEditing(blankProvider(app));
    setEditingIsNew(true);
  };
  const startEdit = (p: Provider) => {
    setEditing({ ...p });
    setEditingIsNew(false);
  };
  const closeEditor = () => {
    setEditing(null);
    setEditingIsNew(false);
  };

  const saveProvider = async (next: Provider) => {
    const prev = providers.find((p) => p.id === next.id);
    const updated = prev
      ? providers.map((p) => (p.id === next.id ? next : p))
      : [...providers, next];
    setProviders(updated);
    closeEditor();

    // Saving only updates providers.json. If the edited provider is the
    // one currently live for its CLI, re-activate so the change (model /
    // base URL / key / options / …) actually reaches the live config —
    // otherwise the edit silently doesn't take effect until the user
    // manually re-activates. New providers and edits to an inactive
    // provider don't touch any live config.
    const state = activeStates[next.app];
    const isLive =
      next.app === "opencode"
        ? (state?.configuredProviderIds ?? []).includes(next.id)
        : state?.matchedProviderId === next.id;
    if (next.kind !== "custom" || !isLive) return;

    // The backend strips the union of all providers' CURRENT option keys
    // before re-applying — so a key the edit REMOVED is no longer in the
    // union and would be orphaned in the live config. Fold the previous
    // version of this provider into the strip set (backend only reads its
    // option keys; a duplicate id is harmless) so removed keys get
    // cleaned. The applied provider (`next`) carries only current keys,
    // so removed ones are stripped and not written back.
    const stripSet = updated.filter((p) => p.app === next.app);
    if (prev) stripSet.push(prev);
    try {
      await invoke("activate_provider", {
        provider: next,
        providersForApp: stripSet
      });
      // OpenCode: if it was also the startup default, refresh the
      // top-level model pointer in case the primary model changed.
      if (next.app === "opencode" && state?.matchedProviderId === next.id) {
        await invoke("set_opencode_default_provider", { provider: next });
      }
      await refreshActive();
    } catch (err) {
      toast.error(
        `Saved, but couldn't update ${CLI_APP_LABEL[next.app]} live config: ${String(err)}`
      );
    }
  };

  const deleteProvider = async (id: string) => {
    const target = providers.find((p) => p.id === id);
    if (!target) return;
    // Tauri's native confirm dialog — feels at home on each OS
    // (macOS sheet, Windows MessageBox, Linux GTK) and the Delete
    // button is highlighted as the destructive one. Replaces the
    // browser `window.confirm` which used to render an out-of-place
    // generic "OK / Cancel" alert.
    const confirmed = await ask(
      `Delete ${target.name || "this provider"}? This can't be undone.`,
      {
        title: "Delete provider",
        kind: "warning",
        okLabel: "Delete",
        cancelLabel: "Cancel"
      }
    );
    if (!confirmed) return;
    const isInUse = activeStates[target.app]?.matchedProviderId === id;
    try {
      if (target.app === "opencode") {
        // OpenCode is multi-slot — delete only this provider's slot
        // from opencode.json (and the top-level model if it pointed
        // at this provider). Other Termory slots stay intact.
        await invoke("delete_provider", { provider: target });
      } else if (isInUse) {
        // Single-slot CLIs — when the deleted one is the live record,
        // full deactivate clears Termory's writes so the CLI falls
        // back to its native auth.
        await invoke("deactivate_provider", {
          app: target.app,
          providersForApp: providers.filter((p) => p.app === target.app)
        });
      }
    } catch (err) {
      toast.error(`Could not clear ${CLI_APP_LABEL[target.app]} live config: ${String(err)}`);
      return;
    }
    setProviders((cur) => cur.filter((p) => p.id !== id));
    await refreshActive();
  };

  // OpenCode-only: toggle the provider's slot in opencode.json.
  // Enabled means the slot exists (multi-slot coexist). Other CLIs
  // don't have this concept — they only have "Set as default".
  const toggleEnabled = async (target: Provider) => {
    if (target.app !== "opencode") return;
    if (!(await ensureCliInstalled(target.app))) return;
    const state = activeStates[target.app];
    const enabled = (state?.configuredProviderIds ?? []).includes(target.id);
    setToggling(target.id);
    try {
      if (enabled) {
        await invoke("delete_provider", { provider: target });
        toast.success(`Disabled ${target.name || "(unnamed)"}.`);
      } else {
        await invoke("activate_provider", {
          provider: target,
          providersForApp
        });
        toast.success(`Enabled ${target.name || "(unnamed)"}.`);
      }
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setToggling(null);
    }
  };

  // Universal "Set as default" — promotes a provider to "In use".
  const setAsDefault = async (target: Provider) => {
    if (!(await ensureCliInstalled(target.app))) return;
    setSettingDefault(target.id);
    try {
      if (target.app === "opencode") {
        await invoke("set_opencode_default_provider", { provider: target });
      } else {
        await invoke("activate_provider", {
          provider: target,
          providersForApp
        });
      }
      toast.success(`${target.name || "(unnamed)"} is now in use.`);
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setSettingDefault(null);
    }
  };

  // Official "Set as default" — clears Termory writes from the CLI's
  // live config so it falls back to its native auth flow.
  const setOfficialAsDefault = async () => {
    if (!(await ensureCliInstalled(app))) return;
    setSettingDefault("__official__");
    try {
      await invoke("deactivate_provider", {
        app,
        providersForApp
      });
      toast.success(`Official is now in use for ${CLI_APP_LABEL[app]}.`);
      await refreshActive();
    } catch (err) {
      toast.error(String(err));
    } finally {
      setSettingDefault(null);
    }
  };

  const testOne = async (target: Provider) => {
    setTesting(target.id);
    try {
      const result = await invoke<TestResult>("test_provider_api", { provider: target });
      setTestResults((cur) => ({ ...cur, [target.id]: result }));
    } catch (err) {
      setTestResults((cur) => ({
        ...cur,
        [target.id]: {
          ok: false,
          status: null,
          latencyMs: 0,
          message: String(err)
        }
      }));
    } finally {
      setTesting(null);
    }
  };

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-background">
      <div className="px-3 pt-3 pb-3">
        <div className="flex items-center gap-1 rounded-md bg-muted p-3">
          <div className="flex-1 min-w-0">
            <Tabs value={app} onValueChange={(v) => setApp(v as CliApp)}>
              <TabsList className="w-full justify-start gap-1 bg-transparent p-0 [&>button]:flex-none [&>button]:rounded-md [&>button]:px-3">
                {CLI_APPS.map((id) => (
                  <TabsTrigger key={id} value={id}>
                    <BrandIcon source={CLI_APP_SOURCE_BADGE[id]} />
                    <span>{CLI_APP_LABEL[id]}</span>
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
          </div>
          <Button
            type="button"
            size="icon"
            onClick={startNew}
            disabled={!installed[app]}
            aria-label="Add provider"
            className="rounded-md size-8 shrink-0 shadow-sm"
          >
            <Plus className="size-4" />
          </Button>
        </div>
      </div>

      {!installed[app] && customProviders.length === 0 ? (
        <React.Suspense fallback={null}>
          <InstallGuide
            app={app}
            rechecking={rechecking}
            onRecheck={() => void handleRecheckInstall()}
          />
        </React.Suspense>
      ) : (
        <div className="flex-1 min-h-0 overflow-auto px-3 pb-0">
          <div className="flex flex-col gap-3">
            {!installed[app] && (
              <div className="flex items-center gap-2 rounded-md outline outline-1 outline-amber-500/30 bg-amber-50 dark:bg-amber-950/40 text-amber-700 dark:text-amber-300 px-3 py-2 text-base leading-relaxed">
                <AlertTriangle className="size-4 shrink-0" />
                <div className="flex-1">
                  <strong className="font-medium">
                    {CLI_APP_LABEL[app]} is not installed.
                  </strong>{" "}
                  Edit and delete still work, but providers can't be activated
                  until it's installed.
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={rechecking}
                  onClick={() => void handleRecheckInstall()}
                  className="shrink-0"
                >
                  {rechecking ? "Checking…" : "Recheck"}
                </Button>
              </div>
            )}
            {installed[app] && (
              <ProviderOfficialCard
                app={app}
                isInUse={activeState?.kind === "official"}
                settingDefault={settingDefault === "__official__"}
                version={versions[app]}
                versionLoading={versionsLoading}
                onSetDefault={() => void setOfficialAsDefault()}
              />
            )}

            {customProviders.map((p) => {
              const configuredIds = activeState?.configuredProviderIds ?? [];
              const matchedId = activeState?.matchedProviderId ?? null;
              const isOpencode = p.app === "opencode";
              const isConfigured = isOpencode
                ? configuredIds.includes(p.id)
                : matchedId === p.id;
              const isInUse = matchedId === p.id;
              return (
                <ProviderCard
                  key={p.id}
                  provider={p}
                  isConfigured={isConfigured}
                  isInUse={isInUse}
                  toggling={toggling === p.id}
                  settingDefault={settingDefault === p.id}
                  testing={testing === p.id}
                  testResult={testResults[p.id]}
                  activatable={installed[app]}
                  onToggleEnabled={isOpencode ? () => void toggleEnabled(p) : undefined}
                  onSetDefault={() => void setAsDefault(p)}
                  onEdit={() => startEdit(p)}
                  onDelete={() => deleteProvider(p.id)}
                  onTest={() => void testOne(p)}
                />
              );
            })}

            {customProviders.length === 0 && (
              <EmptyState
                icon={<Plug size={32} />}
                title="No custom providers yet"
                description={`Add a third-party API platform for ${CLI_APP_LABEL[app]} and switch to it with one click.`}
                action={{ label: "Add provider", onClick: startNew }}
              />
            )}
          </div>
        </div>
      )}

      {editing && (
        <React.Suspense fallback={null}>
          <ProviderEditor
            provider={editing}
            isNew={editingIsNew}
            onSave={saveProvider}
            onClose={closeEditor}
          />
        </React.Suspense>
      )}
    </div>
  );
}
