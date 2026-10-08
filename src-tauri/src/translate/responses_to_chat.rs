//! OpenAI Responses (client) ⇄ OpenAI Chat Completions (upstream) translator.
//!
//! Faithful port of CLIProxyAPI's `internal/translator/openai/openai/responses`
//! package (commit ed980be), registered upstream as
//! `translator.Register(OpenaiResponse, OpenAI, ConvertOpenAIResponsesRequestToOpenAIChatCompletions,
//! {Stream: ConvertOpenAIChatCompletionsResponseToOpenAIResponses,
//!  NonStream: ConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream})`.
//!
//! The client speaks `/v1/responses` (e.g. Codex CLI); the upstream speaks
//! `/v1/chat/completions` (e.g. DeepSeek). Every ported function carries a
//! `// port of <GoFunc> (<file>)` comment so it can be diffed against the source.
//!
//! The Go code works on raw bytes with gjson/sjson; this port works on
//! `serde_json::Value`. The `g*` helpers below reproduce the gjson accessor
//! semantics the Go code relies on (`Exists()` is true for a JSON null,
//! `String()` of a number/object is its JSON text, and so on).
//!
//! Deliberately NOT ported: video inputs and image/video generation tools,
//! `model(level)` thinking-suffix parsing, metrics/logging, config hooks and
//! performance caches.

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// gjson-compatible accessors
// ---------------------------------------------------------------------------

/// gjson `Get(path)` for plain dotted paths (object keys and array indexes).
fn gpath<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
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

/// gjson `Result.String()`.
fn gstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

fn gs(v: &Value, path: &str) -> String {
    gstr(gpath(v, path))
}

/// gjson `Result.Int()`.
fn gint(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::Bool(true)) => 1,
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// gjson `Result.Float()`.
fn gfloat(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::Bool(true)) => 1.0,
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// gjson `Result.Bool()`.
fn gbool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "t" | "T" | "true" | "TRUE" | "True"),
        _ => false,
    }
}

/// A float as Go's encoder would print it (integral values carry no fraction).
fn float_value(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 {
        json!(f as i64)
    } else {
        json!(f)
    }
}

fn is_array(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Array(_)))
}

fn as_array(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    }
}

fn set(obj: &mut Value, key: &str, val: Value) {
    if let Value::Object(m) = obj {
        m.insert(key.to_string(), val);
    }
}

fn remove(obj: &mut Value, key: &str) {
    if let Value::Object(m) = obj {
        m.shift_remove(key);
    }
}

fn is_valid_json(s: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

// ---------------------------------------------------------------------------
// translator/common helpers
// ---------------------------------------------------------------------------

// port of ExtractResponsesCallID (internal/translator/common/responses.go)
fn extract_responses_call_id(node: &Value) -> String {
    for key in ["call_id", "tool_call_id", "callId"] {
        let v = gs(node, key);
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    let id = gs(node, "id");
    let id = id.trim();
    if id.starts_with("fco_") {
        return String::new();
    }
    id.to_string()
}

fn is_tool_output_type(t: &str) -> bool {
    t == "function_call_output" || t == "custom_tool_call_output"
}

// port of NormalizeResponsesToolCallOutputs (internal/translator/common/responses.go)
fn normalize_responses_tool_call_outputs(items: &[Value]) -> Vec<Value> {
    let mut normalized = items.to_vec();
    if normalized.is_empty() {
        return normalized;
    }

    let mut explicit_output_counts: HashMap<String, i64> = HashMap::new();
    for item in items {
        if is_tool_output_type(&gs(item, "type")) {
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
        let item_type = gs(&normalized[i], "type");
        match item_type.as_str() {
            "function_call" | "custom_tool_call" => {
                let call_id = extract_responses_call_id(&normalized[i]);
                if !call_id.is_empty() {
                    pending_call_ids.push(call_id.clone());
                    pending_call_names.insert(call_id, gs(&normalized[i], "name"));
                }
                i += 1;
            }
            "function_call_output" | "custom_tool_call_output" => {
                let start = i;
                while i < normalized.len() && is_tool_output_type(&gs(&normalized[i], "type")) {
                    i += 1;
                }
                let outputs: Vec<Value> = normalized[start..i].to_vec();

                if !pending_call_ids.is_empty() {
                    let mut used = vec![false; outputs.len()];
                    let mut matched_for_pending: Vec<isize> = vec![-1; pending_call_ids.len()];
                    let explicit = |counts: &HashMap<String, i64>, id: &str| -> i64 {
                        counts.get(id).copied().unwrap_or(0)
                    };

                    // Pass 1: exact explicit call ID match
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        for (out_idx, out) in outputs.iter().enumerate() {
                            if !used[out_idx] && extract_responses_call_id(out) == *pending_id {
                                used[out_idx] = true;
                                matched_for_pending[pending_idx] = out_idx as isize;
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
                            || explicit(&explicit_output_counts, pending_id) > 0
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
                                    let out_name = gs(out, "name");
                                    let out_name = out_name.trim();
                                    if !out_name.is_empty() && out_name == expected_name {
                                        used[out_idx] = true;
                                        matched_for_pending[pending_idx] = out_idx as isize;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // Pass 3: FIFO fallback for outputs with no explicit call ID
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        if matched_for_pending[pending_idx] >= 0
                            || explicit(&explicit_output_counts, pending_id) > 0
                        {
                            continue;
                        }
                        for (out_idx, out) in outputs.iter().enumerate() {
                            if !used[out_idx] && extract_responses_call_id(out).is_empty() {
                                let out_name = gs(out, "name");
                                let out_name = out_name.trim();
                                let expected_name = pending_call_names
                                    .get(pending_id)
                                    .map(String::as_str)
                                    .unwrap_or("");
                                if out_name.is_empty()
                                    || expected_name.is_empty()
                                    || out_name == expected_name
                                {
                                    used[out_idx] = true;
                                    matched_for_pending[pending_idx] = out_idx as isize;
                                    break;
                                }
                            }
                        }
                    }

                    // Apply matched call_ids to outputs
                    let mut remaining_pending = Vec::new();
                    for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                        let out_idx = matched_for_pending[pending_idx];
                        if out_idx < 0 {
                            remaining_pending.push(pending_id.clone());
                            continue;
                        }
                        let out_idx = out_idx as usize;
                        let matched_out = &outputs[out_idx];
                        if gs(matched_out, "call_id") != *pending_id {
                            let mut raw = matched_out.clone();
                            set(&mut raw, "call_id", json!(pending_id));
                            normalized[start + out_idx] = raw;
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

// port of AlignOpenAIToolCallMessages (internal/translator/common/openai_tools.go)
fn align_openai_tool_call_messages(
    messages: Vec<Value>,
    extra_ambiguous_ids: &[String],
) -> Vec<Value> {
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
    let mut ambiguous_call_ids: HashSet<String> = HashSet::new();
    for id in extra_ambiguous_ids {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            ambiguous_call_ids.insert(trimmed.to_string());
        }
    }
    let mut tool_msg_indices_by_call_id: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, raw) in messages.iter().enumerate() {
        match gs(raw, "role").as_str() {
            "assistant" => {
                let raw_calls = as_array(gpath(raw, "tool_calls"));
                if !raw_calls.is_empty() {
                    let mut call_ids = Vec::new();
                    let mut has_empty_call_id = false;
                    for tc in raw_calls {
                        let call_id = gs(tc, "id");
                        if call_id.is_empty() {
                            ambiguous_call_ids.insert(String::new());
                            has_empty_call_id = true;
                            continue;
                        }
                        if assistant_by_call_id.contains_key(&call_id) {
                            ambiguous_call_ids.insert(call_id.clone());
                        }
                        assistant_by_call_id.insert(call_id.clone(), i);
                        call_ids.push(call_id);
                    }
                    if !call_ids.is_empty() || has_empty_call_id {
                        assistants.push(AssistantRecord {
                            msg_index: i,
                            call_ids,
                            has_invalid_or_empty_id: has_empty_call_id,
                        });
                    }
                }
            }
            "tool" => {
                let call_id = gs(raw, "tool_call_id");
                if call_id.is_empty() {
                    ambiguous_call_ids.insert(String::new());
                } else {
                    let entry = tool_msg_indices_by_call_id
                        .entry(call_id.clone())
                        .or_default();
                    entry.push(i);
                    if entry.len() > 1 {
                        ambiguous_call_ids.insert(call_id);
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
    let mut needs_reorder = false;

    for ast in &assistants {
        if ast.has_invalid_or_empty_id {
            continue;
        }
        let mut is_eligible = true;
        let mut matched_tool_indices = Vec::with_capacity(ast.call_ids.len());
        for call_id in &ast.call_ids {
            if ambiguous_call_ids.contains(call_id) {
                is_eligible = false;
                break;
            }
            let indices = tool_msg_indices_by_call_id
                .get(call_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if indices.len() != 1 {
                is_eligible = false;
                break;
            }
            let tool_idx = indices[0];
            if tool_idx <= ast.msg_index {
                is_eligible = false;
                break;
            }
            matched_tool_indices.push(tool_idx);
        }
        if !is_eligible {
            continue;
        }
        matched_tool_indices.sort_unstable();
        let already_adjacent = matched_tool_indices
            .iter()
            .enumerate()
            .all(|(offset, &tool_idx)| tool_idx == ast.msg_index + offset + 1);
        if !already_adjacent {
            needs_reorder = true;
            groups.push((ast.msg_index, matched_tool_indices));
        }
    }

    if !needs_reorder {
        return messages;
    }

    let mut moved_tool_indices: HashSet<usize> = HashSet::new();
    let mut tools_to_insert: HashMap<usize, Vec<usize>> = HashMap::new();
    for (assistant_index, tool_indices) in groups {
        for &idx in &tool_indices {
            moved_tool_indices.insert(idx);
        }
        tools_to_insert.insert(assistant_index, tool_indices);
    }

    let mut reordered = Vec::with_capacity(messages.len());
    for i in 0..messages.len() {
        if moved_tool_indices.contains(&i) {
            continue;
        }
        reordered.push(messages[i].clone());
        if let Some(tools) = tools_to_insert.get(&i) {
            for &t in tools {
                reordered.push(messages[t].clone());
            }
        }
    }
    reordered
}

// port of SetResponsesToolCallIdentity (internal/translator/common/responses.go)
fn set_responses_tool_call_identity(item: &mut Value, name: &str, namespace: &str) {
    set(item, "name", json!(name));
    if !namespace.is_empty() {
        set(item, "namespace", json!(namespace));
    } else {
        remove(item, "namespace");
    }
}

// port of RequestModelName / requestModelName (internal/translator/common/request.go)
fn request_model_name(original: Option<&Value>, request: Option<&Value>) -> String {
    for raw in [original, request].into_iter().flatten() {
        for path in ["model", "request.model"] {
            if let Some(Value::String(model)) = gpath(raw, path) {
                if !model.trim().is_empty() {
                    return model.clone();
                }
            }
        }
    }
    String::new()
}

// port of SSEEventData (internal/translator/common/bytes.go); the frame
// terminator the Go stream writer appends is included here.
fn emit_resp_event(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

// ---------------------------------------------------------------------------
// openai_openai-responses_tools.go + responses_tool_index.go
// ---------------------------------------------------------------------------

// port of responsesChatToolNameLimit (openai_openai-responses_tools.go)
const RESPONSES_CHAT_TOOL_NAME_LIMIT: usize = 64;

// port of responsesToolDeclaration (openai_openai-responses_tools.go)
#[derive(Clone, Debug)]
struct ToolDeclaration {
    tool: Value,
    chat_name: String,
    local_name: String,
    namespace: String,
    custom: bool,
}

// port of walkResponsesToolDeclarations (openai_openai-responses_tools.go)
fn walk_responses_tool_declarations(root: &Value) -> Vec<ToolDeclaration> {
    let mut declarations: Vec<ToolDeclaration> = Vec::new();
    let emit = |declarations: &mut Vec<ToolDeclaration>, tool: &Value, namespace_name: &str| {
        let custom = match gs(tool, "type").trim() {
            "" | "function" => false,
            "custom" => true,
            _ => return,
        };
        let local_name = responses_tool_name(tool);
        if local_name.is_empty() {
            return;
        }
        declarations.push(ToolDeclaration {
            tool: tool.clone(),
            chat_name: qualify_responses_namespace_tool_name(namespace_name, &local_name),
            local_name,
            namespace: namespace_name.to_string(),
            custom,
        });
    };
    let scan = |declarations: &mut Vec<ToolDeclaration>, tools: Option<&Value>| {
        for tool in as_array(tools) {
            if gs(tool, "type").trim() == "namespace" {
                if let Some(Value::Array(children)) = gpath(tool, "tools") {
                    let namespace_name = gs(tool, "name").trim().to_string();
                    for child in children {
                        emit(declarations, child, &namespace_name);
                    }
                }
                continue;
            }
            emit(declarations, tool, "");
        }
    };

    scan(&mut declarations, gpath(root, "tools"));
    for item in as_array(gpath(root, "input")) {
        if gs(item, "type") == "additional_tools" {
            scan(&mut declarations, gpath(item, "tools"));
        }
    }

    disambiguate_responses_chat_tool_names(&mut declarations);
    declarations
}

// port of disambiguateResponsesChatToolNames (openai_openai-responses_tools.go)
fn disambiguate_responses_chat_tool_names(declarations: &mut [ToolDeclaration]) {
    let mut claimed: HashMap<String, String> = HashMap::new();
    let claim = |claimed: &mut HashMap<String, String>, candidate: &str, identity: &str| -> bool {
        match claimed.get(candidate) {
            None => {
                claimed.insert(candidate.to_string(), identity.to_string());
                true
            }
            Some(owner) => owner == identity,
        }
    };
    let mut long_declarations = Vec::new();
    let mut identities = Vec::with_capacity(declarations.len());
    // localName → the single identity that declares it, or "" once ambiguous.
    // A Vec keeps the (order-insensitive) reservation pass deterministic.
    let mut local_owners: Vec<(String, String)> = Vec::new();
    for (i, d) in declarations.iter().enumerate() {
        let identity = raw_responses_namespace_qualified_name(&d.namespace, &d.local_name);
        identities.push(identity.clone());
        if identity.len() > RESPONSES_CHAT_TOOL_NAME_LIMIT {
            long_declarations.push(i);
        } else {
            claim(&mut claimed, &identity, &identity);
        }
        let local = &d.local_name;
        if local.is_empty() || *local == identity || local.len() > RESPONSES_CHAT_TOOL_NAME_LIMIT {
            continue;
        }
        match local_owners.iter_mut().find(|(l, _)| l == local) {
            None => local_owners.push((local.clone(), identity.clone())),
            Some((_, owner)) => {
                if !owner.is_empty() && *owner != identity {
                    owner.clear();
                }
            }
        }
    }
    let mut ambiguous_local_names: HashSet<String> = HashSet::new();
    for (local, owner) in &local_owners {
        claim(&mut claimed, local, owner);
        if owner.is_empty() {
            ambiguous_local_names.insert(local.clone());
        }
    }
    for i in long_declarations {
        let identity = identities[i].clone();
        let name = declarations[i].chat_name.clone();
        if !ambiguous_local_names.contains(&name) && claim(&mut claimed, &name, &identity) {
            continue;
        }
        let mut suffix = 1;
        loop {
            let candidate = cap_responses_chat_tool_name(&format!("{name}_{suffix}"));
            suffix += 1;
            if ambiguous_local_names.contains(&candidate) {
                continue;
            }
            if claim(&mut claimed, &candidate, &identity) {
                declarations[i].chat_name = candidate;
                break;
            }
        }
    }
}

// port of convertResponsesCustomToolToOpenAIChat (openai_openai-responses_tools.go)
fn convert_responses_custom_tool_to_openai_chat(
    tool: &Value,
    override_name: &str,
) -> Option<Value> {
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = responses_tool_name(tool);
    }
    if name.is_empty() {
        return None;
    }
    let mut chat_tool = json!({"type":"function","function":{"name":"","description":"","parameters":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}});
    chat_tool["function"]["name"] = json!(name);
    let description = responses_tool_description(tool);
    if !description.is_empty() {
        chat_tool["function"]["description"] = json!(description);
    }
    Some(chat_tool)
}

// port of convertResponsesFunctionToolToOpenAIChat (openai_openai-responses_tools.go)
fn convert_responses_function_tool_to_openai_chat(
    tool: &Value,
    override_name: &str,
) -> Option<Value> {
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = responses_tool_name(tool);
    }
    if name.is_empty() {
        return None;
    }
    let mut chat_tool =
        json!({"type":"function","function":{"name":"","description":"","parameters":{}}});
    chat_tool["function"]["name"] = json!(name);
    let description = responses_tool_description(tool);
    if !description.is_empty() {
        chat_tool["function"]["description"] = json!(description);
    }
    if let Some(parameters) = responses_tool_parameters(tool) {
        chat_tool["function"]["parameters"] = parameters.clone();
    }
    Some(chat_tool)
}

// port of responsesToolName (openai_openai-responses_tools.go)
fn responses_tool_name(tool: &Value) -> String {
    let name = gs(tool, "name");
    if !name.trim().is_empty() {
        return name.trim().to_string();
    }
    gs(tool, "function.name").trim().to_string()
}

// port of responsesToolDescription (openai_openai-responses_tools.go)
fn responses_tool_description(tool: &Value) -> String {
    let description = gs(tool, "description");
    if !description.is_empty() {
        return description;
    }
    gs(tool, "function.description")
}

// port of responsesToolParameters (openai_openai-responses_tools.go)
fn responses_tool_parameters(tool: &Value) -> Option<&Value> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .iter()
    .find_map(|path| gpath(tool, path))
}

// port of responsesToolOutputText (openai_openai-responses_tools.go)
fn responses_tool_output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut b = String::new();
            for part in parts {
                if let Value::String(s) = part {
                    b.push_str(s);
                    continue;
                }
                if let Some(text) = gpath(part, "text") {
                    b.push_str(&gstr(Some(text)));
                }
            }
            b
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

// port of unwrapCustomToolInput (openai_openai-responses_tools.go)
fn unwrap_custom_tool_input(arguments: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(arguments) {
        match gpath(&parsed, "input") {
            Some(Value::String(s)) => return s.clone(),
            Some(other) => return other.to_string(),
            None => {}
        }
    }
    arguments.to_string()
}

// port of qualifyResponsesNamespaceToolName (openai_openai-responses_tools.go)
fn qualify_responses_namespace_tool_name(namespace_name: &str, child_name: &str) -> String {
    cap_responses_chat_tool_name(&raw_responses_namespace_qualified_name(
        namespace_name,
        child_name,
    ))
}

// port of rawResponsesNamespaceQualifiedName (openai_openai-responses_tools.go)
fn raw_responses_namespace_qualified_name(namespace_name: &str, child_name: &str) -> String {
    let child_name = child_name.trim();
    if child_name.is_empty() || namespace_name.is_empty() || child_name.starts_with("mcp__") {
        return child_name.to_string();
    }
    if child_name.starts_with(namespace_name) {
        return child_name.to_string();
    }
    if namespace_name.ends_with("__") {
        return format!("{namespace_name}{child_name}");
    }
    format!("{namespace_name}__{child_name}")
}

// port of capResponsesChatToolName (openai_openai-responses_tools.go)
//
// Go slices bytes; for a non-ASCII name that could split a character, so the
// cut moves forward to the next char boundary instead of producing invalid
// UTF-8.
fn cap_responses_chat_tool_name(name: &str) -> String {
    if name.len() <= RESPONSES_CHAT_TOOL_NAME_LIMIT {
        return name.to_string();
    }
    let mut start = name.len() - RESPONSES_CHAT_TOOL_NAME_LIMIT;
    while !name.is_char_boundary(start) {
        start += 1;
    }
    let truncated = &name[start..];
    let trimmed = truncated.trim_start_matches(['_', '-']);
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    truncated.to_string()
}

// port of responsesToolIndex (responses_tool_index.go)
#[derive(Clone, Debug, Default)]
struct ResponsesToolIndex {
    declarations: Vec<ToolDeclaration>,
    by_chat: HashMap<String, ToolDeclaration>,
    by_identity: HashMap<(String, String), String>,
    by_raw: HashMap<String, String>,
    /// Empty string means multiple distinct emitted tools.
    by_local: HashMap<String, String>,
    custom: HashSet<String>,
}

impl ResponsesToolIndex {
    // port of newResponsesToolIndex (responses_tool_index.go)
    fn new(root: &Value) -> Self {
        let mut idx = ResponsesToolIndex::default();
        for d in walk_responses_tool_declarations(root) {
            idx.declarations.push(d.clone());
            idx.by_identity
                .entry((d.namespace.clone(), d.local_name.clone()))
                .or_insert_with(|| d.chat_name.clone());
            let raw = raw_responses_namespace_qualified_name(&d.namespace, &d.local_name);
            idx.by_raw.entry(raw).or_insert_with(|| d.chat_name.clone());
            if idx.by_chat.contains_key(&d.chat_name) {
                continue;
            }
            idx.by_chat.insert(d.chat_name.clone(), d.clone());
            if idx.by_local.contains_key(&d.local_name) {
                idx.by_local.insert(d.local_name.clone(), String::new());
            } else {
                idx.by_local
                    .insert(d.local_name.clone(), d.chat_name.clone());
            }
            if d.custom {
                idx.custom.insert(d.chat_name.clone());
            }
        }
        idx
    }

    // port of (*responsesToolIndex).namespaceName (responses_tool_index.go)
    fn namespace_name(&self, namespace: &str, name: &str) -> String {
        if let Some(chat_name) = self
            .by_identity
            .get(&(namespace.to_string(), name.to_string()))
        {
            return chat_name.clone();
        }
        self.avoid_alias(qualify_responses_namespace_tool_name(namespace, name))
    }

    // port of (*responsesToolIndex).canonicalName (responses_tool_index.go)
    fn canonical_name(&self, name: &str) -> String {
        if self.by_chat.contains_key(name) {
            return name.to_string();
        }
        if let Some(chat_name) = self.by_raw.get(name) {
            return chat_name.clone();
        }
        if let Some(chat_name) = self.by_local.get(name) {
            if !chat_name.is_empty() {
                return chat_name.clone();
            }
        }
        self.avoid_alias(cap_responses_chat_tool_name(name))
    }

    // port of (*responsesToolIndex).avoidAlias (responses_tool_index.go)
    fn avoid_alias(&self, candidate: String) -> String {
        if !self.by_chat.contains_key(&candidate) {
            return candidate;
        }
        let mut suffix = 1;
        loop {
            let variant = cap_responses_chat_tool_name(&format!("{candidate}_{suffix}"));
            if !self.by_chat.contains_key(&variant) {
                return variant;
            }
            suffix += 1;
        }
    }

    // port of (*responsesToolIndex).applyIdentity (responses_tool_index.go)
    fn apply_identity(&self, item: &mut Value, qualified_name: &str) {
        let mut name = qualified_name.trim().to_string();
        let mut namespace = String::new();
        if let Some(d) = self.by_chat.get(&name) {
            name = d.local_name.clone();
            namespace = d.namespace.clone();
        }
        set_responses_tool_call_identity(item, &name, &namespace);
    }

    // port of (*responsesToolIndex).singleCustomName (responses_tool_index.go)
    fn single_custom_name(&self) -> Option<String> {
        if self.custom.len() == 1 && self.by_chat.len() == 1 {
            return self.custom.iter().next().cloned();
        }
        None
    }

    // port of (*responsesToolIndex).chatTools (responses_tool_index.go)
    fn chat_tools(&self) -> Vec<Value> {
        let mut merged = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for d in &self.declarations {
            if seen.contains(d.chat_name.as_str()) {
                continue;
            }
            let converted = if d.custom {
                convert_responses_custom_tool_to_openai_chat(&d.tool, &d.chat_name)
            } else {
                convert_responses_function_tool_to_openai_chat(&d.tool, &d.chat_name)
            };
            if let Some(tool) = converted {
                merged.push(tool);
                seen.insert(d.chat_name.as_str());
            }
        }
        merged
    }
}

// port of pickRequestJSON (openai_openai-responses_tools.go)
fn pick_request_json(original: Option<&Value>, request: Option<&Value>) -> Option<Value> {
    original.or(request).cloned()
}

// ---------------------------------------------------------------------------
// openai_openai-responses_request.go
// ---------------------------------------------------------------------------

const REASONING_UNAVAILABLE: &str = "[reasoning unavailable]";

/// Client request (Responses body) → upstream request (Chat Completions body).
/// `model` = upstream model id for the body. `stream` = whether the client asked to stream.
// port of ConvertOpenAIResponsesRequestToOpenAIChatCompletions (openai_openai-responses_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    let root = body;
    let mut out = json!({"model":"","messages":[],"stream":false});
    let tool_index = ResponsesToolIndex::new(root);

    out["model"] = json!(model);
    out["stream"] = json!(stream);

    if let Some(text_format) = gpath(root, "text.format") {
        if let Some(response_format) =
            convert_responses_text_format_to_chat_response_format(text_format)
        {
            set(&mut out, "response_format", response_format);
        }
    }

    if let Some(max_tokens) = gpath(root, "max_output_tokens") {
        set(&mut out, "max_tokens", max_tokens.clone());
    }

    let mut b = RequestBuilder::default();

    if let Some(instructions) = gpath(root, "instructions") {
        b.messages
            .push(json!({"role":"system","content": gstr(Some(instructions))}));
    }

    let mut duplicate_output_ids: Vec<String> = Vec::new();

    let input = gpath(root, "input");
    if let Some(Value::Array(raw_input_array)) = input {
        let mut explicit_output_counts: HashMap<String, i64> = HashMap::new();
        let mut missing_id_outputs_count = 0;
        for item in raw_input_array {
            if is_tool_output_type(&gs(item, "type")) {
                let id = extract_responses_call_id(item);
                if !id.is_empty() {
                    *explicit_output_counts.entry(id).or_insert(0) += 1;
                } else {
                    missing_id_outputs_count += 1;
                }
            }
        }

        let mut unclaimed_calls: HashSet<String> = HashSet::new();
        for item in raw_input_array {
            let t = gs(item, "type");
            if t == "function_call" || t == "custom_tool_call" {
                let id = extract_responses_call_id(item);
                if !id.is_empty() && explicit_output_counts.get(&id).copied().unwrap_or(0) == 0 {
                    unclaimed_calls.insert(id);
                }
            }
        }

        let mut input_items = normalize_responses_tool_call_outputs(raw_input_array);
        if missing_id_outputs_count > 1
            || (missing_id_outputs_count > 0 && unclaimed_calls.len() > 1)
        {
            for (idx, item) in input_items.iter_mut().enumerate() {
                if is_tool_output_type(&gs(item, "type"))
                    && idx < raw_input_array.len()
                    && extract_responses_call_id(&raw_input_array[idx]).is_empty()
                {
                    remove(item, "call_id");
                    remove(item, "tool_call_id");
                    remove(item, "callId");
                }
            }
        }

        let effort_meaningful = |effort: &str| {
            let e = effort.trim().to_lowercase();
            !e.is_empty() && e != "none" && e != "0" && e != "false"
        };
        if let Some(effort) = gpath(root, "reasoning.effort") {
            b.has_reasoning_in_session = effort_meaningful(&gstr(Some(effort)));
        } else if let Some(effort) = gpath(root, "reasoning_effort") {
            b.has_reasoning_in_session = effort_meaningful(&gstr(Some(effort)));
        } else if let Some(reasoning) = gpath(root, "reasoning") {
            let r = gstr(Some(reasoning)).trim().to_lowercase();
            b.has_reasoning_in_session = !r.is_empty() && r != "none" && r != "false" && r != "{}";
        }
        if !b.has_reasoning_in_session {
            b.has_reasoning_in_session = raw_input_array.iter().any(|item| {
                gs(item, "type") == "reasoning" || gpath(item, "reasoning_content").is_some()
            });
        }

        for item in &input_items {
            let mut item_type = gs(item, "type");
            if item_type.is_empty() && !gs(item, "role").is_empty() {
                item_type = "message".to_string();
            }
            if item_type != "function_call" && item_type != "custom_tool_call" {
                b.flush_pending_tool_calls();
            }

            match item_type.as_str() {
                "message" | "" => {
                    let mut role = gs(item, "role");
                    if role == "developer" {
                        role = "user".to_string();
                    }
                    b.mergeable_assistant_index = None;
                    if role != "assistant" {
                        b.append_pending_reasoning_message();
                        b.latest_reasoning_content.clear();
                    }
                    let mut message = json!({"role": role, "content": []});

                    match gpath(item, "content") {
                        Some(Value::Array(content)) => {
                            let mut content_items = Vec::new();
                            for content_item in content {
                                let mut content_type = gs(content_item, "type");
                                if content_type.is_empty() {
                                    content_type = "input_text".to_string();
                                }
                                match content_type.as_str() {
                                    "input_text" | "output_text" => {
                                        content_items.push(
                                            json!({"type":"text","text": gs(content_item, "text")}),
                                        );
                                    }
                                    "input_image" => {
                                        let mut part = json!({"type":"image_url","image_url":{"url": gs(content_item, "image_url")}});
                                        if let Some(detail) = normalize_chat_image_detail(gpath(
                                            content_item,
                                            "detail",
                                        )) {
                                            if !detail.is_empty() {
                                                part["image_url"]["detail"] = json!(detail);
                                            }
                                        }
                                        content_items.push(part);
                                    }
                                    // "input_video" / "video_url" parts are out of scope for this port.
                                    _ => {}
                                }
                            }
                            if !content_items.is_empty() {
                                message["content"] = Value::Array(content_items);
                            }
                        }
                        Some(Value::String(s)) => {
                            message["content"] = json!(s);
                        }
                        _ => {}
                    }

                    if role == "assistant" {
                        let pending = b.take_pending_reasoning_content();
                        let reasoning_content = combine_openai_responses_reasoning(
                            &pending,
                            &gs(item, "reasoning_content"),
                        );
                        if !reasoning_content.is_empty() {
                            set(&mut message, "reasoning_content", json!(reasoning_content));
                            if is_usable_responses_reasoning(&reasoning_content) {
                                b.latest_reasoning_content = reasoning_content;
                            }
                        }
                    }

                    let message_index = b.append_regular_message(message);
                    if role == "assistant" {
                        b.mergeable_assistant_index = Some(message_index);
                    }
                }

                "reasoning" => {
                    let reasoning_content = collect_openai_responses_reasoning_content(item);
                    b.pending_reasoning_content = combine_openai_responses_reasoning(
                        &b.pending_reasoning_content,
                        &reasoning_content,
                    );
                    if is_usable_responses_reasoning(&reasoning_content) {
                        b.latest_reasoning_content = reasoning_content;
                    }
                }

                "function_call" => {
                    b.absorb_call_reasoning(&gs(item, "reasoning_content"));
                    let mut tool_call =
                        json!({"id":"","type":"function","function":{"name":"","arguments":""}});
                    let call_id = extract_responses_call_id(item);
                    if !call_id.is_empty() {
                        tool_call["id"] = json!(call_id);
                    }
                    if let Some(name) = gpath(item, "name") {
                        let mut function_name = gstr(Some(name));
                        let namespace = gs(item, "namespace");
                        let namespace = namespace.trim();
                        if !namespace.is_empty() {
                            function_name = tool_index.namespace_name(namespace, &function_name);
                        } else {
                            function_name = tool_index.canonical_name(&function_name);
                        }
                        tool_call["function"]["name"] = json!(function_name);
                    }
                    if let Some(arguments) = gpath(item, "arguments") {
                        tool_call["function"]["arguments"] = json!(gstr(Some(arguments)));
                    }
                    b.pending_tool_calls.push(tool_call);
                    if !call_id.is_empty() {
                        b.pending_tool_call_ids.push(call_id);
                    }
                }

                "function_call_output" | "custom_tool_call_output" => {
                    let custom = item_type == "custom_tool_call_output";
                    let set_content: fn(&mut Value, &Value) = if custom {
                        set_custom_tool_call_output_content
                    } else {
                        set_function_call_output_content
                    };
                    b.mergeable_assistant_index = None;
                    let call_id = extract_responses_call_id(item);
                    if !call_id.is_empty() {
                        let count = b.output_counts.entry(call_id.clone()).or_insert(0);
                        *count += 1;
                        if *count > 1 && !duplicate_output_ids.contains(&call_id) {
                            duplicate_output_ids.push(call_id.clone());
                        }
                    }
                    if !b.awaiting_tool_outputs.contains(&call_id) {
                        // Orphan outputs (empty call_id or no matching assistant
                        // tool_calls) must not become tool messages. Emit as user text instead.
                        append_standalone_responses_tool_output_as_user(
                            gpath(item, "output"),
                            set_content,
                            &mut b,
                        );
                    } else {
                        let mut tool_message =
                            json!({"role":"tool","tool_call_id": call_id,"content":""});
                        b.awaiting_tool_outputs.remove(&call_id);
                        if let Some(output) = gpath(item, "output") {
                            set_content(&mut tool_message, output);
                        }
                        b.messages.push(tool_message);
                    }
                }

                "custom_tool_call" => {
                    b.absorb_call_reasoning(&gs(item, "reasoning_content"));
                    // Codex freeform tool call replay: wrap the raw input so it
                    // matches the {"input": string} function shape.
                    let call_id = extract_responses_call_id(item);
                    let mut function_name = gs(item, "name");
                    let namespace = gs(item, "namespace");
                    if !namespace.is_empty() {
                        function_name = tool_index.namespace_name(&namespace, &function_name);
                    } else {
                        function_name = tool_index.canonical_name(&function_name);
                    }
                    let wrapped_args = json!({"input": gs(item, "input")}).to_string();
                    let tool_call = json!({"id": call_id, "type":"function","function":{"name": function_name,"arguments": wrapped_args}});
                    b.pending_tool_calls.push(tool_call);
                    if !call_id.is_empty() {
                        b.pending_tool_call_ids.push(call_id);
                    }
                }

                _ => {
                    b.mergeable_assistant_index = None;
                }
            }
        }
        b.flush_pending_tool_calls();
        b.append_pending_reasoning_message();
    } else if let Some(Value::String(s)) = input {
        b.messages.push(json!({"role":"user","content": s}));
    }

    if !b.messages.is_empty() {
        let messages =
            align_openai_tool_call_messages(std::mem::take(&mut b.messages), &duplicate_output_ids);
        out["messages"] = Value::Array(messages);
    }

    // Codex Desktop (Responses Lite) delivers tool definitions through an
    // "additional_tools" input item as well as the top-level "tools" field.
    let chat_tools = tool_index.chat_tools();
    if !chat_tools.is_empty() {
        set(&mut out, "tools", Value::Array(chat_tools));
        if let Some(parallel_tool_calls) = gpath(root, "parallel_tool_calls") {
            set(
                &mut out,
                "parallel_tool_calls",
                json!(gbool(Some(parallel_tool_calls))),
            );
        }
        if let Some(tool_choice) = gpath(root, "tool_choice") {
            set(
                &mut out,
                "tool_choice",
                convert_responses_tool_choice_with_index(tool_choice, &tool_index),
            );
        }
    }

    if let Some(reasoning_effort) = gpath(root, "reasoning.effort") {
        let effort = gstr(Some(reasoning_effort)).trim().to_lowercase();
        if !effort.is_empty() {
            set(&mut out, "reasoning_effort", json!(effort));
        }
    }

    out
}

/// The closures of ConvertOpenAIResponsesRequestToOpenAIChatCompletions,
/// lifted into a struct so they can share the mutable message state.
#[derive(Default)]
struct RequestBuilder {
    messages: Vec<Value>,
    has_reasoning_in_session: bool,
    pending_tool_calls: Vec<Value>,
    pending_tool_call_ids: Vec<String>,
    pending_reasoning_content: String,
    latest_reasoning_content: String,
    awaiting_tool_outputs: HashSet<String>,
    output_counts: HashMap<String, i64>,
    mergeable_assistant_index: Option<usize>,
}

impl RequestBuilder {
    // port of the fallbackToolReasoning closure (openai_openai-responses_request.go)
    fn fallback_tool_reasoning(&self) -> String {
        if !self.latest_reasoning_content.is_empty() {
            return self.latest_reasoning_content.clone();
        }
        if self.has_reasoning_in_session {
            return REASONING_UNAVAILABLE.to_string();
        }
        String::new()
    }

    // port of the takePendingReasoningContent closure (openai_openai-responses_request.go)
    fn take_pending_reasoning_content(&mut self) -> String {
        std::mem::take(&mut self.pending_reasoning_content)
    }

    /// The reasoning bookkeeping shared by the function_call and custom_tool_call arms.
    fn absorb_call_reasoning(&mut self, rc: &str) {
        self.pending_reasoning_content =
            combine_openai_responses_reasoning(&self.pending_reasoning_content, rc);
        if is_usable_responses_reasoning(rc) {
            self.latest_reasoning_content = rc.to_string();
        }
    }

    // port of the flushPendingToolCalls closure (openai_openai-responses_request.go)
    fn flush_pending_tool_calls(&mut self) {
        if self.pending_tool_calls.is_empty() {
            return;
        }
        let reasoning_content = self.take_pending_reasoning_content();
        let mut merged_into_assistant = false;
        if let Some(idx) = self.mergeable_assistant_index {
            if idx + 1 == self.messages.len() {
                let assistant_message = &self.messages[idx];
                if gs(assistant_message, "role") == "assistant"
                    && gpath(assistant_message, "tool_calls").is_none()
                {
                    let existing_reasoning = gs(assistant_message, "reasoning_content");
                    let mut updated = assistant_message.clone();
                    set(
                        &mut updated,
                        "tool_calls",
                        Value::Array(self.pending_tool_calls.clone()),
                    );
                    let combined =
                        combine_openai_responses_reasoning(&existing_reasoning, &reasoning_content);
                    if !combined.is_empty() {
                        set(&mut updated, "reasoning_content", json!(combined));
                        if is_usable_responses_reasoning(&combined) {
                            self.latest_reasoning_content = combined;
                        }
                    } else {
                        let fallback = self.fallback_tool_reasoning();
                        if !fallback.is_empty() {
                            set(&mut updated, "reasoning_content", json!(fallback));
                        }
                    }
                    self.messages[idx] = updated;
                    merged_into_assistant = true;
                }
            }
        }
        if !merged_into_assistant {
            let mut assistant_message = json!({"role":"assistant","tool_calls": Value::Array(self.pending_tool_calls.clone())});
            if !reasoning_content.is_empty() {
                set(
                    &mut assistant_message,
                    "reasoning_content",
                    json!(reasoning_content),
                );
                if is_usable_responses_reasoning(&reasoning_content) {
                    self.latest_reasoning_content = reasoning_content;
                }
            } else {
                let fallback = self.fallback_tool_reasoning();
                if !fallback.is_empty() {
                    set(&mut assistant_message, "reasoning_content", json!(fallback));
                }
            }
            self.messages.push(assistant_message);
        }
        for id in &self.pending_tool_call_ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                self.awaiting_tool_outputs.insert(trimmed.to_string());
            }
        }
        self.pending_tool_calls.clear();
        self.pending_tool_call_ids.clear();
        self.mergeable_assistant_index = None;
    }

    // port of the appendRegularMessage closure (openai_openai-responses_request.go)
    fn append_regular_message(&mut self, message: Value) -> usize {
        self.messages.push(message);
        self.messages.len() - 1
    }

    // port of the appendPendingReasoningMessage closure (openai_openai-responses_request.go)
    fn append_pending_reasoning_message(&mut self) {
        let reasoning_content = self.take_pending_reasoning_content();
        if reasoning_content.is_empty() {
            return;
        }
        if is_usable_responses_reasoning(&reasoning_content) {
            self.latest_reasoning_content = reasoning_content.clone();
        }
        self.append_regular_message(
            json!({"role":"assistant","content":"","reasoning_content": reasoning_content}),
        );
    }
}

// port of convertResponsesToolChoiceWithIndex (openai_openai-responses_request.go)
fn convert_responses_tool_choice_with_index(
    tool_choice: &Value,
    tool_index: &ResponsesToolIndex,
) -> Value {
    if !tool_choice.is_object() {
        return tool_choice.clone();
    }
    let choice_type = gs(tool_choice, "type");
    if choice_type != "function" && choice_type != "custom" {
        return tool_choice.clone();
    }
    let mut name = gs(tool_choice, "function.name");
    if name.is_empty() {
        name = gs(tool_choice, "custom.name");
    }
    if name.is_empty() {
        name = gs(tool_choice, "name");
    }
    if name.is_empty() {
        return tool_choice.clone();
    }
    let mut namespace = gs(tool_choice, "namespace").trim().to_string();
    if namespace.is_empty() {
        namespace = gs(tool_choice, "function.namespace").trim().to_string();
    }
    if namespace.is_empty() {
        namespace = gs(tool_choice, "custom.namespace").trim().to_string();
    }
    let name = if !namespace.is_empty() {
        tool_index.namespace_name(&namespace, &name)
    } else {
        tool_index.canonical_name(&name)
    };
    json!({"type":"function","function":{"name": name}})
}

// port of convertResponsesTextFormatToChatResponseFormat (openai_openai-responses_request.go)
fn convert_responses_text_format_to_chat_response_format(text_format: &Value) -> Option<Value> {
    let format_type = gs(text_format, "type");
    match format_type.as_str() {
        "text" | "json_object" => Some(json!({"type": format_type})),
        "json_schema" => {
            let mut schema_obj = Map::new();
            for field in ["name", "description", "strict"] {
                if let Some(value) = gpath(text_format, field) {
                    schema_obj.insert(field.to_string(), value.clone());
                }
            }
            if let Some(schema) = gpath(text_format, "schema") {
                schema_obj.insert("schema".to_string(), schema.clone());
            }
            Some(json!({"type":"json_schema","json_schema": Value::Object(schema_obj)}))
        }
        _ => None,
    }
}

// port of appendStandaloneResponsesToolOutputAsUser (openai_openai-responses_request.go)
fn append_standalone_responses_tool_output_as_user(
    output: Option<&Value>,
    set_content: fn(&mut Value, &Value),
    b: &mut RequestBuilder,
) {
    let mut user_message = json!({"role":"user","content":""});
    if let Some(output) = output {
        set_content(&mut user_message, output);
    }
    match gpath(&user_message, "content") {
        None => return,
        Some(Value::String(s)) if s.trim().is_empty() => return,
        Some(Value::Array(a)) if a.is_empty() => return,
        _ => {}
    }
    b.append_regular_message(user_message);
}

// port of setFunctionCallOutputContent (openai_openai-responses_request.go)
fn set_function_call_output_content(tool_message: &mut Value, output: &Value) {
    let parsed;
    let structured_content = if let Value::String(s) = output {
        match serde_json::from_str::<Value>(s) {
            Ok(v) => {
                parsed = v;
                &parsed
            }
            Err(_) => {
                set(tool_message, "content", json!(s));
                return;
            }
        }
    } else {
        output
    };

    if has_chat_tool_output_image_part(structured_content) {
        let content_items: Vec<Value> = as_array(Some(structured_content))
            .iter()
            .map(chat_tool_output_content_part)
            .collect();
        if !content_items.is_empty() {
            set(tool_message, "content", Value::Array(content_items));
        }
        return;
    }

    set(tool_message, "content", json!(gstr(Some(output))));
}

// port of setCustomToolCallOutputContent (openai_openai-responses_request.go)
fn set_custom_tool_call_output_content(tool_message: &mut Value, output: &Value) {
    let parsed;
    let structured_content = match output {
        Value::String(s) if is_valid_json(s) => {
            parsed = serde_json::from_str::<Value>(s).unwrap_or(Value::Null);
            &parsed
        }
        _ => output,
    };
    if has_chat_tool_output_image_part(structured_content) {
        set_function_call_output_content(tool_message, output);
        return;
    }
    set(
        tool_message,
        "content",
        json!(responses_tool_output_text(Some(output))),
    );
}

// port of chatToolOutputContentPart (openai_openai-responses_request.go)
fn chat_tool_output_content_part(item: &Value) -> Value {
    match gs(item, "type").as_str() {
        "text" | "input_text" | "output_text" => json!({"type":"text","text": gs(item, "text")}),
        "image_url" | "input_image" => match chat_tool_output_image_fields(item) {
            Some((image_url, detail)) => {
                let mut part = json!({"type":"image_url","image_url":{"url": image_url}});
                if !detail.is_empty() {
                    part["image_url"]["detail"] = json!(detail);
                }
                part
            }
            None => chat_tool_output_fallback_part(item),
        },
        _ => chat_tool_output_fallback_part(item),
    }
}

// port of hasChatToolOutputImagePart (openai_openai-responses_request.go)
fn has_chat_tool_output_image_part(content: &Value) -> bool {
    let Value::Array(items) = content else {
        return false;
    };
    let mut has_image = false;
    for item in items {
        let Some(Value::String(item_type)) = gpath(item, "type") else {
            continue;
        };
        match item_type.as_str() {
            "text" | "input_text" | "output_text"
                if !matches!(gpath(item, "text"), Some(Value::String(_))) =>
            {
                return false;
            }
            "image_url" | "input_image" => {
                if chat_tool_output_image_fields(item).is_none() {
                    return false;
                }
                has_image = true;
            }
            _ => {}
        }
    }
    has_image
}

// port of chatToolOutputImageFields (openai_openai-responses_request.go)
fn chat_tool_output_image_fields(item: &Value) -> Option<(String, String)> {
    let (image_url_value, detail_value) = match gs(item, "type").as_str() {
        "image_url" => (
            gpath(item, "image_url.url"),
            gpath(item, "image_url.detail"),
        ),
        "input_image" => (gpath(item, "image_url"), gpath(item, "detail")),
        _ => return None,
    };
    let Some(Value::String(image_url)) = image_url_value else {
        return None;
    };
    let image_url = image_url.trim();
    if image_url.is_empty() {
        return None;
    }
    let detail = normalize_chat_image_detail(detail_value)?;
    Some((image_url.to_string(), detail))
}

// port of normalizeChatImageDetail (openai_openai-responses_request.go)
//
// `None` is Go's `ok == false`; `Some("")` drops the detail.
fn normalize_chat_image_detail(detail_value: Option<&Value>) -> Option<String> {
    let Some(detail_value) = detail_value else {
        return Some(String::new());
    };
    let Value::String(detail) = detail_value else {
        return None;
    };
    let normalized = detail.trim().to_lowercase();
    Some(match normalized.as_str() {
        "auto" | "low" | "high" => normalized,
        // Chat Completions does not support Codex's original detail value.
        "original" => "high".to_string(),
        _ => String::new(),
    })
}

// port of chatToolOutputFallbackPart (openai_openai-responses_request.go)
fn chat_tool_output_fallback_part(item: &Value) -> Value {
    let text = match item {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    json!({"type":"text","text": text})
}

// port of collectOpenAIResponsesReasoningContent (openai_openai-responses_request.go)
fn collect_openai_responses_reasoning_content(item: &Value) -> String {
    let mut reasoning_text = String::new();
    for summary_item in as_array(gpath(item, "summary")) {
        if gs(summary_item, "type") != "summary_text" {
            continue;
        }
        reasoning_text.push_str(&gs(summary_item, "text"));
    }
    if reasoning_text.is_empty() {
        return REASONING_UNAVAILABLE.to_string();
    }
    reasoning_text
}

// port of combineOpenAIResponsesReasoning (openai_openai-responses_request.go)
fn combine_openai_responses_reasoning(existing: &str, incoming: &str) -> String {
    let existing_trimmed = existing.trim();
    let incoming_trimmed = incoming.trim();
    if existing_trimmed.is_empty() {
        incoming.to_string()
    } else if incoming_trimmed.is_empty() {
        existing.to_string()
    } else if existing_trimmed == REASONING_UNAVAILABLE {
        incoming.to_string()
    } else if incoming_trimmed == REASONING_UNAVAILABLE || existing_trimmed == incoming_trimmed {
        existing.to_string()
    } else {
        format!("{existing}\n\n{incoming}")
    }
}

// port of isUsableResponsesReasoning (openai_openai-responses_request.go)
fn is_usable_responses_reasoning(reasoning: &str) -> bool {
    let trimmed = reasoning.trim();
    !trimmed.is_empty() && trimmed != REASONING_UNAVAILABLE
}

// ---------------------------------------------------------------------------
// openai_openai-responses_response.go
// ---------------------------------------------------------------------------

// port of responseIDCounter (openai_openai-responses_response.go)
static RESPONSE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `resp_<unix-nanos hex>_<counter>`, the Go synthesized id shape.
fn synthesize_response_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = RESPONSE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("resp_{nanos:x}_{n}")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// port of incompleteByFinishReason (openai_openai-responses_response.go)
fn incomplete_by_finish_reason(reason: &str) -> Option<Value> {
    match reason {
        "length" | "max_tokens" => Some(json!({"reason":"max_output_tokens"})),
        "content_filter" => Some(json!({"reason":"content_filter"})),
        _ => None,
    }
}

/// The request fields echoed into a Responses `response` object, shared by
/// buildResponsesCompletedEvent and the non-stream converter (which also
/// accepts `max_tokens`). Keys land in Go's order.
fn echo_request_fields(target: &mut Value, req: &Value, max_tokens_fallback: bool) {
    let mut put = |key: &str, val: Value| set(target, key, val);
    if let Some(v) = gpath(req, "instructions") {
        put("instructions", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "max_output_tokens") {
        put("max_output_tokens", json!(gint(Some(v))));
    } else if max_tokens_fallback {
        if let Some(v) = gpath(req, "max_tokens") {
            put("max_output_tokens", json!(gint(Some(v))));
        }
    }
    if let Some(v) = gpath(req, "max_tool_calls") {
        put("max_tool_calls", json!(gint(Some(v))));
    }
    if let Some(v) = gpath(req, "model") {
        put("model", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "parallel_tool_calls") {
        put("parallel_tool_calls", json!(gbool(Some(v))));
    }
    if let Some(v) = gpath(req, "previous_response_id") {
        put("previous_response_id", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "prompt_cache_key") {
        put("prompt_cache_key", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "reasoning") {
        put("reasoning", v.clone());
    }
    if let Some(v) = gpath(req, "safety_identifier") {
        put("safety_identifier", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "service_tier") {
        put("service_tier", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "store") {
        put("store", json!(gbool(Some(v))));
    }
    if let Some(v) = gpath(req, "temperature") {
        put("temperature", float_value(gfloat(Some(v))));
    }
    if let Some(v) = gpath(req, "text") {
        put("text", v.clone());
    }
    if let Some(v) = gpath(req, "tool_choice") {
        put("tool_choice", v.clone());
    }
    if let Some(v) = gpath(req, "tools") {
        put("tools", v.clone());
    }
    if let Some(v) = gpath(req, "top_logprobs") {
        put("top_logprobs", json!(gint(Some(v))));
    }
    if let Some(v) = gpath(req, "top_p") {
        put("top_p", float_value(gfloat(Some(v))));
    }
    if let Some(v) = gpath(req, "truncation") {
        put("truncation", json!(gstr(Some(v))));
    }
    if let Some(v) = gpath(req, "user") {
        put("user", v.clone());
    }
    if let Some(v) = gpath(req, "metadata") {
        put("metadata", v.clone());
    }
}

fn message_item(id: &str, status: &str, text: &str) -> Value {
    json!({"id": id,"type":"message","status": status,"content":[{"type":"output_text","annotations":[],"logprobs":[],"text": text}],"role":"assistant"})
}

fn custom_tool_call_item(call_id: &str, status: &str, input: &str) -> Value {
    json!({"id": format!("ctc_{call_id}"),"type":"custom_tool_call","status": status,"input": input,"call_id": call_id,"name":""})
}

fn function_call_item(call_id: &str, status: &str, arguments: &str) -> Value {
    json!({"id": format!("fc_{call_id}"),"type":"function_call","status": status,"arguments": arguments,"call_id": call_id,"name":""})
}

// port of oaiToResponsesStateReasoning (openai_openai-responses_response.go)
#[derive(Clone, Debug)]
struct StateReasoning {
    reasoning_id: String,
    reasoning_data: String,
    output_index: i64,
}

/// Per-stream state of the Chat Completions → Responses SSE translation.
// port of oaiToResponsesState (openai_openai-responses_response.go)
pub struct StreamTranslator {
    original_request: Option<Value>,
    translated_request: Option<Value>,
    model_name: String,

    request_json: Option<Value>,
    tool_index: ResponsesToolIndex,
    request_initialized: bool,
    seq: i64,
    response_id: String,
    created: i64,
    started: bool,
    completed_emitted: bool,
    reasoning_id: String,
    reasoning_index: i64,
    msg_text_buf: HashMap<i64, String>,
    reasoning_buf: String,
    reasonings: Vec<StateReasoning>,
    func_args_buf: HashMap<String, String>,
    func_names: HashMap<String, String>,
    func_call_ids: HashMap<String, String>,
    func_output_ix: HashMap<String, i64>,
    func_args_sent: HashMap<String, usize>,
    msg_output_ix: HashMap<i64, i64>,
    next_output_ix: i64,
    msg_item_added: HashMap<i64, bool>,
    msg_content_added: HashMap<i64, bool>,
    msg_item_done: HashMap<i64, bool>,
    func_item_added: HashMap<String, bool>,
    func_item_custom: HashMap<String, bool>,
    func_args_done: HashMap<String, bool>,
    func_item_done: HashMap<String, bool>,
    custom_tool_names: HashSet<String>,
    finish_reason: String,
    prompt_tokens: i64,
    cached_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    reasoning_tokens: i64,
    usage_seen: bool,
}

fn flag<K: std::hash::Hash + Eq>(m: &HashMap<K, bool>, k: &K) -> bool {
    m.get(k).copied().unwrap_or(false)
}

impl StreamTranslator {
    pub fn new(original_request: &Value) -> Self {
        Self::with_requests(Some(original_request.clone()), None, "")
    }

    /// Go's full parameter set: `original` / `translated` are the
    /// `originalRequestRawJSON` / `requestRawJSON` arguments (None = nil or
    /// invalid), `model_name` the executor's model fallback for
    /// `response.created`.
    fn with_requests(original: Option<Value>, translated: Option<Value>, model_name: &str) -> Self {
        StreamTranslator {
            original_request: original,
            translated_request: translated,
            model_name: model_name.to_string(),
            request_json: None,
            tool_index: ResponsesToolIndex::default(),
            request_initialized: false,
            seq: 0,
            response_id: String::new(),
            created: 0,
            started: false,
            completed_emitted: false,
            reasoning_id: String::new(),
            reasoning_index: 0,
            msg_text_buf: HashMap::new(),
            reasoning_buf: String::new(),
            reasonings: Vec::new(),
            func_args_buf: HashMap::new(),
            func_names: HashMap::new(),
            func_call_ids: HashMap::new(),
            func_output_ix: HashMap::new(),
            func_args_sent: HashMap::new(),
            msg_output_ix: HashMap::new(),
            next_output_ix: 0,
            msg_item_added: HashMap::new(),
            msg_content_added: HashMap::new(),
            msg_item_done: HashMap::new(),
            func_item_added: HashMap::new(),
            func_item_custom: HashMap::new(),
            func_args_done: HashMap::new(),
            func_item_done: HashMap::new(),
            custom_tool_names: HashSet::new(),
            finish_reason: String::new(),
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            reasoning_tokens: 0,
            usage_seen: false,
        }
    }

    /// One upstream Chat SSE chunk → zero or more complete client SSE frames.
    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert(Some(data))
    }

    /// Upstream stream ended (the `[DONE]` marker).
    pub fn finish(&mut self) -> Vec<String> {
        self.convert(None)
    }

    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    /// `response.failed` for an in-stream Chat error object; nothing follows.
    fn fail_with(&mut self, err: &Value) -> Vec<String> {
        let message = match gs(err, "message") {
            m if m.is_empty() => "upstream request failed".to_string(),
            m => m,
        };
        let code = match err.get("code") {
            Some(Value::String(c)) if !c.is_empty() => c.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => "server_error".to_string(),
        };
        self.completed_emitted = true;
        self.started = true;
        let seq = self.next_seq();
        let failed = json!({"type":"response.failed","sequence_number": seq,"response":{
            "id": self.response_id,"object":"response","created_at": self.created,"status":"failed",
            "error": {"code": code, "message": message}}});
        vec![emit_resp_event("response.failed", &failed)]
    }

    fn alloc_output_index(&mut self) -> i64 {
        let ix = self.next_output_ix;
        self.next_output_ix += 1;
        ix
    }

    // port of ConvertOpenAIChatCompletionsResponseToOpenAIResponses (openai_openai-responses_response.go)
    //
    // `chunk == None` is the `[DONE]` marker.
    fn convert(&mut self, chunk: Option<&Value>) -> Vec<String> {
        if !self.request_initialized {
            self.request_json = pick_request_json(
                self.original_request.as_ref(),
                self.translated_request.as_ref(),
            );
            self.tool_index =
                ResponsesToolIndex::new(self.request_json.as_ref().unwrap_or(&Value::Null));
            self.request_initialized = true;
        }
        let is_done = chunk.is_none();
        if is_done && (!self.started || self.completed_emitted) {
            return Vec::new();
        }
        let empty = Value::Null;
        let root = chunk.unwrap_or(&empty);
        if !is_done {
            // An error delivered inside the 200 stream (`{"error":{…}}`, no
            // `choices`): the Responses client gets `response.failed` with
            // the upstream's reason, not a stream that ends without its
            // terminal event (Codex: "stream closed before
            // response.completed", retried, the reason lost).
            if let Some(err) = root.get("error").filter(|e| e.is_object()) {
                if root.get("choices").is_none() {
                    return self.fail_with(err);
                }
            }
            let obj = gs(root, "object");
            if !obj.is_empty() && obj != "chat.completion.chunk" {
                return Vec::new();
            }
            if !is_array(gpath(root, "choices")) {
                return Vec::new();
            }
        }

        if let Some(usage) = gpath(root, "usage") {
            if let Some(v) = gpath(usage, "prompt_tokens") {
                self.prompt_tokens = gint(Some(v));
                self.usage_seen = true;
            }
            if let Some(v) = gpath(usage, "prompt_tokens_details.cached_tokens") {
                self.cached_tokens = gint(Some(v));
                self.usage_seen = true;
            }
            if let Some(v) = gpath(usage, "completion_tokens") {
                self.completion_tokens = gint(Some(v));
                self.usage_seen = true;
            } else if let Some(v) = gpath(usage, "output_tokens") {
                self.completion_tokens = gint(Some(v));
                self.usage_seen = true;
            }
            if let Some(v) = gpath(usage, "output_tokens_details.reasoning_tokens") {
                self.reasoning_tokens = gint(Some(v));
                self.usage_seen = true;
            } else if let Some(v) = gpath(usage, "completion_tokens_details.reasoning_tokens") {
                self.reasoning_tokens = gint(Some(v));
                self.usage_seen = true;
            }
            if let Some(v) = gpath(usage, "total_tokens") {
                self.total_tokens = gint(Some(v));
                self.usage_seen = true;
            }
        }

        let mut out: Vec<String> = Vec::new();

        if !self.started {
            self.response_id = gs(root, "id");
            self.created = gint(gpath(root, "created"));
            // reset aggregation state for a new streaming response
            self.msg_text_buf.clear();
            self.reasoning_buf.clear();
            self.reasoning_id.clear();
            self.reasoning_index = 0;
            self.func_args_buf.clear();
            self.func_names.clear();
            self.func_call_ids.clear();
            self.func_output_ix.clear();
            self.func_args_sent.clear();
            self.msg_output_ix.clear();
            self.next_output_ix = 0;
            self.msg_item_added.clear();
            self.msg_content_added.clear();
            self.msg_item_done.clear();
            self.func_item_added.clear();
            self.func_item_custom.clear();
            self.func_args_done.clear();
            self.func_item_done.clear();
            self.custom_tool_names = self.tool_index.custom.clone();
            self.prompt_tokens = 0;
            self.cached_tokens = 0;
            self.completion_tokens = 0;
            self.total_tokens = 0;
            self.reasoning_tokens = 0;
            self.finish_reason.clear();
            self.usage_seen = false;
            self.completed_emitted = false;

            let mut request_model_name = request_model_name(
                self.original_request.as_ref(),
                self.translated_request.as_ref(),
            );
            if request_model_name.is_empty() {
                request_model_name = self.model_name.clone();
            }

            let seq = self.next_seq();
            let mut created = json!({"type":"response.created","sequence_number": seq,"response":{"id": self.response_id,"object":"response","created_at": self.created,"status":"in_progress","background":false,"error":null,"output":[]}});
            if !request_model_name.is_empty() {
                created["response"]["model"] = json!(request_model_name);
            }
            out.push(emit_resp_event("response.created", &created));

            let seq = self.next_seq();
            let mut inprog = json!({"type":"response.in_progress","sequence_number": seq,"response":{"id": self.response_id,"object":"response","created_at": self.created,"status":"in_progress","output":[]}});
            if !request_model_name.is_empty() {
                inprog["response"]["model"] = json!(request_model_name);
            }
            out.push(emit_resp_event("response.in_progress", &inprog));
            self.started = true;
        }

        if is_done {
            self.finalize_open_items(&mut out);
            let has_active_unfinished_tool = self
                .func_item_added
                .keys()
                .any(|key| !flag(&self.func_item_done, key));
            if has_active_unfinished_tool {
                return out;
            }
            // Go returns here when no message or function item was produced
            // (an empty answer, reasoning only): the client then waits for
            // a `response.completed` that never comes. The terminal event is
            // sent with whatever output there is.
            if false {
                return out;
            }
            self.completed_emitted = true;
            let completed = self.build_responses_completed_event();
            out.push(completed);
            return out;
        }

        // choices[].delta content / tool_calls / reasoning_content
        for choice in as_array(gpath(root, "choices")) {
            let idx = gint(gpath(choice, "index"));
            if let Some(delta) = gpath(choice, "delta") {
                // reasoning_content (OpenAI reasoning incremental text)
                let mut rc = gpath(delta, "reasoning_content");
                if gstr(rc).is_empty() {
                    rc = gpath(delta, "reasoning");
                }
                let rc_text = gstr(rc);
                if !rc_text.is_empty() {
                    // On first appearance, add reasoning item and part
                    if self.reasoning_id.is_empty() {
                        self.reasoning_id = format!("rs_{}_{}", self.response_id, idx);
                        self.reasoning_index = self.alloc_output_index();
                        let seq = self.next_seq();
                        let item = json!({"type":"response.output_item.added","sequence_number": seq,"output_index": self.reasoning_index,"item":{"id": self.reasoning_id,"type":"reasoning","status":"in_progress","summary":[]}});
                        out.push(emit_resp_event("response.output_item.added", &item));
                        let seq = self.next_seq();
                        let part = json!({"type":"response.reasoning_summary_part.added","sequence_number": seq,"item_id": self.reasoning_id,"output_index": self.reasoning_index,"summary_index":0,"part":{"type":"summary_text","text":""}});
                        out.push(emit_resp_event(
                            "response.reasoning_summary_part.added",
                            &part,
                        ));
                    }
                    self.reasoning_buf.push_str(&rc_text);
                    let seq = self.next_seq();
                    let msg = json!({"type":"response.reasoning_summary_text.delta","sequence_number": seq,"item_id": self.reasoning_id,"output_index": self.reasoning_index,"summary_index":0,"delta": rc_text});
                    out.push(emit_resp_event(
                        "response.reasoning_summary_text.delta",
                        &msg,
                    ));
                }

                let content = gs(delta, "content");
                if !content.is_empty() {
                    // Ensure the message item and its first content part are announced before any text deltas
                    if !self.reasoning_id.is_empty() {
                        let text = std::mem::take(&mut self.reasoning_buf);
                        self.stop_reasoning(&text, &mut out);
                    }
                    if !self.msg_output_ix.contains_key(&idx) {
                        let ix = self.alloc_output_index();
                        self.msg_output_ix.insert(idx, ix);
                    }
                    let msg_output_index = self.msg_output_ix[&idx];
                    let item_id = format!("msg_{}_{}", self.response_id, idx);
                    if !flag(&self.msg_item_added, &idx) {
                        let seq = self.next_seq();
                        let item = json!({"type":"response.output_item.added","sequence_number": seq,"output_index": msg_output_index,"item":{"id": item_id,"type":"message","status":"in_progress","content":[],"role":"assistant"}});
                        out.push(emit_resp_event("response.output_item.added", &item));
                        self.msg_item_added.insert(idx, true);
                    }
                    if !flag(&self.msg_content_added, &idx) {
                        let seq = self.next_seq();
                        let part = json!({"type":"response.content_part.added","sequence_number": seq,"item_id": item_id,"output_index": msg_output_index,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}});
                        out.push(emit_resp_event("response.content_part.added", &part));
                        self.msg_content_added.insert(idx, true);
                    }
                    let seq = self.next_seq();
                    let msg = json!({"type":"response.output_text.delta","sequence_number": seq,"item_id": item_id,"output_index": msg_output_index,"content_index":0,"delta": content,"logprobs":[]});
                    out.push(emit_resp_event("response.output_text.delta", &msg));
                    // aggregate for response.output
                    self.msg_text_buf.entry(idx).or_default().push_str(&content);
                }

                // tool calls
                let tcs = as_array(gpath(delta, "tool_calls"));
                if !tcs.is_empty() {
                    if !self.reasoning_id.is_empty() {
                        let text = std::mem::take(&mut self.reasoning_buf);
                        self.stop_reasoning(&text, &mut out);
                    }
                    // Before emitting any function events, if a message is open for this index,
                    // close its text/content to match Codex expected ordering.
                    self.emit_message_item_done(idx, &mut out);

                    for tc in tcs {
                        let tool_index = gint(gpath(tc, "index"));
                        let key = format!("{idx}:{tool_index}");
                        if !self.func_args_buf.contains_key(&key) {
                            self.func_args_buf.insert(key.clone(), String::new());
                            let ix = self.alloc_output_index();
                            self.func_output_ix.insert(key.clone(), ix);
                        }
                        let new_call_id = gs(tc, "id");
                        if !new_call_id.is_empty()
                            && self
                                .func_call_ids
                                .get(&key)
                                .map(String::is_empty)
                                .unwrap_or(true)
                        {
                            self.func_call_ids.insert(key.clone(), new_call_id);
                        }
                        let name_chunk = gs(tc, "function.name");
                        if !name_chunk.is_empty() && !flag(&self.func_item_added, &key) {
                            self.func_names.insert(key.clone(), name_chunk);
                        }
                        let args = gs(tc, "function.arguments");
                        if !args.is_empty() {
                            self.func_args_buf
                                .entry(key.clone())
                                .or_default()
                                .push_str(&args);
                        }
                        self.emit_tool_item(&key, false, &mut out);
                        self.emit_pending_function_args(&key, &mut out);
                    }
                }
            }

            // finish_reason triggers item-level finalization. response.completed is
            // deferred until the terminal [DONE] marker so late usage-only chunks can
            // still populate response.usage.
            let fr = gs(choice, "finish_reason");
            if !fr.is_empty() {
                self.finish_reason = fr;
                self.finalize_open_items(&mut out);
            }
        }

        out
    }

    // port of the emitToolItem closure (openai_openai-responses_response.go)
    fn emit_tool_item(&mut self, key: &str, force: bool, out: &mut Vec<String>) {
        if flag(&self.func_item_added, &key.to_string()) {
            return;
        }
        let mut call_id = self.func_call_ids.get(key).cloned().unwrap_or_default();
        let mut name = self
            .tool_index
            .canonical_name(self.func_names.get(key).map(String::as_str).unwrap_or(""));
        self.func_names.insert(key.to_string(), name.clone());
        if !force && (call_id.is_empty() || name.is_empty()) {
            return;
        }
        if name.is_empty() {
            if let Some(custom_tool_name) = self.tool_index.single_custom_name() {
                name = custom_tool_name.clone();
                self.func_names.insert(key.to_string(), custom_tool_name);
            }
        }
        if call_id.is_empty() {
            call_id = format!("call_{}_{}", self.response_id, key.replace(':', "_"));
            self.func_call_ids.insert(key.to_string(), call_id.clone());
        }

        let output_index = self.func_output_ix.get(key).copied().unwrap_or(0);
        let is_custom_tool = self.custom_tool_names.contains(&name);
        self.func_item_custom
            .insert(key.to_string(), is_custom_tool);
        let seq = self.next_seq();
        let mut item = if is_custom_tool {
            json!({"id": format!("ctc_{call_id}"),"type":"custom_tool_call","status":"in_progress","input":"","call_id": call_id,"name":""})
        } else {
            json!({"id": format!("fc_{call_id}"),"type":"function_call","status":"in_progress","arguments":"","call_id": call_id,"name":""})
        };
        self.tool_index.apply_identity(&mut item, &name);
        let o = json!({"type":"response.output_item.added","sequence_number": seq,"output_index": output_index,"item": item});
        out.push(emit_resp_event("response.output_item.added", &o));
        self.func_item_added.insert(key.to_string(), true);
    }

    // port of the emitPendingFunctionArgs closure (openai_openai-responses_response.go)
    fn emit_pending_function_args(&mut self, key: &str, out: &mut Vec<String>) {
        let k = key.to_string();
        if !flag(&self.func_item_added, &k) || flag(&self.func_item_custom, &k) {
            return;
        }
        let sent = self.func_args_sent.get(key).copied().unwrap_or(0);
        let Some(args) = self.func_args_buf.get(key) else {
            return;
        };
        if args.len() <= sent {
            return;
        }
        let args = args.clone();
        let delta = &args[sent..];
        let call_id = self.func_call_ids.get(key).cloned().unwrap_or_default();
        let output_index = self.func_output_ix.get(key).copied().unwrap_or(0);
        let seq = self.next_seq();
        let ad = json!({"type":"response.function_call_arguments.delta","sequence_number": seq,"item_id": format!("fc_{call_id}"),"output_index": output_index,"delta": delta});
        out.push(emit_resp_event(
            "response.function_call_arguments.delta",
            &ad,
        ));
        self.func_args_sent.insert(k, args.len());
    }

    // port of the stopReasoning closure (openai_openai-responses_response.go)
    fn stop_reasoning(&mut self, text: &str, out: &mut Vec<String>) {
        let seq = self.next_seq();
        let text_done = json!({"type":"response.reasoning_summary_text.done","sequence_number": seq,"item_id": self.reasoning_id,"output_index": self.reasoning_index,"summary_index":0,"text": text});
        out.push(emit_resp_event(
            "response.reasoning_summary_text.done",
            &text_done,
        ));
        let seq = self.next_seq();
        let part_done = json!({"type":"response.reasoning_summary_part.done","sequence_number": seq,"item_id": self.reasoning_id,"output_index": self.reasoning_index,"summary_index":0,"part":{"type":"summary_text","text": text}});
        out.push(emit_resp_event(
            "response.reasoning_summary_part.done",
            &part_done,
        ));
        let seq = self.next_seq();
        let output_item_done = json!({"type":"response.output_item.done","item":{"id": self.reasoning_id,"type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text": text}]},"output_index": self.reasoning_index,"sequence_number": seq});
        out.push(emit_resp_event(
            "response.output_item.done",
            &output_item_done,
        ));

        self.reasonings.push(StateReasoning {
            reasoning_id: self.reasoning_id.clone(),
            reasoning_data: text.to_string(),
            output_index: self.reasoning_index,
        });
        self.reasoning_id.clear();
    }

    // port of the emitMessageItemDone closure (openai_openai-responses_response.go)
    fn emit_message_item_done(&mut self, idx: i64, out: &mut Vec<String>) {
        if !flag(&self.msg_item_added, &idx) || flag(&self.msg_item_done, &idx) {
            return;
        }
        let msg_output_index = self.msg_output_ix.get(&idx).copied().unwrap_or(0);
        let full_text = self.msg_text_buf.get(&idx).cloned().unwrap_or_default();
        let item_id = format!("msg_{}_{}", self.response_id, idx);

        let seq = self.next_seq();
        let done = json!({"type":"response.output_text.done","sequence_number": seq,"item_id": item_id,"output_index": msg_output_index,"content_index":0,"text": full_text,"logprobs":[]});
        out.push(emit_resp_event("response.output_text.done", &done));

        let seq = self.next_seq();
        let part_done = json!({"type":"response.content_part.done","sequence_number": seq,"item_id": item_id,"output_index": msg_output_index,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text": full_text}});
        out.push(emit_resp_event("response.content_part.done", &part_done));

        let msg_status = if incomplete_by_finish_reason(&self.finish_reason).is_some() {
            "incomplete"
        } else {
            "completed"
        };
        let seq = self.next_seq();
        let item_done = json!({"type":"response.output_item.done","sequence_number": seq,"output_index": msg_output_index,"item": message_item(&item_id, msg_status, &full_text)});
        out.push(emit_resp_event("response.output_item.done", &item_done));
        self.msg_item_done.insert(idx, true);
    }

    // port of the finalizeOpenItems closure (openai_openai-responses_response.go)
    fn finalize_open_items(&mut self, out: &mut Vec<String>) {
        if !self.msg_item_added.is_empty() {
            let mut idxs: Vec<i64> = self.msg_item_added.keys().copied().collect();
            idxs.sort_by_key(|i| self.msg_output_ix.get(i).copied().unwrap_or(0));
            for idx in idxs {
                self.emit_message_item_done(idx, out);
            }
        }

        if !self.reasoning_id.is_empty() {
            let text = std::mem::take(&mut self.reasoning_buf);
            self.stop_reasoning(&text, out);
        }

        if self.func_args_buf.is_empty() {
            return;
        }
        let mut keys: Vec<String> = self.func_args_buf.keys().cloned().collect();
        keys.sort_by(|a, b| {
            let left = self.func_output_ix.get(a).copied().unwrap_or(0);
            let right = self.func_output_ix.get(b).copied().unwrap_or(0);
            left.cmp(&right).then_with(|| a.cmp(b))
        });
        for key in keys {
            if flag(&self.func_item_done, &key) {
                continue;
            }
            let args_buf = self.func_args_buf.get(&key).cloned().unwrap_or_default();
            let has_args = !args_buf.is_empty();
            let is_incomplete = incomplete_by_finish_reason(&self.finish_reason).is_some();
            let is_explicit_tool_finish =
                self.finish_reason == "tool_calls" || self.finish_reason == "stop";

            // If stream ended without finish_reason and no or partial/invalid JSON
            // arguments were received, do not synthesize or complete the call.
            if self.finish_reason.is_empty() && (!has_args || !is_valid_json(&args_buf)) {
                continue;
            }

            self.emit_tool_item(&key, true, out);
            self.emit_pending_function_args(&key, out);
            let call_id = self.func_call_ids.get(&key).cloned().unwrap_or_default();
            if call_id.is_empty() || flag(&self.func_item_done, &key) {
                continue;
            }

            let output_index = self.func_output_ix.get(&key).copied().unwrap_or(0);
            let mut args = "{}".to_string();
            if has_args {
                args = args_buf;
            } else if is_incomplete || !is_explicit_tool_finish {
                args = String::new();
            }
            let tool_status = if is_incomplete {
                "incomplete"
            } else {
                "completed"
            };
            let name = self.func_names.get(&key).cloned().unwrap_or_default();

            if flag(&self.func_item_custom, &key) {
                let input = unwrap_custom_tool_input(&args);
                let seq = self.next_seq();
                let input_done = json!({"type":"response.custom_tool_call_input.done","sequence_number": seq,"item_id": format!("ctc_{call_id}"),"output_index": output_index,"input": input});
                out.push(emit_resp_event(
                    "response.custom_tool_call_input.done",
                    &input_done,
                ));

                let mut item = custom_tool_call_item(&call_id, tool_status, &input);
                self.tool_index.apply_identity(&mut item, &name);
                let seq = self.next_seq();
                let item_done = json!({"type":"response.output_item.done","sequence_number": seq,"output_index": output_index,"item": item});
                out.push(emit_resp_event("response.output_item.done", &item_done));
                self.func_item_done.insert(key.clone(), true);
                self.func_args_done.insert(key, true);
                continue;
            }
            let seq = self.next_seq();
            let fc_done = json!({"type":"response.function_call_arguments.done","sequence_number": seq,"item_id": format!("fc_{call_id}"),"output_index": output_index,"arguments": args});
            out.push(emit_resp_event(
                "response.function_call_arguments.done",
                &fc_done,
            ));

            let mut item = function_call_item(&call_id, tool_status, &args);
            self.tool_index.apply_identity(&mut item, &name);
            let seq = self.next_seq();
            let item_done = json!({"type":"response.output_item.done","sequence_number": seq,"output_index": output_index,"item": item});
            out.push(emit_resp_event("response.output_item.done", &item_done));
            self.func_item_done.insert(key.clone(), true);
            self.func_args_done.insert(key, true);
        }
    }

    // port of buildResponsesCompletedEvent (openai_openai-responses_response.go)
    fn build_responses_completed_event(&mut self) -> String {
        let incomplete_details = incomplete_by_finish_reason(&self.finish_reason);
        let (event_type, status) = if incomplete_details.is_some() {
            ("response.incomplete", "incomplete")
        } else {
            ("response.completed", "completed")
        };
        let item_status = status;

        let seq = self.next_seq();
        let mut response = json!({"id": self.response_id,"object":"response","created_at": self.created,"status": status,"background":false,"error":null});
        if let Some(details) = incomplete_details {
            set(&mut response, "incomplete_details", details);
        }
        // Inject original request fields into response as per docs/response.completed.json
        if let Some(req) = &self.request_json {
            echo_request_fields(&mut response, req, false);
        }

        let mut output_items: Vec<(i64, Value)> = Vec::new();
        for r in &self.reasonings {
            let item = json!({"id": r.reasoning_id,"type":"reasoning","summary":[{"type":"summary_text","text": r.reasoning_data}]});
            output_items.push((r.output_index, item));
        }
        for i in self.msg_item_added.keys() {
            let txt = self.msg_text_buf.get(i).cloned().unwrap_or_default();
            let item = message_item(
                &format!("msg_{}_{}", self.response_id, i),
                item_status,
                &txt,
            );
            output_items.push((self.msg_output_ix.get(i).copied().unwrap_or(0), item));
        }
        for (key, args) in &self.func_args_buf {
            if !flag(&self.func_item_done, key) {
                continue;
            }
            let call_id = self.func_call_ids.get(key).cloned().unwrap_or_default();
            let name = self.func_names.get(key).cloned().unwrap_or_default();
            let index = self.func_output_ix.get(key).copied().unwrap_or(0);
            let mut item = if flag(&self.func_item_custom, key) {
                custom_tool_call_item(&call_id, item_status, &unwrap_custom_tool_input(args))
            } else {
                function_call_item(&call_id, item_status, args)
            };
            self.tool_index.apply_identity(&mut item, &name);
            output_items.push((index, item));
        }
        output_items.sort_by_key(|(index, _)| *index);
        if !output_items.is_empty() {
            set(
                &mut response,
                "output",
                Value::Array(output_items.into_iter().map(|(_, item)| item).collect()),
            );
        }
        if self.usage_seen {
            let mut usage = json!({
                "input_tokens": self.prompt_tokens,
                "input_tokens_details": {"cached_tokens": self.cached_tokens},
                "output_tokens": self.completion_tokens,
            });
            if self.reasoning_tokens > 0 {
                set(
                    &mut usage,
                    "output_tokens_details",
                    json!({"reasoning_tokens": self.reasoning_tokens}),
                );
            }
            let mut total = self.total_tokens;
            if total == 0 {
                total = self.prompt_tokens + self.completion_tokens;
            }
            set(&mut usage, "total_tokens", json!(total));
            set(&mut response, "usage", usage);
        }
        let completed = json!({"type": event_type,"sequence_number": seq,"response": response});
        emit_resp_event(event_type, &completed)
    }
}

/// Complete upstream non-stream response (chat.completion JSON) → client
/// Responses `response` object JSON.
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    // Go passes the translated Chat request as requestRawJSON for the echo;
    // this API only has the client request, so it is used for both (which is
    // also what the Go tests do, and what the streaming path echoes).
    convert_non_stream(upstream, Some(original_request), Some(original_request))
}

// port of ConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream (openai_openai-responses_response.go)
fn convert_non_stream(
    root: &Value,
    original_request: Option<&Value>,
    request: Option<&Value>,
) -> Value {
    let request_for_namespace = pick_request_json(original_request, request);
    let tool_index =
        ResponsesToolIndex::new(request_for_namespace.as_ref().unwrap_or(&Value::Null));

    let finish_reason = gs(root, "choices.0.finish_reason");
    let incomplete_details = incomplete_by_finish_reason(&finish_reason);
    let is_incomplete = incomplete_details.is_some();
    let item_status = if is_incomplete {
        "incomplete"
    } else {
        "completed"
    };

    let mut resp = json!({"id":"","object":"response","created_at":0,"status": item_status,"background":false,"error":null,"incomplete_details":null});
    if let Some(details) = incomplete_details {
        resp["incomplete_details"] = details;
    }

    // id: use provider id if present, otherwise synthesize
    let mut id = gs(root, "id");
    if id.is_empty() {
        id = synthesize_response_id();
    }
    resp["id"] = json!(id);

    // created_at: map from chat.completion created
    let mut created = gint(gpath(root, "created"));
    if created == 0 {
        created = unix_now();
    }
    resp["created_at"] = json!(created);

    // Echo request fields when available (aligns with streaming path behavior)
    if let Some(req) = request {
        echo_request_fields(&mut resp, req, true);
        if gpath(req, "model").is_none() {
            if let Some(v) = gpath(root, "model") {
                // Go sets model between max_tool_calls and parallel_tool_calls;
                // key order is not significant for the JSON value.
                set(&mut resp, "model", json!(gstr(Some(v))));
            }
        }
    } else if let Some(v) = gpath(root, "model") {
        // Fallback model from response
        set(&mut resp, "model", json!(gstr(Some(v))));
    }

    // Build output list from choices[...]
    let mut output_items: Vec<Value> = Vec::new();
    // Detect and capture reasoning content if present (with fallback to reasoning)
    let mut rc_text = gs(root, "choices.0.message.reasoning_content");
    if rc_text.is_empty() {
        rc_text = gs(root, "choices.0.message.reasoning");
    }
    let mut include_reasoning = !rc_text.is_empty();
    if !include_reasoning {
        if let Some(req) = request {
            include_reasoning = gpath(req, "reasoning").is_some();
        }
    }
    if include_reasoning {
        let rid = id.strip_prefix("resp_").unwrap_or(&id);
        // Prefer summary_text from reasoning_content; encrypted_content is optional
        let mut reasoning_item = json!({"id": format!("rs_{rid}"),"type":"reasoning","encrypted_content":"","summary":[]});
        if !rc_text.is_empty() {
            reasoning_item["summary"] = json!([{"type":"summary_text","text": rc_text}]);
        }
        output_items.push(reasoning_item);
    }

    for choice in as_array(gpath(root, "choices")) {
        let Some(msg) = gpath(choice, "message") else {
            continue;
        };
        let choice_index = gint(gpath(choice, "index"));
        // Text message part
        let content = gs(msg, "content");
        if !content.is_empty() {
            output_items.push(message_item(
                &format!("msg_{id}_{choice_index}"),
                item_status,
                &content,
            ));
        }

        // Function/tool calls
        if let Some(Value::Array(tcs)) = gpath(msg, "tool_calls") {
            for (tc_index, tc) in tcs.iter().enumerate() {
                let mut call_id = gs(tc, "id");
                if call_id.is_empty() {
                    // Providers may omit tool_call ids; synthesize one so the
                    // function_call item stays usable for Codex round-trips.
                    call_id = format!("call_{id}_{choice_index}_{tc_index}");
                }
                let name = tool_index.canonical_name(&gs(tc, "function.name"));
                let args = gs(tc, "function.arguments");
                let mut item = if tool_index.custom.contains(&name) {
                    custom_tool_call_item(&call_id, item_status, &unwrap_custom_tool_input(&args))
                } else {
                    function_call_item(&call_id, item_status, &args)
                };
                tool_index.apply_identity(&mut item, &name);
                output_items.push(item);
            }
        }
    }
    if !output_items.is_empty() {
        set(&mut resp, "output", Value::Array(output_items));
    }

    // usage mapping
    if let Some(usage) = gpath(root, "usage") {
        if gpath(usage, "prompt_tokens").is_some()
            || gpath(usage, "completion_tokens").is_some()
            || gpath(usage, "total_tokens").is_some()
        {
            let mut u = json!({"input_tokens": gint(gpath(usage, "prompt_tokens"))});
            if let Some(d) = gpath(usage, "prompt_tokens_details.cached_tokens") {
                set(
                    &mut u,
                    "input_tokens_details",
                    json!({"cached_tokens": gint(Some(d))}),
                );
            }
            set(
                &mut u,
                "output_tokens",
                json!(gint(gpath(usage, "completion_tokens"))),
            );
            // Reasoning tokens not available in Chat Completions; set only if present under output_tokens_details
            if let Some(d) = gpath(usage, "output_tokens_details.reasoning_tokens") {
                set(
                    &mut u,
                    "output_tokens_details",
                    json!({"reasoning_tokens": gint(Some(d))}),
                );
            }
            set(
                &mut u,
                "total_tokens",
                json!(gint(gpath(usage, "total_tokens"))),
            );
            set(&mut resp, "usage", u);
        } else {
            // Fallback to raw usage object if structure differs
            set(&mut resp, "usage", usage.clone());
        }
    }

    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers ------------------------------------------------------------

    fn opt(v: &Value) -> Option<&Value> {
        if v.is_null() {
            None
        } else {
            Some(v)
        }
    }

    /// Splits one client SSE frame into (event, parsed data), asserting the
    /// `event: <type>\ndata: <json>\n\n` shape.
    fn parse_frame(frame: &str) -> (String, Value) {
        let body = frame
            .strip_suffix("\n\n")
            .unwrap_or_else(|| panic!("frame not terminated by a blank line: {frame:?}"));
        let (event_line, data_line) = body
            .split_once('\n')
            .unwrap_or_else(|| panic!("frame without data line: {frame:?}"));
        let event = event_line
            .strip_prefix("event: ")
            .unwrap_or_else(|| panic!("frame without event line: {frame:?}"));
        let data = data_line
            .strip_prefix("data: ")
            .unwrap_or_else(|| panic!("frame without data payload: {frame:?}"));
        (
            event.to_string(),
            serde_json::from_str(data).expect("frame data is JSON"),
        )
    }

    /// DeepSeek-style reasoning-only answer, then `[DONE]`: the terminal
    /// `response.completed` arrives with the reasoning item as its output —
    /// without it Codex reports "stream closed before response.completed".
    fn reasoning_only_stream_completes() {
        let mut t = StreamTranslator::new(&json!({"model": "deepseek-v4-flash"}));
        t.push(None, &json!({"id":"resp_reasoning_only","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"still thinking"},"finish_reason":null}]}));
        let done = t.finish();
        let last = parse_frame(done.last().expect("terminal frame"));
        assert_eq!(last.0, "response.completed");
        assert_eq!(last.1["response"]["status"], json!("completed"));
        assert_eq!(last.1["response"]["output"][0]["type"], json!("reasoning"));
        assert_eq!(
            last.1["response"]["output"][0]["summary"][0]["text"],
            json!("still thinking")
        );
    }

    fn frames_as_json(frames: &[String]) -> Value {
        Value::Array(
            frames
                .iter()
                .map(|f| {
                    let (event, data) = parse_frame(f);
                    json!({"event": event, "data": data})
                })
                .collect(),
        )
    }

    fn assert_json_eq(got: &Value, want: &Value, ctx: &str) {
        assert!(
            got == want,
            "{ctx}\n--- got ---\n{}\n--- want ---\n{}",
            serde_json::to_string_pretty(got).unwrap(),
            serde_json::to_string_pretty(want).unwrap()
        );
    }

    /// Replays every recorded call one Go test made against the Go
    /// implementation (commit ed980be) and asserts the exact same JSON.
    fn run_golden(test: &str, expected_cases: usize) {
        let mut n = 0;
        for (line_no, line) in GOLDEN.lines().enumerate() {
            let case: Value = serde_json::from_str(line).expect("golden line is JSON");
            if case["t"] != test {
                continue;
            }
            n += 1;
            let ctx = format!("{test} case #{n} (golden line {})", line_no + 1);
            match case["k"].as_str().unwrap() {
                "req" => {
                    let got = translate_request(
                        case["m"].as_str().unwrap(),
                        &case["in"],
                        case["s"].as_bool().unwrap(),
                    );
                    assert_json_eq(&got, &case["out"], &ctx);
                }
                "ns" => {
                    let upstream = &case["in"];
                    let got = convert_non_stream(upstream, opt(&case["o"]), opt(&case["r"]));
                    let mut want = case["out"].clone();
                    // Ids and timestamps the Go side synthesized from the clock.
                    if gs(upstream, "id").is_empty() {
                        let strip = |v: &Value| {
                            let s = v.as_str().unwrap().to_string();
                            s.strip_prefix("resp_").map(str::to_string).unwrap_or(s)
                        };
                        let (from, to) = (strip(&want["id"]), strip(&got["id"]));
                        want = serde_json::from_str(&want.to_string().replace(&from, &to)).unwrap();
                    }
                    if gint(gpath(upstream, "created")) == 0 {
                        want["created_at"] = got["created_at"].clone();
                    }
                    assert_json_eq(&got, &want, &ctx);
                }
                "st" => {
                    let mut tr = StreamTranslator::with_requests(
                        opt(&case["o"]).cloned(),
                        opt(&case["r"]).cloned(),
                        case["m"].as_str().unwrap(),
                    );
                    for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                        let frames = if step.get("done").is_some() {
                            tr.finish()
                        } else if step.get("empty").is_some() {
                            Vec::new()
                        } else {
                            tr.push(None, &step["in"])
                        };
                        assert_json_eq(
                            &frames_as_json(&frames),
                            &step["out"],
                            &format!("{ctx} step {i}"),
                        );
                    }
                }
                other => panic!("unknown golden kind {other}"),
            }
        }
        assert_eq!(n, expected_cases, "{test}: golden case count");
    }

    // -- hand-written tests ---------------------------------------------------

    fn realistic_request() -> Value {
        json!({"model":"gpt-5-codex","instructions":"You are Codex.","input":[{"role":"user","content":"list files"}],"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object"}}],"stream":true,"reasoning":{"effort":"medium"}})
    }

    fn chunk(choices: Value) -> Value {
        json!({"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-chat","choices": choices})
    }

    /// End-to-end: a DeepSeek-style chunk sequence with reasoning_content, text
    /// and a streamed tool call, through the public API. Expected frames were
    /// produced by the Go implementation for the same input.
    #[test]
    fn stream_end_to_end_reasoning_text_and_tool_call() {
        let mut tr = StreamTranslator::new(&realistic_request());
        let mut frames = Vec::new();
        for c in [
            chunk(
                json!([{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":"Let me"},"finish_reason":null}]),
            ),
            chunk(
                json!([{"index":0,"delta":{"reasoning_content":" check."},"finish_reason":null}]),
            ),
            chunk(json!([{"index":0,"delta":{"content":"Checking"},"finish_reason":null}])),
            chunk(json!([{"index":0,"delta":{"content":" files."},"finish_reason":null}])),
            chunk(
                json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"exec_command","arguments":""}}]},"finish_reason":null}]),
            ),
            chunk(
                json!([{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]},"finish_reason":null}]),
            ),
            chunk(
                json!([{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]),
            ),
            chunk(json!([{"index":0,"delta":{},"finish_reason":"tool_calls"}])),
            {
                let mut usage_only = chunk(json!([]));
                usage_only["usage"] = json!({"prompt_tokens":50,"completion_tokens":20,"total_tokens":70,"prompt_tokens_details":{"cached_tokens":10},"completion_tokens_details":{"reasoning_tokens":5}});
                usage_only
            },
        ] {
            frames.extend(tr.push(None, &c));
        }
        frames.extend(tr.finish());
        assert!(tr.finish().is_empty(), "a second finish() emits nothing");

        let want = Value::Array(vec![
            json!({"event":"response.created","data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl-1","object":"response","created_at":1700000000,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5-codex"}}}),
            json!({"event":"response.in_progress","data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl-1","object":"response","created_at":1700000000,"status":"in_progress","output":[],"model":"gpt-5-codex"}}}),
            json!({"event":"response.output_item.added","data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"rs_chatcmpl-1_0","type":"reasoning","status":"in_progress","summary":[]}}}),
            json!({"event":"response.reasoning_summary_part.added","data":{"type":"response.reasoning_summary_part.added","sequence_number":4,"item_id":"rs_chatcmpl-1_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}}),
            json!({"event":"response.reasoning_summary_text.delta","data":{"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_chatcmpl-1_0","output_index":0,"summary_index":0,"delta":"Let me"}}),
            json!({"event":"response.reasoning_summary_text.delta","data":{"type":"response.reasoning_summary_text.delta","sequence_number":6,"item_id":"rs_chatcmpl-1_0","output_index":0,"summary_index":0,"delta":" check."}}),
            json!({"event":"response.reasoning_summary_text.done","data":{"type":"response.reasoning_summary_text.done","sequence_number":7,"item_id":"rs_chatcmpl-1_0","output_index":0,"summary_index":0,"text":"Let me check."}}),
            json!({"event":"response.reasoning_summary_part.done","data":{"type":"response.reasoning_summary_part.done","sequence_number":8,"item_id":"rs_chatcmpl-1_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Let me check."}}}),
            json!({"event":"response.output_item.done","data":{"type":"response.output_item.done","item":{"id":"rs_chatcmpl-1_0","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"Let me check."}]},"output_index":0,"sequence_number":9}}),
            json!({"event":"response.output_item.added","data":{"type":"response.output_item.added","sequence_number":10,"output_index":1,"item":{"id":"msg_chatcmpl-1_0","type":"message","status":"in_progress","content":[],"role":"assistant"}}}),
            json!({"event":"response.content_part.added","data":{"type":"response.content_part.added","sequence_number":11,"item_id":"msg_chatcmpl-1_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}}),
            json!({"event":"response.output_text.delta","data":{"type":"response.output_text.delta","sequence_number":12,"item_id":"msg_chatcmpl-1_0","output_index":1,"content_index":0,"delta":"Checking","logprobs":[]}}),
            json!({"event":"response.output_text.delta","data":{"type":"response.output_text.delta","sequence_number":13,"item_id":"msg_chatcmpl-1_0","output_index":1,"content_index":0,"delta":" files.","logprobs":[]}}),
            json!({"event":"response.output_text.done","data":{"type":"response.output_text.done","sequence_number":14,"item_id":"msg_chatcmpl-1_0","output_index":1,"content_index":0,"text":"Checking files.","logprobs":[]}}),
            json!({"event":"response.content_part.done","data":{"type":"response.content_part.done","sequence_number":15,"item_id":"msg_chatcmpl-1_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Checking files."}}}),
            json!({"event":"response.output_item.done","data":{"type":"response.output_item.done","sequence_number":16,"output_index":1,"item":{"id":"msg_chatcmpl-1_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Checking files."}],"role":"assistant"}}}),
            json!({"event":"response.output_item.added","data":{"type":"response.output_item.added","sequence_number":17,"output_index":2,"item":{"id":"fc_call_abc","type":"function_call","status":"in_progress","arguments":"","call_id":"call_abc","name":"exec_command"}}}),
            json!({"event":"response.function_call_arguments.delta","data":{"type":"response.function_call_arguments.delta","sequence_number":18,"item_id":"fc_call_abc","output_index":2,"delta":"{\"cmd\":"}}),
            json!({"event":"response.function_call_arguments.delta","data":{"type":"response.function_call_arguments.delta","sequence_number":19,"item_id":"fc_call_abc","output_index":2,"delta":"\"ls\"}"}}),
            json!({"event":"response.function_call_arguments.done","data":{"type":"response.function_call_arguments.done","sequence_number":20,"item_id":"fc_call_abc","output_index":2,"arguments":"{\"cmd\":\"ls\"}"}}),
            json!({"event":"response.output_item.done","data":{"type":"response.output_item.done","sequence_number":21,"output_index":2,"item":{"id":"fc_call_abc","type":"function_call","status":"completed","arguments":"{\"cmd\":\"ls\"}","call_id":"call_abc","name":"exec_command"}}}),
            json!({"event":"response.completed","data":{"type":"response.completed","sequence_number":22,"response":{"id":"chatcmpl-1","object":"response","created_at":1700000000,"status":"completed","background":false,"error":null,"instructions":"You are Codex.","model":"gpt-5-codex","reasoning":{"effort":"medium"},"tools":[{"name":"exec_command","parameters":{"type":"object"},"type":"function"}],"output":[{"id":"rs_chatcmpl-1_0","type":"reasoning","summary":[{"type":"summary_text","text":"Let me check."}]},{"id":"msg_chatcmpl-1_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Checking files."}],"role":"assistant"},{"id":"fc_call_abc","type":"function_call","status":"completed","arguments":"{\"cmd\":\"ls\"}","call_id":"call_abc","name":"exec_command"}],"usage":{"input_tokens":50,"input_tokens_details":{"cached_tokens":10},"output_tokens":20,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":70}}}}),
        ]);
        assert_json_eq(&frames_as_json(&frames), &want, "end-to-end stream");
    }

    #[test]
    fn request_end_to_end_through_public_api() {
        let got = translate_request("deepseek-chat", &realistic_request(), true);
        let want = json!({"model":"deepseek-chat","messages":[{"role":"system","content":"You are Codex."},{"role":"user","content":"list files"}],"stream":true,"tools":[{"type":"function","function":{"name":"exec_command","description":"","parameters":{"type":"object"}}}],"reasoning_effort":"medium"});
        assert_json_eq(&got, &want, "end-to-end request");
    }

    #[test]
    fn non_stream_end_to_end_through_public_api() {
        let upstream = json!({"id":"chatcmpl-2","object":"chat.completion","created":1700000001,"model":"deepseek-chat","choices":[{"index":0,"message":{"role":"assistant","content":"Checking files.","reasoning_content":"Let me check.","tool_calls":[{"id":"call_abc","type":"function","function":{"name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":50,"completion_tokens":20,"total_tokens":70,"prompt_tokens_details":{"cached_tokens":10}}});
        let got = translate_non_stream(&upstream, &realistic_request());
        let want = json!({"id":"chatcmpl-2","object":"response","created_at":1700000001,"status":"completed","background":false,"error":null,"incomplete_details":null,"instructions":"You are Codex.","model":"gpt-5-codex","reasoning":{"effort":"medium"},"tools":[{"name":"exec_command","parameters":{"type":"object"},"type":"function"}],"output":[{"id":"rs_chatcmpl-2","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"Let me check."}]},{"id":"msg_chatcmpl-2_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Checking files."}],"role":"assistant"},{"id":"fc_call_abc","type":"function_call","status":"completed","arguments":"{\"cmd\":\"ls\"}","call_id":"call_abc","name":"exec_command"}],"usage":{"input_tokens":50,"input_tokens_details":{"cached_tokens":10},"output_tokens":20,"total_tokens":70}});
        assert_json_eq(&got, &want, "end-to-end non-stream");
    }

    #[test]
    fn non_stream_synthesizes_unique_ids() {
        let upstream =
            json!({"choices":[{"index":0,"message":{"content":"hi"},"finish_reason":"stop"}]});
        let a = translate_non_stream(&upstream, &json!({}));
        let b = translate_non_stream(&upstream, &json!({}));
        let (ia, ib) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
        assert!(ia.starts_with("resp_") && ib.starts_with("resp_"));
        assert_ne!(ia, ib);
        assert!(a["created_at"].as_i64().unwrap() > 0);
    }

    // port of TestResponsesSingleCustomToolName_CountsDeduplicatedTools (openai_openai-responses_request_test.go)
    #[test]
    fn go_single_custom_tool_name_counts_deduplicated_tools() {
        let raw = json!({"input":[{"role":"user","content":"Patch the file."},{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch","description":"copy"}]}],"tools":[{"type":"custom","name":"apply_patch","description":"authoritative"}]});
        assert_eq!(
            ResponsesToolIndex::new(&raw).single_custom_name(),
            Some("apply_patch".to_string())
        );
    }

    /// splitResponsesQualifiedFunctionCallFromRequest, used only by the Go tests.
    fn split_qualified(raw: &Value, qualified: &str) -> (String, String) {
        let qualified = qualified.trim();
        if qualified.is_empty() {
            return (String::new(), String::new());
        }
        match ResponsesToolIndex::new(raw).by_chat.get(qualified) {
            Some(d) => (d.local_name.clone(), d.namespace.clone()),
            None => (qualified.to_string(), String::new()),
        }
    }

    // port of TestSplitResponsesQualifiedFunctionCallFromRequest_FirstDeclarationWins (openai_openai-responses_request_test.go)
    #[test]
    fn go_split_qualified_function_call_first_declaration_wins() {
        let flat_first = json!({"tools":[{"type":"function","name":"editor__apply_patch","parameters":{"type":"object"}},{"type":"namespace","name":"editor","tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object"}}]}]});
        let namespace_first = json!({"tools":[{"type":"namespace","name":"editor","tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object"}}]},{"type":"function","name":"editor__apply_patch","parameters":{"type":"object"}}]});
        let namespace_only = json!({"tools":[{"type":"namespace","name":"mcp__github","tools":[{"type":"function","name":"get_me","parameters":{"type":"object"}}]}]});
        let cases = [
            (
                &flat_first,
                "editor__apply_patch",
                "editor__apply_patch",
                "",
            ),
            (
                &namespace_first,
                "editor__apply_patch",
                "apply_patch",
                "editor",
            ),
            (
                &namespace_only,
                "mcp__github__get_me",
                "get_me",
                "mcp__github",
            ),
            (&flat_first, "something_else", "something_else", ""),
        ];
        for (raw, qualified, want_name, want_namespace) in cases {
            assert_eq!(
                split_qualified(raw, qualified),
                (want_name.to_string(), want_namespace.to_string()),
                "split({qualified})"
            );
        }
    }

    // port of TestSplitResponsesQualifiedFunctionCallFromRequest_MatchesMergedToolIdentity (openai_openai-responses_request_test.go)
    #[test]
    fn go_split_qualified_function_call_matches_merged_tool_identity() {
        let raw = json!({"tools":[{"type":"function","name":"editor__apply_patch","parameters":{"type":"object"}},{"type":"namespace","name":"editor","tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object"}}]}]});
        let merged = ResponsesToolIndex::new(&raw).chat_tools();
        assert_json_eq(
            &Value::Array(merged.clone()),
            &json!([{"type":"function","function":{"name":"editor__apply_patch","description":"","parameters":{"type":"object"}}}]),
            "merged tools",
        );
        let emitted = gs(&merged[0], "function.name");
        assert_eq!(
            split_qualified(&raw, &emitted),
            (emitted.clone(), String::new())
        );
    }

    // port of TestResponsesCustomToolNames_FollowsMergedDeclaration (openai_openai-responses_request_test.go)
    #[test]
    fn go_custom_tool_names_follow_merged_declaration() {
        let function_first = json!({"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"exec","description":"copy"}]}],"tools":[{"type":"function","name":"exec","parameters":{"type":"object"}}]});
        let custom_first = json!({"input":[{"type":"additional_tools","tools":[{"type":"function","name":"exec","parameters":{"type":"object"}}]}],"tools":[{"type":"custom","name":"exec","description":"authoritative"}]});
        for (raw, want_custom) in [(&function_first, false), (&custom_first, true)] {
            let index = ResponsesToolIndex::new(raw);
            let merged = index.chat_tools();
            assert_eq!(merged.len(), 1);
            assert_eq!(
                gpath(&merged[0], "function.parameters.properties.input").is_some(),
                want_custom
            );
            assert_eq!(index.custom.contains("exec"), want_custom);
            assert_eq!(
                index.single_custom_name(),
                want_custom.then(|| "exec".to_string())
            );
        }
    }

    // port of TestResponsesCustomToolNames_OnlyReportsMergedTools (openai_openai-responses_request_test.go)
    #[test]
    fn go_custom_tool_names_only_report_merged_tools() {
        let raw = json!({"tools":[{"type":"namespace","name":"outer","tools":[{"type":"namespace","name":"inner","tools":[{"type":"custom","name":"buried"}]},{"type":"custom","name":"reachable"}]}]});
        let index = ResponsesToolIndex::new(&raw);
        let merged: HashSet<String> = index
            .chat_tools()
            .iter()
            .map(|t| gs(t, "function.name"))
            .collect();
        assert_eq!(merged, HashSet::from(["outer__reachable".to_string()]));
        assert_eq!(
            index.custom,
            HashSet::from(["outer__reachable".to_string()])
        );
    }

    // port of TestNamespaceRecoveryDoesNotGuessAmbiguousOrOverrideExactNames (custom_tool_namespace_recovery_test.go)
    #[test]
    fn go_namespace_recovery_does_not_guess_ambiguous_or_override_exact_names() {
        let recovery = json!({"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]});
        let cases = [
            (recovery.clone(), "wait", "functions__wait"),
            (recovery, "unknown", "unknown"),
            (
                json!({"tools":[{"type":"function","name":"exec"},{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]}),
                "exec",
                "exec",
            ),
            (
                json!({"tools":[{"type":"namespace","name":"first","tools":[{"type":"custom","name":"exec"}]},{"type":"namespace","name":"second","tools":[{"type":"custom","name":"exec"}]}]}),
                "exec",
                "exec",
            ),
        ];
        for (request, name, want) in cases {
            assert_eq!(ResponsesToolIndex::new(&request).canonical_name(name), want);
        }
    }

    #[test]
    fn cap_keeps_tail_and_strips_leading_separators() {
        let name = format!("mcp__{}__tool", "x".repeat(70));
        let capped = cap_responses_chat_tool_name(&name);
        assert_eq!(capped, format!("{}__tool", "x".repeat(58)));
        // A multibyte boundary never panics.
        let wide = format!("{}é{}", "a".repeat(10), "b".repeat(63));
        assert_eq!(cap_responses_chat_tool_name(&wide), "b".repeat(63));
    }

    // -- golden replays of the Go test suite --------------------------------
    //
    // One test per Go test function. Each replays every call that Go test made
    // (request, non-stream, and per-chunk stream calls) with the Go outputs
    // recorded from commit ed980be. Video-input tests are excluded (out of
    // scope), as are the digest test's 10/100-turn perf requests.

    #[test]
    fn go_custom_tool_namespace_recovery_preserves_stream_and_non_stream() {
        run_golden(
            "TestCustomToolNamespaceRecoveryPreservesStreamAndNonStream",
            4,
        );
    }

    #[test]
    fn go_custom_tool_replay_preserves_namespace_and_result_pair() {
        run_golden("TestCustomToolReplayPreservesNamespaceAndResultPair", 2);
    }

    #[test]
    fn go_req_merge_consecutive_function_calls() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MergeConsecutiveFunctionCalls", 1);
    }

    #[test]
    fn go_req_split_function_calls_when_interrupted() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SplitFunctionCallsWhenInterrupted", 1);
    }

    #[test]
    fn go_req_defers_message_until_tool_output() {
        run_golden(
            "TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DefersMessageUntilToolOutput",
            1,
        );
    }

    #[test]
    fn go_req_unwraps_stringified_tool_output_images() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnwrapsStringifiedToolOutputImages", 2);
    }

    #[test]
    fn go_req_unwraps_stringified_custom_tool_output_images() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnwrapsStringifiedCustomToolOutputImages", 1);
    }

    #[test]
    fn go_req_preserves_custom_tool_output_fallbacks() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesCustomToolOutputFallbacks", 3);
    }

    #[test]
    fn go_req_converts_structured_tool_output_images() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsStructuredToolOutputImages", 1);
    }

    #[test]
    fn go_req_keeps_non_image_tool_output_strings() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings", 9);
    }

    #[test]
    fn go_req_attaches_reasoning_to_assistant_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_AttachesReasoningToAssistantMessage", 1);
    }

    #[test]
    fn go_req_preserves_assistant_content_with_tool_calls() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesAssistantContentWithToolCalls", 1);
    }

    #[test]
    fn go_req_does_not_merge_tool_calls_across_user_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DoesNotMergeToolCallsAcrossUserMessage", 1);
    }

    #[test]
    fn go_req_merges_distinct_reasoning_within_assistant_turn() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MergesDistinctReasoningWithinAssistantTurn", 1);
    }

    #[test]
    fn go_req_replaces_unavailable_reasoning_within_assistant_turn() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ReplacesUnavailableReasoningWithinAssistantTurn", 1);
    }

    #[test]
    fn go_req_attaches_reasoning_to_tool_call_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_AttachesReasoningToToolCallMessage", 1);
    }

    #[test]
    fn go_req_keeps_reasoning_before_user_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsReasoningBeforeUserMessage", 1);
    }

    #[test]
    fn go_req_preserves_reasoning_on_follow_up_tool_turns() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesReasoningOnFollowUpToolTurns", 1);
    }

    #[test]
    fn go_req_falls_back_to_placeholder_when_no_prior_reasoning() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FallsBackToPlaceholderWhenNoPriorReasoning", 1);
    }

    #[test]
    fn go_req_effort_none_does_not_inject_reasoning() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_EffortNoneDoesNotInjectReasoning", 1);
    }

    #[test]
    fn go_req_preserves_reasoning_on_custom_tool_call_turns() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesReasoningOnCustomToolCallTurns", 1);
    }

    #[test]
    fn go_req_resets_reasoning_across_user_message_boundary() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ResetsReasoningAcrossUserMessageBoundary", 1);
    }

    #[test]
    fn go_req_flattens_namespace_tools() {
        run_golden(
            "TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FlattensNamespaceTools",
            1,
        );
    }

    #[test]
    fn go_req_qualifies_namespace_function_call_history() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiesNamespaceFunctionCallHistory", 1);
    }

    #[test]
    fn go_req_flattens_namespace_custom_tools() {
        run_golden(
            "TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FlattensNamespaceCustomTools",
            2,
        );
    }

    #[test]
    fn go_req_preserves_structured_tool_choice() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesStructuredToolChoice", 1);
    }

    #[test]
    fn go_req_converts_canonical_responses_named_tool_choice() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsCanonicalResponsesNamedToolChoice", 1);
    }

    #[test]
    fn go_req_converts_namespace_and_custom_tool_choice() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice", 6);
    }

    #[test]
    fn go_req_omits_tool_settings_without_tools() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OmitsToolSettingsWithoutTools", 2);
    }

    #[test]
    fn go_req_preserves_parallel_tool_calls_with_tools() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesParallelToolCallsWithTools", 1);
    }

    #[test]
    fn go_req_preserves_json_schema_text_format() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesJSONSchemaTextFormat", 1);
    }

    #[test]
    fn go_req_preserves_json_object_text_format() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesJSONObjectTextFormat", 1);
    }

    #[test]
    fn go_req_omits_response_format_without_text_format() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OmitsResponseFormatWithoutTextFormat", 1);
    }

    #[test]
    fn go_req_normalizes_input_image_detail() {
        run_golden(
            "TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_NormalizesInputImageDetail",
            4,
        );
    }

    #[test]
    fn go_req_deduplicates_tools_across_additional_tools() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DeduplicatesToolsAcrossAdditionalTools", 1);
    }

    #[test]
    fn go_req_deduplicates_namespace_qualified_collision() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DeduplicatesNamespaceQualifiedCollision", 1);
    }

    #[test]
    fn go_req_keeps_distinct_tools_from_both_sources() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsDistinctToolsFromBothSources", 1);
    }

    #[test]
    fn go_req_function_call_output_alternate_ids_and_queue_fallback() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FunctionCallOutputAlternateIDsAndQueueFallback", 5);
    }

    #[test]
    fn go_req_mixed_missing_and_explicit_parallel_outputs() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MixedMissingAndExplicitParallelOutputs", 1);
    }

    #[test]
    fn go_req_defers_message_until_missing_id_tool_output() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DefersMessageUntilMissingIDToolOutput", 1);
    }

    #[test]
    fn go_req_mixed_missing_and_explicit_parallel_outputs_across_user_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MixedMissingAndExplicitParallelOutputsAcrossUserMessage", 1);
    }

    #[test]
    fn go_req_orphan_function_call_output_becomes_user_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OrphanFunctionCallOutputBecomesUserMessage", 1);
    }

    #[test]
    fn go_req_unpaired_explicit_call_id_becomes_user_message() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnpairedExplicitCallIDBecomesUserMessage", 1);
    }

    #[test]
    fn go_req_caps_long_namespace_tool_names() {
        run_golden(
            "TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_CapsLongNamespaceToolNames",
            1,
        );
    }

    #[test]
    fn go_req_disambiguates_truncation_collisions() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DisambiguatesTruncationCollisions", 2);
    }

    #[test]
    fn go_req_long_declaration_does_not_displace_short_original() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongDeclarationDoesNotDisplaceShortOriginal", 4);
    }

    #[test]
    fn go_req_ambiguous_long_local_name_stays_unresolved() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_AmbiguousLongLocalNameStaysUnresolved", 1);
    }

    #[test]
    fn go_req_long_alias_does_not_displace_namespaced_local_name() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongAliasDoesNotDisplaceNamespacedLocalName", 3);
    }

    #[test]
    fn go_req_shared_local_name_is_never_emitted() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted", 6);
    }

    #[test]
    fn go_req_qualified_identity_outranks_foreign_local_name() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiedIdentityOutranksForeignLocalName", 5);
    }

    #[test]
    fn go_req_incomplete_tool_calls_do_not_defer_messages() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_IncompleteToolCallsDoNotDeferMessages", 1);
    }

    #[test]
    fn go_req_complete_tool_calls_do_pair_messages() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_CompleteToolCallsDoPairMessages", 1);
    }

    #[test]
    fn go_req_mixed_empty_id_does_not_reorder() {
        run_golden(
            "TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MixedEmptyIDDoesNotReorder",
            1,
        );
    }

    #[test]
    fn go_req_duplicate_call_id_does_not_reorder() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DuplicateCallIDDoesNotReorder", 1);
    }

    #[test]
    fn go_req_duplicate_output_call_id_does_not_reorder() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DuplicateOutputCallIDDoesNotReorder", 1);
    }

    #[test]
    fn go_req_duplicate_custom_output_call_id_does_not_reorder() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DuplicateCustomOutputCallIDDoesNotReorder", 1);
    }

    #[test]
    fn go_req_multiple_outputs_without_id_do_not_guess_or_reorder() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MultipleOutputsWithoutIDDoNotGuessOrReorder", 1);
    }

    #[test]
    fn go_req_multiple_outputs_without_id_and_orphan_output_do_not_guess_or_reorder() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MultipleOutputsWithoutIDAndOrphanOutputDoNotGuessOrReorder", 1);
    }

    #[test]
    fn go_req_maps_max_output_tokens_to_max_tokens() {
        run_golden("TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MapsMaxOutputTokensToMaxTokens", 3);
    }

    #[test]
    fn go_stream_multiple_tool_calls_remain_separate() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_MultipleToolCallsRemainSeparate", 1);
    }

    #[test]
    fn go_stream_multi_choice_tool_calls_use_distinct_output_indexes() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_MultiChoiceToolCallsUseDistinctOutputIndexes", 1);
    }

    #[test]
    fn go_stream_mixed_message_and_tool_use_distinct_output_indexes() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_MixedMessageAndToolUseDistinctOutputIndexes", 1);
    }

    #[test]
    fn go_stream_completed_omits_top_level_output_text() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_CompletedOmitsTopLevelOutputText", 1);
    }

    #[test]
    fn go_stream_tool_call_completed_omits_top_level_output_text() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ToolCallCompletedOmitsTopLevelOutputText", 1);
    }

    #[test]
    fn go_stream_function_call_done_and_completed_output_stay_ascending() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FunctionCallDoneAndCompletedOutputStayAscending", 1);
    }

    #[test]
    fn go_non_stream_omits_top_level_output_text() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_OmitsTopLevelOutputText", 1);
    }

    #[test]
    fn go_stream_restores_namespace_function_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresNamespaceFunctionCall", 1);
    }

    #[test]
    fn go_non_stream_restores_namespace_function_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresNamespaceFunctionCall", 1);
    }

    #[test]
    fn go_stream_restores_capped_namespace_function_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresCappedNamespaceFunctionCall", 1);
    }

    #[test]
    fn go_non_stream_restores_capped_namespace_function_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresCappedNamespaceFunctionCall", 1);
    }

    #[test]
    fn go_stream_custom_tool_name_arrives_late() {
        run_golden(
            "TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_CustomToolNameArrivesLate",
            1,
        );
    }

    #[test]
    fn go_stream_custom_tool_name_and_id_are_missing() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_CustomToolNameAndIDAreMissing", 1);
    }

    #[test]
    fn go_stream_tool_call_id_may_arrive_late_or_be_missing() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ToolCallIDMayArriveLateOrBeMissing", 2);
    }

    #[test]
    fn go_stream_restores_additional_namespace_function_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresAdditionalNamespaceFunctionCall", 1);
    }

    #[test]
    fn go_non_stream_restores_additional_namespace_function_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresAdditionalNamespaceFunctionCall", 1);
    }

    #[test]
    fn go_stream_restores_additional_namespace_custom_tool_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresAdditionalNamespaceCustomToolCall", 1);
    }

    #[test]
    fn go_non_stream_restores_additional_namespace_custom_tool_call() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresAdditionalNamespaceCustomToolCall", 1);
    }

    #[test]
    fn go_stream_does_not_complete_reasoning_only_stream() {
        // Termory diverges from Go here: a reasoning-only stream still gets its
        // `response.completed` (Go left the client waiting for it).
        reasoning_only_stream_completes();
    }

    #[test]
    fn go_stream_incomplete_tool_stream_does_not_finalize_as_completed() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_IncompleteToolStreamDoesNotFinalizeAsCompleted", 2);
    }

    #[test]
    fn go_stream_finish_reason_length_emits_incomplete() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinishReasonLengthEmitsIncomplete", 1);
    }

    #[test]
    fn go_stream_finish_reason_content_filter_emits_incomplete() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinishReasonContentFilterEmitsIncomplete", 1);
    }

    #[test]
    fn go_non_stream_finish_reason_length() {
        run_golden(
            "TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_FinishReasonLength",
            1,
        );
    }

    #[test]
    fn go_non_stream_finish_reason_content_filter() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_FinishReasonContentFilter", 1);
    }

    #[test]
    fn go_non_stream_reasoning_fallback() {
        run_golden(
            "TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback",
            6,
        );
    }

    #[test]
    fn go_stream_chunk_with_content_and_reasoning_content() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ChunkWithContentAndReasoningContent", 1);
    }

    #[test]
    fn go_stream_single_chunk_with_both_content_and_reasoning_content() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_SingleChunkWithBothContentAndReasoningContent", 1);
    }

    #[test]
    fn go_responses_compatibility_digest() {
        run_golden("TestResponsesCompatibilityDigest", 18);
    }

    #[test]
    fn go_responses_request_selection_and_stream_isolation() {
        run_golden("TestResponsesRequestSelectionAndStreamIsolation", 4);
    }

    #[test]
    fn go_stream_response_completed_waits_for_done() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ResponseCompletedWaitsForDone", 4);
    }

    #[test]
    fn go_stream_empty_tool_calls_array_does_not_terminate_items() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_EmptyToolCallsArrayDoesNotTerminateItems", 1);
    }

    #[test]
    fn go_stream_finalizes_open_message_at_stream_end() {
        run_golden("TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinalizesOpenMessageAtStreamEnd", 2);
    }

    /// Recorded Go inputs/outputs (commit ed980be), one JSON case per line.
    const GOLDEN: &str = r####"{"t":"TestCustomToolNamespaceRecoveryPreservesStreamAndNonStream","k":"ns","o":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]},"r":null,"in":{"id":"fixture","choices":[{"index":0,"message":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"functions__exec","arguments":"{\"input\":\"text(\\\"测试\\\\n\\\");\"}"}}]},"finish_reason":"tool_calls"}]},"out":{"id":"fixture","object":"response","created_at":1790818739,"status":"completed","background":false,"error":null,"incomplete_details":null,"output":[{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"text(\"测试\\n\");","call_id":"call_fixture","name":"exec","namespace":"functions"}]}}
{"t":"TestCustomToolNamespaceRecoveryPreservesStreamAndNonStream","k":"st","o":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]},"r":null,"m":"fixture","steps":[{"in":{"id":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"functions__exec","arguments":"{\"input\":\"text(\\\"测试\\\\n\\\");\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"fixture","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[],"model":"fixture"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"fixture","object":"response","created_at":0,"status":"in_progress","output":[],"model":"fixture"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"ctc_call_fixture","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call_fixture","name":"exec","namespace":"functions"}},"event":"response.output_item.added"},{"data":{"type":"response.custom_tool_call_input.done","sequence_number":4,"item_id":"ctc_call_fixture","output_index":0,"input":"text(\"测试\\n\");"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"text(\"测试\\n\");","call_id":"call_fixture","name":"exec","namespace":"functions"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":6,"response":{"id":"fixture","object":"response","created_at":0,"status":"completed","background":false,"error":null,"output":[{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"text(\"测试\\n\");","call_id":"call_fixture","name":"exec","namespace":"functions"}]}},"event":"response.completed"}]}]}
{"t":"TestCustomToolNamespaceRecoveryPreservesStreamAndNonStream","k":"ns","o":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]},"r":null,"in":{"id":"fixture","choices":[{"index":0,"message":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"exec","arguments":"{\"input\":\"text(\\\"测试\\\\n\\\");\"}"}}]},"finish_reason":"tool_calls"}]},"out":{"id":"fixture","object":"response","created_at":1790818739,"status":"completed","background":false,"error":null,"incomplete_details":null,"output":[{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"text(\"测试\\n\");","call_id":"call_fixture","name":"exec","namespace":"functions"}]}}
{"t":"TestCustomToolNamespaceRecoveryPreservesStreamAndNonStream","k":"st","o":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]},"r":null,"m":"fixture","steps":[{"in":{"id":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"exec","arguments":"{\"input\":\"text(\\\"测试\\\\n\\\");\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"fixture","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[],"model":"fixture"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"fixture","object":"response","created_at":0,"status":"in_progress","output":[],"model":"fixture"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"ctc_call_fixture","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call_fixture","name":"exec","namespace":"functions"}},"event":"response.output_item.added"},{"data":{"type":"response.custom_tool_call_input.done","sequence_number":4,"item_id":"ctc_call_fixture","output_index":0,"input":"text(\"测试\\n\");"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"text(\"测试\\n\");","call_id":"call_fixture","name":"exec","namespace":"functions"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":6,"response":{"id":"fixture","object":"response","created_at":0,"status":"completed","background":false,"error":null,"output":[{"id":"ctc_call_fixture","type":"custom_tool_call","status":"completed","input":"text(\"测试\\n\");","call_id":"call_fixture","name":"exec","namespace":"functions"}]}},"event":"response.completed"}]}]}
{"t":"TestCustomToolReplayPreservesNamespaceAndResultPair","k":"req","m":"fixture","s":false,"in":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]},{"type":"custom_tool_call","name":"exec","namespace":"functions","call_id":"call_fixture","input":"text(1);"},{"type":"custom_tool_call_output","call_id":"call_fixture","output":[{"type":"input_text","text":"1"}]}]},"out":{"model":"fixture","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"text(1);\"}","name":"functions__exec"},"id":"call_fixture","type":"function"}]},{"role":"tool","tool_call_id":"call_fixture","content":"1"}],"stream":false,"tools":[{"function":{"description":"","name":"functions__exec","parameters":{"properties":{"input":{"type":"string"}},"required":["input"],"type":"object"}},"type":"function"},{"function":{"description":"","name":"functions__wait","parameters":{}},"type":"function"}]}}
{"t":"TestCustomToolReplayPreservesNamespaceAndResultPair","k":"req","m":"fixture","s":false,"in":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]},{"type":"custom_tool_call","name":"exec","call_id":"call_fixture","input":"text(1);"},{"type":"custom_tool_call_output","call_id":"call_fixture","output":[{"type":"input_text","text":"1"}]}]},"out":{"model":"fixture","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"text(1);\"}","name":"functions__exec"},"id":"call_fixture","type":"function"}]},{"role":"tool","tool_call_id":"call_fixture","content":"1"}],"stream":false,"tools":[{"function":{"description":"","name":"functions__exec","parameters":{"properties":{"input":{"type":"string"}},"required":["input"],"type":"object"}},"type":"function"},{"function":{"description":"","name":"functions__wait","parameters":{}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MergeConsecutiveFunctionCalls","k":"req","m":"kimi-k2.6","s":true,"in":{"input":[{"type":"function_call","call_id":"exec_command:0","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"},{"type":"function_call","call_id":"exec_command:1","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"},{"type":"function_call_output","call_id":"exec_command:0","output":"ok0"},{"type":"function_call_output","call_id":"exec_command:1","output":"ok1"}]},"out":{"model":"kimi-k2.6","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"cmd\":\"ls\"}","name":"exec_command"},"id":"exec_command:0","type":"function"},{"function":{"arguments":"{\"cmd\":\"pwd\"}","name":"exec_command"},"id":"exec_command:1","type":"function"}]},{"role":"tool","tool_call_id":"exec_command:0","content":"ok0"},{"role":"tool","tool_call_id":"exec_command:1","content":"ok1"}],"stream":true}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SplitFunctionCallsWhenInterrupted","k":"req","m":"kimi-k2.6","s":false,"in":{"input":[{"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},{"type":"message","role":"user","content":"next"},{"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"}]},"out":{"model":"kimi-k2.6","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_a","type":"function"}]},{"role":"user","content":"next"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_b"},"id":"call_b","type":"function"}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DefersMessageUntilToolOutput","k":"req","m":"kimi-k2.6","s":true,"in":{"input":[{"type":"function_call","call_id":"call_x","name":"exec_command","arguments":"{\"cmd\":\"echo hi\"}"},{"type":"message","role":"user","content":"Approved command prefix saved"},{"type":"function_call_output","call_id":"call_x","output":"ok"},{"type":"message","role":"user","content":"next"}]},"out":{"model":"kimi-k2.6","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"cmd\":\"echo hi\"}","name":"exec_command"},"id":"call_x","type":"function"}]},{"role":"tool","tool_call_id":"call_x","content":"ok"},{"role":"user","content":"Approved command prefix saved"},{"role":"user","content":"next"}],"stream":true}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnwrapsStringifiedToolOutputImages","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_image","name":"view_image","arguments":"{}"},{"type":"function_call_output","call_id":"call_image","output":"[{\"type\":\"input_text\",\"text\":\"Captured screenshot.\"},{\"detail\":\"original\",\"image_url\":\"data:image/png;base64,AA==\",\"type\":\"input_image\"}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"view_image"},"id":"call_image","type":"function"}]},{"role":"tool","tool_call_id":"call_image","content":[{"type":"text","text":"Captured screenshot."},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"high"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnwrapsStringifiedToolOutputImages","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_image","name":"view_image","arguments":"{}"},{"type":"function_call_output","call_id":"call_image","output":"[{\"type\":\"image_url\",\"image_url\":{\"url\":\"https://example.com/generated.png\",\"detail\":\"high\"}}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"view_image"},"id":"call_image","type":"function"}]},{"role":"tool","tool_call_id":"call_image","content":[{"type":"image_url","image_url":{"url":"https://example.com/generated.png","detail":"high"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnwrapsStringifiedCustomToolOutputImages","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"custom_tool_call","call_id":"call_image","name":"view_image","input":"{}"},{"type":"custom_tool_call_output","call_id":"call_image","output":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\",\"detail\":\"original\"}]"}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"{}\"}","name":"view_image"},"id":"call_image","type":"function"}]},{"role":"tool","tool_call_id":"call_image","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"high"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesCustomToolOutputFallbacks","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"custom_tool_call","call_id":"call_output","name":"inspect","input":"{}"},{"type":"custom_tool_call_output","call_id":"call_output","output":"plain output"}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"{}\"}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"plain output"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesCustomToolOutputFallbacks","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"custom_tool_call","call_id":"call_output","name":"inspect","input":"{}"},{"type":"custom_tool_call_output","call_id":"call_output","output":[{"type":"input_text","text":"done"}]}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"{}\"}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"done"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesCustomToolOutputFallbacks","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"custom_tool_call","call_id":"call_output","name":"inspect","input":"{}"},{"type":"custom_tool_call_output","call_id":"call_output","output":[{"type":"input_image","detail":"low"}]}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"{}\"}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":""}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsStructuredToolOutputImages","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_image","name":"view_image","arguments":"{}"},{"type":"function_call_output","call_id":"call_image","output":[{"type":"input_text","text":"Captured screenshot."},{"type":"input_image","image_url":"data:image/png;base64,AA==","detail":"original"}]}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"view_image"},"id":"call_image","type":"function"}]},{"role":"tool","tool_call_id":"call_image","content":[{"type":"text","text":"Captured screenshot."},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"high"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"plain output"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"plain output"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"{\"status\":\"ok\"}"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"{\"status\":\"ok\"}"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_text\",\"text\":\"still text\"}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_text\",\"text\":\"still text\"}]"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_image\",\"detail\":\"low\"}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_image\",\"detail\":\"low\"}]"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\"}] trailing"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\"}] trailing"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\"}"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\"}"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_image\",\"image_url\":123}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_image\",\"image_url\":123}]"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\",\"detail\":123}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\",\"detail\":123}]"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsNonImageToolOutputStrings","k":"req","m":"k3","s":false,"in":{"input":[{"type":"function_call","call_id":"call_output","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_output","output":"[{\"type\":\"input_text\",\"text\":123},{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\"}]"}]},"out":{"model":"k3","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"inspect"},"id":"call_output","type":"function"}]},{"role":"tool","tool_call_id":"call_output","content":"[{\"type\":\"input_text\",\"text\":123},{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA==\"}]"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_AttachesReasoningToAssistantMessage","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"first line\n"},{"type":"summary_text","text":"second line"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"message","role":"user","content":"next"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","content":[{"type":"text","text":"answer"}],"reasoning_content":"first line\nsecond line"},{"role":"user","content":"next"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesAssistantContentWithToolCalls","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"inspect the next step"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Step 3 completed; continue to step 4."}]},{"type":"function_call","call_id":"call_4","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"},{"type":"function_call_output","call_id":"call_4","output":"ok"}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","content":[{"type":"text","text":"Step 3 completed; continue to step 4."}],"reasoning_content":"inspect the next step","tool_calls":[{"function":{"arguments":"{\"cmd\":\"pwd\"}","name":"exec_command"},"id":"call_4","type":"function"}]},{"role":"tool","tool_call_id":"call_4","content":"ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DoesNotMergeToolCallsAcrossUserMessage","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]},{"type":"function_call","call_id":"call_next","name":"exec_command","arguments":"{}"},{"type":"function_call_output","call_id":"call_next","output":"ok"}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","content":[{"type":"text","text":"done"}]},{"role":"user","content":[{"type":"text","text":"next"}]},{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"exec_command"},"id":"call_next","type":"function"}]},{"role":"tool","tool_call_id":"call_next","content":"ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MergesDistinctReasoningWithinAssistantTurn","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"first"}]},{"type":"message","role":"assistant","reasoning_content":"first","content":[{"type":"output_text","text":"working"}]},{"type":"reasoning","summary":[{"type":"summary_text","text":"second"}]},{"type":"function_call","call_id":"call_reasoning","name":"exec_command","arguments":"{}"}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","content":[{"type":"text","text":"working"}],"reasoning_content":"first\n\nsecond","tool_calls":[{"function":{"arguments":"{}","name":"exec_command"},"id":"call_reasoning","type":"function"}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ReplacesUnavailableReasoningWithinAssistantTurn","k":"req","m":"kimi-k3","s":false,"in":{"input":[{"type":"reasoning","summary":[]},{"type":"message","role":"assistant","reasoning_content":"real reasoning","content":[{"type":"output_text","text":"working"}]},{"type":"function_call","call_id":"call_real_reasoning","name":"exec_command","arguments":"{}"}]},"out":{"model":"kimi-k3","messages":[{"role":"assistant","content":[{"type":"text","text":"working"}],"reasoning_content":"real reasoning","tool_calls":[{"function":{"arguments":"{}","name":"exec_command"},"id":"call_real_reasoning","type":"function"}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_AttachesReasoningToToolCallMessage","k":"req","m":"deepseek-v4-flash","s":true,"in":{"input":[{"type":"reasoning","id":"rs_tool","summary":[{"type":"summary_text","text":"tool reasoning"}]},{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"cmd\":\"pwd\"}","name":"exec_command"},"id":"call_1","type":"function"}],"reasoning_content":"tool reasoning"},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":true}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsReasoningBeforeUserMessage","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"type":"reasoning","id":"rs_empty","summary":[]},{"type":"message","role":"user","content":"continue"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","content":"","reasoning_content":"[reasoning unavailable]"},{"role":"user","content":"continue"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesReasoningOnFollowUpToolTurns","k":"req","m":"deepseek-v4.1-flash","s":true,"in":{"model":"deepseek-v4.1-flash","reasoning":{"effort":"high"},"input":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"first plan"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"starting"}]},{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"},{"type":"function_call_output","call_id":"call_1","output":"ok"},{"type":"function_call","call_id":"call_2","name":"write_stdin","arguments":"{\"data\":\"x\"}"},{"type":"function_call_output","call_id":"call_2","output":"ok"},{"type":"reasoning","id":"rs_2","summary":[{"type":"summary_text","text":"second plan"}]},{"type":"function_call","call_id":"call_3","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"},{"type":"function_call_output","call_id":"call_3","output":"ok"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"apply_patch is available"}]},{"type":"function_call","call_id":"call_4","name":"exec_command","arguments":"{\"cmd\":\"cat\"}"},{"type":"function_call_output","call_id":"call_4","output":"ok"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","content":[{"type":"text","text":"starting"}],"reasoning_content":"first plan","tool_calls":[{"function":{"arguments":"{\"cmd\":\"ls\"}","name":"exec_command"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"data\":\"x\"}","name":"write_stdin"},"id":"call_2","type":"function"}],"reasoning_content":"first plan"},{"role":"tool","tool_call_id":"call_2","content":"ok"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"cmd\":\"pwd\"}","name":"exec_command"},"id":"call_3","type":"function"}],"reasoning_content":"second plan"},{"role":"tool","tool_call_id":"call_3","content":"ok"},{"role":"assistant","content":[{"type":"text","text":"apply_patch is available"}],"tool_calls":[{"function":{"arguments":"{\"cmd\":\"cat\"}","name":"exec_command"},"id":"call_4","type":"function"}],"reasoning_content":"second plan"},{"role":"tool","tool_call_id":"call_4","content":"ok"}],"stream":true,"reasoning_effort":"high"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FallsBackToPlaceholderWhenNoPriorReasoning","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","reasoning":{"effort":"high"},"input":[{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"cmd\":\"ls\"}","name":"exec_command"},"id":"call_1","type":"function"}],"reasoning_content":"[reasoning unavailable]"},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"reasoning_effort":"high"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_EffortNoneDoesNotInjectReasoning","k":"req","m":"gpt-4o","s":false,"in":{"model":"gpt-4o","reasoning":{"effort":"none"},"input":[{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}]},"out":{"model":"gpt-4o","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"exec_command"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"reasoning_effort":"none"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesReasoningOnCustomToolCallTurns","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","reasoning":{"effort":"high"},"input":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"custom plan"}]},{"type":"custom_tool_call","call_id":"cust_1","name":"do_work","input":"step1"},{"type":"custom_tool_call_output","call_id":"cust_1","output":"done"},{"type":"custom_tool_call","call_id":"cust_2","name":"do_work","input":"step2"},{"type":"custom_tool_call_output","call_id":"cust_2","output":"done"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"step1\"}","name":"do_work"},"id":"cust_1","type":"function"}],"reasoning_content":"custom plan"},{"role":"tool","tool_call_id":"cust_1","content":"done"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"step2\"}","name":"do_work"},"id":"cust_2","type":"function"}],"reasoning_content":"custom plan"},{"role":"tool","tool_call_id":"cust_2","content":"done"}],"stream":false,"reasoning_effort":"high"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ResetsReasoningAcrossUserMessageBoundary","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","reasoning":{"effort":"high"},"input":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"turn 1 plan"}]},{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"},{"type":"message","role":"user","content":"now do step 2"},{"type":"function_call","call_id":"call_2","name":"exec_command","arguments":"{}"},{"type":"function_call_output","call_id":"call_2","output":"ok"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"exec_command"},"id":"call_1","type":"function"}],"reasoning_content":"turn 1 plan"},{"role":"tool","tool_call_id":"call_1","content":"ok"},{"role":"user","content":"now do step 2"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"exec_command"},"id":"call_2","type":"function"}],"reasoning_content":"[reasoning unavailable]"},{"role":"tool","tool_call_id":"call_2","content":"ok"}],"stream":false,"reasoning_effort":"high"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FlattensNamespaceTools","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"role":"user","content":"Use add_numbers."}],"tools":[{"type":"namespace","name":"mcp__test_mcp__","description":"Tools in the mcp__test_mcp__ namespace.","tools":[{"type":"function","name":"add_numbers","description":"Add two numbers","parameters":{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]}}]}],"tool_choice":"auto"},"out":{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"Use add_numbers."}],"stream":false,"tools":[{"function":{"description":"Add two numbers","name":"mcp__test_mcp__add_numbers","parameters":{"properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"],"type":"object"}},"type":"function"}],"tool_choice":"auto"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiesNamespaceFunctionCallHistory","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"type":"function_call","call_id":"call_get_me","name":"get_me","namespace":"mcp__github","arguments":"{}"},{"type":"function_call_output","call_id":"call_get_me","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__github","tools":[{"type":"function","name":"get_me","parameters":{"type":"object"}}]}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"mcp__github__get_me"},"id":"call_get_me","type":"function"}]},{"role":"tool","tool_call_id":"call_get_me","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"mcp__github__get_me","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FlattensNamespaceCustomTools","k":"req","m":"gpt-5.4","s":false,"in":{"tools":[{"type":"namespace","name":"terminal","tools":[{"type":"custom","name":"exec","description":"Run a command"}]}]},"out":{"model":"gpt-5.4","messages":[],"stream":false,"tools":[{"function":{"description":"Run a command","name":"terminal__exec","parameters":{"properties":{"input":{"type":"string"}},"required":["input"],"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FlattensNamespaceCustomTools","k":"req","m":"gpt-5.4","s":false,"in":{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"terminal","tools":[{"type":"custom","name":"exec","description":"Run a command"}]}]}]},"out":{"model":"gpt-5.4","messages":[],"stream":false,"tools":[{"function":{"description":"Run a command","name":"terminal__exec","parameters":{"properties":{"input":{"type":"string"}},"required":["input"],"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesStructuredToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"input":[{"role":"user","content":"Run command."}],"tools":[{"type":"function","name":"run_command","parameters":{"type":"object"}}],"tool_choice":{"type":"function","function":{"name":"run_command"}}},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"Run command."}],"stream":false,"tools":[{"function":{"description":"","name":"run_command","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"run_command"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsCanonicalResponsesNamedToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":[{"role":"user","content":"Call gateway_echo with value TOOL_OK."}],"tools":[{"type":"function","name":"gateway_echo","description":"Returns the given value","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}],"tool_choice":{"type":"function","name":"gateway_echo"},"max_output_tokens":512},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"Call gateway_echo with value TOOL_OK."}],"stream":false,"max_tokens":512,"tools":[{"function":{"description":"Returns the given value","name":"gateway_echo","parameters":{"additionalProperties":false,"properties":{"value":{"type":"string"}},"required":["value"],"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"gateway_echo"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"test","tools":[{"type":"namespace","name":"service_tools","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}],"tool_choice":{"type":"function","name":"lookup"}},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"test"}],"stream":false,"tools":[{"function":{"description":"","name":"service_tools__lookup","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"service_tools__lookup"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"test","tools":[{"type":"namespace","name":"service_tools","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}],"tool_choice":{"type":"function","name":"lookup","namespace":"service_tools"}},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"test"}],"stream":false,"tools":[{"function":{"description":"","name":"service_tools__lookup","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"service_tools__lookup"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"test","tools":[{"type":"custom","name":"patch_runner","description":"Applies diff"}],"tool_choice":{"type":"custom","name":"patch_runner"}},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"test"}],"stream":false,"tools":[{"function":{"description":"Applies diff","name":"patch_runner","parameters":{"properties":{"input":{"type":"string"}},"required":["input"],"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"patch_runner"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"test","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],"tool_choice":"auto"},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"test"}],"stream":false,"tools":[{"function":{"description":"","name":"lookup","parameters":{"type":"object"}},"type":"function"}],"tool_choice":"auto"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"test","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],"tool_choice":"none"},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"test"}],"stream":false,"tools":[{"function":{"description":"","name":"lookup","parameters":{"type":"object"}},"type":"function"}],"tool_choice":"none"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ConvertsNamespaceAndCustomToolChoice","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"test","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],"tool_choice":"required"},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"test"}],"stream":false,"tools":[{"function":{"description":"","name":"lookup","parameters":{"type":"object"}},"type":"function"}],"tool_choice":"required"}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OmitsToolSettingsWithoutTools","k":"req","m":"grok-4.5","s":false,"in":{"input":[{"role":"user","content":"say ok"}],"tools":[],"tool_choice":"auto","parallel_tool_calls":false},"out":{"model":"grok-4.5","messages":[{"role":"user","content":"say ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OmitsToolSettingsWithoutTools","k":"req","m":"grok-4.5","s":false,"in":{"tools":[{"type":"unsupported"}],"tool_choice":"auto","parallel_tool_calls":false},"out":{"model":"grok-4.5","messages":[],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesParallelToolCallsWithTools","k":"req","m":"grok-4.5","s":false,"in":{"tools":[{"type":"function","name":"run_command","parameters":{"type":"object"}}],"parallel_tool_calls":false},"out":{"model":"grok-4.5","messages":[],"stream":false,"tools":[{"function":{"description":"","name":"run_command","parameters":{"type":"object"}},"type":"function"}],"parallel_tool_calls":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesJSONSchemaTextFormat","k":"req","m":"deepseek-v4-flash","s":false,"in":{"text":{"format":{"type":"json_schema","name":"answer","description":"Structured answer","strict":true,"schema":{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}}}},"out":{"model":"deepseek-v4-flash","messages":[],"stream":false,"response_format":{"type":"json_schema","json_schema":{"name":"answer","description":"Structured answer","strict":true,"schema":{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesJSONObjectTextFormat","k":"req","m":"deepseek-v4-flash","s":false,"in":{"text":{"format":{"type":"json_object"}}},"out":{"model":"deepseek-v4-flash","messages":[],"stream":false,"response_format":{"type":"json_object"}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OmitsResponseFormatWithoutTextFormat","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":"Return plain text."},"out":{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"Return plain text."}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_NormalizesInputImageDetail","k":"req","m":"gpt-5.4","s":false,"in":{"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png","detail":"high"}]}]},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png","detail":"high"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_NormalizesInputImageDetail","k":"req","m":"gpt-5.4","s":false,"in":{"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png","detail":"original"}]}]},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png","detail":"high"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_NormalizesInputImageDetail","k":"req","m":"gpt-5.4","s":false,"in":{"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png","detail":"medium"}]}]},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_NormalizesInputImageDetail","k":"req","m":"gpt-5.4","s":false,"in":{"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png","detail":123}]}]},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DeduplicatesToolsAcrossAdditionalTools","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"role":"user","content":"What time is it?"},{"type":"additional_tools","tools":[{"type":"function","name":"get_time","description":"copy from additional_tools","parameters":{"type":"object","properties":{"tz":{"type":"string"}}}}]}],"tools":[{"type":"function","name":"get_time","description":"authoritative top-level definition","parameters":{"type":"object","properties":{"timezone":{"type":"string"}}}}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"What time is it?"}],"stream":false,"tools":[{"function":{"description":"authoritative top-level definition","name":"get_time","parameters":{"properties":{"timezone":{"type":"string"}},"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DeduplicatesNamespaceQualifiedCollision","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"role":"user","content":"Patch the file."}],"tools":[{"type":"function","name":"editor__apply_patch","parameters":{"type":"object"}},{"type":"namespace","name":"editor","tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object"}}]}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"Patch the file."}],"stream":false,"tools":[{"function":{"description":"","name":"editor__apply_patch","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsDistinctToolsFromBothSources","k":"req","m":"deepseek-v4-flash","s":false,"in":{"input":[{"role":"user","content":"Do the thing."},{"type":"additional_tools","tools":[{"type":"function","name":"get_date","parameters":{"type":"object"}},{"type":"function","name":"get_time","parameters":{"type":"object"}}]}],"tools":[{"type":"function","name":"get_time","parameters":{"type":"object"}},{"type":"function","name":"get_weather","parameters":{"type":"object"}}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"Do the thing."}],"stream":false,"tools":[{"function":{"description":"","name":"get_time","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"get_weather","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"get_date","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FunctionCallOutputAlternateIDsAndQueueFallback","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{\"command\":\"ls\"}"},{"type":"function_call_output","output":"tool_result_ok","call_id":"call_123"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"ls\"}","name":"Bash"},"id":"call_123","type":"function"}]},{"role":"tool","tool_call_id":"call_123","content":"tool_result_ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FunctionCallOutputAlternateIDsAndQueueFallback","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{\"command\":\"ls\"}"},{"type":"function_call_output","output":"tool_result_ok","tool_call_id":"call_123"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"ls\"}","name":"Bash"},"id":"call_123","type":"function"}]},{"role":"tool","tool_call_id":"call_123","content":"tool_result_ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FunctionCallOutputAlternateIDsAndQueueFallback","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{\"command\":\"ls\"}"},{"type":"function_call_output","output":"tool_result_ok","callId":"call_123"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"ls\"}","name":"Bash"},"id":"call_123","type":"function"}]},{"role":"tool","tool_call_id":"call_123","content":"tool_result_ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FunctionCallOutputAlternateIDsAndQueueFallback","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{\"command\":\"ls\"}"},{"type":"function_call_output","output":"tool_result_ok","id":"call_123"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"ls\"}","name":"Bash"},"id":"call_123","type":"function"}]},{"role":"tool","tool_call_id":"call_123","content":"tool_result_ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_FunctionCallOutputAlternateIDsAndQueueFallback","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{\"command\":\"ls\"}"},{"type":"function_call_output","output":"tool_result_ok"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"ls\"}","name":"Bash"},"id":"call_123","type":"function"}]},{"role":"tool","tool_call_id":"call_123","content":"tool_result_ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MixedMissingAndExplicitParallelOutputs","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},{"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},{"type":"function_call_output","output":"result_b"},{"type":"function_call_output","call_id":"call_a","output":"result_a"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_a","type":"function"},{"function":{"arguments":"{}","name":"tool_b"},"id":"call_b","type":"function"}]},{"role":"tool","tool_call_id":"call_b","content":"result_b"},{"role":"tool","tool_call_id":"call_a","content":"result_a"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DefersMessageUntilMissingIDToolOutput","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},{"type":"message","role":"user","content":"User command while running"},{"type":"function_call_output","output":"result_a"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_a","type":"function"}]},{"role":"tool","tool_call_id":"call_a","content":"result_a"},{"role":"user","content":"User command while running"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MixedMissingAndExplicitParallelOutputsAcrossUserMessage","k":"req","m":"deepseek-v4-flash","s":false,"in":{"model":"deepseek-v4-flash","input":[{"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},{"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},{"type":"function_call_output","output":"result_b"},{"type":"message","role":"user","content":"status?"},{"type":"function_call_output","call_id":"call_a","output":"result_a"}]},"out":{"model":"deepseek-v4-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_a","type":"function"},{"function":{"arguments":"{}","name":"tool_b"},"id":"call_b","type":"function"}]},{"role":"tool","tool_call_id":"call_b","content":"result_b"},{"role":"tool","tool_call_id":"call_a","content":"result_a"},{"role":"user","content":"status?"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_OrphanFunctionCallOutputBecomesUserMessage","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"role":"user","content":[{"type":"input_text","text":"Task initialization"}]},{"type":"function_call_output","id":"fco_01a09fca-8d33-73a1-97fd-4d83ecc02f9d","name":"send_message_to_thread","output":"<codex_delegation>\n  <source_thread_id>01a022d7-d4d0-72b2-8571-4590484ccaee</source_thread_id>\n  <input>Execute sub-task</input>\n</codex_delegation>"},{"type":"function_call","call_id":"call_1789387253098037589_85","name":"Bash","arguments":"{\"command\":\"pwd\"}"},{"type":"function_call_output","call_id":"call_1789387253098037589_85","id":"fco_01a09fca-a5f0-7b40-9943-21fbc923c537","output":"/Users/developer"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":[{"type":"text","text":"Task initialization"}]},{"role":"user","content":"<codex_delegation>\n  <source_thread_id>01a022d7-d4d0-72b2-8571-4590484ccaee</source_thread_id>\n  <input>Execute sub-task</input>\n</codex_delegation>"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"pwd\"}","name":"Bash"},"id":"call_1789387253098037589_85","type":"function"}]},{"role":"tool","tool_call_id":"call_1789387253098037589_85","content":"/Users/developer"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnpairedExplicitCallIDBecomesUserMessage","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"role":"user","content":[{"type":"input_text","text":"Task initialization"}]},{"type":"function_call_output","call_id":"call_missing","name":"send_message_to_thread","output":"<codex_delegation>Execute sub-task</codex_delegation>"},{"type":"function_call","call_id":"call_1789387253098037589_85","name":"Bash","arguments":"{\"command\":\"pwd\"}"},{"type":"function_call_output","call_id":"call_1789387253098037589_85","output":"/Users/developer"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":[{"type":"text","text":"Task initialization"}]},{"role":"user","content":"<codex_delegation>Execute sub-task</codex_delegation>"},{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"command\":\"pwd\"}","name":"Bash"},"id":"call_1789387253098037589_85","type":"function"}]},{"role":"tool","tool_call_id":"call_1789387253098037589_85","content":"/Users/developer"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_CapsLongNamespaceToolNames","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object"}},{"type":"namespace","name":"mcp__codex_apps__codex_document_control","tools":[{"type":"function","name":"_execute_document_command","parameters":{"type":"object"}},{"type":"function","name":"_get_document_tool_schemas","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__codex_apps__safety_settings","tools":[{"type":"function","name":"_prepare_parental_control_update","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"exec_command","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"p__codex_apps__codex_document_control___execute_document_command","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"codex_apps__codex_document_control___get_document_tool_schemas","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"p__codex_apps__safety_settings___prepare_parental_control_update","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DisambiguatesTruncationCollisions","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"mcp__server_one__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tools":[{"type":"function","name":"_same_tail_tool_name","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__server_two__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tools":[{"type":"function","name":"_same_tail_tool_name","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa___same_tail_tool_name","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa___same_tail_tool_name_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DisambiguatesTruncationCollisions","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"custom_tool_call","namespace":"mcp__server_two__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","name":"_same_tail_tool_name","call_id":"call_1","input":"x"},{"type":"custom_tool_call_output","call_id":"call_1","output":"y"}],"tools":[{"type":"namespace","name":"mcp__server_one__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tools":[{"type":"function","name":"_same_tail_tool_name","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__server_two__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tools":[{"type":"function","name":"_same_tail_tool_name","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"x\"}","name":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa___same_tail_tool_name_1"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"y"}],"stream":false,"tools":[{"function":{"description":"","name":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa___same_tail_tool_name","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa___same_tail_tool_name_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongDeclarationDoesNotDisplaceShortOriginal","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"mcp__a__bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","tools":[{"type":"function","name":"child_tool","parameters":{"type":"object"}}]},{"type":"function","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongDeclarationDoesNotDisplaceShortOriginal","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__a__bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","tools":[{"type":"function","name":"child_tool","parameters":{"type":"object"}}]},{"type":"function","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongDeclarationDoesNotDisplaceShortOriginal","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"mcp__a__bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","tools":[{"type":"function","name":"child_tool","parameters":{"type":"object"}}]},{"type":"function","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}}],"tool_choice":{"type":"function","function":{"name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool"}}},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongDeclarationDoesNotDisplaceShortOriginal","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_2","name":"mcp__a__bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","arguments":"{}"},{"type":"function_call_output","call_id":"call_2","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__a__bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","tools":[{"type":"function","name":"child_tool","parameters":{"type":"object"}}]},{"type":"function","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool_1"},"id":"call_2","type":"function"}]},{"role":"tool","tool_call_id":"call_2","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb__child_tool","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_AmbiguousLongLocalNameStaysUnresolved","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","name":"shared_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"shared_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"shared_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"d_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx_2"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"red_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"d_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongAliasDoesNotDisplaceNamespacedLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"function","name":"nnnnnnnnnnnlmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongAliasDoesNotDisplaceNamespacedLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"function","name":"nnnnnnnnnnnlmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_LongAliasDoesNotDisplaceNamespacedLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"function","name":"nnnnnnnnnnnlmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}}]}],"tool_choice":{"type":"function","function":{"name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm"}}},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"lmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","namespace":"mcp__alpha","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","namespace":"mcp__beta","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]}],"tool_choice":{"type":"function","function":{"name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt"}}},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_SharedLocalNameIsNeverEmitted","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"mcp__alpha","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]},{"type":"namespace","name":"mcp__beta","tools":[{"type":"function","name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt","parameters":{"type":"object"}}]}],"tool_choice":{"type":"function","namespace":"mcp__beta","function":{"name":"sttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt"}}},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_1","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt_2"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiedIdentityOutranksForeignLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"alpha_ns","tools":[{"type":"function","name":"read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]},{"type":"namespace","name":"beta_ns","tools":[{"type":"function","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"d_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiedIdentityOutranksForeignLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"alpha_ns","tools":[{"type":"function","name":"read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]},{"type":"namespace","name":"beta_ns","tools":[{"type":"function","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"d_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiedIdentityOutranksForeignLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"namespace","name":"alpha_ns","tools":[{"type":"function","name":"read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]},{"type":"namespace","name":"beta_ns","tools":[{"type":"function","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]}],"tool_choice":{"type":"function","function":{"name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}}},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"d_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_1","parameters":{"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}}}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiedIdentityOutranksForeignLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","namespace":"alpha_ns","name":"read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"alpha_ns","tools":[{"type":"function","name":"read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]},{"type":"namespace","name":"beta_ns","tools":[{"type":"function","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"d_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_QualifiedIdentityOutranksForeignLocalName","k":"req","m":"z-ai/glm-5.3-free","s":false,"in":{"input":[{"type":"function_call","call_id":"call_1","namespace":"beta_ns","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}],"tools":[{"type":"namespace","name":"alpha_ns","tools":[{"type":"function","name":"read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]},{"type":"namespace","name":"beta_ns","tools":[{"type":"function","name":"alpha_ns__read_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}}]}]},"out":{"model":"z-ai/glm-5.3-free","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"d_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_1"},"id":"call_1","type":"function"}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}],"stream":false,"tools":[{"function":{"description":"","name":"ead_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","parameters":{"type":"object"}},"type":"function"},{"function":{"description":"","name":"d_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_1","parameters":{"type":"object"}},"type":"function"}]}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_IncompleteToolCallsDoNotDeferMessages","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},{"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},{"role":"user","content":"reminder before results"},{"type":"function_call_output","call_id":"call_a","output":"result_a"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_a","type":"function"},{"function":{"arguments":"{}","name":"tool_b"},"id":"call_b","type":"function"}]},{"role":"user","content":"reminder before results"},{"role":"tool","tool_call_id":"call_a","content":"result_a"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_CompleteToolCallsDoPairMessages","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},{"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},{"role":"user","content":"reminder during execution"},{"type":"function_call_output","call_id":"call_b","output":"result_b"},{"type":"function_call_output","call_id":"call_a","output":"result_a"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_a","type":"function"},{"function":{"arguments":"{}","name":"tool_b"},"id":"call_b","type":"function"}]},{"role":"tool","tool_call_id":"call_b","content":"result_b"},{"role":"tool","tool_call_id":"call_a","content":"result_a"},{"role":"user","content":"reminder during execution"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MixedEmptyIDDoesNotReorder","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"","name":"unknown","arguments":"{}"},{"type":"function_call","call_id":"a","name":"known","arguments":"{}"},{"role":"user","content":"reminder"},{"type":"function_call_output","call_id":"a","output":"ok"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"unknown"},"id":"","type":"function"},{"function":{"arguments":"{}","name":"known"},"id":"a","type":"function"}]},{"role":"user","content":"reminder"},{"role":"tool","tool_call_id":"a","content":"ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DuplicateCallIDDoesNotReorder","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"dup","name":"tool_1","arguments":"{}"},{"type":"function_call","call_id":"dup","name":"tool_2","arguments":"{}"},{"role":"user","content":"reminder"},{"type":"function_call_output","call_id":"dup","output":"ok"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_1"},"id":"dup","type":"function"},{"function":{"arguments":"{}","name":"tool_2"},"id":"dup","type":"function"}]},{"role":"user","content":"reminder"},{"role":"tool","tool_call_id":"dup","content":"ok"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DuplicateOutputCallIDDoesNotReorder","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"call_dup_out","name":"tool_a","arguments":"{}"},{"role":"user","content":"reminder before results"},{"type":"function_call_output","call_id":"call_dup_out","output":"first"},{"type":"function_call_output","call_id":"call_dup_out","output":"second"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"call_dup_out","type":"function"}]},{"role":"user","content":"reminder before results"},{"role":"tool","tool_call_id":"call_dup_out","content":"first"},{"role":"user","content":"second"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_DuplicateCustomOutputCallIDDoesNotReorder","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"custom_tool_call","call_id":"custom_dup","name":"custom_a","input":"{}"},{"role":"user","content":"reminder before custom results"},{"type":"custom_tool_call_output","call_id":"custom_dup","output":"output 1"},{"type":"custom_tool_call_output","call_id":"custom_dup","output":"output 2"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"input\":\"{}\"}","name":"custom_a"},"id":"custom_dup","type":"function"}]},{"role":"user","content":"reminder before custom results"},{"role":"tool","tool_call_id":"custom_dup","content":"output 1"},{"role":"user","content":"output 2"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MultipleOutputsWithoutIDDoNotGuessOrReorder","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"a","name":"unknown_a","arguments":"{}"},{"type":"function_call","call_id":"b","name":"unknown_b","arguments":"{}"},{"role":"user","content":"reminder before results"},{"type":"function_call_output","output":"output X"},{"type":"function_call_output","output":"output Y"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"unknown_a"},"id":"a","type":"function"},{"function":{"arguments":"{}","name":"unknown_b"},"id":"b","type":"function"}]},{"role":"user","content":"reminder before results"},{"role":"user","content":"output X"},{"role":"user","content":"output Y"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MultipleOutputsWithoutIDAndOrphanOutputDoNotGuessOrReorder","k":"req","m":"deepseek-v4.1-flash","s":false,"in":{"model":"deepseek-v4.1-flash","input":[{"type":"function_call","call_id":"a","name":"tool_a","arguments":"{}"},{"type":"function_call","call_id":"b","name":"tool_b","arguments":"{}"},{"role":"user","content":"reminder before results"},{"type":"function_call_output","output":"output X"},{"type":"function_call_output","output":"output Y"},{"type":"function_call_output","call_id":"orphan_id","output":"output Z"}]},"out":{"model":"deepseek-v4.1-flash","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"tool_a"},"id":"a","type":"function"},{"function":{"arguments":"{}","name":"tool_b"},"id":"b","type":"function"}]},{"role":"user","content":"reminder before results"},{"role":"user","content":"output X"},{"role":"user","content":"output Y"},{"role":"user","content":"output Z"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MapsMaxOutputTokensToMaxTokens","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"hello","max_output_tokens":1024},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"stream":false,"max_tokens":1024}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MapsMaxOutputTokensToMaxTokens","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"hello"},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"stream":false}}
{"t":"TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MapsMaxOutputTokensToMaxTokens","k":"req","m":"gpt-5.4","s":false,"in":{"model":"gpt-5.4","input":"hello","max_output_tokens":null},"out":{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"stream":false,"max_tokens":null}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_MultipleToolCallsRemainSeparate","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_read","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_test","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_test","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_read","type":"function_call","status":"in_progress","arguments":"","call_id":"call_read","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\",\"limit\":400,\"offset\":1}"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_read","output_index":0,"delta":"{\"filePath\":\"C:\\\\repo\",\"limit\":400,\"offset\":1}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":1,"id":"call_glob","type":"function","function":{"name":"glob","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":{"id":"fc_call_glob","type":"function_call","status":"in_progress","arguments":"","call_id":"call_glob","name":"glob"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.{yml,yaml}\"}"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":6,"item_id":"fc_call_glob","output_index":1,"delta":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.{yml,yaml}\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":7,"item_id":"fc_call_read","output_index":0,"arguments":"{\"filePath\":\"C:\\\\repo\",\"limit\":400,\"offset\":1}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"fc_call_read","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\",\"limit\":400,\"offset\":1}","call_id":"call_read","name":"read"}},"event":"response.output_item.done"},{"data":{"type":"response.function_call_arguments.done","sequence_number":9,"item_id":"fc_call_glob","output_index":1,"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.{yml,yaml}\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":10,"output_index":1,"item":{"id":"fc_call_glob","type":"function_call","status":"completed","arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.{yml,yaml}\"}","call_id":"call_glob","name":"glob"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":11,"response":{"id":"resp_test","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"fc_call_read","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\",\"limit\":400,\"offset\":1}","call_id":"call_read","name":"read"},{"id":"fc_call_glob","type":"function_call","status":"completed","arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.{yml,yaml}\"}","call_id":"call_glob","name":"glob"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":20}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_MultiChoiceToolCallsUseDistinctOutputIndexes","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_multi_choice","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_choice0","type":"function","function":{"name":"glob","arguments":""}}]},"finish_reason":null},{"index":1,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_choice1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_multi_choice","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_multi_choice","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_choice0","type":"function_call","status":"in_progress","arguments":"","call_id":"call_choice0","name":"glob"}},"event":"response.output_item.added"},{"data":{"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"id":"fc_call_choice1","type":"function_call","status":"in_progress","arguments":"","call_id":"call_choice1","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_multi_choice","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"}}]},"finish_reason":null},{"index":1,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":5,"item_id":"fc_call_choice0","output_index":0,"delta":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":6,"item_id":"fc_call_choice1","output_index":1,"delta":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"resp_multi_choice","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"},{"index":1,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":7,"item_id":"fc_call_choice0","output_index":0,"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"fc_call_choice0","type":"function_call","status":"completed","arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}","call_id":"call_choice0","name":"glob"}},"event":"response.output_item.done"},{"data":{"type":"response.function_call_arguments.done","sequence_number":9,"item_id":"fc_call_choice1","output_index":1,"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":10,"output_index":1,"item":{"id":"fc_call_choice1","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}","call_id":"call_choice1","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":11,"response":{"id":"resp_multi_choice","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"fc_call_choice0","type":"function_call","status":"completed","arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}","call_id":"call_choice0","name":"glob"},{"id":"fc_call_choice1","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}","call_id":"call_choice1","name":"read"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":20}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_MixedMessageAndToolUseDistinctOutputIndexes","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_mixed","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello","reasoning_content":null,"tool_calls":null},"finish_reason":null},{"index":1,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_choice1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_mixed","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_mixed","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_mixed_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_mixed_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_mixed_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"},{"data":{"type":"response.output_item.added","sequence_number":6,"output_index":1,"item":{"id":"fc_call_choice1","type":"function_call","status":"in_progress","arguments":"","call_id":"call_choice1","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_mixed","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"stop"},{"index":1,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}},"out":[{"data":{"type":"response.output_text.done","sequence_number":7,"item_id":"msg_resp_mixed_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":8,"item_id":"msg_resp_mixed_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":9,"output_index":0,"item":{"id":"msg_resp_mixed_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.function_call_arguments.done","sequence_number":10,"item_id":"fc_call_choice1","output_index":1,"arguments":"{}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":11,"output_index":1,"item":{"id":"fc_call_choice1","type":"function_call","status":"completed","arguments":"{}","call_id":"call_choice1","name":"read"}},"event":"response.output_item.done"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":12,"item_id":"fc_call_choice1","output_index":1,"delta":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"},"event":"response.function_call_arguments.delta"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"resp_mixed","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"msg_resp_mixed_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call_choice1","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}","call_id":"call_choice1","name":"read"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":20}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_CompletedOmitsTopLevelOutputText","k":"st","o":{"model":"gpt-5.4"},"r":{"model":"gpt-5.4"},"m":"model","steps":[{"in":{"id":"resp_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello ","reasoning_content":null,"tool_calls":null},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_output_text","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_output_text","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_output_text_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_output_text_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_output_text_0","output_index":0,"content_index":0,"delta":"hello ","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"resp_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":"world","reasoning_content":null,"tool_calls":null},"finish_reason":"stop"}],"usage":{"completion_tokens":2,"total_tokens":4,"prompt_tokens":2}},"out":[{"data":{"type":"response.output_text.delta","sequence_number":6,"item_id":"msg_resp_output_text_0","output_index":0,"content_index":0,"delta":"world","logprobs":[]},"event":"response.output_text.delta"},{"data":{"type":"response.output_text.done","sequence_number":7,"item_id":"msg_resp_output_text_0","output_index":0,"content_index":0,"text":"hello world","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":8,"item_id":"msg_resp_output_text_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello world"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":9,"output_index":0,"item":{"id":"msg_resp_output_text_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello world"}],"role":"assistant"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":10,"response":{"id":"resp_output_text","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","output":[{"id":"msg_resp_output_text_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello world"}],"role":"assistant"}],"usage":{"input_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens":2,"total_tokens":4}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ToolCallCompletedOmitsTopLevelOutputText","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_tool_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"I will call the weather tool.","reasoning_content":null,"tool_calls":null},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_tool_output_text","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_tool_output_text","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_tool_output_text_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_tool_output_text_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_tool_output_text_0","output_index":0,"content_index":0,"delta":"I will call the weather tool.","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"resp_tool_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_weather","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_resp_tool_output_text_0","output_index":0,"content_index":0,"text":"I will call the weather tool.","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_resp_tool_output_text_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"I will call the weather tool."}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_resp_tool_output_text_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"I will call the weather tool."}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call_weather","type":"function_call","status":"in_progress","arguments":"","call_id":"call_weather","name":"get_weather"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_tool_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"location\":\"北京\",\"unit\":\"celsius\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call_weather","output_index":1,"delta":"{\"location\":\"北京\",\"unit\":\"celsius\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call_weather","output_index":1,"arguments":"{\"location\":\"北京\",\"unit\":\"celsius\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call_weather","type":"function_call","status":"completed","arguments":"{\"location\":\"北京\",\"unit\":\"celsius\"}","call_id":"call_weather","name":"get_weather"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"resp_tool_output_text","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"msg_resp_tool_output_text_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"I will call the weather tool."}],"role":"assistant"},{"id":"fc_call_weather","type":"function_call","status":"completed","arguments":"{\"location\":\"北京\",\"unit\":\"celsius\"}","call_id":"call_weather","name":"get_weather"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":20}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FunctionCallDoneAndCompletedOutputStayAscending","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_glob","type":"function","function":{"name":"glob","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_order","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_order","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_glob","type":"function_call","status":"in_progress","arguments":"","call_id":"call_glob","name":"glob"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_glob","output_index":0,"delta":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":1,"id":"call_read","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":{"id":"fc_call_read","type":"function_call","status":"in_progress","arguments":"","call_id":"call_read","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":1,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":6,"item_id":"fc_call_read","output_index":1,"delta":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":7,"item_id":"fc_call_glob","output_index":0,"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"fc_call_glob","type":"function_call","status":"completed","arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}","call_id":"call_glob","name":"glob"}},"event":"response.output_item.done"},{"data":{"type":"response.function_call_arguments.done","sequence_number":9,"item_id":"fc_call_read","output_index":1,"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":10,"output_index":1,"item":{"id":"fc_call_read","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}","call_id":"call_read","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":11,"response":{"id":"resp_order","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"fc_call_glob","type":"function_call","status":"completed","arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}","call_id":"call_glob","name":"glob"},{"id":"fc_call_read","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}","call_id":"call_read","name":"read"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":20}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_OmitsTopLevelOutputText","k":"ns","o":{"model":"gpt-5.4"},"r":{"model":"gpt-5.4"},"in":{"id":"chatcmpl_output_text","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"ping"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}},"out":{"id":"chatcmpl_output_text","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"gpt-5.4","output":[{"id":"msg_chatcmpl_output_text_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"ping"}],"role":"assistant"}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresNamespaceFunctionCall","k":"st","o":{"model":"deepseek-v4-flash","tools":[{"type":"namespace","name":"mcp__test_mcp__","tools":[{"type":"function","name":"add_numbers","parameters":{"type":"object","properties":{}}}]}]},"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_ns","type":"function","function":{"name":"mcp__test_mcp__add_numbers","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_namespace_stream","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"deepseek-v4-flash"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_namespace_stream","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"deepseek-v4-flash"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_ns","type":"function_call","status":"in_progress","arguments":"","call_id":"call_ns","name":"add_numbers","namespace":"mcp__test_mcp__"}},"event":"response.output_item.added"}]},{"in":{"id":"chatcmpl_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":3,\"b\":5}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_ns","output_index":0,"delta":"{\"a\":3,\"b\":5}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_ns","output_index":0,"arguments":"{\"a\":3,\"b\":5}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_ns","type":"function_call","status":"completed","arguments":"{\"a\":3,\"b\":5}","call_id":"call_ns","name":"add_numbers","namespace":"mcp__test_mcp__"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"chatcmpl_namespace_stream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"deepseek-v4-flash","tools":[{"name":"mcp__test_mcp__","tools":[{"name":"add_numbers","parameters":{"properties":{},"type":"object"},"type":"function"}],"type":"namespace"}],"output":[{"id":"fc_call_ns","type":"function_call","status":"completed","arguments":"{\"a\":3,\"b\":5}","call_id":"call_ns","name":"add_numbers","namespace":"mcp__test_mcp__"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresNamespaceFunctionCall","k":"ns","o":{"model":"deepseek-v4-flash","tools":[{"type":"namespace","name":"mcp__test_mcp__","tools":[{"type":"function","name":"add_numbers","parameters":{"type":"object","properties":{}}}]}]},"r":null,"in":{"id":"chatcmpl_namespace_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_ns","type":"function","function":{"name":"mcp__test_mcp__add_numbers","arguments":"{\"a\":3,\"b\":5}"}}]},"finish_reason":"tool_calls"}]},"out":{"id":"chatcmpl_namespace_nonstream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"model","output":[{"id":"fc_call_ns","type":"function_call","status":"completed","arguments":"{\"a\":3,\"b\":5}","call_id":"call_ns","name":"add_numbers","namespace":"mcp__test_mcp__"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresCappedNamespaceFunctionCall","k":"st","o":{"model":"deepseek-v4-flash","tools":[{"type":"namespace","name":"mcp__codex_apps__codex_document_control","tools":[{"type":"function","name":"_execute_document_command","parameters":{"type":"object"}}]}]},"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_capped_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_capped","type":"function","function":{"name":"p__codex_apps__codex_document_control___execute_document_command","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_capped_stream","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"deepseek-v4-flash"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_capped_stream","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"deepseek-v4-flash"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_capped","type":"function_call","status":"in_progress","arguments":"","call_id":"call_capped","name":"_execute_document_command","namespace":"mcp__codex_apps__codex_document_control"}},"event":"response.output_item.added"}]},{"in":{"id":"chatcmpl_capped_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"run\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_capped","output_index":0,"delta":"{\"cmd\":\"run\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_capped","output_index":0,"arguments":"{\"cmd\":\"run\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_capped","type":"function_call","status":"completed","arguments":"{\"cmd\":\"run\"}","call_id":"call_capped","name":"_execute_document_command","namespace":"mcp__codex_apps__codex_document_control"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"chatcmpl_capped_stream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"deepseek-v4-flash","tools":[{"name":"mcp__codex_apps__codex_document_control","tools":[{"name":"_execute_document_command","parameters":{"type":"object"},"type":"function"}],"type":"namespace"}],"output":[{"id":"fc_call_capped","type":"function_call","status":"completed","arguments":"{\"cmd\":\"run\"}","call_id":"call_capped","name":"_execute_document_command","namespace":"mcp__codex_apps__codex_document_control"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresCappedNamespaceFunctionCall","k":"ns","o":{"model":"deepseek-v4-flash","tools":[{"type":"namespace","name":"mcp__codex_apps__codex_document_control","tools":[{"type":"function","name":"_execute_document_command","parameters":{"type":"object"}}]}]},"r":null,"in":{"id":"chatcmpl_capped_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_capped","type":"function","function":{"name":"p__codex_apps__codex_document_control___execute_document_command","arguments":"{\"cmd\":\"run\"}"}}]},"finish_reason":"tool_calls"}]},"out":{"id":"chatcmpl_capped_nonstream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"model","output":[{"id":"fc_call_capped","type":"function_call","status":"completed","arguments":"{\"cmd\":\"run\"}","call_id":"call_capped","name":"_execute_document_command","namespace":"mcp__codex_apps__codex_document_control"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_CustomToolNameArrivesLate","k":"st","o":{"model":"gpt-5.4","tools":[{"type":"custom","name":"exec"}]},"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_custom_late_name","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_exec","type":"function","function":{"arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_custom_late_name","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_custom_late_name","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"}]},{"in":{"id":"chatcmpl_custom_late_name","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"exec","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"ctc_call_exec","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call_exec","name":"exec"}},"event":"response.output_item.added"}]},{"in":{"id":"chatcmpl_custom_late_name","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.custom_tool_call_input.done","sequence_number":4,"item_id":"ctc_call_exec","output_index":0,"input":"pwd"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"ctc_call_exec","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_exec","name":"exec"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":6,"response":{"id":"chatcmpl_custom_late_name","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","tools":[{"name":"exec","type":"custom"}],"output":[{"id":"ctc_call_exec","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_exec","name":"exec"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_CustomToolNameAndIDAreMissing","k":"st","o":{"model":"gpt-5.4","tools":[{"type":"custom","name":"exec"}]},"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_custom_missing_fields","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"function","function":{"arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_custom_missing_fields","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_custom_missing_fields","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"ctc_call_chatcmpl_custom_missing_fields_0_0","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call_chatcmpl_custom_missing_fields_0_0","name":"exec"}},"event":"response.output_item.added"},{"data":{"type":"response.custom_tool_call_input.done","sequence_number":4,"item_id":"ctc_call_chatcmpl_custom_missing_fields_0_0","output_index":0,"input":"pwd"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"ctc_call_chatcmpl_custom_missing_fields_0_0","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_chatcmpl_custom_missing_fields_0_0","name":"exec"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":6,"response":{"id":"chatcmpl_custom_missing_fields","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","tools":[{"name":"exec","type":"custom"}],"output":[{"id":"ctc_call_chatcmpl_custom_missing_fields_0_0","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_chatcmpl_custom_missing_fields_0_0","name":"exec"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ToolCallIDMayArriveLateOrBeMissing","k":"st","o":null,"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_late_id","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"function","function":{"name":"read","arguments":"{\"file"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_late_id","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"model"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_late_id","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"model"}},"event":"response.in_progress"}]},{"in":{"id":"chatcmpl_late_id","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_late","function":{"arguments":"Path\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_late","type":"function_call","status":"in_progress","arguments":"","call_id":"call_late","name":"read"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_late","output_index":0,"delta":"{\"filePath\":\"README.md\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_late","output_index":0,"arguments":"{\"filePath\":\"README.md\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_late","type":"function_call","status":"completed","arguments":"{\"filePath\":\"README.md\"}","call_id":"call_late","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"chatcmpl_late_id","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"output":[{"id":"fc_call_late","type":"function_call","status":"completed","arguments":"{\"filePath\":\"README.md\"}","call_id":"call_late","name":"read"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ToolCallIDMayArriveLateOrBeMissing","k":"st","o":null,"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_missing_id","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"function","function":{"name":"read","arguments":"{\"filePath\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_missing_id","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"model"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_missing_id","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"model"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_chatcmpl_missing_id_0_0","type":"function_call","status":"in_progress","arguments":"","call_id":"call_chatcmpl_missing_id_0_0","name":"read"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_chatcmpl_missing_id_0_0","output_index":0,"delta":"{\"filePath\":\"README.md\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_chatcmpl_missing_id_0_0","output_index":0,"arguments":"{\"filePath\":\"README.md\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_chatcmpl_missing_id_0_0","type":"function_call","status":"completed","arguments":"{\"filePath\":\"README.md\"}","call_id":"call_chatcmpl_missing_id_0_0","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"chatcmpl_missing_id","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"output":[{"id":"fc_call_chatcmpl_missing_id_0_0","type":"function_call","status":"completed","arguments":"{\"filePath\":\"README.md\"}","call_id":"call_chatcmpl_missing_id_0_0","name":"read"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresAdditionalNamespaceFunctionCall","k":"st","o":{"model":"gpt-5.4","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"send_message","parameters":{"type":"object","properties":{}}}]}]}]},"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_additional_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_send","type":"function","function":{"name":"collaboration__send_message","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_additional_namespace_stream","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_additional_namespace_stream","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_send","type":"function_call","status":"in_progress","arguments":"","call_id":"call_send","name":"send_message","namespace":"collaboration"}},"event":"response.output_item.added"}]},{"in":{"id":"chatcmpl_additional_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"target\":\"worker\",\"message\":\"ping\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_send","output_index":0,"delta":"{\"target\":\"worker\",\"message\":\"ping\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_send","output_index":0,"arguments":"{\"target\":\"worker\",\"message\":\"ping\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_send","type":"function_call","status":"completed","arguments":"{\"target\":\"worker\",\"message\":\"ping\"}","call_id":"call_send","name":"send_message","namespace":"collaboration"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"chatcmpl_additional_namespace_stream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","output":[{"id":"fc_call_send","type":"function_call","status":"completed","arguments":"{\"target\":\"worker\",\"message\":\"ping\"}","call_id":"call_send","name":"send_message","namespace":"collaboration"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresAdditionalNamespaceFunctionCall","k":"ns","o":{"model":"gpt-5.4","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"send_message","parameters":{"type":"object","properties":{}}}]}]}]},"r":null,"in":{"id":"chatcmpl_additional_namespace_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_send","type":"function","function":{"name":"collaboration__send_message","arguments":"{\"target\":\"worker\",\"message\":\"ping\"}"}}]},"finish_reason":"tool_calls"}]},"out":{"id":"chatcmpl_additional_namespace_nonstream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"model","output":[{"id":"fc_call_send","type":"function_call","status":"completed","arguments":"{\"target\":\"worker\",\"message\":\"ping\"}","call_id":"call_send","name":"send_message","namespace":"collaboration"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_RestoresAdditionalNamespaceCustomToolCall","k":"st","o":{"model":"gpt-5.4","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]}]},"r":null,"m":"model","steps":[{"in":{"id":"chatcmpl_additional_namespace_custom_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_exec","type":"function","function":{"name":"functions__exec","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_additional_namespace_custom_stream","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_additional_namespace_custom_stream","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"ctc_call_exec","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call_exec","name":"exec","namespace":"functions"}},"event":"response.output_item.added"}]},{"in":{"id":"chatcmpl_additional_namespace_custom_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.custom_tool_call_input.done","sequence_number":4,"item_id":"ctc_call_exec","output_index":0,"input":"pwd"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"ctc_call_exec","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_exec","name":"exec","namespace":"functions"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":6,"response":{"id":"chatcmpl_additional_namespace_custom_stream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","output":[{"id":"ctc_call_exec","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_exec","name":"exec","namespace":"functions"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_RestoresAdditionalNamespaceCustomToolCall","k":"ns","o":{"model":"gpt-5.4","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]}]},"r":null,"in":{"id":"chatcmpl_additional_namespace_custom_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_exec","type":"function","function":{"name":"functions__exec","arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]},"out":{"id":"chatcmpl_additional_namespace_custom_nonstream","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"model","output":[{"id":"ctc_call_exec","type":"custom_tool_call","status":"completed","input":"pwd","call_id":"call_exec","name":"exec","namespace":"functions"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_DoesNotCompleteReasoningOnlyStream","k":"st","o":{"model":"deepseek-v4-flash"},"r":{"model":"deepseek-v4-flash"},"m":"deepseek-v4-flash","steps":[{"in":{"id":"resp_reasoning_only","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"still thinking"},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_reasoning_only","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"deepseek-v4-flash"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_reasoning_only","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"deepseek-v4-flash"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"rs_resp_reasoning_only_0","type":"reasoning","status":"in_progress","summary":[]}},"event":"response.output_item.added"},{"data":{"type":"response.reasoning_summary_part.added","sequence_number":4,"item_id":"rs_resp_reasoning_only_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}},"event":"response.reasoning_summary_part.added"},{"data":{"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_resp_reasoning_only_0","output_index":0,"summary_index":0,"delta":"still thinking"},"event":"response.reasoning_summary_text.delta"}]},{"done":true,"out":[{"data":{"type":"response.reasoning_summary_text.done","sequence_number":6,"item_id":"rs_resp_reasoning_only_0","output_index":0,"summary_index":0,"text":"still thinking"},"event":"response.reasoning_summary_text.done"},{"data":{"type":"response.reasoning_summary_part.done","sequence_number":7,"item_id":"rs_resp_reasoning_only_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"still thinking"}},"event":"response.reasoning_summary_part.done"},{"data":{"type":"response.output_item.done","item":{"id":"rs_resp_reasoning_only_0","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"still thinking"}]},"output_index":0,"sequence_number":8},"event":"response.output_item.done"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_IncompleteToolStreamDoesNotFinalizeAsCompleted","k":"st","o":{"model":"gpt-5.6-terra"},"r":{"model":"gpt-5.6-terra"},"m":"gpt-5.6-terra","steps":[{"in":{"id":"resp_interrupted_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-terra","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_interrupted_tool","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.6-terra"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_interrupted_tool","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.6-terra"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_patch","type":"function_call","status":"in_progress","arguments":"","call_id":"call_patch","name":"apply_patch"}},"event":"response.output_item.added"}]},{"done":true,"out":[]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_IncompleteToolStreamDoesNotFinalizeAsCompleted","k":"st","o":{"model":"gpt-5.6-terra"},"r":{"model":"gpt-5.6-terra"},"m":"gpt-5.6-terra","steps":[{"in":{"id":"resp_interrupted_partial","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-terra","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":"{\"filePath\":\"foo"}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_interrupted_partial","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.6-terra"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_interrupted_partial","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.6-terra"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_patch","type":"function_call","status":"in_progress","arguments":"","call_id":"call_patch","name":"apply_patch"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_patch","output_index":0,"delta":"{\"filePath\":\"foo"},"event":"response.function_call_arguments.delta"}]},{"done":true,"out":[]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinishReasonLengthEmitsIncomplete","k":"st","o":{"model":"gpt-5.6-luna"},"r":{"model":"gpt-5.6-luna"},"m":"gpt-5.6-luna","steps":[{"in":{"id":"resp_length_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_length_tool","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.6-luna"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_length_tool","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.6-luna"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_patch","type":"function_call","status":"in_progress","arguments":"","call_id":"call_patch","name":"apply_patch"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_length_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":4,"item_id":"fc_call_patch","output_index":0,"arguments":""},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"fc_call_patch","type":"function_call","status":"incomplete","arguments":"","call_id":"call_patch","name":"apply_patch"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.incomplete","sequence_number":6,"response":{"id":"resp_length_tool","object":"response","created_at":1773896263,"status":"incomplete","background":false,"error":null,"incomplete_details":{"reason":"max_output_tokens"},"model":"gpt-5.6-luna","output":[{"id":"fc_call_patch","type":"function_call","status":"incomplete","arguments":"","call_id":"call_patch","name":"apply_patch"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":5,"total_tokens":15}}},"event":"response.incomplete"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinishReasonContentFilterEmitsIncomplete","k":"st","o":{"model":"gpt-5.6-luna"},"r":{"model":"gpt-5.6-luna"},"m":"gpt-5.6-luna","steps":[{"in":{"id":"resp_filter_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_filter_tool","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.6-luna"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_filter_tool","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.6-luna"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_patch","type":"function_call","status":"in_progress","arguments":"","call_id":"call_patch","name":"apply_patch"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_filter_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":4,"item_id":"fc_call_patch","output_index":0,"arguments":""},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"fc_call_patch","type":"function_call","status":"incomplete","arguments":"","call_id":"call_patch","name":"apply_patch"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.incomplete","sequence_number":6,"response":{"id":"resp_filter_tool","object":"response","created_at":1773896263,"status":"incomplete","background":false,"error":null,"incomplete_details":{"reason":"content_filter"},"model":"gpt-5.6-luna","output":[{"id":"fc_call_patch","type":"function_call","status":"incomplete","arguments":"","call_id":"call_patch","name":"apply_patch"}],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":5,"total_tokens":15}}},"event":"response.incomplete"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_FinishReasonLength","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_len","object":"chat.completion","created":1773896263,"model":"gpt-5.6","choices":[{"index":0,"message":{"role":"assistant","content":"truncated text"},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}},"out":{"id":"chatcmpl_len","object":"response","created_at":1773896263,"status":"incomplete","background":false,"error":null,"incomplete_details":{"reason":"max_output_tokens"},"model":"gpt-5.6","output":[{"id":"msg_chatcmpl_len_0","type":"message","status":"incomplete","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"truncated text"}],"role":"assistant"}],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_FinishReasonContentFilter","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_filter","object":"chat.completion","created":1773896263,"model":"gpt-5.6","choices":[{"index":0,"message":{"role":"assistant","content":"blocked text"},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}},"out":{"id":"chatcmpl_filter","object":"response","created_at":1773896263,"status":"incomplete","background":false,"error":null,"incomplete_details":{"reason":"content_filter"},"model":"gpt-5.6","output":[{"id":"msg_chatcmpl_filter_0","type":"message","status":"incomplete","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"blocked text"}],"role":"assistant"}],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_rc","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning_content":"thought from reasoning_content"},"finish_reason":"stop"}]},"out":{"id":"chatcmpl_rc","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"o3-mini","output":[{"id":"rs_chatcmpl_rc","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"thought from reasoning_content"}]},{"id":"msg_chatcmpl_rc_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_r","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning":"thought from reasoning"},"finish_reason":"stop"}]},"out":{"id":"chatcmpl_r","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"o3-mini","output":[{"id":"rs_chatcmpl_r","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"thought from reasoning"}]},{"id":"msg_chatcmpl_r_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_both","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning_content":"priority thought","reasoning":"ignored thought"},"finish_reason":"stop"}]},"out":{"id":"chatcmpl_both","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"o3-mini","output":[{"id":"rs_chatcmpl_both","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"priority thought"}]},{"id":"msg_chatcmpl_both_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_empty_rc","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning_content":"","reasoning":"fallback thought"},"finish_reason":"stop"}]},"out":{"id":"chatcmpl_empty_rc","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"o3-mini","output":[{"id":"rs_chatcmpl_empty_rc","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"fallback thought"}]},{"id":"msg_chatcmpl_empty_rc_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback","k":"ns","o":null,"r":null,"in":{"id":"chatcmpl_none","object":"chat.completion","created":1773896263,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]},"out":{"id":"chatcmpl_none","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"gpt-4o","output":[{"id":"msg_chatcmpl_none_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream_ReasoningFallback","k":"ns","o":{"model":"gpt-4o","reasoning":{"effort":"medium"}},"r":{"model":"gpt-4o","reasoning":{"effort":"medium"}},"in":{"id":"chatcmpl_req_only","object":"chat.completion","created":1773896263,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]},"out":{"id":"chatcmpl_req_only","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"incomplete_details":null,"model":"gpt-4o","reasoning":{"effort":"medium"},"output":[{"id":"rs_chatcmpl_req_only","type":"reasoning","encrypted_content":"","summary":[]},{"id":"msg_chatcmpl_req_only_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ChunkWithContentAndReasoningContent","k":"st","o":{"model":"deepseek-v4-flash"},"r":{"model":"deepseek-v4-flash"},"m":"deepseek-v4-flash","steps":[{"in":{"id":"chatcmpl_ds","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Thinking part 1,"},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_ds","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"deepseek-v4-flash"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_ds","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"deepseek-v4-flash"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"rs_chatcmpl_ds_0","type":"reasoning","status":"in_progress","summary":[]}},"event":"response.output_item.added"},{"data":{"type":"response.reasoning_summary_part.added","sequence_number":4,"item_id":"rs_chatcmpl_ds_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}},"event":"response.reasoning_summary_part.added"},{"data":{"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_chatcmpl_ds_0","output_index":0,"summary_index":0,"delta":"Thinking part 1,"},"event":"response.reasoning_summary_text.delta"}]},{"in":{"id":"chatcmpl_ds","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":"Bien","reasoning_content":" Just professional."},"finish_reason":null}]},"out":[{"data":{"type":"response.reasoning_summary_text.delta","sequence_number":6,"item_id":"rs_chatcmpl_ds_0","output_index":0,"summary_index":0,"delta":" Just professional."},"event":"response.reasoning_summary_text.delta"},{"data":{"type":"response.reasoning_summary_text.done","sequence_number":7,"item_id":"rs_chatcmpl_ds_0","output_index":0,"summary_index":0,"text":"Thinking part 1, Just professional."},"event":"response.reasoning_summary_text.done"},{"data":{"type":"response.reasoning_summary_part.done","sequence_number":8,"item_id":"rs_chatcmpl_ds_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Thinking part 1, Just professional."}},"event":"response.reasoning_summary_part.done"},{"data":{"type":"response.output_item.done","item":{"id":"rs_chatcmpl_ds_0","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"Thinking part 1, Just professional."}]},"output_index":0,"sequence_number":9},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":10,"output_index":1,"item":{"id":"msg_chatcmpl_ds_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":11,"item_id":"msg_chatcmpl_ds_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":12,"item_id":"msg_chatcmpl_ds_0","output_index":1,"content_index":0,"delta":"Bien","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"chatcmpl_ds","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":" continues here."},"finish_reason":"stop"}]},"out":[{"data":{"type":"response.output_text.delta","sequence_number":13,"item_id":"msg_chatcmpl_ds_0","output_index":1,"content_index":0,"delta":" continues here.","logprobs":[]},"event":"response.output_text.delta"},{"data":{"type":"response.output_text.done","sequence_number":14,"item_id":"msg_chatcmpl_ds_0","output_index":1,"content_index":0,"text":"Bien continues here.","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":15,"item_id":"msg_chatcmpl_ds_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Bien continues here."}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":16,"output_index":1,"item":{"id":"msg_chatcmpl_ds_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Bien continues here."}],"role":"assistant"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":17,"response":{"id":"chatcmpl_ds","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"deepseek-v4-flash","output":[{"id":"rs_chatcmpl_ds_0","type":"reasoning","summary":[{"type":"summary_text","text":"Thinking part 1, Just professional."}]},{"id":"msg_chatcmpl_ds_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Bien continues here."}],"role":"assistant"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_SingleChunkWithBothContentAndReasoningContent","k":"st","o":{"model":"deepseek-v4-flash"},"r":{"model":"deepseek-v4-flash"},"m":"deepseek-v4-flash","steps":[{"in":{"id":"chatcmpl_single","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":"Answer","reasoning_content":"Thought"},"finish_reason":"stop"}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_single","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"deepseek-v4-flash"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_single","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"deepseek-v4-flash"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"rs_chatcmpl_single_0","type":"reasoning","status":"in_progress","summary":[]}},"event":"response.output_item.added"},{"data":{"type":"response.reasoning_summary_part.added","sequence_number":4,"item_id":"rs_chatcmpl_single_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}},"event":"response.reasoning_summary_part.added"},{"data":{"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_chatcmpl_single_0","output_index":0,"summary_index":0,"delta":"Thought"},"event":"response.reasoning_summary_text.delta"},{"data":{"type":"response.reasoning_summary_text.done","sequence_number":6,"item_id":"rs_chatcmpl_single_0","output_index":0,"summary_index":0,"text":"Thought"},"event":"response.reasoning_summary_text.done"},{"data":{"type":"response.reasoning_summary_part.done","sequence_number":7,"item_id":"rs_chatcmpl_single_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Thought"}},"event":"response.reasoning_summary_part.done"},{"data":{"type":"response.output_item.done","item":{"id":"rs_chatcmpl_single_0","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"Thought"}]},"output_index":0,"sequence_number":8},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"msg_chatcmpl_single_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":10,"item_id":"msg_chatcmpl_single_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":11,"item_id":"msg_chatcmpl_single_0","output_index":1,"content_index":0,"delta":"Answer","logprobs":[]},"event":"response.output_text.delta"},{"data":{"type":"response.output_text.done","sequence_number":12,"item_id":"msg_chatcmpl_single_0","output_index":1,"content_index":0,"text":"Answer","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":13,"item_id":"msg_chatcmpl_single_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Answer"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":14,"output_index":1,"item":{"id":"msg_chatcmpl_single_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Answer"}],"role":"assistant"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":15,"response":{"id":"chatcmpl_single","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"deepseek-v4-flash","output":[{"id":"rs_chatcmpl_single_0","type":"reasoning","summary":[{"type":"summary_text","text":"Thought"}]},{"id":"msg_chatcmpl_single_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Answer"}],"role":"assistant"}]}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"req","m":"test","s":true,"in":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"out":{"model":"test","messages":[],"stream":true,"tools":[{"function":{"description":"","name":"editor__patch","parameters":{"properties":{"input":{"type":"string"}},"required":["input"],"type":"object"}},"type":"function"},{"function":{"description":"","name":"editor__read","parameters":{}},"type":"function"}]}}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"editor__read","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"read","namespace":"editor"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read","namespace":"editor"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor","tools":[{"name":"patch","type":"custom"},{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read","namespace":"editor"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"read","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"read","namespace":"editor"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read","namespace":"editor"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor","tools":[{"name":"patch","type":"custom"},{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read","namespace":"editor"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"editor__patch","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"ctc_call-test","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call-test","name":"patch","namespace":"editor"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.custom_tool_call_input.done","sequence_number":10,"item_id":"ctc_call-test","output_index":1,"input":"hello"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":11,"output_index":1,"item":{"id":"ctc_call-test","type":"custom_tool_call","status":"completed","input":"hello","call_id":"call-test","name":"patch","namespace":"editor"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":12,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor","tools":[{"name":"patch","type":"custom"},{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"ctc_call-test","type":"custom_tool_call","status":"completed","input":"hello","call_id":"call-test","name":"patch","namespace":"editor"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"patch","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"ctc_call-test","type":"custom_tool_call","status":"in_progress","input":"","call_id":"call-test","name":"patch","namespace":"editor"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.custom_tool_call_input.done","sequence_number":10,"item_id":"ctc_call-test","output_index":1,"input":"hello"},"event":"response.custom_tool_call_input.done"},{"data":{"type":"response.output_item.done","sequence_number":11,"output_index":1,"item":{"id":"ctc_call-test","type":"custom_tool_call","status":"completed","input":"hello","call_id":"call-test","name":"patch","namespace":"editor"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":12,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor","tools":[{"name":"patch","type":"custom"},{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"ctc_call-test","type":"custom_tool_call","status":"completed","input":"hello","call_id":"call-test","name":"patch","namespace":"editor"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"unknown","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"unknown"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"unknown"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor","tools":[{"name":"patch","type":"custom"},{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"unknown"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"req","m":"test","s":true,"in":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"out":{"model":"test","messages":[],"stream":true,"tools":[{"function":{"description":"","name":"editor__patch","parameters":{}},"type":"function"}]}}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"r":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"editor__read","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"editor__read"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor__patch","type":"function"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__read"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"r":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"read","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor__patch","type":"function"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"r":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"editor__patch","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"editor__patch"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__patch"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor__patch","type":"function"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__patch"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"r":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"patch","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"patch"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"patch"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor__patch","type":"function"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"patch"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"r":{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"unknown","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"unknown"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"unknown"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"editor__patch","type":"function"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"unknown"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"req","m":"test","s":true,"in":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"out":{"model":"test","messages":[],"stream":true,"tools":[{"function":{"description":"","name":"pacenamespacenamespacenamespacenamespacenamespacenamespace__read","parameters":{}},"type":"function"},{"function":{"description":"","name":"other__read","parameters":{}},"type":"function"}]}}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"editor__read","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"editor__read"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"name":"read","type":"function"}],"type":"namespace"},{"name":"other","tools":[{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__read"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"read","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"name":"read","type":"function"}],"type":"namespace"},{"name":"other","tools":[{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"read"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"editor__patch","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"editor__patch"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__patch"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"name":"read","type":"function"}],"type":"namespace"},{"name":"other","tools":[{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"editor__patch"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"patch","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"patch"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"patch"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"name":"read","type":"function"}],"type":"namespace"},{"name":"other","tools":[{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"patch"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesCompatibilityDigest","k":"st","o":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"r":{"model":"test","tools":[{"type":"namespace","name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"type":"function","name":"read"}]},{"type":"namespace","name":"other","tools":[{"type":"function","name":"read"}]}],"input":[]},"m":"test","steps":[{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r-test","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-test","function":{"name":"unknown","arguments":""}}]}}]},"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_r-test_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":9,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"in_progress","arguments":"","call_id":"call-test","name":"unknown"}},"event":"response.output_item.added"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_call-test","output_index":1,"delta":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.delta"}]},{"in":{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}},"out":[{"data":{"type":"response.function_call_arguments.done","sequence_number":11,"item_id":"fc_call-test","output_index":1,"arguments":"{\"input\":\"hello\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":12,"output_index":1,"item":{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"unknown"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":13,"response":{"id":"r-test","object":"response","created_at":1,"status":"completed","background":false,"error":null,"model":"test","tools":[{"name":"namespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespacenamespace","tools":[{"name":"read","type":"function"}],"type":"namespace"},{"name":"other","tools":[{"name":"read","type":"function"}],"type":"namespace"}],"output":[{"id":"msg_r-test_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"},{"id":"fc_call-test","type":"function_call","status":"completed","arguments":"{\"input\":\"hello\"}","call_id":"call-test","name":"unknown"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"total_tokens":110}}},"event":"response.completed"}]}]}
{"t":"TestResponsesRequestSelectionAndStreamIsolation","k":"st","o":{"tools":[{"type":"namespace","name":"original","tools":[{"type":"function","name":"run"}]}]},"r":{"tools":[{"type":"namespace","name":"translated","tools":[{"type":"function","name":"run"}]}]},"m":"test","steps":[{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"role":"assistant"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"}]},{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"in_progress","arguments":"","call_id":"c","name":"run","namespace":"original"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_c","output_index":0,"delta":"{}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_c","output_index":0,"arguments":"{}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run","namespace":"original"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"r","object":"response","created_at":1,"status":"completed","background":false,"error":null,"tools":[{"name":"original","tools":[{"name":"run","type":"function"}],"type":"namespace"}],"output":[{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run","namespace":"original"}]}},"event":"response.completed"}]}]}
{"t":"TestResponsesRequestSelectionAndStreamIsolation","k":"st","o":null,"r":{"tools":[{"type":"namespace","name":"fallback","tools":[{"type":"function","name":"run"}]}]},"m":"test","steps":[{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"role":"assistant"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"}]},{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"in_progress","arguments":"","call_id":"c","name":"run","namespace":"fallback"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_c","output_index":0,"delta":"{}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_c","output_index":0,"arguments":"{}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run","namespace":"fallback"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"r","object":"response","created_at":1,"status":"completed","background":false,"error":null,"tools":[{"name":"fallback","tools":[{"name":"run","type":"function"}],"type":"namespace"}],"output":[{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run","namespace":"fallback"}]}},"event":"response.completed"}]}]}
{"t":"TestResponsesRequestSelectionAndStreamIsolation","k":"st","o":null,"r":{"tools":[{"type":"namespace","name":"separate","tools":[{"type":"function","name":"run"}]}]},"m":"test","steps":[{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"role":"assistant"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"}]},{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"in_progress","arguments":"","call_id":"c","name":"run","namespace":"separate"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_c","output_index":0,"delta":"{}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_c","output_index":0,"arguments":"{}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run","namespace":"separate"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"r","object":"response","created_at":1,"status":"completed","background":false,"error":null,"tools":[{"name":"separate","tools":[{"name":"run","type":"function"}],"type":"namespace"}],"output":[{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run","namespace":"separate"}]}},"event":"response.completed"}]}]}
{"t":"TestResponsesRequestSelectionAndStreamIsolation","k":"st","o":null,"r":null,"m":"test","steps":[{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"role":"assistant"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","background":false,"error":null,"output":[],"model":"test"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"r","object":"response","created_at":1,"status":"in_progress","output":[],"model":"test"}},"event":"response.in_progress"}]},{"in":{"id":"r","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"in_progress","arguments":"","call_id":"c","name":"run"}},"event":"response.output_item.added"},{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_c","output_index":0,"delta":"{}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_c","output_index":0,"arguments":"{}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"r","object":"response","created_at":1,"status":"completed","background":false,"error":null,"output":[{"id":"fc_c","type":"function_call","status":"completed","arguments":"{}","call_id":"c","name":"run"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ResponseCompletedWaitsForDone","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_late_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_late_usage","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_late_usage","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_late_usage","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_late_usage","type":"function_call","status":"in_progress","arguments":"","call_id":"call_late_usage","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_late_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_late_usage","output_index":0,"delta":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_late_usage","output_index":0,"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_late_usage","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}","call_id":"call_late_usage","name":"read"}},"event":"response.output_item.done"}]},{"in":{"id":"resp_late_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}},"out":[]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"resp_late_usage","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"fc_call_late_usage","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}","call_id":"call_late_usage","name":"read"}],"usage":{"input_tokens":11,"input_tokens_details":{"cached_tokens":0},"output_tokens":7,"total_tokens":18}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_EmptyToolCallsArrayDoesNotTerminateItems","k":"st","o":{"model":"codebuddy-hy4"},"r":{"model":"codebuddy-hy4"},"m":"codebuddy-hy4","steps":[{"in":{"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":"Thinking part 1, ","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"chatcmpl_empty_tc","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"codebuddy-hy4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"chatcmpl_empty_tc","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"codebuddy-hy4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"rs_chatcmpl_empty_tc_0","type":"reasoning","status":"in_progress","summary":[]}},"event":"response.output_item.added"},{"data":{"type":"response.reasoning_summary_part.added","sequence_number":4,"item_id":"rs_chatcmpl_empty_tc_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}},"event":"response.reasoning_summary_part.added"},{"data":{"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_chatcmpl_empty_tc_0","output_index":0,"summary_index":0,"delta":"Thinking part 1, "},"event":"response.reasoning_summary_text.delta"}]},{"in":{"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"content":"","reasoning_content":"thinking part 2.","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":null}]},"out":[{"data":{"type":"response.reasoning_summary_text.delta","sequence_number":6,"item_id":"rs_chatcmpl_empty_tc_0","output_index":0,"summary_index":0,"delta":"thinking part 2."},"event":"response.reasoning_summary_text.delta"}]},{"in":{"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"content":"Hello ","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":null}]},"out":[{"data":{"type":"response.reasoning_summary_text.done","sequence_number":7,"item_id":"rs_chatcmpl_empty_tc_0","output_index":0,"summary_index":0,"text":"Thinking part 1, thinking part 2."},"event":"response.reasoning_summary_text.done"},{"data":{"type":"response.reasoning_summary_part.done","sequence_number":8,"item_id":"rs_chatcmpl_empty_tc_0","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Thinking part 1, thinking part 2."}},"event":"response.reasoning_summary_part.done"},{"data":{"type":"response.output_item.done","item":{"id":"rs_chatcmpl_empty_tc_0","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":"Thinking part 1, thinking part 2."}]},"output_index":0,"sequence_number":9},"event":"response.output_item.done"},{"data":{"type":"response.output_item.added","sequence_number":10,"output_index":1,"item":{"id":"msg_chatcmpl_empty_tc_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":11,"item_id":"msg_chatcmpl_empty_tc_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":12,"item_id":"msg_chatcmpl_empty_tc_0","output_index":1,"content_index":0,"delta":"Hello ","logprobs":[]},"event":"response.output_text.delta"}]},{"in":{"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"content":"world!","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":"stop"}]},"out":[{"data":{"type":"response.output_text.delta","sequence_number":13,"item_id":"msg_chatcmpl_empty_tc_0","output_index":1,"content_index":0,"delta":"world!","logprobs":[]},"event":"response.output_text.delta"},{"data":{"type":"response.output_text.done","sequence_number":14,"item_id":"msg_chatcmpl_empty_tc_0","output_index":1,"content_index":0,"text":"Hello world!","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":15,"item_id":"msg_chatcmpl_empty_tc_0","output_index":1,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello world!"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":16,"output_index":1,"item":{"id":"msg_chatcmpl_empty_tc_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello world!"}],"role":"assistant"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":17,"response":{"id":"chatcmpl_empty_tc","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"codebuddy-hy4","output":[{"id":"rs_chatcmpl_empty_tc_0","type":"reasoning","summary":[{"type":"summary_text","text":"Thinking part 1, thinking part 2."}]},{"id":"msg_chatcmpl_empty_tc_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello world!"}],"role":"assistant"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinalizesOpenMessageAtStreamEnd","k":"st","o":{"model":"gpt-5.4"},"r":{"model":"gpt-5.4"},"m":"model","steps":[{"in":{"id":"resp_missing_finish_reason","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_missing_finish_reason","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_missing_finish_reason","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_missing_finish_reason_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_missing_finish_reason_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_missing_finish_reason_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"done":true,"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_resp_missing_finish_reason_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_resp_missing_finish_reason_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_resp_missing_finish_reason_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.completed","sequence_number":9,"response":{"id":"resp_missing_finish_reason","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","output":[{"id":"msg_resp_missing_finish_reason_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ResponseCompletedWaitsForDone","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_usage_same_chunk","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_usage_same_chunk","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_usage_same_chunk","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_usage_same_chunk","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_usage_same_chunk","type":"function_call","status":"in_progress","arguments":"","call_id":"call_usage_same_chunk","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_usage_same_chunk","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":13,"completion_tokens":5,"total_tokens":18}},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_usage_same_chunk","output_index":0,"delta":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_usage_same_chunk","output_index":0,"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_usage_same_chunk","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}","call_id":"call_usage_same_chunk","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"resp_usage_same_chunk","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"fc_call_usage_same_chunk","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}","call_id":"call_usage_same_chunk","name":"read"}],"usage":{"input_tokens":13,"input_tokens_details":{"cached_tokens":0},"output_tokens":5,"total_tokens":18}}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinalizesOpenMessageAtStreamEnd","k":"st","o":{"model":"gpt-5.4"},"r":{"model":"gpt-5.4"},"m":"model","steps":[{"in":{"id":"resp_null_finish_reason","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_null_finish_reason","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_null_finish_reason","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_null_finish_reason_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_null_finish_reason_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_null_finish_reason_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"done":true,"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_resp_null_finish_reason_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_resp_null_finish_reason_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_resp_null_finish_reason_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.completed","sequence_number":9,"response":{"id":"resp_null_finish_reason","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","output":[{"id":"msg_resp_null_finish_reason_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ResponseCompletedWaitsForDone","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_no_finish_reason","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_no_finish_reason","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_no_finish_reason","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"msg_resp_no_finish_reason_0","type":"message","status":"in_progress","content":[],"role":"assistant"}},"event":"response.output_item.added"},{"data":{"type":"response.content_part.added","sequence_number":4,"item_id":"msg_resp_no_finish_reason_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}},"event":"response.content_part.added"},{"data":{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_resp_no_finish_reason_0","output_index":0,"content_index":0,"delta":"hello","logprobs":[]},"event":"response.output_text.delta"}]},{"done":true,"out":[{"data":{"type":"response.output_text.done","sequence_number":6,"item_id":"msg_resp_no_finish_reason_0","output_index":0,"content_index":0,"text":"hello","logprobs":[]},"event":"response.output_text.done"},{"data":{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_resp_no_finish_reason_0","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}},"event":"response.content_part.done"},{"data":{"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_resp_no_finish_reason_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}},"event":"response.output_item.done"},{"data":{"type":"response.completed","sequence_number":9,"response":{"id":"resp_no_finish_reason","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"msg_resp_no_finish_reason_0","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"hello"}],"role":"assistant"}]}},"event":"response.completed"}]}]}
{"t":"TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_ResponseCompletedWaitsForDone","k":"st","o":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"r":{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true},"m":"model","steps":[{"in":{"id":"resp_no_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_no_usage","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]},"out":[{"data":{"type":"response.created","sequence_number":1,"response":{"id":"resp_no_usage","object":"response","created_at":1773896263,"status":"in_progress","background":false,"error":null,"output":[],"model":"gpt-5.4"}},"event":"response.created"},{"data":{"type":"response.in_progress","sequence_number":2,"response":{"id":"resp_no_usage","object":"response","created_at":1773896263,"status":"in_progress","output":[],"model":"gpt-5.4"}},"event":"response.in_progress"},{"data":{"type":"response.output_item.added","sequence_number":3,"output_index":0,"item":{"id":"fc_call_no_usage","type":"function_call","status":"in_progress","arguments":"","call_id":"call_no_usage","name":"read"}},"event":"response.output_item.added"}]},{"in":{"id":"resp_no_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"}}]},"finish_reason":"tool_calls"}]},"out":[{"data":{"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"fc_call_no_usage","output_index":0,"delta":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"},"event":"response.function_call_arguments.delta"},{"data":{"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"fc_call_no_usage","output_index":0,"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"},"event":"response.function_call_arguments.done"},{"data":{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"fc_call_no_usage","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}","call_id":"call_no_usage","name":"read"}},"event":"response.output_item.done"}]},{"done":true,"out":[{"data":{"type":"response.completed","sequence_number":7,"response":{"id":"resp_no_usage","object":"response","created_at":1773896263,"status":"completed","background":false,"error":null,"model":"gpt-5.4","parallel_tool_calls":true,"tool_choice":"auto","output":[{"id":"fc_call_no_usage","type":"function_call","status":"completed","arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}","call_id":"call_no_usage","name":"read"}]}},"event":"response.completed"}]}]}"####;
}
