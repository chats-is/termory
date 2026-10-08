//! Anthropic Messages (client) ⇄ OpenAI Chat Completions (upstream) translator.
//!
//! Faithful port of CLIProxyAPI `internal/translator/openai/claude/` at commit
//! `ed980be` (`openai_claude_request.go`, `openai_claude_response.go`), plus the
//! helpers they call in other packages (`translator/common`, `util`, `thinking`,
//! `signature`). Each ported function carries a `// port of <GoFunc> (<file>)`
//! comment so the two can be diffed when upstream moves.
//!
//! The Go code works on raw bytes through gjson/sjson; this port works on
//! `serde_json::Value`. The gjson coercion rules the Go code relies on
//! (`.String()` / `.Int()` / `.Float()` on any JSON type, `.Exists()` being true
//! for an explicit `null`) are reproduced by the `g*` helpers below so edge
//! cases behave the same.
//!
//! Deliberately NOT ported: the `model(level)` thinking-suffix parsing, the
//! count_tokens response (`ClaudeTokenCount`), metrics/logging and config hooks.

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
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

/// sjson writes a float64 with `strconv.FormatFloat(f, 'f', -1, 64)`, so an
/// integral value goes out as `1`, not `1.0`.
fn float_value(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

fn is_str(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(_)))
}

fn sse_frame(event: &str, payload: &Value) -> String {
    // port of AppendSSEEventBytes (translator/common/bytes.go), trailingNewlines = 2
    format!("event: {event}\ndata: {payload}\n\n")
}

// ---------------------------------------------------------------------------
// util / thinking / signature helpers
// ---------------------------------------------------------------------------

const CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

// port of IsClaudeCodeAttributionSystemText (util/claude_attribution.go)
fn is_claude_code_attribution_system_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX)
}

// port of HasUnsupportedUnicodePropertyEscape (util/claude_schema.go)
fn has_unsupported_unicode_property_escape(pattern: &str) -> bool {
    let b = pattern.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if i + 1 >= b.len() {
            break;
        }
        let next = b[i + 1];
        if (next == b'p' || next == b'P') && i + 2 < b.len() && b[i + 2] == b'{' {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2; // skip the escaped character (including escaped backslash)
    }
    false
}

// port of SchemaMapKeywords (util/claude_schema.go)
const SCHEMA_MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

// port of SchemaValueKeywords (util/claude_schema.go)
const SCHEMA_VALUE_KEYWORDS: [&str; 16] = [
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];

// port of FixJSON (util/translator.go)
fn fix_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    let runes: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < runes.len() {
        let r = runes[i];
        if in_double {
            out.push(r);
            if escaped {
                escaped = false;
            } else if r == '\\' {
                escaped = true;
            } else if r == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if in_single {
            if escaped {
                escaped = false;
                match r {
                    'n' | 'r' | 't' | 'b' | 'f' | '/' | '"' => {
                        out.push('\\');
                        out.push(r);
                    }
                    '\\' => out.push_str("\\\\"),
                    '\'' => out.push('\''),
                    'u' => {
                        out.push_str("\\u");
                        let mut k = 0;
                        while k < 4 && i + 1 < runes.len() {
                            let peek = runes[i + 1];
                            if peek.is_ascii_hexdigit() {
                                out.push(peek);
                                i += 1;
                            } else {
                                break;
                            }
                            k += 1;
                        }
                    }
                    _ => {
                        out.push('\\');
                        out.push(r);
                    }
                }
                i += 1;
                continue;
            }
            if r == '\\' {
                escaped = true;
                i += 1;
                continue;
            }
            if r == '\'' {
                out.push('"');
                in_single = false;
                i += 1;
                continue;
            }
            if r == '"' {
                out.push_str("\\\"");
            } else {
                out.push(r);
            }
            i += 1;
            continue;
        }
        if r == '"' {
            in_double = true;
            out.push(r);
            i += 1;
            continue;
        }
        if r == '\'' {
            in_single = true;
            out.push('"');
            i += 1;
            continue;
        }
        out.push(r);
        i += 1;
    }
    if in_single {
        out.push('"');
    }
    out
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

static TOOL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generated ids: wall-clock nanos plus a process-wide counter, the same shape
/// as Go's `toolu_%d_%d`.
fn generate_tool_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = TOOL_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
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

// port of ConvertBudgetToLevel (thinking/convert.go)
fn convert_budget_to_level(budget: i64) -> Option<&'static str> {
    match budget {
        b if b < -1 => None,
        -1 => Some("auto"),
        0 => Some("none"),
        b if b <= 512 => Some("minimal"),
        b if b <= 1024 => Some("low"),
        b if b <= 8192 => Some("medium"),
        b if b <= 24576 => Some("high"),
        _ => Some("xhigh"),
    }
}

// port of GetThinkingText (thinking/text.go)
fn get_thinking_text(part: &Value) -> String {
    if let Some(Value::String(t)) = part.get("text") {
        return t.clone();
    }
    match part.get("thinking") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Object(o)) => {
            if let Some(Value::String(t)) = o.get("text") {
                return t.clone();
            }
            if let Some(Value::String(t)) = o.get("thinking") {
                return t.clone();
            }
            String::new()
        }
        _ => String::new(),
    }
}

const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

// port of IsValidGPTReasoningSignature / InspectGPTReasoningSignature /
// decodeGPTReasoningSignature (signature/gpt_validation.go)
fn is_valid_gpt_reasoning_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return false;
    }
    if !sig.starts_with("gAAAA") {
        return false;
    }
    if !sig
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'=')
    {
        return false;
    }
    // Go's base64 decoders tolerate non-zero trailing bits; mirror that.
    let raw_url = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    let padded_url = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let decoded = match raw_url.decode(sig).or_else(|_| padded_url.decode(sig)) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if decoded.len() < 73 || decoded[0] != 0x80 {
        return false;
    }
    let ciphertext_len = decoded.len() as i64 - 1 - 8 - 16 - 32;
    ciphertext_len > 0 && ciphertext_len % 16 == 0
}

// port of CompatibleSignatureForProvider(SignatureProviderGPT, ·)
// (signature/provider_compatibility.go), reduced to the GPT target: the
// signature is replayable only when DetectSignatureProviderForBlock classifies
// it as GPT, either bare or behind an `openai#` / `gpt#` / `codex#` cache prefix.
// Every other family (Claude, Gemini, SWE, Kimi, unknown) is incompatible with a
// GPT target, so their validators are not needed here.
fn gpt_compatible_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() {
        return false;
    }
    if let Some((prefix, rest)) = sig.split_once('#') {
        // SplitSignatureProviderPrefix + SignatureProviderFromCachePrefix
        return matches!(
            prefix.trim().to_lowercase().as_str(),
            "openai" | "gpt" | "codex"
        ) && is_valid_gpt_reasoning_signature(rest.trim());
    }
    // maybeSelfDescribingSignatureEnvelope gate ("CERg"), then the GPT probe.
    sig.as_bytes().first().is_some_and(|c| b"CERg".contains(c))
        && is_valid_gpt_reasoning_signature(sig)
}

// ---------------------------------------------------------------------------
// translator/common helpers
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

// port of AlignOpenAIToolCallMessages (translator/common/openai_tools.go)
fn align_openai_tool_call_messages(messages: Vec<Value>) -> Vec<Value> {
    if messages.len() <= 1 {
        return messages;
    }
    struct AssistantRecord {
        msg_index: usize,
        call_ids: Vec<String>,
        has_invalid_or_empty_id: bool,
    }
    let mut assistants: Vec<AssistantRecord> = Vec::new();
    let mut assistant_by_call_id: HashMap<String, usize> = HashMap::new();
    let mut ambiguous: HashMap<String, bool> = HashMap::new();
    let mut tool_msg_indices: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, raw) in messages.iter().enumerate() {
        match gstr(gget(raw, "role")).as_str() {
            "assistant" => {
                if let Some(Value::Array(raw_calls)) = raw.get("tool_calls") {
                    if !raw_calls.is_empty() {
                        let mut call_ids = Vec::new();
                        let mut has_empty = false;
                        for tc in raw_calls {
                            let call_id = gstr(gget(tc, "id"));
                            if call_id.is_empty() {
                                ambiguous.insert(String::new(), true);
                                has_empty = true;
                                continue;
                            }
                            if assistant_by_call_id.contains_key(&call_id) {
                                ambiguous.insert(call_id.clone(), true);
                            }
                            assistant_by_call_id.insert(call_id.clone(), i);
                            call_ids.push(call_id);
                        }
                        if !call_ids.is_empty() || has_empty {
                            assistants.push(AssistantRecord {
                                msg_index: i,
                                call_ids,
                                has_invalid_or_empty_id: has_empty,
                            });
                        }
                    }
                }
            }
            "tool" => {
                let call_id = gstr(gget(raw, "tool_call_id"));
                if call_id.is_empty() {
                    ambiguous.insert(String::new(), true);
                } else {
                    let list = tool_msg_indices.entry(call_id.clone()).or_default();
                    list.push(i);
                    if list.len() > 1 {
                        ambiguous.insert(call_id, true);
                    }
                }
            }
            _ => {}
        }
    }

    if assistants.is_empty() {
        return messages;
    }

    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for ast in &assistants {
        if ast.has_invalid_or_empty_id {
            continue;
        }
        let mut eligible = true;
        let mut matched = Vec::with_capacity(ast.call_ids.len());
        for call_id in &ast.call_ids {
            if ambiguous.get(call_id).copied().unwrap_or(false) {
                eligible = false;
                break;
            }
            let indices = tool_msg_indices
                .get(call_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if indices.len() != 1 {
                eligible = false;
                break;
            }
            let tool_idx = indices[0];
            if tool_idx <= ast.msg_index {
                eligible = false;
                break;
            }
            matched.push(tool_idx);
        }
        if !eligible {
            continue;
        }
        matched.sort_unstable();
        let already_adjacent = matched
            .iter()
            .enumerate()
            .all(|(offset, &idx)| idx == ast.msg_index + offset + 1);
        if !already_adjacent {
            groups.push((ast.msg_index, matched));
        }
    }

    if groups.is_empty() {
        return messages;
    }

    let mut moved: HashMap<usize, bool> = HashMap::new();
    let mut to_insert: HashMap<usize, Vec<usize>> = HashMap::new();
    for (assistant_index, tool_indices) in groups {
        for &idx in &tool_indices {
            moved.insert(idx, true);
        }
        to_insert.insert(assistant_index, tool_indices);
    }

    let mut reordered = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        if moved.get(&i).copied().unwrap_or(false) {
            continue;
        }
        reordered.push(msg.clone());
        if let Some(tools) = to_insert.get(&i) {
            for &t in tools {
                reordered.push(messages[t].clone());
            }
        }
    }
    reordered
}

// ---------------------------------------------------------------------------
// Request: Anthropic Messages → Chat Completions
// ---------------------------------------------------------------------------

/// Client request (Anthropic Messages body) → upstream request (Chat
/// Completions body). `model` = upstream model id for the body. `stream` =
/// whether the client asked to stream. The Go code sets only `stream` — it
/// does NOT add `stream_options.include_usage` — so neither does this port.
// port of ConvertClaudeRequestToOpenAI (openai_claude_request.go)
// The registered (non-compat) form; the router uses the compat one (see
// `translate::request`). Kept for parity with the Go pair and its tests.
#[allow(dead_code)]
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    convert_claude_request_to_openai(model, body, stream, false)
}

/// Variant that keeps assistant thinking text as `reasoning_content` even when
/// its signature is not a GPT one (Go wires this for configured compatibility
/// endpoints such as DeepSeek, which require reasoning_content echoed back).
// port of ConvertClaudeRequestToOpenAIWithCompat (openai_claude_request.go)
pub fn translate_request_with_compat(model: &str, body: &Value, stream: bool) -> Value {
    convert_claude_request_to_openai(model, body, stream, true)
}

/// An Anthropic server tool: typed (`web_search_20250305`, `code_execution_…`,
/// …) with no `input_schema`. A `custom` type or a schema makes it a
/// client tool.
fn is_anthropic_server_tool(tool: &Value) -> bool {
    let kind = gstr(gget(tool, "type"));
    !kind.is_empty() && kind != "custom" && gget(tool, "input_schema").is_none_or(|s| s.is_null())
}

// port of convertClaudeRequestToOpenAI (openai_claude_request.go)
fn convert_claude_request_to_openai(
    model_name: &str,
    root: &Value,
    stream: bool,
    preserve_thinking_blocks: bool,
) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), Value::from(model_name));
    out.insert("messages".into(), json!([]));

    // Max tokens
    if let Some(v) = gget(root, "max_tokens") {
        out.insert("max_tokens".into(), Value::from(gint(Some(v))));
    }

    // Temperature, else Top P
    if let Some(v) = gget(root, "temperature") {
        out.insert("temperature".into(), float_value(gfloat(Some(v))));
    } else if let Some(v) = gget(root, "top_p") {
        out.insert("top_p".into(), float_value(gfloat(Some(v))));
    }

    // Stop sequences -> stop
    if let Some(Value::Array(seqs)) = gget(root, "stop_sequences") {
        let stops: Vec<Value> = seqs.iter().map(|s| Value::from(gstr(Some(s)))).collect();
        if !stops.is_empty() {
            out.insert("stop".into(), Value::Array(stops));
        }
    }

    // Stream
    out.insert("stream".into(), Value::Bool(stream));

    // Thinking: Claude thinking.budget_tokens -> OpenAI reasoning_effort
    if let Some(thinking_config @ Value::Object(_)) = gget(root, "thinking") {
        if let Some(thinking_type) = gget(thinking_config, "type") {
            match gstr(Some(thinking_type)).as_str() {
                "enabled" => {
                    let budget = match gget(thinking_config, "budget_tokens") {
                        Some(b) => gint(Some(b)),
                        // No budget_tokens specified, default to "auto"
                        None => -1,
                    };
                    if let Some(effort) = convert_budget_to_level(budget) {
                        out.insert("reasoning_effort".into(), Value::from(effort));
                    }
                }
                "adaptive" | "auto" => {
                    let effort = match gget(root, "output_config.effort") {
                        Some(Value::String(s)) => s.trim().to_lowercase(),
                        _ => String::new(),
                    };
                    let effort = if effort.is_empty() {
                        "xhigh".to_string()
                    } else {
                        effort
                    };
                    out.insert("reasoning_effort".into(), Value::from(effort));
                }
                "disabled" => {
                    if let Some(effort) = convert_budget_to_level(0) {
                        out.insert("reasoning_effort".into(), Value::from(effort));
                    }
                }
                _ => {}
            }
        }
    }

    let mut message_items: Vec<Value> = Vec::new();

    // Handle system message first.
    let mut system_content_items: Vec<Value> = Vec::new();
    match gget(root, "system") {
        Some(Value::String(s)) if !s.is_empty() && !is_claude_code_attribution_system_text(s) => {
            system_content_items.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(ci) = convert_claude_content_part(item) {
                    system_content_items.push(ci);
                }
            }
        }
        _ => {}
    }
    if !system_content_items.is_empty() {
        message_items.push(json!({"role": "system", "content": system_content_items}));
    }

    // Process Anthropic messages
    if let Some(Value::Array(messages)) = gget(root, "messages") {
        let mut pending_tool_use_ids: Vec<String> = Vec::new();
        let mut pending_system_reminders: Vec<Value> = Vec::new();
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

        for message in messages {
            let role = gstr(gget(message, "role"));
            let content_result = gget(message, "content");
            if role == "system" {
                if let Some(reminder) = claude_message_system_reminder_text(content_result) {
                    let msg =
                        json!({"role": "user", "content": [{"type": "text", "text": reminder}]});
                    if !pending_tool_use_ids.is_empty() {
                        pending_system_reminders.push(msg);
                    } else {
                        message_items.push(msg);
                    }
                }
                continue;
            }

            match content_result {
                Some(Value::Array(raw_parts)) => {
                    let parts = if role == "user" && !pending_tool_use_ids.is_empty() {
                        align_claude_tool_results(raw_parts, &pending_tool_use_ids)
                    } else {
                        raw_parts.clone()
                    };
                    let preceding_tool_calls_pending = !pending_tool_use_ids.is_empty();
                    pending_tool_use_ids.clear();

                    let mut content_items: Vec<Value> = Vec::new();
                    let mut reasoning_parts: Vec<String> = Vec::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    let mut tool_results: Vec<Value> = Vec::new();
                    let mut relayed_tool_images: Vec<Value> = Vec::new();

                    for part in &parts {
                        match gstr(gget(part, "type")).as_str() {
                            // Only assistant thinking maps to reasoning_content; thinking
                            // in other roles falls through to the no-op arm.
                            "thinking" if role == "assistant" => {
                                if !should_map_claude_thinking_to_gpt_reasoning(
                                    part,
                                    preserve_thinking_blocks,
                                ) {
                                    continue;
                                }
                                let text = get_thinking_text(part);
                                if !text.trim().is_empty() {
                                    reasoning_parts.push(text);
                                }
                            }
                            "redacted_thinking" => {}
                            "text" | "image" => {
                                if let Some(ci) = convert_claude_content_part(part) {
                                    content_items.push(ci);
                                }
                            }
                            // Only assistant tool_use becomes tool_calls (injection guard).
                            "tool_use" if role == "assistant" => {
                                let tool_use_id = gstr(gget(part, "id"));
                                let tool_name = gstr(gget(part, "name"));
                                if !tool_use_id.is_empty() {
                                    pending_tool_use_ids.push(tool_use_id.clone());
                                    if !tool_name.is_empty() {
                                        tool_name_by_id
                                            .insert(tool_use_id.clone(), tool_name.clone());
                                    }
                                }
                                let arguments = match gget(part, "input") {
                                    Some(input) => input.to_string(),
                                    None => "{}".to_string(),
                                };
                                tool_calls.push(json!({
                                    "id": tool_use_id,
                                    "type": "function",
                                    "function": {"name": tool_name, "arguments": arguments}
                                }));
                            }
                            "tool_result" => {
                                let tool_use_id = gstr(gget(part, "tool_use_id"));
                                let mut tr = Map::new();
                                tr.insert("role".into(), Value::from("tool"));
                                tr.insert("tool_call_id".into(), Value::from(tool_use_id.clone()));
                                tr.insert("content".into(), Value::from(""));
                                if let Some(name) = tool_name_by_id.get(&tool_use_id) {
                                    if !name.is_empty() {
                                        tr.insert("name".into(), Value::from(name.clone()));
                                    }
                                }
                                let (text, images) =
                                    convert_claude_tool_result_content(gget(part, "content"));
                                tr.insert("content".into(), Value::from(text));
                                relayed_tool_images.extend(images);
                                tool_results.push(Value::Object(tr));
                            }
                            _ => {}
                        }
                    }

                    let reasoning_content = reasoning_parts.join("\n\n");
                    let has_content = !content_items.is_empty();
                    let has_reasoning = !reasoning_content.is_empty();
                    let has_tool_calls = !tool_calls.is_empty();
                    let has_tool_results = !tool_results.is_empty();

                    // Flush pending reminders when no tool_results answered the preceding calls.
                    if preceding_tool_calls_pending
                        && !has_tool_results
                        && !pending_system_reminders.is_empty()
                    {
                        message_items.append(&mut pending_system_reminders);
                    }

                    // Tool messages must immediately follow the assistant tool_calls.
                    message_items.extend(tool_results);

                    // Tool messages cannot carry images; replay them as a user message.
                    if !relayed_tool_images.is_empty() {
                        let mut relay_items =
                            vec![json!({"type": "text", "text": TOOL_RESULT_IMAGE_RELAY_NOTICE})];
                        relay_items.extend(relayed_tool_images);
                        if role == "user" && has_content {
                            relay_items.append(&mut content_items);
                            content_items = relay_items;
                        } else {
                            message_items.push(json!({"role": "user", "content": relay_items}));
                        }
                    }

                    if !pending_system_reminders.is_empty() {
                        message_items.append(&mut pending_system_reminders);
                    }

                    if role == "assistant" {
                        if has_content || has_reasoning || has_tool_calls {
                            let mut msg = Map::new();
                            msg.insert("role".into(), Value::from("assistant"));
                            if has_content {
                                msg.insert("content".into(), Value::Array(content_items));
                            } else {
                                msg.insert("content".into(), Value::from(""));
                            }
                            if has_reasoning {
                                msg.insert(
                                    "reasoning_content".into(),
                                    Value::from(reasoning_content),
                                );
                            }
                            if has_tool_calls {
                                msg.insert("tool_calls".into(), Value::Array(tool_calls));
                            }
                            message_items.push(Value::Object(msg));
                        }
                    } else if has_content {
                        message_items.push(json!({"role": role, "content": content_items}));
                    }
                }
                Some(Value::String(s)) => {
                    message_items.push(json!({"role": role, "content": s}));
                }
                _ => {}
            }
        }
        message_items.append(&mut pending_system_reminders);
    }

    // Set messages.
    if !message_items.is_empty() {
        let aligned = align_openai_tool_call_messages(message_items);
        out.insert("messages".into(), Value::Array(aligned));
    }

    // Tools. Anthropic SERVER tools (`web_search_20250305`, …: a `type`
    // other than `custom` and no `input_schema`) run on Anthropic's side;
    // forwarded as a callable function, the model may call `web_search` and
    // the client has no such tool to run. Left out, as the Gemini
    // translator does.
    if let Some(Value::Array(tools)) = gget(root, "tools") {
        let mut tool_items = Vec::new();
        for tool in tools {
            if is_anthropic_server_tool(tool) {
                continue;
            }
            let parameters = match gget(tool, "input_schema") {
                Some(schema) if !schema.is_null() => {
                    normalize_object_schema_properties(schema.clone())
                }
                _ => json!({"type": "object", "properties": {}}),
            };
            tool_items.push(json!({
                "type": "function",
                "function": {
                    "name": gstr(gget(tool, "name")),
                    "description": gstr(gget(tool, "description")),
                    "parameters": parameters
                }
            }));
        }
        if !tool_items.is_empty() {
            out.insert("tools".into(), Value::Array(tool_items));
        }
    }

    // Tool choice
    if let Some(tool_choice) = gget(root, "tool_choice") {
        if !tool_choice.is_null() {
            let mut choice_type = gstr(gget(tool_choice, "type"));
            if choice_type.is_empty() {
                if let Value::String(s) = tool_choice {
                    choice_type = s.clone();
                }
            }
            let mapped = match choice_type.as_str() {
                "auto" => Value::from("auto"),
                "any" => Value::from("required"),
                "none" => Value::from("none"),
                "tool" => {
                    let tool_name = gstr(gget(tool_choice, "name"));
                    if !tool_name.is_empty() {
                        json!({"type": "function", "function": {"name": tool_name}})
                    } else {
                        Value::from("none")
                    }
                }
                // Fail closed: unrecognized values must not turn into permission
                _ => Value::from("none"),
            };
            out.insert("tool_choice".into(), mapped);
            if gget(tool_choice, "disable_parallel_tool_use") == Some(&Value::Bool(true)) {
                out.insert("parallel_tool_calls".into(), Value::Bool(false));
            }
        }
    }

    // user (for tracking)
    if let Some(user) = gget(root, "user") {
        out.insert("user".into(), Value::from(gstr(Some(user))));
    }

    Value::Object(out)
}

// port of normalizeObjectSchemaProperties (openai_claude_request.go)
fn normalize_object_schema_properties(schema: Value) -> Value {
    match schema {
        Value::Object(mut value) => {
            if value.get("type").and_then(Value::as_str) == Some("object")
                && !value.contains_key("properties")
            {
                value.insert("properties".into(), json!({}));
            }
            let drop_pattern = matches!(
                value.get("pattern"),
                Some(Value::String(p)) if has_unsupported_unicode_property_escape(p)
            );
            if drop_pattern {
                value.shift_remove("pattern");
            }
            if let Some(Value::Object(pattern_props)) = value.get_mut("patternProperties") {
                let keys: Vec<String> = pattern_props.keys().cloned().collect();
                for key in keys {
                    if has_unsupported_unicode_property_escape(&key) {
                        pattern_props.shift_remove(&key);
                    } else if let Some(sub) = pattern_props.get_mut(&key) {
                        *sub = normalize_object_schema_properties(sub.take());
                    }
                }
            }
            for map_key in SCHEMA_MAP_KEYWORDS {
                if map_key == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = value.get_mut(map_key) {
                    for (_, sub) in sub_map.iter_mut() {
                        *sub = normalize_object_schema_properties(sub.take());
                    }
                }
            }
            for val_key in SCHEMA_VALUE_KEYWORDS {
                match value.get_mut(val_key) {
                    Some(v @ Value::Object(_)) => {
                        *v = normalize_object_schema_properties(v.take());
                    }
                    Some(Value::Array(items)) => {
                        for item in items.iter_mut() {
                            *item = normalize_object_schema_properties(item.take());
                        }
                    }
                    _ => {}
                }
            }
            Value::Object(value)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(normalize_object_schema_properties)
                .collect(),
        ),
        other => other,
    }
}

// port of shouldMapClaudeThinkingToGPTReasoning (openai_claude_request.go)
fn should_map_claude_thinking_to_gpt_reasoning(part: &Value, preserve_thinking: bool) -> bool {
    if preserve_thinking {
        return true;
    }
    let Some(signature) = gget(part, "signature") else {
        return false;
    };
    let signature = gstr(Some(signature));
    if signature.trim().is_empty() {
        return false;
    }
    gpt_compatible_signature(&signature)
}

// port of convertClaudeContentPart (openai_claude_request.go)
fn convert_claude_content_part(part: &Value) -> Option<Value> {
    match gstr(gget(part, "type")).as_str() {
        "text" => {
            let text = gstr(gget(part, "text"));
            if text.trim().is_empty() || is_claude_code_attribution_system_text(&text) {
                return None;
            }
            Some(json!({"type": "text", "text": text}))
        }
        "image" => {
            let mut image_url = String::new();
            if let Some(source) = gget(part, "source") {
                match gstr(gget(source, "type")).as_str() {
                    "base64" => {
                        let mut media_type = gstr(gget(source, "media_type"));
                        if media_type.is_empty() {
                            media_type = "application/octet-stream".into();
                        }
                        let data = gstr(gget(source, "data"));
                        if !data.is_empty() {
                            image_url = format!("data:{media_type};base64,{data}");
                        }
                    }
                    "url" => image_url = gstr(gget(source, "url")),
                    _ => {}
                }
            }
            if image_url.is_empty() {
                image_url = gstr(gget(part, "url"));
            }
            if image_url.is_empty() {
                return None;
            }
            Some(json!({"type": "image_url", "image_url": {"url": image_url}}))
        }
        _ => None,
    }
}

// port of toolResultImagePlaceholder (openai_claude_request.go)
const TOOL_RESULT_IMAGE_PLACEHOLDER: &str =
    "[Tool returned image content; the images follow in the next user message.]";

// port of toolResultImageRelayNotice (openai_claude_request.go)
const TOOL_RESULT_IMAGE_RELAY_NOTICE: &str = "Images returned by the preceding tool call(s):";

// port of convertClaudeToolResultContent (openai_claude_request.go)
fn convert_claude_tool_result_content(content: Option<&Value>) -> (String, Vec<Value>) {
    let Some(content) = content else {
        return (String::new(), Vec::new());
    };
    match content {
        Value::String(s) => (s.clone(), Vec::new()),
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            let mut images: Vec<Value> = Vec::new();
            for item in items {
                match item {
                    Value::String(s) => parts.push(s.clone()),
                    Value::Object(_) if gstr(gget(item, "type")) == "text" => {
                        parts.push(gstr(gget(item, "text")))
                    }
                    Value::Object(_) if gstr(gget(item, "type")) == "image" => {
                        match convert_claude_content_part(item) {
                            Some(ci) => images.push(ci),
                            None => parts.push(item.to_string()),
                        }
                    }
                    Value::Object(_) if is_str(gget(item, "text")) => {
                        parts.push(gstr(gget(item, "text")))
                    }
                    _ => parts.push(item.to_string()),
                }
            }
            let joined = parts.join("\n\n");
            if joined.trim().is_empty() {
                if !images.is_empty() {
                    return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_string(), images);
                }
                return (content.to_string(), Vec::new());
            }
            (joined, images)
        }
        Value::Object(_) => {
            if gstr(gget(content, "type")) == "image" {
                if let Some(ci) = convert_claude_content_part(content) {
                    return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_string(), vec![ci]);
                }
            }
            if let Some(Value::String(t)) = gget(content, "text") {
                return (t.clone(), Vec::new());
            }
            (content.to_string(), Vec::new())
        }
        other => (other.to_string(), Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

// port of mapOpenAIFinishReasonToAnthropic (openai_claude_response.go)
fn map_openai_finish_reason_to_anthropic(reason: &str) -> &'static str {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "end_turn",
        "function_call" => "tool_use",
        _ => "end_turn",
    }
}

// port of collectOpenAIObjectReasoningTexts (openai_claude_response.go)
fn collect_openai_object_reasoning_texts(obj: Option<&Value>) -> Vec<String> {
    let Some(obj) = obj else {
        return Vec::new();
    };
    for path in ["reasoning_content", "reasoning", "reasoning_details"] {
        let texts = collect_openai_reasoning_texts(gget(obj, path));
        if !texts.is_empty() {
            return texts;
        }
    }
    Vec::new()
}

// port of collectOpenAIReasoningTexts (openai_claude_response.go)
fn collect_openai_reasoning_texts(node: Option<&Value>) -> Vec<String> {
    let mut texts = Vec::new();
    match node {
        Some(Value::Array(items)) => {
            for item in items {
                texts.extend(collect_openai_reasoning_texts(Some(item)));
            }
        }
        Some(Value::String(s)) if !s.is_empty() => texts.push(s.clone()),
        Some(obj @ Value::Object(_)) => {
            if let Some(text) = gget(obj, "text") {
                let s = gstr(Some(text));
                if !s.is_empty() {
                    texts.push(s);
                }
            }
        }
        _ => {}
    }
    texts
}

// port of extractOpenAIUsage (openai_claude_response.go)
fn extract_openai_usage(usage: Option<&Value>) -> (i64, i64, i64, i64) {
    let usage = match usage {
        None | Some(Value::Null) => return (0, 0, 0, 0),
        Some(u) => u,
    };
    let mut input_tokens = gint(gget(usage, "prompt_tokens"));
    let output_tokens = gint(gget(usage, "completion_tokens"));
    let cached_tokens = gint(gget(usage, "prompt_tokens_details.cached_tokens"));
    let mut cache_write_tokens = gint(gget(usage, "prompt_tokens_details.cache_write_tokens"));
    if cache_write_tokens <= 0 {
        cache_write_tokens = gint(gget(usage, "prompt_tokens_details.cache_creation_tokens"));
    }

    let mut deduct: i64 = 0;
    if cached_tokens > 0 {
        deduct += cached_tokens;
    }
    if cache_write_tokens > 0 {
        deduct = deduct.checked_add(cache_write_tokens).unwrap_or(i64::MAX);
    }
    if deduct > 0 {
        if input_tokens >= deduct {
            input_tokens -= deduct;
        } else {
            input_tokens = 0;
        }
    }
    if input_tokens < 0 {
        input_tokens = 0;
    }
    (
        input_tokens,
        output_tokens,
        cached_tokens,
        cache_write_tokens,
    )
}

/// Builds a `tool_use` content block from one Chat `tool_calls[]` entry.
/// Shared body of the two identical loops in the Go non-stream converters.
fn tool_use_block_from_call(tool_call: &Value, name: String) -> Value {
    let args = fix_json(&gstr(gget(tool_call, "function.arguments")));
    let input = match serde_json::from_str::<Value>(&args) {
        Ok(v @ Value::Object(_)) if !args.is_empty() => v,
        _ => json!({}),
    };
    json!({
        "type": "tool_use",
        "id": sanitize_claude_tool_id(&gstr(gget(tool_call, "id"))),
        "name": name,
        "input": input
    })
}

fn set_usage(out: &mut Map<String, Value>, usage: Option<&Value>) {
    let (input, output, cached, cache_write) = extract_openai_usage(usage);
    if let Some(Value::Object(u)) = out.get_mut("usage") {
        u.insert("input_tokens".into(), Value::from(input));
        u.insert("output_tokens".into(), Value::from(output));
        if cached > 0 {
            u.insert("cache_read_input_tokens".into(), Value::from(cached));
        }
        if cache_write > 0 {
            u.insert(
                "cache_creation_input_tokens".into(),
                Value::from(cache_write),
            );
        }
    }
}

fn empty_message(root: &Value) -> Map<String, Value> {
    let Value::Object(m) = json!({
        "id": gstr(gget(root, "id")),
        "type": "message",
        "role": "assistant",
        "model": gstr(gget(root, "model")),
        "content": [],
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0}
    }) else {
        unreachable!()
    };
    m
}

/// Complete upstream non-stream response (chat.completion JSON) → client
/// Anthropic Messages response JSON.
// port of ConvertOpenAIResponseToClaudeNonStream (openai_claude_response.go)
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let root = upstream;
    let tool_name_map = tool_name_map_from_claude_request(original_request);
    let mut out = empty_message(root);

    let mut has_tool_call = false;
    let mut stop_reason_set = false;
    let mut blocks: Vec<Value> = Vec::new();

    if let Some(Value::Array(choices)) = gget(root, "choices") {
        if let Some(choice) = choices.first() {
            if let Some(fr) = gget(choice, "finish_reason") {
                out.insert(
                    "stop_reason".into(),
                    Value::from(map_openai_finish_reason_to_anthropic(&gstr(Some(fr)))),
                );
                stop_reason_set = true;
            }

            if let Some(message) = gget(choice, "message") {
                match gget(message, "content") {
                    Some(Value::Array(items)) => {
                        let mut text_builder = String::new();
                        let mut thinking_builder = String::new();
                        fn flush_text(b: &mut String, blocks: &mut Vec<Value>) {
                            if !b.is_empty() {
                                blocks.push(json!({"type": "text", "text": std::mem::take(b)}));
                            }
                        }
                        fn flush_thinking(b: &mut String, blocks: &mut Vec<Value>) {
                            if !b.is_empty() {
                                blocks.push(
                                    json!({"type": "thinking", "thinking": std::mem::take(b)}),
                                );
                            }
                        }
                        for item in items {
                            match gstr(gget(item, "type")).as_str() {
                                "text" => {
                                    flush_thinking(&mut thinking_builder, &mut blocks);
                                    text_builder.push_str(&gstr(gget(item, "text")));
                                }
                                "tool_calls" => {
                                    flush_thinking(&mut thinking_builder, &mut blocks);
                                    flush_text(&mut text_builder, &mut blocks);
                                    if let Some(Value::Array(tcs)) = gget(item, "tool_calls") {
                                        for tc in tcs {
                                            has_tool_call = true;
                                            let name = map_tool_name(
                                                tool_name_map.as_ref(),
                                                &gstr(gget(tc, "function.name")),
                                            );
                                            blocks.push(tool_use_block_from_call(tc, name));
                                        }
                                    }
                                }
                                "reasoning" => {
                                    flush_text(&mut text_builder, &mut blocks);
                                    if let Some(t) = gget(item, "text") {
                                        thinking_builder.push_str(&gstr(Some(t)));
                                    }
                                }
                                _ => {
                                    flush_thinking(&mut thinking_builder, &mut blocks);
                                    flush_text(&mut text_builder, &mut blocks);
                                }
                            }
                        }
                        flush_thinking(&mut thinking_builder, &mut blocks);
                        flush_text(&mut text_builder, &mut blocks);
                    }
                    Some(Value::String(s)) if !s.is_empty() => {
                        blocks.push(json!({"type": "text", "text": s}));
                    }
                    _ => {}
                }

                for text in collect_openai_object_reasoning_texts(Some(message)) {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "thinking", "thinking": text}));
                    }
                }

                if let Some(Value::Array(tcs)) = gget(message, "tool_calls") {
                    for tc in tcs {
                        has_tool_call = true;
                        let name =
                            map_tool_name(tool_name_map.as_ref(), &gstr(gget(tc, "function.name")));
                        blocks.push(tool_use_block_from_call(tc, name));
                    }
                }
            }
        }
    }

    if !blocks.is_empty() {
        out.insert("content".into(), Value::Array(blocks));
    }

    if let Some(usage) = gget(root, "usage") {
        set_usage(&mut out, Some(usage));
    }

    if !stop_reason_set {
        let reason = if has_tool_call {
            "tool_use"
        } else {
            "end_turn"
        };
        out.insert("stop_reason".into(), Value::from(reason));
    }

    Value::Object(out)
}

/// Go's streaming entry point falls back to this when the ORIGINAL request did
/// not ask for a stream. `StreamTranslator` is only ever built for a streaming
/// client, so this is kept for parity and covered by a test, not wired in.
// port of convertOpenAINonStreamingToAnthropic (openai_claude_response.go)
#[allow(dead_code)]
fn convert_openai_non_streaming_to_anthropic(root: &Value) -> Value {
    let mut out = empty_message(root);
    if let Some(Value::Array(choices)) = gget(root, "choices") {
        if let Some(choice) = choices.first() {
            let mut blocks: Vec<Value> = Vec::new();
            for text in collect_openai_object_reasoning_texts(gget(choice, "message")) {
                if !text.is_empty() {
                    blocks.push(json!({"type": "thinking", "thinking": text}));
                }
            }
            if let Some(content) = gget(choice, "message.content") {
                let s = gstr(Some(content));
                if !s.is_empty() {
                    blocks.push(json!({"type": "text", "text": s}));
                }
            }
            if let Some(Value::Array(tcs)) = gget(choice, "message.tool_calls") {
                for tc in tcs {
                    let name = gstr(gget(tc, "function.name"));
                    blocks.push(tool_use_block_from_call(tc, name));
                }
            }
            if !blocks.is_empty() {
                out.insert("content".into(), Value::Array(blocks));
            }
            if let Some(fr) = gget(choice, "finish_reason") {
                out.insert(
                    "stop_reason".into(),
                    Value::from(map_openai_finish_reason_to_anthropic(&gstr(Some(fr)))),
                );
            }
        }
    }
    if let Some(usage) = gget(root, "usage") {
        set_usage(&mut out, Some(usage));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

// port of InterleavedContentChunk (openai_claude_response.go)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone)]
struct InterleavedContentChunk {
    kind: ChunkKind,
    text: String,
}

// port of ToolCallAccumulator (openai_claude_response.go)
#[derive(Debug, Default, Clone)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    start_emitted: bool,
}

/// Per-stream state.
// port of ConvertOpenAIResponseToAnthropicParams (openai_claude_response.go).
// `CreatedAt` is dropped: Go records it but never reads it.
pub struct StreamTranslator {
    message_id: String,
    model: String,
    tool_name_map: Option<HashMap<String, String>>,
    saw_tool_call: bool,
    content_accumulator_len: usize,
    tool_calls_accumulator: BTreeMap<i64, ToolCallAccumulator>,
    text_content_block_started: bool,
    thinking_content_block_started: bool,
    finish_reason: String,
    content_blocks_stopped: bool,
    message_delta_sent: bool,
    message_started: bool,
    message_stop_sent: bool,
    tool_call_block_indexes: HashMap<i64, i64>,
    text_content_block_index: i64,
    thinking_content_block_index: i64,
    next_content_block_index: i64,
    open_tool_call_index: i64,
    interleaved_content_chunks: Vec<InterleavedContentChunk>,
    usage_input_tokens: i64,
    usage_output_tokens: i64,
    usage_cached_tokens: i64,
    usage_cache_write_tokens: i64,
    /// An in-stream error was reported: nothing is closed as complete after it.
    errored: bool,
}

impl StreamTranslator {
    // port of the param initialisation in ConvertOpenAIResponseToClaude
    // (openai_claude_response.go), including the lazy ToolNameMap fill.
    pub fn new(original_request: &Value) -> Self {
        Self {
            message_id: String::new(),
            model: String::new(),
            tool_name_map: tool_name_map_from_claude_request(original_request),
            saw_tool_call: false,
            content_accumulator_len: 0,
            tool_calls_accumulator: BTreeMap::new(),
            text_content_block_started: false,
            thinking_content_block_started: false,
            finish_reason: String::new(),
            content_blocks_stopped: false,
            message_delta_sent: false,
            message_started: false,
            message_stop_sent: false,
            tool_call_block_indexes: HashMap::new(),
            text_content_block_index: -1,
            thinking_content_block_index: -1,
            next_content_block_index: 0,
            open_tool_call_index: -1,
            interleaved_content_chunks: Vec::new(),
            usage_input_tokens: 0,
            usage_output_tokens: 0,
            usage_cached_tokens: 0,
            usage_cache_write_tokens: 0,
            errored: false,
        }
    }

    /// One upstream Chat SSE chunk. `event` is ignored, exactly as the Go code
    /// only looks at the `data:` payload.
    // port of ConvertOpenAIResponseToClaude (openai_claude_response.go), stream branch
    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert_streaming_chunk(data)
    }

    /// Upstream stream ended (the `[DONE]` marker, or EOF without one).
    // port of convertOpenAIDoneToAnthropic (openai_claude_response.go)
    pub fn finish(&mut self) -> Vec<String> {
        let mut results = Vec::new();
        if self.errored {
            return results;
        }
        self.finalize_content_blocks(&mut results);
        if !self.message_delta_sent {
            self.emit_message_delta(&mut results);
        }
        self.emit_message_stop_if_needed(&mut results);
        results
    }

    // port of hasValidToolCallArguments (openai_claude_response.go)
    fn has_valid_tool_call_arguments(&self) -> bool {
        for acc in self.tool_calls_accumulator.values() {
            if !acc.start_emitted
                && acc.name.is_empty()
                && acc.id.is_empty()
                && acc.arguments.is_empty()
            {
                continue;
            }
            if acc.arguments.is_empty() {
                continue;
            }
            let args = acc.arguments.trim();
            if args.is_empty() {
                return false;
            }
            if args == "{}" {
                continue;
            }
            let fixed = fix_json(args);
            if !matches!(serde_json::from_str::<Value>(&fixed), Ok(Value::Object(_))) {
                return false;
            }
        }
        true
    }

    // port of effectiveOpenAIFinishReason (openai_claude_response.go)
    fn effective_finish_reason(&self) -> String {
        if self.finish_reason == "length" || self.finish_reason == "content_filter" {
            return self.finish_reason.clone();
        }
        if self.saw_tool_call {
            if self.has_valid_tool_call_arguments() {
                return "tool_calls".into();
            }
            return "length".into();
        }
        self.finish_reason.clone()
    }

    // port of terminalOpenAIFinishReason (openai_claude_response.go)
    fn terminal_finish_reason(&self) -> String {
        let r = self.effective_finish_reason();
        if r.is_empty() {
            "stop".into()
        } else {
            r
        }
    }

    fn push_interleaved(&mut self, kind: ChunkKind, text: &str) {
        match self.interleaved_content_chunks.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(text),
            _ => self
                .interleaved_content_chunks
                .push(InterleavedContentChunk {
                    kind,
                    text: text.to_string(),
                }),
        }
    }

    // port of convertOpenAIStreamingChunkToAnthropic (openai_claude_response.go)
    fn convert_streaming_chunk(&mut self, root: &Value) -> Vec<String> {
        let mut results = Vec::new();

        // An error delivered inside a 200 stream (`{"error":{…}}`, no
        // `choices` — OpenAI-compatible gateways report an exhausted balance
        // or an upstream fault this way). Claude Code gets the error, not an
        // empty answer. Not in Go, which has no branch for it.
        if let Some(err) = root.get("error").filter(|e| e.is_object()) {
            if root.get("choices").is_none() {
                self.errored = true;
                let kind = match gstr(err.get("type")).as_str() {
                    "" => "api_error".to_string(),
                    t => t.to_string(),
                };
                let mut message = gstr(err.get("message"));
                if message.is_empty() {
                    message = gstr(err.get("code"));
                }
                if message.is_empty() {
                    message = kind.clone();
                }
                results.push(sse_frame(
                    "error",
                    &json!({"type": "error", "error": {"type": kind, "message": message}}),
                ));
                return results;
            }
        }

        if self.message_id.is_empty() {
            self.message_id = gstr(gget(root, "id"));
        }
        if self.model.is_empty() {
            self.model = gstr(gget(root, "model"));
        }

        // message_start on the very first chunk carrying a delta.
        if let Some(delta) = gget(root, "choices.0.delta") {
            if !self.message_started {
                let start = json!({
                    "type": "message_start",
                    "message": {
                        "id": self.message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                });
                results.push(sse_frame("message_start", &start));
                self.message_started = true;
            }

            // Reasoning content delta
            for reasoning_text in collect_openai_object_reasoning_texts(Some(delta)) {
                if reasoning_text.is_empty() {
                    continue;
                }
                if self.open_tool_call_index != -1 {
                    self.push_interleaved(ChunkKind::Thinking, &reasoning_text);
                } else {
                    self.stop_text_content_block(&mut results);
                    if !self.thinking_content_block_started {
                        if self.thinking_content_block_index == -1 {
                            self.thinking_content_block_index = self.next_content_block_index;
                            self.next_content_block_index += 1;
                        }
                        let start = json!({
                            "type": "content_block_start",
                            "index": self.thinking_content_block_index,
                            "content_block": {"type": "thinking", "thinking": ""}
                        });
                        results.push(sse_frame("content_block_start", &start));
                        self.thinking_content_block_started = true;
                    }
                    let d = json!({
                        "type": "content_block_delta",
                        "index": self.thinking_content_block_index,
                        "delta": {"type": "thinking_delta", "thinking": reasoning_text}
                    });
                    results.push(sse_frame("content_block_delta", &d));
                }
            }

            // Content delta
            if let Some(content) = gget(delta, "content") {
                let text = gstr(Some(content));
                if !text.is_empty() {
                    if self.open_tool_call_index != -1 {
                        // A tool_use block is open on the wire: buffer so blocks stay sequential.
                        self.push_interleaved(ChunkKind::Text, &text);
                        self.content_accumulator_len += text.len();
                    } else {
                        if !self.text_content_block_started {
                            self.stop_thinking_content_block(&mut results);
                            if self.text_content_block_index == -1 {
                                self.text_content_block_index = self.next_content_block_index;
                                self.next_content_block_index += 1;
                            }
                            let start = json!({
                                "type": "content_block_start",
                                "index": self.text_content_block_index,
                                "content_block": {"type": "text", "text": ""}
                            });
                            results.push(sse_frame("content_block_start", &start));
                            self.text_content_block_started = true;
                        }
                        let d = json!({
                            "type": "content_block_delta",
                            "index": self.text_content_block_index,
                            "delta": {"type": "text_delta", "text": text}
                        });
                        results.push(sse_frame("content_block_delta", &d));
                        self.content_accumulator_len += text.len();
                    }
                }
            }

            // Tool calls
            if let Some(Value::Array(tool_calls)) = gget(delta, "tool_calls") {
                for (array_index, tool_call) in tool_calls.iter().enumerate() {
                    let index = match gget(tool_call, "index") {
                        Some(i) => gint(Some(i)),
                        None => array_index as i64,
                    };
                    let acc = self.tool_calls_accumulator.entry(index).or_default();

                    // Only accept JSON-string, non-empty ids.
                    if let Some(Value::String(id)) = gget(tool_call, "id") {
                        if !id.is_empty() {
                            acc.id = id.clone();
                        }
                    }

                    if let Some(function) = gget(tool_call, "function") {
                        // Record the name only until content_block_start is emitted.
                        if !acc.start_emitted {
                            if let Some(Value::String(name)) = gget(function, "name") {
                                if !name.is_empty() {
                                    acc.name = map_tool_name(self.tool_name_map.as_ref(), name);
                                }
                            }
                        }
                        if let Some(args) = gget(function, "arguments") {
                            let args_text = gstr(Some(args));
                            if !args_text.is_empty() {
                                acc.arguments.push_str(&args_text);
                            }
                        }
                    }

                    // Re-checked on every chunk; mid-stream start only when no other
                    // tool block is open.
                    let ready = !acc.start_emitted && !acc.name.is_empty() && !acc.id.is_empty();
                    if ready && !self.content_blocks_stopped && self.open_tool_call_index == -1 {
                        self.emit_tool_use_start(index, &mut results);
                    }
                }
            }
        }

        // finish_reason (message_delta waits for usage or [DONE])
        if let Some(fr) = gget(root, "choices.0.finish_reason") {
            let reason = gstr(Some(fr));
            if !reason.is_empty() {
                self.finish_reason = if reason == "length" {
                    "length".into()
                } else if reason == "content_filter" {
                    "content_filter".into()
                } else if self.saw_tool_call {
                    if self.has_valid_tool_call_arguments() {
                        "tool_calls".into()
                    } else {
                        "length".into()
                    }
                } else if reason == "tool_calls" {
                    "stop".into()
                } else {
                    reason
                };
                self.finalize_content_blocks(&mut results);
            }
        }

        // Cache usage whenever present
        let usage = gget(root, "usage");
        let has_usage = matches!(usage, Some(u) if !u.is_null());
        if has_usage {
            let (i, o, c, w) = extract_openai_usage(usage);
            self.usage_input_tokens = i;
            self.usage_output_tokens = o;
            self.usage_cached_tokens = c;
            self.usage_cache_write_tokens = w;
        }

        let is_trailing_usage_chunk = has_usage
            && gget(root, "choices.0").is_none()
            && (!self.finish_reason.is_empty()
                || self.saw_tool_call
                || self.text_content_block_started
                || self.thinking_content_block_started
                || self.content_accumulator_len > 0
                || !self.interleaved_content_chunks.is_empty());

        if !self.message_delta_sent
            && (!self.finish_reason.is_empty() || is_trailing_usage_chunk)
            && has_usage
        {
            self.finalize_content_blocks(&mut results);
            self.emit_message_delta(&mut results);
            self.emit_message_stop_if_needed(&mut results);
        }

        results
    }

    // port of toolContentBlockIndex (openai_claude_response.go)
    fn tool_content_block_index(&mut self, openai_tool_index: i64) -> i64 {
        if let Some(&idx) = self.tool_call_block_indexes.get(&openai_tool_index) {
            return idx;
        }
        let idx = self.next_content_block_index;
        self.next_content_block_index += 1;
        self.tool_call_block_indexes.insert(openai_tool_index, idx);
        idx
    }

    // port of stopThinkingContentBlock (openai_claude_response.go)
    fn stop_thinking_content_block(&mut self, results: &mut Vec<String>) {
        if !self.thinking_content_block_started {
            return;
        }
        let stop =
            json!({"type": "content_block_stop", "index": self.thinking_content_block_index});
        results.push(sse_frame("content_block_stop", &stop));
        self.thinking_content_block_started = false;
        self.thinking_content_block_index = -1;
    }

    // port of emitMessageStopIfNeeded (openai_claude_response.go)
    fn emit_message_stop_if_needed(&mut self, results: &mut Vec<String>) {
        if self.message_stop_sent {
            return;
        }
        results.push(sse_frame("message_stop", &json!({"type": "message_stop"})));
        self.message_stop_sent = true;
    }

    // port of stopTextContentBlock (openai_claude_response.go)
    fn stop_text_content_block(&mut self, results: &mut Vec<String>) {
        if !self.text_content_block_started {
            return;
        }
        let stop = json!({"type": "content_block_stop", "index": self.text_content_block_index});
        results.push(sse_frame("content_block_stop", &stop));
        self.text_content_block_started = false;
        self.text_content_block_index = -1;
    }

    // port of emitToolUseStart (openai_claude_response.go)
    fn emit_tool_use_start(&mut self, openai_tool_index: i64, results: &mut Vec<String>) {
        self.stop_thinking_content_block(results);
        self.stop_text_content_block(results);

        let block_index = self.tool_content_block_index(openai_tool_index);
        let acc = self
            .tool_calls_accumulator
            .entry(openai_tool_index)
            .or_default();
        let start = json!({
            "type": "content_block_start",
            "index": block_index,
            "content_block": {
                "type": "tool_use",
                "id": sanitize_claude_tool_id(&acc.id),
                "name": acc.name,
                "input": {}
            }
        });
        results.push(sse_frame("content_block_start", &start));
        acc.start_emitted = true;
        self.saw_tool_call = true;
        self.open_tool_call_index = openai_tool_index;
    }

    // port of emitBelatedToolUseStart (openai_claude_response.go)
    fn emit_belated_tool_use_start(
        &mut self,
        openai_tool_index: i64,
        results: &mut Vec<String>,
    ) -> bool {
        let Some(acc) = self.tool_calls_accumulator.get_mut(&openai_tool_index) else {
            return false;
        };
        if acc.start_emitted {
            return true;
        }
        if acc.name.is_empty() && acc.id.is_empty() && acc.arguments.is_empty() {
            return false;
        }
        if acc.name.is_empty() {
            acc.name = format!("tool_{openai_tool_index}");
        }
        self.emit_tool_use_start(openai_tool_index, results);
        true
    }

    // port of finalizeSingleToolCall (openai_claude_response.go)
    fn finalize_single_tool_call(&mut self, openai_tool_index: i64, results: &mut Vec<String>) {
        let Some(acc) = self.tool_calls_accumulator.get(&openai_tool_index) else {
            return;
        };
        if !acc.start_emitted && !self.emit_belated_tool_use_start(openai_tool_index, results) {
            return;
        }
        let block_index = self.tool_content_block_index(openai_tool_index);
        let arguments = self
            .tool_calls_accumulator
            .get(&openai_tool_index)
            .map(|a| a.arguments.clone())
            .unwrap_or_default();

        // Complete input_json_delta with all accumulated arguments
        if !arguments.is_empty() {
            let d = json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {"type": "input_json_delta", "partial_json": fix_json(&arguments)}
            });
            results.push(sse_frame("content_block_delta", &d));
        }

        let stop = json!({"type": "content_block_stop", "index": block_index});
        results.push(sse_frame("content_block_stop", &stop));
        self.tool_call_block_indexes.remove(&openai_tool_index);
        self.open_tool_call_index = -1;
    }

    // port of emitBufferedInterleavedContent (openai_claude_response.go)
    fn emit_buffered_interleaved_content(&mut self, results: &mut Vec<String>) {
        if self.interleaved_content_chunks.is_empty() {
            return;
        }
        for chunk in std::mem::take(&mut self.interleaved_content_chunks) {
            if chunk.text.is_empty() {
                continue;
            }
            let idx = self.next_content_block_index;
            self.next_content_block_index += 1;
            let (block, delta) = match chunk.kind {
                ChunkKind::Thinking => (
                    json!({"type": "thinking", "thinking": ""}),
                    json!({"type": "thinking_delta", "thinking": chunk.text}),
                ),
                ChunkKind::Text => (
                    json!({"type": "text", "text": ""}),
                    json!({"type": "text_delta", "text": chunk.text}),
                ),
            };
            results.push(sse_frame(
                "content_block_start",
                &json!({"type": "content_block_start", "index": idx, "content_block": block}),
            ));
            results.push(sse_frame(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": idx, "delta": delta}),
            ));
            results.push(sse_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    // port of finalizeOpenAIAnthropicContentBlocks (openai_claude_response.go)
    fn finalize_content_blocks(&mut self, results: &mut Vec<String>) {
        self.stop_thinking_content_block(results);
        self.stop_text_content_block(results);

        if !self.content_blocks_stopped {
            if self.open_tool_call_index != -1 {
                self.finalize_single_tool_call(self.open_tool_call_index, results);
            }
            // toolCallAccumulatorIndexes: BTreeMap keys are already sorted.
            let indexes: Vec<i64> = self.tool_calls_accumulator.keys().copied().collect();
            for index in indexes {
                if self.tool_calls_accumulator[&index].start_emitted {
                    continue;
                }
                self.finalize_single_tool_call(index, results);
            }
            self.content_blocks_stopped = true;
            self.emit_buffered_interleaved_content(results);
        }
    }

    // port of emitAnthropicMessageDelta (openai_claude_response.go)
    fn emit_message_delta(&mut self, results: &mut Vec<String>) {
        if self.message_delta_sent {
            return;
        }
        let mut usage = Map::new();
        usage.insert("input_tokens".into(), Value::from(self.usage_input_tokens));
        usage.insert(
            "output_tokens".into(),
            Value::from(self.usage_output_tokens),
        );
        if self.usage_cached_tokens > 0 {
            usage.insert(
                "cache_read_input_tokens".into(),
                Value::from(self.usage_cached_tokens),
            );
        }
        if self.usage_cache_write_tokens > 0 {
            usage.insert(
                "cache_creation_input_tokens".into(),
                Value::from(self.usage_cache_write_tokens),
            );
        }
        let stop_reason = map_openai_finish_reason_to_anthropic(&self.terminal_finish_reason());
        let delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": Value::Object(usage)
        });
        results.push(sse_frame("message_delta", &delta));
        self.message_delta_sent = true;
    }
}

// ---------------------------------------------------------------------------
// Tests (ported from openai_claude_request_test.go, openai_claude_response_test.go,
// openai_claude_compat_test.go)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE;

    fn req(input: &str) -> Value {
        translate_request("test-model", &serde_json::from_str(input).unwrap(), false)
    }

    fn msgs(out: &Value) -> Vec<Value> {
        out["messages"].as_array().cloned().unwrap_or_default()
    }

    fn roles(out: &Value) -> Vec<String> {
        msgs(out).iter().map(|m| gstr(m.get("role"))).collect()
    }

    // ---- request ----

    #[test]
    fn request_basic_fields_exact() {
        let out = translate_request(
            "deepseek-chat",
            &json!({
                "model": "claude-x",
                "max_tokens": 1024,
                "temperature": 0.7,
                "top_p": 0.9,
                "stop_sequences": ["</a>", "</b>"],
                "system": "Be terse",
                "thinking": {"type": "enabled", "budget_tokens": 2048},
                "metadata": {"user_id": "u"},
                "user": "someone",
                "messages": [{"role": "user", "content": "hi"}]
            }),
            true,
        );
        assert_eq!(
            out,
            json!({
                "model": "deepseek-chat",
                "messages": [
                    {"role": "system", "content": [{"type": "text", "text": "Be terse"}]},
                    {"role": "user", "content": "hi"}
                ],
                "max_tokens": 1024,
                "temperature": 0.7,
                "stop": ["</a>", "</b>"],
                "stream": true,
                "reasoning_effort": "medium",
                "user": "someone"
            })
        );
    }

    #[test]
    fn request_top_p_and_integral_temperature() {
        let out = req(r#"{"top_p":0.5,"messages":[]}"#);
        assert_eq!(out["top_p"], json!(0.5));
        assert!(out.get("temperature").is_none());
        let out = req(r#"{"temperature":1,"messages":[]}"#);
        assert_eq!(out["temperature"], json!(1));
    }

    #[test]
    fn request_thinking_effort_mapping() {
        let cases = [
            (json!({"type": "enabled"}), json!(null), Some("auto")),
            (
                json!({"type": "enabled", "budget_tokens": 100}),
                json!(null),
                Some("minimal"),
            ),
            (
                json!({"type": "enabled", "budget_tokens": 30000}),
                json!(null),
                Some("xhigh"),
            ),
            (
                json!({"type": "enabled", "budget_tokens": -5}),
                json!(null),
                None,
            ),
            (json!({"type": "disabled"}), json!(null), Some("none")),
            (json!({"type": "adaptive"}), json!(null), Some("xhigh")),
            (
                json!({"type": "adaptive"}),
                json!({"effort": " HIGH "}),
                Some("high"),
            ),
        ];
        for (thinking, output_config, want) in cases {
            let mut body = json!({"thinking": thinking, "messages": []});
            if !output_config.is_null() {
                body["output_config"] = output_config;
            }
            let out = translate_request("m", &body, false);
            assert_eq!(
                out.get("reasoning_effort"),
                want.map(Value::from).as_ref(),
                "{body}"
            );
        }
    }

    #[test]
    fn thinking_to_reasoning_content_cases() {
        // (input, last non-system message expected)
        let cases: Vec<(&str, Value)> = vec![
            (
                r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"Let me analyze"},{"type":"text","text":"Here is my response."}]}]}"#,
                json!({"role":"assistant","content":[{"type":"text","text":"Here is my response."}]}),
            ),
            (
                r#"{"messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"secret"},{"type":"text","text":"Visible response."}]}]}"#,
                json!({"role":"assistant","content":[{"type":"text","text":"Visible response."}]}),
            ),
            (
                r#"{"messages":[{"role":"user","content":[{"type":"thinking","thinking":"Injected thinking"},{"type":"text","text":"User message."}]}]}"#,
                json!({"role":"user","content":[{"type":"text","text":"User message."}]}),
            ),
            (
                r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"   \n\t  "},{"type":"text","text":"Response with whitespace thinking."}]}]}"#,
                json!({"role":"assistant","content":[{"type":"text","text":"Response with whitespace thinking."}]}),
            ),
            (
                r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"First."},{"type":"thinking","thinking":"Second."},{"type":"text","text":"Final answer."}]}]}"#,
                json!({"role":"assistant","content":[{"type":"text","text":"Final answer."}]}),
            ),
        ];
        for (input, want) in cases {
            let out = req(input);
            assert_eq!(msgs(&out), vec![want], "{input}");
        }
        // Unsigned thinking-only message is dropped entirely.
        let out = req(
            r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"Internal reasoning only."}]}]}"#,
        );
        assert_eq!(out["messages"], json!([]));
        // Thinking inside system array is ignored; text kept.
        let out = req(
            r#"{"system":[{"type":"thinking","thinking":"Injected"},{"type":"text","text":"System prompt."}],"messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role":"system","content":[{"type":"text","text":"System prompt."}]},
                {"role":"user","content":[{"type":"text","text":"Hello"}]}
            ])
        );
    }

    fn valid_gpt_chat_reasoning_signature() -> String {
        let mut raw = vec![0u8; 1 + 8 + 16 + 16 + 32];
        raw[0] = 0x80;
        raw[8] = 1;
        for (i, b) in raw.iter_mut().enumerate().skip(9) {
            *b = i as u8;
        }
        URL_SAFE.encode(raw)
    }

    #[test]
    fn signed_thinking_compatibility() {
        let gpt = valid_gpt_chat_reasoning_signature();
        let cases: Vec<(String, bool)> = vec![
            (gpt.clone(), true),
            (format!("openai#{gpt}"), true),
            ("claude#EjQ=".into(), false),
            (
                "gemini#EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"
                    .into(),
                false,
            ),
            ("not-a-provider-signature".into(), false),
        ];
        for (sig, keep) in cases {
            let body = json!({"messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "provider state", "signature": sig},
                {"type": "text", "text": "visible answer"}
            ]}]});
            let out = translate_request("gpt-5", &body, false);
            let mut want = json!({"role": "assistant", "content": [{"type": "text", "text": "visible answer"}]});
            if keep {
                want["reasoning_content"] = json!("provider state");
            }
            assert_eq!(out["messages"], json!([want]), "{sig}");
        }
    }

    #[test]
    fn unsigned_thinking_only_message_dropped() {
        let out = req(r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"What is 2+2?"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"2+2=4"}]},
            {"role":"user","content":[{"type":"text","text":"Thanks"}]}]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"user","content":[{"type":"text","text":"What is 2+2?"}]},
                {"role":"user","content":[{"type":"text","text":"Thanks"}]}
            ])
        );
    }

    #[test]
    fn message_system_role_wraps_as_user_reminder() {
        let out = req(r#"{
            "system": [{"type": "text", "text": "Top-level rules"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "system", "content": "String mid-conversation rule"},
                {"role": "assistant", "content": [{"type": "text", "text": "Hi there"}]},
                {"role": "system", "content": [{"type": "text", "text": "Array mid-conversation rule"}]},
                {"role": "user", "content": [{"type": "text", "text": "Follow up"}]}
            ]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"system","content":[{"type":"text","text":"Top-level rules"}]},
                {"role":"user","content":[{"type":"text","text":"Hello"}]},
                {"role":"user","content":[{"type":"text","text":"<system-reminder>\nString mid-conversation rule\n</system-reminder>"}]},
                {"role":"assistant","content":[{"type":"text","text":"Hi there"}]},
                {"role":"user","content":[{"type":"text","text":"<system-reminder>\nArray mid-conversation rule\n</system-reminder>"}]},
                {"role":"user","content":[{"type":"text","text":"Follow up"}]}
            ])
        );
    }

    #[test]
    fn preserves_tool_adjacency_with_intervening_system_message() {
        let out = req(r#"{"messages": [
            {"role": "user", "content": [{"type": "text", "text": "Execute tools"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "tool_one", "input": {"a": 1}},
                {"type": "tool_use", "id": "call_2", "name": "tool_two", "input": {"b": 2}}]},
            {"role": "system", "content": "Context update between tool call and tool result"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_2", "content": "result 2"},
                {"type": "tool_result", "tool_use_id": "call_1", "content": "result 1"},
                {"type": "text", "text": "Now summarize"}]}]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"user","content":[{"type":"text","text":"Execute tools"}]},
                {"role":"assistant","content":"","tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"tool_one","arguments":"{\"a\":1}"}},
                    {"id":"call_2","type":"function","function":{"name":"tool_two","arguments":"{\"b\":2}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"result 1","name":"tool_one"},
                {"role":"tool","tool_call_id":"call_2","content":"result 2","name":"tool_two"},
                {"role":"user","content":[{"type":"text","text":"<system-reminder>\nContext update between tool call and tool result\n</system-reminder>"}]},
                {"role":"user","content":[{"type":"text","text":"Now summarize"}]}
            ])
        );
    }

    #[test]
    fn system_message_scenarios() {
        assert_eq!(
            roles(&req(r#"{"messages":[{"role":"user","content":"hello"}]}"#)),
            vec!["user"]
        );
        assert_eq!(
            roles(&req(
                r#"{"system":"","messages":[{"role":"user","content":"hello"}]}"#
            )),
            vec!["user"]
        );
        assert_eq!(
            req(r#"{"system":"Be helpful","messages":[{"role":"user","content":"hello"}]}"#)
                ["messages"][0],
            json!({"role":"system","content":[{"type":"text","text":"Be helpful"}]})
        );
        assert_eq!(
            req(
                r#"{"system":[{"type":"text","text":"Block 1"},{"type":"text","text":"Block 2"}],"messages":[{"role":"user","content":"hello"}]}"#
            )["messages"][0],
            json!({"role":"system","content":[{"type":"text","text":"Block 1"},{"type":"text","text":"Block 2"}]})
        );
    }

    #[test]
    fn tool_schema_adds_missing_object_properties() {
        let out = req(r#"{"tools":[
            {"name":"empty_params","description":"No args","input_schema":{"type":"object"}},
            {"name":"nested_params","description":"Nested args","input_schema":{"type":"object","properties":{
                "nested":{"type":"object"},
                "items":{"type":"array","items":{"type":"object"}}}}}],
            "messages":[{"role":"user","content":"hello"}]}"#);
        assert_eq!(
            out["tools"],
            json!([
                {"type":"function","function":{"name":"empty_params","description":"No args","parameters":{"type":"object","properties":{}}}},
                {"type":"function","function":{"name":"nested_params","description":"Nested args","parameters":{"type":"object","properties":{
                    "nested":{"type":"object","properties":{}},
                    "items":{"type":"array","items":{"type":"object","properties":{}}}}}}}
            ])
        );
    }

    #[test]
    fn tool_result_order_and_content() {
        let out = req(r#"{"messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"do_work","input":{"a":1}}]},
            {"role":"user","content":[
                {"type":"text","text":"before"},
                {"type":"tool_result","tool_use_id":"call_1","content":[{"type":"text","text":"tool ok"}]},
                {"type":"text","text":"after"}]}]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"do_work","arguments":"{\"a\":1}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"tool ok","name":"do_work"},
                {"role":"user","content":[{"type":"text","text":"before"},{"type":"text","text":"after"}]}
            ])
        );
    }

    #[test]
    fn tool_result_object_content() {
        let out = req(r#"{"messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"do_work","input":{"a":1}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":{"foo":"bar"}}]}]}"#);
        assert_eq!(
            out["messages"][1],
            json!({"role":"tool","tool_call_id":"call_1","content":"{\"foo\":\"bar\"}","name":"do_work"})
        );
        assert_eq!(msgs(&out).len(), 2);
    }

    #[test]
    fn tool_result_text_and_image_content() {
        let out = req(r#"{"messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"do_work","input":{"a":1}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"tool ok"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUg=="}}]}]}]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"do_work","arguments":"{\"a\":1}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"tool ok","name":"do_work"},
                {"role":"user","content":[
                    {"type":"text","text":TOOL_RESULT_IMAGE_RELAY_NOTICE},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="}}]}
            ])
        );
    }

    #[test]
    fn tool_result_url_image_only() {
        let out = req(r#"{"messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"do_work","input":{"a":1}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":
                {"type":"image","source":{"type":"url","url":"https://example.com/tool.png"}}}]}]}"#);
        assert_eq!(
            msgs(&out)[1..].to_vec(),
            vec![
                json!({"role":"tool","tool_call_id":"call_1","content":TOOL_RESULT_IMAGE_PLACEHOLDER,"name":"do_work"}),
                json!({"role":"user","content":[
                    {"type":"text","text":TOOL_RESULT_IMAGE_RELAY_NOTICE},
                    {"type":"image_url","image_url":{"url":"https://example.com/tool.png"}}]}),
            ]
        );
    }

    #[test]
    fn tool_result_image_merges_into_user_text() {
        let out = req(r#"{"messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"screenshot","input":{}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call_1","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUg=="}}]},
                {"type":"text","text":"What color?"}]}]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"screenshot","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":TOOL_RESULT_IMAGE_PLACEHOLDER,"name":"screenshot"},
                {"role":"user","content":[
                    {"type":"text","text":TOOL_RESULT_IMAGE_RELAY_NOTICE},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="}},
                    {"type":"text","text":"What color?"}]}
            ])
        );
    }

    #[test]
    fn multiple_tool_results_with_images() {
        let out = req(r#"{"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","id":"call_1","name":"shot1","input":{}},
                {"type":"tool_use","id":"call_2","name":"shot2","input":{}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call_1","content":[
                    {"type":"text","text":"result 1"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"img1"}}]},
                {"type":"tool_result","tool_use_id":"call_2","content":
                    {"type":"image","source":{"type":"url","url":"https://example.com/2.png"}}}]}]}"#);
        assert_eq!(
            msgs(&out)[1..].to_vec(),
            vec![
                json!({"role":"tool","tool_call_id":"call_1","content":"result 1","name":"shot1"}),
                json!({"role":"tool","tool_call_id":"call_2","content":TOOL_RESULT_IMAGE_PLACEHOLDER,"name":"shot2"}),
                json!({"role":"user","content":[
                    {"type":"text","text":TOOL_RESULT_IMAGE_RELAY_NOTICE},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,img1"}},
                    {"type":"image_url","image_url":{"url":"https://example.com/2.png"}}]}),
            ]
        );
    }

    #[test]
    fn assistant_text_tool_use_text_order_and_thinking_split() {
        let want = json!([{"role":"assistant","content":[{"type":"text","text":"pre"},{"type":"text","text":"post"}],
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"do_work","arguments":"{\"a\":1}"}}]}]);
        let out = req(r#"{"messages":[{"role":"assistant","content":[
            {"type":"text","text":"pre"},
            {"type":"tool_use","id":"call_1","name":"do_work","input":{"a":1}},
            {"type":"text","text":"post"}]}]}"#);
        assert_eq!(out["messages"], want);
        let out = req(r#"{"messages":[{"role":"assistant","content":[
            {"type":"thinking","thinking":"t1"},
            {"type":"text","text":"pre"},
            {"type":"tool_use","id":"call_1","name":"do_work","input":{"a":1}},
            {"type":"thinking","thinking":"t2"},
            {"type":"text","text":"post"}]}]}"#);
        assert_eq!(out["messages"], want);
    }

    #[test]
    fn strips_claude_code_attribution() {
        let out = req(r#"{"system":[
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.63.abc; cc_entrypoint=cli; cch=12345;"},
            {"type":"text","text":"User system prompt"}],
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#);
        assert_eq!(
            out["messages"][0],
            json!({"role":"system","content":[{"type":"text","text":"User system prompt"}]})
        );
    }

    #[test]
    fn stop_sequences() {
        assert_eq!(
            req(r#"{"stop_sequences":["</block>"],"messages":[]}"#)["stop"],
            json!(["</block>"])
        );
        assert_eq!(
            req(r#"{"stop_sequences":["stop1","stop2"],"messages":[]}"#)["stop"],
            json!(["stop1", "stop2"])
        );
        assert!(req(r#"{"stop_sequences":[],"messages":[]}"#)
            .get("stop")
            .is_none());
    }

    #[test]
    fn tool_without_input_schema_defaults_parameters() {
        // The server tool (`web_search_20250305`) is left out: a Chat
        // upstream could call it and the client has nothing to run.
        let out = req(r#"{"tools":[
            {"type":"web_search_20250305","name":"web_search","max_uses":8},
            {"name":"no_schema_custom"},
            {"name":"null_schema_custom","input_schema":null},
            {"type":"custom","name":"typed_custom","input_schema":{"type":"object"}}],
            "messages":[{"role":"user","content":"hello"}]}"#);
        let params = json!({"type":"object","properties":{}});
        assert_eq!(
            out["tools"],
            json!([
                {"type":"function","function":{"name":"no_schema_custom","description":"","parameters":params}},
                {"type":"function","function":{"name":"null_schema_custom","description":"","parameters":params}},
                {"type":"function","function":{"name":"typed_custom","description":"","parameters":params}}
            ])
        );
    }

    // An OpenAI-compatible gateway reporting a failure INSIDE a 200 stream:
    // Claude Code gets the error event with the gateway's message, and the
    // stream is not closed as a complete (empty) answer afterwards.
    #[test]
    fn stream_in_stream_error_object_becomes_error_event() {
        let mut t = StreamTranslator::new(&json!({}));
        t.push(None, &json!({"id": "c1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}));
        let out = t.push(None, &json!({"error": {"message": "Insufficient Balance", "type": "unknown_error", "param": null, "code": "invalid_request_error"}}));
        assert_eq!(
            out,
            vec!["event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"unknown_error\",\"message\":\"Insufficient Balance\"}}\n\n"]
        );
        assert!(t.finish().is_empty());
    }

    #[test]
    fn strips_unsupported_unicode_property_escape_patterns() {
        let body = json!({"messages":[{"role":"user","content":"hello"}],"tools":[{
            "name":"Artifact","description":"Render","input_schema":{"type":"object","properties":{
                "field":{"type":"string","description":"field to replace",
                    "pattern":"^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$"},
                "asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},
                "lookahead_safe":{"type":"string","pattern":"^(?!__.*__$).{1,200}$"},
                "nul_guard":{"type":"string","pattern":"^[^\\0]*$"}}}}]});
        let out = translate_request("gpt-5.6", &body, false);
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type":"object","properties":{
                "field":{"type":"string","description":"field to replace"},
                "asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},
                "lookahead_safe":{"type":"string","pattern":"^(?!__.*__$).{1,200}$"},
                "nul_guard":{"type":"string"}}})
        );
    }

    #[test]
    fn preserves_non_schema_pattern_keys() {
        let body = json!({"messages":[],"tools":[{"name":"config_tool","input_schema":{"type":"object","properties":{
            "regex_config":{"type":"object","default":{"pattern":"\\p{L}+"},"enum":[{"pattern":"\\p{N}+"}]},
            "real_schema":{"type":"string","pattern":"\\p{L}+"}}}}]});
        let out = translate_request("m", &body, false);
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type":"object","properties":{
                "regex_config":{"type":"object","default":{"pattern":"\\p{L}+"},"enum":[{"pattern":"\\p{N}+"}],"properties":{}},
                "real_schema":{"type":"string"}}})
        );
    }

    #[test]
    fn strips_pattern_properties_incompatible_keys() {
        let body = json!({"messages":[],"tools":[{"name":"pattern_tool","input_schema":{"type":"object",
            "patternProperties":{"^\\p{L}+$":{"type":"string"},"^[a-z]+$":{"type":"object"}}}}]});
        let out = translate_request("m", &body, false);
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type":"object","patternProperties":{"^[a-z]+$":{"type":"object","properties":{}}},"properties":{}})
        );
    }

    #[test]
    fn tool_result_preserves_function_name_and_unknown_id() {
        let out = req(r#"{"messages":[
            {"role":"user","content":"What's the weather and time in Jakarta?"},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"toolu_01ABC","name":"get_weather","input":{"city":"Jakarta"}},
                {"type":"tool_use","id":"toolu_02DEF","name":"get_time","input":{"city":"Jakarta"}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_01ABC","content":"32C, humid"},
                {"type":"tool_result","tool_use_id":"toolu_02DEF","content":"12:00 PM"}]}]}"#);
        assert_eq!(
            msgs(&out)[2..].to_vec(),
            vec![
                json!({"role":"tool","tool_call_id":"toolu_01ABC","content":"32C, humid","name":"get_weather"}),
                json!({"role":"tool","tool_call_id":"toolu_02DEF","content":"12:00 PM","name":"get_time"}),
            ]
        );
        let out = req(
            r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"orphan_call_1","content":"result"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role":"tool","tool_call_id":"orphan_call_1","content":"result"}])
        );
    }

    #[test]
    fn tool_call_pairing_by_id() {
        let out = req(r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"Run analysis"}]},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"call_1","name":"fetch_data","input":{"id":123}},
                {"type":"tool_use","id":"call_2","name":"calc_metric","input":{"scale":1.5}}]},
            {"role":"assistant","content":[
                {"type":"text","text":"Waiting for results to continue"},
                {"type":"thinking","thinking":"Thinking about next steps","signature":"sig_abc"}]},
            {"role":"user","content":[{"type":"text","text":"Reminder: keep timeout short"}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call_2","content":"metric_ok"},
                {"type":"tool_result","tool_use_id":"call_1","content":"data_ok"}]}]}"#);
        assert_eq!(
            out["messages"],
            json!([
                {"role":"user","content":[{"type":"text","text":"Run analysis"}]},
                {"role":"assistant","content":"","tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"fetch_data","arguments":"{\"id\":123}"}},
                    {"id":"call_2","type":"function","function":{"name":"calc_metric","arguments":"{\"scale\":1.5}"}}]},
                {"role":"tool","tool_call_id":"call_2","content":"metric_ok","name":"calc_metric"},
                {"role":"tool","tool_call_id":"call_1","content":"data_ok","name":"fetch_data"},
                {"role":"assistant","content":[{"type":"text","text":"Waiting for results to continue"}]},
                {"role":"user","content":[{"type":"text","text":"Reminder: keep timeout short"}]}
            ])
        );
    }

    #[test]
    fn tool_call_pairing_orphan_and_incomplete_preserved() {
        let out = req(r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"Start"}]},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"call_alpha","name":"do_a","input":{}},
                {"type":"tool_use","id":"call_beta","name":"do_b","input":{}}]},
            {"role":"user","content":[{"type":"text","text":"Waiting on results"}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_unmatched","content":"orphan_result"}]}]}"#);
        assert_eq!(roles(&out), vec!["user", "assistant", "user", "tool"]);
        assert_eq!(
            out["messages"][3],
            json!({"role":"tool","tool_call_id":"call_unmatched","content":"orphan_result"})
        );
    }

    #[test]
    fn tool_choice_mapping() {
        let tc = |choice: Value| {
            translate_request("m", &json!({"messages": [], "tool_choice": choice}), false)
        };
        assert_eq!(tc(json!({"type": "none"}))["tool_choice"], json!("none"));
        assert_eq!(tc(json!({"type": "auto"}))["tool_choice"], json!("auto"));
        assert_eq!(tc(json!({"type": "any"}))["tool_choice"], json!("required"));
        assert_eq!(tc(json!("any"))["tool_choice"], json!("required"));
        assert_eq!(
            tc(json!({"type": "tool", "name": "tool_a"}))["tool_choice"],
            json!({"type": "function", "function": {"name": "tool_a"}})
        );
        assert_eq!(
            tc(json!({"type": "tool", "name": ""}))["tool_choice"],
            json!("none")
        );
        assert_eq!(
            tc(json!({"type": "unknown_future_restriction"}))["tool_choice"],
            json!("none")
        );
        let out = tc(json!({"type": "auto", "disable_parallel_tool_use": true}));
        assert_eq!(out["parallel_tool_calls"], json!(false));
        let out = tc(Value::Null);
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn compat_preserves_thinking() {
        let empty_sig = json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":""}]}]});
        assert_eq!(
            translate_request("deepseek-v4", &empty_sig, false)["messages"],
            json!([])
        );
        assert_eq!(
            translate_request_with_compat("deepseek-v4", &empty_sig, false)["messages"],
            json!([{"role":"assistant","content":"","reasoning_content":"reason"}])
        );

        let with_tools = json!({"messages":[{"role":"assistant","content":[
            {"type":"thinking","thinking":"reason","signature":"claude#opaque"},
            {"type":"text","text":"Reading files."},
            {"type":"tool_use","id":"call_1","name":"Read","input":{"path":"main.go"}}]}]});
        assert_eq!(
            translate_request_with_compat("deepseek-v4", &with_tools, false)["messages"],
            json!([{"role":"assistant","content":[{"type":"text","text":"Reading files."}],
                "reasoning_content":"reason",
                "tool_calls":[{"id":"call_1","type":"function","function":{"name":"Read","arguments":"{\"path\":\"main.go\"}"}}]}])
        );

        let no_thinking = json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"Read","input":{}}]}]});
        let want = json!([{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"Read","arguments":"{}"}}]}]);
        assert_eq!(
            translate_request_with_compat("deepseek-v4", &no_thinking, false)["messages"],
            want
        );
        assert_eq!(
            translate_request("deepseek-v4", &no_thinking, false)["messages"],
            want
        );
    }

    #[test]
    fn fix_json_cases() {
        assert_eq!(fix_json("{'a': 1, 'b': '2'}"), r#"{"a": 1, "b": "2"}"#);
        assert_eq!(
            fix_json(r#"{"t": 'He said "hi"'}"#),
            r#"{"t": "He said \"hi\""}"#
        );
        assert_eq!(fix_json(r#"{"x": "it's"}"#), r#"{"x": "it's"}"#);
        assert_eq!(fix_json("{'a': 'unterminated"), r#"{"a": "unterminated""#);
    }

    // ---- response: stream ----

    fn parse_frames(frames: &[String]) -> Vec<(String, Value)> {
        frames
            .iter()
            .map(|f| {
                assert!(f.ends_with("\n\n"), "frame must end with blank line: {f:?}");
                let body = f.strip_suffix("\n\n").unwrap();
                let (ev, data) = body.split_once('\n').unwrap();
                let ev = ev.strip_prefix("event: ").unwrap().to_string();
                let data = data.strip_prefix("data: ").unwrap();
                (ev, serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    fn run_stream(original: &Value, chunks: &[&str]) -> Vec<(String, Value)> {
        let mut t = StreamTranslator::new(original);
        let mut frames = Vec::new();
        for c in chunks {
            frames.extend(t.push(None, &serde_json::from_str(c).unwrap()));
        }
        frames.extend(t.finish());
        parse_frames(&frames)
    }

    fn stream(chunks: &[&str]) -> Vec<(String, Value)> {
        run_stream(&json!({"stream": true}), chunks)
    }

    fn count(events: &[(String, Value)], ty: &str) -> usize {
        events.iter().filter(|(e, _)| e == ty).count()
    }

    fn tool_starts(events: &[(String, Value)]) -> Vec<Value> {
        events
            .iter()
            .filter(|(e, v)| e == "content_block_start" && v["content_block"]["type"] == "tool_use")
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn last_stop_reason(events: &[(String, Value)]) -> Value {
        events
            .iter()
            .rev()
            .find(|(e, _)| e == "message_delta")
            .map(|(_, v)| v["delta"]["stop_reason"].clone())
            .unwrap_or(Value::Null)
    }

    fn deltas_of(events: &[(String, Value)], ty: &str, field: &str) -> Vec<String> {
        events
            .iter()
            .filter(|(e, v)| e == "content_block_delta" && v["delta"]["type"] == ty)
            .map(|(_, v)| gstr(v["delta"].get(field)))
            .collect()
    }

    fn assert_sequential(events: &[(String, Value)]) {
        let mut active: i64 = -1;
        for (e, v) in events {
            let idx = v["index"].as_i64().unwrap_or(-2);
            match e.as_str() {
                "content_block_start" => {
                    assert_eq!(active, -1, "start while open: {events:?}");
                    active = idx;
                }
                "content_block_delta" => assert_eq!(idx, active, "{events:?}"),
                "content_block_stop" => {
                    assert_eq!(idx, active, "{events:?}");
                    active = -1;
                }
                _ => {}
            }
        }
        assert_eq!(active, -1, "unclosed block: {events:?}");
    }

    /// End-to-end: DeepSeek-style reasoning_content, then text, then a streamed
    /// tool call, finish_reason, and a trailing usage-only chunk.
    #[test]
    fn e2e_reasoning_text_tool_call_exact_frames() {
        let original = json!({"stream": true, "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]});
        let mut t = StreamTranslator::new(&original);
        let chunks = [
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":"Let me"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"reasoning_content":" think."},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"Sure."},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"GET_WEATHER","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-chat","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":5}}}"#,
        ];
        let mut frames: Vec<String> = Vec::new();
        for c in chunks {
            frames.extend(t.push(None, &serde_json::from_str(c).unwrap()));
        }
        let tail = t.finish();
        assert!(tail.is_empty(), "everything already closed: {tail:?}");

        let expected: Vec<(&str, Value)> = vec![
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"chatcmpl-1","type":"message","role":"assistant","model":"deepseek-chat","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" think."}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Sure."}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_abc","name":"get_weather","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"Paris\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":15,"output_tokens":10,"cache_read_input_tokens":5}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let got = parse_frames(&frames);
        let want: Vec<(String, Value)> = expected
            .into_iter()
            .map(|(e, v)| (e.to_string(), v))
            .collect();
        assert_eq!(got, want);
        // Byte-level shape of one frame.
        assert_eq!(
            frames.last().unwrap(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }

    #[test]
    fn late_usage_only_does_not_emit_after_message_stop() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            r#"{"id":"c1","model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ]);
        let types: Vec<&str> = ev.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            ev[4].1,
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":1}})
        );
    }

    #[test]
    fn stream_ignores_null_tool_name_delta() {
        let mut t = StreamTranslator::new(&json!({"stream": true}));
        let first = parse_frames(&t.push(None, &json!({"id":"chatcmpl_1","model":"test-model","created":1,"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]})));
        assert_eq!(
            first[1].1,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"read_file","input":{}}})
        );
        let second = t.push(None, &json!({"id":"chatcmpl_1","model":"test-model","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":null,"arguments":"{\"path\":\"/tmp/a\"}"}}]},"finish_reason":null}]}));
        assert!(second.is_empty());
    }

    #[test]
    fn tool_empty_null_or_non_string_name_gets_synthetic_name() {
        for name in [r#""""#, "null", "123"] {
            let first = format!(
                r#"{{"id":"c1","model":"m","choices":[{{"index":0,"delta":{{"role":"assistant","tool_calls":[{{"index":0,"id":"call_a","function":{{"name":{name},"arguments":""}}}}]}}}}]}}"#
            );
            let ev = stream(&[
                &first,
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"x\":1}"}}]}}]}"#,
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]);
            assert_eq!(
                tool_starts(&ev),
                vec![
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_a","name":"tool_0","input":{}}})
                ]
            );
            assert_eq!(
                deltas_of(&ev, "input_json_delta", "partial_json"),
                vec![r#"{"x":1}"#]
            );
            assert_eq!(count(&ev, "content_block_stop"), 1);
            assert_eq!(last_stop_reason(&ev), json!("tool_use"));
        }
    }

    #[test]
    fn tool_repeated_name() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"do_it","arguments":"{\"x\""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"do_it","arguments":":1}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(tool_starts(&ev).len(), 1);
        assert_eq!(tool_starts(&ev)[0]["content_block"]["name"], json!("do_it"));
        assert_eq!(count(&ev, "content_block_stop"), 1);
    }

    #[test]
    fn tool_mixed_empty_name_and_valid() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call_empty","function":{"name":"","arguments":""}},
                {"index":1,"id":"call_real","function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(
            tool_starts(&ev),
            vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_real","name":"do_it","input":{}}}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_empty","name":"tool_0","input":{}}}),
            ]
        );
        assert_eq!(count(&ev, "content_block_stop"), 2);
        assert_eq!(last_stop_reason(&ev), json!("tool_use"));
        assert_sequential(&ev);
    }

    #[test]
    fn tool_empty_name_without_signal_is_suppressed() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert!(tool_starts(&ev).is_empty());
        // Go maps a bare "tool_calls" finish with no announced tool to "stop".
        assert_eq!(last_stop_reason(&ev), json!("end_turn"));
    }

    #[test]
    fn tool_empty_id_defers_start_and_id_in_function_less_delta() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"","function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_real","function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(tool_starts(&ev).len(), 1);
        assert_eq!(
            tool_starts(&ev)[0]["content_block"]["id"],
            json!("call_real")
        );

        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"do_it"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_real"}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(
            tool_starts(&ev),
            vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_real","name":"do_it","input":{}}})
            ]
        );
        assert_eq!(count(&ev, "content_block_stop"), 1);
    }

    #[test]
    fn tool_stop_reason_with_emitted_tool_and_when_id_never_arrives() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":"do_it","arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ]);
        assert_eq!(last_stop_reason(&ev), json!("tool_use"));

        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let starts = tool_starts(&ev);
        assert_eq!(starts.len(), 1);
        assert!(gstr(starts[0]["content_block"].get("id")).starts_with("toolu_"));
        assert_eq!(starts[0]["content_block"]["name"], json!("do_it"));
        assert_eq!(last_stop_reason(&ev), json!("tool_use"));
    }

    #[test]
    fn belated_starts_use_openai_tool_index_order() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":2,"function":{"name":"third_tool","arguments":"{}"}},
                {"index":0,"function":{"name":"first_tool","arguments":"{}"}},
                {"index":1,"function":{"name":"second_tool","arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let starts = tool_starts(&ev);
        let got: Vec<(Value, Value)> = starts
            .iter()
            .map(|s| (s["content_block"]["name"].clone(), s["index"].clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (json!("first_tool"), json!(0)),
                (json!("second_tool"), json!(1)),
                (json!("third_tool"), json!(2))
            ]
        );
    }

    #[test]
    fn late_id_after_finalization_emits_nothing_after_stop() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"do_it"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_late"}]}}]}"#,
        ]);
        assert_eq!(tool_starts(&ev).len(), 1);
        let stop_at = ev.iter().position(|(e, _)| e == "message_stop").unwrap();
        assert_eq!(stop_at, ev.len() - 1);
    }

    #[test]
    fn empty_name_args_only_no_id() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"q\":\"x\"}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let starts = tool_starts(&ev);
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["content_block"]["name"], json!("tool_0"));
        assert!(gstr(starts[0]["content_block"].get("id")).starts_with("toolu_"));
        assert_eq!(last_stop_reason(&ev), json!("tool_use"));
    }

    #[test]
    fn omitted_finish_reason_emits_message_delta_on_done() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"loc\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
        ]);
        assert_eq!(count(&ev, "message_delta"), 1);
        assert_eq!(count(&ev, "message_stop"), 1);
        let n = ev.len();
        assert_eq!(
            ev[n - 2].1,
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}})
        );
        assert_eq!(ev[n - 1].0, "message_stop");

        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hello world"},"finish_reason":null}]}"#,
        ]);
        assert_eq!(count(&ev, "message_delta"), 1);
        assert_eq!(last_stop_reason(&ev), json!("end_turn"));
        assert_eq!(count(&ev, "message_stop"), 1);
    }

    #[test]
    fn usage_without_finish_reason_emits_message_delta() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"loc\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        ]);
        assert_eq!(count(&ev, "message_delta"), 1);
        let delta = ev.iter().find(|(e, _)| e == "message_delta").unwrap();
        assert_eq!(
            delta.1,
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":10,"output_tokens":5}})
        );
        assert_eq!(count(&ev, "message_stop"), 1);
    }

    #[test]
    fn per_chunk_usage_preserves_tool_arguments() {
        for final_reason in [r#""tool_calls""#, "null"] {
            let third = format!(
                r#"{{"id":"c1","model":"m","choices":[{{"index":0,"delta":{{"role":"assistant","tool_calls":[{{"index":0,"function":{{"arguments":"lop\"}}"}}}}]}},"finish_reason":{final_reason}}}],"usage":{{"prompt_tokens":191,"completion_tokens":15}}}}"#
            );
            let ev = stream(&[
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Skill","arguments":""}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":5}}"#,
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"{\"skill\": \"stop-s"}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":10}}"#,
                &third,
            ]);
            assert_eq!(
                tool_starts(&ev),
                vec![
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"Skill","input":{}}})
                ]
            );
            assert_eq!(
                deltas_of(&ev, "input_json_delta", "partial_json").concat(),
                r#"{"skill": "stop-slop"}"#
            );
            assert_eq!(count(&ev, "message_delta"), 1);
            assert_eq!(count(&ev, "message_stop"), 1);
            let delta = ev.iter().find(|(e, _)| e == "message_delta").unwrap();
            assert_eq!(
                delta.1,
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":191,"output_tokens":15}})
            );
        }
    }

    #[test]
    fn omitted_tool_call_index_preserves_parallel_calls() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"id":"call_weather","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}},
                {"id":"call_time","type":"function","function":{"name":"get_time","arguments":"{\"tz\":\"UTC\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        let ids: Vec<Value> = tool_starts(&ev)
            .iter()
            .map(|s| s["content_block"]["id"].clone())
            .collect();
        assert_eq!(ids, vec![json!("call_weather"), json!("call_time")]);
        assert_eq!(
            deltas_of(&ev, "input_json_delta", "partial_json"),
            vec![r#"{"city":"Paris"}"#, r#"{"tz":"UTC"}"#]
        );
        assert_eq!(count(&ev, "content_block_stop"), 2);
        assert_eq!(last_stop_reason(&ev), json!("tool_use"));
        assert_sequential(&ev);
    }

    const USAGE_CASES: [(&str, Value); 6] = [
        (
            r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
            Value::Null,
        ),
        (
            r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_creation_tokens":150}}"#,
            Value::Null,
        ),
        (
            r#"{"prompt_tokens":500,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":50}}"#,
            Value::Null,
        ),
        (
            r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":0}}"#,
            Value::Null,
        ),
        (
            r#"{"prompt_tokens":4022,"completion_tokens":462,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":4019}}"#,
            Value::Null,
        ),
        (
            r#"{"prompt_tokens":500,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
            Value::Null,
        ),
    ];

    fn usage_wants() -> Vec<Value> {
        vec![
            json!({"input_tokens":50,"output_tokens":200,"cache_read_input_tokens":800,"cache_creation_input_tokens":150}),
            json!({"input_tokens":50,"output_tokens":200,"cache_read_input_tokens":800,"cache_creation_input_tokens":150}),
            json!({"input_tokens":0,"output_tokens":100,"cache_read_input_tokens":800,"cache_creation_input_tokens":50}),
            json!({"input_tokens":200,"output_tokens":200,"cache_read_input_tokens":800}),
            json!({"input_tokens":3,"output_tokens":462,"cache_creation_input_tokens":4019}),
            json!({"input_tokens":0,"output_tokens":100,"cache_read_input_tokens":300,"cache_creation_input_tokens":300}),
        ]
    }

    #[test]
    fn streaming_usage_preserves_cache_write_tokens() {
        for ((usage, _), want) in USAGE_CASES.iter().zip(usage_wants()) {
            let last = format!(r#"{{"id":"c1","model":"m","choices":[],"usage":{usage}}}"#);
            let ev = stream(&[
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]}"#,
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                &last,
            ]);
            let delta = ev.iter().find(|(e, _)| e == "message_delta").unwrap();
            assert_eq!(delta.1["usage"], want, "{usage}");
        }
    }

    #[test]
    fn non_streaming_usage_preserves_cache_write_tokens() {
        let original = json!({"model":"claude-3-5-sonnet-20241022","messages":[{"role":"user","content":"Hello"}]});
        for ((usage, _), want) in USAGE_CASES.iter().zip(usage_wants()) {
            let raw: Value = serde_json::from_str(&format!(
                r#"{{"id":"chatcmpl-123","object":"chat.completion","created":1677652288,"model":"gpt-5.4",
                "choices":[{{"index":0,"message":{{"role":"assistant","content":"Hello world"}},"finish_reason":"stop"}}],"usage":{usage}}}"#
            ))
            .unwrap();
            let out = translate_non_stream(&raw, &original);
            assert_eq!(
                out,
                json!({"id":"chatcmpl-123","type":"message","role":"assistant","model":"gpt-5.4",
                    "content":[{"type":"text","text":"Hello world"}],"stop_reason":"end_turn","stop_sequence":null,"usage":want}),
                "{usage}"
            );
        }
    }

    #[test]
    fn interleaved_content_and_tool_use_strict_sequential_blocks() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"\n"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_sequential(&ev);
        assert_eq!(
            deltas_of(&ev, "input_json_delta", "partial_json"),
            vec![r#"{"command":"ls"}"#]
        );
        assert_eq!(deltas_of(&ev, "text_delta", "text"), vec!["\n"]);
    }

    #[test]
    fn parallel_tool_calls_strict_sequential_blocks() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}},
                {"index":1,"id":"call_2","type":"function","function":{"name":"Read","arguments":"{\"path\":\"/tmp\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_sequential(&ev);
        assert_eq!(
            tool_starts(&ev),
            vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"Bash","input":{}}}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_2","name":"Read","input":{}}}),
            ]
        );
        assert_eq!(
            deltas_of(&ev, "input_json_delta", "partial_json"),
            vec![r#"{"command":"ls"}"#, r#"{"path":"/tmp"}"#]
        );
    }

    #[test]
    fn interleaved_text_and_thinking_preserves_order() {
        let ev = stream(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Note A: "},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"running check"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"Thinking about safety"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Note B: done"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_sequential(&ev);
        let starts: Vec<Value> = ev
            .iter()
            .filter(|(e, _)| e == "content_block_start")
            .map(|(_, v)| v["content_block"]["type"].clone())
            .collect();
        assert_eq!(
            starts,
            vec![
                json!("tool_use"),
                json!("text"),
                json!("thinking"),
                json!("text")
            ]
        );
        assert_eq!(
            deltas_of(&ev, "input_json_delta", "partial_json"),
            vec![r#"{"command":"pwd"}"#]
        );
        assert_eq!(
            deltas_of(&ev, "text_delta", "text"),
            vec!["Note A: running check", "Note B: done"]
        );
        assert_eq!(
            deltas_of(&ev, "thinking_delta", "thinking"),
            vec!["Thinking about safety"]
        );
    }

    #[test]
    fn stop_reason_truncation_matrix() {
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\",\"content\":\"hello"}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":400}}"#,
                ],
                "max_tokens",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\",\"content\":\"hello"}}]},"finish_reason":null}]}"#,
                ],
                "max_tokens",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\"}"}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#,
                ],
                "tool_use",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\""}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#,
                ],
                "max_tokens",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_time","arguments":""}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":10}}"#,
                ],
                "tool_use",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":" \n\t"}}]},"finish_reason":null}]}"#,
                ],
                "max_tokens",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\"}"}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#,
                ],
                "end_turn",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/a\"}"}},{"index":1,"id":"call_2","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/b"}}]},"finish_reason":null}]}"#,
                ],
                "max_tokens",
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/a\","}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"\"content\":\"incompl"}}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":400}}"#,
                ],
                "max_tokens",
            ),
        ];
        for (chunks, want) in cases {
            let ev = stream(&chunks);
            assert_eq!(last_stop_reason(&ev), json!(want), "{chunks:?}");
        }
    }

    #[test]
    fn reasoning_field_variants_emit_thinking_delta() {
        let cases: Vec<(Vec<&str>, Vec<&str>)> = vec![
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning":"I am thinking","reasoning_details":[{"type":"reasoning.text","text":"I am thinking"}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                ],
                vec!["I am thinking"],
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"primary reasoning","reasoning":"fallback reasoning"},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                ],
                vec!["primary reasoning"],
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"Only details thinking"}]},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Answer"},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                ],
                vec!["Only details thinking"],
            ),
            (
                vec![
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"","reasoning":"fallback from empty"},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":null,"reasoning":"fallback from null"},"finish_reason":null}]}"#,
                    r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                ],
                vec!["fallback from empty", "fallback from null"],
            ),
        ];
        for (chunks, want) in cases {
            let ev = stream(&chunks);
            assert_eq!(
                deltas_of(&ev, "thinking_delta", "thinking"),
                want,
                "{chunks:?}"
            );
        }
    }

    #[test]
    fn finish_without_any_chunk_still_closes() {
        let mut t = StreamTranslator::new(&json!({"stream": true}));
        let ev = parse_frames(&t.finish());
        assert_eq!(
            ev,
            vec![
                (
                    "message_delta".to_string(),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}})
                ),
                ("message_stop".to_string(), json!({"type":"message_stop"})),
            ]
        );
        assert!(t.finish().is_empty());
    }

    // ---- response: non-stream ----

    #[test]
    fn non_stream_reasoning_field_emits_thinking_block() {
        let raw = json!({"id":"chatcmpl-1","object":"chat.completion","model":"deepseek","choices":[{"index":0,"message":{"role":"assistant","content":"Done","reasoning":"Thought process"},"finish_reason":"stop"}]});
        assert_eq!(
            translate_non_stream(&raw, &Value::Null),
            json!({"id":"chatcmpl-1","type":"message","role":"assistant","model":"deepseek",
                "content":[{"type":"text","text":"Done"},{"type":"thinking","thinking":"Thought process"}],
                "stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}})
        );
        // convertOpenAINonStreamingToAnthropic orders thinking first.
        assert_eq!(
            convert_openai_non_streaming_to_anthropic(&raw),
            json!({"id":"chatcmpl-1","type":"message","role":"assistant","model":"deepseek",
                "content":[{"type":"thinking","thinking":"Thought process"},{"type":"text","text":"Done"}],
                "stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}})
        );
    }

    #[test]
    fn non_stream_tool_calls_mapped_names_and_bad_args() {
        let original = json!({"tools": [{"name": "Read"}]});
        let raw = json!({"id":"x","model":"deepseek-chat","choices":[{"index":0,"message":{"role":"assistant","content":null,
            "reasoning_content":"plan","tool_calls":[
                {"id":"call.1","type":"function","function":{"name":"read","arguments":"{'path': 'a.go'}"}},
                {"id":"call_2","type":"function","function":{"name":"other","arguments":"[1]"}},
                {"id":"","type":"function","function":{"name":"other","arguments":"not json"}}]},
            "finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3}});
        let out = translate_non_stream(&raw, &original);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type":"thinking","thinking":"plan"}));
        assert_eq!(
            content[1],
            json!({"type":"tool_use","id":"call_1","name":"Read","input":{"path":"a.go"}})
        );
        assert_eq!(
            content[2],
            json!({"type":"tool_use","id":"call_2","name":"other","input":{}})
        );
        assert_eq!(content[3]["input"], json!({}));
        assert!(gstr(content[3].get("id")).starts_with("toolu_"));
        assert_eq!(out["stop_reason"], json!("tool_use"));
        assert_eq!(out["usage"], json!({"input_tokens":7,"output_tokens":3}));
    }

    #[test]
    fn non_stream_array_content_and_missing_finish_reason() {
        let raw = json!({"id":"x","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":[
            {"type":"reasoning","text":"r1"},{"type":"reasoning","text":"r2"},
            {"type":"text","text":"a"},{"type":"text","text":"b"},
            {"type":"tool_calls","tool_calls":[{"id":"t1","function":{"name":"f","arguments":"{}"}}]}]}}]});
        assert_eq!(
            translate_non_stream(&raw, &Value::Null),
            json!({"id":"x","type":"message","role":"assistant","model":"m","content":[
                {"type":"thinking","thinking":"r1r2"},
                {"type":"text","text":"ab"},
                {"type":"tool_use","id":"t1","name":"f","input":{}}],
                "stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}})
        );
    }

    #[test]
    fn extract_openai_usage_cases() {
        type Usage4 = (i64, i64, i64, i64);
        let cases: Vec<(Option<Value>, Usage4)> = vec![
            (None, (0, 0, 0, 0)),
            (Some(Value::Null), (0, 0, 0, 0)),
            (
                Some(json!({"prompt_tokens":100,"completion_tokens":50})),
                (100, 50, 0, 0),
            ),
            (
                Some(
                    json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":30}}),
                ),
                (70, 50, 30, 0),
            ),
            (
                Some(
                    json!({"prompt_tokens":4022,"completion_tokens":462,"prompt_tokens_details":{"cache_write_tokens":4019}}),
                ),
                (3, 462, 0, 4019),
            ),
            (
                Some(
                    json!({"prompt_tokens":4022,"completion_tokens":462,"prompt_tokens_details":{"cache_creation_tokens":4019}}),
                ),
                (3, 462, 0, 4019),
            ),
            (
                Some(
                    json!({"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}),
                ),
                (50, 200, 800, 150),
            ),
            (
                Some(
                    json!({"prompt_tokens":500,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}),
                ),
                (0, 100, 300, 300),
            ),
            (
                Some(
                    json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":-10,"cache_write_tokens":-5}}),
                ),
                (100, 50, -10, 0),
            ),
            (
                Some(json!({"prompt_tokens":-10,"completion_tokens":50})),
                (0, 50, 0, 0),
            ),
            (
                Some(
                    json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cache_write_tokens":-1,"cache_creation_tokens":40}}),
                ),
                (60, 50, 0, 40),
            ),
            (
                Some(
                    json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":9223372036854775800_i64,"cache_write_tokens":100}}),
                ),
                (0, 50, 9223372036854775800, 100),
            ),
        ];
        for (usage, want) in cases {
            assert_eq!(extract_openai_usage(usage.as_ref()), want, "{usage:?}");
        }
    }
}
