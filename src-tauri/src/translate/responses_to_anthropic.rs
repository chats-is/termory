//! OpenAI Responses (client) ⇄ Anthropic Messages (upstream) translation.
//!
//! A faithful port of CLIProxyAPI's `internal/translator/claude/openai/responses`
//! pair (commit ed980be), registered upstream as
//! `translator.Register(OpenaiResponse, Claude, ConvertOpenAIResponsesRequestToClaude,
//! {Stream: ConvertClaudeResponseToOpenAIResponses, NonStream: ConvertClaudeResponseToOpenAIResponsesNonStream})`.
//! The CLIENT speaks OpenAI Responses (Codex CLI); the UPSTREAM speaks
//! Anthropic Messages.
//!
//! Each ported function carries a `// port of <GoFunc> (<file>)` comment so it
//! can be diffed against the Go source. The helpers the pair reaches into
//! other packages for (`translator/common`, `util`, `thinking`, `signature`)
//! are ported the same way, as far as this pair needs them.
//!
//! Go reads the body with gjson, whose accessors never fail: a missing path
//! is the empty string / zero / false, and `Exists()` is true for an explicit
//! JSON null. The `gp` / `gs` / `gi` / `gf` / `gb` helpers reproduce those
//! semantics so the ported branches read like the original.
//!
//! Deliberately NOT ported: `model(level)` thinking-suffix parsing (handled
//! elsewhere), token counting, metrics/logging (the `log.Warnf` diagnostics,
//! including `claudeMessageInvariantProblems`, which only feeds a log line),
//! config/plugin hooks, image/video generation. Termory has no model
//! registry, so `registry.LookupModelInfo` is a stand-in that knows no model
//! (the same choice `thinking.rs` makes): every Claude model takes the manual
//! (`enabled` + `budget_tokens`) thinking branch and no `max_tokens` clamp.
//!
//! Deviations from the Go code, each deliberate:
//! - Go iterates `FuncCallIDs` (a map) in random order when it finalizes
//!   pending tool calls; this port iterates in ascending block index so the
//!   event order is deterministic.
//! - `CurrentTextBuf` is written but never read in Go; it is not kept.
//! - `GenerateClaudeToolCallID` draws from `crypto/rand`; this port derives the
//!   24 characters from a time + atomic-counter seed (module-private helper).
//! - Raw JSON that Go copies verbatim (`schema.Raw`, an unconvertible tool
//!   output's `output.Raw`) is re-serialized compactly here.
//! - `StreamTranslator::finish` runs the `message_stop` branch when the
//!   upstream ended without one (Go emits nothing in that case).
//! - `translate_non_stream` also accepts a complete Messages object (replayed
//!   as the events Anthropic would have streamed), besides the SSE text Go
//!   aggregates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

// ───────────────────────────── gjson-like accessors ─────────────────────────────

/// Dotted-path lookup (`a.b.0.c`). `Some(Value::Null)` is an EXISTING null,
/// like gjson's `Exists()` on a JSON null.
fn gp<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
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

/// gjson `Result.String()`: strings verbatim, null/missing empty, everything
/// else its JSON text.
fn gs(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

/// `gs(gp(v, path))`.
fn gstr(v: &Value, path: &str) -> String {
    gs(gp(v, path))
}

/// gjson `Result.Int()`.
fn gi(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().map(|f| f as i64).unwrap_or(0)),
        Some(Value::String(s)) => {
            let t = s.trim();
            t.parse::<i64>()
                .ok()
                .or_else(|| t.parse::<f64>().ok().map(|f| f as i64))
                .unwrap_or(0)
        }
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// gjson `Result.Float()`.
fn gf(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        Some(Value::Bool(true)) => 1.0,
        _ => 0.0,
    }
}

/// gjson `Result.Bool()`.
fn gb(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "1" | "t" | "true"),
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

/// sjson `SetRawBytes(x, "arr.<n>", item)`: an index past the end pads the
/// array with nulls first.
fn set_array_index(arr: &mut Vec<Value>, index: i64, item: Value) {
    let index = index.max(0) as usize;
    while arr.len() <= index {
        arr.push(Value::Null);
    }
    arr[index] = item;
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn sha256_hex(input: &str) -> String {
    let sum = Sha256::digest(input.as_bytes());
    sum.iter().map(|b| format!("{b:02x}")).collect()
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Module-private id seed: wall-clock nanoseconds plus a process-wide counter,
/// so two ids minted in the same instant still differ.
fn next_id_seed() -> (u128, u64) {
    (now_nanos(), ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1)
}

// port of SSEEventData (translator/common/bytes.go), framed with the blank
// line the HTTP layer appends after each event.
fn sse(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

// ───────────────────────────── translator/common ─────────────────────────────

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
fn attach_cache_control(dst: &mut Value, src: Option<&Value>) {
    let cc = src.and_then(|s| s.get("cache_control"));
    if !is_valid_cache_control(cc) {
        return;
    }
    if let (Some(cc), Some(m)) = (cc, dst.as_object_mut()) {
        m.insert("cache_control".to_string(), cc.clone());
    }
}

// port of BuildClaudeStructuredOutputInstruction (translator/common/claude_system.go)
fn build_claude_structured_output_instruction(format: Option<&Value>) -> String {
    let Some(format) = format else {
        return String::new();
    };
    const JSON_OBJECT: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";
    let format_type = gstr(format, "type").trim().to_lowercase();
    match format_type.as_str() {
        "json_object" => JSON_OBJECT.to_string(),
        "json_schema" => {
            let json_schema = format.get("json_schema");
            let schema = json_schema
                .and_then(|j| j.get("schema"))
                .or_else(|| format.get("schema"));
            let Some(schema) = schema else {
                return JSON_OBJECT.to_string();
            };
            let mut builder = String::from(
                "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n",
            );
            let mut name = gs(json_schema.and_then(|j| j.get("name")))
                .trim()
                .to_string();
            if name.is_empty() {
                name = gstr(format, "name").trim().to_string();
            }
            if !name.is_empty() {
                builder.push_str("Schema Name: ");
                builder.push_str(&name);
                builder.push('\n');
            }
            let mut desc = gs(json_schema.and_then(|j| j.get("description")))
                .trim()
                .to_string();
            if desc.is_empty() {
                desc = gstr(format, "description").trim().to_string();
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

// port of ExtractResponsesCallID (translator/common/responses.go)
fn extract_responses_call_id(node: &Value) -> String {
    let call_id = gstr(node, "call_id").trim().to_string();
    if !call_id.is_empty() {
        return call_id;
    }
    let tool_call_id = gstr(node, "tool_call_id").trim().to_string();
    if !tool_call_id.is_empty() {
        return tool_call_id;
    }
    let call_id_camel = gstr(node, "callId").trim().to_string();
    if !call_id_camel.is_empty() {
        return call_id_camel;
    }
    let id = gstr(node, "id").trim().to_string();
    if id.starts_with("fco_") {
        return String::new();
    }
    id
}

fn is_tool_output_type(item: &Value) -> bool {
    matches!(
        gstr(item, "type").as_str(),
        "function_call_output" | "custom_tool_call_output"
    )
}

// port of NormalizeResponsesToolCallOutputs (translator/common/responses.go)
fn normalize_responses_tool_call_outputs(items: &[Value]) -> Vec<Value> {
    let mut normalized: Vec<Value> = items.to_vec();
    if normalized.is_empty() {
        return normalized;
    }

    let mut explicit_output_counts: HashMap<String, i64> = HashMap::new();
    for item in items {
        if is_tool_output_type(item) {
            let id = extract_responses_call_id(item);
            if !id.is_empty() {
                *explicit_output_counts.entry(id).or_insert(0) += 1;
            }
        }
    }

    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut pending_call_names: HashMap<String, String> = HashMap::new();

    let mut i = 0;
    while i < normalized.len() {
        let item_type = gstr(&normalized[i], "type");
        match item_type.as_str() {
            "function_call" | "custom_tool_call" => {
                let call_id = extract_responses_call_id(&normalized[i]);
                if !call_id.is_empty() {
                    pending_call_ids.push(call_id.clone());
                    pending_call_names.insert(call_id, gstr(&normalized[i], "name"));
                }
                i += 1;
            }
            "function_call_output" | "custom_tool_call_output" => {
                let start = i;
                while i < normalized.len() && is_tool_output_type(&normalized[i]) {
                    i += 1;
                }
                let outputs: Vec<Value> = normalized[start..i].to_vec();

                if !pending_call_ids.is_empty() {
                    let mut used = vec![false; outputs.len()];
                    let mut matched_for_pending: Vec<i64> = vec![-1; pending_call_ids.len()];

                    // Pass 1: exact explicit call ID match
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        for (out_idx, out) in outputs.iter().enumerate() {
                            if !used[out_idx] && extract_responses_call_id(out) == *pending_id {
                                used[out_idx] = true;
                                matched_for_pending[pending_idx] = out_idx as i64;
                                *explicit_output_counts
                                    .entry(pending_id.clone())
                                    .or_insert(0) -= 1;
                                break;
                            }
                        }
                    }

                    // Pass 2: match by function name for outputs with no explicit call ID
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        if matched_for_pending[pending_idx] >= 0
                            || explicit_output_counts.get(pending_id).copied().unwrap_or(0) > 0
                        {
                            continue;
                        }
                        let expected_name = pending_call_names
                            .get(pending_id)
                            .cloned()
                            .unwrap_or_default();
                        if !expected_name.is_empty() {
                            for (out_idx, out) in outputs.iter().enumerate() {
                                if !used[out_idx] && extract_responses_call_id(out).is_empty() {
                                    let out_name = gstr(out, "name").trim().to_string();
                                    if !out_name.is_empty() && out_name == expected_name {
                                        used[out_idx] = true;
                                        matched_for_pending[pending_idx] = out_idx as i64;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // Pass 3: FIFO fallback for outputs with no explicit call ID
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        if matched_for_pending[pending_idx] >= 0
                            || explicit_output_counts.get(pending_id).copied().unwrap_or(0) > 0
                        {
                            continue;
                        }
                        for (out_idx, out) in outputs.iter().enumerate() {
                            if !used[out_idx] && extract_responses_call_id(out).is_empty() {
                                let out_name = gstr(out, "name").trim().to_string();
                                let expected_name = pending_call_names
                                    .get(pending_id)
                                    .cloned()
                                    .unwrap_or_default();
                                if out_name.is_empty()
                                    || expected_name.is_empty()
                                    || out_name == expected_name
                                {
                                    used[out_idx] = true;
                                    matched_for_pending[pending_idx] = out_idx as i64;
                                    break;
                                }
                            }
                        }
                    }

                    // Apply matched call_ids to outputs
                    let mut remaining_pending: Vec<String> = Vec::new();
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        let out_idx = matched_for_pending[pending_idx];
                        if out_idx < 0 {
                            remaining_pending.push(pending_id.clone());
                            continue;
                        }
                        let matched_out = &outputs[out_idx as usize];
                        if gstr(matched_out, "call_id") != *pending_id {
                            let mut raw = matched_out.clone();
                            if raw.is_object() {
                                sj_set(&mut raw, "call_id", Value::from(pending_id.clone()));
                            }
                            normalized[start + out_idx as usize] = raw;
                        }
                    }
                    pending_call_ids = remaining_pending;
                }
            }
            _ => i += 1,
        }
    }

    normalized
}

const TOOLU_LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

// port of GenerateClaudeToolCallID (translator/common/request.go). Go draws
// rejection-sampled bytes from crypto/rand; this port hashes the module's
// time + counter seed and keeps the same rejection sampling.
fn generate_claude_tool_call_id() -> String {
    const MAX_VALID_BYTE: usize = 256 - (256 % 62); // 248: exact multiple of 62
    let (nanos, counter) = next_id_seed();
    let mut out = String::from("toolu_");
    let mut n = 0;
    let mut round: u64 = 0;
    while n < 24 {
        let digest = Sha256::digest(format!("{nanos}:{counter}:{round}").as_bytes());
        for &b in digest.iter() {
            if (b as usize) < MAX_VALID_BYTE {
                out.push(TOOLU_LETTERS[(b as usize) % TOOLU_LETTERS.len()] as char);
                n += 1;
                if n == 24 {
                    break;
                }
            }
        }
        round += 1;
    }
    out
}

// port of RequestModelName + requestModelName (translator/common/request.go),
// for the original request only.
fn request_model_name(raw: &Value) -> String {
    for path in ["model", "request.model"] {
        if let Some(Value::String(model)) = gp(raw, path) {
            if !model.trim().is_empty() {
                return model.clone();
            }
        }
    }
    String::new()
}

// port of SetResponsesToolCallIdentity (translator/common/responses.go)
fn set_responses_tool_call_identity(
    item: &mut Value,
    name: &str,
    namespace: &str,
    item_path: &str,
) {
    let (name_path, namespace_path) = if item_path.is_empty() {
        ("name".to_string(), "namespace".to_string())
    } else {
        (
            format!("{item_path}.name"),
            format!("{item_path}.namespace"),
        )
    };
    sj_set(item, &name_path, Value::from(name));
    if !namespace.is_empty() {
        sj_set(item, &namespace_path, Value::from(namespace));
    } else {
        sj_delete(item, &namespace_path);
    }
}

// port of IsGeminiThoughtPart (translator/common/gemini.go)
fn is_gemini_thought_part(part: &Value) -> bool {
    gb(part.get("thought"))
}

// port of DeriveClaudeUserID (translator/common/claude_user_id.go)
fn derive_claude_user_id(root: &Value) -> String {
    if let Some(Value::String(raw)) = gp(root, "metadata.user_id") {
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
        let value = gs(Some(v)).trim().to_string();
        if !value.is_empty() {
            seed.push_str("prompt_cache_key:");
            seed.push_str(&value);
        }
    }

    if seed.is_empty() {
        for path in ["session_id", "sessionId"] {
            if let Some(v) = root.get(path) {
                let value = gs(Some(v)).trim().to_string();
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
        let sid = gs(conversation.and_then(|c| c.get("id")))
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
            let sid = gs(Some(v)).trim().to_string();
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
            let value = gs(Some(v)).trim().to_string();
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
                seed.push_str(&gs(Some(v)));
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
            let role = gstr(message, "role").trim().to_lowercase();
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
            let role = gstr(content_item, "role").trim().to_lowercase();
            // In Gemini API format, missing role defaults to "user"
            if role.is_empty() || role == "user" {
                if let Some(Value::Array(parts)) = content_item.get("parts") {
                    let mut texts: Vec<String> = Vec::new();
                    for part in parts {
                        if is_gemini_thought_part(part) {
                            continue;
                        }
                        if let Some(text) = part.get("text") {
                            let val = gs(Some(text)).trim().to_string();
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

// port of extractTextContent (translator/common/claude_user_id.go)
fn extract_text_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<String> = Vec::new();
            for part in parts {
                if gstr(part, "type") == "text" {
                    if let Some(text) = part.get("text") {
                        let val = gs(Some(text)).trim().to_string();
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
    let role = gstr(item, "role").trim().to_lowercase();
    if role == "user" {
        return true;
    }
    if role == "system" || role == "developer" || role == "assistant" {
        return false;
    }
    gstr(item, "type").trim().to_lowercase() == "message"
}

// port of extractResponsesItemText (translator/common/claude_user_id.go)
fn extract_responses_item_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<String> = Vec::new();
            for part in parts {
                if matches!(
                    gstr(part, "type").as_str(),
                    "input_text" | "output_text" | "text"
                ) {
                    if let Some(text) = part.get("text") {
                        let val = gs(Some(text)).trim().to_string();
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

// ───────────────────────────── util ─────────────────────────────

// port of SanitizeClaudeToolID (util/claude_tool_id.go): regexp
// `[^a-zA-Z0-9_-]` → "_"; an empty result gets a generated fallback.
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
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Every char is ASCII now, so a byte cut is a char cut.
    s.truncate(64);
    if s.is_empty() {
        s.push('_');
    }
    s
}

// port of NormalizeClaudeToolInputSchema (util/claude_schema.go). Go
// unmarshals into map[string]json.RawMessage and marshals it back, so the
// root's keys (and the merged `properties` keys) come out sorted.
fn normalize_claude_tool_input_schema(schema: Option<&Value>) -> Value {
    let Some(Value::Object(root_in)) = schema else {
        return json!({"type": "object", "properties": {}});
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
        for branch in branches {
            let Value::Object(branch) = branch else {
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

/// Go's `json.Marshal` of a map sorts its keys.
fn sorted_object(map: Map<String, Value>) -> Value {
    let sorted: BTreeMap<String, Value> = map.into_iter().collect();
    Value::Object(sorted.into_iter().collect())
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
    match schema.get("type") {
        None => true,
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(types)) => {
            if !types.iter().all(Value::is_string) {
                return false;
            }
            types.iter().any(|t| t == "object")
        }
        _ => false,
    }
}

/// `json.Unmarshal(raw, &[]string)`: null is an empty slice, anything but an
/// array of strings is an error.
fn string_slice(raw: Option<&Value>) -> Result<Vec<String>, ()> {
    match raw {
        None => Err(()),
        Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or(()))
            .collect(),
        _ => Err(()),
    }
}

// port of mergeClaudeSchemaRequired (util/claude_schema.go)
fn merge_claude_schema_required(root: &mut Map<String, Value>, branch_required: Option<&Value>) {
    let mut required: Vec<String> = match root.get("required") {
        Some(r) => string_slice(Some(r)).unwrap_or_default(),
        None => Vec::new(),
    };
    let Ok(branch_names) = string_slice(branch_required) else {
        return;
    };
    let mut seen: HashSet<String> = required.iter().cloned().collect();
    for name in branch_names {
        if seen.contains(&name) {
            continue;
        }
        required.push(name.clone());
        seen.insert(name);
    }
    if required.is_empty() {
        return;
    }
    root.insert(
        "required".to_string(),
        Value::Array(required.into_iter().map(Value::from).collect()),
    );
}

// ───────────────────────────── thinking ─────────────────────────────

/// The parts of `registry.ModelInfo` this pair reads:
/// `(MaxCompletionTokens, Thinking: Option<(Min, Levels)>)`.
type ClaudeModelInfo = (i64, Option<(i64, Vec<String>)>);

/// Stand-in for `registry.LookupModelInfo(model, "claude")`
/// (internal/registry/model_registry.go). Termory has no model registry.
fn lookup_claude_model_info(model: &str) -> Option<ClaudeModelInfo> {
    crate::translate::claude_thinking_support(model).map(|t| (0, Some(t)))
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
fn map_to_claude_effort(level: &str, supports_max: bool) -> Option<String> {
    let level = level.trim().to_lowercase();
    match level.as_str() {
        "minimal" => Some("low".to_string()),
        "low" | "medium" | "high" => Some(level),
        // Claude has `xhigh` of its own: a client's xhigh stays xhigh,
        // only `max` is max.
        "xhigh" => Some(if supports_max { "xhigh" } else { "high" }.to_string()),
        "max" => Some(if supports_max { "max" } else { "high" }.to_string()),
        "auto" => Some("high".to_string()),
        _ => None,
    }
}

/// port of SummaryMode (thinking/summary.go); `None` is SummaryUnspecified.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SummaryMode {
    Disabled,
    Enabled,
}

// port of ApplyTranslatedSummaryToClaude (thinking/summary.go), with
// ExtractTranslatedSummaryConfig fixed to source "openai-response" and
// applySummaryConfigForProvider fixed to target "claude".
fn apply_translated_summary_to_claude(out: &mut Value, source: &Value, model: &str) {
    // port of ExtractSummaryConfig (thinking/summary.go), case "openai-response"
    let mode = responses_summary_config(source, "reasoning.summary")
        .or_else(|| responses_summary_config(source, "reasoning.generate_summary"));
    let Some(mode) = mode else {
        return;
    };

    let enabled = mode == SummaryMode::Enabled;
    if enabled && gp(out, "thinking.type").is_none() {
        enable_claude_thinking_for_summary(out, model);
    }
    if !claude_thinking_accepts_display(out) {
        return;
    }
    let value = if enabled { "summarized" } else { "omitted" };
    sj_set(out, "thinking.display", Value::from(value));
}

// port of responsesSummaryConfig (thinking/summary.go)
fn responses_summary_config(body: &Value, path: &str) -> Option<SummaryMode> {
    match gp(body, path)? {
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
    match gstr(body, "thinking.type").trim().to_lowercase().as_str() {
        "adaptive" => true,
        "enabled" => match gp(body, "thinking.budget_tokens") {
            Some(budget @ Value::Number(_)) => {
                let value = gi(Some(budget));
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
        lookup_claude_model_info(&gstr(body, "model"))
    } else {
        lookup_claude_model_info(model)
    };
    let Some((_, Some((min, levels)))) = model_info else {
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
        if gi(Some(max_tokens)) <= budget {
            return;
        }
    }
    sj_set(body, "thinking.type", Value::from("enabled"));
    sj_set(body, "thinking.budget_tokens", Value::from(budget));
}

// ───────────────────────────── signature (Claude replay) ─────────────────────────────

const MAX_CLAUDE_THINKING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
const CLAUDE_CAIS_SIGNATURE_MARKER: u8 = 0x08;
const CLAUDE_CAIS_MODEL_TEXT_PREFIX: &str = "claude-";
const SELF_DESCRIBING_SIGNATURE_FIRST_CHARS: &str = "CERg";

/// Go `base64.StdEncoding.DecodeString`: padded standard alphabet, `\r`/`\n`
/// skipped, non-zero trailing bits tolerated.
fn std_base64_decode(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    let engine = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    engine.decode(cleaned).ok()
}

// port of stripClaudeSignaturePrefix (signature/claude_validation.go)
fn strip_claude_signature_prefix(raw_signature: &str) -> String {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return String::new();
    }
    match sig.find('#') {
        Some(idx) => sig[idx + 1..].trim().to_string(),
        None => sig.to_string(),
    }
}

// port of NormalizeClaudeThinkingSignature (signature/claude_validation.go),
// answering only "is it valid?" — the R-form it returns is not needed here.
fn is_valid_claude_thinking_signature(raw_signature: &str, strict: bool) -> bool {
    let sig = strip_claude_signature_prefix(raw_signature);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return false;
    }
    match sig.as_bytes()[0] {
        b'R' => validate_claude_double_layer_signature(&sig, strict),
        b'E' => validate_claude_single_layer_signature_content(&sig, strict),
        _ => false,
    }
}

// port of NormalizeClaudeProviderNativeThinkingSignature (signature/claude_validation.go)
fn normalize_claude_provider_native_thinking_signature(
    raw_signature: &str,
    strict: bool,
) -> Option<String> {
    let sig = strip_claude_signature_prefix(raw_signature);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return None;
    }
    match sig.as_bytes()[0] {
        b'E' => validate_claude_single_layer_signature_content(&sig, strict).then_some(sig),
        b'R' => {
            if !validate_claude_double_layer_signature(&sig, strict) {
                return None;
            }
            let decoded = std_base64_decode(&sig)?;
            String::from_utf8(decoded).ok()
        }
        _ => None,
    }
}

// port of validateClaudeDoubleLayerSignature (signature/claude_validation.go)
fn validate_claude_double_layer_signature(sig: &str, strict: bool) -> bool {
    let Some(decoded) = std_base64_decode(sig) else {
        return false;
    };
    if decoded.first() != Some(&b'E') {
        return false;
    }
    match std::str::from_utf8(&decoded) {
        Ok(inner) => validate_claude_single_layer_signature_content(inner, strict),
        Err(_) => false,
    }
}

// port of validateClaudeSingleLayerSignatureContent (signature/claude_validation.go)
fn validate_claude_single_layer_signature_content(sig: &str, strict: bool) -> bool {
    let Some(decoded) = std_base64_decode(sig) else {
        return false;
    };
    if decoded.first() != Some(&0x12) {
        return false;
    }
    if !strict {
        return true;
    }
    inspect_claude_signature_payload(&decoded)
}

// port of InspectClaudeSignaturePayload (signature/claude_validation.go)
fn inspect_claude_signature_payload(payload: &[u8]) -> bool {
    if payload.first() != Some(&0x12) {
        return false;
    }
    let Some(container) = extract_claude_bytes_field(payload, 2) else {
        return false;
    };
    let Some(channel_block) = extract_claude_bytes_field(container, 1) else {
        return false;
    };
    inspect_claude_channel_block(channel_block)
}

// port of inspectClaudeChannelBlock (signature/claude_validation.go), as a
// validity check: the routing / infrastructure / schema classes it records
// only feed diagnostics.
fn inspect_claude_channel_block(channel_block: &[u8]) -> bool {
    let mut have_channel_id = false;
    let ok = walk_claude_protobuf_fields(channel_block, |num, typ, raw| match num {
        1 => {
            if typ != WIRE_VARINT || consume_varint(raw).is_none() {
                return false;
            }
            have_channel_id = true;
            true
        }
        2 | 7 => typ == WIRE_VARINT && consume_varint(raw).is_some(),
        6 => {
            if typ != WIRE_BYTES {
                return false;
            }
            match consume_bytes(raw) {
                Some((model_bytes, _)) => std::str::from_utf8(model_bytes).is_ok(),
                None => false,
            }
        }
        _ => true,
    });
    ok && have_channel_id
}

// port of extractClaudeBytesField (signature/claude_validation.go)
fn extract_claude_bytes_field(msg: &[u8], field_num: u64) -> Option<&[u8]> {
    let mut value: Option<&[u8]> = None;
    let ok = walk_claude_protobuf_fields(msg, |num, typ, raw| {
        if num != field_num {
            return true;
        }
        if typ != WIRE_BYTES {
            return false;
        }
        match consume_bytes(raw) {
            Some((bytes_value, _)) => {
                value = Some(bytes_value);
                true
            }
            None => false,
        }
    });
    if !ok {
        return None;
    }
    value
}

// port of walkClaudeProtobufFields (signature/claude_validation.go)
fn walk_claude_protobuf_fields<'a>(
    msg: &'a [u8],
    mut visit: impl FnMut(u64, u64, &'a [u8]) -> bool,
) -> bool {
    let mut offset = 0;
    while offset < msg.len() {
        let Some((num, typ, n)) = consume_tag(&msg[offset..]) else {
            return false;
        };
        offset += n;
        let Some(value_len) = consume_field_value(num, typ, &msg[offset..], 0) else {
            return false;
        };
        let field_raw = &msg[offset..offset + value_len];
        if !visit(num, typ, field_raw) {
            return false;
        }
        offset += value_len;
    }
    true
}

// port of IsValidClaudeCAISSignature + InspectClaudeCAISSignature
// (signature/claude_validation.go)
fn is_valid_claude_cais_signature(raw_signature: &str) -> bool {
    let sig = strip_claude_signature_prefix(raw_signature);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return false;
    }
    if sig.as_bytes()[0] != b'C' {
        return false;
    }
    let Some(decoded) = std_base64_decode(&sig) else {
        return false;
    };
    if decoded.first() != Some(&CLAUDE_CAIS_SIGNATURE_MARKER) {
        return false;
    }

    let mut envelope_version: u64 = 0;
    let mut container: Option<&[u8]> = None;
    let ok = walk_claude_protobuf_fields(&decoded, |num, typ, raw| match num {
        1 => match decode_claude_cais_varint(raw, typ) {
            Some(v) => {
                envelope_version = v;
                true
            }
            None => false,
        },
        2 => match decode_claude_cais_bytes(raw, typ) {
            Some(v) => {
                container = Some(v);
                true
            }
            None => false,
        },
        3 => decode_claude_cais_varint(raw, typ).is_some(),
        _ => true,
    });
    if !ok {
        return false;
    }
    let Some(container) = container else {
        return false;
    };

    let mut channel_block: Option<&[u8]> = None;
    let mut container_signature_len = 0usize;
    let ok = walk_claude_protobuf_fields(container, |num, typ, raw| match num {
        1 => match decode_claude_cais_bytes(raw, typ) {
            Some(v) => {
                channel_block = Some(v);
                true
            }
            None => false,
        },
        5 => match decode_claude_cais_bytes(raw, typ) {
            Some(v) => {
                container_signature_len = v.len();
                true
            }
            None => false,
        },
        _ => true,
    });
    if !ok {
        return false;
    }
    let Some(channel_block) = channel_block else {
        return false;
    };

    let (mut have_channel_id, mut have_signature_bytes, mut have_model_text) =
        (false, false, false);
    let mut block_kind = String::new();
    let ok = walk_claude_protobuf_fields(channel_block, |num, typ, raw| match num {
        1 => {
            have_channel_id = decode_claude_cais_varint(raw, typ).is_some();
            have_channel_id
        }
        3 | 7 => decode_claude_cais_varint(raw, typ).is_some(),
        5 => match decode_claude_cais_bytes(raw, typ) {
            Some(v) if !v.is_empty() => {
                have_signature_bytes = true;
                true
            }
            _ => false,
        },
        6 => match decode_claude_cais_utf8(raw, typ) {
            Some(v) if v.starts_with(CLAUDE_CAIS_MODEL_TEXT_PREFIX) => {
                have_model_text = true;
                true
            }
            _ => false,
        },
        8 => match decode_claude_cais_utf8(raw, typ) {
            Some(v) => {
                block_kind = v.to_string();
                true
            }
            None => false,
        },
        11 => matches!(decode_claude_cais_utf8(raw, typ), Some(v) if is_canonical_uuid(v)),
        _ => true,
    });
    if !ok {
        return false;
    }
    if !have_signature_bytes && envelope_version >= 4 && container_signature_len > 0 {
        have_signature_bytes = true;
    }
    if !have_channel_id || !have_signature_bytes {
        return false;
    }
    if !have_model_text && envelope_version < 4 {
        return false;
    }
    if envelope_version >= 4 && block_kind != "thinking" && block_kind != "narration" {
        return false;
    }
    true
}

// port of decodeClaudeCAISVarint (signature/claude_validation.go)
fn decode_claude_cais_varint(raw: &[u8], typ: u64) -> Option<u64> {
    if typ != WIRE_VARINT {
        return None;
    }
    consume_varint(raw).map(|(v, _)| v)
}

// port of decodeClaudeCAISBytes (signature/claude_validation.go)
fn decode_claude_cais_bytes(raw: &[u8], typ: u64) -> Option<&[u8]> {
    if typ != WIRE_BYTES {
        return None;
    }
    consume_bytes(raw).map(|(v, _)| v)
}

// port of decodeClaudeCAISUTF8 (signature/claude_validation.go)
fn decode_claude_cais_utf8(raw: &[u8], typ: u64) -> Option<&str> {
    std::str::from_utf8(decode_claude_cais_bytes(raw, typ)?).ok()
}

// port of isCanonicalUUID (signature/claude_validation.go)
fn is_canonical_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

// port of SplitSignatureProviderPrefix + SignatureProviderFromCachePrefix
// (signature/provider_compatibility.go), answering "Claude prefix?" with the
// unprefixed payload; other known prefixes return `Some((false, _))`.
fn split_signature_provider_prefix(raw_signature: &str) -> Option<(bool, String)> {
    let (prefix, rest) = raw_signature.trim().split_once('#')?;
    let is_claude = match prefix.trim().to_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
        | "claude-code-max" | "claude_code_max" => true,
        "gemini" | "google" | "openai" | "gpt" | "codex" | "swe" | "sealed" => false,
        _ => return None,
    };
    Some((is_claude, rest.trim().to_string()))
}

// port of SignaturePayloadWithoutProviderPrefix (signature/provider_compatibility.go)
fn signature_payload_without_provider_prefix(raw_signature: &str) -> String {
    match split_signature_provider_prefix(raw_signature) {
        Some((_, unprefixed)) => unprefixed,
        None => raw_signature.trim().to_string(),
    }
}

// port of DetectSignatureProviderForBlock + signatureProviderMatchesTarget
// (signature/provider_compatibility.go), answering only "is it Claude?". The
// GPT probe that runs before the Claude ones needs a `g` first character and
// the Gemini / Kimi probes after them only run once Claude declined, so none
// of them can change the answer and they are not ported.
fn detects_as_claude(raw_signature: &str) -> bool {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return false;
    }
    if let Some((is_claude, unprefixed)) = split_signature_provider_prefix(sig) {
        return is_claude
            && (is_valid_claude_thinking_signature(&unprefixed, true)
                || is_valid_claude_cais_signature(&unprefixed));
    }
    if sig.contains('#') {
        return false;
    }
    // port of maybeSelfDescribingSignatureEnvelope
    if !SELF_DESCRIBING_SIGNATURE_FIRST_CHARS
        .as_bytes()
        .contains(&sig.as_bytes()[0])
    {
        return false;
    }
    is_valid_claude_cais_signature(sig) || is_valid_claude_thinking_signature(sig, true)
}

// port of CompatibleSignatureForProvider (signature/provider_compatibility.go)
// with target SignatureProviderClaude, through DecideSignatureCompatibility
// and normalizeCompatibleSignatureForProvider.
fn compatible_signature_for_claude(raw_signature: &str) -> Option<String> {
    if !detects_as_claude(raw_signature) {
        return None;
    }
    let payload = signature_payload_without_provider_prefix(raw_signature);
    let normalized = if is_valid_claude_cais_signature(&payload) {
        payload
    } else {
        normalize_claude_provider_native_thinking_signature(&payload, false)?
    };
    (!normalized.is_empty()).then_some(normalized)
}

// protowire (google.golang.org/protobuf/encoding/protowire) — the subset the
// Claude envelope walk needs.
const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_BYTES: u64 = 2;
const WIRE_START_GROUP: u64 = 3;
const WIRE_END_GROUP: u64 = 4;
const WIRE_FIXED32: u64 = 5;

// port of protowire.ConsumeVarint
fn consume_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &byte) in b.iter().enumerate().take(10) {
        if i == 9 {
            if byte > 1 {
                return None;
            }
            return Some((v | (u64::from(byte) << 63), 10));
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

// port of protowire.ConsumeTag (field number must be 1..=MaxInt32)
fn consume_tag(b: &[u8]) -> Option<(u64, u64, usize)> {
    let (v, n) = consume_varint(b)?;
    let num = v >> 3;
    if num < 1 || num > i32::MAX as u64 {
        return None;
    }
    Some((num, v & 7, n))
}

// port of protowire.ConsumeBytes
fn consume_bytes(b: &[u8]) -> Option<(&[u8], usize)> {
    let (m, n) = consume_varint(b)?;
    if m > (b.len() - n) as u64 {
        return None;
    }
    let m = m as usize;
    Some((&b[n..n + m], n + m))
}

// port of protowire.ConsumeFieldValue (with its group recursion limit)
fn consume_field_value(num: u64, typ: u64, b: &[u8], depth: usize) -> Option<usize> {
    match typ {
        WIRE_VARINT => consume_varint(b).map(|(_, n)| n),
        WIRE_FIXED32 => (b.len() >= 4).then_some(4),
        WIRE_FIXED64 => (b.len() >= 8).then_some(8),
        WIRE_BYTES => consume_bytes(b).map(|(_, n)| n),
        WIRE_START_GROUP => {
            if depth >= 10_000 {
                return None;
            }
            let mut offset = 0;
            loop {
                let (num2, typ2, n) = consume_tag(&b[offset..])?;
                offset += n;
                if typ2 == WIRE_END_GROUP {
                    return (num2 == num).then_some(offset);
                }
                offset += consume_field_value(num2, typ2, &b[offset..], depth + 1)?;
            }
        }
        _ => None,
    }
}

// ───────────────────────────── web search (claude_openai-responses_web_search.go) ─────────────────────────────

const CLAUDE_WEB_SEARCH_TOOL_NAME: &str = "web_search";
const RESPONSES_WEB_SEARCH_ID_PREFIX: &str = "ws_";
const CLAUDE_SERVER_TOOL_ID_PREFIX: &str = "srvtoolu_";

// port of responsesWebSearchCallID (claude_openai-responses_web_search.go)
fn responses_web_search_call_id(claude_tool_use_id: &str) -> String {
    format!("{RESPONSES_WEB_SEARCH_ID_PREFIX}{claude_tool_use_id}")
}

// port of claudeWebSearchToolUseID (claude_openai-responses_web_search.go)
fn claude_web_search_tool_use_id(responses_item_id: &str) -> String {
    let trimmed = responses_item_id.trim();
    let body = trimmed
        .strip_prefix(RESPONSES_WEB_SEARCH_ID_PREFIX)
        .unwrap_or(trimmed);
    let body = body
        .strip_prefix(CLAUDE_SERVER_TOOL_ID_PREFIX)
        .unwrap_or(body);
    // claudeServerToolIDSanitizer: `[^a-zA-Z0-9_]` → "_"
    let body: String = body
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if body.is_empty() {
        return String::new();
    }
    format!("{CLAUDE_SERVER_TOOL_ID_PREFIX}{body}")
}

// port of claudeWebSearchQuery (claude_openai-responses_web_search.go)
fn claude_web_search_query(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(input) {
        Ok(v) => gstr(&v, "query").trim().to_string(),
        Err(_) => String::new(),
    }
}

// port of claudeWebSearchResultsToResponses (claude_openai-responses_web_search.go)
fn claude_web_search_results_to_responses(content: Option<&Value>) -> Option<Value> {
    match content {
        Some(obj @ Value::Object(_)) => Some(obj.clone()),
        Some(Value::Array(entries)) => Some(Value::Array(
            entries
                .iter()
                .filter(|entry| {
                    gstr(entry, "type") == "web_search_tool_result_error"
                        || !gstr(entry, "url").trim().is_empty()
                })
                .cloned()
                .collect(),
        )),
        _ => None,
    }
}

// port of buildResponsesWebSearchCallItem (claude_openai-responses_web_search.go)
fn build_responses_web_search_call_item(
    claude_tool_use_id: &str,
    query: &str,
    results: Option<&Value>,
) -> Value {
    let mut item = json!({
        "id": responses_web_search_call_id(claude_tool_use_id),
        "type": "web_search_call",
        "status": "completed",
        "action": {"type": "search", "query": query}
    });
    if let Some(results) = results {
        item["results"] = results.clone();
    }
    item
}

// port of convertResponsesWebSearchCallToClaudeBlocks (claude_openai-responses_web_search.go)
fn convert_responses_web_search_call_to_claude_blocks(item: &Value) -> Vec<Value> {
    let tool_use_id = claude_web_search_tool_use_id(gstr(item, "id").trim());
    if tool_use_id.is_empty() {
        return Vec::new();
    }

    let mut use_block = json!({"type": "server_tool_use", "id": tool_use_id, "name": CLAUDE_WEB_SEARCH_TOOL_NAME, "input": {}});
    let query = responses_web_search_call_query(item);
    if !query.is_empty() {
        sj_set(&mut use_block, "input.query", Value::from(query));
    }

    let mut result =
        json!({"type": "web_search_tool_result", "tool_use_id": tool_use_id, "content": []});
    if let Some(content) = responses_web_search_results_to_claude(item.get("results")) {
        result["content"] = content;
    }
    vec![use_block, result]
}

// port of responsesWebSearchCallQuery (claude_openai-responses_web_search.go)
fn responses_web_search_call_query(item: &Value) -> String {
    let query = gstr(item, "action.query").trim().to_string();
    if !query.is_empty() {
        return query;
    }
    let query = gstr(item, "action.queries.0").trim().to_string();
    if !query.is_empty() {
        return query;
    }
    gstr(item, "action.url").trim().to_string()
}

// port of responsesWebSearchResultsToClaude (claude_openai-responses_web_search.go)
fn responses_web_search_results_to_claude(results: Option<&Value>) -> Option<Value> {
    match results {
        Some(obj @ Value::Object(_)) => Some(obj.clone()),
        Some(Value::Array(entries)) => {
            let mut blocks: Vec<Value> = Vec::new();
            for entry in entries {
                if gstr(entry, "type") == "web_search_tool_result_error" {
                    blocks.push(entry.clone());
                    continue;
                }
                if gstr(entry, "encrypted_content").trim().is_empty() {
                    continue;
                }
                let mut block = entry.clone();
                if block.is_object() {
                    sj_set(&mut block, "type", Value::from("web_search_result"));
                }
                blocks.push(block);
            }
            (!blocks.is_empty()).then_some(Value::Array(blocks))
        }
        _ => None,
    }
}

// port of attachClaudeCitations (claude_openai-responses_web_search.go)
fn attach_claude_citations(text_block: &mut Value, annotations: Option<&Value>) {
    let Some(Value::Array(annotations)) = annotations else {
        return;
    };
    let citations: Vec<Value> = annotations
        .iter()
        .filter(|a| !gstr(a, "encrypted_index").trim().is_empty())
        .cloned()
        .collect();
    if citations.is_empty() {
        return;
    }
    sj_set(text_block, "citations", Value::Array(citations));
}

// ───────────────────────────── request: Responses → Claude ─────────────────────────────

const DEFAULT_CLAUDE_RESPONSES_MAX_TOKENS: i64 = 32000;
const DEFAULT_FABLE_RESPONSES_MAX_TOKENS: i64 = 64000;

/// port of ClaudeResponsesRedactedThinkingPrefix (claude_openai-responses_response.go)
const CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX: &str = "claude-redacted-thinking:";

// port of ConvertOpenAIResponsesRequestToClaude (claude_openai-responses_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    convert_openai_responses_request_to_claude(model, body, stream, false)
}

// port of ConvertOpenAIResponsesRequestToClaudeWithCompat (claude_openai-responses_request.go):
// keeps reasoning items whose encrypted content is empty or foreign, for
// Anthropic-compatible endpoints that do not validate signatures.
// Kept for parity: the router uses the registered (non-compat) form.
#[allow(dead_code)]
pub fn translate_request_with_compat(model: &str, body: &Value, stream: bool) -> Value {
    convert_openai_responses_request_to_claude(model, body, stream, true)
}

/// The pending-message bookkeeping of convertOpenAIResponsesRequestToClaude
/// (claude_openai-responses_request.go): the `pendingRole` / `pendingParts` /
/// `pendingToolUseParts` locals and the closures over them.
#[derive(Default)]
struct PendingMessages {
    messages: Vec<Value>,
    role: String,
    parts: Vec<Value>,
    tool_use_parts: Vec<Value>,
}

impl PendingMessages {
    // port of the flushPendingMessage closure
    fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.parts);
        if self.role == "assistant" && !self.tool_use_parts.is_empty() {
            parts.append(&mut self.tool_use_parts);
        }
        if !parts.is_empty() {
            let content = single_text_part_or_array(parts);
            self.messages
                .push(json!({"role": self.role, "content": content}));
        }
        self.role.clear();
        self.parts.clear();
        self.tool_use_parts.clear();
    }

    // port of the appendParts closure
    fn append_parts(&mut self, role: &str, parts: Vec<Value>) {
        if role.is_empty() || parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        self.role = role.to_string();
        self.parts.extend(parts);
    }

    // port of the appendToolUse closure
    fn append_tool_use(&mut self, tool_use: Value) {
        if !self.role.is_empty() && self.role != "assistant" {
            self.flush();
        }
        self.role = "assistant".to_string();
        self.tool_use_parts.push(tool_use);
    }

    // port of the appendReasoning closure
    fn append_reasoning(&mut self, reasoning_part: Option<Value>) {
        let Some(reasoning_part) = reasoning_part else {
            return;
        };
        if !self.role.is_empty() && self.role != "assistant" {
            self.flush();
        }
        self.role = "assistant".to_string();

        // Client tool calls normally stay at the end of an assistant message, but
        // a later reasoning item makes them a real separator between thinking blocks.
        if !self.tool_use_parts.is_empty() {
            self.parts.append(&mut self.tool_use_parts);
        }

        if gstr(&reasoning_part, "type") == "thinking" {
            if let Some(last) = self.parts.last_mut() {
                if gstr(last, "type") == "thinking" {
                    *last = reasoning_part;
                    return;
                }
            }
        }
        self.parts.push(reasoning_part);
    }
}

/// The single-part shortcut shared by flushPendingMessage and
/// stripTrailingClaudeThinkingBlocks: one plain text block (no cache_control,
/// no citations) becomes a string content.
fn single_text_part_or_array(parts: Vec<Value>) -> Value {
    if parts.len() == 1 {
        let part = &parts[0];
        if gstr(part, "type") == "text"
            && part.get("cache_control").is_none()
            && part.get("citations").is_none()
        {
            return Value::from(gstr(part, "text"));
        }
    }
    Value::Array(parts)
}

fn text_part(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

// port of convertOpenAIResponsesRequestToClaude (claude_openai-responses_request.go)
fn convert_openai_responses_request_to_claude(
    model_name: &str,
    input_raw: &Value,
    stream: bool,
    preserve_empty_thinking_blocks: bool,
) -> Value {
    let raw = normalize_codex_agent_messages(input_raw);
    let root = &raw;

    let user_id = derive_claude_user_id(root);

    // Base Claude message payload
    let mut out = json!({"model": "", "max_tokens": 32000, "messages": [], "metadata": {}});
    sj_set(&mut out, "metadata.user_id", Value::from(user_id));
    sj_set(
        &mut out,
        "max_tokens",
        Value::from(default_claude_responses_max_tokens_for_model(model_name)),
    );

    // Convert OpenAI Responses reasoning.effort to Claude thinking config.
    if let Some(v) = gp(root, "reasoning.effort") {
        let mut effort = gs(Some(v)).trim().to_lowercase();
        if !effort.is_empty() {
            let thinking = lookup_claude_model_info(model_name).and_then(|(_, t)| t);
            let supports_adaptive = thinking
                .as_ref()
                .is_some_and(|(_, levels)| !levels.is_empty());
            let supports_max = supports_adaptive
                && thinking
                    .as_ref()
                    .is_some_and(|(_, levels)| has_level(levels, "max"));

            if supports_adaptive {
                match effort.as_str() {
                    // "none": `disabled` is a 400 on Opus 5.5, Sonnet 5.5
                    // and the Fable tier; adaptive at the lowest effort is
                    // accepted everywhere adaptive is.
                    "none" => {
                        sj_set(&mut out, "thinking.type", Value::from("adaptive"));
                        sj_delete(&mut out, "thinking.budget_tokens");
                        sj_set(&mut out, "output_config.effort", Value::from("low"));
                    }
                    "auto" => {
                        sj_set(&mut out, "thinking.type", Value::from("adaptive"));
                        sj_delete(&mut out, "thinking.budget_tokens");
                        sj_delete(&mut out, "output_config.effort");
                    }
                    _ => {
                        if let Some(mapped) = map_to_claude_effort(&effort, supports_max) {
                            effort = mapped;
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
                    // `enabled` needs a budget: "auto" takes the medium one.
                    -1 => {
                        sj_set(&mut out, "thinking.type", Value::from("enabled"));
                        sj_set(&mut out, "thinking.budget_tokens", Value::from(8192));
                    }
                    b if b > 0 => {
                        sj_set(&mut out, "thinking.type", Value::from("enabled"));
                        sj_set(&mut out, "thinking.budget_tokens", Value::from(b));
                    }
                    _ => {}
                }
            }
        }
    }

    // Model
    sj_set(&mut out, "model", Value::from(model_name));

    // Max tokens
    if let Some(mot) = root.get("max_output_tokens") {
        if !mot.is_null() {
            let mut val = gi(Some(mot));
            if let Some((max_completion, _)) = lookup_claude_model_info(model_name) {
                if max_completion > 0 && val > max_completion {
                    val = max_completion;
                }
            }
            sj_set(&mut out, "max_tokens", Value::from(val));
        }
    }

    // Stream
    sj_set(&mut out, "stream", Value::from(stream));

    // Service Tier -> Speed
    if let Some(Value::String(st)) = root.get("service_tier") {
        if st == "priority" {
            sj_set(&mut out, "speed", Value::from("fast"));
        }
    }

    // System-level inputs become canonical top-level Claude system blocks in
    // source order: instructions first, then every input item whose role is
    // system or developer.
    let mut system_blocks: Vec<Value> = Vec::new();
    if let Some(Value::String(instr)) = root.get("instructions") {
        append_system_text(&mut system_blocks, instr, None);
    }
    if let Some(Value::Array(input)) = root.get("input") {
        for item in input {
            if !is_responses_system_level_role(&gstr(item, "role")) {
                continue;
            }
            let start_idx = system_blocks.len();
            match item.get("content") {
                Some(Value::String(content)) => {
                    append_system_text(&mut system_blocks, content, None)
                }
                Some(Value::Array(parts)) => {
                    for part in parts {
                        match gstr(part, "type").as_str() {
                            "input_text" | "output_text" | "text" => {
                                append_system_text(
                                    &mut system_blocks,
                                    &gstr(part, "text"),
                                    Some(part),
                                );
                            }
                            _ => {
                                if let Some(block) = responses_system_unsupported_block(part) {
                                    system_blocks.push(block);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            // Item-level cache_control applies to the last block this item produced.
            if item.get("cache_control").is_some() && system_blocks.len() > start_idx {
                if let Some(last) = system_blocks.last_mut() {
                    if last.get("cache_control").is_none() {
                        attach_cache_control(last, Some(item));
                    }
                }
            }
        }
    }

    let format_result = gp(root, "text.format").or_else(|| root.get("response_format"));
    let format_instruction = build_claude_structured_output_instruction(format_result);
    if !format_instruction.is_empty() {
        append_system_text(&mut system_blocks, &format_instruction, None);
    }

    // input array processing
    let mut pending = PendingMessages::default();

    let mut input_items: Vec<Value> = Vec::new();
    match root.get("input") {
        Some(Value::Array(items)) => input_items = normalize_responses_tool_call_outputs(items),
        Some(Value::String(text)) => pending.append_parts("user", vec![text_part(text)]),
        _ => {}
    }

    let mut last_tool_result: HashMap<String, Value> = HashMap::new();
    for item in &input_items {
        if is_tool_output_type(item) {
            let raw_id = extract_responses_call_id(item);
            if !raw_id.is_empty() {
                last_tool_result.insert(raw_id, item.clone());
            }
        }
    }
    let mut emitted_tool_results: HashSet<String> = HashSet::new();
    let mut emitted_raw_tool_uses: HashSet<String> = HashSet::new();

    for item in &input_items {
        // System-level items already became top-level system blocks.
        if is_responses_system_level_role(&gstr(item, "role")) {
            continue;
        }
        let mut typ = gstr(item, "type");
        if typ.is_empty() && !gstr(item, "role").is_empty() {
            typ = "message".to_string();
        }
        match typ.as_str() {
            "message" => {
                // Determine role and construct Claude-compatible content parts.
                let mut role = String::new();
                let mut parts_json: Vec<Value> = Vec::new();
                match item.get("content") {
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            let ptype = gstr(part, "type");
                            match ptype.as_str() {
                                "input_text" | "output_text" => {
                                    if let Some(t) = part.get("text") {
                                        let mut content_part = text_part(&gs(Some(t)));
                                        attach_claude_citations(
                                            &mut content_part,
                                            part.get("annotations"),
                                        );
                                        attach_cache_control(&mut content_part, Some(part));
                                        parts_json.push(content_part);
                                    }
                                    role = if ptype == "input_text" {
                                        "user".to_string()
                                    } else {
                                        "assistant".to_string()
                                    };
                                }
                                "refusal" => {
                                    // Claude has no refusal block; the text keeps the turn intact.
                                    let refusal = gstr(part, "refusal");
                                    if !refusal.is_empty() {
                                        let mut content_part = text_part(&refusal);
                                        attach_cache_control(&mut content_part, Some(part));
                                        parts_json.push(content_part);
                                    }
                                    role = "assistant".to_string();
                                }
                                "input_image" => {
                                    if let Some(mut content_part) =
                                        convert_responses_image_part(part)
                                    {
                                        attach_cache_control(&mut content_part, Some(part));
                                        parts_json.push(content_part);
                                        if role.is_empty() {
                                            role = "user".to_string();
                                        }
                                    }
                                }
                                "input_file" => {
                                    if let Some(mut content_part) =
                                        convert_responses_file_part(part)
                                    {
                                        attach_cache_control(&mut content_part, Some(part));
                                        parts_json.push(content_part);
                                        if role.is_empty() {
                                            role = "user".to_string();
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Value::String(s)) if !s.is_empty() => parts_json.push(text_part(s)),
                    _ => {}
                }

                // Fallback to given role if content types not decisive
                if role.is_empty() {
                    let r = gstr(item, "role");
                    role = match r.as_str() {
                        "user" | "assistant" => r,
                        _ => "user".to_string(),
                    };
                }

                if let Some(last) = parts_json.last_mut() {
                    if last.get("cache_control").is_none() {
                        attach_cache_control(last, Some(item));
                    }
                }
                if !parts_json.is_empty() {
                    pending.append_parts(&role, parts_json);
                }
            }

            "web_search_call" => {
                // Rebuild the Claude server-side search pair so the replayed turn
                // still shows the search and its hits.
                let blocks = convert_responses_web_search_call_to_claude_blocks(item);
                if !blocks.is_empty() {
                    pending.append_parts("assistant", blocks);
                }
            }

            "reasoning" => {
                pending.append_reasoning(convert_responses_reasoning_to_claude_thinking(
                    item,
                    preserve_empty_thinking_blocks,
                ));
            }

            "function_call" | "custom_tool_call" => {
                // Map to assistant tool_use. Freeform custom input is wrapped in an
                // object because Claude tool_use input must be a JSON object.
                let raw_call_id = extract_responses_call_id(item);
                let mut call_id = raw_call_id.clone();
                if call_id.is_empty() {
                    call_id = generate_claude_tool_call_id();
                }
                let call_id = sanitize_claude_tool_id(&call_id);
                if !raw_call_id.is_empty() {
                    emitted_raw_tool_uses.insert(raw_call_id);
                }
                let mut name = gstr(item, "name");
                let namespace_name = gstr(item, "namespace").trim().to_string();
                if !namespace_name.is_empty() {
                    // Rebuild the qualified name emitted by the previous Responses turn.
                    name = qualify_responses_namespace_tool_name(&namespace_name, &name);
                }

                let mut tool_use = json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": sanitize_claude_function_name(&name),
                    "input": {}
                });
                if typ == "custom_tool_call" {
                    sj_set(
                        &mut tool_use,
                        "input.input",
                        Value::from(gstr(item, "input")),
                    );
                } else {
                    let args_str = gstr(item, "arguments");
                    if !args_str.is_empty() {
                        if let Ok(args @ Value::Object(_)) =
                            serde_json::from_str::<Value>(&args_str)
                        {
                            tool_use["input"] = args;
                        }
                    }
                }

                pending.append_tool_use(tool_use);
            }

            "function_call_output" | "custom_tool_call_output" => {
                // Map to user tool_result
                let raw_id = extract_responses_call_id(item);
                if !raw_id.is_empty() {
                    if emitted_tool_results.contains(&raw_id) {
                        continue;
                    }
                    emitted_tool_results.insert(raw_id.clone());
                }
                let mut output = item.get("output");
                if !raw_id.is_empty() {
                    if let Some(last_item) = last_tool_result.get(&raw_id) {
                        output = last_item.get("output");
                    }
                }
                // Standalone outputs (no call_id, or one that never paired with a
                // function_call in this input) have no tool_use to attach to.
                // Claude rejects orphan tool_result blocks, so surface them as
                // plain user text instead.
                if raw_id.is_empty() || !emitted_raw_tool_uses.contains(&raw_id) {
                    pending.append_parts(
                        "user",
                        convert_responses_standalone_tool_output_to_claude_text(output),
                    );
                    continue;
                }
                let call_id = sanitize_claude_tool_id(&raw_id);
                let mut tool_result =
                    json!({"type": "tool_result", "tool_use_id": call_id, "content": ""});
                apply_responses_tool_result_content(&mut tool_result, output);

                pending.append_parts("user", vec![tool_result]);
            }

            // Reachability guard in Go: it only counts the dropped types for a
            // log line (logging is out of scope).
            _ => {}
        }
    }
    pending.flush();
    let mut message_blocks = pending.messages;

    let had_messages = !message_blocks.is_empty();
    if !preserve_empty_thinking_blocks {
        message_blocks = strip_trailing_claude_thinking_blocks(message_blocks);
    }
    // Answer dangling tool_use blocks before the prefill check below so an
    // interrupted turn ends with a synthesized user tool_result instead of a
    // rejected assistant prefill on models that disallow it.
    message_blocks = repair_claude_tool_pairing(message_blocks);
    if !preserve_empty_thinking_blocks {
        message_blocks = drop_unsupported_claude_assistant_prefill(model_name, message_blocks);
    }
    // Preserve a minimal conversational turn for system-only inputs or when messages became empty
    // so downstream validation still sees a Claude-shaped request.
    if message_blocks.is_empty() && (!system_blocks.is_empty() || had_messages) {
        message_blocks.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }
    out["messages"] = Value::Array(message_blocks);
    if !system_blocks.is_empty() {
        sj_set(&mut out, "system", Value::Array(system_blocks));
    }

    // Responses Lite puts tool definitions in input[].additional_tools. Select
    // one winner for each final name, while keeping the original order for the
    // tools that survive conversion.
    let descriptors = responses_tool_descriptors(root);
    let winners = responses_tool_winners(&descriptors);
    let mut included_tool_names: HashSet<String> = HashSet::new();
    let mut tool_items: Vec<Value> = Vec::new();
    for descriptor in &descriptors {
        match winners.get(&descriptor.name) {
            Some(&w) if descriptors[w].order == descriptor.order => {}
            _ => continue,
        }
        let Some(t_json) = convert_responses_tool_descriptor_to_claude(descriptor) else {
            continue;
        };
        let tool_name = gstr(&t_json, "name");
        if !tool_name.is_empty() {
            included_tool_names.insert(tool_name);
        }
        tool_items.push(t_json);
    }
    let tool_name_map = responses_tool_name_map(&descriptors, &winners, &included_tool_names);
    if !tool_items.is_empty() {
        sj_set(&mut out, "tools", Value::Array(tool_items));
    }

    // Map tool_choice similar to Chat Completions translator
    match root.get("tool_choice") {
        Some(Value::String(choice)) => match choice.as_str() {
            "auto" => sj_set(&mut out, "tool_choice", json!({"type": "auto"})),
            "required" if !included_tool_names.is_empty() => {
                sj_set(&mut out, "tool_choice", json!({"type": "any"}));
            }
            // "none": leave unset; implies no tools
            _ => {}
        },
        Some(tool_choice @ (Value::Object(_) | Value::Array(_))) => {
            let choice_type = gstr(tool_choice, "type");
            if choice_type == "function" || choice_type == "custom" {
                let mut fn_name = gstr(tool_choice, "function.name");
                if fn_name.is_empty() {
                    fn_name = gstr(tool_choice, "custom.name");
                }
                if fn_name.is_empty() {
                    fn_name = gstr(tool_choice, "name");
                }
                let mut namespace_name = gstr(tool_choice, "namespace");
                if namespace_name.is_empty() {
                    namespace_name = gstr(tool_choice, "function.namespace");
                }
                if namespace_name.is_empty() {
                    namespace_name = gstr(tool_choice, "custom.namespace");
                }
                if !namespace_name.is_empty() {
                    fn_name = qualify_responses_namespace_tool_name(&namespace_name, &fn_name);
                }
                if let Some(mapped) = tool_name_map.get(&fn_name) {
                    if !mapped.is_empty() {
                        fn_name = mapped.clone();
                    }
                }
                if included_tool_names.contains(&fn_name) {
                    sj_set(
                        &mut out,
                        "tool_choice",
                        json!({"name": sanitize_claude_function_name(&fn_name), "type": "tool"}),
                    );
                }
            }
        }
        _ => {}
    }

    apply_translated_summary_to_claude(&mut out, &raw, model_name);
    crate::translate::clamp_claude_budget(&mut out);
    out
}

// port of the appendSystemText closure (claude_openai-responses_request.go)
fn append_system_text(system_blocks: &mut Vec<Value>, text: &str, cache_source: Option<&Value>) {
    if text.is_empty() {
        return;
    }
    let mut block = text_part(text);
    if cache_source.is_some() {
        attach_cache_control(&mut block, cache_source);
    }
    system_blocks.push(block);
}

// port of defaultClaudeResponsesMaxTokensForModel (claude_openai-responses_request.go)
fn default_claude_responses_max_tokens_for_model(model_name: &str) -> i64 {
    let mut max_tokens = DEFAULT_CLAUDE_RESPONSES_MAX_TOKENS;
    if model_name.trim().to_lowercase().contains("fable") {
        max_tokens = DEFAULT_FABLE_RESPONSES_MAX_TOKENS;
    }
    if let Some((max_completion, _)) = lookup_claude_model_info(model_name) {
        if max_completion > 0 && max_completion < max_tokens {
            return max_completion;
        }
    }
    max_tokens
}

// port of isResponsesSystemLevelRole (claude_openai-responses_request.go)
fn is_responses_system_level_role(role: &str) -> bool {
    matches!(role.trim().to_lowercase().as_str(), "system" | "developer")
}

// port of dropUnsupportedClaudeAssistantPrefill (claude_openai-responses_request.go)
fn drop_unsupported_claude_assistant_prefill(
    model_name: &str,
    mut messages: Vec<Value>,
) -> Vec<Value> {
    if !claude_model_rejects_assistant_prefill(model_name) || messages.is_empty() {
        return messages;
    }
    let last = &messages[messages.len() - 1];
    if !gstr(last, "role").trim().eq_ignore_ascii_case("assistant") {
        return messages;
    }
    messages.pop();
    messages
}

// port of stripTrailingClaudeThinkingBlocks (claude_openai-responses_request.go)
fn strip_trailing_claude_thinking_blocks(mut messages: Vec<Value>) -> Vec<Value> {
    let Some(last) = messages.last() else {
        return messages;
    };
    if !gstr(last, "role").trim().eq_ignore_ascii_case("assistant") {
        return messages;
    }
    let Some(Value::Array(parts)) = last.get("content") else {
        return messages;
    };
    let mut end = parts.len();
    while end > 0 {
        let part_type = gstr(&parts[end - 1], "type").trim().to_string();
        if part_type == "thinking" || part_type == "redacted_thinking" {
            end -= 1;
        } else {
            break;
        }
    }
    if end == parts.len() {
        return messages;
    }
    if end == 0 {
        messages.pop();
        return messages;
    }
    let remaining: Vec<Value> = parts[..end].to_vec();
    let content = single_text_part_or_array(remaining);
    let last_idx = messages.len() - 1;
    sj_set(&mut messages[last_idx], "content", content);
    messages
}

// port of claudeModelRejectsAssistantPrefill (claude_openai-responses_request.go)
fn claude_model_rejects_assistant_prefill(model_name: &str) -> bool {
    let normalized = model_name.trim().to_lowercase();
    ["fable", "opus-5", "sonnet-4-6"]
        .iter()
        .any(|family| normalized.contains(family))
}

// port of responsesSystemUnsupportedBlock (claude_openai-responses_request.go)
fn responses_system_unsupported_block(part: &Value) -> Option<Value> {
    let part_type = gstr(part, "type").trim().to_string();
    if part_type.is_empty() {
        return None;
    }
    Some(json!({"type": part_type}))
}

// port of convertResponsesReasoningToClaudeThinking (claude_openai-responses_request.go)
fn convert_responses_reasoning_to_claude_thinking(
    item: &Value,
    preserve_empty: bool,
) -> Option<Value> {
    let encrypted = gstr(item, "encrypted_content");
    if let Some(data) = responses_redacted_thinking_data(&encrypted) {
        if data.is_empty() {
            return None;
        }
        return Some(json!({"type": "redacted_thinking", "data": data}));
    }

    let signature = match compatible_signature_for_claude(&encrypted) {
        Some(sig) => sig,
        None => {
            if !preserve_empty {
                return None;
            }
            encrypted
        }
    };

    let thinking_text = responses_reasoning_text(item);
    Some(json!({"type": "thinking", "thinking": thinking_text, "signature": signature}))
}

// port of responsesRedactedThinkingData (claude_openai-responses_request.go)
fn responses_redacted_thinking_data(encrypted_content: &str) -> Option<String> {
    let trimmed = encrypted_content.trim();
    let rest = trimmed.strip_prefix(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX)?;
    Some(rest.trim().to_string())
}

// port of responsesReasoningText (claude_openai-responses_request.go)
fn responses_reasoning_text(item: &Value) -> String {
    let text = responses_reasoning_parts_text(item.get("summary"));
    if !text.is_empty() {
        return text;
    }
    responses_reasoning_parts_text(item.get("content"))
}

// port of responsesReasoningPartsText (claude_openai-responses_request.go)
fn responses_reasoning_parts_text(parts: Option<&Value>) -> String {
    let Some(Value::Array(parts)) = parts else {
        return String::new();
    };
    let mut builder = String::new();
    for part in parts {
        if let Some(text) = part.get("text") {
            builder.push_str(&gs(Some(text)));
        } else if let Value::String(s) = part {
            builder.push_str(s);
        }
    }
    builder
}

// port of applyResponsesToolResultContent (claude_openai-responses_request.go)
fn apply_responses_tool_result_content(tool_result: &mut Value, output: Option<&Value>) {
    if let Some(Value::Array(output_parts)) = output {
        let mut parts_json: Vec<Value> = Vec::new();
        let mut has_image = false;
        let mut has_file = false;
        for part in output_parts {
            if let Some(part_json) = convert_responses_content_part_to_claude(part) {
                match gstr(&part_json, "type").as_str() {
                    "image" => has_image = true,
                    "document" => has_file = true,
                    _ => {}
                }
                parts_json.push(part_json);
            }
        }
        if parts_json.is_empty() {
            sj_set(
                tool_result,
                "content",
                Value::from(Value::Array(output_parts.clone()).to_string()),
            );
            return;
        }
        if parts_json.len() == 1
            && !has_image
            && !has_file
            && gstr(&parts_json[0], "type") == "text"
        {
            let text = gstr(&parts_json[0], "text");
            sj_set(tool_result, "content", Value::from(text));
            return;
        }
        sj_delete(tool_result, "content");
        sj_set(tool_result, "content", Value::Array(parts_json));
        return;
    }
    sj_set(tool_result, "content", Value::from(gs(output)));
}

// port of repairClaudeToolPairing (claude_openai-responses_request.go)
fn repair_claude_tool_pairing(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }
    let mut prev_tool_use_ids: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    let mut i = 0;
    while i < messages.len() {
        let mut msg = messages[i].clone();
        let role = gstr(&msg, "role");

        if role == "user" {
            // Fold tool_result blocks that do not answer a tool_use in the
            // immediately preceding assistant message into plain text, and move
            // the real ones ahead of any other content.
            if let Some(rebuilt) = normalize_claude_tool_result_message(&msg, &prev_tool_use_ids) {
                msg = rebuilt;
                messages[i] = msg.clone();
            }
        }

        out.push(msg.clone());

        prev_tool_use_ids = HashSet::new();
        if role == "assistant" {
            let mut tool_use_ids: Vec<String> = Vec::new();
            for block in content_blocks(msg.get("content")) {
                if gstr(block, "type") == "tool_use" {
                    let id = gstr(block, "id");
                    if !id.is_empty() {
                        tool_use_ids.push(id.clone());
                        prev_tool_use_ids.insert(id);
                    }
                }
            }
            if tool_use_ids.is_empty() {
                i += 1;
                continue;
            }

            let has_next_user = i + 1 < messages.len() && gstr(&messages[i + 1], "role") == "user";
            let mut answered: HashSet<String> = HashSet::new();
            if has_next_user {
                for block in content_blocks(messages[i + 1].get("content")) {
                    if gstr(block, "type") == "tool_result" {
                        answered.insert(gstr(block, "tool_use_id"));
                    }
                }
            }

            let mut synthesized: Vec<Value> = Vec::new();
            for id in &tool_use_ids {
                if answered.contains(id) {
                    continue;
                }
                synthesized.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "is_error": true,
                    "content": "Tool call was interrupted before any output was recorded."
                }));
            }
            if synthesized.is_empty() {
                i += 1;
                continue;
            }

            if has_next_user {
                // Prepend the missing results so tool_result blocks still lead
                // the existing user message.
                let mut parts = synthesized;
                match messages[i + 1].get("content") {
                    Some(Value::Array(blocks)) => parts.extend(blocks.iter().cloned()),
                    Some(Value::String(s)) => parts.push(text_part(s)),
                    _ => {}
                }
                messages[i + 1] = json!({"role": "user", "content": parts});
            } else {
                out.push(json!({"role": "user", "content": synthesized}));
            }
        }
        i += 1;
    }
    out
}

/// gjson `ForEach` over a message content: an array yields its elements, any
/// other existing value yields itself once.
fn content_blocks(content: Option<&Value>) -> Vec<&Value> {
    match content {
        Some(Value::Array(blocks)) => blocks.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

// port of normalizeClaudeToolResultMessage (claude_openai-responses_request.go);
// `None` is Go's `changed == false`.
fn normalize_claude_tool_result_message(
    msg: &Value,
    answered_ids: &HashSet<String>,
) -> Option<Value> {
    let Some(Value::Array(content)) = msg.get("content") else {
        return None;
    };
    let mut result_parts: Vec<Value> = Vec::new();
    let mut other_parts: Vec<Value> = Vec::new();
    let mut seen_other = false;
    let mut changed = false;
    for block in content {
        if gstr(block, "type") == "tool_result" {
            if !answered_ids.contains(&gstr(block, "tool_use_id")) {
                changed = true;
                seen_other = true;
                let text_parts = tool_result_text_parts(block);
                if !text_parts.is_empty() {
                    other_parts.extend(text_parts);
                } else {
                    // An empty orphan result folds to nothing, so keep an
                    // explicit marker instead of producing an empty user
                    // message that Anthropic also rejects.
                    other_parts.push(text_part("Tool result was empty."));
                }
                continue;
            }
            result_parts.push(block.clone());
            if seen_other {
                changed = true;
            }
            continue;
        }
        seen_other = true;
        other_parts.push(block.clone());
    }
    if !changed {
        return None;
    }
    let mut parts = result_parts;
    parts.extend(other_parts);
    Some(json!({"role": "user", "content": parts}))
}

// port of toolResultTextParts (claude_openai-responses_request.go)
fn tool_result_text_parts(block: &Value) -> Vec<Value> {
    let content = block.get("content");
    if let Some(Value::Array(parts)) = content {
        let mut out: Vec<Value> = Vec::new();
        for part in parts {
            let mut raw = part.clone();
            if gstr(part, "type").is_empty() && raw.is_object() {
                sj_set(&mut raw, "type", Value::from("text"));
            }
            if !content_part_has_visible_content(&raw) {
                continue;
            }
            out.push(raw);
        }
        return out;
    }
    let text = gs(content);
    if text.trim().is_empty() {
        return Vec::new();
    }
    vec![text_part(&text)]
}

// port of convertResponsesStandaloneToolOutputToClaudeText (claude_openai-responses_request.go)
fn convert_responses_standalone_tool_output_to_claude_text(output: Option<&Value>) -> Vec<Value> {
    if let Some(Value::Array(parts)) = output {
        let parts_json: Vec<Value> = parts
            .iter()
            .filter_map(convert_responses_content_part_to_claude)
            // Drop empty text parts so a mixed array never emits blocks
            // Anthropic rejects; images and documents always count.
            .filter(content_part_has_visible_content)
            .collect();
        if !parts_json.is_empty() {
            return parts_json;
        }
    }
    let text = gs(output);
    if matches!(output, Some(Value::Array(_))) || text.trim().is_empty() {
        // An empty standalone output still needs a block so the user message
        // it joins never degenerates to an empty content array.
        return vec![text_part("Tool result was empty.")];
    }
    vec![text_part(&text)]
}

// port of contentPartHasVisibleContent (claude_openai-responses_request.go)
fn content_part_has_visible_content(part: &Value) -> bool {
    match gstr(part, "type").as_str() {
        "text" => !gstr(part, "text").trim().is_empty(),
        // Non-text blocks (image, document, ...) always carry content.
        _ => true,
    }
}

/// `data:<media>;base64,<data>` split shared by the image / file branches.
fn split_data_url(trimmed: &str) -> Option<(String, String)> {
    let (media, data) = trimmed.split_once(";base64,")?;
    Some((media.to_string(), data.to_string()))
}

/// The `input_image` branch shared by the message loop and
/// convertResponsesContentPartToClaude (claude_openai-responses_request.go).
fn convert_responses_image_part(part: &Value) -> Option<Value> {
    let mut url = gstr(part, "image_url");
    if url.is_empty() {
        url = gstr(part, "url");
    }
    if url.is_empty() {
        return None;
    }
    if let Some(trimmed) = url.strip_prefix("data:") {
        let mut media_type = "application/octet-stream".to_string();
        let mut data = String::new();
        if let Some((media, d)) = split_data_url(trimmed) {
            if !media.is_empty() {
                media_type = media;
            }
            data = d;
        }
        if data.is_empty() {
            return None;
        }
        return Some(
            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}),
        );
    }
    Some(json!({"type": "image", "source": {"type": "url", "url": url}}))
}

/// The `input_file` branch shared by the message loop and
/// convertResponsesContentPartToClaude (claude_openai-responses_request.go).
fn convert_responses_file_part(part: &Value) -> Option<Value> {
    let file_data = gstr(part, "file_data");
    if file_data.is_empty() {
        return None;
    }
    let mut media_type = "application/octet-stream".to_string();
    let mut data = file_data.clone();
    if let Some(trimmed) = file_data.strip_prefix("data:") {
        if let Some((media, d)) = split_data_url(trimmed) {
            if !media.is_empty() {
                media_type = media;
            }
            data = d;
        }
    }
    Some(
        json!({"type": "document", "source": {"type": "base64", "media_type": media_type, "data": data}}),
    )
}

// port of convertResponsesContentPartToClaude (claude_openai-responses_request.go)
fn convert_responses_content_part_to_claude(part: &Value) -> Option<Value> {
    match gstr(part, "type").as_str() {
        "input_text" | "output_text" => part.get("text").map(|t| text_part(&gs(Some(t)))),
        "input_image" => convert_responses_image_part(part),
        "input_file" => convert_responses_file_part(part),
        _ => None,
    }
}

// port of isOpenAIResponsesApplyPatchCustomTool (claude_openai-responses_request.go)
fn is_openai_responses_apply_patch_custom_tool(tool_type: &str, tool: &Value) -> bool {
    tool_type == "custom" && gstr(tool, "name").trim() == "apply_patch"
}

// port of convertResponsesToolDescriptorToClaude (claude_openai-responses_request.go)
fn convert_responses_tool_descriptor_to_claude(descriptor: &ToolDescriptor) -> Option<Value> {
    let override_name = if descriptor.direct {
        ""
    } else {
        descriptor.name.as_str()
    };
    match descriptor.tool_type.as_str() {
        "function" => convert_responses_function_tool_to_claude(&descriptor.tool, override_name),
        "custom" => convert_responses_custom_tool_to_claude(&descriptor.tool, override_name),
        "web_search" => convert_responses_web_search_tool_to_claude(&descriptor.tool),
        other => {
            if is_unsupported_openai_builtin_tool_type(other)
                || gstr(&descriptor.tool, "name").is_empty()
            {
                return None;
            }
            Some(descriptor.tool.clone())
        }
    }
}

/// port of responsesToolDescriptor (claude_openai-responses_request.go)
struct ToolDescriptor {
    name: String,
    child_name: String,
    namespace: String,
    tool_type: String,
    tool: Value,
    source_priority: i32,
    direct: bool,
    order: usize,
}

// port of responsesToolSources (claude_openai-responses_request.go)
fn responses_tool_sources(root: &Value) -> Vec<(&Vec<Value>, i32)> {
    let mut sources: Vec<(&Vec<Value>, i32)> = Vec::new();
    if let Some(Value::Array(tools)) = root.get("tools") {
        sources.push((tools, 0));
    }
    if let Some(Value::Array(input)) = root.get("input") {
        for item in input {
            if gstr(item, "type") == "additional_tools" {
                if let Some(Value::Array(tools)) = item.get("tools") {
                    sources.push((tools, 1));
                }
            }
        }
    }
    sources
}

// port of responsesToolDescriptors (claude_openai-responses_request.go)
fn responses_tool_descriptors(root: &Value) -> Vec<ToolDescriptor> {
    let mut descriptors: Vec<ToolDescriptor> = Vec::new();
    #[allow(clippy::too_many_arguments)]
    fn append_descriptor(
        descriptors: &mut Vec<ToolDescriptor>,
        tool: &Value,
        name: String,
        child_name: String,
        namespace: String,
        tool_type: &str,
        source_priority: i32,
        direct: bool,
    ) {
        if name.is_empty() {
            return;
        }
        let order = descriptors.len();
        descriptors.push(ToolDescriptor {
            name,
            child_name,
            namespace,
            tool_type: tool_type.to_string(),
            tool: tool.clone(),
            source_priority,
            direct,
            order,
        });
    }

    for (tools, priority) in responses_tool_sources(root) {
        for tool in tools {
            let tool_type = gstr(tool, "type").trim().to_string();
            match tool_type.as_str() {
                "" | "function" => append_descriptor(
                    &mut descriptors,
                    tool,
                    responses_tool_name(tool),
                    String::new(),
                    String::new(),
                    "function",
                    priority,
                    true,
                ),
                "custom" => {
                    if !is_openai_responses_apply_patch_custom_tool("custom", tool) {
                        append_descriptor(
                            &mut descriptors,
                            tool,
                            responses_tool_name(tool),
                            String::new(),
                            String::new(),
                            "custom",
                            priority,
                            true,
                        );
                    }
                }
                "namespace" => {
                    // port of the appendNamespaceChildren closure
                    let namespace_name = gstr(tool, "name").trim().to_string();
                    let Some(Value::Array(children)) = tool.get("tools") else {
                        continue;
                    };
                    for child in children {
                        let child_name = responses_tool_name(child);
                        if child_name.is_empty() {
                            continue;
                        }
                        let qualified_name =
                            qualify_responses_namespace_tool_name(&namespace_name, &child_name);
                        match gstr(child, "type").trim() {
                            "" | "function" => append_descriptor(
                                &mut descriptors,
                                child,
                                qualified_name,
                                child_name,
                                namespace_name.clone(),
                                "function",
                                priority,
                                false,
                            ),
                            "custom"
                                if !is_openai_responses_apply_patch_custom_tool(
                                    "custom", child,
                                ) =>
                            {
                                append_descriptor(
                                    &mut descriptors,
                                    child,
                                    qualified_name,
                                    child_name,
                                    namespace_name.clone(),
                                    "custom",
                                    priority,
                                    false,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                "web_search" => {
                    if let Some(external) = tool.get("external_web_access") {
                        if !gb(Some(external)) {
                            continue;
                        }
                    }
                    let mut name = gstr(tool, "name").trim().to_string();
                    if name.is_empty() {
                        name = "web_search".to_string();
                    }
                    append_descriptor(
                        &mut descriptors,
                        tool,
                        name,
                        String::new(),
                        String::new(),
                        "web_search",
                        priority,
                        true,
                    );
                }
                other => {
                    if is_unsupported_openai_builtin_tool_type(other) {
                        continue;
                    }
                    append_descriptor(
                        &mut descriptors,
                        tool,
                        gstr(tool, "name").trim().to_string(),
                        String::new(),
                        String::new(),
                        other,
                        priority,
                        true,
                    );
                }
            }
        }
    }
    descriptors
}

// port of responsesToolDescriptorPrecedes (claude_openai-responses_request.go)
fn responses_tool_descriptor_precedes(left: &ToolDescriptor, right: &ToolDescriptor) -> bool {
    // Keep top-level tools ahead of additional_tools, then let direct
    // declarations win over namespace children within the same source class.
    if left.source_priority != right.source_priority {
        return left.source_priority < right.source_priority;
    }
    if left.direct != right.direct {
        return left.direct;
    }
    left.order < right.order
}

// port of responsesToolWinners (claude_openai-responses_request.go): final
// name → index of the winning descriptor.
fn responses_tool_winners(descriptors: &[ToolDescriptor]) -> HashMap<String, usize> {
    let mut winners: HashMap<String, usize> = HashMap::new();
    for (i, descriptor) in descriptors.iter().enumerate() {
        match winners.get(&descriptor.name) {
            Some(&current)
                if !responses_tool_descriptor_precedes(descriptor, &descriptors[current]) => {}
            _ => {
                winners.insert(descriptor.name.clone(), i);
            }
        }
    }
    winners
}

// port of responsesToolNameMap (claude_openai-responses_request.go)
fn responses_tool_name_map(
    descriptors: &[ToolDescriptor],
    winners: &HashMap<String, usize>,
    accepted_tool_names: &HashSet<String>,
) -> HashMap<String, String> {
    let mut tool_name_map: HashMap<String, String> = HashMap::new();
    let is_winner = |d: &ToolDescriptor| {
        winners
            .get(&d.name)
            .is_some_and(|&w| descriptors[w].order == d.order)
    };

    // Direct tool names are canonical aliases and must win over namespace
    // child aliases, regardless of declaration order.
    for descriptor in descriptors {
        if !is_winner(descriptor) || !descriptor.direct {
            continue;
        }
        if !accepted_tool_names.contains(&descriptor.name) {
            continue;
        }
        tool_name_map.insert(descriptor.name.clone(), descriptor.name.clone());
    }

    // Namespace aliases fill only names that are not already owned by a
    // winning direct function/custom tool.
    for descriptor in descriptors {
        if !is_winner(descriptor) || descriptor.direct || descriptor.child_name.is_empty() {
            continue;
        }
        if !accepted_tool_names.contains(&descriptor.name) {
            continue;
        }
        if tool_name_map.contains_key(&descriptor.child_name) {
            continue;
        }
        tool_name_map.insert(descriptor.child_name.clone(), descriptor.name.clone());
    }
    tool_name_map
}

// port of responsesCustomToolNames (claude_openai-responses_request.go)
fn responses_custom_tool_names(request: Option<&Value>) -> HashSet<String> {
    let Some(root) = request else {
        return HashSet::new();
    };
    let descriptors = responses_tool_descriptors(root);
    responses_tool_winners(&descriptors)
        .into_iter()
        .filter(|(_, w)| descriptors[*w].tool_type == "custom")
        .map(|(name, _)| name)
        .collect()
}

/// `utf16.IsSurrogate`.
fn is_surrogate(r: u32) -> bool {
    (0xD800..0xE000).contains(&r)
}

/// `strconv.ParseUint(s, 16, 16)` on exactly four bytes.
fn parse_hex4(b: &[u8]) -> Option<u32> {
    if b.len() != 4 || !b.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(b).ok()?, 16).ok()
}

/// `strings.Builder.WriteRune`: an invalid rune (a lone surrogate) is U+FFFD.
fn push_rune(out: &mut Vec<u8>, r: u32) {
    let c = char::from_u32(r).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

// port of unwrapCustomToolInput (claude_openai-responses_request.go). The
// gjson lookup that runs first is a parse of the whole (complete) document;
// a truncated one falls through to the hand-rolled string scan.
fn unwrap_custom_tool_input(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(trimmed) {
        if let Some(v) = m.get("input") {
            return match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    if let Some(idx) = trimmed.find("\"input\"") {
        let rest = trimmed[idx + 7..].trim();
        if let Some(rest) = rest.strip_prefix(':') {
            let rest = rest.trim();
            if let Some(content) = rest.strip_prefix('"') {
                let content = content.as_bytes();
                let mut unescaped: Vec<u8> = Vec::new();
                let mut in_escape = false;
                let mut i = 0;
                while i < content.len() {
                    let c = content[i];
                    if in_escape {
                        match c {
                            b'"' | b'\\' | b'/' => unescaped.push(c),
                            b'b' => unescaped.push(0x08),
                            b'f' => unescaped.push(0x0c),
                            b'n' => unescaped.push(b'\n'),
                            b'r' => unescaped.push(b'\r'),
                            b't' => unescaped.push(b'\t'),
                            b'u' => {
                                if i + 4 < content.len() {
                                    if let Some(r) = parse_hex4(&content[i + 1..i + 5]) {
                                        if is_surrogate(r)
                                            && i + 10 < content.len()
                                            && &content[i + 5..i + 7] == b"\\u"
                                        {
                                            if let Some(r2) = parse_hex4(&content[i + 7..i + 11]) {
                                                // utf16.DecodeRune
                                                let decoded = if (0xD800..0xDC00).contains(&r)
                                                    && (0xDC00..0xE000).contains(&r2)
                                                {
                                                    ((r - 0xD800) << 10 | (r2 - 0xDC00)) + 0x10000
                                                } else {
                                                    0xFFFD
                                                };
                                                push_rune(&mut unescaped, decoded);
                                                i += 11;
                                                in_escape = false;
                                                continue;
                                            }
                                        }
                                        push_rune(&mut unescaped, r);
                                        i += 5;
                                        in_escape = false;
                                        continue;
                                    }
                                }
                                unescaped.extend_from_slice(b"\\u");
                            }
                            _ => {
                                unescaped.push(b'\\');
                                unescaped.push(c);
                            }
                        }
                        in_escape = false;
                    } else if c == b'\\' {
                        in_escape = true;
                    } else if c == b'"' {
                        break;
                    } else {
                        unescaped.push(c);
                    }
                    i += 1;
                }
                if in_escape {
                    unescaped.push(b'\\');
                }
                return String::from_utf8_lossy(&unescaped).into_owned();
            }
        }
    }
    arguments.to_string()
}

// port of convertResponsesFunctionToolToClaude (claude_openai-responses_request.go)
fn convert_responses_function_tool_to_claude(tool: &Value, override_name: &str) -> Option<Value> {
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = responses_tool_name(tool);
    }
    if name.is_empty() {
        return None;
    }

    let mut t_json = json!({
        "name": sanitize_claude_function_name(&name),
        "description": "",
        "input_schema": {"type": "object", "properties": {}}
    });
    let d = responses_tool_description(tool);
    if !d.is_empty() {
        t_json["description"] = Value::from(d);
    }
    t_json["input_schema"] = normalize_claude_tool_input_schema(responses_tool_parameters(tool));
    attach_cache_control(&mut t_json, Some(tool));
    if t_json.get("cache_control").is_none() {
        attach_cache_control(&mut t_json, tool.get("function"));
    }
    Some(t_json)
}

// port of convertResponsesCustomToolToClaude (claude_openai-responses_request.go)
fn convert_responses_custom_tool_to_claude(tool: &Value, override_name: &str) -> Option<Value> {
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = responses_tool_name(tool);
    }
    if name.is_empty() {
        return None;
    }

    let mut t_json = json!({
        "name": sanitize_claude_function_name(&name),
        "description": "",
        "input_schema": {"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]}
    });
    let description = responses_tool_description(tool);
    if !description.is_empty() {
        t_json["description"] = Value::from(description);
    }
    attach_cache_control(&mut t_json, Some(tool));
    Some(t_json)
}

// port of convertResponsesWebSearchToolToClaude (claude_openai-responses_request.go)
fn convert_responses_web_search_tool_to_claude(tool: &Value) -> Option<Value> {
    if let Some(external) = tool.get("external_web_access") {
        if !gb(Some(external)) {
            return None;
        }
    }

    let mut name = gstr(tool, "name").trim().to_string();
    if name.is_empty() {
        name = "web_search".to_string();
    }
    let mut t_json = json!({"type": "web_search_20250305", "name": name});
    if let Some(max_uses) = tool.get("max_uses") {
        t_json["max_uses"] = Value::from(gi(Some(max_uses)));
    }
    if let Some(allowed @ Value::Array(_)) = gp(tool, "filters.allowed_domains") {
        t_json["allowed_domains"] = allowed.clone();
    }
    if let Some(location @ Value::Object(_)) = tool.get("user_location") {
        t_json["user_location"] = location.clone();
    }
    Some(t_json)
}

// port of responsesToolName (claude_openai-responses_request.go)
fn responses_tool_name(tool: &Value) -> String {
    let name = gstr(tool, "name").trim().to_string();
    if !name.is_empty() {
        return name;
    }
    gstr(tool, "function.name").trim().to_string()
}

// port of responsesToolDescription (claude_openai-responses_request.go)
fn responses_tool_description(tool: &Value) -> String {
    let description = gstr(tool, "description");
    if !description.is_empty() {
        return description;
    }
    gstr(tool, "function.description")
}

// port of responsesToolParameters (claude_openai-responses_request.go)
fn responses_tool_parameters(tool: &Value) -> Option<&Value> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .iter()
    .find_map(|path| gp(tool, path))
}

// port of qualifyResponsesNamespaceToolName (claude_openai-responses_request.go)
fn qualify_responses_namespace_tool_name(namespace_name: &str, child_name: &str) -> String {
    let child_name = child_name.trim();
    if child_name.is_empty() || namespace_name.is_empty() || child_name.starts_with("mcp__") {
        return child_name.to_string();
    }
    if child_name == namespace_name || child_name.starts_with(&format!("{namespace_name}__")) {
        return child_name.to_string();
    }
    if namespace_name.ends_with("__") {
        return format!("{namespace_name}{child_name}");
    }
    format!("{namespace_name}__{child_name}")
}

// port of splitResponsesQualifiedFunctionCallFromRequest (claude_openai-responses_request.go)
fn split_responses_qualified_function_call_from_request(
    request: Option<&Value>,
    qualified_name: &str,
) -> (String, String) {
    let qualified_name = qualified_name.trim();
    if qualified_name.is_empty() {
        return (String::new(), String::new());
    }
    let Some(root) = request else {
        return (qualified_name.to_string(), String::new());
    };
    let descriptors = responses_tool_descriptors(root);
    let winners = responses_tool_winners(&descriptors);
    // Claude answers with the name it was DECLARED — the sanitized form
    // (non-`[A-Za-z0-9_-]` replaced, cut at 64 chars). A long MCP tool name
    // comes back truncated, which Codex never declared; map it back to the
    // request's own name.
    let original = match winners.get(qualified_name) {
        Some(_) => qualified_name.to_string(),
        None => match winners
            .keys()
            .find(|k| sanitize_claude_function_name(k) == qualified_name)
        {
            Some(k) => k.clone(),
            None => return (qualified_name.to_string(), String::new()),
        },
    };
    let w = winners[&original];
    let descriptor = &descriptors[w];
    if !descriptor.direct {
        return (descriptor.child_name.clone(), descriptor.namespace.clone());
    }
    (original, String::new())
}

// port of isUnsupportedOpenAIBuiltinToolType (claude_openai-responses_request.go)
fn is_unsupported_openai_builtin_tool_type(tool_type: &str) -> bool {
    matches!(
        tool_type,
        "image_generation" | "file_search" | "code_interpreter" | "computer_use_preview"
    )
}

// port of normalizeCodexAgentMessages (claude_openai-responses_request.go)
fn normalize_codex_agent_messages(payload: &Value) -> Value {
    let mut updated = payload.clone();
    let Some(Value::Array(input)) = updated.get_mut("input") else {
        return updated;
    };
    for item in input.iter_mut() {
        if gstr(item, "type").trim() != "agent_message" || !item.is_object() {
            continue;
        }
        if let Some(Value::Array(content)) = item.get_mut("content") {
            for part in content.iter_mut() {
                if gstr(part, "type").trim() != "encrypted_content" {
                    continue;
                }
                let Some(Value::String(enc)) = part.get("encrypted_content").cloned() else {
                    continue;
                };
                sj_set(part, "type", Value::from("input_text"));
                sj_set(part, "text", Value::from(enc));
                sj_delete(part, "encrypted_content");
            }
        }
        sj_set(item, "role", Value::from("user"));
        sj_set(item, "type", Value::from("message"));
    }
    updated
}

// ───────────────────────────── response: Claude → Responses ─────────────────────────────

// port of claudeReasoningCarrier (claude_openai-responses_response.go)
fn claude_reasoning_carrier(content_block: &Value) -> String {
    if gstr(content_block, "type") == "redacted_thinking" {
        let data = gstr(content_block, "data");
        if !data.is_empty() {
            return format!("{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}{data}");
        }
        return String::new();
    }
    gstr(content_block, "signature")
}

/// port of claudeResponsesUsageTokens (claude_openai-responses_response.go)
#[derive(Default, Clone)]
struct UsageTokens {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    has_usage: bool,
}

impl UsageTokens {
    // port of claudeResponsesUsageTokens.Merge (claude_openai-responses_response.go)
    fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        self.has_usage = true;
        if let Some(v) = usage.get("input_tokens") {
            self.input_tokens = gi(Some(v));
        }
        if let Some(v) = usage.get("output_tokens") {
            self.output_tokens = gi(Some(v));
        }
        if let Some(v) = usage.get("cache_creation_input_tokens") {
            self.cache_creation_input_tokens = gi(Some(v));
        }
        if let Some(v) = usage.get("cache_read_input_tokens") {
            self.cache_read_input_tokens = gi(Some(v));
        }
    }

    // port of claudeResponsesUsageTokens.OpenAIResponsesUsage (claude_openai-responses_response.go):
    // (input, output, total, cached)
    fn openai_responses_usage(&self) -> (i64, i64, i64, i64) {
        let cached = self.cache_read_input_tokens;
        let input = self.input_tokens + self.cache_creation_input_tokens + cached;
        let output = self.output_tokens;
        (input, output, input + output, cached)
    }
}

// port of claudeResponsesIncompleteDetails (claude_openai-responses_response.go)
fn claude_responses_incomplete_details(stop_reason: &str) -> Option<Value> {
    stop_reason
        .trim()
        .eq_ignore_ascii_case("max_tokens")
        .then(|| json!({"reason": "max_output_tokens"}))
}

// port of claudeResponsesOutputStatus (claude_openai-responses_response.go)
fn claude_responses_output_status(stop_reason: &str) -> &'static str {
    if claude_responses_incomplete_details(stop_reason).is_some() {
        "incomplete"
    } else {
        "completed"
    }
}

// port of claudeResponsesTerminalState (claude_openai-responses_response.go)
fn claude_responses_terminal_state(
    stop_reason: &str,
) -> (&'static str, &'static str, Option<Value>) {
    match claude_responses_incomplete_details(stop_reason) {
        Some(details) => ("response.incomplete", "incomplete", Some(details)),
        None => ("response.completed", "completed", None),
    }
}

// port of pickRequestJSON (claude_openai-responses_response.go): only the
// original request reaches this port.
fn pick_request_json(original_request: &Value) -> Option<Value> {
    (!original_request.is_null()).then(|| original_request.clone())
}

// port of applyResponsesFunctionCallNamespaceFields (claude_openai-responses_response.go)
fn apply_responses_function_call_namespace_fields(
    item: &mut Value,
    request: Option<&Value>,
    qualified_name: &str,
    item_path: &str,
) {
    let (name, namespace) =
        split_responses_qualified_function_call_from_request(request, qualified_name);
    set_responses_tool_call_identity(item, &name, &namespace, item_path);
}

/// The request-echo block shared by the stream's `message_stop` branch and the
/// non-stream aggregate (claude_openai-responses_response.go).
fn echo_request_fields(target: &mut Value, req: &Value) {
    if let Some(v) = req.get("instructions") {
        sj_set(target, "instructions", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("max_output_tokens") {
        sj_set(target, "max_output_tokens", Value::from(gi(Some(v))));
    }
    if let Some(v) = req.get("max_tool_calls") {
        sj_set(target, "max_tool_calls", Value::from(gi(Some(v))));
    }
    if let Some(v) = req.get("model") {
        sj_set(target, "model", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("parallel_tool_calls") {
        sj_set(target, "parallel_tool_calls", Value::from(gb(Some(v))));
    }
    if let Some(v) = req.get("previous_response_id") {
        sj_set(target, "previous_response_id", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("prompt_cache_key") {
        sj_set(target, "prompt_cache_key", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("reasoning") {
        sj_set(target, "reasoning", v.clone());
    }
    if let Some(v) = req.get("safety_identifier") {
        sj_set(target, "safety_identifier", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("service_tier") {
        sj_set(target, "service_tier", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("store") {
        sj_set(target, "store", Value::from(gb(Some(v))));
    }
    if let Some(v) = req.get("temperature") {
        sj_set(target, "temperature", float_value(gf(Some(v))));
    }
    if let Some(v) = req.get("text") {
        sj_set(target, "text", v.clone());
    }
    if let Some(v) = req.get("tool_choice") {
        sj_set(target, "tool_choice", v.clone());
    }
    if let Some(v) = req.get("tools") {
        sj_set(target, "tools", v.clone());
    }
    if let Some(v) = req.get("top_logprobs") {
        sj_set(target, "top_logprobs", Value::from(gi(Some(v))));
    }
    if let Some(v) = req.get("top_p") {
        sj_set(target, "top_p", float_value(gf(Some(v))));
    }
    if let Some(v) = req.get("truncation") {
        sj_set(target, "truncation", Value::from(gs(Some(v))));
    }
    if let Some(v) = req.get("user") {
        sj_set(target, "user", v.clone());
    }
    if let Some(v) = req.get("metadata") {
        sj_set(target, "metadata", v.clone());
    }
}

/// port of claudeResponsesWebSearchItem (claude_openai-responses_response.go)
struct WebSearchItem {
    tool_use_id: String,
    output_index: i64,
    input_buf: String,
    results: Option<Value>,
    emitted: bool,
    status: String,
}

impl WebSearchItem {
    // port of claudeResponsesWebSearchItem.render (claude_openai-responses_response.go)
    fn render(&self) -> Value {
        build_responses_web_search_call_item(
            &self.tool_use_id,
            &claude_web_search_query(&self.input_buf),
            self.results.as_ref(),
        )
    }
}

/// port of claudeResponsesMessageItem (claude_openai-responses_response.go)
struct MessageItem {
    id: String,
    output_index: i64,
    text: String,
    annotations: Vec<Value>,
    status: String,
}

/// port of claudeResponsesReasoningItem (claude_openai-responses_response.go)
struct ReasoningItem {
    id: String,
    output_index: i64,
    text: String,
    signature: String,
    status: String,
}

/// Per-stream state: port of claudeToResponsesState
/// (claude_openai-responses_response.go), plus the request-derived values Go
/// recomputes on every chunk.
pub struct StreamTranslator {
    request: Option<Value>,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: bool,
    stopped: bool,
    /// An Anthropic in-stream `error` event (e.g. `overloaded_error`).
    /// Termory's addition — the Go translator ignores the event, and the
    /// stream then closes as `response.completed`, a truncated answer
    /// reported as a success. Set, the stream ends as `response.failed`.
    error: Option<Value>,

    seq: i64,
    response_id: String,
    created_at: i64,
    next_output_index: i64,
    current_msg_id: String,
    current_fc_id: String,
    in_text_block: bool,
    in_func_block: bool,
    message_open: bool,
    content_part_open: bool,
    message_output_index: i64,
    func_args_buf: HashMap<i64, String>,
    func_args_done: HashSet<i64>,
    func_item_done: HashSet<i64>,
    func_item_status: HashMap<i64, String>,
    func_names: HashMap<i64, String>,
    func_call_ids: BTreeMap<i64, String>,
    func_custom: HashMap<i64, bool>,
    func_output_indices: HashMap<i64, i64>,
    text_buf: String,
    message_annotations: Vec<Value>,
    message_items: Vec<MessageItem>,
    reasoning_active: bool,
    reasoning_deltas_done: bool,
    reasoning_item_id: String,
    reasoning_buf: String,
    reasoning_signature: String,
    reasoning_index: i64,
    reasoning_items: Vec<ReasoningItem>,
    web_search_by_block: HashMap<i64, usize>,
    web_search_by_tool_id: HashMap<String, usize>,
    web_search_items: Vec<WebSearchItem>,
    stop_reason: String,
    usage: UsageTokens,
}

impl StreamTranslator {
    pub fn new(original_request: &Value) -> Self {
        let request = pick_request_json(original_request);
        let custom_tool_names = responses_custom_tool_names(request.as_ref());
        StreamTranslator {
            request_model: request_model_name(original_request),
            request,
            custom_tool_names,
            started: false,
            stopped: false,
            error: None,
            seq: 0,
            response_id: String::new(),
            created_at: 0,
            next_output_index: 0,
            current_msg_id: String::new(),
            current_fc_id: String::new(),
            in_text_block: false,
            in_func_block: false,
            message_open: false,
            content_part_open: false,
            message_output_index: -1,
            func_args_buf: HashMap::new(),
            func_args_done: HashSet::new(),
            func_item_done: HashSet::new(),
            func_item_status: HashMap::new(),
            func_names: HashMap::new(),
            func_call_ids: BTreeMap::new(),
            func_custom: HashMap::new(),
            func_output_indices: HashMap::new(),
            text_buf: String::new(),
            message_annotations: Vec::new(),
            message_items: Vec::new(),
            reasoning_active: false,
            reasoning_deltas_done: false,
            reasoning_item_id: String::new(),
            reasoning_buf: String::new(),
            reasoning_signature: String::new(),
            reasoning_index: -1,
            reasoning_items: Vec::new(),
            web_search_by_block: HashMap::new(),
            web_search_by_tool_id: HashMap::new(),
            web_search_items: Vec::new(),
            stop_reason: String::new(),
            usage: UsageTokens::default(),
        }
    }

    /// One upstream Anthropic SSE event. Go dispatches on the payload's
    /// `type` and ignores the `event:` line, so `event` is unused.
    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
        let _ = event;
        self.convert(data)
    }

    /// Upstream ended: an Anthropic stream that was cut before `message_stop`
    /// is closed here as if `message_stop` had arrived (Go emits nothing).
    pub fn finish(&mut self) -> Vec<String> {
        if !self.started || self.stopped {
            return Vec::new();
        }
        self.message_stop()
    }

    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    // port of claudeToResponsesState.appendMessageAnnotation (claude_openai-responses_response.go)
    fn append_message_annotation(&mut self, annotation: &Value) {
        if annotation.is_null() {
            return;
        }
        self.message_annotations.push(annotation.clone());
    }

    // port of claudeToResponsesState.allocateOutputIndex (claude_openai-responses_response.go)
    fn allocate_output_index(&mut self) -> i64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    // port of claudeToResponsesState.messageOutputIndex (claude_openai-responses_response.go)
    fn message_output_index(&mut self) -> i64 {
        if self.message_output_index < 0 {
            self.message_output_index = self.allocate_output_index();
        }
        self.message_output_index
    }

    // port of claudeToResponsesState.functionOutputIndex (claude_openai-responses_response.go)
    fn function_output_index(&mut self, block_index: i64) -> i64 {
        if let Some(&index) = self.func_output_indices.get(&block_index) {
            return index;
        }
        let index = self.allocate_output_index();
        self.func_output_indices.insert(block_index, index);
        index
    }

    // port of claudeToResponsesState.startWebSearch (claude_openai-responses_response.go)
    fn start_web_search(&mut self, block_index: i64, tool_use_id: String) -> usize {
        let output_index = self.allocate_output_index();
        let slot = self.web_search_items.len();
        self.web_search_items.push(WebSearchItem {
            tool_use_id: tool_use_id.clone(),
            output_index,
            input_buf: String::new(),
            results: None,
            emitted: false,
            status: String::new(),
        });
        self.web_search_by_block.insert(block_index, slot);
        self.web_search_by_tool_id.insert(tool_use_id, slot);
        slot
    }

    // port of claudeToResponsesState.finalizeWebSearchWithStatus (claude_openai-responses_response.go)
    fn finalize_web_search_with_status(&mut self, slot: usize, status: &str) -> Vec<String> {
        if self.web_search_items[slot].emitted {
            return Vec::new();
        }
        self.web_search_items[slot].emitted = true;
        self.web_search_items[slot].status = status.to_string();
        let seq = self.next_seq();
        let item = &self.web_search_items[slot];
        let mut rendered = item.render();
        rendered["status"] = Value::from(status);
        let done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": item.output_index,
            "item": rendered
        });
        vec![sse("response.output_item.done", &done)]
    }

    // port of claudeToResponsesState.finalizeFuncItem (claude_openai-responses_response.go)
    fn finalize_func_item(&mut self, idx: i64, status: &str) -> Vec<String> {
        if self.func_item_done.contains(&idx) {
            return Vec::new();
        }
        self.func_item_done.insert(idx);
        self.func_item_status.insert(idx, status.to_string());

        let output_index = self.function_output_index(idx);
        let mut args = self.func_args_buf.get(&idx).cloned().unwrap_or_default();
        let custom = self.func_custom.get(&idx).copied().unwrap_or(false);
        if !custom && args.is_empty() && status == "completed" {
            args = "{}".to_string();
        }
        let mut call_id = self.func_call_ids.get(&idx).cloned().unwrap_or_default();
        if call_id.is_empty() {
            call_id = self.current_fc_id.clone();
        }
        let name = self.func_names.get(&idx).cloned().unwrap_or_default();

        let mut out = Vec::new();
        if custom {
            let input = unwrap_custom_tool_input(&args);
            if !self.func_args_done.contains(&idx) {
                self.func_args_done.insert(idx);
                let seq = self.next_seq();
                let input_done = json!({
                    "type": "response.custom_tool_call_input.done",
                    "sequence_number": seq,
                    "item_id": format!("ctc_{call_id}"),
                    "output_index": output_index,
                    "input": input
                });
                out.push(sse("response.custom_tool_call_input.done", &input_done));
            }

            let seq = self.next_seq();
            let mut item_done = json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": output_index,
                "item": {"id": format!("ctc_{call_id}"), "type": "custom_tool_call", "status": status, "input": input, "call_id": call_id, "name": ""}
            });
            apply_responses_function_call_namespace_fields(
                &mut item_done,
                self.request.as_ref(),
                &name,
                "item",
            );
            out.push(sse("response.output_item.done", &item_done));
        } else {
            if !self.func_args_done.contains(&idx) {
                self.func_args_done.insert(idx);
                let seq = self.next_seq();
                let fc_done = json!({
                    "type": "response.function_call_arguments.done",
                    "sequence_number": seq,
                    "item_id": format!("fc_{call_id}"),
                    "output_index": output_index,
                    "arguments": args
                });
                out.push(sse("response.function_call_arguments.done", &fc_done));
            }

            let seq = self.next_seq();
            let mut item_done = json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": output_index,
                "item": {"id": format!("fc_{call_id}"), "type": "function_call", "status": status, "arguments": args, "call_id": call_id, "name": ""}
            });
            apply_responses_function_call_namespace_fields(
                &mut item_done,
                self.request.as_ref(),
                &name,
                "item",
            );
            out.push(sse("response.output_item.done", &item_done));
        }
        self.in_func_block = false;
        out
    }

    // port of claudeToResponsesState.finalizeReasoningDeltas (claude_openai-responses_response.go)
    fn finalize_reasoning_deltas(&mut self) -> Vec<String> {
        if !self.reasoning_active || self.reasoning_deltas_done {
            return Vec::new();
        }
        self.reasoning_deltas_done = true;
        let full = self.reasoning_buf.clone();
        let mut out = Vec::new();
        let seq = self.next_seq();
        let text_done = json!({
            "type": "response.reasoning_summary_text.done",
            "sequence_number": seq,
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "text": full
        });
        out.push(sse("response.reasoning_summary_text.done", &text_done));
        let seq = self.next_seq();
        let part_done = json!({
            "type": "response.reasoning_summary_part.done",
            "sequence_number": seq,
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "part": {"type": "summary_text", "text": full}
        });
        out.push(sse("response.reasoning_summary_part.done", &part_done));
        out
    }

    // port of claudeToResponsesState.finalizeReasoningItem (claude_openai-responses_response.go)
    fn finalize_reasoning_item(&mut self, status: &str) -> Vec<String> {
        if !self.reasoning_active && self.reasoning_item_id.is_empty() {
            return Vec::new();
        }
        let mut out = self.finalize_reasoning_deltas();

        let full = self.reasoning_buf.clone();
        let seq = self.next_seq();
        let item_done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": self.reasoning_index,
            "item": {
                "id": self.reasoning_item_id,
                "type": "reasoning",
                "status": status,
                "encrypted_content": self.reasoning_signature,
                "summary": [{"type": "summary_text", "text": full}]
            }
        });
        out.push(sse("response.output_item.done", &item_done));
        self.reasoning_items.push(ReasoningItem {
            id: self.reasoning_item_id.clone(),
            output_index: self.reasoning_index,
            text: full,
            signature: self.reasoning_signature.clone(),
            status: status.to_string(),
        });
        self.reasoning_active = false;
        self.reasoning_item_id.clear();
        self.reasoning_buf.clear();
        self.reasoning_signature.clear();
        self.reasoning_index = -1;
        out
    }

    // port of claudeToResponsesState.finalizeAssistantMessage (claude_openai-responses_response.go)
    fn finalize_assistant_message(&mut self) -> Vec<String> {
        if !self.message_open {
            return Vec::new();
        }
        let full_text = self.text_buf.clone();
        let output_index = self.message_output_index();
        let status = claude_responses_output_status(&self.stop_reason);
        let mut out = Vec::new();

        let seq = self.next_seq();
        let done = json!({
            "type": "response.output_text.done",
            "sequence_number": seq,
            "item_id": self.current_msg_id,
            "output_index": output_index,
            "content_index": 0,
            "text": full_text,
            "logprobs": []
        });
        out.push(sse("response.output_text.done", &done));

        let seq = self.next_seq();
        let mut part_done = json!({
            "type": "response.content_part.done",
            "sequence_number": seq,
            "item_id": self.current_msg_id,
            "output_index": output_index,
            "content_index": 0,
            "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": full_text}
        });
        if !self.message_annotations.is_empty() {
            part_done["part"]["annotations"] = Value::Array(self.message_annotations.clone());
        }
        out.push(sse("response.content_part.done", &part_done));

        let seq = self.next_seq();
        let mut fin = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": output_index,
            "item": {
                "id": self.current_msg_id,
                "type": "message",
                "status": status,
                "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": full_text}],
                "role": "assistant"
            }
        });
        if !self.message_annotations.is_empty() {
            fin["item"]["content"][0]["annotations"] =
                Value::Array(self.message_annotations.clone());
        }
        out.push(sse("response.output_item.done", &fin));

        self.message_items.push(MessageItem {
            id: self.current_msg_id.clone(),
            output_index,
            text: full_text,
            annotations: self.message_annotations.clone(),
            status: status.to_string(),
        });
        self.in_text_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.current_msg_id.clear();
        self.message_output_index = -1;
        self.text_buf.clear();
        self.message_annotations.clear();
        out
    }

    // port of ConvertClaudeResponseToOpenAIResponses (claude_openai-responses_response.go)
    fn convert(&mut self, root: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        match gstr(root, "type").as_str() {
            "message_start" => {
                let Some(msg) = root.get("message") else {
                    return out;
                };
                self.started = true;
                self.stopped = false;
                self.response_id = gstr(msg, "id");
                self.created_at = now_unix();
                // Reset per-message aggregation state
                self.text_buf.clear();
                self.message_annotations.clear();
                self.message_items.clear();
                self.reasoning_buf.clear();
                self.reasoning_active = false;
                self.reasoning_deltas_done = false;
                self.next_output_index = 0;
                self.in_text_block = false;
                self.in_func_block = false;
                self.message_open = false;
                self.content_part_open = false;
                self.current_msg_id.clear();
                self.current_fc_id.clear();
                self.message_output_index = -1;
                self.reasoning_item_id.clear();
                self.reasoning_signature.clear();
                self.reasoning_index = -1;
                self.reasoning_items.clear();
                self.stop_reason.clear();
                self.func_args_buf.clear();
                self.func_args_done.clear();
                self.func_item_done.clear();
                self.func_item_status.clear();
                self.func_names.clear();
                self.func_call_ids.clear();
                self.func_custom.clear();
                self.func_output_indices.clear();
                self.usage = UsageTokens::default();
                self.usage.merge(msg.get("usage"));

                // response.created
                let seq = self.next_seq();
                let mut created = json!({
                    "type": "response.created",
                    "sequence_number": seq,
                    "response": {
                        "id": self.response_id,
                        "object": "response",
                        "created_at": self.created_at,
                        "status": "in_progress",
                        "background": false,
                        "error": null,
                        "output": []
                    }
                });
                if !self.request_model.is_empty() {
                    sj_set(
                        &mut created,
                        "response.model",
                        Value::from(self.request_model.clone()),
                    );
                }
                out.push(sse("response.created", &created));
                // response.in_progress
                let seq = self.next_seq();
                let mut inprog = json!({
                    "type": "response.in_progress",
                    "sequence_number": seq,
                    "response": {
                        "id": self.response_id,
                        "object": "response",
                        "created_at": self.created_at,
                        "status": "in_progress",
                        "output": []
                    }
                });
                if !self.request_model.is_empty() {
                    sj_set(
                        &mut inprog,
                        "response.model",
                        Value::from(self.request_model.clone()),
                    );
                }
                out.push(sse("response.in_progress", &inprog));
            }
            "content_block_start" => {
                let Some(cb) = root.get("content_block") else {
                    return out;
                };
                let idx = gi(root.get("index"));
                let typ = gstr(cb, "type");

                // Keep adjacent text blocks in the same assistant message.
                if typ != "text" {
                    out.extend(self.finalize_assistant_message());
                }
                // Finalize previous reasoning item
                if self.reasoning_active || !self.reasoning_item_id.is_empty() {
                    out.extend(self.finalize_reasoning_item("completed"));
                }
                // Finalize any previous completed function calls
                let prev_indices: Vec<i64> = self.func_call_ids.keys().copied().collect();
                for prev_idx in prev_indices {
                    if !self.func_item_done.contains(&prev_idx) && prev_idx != idx {
                        out.extend(self.finalize_func_item(prev_idx, "completed"));
                    }
                }
                // Finalize any previous web search items
                for slot in 0..self.web_search_items.len() {
                    let item = &self.web_search_items[slot];
                    if !item.emitted && item.results.is_some() {
                        out.extend(self.finalize_web_search_with_status(slot, "completed"));
                    }
                }

                match typ.as_str() {
                    "text" => {
                        self.in_text_block = true;
                        let output_index = self.message_output_index();
                        if self.current_msg_id.is_empty() {
                            self.current_msg_id =
                                format!("msg_{}_{}", self.response_id, self.message_items.len());
                        }
                        if !self.message_open {
                            let seq = self.next_seq();
                            let item = json!({
                                "type": "response.output_item.added",
                                "sequence_number": seq,
                                "output_index": output_index,
                                "item": {"id": self.current_msg_id, "type": "message", "status": "in_progress", "content": [], "role": "assistant"}
                            });
                            out.push(sse("response.output_item.added", &item));
                            self.message_open = true;
                        }
                        if !self.content_part_open {
                            let seq = self.next_seq();
                            let part = json!({
                                "type": "response.content_part.added",
                                "sequence_number": seq,
                                "item_id": self.current_msg_id,
                                "output_index": output_index,
                                "content_index": 0,
                                "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""}
                            });
                            out.push(sse("response.content_part.added", &part));
                            self.content_part_open = true;
                        }
                    }
                    "tool_use" => {
                        self.in_func_block = true;
                        self.current_fc_id = gstr(cb, "id");
                        let name = gstr(cb, "name");
                        let is_custom_tool = self.custom_tool_names.contains(&name);
                        self.func_custom.insert(idx, is_custom_tool);
                        let output_index = self.function_output_index(idx);
                        let mut item = if is_custom_tool {
                            json!({
                                "type": "response.output_item.added",
                                "sequence_number": 0,
                                "output_index": 0,
                                "item": {"id": format!("ctc_{}", self.current_fc_id), "type": "custom_tool_call", "status": "in_progress", "input": "", "call_id": self.current_fc_id, "name": ""}
                            })
                        } else {
                            json!({
                                "type": "response.output_item.added",
                                "sequence_number": 0,
                                "output_index": 0,
                                "item": {"id": format!("fc_{}", self.current_fc_id), "type": "function_call", "status": "in_progress", "arguments": "", "call_id": self.current_fc_id, "name": ""}
                            })
                        };
                        apply_responses_function_call_namespace_fields(
                            &mut item,
                            self.request.as_ref(),
                            &name,
                            "item",
                        );
                        item["sequence_number"] = Value::from(self.next_seq());
                        item["output_index"] = Value::from(output_index);
                        out.push(sse("response.output_item.added", &item));
                        self.func_args_buf.entry(idx).or_default();
                        // Record function metadata for aggregation.
                        self.func_call_ids.insert(idx, self.current_fc_id.clone());
                        self.func_names.insert(idx, name);
                    }
                    // Reachability guard: only web_search can be enabled on
                    // Claude by this translator (Go logs anything else).
                    "server_tool_use" if gstr(cb, "name") == CLAUDE_WEB_SEARCH_TOOL_NAME => {
                        let slot = self.start_web_search(idx, gstr(cb, "id"));
                        let seq = self.next_seq();
                        let item = &self.web_search_items[slot];
                        let added = json!({
                            "type": "response.output_item.added",
                            "sequence_number": seq,
                            "output_index": item.output_index,
                            "item": {"id": responses_web_search_call_id(&item.tool_use_id), "type": "web_search_call", "status": "in_progress", "action": {"type": "search", "query": ""}}
                        });
                        out.push(sse("response.output_item.added", &added));
                    }
                    "web_search_tool_result" => {
                        // A result block carries its full content up front and has
                        // no deltas. Results are stored here and the item is closed
                        // when the next block starts or at message_stop.
                        if let Some(&slot) =
                            self.web_search_by_tool_id.get(&gstr(cb, "tool_use_id"))
                        {
                            self.web_search_items[slot].results =
                                claude_web_search_results_to_responses(cb.get("content"));
                        }
                    }
                    "thinking" | "redacted_thinking" => {
                        // start reasoning item
                        self.reasoning_active = true;
                        self.reasoning_deltas_done = false;
                        self.reasoning_index = self.allocate_output_index();
                        self.reasoning_buf.clear();
                        self.reasoning_signature = claude_reasoning_carrier(cb);
                        self.reasoning_item_id = format!("rs_{}_{}", self.response_id, idx);
                        let seq = self.next_seq();
                        let item = json!({
                            "type": "response.output_item.added",
                            "sequence_number": seq,
                            "output_index": self.reasoning_index,
                            "item": {"id": self.reasoning_item_id, "type": "reasoning", "status": "in_progress", "encrypted_content": self.reasoning_signature, "summary": []}
                        });
                        out.push(sse("response.output_item.added", &item));
                        // add a summary part placeholder
                        let seq = self.next_seq();
                        let part = json!({
                            "type": "response.reasoning_summary_part.added",
                            "sequence_number": seq,
                            "item_id": self.reasoning_item_id,
                            "output_index": self.reasoning_index,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": ""}
                        });
                        out.push(sse("response.reasoning_summary_part.added", &part));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(d) = root.get("delta") else {
                    return out;
                };
                match gstr(d, "type").as_str() {
                    "text_delta" => {
                        if let Some(t) = d.get("text") {
                            let text = gs(Some(t));
                            let seq = self.next_seq();
                            let output_index = self.message_output_index();
                            let msg = json!({
                                "type": "response.output_text.delta",
                                "sequence_number": seq,
                                "item_id": self.current_msg_id,
                                "output_index": output_index,
                                "content_index": 0,
                                "delta": text,
                                "logprobs": []
                            });
                            out.push(sse("response.output_text.delta", &msg));
                            // aggregate text for response.output
                            self.text_buf.push_str(&text);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(&slot) = self.web_search_by_block.get(&gi(root.get("index"))) {
                            if let Some(pj) = d.get("partial_json") {
                                self.web_search_items[slot]
                                    .input_buf
                                    .push_str(&gs(Some(pj)));
                            }
                            return Vec::new();
                        }
                        if !self.in_func_block || self.current_fc_id.is_empty() {
                            return Vec::new();
                        }
                        let idx = gi(root.get("index"));
                        if let Some(pj) = d.get("partial_json") {
                            let pj = gs(Some(pj));
                            self.func_args_buf.entry(idx).or_default().push_str(&pj);
                            if self.func_custom.get(&idx).copied().unwrap_or(false) {
                                return Vec::new();
                            }
                            let output_index = self.function_output_index(idx);
                            let seq = self.next_seq();
                            let msg = json!({
                                "type": "response.function_call_arguments.delta",
                                "sequence_number": seq,
                                "item_id": format!("fc_{}", self.current_fc_id),
                                "output_index": output_index,
                                "delta": pj
                            });
                            out.push(sse("response.function_call_arguments.delta", &msg));
                        }
                    }
                    "thinking_delta" if self.reasoning_active => {
                        if let Some(t) = d.get("thinking") {
                            let text = gs(Some(t));
                            self.reasoning_buf.push_str(&text);
                            let seq = self.next_seq();
                            let msg = json!({
                                "type": "response.reasoning_summary_text.delta",
                                "sequence_number": seq,
                                "item_id": self.reasoning_item_id,
                                "output_index": self.reasoning_index,
                                "summary_index": 0,
                                "delta": text
                            });
                            out.push(sse("response.reasoning_summary_text.delta", &msg));
                        }
                    }
                    "signature_delta" => {
                        if self.reasoning_active {
                            let signature = gstr(d, "signature");
                            if !signature.is_empty() {
                                self.reasoning_signature = signature;
                            }
                        }
                        return Vec::new();
                    }
                    "citations_delta" => {
                        if let Some(citation) = d.get("citation") {
                            self.append_message_annotation(citation);
                        }
                        return Vec::new();
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if self.in_text_block {
                    self.in_text_block = false;
                } else if self.in_func_block {
                    self.in_func_block = false;
                } else if self.reasoning_active {
                    out.extend(self.finalize_reasoning_deltas());
                }
            }
            "message_delta" => {
                self.usage.merge(root.get("usage"));
                if let Some(stop_reason) = gp(root, "delta.stop_reason") {
                    self.stop_reason = gs(Some(stop_reason));
                }
                return Vec::new();
            }
            "message_stop" => return self.message_stop(),
            "error" => {
                if self.stopped {
                    return Vec::new();
                }
                let e = root.get("error").cloned().unwrap_or_else(|| json!({}));
                self.error = Some(json!({
                    "code": gs(e.get("type")),
                    "message": gs(e.get("message")),
                }));
                if !self.started {
                    // Nothing was opened to close: the bootstrap reads an
                    // error as the first payload and fails over instead.
                    self.stopped = true;
                    return Vec::new();
                }
                return self.message_stop();
            }
            _ => {}
        }
        out
    }

    /// The `message_stop` branch of ConvertClaudeResponseToOpenAIResponses
    /// (claude_openai-responses_response.go).
    fn message_stop(&mut self) -> Vec<String> {
        self.stopped = true;
        let mut out: Vec<String> = Vec::new();
        let tool_status = claude_responses_output_status(&self.stop_reason);
        if self.reasoning_active || !self.reasoning_item_id.is_empty() {
            out.extend(self.finalize_reasoning_item(tool_status));
        }
        out.extend(self.finalize_assistant_message());
        let indices: Vec<i64> = self.func_call_ids.keys().copied().collect();
        for idx in indices {
            if !self.func_item_done.contains(&idx) {
                out.extend(self.finalize_func_item(idx, tool_status));
            }
        }
        for slot in 0..self.web_search_items.len() {
            if !self.web_search_items[slot].emitted {
                out.extend(self.finalize_web_search_with_status(slot, tool_status));
            }
        }

        let (event_type, response_status, incomplete_details) = if self.error.is_some() {
            ("response.failed", "failed", None)
        } else {
            claude_responses_terminal_state(&self.stop_reason)
        };
        let seq = self.next_seq();
        let mut completed = json!({
            "type": event_type,
            "sequence_number": seq,
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": response_status,
                "background": false,
                "error": null
            }
        });
        if let Some(details) = incomplete_details {
            sj_set(&mut completed, "response.incomplete_details", details);
        }
        if let Some(err) = self.error.clone() {
            completed["response"]["error"] = err;
        }
        // Inject original request fields into response as per docs/response.completed.json
        if let Some(req) = &self.request {
            echo_request_fields(&mut completed["response"], req);
        }

        // Build response.output from aggregated state
        let mut outputs: Vec<Value> = Vec::new();
        // reasoning items
        for reasoning in &self.reasoning_items {
            let status = if reasoning.status.is_empty() {
                "completed"
            } else {
                reasoning.status.as_str()
            };
            let item = json!({
                "id": reasoning.id,
                "type": "reasoning",
                "status": status,
                "encrypted_content": reasoning.signature,
                "summary": [{"type": "summary_text", "text": reasoning.text}]
            });
            set_array_index(&mut outputs, reasoning.output_index, item);
        }
        // assistant message items
        for message in &self.message_items {
            let mut item = json!({
                "id": message.id,
                "type": "message",
                "status": message.status,
                "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": message.text}],
                "role": "assistant"
            });
            if !message.annotations.is_empty() {
                item["content"][0]["annotations"] = Value::Array(message.annotations.clone());
            }
            set_array_index(&mut outputs, message.output_index, item);
        }
        // web_search_call items
        for item in &self.web_search_items {
            let status = if item.status.is_empty() {
                "completed"
            } else {
                item.status.as_str()
            };
            let mut rendered = item.render();
            rendered["status"] = Value::from(status);
            set_array_index(&mut outputs, item.output_index, rendered);
        }
        // function_call items (in ascending index order for determinism)
        let mut idxs: Vec<i64> = self.func_args_buf.keys().copied().collect();
        idxs.sort_unstable();
        for idx in idxs {
            let status = self
                .func_item_status
                .get(&idx)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| "completed".to_string());
            let custom = self.func_custom.get(&idx).copied().unwrap_or(false);
            let mut args = if !custom && status == "completed" {
                "{}".to_string()
            } else {
                String::new()
            };
            if let Some(b) = self.func_args_buf.get(&idx) {
                if !b.is_empty() {
                    args = b.clone();
                }
            }
            let mut call_id = self.func_call_ids.get(&idx).cloned().unwrap_or_default();
            let name = self.func_names.get(&idx).cloned().unwrap_or_default();
            if call_id.is_empty() && !self.current_fc_id.is_empty() {
                call_id = self.current_fc_id.clone();
            }
            let output_index = self.func_output_indices.get(&idx).copied().unwrap_or(0);
            let mut item = if custom {
                json!({"id": format!("ctc_{call_id}"), "type": "custom_tool_call", "status": status, "input": unwrap_custom_tool_input(&args), "call_id": call_id, "name": ""})
            } else {
                json!({"id": format!("fc_{call_id}"), "type": "function_call", "status": status, "arguments": args, "call_id": call_id, "name": ""})
            };
            apply_responses_function_call_namespace_fields(
                &mut item,
                self.request.as_ref(),
                &name,
                "",
            );
            set_array_index(&mut outputs, output_index, item);
        }
        if !outputs.is_empty() {
            sj_set(&mut completed, "response.output", Value::Array(outputs));
        }

        let reasoning_length: usize = self.reasoning_items.iter().map(|r| r.text.len()).sum();
        let reasoning_tokens = (reasoning_length / 4) as i64;
        let usage_present = self.usage.has_usage || reasoning_tokens > 0;
        if usage_present {
            let (input_tokens, output_tokens, total_tokens, cached_tokens) =
                self.usage.openai_responses_usage();
            sj_set(
                &mut completed,
                "response.usage.input_tokens",
                Value::from(input_tokens),
            );
            sj_set(
                &mut completed,
                "response.usage.input_tokens_details.cached_tokens",
                Value::from(cached_tokens),
            );
            sj_set(
                &mut completed,
                "response.usage.output_tokens",
                Value::from(output_tokens),
            );
            sj_set(
                &mut completed,
                "response.usage.output_tokens_details.reasoning_tokens",
                Value::from(reasoning_tokens),
            );
            if total_tokens > 0 || self.usage.has_usage {
                sj_set(
                    &mut completed,
                    "response.usage.total_tokens",
                    Value::from(total_tokens),
                );
            }
        }
        out.push(sse(event_type, &completed));
        out
    }
}

/// A complete upstream response → the client's Responses object. The Go
/// function aggregates raw SSE text; a string `upstream` is taken as that
/// text, an array as already-parsed events, and an object as a complete
/// Messages response, replayed as the events Anthropic would have streamed.
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let events: Vec<Value> = match upstream {
        Value::String(text) => sse_data_events(text),
        Value::Array(events) => events.clone(),
        Value::Object(_) => message_to_events(upstream),
        _ => Vec::new(),
    };
    convert_claude_response_to_openai_responses_non_stream(&events, original_request)
}

/// The SSE line split at the top of ConvertClaudeResponseToOpenAIResponsesNonStream
/// (claude_openai-responses_response.go): every `data:` line. A payload that is
/// not JSON parses as nothing in gjson and is dropped here.
fn sse_data_events(text: &str) -> Vec<Value> {
    text.split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty())
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

/// The stream events Anthropic emits for a complete Messages response — not a
/// port; it adapts the router's non-stream body to the Go event loop.
fn message_to_events(message: &Value) -> Vec<Value> {
    if gstr(message, "type") == "error" {
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
            let start_block = match gstr(block, "type").as_str() {
                "text" => {
                    deltas.push(json!({"type": "text_delta", "text": gstr(block, "text")}));
                    if let Some(Value::Array(citations)) = block.get("citations") {
                        for citation in citations {
                            deltas.push(json!({"type": "citations_delta", "citation": citation}));
                        }
                    }
                    json!({"type": "text", "text": ""})
                }
                "thinking" => {
                    deltas.push(
                        json!({"type": "thinking_delta", "thinking": gstr(block, "thinking")}),
                    );
                    let sig = gstr(block, "signature");
                    if !sig.is_empty() {
                        deltas.push(json!({"type": "signature_delta", "signature": sig}));
                    }
                    json!({"type": "thinking", "thinking": ""})
                }
                "tool_use" => {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    // An empty input streams as no delta at all.
                    if !matches!(&input, Value::Object(m) if m.is_empty()) {
                        deltas.push(
                            json!({"type": "input_json_delta", "partial_json": input.to_string()}),
                        );
                    }
                    json!({"type": "tool_use", "id": block.get("id").cloned().unwrap_or(Value::Null), "name": block.get("name").cloned().unwrap_or(Value::Null), "input": {}})
                }
                _ => block.clone(),
            };
            events.push(json!({"type": "content_block_start", "index": index, "content_block": start_block}));
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

/// The `nonStreamOutputItem` local type of
/// ConvertClaudeResponseToOpenAIResponsesNonStream (claude_openai-responses_response.go).
struct NonStreamOutputItem {
    item_type: String,
    id: String,
    call_id: String,
    name: String,
    text: String,
    signature: String,
    annotations: Vec<Value>,
    args: String,
    results: Option<Value>,
}

// port of ConvertClaudeResponseToOpenAIResponsesNonStream (claude_openai-responses_response.go),
// from the point where the SSE body has been split into events.
fn convert_claude_response_to_openai_responses_non_stream(
    events: &[Value],
    original_request: &Value,
) -> Value {
    let req = pick_request_json(original_request);
    let custom_tool_names = responses_custom_tool_names(req.as_ref());

    // Base OpenAI Responses (non-stream) object
    let mut out = json!({
        "id": "",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "background": false,
        "error": null,
        "incomplete_details": null,
        "output": [],
        "usage": {
            "input_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 0,
            "output_tokens_details": {},
            "total_tokens": 0
        }
    });

    // Aggregation state
    let mut response_id = String::new();
    let mut created_at: i64 = 0;
    let mut stop_reason = String::new();
    let mut usage_tokens = UsageTokens::default();

    // Output items in allocation order: the output index is the vector index.
    let mut output_items: Vec<NonStreamOutputItem> = Vec::new();
    let mut block_to_item: HashMap<i64, usize> = HashMap::new();
    let mut web_search_by_tool_id: HashMap<String, usize> = HashMap::new();
    let mut message_count = 0;
    let mut active_message_item: Option<usize> = None;
    let mut pending_annotations: Vec<Value> = Vec::new();

    let new_output_item = |items: &mut Vec<NonStreamOutputItem>,
                           block_to_item: &mut HashMap<i64, usize>,
                           item_type: &str,
                           block_index: i64|
     -> usize {
        let slot = items.len();
        items.push(NonStreamOutputItem {
            item_type: item_type.to_string(),
            id: String::new(),
            call_id: String::new(),
            name: String::new(),
            text: String::new(),
            signature: String::new(),
            annotations: Vec::new(),
            args: String::new(),
            results: None,
        });
        block_to_item.insert(block_index, slot);
        slot
    };

    // Walk through SSE chunks to fill state
    for root in events {
        match gstr(root, "type").as_str() {
            "message_start" => {
                if let Some(msg) = root.get("message") {
                    response_id = gstr(msg, "id");
                    created_at = now_unix();
                    usage_tokens.merge(msg.get("usage"));
                }
            }
            "content_block_start" => {
                let Some(cb) = root.get("content_block") else {
                    continue;
                };
                let idx = gi(root.get("index"));
                let typ = gstr(cb, "type");
                if typ != "text" {
                    active_message_item = None;
                }
                match typ.as_str() {
                    "text" => {
                        let slot = match active_message_item {
                            Some(slot) => {
                                block_to_item.insert(idx, slot);
                                slot
                            }
                            None => {
                                let slot = new_output_item(
                                    &mut output_items,
                                    &mut block_to_item,
                                    "message",
                                    idx,
                                );
                                output_items[slot].id =
                                    format!("msg_{response_id}_{message_count}");
                                message_count += 1;
                                slot
                            }
                        };
                        if !pending_annotations.is_empty() {
                            output_items[slot]
                                .annotations
                                .append(&mut pending_annotations);
                        }
                        active_message_item = Some(slot);
                    }
                    "tool_use" => {
                        let item_type = if custom_tool_names.contains(&gstr(cb, "name")) {
                            "custom_tool_call"
                        } else {
                            "function_call"
                        };
                        let slot =
                            new_output_item(&mut output_items, &mut block_to_item, item_type, idx);
                        let item = &mut output_items[slot];
                        item.call_id = gstr(cb, "id");
                        item.id = if item_type == "custom_tool_call" {
                            format!("ctc_{}", item.call_id)
                        } else {
                            format!("fc_{}", item.call_id)
                        };
                        item.name = gstr(cb, "name");
                    }
                    "server_tool_use" => {
                        if gstr(cb, "name") != CLAUDE_WEB_SEARCH_TOOL_NAME {
                            continue;
                        }
                        let tool_use_id = gstr(cb, "id");
                        let slot = new_output_item(
                            &mut output_items,
                            &mut block_to_item,
                            "web_search_call",
                            idx,
                        );
                        let item = &mut output_items[slot];
                        item.id = responses_web_search_call_id(&tool_use_id);
                        item.call_id = tool_use_id.clone();
                        web_search_by_tool_id.insert(tool_use_id, slot);
                        // Streaming announces an empty input and fills it through
                        // input_json_delta; only seed when the query is already present.
                        if let Some(input @ Value::Object(_)) = cb.get("input") {
                            let raw = input.to_string();
                            if !claude_web_search_query(&raw).is_empty() {
                                item.args.push_str(&raw);
                            }
                        }
                    }
                    "web_search_tool_result" => {
                        if let Some(&slot) = web_search_by_tool_id.get(&gstr(cb, "tool_use_id")) {
                            output_items[slot].results =
                                claude_web_search_results_to_responses(cb.get("content"));
                        }
                    }
                    "thinking" | "redacted_thinking" => {
                        let slot = new_output_item(
                            &mut output_items,
                            &mut block_to_item,
                            "reasoning",
                            idx,
                        );
                        output_items[slot].id = format!("rs_{response_id}_{idx}");
                        output_items[slot].signature = claude_reasoning_carrier(cb);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(d) = root.get("delta") else {
                    continue;
                };
                let idx = gi(root.get("index"));
                let item_slot = block_to_item.get(&idx).copied();
                let item_type = item_slot
                    .map(|s| output_items[s].item_type.clone())
                    .unwrap_or_default();
                match gstr(d, "type").as_str() {
                    "text_delta" => {
                        if let (Some(slot), "message") = (item_slot, item_type.as_str()) {
                            if let Some(t) = d.get("text") {
                                output_items[slot].text.push_str(&gs(Some(t)));
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(slot) = item_slot {
                            if matches!(
                                item_type.as_str(),
                                "function_call" | "custom_tool_call" | "web_search_call"
                            ) {
                                if let Some(pj) = d.get("partial_json") {
                                    output_items[slot].args.push_str(&gs(Some(pj)));
                                }
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let (Some(slot), "reasoning") = (item_slot, item_type.as_str()) {
                            if let Some(t) = d.get("thinking") {
                                output_items[slot].text.push_str(&gs(Some(t)));
                            }
                        }
                    }
                    "signature_delta" => {
                        if let (Some(slot), "reasoning") = (item_slot, item_type.as_str()) {
                            let signature = gstr(d, "signature");
                            if !signature.is_empty() {
                                output_items[slot].signature = signature;
                            }
                        }
                    }
                    "citations_delta" => {
                        if let Some(citation) = d.get("citation") {
                            if let (Some(slot), "message") = (item_slot, item_type.as_str()) {
                                output_items[slot].annotations.push(citation.clone());
                            } else if let Some(slot) = active_message_item {
                                output_items[slot].annotations.push(citation.clone());
                            } else {
                                pending_annotations.push(citation.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            // content_block_stop: output items are finalized after all deltas
            // have been aggregated.
            "message_delta" => {
                usage_tokens.merge(root.get("usage"));
                if let Some(value) = gp(root, "delta.stop_reason") {
                    stop_reason = gs(Some(value));
                }
            }
            _ => {}
        }
    }

    // Populate base fields
    let (_, response_status, incomplete_details) = claude_responses_terminal_state(&stop_reason);
    out["id"] = Value::from(response_id);
    out["created_at"] = Value::from(created_at);
    out["status"] = Value::from(response_status);
    if let Some(details) = incomplete_details {
        out["incomplete_details"] = details;
    }

    // Inject request echo fields as top-level (similar to streaming variant)
    if let Some(req) = &req {
        echo_request_fields(&mut out, req);
    }

    // Build output array in the order of the original content blocks.
    let mut outputs: Vec<Value> = Vec::with_capacity(output_items.len());
    let last = output_items.len().saturating_sub(1);
    for (i, output_item) in output_items.iter().enumerate() {
        let item_status = if response_status == "incomplete" && i == last {
            "incomplete"
        } else {
            "completed"
        };
        let item = match output_item.item_type.as_str() {
            "reasoning" => Some(json!({
                "id": output_item.id,
                "type": "reasoning",
                "status": item_status,
                "encrypted_content": output_item.signature,
                "summary": [{"type": "summary_text", "text": output_item.text}]
            })),
            "web_search_call" => {
                let mut item = build_responses_web_search_call_item(
                    &output_item.call_id,
                    &claude_web_search_query(&output_item.args),
                    output_item.results.as_ref(),
                );
                item["status"] = Value::from(item_status);
                Some(item)
            }
            "message" => {
                let mut item = json!({
                    "id": output_item.id,
                    "type": "message",
                    "status": item_status,
                    "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": output_item.text}],
                    "role": "assistant"
                });
                if !output_item.annotations.is_empty() {
                    item["content"][0]["annotations"] =
                        Value::Array(output_item.annotations.clone());
                }
                Some(item)
            }
            "custom_tool_call" => {
                let mut item = json!({
                    "id": output_item.id,
                    "type": "custom_tool_call",
                    "status": item_status,
                    "input": unwrap_custom_tool_input(&output_item.args),
                    "call_id": output_item.call_id,
                    "name": ""
                });
                apply_responses_function_call_namespace_fields(
                    &mut item,
                    req.as_ref(),
                    &output_item.name,
                    "",
                );
                Some(item)
            }
            "function_call" => {
                let mut args = output_item.args.clone();
                if args.is_empty() && item_status == "completed" {
                    args = "{}".to_string();
                }
                let mut item = json!({
                    "id": output_item.id,
                    "type": "function_call",
                    "status": item_status,
                    "arguments": args,
                    "call_id": output_item.call_id,
                    "name": ""
                });
                apply_responses_function_call_namespace_fields(
                    &mut item,
                    req.as_ref(),
                    &output_item.name,
                    "",
                );
                Some(item)
            }
            _ => None,
        };
        if let Some(item) = item {
            outputs.push(item);
        }
    }
    if !outputs.is_empty() {
        out["output"] = Value::Array(outputs);
    }

    // Usage
    let (input_tokens, output_tokens, total_tokens, cached_tokens) =
        usage_tokens.openai_responses_usage();
    if input_tokens != 0 {
        sj_set(&mut out, "usage.input_tokens", Value::from(input_tokens));
    }
    if cached_tokens != 0 {
        sj_set(
            &mut out,
            "usage.input_tokens_details.cached_tokens",
            Value::from(cached_tokens),
        );
    }
    if output_tokens != 0 {
        sj_set(&mut out, "usage.output_tokens", Value::from(output_tokens));
    }
    if total_tokens != 0 {
        sj_set(&mut out, "usage.total_tokens", Value::from(total_tokens));
    }
    let reasoning_length: usize = output_items
        .iter()
        .filter(|item| item.item_type == "reasoning")
        .map(|item| item.text.len())
        .sum();
    if reasoning_length > 0 {
        // Rough estimate similar to chat completions
        let reasoning_tokens = (reasoning_length / 4) as i64;
        if reasoning_tokens > 0 {
            sj_set(
                &mut out,
                "usage.output_tokens_details.reasoning_tokens",
                Value::from(reasoning_tokens),
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("bad test JSON {s}: {e}"))
    }

    fn convert(model: &str, raw: &str) -> Value {
        translate_request(model, &parse(raw), false)
    }

    fn convert_compat(model: &str, raw: &str) -> Value {
        translate_request_with_compat(model, &parse(raw), false)
    }

    // port of responsesRequestFromItems (claude_openai-responses_testsupport_test.go)
    fn request_from_items(items: &[&str]) -> String {
        format!(r#"{{"model":"claude-test","input":[{}]}}"#, items.join(","))
    }

    // port of claudeAssistantBlockTypes (claude_openai-responses_testsupport_test.go)
    fn assistant_block_types(out: &Value) -> Vec<String> {
        let mut kinds = Vec::new();
        if let Some(Value::Array(messages)) = out.get("messages") {
            for m in messages {
                if gstr(m, "role") != "assistant" {
                    continue;
                }
                kinds = content_blocks(m.get("content"))
                    .into_iter()
                    .map(|b| gstr(b, "type"))
                    .collect();
            }
        }
        kinds
    }

    fn append_varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    fn append_tag(buf: &mut Vec<u8>, num: u64, typ: u64) {
        append_varint(buf, (num << 3) | typ);
    }

    fn append_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
        append_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }

    // port of testClaudeResponsesThinkingSignatureForModel (claude_openai-responses_request_test.go)
    fn signature_for_model(model: &str) -> (String, String) {
        let mut channel_block = Vec::new();
        append_tag(&mut channel_block, 1, WIRE_VARINT);
        append_varint(&mut channel_block, 12);
        append_tag(&mut channel_block, 2, WIRE_VARINT);
        append_varint(&mut channel_block, 2);
        append_tag(&mut channel_block, 6, WIRE_BYTES);
        append_bytes(&mut channel_block, model.as_bytes());

        let mut container = Vec::new();
        append_tag(&mut container, 1, WIRE_BYTES);
        append_bytes(&mut container, &channel_block);

        let mut payload = Vec::new();
        append_tag(&mut payload, 2, WIRE_BYTES);
        append_bytes(&mut payload, &container);
        append_tag(&mut payload, 3, WIRE_VARINT);
        append_varint(&mut payload, 1);

        let raw = STANDARD.encode(&payload);
        let normalized = compatible_signature_for_claude(&raw)
            .expect("test Claude signature should be compatible");
        (raw, normalized)
    }

    // port of testClaudeResponsesThinkingSignature (claude_openai-responses_request_test.go)
    fn test_signature() -> (String, String) {
        signature_for_model("claude-sonnet-4-6")
    }

    // port of testGPTResponsesReasoningSignature (claude_openai-responses_request_test.go)
    fn gpt_signature() -> String {
        let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
        payload[0] = 0x80;
        payload[8] = 1;
        for (i, b) in payload.iter_mut().enumerate().skip(9) {
            *b = i as u8;
        }
        URL_SAFE.encode(&payload)
    }

    // port of responsesReasoningItem / responsesFunctionCallItem /
    // responsesFunctionCallOutputItem (claude_openai-responses_reasoning_order_test.go)
    fn reasoning_item(signature: &str, text: &str) -> String {
        json!({"type": "reasoning", "encrypted_content": signature, "summary": [{"type": "summary_text", "text": text}]}).to_string()
    }

    fn function_call_item(call_id: &str, name: &str) -> String {
        json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": "{}"})
            .to_string()
    }

    fn function_call_output_item(call_id: &str, output: &str) -> String {
        json!({"type": "function_call_output", "call_id": call_id, "output": output}).to_string()
    }

    /// One client frame → (event, data), checking the wire shape.
    fn parse_frame(frame: &str) -> (String, Value) {
        let body = frame
            .strip_suffix("\n\n")
            .expect("frame ends with a blank line");
        let (event_line, data_line) = body.split_once('\n').expect("event + data lines");
        let event = event_line.strip_prefix("event: ").expect("event line");
        let data = data_line.strip_prefix("data: ").expect("data line");
        let data = parse(data);
        assert_eq!(
            gstr(&data, "type"),
            event,
            "frame event matches payload type"
        );
        (event.to_string(), data)
    }

    /// Drive the stream translator with the JSON payloads of `data:` lines.
    fn run_stream(original: &Value, chunks: &[&str]) -> Vec<(String, Value)> {
        let mut translator = StreamTranslator::new(original);
        let mut frames = Vec::new();
        for chunk in chunks {
            for frame in translator.push(None, &parse(chunk)) {
                frames.push(parse_frame(&frame));
            }
        }
        for frame in translator.finish() {
            frames.push(parse_frame(&frame));
        }
        frames
    }

    fn stream(chunks: &[&str]) -> Vec<(String, Value)> {
        run_stream(&Value::Null, chunks)
    }

    fn last_event(frames: &[(String, Value)], event: &str) -> Value {
        frames
            .iter()
            .rev()
            .find(|(e, _)| e == event)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("no {event} event"))
    }

    fn completed(frames: &[(String, Value)]) -> Value {
        last_event(frames, "response.completed")
    }

    fn count(frames: &[(String, Value)], event: &str) -> usize {
        frames.iter().filter(|(e, _)| e == event).count()
    }

    fn item_done(frames: &[(String, Value)], item_type: &str) -> Vec<Value> {
        frames
            .iter()
            .filter(|(e, d)| e == "response.output_item.done" && gstr(d, "item.type") == item_type)
            .map(|(_, d)| d.clone())
            .collect()
    }

    fn output_types(output: &Value) -> Vec<String> {
        output
            .as_array()
            .map(|items| items.iter().map(|i| gstr(i, "type")).collect())
            .unwrap_or_default()
    }

    fn non_stream(original: &Value, chunks: &[&str]) -> Value {
        let body: Vec<String> = chunks.iter().map(|c| format!("data: {c}")).collect();
        translate_non_stream(&Value::from(body.join("\n")), original)
    }

    fn lifecycle(frames: &[(String, Value)]) -> Vec<String> {
        frames
            .iter()
            .filter(|(e, _)| e == "response.output_item.added" || e == "response.output_item.done")
            .map(|(e, d)| {
                format!(
                    "{}:{}:{}",
                    e,
                    gi(d.get("output_index")),
                    gstr(d, "item.type")
                )
            })
            .collect()
    }

    const MSG_START: &str = r#"{"type":"message_start","message":{"id":"msg_123","usage":{"input_tokens":1,"output_tokens":0}}}"#;
    const MSG_STOP: &str = r#"{"type":"message_stop"}"#;

    // ─────────────── request tests (claude_openai-responses_request_test.go) ───────────────

    #[test]
    fn sanitizes_tool_call_ids_for_claude() {
        let out = convert(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","input":[
                {"type":"function_call","call_id":"call.with space:1","name":"Read","arguments":"{\"path\":\"README.md\"}"},
                {"type":"function_call_output","call_id":"call.with space:1","output":"ok"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_with_space_1", "name": "Read", "input": {"path": "README.md"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_with_space_1", "content": "ok"}]}
            ])
        );
    }

    // The two "registered model maximum" subtests need the model registry,
    // which Termory does not have.
    #[test]
    fn fable_max_tokens() {
        let out = translate_request(
            "claude-fable-5-1",
            &parse(r#"{"model":"claude-fable-5-1","input":"hello"}"#),
            true,
        );
        assert_eq!(out["max_tokens"], json!(64000));
        let out = translate_request(
            "claude-fable-5-1",
            &parse(r#"{"model":"claude-fable-5-1","max_output_tokens":128000,"input":"hello"}"#),
            true,
        );
        assert_eq!(out["max_tokens"], json!(128000));
        let out = translate_request(
            "claude-fable-5-1",
            &parse(r#"{"model":"claude-fable-5-1","max_output_tokens":null,"input":"hello"}"#),
            true,
        );
        assert_eq!(out["max_tokens"], json!(64000));
    }

    #[test]
    fn full_request_shape() {
        let out = convert(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-5","input":"hi","max_output_tokens":16,"prompt_cache_key":"k"}"#,
        );
        assert_eq!(
            out,
            json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}],
                "metadata": {"user_id": sha256_hex("prompt_cache_key:k")},
                "stream": false
            })
        );
    }

    #[test]
    fn reasoning_item_to_thinking_block() {
        let (raw_sig, expected_sig) = test_signature();
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{raw_sig}","summary":[{{"type":"summary_text","text":"internal reasoning"}}]}},
                {{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"visible answer"}}]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "internal reasoning", "signature": expected_sig},
                    {"type": "text", "text": "visible answer"}
                ]},
                {"role": "user", "content": "continue"}
            ])
        );
    }

    #[test]
    fn signature_only_reasoning_flushes_before_user() {
        let (raw_sig, expected_sig) = test_signature();
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{raw_sig}","summary":[]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": expected_sig}]},
                {"role": "user", "content": "continue"}
            ])
        );
    }

    #[test]
    fn redacted_reasoning_item_restores_redacted_thinking() {
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}EroBCkYIBRgCKkA","summary":[]}},
                {{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"visible answer"}}]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "EroBCkYIBRgCKkA"},
                    {"type": "text", "text": "visible answer"}
                ]},
                {"role": "user", "content": "continue"}
            ])
        );
    }

    #[test]
    fn empty_redacted_reasoning_item_is_dropped() {
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}","summary":[]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "continue"}])
        );
    }

    #[test]
    fn reasoning_content_text_rebuilds_thinking() {
        let (raw_sig, expected_sig) = test_signature();
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{raw_sig}","summary":[],"content":[{{"type":"reasoning_text","text":"restored from content"}}]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"][0]["content"][0],
            json!({"type": "thinking", "thinking": "restored from content", "signature": expected_sig})
        );
    }

    #[test]
    fn summary_sets_thinking_display() {
        for (summary, want) in [
            (r#","summary":"auto""#, Some("summarized")),
            (r#","summary":"concise""#, Some("summarized")),
            (r#","summary":"none""#, Some("omitted")),
            ("", None),
        ] {
            let raw = format!(
                r#"{{"model":"claude-opus-5-5","reasoning":{{"effort":"high"{summary}}},"input":"hi"}}"#
            );
            let out = convert("claude-opus-5-5", &raw);
            // Opus 5.5: adaptive thinking (budget_tokens is a 400).
            let mut thinking = json!({"type": "adaptive"});
            if let Some(display) = want {
                thinking["display"] = json!(display);
            }
            assert_eq!(out["thinking"], thinking, "summary {summary:?}");
            assert_eq!(out["output_config"]["effort"], json!("high"));
        }
    }

    #[test]
    fn summary_wins_over_duplicated_reasoning_content() {
        let (raw_sig, _) = test_signature();
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{raw_sig}","summary":[{{"type":"summary_text","text":"chain of thought"}}],"content":[{{"type":"reasoning_text","text":"chain of thought"}}]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"][0]["content"][0]["thinking"],
            json!("chain of thought")
        );
    }

    #[test]
    fn drops_incompatible_reasoning_signature() {
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{}","summary":[{{"type":"summary_text","text":"must not become Claude thinking"}}]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"continue"}}]}}]}}"#,
            gpt_signature()
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "continue"}])
        );
    }

    #[test]
    fn groups_assistant_and_tool_result_turns() {
        let (raw_sig, expected_sig) = test_signature();
        let raw = format!(
            r#"{{"model":"claude-test","input":[
                {{"type":"reasoning","encrypted_content":"{raw_sig}","summary":[{{"type":"summary_text","text":"internal reasoning"}}]}},
                {{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"visible answer"}}]}},
                {{"type":"function_call","call_id":"call_first","name":"read_file","arguments":"{{\"path\":\"first\"}}"}},
                {{"type":"function_call","call_id":"call_second","name":"read_file","arguments":"{{\"path\":\"second\"}}"}},
                {{"type":"function_call_output","call_id":"call_first","output":"first result"}},
                {{"type":"function_call_output","call_id":"call_second","output":"second result"}}]}}"#
        );
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "internal reasoning", "signature": expected_sig},
                    {"type": "text", "text": "visible answer"},
                    {"type": "tool_use", "id": "call_first", "name": "read_file", "input": {"path": "first"}},
                    {"type": "tool_use", "id": "call_second", "name": "read_file", "input": {"path": "second"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_first", "content": "first result"},
                    {"type": "tool_result", "tool_use_id": "call_second", "content": "second result"}
                ]}
            ])
        );
    }

    #[test]
    fn merges_consecutive_user_messages_and_preserves_cache_control() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"user","cache_control":{"type":"ephemeral"},"content":[{"type":"input_text","text":"first"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "first", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "second"}
            ]}])
        );
    }

    #[test]
    fn does_not_merge_across_role_changes() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"first assistant"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"user reply"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"second assistant"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": "first assistant"},
                {"role": "user", "content": "user reply"},
                {"role": "assistant", "content": "second assistant"}
            ])
        );
    }

    #[test]
    fn empty_string_content_does_not_break_assistant_turn() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"assistant","content":"first assistant"},
                {"type":"message","role":"user","content":""},
                {"type":"message","role":"assistant","content":"second assistant"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "assistant", "content": [
                {"type": "text", "text": "first assistant"},
                {"type": "text", "text": "second assistant"}
            ]}])
        );
    }

    #[test]
    fn function_call_output_preserves_input_image() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"function_call","call_id":"call_view_image_1","name":"view_image","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_view_image_1","output":[{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}]}]}"#,
        );
        assert_eq!(
            out["messages"][1]["content"][0],
            json!({"type": "tool_result", "tool_use_id": "call_view_image_1", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}
            ]})
        );
    }

    #[test]
    fn standalone_tool_output_becomes_user_text() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"function_call_output","call_id":"toolu_1789312108939888000_16","output":"<codex_delegation>Launched from another task.</codex_delegation>"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with PROBE_OK."}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "<codex_delegation>Launched from another task.</codex_delegation>"},
                {"type": "text", "text": "Reply with PROBE_OK."}
            ]}])
        );
    }

    fn interrupted(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "is_error": true, "content": "Tool call was interrupted before any output was recorded."})
    }

    #[test]
    fn synthesizes_result_for_dangling_tool_use() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Run the tests."}]},
                {"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"cmd\":\"npm test\"}"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Session was restarted; carry on."}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": "Run the tests."},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "shell", "input": {"cmd": "npm test"}}]},
                {"role": "user", "content": [interrupted("call_1"), {"type": "text", "text": "Session was restarted; carry on."}]}
            ])
        );
    }

    #[test]
    fn synthesizes_result_for_trailing_tool_use() {
        let raw = r#"{"model":"claude-test","input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Run ls."}]},
            {"type":"function_call","call_id":"call_tail","name":"exec","arguments":"{}"}]}"#;
        let want = json!([
            {"role": "user", "content": "Run ls."},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "call_tail", "name": "exec", "input": {}}]},
            {"role": "user", "content": [interrupted("call_tail")]}
        ]);
        assert_eq!(convert("claude-test", raw)["messages"], want);
        // Models that reject assistant prefill must keep the synthesized answer too.
        assert_eq!(convert("claude-fable-5-1", raw)["messages"], want);
    }

    #[test]
    fn moves_tool_results_ahead_of_injected_text() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Run ls."}]},
                {"type":"function_call","call_id":"call_hb","name":"exec","arguments":"{}"},
                {"type":"function_call_output","id":"fco_seed","name":"automation_update","namespace":"codex_app","output":"<heartbeat>tick</heartbeat>"},
                {"type":"function_call_output","call_id":"call_hb","output":"a.txt"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Continue."}]}]}"#,
        );
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
        assert_eq!(
            out["messages"][2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_hb", "content": "a.txt"},
                {"type": "text", "text": "<heartbeat>tick</heartbeat>"},
                {"type": "text", "text": "Continue."}
            ]})
        );
    }

    #[test]
    fn late_orphan_tool_result_folds_to_text() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Run ls."}]},
                {"type":"function_call","call_id":"call_a","name":"exec","arguments":"{}"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"wait"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"interlude"}]},
                {"type":"function_call_output","call_id":"call_a","output":"a.txt"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": "Run ls."},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_a", "name": "exec", "input": {}}]},
                {"role": "user", "content": [interrupted("call_a"), {"type": "text", "text": "wait"}]},
                {"role": "assistant", "content": "interlude"},
                {"role": "user", "content": [{"type": "text", "text": "a.txt"}]}
            ])
        );
    }

    #[test]
    fn empty_standalone_tool_output_keeps_marker() {
        for output in [r#""""#, r#"[{"type":"input_text","text":""}]"#] {
            let raw = format!(
                r#"{{"model":"claude-test","input":[{{"type":"function_call_output","call_id":"orphan","output":{output}}}]}}"#
            );
            let out = convert("claude-test", &raw);
            assert_eq!(
                out["messages"],
                json!([{"role": "user", "content": "Tool result was empty."}]),
                "output {output}"
            );
        }
    }

    #[test]
    fn sanitized_id_collision_keeps_orphan_as_text() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Run."}]},
                {"type":"function_call","call_id":"call.custom:1","name":"exec","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_custom_1","output":"unrelated context"},
                {"type":"function_call_output","call_id":"call.custom:1","output":"real result"}]}"#,
        );
        assert_eq!(
            out["messages"][2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_custom_1", "content": "real result"},
                {"type": "text", "text": "unrelated context"}
            ]})
        );
    }

    #[test]
    fn empty_late_orphan_tool_result_keeps_non_empty_user_message() {
        let out = convert(
            "claude-fable-5-1",
            r#"{"model":"claude-fable-5-1","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Run the tool."}]},
                {"type":"function_call","call_id":"call_a","name":"exec","arguments":"{}"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Wait."}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Interlude."}]},
                {"type":"function_call_output","call_id":"call_a","output":""}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": "Run the tool."},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_a", "name": "exec", "input": {}}]},
                {"role": "user", "content": [interrupted("call_a"), {"type": "text", "text": "Wait."}]},
                {"role": "assistant", "content": "Interlude."},
                {"role": "user", "content": [{"type": "text", "text": "Tool result was empty."}]}
            ])
        );
    }

    #[test]
    fn late_orphan_tool_result_array_of_empty_text_keeps_marker() {
        let out = convert(
            "claude-fable-5-1",
            r#"{"model":"claude-fable-5-1","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Run."}]},
                {"type":"function_call","call_id":"a","name":"exec","arguments":"{}"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Wait."}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Interlude."}]},
                {"type":"function_call_output","call_id":"a","output":[{"type":"input_text","text":""},{"type":"input_text","text":""}]}]}"#,
        );
        assert_eq!(
            out["messages"][4],
            json!({"role": "user", "content": [{"type": "text", "text": "Tool result was empty."}]})
        );
    }

    #[test]
    fn standalone_tool_output_drops_empty_text_in_mixed_array() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"function_call_output","call_id":"orphan","output":[{"type":"input_text","text":""},{"type":"input_text","text":"context"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "context"}])
        );
    }

    #[test]
    fn keeps_tool_use_adjacent_to_tool_result() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"function_call","call_id":"call_00_awGuheXs4aRbtedNK8LE3743","name":"js","arguments":"{\"code\":\"nodeRepl.write('ok')\",\"title\":\"List Obsidian vault contents\"}"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"I'll check your Obsidian vault for articles."}]},
                {"type":"function_call_output","call_id":"call_00_awGuheXs4aRbtedNK8LE3743","output":"Wall time: 0.1963 seconds\nOutput:\n[{\"type\":\"text\",\"text\":\"\"}]"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "text", "text": "I'll check your Obsidian vault for articles."},
                    {"type": "tool_use", "id": "call_00_awGuheXs4aRbtedNK8LE3743", "name": "js", "input": {"code": "nodeRepl.write('ok')", "title": "List Obsidian vault contents"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_00_awGuheXs4aRbtedNK8LE3743", "content": "Wall time: 0.1963 seconds\nOutput:\n[{\"type\":\"text\",\"text\":\"\"}]"}
                ]}
            ])
        );
    }

    #[test]
    fn drops_apply_patch_custom_tool() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
                "tools":[
                    {"type":"custom","name":"apply_patch","description":"Use the apply_patch tool to edit files.","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}},
                    {"type":"function","name":"exec_command","description":"Runs a command.","parameters":{"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}}]}"#,
        );
        assert_eq!(
            out["tools"],
            json!([{"name": "exec_command", "description": "Runs a command.", "input_schema": {"properties": {"cmd": {"type": "string"}}, "required": ["cmd"], "type": "object"}}])
        );
    }

    #[test]
    fn normalizes_root_tool_schema_union() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
                "tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"query":{"type":"string"},"id":{"type":"string"}},"oneOf":[{"required":["query"]},{"required":["id"]}]}}]}"#,
        );
        assert_eq!(
            out["tools"][0]["input_schema"],
            json!({"properties": {"id": {"type": "string"}, "query": {"type": "string"}}, "type": "object"})
        );
    }

    fn empty_schema() -> Value {
        json!({"properties": {}, "type": "object"})
    }

    fn custom_schema() -> Value {
        json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]})
    }

    #[test]
    fn merges_additional_tools_and_prefers_top_level() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test",
                "tools":[
                    {"type":"function","name":"exec","description":"top-level exec","parameters":{"type":"object","properties":{"command":{"type":"string"}}}},
                    {"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn","description":"top-level spawn","parameters":{"type":"object","properties":{}}}]}],
                "input":[
                    {"type":"additional_tools","role":"developer","tools":[
                        {"type":"custom","name":"exec","description":"additional exec"},
                        {"type":"function","name":"wait","parameters":{"type":"object","properties":{}}},
                        {"type":"namespace","name":"collaboration","tools":[
                            {"type":"function","name":"spawn","parameters":{"type":"object","properties":{}}},
                            {"type":"custom","name":"send","description":"send a message"}]}]},
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
        );
        assert_eq!(
            out["tools"],
            json!([
                {"name": "exec", "description": "top-level exec", "input_schema": {"properties": {"command": {"type": "string"}}, "type": "object"}},
                {"name": "collaboration__spawn", "description": "top-level spawn", "input_schema": empty_schema()},
                {"name": "wait", "description": "", "input_schema": empty_schema()},
                {"name": "collaboration__send", "description": "send a message", "input_schema": custom_schema()}
            ])
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "hello"}])
        );
    }

    #[test]
    fn deduplicates_expanded_tool_names() {
        let raw = parse(
            r#"{"model":"claude-test",
                "tools":[{"type":"function","name":"collaboration__send","description":"top-level send","parameters":{"type":"object","properties":{}}}],
                "input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"send","description":"additional send","parameters":{"type":"object","properties":{}}},
                    {"type":"function","name":"other","parameters":{"type":"object","properties":{}}}]}]}]}"#,
        );
        let out = translate_request("claude-test", &raw, false);
        assert_eq!(
            out["tools"],
            json!([
                {"name": "collaboration__send", "description": "top-level send", "input_schema": empty_schema()},
                {"name": "collaboration__other", "description": "", "input_schema": empty_schema()}
            ])
        );
        assert_eq!(responses_custom_tool_names(Some(&raw)), HashSet::new());
        assert_eq!(
            split_responses_qualified_function_call_from_request(Some(&raw), "collaboration__send"),
            ("collaboration__send".to_string(), String::new())
        );
    }

    #[test]
    fn direct_tool_wins_over_earlier_namespace_collision() {
        let raw = parse(
            r#"{"model":"claude-test",
                "tools":[
                    {"type":"namespace","name":"n","tools":[{"type":"function","name":"x","parameters":{"type":"object","properties":{}}}]},
                    {"type":"custom","name":"n__x"}],
                "tool_choice":{"type":"custom","name":"n__x"}}"#,
        );
        let out = translate_request("claude-test", &raw, false);
        assert_eq!(
            out["tools"],
            json!([{"name": "n__x", "description": "", "input_schema": custom_schema()}])
        );
        assert_eq!(out["tool_choice"], json!({"name": "n__x", "type": "tool"}));
        assert_eq!(
            responses_custom_tool_names(Some(&raw)),
            HashSet::from(["n__x".to_string()])
        );
    }

    #[test]
    fn prefers_direct_tool_across_additional_sources() {
        let raw = parse(
            r#"{"model":"claude-test","input":[
                {"type":"additional_tools","tools":[{"type":"namespace","name":"n","tools":[{"type":"function","name":"x","description":"namespace x","parameters":{"type":"object","properties":{}}}]}]},
                {"type":"additional_tools","tools":[{"type":"custom","name":"n__x","description":"direct x"}]}]}"#,
        );
        let out = translate_request("claude-test", &raw, false);
        assert_eq!(
            out["tools"],
            json!([{"name": "n__x", "description": "direct x", "input_schema": custom_schema()}])
        );
        assert_eq!(
            responses_custom_tool_names(Some(&raw)),
            HashSet::from(["n__x".to_string()])
        );
    }

    #[test]
    fn preserves_tool_declaration_order() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","tools":[
                {"type":"function","name":"first","parameters":{"type":"object","properties":{}}},
                {"type":"namespace","name":"n","tools":[{"type":"function","name":"middle","parameters":{"type":"object","properties":{}}}]},
                {"type":"function","name":"last","parameters":{"type":"object","properties":{}}}]}"#,
        );
        let names: Vec<Value> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].clone())
            .collect();
        assert_eq!(
            names,
            vec![json!("first"), json!("n__middle"), json!("last")]
        );
    }

    #[test]
    fn replays_custom_tool_call_history() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"custom_tool_call","call_id":"call.custom:1","name":"exec","input":"pwd"},
                {"type":"custom_tool_call_output","call_id":"call.custom:1","output":"/workspace"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_custom_1", "name": "exec", "input": {"input": "pwd"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_custom_1", "content": "/workspace"}]}
            ])
        );
    }

    #[test]
    fn replays_namespaced_function_call_history() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"additional_tools","tools":[{"type":"namespace","name":"mcp__node_repl","tools":[{"type":"function","name":"js","parameters":{"type":"object","properties":{}}}]}]},
                {"type":"function_call","call_id":"call.namespace","name":"js","namespace":"mcp__node_repl","arguments":"{\"code\":\"pwd\"}"},
                {"type":"function_call_output","call_id":"call.namespace","output":"ok"}]}"#,
        );
        assert_eq!(
            out["tools"],
            json!([{"name": "mcp__node_repl__js", "description": "", "input_schema": empty_schema()}])
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_namespace", "name": "mcp__node_repl__js", "input": {"code": "pwd"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_namespace", "content": "ok"}]}
            ])
        );
    }

    #[test]
    fn maps_custom_and_namespaced_tool_choice() {
        for (raw, want) in [
            (
                r#"{"model":"claude-test","tools":[{"type":"custom","name":"exec"}],"tool_choice":{"type":"custom","name":"exec"}}"#,
                "exec",
            ),
            (
                r#"{"model":"claude-test","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"mcp__node_repl","tools":[{"type":"function","name":"js"}]}]}],"tool_choice":{"type":"function","name":"js","namespace":"mcp__node_repl"}}"#,
                "mcp__node_repl__js",
            ),
            (
                r#"{"model":"claude-test","tools":[{"type":"function","name":"foo"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"mcp__tools","tools":[{"type":"function","name":"foo"}]}]}],"tool_choice":{"type":"function","name":"foo"}}"#,
                "foo",
            ),
        ] {
            assert_eq!(
                convert("claude-test", raw)["tool_choice"],
                json!({"name": want, "type": "tool"})
            );
        }
    }

    #[test]
    fn maps_string_tool_choice() {
        let tools = r#""tools":[{"type":"function","name":"exec"}]"#;
        for (choice, want) in [
            ("auto", Some(json!({"type": "auto"}))),
            ("required", Some(json!({"type": "any"}))),
            ("none", None),
        ] {
            let out = convert(
                "claude-test",
                &format!(r#"{{"model":"claude-test",{tools},"tool_choice":"{choice}"}}"#),
            );
            assert_eq!(
                out.get("tool_choice").cloned(),
                want,
                "tool_choice {choice}"
            );
        }
        // "required" with no surviving tool is left unset.
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","tool_choice":"required"}"#,
        );
        assert_eq!(out.get("tool_choice"), None);
    }

    #[test]
    fn qualify_responses_namespace_tool_name_avoids_prefix_collision() {
        for (namespace, child, want) in [
            ("collab", "collaboration", "collab__collaboration"),
            ("collab", "collab__send", "collab__send"),
            ("collab__", "send", "collab__send"),
            ("mcp__node_repl", "mcp__node_repl__js", "mcp__node_repl__js"),
        ] {
            assert_eq!(
                qualify_responses_namespace_tool_name(namespace, child),
                want
            );
        }
        let out = convert(
            "claude-test",
            r#"{"tools":[{"type":"namespace","name":"collab","tools":[{"type":"function","name":"collaboration"}]}]}"#,
        );
        assert_eq!(out["tools"][0]["name"], json!("collab__collaboration"));
    }

    #[test]
    fn split_responses_qualified_function_call_from_additional_tools() {
        let raw = parse(
            r#"{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"mcp__node_repl","tools":[{"type":"function","name":"js"}]}]}]}"#,
        );
        assert_eq!(
            split_responses_qualified_function_call_from_request(Some(&raw), "mcp__node_repl__js"),
            ("js".to_string(), "mcp__node_repl".to_string())
        );
    }

    #[test]
    fn preserves_content_part_cache_control() {
        let out = convert(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","input":[{"type":"message","role":"user","content":[
                {"type":"input_text","text":"cached prefix","cache_control":{"type":"ephemeral"}},
                {"type":"input_text","text":"fresh question"}]}]}"#,
        );
        assert_eq!(
            out["messages"][0]["content"],
            json!([
                {"type": "text", "text": "cached prefix", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "fresh question"}
            ])
        );
    }

    #[test]
    fn system_level_inputs_become_separate_system_blocks() {
        let out = convert(
            "claude-sonnet-4-5",
            r#"{"model":"gpt-4.1","instructions":"I1","input":[
                {"type":"message","role":"system","content":[{"type":"input_text","text":"S1"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"U1"}]},
                {"type":"message","role":"developer","content":"D1"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"A1"}]},
                {"type":"message","role":"system","content":[{"type":"input_text","text":"S2"}]}]}"#,
        );
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": "I1"},
                {"type": "text", "text": "S1"},
                {"type": "text", "text": "D1"},
                {"type": "text", "text": "S2"}
            ])
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "U1"}, {"role": "assistant", "content": "A1"}])
        );
    }

    #[test]
    fn system_only_input_keeps_fallback_user_message() {
        let out = convert(
            "claude-opus-5",
            r#"{"model":"gpt-4.1","instructions":"I1"}"#,
        );
        assert_eq!(out["system"], json!([{"type": "text", "text": "I1"}]));
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": ""}]}])
        );
    }

    #[test]
    fn system_non_text_part_kept_as_typed_marker() {
        let out = convert(
            "claude-opus-5",
            r#"{"model":"gpt-4.1","input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"D1"},{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"U1"}]}]}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": "D1"}, {"type": "input_image"}])
        );
    }

    #[test]
    fn system_item_cache_control_applies_to_last_block() {
        let out = convert(
            "claude-opus-5",
            r#"{"model":"gpt-4.1","input":[
                {"type":"message","role":"system","cache_control":{"type":"ephemeral"},"content":[{"type":"input_text","text":"S1"},{"type":"input_text","text":"S2"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"U1"}]}]}"#,
        );
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": "S1"},
                {"type": "text", "text": "S2", "cache_control": {"type": "ephemeral"}}
            ])
        );
    }

    #[test]
    fn deduplicates_tool_outputs() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Use lookup."}]},
                {"type":"function_call","call_id":"toolu_dup","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"toolu_dup","output":"first result"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Intermediate step"}]},
                {"type":"function_call","call_id":"toolu_parallel","name":"other","arguments":"{}"},
                {"type":"function_call_output","call_id":"toolu_dup","output":"final result"},
                {"type":"custom_tool_call_output","call_id":"call.custom:dup","output":"custom first"},
                {"type":"custom_tool_call_output","call_id":"call.custom:dup","output":"custom final"},
                {"type":"function_call_output","call_id":"toolu_parallel","output":"parallel result"},
                {"type":"function_call_output","call_id":"","output":"empty id output"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": "Use lookup."},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_dup", "name": "lookup", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_dup", "content": "final result"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Intermediate step"},
                    {"type": "tool_use", "id": "toolu_parallel", "name": "other", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_parallel", "content": "parallel result"},
                    {"type": "text", "text": "custom final"},
                    {"type": "text", "text": "empty id output"}
                ]}
            ])
        );
    }

    #[test]
    fn service_tier_to_speed() {
        for (tier, effort, want) in [
            (None, None, None),
            (Some("default"), None, None),
            (Some("standard"), None, None),
            (Some("flex"), None, None),
            (Some("priority"), None, Some("fast")),
            (Some("priority"), Some("low"), Some("fast")),
            (Some("priority"), Some("medium"), Some("fast")),
            (Some("priority"), Some("high"), Some("fast")),
            (Some("priority"), Some("xhigh"), Some("fast")),
            (Some("priority"), Some("max"), Some("fast")),
        ] {
            let mut raw = json!({"model": "claude-3-7-sonnet-20250219", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]});
            if let Some(tier) = tier {
                raw["service_tier"] = json!(tier);
            }
            if let Some(effort) = effort {
                raw["reasoning"] = json!({"effort": effort});
            }
            let out = translate_request("claude-3-7-sonnet-20250219", &raw, false);
            assert_eq!(
                out.get("speed").cloned(),
                want.map(Value::from),
                "tier {tier:?} effort {effort:?}"
            );
        }
    }

    // A Codex MCP tool name longer than Claude's 64-char limit is declared
    // truncated; the call Claude makes with the truncated name comes back
    // under the name Codex declared.
    #[test]
    fn truncated_tool_name_is_restored_on_the_way_back() {
        let long = format!("mcp__some-very-long-server-name__{}", "x".repeat(70));
        let request = json!({"model": "claude-sonnet-4-5", "input": "hi",
            "tools": [{"type": "function", "name": long, "parameters": {"type": "object", "properties": {}}}]});
        let declared = sanitize_claude_function_name(&long);
        assert_eq!(declared.len(), 64);
        let (name, ns) =
            split_responses_qualified_function_call_from_request(Some(&request), &declared);
        assert_eq!(name, long);
        assert_eq!(ns, "");
    }

    // Codex's default `reasoning.effort` against a current Claude model:
    // adaptive thinking with the effort — `budget_tokens` is a 400 there.
    #[test]
    fn codex_effort_against_current_claude_models_is_adaptive() {
        for (model, effort, want_effort) in [
            ("claude-opus-5-5", "medium", "medium"),
            ("claude-sonnet-5", "xhigh", "xhigh"),
            ("claude-fable-5-1", "none", "low"),
            ("claude-opus-4-7", "high", "high"),
        ] {
            let out = convert(
                model,
                &format!(r#"{{"input":"hi","reasoning":{{"effort":"{effort}"}}}}"#),
            );
            assert_eq!(
                out["thinking"],
                json!({"type": "adaptive"}),
                "{model} {effort}"
            );
            assert_eq!(
                out["output_config"]["effort"],
                json!(want_effort),
                "{model} {effort}"
            );
        }
    }

    #[test]
    fn reasoning_effort_maps_to_manual_thinking_without_registry() {
        for (effort, want) in [
            ("none", json!({"type": "disabled"})),
            ("auto", json!({"type": "enabled", "budget_tokens": 8192})),
            ("minimal", json!({"type": "enabled", "budget_tokens": 512})),
            // 32768 is capped under the default max_tokens (32000).
            ("xhigh", json!({"type": "enabled", "budget_tokens": 31999})),
        ] {
            let out = convert(
                "claude-sonnet-4-5",
                &format!(r#"{{"input":"hi","reasoning":{{"effort":"{effort}"}}}}"#),
            );
            assert_eq!(out["thinking"], want, "effort {effort}");
        }
        let out = convert(
            "claude-sonnet-4-5",
            r#"{"input":"hi","reasoning":{"effort":"bogus"}}"#,
        );
        assert_eq!(out.get("thinking"), None);
    }

    #[test]
    fn preserves_caller_supplied_metadata_user_id() {
        for (raw, want) in [
            (
                r#"{"model":"claude-test","metadata":{"user_id":"custom-resp-user-123"},"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
                "custom-resp-user-123",
            ),
            (
                r#"{"model":"claude-test","metadata":{"user_id":"foo\"bar\nbaz\\qux"},"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
                "foo\"bar\nbaz\\qux",
            ),
            (
                r#"{"model":"claude-test","metadata":{"user_id":"{\"device_id\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"session_id\":\"11111111-2222-4333-8444-555555555555\"}"},"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
                r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#,
            ),
        ] {
            assert_eq!(
                convert("claude-test", raw)["metadata"],
                json!({"user_id": want})
            );
        }
    }

    #[test]
    fn preserves_user_field() {
        let out = convert(
            "claude-test",
            r#"{"model":"claude-test","user":"openai-resp-user-456","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
        );
        assert_eq!(out["metadata"], json!({"user_id": "openai-resp-user-456"}));
    }

    #[test]
    fn different_sessions_produce_different_user_ids() {
        let a = convert(
            "claude-test",
            r#"{"model":"claude-test","prompt_cache_key":"resp-session-a","input":"hello"}"#,
        );
        let b = convert(
            "claude-test",
            r#"{"model":"claude-test","prompt_cache_key":"resp-session-b","input":"hello"}"#,
        );
        assert_eq!(
            a["metadata"]["user_id"],
            json!(sha256_hex("prompt_cache_key:resp-session-a"))
        );
        assert_eq!(
            b["metadata"]["user_id"],
            json!(sha256_hex("prompt_cache_key:resp-session-b"))
        );
    }

    #[test]
    fn different_user_content_with_same_system_prompt() {
        let raw = |q: &str| {
            format!(
                r#"{{"model":"claude-test","instructions":"global instruction","input":[
                    {{"type":"message","role":"system","content":"system context"}},
                    {{"type":"message","role":"user","content":"user question {q}"}}]}}"#
            )
        };
        let a = convert("claude-test", &raw("A"));
        let b = convert("claude-test", &raw("B"));
        assert_eq!(
            a["metadata"]["user_id"],
            json!(sha256_hex("content:user question A"))
        );
        assert_eq!(
            b["metadata"]["user_id"],
            json!(sha256_hex("content:user question B"))
        );
    }

    fn prefill_request(model: &str, text: &str) -> String {
        format!(
            r#"{{"model":"{model}","input":[
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}},
                {{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}]}}"#
        )
    }

    #[test]
    fn unsupported_prefill_models_strip_trailing_assistant() {
        for model in ["claude-fable-5", "claude-opus-5", "claude-sonnet-4-6"] {
            let out = convert(model, &prefill_request(model, "progress update"));
            assert_eq!(
                out["messages"],
                json!([{"role": "user", "content": "hello"}]),
                "model {model}"
            );
        }
    }

    #[test]
    fn fable_only_assistant_message_yields_fallback_user() {
        let out = convert(
            "claude-fable-5",
            r#"{"model":"claude-fable-5","input":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"orphan progress"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": ""}]}])
        );
    }

    #[test]
    fn supported_prefill_models_preserve_assistant_prefill() {
        for model in ["claude-sonnet-4-5", "claude-haiku-4-5"] {
            let out = convert(model, &prefill_request(model, "prefill text"));
            assert_eq!(
                out["messages"],
                json!([{"role": "user", "content": "hello"}, {"role": "assistant", "content": "prefill text"}]),
                "model {model}"
            );
        }
    }

    #[test]
    fn compat_fable_preserves_assistant_prefill() {
        let out = convert_compat(
            "claude-fable-5",
            &prefill_request("claude-fable-5", "prefill text"),
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "hello"}, {"role": "assistant", "content": "prefill text"}])
        );
    }

    #[test]
    fn strips_trailing_thinking_blocks_from_assistant() {
        let (raw_sig, expected_sig) = test_signature();
        let model = "claude-haiku-4-5-20251001";
        let user =
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}"#;
        let prefill = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"prefill text"}]}"#;
        let thought = reasoning_item(&raw_sig, "thought");
        let input =
            |items: &[&str]| format!(r#"{{"model":"{model}","input":[{}]}}"#, items.join(","));

        // user_then_reasoning_drops_trailing_assistant_message
        let out = convert(model, &input(&[user, &thought]));
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "hello"}])
        );

        // user_assistant_text_then_reasoning_strips_trailing_thinking_keeps_assistant_text
        let out = convert(model, &input(&[user, prefill, &thought]));
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "hello"}, {"role": "assistant", "content": "prefill text"}])
        );

        // trailing_redacted_thinking_is_stripped
        let bare =
            json!({"type": "reasoning", "encrypted_content": raw_sig, "summary": []}).to_string();
        let redacted = json!({"type": "reasoning", "encrypted_content": format!("{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}redacted-data"), "summary": []}).to_string();
        let out = convert(model, &input(&[user, prefill, &bare, &redacted]));
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "hello"}, {"role": "assistant", "content": "prefill text"}])
        );

        // only_reasoning_item_yields_fallback_user_message
        let out = convert(model, &input(&[&thought]));
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": ""}]}])
        );

        // compat_mode_preserves_trailing_thinking
        let out = convert_compat(model, &input(&[user, &thought]));
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "thought", "signature": expected_sig}]}
            ])
        );
    }

    #[test]
    fn keeps_agent_message_text() {
        let raw = r#"{"model":"claude-fable-5-1","input":[
            {"type":"agent_message","content":[{"type":"input_text","text":"do X"}]},
            {"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"secret task"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"plain"}]}]}"#;
        let want = json!([{"role": "user", "content": [
            {"type": "text", "text": "do X"},
            {"type": "text", "text": "secret task"},
            {"type": "text", "text": "plain"}
        ]}]);
        assert_eq!(convert("claude-fable-5-1", raw)["messages"], want);
        assert_eq!(convert_compat("claude-fable-5-1", raw)["messages"], want);

        let mixed = r#"{"model":"claude-fable-5-1","input":[
            {"type":"agent_message","content":[{"type":"input_text","text":"step 1"},{"type":"encrypted_content","encrypted_content":"step 2"}]}]}"#;
        assert_eq!(
            convert("claude-fable-5-1", mixed)["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "step 1"}, {"type": "text", "text": "step 2"}]}])
        );
    }

    #[test]
    fn text_format_structured_output() {
        let tail = "\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";
        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","input":"Extract facts.","text":{"format":{"type":"json_schema","name":"extracted_facts",
                "schema":{"type":"object","properties":{"facts":{"type":"array","items":{"type":"string"}}},"required":["facts"]}}}}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": format!(
                "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\nSchema Name: extracted_facts\nJSON Schema:\n{}{tail}",
                r#"{"type":"object","properties":{"facts":{"type":"array","items":{"type":"string"}}},"required":["facts"]}"#
            )}])
        );

        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","input":"Return JSON.","text":{"format":{"type":"json_object"}}}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": format!("You must format your entire response as a valid JSON object.{}", tail.replace("\nDo", " Do"))}])
        );

        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","instructions":"Be concise.","input":"Extract facts.",
                "text":{"format":{"type":"json_schema","name":"winning_schema","description":"Primary facts","schema":{"type":"object","properties":{"item":{"type":"string"}}}}},
                "response_format":{"type":"json_object"}}"#,
        );
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": "Be concise."},
                {"type": "text", "text": format!(
                    "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\nSchema Name: winning_schema\nSchema Description: Primary facts\nJSON Schema:\n{}{tail}",
                    r#"{"type":"object","properties":{"item":{"type":"string"}}}"#
                )}
            ])
        );
    }

    #[test]
    fn function_call_output_alternate_ids_and_queue_fallback() {
        for field in [
            r#","call_id":"call_123""#,
            r#","tool_call_id":"call_123""#,
            r#","callId":"call_123""#,
            r#","id":"call_123""#,
            "",
        ] {
            let raw = format!(
                r#"{{"model":"claude-sonnet-4-6","input":[
                    {{"type":"message","role":"user","content":[{{"type":"input_text","text":"run"}}]}},
                    {{"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{{\"command\":\"ls\"}}"}},
                    {{"type":"function_call_output","output":"tool_result_ok"{field}}}]}}"#
            );
            let out = convert("claude-sonnet-4-6", &raw);
            assert_eq!(
                out["messages"],
                json!([
                    {"role": "user", "content": "run"},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "call_123", "name": "Bash", "input": {"command": "ls"}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_123", "content": "tool_result_ok"}]}
                ]),
                "field {field:?}"
            );
        }
    }

    #[test]
    fn mixed_missing_and_explicit_parallel_outputs() {
        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]},
                {"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},
                {"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},
                {"type":"function_call_output","output":"result_b"},
                {"type":"function_call_output","call_id":"call_a","output":"result_a"}]}"#,
        );
        assert_eq!(
            out["messages"][2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_b", "content": "result_b"},
                {"type": "tool_result", "tool_use_id": "call_a", "content": "result_a"}
            ]})
        );
    }

    #[test]
    fn mixed_missing_and_explicit_parallel_outputs_across_user_message() {
        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","input":[
                {"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},
                {"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},
                {"type":"function_call_output","output":"result_b"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"status?"}]},
                {"type":"function_call_output","call_id":"call_a","output":"result_a"}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_a", "name": "tool_a", "input": {}},
                    {"type": "tool_use", "id": "call_b", "name": "tool_b", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_b", "content": "result_b"},
                    {"type": "tool_result", "tool_use_id": "call_a", "content": "result_a"},
                    {"type": "text", "text": "status?"}
                ]}
            ])
        );
    }

    #[test]
    fn string_input() {
        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","input":"hi","max_output_tokens":16,"stream":false}"#,
        );
        assert_eq!(out["messages"], json!([{"role": "user", "content": "hi"}]));

        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","instructions":"Be concise.","input":"hello world","max_output_tokens":32,"stream":false}"#,
        );
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": "Be concise."}])
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "hello world"}])
        );

        let out = convert(
            "claude-sonnet-4-6",
            r#"{"model":"claude-sonnet-4-6","input":"line 1\n\"line 2\"\n你好，世界 🌍","max_output_tokens":16,"stream":false}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": "line 1\n\"line 2\"\n你好，世界 🌍"}])
        );
    }

    // ─────────────── compat (claude_openai_responses_compat_test.go) ───────────────

    #[test]
    fn with_compat_preserves_empty_reasoning() {
        let payload = r#"{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"reason"}],"encrypted_content":""}]}"#;
        assert_eq!(convert("deepseek-v4", payload)["messages"], json!([]));
        assert_eq!(
            convert_compat("deepseek-v4", payload)["messages"],
            json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "reason", "signature": ""}]}])
        );
        let opaque = r#"{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"reason"}],"encrypted_content":"opaque-deepseek-id"}]}"#;
        assert_eq!(
            convert_compat("deepseek-v4", opaque)["messages"][0]["content"][0],
            json!({"type": "thinking", "thinking": "reason", "signature": "opaque-deepseek-id"})
        );
    }

    // ─────────────── reasoning order (claude_openai-responses_reasoning_order_test.go) ───────────────

    #[test]
    fn keeps_latest_consecutive_reasoning() {
        let (first, _) = signature_for_model("claude-opus-5-first");
        let (second, _) = signature_for_model("claude-opus-5-second");
        let (third, third_sig) = signature_for_model("claude-opus-5-third");
        let raw = request_from_items(&[
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"prefix"}]}"#,
            &reasoning_item(&first, "first reasoning"),
            &reasoning_item(&second, "second reasoning"),
            &reasoning_item(&third, "third reasoning"),
            &function_call_item("call_latest", "latest_tool"),
            &function_call_output_item("call_latest", "done"),
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"][0]["content"],
            json!([
                {"type": "text", "text": "prefix"},
                {"type": "thinking", "thinking": "third reasoning", "signature": third_sig},
                {"type": "tool_use", "id": "call_latest", "name": "latest_tool", "input": {}}
            ])
        );
    }

    #[test]
    fn tool_calls_separate_reasoning_blocks() {
        let (first, first_sig) = signature_for_model("claude-opus-5-first");
        let (second, second_sig) = signature_for_model("claude-opus-5-second");
        let raw = request_from_items(&[
            &reasoning_item(&first, "first reasoning"),
            &function_call_item("call_first", "first_tool"),
            &reasoning_item(&second, "second reasoning"),
            &function_call_item("call_second", "second_tool"),
            &function_call_output_item("call_first", "first result"),
            &function_call_output_item("call_second", "second result"),
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"][0]["content"],
            json!([
                {"type": "thinking", "thinking": "first reasoning", "signature": first_sig},
                {"type": "tool_use", "id": "call_first", "name": "first_tool", "input": {}},
                {"type": "thinking", "thinking": "second reasoning", "signature": second_sig},
                {"type": "tool_use", "id": "call_second", "name": "second_tool", "input": {}}
            ])
        );
    }

    #[test]
    fn non_thinking_blocks_separate_reasoning() {
        let (first, first_sig) = signature_for_model("claude-opus-5-first");
        let (second, second_sig) = signature_for_model("claude-opus-5-second");
        let redacted = json!({"type": "reasoning", "encrypted_content": format!("{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}opaque-redacted-data"), "summary": []}).to_string();
        let raw = request_from_items(&[
            &reasoning_item(&first, "first reasoning"),
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"visible separator"}]}"#,
            &reasoning_item(&second, "second reasoning"),
            &redacted,
            &reasoning_item(&first, "third reasoning"),
            &function_call_item("call_separator", "separator_tool"),
            &function_call_output_item("call_separator", "done"),
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"][0]["content"],
            json!([
                {"type": "thinking", "thinking": "first reasoning", "signature": first_sig},
                {"type": "text", "text": "visible separator"},
                {"type": "thinking", "thinking": "second reasoning", "signature": second_sig},
                {"type": "redacted_thinking", "data": "opaque-redacted-data"},
                {"type": "thinking", "thinking": "third reasoning", "signature": first_sig},
                {"type": "tool_use", "id": "call_separator", "name": "separator_tool", "input": {}}
            ])
        );
    }

    // ─────────────── signature compatibility ───────────────

    #[test]
    fn claude_signature_compatibility() {
        let (raw, normalized) = test_signature();
        // A single-layer E signature is already Claude-native.
        assert_eq!(normalized, raw);
        // The double-layer R form and a provider prefix both normalize back to E.
        let double = STANDARD.encode(raw.as_bytes());
        assert_eq!(compatible_signature_for_claude(&double), Some(raw.clone()));
        assert_eq!(
            compatible_signature_for_claude(&format!("claude#{raw}")),
            Some(raw.clone())
        );
        // Foreign or unknown prefixes and non-Claude envelopes are rejected.
        assert_eq!(
            compatible_signature_for_claude(&format!("gemini#{raw}")),
            None
        );
        assert_eq!(
            compatible_signature_for_claude(&format!("modelgroup#{raw}")),
            None
        );
        assert_eq!(compatible_signature_for_claude(&gpt_signature()), None);
        assert_eq!(compatible_signature_for_claude("claude_sig_123"), None);
        assert_eq!(compatible_signature_for_claude(""), None);
    }

    #[test]
    fn claude_cais_signature_is_compatible() {
        let mut channel = Vec::new();
        append_tag(&mut channel, 1, WIRE_VARINT);
        append_varint(&mut channel, 16);
        append_tag(&mut channel, 5, WIRE_BYTES);
        append_bytes(&mut channel, &[7u8; 64]);
        append_tag(&mut channel, 6, WIRE_BYTES);
        append_bytes(&mut channel, b"claude-opus-5");
        let mut container = Vec::new();
        append_tag(&mut container, 1, WIRE_BYTES);
        append_bytes(&mut container, &channel);
        let mut payload = Vec::new();
        append_tag(&mut payload, 1, WIRE_VARINT);
        append_varint(&mut payload, 2);
        append_tag(&mut payload, 2, WIRE_BYTES);
        append_bytes(&mut payload, &container);
        let raw = STANDARD.encode(&payload);
        assert!(raw.starts_with('C'));
        assert_eq!(compatible_signature_for_claude(&raw), Some(raw.clone()));
    }

    // ─────────────── server tools (claude_openai-responses_server_tool_test.go) ───────────────

    const WEB_SEARCH_CHUNKS: [&str; 7] = [
        r#"{"type":"message_start","message":{"id":"msg_ws","usage":{"input_tokens":1,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"lindorm vector\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"Lindorm Vector","url":"https://example.com/a","encrypted_content":"ENC_A","page_age":"1 day"},{"type":"web_search_result","title":"Docs","url":"https://example.com/b","encrypted_content":"ENC_B"}]}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        MSG_STOP,
    ];

    fn web_search_item() -> Value {
        json!({
            "id": "ws_srvtoolu_1",
            "type": "web_search_call",
            "status": "completed",
            "action": {"type": "search", "query": "lindorm vector"},
            "results": [
                {"type": "web_search_result", "title": "Lindorm Vector", "url": "https://example.com/a", "encrypted_content": "ENC_A", "page_age": "1 day"},
                {"type": "web_search_result", "title": "Docs", "url": "https://example.com/b", "encrypted_content": "ENC_B"}
            ]
        })
    }

    #[test]
    fn claude_web_search_blocks_become_web_search_call_item() {
        let frames = stream(&WEB_SEARCH_CHUNKS);
        assert_eq!(
            completed(&frames)["response"]["output"],
            json!([web_search_item()])
        );
    }

    #[test]
    fn claude_web_search_blocks_become_web_search_call_item_non_stream() {
        let out = non_stream(&Value::Null, &WEB_SEARCH_CHUNKS);
        assert_eq!(out["output"], json!([web_search_item()]));
    }

    #[test]
    fn web_search_call_item_replays_as_claude_server_tool_blocks() {
        let raw = request_from_items(&[
            r#"{"type":"web_search_call","id":"ws_srvtoolu_1","status":"completed",
            "action":{"type":"search","query":"lindorm vector"},
            "results":[{"title":"Lindorm Vector","url":"https://example.com/a","encrypted_content":"ENC_A"}]}"#,
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "lindorm vector"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                    {"title": "Lindorm Vector", "url": "https://example.com/a", "encrypted_content": "ENC_A", "type": "web_search_result"}
                ]}
            ]}])
        );
    }

    #[test]
    fn web_search_call_without_encrypted_content_replays_empty_results() {
        let raw = request_from_items(&[
            r#"{"type":"web_search_call","id":"ws_srvtoolu_1","status":"completed",
            "action":{"type":"search","query":"q"},"results":[{"title":"T","url":"https://example.com/a"}]}"#,
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            assistant_block_types(&out),
            vec!["server_tool_use", "web_search_tool_result"]
        );
        assert_eq!(out["messages"][0]["content"][1]["content"], json!([]));
    }

    #[test]
    fn output_text_annotations_replay_as_claude_citations() {
        let raw = request_from_items(&[
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer.","annotations":[
            {"type":"web_search_result_location","url":"https://example.com/a","title":"A","cited_text":"Answer","encrypted_index":"IDX_A"}]}]}"#,
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"][0]["content"][0],
            json!({"type": "text", "text": "Answer.", "citations": [
                {"type": "web_search_result_location", "url": "https://example.com/a", "title": "A", "cited_text": "Answer", "encrypted_index": "IDX_A"}
            ]})
        );
    }

    #[test]
    fn annotations_without_encrypted_index_are_not_replayed_as_citations() {
        let raw = request_from_items(&[
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer.","annotations":[
            {"type":"url_citation","url":"https://example.com/a","title":"A"}]}]}"#,
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([{"role": "assistant", "content": "Answer."}])
        );
    }

    #[test]
    fn refusal_part_replays_as_claude_text() {
        let raw = request_from_items(&[
            r#"{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I cannot help with that."}]}"#,
        ]);
        let out = convert("claude-test", &raw);
        assert_eq!(
            out["messages"],
            json!([{"role": "assistant", "content": "I cannot help with that."}])
        );
    }

    fn replay(items: &Value) -> Value {
        translate_request(
            "claude-test",
            &json!({"model": "claude-test", "input": items}),
            false,
        )
    }

    #[test]
    fn round_trip_preserves_reachable_claude_blocks() {
        let (sig, _) = test_signature();
        let sig_delta = |index: i64| {
            json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": sig}}).to_string()
        };
        let (sig0, sig4) = (sig_delta(0), sig_delta(4));
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_rt","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ponder"}}"#,
            &sig0,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Researching."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"q\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"T","url":"https://example.com/a"}]}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":"more"}}"#,
            &sig4,
            r#"{"type":"content_block_stop","index":4}"#,
            r#"{"type":"content_block_start","index":5,"content_block":{"type":"tool_use","id":"toolu_1","name":"exec","input":{}}}"#,
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"{"type":"content_block_stop","index":5}"#,
            MSG_STOP,
        ]);
        let out = replay(&completed(&frames)["response"]["output"]);
        assert_eq!(
            assistant_block_types(&out),
            vec![
                "thinking",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use"
            ]
        );
    }

    #[test]
    fn web_search_call_id_normalised_to_claude_server_tool_pattern() {
        for (responses_id, want) in [
            ("ws_srvtoolu_abc123", "srvtoolu_abc123"),
            ("ws_00112233aabb", "srvtoolu_00112233aabb"),
            ("ws_00112233-aabb.cc", "srvtoolu_00112233_aabb_cc"),
        ] {
            let item = format!(
                r#"{{"type":"web_search_call","id":"{responses_id}","status":"completed","action":{{"type":"search","query":"q"}}}}"#
            );
            let out = convert("claude-test", &request_from_items(&[&item]));
            assert_eq!(out["messages"][0]["content"][0]["id"], json!(want));
            assert_eq!(out["messages"][0]["content"][1]["tool_use_id"], json!(want));
        }
    }

    #[test]
    fn web_search_call_without_id_produces_no_blocks() {
        for id in ["", "ws_"] {
            let item = format!(
                r#"{{"type":"web_search_call","id":"{id}","status":"completed","action":{{"type":"search","query":"q"}}}}"#
            );
            let out = convert("claude-test", &request_from_items(&[&item]));
            assert_eq!(
                assistant_block_types(&out),
                Vec::<String>::new(),
                "id {id:?}"
            );
        }
    }

    #[test]
    fn claude_web_search_without_result_block_still_emits_item() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_ws","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"q\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MSG_STOP,
        ]);
        assert_eq!(item_done(&frames, "web_search_call").len(), 1);
        assert_eq!(
            completed(&frames)["response"]["output"],
            json!([{"id": "ws_srvtoolu_1", "type": "web_search_call", "status": "completed", "action": {"type": "search", "query": "q"}}])
        );
    }

    #[test]
    fn unmapped_server_tool_produces_no_item() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_ws","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"code_execution","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            output_types(&completed(&frames)["response"]["output"]),
            vec!["message"]
        );
    }

    #[test]
    fn web_search_result_without_matching_use_is_ignored() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_ws","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_missing","content":[]}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MSG_STOP,
        ]);
        assert_eq!(completed(&frames)["response"].get("output"), None);
    }

    #[test]
    fn web_search_call_query_accepts_native_openai_action_shapes() {
        for (action, want) in [
            (
                r#"{"type":"search","query":"go release","queries":["go release"]}"#,
                "go release",
            ),
            (
                r#"{"type":"search","queries":["go release"]}"#,
                "go release",
            ),
            (
                r#"{"type":"open_page","url":"https://go.dev/dl"}"#,
                "https://go.dev/dl",
            ),
        ] {
            let item = format!(
                r#"{{"type":"web_search_call","id":"ws_srvtoolu_1","status":"completed","action":{action}}}"#
            );
            let out = convert("claude-test", &request_from_items(&[&item]));
            assert_eq!(
                out["messages"][0]["content"][0]["input"],
                json!({"query": want})
            );
        }
    }

    #[test]
    fn claude_web_search_with_empty_results_keeps_empty_list() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_ws","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"q\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[]}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            completed(&frames)["response"]["output"][0]["results"],
            json!([])
        );
    }

    #[test]
    fn claude_web_search_error_result_survives_round_trip() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_ws_err","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_err","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"err_query\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_err","content":[{"type":"web_search_tool_result_error","error_code":"rate_limited"}]}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(
            output,
            json!([{"id": "ws_srvtoolu_err", "type": "web_search_call", "status": "completed",
                "action": {"type": "search", "query": "err_query"},
                "results": [{"type": "web_search_tool_result_error", "error_code": "rate_limited"}]}])
        );
        let replayed = replay(&output);
        assert_eq!(
            replayed["messages"][0]["content"][1]["content"],
            json!([{"type": "web_search_tool_result_error", "error_code": "rate_limited"}])
        );
    }

    #[test]
    fn text_search_text_order_preserved_in_streaming_and_replay() {
        let chunks = [
            r#"{"type":"message_start","message":{"id":"msg_order","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Before search."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_order","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"query\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_order","content":[{"type":"web_search_result","title":"T","url":"https://example.com/order","encrypted_content":"ENC_ORD"}]}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","cited_text":"After search.","url":"https://example.com/order","title":"T","encrypted_index":"IDX_ORD"}}}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"After search."}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            MSG_STOP,
        ];
        let citation = json!({"type": "web_search_result_location", "cited_text": "After search.", "url": "https://example.com/order", "title": "T", "encrypted_index": "IDX_ORD"});
        let want = json!([
            {"id": "msg_msg_order_0", "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "Before search."}], "role": "assistant"},
            {"id": "ws_srvtoolu_order", "type": "web_search_call", "status": "completed", "action": {"type": "search", "query": "query"},
                "results": [{"type": "web_search_result", "title": "T", "url": "https://example.com/order", "encrypted_content": "ENC_ORD"}]},
            {"id": "msg_msg_order_1", "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [citation], "logprobs": [], "text": "After search."}], "role": "assistant"}
        ]);
        let frames = stream(&chunks);
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(output, want);
        assert_eq!(non_stream(&Value::Null, &chunks)["output"], want);
        assert_eq!(
            assistant_block_types(&replay(&output)),
            vec!["text", "server_tool_use", "web_search_tool_result", "text"]
        );
    }

    // ─────────────── citations / interleaved search ───────────────

    #[test]
    fn adjacent_cited_text_blocks_share_message() {
        let chunks = [
            r#"{"type":"message_start","message":{"id":"msg_citations","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[]}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"The store reopened in 2024"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://example.com/store","title":"Store"}}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":", and "}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"Olga was named for Ohlert."}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://example.com/olga","title":"Olga"}}}"#,
            r#"{"type":"content_block_stop","index":4}"#,
            MSG_STOP,
        ];
        let want = json!([
            {"id": "ws_srv_1", "type": "web_search_call", "status": "completed", "action": {"type": "search", "query": ""}, "results": []},
            {"id": "msg_msg_citations_0", "type": "message", "status": "completed", "content": [{
                "type": "output_text",
                "annotations": [
                    {"type": "web_search_result_location", "url": "https://example.com/store", "title": "Store"},
                    {"type": "web_search_result_location", "url": "https://example.com/olga", "title": "Olga"}
                ],
                "logprobs": [],
                "text": "The store reopened in 2024, and Olga was named for Ohlert."
            }], "role": "assistant"}
        ]);
        let frames = stream(&chunks);
        assert_eq!(completed(&frames)["response"]["output"], want);
        for event in [
            "response.content_part.added",
            "response.output_text.done",
            "response.content_part.done",
        ] {
            assert_eq!(count(&frames, event), 1, "{event}");
        }
        assert_eq!(count(&frames, "response.output_item.done"), 2);
        assert_eq!(non_stream(&Value::Null, &chunks)["output"], want);
    }

    #[test]
    fn interleaved_thinking_and_search_survives_round_trip() {
        let (sig, _) = test_signature();
        let thinking = |index: i64, text: &str| -> Vec<String> {
            vec![
                json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": ""}}).to_string(),
                json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": text}}).to_string(),
                json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": sig}}).to_string(),
                json!({"type": "content_block_stop", "index": index}).to_string(),
            ]
        };
        let search = |index: i64, id: &str| -> Vec<String> {
            vec![
                json!({"type": "content_block_start", "index": index, "content_block": {"type": "server_tool_use", "id": id, "name": "web_search", "input": {}}}).to_string(),
                json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"lindorm\"}"}}).to_string(),
                json!({"type": "content_block_stop", "index": index}).to_string(),
                json!({"type": "content_block_start", "index": index + 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": id, "content": [{"type": "web_search_result", "title": "T", "url": format!("https://example.com/{id}")}]}}).to_string(),
                json!({"type": "content_block_stop", "index": index + 1}).to_string(),
            ]
        };
        let mut chunks: Vec<String> = vec![r#"{"type":"message_start","message":{"id":"msg_x","usage":{"input_tokens":1,"output_tokens":0}}}"#.to_string()];
        chunks.extend(thinking(0, "first"));
        chunks.push(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
                .to_string(),
        );
        chunks.push(r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Researching."}}"#.to_string());
        chunks.push(r#"{"type":"content_block_stop","index":1}"#.to_string());
        chunks.extend(search(2, "srvtoolu_a"));
        chunks.extend(thinking(6, "second"));
        chunks.extend(search(7, "srvtoolu_b"));
        chunks.extend(thinking(11, "third"));
        chunks.push(r#"{"type":"content_block_start","index":12,"content_block":{"type":"tool_use","id":"toolu_1","name":"exec","input":{}}}"#.to_string());
        chunks.push(r#"{"type":"content_block_delta","index":12,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#.to_string());
        chunks.push(r#"{"type":"content_block_stop","index":12}"#.to_string());
        chunks.push(MSG_STOP.to_string());
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();

        let frames = stream(&refs);
        let out = replay(&completed(&frames)["response"]["output"]);
        assert_eq!(
            assistant_block_types(&out),
            vec![
                "thinking",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use"
            ]
        );
    }

    // ─────────────── response tests (claude_openai-responses_response_test.go) ───────────────

    #[test]
    fn created_includes_original_request_model() {
        let frames = run_stream(
            &json!({"model": "original-claude-model"}),
            &[r#"{"type":"message_start","message":{"id":"msg_123"}}"#],
        );
        assert_eq!(
            last_event(&frames, "response.created")["response"]["model"],
            json!("original-claude-model")
        );
        assert_eq!(
            last_event(&frames, "response.in_progress")["response"]["model"],
            json!("original-claude-model")
        );
    }

    #[test]
    fn thinking_includes_signature() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"internal "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reasoning"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"claude_sig_123"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MSG_STOP,
        ]);
        let item = json!({"id": "rs_msg_123_0", "type": "reasoning", "status": "completed", "encrypted_content": "claude_sig_123", "summary": [{"type": "summary_text", "text": "internal reasoning"}]});
        assert_eq!(item_done(&frames, "reasoning")[0]["item"], item);
        assert_eq!(completed(&frames)["response"]["output"], json!([item]));
    }

    #[test]
    fn redacted_thinking_becomes_marked_reasoning_item() {
        let want = format!("{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}EroBCkYIBRgCKkA");
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"EroBCkYIBRgCKkA"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            item_done(&frames, "reasoning")[0]["item"]["encrypted_content"],
            json!(want)
        );
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(output[0]["encrypted_content"], json!(want));
        assert_eq!(output_types(&output), vec!["reasoning", "message"]);
    }

    #[test]
    fn non_stream_redacted_thinking_becomes_marked_reasoning_item() {
        let out = non_stream(
            &Value::Null,
            &[
                MSG_START,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"EroBCkYIBRgCKkA"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                MSG_STOP,
            ],
        );
        assert_eq!(
            out["output"],
            json!([{"id": "rs_msg_123_0", "type": "reasoning", "status": "completed",
                "encrypted_content": format!("{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}EroBCkYIBRgCKkA"),
                "summary": [{"type": "summary_text", "text": ""}]}])
        );
    }

    #[test]
    fn suppresses_signature_delta_passthrough() {
        let frames = stream(&[
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"claude_sig_123"}}"#,
        ]);
        assert!(frames.is_empty());
    }

    #[test]
    fn aggregates_text_blocks_until_message_stop() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"**Compare competitors**\n- "}}"#,
            r#"{"type":"content_block_stop","index":4}"#,
            r#"{"type":"content_block_start","index":5,"content_block":{"type":"server_tool_use","id":"srv_123","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Qwen3\"}"}}"#,
            r#"{"type":"content_block_stop","index":5}"#,
            r#"{"type":"content_block_start","index":6,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_123","content":[{"type":"web_search_result","title":"Example","url":"https://example.com"}]}}"#,
            r#"{"type":"content_block_stop","index":6}"#,
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","cited_text":"Qwen 3.7 Max","url":"https://example.com","title":"Example"}}}"#,
            r#"{"type":"content_block_start","index":7,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":7,"delta":{"type":"text_delta","text":"Qwen 3.7 Max leads."}}"#,
            r#"{"type":"content_block_stop","index":7}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":12}}"#,
            MSG_STOP,
        ]);
        for (event, want) in [
            ("response.output_item.added", 3),
            ("response.content_part.added", 2),
            ("response.output_text.done", 2),
            ("response.content_part.done", 2),
            ("response.output_item.done", 3),
            ("response.function_call_arguments.delta", 0),
        ] {
            assert_eq!(count(&frames, event), want, "{event}");
        }
        assert!(frames.iter().all(|(e, _)| e.starts_with("response.")));
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(
            output_types(&output),
            vec!["message", "web_search_call", "message"]
        );
        assert_eq!(
            output[0]["content"][0]["text"],
            json!("**Compare competitors**\n- ")
        );
        assert_eq!(
            output[2]["content"][0]["text"],
            json!("Qwen 3.7 Max leads.")
        );
        assert_eq!(
            output[2]["content"][0]["annotations"],
            json!([{"type": "web_search_result_location", "cited_text": "Qwen 3.7 Max", "url": "https://example.com", "title": "Example"}])
        );
    }

    #[test]
    fn finalizes_message_before_function_call() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking the workspace."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            lifecycle(&frames),
            vec![
                "response.output_item.added:0:message",
                "response.output_item.done:0:message",
                "response.output_item.added:1:function_call",
                "response.output_item.done:1:function_call"
            ]
        );
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(output_types(&output), vec!["message", "function_call"]);
        assert_eq!(
            output[0]["content"][0]["text"],
            json!("Checking the workspace.")
        );
        assert_eq!(
            output[1],
            json!({"id": "fc_call_123", "type": "function_call", "status": "completed", "arguments": "{\"cmd\":\"pwd\"}", "call_id": "call_123", "name": "exec_command"})
        );
    }

    #[test]
    fn uses_contiguous_indices_for_reasoning_text_and_tool() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_123","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Qwen3\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_123","content":[]}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"Inspect first."}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"Checking the workspace."}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"{"type":"content_block_stop","index":4}"#,
            MSG_STOP,
        ]);
        for (event, data) in &frames {
            let want =
                if event == "response.output_item.added" || event == "response.output_item.done" {
                    match gstr(data, "item.type").as_str() {
                        "web_search_call" => 0,
                        "reasoning" => 1,
                        "message" => 2,
                        "function_call" => 3,
                        _ => continue,
                    }
                } else if event.starts_with("response.reasoning_") {
                    1
                } else if event.starts_with("response.output_text.")
                    || event.starts_with("response.content_part.")
                {
                    2
                } else if event.starts_with("response.function_call_arguments.") {
                    3
                } else {
                    continue;
                };
            assert_eq!(data["output_index"], json!(want), "{event}");
        }
        assert_eq!(
            output_types(&completed(&frames)["response"]["output"]),
            vec!["web_search_call", "reasoning", "message", "function_call"]
        );
    }

    #[test]
    fn server_tools_surface_without_output_index_gaps() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Searching. "}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srv_123","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Qwen3\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_123","content":[]}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"Found it."}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"{"type":"content_block_stop","index":4}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            lifecycle(&frames),
            vec![
                "response.output_item.added:0:message",
                "response.output_item.done:0:message",
                "response.output_item.added:1:web_search_call",
                "response.output_item.done:1:web_search_call",
                "response.output_item.added:2:message",
                "response.output_item.done:2:message",
                "response.output_item.added:3:function_call",
                "response.output_item.done:3:function_call"
            ]
        );
        assert_eq!(
            output_types(&completed(&frames)["response"]["output"]),
            vec!["message", "web_search_call", "message", "function_call"]
        );
    }

    #[test]
    fn starts_new_message_after_function_call() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Before tool."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"After tool."}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            lifecycle(&frames),
            vec![
                "response.output_item.added:0:message",
                "response.output_item.done:0:message",
                "response.output_item.added:1:function_call",
                "response.output_item.done:1:function_call",
                "response.output_item.added:2:message",
                "response.output_item.done:2:message"
            ]
        );
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(
            output_types(&output),
            vec!["message", "function_call", "message"]
        );
        assert_eq!(output[0]["id"], json!("msg_msg_123_0"));
        assert_eq!(output[2]["id"], json!("msg_msg_123_1"));
        assert_eq!(output[0]["content"][0]["text"], json!("Before tool."));
        assert_eq!(output[2]["content"][0]["text"], json!("After tool."));
    }

    #[test]
    fn finalizes_message_before_reasoning() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Visible first."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Reason later."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            lifecycle(&frames),
            vec![
                "response.output_item.added:0:message",
                "response.output_item.done:0:message",
                "response.output_item.added:1:reasoning",
                "response.output_item.done:1:reasoning"
            ]
        );
        assert_eq!(
            output_types(&completed(&frames)["response"]["output"]),
            vec!["message", "reasoning"]
        );
    }

    #[test]
    fn preserves_multiple_reasoning_items() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"First reason."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Second reason."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Visible response."}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            MSG_STOP,
        ]);
        let done: Vec<Value> = item_done(&frames, "reasoning")
            .iter()
            .map(|d| d["output_index"].clone())
            .collect();
        assert_eq!(done, vec![json!(0), json!(1)]);
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(
            output_types(&output),
            vec!["reasoning", "reasoning", "message"]
        );
        assert_eq!(
            output[0]["summary"],
            json!([{"type": "summary_text", "text": "First reason."}])
        );
        assert_eq!(
            output[1]["summary"],
            json!([{"type": "summary_text", "text": "Second reason."}])
        );
    }

    #[test]
    fn normalizes_empty_function_arguments() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            item_done(&frames, "function_call")[0]["item"]["arguments"],
            json!("{}")
        );
        assert_eq!(
            completed(&frames)["response"]["output"][0]["arguments"],
            json!("{}")
        );
    }

    #[test]
    fn includes_empty_reasoning_in_completed_output() {
        let frames = stream(&[
            MSG_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Visible response."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            item_done(&frames, "reasoning")[0]["item"]["summary"],
            json!([{"type": "summary_text", "text": ""}])
        );
        let output = completed(&frames)["response"]["output"].clone();
        assert_eq!(output_types(&output), vec!["reasoning", "message"]);
        assert_eq!(
            output[0]["summary"],
            json!([{"type": "summary_text", "text": ""}])
        );
    }

    #[test]
    fn reports_cache_tokens() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_123","usage":{"input_tokens":13,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":4,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}"#,
            MSG_STOP,
        ]);
        assert_eq!(
            completed(&frames)["response"]["usage"],
            json!({"input_tokens": 22044, "input_tokens_details": {"cached_tokens": 22000}, "output_tokens": 4, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 22048})
        );
    }

    #[test]
    fn non_stream_thinking_includes_signature() {
        let out = non_stream(
            &Value::Null,
            &[
                r#"{"type":"message_start","message":{"id":"msg_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"nonstream reasoning"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"claude_sig_nonstream"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                MSG_STOP,
            ],
        );
        assert_eq!(
            out["output"],
            json!([{"id": "rs_msg_nonstream_0", "type": "reasoning", "status": "completed", "encrypted_content": "claude_sig_nonstream", "summary": [{"type": "summary_text", "text": "nonstream reasoning"}]}])
        );
    }

    #[test]
    fn non_stream_preserves_content_block_order() {
        let out = non_stream(
            &Value::Null,
            &[
                r#"{"type":"message_start","message":{"id":"msg_nonstream_order","usage":{"input_tokens":1,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_order","name":"exec_command","input":{}}}"#,
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
                r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"done"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"content_block_stop","index":2}"#,
                r#"{"type":"content_block_stop","index":3}"#,
                r#"{"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":"more"}}"#,
                r#"{"type":"content_block_stop","index":4}"#,
                MSG_STOP,
            ],
        );
        let message = |id: &str, text: &str| json!({"id": id, "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": text}], "role": "assistant"});
        let reasoning = |id: &str, text: &str| json!({"id": id, "type": "reasoning", "status": "completed", "encrypted_content": "", "summary": [{"type": "summary_text", "text": text}]});
        assert_eq!(
            out["output"],
            json!([
                message("msg_msg_nonstream_order_0", ""),
                reasoning("rs_msg_nonstream_order_1", "plan"),
                {"id": "fc_call_order", "type": "function_call", "status": "completed", "arguments": "{\"cmd\":\"pwd\"}", "call_id": "call_order", "name": "exec_command"},
                message("msg_msg_nonstream_order_1", "done"),
                reasoning("rs_msg_nonstream_order_4", "more")
            ])
        );
        assert_eq!(
            out["usage"]["output_tokens_details"],
            json!({"reasoning_tokens": 2})
        );
    }

    #[test]
    fn non_stream_reports_cache_tokens() {
        let out = non_stream(
            &Value::Null,
            &[
                r#"{"type":"message_start","message":{"id":"msg_nonstream","usage":{"input_tokens":13,"output_tokens":1,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}}"#,
                r#"{"type":"message_delta","usage":{"output_tokens":4}}"#,
                MSG_STOP,
            ],
        );
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 22044, "input_tokens_details": {"cached_tokens": 22000}, "output_tokens": 4, "output_tokens_details": {}, "total_tokens": 22048})
        );
    }

    const NAMESPACE_CUSTOM_REQUEST: &str = r#"{"model":"gpt-test","input":[{"type":"additional_tools","role":"developer","tools":[
        {"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]}]}"#;

    #[test]
    fn restores_additional_namespace_custom_tool_call() {
        let frames = run_stream(
            &parse(NAMESPACE_CUSTOM_REQUEST),
            &[
                r#"{"type":"message_start","message":{"id":"msg_custom","usage":{"input_tokens":1,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom","name":"functions__exec","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                MSG_STOP,
            ],
        );
        assert_eq!(count(&frames, "response.function_call_arguments.delta"), 0);
        assert_eq!(count(&frames, "response.function_call_arguments.done"), 0);
        assert_eq!(
            last_event(&frames, "response.output_item.added")["item"],
            json!({"id": "ctc_call_custom", "type": "custom_tool_call", "status": "in_progress", "input": "", "call_id": "call_custom", "name": "exec", "namespace": "functions"})
        );
        assert_eq!(
            last_event(&frames, "response.custom_tool_call_input.done"),
            json!({"type": "response.custom_tool_call_input.done", "sequence_number": 4, "item_id": "ctc_call_custom", "output_index": 0, "input": "pwd"})
        );
        let item = json!({"id": "ctc_call_custom", "type": "custom_tool_call", "status": "completed", "input": "pwd", "call_id": "call_custom", "name": "exec", "namespace": "functions"});
        assert_eq!(item_done(&frames, "custom_tool_call")[0]["item"], item);
        assert_eq!(completed(&frames)["response"]["output"], json!([item]));
    }

    #[test]
    fn direct_custom_wins_namespace_collision() {
        let original = parse(
            r#"{"model":"gpt-test","tools":[{"type":"namespace","name":"n","tools":[{"type":"function","name":"x"}]},{"type":"custom","name":"n__x"}]}"#,
        );
        let chunks = [
            r#"{"type":"message_start","message":{"id":"msg_collision","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_collision","name":"n__x","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MSG_STOP,
        ];
        let want = json!([{"id": "ctc_call_collision", "type": "custom_tool_call", "status": "completed", "input": "pwd", "call_id": "call_collision", "name": "n__x"}]);
        assert_eq!(
            completed(&run_stream(&original, &chunks))["response"]["output"],
            want
        );
        assert_eq!(non_stream(&original, &chunks)["output"], want);
    }

    #[test]
    fn non_stream_restores_additional_namespace_custom_tool_call() {
        let out = non_stream(
            &parse(NAMESPACE_CUSTOM_REQUEST),
            &[
                r#"{"type":"message_start","message":{"id":"msg_custom_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom_nonstream","name":"functions__exec","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                MSG_STOP,
            ],
        );
        assert_eq!(
            out["output"],
            json!([{"id": "ctc_call_custom_nonstream", "type": "custom_tool_call", "status": "completed", "input": "pwd", "call_id": "call_custom_nonstream", "name": "exec", "namespace": "functions"}])
        );
    }

    #[test]
    fn custom_tool_empty_input_matches_non_stream() {
        let original = parse(r#"{"model":"gpt-test","tools":[{"type":"custom","name":"exec"}]}"#);
        let chunks = [
            r#"{"type":"message_start","message":{"id":"msg_custom_empty","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom_empty","name":"exec","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MSG_STOP,
        ];
        let want = json!([{"id": "ctc_call_custom_empty", "type": "custom_tool_call", "status": "completed", "input": "", "call_id": "call_custom_empty", "name": "exec"}]);
        assert_eq!(
            completed(&run_stream(&original, &chunks))["response"]["output"],
            want
        );
        assert_eq!(non_stream(&original, &chunks)["output"], want);
    }

    const NAMESPACE_FUNCTION_REQUEST: &str = r#"{"model":"gpt-test","tools":[{"type":"namespace","name":"mcp__node_repl",
        "tools":[{"type":"function","name":"js","parameters":{"type":"object","properties":{}}}]}]}"#;

    // The Go stream case feeds a partial_json that is not valid JSON; the
    // chunk here is the valid form of the same delta.
    #[test]
    fn restores_namespace_function_call() {
        let chunks = [
            MSG_START,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_abc","name":"mcp__node_repl__js","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"code\":\"nodeRepl.write('hello')\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MSG_STOP,
        ];
        let frames = run_stream(&parse(NAMESPACE_FUNCTION_REQUEST), &chunks);
        assert_eq!(
            last_event(&frames, "response.output_item.added")["item"],
            json!({"id": "fc_call_abc", "type": "function_call", "status": "in_progress", "arguments": "", "call_id": "call_abc", "name": "js", "namespace": "mcp__node_repl"})
        );
        let item = json!({"id": "fc_call_abc", "type": "function_call", "status": "completed", "arguments": "{\"code\":\"nodeRepl.write('hello')\"}", "call_id": "call_abc", "name": "js", "namespace": "mcp__node_repl"});
        assert_eq!(item_done(&frames, "function_call")[0]["item"], item);
        assert_eq!(completed(&frames)["response"]["output"], json!([item]));
        assert_eq!(
            non_stream(&parse(NAMESPACE_FUNCTION_REQUEST), &chunks)["output"],
            json!([item])
        );
    }

    const MAX_TOKENS_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":64000}}"#;

    #[test]
    fn max_tokens_emits_incomplete() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_max_tokens","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"unfinished reasoning"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_max_tokens"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ]);
        assert_eq!(count(&frames, "response.completed"), 0);
        let incomplete = last_event(&frames, "response.incomplete");
        assert_eq!(incomplete["response"]["status"], json!("incomplete"));
        assert_eq!(
            incomplete["response"]["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
        assert_eq!(
            incomplete["response"]["output"],
            json!([{"id": "rs_msg_max_tokens_0", "type": "reasoning", "status": "incomplete", "encrypted_content": "sig_max_tokens", "summary": [{"type": "summary_text", "text": "unfinished reasoning"}]}])
        );
        assert_eq!(
            incomplete["response"]["usage"]["output_tokens"],
            json!(64000)
        );
    }

    #[test]
    fn non_stream_max_tokens_preserves_partial_text() {
        let out = non_stream(
            &Value::Null,
            &[
                r#"{"type":"message_start","message":{"id":"msg_partial","usage":{"input_tokens":10,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial answer"}}"#,
                MAX_TOKENS_DELTA,
                MSG_STOP,
            ],
        );
        assert_eq!(out["status"], json!("incomplete"));
        assert_eq!(
            out["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
        assert_eq!(
            out["output"],
            json!([{"id": "msg_msg_partial_0", "type": "message", "status": "incomplete", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "partial answer"}], "role": "assistant"}])
        );
    }

    #[test]
    fn max_tokens_with_tool_call_and_text_emits_incomplete() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_tool_incomplete","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"calling tool"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_inc_1","name":"get_weather","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"San"}}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ]);
        let function_item = json!({"id": "fc_call_inc_1", "type": "function_call", "status": "incomplete", "arguments": "{\"city\":\"San", "call_id": "call_inc_1", "name": "get_weather"});
        assert_eq!(
            item_done(&frames, "function_call")[0]["item"],
            function_item
        );
        let incomplete = last_event(&frames, "response.incomplete");
        assert_eq!(incomplete["response"]["status"], json!("incomplete"));
        assert_eq!(
            incomplete["response"]["output"],
            json!([
                {"id": "msg_msg_tool_incomplete_0", "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "calling tool"}], "role": "assistant"},
                function_item
            ])
        );
    }

    #[test]
    fn max_tokens_with_web_search_emits_incomplete() {
        let chunks = [
            r#"{"type":"message_start","message":{"id":"msg_ws_incomplete","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"golang\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ];
        let item = json!({"id": "ws_srv_1", "type": "web_search_call", "status": "incomplete", "action": {"type": "search", "query": "golang"}});
        let frames = stream(&chunks);
        assert_eq!(item_done(&frames, "web_search_call")[0]["item"], item);
        assert_eq!(
            last_event(&frames, "response.incomplete")["response"]["output"],
            json!([item])
        );
        let out = non_stream(&Value::Null, &chunks);
        assert_eq!(out["status"], json!("incomplete"));
        assert_eq!(out["output"], json!([item]));
    }

    #[test]
    fn max_tokens_reasoning_without_block_stop() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_reasoning_nostop","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"partial thought before cut"}}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ]);
        let item = json!({"id": "rs_msg_reasoning_nostop_0", "type": "reasoning", "status": "incomplete", "encrypted_content": "", "summary": [{"type": "summary_text", "text": "partial thought before cut"}]});
        assert_eq!(item_done(&frames, "reasoning")[0]["item"], item);
        assert_eq!(
            last_event(&frames, "response.incomplete")["response"]["output"],
            json!([item])
        );
    }

    #[test]
    fn max_tokens_with_tool_block_stop_emits_incomplete() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_tool_blockstop","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_stop_1","name":"do_work","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"step\":1}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ]);
        let item = json!({"id": "fc_call_stop_1", "type": "function_call", "status": "incomplete", "arguments": "{\"step\":1}", "call_id": "call_stop_1", "name": "do_work"});
        assert_eq!(item_done(&frames, "function_call")[0]["item"], item);
        assert_eq!(
            last_event(&frames, "response.incomplete")["response"]["output"],
            json!([item])
        );
    }

    #[test]
    fn max_tokens_with_web_search_results_emits_incomplete() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_ws_res_incomplete","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_2","name":"web_search"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"golang\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_2","content":[{"type":"web_search_result","title":"Go","url":"https://golang.org"}]}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ]);
        let item = json!({"id": "ws_srv_2", "type": "web_search_call", "status": "incomplete", "action": {"type": "search", "query": "golang"},
            "results": [{"type": "web_search_result", "title": "Go", "url": "https://golang.org"}]});
        assert_eq!(item_done(&frames, "web_search_call")[0]["item"], item);
        assert_eq!(
            last_event(&frames, "response.incomplete")["response"]["output"],
            json!([item])
        );
    }

    #[test]
    fn reasoning_events_not_duplicated() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_rs_once","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"full thought"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}"#,
            MSG_STOP,
        ]);
        assert_eq!(count(&frames, "response.reasoning_summary_text.done"), 1);
        assert_eq!(count(&frames, "response.reasoning_summary_part.done"), 1);
        assert_eq!(count(&frames, "response.output_item.done"), 1);
    }

    #[test]
    fn custom_tool_truncated_input_unwrapped() {
        let frames = run_stream(
            &parse(r#"{"tools":[{"type":"custom","name":"bash"}]}"#),
            &[
                r#"{"type":"message_start","message":{"id":"msg_custom_trunc","usage":{"input_tokens":10,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_c1","name":"bash","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"echo \\u4F60"}}"#,
                MAX_TOKENS_DELTA,
                MSG_STOP,
            ],
        );
        let item = json!({"id": "ctc_call_c1", "type": "custom_tool_call", "status": "incomplete", "input": "echo 你", "call_id": "call_c1", "name": "bash"});
        assert_eq!(item_done(&frames, "custom_tool_call")[0]["item"], item);
        assert_eq!(
            last_event(&frames, "response.incomplete")["response"]["output"],
            json!([item])
        );
    }

    #[test]
    fn empty_function_args_consistent_on_truncation() {
        let frames = stream(&[
            r#"{"type":"message_start","message":{"id":"msg_empty_args_trunc","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_empty","name":"get_info","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            MSG_STOP,
        ]);
        assert_eq!(
            last_event(&frames, "response.function_call_arguments.done")["arguments"],
            json!("")
        );
        let item = json!({"id": "fc_call_empty", "type": "function_call", "status": "incomplete", "arguments": "", "call_id": "call_empty", "name": "get_info"});
        assert_eq!(item_done(&frames, "function_call")[0]["item"], item);
        assert_eq!(
            last_event(&frames, "response.incomplete")["response"]["output"],
            json!([item])
        );
    }

    // port of TestConvertClaudeResponseToOpenAIResponsesNonStreamKeepsZeroUsageDefaults (noop_optimization_test.go)
    #[test]
    fn non_stream_keeps_zero_usage_defaults() {
        let out = non_stream(
            &Value::Null,
            &[
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
            ],
        );
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 0, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 0, "output_tokens_details": {}, "total_tokens": 0})
        );
    }

    #[test]
    fn unwrap_custom_tool_input_cases() {
        assert_eq!(unwrap_custom_tool_input(r#"{"input":"pwd"}"#), "pwd");
        assert_eq!(
            unwrap_custom_tool_input(r#"{"input":{"a":1}}"#),
            r#"{"a":1}"#
        );
        assert_eq!(
            unwrap_custom_tool_input(r#"{"input":"echo 😀 \n"#),
            "echo 😀 \n"
        );
        assert_eq!(unwrap_custom_tool_input(r#"{"input":"a\"b"#), "a\"b");
        assert_eq!(unwrap_custom_tool_input(r#"{"other":1}"#), r#"{"other":1}"#);
        assert_eq!(unwrap_custom_tool_input(""), "");
    }

    // ─────────────── end-to-end stream: thinking + text + tool_use ───────────────

    #[test]
    fn end_to_end_stream_thinking_text_and_tool_use() {
        let original = json!({
            "model": "gpt-5",
            "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}]
        });
        let mut translator = StreamTranslator::new(&original);
        let chunks: Vec<(&str, &str)> = vec![
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":1}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think."}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Running it."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"exec_command","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":20}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ];
        let mut raw_frames: Vec<String> = Vec::new();
        for (event, data) in &chunks {
            raw_frames.extend(translator.push(Some(event), &parse(data)));
        }
        assert!(
            translator.finish().is_empty(),
            "message_stop already closed the stream"
        );

        let frames: Vec<(String, Value)> = raw_frames.iter().map(|f| parse_frame(f)).collect();
        let created_at = frames[0].1["response"]["created_at"].clone();
        assert!(created_at.as_i64().unwrap() > 0);

        let rs = "rs_msg_1_0";
        let msg = "msg_msg_1_0";
        let reasoning_item = json!({"id": rs, "type": "reasoning", "status": "completed", "encrypted_content": "sig_abc", "summary": [{"type": "summary_text", "text": "Let me think."}]});
        let message_item = json!({"id": msg, "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "Running it."}], "role": "assistant"});
        let function_item = json!({"id": "fc_toolu_1", "type": "function_call", "status": "completed", "arguments": "{\"cmd\":\"ls\"}", "call_id": "toolu_1", "name": "exec_command"});
        let want: Vec<Value> = vec![
            json!({"type": "response.created", "sequence_number": 1, "response": {"id": "msg_1", "object": "response", "created_at": created_at, "status": "in_progress", "background": false, "error": null, "output": [], "model": "gpt-5"}}),
            json!({"type": "response.in_progress", "sequence_number": 2, "response": {"id": "msg_1", "object": "response", "created_at": created_at, "status": "in_progress", "output": [], "model": "gpt-5"}}),
            json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 0, "item": {"id": rs, "type": "reasoning", "status": "in_progress", "encrypted_content": "", "summary": []}}),
            json!({"type": "response.reasoning_summary_part.added", "sequence_number": 4, "item_id": rs, "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "sequence_number": 5, "item_id": rs, "output_index": 0, "summary_index": 0, "delta": "Let me think."}),
            json!({"type": "response.reasoning_summary_text.done", "sequence_number": 6, "item_id": rs, "output_index": 0, "summary_index": 0, "text": "Let me think."}),
            json!({"type": "response.reasoning_summary_part.done", "sequence_number": 7, "item_id": rs, "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": "Let me think."}}),
            json!({"type": "response.output_item.done", "sequence_number": 8, "output_index": 0, "item": reasoning_item}),
            json!({"type": "response.output_item.added", "sequence_number": 9, "output_index": 1, "item": {"id": msg, "type": "message", "status": "in_progress", "content": [], "role": "assistant"}}),
            json!({"type": "response.content_part.added", "sequence_number": 10, "item_id": msg, "output_index": 1, "content_index": 0, "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""}}),
            json!({"type": "response.output_text.delta", "sequence_number": 11, "item_id": msg, "output_index": 1, "content_index": 0, "delta": "Running it.", "logprobs": []}),
            json!({"type": "response.output_text.done", "sequence_number": 12, "item_id": msg, "output_index": 1, "content_index": 0, "text": "Running it.", "logprobs": []}),
            json!({"type": "response.content_part.done", "sequence_number": 13, "item_id": msg, "output_index": 1, "content_index": 0, "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": "Running it."}}),
            json!({"type": "response.output_item.done", "sequence_number": 14, "output_index": 1, "item": message_item}),
            json!({"type": "response.output_item.added", "sequence_number": 15, "output_index": 2, "item": {"id": "fc_toolu_1", "type": "function_call", "status": "in_progress", "arguments": "", "call_id": "toolu_1", "name": "exec_command"}}),
            json!({"type": "response.function_call_arguments.delta", "sequence_number": 16, "item_id": "fc_toolu_1", "output_index": 2, "delta": "{\"cmd\":"}),
            json!({"type": "response.function_call_arguments.delta", "sequence_number": 17, "item_id": "fc_toolu_1", "output_index": 2, "delta": "\"ls\"}"}),
            json!({"type": "response.function_call_arguments.done", "sequence_number": 18, "item_id": "fc_toolu_1", "output_index": 2, "arguments": "{\"cmd\":\"ls\"}"}),
            json!({"type": "response.output_item.done", "sequence_number": 19, "output_index": 2, "item": function_item}),
            json!({"type": "response.completed", "sequence_number": 20, "response": {
                "id": "msg_1", "object": "response", "created_at": created_at, "status": "completed", "background": false, "error": null,
                "model": "gpt-5",
                "tools": original["tools"],
                "output": [reasoning_item, message_item, function_item],
                "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 3}, "total_tokens": 30}
            }}),
        ];
        let got: Vec<Value> = frames.iter().map(|(_, d)| d.clone()).collect();
        assert_eq!(got, want);
        // The wire framing itself.
        assert!(raw_frames[0].starts_with(
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":1,"
        ));

        // The same response as one complete Messages object (non-stream).
        let upstream = json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [
                {"type": "thinking", "thinking": "Let me think.", "signature": "sig_abc"},
                {"type": "text", "text": "Running it."},
                {"type": "tool_use", "id": "toolu_1", "name": "exec_command", "input": {"cmd": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });
        let out = translate_non_stream(&upstream, &original);
        let created = out["created_at"].clone();
        assert_eq!(
            out,
            json!({
                "id": "msg_1", "object": "response", "created_at": created, "status": "completed", "background": false, "error": null,
                "incomplete_details": null,
                "output": [reasoning_item, message_item, function_item],
                "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 3}, "total_tokens": 30},
                "model": "gpt-5",
                "tools": original["tools"]
            })
        );
    }

    #[test]
    fn finish_closes_a_cut_stream() {
        let mut translator = StreamTranslator::new(&Value::Null);
        assert!(
            translator.finish().is_empty(),
            "nothing started, nothing to close"
        );
        translator.push(None, &parse(MSG_START));
        translator.push(None, &parse(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#));
        translator.push(None, &parse(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#));
        let frames: Vec<(String, Value)> =
            translator.finish().iter().map(|f| parse_frame(f)).collect();
        let events: Vec<&str> = frames.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(
            events,
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
        assert_eq!(
            completed(&frames)["response"]["output"][0]["content"][0]["text"],
            json!("partial")
        );
        assert!(translator.finish().is_empty(), "finish is idempotent");
    }

    #[test]
    fn an_in_stream_error_ends_as_response_failed_not_completed() {
        let mut translator = StreamTranslator::new(&Value::Null);
        translator.push(None, &parse(MSG_START));
        translator.push(None, &parse(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#));
        translator.push(None, &parse(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#));
        let frames: Vec<(String, Value)> = translator
            .push(
                None,
                &parse(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            )
            .iter()
            .map(|f| parse_frame(f))
            .collect();
        let last = frames.last().unwrap();
        assert_eq!(last.0, "response.failed");
        assert_eq!(last.1["response"]["status"], json!("failed"));
        assert_eq!(
            last.1["response"]["error"]["code"],
            json!("overloaded_error")
        );
        assert_eq!(last.1["response"]["error"]["message"], json!("Overloaded"));
        assert!(
            translator.finish().is_empty(),
            "the clean close afterwards must not add response.completed"
        );
    }
}
