//! OpenAI Chat Completions (client) ⇄ Anthropic Messages (upstream) translator.
//!
//! Faithful port of CLIProxyAPI `internal/translator/claude/openai/chat-completions/`
//! at commit `ed980be` (`claude_openai_request.go`, `claude_openai_response.go`),
//! plus the helpers they call in other packages (`translator/common`, `util`,
//! `thinking`). Each ported function carries a `// port of <GoFunc> (<file>)`
//! comment so the two can be diffed when upstream moves.
//!
//! The Go code works on raw bytes through gjson/sjson; this port works on
//! `serde_json::Value`. The gjson coercion rules the Go code relies on
//! (`.String()` / `.Int()` / `.Float()` on any JSON type, `.Exists()` being
//! true for an explicit `null`, `.Array()` wrapping a scalar) are reproduced by
//! the `g*` helpers below so edge cases behave the same.
//!
//! Deliberately NOT ported: the `model(level)` thinking-suffix parsing, token
//! counting, metrics/logging and config/plugin hooks. Termory has no model
//! registry, so `registry.LookupModelInfo` is a stand-in that knows no model
//! (the same choice `thinking.rs` makes): `reasoning_effort` always takes the
//! manual (`enabled` + `budget_tokens`) branch.
//!
//! Deviations from the Go code, each deliberate:
//! - `GenerateClaudeToolCallID` draws its 24 characters from a time + atomic
//!   counter mix instead of `crypto/rand` (same alphabet, prefix and length).
//! - Values Go copies by RAW text (`schema.Raw` inside the structured-output
//!   instruction, a tool result's unrecognised `content.Raw`) are re-serialised
//!   compactly, so insignificant whitespace from the client is not preserved.
//! - `translate_non_stream` also accepts a complete Messages object (replayed
//!   as the events Anthropic would have streamed) or a parsed event array; Go
//!   only reads the SSE text, which a string body still is.
//! - `StreamTranslator::finish` emits the `data: [DONE]` terminator that the Go
//!   handler layer (not the translator) writes for OpenAI clients.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// gjson / sjson-compatible accessors
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

/// gjson `Result.String()`: strings verbatim, scalars as their literal, null /
/// missing as "", objects and arrays as their JSON text.
fn gstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
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

/// gjson `Result.Float()`.
fn gfloat(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        Some(Value::Bool(true)) => 1.0,
        _ => 0.0,
    }
}

/// gjson `Result.Array()`: missing / null → empty, an array → its items, any
/// other value → a one-element list holding it.
fn garray(v: Option<&Value>) -> Vec<&Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(other) => vec![other],
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

/// sjson `SetBytes` on a dotted object path: missing (or non-object)
/// intermediates become objects, an existing key keeps its position, a new
/// key is appended.
fn sj_set(root: &mut Value, path: &str, value: Value) {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let map = cur.as_object_mut().expect("object");
        if i == segs.len() - 1 {
            map.insert((*seg).to_string(), value);
            return;
        }
        cur = map
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// sjson `DeleteBytes` on a dotted object path (no-op when absent).
fn sj_delete(root: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (Some(p), l),
        None => (None, path),
    };
    let target = match parent {
        Some(p) => {
            let mut cur = &mut *root;
            for seg in p.split('.') {
                cur = match cur.get_mut(seg) {
                    Some(v) => v,
                    None => return,
                };
            }
            cur
        }
        None => root,
    };
    if let Some(m) = target.as_object_mut() {
        m.shift_remove(last);
    }
}

/// `sjson.SetBytes(x, "choices.0.<path>", value)` on the single-choice
/// templates both response directions build.
fn set_choice(template: &mut Value, path: &str, value: Value) {
    if let Some(choice) = template.get_mut("choices").and_then(|c| c.get_mut(0)) {
        sj_set(choice, path, value);
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn sha256_hex(input: &str) -> String {
    let sum = Sha256::digest(input.as_bytes());
    sum.iter().map(|b| format!("{b:02x}")).collect()
}

/// Module-private id source: wall-clock nanoseconds plus a process-wide
/// counter, so two ids minted in the same nanosecond still differ.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_id_seed() -> (u128, u64) {
    (
        now_unix_nanos(),
        ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1,
    )
}

// ---------------------------------------------------------------------------
// translator/common
// ---------------------------------------------------------------------------

const TOOLU_LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

// port of GenerateClaudeToolCallID (translator/common/request.go); the 24
// characters come from a splitmix64 stream over the time+counter seed instead
// of crypto/rand.
fn generate_claude_tool_call_id() -> String {
    let (nanos, counter) = next_id_seed();
    let mut state =
        (nanos as u64) ^ ((nanos >> 64) as u64) ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity("toolu_".len() + 24);
    out.push_str("toolu_");
    for _ in 0..24 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push(TOOLU_LETTERS[(z % TOOLU_LETTERS.len() as u64) as usize] as char);
    }
    out
}

// port of isValidCacheControl (translator/common/cache_control.go)
fn is_valid_cache_control(cc: Option<&Value>) -> bool {
    match cc {
        Some(Value::Object(m)) => {
            matches!(m.get("type"), Some(Value::String(t)) if t == "ephemeral")
        }
        _ => false,
    }
}

// port of AttachCacheControl (translator/common/cache_control.go)
fn attach_cache_control(dst: &mut Value, src: &Value) {
    let cc = src.get("cache_control");
    if !is_valid_cache_control(cc) {
        return;
    }
    if let (Some(m), Some(cc)) = (dst.as_object_mut(), cc) {
        m.insert("cache_control".to_string(), cc.clone());
    }
}

// port of AttachMessageCacheControl (translator/common/cache_control.go)
fn attach_message_cache_control(msg: &mut Value, src: &Value) {
    let cc = src.get("cache_control");
    if !is_valid_cache_control(cc) {
        return;
    }
    let cc = cc.cloned().unwrap_or(Value::Null);
    match msg.get_mut("content") {
        Some(Value::Array(arr)) => {
            let Some(last) = arr.last_mut() else {
                return;
            };
            if last.get("cache_control").is_some() {
                return;
            }
            sj_set(last, "cache_control", cc);
        }
        Some(Value::String(s)) => {
            let text_part = json!({"type": "text", "text": s.clone(), "cache_control": cc});
            msg["content"] = Value::Array(vec![text_part]);
        }
        _ => {}
    }
}

// port of AttachToolMessageCacheControl (translator/common/cache_control.go)
fn attach_tool_message_cache_control(msg: &mut Value, src: &Value) {
    let mut raw_cc = extract_first_part_cache_control(src);
    if raw_cc.is_none() {
        let cc = src.get("cache_control");
        if is_valid_cache_control(cc) {
            raw_cc = cc.cloned();
        }
    }
    let Some(raw_cc) = raw_cc else {
        return;
    };
    if let Some(Value::Array(arr)) = msg.get_mut("content") {
        if let Some(block) = arr
            .iter_mut()
            .find(|block| gstr(block.get("type")) == "tool_result")
        {
            sj_set(block, "cache_control", raw_cc);
        }
    }
}

// port of extractFirstPartCacheControl (translator/common/cache_control.go)
fn extract_first_part_cache_control(src: &Value) -> Option<Value> {
    let content = src.get("content").unwrap_or(src);
    match content {
        Value::Array(parts) => parts
            .iter()
            .map(|part| part.get("cache_control"))
            .find(|cc| is_valid_cache_control(*cc))
            .flatten()
            .cloned(),
        Value::Object(_) => {
            let cc = content.get("cache_control");
            if is_valid_cache_control(cc) {
                cc.cloned()
            } else {
                None
            }
        }
        _ => None,
    }
}

// port of DeriveClaudeUserID (translator/common/claude_user_id.go)
fn derive_claude_user_id(root: &Value) -> String {
    if let Some(Value::String(raw)) = gget(root, "metadata.user_id") {
        if !raw.trim().is_empty() {
            return raw.clone();
        }
    }
    if let Some(Value::String(raw)) = root.get("user") {
        if !raw.trim().is_empty() {
            return raw.clone();
        }
    }

    let mut seed = String::new();

    if let Some(v) = root.get("prompt_cache_key") {
        let value = gstr(Some(v)).trim().to_string();
        if !value.is_empty() {
            seed.push_str("prompt_cache_key:");
            seed.push_str(&value);
        }
    }

    if seed.is_empty() {
        for path in ["session_id", "sessionId"] {
            if let Some(v) = root.get(path) {
                let value = gstr(Some(v)).trim().to_string();
                if !value.is_empty() {
                    seed.push_str("session_id:");
                    seed.push_str(&value);
                    break;
                }
            }
        }
    }

    if seed.is_empty() {
        let conversation = root.get("conversation");
        let sid = gstr(conversation.and_then(|c| c.get("id")))
            .trim()
            .to_string();
        if !sid.is_empty() {
            seed.push_str("conversation_id:");
            seed.push_str(&sid);
        } else if let Some(Value::String(s)) = conversation {
            let sid = s.trim();
            if !sid.is_empty() {
                seed.push_str("conversation_id:");
                seed.push_str(sid);
            }
        } else if let Some(v) = root.get("conversation_id") {
            let sid = gstr(Some(v)).trim().to_string();
            if !sid.is_empty() {
                seed.push_str("conversation_id:");
                seed.push_str(&sid);
            }
        }
    }

    if seed.is_empty() {
        let content = first_stable_request_content(root);
        if !content.is_empty() {
            seed.push_str("content:");
            seed.push_str(&content);
        }
    }

    if seed.is_empty() {
        if let Some(v) = root.get("model") {
            let value = gstr(Some(v)).trim().to_string();
            if !value.is_empty() {
                seed.push_str("model:");
                seed.push_str(&value);
            }
        }
        for key in [
            "instructions",
            "system",
            "systemInstruction",
            "system_instruction",
        ] {
            if let Some(v) = root.get(key) {
                seed.push(';');
                seed.push_str(key);
                seed.push(':');
                seed.push_str(&gstr(Some(v)));
            }
        }
    }

    if seed.is_empty() {
        return "unknown".to_string();
    }
    sha256_hex(&seed)
}

// port of firstStableRequestContent (translator/common/claude_user_id.go)
fn first_stable_request_content(root: &Value) -> String {
    if let Some(Value::Array(messages)) = root.get("messages") {
        for message in messages {
            let role = gstr(message.get("role")).trim().to_lowercase();
            if role == "user" {
                let content = extract_text_content(message.get("content"));
                if !content.is_empty() {
                    return content;
                }
            }
        }
    }

    if let Some(input) = root.get("input") {
        match input {
            Value::String(s) => {
                let text = s.trim();
                if !text.is_empty() {
                    return text.to_string();
                }
            }
            Value::Array(items) => {
                for item in items {
                    if is_responses_user_item(item) {
                        let content = extract_responses_item_text(item.get("content"));
                        if !content.is_empty() {
                            return content;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(Value::Array(contents)) = root.get("contents") {
        for content_item in contents {
            let role = gstr(content_item.get("role")).trim().to_lowercase();
            // In Gemini API format, missing role defaults to "user"
            if role.is_empty() || role == "user" {
                if let Some(Value::Array(parts)) = content_item.get("parts") {
                    let mut texts: Vec<String> = Vec::new();
                    for part in parts {
                        if is_gemini_thought_part(part) {
                            continue;
                        }
                        if let Some(text) = part.get("text") {
                            let val = gstr(Some(text)).trim().to_string();
                            if !val.is_empty() {
                                texts.push(val);
                            }
                        }
                    }
                    if !texts.is_empty() {
                        return texts.join("\n");
                    }
                }
            }
        }
    }

    String::new()
}

// port of IsGeminiThoughtPart (translator/common/gemini.go): gjson `.Bool()`.
fn is_gemini_thought_part(part: &Value) -> bool {
    match part.get("thought") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "1" | "t" | "true"),
        _ => false,
    }
}

// port of extractTextContent (translator/common/claude_user_id.go)
fn extract_text_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<String> = Vec::new();
            for part in parts {
                if gstr(part.get("type")) == "text" {
                    if let Some(text) = part.get("text") {
                        let val = gstr(Some(text)).trim().to_string();
                        if !val.is_empty() {
                            texts.push(val);
                        }
                    }
                }
            }
            texts.join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

// port of isResponsesUserItem (translator/common/claude_user_id.go)
fn is_responses_user_item(item: &Value) -> bool {
    let role = gstr(item.get("role")).trim().to_lowercase();
    if role == "user" {
        return true;
    }
    if role == "system" || role == "developer" || role == "assistant" {
        return false;
    }
    gstr(item.get("type")).trim().to_lowercase() == "message"
}

// port of extractResponsesItemText (translator/common/claude_user_id.go)
fn extract_responses_item_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<String> = Vec::new();
            for part in parts {
                if matches!(
                    gstr(part.get("type")).as_str(),
                    "input_text" | "output_text" | "text"
                ) {
                    if let Some(text) = part.get("text") {
                        let val = gstr(Some(text)).trim().to_string();
                        if !val.is_empty() {
                            texts.push(val);
                        }
                    }
                }
            }
            texts.join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

// port of ClaudeMessageAccumulator (translator/common/claude_messages.go)
#[derive(Default)]
struct ClaudeMessageAccumulator {
    messages: Vec<Value>,
    role: String,
    content: Vec<Value>,
    tool_use_parts: Vec<Value>,
}

impl ClaudeMessageAccumulator {
    // port of ClaudeMessageAccumulator.Append (translator/common/claude_messages.go)
    fn append(&mut self, message: Value) {
        let role = gstr(message.get("role"));
        if role != "user" && role != "assistant" {
            return;
        }
        let parts = claude_message_content_parts(message.get("content"));
        if parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        self.role = role.clone();
        for part in parts {
            if role == "assistant" && gstr(part.get("type")) == "tool_use" {
                self.tool_use_parts.push(part);
                continue;
            }
            self.content.push(part);
        }
    }

    // port of ClaudeMessageAccumulator.Flush (translator/common/claude_messages.go)
    fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.content);
        parts.append(&mut self.tool_use_parts);
        if !parts.is_empty() {
            self.messages
                .push(json!({"role": self.role, "content": parts}));
        }
        self.role.clear();
    }

    // port of ClaudeMessageAccumulator.Messages (translator/common/claude_messages.go)
    fn into_messages(mut self) -> Vec<Value> {
        self.flush();
        self.messages
    }
}

// port of claudeMessageContentParts (translator/common/claude_messages.go)
fn claude_message_content_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({"type": "text", "text": s})]
            }
        }
        Some(Value::Array(parts)) => parts.iter().filter(|p| p.is_object()).cloned().collect(),
        _ => Vec::new(),
    }
}

const JSON_OBJECT_INSTRUCTION: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";

// port of BuildClaudeStructuredOutputInstruction (translator/common/claude_system.go)
fn build_claude_structured_output_instruction(format: Option<&Value>) -> String {
    let Some(format) = format else {
        return String::new();
    };

    let format_type = gstr(format.get("type")).trim().to_lowercase();
    match format_type.as_str() {
        "json_object" => JSON_OBJECT_INSTRUCTION.to_string(),
        "json_schema" => {
            let json_schema = format.get("json_schema");
            let schema = json_schema
                .and_then(|j| j.get("schema"))
                .or_else(|| format.get("schema"));
            let Some(schema) = schema else {
                return JSON_OBJECT_INSTRUCTION.to_string();
            };

            let mut builder = String::from(
                "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n",
            );
            let mut name = gstr(json_schema.and_then(|j| j.get("name")))
                .trim()
                .to_string();
            if name.is_empty() {
                name = gstr(format.get("name")).trim().to_string();
            }
            if !name.is_empty() {
                builder.push_str("Schema Name: ");
                builder.push_str(&name);
                builder.push('\n');
            }
            let mut desc = gstr(json_schema.and_then(|j| j.get("description")))
                .trim()
                .to_string();
            if desc.is_empty() {
                desc = gstr(format.get("description")).trim().to_string();
            }
            if !desc.is_empty() {
                builder.push_str("Schema Description: ");
                builder.push_str(&desc);
                builder.push('\n');
            }
            builder.push_str("JSON Schema:\n");
            builder.push_str(&schema.to_string());
            builder.push_str("\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.");
            builder
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// util
// ---------------------------------------------------------------------------

/// regexp `[^a-zA-Z0-9_-]` → "_" (per rune, as Go's ReplaceAllString does).
fn replace_non_claude_id_chars(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// port of SanitizeClaudeToolID (util/claude_tool_id.go)
fn sanitize_claude_tool_id(id: &str) -> String {
    let s = replace_non_claude_id_chars(id);
    if s.is_empty() {
        let (nanos, counter) = next_id_seed();
        return format!("toolu_{nanos}_{counter}");
    }
    s
}

// port of SanitizeClaudeFunctionName (util/claude_tool_id.go)
fn sanitize_claude_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut s = replace_non_claude_id_chars(name);
    // Every char is ASCII now, so a byte cut is a char cut.
    s.truncate(64);
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// Go's `json.Marshal` of a map sorts its keys.
fn sorted_object(map: Map<String, Value>) -> Value {
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(entries.into_iter().collect())
}

/// `json.Unmarshal(raw, &[]string)`: `null` → empty (no error), an array of
/// strings (a `null` element decodes as ""), anything else → error.
fn unmarshal_string_slice(raw: &Value) -> Option<Vec<String>> {
    match raw {
        Value::Null => Some(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => Some(s.clone()),
                Value::Null => Some(String::new()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

// port of NormalizeClaudeToolInputSchema (util/claude_schema.go)
fn normalize_claude_tool_input_schema(schema: Option<&Value>) -> Value {
    let empty = || json!({"type": "object", "properties": {}});
    let Some(Value::Object(root_in)) = schema else {
        return empty();
    };
    let mut root = root_in.clone();

    let mut properties = claude_schema_object(root.get("properties"));
    for union_name in ["anyOf", "oneOf", "allOf"] {
        let Some(union_raw) = root.shift_remove(union_name) else {
            continue;
        };
        let Value::Array(branches) = union_raw else {
            continue;
        };
        for branch_raw in branches {
            // A `null` branch decodes to a nil map, which adds nothing.
            let Value::Object(branch) = branch_raw else {
                continue;
            };
            if !claude_schema_can_be_object(&branch) {
                continue;
            }
            for (name, property) in claude_schema_object(branch.get("properties")) {
                properties.entry(name).or_insert(property);
            }
            if union_name == "allOf" {
                merge_claude_schema_required(&mut root, branch.get("required"));
            }
        }
    }

    root.insert("type".to_string(), Value::from("object"));
    root.insert("properties".to_string(), sorted_object(properties));
    sorted_object(root)
}

// port of claudeSchemaObject (util/claude_schema.go)
fn claude_schema_object(raw: Option<&Value>) -> Map<String, Value> {
    match raw {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

// port of claudeSchemaCanBeObject (util/claude_schema.go)
fn claude_schema_can_be_object(schema: &Map<String, Value>) -> bool {
    let Some(type_raw) = schema.get("type") else {
        return true;
    };
    match type_raw {
        Value::String(s) => s == "object",
        // `null` decodes into a string as "", which is not "object".
        Value::Null => false,
        other => match unmarshal_string_slice(other) {
            Some(types) => types.iter().any(|t| t == "object"),
            None => false,
        },
    }
}

// port of mergeClaudeSchemaRequired (util/claude_schema.go)
fn merge_claude_schema_required(root: &mut Map<String, Value>, branch_required: Option<&Value>) {
    let mut required: Vec<String> = root
        .get("required")
        .and_then(unmarshal_string_slice)
        .unwrap_or_default();

    // A missing branch `required` is an empty RawMessage, which fails to decode.
    let Some(branch_names) = branch_required.and_then(unmarshal_string_slice) else {
        return;
    };

    let mut seen: HashSet<String> = required.iter().cloned().collect();
    for name in branch_names {
        if seen.contains(&name) {
            continue;
        }
        seen.insert(name.clone());
        required.push(name);
    }
    if required.is_empty() {
        return;
    }
    root.insert("required".to_string(), Value::from(required));
}

// ---------------------------------------------------------------------------
// thinking
// ---------------------------------------------------------------------------

/// The parts of `registry.ThinkingSupport` this pair reads: `(Min, Levels)`.
type ThinkingSupport = (i64, Vec<String>);

/// Stand-in for `registry.LookupModelInfo(model, "claude").Thinking`
/// (internal/registry/model_registry.go). Termory has no model registry.
fn lookup_claude_thinking_support(_model: &str) -> Option<ThinkingSupport> {
    None
}

// port of ConvertLevelToBudget (thinking/convert.go)
fn convert_level_to_budget(level: &str) -> Option<i64> {
    match level.to_lowercase().as_str() {
        "none" => Some(0),
        "auto" => Some(-1),
        "minimal" => Some(512),
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        "xhigh" => Some(32768),
        "max" => Some(128000),
        _ => None,
    }
}

// port of HasLevel (thinking/convert.go)
fn has_level(levels: &[String], target: &str) -> bool {
    levels
        .iter()
        .any(|level| level.trim().eq_ignore_ascii_case(target))
}

// port of MapToClaudeEffort (thinking/convert.go)
fn map_to_claude_effort(level: &str, supports_max: bool) -> Option<&'static str> {
    match level.trim().to_lowercase().as_str() {
        "minimal" => Some("low"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" => Some(if supports_max { "max" } else { "high" }),
        "auto" => Some("high"),
        _ => None,
    }
}

/// port of SummaryMode (thinking/summary.go); `None` is SummaryUnspecified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SummaryMode {
    Disabled,
    Enabled,
}

// port of ApplyTranslatedSummaryToClaude (thinking/summary.go), with
// ExtractTranslatedSummaryConfig fixed to source "openai" / target "claude"
// (→ ExtractExplicitSummaryConfig → extractOpenAIExplicitSummaryConfig) and
// applySummaryConfigForProvider fixed to target "claude".
fn apply_translated_summary_to_claude(out: &mut Value, source: &Value, model: &str) {
    let Some(mode) = extract_openai_explicit_summary_config(source) else {
        return;
    };

    let enabled = mode == SummaryMode::Enabled;
    if enabled && gget(out, "thinking.type").is_none() {
        enable_claude_thinking_for_summary(out, model);
    }
    if !claude_thinking_accepts_display(out) {
        return;
    }
    let value = if enabled { "summarized" } else { "omitted" };
    sj_set(out, "thinking.display", Value::from(value));
}

// port of extractOpenAIExplicitSummaryConfig (thinking/summary.go); only the
// mode is kept — the Detail it also returns is never read on the Claude side.
fn extract_openai_explicit_summary_config(body: &Value) -> Option<SummaryMode> {
    for path in [
        "extra_body.google.thinking_config.include_thoughts",
        "extra_body.google.thinking_config.includeThoughts",
        "extra_body.google.thinkingConfig.include_thoughts",
        "extra_body.google.thinkingConfig.includeThoughts",
        "extra_body.extra_body.google.thinking_config.include_thoughts",
        "extra_body.extra_body.google.thinking_config.includeThoughts",
        "google.thinking_config.include_thoughts",
        "google.thinking_config.includeThoughts",
        "thinking.includeThoughts",
        "thinking.include_thoughts",
        "reasoning.includeThoughts",
        "reasoning.include_thoughts",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
    ] {
        if let Some(mode) = summary_bool_config(body, path) {
            return Some(mode);
        }
    }

    for path in ["reasoning.summary", "reasoning.generate_summary"] {
        if let Some(mode) = responses_summary_config(body, path) {
            return Some(mode);
        }
    }

    if let Some(Value::Bool(exclude)) = gget(body, "reasoning.exclude") {
        return Some(if *exclude {
            SummaryMode::Disabled
        } else {
            SummaryMode::Enabled
        });
    }
    if let Some(Value::Bool(include)) = gget(body, "include_reasoning") {
        return Some(if *include {
            SummaryMode::Enabled
        } else {
            SummaryMode::Disabled
        });
    }
    if let Some(Value::Bool(enabled)) = gget(body, "reasoning.enabled") {
        return Some(if *enabled {
            SummaryMode::Enabled
        } else {
            SummaryMode::Disabled
        });
    }
    None
}

// port of summaryBoolConfig (thinking/summary.go)
fn summary_bool_config(body: &Value, path: &str) -> Option<SummaryMode> {
    match gget(body, path) {
        Some(Value::Bool(true)) => Some(SummaryMode::Enabled),
        Some(Value::Bool(false)) => Some(SummaryMode::Disabled),
        _ => None,
    }
}

// port of responsesSummaryConfig (thinking/summary.go)
fn responses_summary_config(body: &Value, path: &str) -> Option<SummaryMode> {
    match gget(body, path)? {
        Value::Null => Some(SummaryMode::Disabled),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "auto" | "concise" | "detailed" => Some(SummaryMode::Enabled),
            "none" => Some(SummaryMode::Disabled),
            _ => None,
        },
        _ => None,
    }
}

// port of claudeThinkingAcceptsDisplay (thinking/summary.go)
fn claude_thinking_accepts_display(body: &Value) -> bool {
    match gstr(gget(body, "thinking.type"))
        .trim()
        .to_lowercase()
        .as_str()
    {
        "adaptive" => true,
        "enabled" => match gget(body, "thinking.budget_tokens") {
            Some(budget @ Value::Number(_)) => {
                let value = gint(Some(budget));
                value == -1 || value > 0
            }
            _ => true,
        },
        _ => false,
    }
}

// port of enableClaudeThinkingForSummary (thinking/summary.go); the
// ParseSuffix step is out of scope, the registry lookup is the stand-in.
fn enable_claude_thinking_for_summary(body: &mut Value, model: &str) {
    let model_info = if model.is_empty() {
        lookup_claude_thinking_support(&gstr(body.get("model")))
    } else {
        lookup_claude_thinking_support(model)
    };
    let Some((min, levels)) = model_info else {
        return;
    };
    if !levels.is_empty() {
        sj_set(body, "thinking.type", Value::from("adaptive"));
        sj_delete(body, "thinking.budget_tokens");
        return;
    }
    let budget = min;
    if budget <= 0 {
        return;
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        if gint(Some(max_tokens)) <= budget {
            return;
        }
    }
    sj_set(body, "thinking.type", Value::from("enabled"));
    sj_set(body, "thinking.budget_tokens", Value::from(budget));
}

// ---------------------------------------------------------------------------
// Request: OpenAI Chat Completions → Anthropic Messages
// ---------------------------------------------------------------------------

// port of ConvertOpenAIRequestToClaude (claude_openai_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    convert_openai_request_to_claude(model, body, stream, false)
}

// port of ConvertOpenAIRequestToClaudeWithCompat (claude_openai_request.go):
// an assistant's `reasoning_content` is kept as an unsigned thinking block.
// Kept for parity: the router uses the registered (non-compat) form, as
// CLIProxyAPI's Claude executor does.
#[allow(dead_code)]
pub fn translate_request_with_compat(model: &str, body: &Value, stream: bool) -> Value {
    convert_openai_request_to_claude(model, body, stream, true)
}

// port of convertOpenAIRequestToClaude (claude_openai_request.go)
fn convert_openai_request_to_claude(
    model_name: &str,
    root: &Value,
    stream: bool,
    preserve_empty_thinking_blocks: bool,
) -> Value {
    let user_id = derive_claude_user_id(root);

    // Base Claude Code API template with default max_tokens value
    let mut out = json!({"model": "", "max_tokens": 32000, "messages": [], "metadata": {}});
    sj_set(&mut out, "metadata.user_id", Value::from(user_id));

    // Convert OpenAI reasoning_effort to Claude thinking config.
    if let Some(v) = root.get("reasoning_effort") {
        let mut effort = gstr(Some(v)).trim().to_lowercase();
        if !effort.is_empty() {
            let mi = lookup_claude_thinking_support(model_name);
            let supports_adaptive = mi
                .as_ref()
                .map(|(_, levels)| !levels.is_empty())
                .unwrap_or(false);
            let supports_max = supports_adaptive
                && mi
                    .as_ref()
                    .map(|(_, levels)| has_level(levels, "max"))
                    .unwrap_or(false);

            if supports_adaptive {
                match effort.as_str() {
                    "none" => {
                        sj_set(&mut out, "thinking.type", Value::from("disabled"));
                        sj_delete(&mut out, "thinking.budget_tokens");
                        sj_delete(&mut out, "output_config.effort");
                    }
                    "auto" => {
                        sj_set(&mut out, "thinking.type", Value::from("adaptive"));
                        sj_delete(&mut out, "thinking.budget_tokens");
                        sj_delete(&mut out, "output_config.effort");
                    }
                    _ => {
                        if let Some(mapped) = map_to_claude_effort(&effort, supports_max) {
                            effort = mapped.to_string();
                        }
                        sj_set(&mut out, "thinking.type", Value::from("adaptive"));
                        sj_delete(&mut out, "thinking.budget_tokens");
                        sj_set(&mut out, "output_config.effort", Value::from(effort));
                    }
                }
            } else if let Some(budget) = convert_level_to_budget(&effort) {
                // Legacy/manual thinking (budget_tokens).
                match budget {
                    0 => sj_set(&mut out, "thinking.type", Value::from("disabled")),
                    -1 => sj_set(&mut out, "thinking.type", Value::from("enabled")),
                    b if b > 0 => {
                        sj_set(&mut out, "thinking.type", Value::from("enabled"));
                        sj_set(&mut out, "thinking.budget_tokens", Value::from(b));
                    }
                    _ => {}
                }
            }
        }
    }

    // Model mapping to specify which Claude Code model to use
    sj_set(&mut out, "model", Value::from(model_name));

    // Max tokens configuration with fallback to default value; either spelling.
    if let Some(max_tokens) =
        first_existing(&[root.get("max_tokens"), root.get("max_completion_tokens")])
    {
        sj_set(&mut out, "max_tokens", Value::from(gint(Some(max_tokens))));
    }

    // Top P setting for nucleus sampling.
    if let Some(top_p) = root.get("top_p") {
        sj_set(&mut out, "top_p", float_value(gfloat(Some(top_p))));
    }

    // Stop sequences configuration for custom termination conditions
    if let Some(stop) = root.get("stop") {
        if let Value::Array(items) = stop {
            let stop_sequences: Vec<String> = items.iter().map(|v| gstr(Some(v))).collect();
            if !stop_sequences.is_empty() {
                sj_set(&mut out, "stop_sequences", Value::from(stop_sequences));
            }
        } else {
            sj_set(
                &mut out,
                "stop_sequences",
                Value::from(vec![gstr(Some(stop))]),
            );
        }
    }

    // Stream configuration to enable or disable streaming responses
    sj_set(&mut out, "stream", Value::Bool(stream));

    let mut system_blocks: Vec<Value> = Vec::new();
    let mut message_blocks: Vec<Value> = Vec::new();

    // Process messages and transform them to Claude Code format
    if let Some(Value::Array(messages)) = root.get("messages") {
        let mut last_tool_message: HashMap<String, &Value> = HashMap::new();
        for message in messages {
            if gstr(message.get("role")) == "tool" {
                let raw_id = gstr(message.get("tool_call_id"));
                if !raw_id.is_empty() {
                    last_tool_message.insert(raw_id, message);
                }
            }
        }
        let mut emitted_tool_results: HashSet<String> = HashSet::new();

        let mut message_accumulator = ClaudeMessageAccumulator::default();
        for message in messages {
            let role = gstr(message.get("role"));
            let content_result = message.get("content");

            match role.as_str() {
                // Developer messages rank with system messages in OpenAI's
                // instruction hierarchy, so both become top-level system blocks.
                "system" | "developer" => {
                    let system_start = system_blocks.len();
                    match content_result {
                        Some(Value::String(s)) if !s.is_empty() => {
                            let mut text_part = json!({"type": "text", "text": s});
                            attach_cache_control(&mut text_part, message);
                            system_blocks.push(text_part);
                        }
                        Some(Value::Array(parts)) => {
                            for part in parts {
                                if gstr(part.get("type")) == "text" {
                                    let mut text_part =
                                        json!({"type": "text", "text": gstr(part.get("text"))});
                                    attach_cache_control(&mut text_part, part);
                                    system_blocks.push(text_part);
                                }
                            }
                            // Message-level cache_control applies to the last
                            // system block from this message.
                            if message.get("cache_control").is_some()
                                && system_blocks.len() > system_start
                            {
                                if let Some(last) = system_blocks.last_mut() {
                                    if last.get("cache_control").is_none() {
                                        attach_cache_control(last, message);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "user" | "assistant" => {
                    let mut content_blocks: Vec<Value> = Vec::with_capacity(4);
                    if preserve_empty_thinking_blocks && role == "assistant" {
                        if let Some(Value::String(reasoning_content)) =
                            message.get("reasoning_content")
                        {
                            if !reasoning_content.trim().is_empty() {
                                content_blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": reasoning_content,
                                    "signature": ""
                                }));
                            }
                        }
                    }

                    // Handle content based on its type
                    match content_result {
                        Some(Value::String(s)) if !s.is_empty() => {
                            content_blocks.push(json!({"type": "text", "text": s}));
                        }
                        Some(Value::Array(parts)) => {
                            for part in parts {
                                if let Some(claude_part) =
                                    convert_openai_content_part_to_claude_part(part)
                                {
                                    content_blocks.push(claude_part);
                                }
                            }
                        }
                        _ => {}
                    }

                    // Handle tool calls (for assistant messages)
                    if let (Some(Value::Array(tool_calls)), true) =
                        (message.get("tool_calls"), role == "assistant")
                    {
                        for tool_call in tool_calls {
                            if gstr(tool_call.get("type")) != "function" {
                                continue;
                            }
                            let mut tool_call_id = gstr(tool_call.get("id"));
                            if tool_call_id.is_empty() {
                                tool_call_id = generate_claude_tool_call_id();
                            }
                            let tool_call_id = sanitize_claude_tool_id(&tool_call_id);

                            let function = tool_call.get("function");
                            let name = gstr(function.and_then(|f| f.get("name")));

                            // Parse arguments for the tool call
                            let input = match function.and_then(|f| f.get("arguments")) {
                                Some(args) => {
                                    let args_str = gstr(Some(args));
                                    match serde_json::from_str::<Value>(&args_str) {
                                        Ok(parsed @ Value::Object(_)) if !args_str.is_empty() => {
                                            parsed
                                        }
                                        _ => json!({}),
                                    }
                                }
                                None => json!({}),
                            };

                            content_blocks.push(json!({
                                "type": "tool_use",
                                "id": tool_call_id,
                                "name": sanitize_claude_function_name(&name),
                                "input": input
                            }));
                        }
                    }

                    let mut msg = json!({"role": role, "content": content_blocks});
                    attach_message_cache_control(&mut msg, message);
                    message_accumulator.append(msg);
                }
                "tool" => {
                    // Handle tool result messages conversion
                    let raw_id = gstr(message.get("tool_call_id"));
                    let tool_call_id = sanitize_claude_tool_id(&raw_id);
                    if !raw_id.is_empty() {
                        if emitted_tool_results.contains(&raw_id) {
                            continue;
                        }
                        emitted_tool_results.insert(raw_id.clone());
                    }

                    let mut target_msg = message;
                    if !raw_id.is_empty() {
                        if let Some(last_msg) = last_tool_message.get(&raw_id) {
                            target_msg = last_msg;
                        }
                    }
                    let tool_content =
                        convert_openai_tool_result_content(target_msg.get("content"));

                    let mut msg = json!({
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": tool_call_id, "content": tool_content}]
                    });
                    // Anthropic rejects cache_control inside tool_result.content,
                    // so part-level or message-level cache_control is hoisted
                    // onto the tool_result block itself.
                    attach_tool_message_cache_control(&mut msg, target_msg);
                    message_accumulator.append(msg);
                }
                _ => {}
            }
        }

        message_blocks = message_accumulator.into_messages();
    }

    let format_instruction =
        build_claude_structured_output_instruction(root.get("response_format"));
    if !format_instruction.is_empty() {
        system_blocks.push(json!({"type": "text", "text": format_instruction}));
    }

    // Preserve a minimal conversational turn for system-only inputs.
    if message_blocks.is_empty() && !system_blocks.is_empty() {
        message_blocks.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }

    if !system_blocks.is_empty() {
        sj_set(&mut out, "system", Value::Array(system_blocks));
    }
    if !message_blocks.is_empty() {
        sj_set(&mut out, "messages", Value::Array(message_blocks));
    }

    // Tools mapping: OpenAI tools -> Claude Code tools
    let mut allowed_tool_names: HashSet<String> = HashSet::new();
    let mut is_allowed_tools = false;
    let mut allowed_mode = "auto".to_string();
    if let Some(tool_choice @ Value::Object(_)) = root.get("tool_choice") {
        if gstr(tool_choice.get("type")) == "allowed_tools" {
            is_allowed_tools = true;
            let mut tool_list = garray(gget(tool_choice, "allowed_tools.tools"));
            if tool_list.is_empty() {
                tool_list = garray(tool_choice.get("tools"));
            }
            for t in tool_list {
                let mut fn_name = gstr(gget(t, "function.name")).trim().to_string();
                if fn_name.is_empty() {
                    fn_name = gstr(t.get("name")).trim().to_string();
                }
                if !fn_name.is_empty() {
                    allowed_tool_names.insert(sanitize_claude_function_name(&fn_name));
                    allowed_tool_names.insert(fn_name);
                }
            }
            let mut mode_val = gstr(gget(tool_choice, "allowed_tools.mode"))
                .trim()
                .to_lowercase();
            if mode_val.is_empty() {
                mode_val = gstr(tool_choice.get("mode")).trim().to_lowercase();
            }
            if !mode_val.is_empty() {
                allowed_mode = mode_val;
            }
        }
    }

    let mut anthropic_tools: Vec<Value> = Vec::new();
    if let Some(Value::Array(tools)) = root.get("tools") {
        if !tools.is_empty() {
            for tool in tools {
                if gstr(tool.get("type")) != "function" {
                    continue;
                }
                let empty = Value::Null;
                let function = tool.get("function").unwrap_or(&empty);
                let fn_name = gstr(function.get("name"));
                let sanitized_fn_name = sanitize_claude_function_name(&fn_name);
                if is_allowed_tools
                    && !allowed_tool_names.contains(&fn_name)
                    && !allowed_tool_names.contains(&sanitized_fn_name)
                {
                    continue;
                }
                let mut anthropic_tool = json!({
                    "name": sanitized_fn_name,
                    "description": gstr(function.get("description"))
                });

                // Convert parameters schema for the tool
                let input_schema = if let Some(parameters) = function.get("parameters") {
                    normalize_claude_tool_input_schema(Some(parameters))
                } else if let Some(parameters) = function.get("parametersJsonSchema") {
                    normalize_claude_tool_input_schema(Some(parameters))
                } else {
                    normalize_claude_tool_input_schema(None)
                };
                sj_set(&mut anthropic_tool, "input_schema", input_schema);
                attach_cache_control(&mut anthropic_tool, tool);
                if anthropic_tool.get("cache_control").is_none() {
                    attach_cache_control(&mut anthropic_tool, function);
                }
                let strict = function.get("strict").or_else(|| tool.get("strict"));
                if let Some(Value::Bool(b)) = strict {
                    sj_set(&mut anthropic_tool, "strict", Value::Bool(*b));
                }

                anthropic_tools.push(anthropic_tool);
            }

            if !anthropic_tools.is_empty() {
                sj_set(&mut out, "tools", Value::Array(anthropic_tools.clone()));
            } else {
                sj_delete(&mut out, "tools");
            }
        }
    }

    // Tool choice mapping from OpenAI format to Claude Code format
    if is_allowed_tools {
        let choice = if anthropic_tools.is_empty() {
            json!({"type": "none"})
        } else if allowed_mode == "required" {
            json!({"type": "any"})
        } else {
            json!({"type": "auto"})
        };
        sj_set(&mut out, "tool_choice", choice);
    } else if let Some(tool_choice) = root.get("tool_choice") {
        match tool_choice {
            Value::String(choice) => match choice.as_str() {
                "none" => sj_set(&mut out, "tool_choice", json!({"type": "none"})),
                "auto" => sj_set(&mut out, "tool_choice", json!({"type": "auto"})),
                "required" => sj_set(&mut out, "tool_choice", json!({"type": "any"})),
                _ => {}
            },
            Value::Object(_) | Value::Array(_) => match gstr(tool_choice.get("type")).as_str() {
                "none" => sj_set(&mut out, "tool_choice", json!({"type": "none"})),
                "auto" => sj_set(&mut out, "tool_choice", json!({"type": "auto"})),
                "required" | "any" => sj_set(&mut out, "tool_choice", json!({"type": "any"})),
                "function" => {
                    let mut function_name = gstr(gget(tool_choice, "function.name"));
                    if function_name.is_empty() {
                        function_name = gstr(tool_choice.get("name"));
                    }
                    if !function_name.is_empty() {
                        sj_set(
                            &mut out,
                            "tool_choice",
                            json!({"type": "tool", "name": sanitize_claude_function_name(&function_name)}),
                        );
                    } else {
                        sj_set(&mut out, "tool_choice", json!({"type": "none"}));
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    if let Some(Value::Bool(false)) = root.get("parallel_tool_calls") {
        if out.get("tool_choice").is_some() {
            if gstr(gget(&out, "tool_choice.type")) != "none" {
                sj_set(
                    &mut out,
                    "tool_choice.disable_parallel_tool_use",
                    Value::Bool(true),
                );
            }
        } else if out.get("tools").is_some() {
            sj_set(
                &mut out,
                "tool_choice",
                json!({"type": "auto", "disable_parallel_tool_use": true}),
            );
        }
    }

    apply_translated_summary_to_claude(&mut out, root, model_name);
    out
}

// port of convertOpenAIContentPartToClaudePartRaw (claude_openai_request.go)
fn convert_openai_content_part_to_claude_part_raw(part: &Value) -> Option<Value> {
    match gstr(part.get("type")).as_str() {
        "text" => Some(json!({"type": "text", "text": gstr(part.get("text"))})),
        "image_url" => convert_openai_image_url_to_claude_part(&gstr(gget(part, "image_url.url"))),
        "file" => {
            let file_data = gstr(gget(part, "file.file_data"));
            if file_data.starts_with("data:") {
                let semicolon_idx = file_data.find(';');
                let comma_idx = file_data.find(',');
                if let (Some(semi), Some(comma)) = (semicolon_idx, comma_idx) {
                    if comma > semi {
                        let media_type = file_data[..semi]
                            .strip_prefix("data:")
                            .unwrap_or(&file_data[..semi]);
                        let data = &file_data[comma + 1..];
                        return Some(json!({
                            "type": "document",
                            "source": {"type": "base64", "media_type": media_type, "data": data}
                        }));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// port of convertOpenAIContentPartToClaudePart (claude_openai_request.go)
fn convert_openai_content_part_to_claude_part(part: &Value) -> Option<Value> {
    let mut claude_part = convert_openai_content_part_to_claude_part_raw(part)?;
    attach_cache_control(&mut claude_part, part);
    Some(claude_part)
}

// port of convertOpenAIImageURLToClaudePart (claude_openai_request.go)
fn convert_openai_image_url_to_claude_part(image_url: &str) -> Option<Value> {
    if image_url.is_empty() {
        return None;
    }

    if image_url.starts_with("data:") {
        let (header, data) = image_url.split_once(',')?;
        let media_type_part = header.split(';').next().unwrap_or("");
        let mut media_type = media_type_part
            .strip_prefix("data:")
            .unwrap_or(media_type_part);
        if media_type.is_empty() {
            media_type = "application/octet-stream";
        }
        return Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data}
        }));
    }

    Some(json!({"type": "image", "source": {"type": "url", "url": image_url}}))
}

// port of convertOpenAIToolResultContent (claude_openai_request.go). Go
// returns `(text, isRaw)`; here the raw form is the parsed array and the
// text form a JSON string, which is what the caller writes either way.
fn convert_openai_tool_result_content(content: Option<&Value>) -> Value {
    let Some(content) = content else {
        return Value::from("");
    };

    match content {
        Value::String(s) => Value::from(s.clone()),
        Value::Array(items) => {
            let mut claude_parts: Vec<Value> = Vec::with_capacity(4);
            for part in items {
                if let Value::String(s) = part {
                    claude_parts.push(json!({"type": "text", "text": s}));
                    continue;
                }
                if let Some(claude_part) = convert_openai_content_part_to_claude_part_raw(part) {
                    claude_parts.push(claude_part);
                }
            }
            if !claude_parts.is_empty() || items.is_empty() {
                return Value::Array(claude_parts);
            }
            Value::from(content.to_string())
        }
        Value::Object(_) => match convert_openai_content_part_to_claude_part_raw(content) {
            Some(claude_part) => Value::Array(vec![claude_part]),
            None => Value::from(content.to_string()),
        },
        other => Value::from(other.to_string()),
    }
}

// port of firstExisting (claude_openai_request.go)
fn first_existing<'a>(values: &[Option<&'a Value>]) -> Option<&'a Value> {
    values.iter().find_map(|v| *v)
}

// ---------------------------------------------------------------------------
// Response: Anthropic Messages → OpenAI Chat Completions
// ---------------------------------------------------------------------------

// port of claudeUsageTokens (claude_openai_response.go)
#[derive(Default, Clone, Copy)]
struct ClaudeUsageTokens {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    has_usage: bool,
}

impl ClaudeUsageTokens {
    // port of claudeUsageTokens.Merge (claude_openai_response.go)
    fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        self.has_usage = true;
        if let Some(v) = usage.get("input_tokens") {
            self.input_tokens = gint(Some(v));
        }
        if let Some(v) = usage.get("output_tokens") {
            self.output_tokens = gint(Some(v));
        }
        if let Some(v) = usage.get("cache_creation_input_tokens") {
            self.cache_creation_input_tokens = gint(Some(v));
        }
        if let Some(v) = usage.get("cache_read_input_tokens") {
            self.cache_read_input_tokens = gint(Some(v));
        }
    }

    // port of claudeUsageTokens.OpenAIUsage (claude_openai_response.go):
    // (prompt, completion, total, cached, cachedCreation).
    fn openai_usage(&self) -> (i64, i64, i64, i64, i64) {
        let cached_tokens = self.cache_read_input_tokens;
        let cached_creation_tokens = self.cache_creation_input_tokens;
        let prompt_tokens = self.input_tokens + cached_creation_tokens + cached_tokens;
        let completion_tokens = self.output_tokens;
        let total_tokens = prompt_tokens + completion_tokens;
        (
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
            cached_creation_tokens,
        )
    }

    /// The six `usage.*` sjson writes every response site repeats.
    fn write_into(&self, target: &mut Value) {
        let (prompt, completion, total, cached, cached_creation) = self.openai_usage();
        sj_set(target, "usage.prompt_tokens", Value::from(prompt));
        sj_set(target, "usage.completion_tokens", Value::from(completion));
        sj_set(target, "usage.total_tokens", Value::from(total));
        sj_set(
            target,
            "usage.prompt_tokens_details.cached_tokens",
            Value::from(cached),
        );
        sj_set(
            target,
            "usage.prompt_tokens_details.cached_creation_tokens",
            Value::from(cached_creation),
        );
        sj_set(
            target,
            "usage.prompt_tokens_details.cache_write_tokens",
            Value::from(cached_creation),
        );
    }
}

// port of ToolCallAccumulator (claude_openai_response.go)
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    index: i64,
    arguments: String,
}

/// port of ConvertAnthropicResponseToOpenAIParams (claude_openai_response.go),
/// plus the model name Go passes per call and a guard for the `[DONE]` frame.
pub struct StreamTranslator {
    model: String,
    created_at: i64,
    response_id: String,
    finish_reason: String,
    usage: ClaudeUsageTokens,
    trailing_usage_sent: bool,
    tool_calls_accumulator: HashMap<i64, ToolCallAccumulator>,
    next_tool_call_index: i64,
    done_sent: bool,
}

impl StreamTranslator {
    // port of the param initialisation in ConvertClaudeResponseToOpenAI; the
    // model name is the client's requested model.
    pub fn new(original_request: &Value) -> Self {
        Self {
            model: gstr(original_request.get("model")),
            created_at: 0,
            response_id: String::new(),
            finish_reason: String::new(),
            usage: ClaudeUsageTokens::default(),
            trailing_usage_sent: false,
            tool_calls_accumulator: HashMap::new(),
            next_tool_call_index: 0,
            done_sent: false,
        }
    }

    /// One upstream Anthropic SSE event (event = its 'event:' name, data =
    /// parsed JSON). Returns complete client frames "data: <json>\n\n".
    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert(data)
            .into_iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect()
    }

    /// Upstream ended: the OpenAI stream terminator, exactly once.
    pub fn finish(&mut self) -> Vec<String> {
        if self.done_sent {
            return Vec::new();
        }
        self.done_sent = true;
        vec!["data: [DONE]\n\n".to_string()]
    }

    // port of ConvertClaudeResponseToOpenAI (claude_openai_response.go)
    fn convert(&mut self, root: &Value) -> Vec<Value> {
        let event_type = gstr(root.get("type"));

        // Base OpenAI streaming response template
        let mut template = json!({
            "id": "",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "",
            "choices": [{"index": 0, "delta": {}, "finish_reason": null}]
        });

        if !self.model.is_empty() {
            template["model"] = Value::from(self.model.clone());
        }
        if !self.response_id.is_empty() {
            template["id"] = Value::from(self.response_id.clone());
        }
        if self.created_at > 0 {
            template["created"] = Value::from(self.created_at);
        }

        match event_type.as_str() {
            "message_start" => {
                // Initialize response with message metadata when a new message begins
                if let Some(message) = root.get("message") {
                    self.response_id = gstr(message.get("id"));
                    self.created_at = now_unix();

                    template["id"] = Value::from(self.response_id.clone());
                    template["model"] = Value::from(self.model.clone());
                    template["created"] = Value::from(self.created_at);

                    // Set initial role to assistant for the response
                    set_choice(&mut template, "delta.role", Value::from("assistant"));

                    self.next_tool_call_index = 0;
                    self.usage.merge(message.get("usage"));
                }
                vec![template]
            }

            "content_block_start" => {
                // Start of a content block (text, tool use, or reasoning)
                if let Some(content_block) = root.get("content_block") {
                    if gstr(content_block.get("type")) == "tool_use" {
                        // Start of tool call - initialize accumulator to track arguments
                        let index = gint(root.get("index"));
                        let tool_call_index = self.next_tool_call_index;
                        self.next_tool_call_index += 1;
                        self.tool_calls_accumulator.insert(
                            index,
                            ToolCallAccumulator {
                                id: gstr(content_block.get("id")),
                                name: gstr(content_block.get("name")),
                                index: tool_call_index,
                                arguments: String::new(),
                            },
                        );
                    }
                }
                // Don't output anything yet - wait for complete tool call
                Vec::new()
            }

            "content_block_delta" => {
                // Handle content delta (text, tool use arguments, or reasoning content)
                let mut has_content = false;
                if let Some(delta) = root.get("delta") {
                    match gstr(delta.get("type")).as_str() {
                        "text_delta" => {
                            if let Some(text) = delta.get("text") {
                                set_choice(
                                    &mut template,
                                    "delta.content",
                                    Value::from(gstr(Some(text))),
                                );
                                has_content = true;
                            }
                        }
                        "thinking_delta" => {
                            if let Some(thinking) = delta.get("thinking") {
                                set_choice(
                                    &mut template,
                                    "delta.reasoning_content",
                                    Value::from(gstr(Some(thinking))),
                                );
                                has_content = true;
                            }
                        }
                        "input_json_delta" => {
                            // Tool use input delta - accumulate arguments for tool calls
                            if let Some(partial_json) = delta.get("partial_json") {
                                let index = gint(root.get("index"));
                                if let Some(accumulator) =
                                    self.tool_calls_accumulator.get_mut(&index)
                                {
                                    accumulator.arguments.push_str(&gstr(Some(partial_json)));
                                }
                            }
                            // Don't output anything yet - wait for complete tool call
                            return Vec::new();
                        }
                        _ => {}
                    }
                }
                if has_content {
                    vec![template]
                } else {
                    Vec::new()
                }
            }

            "content_block_stop" => {
                // End of content block - output complete tool call if it's a tool_use block
                let index = gint(root.get("index"));
                if let Some(accumulator) = self.tool_calls_accumulator.remove(&index) {
                    // Build complete tool call with accumulated arguments
                    let arguments = if accumulator.arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        accumulator.arguments
                    };
                    set_choice(
                        &mut template,
                        "delta.tool_calls",
                        json!([{
                            "index": accumulator.index,
                            "id": accumulator.id,
                            "type": "function",
                            "function": {"name": accumulator.name, "arguments": arguments}
                        }]),
                    );
                    return vec![template];
                }
                Vec::new()
            }

            "message_delta" => {
                // Handle message-level changes including stop reason and usage
                if let Some(stop_reason) = gget(root, "delta.stop_reason") {
                    self.finish_reason =
                        map_anthropic_stop_reason_to_openai(&gstr(Some(stop_reason))).to_string();
                    set_choice(
                        &mut template,
                        "finish_reason",
                        Value::from(self.finish_reason.clone()),
                    );
                }

                // Handle usage information for token counts
                if let Some(usage) = root.get("usage") {
                    self.usage.merge(Some(usage));
                    self.usage.write_into(&mut template);
                }
                vec![template]
            }

            "message_stop" => {
                // Final message event - emit the standard OpenAI trailing usage
                // chunk with an empty choices array if usage was tracked.
                if self.usage.has_usage && !self.trailing_usage_sent {
                    self.trailing_usage_sent = true;
                    let mut usage_template = json!({
                        "id": "",
                        "object": "chat.completion.chunk",
                        "created": 0,
                        "model": "",
                        "choices": []
                    });
                    if !self.response_id.is_empty() {
                        usage_template["id"] = Value::from(self.response_id.clone());
                    }
                    if !self.model.is_empty() {
                        usage_template["model"] = Value::from(self.model.clone());
                    }
                    if self.created_at > 0 {
                        usage_template["created"] = Value::from(self.created_at);
                    }
                    self.usage.write_into(&mut usage_template);
                    return vec![usage_template];
                }
                Vec::new()
            }

            // Ping events for keeping connection alive - no output needed
            "ping" => Vec::new(),

            "error" => {
                // Error event - format and return error response
                if let Some(error_data) = root.get("error") {
                    return vec![json!({
                        "error": {
                            "message": gstr(error_data.get("message")),
                            "type": gstr(error_data.get("type"))
                        }
                    })];
                }
                Vec::new()
            }

            // Unknown event type - ignore
            _ => Vec::new(),
        }
    }
}

// port of mapAnthropicStopReasonToOpenAI (claude_openai_response.go)
fn map_anthropic_stop_reason_to_openai(anthropic_reason: &str) -> &'static str {
    match anthropic_reason {
        "end_turn" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "refusal" | "sensitive" => "content_filter",
        _ => "stop",
    }
}

/// Complete upstream response → OpenAI chat.completion.
///
/// Go's ConvertClaudeResponseToOpenAINonStream reads the upstream body as SSE
/// text. A string body is read exactly like that; an array is taken as the
/// already-parsed event list; an object (a Messages response) is first
/// replayed as the event sequence Anthropic would have streamed for it.
pub fn translate_non_stream(upstream: &Value, _original_request: &Value) -> Value {
    let events: Vec<Value> = match upstream {
        Value::String(text) => sse_data_events(text),
        Value::Array(events) => events.clone(),
        Value::Object(_) => message_to_events(upstream),
        _ => Vec::new(),
    };
    convert_claude_response_to_openai_non_stream(&events)
}

/// The SSE line split at the top of ConvertClaudeResponseToOpenAINonStream
/// (claude_openai_response.go): every `data:` line, trimmed. A payload that is
/// not JSON parses as nothing in gjson (no `type`) and is dropped here.
fn sse_data_events(text: &str) -> Vec<Value> {
    text.split('\n')
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

/// The stream events Anthropic emits for a complete Messages response — not a
/// port; it adapts the router's non-stream body to the Go event loop.
fn message_to_events(message: &Value) -> Vec<Value> {
    if gstr(message.get("type")) == "error" {
        return vec![message.clone()];
    }
    let mut start = Map::new();
    for key in ["id", "model"] {
        if let Some(v) = message.get(key) {
            start.insert(key.to_string(), v.clone());
        }
    }
    let mut events = vec![json!({"type": "message_start", "message": start})];
    if let Some(Value::Array(blocks)) = message.get("content") {
        for (index, block) in blocks.iter().enumerate() {
            let mut deltas: Vec<Value> = Vec::new();
            let start_block = match gstr(block.get("type")).as_str() {
                "text" => {
                    deltas.push(json!({"type": "text_delta", "text": gstr(block.get("text"))}));
                    json!({"type": "text", "text": ""})
                }
                "thinking" => {
                    deltas.push(
                        json!({"type": "thinking_delta", "thinking": gstr(block.get("thinking"))}),
                    );
                    json!({"type": "thinking", "thinking": ""})
                }
                "tool_use" => {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    deltas.push(
                        json!({"type": "input_json_delta", "partial_json": input.to_string()}),
                    );
                    json!({
                        "type": "tool_use",
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "name": block.get("name").cloned().unwrap_or(Value::Null),
                        "input": {}
                    })
                }
                _ => block.clone(),
            };
            events.push(
                json!({"type": "content_block_start", "index": index, "content_block": start_block}),
            );
            for delta in deltas {
                events.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
            }
            events.push(json!({"type": "content_block_stop", "index": index}));
        }
    }
    let mut message_delta = json!({
        "type": "message_delta",
        "delta": {"stop_reason": message.get("stop_reason").cloned().unwrap_or(Value::Null)}
    });
    if let Some(usage) = message.get("usage") {
        message_delta["usage"] = usage.clone();
    }
    events.push(message_delta);
    events.push(json!({"type": "message_stop"}));
    events
}

// port of ConvertClaudeResponseToOpenAINonStream (claude_openai_response.go),
// from the point where the SSE body has been split into events.
fn convert_claude_response_to_openai_non_stream(events: &[Value]) -> Value {
    // Base OpenAI non-streaming response template
    let mut out = json!({
        "id": "",
        "object": "chat.completion",
        "created": 0,
        "model": "",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": ""}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    });

    let mut message_id = String::new();
    let mut model = String::new();
    let mut created_at: i64 = 0;
    let mut stop_reason = String::new();
    let mut content_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut usage_tokens = ClaudeUsageTokens::default();
    let mut tool_calls_accumulator: HashMap<i64, ToolCallAccumulator> = HashMap::new();

    for root in events {
        match gstr(root.get("type")).as_str() {
            "message_start" => {
                // Extract initial message metadata including ID, model, and input token count
                if let Some(message) = root.get("message") {
                    message_id = gstr(message.get("id"));
                    model = gstr(message.get("model"));
                    created_at = now_unix();
                    usage_tokens.merge(message.get("usage"));
                }
            }

            "content_block_start" => {
                // Initialize tool call accumulator for this index
                if let Some(content_block) = root.get("content_block") {
                    if gstr(content_block.get("type")) == "tool_use" {
                        let index = gint(root.get("index"));
                        tool_calls_accumulator.insert(
                            index,
                            ToolCallAccumulator {
                                id: gstr(content_block.get("id")),
                                name: gstr(content_block.get("name")),
                                ..Default::default()
                            },
                        );
                    }
                }
            }

            "content_block_delta" => {
                // Process incremental content updates
                if let Some(delta) = root.get("delta") {
                    match gstr(delta.get("type")).as_str() {
                        "text_delta" => {
                            if let Some(text) = delta.get("text") {
                                content_parts.push(gstr(Some(text)));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(thinking) = delta.get("thinking") {
                                reasoning_parts.push(gstr(Some(thinking)));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(partial_json) = delta.get("partial_json") {
                                let index = gint(root.get("index"));
                                if let Some(accumulator) = tool_calls_accumulator.get_mut(&index) {
                                    accumulator.arguments.push_str(&gstr(Some(partial_json)));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            "content_block_stop" => {
                // Finalize tool call arguments for this index when content block ends
                let index = gint(root.get("index"));
                if let Some(accumulator) = tool_calls_accumulator.get_mut(&index) {
                    if accumulator.arguments.is_empty() {
                        accumulator.arguments.push_str("{}");
                    }
                }
            }

            "message_delta" => {
                // Extract stop reason and output token count when message ends
                if let Some(sr) = gget(root, "delta.stop_reason") {
                    stop_reason = gstr(Some(sr));
                }
                if let Some(usage) = root.get("usage") {
                    usage_tokens.merge(Some(usage));
                }
            }

            _ => {}
        }
    }

    if usage_tokens.has_usage {
        usage_tokens.write_into(&mut out);
    }

    // Set basic response fields including message ID, creation time, and model
    out["id"] = Value::from(message_id);
    out["created"] = Value::from(created_at);
    out["model"] = Value::from(model);

    // Set message content by combining all text parts
    set_choice(
        &mut out,
        "message.content",
        Value::from(content_parts.concat()),
    );

    // Add reasoning content if available (following OpenAI reasoning format)
    if !reasoning_parts.is_empty() {
        set_choice(
            &mut out,
            "message.reasoning_content",
            Value::from(reasoning_parts.concat()),
        );
    }

    // Set tool calls if any were accumulated during processing
    let mut tool_calls: Vec<Value> = Vec::new();
    if !tool_calls_accumulator.is_empty() {
        let max_index = tool_calls_accumulator.keys().copied().max().unwrap_or(-1);
        for i in 0..=max_index {
            let Some(accumulator) = tool_calls_accumulator.get(&i) else {
                continue;
            };
            tool_calls.push(json!({
                "id": accumulator.id,
                "type": "function",
                "function": {"name": accumulator.name, "arguments": accumulator.arguments}
            }));
        }
    }
    if !tool_calls.is_empty() {
        set_choice(&mut out, "message.tool_calls", Value::Array(tool_calls));
        set_choice(&mut out, "finish_reason", Value::from("tool_calls"));
    } else {
        let finish_reason = map_anthropic_stop_reason_to_openai(&stop_reason);
        if finish_reason != "stop" {
            set_choice(&mut out, "finish_reason", Value::from(finish_reason));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str, raw: &str) -> Value {
        translate_request(model, &serde_json::from_str(raw).unwrap(), false)
    }

    fn user_id(content: &str) -> String {
        sha256_hex(&format!("content:{content}"))
    }

    /// Feeds events through one StreamTranslator; every frame must be a
    /// complete `data: <json>\n\n`. A non-zero `created` is replaced by 1 so
    /// the clock-stamped value can be compared exactly.
    fn stream(model: &str, events: &[Value]) -> Vec<Value> {
        let mut t = StreamTranslator::new(&json!({"model": model}));
        let mut out = Vec::new();
        for ev in events {
            for frame in t.push(None, ev) {
                let body = frame
                    .strip_prefix("data: ")
                    .and_then(|f| f.strip_suffix("\n\n"))
                    .expect("complete data frame");
                let mut v: Value = serde_json::from_str(body).unwrap();
                if let Some(c) = v.get_mut("created") {
                    if c.as_i64().unwrap_or(0) > 0 {
                        *c = json!(1);
                    }
                }
                out.push(v);
            }
        }
        out
    }

    fn sse(events: &[Value]) -> Value {
        Value::from(
            events
                .iter()
                .map(|e| format!("data: {e}\n"))
                .collect::<String>(),
        )
    }

    fn non_stream(events: &[Value]) -> Value {
        let mut out = translate_non_stream(&sse(events), &json!({}));
        if out["created"].as_i64().unwrap_or(0) > 0 {
            out["created"] = json!(1);
        }
        out
    }

    // port of TestConvertOpenAIRequestToClaude_ThinkingSummaryVisibility (claude_openai_request_test.go)
    #[test]
    fn thinking_summary_visibility() {
        let cases = [
            (
                r#"{"reasoning_effort":"high","messages":[{"role":"user","content":"hi"}]}"#,
                json!({"type": "enabled", "budget_tokens": 24576}),
            ),
            (
                r#"{"reasoning_effort":"high","include_reasoning":true,"messages":[{"role":"user","content":"hi"}]}"#,
                json!({"type": "enabled", "budget_tokens": 24576, "display": "summarized"}),
            ),
            (
                r#"{"reasoning_effort":"high","reasoning":{"exclude":true},"messages":[{"role":"user","content":"hi"}]}"#,
                json!({"type": "enabled", "budget_tokens": 24576, "display": "omitted"}),
            ),
        ];
        for (input, thinking) in cases {
            assert_eq!(
                req("claude-opus-5-5", input),
                json!({
                    "model": "claude-opus-5-5",
                    "max_tokens": 32000,
                    "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                    "metadata": {"user_id": user_id("hi")},
                    "thinking": thinking,
                    "stream": false
                })
            );
        }
    }

    #[test]
    fn reasoning_effort_budget_mapping() {
        for (effort, thinking) in [
            ("none", json!({"type": "disabled"})),
            ("auto", json!({"type": "enabled"})),
            ("LOW", json!({"type": "enabled", "budget_tokens": 1024})),
            ("xhigh", json!({"type": "enabled", "budget_tokens": 32768})),
        ] {
            let out = translate_request(
                "claude-test",
                &json!({"reasoning_effort": effort, "messages": [{"role": "user", "content": "hi"}]}),
                false,
            );
            assert_eq!(out["thinking"], thinking, "effort {effort}");
        }
        let out = translate_request(
            "claude-test",
            &json!({"reasoning_effort": "bogus", "messages": [{"role": "user", "content": "hi"}]}),
            false,
        );
        assert_eq!(out.get("thinking"), None);
    }

    // port of TestConvertOpenAIRequestToClaudeWithCompatPreservesReasoningContent (claude_openai_compat_test.go)
    #[test]
    fn compat_preserves_reasoning_content() {
        let payload = json!({"messages": [{"role": "assistant", "content": "answer", "reasoning_content": "reason"}]});

        let without = translate_request("deepseek-v4", &payload, false);
        assert_eq!(
            without["messages"],
            json!([{"role": "assistant", "content": [{"type": "text", "text": "answer"}]}])
        );

        let with = translate_request_with_compat("deepseek-v4", &payload, false);
        assert_eq!(
            with["messages"],
            json!([{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reason", "signature": ""},
                {"type": "text", "text": "answer"}
            ]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaudeWithCompat_GroupsAssistantThinkingTextAndTools (claude_openai_request_test.go)
    #[test]
    fn compat_groups_assistant_thinking_text_and_tools() {
        let input = json!({"messages": [
            {"role": "assistant", "reasoning_content": "reason", "content": "answer"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "first", "arguments": "{}"}},
                {"id": "call_2", "type": "function", "function": {"name": "second", "arguments": "{}"}}
            ]}
        ]});
        let out = translate_request_with_compat("claude-test", &input, false);
        assert_eq!(
            out["messages"],
            json!([{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reason", "signature": ""},
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "id": "call_1", "name": "first", "input": {}},
                {"type": "tool_use", "id": "call_2", "name": "second", "input": {}}
            ]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_MergesToolResultWithAdjacentUserContent (claude_openai_request_test.go)
    #[test]
    fn merges_tool_result_with_adjacent_user_content() {
        let out = req(
            "claude-test",
            r#"{"messages":[
                {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"work","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"ok"},
                {"role":"user","content":"continue"}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "work", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"},
                    {"type": "text", "text": "continue"}
                ]}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_SystemDoesNotBreakUserTurnAndCacheBoundary (claude_openai_request_test.go)
    #[test]
    fn system_does_not_break_user_turn_and_cache_boundary() {
        let out = req(
            "claude-test",
            r#"{"messages":[
                {"role":"user","content":"first","cache_control":{"type":"ephemeral"}},
                {"role":"system","content":"system rule"},
                {"role":"user","content":"second"}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "first", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "second"}
            ]}])
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": "system rule"}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_SanitizesToolCallIDsForClaude (claude_openai_request_test.go)
    #[test]
    fn sanitizes_tool_call_ids_for_claude() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","messages":[
                {"role":"assistant","tool_calls":[{"id":"call.with space:1","type":"function","function":{"name":"Read","arguments":"{\"path\":\"README.md\"}"}}]},
                {"role":"tool","tool_call_id":"call.with space:1","content":"ok"}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_with_space_1", "name": "Read", "input": {"path": "README.md"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_with_space_1", "content": "ok"}]}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_GroupsConsecutiveParallelToolResults (claude_openai_request_test.go)
    #[test]
    fn groups_consecutive_parallel_tool_results() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","messages":[
                {"role":"user","content":"Use both tools."},
                {"role":"assistant","content":"","tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"tool_a","arguments":"{}"}},
                    {"id":"call_2","type":"function","function":{"name":"tool_b","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_1","content":"one","cache_control":{"type":"ephemeral"}},
                {"role":"tool","tool_call_id":"call_2","content":"two"},
                {"role":"assistant","content":"Done."}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "Use both tools."}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "tool_a", "input": {}},
                    {"type": "tool_use", "id": "call_2", "name": "tool_b", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one", "cache_control": {"type": "ephemeral"}},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "two"}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_DropsTemperature (claude_openai_request_test.go)
    #[test]
    fn drops_temperature() {
        let out = req(
            "claude-sonnet-5",
            r#"{"model":"gpt-4.1","temperature":0.2,"top_p":0.8,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(
            out,
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 32000,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "metadata": {"user_id": user_id("hi")},
                "top_p": 0.8,
                "stream": false
            })
        );
    }

    #[test]
    fn stop_sequences_and_stream_flag() {
        let out = translate_request(
            "claude-test",
            &json!({"stop": ["a", "b"], "messages": [{"role": "user", "content": "hi"}]}),
            true,
        );
        assert_eq!(out["stop_sequences"], json!(["a", "b"]));
        assert_eq!(out["stream"], json!(true));
        let out = req(
            "claude-test",
            r#"{"stop":"END","top_p":1,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(out["stop_sequences"], json!(["END"]));
        assert_eq!(out["top_p"], json!(1));
        let out = req(
            "claude-test",
            r#"{"stop":[],"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(out.get("stop_sequences"), None);
    }

    // port of TestConvertOpenAIRequestToClaude_ToolResultTextAndBase64Image (claude_openai_request_test.go)
    #[test]
    fn tool_result_text_and_base64_image() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","messages":[
                {"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"do_work","arguments":"{\"a\":1}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":[
                    {"type":"text","text":"tool ok"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="}}
                ]}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": [
                    {"type": "text", "text": "tool ok"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUg=="}}
                ]}]}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_ToolResultURLImageOnly (claude_openai_request_test.go)
    #[test]
    fn tool_result_url_image_only() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[
                {"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"do_work","arguments":"{\"a\":1}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":[{"type":"image_url","image_url":{"url":"https://example.com/tool.png"}}]}
            ]}"#,
        );
        assert_eq!(
            out["messages"][1],
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://example.com/tool.png"}}
            ]}]})
        );
    }

    #[test]
    fn tool_result_content_shapes() {
        let out = req(
            "claude-test",
            r#"{"messages":[
                {"role":"tool","tool_call_id":"c1","content":["plain",{"type":"weird"}]},
                {"role":"tool","tool_call_id":"c2","content":[{"type":"weird","x":1}]},
                {"role":"tool","tool_call_id":"c3","content":[]},
                {"role":"tool","tool_call_id":"c4","content":{"type":"text","text":"obj"}},
                {"role":"tool","tool_call_id":"c5","content":42},
                {"role":"tool","tool_call_id":"c6"}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": [{"type": "text", "text": "plain"}]},
                {"type": "tool_result", "tool_use_id": "c2", "content": "[{\"type\":\"weird\",\"x\":1}]"},
                {"type": "tool_result", "tool_use_id": "c3", "content": []},
                {"type": "tool_result", "tool_use_id": "c4", "content": [{"type": "text", "text": "obj"}]},
                {"type": "tool_result", "tool_use_id": "c5", "content": "42"},
                {"type": "tool_result", "tool_use_id": "c6", "content": ""}
            ]}])
        );
    }

    #[test]
    fn user_file_and_image_parts() {
        let out = req(
            "claude-test",
            r#"{"messages":[{"role":"user","content":[
                {"type":"file","file":{"file_data":"data:application/pdf;base64,JVBERi0x"}},
                {"type":"file","file":{"file_data":"data:application/pdf,abc;x"}},
                {"type":"image_url","image_url":{"url":"data:;base64,QQ=="}},
                {"type":"image_url","image_url":{"url":"data:image/png"}},
                {"type":"input_audio"}
            ]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0x"}},
                {"type": "image", "source": {"type": "base64", "media_type": "application/octet-stream", "data": "QQ=="}}
            ]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_SystemRoleBecomesTopLevelSystem (claude_openai_request_test.go)
    #[test]
    fn system_role_becomes_top_level_system() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Hello"}]}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": "You are a helpful assistant."}])
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_MultipleSystemMessagesMergedIntoTopLevelSystem (claude_openai_request_test.go)
    #[test]
    fn multiple_system_messages_merged_into_top_level_system() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[{"role":"system","content":"Rule 1"},{"role":"system","content":[{"type":"text","text":"Rule 2"}]},{"role":"user","content":"Hello"}]}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": "Rule 1"}, {"type": "text", "text": "Rule 2"}])
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_SystemOnlyInputKeepsFallbackUserMessage (claude_openai_request_test.go)
    #[test]
    fn system_only_input_keeps_fallback_user_message() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","messages":[{"role":"system","content":"You are a helpful assistant."}]}"#,
        );
        assert_eq!(
            out,
            json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 32000,
                "messages": [{"role": "user", "content": [{"type": "text", "text": ""}]}],
                "metadata": {"user_id": sha256_hex("model:gpt-4.1")},
                "stream": false,
                "system": [{"type": "text", "text": "You are a helpful assistant."}]
            })
        );
    }

    // port of TestConvertOpenAIRequestToClaude_PreservesContentPartCacheControl (claude_openai_request_test.go)
    #[test]
    fn preserves_content_part_cache_control() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[{"role":"user","content":[
                {"type":"text","text":"cached prefix","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"fresh question"}
            ]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "cached prefix", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "fresh question"}
            ]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_PreservesMessageLevelCacheControl (claude_openai_request_test.go)
    #[test]
    fn preserves_message_level_cache_control() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[{"role":"user","content":"cache me","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "cache me", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_PreservesToolCacheControl (claude_openai_request_test.go)
    #[test]
    fn preserves_tool_cache_control() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[{"role":"user","content":"hi"}],"tools":[{
                "type":"function",
                "function":{"name":"lookup","description":"Lookup something","parameters":{"type":"object","properties":{}}},
                "cache_control":{"type":"ephemeral"}
            }]}"#,
        );
        assert_eq!(
            out["tools"],
            json!([{
                "name": "lookup",
                "description": "Lookup something",
                "input_schema": {"properties": {}, "type": "object"},
                "cache_control": {"type": "ephemeral"}
            }])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_NormalizesRootToolSchemaUnions (claude_openai_request_test.go)
    #[test]
    fn normalizes_root_tool_schema_unions() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[{"role":"user","content":"hi"}],"tools":[
                {"type":"function","function":{"name":"without_type","parameters":{"anyOf":[
                    {"type":"object","properties":{"a":{"type":"string"}}},
                    {"type":"object","properties":{"b":{"type":"string"}}}
                ]}}},
                {"type":"function","function":{"name":"constraint_union","parametersJsonSchema":{
                    "type":"object",
                    "properties":{"a":{"type":"string"},"b":{"type":"string"}},
                    "anyOf":[{"required":["a"]},{"required":["b"]}]
                }}}
            ]}"#,
        );
        let schema = json!({"properties": {"a": {"type": "string"}, "b": {"type": "string"}}, "type": "object"});
        assert_eq!(
            out["tools"],
            json!([
                {"name": "without_type", "description": "", "input_schema": schema},
                {"name": "constraint_union", "description": "", "input_schema": schema}
            ])
        );
    }

    #[test]
    fn normalize_schema_all_of_merges_required() {
        let schema = json!({
            "required": ["z"],
            "description": "d",
            "allOf": [
                {"type": ["object", "null"], "properties": {"z": {"type": "integer"}, "y": {}}, "required": ["y", "z"]},
                {"type": "string", "properties": {"no": {}}},
                {"type": 7}
            ],
            "oneOf": "bad",
            "properties": {"z": {"type": "number"}}
        });
        assert_eq!(
            normalize_claude_tool_input_schema(Some(&schema)),
            json!({
                "description": "d",
                "properties": {"y": {}, "z": {"type": "number"}},
                "required": ["z", "y"],
                "type": "object"
            })
        );
        assert_eq!(
            normalize_claude_tool_input_schema(Some(&Value::Null)),
            json!({"type": "object", "properties": {}})
        );
    }

    // port of TestConvertOpenAIRequestToClaude_PartCacheControlWinsOverMessageLevel (claude_openai_request_test.go)
    #[test]
    fn part_cache_control_wins_over_message_level() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[{"role":"user","cache_control":{"type":"ephemeral","ttl":"1h"},"content":[
                {"type":"text","text":"part cached","cache_control":{"type":"ephemeral"}}
            ]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "part cached", "cache_control": {"type": "ephemeral"}}
            ]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_DeveloperRoleBecomesTopLevelSystem (claude_openai_request_test.go)
    #[test]
    fn developer_role_becomes_top_level_system() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[
                {"role":"system","content":"S1"},
                {"role":"developer","content":[{"type":"text","text":"D1"},{"type":"text","text":"D2"}]},
                {"role":"user","content":"Hello"}
            ]}"#,
        );
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": "S1"},
                {"type": "text", "text": "D1"},
                {"type": "text", "text": "D2"}
            ])
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_DeveloperMessageCacheControlAppliesToLastBlock (claude_openai_request_test.go)
    #[test]
    fn developer_message_cache_control_applies_to_last_block() {
        let out = req(
            "claude-sonnet-4-5",
            r#"{"messages":[
                {"role":"developer","content":[{"type":"text","text":"D1"},{"type":"text","text":"D2"}],"cache_control":{"type":"ephemeral"}},
                {"role":"user","content":"Hello"}
            ]}"#,
        );
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": "D1"},
                {"type": "text", "text": "D2", "cache_control": {"type": "ephemeral"}}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_DeduplicatesToolResults (claude_openai_request_test.go)
    #[test]
    fn deduplicates_tool_results() {
        let mut out = req(
            "claude-test",
            r#"{"messages":[
                {"role":"user","content":"Run tools"},
                {"role":"assistant","tool_calls":[{"id":"call_dup","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_dup","content":"first output"},
                {"role":"assistant","content":"Next step","tool_calls":[{"id":"call_other","type":"function","function":{"name":"search","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_dup","content":"final output"},
                {"role":"tool","tool_call_id":"call_other","content":"search output"},
                {"role":"tool","tool_call_id":"","content":"empty id output"}
            ]}"#,
        );
        // An empty tool_call_id gets SanitizeClaudeToolID's time-based fallback.
        let generated = out["messages"][4]["content"][1]["tool_use_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(generated.starts_with("toolu_"), "{generated}");
        out["messages"][4]["content"][1]["tool_use_id"] = json!("<generated>");
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "Run tools"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_dup", "name": "lookup", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_dup", "content": "final output"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Next step"},
                    {"type": "tool_use", "id": "call_other", "name": "search", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_other", "content": "search output"},
                    {"type": "tool_result", "tool_use_id": "<generated>", "content": "empty id output"}
                ]}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_MaxTokensAndMaxCompletionTokens (claude_openai_request_test.go)
    #[test]
    fn max_tokens_and_max_completion_tokens() {
        for (raw, want) in [
            (
                r#"{"messages":[{"role":"user","content":"hi"}],"max_completion_tokens":128000}"#,
                128000,
            ),
            (
                r#"{"messages":[{"role":"user","content":"hi"}],"max_tokens":4096}"#,
                4096,
            ),
            (
                r#"{"messages":[{"role":"user","content":"hi"}],"max_tokens":4096,"max_completion_tokens":128000}"#,
                4096,
            ),
            (r#"{"messages":[{"role":"user","content":"hi"}]}"#, 32000),
        ] {
            assert_eq!(
                req("claude-3-7-sonnet-20250219", raw)["max_tokens"],
                json!(want),
                "{raw}"
            );
        }
    }

    // port of TestConvertOpenAIRequestToClaude_PreservesCallerSuppliedMetadataUserID (claude_openai_request_test.go)
    #[test]
    fn preserves_caller_supplied_metadata_user_id() {
        for (raw, expected) in [
            (
                r#"{"model":"claude-test","metadata":{"user_id":"custom-user-123"},"messages":[{"role":"user","content":"hello"}]}"#,
                "custom-user-123",
            ),
            (
                r#"{"model":"claude-test","metadata":{"user_id":"foo\"bar\nbaz\\qux"},"messages":[{"role":"user","content":"hello"}]}"#,
                "foo\"bar\nbaz\\qux",
            ),
            (
                r#"{"model":"claude-test","metadata":{"user_id":"{\"device_id\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"session_id\":\"11111111-2222-4333-8444-555555555555\"}"},"messages":[{"role":"user","content":"hello"}]}"#,
                r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#,
            ),
        ] {
            assert_eq!(
                req("claude-test", raw)["metadata"],
                json!({"user_id": expected})
            );
        }
    }

    // port of TestConvertOpenAIRequestToClaude_PreservesOpenAIUserField (claude_openai_request_test.go)
    #[test]
    fn preserves_openai_user_field() {
        let out = req(
            "claude-test",
            r#"{"model":"claude-test","user":"openai-user-456","messages":[{"role":"user","content":"hello"}]}"#,
        );
        assert_eq!(out["metadata"], json!({"user_id": "openai-user-456"}));
    }

    // port of TestConvertOpenAIRequestToClaude_DifferentSessionsProduceDifferentUserIDs (claude_openai_request_test.go)
    #[test]
    fn different_sessions_produce_different_user_ids() {
        let a = req(
            "claude-test",
            r#"{"model":"claude-test","prompt_cache_key":"session-a","messages":[{"role":"user","content":"hello"}]}"#,
        );
        let b = req(
            "claude-test",
            r#"{"model":"claude-test","prompt_cache_key":"session-b","messages":[{"role":"user","content":"hello"}]}"#,
        );
        assert_eq!(
            a["metadata"]["user_id"],
            json!(sha256_hex("prompt_cache_key:session-a"))
        );
        assert_eq!(
            b["metadata"]["user_id"],
            json!(sha256_hex("prompt_cache_key:session-b"))
        );
        assert_ne!(a["metadata"]["user_id"], b["metadata"]["user_id"]);
    }

    // port of TestConvertOpenAIRequestToClaude_DeterministicWithoutSessionKey (claude_openai_request_test.go)
    #[test]
    fn deterministic_without_session_key() {
        let first = req(
            "claude-test",
            r#"{"model":"claude-test","messages":[{"role":"user","content":"stable first message"}]}"#,
        );
        let second = req(
            "claude-test",
            r#"{"model":"claude-test","messages":[{"role":"user","content":"stable first message"},{"role":"assistant","content":"hi"},{"role":"user","content":"second message"}]}"#,
        );
        let want = json!(user_id("stable first message"));
        assert_eq!(first["metadata"]["user_id"], want);
        assert_eq!(second["metadata"]["user_id"], want);
    }

    // port of TestConvertOpenAIRequestToClaude_ResponseFormatJSONSchema (claude_openai_request_test.go)
    #[test]
    fn response_format_json_schema() {
        let out = req(
            "claude-sonnet-4-6",
            r#"{
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "Extract facts from: Yesterday it rained in Beijing."}],
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": "extracted_facts",
                    "strict": true,
                    "schema": {"type": "object", "properties": {"facts": {"type": "array", "items": {"type": "string"}}}, "required": ["facts"]}
                }}
            }"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\nSchema Name: extracted_facts\nJSON Schema:\n{\"type\":\"object\",\"properties\":{\"facts\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"required\":[\"facts\"]}\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object."}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_ResponseFormatJSONObject (claude_openai_request_test.go)
    #[test]
    fn response_format_json_object() {
        let out = req(
            "claude-sonnet-4-6",
            r#"{"messages":[{"role":"user","content":"Return a JSON object."}],"response_format":{"type":"json_object"}}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": JSON_OBJECT_INSTRUCTION}])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_ResponseFormatPreservesExistingSystem (claude_openai_request_test.go)
    #[test]
    fn response_format_preserves_existing_system() {
        let out = req(
            "claude-sonnet-4-6",
            r#"{"messages":[{"role":"system","content":"Custom operator instruction."},{"role":"user","content":"Extract facts."}],"response_format":{"type":"json_object"}}"#,
        );
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": "Custom operator instruction."},
                {"type": "text", "text": JSON_OBJECT_INSTRUCTION}
            ])
        );
    }

    // port of TestConvertOpenAIRequestToClaude_ResponseFormatAbsentOrTextNoOp (claude_openai_request_test.go)
    #[test]
    fn response_format_absent_or_text_no_op() {
        for body in [
            r#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"plain text"}]}"#,
            r#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"plain text"}],"response_format":{"type":"text"}}"#,
        ] {
            assert_eq!(req("claude-sonnet-4-6", body).get("system"), None);
        }
    }

    // port of TestConvertOpenAIRequestToClaude_ToolResultPartCacheControlHoisted (claude_openai_request_test.go)
    #[test]
    fn tool_result_part_cache_control_hoisted() {
        let out = req(
            "claude-test",
            r#"{
                "messages":[
                    {"role":"user","content":[{"type":"text","text":"Use calc for 2+2.","cache_control":{"type":"ephemeral"}}]},
                    {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"calc","arguments":"{\"expr\":\"2+2\"}"}}]},
                    {"role":"tool","tool_call_id":"call_1","content":[{"type":"text","text":"4","cache_control":{"type":"ephemeral"}}]}
                ],
                "tools":[{"type":"function","function":{"name":"calc","description":"calc","parameters":{"type":"object","properties":{"expr":{"type":"string"}},"required":["expr"]}}}]
            }"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "Use calc for 2+2.", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "calc", "input": {"expr": "2+2"}}]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": [{"type": "text", "text": "4"}],
                    "cache_control": {"type": "ephemeral"}
                }]}
            ])
        );
        assert_eq!(
            out["tools"],
            json!([{
                "name": "calc",
                "description": "calc",
                "input_schema": {"properties": {"expr": {"type": "string"}}, "required": ["expr"], "type": "object"}
            }])
        );
    }

    fn tool(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name, "parameters": {"type": "object", "properties": {}}}})
    }

    fn claude_tool(name: &str) -> Value {
        json!({"name": name, "description": "", "input_schema": {"properties": {}, "type": "object"}})
    }

    fn with_tools(extra: Value) -> Value {
        let mut body = json!({"model": "claude-sonnet-4-6", "messages": [{"role": "user", "content": "test"}]});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        translate_request("claude-sonnet-4-6", &body, false)
    }

    // port of TestConvertOpenAIRequestToClaude_ToolChoice (claude_openai_request_test.go)
    #[test]
    fn tool_choice() {
        // none produces type none
        let out =
            with_tools(json!({"tool_choice": "none", "tools": [tool("tool_a"), tool("tool_b")]}));
        assert_eq!(out["tool_choice"], json!({"type": "none"}));

        // object none produces type none
        let out = with_tools(json!({"tool_choice": {"type": "none"}, "tools": [tool("tool_a")]}));
        assert_eq!(out["tool_choice"], json!({"type": "none"}));

        // allowed_tools filters tools and sets auto mode
        let out = with_tools(json!({
            "tool_choice": {"type": "allowed_tools", "allowed_tools": {"mode": "auto", "tools": [{"type": "function", "function": {"name": "tool_b"}}]}},
            "tools": [tool("tool_a"), tool("tool_b")]
        }));
        assert_eq!(out["tool_choice"], json!({"type": "auto"}));
        assert_eq!(out["tools"], json!([claude_tool("tool_b")]));

        // allowed_tools multi function filters tools and supports required mode
        let out = with_tools(json!({
            "tool_choice": {"type": "allowed_tools", "allowed_tools": {"mode": "required", "tools": [
                {"type": "function", "function": {"name": "tool_b"}},
                {"type": "function", "function": {"name": "tool_c"}}
            ]}},
            "tools": [tool("tool_a"), tool("tool_b"), tool("tool_c")]
        }));
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
        assert_eq!(
            out["tools"],
            json!([claude_tool("tool_b"), claude_tool("tool_c")])
        );

        // parallel_tool_calls false adds disable_parallel_tool_use
        let out = with_tools(
            json!({"tool_choice": "required", "parallel_tool_calls": false, "tools": [tool("tool_a")]}),
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );

        // parallel_tool_calls null / true do not add disable_parallel_tool_use
        for parallel in [Value::Null, Value::Bool(true)] {
            let out = with_tools(
                json!({"tool_choice": "required", "parallel_tool_calls": parallel, "tools": [tool("tool_a")]}),
            );
            assert_eq!(out["tool_choice"], json!({"type": "any"}));
        }

        // omitted tool_choice with parallel_tool_calls false sets auto with disable_parallel_tool_use
        let out = with_tools(json!({"parallel_tool_calls": false, "tools": [tool("tool_a")]}));
        assert_eq!(
            out["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );

        // empty allowed_tools fails closed to type none
        let out = with_tools(json!({
            "tool_choice": {"type": "allowed_tools", "allowed_tools": {"tools": []}},
            "tools": [tool("tool_a")]
        }));
        assert_eq!(out["tool_choice"], json!({"type": "none"}));
        assert_eq!(out.get("tools"), None);

        // function choice with missing name fails closed to type none
        let out = with_tools(
            json!({"tool_choice": {"type": "function", "function": {}}, "tools": [tool("tool_a")]}),
        );
        assert_eq!(out["tool_choice"], json!({"type": "none"}));

        // tool_choice null does not set tool_choice
        let out = with_tools(json!({"tool_choice": null, "tools": [tool("tool_a")]}));
        assert_eq!(out.get("tool_choice"), None);
    }

    #[test]
    fn tool_choice_function_name_fallback_and_any() {
        let out = with_tools(
            json!({"tool_choice": {"type": "function", "name": "a.b"}, "tools": [tool("a.b")]}),
        );
        assert_eq!(out["tool_choice"], json!({"type": "tool", "name": "a_b"}));
        let out = with_tools(json!({"tool_choice": {"type": "any"}, "tools": [tool("x")]}));
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
    }

    fn strict_tool(strict_in_function: Option<bool>, strict_on_tool: Option<bool>) -> Value {
        let mut function = json!({"name": "tool_a", "description": "Controlled tool.", "parameters": {"type": "object", "properties": {}}});
        if let Some(s) = strict_in_function {
            function["strict"] = json!(s);
        }
        let mut t = json!({"type": "function", "function": function});
        if let Some(s) = strict_on_tool {
            t["strict"] = json!(s);
        }
        with_tools(json!({"tools": [t]}))["tools"][0].clone()
    }

    // port of TestConvertOpenAIRequestToClaude_ToolStrict (claude_openai_request_test.go)
    #[test]
    fn tool_strict() {
        let base = json!({"name": "tool_a", "description": "Controlled tool.", "input_schema": {"properties": {}, "type": "object"}});
        let with_strict = |s: bool| {
            let mut v = base.clone();
            v["strict"] = json!(s);
            v
        };
        assert_eq!(strict_tool(Some(true), None), with_strict(true));
        assert_eq!(strict_tool(None, Some(true)), with_strict(true));
        assert_eq!(strict_tool(Some(false), None), with_strict(false));
        assert_eq!(strict_tool(None, None), base);
        // function-level strict wins over the tool-level one
        assert_eq!(strict_tool(Some(false), Some(true)), with_strict(false));
    }

    // port of TestConvertOpenAIRequestToClaude_SanitizesToolNamesAndProvidesFallbackSchema (claude_openai_request_test.go)
    #[test]
    fn sanitizes_tool_names_and_provides_fallback_schema() {
        let out = req(
            "claude-sonnet-4-6",
            r#"{
                "model": "claude-sonnet-4-6",
                "messages": [
                    {"role": "assistant", "content": "calling tool", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "mcp.server.special:get_time", "arguments": "{}"}}]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "12:00 PM"},
                    {"role": "user", "content": "continue"}
                ],
                "tools": [
                    {"type": "function", "function": {"name": "mcp.server.special:get_time", "description": "Get current time"}},
                    {"type": "function", "function": {"name": "clean_tool", "description": "Parameterless clean tool"}}
                ],
                "tool_choice": {"type": "function", "function": {"name": "mcp.server.special:get_time"}}
            }"#,
        );
        assert_eq!(
            out["tools"],
            json!([
                {"name": "mcp_server_special_get_time", "description": "Get current time", "input_schema": {"type": "object", "properties": {}}},
                {"name": "clean_tool", "description": "Parameterless clean tool", "input_schema": {"type": "object", "properties": {}}}
            ])
        );
        assert_eq!(
            out["messages"][0],
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "calling tool"},
                {"type": "tool_use", "id": "call_1", "name": "mcp_server_special_get_time", "input": {}}
            ]})
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "tool", "name": "mcp_server_special_get_time"})
        );
    }

    #[test]
    fn tool_call_arguments_shapes_and_generated_id() {
        let out = req(
            "claude-test",
            r#"{"messages":[{"role":"assistant","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"a","arguments":"[1,2]"}},
                {"id":"c2","type":"function","function":{"name":"b","arguments":"not json"}},
                {"id":"c3","type":"function","function":{"name":"c","arguments":{"k":"v"}}},
                {"id":"c4","type":"function","function":{"name":"d"}},
                {"id":"c5","type":"other","function":{"name":"e"}},
                {"type":"function","function":{"name":"f","arguments":"{\"x\":1}"}}
            ]}]}"#,
        );
        let mut content = out["messages"][0]["content"].clone();
        let generated = content[4]["id"].as_str().unwrap().to_string();
        assert_eq!(generated.len(), "toolu_".len() + 24);
        assert!(generated.starts_with("toolu_"));
        assert!(generated["toolu_".len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric()));
        content[4]["id"] = json!("<generated>");
        assert_eq!(
            content,
            json!([
                {"type": "tool_use", "id": "c1", "name": "a", "input": {}},
                {"type": "tool_use", "id": "c2", "name": "b", "input": {}},
                {"type": "tool_use", "id": "c3", "name": "c", "input": {"k": "v"}},
                {"type": "tool_use", "id": "c4", "name": "d", "input": {}},
                {"type": "tool_use", "id": "<generated>", "name": "f", "input": {"x": 1}}
            ])
        );
        assert_ne!(
            generate_claude_tool_call_id(),
            generate_claude_tool_call_id()
        );
    }

    // ----- response -----

    // port of TestConvertClaudeResponseToOpenAINonStreamFinishReasons (noop_optimization_test.go)
    #[test]
    fn non_stream_finish_reasons() {
        for (stop_reason, want) in [
            ("", "stop"),
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("refusal", "content_filter"),
            ("sensitive", "content_filter"),
        ] {
            let out = non_stream(&[
                json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}}),
            ]);
            assert_eq!(
                out,
                json!({
                    "id": "",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": ""}, "finish_reason": want}],
                    "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
                }),
                "stop_reason {stop_reason:?}"
            );
        }
    }

    fn usage_json(prompt: i64, completion: i64, cached: i64, creation: i64) -> Value {
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "prompt_tokens_details": {"cached_tokens": cached, "cached_creation_tokens": creation, "cache_write_tokens": creation}
        })
    }

    // port of TestConvertClaudeResponseToOpenAI_StreamUsageIncludesCachedTokens (claude_openai_response_test.go)
    #[test]
    fn stream_usage_includes_cached_tokens() {
        let out = stream(
            "claude-opus-4-6",
            &[
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 13, "output_tokens": 4, "cache_read_input_tokens": 22000, "cache_creation_input_tokens": 31}}),
            ],
        );
        assert_eq!(
            out,
            vec![json!({
                "id": "",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "claude-opus-4-6",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": usage_json(22044, 4, 22000, 31)
            })]
        );
    }

    // port of TestConvertClaudeResponseToOpenAI_StreamUsageMergesMessageStartUsage (claude_openai_response_test.go)
    #[test]
    fn stream_usage_merges_message_start_usage() {
        let out = stream(
            "claude-opus-4-6",
            &[
                json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6", "usage": {"input_tokens": 13, "output_tokens": 1, "cache_read_input_tokens": 22000, "cache_creation_input_tokens": 31}}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
            ],
        );
        assert_eq!(
            out,
            vec![
                json!({
                    "id": "msg_123", "object": "chat.completion.chunk", "created": 1, "model": "claude-opus-4-6",
                    "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                }),
                json!({
                    "id": "msg_123", "object": "chat.completion.chunk", "created": 1, "model": "claude-opus-4-6",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": usage_json(22044, 4, 22000, 31)
                }),
            ]
        );
    }

    // port of TestConvertClaudeResponseToOpenAINonStream_UsageIncludesCachedTokens (claude_openai_response_test.go)
    #[test]
    fn non_stream_usage_includes_cached_tokens() {
        let out = non_stream(&[
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 13, "output_tokens": 4, "cache_read_input_tokens": 22000, "cache_creation_input_tokens": 31}}),
        ]);
        assert_eq!(
            out,
            json!({
                "id": "msg_123",
                "object": "chat.completion",
                "created": 1,
                "model": "claude-opus-4-6",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": ""}, "finish_reason": "stop"}],
                "usage": usage_json(22044, 4, 22000, 31)
            })
        );
    }

    // port of TestConvertClaudeResponseToOpenAINonStream_UsageMergesMessageStartUsage (claude_openai_response_test.go)
    #[test]
    fn non_stream_usage_merges_message_start_usage() {
        let out = non_stream(&[
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6", "usage": {"input_tokens": 13, "output_tokens": 1, "cache_read_input_tokens": 22000, "cache_creation_input_tokens": 31}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
        ]);
        assert_eq!(out["usage"], usage_json(22044, 4, 22000, 31));
    }

    // port of TestConvertClaudeResponseToOpenAI_RefusalStopReason (claude_openai_response_test.go)
    #[test]
    fn stream_refusal_stop_reason() {
        for reason in ["refusal", "sensitive"] {
            let out = stream(
                "claude-opus-4-6",
                &[
                    json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 10}}),
                ],
            );
            assert_eq!(out.len(), 1);
            assert_eq!(
                out[0]["choices"][0]["finish_reason"],
                json!("content_filter")
            );
        }
    }

    // port of TestConvertClaudeResponseToOpenAINonStream_RefusalStopReason (claude_openai_response_test.go)
    #[test]
    fn non_stream_refusal_stop_reason() {
        for reason in ["refusal", "sensitive"] {
            let out = non_stream(&[
                json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6"}}),
                json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"input_tokens": 10, "output_tokens": 20}}),
            ]);
            assert_eq!(out["choices"][0]["finish_reason"], json!("content_filter"));
        }
    }

    // port of TestConvertClaudeResponseToOpenAINonStream_ReasoningContent (claude_openai_response_test.go)
    #[test]
    fn non_stream_reasoning_content() {
        let out = non_stream(&[
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me analyze the problem."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": " Step 2 is clear."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Here is the solution."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 10, "output_tokens": 20}}),
        ]);
        assert_eq!(
            out["choices"],
            json!([{
                "index": 0,
                "message": {"role": "assistant", "content": "Here is the solution.", "reasoning_content": "Let me analyze the problem. Step 2 is clear."},
                "finish_reason": "stop"
            }])
        );
    }

    // port of TestConvertClaudeResponseToOpenAINonStream_OmitsReasoningContentWhenAbsent (claude_openai_response_test.go)
    #[test]
    fn non_stream_omits_reasoning_content_when_absent() {
        let out = non_stream(&[
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Just plain text."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 10, "output_tokens": 20}}),
        ]);
        assert_eq!(
            out["choices"],
            json!([{"index": 0, "message": {"role": "assistant", "content": "Just plain text."}, "finish_reason": "stop"}])
        );
    }

    // port of TestConvertClaudeResponseToOpenAI_StreamAndNonStreamParity (claude_openai_response_test.go)
    #[test]
    fn stream_and_non_stream_parity() {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6", "usage": {"input_tokens": 15, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "First thought. "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Second thought."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Final "}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "answer."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 25}}),
            json!({"type": "message_stop"}),
        ];
        let chunks = stream("claude-opus-4-6", &events);
        let mut reasoning = String::new();
        let mut content = String::new();
        let mut finish = String::new();
        for chunk in &chunks {
            reasoning.push_str(
                chunk["choices"][0]["delta"]["reasoning_content"]
                    .as_str()
                    .unwrap_or(""),
            );
            content.push_str(
                chunk["choices"][0]["delta"]["content"]
                    .as_str()
                    .unwrap_or(""),
            );
            if let Some(fr) = chunk["choices"][0]["finish_reason"].as_str() {
                finish = fr.to_string();
            }
        }
        assert_eq!(reasoning, "First thought. Second thought.");
        assert_eq!(content, "Final answer.");
        assert_eq!(finish, "stop");

        let out = non_stream(&events);
        assert_eq!(
            out,
            json!({
                "id": "msg_123",
                "object": "chat.completion",
                "created": 1,
                "model": "claude-opus-4-6",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": content, "reasoning_content": reasoning}, "finish_reason": finish}],
                "usage": usage_json(15, 25, 0, 0)
            })
        );
    }

    // port of TestConvertClaudeResponseToOpenAI_RedactedThinkingIgnored (claude_openai_response_test.go)
    #[test]
    fn redacted_thinking_ignored() {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "redacted_thinking", "data": "encrypted_blob"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Visible reply."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 10, "output_tokens": 20}}),
        ];
        let out = non_stream(&events);
        assert_eq!(
            out["choices"][0]["message"],
            json!({"role": "assistant", "content": "Visible reply."})
        );

        let chunks = stream("claude-opus-4-6", &events);
        let deltas: Vec<Value> = chunks
            .iter()
            .map(|c| c["choices"][0]["delta"].clone())
            .collect();
        assert_eq!(
            deltas,
            vec![
                json!({"role": "assistant"}),
                json!({"content": "Visible reply."}),
                json!({})
            ]
        );
    }

    // port of TestConvertClaudeResponseToOpenAI_StreamToolCallIndexIsZeroBased (claude_openai_response_test.go)
    #[test]
    fn stream_tool_call_index_is_zero_based() {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6", "usage": {"input_tokens": 15, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Thinking..."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"city\": \"Paris\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "get_time"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":\"Tokyo\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 25}}),
            json!({"type": "message_stop"}),
        ];
        let calls: Vec<Value> = stream("claude-opus-4-6", &events)
            .iter()
            .filter_map(|c| c["choices"][0]["delta"].get("tool_calls").cloned())
            .collect();
        assert_eq!(
            calls,
            vec![
                json!([{"index": 0, "id": "toolu_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}}]),
                json!([{"index": 1, "id": "toolu_2", "type": "function", "function": {"name": "get_time", "arguments": "{\"city\":\"Tokyo\"}"}}]),
            ]
        );
    }

    // port of TestConvertClaudeResponseToOpenAI_StreamEmitsTrailingUsageChunkWithCacheDetails (claude_openai_response_test.go)
    #[test]
    fn stream_emits_trailing_usage_chunk_with_cache_details() {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_123", "model": "claude-opus-4-6", "usage": {"input_tokens": 100, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 50, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 15}}),
            json!({"type": "message_stop"}),
            json!({"type": "message_stop"}), // duplicate: ignored via trailing_usage_sent
        ];
        let chunks = stream("claude-opus-4-6", &events);
        let head = |delta: Value, finish: Value| {
            json!({
                "id": "msg_123", "object": "chat.completion.chunk", "created": 1, "model": "claude-opus-4-6",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
            })
        };
        let mut finish_chunk = head(json!({}), json!("stop"));
        finish_chunk["usage"] = usage_json(170, 15, 50, 20);
        assert_eq!(
            chunks,
            vec![
                head(json!({"role": "assistant"}), Value::Null),
                head(json!({"content": "hello"}), Value::Null),
                finish_chunk,
                json!({
                    "id": "msg_123", "object": "chat.completion.chunk", "created": 1, "model": "claude-opus-4-6",
                    "choices": [],
                    "usage": usage_json(170, 15, 50, 20)
                }),
            ]
        );
    }

    #[test]
    fn stream_error_ping_and_unknown_events() {
        let out = stream(
            "m",
            &[
                json!({"type": "ping"}),
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
                json!({"type": "something_new"}),
                json!({"type": "message_stop"}),
            ],
        );
        assert_eq!(
            out,
            vec![json!({"error": {"message": "Overloaded", "type": "overloaded_error"}})]
        );
    }

    /// End to end: thinking + text + a tool_use streamed as input_json_delta
    /// fragments, then the terminator — every client frame checked exactly.
    #[test]
    fn end_to_end_stream_thinking_text_tool_use() {
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_e2e", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "usage": {"input_tokens": 40, "cache_read_input_tokens": 10, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Need the weather."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "c2ln"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Checking "}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "now."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_abc", "name": "get_weather", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": ""}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": " \"Paris\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 30}}),
            json!({"type": "message_stop"}),
        ];

        let mut t = StreamTranslator::new(&json!({"model": "my-chat-model", "stream": true}));
        let mut frames: Vec<String> = Vec::new();
        for ev in &events {
            frames.extend(t.push(Some(ev["type"].as_str().unwrap()), ev));
        }
        frames.extend(t.finish());
        assert!(t.finish().is_empty(), "[DONE] is emitted once");

        let created = t.created_at;
        assert!(created > 0);
        let chunk = |delta: Value, finish: Value| {
            json!({
                "id": "msg_e2e", "object": "chat.completion.chunk", "created": created, "model": "my-chat-model",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
            })
        };
        let mut finish_chunk = chunk(json!({}), json!("tool_calls"));
        finish_chunk["usage"] = usage_json(50, 30, 10, 0);
        let expected: Vec<String> = vec![
            chunk(json!({"role": "assistant"}), Value::Null),
            chunk(json!({"reasoning_content": "Need the weather."}), Value::Null),
            chunk(json!({"content": "Checking "}), Value::Null),
            chunk(json!({"content": "now."}), Value::Null),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "toolu_abc", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}}]}),
                Value::Null,
            ),
            finish_chunk,
            json!({
                "id": "msg_e2e", "object": "chat.completion.chunk", "created": created, "model": "my-chat-model",
                "choices": [],
                "usage": usage_json(50, 30, 10, 0)
            }),
        ]
        .into_iter()
        .map(|v| format!("data: {v}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
        assert_eq!(frames, expected);
        assert_eq!(
            frames
                .iter()
                .filter(|f| f.as_str() == "data: [DONE]\n\n")
                .count(),
            1
        );
    }

    #[test]
    fn finish_without_any_event_still_terminates() {
        let mut t = StreamTranslator::new(&json!({}));
        assert_eq!(t.finish(), vec!["data: [DONE]\n\n".to_string()]);
        assert!(t.finish().is_empty());
    }

    #[test]
    fn non_stream_from_messages_object() {
        let upstream = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "text", "text": "Calling."},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_use", "id": "toolu_2", "name": "noop", "input": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 7, "output_tokens": 9, "cache_creation_input_tokens": 2}
        });
        let mut out = translate_non_stream(&upstream, &json!({"model": "client-model"}));
        assert!(out["created"].as_i64().unwrap() > 0);
        out["created"] = json!(1);
        assert_eq!(
            out,
            json!({
                "id": "msg_1",
                "object": "chat.completion",
                "created": 1,
                "model": "claude-sonnet-4-6",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Calling.",
                        "reasoning_content": "hmm",
                        "tool_calls": [
                            {"id": "toolu_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                            {"id": "toolu_2", "type": "function", "function": {"name": "noop", "arguments": "{}"}}
                        ]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": usage_json(9, 9, 0, 2)
            })
        );

        // A parsed event array is accepted as-is.
        let events = json!([{"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}]);
        assert_eq!(
            translate_non_stream(&events, &json!({}))["choices"][0]["finish_reason"],
            json!("length")
        );
    }
}
