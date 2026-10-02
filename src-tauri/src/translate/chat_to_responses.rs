//! OpenAI Chat Completions client ⇄ OpenAI Responses upstream (the ChatGPT
//! Codex backend).
//!
//! A faithful port of CLIProxyAPI's `internal/translator/codex/openai/chat-completions`
//! pair at commit `ed980be` (`codex_openai_request.go` + `codex_openai_response.go`,
//! registered in that package's `init.go` as `ConvertOpenAIRequestToCodex` /
//! `ConvertCodexResponseToOpenAI` / `ConvertCodexResponseToOpenAINonStream`).
//! Each ported function carries a `// port of <GoFunc> (<file>)` comment.
//!
//! The Go code reads with gjson and writes with sjson; the helpers at the bottom
//! (`get` / `gstr` / `gint`) reproduce the gjson semantics the ported code relies
//! on (`Exists()` is true for an explicit JSON `null`, `String()` renders
//! non-strings as their JSON text, `Int()` coerces). sjson's "set a key on an
//! existing object keeps its position, a new key appends" matches serde_json's
//! `preserve_order` map, so the output key order matches the Go output.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

// ───────────────────────────── request ─────────────────────────────

struct PendingToolCall {
    call_id: String,
    source_call_id: String,
    call_type: &'static str,
    consumed: bool,
}

/// Client request (Chat Completions body) → upstream request (standard OpenAI
/// Responses body). `model` = upstream model id for the body. `stream` =
/// whether the client asked to stream.
// port of ConvertOpenAIRequestToCodex (codex_openai_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    let tools = body.get("tools");
    let tool_results: Vec<&Value> = match tools {
        Some(Value::Array(a)) => a.iter().collect(),
        _ => Vec::new(),
    };

    // Start with empty JSON object
    let mut out = Map::new();
    out.insert("instructions".into(), json!(""));
    out.insert("stream".into(), json!(stream));
    // Codex does not support temperature / top_p / top_k / max_output_tokens;
    // the Go source leaves those mappings commented out.

    // Map reasoning effort
    let effort = match body.get("reasoning_effort") {
        Some(v) => v.clone(),
        None => json!("medium"),
    };
    out.insert("reasoning".into(), json!({ "effort": effort }));
    out.insert("parallel_tool_calls".into(), json!(true));
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    out.insert("model".into(), json!(model));

    // Build request-local tool metadata and name shortening map.
    let mut custom_tool_names: HashSet<String> = HashSet::new();
    let mut original_tool_name_map: HashMap<String, String> = HashMap::new();
    {
        if !tool_results.is_empty() {
            let mut function_tool_names: HashSet<String> = HashSet::new();
            for tool in &tool_results {
                match gstr(get(tool, "type")).as_str() {
                    "function" => {
                        function_tool_names.insert(gstr(get(tool, "function.name")));
                    }
                    "custom" => {
                        custom_tool_names.insert(gstr(get(tool, "name")));
                    }
                    _ => {}
                }
            }
            // A normalized function envelope cannot disambiguate declarations that
            // share a name. Preserve function behavior for such ambiguous names.
            for name in &function_tool_names {
                custom_tool_names.remove(name);
            }
        }
        let all_names = collect_request_tool_names(body);
        if !all_names.is_empty() {
            original_tool_name_map = build_short_name_map(&all_names);
        }
    }
    let short_name = |name: &str| -> String {
        match original_tool_name_map.get(name) {
            Some(short) => short.clone(),
            None => shorten_name_if_needed(name),
        }
    };

    let resolve_tool_call = |tool_call: &Value| -> Option<(&'static str, String, String)> {
        match gstr(get(tool_call, "type")).as_str() {
            "custom" => Some((
                "custom",
                gstr(get(tool_call, "custom.name")),
                gstr(get(tool_call, "custom.input")),
            )),
            "function" => {
                let name = gstr(get(tool_call, "function.name"));
                let call_type = if custom_tool_names.contains(&name) {
                    "custom"
                } else {
                    "function"
                };
                Some((call_type, name, gstr(get(tool_call, "function.arguments"))))
            }
            _ => None,
        }
    };

    // Build input from messages, handling all message types including tool calls
    let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut ambiguous_tool_call_ids: HashSet<String> = HashSet::new();
    let mut input_items: Vec<Value> = Vec::new();
    if let Some(Value::Array(arr)) = body.get("messages") {
        for (i, m) in arr.iter().enumerate() {
            let role = gstr(get(m, "role"));

            if role == "tool" {
                // Handle tool response messages as top-level tool call output objects.
                let tool_call_id = gstr(get(m, "tool_call_id"));
                if !tool_call_id.is_empty() && ambiguous_tool_call_ids.contains(&tool_call_id) {
                    continue;
                }

                let pending_index = pending_tool_calls.iter().position(|p| {
                    !p.consumed
                        && (tool_call_id.is_empty()
                            || p.source_call_id == tool_call_id
                            || p.call_id == tool_call_id)
                });
                let Some(pending_index) = pending_index else {
                    continue;
                };
                let pending_call = &mut pending_tool_calls[pending_index];
                pending_call.consumed = true;
                let output_type = if pending_call.call_type == "custom" {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };

                let mut tool_output = Map::new();
                tool_output.insert("type".into(), json!(output_type));
                tool_output.insert("call_id".into(), json!(pending_call.call_id));
                set_tool_call_output_content(&mut tool_output, m.get("content"));
                input_items.push(Value::Object(tool_output));
                continue;
            }

            // A new conversational message starts a new tool-call batch.
            pending_tool_calls.clear();
            ambiguous_tool_call_ids.clear();

            // Handle regular messages
            let mut msg = Map::new();
            msg.insert("type".into(), json!("message"));
            if role == "system" {
                msg.insert("role".into(), json!("developer"));
            } else {
                msg.insert("role".into(), json!(role));
            }

            let text_part_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            let mut content_items: Vec<Value> = Vec::new();
            match m.get("content") {
                Some(Value::String(s)) if !s.is_empty() => {
                    // Single string content
                    content_items.push(json!({ "type": text_part_type, "text": s }));
                }
                Some(Value::Array(items)) => {
                    for it in items {
                        match gstr(get(it, "type")).as_str() {
                            "text" => {
                                content_items.push(json!({
                                    "type": text_part_type,
                                    "text": gstr(get(it, "text")),
                                }));
                            }
                            // Map image inputs to input_image for Responses API
                            "image_url" if role == "user" => {
                                let mut part = Map::new();
                                part.insert("type".into(), json!("input_image"));
                                if let Some(u) = get(it, "image_url.url") {
                                    part.insert("image_url".into(), json!(gstr(Some(u))));
                                }
                                content_items.push(Value::Object(part));
                            }
                            "file" if role == "user" => {
                                let file_data = gstr(get(it, "file.file_data"));
                                let filename = gstr(get(it, "file.filename"));
                                if !file_data.is_empty() {
                                    let mut part = Map::new();
                                    part.insert("type".into(), json!("input_file"));
                                    part.insert("file_data".into(), json!(file_data));
                                    if !filename.is_empty() {
                                        part.insert("filename".into(), json!(filename));
                                    }
                                    content_items.push(Value::Object(part));
                                }
                            }
                            "input_audio" if role == "user" => {
                                let audio_data = gstr(get(it, "input_audio.data"));
                                let audio_format = gstr(get(it, "input_audio.format"));
                                if !audio_data.is_empty() {
                                    let mut part = Map::new();
                                    part.insert("type".into(), json!("input_audio"));
                                    part.insert("data".into(), json!(audio_data));
                                    if !audio_format.is_empty() {
                                        part.insert("format".into(), json!(audio_format));
                                    }
                                    content_items.push(Value::Object(part));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }

            // Don't emit empty assistant messages when only tool_calls are present —
            // Responses API needs function_call items directly, otherwise call_id
            // matching fails (#2132).
            if role != "assistant" || !content_items.is_empty() {
                msg.insert("content".into(), Value::Array(content_items));
                input_items.push(Value::Object(msg));
            }

            // Handle tool calls for assistant messages as separate top-level objects
            if role != "assistant" {
                continue;
            }
            let Some(Value::Array(tool_calls_arr)) = m.get("tool_calls") else {
                continue;
            };
            let mut call_id_counts: HashMap<String, usize> = HashMap::new();
            let mut used_call_ids: HashSet<String> = HashSet::new();
            for tc in tool_calls_arr {
                let call_id = gstr(get(tc, "id"));
                if resolve_tool_call(tc).is_some() && !call_id.is_empty() {
                    *call_id_counts.entry(call_id.clone()).or_default() += 1;
                    used_call_ids.insert(call_id);
                }
            }
            for (call_id, count) in &call_id_counts {
                if *count > 1 {
                    ambiguous_tool_call_ids.insert(call_id.clone());
                }
            }

            for (j, tc) in tool_calls_arr.iter().enumerate() {
                let Some((tool_call_type, tool_call_name, tool_call_input)) = resolve_tool_call(tc)
                else {
                    continue;
                };
                let source_call_id = gstr(get(tc, "id"));
                if !source_call_id.is_empty() && ambiguous_tool_call_ids.contains(&source_call_id) {
                    continue;
                }
                let mut call_id = source_call_id.clone();
                if call_id.is_empty() {
                    let base_call_id = format!("call_missing_{i}_{j}");
                    call_id = base_call_id.clone();
                    let mut suffix = 1;
                    while used_call_ids.contains(&call_id) {
                        call_id = format!("{base_call_id}_{suffix}");
                        suffix += 1;
                    }
                    used_call_ids.insert(call_id.clone());
                }
                pending_tool_calls.push(PendingToolCall {
                    call_id: call_id.clone(),
                    source_call_id,
                    call_type: tool_call_type,
                    consumed: false,
                });

                let name = short_name(&tool_call_name);
                if tool_call_type == "function" {
                    // Create function_call as top-level object
                    input_items.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": tool_call_input,
                    }));
                } else {
                    input_items.push(json!({
                        "type": "custom_tool_call",
                        "call_id": call_id,
                        "name": name,
                        "input": tool_call_input,
                    }));
                }
            }
        }
    }
    out.insert("input".into(), Value::Array(input_items));

    // Map response_format and text settings to Responses API text.format
    let text = body.get("text");
    if let Some(rf) = body.get("response_format") {
        // Always create text object when response_format provided
        let mut text_out = Map::new();
        match gstr(get(rf, "type")).as_str() {
            "text" => {
                text_out.insert("format".into(), json!({ "type": "text" }));
            }
            "json_schema" => {
                if let Some(js) = get(rf, "json_schema") {
                    let mut format = Map::new();
                    format.insert("type".into(), json!("json_schema"));
                    if let Some(v) = get(js, "name") {
                        format.insert("name".into(), v.clone());
                    }
                    if let Some(v) = get(js, "strict") {
                        format.insert("strict".into(), v.clone());
                    }
                    if let Some(v) = get(js, "schema") {
                        format.insert("schema".into(), v.clone());
                    }
                    text_out.insert("format".into(), Value::Object(format));
                }
            }
            _ => {}
        }
        // Map verbosity if provided
        if let Some(v) = text.and_then(|t| get(t, "verbosity")) {
            text_out.insert("verbosity".into(), v.clone());
        }
        out.insert("text".into(), Value::Object(text_out));
    } else if let Some(v) = text.and_then(|t| get(t, "verbosity")) {
        // If only text.verbosity present (no response_format), map verbosity
        out.insert("text".into(), json!({ "verbosity": v.clone() }));
    }

    // Map tools (flatten function fields)
    if !tool_results.is_empty() {
        let mut tool_items: Vec<Value> = Vec::with_capacity(tool_results.len());
        for t in &tool_results {
            let tool_type = gstr(get(t, "type"));
            if tool_type == "custom" {
                let mut item = (*t).clone();
                let name = short_name(&gstr(get(t, "name")));
                if let Value::Object(map) = &mut item {
                    map.insert("name".into(), json!(name));
                }
                tool_items.push(item);
                continue;
            }

            // Pass through built-in tools (e.g. {"type":"web_search"}) directly for the
            // Responses API. Only function and custom tools need structural conversion.
            if !tool_type.is_empty() && tool_type != "function" && t.is_object() {
                tool_items.push((*t).clone());
                continue;
            }

            if tool_type == "function" {
                let mut item = Map::new();
                item.insert("type".into(), json!("function"));
                if let Some(func) = get(t, "function") {
                    if let Some(v) = get(func, "name") {
                        item.insert("name".into(), json!(short_name(&gstr(Some(v)))));
                    }
                    if let Some(v) = get(func, "description") {
                        item.insert("description".into(), v.clone());
                    }
                    if let Some(v) = get(func, "parameters") {
                        item.insert("parameters".into(), v.clone());
                    }
                    match get(func, "strict") {
                        Some(v) => {
                            item.insert("strict".into(), v.clone());
                        }
                        None => {
                            // Chat Completions defaults strict to false while the Responses
                            // API defaults it to true, so an omitted value must be forwarded
                            // explicitly.
                            item.insert("strict".into(), json!(false));
                        }
                    }
                }
                tool_items.push(Value::Object(item));
            }
        }
        out.insert("tools".into(), Value::Array(tool_items));
    }

    // Map tool_choice when present.
    // Chat Completions: a string ("auto"/"none") or an object
    // ({"type":"function","function":{"name":"..."}}). Responses API: keep built-in
    // tool choices as-is and flatten named choices to {"type":"...","name":"..."}.
    match body.get("tool_choice") {
        Some(Value::String(s)) => {
            out.insert("tool_choice".into(), json!(s));
        }
        Some(tc @ Value::Object(_)) => {
            let mut tc_type = gstr(get(tc, "type"));
            if tc_type == "function" || tc_type == "custom" {
                let mut name = gstr(get(tc, "name"));
                if tc_type == "function" {
                    name = gstr(get(tc, "function.name"));
                    if custom_tool_names.contains(&name) {
                        tc_type = "custom".into();
                    }
                }
                if !name.is_empty() {
                    name = short_name(&name);
                }
                let mut choice = Map::new();
                choice.insert("type".into(), json!(tc_type));
                if !name.is_empty() {
                    choice.insert("name".into(), json!(name));
                }
                out.insert("tool_choice".into(), Value::Object(choice));
            } else if !tc_type.is_empty() {
                // Built-in tool choices (e.g. {"type":"web_search"}) are already
                // Responses-compatible.
                out.insert("tool_choice".into(), tc.clone());
            }
        }
        _ => {}
    }

    out.insert("store".into(), json!(false));
    Value::Object(out)
}

// port of setToolCallOutputContent (codex_openai_request.go)
fn set_tool_call_output_content(func_output: &mut Map<String, Value>, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => {
            if let Ok(structured) = serde_json::from_str::<Value>(s) {
                if has_tool_output_image_part(&structured) {
                    return set_tool_call_output_content(func_output, Some(&structured));
                }
            }
            func_output.insert("output".into(), json!(s));
        }
        Some(Value::Array(items)) => {
            let parts: Vec<Value> = items.iter().map(tool_output_content_part).collect();
            func_output.insert("output".into(), Value::Array(parts));
        }
        // gjson: `content.Raw`, else `content.String()` ("" when absent).
        Some(other) => {
            func_output.insert("output".into(), json!(other.to_string()));
        }
        None => {
            func_output.insert("output".into(), json!(""));
        }
    }
}

// port of toolOutputContentPart (codex_openai_request.go)
fn tool_output_content_part(item: &Value) -> Value {
    let item_type = gstr(get(item, "type"));
    match item_type.as_str() {
        "text" | "input_text" | "output_text" => {
            json!({ "type": "input_text", "text": gstr(get(item, "text")) })
        }
        "image_url" | "input_image" => {
            let is_input_image = item_type == "input_image";
            let (image_url, file_id, detail) = if is_input_image {
                (
                    gstr(get(item, "image_url")),
                    gstr(get(item, "file_id")),
                    gstr(get(item, "detail")),
                )
            } else {
                (
                    gstr(get(item, "image_url.url")),
                    gstr(get(item, "image_url.file_id")),
                    gstr(get(item, "image_url.detail")),
                )
            };
            if image_url.is_empty() && file_id.is_empty() {
                return tool_output_fallback_part(item);
            }
            let mut part = Map::new();
            part.insert("type".into(), json!("input_image"));
            if !image_url.is_empty() {
                part.insert("image_url".into(), json!(image_url));
            }
            if !file_id.is_empty() {
                part.insert("file_id".into(), json!(file_id));
            }
            if !detail.is_empty() {
                part.insert("detail".into(), json!(detail));
            }
            Value::Object(part)
        }
        "file" => {
            let file_id = gstr(get(item, "file.file_id"));
            let file_data = gstr(get(item, "file.file_data"));
            let file_url = gstr(get(item, "file.file_url"));
            if file_id.is_empty() && file_data.is_empty() && file_url.is_empty() {
                return tool_output_fallback_part(item);
            }
            let mut part = Map::new();
            part.insert("type".into(), json!("input_file"));
            if !file_id.is_empty() {
                part.insert("file_id".into(), json!(file_id));
            }
            if !file_data.is_empty() {
                part.insert("file_data".into(), json!(file_data));
            }
            if !file_url.is_empty() {
                part.insert("file_url".into(), json!(file_url));
            }
            let filename = gstr(get(item, "file.filename"));
            if !filename.is_empty() {
                part.insert("filename".into(), json!(filename));
            }
            Value::Object(part)
        }
        _ => tool_output_fallback_part(item),
    }
}

// port of hasToolOutputImagePart (codex_openai_request.go)
fn has_tool_output_image_part(content: &Value) -> bool {
    let Value::Array(items) = content else {
        return false;
    };
    items
        .iter()
        .any(|item| match gstr(get(item, "type")).as_str() {
            "image_url" => {
                !gstr(get(item, "image_url.url")).is_empty()
                    || !gstr(get(item, "image_url.file_id")).is_empty()
            }
            "input_image" => {
                !gstr(get(item, "image_url")).is_empty() || !gstr(get(item, "file_id")).is_empty()
            }
            _ => false,
        })
}

// port of toolOutputFallbackPart (codex_openai_request.go)
fn tool_output_fallback_part(item: &Value) -> Value {
    json!({ "type": "input_text", "text": item.to_string() })
}

// port of sanitizeToolName (codex_openai_request.go)
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|r| {
            if r.is_ascii_alphanumeric() || r == '_' || r == '-' {
                r
            } else {
                '_'
            }
        })
        .collect()
}

// port of shortenNameIfNeeded (codex_openai_request.go)
// After sanitizing the name is pure ASCII, so byte slicing is char-safe.
fn shorten_name_if_needed(name: &str) -> String {
    const LIMIT: usize = 64;
    let sanitized = sanitize_tool_name(name);
    if sanitized.len() <= LIMIT {
        return sanitized;
    }
    if sanitized.starts_with("mcp__") {
        // Keep prefix and last segment after '__'
        if let Some(idx) = sanitized.rfind("__") {
            if idx > 0 {
                let candidate = format!("mcp__{}", &sanitized[idx + 2..]);
                if candidate.len() > LIMIT {
                    return candidate[..LIMIT].to_string();
                }
                return candidate;
            }
        }
    }
    sanitized[..LIMIT].to_string()
}

// port of collectRequestToolNames (codex_openai_request.go)
fn collect_request_tool_names(raw: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add_name = |name: String| {
        if !name.is_empty() && seen.insert(name.clone()) {
            names.push(name);
        }
    };

    // 1. tools declarations
    if let Some(Value::Array(tools)) = raw.get("tools") {
        for tool in tools {
            match gstr(get(tool, "type")).as_str() {
                "function" => add_name(gstr(get(tool, "function.name"))),
                "custom" => add_name(gstr(get(tool, "name"))),
                _ => {}
            }
        }
    }

    // 2. tool_choice
    if let Some(tc @ Value::Object(_)) = raw.get("tool_choice") {
        match gstr(get(tc, "type")).as_str() {
            "function" => {
                let mut fn_name = gstr(get(tc, "function.name"));
                if fn_name.is_empty() {
                    fn_name = gstr(get(tc, "name"));
                }
                add_name(fn_name);
            }
            "custom" => add_name(gstr(get(tc, "name"))),
            _ => {}
        }
    }

    // 3. assistant tool_calls in messages
    if let Some(Value::Array(messages)) = raw.get("messages") {
        for msg in messages {
            if gstr(get(msg, "role")) != "assistant" {
                continue;
            }
            if let Some(Value::Array(tool_calls)) = msg.get("tool_calls") {
                for tc in tool_calls {
                    let fn_name = gstr(get(tc, "function.name"));
                    if !fn_name.is_empty() {
                        add_name(fn_name);
                    } else {
                        add_name(gstr(get(tc, "custom.name")));
                    }
                }
            }
        }
    }

    names
}

// port of buildShortNameMap (codex_openai_request.go)
fn build_short_name_map(names: &[String]) -> HashMap<String, String> {
    const LIMIT: usize = 64;
    let mut used: HashSet<String> = HashSet::new();
    let mut m: HashMap<String, String> = HashMap::new();

    let make_unique = |cand: String, used: &HashSet<String>| -> String {
        if !used.contains(&cand) {
            return cand;
        }
        let mut i = 1;
        loop {
            let suffix = format!("_{i}");
            let allowed = LIMIT.saturating_sub(suffix.len());
            let mut tmp = cand.clone();
            if tmp.len() > allowed {
                tmp.truncate(allowed);
            }
            tmp.push_str(&suffix);
            if !used.contains(&tmp) {
                return tmp;
            }
            i += 1;
        }
    };

    for n in names {
        let uniq = make_unique(shorten_name_if_needed(n), &used);
        used.insert(uniq.clone());
        m.insert(n.clone(), uniq);
    }
    m
}

// ───────────────────────────── response ─────────────────────────────

// port of toolCallStreamState (codex_openai_response.go)
struct ToolCallStreamState {
    index: i64,
    arguments_emitted: bool,
    done: bool,
}

/// Per-stream state for the upstream Responses SSE → client Chat Completions
/// chunk translation.
// port of ConvertCliToOpenAIParams (codex_openai_response.go)
pub struct StreamTranslator {
    /// The model name the Go translator is called with (the client's request model).
    request_model: String,
    /// shortened → original tool names, from the client's original request.
    reverse_names: HashMap<String, String>,
    service_tier: String,
    response_id: String,
    created_at: i64,
    model: String,
    function_call_index: i64,
    /// Go keeps `map[string]*toolCallStreamState`; here the map points into `states`.
    states: Vec<ToolCallStreamState>,
    state_keys: HashMap<String, usize>,
    current_tool_call: Option<usize>,
    last_image_hash_by_item_id: HashMap<String, [u8; 32]>,
    done_sent: bool,
}

impl StreamTranslator {
    /// `original_request` = the client's ORIGINAL request body.
    pub fn new(original_request: &Value) -> Self {
        let request_model = gstr(original_request.get("model"));
        StreamTranslator {
            model: request_model.clone(),
            request_model,
            reverse_names: build_reverse_map_from_original_openai(original_request),
            service_tier: String::new(),
            response_id: String::new(),
            created_at: 0,
            function_call_index: -1,
            states: Vec::new(),
            state_keys: HashMap::new(),
            current_tool_call: None,
            last_image_hash_by_item_id: HashMap::new(),
            done_sent: false,
        }
    }

    /// One upstream Responses SSE event: `event` = its `event:` field if present,
    /// `data` = parsed JSON of its `data:` payload. Returns zero or more COMPLETE
    /// client SSE frames, each ending in "\n\n" (Chat: "data: <chunk json>\n\n").
    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert(event, data)
            .into_iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect()
    }

    /// Upstream stream ended. Returns trailing client frames; ends with
    /// "data: [DONE]\n\n" exactly once.
    // The Go translator emits nothing at stream end; the [DONE] sentinel is
    // written by the chat-completions handler (sdk/api/handlers/openai/openai_handlers.go).
    pub fn finish(&mut self) -> Vec<String> {
        if self.done_sent {
            return Vec::new();
        }
        self.done_sent = true;
        vec!["data: [DONE]\n\n".to_string()]
    }

    fn restore_name(&self, name: String) -> String {
        match self.reverse_names.get(&name) {
            Some(orig) => orig.clone(),
            None => name,
        }
    }

    // port of ConvertCodexResponseToOpenAI (codex_openai_response.go)
    fn convert(&mut self, event: Option<&str>, root: &Value) -> Vec<Value> {
        // Initialize the OpenAI SSE template.
        let mut template = json!({
            "id": "",
            "object": "chat.completion.chunk",
            "created": 12345,
            "model": "model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": null, "native_finish_reason": null}],
        });

        let tier = root
            .get("response")
            .map(codex_response_service_tier)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| codex_response_service_tier(root));
        if !tier.is_empty() {
            self.service_tier = tier;
        }
        if !self.service_tier.is_empty() {
            template["service_tier"] = json!(self.service_tier);
        }

        // Go reads the type from the data payload only; the SSE `event:` name is the
        // same string and only used here when the payload carries no `type`.
        let data_type = match get(root, "type") {
            Some(t) => gstr(Some(t)),
            None => event.unwrap_or_default().to_string(),
        };
        if data_type == "response.created" {
            self.response_id = gstr(get(root, "response.id"));
            self.created_at = gint(get(root, "response.created_at"));
            self.model = gstr(get(root, "response.model"));
            return Vec::new();
        }

        // Extract and set the model version.
        if let Some(model) = get(root, "model") {
            template["model"] = json!(gstr(Some(model)));
        } else if !self.model.is_empty() {
            template["model"] = json!(self.model);
        } else if !self.request_model.is_empty() {
            template["model"] = json!(self.request_model);
        }

        template["created"] = json!(self.created_at);
        // Extract and set the response ID.
        template["id"] = json!(self.response_id);

        // Extract and set usage metadata (token counts).
        if let Some(usage) = get(root, "response.usage") {
            apply_usage(&mut template, usage);
        }

        let delta = &mut template["choices"][0]["delta"];
        match data_type.as_str() {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = get(root, "delta") {
                    delta["role"] = json!("assistant");
                    delta["reasoning_content"] = json!(gstr(Some(d)));
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                delta["role"] = json!("assistant");
                delta["reasoning_content"] = json!("\n\n");
            }
            "response.output_text.delta" => {
                if let Some(d) = get(root, "delta") {
                    delta["role"] = json!("assistant");
                    delta["content"] = json!(gstr(Some(d)));
                }
            }
            "response.image_generation_call.partial_image" => {
                let item_id = gstr(get(root, "item_id"));
                let b64 = gstr(get(root, "partial_image_b64"));
                if b64.is_empty() || self.image_already_sent(&item_id, &b64) {
                    return Vec::new();
                }
                let output_format = gstr(get(root, "output_format"));
                set_delta_image(delta, &output_format, &b64);
            }
            "response.completed" | "response.incomplete" => {
                let mut finish_reason = "stop".to_string();
                let mut native_finish_reason = finish_reason.clone();
                if data_type == "response.incomplete" {
                    native_finish_reason = gstr(get(root, "response.incomplete_details.reason"));
                    match native_finish_reason.as_str() {
                        "max_tokens" | "max_output_tokens" => finish_reason = "length".into(),
                        "content_filter" => finish_reason = "content_filter".into(),
                        _ => {}
                    }
                } else if self.function_call_index != -1 {
                    finish_reason = "tool_calls".into();
                    native_finish_reason = finish_reason.clone();
                }
                template["choices"][0]["finish_reason"] = json!(finish_reason);
                template["choices"][0]["native_finish_reason"] = json!(native_finish_reason);
            }
            "response.output_item.added" => {
                let Some(item) = get(root, "item") else {
                    return Vec::new();
                };
                if !is_codex_tool_call_type(&gstr(get(item, "type"))) {
                    return Vec::new();
                }

                // Increment index for this new tool call item.
                self.function_call_index += 1;
                let index = self.function_call_index;
                self.register_tool_call_state(
                    root,
                    Some(item),
                    ToolCallStreamState {
                        index,
                        arguments_emitted: false,
                        done: false,
                    },
                );

                // Restore original tool name if it was shortened.
                let name = self.restore_name(gstr(get(item, "name")));
                let delta = &mut template["choices"][0]["delta"];
                delta["role"] = json!("assistant");
                delta["tool_calls"] = json!([{
                    "index": index,
                    "id": gstr(get(item, "call_id")),
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                }]);
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let delta_value = gstr(get(root, "delta"));
                let Some(si) = self.find_tool_call_state(root, None) else {
                    return Vec::new();
                };
                let state = &mut self.states[si];
                if state.done || delta_value.is_empty() {
                    return Vec::new();
                }
                state.arguments_emitted = true;
                delta["tool_calls"] =
                    json!([{"index": state.index, "function": {"arguments": delta_value}}]);
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                let Some(si) = self.find_tool_call_state(root, None) else {
                    return Vec::new();
                };
                let state = &mut self.states[si];
                if state.done || state.arguments_emitted {
                    // Arguments were already streamed via delta events; nothing to emit.
                    return Vec::new();
                }
                // Fallback: no delta events were received, emit the full arguments as a
                // single chunk.
                let full_args_field = if data_type == "response.custom_tool_call_input.done" {
                    "input"
                } else {
                    "arguments"
                };
                state.arguments_emitted = true;
                let full_args = gstr(get(root, full_args_field));
                if full_args.is_empty() {
                    return Vec::new();
                }
                delta["tool_calls"] =
                    json!([{"index": state.index, "function": {"arguments": full_args}}]);
            }
            "response.output_item.done" => {
                let Some(item) = get(root, "item") else {
                    return Vec::new();
                };
                let item_type = gstr(get(item, "type"));
                if item_type == "image_generation_call" {
                    let item_id = gstr(get(item, "id"));
                    let b64 = gstr(get(item, "result"));
                    if b64.is_empty() || self.image_already_sent(&item_id, &b64) {
                        return Vec::new();
                    }
                    let output_format = gstr(get(item, "output_format"));
                    set_delta_image(&mut template["choices"][0]["delta"], &output_format, &b64);
                    return vec![template];
                }
                if !is_codex_tool_call_type(&item_type) {
                    return Vec::new();
                }

                if let Some(si) = self.find_tool_call_state(root, Some(item)) {
                    let state = &mut self.states[si];
                    if state.done {
                        return Vec::new();
                    }
                    state.done = true;
                    if state.arguments_emitted {
                        return Vec::new();
                    }
                    // The tool was announced, but no argument event arrived. Emit only
                    // the completed arguments so the id and name are not duplicated.
                    state.arguments_emitted = true;
                    let full_args = codex_tool_call_arguments(item);
                    if full_args.is_empty() {
                        return Vec::new();
                    }
                    template["choices"][0]["delta"]["tool_calls"] =
                        json!([{"index": state.index, "function": {"arguments": full_args}}]);
                    return vec![template];
                }

                // Fallback path: model skipped output_item.added, so emit the complete
                // tool call now.
                self.function_call_index += 1;
                let index = self.function_call_index;
                self.register_tool_call_state(
                    root,
                    Some(item),
                    ToolCallStreamState {
                        index,
                        arguments_emitted: true,
                        done: true,
                    },
                );
                // Restore original tool name if it was shortened.
                let name = self.restore_name(gstr(get(item, "name")));
                let delta = &mut template["choices"][0]["delta"];
                delta["tool_calls"] = json!([{
                    "index": index,
                    "id": gstr(get(item, "call_id")),
                    "type": "function",
                    "function": {"name": name, "arguments": codex_tool_call_arguments(item)},
                }]);
                delta["role"] = json!("assistant");
            }
            _ => return Vec::new(),
        }

        vec![template]
    }

    /// The `LastImageHashByItemID` dedup shared by both image branches of
    /// ConvertCodexResponseToOpenAI: true when this item's image is unchanged.
    fn image_already_sent(&mut self, item_id: &str, b64: &str) -> bool {
        if item_id.is_empty() {
            return false;
        }
        let hash: [u8; 32] = Sha256::digest(b64.as_bytes()).into();
        if self.last_image_hash_by_item_id.get(item_id) == Some(&hash) {
            return true;
        }
        self.last_image_hash_by_item_id
            .insert(item_id.to_string(), hash);
        false
    }

    // port of registerToolCallState (codex_openai_response.go)
    fn register_tool_call_state(
        &mut self,
        event: &Value,
        item: Option<&Value>,
        state: ToolCallStreamState,
    ) {
        let si = self.states.len();
        self.states.push(state);
        let item_id = gstr(get(event, "item_id"));
        if !item_id.is_empty() {
            self.state_keys.insert(format!("item:{item_id}"), si);
        }
        let item_id = gstr(item.and_then(|i| get(i, "id")));
        if !item_id.is_empty() {
            self.state_keys.insert(format!("item:{item_id}"), si);
        }
        if let Some(output_index) = get(event, "output_index") {
            self.state_keys.insert(format!("output:{output_index}"), si);
        }
        self.current_tool_call = Some(si);
    }

    // port of findToolCallState (codex_openai_response.go)
    fn find_tool_call_state(&self, event: &Value, item: Option<&Value>) -> Option<usize> {
        let item_id = gstr(get(event, "item_id"));
        if !item_id.is_empty() {
            if let Some(si) = self.state_keys.get(&format!("item:{item_id}")) {
                return Some(*si);
            }
        }
        let item_id = gstr(item.and_then(|i| get(i, "id")));
        if !item_id.is_empty() {
            if let Some(si) = self.state_keys.get(&format!("item:{item_id}")) {
                return Some(*si);
            }
        }
        if let Some(output_index) = get(event, "output_index") {
            if let Some(si) = self.state_keys.get(&format!("output:{output_index}")) {
                return Some(*si);
            }
        }
        self.current_tool_call
    }
}

/// Appends one image to a chunk's (fresh, so empty) `delta.images` — the shared
/// body of both image branches of ConvertCodexResponseToOpenAI.
fn set_delta_image(delta: &mut Value, output_format: &str, b64: &str) {
    let image_url = format!(
        "data:{};base64,{}",
        mime_type_from_codex_output_format(output_format),
        b64
    );
    if !matches!(delta.get("images"), Some(Value::Array(_))) {
        delta["images"] = json!([]);
    }
    let image_index = delta["images"].as_array().map_or(0, |a| a.len());
    delta["role"] = json!("assistant");
    if let Some(images) = delta["images"].as_array_mut() {
        images.push(
            json!({"type": "image_url", "image_url": {"url": image_url}, "index": image_index}),
        );
    }
}

/// The usage block ConvertCodexResponseToOpenAI and
/// ConvertCodexResponseToOpenAINonStream both inline.
fn apply_usage(template: &mut Value, usage: &Value) {
    if let Some(v) = get(usage, "output_tokens") {
        template["usage"]["completion_tokens"] = json!(gint(Some(v)));
    }
    if let Some(v) = get(usage, "total_tokens") {
        template["usage"]["total_tokens"] = json!(gint(Some(v)));
    }
    if let Some(v) = get(usage, "input_tokens") {
        template["usage"]["prompt_tokens"] = json!(gint(Some(v)));
    }
    if let Some(v) = get(usage, "input_tokens_details.cached_tokens") {
        template["usage"]["prompt_tokens_details"]["cached_tokens"] = json!(gint(Some(v)));
    }
    set_codex_cache_write_tokens(template, usage);
    if let Some(v) = get(usage, "output_tokens_details.reasoning_tokens") {
        template["usage"]["completion_tokens_details"]["reasoning_tokens"] = json!(gint(Some(v)));
    }
}

/// Complete upstream non-stream response (a final Responses `response` object;
/// the router has already folded the Codex SSE into it with `output` filled
/// from response.output_item.done items) → client chat.completion JSON.
///
/// Accepts either the bare `response` object or the whole terminal event
/// (`{"type":"response.completed","response":{…}}`, the shape the Go function
/// takes). Anything that is neither yields `Value::Null` (Go returns empty bytes).
// port of ConvertCodexResponseToOpenAINonStream (codex_openai_response.go)
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let event_type = gstr(get(upstream, "type"));
    let (root, response) =
        if event_type == "response.completed" || event_type == "response.incomplete" {
            (upstream, upstream.get("response").unwrap_or(&Value::Null))
        } else if upstream.is_object()
            && (gstr(get(upstream, "object")) == "response" || upstream.get("output").is_some())
        {
            (upstream, upstream)
        } else {
            // Verify this is a terminal response event.
            return Value::Null;
        };

    let unix_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut template = json!({
        "id": "",
        "object": "chat.completion",
        "created": 123456,
        "model": "model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "reasoning_content": null, "tool_calls": null},
            "finish_reason": null,
            "native_finish_reason": null,
        }],
    });

    let tier = codex_response_service_tier(response);
    let tier = if tier.is_empty() {
        codex_response_service_tier(root)
    } else {
        tier
    };
    if !tier.is_empty() {
        template["service_tier"] = json!(tier);
    }

    // Extract and set the model version.
    if let Some(v) = get(response, "model") {
        template["model"] = json!(gstr(Some(v)));
    }
    // Extract and set the creation timestamp.
    match get(response, "created_at") {
        Some(v) => template["created"] = json!(gint(Some(v))),
        None => template["created"] = json!(unix_timestamp),
    }
    // Extract and set the response ID.
    if let Some(v) = get(response, "id") {
        template["id"] = json!(gstr(Some(v)));
    }
    // Extract and set usage metadata (token counts).
    if let Some(usage) = get(response, "usage") {
        apply_usage(&mut template, usage);
    }

    // Process the output array for content and function calls
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    if let Some(Value::Array(output_array)) = response.get("output") {
        let mut content_text = String::new();
        let mut reasoning_text = String::new();
        let reverse_names = build_reverse_map_from_original_openai(original_request);

        for output_item in output_array {
            match gstr(get(output_item, "type")).as_str() {
                "reasoning" => {
                    // Extract reasoning content from summary
                    if let Some(Value::Array(summary)) = output_item.get("summary") {
                        if let Some(s) = summary
                            .iter()
                            .find(|s| gstr(get(s, "type")) == "summary_text")
                        {
                            reasoning_text.push_str(&gstr(get(s, "text")));
                        }
                    }
                    // Extract reasoning content from content
                    if let Some(Value::Array(content)) = output_item.get("content") {
                        for c in content {
                            if gstr(get(c, "type")) == "reasoning_text" {
                                reasoning_text.push_str(&gstr(get(c, "text")));
                            }
                        }
                    }
                }
                "message" => {
                    // Extract message content (the first output_text part only)
                    if let Some(Value::Array(content)) = output_item.get("content") {
                        if let Some(c) = content
                            .iter()
                            .find(|c| gstr(get(c, "type")) == "output_text")
                        {
                            content_text.push_str(&gstr(get(c, "text")));
                        }
                    }
                }
                "function_call" | "custom_tool_call" => {
                    // Handle function and custom tool call content.
                    let mut call = json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}});
                    if let Some(v) = get(output_item, "call_id") {
                        call["id"] = json!(gstr(Some(v)));
                    }
                    if let Some(v) = get(output_item, "name") {
                        let n = gstr(Some(v));
                        let n = reverse_names.get(&n).cloned().unwrap_or(n);
                        call["function"]["name"] = json!(n);
                    }
                    call["function"]["arguments"] = json!(codex_tool_call_arguments(output_item));
                    tool_calls.push(call);
                }
                "image_generation_call" => {
                    let b64 = gstr(get(output_item, "result"));
                    if b64.is_empty() {
                        continue;
                    }
                    let output_format = gstr(get(output_item, "output_format"));
                    let image_url = format!(
                        "data:{};base64,{}",
                        mime_type_from_codex_output_format(&output_format),
                        b64
                    );
                    images.push(json!({"type": "image_url", "image_url": {"url": image_url}, "index": images.len()}));
                }
                _ => {}
            }
        }

        // Set content and reasoning content if found
        let message = &mut template["choices"][0]["message"];
        if !content_text.is_empty() {
            message["content"] = json!(content_text);
        }
        if !reasoning_text.is_empty() {
            message["reasoning_content"] = json!(reasoning_text);
        }
        // Add tool calls if any
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls.clone());
        }
        // Add images if any
        if !images.is_empty() {
            message["images"] = Value::Array(images);
        }
    }

    // Extract and set the finish reason based on status.
    if let Some(status) = get(response, "status") {
        let mut finish_reason = String::new();
        let mut native_finish_reason = String::new();
        match gstr(Some(status)).as_str() {
            "completed" => {
                finish_reason = if tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }
                .into();
                native_finish_reason = finish_reason.clone();
            }
            "incomplete" => {
                native_finish_reason = gstr(get(response, "incomplete_details.reason"));
                finish_reason = match native_finish_reason.as_str() {
                    "max_tokens" | "max_output_tokens" => "length",
                    "content_filter" => "content_filter",
                    _ => "stop",
                }
                .into();
            }
            _ => {}
        }
        if !finish_reason.is_empty() {
            template["choices"][0]["finish_reason"] = json!(finish_reason);
            template["choices"][0]["native_finish_reason"] = json!(native_finish_reason);
        }
    }

    template
}

// port of isCodexToolCallType (codex_openai_response.go)
fn is_codex_tool_call_type(item_type: &str) -> bool {
    item_type == "function_call" || item_type == "custom_tool_call"
}

// port of codexToolCallArguments (codex_openai_response.go)
fn codex_tool_call_arguments(item: &Value) -> String {
    if gstr(get(item, "type")) == "custom_tool_call" {
        return gstr(get(item, "input"));
    }
    gstr(get(item, "arguments"))
}

// port of buildReverseMapFromOriginalOpenAI (codex_openai_response.go)
fn build_reverse_map_from_original_openai(original: &Value) -> HashMap<String, String> {
    let names = collect_request_tool_names(original);
    if names.is_empty() {
        return HashMap::new();
    }
    build_short_name_map(&names)
        .into_iter()
        .map(|(orig, short)| (short, orig))
        .collect()
}

// port of mimeTypeFromCodexOutputFormat (codex_openai_response.go)
fn mime_type_from_codex_output_format(output_format: &str) -> String {
    if output_format.is_empty() {
        return "image/png".into();
    }
    if output_format.contains('/') {
        return output_format.into();
    }
    match output_format.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
    .into()
}

// port of codexResponseServiceTier (codex_openai_response.go)
// Returns only an actual nonempty upstream tier.
fn codex_response_service_tier(response: &Value) -> String {
    match get(response, "service_tier") {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

// port of setCodexCacheWriteTokens (codex_openai_response.go)
// Preserves the upstream integer without float conversion. Go accepts any
// all-digit raw literal; serde_json (no `arbitrary_precision`) has already turned
// a literal beyond u64 into an f64 by the time it reaches here, so only u64
// values qualify.
fn set_codex_cache_write_tokens(template: &mut Value, usage: &Value) {
    let Some(value) = get(usage, "input_tokens_details.cache_write_tokens") else {
        return;
    };
    let Value::Number(n) = value else {
        return;
    };
    if !n.is_u64() {
        return;
    }
    template["usage"]["prompt_tokens_details"]["cache_write_tokens"] = value.clone();
    template["usage"]["prompt_tokens_details"]["cached_creation_tokens"] = value.clone();
}

// ───────────────────────────── gjson helpers ─────────────────────────────

/// gjson `Get(path)`: dot-separated object keys / array indexes. `Some` ⇔ `Exists()`
/// (an explicit JSON null exists).
fn get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
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

/// gjson `Result.String()`: strings verbatim, absent/null → "", anything else
/// as its JSON text.
fn gstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// gjson `Result.Int()`: numbers truncated, numeric strings parsed, true → 1.
fn gint(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG_FN: &str =
        "a_very_long_tool_name_that_exceeds_sixty_four_characters_limit_here_test";
    const LONG_FN_SHORT: &str = "a_very_long_tool_name_that_exceeds_sixty_four_characters_limit_h";
    const LONG_CUSTOM: &str =
        "a_very_long_custom_tool_name_that_exceeds_sixty_four_characters_limit_test";
    const LONG_CUSTOM_SHORT: &str =
        "a_very_long_custom_tool_name_that_exceeds_sixty_four_characters_";
    const LONG_CUSTOM_SHORT_1: &str =
        "a_very_long_custom_tool_name_that_exceeds_sixty_four_character_1";

    fn req(body: Value) -> Value {
        translate_request("gpt-5.6-sol", &body, true)
    }

    fn push(t: &mut StreamTranslator, data: Value) -> Vec<Value> {
        let event = data
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        t.push(event.as_deref(), &data)
            .into_iter()
            .map(|frame| {
                assert!(
                    frame.starts_with("data: ") && frame.ends_with("\n\n"),
                    "{frame:?}"
                );
                serde_json::from_str(&frame["data: ".len()..frame.len() - 2]).unwrap()
            })
            .collect()
    }

    /// The chunk template with every bookkeeping field at its pre-`response.created` value.
    fn ck(model: &str, delta: Value) -> Value {
        json!({
            "id": "", "object": "chat.completion.chunk", "created": 0, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null, "native_finish_reason": null}],
        })
    }

    fn tool_round(tool_content: Value) -> Value {
        req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Check tool output."},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_x", "type": "function", "function": {"name": "inspect", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_x", "content": tool_content}
            ],
            "tools": [{"type": "function", "function": {"name": "inspect", "parameters": {"type": "object", "properties": {}}}}]
        }))
    }

    // ── request: ports of codex_openai_request_test.go ──

    // port of TestToolCallSimple
    #[test]
    fn tool_call_simple() {
        let out = translate_request(
            "gpt-4o",
            &json!({
                "model": "gpt-4o",
                "messages": [
                    {"role": "system", "content": "You are a helpful assistant."},
                    {"role": "user", "content": "What is the weather in Paris?"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "sunny, 22C"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "get_weather", "description": "Get weather for a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }}]
            }),
            true,
        );
        assert_eq!(
            out,
            json!({
                "instructions": "",
                "stream": true,
                "reasoning": {"effort": "medium"},
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"],
                "model": "gpt-4o",
                "input": [
                    {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are a helpful assistant."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What is the weather in Paris?"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "sunny, 22C"}
                ],
                "tools": [{
                    "type": "function", "name": "get_weather", "description": "Get weather for a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                    "strict": false
                }],
                "store": false
            })
        );
        // key order matches the Go output too
        let keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "instructions",
                "stream",
                "reasoning",
                "parallel_tool_calls",
                "include",
                "model",
                "input",
                "tools",
                "store"
            ]
        );
    }

    // port of TestToolCallWithContent
    #[test]
    fn tool_call_with_content() {
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "What is the weather?"},
                {"role": "assistant", "content": "Let me check the weather for you.", "tool_calls": [
                    {"id": "call_abc", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_abc", "content": "rainy, 15C"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "description": "Get weather", "parameters": {"type": "object", "properties": {}}}}]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What is the weather?"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Let me check the weather for you."}]},
                {"type": "function_call", "call_id": "call_abc", "name": "get_weather", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_abc", "output": "rainy, 15C"}
            ])
        );
    }

    // port of TestToolCallOutputWithMultimodalContent
    #[test]
    fn tool_call_output_with_multimodal_content() {
        let out = tool_round(json!([
            {"type": "text", "text": "Rendered result attached."},
            {"type": "image_url", "image_url": {"url": "https://example.com/generated.png", "detail": "high"}},
            {"type": "image_url", "image_url": {"file_id": "file-img-123"}},
            {"type": "file", "file": {"file_id": "file-doc-123", "filename": "doc.pdf"}},
            {"type": "file", "file": {"file_data": "SGVsbG8=", "filename": "inline.txt"}},
            {"type": "file", "file": {"file_url": "https://example.com/report.pdf", "filename": "report.pdf"}}
        ]));
        assert_eq!(
            out["input"][2],
            json!({"type": "function_call_output", "call_id": "call_x", "output": [
                {"type": "input_text", "text": "Rendered result attached."},
                {"type": "input_image", "image_url": "https://example.com/generated.png", "detail": "high"},
                {"type": "input_image", "file_id": "file-img-123"},
                {"type": "input_file", "file_id": "file-doc-123", "filename": "doc.pdf"},
                {"type": "input_file", "file_data": "SGVsbG8=", "filename": "inline.txt"},
                {"type": "input_file", "file_url": "https://example.com/report.pdf", "filename": "report.pdf"}
            ]})
        );
    }

    // port of TestToolCallOutputWithStringifiedImageContent
    #[test]
    fn tool_call_output_with_stringified_image_content() {
        let out = tool_round(json!(
            "[{\"type\":\"input_text\",\"text\":\"Captured screenshot.\"},{\"detail\":\"original\",\"image_url\":\"data:image/png;base64,AA==\",\"type\":\"input_image\"}]"
        ));
        assert_eq!(
            out["input"][2]["output"],
            json!([
                {"type": "input_text", "text": "Captured screenshot."},
                {"type": "input_image", "image_url": "data:image/png;base64,AA==", "detail": "original"}
            ])
        );
        let out = tool_round(json!(
            "[{\"type\":\"image_url\",\"image_url\":{\"url\":\"https://example.com/generated.png\",\"detail\":\"high\"}}]"
        ));
        assert_eq!(
            out["input"][2]["output"],
            json!([{"type": "input_image", "image_url": "https://example.com/generated.png", "detail": "high"}])
        );
    }

    // port of TestToolCallOutputKeepsNonImageStrings
    #[test]
    fn tool_call_output_keeps_non_image_strings() {
        for s in [
            "plain output",
            "{\"status\":\"ok\"}",
            "[{\"type\":\"input_text\",\"text\":\"still text\"}]",
            "[{\"type\":\"input_image\",\"detail\":\"low\"}]",
        ] {
            assert_eq!(tool_round(json!(s))["input"][2]["output"], json!(s));
        }
    }

    // port of TestToolCallOutputFallsBackForInvalidStructuredParts
    #[test]
    fn tool_call_output_falls_back_for_invalid_structured_parts() {
        let out = tool_round(json!([
            {"type": "image_url", "image_url": {"detail": "low"}},
            {"type": "file", "file": {"filename": "orphan.txt"}},
            {"type": "unknown_type", "foo": "bar", "nested": {"a": 1}}
        ]));
        assert_eq!(
            out["input"][2]["output"],
            json!([
                {"type": "input_text", "text": "{\"type\":\"image_url\",\"image_url\":{\"detail\":\"low\"}}"},
                {"type": "input_text", "text": "{\"type\":\"file\",\"file\":{\"filename\":\"orphan.txt\"}}"},
                {"type": "input_text", "text": "{\"type\":\"unknown_type\",\"foo\":\"bar\",\"nested\":{\"a\":1}}"}
            ])
        );
    }

    // port of TestToolCallOutputWithNonStringJSONContent
    #[test]
    fn tool_call_output_with_non_string_json_content() {
        assert_eq!(tool_round(Value::Null)["input"][2]["output"], json!("null"));
        assert_eq!(
            tool_round(json!({"status": "ok", "count": 2}))["input"][2]["output"],
            json!("{\"status\":\"ok\",\"count\":2}")
        );
    }

    // port of TestConvertOpenAIRequestToCodexPreservesInputAudio
    #[test]
    fn preserves_input_audio() {
        let out = req(json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Transcribe this audio verbatim."},
            {"type": "input_audio", "input_audio": {"data": "SUQzBA==", "format": "mp3"}}
        ]}]}));
        assert_eq!(
            out["input"],
            json!([{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Transcribe this audio verbatim."},
                {"type": "input_audio", "data": "SUQzBA==", "format": "mp3"}
            ]}])
        );
    }

    // port of TestMultipleToolCalls
    #[test]
    fn multiple_tool_calls() {
        let call = |id: &str, city: &str| json!({"id": id, "type": "function", "function": {"name": "get_weather", "arguments": format!("{{\"city\":\"{city}\"}}")}});
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Compare weather in Paris, London and Tokyo"},
                {"role": "assistant", "content": null, "tool_calls": [call("call_paris", "Paris"), call("call_london", "London"), call("call_tokyo", "Tokyo")]},
                {"role": "tool", "tool_call_id": "call_paris", "content": "sunny, 22C"},
                {"role": "tool", "tool_call_id": "call_london", "content": "cloudy, 14C"},
                {"role": "tool", "tool_call_id": "call_tokyo", "content": "humid, 28C"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Compare weather in Paris, London and Tokyo"}]},
                {"type": "function_call", "call_id": "call_paris", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call", "call_id": "call_london", "name": "get_weather", "arguments": "{\"city\":\"London\"}"},
                {"type": "function_call", "call_id": "call_tokyo", "name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"},
                {"type": "function_call_output", "call_id": "call_paris", "output": "sunny, 22C"},
                {"type": "function_call_output", "call_id": "call_london", "output": "cloudy, 14C"},
                {"type": "function_call_output", "call_id": "call_tokyo", "output": "humid, 28C"}
            ])
        );
    }

    // port of TestNoSpuriousEmptyAssistantMessage + TestEmptyStringContent
    #[test]
    fn no_spurious_empty_assistant_message_and_empty_string_content() {
        for content in [Value::Null, json!("")] {
            let out = req(json!({
                "messages": [
                    {"role": "user", "content": "Call a tool"},
                    {"role": "assistant", "content": content, "tool_calls": [
                        {"id": "call_x", "type": "function", "function": {"name": "do_thing", "arguments": "{}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_x", "content": "done"}
                ],
                "tools": [{"type": "function", "function": {"name": "do_thing"}}]
            }));
            assert_eq!(
                out["input"],
                json!([
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Call a tool"}]},
                    {"type": "function_call", "call_id": "call_x", "name": "do_thing", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_x", "output": "done"}
                ])
            );
        }
    }

    // port of TestMultiTurnToolCalling
    #[test]
    fn multi_turn_tool_calling() {
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Weather in Paris?"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "call_r1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                {"role": "tool", "tool_call_id": "call_r1", "content": "sunny"},
                {"role": "assistant", "content": "It is sunny in Paris."},
                {"role": "user", "content": "And London?"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "call_r2", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"London\"}"}}]},
                {"role": "tool", "tool_call_id": "call_r2", "content": "rainy"}
            ]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather in Paris?"}]},
                {"type": "function_call", "call_id": "call_r1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_r1", "output": "sunny"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "It is sunny in Paris."}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "And London?"}]},
                {"type": "function_call", "call_id": "call_r2", "name": "get_weather", "arguments": "{\"city\":\"London\"}"},
                {"type": "function_call_output", "call_id": "call_r2", "output": "rainy"}
            ])
        );
    }

    // port of TestToolNameShortening
    #[test]
    fn tool_name_shortening() {
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Do it"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "call_long", "type": "function", "function": {"name": LONG_FN, "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_long", "content": "ok"}
            ],
            "tools": [{"type": "function", "function": {"name": LONG_FN, "description": "A tool with a very long name", "parameters": {"type": "object", "properties": {}}}}]
        }));
        assert_eq!(
            out["input"][1],
            json!({"type": "function_call", "call_id": "call_long", "name": LONG_FN_SHORT, "arguments": "{}"})
        );
        assert_eq!(out["tools"][0]["name"], json!(LONG_FN_SHORT));
        // mcp__ names keep the prefix and the last segment
        assert_eq!(
            shorten_name_if_needed(&format!("mcp__some_server__{LONG_FN}")),
            format!("mcp__{LONG_FN}")[..64]
        );
        assert_eq!(
            shorten_name_if_needed(
                "mcp__a_very_long_server_name_exceeding_the_limit_on_its_own__read"
            ),
            "mcp__read"
        );
    }

    // port of TestCustomToolNameShortening
    #[test]
    fn custom_tool_name_shortening() {
        let input = json!({
            "messages": [
                {"role": "user", "content": "Apply the patch."},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_custom_long", "type": "function", "function": {"name": LONG_CUSTOM, "arguments": "patch"}}
                ]},
                {"role": "tool", "tool_call_id": "call_custom_long", "content": "patched"}
            ],
            "tools": [{"type": "custom", "name": LONG_CUSTOM, "description": "Apply a patch."}],
            "tool_choice": {"type": "custom", "name": LONG_CUSTOM}
        });
        let out = req(input.clone());
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Apply the patch."}]},
                {"type": "custom_tool_call", "call_id": "call_custom_long", "name": LONG_CUSTOM_SHORT, "input": "patch"},
                {"type": "custom_tool_call_output", "call_id": "call_custom_long", "output": "patched"}
            ])
        );
        assert_eq!(
            out["tools"],
            json!([{"type": "custom", "name": LONG_CUSTOM_SHORT, "description": "Apply a patch."}])
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "custom", "name": LONG_CUSTOM_SHORT})
        );
        assert_eq!(
            build_reverse_map_from_original_openai(&input)[LONG_CUSTOM_SHORT],
            LONG_CUSTOM
        );
    }

    // port of TestCustomToolShortNameCollisionPreservesFunctionFamily
    #[test]
    fn custom_tool_short_name_collision_preserves_function_family() {
        let function_name = shorten_name_if_needed(LONG_CUSTOM);
        assert_eq!(function_name, LONG_CUSTOM_SHORT);
        let out = req(json!({
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_function", "type": "function", "function": {"name": function_name, "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_function", "content": "done"}
            ],
            "tools": [
                {"type": "custom", "name": LONG_CUSTOM, "description": "Custom tool."},
                {"type": "function", "function": {"name": function_name, "parameters": {"type": "object"}}}
            ],
            "tool_choice": {"type": "function", "function": {"name": function_name}}
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "function_call", "call_id": "call_function", "name": LONG_CUSTOM_SHORT_1, "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_function", "output": "done"}
            ])
        );
        assert_eq!(
            out["tools"],
            json!([
                {"type": "custom", "name": LONG_CUSTOM_SHORT, "description": "Custom tool."},
                {"type": "function", "name": LONG_CUSTOM_SHORT_1, "parameters": {"type": "object"}, "strict": false}
            ])
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "name": LONG_CUSTOM_SHORT_1})
        );
    }

    // port of TestSameNameCustomAndFunctionDefaultsToFunctionFamily
    #[test]
    fn same_name_custom_and_function_defaults_to_function_family() {
        let out = req(json!({
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_shared", "type": "function", "function": {"name": "shared_tool", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_shared", "content": "done"}
            ],
            "tools": [
                {"type": "custom", "name": "shared_tool", "description": "Custom tool."},
                {"type": "function", "function": {"name": "shared_tool", "parameters": {"type": "object"}}}
            ],
            "tool_choice": {"type": "function", "function": {"name": "shared_tool"}}
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "function_call", "call_id": "call_shared", "name": "shared_tool", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_shared", "output": "done"}
            ])
        );
        assert_eq!(
            out["tools"],
            json!([
                {"type": "custom", "name": "shared_tool", "description": "Custom tool."},
                {"type": "function", "name": "shared_tool", "parameters": {"type": "object"}, "strict": false}
            ])
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "name": "shared_tool"})
        );
    }

    // port of TestCallIDsMatchBetweenCallAndOutput
    #[test]
    fn call_ids_match_between_call_and_output() {
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Multi-tool"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "id_a", "type": "function", "function": {"name": "tool_a", "arguments": "{}"}},
                    {"id": "id_b", "type": "function", "function": {"name": "tool_b", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "id_a", "content": "res_a"},
                {"role": "tool", "tool_call_id": "id_b", "content": "res_b"}
            ]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Multi-tool"}]},
                {"type": "function_call", "call_id": "id_a", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call", "call_id": "id_b", "name": "tool_b", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "id_a", "output": "res_a"},
                {"type": "function_call_output", "call_id": "id_b", "output": "res_b"}
            ])
        );
    }

    // port of TestCustomToolCallHistory
    #[test]
    fn custom_tool_call_history() {
        let patch = "*** Begin Patch\n*** Add File: spec.md\n+done\n*** End Patch";
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Update the specification."},
                {"role": "assistant", "content": "I will update the file.", "tool_calls": [
                    {"id": "call_apply_patch", "type": "function", "function": {"name": "apply_patch", "arguments": patch}}
                ]},
                {"role": "tool", "tool_call_id": "call_apply_patch", "content": "Added spec.md"},
                {"role": "assistant", "content": "The specification is updated."}
            ],
            "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a freeform patch."}],
            "tool_choice": {"type": "function", "function": {"name": "apply_patch"}}
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Update the specification."}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I will update the file."}]},
                {"type": "custom_tool_call", "call_id": "call_apply_patch", "name": "apply_patch", "input": patch},
                {"type": "custom_tool_call_output", "call_id": "call_apply_patch", "output": "Added spec.md"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The specification is updated."}]}
            ])
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "custom", "name": "apply_patch"})
        );
    }

    // port of TestCustomToolCallResponseFollowUpRoundTrip
    #[test]
    fn custom_tool_call_response_follow_up_round_trip() {
        let original = json!({
            "messages": [{"role": "user", "content": "Apply the patch."}],
            "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a patch."}]
        });
        let chat = translate_non_stream(
            &json!({"type": "response.completed", "response": {"status": "completed", "output": [
                {"type": "custom_tool_call", "call_id": "call_patch", "name": "apply_patch", "input": "patch"}
            ]}}),
            &original,
        );
        let assistant = chat["choices"][0]["message"].clone();
        assert_eq!(
            assistant["tool_calls"],
            json!([{"id": "call_patch", "type": "function", "function": {"name": "apply_patch", "arguments": "patch"}}])
        );
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Apply the patch."},
                assistant,
                {"role": "tool", "tool_call_id": "call_patch", "content": "patched"}
            ],
            "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a patch."}]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Apply the patch."}]},
                {"type": "custom_tool_call", "call_id": "call_patch", "name": "apply_patch", "input": "patch"},
                {"type": "custom_tool_call_output", "call_id": "call_patch", "output": "patched"}
            ])
        );
    }

    // port of TestMixedToolCallHistoryPreservesCallFamilies
    #[test]
    fn mixed_tool_call_history_preserves_call_families() {
        let out = req(json!({
            "messages": [
                {"role": "user", "content": "Run both tools."},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_function", "type": "function", "function": {"name": "lookup", "arguments": "{}"}},
                    {"id": "call_custom", "type": "function", "function": {"name": "apply_patch", "arguments": "patch"}}
                ]},
                {"role": "tool", "tool_call_id": "call_custom", "content": "patched"},
                {"role": "tool", "tool_call_id": "call_function", "content": "found"}
            ],
            "tools": [
                {"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}},
                {"type": "custom", "name": "apply_patch", "description": "Apply a patch."}
            ]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run both tools."}]},
                {"type": "function_call", "call_id": "call_function", "name": "lookup", "arguments": "{}"},
                {"type": "custom_tool_call", "call_id": "call_custom", "name": "apply_patch", "input": "patch"},
                {"type": "custom_tool_call_output", "call_id": "call_custom", "output": "patched"},
                {"type": "function_call_output", "call_id": "call_function", "output": "found"}
            ])
        );
    }

    // port of TestToolCallHistoryAllowsReusedCallIDAcrossRounds
    #[test]
    fn tool_call_history_allows_reused_call_id_across_rounds() {
        let out = req(json!({"messages": [
            {"role": "user", "content": "Run the first tool."},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_reused", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_reused", "content": "found"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_reused", "type": "custom", "custom": {"name": "apply_patch", "input": "patch"}}
            ]},
            {"role": "tool", "tool_call_id": "call_reused", "content": "patched"}
        ]}));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run the first tool."}]},
                {"type": "function_call", "call_id": "call_reused", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_reused", "output": "found"},
                {"type": "custom_tool_call", "call_id": "call_reused", "name": "apply_patch", "input": "patch"},
                {"type": "custom_tool_call_output", "call_id": "call_reused", "output": "patched"}
            ])
        );
    }

    // port of TestCustomToolCallHistorySynthesizesMissingCallID
    #[test]
    fn custom_tool_call_history_synthesizes_missing_call_id() {
        let out = req(json!({"messages": [
            {"role": "tool", "content": "orphan"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"type": "custom", "custom": {"name": "apply_patch", "input": "patch"}}
            ]},
            {"role": "tool", "content": "patched"}
        ]}));
        assert_eq!(
            out["input"],
            json!([
                {"type": "custom_tool_call", "call_id": "call_missing_1_0", "name": "apply_patch", "input": "patch"},
                {"type": "custom_tool_call_output", "call_id": "call_missing_1_0", "output": "patched"}
            ])
        );
    }

    // port of TestToolCallHistoryClearsUnmatchedCallAtNewBatch
    #[test]
    fn tool_call_history_clears_unmatched_call_at_new_batch() {
        let out = req(json!({"messages": [
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_reused", "type": "custom", "custom": {"name": "apply_patch", "input": "old patch"}}
            ]},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_reused", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_reused", "content": "found"}
        ]}));
        assert_eq!(
            out["input"],
            json!([
                {"type": "custom_tool_call", "call_id": "call_reused", "name": "apply_patch", "input": "old patch"},
                {"type": "function_call", "call_id": "call_reused", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_reused", "output": "found"}
            ])
        );
    }

    // port of TestToolCallOutputWithoutIDUsesPendingCall
    #[test]
    fn tool_call_output_without_id_uses_pending_call() {
        let out = req(json!({"messages": [
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_explicit", "type": "function", "function": {"name": "lookup", "arguments": "{}"}},
                {"type": "custom", "custom": {"name": "apply_patch", "input": "patch"}}
            ]},
            {"role": "tool", "content": "found"},
            {"role": "tool", "content": "patched"}
        ]}));
        assert_eq!(
            out["input"],
            json!([
                {"type": "function_call", "call_id": "call_explicit", "name": "lookup", "arguments": "{}"},
                {"type": "custom_tool_call", "call_id": "call_missing_0_1", "name": "apply_patch", "input": "patch"},
                {"type": "function_call_output", "call_id": "call_explicit", "output": "found"},
                {"type": "custom_tool_call_output", "call_id": "call_missing_0_1", "output": "patched"}
            ])
        );
    }

    // port of TestAmbiguousDuplicateToolCallIDsAreDropped
    #[test]
    fn ambiguous_duplicate_tool_call_ids_are_dropped() {
        let out = req(json!({"messages": [
            {"role": "user", "content": "Run both tools."},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_duplicate", "type": "function", "function": {"name": "lookup", "arguments": "{}"}},
                {"id": "call_duplicate", "type": "custom", "custom": {"name": "apply_patch", "input": "patch"}}
            ]},
            {"role": "tool", "tool_call_id": "call_duplicate", "content": "first"},
            {"role": "tool", "tool_call_id": "call_duplicate", "content": "second"}
        ]}));
        assert_eq!(
            out["input"],
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run both tools."}]}])
        );
    }

    // port of TestOrphanAndDuplicateToolCallOutputsAreDropped
    #[test]
    fn orphan_and_duplicate_tool_call_outputs_are_dropped() {
        let out = req(json!({
            "messages": [
                {"role": "tool", "tool_call_id": "call_orphan", "content": "orphan"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_custom", "type": "function", "function": {"name": "apply_patch", "arguments": "patch"}}
                ]},
                {"role": "tool", "tool_call_id": "call_custom", "content": "patched"},
                {"role": "tool", "tool_call_id": "call_custom", "content": "duplicate"}
            ],
            "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a patch."}]
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "custom_tool_call", "call_id": "call_custom", "name": "apply_patch", "input": "patch"},
                {"type": "custom_tool_call_output", "call_id": "call_custom", "output": "patched"}
            ])
        );
    }

    // port of TestToolsDefinitionTranslated + TestFunctionToolStrictDefaultsToFalse
    #[test]
    fn tools_definition_translated_and_strict_defaults_to_false() {
        let out = req(json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "tools": [
                {"type": "function", "function": {"name": "search", "description": "Search the web",
                    "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}},
                {"type": "function", "function": {"name": "explicit_true", "strict": true, "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "explicit_false", "strict": false, "parameters": {"type": "object"}}},
                {"type": "web_search"}
            ]
        }));
        assert_eq!(
            out["tools"],
            json!([
                {"type": "function", "name": "search", "description": "Search the web",
                    "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}, "strict": false},
                {"type": "function", "name": "explicit_true", "parameters": {"type": "object"}, "strict": true},
                {"type": "function", "name": "explicit_false", "parameters": {"type": "object"}, "strict": false},
                {"type": "web_search"}
            ])
        );
    }

    // port of TestNormalizeInvalidToolNames
    #[test]
    fn normalize_invalid_tool_names() {
        let bad = "mcp.server:search tool";
        let input = json!({
            "messages": [
                {"role": "user", "content": "Search for info"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": bad, "arguments": "{\"query\":\"test\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "result"}
            ],
            "tools": [{"type": "function", "function": {"name": bad, "description": "Search tool", "parameters": {"type": "object", "properties": {}}}}],
            "tool_choice": {"type": "function", "function": {"name": bad}}
        });
        let out = req(input.clone());
        assert_eq!(out["tools"][0]["name"], json!("mcp_server_search_tool"));
        assert_eq!(out["input"][1]["name"], json!("mcp_server_search_tool"));
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "name": "mcp_server_search_tool"})
        );
        assert_eq!(
            build_reverse_map_from_original_openai(&input)["mcp_server_search_tool"],
            bad
        );
    }

    // port of TestNormalizeInvalidToolNamesCollisionAndNonASCII
    #[test]
    fn normalize_invalid_tool_names_collision_and_non_ascii() {
        let out = req(json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "function", "function": {"name": "tool.search"}},
                {"type": "function", "function": {"name": "tool:search"}},
                {"type": "function", "function": {"name": "工具_run"}}
            ]
        }));
        assert_eq!(
            out["tools"],
            json!([
                {"type": "function", "name": "tool_search", "strict": false},
                {"type": "function", "name": "tool_search_1", "strict": false},
                {"type": "function", "name": "___run", "strict": false}
            ])
        );
    }

    // port of TestHistoricalToolCallCollisionWithDeclaredTool
    #[test]
    fn historical_tool_call_collision_with_declared_tool() {
        let input = json!({
            "messages": [
                {"role": "user", "content": "previous call"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_hist_1", "type": "function", "function": {"name": "tool.search", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_hist_1", "content": "hist result"},
                {"role": "user", "content": "new query"}
            ],
            "tools": [{"type": "function", "function": {"name": "tool:search", "description": "current search tool"}}]
        });
        let out = req(input.clone());
        assert_eq!(out["tools"][0]["name"], json!("tool_search"));
        assert_eq!(out["input"][1]["name"], json!("tool_search_1"));
        let rev = build_reverse_map_from_original_openai(&input);
        assert_eq!(rev["tool_search"], "tool:search");
        assert_eq!(rev["tool_search_1"], "tool.search");
    }

    /// Not a Go test: pins the response_format / text / reasoning_effort /
    /// tool_choice / dropped-sampling-params behaviour of the Go code.
    #[test]
    fn response_format_reasoning_effort_and_tool_choice() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let out = translate_request(
            "gpt-5.5",
            &json!({
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}]}],
                "reasoning_effort": "high",
                "response_format": {"type": "json_schema", "json_schema": {"name": "out", "strict": true, "schema": schema}},
                "text": {"verbosity": "low"},
                "tool_choice": "auto",
                "temperature": 0.2,
                "max_tokens": 100
            }),
            false,
        );
        assert_eq!(
            out,
            json!({
                "instructions": "", "stream": false, "reasoning": {"effort": "high"},
                "parallel_tool_calls": true, "include": ["reasoning.encrypted_content"], "model": "gpt-5.5",
                "input": [{"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hi"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
                ]}],
                "text": {"format": {"type": "json_schema", "name": "out", "strict": true, "schema": schema}, "verbosity": "low"},
                "tool_choice": "auto",
                "store": false
            })
        );
        let out = req(
            json!({"messages": [], "text": {"verbosity": "high"}, "tool_choice": {"type": "web_search"}}),
        );
        assert_eq!(out["text"], json!({"verbosity": "high"}));
        assert_eq!(out["tool_choice"], json!({"type": "web_search"}));
        let out = req(json!({"messages": [], "response_format": {"type": "text"}}));
        assert_eq!(out["text"], json!({"format": {"type": "text"}}));
    }

    // ── response: ports of codex_openai_response_test.go + noop_optimization_test.go ──

    // port of TestConvertCodexResponseToOpenAINonStreamKeepsAssistantRole
    #[test]
    fn non_stream_keeps_assistant_role() {
        let out = translate_non_stream(
            &json!({"type": "response.completed", "response": {"status": "completed", "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hello"}]}
            ]}}),
            &Value::Null,
        );
        let created = out["created"].clone();
        assert!(created.as_i64().unwrap() > 1_700_000_000);
        assert_eq!(
            out,
            json!({
                "id": "", "object": "chat.completion", "created": created, "model": "model",
                "choices": [{"index": 0,
                    "message": {"role": "assistant", "content": "hello", "reasoning_content": null, "tool_calls": null},
                    "finish_reason": "stop", "native_finish_reason": "stop"}]
            })
        );
    }

    // port of TestConvertCodexResponseToOpenAI_IncompleteTerminal
    #[test]
    fn incomplete_terminal() {
        let terminal = json!({"type": "response.incomplete", "response": {"id": "resp_1", "model": "gpt-5.5", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}, "output": [],
            "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}}});
        let req_model = json!({"model": "gpt-5.5"});

        let mut t = StreamTranslator::new(&req_model);
        assert_eq!(
            push(&mut t, terminal.clone()),
            vec![json!({
                "id": "", "object": "chat.completion.chunk", "created": 0, "model": "gpt-5.5",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "length", "native_finish_reason": "max_output_tokens"}],
                "usage": {"completion_tokens": 2, "total_tokens": 3, "prompt_tokens": 1}
            })]
        );

        let mut t = StreamTranslator::new(&req_model);
        push(
            &mut t,
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_1", "name": "lookup"}}),
        );
        assert_eq!(
            push(&mut t, terminal.clone())[0]["choices"][0]["finish_reason"],
            json!("length")
        );

        let out = translate_non_stream(&terminal, &req_model);
        assert_eq!(out["choices"][0]["finish_reason"], json!("length"));
        assert_eq!(
            out["choices"][0]["native_finish_reason"],
            json!("max_output_tokens")
        );
        // the bare `response` object is accepted too
        assert_eq!(translate_non_stream(&terminal["response"], &req_model), out);
    }

    // port of TestConvertCodexResponseToOpenAI_StreamSetsModelFromResponseCreated
    //       + TestConvertCodexResponseToOpenAI_FirstChunkUsesRequestModelName
    #[test]
    fn stream_model_from_response_created_or_request() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.3-codex"}));
        assert!(push(&mut t, json!({"type": "response.created", "response": {"id": "resp_123", "created_at": 1700000000, "model": "gpt-5.3-codex-upstream"}})).is_empty());
        let mut want = ck(
            "gpt-5.3-codex-upstream",
            json!({"role": "assistant", "content": "hello"}),
        );
        want["id"] = json!("resp_123");
        want["created"] = json!(1700000000);
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_text.delta", "delta": "hello"})
            ),
            vec![want]
        );

        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.3-codex"}));
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_text.delta", "delta": "hello"})
            ),
            vec![ck(
                "gpt-5.3-codex",
                json!({"role": "assistant", "content": "hello"})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_ToolCallChunkOmitsNullContentFields
    //       + TestConvertCodexResponseToOpenAI_ToolCallArgumentsDeltaOmitsNullContentFields
    #[test]
    fn tool_call_chunks_omit_null_content_fields() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.4"}));
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_123", "name": "websearch"}})
            ),
            vec![ck(
                "gpt-5.4",
                json!({"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call_123", "type": "function", "function": {"name": "websearch", "arguments": ""}}
                ]})
            )]
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.function_call_arguments.delta", "delta": "{\"query\":\"OpenAI\"}"})
            ),
            vec![ck(
                "gpt-5.4",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"query\":\"OpenAI\"}"}}]})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_CustomToolCallStreamDeltas
    #[test]
    fn custom_tool_call_stream_deltas() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.added", "item": {"type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": "unexpected input"}})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call_apply", "type": "function", "function": {"name": "ApplyPatch", "arguments": ""}}
                ]})
            )]
        );
        for d in ["*** Begin Patch\n", "*** End Patch"] {
            assert_eq!(
                push(
                    &mut t,
                    json!({"type": "response.custom_tool_call_input.delta", "delta": d})
                ),
                vec![ck(
                    "gpt-5.5",
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": d}}]})
                )]
            );
        }
        let full = "*** Begin Patch\n*** End Patch";
        assert!(push(
            &mut t,
            json!({"type": "response.custom_tool_call_input.done", "input": full})
        )
        .is_empty());
        assert!(push(&mut t, json!({"type": "response.output_item.done", "item": {"type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": full}})).is_empty());
        let out = push(
            &mut t,
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
        );
        assert_eq!(
            out,
            vec![json!({
                "id": "", "object": "chat.completion.chunk", "created": 0, "model": "gpt-5.5",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls", "native_finish_reason": "tool_calls"}],
                "usage": {"completion_tokens": 1, "total_tokens": 2, "prompt_tokens": 1}
            })]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_EmptyCustomToolDeltaUsesDoneFallback
    #[test]
    fn empty_custom_tool_delta_uses_done_fallback() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        push(
            &mut t,
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "ctc_1", "type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": ""}}),
        );
        assert!(push(&mut t, json!({"type": "response.custom_tool_call_input.delta", "item_id": "ctc_1", "output_index": 0, "delta": ""})).is_empty());
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.custom_tool_call_input.done", "item_id": "ctc_1", "output_index": 0, "input": "full patch"})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "full patch"}}]})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_InterleavedToolCallsKeepStateByItem
    #[test]
    fn interleaved_tool_calls_keep_state_by_item() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        let out = push(
            &mut t,
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_lookup", "name": "lookup", "arguments": ""}}),
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
        let out = push(
            &mut t,
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "ctc_2", "type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": ""}}),
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(1)
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0, "delta": "{\"query\":"})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"query\":"}}]})
            )]
        );
        assert!(push(
            &mut t,
            json!({"type": "response.custom_tool_call_input.delta", "output_index": 1, "delta": ""})
        )
        .is_empty());
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.custom_tool_call_input.done", "output_index": 1, "input": "patch"})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 1, "function": {"arguments": "patch"}}]})
            )]
        );
        for ev in [
            json!({"type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 0, "arguments": "{\"query\":\"test\"}"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_lookup", "name": "lookup", "arguments": "{\"query\":\"test\"}"}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"id": "ctc_2", "type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": "patch"}}),
        ] {
            assert!(push(&mut t, ev).is_empty());
        }
    }

    // port of TestConvertCodexResponseToOpenAI_CustomToolCallInputDoneFallback
    #[test]
    fn custom_tool_call_input_done_fallback() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        push(
            &mut t,
            json!({"type": "response.output_item.added", "item": {"type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": ""}}),
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.custom_tool_call_input.done", "input": "full patch"})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "full patch"}}]})
            )]
        );
        assert!(push(&mut t, json!({"type": "response.output_item.done", "item": {"type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": "full patch"}})).is_empty());
    }

    // port of TestConvertCodexResponseToOpenAI_ToolCallOutputItemDoneFallbacks
    #[test]
    fn tool_call_output_item_done_fallbacks() {
        // announced custom call emits arguments only
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        push(
            &mut t,
            json!({"type": "response.output_item.added", "item": {"type": "custom_tool_call", "call_id": "call_first", "name": "ApplyPatch", "input": ""}}),
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.done", "item": {"type": "custom_tool_call", "call_id": "call_first", "name": "ApplyPatch", "input": "first patch"}})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "first patch"}}]})
            )]
        );
        push(
            &mut t,
            json!({"type": "response.output_item.added", "item": {"type": "custom_tool_call", "call_id": "call_second", "name": "ApplyPatch", "input": ""}}),
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.done", "item": {"type": "custom_tool_call", "call_id": "call_second", "name": "ApplyPatch", "input": "second patch"}})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 1, "function": {"arguments": "second patch"}}]})
            )]
        );

        // unannounced custom call emits the complete call (tool_calls before role, as sjson writes it)
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        let frames = t.push(
            None,
            &json!({"type": "response.output_item.done", "item": {"type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": "full patch"}}),
        );
        assert_eq!(
            frames,
            vec![format!(
                "data: {}\n\n",
                ck(
                    "gpt-5.5",
                    json!({"tool_calls": [
                    {"index": 0, "id": "call_apply", "type": "function", "function": {"name": "ApplyPatch", "arguments": "full patch"}}
                ], "role": "assistant"})
                )
            )]
        );

        // announced function call still falls back
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.5"}));
        push(
            &mut t,
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_lookup", "name": "lookup", "arguments": ""}}),
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "call_lookup", "name": "lookup", "arguments": "{\"query\":\"test\"}"}})
            ),
            vec![ck(
                "gpt-5.5",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"query\":\"test\"}"}}]})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_ToolCallStateFallsBackFromUnknownItemID
    #[test]
    fn tool_call_state_falls_back_from_unknown_item_id() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.6-terra"}));
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "call_1", "name": "TaskCreate", "arguments": ""}})
            ),
            vec![ck(
                "gpt-5.6-terra",
                json!({"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call_1", "type": "function", "function": {"name": "TaskCreate", "arguments": ""}}
                ]})
            )]
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "TaskCreate", "arguments": "{\"subject\":\"test\"}"}})
            ),
            vec![ck(
                "gpt-5.6-terra",
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"subject\":\"test\"}"}}]})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAINonStream_CustomToolCall
    #[test]
    fn non_stream_custom_tool_call() {
        let out = translate_non_stream(
            &json!({"type": "response.completed", "response": {"id": "resp_123", "created_at": 1700000000, "model": "gpt-5.5", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                "output": [{"type": "custom_tool_call", "call_id": "call_apply", "name": "ApplyPatch", "input": "full patch"}]}}),
            &json!({"model": "gpt-5.5"}),
        );
        assert_eq!(
            out,
            json!({
                "id": "resp_123", "object": "chat.completion", "created": 1700000000, "model": "gpt-5.5",
                "choices": [{"index": 0,
                    "message": {"role": "assistant", "content": null, "reasoning_content": null, "tool_calls": [
                        {"id": "call_apply", "type": "function", "function": {"name": "ApplyPatch", "arguments": "full patch"}}
                    ]},
                    "finish_reason": "tool_calls", "native_finish_reason": "tool_calls"}],
                "usage": {"completion_tokens": 1, "total_tokens": 2, "prompt_tokens": 1}
            })
        );
    }

    // port of TestConvertCodexResponseToOpenAI_StreamPartialImageEmitsDeltaImages
    //       + TestConvertCodexResponseToOpenAI_StreamImageGenerationCallDoneEmitsDeltaImages
    #[test]
    fn stream_images() {
        let mut t = StreamTranslator::new(&json!({"model": "gpt-5.4"}));
        let partial = json!({"type": "response.image_generation_call.partial_image", "item_id": "ig_123", "output_format": "png", "partial_image_b64": "aGVsbG8=", "partial_image_index": 0});
        assert_eq!(
            push(&mut t, partial.clone()),
            vec![ck(
                "gpt-5.4",
                json!({"images": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}, "index": 0}], "role": "assistant"})
            )]
        );
        // a duplicate partial and an identical final image are suppressed
        assert!(push(&mut t, partial).is_empty());
        assert!(push(&mut t, json!({"type": "response.output_item.done", "item": {"id": "ig_123", "type": "image_generation_call", "output_format": "png", "result": "aGVsbG8="}})).is_empty());
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.done", "item": {"id": "ig_123", "type": "image_generation_call", "output_format": "jpeg", "result": "Ymll"}})
            ),
            vec![ck(
                "gpt-5.4",
                json!({"images": [{"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,Ymll"}, "index": 0}], "role": "assistant"})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_NonStreamImageGenerationCallAddsMessageImages
    #[test]
    fn non_stream_image_generation_call_adds_message_images() {
        let out = translate_non_stream(
            &json!({"type": "response.completed", "response": {"id": "resp_123", "created_at": 1700000000, "model": "gpt-5.4", "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]},
                    {"type": "image_generation_call", "output_format": "png", "result": "aGVsbG8="}]}}),
            &Value::Null,
        );
        assert_eq!(
            out["choices"][0]["message"],
            json!({"role": "assistant", "content": "ok", "reasoning_content": null, "tool_calls": null,
                "images": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}, "index": 0}]})
        );
    }

    fn usage_with_cache_write(cache_write: Option<Value>) -> Value {
        let mut details = json!({"cached_tokens": 30});
        if let Some(v) = cache_write {
            details["cache_write_tokens"] = v;
        }
        json!({"input_tokens": 100, "output_tokens": 20, "total_tokens": 120, "input_tokens_details": details, "output_tokens_details": {"reasoning_tokens": 5}})
    }

    fn expected_usage(cache_write: Option<Value>) -> Value {
        let mut prompt_details = json!({"cached_tokens": 30});
        if let Some(v) = cache_write {
            prompt_details["cache_write_tokens"] = v.clone();
            prompt_details["cached_creation_tokens"] = v;
        }
        json!({"completion_tokens": 20, "total_tokens": 120, "prompt_tokens": 100,
            "prompt_tokens_details": prompt_details, "completion_tokens_details": {"reasoning_tokens": 5}})
    }

    // port of TestConvertCodexResponseToOpenAI_{Stream,NonStream}{ForwardsCacheWriteTokens,
    //       OmitsMissingCacheWriteTokens,PreservesExplicitZeroCacheWriteTokens}
    #[test]
    fn cache_write_tokens_stream_and_non_stream() {
        for cw in [Some(json!(40)), None, Some(json!(0))] {
            let usage = usage_with_cache_write(cw.clone());
            let mut t = StreamTranslator::new(&json!({"model": "gpt-5.4"}));
            push(
                &mut t,
                json!({"type": "response.created", "response": {"id": "resp_123", "created_at": 1700000000, "model": "gpt-5.4"}}),
            );
            let out = push(
                &mut t,
                json!({"type": "response.completed", "response": {"id": "resp_123", "created_at": 1700000000, "model": "gpt-5.4", "usage": usage}}),
            );
            assert_eq!(
                out,
                vec![json!({
                    "id": "resp_123", "object": "chat.completion.chunk", "created": 1700000000, "model": "gpt-5.4",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop", "native_finish_reason": "stop"}],
                    "usage": expected_usage(cw.clone())
                })]
            );

            let out = translate_non_stream(
                &json!({"type": "response.completed", "response": {"id": "resp_123", "created_at": 1700000000, "model": "gpt-5.4", "status": "completed",
                    "usage": usage, "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]}}),
                &Value::Null,
            );
            assert_eq!(out["usage"], expected_usage(cw));
        }
    }

    // port of TestConvertCodexResponseToOpenAI_NonStreamMultiMessageEmptyTrailingKeepsContent
    #[test]
    fn non_stream_multi_message_empty_trailing_keeps_content() {
        let out = translate_non_stream(
            &json!({"type": "response.completed", "response": {"id": "resp_1", "created_at": 1700000000, "model": "gpt-5.5", "status": "completed",
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "the real answer"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking again"}]},
                {"type": "message", "content": [{"type": "output_text", "text": ""}]}
            ]}}),
            &Value::Null,
        );
        assert_eq!(
            out["choices"][0]["message"],
            json!({"role": "assistant", "content": "the real answer", "reasoning_content": "thinkingthinking again", "tool_calls": null})
        );
    }

    // port of TestConvertCodexResponseToOpenAI_StreamReasoningTextDeltaAndDone
    #[test]
    fn stream_reasoning_text_delta_and_done() {
        let mut t = StreamTranslator::new(&json!({"model": "MiniMax-M3"}));
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.reasoning_text.delta", "delta": "Thinking step 1"})
            ),
            vec![ck(
                "MiniMax-M3",
                json!({"role": "assistant", "reasoning_content": "Thinking step 1"})
            )]
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.reasoning_text.done", "text": "Thinking step 1"})
            ),
            vec![ck(
                "MiniMax-M3",
                json!({"role": "assistant", "reasoning_content": "\n\n"})
            )]
        );
    }

    // port of TestConvertCodexResponseToOpenAI_NonStreamReasoningTextContent
    //       + TestConvertCodexResponseToOpenAI_NonStreamReasoningSummaryAndContent
    #[test]
    fn non_stream_reasoning_text_content_and_summary() {
        let run = |reasoning: Value| {
            translate_non_stream(
                &json!({"type": "response.completed", "response": {"id": "resp_1", "created_at": 1700000000, "model": "MiniMax-M3", "status": "completed",
                    "output": [reasoning, {"type": "message", "content": [{"type": "output_text", "text": "Answer"}]}]}}),
                &Value::Null,
            )["choices"][0]["message"]["reasoning_content"]
                .clone()
        };
        assert_eq!(
            run(
                json!({"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "Full reasoning from MiniMax"}]})
            ),
            json!("Full reasoning from MiniMax")
        );
        assert_eq!(
            run(
                json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Summary part"}], "content": [{"type": "reasoning_text", "text": " and Content part"}]})
            ),
            json!("Summary part and Content part")
        );
    }

    // port of TestConvertCodexResponseToOpenAI_Issue5543_CacheWriteTokensAndServiceTier
    // (minus the "beyond int64" subcase — see set_codex_cache_write_tokens)
    #[test]
    fn issue5543_cache_write_tokens_and_service_tier() {
        let raw = json!({"type": "response.completed", "response": {"id": "resp_example", "model": "example-model", "service_tier": "default", "output": [],
            "usage": {"input_tokens": 7378, "output_tokens": 6, "total_tokens": 7384, "input_tokens_details": {"cached_tokens": 7168, "cache_write_tokens": 128}}}});
        let usage = json!({"completion_tokens": 6, "total_tokens": 7384, "prompt_tokens": 7378,
            "prompt_tokens_details": {"cached_tokens": 7168, "cache_write_tokens": 128, "cached_creation_tokens": 128}});

        // non-stream retains cache_write_tokens and service_tier
        let out = translate_non_stream(&raw, &Value::Null);
        assert_eq!(out["service_tier"], json!("default"));
        assert_eq!(out["usage"], usage);

        // streaming terminal chunk retains them
        let mut t = StreamTranslator::new(&json!({"model": "example-model"}));
        assert_eq!(
            push(&mut t, raw.clone()),
            vec![json!({
                "id": "", "object": "chat.completion.chunk", "created": 0, "model": "example-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop", "native_finish_reason": "stop"}],
                "service_tier": "default", "usage": usage
            })]
        );

        // response.created carries the tier over to deltas; a terminal without one keeps it
        let mut t = StreamTranslator::new(&json!({"model": "example-model"}));
        assert!(push(&mut t, json!({"type": "response.created", "response": {"id": "resp_stream", "created_at": 1700000000, "model": "example-model", "service_tier": "priority"}})).is_empty());
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_text.delta", "delta": "hello"})
            )[0]["service_tier"],
            json!("priority")
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.completed", "response": {"id": "resp_stream", "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}})
            )[0]["service_tier"],
            json!("priority")
        );

        // in_progress updates the tier, completed overrides it
        let mut t = StreamTranslator::new(&json!({"model": "example-model"}));
        push(
            &mut t,
            json!({"type": "response.created", "response": {"id": "resp_seq", "created_at": 1700000000, "model": "example-model", "service_tier": "default"}}),
        );
        assert!(push(&mut t, json!({"type": "response.in_progress", "response": {"id": "resp_seq", "service_tier": "priority"}})).is_empty());
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_text.delta", "delta": "hi"})
            )[0]["service_tier"],
            json!("priority")
        );
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.completed", "response": {"id": "resp_seq", "service_tier": "scale", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})
            )[0]["service_tier"],
            json!("scale")
        );

        // large integer (2^53 + 1) preserved exactly
        let large: Value = serde_json::from_str("9007199254740993").unwrap();
        let raw_large = json!({"type": "response.completed", "response": {"id": "resp_large", "model": "example-model",
            "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120, "input_tokens_details": {"cache_write_tokens": large}}}});
        let out = translate_non_stream(&raw_large, &Value::Null);
        assert_eq!(
            out["usage"]["prompt_tokens_details"].to_string(),
            r#"{"cache_write_tokens":9007199254740993,"cached_creation_tokens":9007199254740993}"#
        );
        let mut t = StreamTranslator::new(&Value::Null);
        let frames = t.push(None, &raw_large);
        assert!(frames[0].contains(
            r#""prompt_tokens_details":{"cache_write_tokens":9007199254740993,"cached_creation_tokens":9007199254740993}"#
        ));

        // invalid cache_write_tokens formats are rejected
        for bad in [json!(-1), json!(1.5), json!("128"), json!(true)] {
            let raw = json!({"type": "response.completed", "response": {"id": "resp_inv", "model": "example-model",
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "input_tokens_details": {"cache_write_tokens": bad}}}});
            let want = json!({"completion_tokens": 5, "total_tokens": 15, "prompt_tokens": 10});
            assert_eq!(translate_non_stream(&raw, &Value::Null)["usage"], want);
            let mut t = StreamTranslator::new(&Value::Null);
            assert_eq!(push(&mut t, raw)[0]["usage"], want);
        }

        // invalid or whitespace service_tier is ignored
        for tier in [
            json!("   "),
            json!(""),
            json!(123),
            json!(true),
            Value::Null,
        ] {
            let raw = json!({"type": "response.completed", "response": {"id": "resp_tier", "model": "example-model", "service_tier": tier,
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}});
            assert!(translate_non_stream(&raw, &Value::Null)
                .get("service_tier")
                .is_none());
        }
    }

    // port of TestConvertCodexResponseToOpenAI_RestoresNormalizedToolNames
    #[test]
    fn restores_normalized_tool_names() {
        let original = json!({"tools": [{"type": "function", "function": {"name": "mcp.server:search tool"}}]});
        let out = translate_non_stream(
            &json!({"type": "response.completed", "response": {"id": "resp_1", "created_at": 1700000000, "model": "gpt-5.6-sol", "status": "completed",
                "output": [{"type": "function_call", "call_id": "call_1", "name": "mcp_server_search_tool", "arguments": "{}"}]}}),
            &original,
        );
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"],
            json!([{"id": "call_1", "type": "function", "function": {"name": "mcp.server:search tool", "arguments": "{}"}}])
        );

        let mut t = StreamTranslator::new(&original);
        assert_eq!(
            push(
                &mut t,
                json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_1", "name": "mcp_server_search_tool"}})
            ),
            // no model in the original request and no response.created: the template's
            // placeholder stays, as in Go
            vec![ck(
                "model",
                json!({"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call_1", "type": "function", "function": {"name": "mcp.server:search tool", "arguments": ""}}
                ]})
            )]
        );
    }

    // ── end to end: a realistic Codex SSE sequence (reasoning + text + a function call) ──

    #[test]
    fn stream_end_to_end_reasoning_text_and_function_call() {
        let original = json!({
            "model": "gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "Weather in Paris?"}],
            "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}]
        });
        let resp = |status: &str| json!({"id": "resp_e2e", "object": "response", "created_at": 1759300000, "model": "gpt-5.5-codex", "status": status, "output": []});
        let mut completed = resp("completed");
        completed["usage"] = json!({"input_tokens": 50, "input_tokens_details": {"cached_tokens": 10},
            "output_tokens": 30, "output_tokens_details": {"reasoning_tokens": 12}, "total_tokens": 80});
        let fc_item = json!({"id": "fc_1", "type": "function_call", "status": "in_progress", "arguments": "", "call_id": "call_w1", "name": "get_weather"});
        let mut fc_done = fc_item.clone();
        fc_done["status"] = json!("completed");
        fc_done["arguments"] = json!("{\"city\":\"Paris\"}");

        let events = vec![
            json!({"type": "response.created", "sequence_number": 0, "response": resp("in_progress")}),
            json!({"type": "response.in_progress", "sequence_number": 1, "response": resp("in_progress")}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "rs_1", "type": "reasoning", "summary": []}}),
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "Need the "}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "weather."}),
            json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "text": "Need the weather."}),
            json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": "Need the weather."}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "rs_1", "type": "reasoning", "encrypted_content": "gAAA", "summary": [{"type": "summary_text", "text": "Need the weather."}]}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
            json!({"type": "response.content_part.added", "item_id": "msg_1", "output_index": 1, "content_index": 0, "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": "Checking "}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": "Paris."}),
            json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 1, "content_index": 0, "text": "Checking Paris."}),
            json!({"type": "response.content_part.done", "item_id": "msg_1", "output_index": 1, "content_index": 0, "part": {"type": "output_text", "text": "Checking Paris."}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking Paris."}]}}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": fc_item}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 2, "delta": "{\"city\":"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 2, "delta": "\"Paris\"}"}),
            json!({"type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 2, "arguments": "{\"city\":\"Paris\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": fc_done}),
            json!({"type": "response.completed", "sequence_number": 22, "response": completed}),
        ];

        let mut t = StreamTranslator::new(&original);
        let mut frames: Vec<String> = Vec::new();
        for ev in &events {
            frames.extend(t.push(ev["type"].as_str(), ev));
        }

        let chunk = |delta: Value| {
            json!({
                "id": "resp_e2e", "object": "chat.completion.chunk", "created": 1759300000, "model": "gpt-5.5-codex",
                "choices": [{"index": 0, "delta": delta, "finish_reason": null, "native_finish_reason": null}],
            })
        };
        let expected = [
            chunk(json!({"role": "assistant", "reasoning_content": "Need the "})),
            chunk(json!({"role": "assistant", "reasoning_content": "weather."})),
            chunk(json!({"role": "assistant", "reasoning_content": "\n\n"})),
            chunk(json!({"role": "assistant", "content": "Checking "})),
            chunk(json!({"role": "assistant", "content": "Paris."})),
            chunk(json!({"role": "assistant", "tool_calls": [
                {"index": 0, "id": "call_w1", "type": "function", "function": {"name": "get_weather", "arguments": ""}}
            ]})),
            chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]})),
            chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Paris\"}"}}]})),
            json!({
                "id": "resp_e2e", "object": "chat.completion.chunk", "created": 1759300000, "model": "gpt-5.5-codex",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls", "native_finish_reason": "tool_calls"}],
                "usage": {"completion_tokens": 30, "total_tokens": 80, "prompt_tokens": 50,
                    "prompt_tokens_details": {"cached_tokens": 10}, "completion_tokens_details": {"reasoning_tokens": 12}}
            }),
        ];
        // exact frame text (key order included), then exact JSON
        let expected_frames: Vec<String> =
            expected.iter().map(|c| format!("data: {c}\n\n")).collect();
        assert_eq!(frames, expected_frames);
        let parsed: Vec<Value> = frames
            .iter()
            .map(|f| serde_json::from_str(&f["data: ".len()..f.len() - 2]).unwrap())
            .collect();
        assert_eq!(parsed, expected);

        assert_eq!(t.finish(), vec!["data: [DONE]\n\n".to_string()]);
        assert!(t.finish().is_empty(), "[DONE] must be emitted exactly once");
    }
}
