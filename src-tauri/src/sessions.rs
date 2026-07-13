use chrono::{DateTime, Local, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use walkdir::WalkDir;

const CLAUDE_LITE_READ_BUF_SIZE: usize = 65_536;

/// Token usage rollup for a single session, populated by the parser
/// when the source platform persists token counts. All counters are
/// cumulative for the entire session.
///
/// Field semantics (chosen to match across all 4 platforms):
///   * `input`     — non-cached input/prompt tokens
///   * `output`    — completion / assistant tokens
///   * `cached`    — `cache_read + cache_creation` (Anthropic) /
///                   `cached_input_tokens` (Codex) / `cached`
///                   (Gemini) / `cache.read + cache.write` (OpenCode)
///   * `reasoning` — Codex `reasoning_output_tokens` / Gemini
///                   `thoughts` / OpenCode `reasoning`. Zero for
///                   platforms without separate reasoning tokens.
///   * `total`     — input + output + cached + reasoning (or the
///                   platform's own `total` field when present)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenStats {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub reasoning: u64,
    pub total: u64,
}

/// One entry per local date a session was active. Lets the frontend
/// place tokens on the actual day they were consumed, instead of
/// lumping a multi-day session's total onto `updated_at`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DailyTokenBreakdown {
    /// YYYY-MM-DD in the SCANNING machine's local timezone (same TZ
    /// used by every other date field on AppSession).
    pub date: String,
    pub tokens: TokenStats,
    /// Number of AI interactions counted on this date. One unit per
    /// assistant message (Claude) / token_count event (Codex) / tokens
    /// record (Gemini) / step-finish part (OpenCode). Drives the
    /// daily-workload chart on the Stats page.
    #[serde(default)]
    pub messages: u64,
    /// Per-hour message count, length 24, indexed by local hour 0..23.
    /// Drives the stacked time-of-day breakdown in the daily workload
    /// chart. Empty when no interactions had timestamps that could be
    /// resolved to a local hour.
    #[serde(default)]
    pub hours: Vec<u64>,
    /// Per-hour token total, length 24, indexed by local hour 0..23.
    /// Same shape as `hours` but accumulates `tokens.total` instead
    /// of an interaction count. Empty when no timestamps resolved.
    #[serde(default)]
    pub hour_tokens: Vec<u64>,
}

/// A first-class project: a working directory a CLI keeps records for. Shown
/// in the sidebar as long as its backing folder / entity exists, independently
/// of whether it currently has any records. `project` is the cwd string that
/// records carry in their own `project` field — the join key between a project
/// and its records. An empty project = one in the list that no record points at.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    pub source: String,
    pub project: String,
}

/// What a scan returns: the projects that exist + the records inside them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanResult {
    pub projects: Vec<Project>,
    pub records: Vec<AppSession>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSession {
    pub id: String,
    pub source: String,
    pub title: String,
    pub project: String,
    pub path: String,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: usize,
    /// True when the source CLI has archived this session. Only Codex
    /// (`threads.archived = 1`) and OpenCode (`session.time_archived`
    /// set) have a local archive flag; Claude / Gemini local history has
    /// no archive concept, so this is always `false` for them. Termory
    /// surfaces archived sessions (a history browser shows everything)
    /// and the list card marks them with an "Archived" badge.
    #[serde(default)]
    pub archived: bool,
    pub preview: String,
    /// One-line content hint shown beneath the project on list cards.
    /// For sessions: the last (or first) user prompt, trimmed and
    /// single-lined. For Memory/Skill: the first non-empty body line.
    /// Empty when nothing reasonable could be extracted.
    #[serde(default)]
    pub snippet: String,
    pub message_previews: Vec<SessionMessage>,
    /// Aggregate token usage for the session. `None` when the source
    /// didn't record any token data (legacy sessions, Memory/Skill
    /// entries, etc.). The Stats page renders coverage explicitly:
    /// "{N} of {M} sessions covered" so users aren't misled by
    /// partial totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenStats>,
    /// Best-guess model id used for this session
    /// (e.g. `claude-opus-4-7`, `gpt-5`, `gemini-3-flash-preview`).
    /// `None` when the source didn't record one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-day token spend, sorted chronologically. Populated by all
    /// four scanners when the underlying records carry per-message /
    /// per-part timestamps (Claude `message.usage`, Codex
    /// `token_count` events, Gemini per-record tokens, OpenCode
    /// `step-finish` parts). `None` only when no timestamped token
    /// data could be recovered — the frontend then falls back to
    /// even-distribution across [started_at, updated_at].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_tokens: Option<Vec<DailyTokenBreakdown>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
    pub kind: String,
    /// Internal: links a tool_use message with its tool_result by provider id
    /// (Claude tool_use.id / Codex function_call.call_id). Used by
    /// `merge_tool_outputs` and not exposed to the frontend.
    #[serde(skip)]
    pub tool_use_id: Option<String>,
    /// Internal: shell exit code parsed from a Codex `function_call_output`
    /// metadata wrapper. `merge_tool_outputs` surfaces it as part of the
    /// `**Error** · exit {N}` footer so failed commands carry diagnosis
    /// info even when stdout/stderr were empty.
    #[serde(skip)]
    pub exit_code: Option<i64>,
}

/// Leading status marker for tool-card headers. Pure unicode (no HTML
/// or styling — the frontend renders as-is):
/// * success → `⏺` BLACK CIRCLE FOR RECORD (Claude TUI BLACK_CIRCLE,
///   constants/figures.ts:4)
/// * failure → `✗` BALLOT X (Codex failure badge,
///   exec_cell/render.rs:236)
fn status_marker(failed: bool) -> &'static str {
    if failed {
        "✗ "
    } else {
        "⏺ "
    }
}

/// Wrap a string as a markdown inline code span, choosing the backtick
/// delimiter so the content's own backticks don't terminate the span.
/// Implements CommonMark 6.1 (Code spans): if the content begins or ends
/// with a backtick, both delimiters are padded with a space (those spaces
/// are stripped during render).
fn wrap_inline_code(content: &str) -> String {
    let max_run = content
        .chars()
        .fold((0usize, 0usize), |(max, cur), c| {
            if c == '`' {
                let new_cur = cur + 1;
                (max.max(new_cur), new_cur)
            } else {
                (max, 0)
            }
        })
        .0;
    let delim = "`".repeat(max_run + 1);
    if content.starts_with('`') || content.ends_with('`') {
        format!("{delim} {content} {delim}")
    } else {
        format!("{delim}{content}{delim}")
    }
}

/// Format reasoning content as italic blockquote (`> *content*`) so the
/// frontend's default markdown renderer styles it visibly distinct from
/// normal assistant text — matching the common "thinking" display
/// convention.
fn format_reasoning_body(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Codex's `agent_reasoning` text often starts with a bold header
    // line (`**Identifying X**\n\nbody...`) — TUI's ReasoningSummaryCell
    // (messages.rs:429-455) renders that as a bold title + italic body.
    // Detect the leading `**...**` and keep it as bold inside the
    // blockquote (no italic escaping), so the title visually stands
    // apart from the body lines below.
    let mut out = String::new();
    let mut lines = trimmed.lines().peekable();
    let mut handled_first = false;
    if let Some(first) = lines.peek() {
        let first_trim = first.trim();
        if first_trim.starts_with("**")
            && first_trim.ends_with("**")
            && first_trim.len() > 4
            && !first_trim[2..first_trim.len() - 2].contains("**")
        {
            out.push_str("> ");
            out.push_str(first_trim);
            handled_first = true;
            lines.next();
        }
    }
    // Italic doesn't survive multiple paragraphs cleanly, so wrap each
    // remaining non-empty line in `> *...*` so the entire block renders
    // as one styled blockquote.
    for (idx, line) in lines.enumerate() {
        if handled_first || idx > 0 {
            out.push('\n');
        }
        let line = line.trim_end();
        if line.is_empty() {
            out.push('>');
        } else {
            out.push_str("> *");
            // Escape stray `*` chars in the source so italic isn't broken.
            for c in line.chars() {
                if c == '*' || c == '_' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('*');
        }
    }
    out
}

/// Walk a list of tool-related messages and fold `tool_result` /
/// `tool_error` output into the preceding `tool_use` matched by
/// `tool_use_id`. Mirrors the TUI experience where a tool call and its
/// output render in a single block. Messages without a matching pair are
/// left in place so orphaned results are not lost.
fn merge_tool_outputs(messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
    let call_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.kind == "tool_use")
        .filter_map(|m| m.tool_use_id.clone())
        .collect();
    if call_ids.is_empty() {
        return messages;
    }

    // (text, is_error, exit_code) — multiple result blocks per call get
    // concatenated; any error marks the whole bundle so merge can append
    // `**Error**` (with the most recent non-zero exit code, when known).
    let mut output_by_id: HashMap<String, (String, bool, Option<i64>)> = HashMap::new();
    for msg in &messages {
        if !matches!(msg.kind.as_str(), "tool_result" | "tool_error") {
            continue;
        }
        let Some(id) = msg.tool_use_id.clone() else {
            continue;
        };
        if !call_ids.contains(&id) {
            continue;
        }
        let is_error = msg.kind == "tool_error";
        output_by_id
            .entry(id)
            .and_modify(|(text, err, code)| {
                text.push_str("\n\n");
                text.push_str(&msg.text);
                *err = *err || is_error;
                if msg.exit_code.is_some() {
                    *code = msg.exit_code;
                }
            })
            .or_insert_with(|| (msg.text.clone(), is_error, msg.exit_code));
    }

    messages
        .into_iter()
        .filter_map(|mut msg| match msg.kind.as_str() {
            "tool_result" | "tool_error" => {
                if let Some(id) = &msg.tool_use_id {
                    if output_by_id.contains_key(id) {
                        return None;
                    }
                }
                // Orphan result (no matching tool_use kept it). Prefix
                // with the same status marker we use for tool_use cards
                // so the user can scan failures at a glance.
                let prefix = status_marker(msg.kind == "tool_error");
                msg.text = format!("{prefix}{}", msg.text);
                Some(msg)
            }
            "tool_use" => {
                // Status marker BEFORE the tool name. `⏺` for success
                // (Claude TUI's BLACK_CIRCLE, constants/figures.ts:4),
                // `✗` for failure (Codex `exec_cell/render.rs:236-239`
                // failure badge). Either way it's the first visual signal.
                let matched = msg.tool_use_id.as_ref().and_then(|id| output_by_id.get(id));
                let failed = matched.is_some_and(|(_, err, _)| *err);
                let marker = status_marker(failed);
                msg.text = format!("{marker}{}", msg.text);

                if let Some((output, is_error, exit_code)) = matched {
                    let trimmed = output.trim();
                    // Write tool: tool_use card already has the
                    // `⎿ Wrote N lines to file` summary + content
                    // preview from `claude_tool_use_text`. Drop the
                    // redundant tool_result confirmation entirely.
                    if !*is_error && msg.text.contains("⏺ **Write**(") {
                        return Some(msg);
                    }
                    // Read tool: emit a `⎿ Read **N** lines` summary
                    // as a markdown paragraph (where `⎿` width doesn't
                    // matter and `**N**` renders bold), then content in
                    // a plain fence below. Mirrors FileReadTool/UI.tsx.
                    let is_read = !*is_error
                        && (msg.text.contains("⏺ **Read**(")
                            || msg.text.contains("⏺ **Read agent output**("));
                    if is_read {
                        let line_count = trimmed.lines().count();
                        let label = if line_count == 1 { "line" } else { "lines" };
                        msg.text
                            .push_str(&format!("\n\n⎿ Read **{line_count}** {label}"));
                        if !trimmed.is_empty() {
                            msg.text.push_str(&format!("\n\n````\n{trimmed}\n````"));
                        }
                        return Some(msg);
                    }
                    // Grep / Glob: split off the `Found **N** …` summary
                    // line so the bold count renders outside the fence
                    // (GrepTool/UI.tsx:14-72).
                    if !*is_error && msg.text.contains("⏺ **Search**(") {
                        if let Some((summary, rest)) = claude_split_grep_summary(trimmed) {
                            msg.text.push_str(&format!("\n\n⎿ {summary}"));
                            if !rest.is_empty() {
                                msg.text.push_str(&format!("\n\n````\n{rest}\n````"));
                            }
                            return Some(msg);
                        }
                    }
                    // Error path: `⎿ Error: ...` summary as a markdown
                    // paragraph so the font width of `⎿` never affects
                    // alignment. Codex has exit codes — `⎿ Error: Exit
                    // code N` summary + stdout/stderr fence below.
                    // Claude has no exit code — `⎿ Error: {message}`
                    // inline on one line (typical Claude errors are a
                    // single sentence, e.g. InputValidationError).
                    if *is_error {
                        msg.text.push_str("\n\n");
                        match *exit_code {
                            Some(code) if code != 0 => {
                                msg.text.push_str(&format!("⎿ Error: Exit code {code}"));
                                if !trimmed.is_empty() {
                                    msg.text.push_str(&format!("\n\n````\n{trimmed}\n````"));
                                }
                            }
                            _ => {
                                msg.text.push_str("⎿ Error:");
                                if !trimmed.is_empty() {
                                    msg.text.push(' ');
                                    msg.text.push_str(trimmed);
                                }
                            }
                        }
                        return Some(msg);
                    }
                    // Generic success: when the tool_result body was
                    // pre-formatted by a structured-result builder
                    // (Edit / WebFetch / NotebookEdit / Worktree /
                    // Config / ScheduleCron — all return `⎿ summary
                    // ...\n\n```fence`), pass it through verbatim. A
                    // body that begins with a code fence is also
                    // already-markdown. Everything else (raw shell
                    // output, etc.) goes into a plain 4-backtick fence.
                    //
                    // The `⎿ ` start-anchor is the strict signal —
                    // earlier heuristics that triggered on `**`
                    // anywhere in the body misfired on shell output
                    // containing `/**` JSDoc comments, `**kwargs`,
                    // emphasis examples in docs, etc., causing the
                    // whole block to render as paragraphs instead of
                    // monospace code.
                    if !trimmed.is_empty() {
                        msg.text.push_str("\n\n");
                        let already_markdown =
                            trimmed.starts_with("⎿ ") || trimmed.starts_with("```");
                        if already_markdown {
                            msg.text.push_str(trimmed);
                        } else {
                            msg.text.push_str(&format!("````\n{trimmed}\n````"));
                        }
                    }
                }
                Some(msg)
            }
            _ => Some(msg),
        })
        .collect()
}

// ===========================================================================
// Termory unified message format helpers
// ===========================================================================
//
// Goal: take heterogeneous tool/message data from 4 platforms (Claude /
// Codex / Gemini / OpenCode) and emit a single, consistent markdown body
// shape per content kind. **Content fidelity** is the constraint — every
// source field that has user-visible meaning gets a place in the output
// (either the primary template line, the body block, or the `- key: value`
// extras list below). Nothing is silently dropped, truncated, or renamed.
//
// Templates do NOT mimic any one CLI's visual style (Claude TUI bold,
// Codex magenta `$ `, OpenCode panel border, Gemini `> `/`✦ ` prefixes).
// Termory's rendering layer is markdown → HTML, so the templates use
// neutral markdown constructs (code fences, headings, bullet lists,
// blockquotes) that the renderer can style with CSS.

/// Symbolic constants for the SessionMessage.kind values used across this
/// module. The strings match what each platform's parser historically
/// emitted; the constants only exist to avoid bare string literals in
/// the parser code.
mod kind {
    pub const TEXT: &str = "message";
    pub const REASONING: &str = "reasoning";
    pub const TOOL_USE: &str = "tool_use";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const TOOL_ERROR: &str = "tool_error";
    pub const COMPACTION: &str = "compaction";
    pub const LOCAL_COMMAND: &str = "local_command";
    pub const SHELL: &str = "shell";
    pub const COMMAND_EXECUTION: &str = "command_execution";
    pub const PLAN: &str = "plan";
    pub const AGENT_SWITCHED: &str = "agent-switched";
    pub const MODEL_SWITCHED: &str = "model-switched";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session: AppSession,
    pub messages: Vec<SessionMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session: AppSession,
    pub snippet: String,
    pub role: String,
    pub match_count: usize,
    /// True when the per-session search loop stopped counting after
    /// hitting the [`SEARCH_MATCH_CAP`] ceiling — i.e. the real match
    /// count is at least `match_count` but could be much higher. The
    /// frontend uses this to render `×500+` instead of `×500`.
    #[serde(default)]
    pub truncated: bool,
    /// 0-based index of the first message containing the query in the
    /// parsed `detail.messages` list. The frontend feeds this to
    /// `openItem(session, messageIndex)` so the Records detail pane
    /// scrolls straight to the matching message. `None` only if the
    /// hit came from a code path that didn't observe an index
    /// (currently impossible — every populated `SearchHit` carries
    /// one — but optional on the wire for forward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_match_index: Option<usize>,
}

/// Per-session match-count ceiling. After this many hits we stop
/// scanning the rest of the session — the user can already tell
/// "this one has a lot" and counting further is wasted work.
const SEARCH_MATCH_CAP: usize = 500;

pub fn search_sessions(query: &str) -> Result<Vec<SearchHit>, Box<dyn Error>> {
    let trimmed = query.trim();
    if trimmed.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let needle = trimmed.to_lowercase();
    let sessions = scan_sessions()?.records;
    let mut hits = Vec::new();
    for session in sessions {
        let Some(detail) = get_session_for_search(&session) else {
            continue;
        };
        let mut first: Option<(String, String, usize)> = None;
        let mut match_count = 0usize;
        let mut truncated = false;
        for (msg_index, message) in detail.messages.iter().enumerate() {
            let lower = message.text.to_lowercase();
            let mut cursor = 0usize;
            while let Some(found) = lower[cursor..].find(&needle) {
                let pos = cursor + found;
                let end = pos + needle.len();
                if first.is_none()
                    && message.text.is_char_boundary(pos)
                    && message.text.is_char_boundary(end)
                {
                    first = Some((
                        message.role.clone(),
                        make_search_snippet(&message.text, pos, end),
                        msg_index,
                    ));
                }
                match_count += 1;
                if match_count >= SEARCH_MATCH_CAP {
                    truncated = true;
                    break;
                }
                cursor = end;
            }
            if truncated {
                break;
            }
        }
        if let Some((role, snippet, first_match_index)) = first {
            hits.push(SearchHit {
                session: detail.session.clone(),
                snippet,
                role,
                match_count,
                truncated,
                first_match_index: Some(first_match_index),
            });
        }
    }
    Ok(hits)
}

/// LRU cache backing `search_sessions`. Search calls `get_session` for
/// every record in scan output, which is `O(records × parse_cost)` —
/// for users with 100s of sessions that means several seconds of
/// JSONL / SQLite work per keystroke. Keyed by `(path, mtime)` so the
/// cache invalidates automatically when the underlying file changes:
/// a fresh write produces a new mtime → new key → cache miss → re-
/// parse.
///
/// Capacity is intentionally small (default 200 entries). Real LRU
/// eviction via a `VecDeque<key>` companion to the `HashMap` — when
/// inserting past capacity, drop the front of the deque. Hot entries
/// stay because every `get` rotates them to the back.
struct SearchCache {
    map: HashMap<(std::path::PathBuf, std::time::SystemTime), Arc<SessionDetail>>,
    order: std::collections::VecDeque<(std::path::PathBuf, std::time::SystemTime)>,
    capacity: usize,
}

impl SearchCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            capacity,
        }
    }

    fn get(
        &mut self,
        key: &(std::path::PathBuf, std::time::SystemTime),
    ) -> Option<Arc<SessionDetail>> {
        if !self.map.contains_key(key) {
            return None;
        }
        // Promote to MRU.
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
        self.map.get(key).map(Arc::clone)
    }

    fn put(&mut self, key: (std::path::PathBuf, std::time::SystemTime), value: Arc<SessionDetail>) {
        if self.map.contains_key(&key) {
            self.order.retain(|k| k != &key);
        } else if self.map.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

static SEARCH_CACHE: std::sync::LazyLock<std::sync::Mutex<SearchCache>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(SearchCache::new(200)));

/// Everything the LIST scan derives from reading a session file's full
/// contents: token usage, model, per-day breakdown, AND the visible
/// message count. Bundled so one streamed pass over the file produces
/// all of it and the whole bundle is cached together by mtime.
#[derive(Clone, Default)]
struct ScanExtract {
    tokens: Option<TokenStats>,
    model: Option<String>,
    daily_tokens: Option<Vec<DailyTokenBreakdown>>,
    message_count: usize,
}

/// `(path, mtime) → ScanExtract` cache used by scan-time extraction for
/// Codex / Claude. Scans iterate hundreds of session files (hundreds of
/// MB of JSONL); re-parsing every file on every watcher-triggered
/// re-scan would peg a core continuously while any CLI is in active
/// use. Cache keys are absolute paths and mtimes, so any user-side
/// edit (or watcher-driven file replacement) cleanly invalidates the
/// entry — no manual eviction needed.
struct ScanCacheEntry {
    mtime: std::time::SystemTime,
    extract: ScanExtract,
}

static SCAN_EXTRACT_CACHE: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, ScanCacheEntry>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Read-buffer size for the streaming scan extractors. Session JSONLs
/// run to 100+ MB; the BufReader default (8 KB) costs ~32x more read
/// syscalls than this on the cache-miss path.
const SCAN_READ_BUF_SIZE: usize = 256 * 1024;

/// Drop cache entries whose file no longer exists. Without this,
/// deleted sessions pin their parsed stats in memory for the lifetime
/// of the app. Called once per scan — one `exists()` probe per cached
/// path is noise next to the scan's own directory walks.
fn evict_stale_scan_cache() {
    if let Ok(mut cache) = SCAN_EXTRACT_CACHE.lock() {
        cache.retain(|path, _| path.exists());
    }
}

/// Convert an ISO-8601 timestamp (typically RFC 3339 UTC) into a
/// `(YYYY-MM-DD, hour)` pair in the scanning machine's local timezone.
/// Used by per-message token + per-hour workload bucketing — every
/// other date field on AppSession is in local time, so the buckets
/// need to match.
fn iso_to_local_date_hour(iso: &str) -> Option<(String, u8)> {
    use chrono::Timelike;
    let parsed = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    let local = parsed.with_timezone(&Local);
    Some((local.format("%Y-%m-%d").to_string(), local.hour() as u8))
}

/// Same as `iso_to_local_date_hour` but for epoch-milliseconds inputs.
/// OpenCode stores `part.time_created` (and the JSON `time.created`
/// field) as ms since the Unix epoch.
fn epoch_ms_to_local_date_hour(ms: i64) -> Option<(String, u8)> {
    use chrono::Timelike;
    let dt = Utc.timestamp_millis_opt(ms).single()?;
    let local = dt.with_timezone(&Local);
    Some((local.format("%Y-%m-%d").to_string(), local.hour() as u8))
}

fn cached_scan_extract<F>(path: &Path, extract: F) -> ScanExtract
where
    F: FnOnce(&Path) -> ScanExtract,
{
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some(mtime) = mtime {
        if let Ok(cache) = SCAN_EXTRACT_CACHE.lock() {
            if let Some(entry) = cache.get(path) {
                if entry.mtime == mtime {
                    return entry.extract.clone();
                }
            }
        }
        let extracted = extract(path);
        if let Ok(mut cache) = SCAN_EXTRACT_CACHE.lock() {
            cache.insert(
                path.to_path_buf(),
                ScanCacheEntry {
                    mtime,
                    extract: extracted.clone(),
                },
            );
        }
        extracted
    } else {
        extract(path)
    }
}

/// Look up the session's parsed detail, using the LRU cache when the
/// file's mtime hasn't changed since last time we parsed it. Returns
/// `None` when the path can't be resolved or the file can't be parsed —
/// search treats either as "skip this record" rather than fail.
fn get_session_for_search(session: &AppSession) -> Option<Arc<SessionDetail>> {
    let path = lookup_session_path(&session.source, &session.id)?;
    // Look up mtime so we can key the cache on (path, mtime). If we
    // can't stat the file, skip caching but still try a fresh parse —
    // the file may exist but live in a path the runtime can't stat.
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Some(mtime) = mtime {
        let key = (path.clone(), mtime);
        if let Ok(mut cache) = SEARCH_CACHE.lock() {
            if let Some(cached) = cache.get(&key) {
                return Some(cached);
            }
        }
        let detail = get_session(&session.source, &session.id).ok()?;
        let arc = Arc::new(detail);
        if let Ok(mut cache) = SEARCH_CACHE.lock() {
            cache.put(key, Arc::clone(&arc));
        }
        Some(arc)
    } else {
        get_session(&session.source, &session.id).ok().map(Arc::new)
    }
}

fn make_search_snippet(text: &str, match_start: usize, match_end: usize) -> String {
    let before_chars = 60usize;
    let after_chars = 100usize;

    let mut snippet_start = match_start;
    for (_, (b, _)) in (0..before_chars).zip(text[..match_start].char_indices().rev()) {
        snippet_start = b;
    }

    let mut snippet_end = text.len();
    for (taken, (b, c)) in text[match_end..].char_indices().enumerate() {
        if taken >= after_chars {
            snippet_end = match_end + b;
            break;
        }
        snippet_end = match_end + b + c.len_utf8();
    }

    let prefix = if snippet_start > 0 { "…" } else { "" };
    let suffix = if snippet_end < text.len() { "…" } else { "" };
    let core = collapse_whitespace(&text[snippet_start..snippet_end]);
    format!("{prefix}{}{suffix}", core.trim())
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

pub fn scan_sessions() -> Result<ScanResult, Box<dyn Error>> {
    // Records (sessions + memory + skills) — the contents that live INSIDE
    // projects. Scanned independently of the project list.
    let mut records = Vec::new();
    records.extend(scan_codex()?);
    records.extend(scan_claude()?);
    records.extend(scan_gemini()?);
    records.extend(scan_opencode()?);
    records.extend(scan_grok()?);
    let project_cwds: HashSet<String> = records
        .iter()
        .filter(|s| !s.project.is_empty() && Path::new(&s.project).is_absolute())
        .map(|s| s.project.clone())
        .collect();
    records.extend(scan_memory(&project_cwds)?);
    records.extend(scan_skills(&project_cwds)?);
    records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    // Projects = the project ENTITIES each CLI actually keeps, enumerated
    // directly and INDEPENDENTLY of records. A project shows because its entity
    // exists (folder / row / cwd), not because a record groups under it. The
    // folder-based CLIs (Claude, Gemini) are enumerated from disk;
    // `add_record_projects` fills in CLIs that have no enumerable folder entity
    // (Codex — sessions in one DB keyed by cwd; OpenCode — likewise for now).
    let mut projects = scan_project_folders();
    // OpenCode keeps projects in a `project` table independent of sessions, so a
    // project with no sessions (empty) still exists — surface those by worktree.
    // Projects WITH sessions come from `add_record_projects` below (by the
    // session's directory), so there's no overlap.
    projects.extend(opencode_empty_projects());
    add_record_projects(&mut projects, &records);

    // Path index BEFORE the tool-toggle filter: `load_session` resolves
    // by (source, id) through this index, and Favorites' "Open original"
    // must keep working for a snapshot whose source is currently toggled
    // off (the toggle hides listings, it doesn't revoke access to data
    // that a stored favorite legitimately references). The index is
    // still bounded to scan-discovered files — never frontend paths.
    refresh_session_path_index(&records);

    // Tool toggles (Settings → Tools; config.json `sources` map, absent
    // key = enabled) — ONE post-pass filters everything scan-derived, so
    // every consumer (Records, Search, Stats, the tray's recent list)
    // follows the setting automatically.
    apply_source_toggles(
        &mut projects,
        &mut records,
        &crate::config::disabled_sources(),
    );

    evict_stale_scan_cache();
    Ok(ScanResult { projects, records })
}

/// Drop scan output belonging to DISABLED sources (`disabled` holds CLI
/// keys — "codex" / "claude" / "gemini" / "opencode"). Session records
/// and projects match by their source name lowercased; Memory/Skill
/// records are multi-tagged (`preview` = "codex,opencode"), so only the
/// disabled tags are removed and the record drops when none remain.
///
/// CONVENTION (test-pinned): every session source name lowercases
/// EXACTLY to its cli key ("OpenCode" → "opencode"). The one hyphenated
/// key, "claude-desktop", has NO session source (the GUI app keeps no
/// history), so it never needs to survive this mapping — a future
/// source whose name doesn't lowercase to its key must switch this to
/// an explicit match.
fn apply_source_toggles(
    projects: &mut Vec<Project>,
    records: &mut Vec<AppSession>,
    disabled: &HashSet<String>,
) {
    if disabled.is_empty() {
        return;
    }
    projects.retain(|p| !disabled.contains(&p.source.to_ascii_lowercase()));
    records.retain_mut(|r| match r.source.as_str() {
        "Memory" | "Skill" => {
            let kept: Vec<&str> = r
                .preview
                .split(',')
                .map(str::trim)
                .filter(|tag| !tag.is_empty() && !disabled.contains(*tag))
                .collect();
            if kept.is_empty() {
                return false;
            }
            r.preview = kept.join(",");
            true
        }
        source => !disabled.contains(&source.to_ascii_lowercase()),
    });
}

/// Enumerate the project FOLDERS each folder-based CLI keeps on disk — the
/// project list, independent of whether any record lives inside. Claude: every
/// slug dir under `~/.claude/projects/`. Gemini: every `~/.gemini/tmp/<id>/`
/// carrying a `.project_root` marker.
fn scan_project_folders() -> Vec<Project> {
    let mut projects = Vec::new();
    if let Some(root) = claude_projects_root() {
        projects.extend(claude_project_folders_in(&root));
    }
    if let Some(tmp) = gemini_tmp_root() {
        projects.extend(gemini_project_folders_in(&tmp));
    }
    projects
}

/// Every slug dir under `~/.claude/projects/` is a Claude project. The cwd is
/// read from a sibling session (authoritative) or, for a session-less dir,
/// decoded from the slug (lossy fallback). A real slug is a sanitized absolute
/// path, so it always contains `-`; skip plain single-word dirs.
fn claude_project_folders_in(projects: &Path) -> Vec<Project> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(projects) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(slug) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !slug.contains('-') {
            continue;
        }
        let cwd =
            claude_cwd_from_project_dir(&dir).unwrap_or_else(|| decode_claude_project_slug(slug));
        out.push(Project {
            source: "Claude".to_string(),
            project: cwd,
        });
    }
    out
}

/// Every `~/.gemini/tmp/<id>/` dir with a `.project_root` marker is a Gemini
/// project (the marker excludes `bin` and other non-project temp dirs); the
/// project path is the marker's contents.
fn gemini_project_folders_in(tmp: &Path) -> Vec<Project> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(tmp) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.is_dir() {
            if let Some(root) = gemini_project_root(&dir) {
                out.push(Project {
                    source: "Gemini".to_string(),
                    project: root,
                });
            }
        }
    }
    out
}

/// OpenCode persists projects in a `project` table independently of sessions
/// (deleting/archiving a session never removes the row), so a project with no
/// live session still exists. Surface those session-less rows as projects keyed
/// by `worktree` — projects WITH sessions come from `add_record_projects` (by
/// the session's `directory`), so the two don't overlap.
fn opencode_empty_projects() -> Vec<Project> {
    let Some(home) = crate::home_dir() else {
        return Vec::new();
    };
    let db = opencode_data_dir(&home).join("opencode.db");
    if !db.exists() {
        return Vec::new();
    }
    opencode_empty_projects_in(&db).unwrap_or_default()
}

fn opencode_empty_projects_in(path: &Path) -> Result<Vec<Project>, Box<dyn Error>> {
    if fs::metadata(path).map(|m| m.len() == 0).unwrap_or(false) {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if !table_exists(&conn, "project")? || !table_exists(&conn, "session")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "select worktree from project p \
         where not exists ( \
            select 1 from session s \
            where s.project_id = p.id and s.parent_id is null and s.time_archived is null \
         )",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for worktree in rows.flatten() {
        if !worktree.is_empty() {
            out.push(Project {
                source: "OpenCode".to_string(),
                project: worktree,
            });
        }
    }
    Ok(out)
}

/// Fill in projects for CLIs that keep no enumerable folder entity (Codex;
/// OpenCode sessions are keyed by `directory`): each session-type record's
/// (source, cwd) becomes a project if not already present. Memory / Skill are
/// excluded — they're contents, and a folder-based CLI's projects already come
/// from disk.
fn add_record_projects(projects: &mut Vec<Project>, records: &[AppSession]) {
    let mut seen: HashSet<(String, String)> = projects
        .iter()
        .map(|p| (p.source.clone(), p.project.clone()))
        .collect();
    for s in records {
        if s.source == "Memory" || s.source == "Skill" || s.project.is_empty() {
            continue;
        }
        if seen.insert((s.source.clone(), s.project.clone())) {
            projects.push(Project {
                source: s.source.clone(),
                project: s.project.clone(),
            });
        }
    }
}

/// Lookup table `(source, id) → path` for every record we returned to
/// the frontend in the most recent `scan_sessions()` call. Updated
/// atomically (replace, not grow) by every scan, including watcher
/// ticks and Recheck.
///
/// **Security boundary**: `path` is never accepted as an IPC argument.
/// The frontend only sends `(source, id)` — we look up the path here
/// and refuse anything that wasn't part of the most recent scan. This
/// inverts the previous design (where path came from the frontend and
/// was validated against a path allow-list); a path the renderer
/// doesn't know about can no longer reach `parse_doc_file` /
/// `parse_codex_session` / etc. at all, regardless of whether a
/// renderer-side XSS / prompt-injection vector ever materializes.
static SESSION_PATH_INDEX: std::sync::Mutex<Option<HashMap<(String, String), std::path::PathBuf>>> =
    std::sync::Mutex::new(None);

fn refresh_session_path_index(sessions: &[AppSession]) {
    let map: HashMap<(String, String), std::path::PathBuf> = sessions
        .iter()
        .map(|s| {
            (
                (s.source.clone(), s.id.clone()),
                std::path::PathBuf::from(&s.path),
            )
        })
        .collect();
    if let Ok(mut guard) = SESSION_PATH_INDEX.lock() {
        *guard = Some(map);
    }
}

fn lookup_session_path(source: &str, id: &str) -> Option<std::path::PathBuf> {
    SESSION_PATH_INDEX
        .lock()
        .ok()?
        .as_ref()?
        .get(&(source.to_string(), id.to_string()))
        .cloned()
}

// ===================================================================
// Grok Build (xAI) — sessions
// ===================================================================
//
// xAI's Grok Build is NOT open source, so the on-disk layout itself is
// the verified reference (surveyed on real data, grok 0.2.93):
//
//   ~/.grok/sessions/<percent-encoded-cwd>/<uuid>/summary.json
//   ~/.grok/sessions/<percent-encoded-cwd>/<uuid>/updates.jsonl
//
// summary.json carries the LIST metadata verbatim (`info.id`,
// `info.cwd`, `session_summary` title, `created_at`/`updated_at`,
// `num_chat_messages`, `current_model_id`) — read as-is, no invented
// fallbacks. updates.jsonl is an ACP (Agent Client Protocol)
// `session/update` event stream; see `parse_grok_session`.

fn grok_sessions_root() -> Option<PathBuf> {
    // Honors $GROK_HOME (docs.x.ai/build/settings) like the rest of the
    // grok paths.
    crate::providers::grok_home_dir().map(|d| d.join("sessions"))
}

fn scan_grok() -> Result<Vec<AppSession>, Box<dyn Error>> {
    match grok_sessions_root() {
        Some(root) if root.is_dir() => scan_grok_sessions_in(&root),
        _ => Ok(Vec::new()),
    }
}

fn scan_grok_sessions_in(root: &Path) -> Result<Vec<AppSession>, Box<dyn Error>> {
    let mut out = Vec::new();
    for cwd_entry in fs::read_dir(root)?.flatten() {
        let cwd_dir = cwd_entry.path();
        // Skips the sibling session_search.sqlite / lock files.
        if !cwd_dir.is_dir() {
            continue;
        }
        let Ok(sessions) = fs::read_dir(&cwd_dir) else {
            continue;
        };
        for sess in sessions.flatten() {
            let dir = sess.path();
            // Skips per-project prompt_history.jsonl.
            if !dir.is_dir() {
                continue;
            }
            if let Some(app) = grok_session_from_summary(&dir) {
                out.push(app);
            }
        }
    }
    Ok(out)
}

fn grok_session_from_summary(session_dir: &Path) -> Option<AppSession> {
    let summary: Value =
        serde_json::from_str(&fs::read_to_string(session_dir.join("summary.json")).ok()?).ok()?;
    let id = summary.get("info")?.get("id")?.as_str()?.to_string();
    let updates = session_dir.join("updates.jsonl");
    if !updates.is_file() {
        return None;
    }
    let str_of = |key: &str| {
        summary
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    Some(AppSession {
        id,
        source: "Grok".to_string(),
        title: str_of("session_summary").unwrap_or_default(),
        project: summary["info"]["cwd"].as_str().unwrap_or("").to_string(),
        path: updates.to_string_lossy().into_owned(),
        started_at: str_of("created_at"),
        updated_at: str_of("updated_at"),
        message_count: summary
            .get("num_chat_messages")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        model: str_of("current_model_id"),
        ..Default::default()
    })
}

/// Grok's own per-tool display label + the rawInput arg worth showing —
/// both read from the RECORDED stream (`_meta["x.ai/tool"].label` is the
/// label Grok Build itself renders; closed-source, so the recording is
/// the reference). Observed labels: read_file→Read, grep→Search,
/// run_terminal_command→Run Command, search_replace→Edit,
/// list_dir→List Files, todo_write→Plan.
fn grok_tool_arg(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let first = |keys: &[&str]| {
        keys.iter()
            .map(|k| s(k))
            .find(|v| !v.is_empty())
            .unwrap_or("")
            .to_string()
    };
    match name {
        "read_file" => first(&["target_file"]),
        "grep" => first(&["pattern"]),
        "run_terminal_command" => first(&["command"]),
        "search_replace" => first(&["file_path"]),
        "list_dir" => first(&["target_directory"]),
        "todo_write" => String::new(),
        _ => {
            // Unknown tool — a compact single-line JSON of the input,
            // mirroring the Codex generic fallback.
            let generic = first(&["path", "file_path", "command", "query", "pattern", "url"]);
            if !generic.is_empty() {
                generic
            } else if input.is_null() {
                String::new()
            } else {
                serde_json::to_string(input).unwrap_or_default()
            }
        }
    }
}

/// Pull the human-readable body out of a Grok `tool_call_update`
/// rawOutput. Shape observed on real data: either a plain string or a
/// one-variant envelope `{"type": "<Variant>", "<Variant>": {"content":
/// "…", …}}` (e.g. ListDir/Content). Falls back to pretty JSON so
/// nothing recorded is dropped (LOCKED rule 7).
fn grok_output_text(raw: &Value) -> Option<String> {
    if let Some(s) = raw.as_str() {
        return (!s.trim().is_empty()).then(|| s.to_string());
    }
    let obj = raw.as_object()?;
    // Direct content field, or any variant object carrying one.
    let content = obj
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            obj.values().find_map(|v| {
                v.get("content")
                    .or_else(|| v.get("summary_for_prompt"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
            })
        });
    match content {
        Some(c) if !c.trim().is_empty() => Some(c),
        _ => serde_json::to_string_pretty(raw)
            .ok()
            .filter(|s| s != "null" && s != "{}"),
    }
}

/// Parse a Grok Build `updates.jsonl` — an ACP `session/update` stream.
/// Streaming `*_message_chunk` events of the same type merge into one
/// message; `tool_call` opens a unified `**{Label}**(arg)` card that the
/// terminal `tool_call_update` (`completed`/`failed`) finishes with the
/// `⏺`/`✗` marker + fenced output. `plan` renders as a GFM task list.
/// Intermediate tool updates, `retry_state` and `turn_completed` are
/// transport bookkeeping with no conversation content — skipped.
fn parse_grok_session(path: &Path) -> Result<SessionDetail, Box<dyn Error>> {
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::with_capacity(256 * 1024, file);

    let mut messages: Vec<SessionMessage> = Vec::new();
    // Open streaming-chunk accumulator: (kind, buffered text).
    let mut open_chunk: Option<(String, String)> = None;
    // toolCallId → messages index, for folding updates into the card.
    let mut tool_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    fn flush(open: &mut Option<(String, String)>, messages: &mut Vec<SessionMessage>) {
        let Some((kind, text)) = open.take() else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        let (role, kind_str, body) = match kind.as_str() {
            "user_message_chunk" => ("user", "text", text),
            "agent_thought_chunk" => ("assistant", "reasoning", format_reasoning_body(&text)),
            _ => ("assistant", "text", text),
        };
        messages.push(SessionMessage {
            role: role.to_string(),
            text: body,
            timestamp: None,
            kind: kind_str.to_string(),
            ..Default::default()
        });
    }

    for line in std::io::BufRead::lines(reader) {
        let Ok(line) = line else { continue };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(update) = event.pointer("/params/update") else {
            continue;
        };
        let kind = update
            .get("sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match kind {
            "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
                let text = update
                    .pointer("/content/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match &mut open_chunk {
                    Some((open_kind, buf)) if open_kind == kind => buf.push_str(text),
                    _ => {
                        flush(&mut open_chunk, &mut messages);
                        open_chunk = Some((kind.to_string(), text.to_string()));
                    }
                }
            }
            "tool_call" => {
                flush(&mut open_chunk, &mut messages);
                let meta = update.pointer("/_meta/x.ai~1tool");
                // JSON pointer escapes '/' as ~1 — the meta key is
                // literally "x.ai/tool".
                let name = meta
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .or_else(|| update.get("title").and_then(|v| v.as_str()))
                    .unwrap_or("tool");
                let label = meta
                    .and_then(|m| m.get("label"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(name);
                let arg = grok_tool_arg(name, update.get("rawInput").unwrap_or(&Value::Null));
                let header = if arg.is_empty() {
                    format!("**{label}**")
                } else {
                    format!("**{label}**({})", wrap_inline_code(&arg))
                };
                if let Some(id) = update.get("toolCallId").and_then(|v| v.as_str()) {
                    tool_index.insert(id.to_string(), messages.len());
                }
                messages.push(SessionMessage {
                    role: "tool".to_string(),
                    text: format!("⏺ {header}"),
                    timestamp: None,
                    kind: "tool_use".to_string(),
                    ..Default::default()
                });
            }
            "tool_call_update" => {
                let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
                if status != "completed" && status != "failed" {
                    continue; // intermediate progress — bookkeeping only
                }
                let Some(idx) = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .and_then(|id| tool_index.get(id).copied())
                else {
                    continue;
                };
                let msg = &mut messages[idx];
                if status == "failed" {
                    msg.text = msg.text.replacen("⏺ ", "✗ ", 1);
                    msg.kind = "tool_error".to_string();
                }
                if let Some(body) = update.get("rawOutput").and_then(|o| grok_output_text(o)) {
                    msg.text
                        .push_str(&format!("\n\n````\n{}\n````", body.trim_end()));
                }
            }
            "plan" => {
                flush(&mut open_chunk, &mut messages);
                let entries = update
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut body = String::from("**Plan**");
                for e in &entries {
                    let content = e.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let status = e.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    let mark = match status {
                        "completed" => "x",
                        "in_progress" => "~",
                        _ => " ",
                    };
                    body.push_str(&format!("\n- [{mark}] {content}"));
                }
                if !entries.is_empty() {
                    messages.push(SessionMessage {
                        role: "tool".to_string(),
                        text: body,
                        timestamp: None,
                        kind: "plan".to_string(),
                        ..Default::default()
                    });
                }
            }
            // retry_state / turn_completed / unknown — transport
            // bookkeeping, no conversation content.
            _ => {}
        }
    }
    flush(&mut open_chunk, &mut messages);

    let session =
        grok_session_from_summary(path.parent().unwrap_or(Path::new(""))).unwrap_or_else(|| {
            AppSession {
                source: "Grok".to_string(),
                path: path.to_string_lossy().into_owned(),
                ..Default::default()
            }
        });
    Ok(SessionDetail { session, messages })
}

pub fn get_session(source: &str, id: &str) -> Result<SessionDetail, Box<dyn Error>> {
    let path = lookup_session_path(source, id).ok_or_else(|| {
        format!("unknown session (call scan_all_sessions first): source={source} id={id}")
    })?;
    let p = path.as_path();
    let path_str = p.to_string_lossy();
    match source {
        "Codex" => parse_codex_session(p, id),
        "Claude" => parse_claude_session(p),
        "Grok" => parse_grok_session(p),
        "Memory" => parse_doc_file(p, "Memory"),
        "Skill" => parse_doc_file(p, "Skill"),
        "Gemini" if path_str.ends_with(".jsonl") => parse_gemini_jsonl_session(p),
        "Gemini" if path_str.ends_with(".json") => parse_gemini_json_session(p),
        "OpenCode" if path_str.ends_with(".db") => parse_opencode_db_session(p, id),
        "OpenCode" => parse_opencode_storage_session(p),
        _ => Err(format!("unsupported source: {source}").into()),
    }
}

/// Stream a Codex rollout JSONL and return:
///   1. Cumulative session-total token usage (from the LAST
///      `event_msg.token_count` event — Codex emits cumulative values)
///   2. The model name from `turn_context.payload.model` (current Codex —
///      `SessionMeta` has no model field, only `model_provider`; falls back
///      to `session_meta.payload.model` for older rollouts). Last-seen wins.
///   3. Per-day token spend, computed by diffing consecutive
///      `token_count` events. Each event carries the cumulative
///      total at its timestamp, so `current - previous` is the
///      delta produced between the two events, which we attribute
///      to the day the new event landed on.
///
/// Mirrors the detail-time logic in `parse_codex_session` for (1)
/// and (2); (3) is new and avoids the multi-day attribution bug
/// where one long Codex thread shows all tokens on its last update.
fn codex_scan_extract(path: &Path) -> ScanExtract {
    // Stream line-by-line instead of read_to_string — rollout files run
    // tens of MB and the scan visits every one of them; holding whole
    // files in memory inflates the peak footprint for no benefit.
    let Ok(file) = fs::File::open(path) else {
        return ScanExtract::default();
    };
    let reader = std::io::BufReader::with_capacity(SCAN_READ_BUF_SIZE, file);
    let mut tokens: Option<TokenStats> = None;
    let mut model: Option<String> = None;
    let mut message_count = 0usize;
    // [input, output, cached, reasoning, messages_count]
    let mut daily: BTreeMap<String, [u64; 5]> = BTreeMap::new();
    let mut daily_hours: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    let mut daily_hour_tokens: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    // Previous cumulative usage we've seen (any null `info` event
    // resets, so a fresh delta starts from zero on the next one).
    let mut prev_cumulative: Option<[u64; 4]> = None;
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Visible message count, in the same pass — this used to be a
        // separate uncached full-file parse per scan (the CPU hot spot).
        if codex_message_from_value(&value).is_some() {
            message_count += 1;
        }
        // Model id: current Codex records it in `turn_context.payload.model`
        // (TurnContextItem.model, persisted once per user turn — the model can
        // change mid-session via /model, so last-seen wins). `SessionMeta` has
        // NO `model` field (only `model_provider`), but older/odd rollouts may
        // still carry `session_meta.payload.model`, so read both — turn_context
        // appears after session_meta and on every turn, so it naturally wins.
        match value.get("type").and_then(Value::as_str) {
            Some("turn_context") | Some("session_meta") => {
                if let Some(m) = value
                    .get("payload")
                    .and_then(|p| p.get("model"))
                    .and_then(value_to_string)
                {
                    if !m.is_empty() {
                        model = Some(m);
                    }
                }
            }
            _ => {}
        }
        if value.get("type").and_then(Value::as_str) == Some("event_msg") {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("token_count") {
                    if let Some(usage) =
                        payload.get("info").and_then(|v| v.get("total_token_usage"))
                    {
                        let input = usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cached = usage
                            .get("cached_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let output = usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let reasoning = usage
                            .get("reasoning_output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let total = usage
                            .get("total_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(input + output + cached + reasoning);
                        tokens = Some(TokenStats {
                            input,
                            output,
                            cached,
                            reasoning,
                            total,
                        });

                        // Attribute the delta (current cumulative -
                        // previous cumulative) to the day this event
                        // arrived on. `saturating_sub` defends against
                        // resets where the cumulative resets to zero
                        // partway through.
                        let current = [input, output, cached, reasoning];
                        let date_hour = value
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .and_then(iso_to_local_date_hour);
                        if let Some((date, hour)) = date_hour {
                            let delta = match prev_cumulative {
                                Some(prev) => [
                                    current[0].saturating_sub(prev[0]),
                                    current[1].saturating_sub(prev[1]),
                                    current[2].saturating_sub(prev[2]),
                                    current[3].saturating_sub(prev[3]),
                                ],
                                None => current,
                            };
                            let bucket = daily.entry(date.clone()).or_insert([0u64; 5]);
                            bucket[0] += delta[0];
                            bucket[1] += delta[1];
                            bucket[2] += delta[2];
                            bucket[3] += delta[3];
                            bucket[4] += 1; // one AI interaction
                            let hourbox = daily_hours.entry(date.clone()).or_insert([0u64; 24]);
                            hourbox[hour as usize] += 1;
                            let delta_total = delta[0] + delta[1] + delta[2] + delta[3];
                            let tokbox = daily_hour_tokens.entry(date).or_insert([0u64; 24]);
                            tokbox[hour as usize] += delta_total;
                        }
                        prev_cumulative = Some(current);
                    }
                }
            }
        }
    }
    let daily_out: Option<Vec<DailyTokenBreakdown>> = if daily.is_empty() {
        None
    } else {
        Some(
            daily
                .into_iter()
                .map(|(date, [i, o, c, r, m])| {
                    let hours = daily_hours
                        .get(&date)
                        .map(|a| a.to_vec())
                        .unwrap_or_default();
                    let hour_tokens = daily_hour_tokens
                        .get(&date)
                        .map(|a| a.to_vec())
                        .unwrap_or_default();
                    DailyTokenBreakdown {
                        date,
                        tokens: TokenStats {
                            input: i,
                            output: o,
                            cached: c,
                            reasoning: r,
                            total: i + o + c + r,
                        },
                        messages: m,
                        hours,
                        hour_tokens,
                    }
                })
                .collect(),
        )
    };
    ScanExtract {
        tokens,
        model,
        daily_tokens: daily_out,
        message_count,
    }
}

fn scan_codex() -> Result<Vec<AppSession>, Box<dyn Error>> {
    let Some(home) = crate::home_dir() else {
        return Ok(Vec::new());
    };
    let state_db = crate::providers::codex_root(&home).join("state_5.sqlite");
    if !state_db.exists() {
        return Ok(Vec::new());
    }
    scan_codex_state_db(&state_db)
}

fn scan_codex_state_db(path: &Path) -> Result<Vec<AppSession>, Box<dyn Error>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Filter mirrors Codex's official thread listing:
    //   * codex-rs/state/src/runtime/threads.rs `push_thread_filters` requires
    //     `archived = 0` and `preview <> ''` for non-archived listings.
    //   * codex-rs/rollout/src/lib.rs `INTERACTIVE_SESSION_SOURCES` lists the
    //     four sources surfaced in the resume picker: cli, vscode, atlas,
    //     chatgpt. SessionSource is serialized with `rename_all = "lowercase"`,
    //     and `Custom("atlas")` / `Custom("chatgpt")` round-trip as their
    //     inner string ("atlas" / "chatgpt") in the `source` column.
    // The `preview` column was added in migration 0032_threads_preview.sql;
    // older state_5.sqlite files predate it, so we omit the filter when the
    // column is absent.
    let preview_clause = if column_exists(&conn, "threads", "preview")? {
        " and preview <> ''"
    } else {
        ""
    };
    // Include archived threads (`archived` not filtered) — Termory is a
    // history browser and surfaces them with an "Archived" badge. The
    // column is selected so the badge can be set per row.
    let sql = format!(
        "select id, rollout_path, created_at, updated_at, cwd, title, first_user_message, archived \
         from threads \
         where 1 = 1{preview_clause} \
           and source in ('cli', 'vscode', 'atlas', 'chatgpt') \
         order by updated_at desc"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let rollout_path: String = row.get(1)?;
        let created_at: i64 = row.get(2)?;
        let updated_at: i64 = row.get(3)?;
        let cwd: String = row.get(4)?;
        let title: String = row.get(5).unwrap_or_default();
        let first_user_message: String = row.get(6).unwrap_or_default();
        let archived: bool = row.get::<_, i64>(7).unwrap_or(0) != 0;
        let display_title = codex_display_title(&title)
            .or_else(|| codex_display_title(&first_user_message))
            .unwrap_or_default();
        let snippet = snippet_line(&first_user_message);
        let extract = cached_scan_extract(Path::new(&rollout_path), codex_scan_extract);

        Ok(AppSession {
            id: id.clone(),
            source: "Codex".to_string(),
            title: display_title,
            project: cwd,
            path: rollout_path.clone(),
            started_at: normalize_time(created_at.to_string()),
            updated_at: normalize_time(updated_at.to_string()),
            message_count: extract.message_count,
            archived,
            preview: String::new(),
            snippet,
            message_previews: Vec::new(),
            tokens: extract.tokens,
            model: extract.model,
            daily_tokens: extract.daily_tokens,
        })
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        let session = row?;
        if Path::new(&session.path).exists() {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

/// Stream a Claude project JSONL and return:
///   1. Aggregate `message.usage` across the whole session
///   2. Last seen `message.model` (skipping `<synthetic>` placeholders)
///   3. Per-day token spend, bucketed by each message's `timestamp`.
///      Anthropic stamps every assistant turn with a real ISO time,
///      so this gives accurate per-day attribution for `--continue`
///      sessions that span multiple days.
///
/// Same per-message arithmetic as `collect_claude_metadata`'s token
/// branch (input_tokens is summed-as-billed, not deduplicated — the
/// session total matches Anthropic's invoice).
fn claude_scan_extract(path: &Path) -> ScanExtract {
    // Stream line-by-line instead of read_to_string — project JSONLs run
    // to 100+ MB and the scan visits every one of them.
    let Ok(file) = fs::File::open(path) else {
        return ScanExtract::default();
    };
    let reader = std::io::BufReader::with_capacity(SCAN_READ_BUF_SIZE, file);
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_creation = 0u64;
    let mut saw_any = false;
    let mut model: Option<String> = None;
    let mut message_count = 0usize;
    // [input, output, cache_read, cache_creation, messages] per local date
    let mut daily: BTreeMap<String, [u64; 5]> = BTreeMap::new();
    let mut daily_hours: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    let mut daily_hour_tokens: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Visible message count, in the same pass — this used to be a
        // separate uncached full-file parse per scan (the CPU hot spot).
        // Counted BEFORE the `message`-field skip below: system /
        // attachment records have no `message` but still emit messages.
        message_count += claude_message_from_value(&value)
            .iter()
            .filter(|message| message.kind == kind::TEXT)
            .count();
        let date_hour = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso_to_local_date_hour);
        let date_key = date_hour.as_ref().map(|(d, _)| d.clone());
        let Some(message) = value.get("message") else {
            continue;
        };
        if let Some(usage) = message.get("usage") {
            let mut line_input = 0u64;
            let mut line_output = 0u64;
            let mut line_cache_read = 0u64;
            let mut line_cache_creation = 0u64;
            if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
                input += n;
                line_input = n;
                saw_any = true;
            }
            if let Some(n) = usage.get("output_tokens").and_then(Value::as_u64) {
                output += n;
                line_output = n;
                saw_any = true;
            }
            if let Some(n) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
                cache_read += n;
                line_cache_read = n;
                saw_any = true;
            }
            if let Some(n) = usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
            {
                cache_creation += n;
                line_cache_creation = n;
                saw_any = true;
            }
            if let Some(date) = date_key.clone() {
                let bucket = daily.entry(date.clone()).or_insert([0u64; 5]);
                bucket[0] += line_input;
                bucket[1] += line_output;
                bucket[2] += line_cache_read;
                bucket[3] += line_cache_creation;
                bucket[4] += 1; // one assistant message
                if let Some((_, hour)) = date_hour.as_ref() {
                    let hourbox = daily_hours.entry(date.clone()).or_insert([0u64; 24]);
                    hourbox[*hour as usize] += 1;
                    let line_total =
                        line_input + line_output + line_cache_read + line_cache_creation;
                    let tokbox = daily_hour_tokens.entry(date).or_insert([0u64; 24]);
                    tokbox[*hour as usize] += line_total;
                }
            }
        }
        if let Some(m) = message.get("model").and_then(value_to_string) {
            if !m.starts_with('<') {
                model = Some(m);
            }
        }
    }
    let tokens = if saw_any {
        let cached = cache_read + cache_creation;
        Some(TokenStats {
            input,
            output,
            cached,
            // Claude folds reasoning into output_tokens — no separate
            // breakout to surface here.
            reasoning: 0,
            total: input + output + cached,
        })
    } else {
        None
    };
    let daily_out: Option<Vec<DailyTokenBreakdown>> = if daily.is_empty() {
        None
    } else {
        Some(
            daily
                .into_iter()
                .map(|(date, [i, o, cr, cc, m])| {
                    let cached = cr + cc;
                    let hours = daily_hours
                        .get(&date)
                        .map(|a| a.to_vec())
                        .unwrap_or_default();
                    let hour_tokens = daily_hour_tokens
                        .get(&date)
                        .map(|a| a.to_vec())
                        .unwrap_or_default();
                    DailyTokenBreakdown {
                        date,
                        tokens: TokenStats {
                            input: i,
                            output: o,
                            cached,
                            reasoning: 0,
                            total: i + o + cached,
                        },
                        messages: m,
                        hours,
                        hour_tokens,
                    }
                })
                .collect(),
        )
    };
    ScanExtract {
        tokens,
        model,
        daily_tokens: daily_out,
        message_count,
    }
}

fn scan_claude() -> Result<Vec<AppSession>, Box<dyn Error>> {
    let Some(home) = crate::home_dir() else {
        return Ok(Vec::new());
    };
    let root = claude_config_root(&home).join("projects");
    scan_claude_projects(&root)
}

fn scan_claude_projects(root: &Path) -> Result<Vec<AppSession>, Box<dyn Error>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    // Sessions only — the project LIST comes from `scan_project_folders`, which
    // enumerates the slug dirs directly (so empty / memory-only projects show
    // regardless of records). This function returns just the session records.
    let mut sessions = Vec::new();
    for project_entry in fs::read_dir(root)? {
        let project_entry = project_entry?;
        let project_dir = project_entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&project_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || !is_claude_session_file(&path) {
                continue;
            }
            if let Ok(lite) = parse_claude_lite_session(&path, None) {
                let extract = cached_scan_extract(&path, claude_scan_extract);
                sessions.push(AppSession {
                    id: lite.id,
                    source: "Claude".to_string(),
                    title: lite.title,
                    project: lite.project,
                    path: path.display().to_string(),
                    started_at: lite.started_at,
                    updated_at: lite.updated_at,
                    message_count: extract.message_count,
                    archived: false,
                    preview: String::new(),
                    snippet: lite.snippet,
                    message_previews: Vec::new(),
                    tokens: extract.tokens,
                    model: extract.model,
                    daily_tokens: extract.daily_tokens,
                });
            }
        }
    }

    let mut latest_by_id: HashMap<String, AppSession> = HashMap::new();
    for session in sessions {
        let replace = latest_by_id
            .get(&session.id)
            .map(|current| session.updated_at > current.updated_at)
            .unwrap_or(true);
        if replace {
            latest_by_id.insert(session.id.clone(), session);
        }
    }
    let mut sessions = latest_by_id.into_values().collect::<Vec<_>>();
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(sessions)
}

fn scan_gemini() -> Result<Vec<AppSession>, Box<dyn Error>> {
    let Some(home) = crate::home_dir() else {
        return Ok(Vec::new());
    };
    let tmp_dir = home.join(".gemini").join("tmp");
    if !tmp_dir.exists() {
        return Ok(Vec::new());
    }
    // Sessions only — the project LIST comes from `scan_project_folders`, which
    // enumerates the `~/.gemini/tmp/<id>/` dirs (with `.project_root`) directly.
    let mut sessions = Vec::new();
    for entry in fs::read_dir(tmp_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let chats_dir = path.join("chats");
        if chats_dir.exists() {
            sessions.extend(scan_gemini_chats_dir(&chats_dir)?);
        }
    }

    let mut latest_by_id: HashMap<String, AppSession> = HashMap::new();
    for session in sessions {
        let replace = latest_by_id
            .get(&session.id)
            .map(|current| session.updated_at > current.updated_at)
            .unwrap_or(true);
        if replace {
            latest_by_id.insert(session.id.clone(), session);
        }
    }
    let mut sessions = latest_by_id.into_values().collect::<Vec<_>>();
    sessions.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    Ok(sessions)
}

/// The project path recorded in `~/.gemini/tmp/<id>/.project_root`, if any.
fn gemini_project_root(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(".project_root"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn scan_opencode() -> Result<Vec<AppSession>, Box<dyn Error>> {
    let Some(home) = crate::home_dir() else {
        return Ok(Vec::new());
    };
    let root = opencode_data_dir(&home);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    let db = root.join("opencode.db");
    if db.exists() {
        sessions.extend(scan_opencode_db(&db)?);
    }
    for storage_root in opencode_storage_roots(&root) {
        sessions.extend(scan_opencode_storage(&storage_root)?);
    }

    let mut seen = HashSet::new();
    sessions.retain(|session| seen.insert(session.id.clone()));
    Ok(sessions)
}

fn scan_memory(project_cwds: &HashSet<String>) -> Result<Vec<AppSession>, Box<dyn Error>> {
    let mut sessions = Vec::new();
    sessions.extend(scan_claude_memory()?);
    sessions.extend(scan_claude_rules(project_cwds));
    sessions.extend(scan_codex_memory());
    sessions.extend(scan_gemini_memory());
    sessions.extend(scan_global_instructions());

    // Per the AGENTS.md spec the file is always usable by both Codex and
    // OpenCode, so project-level AGENTS.md is tagged with both regardless of
    // which tool had a session in the cwd.

    // Codex, Gemini CLI, and OpenCode all gate their ancestor walk on the
    // presence of a project-root marker (`.git` by default). Verified against
    // each tool's source:
    //   * Codex (codex-rs/core/src/agents_md.rs): "if no marker is found, only
    //     the current working directory is considered".
    //   * Gemini (packages/core/src/utils/memoryDiscovery.ts findProjectRoot):
    //     defaults to ['.git']; when null, ceiling = startDir → walk no-ops.
    //   * OpenCode (packages/opencode/src/project/project.ts): worktree comes
    //     from `git rev-parse --git-common-dir`; without git the worktree
    //     equals the cwd and findUp does not ascend.
    // We mirror that: scan cwd, then only ascend if a .git is found at cwd or
    // any ancestor up to (but not including) $HOME. Walk stops at the git root
    // (inclusive).
    let home_for_walk = crate::home_dir();
    for cwd in project_cwds {
        let cwd_path = Path::new(cwd);
        push_project_root_instruction_files(cwd_path, &mut sessions);

        let git_root = find_git_root(cwd_path, home_for_walk.as_deref());
        if let Some(git_root) = git_root {
            if git_root != cwd_path {
                let mut current = cwd_path.parent().map(|p| p.to_path_buf());
                while let Some(dir) = current {
                    if home_for_walk.as_deref() == Some(dir.as_path()) {
                        break;
                    }
                    push_ancestor_instruction_files(&dir, &mut sessions);
                    if dir == git_root {
                        break;
                    }
                    current = match dir.parent() {
                        Some(parent) if parent != dir => Some(parent.to_path_buf()),
                        _ => None,
                    };
                }
            }
        }
    }

    // Deduplicate by path — one entry per file even if multiple tools support it
    // (the tool list is encoded into preview).
    let mut seen = HashSet::new();
    sessions.retain(|s| seen.insert(s.path.clone()));
    Ok(sessions)
}

fn scan_global_instructions() -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    let claude_root = claude_config_root(&home);
    let claude_label = claude_root.to_string_lossy();
    // OpenCode officially falls back to ~/.claude/CLAUDE.md and project
    // CLAUDE.md when no AGENTS.md is found, per https://opencode.ai/docs/rules/
    push_tagged_instruction_file(
        &claude_root.join("CLAUDE.md"),
        &claude_label,
        &["claude", "opencode"],
        &mut sessions,
    );

    let codex_dir = crate::providers::codex_root(&home);
    let codex_label = codex_dir.to_string_lossy();
    push_tagged_instruction_file(
        &codex_dir.join("AGENTS.md"),
        &codex_label,
        &["codex"],
        &mut sessions,
    );
    push_tagged_instruction_file(
        &codex_dir.join("AGENTS.override.md"),
        &codex_label,
        &["codex"],
        &mut sessions,
    );

    let opencode_config = opencode_config_dir(&home);
    push_tagged_instruction_file(
        &opencode_config.join("AGENTS.md"),
        &opencode_config.to_string_lossy(),
        &["opencode"],
        &mut sessions,
    );

    sessions
}

// Walks up from `start` looking for a `.git` entry. Returns the first
// directory that contains one (the project root). Stops before $HOME (never
// returns the home directory itself) and at the filesystem root. Mirrors
// Codex's default `project_root_markers = [".git"]` lookup.
fn find_git_root(start: &Path, home: Option<&Path>) -> Option<std::path::PathBuf> {
    let mut cursor = start.to_path_buf();
    loop {
        if home == Some(cursor.as_path()) {
            return None;
        }
        if cursor.join(".git").exists() {
            return Some(cursor);
        }
        match cursor.parent() {
            Some(parent) if parent != cursor => cursor = parent.to_path_buf(),
            _ => return None,
        }
    }
}

// Instruction files at the project root (the session cwd). The .claude/CLAUDE.md
// layout only applies at the project root per Claude Code docs, not at every
// ancestor.
fn push_project_root_instruction_files(dir: &Path, sessions: &mut Vec<AppSession>) {
    let label = dir.to_string_lossy().to_string();
    push_tagged_instruction_file(
        &dir.join("CLAUDE.md"),
        &label,
        &["claude", "opencode"],
        sessions,
    );
    push_tagged_instruction_file(&dir.join("CLAUDE.local.md"), &label, &["claude"], sessions);
    push_tagged_instruction_file(
        &dir.join(".claude").join("CLAUDE.md"),
        &label,
        &["claude"],
        sessions,
    );
    push_tagged_instruction_file(&dir.join("GEMINI.md"), &label, &["gemini"], sessions);
    // MEMORY.md is Gemini's modern context file (legacy GEMINI.md alias)
    // per packages/core/src/tools/memoryTool.ts:12.
    push_tagged_instruction_file(&dir.join("MEMORY.md"), &label, &["gemini"], sessions);
    push_tagged_instruction_file(
        &dir.join("AGENTS.md"),
        &label,
        &["codex", "opencode"],
        sessions,
    );
    // AGENTS.override.md is Codex-only per
    // https://developers.openai.com/codex/guides/agents-md
    push_tagged_instruction_file(
        &dir.join("AGENTS.override.md"),
        &label,
        &["codex"],
        sessions,
    );
}

// Instruction files at an ancestor directory (above the session cwd, up the
// walk). Same set as project-root EXCEPT for .claude/CLAUDE.md, which is a
// project-root-only convention.
fn push_ancestor_instruction_files(dir: &Path, sessions: &mut Vec<AppSession>) {
    let label = dir.to_string_lossy().to_string();
    push_tagged_instruction_file(
        &dir.join("CLAUDE.md"),
        &label,
        &["claude", "opencode"],
        sessions,
    );
    push_tagged_instruction_file(&dir.join("CLAUDE.local.md"), &label, &["claude"], sessions);
    push_tagged_instruction_file(&dir.join("GEMINI.md"), &label, &["gemini"], sessions);
    push_tagged_instruction_file(&dir.join("MEMORY.md"), &label, &["gemini"], sessions);
    push_tagged_instruction_file(
        &dir.join("AGENTS.md"),
        &label,
        &["codex", "opencode"],
        sessions,
    );
    push_tagged_instruction_file(
        &dir.join("AGENTS.override.md"),
        &label,
        &["codex"],
        sessions,
    );
}

fn push_tagged_instruction_file(
    path: &Path,
    project_label: &str,
    tools: &[&str],
    sessions: &mut Vec<AppSession>,
) {
    if !path.is_file() {
        return;
    }
    if let Some(mut session) = memory_session_from_file(path, project_label) {
        session.preview = tools.join(",");
        sessions.push(session);
    }
}

// Claude Code "rules" are personal/project-wide markdown instructions per
// https://code.claude.com/docs/en/memory. `~/.claude/rules/**/*.md` apply
// globally, `<cwd>/.claude/rules/**/*.md` apply to that project. All .md files
// are discovered recursively.
fn scan_claude_rules(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    let claude_root = claude_config_root(&home);

    let global_dir = claude_root.join("rules");
    if global_dir.is_dir() {
        push_doc_files_recursive(
            &global_dir,
            &global_dir,
            &global_dir.to_string_lossy(),
            "claude",
            "Memory",
            &[],
            &mut sessions,
        );
    }

    for cwd in project_cwds {
        let project_dir = Path::new(cwd).join(".claude").join("rules");
        if project_dir.is_dir() {
            push_doc_files_recursive(
                &project_dir,
                &project_dir,
                cwd,
                "claude",
                "Memory",
                &[],
                &mut sessions,
            );
        }
    }

    sessions
}

fn scan_claude_memory() -> Result<Vec<AppSession>, Box<dyn Error>> {
    let Some(home) = crate::home_dir() else {
        return Ok(Vec::new());
    };
    let root = claude_config_root(&home).join("projects");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for project_entry in fs::read_dir(&root)? {
        let project_entry = project_entry?;
        let project_dir = project_entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        let memory_dir = project_dir.join("memory");
        if !memory_dir.is_dir() {
            continue;
        }
        let slug = project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        // Prefer the real cwd from a sibling session JSONL — the slug is
        // lossy (see claude_cwd_from_project_dir); fall back to slug decode
        // for memory-only projects with no session.
        let project_name = claude_cwd_from_project_dir(&project_dir)
            .unwrap_or_else(|| decode_claude_project_slug(&slug));
        push_memory_files_recursive(
            &memory_dir,
            &memory_dir,
            &project_name,
            "claude",
            &mut sessions,
        );
    }
    Ok(sessions)
}

fn scan_codex_memory() -> Vec<AppSession> {
    let Some(home) = crate::home_dir() else {
        return Vec::new();
    };
    let root = crate::providers::codex_root(&home).join("memories");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut sessions = Vec::new();
    push_doc_files_recursive(
        &root,
        &root,
        &root.to_string_lossy(),
        "codex",
        "Memory",
        &["skills"],
        &mut sessions,
    );
    sessions
}

fn scan_gemini_memory() -> Vec<AppSession> {
    let Some(home) = crate::home_dir() else {
        return Vec::new();
    };
    let mut sessions = Vec::new();

    // Global memory: ~/.gemini/{GEMINI.md,MEMORY.md}. Both filenames are
    // returned by getAllGeminiMdFilenames() in Gemini's source
    // (packages/core/src/tools/memoryTool.ts:11-12 and
    // packages/core/src/utils/memoryDiscovery.ts:323).
    let gemini_dir = home.join(".gemini");
    for filename in ["GEMINI.md", "MEMORY.md"] {
        let global_file = gemini_dir.join(filename);
        if global_file.is_file() {
            if let Some(mut session) = memory_session_from_file(&global_file, "~/.gemini") {
                session.preview = "gemini".to_string();
                sessions.push(session);
            }
        }
    }

    // Per-project memory: ~/.gemini/tmp/<id>/memory/{MEMORY.md preferred,
    // GEMINI.md legacy fallback}. Confirmed via
    // packages/core/src/config/storage.ts getProjectMemoryDir() →
    // getProjectMemoryTempDir() = path.join(getProjectTempDir(), 'memory').
    // The skills/ subdir is surfaced under Skills (scan_gemini_skills), so
    // it is skipped here.
    let tmp_dir = gemini_dir.join("tmp");
    if tmp_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&tmp_dir) {
            for entry in entries.flatten() {
                let project_dir = entry.path();
                if !project_dir.is_dir() {
                    continue;
                }
                let memory_dir = project_dir.join("memory");
                if !memory_dir.is_dir() {
                    continue;
                }
                let project_label = gemini_project_label(&project_dir);
                push_doc_files_recursive(
                    &memory_dir,
                    &memory_dir,
                    &project_label,
                    "gemini",
                    "Memory",
                    &["skills"],
                    &mut sessions,
                );
            }
        }
    }

    sessions
}

fn gemini_project_label(project_dir: &Path) -> String {
    let project_root = project_dir.join(".project_root");
    if let Ok(raw) = fs::read_to_string(&project_root) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

fn scan_skills(project_cwds: &HashSet<String>) -> Result<Vec<AppSession>, Box<dyn Error>> {
    let mut sessions = Vec::new();
    sessions.extend(scan_claude_skills(project_cwds));
    sessions.extend(scan_codex_skills(project_cwds));
    sessions.extend(scan_gemini_skills(project_cwds));
    sessions.extend(scan_opencode_skills(project_cwds));
    sessions.extend(scan_grok_skills(project_cwds));
    sessions.extend(scan_agents_skills(project_cwds));

    let mut seen = HashSet::new();
    sessions.retain(|s| seen.insert(s.path.clone()));
    Ok(sessions)
}

/// Grok Build skills — `~/.grok/skills/<name>/SKILL.md` (verified on the
/// real install, grok 0.2.93; closed-source, the on-disk layout is the
/// reference). Only the global dir is scanned — no per-project skills
/// convention has been observed for Grok Build yet.
fn scan_grok_skills(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    // Global: <grok-home>/skills ($GROK_HOME-aware). Official discovery
    // paths: docs.x.ai/build/features/skills-plugins-marketplaces.
    if let Some(global_dir) = crate::providers::grok_home_dir().map(|d| d.join("skills")) {
        if global_dir.is_dir() {
            push_doc_files_recursive(
                &global_dir,
                &global_dir,
                &global_dir.to_string_lossy(),
                "grok",
                "Skill",
                &[],
                &mut sessions,
            );
        }
    }
    // Project: <cwd>/.grok/skills (the doc's "./.grok/skills/", scanned
    // at cwd level like the other tools' project skill dirs).
    for cwd in project_cwds {
        let dir = Path::new(cwd).join(".grok").join("skills");
        if dir.is_dir() {
            push_doc_files_recursive(&dir, &dir, cwd, "grok", "Skill", &[], &mut sessions);
        }
    }
    sessions
}

fn scan_claude_skills(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    // OpenCode officially also reads .claude/skills/
    // (https://opencode.ai/docs/skills/), and Grok Build reads skills
    // from ~/.claude/ for Claude Code compatibility
    // (docs.x.ai/build/features/skills-plugins-marketplaces) — tag all
    // three.
    let tag = "claude,opencode,grok";

    let claude_root = claude_config_root(&home);
    let global_dir = claude_root.join("skills");
    if global_dir.is_dir() {
        push_doc_files_recursive(
            &global_dir,
            &global_dir,
            &global_dir.to_string_lossy(),
            tag,
            "Skill",
            &[],
            &mut sessions,
        );
    }

    for cwd in project_cwds {
        let project_dir = Path::new(cwd).join(".claude").join("skills");
        if project_dir.is_dir() {
            push_doc_files_recursive(
                &project_dir,
                &project_dir,
                cwd,
                tag,
                "Skill",
                &[],
                &mut sessions,
            );
        }
    }

    sessions
}

fn scan_codex_skills(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    // Codex stores user skills at $CODEX_HOME/skills/, verified in
    // codex-rs/core/src/session/tests.rs (`codex_home.join("skills")`) and
    // codex-rs/core/tests/suite/compact_remote_parity.rs
    // (`<CODEX_HOME>/skills/.system/imagegen/SKILL.md`).
    let global_dir = crate::providers::codex_root(&home).join("skills");
    if global_dir.is_dir() {
        push_doc_files_recursive(
            &global_dir,
            &global_dir,
            &global_dir.to_string_lossy(),
            "codex",
            "Skill",
            &[],
            &mut sessions,
        );
    }

    for cwd in project_cwds {
        let project_dir = Path::new(cwd).join(".codex").join("skills");
        if project_dir.is_dir() {
            push_doc_files_recursive(
                &project_dir,
                &project_dir,
                cwd,
                "codex",
                "Skill",
                &[],
                &mut sessions,
            );
        }
    }

    sessions
}

fn scan_gemini_skills(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    let global_dir = home.join(".gemini").join("skills");
    if global_dir.is_dir() {
        push_doc_files_recursive(
            &global_dir,
            &global_dir,
            "~/.gemini/skills",
            "gemini",
            "Skill",
            &[],
            &mut sessions,
        );
    }

    // Per-project skills: ~/.gemini/tmp/<id>/memory/skills/. The Gemini docs
    // are misleading here — they mention tmp/<hash>/ as chat checkpoints — but
    // the source code (packages/core/src/config/storage.ts) clearly defines
    // getProjectSkillsMemoryDir() as path.join(getProjectMemoryTempDir(),
    // 'skills'), which resolves to this exact path.
    let tmp_dir = home.join(".gemini").join("tmp");
    if tmp_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&tmp_dir) {
            for entry in entries.flatten() {
                let project_dir = entry.path();
                if !project_dir.is_dir() {
                    continue;
                }
                let skills_dir = project_dir.join("memory").join("skills");
                if !skills_dir.is_dir() {
                    continue;
                }
                let project_label = gemini_project_label(&project_dir);
                push_doc_files_recursive(
                    &skills_dir,
                    &skills_dir,
                    &project_label,
                    "gemini",
                    "Skill",
                    &[],
                    &mut sessions,
                );
            }
        }
    }

    for cwd in project_cwds {
        let project_dir = Path::new(cwd).join(".gemini").join("skills");
        if project_dir.is_dir() {
            push_doc_files_recursive(
                &project_dir,
                &project_dir,
                cwd,
                "gemini",
                "Skill",
                &[],
                &mut sessions,
            );
        }
    }

    sessions
}

fn scan_opencode_skills(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    let global_dir = opencode_config_dir(&home).join("skills");
    if global_dir.is_dir() {
        push_doc_files_recursive(
            &global_dir,
            &global_dir,
            &global_dir.to_string_lossy(),
            "opencode",
            "Skill",
            &[],
            &mut sessions,
        );
    }

    for cwd in project_cwds {
        let project_dir = Path::new(cwd).join(".opencode").join("skills");
        if project_dir.is_dir() {
            push_doc_files_recursive(
                &project_dir,
                &project_dir,
                cwd,
                "opencode",
                "Skill",
                &[],
                &mut sessions,
            );
        }
    }

    sessions
}

// Tool-neutral skills location. Codex, Gemini CLI, and OpenCode all officially
// read from ~/.agents/skills/ (global) and <cwd>/.agents/skills/ (project) as
// an interoperable, cross-tool path. Claude Code does not currently read from
// this location.
fn scan_agents_skills(project_cwds: &HashSet<String>) -> Vec<AppSession> {
    let mut sessions = Vec::new();
    let Some(home) = crate::home_dir() else {
        return sessions;
    };

    let tag = "codex,gemini,opencode";
    // Grok Build additionally discovers USER-level ~/.agents/skills
    // (docs.x.ai/build/features/skills-plugins-marketplaces); it does
    // not document the project-level .agents/skills, so only the global
    // dir gets the grok tag.
    let global_tag = "codex,gemini,opencode,grok";

    let global_dir = home.join(".agents").join("skills");
    if global_dir.is_dir() {
        push_doc_files_recursive(
            &global_dir,
            &global_dir,
            "~/.agents/skills",
            global_tag,
            "Skill",
            &[],
            &mut sessions,
        );
    }

    for cwd in project_cwds {
        let project_dir = Path::new(cwd).join(".agents").join("skills");
        if project_dir.is_dir() {
            push_doc_files_recursive(
                &project_dir,
                &project_dir,
                cwd,
                tag,
                "Skill",
                &[],
                &mut sessions,
            );
        }
    }

    sessions
}

fn push_memory_files_recursive(
    dir: &Path,
    base: &Path,
    project_label: &str,
    tool_tag: &str,
    sessions: &mut Vec<AppSession>,
) {
    push_doc_files_recursive(dir, base, project_label, tool_tag, "Memory", &[], sessions);
}

fn push_doc_files_recursive(
    dir: &Path,
    base: &Path,
    project_label: &str,
    tool_tag: &str,
    source: &str,
    skip_dirs: &[&str],
    sessions: &mut Vec<AppSession>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if skip_dirs.iter().any(|d| name_str == *d) {
                continue;
            }
            push_doc_files_recursive(
                &path,
                base,
                project_label,
                tool_tag,
                source,
                skip_dirs,
                sessions,
            );
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let ext_match = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !ext_match {
            continue;
        }
        let Some(mut session) = doc_session_from_file(&path, project_label, source) else {
            continue;
        };
        session.preview = tool_tag.to_string();
        if let Ok(rel) = path.strip_prefix(base) {
            let rel_str = rel.to_string_lossy().to_string();
            if !rel_str.is_empty() {
                session.title = rel_str.clone();
                session.id = rel_str;
            }
        }
        sessions.push(session);
    }
}

fn decode_claude_project_slug(slug: &str) -> String {
    if slug.is_empty() {
        return String::new();
    }
    // Claude sanitizes a cwd into the slug by replacing `/` separators with
    // `-`. A directory name that ITSELF contains `-` (e.g. `copilot-is`)
    // then becomes indistinguishable from a separator, so a blind
    // `-`→`/` decode would split `…/copilot-is` into `…/copilot/is`.
    // Reverse it against the filesystem: at each component take the LONGEST
    // run of `-`-joined segments that names an existing directory, so a real
    // `copilot-is` folder is recovered. When nothing exists on disk
    // (project moved/deleted) this degrades to the naive `-`→`/` decode.
    let (base, body) = match slug.strip_prefix('-') {
        Some(rest) => (std::path::PathBuf::from("/"), rest),
        None => (std::path::PathBuf::new(), slug),
    };
    let segments: Vec<&str> = body.split('-').collect();
    resolve_dashed_path(&base, &segments)
        .to_string_lossy()
        .into_owned()
}

/// The real project cwd for a Claude slug dir (`~/.claude/projects/<slug>/`),
/// read from the `cwd` field on a sibling session JSONL's first records.
/// This is authoritative: the slug itself is lossy — Claude replaces `/`,
/// `.`, `_` etc. ALL with `-`, so `copilot.is`, `copilot-is` and `copilot/is`
/// collapse to the same `-…-copilot-is` and can't be reversed from the slug
/// alone. Returns None for a memory-only project (no parseable session), so
/// the caller falls back to the best-effort slug decode.
fn claude_cwd_from_project_dir(project_dir: &Path) -> Option<String> {
    let entries = fs::read_dir(project_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        // The cwd is on every message record, and these files can be tens of
        // MB — read only the first few lines, never the whole file.
        let mut reader = std::io::BufReader::new(file);
        for _ in 0..8 {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Some(cwd) = extract_json_string_field(&line, "cwd").filter(|s| !s.is_empty()) {
                return Some(cwd);
            }
        }
    }
    None
}

/// Rebuild a path from `-`-split `segments` under `base`, merging adjacent
/// segments with `-` when that names a real directory (resolves the
/// `copilot-is` vs `copilot/is` ambiguity). Falls back to one segment per
/// component when no directory matches, reproducing the naive decode.
fn resolve_dashed_path(base: &Path, segments: &[&str]) -> std::path::PathBuf {
    let mut current = base.to_path_buf();
    let mut i = 0;
    while i < segments.len() {
        // Default: this single segment is its own component.
        let mut chosen = segments[i].to_string();
        let mut chosen_end = i + 1;
        // Probe increasingly long `-`-joined runs; keep the longest that
        // exists as a directory under `current`.
        let mut acc = String::new();
        for end in (i + 1)..=segments.len() {
            if acc.is_empty() {
                acc.push_str(segments[i]);
            } else {
                acc.push('-');
                acc.push_str(segments[end - 1]);
            }
            if current.join(&acc).is_dir() {
                chosen = acc.clone();
                chosen_end = end;
            }
        }
        current.push(&chosen);
        i = chosen_end;
    }
    current
}

// ---------------------------------------------------------------------------
// Project migration (Claude) — re-home a renamed project's sessions + memory
// ---------------------------------------------------------------------------

/// Counts + path info returned to the UI after a migration. `old_dir` /
/// `new_dir` are the source/destination slug dirs (absolute) and `new_project`
/// is the destination project path — the frontend uses them to re-point the
/// moved records' `project` + `path` in memory, instead of a full re-scan.
#[derive(Serialize)]
pub struct ClaudeMigrationResult {
    pub sessions: usize,
    pub memory_files: usize,
    pub old_dir: String,
    pub new_dir: String,
    pub new_project: String,
}

/// Claude Code's config root — `$CLAUDE_CONFIG_DIR` when set, else
/// `<home>/.claude`. Single source for the scanners in this file and
/// for quota.rs's credential lookup.
pub(crate) fn claude_config_root(home: &Path) -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".claude"),
    }
}

/// XDG base dir: the `env` variable when set & non-empty (an absolute
/// override per the XDG spec), else `default`. Shared by the OpenCode
/// resolvers below.
fn xdg_base(env: &str, default: PathBuf) -> PathBuf {
    match std::env::var_os(env) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => default,
    }
}

/// OpenCode's per-user **config** dir (`opencode.json` lives here) —
/// `$XDG_CONFIG_HOME/opencode` when set, else `<home>/.config/opencode`.
/// OpenCode resolves it via xdg-basedir's `xdgConfig`
/// (`.audit-sources/opencode/packages/core/src/global.ts:12`), which
/// honors the env var, so Termory must too. Single source for the
/// scanners here, the watcher, and provider switching.
pub(crate) fn opencode_config_dir(home: &Path) -> PathBuf {
    xdg_base("XDG_CONFIG_HOME", home.join(".config")).join("opencode")
}

/// OpenCode's per-user **data** dir (`opencode.db` + storage live here) —
/// `$XDG_DATA_HOME/opencode` when set, else `<home>/.local/share/opencode`
/// (xdg-basedir's `xdgData`, same source).
pub(crate) fn opencode_data_dir(home: &Path) -> PathBuf {
    xdg_base("XDG_DATA_HOME", home.join(".local").join("share")).join("opencode")
}

/// Live work status of a currently-running Claude session — `Busy`
/// (actively processing) or `Waiting` (blocked on user input / a
/// permission approval). Read from the native `~/.claude/sessions/<pid>.json`
/// snapshot Claude Code writes for `claude ps`
/// (`utils/concurrentSessions.ts:19` `SessionStatus = 'busy' | 'idle' | 'waiting'`;
/// derived in `screens/REPL.tsx:1605`). `idle`, the `dialog open`
/// waiting reason, and the terminal states get NO variant — the tray
/// shows a marker only for genuinely busy / input-blocked sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeWorkStatus {
    Busy,
    Waiting,
}

/// PID-reuse backstop for the liveness check: even if `process_alive`
/// says yes, a `<pid>.json` not rewritten within this window is ignored,
/// since the OS may have recycled the pid for an unrelated process after
/// the original session crashed. (Crash residue itself is caught by the
/// liveness probe; this only guards the reuse corner case.)
const CLAUDE_STATUS_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Map `sessionId → ClaudeWorkStatus` for every live Claude session
/// that's busy or input-blocked. Empty when the sessions dir is absent
/// (Claude never run, or the `BG_SESSIONS` feature is off so nothing is
/// written). Cheap — a handful of tiny single-line JSON files.
pub fn claude_work_statuses() -> HashMap<String, ClaudeWorkStatus> {
    let Some(home) = crate::home_dir() else {
        return HashMap::new();
    };
    claude_work_statuses_in(&claude_config_root(&home).join("sessions"))
}

/// Whether a process with `pid` is currently alive. Mirrors Claude's own
/// `isProcessRunning` (`genericProcessUtils.ts` — `process.kill(pid, 0)`)
/// so a crashed session's residual `<pid>.json` doesn't surface a stale
/// "busy"/"waiting" status. mtime alone can't tell a long-running busy
/// session (whose file isn't rewritten while the status holds) from a
/// crashed one. Unix probes via `kill(pid, 0)`, Windows via `OpenProcess`;
/// any other (exotic) target assumes alive and leans on the mtime backstop
/// + Claude's own stale-file sweep.
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // 0 → alive & signalable; EPERM → alive but owned by another user;
    // ESRCH → no such process.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid <= 0 {
        return false;
    }
    // SAFETY: plain FFI; we close any handle we successfully open.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            // ERROR_INVALID_PARAMETER ⇒ no such process; any other error
            // (e.g. access denied) ⇒ it exists. Mirrors Node's kill(pid, 0).
            return std::io::Error::last_os_error().raw_os_error()
                != Some(ERROR_INVALID_PARAMETER as i32);
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: i32) -> bool {
    true
}

/// Env-free core of [`claude_work_statuses`] — reads `<pid>.json`
/// snapshots from `sessions_dir`. Skips files whose process is dead
/// (crash residue) or whose mtime is stale (PID-reuse backstop).
fn claude_work_statuses_in(sessions_dir: &Path) -> HashMap<String, ClaudeWorkStatus> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        // Only `<pid>.json` — matches Claude's own filename guard
        // (`concurrentSessions.ts` `/^\d+\.json$/`). All-ASCII-digit stem
        // (no sign) so the parse below can't yield a negative pid, which
        // `kill` would read as a process GROUP.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = stem.parse::<i32>() else {
            continue; // pid larger than i32 — not a real pid, skip
        };
        // Crash residue: the owning process must still be alive.
        if !process_alive(pid) {
            continue;
        }
        // PID-reuse backstop: also require the file to be recent.
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            if now
                .duration_since(modified)
                .map(|age| age > CLAUDE_STATUS_MAX_AGE)
                .unwrap_or(false)
            {
                continue;
            }
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some((session_id, status)) = parse_claude_status_snapshot(&text) {
                out.insert(session_id, status);
            }
        }
    }
    out
}

/// Parse one `<pid>.json` snapshot into `(sessionId, status)`. `None`
/// when there's no sessionId, or the session is idle / browsing a dialog
/// / in a terminal state (done/failed/stopped) — those get no tray
/// marker. Prefers the richer `state` field (`claude agents --json` /
/// cc-status compat) then falls back to `status` + `waitingFor` (the
/// fields Claude 2.1.x's REPL PID file carries — verified on disk).
fn parse_claude_status_snapshot(json: &str) -> Option<(String, ClaudeWorkStatus)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let session_id = v.get("sessionId")?.as_str()?.trim().to_string();
    if session_id.is_empty() {
        return None;
    }
    let waiting_for = v.get("waitingFor").and_then(|s| s.as_str()).unwrap_or("");
    // `state` (newer / `claude agents --json`): working|blocked|done|failed|stopped.
    if let Some(state) = v.get("state").and_then(|s| s.as_str()) {
        return match state {
            "working" => Some((session_id, ClaudeWorkStatus::Busy)),
            "blocked" => Some((session_id, ClaudeWorkStatus::Waiting)),
            _ => None, // done / failed / stopped / unknown → no marker
        };
    }
    // `status` (REPL PID file): busy | waiting | idle.
    match v.get("status").and_then(|s| s.as_str()) {
        Some("busy") => Some((session_id, ClaudeWorkStatus::Busy)),
        // `dialog open` = the user opened a UI, not a real input-block
        // (`REPL.tsx` ignorableWaitingReasons / cc-status).
        Some("waiting") if waiting_for != "dialog open" => {
            Some((session_id, ClaudeWorkStatus::Waiting))
        }
        _ => None, // idle / dialog open / unknown → no marker
    }
}

/// `~/.claude/projects` (or `$CLAUDE_CONFIG_DIR/projects`).
fn claude_projects_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Some(Path::new(&dir).join("projects"));
    }
    crate::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// A frontend-supplied id / relative name is safe to `join` under a bounded
/// dir: non-empty and every component is `Normal` (no `..`, no absolute, no
/// drive/root). This is what lets delete/migrate build paths from
/// `project + id/rel` — the frontend never passes a full filesystem path, so
/// the only thing to validate is this short name. (`a/b` is allowed for nested
/// memory; `..`, `/etc`, `a/../b` are rejected.)
fn is_safe_rel(rel: &str) -> bool {
    let mut any = false;
    for c in Path::new(rel).components() {
        if !matches!(c, std::path::Component::Normal(_)) {
            return false;
        }
        any = true;
    }
    any
}

/// The `~/.gemini/tmp/<id>/` dir(s) whose `.project_root` equals `project`.
fn gemini_project_dirs(tmp: &Path, project: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(tmp) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.is_dir() && gemini_project_root(&dir).as_deref() == Some(project) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Claude's project-dir slug: every non-alphanumeric char → `-`
/// (claude-code `sessionStoragePortable.ts:311` `sanitizePath`). Paths whose
/// slug exceeds `MAX_SANITIZED_LENGTH` (200) get a hash suffix we don't
/// replicate — reject them rather than risk writing into the wrong dir.
fn sanitize_claude_path(path: &str) -> Result<String, String> {
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if slug.len() > 200 {
        return Err("path too long to migrate (slug > 200 chars)".into());
    }
    Ok(slug)
}

/// Read-only check: is `path` registered in Claude's `~/.claude.json` `projects`
/// map? Claude's `--resume` only surfaces a project's sessions once that project
/// is registered (opening Claude in the dir once does it). A migration moves the
/// session files into the new slug dir but can't register the project — and
/// Termory deliberately NEVER writes `~/.claude.json` (it's Claude's live config,
/// rewritten by any running claude, so a concurrent write would clash). So we
/// only READ it here, to warn the user when the moved sessions won't show yet.
pub fn claude_project_registered(path: &str) -> bool {
    let Some(home) = crate::home_dir() else {
        return false;
    };
    claude_project_registered_in(&home.join(".claude.json"), path)
}

fn claude_project_registered_in(claude_json: &Path, path: &str) -> bool {
    let Ok(raw) = fs::read_to_string(claude_json) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(projects) = json.get("projects").and_then(|p| p.as_object()) else {
        return false;
    };
    // The `~/.claude.json` key is the project's realpath (what Claude stored).
    // The path from the folder picker may not be canonical, so match either the
    // canonicalized form (\\?\ prefix stripped, as on Windows) or the raw input.
    let canon = fs::canonicalize(path).ok().map(|p| {
        let s = p.to_string_lossy().to_string();
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    });
    canon.as_deref().is_some_and(|c| projects.contains_key(c)) || projects.contains_key(path)
}

/// Rewrite the top-level `cwd` field of each JSONL line to `new_cwd`,
/// preserving every other field + key order (serde `preserve_order`).
/// Lines that aren't a JSON object with a `cwd` pass through untouched.
fn rewrite_jsonl_cwd(content: &str, new_cwd: &str) -> String {
    content
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                return String::new();
            }
            match serde_json::from_str::<Value>(line) {
                Ok(Value::Object(mut map)) if map.contains_key("cwd") => {
                    map.insert("cwd".to_string(), Value::String(new_cwd.to_string()));
                    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| line.to_string())
                }
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Copy `src`'s last-modified time onto `dst` (best-effort, silent on failure).
/// Migration writes fresh files (`fs::write` / `fs::copy` reset the mtime), but
/// Claude's `/resume` list and Termory's mtime fallback (when a session lacks an
/// explicit timestamp) order by it — so a naive copy would make every migrated
/// session read "just now" and lose chronological order. Mirrors the Codex
/// rollout rewrite's mtime restore. mtime is cosmetic, not durability.
fn preserve_mtime(src: &Path, dst: &Path) {
    if let Ok(mtime) = fs::metadata(src).and_then(|m| m.modified()) {
        if let Ok(f) = fs::OpenOptions::new().write(true).open(dst) {
            let _ = f.set_modified(mtime);
        }
    }
}

/// Recursively copy a Claude project subtree, rewriting the `cwd` in every
/// `*.jsonl` to `new_cwd` and copying all other files (subagent transcripts,
/// `tool-results/`, memory `.md`, …) verbatim. Skips the regenerable
/// `sessions-index.json` cache (Claude rebuilds it by scanning the dir) and
/// `.DS_Store`. Merge semantics: an existing destination file is left
/// untouched, so re-running never clobbers newer data.
fn copy_tree_rewrite_cwd(src: &Path, dst: &Path, new_cwd: &str) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        if lossy == "sessions-index.json" || lossy == ".DS_Store" {
            continue;
        }
        let path = entry.path();
        let dest = dst.join(&name);
        if path.is_dir() {
            copy_tree_rewrite_cwd(&path, &dest, new_cwd)?;
        } else if dest.exists() {
            continue;
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            fs::write(&dest, rewrite_jsonl_cwd(&content, new_cwd)).map_err(|e| e.to_string())?;
            preserve_mtime(&path, &dest);
        } else {
            fs::copy(&path, &dest).map_err(|e| e.to_string())?;
            preserve_mtime(&path, &dest);
        }
    }
    Ok(())
}

/// Count regular files in a directory tree (0 if it doesn't exist).
fn count_files_recursive(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += count_files_recursive(&path);
        } else {
            n += 1;
        }
    }
    n
}

/// Migrate a Claude project's sessions + memory from `old_path` to `new_path`
/// after the folder was renamed. Claude keys both by a slug of the project
/// path, so the renamed folder gets a fresh (empty) slug dir and the old
/// records are stranded; this copies them into the new slug dir and rewrites
/// each session's `cwd` so the CLI lists/resumes them from the new path.
///
/// COPY by default (the old dir stays — a built-in backup). `delete_old`
/// removes the source dir only after a successful copy.
pub fn migrate_claude_project(
    old_path: &str,
    new_path: &str,
    delete_old: bool,
) -> Result<ClaudeMigrationResult, String> {
    let projects = claude_projects_root().ok_or("cannot locate ~/.claude/projects")?;
    migrate_claude_project_in(&projects, old_path, new_path, delete_old)
}

/// Inner migration against an explicit `projects` root (so tests don't have to
/// mutate the process-global `CLAUDE_CONFIG_DIR`).
fn migrate_claude_project_in(
    projects: &Path,
    old_path: &str,
    new_path: &str,
    delete_old: bool,
) -> Result<ClaudeMigrationResult, String> {
    // Old dir already exists under the old path's slug (the renamed folder is
    // gone from disk, so we sanitize the recorded path directly).
    let old_dir = projects.join(sanitize_claude_path(old_path)?);
    if !old_dir.is_dir() {
        return Err(format!(
            "no Claude history found for {old_path} ({})",
            old_dir.display()
        ));
    }
    let (new_dir, new_canon) = resolve_new_claude_dir(projects, new_path)?;
    if old_dir == new_dir {
        return Err("source and destination are the same project".into());
    }
    fs::create_dir_all(&new_dir).map_err(|e| e.to_string())?;

    // Counts for the toast: top-level sessions + every memory file in the
    // source (the companion <id>/ dirs ride along but aren't counted — they're
    // part of their session).
    let sessions = fs::read_dir(&old_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .count();
    let memory_files = count_files_recursive(&old_dir.join("memory"));

    // Copy the entire slug dir — sessions, their <id>/ companion dirs
    // (subagents / tool-results), and memory/ — rewriting cwd in every .jsonl.
    copy_tree_rewrite_cwd(&old_dir, &new_dir, &new_canon)?;

    let old_dir_str = old_dir.to_string_lossy().to_string();
    // Only after a clean copy, optionally drop the old dir.
    if delete_old {
        fs::remove_dir_all(&old_dir).map_err(|e| e.to_string())?;
    }

    Ok(ClaudeMigrationResult {
        sessions,
        memory_files,
        old_dir: old_dir_str,
        new_dir: new_dir.to_string_lossy().to_string(),
        new_project: new_canon,
    })
}

/// New project dir + its canonical path, shared by every migrate variant: the
/// slug of `realpath(new_path)` (matches what Claude computes when launched
/// there). The new folder must exist.
fn resolve_new_claude_dir(projects: &Path, new_path: &str) -> Result<(PathBuf, String), String> {
    let canon =
        fs::canonicalize(new_path).map_err(|e| format!("new path not found ({new_path}): {e}"))?;
    let canon = canon.to_string_lossy();
    // Windows `fs::canonicalize` yields a `\\?\` verbatim prefix; Claude/Node
    // realpath does not, so strip it to match the slug the CLI computes
    // (no-op off Windows — the prefix never appears there).
    let new_canon = canon.strip_prefix(r"\\?\").unwrap_or(&canon).to_string();
    let new_dir = projects.join(sanitize_claude_path(&new_canon)?);
    Ok((new_dir, new_canon))
}

/// Migrate a single Claude session JSONL into the new path's slug dir,
/// rewriting its `cwd`. Memory is project-level, so single-session migration
/// doesn't touch it. Copy by default; `delete_old` removes the source file.
pub fn migrate_claude_session(
    project: &str,
    rel: &str,
    new_path: &str,
    delete_old: bool,
) -> Result<ClaudeMigrationResult, String> {
    let projects = claude_projects_root().ok_or("cannot locate ~/.claude/projects")?;
    migrate_claude_session_in(&projects, project, rel, new_path, delete_old)
}

fn migrate_claude_session_in(
    projects: &Path,
    project: &str,
    rel: &str,
    new_path: &str,
    delete_old: bool,
) -> Result<ClaudeMigrationResult, String> {
    if !is_safe_rel(rel) {
        return Err("invalid session path".into());
    }
    let old_dir = projects.join(sanitize_claude_path(project)?);
    let src = old_dir.join(rel);
    if !src.is_file() {
        return Err(format!("session not found ({rel})"));
    }
    let (new_dir, new_canon) = resolve_new_claude_dir(projects, new_path)?;
    let dest = new_dir.join(rel);
    if dest == src {
        return Err("source and destination are the same".into());
    }
    if dest.exists() {
        return Err("a session with this id already exists at the destination".into());
    }
    fs::create_dir_all(&new_dir).map_err(|e| e.to_string())?;
    let content = fs::read_to_string(&src).map_err(|e| e.to_string())?;
    fs::write(&dest, rewrite_jsonl_cwd(&content, &new_canon)).map_err(|e| e.to_string())?;
    preserve_mtime(&src, &dest);

    // Bring the session's companion dir (<id>/ — subagent transcripts +
    // tool-results) along, if present, rewriting cwd in any .jsonl inside. It's
    // the session path without the `.jsonl` extension.
    let companion = src.with_extension("");
    if companion != src && companion.is_dir() {
        copy_tree_rewrite_cwd(&companion, &dest.with_extension(""), &new_canon)?;
    }

    if delete_old {
        fs::remove_file(&src).map_err(|e| e.to_string())?;
        if companion != src && companion.is_dir() {
            fs::remove_dir_all(&companion).map_err(|e| e.to_string())?;
        }
    }
    Ok(ClaudeMigrationResult {
        sessions: 1,
        memory_files: 0,
        old_dir: old_dir.to_string_lossy().to_string(),
        new_dir: new_dir.to_string_lossy().to_string(),
        new_project: new_canon,
    })
}

/// Migrate a single Claude auto-memory `.md` (under
/// `~/.claude/projects/<slug>/memory/…`) into the new path's slug dir,
/// preserving its relative path under `memory/`. Copy by default. Rejects
/// files that aren't auto-memory (e.g. a project-folder CLAUDE.md, which
/// already moved with the renamed folder).
pub fn migrate_claude_memory(
    project: &str,
    rel: &str,
    new_path: &str,
    delete_old: bool,
) -> Result<ClaudeMigrationResult, String> {
    let projects = claude_projects_root().ok_or("cannot locate ~/.claude/projects")?;
    migrate_claude_memory_in(&projects, project, rel, new_path, delete_old)
}

fn migrate_claude_memory_in(
    projects: &Path,
    project: &str,
    rel: &str,
    new_path: &str,
    delete_old: bool,
) -> Result<ClaudeMigrationResult, String> {
    if !is_safe_rel(rel) {
        return Err("invalid memory path".into());
    }
    let old_dir = projects.join(sanitize_claude_path(project)?);
    let src = old_dir.join(rel);
    if !src.is_file() {
        return Err(format!("memory file not found ({rel})"));
    }
    let (new_dir, new_canon) = resolve_new_claude_dir(projects, new_path)?;
    let dest = new_dir.join(rel);
    if dest == src {
        return Err("source and destination are the same".into());
    }
    if dest.exists() {
        return Err("this memory file already exists at the destination".into());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::copy(&src, &dest).map_err(|e| e.to_string())?;
    preserve_mtime(&src, &dest);
    if delete_old {
        fs::remove_file(&src).map_err(|e| e.to_string())?;
    }
    Ok(ClaudeMigrationResult {
        sessions: 0,
        memory_files: 1,
        old_dir: old_dir.to_string_lossy().to_string(),
        new_dir: new_dir.to_string_lossy().to_string(),
        new_project: new_canon,
    })
}

// ---------------------------------------------------------------------------
// Project deletion (Claude) — permanently remove a project / session / memory
// ---------------------------------------------------------------------------

/// Permanently delete a whole Claude project (its slug dir: all sessions,
/// companion dirs, and memory).
pub fn delete_claude_project(project: &str) -> Result<(), String> {
    let projects = claude_projects_root().ok_or("cannot locate ~/.claude/projects")?;
    delete_claude_project_in(&projects, project)
}

fn delete_claude_project_in(projects: &Path, project: &str) -> Result<(), String> {
    let dir = projects.join(sanitize_claude_path(project)?);
    if !dir.is_dir() {
        return Err(format!("no Claude history found for {project}"));
    }
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

/// Permanently delete one Claude session — `<project-slug>/<rel>` (rel =
/// `<id>.jsonl`) + its companion `<id>/` dir. The backend builds the path from
/// `project` (→ slug dir) + `rel` (the file's path within that dir, which the
/// frontend already has); bounded by `sanitize_claude_path` + `is_safe_rel`.
pub fn delete_claude_session(project: &str, rel: &str) -> Result<(), String> {
    let projects = claude_projects_root().ok_or("cannot locate ~/.claude/projects")?;
    delete_claude_session_in(&projects, project, rel)
}

fn delete_claude_session_in(projects: &Path, project: &str, rel: &str) -> Result<(), String> {
    if !is_safe_rel(rel) {
        return Err("invalid session path".into());
    }
    let dir = projects.join(sanitize_claude_path(project)?);
    let file = dir.join(rel);
    if !file.is_file() {
        return Err(format!("session not found ({rel})"));
    }
    fs::remove_file(&file).map_err(|e| e.to_string())?;
    // Companion `<id>/` dir (subagents / tool-results) — same path without the
    // `.jsonl` extension.
    let companion = file.with_extension("");
    if companion != file && companion.is_dir() {
        fs::remove_dir_all(&companion).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Permanently delete one Claude auto-memory — `<project-slug>/<rel>`
/// (rel = `memory/<…>`).
pub fn delete_claude_memory(project: &str, rel: &str) -> Result<(), String> {
    let projects = claude_projects_root().ok_or("cannot locate ~/.claude/projects")?;
    delete_claude_memory_in(&projects, project, rel)
}

fn delete_claude_memory_in(projects: &Path, project: &str, rel: &str) -> Result<(), String> {
    if !is_safe_rel(rel) {
        return Err("invalid memory path".into());
    }
    let file = projects.join(sanitize_claude_path(project)?).join(rel);
    if !file.is_file() {
        return Err(format!("memory file not found ({rel})"));
    }
    fs::remove_file(&file).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Gemini deletion — chats / memory under ~/.gemini/tmp/<id>/. The project's
// tmp dir is found by `.project_root` match; the chat filename / memory rel
// (which the frontend already has) is joined under it — no full path, no scan.
// ---------------------------------------------------------------------------

/// `~/.gemini/tmp` — Gemini's per-project temp root.
fn gemini_tmp_root() -> Option<PathBuf> {
    crate::home_dir().map(|h| h.join(".gemini").join("tmp"))
}

/// Permanently delete one Gemini session — `<project-tmp-dir>/<rel>` (rel =
/// `chats/<file>`). The frontend already has the chat filename, so `rel` is
/// joined under the project's tmp dir — no scan / sessionId matching.
pub fn delete_gemini_session(project: &str, rel: &str) -> Result<(), String> {
    let tmp = gemini_tmp_root().ok_or("cannot locate ~/.gemini/tmp")?;
    delete_gemini_record_in(&tmp, project, rel)
}

/// Permanently delete one Gemini auto-memory — `<project-tmp-dir>/<rel>`
/// (rel = `memory/<…>`).
pub fn delete_gemini_memory(project: &str, rel: &str) -> Result<(), String> {
    let tmp = gemini_tmp_root().ok_or("cannot locate ~/.gemini/tmp")?;
    delete_gemini_record_in(&tmp, project, rel)
}

/// Remove `<project-tmp-dir>/<rel>` for every tmp dir whose `.project_root`
/// matches `project`. Shared by the Gemini session + memory deletes (both are
/// just "join a project-dir-relative path and remove").
fn delete_gemini_record_in(tmp: &Path, project: &str, rel: &str) -> Result<(), String> {
    if !is_safe_rel(rel) {
        return Err("invalid record path".into());
    }
    let mut removed = false;
    for dir in gemini_project_dirs(tmp, project) {
        let file = dir.join(rel);
        if file.is_file() {
            fs::remove_file(&file).map_err(|e| e.to_string())?;
            removed = true;
        }
    }
    if !removed {
        return Err(format!("Gemini record not found ({rel})"));
    }
    Ok(())
}

/// Permanently delete a Gemini project — every `~/.gemini/tmp/<id>/` dir whose
/// `.project_root` matches `project`.
pub fn delete_gemini_project(project: &str) -> Result<(), String> {
    let tmp = gemini_tmp_root().ok_or("cannot locate ~/.gemini/tmp")?;
    delete_gemini_project_in(&tmp, project)
}

fn delete_gemini_project_in(tmp: &Path, project: &str) -> Result<(), String> {
    let dirs = gemini_project_dirs(tmp, project);
    if dirs.is_empty() {
        return Err(format!("no Gemini project dir found for {project}"));
    }
    for dir in dirs {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Codex deletion — threads live in ~/.codex/state_5.sqlite keyed by id/cwd,
// with the transcript in a rollout JSONL file. Deleting only the row is NOT
// durable: Codex's backfill re-creates it from the rollout file (the file is
// authoritative — see codex_follow.rs). So we DELETE the row AND remove the
// rollout file. Memories are standalone .md files under ~/.codex/memories/.
// All DB writes use a busy timeout and surface a clean "locked" error.
// ---------------------------------------------------------------------------

fn codex_state_db_path() -> Option<PathBuf> {
    crate::home_dir().map(|h| crate::providers::codex_root(&h).join("state_5.sqlite"))
}

fn open_codex_rw(db: &Path) -> Result<Connection, String> {
    if !db.exists() {
        return Err("Codex state database not found".into());
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn map_db_locked(err: rusqlite::Error, who: &str) -> String {
    let msg = err.to_string();
    if msg.contains("locked") || msg.contains("busy") {
        return format!("{who} database is locked — quit the running {who} and try again.");
    }
    msg
}

/// Delete every Codex thread where `column = value` (column is an internal
/// literal — "id" or "cwd"), removing both the row and its rollout file.
fn delete_codex_threads_in(db: &Path, column: &str, value: &str) -> Result<(), String> {
    let conn = open_codex_rw(db)?;
    let select = format!("select rollout_path from threads where {column} = ?1");
    let paths: Vec<String> = {
        let mut stmt = conn
            .prepare(&select)
            .map_err(|e| map_db_locked(e, "Codex"))?;
        let rows = stmt
            .query_map([value], |r| r.get::<_, String>(0))
            .map_err(|e| map_db_locked(e, "Codex"))?;
        rows.filter_map(Result::ok).collect()
    };
    if paths.is_empty() {
        return Err("no Codex session found".into());
    }
    // Remove the rollout transcripts FIRST, then the rows. The file is the
    // authoritative source: once it's gone, a crash before the row DELETE can't
    // resurrect the session (backfill has nothing to rebuild from — it just
    // leaves a harmless, re-deletable orphan row). The reverse order would let
    // a crash between the committed DELETE and the file removal resurrect the
    // row on Codex's next backfill. Missing files are tolerated.
    for p in &paths {
        let _ = fs::remove_file(p);
    }
    let delete = format!("delete from threads where {column} = ?1");
    conn.execute(&delete, [value])
        .map_err(|e| map_db_locked(e, "Codex"))?;
    Ok(())
}

/// Permanently delete one Codex session by thread id (+ its rollout file).
pub fn delete_codex_session(id: &str) -> Result<(), String> {
    let db = codex_state_db_path().ok_or("cannot locate ~/.codex/state_5.sqlite")?;
    delete_codex_threads_in(&db, "id", id)
}

/// Permanently delete every Codex session under a project cwd (+ their rollout
/// files). Codex has no project folder — a "project" is the `cwd`.
pub fn delete_codex_project(project: &str) -> Result<(), String> {
    let db = codex_state_db_path().ok_or("cannot locate ~/.codex/state_5.sqlite")?;
    delete_codex_threads_in(&db, "cwd", project)
}

/// Permanently delete one Codex auto-memory file — `rel` joined under
/// ~/.codex/memories/ (bounded by `is_safe_rel`). These are standalone .md
/// files; Termory never reads memories_1.sqlite, so there's nothing to sync.
pub fn delete_codex_memory(rel: &str) -> Result<(), String> {
    let root = crate::home_dir()
        .map(|h| crate::providers::codex_root(&h).join("memories"))
        .ok_or("cannot locate ~/.codex/memories")?;
    delete_codex_memory_in(&root, rel)
}

fn delete_codex_memory_in(root: &Path, rel: &str) -> Result<(), String> {
    if !is_safe_rel(rel) {
        return Err("invalid memory path".into());
    }
    let file = root.join(rel);
    if !file.is_file() {
        return Err(format!("memory file not found ({rel})"));
    }
    fs::remove_file(&file).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Codex migration — move a session / project's sessions to a new cwd.
//
// Codex has no project FOLDER: a "project" is just the `threads.cwd` value, and
// the rollout JSONL files live under ~/.codex/sessions/ regardless of cwd. So
// migrating is a pure METADATA rewrite (no file relocation, unlike Claude).
//
// The `threads` table is only a CACHE: Codex rebuilds each row's `cwd` from the
// rollout JSONL's first-line `session_meta.payload.cwd` on startup backfill /
// resume reconcile (mirrors codex_follow.rs's model_provider story). So we MUST
// rewrite the file's payload.cwd (authoritative, durable) AND update the table
// row (immediate visibility) — editing the table alone is silently reverted.
//
// Lower value than Claude migrate: `codex resume <id>` already works across a
// rename (the DB is global, keyed by id), so this is a regrouping convenience,
// not a "history lost" fix. Mirrors delete_codex_* in shape (id / cwd column,
// RW + busy_timeout + map_db_locked, env-free `*_in` for tests).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CodexMigrationResult {
    /// Number of sessions whose cwd was rewritten (file + table).
    pub sessions: usize,
    /// The canonicalized destination cwd that was written.
    pub new_project: String,
}

/// Canonicalize a destination path to the cwd string Codex stores. Falls back to
/// the trimmed input when the dir doesn't exist yet (Codex resume keys on id, so
/// a not-yet-created dir still works); strips the Windows `\\?\` verbatim prefix.
fn codex_canonical_cwd(new_path: &str) -> Result<String, String> {
    let trimmed = new_path.trim();
    if trimmed.is_empty() {
        return Err("destination path is empty".into());
    }
    let canon = match fs::canonicalize(trimmed) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return Ok(trimmed.to_string()),
    };
    Ok(canon.strip_prefix(r"\\?\").unwrap_or(&canon).to_string())
}

/// Rewrite a rollout JSONL's first-line `payload.cwd` to `new_cwd`, streaming
/// the (100+ MB) remainder unchanged. Returns Ok(false) when there's nothing to
/// change (no payload object, or cwd already equals new_cwd). Atomic via
/// temp + rename; the original mtime is restored afterwards (the Codex resume
/// picker orders by file mtime, NOT threads.updated_at). Durability does NOT
/// rely on the mtime — the table is updated directly and Codex's resume
/// reconcile re-reads the rewritten file. Parallels codex_follow.rs's
/// `rewrite_rollout_provider` (same shape, `cwd` instead of `model_provider`).
fn rewrite_rollout_cwd(path: &Path, new_cwd: &str) -> Result<bool, String> {
    use std::io::{BufRead, BufReader, BufWriter, Write};

    if !path.exists() {
        return Err(format!("rollout file not found: {}", path.display()));
    }
    let original_mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    if reader
        .read_line(&mut first_line)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Ok(false); // empty file
    }
    let trimmed = first_line.trim_end_matches(['\n', '\r']);
    let mut value: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    let payload = match value.get_mut("payload").and_then(|p| p.as_object_mut()) {
        Some(p) => p,
        None => return Ok(false),
    };
    if let Some(Value::String(s)) = payload.get("cwd") {
        if s == new_cwd {
            return Ok(false);
        }
    }
    payload.insert("cwd".into(), Value::String(new_cwd.to_string()));
    // serde_json has preserve_order enabled (Cargo.toml), so key order survives.
    let new_first = serde_json::to_string(&value).map_err(|e| e.to_string())?;

    let mut tmp_name = path.file_name().ok_or("invalid rollout path")?.to_owned();
    tmp_name.push(".termory-tmp");
    let tmp_path = path.with_file_name(tmp_name);
    {
        let out = fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::new(out);
        writer
            .write_all(new_first.as_bytes())
            .map_err(|e| e.to_string())?;
        writer.write_all(b"\n").map_err(|e| e.to_string())?;
        std::io::copy(&mut reader, &mut writer).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        writer.get_ref().sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
    if let Some(mtime) = original_mtime {
        if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_modified(mtime);
        }
    }
    Ok(true)
}

/// Move every Codex thread matched by `column = value` (column is an internal
/// literal — "id" or "cwd") to `new_cwd`: rewrite each rollout file's
/// payload.cwd FIRST (authoritative — a crash before the table update
/// self-heals on Codex's next backfill, converging to the new cwd), then
/// `UPDATE threads.cwd` for the rewritten rows. Returns how many sessions moved.
fn migrate_codex_threads_in(
    db: &Path,
    column: &str,
    value: &str,
    new_cwd: &str,
) -> Result<usize, String> {
    let conn = open_codex_rw(db)?;
    let select = format!("select id, rollout_path from threads where {column} = ?1");
    let candidates: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(&select)
            .map_err(|e| map_db_locked(e, "Codex"))?;
        let rows = stmt
            .query_map([value], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| map_db_locked(e, "Codex"))?;
        rows.filter_map(Result::ok).collect()
    };
    if candidates.is_empty() {
        return Err("no Codex session found".into());
    }
    // Rewrite files first; collect the ids that landed so only those get their
    // table row updated (a per-file failure self-heals via Codex's backfill).
    let mut moved_ids: Vec<String> = Vec::new();
    for (id, rollout_path) in &candidates {
        match rewrite_rollout_cwd(Path::new(rollout_path), new_cwd) {
            Ok(_) => moved_ids.push(id.clone()),
            Err(e) => log::warn!("codex migrate: skip {id}: {e}"),
        }
    }
    if moved_ids.is_empty() {
        return Err("no Codex session could be migrated".into());
    }
    let placeholders = std::iter::repeat("?")
        .take(moved_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let update = format!("update threads set cwd = ?1 where id in ({placeholders})");
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(moved_ids.len() + 1);
    params.push(&new_cwd);
    for id in &moved_ids {
        params.push(id);
    }
    conn.execute(&update, params.as_slice())
        .map_err(|e| map_db_locked(e, "Codex"))?;
    Ok(moved_ids.len())
}

/// Migrate one Codex session (by thread id) to a new cwd.
pub fn migrate_codex_session(id: &str, new_path: &str) -> Result<CodexMigrationResult, String> {
    let db = codex_state_db_path().ok_or("cannot locate ~/.codex/state_5.sqlite")?;
    let new_cwd = codex_canonical_cwd(new_path)?;
    let sessions = migrate_codex_threads_in(&db, "id", id, &new_cwd)?;
    Ok(CodexMigrationResult {
        sessions,
        new_project: new_cwd,
    })
}

/// Migrate every Codex session under a project cwd to a new cwd. Includes
/// archived threads (whole-project move, consistent with delete_codex_project).
pub fn migrate_codex_project(
    project: &str,
    new_path: &str,
) -> Result<CodexMigrationResult, String> {
    let db = codex_state_db_path().ok_or("cannot locate ~/.codex/state_5.sqlite")?;
    let new_cwd = codex_canonical_cwd(new_path)?;
    if new_cwd == project {
        return Err("source and destination are the same".into());
    }
    let sessions = migrate_codex_threads_in(&db, "cwd", project, &new_cwd)?;
    Ok(CodexMigrationResult {
        sessions,
        new_project: new_cwd,
    })
}

// ---------------------------------------------------------------------------
// OpenCode deletion — sessions/projects live in
// ~/.local/share/opencode/opencode.db. session.project_id and the message/part
// tables have ON DELETE CASCADE, so deleting a session (or a project) cascades.
// The session table is a durable store (not a rebuilt cache), so a direct
// DELETE is permanent. OpenCode has NO deletable auto-memory — its memory is
// the user's own AGENTS.md/CLAUDE.md project files, which are never touched.
// ---------------------------------------------------------------------------

fn opencode_db_path() -> Option<PathBuf> {
    crate::home_dir().map(|h| opencode_data_dir(&h).join("opencode.db"))
}

fn open_opencode_rw(db: &Path) -> Result<Connection, String> {
    if !db.exists() {
        return Err("OpenCode database not found".into());
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))
        .map_err(|e| e.to_string())?;
    // Required for the ON DELETE CASCADE to message/part to actually fire.
    conn.execute_batch("PRAGMA foreign_keys = ON")
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Clear the event log for the given aggregate ids — mirrors the official
/// `sync.remove()` (sync/index.ts:173-178). The `event` / `event_sequence`
/// tables only exist on newer OpenCode DBs (written only under
/// `experimentalWorkspaces`); older layouts skip this. `event` FK-cascades from
/// `event_sequence`, but we delete both explicitly to match the official op and
/// to be robust if foreign_keys is off. Orphan events are never replayed on
/// startup (verified), so this is tidiness/sync-correctness, not a rebuild fix.
fn clean_opencode_events(conn: &Connection, aggregate_ids: &[String]) -> Result<(), String> {
    if !table_exists(conn, "event_sequence").unwrap_or(false) {
        return Ok(());
    }
    for id in aggregate_ids {
        conn.execute("delete from event where aggregate_id = ?1", [id])
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
        conn.execute("delete from event_sequence where aggregate_id = ?1", [id])
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
    }
    Ok(())
}

/// Collect a session id and all its descendants via the `parent_id` tree
/// (sub-agent sessions). `parent_id` has NO FK cascade, so the official
/// `session.remove()` walks children recursively — we do the same. A `seen`
/// set guards against a malformed cycle.
fn opencode_session_tree(conn: &Connection, root: &str) -> Result<Vec<String>, String> {
    let mut ids = vec![root.to_string()];
    let mut i = 0;
    while i < ids.len() {
        let parent = ids[i].clone();
        let mut stmt = conn
            .prepare("select id from session where parent_id = ?1")
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
        let kids: Vec<String> = stmt
            .query_map([&parent], |r| r.get::<_, String>(0))
            .map_err(|e| map_db_locked(e, "OpenCode"))?
            .filter_map(Result::ok)
            .collect();
        for k in kids {
            if !ids.contains(&k) {
                ids.push(k);
            }
        }
        i += 1;
    }
    Ok(ids)
}

/// Permanently delete one OpenCode session by id — plus its sub-agent children
/// (recursively, matching the official `session.remove()`), each row cascading
/// to its message/part, and the event log of every removed aggregate cleared.
pub fn delete_opencode_session(id: &str) -> Result<(), String> {
    let db = opencode_db_path().ok_or("cannot locate opencode.db")?;
    delete_opencode_session_in(&db, id)
}

fn delete_opencode_session_in(db: &Path, id: &str) -> Result<(), String> {
    let conn = open_opencode_rw(db)?;
    let ids = opencode_session_tree(&conn, id)?;
    let mut total = 0usize;
    for sid in &ids {
        total += conn
            .execute("delete from session where id = ?1", [sid])
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
    }
    if total == 0 {
        return Err("OpenCode session not found".into());
    }
    // Each session's aggregate id IS its session id (sync.remove(sessionID)).
    clean_opencode_events(&conn, &ids)?;
    Ok(())
}

/// Permanently delete an OpenCode project (keyed by `worktree`) — cascades to
/// its sessions, then to message/part, and clears the event log of the project
/// and every session it removes.
pub fn delete_opencode_project(project: &str) -> Result<(), String> {
    let db = opencode_db_path().ok_or("cannot locate opencode.db")?;
    delete_opencode_project_in(&db, project)
}

fn delete_opencode_project_in(db: &Path, project: &str) -> Result<(), String> {
    let conn = open_opencode_rw(db)?;
    // Collect every aggregate id (the project ids + their session ids) BEFORE
    // the cascade removes the rows, so we can clear their events afterward.
    let project_ids: Vec<String> = {
        let mut stmt = conn
            .prepare("select id from project where worktree = ?1")
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
        let rows = stmt
            .query_map([project], |r| r.get::<_, String>(0))
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
        rows.filter_map(Result::ok).collect()
    };
    if project_ids.is_empty() {
        return Err("OpenCode project not found".into());
    }
    let mut aggregates = project_ids.clone();
    for pid in &project_ids {
        let mut stmt = conn
            .prepare("select id from session where project_id = ?1")
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
        let rows = stmt
            .query_map([pid], |r| r.get::<_, String>(0))
            .map_err(|e| map_db_locked(e, "OpenCode"))?;
        aggregates.extend(rows.filter_map(Result::ok));
    }
    conn.execute("delete from project where worktree = ?1", [project])
        .map_err(|e| map_db_locked(e, "OpenCode"))?;
    clean_opencode_events(&conn, &aggregates)?;
    Ok(())
}

fn memory_session_from_file(path: &Path, project: &str) -> Option<AppSession> {
    doc_session_from_file(path, project, "Memory")
}

fn doc_session_from_file(path: &Path, project: &str, source: &str) -> Option<AppSession> {
    let raw = fs::read_to_string(path).ok()?;
    let (front, body) = split_memory_frontmatter(&raw);
    let body_is_empty = body.trim().is_empty();
    let name = front
        .as_ref()
        .and_then(|f| f.get("name").cloned())
        .filter(|s| !s.is_empty());
    let description = front
        .as_ref()
        .and_then(|f| f.get("description").cloned())
        .unwrap_or_default();
    let mem_type = front
        .as_ref()
        .and_then(|f| f.get("type").cloned())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("memory")
        .to_string();
    let title = name.unwrap_or_else(|| file_name.clone());
    // The id keys BOTH the backend load path-index `(source, id)` AND the
    // frontend `sessionKey` — it MUST be unique per file. The filename /
    // frontmatter `name` collide across projects (every project's `CLAUDE.md`
    // shares the name "CLAUDE.md"), which made every same-named memory load the
    // last-scanned file's content. Use the full path (unique, not displayed for
    // memory/skill rows).
    let id = path.to_string_lossy().to_string();
    let updated_at = file_time(path, true);
    let started_at = file_time(path, false);
    let preview = if !description.is_empty() {
        description.clone()
    } else if !mem_type.is_empty() {
        mem_type.clone()
    } else {
        memory_tool_for_file(&file_name).to_string()
    };
    // Snippet: prefer the `description:` frontmatter (already a
    // human-curated summary) over the raw body's first line.
    let snippet_source = if !description.is_empty() {
        description
    } else {
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_string()
    };
    let snippet = snippet_line(&snippet_source);
    Some(AppSession {
        id,
        source: source.to_string(),
        title,
        project: project.to_string(),
        path: path.to_string_lossy().to_string(),
        started_at,
        updated_at,
        message_count: if body_is_empty { 0 } else { 1 },
        archived: false,
        preview,
        snippet,
        message_previews: Vec::new(),
        tokens: None,
        model: None,
        daily_tokens: None,
    })
}

fn memory_tool_for_file(file_name: &str) -> &'static str {
    let lower = file_name.to_ascii_lowercase();
    if lower.starts_with("claude") {
        "claude"
    } else if lower.starts_with("agents") {
        "agents"
    } else if lower.starts_with("gemini") {
        "gemini"
    } else if lower.ends_with(".rules") {
        "rules"
    } else {
        "memory"
    }
}

fn split_memory_frontmatter(
    raw: &str,
) -> (Option<std::collections::HashMap<String, String>>, String) {
    let trimmed = raw.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return (None, raw.to_string());
    }
    let after_open = match trimmed.strip_prefix("---") {
        Some(rest) => rest.trim_start_matches('\n').trim_start_matches("\r\n"),
        None => return (None, raw.to_string()),
    };
    let Some(close_pos) = after_open.find("\n---") else {
        return (None, raw.to_string());
    };
    let yaml_block = &after_open[..close_pos];
    let body_start = close_pos + 4;
    let body = after_open[body_start..]
        .trim_start_matches('\n')
        .trim_start_matches("\r\n")
        .to_string();
    let mut map = std::collections::HashMap::new();
    for line in yaml_block.lines() {
        let stripped = line.trim_end_matches('\r');
        let key_value = stripped.trim_start();
        if key_value.is_empty() || key_value.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = key_value.split_once(':') {
            let key = key.trim().to_string();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !value.is_empty() {
                map.insert(key, value);
            }
        }
    }
    (Some(map), body)
}

fn parse_doc_file(path: &Path, source: &str) -> Result<SessionDetail, Box<dyn Error>> {
    let raw = fs::read_to_string(path)?;
    let (front, body) = split_memory_frontmatter(&raw);
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("doc")
        .to_string();
    let project = derive_memory_project_label(path);
    let name = front
        .as_ref()
        .and_then(|f| f.get("name").cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| file_name.clone());
    let mem_type = front
        .as_ref()
        .and_then(|f| f.get("type").cloned())
        .unwrap_or_default();
    let description = front
        .as_ref()
        .and_then(|f| f.get("description").cloned())
        .unwrap_or_default();
    let updated_at = file_time(path, true);
    let started_at = file_time(path, false);

    let body_text = body.trim();
    let display_body = if body_text.is_empty() {
        if description.is_empty() {
            "(empty)".to_string()
        } else {
            description.clone()
        }
    } else {
        body_text.to_string()
    };

    let snippet_source = if !description.is_empty() {
        description.clone()
    } else {
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_string()
    };
    let session = AppSession {
        id: name.clone(),
        source: source.to_string(),
        title: name,
        project,
        path: path.to_string_lossy().to_string(),
        started_at: started_at.clone(),
        updated_at: updated_at.clone(),
        message_count: 1,
        archived: false,
        preview: description,
        snippet: snippet_line(&snippet_source),
        message_previews: Vec::new(),
        tokens: None,
        model: None,
        daily_tokens: None,
    };
    let role = if !mem_type.is_empty() {
        mem_type
    } else {
        memory_tool_for_file(&file_name).to_string()
    };
    let kind = if source == "Skill" { "skill" } else { "memory" };
    let messages = vec![SessionMessage {
        role,
        text: display_body,
        timestamp: updated_at,
        kind: kind.to_string(),
        tool_use_id: None,
        exit_code: None,
    }];
    Ok(SessionDetail { session, messages })
}

fn derive_memory_project_label(path: &Path) -> String {
    let Some(home) = crate::home_dir() else {
        return String::new();
    };

    let claude_root = claude_config_root(&home);

    // Claude global skills: <claude_root>/skills/<name>/...
    if path.starts_with(claude_root.join("skills")) {
        return format!("{}/skills", claude_root.display());
    }

    // Claude global rules: <claude_root>/rules/**
    if path.starts_with(claude_root.join("rules")) {
        return format!("{}/rules", claude_root.display());
    }

    // Claude structured memory: <claude_root>/projects/<slug>/memory/.../<file>.md
    let claude_projects_root = claude_root.join("projects");
    if let Ok(rel) = path.strip_prefix(&claude_projects_root) {
        if let Some(slug) = rel.iter().next().and_then(|c| c.to_str()) {
            let project_dir = claude_projects_root.join(slug);
            return claude_cwd_from_project_dir(&project_dir)
                .unwrap_or_else(|| decode_claude_project_slug(slug));
        }
    }

    // Codex global skills: $CODEX_HOME/skills/**
    let codex_home = crate::providers::codex_root(&home);
    if path.starts_with(codex_home.join("skills")) {
        return format!("{}/skills", codex_home.display());
    }

    // Codex global memory: $CODEX_HOME/memories/**
    if path.starts_with(codex_home.join("memories")) {
        return format!("{}/memories", codex_home.display());
    }

    // Gemini global skills: ~/.gemini/skills/**
    if path.starts_with(home.join(".gemini").join("skills")) {
        return "~/.gemini/skills".to_string();
    }

    // Gemini per-project memory/skills: ~/.gemini/tmp/<id>/{memory,skills}/**
    let gemini_tmp = home.join(".gemini").join("tmp");
    if let Ok(rel) = path.strip_prefix(&gemini_tmp) {
        if let Some(project_id) = rel.iter().next() {
            let project_dir = gemini_tmp.join(project_id);
            return gemini_project_label(&project_dir);
        }
    }

    // OpenCode global skills: $XDG_CONFIG_HOME/opencode/skills/**
    let oc_config = opencode_config_dir(&home);
    if path.starts_with(oc_config.join("skills")) {
        return format!("{}/skills", oc_config.display());
    }

    // Tool-neutral global skills: ~/.agents/skills/**
    if path.starts_with(home.join(".agents").join("skills")) {
        return "~/.agents/skills".to_string();
    }

    // Global instruction files
    if path == claude_root.join("CLAUDE.md") || path == claude_root.join("CLAUDE.local.md") {
        return format!("{}", claude_root.display());
    }
    let codex_dir = crate::providers::codex_root(&home);
    if path == codex_dir.join("AGENTS.md") || path == codex_dir.join("instructions.md") {
        return format!("{}", codex_dir.display());
    }
    if path == home.join(".gemini").join("GEMINI.md") {
        return "~/.gemini".to_string();
    }
    let opencode_config = opencode_config_dir(&home);
    if path == opencode_config.join("AGENTS.md") || path == opencode_config.join("AGENTS.local.md")
    {
        return format!("{}", opencode_config.display());
    }

    // Project-level skill/rule files: <cwd>/.{claude,codex,gemini,opencode,agents}/{skills,rules}/<name>/...
    // Walk up the path; if we cross a "skills" or "rules" dir whose parent is a
    // known tool dotdir, the grandparent of the tool dotdir is the project cwd.
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        let dir_name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if dir_name == "skills" || dir_name == "rules" {
            if let Some(tool_dir) = dir.parent() {
                let is_tool_dot = tool_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| {
                        matches!(
                            n,
                            ".claude" | ".codex" | ".gemini" | ".opencode" | ".agents"
                        )
                    })
                    .unwrap_or(false);
                if is_tool_dot {
                    if let Some(cwd) = tool_dir.parent() {
                        return cwd.to_string_lossy().to_string();
                    }
                }
            }
            break;
        }
        cursor = dir.parent();
    }

    // Project-level instruction files. Strip a trailing ".claude" wrapper if any.
    let parent = path.parent().unwrap_or(path);
    if parent
        .file_name()
        .and_then(|s| s.to_str())
        .map(|n| n == ".claude")
        .unwrap_or(false)
    {
        if let Some(grand) = parent.parent() {
            return grand.to_string_lossy().to_string();
        }
    }
    parent.to_string_lossy().to_string()
}

#[derive(Default)]
struct ClaudeConversation {
    session_id: String,
    project: String,
    timestamps: Vec<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    agent_name: Option<String>,
    summary: Option<String>,
    first_prompt: Option<String>,
    command_prompt_fallback: Option<String>,
    team_name: Option<String>,
    messages: Vec<SessionMessage>,
    visible_message_count: usize,
    /// Cumulative token usage extracted from `message.usage` blocks
    /// (Anthropic API response shape, written verbatim into the
    /// JSONL by Claude Code). `(input, output, cache_read, cache_creation)`.
    token_input: u64,
    token_output: u64,
    token_cache_read: u64,
    token_cache_creation: u64,
    /// True once we've seen at least one usage block, so we can tell
    /// "no usage data at all" (legacy session) apart from "all zeros".
    has_token_data: bool,
    /// Last-seen `message.model` — for Claude this is generally
    /// stable within a session so the last one wins.
    model: Option<String>,
}

struct ClaudeLiteSessionFile {
    head: String,
    tail: String,
    mtime: SystemTime,
}

/// The metadata `parse_claude_lite_session` actually derives from a
/// session file's head/tail — deliberately NOT an `AppSession`, so the
/// fields the lite parse cannot produce (message_count, tokens, …)
/// must be supplied where the `AppSession` is assembled: the LIST scan
/// takes them from the mtime-cached `claude_scan_extract`; the DETAIL
/// path computes them from the full parse.
struct ClaudeLiteSession {
    id: String,
    title: String,
    project: String,
    started_at: Option<String>,
    updated_at: Option<String>,
    snippet: String,
}

fn parse_claude_lite_session(
    path: &Path,
    project_path: Option<&str>,
) -> Result<ClaudeLiteSession, Box<dyn Error>> {
    let session_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or("missing Claude session id")?
        .to_string();
    let lite = read_claude_lite_session(path).ok_or("unreadable Claude session")?;
    let first_line = lite.head.lines().next().unwrap_or_default();
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return Err("Claude sidechain session is hidden from resume".into());
    }

    let custom_or_ai_title = extract_last_json_string_field(&lite.tail, "customTitle")
        .or_else(|| extract_last_json_string_field(&lite.head, "customTitle"))
        .or_else(|| extract_last_json_string_field(&lite.tail, "aiTitle"))
        .or_else(|| extract_last_json_string_field(&lite.head, "aiTitle"));
    let first_prompt = extract_claude_first_prompt_from_head(&lite.head);
    let last_prompt = extract_last_json_string_field(&lite.tail, "lastPrompt");
    let summary = custom_or_ai_title
        .or_else(|| last_prompt.clone())
        .or_else(|| extract_last_json_string_field(&lite.tail, "summary"))
        .or(first_prompt.clone())
        .filter(|summary| !summary.trim().is_empty())
        .ok_or("Claude metadata-only session is hidden from resume")?;
    let project = extract_json_string_field(&lite.head, "cwd")
        .or_else(|| project_path.map(ToString::to_string))
        .unwrap_or_default();
    // Snippet for the list card: the latest user prompt is most
    // informative ("what was this conversation just about?"); fall
    // back to the first prompt for short sessions that have no
    // separate lastPrompt yet.
    let snippet = snippet_line(
        last_prompt
            .as_deref()
            .or(first_prompt.as_deref())
            .unwrap_or(""),
    );

    Ok(ClaudeLiteSession {
        id: session_id,
        title: official_title_from_text(&strip_display_tags(&summary)).unwrap_or(summary),
        project,
        started_at: extract_json_string_field(&lite.head, "timestamp")
            .and_then(normalize_time)
            .or_else(|| file_time(path, false)),
        updated_at: Some(system_time_to_iso(lite.mtime)),
        snippet,
    })
}

fn read_claude_lite_session(path: &Path) -> Option<ClaudeLiteSessionFile> {
    let mut file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; CLAUDE_LITE_READ_BUF_SIZE];
    let head_len = file.read(&mut buffer).ok()?;
    if head_len == 0 {
        return None;
    }
    let head = String::from_utf8_lossy(&buffer[..head_len]).to_string();

    let tail = if metadata.len() as usize <= CLAUDE_LITE_READ_BUF_SIZE {
        head.clone()
    } else {
        let tail_offset = metadata
            .len()
            .saturating_sub(CLAUDE_LITE_READ_BUF_SIZE as u64);
        file.seek(SeekFrom::Start(tail_offset)).ok()?;
        let tail_len = file.read(&mut buffer).ok()?;
        String::from_utf8_lossy(&buffer[..tail_len]).to_string()
    };

    Some(ClaudeLiteSessionFile {
        head,
        tail,
        mtime: metadata.modified().ok()?,
    })
}

fn extract_claude_first_prompt_from_head(head: &str) -> Option<String> {
    let mut command_fallback = None;
    for line in head.lines() {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"")
            || line.contains("\"isMeta\":true")
            || line.contains("\"isMeta\": true")
            || line.contains("\"isCompactSummary\":true")
            || line.contains("\"isCompactSummary\": true")
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match claude_first_prompt_from_value(&value) {
            ClaudePromptCandidate::Meaningful(prompt) => return Some(prompt),
            ClaudePromptCandidate::Command(command) => {
                if command_fallback.is_none() {
                    command_fallback = Some(command);
                }
            }
            ClaudePromptCandidate::None => {}
        }
    }
    command_fallback
}

fn parse_claude_session(path: &Path) -> Result<SessionDetail, Box<dyn Error>> {
    if !is_claude_session_file(path) {
        return Err("not a Claude resume session file".into());
    }
    let official_session = parse_claude_lite_session(path, None)?;

    let content = fs::read_to_string(path)?;
    let mut conversation = ClaudeConversation {
        session_id: path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string(),
        project: String::new(),
        ..Default::default()
    };

    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        collect_claude_metadata(&mut conversation, &value);
        for message in claude_message_from_value(&value) {
            if message.kind == kind::TEXT {
                conversation.visible_message_count += 1;
            }
            conversation.messages.push(message);
        }
        if conversation.first_prompt.is_none() {
            match claude_first_prompt_from_value(&value) {
                ClaudePromptCandidate::Meaningful(prompt) => {
                    conversation.first_prompt = Some(prompt)
                }
                ClaudePromptCandidate::Command(command) => {
                    if conversation.command_prompt_fallback.is_none() {
                        conversation.command_prompt_fallback = Some(command);
                    }
                }
                ClaudePromptCandidate::None => {}
            }
        }
    }

    // team_name is a real LIST filter from videcoding/cli sessionStorage.ts
    // (`if (enriched.teamName) return null`) — team-shared sessions are
    // hidden from the resume picker.
    //
    // `is_sidechain` is intentionally NOT checked here: official only filters
    // at LIST time using the FIRST line's isSidechain (handled in
    // parse_claude_lite_session). Mid-session sidechain entries are filtered
    // per-message inside `claude_message_from_value` so the main thread of a
    // session that spawns sub-agents stays visible.
    if conversation.team_name.is_some() {
        return Err("Claude related session is hidden from resume".into());
    }
    if conversation.first_prompt.is_none() {
        conversation.first_prompt = conversation.command_prompt_fallback.take();
    }
    let merged_messages = merge_tool_outputs(conversation.messages);
    let mut detail = detail_from_messages(
        "Claude",
        path,
        conversation.session_id,
        conversation.project,
        merged_messages,
        conversation.timestamps,
        Some(official_session.title.clone()),
    );
    detail.session.title = official_session.title;
    if detail.session.project.is_empty() {
        detail.session.project = official_session.project;
    }
    detail.session.message_count = conversation.visible_message_count;
    if detail.session.started_at.is_none() {
        detail.session.started_at = official_session
            .started_at
            .or_else(|| file_time(path, false));
    }
    if detail.session.updated_at.is_none() {
        detail.session.updated_at = official_session
            .updated_at
            .or_else(|| file_time(path, true));
    }
    if conversation.has_token_data {
        let cached = conversation.token_cache_read + conversation.token_cache_creation;
        let total = conversation.token_input + conversation.token_output + cached;
        detail.session.tokens = Some(TokenStats {
            input: conversation.token_input,
            output: conversation.token_output,
            cached,
            // Claude doesn't break out reasoning tokens — they're
            // folded into `output_tokens`. Leave the field at 0.
            reasoning: 0,
            total,
        });
    }
    detail.session.model = conversation.model.clone();
    Ok(detail)
}

fn collect_claude_metadata(conversation: &mut ClaudeConversation, value: &Value) {
    // Note: isSidechain is filtered per-message in claude_message_from_value
    // and at LIST time in parse_claude_lite_session. We intentionally do not
    // aggregate it onto the conversation struct, because hiding a whole
    // session because of mid-session sub-agent branches is over-restrictive
    // vs official behavior (see parse_claude_session for details).
    if let Some(team_name) = value.get("teamName").and_then(value_to_string) {
        conversation.team_name = Some(team_name);
    }
    if let Some(session_id) = value.get("sessionId").and_then(value_to_string) {
        conversation.session_id = session_id;
    }
    if let Some(cwd) = value.get("cwd").and_then(value_to_string) {
        conversation.project = cwd;
    }
    if let Some(timestamp) = value.get("timestamp").and_then(value_to_string) {
        conversation.timestamps.push(timestamp);
    }
    if let Some(custom_title) = value.get("customTitle").and_then(value_to_string) {
        conversation.custom_title = Some(custom_title);
    }
    if let Some(ai_title) = value.get("aiTitle").and_then(value_to_string) {
        conversation.ai_title = Some(ai_title);
    }
    if let Some(last_prompt) = value.get("lastPrompt").and_then(value_to_string) {
        conversation.last_prompt = Some(last_prompt);
    }
    if let Some(agent_name) = value.get("agentName").and_then(value_to_string) {
        conversation.agent_name = Some(agent_name);
    }
    if let Some(summary) = value.get("summary").and_then(value_to_string) {
        conversation.summary = Some(summary);
    }
    // Token usage lives at `message.usage` for assistant entries. The
    // shape matches the Anthropic API response verbatim. Accumulate
    // across every line; the model name (also under `message.model`)
    // is generally stable per session so the last seen one wins.
    if let Some(message) = value.get("message") {
        if let Some(usage) = message.get("usage") {
            let mut saw_any = false;
            if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
                conversation.token_input += n;
                saw_any = true;
            }
            if let Some(n) = usage.get("output_tokens").and_then(Value::as_u64) {
                conversation.token_output += n;
                saw_any = true;
            }
            if let Some(n) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
                conversation.token_cache_read += n;
                saw_any = true;
            }
            if let Some(n) = usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
            {
                conversation.token_cache_creation += n;
                saw_any = true;
            }
            if saw_any {
                conversation.has_token_data = true;
            }
        }
        if let Some(m) = message.get("model").and_then(value_to_string) {
            // Skip placeholder strings the CLI sometimes writes
            // (`<synthetic>`, etc.).
            if !m.starts_with('<') {
                conversation.model = Some(m);
            }
        }
    }
}

fn is_claude_session_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(is_uuid_like)
}

fn is_uuid_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && value.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

enum ClaudePromptCandidate {
    Meaningful(String),
    Command(String),
    None,
}

fn claude_first_prompt_from_value(value: &Value) -> ClaudePromptCandidate {
    if value.get("type").and_then(Value::as_str) != Some("user")
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
        || value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return ClaudePromptCandidate::None;
    }
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return ClaudePromptCandidate::None;
    };
    if claude_user_content_is_tool_result_only(content) {
        return ClaudePromptCandidate::None;
    }
    for text in claude_text_blocks(content) {
        let prompt = text.replace('\n', " ").trim().to_string();
        if prompt.is_empty() {
            continue;
        }
        if let Some(command) = extract_xml_tag_value(&prompt, "command-name") {
            if !command.trim().is_empty() {
                return ClaudePromptCandidate::Command(command.trim().to_string());
            }
            continue;
        }
        if let Some(command) = extract_xml_tag_value(&prompt, "bash-input") {
            return ClaudePromptCandidate::Meaningful(truncate_claude_first_prompt(&format!(
                "! {}",
                command.trim()
            )));
        }
        if claude_skip_first_prompt_pattern(&prompt) {
            continue;
        }
        return ClaudePromptCandidate::Meaningful(truncate_claude_first_prompt(&prompt));
    }
    ClaudePromptCandidate::None
}

fn truncate_claude_first_prompt(value: &str) -> String {
    if value.chars().count() > 200 {
        format!("{}…", value.chars().take(200).collect::<String>().trim())
    } else {
        value.to_string()
    }
}

fn claude_skip_first_prompt_pattern(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("[Request interrupted by user")
        || trimmed
            .strip_prefix('<')
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_lowercase())
}

fn strip_display_tags(text: &str) -> String {
    let stripped = strip_display_tags_allow_empty(text);
    if stripped.is_empty() {
        text.trim().to_string()
    } else {
        stripped
    }
}

fn strip_display_tags_allow_empty(text: &str) -> String {
    let mut result = String::new();
    let mut index = 0;
    while let Some(relative_start) = text[index..].find('<') {
        let start = index + relative_start;
        result.push_str(&text[index..start]);
        let Some(relative_close) = text[start..].find('>') else {
            result.push_str(&text[start..]);
            index = text.len();
            break;
        };
        let tag_content = &text[start + 1..start + relative_close];
        let tag_name = tag_content
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches('/');
        if tag_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
            && tag_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            let end_tag = format!("</{tag_name}>");
            if let Some(relative_end) = text[start + relative_close + 1..].find(&end_tag) {
                index = start + relative_close + 1 + relative_end + end_tag.len();
                if text[index..].starts_with('\n') {
                    index += 1;
                }
                continue;
            }
        }
        result.push_str(&text[start..start + relative_close + 1]);
        index = start + relative_close + 1;
    }
    if index < text.len() {
        result.push_str(&text[index..]);
    }
    result.trim().to_string()
}

fn file_time(path: &Path, modified: bool) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let time = if modified {
        metadata.modified().ok()
    } else {
        metadata.created().ok().or_else(|| metadata.modified().ok())
    }?;
    Some(system_time_to_iso(time))
}

fn system_time_to_iso(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();
    datetime.to_rfc3339()
}

fn scan_gemini_chats_dir(root: &Path) -> Result<Vec<AppSession>, Box<dyn Error>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !is_gemini_session_file(&path) {
            continue;
        }
        if let Ok(detail) = parse_gemini_session_file(&path, true) {
            sessions.push(detail.session);
        }
    }
    Ok(sessions)
}

fn parse_gemini_jsonl_session(path: &Path) -> Result<SessionDetail, Box<dyn Error>> {
    parse_gemini_session_file(path, false)
}

fn parse_gemini_json_session(path: &Path) -> Result<SessionDetail, Box<dyn Error>> {
    parse_gemini_session_file(path, false)
}

#[derive(Default)]
struct GeminiConversation {
    session_id: String,
    start_time: Option<String>,
    last_updated: Option<String>,
    summary: Option<String>,
    first_user_message: Option<String>,
    kind: Option<String>,
    messages: Vec<Value>,
    message_count: usize,
    has_user_or_assistant: bool,
    // Token + model are extracted during the same line/record walk as
    // the loader does, so metadata_only scans (Stats page) get the
    // numbers without paying for full message construction.
    token_stats: Option<TokenStats>,
    model: Option<String>,
    daily_tokens: Option<Vec<DailyTokenBreakdown>>,
}

fn is_gemini_session_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    file_name.starts_with("session-")
        && (file_name.ends_with(".json") || file_name.ends_with(".jsonl"))
}

fn parse_gemini_session_file(
    path: &Path,
    metadata_only: bool,
) -> Result<SessionDetail, Box<dyn Error>> {
    let mut conversation = match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => load_gemini_jsonl_conversation(path, metadata_only)?,
        Some("json") => load_gemini_json_conversation(path, metadata_only)?,
        _ => return Err("unsupported Gemini session file".into()),
    };
    if conversation.session_id.is_empty()
        || !conversation.has_user_or_assistant
        || conversation.kind.as_deref() == Some("subagent")
    {
        return Err("not a Gemini session list entry".into());
    }

    // Timestamp fallback mirrors getAllSessionFiles in
    // packages/cli/src/utils/sessionUtils.ts: when startTime/lastUpdated are
    // missing on the record, fall back to (in order) the other field, the
    // file's mtime, then "now". Without this, sessions written by older
    // recorders that omit timestamps would be hidden from Termory even though
    // the official CLI still lists them.
    if conversation.start_time.is_none() || conversation.last_updated.is_none() {
        let mtime = file_time(path, true);
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let fallback = mtime.unwrap_or(now);
        if conversation.start_time.is_none() {
            conversation.start_time = conversation
                .last_updated
                .clone()
                .or_else(|| Some(fallback.clone()));
        }
        if conversation.last_updated.is_none() {
            conversation.last_updated = conversation.start_time.clone().or(Some(fallback));
        }
    }

    let project = gemini_project_from_chat_path(path).ok_or("missing Gemini .project_root")?;

    // Per-message `tokens` block + `model` field are accumulated
    // inside the loader so metadata_only scans (Stats page) get the
    // same numbers as detail loads — the messages array is empty in
    // that mode, so we can't recompute here.
    let token_stats = conversation.token_stats.take();
    let model = conversation.model.take();
    let daily_tokens = conversation.daily_tokens.take();

    let messages = if metadata_only {
        Vec::new()
    } else {
        gemini_messages_from_values(&conversation.messages)
    };
    let title = conversation
        .summary
        .as_deref()
        .map(strip_unsafe_characters)
        .and_then(|title| official_title_from_text(&title))
        // Cap ONLY the first-user-message fallback (a session_context-derived
        // title can be very long); `snippet_line` collapses whitespace and trims
        // to 120 chars + …. The summary path is left untouched.
        .or_else(|| conversation.first_user_message.as_deref().map(snippet_line));
    let mut detail = detail_from_messages(
        "Gemini",
        path,
        conversation.session_id,
        project,
        messages,
        Vec::new(),
        title,
    );
    detail.session.started_at = conversation.start_time;
    detail.session.updated_at = conversation.last_updated;
    detail.session.message_count = conversation.message_count;
    detail.session.tokens = token_stats;
    detail.session.model = model;
    detail.session.daily_tokens = daily_tokens;
    Ok(detail)
}

/// Streaming token-usage accumulator. Lets the loader feed each
/// record as it parses lines, so scan-time (metadata_only) paths get
/// the same numbers as detail-time paths without rebuilding the
/// full messages array.
#[derive(Default)]
struct GeminiTokenAccumulator {
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
    total: u64,
    saw_tokens: bool,
    model: Option<String>,
    /// Per-day token spend, keyed by local YYYY-MM-DD. Built only
    /// when records carry their own `timestamp` field (Gemini does
    /// for normal session records). Stored as a sorted BTreeMap so
    /// the finish step can hand back a chronological Vec without
    /// re-sorting.
    /// [input, output, cached, reasoning, total, messages]
    daily: BTreeMap<String, [u64; 6]>,
    /// Per-hour message count by local date.
    daily_hours: BTreeMap<String, [u64; 24]>,
    /// Per-hour token total by local date.
    daily_hour_tokens: BTreeMap<String, [u64; 24]>,
}

impl GeminiTokenAccumulator {
    fn observe(&mut self, record: &Value) {
        let date_hour = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso_to_local_date_hour);
        let date_key = date_hour.as_ref().map(|(d, _)| d.clone());
        if let Some(tokens) = record.get("tokens") {
            let mut any = false;
            let mut line_input = 0u64;
            let mut line_output = 0u64;
            let mut line_cached = 0u64;
            let mut line_reasoning = 0u64;
            let mut line_total = 0u64;
            if let Some(n) = tokens.get("input").and_then(Value::as_u64) {
                self.input += n;
                line_input = n;
                any = true;
            }
            if let Some(n) = tokens.get("output").and_then(Value::as_u64) {
                self.output += n;
                line_output = n;
                any = true;
            }
            if let Some(n) = tokens.get("cached").and_then(Value::as_u64) {
                self.cached += n;
                line_cached = n;
                any = true;
            }
            if let Some(n) = tokens.get("thoughts").and_then(Value::as_u64) {
                self.reasoning += n;
                line_reasoning = n;
                any = true;
            }
            if let Some(n) = tokens.get("total").and_then(Value::as_u64) {
                self.total += n;
                line_total = n;
                any = true;
            }
            if any {
                self.saw_tokens = true;
                if let Some(date) = date_key.clone() {
                    let bucket = self.daily.entry(date.clone()).or_insert([0u64; 6]);
                    bucket[0] += line_input;
                    bucket[1] += line_output;
                    bucket[2] += line_cached;
                    bucket[3] += line_reasoning;
                    bucket[4] += line_total;
                    bucket[5] += 1; // one tokens record = 1 interaction
                    if let Some((_, hour)) = date_hour.as_ref() {
                        let hourbox = self.daily_hours.entry(date.clone()).or_insert([0u64; 24]);
                        hourbox[*hour as usize] += 1;
                        let recorded_total = if line_total > 0 {
                            line_total
                        } else {
                            line_input + line_output + line_cached + line_reasoning
                        };
                        let tokbox = self.daily_hour_tokens.entry(date).or_insert([0u64; 24]);
                        tokbox[*hour as usize] += recorded_total;
                    }
                }
            }
        }
        if let Some(m) = record.get("model").and_then(value_to_string) {
            self.model = Some(m);
        }
    }

    fn finish(
        self,
    ) -> (
        Option<TokenStats>,
        Option<String>,
        Option<Vec<DailyTokenBreakdown>>,
    ) {
        let stats = if self.saw_tokens {
            Some(TokenStats {
                input: self.input,
                output: self.output,
                cached: self.cached,
                reasoning: self.reasoning,
                // Use the per-record `total` sum when present; else
                // derive from components.
                total: if self.total > 0 {
                    self.total
                } else {
                    self.input + self.output + self.cached + self.reasoning
                },
            })
        } else {
            None
        };
        let daily = if self.daily.is_empty() {
            None
        } else {
            Some(
                self.daily
                    .into_iter()
                    .map(|(date, [i, o, c, r, t, m])| {
                        let hours = self
                            .daily_hours
                            .get(&date)
                            .map(|a| a.to_vec())
                            .unwrap_or_default();
                        let hour_tokens = self
                            .daily_hour_tokens
                            .get(&date)
                            .map(|a| a.to_vec())
                            .unwrap_or_default();
                        DailyTokenBreakdown {
                            date,
                            tokens: TokenStats {
                                input: i,
                                output: o,
                                cached: c,
                                reasoning: r,
                                total: if t > 0 { t } else { i + o + c + r },
                            },
                            messages: m,
                            hours,
                            hour_tokens,
                        }
                    })
                    .collect(),
            )
        };
        (stats, self.model, daily)
    }
}

fn gemini_extract_token_usage(
    records: &[Value],
) -> (
    Option<TokenStats>,
    Option<String>,
    Option<Vec<DailyTokenBreakdown>>,
) {
    let mut acc = GeminiTokenAccumulator::default();
    for record in records {
        acc.observe(record);
    }
    acc.finish()
}

fn load_gemini_jsonl_conversation(
    path: &Path,
    metadata_only: bool,
) -> Result<GeminiConversation, Box<dyn Error>> {
    let content = fs::read_to_string(path)?;
    let mut metadata = Value::Object(Default::default());
    let mut messages_map = HashMap::<String, Value>::new();
    let mut message_order = Vec::<String>::new();
    let mut message_ids = Vec::<String>::new();
    let mut message_kinds = HashMap::<String, (bool, bool)>::new();
    let mut first_user_message: Option<String> = None;
    let mut token_acc = GeminiTokenAccumulator::default();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Track tokens / model on every record so metadata_only scans
        // (Stats page) get the same numbers detail loads do, without
        // paying for the full messages array construction.
        token_acc.observe(&value);
        if let Some(rewind_id) = value.get("$rewindTo").and_then(value_to_string) {
            if metadata_only {
                if let Some(index) = message_ids.iter().position(|id| id == &rewind_id) {
                    for removed in message_ids.split_off(index) {
                        message_kinds.remove(&removed);
                    }
                } else {
                    message_ids.clear();
                    message_kinds.clear();
                }
            } else {
                if let Some(index) = message_order.iter().position(|id| id == &rewind_id) {
                    for id in message_order.split_off(index) {
                        messages_map.remove(&id);
                    }
                } else {
                    messages_map.clear();
                    message_order.clear();
                }
            }
            continue;
        }

        if let Some(id) = value.get("id").and_then(value_to_string) {
            let is_user = value.get("type").and_then(Value::as_str) == Some("user");
            let is_user_or_assistant = matches!(
                value.get("type").and_then(Value::as_str),
                Some("user" | "gemini")
            );
            if is_user && first_user_message.is_none() {
                first_user_message = value
                    .get("content")
                    .and_then(gemini_content_text_raw)
                    .map(|content| gemini_clean_message(&content));
            }
            if metadata_only {
                message_ids.push(id.clone());
                message_kinds.insert(id.clone(), (is_user, is_user_or_assistant));
            }
            if !metadata_only {
                if !messages_map.contains_key(&id) {
                    message_order.push(id.clone());
                }
                messages_map.insert(id, value);
            }
            continue;
        }

        if let Some(update) = value.get("$set").and_then(Value::as_object) {
            let mut next = metadata.as_object().cloned().unwrap_or_default();
            for (key, value) in update {
                next.insert(key.clone(), value.clone());
            }
            metadata = Value::Object(next);
            continue;
        }

        if value.get("sessionId").is_some() && value.get("projectHash").is_some() {
            let mut next = metadata.as_object().cloned().unwrap_or_default();
            if let Some(object) = value.as_object() {
                for (key, value) in object {
                    next.insert(key.clone(), value.clone());
                }
            }
            metadata = Value::Object(next);
        }
    }

    let metadata_messages = metadata
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let loaded_messages = if metadata_messages.is_empty() {
        message_order
            .into_iter()
            .filter_map(|id| messages_map.remove(&id))
            .collect::<Vec<_>>()
    } else {
        metadata_messages.clone()
    };
    let message_count = if metadata_only {
        if metadata_messages.is_empty() {
            message_ids.len()
        } else {
            metadata_messages.len()
        }
    } else {
        loaded_messages.len()
    };
    let has_user_or_assistant = if metadata_only && !metadata_messages.is_empty() {
        metadata_messages.iter().any(gemini_is_user_or_assistant)
    } else if metadata_only {
        message_kinds.values().any(|(_, visible)| *visible)
    } else {
        loaded_messages.iter().any(gemini_is_user_or_assistant)
    };
    // metadata_only path skips the loop above, so feed the metadata
    // messages (set via `$set`) into the accumulator before finishing.
    if metadata_only && !metadata_messages.is_empty() {
        for record in &metadata_messages {
            token_acc.observe(record);
        }
    }
    let (token_stats, model, daily_tokens) = token_acc.finish();

    Ok(GeminiConversation {
        session_id: metadata
            .get("sessionId")
            .and_then(value_to_string)
            .unwrap_or_default(),
        start_time: metadata
            .get("startTime")
            .and_then(value_to_string)
            .and_then(normalize_time),
        last_updated: metadata
            .get("lastUpdated")
            .and_then(value_to_string)
            .and_then(normalize_time),
        summary: metadata.get("summary").and_then(value_to_string),
        first_user_message: metadata
            .get("firstUserMessage")
            .and_then(value_to_string)
            .map(|message| gemini_clean_message(&message))
            .or_else(|| gemini_extract_first_user_message(&loaded_messages))
            .or(first_user_message),
        kind: metadata.get("kind").and_then(value_to_string),
        messages: if metadata_only {
            Vec::new()
        } else {
            loaded_messages
        },
        message_count,
        has_user_or_assistant,
        token_stats,
        model,
        daily_tokens,
    })
}

fn load_gemini_json_conversation(
    path: &Path,
    metadata_only: bool,
) -> Result<GeminiConversation, Box<dyn Error>> {
    let value = serde_json::from_str::<Value>(&fs::read_to_string(path)?)?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_user_or_assistant = messages.iter().any(gemini_is_user_or_assistant);
    // The .json layout has all messages up-front, so the simple
    // aggregating helper is enough — no streaming needed.
    let (token_stats, model, daily_tokens) = gemini_extract_token_usage(&messages);
    Ok(GeminiConversation {
        session_id: value
            .get("sessionId")
            .and_then(value_to_string)
            .unwrap_or_default(),
        start_time: value
            .get("startTime")
            .and_then(value_to_string)
            .and_then(normalize_time),
        last_updated: value
            .get("lastUpdated")
            .and_then(value_to_string)
            .and_then(normalize_time),
        summary: value.get("summary").and_then(value_to_string),
        first_user_message: value
            .get("firstUserMessage")
            .and_then(value_to_string)
            .map(|message| gemini_clean_message(&message))
            .or_else(|| gemini_extract_first_user_message(&messages)),
        kind: value.get("kind").and_then(value_to_string),
        messages: if metadata_only {
            Vec::new()
        } else {
            messages.clone()
        },
        message_count: messages.len(),
        has_user_or_assistant,
        token_stats,
        model,
        daily_tokens,
    })
}

/// Gemini injects an environment-context message as the FIRST `user` record of
/// every session — a `<session_context>…</session_context>` wrapper built by
/// `environmentContext.ts` (date / OS / workspace / directory tree). It is not a
/// real user prompt, so it must NOT become the session title. The official CLI
/// special-cases the same prefix (`toGraph.ts: text.startsWith('<session_context>')`).
fn gemini_is_injected_context(content: &str) -> bool {
    content.trim_start().starts_with("<session_context>")
}

fn gemini_extract_first_user_message(messages: &[Value]) -> Option<String> {
    let first_meaningful = messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| {
            message
                .get("content")
                .and_then(gemini_content_text_raw)
                // Strip Gemini's injected `<session_context>` wrapper, keep the inner text.
                .map(|content| {
                    gemini_clean_message(
                        &extract_xml_tag_value(&content, "session_context").unwrap_or(content),
                    )
                })
        })
        .find(|content| {
            !content.starts_with('/') && !content.starts_with('?') && !content.trim().is_empty()
        });
    if first_meaningful.is_some() {
        return first_meaningful;
    }
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| {
            message
                .get("content")
                .and_then(gemini_content_text_raw)
                .map(|content| gemini_clean_message(&content))
        })
        .find(|content| !content.trim().is_empty())
        .or_else(|| Some("Empty conversation".to_string()))
}

fn gemini_clean_message(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect::<String>()
        .trim()
        .to_string()
}

fn strip_unsafe_characters(message: &str) -> String {
    message
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect::<String>()
}

fn gemini_messages_from_values(values: &[Value]) -> Vec<SessionMessage> {
    let mut messages = Vec::new();
    for value in values {
        // Gemini's `thoughts` array (rendered by the TUI's `ThinkingMessage`
        // at .audit-sources/gemini-cli/packages/cli/src/ui/components/messages/ThinkingMessage.tsx)
        // is sibling to `content`. We surface each thought as a separate
        // reasoning message using the shared italic-blockquote format so
        // Claude / Codex / OpenCode / Gemini all use the same convention.
        messages.extend(gemini_thought_messages_from_value(value));
        if let Some(message) = gemini_message_from_value(value) {
            messages.push(message);
        }
        if value.get("type").and_then(Value::as_str) != Some("user") {
            messages.extend(gemini_tool_messages_from_value(value));
        }
    }
    messages
}

/// Extract Gemini thought entries (`thoughts: [{subject, description, ...}]`)
/// and emit each as a reasoning SessionMessage. Mirrors
/// `normalizeThoughtLines` in
/// `.audit-sources/gemini-cli/packages/cli/src/ui/components/messages/ThinkingMessage.tsx:22`
/// — subject + description joined, noise lines (whitespace / `...` runs)
/// filtered out.
///
/// Per the TUI's render at l.82-93, the subject is **bold italic** while
/// description lines are plain italic. We wrap the subject in `**...**`
/// before joining so `format_reasoning_body` preserves it as a bold
/// blockquote header (with description lines as italic body) — matches
/// Codex's reasoning bold-header path.
///
/// Source also applies `normalizeEscapedNewlines` (textUtils.ts:168) to
/// both subject and description so persisted `\\n` / `\\r\\n` literals
/// render as real line breaks. Termory does the same before splitting.
fn gemini_thought_messages_from_value(value: &Value) -> Vec<SessionMessage> {
    let Some(thoughts) = value.get("thoughts").and_then(Value::as_array) else {
        return Vec::new();
    };
    let parent_ts = value
        .get("timestamp")
        .and_then(value_to_string)
        .and_then(normalize_time);
    let mut out = Vec::new();
    for thought in thoughts {
        let subject_raw = thought
            .get("subject")
            .and_then(Value::as_str)
            .map(gemini_normalize_escaped_newlines)
            .unwrap_or_default();
        let description_raw = thought
            .get("description")
            .and_then(Value::as_str)
            .map(gemini_normalize_escaped_newlines)
            .unwrap_or_default();
        let subject = subject_raw.trim();
        let description = description_raw.trim();
        let is_noise = |line: &str| {
            let trimmed = line.trim();
            trimmed.is_empty() || trimmed.chars().all(|c| c == '.')
        };
        let mut lines: Vec<String> = Vec::new();
        if !subject.is_empty() && !is_noise(subject) {
            // Bold subject so format_reasoning_body keeps it as a bold
            // header line in the blockquote (matches TUI bold-italic).
            lines.push(format!("**{subject}**"));
        }
        if !description.is_empty() {
            lines.extend(
                description
                    .lines()
                    .map(str::trim)
                    .filter(|line| !is_noise(line))
                    .map(str::to_string),
            );
        }
        if lines.is_empty() {
            continue;
        }
        let joined = lines.join("\n");
        let formatted = format_reasoning_body(&joined);
        if formatted.is_empty() {
            continue;
        }
        let ts = thought
            .get("timestamp")
            .and_then(value_to_string)
            .and_then(normalize_time)
            .or_else(|| parent_ts.clone());
        out.push(SessionMessage {
            role: "assistant".to_string(),
            text: formatted,
            timestamp: ts,
            kind: kind::REASONING.to_string(),
            tool_use_id: None,
            exit_code: None,
        });
    }
    out
}

/// Fence only the directory-tree block(s) inside Gemini's session_context so the
/// tree (`├─└│` box chars + indentation) keeps its layout, while the surrounding
/// `- **…**` markdown still renders. A tree block = a contiguous run of lines
/// containing box-drawing chars, plus the root-path line just before it.
fn gemini_fence_tree_blocks(text: &str) -> String {
    let is_tree = |l: &str| l.contains('├') || l.contains('│') || l.contains('└');
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_tree(lines[i]) {
            let mut block: Vec<String> = Vec::new();
            // Pull the preceding non-blank line (the tree's root path) into the
            // fence so the header stays attached to the tree.
            if out.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                block.push(out.pop().unwrap());
            }
            while i < lines.len() && is_tree(lines[i]) {
                block.push(lines[i].to_string());
                i += 1;
            }
            out.push("````".to_string());
            out.extend(block);
            out.push("````".to_string());
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }
    out.join("\n")
}

fn gemini_message_from_value(value: &Value) -> Option<SessionMessage> {
    let raw_type = value.get("type").and_then(value_to_string)?;
    let role = match raw_type.as_str() {
        "user" => "user",
        "gemini" => "assistant",
        "info" | "error" | "warning" => raw_type.as_str(),
        _ => return None,
    };
    let raw_text = value
        .get("displayContent")
        .and_then(gemini_content_text_raw)
        .or_else(|| value.get("content").and_then(gemini_content_text_raw))?;
    if raw_text.trim().is_empty() {
        return None;
    }
    // Gemini injects a `<session_context>…</session_context>` environment block
    // (date / OS / workspace / directory tree) as the first `user` record. The
    // official TUI hides it; Termory surfaces it — but UNWRAPPED, as a system
    // notice, mirroring the Codex `<environment_context>` handling
    // (`codex_user_message_event`). Unwrapping also keeps the inner content from
    // being swallowed by the markdown renderer's raw-HTML-block rule.
    if role == "user" && gemini_is_injected_context(&raw_text) {
        let inner =
            extract_xml_tag_value(&raw_text, "session_context").unwrap_or_else(|| raw_text.clone());
        let inner = inner.trim();
        // Fence ONLY the directory-tree block(s) so the tree keeps its layout
        // while the surrounding `- **…**` markdown still renders.
        let body = if inner.is_empty() {
            "*[Gemini session_context]*".to_string()
        } else {
            format!(
                "*[Gemini session_context]*\n\n{}",
                gemini_fence_tree_blocks(inner)
            )
        };
        let timestamp = value
            .get("timestamp")
            .and_then(value_to_string)
            .and_then(normalize_time);
        return Some(SessionMessage {
            role: "system".to_string(),
            text: body,
            timestamp,
            kind: kind::TEXT.to_string(),
            tool_use_id: None,
            exit_code: None,
        });
    }
    // System notices (info / error / warning) get a leading TUI icon +
    // italic body so they're visually distinct from regular assistant /
    // user text. Prefixes come straight from the TUI components:
    //   * info     → `ℹ ` (InfoMessage.tsx:30, warning color)
    //   * error    → `✕ ` (ErrorMessage.tsx:16, error color)
    //   * warning  → `⚠ ` (WarningMessage.tsx:17, warning color)
    let text = match raw_type.as_str() {
        "info" => format_gemini_system_notice("ℹ", &raw_text),
        "error" => format_gemini_system_notice("✕", &raw_text),
        "warning" => format_gemini_system_notice("⚠", &raw_text),
        _ => raw_text,
    };
    let timestamp = value
        .get("timestamp")
        .and_then(value_to_string)
        .and_then(normalize_time);
    Some(SessionMessage {
        role: role.to_string(),
        text,
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

fn gemini_tool_messages_from_value(value: &Value) -> Vec<SessionMessage> {
    let Some(tools) = value.get("toolCalls").and_then(Value::as_array) else {
        return Vec::new();
    };
    let timestamp = value
        .get("timestamp")
        .and_then(value_to_string)
        .and_then(normalize_time);
    tools
        .iter()
        .filter_map(|tool| {
            // Gemini TUI render: ToolMessage → ToolInfo at
            //   .audit-sources/gemini-cli/packages/cli/src/ui/components/messages/ToolShared.tsx:202
            // `<bold>{displayName}</bold> {description}` — bold tool display
            // name + space + description in secondary text. Result content
            // renders separately below via ToolResultDisplay using the
            // `resultDisplay` field (IndividualToolCallDisplay).
            let display_name = tool
                .get("displayName")
                .and_then(value_to_string)
                .or_else(|| tool.get("name").and_then(value_to_string))?;
            let description = tool
                .get("description")
                .and_then(value_to_string)
                .unwrap_or_default();
            // Per `sessionUtils.ts:654-657`, Gemini's session loader maps
            // `status === 'success'` → Success, anything else → Error. So
            // any non-'success' status (e.g. 'error', 'cancelled') flips
            // the leading marker to `✗` and the body gets an `Error:`
            // prefix — matching Codex / Claude failure handling.
            let status = tool.get("status").and_then(Value::as_str).unwrap_or("");
            let failed = !status.is_empty() && status != "success";

            // Unified `**Verb**(args)` shape — description (Gemini's TUI
            // secondary text from `ToolShared.tsx:202`) goes inside parens
            // so the card reads the same as Codex / Claude / OpenCode tool
            // headers.
            let header = if description.is_empty() {
                format!("**{display_name}**")
            } else {
                format!("**{display_name}**({description})")
            };
            let mut text = String::from(status_marker(failed));
            text.push_str(&header);

            let body = tool
                .get("resultDisplay")
                .and_then(gemini_result_display_to_text);
            if let Some(b) = body {
                let trimmed = b.trim();
                if !trimmed.is_empty() {
                    text.push_str("\n\n");
                    if failed {
                        // Failures: `Error: ` prefix inside a 4-backtick
                        // fence (matches Codex / Claude failure shape).
                        text.push_str("````\nError: ");
                        text.push_str(trimmed);
                        text.push_str("\n````");
                    } else if trimmed.starts_with("```") {
                        // Diff / structured fence already present (the
                        // body is a complete ```diff … ``` block from
                        // `gemini_result_display_to_text`).
                        text.push_str(trimmed);
                    } else {
                        // Plain string / pretty-printed JSON output:
                        // wrap in 4-backtick fence so embedded triple
                        // backticks in the content survive.
                        text.push_str("````\n");
                        text.push_str(trimmed);
                        text.push_str("\n````");
                    }
                }
            } else if failed {
                // No body but failed status — still surface the failure.
                text.push_str("\n\n````\nError\n````");
            }
            Some(SessionMessage {
                role: "tool".to_string(),
                text,
                timestamp: timestamp.clone(),
                kind: if failed {
                    kind::TOOL_ERROR.to_string()
                } else {
                    kind::TOOL_USE.to_string()
                },
                tool_use_id: None,
                exit_code: None,
            })
        })
        .collect()
}

/// Convert a Gemini `resultDisplay` value to a markdown body string per
/// `ToolResultDisplay.tsx`:
///
/// * `null` / empty → `None` (no body)
/// * `string` → returned verbatim (markdown / plain-text rendered as-is)
/// * `Array<AnsiLine>` where each line is `Array<AnsiToken>` → join token
///   `text` fields with `\n` between lines, trim per-line trailing
///   whitespace (xterm-headless pads each line to terminal width). Detect
///   by checking whether the first element is itself an array.
/// * `Array<Part>` (legacy Gemini content parts) → fall through to
///   `gemini_content_text_raw`.
/// * `{ todos }` → drop (TUI hides it; TodoTray renders todos separately —
///   `ToolResultDisplay.tsx:84-87`).
/// * `{ isSubagentProgress: true, ... }` → drop (complex live-progress
///   widget; no useful static representation).
/// * `{ fileDiff, fileName? }` → ```diff fence wrapping the unified diff
///   (the diff already embeds file headers, so `fileName` is not surfaced
///   separately — `ToolResultDisplay.tsx:145-161`).
/// * `{ summary, ... }` (StructuredToolResult — GrepResult / ListDirectory /
///   ReadManyFiles all share this shape) → return the `summary` string.
/// * other object → `serde_json::to_string_pretty` (matches the TUI's
///   `JSON.stringify(obj, null, 2)` fallback at
///   `ToolResultDisplay.tsx:179-184`).
fn gemini_result_display_to_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Value::Array(items) => {
            // AnsiOutput detection per `isAnsiOutput` in types.ts:105-106:
            // an array whose first element is itself an array.
            if items.first().is_some_and(Value::is_array) {
                let mut lines = Vec::with_capacity(items.len());
                for line in items {
                    let Some(tokens) = line.as_array() else {
                        continue;
                    };
                    let mut row = String::new();
                    for tok in tokens {
                        if let Some(text) = tok.get("text").and_then(Value::as_str) {
                            row.push_str(text);
                        }
                    }
                    lines.push(row.trim_end().to_string());
                }
                let joined = lines.join("\n");
                if joined.trim().is_empty() {
                    None
                } else {
                    Some(joined)
                }
            } else {
                gemini_content_text_raw(value)
            }
        }
        Value::Object(map) => {
            if map.contains_key("todos") {
                return None;
            }
            if map
                .get("isSubagentProgress")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            if let Some(file_diff) = map.get("fileDiff").and_then(Value::as_str) {
                let file_name = map.get("fileName").and_then(Value::as_str);
                return Some(gemini_format_file_diff(file_diff, file_name));
            }
            if let Some(summary) = map.get("summary").and_then(Value::as_str) {
                if !summary.trim().is_empty() {
                    return Some(summary.to_string());
                }
            }
            serde_json::to_string_pretty(value).ok()
        }
        _ => None,
    }
}

/// Mirror Gemini TUI's `normalizeEscapedNewlines`
/// (`packages/cli/src/ui/utils/textUtils.ts:168`): convert escaped CRLF
/// (`\r\n`) and LF (`\n`) literals to real line breaks. Persisted
/// `subject` / `description` strings sometimes carry the escaped form
/// literally; without this they render as a single line with visible
/// `\n` sequences.
fn gemini_normalize_escaped_newlines(value: &str) -> String {
    value.replace("\\r\\n", "\n").replace("\\n", "\n")
}

/// Wrap an info / warning / error system notice in italic with the
/// TUI's icon glyph. Multi-line bodies stay italic across each line so
/// the entire notice reads as one styled block (matches the way
/// InfoMessage/ErrorMessage/WarningMessage colors the whole text).
fn format_gemini_system_notice(icon: &str, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (idx, line) in trimmed.lines().enumerate() {
        if idx > 0 {
            // Markdown line breaks via two trailing spaces, so the italic
            // span continues across visual lines without a paragraph
            // break (which would terminate the italic).
            out.push_str("  \n");
        }
        if idx == 0 {
            out.push('*');
            out.push_str(icon);
            out.push(' ');
            out.push_str(line.trim());
            // Close italic at the very end (handled outside the loop)
        } else {
            out.push_str(line.trim());
        }
    }
    out.push('*');
    out
}

/// Render a Gemini `resultDisplay: {fileDiff, fileName?}` body. Per
/// `DiffRenderer.tsx:204-214`, a "new file" diff (every non-context line
/// is an add / hunk / header) is rendered as raw added content in a
/// syntax-highlighted code block, not as a diff. Termory mirrors:
///   * new file → ```{lang}\n{added lines}\n```` (lang inferred from
///     the filename extension)
///   * normal patch → ```diff\n{full diff}\n````
fn gemini_format_file_diff(file_diff: &str, file_name: Option<&str>) -> String {
    let diff = file_diff.trim_end_matches('\n');
    if gemini_is_new_file_diff(diff) {
        let added: Vec<&str> = diff
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .map(|line| &line[1..])
            .collect();
        let body = added.join("\n");
        let lang = file_name
            .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext))
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if lang.is_empty() {
            format!("```\n{body}\n```")
        } else {
            format!("```{lang}\n{body}\n```")
        }
    } else {
        format!("```diff\n{diff}\n```")
    }
}

/// Approximation of TUI's `isNewFile` predicate (DiffRenderer.tsx:204-214):
/// at least one `+` line is present, AND every meaningful line is either
/// an addition, a hunk header `@@`, or a unified-diff header. We treat
/// any non-empty content line starting with `-` or ` ` (context) as
/// proof the file is not new.
fn gemini_is_new_file_diff(diff: &str) -> bool {
    let mut has_add = false;
    for line in diff.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with("@@") {
            continue;
        }
        if line.starts_with("diff --git") || line.starts_with("new file mode") {
            continue;
        }
        if line.starts_with('+') {
            has_add = true;
            continue;
        }
        // Any deletion / context line → not a new file.
        return false;
    }
    has_add
}

fn gemini_content_text_raw(value: &Value) -> Option<String> {
    let mut parts = Vec::new();
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                if let Some(text) = gemini_part_to_string(item) {
                    parts.push(text);
                }
            }
        }
        _ => {}
    }
    let text = parts.join("").trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn gemini_part_to_string(value: &Value) -> Option<String> {
    if let Some(text) = value.get("text").and_then(value_to_string) {
        return Some(text);
    }
    if value.get("videoMetadata").is_some() {
        return Some("[Video Metadata]".to_string());
    }
    if let Some(thought) = value.get("thought").and_then(value_to_string) {
        return Some(format!("[Thought: {thought}]"));
    }
    // `executableCode` ({code, language}) is the model's "I will run this
    // code" announcement; render as a fenced block so it's visible
    // (matches Gemini TUI's `ExecutableCodeRenderer`, which displays code
    // in a syntax-highlighted block).
    if let Some(exec) = value.get("executableCode") {
        let code = exec.get("code").and_then(Value::as_str).unwrap_or("");
        let lang = exec
            .get("language")
            .and_then(Value::as_str)
            .map(|s| s.to_lowercase())
            .filter(|s| !s.is_empty());
        if !code.trim().is_empty() {
            return Some(match lang {
                Some(l) => format!("```{l}\n{}\n```", code.trim_end()),
                None => format!("```\n{}\n```", code.trim_end()),
            });
        }
    }
    // `codeExecutionResult` ({outcome, output}) is the result of running
    // executable code. Show the output in a fenced block + outcome label
    // (e.g., `OUTCOME_OK` / `OUTCOME_FAILED`).
    if let Some(result) = value.get("codeExecutionResult") {
        let output = result
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end();
        let outcome = result.get("outcome").and_then(Value::as_str).unwrap_or("");
        if !output.is_empty() || !outcome.is_empty() {
            let mut text = String::new();
            if !output.is_empty() {
                text.push_str("````\n");
                text.push_str(output);
                text.push_str("\n````");
            }
            if !outcome.is_empty() && outcome != "OUTCOME_OK" {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!("*Outcome: {outcome}*"));
            }
            return Some(text);
        }
    }
    // `inlineData` ({mimeType, data}) carries base64 binary content
    // (images / audio). We can't show it in markdown but we should
    // signal what was attached.
    if let Some(inline) = value.get("inlineData") {
        let mime = inline
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        return Some(format!("*Inline data ({mime})*"));
    }
    if value.get("fileData").is_some() {
        let uri = value
            .get("fileData")
            .and_then(|f| f.get("fileUri"))
            .and_then(Value::as_str);
        return Some(match uri {
            Some(u) => format!("*File: {u}*"),
            None => "[File Data]".to_string(),
        });
    }
    // `functionCall` / `functionResponse` in PARTS (sibling to `text`)
    // are inline markers of the assistant invoking a tool. The structured
    // ToolCall/ToolResponse cards arrive separately via `toolCalls` and
    // are rendered by `gemini_tool_messages_from_value`. The PART version
    // here just signals "a tool call happened" without duplicating the
    // full body.
    if let Some(call) = value.get("functionCall") {
        let name = call
            .get("name")
            .and_then(value_to_string)
            .unwrap_or_default();
        let display = if name.is_empty() {
            "*Tool call*".to_string()
        } else {
            format!("*Tool call: {name}*")
        };
        return Some(display);
    }
    if let Some(response) = value.get("functionResponse") {
        let name = response
            .get("name")
            .and_then(value_to_string)
            .unwrap_or_default();
        let display = if name.is_empty() {
            "*Tool response*".to_string()
        } else {
            format!("*Tool response: {name}*")
        };
        return Some(display);
    }
    None
}

fn gemini_is_user_or_assistant(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("user" | "gemini")
    )
}

fn gemini_project_from_chat_path(path: &Path) -> Option<String> {
    let project_dir = path.parent()?.parent()?;
    fs::read_to_string(project_dir.join(".project_root"))
        .ok()
        .map(|project| project.trim().to_string())
        .filter(|project| !project.is_empty())
}

fn parse_codex_session(path: &Path, id: &str) -> Result<SessionDetail, Box<dyn Error>> {
    let mut session_from_state = codex_thread_from_state(id).ok();
    let content = fs::read_to_string(path)?;
    let mut messages = Vec::new();
    let mut session_id = id.to_string();
    let mut project = session_from_state
        .as_ref()
        .map(|session| session.project.clone())
        .unwrap_or_default();
    let mut timestamps = Vec::new();
    // Codex emits periodic `event_msg.payload.type = "token_count"`
    // events. The `info.total_token_usage` block is cumulative for
    // the whole session, so we just keep overwriting and the last
    // non-null one wins.
    let mut latest_token_usage: Option<TokenStats> = None;
    let mut model: Option<String> = None;

    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(timestamp) = value.get("timestamp").and_then(value_to_string) {
            timestamps.push(timestamp.clone());
        }
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            if let Some(payload) = value.get("payload") {
                if let Some(found_id) = payload.get("id").and_then(value_to_string) {
                    session_id = found_id;
                }
                if let Some(cwd) = payload.get("cwd").and_then(value_to_string) {
                    project = cwd;
                }
                // SessionMeta has no `model` field in current Codex (only
                // model_provider) — kept for older rollouts that did. The real
                // model comes from turn_context below.
                if let Some(m) = payload.get("model").and_then(value_to_string) {
                    model = Some(m);
                }
            }
            continue;
        }
        // Model id lives in `turn_context.payload.model` (TurnContextItem.model,
        // once per user turn; /model switches re-emit it → last-seen wins).
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            if let Some(m) = value
                .get("payload")
                .and_then(|p| p.get("model"))
                .and_then(value_to_string)
            {
                if !m.is_empty() {
                    model = Some(m);
                }
            }
            continue;
        }
        // Token usage rollups
        if value.get("type").and_then(Value::as_str) == Some("event_msg") {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("token_count") {
                    if let Some(usage) =
                        payload.get("info").and_then(|v| v.get("total_token_usage"))
                    {
                        let input = usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cached = usage
                            .get("cached_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let output = usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let reasoning = usage
                            .get("reasoning_output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let total = usage
                            .get("total_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(input + output + cached + reasoning);
                        latest_token_usage = Some(TokenStats {
                            input,
                            output,
                            cached,
                            reasoning,
                            total,
                        });
                    }
                }
            }
        }
        // Model can also surface on response_item messages.
        if model.is_none() {
            if let Some(m) = value
                .get("payload")
                .and_then(|p| p.get("model"))
                .and_then(value_to_string)
            {
                model = Some(m);
            }
        }
        if let Some(message) = codex_message_from_value(&value) {
            messages.push(message);
        }
    }

    let messages = merge_tool_outputs(messages);

    let mut detail = detail_from_messages(
        "Codex",
        path,
        session_id,
        project,
        messages,
        timestamps,
        session_from_state
            .as_ref()
            .map(|session| session.title.clone()),
    );

    if let Some(session) = session_from_state.take() {
        detail.session.title = session.title;
        detail.session.started_at = session.started_at;
        detail.session.updated_at = session.updated_at;
        detail.session.message_count = detail.messages.len();
    }

    detail.session.tokens = latest_token_usage;
    detail.session.model = model;

    Ok(detail)
}

fn codex_thread_from_state(id: &str) -> Result<AppSession, Box<dyn Error>> {
    let Some(home) = crate::home_dir() else {
        return Err("home directory not found".into());
    };
    let path = crate::providers::codex_root(&home).join("state_5.sqlite");
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Archived threads load too (the badge stays accurate in the detail
    // header) — only the `archived = 0` list filter was for the picker.
    let mut stmt = conn.prepare(
        "select id, rollout_path, created_at, updated_at, cwd, title, first_user_message, archived \
         from threads \
         where id = ?1 \
           and source in ('cli', 'vscode', 'atlas', 'chatgpt')",
    )?;
    let session = stmt.query_row([id], |row| {
        let id: String = row.get(0)?;
        let rollout_path: String = row.get(1)?;
        let created_at: i64 = row.get(2)?;
        let updated_at: i64 = row.get(3)?;
        let cwd: String = row.get(4)?;
        let title: String = row.get(5).unwrap_or_default();
        let first_user_message: String = row.get(6).unwrap_or_default();
        let archived: bool = row.get::<_, i64>(7).unwrap_or(0) != 0;
        let display_title = codex_display_title(&title)
            .or_else(|| codex_display_title(&first_user_message))
            .unwrap_or_default();
        let snippet = snippet_line(&first_user_message);
        Ok(AppSession {
            id,
            source: "Codex".to_string(),
            title: display_title,
            project: cwd,
            path: rollout_path.clone(),
            started_at: normalize_time(created_at.to_string()),
            updated_at: normalize_time(updated_at.to_string()),
            message_count: 0,
            archived,
            preview: String::new(),
            snippet,
            message_previews: Vec::new(),
            tokens: None,
            model: None,
            daily_tokens: None,
        })
    })?;
    Ok(session)
}

fn scan_opencode_db(path: &Path) -> Result<Vec<AppSession>, Box<dyn Error>> {
    // 0-byte / freshly-created opencode.db has no tables yet — OpenCode
    // creates the file on launch but only writes the schema on first
    // session. Treat this as "no sessions" rather than a hard error so
    // the rest of the scan (Codex / Claude / Gemini / memories) doesn't
    // get aborted by the `?` further down.
    if fs::metadata(path).map(|m| m.len() == 0).unwrap_or(false) {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if !table_exists(&conn, "session")? {
        return Ok(Vec::new());
    }
    ensure_opencode_schema(&conn)?;
    let token_cols = opencode_token_columns(&conn)?;
    // Include archived sessions (`time_archived` not filtered) — surfaced
    // with an "Archived" badge. `time_archived` is selected (after the
    // dynamic token columns) so the badge can be set per row. Sub-agent
    // children (`parent_id` set) are still excluded.
    let mut stmt = conn.prepare(&format!(
        "select id, directory, title, time_created, time_updated{}, time_archived \
         from session \
         where parent_id is null \
         order by time_updated desc, id desc",
        token_cols.select_suffix()
    ))?;
    // `time_archived` is the last column: 5 fixed + token cols + model? + 1.
    let archived_col = token_cols.model_col_index() + usize::from(token_cols.has_model);
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let project: String = row.get(1)?;
        let title: String = row.get(2)?;
        let created_raw: i64 = row.get(3)?;
        let updated_raw: i64 = row.get(4)?;
        let archived: bool = row
            .get::<_, Option<i64>>(archived_col)
            .ok()
            .flatten()
            .is_some();
        let tokens = token_cols.read_from_row(row, 5);
        let model_raw: Option<String> = if token_cols.has_model {
            row.get(token_cols.model_col_index()).ok()
        } else {
            None
        };
        let model = model_raw.and_then(parse_opencode_model_json);
        let message_count = count_opencode_visible_messages(&conn, &id).unwrap_or(0);
        let daily_tokens = extract_opencode_daily_tokens(&conn, &id);
        // OpenCode's `session.title` is an auto-summarized prompt
        // (matches what the user sees in their TUI); reusing it as
        // the snippet keeps the card informative without a second
        // query per session.
        let snippet = snippet_line(&opencode_display_title(&title));
        Ok(AppSession {
            id: id.clone(),
            source: "OpenCode".to_string(),
            title: opencode_display_title(&title),
            project,
            path: path.display().to_string(),
            started_at: normalize_time(created_raw.to_string()),
            updated_at: normalize_time(updated_raw.to_string()),
            message_count,
            archived,
            preview: String::new(),
            snippet,
            message_previews: Vec::new(),
            tokens,
            model,
            daily_tokens,
        })
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(row?);
    }
    Ok(sessions)
}

fn parse_opencode_db_session(path: &Path, id: &str) -> Result<SessionDetail, Box<dyn Error>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    ensure_opencode_schema(&conn)?;
    let session = opencode_session_from_db(&conn, path, id)?
        .ok_or_else(|| format!("OpenCode session not found: {id}"))?;
    let messages = read_opencode_db_messages(&conn, id)?;
    let mut detail = SessionDetail { session, messages };
    detail.session.message_count = detail.messages.len();
    Ok(detail)
}

fn ensure_opencode_schema(conn: &Connection) -> Result<(), Box<dyn Error>> {
    first_existing_table(conn, &["session"])?;
    if table_exists(conn, "session_message")? {
        return Ok(());
    }
    first_existing_table(conn, &["message"])?;
    first_existing_table(conn, &["part"])?;
    Ok(())
}

fn opencode_session_from_db(
    conn: &Connection,
    path: &Path,
    id: &str,
) -> Result<Option<AppSession>, Box<dyn Error>> {
    let token_cols = opencode_token_columns(conn)?;
    // Archived sessions load too (badge stays accurate in the detail
    // header) — the `time_archived is null` filter was only for the list.
    let archived_col = token_cols.model_col_index() + usize::from(token_cols.has_model);
    let session = conn
        .query_row(
            &format!(
                "select id, directory, title, time_created, time_updated{}, time_archived \
                 from session \
                 where id = ?1",
                token_cols.select_suffix()
            ),
            [id],
            |row| {
                let id: String = row.get(0)?;
                let project: String = row.get(1)?;
                let title: String = row.get(2)?;
                let created_raw: i64 = row.get(3)?;
                let updated_raw: i64 = row.get(4)?;
                let archived: bool = row
                    .get::<_, Option<i64>>(archived_col)
                    .ok()
                    .flatten()
                    .is_some();
                let tokens = token_cols.read_from_row(row, 5);
                let model_raw: Option<String> = if token_cols.has_model {
                    row.get(token_cols.model_col_index()).ok()
                } else {
                    None
                };
                let model = model_raw.and_then(parse_opencode_model_json);
                let snippet = snippet_line(&opencode_display_title(&title));
                Ok(AppSession {
                    id: id.clone(),
                    source: "OpenCode".to_string(),
                    title: opencode_display_title(&title),
                    project,
                    path: path.display().to_string(),
                    started_at: normalize_time(created_raw.to_string()),
                    updated_at: normalize_time(updated_raw.to_string()),
                    message_count: 0,
                    archived,
                    preview: String::new(),
                    snippet,
                    message_previews: Vec::new(),
                    tokens,
                    model,
                    // Filled in below — the closure can't easily run a
                    // second query against `conn` (already borrowed for
                    // this row), so defer to outside the query_row.
                    daily_tokens: None,
                })
            },
        )
        .optional()?;
    let session = session.map(|mut s| {
        s.daily_tokens = extract_opencode_daily_tokens(conn, id);
        s
    });
    Ok(session)
}

/// Detect which token / model columns exist on the `session` table.
/// OpenCode schemas have evolved — older databases may not have the
/// reasoning / cache_read / cache_write breakdown, and even older
/// ones may not have the `model` column. Probe once and reuse.
#[derive(Default, Clone, Copy)]
struct OpencodeTokenColumns {
    has_input: bool,
    has_output: bool,
    has_reasoning: bool,
    has_cache_read: bool,
    has_cache_write: bool,
    has_model: bool,
}

impl OpencodeTokenColumns {
    /// Trailing fragment for the `select` list — empty when none of
    /// the optional columns are present.
    fn select_suffix(self) -> String {
        let mut out = String::new();
        if self.has_input {
            out.push_str(", tokens_input");
        }
        if self.has_output {
            out.push_str(", tokens_output");
        }
        if self.has_reasoning {
            out.push_str(", tokens_reasoning");
        }
        if self.has_cache_read {
            out.push_str(", tokens_cache_read");
        }
        if self.has_cache_write {
            out.push_str(", tokens_cache_write");
        }
        if self.has_model {
            out.push_str(", model");
        }
        out
    }

    fn token_count(self) -> usize {
        [
            self.has_input,
            self.has_output,
            self.has_reasoning,
            self.has_cache_read,
            self.has_cache_write,
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    fn model_col_index(self) -> usize {
        // 5 = the 5 fixed columns (id, directory, title, time_created,
        // time_updated). Token columns come next, then model.
        5 + self.token_count()
    }

    /// Read whichever token columns we know exist, starting at the
    /// given base index. Returns `None` when every column is missing
    /// OR every present column is zero — so we don't surface a
    /// "0 input / 0 output" total that the user would assume means
    /// "no data" anyway.
    fn read_from_row(self, row: &rusqlite::Row<'_>, base: usize) -> Option<TokenStats> {
        let mut idx = base;
        let mut input = 0u64;
        let mut output = 0u64;
        let mut reasoning = 0u64;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;
        if self.has_input {
            input = row
                .get::<_, i64>(idx)
                .ok()
                .map(|v| v.max(0) as u64)
                .unwrap_or(0);
            idx += 1;
        }
        if self.has_output {
            output = row
                .get::<_, i64>(idx)
                .ok()
                .map(|v| v.max(0) as u64)
                .unwrap_or(0);
            idx += 1;
        }
        if self.has_reasoning {
            reasoning = row
                .get::<_, i64>(idx)
                .ok()
                .map(|v| v.max(0) as u64)
                .unwrap_or(0);
            idx += 1;
        }
        if self.has_cache_read {
            cache_read = row
                .get::<_, i64>(idx)
                .ok()
                .map(|v| v.max(0) as u64)
                .unwrap_or(0);
            idx += 1;
        }
        if self.has_cache_write {
            cache_write = row
                .get::<_, i64>(idx)
                .ok()
                .map(|v| v.max(0) as u64)
                .unwrap_or(0);
        }
        let cached = cache_read + cache_write;
        let total = input + output + reasoning + cached;
        if total == 0 {
            return None;
        }
        Some(TokenStats {
            input,
            output,
            cached,
            reasoning,
            total,
        })
    }
}

fn opencode_token_columns(conn: &Connection) -> Result<OpencodeTokenColumns, Box<dyn Error>> {
    Ok(OpencodeTokenColumns {
        has_input: column_exists(conn, "session", "tokens_input")?,
        has_output: column_exists(conn, "session", "tokens_output")?,
        has_reasoning: column_exists(conn, "session", "tokens_reasoning")?,
        has_cache_read: column_exists(conn, "session", "tokens_cache_read")?,
        has_cache_write: column_exists(conn, "session", "tokens_cache_write")?,
        has_model: column_exists(conn, "session", "model")?,
    })
}

/// `session.model` is a JSON column with the shape
/// `{"id":"...", "providerID":"...", "variant"?: "..."}`. Pull just
/// the id so the UI displays "gpt-5" / "claude-opus-4-7" / etc.
fn parse_opencode_model_json(raw: String) -> Option<String> {
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed.get("id").and_then(Value::as_str).map(str::to_string)
}

fn count_opencode_visible_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<usize, Box<dyn Error>> {
    Ok(read_opencode_db_messages(conn, session_id)?.len())
}

/// Pull per-day token spend out of OpenCode's `part` table by walking
/// every `step-finish` part for the session. Each step-finish record
/// is one upstream API call and carries:
///   - `time.created`: ms since epoch (also mirrored as `time_created`
///     column on the row)
///   - `tokens.{input, output, reasoning, cache.{read, write}}`: the
///     exact same structure the session-level aggregates roll up from
///     (per `session/projectors.ts:36-40`)
///
/// Returns `None` when no step-finish part was seen (empty session,
/// older schema, etc.) so the caller falls back to even-distribution.
fn extract_opencode_daily_tokens(
    conn: &Connection,
    session_id: &str,
) -> Option<Vec<DailyTokenBreakdown>> {
    if !table_exists(conn, "part").ok()? {
        return None;
    }
    let mut stmt = conn
        .prepare(
            "select time_created, data from part \
             where session_id = ?1 \
             order by time_created asc, id asc",
        )
        .ok()?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;

    // [input, output, cached, reasoning, total, messages]
    let mut daily: BTreeMap<String, [u64; 6]> = BTreeMap::new();
    let mut daily_hours: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    let mut daily_hour_tokens: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    let mut saw_any = false;
    for row in rows {
        let Ok((time_ms_col, data)) = row else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        // Only step-finish parts carry token usage — text/tool/etc.
        // parts have no tokens field.
        if value.get("type").and_then(Value::as_str) != Some("step-finish") {
            continue;
        }
        let Some(tokens) = value.get("tokens") else {
            continue;
        };
        let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
        let output = tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
        let reasoning = tokens.get("reasoning").and_then(Value::as_u64).unwrap_or(0);
        let cache = tokens.get("cache");
        let cache_read = cache
            .and_then(|c| c.get("read"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_write = cache
            .and_then(|c| c.get("write"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = cache_read + cache_write;
        if input == 0 && output == 0 && reasoning == 0 && cached == 0 {
            continue;
        }
        saw_any = true;

        // Prefer the embedded `time.created`; fall back to the
        // `time_created` column when the JSON shape varies across
        // OpenCode versions.
        let time_ms = value
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(time_ms_col);
        let Some((date, hour)) = epoch_ms_to_local_date_hour(time_ms) else {
            continue;
        };
        let bucket = daily.entry(date.clone()).or_insert([0u64; 6]);
        bucket[0] += input;
        bucket[1] += output;
        bucket[2] += cached;
        bucket[3] += reasoning;
        let part_total = input + output + cached + reasoning;
        bucket[4] += part_total;
        bucket[5] += 1; // one step-finish = 1 API call
        let hourbox = daily_hours.entry(date.clone()).or_insert([0u64; 24]);
        hourbox[hour as usize] += 1;
        let tokbox = daily_hour_tokens.entry(date).or_insert([0u64; 24]);
        tokbox[hour as usize] += part_total;
    }
    if !saw_any {
        return None;
    }
    Some(
        daily
            .into_iter()
            .map(|(date, [i, o, c, r, t, m])| {
                let hours = daily_hours
                    .get(&date)
                    .map(|a| a.to_vec())
                    .unwrap_or_default();
                let hour_tokens = daily_hour_tokens
                    .get(&date)
                    .map(|a| a.to_vec())
                    .unwrap_or_default();
                DailyTokenBreakdown {
                    date,
                    tokens: TokenStats {
                        input: i,
                        output: o,
                        cached: c,
                        reasoning: r,
                        total: t,
                    },
                    messages: m,
                    hours,
                    hour_tokens,
                }
            })
            .collect(),
    )
}

fn read_opencode_db_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionMessage>, Box<dyn Error>> {
    if !table_exists(conn, "message")? || !table_exists(conn, "part")? {
        if table_exists(conn, "session_message")? {
            return read_opencode_v2_db_messages(conn, session_id);
        }
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "select id, time_created, data \
         from message \
         where session_id = ?1 \
         order by time_created asc, id asc",
    )?;
    let message_rows: Vec<(String, i64, String)> = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let mut out = Vec::new();
    for (message_id, created, data) in message_rows {
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let Some(role) = value.get("role").and_then(value_to_string) else {
            continue;
        };
        if role != "user" && role != "assistant" {
            continue;
        }
        let timestamp = value
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(value_to_string)
            .and_then(normalize_time)
            .or_else(|| normalize_time(created.to_string()));

        // Emit one SessionMessage per relevant part so the TUI-style tool
        // labels (Bash, Update, Read, ...) and per-part timestamps show
        // through. Matches transcript.ts formatPart's part-by-part loop.
        let mut part_stmt = conn.prepare(
            "select data from part \
             where session_id = ?1 and message_id = ?2 \
             order by time_created asc, id asc",
        )?;
        let part_rows: Vec<String> = part_stmt
            .query_map((session_id, &message_id), |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        for data in part_rows {
            let Ok(part) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
            match part_type {
                "text" => {
                    // OpenCode TUI hides `synthetic: true` text parts
                    // (env / tool-ack injections). Termory surfaces them
                    // as `*[synthetic]* {body}` so the transcript shows
                    // every part recorded in the database.
                    let synthetic = part.get("synthetic").and_then(Value::as_bool) == Some(true);
                    let Some(text) = part.get("text").and_then(value_to_string) else {
                        continue;
                    };
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let display = if synthetic {
                        format!("*[synthetic]* {trimmed}")
                    } else {
                        trimmed.to_string()
                    };
                    out.push(SessionMessage {
                        role: role.clone(),
                        text: display,
                        timestamp: timestamp.clone(),
                        kind: kind::TEXT.to_string(),
                        tool_use_id: None,
                        exit_code: None,
                    });
                }
                "reasoning" => {
                    let Some(text) = part.get("text").and_then(value_to_string) else {
                        continue;
                    };
                    let cleaned = text.replace("[REDACTED]", "");
                    let formatted = format_reasoning_body(&cleaned);
                    if formatted.is_empty() {
                        continue;
                    }
                    out.push(SessionMessage {
                        role: "assistant".to_string(),
                        text: formatted,
                        timestamp: timestamp.clone(),
                        kind: kind::REASONING.to_string(),
                        tool_use_id: None,
                        exit_code: None,
                    });
                }
                "tool" => {
                    let Some(body) = opencode_v2_tool_part_text(&part) else {
                        continue;
                    };
                    // Per session-v2.tsx:572 + l.669, a tool state of
                    // `"error"` flips the InlineTool / BlockTool to the
                    // error color and surfaces `state.error.message`.
                    // Termory reflects this through `kind` so future UI
                    // can colour the card uniformly across platforms.
                    let failed = part
                        .get("state")
                        .and_then(|s| s.get("status"))
                        .and_then(Value::as_str)
                        == Some("error");
                    out.push(SessionMessage {
                        role: "tool".to_string(),
                        text: body,
                        timestamp: timestamp.clone(),
                        kind: if failed {
                            kind::TOOL_ERROR.to_string()
                        } else {
                            kind::TOOL_USE.to_string()
                        },
                        tool_use_id: part.get("callID").and_then(value_to_string),
                        exit_code: None,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

fn read_opencode_v2_db_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionMessage>, Box<dyn Error>> {
    let mut stmt = conn.prepare(
        "select id, type, time_created, data \
         from session_message \
         where session_id = ?1 \
         order by time_created asc, id asc",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut messages = Vec::new();
    for row in rows {
        let (id, message_type, created, data) = row?;
        let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let Some(object) = value.as_object_mut() {
            object.insert("id".to_string(), Value::String(id));
            object.insert("type".to_string(), Value::String(message_type.clone()));
        }
        if let Some(message) = opencode_v2_message_from_value(&value, created) {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn opencode_v2_message_from_value(value: &Value, created: i64) -> Option<SessionMessage> {
    let message_type = value.get("type").and_then(Value::as_str)?;
    let timestamp = value
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(value_to_string)
        .and_then(normalize_time)
        .or_else(|| normalize_time(created.to_string()));
    match message_type {
        "user" => {
            let text = value.get("text").and_then(value_to_string)?;
            // Per session-v2.tsx:176-194, UserMessage renders file +
            // agent attachments as colored pills beneath the prompt
            // body (references are NOT rendered — see the JSX). Termory
            // surfaces them as `` `kind` `name` `` code-span pairs on a
            // line below the text so the markdown reader sees a
            // visually-distinct row.
            let mut body = text.trim().to_string();
            let attachments = opencode_v2_user_attachments(value);
            if !attachments.is_empty() {
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(&attachments);
            }
            Some(SessionMessage {
                role: "user".to_string(),
                text: body,
                timestamp,
                kind: kind::TEXT.to_string(),
                tool_use_id: None,
                exit_code: None,
            })
        }
        "assistant" => {
            let mut text = opencode_v2_assistant_text(value).unwrap_or_default();
            // Per session-v2.tsx:339-353, AssistantMessage shows an
            // error box (red border + error message) when `error` is
            // set. Surface as an italic error notice on its own line.
            if let Some(error_message) = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(value_to_string)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!("*✕ {error_message}*"));
            }
            if text.is_empty() {
                return None;
            }
            Some(SessionMessage {
                role: "assistant".to_string(),
                text,
                timestamp,
                kind: kind::TEXT.to_string(),
                tool_use_id: None,
                exit_code: None,
            })
        }
        // session-v2.tsx:105-107 — OpenCode TUI hides synthetic
        // messages; Termory surfaces them as `*[synthetic]*` notices
        // so the transcript shows every recorded part. The full body
        // is included when present so the user can see what was
        // injected (env, tool acks, etc.).
        "synthetic" => {
            let body = value
                .get("content")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let text = match body {
                Some(b) => format!("*[synthetic]*\n\n{b}"),
                None => "*[synthetic]*".to_string(),
            };
            Some(SessionMessage {
                role: "system".to_string(),
                text,
                timestamp,
                kind: kind::TEXT.to_string(),
                tool_use_id: None,
                exit_code: None,
            })
        }
        "shell" => {
            let command = value.get("command").and_then(value_to_string)?;
            let mut lines = vec![format!("$ {command}")];
            if let Some(output) = value.get("output").and_then(value_to_string) {
                // session-v2.tsx:203 ShellMessage applies `stripAnsi` to
                // the output before rendering. Match here so terminal
                // colour codes don't leak into the markdown viewer.
                let stripped = strip_ansi(&output);
                let output = stripped.trim();
                if !output.is_empty() {
                    lines.push(output.to_string());
                }
            }
            Some(SessionMessage {
                role: "tool".to_string(),
                text: lines.join("\n"),
                timestamp,
                kind: kind::SHELL.to_string(),
                tool_use_id: None,
                exit_code: None,
            })
        }
        "compaction" => value
            .get("summary")
            .and_then(value_to_string)
            .map(|summary| {
                // session-v2.tsx:237 — title is "Auto Compaction" when
                // `reason === "auto"`, else just "Compaction". Surface the
                // reason in a bold header so users can tell auto-triggered
                // compactions apart from manual `/compact` invocations.
                let title = if value.get("reason").and_then(Value::as_str) == Some("auto") {
                    "Auto Compaction"
                } else {
                    "Compaction"
                };
                SessionMessage {
                    role: "system".to_string(),
                    text: format!("**{title}**\n\n{}", summary.trim()),
                    timestamp,
                    kind: kind::COMPACTION.to_string(),
                    tool_use_id: None,
                    exit_code: None,
                }
            }),
        "agent-switched" => {
            value
                .get("agent")
                .and_then(value_to_string)
                .map(|agent| SessionMessage {
                    role: "system".to_string(),
                    // session-v2.tsx:267 prepends `▣ ` in agent color.
                    text: format!("▣ Switched agent to {}", title_case(&agent)),
                    timestamp,
                    kind: kind::AGENT_SWITCHED.to_string(),
                    tool_use_id: None,
                    exit_code: None,
                })
        }
        "model-switched" => value.get("model").map(|model| {
            let provider = model
                .get("providerID")
                .and_then(value_to_string)
                .unwrap_or_default();
            let id = model
                .get("id")
                .and_then(value_to_string)
                .unwrap_or_default();
            let variant = model.get("variant").and_then(value_to_string);
            let mut label = format!("{provider}/{id}");
            if let Some(variant) = variant {
                if !variant.is_empty() && variant != "default" {
                    label.push('/');
                    label.push_str(&variant);
                }
            }
            SessionMessage {
                role: "system".to_string(),
                // session-v2.tsx:284 prepends `◇ ` in secondary color.
                text: format!("◇ Switched model to {label}"),
                timestamp,
                kind: kind::MODEL_SWITCHED.to_string(),
                tool_use_id: None,
                exit_code: None,
            }
        }),
        _ => None,
    }
}

/// Format the user-message attachment row as code-span pills, per
/// session-v2.tsx:176-194. Renders one inline-code "kind" tag + one
/// inline-code "name" tag per attachment, separated by spaces:
///
/// ```text
/// `image/png` `screenshot.png` `agent` `code-reviewer`
/// ```
///
/// `references` (PromptReferenceAttachment) are persisted but NOT
/// rendered by the TUI — we mirror that and ignore them.
fn opencode_v2_user_attachments(value: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(files) = value.get("files").and_then(Value::as_array) {
        for file in files {
            let mime = file.get("mime").and_then(Value::as_str).unwrap_or("");
            let name = file
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| file.get("uri").and_then(Value::as_str))
                .unwrap_or("");
            if mime.is_empty() && name.is_empty() {
                continue;
            }
            if !mime.is_empty() {
                parts.push(wrap_inline_code(mime));
            }
            if !name.is_empty() {
                parts.push(wrap_inline_code(name));
            }
        }
    }
    if let Some(agents) = value.get("agents").and_then(Value::as_array) {
        for agent in agents {
            let name = agent.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            parts.push(wrap_inline_code("agent"));
            parts.push(wrap_inline_code(name));
        }
    }
    parts.join(" ")
}

fn opencode_v2_assistant_text(value: &Value) -> Option<String> {
    let content = value.get("content").and_then(Value::as_array)?;
    let parts = content
        .iter()
        .filter_map(opencode_v2_assistant_part_text)
        .collect::<Vec<_>>();
    let text = parts.join("\n\n").trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn opencode_v2_assistant_part_text(part: &Value) -> Option<String> {
    match part.get("type").and_then(Value::as_str)? {
        "text" => part.get("text").and_then(value_to_string),
        "reasoning" => part
            .get("text")
            .and_then(value_to_string)
            .map(|text| format_reasoning_body(&text.replace("[REDACTED]", "")))
            .filter(|s| !s.is_empty()),
        "tool" => opencode_v2_tool_part_text(part),
        _ => None,
    }
}

// OpenCode tool rendering — unified `**Verb**(args)` format across the
// four platforms. The verb (Bash / Read / Edit / etc.) still comes
// from OpenCode's TUI source (session-v2.tsx referenced per-branch),
// but the layout is the Termory unified shape:
//   * header → `**Verb**({wrap_inline_code(arg)})`
//   * body   → 4-backtick fence with output / content / diff (when present)
// The OpenCode-specific decorations (`\# Title` H1 escape, `←` arrow,
// `↳ Loaded`, `"pattern" in path` quoted format) are dropped in favor
// of the unified shape so all four platforms read consistently.
fn opencode_v2_tool_part_text(part: &Value) -> Option<String> {
    let card = opencode_v2_tool_part_card(part)?;
    let state = part.get("state");
    let failed = state.and_then(|s| s.get("status")).and_then(Value::as_str) == Some("error");
    let error_message = if failed {
        state
            .and_then(|s| s.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(value_to_string)
    } else {
        None
    };
    let marker = status_marker(failed);
    let mut out = format!("{marker}{card}");
    // Per session-v2.tsx:669-702, the error text is rendered separately
    // beneath the tool card in `theme.error`. Wrap in a 4-backtick fence
    // with the `Error: {message}` prefix used by Codex / Claude / Gemini.
    if let Some(message) = error_message.as_deref().map(str::trim) {
        if !message.is_empty() {
            out.push_str("\n\n````\nError: ");
            out.push_str(message);
            out.push_str("\n````");
        }
    } else if failed {
        // Failure with no message — still surface "Error" so the user
        // sees the failure indicator.
        out.push_str("\n\n````\nError\n````");
    }
    Some(out)
}

fn opencode_v2_tool_part_card(part: &Value) -> Option<String> {
    let name = part
        .get("name")
        .and_then(value_to_string)
        .or_else(|| part.get("tool").and_then(value_to_string))?;
    let state = part.get("state")?;
    let input = state.get("input").unwrap_or(&Value::Null);
    let input_record = input.as_object();
    let metadata = part
        .get("provider")
        .and_then(|provider| provider.get("metadata"))
        .or_else(|| state.get("metadata"))
        .or_else(|| part.get("metadata"));
    let output = state
        .get("content")
        .and_then(opencode_v2_tool_output)
        .or_else(|| state.get("output").and_then(value_to_string))
        .or_else(|| {
            metadata
                .and_then(|metadata| metadata.get("output"))
                .and_then(value_to_string)
        })
        .unwrap_or_default();
    // Unified block builder: header `**Verb**(args)` + (optional) fenced
    // body. The 4-backtick fence matches `merge_tool_outputs` so the
    // OpenCode parts (which arrive already combined) look identical to
    // Codex / Claude merged tool cards.
    let unified = |verb: &str, args: &str, body: &str, lang: &str| -> String {
        let header = if args.is_empty() {
            format!("**{verb}**")
        } else {
            format!("**{verb}**({args})")
        };
        let trimmed = body.trim();
        if trimmed.is_empty() {
            header
        } else if lang.is_empty() {
            format!("{header}\n\n````\n{trimmed}\n````")
        } else {
            format!("{header}\n\n```{lang}\n{trimmed}\n```")
        }
    };
    // session-v2.tsx:707 Bash — header `**Shell**(\`cmd\`)` (matches the
    // original OpenCode default title `\# Shell`); body keeps the
    // description title + bash fence with `$ cmd` prefix unchanged.
    if name == "bash" || name == "shell" {
        let command = input_record
            .and_then(|record| record.get("command"))
            .and_then(value_to_string)
            .or_else(|| input.as_str().map(ToString::to_string))
            .unwrap_or_default();
        let description = input_record
            .and_then(|record| record.get("description"))
            .and_then(value_to_string);
        let args = if command.is_empty() {
            String::new()
        } else {
            wrap_inline_code(command.trim())
        };
        let header = if args.is_empty() {
            "**Shell**".to_string()
        } else {
            format!("**Shell**({args})")
        };
        // Per session-v2.tsx:710 Bash prefers `metadata.output` (the
        // already-formatted shell capture) over `state.content` (the
        // generic toolOutput projection). Resolve in that order and
        // strip ANSI escapes — `stripAnsi` matches the TUI's behavior.
        let bash_output_raw = metadata
            .and_then(|m| m.get("output"))
            .and_then(value_to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| output.clone());
        let bash_output = strip_ansi(&bash_output_raw);
        let trimmed_output = bash_output.trim();
        if trimmed_output.is_empty() {
            return Some(header);
        }
        let title_text = description
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or("Shell");
        let title = format!("\\# {title_text}");
        let body = format!("$ {command}\n{trimmed_output}");
        return Some(format!("{header}\n\n{title}\n\n```bash\n{body}\n```"));
    }
    // session-v2.tsx:748 Glob
    if name == "glob" {
        let pattern = input_string(input_record, "pattern")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        let mut args = format!("pattern: {}", wrap_inline_code(&pattern));
        if let Some(path) = input_string(input_record, "path") {
            args.push_str(&format!(", path: {}", wrap_inline_code(&path)));
        }
        if let Some(count) = metadata
            .and_then(|m| m.get("count"))
            .and_then(value_to_string)
        {
            let plural = if count == "1" { "match" } else { "matches" };
            args.push_str(&format!(" — {count} {plural}"));
        }
        return Some(format!("**Glob**({args})"));
    }
    // session-v2.tsx:764 Read
    if name == "read" {
        let file_path = input_string(input_record, "filePath")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        let other = input_record
            .map(|r| opencode_v2_input_other(r, &["filePath"]))
            .unwrap_or_default();
        let args = if other.is_empty() {
            wrap_inline_code(&file_path)
        } else {
            format!("{} {other}", wrap_inline_code(&file_path))
        };
        let mut text = format!("**Read**({args})");
        // Restore `metadata.loaded` (was `↳ Loaded {path}` per entry before
        // the unified-format refactor stripped it). Each entry on its own
        // line — trailing `\` is a CommonMark hard line break so the lines
        // don't collapse into a paragraph.
        if let Some(loaded) = metadata
            .and_then(|m| m.get("loaded"))
            .and_then(Value::as_array)
        {
            let paths: Vec<String> = loaded.iter().filter_map(value_to_string).collect();
            if !paths.is_empty() {
                text.push('\\');
                for path in &paths {
                    text.push_str("\n↳ Loaded ");
                    text.push_str(path);
                    text.push('\\');
                }
                // Drop the trailing `\` on the last line so it doesn't
                // produce an empty `<br>` after the final entry.
                if text.ends_with('\\') {
                    text.pop();
                }
            }
        }
        return Some(text);
    }
    // session-v2.tsx:794 Grep
    if name == "grep" {
        let pattern = input_string(input_record, "pattern")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        let mut args = format!("pattern: {}", wrap_inline_code(&pattern));
        if let Some(path) = input_string(input_record, "path") {
            args.push_str(&format!(", path: {}", wrap_inline_code(&path)));
        }
        if let Some(matches) = metadata
            .and_then(|m| m.get("matches"))
            .and_then(value_to_string)
        {
            let plural = if matches == "1" { "match" } else { "matches" };
            args.push_str(&format!(" — {matches} {plural}"));
        }
        return Some(format!("**Grep**({args})"));
    }
    // session-v2.tsx:810 WebFetch
    if name == "webfetch" {
        let url = input_string(input_record, "url")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        return Some(format!("**WebFetch**({})", wrap_inline_code(&url)));
    }
    // session-v2.tsx:818 WebSearch — verb is provider-derived label per
    // `webSearchProviderLabel` in `packages/opencode/src/tool/websearch.ts:39-43`:
    //   * `"parallel"` → "Parallel Web Search"
    //   * `"exa"`      → "Exa Web Search"
    //   * (default)    → "Web Search"
    if name == "websearch" {
        let query = input_string(input_record, "query")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        let verb = match metadata
            .and_then(|m| m.get("provider"))
            .and_then(Value::as_str)
        {
            Some("parallel") => "Parallel Web Search",
            Some("exa") => "Exa Web Search",
            _ => "Web Search",
        };
        let mut args = wrap_inline_code(&query);
        if let Some(results) = metadata
            .and_then(|m| m.get("numResults"))
            .and_then(value_to_string)
        {
            args.push_str(&format!(" — {results} results"));
        }
        return Some(format!("**{verb}**({args})"));
    }
    // session-v2.tsx:828 Write
    if name == "write" {
        let file_path = input_string(input_record, "filePath").unwrap_or_default();
        let args = if file_path.is_empty() {
            String::new()
        } else {
            wrap_inline_code(&file_path)
        };
        if state.get("status").and_then(Value::as_str) == Some("completed") {
            if let Some(content) = input_string(input_record, "content") {
                let lang = filetype_hint(&file_path);
                return Some(unified("Write", &args, &content, lang));
            }
        }
        let header = if args.is_empty() {
            "**Write**".to_string()
        } else {
            format!("**Write**({args})")
        };
        return Some(header);
    }
    // session-v2.tsx:857 Edit
    if name == "edit" {
        let file_path = input_string(input_record, "filePath").unwrap_or_default();
        let args = if file_path.is_empty() {
            String::new()
        } else {
            wrap_inline_code(&file_path)
        };
        if let Some(diff) = metadata
            .and_then(|m| m.get("diff"))
            .and_then(value_to_string)
        {
            return Some(unified("Edit", &args, &diff, "diff"));
        }
        let replace_all = input_record
            .and_then(|r| r.get("replaceAll"))
            .and_then(value_to_string);
        let header = if args.is_empty() {
            "**Edit**".to_string()
        } else {
            format!("**Edit**({args})")
        };
        return Some(match replace_all {
            Some(value) => format!("{header} [replaceAll={value}]"),
            None => header,
        });
    }
    // session-v2.tsx:891 ApplyPatch
    if name == "apply_patch" {
        if let Some(files) = metadata
            .and_then(|m| m.get("files"))
            .and_then(Value::as_array)
        {
            let blocks = files
                .iter()
                .filter_map(|file| {
                    let (verb, path) = opencode_v2_patch_file_verb_path(file)?;
                    let body = file
                        .get("patch")
                        .and_then(value_to_string)
                        .or_else(|| {
                            // Per session-v2.tsx:923-924, fall back to the
                            // deletion count. Pluralize TUI-style: `-1 line`,
                            // `-N lines`.
                            let n = file.get("deletions").and_then(Value::as_i64)?;
                            let unit = if n == 1 { "line" } else { "lines" };
                            Some(format!("-{n} {unit}"))
                        })
                        .unwrap_or_default();
                    Some(unified(verb, &wrap_inline_code(&path), &body, "diff"))
                })
                .collect::<Vec<_>>();
            if !blocks.is_empty() {
                return Some(blocks.join("\n\n"));
            }
        }
        return Some("**ApplyPatch**".to_string());
    }
    // session-v2.tsx:964 TodoWrite — verb is "Todos" (matches the original
    // BlockTool title `\# Todos`).
    if name == "todowrite" {
        let metadata_has_todos = metadata
            .and_then(|m| m.get("todos"))
            .and_then(Value::as_array)
            .is_some_and(|todos| !todos.is_empty());
        let completed = state.get("status").and_then(Value::as_str) == Some("completed");
        if let Some(todos) = input_record
            .and_then(|r| r.get("todos"))
            .and_then(Value::as_array)
        {
            if (metadata_has_todos || completed) && !todos.is_empty() {
                let lines = todos
                    .iter()
                    .filter_map(|todo| {
                        let status = todo
                            .get("status")
                            .and_then(value_to_string)
                            .unwrap_or_default();
                        let content = todo.get("content").and_then(value_to_string)?;
                        Some(format!("{} {content}", todo_icon(&status)))
                    })
                    .collect::<Vec<_>>();
                if !lines.is_empty() {
                    return Some(format!("**Todos**\n\n{}", lines.join("\n")));
                }
            }
        }
        return Some("**Todos**".to_string());
    }
    // session-v2.tsx:991 Question — verb is "Questions" (matches the
    // original BlockTool title `\# Questions`).
    if name == "question" {
        let questions = input_record
            .and_then(|r| r.get("questions"))
            .and_then(Value::as_array);
        let answers = metadata
            .and_then(|m| m.get("answers"))
            .and_then(Value::as_array);
        if let (Some(questions), Some(answers)) = (questions, answers) {
            if !answers.is_empty() {
                let lines = questions
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, question)| {
                        let question = question.get("question").and_then(value_to_string)?;
                        let answer = answers.get(idx).map(format_answer).unwrap_or_default();
                        Some(format!("{question}\n{answer}"))
                    })
                    .collect::<Vec<_>>();
                if !lines.is_empty() {
                    return Some(format!("**Questions**\n\n{}", lines.join("\n\n")));
                }
            }
        }
        let count = questions.map(|q| q.len()).unwrap_or(0);
        let plural = if count == 1 { "question" } else { "questions" };
        return Some(format!("**Questions**({count} {plural})"));
    }
    // session-v2.tsx:1022 Skill
    if name == "skill" {
        let skill = input_string(input_record, "name")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        return Some(format!("**Skill**({})", wrap_inline_code(&skill)));
    }
    // session-v2.tsx:1030 Task — verb is `{Titlecase(agent)} Task`
    // (matches the original `{Agent} Task — description` heading where
    // the agent name prefixes "Task").
    if name == "task" {
        let description = input_string(input_record, "description")
            .or_else(|| input.as_str().map(ToString::to_string))?;
        let agent =
            input_string(input_record, "subagent_type").unwrap_or_else(|| "General".to_string());
        return Some(format!(
            "**{} Task**({})",
            title_case(&agent),
            wrap_inline_code(&description)
        ));
    }
    // session-v2.tsx:522 GenericTool fallback
    let input_text = input_record.map(opencode_v2_tool_input).unwrap_or_default();
    let header = if input_text.is_empty() {
        format!("**{name}**")
    } else {
        format!("**{name}**({input_text})")
    };
    if output.trim().is_empty() {
        Some(header)
    } else {
        Some(format!("{header}\n\n````\n{}\n````", output.trim()))
    }
}

// Helper functions ported from TUI conventions.

fn opencode_v2_input_other(input: &serde_json::Map<String, Value>, omit: &[&str]) -> String {
    let pairs = input
        .iter()
        .filter(|(key, _)| !omit.contains(&key.as_str()))
        .filter_map(|(key, value)| match value {
            Value::String(text) => Some(format!("{key}={text}")),
            Value::Number(number) => Some(format!("{key}={number}")),
            Value::Bool(boolean) => Some(format!("{key}={boolean}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        String::new()
    } else {
        format!("[{}]", pairs.join(", "))
    }
}

fn opencode_v2_patch_file_verb_path(file: &Value) -> Option<(&'static str, String)> {
    // session-v2.tsx:905-912 fileTitle() — picks the verb based on the
    // FileChange tag. Returns `(verb, path)` for the unified
    // `**Verb**(args)` header builder.
    let file_type = file.get("type").and_then(value_to_string);
    let relative_path = file
        .get("relativePath")
        .and_then(value_to_string)
        .or_else(|| file.get("filePath").and_then(value_to_string))
        .unwrap_or_else(|| "patch".to_string());
    Some(match file_type.as_deref() {
        Some("delete") => ("Deleted", relative_path),
        Some("add") => ("Created", relative_path),
        Some("move") => {
            let original = file
                .get("filePath")
                .and_then(value_to_string)
                .unwrap_or_default();
            ("Moved", format!("{original} → {relative_path}"))
        }
        _ => ("Patched", relative_path),
    })
}

/// Strip ANSI escape sequences from terminal output. Mirrors the
/// `stripAnsi` calls in OpenCode's session-v2.tsx (Bash output at l.710,
/// shell-message output at l.203). We don't reach for a regex crate
/// because the patterns we need to remove are a small, stable subset:
///
/// * CSI sequences — `ESC [ {params} {final}` where `final` ∈ `@..~`
///   (most colour / cursor codes)
/// * OSC sequences — `ESC ] ... BEL` or `ESC ] ... ESC \`
/// * Single-character escapes — `ESC {letter}` (e.g. `ESC c` reset)
///
/// Anything else is left as-is so we don't accidentally strip user
/// content that happens to contain a `0x1b` byte.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            break;
        };
        match next {
            '[' => {
                // CSI — consume params (0x20-0x3F) then the final byte
                // (0x40-0x7E) which terminates the sequence.
                while let Some(&p) = chars.peek() {
                    if matches!(p, '\u{20}'..='\u{3f}') {
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some(&p) = chars.peek() {
                    if matches!(p, '\u{40}'..='\u{7e}') {
                        chars.next();
                    }
                }
            }
            ']' => {
                // OSC — consume until BEL (0x07) or ESC \\.
                while let Some(p) = chars.next() {
                    if p == '\u{07}' {
                        break;
                    }
                    if p == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Single-character escape — drop the letter and continue.
            _ => {}
        }
    }
    out
}

/// Collapse a multi-line / messy string into a single, trimmed,
/// length-capped line suitable for the list-card snippet row.
/// Returns empty string on empty input so callers can drop it cleanly.
fn snippet_line(raw: &str) -> String {
    const MAX: usize = 120;
    let mut buf = String::new();
    let mut prev_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !buf.is_empty() && !prev_space {
                buf.push(' ');
                prev_space = true;
            }
            continue;
        }
        buf.push(ch);
        prev_space = false;
    }
    let trimmed = buf.trim_end().to_string();
    if trimmed.chars().count() <= MAX {
        return trimmed;
    }
    let mut out: String = trimmed.chars().take(MAX).collect();
    out.push('…');
    out
}

fn todo_icon(status: &str) -> &'static str {
    // Matches packages/opencode/src/cli/cmd/tui/feature-plugins/system/session-v2.tsx
    // (todoIcon helper) — ✓ completed, ~ in_progress, ✕ cancelled, ☐ pending.
    match status {
        "completed" => "✓",
        "in_progress" => "~",
        "cancelled" => "✕",
        _ => "☐",
    }
}

fn filetype_hint(path: &str) -> &'static str {
    // Hint for the markdown renderer's syntax highlighter. Mirrors the
    // intent of the OpenCode TUI `filetype(...)` helper (returns a language
    // id for highlight.js). Conservative subset — unknown extensions fall
    // back to no hint so highlight.js can auto-detect.
    let lower = path.to_ascii_lowercase();
    match lower.rsplit('.').next() {
        Some("rs") => "rust",
        Some("ts" | "tsx") => "typescript",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("py") => "python",
        Some("go") => "go",
        Some("rb") => "ruby",
        Some("sh" | "bash" | "zsh") => "bash",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("json" | "jsonc") => "json",
        Some("md" | "mdx") => "markdown",
        Some("html" | "htm") => "html",
        Some("css") => "css",
        Some("scss" | "sass") => "scss",
        Some("c" | "h") => "c",
        Some("cpp" | "cc" | "hpp") => "cpp",
        Some("java") => "java",
        Some("kt") => "kotlin",
        Some("swift") => "swift",
        Some("sql") => "sql",
        Some("dockerfile") => "dockerfile",
        _ => "",
    }
}

fn input_string(input: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<String> {
    input?.get(key).and_then(value_to_string)
}

fn format_answer(value: &Value) -> String {
    let Some(items) = value.as_array() else {
        return "(no answer)".to_string();
    };
    let answers = items.iter().filter_map(value_to_string).collect::<Vec<_>>();
    if answers.is_empty() {
        "(no answer)".to_string()
    } else {
        answers.join(", ")
    }
}

fn opencode_v2_tool_output(content: &Value) -> Option<String> {
    let items = content.as_array()?;
    let output = items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str)? {
            "text" => item
                .get("text")
                .and_then(value_to_string)
                .map(|text| text.trim().to_string()),
            "file" => {
                let name = item
                    .get("name")
                    .and_then(value_to_string)
                    .or_else(|| item.get("uri").and_then(value_to_string))
                    .unwrap_or_default();
                Some(format!("[file {name}]"))
            }
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!output.is_empty()).then_some(output)
}

fn opencode_v2_tool_input(input: &serde_json::Map<String, Value>) -> String {
    let primitives = input
        .iter()
        .filter_map(|(key, value)| match value {
            Value::String(text) => Some(format!("{key}={text}")),
            Value::Number(number) => Some(format!("{key}={number}")),
            Value::Bool(boolean) => Some(format!("{key}={boolean}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    if primitives.is_empty() {
        String::new()
    } else {
        format!("[{}]", primitives.join(", "))
    }
}

// Note: prior helpers (read_opencode_db_parts_text + opencode_part_text)
// joined every part into one text body. They were replaced by per-part
// emission inside read_opencode_db_messages so tool parts carry a TUI
// label and own their own SessionMessage. The synthetic / reasoning /
// tool dispatching that used to live here is now inline at the call site.

fn opencode_storage_roots(root: &Path) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    let legacy = root.join("storage");
    if legacy.join("session").exists() {
        roots.push(legacy);
    }

    let project_root = root.join("project");
    if let Ok(entries) = fs::read_dir(project_root) {
        for entry in entries.filter_map(Result::ok) {
            let storage = entry.path().join("storage");
            if storage.join("session").exists() {
                roots.push(storage);
            }
        }
    }

    let global = root.join("global").join("storage");
    if global.join("session").exists() {
        roots.push(global);
    }
    roots
}

fn scan_opencode_storage(root: &Path) -> Result<Vec<AppSession>, Box<dyn Error>> {
    let sessions_root = root.join("session");
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in WalkDir::new(&sessions_root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        if let Ok(detail) = parse_opencode_storage_session(path) {
            sessions.push(detail.session);
        }
    }
    Ok(sessions)
}

fn parse_opencode_storage_session(path: &Path) -> Result<SessionDetail, Box<dyn Error>> {
    let content = fs::read_to_string(path)?;
    let value = serde_json::from_str::<Value>(&content)?;
    if value.get("parentID").is_some() {
        return Err("OpenCode child session".into());
    }
    let id = value
        .get("id")
        .and_then(value_to_string)
        .ok_or("missing OpenCode session id")?;
    let project = value
        .get("directory")
        .and_then(value_to_string)
        .or_else(|| value.get("project").and_then(value_to_string))
        .ok_or("missing OpenCode session directory")?;
    let explicit_title = value
        .get("title")
        .and_then(value_to_string)
        .map(|title| opencode_display_title(&title))
        .and_then(|title| official_title_from_text(&title));
    let started_at = value
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(value_to_string)
        .or_else(|| value.get("created").and_then(value_to_string))
        .and_then(normalize_time);
    let updated_at = value
        .get("time")
        .and_then(|time| time.get("updated"))
        .and_then(value_to_string)
        .or_else(|| value.get("updated").and_then(value_to_string))
        .and_then(normalize_time)
        .or_else(|| started_at.clone());
    let messages = read_opencode_storage_messages(path, &id)?;
    let mut detail = detail_from_messages(
        "OpenCode",
        path,
        id,
        project,
        messages,
        Vec::new(),
        explicit_title,
    );
    detail.session.started_at = detail.session.started_at.or(started_at);
    detail.session.updated_at = updated_at.or(detail.session.updated_at);
    Ok(detail)
}

fn read_opencode_storage_messages(
    session_path: &Path,
    session_id: &str,
) -> Result<Vec<SessionMessage>, Box<dyn Error>> {
    let Some(storage_root) = opencode_storage_root_from_session_path(session_path) else {
        return Ok(Vec::new());
    };
    let message_root = storage_root.join("message").join(session_id);
    if !message_root.exists() {
        return Ok(Vec::new());
    }
    let mut messages = Vec::new();
    for entry in fs::read_dir(message_root)? {
        let path = entry?.path();
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let content = fs::read_to_string(path)?;
        let value = serde_json::from_str::<Value>(&content)?;
        if let Some(message) = opencode_storage_message_from_value(&value) {
            messages.push(message);
        }
    }
    messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    Ok(messages)
}

fn opencode_storage_root_from_session_path(path: &Path) -> Option<std::path::PathBuf> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.file_name().and_then(|name| name.to_str()) == Some("storage") {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn detail_from_messages(
    source: &str,
    path: &Path,
    id: String,
    project: String,
    messages: Vec<SessionMessage>,
    timestamps: Vec<String>,
    explicit_title: Option<String>,
) -> SessionDetail {
    let started_at = timestamps
        .iter()
        .filter_map(|t| normalize_time(t.clone()))
        .min();
    let updated_at = timestamps
        .iter()
        .filter_map(|t| normalize_time(t.clone()))
        .max();
    let title = explicit_title.unwrap_or_default();
    // Snippet for the list card: prefer the latest user message in the
    // parsed transcript (last what the human said), falling back to the
    // first. Empty when no user messages are present.
    let snippet = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .or_else(|| messages.iter().find(|m| m.role == "user"))
        .map(|m| snippet_line(&m.text))
        .unwrap_or_default();

    SessionDetail {
        session: AppSession {
            id,
            source: source.to_string(),
            title,
            project,
            path: path.display().to_string(),
            started_at,
            updated_at,
            message_count: messages.len(),
            archived: false,
            preview: String::new(),
            snippet,
            message_previews: Vec::new(),
            tokens: None,
            model: None,
            daily_tokens: None,
        },
        messages,
    }
}

fn opencode_storage_message_from_value(value: &Value) -> Option<SessionMessage> {
    let role = value.get("role").and_then(value_to_string)?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let text = value
        .get("content")
        .and_then(opencode_storage_content_text)
        .or_else(|| value.get("text").and_then(value_to_string))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let timestamp = value
        .get("time")
        .and_then(value_to_string)
        .or_else(|| value.get("timestamp").and_then(value_to_string))
        .or_else(|| value.get("created").and_then(value_to_string))
        .and_then(normalize_time);
    Some(SessionMessage {
        role,
        text: trimmed.to_string(),
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

fn opencode_storage_content_text(value: &Value) -> Option<String> {
    let mut parts = Vec::new();
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                if item
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "text")
                {
                    continue;
                }
                if let Some(text) = item.get("text").and_then(value_to_string) {
                    parts.push(text);
                }
            }
        }
        _ => {}
    }
    let text = parts.join("\n").trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn opencode_display_title(title: &str) -> String {
    for prefix in ["New session", "Child session"] {
        let marker = format!("{prefix} - ");
        if let Some(timestamp) = title.strip_prefix(&marker) {
            if DateTime::parse_from_rfc3339(timestamp).is_ok() {
                return prefix.to_string();
            }
        }
    }
    title.to_string()
}

fn title_case(value: &str) -> String {
    value
        .split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn claude_message_from_value(value: &Value) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return out;
    }
    let Some(entry_type) = value.get("type").and_then(Value::as_str) else {
        return out;
    };
    let timestamp = value
        .get("timestamp")
        .and_then(value_to_string)
        .and_then(normalize_time);

    match entry_type {
        "user" => {
            // Claude TUI hides `isMeta: true` user messages (synthetic
            // wrappers like `<local-command-caveat>` injected by the
            // CLI). Termory shows them as italic notices so the user
            // can see what was sent. The wrapped content itself is
            // surfaced via claude_display_text's wrapper handling.
            let is_meta = value.get("isMeta").and_then(Value::as_bool) == Some(true);
            let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
                return out;
            };
            if is_meta {
                let text = claude_text_blocks(content).join("\n");
                if let Some(display) = claude_display_text(&text) {
                    out.push(SessionMessage {
                        role: "system".to_string(),
                        text: format!("*[meta]* {display}"),
                        timestamp: timestamp.clone(),
                        kind: kind::TEXT.to_string(),
                        tool_use_id: None,
                        exit_code: None,
                    });
                }
                return out;
            }
            // 1. Plain text portion (skipped when empty).
            let text = claude_text_blocks(content).join("\n");
            if !text.trim().is_empty() {
                if let Some(display) = claude_display_text(&text) {
                    out.push(SessionMessage {
                        role: "user".to_string(),
                        text: display,
                        timestamp: timestamp.clone(),
                        kind: kind::TEXT.to_string(),
                        tool_use_id: None,
                        exit_code: None,
                    });
                }
            }
            // 2. Tool result blocks — surfaced as tool output messages so the
            //    transcript reflects what each prior tool call returned. Carry
            //    the tool_use_id so merge_tool_outputs can fold the result
            //    back into the matching tool_use card.
            //
            //    For Edit / MultiEdit / Write tools, Claude also stores a
            //    `toolUseResult.structuredPatch` sibling field on the JSONL
            //    line (next to `message`) — this is what Claude TUI's
            //    `StructuredDiff.tsx` renders as a unified diff with
            //    `+`/`-` lines. We prefer that diff over the brief
            //    "The file ... has been updated successfully" content
            //    when present, matching the official UI's verbosity.
            //
            //    Other tools (WebFetch / NotebookEdit / EnterWorktree /
            //    ExitWorktree / ConfigTool / ScheduleCron) store their
            //    own structured fields on `toolUseResult`; the
            //    `claude_format_structured_result` dispatcher tries each
            //    in turn so the bold-count summary line matches the
            //    per-tool TUI rendering.
            let tool_use_result = value.get("toolUseResult");
            let diff_text = tool_use_result.and_then(claude_format_structured_patch);
            for tool_result in claude_tool_results(content) {
                let structured = tool_use_result
                    .and_then(|tur| claude_format_structured_result(tur, &tool_result.text));
                let body_text = diff_text
                    .clone()
                    .filter(|_| !tool_result.is_error)
                    .or_else(|| structured.filter(|_| !tool_result.is_error))
                    .or_else(|| claude_display_text(&tool_result.text));
                if let Some(display) = body_text {
                    out.push(SessionMessage {
                        role: "tool".to_string(),
                        text: display,
                        timestamp: timestamp.clone(),
                        kind: if tool_result.is_error {
                            kind::TOOL_ERROR.to_string()
                        } else {
                            kind::TOOL_RESULT.to_string()
                        },
                        tool_use_id: tool_result.tool_use_id,
                        exit_code: None,
                    });
                }
            }
        }
        "assistant" => {
            let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
                return out;
            };
            // 1. Extended-thinking blocks render BEFORE the visible text
            //    (matching `AssistantThinkingMessage`'s `∴ Thinking…`
            //    indented dim markdown placement in claude-code TUI).
            //    Format via the shared `format_reasoning_body` helper so
            //    Claude / Codex / OpenCode all use the same `> *content*`
            //    italic-blockquote convention.
            for thinking in claude_thinking_blocks(content) {
                let formatted = format_reasoning_body(&thinking);
                if formatted.is_empty() {
                    continue;
                }
                out.push(SessionMessage {
                    role: "assistant".to_string(),
                    text: formatted,
                    timestamp: timestamp.clone(),
                    kind: kind::REASONING.to_string(),
                    tool_use_id: None,
                    exit_code: None,
                });
            }
            // 2. Plain assistant text.
            let text = claude_assistant_text_blocks(content).join("\n");
            if !text.trim().is_empty() {
                if let Some(display) = claude_display_text(&text) {
                    out.push(SessionMessage {
                        role: "assistant".to_string(),
                        text: display,
                        timestamp: timestamp.clone(),
                        kind: kind::TEXT.to_string(),
                        tool_use_id: None,
                        exit_code: None,
                    });
                }
            }
            // 3. Tool use blocks — emitted as separate tool messages, each
            //    labeled with the canonical TUI tool name (Bash/Update/Read/...).
            //    The tool_use.id is preserved so the eventual tool_result can
            //    be paired up and merged into this card.
            for tool_use in claude_tool_uses(content) {
                out.push(SessionMessage {
                    role: "tool".to_string(),
                    text: tool_use.text,
                    timestamp: timestamp.clone(),
                    kind: kind::TOOL_USE.to_string(),
                    tool_use_id: tool_use.id,
                    exit_code: None,
                });
            }
        }
        "system" if value.get("subtype").and_then(Value::as_str) == Some("local_command") => {
            if let Some(text) = value.get("content").and_then(value_to_string) {
                if let Some(display) = claude_display_text(&text) {
                    out.push(SessionMessage {
                        role: "tool".to_string(),
                        text: display,
                        timestamp,
                        kind: kind::LOCAL_COMMAND.to_string(),
                        tool_use_id: None,
                        exit_code: None,
                    });
                }
            }
        }
        // System subtype dispatch. Branches mirror
        // `.audit-sources/claude-code/src/components/messages/SystemTextMessage.tsx`:
        //   - `turn_duration` (line 342-401): `※ Worked for {duration}` italic dim
        //   - `away_summary` (line 70-84): `※ {content}` italic dim
        //   - `compact_boundary` (Message.tsx:195-203): divider notice
        //   - `microcompact_boundary` (Message.tsx:204-207): null
        //   - `api_error`: hidden unless verbose (SystemTextMessage.tsx:139-145)
        // Other subtypes drop silently (matches verbose-only fallthrough).
        "system" => {
            if let Some(msg) = claude_system_message(value, timestamp) {
                out.push(msg);
            }
        }
        // Attachment records — dispatched by `attachment.type` per Claude
        // TUI's `AttachmentMessage.tsx`. Many subtypes are NULL_RENDERING
        // (drop silently). The rest emit a short notice line matching
        // the TUI's `<Line>` output.
        "attachment" => {
            out.extend(claude_attachment_messages(value, timestamp));
        }
        _ => {}
    }
    out
}

/// Render a Claude `system` record (non-`local_command` subtypes) to a
/// single SessionMessage. Mirrors `SystemTextMessage.tsx`'s per-subtype
/// branches; drops subtypes that Claude TUI also hides (api_error /
/// microcompact_boundary / verbose-only generic info).
fn claude_system_message(value: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let subtype = value.get("subtype").and_then(Value::as_str)?;
    let make = |text: String| {
        Some(SessionMessage {
            role: "system".to_string(),
            text,
            timestamp: timestamp.clone(),
            kind: kind::TEXT.to_string(),
            tool_use_id: None,
            exit_code: None,
        })
    };
    match subtype {
        // SystemTextMessage.tsx:342-401 — `※ Worked for {duration}`.
        // The TUI samples a verb from TURN_COMPLETION_VERBS ("Worked",
        // "Toiled", "Labored", etc.); Termory uses a stable "Worked"
        // since session replay needs a deterministic transcript.
        "turn_duration" => {
            let ms = value.get("durationMs").and_then(Value::as_i64)?;
            make(format!("*※ Worked for {}*", format_duration_short(ms)))
        }
        // SystemTextMessage.tsx:70-84 — `※ {content}` dim italic.
        "away_summary" => {
            let content = value.get("content").and_then(Value::as_str)?;
            let trimmed = content.trim();
            if trimmed.is_empty() {
                None
            } else {
                make(format!("*※ {trimmed}*"))
            }
        }
        // SystemTextMessage.tsx:87-101 — red BLACK_CIRCLE + dim text.
        "agents_killed" => make("**Error** All background agents stopped".to_string()),
        // Message.tsx:195-203 — `<CompactBoundaryMessage>` divider.
        // The actual TUI renders a horizontal rule with a label; in
        // markdown we approximate with `---` GFM divider + italic notice.
        "compact_boundary" => {
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let body = match content {
                Some(c) => format!("---\n\n*{c}*\n\n---"),
                None => "---\n\n*Conversation compacted*\n\n---".to_string(),
            };
            make(body)
        }
        // microcompact_boundary → null (Message.tsx:204-207)
        // api_error → verbose-only (SystemTextMessage.tsx:139-145)
        // Other subtypes drop silently.
        _ => None,
    }
}

/// Compact human-readable duration (`45269 ms` → `45.3s`).
/// Used by `turn_duration` system messages.
fn format_duration_short(ms: i64) -> String {
    if ms < 0 {
        return "0s".to_string();
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else if secs < 3600.0 {
        let m = (secs / 60.0).floor();
        let s = secs - m * 60.0;
        format!("{}m {:.0}s", m as i64, s)
    } else {
        let h = (secs / 3600.0).floor();
        let m = ((secs - h * 3600.0) / 60.0).floor();
        format!("{}h {}m", h as i64, m as i64)
    }
}

/// Render a Claude `attachment` record to zero-or-one SessionMessages.
/// Branches mirror `.audit-sources/claude-code/src/components/messages/AttachmentMessage.tsx:162-450`.
/// NULL_RENDERING types (task_reminder / deferred_tools_delta /
/// command_permissions / date_change / hook_success / async_hook_response
/// / agent_setting / etc.) return an empty list to match Claude's
/// `nullRenderingAttachments.ts:14-49`.
fn claude_attachment_messages(value: &Value, timestamp: Option<String>) -> Vec<SessionMessage> {
    let Some(att) = value.get("attachment") else {
        return Vec::new();
    };
    let Some(att_type) = att.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let push = |text: String| {
        vec![SessionMessage {
            role: "system".to_string(),
            text,
            timestamp: timestamp.clone(),
            kind: kind::TEXT.to_string(),
            tool_use_id: None,
            exit_code: None,
        }]
    };
    let display_path = att
        .get("displayPath")
        .and_then(Value::as_str)
        .or_else(|| att.get("filename").and_then(Value::as_str))
        .unwrap_or("");
    match att_type {
        // AttachmentMessage.tsx:163-168
        "directory" => push(format!(
            "Listed directory {}",
            wrap_inline_code(&format!("{display_path}/"))
        )),
        // AttachmentMessage.tsx:169-194
        "file" | "already_read_file" => {
            let content = att.get("content");
            let inner_type = content
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let detail = match inner_type {
                "notebook" => {
                    let cells = content
                        .and_then(|c| c.get("file"))
                        .and_then(|f| f.get("cells"))
                        .and_then(Value::as_array)
                        .map(|c| c.len())
                        .unwrap_or(0);
                    format!("{cells} cells")
                }
                "file_unchanged" => "unchanged".to_string(),
                "text" => {
                    let num_lines = content
                        .and_then(|c| c.get("file"))
                        .and_then(|f| f.get("numLines"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let truncated = att
                        .get("truncated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let plus = if truncated { "+" } else { "" };
                    format!("{num_lines}{plus} lines")
                }
                _ => {
                    // image/pdf/parts paths use `file.originalSize`
                    let size = content
                        .and_then(|c| c.get("file"))
                        .and_then(|f| f.get("originalSize"))
                        .and_then(Value::as_i64);
                    match size {
                        Some(s) => format!("{} bytes", s),
                        None => String::new(),
                    }
                }
            };
            if detail.is_empty() {
                push(format!("Read {}", wrap_inline_code(display_path)))
            } else {
                push(format!(
                    "Read {} ({detail})",
                    wrap_inline_code(display_path)
                ))
            }
        }
        // AttachmentMessage.tsx:195-200
        "compact_file_reference" => push(format!(
            "Referenced file {}",
            wrap_inline_code(display_path)
        )),
        // AttachmentMessage.tsx:201-207
        "pdf_reference" => {
            let pages = att.get("pageCount").and_then(Value::as_i64).unwrap_or(0);
            push(format!(
                "Referenced PDF {} ({pages} pages)",
                wrap_inline_code(display_path)
            ))
        }
        // AttachmentMessage.tsx:208-216
        "selected_lines_in_ide" => {
            let start = att.get("lineStart").and_then(Value::as_i64).unwrap_or(0);
            let end = att.get("lineEnd").and_then(Value::as_i64).unwrap_or(0);
            let lines = end - start + 1;
            let ide = att.get("ideName").and_then(Value::as_str).unwrap_or("");
            push(format!(
                "⧉ Selected {lines} lines from {} in {ide}",
                wrap_inline_code(display_path)
            ))
        }
        // AttachmentMessage.tsx:217-222
        "nested_memory" => push(format!("Loaded {}", wrap_inline_code(display_path))),
        // AttachmentMessage.tsx:280-290 — isInitial returns null
        "skill_listing" => {
            if att.get("isInitial").and_then(Value::as_bool) == Some(true) {
                return Vec::new();
            }
            let count = att.get("skillCount").and_then(Value::as_i64).unwrap_or(0);
            let noun = if count == 1 { "skill" } else { "skills" };
            push(format!("{count} {noun} available"))
        }
        // AttachmentMessage.tsx:302-322 — queued_command wraps a prompt
        // that itself goes through the UserTextMessage XML-wrapper
        // dispatch chain (i.e. `<task-notification>` etc. apply).
        "queued_command" => {
            let prompt = att.get("prompt");
            let text = match prompt {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| {
                        if item.get("type").and_then(Value::as_str) == Some("text") {
                            item.get("text").and_then(value_to_string)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if text.trim().is_empty() {
                Vec::new()
            } else {
                claude_display_text(&text).map(push).unwrap_or_default()
            }
        }
        // AttachmentMessage.tsx:324-329
        "plan_file_reference" => {
            let plan_path = att
                .get("planFilePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            push(format!(
                "Plan file referenced ({})",
                wrap_inline_code(plan_path)
            ))
        }
        // AttachmentMessage.tsx:330-336
        "invoked_skills" => {
            let names: Vec<String> = att
                .get("skills")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.get("name").and_then(value_to_string))
                        .collect()
                })
                .unwrap_or_default();
            if names.is_empty() {
                Vec::new()
            } else {
                push(format!("Skills restored ({})", names.join(", ")))
            }
        }
        // AttachmentMessage.tsx:339-345
        "mcp_resource" => {
            let name = att.get("name").and_then(Value::as_str).unwrap_or("");
            let server = att.get("server").and_then(Value::as_str).unwrap_or("");
            push(format!(
                "Read MCP resource {} from {server}",
                wrap_inline_code(name)
            ))
        }
        // `date_change`: `{newDate: "2026-05-25"}` — Claude TUI hides
        // (NULL_RENDERING). Termory surfaces the new date inline.
        "date_change" => {
            let date = att
                .get("newDate")
                .and_then(Value::as_str)
                .unwrap_or("(unknown)");
            push(format!("*[date changed to {date}]*"))
        }
        // `auto_mode`: `{}` — flag-only attachment indicating auto
        // mode toggled on. Show as a single italic notice.
        "auto_mode" => push("*[auto mode]*".to_string()),
        // `deferred_tools_delta`: `{addedNames: [...], removedNames?: [...]}`
        // — Claude TUI hides. Termory surfaces a count + truncated
        // list so the user can see what changed without a wall of text.
        "deferred_tools_delta" => {
            let added = att
                .get("addedNames")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let removed = att
                .get("removedNames")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut text = String::from("**Deferred tools changed**");
            if !added.is_empty() {
                text.push_str(&format!(
                    "\n\n⎿ Added ({}): {}",
                    added.len(),
                    added.join(", ")
                ));
            }
            if !removed.is_empty() {
                text.push_str(&format!(
                    "\n\n⎿ Removed ({}): {}",
                    removed.len(),
                    removed.join(", ")
                ));
            }
            push(text)
        }
        // `task_reminder`: `{content: [...], itemCount: N}` — Claude
        // TUI hides. When `content` has items, render them; otherwise
        // just show the count so the empty case is still visible.
        "task_reminder" => {
            let count = att.get("itemCount").and_then(Value::as_i64).unwrap_or(0);
            let items: Vec<String> = att
                .get("content")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            v.get("content")
                                .and_then(Value::as_str)
                                .or_else(|| v.as_str())
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut text = format!("**Task reminder** ({count} items)");
            for item in items {
                text.push_str(&format!("\n\n⎿ {item}"));
            }
            push(text)
        }
        // `command_permissions`: `{allowedTools: [...]}` — Claude TUI
        // hides (SkillTool renders the success message instead).
        "command_permissions" => {
            let tools: Vec<String> = att
                .get("allowedTools")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let text = if tools.is_empty() {
                "*[command permissions: none allowed]*".to_string()
            } else {
                format!(
                    "**Command permissions** ({} allowed)\n\n⎿ {}",
                    tools.len(),
                    tools.join(", ")
                )
            };
            push(text)
        }
        // `edited_text_file`: `{filename, snippet}` — context Claude
        // sends back to the model showing recent edits. Surface so the
        // user sees what the model saw.
        "edited_text_file" => {
            let filename = att.get("filename").and_then(Value::as_str).unwrap_or("");
            let snippet = att.get("snippet").and_then(Value::as_str);
            let mut text = format!("**Edited text file**({})", wrap_inline_code(filename));
            if let Some(s) = snippet.map(str::trim).filter(|s| !s.is_empty()) {
                text.push_str(&format!("\n\n````\n{s}\n````"));
            }
            push(text)
        }
        // `plan_mode` / `plan_mode_exit`: `{planFilePath, planExists,
        // reminderType?, isSubAgent?}` — plan-mode lifecycle markers.
        // Claude TUI hides these; Termory surfaces the plan path so
        // the user can see which plan was entered/exited.
        "plan_mode" | "plan_mode_exit" => {
            let verb = if att_type == "plan_mode" {
                "Entered plan mode"
            } else {
                "Exited plan mode"
            };
            let plan_path = att
                .get("planFilePath")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let exists = att.get("planExists").and_then(Value::as_bool);
            let mut text = format!("**{verb}**");
            if let Some(p) = plan_path {
                text.push_str(&format!("\n\n⎿ {}", wrap_inline_code(p)));
            }
            if exists == Some(false) {
                text.push_str("\n\n⎿ *(plan file does not exist yet)*");
            }
            push(text)
        }
        // Goal-mode sentinel attachments. Two flavors observed in real
        // sessions:
        //   sentinel:true, met:false → goal was just set (initial)
        //   met:true → goal was checked and satisfied (with reason +
        //              stats: iterations, durationMs, tokens)
        //   met:false, sentinel missing → goal was checked, not met
        //              (failure with reason)
        "goal_status" => {
            let condition = att
                .get("condition")
                .and_then(Value::as_str)
                .unwrap_or("(no condition)");
            let met = att.get("met").and_then(Value::as_bool).unwrap_or(false);
            let sentinel = att
                .get("sentinel")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reason = att.get("reason").and_then(Value::as_str);
            let iterations = att.get("iterations").and_then(Value::as_i64);
            let duration_ms = att.get("durationMs").and_then(Value::as_i64);
            let tokens = att.get("tokens").and_then(Value::as_i64);
            if sentinel && !met {
                push(format!("**Goal set**\n\n⎿ {condition}"))
            } else {
                let verb = if met { "Goal met" } else { "Goal not met" };
                let mut stats: Vec<String> = Vec::new();
                if let Some(n) = iterations {
                    stats.push(format!("{n} iter"));
                }
                if let Some(ms) = duration_ms {
                    stats.push(format_duration_short(ms));
                }
                if let Some(t) = tokens {
                    stats.push(format!("{t} tokens"));
                }
                let header = if stats.is_empty() {
                    format!("**{verb}**")
                } else {
                    format!("**{verb}** ({})", stats.join(", "))
                };
                let mut text = format!("{header}\n\n⎿ {condition}");
                if let Some(r) = reason {
                    text.push_str(&format!("\n\n⎿ {r}"));
                }
                push(text)
            }
        }
        // Any other attachment type (NULL_RENDERING ones that Claude
        // TUI silently drops, plus types we don't have a specialized
        // formatter for yet) — surface the type name AND a compact
        // JSON dump of the remaining fields so payload data isn't lost.
        // Termory's history-browser promise: nothing on disk should
        // vanish silently. Even if we don't know how to format
        // `hook_success` / `async_hook_response` / `dynamic_skill`
        // nicely, the user can read the raw fields.
        other => {
            let mut extras = serde_json::Map::new();
            if let Some(obj) = att.as_object() {
                for (k, v) in obj {
                    if k == "type" {
                        continue;
                    }
                    extras.insert(k.clone(), v.clone());
                }
            }
            let header = format!("*[attachment: {other}]*");
            if extras.is_empty() {
                push(header)
            } else {
                let payload =
                    serde_json::to_string_pretty(&Value::Object(extras)).unwrap_or_default();
                push(format!("{header}\n\n````json\n{payload}\n````"))
            }
        }
    }
}

struct ClaudeToolResult {
    text: String,
    is_error: bool,
    tool_use_id: Option<String>,
}

/// Build a unified-diff fence from Claude's `toolUseResult.structuredPatch`
/// array — the same data Claude TUI's `StructuredDiff.tsx` consumes.
/// Each hunk has `{oldStart, oldLines, newStart, newLines, lines: [...]}`
/// where `lines` entries are already prefixed with ` ` (context),
/// `+` (added), or `-` (removed). Termory wraps the output in a
/// ```diff fence so the markdown layer can syntax-highlight it later.
///
/// Returns None when the field is missing or empty (caller falls back
/// to the brief tool_result `content` text).
fn claude_format_structured_patch(tool_use_result: &Value) -> Option<String> {
    let hunks = tool_use_result.get("structuredPatch")?.as_array()?;
    if hunks.is_empty() {
        return None;
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut body = String::new();
    for (idx, hunk) in hunks.iter().enumerate() {
        // Claude TUI's `formatDiff` (StructuredDiff/Fallback.tsx:373-440)
        // does NOT emit `@@ -X,N +Y,M @@` header lines. Hunk boundaries
        // are conveyed by line-number jumps in the gutter. Termory has
        // no gutter in markdown, so we use a blank line between hunks
        // for visual separation but skip the `@@` text to match Claude.
        if idx > 0 {
            body.push('\n');
        }
        if let Some(lines) = hunk.get("lines").and_then(Value::as_array) {
            for line in lines.iter().filter_map(Value::as_str) {
                // Count line types for the summary header.
                // `+++` / `---` are file headers (unified diff
                // convention) — Claude's structuredPatch doesn't emit
                // those into `lines`, but guard anyway.
                if line.starts_with('+') && !line.starts_with("+++") {
                    added += 1;
                } else if line.starts_with('-') && !line.starts_with("---") {
                    removed += 1;
                }
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    // Summary line above the fence. Mirrors FileEditToolUpdatedMessage
    // (`components/FileEditToolUpdatedMessage.tsx:38-54`): only the
    // counts are bolded. Every Termory tool summary starts with `⎿ `
    // (locked rule — see CLAUDE.md "Unified tool-message format").
    let mut out = String::from("⎿ ");
    let added_label = if added == 1 { "line" } else { "lines" };
    let removed_label = if removed == 1 { "line" } else { "lines" };
    if added > 0 && removed > 0 {
        out.push_str(&format!(
            "Added **{added}** {added_label}, removed **{removed}** {removed_label}"
        ));
    } else if added > 0 {
        out.push_str(&format!("Added **{added}** {added_label}"));
    } else if removed > 0 {
        out.push_str(&format!("Removed **{removed}** {removed_label}"));
    }
    out.push_str("\n\n```diff\n");
    out.push_str(&body);
    out.push_str("```");
    Some(out)
}

/// Try every known structured-result shape in turn; return the first
/// match. Each helper inspects `toolUseResult` for tool-specific
/// fields and produces a body with a bold-count summary followed
/// (optionally) by a fenced preview, matching the MessageResponse
/// layout of Claude's per-tool UI components.
fn claude_format_structured_result(
    tool_use_result: &Value,
    fallback_content: &str,
) -> Option<String> {
    claude_format_webfetch_body(tool_use_result, fallback_content)
        .or_else(|| claude_format_notebook_edit_body(tool_use_result))
        .or_else(|| claude_format_worktree_body(tool_use_result))
        .or_else(|| claude_format_config_body(tool_use_result))
        .or_else(|| claude_format_schedule_cron_body(tool_use_result))
}

/// WebFetch (`WebFetchTool/UI.tsx:31-58`): `Received **{size}** ({code}
/// {codeText})` summary + 4-backtick fence with the fetched body. The
/// `\n\n````` separator makes `merge_tool_outputs` treat the whole
/// thing as `already_markdown` so the bold renders outside the fence.
fn claude_format_webfetch_body(tool_use_result: &Value, fallback_content: &str) -> Option<String> {
    let bytes = tool_use_result.get("bytes").and_then(Value::as_u64)?;
    let size = claude_format_file_size(bytes);
    let code = tool_use_result.get("code").and_then(Value::as_i64);
    let code_text = tool_use_result
        .get("codeText")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let result_text = tool_use_result
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_content.trim());
    let mut out = format!("⎿ Received **{size}**");
    match (code, code_text) {
        (Some(c), Some(t)) => out.push_str(&format!(" ({c} {t})")),
        (Some(c), None) => out.push_str(&format!(" ({c})")),
        (None, Some(t)) => out.push_str(&format!(" ({t})")),
        (None, None) => {}
    }
    if !result_text.is_empty() {
        out.push_str("\n\n````\n");
        out.push_str(result_text);
        out.push_str("\n````");
    }
    Some(out)
}

/// Human-readable byte count: `1.4 KB`, `12.4 MB`, etc.
fn claude_format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

/// NotebookEdit (`NotebookEditTool/UI.tsx:99-124`):
/// `Updated cell **{cell_id}**` + fenced `new_source` preview.
fn claude_format_notebook_edit_body(tool_use_result: &Value) -> Option<String> {
    let cell_id = tool_use_result.get("cell_id").and_then(Value::as_str)?;
    let mut out = format!("⎿ Updated cell **{cell_id}**");
    if let Some(src) = tool_use_result
        .get("new_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\n\n````\n");
        out.push_str(src);
        out.push_str("\n````");
    }
    Some(out)
}

/// EnterWorktree / ExitWorktree: distinguished by `worktreePath`
/// (enter) vs `originalCwd` (exit).
fn claude_format_worktree_body(tool_use_result: &Value) -> Option<String> {
    let branch = tool_use_result
        .get("worktreeBranch")
        .and_then(Value::as_str)?;
    let path = tool_use_result
        .get("worktreePath")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let original_cwd = tool_use_result
        .get("originalCwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut out = if path.is_some() {
        format!("⎿ Switched to worktree on branch **{branch}**")
    } else if original_cwd.is_some() {
        format!("⎿ Exited worktree (branch **{branch}**)")
    } else {
        format!("⎿ Worktree branch **{branch}**")
    };
    if let Some(p) = path {
        out.push_str(&format!("\n\n⎿ {p}"));
    }
    if let Some(c) = original_cwd {
        out.push_str(&format!("\n\n⎿ Returned to {c}"));
    }
    Some(out)
}

/// ConfigTool (`ConfigTool/UI.tsx:20-43`): get → bold setting + value;
/// set → bold setting + bold newValue.
fn claude_format_config_body(tool_use_result: &Value) -> Option<String> {
    let setting = tool_use_result.get("setting").and_then(Value::as_str)?;
    if tool_use_result.get("success").and_then(Value::as_bool) == Some(false) {
        let err = tool_use_result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("(unknown error)");
        return Some(format!("⎿ Failed: {err}"));
    }
    let op = tool_use_result
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("");
    if op == "get" {
        let value = tool_use_result
            .get("value")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(no value)".to_string());
        return Some(format!("⎿ **{setting}** = {value}"));
    }
    let new_value = tool_use_result
        .get("newValue")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(no value)".to_string());
    Some(format!("⎿ Set **{setting}** to **{new_value}**"))
}

/// ScheduleCron create (`ScheduleCronTool/UI.tsx:17-28`):
/// `Scheduled **{id}** ({humanSchedule})`. Delete shape is ambiguous
/// without tool_use context, so handled only when humanSchedule is set.
fn claude_format_schedule_cron_body(tool_use_result: &Value) -> Option<String> {
    let id = tool_use_result.get("id").and_then(Value::as_str)?;
    let human = tool_use_result
        .get("humanSchedule")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(format!("⎿ Scheduled **{id}** ({human})"))
}

/// Split a Grep / Glob tool_result body into `(summary, rest)` where
/// the summary line gets the count bolded per GrepTool/UI.tsx:30-45.
/// Returns `None` when the body doesn't begin with `Found N …`.
fn claude_split_grep_summary(body: &str) -> Option<(String, &str)> {
    let (first, rest) = body.split_once('\n').unwrap_or((body, ""));
    let after_found = first.strip_prefix("Found ")?;
    let (count, label) = after_found.split_once(' ')?;
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((format!("Found **{count}** {label}"), rest))
}

fn claude_tool_results(content: &Value) -> Vec<ClaudeToolResult> {
    let Value::Array(items) = content else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|item| {
            let text = match item.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            block.get("text").and_then(value_to_string)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return None,
            };
            if text.trim().is_empty() {
                return None;
            }
            Some(ClaudeToolResult {
                text,
                is_error: item
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                tool_use_id: item.get("tool_use_id").and_then(value_to_string),
            })
        })
        .collect()
}

struct ClaudeToolUse {
    text: String,
    id: Option<String>,
}

fn claude_tool_uses(content: &Value) -> Vec<ClaudeToolUse> {
    let Value::Array(items) = content else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|item| {
            let name = item
                .get("name")
                .and_then(value_to_string)
                .unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            let input = item.get("input").unwrap_or(&Value::Null);
            // Termory shows every tool card even when Claude TUI suppresses
            // it — see specialized branches at the top of claude_tool_use_text.
            let text = claude_tool_use_text(&name, input)?;
            Some(ClaudeToolUse {
                text,
                id: item.get("id").and_then(value_to_string),
            })
        })
        .collect()
}

/// Format a Claude tool_use block as the TUI's `**{userFacingName}**(...)`
/// inline. Each branch mirrors the Tool's `renderToolUseMessage` output
/// (returns the content inside the parens) and the Tool's `userFacingName`
/// (the bold label).
///
/// Sources in .audit-sources/claude-code/src/tools/:
/// * src/components/messages/AssistantToolUseMessage.tsx:152 — wrapper
///   emits `<bold>{userFacingName}</bold>(<text>{render output}</text>)`.
/// * BashTool/UI.tsx renderToolUseMessage — returns the command.
/// * FileReadTool/UI.tsx → "Read", returns displayPath.
/// * FileWriteTool/UI.tsx → "Write", returns displayPath.
/// * FileEditTool/UI.tsx → "Update", returns displayPath.
/// * GrepTool.ts:170 → "Search", returns `pattern: "{pattern}"` (+ `, path: ...`).
/// * GlobTool/UI.tsx:13 → "Search", same `pattern: "..."` format.
/// * WebFetchTool.ts:81 → "Fetch", returns the URL.
/// * WebSearchTool.ts:160 → "Web Search", returns `"{query}"`.
/// Detect agent-output file paths used by Claude Code's TaskOutput tool.
/// Pattern: `…/tasks/{taskId}.output` where taskId is alphanumeric +
/// `_-` and ≤20 chars. Matches `FileReadTool/UI.tsx:18-33`.
fn claude_agent_output_task_id(path: &str) -> Option<String> {
    let stripped = path.strip_suffix(".output")?;
    let after_tasks = stripped.rsplit_once("/tasks/")?.1;
    if after_tasks.is_empty() || after_tasks.len() > 20 {
        return None;
    }
    if after_tasks
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Some(after_tasks.to_string())
    } else {
        None
    }
}

/// `TodoWrite` (`TodoWriteTool.ts:15`): input.todos = `[{status, content, ...}]`.
/// Claude TUI hides this; Termory shows it with the same `✓/~/✕/☐`
/// glyphs OpenCode TUI uses (matches `todo_icon` helper).
fn format_todowrite_card(input: &Value) -> String {
    let mut out = String::from("**Todos**");
    let Some(todos) = input.get("todos").and_then(Value::as_array) else {
        return out;
    };
    if todos.is_empty() {
        out.push_str("\n\n⎿ (no todos)");
        return out;
    }
    out.push_str("\n");
    for todo in todos {
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let content = todo
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("(no content)");
        out.push_str(&format!("\n{} {}", todo_icon(status), content));
    }
    out
}

/// `AskUserQuestion` (`AskUserQuestionTool.tsx:135-149`): input.questions =
/// `[{question, options: [{label, description?}], multiSelect?}]`. Claude
/// TUI hides this; Termory renders it as a Q+choices block.
fn format_ask_user_question_card(input: &Value) -> String {
    let mut out = String::from("**Ask User Question**");
    let Some(questions) = input.get("questions").and_then(Value::as_array) else {
        return out;
    };
    for q in questions {
        let question = q
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("(no question)");
        out.push_str(&format!("\n\n⎿ {question}"));
        if let Some(options) = q.get("options").and_then(Value::as_array) {
            for opt in options {
                if let Some(label) = opt.get("label").and_then(Value::as_str) {
                    out.push_str(&format!("\n  - {label}"));
                }
            }
        }
    }
    out
}

/// `EnterPlanMode` (`EnterPlanModeTool.ts:29`) / `ExitPlanMode`
/// (`ExitPlanModeV2Tool.ts:99,112`): both carry either `message` (enter)
/// or `plan` (exit) as a string. Claude TUI hides these; Termory shows
/// the body so the transcript records the plan content.
fn format_plan_mode_card(verb: &str, input: &Value) -> String {
    let body = input
        .get("plan")
        .or_else(|| input.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut out = format!("**{verb}**");
    if let Some(b) = body {
        out.push_str("\n\n");
        out.push_str(b);
    }
    out
}

fn claude_tool_use_text(name: &str, input: &Value) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let obj = input.as_object();
    let get = |key: &str| obj.and_then(|o| o.get(key)).and_then(value_to_string);
    // Tools that Claude TUI suppresses entirely (`userFacingName: ''`
    // AND `renderToolUseMessage: () => null`) are RENDERED HERE — Termory
    // is a history browser, so hiding parts of the transcript would be
    // misleading. We provide compact summaries; full input is rendered
    // as fallback when no specialized formatter matches.
    if matches!(lower.as_str(), "todowrite" | "todo_write") {
        return Some(format_todowrite_card(input));
    }
    if matches!(lower.as_str(), "askuserquestion" | "ask_user_question") {
        return Some(format_ask_user_question_card(input));
    }
    if matches!(lower.as_str(), "enterplanmode" | "enter_plan_mode") {
        return Some(format_plan_mode_card("EnterPlanMode", input));
    }
    if matches!(
        lower.as_str(),
        "exitplanmode" | "exit_plan_mode" | "exitplanmodev2"
    ) {
        return Some(format_plan_mode_card("ExitPlanMode", input));
    }
    // All dynamic arguments (commands, paths, URLs, patterns) are wrapped
    // with `wrap_inline_code` so any markdown special characters inside the
    // user payload (backticks, `*`, `_`, `()`) don't leak into the rendered
    // output. The TUI version (Claude Code Ink) doesn't need this because
    // it bypasses markdown entirely.
    let text = match lower.as_str() {
        "bash" => {
            let command = get("command").unwrap_or_default();
            if command.is_empty() {
                "**Bash**".to_string()
            } else {
                format!("**Bash**({})", wrap_inline_code(&command))
            }
        }
        "read" | "view" => {
            let path = get("file_path").unwrap_or_default();
            if path.is_empty() {
                return Some("**Read**".to_string());
            }
            // FileReadTool/UI.tsx userFacingName (l.179-187): the verb
            // varies by file path. Agent-output reads (path matches
            // `{tempDir}/tasks/{taskId}.output`) use verb
            // "Read agent output" and show just the taskId as args.
            // Plans-directory variant is skipped — `getPlansDirectory()`
            // depends on session config Termory doesn't have access to.
            if let Some(task_id) = claude_agent_output_task_id(&path) {
                return Some(format!(
                    "**Read agent output**({})",
                    wrap_inline_code(&task_id)
                ));
            }
            let pages = get("pages");
            let offset = get("offset");
            let limit = get("limit");
            let mut text = format!("**Read**({}", wrap_inline_code(&path));
            if let Some(p) = pages {
                text.push_str(&format!(" · pages {p}"));
            } else {
                // Match FileReadTool/UI.tsx:58-67 exactly:
                //   * offset defaults to 1 when absent
                //   * with limit  → `lines {start}-{start + limit - 1}`
                //   * without limit but with offset → `from line {start}`
                //   * neither → no suffix
                let start: i64 = offset.as_deref().and_then(|s| s.parse().ok()).unwrap_or(1);
                let limit_n: Option<i64> = limit.as_deref().and_then(|s| s.parse().ok());
                if let Some(n) = limit_n {
                    text.push_str(&format!(" · lines {start}-{}", start + n - 1));
                } else if offset.is_some() {
                    text.push_str(&format!(" · from line {start}"));
                }
            }
            text.push(')');
            text
        }
        "write" => {
            let path = get("file_path").unwrap_or_default();
            if path.is_empty() {
                return Some("**Write**".to_string());
            }
            // Mirror FileWriteTool/UI.tsx:55-74: emit a `⎿ Wrote **N**
            // lines to **{file}**` summary + syntax-hinted content
            // preview inside a fence. The tool_result confirmation
            // gets dropped in merge_tool_outputs (Write tool branch).
            let content = get("content").unwrap_or_default();
            let line_count = content.lines().count();
            let label = if line_count == 1 { "line" } else { "lines" };
            let basename = path
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&path);
            let lang = filetype_hint(&path);
            let mut text = format!("**Write**({})", wrap_inline_code(&path));
            text.push_str(&format!(
                "\n\n⎿ Wrote **{line_count}** {label} to **{basename}**"
            ));
            if !content.is_empty() {
                if lang.is_empty() {
                    text.push_str(&format!("\n\n```\n{content}\n```"));
                } else {
                    text.push_str(&format!("\n\n```{lang}\n{content}\n```"));
                }
            }
            text
        }
        "edit" | "multiedit" | "str_replace" | "str_replace_editor" => {
            let path = get("file_path").unwrap_or_default();
            // FileEditTool/UI.tsx:28-87 — verb defaults to "Update", but
            // becomes "Create" when `old_string === ''` (new file being
            // written via edit). MultiEdit checks the first edit.
            // Use `Value::as_str` directly (NOT `get` helper) because
            // `value_to_string` drops empty strings as None — and an
            // empty string is the exact signal we need to detect here.
            let creating = match lower.as_str() {
                "multiedit" => input
                    .get("edits")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(|e| e.get("old_string"))
                    .and_then(Value::as_str)
                    .is_some_and(str::is_empty),
                _ => input
                    .get("old_string")
                    .and_then(Value::as_str)
                    .is_some_and(str::is_empty),
            };
            let verb = if creating { "Create" } else { "Update" };
            if path.is_empty() {
                format!("**{verb}**")
            } else {
                format!("**{verb}**({})", wrap_inline_code(&path))
            }
        }
        "grep" => {
            let pattern = get("pattern").unwrap_or_default();
            if pattern.is_empty() && get("path").is_none() {
                return Some("**Search**".to_string());
            }
            let mut parts: Vec<String> = Vec::new();
            if !pattern.is_empty() {
                parts.push(format!("pattern: {}", wrap_inline_code(&pattern)));
            }
            if let Some(path) = get("path") {
                parts.push(format!("path: {}", wrap_inline_code(&path)));
            }
            format!("**Search**({})", parts.join(", "))
        }
        "glob" => {
            let pattern = get("pattern").unwrap_or_default();
            let path = get("path");
            match (pattern.is_empty(), path.as_deref()) {
                (true, None) => "**Search**".to_string(),
                (false, Some(p)) => format!(
                    "**Search**(pattern: {}, path: {})",
                    wrap_inline_code(&pattern),
                    wrap_inline_code(p)
                ),
                (false, None) => format!("**Search**(pattern: {})", wrap_inline_code(&pattern)),
                (true, Some(p)) => format!("**Search**(path: {})", wrap_inline_code(p)),
            }
        }
        "websearch" | "web_search" => {
            let query = get("query").unwrap_or_default();
            if query.is_empty() {
                "**Web Search**".to_string()
            } else {
                format!("**Web Search**({})", wrap_inline_code(&query))
            }
        }
        "webfetch" | "web_fetch" => {
            let url = get("url").unwrap_or_default();
            if url.is_empty() {
                "**Fetch**".to_string()
            } else {
                format!("**Fetch**({})", wrap_inline_code(&url))
            }
        }
        // AgentTool userFacingName depends on `subagent_type`
        // (AgentTool/UI.tsx export userFacingName):
        //   * `worker` → "Agent"
        //   * `general-purpose` (or missing) → "Agent" (description as args)
        //   * other subagent_type → use it verbatim
        // renderToolUseMessage emits the description.
        "task" | "agent" => {
            let description = get("description").unwrap_or_default();
            let subagent_type = get("subagent_type").unwrap_or_default();
            let verb = if subagent_type.is_empty()
                || subagent_type == "general-purpose"
                || subagent_type == "worker"
            {
                "Agent".to_string()
            } else {
                subagent_type
            };
            if description.is_empty() {
                format!("**{verb}**")
            } else {
                format!("**{verb}**({})", wrap_inline_code(&description))
            }
        }
        // SkillTool/UI.tsx — emits `skill` or `/skill` (legacy commands).
        // Termory just shows `**Skill**({name})` to keep it predictable.
        "skill" => {
            let skill = get("skill").or_else(|| get("name")).unwrap_or_default();
            if skill.is_empty() {
                "**Skill**".to_string()
            } else {
                format!("**Skill**({})", wrap_inline_code(&skill))
            }
        }
        // NotebookEditTool/NotebookEditTool.ts userFacingName → "Edit Notebook".
        // Renders `**Edit Notebook**({notebook_path})`.
        "notebookedit" | "notebook_edit" => {
            let path = get("notebook_path")
                .or_else(|| get("file_path"))
                .unwrap_or_default();
            if path.is_empty() {
                "**Edit Notebook**".to_string()
            } else {
                format!("**Edit Notebook**({})", wrap_inline_code(&path))
            }
        }
        // ReadMcpResourceTool/UI.tsx userFacingName → literal 'readMcpResource'
        // (camelCase, matches Claude TUI exactly).
        "readmcpresource" | "read_mcp_resource" => {
            let uri = get("uri").or_else(|| get("url")).unwrap_or_default();
            if uri.is_empty() {
                "**readMcpResource**".to_string()
            } else {
                format!("**readMcpResource**({})", wrap_inline_code(&uri))
            }
        }
        // ListMcpResourcesTool userFacingName → literal 'listMcpResources'.
        "listmcpresources" | "list_mcp_resources" => {
            let server = get("server").unwrap_or_default();
            if server.is_empty() {
                "**listMcpResources**".to_string()
            } else {
                format!("**listMcpResources**({})", wrap_inline_code(&server))
            }
        }
        // McpAuthTool userFacingName → '{server} - authenticate (MCP)'.
        // Whole label IS the verb (bold).
        "mcpauth" | "mcp_auth" => {
            let server = get("server").unwrap_or_default();
            if server.is_empty() {
                "**authenticate (MCP)**".to_string()
            } else {
                format!("**{server} - authenticate (MCP)**")
            }
        }
        _ => {
            // MCP tools follow the canonical name pattern
            // `mcp__{server}__{tool}` (Claude Code's MCP client
            // strips the prefix in display). Surface them as
            // `**MCP**({server}/{tool})` so they look the same as
            // the Codex `EventMsg::McpToolCallEnd` rendering.
            if let Some(rest) = name.strip_prefix("mcp__") {
                if let Some((server, tool)) = rest.split_once("__") {
                    return Some(format!("**MCP**({server}/{tool})"));
                }
            }
            // Otherwise: bold raw name + compact JSON args.
            let json = serde_json::to_string(input).unwrap_or_default();
            if json == "null" || json == "{}" {
                format!("**{name}**")
            } else {
                format!("**{name}**({})", compact(&json, 200))
            }
        }
    };
    Some(text)
}

fn claude_text_blocks(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str)? {
                "text" => item.get("text").and_then(value_to_string),
                // Anthropic image content blocks
                // (`{"type":"image","source":{"type":"base64"|"url",...}}`):
                // surface as an italic notice with the mime type / URL so
                // the transcript shows that an image was attached. The
                // base64 payload is too large to render inline.
                "image" => Some(claude_image_part_label(item.get("source")?)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn claude_image_part_label(source: &Value) -> String {
    let kind = source.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "base64" => {
            let mime = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            format!("*Image ({mime})*")
        }
        "url" => {
            let url = source.get("url").and_then(Value::as_str).unwrap_or("");
            if url.is_empty() {
                "*Image*".to_string()
            } else {
                format!("*Image: {url}*")
            }
        }
        _ => "*Image*".to_string(),
    }
}

fn claude_assistant_text_blocks(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    item.get("text").and_then(value_to_string)
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Extract Claude extended-thinking blocks
/// (`{"type":"thinking","thinking":"..."}` and `redacted_thinking`).
/// Claude Code's TUI renders these via `AssistantThinkingMessage` as
/// `∴ Thinking…` + dim markdown body. We surface them as separate
/// reasoning messages (italic blockquote via `format_reasoning_body`)
/// instead of dropping the content.
fn claude_thinking_blocks(value: &Value) -> Vec<String> {
    let Value::Array(items) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str)? {
            "thinking" => item.get("thinking").and_then(value_to_string),
            "redacted_thinking" => Some("[redacted thinking]".to_string()),
            _ => None,
        })
        .collect()
}

fn claude_user_content_is_tool_result_only(value: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    !items.is_empty()
        && items
            .iter()
            .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
}

fn claude_display_text(text: &str) -> Option<String> {
    let trimmed_full = text.trim();
    // `(no content)` NO_CONTENT_MESSAGE: Claude TUI hides; Termory
    // shows it as an italic placeholder so the transcript still
    // reflects that an empty message was recorded.
    if trimmed_full == "(no content)" {
        return Some("*(no content)*".to_string());
    }
    // `[Request interrupted by user]` / `[Request interrupted by user
    // for tool use]` → italic notice (UserTextMessage.tsx:83-92 renders
    // `<InterruptedByUser>`; constants at utils/messages.ts:207-209).
    if trimmed_full == "[Request interrupted by user]"
        || trimmed_full == "[Request interrupted by user for tool use]"
    {
        return Some("*[Interrupted by user]*".to_string());
    }
    // `<tick>...</tick>` (UserTextMessage.tsx:57-59 returns null) —
    // Termory unwraps to show the inner text. Empty tick → drop.
    if let Some(inner) = extract_xml_tag_value(text, "tick") {
        let inner = inner.trim();
        return if inner.is_empty() {
            Some("*[tick]*".to_string())
        } else {
            Some(inner.to_string())
        };
    }
    // `<local-command-caveat>...</local-command-caveat>` — Claude TUI
    // hides; Termory shows the caveat text wrapped in an italic
    // notice so the user knows it was a synthetic caveat.
    if let Some(inner) = extract_xml_tag_value(text, "local-command-caveat") {
        let inner = inner.trim();
        return if inner.is_empty() {
            Some("*[local-command-caveat]*".to_string())
        } else {
            Some(format!("*[local-command-caveat]* {inner}"))
        };
    }
    // `<user-memory-input>...</user-memory-input>` → `\# {content}`
    // memory entry (UserMemoryInputMessage.tsx:21-43 renders the inner
    // text as a colored `# {input}` chip; we use `\#` so markdown
    // doesn't interpret as H1).
    if let Some(memory) = extract_xml_tag_value(text, "user-memory-input") {
        let m = memory.trim();
        if !m.is_empty() {
            return Some(format!("\\# {m}"));
        }
    }
    // `({tool} completed with no output)` is the placeholder that
    // `toolResultStorage.ts:293` writes into JSONL tool_result.content
    // when stdout+stderr are both empty. Claude TUI's
    // `BashToolResultMessage.tsx:107-121` detects empty streams and
    // renders the cleaner `(No output)` instead. Termory matches that.
    if trimmed_full.starts_with('(') && trimmed_full.ends_with(" completed with no output)") {
        return Some("(No output)".to_string());
    }
    // Claude's internal `<tool_use_error>...</tool_use_error>` wrapper is
    // not meaningful to humans — it's an Anthropic-internal marker. Strip
    // it to show just the inner error message.
    if let Some(inner) = extract_xml_tag_value(text, "tool_use_error") {
        let trimmed = inner.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    // Bash tool_result wrappers: `<bash-stdout>...</bash-stdout>` and
    // `<bash-stderr>...</bash-stderr>`. Claude TUI's `UserBashOutputMessage`
    // extracts both and renders stdout + stderr. We concatenate stdout
    // first, then stderr (prefixed) so the unified card carries both
    // streams. Also unwrap the inner `<persisted-output>` wrapper that
    // claude-code adds for long captured bash output.
    if text.contains("<bash-stdout>") || text.contains("<bash-stderr>") {
        let stdout = extract_xml_tag_value(text, "bash-stdout")
            .map(|s| extract_xml_tag_value(&s, "persisted-output").unwrap_or(s))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let stderr = extract_xml_tag_value(text, "bash-stderr")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let combined = match (stdout, stderr) {
            (Some(o), Some(e)) => format!("{o}\n\n{e}"),
            (Some(o), None) => o,
            (None, Some(e)) => e,
            (None, None) => return None,
        };
        return Some(combined);
    }
    // Background-task notifications (`<task-notification>...`) emitted by
    // LocalShellTask / LocalAgentTask / RemoteAgentTask. Claude Code's
    // `UserAgentNotificationMessage.tsx` extracts only `<summary>` and
    // shows it as `⏺ {summary}` (with status-derived color). Termory
    // emits the same `⏺ {summary}` prefix; the other tags (task-id /
    // output-file / event / status) are dropped to match Claude TUI.
    if text.contains("<task-notification>") || text.contains("<task-notification ") {
        if let Some(summary) = extract_xml_tag_value(text, "summary") {
            let trimmed = summary.trim();
            if !trimmed.is_empty() {
                return Some(format!("{}{trimmed}", status_marker(false)));
            }
        }
    }
    if text.contains("<command-message>") {
        let command_message = extract_xml_tag_value(text, "command-message")?;
        let args = extract_xml_tag_value(text, "command-args");
        let content = [Some(command_message), args]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        return non_empty_xml_value(format!("/{}", content.trim()));
    }
    if let Some(bash_input) = extract_xml_tag_value(text, "bash-input") {
        return non_empty_xml_value(format!("! {bash_input}"));
    }
    if let Some(stdout) = extract_xml_tag_value(text, "local-command-stdout") {
        return Some(non_empty_xml_value(stdout).unwrap_or_else(|| "(no content)".to_string()));
    }
    if let Some(stderr) = extract_xml_tag_value(text, "local-command-stderr") {
        return Some(non_empty_xml_value(stderr).unwrap_or_else(|| "(no content)".to_string()));
    }
    // Feature-gated wrappers that Claude TUI drops via the generic
    // strip path. Termory surfaces each one as `*[wrapper-name]* {inner}`
    // so the transcript records that one was present.
    for tag in &[
        "github-webhook-activity",
        "teammate-message",
        "fork-boilerplate",
        "cross-session-message",
        "channel",
        "mcp-resource-update",
        "mcp-polling-update",
    ] {
        if let Some(inner) = extract_xml_tag_value(text, tag) {
            let inner = inner.trim();
            return Some(if inner.is_empty() {
                format!("*[{tag}]*")
            } else {
                format!("*[{tag}]* {inner}")
            });
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        let stripped = strip_display_tags_allow_empty(trimmed);
        let display = if stripped.is_empty() {
            trimmed
        } else {
            &stripped
        };
        Some(display.to_string())
    }
}

fn extract_xml_tag_value(text: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = text.find(&start_tag)? + start_tag.len();
    let end = text[start..].find(&end_tag)? + start;
    Some(text[start..end].trim().to_string())
}

fn non_empty_xml_value(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn codex_message_from_value(value: &Value) -> Option<SessionMessage> {
    let item_type = value.get("type").and_then(Value::as_str)?;
    let payload = value.get("payload")?;
    let timestamp = value
        .get("timestamp")
        .and_then(value_to_string)
        .and_then(normalize_time);

    // `event_msg` records are what Codex's TUI uses to render replayed sessions
    // (see `ThreadHistoryBuilder::handle_event` in
    // codex-rs/app-server-protocol/src/protocol/thread_history.rs). For shell
    // tool calls Codex pulls raw `aggregated_output` from
    // `EventMsg::ExecCommandEnd`, NOT from `ResponseItem::FunctionCallOutput`
    // (which only contains the model-facing `Chunk ID: ...Output: ...` format
    // and is a no-op during replay).
    if item_type == "event_msg" {
        return codex_event_msg_to_message(payload, timestamp);
    }

    if item_type != "response_item" {
        return None;
    }
    let payload_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("event");

    // For Codex CLI sessions the default rollout mode is `Limited`
    // (codex-rs/tui/src/app_server_session.rs: `persist_extended_history:
    // false`). In Limited mode the `EventMsg::ExecCommandEnd` event is NOT
    // persisted (see codex-rs/rollout/src/policy.rs:166 — it's
    // Extended-only). So the only source for shell command + output in
    // these sessions is `ResponseItem::FunctionCall` +
    // `ResponseItem::FunctionCallOutput`. We process them here as a
    // fallback. When a session DOES have `EventMsg::ExecCommandEnd` (the
    // canonical source — Extended mode), `merge_tool_outputs` will dedupe
    // by `call_id` and the EventMsg-derived message wins.
    //
    // We do skip `Message` and `Reasoning` ResponseItems unconditionally
    // because their EventMsg counterparts (`UserMessage`, `AgentMessage`,
    // `AgentReasoning`) are persisted in BOTH modes (policy.rs:137-153).
    if payload_type == "function_call" {
        return codex_function_call_message(payload, timestamp);
    }
    if payload_type == "function_call_output" {
        return codex_function_call_output_message(payload, timestamp);
    }
    // `custom_tool_call` / `custom_tool_call_output` are the modern Codex
    // shape for apply_patch (and other "custom" tools — see
    // `codex-rs/protocol/src/models.rs ResponseItem::CustomToolCall`). The
    // structural difference vs `function_call` is:
    //   * input arrives in an `input` field (raw text), NOT `arguments`
    //     (which is reserved for JSON-encoded structured args).
    //   * output is wrapped in a JSON envelope `{"output": "..."}`.
    // Reusing `codex_function_call_message` / `_output_message` after a
    // field rename keeps apply_patch / patch-diff rendering identical.
    if payload_type == "custom_tool_call" {
        return codex_custom_tool_call_message(payload, timestamp);
    }
    if payload_type == "custom_tool_call_output" {
        return codex_custom_tool_call_output_message(payload, timestamp);
    }
    if payload_type == "command_execution" {
        return codex_command_execution_message(payload, timestamp);
    }
    if payload_type == "plan" {
        return codex_plan_message(payload, timestamp);
    }
    if payload_type == "reasoning" {
        // Reasoning content is delivered via `EventMsg::AgentReasoning` in
        // both Limited and Extended modes — skip the ResponseItem variant
        // to avoid duplicate reasoning cards.
        return None;
    }
    if let Some(message) = codex_fallback_event_message(payload_type, payload, timestamp.clone()) {
        return Some(message);
    }
    if payload_type != "message" {
        return None;
    }
    // `Message` ResponseItems are also covered by `EventMsg::UserMessage` /
    // `AgentMessage` in both persistence modes. Skip them to avoid
    // duplicates. (Hook prompts use a different parser path, not handled
    // here.)
    None
}

fn codex_visible_assistant_markdown(markdown: &str) -> String {
    let mut visible_lines = markdown
        .lines()
        .map(|line| strip_codex_git_directives(line).trim_end().to_string())
        .collect::<Vec<_>>();
    while visible_lines.last().is_some_and(String::is_empty) {
        visible_lines.pop();
    }
    visible_lines.join("\n")
}

fn strip_codex_git_directives(line: &str) -> String {
    let mut visible = String::new();
    let mut remaining = line;
    while let Some(start) = remaining.find("::git-") {
        visible.push_str(&remaining[..start]);
        let directive = &remaining[start + 2..];
        let Some(open_brace) = directive.find('{') else {
            visible.push_str(&remaining[start..]);
            return visible;
        };
        let Some(close_brace) = directive[open_brace + 1..].find('}') else {
            visible.push_str(&remaining[start..]);
            return visible;
        };
        let close_brace = open_brace + 1 + close_brace;
        remaining = &directive[close_brace + 1..];
    }
    visible.push_str(remaining);
    visible
}

fn codex_plan_message(payload: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let text = payload.get("text").and_then(value_to_string)?;
    if text.trim().is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: "assistant".to_string(),
        text: text.trim().to_string(),
        timestamp,
        kind: kind::PLAN.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

fn codex_function_call_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    // Each branch produces the unified Termory header markdown — bold verb
    // + inline-code argument (or a fenced block for content that doesn't
    // fit on one line). Output, when present, is appended later by
    // `merge_tool_outputs` in a 4-backtick fence. Codex's official TUI
    // uses bold verbs (`"Ran"` / `"You ran"`) per exec_cell/render.rs:381.
    let name = payload
        .get("name")
        .and_then(value_to_string)
        .unwrap_or_else(|| "tool".to_string());
    let arguments = payload.get("arguments").and_then(value_to_string);
    let body = match name.as_str() {
        // Codex treats these 4 names as shell-exec calls, per
        // .audit-sources/codex/codex-rs/rollout-trace/src/tool_dispatch.rs:263:
        //   `"exec_command" | "local_shell" | "shell" | "shell_command"`
        // The TUI verb is `"Ran"` (or `"You ran"` for user-initiated shells)
        // — exec_cell/render.rs:381-385.
        "exec_command" | "shell" | "shell_command" | "local_shell" => {
            // Unified shell verb across Codex / Claude — both render as
            // `**Bash**({wrap_inline_code(command)})` so the user sees the
            // same shape regardless of platform. Codex's TUI uses "Ran"
            // (exec_cell/render.rs:381-385) but the per-platform card
            // badge already disambiguates the source; using one verb
            // makes scanning the transcript easier.
            let command = codex_command_from_arguments(arguments.as_deref())
                .or_else(|| arguments.clone())
                .unwrap_or_default();
            let trimmed = command.trim();
            if trimmed.is_empty() {
                "**Bash**".to_string()
            } else {
                format!("**Bash**({})", wrap_inline_code(trimmed))
            }
        }
        "apply_patch" => {
            let patch = codex_apply_patch_text(arguments.as_deref())
                .or_else(|| arguments.clone())
                .unwrap_or_default();
            let header = codex_patch_header(&codex_parse_patch_actions(&patch));
            format!("{header}\n\n```diff\n{}\n```", patch.trim())
        }
        // Codex `update_plan` tool — TUI shows a checkbox-style plan via
        // PlanUpdateCell (history_cell/plans.rs:138-194 emits `✔` for
        // completed, `□` for in_progress / pending). We map to GFM task
        // lists which remark-gfm renders with checkbox UI:
        //   - [x] completed
        //   - [~] in_progress  (`[~]` is a common extension marker)
        //   - [ ] pending
        "update_plan" => codex_update_plan_message(arguments.as_deref()),
        // Codex `view_image` tool — TUI shows `• Viewed Image` + indented
        // path (history_cell/patches.rs:63-72). We map to `**Viewed Image**
        // \`{path}\`` so the path stays monospaced.
        "view_image" => {
            let path = arguments
                .as_deref()
                .and_then(|args| {
                    serde_json::from_str::<Value>(args)
                        .ok()
                        .and_then(|v| v.get("path").and_then(value_to_string))
                })
                .unwrap_or_default();
            if path.is_empty() {
                "**Viewed Image**".to_string()
            } else {
                format!("**Viewed Image**({})", wrap_inline_code(&path))
            }
        }
        // Codex `write_stdin` tool (unified_exec/write_stdin.rs:WriteStdinArgs)
        // — args: `{session_id, chars, yield_time_ms?, max_output_tokens?}`.
        // TUI renders via `UnifiedExecInteractionCell` (history_cell/exec.rs:20-95):
        //   * Empty/absent `chars` → `• Waited for background terminal`
        //   * With `chars` → `↳ Interacted with background terminal · {cmd}\n  └ {stdin}`
        // We can't resolve `session_id` → `command_display` from a flat
        // rollout, so we collapse the "with input" case to
        // `**Stdin**({wrap_inline_code(chars)})` and the empty case to
        // `**Waited for background terminal**`.
        "write_stdin" => {
            let chars = arguments
                .as_deref()
                .and_then(|args| {
                    serde_json::from_str::<Value>(args)
                        .ok()
                        .and_then(|v| v.get("chars").and_then(Value::as_str).map(str::to_string))
                })
                .unwrap_or_default();
            if chars.trim().is_empty() {
                "**Waited for background terminal**".to_string()
            } else {
                format!("**Stdin**({})", wrap_inline_code(chars.trim()))
            }
        }
        _ => {
            // Generic fallback: `{name}({compact args})` as plain text.
            let args_summary = arguments
                .as_deref()
                .map(|args| compact(args, 200))
                .unwrap_or_default();
            if args_summary.is_empty() {
                name.clone()
            } else {
                format!("{name}({args_summary})")
            }
        }
    };
    Some(SessionMessage {
        role: "tool".to_string(),
        text: body,
        timestamp,
        kind: kind::TOOL_USE.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// Build the unified-markdown body for a Codex `update_plan` tool call.
/// Matches the structure of `PlanUpdateCell.display_lines`:
///   * `**Updated Plan**` header
///   * optional explanation in italic
///   * each plan step rendered as a GFM task list entry with status marker
fn codex_update_plan_message(arguments: Option<&str>) -> String {
    let mut text = String::from("**Updated Plan**");
    let Some(args) = arguments else {
        return text;
    };
    let Ok(value) = serde_json::from_str::<Value>(args.trim()) else {
        return text;
    };
    if let Some(explanation) = value
        .get("explanation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        text.push_str("\n\n*");
        // Escape stray `*` so italic span isn't broken mid-line.
        for c in explanation.chars() {
            if c == '*' || c == '_' {
                text.push('\\');
            }
            text.push(c);
        }
        text.push('*');
    }
    let plan = value.get("plan").and_then(Value::as_array);
    let Some(items) = plan else {
        return text;
    };
    if items.is_empty() {
        return text;
    }
    text.push_str("\n");
    for item in items {
        let step = item
            .get("step")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if step.is_empty() {
            continue;
        }
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let marker = match status {
            "completed" => "- [x]",
            "in_progress" => "- [~]",
            _ => "- [ ]",
        };
        text.push('\n');
        text.push_str(marker);
        text.push(' ');
        text.push_str(step);
    }
    text
}

fn codex_apply_patch_text(arguments: Option<&str>) -> Option<String> {
    let arguments = arguments?.trim();
    if arguments.is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    value
        .get("input")
        .and_then(value_to_string)
        .or_else(|| value.get("patch").and_then(value_to_string))
}

/// Codex apply_patch verb per file action. Mirrors
/// `.audit-sources/codex/codex-rs/tui/src/diff_render.rs:421-436` which
/// picks "Added" / "Deleted" / "Edited" depending on the FileChange
/// variant. We don't have the structured FileChange in Limited-mode
/// rollouts, so we scan the patch text for `*** Add File:` /
/// `*** Delete File:` / `*** Update File:` markers from Codex's
/// apply_patch grammar.
#[derive(Debug, PartialEq, Eq)]
struct CodexPatchAction {
    verb: &'static str,
    path: String,
    move_to: Option<String>,
}

fn codex_parse_patch_actions(patch_text: &str) -> Vec<CodexPatchAction> {
    let mut actions: Vec<CodexPatchAction> = Vec::new();
    let mut lines = patch_text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let (verb, rest) = if let Some(rest) = trimmed.strip_prefix("*** Add File:") {
            ("Added", rest)
        } else if let Some(rest) = trimmed.strip_prefix("*** Delete File:") {
            ("Deleted", rest)
        } else if let Some(rest) = trimmed.strip_prefix("*** Update File:") {
            ("Edited", rest)
        } else {
            continue;
        };
        let path = rest.trim().to_string();
        if path.is_empty() {
            continue;
        }
        let mut move_to = None;
        if verb == "Edited" {
            if let Some(next) = lines.peek() {
                if let Some(rest) = next.trim_start().strip_prefix("*** Move to:") {
                    move_to = Some(rest.trim().to_string());
                    lines.next();
                }
            }
        }
        actions.push(CodexPatchAction {
            verb,
            path,
            move_to,
        });
    }
    actions
}

/// Build the unified-markdown header for an apply_patch call. Matches
/// Codex's `render_changes_block` (diff_render.rs:415-437) for the verb
/// choice, but uses the unified `**Verb**(args)` shape so the card
/// reads the same as `**Bash**(cmd)` / `**Edit Notebook**(path)`:
///   * single-file patch → `**{Verb}**(\`{path}\`)` (with optional `→ {move_to}`)
///   * multi-file patch  → `**Edited**({N} files)`
fn codex_patch_header(actions: &[CodexPatchAction]) -> String {
    if actions.is_empty() {
        return "**Applied patch**".to_string();
    }
    if let [single] = actions {
        let path_segment = match &single.move_to {
            Some(target) => format!("{} → {}", single.path, target),
            None => single.path.clone(),
        };
        return format!("**{}**({})", single.verb, wrap_inline_code(&path_segment));
    }
    let count = actions.len();
    let noun = if count == 1 { "file" } else { "files" };
    format!("**Edited**({count} {noun})")
}

/// Codex `custom_tool_call` (`payload.type = "custom_tool_call"`) is the
/// modern shape for apply_patch and similar tools. Differs from
/// `function_call` in:
///   * `input` (raw text) instead of `arguments` (JSON-encoded args)
///   * No structured args wrapper around the patch body
/// Real-world example (`/Users/john/.codex/sessions/.../*.jsonl`):
///   `{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: ...","call_id":"..."}`
fn codex_custom_tool_call_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let name = payload.get("name").and_then(value_to_string)?;
    let input = payload.get("input").and_then(value_to_string);
    let body = match name.as_str() {
        "apply_patch" => {
            let patch = input.clone().unwrap_or_default();
            let header = codex_patch_header(&codex_parse_patch_actions(&patch));
            format!("{header}\n\n```diff\n{}\n```", patch.trim())
        }
        _ => {
            // Generic custom tool: bold name + compact input.
            let compact_input = input
                .as_deref()
                .map(|s| compact(s, 200))
                .unwrap_or_default();
            if compact_input.is_empty() {
                format!("**{name}**")
            } else {
                format!("**{name}**({compact_input})")
            }
        }
    };
    Some(SessionMessage {
        role: "tool".to_string(),
        text: body,
        timestamp,
        kind: kind::TOOL_USE.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// Codex `custom_tool_call_output` — paired with `custom_tool_call`.
/// The `output` field is a JSON-encoded string like
/// `{"output":"Success. Updated:\n..."}` (extracted from the same source
/// file referenced above). We unwrap the JSON envelope and emit a
/// tool_result message that `merge_tool_outputs` will fold back into
/// the matching tool_use card.
fn codex_custom_tool_call_output_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let raw_output = payload
        .get("output")
        .and_then(value_to_string)
        .or_else(|| payload.get("content").and_then(value_to_string))?;
    // Try JSON unwrap: many custom tool outputs use `{"output":"..."}`
    // / `{"text":"..."}` envelopes. Fall back to the raw string.
    let unwrapped = serde_json::from_str::<Value>(&raw_output)
        .ok()
        .and_then(|value| {
            value
                .get("output")
                .or_else(|| value.get("text"))
                .or_else(|| value.get("result"))
                .and_then(value_to_string)
        })
        .unwrap_or(raw_output);
    let trimmed = unwrapped.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: "tool".to_string(),
        text: trimmed.to_string(),
        timestamp,
        kind: kind::TOOL_RESULT.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

fn codex_function_call_output_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let text = payload
        .get("output")
        .and_then(value_to_string)
        .or_else(|| payload.get("content").and_then(value_to_string))?;
    let extracted = codex_parse_exec_output(&text);
    if extracted.raw.is_empty() && extracted.exit_code.is_none() {
        return None;
    }
    let is_error = extracted.exit_code.is_some_and(|code| code != 0);
    Some(SessionMessage {
        role: "tool".to_string(),
        text: extracted.raw,
        timestamp,
        kind: if is_error {
            kind::TOOL_ERROR.to_string()
        } else {
            kind::TOOL_RESULT.to_string()
        },
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: extracted.exit_code,
    })
}

/// Strip Codex's `ExecCommandToolOutput.response_text()` metadata wrapper.
/// The wrapper format (codex-rs/core/src/tools/context.rs:409-434) is:
///
/// ```text
/// [Chunk ID: X]
/// Wall time: X seconds
/// [Process exited with code X]
/// [Process running with session ID X]
/// [Original token count: X]
/// Output:
/// {raw command output}
/// ```
///
/// All sections are joined with single `\n`. The metadata is only useful
/// to the model; Codex's TUI never displays it in the main viewport (it
/// uses `aggregated_output` directly from `EventMsg::ExecCommandEnd`).
/// In Limited-mode rollouts the EventMsg is not persisted, so this
/// wrapper string is our only handle on the raw output.
///
/// Detection is line-by-line so we tolerate trailing whitespace on the
/// `Output:` delimiter and don't depend on an exact `\nOutput:\n`
/// substring match. Also pulls the exit code out of the metadata lines
/// (`Process exited with code N` / `Exit code: N`) so the caller can
/// surface a `**Error**` footer on non-zero exits.
#[derive(Debug, PartialEq, Eq, Default)]
struct CodexExecOutput {
    raw: String,
    exit_code: Option<i64>,
}

fn codex_parse_exec_output(text: &str) -> CodexExecOutput {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return CodexExecOutput::default();
    }
    let first_line = trimmed.lines().next().unwrap_or("");
    let looks_like_exec_wrapper = first_line.starts_with("Chunk ID:")
        || first_line.starts_with("Wall time:")
        || first_line.starts_with("Exit code:")
        || first_line.starts_with("Script ");
    if !looks_like_exec_wrapper {
        return CodexExecOutput {
            raw: trimmed.to_string(),
            exit_code: None,
        };
    }
    // The wrapper format guarantees a single `Output:` delimiter line
    // before the actual raw output. Split on it.
    let mut output_started = false;
    let mut output_lines: Vec<&str> = Vec::new();
    let mut exit_code: Option<i64> = None;
    for line in trimmed.lines() {
        if output_started {
            output_lines.push(line);
            continue;
        }
        // Extract exit code from one of the canonical metadata lines.
        // Codex's context.rs:420 emits `"Process exited with code {N}"`;
        // the older `unified_exec` format uses `"Exit code: {N}"`.
        if exit_code.is_none() {
            let trimmed_line = line.trim_start();
            if let Some(rest) = trimmed_line.strip_prefix("Process exited with code ") {
                exit_code = rest.trim().parse().ok();
            } else if let Some(rest) = trimmed_line.strip_prefix("Exit code:") {
                exit_code = rest.trim().parse().ok();
            }
        }
        if line.trim_end() == "Output:" {
            output_started = true;
        }
    }
    if !output_started {
        // Wrapper-looking content but missing the `Output:` marker
        // (truncated stream, etc.) — keep the original text and exit code.
        return CodexExecOutput {
            raw: trimmed.to_string(),
            exit_code,
        };
    }
    CodexExecOutput {
        raw: output_lines.join("\n").trim_end().to_string(),
        exit_code,
    }
}

fn codex_command_execution_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let command = payload.get("command").and_then(value_to_string)?;
    let mut lines = vec![format!("$ {command}")];
    if let Some(status) = payload.get("status").and_then(value_to_string) {
        let mut status_line = format!("status: {status}");
        if let Some(exit_code) = payload.get("exit_code").and_then(value_to_string) {
            status_line.push_str(" · exit ");
            status_line.push_str(&exit_code);
        }
        lines.push(status_line);
    }
    if let Some(output) = payload.get("aggregated_output").and_then(value_to_string) {
        let output = output.trim();
        if !output.is_empty() {
            lines.extend(output.lines().map(|line| format!("  {}", line.trim_end())));
        }
    }
    Some(SessionMessage {
        role: "tool".to_string(),
        text: lines.join("\n"),
        timestamp,
        kind: kind::COMMAND_EXECUTION.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// Dispatch by the inner `type` of a Codex `RolloutItem::EventMsg` payload.
/// Mirrors `ThreadHistoryBuilder::handle_event` (in
/// codex-rs/app-server-protocol/src/protocol/thread_history.rs:163-225) — the
/// same reducer Codex's TUI uses to rehydrate replayed sessions.
fn codex_event_msg_to_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    match event_type {
        "user_message" => codex_user_message_event(payload, timestamp),
        "agent_message" => codex_agent_message_event(payload, timestamp),
        "agent_reasoning" => codex_agent_reasoning_event(payload, timestamp),
        // Raw chain-of-thought (`AgentReasoningRawContentEvent`,
        // protocol.rs:2157). Codex's TUI renders this with the same
        // `ReasoningSummaryCell` style when `show_raw_agent_reasoning`
        // is on — and `replay_thread_item` ALSO emits it via
        // `on_agent_reasoning_delta` (replay.rs:118-121). Termory
        // surfaces it as a reasoning message using the same italic
        // blockquote format so it's distinguishable from regular text.
        "agent_reasoning_raw_content" => codex_agent_reasoning_event(payload, timestamp),
        "web_search_end" => codex_web_search_end_event(payload, timestamp),
        "mcp_tool_call_end" => codex_mcp_tool_call_end_event(payload, timestamp),
        "image_generation_end" => codex_image_generation_end_event(payload, timestamp),
        // `EventMsg::ViewImageToolCall` ({call_id, path}) — separate from
        // the `function_call` shape; Codex emits this when the agent
        // attaches a local image. patches.rs:63-72 renders it as
        // `• Viewed Image\n  └ {path}`. Map to the same markdown form
        // we use for the function_call variant for consistency.
        "view_image_tool_call" => codex_view_image_tool_call_event(payload, timestamp),
        "plan_update" => codex_plan_update_event(payload, timestamp),
        // `EventMsg::PatchApplyEnd` (Extended-mode rollouts only) carries
        // the structured `changes: HashMap<PathBuf, FileChange>` map and a
        // `success: bool`. Codex's TUI renders this via PatchHistoryCell
        // → `create_diff_summary` (per-file colored summary). We approximate
        // by listing each file with its verb (Added/Deleted/Edited) and
        // marking the apply as failed when `success: false`.
        "patch_apply_end" => codex_patch_apply_end_event(payload, timestamp),
        "context_compacted" => codex_context_compacted_event(payload, timestamp),
        // Lifecycle notices — Codex's TUI emits these as small one-line
        // info cards. Termory surfaces them as plain text so the
        // transcript carries the signal but they don't masquerade as
        // tool messages.
        "error" => codex_error_event(payload, timestamp),
        "turn_aborted" => codex_turn_aborted_event(payload, timestamp),
        "thread_rolled_back" => codex_thread_rolled_back_event(payload, timestamp),
        "entered_review_mode" => codex_entered_review_mode_event(payload, timestamp),
        "exited_review_mode" => codex_exited_review_mode_event(payload, timestamp),
        // `exec_command_end` is intentionally NOT handled yet. In Extended
        // mode it would supply better data than the ResponseItem path
        // (raw `aggregated_output` instead of `Chunk ID:...Output:...`),
        // but in Limited mode (the CLI default) it's simply not persisted,
        // so the ResponseItem path is authoritative. Mixing the two
        // currently produces duplicate tool cards for Extended-mode
        // sessions. Handle when we add proper call_id-based deduplication.
        _ => None,
    }
}

/// `EventMsg::WebSearchEnd` — Codex renders this via WebSearchCell with a
/// `Searched ...` header showing the query or, for structured actions
/// (`OpenPage` / `FindInPage`), the URL / "'pattern' in url" detail.
/// Mirrors `web_search_action_detail` at
/// `.audit-sources/codex/codex-rs/tui/src/history_cell/search.rs:13-38`.
fn codex_web_search_end_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let action = payload.get("action");
    let detail = action
        .and_then(codex_web_search_action_detail)
        .filter(|s| !s.is_empty());
    let fallback_query = payload
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let text = match detail.or(fallback_query) {
        Some(d) => format!("**Searched**({})", wrap_inline_code(&d)),
        None => "**Searched**".to_string(),
    };
    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: kind::TOOL_USE.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// Codex `WebSearchAction` → human-readable detail string per
/// `search.rs:13-38`. Returns:
///   * `Search` → `query` (falls back to first of `queries`, with `...`
///     suffix if multiple)
///   * `OpenPage` → `url`
///   * `FindInPage` → `'{pattern}' in {url}` / `'{pattern}'` / `{url}`
///   * `Other` / unknown → empty string
fn codex_web_search_action_detail(action: &Value) -> Option<String> {
    let ty = action.get("type").and_then(Value::as_str)?;
    match ty {
        "search" => {
            if let Some(q) = action
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(q.to_string());
            }
            let queries = action.get("queries").and_then(Value::as_array)?;
            let first = queries.first().and_then(Value::as_str).unwrap_or("").trim();
            if first.is_empty() {
                Some(String::new())
            } else if queries.len() > 1 {
                Some(format!("{first} ..."))
            } else {
                Some(first.to_string())
            }
        }
        "open_page" => Some(
            action
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        "find_in_page" => {
            let pattern = action
                .get("pattern")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let url = action
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Some(match (pattern, url) {
                (Some(p), Some(u)) => format!("'{p}' in {u}"),
                (Some(p), None) => format!("'{p}'"),
                (None, Some(u)) => u.to_string(),
                (None, None) => String::new(),
            })
        }
        _ => Some(String::new()),
    }
}

/// `EventMsg::McpToolCallEnd` — Codex renders via McpToolCallCell. The
/// invocation has `server`/`tool`/`arguments`; the result is either the
/// successful CallToolResult or an error string. Unified-markdown form:
/// `**MCP** {server}/{tool}` + result body (4-backtick fence) + optional
/// `**Error**` footer (added by merge_tool_outputs when kind=tool_error).
fn codex_mcp_tool_call_end_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let invocation = payload.get("invocation")?;
    let server = invocation
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool = invocation.get("tool").and_then(Value::as_str).unwrap_or("");
    if server.is_empty() && tool.is_empty() {
        return None;
    }
    // Codex TUI's `format_mcp_invocation` (mcp.rs:761-780) renders MCP
    // calls as `{server}.{tool}({args_json})` (dot separator, compact
    // args). The cell prefix is `• Called` (mcp.rs:133-140). We adapt
    // to the unified `**Verb**(args)` shape:
    //   * verb keeps `MCP` for cross-platform consistency with Claude's
    //     generic `mcp__server__tool` mapping
    //   * server/tool uses Codex's dot separator
    //   * args (when present) appear inline as compact JSON
    let args_str = invocation
        .get("arguments")
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .filter(|s| !s.is_empty() && s != "null" && s != "{}");
    let header = match args_str {
        Some(args) => format!("**MCP**({server}.{tool}, {})", compact(&args, 200)),
        None => format!("**MCP**({server}.{tool})"),
    };
    // `result` is either `{"Ok": CallToolResult}` or `{"Err": "..."}`
    // (Rust `Result` serialization). Inspect both arms.
    let result = payload.get("result");
    let (body, is_error) = match result {
        Some(r) if r.get("Err").is_some() => {
            let err = r.get("Err").and_then(Value::as_str).unwrap_or("");
            (err.to_string(), true)
        }
        Some(r) if r.get("Ok").is_some() => {
            let ok = r.get("Ok").expect("Ok arm");
            let is_err = ok.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let content_text = mcp_call_tool_result_text(ok);
            (content_text, is_err)
        }
        _ => (String::new(), false),
    };
    let trimmed = body.trim();
    let mut text = header;
    if !trimmed.is_empty() {
        text.push_str("\n\n````\n");
        text.push_str(trimmed);
        text.push_str("\n````");
    }
    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: if is_error {
            kind::TOOL_ERROR.to_string()
        } else {
            kind::TOOL_RESULT.to_string()
        },
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// Extract human-readable text from an MCP `CallToolResult`. Joins all
/// `content[].text` entries with newlines; image / resource items are
/// skipped because they aren't representable in plain markdown.
fn mcp_call_tool_result_text(result: &Value) -> String {
    let Some(items) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
}

/// `EventMsg::ImageGenerationEnd` — Codex shows "Generated Image:"
/// with the (optional) revised prompt and saved file URL.
/// Unified-markdown form: `**Generated Image** {revised_prompt}` +
/// optional `\nSaved: {path}` line.
fn codex_image_generation_end_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let revised = payload
        .get("revised_prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let saved = payload
        .get("saved_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if revised.is_none() && saved.is_none() {
        return None;
    }
    let mut text = String::from("**Generated Image**");
    if let Some(prompt) = revised {
        text.push('(');
        text.push_str(&wrap_inline_code(prompt));
        text.push(')');
    }
    if let Some(path) = saved {
        text.push_str("\n\nSaved: ");
        text.push_str(&wrap_inline_code(path));
    }
    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: kind::TOOL_USE.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// `EventMsg::ContextCompacted` — Codex shows a brief "Context compacted"
/// info line in the transcript. Unified-markdown form: italic notice.
fn codex_context_compacted_event(
    _payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    Some(SessionMessage {
        role: "system".to_string(),
        text: "*Context compacted*".to_string(),
        timestamp,
        kind: kind::COMPACTION.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// `EventMsg::Error` — Codex renders error notices via the chat widget.
/// `ErrorEvent.message` is human-readable; we surface it inline as a
/// `**Error**: {message}` system notice (bold marker + plain message
/// so the transcript shows what went wrong without losing the body).
fn codex_error_event(payload: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let message = payload.get("message").and_then(Value::as_str)?;
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: "system".to_string(),
        text: format!("**Error**: {trimmed}"),
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// `EventMsg::TurnAborted` — `reason` is one of `interrupted` / `budget_limited`.
/// Codex shows this as a small dim notice in the transcript when a turn
/// stops mid-flight.
fn codex_turn_aborted_event(payload: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("");
    let text = match reason {
        "interrupted" => "*Turn interrupted by user*".to_string(),
        "budget_limited" => "*Turn stopped — budget limit reached*".to_string(),
        other if !other.is_empty() => format!("*Turn aborted — {other}*"),
        _ => "*Turn aborted*".to_string(),
    };
    Some(SessionMessage {
        role: "system".to_string(),
        text,
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// `EventMsg::ThreadRolledBack` — Codex shows this when the conversation
/// has been rolled back N user turns.
fn codex_thread_rolled_back_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    // Real JSONL field is `num_turns` (confirmed by inspecting user's
    // `~/.codex/sessions/.../*.jsonl` records); `dropped_turn_count` /
    // `count` were guesses from earlier drafts. Per protocol.rs the
    // canonical field is `num_turns`.
    let dropped = payload
        .get("num_turns")
        .or_else(|| payload.get("dropped_turn_count"))
        .or_else(|| payload.get("count"))
        .and_then(Value::as_i64);
    let text = match dropped {
        Some(1) => "*Rolled back 1 turn*".to_string(),
        Some(n) if n > 0 => format!("*Rolled back {n} turns*"),
        _ => "*Conversation rolled back*".to_string(),
    };
    Some(SessionMessage {
        role: "system".to_string(),
        text,
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// `EventMsg::PatchApplyEnd` ({call_id, changes, success, stdout,
/// stderr}). Renders each FileChange entry as a `**{Verb}** {path}`
/// line and surfaces stderr inline when `success: false` so failed
/// patches show why they failed.
fn codex_patch_apply_end_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    // PatchApplyEndEvent.changes is a HashMap<PathBuf, FileChange> where
    // FileChange (protocol.rs) is `#[serde(tag = "type")]`:
    //   * Add { content }
    //   * Delete { content }
    //   * Update { unified_diff, move_path }
    // Earlier Termory only emitted the verb + path header; the full diff
    // / content was dropped — for `update` that's the unified diff text,
    // for `add` / `delete` it's the full file body. Match Codex TUI's
    // `PatchHistoryCell` (patches.rs:11-23 → create_diff_summary at
    // diff_render.rs) which shows per-file diff content.
    let changes = payload.get("changes").and_then(Value::as_object);
    let mut blocks: Vec<String> = Vec::new();
    if let Some(changes) = changes {
        let mut entries: Vec<(&String, &Value)> = changes.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (path, change) in entries {
            let kind = change.get("type").and_then(Value::as_str).unwrap_or("");
            let verb = match kind {
                "add" => "Added",
                "delete" => "Deleted",
                "update" => "Edited",
                _ => "Edited",
            };
            let display_path = match (kind, change.get("move_path").and_then(Value::as_str)) {
                ("update", Some(target)) if !target.is_empty() => {
                    format!("{path} → {target}")
                }
                _ => path.clone(),
            };
            // Body: unified_diff for `update`, content for `add`/`delete`,
            // empty otherwise. Wrap in ```diff for update (renders +/-
            // colored via hljs); for add/delete pick a language tag from
            // the file extension via `filetype_hint` so syntax highlights
            // when registered.
            let body_field = if kind == "update" {
                "unified_diff"
            } else {
                "content"
            };
            let body = change
                .get(body_field)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_end()
                .to_string();
            let header = format!("**{verb}**({})", wrap_inline_code(&display_path));
            if body.is_empty() {
                blocks.push(header);
            } else {
                let lang = if kind == "update" {
                    "diff"
                } else {
                    filetype_hint(path)
                };
                blocks.push(format!("{header}\n\n```{lang}\n{body}\n```"));
            }
        }
    }
    let success = payload
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut text = if blocks.is_empty() {
        "**Applied patch**".to_string()
    } else {
        blocks.join("\n\n")
    };
    if !success {
        let err = payload
            .get("stderr")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        text.push_str("\n\n");
        if let Some(e) = err {
            text.push_str("````\n");
            text.push_str(e);
            text.push_str("\n````\n\n");
        }
        text.push_str("**Error**");
    }
    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: if success {
            kind::TOOL_USE.to_string()
        } else {
            kind::TOOL_ERROR.to_string()
        },
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// `EventMsg::ViewImageToolCall` — `{call_id, path}` payload. Same
/// surface as the `function_call::view_image` ResponseItem variant
/// (codex_function_call_message): `**Viewed Image** \`{path}\``.
fn codex_view_image_tool_call_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let path = payload.get("path").and_then(Value::as_str)?;
    if path.is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: "tool".to_string(),
        text: format!("**Viewed Image**({})", wrap_inline_code(path)),
        timestamp,
        kind: kind::TOOL_USE.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

/// `EventMsg::PlanUpdate` — the plan EventMsg variant (carries
/// `UpdatePlanArgs` directly, not nested in `arguments`). Same surface
/// as the `update_plan` function_call but the payload IS the
/// UpdatePlanArgs.
fn codex_plan_update_event(payload: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let args = serde_json::to_string(payload).ok()?;
    let text = codex_update_plan_message(Some(&args));
    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: kind::PLAN.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// `EventMsg::EnteredReviewMode` — start of an automated code review.
fn codex_entered_review_mode_event(
    _payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    Some(SessionMessage {
        role: "system".to_string(),
        text: "*Entered review mode*".to_string(),
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// `EventMsg::ExitedReviewMode` — end of an automated review. The
/// payload's `review_output` carries the structured findings; we leave
/// it to a future iteration to render the structured form and only
/// emit the lifecycle notice here.
fn codex_exited_review_mode_event(
    _payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    Some(SessionMessage {
        role: "system".to_string(),
        text: "*Exited review mode*".to_string(),
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

fn codex_user_message_event(payload: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let message = payload.get("message").and_then(Value::as_str)?;
    if message.trim().is_empty() {
        return None;
    }
    // Codex TUI hides contextual user fragments (environment_context,
    // user_instructions, skill_instructions, ...). Termory surfaces
    // them as system notices so the transcript still shows what the
    // model received — the user wanted full history visibility.
    if let Some(tag) = codex_contextual_tag(message) {
        let inner = extract_xml_tag_value(message, tag).unwrap_or_else(|| message.to_string());
        let inner = inner.trim();
        let body = if inner.is_empty() {
            format!("*[Codex {tag}]*")
        } else {
            format!("*[Codex {tag}]*\n\n{inner}")
        };
        return Some(SessionMessage {
            role: "system".to_string(),
            text: body,
            timestamp,
            kind: kind::TEXT.to_string(),
            tool_use_id: None,
            exit_code: None,
        });
    }
    Some(SessionMessage {
        role: "user".to_string(),
        text: message.to_string(),
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// Return the wrapper tag name if `text` starts with one of Codex's
/// known contextual prefixes; otherwise `None`.
fn codex_contextual_tag(text: &str) -> Option<&'static str> {
    let trimmed = text.trim_start();
    for tag in [
        "environment_context",
        "user_instructions",
        "skill_instructions",
        "user_shell_command",
        "turn_aborted",
        "subagent_notification",
        "goal_context",
        "legacy_unified_exec_process_limit_warning",
        "legacy_apply_patch_exec_command_warning",
        "legacy_model_mismatch_warning",
    ] {
        let open = format!("<{tag}>");
        if trimmed.starts_with(&open) {
            return Some(tag);
        }
    }
    None
}

fn codex_agent_message_event(payload: &Value, timestamp: Option<String>) -> Option<SessionMessage> {
    let message = payload.get("message").and_then(Value::as_str)?;
    let visible = codex_visible_assistant_markdown(message);
    if visible.is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: "assistant".to_string(),
        text: visible,
        timestamp,
        kind: kind::TEXT.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

fn codex_agent_reasoning_event(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let text = payload.get("text").and_then(Value::as_str)?;
    let formatted = format_reasoning_body(text);
    if formatted.is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: "assistant".to_string(),
        text: formatted,
        timestamp,
        kind: kind::REASONING.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

/// Build a tool message from `EventMsg::ExecCommandEnd` (the canonical source
/// for shell-tool replay rendering in Codex's Extended mode). Codex's
/// TUI feeds these to `ExecCell` and renders:
///   `$ {command}` (bash-highlighted, magenta `$`)
///   {aggregated_output}     (dim, `└ ` / `    ` prefixed)
///   `✓` / `✗ ({exit})` • {duration}
///
/// Currently NOT dispatched from `codex_event_msg_to_message` — Extended
/// mode emits both this AND a `ResponseItem::FunctionCallOutput`, so
/// wiring this requires call_id-based dedup (see CLAUDE.md "Limited vs
/// Extended mode"). Kept as a reference implementation for that future
/// wiring; `#[allow(dead_code)]` silences the unused warning meanwhile.
#[allow(dead_code)]
fn codex_exec_command_end_message(
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let command = payload
        .get("command")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())?;
    let aggregated_output = payload
        .get("aggregated_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    let exit_code = payload.get("exit_code").and_then(Value::as_i64);

    let mut text = String::new();
    text.push_str("```bash\n$ ");
    text.push_str(&command);
    text.push_str("\n```");

    let output_trimmed = aggregated_output.trim_end();
    if !output_trimmed.is_empty() {
        text.push_str("\n\n````\n");
        text.push_str(output_trimmed);
        text.push_str("\n````");
    }

    if let Some(code) = exit_code {
        text.push_str("\n\n");
        if code == 0 {
            text.push('✓');
        } else {
            text.push_str(&format!("✗ exit {code}"));
        }
    }

    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: kind::COMMAND_EXECUTION.to_string(),
        tool_use_id: payload.get("call_id").and_then(value_to_string),
        exit_code: None,
    })
}

fn codex_command_from_arguments(arguments: Option<&str>) -> Option<String> {
    let arguments = arguments?.trim();
    if arguments.is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    value
        .get("cmd")
        .and_then(value_to_string)
        .or_else(|| value.get("command").and_then(value_to_string))
        .or_else(|| {
            value
                .get("argv")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(value_to_string)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|command| !command.trim().is_empty())
        })
}

fn codex_fallback_event_message(
    payload_type: &str,
    payload: &Value,
    timestamp: Option<String>,
) -> Option<SessionMessage> {
    let text = match payload_type {
        "hook_prompt" => {
            let fragments = payload.get("fragments").and_then(Value::as_array);
            if let Some(fragments) = fragments {
                let lines = fragments
                    .iter()
                    .filter_map(|fragment| fragment.get("text").and_then(value_to_string))
                    .map(|text| format!("hook prompt: {}", text.trim()))
                    .collect::<Vec<_>>();
                (!lines.is_empty()).then(|| lines.join("\n"))
            } else {
                payload
                    .get("text")
                    .and_then(value_to_string)
                    .map(|text| format!("hook prompt: {}", text.trim()))
            }
        }
        "file_change" => {
            let status = payload
                .get("status")
                .and_then(value_to_string)
                .unwrap_or_else(|| "unknown".to_string());
            let change_count = payload
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| changes.len())
                .unwrap_or(0);
            Some(format!("file changes: {status} · {change_count} changes"))
        }
        "mcp_tool_call" => {
            let server = payload
                .get("server")
                .and_then(value_to_string)
                .unwrap_or_default();
            let tool = payload
                .get("tool")
                .and_then(value_to_string)
                .or_else(|| payload.get("name").and_then(value_to_string))
                .unwrap_or_default();
            let status = payload
                .get("status")
                .and_then(value_to_string)
                .unwrap_or_else(|| "unknown".to_string());
            Some(format!("mcp tool: {server}/{tool} · {status}"))
        }
        "dynamic_tool_call" => {
            let tool = payload
                .get("tool")
                .and_then(value_to_string)
                .or_else(|| payload.get("name").and_then(value_to_string))
                .unwrap_or_else(|| "tool".to_string());
            let namespace = payload.get("namespace").and_then(value_to_string);
            let status = payload
                .get("status")
                .and_then(value_to_string)
                .unwrap_or_else(|| "unknown".to_string());
            let label = namespace
                .map(|namespace| format!("{namespace}/{tool}"))
                .unwrap_or(tool);
            Some(format!("tool: {label} · {status}"))
        }
        "collab_agent_tool_call" => {
            let tool = payload
                .get("tool")
                .and_then(value_to_string)
                .unwrap_or_else(|| "tool".to_string());
            let status = payload
                .get("status")
                .and_then(value_to_string)
                .unwrap_or_else(|| "unknown".to_string());
            Some(format!("agent tool: {tool} · {status}"))
        }
        "web_search" => payload
            .get("query")
            .and_then(value_to_string)
            .map(|query| format!("web search: {query}")),
        "image_view" => payload
            .get("path")
            .and_then(value_to_string)
            .map(|path| format!("image: {path}")),
        "image_generation" => {
            let status = payload
                .get("status")
                .and_then(value_to_string)
                .unwrap_or_else(|| "unknown".to_string());
            let mut text = format!("image generation: {status}");
            if let Some(path) = payload.get("saved_path").and_then(value_to_string) {
                text.push_str(" · ");
                text.push_str(&path);
            }
            Some(text)
        }
        "context_compaction" => Some("context compacted".to_string()),
        "entered_review_mode" => payload
            .get("review")
            .and_then(value_to_string)
            .map(|review| format!("review started: {review}")),
        "exited_review_mode" => payload
            .get("review")
            .and_then(value_to_string)
            .map(|review| format!("review finished: {review}")),
        _ => None,
    }?;
    Some(SessionMessage {
        role: "tool".to_string(),
        text,
        timestamp,
        kind: payload_type.to_string(),
        tool_use_id: None,
        exit_code: None,
    })
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn official_title_from_text(text: &str) -> Option<String> {
    let cleaned = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .trim_matches('"')
        .trim();
    if cleaned.len() < 2 {
        None
    } else {
        Some(cleaned.to_string())
    }
}

fn codex_display_title(text: &str) -> Option<String> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('{') && !line.starts_with('['))
        .filter(|line| !line.starts_with("cwd=") && !line.starts_with("session"))
        .filter(|line| !looks_like_codex_metadata(line));

    let first = lines.next()?;
    let cleaned = first.trim_matches('"').trim();
    if cleaned.len() < 2 {
        return None;
    }
    Some(compact(cleaned, 1_000))
}

fn looks_like_codex_metadata(text: &str) -> bool {
    text.starts_with("msg_")
        || text.starts_with("sess_")
        || (text.len() > 40 && text.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
}

// `compact` collapses whitespace + truncates with an ellipsis. Used only
// for session list title previews (where the platforms themselves
// truncate at display time); message bodies are not truncated anywhere
// in the unified format pipeline.
fn compact(value: &str, max: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max {
        normalized
    } else {
        format!("{}...", normalized.chars().take(max).collect::<String>())
    }
}

fn extract_json_string_field(text: &str, key: &str) -> Option<String> {
    extract_json_string_field_from(text, key, false)
}

fn extract_last_json_string_field(text: &str, key: &str) -> Option<String> {
    extract_json_string_field_from(text, key, true)
}

fn extract_json_string_field_from(text: &str, key: &str, last: bool) -> Option<String> {
    let patterns = [format!("\"{key}\":\""), format!("\"{key}\": \"")];
    let mut result = None;
    for pattern in patterns {
        let mut search_from = 0;
        while let Some(index) = text[search_from..].find(&pattern) {
            let value_start = search_from + index + pattern.len();
            let mut cursor = value_start;
            while cursor < text.len() {
                let byte = text.as_bytes()[cursor];
                if byte == b'\\' {
                    cursor += 2;
                    continue;
                }
                if byte == b'"' {
                    let raw = &text[value_start..cursor];
                    let value = serde_json::from_str::<String>(&format!("\"{raw}\"")).ok();
                    if !last {
                        return value;
                    }
                    result = value;
                    break;
                }
                cursor += 1;
            }
            search_from = cursor.saturating_add(1);
        }
    }
    result
}

fn normalize_time(value: String) -> Option<String> {
    if value.trim().is_empty() {
        return None;
    }
    if let Ok(ts) = value.parse::<i64>() {
        let millis = if ts > 10_000_000_000 { ts } else { ts * 1000 };
        return Local
            .timestamp_millis_opt(millis)
            .single()
            .map(|dt| dt.to_rfc3339());
    }
    DateTime::parse_from_rfc3339(&value)
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
        .ok()
        .or(Some(value))
}

fn first_existing_table(conn: &Connection, names: &[&str]) -> Result<String, Box<dyn Error>> {
    for name in names {
        let exists: i64 = conn.query_row(
            "select count(*) from sqlite_master where type='table' and name=?1",
            [name],
            |row| row.get(0),
        )?;
        if exists > 0 {
            return Ok((*name).to_string());
        }
    }
    Err(format!("none of these tables exist: {}", names.join(", ")).into())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, Box<dyn Error>> {
    let exists: i64 = conn.query_row(
        "select count(*) from sqlite_master where type='table' and name=?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(exists > 0)
}

/// Reject anything that isn't a bare alphanumeric/underscore SQL
/// identifier (no quoting, no dots, no semicolons, no spaces). Our
/// schemas only use plain names so this is strict enough to keep
/// `format!()`-style PRAGMA injection at `column_exists` safe.
fn is_safe_sql_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, Box<dyn Error>> {
    // Defense in depth: SQLite `PRAGMA table_info(<name>)` does not
    // accept bind parameters for the table identifier, so we have to
    // inject `<name>` into the SQL string. Today the only caller
    // passes the hardcoded literal `"threads"` — but reject anything
    // that isn't a bare SQL identifier so a future caller that pipes
    // a row value here can't accidentally introduce injection.
    if !is_safe_sql_identifier(table) {
        return Err(format!("rejecting unsafe table identifier: {table}").into());
    }
    let mut stmt = conn.prepare(&format!("pragma table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{lock_home, EnvVarGuard};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn opencode_dirs_honor_xdg_env() {
        // Reads/sets process-global XDG vars → serialize against the
        // providers opencode tests (which resolve the same dirs) and
        // restore on drop.
        let _g = lock_home();
        let home = Path::new("/tmp/fake-home");

        {
            let _c = EnvVarGuard::unset("XDG_CONFIG_HOME");
            let _d = EnvVarGuard::unset("XDG_DATA_HOME");
            assert_eq!(
                opencode_config_dir(home),
                home.join(".config").join("opencode")
            );
            assert_eq!(
                opencode_data_dir(home),
                home.join(".local").join("share").join("opencode")
            );
        }
        {
            let _c = EnvVarGuard::set("XDG_CONFIG_HOME", "/xc");
            let _d = EnvVarGuard::set("XDG_DATA_HOME", "/xd");
            assert_eq!(opencode_config_dir(home), PathBuf::from("/xc/opencode"));
            assert_eq!(opencode_data_dir(home), PathBuf::from("/xd/opencode"));
        }
        // Empty value is ignored (falls back to the home default).
        {
            let _c = EnvVarGuard::set("XDG_CONFIG_HOME", "");
            assert_eq!(
                opencode_config_dir(home),
                home.join(".config").join("opencode")
            );
        }
    }

    #[test]
    fn is_safe_sql_identifier_accepts_normal_names_and_rejects_injection() {
        // Accept plain identifiers we actually use
        assert!(is_safe_sql_identifier("threads"));
        assert!(is_safe_sql_identifier("session_message"));
        assert!(is_safe_sql_identifier("_internal"));
        assert!(is_safe_sql_identifier("Table42"));
        // Reject empty / leading digit / illegal chars / SQL fragments
        assert!(!is_safe_sql_identifier(""));
        assert!(!is_safe_sql_identifier("9threads"));
        assert!(!is_safe_sql_identifier("threads;DROP TABLE x"));
        assert!(!is_safe_sql_identifier("threads.name"));
        assert!(!is_safe_sql_identifier("\"threads\""));
        assert!(!is_safe_sql_identifier("threads--"));
        assert!(!is_safe_sql_identifier("threads OR 1=1"));
    }

    #[test]
    fn search_cache_evicts_oldest_when_full() {
        let mut cache = SearchCache::new(3);
        let mtime = SystemTime::now();
        let arc = || {
            Arc::new(SessionDetail {
                session: AppSession {
                    id: String::new(),
                    source: String::new(),
                    title: String::new(),
                    project: String::new(),
                    path: String::new(),
                    started_at: None,
                    updated_at: None,
                    message_count: 0,
                    archived: false,
                    preview: String::new(),
                    snippet: String::new(),
                    message_previews: Vec::new(),
                    tokens: None,
                    model: None,
                    daily_tokens: None,
                },
                messages: Vec::new(),
            })
        };
        cache.put((PathBuf::from("/a"), mtime), arc());
        cache.put((PathBuf::from("/b"), mtime), arc());
        cache.put((PathBuf::from("/c"), mtime), arc());
        assert_eq!(cache.len(), 3);
        // Capacity reached; next put evicts oldest ("/a").
        cache.put((PathBuf::from("/d"), mtime), arc());
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&(PathBuf::from("/a"), mtime)).is_none());
        assert!(cache.get(&(PathBuf::from("/d"), mtime)).is_some());
    }

    #[test]
    fn search_cache_get_promotes_to_mru() {
        let mut cache = SearchCache::new(3);
        let mtime = SystemTime::now();
        let arc = || {
            Arc::new(SessionDetail {
                session: AppSession {
                    id: String::new(),
                    source: String::new(),
                    title: String::new(),
                    project: String::new(),
                    path: String::new(),
                    started_at: None,
                    updated_at: None,
                    message_count: 0,
                    archived: false,
                    preview: String::new(),
                    snippet: String::new(),
                    message_previews: Vec::new(),
                    tokens: None,
                    model: None,
                    daily_tokens: None,
                },
                messages: Vec::new(),
            })
        };
        cache.put((PathBuf::from("/a"), mtime), arc());
        cache.put((PathBuf::from("/b"), mtime), arc());
        cache.put((PathBuf::from("/c"), mtime), arc());
        // Touch /a to make it MRU.
        let _ = cache.get(&(PathBuf::from("/a"), mtime));
        // Now /b is the least-recently-used. Inserting /d should evict /b.
        cache.put((PathBuf::from("/d"), mtime), arc());
        assert!(cache.get(&(PathBuf::from("/a"), mtime)).is_some());
        assert!(cache.get(&(PathBuf::from("/b"), mtime)).is_none());
    }

    #[test]
    fn search_cache_put_refreshes_existing_position() {
        let mut cache = SearchCache::new(2);
        let mtime = SystemTime::now();
        let arc = || {
            Arc::new(SessionDetail {
                session: AppSession {
                    id: String::new(),
                    source: String::new(),
                    title: String::new(),
                    project: String::new(),
                    path: String::new(),
                    started_at: None,
                    updated_at: None,
                    message_count: 0,
                    archived: false,
                    preview: String::new(),
                    snippet: String::new(),
                    message_previews: Vec::new(),
                    tokens: None,
                    model: None,
                    daily_tokens: None,
                },
                messages: Vec::new(),
            })
        };
        cache.put((PathBuf::from("/a"), mtime), arc());
        cache.put((PathBuf::from("/b"), mtime), arc());
        // Re-put /a — should refresh position, NOT evict anything.
        cache.put((PathBuf::from("/a"), mtime), arc());
        assert_eq!(cache.len(), 2);
        // Inserting /c now should evict /b (oldest), not /a.
        cache.put((PathBuf::from("/c"), mtime), arc());
        assert!(cache.get(&(PathBuf::from("/a"), mtime)).is_some());
        assert!(cache.get(&(PathBuf::from("/b"), mtime)).is_none());
    }

    #[test]
    fn get_session_rejects_id_outside_scan_set() {
        // Force a known empty-index state so we don't depend on any
        // previous scan_sessions() call leaving residue.
        refresh_session_path_index(&[]);
        let err =
            get_session("Memory", "any-id").expect_err("must refuse an id that wasn't scanned");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown session"),
            "expected unknown-session rejection, got: {msg}"
        );
    }

    #[test]
    fn get_session_looks_up_path_from_scan_index() {
        // Seed the index with a synthetic (source, id) → path entry,
        // then confirm get_session uses that path internally (we hit
        // a real-file-not-found error, which proves the lookup ran).
        let probe = AppSession {
            id: "synthetic-probe-id".to_string(),
            source: "Memory".to_string(),
            title: String::new(),
            project: String::new(),
            path: "/nonexistent/synthetic.md".to_string(),
            started_at: None,
            updated_at: None,
            message_count: 0,
            archived: false,
            preview: String::new(),
            snippet: String::new(),
            message_previews: Vec::new(),
            tokens: None,
            model: None,
            daily_tokens: None,
        };
        refresh_session_path_index(std::slice::from_ref(&probe));
        let err =
            get_session("Memory", "synthetic-probe-id").expect_err("file doesn't exist on disk");
        let msg = err.to_string();
        // Should be a real IO error, NOT the "unknown session" guard.
        assert!(
            !msg.contains("unknown session"),
            "lookup should have succeeded, got guard message: {msg}"
        );
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("termory-{name}-{nanos}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn codex_scans_threads_from_state_db_only() {
        let dir = TestDir::new("codex");
        let rollout = dir.path().join("rollout.jsonl");
        // User and assistant text both come from EventMsg variants in
        // Codex Limited-mode rollouts (policy.rs:137-153 — UserMessage /
        // AgentMessage are persisted in Limited mode). The duplicate
        // ResponseItem::Message records are present but ignored.
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-1","cwd":"/workspace/project"}}"# .to_string()
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"user_message","message":"Review backend changes"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Review backend changes"}]}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"agent_message","message":"Done"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"message","role":"assistant","content":[{"text":"Done"}]}}"#,
        )
        .unwrap();

        let db = dir.path().join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        // Schema mirrors codex-rs/state/migrations: id PK, rollout_path,
        // timestamps, source, cwd, title, first_user_message, preview,
        // archived. `preview` was added in migration 0032 and is part of the
        // official non-archived filter (push_thread_filters requires
        // `preview <> ''`).
        conn.execute_batch(
            "create table threads (
                id text,
                rollout_path text,
                created_at integer,
                updated_at integer,
                source text,
                cwd text,
                title text,
                first_user_message text,
                preview text,
                archived integer
            );",
        )
        .unwrap();
        conn.execute(
            "insert into threads values (?1, ?2, 1714521600000, 1714521700000, 'cli', ?3, ?4, ?5, 'preview', 0)",
            (
                "thread-1",
                rollout.display().to_string(),
                "/workspace/project",
                "Review backend changes",
                "fallback message",
            ),
        )
        .unwrap();
        // Same shape, source='atlas' — must also be picked up because Codex's
        // INTERACTIVE_SESSION_SOURCES includes 'atlas'.
        let rollout_atlas = dir.path().join("rollout-atlas.jsonl");
        fs::write(
            &rollout_atlas,
            r#"{"type":"session_meta","payload":{"id":"thread-atlas","cwd":"/workspace/project"}}"#,
        )
        .unwrap();
        conn.execute(
            "insert into threads values (?1, ?2, 1714521800000, 1714521900000, 'atlas', '/workspace/project', 'Atlas thread', 'first', 'preview', 0)",
            (
                "thread-atlas",
                rollout_atlas.display().to_string(),
            ),
        )
        .unwrap();
        // Same shape, but preview is empty — must be filtered out per
        // push_thread_filters' `preview <> ''` clause.
        conn.execute(
            "insert into threads values ('thread-no-preview', '/nonexistent', 1714521600000, 1714521700000, 'cli', '/workspace/project', 'No preview', 'first', '', 0)",
            (),
        )
        .unwrap();
        // Archived thread (archived = 1) — Termory surfaces it WITH a badge
        // (history browser), unlike `codex resume` which hides it.
        let rollout_arch = dir.path().join("rollout-archived.jsonl");
        fs::write(
            &rollout_arch,
            r#"{"type":"session_meta","payload":{"id":"thread-archived","cwd":"/workspace/project"}}"#,
        )
        .unwrap();
        conn.execute(
            "insert into threads values ('thread-archived', ?1, 1714521600000, 1714521700000, 'cli', '/workspace/project', 'Archived thread', 'first', 'preview', 1)",
            [rollout_arch.display().to_string()],
        )
        .unwrap();

        let sessions = scan_codex_state_db(&db).unwrap();
        // thread-1 (cli + non-empty preview) and thread-atlas (atlas) appear;
        // thread-no-preview is filtered out by `preview <> ''`.
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"thread-1"));
        assert!(
            ids.contains(&"thread-atlas"),
            "atlas-source threads must be surfaced (matches Codex INTERACTIVE_SESSION_SOURCES)"
        );
        assert!(
            !ids.contains(&"thread-no-preview"),
            "threads with empty preview must be filtered (matches push_thread_filters preview <> '')"
        );
        let main = sessions.iter().find(|s| s.id == "thread-1").unwrap();
        assert_eq!(main.title, "Review backend changes");
        assert_eq!(main.project, "/workspace/project");
        assert_eq!(main.message_count, 2);
        assert!(!main.archived, "a non-archived thread is not flagged");
        let archived = sessions.iter().find(|s| s.id == "thread-archived").unwrap();
        assert!(
            archived.archived,
            "archived = 1 threads are surfaced and flagged archived"
        );
    }

    #[test]
    fn codex_scans_legacy_state_db_without_preview_column() {
        // Older state_5.sqlite files predate migration 0032_threads_preview.sql.
        // Termory should still list their threads — it must detect the missing
        // column and skip the `preview <> ''` clause instead of returning a
        // SQLite "no such column" error to the user.
        let dir = TestDir::new("codex-legacy");
        let rollout = dir.path().join("rollout-legacy.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-legacy","cwd":"/workspace/legacy"}}"#,
        )
        .unwrap();

        let db = dir.path().join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        // Schema WITHOUT the preview column.
        conn.execute_batch(
            "create table threads (
                id text,
                rollout_path text,
                created_at integer,
                updated_at integer,
                source text,
                cwd text,
                title text,
                first_user_message text,
                archived integer
            );",
        )
        .unwrap();
        conn.execute(
            "insert into threads values ('thread-legacy', ?1, 1714521600000, 1714521700000, 'cli', '/workspace/legacy', 'Legacy thread', 'first', 0)",
            [rollout.display().to_string()],
        )
        .unwrap();

        let sessions = scan_codex_state_db(&db).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "thread-legacy");
    }

    #[test]
    fn codex_keeps_absolute_path_titles_from_state() {
        assert_eq!(
            codex_display_title(
                "/Applications/QClaw.app 请帮我逆向分析这个应用，移除InviteCodeModel前端邀请码验证"
            )
            .as_deref(),
            Some(
                "/Applications/QClaw.app 请帮我逆向分析这个应用，移除InviteCodeModel前端邀请码验证"
            )
        );
    }

    #[test]
    fn codex_parses_visible_thread_messages_only() {
        let dir = TestDir::new("codex-messages");
        let path = dir.path().join("rollout.jsonl");
        // Codex CLI persists rollout in Limited mode by default
        // (codex-rs/tui/src/app_server_session.rs: `persist_extended_history:
        // false`). In Limited mode the `EventMsg::ExecCommandEnd` event is
        // NOT persisted, so shell command + output come only from
        // `ResponseItem::FunctionCall` + `FunctionCallOutput`. Other
        // content (user / assistant / reasoning) comes from `EventMsg`
        // variants in BOTH modes — we prefer those and skip the
        // duplicate ResponseItem::Message / Reasoning entries.
        //
        // The `FunctionCallOutput.output` field carries Codex's
        // `ExecCommandToolOutput.response_text()` formatted string
        // (Chunk ID / Wall time / Output: / ...). Termory strips that
        // wrapper via `codex_extract_raw_command_output` so users see the
        // same raw command output Codex's TUI does.
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-2","cwd":"/workspace/project"}}"#.to_string()
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"user_message","message":"Fix the app"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the app"}]}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:01Z","payload":{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:02Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Chunk ID: 209810\nWall time: 0.0000 seconds\nProcess exited with code 0\nOriginal token count: 1\nOutput:\nsrc"}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:03Z","payload":{"type":"agent_message","message":"Done"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:03Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]}}"#,
        )
        .unwrap();

        let detail = parse_codex_session(&path, "thread-2").unwrap();
        assert_eq!(detail.session.title, "");
        // Visible flow: user (from event_msg) → tool (merged from
        // FunctionCall + FunctionCallOutput) → assistant (from
        // event_msg). The duplicate ResponseItem::Message records are
        // skipped.
        assert_eq!(detail.messages.len(), 3);
        assert_eq!(detail.messages[0].role, "user");
        assert_eq!(detail.messages[0].text, "Fix the app");
        assert_eq!(detail.messages[1].role, "tool");
        // Unified tool card: `**Ran** \`<cmd>\`` header (Codex's "Ran" verb
        // from exec_cell/render.rs:381-385) + raw output extracted from the
        // `Chunk ID:...Output:\nsrc` formatted wrapper (so the metadata
        // noise is gone, matching Codex TUI which uses aggregated_output).
        assert_eq!(
            detail.messages[1].text,
            "⏺ **Bash**(`ls`)\n\n````\nsrc\n````"
        );
        assert_eq!(detail.messages[2].role, "assistant");
        assert_eq!(detail.messages[2].text, "Done");
    }

    #[test]
    fn codex_view_image_function_call_uses_inline_path() {
        // function_call name="view_image" → `**Viewed Image** \`{path}\``,
        // matching Codex `new_view_image_tool_call` (patches.rs:63-72)
        // which renders `• Viewed Image\n  └ {display_path}`.
        let body = codex_function_call_message(
            &serde_json::json!({
                "type": "function_call",
                "name": "view_image",
                "arguments": "{\"path\":\"/tmp/shot.png\"}",
                "call_id": "img_1"
            }),
            None,
        )
        .unwrap();
        assert_eq!(body.text, "**Viewed Image**(`/tmp/shot.png`)");
        assert_eq!(body.kind, kind::TOOL_USE);
    }

    #[test]
    fn codex_update_plan_emits_gfm_task_list() {
        // Arguments shape matches Codex's `UpdatePlanArgs`
        // (plan_tool::PlanItemArg with snake_case status).
        let body = codex_update_plan_message(Some(
            r#"{"explanation":"Sequenced work","plan":[
                {"step":"Read repo layout","status":"completed"},
                {"step":"Fix tests","status":"in_progress"},
                {"step":"Ship release","status":"pending"}
            ]}"#,
        ));
        assert_eq!(
            body,
            "**Updated Plan**\n\n*Sequenced work*\n\n- [x] Read repo layout\n- [~] Fix tests\n- [ ] Ship release"
        );

        // No explanation, no steps — bare header.
        assert_eq!(codex_update_plan_message(Some("{}")), "**Updated Plan**");
    }

    #[test]
    fn gemini_thoughts_array_emits_reasoning_messages() {
        // Real Gemini JSONL format: `thoughts: [{subject, description, timestamp}, ...]`
        // sibling to `content`. Gemini TUI renders each via
        // `ThinkingMessage`. Termory surfaces them as separate reasoning
        // messages (italic-blockquote) — same convention as Claude /
        // Codex / OpenCode.
        let messages = gemini_thought_messages_from_value(&serde_json::json!({
            "type": "gemini",
            "timestamp": "2026-04-30T06:49:18Z",
            "content": "",
            "thoughts": [
                {
                    "subject": "Examining Workspace State",
                    "description": "Reviewing git status.\n\nWill check diff next.",
                    "timestamp": "2026-04-30T06:49:14Z"
                },
                {
                    "subject": "...",
                    "description": "noise only"
                }
            ]
        }));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].kind, kind::REASONING);
        // Per ThinkingMessage.tsx:84-93, the subject renders as bold +
        // italic and description lines render as plain italic. We map to
        // the unified blockquote shape: subject becomes a bold header
        // line (`> **subject**`) and description lines stay italic.
        assert_eq!(
            messages[0].text,
            "> **Examining Workspace State**\n> *Reviewing git status.*\n> *Will check diff next.*"
        );
        // Second thought has noise-only subject (`...`) but a real
        // description — subject is dropped, description rendered.
        assert_eq!(messages[1].text, "> *noise only*");
    }

    #[test]
    fn gemini_thought_normalizes_escaped_newlines_in_description() {
        // Persisted `description` strings sometimes carry literal `\\n`
        // escape sequences instead of real newlines. TUI applies
        // `normalizeEscapedNewlines` (textUtils.ts:168) before splitting,
        // and Termory mirrors so multi-paragraph reasoning renders as
        // multiple italic lines instead of one long line with visible
        // backslash-n.
        let messages = gemini_thought_messages_from_value(&serde_json::json!({
            "type": "gemini",
            "thoughts": [{
                "subject": "Plan",
                "description": "Step one.\\nStep two.\\r\\nStep three."
            }]
        }));
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].text,
            "> **Plan**\n> *Step one.*\n> *Step two.*\n> *Step three.*"
        );
    }

    #[test]
    fn gemini_system_notices_get_tui_icon_prefix() {
        // `info` / `error` / `warning` are persisted message types but
        // were previously rendered as plain text with no visual cue.
        // TUI uses `ℹ`, `✕`, `⚠` icons (InfoMessage / ErrorMessage /
        // WarningMessage); Termory now prefixes the same icons inside an
        // italic span so they stand out under the unified markdown
        // renderer.
        let m = gemini_message_from_value(&serde_json::json!({
            "type": "info",
            "content": "Request cancelled.",
            "timestamp": "2026-04-30T10:32:22Z"
        }))
        .unwrap();
        assert_eq!(m.role, "info");
        assert_eq!(m.text, "*ℹ Request cancelled.*");

        let m = gemini_message_from_value(&serde_json::json!({
            "type": "error",
            "content": "Tool failed"
        }))
        .unwrap();
        assert_eq!(m.text, "*✕ Tool failed*");

        let m = gemini_message_from_value(&serde_json::json!({
            "type": "warning",
            "content": "Approaching token limit"
        }))
        .unwrap();
        assert_eq!(m.text, "*⚠ Approaching token limit*");

        // Multi-line bodies wrap each subsequent line with a CommonMark
        // hard-break (`  \n`) so the italic span continues across lines
        // without a paragraph break terminating it.
        let m = gemini_message_from_value(&serde_json::json!({
            "type": "info",
            "content": "Line one\nLine two"
        }))
        .unwrap();
        assert_eq!(m.text, "*ℹ Line one  \nLine two*");
    }

    #[test]
    fn gemini_new_file_diff_unwraps_to_code_block() {
        // Per DiffRenderer.tsx:204-214 `isNewFile` predicate, when every
        // non-header line in the diff is an addition the TUI renders the
        // raw added content as a syntax-highlighted code block instead of
        // a unified diff. Termory mirrors: ```{lang}\n{adds}\n``` for
        // new files (lang from filename extension), ```diff\n...\n``` for
        // normal patches.
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t1",
                "name": "write_file",
                "displayName": "Write",
                "description": "greeting.py",
                "status": "success",
                "resultDisplay": {
                    "fileDiff": "--- a/greeting.py\n+++ b/greeting.py\n@@ -0,0 +1,2 @@\n+def hello():\n+    print('hi')\n",
                    "fileName": "greeting.py"
                }
            }]
        }));
        assert_eq!(
            msgs[0].text,
            "⏺ **Write**(greeting.py)\n\n```py\ndef hello():\n    print('hi')\n```"
        );

        // Normal patch (has a deletion) → ```diff fence (unchanged).
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t2",
                "name": "replace",
                "displayName": "Edit",
                "description": "greeting.py",
                "status": "success",
                "resultDisplay": {
                    "fileDiff": "--- a/greeting.py\n+++ b/greeting.py\n@@ -1 +1 @@\n-old\n+new\n",
                    "fileName": "greeting.py"
                }
            }]
        }));
        assert!(msgs[0].text.contains("```diff\n"));
    }

    #[test]
    fn codex_lifecycle_event_messages_render_inline_notices() {
        let make = |payload: serde_json::Value| codex_event_msg_to_message(&payload, None).unwrap();

        // Error event — surface the human-readable message.
        let msg = make(serde_json::json!({
            "type": "error",
            "message": "Network timeout"
        }));
        assert_eq!(msg.role, "system");
        assert_eq!(msg.text, "**Error**: Network timeout");

        // TurnAborted with reason=interrupted.
        let msg = make(serde_json::json!({
            "type": "turn_aborted",
            "turn_id": "t1",
            "reason": "interrupted"
        }));
        assert_eq!(msg.text, "*Turn interrupted by user*");

        // TurnAborted with budget reason.
        let msg = make(serde_json::json!({
            "type": "turn_aborted",
            "reason": "budget_limited"
        }));
        assert_eq!(msg.text, "*Turn stopped — budget limit reached*");

        // ThreadRolledBack — real JSONL field is `num_turns` (confirmed by
        // inspecting user's rollouts). The earlier guesses
        // `dropped_turn_count` / `count` still work as fallbacks but the
        // canonical field is checked first.
        let msg = make(serde_json::json!({
            "type": "thread_rolled_back",
            "num_turns": 3
        }));
        assert_eq!(msg.text, "*Rolled back 3 turns*");

        let msg = make(serde_json::json!({
            "type": "thread_rolled_back",
            "num_turns": 1
        }));
        assert_eq!(msg.text, "*Rolled back 1 turn*");

        // Fallback to legacy field names still works for forward-compat.
        let msg = make(serde_json::json!({
            "type": "thread_rolled_back",
            "dropped_turn_count": 2
        }));
        assert_eq!(msg.text, "*Rolled back 2 turns*");

        // Review mode lifecycle.
        let msg = make(serde_json::json!({"type": "entered_review_mode"}));
        assert_eq!(msg.text, "*Entered review mode*");

        let msg = make(serde_json::json!({"type": "exited_review_mode"}));
        assert_eq!(msg.text, "*Exited review mode*");
    }

    #[test]
    fn codex_write_stdin_function_call_renders_bg_terminal_card() {
        // Real-world write_stdin args from user's rollout:
        //   {"session_id":85980,"yield_time_ms":1000,"max_output_tokens":20000}
        // (no `chars` field → empty stdin → `Waited for background terminal`)
        let msg = codex_function_call_message(
            &serde_json::json!({
                "type": "function_call",
                "name": "write_stdin",
                "arguments": "{\"session_id\":85980,\"yield_time_ms\":1000}",
                "call_id": "ws_1"
            }),
            None,
        )
        .unwrap();
        assert_eq!(msg.text, "**Waited for background terminal**");

        // With chars → `**Stdin**(\`chars\`)`.
        let msg = codex_function_call_message(
            &serde_json::json!({
                "type": "function_call",
                "name": "write_stdin",
                "arguments": "{\"session_id\":85980,\"chars\":\"y\\n\"}",
                "call_id": "ws_2"
            }),
            None,
        )
        .unwrap();
        // chars are trim()-ed so trailing newline is dropped.
        assert_eq!(msg.text, "**Stdin**(`y`)");
    }

    #[test]
    fn codex_patch_apply_end_renders_unified_diff_per_file() {
        // PatchApplyEndEvent.changes is HashMap<PathBuf, FileChange>
        // where FileChange::Update { unified_diff, move_path } carries
        // the actual diff text — previously dropped by Termory which only
        // emitted `**Verb** {path}` headers.
        let msg = codex_patch_apply_end_event(
            &serde_json::json!({
                "type": "patch_apply_end",
                "call_id": "patch_1",
                "stdout": "",
                "stderr": "",
                "success": true,
                "changes": {
                    "/tmp/foo.ts": {
                        "type": "update",
                        "unified_diff": "@@ -1,2 +1,2 @@\n-old\n+new",
                        "move_path": null
                    }
                },
                "status": "completed"
            }),
            None,
        )
        .unwrap();
        // Should contain the unified diff text inside a ```diff fence.
        assert!(
            msg.text.contains("**Edited**(`/tmp/foo.ts`)"),
            "expected per-file header, got: {}",
            msg.text
        );
        assert!(
            msg.text
                .contains("```diff\n@@ -1,2 +1,2 @@\n-old\n+new\n```"),
            "expected unified_diff in ```diff fence; got: {}",
            msg.text
        );

        // FileChange::Add { content } — full file body in lang fence.
        let msg = codex_patch_apply_end_event(
            &serde_json::json!({
                "type": "patch_apply_end",
                "call_id": "patch_2",
                "stdout": "", "stderr": "", "success": true,
                "changes": {
                    "/tmp/new.py": {
                        "type": "add",
                        "content": "def hello():\n    pass"
                    }
                },
                "status": "completed"
            }),
            None,
        )
        .unwrap();
        assert!(msg.text.contains("**Added**(`/tmp/new.py`)"));
        assert!(msg.text.contains("```python\ndef hello():\n    pass\n```"));
    }

    #[test]
    fn format_reasoning_body_preserves_codex_bold_header() {
        // Codex `agent_reasoning` text often opens with `**Header**\n\nbody…`.
        // The first bold line should stay as bold in the blockquote (not
        // get its `*` chars escaped); subsequent lines stay italic-wrapped.
        let body = format_reasoning_body("**Identifying issue**\n\nReviewing the code path now.");
        assert_eq!(
            body,
            "> **Identifying issue**\n>\n> *Reviewing the code path now.*"
        );

        // Lone header (no body) keeps just the bold line.
        let body = format_reasoning_body("**Just a title**");
        assert_eq!(body, "> **Just a title**");

        // Text WITHOUT bold header — every line escaped + italic-wrapped
        // as before.
        let body = format_reasoning_body("plain body line\nsecond line");
        assert_eq!(body, "> *plain body line*\n> *second line*");

        // First line `**...**` with nested `**` inside doesn't qualify as
        // a clean header; falls back to italic-wrap so the nested `**`
        // get escaped instead of producing malformed bold.
        let body = format_reasoning_body("**not **clean** header**\nbody");
        assert!(body.starts_with("> *\\*\\*not"), "got: {body}");
    }

    #[test]
    fn claude_tool_use_with_error_result_shows_message_above_cross() {
        // Real-world Claude error pattern (e.g., InputValidationError
        // wrapped in `<tool_use_error>...`). The unified format flips the
        // status marker to `✗` at the front and adds an `Error:` prefix to
        // the fence body. Claude has no exit_code so the prefix is just
        // `Error:` without a number.
        let messages = parse_claude_messages_for_test(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}]},"timestamp":"2026-05-01T00:00:00Z"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","is_error":true,"content":"<tool_use_error>InputValidationError: Write failed due to missing parameter file_path</tool_use_error>"}]},"timestamp":"2026-05-01T00:00:01Z"}"#,
        );
        let tool = messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool card present");
        assert!(
            tool.text.starts_with("✗ **Bash**"),
            "failed tool card should start with ✗ marker; got: {}",
            tool.text
        );
        assert!(
            tool.text.contains("Error: InputValidationError"),
            "fence body should be `Error: ` + the message inline; got: {}",
            tool.text
        );
    }

    #[test]
    #[ignore = "manual diagnostic — times scan_sessions twice on the real HOME (cold vs warm cache)"]
    fn scan_sessions_timing_cold_vs_warm() {
        let t0 = std::time::Instant::now();
        let first = scan_sessions().expect("scan should succeed");
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let second = scan_sessions().expect("scan should succeed");
        let warm = t1.elapsed();
        println!(
            "cold: {cold:?} ({} records)  warm: {warm:?} ({} records)",
            first.records.len(),
            second.records.len()
        );
    }

    #[test]
    #[ignore = "manual diagnostic — dump Codex apply_patch fence text"]
    fn codex_real_session_dump_diff_fence() {
        let path = std::path::Path::new(
            "/Users/john/.codex/sessions/2026/04/28/rollout-2026-04-28T12-00-14-019dd23e-a50a-7c33-8df5-09eb6ae0f7e8.jsonl",
        );
        if !path.exists() {
            return;
        }
        let detail = parse_codex_session(path, "019dd23e-a50a-7c33-8df5-09eb6ae0f7e8").unwrap();
        for (i, m) in detail.messages.iter().enumerate() {
            if m.text.contains("```diff") {
                println!("--- msg #{i} kind={} ---\n{}\n", m.kind, m.text);
                if i > 30 {
                    break;
                }
            }
        }
    }

    #[test]
    fn gemini_parses_official_large_chat_session_without_panicking() {
        // Real Gemini chat-history fixture committed verbatim from
        // gemini-cli's own repo (`memory-tests/large-chat-session.json`,
        // ≈54MB, pre-anonymized by Google). Vendored at
        // `src-tauri/tests/fixtures/gemini-large-chat-session.json`
        // so this test is self-contained — no `.audit-sources/`
        // dependency, no opt-in `--ignored` flag.
        //
        // We bypass `parse_gemini_session_file` (which requires a
        // `.project_root` sibling) and drive the lower-level
        // `load_gemini_json_conversation` + `gemini_messages_from_values`
        // path directly. Path is anchored on `CARGO_MANIFEST_DIR` so
        // the test works regardless of the runner's cwd.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/gemini-large-chat-session.json");
        let path = fixture.as_path();

        let conversation = load_gemini_json_conversation(path, /*metadata_only=*/ false)
            .expect("load_gemini_json_conversation");

        // Metadata sanity — sessionId is non-empty, real chat has
        // hundreds of messages, the firstUserMessage was extracted.
        assert!(
            !conversation.session_id.is_empty(),
            "sessionId should be populated"
        );
        assert!(conversation.message_count > 100, "expected a real chat");
        assert!(conversation.has_user_or_assistant);
        assert!(
            conversation.first_user_message.is_some(),
            "firstUserMessage extraction should not silently fail"
        );

        // Per-message rendering — every record must produce a result
        // without panicking. Track counts by role / kind so a future
        // regression that silently drops (say) all `info` notices or
        // all reasoning fails the assertion below.
        let messages = gemini_messages_from_values(&conversation.messages);
        assert!(
            !messages.is_empty(),
            "gemini_messages_from_values dropped every record"
        );

        let mut user_count = 0;
        let mut assistant_count = 0;
        let mut reasoning_count = 0;
        let mut tool_count = 0;
        let mut info_count = 0;
        let mut error_count = 0;
        let mut warning_count = 0;
        for m in &messages {
            match (m.role.as_str(), m.kind.as_str()) {
                ("user", _) => user_count += 1,
                ("assistant", k) if k == kind::REASONING => reasoning_count += 1,
                ("assistant", _) => assistant_count += 1,
                ("tool", _) => tool_count += 1,
                ("info", _) => info_count += 1,
                ("error", _) => error_count += 1,
                ("warning", _) => warning_count += 1,
                _ => {}
            }
        }

        // The fixture is a multi-day real chat — we should see all
        // four major flavors. Pin loose lower bounds so additions to
        // the fixture don't break the assertion, but a regression that
        // wipes out a whole category (e.g. all reasoning messages) is
        // still caught.
        assert!(user_count > 0, "no user messages parsed");
        assert!(assistant_count > 0, "no assistant text messages parsed");
        assert!(reasoning_count > 0, "no thoughts → reasoning messages");
        assert!(tool_count > 0, "no toolCalls → tool messages");

        // Spot-check that the new TUI-aligned formatting actually
        // fires on real data: at least one tool card should carry the
        // `⏺` success marker, and any info/error/warning notice should
        // wear its TUI icon prefix.
        let any_tool_marker = messages
            .iter()
            .any(|m| m.role == "tool" && m.text.starts_with("⏺ "));
        assert!(any_tool_marker, "no tool card carries the ⏺ marker");

        for m in &messages {
            match m.role.as_str() {
                "info" => assert!(
                    m.text.starts_with("*ℹ "),
                    "info notice missing ℹ prefix: {:?}",
                    &m.text[..m.text.len().min(80)]
                ),
                "error" => assert!(
                    m.text.starts_with("*✕ "),
                    "error notice missing ✕ prefix: {:?}",
                    &m.text[..m.text.len().min(80)]
                ),
                "warning" => assert!(
                    m.text.starts_with("*⚠ "),
                    "warning notice missing ⚠ prefix: {:?}",
                    &m.text[..m.text.len().min(80)]
                ),
                _ => {}
            }
        }

        eprintln!(
            "parsed {} records → {} user / {} assistant / {} reasoning / {} tool / {} info / {} error / {} warning",
            conversation.message_count,
            user_count,
            assistant_count,
            reasoning_count,
            tool_count,
            info_count,
            error_count,
            warning_count
        );
    }

    #[test]
    #[ignore = "manual diagnostic — reads the user's real session file"]
    fn claude_real_session_dump_tool_messages() {
        let path = std::path::Path::new(
            "/Users/john/.claude/projects/-Users-john-Documents-termory/0d6abfa5-8939-4bd8-8437-30fbb3bc6f09.jsonl",
        );
        if !path.exists() {
            return;
        }
        let detail = parse_claude_session(path).unwrap();
        for (i, m) in detail.messages.iter().enumerate() {
            if m.text
                .contains("**Update**(`/Users/john/Documents/termory/src/main.tsx`)")
            {
                println!("--- msg #{i} kind={} ---", m.kind);
                println!("{}", &m.text[..m.text.len().min(400)]);
                println!();
            }
        }
    }

    fn parse_claude_messages_for_test(line1: &str, line2: &str) -> Vec<SessionMessage> {
        let mut out: Vec<SessionMessage> = Vec::new();
        for line in [line1, line2] {
            let v: Value = serde_json::from_str(line).unwrap();
            out.extend(claude_message_from_value(&v));
        }
        merge_tool_outputs(out)
    }

    #[test]
    fn claude_task_notification_collapses_to_summary_line() {
        // Background task notifications wrap a summary + extra metadata
        // (task-id / event / output-file). Claude Code's UI renders ONLY
        // the summary via `UserAgentNotificationMessage.tsx`. Termory
        // mirrors: `⏺ {summary}` and drops the rest.
        let formatted = claude_display_text(
            "<task-notification>\n<task-id>bd21wlkuh</task-id>\n<summary>Monitor event: \"Tauri dev startup progress\"</summary>\n<event>VITE v6.4.2 ready in 179 ms</event>\n</task-notification>",
        );
        assert_eq!(
            formatted.as_deref(),
            Some("⏺ Monitor event: \"Tauri dev startup progress\"")
        );
    }

    #[test]
    fn claude_image_content_block_emits_italic_notice() {
        // base64 source — surface mime type so the user knows an image
        // was attached (we can't render the actual bytes inline).
        let text = claude_text_blocks(&serde_json::json!([
            {"type":"text","text":"Look:"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},
            {"type":"text","text":"end"}
        ]))
        .join("\n");
        assert_eq!(text, "Look:\n*Image (image/png)*\nend");

        // url source.
        let text = claude_text_blocks(&serde_json::json!([
            {"type":"image","source":{"type":"url","url":"https://x.example/a.png"}}
        ]))
        .join("\n");
        assert_eq!(text, "*Image: https://x.example/a.png*");
    }

    #[test]
    fn claude_extra_tool_names_render_aligned_verbs() {
        // NotebookEdit (NotebookEditTool.ts userFacingName="Edit Notebook").
        assert_eq!(
            claude_tool_use_text(
                "NotebookEdit",
                &serde_json::json!({"notebook_path": "notes/a.ipynb"})
            ),
            Some("**Edit Notebook**(`notes/a.ipynb`)".to_string())
        );

        // ExitPlanMode / EnterPlanMode / AskUserQuestion / TodoWrite /
        // Task* / ToolSearch — Claude TUI suppresses these but Termory
        // renders every tool card because the transcript is the source
        // of truth (history browser, not live UI).
        assert_eq!(
            claude_tool_use_text(
                "ExitPlanMode",
                &serde_json::json!({"plan": "1. step\n2. step"})
            ),
            Some("**ExitPlanMode**\n\n1. step\n2. step".to_string())
        );
        assert_eq!(
            claude_tool_use_text("EnterPlanMode", &serde_json::json!({"message": "OK"})),
            Some("**EnterPlanMode**\n\nOK".to_string())
        );
        assert_eq!(
            claude_tool_use_text(
                "AskUserQuestion",
                &serde_json::json!({"questions": [{"question": "Which path?", "options": [{"label": "A"}, {"label": "B"}]}]})
            ),
            Some("**Ask User Question**\n\n⎿ Which path?\n  - A\n  - B".to_string())
        );
        assert_eq!(
            claude_tool_use_text("TodoWrite", &serde_json::json!({"todos": []})),
            Some("**Todos**\n\n⎿ (no todos)".to_string())
        );
        assert_eq!(
            claude_tool_use_text(
                "TodoWrite",
                &serde_json::json!({"todos": [
                    {"status": "completed", "content": "Read code"},
                    {"status": "in_progress", "content": "Patch parser"},
                    {"status": "pending", "content": "Run tests"}
                ]})
            ),
            Some("**Todos**\n\n✓ Read code\n~ Patch parser\n☐ Run tests".to_string())
        );
        // TaskCreate / ToolSearch hit the generic fallback and render as
        // `**ToolName**({json})` so the transcript records the call.
        assert_eq!(
            claude_tool_use_text("TaskCreate", &serde_json::json!({})),
            Some("**TaskCreate**".to_string())
        );
        assert_eq!(
            claude_tool_use_text("ToolSearch", &serde_json::json!({"query": "ls"})),
            Some("**ToolSearch**({\"query\":\"ls\"})".to_string())
        );

        // ReadMcpResource — verb is literal `readMcpResource` (camelCase
        // from Claude TUI userFacingName).
        assert_eq!(
            claude_tool_use_text(
                "ReadMcpResource",
                &serde_json::json!({"uri": "mcp://figma/file/x"})
            ),
            Some("**readMcpResource**(`mcp://figma/file/x`)".to_string())
        );

        // Generic MCP tool name `mcp__{server}__{tool}` → `**MCP**(s/t)`.
        assert_eq!(
            claude_tool_use_text("mcp__figma__inspect", &serde_json::json!({})),
            Some("**MCP**(figma/inspect)".to_string())
        );
    }

    #[test]
    fn claude_extended_thinking_emits_reasoning_message() {
        // Assistant content with a `thinking` block followed by `text`
        // and `tool_use`. Claude Code TUI renders thinking via
        // AssistantThinkingMessage (`∴ Thinking…` + dim markdown).
        // Termory surfaces it as a separate reasoning message using the
        // shared `> *content*` italic-blockquote format.
        let messages = claude_message_from_value(&serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Let me check the tests"},
                    {"type": "text", "text": "Done."},
                    {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {"command":"ls"}}
                ]
            },
            "timestamp": "2026-05-01T00:00:00Z"
        }));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].kind, kind::REASONING);
        assert_eq!(messages[0].text, "> *Let me check the tests*");
        assert_eq!(messages[1].kind, kind::TEXT);
        assert_eq!(messages[1].text, "Done.");
        assert_eq!(messages[2].kind, kind::TOOL_USE);
        assert!(messages[2].text.starts_with("**Bash**"));

        // Redacted thinking → placeholder text (not silently dropped).
        let messages = claude_message_from_value(&serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "redacted_thinking", "data": "encrypted-blob"}]
            },
            "timestamp": "2026-05-01T00:00:00Z"
        }));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, kind::REASONING);
        assert_eq!(messages[0].text, "> *[redacted thinking]*");
    }

    #[test]
    fn claude_attachment_messages_render_rendered_subtypes() {
        // `file` with text content → `Read {path} (N lines)`.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "type": "attachment",
                "attachment": {
                    "type": "file",
                    "displayPath": "src/main.tsx",
                    "content": {"type": "text", "file": {"numLines": 42}}
                }
            }),
            None,
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "Read `src/main.tsx` (42 lines)");
        assert_eq!(msgs[0].role, "system");

        // `file_unchanged`.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "attachment": {
                    "type": "file",
                    "displayPath": "src/cache.ts",
                    "content": {"type": "file_unchanged"}
                }
            }),
            None,
        );
        assert_eq!(msgs[0].text, "Read `src/cache.ts` (unchanged)");

        // `notebook`.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "attachment": {
                    "type": "file",
                    "displayPath": "notes.ipynb",
                    "content": {"type": "notebook", "file": {"cells": [1, 2, 3]}}
                }
            }),
            None,
        );
        assert_eq!(msgs[0].text, "Read `notes.ipynb` (3 cells)");

        // `compact_file_reference`.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "attachment": {"type": "compact_file_reference", "displayPath": "x.md"}
            }),
            None,
        );
        assert_eq!(msgs[0].text, "Referenced file `x.md`");

        // `skill_listing` with isInitial → drop.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "attachment": {"type": "skill_listing", "isInitial": true, "skillCount": 5}
            }),
            None,
        );
        assert!(msgs.is_empty());

        // `skill_listing` without isInitial → `N skills available`.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "attachment": {"type": "skill_listing", "skillCount": 3}
            }),
            None,
        );
        assert_eq!(msgs[0].text, "3 skills available");

        // `queued_command` with plain prompt → runs through claude_display_text.
        let msgs = claude_attachment_messages(
            &serde_json::json!({
                "attachment": {"type": "queued_command", "prompt": "Run tests"}
            }),
            None,
        );
        assert_eq!(msgs[0].text, "Run tests");

        // NULL_RENDERING subtypes (Claude TUI hides). Termory surfaces
        // each with the payload exposed — specialized formatters for
        // the common ones, JSON-dump fallback for the rest. The user
        // must be able to see what was recorded.
        let msgs = claude_attachment_messages(
            &serde_json::json!({"attachment": {"type": "date_change", "newDate": "2026-05-25"}}),
            None,
        );
        assert_eq!(msgs[0].text, "*[date changed to 2026-05-25]*");

        let msgs = claude_attachment_messages(
            &serde_json::json!({"attachment": {"type": "task_reminder", "itemCount": 0, "content": []}}),
            None,
        );
        assert_eq!(msgs[0].text, "**Task reminder** (0 items)");

        let msgs = claude_attachment_messages(
            &serde_json::json!({"attachment": {"type": "command_permissions", "allowedTools": []}}),
            None,
        );
        assert_eq!(msgs[0].text, "*[command permissions: none allowed]*");

        let msgs = claude_attachment_messages(
            &serde_json::json!({"attachment": {"type": "deferred_tools_delta", "addedNames": ["X", "Y"]}}),
            None,
        );
        assert_eq!(
            msgs[0].text,
            "**Deferred tools changed**\n\n⎿ Added (2): X, Y"
        );

        // Types we don't have a specialized formatter for fall through
        // to the JSON-dump fallback — the type name + payload survive.
        let msgs = claude_attachment_messages(
            &serde_json::json!({"attachment": {"type": "hook_success", "hookEvent": "PostToolUse"}}),
            None,
        );
        assert!(msgs[0].text.starts_with("*[attachment: hook_success]*"));
        assert!(msgs[0].text.contains("PostToolUse"));
    }

    #[test]
    fn claude_system_subtype_dispatch() {
        // turn_duration: `*※ Worked for {duration}*` (SystemTextMessage.tsx:342).
        let msg = claude_system_message(
            &serde_json::json!({"subtype": "turn_duration", "durationMs": 45269}),
            None,
        )
        .unwrap();
        assert_eq!(msg.text, "*※ Worked for 45.3s*");

        // away_summary: `*※ {content}*`.
        let msg = claude_system_message(
            &serde_json::json!({"subtype": "away_summary", "content": "Tauri running"}),
            None,
        )
        .unwrap();
        assert_eq!(msg.text, "*※ Tauri running*");

        // agents_killed: error notice.
        let msg =
            claude_system_message(&serde_json::json!({"subtype": "agents_killed"}), None).unwrap();
        assert_eq!(msg.text, "**Error** All background agents stopped");

        // compact_boundary: `---` divider + italic content.
        let msg = claude_system_message(
            &serde_json::json!({"subtype": "compact_boundary", "content": "Conversation compacted"}),
            None,
        )
        .unwrap();
        assert_eq!(msg.text, "---\n\n*Conversation compacted*\n\n---");

        // Unhandled subtypes drop silently (verbose-only / null in TUI).
        for sub in &["microcompact_boundary", "api_error", "unknown_xyz"] {
            let msg = claude_system_message(&serde_json::json!({"subtype": sub}), None);
            assert!(msg.is_none(), "subtype {sub} should drop");
        }

        // Duration formatter edge cases.
        assert_eq!(format_duration_short(0), "0.0s");
        assert_eq!(format_duration_short(59999), "60.0s");
        assert_eq!(format_duration_short(60000), "1m 0s");
        assert_eq!(format_duration_short(3_600_000), "1h 0m");
    }

    #[test]
    fn claude_unhide_hidden_tool_cards() {
        // Tools Claude TUI hides (`userFacingName: ''` +
        // `renderToolUseMessage: () => null`) are surfaced in Termory
        // because the transcript should reflect every recorded call.
        for name in &[
            "TodoWrite",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "ExitPlanModeV2",
            "TaskCreate",
            "TaskUpdate",
            "TaskGet",
            "TaskList",
            "TaskStop",
            "TaskOutput",
            "ToolSearch",
        ] {
            let card = claude_tool_use_text(name, &serde_json::json!({"foo": "bar"}));
            assert!(
                card.is_some(),
                "tool {name} should now produce a visible card"
            );
            assert!(
                card.as_ref().is_some_and(|t| !t.is_empty()),
                "tool {name} card should not be empty"
            );
        }
    }

    #[test]
    fn claude_display_text_unwraps_bash_and_special_markers() {
        // `<bash-stdout>...</bash-stdout>` → just the stdout content.
        assert_eq!(
            claude_display_text("<bash-stdout>hello\nworld</bash-stdout>"),
            Some("hello\nworld".to_string())
        );
        // `<bash-stderr>...</bash-stderr>` → just the stderr content.
        assert_eq!(
            claude_display_text("<bash-stderr>oops</bash-stderr>"),
            Some("oops".to_string())
        );
        // Both → stdout then stderr separated by blank line.
        assert_eq!(
            claude_display_text("<bash-stdout>ok</bash-stdout>\n<bash-stderr>warn</bash-stderr>"),
            Some("ok\n\nwarn".to_string())
        );
        // `<persisted-output>` wrapper inside `<bash-stdout>` is unwrapped.
        assert_eq!(
            claude_display_text(
                "<bash-stdout><persisted-output>/tmp/log.txt</persisted-output></bash-stdout>"
            ),
            Some("/tmp/log.txt".to_string())
        );

        // `(no content)` (NO_CONTENT_MESSAGE) — Termory surfaces as
        // italic placeholder so the empty message stays in the transcript.
        assert_eq!(
            claude_display_text("(no content)"),
            Some("*(no content)*".to_string())
        );

        // `[Request interrupted by user]` / `[Request interrupted by user for tool use]`
        // → italic notice.
        assert_eq!(
            claude_display_text("[Request interrupted by user]"),
            Some("*[Interrupted by user]*".to_string())
        );
        assert_eq!(
            claude_display_text("[Request interrupted by user for tool use]"),
            Some("*[Interrupted by user]*".to_string())
        );

        // `<tick>` wrapper — Termory unwraps the inner text instead of
        // dropping the whole message.
        assert_eq!(
            claude_display_text("<tick>ignored</tick>"),
            Some("ignored".to_string())
        );

        // `<local-command-caveat>` — Termory shows the caveat content
        // wrapped in an italic type tag.
        assert_eq!(
            claude_display_text("<local-command-caveat>hidden</local-command-caveat>"),
            Some("*[local-command-caveat]* hidden".to_string())
        );

        // Feature-gated wrappers (`<github-webhook-activity>`, etc.) —
        // Termory surfaces each as `*[tag-name]* {inner}` notice.
        assert_eq!(
            claude_display_text("<teammate-message>Hi from Bob</teammate-message>"),
            Some("*[teammate-message]* Hi from Bob".to_string())
        );

        // `<user-memory-input>...</user-memory-input>` → `\# {content}` memory line.
        assert_eq!(
            claude_display_text("<user-memory-input>remember this</user-memory-input>"),
            Some("\\# remember this".to_string())
        );

        // `({tool} completed with no output)` placeholder → `(No output)`.
        assert_eq!(
            claude_display_text("(Bash completed with no output)"),
            Some("(No output)".to_string())
        );
        assert_eq!(
            claude_display_text("(Read completed with no output)"),
            Some("(No output)".to_string())
        );
    }

    #[test]
    fn claude_agent_skill_edit_read_variants() {
        // Agent — subagent_type 'worker' / 'general-purpose' / missing →
        // "Agent" verb.
        for st in &["worker", "general-purpose"] {
            let out = claude_tool_use_text(
                "Task",
                &serde_json::json!({"description": "do thing", "subagent_type": st}),
            )
            .unwrap();
            assert_eq!(out, "**Agent**(`do thing`)");
        }
        // Custom subagent_type used verbatim.
        let out = claude_tool_use_text(
            "Task",
            &serde_json::json!({"description": "review", "subagent_type": "code-reviewer"}),
        )
        .unwrap();
        assert_eq!(out, "**code-reviewer**(`review`)");

        // Skill → `**Skill**({name})`.
        let out =
            claude_tool_use_text("Skill", &serde_json::json!({"skill": "format-md"})).unwrap();
        assert_eq!(out, "**Skill**(`format-md`)");

        // Edit with empty `old_string` → `**Create**(path)`.
        let out = claude_tool_use_text(
            "Edit",
            &serde_json::json!({"file_path": "/tmp/new.ts", "old_string": "", "new_string": "x"}),
        )
        .unwrap();
        assert_eq!(out, "**Create**(`/tmp/new.ts`)");
        // Edit with non-empty `old_string` → `**Update**`.
        let out = claude_tool_use_text(
            "Edit",
            &serde_json::json!({"file_path": "/tmp/x.ts", "old_string": "old", "new_string": "new"}),
        )
        .unwrap();
        assert_eq!(out, "**Update**(`/tmp/x.ts`)");
        // MultiEdit branch: first edit's old_string drives the verb.
        let out = claude_tool_use_text(
            "MultiEdit",
            &serde_json::json!({
                "file_path": "/tmp/x.ts",
                "edits": [{"old_string": "", "new_string": "first"}]
            }),
        )
        .unwrap();
        assert_eq!(out, "**Create**(`/tmp/x.ts`)");

        // Read with agent-output path → "Read agent output" verb + taskId.
        let out = claude_tool_use_text(
            "Read",
            &serde_json::json!({"file_path": "/tmp/proj-cache/tasks/abc123.output"}),
        )
        .unwrap();
        assert_eq!(out, "**Read agent output**(`abc123`)");
        // Plain read still uses "Read".
        let out =
            claude_tool_use_text("Read", &serde_json::json!({"file_path": "/src/foo.ts"})).unwrap();
        assert_eq!(out, "**Read**(`/src/foo.ts`)");

        // claude_agent_output_task_id heuristics.
        assert_eq!(
            claude_agent_output_task_id("/a/tasks/abc-1.output"),
            Some("abc-1".to_string())
        );
        assert_eq!(claude_agent_output_task_id("/a/tasks/no-ext.txt"), None);
        assert_eq!(
            claude_agent_output_task_id("/a/tasks/way-too-long-id-over-twenty.output"),
            None
        );
    }

    #[test]
    fn claude_mcp_tool_verbs_match_official_casing() {
        // ListMcpResources — literal lowerCamelCase verb.
        assert_eq!(
            claude_tool_use_text("ListMcpResources", &serde_json::json!({"server": "figma"})),
            Some("**listMcpResources**(`figma`)".to_string())
        );
        // McpAuth — whole label is the bold verb.
        assert_eq!(
            claude_tool_use_text("McpAuth", &serde_json::json!({"server": "github"})),
            Some("**github - authenticate (MCP)**".to_string())
        );
        // ReadMcpResource already covered in `claude_extra_tool_names_render_aligned_verbs`.
    }

    #[test]
    fn claude_format_structured_patch_summary_and_no_at_at_header() {
        // 2 added + 1 removed across one hunk.
        let body = claude_format_structured_patch(&serde_json::json!({
            "structuredPatch": [
                {
                    "oldStart": 1, "oldLines": 3, "newStart": 1, "newLines": 4,
                    "lines": [
                        " context",
                        "+added one",
                        "+added two",
                        "-removed one",
                        " context"
                    ]
                }
            ]
        }))
        .unwrap();
        // Output must include the `Added N lines, removed M lines` summary
        // (bold COUNTS only, matching FileEditToolUpdatedMessage.tsx:47-52)
        // on its own line, blank line, then the ```diff fence — NO `@@`
        // hunk header.
        assert!(
            body.starts_with("⎿ Added **2** lines, removed **1** line\n\n```diff\n"),
            "missing or wrong summary header; got:\n{body}"
        );
        assert!(!body.contains("@@ "), "should not include `@@` hunk header");
        assert!(body.ends_with("```"));

        // Multi-hunk: blank line separator between hunks but still no `@@`.
        let body = claude_format_structured_patch(&serde_json::json!({
            "structuredPatch": [
                {
                    "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1,
                    "lines": ["+a"]
                },
                {
                    "oldStart": 10, "oldLines": 1, "newStart": 10, "newLines": 0,
                    "lines": ["-b"]
                }
            ]
        }))
        .unwrap();
        assert!(body.starts_with("⎿ Added **1** line, removed **1** line\n\n```diff\n"));
        assert!(!body.contains("@@ "));
        assert!(
            body.contains("+a\n\n-b\n"),
            "hunks should be separated by blank line; got:\n{body}"
        );

        // Empty structuredPatch → None.
        assert_eq!(
            claude_format_structured_patch(&serde_json::json!({"structuredPatch": []})),
            None
        );
        // Missing structuredPatch → None.
        assert_eq!(claude_format_structured_patch(&serde_json::json!({})), None);
    }

    #[test]
    fn codex_event_msg_renders_web_search_mcp_image_compaction() {
        // EventMsg::WebSearchEnd → `**Searched** \`{query}\`` (call_id
        // surfaces as tool_use_id so the merge can match a potential
        // future tool_result, though web search doesn't have one).
        let msg = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "web_search_end",
                "call_id": "ws_1",
                "query": "react markdown rendering",
                "action": {"type": "search"}
            }),
            None,
        )
        .unwrap();
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.text, "**Searched**(`react markdown rendering`)");
        assert_eq!(msg.tool_use_id.as_deref(), Some("ws_1"));

        // EventMsg::McpToolCallEnd → header `**MCP** {server}/{tool}`
        // and body from CallToolResult.content[].text fenced in
        // 4-backticks. Successful result keeps kind=tool_result.
        let msg = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "mcp_tool_call_end",
                "call_id": "mcp_1",
                "invocation": {
                    "server": "figma",
                    "tool": "inspect",
                    "arguments": {}
                },
                "duration": {"secs":0,"nanos":0},
                "result": {
                    "Ok": {
                        "content": [{"type":"text","text":"frame: 12px"}],
                        "is_error": false
                    }
                }
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            msg.text,
            "**MCP**(figma.inspect)\n\n````\nframe: 12px\n````"
        );
        assert_eq!(msg.kind, kind::TOOL_RESULT);

        // McpToolCallEnd with Err(_) → kind=tool_error so merge_tool_outputs
        // can fold the `**Error**` footer in.
        let msg = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "mcp_tool_call_end",
                "call_id": "mcp_fail",
                "invocation": {"server":"x","tool":"y","arguments":{}},
                "duration":{"secs":0,"nanos":0},
                "result": {"Err":"connection refused"}
            }),
            None,
        )
        .unwrap();
        assert_eq!(msg.kind, kind::TOOL_ERROR);
        assert!(msg.text.contains("connection refused"));

        // ImageGenerationEnd → `**Generated Image** {prompt}` + saved path.
        let msg = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "image_generation_end",
                "call_id": "img_1",
                "status": "completed",
                "revised_prompt": "A red cat",
                "result": "ok",
                "saved_path": "/tmp/img.png"
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            msg.text,
            "**Generated Image**(`A red cat`)\n\nSaved: `/tmp/img.png`"
        );

        // ContextCompacted → system notice.
        let msg =
            codex_event_msg_to_message(&serde_json::json!({"type": "context_compacted"}), None)
                .unwrap();
        assert_eq!(msg.role, "system");
        assert_eq!(msg.text, "*Context compacted*");
        assert_eq!(msg.kind, kind::COMPACTION);
    }

    #[test]
    fn codex_mcp_includes_compact_args_when_non_empty() {
        // Codex `format_mcp_invocation` (mcp.rs:761-780) renders args
        // inline as compact JSON. Non-empty objects are surfaced; `{}`,
        // `null`, and missing fields drop the args entirely.
        let with_args = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "mcp_tool_call_end",
                "call_id": "mcp_args",
                "invocation": {
                    "server": "figma",
                    "tool": "inspect",
                    "arguments": {"frame": "Hero", "tokens": true}
                },
                "duration": {"secs":0,"nanos":0},
                "result": {"Ok": {"content": [], "is_error": false}}
            }),
            None,
        )
        .unwrap();
        assert!(
            with_args
                .text
                .starts_with("**MCP**(figma.inspect, {\"frame\":\"Hero\",\"tokens\":true})"),
            "got: {}",
            with_args.text
        );

        let empty_obj = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "mcp_tool_call_end",
                "call_id": "mcp_empty",
                "invocation": {"server": "x", "tool": "y", "arguments": {}},
                "duration": {"secs":0,"nanos":0},
                "result": {"Ok": {"content": [], "is_error": false}}
            }),
            None,
        )
        .unwrap();
        assert_eq!(empty_obj.text, "**MCP**(x.y)");
    }

    #[test]
    fn codex_web_search_renders_action_details() {
        // OpenPage → URL is the detail.
        let m = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "web_search_end",
                "call_id": "ws_open",
                "query": "",
                "action": {"type": "open_page", "url": "https://example.com/x"}
            }),
            None,
        )
        .unwrap();
        assert_eq!(m.text, "**Searched**(`https://example.com/x`)");

        // FindInPage with pattern + url → `'pattern' in url`.
        let m = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "web_search_end",
                "call_id": "ws_find",
                "query": "",
                "action": {
                    "type": "find_in_page",
                    "url": "https://example.com",
                    "pattern": "TODO"
                }
            }),
            None,
        )
        .unwrap();
        assert_eq!(m.text, "**Searched**(`'TODO' in https://example.com`)");

        // Search with `queries: [a, b]` (no `query`) → `a ...` suffix
        // because there are multiple queries.
        let m = codex_event_msg_to_message(
            &serde_json::json!({
                "type": "web_search_end",
                "call_id": "ws_multi",
                "query": "",
                "action": {
                    "type": "search",
                    "queries": ["alpha", "beta"]
                }
            }),
            None,
        )
        .unwrap();
        assert_eq!(m.text, "**Searched**(`alpha ...`)");
    }

    #[test]
    fn codex_failed_exec_appends_error_footer_below_output() {
        // Non-zero `Process exited with code` flips the leading status
        // marker to `✗` and prefixes the fence body with `Error: Exit
        // code N`. Aligns with Claude Code's TUI `⎿ Error: Exit code N`
        // line — same shape for both platforms.
        let dir = TestDir::new("codex-failed-exec");
        let path = dir.path().join("rollout.jsonl");
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-fail","cwd":"/workspace/project"}}"#.to_string()
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"user_message","message":"Run tests"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:01Z","payload":{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"cmd\":\"npm test\"}"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:02Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Wall time: 0.5 seconds\nProcess exited with code 1\nOutput:\nFAIL\ntrace"}}"#,
        )
        .unwrap();

        let detail = parse_codex_session(&path, "thread-fail").unwrap();
        let tool = detail
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool message present");
        // Error path: `⎿ Error: Exit code N` summary line outside the
        // fence (markdown paragraph — no monospace-width concerns for
        // `⎿`), stdout/stderr inside a plain 4-backtick fence below.
        assert_eq!(
            tool.text,
            "✗ **Bash**(`npm test`)\n\n⎿ Error: Exit code 1\n\n````\nFAIL\ntrace\n````"
        );
    }

    #[test]
    fn codex_failed_exec_with_empty_output_still_shows_exit_code() {
        // User-reported scenario (rollout JSONL line copied verbatim from
        // .codex/sessions/...): an `rg` call that finds no matches
        // emits `Process exited with code 1` and an empty Output: section.
        // Termory must report `✗ (1)` so the user knows the failure mode
        // even when stdout/stderr were empty.
        let dir = TestDir::new("codex-empty-error");
        let path = dir.path().join("rollout.jsonl");
        let function_call_payload = "exec_command";
        let function_call_args = "{\"cmd\":\"rg --files -g '*release-airouter.yml' -g '!*node_modules*' -g '!web/**/node_modules/**'\",\"workdir\":\"/Users/john/Documents/airouter\",\"yield_time_ms\":1000,\"max_output_tokens\":4000}";
        let function_call_output = "Chunk ID: 9593aa\\nWall time: 0.0000 seconds\\nProcess exited with code 1\\nOriginal token count: 0\\nOutput:\\n";
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-empty","cwd":"/workspace/project"}}"#.to_string()
                + "\n"
                + &format!(
                    r#"{{"type":"response_item","timestamp":"2026-05-20T13:39:48.676Z","payload":{{"type":"function_call","name":"{function_call_payload}","arguments":{:?},"call_id":"call_X"}}}}"#,
                    function_call_args
                )
                + "\n"
                + &format!(
                    r#"{{"type":"response_item","timestamp":"2026-05-20T13:39:48.834Z","payload":{{"type":"function_call_output","call_id":"call_X","output":"{function_call_output}"}}}}"#
                ),
        )
        .unwrap();

        let detail = parse_codex_session(&path, "thread-empty").unwrap();
        let tool = detail
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool message present");
        // Empty-output failure: no fence, just the bare `⎿ Error:` line.
        assert_eq!(
            tool.text,
            "✗ **Bash**(`rg --files -g '*release-airouter.yml' -g '!*node_modules*' -g '!web/**/node_modules/**'`)\n\n⎿ Error: Exit code 1"
        );
    }

    #[test]
    fn codex_shell_output_with_double_star_stays_in_fence() {
        // Regression: shell output that incidentally contains `**`
        // (e.g. `/**` JSDoc, `**kwargs`, markdown emphasis examples)
        // used to slip past the `already_markdown` heuristic in
        // merge_tool_outputs, so the body was emitted as a raw
        // markdown paragraph instead of wrapped in a code fence. The
        // browser then rendered it as h1/h2/p depending on line shape.
        // Strict rule: only `⎿ ` (structured-result builder marker)
        // or a leading ```fence are treated as already-markdown.
        let dir = TestDir::new("codex-doublestars");
        let path = dir.path().join("rollout.jsonl");
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-stars","cwd":"/workspace/project"}}"#.to_string()
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"user_message","message":"show jsdoc"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:01Z","payload":{"type":"function_call","call_id":"call_X","name":"shell","arguments":"{\"cmd\":\"head -3 a.js\"}"}}"#
                + "\n"
                + r#"{"type":"response_item","timestamp":"2026-05-01T00:00:02Z","payload":{"type":"function_call_output","call_id":"call_X","output":"Chunk ID: 1\nWall time: 0.0 seconds\nProcess exited with code 0\nOutput:\n308-/**\n309- * doc\n310- */"}}"#,
        )
        .unwrap();

        let detail = parse_codex_session(&path, "thread-stars").unwrap();
        let tool = detail
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool message present");
        // The `/**` in line 1 must NOT cause the body to skip fence
        // wrapping — the whole output goes inside a 4-backtick fence
        // so the browser renders monospace, not paragraphs.
        assert_eq!(
            tool.text,
            "⏺ **Bash**(`head -3 a.js`)\n\n````\n308-/**\n309- * doc\n310- */\n````"
        );
    }

    #[test]
    fn codex_surfaces_environment_context_as_system_notice() {
        let dir = TestDir::new("codex-environment-context");
        let path = dir.path().join("rollout.jsonl");
        // EventMsg::UserMessage carries the user input verbatim — Codex's
        // TUI hides `<environment_context>` wrappers because they are
        // synthetic prefixes added before sending to the model, not real
        // user text. Termory now SURFACES them as `*[Codex {tag}]*`
        // system notices (see `codex_contextual_tag` +
        // `codex_user_message_event`); user wanted full transcript
        // visibility regardless of TUI suppression.
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-env","cwd":"/workspace/project"}}"#.to_string()
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"user_message","message":"<environment_context>\n  <cwd>/Users/john/Documents</cwd>\n  <shell>zsh</shell>\n  <current_date>2026-05-15</current_date>\n  <timezone>Asia/Shanghai</timezone>\n</environment_context>"}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:01Z","payload":{"type":"user_message","message":"Build the app"}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:02Z","payload":{"type":"agent_message","message":"Done"}}"#,
        )
        .unwrap();

        let detail = parse_codex_session(&path, "thread-env").unwrap();
        assert_eq!(detail.session.title, "");
        // Termory shows the contextual prefix as a system notice (was
        // dropped in earlier versions to match Codex TUI). User asked
        // for full transcript visibility.
        assert_eq!(detail.session.message_count, 3);
        assert_eq!(detail.messages.len(), 3);
        assert_eq!(detail.messages[0].role, "system");
        assert!(detail.messages[0]
            .text
            .starts_with("*[Codex environment_context]*"));
        assert_eq!(detail.messages[1].text, "Build the app");
        assert_eq!(detail.messages[2].text, "Done");
    }

    #[test]
    fn codex_surfaces_official_contextual_user_fragments() {
        let dir = TestDir::new("codex-contextual-fragments");
        let path = dir.path().join("rollout.jsonl");
        // Same contextual-fragment filter applied to EventMsg::UserMessage:
        // synthetic prefixes (`<user_instructions>`, `<skill_instructions>`,
        // etc.) never reach the display, while normal user input does.
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-fragments","cwd":"/workspace/project"}}"#.to_string()
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:00Z","payload":{"type":"user_message","message":"<user_instructions>\nUse repo conventions.\n</user_instructions>"}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:01Z","payload":{"type":"user_message","message":"<skill_instructions>\nTest carefully.\n</skill_instructions>"}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:02Z","payload":{"type":"user_message","message":"Fix contextual filtering"}}"#
                + "\n"
                + r#"{"type":"event_msg","timestamp":"2026-05-01T00:00:03Z","payload":{"type":"agent_message","message":"Done"}}"#,
        )
        .unwrap();

        let detail = parse_codex_session(&path, "thread-fragments").unwrap();
        assert_eq!(detail.session.title, "");
        // Termory shows BOTH contextual fragments as system notices
        // (was dropped earlier to match Codex TUI).
        assert_eq!(detail.session.message_count, 4);
        assert_eq!(detail.messages.len(), 4);
        assert!(detail.messages[0]
            .text
            .starts_with("*[Codex user_instructions]*"));
        assert!(detail.messages[1]
            .text
            .starts_with("*[Codex skill_instructions]*"));
        assert_eq!(detail.messages[2].text, "Fix contextual filtering");
        assert_eq!(detail.messages[3].text, "Done");
    }

    // Removed: `codex_renders_resume_picker_tool_events`. The fixture used
    // `command_execution` and `mcp_tool_call` payload types under
    // `response_item` — neither exists in Codex's real `ResponseItem` enum
    // (codex-rs/protocol/src/models.rs: variants are `Message`,
    // `Reasoning`, `FunctionCall`, `FunctionCallOutput`, `LocalShellCall`,
    // `CustomToolCall`, `WebSearchCall`, `ImageGenerationCall`, ...).
    // Tool calls reach the TUI via `EventMsg` variants (PatchApplyEnd /
    // McpToolCallEnd / WebSearchEnd / ExecCommandEnd), not as
    // ResponseItem subtypes. The old test asserted a Termory-invented
    // format that never appears in real Codex sessions.

    #[test]
    fn codex_parses_exec_output_extracts_raw_and_exit_code() {
        // Exact shape Codex emits via ExecCommandToolOutput.response_text()
        // (codex-rs/core/src/tools/context.rs:409-434). Termory must strip
        // everything up to and including the `Output:` delimiter line, AND
        // record the exit code from the `Process exited with code N` line
        // so a non-zero exit can surface as a `**Error**` footer.
        let parsed = codex_parse_exec_output(
            "Chunk ID: 79f6dd\nWall time: 0.0000 seconds\nProcess exited with code 0\nOriginal token count: 0\nOutput:\ndiff --git a/foo b/foo\n+changed",
        );
        assert_eq!(parsed.raw, "diff --git a/foo b/foo\n+changed");
        assert_eq!(parsed.exit_code, Some(0));

        let parsed = codex_parse_exec_output(
            "Wall time: 0.5000 seconds\nProcess exited with code 1\nOutput:\nboom",
        );
        assert_eq!(parsed.raw, "boom");
        assert_eq!(parsed.exit_code, Some(1));

        // Older single-chunk wrapper with `Exit code:` prefix.
        let parsed = codex_parse_exec_output("Exit code: 2\nWall time: 0.1\nOutput:\nnope");
        assert_eq!(parsed.raw, "nope");
        assert_eq!(parsed.exit_code, Some(2));

        // Non-exec output (e.g., raw mcp result) passes through unchanged
        // and reports no exit code.
        let parsed = codex_parse_exec_output("mcp call returned: ok");
        assert_eq!(parsed.raw, "mcp call returned: ok");
        assert_eq!(parsed.exit_code, None);

        // Empty output after the delimiter — keep the delimiter trim.
        let parsed = codex_parse_exec_output("Chunk ID: 1\nWall time: 0.0000 seconds\nOutput:\n");
        assert_eq!(parsed.raw, "");
    }

    #[test]
    fn codex_apply_patch_header_uses_per_file_verb() {
        // Single-file add → `**Added** {path}` (matches Codex's
        // render_changes_block, diff_render.rs:421-431).
        let actions = codex_parse_patch_actions(
            "*** Begin Patch\n*** Add File: src/new.rs\n+content\n*** End Patch",
        );
        assert_eq!(
            actions,
            vec![CodexPatchAction {
                verb: "Added",
                path: "src/new.rs".to_string(),
                move_to: None,
            }]
        );
        assert_eq!(codex_patch_header(&actions), "**Added**(`src/new.rs`)");

        // Single-file edit with move → `**Edited** old → new`.
        let actions = codex_parse_patch_actions(
            "*** Update File: a/old.rs\n*** Move to: a/new.rs\n@@\n-x\n+y",
        );
        assert_eq!(
            actions,
            vec![CodexPatchAction {
                verb: "Edited",
                path: "a/old.rs".to_string(),
                move_to: Some("a/new.rs".to_string()),
            }]
        );
        assert_eq!(
            codex_patch_header(&actions),
            "**Edited**(`a/old.rs → a/new.rs`)"
        );

        // Delete.
        let actions = codex_parse_patch_actions("*** Delete File: tmp/out.txt");
        assert_eq!(codex_patch_header(&actions), "**Deleted**(`tmp/out.txt`)");

        // Multi-file → counts collapse to a single `**Edited** N files`.
        let actions = codex_parse_patch_actions(
            "*** Update File: a.rs\n@@\n*** Add File: b.rs\n*** Delete File: c.rs",
        );
        assert_eq!(actions.len(), 3);
        assert_eq!(codex_patch_header(&actions), "**Edited**(3 files)");

        // No recognized markers → generic fallback.
        let actions = codex_parse_patch_actions("diff --git a/x b/x\n...");
        assert!(actions.is_empty());
        assert_eq!(codex_patch_header(&actions), "**Applied patch**");
    }

    #[test]
    fn wrap_inline_code_picks_safe_delimiter_per_commonmark() {
        // No backticks in content — single backticks suffice.
        assert_eq!(wrap_inline_code("ls -la"), "`ls -la`");
        // Content has a backtick run of length 1, AND ends with a backtick,
        // so CommonMark 6.1 forces both: a length-2 delimiter AND padding
        // spaces between delimiter and content.
        assert_eq!(wrap_inline_code("echo `date`"), "`` echo `date` ``");
        // Content starting/ending with backtick gets padding spaces per
        // CommonMark spec section 6.1.
        assert_eq!(wrap_inline_code("`code`"), "`` `code` ``");
    }

    #[test]
    fn codex_strips_official_git_action_directives_from_assistant_markdown() {
        let text = codex_visible_assistant_markdown(
            "Done ::git-push{cwd=\"/workspace/project\" branch=\"main\"} next",
        );
        assert_eq!(text, "Done  next");

        let text = codex_visible_assistant_markdown(
            "::git-diff{cwd=\"/workspace/project\"}\nVisible\n::git-commit{message=\"ship\"}",
        );
        assert_eq!(text, "\nVisible");
    }

    #[test]
    fn claude_parses_project_jsonl_session() {
        let dir = TestDir::new("claude");
        let path = dir
            .path()
            .join("12345678-1234-1234-1234-123456789abc.jsonl");
        fs::write(
            &path,
            r#"{"sessionId":"12345678-1234-1234-1234-123456789abc","cwd":"/workspace/claude","timestamp":"2026-05-01T00:00:00Z","type":"user","message":{"role":"user","content":"Fix the login flow"}}"#,
        )
        .unwrap();

        let detail = parse_claude_session(&path).unwrap();
        assert_eq!(detail.session.id, "12345678-1234-1234-1234-123456789abc");
        assert_eq!(detail.session.project, "/workspace/claude");
        assert_eq!(detail.session.title, "Fix the login flow");
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.session.message_count, 1);
    }

    #[test]
    fn claude_merges_tool_use_with_matching_tool_result() {
        // tool_use is in an assistant message; the matching tool_result lives
        // in the next user message and references the original tool_use.id.
        // Termory should fold the result back into the tool_use card so the
        // user sees one "Bash(cmd) + output" block instead of two cards.
        let dir = TestDir::new("claude-tool-merge");
        let path = dir
            .path()
            .join("12345678-1234-1234-1234-123456789ac1.jsonl");
        fs::write(
            &path,
            r#"{"sessionId":"12345678-1234-1234-1234-123456789ac1","cwd":"/workspace/claude","timestamp":"2026-05-01T00:00:00Z","type":"user","message":{"role":"user","content":"list files"}}"#.to_string()
                + "\n"
                + r#"{"sessionId":"12345678-1234-1234-1234-123456789ac1","timestamp":"2026-05-01T00:00:01Z","type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Running."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#
                + "\n"
                + r#"{"sessionId":"12345678-1234-1234-1234-123456789ac1","timestamp":"2026-05-01T00:00:02Z","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"src\nREADME.md"}]}}"#,
        )
        .unwrap();

        let detail = parse_claude_session(&path).unwrap();
        // user prompt + assistant text + merged tool card = 3 messages
        assert_eq!(detail.messages.len(), 3);
        let tool_card = detail
            .messages
            .iter()
            .find(|m| m.kind == kind::TOOL_USE)
            .expect("merged tool card should exist");
        // Unified tool card: `⏺` status marker on success (matches Claude
        // TUI's BLACK_CIRCLE prefix in constants/figures.ts:4) + bold verb
        // + inline-code argument + 4-backtick fence with the raw merged
        // tool_result body. The fence body is plain monospace so the
        // browser's native code-block rendering keeps lines aligned
        // without depending on the visual width of `⎿` (U+23BF varies
        // across browser font fallbacks). Commands are wrapped in
        // inline backticks for markdown-injection safety.
        assert_eq!(
            tool_card.text,
            "⏺ **Bash**(`ls`)\n\n````\nsrc\nREADME.md\n````"
        );
    }

    #[test]
    fn claude_keeps_session_when_mid_session_sidechain_present() {
        // Regression test for the over-restrictive "any line has isSidechain"
        // whole-session hide. videcoding/cli only filters at LIST time on the
        // first line (sessionStorage.ts enrichLog), and per-message in the
        // transcript chain (findLatestMessage). Mid-session sidechain entries
        // (e.g. sub-agent invocations) must NOT make the session disappear.
        let dir = TestDir::new("claude-midsession-sidechain");
        let path = dir
            .path()
            .join("12345678-1234-1234-1234-123456789abf.jsonl");
        fs::write(
            &path,
            // First line: NOT sidechain (passes the LIST filter)
            r#"{"sessionId":"12345678-1234-1234-1234-123456789abf","cwd":"/workspace/claude","timestamp":"2026-05-01T00:00:00Z","type":"user","message":{"role":"user","content":"Build the auth flow"},"isSidechain":false}"#.to_string()
                + "\n"
                // Mid-session sub-agent sidechain entry — historically caused
                // Termory to hide the whole session.
                + r#"{"sessionId":"12345678-1234-1234-1234-123456789abf","timestamp":"2026-05-01T00:00:05Z","type":"user","message":{"role":"user","content":"sub-agent prompt"},"isSidechain":true}"#
                + "\n"
                + r#"{"sessionId":"12345678-1234-1234-1234-123456789abf","timestamp":"2026-05-01T00:00:10Z","type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Built."}]}}"#,
        )
        .unwrap();

        let detail = parse_claude_session(&path).unwrap();
        assert_eq!(detail.session.id, "12345678-1234-1234-1234-123456789abf");
        // Sidechain entry filtered per-message; main thread (user + assistant)
        // remains.
        let texts: Vec<&str> = detail.messages.iter().map(|m| m.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("Build the auth flow")));
        assert!(texts.iter().any(|t| t.contains("Built.")));
        assert!(
            !texts.iter().any(|t| t.contains("sub-agent prompt")),
            "mid-session sidechain entry should be filtered per-message"
        );
    }

    #[test]
    fn claude_displays_local_command_metadata_like_resume_preview() {
        let dir = TestDir::new("claude-command");
        let path = dir
            .path()
            .join("12345678-1234-1234-1234-123456789abd.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/doctor</command-name>\n<command-message>doctor</command-message>\n<command-args></command-args>"},"sessionId":"12345678-1234-1234-1234-123456789abd","timestamp":"2026-05-01T00:00:00Z","cwd":"/workspace/copilot.is"}"#.to_string()
                + "\n"
                + r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>Help me fix the issues reported by /doctor below.</local-command-stdout>","sessionId":"12345678-1234-1234-1234-123456789abd","timestamp":"2026-05-01T00:00:01Z","cwd":"/workspace/copilot.is"}"#,
        )
        .unwrap();

        let detail = parse_claude_session(&path).unwrap();
        assert_eq!(detail.session.title, "/doctor");
        assert_eq!(detail.messages[0].text, "/doctor");
        assert_eq!(
            detail.messages[1].text,
            "Help me fix the issues reported by /doctor below."
        );
        assert_eq!(detail.session.message_count, 1);
    }

    #[test]
    fn claude_displays_empty_local_command_output_without_xml_tags() {
        let dir = TestDir::new("claude-empty-command");
        let path = dir
            .path()
            .join("12345678-1234-1234-1234-123456789ac0.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"config"},"sessionId":"12345678-1234-1234-1234-123456789ac0","timestamp":"2026-05-01T00:00:00Z","cwd":"/workspace/copilot.is"}"#.to_string()
                + "\n"
                + r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout></local-command-stdout>","sessionId":"12345678-1234-1234-1234-123456789ac0","timestamp":"2026-05-01T00:00:01Z","cwd":"/workspace/copilot.is"}"#,
        )
        .unwrap();

        let detail = parse_claude_session(&path).unwrap();
        assert_eq!(detail.messages[1].text, "(no content)");
        assert!(!detail.messages[1].text.contains("local-command-stdout"));
    }

    #[test]
    fn claude_uses_ai_title_and_skips_slash_command_first_prompt_like_official_list() {
        let dir = TestDir::new("claude-ai-title");
        let path = dir
            .path()
            .join("803df946-490d-43fc-aaf2-124944d6a968.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>ignore local commands</local-command-caveat>"},"isMeta":true,"sessionId":"803df946-490d-43fc-aaf2-124944d6a968","timestamp":"2026-05-17T11:15:48.257Z","cwd":"/Users/john/Documents/ip125"}"#.to_string()
                + "\n"
                + r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"},"sessionId":"803df946-490d-43fc-aaf2-124944d6a968","timestamp":"2026-05-17T11:15:48.252Z","cwd":"/Users/john/Documents/ip125"}"#
                + "\n"
                + r#"{"type":"user","message":{"role":"user","content":"ai-image-editor 页面是否可以在默认页面添加对应演示图片"},"sessionId":"803df946-490d-43fc-aaf2-124944d6a968","timestamp":"2026-05-17T11:18:39.104Z","cwd":"/Users/john/Documents/ip125"}"#
                + "\n"
                + r#"{"type":"ai-title","aiTitle":"Add demo images for AI image editor features","sessionId":"803df946-490d-43fc-aaf2-124944d6a968"}"#,
        )
        .unwrap();

        let detail = parse_claude_session(&path).unwrap();
        assert_eq!(
            detail.session.title,
            "Add demo images for AI image editor features"
        );
        assert_ne!(detail.session.title, "803df946");
    }

    #[test]
    fn claude_hides_metadata_only_sessions_like_official_list() {
        let dir = TestDir::new("claude-metadata-only");
        let path = dir
            .path()
            .join("22345678-1234-1234-1234-123456789ac0.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>ignore local commands</local-command-caveat>"},"isMeta":true,"sessionId":"22345678-1234-1234-1234-123456789ac0","timestamp":"2026-05-17T11:15:48.257Z","cwd":"/Users/john/Documents/ip125"}"#.to_string()
                + "\n"
                + r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout></local-command-stdout>","sessionId":"22345678-1234-1234-1234-123456789ac0","timestamp":"2026-05-17T11:15:49.257Z","cwd":"/Users/john/Documents/ip125"}"#,
        )
        .unwrap();

        assert!(parse_claude_session(&path).is_err());
    }

    #[test]
    fn claude_list_uses_head_tail_only_like_official_lite_reader() {
        let dir = TestDir::new("claude-lite");
        let path = dir
            .path()
            .join("32345678-1234-1234-1234-123456789ac0.jsonl");
        let filler = "x".repeat(CLAUDE_LITE_READ_BUF_SIZE + 128);
        fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>ignore local commands</local-command-caveat>"},"isMeta":true,"sessionId":"32345678-1234-1234-1234-123456789ac0","timestamp":"2026-05-17T11:15:48.257Z","cwd":"/Users/john/Documents/ip125"}"#.to_string()
                + "\n"
                + &filler
                + "\n"
                + r#"{"type":"ai-title","aiTitle":"Middle title should not be visible","sessionId":"32345678-1234-1234-1234-123456789ac0"}"#
                + "\n"
                + &filler,
        )
        .unwrap();

        assert!(parse_claude_lite_session(&path, None).is_err());
    }

    #[test]
    fn claude_scans_resume_sessions_like_videcoding_cli() {
        let dir = TestDir::new("claude-scan");
        let root = dir.path().join("projects");
        let project = root.join("-workspace-claude");
        let subagents = project.join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        let visible = project.join("12345678-1234-1234-1234-123456789abe.jsonl");
        fs::write(
            &visible,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/doctor</command-name>\n<command-message>doctor</command-message>\n<command-args></command-args>"},"sessionId":"12345678-1234-1234-1234-123456789abe","timestamp":"2026-05-01T00:00:00Z","cwd":"/workspace/claude","isSidechain":false}"#.to_string()
                + "\n"
                + r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"Use npm test."},{"type":"tool_use","name":"Bash","input":{}}]},"sessionId":"12345678-1234-1234-1234-123456789abe","timestamp":"2026-05-01T00:00:02Z","cwd":"/workspace/claude"}"#
                + "\n"
                + r#"{"type":"last-prompt","lastPrompt":"Review failing tests","sessionId":"12345678-1234-1234-1234-123456789abe"}"#,
        )
        .unwrap();
        fs::write(
            subagents.join("12345678-1234-1234-1234-123456789abf.jsonl"),
            r#"{"type":"user","message":{"role":"user","content":"hidden"},"sessionId":"12345678-1234-1234-1234-123456789abf","timestamp":"2026-05-01T00:00:00Z","cwd":"/workspace/claude"}"#,
        )
        .unwrap();
        fs::write(
            project.join("not-a-session.jsonl"),
            r#"{"type":"user","message":{"role":"user","content":"hidden"},"sessionId":"not-a-session","timestamp":"2026-05-01T00:00:00Z","cwd":"/workspace/claude"}"#,
        )
        .unwrap();

        let sessions = scan_claude_projects(&root).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "12345678-1234-1234-1234-123456789abe");
        assert_eq!(sessions[0].title, "Review failing tests");
        assert_eq!(sessions[0].project, "/workspace/claude");
        assert_eq!(sessions[0].message_count, 2);
    }

    #[test]
    fn gemini_parses_project_chat_jsonl() {
        let dir = TestDir::new("gemini");
        let project_dir = dir.path().join("tmp").join("ip125");
        let chats_dir = project_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(
            project_dir.join(".project_root"),
            "/Users/john/Documents/ip125",
        )
        .unwrap();
        let path = chats_dir.join("session-2026-04-30T06-47-0686029a.jsonl");
        fs::write(
            &path,
            r#"{"sessionId":"0686029a-fc18-4f5c-9a35-1942f63575d0","projectHash":"hash","startTime":"2026-04-30T06:47:46.299Z","lastUpdated":"2026-04-30T06:47:46.299Z","kind":"main"}"#.to_string()
                + "\n"
                + r#"{"id":"user-1","timestamp":"2026-04-30T06:49:11.414Z","type":"user","content":[{"text":"Review code"}]}"#
                + "\n"
                + r#"{"id":"assistant-1","timestamp":"2026-04-30T06:49:18.370Z","type":"gemini","content":"Looks good"}"#,
        )
        .unwrap();

        let detail = parse_gemini_jsonl_session(&path).unwrap();
        assert_eq!(detail.session.id, "0686029a-fc18-4f5c-9a35-1942f63575d0");
        assert_eq!(detail.session.project, "/Users/john/Documents/ip125");
        assert_eq!(detail.session.title, "Review code");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[1].role, "assistant");
    }

    #[test]
    fn gemini_scans_official_session_files_like_list_sessions() {
        let dir = TestDir::new("gemini-scan");
        let project_dir = dir.path().join("tmp").join("project-hash");
        let chats_dir = project_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(project_dir.join(".project_root"), "/workspace/gemini").unwrap();
        fs::write(
            chats_dir.join("session-2026-05-01T00-00-aaaaaaaa.jsonl"),
            r#"{"sessionId":"main-session","projectHash":"project-hash","startTime":"2026-05-01T00:00:00Z","lastUpdated":"2026-05-01T00:02:00Z","kind":"main","summary":"Summarized title"}"#.to_string()
                + "\n"
                + r#"{"id":"msg-1","timestamp":"2026-05-01T00:01:00Z","type":"user","content":[{"text":"First prompt"}]}"#
                + "\n"
                + r#"{"id":"msg-2","timestamp":"2026-05-01T00:02:00Z","type":"gemini","content":[{"text":"Reply"}],"toolCalls":[{"id":"tool-1","name":"run_shell_command","displayName":"Shell","description":"Ran tests","status":"success","resultDisplay":"ok"}]}"#,
        )
        .unwrap();
        fs::write(
            chats_dir.join("session-2026-05-01T00-03-bbbbbbbb.jsonl"),
            r#"{"sessionId":"subagent-session","projectHash":"project-hash","startTime":"2026-05-01T00:03:00Z","lastUpdated":"2026-05-01T00:04:00Z","kind":"subagent"}"#.to_string()
                + "\n"
                + r#"{"id":"msg-3","type":"user","content":[{"text":"Hidden subagent"}]}"#,
        )
        .unwrap();
        fs::write(
            chats_dir.join("session-2026-05-01T00-05-cccccccc.jsonl"),
            r#"{"sessionId":"empty-session","projectHash":"project-hash","startTime":"2026-05-01T00:05:00Z","lastUpdated":"2026-05-01T00:05:00Z","kind":"main"}"#.to_string()
                + "\n"
                + r#"{"id":"msg-4","type":"info","content":"Only info"}"#,
        )
        .unwrap();

        let sessions = scan_gemini_chats_dir(&chats_dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "main-session");
        assert_eq!(sessions[0].project, "/workspace/gemini");
        assert_eq!(sessions[0].title, "Summarized title");
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].preview, "");

        let detail =
            parse_gemini_jsonl_session(&chats_dir.join("session-2026-05-01T00-00-aaaaaaaa.jsonl"))
                .unwrap();
        assert_eq!(detail.messages.len(), 3);
        assert_eq!(detail.messages[0].role, "user");
        assert_eq!(detail.messages[1].role, "assistant");
        assert_eq!(detail.messages[2].role, "tool");
        assert!(detail.messages[2].text.contains("Ran tests"));
    }

    #[test]
    fn gemini_falls_back_to_file_mtime_when_timestamps_missing() {
        // packages/cli/src/utils/sessionUtils.ts:getAllSessionFiles falls back
        // to fs.stat(filePath).mtime when content.startTime/lastUpdated are
        // missing. Termory should match that — sessions without timestamps
        // must still be listed, not silently dropped.
        let dir = TestDir::new("gemini-no-timestamps");
        let project_dir = dir.path().join("tmp").join("p-no-ts");
        let chats_dir = project_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(project_dir.join(".project_root"), "/workspace/no-ts").unwrap();
        let path = chats_dir.join("session-2026-05-21T00-00-dddddddd.jsonl");
        fs::write(
            &path,
            // Note: no startTime, no lastUpdated.
            r#"{"sessionId":"no-ts","projectHash":"p-no-ts","kind":"main"}"#.to_string()
                + "\n"
                + r#"{"id":"u","type":"user","content":[{"text":"hi"}]}"#,
        )
        .unwrap();

        let detail = parse_gemini_jsonl_session(&path).unwrap();
        assert_eq!(detail.session.id, "no-ts");
        // Must have non-empty timestamps; the value came from file mtime
        // (or fell through to "now"), so we only check non-empty.
        assert!(
            detail
                .session
                .started_at
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            "started_at should fall back to file mtime / now"
        );
        assert!(
            detail
                .session
                .updated_at
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            "updated_at should fall back to file mtime / now"
        );
    }

    #[test]
    fn gemini_formats_parts_like_official_part_to_string() {
        // inlineData parts carry base64 binary content (images / audio /
        // etc.). We can't show them in markdown, so emit a small italic
        // marker that tells the user what was attached.
        let text = gemini_content_text_raw(&serde_json::json!([
            {"text":"before"},
            {"inlineData":{"mimeType":"image/png","data":"AAAA"}},
            {"text":"after"}
        ]))
        .unwrap();
        assert_eq!(text, "before*Inline data (image/png)*after");

        // executableCode → fenced block tagged with the language so
        // highlight.js can pick it up later if syntax highlighting is
        // re-enabled (Gemini TUI renders these in `ExecutableCodeRenderer`).
        let text = gemini_content_text_raw(&serde_json::json!([
            {"executableCode":{"code":"print('hi')","language":"PYTHON"}}
        ]))
        .unwrap();
        assert_eq!(text, "```python\nprint('hi')\n```");

        // codeExecutionResult → 4-backtick output block, with an italic
        // outcome footer only when the outcome is non-OK.
        let text = gemini_content_text_raw(&serde_json::json!([
            {"codeExecutionResult":{"output":"hi","outcome":"OUTCOME_OK"}}
        ]))
        .unwrap();
        assert_eq!(text, "````\nhi\n````");

        let text = gemini_content_text_raw(&serde_json::json!([
            {"codeExecutionResult":{"output":"boom","outcome":"OUTCOME_FAILED"}}
        ]))
        .unwrap();
        assert_eq!(text, "````\nboom\n````\n\n*Outcome: OUTCOME_FAILED*");
    }

    #[test]
    fn gemini_tool_card_renders_status_and_structured_result_display() {
        // Success → `⏺` marker, plain-string resultDisplay wraps in
        // 4-backtick fence. Matches Codex / Claude success shape.
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t1",
                "name": "run_shell_command",
                "displayName": "Shell",
                "description": "Ran tests",
                "status": "success",
                "resultDisplay": "ok"
            }]
        }));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "⏺ **Shell**(Ran tests)\n\n````\nok\n````");
        assert_eq!(msgs[0].kind, kind::TOOL_USE);

        // Error status → `✗` marker, body prefixed with `Error: ` inside
        // the fence. Per sessionUtils.ts:654-657, anything other than
        // `'success'` is mapped to Error in Gemini's own session loader.
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t2",
                "name": "run_shell_command",
                "displayName": "Shell",
                "description": "Ran failing cmd",
                "status": "error",
                "resultDisplay": "command not found"
            }]
        }));
        assert_eq!(
            msgs[0].text,
            "✗ **Shell**(Ran failing cmd)\n\n````\nError: command not found\n````"
        );
        assert_eq!(msgs[0].kind, kind::TOOL_ERROR);

        // `{summary, files}` → ListDirectoryResult shape (one of several
        // StructuredToolResult variants). TUI's ToolResultDisplay reads
        // `.summary` as markdown body; Termory mirrors by passing the
        // summary through and dropping the rest.
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t3",
                "name": "list_directory",
                "displayName": "ReadFolder",
                "description": "Listed src",
                "status": "success",
                "resultDisplay": {
                    "summary": "Found 3 item(s).",
                    "files": ["a.ts", "b.ts", "c.ts"]
                }
            }]
        }));
        assert_eq!(
            msgs[0].text,
            "⏺ **ReadFolder**(Listed src)\n\n````\nFound 3 item(s).\n````"
        );

        // `{fileDiff, fileName}` → DiffRenderer in TUI
        // (ToolResultDisplay.tsx:145-161). Termory emits a complete
        // ```diff fence so the markdown viewer styles it as a diff.
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t4",
                "name": "replace",
                "displayName": "Edit",
                "description": "constants.ts",
                "status": "success",
                "resultDisplay": {
                    "fileDiff": "--- a/constants.ts\n+++ b/constants.ts\n@@ -1 +1 @@\n-x\n+y\n",
                    "fileName": "constants.ts"
                }
            }]
        }));
        assert_eq!(
            msgs[0].text,
            "⏺ **Edit**(constants.ts)\n\n```diff\n--- a/constants.ts\n+++ b/constants.ts\n@@ -1 +1 @@\n-x\n+y\n```"
        );

        // `{todos: ...}` → TUI shows nothing (TodoTray renders todos
        // elsewhere). Termory drops the body but still emits the header
        // card so the user sees that the tool ran.
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t5",
                "name": "todo_write",
                "displayName": "Todos",
                "description": "Updated plan",
                "status": "success",
                "resultDisplay": {"todos": [{"content": "x", "status": "done"}]}
            }]
        }));
        assert_eq!(msgs[0].text, "⏺ **Todos**(Updated plan)");

        // AnsiOutput (array of AnsiLine arrays) → join token `text`
        // fields, trim per-line trailing whitespace (xterm-headless pads
        // each line to terminal width). Detect by checking the first
        // element is itself an array (matches `isAnsiOutput` in
        // types.ts:105-106).
        let msgs = gemini_tool_messages_from_value(&serde_json::json!({
            "toolCalls": [{
                "id": "t6",
                "name": "run_shell_command",
                "displayName": "Shell",
                "description": "git status",
                "status": "success",
                "resultDisplay": [
                    [{"text": "On branch main      "}],
                    [{"text": "nothing to commit   "}]
                ]
            }]
        }));
        assert_eq!(
            msgs[0].text,
            "⏺ **Shell**(git status)\n\n````\nOn branch main\nnothing to commit\n````"
        );
    }

    #[test]
    fn opencode_parses_storage_session_and_messages() {
        let dir = TestDir::new("opencode");
        let storage = dir.path().join("storage");
        let session_dir = storage.join("session").join("project-1");
        let message_dir = storage.join("message").join("ses_123");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&message_dir).unwrap();
        let session_path = session_dir.join("ses_123.json");
        fs::write(
            &session_path,
            r#"{"id":"ses_123","projectID":"project-1","directory":"/workspace/opencode","title":"Implement feature","time":{"created":1714521600000,"updated":1714521700000}}"#,
        )
        .unwrap();
        fs::write(
            message_dir.join("msg_1.json"),
            r#"{"id":"msg_1","role":"user","time":1714521600000,"content":[{"text":"Implement feature"}]}"#,
        )
        .unwrap();
        fs::write(
            message_dir.join("msg_2.json"),
            r#"{"id":"msg_2","role":"assistant","time":1714521700000,"content":[{"text":"Done"}]}"#,
        )
        .unwrap();

        let detail = parse_opencode_storage_session(&session_path).unwrap();
        assert_eq!(detail.session.id, "ses_123");
        assert_eq!(detail.session.project, "/workspace/opencode");
        assert_eq!(detail.session.title, "Implement feature");
        assert_eq!(detail.messages.len(), 2);
    }

    #[test]
    fn opencode_reads_official_sqlite_sessions_messages_and_parts() {
        let dir = TestDir::new("opencode-db");
        let db = dir.path().join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                project_id text not null,
                workspace_id text,
                parent_id text,
                slug text not null,
                directory text not null,
                path text,
                title text not null,
                version text not null,
                time_created integer not null,
                time_updated integer not null,
                time_archived integer
            );
            create table message (
                id text primary key,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );
            create table part (
                id text primary key,
                message_id text not null,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );",
        )
        .unwrap();
        conn.execute(
            "insert into session values (?1, 'project-1', null, null, 'slug', ?2, null, ?3, '0.1.0', 1714521600000, 1714521700000, null)",
            ("ses_123", "/workspace/opencode", "Implement feature"),
        )
        .unwrap();
        conn.execute(
            "insert into session values ('child', 'project-1', null, 'ses_123', 'child', '/workspace/opencode', null, 'Child', '0.1.0', 1714521600000, 1714521700000, null)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into session values ('archived', 'project-1', null, null, 'archived', '/workspace/opencode', null, 'Archived', '0.1.0', 1714521600000, 1714521700000, 1714521800000)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into message values ('msg_1', 'ses_123', 1714521600000, 1714521600000, ?1)",
            [r#"{"role":"user","agent":"build","time":{"created":1714521600000}}"#],
        )
        .unwrap();
        conn.execute(
            "insert into part values ('part_1', 'msg_1', 'ses_123', 1714521600000, 1714521600000, ?1)",
            [r#"{"type":"text","text":"Implement feature"}"#],
        )
        .unwrap();
        conn.execute(
            "insert into message values ('msg_2', 'ses_123', 1714521700000, 1714521700000, ?1)",
            [r#"{"role":"assistant","time":{"created":1714521700000},"modelID":"claude"}"#],
        )
        .unwrap();
        conn.execute(
            "insert into part values ('part_2', 'msg_2', 'ses_123', 1714521700000, 1714521700000, ?1)",
            [r#"{"type":"text","text":"Done"}"#],
        )
        .unwrap();
        conn.execute(
            "insert into part values ('part_3', 'msg_2', 'ses_123', 1714521700001, 1714521700001, ?1)",
            [r#"{"type":"tool","title":"edited src/main.ts","output":"hidden"}"#],
        )
        .unwrap();

        let sessions = scan_opencode_db(&db).unwrap();
        // The live `ses_123` AND the archived top-level session appear
        // (Termory surfaces archived history with a badge); the sub-agent
        // `child` (parent_id set) is still excluded.
        assert_eq!(sessions.len(), 2);
        let live = sessions.iter().find(|s| s.id == "ses_123").unwrap();
        assert_eq!(live.project, "/workspace/opencode");
        assert_eq!(live.title, "Implement feature");
        assert_eq!(live.message_count, 2);
        assert!(!live.archived, "a live session is not archived");
        let archived = sessions.iter().find(|s| s.id == "archived").unwrap();
        assert!(
            archived.archived,
            "a session with time_archived set is flagged archived"
        );
        assert!(
            !sessions.iter().any(|s| s.id == "child"),
            "sub-agent children (parent_id set) stay excluded"
        );

        // Loading an archived session by id works and keeps the badge.
        let archived_detail = parse_opencode_db_session(&db, "archived").unwrap();
        assert!(archived_detail.session.archived);

        let detail = parse_opencode_db_session(&db, "ses_123").unwrap();
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].role, "user");
        assert_eq!(detail.messages[0].text, "Implement feature");
        assert_eq!(detail.messages[1].role, "assistant");
        assert_eq!(detail.messages[1].text, "Done");
        assert_eq!(detail.session.message_count, 2);
    }

    #[test]
    fn claude_project_folders_in_enumerates_every_slug_dir() {
        // Projects come from the FOLDERS, independent of records: a dir with a
        // session, a dir with only memory, and a fully-empty dir all show.
        let tmp = TestDir::new("claude-proj-folders");
        let projects = tmp.path().join("projects");
        let with_session = projects.join("-Users-me-app");
        fs::create_dir_all(&with_session).unwrap();
        fs::write(
            with_session.join("11111111-1111-1111-1111-111111111111.jsonl"),
            "{\"cwd\":\"/Users/me/app\",\"type\":\"user\"}\n",
        )
        .unwrap();
        let memory_only = projects.join("-Users-me-notes");
        fs::create_dir_all(memory_only.join("memory")).unwrap();
        fs::write(memory_only.join("memory").join("N.md"), "x").unwrap();
        let empty = projects.join("-Users-me-empty");
        fs::create_dir_all(&empty).unwrap();
        // Not a project slug (no `-`) → skipped.
        fs::create_dir_all(projects.join("bin")).unwrap();

        let out = claude_project_folders_in(&projects);
        let cwds: Vec<String> = out.iter().map(|p| p.project.clone()).collect();
        // Session dir → cwd read from the session; the others fall back to the
        // decoded slug. All three real slug dirs appear; `bin` does not.
        assert!(cwds.contains(&"/Users/me/app".to_string())); // from session cwd
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|p| p.source == "Claude"));
        assert!(!cwds.iter().any(|c| c == "bin" || c.is_empty()));
    }

    #[test]
    fn gemini_project_folders_in_requires_project_root_marker() {
        let tmp = TestDir::new("gemini-proj-folders");
        let proj = tmp.path().join("hash-a");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join(".project_root"), "/Users/me/proj\n").unwrap();
        // No `.project_root` → not a project (e.g. the bundled `bin/`).
        fs::create_dir_all(tmp.path().join("bin")).unwrap();

        let out = gemini_project_folders_in(tmp.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "Gemini");
        assert_eq!(out[0].project, "/Users/me/proj");
    }

    #[test]
    fn add_record_projects_fills_in_record_derived_clis() {
        let mk = |source: &str, project: &str| AppSession {
            id: "x".to_string(),
            source: source.to_string(),
            project: project.to_string(),
            ..Default::default()
        };
        // Claude already enumerated from a folder; Codex/OpenCode + Memory feed in.
        let mut projects = vec![Project {
            source: "Claude".to_string(),
            project: "/a".to_string(),
        }];
        let records = vec![
            mk("Claude", "/a"),   // already present → no dup
            mk("Codex", "/a"),    // distinct source → added
            mk("Codex", "/a"),    // dup → ignored
            mk("OpenCode", "/b"), // added
            mk("Memory", "/c"),   // excluded (not session-type)
            mk("Codex", ""),      // empty cwd → skipped
        ];
        add_record_projects(&mut projects, &records);
        let keys: Vec<(String, String)> = projects
            .iter()
            .map(|p| (p.source.clone(), p.project.clone()))
            .collect();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&("Claude".into(), "/a".into())));
        assert!(keys.contains(&("Codex".into(), "/a".into())));
        assert!(keys.contains(&("OpenCode".into(), "/b".into())));
    }

    #[test]
    fn opencode_empty_projects_in_returns_session_less_rows() {
        let dir = TestDir::new("opencode-empty-proj");
        let db = dir.path().join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table project (id text primary key, worktree text not null);
             create table session (
                id text primary key, project_id text not null,
                parent_id text, time_archived integer
             );",
        )
        .unwrap();
        conn.execute("insert into project values ('A', '/work/has')", [])
            .unwrap();
        conn.execute("insert into project values ('B', '/work/empty')", [])
            .unwrap();
        conn.execute("insert into project values ('C', '/work/archived')", [])
            .unwrap();
        // A has a live top-level session; C has only an archived one; B has none.
        conn.execute("insert into session values ('s1', 'A', null, null)", [])
            .unwrap();
        conn.execute("insert into session values ('s2', 'C', null, 123)", [])
            .unwrap();
        drop(conn);

        let projects = opencode_empty_projects_in(&db).unwrap();
        let worktrees: Vec<String> = projects.iter().map(|p| p.project.clone()).collect();
        assert!(worktrees.contains(&"/work/empty".to_string())); // no sessions → shown
        assert!(worktrees.contains(&"/work/archived".to_string())); // only archived → shown
        assert!(!worktrees.contains(&"/work/has".to_string())); // has a live session → not "empty"
        assert!(projects.iter().all(|p| p.source == "OpenCode"));
    }

    #[test]
    fn gemini_extract_first_user_message_strips_session_context_tag() {
        // session_context title → same content, just the <session_context>
        // wrapper filtered out (NOT skipped, NOT "Empty conversation").
        let only_context = vec![serde_json::json!({
            "type": "user",
            "content": [{"text": "<session_context>\nThis is the Gemini CLI.\nWorkspace: /x\n</session_context>"}]
        })];
        let title = gemini_extract_first_user_message(&only_context).unwrap();
        assert!(!title.contains("<session_context>"), "got: {title}");
        assert!(title.contains("This is the Gemini CLI"), "got: {title}");
        // Guard against the reverted behavior: it must NOT skip to a placeholder.
        assert_ne!(title, "Empty conversation");
        // A normal prompt passes through untouched.
        let prompt = vec![serde_json::json!({
            "type": "user",
            "content": [{"text": "How do I sort a list?"}]
        })];
        assert_eq!(
            gemini_extract_first_user_message(&prompt).as_deref(),
            Some("How do I sort a list?")
        );
    }

    #[test]
    fn gemini_session_context_unwraps_and_fences_only_the_tree() {
        let value = serde_json::json!({
            "type": "user",
            "content": [{"text": "<session_context>\n- **Directory Structure:**\n\n/proj/\n├───a.txt\n└───b.txt\n</session_context>"}]
        });
        let msg = gemini_message_from_value(&value).unwrap();
        assert_eq!(msg.role, "system");
        assert!(msg.text.starts_with("*[Gemini session_context]*"));
        // The `- **…**` markdown line stays markdown (NOT fenced).
        assert!(msg.text.contains("- **Directory Structure:**"));
        // The tree block (root path + box-drawing lines) IS fenced so its
        // layout survives the markdown renderer.
        assert!(msg
            .text
            .contains("````\n/proj/\n├───a.txt\n└───b.txt\n````"));
        // Wrapper unwrapped → no raw <session_context> tag left.
        assert!(!msg.text.contains("<session_context>"));
    }

    #[test]
    fn gemini_fence_tree_blocks_leaves_non_tree_text_untouched() {
        // No box-drawing chars → returned verbatim (no fence added).
        let plain = "- **Workspace Directories:**\n  - /a\nSome prose.";
        assert_eq!(gemini_fence_tree_blocks(plain), plain);
    }

    #[test]
    fn doc_session_id_is_full_path_not_filename() {
        // Regression: memory/skill id was the filename/`name`, so every project's
        // `CLAUDE.md` shared the id "CLAUDE.md" → the (source, id) load index
        // collided and same-named files all loaded the last-scanned content.
        // The id must be the full path (unique); title stays the filename.
        let dir = TestDir::new("doc-id");
        let a = dir.path().join("proj-a").join("CLAUDE.md");
        let b = dir.path().join("proj-b").join("CLAUDE.md");
        fs::create_dir_all(a.parent().unwrap()).unwrap();
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        fs::write(&a, "alpha").unwrap();
        fs::write(&b, "beta").unwrap();
        let sa = doc_session_from_file(&a, "/proj-a", "Memory").unwrap();
        let sb = doc_session_from_file(&b, "/proj-b", "Memory").unwrap();
        assert_eq!(sa.id, a.to_string_lossy());
        assert_eq!(sb.id, b.to_string_lossy());
        assert_ne!(sa.id, sb.id); // same filename, distinct ids
        assert_eq!(sa.title, "CLAUDE.md"); // title still the filename for display
    }

    #[test]
    fn claude_project_registered_in_matches_projects_map() {
        let dir = TestDir::new("claude-reg");
        let cfg = dir.path().join(".claude.json");
        fs::write(
            &cfg,
            r#"{"projects":{"/Users/me/app":{},"/Users/me/other":{}}}"#,
        )
        .unwrap();
        assert!(claude_project_registered_in(&cfg, "/Users/me/app"));
        assert!(!claude_project_registered_in(&cfg, "/Users/me/nope"));
        // Missing file / no projects key → false, never a panic.
        assert!(!claude_project_registered_in(
            &dir.path().join("absent.json"),
            "/x"
        ));
        let empty = dir.path().join("empty.json");
        fs::write(&empty, "{}").unwrap();
        assert!(!claude_project_registered_in(&empty, "/Users/me/app"));
    }

    #[test]
    fn opencode_skips_synthetic_text_parts() {
        // packages/opencode/src/cli/cmd/tui/util/transcript.ts formatPart
        // gates text rendering on `!part.synthetic` — synthetic text parts
        // are prompt-builder-injected (env block, tool acks, etc.) and
        // intentionally hidden from the user-facing transcript.
        let dir = TestDir::new("opencode-synthetic");
        let db = dir.path().join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                project_id text not null,
                workspace_id text,
                parent_id text,
                slug text not null,
                directory text not null,
                path text,
                title text not null,
                version text not null,
                time_created integer not null,
                time_updated integer not null,
                time_archived integer
            );
            create table message (
                id text primary key,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );
            create table part (
                id text primary key,
                message_id text not null,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );",
        )
        .unwrap();
        conn.execute(
            "insert into session values ('ses_syn', 'project-1', null, null, 'slug', '/w/oc', null, 'Synthetic test', '0.1.0', 1714521600000, 1714521700000, null)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into message values ('msg_1', 'ses_syn', 1714521600000, 1714521600000, ?1)",
            [r#"{"role":"user","time":{"created":1714521600000}}"#],
        )
        .unwrap();
        // Synthetic env block — should be skipped.
        conn.execute(
            "insert into part values ('part_synth', 'msg_1', 'ses_syn', 1714521600000, 1714521600000, ?1)",
            [r#"{"type":"text","text":"<env>cwd=/w/oc</env>","synthetic":true}"#],
        )
        .unwrap();
        // Real user prompt — should be included.
        conn.execute(
            "insert into part values ('part_real', 'msg_1', 'ses_syn', 1714521600001, 1714521600001, ?1)",
            [r#"{"type":"text","text":"Real prompt"}"#],
        )
        .unwrap();

        let detail = parse_opencode_db_session(&db, "ses_syn").unwrap();
        // Two parts: synthetic env block + real prompt. Termory shows
        // BOTH (OpenCode TUI hides synthetic; Termory wraps each as
        // `*[synthetic]* {body}` and renders).
        assert_eq!(detail.messages.len(), 2);
        let synth = &detail.messages[0].text;
        let real = &detail.messages[1].text;
        assert!(
            synth.starts_with("*[synthetic]*"),
            "synthetic text part should surface with a marker"
        );
        assert!(
            synth.contains("<env>"),
            "synthetic content should be visible"
        );
        assert!(real.contains("Real prompt"), "real prompt should remain");
    }

    #[test]
    fn opencode_reads_v2_session_message_table_and_titles_like_official() {
        let dir = TestDir::new("opencode-v2-db");
        let db = dir.path().join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                project_id text not null,
                workspace_id text,
                parent_id text,
                slug text not null,
                directory text not null,
                path text,
                title text not null,
                version text not null,
                time_created integer not null,
                time_updated integer not null,
                time_archived integer
            );
            create table session_message (
                id text primary key,
                session_id text not null,
                type text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );",
        )
        .unwrap();
        conn.execute(
            "insert into session values (?1, 'project-1', null, null, 'slug', ?2, null, ?3, '0.1.0', 1714521600000, 1714521700000, null)",
            (
                "ses_v2",
                "/workspace/opencode",
                "New session - 2026-05-01T00:00:00.000Z",
            ),
        )
        .unwrap();
        conn.execute(
            "insert into session_message values ('msg_1', 'ses_v2', 'user', 1714521600000, 1714521600000, ?1)",
            [r#"{"text":"Implement feature","time":{"created":1714521600000}}"#],
        )
        .unwrap();
        conn.execute(
            "insert into session_message values ('msg_2', 'ses_v2', 'assistant', 1714521700000, 1714521700000, ?1)",
            [r#"{"content":[{"type":"text","text":"Done"},{"type":"reasoning","text":"hidden"}],"time":{"created":1714521700000}}"#],
        )
        .unwrap();

        let sessions = scan_opencode_db(&db).unwrap();
        assert_eq!(sessions[0].title, "New session");
        assert_eq!(sessions[0].message_count, 2);

        let detail = parse_opencode_db_session(&db, "ses_v2").unwrap();
        assert_eq!(detail.session.title, "New session");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].text, "Implement feature");
        // Reasoning content is rendered via the unified italic-blockquote
        // pattern (`> *...*`) so it visually separates from the regular
        // assistant text above.
        assert_eq!(detail.messages[1].text, "Done\n\n> *hidden*");
    }

    #[test]
    fn opencode_formats_v2_special_tool_parts_like_official_preview() {
        // OpenCode tools all use the unified `**Verb**(args)` shape now —
        // same as Codex / Claude / Gemini. The verb still comes from
        // session-v2.tsx but the OpenCode-specific decorations (`\#` H1
        // escape, `←` arrow, `↳ Loaded`, `"pattern" in path` quoted form)
        // are dropped in favor of the unified shape.

        // Bash — completed with output goes in 4-backtick fence;
        // `description` field restored as `\# {description}` between the
        // header and the fence (preserving the OpenCode BlockTool title
        // semantics).
        let bash = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"bash",
            "state":{
                "status":"completed",
                "input":{"command":"npm test","description":"Run tests"},
                "content":[{"type":"text","text":"ok"}]
            }
        }))
        .unwrap();
        assert_eq!(
            bash,
            "⏺ **Shell**(`npm test`)\n\n\\# Run tests\n\n```bash\n$ npm test\nok\n```"
        );

        // Read — `[other=...]` continues to display the other input fields
        // verbatim; metadata.loaded entries dropped (TUI-only `↳ Loaded`).
        let read = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"read",
            "state":{"input":{"filePath":"src/main.ts","offset":10,"limit":20}},
            "provider":{"metadata":{"loaded":["src/main.ts"]}}
        }))
        .unwrap();
        assert_eq!(
            read,
            // Source order (offset, limit) — matches OpenCode's JS TUI,
            // which iterates the input object in insertion order. (Before
            // serde_json's preserve_order this came out alphabetically.)
            "⏺ **Read**(`src/main.ts` [offset=10, limit=20])\\\n↳ Loaded src/main.ts"
        );

        // TodoWrite — todos rendered as a list with status icons.
        let todos = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"todowrite",
            "state":{"input":{"todos":[
                {"status":"completed","content":"Read code"},
                {"status":"in_progress","content":"Patch parser"},
                {"status":"pending","content":"Run tests"}
            ]},"status":"completed"}
        }))
        .unwrap();
        assert_eq!(
            todos,
            "⏺ **Todos**\n\n✓ Read code\n~ Patch parser\n☐ Run tests"
        );

        // Glob — `**Glob**(pattern: ..., path: ... — N matches)`.
        let glob = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"glob",
            "state":{"input":{"pattern":"**/*.ts","path":"src"}},
            "metadata":{"count":12}
        }))
        .unwrap();
        assert_eq!(
            glob,
            "⏺ **Glob**(pattern: `**/*.ts`, path: `src` — 12 matches)"
        );

        // Grep — singular/plural still respected in the trailing summary.
        let grep = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"grep",
            "state":{"input":{"pattern":"TODO","path":"src/lib.rs"}},
            "metadata":{"matches":1}
        }))
        .unwrap();
        assert_eq!(
            grep,
            "⏺ **Grep**(pattern: `TODO`, path: `src/lib.rs` — 1 match)"
        );

        // Edit — diff content in ```diff fence; `←` arrow dropped.
        let edit = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"edit",
            "state":{"input":{"filePath":"src/main.ts"}},
            "metadata":{"diff":"- old\n+ new"}
        }))
        .unwrap();
        assert_eq!(
            edit,
            "⏺ **Edit**(`src/main.ts`)\n\n```diff\n- old\n+ new\n```"
        );
    }

    #[test]
    fn opencode_bash_prefers_metadata_output_and_strips_ansi() {
        // Per session-v2.tsx:710 Bash resolves output as
        // `metadata.output ?? props.output` then strips ANSI. Termory
        // mirrors: pick metadata.output when present (it's the
        // already-formatted shell capture), fall back to state.content
        // (props.output), and run the result through strip_ansi so
        // colour codes don't leak into the markdown viewer.
        let bash = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"bash",
            "state":{
                "status":"completed",
                "input":{"command":"ls","description":"List"},
                // Generic projection has bare text…
                "content":[{"type":"text","text":"raw"}]
            },
            // …but metadata.output has the colourised capture. The TUI
            // prefers metadata.output here.
            "provider":{"metadata":{"output":"\u{1b}[31mred\u{1b}[0m line\nline2"}}
        }))
        .unwrap();
        assert_eq!(
            bash,
            "⏺ **Shell**(`ls`)\n\n\\# List\n\n```bash\n$ ls\nred line\nline2\n```"
        );
    }

    #[test]
    fn opencode_websearch_uses_provider_aware_verb() {
        // session-v2.tsx:818 + websearch.ts:39-43 — provider label varies
        // by `metadata.provider`: "parallel" → "Parallel Web Search",
        // "exa" → "Exa Web Search", anything else → "Web Search".
        let parallel = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool","name":"websearch",
            "state":{"input":{"query":"tauri v2 docs"}},
            "metadata":{"provider":"parallel","numResults":5}
        }))
        .unwrap();
        assert_eq!(
            parallel,
            "⏺ **Parallel Web Search**(`tauri v2 docs` — 5 results)"
        );

        let exa = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool","name":"websearch",
            "state":{"input":{"query":"rust async"}},
            "metadata":{"provider":"exa","numResults":3}
        }))
        .unwrap();
        assert_eq!(exa, "⏺ **Exa Web Search**(`rust async` — 3 results)");

        // No provider → default "Web Search" (with a space, matching the
        // TUI default at websearch.ts:42).
        let default = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool","name":"websearch",
            "state":{"input":{"query":"q"}}
        }))
        .unwrap();
        assert_eq!(default, "⏺ **Web Search**(`q`)");
    }

    #[test]
    fn opencode_apply_patch_pluralizes_deletions_fallback() {
        // Per session-v2.tsx:923 — when a file has no patch text, the
        // TUI falls back to `-N line` / `-N lines` (singular for one
        // deletion). Termory mirrors via the deletions count.
        let one = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool","name":"apply_patch",
            "state":{"input":{}},
            "metadata":{"files":[{"type":"delete","relativePath":"a.txt","deletions":1}]}
        }))
        .unwrap();
        assert!(one.starts_with("⏺ **Deleted**(`a.txt`)"));
        assert!(one.contains("```diff\n-1 line\n```"), "got: {one}");

        let many = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool","name":"apply_patch",
            "state":{"input":{}},
            "metadata":{"files":[{"type":"delete","relativePath":"b.txt","deletions":3}]}
        }))
        .unwrap();
        assert!(many.contains("```diff\n-3 lines\n```"), "got: {many}");
    }

    #[test]
    fn opencode_shell_message_strips_ansi_from_output() {
        // session-v2.tsx:203 ShellMessage applies `stripAnsi` to the
        // captured output before rendering. The shell-typed message in
        // SQLite carries the raw capture; Termory strips ANSI to match.
        let msg = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"shell",
                "command":"ls",
                "output":"\u{1b}[1mbold\u{1b}[0m plain",
                "time":{"created":1714521600000_i64}
            }),
            1714521600000,
        )
        .unwrap();
        assert_eq!(msg.text, "$ ls\nbold plain");
    }

    #[test]
    fn opencode_tool_error_marks_card_and_appends_error_body() {
        // Per session-v2.tsx:572 + l.669, a tool whose state.status ==
        // "error" gets the error color and a `state.error.message` text
        // beneath the tool body. Termory should:
        //   * flip the leading marker to `✗`
        //   * append a 4-backtick fence body `Error: {message}`
        let failed = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"bash",
            "state":{
                "status":"error",
                "input":{"command":"./missing.sh","description":"Run"},
                "error":{"type":"unknown","message":"command not found"}
            }
        }))
        .unwrap();
        // No output → Bash branch returns just the header; wrapper
        // prepends `✗ ` and appends the `Error:` fence below.
        assert_eq!(
            failed,
            "✗ **Shell**(`./missing.sh`)\n\n````\nError: command not found\n````"
        );

        // No `error.message` — still surface "Error" so the user sees
        // the failure indicator instead of a confusingly-clean card.
        let bare = opencode_v2_tool_part_text(&serde_json::json!({
            "type":"tool",
            "name":"bash",
            "state":{"status":"error","input":{"command":"./x"}}
        }))
        .unwrap();
        assert_eq!(bare, "✗ **Shell**(`./x`)\n\n````\nError\n````");
    }

    #[test]
    fn opencode_user_message_surfaces_file_and_agent_attachments() {
        // session-v2.tsx:176-194 — UserMessage renders attached files
        // (mime + name pills) and agents (`agent` + name pills) under
        // the prompt text. `references` are intentionally NOT rendered.
        let msg = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"user",
                "text":"please review",
                "files":[
                    {"uri":"file:///a.png","mime":"image/png","name":"shot.png"},
                    {"uri":"file:///b.md","mime":"text/markdown"}
                ],
                "agents":[{"name":"reviewer"}],
                "references":[{"name":"ignored","kind":"local"}],
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        // Second file has no `name` → falls back to `uri` per TUI
        // `file.name ?? file.uri` at session-v2.tsx:182.
        assert_eq!(
            msg.text,
            "please review\n\n`image/png` `shot.png` `text/markdown` `file:///b.md` `agent` `reviewer`"
        );

        // No attachments — message body is just the trimmed text.
        let plain = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"user",
                "text":"hi",
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(plain.text, "hi");
    }

    #[test]
    fn opencode_compaction_title_reflects_auto_vs_manual_reason() {
        // session-v2.tsx:237 — title is `Auto Compaction` for
        // `reason === "auto"`, otherwise `Compaction`. Surface as a
        // bold markdown header so users can tell auto-triggered
        // compactions apart from manual `/compact` invocations.
        let auto = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"compaction","reason":"auto","summary":"summary text",
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(auto.text, "**Auto Compaction**\n\nsummary text");

        let manual = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"compaction","reason":"manual","summary":"summary text",
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(manual.text, "**Compaction**\n\nsummary text");
    }

    #[test]
    fn opencode_agent_and_model_switch_messages_carry_tui_icons() {
        // session-v2.tsx:267 prepends `▣ ` for agent switches, l.284
        // prepends `◇ ` for model switches.
        let agent = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"agent-switched","agent":"build",
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(agent.text, "▣ Switched agent to Build");

        let model = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"model-switched",
                "model":{"providerID":"anthropic","id":"claude","variant":"default"},
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(model.text, "◇ Switched model to anthropic/claude");
    }

    #[test]
    fn opencode_assistant_error_surfaces_below_text() {
        // session-v2.tsx:339-353 — when assistant.error is set, a red
        // notice box renders below the message body. Termory mirrors
        // with an italic `*✕ {message}*` line.
        let msg = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"assistant",
                "agent":"general",
                "model":{"providerID":"x","id":"y","variant":""},
                "content":[{"type":"text","text":"Working on it"}],
                "error":{"type":"unknown","message":"Connection reset"},
                "time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(msg.text, "Working on it\n\n*✕ Connection reset*");
    }

    #[test]
    fn opencode_synthetic_messages_surface_with_marker() {
        // session-v2.tsx:105-107 — OpenCode TUI hides synthetic
        // messages. Termory wraps them in `*[synthetic]*` notices
        // because the user asked for the full transcript.
        let msg = opencode_v2_message_from_value(
            &serde_json::json!({
                "type":"synthetic","content":"system-generated",
                "sessionID":"s","time":{"created":1_714_000_000_000_i64}
            }),
            1_714_000_000_000,
        )
        .unwrap();
        assert_eq!(msg.text, "*[synthetic]*\n\nsystem-generated");
    }

    #[test]
    fn strip_ansi_handles_csi_osc_and_single_letter_escapes() {
        // CSI colour codes, OSC title-set, lone ESC-c reset — all
        // should disappear, leaving only the printable payload.
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}[1;32mbold green\u{1b}[m"), "bold green");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}body"), "body");
        assert_eq!(
            strip_ansi("\u{1b}]0;title\u{1b}\\body"),
            "body",
            "OSC + ST terminator"
        );
        assert_eq!(strip_ansi("\u{1b}cclear"), "clear");
        // Non-ESC content untouched.
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn opencode_reads_official_115_message_parts_with_tool_field() {
        let dir = TestDir::new("opencode-115-parts");
        let db = dir.path().join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                project_id text not null,
                workspace_id text,
                parent_id text,
                slug text not null,
                directory text not null,
                path text,
                title text not null,
                version text not null,
                time_created integer not null,
                time_updated integer not null,
                time_archived integer
            );
            create table message (
                id text primary key,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );
            create table part (
                id text primary key,
                message_id text not null,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );
            create table session_message (
                id text primary key,
                session_id text not null,
                type text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            );",
        )
        .unwrap();
        conn.execute(
            "insert into session values ('ses_115', 'project-1', null, null, 'slug', '/workspace/opencode', null, 'Implement feature', '1.15.1', 1714521600000, 1714521700000, null)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into message values ('msg_1', 'ses_115', 1714521600000, 1714521600000, ?1)",
            [r#"{"role":"assistant","time":{"created":1714521600000},"agent":"build","modelID":"claude"}"#],
        )
        .unwrap();
        conn.execute(
            "insert into part values ('part_1', 'msg_1', 'ses_115', 1714521600000, 1714521600000, ?1)",
            [r#"{"type":"text","text":"Running tests"}"#],
        )
        .unwrap();
        conn.execute(
            "insert into part values ('part_2', 'msg_1', 'ses_115', 1714521600001, 1714521600001, ?1)",
            [r#"{"type":"tool","tool":"shell","state":{"status":"completed","input":{"command":"npm test","description":"Run tests"},"output":"ok","metadata":{"output":"ok"}}}"#],
        )
        .unwrap();

        let detail = parse_opencode_db_session(&db, "ses_115").unwrap();
        // Each part now produces its own SessionMessage so tool calls carry
        // their TUI label (Bash) on a separate entry instead of being joined
        // into the assistant text.
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].role, "assistant");
        assert_eq!(detail.messages[0].text, "Running tests");
        assert_eq!(detail.messages[1].role, "tool");
        assert_eq!(detail.messages[1].kind, "tool_use");
        assert_eq!(
            detail.messages[1].text,
            "⏺ **Shell**(`npm test`)\n\n\\# Run tests\n\n```bash\n$ npm test\nok\n```"
        );
    }

    // ------------------------------------------------------------------
    // Memory scanning tests
    // ------------------------------------------------------------------

    #[test]
    fn decode_claude_project_slug_translates_dashes_to_slashes() {
        // The `-`-prefixed slug is unix-shaped and decodes through
        // PathBuf, which joins with `\` on Windows; real Windows slugs
        // are `C--Users-…` shaped (Windows-shaped decode coverage is a
        // follow-up), so this case asserts on unix only.
        #[cfg(unix)]
        assert_eq!(
            decode_claude_project_slug("-Users-john-Documents-foo"),
            "/Users/john/Documents/foo"
        );
        assert_eq!(decode_claude_project_slug("foo"), "foo");
        assert_eq!(decode_claude_project_slug(""), "");
    }

    #[test]
    fn resolve_dashed_path_recovers_hyphenated_directory() {
        // A real `copilot-is` folder: the slug split yields ["copilot",
        // "is"], and resolution must merge them back because `base/copilot`
        // doesn't exist but `base/copilot-is` does.
        let dir = TestDir::new("dashed-merge");
        fs::create_dir_all(dir.path().join("copilot-is")).unwrap();
        assert_eq!(
            resolve_dashed_path(dir.path(), &["copilot", "is"]),
            dir.path().join("copilot-is")
        );

        // When the un-merged path exists instead, keep it split.
        let split = TestDir::new("dashed-split");
        fs::create_dir_all(split.path().join("copilot").join("is")).unwrap();
        assert_eq!(
            resolve_dashed_path(split.path(), &["copilot", "is"]),
            split.path().join("copilot").join("is")
        );

        // Nothing on disk → naive one-segment-per-component.
        let missing = TestDir::new("dashed-missing");
        assert_eq!(
            resolve_dashed_path(missing.path(), &["copilot", "is"]),
            missing.path().join("copilot").join("is")
        );
    }

    #[test]
    fn claude_cwd_from_project_dir_reads_real_cwd_from_session() {
        // The slug `-Users-john-Documents-copilot-is` is lossy (Claude turned
        // the `.` in `copilot.is` into `-`), so only the session's recorded
        // `cwd` recovers the true path.
        let dir = TestDir::new("claude-cwd");
        fs::write(
            dir.path()
                .join("141fb4c2-477d-4fbd-85da-44ac71cbc842.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/Users/john/Documents/copilot.is\",\"message\":{}}\n",
        )
        .unwrap();
        assert_eq!(
            claude_cwd_from_project_dir(dir.path()).as_deref(),
            Some("/Users/john/Documents/copilot.is")
        );

        // No session JSONL → None (caller falls back to slug decode).
        let empty = TestDir::new("claude-cwd-empty");
        assert_eq!(claude_cwd_from_project_dir(empty.path()), None);
    }

    #[test]
    fn claude_work_statuses_in_filters_by_filename_and_liveness() {
        let dir = TestDir::new("claude-status");
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // This test process's own pid is guaranteed alive.
        let me = std::process::id();
        fs::write(
            sessions.join(format!("{me}.json")),
            r#"{"sessionId":"sid-live","status":"busy"}"#,
        )
        .unwrap();
        // Non-`<pid>.json` name → excluded by the filename guard.
        fs::write(
            sessions.join("notes.json"),
            r#"{"sessionId":"sid-notes","status":"busy"}"#,
        )
        .unwrap();
        // Implausibly high pid → not a live process.
        fs::write(
            sessions.join("2147483646.json"),
            r#"{"sessionId":"sid-dead","status":"busy"}"#,
        )
        .unwrap();

        let map = claude_work_statuses_in(&sessions);
        // Live pid + busy → included on every platform.
        assert_eq!(map.get("sid-live"), Some(&ClaudeWorkStatus::Busy));
        // Filename guard applies everywhere.
        assert!(!map.contains_key("sid-notes"));
        // Liveness probe runs on unix + windows; only there is a dead pid
        // skipped (exotic targets fall back to a conservative `true`).
        #[cfg(any(unix, windows))]
        assert!(!map.contains_key("sid-dead"));

        // Missing dir → empty, no panic.
        assert!(claude_work_statuses_in(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn parse_claude_status_snapshot_maps_running_states() {
        use super::ClaudeWorkStatus::*;
        let p = super::parse_claude_status_snapshot;

        // Real REPL PID-file shape (verified on disk): status without state.
        assert_eq!(
            p(r#"{"pid":1,"sessionId":"sid-busy","cwd":"/x","status":"busy"}"#),
            Some(("sid-busy".to_string(), Busy))
        );
        assert_eq!(
            p(r#"{"sessionId":"sid-wait","status":"waiting","waitingFor":"input needed"}"#),
            Some(("sid-wait".to_string(), Waiting))
        );
        // `dialog open` = user opened a UI, not a real input-block → no marker.
        assert_eq!(
            p(r#"{"sessionId":"sid-dlg","status":"waiting","waitingFor":"dialog open"}"#),
            None
        );
        // Idle / unknown / missing sessionId → no marker.
        assert_eq!(p(r#"{"sessionId":"sid-idle","status":"idle"}"#), None);
        assert_eq!(p(r#"{"status":"busy"}"#), None);
        assert_eq!(p("not json"), None);

        // `state` (claude agents --json) takes precedence over `status`.
        assert_eq!(
            p(r#"{"sessionId":"sid-s","state":"working","status":"idle"}"#),
            Some(("sid-s".to_string(), Busy))
        );
        assert_eq!(
            p(r#"{"sessionId":"sid-b","state":"blocked"}"#),
            Some(("sid-b".to_string(), Waiting))
        );
        assert_eq!(p(r#"{"sessionId":"sid-d","state":"done"}"#), None);
    }

    #[test]
    fn scan_claude_projects_returns_sessions_only_no_placeholders() {
        // The project LIST now comes from `claude_project_folders_in` (folder
        // enumeration), so `scan_claude_projects` returns ONLY real session
        // records — never an empty-project placeholder (id == "").
        let tmp = TestDir::new("claude-empty-scan");
        let root = tmp.path();
        fs::create_dir_all(root.join("-Users-test-empty")).unwrap();
        fs::create_dir_all(root.join("-Users-test-memonly").join("memory")).unwrap();
        fs::write(
            root.join("-Users-test-memonly").join("memory").join("M.md"),
            "note",
        )
        .unwrap();

        let sessions = scan_claude_projects(root).unwrap();
        assert!(
            sessions.iter().all(|s| !s.id.is_empty()),
            "no placeholder entries"
        );
    }

    #[test]
    fn gemini_project_root_reads_marker() {
        let tmp = TestDir::new("gemini-proj-root");
        let dir = tmp.path().join("hash");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(gemini_project_root(&dir), None); // no .project_root
        fs::write(dir.join(".project_root"), "/Users/me/proj\n").unwrap();
        assert_eq!(gemini_project_root(&dir).as_deref(), Some("/Users/me/proj"));
    }

    #[test]
    fn is_safe_rel_rejects_traversal_and_absolutes() {
        assert!(is_safe_rel("abc-123"));
        assert!(is_safe_rel("sub/NOTE.md"));
        assert!(!is_safe_rel("")); // empty
        assert!(!is_safe_rel("..")); // parent
        assert!(!is_safe_rel("a/../b")); // traversal mid-path
        assert!(!is_safe_rel("/etc/passwd")); // absolute
    }

    #[test]
    fn delete_gemini_session_in_removes_by_project_and_filename() {
        let tmp = TestDir::new("gemini-del-sess");
        let dir = tmp.path().join("hash");
        fs::create_dir_all(dir.join("chats")).unwrap();
        fs::write(dir.join(".project_root"), "/Users/me/proj").unwrap();
        let f = dir.join("chats").join("session-x.json");
        fs::write(&f, "{}").unwrap();

        // Located by project + the chat filename the frontend already has —
        // no scan, no sessionId matching.
        delete_gemini_record_in(tmp.path(), "/Users/me/proj", "chats/session-x.json").unwrap();
        assert!(!f.exists());

        // Unsafe filename (..) rejected; non-matching project errors.
        assert!(delete_gemini_record_in(tmp.path(), "/Users/me/proj", "../x").is_err());
        assert!(
            delete_gemini_record_in(tmp.path(), "/Users/me/nope", "chats/session-x.json").is_err()
        );
    }

    #[test]
    fn delete_gemini_memory_in_removes_by_project_and_rel() {
        let tmp = TestDir::new("gemini-del-mem");
        let dir = tmp.path().join("hash");
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::write(dir.join(".project_root"), "/Users/me/proj").unwrap();
        fs::write(dir.join("memory").join("NOTE.md"), "x").unwrap();

        delete_gemini_record_in(tmp.path(), "/Users/me/proj", "memory/NOTE.md").unwrap();
        assert!(!dir.join("memory").join("NOTE.md").exists());

        // Unsafe rel (..) is rejected; non-matching project errors.
        assert!(delete_gemini_record_in(tmp.path(), "/Users/me/proj", "../x").is_err());
        assert!(delete_gemini_record_in(tmp.path(), "/Users/me/nope", "memory/NOTE.md").is_err());
    }

    #[test]
    fn delete_gemini_project_in_removes_dir_matching_project_root() {
        let tmp = TestDir::new("gemini-del-proj");
        // Two project dirs, only one matches the target project root.
        let target = tmp.path().join("hash-a");
        fs::create_dir_all(target.join("chats")).unwrap();
        fs::write(target.join(".project_root"), "/Users/me/target\n").unwrap();
        let other = tmp.path().join("hash-b");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join(".project_root"), "/Users/me/other").unwrap();

        delete_gemini_project_in(tmp.path(), "/Users/me/target").unwrap();
        assert!(!target.exists());
        assert!(other.exists()); // unrelated project untouched

        // No matching project → error.
        assert!(delete_gemini_project_in(tmp.path(), "/Users/me/nope").is_err());
    }

    // Fixtures + assertions are unix-path-shaped: on Windows
    // fs::canonicalize returns a \\?\-prefixed path (different slug
    // than production's stripped one) and JSON-escapes the
    // backslashes, so the substring asserts can't hold verbatim.
    // Windows-shaped migrate coverage is a tracked follow-up.
    #[cfg(unix)]
    #[test]
    fn migrate_claude_project_copies_sessions_memory_and_rewrites_cwd() {
        let tmp = TestDir::new("migrate");
        let projects = tmp.path().join("projects");
        // Old project: the renamed folder is gone from disk, only the slug dir
        // (with sessions + memory) survives.
        let old_path = "/Users/test/oldproj";
        let old_dir = projects.join(sanitize_claude_path(old_path).unwrap());
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(
            old_dir.join("aaa.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{old_path}\",\"x\":1}}\n{{\"summary\":\"s\"}}\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(old_dir.join("memory")).unwrap();
        fs::write(old_dir.join("memory").join("NOTE.md"), "hello").unwrap();
        // Companion dir <id>/subagents/ (a subagent transcript) + the
        // regenerable index cache that should be skipped.
        fs::create_dir_all(old_dir.join("aaa").join("subagents")).unwrap();
        fs::write(
            old_dir.join("aaa").join("subagents").join("agent-x.jsonl"),
            format!("{{\"cwd\":\"{old_path}\",\"sub\":1}}\n"),
        )
        .unwrap();
        fs::write(old_dir.join("sessions-index.json"), "{\"v\":1}").unwrap();

        // New path must exist on disk (the function canonicalizes it).
        let new_path_dir = tmp.path().join("newproj");
        fs::create_dir_all(&new_path_dir).unwrap();
        let new_path = new_path_dir.to_string_lossy().to_string();

        let res = migrate_claude_project_in(&projects, old_path, &new_path, false).unwrap();
        assert_eq!(res.sessions, 1);
        assert_eq!(res.memory_files, 1);

        // Destination = slug of the canonicalized new path.
        let new_canon = fs::canonicalize(&new_path)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let new_dir = projects.join(sanitize_claude_path(&new_canon).unwrap());
        let migrated = fs::read_to_string(new_dir.join("aaa.jsonl")).unwrap();
        // cwd rewritten to the new path; sibling fields + non-cwd lines intact.
        assert!(migrated.contains(&format!("\"cwd\":\"{new_canon}\"")));
        assert!(migrated.contains("\"x\":1"));
        assert!(migrated.contains("\"summary\":\"s\""));
        assert!(!migrated.contains(old_path));
        assert_eq!(
            fs::read_to_string(new_dir.join("memory").join("NOTE.md")).unwrap(),
            "hello"
        );
        // Companion subagent transcript came along, with cwd rewritten.
        let sub = fs::read_to_string(new_dir.join("aaa").join("subagents").join("agent-x.jsonl"))
            .unwrap();
        assert!(sub.contains(&format!("\"cwd\":\"{new_canon}\"")));
        assert!(sub.contains("\"sub\":1"));
        // The regenerable index cache is NOT copied (Claude rebuilds it).
        assert!(!new_dir.join("sessions-index.json").exists());
        // Copy, not move: the old dir is left intact (built-in backup).
        assert!(old_dir.join("aaa.jsonl").exists());
    }

    // Fixtures + assertions are unix-path-shaped: on Windows
    // fs::canonicalize returns a \\?\-prefixed path (different slug
    // than production's stripped one) and JSON-escapes the
    // backslashes, so the substring asserts can't hold verbatim.
    // Windows-shaped migrate coverage is a tracked follow-up.
    #[cfg(unix)]
    #[test]
    fn migrate_claude_session_in_copies_one_session_and_rewrites_cwd() {
        let tmp = TestDir::new("migrate-sess");
        let projects = tmp.path().join("projects");
        let old_path = "/Users/test/oldproj";
        let old_dir = projects.join(sanitize_claude_path(old_path).unwrap());
        fs::create_dir_all(&old_dir).unwrap();
        let src = old_dir.join("bbb.jsonl");
        fs::write(&src, format!("{{\"cwd\":\"{old_path}\",\"k\":2}}\n")).unwrap();
        // Companion dir bbb/subagents/ rides along with the single session.
        fs::create_dir_all(old_dir.join("bbb").join("subagents")).unwrap();
        fs::write(
            old_dir.join("bbb").join("subagents").join("agent-y.jsonl"),
            format!("{{\"cwd\":\"{old_path}\",\"s\":9}}\n"),
        )
        .unwrap();
        let new_path_dir = tmp.path().join("newproj");
        fs::create_dir_all(&new_path_dir).unwrap();
        let new_path = new_path_dir.to_string_lossy().to_string();

        // Stamp the sources with a known past mtime; the migrated copies must
        // keep it (Claude /resume + Termory's mtime fallback order by it).
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let companion_src = old_dir.join("bbb").join("subagents").join("agent-y.jsonl");
        for p in [&src, &companion_src] {
            fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(past)
                .unwrap();
        }

        let res =
            migrate_claude_session_in(&projects, old_path, "bbb.jsonl", &new_path, false).unwrap();
        assert_eq!((res.sessions, res.memory_files), (1, 0));
        let new_canon = fs::canonicalize(&new_path)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let new_dir = projects.join(sanitize_claude_path(&new_canon).unwrap());
        let migrated = fs::read_to_string(new_dir.join("bbb.jsonl")).unwrap();
        assert!(migrated.contains(&format!("\"cwd\":\"{new_canon}\"")));
        assert!(migrated.contains("\"k\":2"));
        // Companion subagent transcript came along, cwd rewritten.
        let sub = fs::read_to_string(new_dir.join("bbb").join("subagents").join("agent-y.jsonl"))
            .unwrap();
        assert!(sub.contains(&format!("\"cwd\":\"{new_canon}\"")));
        assert!(sub.contains("\"s\":9"));
        assert!(src.exists()); // copy, kept
                               // mtime preserved on both the session JSONL and the companion transcript.
        for p in [
            &new_dir.join("bbb.jsonl"),
            &new_dir.join("bbb").join("subagents").join("agent-y.jsonl"),
        ] {
            assert_eq!(fs::metadata(p).unwrap().modified().unwrap(), past);
        }
    }

    // Fixtures + assertions are unix-path-shaped: on Windows
    // fs::canonicalize returns a \\?\-prefixed path (different slug
    // than production's stripped one) and JSON-escapes the
    // backslashes, so the substring asserts can't hold verbatim.
    // Windows-shaped migrate coverage is a tracked follow-up.
    #[cfg(unix)]
    #[test]
    fn migrate_claude_memory_in_copies_md_preserving_subpath_and_rejects_stray() {
        let tmp = TestDir::new("migrate-mem");
        let projects = tmp.path().join("projects");
        let old_dir = projects.join(sanitize_claude_path("/Users/test/oldproj").unwrap());
        let src = old_dir.join("memory").join("sub").join("NOTE.md");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, "remember").unwrap();
        // Stamp a known past mtime — the migrated copy must keep it (covers the
        // non-jsonl `fs::copy` branch of preserve_mtime).
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(past)
            .unwrap();
        let new_path_dir = tmp.path().join("newproj");
        fs::create_dir_all(&new_path_dir).unwrap();
        let new_path = new_path_dir.to_string_lossy().to_string();

        let res = migrate_claude_memory_in(
            &projects,
            "/Users/test/oldproj",
            "memory/sub/NOTE.md",
            &new_path,
            false,
        )
        .unwrap();
        assert_eq!((res.sessions, res.memory_files), (0, 1));
        let new_canon = fs::canonicalize(&new_path)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let new_dir = projects.join(sanitize_claude_path(&new_canon).unwrap());
        let dest_md = new_dir.join("memory").join("sub").join("NOTE.md");
        assert_eq!(fs::read_to_string(&dest_md).unwrap(), "remember");
        assert_eq!(fs::metadata(&dest_md).unwrap().modified().unwrap(), past);
        assert!(src.exists());

        // An unsafe relative path (`..`) is rejected — can't escape memory/.
        assert!(migrate_claude_memory_in(
            &projects,
            "/Users/test/oldproj",
            "../x",
            &new_path,
            false
        )
        .is_err());
    }

    // End-to-end on a realistic project that mirrors the copilot.is layout:
    // several sessions, a companion <id>/ dir with subagents/ + tool-results/,
    // a nested memory subtree, plus the regenerable index + .DS_Store. Verifies
    // MOVE mode (delete_old) yields a complete new dir AND removes the old one.
    // Fixtures + assertions are unix-path-shaped: on Windows
    // fs::canonicalize returns a \\?\-prefixed path (different slug
    // than production's stripped one) and JSON-escapes the
    // backslashes, so the substring asserts can't hold verbatim.
    // Windows-shaped migrate coverage is a tracked follow-up.
    #[cfg(unix)]
    #[test]
    fn migrate_claude_project_move_mode_is_complete_and_removes_old() {
        let tmp = TestDir::new("migrate-e2e");
        let projects = tmp.path().join("projects");
        let old_path = "/Users/test/copilot.is";
        let old_dir = projects.join(sanitize_claude_path(old_path).unwrap());
        fs::create_dir_all(&old_dir).unwrap();

        // 3 sessions, each line carrying the old cwd.
        for id in ["s1", "s2", "s3"] {
            fs::write(
                old_dir.join(format!("{id}.jsonl")),
                format!("{{\"type\":\"user\",\"cwd\":\"{old_path}\",\"id\":\"{id}\"}}\n"),
            )
            .unwrap();
        }
        // Companion dir for s1: a subagent transcript (cwd) + a raw tool result.
        fs::create_dir_all(old_dir.join("s1").join("subagents")).unwrap();
        fs::write(
            old_dir.join("s1").join("subagents").join("agent-a.jsonl"),
            format!("{{\"cwd\":\"{old_path}\",\"agent\":1}}\n"),
        )
        .unwrap();
        fs::create_dir_all(old_dir.join("s1").join("tool-results")).unwrap();
        fs::write(
            old_dir.join("s1").join("tool-results").join("r.txt"),
            "raw tool output",
        )
        .unwrap();
        // Nested memory subtree + regenerable/junk files that must be skipped.
        fs::create_dir_all(old_dir.join("memory").join("sub")).unwrap();
        fs::write(old_dir.join("memory").join("MEMORY.md"), "m1").unwrap();
        fs::write(old_dir.join("memory").join("sub").join("NOTE.md"), "m2").unwrap();
        fs::write(old_dir.join("sessions-index.json"), "{\"v\":1}").unwrap();
        fs::write(old_dir.join(".DS_Store"), "junk").unwrap();

        let new_path_dir = tmp.path().join("chats.is");
        fs::create_dir_all(&new_path_dir).unwrap();
        let new_path = new_path_dir.to_string_lossy().to_string();

        // MOVE mode.
        let res = migrate_claude_project_in(&projects, old_path, &new_path, true).unwrap();
        assert_eq!((res.sessions, res.memory_files), (3, 2));

        let new_canon = fs::canonicalize(&new_path)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let new_dir = projects.join(sanitize_claude_path(&new_canon).unwrap());

        // All 3 sessions present, cwd rewritten.
        for id in ["s1", "s2", "s3"] {
            let body = fs::read_to_string(new_dir.join(format!("{id}.jsonl"))).unwrap();
            assert!(
                body.contains(&format!("\"cwd\":\"{new_canon}\"")),
                "{id} cwd"
            );
            assert!(!body.contains(old_path), "{id} still has old cwd");
        }
        // Companion subagent transcript (cwd rewritten) + raw tool result (verbatim).
        let agent =
            fs::read_to_string(new_dir.join("s1").join("subagents").join("agent-a.jsonl")).unwrap();
        assert!(agent.contains(&format!("\"cwd\":\"{new_canon}\"")));
        assert!(agent.contains("\"agent\":1"));
        assert_eq!(
            fs::read_to_string(new_dir.join("s1").join("tool-results").join("r.txt")).unwrap(),
            "raw tool output"
        );
        // Nested memory copied; index + .DS_Store skipped.
        assert_eq!(
            fs::read_to_string(new_dir.join("memory").join("MEMORY.md")).unwrap(),
            "m1"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join("memory").join("sub").join("NOTE.md")).unwrap(),
            "m2"
        );
        assert!(!new_dir.join("sessions-index.json").exists());
        assert!(!new_dir.join(".DS_Store").exists());

        // MOVE mode removed the old slug dir entirely (no duplicate UUIDs left).
        assert!(!old_dir.exists());
    }

    #[test]
    fn delete_claude_project_in_removes_the_slug_dir() {
        let tmp = TestDir::new("del-proj");
        let projects = tmp.path().join("projects");
        let dir = projects.join(sanitize_claude_path("/Users/test/p").unwrap());
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::write(dir.join("s.jsonl"), "{}\n").unwrap();
        fs::write(dir.join("memory").join("M.md"), "x").unwrap();

        delete_claude_project_in(&projects, "/Users/test/p").unwrap();
        assert!(!dir.exists());
        // A project with no history → error (nothing to delete).
        assert!(delete_claude_project_in(&projects, "/Users/test/none").is_err());
    }

    #[test]
    fn delete_claude_session_in_removes_jsonl_and_companion_and_guards_path() {
        let tmp = TestDir::new("del-sess");
        let projects = tmp.path().join("projects");
        let dir = projects.join(sanitize_claude_path("/Users/test/p").unwrap());
        fs::create_dir_all(dir.join("s1").join("subagents")).unwrap();
        let jsonl = dir.join("s1.jsonl");
        fs::write(&jsonl, "{}\n").unwrap();
        fs::write(dir.join("s1").join("subagents").join("a.jsonl"), "{}\n").unwrap();

        delete_claude_session_in(&projects, "/Users/test/p", "s1.jsonl").unwrap();
        assert!(!jsonl.exists());
        assert!(!dir.join("s1").exists()); // companion dir gone too

        // Safety: an id with traversal is refused (and the file left alone).
        fs::write(&jsonl, "{}\n").unwrap();
        assert!(delete_claude_session_in(&projects, "/Users/test/p", "../evil").is_err());
        assert!(jsonl.exists());
    }

    #[test]
    fn delete_claude_memory_in_removes_md_and_rejects_non_auto_memory() {
        let tmp = TestDir::new("del-mem");
        let projects = tmp.path().join("projects");
        let dir = projects.join(sanitize_claude_path("/Users/test/p").unwrap());
        let md = dir.join("memory").join("N.md");
        fs::create_dir_all(md.parent().unwrap()).unwrap();
        fs::write(&md, "x").unwrap();

        delete_claude_memory_in(&projects, "/Users/test/p", "memory/N.md").unwrap();
        assert!(!md.exists());

        // An unsafe relative path (`..`) is refused.
        assert!(delete_claude_memory_in(&projects, "/Users/test/p", "../../etc/x").is_err());
    }

    /// Seed a minimal Codex state DB with `threads(id, cwd, rollout_path)` and
    /// the matching rollout files on disk. Returns (db path, dir).
    fn seed_codex_threads(dir: &Path) {
        let db = dir.join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table threads (id text primary key, cwd text not null, \
             rollout_path text not null);",
        )
        .unwrap();
        for (id, cwd) in [("a", "/proj/x"), ("b", "/proj/x"), ("c", "/proj/y")] {
            let roll = dir.join(format!("{id}.jsonl"));
            fs::write(&roll, "{}\n").unwrap();
            conn.execute(
                "insert into threads values (?1, ?2, ?3)",
                rusqlite::params![id, cwd, roll.to_string_lossy()],
            )
            .unwrap();
        }
    }

    #[test]
    fn delete_codex_session_in_removes_row_and_rollout_file() {
        let tmp = TestDir::new("del-codex-sess");
        seed_codex_threads(tmp.path());
        let db = tmp.path().join("state_5.sqlite");

        delete_codex_threads_in(&db, "id", "a").unwrap();
        // Row gone, its rollout file gone; siblings untouched.
        let conn = Connection::open(&db).unwrap();
        let count: i64 = conn
            .query_row("select count(*) from threads where id = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(!tmp.path().join("a.jsonl").exists());
        assert!(tmp.path().join("b.jsonl").exists());

        // Unknown id → error.
        assert!(delete_codex_threads_in(&db, "id", "zzz").is_err());
    }

    #[test]
    fn delete_codex_project_in_removes_all_threads_for_cwd() {
        let tmp = TestDir::new("del-codex-proj");
        seed_codex_threads(tmp.path());
        let db = tmp.path().join("state_5.sqlite");

        delete_codex_threads_in(&db, "cwd", "/proj/x").unwrap();
        let conn = Connection::open(&db).unwrap();
        let remaining: i64 = conn
            .query_row("select count(*) from threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1); // only /proj/y's "c" left
        assert!(!tmp.path().join("a.jsonl").exists());
        assert!(!tmp.path().join("b.jsonl").exists());
        assert!(tmp.path().join("c.jsonl").exists());
    }

    #[test]
    fn delete_codex_memory_in_removes_md_and_guards_path() {
        let tmp = TestDir::new("del-codex-mem");
        let root = tmp.path().join("memories");
        let md = root.join("sub").join("N.md");
        fs::create_dir_all(md.parent().unwrap()).unwrap();
        fs::write(&md, "x").unwrap();

        delete_codex_memory_in(&root, "sub/N.md").unwrap();
        assert!(!md.exists());
        // Traversal is refused.
        assert!(delete_codex_memory_in(&root, "../../etc/x").is_err());
    }

    /// Seed a Codex state DB whose rollout files carry a real `session_meta`
    /// first line with `payload.cwd` (so migrate can rewrite it). a,b → /proj/x;
    /// c → /proj/y. Returns the db path.
    fn seed_codex_threads_with_meta(dir: &Path) -> PathBuf {
        let db = dir.join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "create table threads (id text primary key, cwd text not null, \
             rollout_path text not null);",
        )
        .unwrap();
        for (id, cwd) in [("a", "/proj/x"), ("b", "/proj/x"), ("c", "/proj/y")] {
            let roll = dir.join(format!("{id}.jsonl"));
            fs::write(
                &roll,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"model_provider\":\"openai\"}}}}\n\
                     {{\"type\":\"response_item\",\"payload\":{{\"text\":\"hello\"}}}}\n"
                ),
            )
            .unwrap();
            conn.execute(
                "insert into threads values (?1, ?2, ?3)",
                rusqlite::params![id, cwd, roll.to_string_lossy()],
            )
            .unwrap();
        }
        db
    }

    fn thread_cwd(db: &Path, id: &str) -> String {
        Connection::open(db)
            .unwrap()
            .query_row("select cwd from threads where id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn rewrite_rollout_cwd_changes_first_line_keeps_rest_and_is_idempotent() {
        let tmp = TestDir::new("codex-rewrite-cwd");
        let roll = tmp.path().join("r.jsonl");
        fs::write(
            &roll,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"cwd\":\"/old\",\"model_provider\":\"openai\"}}\n\
             {\"type\":\"response_item\",\"payload\":{\"text\":\"hello\"}}\n\
             {\"type\":\"response_item\",\"payload\":{\"text\":\"world\"}}\n",
        )
        .unwrap();

        assert!(rewrite_rollout_cwd(&roll, "/new/dest").unwrap());
        let lines: Vec<String> = fs::read_to_string(&roll)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let first: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["payload"]["cwd"], "/new/dest");
        // Sibling fields + the id survive; later lines are untouched.
        assert_eq!(first["payload"]["model_provider"], "openai");
        assert_eq!(first["payload"]["id"], "x");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("hello"));
        assert!(lines[2].contains("world"));

        // Idempotent: re-running with the same cwd is a no-op.
        assert!(!rewrite_rollout_cwd(&roll, "/new/dest").unwrap());
        // A line with no payload object → nothing to change.
        let bare = tmp.path().join("bare.jsonl");
        fs::write(&bare, "{\"type\":\"x\"}\n").unwrap();
        assert!(!rewrite_rollout_cwd(&bare, "/new/dest").unwrap());
    }

    #[test]
    fn migrate_codex_session_in_rewrites_one_thread_cwd_and_file() {
        let tmp = TestDir::new("codex-migrate-sess");
        let db = seed_codex_threads_with_meta(tmp.path());

        let moved = migrate_codex_threads_in(&db, "id", "a", "/new/dest").unwrap();
        assert_eq!(moved, 1);
        // Table updated for "a" only; siblings untouched.
        assert_eq!(thread_cwd(&db, "a"), "/new/dest");
        assert_eq!(thread_cwd(&db, "b"), "/proj/x");
        assert_eq!(thread_cwd(&db, "c"), "/proj/y");
        // Authoritative rollout file rewritten for a, not b.
        let a: Value = serde_json::from_str(
            fs::read_to_string(tmp.path().join("a.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(a["payload"]["cwd"], "/new/dest");
        let b: Value = serde_json::from_str(
            fs::read_to_string(tmp.path().join("b.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(b["payload"]["cwd"], "/proj/x");

        // Unknown id → error.
        assert!(migrate_codex_threads_in(&db, "id", "zzz", "/new/dest").is_err());
    }

    #[test]
    fn migrate_codex_project_in_rewrites_all_threads_for_cwd() {
        let tmp = TestDir::new("codex-migrate-proj");
        let db = seed_codex_threads_with_meta(tmp.path());

        let moved = migrate_codex_threads_in(&db, "cwd", "/proj/x", "/new/dest").unwrap();
        assert_eq!(moved, 2); // a + b
        assert_eq!(thread_cwd(&db, "a"), "/new/dest");
        assert_eq!(thread_cwd(&db, "b"), "/new/dest");
        assert_eq!(thread_cwd(&db, "c"), "/proj/y"); // other project untouched
        for id in ["a", "b"] {
            let v: Value = serde_json::from_str(
                fs::read_to_string(tmp.path().join(format!("{id}.jsonl")))
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["payload"]["cwd"], "/new/dest");
        }
    }

    #[test]
    fn codex_canonical_cwd_falls_back_for_missing_dir_and_rejects_empty() {
        // Non-existent path → returned as-is (trimmed).
        assert_eq!(
            codex_canonical_cwd("  /no/such/dir/xyz123  ").unwrap(),
            "/no/such/dir/xyz123"
        );
        assert!(codex_canonical_cwd("   ").is_err());
    }

    /// Schema covering session/message cascade plus the event log tables.
    fn seed_opencode_db(db: &Path) {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "create table project (id text primary key, worktree text not null);
             create table session (
                id text primary key,
                project_id text not null references project(id) on delete cascade,
                parent_id text
             );
             create table message (
                id text primary key,
                session_id text not null references session(id) on delete cascade
             );
             create table event_sequence (aggregate_id text primary key, seq integer not null);
             create table event (
                id text primary key,
                aggregate_id text not null references event_sequence(aggregate_id) on delete cascade,
                seq integer not null, type text not null, data text not null
             );",
        )
        .unwrap();
    }

    #[test]
    fn delete_opencode_session_in_removes_children_cascades_and_clears_events() {
        let tmp = TestDir::new("del-oc-sess");
        let db = tmp.path().join("opencode.db");
        seed_opencode_db(&db);
        let conn = Connection::open(&db).unwrap();
        // s1 has a sub-agent child c1, which itself has a grandchild c2.
        conn.execute_batch(
            "insert into project values ('P', '/w');
             insert into session (id, project_id) values ('s1', 'P');
             insert into session (id, project_id) values ('s2', 'P');
             insert into session (id, project_id, parent_id) values ('c1', 'P', 's1');
             insert into session (id, project_id, parent_id) values ('c2', 'P', 'c1');
             insert into message values ('m1', 's1');
             insert into message values ('mc', 'c1');
             insert into event_sequence values ('s1', 1);
             insert into event_sequence values ('c1', 1);
             insert into event_sequence values ('s2', 1);
             insert into event values ('e1', 's1', 1, 'Created', '{}');",
        )
        .unwrap();
        drop(conn);

        // Deleting s1 also removes child c1 and grandchild c2 (recursive).
        delete_opencode_session_in(&db, "s1").unwrap();
        let conn = Connection::open(&db).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("select count(*) from session"), 1); // only s2 remains
        assert_eq!(count("select count(*) from message"), 0); // s1's + c1's cascaded
                                                              // s1 + c1 events cleared; s2's kept.
        assert_eq!(
            count("select count(*) from event_sequence where aggregate_id = 's1'"),
            0
        );
        assert_eq!(
            count("select count(*) from event_sequence where aggregate_id = 'c1'"),
            0
        );
        assert_eq!(
            count("select count(*) from event_sequence where aggregate_id = 's2'"),
            1
        );
        assert!(delete_opencode_session_in(&db, "missing").is_err());
    }

    #[test]
    fn delete_opencode_project_in_cascades_and_clears_events() {
        let tmp = TestDir::new("del-oc-proj");
        let db = tmp.path().join("opencode.db");
        seed_opencode_db(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "insert into project values ('P', '/w/del');
             insert into project values ('Q', '/w/keep');
             insert into session (id, project_id) values ('s1', 'P');
             insert into session (id, project_id) values ('s2', 'Q');
             insert into event_sequence values ('s1', 1);
             insert into event_sequence values ('s2', 1);",
        )
        .unwrap();
        drop(conn);

        delete_opencode_project_in(&db, "/w/del").unwrap();
        let conn = Connection::open(&db).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("select count(*) from project"), 1); // Q remains
        assert_eq!(count("select count(*) from session"), 1); // P's session cascaded
                                                              // P's session events cleared; Q's session events kept.
        assert_eq!(
            count("select count(*) from event_sequence where aggregate_id = 's1'"),
            0
        );
        assert_eq!(
            count("select count(*) from event_sequence where aggregate_id = 's2'"),
            1
        );
        assert!(delete_opencode_project_in(&db, "/w/nope").is_err());
    }

    #[test]
    fn split_memory_frontmatter_extracts_yaml_fields_and_body() {
        let raw = "---\nname: user-role\ndescription: notes about the user\nmetadata:\n  type: user\n---\n\nbody line one\nbody line two\n";
        let (front, body) = split_memory_frontmatter(raw);
        let front = front.expect("frontmatter map");
        assert_eq!(front.get("name").map(String::as_str), Some("user-role"));
        assert_eq!(
            front.get("description").map(String::as_str),
            Some("notes about the user")
        );
        // `type:` is nested under metadata; the simple parser flattens to the key it finds last
        assert_eq!(front.get("type").map(String::as_str), Some("user"));
        assert!(body.starts_with("body line one"));
    }

    #[test]
    fn split_memory_frontmatter_returns_none_when_no_frontmatter() {
        let raw = "just markdown\nno frontmatter";
        let (front, body) = split_memory_frontmatter(raw);
        assert!(front.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn memory_session_from_file_marks_empty_body_with_zero_message_count() {
        let dir = TestDir::new("memory-empty");
        let empty = dir.path().join("empty.md");
        fs::write(&empty, "").unwrap();
        let only_frontmatter = dir.path().join("only-front.md");
        fs::write(&only_frontmatter, "---\nname: foo\n---\n").unwrap();
        let with_body = dir.path().join("with-body.md");
        fs::write(&with_body, "some content").unwrap();

        let s_empty = memory_session_from_file(&empty, "label").unwrap();
        assert_eq!(s_empty.message_count, 0);
        let s_front = memory_session_from_file(&only_frontmatter, "label").unwrap();
        assert_eq!(s_front.message_count, 0);
        let s_body = memory_session_from_file(&with_body, "label").unwrap();
        assert_eq!(s_body.message_count, 1);
    }

    #[test]
    fn memory_tool_for_file_classifies_known_filenames() {
        assert_eq!(memory_tool_for_file("CLAUDE.md"), "claude");
        assert_eq!(memory_tool_for_file("CLAUDE.local.md"), "claude");
        assert_eq!(memory_tool_for_file("AGENTS.md"), "agents");
        assert_eq!(memory_tool_for_file("GEMINI.md"), "gemini");
        assert_eq!(memory_tool_for_file("default.rules"), "rules");
        assert_eq!(memory_tool_for_file("unknown.md"), "memory");
    }

    #[test]
    fn gemini_project_label_reads_project_root_file_or_falls_back() {
        let dir = TestDir::new("gemini-label");
        let with_root = dir.path().join("with");
        fs::create_dir_all(&with_root).unwrap();
        fs::write(with_root.join(".project_root"), "/some/cwd\n").unwrap();
        assert_eq!(gemini_project_label(&with_root), "/some/cwd");

        let no_root = dir.path().join("hash-id");
        fs::create_dir_all(&no_root).unwrap();
        assert_eq!(gemini_project_label(&no_root), "hash-id");
    }

    #[test]
    fn push_memory_files_recursive_skips_dot_entries_and_tags_with_tool() {
        let dir = TestDir::new("recurse");
        let base = dir.path();
        fs::write(base.join("top.md"), "top body").unwrap();
        fs::create_dir_all(base.join("skills/skill-a")).unwrap();
        fs::write(base.join("skills/skill-a/SKILL.md"), "skill body").unwrap();
        fs::create_dir_all(base.join(".git")).unwrap();
        fs::write(base.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        fs::create_dir_all(base.join(".inbox")).unwrap();
        fs::write(base.join(".inbox/extraction.patch"), "patch").unwrap();
        fs::write(base.join("ignored.txt"), "not md").unwrap();

        let mut out = Vec::new();
        push_memory_files_recursive(base, base, "label", "codex", &mut out);
        let mut paths: Vec<String> = out.iter().map(|s| crate::testutils::norm(&s.id)).collect();
        paths.sort();
        assert_eq!(paths, vec!["skills/skill-a/SKILL.md", "top.md"]);
        for entry in &out {
            assert_eq!(entry.preview, "codex");
            assert_eq!(entry.source, "Memory");
            assert_eq!(entry.project, "label");
        }
    }

    #[test]
    fn push_tagged_instruction_file_joins_multi_tool_tags_into_preview() {
        let dir = TestDir::new("tagged");
        let path = dir.path().join("AGENTS.md");
        fs::write(&path, "body").unwrap();
        let mut out = Vec::new();
        push_tagged_instruction_file(&path, "/some/project", &["codex", "opencode"], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].preview, "codex,opencode");
        assert_eq!(out[0].project, "/some/project");
    }

    #[test]
    fn push_tagged_instruction_file_silently_skips_missing_files() {
        let dir = TestDir::new("tagged-missing");
        let mut out = Vec::new();
        push_tagged_instruction_file(&dir.path().join("nope.md"), "label", &["claude"], &mut out);
        assert!(out.is_empty());
    }

    fn memory_test_cwds(cwd: &str, _tools: &[&str]) -> HashSet<String> {
        // Plant a .git directory in the cwd so the ancestor-walk implemented in
        // scan_memory stops at the test cwd and does not ascend into /tmp (or
        // wherever the test root sits). `_tools` is unused now that scan_memory
        // no longer takes per-tool cwd sets — kept on the signature so existing
        // call sites read naturally.
        let _ = fs::create_dir_all(Path::new(cwd).join(".git"));
        [cwd.to_string()].into_iter().collect()
    }

    #[test]
    fn scan_memory_shows_project_agents_md_with_both_tags_when_no_session_evidence() {
        let dir = TestDir::new("agents-fallback");
        let project = dir.path();
        let agents = project.join("AGENTS.md");
        fs::write(&agents, "body").unwrap();
        let claude_md = project.join("CLAUDE.md");
        fs::write(&claude_md, "body").unwrap();
        let gemini_md = project.join("GEMINI.md");
        fs::write(&gemini_md, "body").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds = memory_test_cwds(&cwd, &["Claude"]);
        let sessions = scan_memory(&project_cwds).unwrap();

        let agents_path = agents.to_string_lossy().to_string();
        let agents_entry = sessions
            .iter()
            .find(|s| s.path == agents_path)
            .expect("AGENTS.md should still be shown (file exists on disk)");
        assert_eq!(
            agents_entry.preview, "codex,opencode",
            "AGENTS.md falls back to both tags when neither tool has a session in this cwd"
        );
        let claude_path = claude_md.to_string_lossy().to_string();
        assert!(
            sessions
                .iter()
                .any(|s| s.path == claude_path && s.preview == "claude,opencode"),
            "project CLAUDE.md should be tagged claude,opencode (OpenCode reads it as fallback)"
        );
        let gemini_path = gemini_md.to_string_lossy().to_string();
        assert!(
            sessions
                .iter()
                .any(|s| s.path == gemini_path && s.preview == "gemini"),
            "GEMINI.md should always be shown with gemini tag"
        );
    }

    #[test]
    fn scan_memory_always_tags_project_agents_md_with_both_codex_and_opencode() {
        // The AGENTS.md spec is tool-neutral (always usable by both Codex and
        // OpenCode), so we ignore session evidence and always tag with both.
        for (case, tools) in [
            ("agents-codex-only", &["Codex"][..]),
            ("agents-opencode-only", &["OpenCode"][..]),
            ("agents-no-sessions", &[][..]),
        ] {
            let dir = TestDir::new(case);
            let project = dir.path();
            let agents = project.join("AGENTS.md");
            fs::write(&agents, "body").unwrap();
            let cwd = project.to_string_lossy().to_string();
            let project_cwds = memory_test_cwds(&cwd, tools);
            let sessions = scan_memory(&project_cwds).unwrap();
            let entry = sessions
                .iter()
                .find(|s| s.path == agents.to_string_lossy())
                .unwrap_or_else(|| panic!("AGENTS.md missing for case {case}"));
            assert_eq!(
                entry.preview, "codex,opencode",
                "AGENTS.md always shows both tags (case: {case})"
            );
        }
    }

    #[test]
    fn scan_memory_tags_project_agents_md_with_both_codex_and_opencode_sessions() {
        // AGENTS.local.md is not in any official tool's spec; the supported
        // overrides come from AGENTS.override.md (Codex). This test only
        // covers AGENTS.md now.
        let dir = TestDir::new("agents-both");
        let project = dir.path();
        let agents = project.join("AGENTS.md");
        fs::write(&agents, "body").unwrap();
        let cwd = project.to_string_lossy().to_string();
        let project_cwds = memory_test_cwds(&cwd, &["Codex", "OpenCode"]);
        let sessions = scan_memory(&project_cwds).unwrap();

        let main = sessions
            .iter()
            .find(|s| s.path == agents.to_string_lossy())
            .expect("AGENTS.md should be present");
        assert_eq!(main.preview, "codex,opencode");
    }

    #[test]
    fn scan_memory_picks_up_dot_claude_claude_md_in_project() {
        let dir = TestDir::new("dot-claude");
        let project = dir.path();
        fs::create_dir_all(project.join(".claude")).unwrap();
        let scoped = project.join(".claude").join("CLAUDE.md");
        fs::write(&scoped, "scoped body").unwrap();
        let cwd = project.to_string_lossy().to_string();
        let project_cwds = memory_test_cwds(&cwd, &["Claude"]);
        let sessions = scan_memory(&project_cwds).unwrap();
        let entry = sessions
            .iter()
            .find(|s| s.path == scoped.to_string_lossy())
            .expect(".claude/CLAUDE.md should be present");
        assert_eq!(entry.preview, "claude");
    }

    #[test]
    fn push_doc_files_recursive_skips_named_subdirs() {
        let dir = TestDir::new("skip-skills");
        let base = dir.path();
        fs::write(base.join("top.md"), "top body").unwrap();
        fs::create_dir_all(base.join("skills/skill-a")).unwrap();
        fs::write(base.join("skills/skill-a/SKILL.md"), "skill body").unwrap();
        fs::create_dir_all(base.join("rollout_summaries")).unwrap();
        fs::write(base.join("rollout_summaries/r1.md"), "summary").unwrap();

        let mut out = Vec::new();
        push_doc_files_recursive(
            base,
            base,
            "label",
            "codex",
            "Memory",
            &["skills"],
            &mut out,
        );
        let mut paths: Vec<String> = out.iter().map(|s| crate::testutils::norm(&s.id)).collect();
        paths.sort();
        assert_eq!(paths, vec!["rollout_summaries/r1.md", "top.md"]);
        for entry in &out {
            assert_eq!(entry.source, "Memory");
        }
    }

    #[test]
    fn push_doc_files_recursive_marks_source_skill() {
        let dir = TestDir::new("skill-source");
        let base = dir.path();
        fs::create_dir_all(base.join("git-workflow")).unwrap();
        fs::write(
            base.join("git-workflow/SKILL.md"),
            "---\nname: git-workflow\ndescription: a skill\n---\nbody",
        )
        .unwrap();

        let mut out = Vec::new();
        push_doc_files_recursive(
            base,
            base,
            "~/.claude/skills",
            "claude",
            "Skill",
            &[],
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "Skill");
        assert_eq!(out[0].preview, "claude");
        assert_eq!(out[0].project, "~/.claude/skills");
        assert_eq!(
            crate::testutils::norm(&out[0].title),
            "git-workflow/SKILL.md"
        );
    }

    #[test]
    fn scan_claude_skills_picks_up_project_skills_dir() {
        let dir = TestDir::new("claude-project-skills");
        let project = dir.path();
        let skills_dir = project.join(".claude").join("skills").join("my-skill");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("SKILL.md"), "skill body").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_claude_skills(&project_cwds);

        let entry = sessions
            .iter()
            .find(|s| {
                crate::testutils::norm(&s.path).ends_with("my-skill/SKILL.md") && s.project == cwd
            })
            .expect("project-level Claude skill should be picked up");
        assert_eq!(entry.source, "Skill");
        // OpenCode also reads .claude/skills/, and Grok Build reads
        // ~/.claude/ skills for compatibility — tagged with all three.
        assert_eq!(entry.preview, "claude,opencode,grok");
    }

    #[test]
    fn scan_codex_skills_picks_up_project_skills_dir() {
        let dir = TestDir::new("codex-project-skills");
        let project = dir.path();
        let skills_dir = project.join(".codex").join("skills").join("dbg");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("SKILL.md"), "skill body").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_codex_skills(&project_cwds);

        let entry = sessions
            .iter()
            .find(|s| crate::testutils::norm(&s.path).ends_with("dbg/SKILL.md") && s.project == cwd)
            .expect("project-level Codex skill should be picked up");
        assert_eq!(entry.source, "Skill");
        assert_eq!(entry.preview, "codex");
    }

    #[test]
    fn scan_gemini_skills_picks_up_project_skills_dir() {
        let dir = TestDir::new("gemini-project-skills");
        let project = dir.path();
        let skills_dir = project.join(".gemini").join("skills").join("trace");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("SKILL.md"), "skill body").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_gemini_skills(&project_cwds);

        let entry = sessions
            .iter()
            .find(|s| {
                crate::testutils::norm(&s.path).ends_with("trace/SKILL.md") && s.project == cwd
            })
            .expect("project-level Gemini skill should be picked up");
        assert_eq!(entry.source, "Skill");
        assert_eq!(entry.preview, "gemini");
    }

    #[test]
    fn scan_opencode_skills_picks_up_project_skills_dir() {
        let dir = TestDir::new("opencode-project-skills");
        let project = dir.path();
        let skills_dir = project.join(".opencode").join("skills").join("review");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("SKILL.md"), "skill body").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_opencode_skills(&project_cwds);

        let entry = sessions
            .iter()
            .find(|s| {
                crate::testutils::norm(&s.path).ends_with("review/SKILL.md") && s.project == cwd
            })
            .expect("project-level OpenCode skill should be picked up");
        assert_eq!(entry.source, "Skill");
        assert_eq!(entry.preview, "opencode");
    }

    #[test]
    fn derive_memory_project_label_strips_dot_tool_wrapper_for_skill_paths() {
        let dir = TestDir::new("derive-project-skill");
        let project = dir.path();
        let path = project
            .join(".claude")
            .join("skills")
            .join("my-skill")
            .join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "body").unwrap();
        let label = derive_memory_project_label(&path);
        assert_eq!(label, project.to_string_lossy());
    }

    #[test]
    fn scan_agents_skills_picks_up_project_dot_agents_skills_dir() {
        let dir = TestDir::new("agents-project-skills");
        let project = dir.path();
        let skills_dir = project.join(".agents").join("skills").join("debug-deploy");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("SKILL.md"), "skill body").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_agents_skills(&project_cwds);

        let entry = sessions
            .iter()
            .find(|s| {
                crate::testutils::norm(&s.path).ends_with("debug-deploy/SKILL.md")
                    && s.project == cwd
            })
            .expect("project-level .agents/skills/ entry should be picked up");
        assert_eq!(entry.source, "Skill");
        // Codex, Gemini CLI, and OpenCode all officially read this path.
        assert_eq!(entry.preview, "codex,gemini,opencode");
    }

    #[test]
    fn derive_memory_project_label_strips_dot_agents_wrapper_for_skill_paths() {
        let dir = TestDir::new("derive-project-agents-skill");
        let project = dir.path();
        let path = project
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "body").unwrap();
        let label = derive_memory_project_label(&path);
        assert_eq!(label, project.to_string_lossy());
    }

    #[test]
    fn scan_claude_rules_picks_up_project_rules_dir_recursively() {
        let dir = TestDir::new("claude-project-rules");
        let project = dir.path();
        let rules_dir = project.join(".claude").join("rules");
        let nested = rules_dir.join("topics");
        fs::create_dir_all(&nested).unwrap();
        fs::write(rules_dir.join("style.md"), "be terse").unwrap();
        fs::write(nested.join("ts.md"), "no any").unwrap();

        let cwd = project.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_claude_rules(&project_cwds);

        let style = sessions
            .iter()
            .find(|s| crate::testutils::norm(&s.path).ends_with("style.md") && s.project == cwd)
            .expect("project-level rule style.md should be picked up");
        assert_eq!(style.source, "Memory");
        assert_eq!(style.preview, "claude");

        let nested_entry = sessions
            .iter()
            .find(|s| crate::testutils::norm(&s.path).ends_with("topics/ts.md") && s.project == cwd)
            .expect("nested rule ts.md should be picked up recursively");
        assert_eq!(nested_entry.source, "Memory");
        assert_eq!(nested_entry.preview, "claude");
    }

    #[test]
    fn derive_memory_project_label_handles_claude_rules_under_project() {
        let dir = TestDir::new("derive-project-rules");
        let project = dir.path();
        let path = project.join(".claude").join("rules").join("style.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "body").unwrap();
        let label = derive_memory_project_label(&path);
        assert_eq!(label, project.to_string_lossy());
    }

    #[test]
    fn scan_memory_walks_up_ancestors_to_git_root() {
        let dir = TestDir::new("ancestor-walk");
        let root = dir.path();
        // Layout: <root>/.git, <root>/AGENTS.md, <root>/sub/, <root>/sub/CLAUDE.md
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "root agents").unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("CLAUDE.md"), "sub claude").unwrap();

        // cwd is the deeper subdir; walk up should find the root AGENTS.md.
        let cwd = sub.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_memory(&project_cwds).unwrap();

        let root_agents_path = root.join("AGENTS.md").to_string_lossy().to_string();
        let root_agents = sessions
            .iter()
            .find(|s| s.path == root_agents_path)
            .expect("ancestor AGENTS.md should be picked up");
        assert_eq!(root_agents.preview, "codex,opencode");
        // The ancestor entry's project label should be the ancestor dir itself,
        // not the deeper cwd.
        assert_eq!(root_agents.project, root.to_string_lossy());

        let sub_claude_path = sub.join("CLAUDE.md").to_string_lossy().to_string();
        let sub_claude = sessions
            .iter()
            .find(|s| s.path == sub_claude_path)
            .expect("cwd CLAUDE.md should still be picked up");
        assert_eq!(sub_claude.preview, "claude,opencode");
        assert_eq!(sub_claude.project, cwd);
    }

    #[test]
    fn scan_memory_no_git_means_cwd_only() {
        let dir = TestDir::new("no-git");
        let root = dir.path();
        // Layout: <root>/AGENTS.md (in cwd) and <root>/sub/AGENTS.md.
        // No .git anywhere. Expect: only the deeper cwd's file is scanned,
        // not its parent's, since the source code in Codex/Gemini/OpenCode
        // refuses to ascend when no project-root marker is found.
        fs::write(root.join("AGENTS.md"), "parent agents").unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("AGENTS.md"), "sub agents").unwrap();

        let cwd = sub.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_memory(&project_cwds).unwrap();

        let sub_path = sub.join("AGENTS.md").to_string_lossy().to_string();
        assert!(
            sessions.iter().any(|s| s.path == sub_path),
            "cwd-level AGENTS.md should be picked up"
        );
        let parent_path = root.join("AGENTS.md").to_string_lossy().to_string();
        assert!(
            sessions.iter().all(|s| s.path != parent_path),
            "without .git the walk must not ascend to the parent dir"
        );
    }

    #[test]
    fn scan_memory_walk_stops_at_git_root() {
        let dir = TestDir::new("ancestor-walk-stop");
        let root = dir.path();
        // The git root is the cwd itself; we must NOT ascend above it.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "root agents").unwrap();
        // Create a file outside the git root that would falsely match if the
        // walk ascended too far. Place it in a parent dir of `root`.
        let above_marker = root.parent().unwrap().join("AGENTS.md");
        // Best effort — if the test runner's temp parent already has this file
        // we still validate the include/exclude logic via path checks below.
        let _ = fs::write(&above_marker, "outside");

        let cwd = root.to_string_lossy().to_string();
        let project_cwds: HashSet<String> = [cwd.clone()].into_iter().collect();
        let sessions = scan_memory(&project_cwds).unwrap();

        let inside_path = root.join("AGENTS.md").to_string_lossy().to_string();
        assert!(
            sessions.iter().any(|s| s.path == inside_path),
            "git root AGENTS.md should be present"
        );
        let outside_path = above_marker.to_string_lossy().to_string();
        assert!(
            sessions.iter().all(|s| s.path != outside_path),
            "ancestor walk must not ascend above the git root"
        );

        // Clean up the polluted parent file so we don't bleed into other tests.
        let _ = fs::remove_file(&above_marker);
    }

    #[test]
    fn parse_doc_file_emits_skill_kind_for_skill_source() {
        let dir = TestDir::new("parse-skill");
        let path = dir.path().join("SKILL.md");
        fs::write(
            &path,
            "---\nname: example\ndescription: a skill\n---\nbody text",
        )
        .unwrap();
        let detail = parse_doc_file(&path, "Skill").unwrap();
        assert_eq!(detail.session.source, "Skill");
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.messages[0].kind, "skill");
        assert_eq!(detail.messages[0].text, "body text");
    }

    #[test]
    fn claude_session_aggregates_token_usage_and_model() {
        // Synthetic Claude resume JSONL with two assistant messages
        // each carrying a `message.usage` block. Token totals should
        // sum across messages; cache_read + cache_creation should be
        // folded into the `cached` counter. Model is taken from the
        // last seen `message.model`.
        let dir = TestDir::new("claude-tokens");
        // Filename has to be UUID-like to pass `is_claude_session_file`.
        let path = dir
            .path()
            .join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
        let lines = [
            r#"{"sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","cwd":"/work","timestamp":"2026-05-29T10:00:00Z","customTitle":"Token test","message":{"role":"user","content":"hi"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:01Z","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":120,"output_tokens":40,"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}"#,
            r#"{"timestamp":"2026-05-29T10:00:02Z","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"more"}],"usage":{"input_tokens":80,"output_tokens":25,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let detail = parse_claude_session(&path).unwrap();
        let tokens = detail
            .session
            .tokens
            .as_ref()
            .expect("Claude session should expose token totals");
        assert_eq!(tokens.input, 200);
        assert_eq!(tokens.output, 65);
        // 10 + 5 + 0 + 0
        assert_eq!(tokens.cached, 15);
        assert_eq!(tokens.reasoning, 0);
        assert_eq!(tokens.total, 200 + 65 + 15);
        assert_eq!(detail.session.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn opencode_db_reads_token_columns_and_model() {
        // Build a minimal `session` table with all 5 token columns +
        // model JSON. Verify the per-id read picks them up correctly,
        // and that the JSON `model.id` is what surfaces on AppSession.
        let dir = TestDir::new("opencode-tokens");
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                directory text,
                title text,
                time_created integer,
                time_updated integer,
                time_archived integer,
                parent_id text,
                tokens_input integer default 0,
                tokens_output integer default 0,
                tokens_reasoning integer default 0,
                tokens_cache_read integer default 0,
                tokens_cache_write integer default 0,
                model text
            );
            create table message (
                id text primary key,
                session_id text,
                time_created integer,
                data text
            );
            create table part (
                id text primary key,
                message_id text,
                session_id text,
                type text,
                data text
            );",
        )
        .unwrap();
        conn.execute(
            "insert into session values \
             ('sess-1', '/work', 'My session', 1714521600000, 1714521700000, \
              null, null, \
              200, 80, 12, 30, 18, '{\"id\":\"claude-opus-4-7\",\"providerID\":\"anthropic\"}')",
            [],
        )
        .unwrap();
        drop(conn);
        let detail = parse_opencode_db_session(&db_path, "sess-1").unwrap();
        let t = detail
            .session
            .tokens
            .as_ref()
            .expect("OpenCode session should expose token totals");
        assert_eq!(t.input, 200);
        assert_eq!(t.output, 80);
        assert_eq!(t.reasoning, 12);
        // cache_read + cache_write
        assert_eq!(t.cached, 48);
        assert_eq!(t.total, 200 + 80 + 12 + 48);
        assert_eq!(detail.session.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn opencode_extracts_per_day_tokens_from_step_finish_parts() {
        // Real OpenCode schema (with time_created column on `part`) +
        // two step-finish parts on different days. Expectation:
        // each day shows up as its own DailyTokenBreakdown entry with
        // the matching tokens. Other part types (text/tool/etc.) and
        // step-finish without tokens are ignored.
        let dir = TestDir::new("opencode-daily-tokens");
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                directory text,
                title text,
                time_created integer,
                time_updated integer,
                time_archived integer,
                parent_id text,
                tokens_input integer default 0,
                tokens_output integer default 0,
                tokens_reasoning integer default 0,
                tokens_cache_read integer default 0,
                tokens_cache_write integer default 0
            );
            create table message (
                id text primary key,
                session_id text,
                time_created integer,
                data text
            );
            create table part (
                id text primary key,
                message_id text,
                session_id text,
                time_created integer,
                time_updated integer,
                data text
            );",
        )
        .unwrap();
        // Session metadata
        conn.execute(
            "insert into session values \
             ('sess-daily', '/work', 'Multi day', \
              1748131200000, 1748390400000, null, null, \
              300, 80, 10, 50, 0)",
            [],
        )
        .unwrap();
        // 2026-05-25 in UTC = `1748131200000` ms — step-finish with
        // 200 input, 50 output, 20 reasoning, 30 cache read, 10 cache write
        let step1 = r#"{"type":"step-finish","time":{"created":1748131200000},"cost":0.05,"tokens":{"input":200,"output":50,"reasoning":20,"cache":{"read":30,"write":10}}}"#;
        // 2026-05-27 in UTC = `1748304000000` ms — step-finish with
        // 100 input, 30 output, 0 reasoning, 10 cache read, 0 cache write
        let step2 = r#"{"type":"step-finish","time":{"created":1748304000000},"cost":0.02,"tokens":{"input":100,"output":30,"reasoning":0,"cache":{"read":10,"write":0}}}"#;
        // Non step-finish part with no tokens — should be IGNORED
        let irrelevant = r#"{"type":"text","time":{"created":1748131200001},"text":"hello"}"#;
        // step-finish with all-zero tokens — should be IGNORED
        let zero_step = r#"{"type":"step-finish","time":{"created":1748390400000},"cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#;
        for (id, time, data) in [
            ("p-1", 1748131200000i64, step1),
            ("p-2", 1748131200001i64, irrelevant),
            ("p-3", 1748304000000i64, step2),
            ("p-4", 1748390400000i64, zero_step),
        ] {
            conn.execute(
                "insert into part values (?1, 'm-1', 'sess-daily', ?2, ?2, ?3)",
                rusqlite::params![id, time, data],
            )
            .unwrap();
        }
        drop(conn);

        let detail = parse_opencode_db_session(&db_path, "sess-daily").unwrap();
        let daily = detail
            .session
            .daily_tokens
            .expect("daily_tokens should be populated from step-finish parts");
        // Two distinct days emerged. Date keys may shift by ±1 day
        // depending on the test machine's local TZ vs UTC, but we
        // can verify the TOTAL is preserved.
        assert_eq!(daily.len(), 2);
        let total_input: u64 = daily.iter().map(|d| d.tokens.input).sum();
        let total_output: u64 = daily.iter().map(|d| d.tokens.output).sum();
        let total_cached: u64 = daily.iter().map(|d| d.tokens.cached).sum();
        let total_reasoning: u64 = daily.iter().map(|d| d.tokens.reasoning).sum();
        assert_eq!(total_input, 300);
        assert_eq!(total_output, 80);
        assert_eq!(total_cached, 50); // 30+10+10+0 from the two real entries
        assert_eq!(total_reasoning, 20);
        // The larger day should be the step1 (May 25 = 300 total),
        // the smaller is step2 (May 27 = 140 total). Verify they
        // split correctly.
        let larger = daily.iter().max_by_key(|d| d.tokens.total).unwrap();
        let smaller = daily.iter().min_by_key(|d| d.tokens.total).unwrap();
        assert_eq!(larger.tokens.input, 200);
        assert_eq!(larger.tokens.output, 50);
        assert_eq!(smaller.tokens.input, 100);
        assert_eq!(smaller.tokens.output, 30);
    }

    #[test]
    fn opencode_daily_tokens_none_when_no_step_finish() {
        // Sessions with only text/tool parts (no step-finish records)
        // should yield daily_tokens: None so the frontend falls back
        // to even-distribution rather than reporting "0 tokens, all days".
        let dir = TestDir::new("opencode-no-step-finish");
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                directory text,
                title text,
                time_created integer,
                time_updated integer,
                time_archived integer,
                parent_id text,
                tokens_input integer default 0,
                tokens_output integer default 0
            );
            create table message (id text primary key, session_id text, time_created integer, data text);
            create table part (
                id text primary key,
                message_id text,
                session_id text,
                time_created integer,
                time_updated integer,
                data text
            );",
        )
        .unwrap();
        conn.execute(
            "insert into session values \
             ('sess-no-sf', '/work', 'Just text', 1748131200000, 1748131200000, null, null, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into part values \
             ('p-1', 'm-1', 'sess-no-sf', 1748131200000, 1748131200000, \
              '{\"type\":\"text\",\"text\":\"hello\"}')",
            [],
        )
        .unwrap();
        drop(conn);
        let detail = parse_opencode_db_session(&db_path, "sess-no-sf").unwrap();
        assert!(detail.session.daily_tokens.is_none());
    }

    #[test]
    fn opencode_db_returns_none_tokens_when_all_zero() {
        let dir = TestDir::new("opencode-zero-tokens");
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "create table session (
                id text primary key,
                directory text,
                title text,
                time_created integer,
                time_updated integer,
                time_archived integer,
                parent_id text,
                tokens_input integer default 0,
                tokens_output integer default 0,
                tokens_reasoning integer default 0,
                tokens_cache_read integer default 0,
                tokens_cache_write integer default 0
            );
            create table message (id text primary key, session_id text, time_created integer, data text);
            create table part (id text primary key, message_id text, session_id text, type text, data text);",
        )
        .unwrap();
        conn.execute(
            "insert into session values \
             ('sess-zero', '/work', 'Empty', 1714521600000, 1714521700000, \
              null, null, 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);
        let detail = parse_opencode_db_session(&db_path, "sess-zero").unwrap();
        assert!(detail.session.tokens.is_none());
    }

    #[test]
    fn gemini_extract_token_usage_sums_per_record() {
        // Synthetic Gemini records. Sum input/output/cached/thoughts
        // across rows; verify the `total` field overrides component
        // sum when present.
        let records = vec![
            serde_json::json!({
                "type": "user",
                "content": "hi",
            }),
            serde_json::json!({
                "type": "gemini",
                "model": "gemini-3-flash-preview",
                "tokens": {
                    "input": 100, "output": 20, "cached": 10,
                    "thoughts": 5, "total": 135
                },
            }),
            serde_json::json!({
                "type": "gemini",
                "model": "gemini-3-flash-preview",
                "tokens": {
                    "input": 50, "output": 10, "cached": 0,
                    "thoughts": 2, "total": 62
                },
            }),
        ];
        let (stats, model, _) = gemini_extract_token_usage(&records);
        let s = stats.unwrap();
        assert_eq!(s.input, 150);
        assert_eq!(s.output, 30);
        assert_eq!(s.cached, 10);
        assert_eq!(s.reasoning, 7);
        // Per-record `total` sum is used when present.
        assert_eq!(s.total, 197);
        assert_eq!(model.as_deref(), Some("gemini-3-flash-preview"));
    }

    #[test]
    fn gemini_extract_token_usage_returns_none_without_tokens() {
        let records = vec![
            serde_json::json!({"type": "user", "content": "hi"}),
            serde_json::json!({"type": "gemini", "content": "hello"}),
        ];
        let (stats, _, _) = gemini_extract_token_usage(&records);
        assert!(stats.is_none());
    }

    #[test]
    fn gemini_scan_metadata_only_carries_token_usage() {
        // Stats page consumes the scan-time AppSession, which loads
        // each session with metadata_only=true. Before this change the
        // metadata_only path dropped the `messages` array, so the
        // token accumulator saw zero records → tokens=None and the
        // Stats widgets stayed empty. Verify both the scan path and a
        // detail load expose the same numbers.
        let dir = TestDir::new("gemini-scan-tokens");
        let project_dir = dir.path().join("tmp").join("token-project");
        let chats_dir = project_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(project_dir.join(".project_root"), "/workspace/tokens").unwrap();
        let path = chats_dir.join("session-2026-05-30T00-00-aaaaaaaa.jsonl");
        fs::write(
            &path,
            r#"{"sessionId":"tok-session","projectHash":"hash","startTime":"2026-05-30T00:00:00Z","lastUpdated":"2026-05-30T00:02:00Z","kind":"main"}"#.to_string()
                + "\n"
                + r#"{"id":"u-1","type":"user","content":[{"text":"hi"}]}"#
                + "\n"
                + r#"{"id":"a-1","type":"gemini","model":"gemini-3-pro","content":"hello","tokens":{"input":100,"output":20,"cached":10,"thoughts":5,"total":135}}"#
                + "\n"
                + r#"{"id":"a-2","type":"gemini","model":"gemini-3-pro","content":"more","tokens":{"input":50,"output":10,"cached":0,"thoughts":2,"total":62}}"#,
        )
        .unwrap();

        let scanned = scan_gemini_chats_dir(&chats_dir).unwrap();
        assert_eq!(scanned.len(), 1);
        let tokens = scanned[0]
            .tokens
            .as_ref()
            .expect("scan-time token usage should be populated");
        assert_eq!(tokens.input, 150);
        assert_eq!(tokens.output, 30);
        assert_eq!(tokens.cached, 10);
        assert_eq!(tokens.reasoning, 7);
        assert_eq!(tokens.total, 197);
        assert_eq!(scanned[0].model.as_deref(), Some("gemini-3-pro"));

        let detail = parse_gemini_jsonl_session(&path).unwrap();
        assert_eq!(detail.session.tokens, scanned[0].tokens);
        assert_eq!(detail.session.model, scanned[0].model);
    }

    #[test]
    fn codex_session_uses_last_token_count_event() {
        // Codex emits periodic `token_count` events with cumulative
        // `total_token_usage`. The first event has `info: null` (no
        // data yet); we should ignore it and use the second event's
        // numbers. Model comes from `turn_context.payload.model` (SessionMeta
        // has no model field in current Codex — only model_provider).
        let dir = TestDir::new("codex-tokens");
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"codex-thread-1","cwd":"/work","model_provider":"openai"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-5"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
            r#"{"timestamp":"2026-05-29T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15000,"cached_input_tokens":6500,"output_tokens":300,"reasoning_output_tokens":70,"total_tokens":21870}}}}"#,
            r#"{"timestamp":"2026-05-29T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20000,"cached_input_tokens":8000,"output_tokens":500,"reasoning_output_tokens":120,"total_tokens":28620}}}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let detail = parse_codex_session(&path, "codex-thread-1").unwrap();
        let tokens = detail
            .session
            .tokens
            .as_ref()
            .expect("Codex session should expose latest token usage");
        // Latest event wins (cumulative semantics).
        assert_eq!(tokens.input, 20000);
        assert_eq!(tokens.output, 500);
        assert_eq!(tokens.cached, 8000);
        assert_eq!(tokens.reasoning, 120);
        assert_eq!(tokens.total, 28620);
        assert_eq!(detail.session.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn claude_session_skips_token_block_when_absent() {
        // Same shape minus `usage` — verifies `has_token_data` gates
        // the Option<TokenStats> instead of returning "all zeros".
        let dir = TestDir::new("claude-no-tokens");
        let path = dir
            .path()
            .join("ffffffff-eeee-dddd-cccc-bbbbbbbbbbbb.jsonl");
        let line = r#"{"sessionId":"ffffffff-eeee-dddd-cccc-bbbbbbbbbbbb","cwd":"/work","timestamp":"2026-05-29T10:00:00Z","customTitle":"No usage","message":{"role":"user","content":"hi"}}"#;
        fs::write(&path, line).unwrap();
        let detail = parse_claude_session(&path).unwrap();
        assert!(detail.session.tokens.is_none());
    }

    #[test]
    fn extract_codex_tokens_matches_parse_codex_session() {
        // The scan-time extractor should yield identical numbers to the
        // detail-time parser for the same rollout, so Stats and detail
        // never disagree. Uses the same multi-event fixture as
        // codex_session_uses_last_token_count_event.
        let dir = TestDir::new("codex-scan-tokens");
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"codex-thread-1","cwd":"/work","model_provider":"openai"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-5"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
            r#"{"timestamp":"2026-05-29T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15000,"cached_input_tokens":6500,"output_tokens":300,"reasoning_output_tokens":70,"total_tokens":21870}}}}"#,
            r#"{"timestamp":"2026-05-29T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20000,"cached_input_tokens":8000,"output_tokens":500,"reasoning_output_tokens":120,"total_tokens":28620}}}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let extract = codex_scan_extract(&path);
        let (tokens, model, daily) = (extract.tokens, extract.model, extract.daily_tokens);
        let t = tokens.expect("scan-time extractor should find token_count event");
        assert_eq!(t.input, 20000);
        assert_eq!(t.output, 500);
        assert_eq!(t.cached, 8000);
        assert_eq!(t.reasoning, 120);
        assert_eq!(t.total, 28620);
        assert_eq!(model.as_deref(), Some("gpt-5"));
        // All three token_count events landed on 2026-05-29 in UTC.
        // The first (info: null) is ignored; second and third both
        // sum into the same date's bucket (delta from previous).
        let daily = daily.expect("daily breakdown should be populated");
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].date.len(), 10); // YYYY-MM-DD shape
                                             // Final delta should equal the final cumulative numbers since
                                             // the first event was null (no prior baseline).
        assert_eq!(daily[0].tokens.input, 20000);
        assert_eq!(daily[0].tokens.output, 500);
        assert_eq!(daily[0].tokens.cached, 8000);
        assert_eq!(daily[0].tokens.reasoning, 120);
    }

    #[test]
    fn codex_model_reads_last_turn_context_when_switched() {
        // A /model switch mid-session re-emits turn_context. We surface the
        // model the session ENDED with (last-seen wins) — the model is read
        // from the session's own history, never the live Codex config.
        let dir = TestDir::new("codex-model-switch");
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"t","cwd":"/w","model_provider":"openai"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:00Z","type":"turn_context","payload":{"cwd":"/w","model":"gpt-5.4"}}"#,
            r#"{"timestamp":"2026-05-29T10:05:00Z","type":"turn_context","payload":{"cwd":"/w","model":"gpt-5.5"}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        // Both the scan-time extractor and the detail parser agree on last-seen.
        assert_eq!(codex_scan_extract(&path).model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            parse_codex_session(&path, "t")
                .unwrap()
                .session
                .model
                .as_deref(),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn extract_codex_tokens_splits_deltas_across_days() {
        // Two token_count events: one on May 28 (cumulative 100/50),
        // one on May 30 (cumulative 300/150). Daily breakdown should
        // attribute the FIRST event's totals to May 28 and the DELTA
        // (200/100) to May 30.
        let dir = TestDir::new("codex-daily-deltas");
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"x","cwd":"/w","model":"gpt-5"}}"#,
            r#"{"timestamp":"2026-05-28T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":50,"cached_input_tokens":0,"reasoning_output_tokens":0,"total_tokens":150}}}}"#,
            r#"{"timestamp":"2026-05-30T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"output_tokens":150,"cached_input_tokens":0,"reasoning_output_tokens":0,"total_tokens":450}}}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let daily = codex_scan_extract(&path).daily_tokens;
        let daily = daily.expect("daily breakdown should be populated");
        assert_eq!(daily.len(), 2);
        // The two date keys may not be strictly "2026-05-28" / "30"
        // depending on the test machine's local timezone (the JSONL
        // timestamps are UTC). We just verify the totals split.
        let total_input: u64 = daily.iter().map(|d| d.tokens.input).sum();
        let total_output: u64 = daily.iter().map(|d| d.tokens.output).sum();
        assert_eq!(total_input, 300);
        assert_eq!(total_output, 150);
        // The day with the smaller numbers should match the first event.
        let smaller = daily.iter().min_by_key(|d| d.tokens.input).unwrap();
        assert_eq!(smaller.tokens.input, 100);
        assert_eq!(smaller.tokens.output, 50);
    }

    #[test]
    fn extract_claude_tokens_accumulates_message_usage() {
        // Two assistant messages, each with a `usage` block. Scan-time
        // extractor must sum input/output and combine
        // cache_read + cache_creation into `cached`, exactly matching
        // collect_claude_metadata's logic.
        let dir = TestDir::new("claude-scan-tokens");
        let path = dir.path().join("session.jsonl");
        let lines = [
            r#"{"sessionId":"s","cwd":"/work","timestamp":"2026-05-29T10:00:00Z","customTitle":"x","message":{"role":"user","content":"hi"}}"#,
            r#"{"message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":30,"cache_creation_input_tokens":10}}}"#,
            r#"{"message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input_tokens":200,"output_tokens":75,"cache_read_input_tokens":40,"cache_creation_input_tokens":0}}}"#,
            // Synthetic placeholder should NOT overwrite the real model.
            r#"{"message":{"role":"assistant","model":"<synthetic>"}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let extract = claude_scan_extract(&path);
        let (tokens, model) = (extract.tokens, extract.model);
        let t = tokens.expect("scan-time extractor should find usage blocks");
        assert_eq!(t.input, 300);
        assert_eq!(t.output, 125);
        assert_eq!(t.cached, 80); // 30+10+40+0
        assert_eq!(t.reasoning, 0);
        assert_eq!(t.total, 505);
        assert_eq!(model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn extract_claude_tokens_buckets_by_message_timestamp() {
        // Three assistant messages on three different days. Each one's
        // usage block should bucket to its own date in the daily
        // breakdown — the multi-day attribution bug we set out to fix.
        let dir = TestDir::new("claude-daily");
        let path = dir.path().join("session.jsonl");
        let lines = [
            r#"{"sessionId":"s","cwd":"/w","timestamp":"2026-05-26T08:00:00Z","customTitle":"x","message":{"role":"user","content":"hi"}}"#,
            r#"{"timestamp":"2026-05-26T08:00:01Z","message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            r#"{"timestamp":"2026-05-28T10:00:00Z","message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input_tokens":200,"output_tokens":75}}}"#,
            r#"{"timestamp":"2026-05-30T12:00:00Z","message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input_tokens":400,"output_tokens":100,"cache_read_input_tokens":50}}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let extract = claude_scan_extract(&path);
        let (tokens, daily) = (extract.tokens, extract.daily_tokens);
        let t = tokens.unwrap();
        // Aggregate matches the previous behavior — sum across all msgs.
        assert_eq!(t.input, 700);
        assert_eq!(t.output, 225);
        assert_eq!(t.cached, 50);
        assert_eq!(t.total, 975);
        let daily = daily.expect("Claude daily breakdown should be populated");
        // Three distinct days emerge — verify they're chronological.
        assert_eq!(daily.len(), 3);
        for d in &daily {
            assert_eq!(d.date.len(), 10);
        }
        let day1_input = daily.iter().map(|d| d.tokens.input).min().unwrap();
        let day3_input = daily.iter().map(|d| d.tokens.input).max().unwrap();
        assert_eq!(day1_input, 100);
        assert_eq!(day3_input, 400);
        let cached_total: u64 = daily.iter().map(|d| d.tokens.cached).sum();
        assert_eq!(cached_total, 50);
    }

    #[test]
    fn claude_scan_extract_counts_visible_messages() {
        // The visible message count now comes from the same streamed
        // pass as token extraction (it used to be a separate uncached
        // full-file parse). Count = messages of kind TEXT, exactly as
        // the detail path's `visible_message_count`.
        let dir = TestDir::new("claude-scan-count");
        let path = dir.path().join("session.jsonl");
        let lines = [
            r#"{"type":"user","sessionId":"s","cwd":"/w","timestamp":"2026-05-29T10:00:00Z","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"assistant","timestamp":"2026-05-29T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            // tool_use is kind=tool, not TEXT — must not count.
            r#"{"type":"assistant","timestamp":"2026-05-29T10:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let extract = claude_scan_extract(&path);
        assert_eq!(extract.message_count, 2);
        let tokens = extract.tokens.expect("usage block should be picked up");
        assert_eq!(tokens.input, 10);
        assert_eq!(tokens.output, 5);
    }

    #[test]
    fn codex_scan_extract_counts_visible_messages() {
        // Same single-pass rule for Codex rollouts: count every record
        // `codex_message_from_value` surfaces, alongside token totals.
        let dir = TestDir::new("codex-scan-count");
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"x","cwd":"/w","model":"gpt-5"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:01Z","type":"event_msg","payload":{"type":"agent_message","message":"hi there"}}"#,
            r#"{"timestamp":"2026-05-29T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":50,"cached_input_tokens":0,"reasoning_output_tokens":0,"total_tokens":150}}}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let extract = codex_scan_extract(&path);
        assert_eq!(extract.message_count, 2);
        let tokens = extract
            .tokens
            .expect("token_count event should be picked up");
        assert_eq!(tokens.input, 100);
    }

    #[test]
    fn cached_scan_extract_reuses_result_for_same_mtime() {
        // Verify the cache short-circuits the extractor: pass a closure
        // that increments a counter, call it twice with the same path,
        // assert the counter is 1 and the cached bundle (including
        // message_count) round-trips intact.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = TestDir::new("scan-token-cache");
        let path = dir.path().join("session.jsonl");
        fs::write(&path, "irrelevant").unwrap();
        let calls = AtomicUsize::new(0);
        let extract = |_: &Path| -> ScanExtract {
            calls.fetch_add(1, Ordering::SeqCst);
            ScanExtract {
                tokens: Some(TokenStats {
                    input: 1,
                    output: 1,
                    cached: 0,
                    reasoning: 0,
                    total: 2,
                }),
                model: None,
                daily_tokens: None,
                message_count: 7,
            }
        };
        // First call: cache miss → extractor runs.
        let first = cached_scan_extract(&path, extract);
        assert_eq!(first.message_count, 7);
        // Second call with the SAME mtime: cache hit → extractor must
        // NOT re-run. (Closure is move'd, so spin a fresh one that we
        // expect never to fire.)
        let extract_should_skip = |_: &Path| -> ScanExtract {
            panic!("cache hit should bypass extractor");
        };
        let second = cached_scan_extract(&path, extract_should_skip);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.message_count, 7);
    }

    #[test]
    fn evict_stale_scan_cache_drops_deleted_files_keeps_live_ones() {
        // Seed the cache with one live file and one file that gets
        // deleted; eviction must drop only the deleted one.
        let dir = TestDir::new("scan-cache-evict");
        let live = dir.path().join("live.jsonl");
        let doomed = dir.path().join("doomed.jsonl");
        fs::write(&live, "x").unwrap();
        fs::write(&doomed, "x").unwrap();
        let seed = |_: &Path| -> ScanExtract {
            ScanExtract {
                message_count: 1,
                ..ScanExtract::default()
            }
        };
        let _ = cached_scan_extract(&live, seed);
        let _ = cached_scan_extract(&doomed, seed);
        fs::remove_file(&doomed).unwrap();

        evict_stale_scan_cache();

        let cache = SCAN_EXTRACT_CACHE.lock().unwrap();
        assert!(cache.contains_key(&live), "live file entry must survive");
        assert!(
            !cache.contains_key(&doomed),
            "deleted file entry must be evicted"
        );
    }

    #[test]
    fn apply_source_toggles_filters_sessions_projects_and_tags() {
        let mk = |source: &str, preview: &str| AppSession {
            source: source.to_string(),
            preview: preview.to_string(),
            ..Default::default()
        };
        let mut records = vec![
            mk("Codex", ""),
            mk("Claude", ""),
            // Multi-tagged memory: the disabled tag drops, the record stays.
            mk("Memory", "codex,opencode"),
            // Single-tagged memory of a disabled source: whole record drops.
            mk("Skill", "codex"),
            mk("Memory", "claude,opencode"),
        ];
        let mut projects = vec![
            Project {
                source: "Codex".into(),
                project: "/a".into(),
            },
            Project {
                source: "Claude".into(),
                project: "/b".into(),
            },
        ];
        let disabled: HashSet<String> = ["codex".to_string()].into();
        apply_source_toggles(&mut projects, &mut records, &disabled);

        let sources: Vec<&str> = records.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(sources, vec!["Claude", "Memory", "Memory"]);
        assert_eq!(records[1].preview, "opencode");
        assert_eq!(records[2].preview, "claude,opencode");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].source, "Claude");

        // Empty disabled set = untouched fast path.
        let before = records.len();
        apply_source_toggles(&mut projects, &mut records, &HashSet::new());
        assert_eq!(records.len(), before);

        // Convention pin: every session source name lowercases EXACTLY to
        // its cli key — the mapping apply_source_toggles relies on.
        // ("claude-desktop" has no session source, so the hyphenated key
        // is deliberately absent here.)
        for (source, key) in [
            ("Codex", "codex"),
            ("Claude", "claude"),
            ("Gemini", "gemini"),
            ("OpenCode", "opencode"),
            ("Grok", "grok"),
        ] {
            assert_eq!(source.to_ascii_lowercase(), key);
        }
    }

    #[test]
    fn grok_scan_and_parse_real_shaped_session() {
        let dir = std::env::temp_dir().join(format!(
            "termory-grok-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Layout: <root>/<encoded-cwd>/<uuid>/{summary.json, updates.jsonl}
        let sess = dir.join("%2Fwork%2Fapp").join("019f-uuid");
        fs::create_dir_all(&sess).unwrap();
        fs::write(
            sess.join("summary.json"),
            r#"{"info":{"id":"019f-uuid","cwd":"/work/app"},
                "session_summary":"Fix the bug",
                "created_at":"2026-07-09T12:23:44.424534Z",
                "updated_at":"2026-07-09T14:36:27.912118Z",
                "num_messages":165,"num_chat_messages":70,
                "current_model_id":"grok-4.5"}"#,
        )
        .unwrap();
        // Chunked user + thought + agent text, a tool call with a
        // completed envelope output, and a failed tool call.
        let updates = [
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Fix "}}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"the bug"}}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Look first"}}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"read_file","rawInput":{"target_file":"src/a.rs","limit":100},"_meta":{"x.ai/tool":{"name":"read_file","label":"Read"}}}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"in_progress"}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","rawOutput":{"type":"ReadFile","Content":{"content":"fn main() {}"}}}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"c2","title":"run_terminal_command","rawInput":{"command":"cargo test"},"_meta":{"x.ai/tool":{"name":"run_terminal_command","label":"Run Command"}}}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"c2","status":"failed","rawOutput":"error: it broke"}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"plan","entries":[{"content":"step one","status":"completed"},{"content":"step two","status":"in_progress"}]}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"turn_completed"}}}"#,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Done."}}}}"#,
        ]
        .join("\n");
        fs::write(sess.join("updates.jsonl"), updates).unwrap();
        // Noise the scanner must skip.
        fs::write(dir.join("session_search.sqlite"), b"").unwrap();
        fs::write(
            dir.join("%2Fwork%2Fapp").join("prompt_history.jsonl"),
            b"{}",
        )
        .unwrap();

        // Scan: list metadata verbatim from summary.json.
        let scanned = scan_grok_sessions_in(&dir).unwrap();
        assert_eq!(scanned.len(), 1);
        let s = &scanned[0];
        assert_eq!(s.source, "Grok");
        assert_eq!(s.id, "019f-uuid");
        assert_eq!(s.title, "Fix the bug");
        assert_eq!(s.project, "/work/app");
        assert_eq!(s.message_count, 70);
        assert_eq!(s.model.as_deref(), Some("grok-4.5"));
        assert_eq!(s.started_at.as_deref(), Some("2026-07-09T12:23:44.424534Z"));

        // Parse: chunk merging + unified tool cards.
        let detail = parse_grok_session(&sess.join("updates.jsonl")).unwrap();
        let texts: Vec<&str> = detail.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts[0], "Fix the bug"); // merged user chunks
        assert_eq!(texts[1], "> *Look first*"); // reasoning blockquote
        assert_eq!(
            texts[2],
            "⏺ **Read**(`src/a.rs`)\n\n````\nfn main() {}\n````"
        );
        assert_eq!(
            texts[3],
            "✗ **Run Command**(`cargo test`)\n\n````\nerror: it broke\n````"
        );
        assert_eq!(detail.messages[3].kind, "tool_error");
        assert_eq!(texts[4], "**Plan**\n- [x] step one\n- [~] step two");
        assert_eq!(texts[5], "Done."); // turn_completed skipped
        assert_eq!(detail.session.id, "019f-uuid");

        fs::remove_dir_all(&dir).unwrap();
    }
}
