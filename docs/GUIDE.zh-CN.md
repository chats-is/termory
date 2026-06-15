# Termory — 使用指南

> English version: [GUIDE.md](GUIDE.md)

Termory 把你的终端 AI 编程工具——**Codex**、**Claude Code**、**Gemini CLI**、**OpenCode**——在本机已经存好的历史(会话、记忆、技能)汇总到一个窗口里浏览,无需导入或额外配置(它只是读取这些工具已有的本地数据)。除了浏览历史,它还能帮你管理并一键切换每个工具的 API 供应商。安装与下载见 [README](../README.md)。

## 功能地图

Termory 在左侧导航栏有六个入口(`⌘1`–`⌘6`),外加一个 macOS 菜单栏托盘:

| # | 入口 | 作用 |
|---|------|------|
| 1 | **[Providers(供应商)](#1-providers供应商)** | 管理每个 CLI 的 API 供应商并切换当前激活项;管理 AI Gateway;查看官方额度。 |
| 2 | **[Records(记录)](#2-records记录)** | 浏览所有会话、记忆文件、技能;恢复、迁移或删除。 |
| 3 | **[Favorites(收藏)](#3-favorites收藏)** | 你标星的消息,以快照形式保存。 |
| 4 | **[Search(搜索)](#4-search搜索)** | 跨全部历史的全文搜索,外加 `⌘K` 快速搜索面板。 |
| 5 | **[Stats(统计)](#5-stats统计)** | 在任意日期范围内的 token、消息、模型与活跃度热力图。 |
| 6 | **[Settings(设置)](#6-settings设置)** | 外观、语言、开机启动、终端、存储、搜索历史、快捷键、更新。 |
| — | **[菜单栏托盘](#菜单栏托盘macos)** | 不打开窗口即可恢复会话、新建会话或切换供应商。 |

两个贯穿性主题——**[隐私与你的数据](#隐私与你的数据)** 和 **[安装与更新](#安装与更新)**——放在最后。

---

## 1. Providers(供应商)

**是什么。** Termory 为每个 CLI 维护一套**供应商**——即不同 API 平台的命名配置,每个含 base URL、API 密钥、模型。一个 CLI 的供应商互相独立:为 Claude Code 你可以同时存一个 OpenRouter 配置、一个本地模型、一个官方登录,随时切换激活哪个。**Providers** 页面(默认首页)就是管理它们的地方。

### 切换当前供应商

1. 打开 **Providers**,选择 CLI 的标签(Claude Code / Codex / Gemini / OpenCode)。
2. 在想用的供应商上点 **Activate(激活)**(或 **Set as default(设为默认)**)。
3. 下次启动该 CLI 时就会使用新供应商——无需手动改配置。

随时点 **Official(官方)** 即可切回你的原生登录。

### 添加或编辑供应商

1. 点 **Add provider(添加供应商)**,填 **Name(名称)**,以及 **Base URL**、**API key**、**Model**。
2. base URL 和密钥填好后,模型字段会自动给出可用模型建议(也可手动输入)。
3. **Test(测试)** 在你正式依赖它之前先检查与 base URL 的连通性。
4. **Save(保存)**。编辑当前已激活的供应商会立即重新生效。

### Termory 不会全量覆盖你的 CLI 配置(技术原理)

这是核心保证:激活某个供应商时,Termory **只把几个字段合并进你已有的配置——绝不替换整个文件。**

**激活是字段级合并。** Termory 读取 CLI 当前的配置,**只**追加把它指向所选供应商所需的字段(base URL、密钥、模型,以及少量 CLI 专属的路由键),再写回。文件里其余的一切——你自己的自定义项、无关设置、其他工具的条目——都原封不动地保留。

**而且你的登录凭据还另存在一个 Termory 完全不写入的独立文件里。** 在上面的合并之外,每个 CLI 都把 OAuth 令牌 / 凭据与 Termory 编辑的配置分开存放,所以你的登录是双重安全的:

| CLI | Termory 合并写入的配置 | 它绝不碰的凭据文件 |
|-----|----------------------|-------------------|
| Claude Code | `~/.claude/settings.json` | `~/.claude/.credentials.json`(或 macOS 钥匙串) |
| Codex | `~/.codex/auth.json` + `~/.codex/config.toml` | `auth.json` 里的 `tokens`(即 ChatGPT 登录) |
| Gemini CLI | `~/.gemini/.env` | `~/.gemini/oauth_creds.json` + `google_accounts.json` |
| OpenCode | `~/.config/opencode/opencode.json` | `~/.local/share/opencode/auth.json` |

(Codex 是唯一一种配置与某个凭据共用一个文件的情况——即便如此 Termory 仍是**合并**:它设置 `auth_mode` 和 API 密钥,但保留你的 `tokens`,因此 ChatGPT 登录得以存活。)

**切回 Official 是对称操作:** Termory 移除它追加的覆盖字段,其余原样保留。由于原生登录从未被覆盖,CLI 会立即重新使用它——无需重新登录。

### 高级配置(每个供应商的 options)

除基本字段外,每个供应商都有 **Advanced settings(高级配置)** 区域,你可以在这里**自行添加**配置项——任何该 CLI 支持、而 Termory 没有专用字段的设置都行。你添加的每一项会在激活该供应商时合并进它的配置,切走时再移除。

**怎么添加:**

1. 在供应商编辑器里展开 **Advanced settings(高级配置)**。
2. 点 **Add(添加)** 新增一行,填入 **KEY** 和 **VALUE**。
3. 需要几行加几行;**Remove(删除)** 去掉一行。最后 **Save(保存)** 供应商。

下面的表格只是常见示例——你可以添加任意该 CLI 接受的键值对。规则如下:

- **key** 是指向该 CLI 配置的点路径——`a.b.c` 会创建嵌套结构。
- **value** 对 JSON/TOML 目标做类型推断:`true`/`false` → 布尔,整数 → 整型,小数 → 浮点,其余 → 字符串。(Gemini 的 `.env` 一律按字面字符串保留。)
- 由专用字段(base URL / 密钥 / 模型)掌管的 key 是**受管**的——编辑器会拦截,因为那些字段已在控制它们。

**Claude Code** → `~/.claude/settings.json`。最典型用途是把 Claude 的 Sonnet / Opus / Haiku 三档映射到具体上游模型(新建 Claude 供应商时预置这三行):

| Key | 示例值 |
|-----|--------|
| `env.ANTHROPIC_DEFAULT_SONNET_MODEL` | `gpt-5` |
| `env.ANTHROPIC_DEFAULT_OPUS_MODEL` | `claude-opus-4-8` |
| `env.ANTHROPIC_DEFAULT_HAIKU_MODEL` | `claude-haiku-4-5` |

在值后加 `[1m]` 可启用其 100 万 token 上下文,例如 `claude-sonnet-4-6[1m]`。*受管(禁止):* `env.ANTHROPIC_BASE_URL`、`env.ANTHROPIC_AUTH_TOKEN`、`env.ANTHROPIC_API_KEY`、`env.ANTHROPIC_MODEL`。

**Codex** → `~/.codex/config.toml`。点路径变成嵌套的 TOML 表:

| Key | 示例值 | 结果 |
|-----|--------|------|
| `model_reasoning_effort` | `high` | `model_reasoning_effort = "high"` |
| `approval_policy` | `on-request` | 字符串值 |
| `tools.web_search` | `true` | 布尔值(类型推断) |

*受管(禁止):* `model`、`model_provider`,以及整个 `model_providers.*` 表。

**Gemini CLI** → `~/.gemini/.env`。任何 Gemini 读取的环境变量——例如指定 Google Cloud 项目或改用 Vertex AI:

| Key | 示例值 |
|-----|--------|
| `GOOGLE_CLOUD_PROJECT` | `my-project-id` |
| `GOOGLE_GENAI_USE_VERTEXAI` | `true` |

*受管(禁止):* `GOOGLE_GEMINI_BASE_URL`、`GEMINI_API_KEY`、`GEMINI_MODEL`。

**OpenCode** → `~/.config/opencode/opencode.json`。OpenCode 在 options 之上还有两个专用字段——**AI SDK**(它加载的 npm 包,默认 `@ai-sdk/openai-compatible`)和 **Additional models(更多模型)**(在选择器里额外显示的模型 ID)。高级 option 的 key 相对于该供应商的 `options` 容器:

| Key | 示例值 | 作用 |
|-----|--------|------|
| `timeout` | `600000` | 请求超时(毫秒) |
| `headers.X-Token` | `abc123` | 自定义请求头 |

*受管(禁止):* `baseURL`、`apiKey`。

### Codex:切换后保留会话

Codex 给每个会话打上创建时所用供应商的标记,而 `codex resume` 只列出与**当前**供应商匹配的会话——所以切换可能让某个项目早期的会话消失。当你在 Official 与自定义供应商之间切换 Codex 时,Termory 会弹出 **"保留早期会话?"**:勾选哪些项目的会话应跟随到新供应商,Termory 就重新打标,让 `codex resume` 仍能列出它们。其他 CLI 按项目路径列出恢复历史,无需此机制。

### AI Gateway(网关)

一个**网关**就是一组 `{base URL, API 密钥}`,可能同时支持多种 API 格式(OpenAI、Anthropic、Gemini……)。与其把同一把密钥分别加到每个 CLI,你只需添加一次:

1. 打开 **AI Gateways** 标签 → **Add** 一个网关,填 base URL 和密钥。
2. 点 **Detect APIs(探测 API)**——Termory 探测网关支持哪些 API 格式。
3. **Apply(应用)** 到格式匹配的每个 CLI(一个网关 → 多个 CLI,一把密钥)。

已绑定的网关也会出现在各 CLI 的供应商列表中(仅可查看/激活——编辑请到 Gateways 标签)。

### 官方额度

对以官方订阅登录的 CLI,卡片以圆环显示你的用量(如 **5 小时** 和 **每周** 窗口),按压力着色(🟢 < 75%、🟡 ≥ 75%、🔴 ≥ 90%)。**Refresh usage(刷新用量)** 可重新拉取(有短暂冷却)。额度只读取你现有的官方登录——自定义供应商激活时会隐藏。

---

## 2. Records(记录)

**是什么。** Records 是历史浏览器。三个面板——**Sessions(会话)**、**Memories(记忆)**、**Skills(技能)**——列出 Termory 在四个工具里找到的一切。

- **Sessions** — 各 CLI 的聊天记录。
- **Memories** — 本地的记忆 / 指令文件(`CLAUDE.md`、`AGENTS.md`、`GEMINI.md`、各项目的记忆文件夹等)。
- **Skills** — 各工具技能目录下的 `SKILL.md` 文件。

### 浏览

- 侧边栏的**源过滤**(Codex / Claude / Gemini / OpenCode / All)会同时收窄三个面板。
- 侧边栏按**项目**(工作目录)分组。
- 点任意记录即可打开;详情面板按该工具自己的方式渲染每条消息(工具调用、diff、推理等)。
- 每条消息都有**复制**按钮(原始 markdown)和**星标**(加入收藏)。
- 详情顶部的 **Open in Finder** 可打开底层文件。

### 右键操作

右键任意会话、记忆或技能:

- **Reveal in Finder** — 打开底层文件。
- **Resume in terminal** / **Copy resume command** — 见下方[恢复会话](#恢复会话)。
- **Copy path / filename / session ID(复制路径/文件名/会话 ID)**。
- **Migrate(迁移)** — 把一个会话或它整个项目重新指向新路径(Claude Code 与 Codex)。在你重命名或移动仓库后很有用,可让历史在新位置重新归组。(项目级迁移也在侧边栏的项目行上。)
- **Delete session / project / memory(删除)** — 带确认步骤。

> **删除是永久的**,且删除会**改动 CLI 自己的数据**。如果某个 CLI 正在运行,可能持有数据库锁——Termory 会提示你先退出它。删除一个**项目**只移除它存储的历史;你实际项目文件夹里的文件(`CLAUDE.md`、`AGENTS.md` 等)绝不会被碰。

### 恢复会话

从**托盘**(点击某个最近会话)或在 Records 里**右键 → Resume in terminal** 来恢复会话。Termory 会打开你的终端、`cd` 进该会话的工作目录,并运行该 CLI 自己的恢复命令(`claude --resume <id>`、`codex resume <id>` 等)。在 **设置 → 终端** 里选择用哪个终端。

侧边栏项目行还有 **Open in terminal(在终端中打开)**——在该文件夹打开终端并全新启动 CLI(新会话,而非恢复)。

---

## 3. Favorites(收藏)

**是什么。** 存放值得保留的单条消息。在 Records 里点任意消息旁的**星标**,它就会以**自包含快照**的形式保存到这里——完整文本、角色、时间戳——所以即使你之后删除或改动原会话,它依然可读。

**怎么用:**

- 在 Records 里给消息标星 → 它出现在 **Favorites**。
- 在收藏详情里,**Open original session(打开原会话)** 跳回 Records 中的它(若仍存在);**Remove from favorites(取消收藏)** 删除该快照。
- 收藏存在本地 `~/.termory/favorites.json`。

---

## 4. Search(搜索)

**是什么。** 对 Termory 扫描到的每个会话、记忆、技能的正文做全文搜索。

**两种入口:**

- **Search** 页面(`⌘4`)——输入查询;结果按来源分组,匹配片段高亮。点结果会打开该记录并滚动到第一处匹配。
- **`⌘K` 快速搜索面板**——可从任何地方唤出(也可用 `⌘F`);同一份索引,键盘驱动(↑/↓/Enter),并列出你最近的搜索。

最近搜索会被记住;可在 **设置 → 搜索历史** 里清除。

---

## 5. Stats(统计)

**是什么。** 针对所选来源、在你选定的日期范围内的用量分析。所有数值都是**窗口精确**的——反映该范围内实际发生的,而非累计总量。

**怎么用:**

1. 选范围——**今天 / 最近 7 / 30 / 90 天**,或**自定义范围**(双月日历选择器)。
2. 可按来源过滤。

**你会看到:**

- **KPI 条** — 会话数、消息数、Tokens、模型数、项目数。悬停 **Tokens** 看 输入/输出/推理/缓存/总计 拆分;悬停 **Models** 看按模型的 token 用量。
- **Daily tokens(每日 token)** — 含 输入/输出/缓存/推理 四条线的趋势图;悬停看当天拆分。
- **Daily activities(每日活跃度)** — 24 小时 × 日期 的热力图;格子深浅由消息数与 token 共同决定。悬停某格看该小时的 会话/消息/token 及按模型用量。

> 模型归因是按会话粒度的(每个会话归到它记录的那个模型);没有记录模型的会话会被隐藏。

---

## 6. Settings(设置)

设置页(`⌘6`)包含这些分区:

| 分区 | 作用 |
|------|------|
| **Appearance(外观)** | 主题——跟随系统 / 浅色 / 深色。 |
| **Language(语言)** | English / 简体中文 / 繁體中文,即时生效。 |
| **Startup(启动)** | **开机启动**——登录时自动启动 Termory(仅托盘,无窗口)。 |
| **Terminal(终端)** | 恢复会话时打开哪个终端。只列出本机找到的终端;"auto" 用系统默认。 |
| **Storage(存储)** | 显示 `~/.termory/` 数据目录,带 **Open** 按钮。 |
| **Search history(搜索历史)** | 存了多少条最近搜索,带 **Clear** 按钮。 |
| **Keyboard shortcuts(快捷键)** | 参考列表(见下)。 |
| **About(关于)** | 应用版本、**检查更新**,以及自动检查开关。 |

### 快捷键

| 快捷键 | 操作 |
|--------|------|
| `⌘1`–`⌘6` | 切换导航栏入口 |
| `⌘K` / `⌘F` | 打开快速搜索面板 |
| `Esc` | 关闭面板 / 下拉 |

---

## 菜单栏托盘(macOS)

托盘让你不打开窗口也能操作:

- **最近会话** — 最多 5 个;点击在终端中恢复。
- **New Session(新建会话)** — 在最近项目文件夹中全新启动某个 CLI,或选一个新文件夹(**Choose Folder…**)。
- **各 CLI 子菜单** — 切换每个 CLI 的当前供应商;官方额度内联显示(🟢/🟡/🔴 按压力)。

关闭窗口不会退出 Termory——它继续在托盘运行。用 **Open** 唤回窗口,**Exit** 彻底退出。

---

## 隐私与你的数据

**Termory 没有服务器、没有账户、没有遥测。** 使用本应用的过程中,你的历史不会离开你的机器。

### Termory 自己的数据存在哪

只有 `~/.termory/`(在 macOS/Linux 上目录为 `0700`、文件为 `0600`,仅你本人可读)。可从 **设置 → 存储** 打开。

| 文件 | 内容 |
|------|------|
| `config.json` | 界面偏好。不含密钥。 |
| `providers.json` | 保存的供应商和网关——**含 API 密钥**。 |
| `favorites.json` | 标星消息的快照。 |

### Termory 会修改我的历史吗?

它**就地读取**你的历史,绝不改动——**除了**几个由你主动触发、会写入 CLI 自身数据存储的操作:

| 操作 | 改动了什么 | 机制 |
|------|-----------|------|
| **删除**会话 / 项目 / 记忆 | 移除该记录 | 基于文件的 CLI(Claude、Gemini):删文件。基于数据库的 CLI(Codex、OpenCode):删行(Codex 还删 rollout 文件,因为只删行会被回填重建)。 |
| **迁移**项目 | 重新指向新路径 | Claude:移动历史文件夹并改写每个会话的顶层 `cwd`。Codex:对 rollout 文件和 `threads` 表里的 `cwd` 做元数据改写——不移动文件。 |
| 切换 Codex 时**保留会话** | 给会话重新打上新供应商标记 | 改写 rollout 文件和 `threads` 表里的 `model_provider`。 |

**这些操作绝不碰的两样东西:** ① 你的 OAuth 登录 / 凭据文件;② 你项目工作目录里的文件(`CLAUDE.md`、`AGENTS.md` 等)——它们在你的仓库里,而非 CLI 的历史存储中。

### 有任何网络请求吗?

只有你主动触发时才会,且只发往**你自己选择**的端点:

| 操作 | 连接目标 |
|------|----------|
| 测试某个供应商 / 获取其模型列表 | 该供应商自己的 base URL |
| 探测网关支持的 API 模式 | 该网关的 base URL |
| 显示官方订阅额度 | 你的 CLI 本就登录的官方端点 |
| 检查应用更新 | GitHub releases |

没有统计分析、崩溃上报或后台回传。

---

## 安装与更新

### macOS:"Termory 已损坏,无法打开"

安装包未经 Apple 公证,因此 macOS 会隔离下载文件(Apple Silicon 上常见)。应用本身没问题——把它拖进 **应用程序(Applications)**,然后清除一次隔离标记:

```bash
xattr -dr com.apple.quarantine /Applications/Termory.app
```

### Windows / Linux

Windows SmartScreen:**更多信息 → 仍要运行**。Linux:从 [Releases](https://github.com/chats-is/termory/releases) 使用 `.AppImage`、`.deb` 或 `.rpm`。

### 更新

Termory 在应用内更新:**设置 → 关于 → 检查更新** → **立即安装**(它会带新版本重启)。自动更新只对"已带签名密钥版本"之后的安装生效;非常老的版本需从 [Releases](https://github.com/chats-is/termory/releases) 页面手动下载一次,之后应用内更新即可工作。
