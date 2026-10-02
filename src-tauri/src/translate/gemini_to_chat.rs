//! Gemini generateContent (client) ⇄ OpenAI Chat Completions (upstream) translator.
//!
//! Faithful port of CLIProxyAPI `internal/translator/openai/gemini/` at commit
//! `ed980be` (`openai_gemini_request.go`, `openai_gemini_response.go`), plus the
//! helpers they call in other packages (`translator/common`, `thinking`). Each
//! ported function carries a `// port of <GoFunc> (<file>)` comment so the two
//! can be diffed when upstream moves.
//!
//! The Go code works on raw bytes through gjson/sjson; this port works on
//! `serde_json::Value`. The gjson coercion rules the Go code relies on
//! (`.String()` / `.Int()` / `.Float()` / `.Bool()` on any JSON type,
//! `.Exists()` being true for an explicit `null`) are reproduced by the `g*`
//! helpers below so edge cases behave the same.
//!
//! Deliberately NOT ported: the `model(level)` thinking-suffix parsing, the
//! count_tokens response (`GeminiTokenCount`), metrics/logging and config hooks.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

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

/// gjson `Result.Bool()`: `true`, a non-zero number, or a string
/// `strconv.ParseBool` accepts after lower-casing.
fn gbool(v: Option<&Value>) -> bool {
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

fn sse_data(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

// ---------------------------------------------------------------------------
// helpers from other packages
// ---------------------------------------------------------------------------

// port of IsGeminiThoughtPart (translator/common/gemini.go)
fn is_gemini_thought_part(part: &Value) -> bool {
    gbool(gget(part, "thought"))
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

// ---------------------------------------------------------------------------
// Request: Gemini generateContent -> OpenAI Chat Completions
// ---------------------------------------------------------------------------

/// Client request (Gemini) -> upstream request (OpenAI Chat Completions).
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    convert_gemini_request_to_openai(model, body, stream)
}

// port of ConvertGeminiRequestToOpenAI (openai_gemini_request.go)
fn convert_gemini_request_to_openai(model_name: &str, root: &Value, stream: bool) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), Value::String(model_name.to_string()));
    out.insert("messages".into(), Value::Array(Vec::new()));

    // Generation config mapping
    if let Some(gen_config) = gget(root, "generationConfig") {
        if let Some(temp) = gget(gen_config, "temperature") {
            out.insert("temperature".into(), float_value(gfloat(Some(temp))));
        }
        if let Some(max_tokens) = gget(gen_config, "maxOutputTokens") {
            out.insert("max_tokens".into(), Value::from(gint(Some(max_tokens))));
        }
        if let Some(top_p) = gget(gen_config, "topP") {
            out.insert("top_p".into(), float_value(gfloat(Some(top_p))));
        }
        if let Some(top_k) = gget(gen_config, "topK") {
            out.insert("top_k".into(), Value::from(gint(Some(top_k))));
        }
        if let Some(Value::Array(stop_sequences)) = gget(gen_config, "stopSequences") {
            let stops: Vec<Value> = stop_sequences
                .iter()
                .map(|v| Value::String(gstr(Some(v))))
                .collect();
            if !stops.is_empty() {
                out.insert("stop".into(), Value::Array(stops));
            }
        }
        if let Some(candidate_count) = gget(gen_config, "candidateCount") {
            out.insert("n".into(), Value::from(gint(Some(candidate_count))));
        }
        if let Some(Value::Array(response_modalities)) = gget(gen_config, "responseModalities") {
            let mut modalities: Vec<Value> = Vec::new();
            for value in response_modalities {
                match gstr(Some(value)).trim().to_lowercase().as_str() {
                    "text" => modalities.push("text".into()),
                    "image" => modalities.push("image".into()),
                    "audio" => modalities.push("audio".into()),
                    _ => {}
                }
            }
            if !modalities.is_empty() {
                out.insert("modalities".into(), Value::Array(modalities));
            }
        }

        // Map Gemini thinkingConfig to OpenAI reasoning_effort (camelCase or
        // the Python SDK's snake_case).
        if let Some(thinking_config @ Value::Object(_)) = gget(gen_config, "thinkingConfig") {
            let thinking_level = gget(thinking_config, "thinkingLevel")
                .or_else(|| gget(thinking_config, "thinking_level"));
            if let Some(level) = thinking_level {
                let effort = gstr(Some(level)).trim().to_lowercase();
                if !effort.is_empty() {
                    out.insert("reasoning_effort".into(), Value::String(effort));
                }
            } else {
                let thinking_budget = gget(thinking_config, "thinkingBudget")
                    .or_else(|| gget(thinking_config, "thinking_budget"));
                if let Some(budget) = thinking_budget {
                    if let Some(effort) = convert_budget_to_level(gint(Some(budget))) {
                        out.insert("reasoning_effort".into(), Value::String(effort.into()));
                    }
                }
            }
        }
    }

    out.insert("stream".into(), Value::Bool(stream));
    if let Some(Value::String(service_tier)) = gget(root, "service_tier") {
        out.insert("service_tier".into(), Value::String(service_tier.clone()));
    }

    let mut message_items: Vec<Value> = Vec::new();
    // Track tool call IDs per function name for matching.
    let mut tool_call_ids_by_name: HashMap<String, Vec<String>> = HashMap::new();

    // System instruction -> OpenAI system message (either key spelling).
    let system_instruction =
        gget(root, "systemInstruction").or_else(|| gget(root, "system_instruction"));
    if let Some(system_instruction) = system_instruction {
        let mut content_items: Vec<Value> = Vec::new();
        if let Some(Value::Array(parts)) = gget(system_instruction, "parts") {
            for part in parts {
                if is_gemini_thought_part(part) {
                    continue;
                }
                if let Some(text) = gget(part, "text") {
                    content_items.push(json!({"type":"text","text": gstr(Some(text))}));
                }
                if let Some(content_part) = openai_content_part_from_gemini_inline_data(part) {
                    content_items.push(content_part);
                }
                if let Some(content_part) = openai_content_part_from_gemini_file_data(part) {
                    content_items.push(content_part);
                }
            }
        }
        if !content_items.is_empty() {
            message_items.push(json!({"role":"system","content": content_items}));
        }
    }

    if let Some(Value::Array(contents)) = gget(root, "contents") {
        for (msg_idx, content) in contents.iter().enumerate() {
            let mut role = gstr(gget(content, "role"));
            if role == "model" {
                role = "assistant".into();
            }

            let mut msg = Map::new();
            msg.insert("role".into(), Value::String(role));
            msg.insert("content".into(), Value::String(String::new()));

            let mut text_builder = String::new();
            let mut content_items: Vec<Value> = Vec::new();
            let mut only_text_content = true;
            let mut tool_call_items: Vec<Value> = Vec::new();
            let mut dropped_thought = false;

            if let Some(Value::Array(parts)) = gget(content, "parts") {
                for (current_part_idx, part) in parts.iter().enumerate() {
                    if is_gemini_thought_part(part) {
                        dropped_thought = true;
                        continue;
                    }

                    if let Some(text) = gget(part, "text") {
                        let formatted_text = gstr(Some(text));
                        text_builder.push_str(&formatted_text);
                        content_items.push(json!({"type":"text","text": formatted_text}));
                    }

                    if let Some(content_part) = openai_content_part_from_gemini_inline_data(part) {
                        only_text_content = false;
                        content_items.push(content_part);
                    }
                    if let Some(content_part) = openai_content_part_from_gemini_file_data(part) {
                        only_text_content = false;
                        content_items.push(content_part);
                    }

                    // functionCall (Gemini) -> tool call (OpenAI)
                    if let Some(function_call) = gget(part, "functionCall") {
                        let func_name = gstr(gget(function_call, "name"));
                        let args_raw = gget(function_call, "args")
                            .map(|a| a.to_string())
                            .unwrap_or_default();
                        let mut tool_call_id = explicit_gemini_tool_id(function_call);
                        if tool_call_id.is_empty() {
                            tool_call_id = deterministic_tool_call_id(
                                "call",
                                msg_idx,
                                current_part_idx,
                                &func_name,
                                &args_raw,
                            );
                        }
                        tool_call_ids_by_name
                            .entry(func_name.clone())
                            .or_default()
                            .push(tool_call_id.clone());

                        let arguments = if args_raw.is_empty() {
                            "{}".to_string()
                        } else {
                            args_raw
                        };
                        tool_call_items.push(json!({
                            "id": tool_call_id,
                            "type": "function",
                            "function": {"name": func_name, "arguments": arguments}
                        }));
                    }

                    // functionResponse (Gemini) -> tool role message (OpenAI)
                    if let Some(function_response) = gget(part, "functionResponse") {
                        let func_name = gstr(gget(function_response, "name"));
                        let mut tool_msg = Map::new();
                        tool_msg.insert("role".into(), "tool".into());
                        tool_msg.insert("tool_call_id".into(), "".into());
                        tool_msg.insert("content".into(), "".into());

                        let mut response_raw = String::new();
                        if let Some(response) = gget(function_response, "response") {
                            response_raw = match gget(response, "content") {
                                Some(content_field) => content_field.to_string(),
                                None => response.to_string(),
                            };
                            tool_msg.insert("content".into(), Value::String(response_raw.clone()));
                        }

                        let explicit = explicit_gemini_tool_id(function_response);
                        if !explicit.is_empty() {
                            tool_msg.insert("tool_call_id".into(), Value::String(explicit.clone()));
                            if let Some(queue) = tool_call_ids_by_name.get_mut(&func_name) {
                                if let Some(i) = queue.iter().position(|id| *id == explicit) {
                                    queue.remove(i);
                                }
                            }
                        } else if let Some(queue) = tool_call_ids_by_name
                            .get_mut(&func_name)
                            .filter(|q| !q.is_empty())
                        {
                            let tool_call_id = queue.remove(0);
                            tool_msg.insert("tool_call_id".into(), Value::String(tool_call_id));
                        } else {
                            // Deterministic fallback when no call is available.
                            let fallback_id = deterministic_tool_call_id(
                                "response",
                                msg_idx,
                                current_part_idx,
                                &func_name,
                                &response_raw,
                            );
                            tool_msg.insert("tool_call_id".into(), Value::String(fallback_id));
                        }

                        message_items.push(Value::Object(tool_msg));
                    }
                }
            }

            let has_content = !content_items.is_empty();
            let has_tool_calls = !tool_call_items.is_empty();
            if has_content {
                if only_text_content {
                    msg.insert("content".into(), Value::String(text_builder));
                } else {
                    msg.insert("content".into(), Value::Array(content_items));
                }
            }
            if has_tool_calls {
                msg.insert("tool_calls".into(), Value::Array(tool_call_items));
            }

            if dropped_thought && !has_content && !has_tool_calls {
                continue;
            }

            message_items.push(Value::Object(msg));
        }
    }
    // port of SetRawArrayItems (translator/common/bytes.go): no items leaves `[]`.
    if !message_items.is_empty() {
        out.insert("messages".into(), Value::Array(message_items));
    }

    // Tools mapping: Gemini functionDeclarations -> OpenAI tools
    if let Some(Value::Array(tools)) = gget(root, "tools") {
        let mut tool_items: Vec<Value> = Vec::new();
        for tool in tools {
            if let Some(Value::Array(function_declarations)) = gget(tool, "functionDeclarations") {
                for func_decl in function_declarations {
                    let mut function = Map::new();
                    function.insert("name".into(), Value::String(gstr(gget(func_decl, "name"))));
                    function.insert(
                        "description".into(),
                        Value::String(gstr(gget(func_decl, "description"))),
                    );
                    if let Some(parameters) = gget(func_decl, "parameters")
                        .or_else(|| gget(func_decl, "parametersJsonSchema"))
                    {
                        function.insert("parameters".into(), parameters.clone());
                    }
                    tool_items.push(json!({"type":"function","function": Value::Object(function)}));
                }
            }
        }
        if !tool_items.is_empty() {
            out.insert("tools".into(), Value::Array(tool_items));
        }
    }

    // Tool choice mapping
    if let Some(tool_config) = gget(root, "toolConfig") {
        if let Some(function_calling_config) = gget(tool_config, "functionCallingConfig") {
            let mode = gstr(gget(function_calling_config, "mode"));
            let allowed_names = gget(function_calling_config, "allowedFunctionNames");
            match mode.as_str() {
                "NONE" => {
                    out.insert("tool_choice".into(), "none".into());
                }
                "AUTO" => {
                    out.insert("tool_choice".into(), "auto".into());
                }
                "ANY" => match allowed_names {
                    Some(Value::Array(names)) if names.len() == 1 => {
                        out.insert(
                            "tool_choice".into(),
                            json!({"type":"function","function":{"name": gstr(Some(&names[0]))}}),
                        );
                    }
                    _ => {
                        out.insert("tool_choice".into(), "required".into());
                    }
                },
                _ => {}
            }
        }
    }

    Value::Object(out)
}

// port of deterministicToolCallID (openai_gemini_request.go)
fn deterministic_tool_call_id(
    kind: &str,
    msg_idx: usize,
    part_idx: usize,
    name: &str,
    payload: &str,
) -> String {
    let sum = Sha256::digest(format!("{kind}|{msg_idx}|{part_idx}|{name}|{payload}").as_bytes());
    let mut id = String::from("call_");
    for b in &sum[..12] {
        id.push_str(&format!("{b:02x}"));
    }
    id
}

// port of explicitGeminiToolID (openai_gemini_request.go)
fn explicit_gemini_tool_id(node: &Value) -> String {
    for key in ["id", "call_id"] {
        let id = gstr(gget(node, key)).trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    gstr(gget(node, "callId")).trim().to_string()
}

// port of openAIContentPartFromGeminiInlineData (openai_gemini_request.go)
fn openai_content_part_from_gemini_inline_data(part: &Value) -> Option<Value> {
    let inline_data = gget(part, "inlineData").or_else(|| gget(part, "inline_data"))?;
    let mut mime_type = gstr(gget(inline_data, "mimeType"));
    if mime_type.is_empty() {
        mime_type = gstr(gget(inline_data, "mime_type"));
    }
    if mime_type.is_empty() {
        mime_type = "application/octet-stream".into();
    }
    let data = gstr(gget(inline_data, "data"));
    if data.is_empty() {
        return None;
    }
    let data_url = format!("data:{mime_type};base64,{data}");
    let lower = mime_type.to_lowercase();
    Some(if lower.starts_with("image/") {
        json!({"type":"image_url","image_url":{"url": data_url}})
    } else if lower.starts_with("audio/") {
        json!({"type":"input_audio","input_audio":{
            "data": data,
            "format": openai_input_audio_format_from_mime(&mime_type)
        }})
    } else if lower.starts_with("video/") {
        json!({"type":"video_url","video_url":{"url": data_url}})
    } else {
        json!({"type":"file","file":{
            "filename": openai_file_name_from_mime(&mime_type),
            "file_data": data
        }})
    })
}

// port of openAIContentPartFromGeminiFileData (openai_gemini_request.go)
fn openai_content_part_from_gemini_file_data(part: &Value) -> Option<Value> {
    let file_data = gget(part, "fileData").or_else(|| gget(part, "file_data"))?;
    let mut file_uri = gstr(gget(file_data, "fileUri"));
    if file_uri.is_empty() {
        file_uri = gstr(gget(file_data, "file_uri"));
    }
    if file_uri.is_empty() {
        return None;
    }
    let mut mime_type = gstr(gget(file_data, "mimeType"));
    if mime_type.is_empty() {
        mime_type = gstr(gget(file_data, "mime_type"));
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        return Some(json!({"type":"image_url","image_url":{"url": file_uri}}));
    }
    if lower.starts_with("video/") {
        return Some(json!({"type":"video_url","video_url":{"url": file_uri}}));
    }
    if lower.starts_with("application/") || lower.starts_with("text/") {
        return Some(json!({"type":"file","file":{
            "filename": openai_file_name_from_mime(&mime_type),
            "file_url": file_uri
        }}));
    }
    let mut file_info = format!("File: {file_uri}");
    if !mime_type.is_empty() {
        file_info.push_str(&format!(" (Type: {mime_type})"));
    }
    Some(json!({"type":"text","text": file_info}))
}

// port of openAIInputAudioFormatFromMIME (openai_gemini_request.go)
fn openai_input_audio_format_from_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

// port of openAIFileNameFromMIME (openai_gemini_request.go)
fn openai_file_name_from_mime(mime_type: &str) -> &'static str {
    let lower = mime_type.trim().to_lowercase();
    match lower.as_str() {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "application/xml" | "text/xml" => "document.xml",
        _ if lower.starts_with("video/") => "video",
        _ => "document",
    }
}

// ---------------------------------------------------------------------------
// Response: OpenAI Chat Completions -> Gemini
// ---------------------------------------------------------------------------

// port of ToolCallAccumulator (openai_gemini_response.go)
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// port of ConvertOpenAIResponseToGeminiParams (openai_gemini_response.go)
///
/// The Go `ContentAccumulator` is write-only (never read) and is not kept.
pub struct StreamTranslator {
    /// Keyed by the OpenAI tool index. Go uses a `map[int]`, whose iteration
    /// order is random; a BTreeMap emits the calls in index order.
    tool_calls_accumulator: BTreeMap<i64, ToolCallAccumulator>,
    /// Go initialises this to false and never sets it true, so the role-only
    /// branch it guards is dead upstream too; kept for a faithful diff.
    is_first_chunk: bool,
}

impl StreamTranslator {
    /// `original_request` = the client's original body (unused by this pair).
    pub fn new(_original_request: &Value) -> Self {
        Self {
            tool_calls_accumulator: BTreeMap::new(),
            is_first_chunk: false,
        }
    }

    /// One upstream Chat Completions chunk -> zero or more Gemini SSE frames.
    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert_openai_response_to_gemini(data)
            .iter()
            .map(sse_data)
            .collect()
    }

    /// Upstream stream ended. The Go translator answers `[DONE]` with nothing;
    /// a Gemini stream has no terminal sentinel.
    pub fn finish(&mut self) -> Vec<String> {
        Vec::new()
    }

    // port of ConvertOpenAIResponseToGemini (openai_gemini_response.go)
    fn convert_openai_response_to_gemini(&mut self, root: &Value) -> Vec<Value> {
        let Some(Value::Array(choices)) = gget(root, "choices") else {
            return Vec::new();
        };

        // Empty choices array: a usage-only chunk.
        if choices.is_empty() {
            if let Some(usage) = gget(root, "usage") {
                let mut template = Map::new();
                template.insert("candidates".into(), Value::Array(Vec::new()));
                template.insert("usageMetadata".into(), Value::Object(Map::new()));
                if let Some(model) = gget(root, "model") {
                    template.insert("model".into(), Value::String(gstr(Some(model))));
                }
                set_gemini_usage_metadata_from_openai_usage(&mut template, usage);
                return vec![Value::Object(template)];
            }
            return Vec::new();
        }

        let mut results: Vec<Value> = Vec::new();

        for choice in choices {
            let mut template = base_gemini_template(root);
            let delta = gget(choice, "delta");
            let base_template = template.clone();

            // Role (only in the first chunk — never reached, see is_first_chunk).
            if let Some(role) = delta.and_then(|d| gget(d, "role")) {
                if self.is_first_chunk {
                    if gstr(Some(role)) == "assistant" {
                        set_candidate_field(&mut template, "role", "model".into());
                    }
                    self.is_first_chunk = false;
                    results.push(Value::Object(template));
                    continue;
                }
            }

            let mut chunk_outputs: Vec<Value> = Vec::new();

            // Reasoning/thinking delta
            if let Some(reasoning) = delta.and_then(|d| gget(d, "reasoning_content")) {
                for reasoning_text in extract_reasoning_texts(Some(reasoning)) {
                    if reasoning_text.is_empty() {
                        continue;
                    }
                    let mut t = base_template.clone();
                    push_candidate_part(&mut t, json!({"thought": true, "text": reasoning_text}));
                    chunk_outputs.push(Value::Object(t));
                }
            }

            // Content delta
            if let Some(content) = delta.and_then(|d| gget(d, "content")) {
                let content_text = gstr(Some(content));
                if !content_text.is_empty() {
                    let mut t = base_template.clone();
                    push_candidate_part(&mut t, json!({"text": content_text}));
                    chunk_outputs.push(Value::Object(t));
                }
            }

            if !chunk_outputs.is_empty() {
                results.extend(chunk_outputs);
                continue;
            }

            // Tool call deltas: accumulate, emit nothing until finish_reason.
            if let Some(Value::Array(tool_calls)) = delta.and_then(|d| gget(d, "tool_calls")) {
                for tool_call in tool_calls {
                    let tool_index = gint(gget(tool_call, "index"));
                    let tool_id = gstr(gget(tool_call, "id"));
                    let tool_type = gstr(gget(tool_call, "type"));

                    if !tool_type.is_empty() && tool_type != "function" {
                        continue;
                    }
                    let Some(function) = gget(tool_call, "function") else {
                        continue;
                    };

                    let function_name = gstr(gget(function, "name"));
                    let function_args = gstr(gget(function, "arguments"));

                    let acc = self
                        .tool_calls_accumulator
                        .entry(tool_index)
                        .or_insert_with(|| ToolCallAccumulator {
                            id: tool_id.clone(),
                            name: function_name.clone(),
                            arguments: String::new(),
                        });
                    if !tool_id.is_empty() {
                        acc.id = tool_id;
                    }
                    if !function_name.is_empty() {
                        acc.name = function_name;
                    }
                    if !function_args.is_empty() {
                        acc.arguments.push_str(&function_args);
                    }
                }
                continue;
            }

            // Finish reason
            if let Some(Value::String(finish_reason)) = gget(choice, "finish_reason") {
                if !finish_reason.is_empty() {
                    set_candidate_field(
                        &mut template,
                        "finishReason",
                        map_openai_finish_reason_to_gemini(finish_reason).into(),
                    );

                    if !self.tool_calls_accumulator.is_empty() {
                        for accumulator in self.tool_calls_accumulator.values() {
                            let mut function_call = Map::new();
                            if !accumulator.id.is_empty() {
                                function_call
                                    .insert("id".into(), Value::String(accumulator.id.clone()));
                            }
                            function_call
                                .insert("name".into(), Value::String(accumulator.name.clone()));
                            function_call.insert(
                                "args".into(),
                                parse_args_to_object(&accumulator.arguments),
                            );
                            push_candidate_part(
                                &mut template,
                                json!({"functionCall": Value::Object(function_call)}),
                            );
                        }
                        self.tool_calls_accumulator = BTreeMap::new();
                    }

                    results.push(Value::Object(template));
                    continue;
                }
            }

            // Usage information
            if let Some(usage) = gget(root, "usage") {
                set_gemini_usage_metadata_from_openai_usage(&mut template, usage);
                results.push(Value::Object(template));
                continue;
            }
        }
        results
    }
}

/// `{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}` plus
/// the chunk's `model` when present (both response directions start here).
fn base_gemini_template(root: &Value) -> Map<String, Value> {
    let mut template = Map::new();
    template.insert(
        "candidates".into(),
        json!([{"content":{"parts":[],"role":"model"},"index":0}]),
    );
    if let Some(model) = gget(root, "model") {
        template.insert("model".into(), Value::String(gstr(Some(model))));
    }
    template
}

fn candidate0(template: &mut Map<String, Value>) -> &mut Map<String, Value> {
    template
        .get_mut("candidates")
        .and_then(|c| c.get_mut(0))
        .and_then(|c| c.as_object_mut())
        .expect("template always carries candidates[0]")
}

/// sjson `candidates.0.<field>` (content.role goes under content).
fn set_candidate_field(template: &mut Map<String, Value>, field: &str, value: Value) {
    let c = candidate0(template);
    if field == "role" {
        if let Some(content) = c.get_mut("content").and_then(|v| v.as_object_mut()) {
            content.insert("role".into(), value);
        }
    } else {
        c.insert(field.into(), value);
    }
}

/// sjson `candidates.0.content.parts.<next>` = part.
fn push_candidate_part(template: &mut Map<String, Value>, part: Value) {
    if let Some(Value::Array(parts)) = candidate0(template)
        .get_mut("content")
        .and_then(|c| c.get_mut("parts"))
    {
        parts.push(part);
    }
}

// port of mapOpenAIFinishReasonToGemini (openai_gemini_response.go)
fn map_openai_finish_reason_to_gemini(openai_reason: &str) -> &'static str {
    match openai_reason {
        "stop" => "STOP",
        "length" => "MAX_TOKENS",
        "tool_calls" => "STOP",
        "content_filter" => "SAFETY",
        _ => "STOP",
    }
}

// port of parseArgsToObjectRaw (openai_gemini_response.go)
fn parse_args_to_object(args_str: &str) -> Value {
    let trimmed = args_str.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return json!({});
    }
    if let Ok(strict @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
        return strict;
    }
    let tolerant = tolerant_parse_json_object(trimmed);
    if tolerant.as_object().is_some_and(|m| !m.is_empty()) {
        return tolerant;
    }
    json!({})
}

fn is_ws(r: char) -> bool {
    r == ' ' || r == '\n' || r == '\r' || r == '\t'
}

// port of tolerantParseJSONObjectRaw (openai_gemini_response.go)
//
// Tolerates bareword values (`{"location": 北京, "unit": celsius}`) seen in
// streamed tool calls. Keys are set literally; Go escapes them for an sjson
// path (escapeSjsonPathKey), which is the same effect.
fn tolerant_parse_json_object(s: &str) -> Value {
    let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) else {
        return json!({});
    };
    if start >= end {
        return json!({});
    }
    let runes: Vec<char> = s[start + 1..end].chars().collect();
    let n = runes.len();
    let mut i = 0;
    let mut result = Map::new();

    while i < n {
        while i < n && (is_ws(runes[i]) || runes[i] == ',') {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Expect a quoted key; otherwise skip to the next comma.
        if runes[i] != '"' {
            while i < n && runes[i] != ',' {
                i += 1;
            }
            continue;
        }

        let Some((key_token, next_idx)) = parse_json_string_runes(&runes, i) else {
            break;
        };
        let key_name = json_string_token_to_raw_string(&key_token);
        i = next_idx;

        while i < n && is_ws(runes[i]) {
            i += 1;
        }
        if i >= n || runes[i] != ':' {
            break;
        }
        i += 1;
        while i < n && is_ws(runes[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }

        match runes[i] {
            '"' => match parse_json_string_runes(&runes, i) {
                None => {
                    // Malformed; treat as empty string.
                    result.insert(key_name, Value::String(String::new()));
                    i = n;
                }
                Some((val_token, ni)) => {
                    result.insert(
                        key_name,
                        Value::String(json_string_token_to_raw_string(&val_token)),
                    );
                    i = ni;
                }
            },
            '{' | '[' => match capture_bracketed(&runes, i) {
                None => i = n,
                Some((seg, ni)) => {
                    let value = serde_json::from_str::<Value>(&seg).unwrap_or(Value::String(seg));
                    result.insert(key_name, value);
                    i = ni;
                }
            },
            _ => {
                let mut j = i;
                while j < n && runes[j] != ',' {
                    j += 1;
                }
                let token: String = runes[i..j].iter().collect::<String>().trim().to_string();
                let value = match token.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    _ => try_parse_number(&token).unwrap_or(Value::String(token)),
                };
                result.insert(key_name, value);
                i = j;
            }
        }

        while i < n && is_ws(runes[i]) {
            i += 1;
        }
        if i < n && runes[i] == ',' {
            i += 1;
        }
    }

    Value::Object(result)
}

// port of parseJSONStringRunes (openai_gemini_response.go)
// The token (quotes included) and the index just after it; None when unterminated.
fn parse_json_string_runes(runes: &[char], start: usize) -> Option<(String, usize)> {
    if start >= runes.len() || runes[start] != '"' {
        return None;
    }
    let mut i = start + 1;
    let mut escaped = false;
    while i < runes.len() {
        let r = runes[i];
        if r == '\\' && !escaped {
            escaped = true;
            i += 1;
            continue;
        }
        if r == '"' && !escaped {
            return Some((runes[start..=i].iter().collect(), i + 1));
        }
        escaped = false;
        i += 1;
    }
    None
}

// port of jsonStringTokenToRawString (openai_gemini_response.go)
fn json_string_token_to_raw_string(token: &str) -> String {
    if let Ok(s) = serde_json::from_str::<String>(token) {
        return s;
    }
    if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        return token[1..token.len() - 1].to_string();
    }
    token.to_string()
}

// port of captureBracketed (openai_gemini_response.go)
fn capture_bracketed(runes: &[char], i: usize) -> Option<(String, usize)> {
    if i >= runes.len() {
        return None;
    }
    let start_rune = runes[i];
    let end_rune = match start_rune {
        '{' => '}',
        '[' => ']',
        _ => return None,
    };
    let mut depth = 0;
    let mut j = i;
    let mut in_str = false;
    let mut escaped = false;
    while j < runes.len() {
        let r = runes[j];
        if in_str {
            if r == '\\' && !escaped {
                escaped = true;
                j += 1;
                continue;
            }
            if r == '"' && !escaped {
                in_str = false;
            } else {
                escaped = false;
            }
            j += 1;
            continue;
        }
        if r == '"' {
            in_str = true;
            j += 1;
            continue;
        }
        if r == start_rune {
            depth += 1;
        } else if r == end_rune {
            depth -= 1;
            if depth == 0 {
                return Some((runes[i..=j].iter().collect(), j + 1));
            }
        }
        j += 1;
    }
    None
}

// port of tryParseNumber (openai_gemini_response.go)
// NaN/Inf (accepted by Go's ParseFloat, unrepresentable in JSON) stay strings.
fn try_parse_number(s: &str) -> Option<Value> {
    if s.is_empty() {
        return None;
    }
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::from(i));
    }
    if let Ok(u) = s.parse::<u64>() {
        return Some(Value::from(u));
    }
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Some(float_value(f)),
        _ => None,
    }
}

/// Complete upstream Chat Completions response -> Gemini response.
pub fn translate_non_stream(upstream: &Value, _original_request: &Value) -> Value {
    convert_openai_response_to_gemini_non_stream(upstream)
}

// port of ConvertOpenAIResponseToGeminiNonStream (openai_gemini_response.go)
fn convert_openai_response_to_gemini_non_stream(root: &Value) -> Value {
    let mut out = base_gemini_template(root);
    // Parts are shared across choices and the index restarts per choice, so a
    // later choice overlays its fields onto the earlier choice's parts.
    let mut all_parts: Vec<Map<String, Value>> = Vec::new();

    fn ensure_part(all_parts: &mut Vec<Map<String, Value>>, idx: usize) -> &mut Map<String, Value> {
        while all_parts.len() <= idx {
            all_parts.push(Map::new());
        }
        &mut all_parts[idx]
    }

    if let Some(Value::Array(choices)) = gget(root, "choices") {
        for choice in choices {
            let choice_idx = gint(gget(choice, "index"));
            let message = gget(choice, "message");

            if let Some(role) = message.and_then(|m| gget(m, "role")) {
                if gstr(Some(role)) == "assistant" {
                    set_candidate_field(&mut out, "role", "model".into());
                }
            }

            let mut part_index = 0usize;

            // Reasoning content before visible text
            if let Some(reasoning) = message.and_then(|m| gget(m, "reasoning_content")) {
                for reasoning_text in extract_reasoning_texts(Some(reasoning)) {
                    if reasoning_text.is_empty() {
                        continue;
                    }
                    let part = ensure_part(&mut all_parts, part_index);
                    part.insert("thought".into(), Value::Bool(true));
                    part.insert("text".into(), Value::String(reasoning_text));
                    part_index += 1;
                }
            }

            if let Some(content) = message.and_then(|m| gget(m, "content")) {
                let text = gstr(Some(content));
                if !text.is_empty() {
                    let part = ensure_part(&mut all_parts, part_index);
                    part.insert("text".into(), Value::String(text));
                    part_index += 1;
                }
            }

            if let Some(Value::Array(tool_calls)) = message.and_then(|m| gget(m, "tool_calls")) {
                for tool_call in tool_calls {
                    if gstr(gget(tool_call, "type")) != "function" {
                        continue;
                    }
                    let function = gget(tool_call, "function");
                    let function_name = gstr(function.and_then(|f| gget(f, "name")));
                    let function_args = gstr(function.and_then(|f| gget(f, "arguments")));
                    let function_id = gstr(gget(tool_call, "id"));

                    let part = ensure_part(&mut all_parts, part_index);
                    // sjson `functionCall.<field>` on an existing value: keep the
                    // object (and its other fields), replace a non-object.
                    if !matches!(part.get("functionCall"), Some(Value::Object(_))) {
                        part.insert("functionCall".into(), Value::Object(Map::new()));
                    }
                    if let Some(Value::Object(fc)) = part.get_mut("functionCall") {
                        if !function_id.is_empty() {
                            fc.insert("id".into(), Value::String(function_id));
                        }
                        fc.insert("name".into(), Value::String(function_name));
                        fc.insert("args".into(), parse_args_to_object(&function_args));
                    }
                    part_index += 1;
                }
            }

            if let Some(Value::String(finish_reason)) = gget(choice, "finish_reason") {
                if !finish_reason.is_empty() {
                    set_candidate_field(
                        &mut out,
                        "finishReason",
                        map_openai_finish_reason_to_gemini(finish_reason).into(),
                    );
                }
            }

            set_candidate_field(&mut out, "index", Value::from(choice_idx));
        }

        if !all_parts.is_empty() {
            let parts: Vec<Value> = all_parts.into_iter().map(Value::Object).collect();
            if let Some(content) = candidate0(&mut out)
                .get_mut("content")
                .and_then(|c| c.as_object_mut())
            {
                content.insert("parts".into(), Value::Array(parts));
            }
        }
    }

    if let Some(usage) = gget(root, "usage") {
        set_gemini_usage_metadata_from_openai_usage(&mut out, usage);
    }

    Value::Object(out)
}

// port of reasoningTokensFromUsage (openai_gemini_response.go)
fn reasoning_tokens_from_usage(usage: &Value) -> i64 {
    if let Some(v) = gget(usage, "completion_tokens_details.reasoning_tokens") {
        return gint(Some(v));
    }
    if let Some(v) = gget(usage, "output_tokens_details.reasoning_tokens") {
        return gint(Some(v));
    }
    0
}

// port of setGeminiUsageMetadataFromOpenAIUsage (openai_gemini_response.go)
fn set_gemini_usage_metadata_from_openai_usage(out: &mut Map<String, Value>, usage: &Value) {
    let prompt = token_count_from_usage(usage, &["prompt_tokens", "input_tokens"]);
    let completion = token_count_from_usage(usage, &["completion_tokens", "output_tokens"]);
    let total = token_count_from_usage(usage, &["total_tokens"]);
    let reasoning_tokens = reasoning_tokens_from_usage(usage);
    let cached_tokens = cached_tokens_from_usage(usage);

    let mut fields: Vec<(&str, i64)> = Vec::new();
    if let Some(p) = prompt {
        fields.push(("promptTokenCount", p));
    }
    if let Some(c) = completion {
        fields.push(("candidatesTokenCount", c));
    }
    if let Some(t) = total {
        fields.push(("totalTokenCount", t));
    } else if prompt.is_some() || completion.is_some() {
        fields.push((
            "totalTokenCount",
            prompt.unwrap_or(0) + completion.unwrap_or(0),
        ));
    }
    if reasoning_tokens > 0 {
        fields.push(("thoughtsTokenCount", reasoning_tokens));
    }
    if cached_tokens > 0 {
        fields.push(("cachedContentTokenCount", cached_tokens));
    }
    if fields.is_empty() {
        return;
    }

    // sjson `usageMetadata.<field>`: create the object when absent.
    if !matches!(out.get("usageMetadata"), Some(Value::Object(_))) {
        out.insert("usageMetadata".into(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(meta)) = out.get_mut("usageMetadata") {
        for (k, v) in fields {
            meta.insert(k.into(), Value::from(v));
        }
    }
}

// port of tokenCountFromUsage (openai_gemini_response.go)
fn token_count_from_usage(usage: &Value, paths: &[&str]) -> Option<i64> {
    paths
        .iter()
        .find_map(|p| gget(usage, p))
        .map(|v| gint(Some(v)))
}

// port of cachedTokensFromUsage (openai_gemini_response.go)
fn cached_tokens_from_usage(usage: &Value) -> i64 {
    if let Some(v) = gget(usage, "prompt_tokens_details.cached_tokens") {
        return gint(Some(v));
    }
    if let Some(v) = gget(usage, "input_tokens_details.cached_tokens") {
        return gint(Some(v));
    }
    0
}

// port of extractReasoningTexts (openai_gemini_response.go)
fn extract_reasoning_texts(node: Option<&Value>) -> Vec<String> {
    let mut texts = Vec::new();
    match node {
        None => {}
        Some(Value::Array(items)) => {
            for item in items {
                texts.extend(extract_reasoning_texts(Some(item)));
            }
        }
        Some(Value::String(s)) => texts.push(s.clone()),
        // An object: its `text`, else nothing (its raw text starts with `{`).
        Some(obj @ Value::Object(_)) => {
            if let Some(text) = gget(obj, "text") {
                texts.push(gstr(Some(text)));
            }
        }
        Some(_) => {}
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(body: Value) -> Value {
        translate_request("test-model", &body, false)
    }

    fn call_id(kind: &str, msg: usize, part: usize, name: &str, payload: &str) -> String {
        deterministic_tool_call_id(kind, msg, part, name, payload)
    }

    #[test]
    fn deterministic_tool_call_id_matches_go_formula() {
        // sha256("call|0|0|read_file|{\"path\":\"a.txt\"}")[:12] hex, computed
        // independently of this module.
        assert_eq!(
            call_id("call", 0, 0, "read_file", r#"{"path":"a.txt"}"#),
            "call_1486760d25add4869d9cec8c"
        );
    }

    // port of TestConvertGeminiRequestToOpenAI_FunctionResponsesConsumeToolCallIDsFIFO
    #[test]
    fn function_responses_consume_tool_call_ids_fifo() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"name":"read_file","args":{"path":"a.txt"}}},
                {"functionCall":{"name":"grep","args":{"pattern":"needle"}}},
                {"functionCall":{"name":"list_dir","args":{"path":"."}}}
            ]},
            {"role":"function","parts":[
                {"functionResponse":{"name":"read_file","response":{"result":"a"}}},
                {"functionResponse":{"name":"grep","response":{"result":"b"}}},
                {"functionResponse":{"name":"list_dir","response":{"result":"c"}}}
            ]}
        ]}));
        let a = call_id("call", 0, 0, "read_file", r#"{"path":"a.txt"}"#);
        let b = call_id("call", 0, 1, "grep", r#"{"pattern":"needle"}"#);
        let c = call_id("call", 0, 2, "list_dir", r#"{"path":"."}"#);
        assert!(a != b && b != c && a != c);
        assert_eq!(
            out,
            json!({
                "model":"test-model",
                "messages":[
                    {"role":"assistant","content":"","tool_calls":[
                        {"id":a,"type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}},
                        {"id":b,"type":"function","function":{"name":"grep","arguments":"{\"pattern\":\"needle\"}"}},
                        {"id":c,"type":"function","function":{"name":"list_dir","arguments":"{\"path\":\".\"}"}}
                    ]},
                    {"role":"tool","tool_call_id":a,"content":"{\"result\":\"a\"}"},
                    {"role":"tool","tool_call_id":b,"content":"{\"result\":\"b\"}"},
                    {"role":"tool","tool_call_id":c,"content":"{\"result\":\"c\"}"},
                    // Go appends the (now empty) function-role turn itself too.
                    {"role":"function","content":""}
                ],
                "stream":false
            })
        );
    }

    // port of TestConvertGeminiRequestToOpenAI_FunctionResponseWithoutPriorCallGetsFallbackID
    // + TestConvertGeminiRequestToOpenAI_DeterministicFallbackOrphanResponse
    #[test]
    fn orphan_function_response_gets_deterministic_fallback_id() {
        let body = json!({"contents":[{"role":"function","parts":[
            {"functionResponse":{"name":"read_file","response":{"result":"ok"}}}
        ]}]});
        let id = call_id("response", 0, 0, "read_file", r#"{"result":"ok"}"#);
        let want = json!({
            "model":"test-model",
            "messages":[
                {"role":"tool","tool_call_id":id,"content":"{\"result\":\"ok\"}"},
                {"role":"function","content":""}
            ],
            "stream":false
        });
        for _ in 0..100 {
            assert_eq!(req(body.clone()), want);
        }
        assert!(id.starts_with("call_"));
    }

    // port of TestConvertGeminiRequestToOpenAI_ExtraFunctionResponsesUseFallbackID
    #[test]
    fn extra_function_responses_use_fallback_id() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"path":"a.txt"}}}]},
            {"role":"function","parts":[
                {"functionResponse":{"name":"read_file","response":{"result":"a"}}},
                {"functionResponse":{"name":"read_file","response":{"result":"extra"}}}
            ]}
        ]}));
        let call = call_id("call", 0, 0, "read_file", r#"{"path":"a.txt"}"#);
        let extra = call_id("response", 1, 1, "read_file", r#"{"result":"extra"}"#);
        assert_ne!(call, extra);
        assert_eq!(out["messages"][1]["tool_call_id"], json!(call));
        assert_eq!(out["messages"][2]["tool_call_id"], json!(extra));
    }

    // port of TestConvertGeminiRequestToOpenAI_PreservesExplicitFunctionCallIDs
    #[test]
    fn preserves_explicit_function_call_ids() {
        for (field, want) in [
            ("id", "call_gateway_id"),
            ("call_id", "call_gateway_call_id"),
            ("callId", "call_gateway_camel_id"),
        ] {
            let mut call = json!({"name":"lookup","args":{"q":"x"}});
            call[field] = json!(want);
            let mut resp = json!({"name":"lookup","response":{"result":"ok"}});
            resp[field] = json!(want);
            let out = req(json!({"contents":[
                {"role":"model","parts":[{"functionCall":call}]},
                {"role":"function","parts":[{"functionResponse":resp}]}
            ]}));
            assert_eq!(
                out["messages"][0]["tool_calls"][0]["id"],
                json!(want),
                "{field}"
            );
            assert_eq!(out["messages"][1]["tool_call_id"], json!(want), "{field}");
        }
    }

    // port of TestConvertGeminiRequestToOpenAI_AcceptsSnakeInlineData
    #[test]
    fn accepts_snake_inline_data() {
        let out = translate_request(
            "gpt-test",
            &json!({"contents":[{"role":"user","parts":[{"inline_data":{"mime_type":"image/png","data":"aGVsbG8="}}]}]}),
            false,
        );
        assert_eq!(
            out["messages"],
            json!([{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}
            ]}])
        );
    }

    // port of TestConvertGeminiRequestToOpenAI_SplitsNonImageInlineDataByMIME
    #[test]
    fn splits_non_image_inline_data_by_mime() {
        let out = translate_request(
            "gpt-test",
            &json!({"contents":[{"role":"user","parts":[
                {"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}},
                {"inlineData":{"mimeType":"video/mp4","data":"AAAAIGZ0eXA="}},
                {"inlineData":{"mimeType":"application/pdf","data":"JVBERi0="}}
            ]}]}),
            false,
        );
        assert_eq!(
            out["messages"],
            json!([{"role":"user","content":[
                {"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}},
                {"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAAIGZ0eXA="}},
                {"type":"file","file":{"filename":"document.pdf","file_data":"JVBERi0="}}
            ]}])
        );
    }

    // port of TestConvertGeminiRequestToOpenAI_DropsHiddenThoughtParts
    #[test]
    fn drops_hidden_thought_parts() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[{"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"}]},
            {"role":"user","parts":[{"text":"continue"}]}
        ]}));
        assert_eq!(
            out["messages"],
            json!([{"role":"user","content":"continue"}])
        );

        let out = req(json!({"contents":[{"role":"model","parts":[
            {"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"},
            {"text":"visible answer"}
        ]}]}));
        assert_eq!(
            out["messages"],
            json!([{"role":"assistant","content":"visible answer"}])
        );
    }

    // port of TestConvertGeminiRequestToOpenAI_DeterministicToolCallIDs
    #[test]
    fn deterministic_tool_call_ids() {
        let body = json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"name":"read_file","args":{"path":"main.go"}}},
                {"functionCall":{"name":"grep","args":{"pattern":"TODO"}}}
            ]},
            {"role":"function","parts":[
                {"functionResponse":{"name":"read_file","response":{"result":"code"}}},
                {"functionResponse":{"name":"grep","response":{"result":"matches"}}}
            ]}
        ]});
        let first = req(body.clone());
        let c0 = call_id("call", 0, 0, "read_file", r#"{"path":"main.go"}"#);
        let c1 = call_id("call", 0, 1, "grep", r#"{"pattern":"TODO"}"#);
        assert_eq!(first["messages"][0]["tool_calls"][0]["id"], json!(c0));
        assert_eq!(first["messages"][0]["tool_calls"][1]["id"], json!(c1));
        assert_eq!(first["messages"][1]["tool_call_id"], json!(c0));
        assert_eq!(first["messages"][2]["tool_call_id"], json!(c1));
        for _ in 0..100 {
            assert_eq!(req(body.clone()), first);
        }
    }

    // port of TestConvertGeminiRequestToOpenAI_SameNameCallsInSameMessageDistinct
    #[test]
    fn same_name_calls_in_same_message_distinct() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"name":"read_file","args":{"path":"a.txt"}}},
                {"functionCall":{"name":"read_file","args":{"path":"a.txt"}}}
            ]},
            {"role":"function","parts":[
                {"functionResponse":{"name":"read_file","response":{"result":"first"}}},
                {"functionResponse":{"name":"read_file","response":{"result":"second"}}}
            ]}
        ]}));
        let id0 = call_id("call", 0, 0, "read_file", r#"{"path":"a.txt"}"#);
        let id1 = call_id("call", 0, 1, "read_file", r#"{"path":"a.txt"}"#);
        assert_ne!(id0, id1);
        assert_eq!(out["messages"][0]["tool_calls"][0]["id"], json!(id0));
        assert_eq!(out["messages"][0]["tool_calls"][1]["id"], json!(id1));
        assert_eq!(out["messages"][1]["tool_call_id"], json!(id0));
        assert_eq!(out["messages"][2]["tool_call_id"], json!(id1));
    }

    // port of TestConvertGeminiRequestToOpenAI_InterleavedPerNameFIFOMatching
    #[test]
    fn interleaved_per_name_fifo_matching() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"name":"tool_a","args":{"step":1}}},
                {"functionCall":{"name":"tool_b","args":{"step":1}}},
                {"functionCall":{"name":"tool_a","args":{"step":2}}},
                {"functionCall":{"name":"tool_b","args":{"step":2}}}
            ]},
            {"role":"function","parts":[
                {"functionResponse":{"name":"tool_b","response":{"step":1}}},
                {"functionResponse":{"name":"tool_a","response":{"step":1}}},
                {"functionResponse":{"name":"tool_b","response":{"step":2}}},
                {"functionResponse":{"name":"tool_a","response":{"step":2}}}
            ]}
        ]}));
        let a1 = call_id("call", 0, 0, "tool_a", r#"{"step":1}"#);
        let b1 = call_id("call", 0, 1, "tool_b", r#"{"step":1}"#);
        let a2 = call_id("call", 0, 2, "tool_a", r#"{"step":2}"#);
        let b2 = call_id("call", 0, 3, "tool_b", r#"{"step":2}"#);
        let ids: Vec<&Value> = (1..=4)
            .map(|i| &out["messages"][i]["tool_call_id"])
            .collect();
        assert_eq!(ids, vec![&json!(b1), &json!(a1), &json!(b2), &json!(a2)]);
    }

    // port of TestConvertGeminiRequestToOpenAI_ExplicitCallInheritedByImplicitResponse
    #[test]
    fn explicit_call_inherited_by_implicit_response() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[{"functionCall":{"name":"lookup","id":"explicit_call_1","args":{"q":"foo"}}}]},
            {"role":"function","parts":[{"functionResponse":{"name":"lookup","response":{"result":"bar"}}}]}
        ]}));
        assert_eq!(
            out["messages"][0]["tool_calls"][0]["id"],
            json!("explicit_call_1")
        );
        assert_eq!(out["messages"][1]["tool_call_id"], json!("explicit_call_1"));
    }

    // port of TestConvertGeminiRequestToOpenAI_OutOrderExplicitResponseDoesNotDuplicateID
    #[test]
    fn out_of_order_explicit_response_does_not_duplicate_id() {
        let out = req(json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"name":"foo","id":"call_1","args":{"n":1}}},
                {"functionCall":{"name":"foo","id":"call_2","args":{"n":2}}},
                {"functionCall":{"name":"foo","id":"call_3","args":{"n":3}}}
            ]},
            {"role":"function","parts":[
                {"functionResponse":{"name":"foo","id":"call_2","response":{"r":2}}},
                {"functionResponse":{"name":"foo","response":{"r":1}}},
                {"functionResponse":{"name":"foo","response":{"r":3}}}
            ]}
        ]}));
        assert_eq!(out["messages"][1]["tool_call_id"], json!("call_2"));
        assert_eq!(out["messages"][2]["tool_call_id"], json!("call_1"));
        assert_eq!(out["messages"][3]["tool_call_id"], json!("call_3"));
    }

    #[test]
    fn maps_generation_config_system_tools_and_tool_choice() {
        let out = translate_request(
            "gpt-up",
            &json!({
                "systemInstruction":{"parts":[{"text":"be brief"},{"thought":true,"text":"x"}]},
                "contents":[{"role":"user","parts":[
                    {"text":"see "},
                    {"fileData":{"fileUri":"gs://b/f.pdf","mimeType":"application/pdf"}},
                    {"fileData":{"fileUri":"gs://b/a.bin"}}
                ]}],
                "generationConfig":{
                    "temperature":0.5,"maxOutputTokens":256,"topP":1,"topK":40,
                    "stopSequences":["END"],"candidateCount":1,
                    "responseModalities":["TEXT"," Image ","VIDEO"],
                    "thinkingConfig":{"thinkingBudget":2048}
                },
                "service_tier":"flex",
                "tools":[{"functionDeclarations":[
                    {"name":"f","description":"d","parameters":{"type":"object"}},
                    {"name":"g","parametersJsonSchema":{"type":"object","properties":{}}}
                ]}],
                "toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["f"]}}
            }),
            true,
        );
        assert_eq!(
            out,
            json!({
                "model":"gpt-up",
                "messages":[
                    {"role":"system","content":[{"type":"text","text":"be brief"}]},
                    {"role":"user","content":[
                        {"type":"text","text":"see "},
                        {"type":"file","file":{"filename":"document.pdf","file_url":"gs://b/f.pdf"}},
                        {"type":"text","text":"File: gs://b/a.bin"}
                    ]}
                ],
                "temperature":0.5,
                "max_tokens":256,
                "top_p":1,
                "top_k":40,
                "stop":["END"],
                "n":1,
                "modalities":["text","image"],
                "reasoning_effort":"medium",
                "stream":true,
                "service_tier":"flex",
                "tools":[
                    {"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object"}}},
                    {"type":"function","function":{"name":"g","description":"","parameters":{"type":"object","properties":{}}}}
                ],
                "tool_choice":{"type":"function","function":{"name":"f"}}
            })
        );
        // Key order follows Go's sjson write order too.
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "model",
                "messages",
                "temperature",
                "max_tokens",
                "top_p",
                "top_k",
                "stop",
                "n",
                "modalities",
                "reasoning_effort",
                "stream",
                "service_tier",
                "tools",
                "tool_choice"
            ]
        );
    }

    #[test]
    fn thinking_level_wins_over_budget_and_tool_modes() {
        let out = req(json!({
            "generationConfig":{"thinkingConfig":{"thinking_level":" HIGH ","thinkingBudget":0}},
            "toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a","b"]}}
        }));
        assert_eq!(
            out,
            json!({"model":"test-model","messages":[],"reasoning_effort":"high","stream":false,"tool_choice":"required"})
        );
        let out = req(json!({
            "generationConfig":{"thinkingConfig":{"thinking_budget":-1}},
            "toolConfig":{"functionCallingConfig":{"mode":"NONE"}}
        }));
        assert_eq!(out["reasoning_effort"], json!("auto"));
        assert_eq!(out["tool_choice"], json!("none"));
    }

    // port of TestConvertOpenAIResponseToGeminiNonStreamPreservesToolCallID
    #[test]
    fn non_stream_preserves_tool_call_id() {
        let raw = json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[
            {"id":"call_chat_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}
        ]}}]});
        assert_eq!(
            translate_non_stream(&raw, &json!({})),
            json!({"candidates":[{"content":{"parts":[
                {"functionCall":{"id":"call_chat_1","name":"lookup","args":{"q":"x"}}}
            ],"role":"model"},"index":0}]})
        );
    }

    // port of TestConvertOpenAIResponseToGeminiStreamPreservesToolCallID
    #[test]
    fn stream_preserves_tool_call_id() {
        let mut t = StreamTranslator::new(&json!({}));
        assert!(t
            .push(
                None,
                &json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_stream_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]}}]})
            )
            .is_empty());
        let out = t.push(
            None,
            &json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        );
        assert_eq!(
            out,
            vec![
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_stream_1\",\"name\":\"lookup\",\"args\":{\"q\":\"x\"}}}],\"role\":\"model\"},\"index\":0,\"finishReason\":\"STOP\"}]}\n\n"
            ]
        );
    }

    // port of TestConvertOpenAIResponseToGeminiNonStream_MultiChoicePartsOverlay
    #[test]
    fn non_stream_multi_choice_parts_overlay() {
        let out1 = translate_non_stream(
            &json!({"choices":[
                {"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]}},
                {"index":1,"message":{"role":"assistant","content":"choice 1 text"}}
            ]}),
            &json!({}),
        );
        assert_eq!(
            out1,
            json!({"candidates":[{"content":{"parts":[
                {"functionCall":{"id":"call_1","name":"lookup","args":{}},"text":"choice 1 text"}
            ],"role":"model"},"index":1}]})
        );

        let out2 = translate_non_stream(
            &json!({"choices":[
                {"index":0,"message":{"role":"assistant","reasoning_content":"initial thought"}},
                {"index":1,"message":{"role":"assistant","content":"final text"}}
            ]}),
            &json!({}),
        );
        assert_eq!(
            out2,
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"final text"}],"role":"model"},"index":1}]})
        );

        let out3 = translate_non_stream(
            &json!({"choices":[
                {"index":0,"message":{"role":"assistant","content":"original text"}},
                {"index":1,"message":{"role":"assistant","tool_calls":[{"id":"call_2","type":"function","function":{"name":"search","arguments":"{}"}}]}}
            ]}),
            &json!({}),
        );
        assert_eq!(
            out3,
            json!({"candidates":[{"content":{"parts":[
                {"text":"original text","functionCall":{"id":"call_2","name":"search","args":{}}}
            ],"role":"model"},"index":1}]})
        );
    }

    // port of TestConvertOpenAIResponseToGeminiStream_NullFinishReasonIgnored
    #[test]
    fn stream_null_finish_reason_ignored() {
        let mut t = StreamTranslator::new(&json!({}));
        assert!(t
            .push(None, &json!({"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}))
            .is_empty());
        assert_eq!(
            t.push(None, &json!({"choices":[{"index":0,"delta":{"reasoning_content":"thinking..."},"finish_reason":null}]})),
            vec!["data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"thinking...\"}],\"role\":\"model\"},\"index\":0}]}\n\n"]
        );
        assert!(t
            .push(
                None,
                &json!({"choices":[{"index":0,"delta":{},"finish_reason":""}]})
            )
            .is_empty());
        assert_eq!(
            t.push(None, &json!({"choices":[{"index":0,"delta":{"content":"hello world"},"finish_reason":null}]})),
            vec!["data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello world\"}],\"role\":\"model\"},\"index\":0}]}\n\n"]
        );
        assert_eq!(
            t.push(None, &json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
            vec!["data: {\"candidates\":[{\"content\":{\"parts\":[],\"role\":\"model\"},\"index\":0,\"finishReason\":\"STOP\"}]}\n\n"]
        );
        assert!(t.finish().is_empty());
    }

    // port of TestConvertOpenAIResponseToGeminiNonStream_NullFinishReasonIgnored
    #[test]
    fn non_stream_null_finish_reason_ignored() {
        for fr in [Value::Null, json!("")] {
            let out = translate_non_stream(
                &json!({"choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":fr}]}),
                &json!({}),
            );
            assert_eq!(
                out,
                json!({"candidates":[{"content":{"parts":[{"text":"hello"}],"role":"model"},"index":0}]})
            );
        }
    }

    #[test]
    fn non_stream_reasoning_text_usage_and_finish() {
        let out = translate_non_stream(
            &json!({
                "id":"x","model":"gpt-up",
                "choices":[{"index":0,"finish_reason":"length","message":{
                    "role":"assistant","reasoning_content":[{"text":"r1"},"r2"],"content":"answer"
                }}],
                "usage":{"prompt_tokens":10,"completion_tokens":5,
                    "completion_tokens_details":{"reasoning_tokens":3},
                    "prompt_tokens_details":{"cached_tokens":4}}
            }),
            &json!({}),
        );
        assert_eq!(
            out,
            json!({
                "candidates":[{"content":{"parts":[
                    {"thought":true,"text":"r1"},
                    {"thought":true,"text":"r2"},
                    {"text":"answer"}
                ],"role":"model"},"index":0,"finishReason":"MAX_TOKENS"}],
                "model":"gpt-up",
                "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15,
                    "thoughtsTokenCount":3,"cachedContentTokenCount":4}
            })
        );
    }

    #[test]
    fn args_parse_strict_tolerant_and_fallback() {
        assert_eq!(parse_args_to_object(""), json!({}));
        assert_eq!(parse_args_to_object(r#" {"a":1} "#), json!({"a":1}));
        assert_eq!(
            parse_args_to_object(
                r#"{"location": 北京, "unit": celsius, "n": 3, "f": 1.5, "ok": true, "z": null, "o": {"k":[1]}, "s": "q\"x"}"#
            ),
            json!({"location":"北京","unit":"celsius","n":3,"f":1.5,"ok":true,"z":null,"o":{"k":[1]},"s":"q\"x"})
        );
        assert_eq!(parse_args_to_object("[1,2]"), json!({}));
        assert_eq!(parse_args_to_object("not json"), json!({}));
    }

    #[test]
    fn stream_end_to_end_reasoning_text_tool_call_usage() {
        let mut t = StreamTranslator::new(&json!({"contents":[]}));
        let chunk = |delta: Value, fr: Value| {
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-up",
                "choices":[{"index":0,"delta":delta,"finish_reason":fr}]})
        };
        let mut frames: Vec<String> = Vec::new();
        for ev in [
            chunk(json!({"role":"assistant","content":""}), Value::Null),
            chunk(json!({"reasoning_content":"Let me check."}), Value::Null),
            chunk(json!({"content":"Checking the weather."}), Value::Null),
            chunk(
                json!({"tool_calls":[{"index":0,"id":"call_w","type":"function","function":{"name":"get_weather","arguments":""}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-up","choices":[],
                "usage":{"prompt_tokens":20,"completion_tokens":12,"total_tokens":32,
                    "completion_tokens_details":{"reasoning_tokens":4}}}),
        ] {
            frames.extend(t.push(None, &ev));
        }
        frames.extend(t.finish());
        assert_eq!(
            frames,
            vec![
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"Let me check.\"}],\"role\":\"model\"},\"index\":0}],\"model\":\"gpt-up\"}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Checking the weather.\"}],\"role\":\"model\"},\"index\":0}],\"model\":\"gpt-up\"}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_w\",\"name\":\"get_weather\",\"args\":{\"city\":\"Paris\"}}}],\"role\":\"model\"},\"index\":0,\"finishReason\":\"STOP\"}],\"model\":\"gpt-up\"}\n\n",
                "data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":12,\"totalTokenCount\":32,\"thoughtsTokenCount\":4},\"model\":\"gpt-up\"}\n\n",
            ]
        );
    }

    #[test]
    fn stream_usage_on_choice_chunk_and_skips_non_function_tools() {
        let mut t = StreamTranslator::new(&json!({}));
        assert!(t
            .push(None, &json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"custom","function":{"name":"x"}}]}}]}))
            .is_empty());
        assert_eq!(
            t.push(None, &json!({"choices":[{"index":0,"delta":{}}],"usage":{"input_tokens":2,"output_tokens":3}})),
            vec!["data: {\"candidates\":[{\"content\":{\"parts\":[],\"role\":\"model\"},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":3,\"totalTokenCount\":5}}\n\n"]
        );
    }
}
