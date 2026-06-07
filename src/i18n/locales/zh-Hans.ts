import type { MessageKey } from "../index";

/** 简体中文 — must define every key in `en` (enforced by the type). */
export const zhHans: Record<MessageKey, string> = {
  "nav.providers": "供应商",
  "nav.records": "记录",
  "nav.favorites": "收藏",
  "nav.search": "搜索",
  "nav.stats": "统计",
  "nav.settings": "设置",

  "search.placeholder": "搜索会话、记忆、技能…",
  "search.hint": "在 Termory 扫描的每个会话、记忆、技能里搜索。",
  "search.press": "按",
  "search.summon": "可随时唤起搜索。",
  "search.indexed": "已索引 {n} 条记录。",
  "search.recent": "最近",
  "search.clear": "清除",
  "search.noMatch": "没有匹配「{query}」的结果",

  "favorites.emptyTitle": "还没有收藏",
  "favorites.emptyDesc":
    "在「记录」里点任意消息旁的星标即可收藏到这里。收藏会快照完整消息内容,即使原会话之后被删除也仍可查看。",
  "favorites.openOriginal": "打开原始会话",
  "favorites.remove": "取消收藏",
  "favorites.archived": "已归档",

  "common.copy": "复制",
  "footer.syncFailed": "同步失败",
  "footer.syncing": "同步中…",
  "footer.syncedJustNow": "刚刚同步",
  "footer.synced": "{ago}同步",
  "footer.status": "同步状态",

  "settings.appearance": "外观",
  "settings.theme.system": "跟随系统",
  "settings.theme.light": "浅色",
  "settings.theme.dark": "深色",

  "settings.language": "语言",
  "settings.language.desc": "应用界面使用的语言。",

  "settings.terminal": "终端",
  "settings.terminal.desc":
    "从菜单栏托盘恢复最近会话时打开哪个终端。只列出本机已安装的终端。",
  "settings.terminal.saveError": "保存终端失败:{error}",

  "settings.storage": "存储",
  "settings.storage.dir": "Termory 数据目录",
  "settings.storage.open": "打开",
  "settings.storage.note":
    "界面偏好存于 config.json,供应商库存于 providers.json。两个文件在 Unix 上均为 chmod 0600。",

  "settings.search": "搜索历史",
  "settings.search.recent": "最近搜索",
  "settings.search.count_one": "已存 {n} 条",
  "settings.search.count_other": "已存 {n} 条",
  "settings.search.clear": "清除",

  "settings.shortcuts": "键盘快捷键",
  "settings.shortcuts.searchPalette": "打开搜索面板",
  "settings.shortcuts.searchPaletteAlias": "打开搜索面板(别名)",
  "settings.shortcuts.switchRoute": "切换导航路由",
  "settings.shortcuts.closePalette": "关闭面板 / 下拉",

  "settings.about": "关于",
  "settings.about.app": "应用",
  "settings.about.version": "版本",
  "settings.about.check": "检查更新",
  "settings.about.checking": "检查中…",
  "settings.about.latest": "已是最新版本。",
  "settings.about.checkFailed": "检查更新失败:{error}",
  "settings.about.auto": "自动检查更新",
  "settings.about.autoDesc": "应用启动几秒后自动检查一次。"
};
