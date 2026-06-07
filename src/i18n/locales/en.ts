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

  // Records
  "records.pane.sessions": "Sessions",
  "records.pane.memories": "Memories",
  "records.pane.skills": "Skills",
  "records.noSessions": "No sessions yet",
  "records.noSessionsDesc":
    "Termory scans Codex, Claude Code, Gemini, and OpenCode for chat history. None of those tools have recorded sessions here yet.",
  "records.noSessionsMatch": "No sessions match",
  "records.noMemory": "No memory files yet",
  "records.noMemoryDesc":
    "Termory looks for AGENTS.md, CLAUDE.md, GEMINI.md, and per-project memory folders in the current working directory and your home folder.",
  "records.noMemoryMatch": "No memory matches",
  "records.noSkills": "No skills yet",
  "records.noSkillsDesc":
    "Termory scans ~/.claude/skills, ~/.codex/skills, ~/.gemini/skills, and ~/.agents/skills, plus project-local .agents/skills folders.",
  "records.noSkillsMatch": "No skill matches",
  "records.tryFilters": "Try a different source, project, or query.",
  "records.nothingMatches": "Nothing matches your current view.",
  "records.openInFinder": "Open in Finder",
  "common.all": "All",
  "favorites.add": "Add to favorites",

  // Context menu
  "menu.revealInFinder": "Reveal in Finder",
  "menu.resumeInTerminal": "Resume in terminal",
  "menu.copyResumeCommand": "Copy resume command",
  "menu.copyPath": "Copy path",
  "menu.copyFilename": "Copy filename",
  "menu.copySessionId": "Copy session ID",
  "menu.copyMessageId": "Copy message ID",
  "menu.copied": "Copied to clipboard",
  "menu.terminalError": "Couldn't open terminal: {error}",

  // Providers
  "providers.tabGateways": "Gateways",
  "providers.official": "Official",
  "providers.activate": "Activate",
  "providers.activating": "Activating…",
  "providers.setDefault": "Set as default",
  "providers.inUse": "In use",
  "providers.unmanaged": "Unmanaged",
  "providers.enable": "Enable",
  "providers.disable": "Disable",
  "providers.delete": "Delete",
  "providers.edit": "Edit",
  "providers.test": "Test",
  "providers.recheck": "Recheck",
  "providers.addProvider": "Add provider",
  "providers.noCustomProviders": "No custom providers yet",
  "providers.aiGateways": "AI Gateways",
  "providers.addGateway": "Add AI Gateway",
  "providers.deleteGateway": "Delete AI Gateway",
  "providers.editGateway": "Edit AI Gateway",
  "providers.noGateways": "No AI Gateways yet",
  // Provider / gateway editor
  "providers.editProvider": "Edit provider",
  "providers.cancel": "Cancel",
  "providers.create": "Create",
  "providers.save": "Save",
  "providers.name": "Name",
  "providers.baseUrl": "Base URL",
  "providers.apiKey": "API key",
  "providers.model": "Model",
  "providers.aiSdk": "AI SDK",
  "providers.additionalModels": "Additional models",
  "providers.advancedSettings": "Advanced settings",
  "providers.modelId": "Model ID",
  "providers.modelDisplayName": "Model display name",
  "providers.overrideKey": "Override key",
  "providers.overrideValue": "Override value",
  "providers.removeModel": "Remove model",
  "providers.removeOverride": "Remove override",
  "providers.showApiKey": "Show API key",
  "providers.hideApiKey": "Hide API key",
  "providers.toggleSettings": "Toggle settings",
  "providers.bindToSources": "Bind to sources",
  "providers.detectApis": "Detect APIs",
  "providers.modelPlaceholder": "Select or type a model id",
  "providers.displayNameOptional": "Display name (optional)",
  "providers.namePlaceholder": "My gateway",
  "providers.apiKeyPlaceholder": "sk-…",
  "providers.installFirst": "Install it first.",
  "providers.setting": "Setting…",
  "providers.noResponse": "no response",
  "providers.unnamed": "(unnamed)",
  "providers.aiGateway": "AI Gateway",
  "providers.gwEmptyDesc": "Add an AI Gateway: one base URL + key. Termory detects which API modes it supports and lets you bind it to the matching CLIs.",
  "providers.unnamedGateway": "(unnamed gateway)",
  "providers.noBaseUrl": "no base URL",
  "providers.noBindings": "No CLI bindings yet — edit to bind.",

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
