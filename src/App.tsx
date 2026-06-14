import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { getVersion } from "@tauri-apps/api/app";
import type { Update } from "@tauri-apps/plugin-updater";
import {
  BookOpen,
  Calendar,
  ChevronRight,
  FolderOpen,
  File,
  FileJson,
  Folder,
  Layers,
  Loader2,
  MessageSquare,
  Sparkles
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type {
  AppSession,
  CliApp,
  Favorite,
  MemoryTool,
  Project,
  Provider,
  Gateway,
  Route,
  ScanResult,
  SearchHit,
  SessionDetail,
  SessionMessage
} from "@/types";
import { formatDate, formatRelativeDate } from "@/lib/format";
import {
  basename,
  isMemoryItem,
  isSessionItem,
  isSkillItem,
  memoryToolsOf,
  projectDisplayName,
  readRouteFromHash,
  resumeCommandFor,
  sessionKey,
  sourceDisplayName
} from "@/lib/session-utils";
import { isProviderList, isGatewayList } from "@/lib/provider-utils";
import {
  favoriteKeySet,
  isFavoriteList,
  toggleFavoriteEntry
} from "@/lib/favorites";
import { addSetValue, toggleSetValue } from "@/lib/set-utils";
import { RAIL_ROUTE_ORDER } from "@/constants";
import { usePersistentState } from "@/hooks/usePersistentState";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger
} from "@/components/ui/context-menu";
import {
  runClaudeMigration,
  runRecordDelete,
  type MigrateResult
} from "@/lib/migrate";
import {
  remappedPath,
  reconcileTombstones,
  projectKey,
  projectDirOf,
  recordUnderFolder,
  withProject
} from "@/lib/records";
import { ActivityRail } from "@/components/ActivityRail";
import { ListItemMenu } from "@/components/ListItemMenu";
import { BrandIcon } from "@/components/BrandIcon";
import { CommandPalette } from "@/components/CommandPalette";
import { CopyMenu } from "@/components/CopyMenu";
import { EmptyState } from "@/components/EmptyState";
import { FreshnessFooter } from "@/components/FreshnessFooter";
import { SessionCardSkeletonList } from "@/components/SessionCardSkeleton";
import { MemoryCard } from "@/components/MemoryCard";
import { MessageBody } from "@/components/MessageBody";
import { MessageList } from "@/components/MessageList";
import { SnippetLine } from "@/components/SnippetLine";
import { useT } from "@/i18n";

// Route + modal code-splitting (M6). Each lazy chunk only ships when
// its surface mounts: Providers / Search / Settings are gated on the
// active rail route; UpdateDialog only mounts when an update is
// actually found. Splitting Settings also frees `@tauri-apps/plugin-
// updater` to land in its own chunk (was previously pinned to the
// main bundle by Settings' static import).
const ProvidersPage = React.lazy(() =>
  import("@/components/providers/ProvidersPage").then((m) => ({
    default: m.ProvidersPage
  }))
);
const SearchPage = React.lazy(() =>
  import("@/components/search/SearchPage").then((m) => ({
    default: m.SearchPage
  }))
);
const StatsPage = React.lazy(() =>
  import("@/components/stats/StatsPage").then((m) => ({
    default: m.StatsPage
  }))
);
const FavoritesPage = React.lazy(() =>
  import("@/components/favorites/FavoritesPage").then((m) => ({
    default: m.FavoritesPage
  }))
);
const SettingsPage = React.lazy(() =>
  import("@/components/settings/SettingsPage").then((m) => ({
    default: m.SettingsPage
  }))
);
const UpdateDialog = React.lazy(() =>
  import("@/components/UpdateDialog").then((m) => ({
    default: m.UpdateDialog
  }))
);

export function App() {
  // Force a re-render every minute so `formatRelativeDate` ("2 min
  // ago" / "Synced 3 min ago" / favorited-at chips) stays accurate
  // without each consumer needing its own timer. Cheap: a noop reducer
  // bumps state once a minute, React reconciles the (mostly static)
  // tree, every formatRelativeDate call re-evaluates against the new
  // wall clock.
  const [, tickRelativeTime] = React.useReducer((n: number) => n + 1, 0);
  React.useEffect(() => {
    const id = setInterval(() => tickRelativeTime(), 60_000);
    return () => clearInterval(id);
  }, []);

  const t = useT();
  // Keep the macOS tray's static rows (Open / Official / Exit) on the app
  // language — re-pushed whenever `t` (the locale) changes. Backend stores the
  // strings + rebuilds the menu; no-ops where there's no tray.
  React.useEffect(() => {
    void invoke("set_tray_labels", {
      open: t("tray.open"),
      official: t("tray.official"),
      exit: t("tray.exit"),
      // Quota window labels on the CLI rows (5h / Weekly / Monthly).
      fiveHour: t("providers.quotaFiveHourShort"),
      weekly: t("providers.quotaSevenDayShort"),
      monthly: t("providers.quotaMonthlyShort"),
      // "New Session" submenu title + its "Choose Folder…" tail.
      newSession: t("tray.newSession"),
      chooseFolder: t("tray.chooseFolder")
    }).catch(() => {});
  }, [t]);
  const [sessions, setSessions] = React.useState<AppSession[]>([]);
  // First-class projects (folders/entities), independent of records. The
  // sidebar renders from this; deleting a record never removes a project here.
  const [projects, setProjects] = React.useState<Project[]>([]);
  const [selected, setSelected] = React.useState<AppSession | null>(null);
  const [detail, setDetail] = React.useState<SessionDetail | null>(null);
  const [query, setQuery] = React.useState("");
  const [source, setSource] = React.useState("All");
  const [project, setProject] = React.useState<string | null>(null);
  const [expandedSources, setExpandedSources] = React.useState<Set<string>>(() => new Set());
  // Starts true because we kick off a background scan on mount —
  // FreshnessFooter shows "Syncing…" briefly even when the user lands
  // on Providers, then flips to "Synced just now" when the scan
  // returns. Sessions are pre-loaded for Records / Stats / Search.
  const [loading, setLoading] = React.useState(true);
  const [detailLoading, setDetailLoading] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [contentHits, setContentHits] = React.useState<Map<string, SearchHit>>(() => new Map());
  const [, setSearchingContent] = React.useState(false);
  const [contentQuery, setContentQuery] = React.useState("");
  const [pane, setPane] = usePersistentState<"sessions" | "memory" | "skills">(
    "default_pane",
    "sessions",
    (raw): raw is "sessions" | "memory" | "skills" =>
      raw === "sessions" || raw === "memory" || raw === "skills"
  );
  const [recentSearches, setRecentSearches] = usePersistentState<string[]>(
    "recent_searches",
    [],
    (raw): raw is string[] =>
      Array.isArray(raw) && raw.every((entry) => typeof entry === "string")
  );
  const [providers, setProviders] = usePersistentState<Provider[]>(
    "providers",
    [],
    isProviderList
  );
  const [gateways, setGateways] = usePersistentState<Gateway[]>(
    "gateways",
    [],
    isGatewayList
  );
  // Per-CLI marker: the provider / gateway-binding id the user last
  // activated through Termory. Disambiguates entries that share identical
  // base+key on disk (a standalone provider vs a gateway binding). Only a
  // hint — validated against the live config before it's trusted.
  const [activeProviderIds, setActiveProviderIds] = usePersistentState<
    Record<string, string>
  >(
    "active_provider_ids",
    {},
    (raw): raw is Record<string, string> =>
      !!raw && typeof raw === "object" && !Array.isArray(raw)
  );
  const [providersApp, setProvidersApp] = usePersistentState<CliApp>(
    "providers_app",
    "claude",
    (raw): raw is CliApp =>
      raw === "claude" || raw === "codex" || raw === "gemini" || raw === "opencode"
  );
  const [autoCheckUpdates, setAutoCheckUpdates] = usePersistentState<boolean>(
    "auto_check_updates",
    true,
    (raw): raw is boolean => typeof raw === "boolean"
  );
  const [favorites, setFavorites] = usePersistentState<Favorite[]>(
    "favorites",
    [],
    isFavoriteList
  );
  // Derived: fast "is this (source, session, index) in favorites?"
  // lookup. Set is rebuilt only when the array reference changes.
  const favoriteKeys = React.useMemo(
    () => favoriteKeySet(favorites),
    [favorites]
  );

  const addRecentSearch = React.useCallback(
    (raw: string) => {
      const trimmed = raw.trim();
      if (trimmed.length < 2) return;
      setRecentSearches((current) => {
        const filtered = current.filter((entry) => entry !== trimmed);
        return [trimmed, ...filtered].slice(0, 5);
      });
    },
    [setRecentSearches]
  );

  const clearRecentSearches = React.useCallback(() => {
    setRecentSearches([]);
  }, [setRecentSearches]);

  const toggleFavorite = React.useCallback(
    (session: AppSession, message: SessionMessage, index: number) => {
      setFavorites((current) =>
        toggleFavoriteEntry(current, session, message, index)
      );
    },
    [setFavorites]
  );
  const removeFavorite = React.useCallback(
    (id: string) => {
      setFavorites((current) => current.filter((f) => f.id !== id));
    },
    [setFavorites]
  );
  const [route, setRouteImmediate] = React.useState<Route>(() => readRouteFromHash());
  const [, startTransition] = React.useTransition();
  const setRoute = React.useCallback(
    (next: Route) => {
      startTransition(() => setRouteImmediate(next));
    },
    []
  );
  const [lastRefreshedAt, setLastRefreshedAt] = React.useState<number | null>(null);

  // Sync route ↔ URL hash so refresh / back-forward / deeplink work.
  React.useEffect(() => {
    const wanted = `#${route}`;
    if (window.location.hash !== wanted) {
      window.history.replaceState(null, "", wanted);
    }
  }, [route]);
  React.useEffect(() => {
    const onHashChange = () => setRoute(readRouteFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  // ⌘1..6 (or Ctrl 1..6) switch rail routes by visual order:
  // 1=Providers, 2=Records, 3=Favorites, 4=Search, 5=Stats, 6=Settings.
  React.useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey)) return;
      const index = Number(event.key) - 1;
      if (!Number.isInteger(index) || index < 0 || index >= RAIL_ROUTE_ORDER.length) return;
      event.preventDefault();
      setRoute(RAIL_ROUTE_ORDER[index]);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  // Auto-check for updates on app launch when enabled. Delayed a few
  // seconds so the persistent state has time to load (user might have
  // it disabled) and so the network call doesn't fight with the
  // initial scan_all_sessions. Found update pops the UpdateDialog;
  // user clicks Install or Later.
  const [pendingUpdate, setPendingUpdate] = React.useState<Update | null>(null);
  const [appVersion, setAppVersion] = React.useState("");
  React.useEffect(() => {
    void getVersion().then(setAppVersion).catch(() => {});
  }, []);
  const autoCheckRef = React.useRef(autoCheckUpdates);
  React.useEffect(() => {
    autoCheckRef.current = autoCheckUpdates;
  }, [autoCheckUpdates]);
  React.useEffect(() => {
    let cancelled = false;
    const timer = window.setTimeout(() => {
      if (!autoCheckRef.current) return;
      void import("@tauri-apps/plugin-updater")
        .then(({ check }) => check())
        .then((update) => {
          if (cancelled || !update) return;
          setPendingUpdate(update);
        })
        .catch(() => {
          /* silent — manual check surfaces the error */
        });
    }, 3000);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, []);

  // Pending scroll request — used when "Open original" on a Favorite
  // wants to land on a specific message. Lives at App level because
  // the navigation flow crosses route change + async detail load +
  // MessageList mount. The nonce lets repeat clicks on the same
  // (session, index) still re-trigger a scroll.
  const [pendingScroll, setPendingScroll] = React.useState<{
    sessionKey: string;
    index: number;
    nonce: number;
  } | null>(null);
  // Stable handle — passed to MessageList so the effect dep array
  // doesn't refire on every parent re-render.
  const clearPendingScroll = React.useCallback(
    () => setPendingScroll(null),
    []
  );

  // Shared "jump to this record" action — used by Search, Cmd-K, and
  // the Favorites "Open original" button. Resets the Records sidebar
  // filters so the target item is visible in its pane and switches
  // the pane tab to match the item type before flipping the route.
  // When `messageIndex` is supplied, the records detail pane will
  // scroll that message into view as soon as it finishes loading.
  const openItem = React.useCallback(
    (item: AppSession, messageIndex?: number) => {
      setSource("All");
      setProject(null);
      if (isMemoryItem(item)) setPane("memory");
      else if (isSkillItem(item)) setPane("skills");
      else setPane("sessions");
      setSelected(item);
      if (messageIndex != null && messageIndex >= 0) {
        setPendingScroll({
          sessionKey: `${item.source}::${item.id}`,
          index: messageIndex,
          nonce: Date.now()
        });
      }
      setRoute("records");
    },
    [setPane]
  );

  // Records / projects just deleted/migrated locally. A background re-scan that
  // predates the mutation (stale, still in-flight) would otherwise re-add them
  // and clobber the optimistic update — so we hide tombstoned keys until a scan
  // confirms they're gone, then drop the tombstone. Records key by path,
  // projects by source+cwd.
  const tombstonesRef = React.useRef<Set<string>>(new Set());
  const projectTombstonesRef = React.useRef<Set<string>>(new Set());

  const applyScanResult = React.useCallback((incoming: ScanResult) => {
    const records = reconcileTombstones(
      incoming.records,
      tombstonesRef.current,
      // Key by full identity, NOT path: DB-backed sources (OpenCode) share one
      // db-file path across all sessions, so a path-keyed tombstone would hide
      // every sibling record. sessionKey is source:path:id.
      (s) => sessionKey(s)
    );
    const projs = reconcileTombstones(
      incoming.projects,
      projectTombstonesRef.current,
      projectKey
    );
    setSessions(records);
    setProjects(projs);
    setSelected((current) => {
      if (!current) return null;
      return (
        records.find(
          (session) =>
            session.source === current.source &&
            session.path === current.path &&
            session.id === current.id
        ) ?? null
      );
    });
    setLastRefreshedAt(Date.now());
  }, []);

  const refresh = React.useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<ScanResult>("scan_all_sessions");
      applyScanResult(result);
    } catch (err) {
      console.error("scan_all_sessions failed", err);
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [applyScanResult]);

  // After a migrate/delete, mutate the in-memory lists directly instead of
  // re-scanning every CLI. Deleting a record only removes the record — its
  // project stays in `projects` (it's first-class, the folder still exists), so
  // deleting a project's last session leaves it showing as an empty project.
  // `removeProject` is passed ONLY for a whole-project delete (the folder is
  // gone), which also drops the project from `projects`.
  const removeRecordsLocally = React.useCallback(
    (
      match: (s: AppSession) => boolean,
      removeProject?: { source: string; project: string }
    ) => {
      setSessions((prev) => {
        for (const s of prev)
          if (match(s)) tombstonesRef.current.add(sessionKey(s));
        return prev.filter((s) => !match(s));
      });
      if (removeProject) {
        const k = projectKey(removeProject);
        projectTombstonesRef.current.add(k);
        setProjects((prev) => prev.filter((p) => projectKey(p) !== k));
      }
      setSelected((cur) => (cur && match(cur) ? null : cur));
    },
    []
  );
  // Migrate (move). Matched records are re-pointed to the new project.
  // `opts.newProjectSource` (a CLI source) adds the destination project to
  // `projects` — set it only when a *session* moves there (a project shows
  // because it has a session; a lone migrated memory wouldn't, so the scan
  // wouldn't list it and adding it here would flicker). `opts.removeProject`
  // (whole-project migrate, move mode) drops the old project — its folder moved
  // away; a single-record migrate omits it so the source project stays.
  const remapRecordsLocally = React.useCallback(
    (
      match: (s: AppSession) => boolean,
      res: MigrateResult,
      opts: {
        newProjectSource?: string;
        removeProject?: { source: string; project: string };
      } = {}
    ) => {
      const remap = (s: AppSession): AppSession => ({
        ...s,
        project: res.new_project,
        path: remappedPath(s.path, res.old_dir, res.new_dir)
      });
      setSessions((prev) => {
        for (const s of prev)
          if (match(s)) tombstonesRef.current.add(sessionKey(s));
        return prev.map((s) => (match(s) ? remap(s) : s));
      });
      setProjects((prev) => {
        let next = prev;
        if (opts.removeProject) {
          const k = projectKey(opts.removeProject);
          projectTombstonesRef.current.add(k);
          next = next.filter((p) => projectKey(p) !== k);
        }
        if (opts.newProjectSource) {
          const newProj = {
            source: opts.newProjectSource,
            project: res.new_project
          };
          projectTombstonesRef.current.delete(projectKey(newProj));
          next = withProject(next, newProj);
        }
        return next;
      });
      setSelected((cur) => (cur && match(cur) ? remap(cur) : cur));
    },
    []
  );

  // Match EVERY record stored in a project's CLI folder (the slug/hash dir) —
  // its sessions AND its auto-memory — so delete/migrate-project drop all of
  // them from display (the backend removed/moved the whole folder). The folder
  // is found from a session, or — for a project with only memory left — from
  // the auto-memory under it (path under the CLI's project tree + `/memory/`).
  // A cwd-level CLAUDE.md/AGENTS.md lives OUTSIDE this folder, so it's never
  // matched (the backend never deletes it; it keeps showing).
  const projectRecordMatch = React.useCallback(
    (source: string, project: string) => {
      const marker = source === "Gemini" ? "/.gemini/tmp/" : "/projects/";
      const anyRecord =
        sessions.find((s) => s.source === source && s.project === project) ??
        sessions.find(
          (s) =>
            s.project === project &&
            s.path.includes(marker) &&
            s.path.includes("/memory/")
        );
      if (anyRecord) {
        return recordUnderFolder(projectDirOf(anyRecord.path));
      }
      return (s: AppSession) => s.source === source && s.project === project;
    },
    [sessions]
  );

  // Everything that SHOWS under a project (source S, cwd C): its sessions, plus
  // the memory/skills at that cwd tagged for S — the auto-memory AND the cwd's
  // own CLAUDE.md / AGENTS.md / skills. Delete-project drops ALL of it from
  // display ("project gone → no data"). The backend only removed the slug
  // folder, so the cwd's own files stay on disk but stop showing here.
  const projectDataMatch = React.useCallback(
    (source: string, project: string) =>
      (r: AppSession): boolean =>
        r.project === project &&
        (r.source === source ||
          ((isMemoryItem(r) || isSkillItem(r)) &&
            memoryToolsOf(r).includes(source as MemoryTool))),
    []
  );

  React.useEffect(() => {
    const unlisten = listen<ScanResult>("termory:sources-changed", (event) => {
      setError(null);
      applyScanResult(event.payload);
    });
    return () => {
      void unlisten.then((fn) => fn()).catch(() => {});
    };
  }, [applyScanResult]);

  // Background scan on mount — runs regardless of landing route so
  // sessions are pre-loaded by the time the user navigates to
  // Records / Stats / Search. Non-blocking; FreshnessFooter shows
  // "Syncing…" while it runs.
  React.useEffect(() => {
    refresh();
  }, [refresh]);

  const prevSelectedKeyRef = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (!selected) {
      setDetail(null);
      prevSelectedKeyRef.current = null;
      return;
    }
    // Identity = (source, id). Path used to be in here but the
    // backend now looks the path up itself from the scan index, so
    // including path would be misleading — it's display data only.
    const identity = `${selected.source}::${selected.id}`;
    const isNewSelection = prevSelectedKeyRef.current !== identity;
    prevSelectedKeyRef.current = identity;

    let cancelled = false;
    if (isNewSelection) setDetailLoading(true);
    invoke<SessionDetail>("load_session", {
      source: selected.source,
      id: selected.id
    })
      .then((result) => {
        if (cancelled) return;
        React.startTransition(() => setDetail(result));
      })
      .catch((err) => {
        if (!cancelled) {
          console.error("load_session failed", err);
          setError(String(err));
        }
      })
      .finally(() => {
        if (!cancelled && isNewSelection) setDetailLoading(false);
      });
    return () => {
      cancelled = true;
    };
    // Re-fetch when identity (source/id) changes — show loading.
    // Also re-fetch when the selection's mtime / message_count advances
    // (watcher-driven content update) — silently swap, no spinner.
    // message_count is included because some sources don't populate
    // updated_at, so it's the more reliable content-change signal.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    selected?.source,
    selected?.id,
    selected?.updated_at,
    selected?.message_count
  ]);

  React.useEffect(() => {
    const trimmed = query.trim();
    if (trimmed.length < 2) {
      setContentHits(new Map());
      setContentQuery("");
      setSearchingContent(false);
      return;
    }
    let cancelled = false;
    const handle = window.setTimeout(() => {
      setSearchingContent(true);
      invoke<SearchHit[]>("search_all_sessions", { query: trimmed })
        .then((hits) => {
          if (cancelled) return;
          const map = new Map<string, SearchHit>();
          for (const hit of hits) map.set(sessionKey(hit.session), hit);
          setContentHits(map);
          setContentQuery(trimmed);
        })
        .catch((err) => {
          if (!cancelled) {
            console.error("search_all_sessions failed", err);
            setError(String(err));
          }
        })
        .finally(() => {
          if (!cancelled) setSearchingContent(false);
        });
    }, 300);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
  }, [query]);

  const sessionItems = React.useMemo(
    () => sessions.filter((item) => !isMemoryItem(item) && !isSkillItem(item)),
    [sessions]
  );
  const memoryItems = React.useMemo(() => sessions.filter(isMemoryItem), [sessions]);
  const skillItems = React.useMemo(() => sessions.filter(isSkillItem), [sessions]);

  const filtered = React.useMemo(() => {
    const needle = query.trim().toLowerCase();
    return sessionItems.filter((session) => {
      if (source !== "All" && session.source !== source) return false;
      if (project && session.project !== project) return false;
      if (!needle) return true;
      const metaMatch = [
        session.title,
        session.project,
        session.source,
        session.id,
        session.path
      ]
        .join("\n")
        .toLowerCase()
        .includes(needle);
      if (metaMatch) return true;
      if (needle === contentQuery.toLowerCase() && contentHits.has(sessionKey(session))) {
        return true;
      }
      return false;
    });
  }, [sessionItems, query, source, project, contentHits, contentQuery]);

  const filteredMemories = React.useMemo(() => {
    const needle = query.trim().toLowerCase();
    return memoryItems.filter((memory) => {
      if (source !== "All") {
        const tools = memoryToolsOf(memory);
        if (!tools.includes(source as MemoryTool)) return false;
      }
      if (project && memory.project !== project) return false;
      if (!needle) return true;
      const metaMatch = [memory.title, memory.project, memory.id, memory.path]
        .join("\n")
        .toLowerCase()
        .includes(needle);
      if (metaMatch) return true;
      if (needle === contentQuery.toLowerCase() && contentHits.has(sessionKey(memory))) {
        return true;
      }
      return false;
    });
  }, [memoryItems, query, source, project, contentHits, contentQuery]);

  const filteredSkills = React.useMemo(() => {
    const needle = query.trim().toLowerCase();
    return skillItems.filter((skill) => {
      if (source !== "All") {
        const tools = memoryToolsOf(skill);
        if (!tools.includes(source as MemoryTool)) return false;
      }
      if (project && skill.project !== project) return false;
      if (!needle) return true;
      const metaMatch = [skill.title, skill.project, skill.id, skill.path]
        .join("\n")
        .toLowerCase()
        .includes(needle);
      if (metaMatch) return true;
      if (needle === contentQuery.toLowerCase() && contentHits.has(sessionKey(skill))) {
        return true;
      }
      return false;
    });
  }, [skillItems, query, source, project, contentHits, contentQuery]);

  const hasActiveFilters =
    source !== "All" || project !== null || query.trim().length > 0;

  const sourceGroups = React.useMemo(() => {
    const sources: string[] = ["All", "Codex", "Claude", "Gemini", "OpenCode"];
    // Session count per (source, cwd) — empty projects simply get 0.
    const counts = new Map<string, number>();
    for (const session of sessionItems) {
      if (!session.project) continue;
      const key = `${session.source}\n${session.project}`;
      counts.set(key, (counts.get(key) ?? 0) + 1);
    }
    return sources.map((item) => {
      const sourceSessions =
        item === "All"
          ? sessionItems
          : sessionItems.filter((session) => session.source === item);
      // Project list comes from the first-class `projects` (so empty projects
      // show), not from grouping records — deleting a project's last record
      // can't make it vanish here.
      const projectList: [string, number][] =
        item === "All"
          ? []
          : projects
              .filter((p) => p.source === item)
              .map(
                (p) =>
                  [p.project, counts.get(`${p.source}\n${p.project}`) ?? 0] as [
                    string,
                    number
                  ]
              )
              .sort(([left], [right]) => left.localeCompare(right));

      return {
        source: item,
        count: sourceSessions.length,
        projects: projectList
      };
    });
  }, [sessionItems, projects]);

  return (
    <div className="relative grid grid-rows-[1fr_auto] w-full h-screen text-foreground bg-background">
      <div className="grid grid-cols-[63px_1fr] min-h-0 min-w-0">
        <ActivityRail route={route} onChange={setRoute} />
        <div className="min-w-0 min-h-0 flex flex-col mb-3">
          {/* Suspense fallback is intentionally empty — route chunks
              are small (<50KB gzipped each) and load near-instantly
              on a warm app; an explicit spinner would just flash. */}
          <React.Suspense fallback={null}>
            {route === "search" && (
              <SearchPage
                sessions={sessions}
                onOpenItem={openItem}
                recentSearches={recentSearches}
                onCommitSearch={addRecentSearch}
                onClearRecent={clearRecentSearches}
              />
            )}
            {route === "providers" && (
              <ProvidersPage
                providers={providers}
                setProviders={setProviders}
                gateways={gateways}
                setGateways={setGateways}
                activeProviderIds={activeProviderIds}
                setActiveProviderIds={setActiveProviderIds}
                app={providersApp}
                setApp={setProvidersApp}
              />
            )}
            {route === "settings" && (
              <SettingsPage
                recentSearches={recentSearches}
                onClearRecent={clearRecentSearches}
                autoCheckUpdates={autoCheckUpdates}
                onAutoCheckUpdatesChange={setAutoCheckUpdates}
                appVersion={appVersion}
                onUpdateFound={(update) => setPendingUpdate(update)}
              />
            )}
            {route === "stats" && (
              <StatsPage
                sessions={sessions}
                onRefresh={refresh}
                refreshing={loading}
              />
            )}
            {route === "favorites" && (
              <FavoritesPage
                favorites={favorites}
                sessions={sessions}
                onOpenSource={(session, messageIndex) =>
                  openItem(session, messageIndex)
                }
                onRemove={removeFavorite}
              />
            )}
          </React.Suspense>
          {route === "records" && (
            <main className="flex-1 min-h-0 grid grid-cols-[220px_minmax(240px,300px)_1fr]">
              <aside className="flex flex-col min-h-0 bg-sidebar mt-3 ml-3 rounded-md">
                <div className="flex-1 min-h-0 overflow-auto p-3 flex flex-col">
                  {sourceGroups.flatMap((group) => {
                    const groupActive = source === group.source && !project;
                    const isExpanded = expandedSources.has(group.source);
                    const hasProjects = group.projects.length > 0;

                    const rows: React.ReactNode[] = [];

                    rows.push(
                      <div
                        key={`source:${group.source}`}
                        role="button"
                        tabIndex={0}
                        aria-current={groupActive ? "page" : undefined}
                        aria-label={`${sourceDisplayName(group.source)} (${group.count})`}
                        onClick={() => {
                          setSource(group.source);
                          setProject(null);
                        }}
                        onKeyDown={(event) => {
                          if (event.key !== "Enter" && event.key !== " ") return;
                          event.preventDefault();
                          setSource(group.source);
                          setProject(null);
                        }}
                        className={cn(
                          "h-8 flex items-center gap-2 px-2 rounded-lg text-base cursor-pointer transition-colors shrink-0",
                          groupActive
                            ? "bg-primary text-primary-foreground"
                            : "text-sidebar-foreground hover:bg-accent/60"
                        )}
                      >
                        <button
                          type="button"
                          aria-label={hasProjects ? `${isExpanded ? "Collapse" : "Expand"} ${group.source} projects` : undefined}
                          disabled={!hasProjects}
                          onClick={(event) => {
                            event.stopPropagation();
                            setExpandedSources((current) =>
                              toggleSetValue(current, group.source)
                            );
                          }}
                          className="flex items-center justify-center size-4 shrink-0 disabled:opacity-0 disabled:pointer-events-none"
                        >
                          <ChevronRight
                            size={14}
                            className={cn("block", isExpanded && "rotate-90")}
                          />
                        </button>
                        <span className="flex items-center justify-center size-4 shrink-0">
                          {group.source === "All" ? (
                            <Layers className="size-4" aria-hidden />
                          ) : (
                            <BrandIcon source={group.source} />
                          )}
                        </span>
                        <span className="flex-1 min-w-0 truncate font-medium text-base">
                          {group.source === "All" ? t("common.all") : sourceDisplayName(group.source)}
                        </span>
                        <span
                          className={cn(
                            "shrink-0 text-[10px] tabular-nums rounded-full px-1.5 leading-[1.4]",
                            groupActive ? "bg-primary-foreground/20" : "bg-foreground/10"
                          )}
                        >
                          {group.count}
                        </span>
                      </div>
                    );

                    if (hasProjects && isExpanded) {
                      for (const [projectName, count] of group.projects) {
                        const projActive =
                          source === group.source && project === projectName;
                        const projKey = `project:${group.source}:${projectName}`;
                        const projRow = (
                          <div
                            key={projKey}
                            role="button"
                            tabIndex={0}
                            aria-current={projActive ? "page" : undefined}
                            onClick={() => {
                              setSource(group.source);
                              setProject(projectName);
                              setExpandedSources((current) =>
                                addSetValue(current, group.source)
                              );
                            }}
                            onKeyDown={(event) => {
                              if (event.key !== "Enter" && event.key !== " ") return;
                              event.preventDefault();
                              setSource(group.source);
                              setProject(projectName);
                            }}
                            className={cn(
                              "h-7 flex items-center gap-2 pl-8 pr-2 rounded-lg text-base cursor-pointer transition-colors shrink-0",
                              projActive
                                ? "bg-primary text-primary-foreground"
                                : "text-foreground/70 hover:bg-accent/60 hover:text-foreground"
                            )}
                          >
                            <span className="flex items-center justify-center size-4 shrink-0">
                              <Folder size={14} className="block" />
                            </span>
                            <span className="flex-1 min-w-0 truncate">
                              {projectDisplayName(projectName)}
                            </span>
                            <span
                              className={cn(
                                "shrink-0 text-[10px] tabular-nums rounded-full px-1.5 leading-[1.4]",
                                projActive ? "bg-primary-foreground/20" : "bg-foreground/10"
                              )}
                            >
                              {count}
                            </span>
                          </div>
                        );
                        // Claude / Gemini / Codex / OpenCode project rows get a
                        // right-click menu. Delete is available for all four;
                        // whole-project migration is Claude-only for now.
                        rows.push(
                          group.source === "Claude" ||
                          group.source === "Gemini" ||
                          group.source === "Codex" ||
                          group.source === "OpenCode" ? (
                            <ContextMenu key={projKey}>
                              <ContextMenuTrigger asChild>
                                {projRow}
                              </ContextMenuTrigger>
                              <ContextMenuContent className="w-56">
                                {/* Migrate is Claude-only and pointless for an
                                    empty project (count 0) — delete only there. */}
                                {group.source === "Claude" && count > 0 && (
                                  <ContextMenuItem
                                    onSelect={() =>
                                      void runClaudeMigration(
                                        "migrate_claude_project",
                                        { oldPath: projectName },
                                        t,
                                        (res) =>
                                          remapRecordsLocally(
                                            projectRecordMatch(
                                              group.source,
                                              projectName
                                            ),
                                            res,
                                            {
                                              newProjectSource: group.source,
                                              removeProject: {
                                                source: group.source,
                                                project: projectName
                                              }
                                            }
                                          )
                                      )
                                    }
                                  >
                                    {t("menu.migrateProject")}
                                  </ContextMenuItem>
                                )}
                                <ContextMenuItem
                                  variant="destructive"
                                  onSelect={() =>
                                    void runRecordDelete(
                                      group.source === "Gemini"
                                        ? "delete_gemini_project"
                                        : group.source === "Codex"
                                          ? "delete_codex_project"
                                          : group.source === "OpenCode"
                                            ? "delete_opencode_project"
                                            : "delete_claude_project",
                                      { project: projectName },
                                      projectDisplayName(projectName),
                                      t,
                                      () =>
                                        removeRecordsLocally(
                                          // Project gone → drop ALL its data
                                          // (sessions + auto-memory + the cwd's
                                          // own CLAUDE.md/skills) from display.
                                          projectDataMatch(
                                            group.source,
                                            projectName
                                          ),
                                          {
                                            source: group.source,
                                            project: projectName
                                          }
                                        )
                                    )
                                  }
                                >
                                  {t("menu.deleteProject")}
                                </ContextMenuItem>
                              </ContextMenuContent>
                            </ContextMenu>
                          ) : (
                            projRow
                          )
                        );
                      }
                    }

                    return rows;
                  })}
                </div>
              </aside>

              <section className="flex flex-col min-h-0">
                <div className="p-3">
                  <Tabs
                    value={pane}
                    onValueChange={(v) => setPane(v as "sessions" | "memory" | "skills")}
                  >
                    <TabsList className="w-full gap-1 bg-transparent p-0 [&>button]:flex-1 [&>button]:rounded-full [&>button]:px-3 [&>button]:bg-muted [&>button]:whitespace-nowrap">
                      <TabsTrigger value="sessions">{t("records.pane.sessions")}</TabsTrigger>
                      <TabsTrigger value="memory">{t("records.pane.memories")}</TabsTrigger>
                      <TabsTrigger value="skills">{t("records.pane.skills")}</TabsTrigger>
                    </TabsList>
                  </Tabs>
                </div>

                {pane === "sessions" && (
                  <div className="flex-1 min-h-0 overflow-auto px-3 flex flex-col gap-2">
                    {loading && sessionItems.length === 0 && (
                      <SessionCardSkeletonList count={6} />
                    )}
                    {!loading && filtered.length === 0 && sessionItems.length === 0 && (
                      <EmptyState
                        icon={<FileJson size={32} />}
                        title={t("records.noSessions")}
                        description={t("records.noSessionsDesc")}
                      />
                    )}
                    {!loading && filtered.length === 0 && sessionItems.length > 0 && (
                      <EmptyState
                        icon={<FileJson size={32} />}
                        title={t("records.noSessionsMatch")}
                        description={
                          hasActiveFilters
                            ? t("records.tryFilters")
                            : t("records.nothingMatches")
                        }
                        action={undefined}
                      />
                    )}
                    {(!loading || sessionItems.length > 0) &&
                      filtered.map((session) => {
                        const hit = contentHits.get(sessionKey(session));
                        const showSnippet =
                          !!hit && query.trim().toLowerCase() === contentQuery.toLowerCase();
                        const isActive =
                          selected?.path === session.path && selected?.id === session.id;
                        return (
                          <ListItemMenu
                            key={sessionKey(session)}
                            path={session.path}
                            id={session.id}
                            source={session.source}
                            project={session.project}
                            onLocalDelete={() =>
                              // Match by full identity, NOT path: OpenCode (and
                              // any DB-backed source) shares one db-file path
                              // across all its sessions, so a path match would
                              // wrongly clear the whole list. sessionKey is
                              // source:path:id — the id disambiguates.
                              removeRecordsLocally(
                                (s) => sessionKey(s) === sessionKey(session)
                              )
                            }
                            onLocalMigrate={(res) =>
                              remapRecordsLocally(
                                (s) => s.path === session.path,
                                res,
                                // A migrated session lands in (and surfaces) the
                                // destination project.
                                { newProjectSource: session.source }
                              )
                            }
                          >
                          <button
                            onClick={() => setSelected(session)}
                            className={cn(
                              "w-full text-left rounded-lg px-2 py-2 flex flex-col gap-1 select-none",
                              isActive
                                ? "bg-primary text-primary-foreground [&_*]:text-primary-foreground"
                                : "hover:bg-accent/60 data-[state=open]:bg-accent/60"
                            )}
                          >
                            <div className="flex items-baseline justify-between gap-2">
                              <h2 className="text-base font-medium leading-snug line-clamp-2 flex-1 min-w-0">
                                {session.title}
                              </h2>
                              <span className="text-xs text-muted-foreground shrink-0">
                                {formatRelativeDate(session.updated_at ?? session.started_at, t)}
                              </span>
                            </div>
                            <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
                              <span className="flex items-center gap-1 min-w-0">
                                <Folder size={12} className="shrink-0" />
                                <span className="truncate">
                                  {projectDisplayName(session.project)}
                                </span>
                              </span>
                              <span className="flex items-center gap-2 shrink-0">
                                <span className="flex items-center gap-1">
                                  <MessageSquare size={11} />
                                  <span className="tabular-nums">{session.message_count}</span>
                                </span>
                                {source === "All" && (
                                  <span aria-label={sourceDisplayName(session.source)}>
                                    <BrandIcon source={session.source} />
                                  </span>
                                )}
                              </span>
                            </div>
                            {showSnippet && hit && (
                              <SnippetLine
                                snippet={hit.snippet}
                                query={query.trim()}
                                role={hit.role}
                                matchCount={hit.match_count}
                                truncated={hit.truncated}
                              />
                            )}
                          </button>
                          </ListItemMenu>
                        );
                      })}
                  </div>
                )}

                {pane === "memory" && (
                  <div className="flex-1 min-h-0 overflow-auto px-3 flex flex-col gap-2">
                    {loading && memoryItems.length === 0 && (
                      <SessionCardSkeletonList count={6} />
                    )}
                    {!loading && filteredMemories.length === 0 && memoryItems.length === 0 && (
                      <EmptyState
                        icon={<BookOpen size={32} />}
                        title={t("records.noMemory")}
                        description={t("records.noMemoryDesc")}
                      />
                    )}
                    {!loading && filteredMemories.length === 0 && memoryItems.length > 0 && (
                      <EmptyState
                        icon={<BookOpen size={32} />}
                        title={t("records.noMemoryMatch")}
                        description={
                          hasActiveFilters
                            ? t("records.tryFilters")
                            : t("records.nothingMatches")
                        }
                        action={undefined}
                      />
                    )}
                    {filteredMemories.map((item) => (
                      <ListItemMenu
                        key={sessionKey(item)}
                        path={item.path}
                        project={item.project}
                        onLocalDelete={() =>
                          removeRecordsLocally((s) => s.path === item.path)
                        }
                        onLocalMigrate={(res) =>
                          remapRecordsLocally((s) => s.path === item.path, res)
                        }
                      >
                        <MemoryCard
                          item={item}
                          selected={selected}
                          onClick={() => setSelected(item)}
                          query={query.trim()}
                          contentQuery={contentQuery}
                          hit={contentHits.get(sessionKey(item))}
                          showSource={source === "All"}
                        />
                      </ListItemMenu>
                    ))}
                  </div>
                )}

                {pane === "skills" && (
                  <div className="flex-1 min-h-0 overflow-auto px-3 flex flex-col gap-2">
                    {loading && skillItems.length === 0 && (
                      <SessionCardSkeletonList count={6} />
                    )}
                    {!loading && filteredSkills.length === 0 && skillItems.length === 0 && (
                      <EmptyState
                        icon={<Sparkles size={32} />}
                        title={t("records.noSkills")}
                        description={t("records.noSkillsDesc")}
                      />
                    )}
                    {!loading && filteredSkills.length === 0 && skillItems.length > 0 && (
                      <EmptyState
                        icon={<Sparkles size={32} />}
                        title={t("records.noSkillsMatch")}
                        description={
                          hasActiveFilters
                            ? t("records.tryFilters")
                            : t("records.nothingMatches")
                        }
                        action={undefined}
                      />
                    )}
                    {filteredSkills.map((item) => (
                      <ListItemMenu
                        key={sessionKey(item)}
                        path={item.path}
                        project={item.project}
                        onLocalDelete={() =>
                          removeRecordsLocally((s) => s.path === item.path)
                        }
                        onLocalMigrate={(res) =>
                          remapRecordsLocally((s) => s.path === item.path, res)
                        }
                      >
                        <MemoryCard
                          item={item}
                          selected={selected}
                          onClick={() => setSelected(item)}
                          query={query.trim()}
                          contentQuery={contentQuery}
                          hit={contentHits.get(sessionKey(item))}
                          showSource={source === "All"}
                        />
                      </ListItemMenu>
                    ))}
                  </div>
                )}
              </section>

              <section className="flex flex-col min-h-0 min-w-0 bg-background">
                {!selected && sessions.length === 0 && (
                  <EmptyState
                    icon={<Sparkles size={32} />}
                    title={t("records.nothingToView")}
                    description={t("records.nothingToViewDesc")}
                  />
                )}
                {!selected && sessions.length > 0 && (
                  <EmptyState icon={<Sparkles />} title={t("records.selectRecord")} />
                )}
                {selected && (
                  <>
                    <header className="flex flex-col gap-2 p-3">
                      <h2 className="text-lg font-semibold leading-snug line-clamp-2">
                        {selected.title || "(untitled)"}
                      </h2>

                      <div className="flex items-center gap-2 text-xs leading-none text-muted-foreground flex-wrap">
                        <span className="inline-flex items-center gap-1">
                          <Calendar size={12} className="shrink-0" />
                          {formatDate(selected.updated_at ?? selected.started_at)}
                        </span>
                        {isSessionItem(selected) && (
                          <>
                            <span className="text-border">·</span>
                            <span className="inline-flex items-center gap-1">
                              <MessageSquare size={12} className="shrink-0" />
                              {selected.message_count}
                            </span>
                          </>
                        )}
                        <span className="text-border">·</span>
                        <span className="inline-flex items-center gap-1 min-w-0">
                          <Folder size={12} className="shrink-0" />
                          <span className="truncate">
                            {projectDisplayName(selected.project)}
                          </span>
                        </span>
                      </div>

                      <div className="flex items-center justify-between gap-2">
                        <div className="inline-flex items-center gap-1 min-w-0 text-xs leading-none font-mono text-muted-foreground">
                          <File size={12} className="shrink-0" />
                          <span className="truncate">{selected.path}</span>
                        </div>
                        <div className="inline-flex items-center gap-2 shrink-0">
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <button
                                type="button"
                                onClick={() => revealItemInDir(selected.path)}
                                aria-label={t("records.openInFinder")}
                                className="inline-flex shrink-0 text-muted-foreground hover:text-foreground transition-colors"
                              >
                                <FolderOpen size={12} />
                              </button>
                            </TooltipTrigger>
                            <TooltipContent>{t("records.openInFinder")}</TooltipContent>
                          </Tooltip>
                          <CopyMenu
                            items={[
                              ...(isSessionItem(selected) &&
                              resumeCommandFor(selected.source, selected.id)
                                ? [
                                    {
                                      label: t("menu.copyResumeCommand"),
                                      value: resumeCommandFor(selected.source, selected.id)!
                                    }
                                  ]
                                : []),
                              { label: t("menu.copyPath"), value: selected.path },
                              { label: t("menu.copyFilename"), value: basename(selected.path) },
                              ...(isSessionItem(selected)
                                ? [{ label: t("menu.copySessionId"), value: selected.id }]
                                : [])
                            ]}
                          />
                        </div>
                      </div>
                    </header>

                    {detailLoading ? (
                      <div className="flex-1 flex items-center justify-center text-muted-foreground">
                        <Loader2 className="animate-spin" />
                      </div>
                    ) : isSessionItem(selected) && detail?.messages.length ? (
                      <MessageList
                        messages={detail.messages}
                        favorites={{
                          session: selected,
                          keys: favoriteKeys,
                          onToggle: (message, index) =>
                            toggleFavorite(selected, message, index)
                        }}
                        scrollRequest={
                          pendingScroll &&
                          pendingScroll.sessionKey ===
                            `${selected.source}::${selected.id}`
                            ? {
                                index: pendingScroll.index,
                                nonce: pendingScroll.nonce
                              }
                            : null
                        }
                        onScrolled={clearPendingScroll}
                      />
                    ) : detail?.messages.length ? (
                      <div className="flex-1 overflow-auto px-4 py-2">
                        <div className="rounded-lg bg-card text-card-foreground px-5 py-4">
                          <MessageBody
                            text={detail.messages.map((m) => m.text).join("\n\n")}
                          />
                        </div>
                      </div>
                    ) : (
                      <div className="flex-1" />
                    )}
                  </>
                )}
              </section>
            </main>
          )}
        </div>
      </div>
      <FreshnessFooter
        syncing={loading}
        lastSyncedAt={lastRefreshedAt}
        error={error}
      />
      <CommandPalette
        sessions={sessions}
        onOpenItem={openItem}
        recentSearches={recentSearches}
        onCommitSearch={addRecentSearch}
        onClearRecent={clearRecentSearches}
      />
      {/* UpdateDialog is gated on `pendingUpdate` being non-null AND
          wrapped in Suspense so its chunk only downloads when an
          update is actually found. */}
      {pendingUpdate && (
        <React.Suspense fallback={null}>
          <UpdateDialog
            update={pendingUpdate}
            currentVersion={appVersion}
            onClose={() => setPendingUpdate(null)}
          />
        </React.Suspense>
      )}
    </div>
  );
}
