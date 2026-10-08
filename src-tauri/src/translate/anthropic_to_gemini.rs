//! Anthropic Messages (client) ⇄ Gemini generateContent (upstream) translator.
//!
//! Faithful port of CLIProxyAPI `internal/translator/gemini/claude/` at commit
//! `ed980be` (`gemini_claude_request.go`, `gemini_claude_response.go`), plus the
//! helpers they call in other packages (`translator/common`,
//! `translator/gemini/common`, `util`, `registry`). Each ported function
//! carries a `// port of <GoFunc> (<file>)` comment so the two can be diffed
//! when upstream moves.
//!
//! The Go code works on raw bytes through gjson/sjson; this port works on
//! `serde_json::Value`. The gjson coercion rules the Go code relies on
//! (`.String()` / `.Int()` / `.Bool()` on any JSON type, `.Exists()` being
//! true for an explicit `null`) are reproduced by the `g*` helpers below.
//!
//! Deliberately NOT ported: the `model(level)` thinking-suffix parsing, the
//! count_tokens response (`ClaudeTokenCount`), metrics/logging, config hooks,
//! and `ConvertClaudeRequestToGeminiWithCompat` (only the executor's
//! multi-agent path uses it; the registered translator is the plain form,
//! which drops assistant `thinking` blocks).

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// gjson-compatible accessors
// ---------------------------------------------------------------------------

/// gjson path lookup (`a.b.0.c`): object keys, numeric segments index arrays.
fn gget<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// gjson `Result.String()` of a number: an all-digit literal verbatim, any
/// other number through `strconv.FormatFloat(f, 'f', -1, 64)`.
fn gnum_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        // Rust's `Display` for f64 is the shortest round-trip form without an
        // exponent — the same digits as Go's 'f' / -1.
        format!("{}", n.as_f64().unwrap_or(0.0))
    }
}

/// gjson `Result.String()`: strings verbatim, scalars as their literal, null /
/// missing as "", objects and arrays as their JSON text.
fn gstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => gnum_str(n),
        Some(other) => other.to_string(),
    }
}

/// gjson `Result.Int()`.
fn gint(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().map(|f| f as i64).unwrap_or(0)),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// gjson `Result.Bool()`.
fn gbool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// sjson writes a float64 with `strconv.FormatFloat(f, 'f', -1, 64)`, so an
/// integral value goes out as `1`, not `1.0`.
fn float_value(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

/// One Anthropic SSE frame. Go appends each event with three trailing
/// newlines into one buffer per chunk (`AppendSSEEventString(…, 3)`); here
/// every event is its own frame ending in the blank line that terminates it.
fn sse_frame(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

// ---------------------------------------------------------------------------
// Path helpers (sjson-compatible set / delete on a segment path)
// ---------------------------------------------------------------------------

type Path = Vec<String>;

fn path_of(segs: &[&str]) -> Path {
    segs.iter().map(|s| s.to_string()).collect()
}

fn join(base: &[String], seg: &str) -> Path {
    let mut p = base.to_vec();
    p.push(seg.to_string());
    p
}

fn get_path<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn get_path_mut<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = match cur {
            Value::Object(m) => m.get_mut(seg)?,
            Value::Array(a) => a.get_mut(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// sjson `Set`: replaces an existing key in place, appends a new one, and
/// creates missing intermediate objects. An empty path replaces the root.
fn set_path(node: &mut Value, path: &[String], v: Value) {
    let Some((seg, rest)) = path.split_first() else {
        *node = v;
        return;
    };
    if let Value::Array(a) = node {
        let Ok(idx) = seg.parse::<usize>() else {
            return;
        };
        while a.len() <= idx {
            a.push(Value::Null);
        }
        set_path(&mut a[idx], rest, v);
        return;
    }
    if !node.is_object() {
        *node = Value::Object(Map::new());
    }
    let Some(m) = node.as_object_mut() else {
        return;
    };
    if rest.is_empty() {
        m.insert(seg.clone(), v);
        return;
    }
    let child = m.entry(seg.clone()).or_insert(Value::Null);
    set_path(child, rest, v);
}

/// sjson `Delete`: removes an object key (order of the rest preserved) or an
/// array element; a missing path is a no-op.
fn delete_path(root: &mut Value, path: &[String]) {
    let Some((last, parent)) = path.split_last() else {
        return;
    };
    match get_path_mut(root, parent) {
        Some(Value::Object(m)) => {
            m.shift_remove(last);
        }
        Some(Value::Array(a)) => {
            if let Ok(i) = last.parse::<usize>() {
                if i < a.len() {
                    a.remove(i);
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// util helpers
// ---------------------------------------------------------------------------

const CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

// port of IsClaudeCodeAttributionSystemText (util/claude_attribution.go)
fn is_claude_code_attribution_system_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX)
}

// port of SanitizeFunctionName (util/util.go)
fn sanitize_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    // functionNameSanitizer = [^a-zA-Z0-9_.:-] → "_" (one per rune).
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Every char is ASCII from here on, so byte slicing is safe.
    let first = sanitized.as_bytes()[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        if sanitized.len() >= 64 {
            sanitized.truncate(63);
        }
        sanitized.insert(0, '_');
    }
    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }
    sanitized
}

// port of CanonicalToolName (util/translator.go)
fn canonical_tool_name(name: &str) -> String {
    name.trim().trim_start_matches('_').to_lowercase()
}

// port of ToolNameMapFromClaudeRequest (util/translator.go)
fn tool_name_map_from_claude_request(raw: &Value) -> Option<HashMap<String, String>> {
    let tools = raw.get("tools")?.as_array()?;
    let mut out = HashMap::with_capacity(tools.len());
    for tool in tools {
        let mut name = gstr(gget(tool, "name")).trim().to_string();
        if name.is_empty() {
            name = gstr(gget(tool, "function.name")).trim().to_string();
        }
        if name.is_empty() {
            continue;
        }
        let key = canonical_tool_name(&name);
        if key.is_empty() {
            continue;
        }
        out.entry(key).or_insert(name);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// port of MapToolName (util/translator.go)
fn map_tool_name(map: Option<&HashMap<String, String>>, name: &str) -> String {
    let Some(map) = map else {
        return name.to_string();
    };
    if name.is_empty() {
        return String::new();
    }
    match map.get(&canonical_tool_name(name)) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => name.to_string(),
    }
}

// port of SanitizedToolNameMap (util/translator.go)
fn sanitized_tool_name_map(raw: &Value) -> Option<HashMap<String, String>> {
    let tools = raw.get("tools")?.as_array()?;
    let mut out = HashMap::new();
    for tool in tools {
        let name = gstr(gget(tool, "name")).trim().to_string();
        if name.is_empty() {
            continue;
        }
        let sanitized = sanitize_function_name(&name);
        if sanitized == name {
            continue;
        }
        // Collision: keep the first (Go logs a warning).
        out.entry(sanitized).or_insert(name);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// port of RestoreSanitizedToolName (util/translator.go)
fn restore_sanitized_tool_name(map: Option<&HashMap<String, String>>, name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    map.and_then(|m| m.get(name))
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

static CLAUDE_TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generated ids: wall-clock nanos plus a process-wide counter, the same shape
/// as Go's `toolu_%d_%d`.
fn generate_tool_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = CLAUDE_TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("toolu_{nanos}_{n}")
}

// port of SanitizeClaudeToolID (util/claude_tool_id.go)
fn sanitize_claude_tool_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        generate_tool_id()
    } else {
        s
    }
}

/// port of ClaudeToolResult (util/claude_tool_result.go). `result_raw` holds
/// the RAW-JSON form (`ResultIsRaw`), `result` the plain-string form.
struct ClaudeToolResult {
    result: String,
    result_raw: Option<Value>,
    images: Vec<(String, String)>,
}

// port of isClaudeBase64Image (util/claude_tool_result.go)
fn is_claude_base64_image(block: &Value) -> bool {
    gstr(gget(block, "type")) == "image" && gstr(gget(block, "source.type")) == "base64"
}

// port of claudeImageFromBlock (util/claude_tool_result.go)
fn claude_image_from_block(block: &Value) -> Option<(String, String)> {
    let data = gstr(gget(block, "source.data"));
    if data.is_empty() {
        return None;
    }
    Some((gstr(gget(block, "source.media_type")), data))
}

// port of ConvertClaudeToolResultContent (util/claude_tool_result.go)
fn convert_claude_tool_result_content(content: Option<&Value>) -> ClaudeToolResult {
    let empty = || ClaudeToolResult {
        result: String::new(),
        result_raw: None,
        images: Vec::new(),
    };
    match content {
        None => empty(),
        Some(Value::String(s)) => ClaudeToolResult {
            result: s.clone(),
            ..empty()
        },
        Some(Value::Array(blocks)) => {
            let mut images = Vec::new();
            let mut filtered = Vec::new();
            for block in blocks {
                if is_claude_base64_image(block) {
                    if let Some(img) = claude_image_from_block(block) {
                        images.push(img);
                    }
                    continue;
                }
                filtered.push(block.clone());
            }
            let result_raw = match filtered.len() {
                0 => None,
                1 => filtered.pop(),
                _ => Some(Value::Array(filtered)),
            };
            ClaudeToolResult {
                result: String::new(),
                result_raw,
                images,
            }
        }
        Some(obj @ Value::Object(_)) => {
            if is_claude_base64_image(obj) {
                return ClaudeToolResult {
                    images: claude_image_from_block(obj).into_iter().collect(),
                    ..empty()
                };
            }
            ClaudeToolResult {
                result_raw: Some(obj.clone()),
                ..empty()
            }
        }
        // number / bool / null: `content.Raw != ""` → raw.
        Some(other) => ClaudeToolResult {
            result_raw: Some(other.clone()),
            ..empty()
        },
    }
}

/// Static `ThinkingSupport.Max` of every model in the static registry
/// (`registry/models/models.json` + `devin_models.json` at ed980be, searched in
/// `LookupStaticModelInfo` order: claude, gemini, vertex, aistudio, codex-pro,
/// kimi, antigravity, xai, devin, meta — first match wins). Only entries with
/// a non-zero max are listed; every other id resolves to 0.
const STATIC_THINKING_MAX: &[(&str, i64)] = &[
    ("claude-haiku-4-5-20251001", 128000),
    ("claude-sonnet-4-5-20250929", 128000),
    ("claude-sonnet-4-6", 128000),
    ("claude-opus-4-6", 128000),
    ("claude-opus-4-7", 128000),
    ("claude-opus-4-8", 128000),
    ("claude-fable-5", 128000),
    ("claude-fable-5-1", 128000),
    ("claude-opus-4-5-20251101", 128000),
    ("claude-opus-4-1-20250805", 128000),
    ("claude-opus-4-20250514", 128000),
    ("claude-sonnet-4-20250514", 128000),
    ("claude-3-7-sonnet-20250219", 128000),
    ("gemini-2.5-pro", 32768),
    ("gemini-2.5-flash", 24576),
    ("gemini-2.5-flash-lite", 24576),
    ("gemini-3-pro-preview", 32768),
    ("gemini-3.1-pro-preview", 32768),
    ("gemini-3.1-flash-image-preview", 32768),
    ("gemini-3-flash-preview", 32768),
    ("gemini-3.1-flash-lite-preview", 32768),
    ("gemini-3-pro-image-preview", 32768),
    ("gemini-3.5-flash", 32768),
    ("gemini-3.5-flash-lite", 32768),
    ("gemini-3.6-flash", 32768),
    ("gemini-3.7-flash", 65535),
    ("gemini-3.8-flash", 65535),
    ("gemini-2.5-flash-image", 24576),
    ("gemini-3-pro", 32768),
    ("gemini-3-flash", 32768),
    ("gemini-3.1-pro", 32768),
    ("gemini-3.1-flash-image", 32768),
    ("gemini-3.1-flash-lite", 32768),
    ("gemini-3-pro-image", 32768),
    ("gemini-pro-latest", 32768),
    ("gemini-flash-latest", 24576),
    ("gemini-flash-lite-latest", 24576),
    ("claude-opus-4-6-thinking", 64000),
    ("gemini-3.6-flash-high", 65535),
    ("gemini-3.7-flash-high", 65535),
    ("gemini-3.8-flash-high", 65535),
    ("gemini-pro-agent", 65535),
    ("gemini-3.1-pro-low", 65535),
];

// port of LookupModelInfo(model, "gemini").Thinking.Max (registry/model_registry.go),
// static definitions only — Termory has no dynamic model registry.
fn lookup_thinking_max(model: &str) -> i64 {
    let model = model.trim();
    STATIC_THINKING_MAX
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, max)| *max)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// translator/common + translator/gemini/common helpers
// ---------------------------------------------------------------------------

const CLAUDE_SYSTEM_REMINDER_START: &str = "<system-reminder>";
const CLAUDE_SYSTEM_REMINDER_END: &str = "</system-reminder>";

// port of SystemReminderText (translator/common/claude_system.go)
fn system_reminder_text(text: &str) -> String {
    format!("{CLAUDE_SYSTEM_REMINDER_START}\n{text}\n{CLAUDE_SYSTEM_REMINDER_END}")
}

// port of claudeSystemTextParts (translator/common/claude_system.go)
fn claude_system_text_parts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => {
            if text.is_empty() || is_claude_code_attribution_system_text(text) {
                Vec::new()
            } else {
                vec![text.clone()]
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| gstr(gget(item, "type")) == "text")
            .map(|item| gstr(gget(item, "text")))
            .filter(|text| !text.is_empty() && !is_claude_code_attribution_system_text(text))
            .collect(),
        _ => Vec::new(),
    }
}

// port of ClaudeMessageSystemReminderText (translator/common/claude_system.go)
fn claude_message_system_reminder_text(content: Option<&Value>) -> Option<String> {
    let parts = claude_system_text_parts(content);
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(system_reminder_text(&text))
}

// port of AlignClaudeToolResults (translator/common/claude_messages.go)
fn align_claude_tool_results(parts: &[Value], tool_use_ids: &[String]) -> Vec<Value> {
    if tool_use_ids.is_empty() {
        return parts.to_vec();
    }
    let mut tool_results = Vec::new();
    let mut tool_result_indices = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if gstr(gget(part, "type")) == "tool_result" {
            tool_results.push(part);
            tool_result_indices.push(i);
        }
    }
    if tool_results.len() != tool_use_ids.len() {
        return parts.to_vec();
    }
    let mut used = vec![false; tool_results.len()];
    let mut reordered = Vec::with_capacity(tool_use_ids.len());
    for id in tool_use_ids {
        let matched = tool_results
            .iter()
            .enumerate()
            .position(|(ri, r)| !used[ri] && !id.is_empty() && gstr(gget(r, "tool_use_id")) == *id);
        let Some(m) = matched else {
            return parts.to_vec();
        };
        used[m] = true;
        reordered.push((*tool_results[m]).clone());
    }
    let mut ordered = parts.to_vec();
    for (i, slot) in tool_result_indices.into_iter().enumerate() {
        ordered[slot] = reordered[i].clone();
    }
    ordered
}

fn is_function_response_part(p: &Value) -> bool {
    p.get("functionResponse").is_some() || p.get("function_response").is_some()
}

// port of ReorderGeminiUserParts (translator/common/gemini.go)
fn reorder_gemini_user_parts(parts: Vec<Value>) -> Vec<Value> {
    let mut has_fr = false;
    let mut has_trailing_text = false;
    for p in &parts {
        if is_function_response_part(p) {
            has_fr = true;
        } else if has_fr && p.get("text").is_some() {
            has_trailing_text = true;
            break;
        }
    }
    if !has_fr || !has_trailing_text {
        return parts;
    }
    let (mut prompt, tool): (Vec<Value>, Vec<Value>) =
        parts.into_iter().partition(|p| p.get("text").is_some());
    prompt.extend(tool);
    prompt
}

// port of MergeAdjacentGeminiContents (translator/common/gemini.go)
fn merge_adjacent_gemini_contents(contents: Vec<Value>) -> Vec<Value> {
    if contents.len() <= 1 {
        return contents;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    for content in contents {
        let role = gstr(content.get("role"));
        let parts = match content.get("parts") {
            Some(Value::Array(p)) if !p.is_empty() => p.clone(),
            _ => continue,
        };
        if let Some(last) = merged.last_mut() {
            if gstr(last.get("role")) == "user" && role == "user" {
                let mut combined = match last.get("parts") {
                    Some(Value::Array(lp)) => lp.clone(),
                    _ => Vec::new(),
                };
                combined.extend(parts);
                let combined = reorder_gemini_user_parts(combined);
                if let Some(obj) = last.as_object_mut() {
                    obj.insert("parts".into(), Value::Array(combined));
                }
                continue;
            }
        }
        merged.push(content);
    }
    merged
}

// port of ContainsJSONRef (translator/common/gemini.go)
fn contains_json_ref(value: &Value) -> bool {
    match value {
        Value::Object(m) => m
            .iter()
            .any(|(k, v)| (k == "$ref" && v.is_string()) || contains_json_ref(v)),
        Value::Array(a) => a.iter().any(contains_json_ref),
        _ => false,
    }
}

// port of SetGeminiFunctionResponseRaw + SetGeminiFunctionResponseResult
// (translator/common/gemini.go), for the `functionResponse.response.result`
// path this pair uses: a value carrying a string `$ref` goes out as its JSON
// text, so Gemini does not read it as a media-part reference.
fn gemini_function_response_raw(raw: Value) -> Value {
    if contains_json_ref(&raw) {
        Value::String(raw.to_string())
    } else {
        raw
    }
}

// port of DefaultSafetySettings (translator/gemini/common/safety.go)
fn default_safety_settings() -> Value {
    json!([
        {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "BLOCK_NONE"}
    ])
}

// port of AttachDefaultSafetySettings (translator/gemini/common/safety.go)
fn attach_default_safety_settings(out: &mut Value, path: &str) {
    if get_path(out, &path_of(&[path])).is_some() {
        return;
    }
    set_path(out, &path_of(&[path]), default_safety_settings());
}

// ---------------------------------------------------------------------------
// util/gemini_schema.go — CleanJSONSchemaForGeminiJSONSchema
// ---------------------------------------------------------------------------
//
// Only the option set CleanJSONSchemaForGeminiJSONSchema passes is ported:
// addMissingArrayItems, removeGeminiMetadata, flattenUnions,
// forceEnumStringType, preserveAllAdditionalProperties,
// preserveStandardConstraints. The Antigravity-only passes (inlineLocalRefs,
// moveNotToDescription, dropIgnoredEnumsToHints, addEmptySchemaPlaceholder)
// and the ones those options switch off (addAdditionalPropertiesHints,
// moveConstraintsToDescription) never run for this cleaner.

const PLACEHOLDER_REASON_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";

// port of CleanJSONSchemaForGeminiJSONSchema + cleanJSONSchema (util/gemini_schema.go)
fn clean_json_schema_for_gemini_json_schema(schema: &Value) -> Value {
    let mut s = normalize_malformed_schema_objects(schema.clone());
    convert_refs_to_hints(&mut s);
    convert_const_to_enum(&mut s);
    convert_enum_values_to_strings(&mut s);
    add_enum_hints(&mut s);
    merge_conditionals(&mut s);
    merge_all_of(&mut s);
    flatten_any_of_one_of(&mut s);
    flatten_type_arrays(&mut s);
    remove_unsupported_keywords(&mut s);
    remove_keywords(&mut s, &["nullable", "title"]);
    remove_placeholder_fields(&mut s);
    cleanup_required_fields(&mut s);
    sanitize_array_items(&mut s);
    s
}

// port of Walk (util/translator.go) / findPaths (util/gemini_schema.go)
fn walk(value: &Value, path: &[String], field: &str, out: &mut Vec<Path>) {
    match value {
        Value::Object(m) => {
            for (k, v) in m {
                let child = join(path, k);
                if k == field {
                    out.push(child.clone());
                }
                walk(v, &child, field, out);
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                walk(v, &join(path, &i.to_string()), field, out);
            }
        }
        _ => {}
    }
}

fn find_paths(root: &Value, field: &str) -> Vec<Path> {
    let mut out = Vec::new();
    walk(root, &[], field, &mut out);
    out
}

// port of findPathsByFields + walkForFields (util/gemini_schema.go)
fn find_paths_by_fields(root: &Value, fields: &[&str]) -> HashMap<String, Vec<Path>> {
    fn rec(value: &Value, path: &[String], fields: &[&str], out: &mut HashMap<String, Vec<Path>>) {
        match value {
            Value::Object(m) => {
                for (k, v) in m {
                    let child = join(path, k);
                    if fields.contains(&k.as_str()) {
                        out.entry(k.clone()).or_default().push(child.clone());
                    }
                    rec(v, &child, fields, out);
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    rec(v, &join(path, &i.to_string()), fields, out);
                }
            }
            _ => {}
        }
    }
    let mut out = HashMap::new();
    rec(root, &[], fields, &mut out);
    out
}

// port of sortByDepth (util/gemini_schema.go) — stable, deepest first.
fn sort_by_depth(paths: &mut [Path]) {
    paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
}

fn parent_of(path: &[String]) -> Path {
    path[..path.len().saturating_sub(1)].to_vec()
}

// port of isPropertyDefinition (util/gemini_schema.go)
fn is_property_definition(path: &[String]) -> bool {
    const NAME_MAP_KEYWORDS: [&str; 5] = [
        "properties",
        "patternProperties",
        "dependentSchemas",
        "$defs",
        "definitions",
    ];
    let trailing = path
        .iter()
        .rev()
        .take_while(|s| NAME_MAP_KEYWORDS.contains(&s.as_str()))
        .count();
    trailing % 2 == 1
}

// port of mergeHint (util/gemini_schema.go)
fn merge_hint(existing: &str, hint: &str) -> String {
    if existing.is_empty() {
        return hint.to_string();
    }
    if existing == hint
        || existing.starts_with(&format!("{hint} ("))
        || existing.contains(&format!("({hint})"))
    {
        return existing.to_string();
    }
    format!("{existing} ({hint})")
}

// port of appendHint (util/gemini_schema.go)
fn append_hint(root: &mut Value, parent: &[String], hint: &str) {
    let desc = join(parent, "description");
    let merged = merge_hint(&gstr(get_path(root, &desc)), hint);
    set_path(root, &desc, Value::String(merged));
}

// port of appendHintRaw (util/gemini_schema.go)
fn append_hint_raw(schema: &mut Value, hint: &str) {
    if let Some(obj) = schema.as_object_mut() {
        let merged = merge_hint(&gstr(obj.get("description")), hint);
        obj.insert("description".into(), Value::String(merged));
    }
}

// port of mergeDescriptionRaw (util/gemini_schema.go)
fn merge_description_raw(schema: &mut Value, parent_desc: &str) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let child = gstr(obj.get("description"));
    if child.is_empty() {
        obj.insert("description".into(), Value::String(parent_desc.into()));
    } else if child != parent_desc {
        obj.insert(
            "description".into(),
            Value::String(format!("{parent_desc} ({child})")),
        );
    }
}

// port of getStrings (util/gemini_schema.go)
fn get_strings(root: &Value, path: &[String]) -> Vec<String> {
    match get_path(root, path) {
        Some(Value::Array(a)) => a.iter().map(|r| gstr(Some(r))).collect(),
        _ => Vec::new(),
    }
}

/// sjson `Set` of a Go `[]string`: a nil slice marshals as `null`.
fn string_slice_value(items: Vec<String>) -> Value {
    if items.is_empty() {
        Value::Null
    } else {
        Value::Array(items.into_iter().map(Value::String).collect())
    }
}

// port of refName (util/gemini_schema.go)
fn ref_name(r: &str) -> String {
    match r.rfind('/') {
        Some(i) if i + 1 < r.len() => r[i + 1..].replace("~1", "/").replace("~0", "~"),
        _ => r.to_string(),
    }
}

// port of normalizeMalformedSchemaObjects (util/gemini_schema.go), with
// addMissingArrayItems = true. Go re-marshals a repaired schema from
// `map[string]any`, which sorts every object's keys; `sort_keys_deep`
// reproduces that, and an unrepaired schema keeps its original order.
fn normalize_malformed_schema_objects(root: Value) -> Value {
    if root == Value::Bool(true) {
        return json!({});
    }
    let Value::Object(map) = &root else {
        return root;
    };
    if is_api_request_document(map) {
        return root;
    }
    if map.len() == 1 {
        match map.get("schema") {
            Some(Value::Object(inner)) => {
                let (repaired, modified) = repair_schema_node(inner);
                if !modified {
                    return root;
                }
                return sort_keys_deep(json!({"schema": Value::Object(repaired)}));
            }
            Some(Value::Bool(true)) => return json!({"schema": {}}),
            _ => {}
        }
    }
    let (repaired, modified) = repair_schema_node(map);
    if !modified {
        return root;
    }
    sort_keys_deep(Value::Object(repaired))
}

fn sort_keys_deep(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sort_keys_deep(v)))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys_deep).collect()),
        other => other,
    }
}

// port of isKnownSchemaKeywordOrExtension (util/gemini_schema.go)
fn is_known_schema_keyword_or_extension(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

// port of isNonObjectDeclaredType (util/gemini_schema.go)
fn is_non_object_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("object"),
        Some(Value::Array(arr)) => {
            let has_object = arr
                .iter()
                .any(|i| i.as_str().is_some_and(|s| s.eq_ignore_ascii_case("object")));
            !has_object && !arr.is_empty()
        }
        _ => false,
    }
}

// port of isArrayDeclaredType (util/gemini_schema.go)
fn is_array_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => s.eq_ignore_ascii_case("array"),
        Some(Value::Array(arr)) => arr
            .iter()
            .any(|i| i.as_str().is_some_and(|s| s.eq_ignore_ascii_case("array"))),
        _ => false,
    }
}

// port of isAPIRequestDocument (util/gemini_schema.go)
fn is_api_request_document(m: &Map<String, Value>) -> bool {
    for key in [
        "tools",
        "contents",
        "messages",
        "functionDeclarations",
        "function_declarations",
    ] {
        if matches!(m.get(key), Some(Value::Array(_))) {
            return true;
        }
    }
    matches!(m.get("request"), Some(Value::Object(r)) if is_api_request_document(r))
}

// port of extractStringArray (util/gemini_schema.go)
fn extract_string_array(val: Option<&Value>) -> Vec<String> {
    match val {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

// port of mergeStringSlices (util/gemini_schema.go)
fn merge_string_slices(existing: Vec<String>, promoted: Vec<String>) -> Vec<String> {
    let mut res: Vec<String> = Vec::new();
    for s in existing.into_iter().chain(promoted) {
        if !s.is_empty() && !res.contains(&s) {
            res.push(s);
        }
    }
    res
}

// port of repairSchemaNode (util/gemini_schema.go), addMissingArrayItems = true
fn repair_schema_node(node: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut modified = false;
    let mut clone = node.clone();

    // 1. Bare property definition maps on a node not declared primitive/array.
    if !is_non_object_declared_type(clone.get("type")) {
        let mut bare = Map::new();
        for (k, v) in &clone {
            if v.is_object() && !is_known_schema_keyword_or_extension(k) {
                bare.insert(k.clone(), v.clone());
            }
        }
        if !bare.is_empty() {
            let (repaired_props, promoted, _) = repair_property_map(&bare);
            for k in bare.keys() {
                clone.shift_remove(k);
            }
            if let Some(Value::Object(existing)) = clone.get("properties") {
                let mut new_props = existing.clone();
                for (k, v) in repaired_props {
                    new_props.insert(k, v);
                }
                clone.insert("properties".into(), Value::Object(new_props));
            } else {
                clone.insert("properties".into(), Value::Object(repaired_props));
                if !clone.contains_key("type") {
                    clone.insert("type".into(), Value::String("object".into()));
                }
            }
            if !promoted.is_empty() {
                let merged =
                    merge_string_slices(extract_string_array(clone.get("required")), promoted);
                clone.insert("required".into(), string_slice_value(merged));
            }
            modified = true;
        }
    }

    // 2. Recursively repair the properties map.
    if let Some(Value::Object(props)) = clone.get("properties") {
        let (repaired_props, promoted, props_mod) = repair_property_map(props);
        if props_mod {
            clone.insert("properties".into(), Value::Object(repaired_props));
            modified = true;
        }
        if !promoted.is_empty() {
            let merged = merge_string_slices(extract_string_array(clone.get("required")), promoted);
            clone.insert("required".into(), string_slice_value(merged));
            modified = true;
        }
    }

    // Tool array schemas need an items definition; items implies an array.
    if is_array_declared_type(clone.get("type")) {
        if !clone.contains_key("items") {
            clone.insert("items".into(), json!({"type": "string"}));
            modified = true;
        }
    } else if clone.contains_key("items") {
        // Go: `clone["type"] == nil || clone["type"] == ""`.
        let untyped = match clone.get("type") {
            None | Some(Value::Null) => true,
            Some(Value::String(t)) => t.is_empty(),
            _ => false,
        };
        if untyped {
            clone.insert("type".into(), Value::String("array".into()));
            modified = true;
        }
    }

    // 3. Recurse into the other schema containers.
    match clone.get("items") {
        Some(Value::Object(items)) => {
            let (repaired, m) = repair_schema_node(items);
            if m {
                clone.insert("items".into(), Value::Object(repaired));
                modified = true;
            }
        }
        Some(Value::Array(list)) => {
            let (repaired, m) = repair_schema_list(list);
            if m {
                clone.insert("items".into(), Value::Array(repaired));
                modified = true;
            }
        }
        Some(Value::Bool(true)) => {
            clone.insert("items".into(), json!({}));
            modified = true;
        }
        _ => {}
    }

    if let Some(Value::Object(add)) = clone.get("additionalProperties") {
        let (repaired, m) = repair_schema_node(add);
        if m {
            clone.insert("additionalProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    if let Some(Value::Object(pat)) = clone.get("patternProperties") {
        let (repaired, _, m) = repair_property_map(pat);
        if m {
            clone.insert("patternProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    for key in [
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "additionalItems",
    ] {
        match clone.get(key) {
            Some(Value::Object(sub)) => {
                let (repaired, m) = repair_schema_node(sub);
                if m {
                    clone.insert(key.into(), Value::Object(repaired));
                    modified = true;
                }
            }
            Some(Value::Bool(true)) => {
                clone.insert(key.into(), json!({}));
                modified = true;
            }
            _ => {}
        }
    }

    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list)) = clone.get(key) {
            let (repaired, m) = repair_schema_list(list);
            if m {
                clone.insert(key.into(), Value::Array(repaired));
                modified = true;
            }
        }
    }

    for key in ["$defs", "definitions", "dependentSchemas", "dependencies"] {
        if let Some(Value::Object(defs)) = clone.get(key) {
            let mut repaired_defs = Map::new();
            let mut defs_modified = false;
            for (dk, dv) in defs {
                match dv {
                    Value::Object(def) => {
                        let (repaired, m) = repair_schema_node(def);
                        repaired_defs.insert(dk.clone(), Value::Object(repaired));
                        if m {
                            defs_modified = true;
                        }
                    }
                    Value::Bool(true) => {
                        repaired_defs.insert(dk.clone(), json!({}));
                        defs_modified = true;
                    }
                    other => {
                        repaired_defs.insert(dk.clone(), other.clone());
                    }
                }
            }
            if defs_modified {
                clone.insert(key.into(), Value::Object(repaired_defs));
                modified = true;
            }
        }
    }

    (clone, modified)
}

// port of repairSchemaList (util/gemini_schema.go)
fn repair_schema_list(list: &[Value]) -> (Vec<Value>, bool) {
    let mut out = Vec::with_capacity(list.len());
    let mut modified = false;
    for item in list {
        match item {
            Value::Object(m) => {
                let (repaired, im) = repair_schema_node(m);
                out.push(Value::Object(repaired));
                modified |= im;
            }
            Value::Bool(true) => {
                out.push(json!({}));
                modified = true;
            }
            other => out.push(other.clone()),
        }
    }
    (out, modified)
}

// port of repairPropertyMap (util/gemini_schema.go)
fn repair_property_map(props: &Map<String, Value>) -> (Map<String, Value>, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted = Vec::new();
    let mut modified = false;
    for (k, v) in props {
        match v {
            Value::Bool(true) => {
                out.insert(k.clone(), json!({}));
                modified = true;
            }
            Value::Object(child) => {
                let mut child_clone = child.clone();
                if let Some(Value::Bool(req)) = child_clone.get("required").cloned() {
                    child_clone.shift_remove("required");
                    modified = true;
                    if req {
                        promoted.push(k.clone());
                    }
                }
                let (repaired, cm) = repair_schema_node(&child_clone);
                modified |= cm;
                out.insert(k.clone(), Value::Object(repaired));
            }
            other => {
                out.insert(k.clone(), other.clone());
            }
        }
    }
    promoted.sort();
    (out, promoted, modified)
}

// port of convertRefsToHints (util/gemini_schema.go), preserveSiblings = false
fn convert_refs_to_hints(s: &mut Value) {
    let mut paths = find_paths(s, "$ref");
    sort_by_depth(&mut paths);
    for p in paths {
        let def_name = ref_name(&gstr(get_path(s, &p)));
        let parent = parent_of(&p);
        let mut hint = format!("See: {def_name}");
        let existing = gstr(get_path(s, &join(&parent, "description")));
        if !existing.is_empty() {
            hint = format!("{existing} ({hint})");
        }
        set_path(s, &parent, json!({"type": "object", "description": hint}));
    }
}

// port of convertConstToEnum (util/gemini_schema.go)
fn convert_const_to_enum(s: &mut Value) {
    for p in find_paths(s, "const") {
        let Some(val) = get_path(s, &p).cloned() else {
            continue;
        };
        let enum_path = join(&parent_of(&p), "enum");
        if get_path(s, &enum_path).is_none() {
            set_path(s, &enum_path, Value::Array(vec![val]));
        }
    }
}

// port of convertEnumValuesToStrings (util/gemini_schema.go), forceStringType = true
fn convert_enum_values_to_strings(s: &mut Value) {
    for p in find_paths(s, "enum") {
        let Some(Value::Array(arr)) = get_path(s, &p) else {
            continue;
        };
        let strings: Vec<String> = arr.iter().map(|i| gstr(Some(i))).collect();
        set_path(s, &p, string_slice_value(strings));
        set_path(
            s,
            &join(&parent_of(&p), "type"),
            Value::String("string".into()),
        );
    }
}

// port of addEnumHints (util/gemini_schema.go)
fn add_enum_hints(s: &mut Value) {
    for p in find_paths(s, "enum") {
        let Some(Value::Array(items)) = get_path(s, &p) else {
            continue;
        };
        if items.len() <= 1 || items.len() > 10 {
            continue;
        }
        let vals: Vec<String> = items.iter().map(|i| gstr(Some(i))).collect();
        append_hint(s, &parent_of(&p), &format!("Allowed: {}", vals.join(", ")));
    }
}

// port of mergeConditionals (util/gemini_schema.go)
fn merge_conditionals(s: &mut Value) {
    let by_field = find_paths_by_fields(s, &["then", "else"]);
    let mut paths: Vec<Path> = Vec::new();
    for key in ["then", "else"] {
        for p in by_field.get(key).into_iter().flatten() {
            if is_property_definition(&parent_of(p)) {
                continue;
            }
            paths.push(p.clone());
        }
    }
    sort_by_depth(&mut paths);
    for p in paths {
        let Some(Value::Object(props)) = get_path(s, &join(&p, "properties")).cloned() else {
            continue;
        };
        let parent = parent_of(&p);
        for (k, v) in props {
            let dest = join(&join(&parent, "properties"), &k);
            if get_path(s, &dest).is_none() {
                set_path(s, &dest, v);
            }
        }
    }
}

// port of mergeAllOf (util/gemini_schema.go)
fn merge_all_of(s: &mut Value) {
    let mut paths = find_paths(s, "allOf");
    sort_by_depth(&mut paths);
    for p in paths {
        let Some(Value::Array(all_of)) = get_path(s, &p).cloned() else {
            continue;
        };
        let parent = parent_of(&p);
        for item in &all_of {
            let Value::Object(item) = item else {
                continue;
            };
            for (field, value) in item {
                match field.as_str() {
                    "required" => {
                        let Value::Array(reqs) = value else {
                            continue;
                        };
                        let req_path = join(&parent, "required");
                        let mut current = get_strings(s, &req_path);
                        for r in reqs {
                            let name = gstr(Some(r));
                            if !current.contains(&name) {
                                current.push(name);
                            }
                        }
                        set_path(s, &req_path, string_slice_value(current));
                    }
                    "if" | "then" | "else" | "allOf" => {}
                    _ => merge_missing_schema_at_path(s, &join(&parent, field), value),
                }
            }
        }
        delete_path(s, &p);
    }
}

// port of mergeMissingSchemaAtPath (util/gemini_schema.go)
fn merge_missing_schema_at_path(s: &mut Value, dest: &[String], incoming: &Value) {
    match get_path(s, dest) {
        None => set_path(s, dest, incoming.clone()),
        Some(existing) => {
            if !existing.is_object() {
                return;
            }
            let Value::Object(inc) = incoming else {
                return;
            };
            for (k, v) in inc {
                merge_missing_schema_at_path(s, &join(dest, k), v);
            }
        }
    }
}

// port of flattenAnyOfOneOf (util/gemini_schema.go)
fn flatten_any_of_one_of(s: &mut Value) {
    for key in ["anyOf", "oneOf"] {
        let mut paths = find_paths(s, key);
        sort_by_depth(&mut paths);
        for p in paths {
            let items = match get_path(s, &p) {
                Some(Value::Array(a)) if !a.is_empty() => a.clone(),
                _ => continue,
            };
            let parent_path = parent_of(&p);
            let parent_props = get_path(s, &parent_path)
                .and_then(|parent| parent.get("properties"))
                .cloned();
            let item_type = |item: &Value| gstr(item.get("type"));

            if let Some(Value::Object(_)) = parent_props {
                let mut has_null = false;
                for item in &items {
                    if item_type(item) == "null" {
                        has_null = true;
                    }
                    if let Some(Value::Object(branch_props)) = item.get("properties") {
                        for (pk, pv) in branch_props {
                            let dest = join(&join(&parent_path, "properties"), pk);
                            merge_missing_schema_at_path(s, &dest, pv);
                        }
                    }
                }
                if has_null {
                    set_path(s, &join(&parent_path, "nullable"), Value::Bool(true));
                }
                delete_path(s, &p);
                continue;
            }

            let parent_desc = gstr(get_path(s, &join(&parent_path, "description")));
            let (best_idx, all_types) = select_best(&items);
            let mut selected = items[best_idx].clone();
            let has_null = items.iter().any(|i| item_type(i) == "null");
            if has_null && item_type(&items[best_idx]) != "null" {
                if let Some(obj) = selected.as_object_mut() {
                    obj.insert("nullable".into(), Value::Bool(true));
                }
            }
            if !parent_desc.is_empty() {
                merge_description_raw(&mut selected, &parent_desc);
            }
            if all_types.len() > 1 {
                append_hint_raw(
                    &mut selected,
                    &format!("Accepts: {}", all_types.join(" | ")),
                );
            }
            set_path(s, &parent_path, selected);
        }
    }
}

// port of selectBest (util/gemini_schema.go)
fn select_best(items: &[Value]) -> (usize, Vec<String>) {
    let mut best_score = -1;
    let mut best_idx = 0;
    let mut types = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut t = gstr(item.get("type"));
        let score;
        if t == "object" || item.get("properties").is_some() {
            score = 3;
            if t.is_empty() {
                t = "object".into();
            }
        } else if t == "array" || item.get("items").is_some() {
            score = 2;
            if t.is_empty() {
                t = "array".into();
            }
        } else if !t.is_empty() && t != "null" {
            score = 1;
        } else if t == "null" {
            score = 0;
        } else {
            score = 0;
            t = String::new();
        }
        if !t.is_empty() {
            types.push(t);
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    (best_idx, types)
}

// port of flattenTypeArrays (util/gemini_schema.go), preserveNativeNullable = false
fn flatten_type_arrays(s: &mut Value) {
    let mut paths = find_paths(s, "type");
    sort_by_depth(&mut paths);
    let mut nullable_fields: Vec<(Path, Vec<String>)> = Vec::new();

    for p in paths {
        let arr = match get_path(s, &p) {
            Some(Value::Array(a)) if !a.is_empty() => a.clone(),
            _ => continue,
        };
        let mut has_null = false;
        let mut non_null: Vec<String> = Vec::new();
        for item in &arr {
            let t = gstr(Some(item));
            if t == "null" {
                has_null = true;
            } else if !t.is_empty() {
                non_null.push(t);
            }
        }
        let parent = parent_of(&p);
        let items_path = join(&parent, "items");
        let first_type = if non_null.is_empty() {
            "string".to_string()
        } else if get_path(s, &items_path).is_some() && non_null.iter().any(|t| t == "array") {
            "array".to_string()
        } else {
            non_null[0].clone()
        };
        set_path(s, &p, Value::String(first_type.clone()));
        if first_type != "array" && get_path(s, &items_path).is_some() {
            delete_path(s, &items_path);
        }
        if non_null.len() > 1 {
            append_hint(s, &parent, &format!("Accepts: {}", non_null.join(" | ")));
        }
        if has_null && p.len() >= 3 && p[p.len() - 3] == "properties" {
            let field = p[p.len() - 2].clone();
            let object_path = p[..p.len() - 3].to_vec();
            append_hint(
                s,
                &join(&join(&object_path, "properties"), &field),
                "(nullable)",
            );
            match nullable_fields
                .iter_mut()
                .find(|(op, _)| *op == object_path)
            {
                Some((_, fields)) => fields.push(field),
                None => nullable_fields.push((object_path, vec![field])),
            }
        }
    }

    for (object_path, fields) in nullable_fields {
        let req_path = join(&object_path, "required");
        let Some(Value::Array(req)) = get_path(s, &req_path) else {
            continue;
        };
        let filtered: Vec<String> = req
            .iter()
            .map(|r| gstr(Some(r)))
            .filter(|r| !fields.contains(r))
            .collect();
        if filtered.is_empty() {
            delete_path(s, &req_path);
        } else {
            set_path(s, &req_path, string_slice_value(filtered));
        }
    }
}

// port of removeUnsupportedKeywords (util/gemini_schema.go), with
// preserveStandardConstraints (no constraint keywords) and
// preserveAllAdditionalProperties.
fn remove_unsupported_keywords(s: &mut Value) {
    const KEYWORDS: [&str; 27] = [
        "$schema",
        "$defs",
        "definitions",
        "const",
        "$ref",
        "$id",
        "id",
        "additionalProperties",
        "$anchor",
        "$vocabulary",
        "$dynamicRef",
        "$dynamicAnchor",
        "propertyNames",
        "patternProperties",
        "if",
        "then",
        "else",
        "$comment",
        "enumDescriptions",
        "enumTitles",
        "prefill",
        "deprecated",
        "encrypted",
        "additionalItems",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
    ];
    let by_field = find_paths_by_fields(s, &KEYWORDS);
    let mut delete_paths: Vec<Path> = Vec::new();
    for key in KEYWORDS {
        for p in by_field.get(key).into_iter().flatten() {
            if is_property_definition(&parent_of(p)) {
                continue;
            }
            if key == "additionalProperties" {
                continue; // preserveAllAdditionalProperties
            }
            delete_paths.push(p.clone());
        }
    }
    sort_by_depth(&mut delete_paths);
    for p in delete_paths {
        delete_path(s, &p);
    }
    remove_extension_fields(s);
}

// port of removeExtensionFields + walkForExtensions (util/gemini_schema.go)
fn remove_extension_fields(s: &mut Value) {
    fn rec(value: &Value, path: &[String], out: &mut Vec<Path>) {
        match value {
            Value::Array(a) => {
                for i in (0..a.len()).rev() {
                    rec(&a[i], &join(path, &i.to_string()), out);
                }
            }
            Value::Object(m) => {
                for (k, v) in m {
                    let child = join(path, k);
                    if k.starts_with("x-") && !is_property_definition(path) {
                        out.push(child);
                        continue;
                    }
                    rec(v, &child, out);
                }
            }
            _ => {}
        }
    }
    let mut paths = Vec::new();
    rec(s, &[], &mut paths);
    for p in paths {
        delete_path(s, &p);
    }
}

// port of removeKeywords (util/gemini_schema.go)
fn remove_keywords(s: &mut Value, keywords: &[&str]) {
    let by_field = find_paths_by_fields(s, keywords);
    let mut delete_paths: Vec<Path> = Vec::new();
    for key in keywords {
        for p in by_field.get(*key).into_iter().flatten() {
            if is_property_definition(&parent_of(p)) {
                continue;
            }
            delete_paths.push(p.clone());
        }
    }
    sort_by_depth(&mut delete_paths);
    for p in delete_paths {
        delete_path(s, &p);
    }
}

/// gjson `strings.HasSuffix(p, ".properties.<name>")`: the property sits
/// under a `properties` map that is itself nested (a root-level
/// `properties.<name>` has no leading dot and does not match).
fn ends_with_nested_property(p: &[String], name: &str) -> bool {
    p.len() >= 3 && p[p.len() - 1] == name && p[p.len() - 2] == "properties"
}

fn drop_required_entry(s: &mut Value, parent: &[String], name: &str) {
    let req_path = join(parent, "required");
    let Some(Value::Array(req)) = get_path(s, &req_path) else {
        return;
    };
    let filtered: Vec<String> = req
        .iter()
        .map(|r| gstr(Some(r)))
        .filter(|r| r != name)
        .collect();
    if filtered.is_empty() {
        delete_path(s, &req_path);
    } else {
        set_path(s, &req_path, string_slice_value(filtered));
    }
}

// port of removePlaceholderFields (util/gemini_schema.go)
fn remove_placeholder_fields(s: &mut Value) {
    let mut paths = find_paths(s, "_");
    sort_by_depth(&mut paths);
    for p in paths {
        if !ends_with_nested_property(&p, "_") {
            continue;
        }
        delete_path(s, &p);
        let parent = p[..p.len() - 2].to_vec();
        drop_required_entry(s, &parent, "_");
    }

    let mut reason_paths = find_paths(s, "reason");
    sort_by_depth(&mut reason_paths);
    for p in reason_paths {
        if !ends_with_nested_property(&p, "reason") {
            continue;
        }
        let parent = p[..p.len() - 2].to_vec();
        match get_path(s, &join(&parent, "properties")) {
            Some(Value::Object(props)) if props.len() == 1 => {}
            _ => continue,
        }
        if gstr(get_path(s, &join(&p, "description"))) != PLACEHOLDER_REASON_DESCRIPTION {
            continue;
        }
        delete_path(s, &p);
        drop_required_entry(s, &parent, "reason");
    }
}

// port of cleanupRequiredFields (util/gemini_schema.go)
fn cleanup_required_fields(s: &mut Value) {
    for p in find_paths(s, "required") {
        let parent = parent_of(&p);
        let Some(Value::Array(req)) = get_path(s, &p).cloned() else {
            continue;
        };
        let Some(Value::Object(props)) = get_path(s, &join(&parent, "properties")).cloned() else {
            delete_path(s, &p);
            continue;
        };
        let valid: Vec<String> = req
            .iter()
            .map(|r| gstr(Some(r)))
            .filter(|k| !k.is_empty() && props.contains_key(k))
            .collect();
        if valid.len() != req.len() {
            if valid.is_empty() {
                delete_path(s, &p);
            } else {
                set_path(s, &p, string_slice_value(valid));
            }
        }
    }
}

// port of sanitizeArrayItems (util/gemini_schema.go)
fn sanitize_array_items(s: &mut Value) {
    let mut paths = find_paths(s, "items");
    sort_by_depth(&mut paths);
    for p in paths {
        let parent = parent_of(&p);
        if is_property_definition(&parent) {
            continue;
        }
        let type_path = join(&parent, "type");
        let t = gstr(get_path(s, &type_path));
        if t.is_empty() {
            set_path(s, &type_path, Value::String("array".into()));
        } else if !t.eq_ignore_ascii_case("array") {
            delete_path(s, &p);
        }
    }
}

// ---------------------------------------------------------------------------
// Request: Anthropic Messages → Gemini generateContent
// ---------------------------------------------------------------------------

const GEMINI_CLAUDE_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Client request (Anthropic Messages body) → upstream request (Gemini
/// generateContent body). `model` = upstream model id, written into the body
/// as Go does. `stream` is unused by the Go code as well.
// port of ConvertClaudeRequestToGemini (gemini_claude_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    convert_claude_request_to_gemini(model, body, stream)
}

// port of convertClaudeRequestToGemini (gemini_claude_request.go), with
// preserveEmptyThinkingBlocks = false (the registered, non-compat form).
fn convert_claude_request_to_gemini(model_name: &str, raw: &Value, _stream: bool) -> Value {
    let mut out = json!({"contents": []});
    set_path(
        &mut out,
        &path_of(&["model"]),
        Value::String(model_name.into()),
    );

    // system instruction
    match raw.get("system") {
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                if gstr(gget(item, "type")) != "text" {
                    continue;
                }
                if let Some(Value::String(text)) = item.get("text") {
                    if is_claude_code_attribution_system_text(text) {
                        continue;
                    }
                    parts.push(json!({"text": text}));
                }
            }
            if !parts.is_empty() {
                set_path(
                    &mut out,
                    &path_of(&["systemInstruction"]),
                    json!({"role": "user", "parts": parts}),
                );
            }
        }
        Some(Value::String(text)) if !is_claude_code_attribution_system_text(text) => {
            set_path(
                &mut out,
                &path_of(&["systemInstruction"]),
                json!({"parts": [{"text": text}]}),
            );
        }
        _ => {}
    }

    // contents
    if let Some(Value::Array(messages)) = raw.get("messages") {
        let mut content_items: Vec<Value> = Vec::with_capacity(messages.len());
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();
        let mut pending_tool_use_ids: Vec<String> = Vec::new();
        for message in messages {
            let Some(Value::String(original_role)) = message.get("role") else {
                continue;
            };
            let original_role = original_role.as_str();
            let is_system = original_role == "system" || original_role == "developer";
            let mut preceding_tool_use_ids = Vec::new();
            if !is_system {
                preceding_tool_use_ids = std::mem::take(&mut pending_tool_use_ids);
            }
            let role = match original_role {
                "assistant" => "model",
                "system" | "developer" => "user",
                other => other,
            };

            let mut part_items: Vec<Value> = Vec::with_capacity(4);
            let contents = message.get("content");
            if is_system {
                if let Some(reminder) = claude_message_system_reminder_text(contents) {
                    part_items.push(json!({"text": reminder}));
                    content_items.push(gemini_content_with_parts(role, part_items));
                }
                continue;
            }
            match contents {
                Some(Value::Array(arr)) => {
                    let blocks = if original_role == "user" {
                        align_claude_tool_results(arr, &preceding_tool_use_ids)
                    } else {
                        arr.clone()
                    };
                    for block in &blocks {
                        match gstr(gget(block, "type")).as_str() {
                            "text" => {
                                let text = gstr(gget(block, "text"));
                                if text.is_empty() {
                                    continue;
                                }
                                part_items.push(json!({"text": text}));
                            }
                            // "thinking" is kept only by the compat form.
                            "tool_use" => {
                                let function_name = gstr(gget(block, "name"));
                                let tool_use_id = gstr(gget(block, "id"));
                                if !tool_use_id.is_empty() && !function_name.is_empty() {
                                    tool_name_by_id
                                        .insert(tool_use_id.clone(), function_name.clone());
                                }
                                let function_name = sanitize_function_name(&function_name);
                                // `input.String()` then `Parse(...).IsObject()`:
                                // an object, or a string holding a JSON object.
                                let args = match block.get("input") {
                                    Some(obj @ Value::Object(_)) => Some(obj.clone()),
                                    Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                                        .ok()
                                        .filter(Value::is_object),
                                    _ => None,
                                };
                                if let Some(args) = args {
                                    let mut call = Map::new();
                                    call.insert("name".into(), Value::String(function_name));
                                    call.insert("args".into(), args);
                                    if !tool_use_id.is_empty() {
                                        call.insert(
                                            "id".into(),
                                            Value::String(tool_use_id.clone()),
                                        );
                                    }
                                    part_items.push(json!({
                                        "thoughtSignature": GEMINI_CLAUDE_THOUGHT_SIGNATURE,
                                        "functionCall": Value::Object(call),
                                    }));
                                    if original_role == "assistant" {
                                        pending_tool_use_ids.push(tool_use_id);
                                    }
                                }
                            }
                            "tool_result" => {
                                let tool_call_id = gstr(gget(block, "tool_use_id"));
                                if tool_call_id.is_empty() {
                                    continue;
                                }
                                let mut func_name = tool_name_by_id
                                    .get(&tool_call_id)
                                    .cloned()
                                    .unwrap_or_default();
                                if func_name.is_empty() {
                                    func_name = tool_name_from_claude_tool_use_id(&tool_call_id);
                                }
                                if func_name.is_empty() {
                                    func_name = tool_call_id.clone();
                                }
                                let func_name = sanitize_function_name(&func_name);
                                let tool_result =
                                    convert_claude_tool_result_content(block.get("content"));
                                let result = match tool_result.result_raw {
                                    Some(raw) => gemini_function_response_raw(raw),
                                    None => Value::String(tool_result.result),
                                };
                                part_items.push(json!({
                                    "functionResponse": {
                                        "name": func_name,
                                        "response": {"result": result},
                                        "id": tool_call_id,
                                    }
                                }));
                                for (mime, data) in tool_result.images {
                                    part_items.push(
                                        json!({"inline_data": {"mime_type": mime, "data": data}}),
                                    );
                                }
                            }
                            "image" => {
                                let source = block.get("source");
                                if gstr(source.and_then(|s| s.get("type"))) != "base64" {
                                    continue;
                                }
                                let mime = gstr(source.and_then(|s| s.get("media_type")));
                                let data = gstr(source.and_then(|s| s.get("data")));
                                if mime.is_empty() || data.is_empty() {
                                    continue;
                                }
                                part_items.push(
                                    json!({"inline_data": {"mime_type": mime, "data": data}}),
                                );
                            }
                            _ => {}
                        }
                    }
                    if role == "user" {
                        part_items = reorder_gemini_user_parts(part_items);
                    }
                    content_items.push(gemini_content_with_parts(role, part_items));
                }
                Some(Value::String(text)) => {
                    part_items.push(json!({"text": text}));
                    content_items.push(gemini_content_with_parts(role, part_items));
                }
                _ => {}
            }
        }

        // Strip a trailing model turn with unanswered function calls.
        if let Some(last) = content_items.last() {
            if gstr(last.get("role")) == "model" {
                let has_function_call = matches!(last.get("parts"), Some(Value::Array(parts))
                    if parts.iter().any(|p| p.get("functionCall").is_some()));
                if has_function_call {
                    content_items.pop();
                }
            }
        }
        set_path(
            &mut out,
            &path_of(&["contents"]),
            Value::Array(merge_adjacent_gemini_contents(content_items)),
        );
    }

    // tools
    let mut tool_items: Vec<Value> = Vec::new();
    let mut has_strict_tool = false;
    if let Some(Value::Array(tools)) = raw.get("tools") {
        for tool in tools {
            if tool.get("strict") == Some(&Value::Bool(true)) {
                has_strict_tool = true;
            }
            let Some(schema @ Value::Object(_)) = tool.get("input_schema") else {
                continue;
            };
            let input_schema = clean_json_schema_for_gemini_json_schema(schema);
            let Value::Object(mut decl) = tool.clone() else {
                continue;
            };
            decl.shift_remove("input_schema");
            decl.insert("parametersJsonSchema".into(), input_schema);
            for key in [
                "strict",
                "input_examples",
                "type",
                "cache_control",
                "defer_loading",
                "eager_input_streaming",
            ] {
                decl.shift_remove(key);
            }
            let name = decl.get("name");
            let original_name = gstr(name);
            let sanitized = sanitize_function_name(&original_name);
            if !matches!(name, Some(Value::String(_))) || sanitized != original_name {
                decl.insert("name".into(), Value::String(sanitized));
            }
            tool_items.push(Value::Object(decl));
        }
        if !tool_items.is_empty() {
            set_path(
                &mut out,
                &path_of(&["tools"]),
                json!([{"functionDeclarations": tool_items.clone()}]),
            );
        }
    }

    // tool_choice
    let mode_path = path_of(&["toolConfig", "functionCallingConfig", "mode"]);
    match raw.get("tool_choice") {
        Some(tc) if !tc.is_null() => {
            let (choice_type, choice_name) = match tc {
                Value::Object(_) => (gstr(tc.get("type")), gstr(tc.get("name"))),
                Value::String(s) => (s.clone(), String::new()),
                _ => (String::new(), String::new()),
            };
            match choice_type.as_str() {
                "auto" => {
                    let mode = if has_strict_tool { "VALIDATED" } else { "AUTO" };
                    set_path(&mut out, &mode_path, Value::String(mode.into()));
                }
                "none" => set_path(&mut out, &mode_path, Value::String("NONE".into())),
                "any" => set_path(&mut out, &mode_path, Value::String("ANY".into())),
                "tool" => {
                    set_path(&mut out, &mode_path, Value::String("ANY".into()));
                    if !choice_name.is_empty() {
                        set_path(
                            &mut out,
                            &path_of(&[
                                "toolConfig",
                                "functionCallingConfig",
                                "allowedFunctionNames",
                            ]),
                            json!([sanitize_function_name(&choice_name)]),
                        );
                    }
                }
                _ => {}
            }
        }
        _ => {
            if has_strict_tool && !tool_items.is_empty() {
                set_path(&mut out, &mode_path, Value::String("VALIDATED".into()));
            }
        }
    }

    // Anthropic thinking → Gemini thinking config.
    if let Some(t @ Value::Object(_)) = raw.get("thinking") {
        let budget_path = path_of(&["generationConfig", "thinkingConfig", "thinkingBudget"]);
        let level_path = path_of(&["generationConfig", "thinkingConfig", "thinkingLevel"]);
        match gstr(t.get("type")).as_str() {
            "enabled" => {
                if let Some(b @ Value::Number(_)) = t.get("budget_tokens") {
                    set_path(&mut out, &budget_path, Value::from(gint(Some(b))));
                }
            }
            "adaptive" | "auto" => {
                let effort = match gget(raw, "output_config.effort") {
                    Some(Value::String(e)) => e.trim().to_lowercase(),
                    _ => String::new(),
                };
                if !effort.is_empty() {
                    set_path(&mut out, &level_path, Value::String(effort));
                } else {
                    let max_budget = lookup_thinking_max(model_name);
                    if max_budget > 0 {
                        set_path(&mut out, &budget_path, Value::from(max_budget));
                    } else {
                        set_path(&mut out, &level_path, Value::String("high".into()));
                    }
                }
            }
            _ => {}
        }
        // The client asked for thinking it can show (Claude Code renders
        // thinking blocks): without `includeThoughts` Gemini spends the
        // budget and returns no thought parts, so the user sees only a pause.
        if out
            .pointer("/generationConfig/thinkingConfig")
            .is_some_and(|c| c.is_object())
        {
            set_path(
                &mut out,
                &path_of(&["generationConfig", "thinkingConfig", "includeThoughts"]),
                Value::Bool(true),
            );
        }
    }
    for (src, dst) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(Value::Number(n)) = raw.get(src) {
            set_path(
                &mut out,
                &path_of(&["generationConfig", dst]),
                float_value(n.as_f64().unwrap_or(0.0)),
            );
        }
    }

    attach_default_safety_settings(&mut out, "safetySettings");
    out
}

// port of geminiContentWithParts (gemini_claude_request.go)
fn gemini_content_with_parts(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

// port of toolNameFromClaudeToolUseID (gemini_claude_request.go)
fn tool_name_from_claude_tool_use_id(tool_use_id: &str) -> String {
    let parts: Vec<&str> = tool_use_id.split('-').collect();
    if parts.len() <= 1 {
        return String::new();
    }
    parts[..parts.len() - 1].join("-")
}

// ---------------------------------------------------------------------------
// Response: Gemini stream → Anthropic SSE
// ---------------------------------------------------------------------------

/// Process-wide counter for streamed tool_use ids (Go's `toolUseIDCounter`).
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-stream state. port of Params (gemini_claude_response.go); `IsGlAPIKey`
/// is never read by the pair and is omitted.
pub struct StreamTranslator {
    has_first_response: bool,
    /// 0 = none, 1 = text, 2 = thinking, 3 = function call.
    response_type: u8,
    response_index: i64,
    has_content: bool,
    tool_name_map: Option<HashMap<String, String>>,
    sanitized_name_map: Option<HashMap<String, String>>,
    saw_tool_call: bool,
    has_final_events: bool,
}

/// The thought signature of a part: `thoughtSignature`, else
/// `thought_signature` (gjson `Exists` — an explicit null counts as present).
fn thought_signature(part: &Value) -> Option<&Value> {
    part.get("thoughtSignature")
        .or_else(|| part.get("thought_signature"))
}

impl StreamTranslator {
    /// `original_request` = the client's ORIGINAL Anthropic body.
    pub fn new(original_request: &Value) -> Self {
        Self {
            has_first_response: false,
            response_type: 0,
            response_index: 0,
            has_content: false,
            tool_name_map: tool_name_map_from_claude_request(original_request),
            sanitized_name_map: sanitized_tool_name_map(original_request),
            saw_tool_call: false,
            has_final_events: false,
        }
    }

    fn signature_delta(&mut self, out: &mut Vec<String>, signature: &str) {
        if signature.is_empty() || self.response_type != 2 {
            return;
        }
        out.push(sse_frame(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": self.response_index,
                    "delta": {"type": "signature_delta", "signature": signature}}),
        ));
        self.has_content = true;
    }

    fn block_stop(&self, out: &mut Vec<String>) {
        out.push(sse_frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.response_index}),
        ));
    }

    fn delta(&self, out: &mut Vec<String>, delta: Value) {
        out.push(sse_frame(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": self.response_index, "delta": delta}),
        ));
    }

    /// One upstream SSE chunk (a `GenerateContentResponse`); the `event:`
    /// field is unused by Gemini.
    // port of ConvertGeminiResponseToClaude (gemini_claude_response.go)
    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        let mut out = Vec::new();

        if !self.has_first_response {
            let mut start = json!({"type": "message_start", "message": {
                "id": "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY", "type": "message",
                "role": "assistant", "content": [], "model": "claude-3-5-sonnet-20241022",
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}}});
            if let Some(v) = data.get("modelVersion") {
                start["message"]["model"] = Value::String(gstr(Some(v)));
            }
            if let Some(v) = data.get("responseId") {
                start["message"]["id"] = Value::String(gstr(Some(v)));
            }
            out.push(sse_frame("message_start", &start));
            self.has_first_response = true;
        }

        if let Some(Value::Array(parts)) = gget(data, "candidates.0.content.parts") {
            for part in parts {
                let text = part.get("text");
                let function_call = part.get("functionCall");
                let sig = gstr(thought_signature(part));
                let has_sig = !sig.is_empty();

                if has_sig && text.is_none() && function_call.is_none() {
                    self.signature_delta(&mut out, &sig);
                    continue;
                }

                if let Some(text) = text {
                    let text = gstr(Some(text));
                    if has_sig && text.is_empty() {
                        self.signature_delta(&mut out, &sig);
                        continue;
                    }
                    // Only `thought: true` is thinking. Gemini 3 attaches the
                    // thought signature to the LAST TEXT part of a text-only
                    // answer — that part is the visible answer, not reasoning.
                    if gbool(part.get("thought")) {
                        if self.response_type == 2 {
                            self.delta(
                                &mut out,
                                json!({"type": "thinking_delta", "thinking": text}),
                            );
                            self.has_content = true;
                        } else {
                            if self.response_type != 0 {
                                self.block_stop(&mut out);
                                self.response_index += 1;
                            }
                            out.push(sse_frame(
                                "content_block_start",
                                &json!({"type": "content_block_start", "index": self.response_index,
                                        "content_block": {"type": "thinking", "thinking": ""}}),
                            ));
                            self.delta(
                                &mut out,
                                json!({"type": "thinking_delta", "thinking": text}),
                            );
                            self.response_type = 2;
                            self.has_content = true;
                        }
                        self.signature_delta(&mut out, &sig);
                    } else if self.response_type == 1 {
                        self.delta(&mut out, json!({"type": "text_delta", "text": text}));
                        self.has_content = true;
                    } else {
                        if self.response_type != 0 {
                            self.block_stop(&mut out);
                            self.response_index += 1;
                        }
                        out.push(sse_frame(
                            "content_block_start",
                            &json!({"type": "content_block_start", "index": self.response_index,
                                    "content_block": {"type": "text", "text": ""}}),
                        ));
                        self.delta(&mut out, json!({"type": "text_delta", "text": text}));
                        self.response_type = 1;
                        self.has_content = true;
                    }
                } else if let Some(fc) = function_call {
                    self.saw_tool_call = true;
                    let upstream_name = restore_sanitized_tool_name(
                        self.sanitized_name_map.as_ref(),
                        &gstr(fc.get("name")),
                    );
                    let client_name = map_tool_name(self.tool_name_map.as_ref(), &upstream_name);

                    // A nameless continuation of the open call: more args.
                    if self.response_type == 3 && upstream_name.is_empty() {
                        if let Some(args) = fc.get("args") {
                            self.delta(
                                &mut out,
                                json!({"type": "input_json_delta", "partial_json": args.to_string()}),
                            );
                        }
                        continue;
                    }

                    if self.response_type == 3 {
                        self.block_stop(&mut out);
                        self.response_index += 1;
                        self.response_type = 0;
                    }
                    if self.response_type != 0 {
                        self.block_stop(&mut out);
                        self.response_index += 1;
                    }

                    let n = TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
                    let id = sanitize_claude_tool_id(&format!("{upstream_name}-{n}"));
                    out.push(sse_frame(
                        "content_block_start",
                        &json!({"type": "content_block_start", "index": self.response_index,
                                "content_block": {"type": "tool_use", "id": id,
                                                  "name": client_name, "input": {}}}),
                    ));
                    if let Some(args) = fc.get("args") {
                        self.delta(
                            &mut out,
                            json!({"type": "input_json_delta", "partial_json": args.to_string()}),
                        );
                    }
                    self.response_type = 3;
                    self.has_content = true;
                }
            }
        }

        // Go: `bytes.Contains(rawJSON, []byte(`"finishReason"`))` on the raw chunk.
        if let Some(usage) = data.get("usageMetadata") {
            if data.to_string().contains("\"finishReason\"")
                && !self.has_final_events
                && self.has_content
            {
                if self.response_type != 0 {
                    self.block_stop(&mut out);
                    self.response_type = 0;
                }
                let stop_reason = if self.saw_tool_call {
                    "tool_use"
                } else if gstr(gget(data, "candidates.0.finishReason")) == "MAX_TOKENS" {
                    "max_tokens"
                } else {
                    "end_turn"
                };
                let thoughts = gint(usage.get("thoughtsTokenCount"));
                let candidates = gint(usage.get("candidatesTokenCount"));
                let cached = gint(usage.get("cachedContentTokenCount"));
                let prompt = (gint(usage.get("promptTokenCount")) - cached).max(0);
                let mut usage_out =
                    json!({"input_tokens": prompt, "output_tokens": candidates + thoughts});
                if cached > 0 {
                    usage_out["cache_read_input_tokens"] = Value::from(cached);
                }
                out.push(sse_frame(
                    "message_delta",
                    &json!({"type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                            "usage": usage_out}),
                ));
                self.has_final_events = true;
            }
        }

        out
    }

    /// Upstream stream ended — the Go `[DONE]` branch: `message_stop`, but
    /// only once some content was emitted.
    // port of ConvertGeminiResponseToClaude, `[DONE]` input (gemini_claude_response.go)
    pub fn finish(&mut self) -> Vec<String> {
        if self.has_content {
            vec![sse_frame("message_stop", &json!({"type": "message_stop"}))]
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Response: Gemini non-stream → Anthropic message
// ---------------------------------------------------------------------------

/// Complete Gemini `GenerateContentResponse` → Anthropic Messages response.
// port of ConvertGeminiResponseToClaudeNonStream (gemini_claude_response.go)
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let root = upstream;
    let tool_name_map = tool_name_map_from_claude_request(original_request);
    let sanitized_name_map = sanitized_tool_name_map(original_request);

    let cached = gint(gget(root, "usageMetadata.cachedContentTokenCount"));
    let input_tokens = (gint(gget(root, "usageMetadata.promptTokenCount")) - cached).max(0);
    let output_tokens = gint(gget(root, "usageMetadata.candidatesTokenCount"))
        + gint(gget(root, "usageMetadata.thoughtsTokenCount"));
    let mut usage = json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
    if cached > 0 {
        usage["cache_read_input_tokens"] = Value::from(cached);
    }
    let mut out = json!({
        "id": gstr(root.get("responseId")), "type": "message", "role": "assistant",
        "model": gstr(root.get("modelVersion")), "content": [],
        "stop_reason": null, "stop_sequence": null, "usage": usage,
    });

    let mut text_buf = String::new();
    let mut thinking_buf = String::new();
    let mut thinking_signature = String::new();
    let mut tool_id_counter = 0;
    let mut has_tool_call = false;
    let mut blocks: Vec<Value> = Vec::new();

    let flush_text = |text_buf: &mut String, blocks: &mut Vec<Value>| {
        if text_buf.is_empty() {
            return;
        }
        blocks.push(json!({"type": "text", "text": std::mem::take(text_buf)}));
    };
    let flush_thinking = |thinking_buf: &mut String, sig: &mut String, blocks: &mut Vec<Value>| {
        if thinking_buf.is_empty() && sig.is_empty() {
            return;
        }
        let mut block = json!({"type": "thinking", "thinking": std::mem::take(thinking_buf)});
        if !sig.is_empty() {
            block["signature"] = Value::String(std::mem::take(sig));
        }
        blocks.push(block);
    };

    if let Some(Value::Array(parts)) = gget(root, "candidates.0.content.parts") {
        for part in parts {
            let sig = gstr(thought_signature(part));
            let has_sig = !sig.is_empty();
            let text = part.get("text").map(|t| gstr(Some(t)));
            let function_call = part.get("functionCall");
            let is_thought = gbool(part.get("thought"));
            // A signature on a plain text part (Gemini 3's text-only answers)
            // belongs to no thinking block: it is not carried.
            if has_sig && (is_thought || text.as_deref().unwrap_or("").is_empty()) {
                thinking_signature = sig;
            }

            if has_sig && text.as_deref().unwrap_or("").is_empty() && function_call.is_none() {
                continue;
            }

            if let Some(text) = text.filter(|t| !t.is_empty()) {
                if is_thought {
                    flush_text(&mut text_buf, &mut blocks);
                    thinking_buf.push_str(&text);
                    continue;
                }
                flush_thinking(&mut thinking_buf, &mut thinking_signature, &mut blocks);
                text_buf.push_str(&text);
                continue;
            }

            if let Some(fc) = function_call {
                flush_thinking(&mut thinking_buf, &mut thinking_signature, &mut blocks);
                flush_text(&mut text_buf, &mut blocks);
                has_tool_call = true;

                let upstream_name =
                    restore_sanitized_tool_name(sanitized_name_map.as_ref(), &gstr(fc.get("name")));
                let client_name = map_tool_name(tool_name_map.as_ref(), &upstream_name);
                tool_id_counter += 1;
                let input = match fc.get("args") {
                    Some(args @ Value::Object(_)) => args.clone(),
                    _ => json!({}),
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": sanitize_claude_tool_id(&format!("{upstream_name}-{tool_id_counter}")),
                    "name": client_name,
                    "input": input,
                }));
            }
        }
    }

    flush_thinking(&mut thinking_buf, &mut thinking_signature, &mut blocks);
    flush_text(&mut text_buf, &mut blocks);

    if !blocks.is_empty() {
        out["content"] = Value::Array(blocks);
    }

    let stop_reason = if has_tool_call {
        "tool_use"
    } else if gstr(gget(root, "candidates.0.finishReason")) == "MAX_TOKENS" {
        "max_tokens"
    } else {
        "end_turn"
    };
    out["stop_reason"] = Value::String(stop_reason.into());

    if input_tokens == 0 && output_tokens == 0 && root.get("usageMetadata").is_none() {
        if let Some(obj) = out.as_object_mut() {
            obj.shift_remove("usage");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(body: Value, model: &str) -> Value {
        translate_request(model, &body, false)
    }

    // port of TestConvertClaudeRequestToGemini_ToolChoice_SpecificTool
    #[test]
    fn tool_choice_specific_tool() {
        let out = req(
            json!({
                "model": "gemini-3-flash-preview",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "tools": [{"name": "json", "description": "A JSON tool",
                           "input_schema": {"type": "object", "properties": {}}}],
                "tool_choice": {"type": "tool", "name": "json"}
            }),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["toolConfig"],
            json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["json"]}})
        );
    }

    // port of TestConvertClaudeRequestToGemini_StringSystemInstruction
    #[test]
    fn string_system_instruction() {
        let out = req(
            json!({"model": "gemini-3-flash-preview", "system": "Be concise",
                   "messages": [{"role": "user", "content": "Hello"}]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["systemInstruction"],
            json!({"parts": [{"text": "Be concise"}]})
        );
        assert!(out.get("system_instruction").is_none());
    }

    // port of TestConvertClaudeRequestToGemini_ImageContent
    #[test]
    fn image_content() {
        let out = req(
            json!({"model": "gemini-3-flash-preview", "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this image"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
            ]}]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"],
            json!([{"role": "user", "parts": [
                {"text": "describe this image"},
                {"inline_data": {"mime_type": "image/png", "data": "aGVsbG8="}}
            ]}])
        );
    }

    // port of TestConvertClaudeRequestToGemini_StripsClaudeCodeAttribution
    #[test]
    fn strips_claude_code_attribution() {
        let out = req(
            json!({"model": "claude-sonnet-4-5", "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.63.abc; cc_entrypoint=cli; cch=12345;"},
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"type": "text", "text": "User system prompt"}
            ], "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["systemInstruction"],
            json!({"role": "user", "parts": [
                {"text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"text": "User system prompt"}
            ]})
        );
    }

    // port of TestConvertClaudeRequestToGemini_ConvertsMessageSystemRoleToUserContent
    #[test]
    fn converts_message_system_role_to_user_content() {
        let out = req(
            json!({"model": "gemini-3-flash-preview",
            "system": [{"type": "text", "text": "Top-level rules"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "system", "content": "String mid-conversation rule"},
                {"role": "system", "content": [{"type": "text", "text": "Array mid-conversation rule"}]}
            ]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"],
            json!([{"role": "user", "parts": [
                {"text": "Hello"},
                {"text": "<system-reminder>\nString mid-conversation rule\n</system-reminder>"},
                {"text": "<system-reminder>\nArray mid-conversation rule\n</system-reminder>"}
            ]}])
        );
        assert_eq!(
            out["systemInstruction"],
            json!({"role": "user", "parts": [{"text": "Top-level rules"}]})
        );
    }

    // port of TestConvertClaudeRequestToGemini_MessageLevelDeveloperInstructionsBecomeMergedUserReminder
    #[test]
    fn developer_messages_become_merged_user_reminder() {
        let out = req(
            json!({"model": "gemini-3-flash-preview", "system": "Top-level rules", "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "developer", "content": "String mid-conversation developer rule"},
                {"role": "developer", "content": [{"type": "text", "text": "Array mid-conversation developer rule"}]}
            ]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"],
            json!([{"role": "user", "parts": [
                {"text": "Hello"},
                {"text": "<system-reminder>\nString mid-conversation developer rule\n</system-reminder>"},
                {"text": "<system-reminder>\nArray mid-conversation developer rule\n</system-reminder>"}
            ]}])
        );
        assert_eq!(
            out["systemInstruction"],
            json!({"parts": [{"text": "Top-level rules"}]})
        );
    }

    // port of TestConvertClaudeRequestToGemini_PreservesToolPairingWithInterveningSystemMessage
    // (ValidateGeminiFunctionCallPairing replaced by exact equality of contents)
    #[test]
    fn preserves_tool_pairing_with_intervening_system_message() {
        let out = req(
            json!({"model": "gemini-3-flash-preview", "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Run two tools"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "tool_one", "input": {"a": 1}},
                    {"type": "tool_use", "id": "toolu_2", "name": "tool_two", "input": {"b": 2}}
                ]},
                {"role": "system", "content": "Context reminder between tool_use and tool_result"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "result 2"},
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "result 1"}
                ]}
            ]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"],
            json!([
                {"role": "user", "parts": [{"text": "Run two tools"}]},
                {"role": "model", "parts": [
                    {"thoughtSignature": "skip_thought_signature_validator",
                     "functionCall": {"name": "tool_one", "args": {"a": 1}, "id": "toolu_1"}},
                    {"thoughtSignature": "skip_thought_signature_validator",
                     "functionCall": {"name": "tool_two", "args": {"b": 2}, "id": "toolu_2"}}
                ]},
                {"role": "user", "parts": [
                    {"text": "<system-reminder>\nContext reminder between tool_use and tool_result\n</system-reminder>"},
                    {"functionResponse": {"name": "tool_one", "response": {"result": "result 1"}, "id": "toolu_1"}},
                    {"functionResponse": {"name": "tool_two", "response": {"result": "result 2"}, "id": "toolu_2"}}
                ]}
            ])
        );
    }

    // port of TestConvertClaudeRequestToGemini_SkipsEmptyTextParts
    #[test]
    fn skips_empty_text_parts() {
        let out = req(
            json!({"model": "claude-3-5-sonnet", "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": ""}, {"type": "text", "text": "hello"}, {"type": "text", "text": ""}
            ]}]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"],
            json!([{"role": "model", "parts": [{"text": "hello"}]}])
        );
    }

    // port of TestConvertClaudeRequestToGemini_StructuredToolResult
    #[test]
    fn structured_tool_result() {
        let out = req(
            json!({"model": "gemini-3-flash-preview", "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "json-call-1", "name": "json", "input": {"ok": true}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "json-call-1", "content": [
                    {"type": "text", "text": "alpha"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                ]}]}
            ]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"][1],
            json!({"role": "user", "parts": [
                {"functionResponse": {"name": "json",
                                      "response": {"result": {"type": "text", "text": "alpha"}},
                                      "id": "json-call-1"}},
                {"inline_data": {"mime_type": "image/png", "data": "aGVsbG8="}}
            ]})
        );
    }

    // port of TestConvertClaudeRequestToGemini_AlignsPermutedParallelToolResultsWithMixedText
    #[test]
    fn aligns_permuted_parallel_tool_results_with_mixed_text() {
        let out = req(
            json!({"model": "gemini-3.7-flash-high", "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "Read", "input": {"file_path": "/tmp/1"}},
                    {"type": "tool_use", "id": "call_2", "name": "Read", "input": {"file_path": "/tmp/2"}},
                    {"type": "tool_use", "id": "call_3", "name": "Read", "input": {"file_path": "/tmp/3"}}
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "Results arrived."},
                    {"type": "tool_result", "tool_use_id": "call_3", "content": "three"},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "two"},
                    {"type": "text", "text": "Continue."}
                ]}
            ]}),
            "gemini-3.7-flash-high",
        );
        let call = |id: &str, path: &str| {
            json!({"thoughtSignature": "skip_thought_signature_validator",
                   "functionCall": {"name": "Read", "args": {"file_path": path}, "id": id}})
        };
        let resp = |id: &str, r: &str| json!({"functionResponse": {"name": "Read", "response": {"result": r}, "id": id}});
        assert_eq!(
            out["contents"],
            json!([
                {"role": "model", "parts": [call("call_1", "/tmp/1"), call("call_2", "/tmp/2"), call("call_3", "/tmp/3")]},
                {"role": "user", "parts": [
                    {"text": "Results arrived."}, {"text": "Continue."},
                    resp("call_1", "one"), resp("call_2", "two"), resp("call_3", "three")
                ]}
            ])
        );
    }

    // port of TestConvertClaudeRequestToGemini_StringToolResult
    #[test]
    fn string_tool_result() {
        let out = req(
            json!({"model": "gemini-3-flash-preview", "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "json-call-1", "name": "json", "input": {"ok": true}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "json-call-1", "content": "alpha"}
                ]}
            ]}),
            "gemini-3-flash-preview",
        );
        assert_eq!(
            out["contents"][1]["parts"][0],
            json!({"functionResponse": {"name": "json", "response": {"result": "alpha"}, "id": "json-call-1"}})
        );
    }

    // port of TestConvertClaudeRequestToGemini_ToolResultWithTrailingSystemReminderReordersParts
    #[test]
    fn tool_result_with_trailing_system_reminder_reorders_parts() {
        let out = req(
            json!({"model": "gemini-3.8-flash", "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read the file"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_01_read", "name": "Read", "input": {"path": "main.go"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01_read", "content": "package main"},
                    {"type": "text", "text": "<system-reminder>\n<total_tokens>1234</total_tokens>\n</system-reminder>"}
                ]}
            ]}),
            "gemini-3.8-flash",
        );
        assert_eq!(out["contents"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            out["contents"][2],
            json!({"role": "user", "parts": [
                {"text": "<system-reminder>\n<total_tokens>1234</total_tokens>\n</system-reminder>"},
                {"functionResponse": {"name": "Read", "response": {"result": "package main"}, "id": "toolu_01_read"}}
            ]})
        );
    }

    // port of TestConvertClaudeRequestToGemini_ToolStrictMapsToValidatedMode
    #[test]
    fn tool_strict_maps_to_validated_mode() {
        let cases: [(Option<Value>, Value); 6] = [
            (
                None,
                json!({"functionCallingConfig": {"mode": "VALIDATED"}}),
            ),
            (
                Some(json!({"type": "auto"})),
                json!({"functionCallingConfig": {"mode": "VALIDATED"}}),
            ),
            (
                Some(Value::Null),
                json!({"functionCallingConfig": {"mode": "VALIDATED"}}),
            ),
            (
                Some(json!({"type": "none"})),
                json!({"functionCallingConfig": {"mode": "NONE"}}),
            ),
            (
                Some(json!({"type": "any"})),
                json!({"functionCallingConfig": {"mode": "ANY"}}),
            ),
            (
                Some(json!({"type": "tool", "name": "tool_a"})),
                json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["tool_a"]}}),
            ),
        ];
        for (tool_choice, want) in cases {
            let mut body = json!({"model": "gemini-3.8-flash",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "tool_a", "description": "Controlled tool.", "strict": true,
                           "input_schema": {"type": "object", "properties": {}}}]});
            if let Some(tc) = tool_choice {
                body["tool_choice"] = tc;
            }
            let out = req(body, "gemini-3.8-flash");
            assert_eq!(
                out["tools"],
                json!([{"functionDeclarations": [{"name": "tool_a", "description": "Controlled tool.",
                    "parametersJsonSchema": {"type": "object", "properties": {}}}]}])
            );
            assert_eq!(out["toolConfig"], want);
        }

        let mixed = req(
            json!({"model": "gemini-3.8-flash", "messages": [{"role": "user", "content": "hi"}], "tools": [
                {"name": "tool_a", "description": "Loose tool.", "strict": false,
                 "input_schema": {"type": "object", "properties": {}}},
                {"name": "tool_b", "description": "Strict tool.", "strict": true,
                 "input_schema": {"type": "object", "properties": {}}}
            ]}),
            "gemini-3.8-flash",
        );
        assert_eq!(
            mixed["toolConfig"],
            json!({"functionCallingConfig": {"mode": "VALIDATED"}})
        );

        let loose = req(
            json!({"model": "gemini-3.8-flash", "messages": [{"role": "user", "content": "hi"}], "tools": [
                {"name": "tool_a", "description": "Loose tool.", "strict": false,
                 "input_schema": {"type": "object", "properties": {}}},
                {"name": "tool_b", "description": "Unspecified tool.",
                 "input_schema": {"type": "object", "properties": {}}}
            ]}),
            "gemini-3.8-flash",
        );
        assert!(loose.get("toolConfig").is_none());
    }

    // port of TestConvertClaudeRequestToGemini_ParametersJsonSchema_PreservesAdditionalPropertiesAndPattern_Issue5959
    #[test]
    fn parameters_json_schema_preserves_additional_properties_and_pattern() {
        let out = req(
            json!({"model": "gemini-2.5-flash", "messages": [{"role": "user", "content": "Use the submit tool."}],
                "tools": [{"name": "submit", "description": "Submit a bounded schema test value.",
                    "input_schema": {
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object", "additionalProperties": false,
                        "properties": {"recipient": {"type": "string", "pattern": "^(alice|bob)$"},
                                       "amount": {"type": "number"}},
                        "required": ["recipient", "amount"]}}]}),
            "gemini-2.5-flash",
        );
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            json!({"type": "object", "additionalProperties": false,
                   "properties": {"recipient": {"type": "string", "pattern": "^(alice|bob)$"},
                                  "amount": {"type": "number"}},
                   "required": ["recipient", "amount"]})
        );
    }

    // port of TestConvertClaudeRequestToGemini_FunctionResponseJSONRef
    #[test]
    fn function_response_json_ref() {
        let out = req(
            json!({"model": "gemini-3.8-flash", "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_schema_1", "name": "get_schema", "input": {}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_schema_1",
                    "content": {"schema": {"$ref": "#/components/schemas/ErrorModel"}}}]}
            ]}),
            "gemini-3.8-flash",
        );
        assert_eq!(
            out["contents"][1]["parts"][0]["functionResponse"]["response"]["result"],
            json!(r##"{"schema":{"$ref":"#/components/schemas/ErrorModel"}}"##)
        );
    }

    // port of the default-form half of TestConvertClaudeRequestToGeminiWithCompatPreservesEmptyThinking
    // (gemini_claude_compat_test.go): the plain translator drops thinking.
    #[test]
    fn default_translation_drops_thinking() {
        let out = req(
            json!({"messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reason", "signature": ""}
            ]}]}),
            "deepseek-v4",
        );
        assert_eq!(out["contents"], json!([{"role": "model", "parts": []}]));
    }

    /// Whole-body equality against output captured from the Go translator
    /// (ed980be) for the same input, covering key order, generation config,
    /// thinking, safety settings and the trailing-call strip.
    #[test]
    fn full_request_matches_go_output() {
        let body = json!({
            "model": "claude-x", "system": "sys", "temperature": 0.5, "top_p": 1, "top_k": 40,
            "thinking": {"type": "adaptive"},
            "tool_choice": "auto",
            "tools": [{"name": "my tool!", "description": "d", "cache_control": {"type": "ephemeral"},
                "input_schema": {"type": "object", "properties": {
                    "mode": {"const": "fast"},
                    "kind": {"type": ["string", "null"], "enum": ["a", "b"]},
                    "x-ext": {"type": "string"}},
                    "required": ["kind", "mode", "gone"], "x-meta": 1}}],
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [{"type": "text", "text": "a"},
                    {"type": "tool_use", "id": "t1", "name": "my tool!", "input": "{\"k\":1}"}]}
            ]
        });
        let out = translate_request("gemini-3.7-flash", &body, true);
        let want = json!({
            "contents": [{"role": "user", "parts": [{"text": "q"}]}],
            "model": "gemini-3.7-flash",
            "systemInstruction": {"parts": [{"text": "sys"}]},
            "tools": [{"functionDeclarations": [{"name": "my_tool_", "description": "d",
                "parametersJsonSchema": {"type": "object", "properties": {
                    "mode": {"enum": ["fast"], "type": "string"},
                    "kind": {"type": "string", "enum": ["a", "b"], "description": "Allowed: a, b"},
                    "x-ext": {"type": "string"}},
                    "required": ["kind", "mode"]}}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 65535, "includeThoughts": true},
                                 "temperature": 0.5, "topP": 1, "topK": 40},
            "safetySettings": default_safety_settings()
        });
        assert_eq!(out, want);
        assert_eq!(out.to_string(), want.to_string(), "key order");
    }

    // port of TestCleanJSONSchemaForGeminiJSONSchema_PreservesAdditionalPropertiesAndPattern_Issue5959
    // (util/gemini_schema_test.go), expected value captured from the Go cleaner.
    #[test]
    fn schema_preserves_additional_properties_and_pattern() {
        let got = clean_json_schema_for_gemini_json_schema(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema", "title": "SubmitTool",
            "type": "object", "additionalProperties": false,
            "properties": {
                "recipient": {"type": "string", "pattern": "^(alice|bob)$", "minLength": 3, "maxLength": 10},
                "amount": {"type": "number", "minimum": 1},
                "nested": {"type": "object", "additionalProperties": false,
                           "properties": {"tag": {"type": "string", "pattern": "^[a-z]+$"}}},
                "items_list": {"type": "array", "items": {"type": "string", "pattern": "^[0-9]+$"}}
            },
            "required": ["recipient", "amount", "non_existent"]
        }));
        let want = json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "recipient": {"type": "string", "pattern": "^(alice|bob)$", "minLength": 3, "maxLength": 10},
                "amount": {"type": "number", "minimum": 1},
                "nested": {"type": "object", "additionalProperties": false,
                           "properties": {"tag": {"type": "string", "pattern": "^[a-z]+$"}}},
                "items_list": {"type": "array", "items": {"type": "string", "pattern": "^[0-9]+$"}}
            },
            "required": ["recipient", "amount"]
        });
        assert_eq!(got, want);
        assert_eq!(got.to_string(), want.to_string(), "key order");
    }

    // port of TestCleanJSONSchemaForGeminiJSONSchema_PreservesSchemaValuedAdditionalProperties
    #[test]
    fn schema_preserves_schema_valued_additional_properties() {
        let input = json!({"type": "object",
            "additionalProperties": {"type": "string", "pattern": "^[a-z]+$", "minLength": 2}});
        assert_eq!(clean_json_schema_for_gemini_json_schema(&input), input);
    }

    // port of TestCleanJSONSchema_ArrayItemsRequireArrayType_Issue6011 (geminiJSONSchema cases)
    #[test]
    fn schema_array_items_require_array_type() {
        // Missing type beside items: Go's repair re-marshals with sorted keys.
        let got = clean_json_schema_for_gemini_json_schema(
            &json!({"type": "object", "properties": {
            "revision_reasons": {"type": "array", "items": {"type": "object", "properties": {
                "evidence_reference": {"description": "references to evidence", "items": {"type": "string"}}}}}}}),
        );
        let want = json!({"properties": {"revision_reasons": {"items": {"properties": {
            "evidence_reference": {"description": "references to evidence", "items": {"type": "string"},
                                   "type": "array"}}, "type": "object"}, "type": "array"}}, "type": "object"});
        assert_eq!(got, want);
        assert_eq!(got.to_string(), want.to_string(), "key order");

        let got = clean_json_schema_for_gemini_json_schema(
            &json!({"type": "object", "properties": {
            "revision_reasons": {"type": "array", "items": {"type": "object", "properties": {
                "evidence_reference": {"type": ["string", "array"], "items": {"type": "string"}}}}}}}),
        );
        assert_eq!(
            got,
            json!({"type": "object", "properties": {"revision_reasons": {"type": "array", "items": {
                "type": "object", "properties": {"evidence_reference": {"type": "array",
                    "items": {"type": "string"}, "description": "Accepts: string | array"}}}}}})
        );

        let got = clean_json_schema_for_gemini_json_schema(
            &json!({"type": "object", "properties": {
            "label": {"type": "string", "items": {"type": "string"}},
            "config": {"type": "object", "properties": {"key": {"type": "string"}}, "items": {"type": "string"}}}}),
        );
        assert_eq!(
            got,
            json!({"type": "object", "properties": {
                "label": {"type": "string"},
                "config": {"type": "object", "properties": {"key": {"type": "string"}}}}})
        );
    }

    /// Unions, allOf, $ref, conditionals and extension fields together —
    /// expected value captured from the Go cleaner.
    #[test]
    fn schema_flattening_matches_go_output() {
        let got = clean_json_schema_for_gemini_json_schema(&json!({
            "type": "object",
            "properties": {
                "a": {"description": "A", "anyOf": [{"type": "string"}, {"type": "null"}, {"type": "integer"}]},
                "b": {"$ref": "#/$defs/B"},
                "c": {"allOf": [{"properties": {"x": {"type": "string"}}, "required": ["x"]}]},
                "d": {"type": "object", "properties": {"p": {"type": "string"}},
                      "oneOf": [{"properties": {"q": {"type": "number"}}}, {"type": "null"}]},
                "e": {"type": "object", "if": {"properties": {"z": {"const": 1}}},
                      "then": {"properties": {"t": {"type": "string"}}}, "x-google": true},
                "f": {"type": ["integer", "number", "null"]}
            },
            "required": ["a", "f", "b"],
            "$defs": {"B": {"type": "string"}}
        }));
        let want = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "description": "A (Accepts: string | null | integer)"},
                "b": {"type": "object", "description": "See: B"},
                "c": {"properties": {"x": {"type": "string"}}, "required": ["x"]},
                "d": {"type": "object", "properties": {"p": {"type": "string"}, "q": {"type": "number"}}},
                "e": {"type": "object", "properties": {"t": {"type": "string"}}},
                "f": {"type": "integer", "description": "Accepts: integer | number ((nullable))"}
            },
            "required": ["a", "b"]
        });
        assert_eq!(got, want);
        assert_eq!(got.to_string(), want.to_string(), "key order");
    }

    #[test]
    fn schema_root_true_normalizes_to_empty_object() {
        // port of TestCleanJSONSchema_RootAndWrappedTrue (wrapped half)
        assert_eq!(
            clean_json_schema_for_gemini_json_schema(&json!({"schema": true})),
            json!({"schema": {}})
        );
        assert_eq!(
            clean_json_schema_for_gemini_json_schema(&json!(true)),
            json!({})
        );
    }

    #[test]
    fn sanitize_function_name_rules() {
        assert_eq!(sanitize_function_name("a b/c"), "a_b_c");
        assert_eq!(sanitize_function_name("1abc"), "_1abc");
        assert_eq!(
            sanitize_function_name("mcp.server:tool-x"),
            "mcp.server:tool-x"
        );
        assert_eq!(
            sanitize_function_name(&"9".repeat(70)),
            format!("_{}", "9".repeat(63))
        );
        assert_eq!(sanitize_function_name(&"a".repeat(70)), "a".repeat(64));
        assert_eq!(sanitize_function_name("工具"), "__");
    }

    fn run_stream(request: &Value, chunks: &[Value]) -> Vec<String> {
        let mut t = StreamTranslator::new(request);
        let mut frames = Vec::new();
        for c in chunks {
            frames.extend(t.push(None, c));
        }
        frames.extend(t.finish());
        frames
    }

    fn frame(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    // port of TestConvertGeminiResponseToClaude_SignatureOnlyPartDoesNotOpenEmptyTextBlock
    #[test]
    fn stream_signature_only_part_does_not_open_empty_text_block() {
        let request = json!({"model": "gemini-test", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        let frames = run_stream(
            &request,
            &[
                json!({"candidates": [{"content": {"parts": [{"text": "thinking text", "thought": true}]}}],
                       "modelVersion": "gemini-test", "responseId": "resp-test"}),
                json!({"candidates": [{"content": {"parts": [{"text": "", "thoughtSignature": "sig-test"}]},
                                       "finishReason": "STOP"}],
                       "usageMetadata": {"promptTokenCount": 10, "thoughtsTokenCount": 2, "totalTokenCount": 12},
                       "modelVersion": "gemini-test", "responseId": "resp-test"}),
            ],
        );
        assert_eq!(
            frames,
            vec![
                frame(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "resp-test",
                    "type": "message", "role": "assistant", "content": [], "model": "gemini-test",
                    "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}}})
                ),
                frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "thinking text"}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "signature_delta", "signature": "sig-test"}})
                ),
                frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0})
                ),
                frame(
                    "message_delta",
                    json!({"type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"input_tokens": 10, "output_tokens": 2}})
                ),
                frame("message_stop", json!({"type": "message_stop"})),
            ]
        );
    }

    // port of TestConvertGeminiResponseToClaude_UsageWithCachedContentTokenCount
    #[test]
    fn stream_usage_with_cached_content_token_count() {
        let request =
            json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
        let mut t = StreamTranslator::new(&request);
        let frames = t.push(
            None,
            &json!({"candidates": [{"content": {"parts": [{"text": "Hello world"}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 7, "cachedContentTokenCount": 91},
                    "modelVersion": "gemini-2.5-pro", "responseId": "resp-usage-cache"}),
        );
        assert_eq!(
            frames.last().cloned(),
            Some(frame(
                "message_delta",
                json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"input_tokens": 9, "output_tokens": 7, "cache_read_input_tokens": 91}})
            ))
        );
    }

    // port of TestConvertGeminiResponseToClaudeNonStream_PreservesThoughtSignature
    #[test]
    fn non_stream_preserves_thought_signature() {
        let request =
            json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
        let out = translate_non_stream(
            &json!({"candidates": [{"content": {"parts": [
                        {"text": "thinking step 1\n", "thought": true},
                        {"text": "thinking step 2", "thought": true, "thoughtSignature": "sig-xyz-123"},
                        {"text": "visible answer"}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
                    "modelVersion": "gemini-2.5-pro", "responseId": "resp-non-stream"}),
            &request,
        );
        let want = json!({"id": "resp-non-stream", "type": "message", "role": "assistant",
            "model": "gemini-2.5-pro",
            "content": [
                {"type": "thinking", "thinking": "thinking step 1\nthinking step 2", "signature": "sig-xyz-123"},
                {"type": "text", "text": "visible answer"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 5}});
        assert_eq!(out, want);
        assert_eq!(out.to_string(), want.to_string(), "key order");
    }

    // Gemini 3 puts the thought signature on the last TEXT part of a
    // text-only answer: that part is visible text, not thinking (Go's
    // `thought || hasThoughtSignature` moved the end of the answer into the
    // collapsed thinking block).
    #[test]
    fn non_stream_signed_plain_text_part_stays_text() {
        let request =
            json!({"model": "gemini-3-pro", "messages": [{"role": "user", "content": "hi"}]});
        let out = translate_non_stream(
            &json!({"candidates": [{"content": {"parts": [
                        {"text": "thinking…", "thought": true},
                        {"text": "The capital of France"},
                        {"text": " is Paris.", "thought_signature": "CpYBAXSig=="}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
                    "modelVersion": "gemini-3-pro", "responseId": "resp-non-stream-2"}),
            &request,
        );
        assert_eq!(
            out["content"],
            json!([{"type": "thinking", "thinking": "thinking…"},
                   {"type": "text", "text": "The capital of France is Paris."}])
        );
    }

    #[test]
    fn stream_signed_plain_text_part_stays_text() {
        let mut t = StreamTranslator::new(&json!({}));
        let mut frames = String::new();
        for chunk in [
            json!({"candidates": [{"content": {"parts": [{"text": "thinking…", "thought": true}], "role": "model"}, "index": 0}], "responseId": "r1"}),
            json!({"candidates": [{"content": {"parts": [{"text": "The capital of France"}], "role": "model"}, "index": 0}]}),
            json!({"candidates": [{"content": {"parts": [{"text": " is Paris.", "thoughtSignature": "CpYBAXSig=="}], "role": "model"}, "index": 0, "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 7, "totalTokenCount": 17}}),
        ] {
            frames.push_str(&t.push(None, &chunk).join(""));
        }
        frames.push_str(&t.finish().join(""));
        assert!(
            frames.contains("\"text_delta\",\"text\":\" is Paris.\""),
            "{frames}"
        );
        assert!(
            !frames.contains("\"thinking_delta\",\"thinking\":\" is Paris.\""),
            "{frames}"
        );
    }

    // port of TestConvertGeminiResponseToClaudeNonStream_TrailingSignatureOnlyPart
    #[test]
    fn non_stream_trailing_signature_only_part() {
        let request =
            json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
        let out = translate_non_stream(
            &json!({"candidates": [{"content": {"parts": [
                        {"text": "thinking step 1\n", "thought": true},
                        {"text": "", "thoughtSignature": "sig-trailing"},
                        {"text": "visible answer"}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
                    "modelVersion": "gemini-2.5-pro", "responseId": "resp-non-stream-trailing"}),
            &request,
        );
        assert_eq!(
            out["content"],
            json!([{"type": "thinking", "thinking": "thinking step 1\n", "signature": "sig-trailing"},
                   {"type": "text", "text": "visible answer"}])
        );
    }

    // port of TestConvertGeminiResponseToClaudeNonStream_UsageWithCachedContentTokenCount
    #[test]
    fn non_stream_usage_with_cached_content_token_count() {
        let request =
            json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
        let out = translate_non_stream(
            &json!({"candidates": [{"content": {"parts": [{"text": "Hello world"}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 7, "cachedContentTokenCount": 91},
                    "modelVersion": "gemini-2.5-pro", "responseId": "resp-usage-cache-nonstream"}),
            &request,
        );
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 9, "output_tokens": 7, "cache_read_input_tokens": 91})
        );
    }

    #[test]
    fn non_stream_tool_call_restores_names_and_drops_empty_usage() {
        let request = json!({"tools": [{"name": "Read File", "input_schema": {"type": "object"}}]});
        let out = translate_non_stream(
            &json!({"candidates": [{"content": {"parts": [
                        {"text": "ok"},
                        {"functionCall": {"name": "Read_File", "args": {"p": 1}}},
                        {"functionCall": {"name": "other", "args": "bad"}}]}, "finishReason": "STOP"}],
                    "modelVersion": "m", "responseId": "r"}),
            &request,
        );
        assert_eq!(
            out,
            json!({"id": "r", "type": "message", "role": "assistant", "model": "m",
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "Read_File-1", "name": "Read File", "input": {"p": 1}},
                    {"type": "tool_use", "id": "other-2", "name": "other", "input": {}}],
                "stop_reason": "tool_use", "stop_sequence": null})
        );
    }

    /// End-to-end: a realistic Gemini stream (thought → thought signature →
    /// text → function call with a sanitized name → usage on the final chunk)
    /// and the exact Anthropic frames it becomes.
    #[test]
    fn stream_end_to_end_thinking_text_and_tool_call() {
        let request = json!({"model": "claude-x", "messages": [{"role": "user", "content": "go"}],
            "tools": [{"name": "read file", "input_schema": {"type": "object"}}]});
        let chunks = [
            json!({"candidates": [{"content": {"role": "model", "parts": [
                    {"text": "Let me think.", "thought": true}]}}],
                   "usageMetadata": {"promptTokenCount": 12, "totalTokenCount": 12},
                   "modelVersion": "gemini-3.7-flash", "responseId": "resp-e2e"}),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                    {"text": " Done.", "thought": true, "thoughtSignature": "c2lnLTE="}]}}],
                   "modelVersion": "gemini-3.7-flash", "responseId": "resp-e2e"}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Reading "}]}}],
                   "modelVersion": "gemini-3.7-flash", "responseId": "resp-e2e"}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "it now."}]}}],
                   "modelVersion": "gemini-3.7-flash", "responseId": "resp-e2e"}),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                    {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}},
                     "thoughtSignature": "c2lnLTI="}]},
                    "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 30,
                                     "thoughtsTokenCount": 8, "cachedContentTokenCount": 20,
                                     "totalTokenCount": 158},
                   "modelVersion": "gemini-3.7-flash", "responseId": "resp-e2e"}),
        ];
        let frames = run_stream(&request, &chunks);

        // The tool id carries the process-wide counter: `<name>-<n>`.
        let start: Value = serde_json::from_str(
            frames[10]
                .strip_prefix("event: content_block_start\ndata: ")
                .and_then(|s| s.strip_suffix("\n\n"))
                .expect("tool_use start frame"),
        )
        .expect("json");
        let id = start["content_block"]["id"]
            .as_str()
            .expect("id")
            .to_string();
        let n = id.strip_prefix("read_file-").expect("id prefix");
        assert!(n.parse::<u64>().is_ok(), "id suffix {id}");

        assert_eq!(
            frames,
            vec![
                frame(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "resp-e2e",
                    "type": "message", "role": "assistant", "content": [], "model": "gemini-3.7-flash",
                    "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}}})
                ),
                frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "Let me think."}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": " Done."}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "signature_delta", "signature": "c2lnLTE="}})
                ),
                frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0})
                ),
                frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 1,
                    "content_block": {"type": "text", "text": ""}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1,
                    "delta": {"type": "text_delta", "text": "Reading "}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1,
                    "delta": {"type": "text_delta", "text": "it now."}})
                ),
                frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 1})
                ),
                frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 2,
                    "content_block": {"type": "tool_use", "id": id, "name": "read file", "input": {}}})
                ),
                frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 2,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"a.txt\"}"}})
                ),
                frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 2})
                ),
                frame(
                    "message_delta",
                    json!({"type": "message_delta",
                    "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                    "usage": {"input_tokens": 100, "output_tokens": 38, "cache_read_input_tokens": 20}})
                ),
                frame("message_stop", json!({"type": "message_stop"})),
            ]
        );
    }

    #[test]
    fn stream_without_content_emits_no_stop() {
        let mut t = StreamTranslator::new(&json!({}));
        let frames = t.push(
            None,
            &json!({"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}],
                                          "usageMetadata": {"promptTokenCount": 1}}),
        );
        assert_eq!(frames.len(), 1, "only message_start");
        assert!(t.finish().is_empty());
    }
}
