//! OpenAI Responses (client) → ChatGPT Codex backend (upstream) request translator.
//!
//! Faithful port of CLIProxyAPI's
//! `internal/translator/codex/openai/responses/codex_openai-responses_request.go`
//! (`ConvertOpenAIResponsesRequestToCodex` and its helpers). It turns an OpenAI
//! Responses request into the body the ChatGPT Codex backend accepts; the
//! executor's own normalization (headers, `prompt_cache_key`, …) is ported
//! elsewhere. Every ported function carries a `// port of <GoFunc> (<file>)`
//! comment so it can be diffed against the source.
//!
//! The Go code edits raw bytes with gjson/sjson; this port edits a cloned
//! `serde_json::Value`. With `preserve_order`, `insert` on an existing key keeps
//! its position and a new key is appended (sjson's behaviour), and every removal
//! is `shift_remove` so the remaining key order is untouched (sjson deletes in
//! place). gjson's `Exists()` is true for a JSON `null`, which `contains_key`
//! reproduces.
//!
//! Deliberately NOT ported: the `bytes.Contains` fast-path guard in
//! `stripCodexResponsesCacheBreakpoints` (an optimisation only — the slow path
//! changes nothing when no breakpoint key exists), the debug logging in the
//! tool normalizers, and the zero-copy "return the same slice" property (a
//! `Value` is always returned by value). A body whose root is not a JSON
//! object is returned unchanged (sjson cannot set object paths on it either).

use serde_json::{json, Map, Value};

/// Translate an OpenAI Responses request body into the Codex backend's shape.
///
/// `model` and `stream` mirror the Go signature
/// `ConvertOpenAIResponsesRequestToCodex(modelName, inputRawJSON, _ bool)`, which
/// uses NEITHER: the client's own `model` field is forwarded as-is, and
/// `stream` is forced to `true` regardless of what the client asked for.
// port of ConvertOpenAIResponsesRequestToCodex (codex_openai-responses_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    let _ = (model, stream);
    let mut raw = body.clone();
    let Some(root) = raw.as_object_mut() else {
        return raw;
    };

    if let Some(Value::String(text)) = root.get("input") {
        let input = json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        }]);
        root.insert("input".to_string(), input);
    }

    set_codex_required_bool(root, "stream", true);
    set_codex_required_bool(root, "store", false);
    set_codex_required_bool(root, "parallel_tool_calls", true);
    set_codex_required_include(root);
    // Codex Responses rejects token limit fields, so strip them out before forwarding.
    delete_codex_request_fields(
        root,
        &[
            "max_output_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
        ],
    );
    if let Some(service_tier) = root.get("service_tier") {
        match service_tier {
            Value::String(tier) => match tier.trim().to_lowercase().as_str() {
                "priority" | "fast" => {
                    if tier != "priority" {
                        root.insert("service_tier".to_string(), json!("priority"));
                    }
                }
                "ultrafast" => {
                    if tier != "ultrafast" {
                        root.insert("service_tier".to_string(), json!("ultrafast"));
                    }
                }
                _ => delete_codex_request_fields(root, &["service_tier"]),
            },
            _ => delete_codex_request_fields(root, &["service_tier"]),
        }
    }

    delete_codex_request_fields(
        root,
        &[
            "truncation",
            "prompt_cache_options",
            "prompt_cache_retention",
        ],
    );
    strip_codex_responses_cache_breakpoints(root);
    apply_responses_compaction_compatibility(root);

    // Delete the user field as it is not supported by the Codex upstream.
    delete_codex_request_fields(root, &["user"]);

    // Convert role "system" to "developer" in input array to comply with Codex API requirements.
    convert_system_role_to_developer(root);
    normalize_codex_builtin_tools(root);

    raw
}

// port of setCodexRequiredBool (codex_openai-responses_request.go)
fn set_codex_required_bool(root: &mut Map<String, Value>, path: &str, value: bool) {
    if root.get(path) == Some(&Value::Bool(value)) {
        return;
    }
    root.insert(path.to_string(), Value::Bool(value));
}

// port of setCodexRequiredInclude (codex_openai-responses_request.go)
fn set_codex_required_include(root: &mut Map<String, Value>) {
    if let Some(Value::Array(values)) = root.get("include") {
        if values.len() == 1 && values[0].as_str() == Some("reasoning.encrypted_content") {
            return;
        }
    }
    root.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );
}

// port of deleteCodexRequestFields (codex_openai-responses_request.go)
fn delete_codex_request_fields(root: &mut Map<String, Value>, paths: &[&str]) {
    for path in paths {
        root.shift_remove(*path);
    }
}

/// Removes any `prompt_cache_breakpoint` hint attached to input items: inside
/// content-part arrays (message `input[].content[]` and function_call_output
/// `input[].output[]`) or as an item-level field. Codex Responses rejects it
/// outright ("prompt_cache_breakpoint is not supported on this model"); the
/// top-level `prompt_cache_options` strip does not cover these nested cases.
// port of stripCodexResponsesCacheBreakpoints (codex_openai-responses_request.go)
fn strip_codex_responses_cache_breakpoints(root: &mut Map<String, Value>) {
    let Some(Value::Array(input_items)) = root.get_mut("input") else {
        return;
    };
    for item in input_items.iter_mut() {
        let Some(item) = item.as_object_mut() else {
            continue;
        };
        for array_path in ["content", "output"] {
            if let Some(Value::Array(parts)) = item.get_mut(array_path) {
                strip_prompt_cache_breakpoint_from_content(parts);
            }
        }
        item.shift_remove("prompt_cache_breakpoint");
    }
}

/// Removes `prompt_cache_breakpoint` from each content part that carries it and
/// reports whether anything changed.
// port of stripPromptCacheBreakpointFromContent (codex_openai-responses_request.go)
fn strip_prompt_cache_breakpoint_from_content(parts: &mut [Value]) -> bool {
    let mut changed = false;
    for part in parts.iter_mut() {
        if let Some(part) = part.as_object_mut() {
            if part.shift_remove("prompt_cache_breakpoint").is_some() {
                changed = true;
            }
        }
    }
    changed
}

/// Codex /responses rejects `context_management` ("Unsupported parameter:
/// context_management"), so it is removed before forwarding.
// port of applyResponsesCompactionCompatibility (codex_openai-responses_request.go)
fn apply_responses_compaction_compatibility(root: &mut Map<String, Value>) {
    root.shift_remove("context_management");
}

/// Converts every input item with role `system` to role `developer` — the Codex
/// API does not accept `system` in the input array.
// port of convertSystemRoleToDeveloper (codex_openai-responses_request.go)
fn convert_system_role_to_developer(root: &mut Map<String, Value>) {
    if let Some(input) = root.get_mut("input") {
        convert_system_role_to_developer_with_input(input);
    }
}

// port of convertSystemRoleToDeveloperWithInput (codex_openai-responses_request.go)
fn convert_system_role_to_developer_with_input(input: &mut Value) {
    let Some(input_items) = input.as_array_mut() else {
        return;
    };
    for item in input_items.iter_mut() {
        if let Some(item) = item.as_object_mut() {
            if item.get("role").and_then(Value::as_str) == Some("system") {
                item.insert("role".to_string(), json!("developer"));
            }
        }
    }
}

/// Rewrites legacy/preview built-in tool variants to the stable names the
/// current Codex upstream expects.
// port of normalizeCodexBuiltinTools (codex_openai-responses_request.go)
fn normalize_codex_builtin_tools(root: &mut Map<String, Value>) {
    if let Some(tools) = root.get_mut("tools") {
        normalize_codex_builtin_tool_array(tools);
    }
    if let Some(Value::Object(tool_choice)) = root.get_mut("tool_choice") {
        normalize_codex_builtin_tool_at_path(tool_choice, "type");
        if let Some(tools) = tool_choice.get_mut("tools") {
            normalize_codex_builtin_tool_array(tools);
        }
    }
}

// port of normalizeCodexBuiltinToolArray (codex_openai-responses_request.go)
fn normalize_codex_builtin_tool_array(tools: &mut Value) {
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    for tool in tools.iter_mut() {
        if let Some(tool) = tool.as_object_mut() {
            normalize_codex_builtin_tool_at_path(tool, "type");
        }
    }
}

// port of normalizeCodexBuiltinToolAtPath (codex_openai-responses_request.go)
fn normalize_codex_builtin_tool_at_path(obj: &mut Map<String, Value>, key: &str) {
    let current = obj.get(key).and_then(Value::as_str).unwrap_or("");
    if let Some(normalized) = normalize_codex_builtin_tool_type(current) {
        obj.insert(key.to_string(), json!(normalized));
    }
}

/// The known Codex Responses built-in tool aliases. Extend this helper rather
/// than adding path-specific rewrites elsewhere.
// port of normalizeCodexBuiltinToolType (codex_openai-responses_request.go)
fn normalize_codex_builtin_tool_type(tool_type: &str) -> Option<&'static str> {
    match tool_type {
        "web_search_preview" | "web_search_preview_2025_03_11" => Some("web_search"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(model: &str, input: Value, stream: bool) -> Value {
        translate_request(model, &input, stream)
    }

    // port of TestConvertSystemRoleToDeveloper_BasicConversion
    #[test]
    fn system_role_basic_conversion() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "You are a pirate."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Say hello."}]}
                ]
            }),
            false,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are a pirate."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Say hello."}]}
                ],
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestConvertSystemRoleToDeveloper_MultipleSystemMessages
    #[test]
    fn system_role_multiple_system_messages() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "You are helpful."}]},
                    {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "Be concise."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}
                ]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are helpful."}]},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Be concise."}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}
            ])
        );
    }

    // port of TestConvertSystemRoleToDeveloper_NoSystemMessages
    #[test]
    fn system_role_no_system_messages() {
        let input = json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi there!"}]}
        ]);
        let out = run(
            "gpt-5.2",
            json!({"model": "gpt-5.2", "input": input.clone()}),
            false,
        );
        assert_eq!(out["input"], input);
    }

    // port of TestConvertSystemRoleToDeveloper_EmptyInput
    #[test]
    fn system_role_empty_input() {
        let out = run("gpt-5.2", json!({"model": "gpt-5.2", "input": []}), false);
        assert_eq!(out["input"], json!([]));
    }

    // port of TestConvertSystemRoleToDeveloper_NoInputField
    #[test]
    fn system_role_no_input_field() {
        let out = run(
            "gpt-5.2",
            json!({"model": "gpt-5.2", "stream": false}),
            false,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.2",
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestConvertOpenAIResponsesRequestToCodex_OriginalIssue
    #[test]
    fn original_issue() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "message", "role": "system", "content": "You are a pirate. Always respond in pirate speak."},
                    {"type": "message", "role": "user", "content": "Say hello."}
                ],
                "stream": false
            }),
            false,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "message", "role": "developer", "content": "You are a pirate. Always respond in pirate speak."},
                    {"type": "message", "role": "user", "content": "Say hello."}
                ],
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestConvertOpenAIResponsesRequestToCodexReusesNormalizedPayload
    // (the pointer-identity half has no Value equivalent; the byte-identity half
    // is checked through serialization, which also pins key order).
    #[test]
    fn reuses_normalized_payload() {
        let text = r#"{"model":"gpt-5.6","stream":true,"store":false,"parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"service_tier":"priority","input":[{"type":"message","role":"user","content":"hello"}]}"#;
        let input: Value = serde_json::from_str(text).unwrap();
        let out = run("gpt-5.6", input.clone(), true);
        assert_eq!(out, input);
        assert_eq!(serde_json::to_string(&out).unwrap(), text);
    }

    // port of TestConvertOpenAIResponsesRequestToCodexNormalizesRequiredFields
    #[test]
    fn normalizes_required_fields() {
        let out = run(
            "gpt-5.6",
            json!({
                "model": "gpt-5.6",
                "stream": "true",
                "store": true,
                "parallel_tool_calls": false,
                "include": ["file_search_call.results", "reasoning.encrypted_content"],
                "max_output_tokens": 4096,
                "max_completion_tokens": 4096,
                "temperature": 0.2,
                "top_p": 0.9,
                "service_tier": "standard",
                "truncation": "auto",
                "prompt_cache_options": {"mode": "implicit"},
                "prompt_cache_retention": "24h",
                "user": "request-owner",
                "input": [{"type": "message", "role": "system", "content": "hello"}]
            }),
            true,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.6",
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"],
                "input": [{"type": "message", "role": "developer", "content": "hello"}]
            })
        );
        // sjson replaces existing keys in place.
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"model":"gpt-5.6","stream":true,"store":false,"parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"input":[{"type":"message","role":"developer","content":"hello"}]}"#
        );
    }

    // port of TestConvertOpenAIResponsesRequestToCodex_FiltersPromptCacheRetention
    #[test]
    fn filters_prompt_cache_retention() {
        let out = run(
            "gpt-5.6-terra",
            json!({
                "model": "gpt-5.6-terra",
                "prompt_cache_retention": "24h",
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]
            }),
            true,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.6-terra",
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestConvertSystemRoleToDeveloper_AssistantRole
    #[test]
    fn system_role_assistant_preserved() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "You are helpful."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi!"}]}
                ]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are helpful."}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi!"}]}
            ])
        );
    }

    // port of TestConvertOpenAIResponsesRequestToCodex_NormalizesWebSearchPreview
    #[test]
    fn normalizes_web_search_preview() {
        let out = run(
            "gpt-5.4-mini",
            json!({
                "model": "gpt-5.4-mini",
                "input": "find latest OpenAI model news",
                "tools": [{"type": "web_search_preview_2025_03_11"}],
                "tool_choice": {
                    "type": "allowed_tools",
                    "tools": [{"type": "web_search_preview"}, {"type": "web_search_preview_2025_03_11"}]
                }
            }),
            false,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.4-mini",
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "find latest OpenAI model news"}]}],
                "tools": [{"type": "web_search"}],
                "tool_choice": {
                    "type": "allowed_tools",
                    "tools": [{"type": "web_search"}, {"type": "web_search"}]
                },
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestConvertOpenAIResponsesRequestToCodex_NormalizesTopLevelToolChoicePreviewAlias
    #[test]
    fn normalizes_top_level_tool_choice_preview_alias() {
        let out = run(
            "gpt-5.4-mini",
            json!({
                "model": "gpt-5.4-mini",
                "input": "find latest OpenAI model news",
                "tool_choice": {"type": "web_search_preview_2025_03_11"}
            }),
            false,
        );
        assert_eq!(out["tool_choice"], json!({"type": "web_search"}));
    }

    // port of TestUserFieldDeletion
    #[test]
    fn user_field_deletion() {
        let out = run(
            "gpt-5.2",
            json!({"model": "gpt-5.2", "user": "test-user", "input": [{"role": "user", "content": "Hello"}]}),
            false,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.2",
                "input": [{"role": "user", "content": "Hello"}],
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestContextManagementCompactionCompatibility
    #[test]
    fn context_management_compaction_compatibility() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "context_management": [{"type": "compaction", "compact_threshold": 12000}],
                "input": [{"role": "user", "content": "hello"}]
            }),
            false,
        );
        assert_eq!(
            out,
            json!({
                "model": "gpt-5.2",
                "input": [{"role": "user", "content": "hello"}],
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
        );
    }

    // port of TestTruncationRemovedForCodexCompatibility
    #[test]
    fn truncation_removed() {
        let out = run(
            "gpt-5.2",
            json!({"model": "gpt-5.2", "truncation": "disabled", "input": [{"role": "user", "content": "hello"}]}),
            false,
        );
        assert!(!out.as_object().unwrap().contains_key("truncation"));
        assert_eq!(out["input"], json!([{"role": "user", "content": "hello"}]));
    }

    // port of TestStripCodexResponsesCacheBreakpoints
    #[test]
    fn strip_cache_breakpoints() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Hello world", "prompt_cache_breakpoint": {"mode": "explicit"}},
                        {"type": "input_text", "text": "Second part"}
                    ]
                }]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Hello world"},
                    {"type": "input_text", "text": "Second part"}
                ]
            }])
        );
    }

    // port of TestStripCodexResponsesCacheBreakpoints_FunctionCallOutputParts
    #[test]
    fn strip_cache_breakpoints_function_call_output_parts() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {"type": "function_call", "name": "shell", "call_id": "call_abc", "arguments": "{}"},
                    {
                        "type": "function_call_output",
                        "call_id": "call_abc",
                        "output": [{"type": "input_text", "text": "tool output", "prompt_cache_breakpoint": {"mode": "explicit"}}]
                    }
                ]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type": "function_call", "name": "shell", "call_id": "call_abc", "arguments": "{}"},
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": [{"type": "input_text", "text": "tool output"}]
                }
            ])
        );
    }

    // port of TestStripCodexResponsesCacheBreakpoints_ItemLevel
    #[test]
    fn strip_cache_breakpoints_item_level() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}],
                        "prompt_cache_breakpoint": {"mode": "explicit"}
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_abc",
                        "output": "plain string output",
                        "prompt_cache_breakpoint": {"mode": "explicit"}
                    }
                ]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call_output", "call_id": "call_abc", "output": "plain string output"}
            ])
        );
    }

    // port of TestStripCodexResponsesCacheBreakpoints_CombinedItemAndPartLevel
    #[test]
    fn strip_cache_breakpoints_combined_item_and_part_level() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [{
                    "type": "function_call_output",
                    "call_id": "call_123",
                    "prompt_cache_breakpoint": {"mode": "explicit"},
                    "output": [
                        {"type": "input_text", "text": "result part 1", "prompt_cache_breakpoint": {"mode": "explicit"}},
                        {"type": "input_text", "text": "result part 2"}
                    ]
                }]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([{
                "type": "function_call_output",
                "call_id": "call_123",
                "output": [
                    {"type": "input_text", "text": "result part 1"},
                    {"type": "input_text", "text": "result part 2"}
                ]
            }])
        );
    }

    // port of TestStripCodexResponsesCacheBreakpoints_WithSystemRole
    #[test]
    fn strip_cache_breakpoints_with_system_role() {
        let out = run(
            "gpt-5.2",
            json!({
                "model": "gpt-5.2",
                "input": [
                    {
                        "type": "message",
                        "role": "system",
                        "content": [{"type": "input_text", "text": "System prompt", "prompt_cache_breakpoint": {"mode": "explicit"}}]
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "User query", "prompt_cache_breakpoint": {"mode": "explicit"}}]
                    }
                ]
            }),
            false,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "System prompt"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "User query"}]}
            ])
        );
    }

    // port of TestConvertOpenAIResponsesRequestToCodex_ServiceTier
    #[test]
    fn service_tier() {
        let cases: &[(&str, Value, Option<&str>)] = &[
            ("priority preserved", json!("priority"), Some("priority")),
            (
                "priority case insensitive and trimmed",
                json!(" Priority "),
                Some("priority"),
            ),
            (
                "fast normalized to priority",
                json!("fast"),
                Some("priority"),
            ),
            ("ultrafast preserved", json!("ultrafast"), Some("ultrafast")),
            (
                "ultrafast case insensitive and trimmed",
                json!(" UltraFast "),
                Some("ultrafast"),
            ),
            ("standard stripped", json!("standard"), None),
            ("default stripped", json!("default"), None),
            ("flex stripped", json!("flex"), None),
            ("non-string stripped", json!(123), None),
            ("null stripped", Value::Null, None),
            ("bool stripped", json!(true), None),
            ("empty string stripped", json!(""), None),
            ("whitespace string stripped", json!("   "), None),
        ];
        for (name, tier, want) in cases {
            let out = run(
                "gpt-5.6",
                json!({
                    "model": "gpt-5.6",
                    "service_tier": tier,
                    "input": [{"type": "message", "role": "user", "content": "hello"}]
                }),
                true,
            );
            let mut expected = json!({"model": "gpt-5.6"});
            if let Some(want) = want {
                expected["service_tier"] = json!(want);
            }
            expected["input"] = json!([{"type": "message", "role": "user", "content": "hello"}]);
            expected["stream"] = json!(true);
            expected["store"] = json!(false);
            expected["parallel_tool_calls"] = json!(true);
            expected["include"] = json!(["reasoning.encrypted_content"]);
            assert_eq!(out, expected, "{name}");
        }
    }
}
