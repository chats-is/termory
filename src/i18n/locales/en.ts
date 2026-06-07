/**
 * English — the SOURCE dictionary. Its keys form the `MessageKey` union, so
 * the other locales (`Record<MessageKey, string>`) must define every key or
 * TypeScript errors. `{name}` placeholders are filled by `t(key, params)`.
 */
export const en = {
  // Navigation (activity rail)
  "nav.providers": "Providers",
  "nav.records": "Records",
  "nav.favorites": "Favorites",
  "nav.search": "Search",
  "nav.stats": "Stats",
  "nav.settings": "Settings",

  // Search
  "search.placeholder": "Search across sessions, memories, skills…",
  "search.hint": "Search inside every session, memory, and skill Termory scans.",
  "search.press": "Press",
  "search.summon": "to summon search from anywhere.",
  "search.indexed": "{n} records indexed.",
  "search.recent": "Recent",
  "search.clear": "Clear",
  "search.noMatch": 'No matches for "{query}"',

  // Favorites
  "favorites.emptyTitle": "No favorites yet",
  "favorites.emptyDesc":
    "Click the star next to any message in Records to save it here. The full message content is snapshotted, so favorites survive even if the original session is later deleted.",
  "favorites.openOriginal": "Open original session",
  "favorites.remove": "Remove from favorites",
  "favorites.archived": "archived",

  // Common
  "common.copy": "Copy",
  "footer.syncFailed": "Sync failed",
  "footer.syncing": "Syncing…",
  "footer.syncedJustNow": "Synced just now",
  "footer.synced": "Synced {ago}",
  "footer.status": "Freshness status",

  // Stats
  "stats.dailyTokens": "Daily tokens",
  "stats.dailyActivities": "Daily activities",
  "stats.kpi.sessions": "Sessions",
  "stats.kpi.messages": "Messages",
  "stats.kpi.tokens": "Tokens",
  "stats.kpi.models": "Models",
  "stats.kpi.projects": "Projects",
  "stats.tokens.input": "Input",
  "stats.tokens.output": "Output",
  "stats.tokens.reasoning": "Reasoning",
  "stats.tokens.cached": "Cached",
  "stats.tokens.total": "Total",
  "stats.col.model": "Model",
  "stats.col.tokens": "Tokens",
  "stats.less": "Less",
  "stats.more": "More",
  "stats.modelCount_one": "{n} model",
  "stats.modelCount_other": "{n} models",
  "stats.summarySessions": "{n} sessions",
  "stats.summaryMessages": "{n} messages",
  "stats.range.today": "Today",
  "stats.range.7d": "Last 7 days",
  "stats.range.30d": "Last 30 days",
  "stats.range.90d": "Last 90 days",
  "stats.range.custom": "Custom range",
  "stats.range.label": "Range",
  "stats.range.apply": "Apply",
  "stats.source.all": "All",
  "stats.sourceFilter": "Source filter",
  "stats.refresh": "Refresh stats",

  // Appearance
  "settings.appearance": "Appearance",
  "settings.theme.system": "System",
  "settings.theme.light": "Light",
  "settings.theme.dark": "Dark",

  // Language
  "settings.language": "Language",
  "settings.language.desc": "The language used across the app.",

  // Terminal
  "settings.terminal": "Terminal",
  "settings.terminal.desc":
    "Which terminal opens when you resume a recent session from the menu-bar tray. Only terminals found on this machine are listed.",
  "settings.terminal.saveError": "Couldn't save terminal: {error}",

  // Storage
  "settings.storage": "Storage",
  "settings.storage.dir": "Termory data directory",
  "settings.storage.open": "Open",
  "settings.storage.note":
    "UI preferences live in config.json, the provider library in providers.json. Both files are chmod 0600 on Unix.",

  // Search history
  "settings.search": "Search history",
  "settings.search.recent": "Recent searches",
  "settings.search.count_one": "{n} stored entry",
  "settings.search.count_other": "{n} stored entries",
  "settings.search.clear": "Clear",

  // Keyboard shortcuts
  "settings.shortcuts": "Keyboard shortcuts",
  "settings.shortcuts.searchPalette": "Open search palette",
  "settings.shortcuts.searchPaletteAlias": "Open search palette (alias)",
  "settings.shortcuts.switchRoute": "Switch rail route",
  "settings.shortcuts.closePalette": "Close palette / dropdown",

  // About
  "settings.about": "About",
  "settings.about.app": "App",
  "settings.about.version": "Version",
  "settings.about.check": "Check for updates",
  "settings.about.checking": "Checking…",
  "settings.about.latest": "You're on the latest version.",
  "settings.about.checkFailed": "Update check failed: {error}",
  "settings.about.auto": "Check for updates automatically",
  "settings.about.autoDesc": "Runs once a few seconds after the app launches."
} as const;
