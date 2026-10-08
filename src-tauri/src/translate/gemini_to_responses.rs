//! Gemini `generateContent` CLIENT ⇄ OpenAI Responses UPSTREAM (the ChatGPT
//! Codex backend). Port of CLIProxyAPI `internal/translator/codex/gemini`
//! (commit ed980be): `codex_gemini_request.go` + `codex_gemini_response.go`,
//! registered in `init.go` as `ConvertGeminiRequestToCodex` /
//! `ConvertCodexResponseToGemini` / `ConvertCodexResponseToGeminiNonStream`.
//!
//! Out of scope (handled elsewhere or not routed): the model-suffix thinking
//! parse, `GeminiTokenCount`, and image generation (`image_generation_call`
//! items and `response.image_generation_call.partial_image` events, which
//! the Go file turns into `inlineData` parts).

use std::collections::{HashMap, HashSet};

use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{json, Map, Value};

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

/// gjson `Result.String()`: strings verbatim, scalars as text, null/missing
/// as "", objects/arrays as their raw JSON.
fn gs(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// gjson `Result.Bool()`.
fn gbool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

/// gjson `Result.Int()`.
fn gint(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Bool(true)) => 1,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .unwrap_or_else(|| n.as_f64().map(|f| f as i64).unwrap_or(0)),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// Longest prefix of `s` that is at most `max` BYTES and ends on a char
/// boundary (Go slices bytes and may split a UTF-8 sequence; Rust cannot).
fn byte_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `time.Unix(sec, 0).Format(time.RFC3339Nano)`, rendered in UTC.
fn unix_rfc3339(sec: i64) -> String {
    Utc.timestamp_opt(sec, 0)
        .single()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::AutoSi, true))
        .unwrap_or_default()
}

// ───────────────────────────── request ─────────────────────────────

// port of ConvertGeminiRequestToCodex (codex_gemini_request.go)
/// Client request (Gemini generateContent) -> upstream request (Responses).
/// `stream` is ignored, as in Go: the Codex backend is always streamed.
pub fn translate_request(model: &str, body: &Value, _stream: bool) -> Value {
    let root = body;
    let mut out = Map::new();
    out.insert("model".into(), json!(""));
    out.insert("instructions".into(), json!(""));
    out.insert("input".into(), json!([]));
    let mut input_items: Vec<Value> = Vec::new();

    // Pre-compute tool name shortening map from declared functionDeclarations
    let short_map = build_short_name_map(&declared_function_names(root));

    // FIFO queue of generated call ids, consumed in order by functionResponses.
    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut call_counter: u64 = 0;

    let get_gemini_call_id = |value: &Value| -> String {
        let id = gs(gp(value, "id")).trim().to_string();
        if !id.is_empty() {
            return id;
        }
        gs(gp(value, "call_id")).trim().to_string()
    };

    // Model
    out.insert("model".into(), json!(model));
    let service_tier = normalize_gemini_codex_service_tier(gp(root, "service_tier"));
    if !service_tier.is_empty() {
        out.insert("service_tier".into(), json!(service_tier));
    }

    // System instruction -> a developer message with input_text parts
    let sys_parts =
        gp(root, "system_instruction.parts").or_else(|| gp(root, "systemInstruction.parts"));
    if let Some(Value::Array(arr)) = sys_parts {
        let mut content_items = Vec::new();
        for p in arr {
            if is_gemini_thought_part(p) {
                continue;
            }
            if let Some(t) = gp(p, "text") {
                content_items.push(json!({"type": "input_text", "text": gs(Some(t))}));
            }
        }
        if !content_items.is_empty() {
            input_items
                .push(json!({"type": "message", "role": "developer", "content": content_items}));
        }
    }

    // Contents -> messages and function calls/results
    if let Some(Value::Array(items)) = gp(root, "contents") {
        for item in items {
            let mut role = gs(gp(item, "role"));
            if role == "model" {
                role = "assistant".into();
            }
            let parr = match gp(item, "parts") {
                Some(Value::Array(a)) => a,
                _ => continue,
            };
            for p in parr {
                if is_gemini_thought_part(p) {
                    continue;
                }

                // text part
                if let Some(t) = gp(p, "text") {
                    let part_type = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    let part = json!({"type": part_type, "text": gs(Some(t))});
                    input_items.push(codex_message_with_part(&role, part));
                    continue;
                }

                if let Some(part) = codex_content_part_from_gemini_inline_data(p) {
                    input_items.push(codex_message_with_part(&role, part));
                    continue;
                }

                if let Some(part) = codex_content_part_from_gemini_file_data(p) {
                    input_items.push(codex_message_with_part(&role, part));
                    continue;
                }

                // function call from model
                if let Some(fc) = gp(p, "functionCall") {
                    let mut f = Map::new();
                    f.insert("type".into(), json!("function_call"));
                    if let Some(name) = gp(fc, "name") {
                        let n = gs(Some(name));
                        let n = match short_map.get(&n) {
                            Some(short) => short.clone(),
                            None => shorten_name_if_needed(&n),
                        };
                        f.insert("name".into(), json!(n));
                    }
                    if let Some(args) = gp(fc, "args") {
                        f.insert("arguments".into(), json!(args.to_string()));
                    }
                    // Reuse gateway-provided ids, otherwise generate one for pairing.
                    let mut id = get_gemini_call_id(fc);
                    if id.is_empty() {
                        call_counter += 1;
                        id = format!("call_gemini_{call_counter:016}");
                    }
                    f.insert("call_id".into(), json!(id));
                    pending_call_ids.push(id);
                    input_items.push(Value::Object(f));
                    continue;
                }

                // function response from user
                if let Some(fr) = gp(p, "functionResponse") {
                    let mut f = Map::new();
                    f.insert("type".into(), json!("function_call_output"));
                    // Prefer a string result; otherwise embed the raw response.
                    if let Some(res) = gp(fr, "response.result") {
                        f.insert("output".into(), json!(gs(Some(res))));
                    } else if let Some(resp) = gp(fr, "response") {
                        f.insert("output".into(), json!(resp.to_string()));
                    }
                    let custom_id = get_gemini_call_id(fr);
                    let id = if !custom_id.is_empty() {
                        if let Some(idx) = pending_call_ids.iter().position(|x| *x == custom_id) {
                            pending_call_ids.remove(idx);
                        }
                        custom_id
                    } else if !pending_call_ids.is_empty() {
                        pending_call_ids.remove(0)
                    } else {
                        call_counter += 1;
                        format!("call_gemini_{call_counter:016}")
                    };
                    f.insert("call_id".into(), json!(id));
                    input_items.push(Value::Object(f));
                    continue;
                }
            }
        }
    }

    if !input_items.is_empty() {
        out.insert("input".into(), Value::Array(input_items));
    }

    // Tools mapping: Gemini functionDeclarations -> Codex tools
    if let Some(Value::Array(tarr)) = gp(root, "tools") {
        let mut tool_items = Vec::new();
        out.insert("tool_choice".into(), json!("auto"));
        for td in tarr {
            let farr = match gp(td, "functionDeclarations") {
                Some(Value::Array(a)) => a,
                _ => continue,
            };
            for f in farr {
                let mut tool = Map::new();
                tool.insert("type".into(), json!("function"));
                if let Some(v) = gp(f, "name") {
                    let name = gs(Some(v));
                    let name = match short_map.get(&name) {
                        Some(short) => short.clone(),
                        None => shorten_name_if_needed(&name),
                    };
                    tool.insert("name".into(), json!(name));
                }
                if let Some(v) = gp(f, "description") {
                    tool.insert("description".into(), json!(gs(Some(v))));
                }
                if let Some(prm) = gp(f, "parameters") {
                    tool.insert("parameters".into(), clean_gemini_codex_tool_parameters(prm));
                } else if let Some(prm) = gp(f, "parametersJsonSchema") {
                    tool.insert("parameters".into(), clean_gemini_codex_tool_parameters(prm));
                }
                tool.insert("strict".into(), json!(false));
                tool_items.push(Value::Object(tool));
            }
        }
        out.insert("tools".into(), Value::Array(tool_items));
    }

    // Fixed flags aligning with Codex expectations
    out.insert("parallel_tool_calls".into(), json!(true));
    set_codex_tool_choice_from_gemini_tool_config(
        &mut out,
        gp(root, "toolConfig.functionCallingConfig"),
    );

    // Gemini thinkingConfig -> Codex reasoning.effort (snake_case accepted too).
    let mut effort_set = false;
    fn set_effort(out: &mut Map<String, Value>, effort: String) {
        out.insert("reasoning".into(), json!({"effort": effort}));
    }
    if let Some(gen) = gp(root, "generationConfig") {
        let level = gp(gen, "thinkingLevel").or_else(|| gp(gen, "thinking_level"));
        if let Some(level) = level {
            let effort = gs(Some(level)).trim().to_lowercase();
            if !effort.is_empty() {
                set_effort(&mut out, effort);
                effort_set = true;
            }
        } else if let Some(tc @ Value::Object(_)) = gp(gen, "thinkingConfig") {
            let level = gp(tc, "thinkingLevel").or_else(|| gp(tc, "thinking_level"));
            if let Some(level) = level {
                let effort = gs(Some(level)).trim().to_lowercase();
                if !effort.is_empty() {
                    set_effort(&mut out, effort);
                    effort_set = true;
                }
            } else if let Some(budget) =
                gp(tc, "thinkingBudget").or_else(|| gp(tc, "thinking_budget"))
            {
                if let Some(effort) = convert_budget_to_level(gint(Some(budget))) {
                    set_effort(&mut out, effort.to_string());
                    effort_set = true;
                }
            }
        }
    }
    if !effort_set {
        // No thinking config, set default effort
        set_effort(&mut out, "medium".into());
    }
    out.insert("stream".into(), json!(true));
    out.insert("store".into(), json!(false));
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));

    // Lower-case every string `type` anywhere under `tools` (Gemini schemas
    // use upper-case `STRING` / `OBJECT`).
    if let Some(tools) = out.get_mut("tools") {
        lower_type_fields(tools);
    }

    Value::Object(out)
}

// port of util.Walk(tools, "", "type", ...) + the lower-casing loop (codex_gemini_request.go)
fn lower_type_fields(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, child) in m.iter_mut() {
                if k == "type" {
                    if let Value::String(s) = child {
                        let lower = s.to_lowercase();
                        if lower != *s {
                            *s = lower;
                        }
                    }
                }
                lower_type_fields(child);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(lower_type_fields),
        _ => {}
    }
}

// port of translatorcommon.IsGeminiThoughtPart (common/gemini.go)
fn is_gemini_thought_part(part: &Value) -> bool {
    gbool(gp(part, "thought"))
}

// port of thinking.ConvertBudgetToLevel (thinking/convert.go)
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

/// Every `tools[].functionDeclarations[].name` (existing), in order.
fn declared_function_names(root: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(Value::Array(tarr)) = gp(root, "tools") {
        for t in tarr {
            if let Some(Value::Array(fns)) = gp(t, "functionDeclarations") {
                for f in fns {
                    if let Some(v) = gp(f, "name") {
                        names.push(gs(Some(v)));
                    }
                }
            }
        }
    }
    names
}

// port of setCodexToolChoiceFromGeminiToolConfig (codex_gemini_request.go)
fn set_codex_tool_choice_from_gemini_tool_config(
    out: &mut Map<String, Value>,
    fcc: Option<&Value>,
) {
    let fcc = match fcc {
        Some(v) => v,
        None => return,
    };
    match gs(gp(fcc, "mode")).as_str() {
        "NONE" => {
            out.insert("tool_choice".into(), json!("none"));
        }
        "AUTO" if out.get("tool_choice") != Some(&json!("auto")) => {
            out.insert("tool_choice".into(), json!("auto"));
        }
        "ANY" => match gp(fcc, "allowedFunctionNames") {
            Some(Value::Array(a)) if a.len() == 1 => {
                let name = shorten_name_if_needed(&gs(a.first()));
                out.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "name": name}),
                );
            }
            _ => {
                out.insert("tool_choice".into(), json!("required"));
            }
        },
        _ => {}
    }
}

// port of cleanGeminiCodexToolParameters (codex_gemini_request.go)
fn clean_gemini_codex_tool_parameters(parameters: &Value) -> Value {
    let mut cleaned = parameters.clone();
    if let Value::Object(m) = &mut cleaned {
        m.shift_remove("$schema");
        if m.get("additionalProperties") != Some(&Value::Bool(false)) {
            m.insert("additionalProperties".into(), json!(false));
        }
    }
    cleaned
}

// port of codexMessageWithPart (codex_gemini_request.go)
fn codex_message_with_part(role: &str, part: Value) -> Value {
    json!({"type": "message", "role": role, "content": [part]})
}

// port of normalizeGeminiCodexServiceTier (codex_gemini_request.go)
fn normalize_gemini_codex_service_tier(service_tier: Option<&Value>) -> &'static str {
    match service_tier {
        Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "priority" | "fast" => "priority",
            _ => "",
        },
        _ => "",
    }
}

// port of codexContentPartFromGeminiInlineData (codex_gemini_request.go)
fn codex_content_part_from_gemini_inline_data(part: &Value) -> Option<Value> {
    let inline = gp(part, "inlineData").or_else(|| gp(part, "inline_data"))?;
    let mut mime_type = gs(gp(inline, "mimeType"));
    if mime_type.is_empty() {
        mime_type = gs(gp(inline, "mime_type"));
    }
    let data = gs(gp(inline, "data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        Some(json!({"type": "input_image", "image_url": format!("data:{mime_type};base64,{data}")}))
    } else if lower.starts_with("audio/") {
        Some(json!({
            "type": "input_audio",
            "input_audio": {"data": data, "format": codex_input_audio_format_from_mime(&mime_type)}
        }))
    } else {
        Some(json!({
            "type": "input_file",
            "file_data": data,
            "filename": codex_file_name_from_mime(&mime_type)
        }))
    }
}

// port of codexContentPartFromGeminiFileData (codex_gemini_request.go)
fn codex_content_part_from_gemini_file_data(part: &Value) -> Option<Value> {
    let fd = gp(part, "fileData").or_else(|| gp(part, "file_data"))?;
    let mut uri = gs(gp(fd, "fileUri"));
    if uri.is_empty() {
        uri = gs(gp(fd, "file_uri"));
    }
    if uri.is_empty() {
        return None;
    }
    let mut mime_type = gs(gp(fd, "mimeType"));
    if mime_type.is_empty() {
        mime_type = gs(gp(fd, "mime_type"));
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        return Some(json!({"type": "input_image", "image_url": uri}));
    }
    if lower.starts_with("video/")
        || lower.starts_with("application/")
        || lower.starts_with("text/")
    {
        return Some(json!({
            "type": "input_file",
            "file_url": uri,
            "filename": codex_file_name_from_mime(&mime_type)
        }));
    }
    let mut info = format!("File: {uri}");
    if !mime_type.is_empty() {
        info.push_str(&format!(" (Type: {mime_type})"));
    }
    Some(json!({"type": "input_text", "text": info}))
}

// port of codexInputAudioFormatFromMIME (codex_gemini_request.go)
fn codex_input_audio_format_from_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

// port of codexFileNameFromMIME (codex_gemini_request.go)
fn codex_file_name_from_mime(mime_type: &str) -> &'static str {
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

const NAME_LIMIT: usize = 64;

// port of shortenNameIfNeeded (codex_gemini_request.go); also buildShortNameMap's baseCandidate
fn shorten_name_if_needed(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_string();
    }
    if name.starts_with("mcp__") {
        if let Some(idx) = name.rfind("__") {
            if idx > 0 {
                let cand = format!("mcp__{}", &name[idx + 2..]);
                return byte_prefix(&cand, NAME_LIMIT).to_string();
            }
        }
    }
    byte_prefix(name, NAME_LIMIT).to_string()
}

// port of buildShortNameMap (codex_gemini_request.go)
fn build_short_name_map(names: &[String]) -> HashMap<String, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut m = HashMap::new();
    for n in names {
        let cand = shorten_name_if_needed(n);
        let uniq = if !used.contains(&cand) {
            cand
        } else {
            let mut i = 1;
            loop {
                let suffix = format!("_{i}");
                let allowed = NAME_LIMIT.saturating_sub(suffix.len());
                let tmp = format!("{}{suffix}", byte_prefix(&cand, allowed));
                if !used.contains(&tmp) {
                    break tmp;
                }
                i += 1;
            }
        };
        used.insert(uniq.clone());
        m.insert(n.clone(), uniq);
    }
    m
}

// port of buildReverseMapFromGeminiOriginal (codex_gemini_response.go)
fn build_reverse_map_from_gemini_original(original: &Value) -> HashMap<String, String> {
    build_short_name_map(&declared_function_names(original))
        .into_iter()
        .map(|(orig, short)| (short, orig))
        .collect()
}

// ───────────────────────────── response (stream) ─────────────────────────────

/// Port of `ConvertCodexResponseToGeminiParams` (codex_gemini_response.go).
pub struct StreamTranslator {
    model: String,
    created_at: i64,
    response_id: String,
    /// Go keeps ONE buffered function-call chunk; a list here (see push).
    last_storage_output: Vec<Value>,
    has_output_text_delta: bool,
    /// Go rebuilds this per function call from the original request.
    rev_names: HashMap<String, String>,
}

impl StreamTranslator {
    /// `original_request` = the client's ORIGINAL body.
    pub fn new(original_request: &Value) -> Self {
        StreamTranslator {
            // Go's `modelName` argument; a Gemini body carries its model in the
            // URL, so this is normally empty until `response.created` fills it.
            model: gs(gp(original_request, "model")),
            created_at: 0,
            response_id: String::new(),
            last_storage_output: Vec::new(),
            has_output_text_delta: false,
            rev_names: build_reverse_map_from_gemini_original(original_request),
        }
    }

    /// One upstream SSE event: `event` = its `event:` field if present, `data` =
    /// parsed JSON of its `data:` payload. Returns zero or more COMPLETE client
    /// SSE frames (`data: <json>\n\n`).
    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
        let chunks = match (event, gp(data, "type")) {
            (Some(ev), None) if data.is_object() => {
                let mut with_type = data.clone();
                if let Value::Object(m) = &mut with_type {
                    m.insert("type".into(), json!(ev));
                }
                self.convert(&with_type)
            }
            _ => self.convert(data),
        };
        chunks.iter().map(frame).collect()
    }

    /// Upstream stream ended. A Gemini stream has no terminator; a function
    /// call still buffered (no `response.completed` arrived) is flushed here.
    pub fn finish(&mut self) -> Vec<String> {
        std::mem::take(&mut self.last_storage_output)
            .iter()
            .map(frame)
            .collect()
    }

    // port of ConvertCodexResponseToGemini (codex_gemini_response.go)
    fn convert(&mut self, root: &Value) -> Vec<Value> {
        let type_str = gs(gp(root, "type"));

        // Base Gemini response template
        let mut template = json!({
            "candidates": [{"content": {"role": "model", "parts": []}}],
            "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
            "modelVersion": "gemini-2.5-pro",
            "createTime": "2025-08-15T02:52:03.884209Z",
            "responseId": "06CeaPH7NaCU48APvNXDyA4"
        });
        template["modelVersion"] = json!(self.model);
        if let Some(created_at) = gp(root, "response.created_at") {
            self.created_at = gint(Some(created_at));
            template["createTime"] = json!(unix_rfc3339(self.created_at));
        }
        template["responseId"] = json!(self.response_id);

        // Handle function call completion (buffered until the next chunk)
        if type_str == "response.output_item.done" {
            let item = gp(root, "item").unwrap_or(&Value::Null);
            if gs(gp(item, "type")) == "function_call" {
                let mut n = gs(gp(item, "name"));
                if let Some(orig) = self.rev_names.get(&n) {
                    n = orig.clone();
                }
                let mut fc = Map::new();
                fc.insert("name".into(), json!(n));
                fc.insert("args".into(), json!({}));
                let args_str = gs(gp(item, "arguments"));
                if !args_str.is_empty() {
                    if let Ok(args @ Value::Object(_)) = serde_json::from_str::<Value>(&args_str) {
                        fc.insert("args".into(), args);
                    }
                }
                set_gemini_function_call_id(&mut fc, item);

                template["candidates"][0]["content"]["parts"] = json!([{"functionCall": fc}]);
                template["candidates"][0]["finishReason"] = json!("STOP");
                // Go overwrites a single slot here, losing all but the last of
                // several parallel calls; every call is kept instead.
                self.last_storage_output.push(template);
                return Vec::new();
            }
        }

        match type_str.as_str() {
            // A failure delivered inside the stream (`error`, or the
            // terminal `response.failed`): the Gemini client gets an error
            // object, and a function call still buffered is dropped — it
            // belongs to the answer that failed.
            "error" | "response.failed" => {
                self.last_storage_output.clear();
                let err = gp(root, "error")
                    .or_else(|| gp(root, "response.error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut message = gs(gp(&err, "message"));
                if message.is_empty() {
                    message = gs(gp(root, "message"));
                }
                if message.is_empty() {
                    message = gs(gp(&err, "code"));
                }
                if message.is_empty() {
                    message = "upstream request failed".to_string();
                }
                let status = gs(gp(&err, "type"));
                return vec![json!({ "error": {
                    "code": 500,
                    "message": message,
                    "status": if status.is_empty() { "INTERNAL".to_string() } else { status },
                }})];
            }
            "response.created" => {
                let model = gs(gp(root, "response.model"));
                template["modelVersion"] = json!(model);
                template["responseId"] = json!(gs(gp(root, "response.id")));
                self.response_id = gs(gp(root, "response.id"));
                if self.model.is_empty() {
                    self.model = model;
                }
            }
            "response.reasoning_summary_text.delta" => {
                template["candidates"][0]["content"]["parts"] =
                    json!([{"thought": true, "text": gs(gp(root, "delta"))}]);
            }
            "response.output_text.delta" => {
                self.has_output_text_delta = true;
                template["candidates"][0]["content"]["parts"] =
                    json!([{"text": gs(gp(root, "delta"))}]);
            }
            "response.output_item.done" => {
                // Fallback: emit final message text when no delta chunks arrived
                let item = gp(root, "item").unwrap_or(&Value::Null);
                if gs(gp(item, "type")) != "message" || self.has_output_text_delta {
                    return Vec::new();
                }
                let content = match gp(item, "content") {
                    Some(Value::Array(a)) => a,
                    _ => return Vec::new(),
                };
                let mut parts = Vec::new();
                for p in content {
                    if gs(gp(p, "type")) != "output_text" {
                        continue;
                    }
                    let text = gs(gp(p, "text"));
                    if text.is_empty() {
                        continue;
                    }
                    parts.push(json!({"text": text}));
                }
                if parts.is_empty() {
                    return Vec::new();
                }
                template["candidates"][0]["content"]["parts"] = Value::Array(parts);
                self.has_output_text_delta = true;
                return vec![template];
            }
            "response.completed" | "response.incomplete" => {
                let input = gint(gp(root, "response.usage.input_tokens"));
                let output = gint(gp(root, "response.usage.output_tokens"));
                let usage = &mut template["usageMetadata"];
                usage["promptTokenCount"] = json!(input);
                usage["candidatesTokenCount"] = json!(output);
                usage["totalTokenCount"] = json!(input + output);
                if type_str == "response.incomplete" {
                    template["candidates"][0]["finishReason"] =
                        json!(codex_gemini_incomplete_finish_reason(&gs(gp(
                            root,
                            "response.incomplete_details.reason"
                        ))));
                } else if self.last_storage_output.is_empty() {
                    // A text-only answer: the Gemini client needs a
                    // finishReason on the last chunk (Gemini CLI fails a
                    // stream without one as "ended without a finish reason"
                    // and re-sends the prompt). A buffered function call
                    // carries its own STOP.
                    template["candidates"][0]["finishReason"] = json!("STOP");
                }
            }
            _ => return Vec::new(),
        }

        let mut out = std::mem::take(&mut self.last_storage_output);
        out.push(template);
        out
    }
}

fn frame(v: &Value) -> String {
    format!("data: {v}\n\n")
}

// ───────────────────────────── response (non-stream) ─────────────────────────────

// port of ConvertCodexResponseToGeminiNonStream (codex_gemini_response.go)
/// Complete upstream response -> Gemini response. Accepts the terminal event
/// (`{"type":"response.completed","response":{…}}`) or the bare response
/// object; anything else yields `Value::Null` (Go: empty bytes).
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let type_str = gs(gp(upstream, "type"));
    let (response_type, response_data) = if type_str.starts_with("response.") {
        if type_str != "response.completed" && type_str != "response.incomplete" {
            return Value::Null;
        }
        (type_str.clone(), gp(upstream, "response"))
    } else if upstream.is_object() {
        let t = if gs(gp(upstream, "status")) == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        (t.to_string(), Some(upstream))
    } else {
        return Value::Null;
    };

    let mut template = json!({
        "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
        "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
        "modelVersion": "",
        "createTime": "",
        "responseId": ""
    });

    // Go's `modelName` argument: the client's model, else the upstream's.
    let mut model = gs(gp(original_request, "model"));
    if model.is_empty() {
        model = gs(response_data.and_then(|r| gp(r, "model")));
    }
    template["modelVersion"] = json!(model);

    let rd = match response_data {
        Some(r) => r,
        None => return template,
    };
    if response_type == "response.incomplete" {
        template["candidates"][0]["finishReason"] = json!(codex_gemini_incomplete_finish_reason(
            &gs(gp(rd, "incomplete_details.reason"))
        ));
    }
    if let Some(id) = gp(rd, "id") {
        template["responseId"] = json!(gs(Some(id)));
    }
    if let Some(created_at) = gp(rd, "created_at") {
        template["createTime"] = json!(unix_rfc3339(gint(Some(created_at))));
    }
    if let Some(usage) = gp(rd, "usage") {
        let input = gint(gp(usage, "input_tokens"));
        let output = gint(gp(usage, "output_tokens"));
        let u = &mut template["usageMetadata"];
        u["promptTokenCount"] = json!(input);
        u["candidatesTokenCount"] = json!(output);
        u["totalTokenCount"] = json!(input + output);
    }

    if let Some(Value::Array(output)) = gp(rd, "output") {
        let rev = build_reverse_map_from_gemini_original(original_request);
        let mut parts: Vec<Value> = Vec::new();
        let mut pending: Vec<Value> = Vec::new();
        for value in output {
            match gs(gp(value, "type")).as_str() {
                "reasoning" => {
                    parts.append(&mut pending);
                    if let Some(content) = gp(value, "content") {
                        parts.push(json!({"text": gs(Some(content)), "thought": true}));
                    }
                }
                "message" => {
                    parts.append(&mut pending);
                    if let Some(Value::Array(content)) = gp(value, "content") {
                        for c in content {
                            if gs(gp(c, "type")) == "output_text" {
                                if let Some(text) = gp(c, "text") {
                                    parts.push(json!({"text": gs(Some(text))}));
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let mut n = gs(gp(value, "name"));
                    if let Some(orig) = rev.get(&n) {
                        n = orig.clone();
                    }
                    let mut fc = Map::new();
                    fc.insert("args".into(), json!({}));
                    fc.insert("name".into(), json!(n));
                    let args_str = gs(gp(value, "arguments"));
                    if !args_str.is_empty() {
                        if let Ok(args @ Value::Object(_)) =
                            serde_json::from_str::<Value>(&args_str)
                        {
                            fc.insert("args".into(), args);
                        }
                    }
                    set_gemini_function_call_id(&mut fc, value);
                    pending.push(json!({"functionCall": fc}));
                }
                _ => {}
            }
        }
        parts.append(&mut pending);
        if !parts.is_empty() {
            template["candidates"][0]["content"]["parts"] = Value::Array(parts);
        }
    }
    template
}

// port of setGeminiFunctionCallID (codex_gemini_response.go)
fn set_gemini_function_call_id(fc: &mut Map<String, Value>, item: &Value) {
    let call_id = gs(gp(item, "call_id")).trim().to_string();
    if !call_id.is_empty() {
        fc.insert("id".into(), json!(call_id));
        return;
    }
    let id = gs(gp(item, "id")).trim().to_string();
    if !id.is_empty() {
        fc.insert("id".into(), json!(id));
    }
}

// port of codexGeminiIncompleteFinishReason (codex_gemini_response.go)
fn codex_gemini_incomplete_finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "max_output_tokens" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "OTHER",
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn input_of(out: &Value) -> &Vec<Value> {
        out["input"].as_array().unwrap()
    }

    // port of TestConvertGeminiRequestToCodex_PreservesCustomCallIDs
    // A `response.failed` mid-stream reaches the Gemini client as an error
    // object; a buffered function call of the failed answer is dropped.
    #[test]
    fn stream_response_failed_becomes_error() {
        let mut t = StreamTranslator::new(&json!({}));
        t.push(None, &json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "c1", "name": "lookup", "arguments": "{}"}}));
        let out = t.push(
            None,
            &json!({"type": "response.failed", "response": {"status": "failed",
            "error": {"code": "server_error", "message": "The model produced invalid output."}}}),
        );
        assert_eq!(
            out,
            vec!["data: {\"error\":{\"code\":500,\"message\":\"The model produced invalid output.\",\"status\":\"INTERNAL\"}}\n\n"]
        );
        assert!(t.finish().is_empty());
    }

    #[test]
    fn preserves_custom_call_ids() {
        for (field, want) in [
            ("id", "call_gateway_id"),
            ("call_id", "call_gateway_call_id"),
        ] {
            let raw = json!({"contents": [
                {"role": "model", "parts": [{"functionCall": {"name": "lookup", field: want, "args": {"query": "status"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "lookup", field: want, "response": {"result": "ok"}}}]}
            ]});
            let out = translate_request("gpt-5.1-codex", &raw, false);
            assert_eq!(
                input_of(&out),
                &vec![
                    json!({"type": "function_call", "name": "lookup", "arguments": "{\"query\":\"status\"}", "call_id": want}),
                    json!({"type": "function_call_output", "output": "ok", "call_id": want}),
                ]
            );
        }
    }

    // port of TestConvertGeminiRequestToCodex_AcceptsInlineData
    #[test]
    fn accepts_inline_data() {
        let out = translate_request(
            "gpt-5.1-codex",
            &json!({"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}]}),
            false,
        );
        assert_eq!(
            input_of(&out),
            &vec![
                json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}]})
            ]
        );
    }

    // port of TestConvertGeminiRequestToCodex_SplitsNonImageInlineDataByMIME
    #[test]
    fn splits_non_image_inline_data_by_mime() {
        let out = translate_request(
            "gpt-5.1-codex",
            &json!({"contents":[{"role":"user","parts":[
                {"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}},
                {"inlineData":{"mimeType":"video/mp4","data":"AAAAIGZ0eXA="}},
                {"inlineData":{"mimeType":"application/pdf","data":"JVBERi0="}}
            ]}]}),
            false,
        );
        assert_eq!(
            input_of(&out),
            &vec![
                json!({"type":"message","role":"user","content":[{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}]}),
                json!({"type":"message","role":"user","content":[{"type":"input_file","file_data":"AAAAIGZ0eXA=","filename":"video"}]}),
                json!({"type":"message","role":"user","content":[{"type":"input_file","file_data":"JVBERi0=","filename":"document.pdf"}]}),
            ]
        );
    }

    // port of TestConvertGeminiRequestToCodex_DropsHiddenThoughtParts ("thought-only turn")
    #[test]
    fn drops_thought_only_turn() {
        let out = translate_request(
            "codex-test",
            &json!({"contents":[
                {"role":"model","parts":[{"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"}]},
                {"role":"user","parts":[{"text":"continue"}]}
            ]}),
            false,
        );
        assert_eq!(
            input_of(&out),
            &vec![
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]})
            ]
        );
    }

    // port of TestConvertGeminiRequestToCodex_DropsHiddenThoughtParts ("mixed turn")
    #[test]
    fn drops_thought_in_mixed_turn() {
        let out = translate_request(
            "codex-test",
            &json!({"contents":[{"role":"model","parts":[
                {"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"},
                {"text":"visible answer"}
            ]}]}),
            false,
        );
        assert_eq!(
            input_of(&out),
            &vec![
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"visible answer"}]})
            ]
        );
    }

    // port of TestConvertGeminiRequestToCodex_DeterministicCallIDs
    #[test]
    fn deterministic_call_ids() {
        let raw = json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "first_tool", "args": {"q": "one"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "first_tool", "response": {"result": "ok1"}}}]},
            {"role": "model", "parts": [{"functionCall": {"name": "second_tool", "args": {"q": "two"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "second_tool", "response": {"result": "ok2"}}}]}
        ]});
        let out1 = translate_request("gpt-5.1-codex", &raw, false);
        let out2 = translate_request("gpt-5.1-codex", &raw, false);
        assert_eq!(out1.to_string(), out2.to_string());
        let ids: Vec<&str> = input_of(&out1)
            .iter()
            .map(|i| i["call_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                "call_gemini_0000000000000001",
                "call_gemini_0000000000000001",
                "call_gemini_0000000000000002",
                "call_gemini_0000000000000002"
            ]
        );
    }

    #[test]
    fn full_request_shape() {
        let raw = json!({
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}, {"fileData": {"fileUri": "gs://b/x.bin"}}]},
                {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"temp": 20}}}]}
            ],
            "tools": [{"functionDeclarations": [{
                "name": "get_weather",
                "description": "weather",
                "parameters": {"$schema": "x", "type": "OBJECT", "properties": {"city": {"type": "STRING"}}}
            }]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY"}},
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 2000}},
            "service_tier": "Fast"
        });
        let out = translate_request("gpt-5.1-codex", &raw, true);
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.1-codex",
                "instructions": "",
                "input": [
                    {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "be brief"}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "File: gs://b/x.bin"}]},
                    {"type": "function_call", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "call_id": "call_gemini_0000000000000001"},
                    {"type": "function_call_output", "output": "{\"temp\":20}", "call_id": "call_gemini_0000000000000001"}
                ],
                "service_tier": "priority",
                "tool_choice": "required",
                "tools": [{
                    "type": "function",
                    "name": "get_weather",
                    "description": "weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "additionalProperties": false},
                    "strict": false
                }],
                "parallel_tool_calls": true,
                "reasoning": {"effort": "medium"},
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    #[test]
    fn long_tool_names_shortened_and_restored() {
        let long = format!("mcp__server__{}", "x".repeat(70));
        let original = json!({"tools": [{"functionDeclarations": [{"name": long}]}]});
        let req = translate_request("m", &original, false);
        let short = format!("mcp__{}", "x".repeat(59));
        assert_eq!(req["tools"][0]["name"], json!(short));
        let resp = json!({"type": "response.completed", "response": {"output": [
            {"type": "function_call", "call_id": "c1", "name": short, "arguments": "{}"}
        ]}});
        let out = translate_non_stream(&resp, &original);
        assert_eq!(
            out["candidates"][0]["content"]["parts"],
            json!([{"functionCall": {"args": {}, "name": long, "id": "c1"}}])
        );
    }

    // port of TestCleanGeminiCodexToolParametersPreservesCanonicalSchema
    #[test]
    fn clean_parameters_preserves_canonical_schema() {
        let input = json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false});
        let output = clean_gemini_codex_tool_parameters(&input);
        assert_eq!(output, input);
        assert_eq!(output.to_string(), input.to_string());
    }

    // port of TestSetCodexToolChoiceFromGeminiToolConfigReusesAutoChoice
    #[test]
    fn tool_choice_auto_is_unchanged() {
        let mut out = json!({"tool_choice":"auto","input":[]})
            .as_object()
            .unwrap()
            .clone();
        set_codex_tool_choice_from_gemini_tool_config(&mut out, Some(&json!({"mode":"AUTO"})));
        assert_eq!(Value::Object(out), json!({"tool_choice":"auto","input":[]}));
    }

    // port of TestCleanGeminiCodexToolParametersNormalizesSchema
    #[test]
    fn clean_parameters_normalizes_schema() {
        let output = clean_gemini_codex_tool_parameters(
            &json!({"type":"object","$schema":"draft","additionalProperties":true}),
        );
        assert_eq!(
            output,
            json!({"type":"object","additionalProperties":false})
        );
    }

    fn parse_frames(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| {
                assert!(
                    f.starts_with("data: ") && f.ends_with("\n\n"),
                    "bad frame {f:?}"
                );
                serde_json::from_str(&f[6..f.len() - 2]).unwrap()
            })
            .collect()
    }

    // port of TestConvertCodexResponseToGemini_IncompleteTerminal
    #[test]
    fn incomplete_terminal() {
        let terminal = json!({"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-5.5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}});
        let mut t = StreamTranslator::new(&Value::Null);
        let out = parse_frames(&t.push(None, &terminal));
        assert_eq!(
            out,
            vec![json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "MAX_TOKENS"}],
                "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT", "promptTokenCount": 1, "candidatesTokenCount": 2, "totalTokenCount": 3},
                "modelVersion": "",
                "createTime": "2025-08-15T02:52:03.884209Z",
                "responseId": ""
            })]
        );
        let ns = translate_non_stream(&terminal, &Value::Null);
        assert_eq!(
            ns,
            json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "MAX_TOKENS"}],
                "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT", "promptTokenCount": 1, "candidatesTokenCount": 2, "totalTokenCount": 3},
                "modelVersion": "gpt-5.5",
                "createTime": "",
                "responseId": "resp_1"
            })
        );
    }

    // port of TestConvertCodexResponseToGemini_StreamEmptyOutputUsesOutputItemDoneMessageFallback
    #[test]
    fn stream_output_item_done_message_fallback() {
        let original = json!({"tools": []});
        let mut t = StreamTranslator::new(&original);
        let mut out = t.push(
            None,
            &json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":0}),
        );
        out.extend(t.push(None, &json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}})));
        let out = parse_frames(&out);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"],
            json!([{"text": "ok"}])
        );
    }

    // port of TestConvertCodexResponseToGemini_StreamPreservesFunctionCallID
    #[test]
    fn stream_preserves_function_call_id() {
        let original = json!({"tools": []});
        let mut t = StreamTranslator::new(&original);
        let out = t.push(
            None,
            &json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_gateway","name":"lookup","arguments":"{\"query\":\"status\"}"}}),
        );
        assert!(out.is_empty());
        let out = parse_frames(&t.push(
            None,
            &json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
        ));
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"],
            json!([{"functionCall": {"name": "lookup", "args": {"query": "status"}, "id": "call_gateway"}}])
        );
    }

    // port of TestConvertCodexResponseToGeminiNonStreamPreservesFunctionCallID
    #[test]
    fn non_stream_preserves_function_call_id() {
        let raw = json!({"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_gateway","name":"lookup","arguments":"{\"query\":\"status\"}"}]}});
        let out = translate_non_stream(&raw, &json!({"tools": []}));
        assert_eq!(
            out,
            json!({
                "candidates": [{"content": {"role": "model", "parts": [
                    {"functionCall": {"args": {"query": "status"}, "name": "lookup", "id": "call_gateway"}}
                ]}, "finishReason": "STOP"}],
                "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT", "promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
                "modelVersion": "",
                "createTime": "2023-11-14T22:13:20Z",
                "responseId": "resp_123"
            })
        );
    }

    #[test]
    fn non_stream_bare_response_object() {
        let raw = json!({"id": "r", "object": "response", "model": "gpt-5.1-codex", "status": "completed", "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}
        ]});
        let out = translate_non_stream(&raw, &json!({}));
        assert_eq!(
            out["candidates"][0]["content"]["parts"],
            json!([{"text": "hi"}])
        );
        assert_eq!(out["modelVersion"], json!("gpt-5.1-codex"));
        assert_eq!(
            translate_non_stream(&json!({"type": "response.created"}), &json!({})),
            Value::Null
        );
    }

    #[test]
    fn stream_parallel_function_calls_all_emitted() {
        let mut t = StreamTranslator::new(&json!({}));
        for (id, name) in [("c1", "a"), ("c2", "b")] {
            let ev = json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":id,"name":name,"arguments":"{}"}});
            assert!(t.push(None, &ev).is_empty());
        }
        let out = parse_frames(&t.push(None, &json!({"type":"response.completed","response":{}})));
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"][0]["functionCall"]["id"],
            json!("c1")
        );
        assert_eq!(
            out[1]["candidates"][0]["content"]["parts"][0]["functionCall"]["id"],
            json!("c2")
        );
        assert!(t.finish().is_empty());
    }

    #[test]
    fn end_to_end_stream() {
        let original = json!({
            "contents": [{"role": "user", "parts": [{"text": "weather in Paris?"}]}],
            "tools": [{"functionDeclarations": [{"name": "get_weather", "parameters": {"type": "OBJECT"}}]}]
        });
        let mut t = StreamTranslator::new(&original);
        let events = [
            (
                Some("response.created"),
                json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_1","object":"response","created_at":1700000000,"status":"in_progress","model":"gpt-5.1-codex","output":[]}}),
            ),
            (
                Some("response.output_item.added"),
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}),
            ),
            (
                Some("response.reasoning_summary_text.delta"),
                json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"Checking"}),
            ),
            (
                Some("response.output_item.done"),
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"Checking"}]}}),
            ),
            (
                Some("response.output_text.delta"),
                json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"Let me look."}),
            ),
            (
                Some("response.output_item.done"),
                json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Let me look."}]}}),
            ),
            (
                Some("response.function_call_arguments.delta"),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"{\"city\":"}),
            ),
            (
                Some("response.output_item.done"),
                json!({"type":"response.output_item.done","output_index":2,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}),
            ),
            (
                Some("response.completed"),
                json!({"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
            ),
        ];
        let mut frames = Vec::new();
        for (ev, data) in &events {
            frames.extend(t.push(*ev, data));
        }
        frames.extend(t.finish());
        let lit = "2025-08-15T02:52:03.884209Z";
        let ts = "2023-11-14T22:13:20Z";
        let expected = vec![
            format!(
                r#"data: {{"candidates":[{{"content":{{"role":"model","parts":[]}}}}],"usageMetadata":{{"trafficType":"PROVISIONED_THROUGHPUT"}},"modelVersion":"gpt-5.1-codex","createTime":"{ts}","responseId":"resp_1"}}"#
            ),
            format!(
                r#"data: {{"candidates":[{{"content":{{"role":"model","parts":[{{"thought":true,"text":"Checking"}}]}}}}],"usageMetadata":{{"trafficType":"PROVISIONED_THROUGHPUT"}},"modelVersion":"gpt-5.1-codex","createTime":"{lit}","responseId":"resp_1"}}"#
            ),
            format!(
                r#"data: {{"candidates":[{{"content":{{"role":"model","parts":[{{"text":"Let me look."}}]}}}}],"usageMetadata":{{"trafficType":"PROVISIONED_THROUGHPUT"}},"modelVersion":"gpt-5.1-codex","createTime":"{lit}","responseId":"resp_1"}}"#
            ),
            format!(
                r#"data: {{"candidates":[{{"content":{{"role":"model","parts":[{{"functionCall":{{"name":"get_weather","args":{{"city":"Paris"}},"id":"call_1"}}}}]}},"finishReason":"STOP"}}],"usageMetadata":{{"trafficType":"PROVISIONED_THROUGHPUT"}},"modelVersion":"gpt-5.1-codex","createTime":"{lit}","responseId":"resp_1"}}"#
            ),
            format!(
                r#"data: {{"candidates":[{{"content":{{"role":"model","parts":[]}}}}],"usageMetadata":{{"trafficType":"PROVISIONED_THROUGHPUT","promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}},"modelVersion":"gpt-5.1-codex","createTime":"{ts}","responseId":"resp_1"}}"#
            ),
        ];
        let expected: Vec<String> = expected.into_iter().map(|s| s + "\n\n").collect();
        assert_eq!(frames, expected);
    }
}
