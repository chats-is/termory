//! Codex executor: the request/response handling CLIProxyAPI applies when it
//! sends an (already translated) Responses body to the ChatGPT Codex backend.
//!
//! A function-by-function port of
//! `.audit-sources/CLIProxyAPI/internal/runtime/executor/codex_executor*.go`
//! plus the `helps` / `util` / `signature` helpers those files call on the
//! request and response path. Each ported function names its Go source.
//!
//! Configuration is fixed at CLIProxyAPI's DEFAULTS (`config.example.yaml`):
//! cloaking ON (`disable-codex-cloaking: false`), identity-confuse OFF,
//! stream-bootstrap-buffering OFF, optimize-multi-agent-v2 OFF, no auth
//! `header:` rules, no codex-api-key `is-compat` model. The credential is
//! always the ChatGPT OAuth login (never an API key), and the client dialect
//! is OpenAI Responses (`openai-response`).
//!
//! Deliberately NOT ported (out of scope): websockets, image generation
//! (`ensureImageGenerationTool` — see `ensure_image_generation_tool` below,
//! ported but NOT applied by `build_request`), `/responses/compact`, quota /
//! usage reporting / logging, plugin and home hooks, the retry loop, the
//! request translators, thinking-suffix parsing, and the Claude-only
//! reasoning replay cache.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `codexUserAgent` (codex_executor_request.go). Cloaking forces it on every
/// request regardless of what the client sent.
pub const CODEX_USER_AGENT: &str =
    "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)";
/// `codexOriginator` (codex_executor_request.go).
pub const CODEX_ORIGINATOR: &str = "codex-tui";
/// The executor's fallback when the auth carries no `base_url`.
pub const CODEX_DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// `codexIncompleteStreamMessage` (codex_executor_terminal.go).
pub const CODEX_INCOMPLETE_STREAM_MESSAGE: &str =
    "stream error: stream disconnected before completion: stream closed before response.completed";
/// `helps.CodexEmptyIncompleteStreamMessage` (helps/codex_terminal_incomplete.go).
pub const CODEX_EMPTY_INCOMPLETE_STREAM_MESSAGE: &str =
    "stream error: upstream terminated with incomplete empty response (0 tokens)";

const CODEX_RESPONSES_LITE_HEADER: &str = "X-OpenAI-Internal-Codex-Responses-Lite";
const CODEX_ROUTING_HINT_HEADER: &str = "X-Codex-Routing-Hint";

/// Inputs the executor needs about the credential.
pub struct CodexCredential<'a> {
    pub access_token: &'a str,
    pub account_id: Option<&'a str>,
}

/// Port of the executor's `statusErr` plus the two request-scoped stream
/// error wrappers (`codexIncompleteStreamError`,
/// `codexEmptyIncompleteStreamError`). `message` is what Go's `Error()`
/// returns — the (classified) error body for upstream failures.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
    /// `IsRequestScoped()`: the failure belongs to this request, not the credential.
    pub request_scoped: bool,
    /// `IsCredentialScoped()`: a usage-limit exhaustion of this credential.
    pub credential_scoped: bool,
    pub retry_after: Option<Duration>,
}

// ---------------------------------------------------------------------------
// gjson / sjson equivalents over serde_json::Value (preserve_order).
// ---------------------------------------------------------------------------

/// gjson `Get` for a plain dotted path (no wildcards / escapes needed here).
fn jget<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
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
fn jstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// gjson `Result.Int()`.
fn jint(v: Option<&Value>) -> i64 {
    match v {
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
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// gjson `Result.Bool()`.
fn jbool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "t" | "T" | "true" | "TRUE" | "True"),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

/// gjson `IsArray() && len(Array()) > 0`.
fn non_empty_array(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Array(a)) if !a.is_empty())
}

/// sjson `Set`: replaces an existing key in place, appends a new one, and
/// creates missing (or null) intermediate objects. A non-object on the way
/// is left alone (sjson refuses those paths).
fn jset(root: &mut Value, path: &str, value: Value) {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        if cur.is_null() {
            *cur = Value::Object(Map::new());
        }
        let last = i + 1 == segs.len();
        match cur {
            Value::Object(m) => {
                if last {
                    m.insert((*seg).to_string(), value);
                    return;
                }
                cur = m.entry((*seg).to_string()).or_insert(Value::Null);
            }
            Value::Array(a) => {
                let Some(slot) = seg.parse::<usize>().ok().and_then(|ix| a.get_mut(ix)) else {
                    return;
                };
                if last {
                    *slot = value;
                    return;
                }
                cur = slot;
            }
            _ => return,
        }
    }
}

/// sjson `Delete` on a top-level key, keeping the remaining key order.
fn jdel(root: &mut Value, key: &str) -> bool {
    match root {
        Value::Object(m) => m.shift_remove(key).is_some(),
        _ => false,
    }
}

/// Port of SetStringIfDifferent (helps/payload_mutations.go).
fn set_string_if_different(body: &mut Value, key: &str, value: &str) {
    if let Some(Value::String(cur)) = jget(body, key) {
        if cur == value {
            return;
        }
    }
    jset(body, key, Value::String(value.to_string()));
}

/// Port of SetBoolIfDifferent (helps/payload_mutations.go).
fn set_bool_if_different(body: &mut Value, key: &str, value: bool) {
    if jget(body, key) == Some(&Value::Bool(value)) {
        return;
    }
    jset(body, key, Value::Bool(value));
}

// ---------------------------------------------------------------------------
// Header maps.
// ---------------------------------------------------------------------------

/// The incoming client headers (lowercase names). Go's `Header.Get`
/// canonicalizes, so lookup is case-insensitive and returns the FIRST value.
struct ClientHeaders<'a>(&'a [(String, String)]);

impl ClientHeaders<'_> {
    fn get(&self, key: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

/// The outgoing header map: insertion-ordered, case-insensitive like Go's
/// canonical `http.Header`, `set` replacing in place.
#[derive(Default)]
struct Headers(Vec<(String, String)>);

impl Headers {
    fn get(&self, key: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
    fn set(&mut self, key: &str, value: &str) {
        if let Some(pos) = self.0.iter().position(|(k, _)| k.eq_ignore_ascii_case(key)) {
            self.0[pos].1 = value.to_string();
            let mut i = pos + 1;
            while i < self.0.len() {
                if self.0[i].0.eq_ignore_ascii_case(key) {
                    self.0.remove(i);
                } else {
                    i += 1;
                }
            }
        } else {
            self.0.push((key.to_string(), value.to_string()));
        }
    }
    fn del(&mut self, key: &str) {
        self.0.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
    }
}

/// Port of misc.EnsureHeader (misc/header_utils.go).
fn ensure_header(target: &mut Headers, source: &ClientHeaders, key: &str, default_value: &str) {
    let val = source.get(key).trim();
    if !val.is_empty() {
        target.set(key, val);
        return;
    }
    if !target.get(key).trim().is_empty() {
        return;
    }
    let val = default_value.trim();
    if !val.is_empty() {
        target.set(key, val);
    }
}

/// Port of ensureHeaderWithConfigPrecedence (codex_websockets_request.go).
fn ensure_header_with_config_precedence(
    target: &mut Headers,
    source: &ClientHeaders,
    key: &str,
    config_value: &str,
    fallback_value: &str,
) {
    if !target.get(key).trim().is_empty() {
        return;
    }
    for val in [
        config_value.trim(),
        source.get(key).trim(),
        fallback_value.trim(),
    ] {
        if !val.is_empty() {
            target.set(key, val);
            return;
        }
    }
}

/// Port of codexSessionHeaderValue (codex_websockets_request.go).
fn codex_session_header_value(headers: &Headers) -> String {
    for key in ["Session-Id", "Session_id", "session_id"] {
        let value = headers.get(key).trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Request building.
// ---------------------------------------------------------------------------

/// Port of util.IsCodexResponsesLiteRequest (util/codex.go). For a Responses
/// client this is also `helps.IsNativeCodexRequest` (helps/codex_native.go),
/// whose format check always passes for `openai-response`.
pub fn is_codex_responses_lite_request(body: &Value, client_headers: &[(String, String)]) -> bool {
    let headers = ClientHeaders(client_headers);
    if headers
        .get(CODEX_RESPONSES_LITE_HEADER)
        .trim()
        .eq_ignore_ascii_case("true")
    {
        return true;
    }
    match jget(
        body,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    ) {
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Port of normalizeCodexInstructions (codex_executor_request.go).
fn normalize_codex_instructions(body: &mut Value, native_request: bool) {
    if native_request {
        return;
    }
    match jget(body, "instructions") {
        None | Some(Value::Null) => jset(body, "instructions", Value::String(String::new())),
        _ => {}
    }
}

/// Port of normalizeCodexParallelToolCalls (codex_executor_request.go).
fn normalize_codex_parallel_tool_calls(body: &mut Value, lite: bool) {
    if lite {
        set_bool_if_different(body, "parallel_tool_calls", false);
        return;
    }
    normalize_codex_parallel_tool_calls_for_tools(body);
}

/// Port of normalizeCodexParallelToolCallsForTools (codex_executor_request.go).
fn normalize_codex_parallel_tool_calls_for_tools(body: &mut Value) {
    if jget(body, "parallel_tool_calls").is_none() {
        return;
    }
    if non_empty_array(jget(body, "tools")) {
        return;
    }
    jdel(body, "parallel_tool_calls");
}

/// Port of isImageGenerationFunctionTool (codex_executor_request.go).
#[allow(dead_code)] // ported for parity; not used by the router (image generation is out of scope / the detailed variant is used)
fn is_image_generation_function_tool(tool: &Value) -> bool {
    match jstr(jget(tool, "type")).as_str() {
        "function" => jstr(jget(tool, "name")) == "image_gen.imagegen",
        "namespace" => {
            if jstr(jget(tool, "name")) != "image_gen" {
                return false;
            }
            let Some(Value::Array(tools)) = jget(tool, "tools") else {
                return false;
            };
            tools.iter().any(|nested| {
                jstr(jget(nested, "type")) == "function" && jstr(jget(nested, "name")) == "imagegen"
            })
        }
        _ => false,
    }
}

/// Port of ensureImageGenerationTool (codex_executor_request.go).
///
/// NOT applied by `build_request` (image generation is out of scope). Go runs
/// it by default (`disable-image-generation` off) for every non-`spark` model
/// on a non-free plan; a caller that wants that parity calls it on the body
/// before `build_request`.
#[allow(dead_code)] // ported for parity; not used by the router (image generation is out of scope / the detailed variant is used)
pub fn ensure_image_generation_tool(
    body: &mut Value,
    base_model: &str,
    is_free_plan: bool,
    client_headers: &[(String, String)],
) {
    if is_codex_responses_lite_request(body, client_headers) {
        return;
    }
    if base_model.ends_with("spark") || is_free_plan {
        return;
    }
    let image_tool = json!({"type":"image_generation","output_format":"png"});
    match body.get_mut("tools") {
        Some(Value::Array(tools)) => {
            if tools.iter().any(|t| {
                jstr(jget(t, "type")) == "image_generation" || is_image_generation_function_tool(t)
            }) {
                return;
            }
            tools.push(image_tool);
        }
        _ => jset(body, "tools", Value::Array(vec![image_tool])),
    }
}

/// Port of signature.InspectGPTReasoningSignature (signature/gpt_validation.go).
fn inspect_gpt_reasoning_signature(raw_signature: &str) -> Result<(), String> {
    use base64::alphabet::URL_SAFE;
    use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
    use base64::engine::DecodePaddingMode;
    use base64::Engine;

    const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return Err("empty GPT reasoning signature".into());
    }
    if sig.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return Err(format!(
            "GPT reasoning signature exceeds maximum length ({MAX_GPT_REASONING_SIGNATURE_LEN} bytes)"
        ));
    }
    if !sig.starts_with("gAAAA") {
        return Err("invalid GPT reasoning signature: expected gAAAA prefix".into());
    }
    if let Some((index, ch)) = sig
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=')))
    {
        return Err(format!(
            "invalid GPT reasoning signature: contains non-base64url character U+{:04X} at byte {index}",
            ch as u32
        ));
    }
    // Go's base64 decoders are non-strict about trailing bits.
    let raw_url = GeneralPurpose::new(
        &URL_SAFE,
        GeneralPurposeConfig::new()
            .with_encode_padding(false)
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    let url = GeneralPurpose::new(
        &URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let decoded = raw_url
        .decode(sig)
        .or_else(|_| url.decode(sig))
        .map_err(|_| "invalid GPT reasoning signature: base64url decode failed".to_string())?;
    if decoded.len() < 73 {
        return Err("invalid GPT reasoning signature: decoded payload too short".into());
    }
    if decoded[0] != 0x80 {
        return Err(format!(
            "invalid GPT reasoning signature: expected version 0x80, got 0x{:02x}",
            decoded[0]
        ));
    }
    let ciphertext_len = decoded.len() as i64 - 1 - 8 - 16 - 32;
    if ciphertext_len <= 0 || ciphertext_len % 16 != 0 {
        return Err(format!(
            "invalid GPT reasoning signature: ciphertext length {ciphertext_len} is not a positive AES block multiple"
        ));
    }
    Ok(())
}

/// Port of openaiResponsesReasoningSummaryIsEmpty (openai_responses_signature.go).
fn openai_responses_reasoning_summary_is_empty(summary: Option<&Value>) -> bool {
    match summary {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// Port of promoteOpenAIResponsesReasoningTextToSummary (openai_responses_signature.go).
fn promote_openai_responses_reasoning_text_to_summary(item: &mut Value, content: &[Value]) {
    let parts: Vec<Value> = content
        .iter()
        .filter(|part| jstr(jget(part, "type")).trim() == "reasoning_text")
        .filter_map(|part| {
            let text = jstr(jget(part, "text"));
            (!text.is_empty()).then(|| json!({"type":"summary_text","text":text}))
        })
        .collect();
    if parts.is_empty() {
        return;
    }
    jset(item, "summary", Value::Array(parts));
}

/// Port of sanitizeOpenAIResponsesReasoningEncryptedContentWithCompat
/// (openai_responses_signature.go).
fn sanitize_openai_responses_reasoning_encrypted_content(body: &mut Value, is_compat: bool) {
    // Codex rejects store=true and persists nothing under store=false, so a
    // reasoning id without usable encrypted_content reads as a store lookup.
    let strip_orphan_reasoning_ids = !jbool(jget(body, "store"));
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return;
    };
    for item in items.iter_mut() {
        if jstr(jget(item, "type")).trim() != "reasoning" {
            continue;
        }
        let had_id = jget(item, "id").is_some();

        // Official Codex sets maxItems: 0 on reasoning.content: promote
        // cleartext thinking into an empty summary, then force content to [].
        if !is_compat {
            if let Some(Value::Array(content)) = jget(item, "content") {
                if !content.is_empty() {
                    let content = content.clone();
                    if openai_responses_reasoning_summary_is_empty(jget(item, "summary")) {
                        promote_openai_responses_reasoning_text_to_summary(item, &content);
                    }
                    jset(item, "content", Value::Array(Vec::new()));
                }
            }
        }

        let invalid = match jget(item, "encrypted_content") {
            None => {
                if !is_compat && strip_orphan_reasoning_ids && had_id {
                    jdel(item, "id");
                }
                continue;
            }
            Some(Value::String(raw)) => {
                raw.as_str() != raw.trim() || inspect_gpt_reasoning_signature(raw).is_err()
            }
            Some(_) => true,
        };
        if !invalid {
            continue;
        }
        jdel(item, "encrypted_content");
        if !is_compat && strip_orphan_reasoning_ids && had_id {
            jdel(item, "id");
        }
    }
}

// --- helps/codex_input_ids.go ---------------------------------------------

const CODEX_INPUT_ITEM_ID_LIMIT: usize = 64;
const CODEX_INPUT_ITEM_ID_OCCUPIED: u8 = 1 << 0;
const CODEX_INPUT_ITEM_ID_PRESERVED: u8 = 1 << 1;

fn rune_len(s: &str) -> usize {
    s.chars().count()
}

/// Port of normalizeCodexInputItemID (helps/codex_input_ids.go).
fn normalize_codex_input_item_id(item: &Value, id: &str) -> String {
    let prefix = match jstr(jget(item, "type")).as_str() {
        "message" => "msg",
        "reasoning" => "rs",
        "function_call" => "fc",
        "custom_tool_call" => "ctc",
        "custom_tool_call_output" => "ctco",
        _ => return id.to_string(),
    };
    if id.is_empty() || id.starts_with(prefix) {
        return id.to_string();
    }
    format!("{prefix}_{id}")
}

/// Port of shouldDropCodexEncryptedReasoningItem (helps/codex_input_ids.go).
fn should_drop_codex_encrypted_reasoning_item(item: &Value) -> bool {
    if jstr(jget(item, "type")) != "reasoning" {
        return false;
    }
    match jget(item, "id") {
        Some(Value::String(id)) if rune_len(id) > CODEX_INPUT_ITEM_ID_LIMIT => {}
        _ => return false,
    }
    matches!(jget(item, "encrypted_content"), Some(Value::String(s)) if !s.is_empty())
}

/// Port of codexInputItemIDWithHashSuffixRunes (helps/codex_input_ids.go).
fn codex_input_item_id_with_hash_suffix(id: &str, attempt: usize) -> String {
    use sha2::{Digest, Sha256};
    let mut hash_input = id.to_string();
    if attempt > 0 {
        hash_input.push('\0');
        hash_input.push_str(&attempt.to_string());
    }
    let sum = Sha256::digest(hash_input.as_bytes());
    let hex: String = sum[..8].iter().map(|b| format!("{b:02x}")).collect();
    let suffix = format!("_{hex}");
    let prefix_length = (CODEX_INPUT_ITEM_ID_LIMIT - suffix.len()).min(rune_len(id));
    let prefix: String = id.chars().take(prefix_length).collect();
    prefix + &suffix
}

/// Port of shortenCodexInputItemIDWithAttempt (helps/codex_input_ids.go).
fn shorten_codex_input_item_id_with_attempt(id: &str, attempt: usize) -> String {
    if rune_len(id) <= CODEX_INPUT_ITEM_ID_LIMIT {
        return id.to_string();
    }
    codex_input_item_id_with_hash_suffix(id, attempt)
}

/// Port of SanitizeCodexInputItemIDs (helps/codex_input_ids.go): prefix the
/// supported item ids, drop encrypted reasoning items whose id exceeds the
/// limit, and deterministically shorten every other overlong id.
fn sanitize_codex_input_item_ids(body: &mut Value) {
    let Some(Value::Array(items)) = jget(body, "input") else {
        return;
    };
    let items = items.clone();

    let mut id_states: HashMap<String, u8> = HashMap::new();
    for item in &items {
        if should_drop_codex_encrypted_reasoning_item(item) {
            continue;
        }
        let Some(Value::String(original)) = jget(item, "id") else {
            continue;
        };
        let id = normalize_codex_input_item_id(item, original);
        let mut state = *id_states.get(&id).unwrap_or(&0);
        if &id == original {
            state |= CODEX_INPUT_ITEM_ID_PRESERVED;
        }
        if rune_len(&id) <= CODEX_INPUT_ITEM_ID_LIMIT {
            state |= CODEX_INPUT_ITEM_ID_OCCUPIED;
        }
        if state != 0 {
            id_states.insert(id, state);
        }
    }

    let mut mapped: HashMap<String, String> = HashMap::new();
    let mut collision_mapped: HashMap<String, String> = HashMap::new();
    let mut rebuilt = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in items {
        if should_drop_codex_encrypted_reasoning_item(&item) {
            changed = true;
            continue;
        }
        let mut item = item;
        if let Some(Value::String(original)) = jget(&item, "id") {
            let original = original.clone();
            let mut id = normalize_codex_input_item_id(&item, &original);
            if id != original
                && id_states.get(&id).copied().unwrap_or(0) & CODEX_INPUT_ITEM_ID_PRESERVED != 0
            {
                id = match collision_mapped.get(&id) {
                    Some(c) => c.clone(),
                    None => {
                        let mut attempt = 0;
                        let collision_id = loop {
                            let candidate = codex_input_item_id_with_hash_suffix(&id, attempt);
                            if id_states.get(&candidate).copied().unwrap_or(0)
                                & CODEX_INPUT_ITEM_ID_OCCUPIED
                                != 0
                            {
                                attempt += 1;
                                continue;
                            }
                            break candidate;
                        };
                        collision_mapped.insert(id.clone(), collision_id.clone());
                        *id_states.entry(collision_id.clone()).or_insert(0) |=
                            CODEX_INPUT_ITEM_ID_OCCUPIED;
                        collision_id
                    }
                };
            }
            if rune_len(&id) > CODEX_INPUT_ITEM_ID_LIMIT {
                id = match mapped.get(&id) {
                    Some(s) => s.clone(),
                    None => {
                        let mut shortened = shorten_codex_input_item_id_with_attempt(&id, 0);
                        let mut attempt = 1;
                        while id_states.get(&shortened).copied().unwrap_or(0)
                            & CODEX_INPUT_ITEM_ID_OCCUPIED
                            != 0
                        {
                            shortened = shorten_codex_input_item_id_with_attempt(&id, attempt);
                            attempt += 1;
                        }
                        mapped.insert(id.clone(), shortened.clone());
                        *id_states.entry(shortened.clone()).or_insert(0) |=
                            CODEX_INPUT_ITEM_ID_OCCUPIED;
                        shortened
                    }
                };
            }
            if id != original {
                jset(&mut item, "id", Value::String(id));
                changed = true;
            }
        }
        rebuilt.push(item);
    }
    if changed {
        jset(body, "input", Value::Array(rebuilt));
    }
}

// --- helps/codex_tool_schema.go -------------------------------------------

/// `util.SchemaMapKeywords` (util/claude_schema.go).
const SCHEMA_MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];
/// `util.SchemaValueKeywords` (util/claude_schema.go).
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
/// `codexComplexUnionBranchThreshold` (helps/codex_tool_schema.go).
const CODEX_COMPLEX_UNION_BRANCH_THRESHOLD: usize = 8;

/// Port of util.HasUnsupportedUnicodePropertyEscape (util/claude_schema.go).
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
        i += 2;
    }
    false
}

/// Port of stripIncompatiblePatterns (helps/codex_tool_schema.go). Schema
/// aware: only subschemas under JSON Schema keyword locations are visited.
fn strip_incompatible_patterns(v: &mut Value) -> bool {
    let mut changed = false;
    match v {
        Value::Object(schema) => {
            if let Some(Value::String(p)) = schema.get("pattern") {
                if has_unsupported_unicode_property_escape(p) {
                    schema.shift_remove("pattern");
                    changed = true;
                }
            }
            if let Some(Value::Object(pattern_props)) = schema.get_mut("patternProperties") {
                let keys: Vec<String> = pattern_props.keys().cloned().collect();
                for key in keys {
                    if has_unsupported_unicode_property_escape(&key) {
                        pattern_props.shift_remove(&key);
                        changed = true;
                    } else if let Some(sub) = pattern_props.get_mut(&key) {
                        changed |= strip_incompatible_patterns(sub);
                    }
                }
            }
            for map_key in SCHEMA_MAP_KEYWORDS {
                if map_key == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = schema.get_mut(map_key) {
                    for sub in sub_map.values_mut() {
                        changed |= strip_incompatible_patterns(sub);
                    }
                }
            }
            for val_key in SCHEMA_VALUE_KEYWORDS {
                match schema.get_mut(val_key) {
                    Some(sub @ Value::Object(_)) => changed |= strip_incompatible_patterns(sub),
                    Some(Value::Array(subs)) => {
                        for sub in subs.iter_mut() {
                            changed |= strip_incompatible_patterns(sub);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                changed |= strip_incompatible_patterns(item);
            }
        }
        _ => {}
    }
    changed
}

/// Canonical decimal for a JSON number literal, so `1` and `1.0` and `1e0`
/// compare equal — the role `big.Rat.RatString` plays in Go.
fn canonical_number(raw: &str) -> String {
    let raw = raw.trim();
    let (neg, rest) = match raw.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    };
    let (mantissa, exp) = match rest.find(['e', 'E']) {
        Some(ix) => (&rest[..ix], rest[ix + 1..].parse::<i64>().unwrap_or(0)),
        None => (rest, 0),
    };
    let (int_part, frac_part) = match mantissa.find('.') {
        Some(ix) => (&mantissa[..ix], &mantissa[ix + 1..]),
        None => (mantissa, ""),
    };
    let mut digits = format!("{int_part}{frac_part}");
    let mut exp = exp - frac_part.len() as i64;
    let trimmed = digits.trim_start_matches('0').to_string();
    digits = trimmed;
    if digits.is_empty() {
        return "0".to_string();
    }
    while digits.ends_with('0') {
        digits.pop();
        exp += 1;
    }
    format!("{}{digits}e{exp}", if neg { "-" } else { "" })
}

/// Port of canonicalJSONValueKey (helps/codex_tool_schema.go).
fn canonical_json_value_key(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(format!("s:{s}")),
        Value::Number(n) => Some(format!("n:{}", canonical_number(&n.to_string()))),
        Value::Bool(true) => Some("b:true".into()),
        Value::Bool(false) => Some("b:false".into()),
        Value::Null => Some("null".into()),
        _ => None,
    }
}

/// Port of isPureConstBranch (helps/codex_tool_schema.go).
fn is_pure_const_branch(branch: &Value) -> Option<(String, Value)> {
    let Value::Object(m) = branch else {
        return None;
    };
    let const_val = m.get("const")?;
    if m.keys()
        .any(|k| k != "const" && k != "description" && k != "title")
    {
        return None;
    }
    let key = canonical_json_value_key(const_val)?;
    Some((key, const_val.clone()))
}

/// Port of equalCanonicalSets (helps/codex_tool_schema.go).
fn equal_canonical_sets(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let set_a: HashSet<&String> = a.iter().collect();
    b.iter().all(|v| set_a.contains(v)) && set_a.len() == a.len()
}

/// Port of normalizeCodexPropertySchema (helps/codex_tool_schema.go): turn a
/// large pure-const oneOf/anyOf into an equivalent `enum`.
fn normalize_codex_property_schema(prop: &mut Value) -> bool {
    let Value::Object(m) = prop else {
        return false;
    };
    let has_one_of = m.contains_key("oneOf");
    let has_any_of = m.contains_key("anyOf");
    let union_name = match (has_one_of, has_any_of) {
        (true, true) | (false, false) => return false,
        (true, false) => "oneOf",
        (false, true) => "anyOf",
    };
    let Some(Value::Array(branches)) = m.get(union_name) else {
        return false;
    };
    if branches.len() < CODEX_COMPLEX_UNION_BRANCH_THRESHOLD {
        return false;
    }
    let mut const_values = Vec::with_capacity(branches.len());
    let mut const_keys = Vec::with_capacity(branches.len());
    let mut seen = HashSet::new();
    for branch in branches {
        let Some((key, raw)) = is_pure_const_branch(branch) else {
            return false;
        };
        if !seen.insert(key.clone()) {
            // A duplicate semantic value violates oneOf exclusivity.
            return false;
        }
        const_keys.push(key);
        const_values.push(raw);
    }
    if const_values.is_empty() {
        return false;
    }
    if let Some(Value::Array(existing)) = m.get("enum") {
        let mut existing_keys = Vec::with_capacity(existing.len());
        for v in existing {
            match canonical_json_value_key(v) {
                Some(k) => existing_keys.push(k),
                None => return false,
            }
        }
        if equal_canonical_sets(&existing_keys, &const_keys) {
            m.shift_remove(union_name);
            return true;
        }
        return false;
    }
    m.insert("enum".to_string(), Value::Array(const_values));
    m.shift_remove(union_name);
    true
}

/// Port of normalizeCodexParameters (helps/codex_tool_schema.go).
fn normalize_codex_parameters(params: &mut Value) -> bool {
    let mut changed = strip_incompatible_patterns(params);
    if let Some(Value::Object(props)) = params.get_mut("properties") {
        for prop in props.values_mut() {
            changed |= normalize_codex_property_schema(prop);
        }
    }
    changed
}

/// Port of normalizeCodexTool (helps/codex_tool_schema.go).
fn normalize_codex_tool(tool: &mut Value) -> bool {
    let tool_type = jstr(jget(tool, "type"));
    if tool_type == "namespace" {
        return match tool.get_mut("tools") {
            Some(list) => normalize_codex_tool_list(list),
            None => false,
        };
    }
    if tool_type != "function" && tool_type != "custom" {
        return false;
    }
    match tool.get_mut("parameters") {
        Some(params @ Value::Object(_)) => normalize_codex_parameters(params),
        _ => false,
    }
}

/// Port of normalizeCodexToolList (helps/codex_tool_schema.go).
fn normalize_codex_tool_list(tools: &mut Value) -> bool {
    let Value::Array(list) = tools else {
        return false;
    };
    let mut changed = false;
    for tool in list.iter_mut() {
        changed |= normalize_codex_tool(tool);
    }
    changed
}

/// Port of NormalizeCodexToolSchemas (helps/codex_tool_schema.go).
fn normalize_codex_tool_schemas(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools") {
        normalize_codex_tool_list(tools);
    }
}

// --- headers ---------------------------------------------------------------

/// Port of applyCodexHeadersFromSources (codex_executor_request.go) for an
/// OAuth credential, followed by applyCodexCloakingHeaders under the default
/// config (cloaking on) and no auth `header:` attributes.
fn apply_codex_headers_from_sources(
    headers: &mut Headers,
    cred: &CodexCredential,
    stream: bool,
    client: &ClientHeaders,
) {
    headers.set("Content-Type", "application/json");
    if !cred.access_token.trim().is_empty() {
        headers.set("Authorization", &format!("Bearer {}", cred.access_token));
    } else {
        headers.del("Authorization");
    }

    let beta = client.get("X-Codex-Beta-Features");
    if !beta.is_empty() {
        headers.set("X-Codex-Beta-Features", beta);
    }
    for key in [
        "Version",
        "X-Codex-Turn-Metadata",
        "X-Codex-Turn-State",
        "X-Client-Request-Id",
        "X-Codex-Window-Id",
        "Thread-Id",
        "Session-Id",
        "X-Openai-Internal-Codex-Responses-Lite",
    ] {
        ensure_header(headers, client, key, "");
    }

    // codexHeaderDefaults: `codex-header-defaults.user-agent` is empty by default.
    ensure_header_with_config_precedence(headers, client, "User-Agent", "", CODEX_USER_AGENT);

    headers.set(
        "Accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    headers.set("Connection", "Keep-Alive");

    let originator = client.get("Originator").trim();
    if !originator.is_empty() {
        headers.set("Originator", originator);
    } else {
        headers.set("Originator", CODEX_ORIGINATOR);
    }
    if let Some(account_id) = cred.account_id {
        headers.set("Chatgpt-Account-Id", account_id);
    }
    apply_codex_cloaking_headers(headers);
}

/// Port of applyCodexCloakingHeaders (codex_executor_request.go) with
/// cloaking enabled (the default).
fn apply_codex_cloaking_headers(headers: &mut Headers) {
    headers.set("User-Agent", CODEX_USER_AGENT);
    headers.set("Originator", CODEX_ORIGINATOR);
}

/// Port of applyCodexRoutingHint (codex_executor_request.go) for an OAuth
/// credential with no operator `header:` rule: "model=<slug>" plus
/// ";tier=<service_tier>" read from the FINAL upstream body. A hint the
/// client forwarded is never passed through.
fn apply_codex_routing_hint(headers: &mut Headers, base_model: &str, upstream_body: &Value) {
    headers.del(CODEX_ROUTING_HINT_HEADER);
    let model = base_model.trim();
    if model.is_empty() {
        return;
    }
    let mut hint = format!("model={model}");
    if let Some(Value::String(tier)) = jget(upstream_body, "service_tier") {
        let tier = tier.trim();
        if !tier.is_empty() {
            hint.push_str(";tier=");
            hint.push_str(tier);
        }
    }
    headers.set(CODEX_ROUTING_HINT_HEADER, &hint);
}

/// `config.override_header` entries from the bundled
/// `internal/registry/models/models.json` (codex-free / -team / -plus / -pro).
/// Only `gpt-5.6-luna` carries one.
fn model_override_headers(model: &str) -> &'static [(&'static str, &'static str)] {
    match model {
        "gpt-5.6-luna" => &[
            ("user-agent", CODEX_USER_AGENT),
            ("originator", CODEX_ORIGINATOR),
        ],
        _ => &[],
    }
}

/// Port of applyModelHeaderOverrides (codex_executor_request.go).
fn apply_model_header_overrides(headers: &mut Headers, model: &str) {
    let overrides = model_override_headers(model);
    if overrides.is_empty() {
        return;
    }
    for (key, value) in overrides {
        headers.set(key, value);
    }
    if headers.get("User-Agent").contains("Mac OS")
        && codex_session_header_value(headers).is_empty()
    {
        headers.set("Session_id", &random_uuid_v4());
    }
}

/// `uuid.NewString()`: a random v4 UUID (std-only entropy; the module adds no crate).
fn random_uuid_v4() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut bytes = [0u8; 16];
    for (half, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_usize(half);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Build the upstream request: returns (url, headers as Vec<(String,String)>, body bytes).
///
/// Port of the request half of `Execute` (codex_executor_execute.go) and
/// `ExecuteStream` (codex_executor_stream.go), plus `cacheHelper`
/// (codex_executor_request.go) for an `openai-response` client. The two Go
/// paths differ in one body rule (`stream_options.reasoning_summary_delivery`
/// survives only on the stream path); the client's own `"stream": true` picks
/// the path, the way CLIProxyAPI's Responses handler derives `opts.Stream`.
///
/// `codex_version` is accepted for the caller's convenience but unused: Go
/// sends the hardcoded `codexUserAgent` (cloaking) and only forwards a
/// client `Version` header — it never derives either from an installed CLI.
pub fn build_request(
    base_url: &str,
    body: &Value,
    cred: &CodexCredential,
    client_headers: &[(String, String)],
    codex_version: &str,
) -> (String, Vec<(String, String)>, Vec<u8>) {
    let _ = codex_version;
    let client = ClientHeaders(client_headers);
    let client_stream = jget(body, "stream") == Some(&Value::Bool(true));
    let native_request = is_codex_responses_lite_request(body, client_headers);
    // baseModel: thinking-suffix parsing is out of scope, so the body's model as-is.
    let base_model = match jget(body, "model") {
        Some(Value::String(m)) => m.clone(),
        _ => String::new(),
    };

    let mut out = body.clone();
    if client_stream {
        for key in [
            "previous_response_id",
            "generate",
            "prompt_cache_retention",
            "safety_identifier",
        ] {
            jdel(&mut out, key);
        }
        let delivery = jget(&out, "stream_options.reasoning_summary_delivery").cloned();
        jdel(&mut out, "stream_options");
        if let Some(delivery) = delivery {
            jset(
                &mut out,
                "stream_options.reasoning_summary_delivery",
                delivery,
            );
        }
        if !base_model.is_empty() {
            set_string_if_different(&mut out, "model", &base_model);
        }
    } else {
        if !base_model.is_empty() {
            set_string_if_different(&mut out, "model", &base_model);
        }
        set_bool_if_different(&mut out, "stream", true);
        for key in [
            "previous_response_id",
            "generate",
            "prompt_cache_retention",
            "safety_identifier",
            "stream_options",
        ] {
            jdel(&mut out, key);
        }
    }
    normalize_codex_instructions(&mut out, native_request);
    sanitize_openai_responses_reasoning_encrypted_content(&mut out, false);
    normalize_codex_parallel_tool_calls(&mut out, native_request);
    normalize_codex_tool_schemas(&mut out);

    let base_url = if base_url.is_empty() {
        CODEX_DEFAULT_BASE_URL
    } else {
        base_url
    };
    // strings.TrimSuffix: at most ONE trailing slash.
    let url = format!(
        "{}/responses",
        base_url.strip_suffix('/').unwrap_or(base_url)
    );

    // cacheHelper, openai-response source: prompt_cache_key of the client
    // payload becomes the cache id and the Session-Id header.
    let mut headers = Headers::default();
    let cache_id = match jget(body, "prompt_cache_key") {
        Some(v) => jstr(Some(v)),
        None => String::new(),
    };
    if !cache_id.is_empty() {
        set_string_if_different(&mut out, "prompt_cache_key", &cache_id);
    }
    sanitize_codex_input_item_ids(&mut out);
    if !cache_id.is_empty() {
        headers.set("Session-Id", &cache_id);
    }

    // Both Go paths call applyCodexHeaders with stream=true.
    apply_codex_headers_from_sources(&mut headers, cred, true, &client);
    apply_codex_routing_hint(&mut headers, &base_model, &out);
    apply_model_header_overrides(&mut headers, &base_model);

    let bytes = serde_json::to_vec(&out).unwrap_or_default();
    (url, headers.0, bytes)
}

// ---------------------------------------------------------------------------
// Terminal events and error classification (codex_executor_terminal.go).
// ---------------------------------------------------------------------------

/// Port of HasMeaningfulCodexOutputDelta (helps/codex_terminal_incomplete.go).
fn has_meaningful_codex_output_delta(event: &Value) -> bool {
    match jstr(jget(event, "type")).as_str() {
        "response.output_text.delta"
        | "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta"
        | "response.function_call_arguments.delta" => match jget(event, "delta") {
            Some(d) => !jstr(Some(d)).trim().is_empty(),
            None => false,
        },
        _ => false,
    }
}

/// Port of IsCodexTerminalEmptyIncomplete (helps/codex_terminal_incomplete.go).
fn is_codex_terminal_empty_incomplete(
    event: &Value,
    output_items_count: usize,
    saw_output_delta: bool,
) -> bool {
    if jstr(jget(event, "type")) != "response.incomplete" {
        return false;
    }
    if saw_output_delta || output_items_count > 0 {
        return false;
    }
    if non_empty_array(jget(event, "response.output")) {
        return false;
    }
    // Explicit integer zero only: missing, null, 0.5 or 0.0 do not count.
    match jget(event, "response.usage.output_tokens") {
        Some(Value::Number(n)) => n.as_u64() == Some(0) || n.as_i64() == Some(0),
        _ => false,
    }
}

/// Port of collectCodexOutputItemDone (codex_executor_terminal.go).
fn collect_codex_output_item_done(
    event: &Value,
    by_index: &mut BTreeMap<i64, Value>,
    fallback: &mut Vec<Value>,
) {
    let item = match jget(event, "item") {
        Some(item @ (Value::Object(_) | Value::Array(_))) => item.clone(),
        _ => return,
    };
    match jget(event, "output_index") {
        Some(ix) => {
            by_index.insert(jint(Some(ix)), item);
        }
        None => fallback.push(item),
    }
}

/// Port of hydrateCodexCompletedOutputItemIDs (codex_executor_terminal.go).
fn hydrate_codex_completed_output_item_ids(event: &mut Value, by_index: &BTreeMap<i64, Value>) {
    let Some(Value::Array(output)) = event.get_mut("response").and_then(|r| r.get_mut("output"))
    else {
        return;
    };
    for (index, item) in output.iter_mut().enumerate() {
        match jget(item, "id") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s.trim().is_empty() => {}
            Some(_) => continue,
        }
        let Some(completed) = by_index.get(&(index as i64)) else {
            continue;
        };
        let completed_id = match jget(completed, "id") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            _ => continue,
        };
        jset(item, "id", Value::String(completed_id));
    }
}

/// Port of patchCodexCompletedOutput (codex_executor_terminal.go): a terminal
/// event whose `response.output` is empty gets it rebuilt from the
/// `response.output_item.done` items (sorted by output_index, then the
/// index-less ones); a non-empty one only has missing item ids filled in.
fn patch_codex_completed_output(
    mut event: Value,
    by_index: &BTreeMap<i64, Value>,
    fallback: &[Value],
) -> Value {
    if non_empty_array(jget(&event, "response.output")) {
        hydrate_codex_completed_output_item_ids(&mut event, by_index);
        return event;
    }
    if by_index.is_empty() && fallback.is_empty() {
        return event;
    }
    let items: Vec<Value> = by_index
        .values()
        .cloned()
        .chain(fallback.iter().cloned())
        .collect();
    jset(&mut event, "response.output", Value::Array(items));
    event
}

/// Port of normalizeCodexWebsocketCompletion (codex_websockets_errors.go),
/// which the SSE stream path applies to every terminal event too.
fn normalize_codex_websocket_completion(event: &mut Value) {
    if jstr(jget(event, "type")).trim() == "response.done" {
        jset(event, "type", Value::String("response.completed".into()));
    }
}

/// Port of codexTerminalErrorBody (codex_executor_terminal.go).
fn codex_terminal_error_body(event: &Value, path: &str) -> Option<Value> {
    let error = jget(event, path)?;
    let mut body = json!({"error":{}});
    match error {
        Value::Object(_) | Value::Array(_) => body["error"] = error.clone(),
        other => {
            let message = jstr(Some(other)).trim().to_string();
            if !message.is_empty() {
                jset(&mut body, "error.message", Value::String(message));
            }
        }
    }
    let fill = |body: &mut Value, candidate: String| {
        if jstr(jget(body, "error.message")).trim().is_empty() && !candidate.is_empty() {
            jset(body, "error.message", Value::String(candidate));
        }
    };
    let from_response = jstr(jget(event, "response.error.message"))
        .trim()
        .to_string();
    fill(&mut body, from_response);
    let code = jstr(jget(&body, "error.code")).trim().to_string();
    fill(&mut body, code);
    let error_type = jstr(jget(&body, "error.type")).trim().to_string();
    fill(&mut body, error_type);
    Some(body)
}

/// Port of codexTerminalTopLevelErrorBody (codex_executor_terminal.go).
fn codex_terminal_top_level_error_body(event: &Value) -> Option<Value> {
    let message = jstr(jget(event, "message")).trim().to_string();
    let code = jstr(jget(event, "code")).trim().to_string();
    let error_type = jstr(jget(event, "error_type")).trim().to_string();
    let param = jstr(jget(event, "param")).trim().to_string();
    if message.is_empty() && code.is_empty() && error_type.is_empty() && param.is_empty() {
        return None;
    }
    let mut body = json!({"error":{}});
    for (key, value) in [
        ("error.message", &message),
        ("error.code", &code),
        ("error.type", &error_type),
        ("error.param", &param),
    ] {
        if !value.is_empty() {
            jset(&mut body, key, Value::String(value.clone()));
        }
    }
    if jstr(jget(&body, "error.message")).trim().is_empty() {
        if !code.is_empty() {
            jset(&mut body, "error.message", Value::String(code));
        } else if !error_type.is_empty() {
            jset(&mut body, "error.message", Value::String(error_type));
        }
    }
    Some(body)
}

/// Port of codexTerminalFailureBody (codex_executor_terminal.go).
fn codex_terminal_failure_body(event: &Value) -> Option<String> {
    let body = match jstr(jget(event, "type")).as_str() {
        "error" => codex_terminal_error_body(event, "error")
            .or_else(|| codex_terminal_top_level_error_body(event)),
        "response.failed" => codex_terminal_error_body(event, "response.error")
            .or_else(|| codex_terminal_error_body(event, "error")),
        _ => return None,
    };
    let mut body = body.unwrap_or_else(
        || json!({"error":{"message":"upstream stream failed without error details"}}),
    );
    if let Some(seq) = jget(event, "sequence_number") {
        let seq = jint(Some(seq));
        jset(&mut body, "sequence_number", json!(seq));
    }
    Some(body.to_string())
}

fn parse_body(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

/// Port of codexTerminalErrorIsContextLength (codex_executor_terminal.go).
fn codex_terminal_error_is_context_length(body: &Value) -> bool {
    let code = jstr(jget(body, "error.code")).trim().to_lowercase();
    let message = jstr(jget(body, "error.message")).trim().to_lowercase();
    code == "context_length_exceeded"
        || code == "context_too_large"
        || message.contains("context window")
        || message.contains("context length")
        || message.contains("too many tokens")
}

/// Port of codexTerminalFailureStatus (codex_executor_terminal.go).
fn codex_terminal_failure_status(body: &Value) -> u16 {
    for path in ["error.status_code", "error.status"] {
        let status = jint(jget(body, path));
        if (400..=599).contains(&status) {
            return status as u16;
        }
    }
    let error_type = jstr(jget(body, "error.type")).trim().to_lowercase();
    let error_code = jstr(jget(body, "error.code")).trim().to_lowercase();
    if error_code == "cyber_policy" {
        400
    } else if error_type == "not_found_error"
        || error_code == "not_found"
        || error_code == "model_not_found"
    {
        404
    } else if error_type == "authentication_error"
        || error_code == "invalid_api_key"
        || error_code == "unauthorized"
    {
        401
    } else if error_type == "permission_error"
        || error_code == "forbidden"
        || error_code == "permission_denied"
    {
        403
    } else if error_type == "rate_limit_error" || error_code == "rate_limit_exceeded" {
        429
    } else if error_type == "invalid_request_error" || error_type == "bad_request_error" {
        400
    } else {
        502
    }
}

/// Port of isCodexModelCapacityError (codex_executor_terminal.go).
fn is_codex_model_capacity_error(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    let body = parse_body(raw);
    let candidates = [
        jstr(jget(&body, "error.message")),
        jstr(jget(&body, "message")),
        raw.to_string(),
    ];
    candidates.iter().any(|candidate| {
        let lower = candidate.trim().to_lowercase();
        !lower.is_empty()
            && (lower.contains("model is at capacity")
                || lower.contains("model_at_capacity")
                || lower.contains("model_is_at_capacity")
                || (lower.contains("model") && lower.contains("at capacity")))
    })
}

/// Port of isCodexUsageLimitError (codex_executor_terminal.go).
fn is_codex_usage_limit_error(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    let body = parse_body(raw);
    [jstr(jget(&body, "error.type")), jstr(jget(&body, "type"))]
        .iter()
        .any(|c| c.trim().eq_ignore_ascii_case("usage_limit_reached"))
}

/// Port of codexStatusErrorClassification (codex_executor_terminal.go).
fn codex_status_error_classification(
    status: u16,
    raw: &str,
) -> Option<(&'static str, &'static str)> {
    let body = parse_body(raw);
    let mut message = jstr(jget(&body, "error.message")).trim().to_lowercase();
    if message.is_empty() {
        message = jstr(jget(&body, "message")).trim().to_lowercase();
    }
    let lower = raw.trim().to_lowercase();
    let upstream_code = jstr(jget(&body, "error.code")).trim().to_lowercase();
    let upstream_type = jstr(jget(&body, "error.type")).trim().to_lowercase();
    let is_invalid_request = upstream_type.is_empty() || upstream_type == "invalid_request_error";

    if status == 413
        || upstream_code == "context_length_exceeded"
        || upstream_code == "context_too_large"
        || (is_invalid_request
            && (message.contains("context length")
                || message.contains("context_length")
                || message.contains("maximum context")
                || message.contains("too many tokens")))
    {
        Some(("context_too_large", "invalid_request_error"))
    } else if lower.contains("invalid signature in thinking block")
        || lower.contains("invalid_encrypted_content")
    {
        Some(("thinking_signature_invalid", "invalid_request_error"))
    } else if upstream_code == "previous_response_not_found"
        || lower.contains("previous_response_not_found")
        || (lower.contains("previous_response_id") && lower.contains("not found"))
    {
        Some(("previous_response_not_found", "invalid_request_error"))
    } else if status == 401
        || upstream_type == "authentication_error"
        || upstream_code == "invalid_api_key"
        || lower.contains("invalid or expired token")
        || lower.contains("refresh_token_reused")
    {
        Some(("auth_unavailable", "authentication_error"))
    } else {
        None
    }
}

/// `http.StatusText` for the statuses this module can produce.
fn status_text(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Request Entity Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// Port of classifyCodexStatusError (codex_executor_terminal.go).
fn classify_codex_status_error(status: u16, raw: &str) -> String {
    let Some((code, error_type)) = codex_status_error_classification(status, raw) else {
        return raw.to_string();
    };
    let body = parse_body(raw);
    let mut message = jstr(jget(&body, "error.message"));
    if message.is_empty() {
        message = jstr(jget(&body, "message"));
    }
    if message.is_empty() {
        message = raw.trim().to_string();
    }
    if message.is_empty() {
        message = status_text(status).to_string();
    }
    json!({"error":{"message":message,"type":error_type,"code":code}}).to_string()
}

/// Port of parseCodexRetryAfter (codex_executor_terminal.go).
fn parse_codex_retry_after(status: u16, raw: &str, now: SystemTime) -> Option<Duration> {
    if status != 429 || raw.is_empty() {
        return None;
    }
    let body = parse_body(raw);
    let null = Value::Null;
    for quota in [jget(&body, "error").unwrap_or(&null), &body] {
        if !jstr(jget(quota, "type"))
            .trim()
            .eq_ignore_ascii_case("usage_limit_reached")
        {
            continue;
        }
        let resets_at = jint(jget(quota, "resets_at"));
        if resets_at > 0 {
            let reset_at = UNIX_EPOCH + Duration::from_secs(resets_at as u64);
            if let Ok(left) = reset_at.duration_since(now) {
                if !left.is_zero() {
                    return Some(left);
                }
            }
        }
        let resets_in = jint(jget(quota, "resets_in_seconds"));
        if resets_in > 0 {
            return Some(Duration::from_secs(resets_in as u64));
        }
    }
    None
}

/// Port of newCodexStatusErrWithCooling (codex_executor_terminal.go) with
/// `codex.model-level-cooling` off (the default). Also the error for a
/// non-2xx HTTP answer from the backend (`newCodexStatusErrWithCooling(
/// httpResp.StatusCode, body)` in both Execute paths).
pub fn codex_status_error(status: u16, raw_body: &str) -> CodexError {
    codex_status_error_at(status, raw_body, SystemTime::now())
}

fn codex_status_error_at(status: u16, raw_body: &str, now: SystemTime) -> CodexError {
    let mut code = status;
    let is_usage_limit = is_codex_usage_limit_error(raw_body);
    if is_codex_model_capacity_error(raw_body) || is_usage_limit {
        code = 429;
    }
    let body = classify_codex_status_error(code, raw_body);
    let retry_after = parse_codex_retry_after(code, &body, now);
    CodexError {
        status: code,
        message: body,
        request_scoped: false,
        credential_scoped: is_usage_limit,
        retry_after,
    }
}

/// Port of codexTerminalStreamErrShouldHandle (codex_executor_terminal.go).
fn codex_terminal_stream_err_should_handle(raw: &str) -> bool {
    if codex_terminal_error_is_context_length(&parse_body(raw)) {
        return true;
    }
    if is_codex_usage_limit_error(raw) || is_codex_model_capacity_error(raw) {
        return true;
    }
    matches!(
        codex_status_error_classification(400, raw),
        Some(("thinking_signature_invalid", _))
    )
}

/// Port of codexTerminalStreamErrWithCooling (codex_executor_terminal.go).
fn codex_terminal_stream_err(event: &Value) -> Option<(CodexError, String)> {
    let body = codex_terminal_failure_body(event)?;
    if !codex_terminal_stream_err_should_handle(&body) {
        return None;
    }
    Some((codex_status_error(400, &body), body))
}

/// Port of codexTerminalStreamContextLengthErr (codex_executor_terminal.go).
#[cfg(test)]
fn codex_terminal_stream_context_length_err(event: &Value) -> Option<CodexError> {
    let (err, body) = codex_terminal_stream_err(event)?;
    codex_terminal_error_is_context_length(&parse_body(&body)).then_some(err)
}

/// Port of codexTerminalFailureErrWithCooling (codex_executor_terminal.go):
/// an `error` / `response.failed` event inside an HTTP 200 stream.
fn codex_terminal_failure_err(event: &Value) -> Option<(CodexError, String)> {
    if let Some(found) = codex_terminal_stream_err(event) {
        return Some(found);
    }
    let body = codex_terminal_failure_body(event)?;
    let status = codex_terminal_failure_status(&parse_body(&body));
    Some((codex_status_error(status, &body), body))
}

/// Port of newCodexIncompleteStreamError (codex_executor_terminal.go).
fn codex_incomplete_stream_error() -> CodexError {
    CodexError {
        status: 408,
        message: CODEX_INCOMPLETE_STREAM_MESSAGE.to_string(),
        request_scoped: true,
        credential_scoped: false,
        retry_after: None,
    }
}

/// Port of newCodexEmptyIncompleteStreamError (codex_executor_terminal.go).
fn codex_empty_incomplete_stream_error() -> CodexError {
    CodexError {
        status: 502,
        message: CODEX_EMPTY_INCOMPLETE_STREAM_MESSAGE.to_string(),
        request_scoped: true,
        credential_scoped: false,
        retry_after: None,
    }
}

// ---------------------------------------------------------------------------
// Responses-side output (translator/codex/openai/responses + helps).
// ---------------------------------------------------------------------------

/// Port of ensureUsageDetailsAt (helps/responses_usage_helpers.go).
fn ensure_usage_details_at(body: &mut Value, path: &str) {
    if !matches!(jget(body, path), Some(Value::Object(_))) {
        return;
    }
    for (details_key, field) in [
        ("output_tokens_details", "reasoning_tokens"),
        ("input_tokens_details", "cached_tokens"),
    ] {
        let details_path = format!("{path}.{details_key}");
        match jget(body, &details_path) {
            None => jset(body, &format!("{details_path}.{field}"), json!(0)),
            Some(Value::Object(m)) => {
                if matches!(m.get(field), None | Some(Value::Null)) {
                    jset(body, &format!("{details_path}.{field}"), json!(0));
                }
            }
            Some(_) => jset(body, &details_path, json!({ field: 0 })),
        }
    }
}

/// Port of EnsureResponsesUsageDetails (helps/responses_usage_helpers.go),
/// JSON-object branch (the one the non-stream Responses output takes).
fn ensure_responses_usage_details(body: &mut Value) {
    if !body.is_object() || jstr(jget(body, "object")) == "response.compaction" {
        return;
    }
    ensure_usage_details_at(body, "response.usage");
    ensure_usage_details_at(body, "usage");
}

/// Port of ConvertCodexResponseToOpenAIResponsesNonStream
/// (translator/codex/openai/responses/codex_openai-responses_response.go).
/// Go returns an empty payload when the event carries no `response`; here
/// that is `Value::Null`.
fn convert_codex_response_to_openai_responses_non_stream(event: &Value) -> Value {
    match jstr(jget(event, "type")).as_str() {
        "response.completed" | "response.incomplete" => {
            jget(event, "response").cloned().unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

/// Port of translatorcommon.RequestModelName's per-body probe
/// (translator/common/request.go): the client body's `model` (or
/// `request.model`) when it is a non-blank string.
pub fn request_model_name(client_body: &Value) -> String {
    for path in ["model", "request.model"] {
        if let Some(Value::String(m)) = jget(client_body, path) {
            if !m.trim().is_empty() {
                return m.clone();
            }
        }
    }
    String::new()
}

/// Per-event rewrite applied while relaying the stream to a Responses client
/// (identity if Go does none). Input/output: one SSE `data:` JSON payload.
///
/// Port of setResponsesModel
/// (translator/codex/openai/responses/codex_openai-responses_response.go):
/// `response.created` / `response.in_progress` without a `response.model`
/// get the client's requested model. Every other event passes unchanged.
/// The stateful terminal-event patch lives in `CodexStreamRelay`.
pub fn rewrite_stream_event(event: &Value, requested_model: &str) -> Value {
    let event_type = jstr(jget(event, "type"));
    if event_type != "response.created" && event_type != "response.in_progress" {
        return event.clone();
    }
    if jget(event, "response.model").is_some() || requested_model.is_empty() {
        return event.clone();
    }
    let mut out = event.clone();
    jset(
        &mut out,
        "response.model",
        Value::String(requested_model.to_string()),
    );
    out
}

/// Assemble the final Responses `response` object from a complete Codex SSE
/// body (non-stream clients). Err = the Go error text when no terminal event.
#[allow(dead_code)] // ported for parity; not used by the router (image generation is out of scope / the detailed variant is used)
pub fn assemble_non_stream(sse_text: &str) -> Result<Value, String> {
    assemble_non_stream_detailed(sse_text).map_err(|e| e.message)
}

/// `assemble_non_stream` with the full error (status, scope, retry-after).
///
/// Port of the response half of `Execute` (codex_executor_execute.go): scan
/// `data:` lines; a terminal failure event is an error; collect
/// `response.output_item.done` items by output_index; on the first
/// `response.completed` / `response.incomplete` patch its output and return
/// its `response` with usage details ensured. No terminal event is the 408
/// "stream closed before response.completed" error. Note this path does NOT
/// treat `response.done` as terminal (only the stream path does).
pub fn assemble_non_stream_detailed(sse_text: &str) -> Result<Value, CodexError> {
    let mut by_index = BTreeMap::new();
    let mut fallback = Vec::new();
    let mut saw_output_delta = false;
    for line in sse_text.split('\n') {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let event: Value = serde_json::from_str(rest.trim()).unwrap_or(Value::Null);
        let event_type = jstr(jget(&event, "type"));

        if has_meaningful_codex_output_delta(&event) {
            saw_output_delta = true;
        }
        if let Some((err, _)) = codex_terminal_failure_err(&event) {
            return Err(err);
        }
        if event_type == "response.output_item.done" {
            collect_codex_output_item_done(&event, &mut by_index, &mut fallback);
            continue;
        }
        if event_type != "response.completed" && event_type != "response.incomplete" {
            continue;
        }
        if is_codex_terminal_empty_incomplete(
            &event,
            by_index.len() + fallback.len(),
            saw_output_delta,
        ) {
            return Err(codex_empty_incomplete_stream_error());
        }
        let completed = patch_codex_completed_output(event, &by_index, &fallback);
        let mut out = convert_codex_response_to_openai_responses_non_stream(&completed);
        ensure_responses_usage_details(&mut out);
        return Ok(out);
    }
    Err(codex_incomplete_stream_error())
}

/// What the stream relay does with one upstream `data:` event.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayStep {
    /// Forward this payload as `data: <json>` and keep reading.
    Forward(Value),
    /// Forward this terminal payload, then end the stream (Go stops reading).
    Terminal(Value),
    /// Stop: deliver this error in-stream instead (Go's `StreamChunk{Err}`).
    Failed(CodexError),
}

/// Stateful relay of the Codex SSE stream to a Responses client — the
/// unbuffered goroutine of `ExecuteStream` (codex_executor_stream.go) plus the
/// Responses translator. Only `data:` payloads go through `on_event`; Go
/// forwards every other line (`event:`, comments, blanks) verbatim, and
/// re-emits each data line as `data: <payload>`.
pub struct CodexStreamRelay {
    requested_model: String,
    preserve_native_output: bool,
    by_index: BTreeMap<i64, Value>,
    fallback: Vec<Value>,
    saw_output_delta: bool,
    finished: bool,
}

impl CodexStreamRelay {
    /// `requested_model`: see `request_model_name`. `preserve_native_output`:
    /// `is_codex_responses_lite_request` of the client request — a native
    /// Responses-Lite client gets the terminal event unpatched.
    pub fn new(requested_model: &str, preserve_native_output: bool) -> Self {
        CodexStreamRelay {
            requested_model: requested_model.to_string(),
            preserve_native_output,
            by_index: BTreeMap::new(),
            fallback: Vec::new(),
            saw_output_delta: false,
            finished: false,
        }
    }

    /// One upstream `data:` payload (already JSON-parsed).
    pub fn on_event(&mut self, event: &Value) -> RelayStep {
        if let Some((err, _)) = codex_terminal_failure_err(event) {
            self.finished = true;
            return RelayStep::Failed(err);
        }
        if has_meaningful_codex_output_delta(event) {
            self.saw_output_delta = true;
        }
        if is_codex_terminal_empty_incomplete(
            event,
            self.by_index.len() + self.fallback.len(),
            self.saw_output_delta,
        ) {
            self.finished = true;
            return RelayStep::Failed(codex_empty_incomplete_stream_error());
        }
        match jstr(jget(event, "type")).as_str() {
            "response.output_item.done" => {
                collect_codex_output_item_done(event, &mut self.by_index, &mut self.fallback);
                RelayStep::Forward(rewrite_stream_event(event, &self.requested_model))
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let mut data = event.clone();
                normalize_codex_websocket_completion(&mut data);
                if !self.preserve_native_output {
                    data = patch_codex_completed_output(data, &self.by_index, &self.fallback);
                }
                self.finished = true;
                RelayStep::Terminal(rewrite_stream_event(&data, &self.requested_model))
            }
            _ => RelayStep::Forward(rewrite_stream_event(event, &self.requested_model)),
        }
    }

    /// Call when the upstream body ends. `Some` = the in-stream error Go
    /// delivers when no terminal event arrived (408, request-scoped).
    pub fn finish(&self) -> Option<CodexError> {
        (!self.finished).then(codex_incomplete_stream_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn oauth() -> CodexCredential<'static> {
        CodexCredential {
            access_token: "oauth-token",
            account_id: Some("acct"),
        }
    }

    fn body_of(req: &(String, Vec<(String, String)>, Vec<u8>)) -> Value {
        serde_json::from_slice(&req.2).unwrap()
    }

    fn header<'a>(req: &'a (String, Vec<(String, String)>, Vec<u8>), key: &str) -> Option<&'a str> {
        req.1
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    fn valid_encrypted_content() -> String {
        use base64::Engine;
        let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
        payload[0] = 0x80;
        for (i, b) in payload.iter_mut().enumerate().skip(9) {
            *b = i as u8;
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    }

    fn v(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    // ---- build_request: full shape ----------------------------------------

    #[test]
    fn build_request_exact_url_headers_and_body() {
        let body = v(
            r#"{"model":"gpt-5.5","input":"hi","stream":true,"store":false,"prompt_cache_key":"pck-1","service_tier":"priority","previous_response_id":"r","safety_identifier":"s","prompt_cache_retention":"24h","generate":false,"stream_options":{"include_usage":true}}"#,
        );
        let client = hdrs(&[
            ("user-agent", "my-client/1.0"),
            ("originator", "codex_exec"),
            ("version", "0.150.0"),
            ("session-id", "client-session"),
            ("x-codex-window-id", "w:0"),
            ("x-codex-routing-hint", "model=other"),
            ("authorization", "Bearer router-key"),
            ("x-unrelated", "dropped"),
        ]);
        let req = build_request(CODEX_DEFAULT_BASE_URL, &body, &oauth(), &client, "0.160.0");
        assert_eq!(req.0, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            req.1,
            hdrs(&[
                ("Session-Id", "client-session"),
                ("Content-Type", "application/json"),
                ("Authorization", "Bearer oauth-token"),
                ("Version", "0.150.0"),
                ("X-Codex-Window-Id", "w:0"),
                ("User-Agent", CODEX_USER_AGENT),
                ("Accept", "text/event-stream"),
                ("Connection", "Keep-Alive"),
                ("Originator", "codex-tui"),
                ("Chatgpt-Account-Id", "acct"),
                ("X-Codex-Routing-Hint", "model=gpt-5.5;tier=priority"),
            ])
        );
        assert_eq!(
            String::from_utf8(req.2).unwrap(),
            r#"{"model":"gpt-5.5","input":"hi","stream":true,"store":false,"prompt_cache_key":"pck-1","service_tier":"priority","instructions":""}"#
        );
    }

    #[test]
    fn build_request_non_stream_client_sets_stream_and_session_from_cache_key() {
        let body = v(
            r#"{"model":"gpt-5.4","input":"hello","prompt_cache_key":"pck-2","stream_options":{"reasoning_summary_delivery":"sequential_cutoff"}}"#,
        );
        let req = build_request("https://h/codex/", &body, &oauth(), &[], "");
        assert_eq!(req.0, "https://h/codex/responses");
        assert_eq!(
            req.1,
            hdrs(&[
                ("Session-Id", "pck-2"),
                ("Content-Type", "application/json"),
                ("Authorization", "Bearer oauth-token"),
                ("User-Agent", CODEX_USER_AGENT),
                ("Accept", "text/event-stream"),
                ("Connection", "Keep-Alive"),
                ("Originator", "codex-tui"),
                ("Chatgpt-Account-Id", "acct"),
                ("X-Codex-Routing-Hint", "model=gpt-5.4"),
            ])
        );
        assert_eq!(
            String::from_utf8(req.2).unwrap(),
            r#"{"model":"gpt-5.4","input":"hello","prompt_cache_key":"pck-2","stream":true,"instructions":""}"#
        );
    }

    #[test]
    fn build_request_without_account_or_token() {
        let cred = CodexCredential {
            access_token: "  ",
            account_id: None,
        };
        let req = build_request("", &v(r#"{"model":"m","input":[]}"#), &cred, &[], "");
        assert_eq!(req.0, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            req.1,
            hdrs(&[
                ("Content-Type", "application/json"),
                ("User-Agent", CODEX_USER_AGENT),
                ("Accept", "text/event-stream"),
                ("Connection", "Keep-Alive"),
                ("Originator", "codex-tui"),
                ("X-Codex-Routing-Hint", "model=m"),
            ])
        );
    }

    #[test]
    fn build_request_passthrough_headers() {
        let client = hdrs(&[
            ("x-codex-beta-features", "a,b"),
            ("x-codex-turn-metadata", " {\"turn_id\":\"t\"} "),
            ("x-codex-turn-state", "st"),
            ("x-client-request-id", "cr"),
            ("thread-id", "th"),
            ("x-openai-internal-codex-responses-lite", "true"),
        ]);
        let req = build_request("", &v(r#"{"model":"m","input":[]}"#), &oauth(), &client, "");
        assert_eq!(
            req.1,
            hdrs(&[
                ("Content-Type", "application/json"),
                ("Authorization", "Bearer oauth-token"),
                ("X-Codex-Beta-Features", "a,b"),
                ("X-Codex-Turn-Metadata", "{\"turn_id\":\"t\"}"),
                ("X-Codex-Turn-State", "st"),
                ("X-Client-Request-Id", "cr"),
                ("Thread-Id", "th"),
                ("X-Openai-Internal-Codex-Responses-Lite", "true"),
                ("User-Agent", CODEX_USER_AGENT),
                ("Accept", "text/event-stream"),
                ("Connection", "Keep-Alive"),
                ("Originator", "codex-tui"),
                ("Chatgpt-Account-Id", "acct"),
                ("X-Codex-Routing-Hint", "model=m"),
            ])
        );
    }

    // port of TestApplyCodexHeadersUsesAccountHeaderForOAuth (codex_executor_cache_test.go)
    #[test]
    fn apply_codex_headers_uses_account_header_for_oauth() {
        let mut h = Headers::default();
        let cred = CodexCredential {
            access_token: "oauth-token",
            account_id: Some("acct-1"),
        };
        apply_codex_headers_from_sources(&mut h, &cred, true, &ClientHeaders(&[]));
        assert_eq!(h.get("Chatgpt-Account-Id"), "acct-1");
    }

    #[test]
    fn model_header_override_adds_session_id_for_luna() {
        let req = build_request(
            "",
            &v(r#"{"model":"gpt-5.6-luna","input":[]}"#),
            &oauth(),
            &[],
            "",
        );
        let session = header(&req, "Session_id").unwrap();
        assert_eq!(session.len(), 36);
        assert_eq!(&session[14..15], "4");
        // A cache key already supplies the session header: no extra one.
        let req = build_request(
            "",
            &v(r#"{"model":"gpt-5.6-luna","input":[],"prompt_cache_key":"k"}"#),
            &oauth(),
            &[],
            "",
        );
        assert_eq!(header(&req, "Session-Id"), Some("k"));
        assert!(!req.1.iter().any(|(k, _)| k == "Session_id"));
    }

    // ---- instructions (codex_executor_instructions_test.go) ---------------

    // port of TestCodexExecutorExecuteNormalizesNullInstructions
    #[test]
    fn execute_normalizes_null_instructions() {
        let req = build_request(
            "",
            &v(r#"{"model":"gpt-5.4","instructions":null,"input":"hello"}"#),
            &oauth(),
            &[],
            "",
        );
        assert_eq!(jget(&body_of(&req), "instructions"), Some(&json!("")));
    }

    // port of TestCodexExecutorExecuteStreamNormalizesNullInstructions
    #[test]
    fn execute_stream_normalizes_null_instructions() {
        let req = build_request(
            "",
            &v(r#"{"model":"gpt-5.4","instructions":null,"input":"hello","stream":true}"#),
            &oauth(),
            &[],
            "",
        );
        assert_eq!(jget(&body_of(&req), "instructions"), Some(&json!("")));
    }

    #[test]
    fn native_lite_request_keeps_null_instructions() {
        let mut body = v(r#"{"instructions":null}"#);
        normalize_codex_instructions(&mut body, true);
        assert_eq!(body, v(r#"{"instructions":null}"#));
        let mut body = v(r#"{"instructions":"keep"}"#);
        normalize_codex_instructions(&mut body, false);
        assert_eq!(body, v(r#"{"instructions":"keep"}"#));
    }

    // ---- parallel_tool_calls (codex_executor_parallel_tool_calls_test.go) -

    // port of TestNormalizeCodexParallelToolCallsForTools_DropsWhenToolsMissing
    #[test]
    fn parallel_tool_calls_drops_when_tools_missing() {
        let mut body = v(r#"{"model":"gpt-5.4","parallel_tool_calls":true,"input":"hi"}"#);
        normalize_codex_parallel_tool_calls_for_tools(&mut body);
        assert_eq!(body, v(r#"{"model":"gpt-5.4","input":"hi"}"#));
    }

    // port of TestNormalizeCodexParallelToolCallsForTools_DropsWhenToolsEmpty
    #[test]
    fn parallel_tool_calls_drops_when_tools_empty() {
        let mut body =
            v(r#"{"model":"gpt-5.4","tools":[],"parallel_tool_calls":false,"input":"hi"}"#);
        normalize_codex_parallel_tool_calls_for_tools(&mut body);
        assert_eq!(body, v(r#"{"model":"gpt-5.4","tools":[],"input":"hi"}"#));
    }

    // port of TestNormalizeCodexParallelToolCallsForTools_PreservesWhenToolsPresent
    #[test]
    fn parallel_tool_calls_preserved_when_tools_present() {
        let raw = r#"{"model":"gpt-5.4","tools":[{"type":"function","name":"lookup"}],"parallel_tool_calls":true,"input":"hi"}"#;
        let mut body = v(raw);
        normalize_codex_parallel_tool_calls_for_tools(&mut body);
        assert_eq!(body, v(raw));
    }

    // port of TestNormalizeCodexParallelToolCalls_ResponsesLiteMetadataForcesFalse
    #[test]
    fn parallel_tool_calls_lite_metadata_forces_false() {
        let mut body = v(
            r#"{"model":"gpt-5.6-luna","tools":[{"type":"function","name":"lookup"}],"parallel_tool_calls":true,"client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":"true"},"input":"hi"}"#,
        );
        let lite = is_codex_responses_lite_request(&body, &[]);
        assert!(lite);
        normalize_codex_parallel_tool_calls(&mut body, lite);
        assert_eq!(jget(&body, "parallel_tool_calls"), Some(&json!(false)));
    }

    // port of TestNormalizeCodexParallelToolCalls_ResponsesLiteHeaderForcesFalse
    #[test]
    fn parallel_tool_calls_lite_header_forces_false() {
        let mut body = v(r#"{"model":"gpt-5.6-luna","parallel_tool_calls":true,"input":"hi"}"#);
        let client = hdrs(&[("x-openai-internal-codex-responses-lite", "true")]);
        let lite = is_codex_responses_lite_request(&body, &client);
        assert!(lite);
        normalize_codex_parallel_tool_calls(&mut body, lite);
        assert_eq!(jget(&body, "parallel_tool_calls"), Some(&json!(false)));
    }

    // ---- stream_options ---------------------------------------------------

    // port of the request side of TestCodexAutoExecutorHTTPFallbackForwardsSequentialCutoffReasoningSummaryDelivery
    #[test]
    fn stream_keeps_only_reasoning_summary_delivery() {
        let req = build_request(
            "",
            &v(
                r#"{"model":"gpt-5.6-sol","input":"hello","stream":true,"reasoning":{"summary":"detailed"},"stream_options":{"reasoning_summary_delivery":"sequential_cutoff","include_usage":true}}"#,
            ),
            &oauth(),
            &[],
            "",
        );
        assert_eq!(
            jget(&body_of(&req), "stream_options"),
            Some(&json!({"reasoning_summary_delivery":"sequential_cutoff"}))
        );
    }

    // ---- routing hint (codex_executor_routing_hint_test.go) ---------------

    // port of TestApplyCodexRoutingHint
    #[test]
    fn routing_hint_cases() {
        let body = v(r#"{"model":"gpt-5.5","service_tier":"priority"}"#);
        // replaces a hint that did not come from an operator rule
        let mut h = Headers::default();
        h.set(CODEX_ROUTING_HINT_HEADER, "model=gpt-5.4-client");
        apply_codex_routing_hint(&mut h, "gpt-5.5", &body);
        assert_eq!(
            h.0,
            hdrs(&[("X-Codex-Routing-Hint", "model=gpt-5.5;tier=priority")])
        );
        // drops a forwarded hint when the resolved model is empty
        let mut h = Headers::default();
        h.set(CODEX_ROUTING_HINT_HEADER, "model=gpt-5.4-client");
        apply_codex_routing_hint(&mut h, "", &body);
        assert!(h.0.is_empty());
        // ignores a non-string tier
        let mut h = Headers::default();
        apply_codex_routing_hint(
            &mut h,
            "gpt-5.5",
            &v(r#"{"model":"gpt-5.5","service_tier":null}"#),
        );
        assert_eq!(h.0, hdrs(&[("X-Codex-Routing-Hint", "model=gpt-5.5")]));
    }

    // port of TestCodexExecutorRoutingHintCarriesRequestedTier (Responses-body rows)
    #[test]
    fn routing_hint_follows_final_body_tier() {
        let req = build_request(
            "",
            &v(r#"{"model":"gpt-5.5","service_tier":"priority","input":[]}"#),
            &oauth(),
            &hdrs(&[("x-codex-routing-hint", "model=gpt-5.4-client")]),
            "",
        );
        assert_eq!(
            header(&req, "X-Codex-Routing-Hint"),
            Some("model=gpt-5.5;tier=priority")
        );
        assert_eq!(
            jget(&body_of(&req), "service_tier"),
            Some(&json!("priority"))
        );
        let req = build_request(
            "",
            &v(r#"{"model":"gpt-5.5","input":[]}"#),
            &oauth(),
            &[],
            "",
        );
        assert_eq!(header(&req, "X-Codex-Routing-Hint"), Some("model=gpt-5.5"));
    }

    // ---- reasoning encrypted_content (openai_responses_signature_test.go) -

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_StripsOrphanIDsWhenStoreDisabled
    #[test]
    fn sanitize_strips_orphan_ids_when_store_disabled() {
        let valid = valid_encrypted_content();
        let mut body = v(&format!(
            r#"{{"store":false,"input":[{{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]}},{{"id":"rs_orphan","type":"reasoning","summary":[]}},{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"id":"msg_1","type":"message","role":"user","content":"hi"}}]}}"#
        ));
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(
            body,
            v(&format!(
                r#"{{"store":false,"input":[{{"type":"reasoning","summary":[]}},{{"type":"reasoning","summary":[]}},{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"id":"msg_1","type":"message","role":"user","content":"hi"}}]}}"#
            ))
        );
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_KeepsIDsWhenStoreEnabled
    #[test]
    fn sanitize_keeps_ids_when_store_enabled() {
        let mut body = v(
            r#"{"store":true,"input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]},{"id":"rs_orphan","type":"reasoning","summary":[]}]}"#,
        );
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(
            body,
            v(
                r#"{"store":true,"input":[{"id":"rs_bad","type":"reasoning","summary":[]},{"id":"rs_orphan","type":"reasoning","summary":[]}]}"#
            )
        );
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_MovesCleartextContentToSummary
    #[test]
    fn sanitize_moves_cleartext_content_to_summary() {
        let mut body = v(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"The model thinking process from a previous turn with a third-party provider..."}],"encrypted_content":null},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(
            body,
            v(
                r#"{"store":false,"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"The model thinking process from a previous turn with a third-party provider..."}],"content":[]},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#
            )
        );
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_DoesNotDuplicateExistingSummary
    #[test]
    fn sanitize_does_not_duplicate_existing_summary() {
        let mut body = v(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"already summarized"}],"content":[{"type":"reasoning_text","text":"duplicate thinking"}]}]}"#,
        );
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(
            body,
            v(
                r#"{"store":false,"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"already summarized"}],"content":[]}]}"#
            )
        );
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_PromotesMultipleReasoningTextParts
    #[test]
    fn sanitize_promotes_multiple_reasoning_text_parts() {
        let mut body = v(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"step one"},{"type":"reasoning_text","text":"step two"}]}]}"#,
        );
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(
            body,
            v(
                r#"{"store":false,"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"step one"},{"type":"summary_text","text":"step two"}],"content":[]}]}"#
            )
        );
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_KeepsValidEncryptedContentWhenStrippingContent
    #[test]
    fn sanitize_keeps_valid_encrypted_content_when_stripping_content() {
        let valid = valid_encrypted_content();
        let mut body = v(&format!(
            r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[],"content":[{{"type":"reasoning_text","text":"cleartext thinking"}}]}}]}}"#
        ));
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(
            body,
            v(&format!(
                r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[{{"type":"summary_text","text":"cleartext thinking"}}],"content":[]}}]}}"#
            ))
        );
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContent_NoopReturnsOriginalBody
    #[test]
    fn sanitize_noop_leaves_body_unchanged() {
        let valid = valid_encrypted_content();
        let raw = format!(
            r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"role":"user","content":"hi"}}]}}"#
        );
        let mut body = v(&raw);
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, false);
        assert_eq!(body.to_string(), raw);
    }

    // port of TestSanitizeOpenAIResponsesReasoningEncryptedContentWithCompat_PreservesReasoningContentAndID
    #[test]
    fn sanitize_compat_preserves_reasoning_content_and_id() {
        let mut body = v(
            r#"{"store":false,"input":[{"id":"rs_compat","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"keep cleartext thinking"}],"encrypted_content":null},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        sanitize_openai_responses_reasoning_encrypted_content(&mut body, true);
        assert_eq!(
            body,
            v(
                r#"{"store":false,"input":[{"id":"rs_compat","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"keep cleartext thinking"}]},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#
            )
        );
    }

    // port of TestCodexExecutorDropsInvalidReasoningEncryptedContentFromFinalRequest
    #[test]
    fn execute_drops_invalid_reasoning_encrypted_content_from_final_request() {
        let valid = valid_encrypted_content();
        let body = v(&format!(
            r#"{{"model":"gpt-5.4","input":[{{"id":"rs_bad","type":"reasoning","encrypted_content":"gAAAABqFTIa\u2026abc","summary":[]}},{{"id":"rs_non_string","type":"reasoning","encrypted_content":123,"summary":[]}},{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"role":"user","content":"hello","encrypted_content":"leave-message-alone"}}]}}"#
        ));
        let req = build_request("", &body, &oauth(), &[], "");
        assert_eq!(
            jget(&body_of(&req), "input"),
            Some(&v(&format!(
                r#"[{{"type":"reasoning","summary":[]}},{{"type":"reasoning","summary":[]}},{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"role":"user","content":"hello","encrypted_content":"leave-message-alone"}}]"#
            )))
        );
    }

    #[test]
    fn inspect_signature_rejections() {
        assert!(inspect_gpt_reasoning_signature(&valid_encrypted_content()).is_ok());
        assert_eq!(
            inspect_gpt_reasoning_signature("bad"),
            Err("invalid GPT reasoning signature: expected gAAAA prefix".to_string())
        );
        assert_eq!(
            inspect_gpt_reasoning_signature("gAAAA-encrypted"),
            Err("invalid GPT reasoning signature: decoded payload too short".to_string())
        );
        assert_eq!(
            inspect_gpt_reasoning_signature("gAAAAB\u{2026}"),
            Err("invalid GPT reasoning signature: contains non-base64url character U+2026 at byte 6".to_string())
        );
    }

    // ---- input item ids (helps/codex_input_ids_test.go) -------------------

    fn ids(body: &Value) -> Vec<String> {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| jstr(jget(i, "id")))
            .collect()
    }

    // port of TestSanitizeCodexInputItemIDsBoundaries
    #[test]
    fn input_ids_boundaries() {
        let id64 = "a".repeat(64);
        let id65 = "b".repeat(65);
        let unicode65 = "界".repeat(65);
        let mut body = json!({"input":[{"id":id64},{"id":id65},{"id":unicode65}]});
        sanitize_codex_input_item_ids(&mut body);
        let got = ids(&body);
        assert_eq!(got[0], id64);
        assert_eq!(got[1], codex_input_item_id_with_hash_suffix(&id65, 0));
        assert_eq!(rune_len(&got[1]), 64);
        assert_eq!(rune_len(&got[2]), 64);
    }

    // port of TestSanitizeCodexInputItemIDsNormalizesMessageIDs
    #[test]
    fn input_ids_normalizes_message_ids() {
        let raw = r#"{"input":[{"type":"message","id":"item_74ec40c883248ebb4885ec84","role":"user"},{"type":"message","id":"msg-1","role":"assistant"},{"type":"function_call","id":"item_call","call_id":"call-1"}]}"#;
        let mut first = v(raw);
        sanitize_codex_input_item_ids(&mut first);
        let mut second = v(raw);
        sanitize_codex_input_item_ids(&mut second);
        assert_eq!(
            ids(&first),
            ["msg_item_74ec40c883248ebb4885ec84", "msg-1", "fc_item_call"]
        );
        assert_eq!(first.to_string(), second.to_string());
    }

    // port of TestSanitizeCodexInputItemIDsNormalizesResponseItemIDs
    #[test]
    fn input_ids_normalizes_response_item_ids() {
        let mut body = v(
            r#"{"input":[{"type":"message","id":"item_message"},{"type":"reasoning","id":"item_reasoning"},{"type":"function_call","id":"item_function_call","call_id":"call-1"},{"type":"function_call_output","id":"item_function_call_output","call_id":"call-1"},{"type":"reasoning","id":"rs-existing"},{"type":"function_call","id":"fc-existing","call_id":"call-2"},{"type":"message","id":"msg-existing"}]}"#,
        );
        sanitize_codex_input_item_ids(&mut body);
        assert_eq!(
            ids(&body),
            [
                "msg_item_message",
                "rs_item_reasoning",
                "fc_item_function_call",
                "item_function_call_output",
                "rs-existing",
                "fc-existing",
                "msg-existing"
            ]
        );
    }

    // port of TestSanitizeCodexInputItemIDsAvoidsNormalizationCollisions
    #[test]
    fn input_ids_avoids_normalization_collisions() {
        for (item_type, prefix) in [
            ("message", "msg_"),
            ("reasoning", "rs_"),
            ("function_call", "fc_"),
            ("custom_tool_call", "ctc_"),
            ("custom_tool_call_output", "ctco_"),
        ] {
            for invalid in [
                "item_collision".to_string(),
                "x".repeat(CODEX_INPUT_ITEM_ID_LIMIT - prefix.len() + 1),
            ] {
                let prefixed = format!("{prefix}{invalid}");
                for (pair, prefixed_index) in [
                    ([invalid.clone(), prefixed.clone()], 1usize),
                    ([prefixed.clone(), invalid.clone()], 0usize),
                ] {
                    let make = || json!({"input":[{"type":item_type,"id":pair[0]},{"type":item_type,"id":pair[1]}]});
                    let mut first = make();
                    sanitize_codex_input_item_ids(&mut first);
                    let mut second = make();
                    sanitize_codex_input_item_ids(&mut second);
                    let mut again = first.clone();
                    sanitize_codex_input_item_ids(&mut again);
                    let got = ids(&first);
                    assert_ne!(got[0], got[1]);
                    for id in &got {
                        assert!(id.starts_with(prefix), "{id}");
                        assert!(rune_len(id) <= CODEX_INPUT_ITEM_ID_LIMIT);
                    }
                    if rune_len(&prefixed) <= CODEX_INPUT_ITEM_ID_LIMIT {
                        assert_eq!(got[prefixed_index], prefixed);
                    }
                    assert_eq!(first.to_string(), second.to_string());
                    assert_eq!(first.to_string(), again.to_string());
                }
            }
        }
    }

    // port of TestSanitizeCodexInputItemIDsNormalizesCustomToolCallIDs
    #[test]
    fn input_ids_normalizes_custom_tool_call_ids() {
        let mut body = v(
            r#"{"input":[{"type":"custom_tool_call","id":"item_44e13caebc1ddf25f1337cbe","call_id":"call-1","name":"lookup","input":"{}"}]}"#,
        );
        sanitize_codex_input_item_ids(&mut body);
        assert_eq!(ids(&body), ["ctc_item_44e13caebc1ddf25f1337cbe"]);
    }

    // port of TestSanitizeCodexInputItemIDsNormalizesCustomToolCallOutputIDs
    #[test]
    fn input_ids_normalizes_custom_tool_call_output_ids() {
        let mut body = v(
            r#"{"input":[{"type":"custom_tool_call_output","id":"item_44e13caebc1ddf25f1337cbe_output","call_id":"call-1","output":"done"},{"type":"custom_tool_call_output","id":"ctco-existing","call_id":"call-2","output":"done"}]}"#,
        );
        sanitize_codex_input_item_ids(&mut body);
        assert_eq!(
            ids(&body),
            ["ctco_item_44e13caebc1ddf25f1337cbe_output", "ctco-existing"]
        );
        let mut again = body.clone();
        sanitize_codex_input_item_ids(&mut again);
        assert_eq!(again, body);
    }

    // port of TestSanitizeCodexInputItemIDsDropsOverlongEncryptedReasoningItem
    #[test]
    fn input_ids_drops_overlong_encrypted_reasoning_item() {
        let long_reasoning = format!("rs_{}", "a".repeat(64));
        let short_reasoning = format!("rs_{}", "b".repeat(48));
        let long_call = "call-item-".repeat(8);
        let mut body = json!({"input":[
            {"type":"message","id":"msg-1","role":"user","content":"before"},
            {"type":"reasoning","id":long_reasoning,"encrypted_content":"gAAAA-encrypted","summary":[{"type":"summary_text","text":"drop me"}]},
            {"type":"reasoning","id":short_reasoning,"encrypted_content":"gAAAA-encrypted","summary":[]},
            {"type":"function_call","id":long_call,"call_id":"call-1","name":"lookup","arguments":"{}"}
        ]});
        sanitize_codex_input_item_ids(&mut body);
        let got = ids(&body);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], "msg-1");
        assert_eq!(got[1], short_reasoning);
        assert_eq!(
            got[2],
            codex_input_item_id_with_hash_suffix(&format!("fc_{long_call}"), 0)
        );
        assert_eq!(rune_len(&got[2]), 64);
    }

    // port of TestSanitizeCodexInputItemIDsShortensOverlongReasoningWithoutEncryptedContent
    #[test]
    fn input_ids_shortens_overlong_reasoning_without_encrypted_content() {
        let long_reasoning = format!("rs_{}", "a".repeat(64));
        for extra in [
            "",
            r#","encrypted_content":"""#,
            r#","encrypted_content":null"#,
        ] {
            let mut body = v(&format!(
                r#"{{"input":[{{"type":"reasoning","id":"{long_reasoning}"{extra},"summary":[]}}]}}"#
            ));
            sanitize_codex_input_item_ids(&mut body);
            let got = ids(&body);
            assert_eq!(
                got,
                [codex_input_item_id_with_hash_suffix(&long_reasoning, 0)]
            );
        }
    }

    // port of TestSanitizeCodexInputItemIDsAvoidsExistingIDCollision
    #[test]
    fn input_ids_avoids_existing_id_collision() {
        let long_id = "grok-item-".repeat(10);
        let colliding = shorten_codex_input_item_id_with_attempt(&long_id, 0);
        let make = || json!({"input":[{"id":long_id},{"id":colliding}]});
        let mut first = make();
        sanitize_codex_input_item_ids(&mut first);
        let mut second = make();
        sanitize_codex_input_item_ids(&mut second);
        let got = ids(&first);
        assert_eq!(
            got[0],
            shorten_codex_input_item_id_with_attempt(&long_id, 1)
        );
        assert_ne!(got[0], colliding);
        assert_eq!(got[1], colliding);
        assert_eq!(first, second);
    }

    // port of TestSanitizeCodexInputItemIDsLeavesUnsupportedPayloadsUnchanged
    #[test]
    fn input_ids_leaves_unsupported_payloads_unchanged() {
        for raw in [
            r#"{"input":{"id":"item-1"}}"#,
            r#"{"input":[1,{"id":2},{"id":"item-1"}]}"#,
        ] {
            let mut body = v(raw);
            sanitize_codex_input_item_ids(&mut body);
            assert_eq!(body.to_string(), raw);
        }
    }

    // port of TestCodexExecutorExecuteStreamSanitizesOverlongInputItemIDs (codex_executor_input_ids_test.go)
    #[test]
    fn execute_stream_sanitizes_overlong_input_item_ids() {
        let long_reasoning = format!("rs_{}", "a".repeat(64));
        let long_call = "grok-call-item-".repeat(6);
        let long_output = "grok-output-item-".repeat(6);
        let enc = valid_encrypted_content();
        let body = json!({"model":"gpt-5.4","stream":true,"input":[
            {"type":"reasoning","id":long_reasoning,"encrypted_content":enc,"summary":[]},
            {"type":"function_call","id":long_call,"call_id":"call-1","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","id":long_output,"call_id":"call-1","output":"ok"},
            {"type":"message","id":"item_74ec40c883248ebb4885ec84","role":"user","content":"continue"}
        ]});
        let req = build_request("", &body, &oauth(), &[], "");
        let out = body_of(&req);
        assert_eq!(
            jget(&out, "input"),
            Some(&json!([
                {"type":"function_call","id":codex_input_item_id_with_hash_suffix(&format!("fc_{long_call}"), 0),"call_id":"call-1","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","id":codex_input_item_id_with_hash_suffix(&long_output, 0),"call_id":"call-1","output":"ok"},
                {"type":"message","id":"msg_item_74ec40c883248ebb4885ec84","role":"user","content":"continue"}
            ]))
        );
    }

    // ---- tool schemas (helps/codex_tool_schema_test.go) --------------------

    fn eight(kind: &str) -> String {
        (1..=8)
            .map(|i| match kind {
                "s" => format!(r#"{{"const":"{i}"}}"#),
                _ => format!(r#"{{"const":{i}}}"#),
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn tool_with_prop(prop: &str) -> Value {
        v(&format!(
            r#"{{"model":"gpt-5.5","tools":[{{"type":"function","name":"t","parameters":{{"type":"object","properties":{{"val":{prop}}}}}}}]}}"#
        ))
    }

    fn normalized(body: &Value) -> Value {
        let mut out = body.clone();
        normalize_codex_tool_schemas(&mut out);
        out
    }

    // port of TestNormalizeCodexToolSchemas_ComplexOneOfSimplified_PreservesProperties
    #[test]
    fn tool_schema_complex_one_of_simplified_preserves_properties() {
        let consts = [
            "p.list",
            "m.list",
            "s.list",
            "s.create",
            "s.send",
            "s.fork",
            "s.status",
            "s.messages",
            "sch.list",
            "sch.create",
            "sch.run",
            "sch.delete",
            "sch.toggle",
        ];
        let one_of: Vec<Value> = consts
            .iter()
            .map(|c| json!({"const":c,"description":"d"}))
            .collect();
        let body = json!({"model":"gpt-5.5","tools":[{"type":"function","name":"t1","description":"test tool","strict":true,"parameters":{"type":"object","properties":{"action":{"type":"string","enum":consts,"oneOf":one_of,"description":"Action to perform"},"target":{"type":"string","description":"Target ID"}},"required":["action"]}}]});
        assert_eq!(
            normalized(&body),
            json!({"model":"gpt-5.5","tools":[{"type":"function","name":"t1","description":"test tool","strict":true,"parameters":{"type":"object","properties":{"action":{"type":"string","enum":consts,"description":"Action to perform"},"target":{"type":"string","description":"Target ID"}},"required":["action"]}}]})
        );
    }

    // port of TestNormalizeCodexToolSchemas_DottedPropertyName / _ColonPropertyName
    #[test]
    fn tool_schema_dotted_and_colon_property_names() {
        for name in ["my.action", ":action"] {
            let body = v(&format!(
                r#"{{"tools":[{{"type":"function","name":"t","parameters":{{"type":"object","properties":{{"{name}":{{"type":"string","enum":["1","2","3","4","5","6","7","8"],"oneOf":[{}]}}}}}}}}]}}"#,
                eight("s")
            ));
            assert_eq!(
                normalized(&body),
                v(&format!(
                    r#"{{"tools":[{{"type":"function","name":"t","parameters":{{"type":"object","properties":{{"{name}":{{"type":"string","enum":["1","2","3","4","5","6","7","8"]}}}}}}}}]}}"#
                ))
            );
        }
    }

    // port of TestNormalizeCodexToolSchemas_NumericDuplicateConstNotTouched
    #[test]
    fn tool_schema_numeric_duplicate_const_not_touched() {
        let body = tool_with_prop(
            r#"{"type":"number","oneOf":[{"const":1},{"const":1.0},{"const":2},{"const":3},{"const":4},{"const":5},{"const":6},{"const":7}]}"#,
        );
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_UnicodeDuplicateConstNotTouched
    #[test]
    fn tool_schema_unicode_duplicate_const_not_touched() {
        let body = tool_with_prop(
            r#"{"type":"string","oneOf":[{"const":"a"},{"const":"\u0061"},{"const":"c"},{"const":"d"},{"const":"e"},{"const":"f"},{"const":"g"},{"const":"h"}]}"#,
        );
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_TypePreservingComparison
    #[test]
    fn tool_schema_type_preserving_comparison() {
        let body = tool_with_prop(&format!(
            r#"{{"enum":["1","2","3","4","5","6","7","8"],"oneOf":[{}]}}"#,
            eight("n")
        ));
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_BothOneOfAndAnyOfUntouched
    #[test]
    fn tool_schema_both_one_of_and_any_of_untouched() {
        let body = tool_with_prop(&format!(
            r#"{{"oneOf":[{}],"anyOf":[{}]}}"#,
            eight("s"),
            eight("s")
        ));
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_MigratesConstBranchesToEnum
    #[test]
    fn tool_schema_migrates_const_branches_to_enum() {
        let one_of: Vec<Value> = (1..=10).map(|i| json!({"const":format!("m{i}")})).collect();
        let enum_vals: Vec<Value> = (1..=10).map(|i| json!(format!("m{i}"))).collect();
        let body = tool_with_prop(&json!({"type":"string","oneOf":one_of}).to_string());
        let out = normalized(&body);
        assert_eq!(
            out,
            tool_with_prop(&json!({"type":"string","enum":enum_vals}).to_string())
        );
        // enum appended at the end, union removed in place
        assert_eq!(
            out["tools"][0]["parameters"]["properties"]["val"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["type", "enum"]
        );
    }

    // port of TestNormalizeCodexToolSchemas_NonMatchingEnumNotTouched
    #[test]
    fn tool_schema_non_matching_enum_not_touched() {
        let body = tool_with_prop(&format!(
            r#"{{"type":"string","enum":["1","2","3","4","5","6","7","8","extra"],"oneOf":[{}]}}"#,
            eight("s")
        ));
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_NonConstUnionNotTouched
    #[test]
    fn tool_schema_non_const_union_not_touched() {
        let body = tool_with_prop(
            r#"{"oneOf":[{"type":"string","pattern":"^[a-z]+$"},{"type":"number","minimum":0},{"type":"boolean"},{"type":"null"},{"type":"array"},{"type":"object"},{"type":"integer"},{"type":"string","pattern":"^[0-9]+$"}]}"#,
        );
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_SimpleToolPreserved
    #[test]
    fn tool_schema_simple_tool_preserved() {
        let body = v(
            r#"{"model":"gpt-5.5","tools":[{"type":"function","name":"lookup","strict":true,"parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}]}"#,
        );
        assert_eq!(normalized(&body), body);
    }

    // port of TestNormalizeCodexToolSchemas_NamespaceToolSimplified
    #[test]
    fn tool_schema_namespace_tool_simplified() {
        let body = v(&format!(
            r#"{{"tools":[{{"type":"namespace","name":"mcp","tools":[{{"type":"function","name":"complex_tool","parameters":{{"type":"object","properties":{{"action":{{"type":"string","oneOf":[{}]}}}}}}}}]}}]}}"#,
            eight("s")
        ));
        assert_eq!(
            normalized(&body),
            v(
                r#"{"tools":[{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"complex_tool","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["1","2","3","4","5","6","7","8"]}}}}]}]}"#
            )
        );
    }

    // port of TestNormalizeCodexToolSchemas_StripsUnsupportedUnicodePropertyEscapePatterns
    #[test]
    fn tool_schema_strips_unicode_property_escape_patterns() {
        let body = v(
            r#"{"tools":[{"type":"function","name":"Artifact","parameters":{"type":"object","properties":{"field":{"type":"string","description":"field to edit","pattern":"^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$"},"asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}},"required":["field"]}}]}"#,
        );
        let out = normalized(&body);
        assert_eq!(
            out,
            v(
                r#"{"tools":[{"type":"function","name":"Artifact","parameters":{"type":"object","properties":{"field":{"type":"string","description":"field to edit"},"asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}},"required":["field"]}}]}"#
            )
        );
        assert_eq!(normalized(&out).to_string(), out.to_string());
    }

    // port of TestNormalizeCodexToolSchemas_StripsOctalNULPatternEscape
    #[test]
    fn tool_schema_strips_octal_nul_pattern_escape() {
        let body = v(
            r#"{"tools":[{"type":"function","name":"Artifact","parameters":{"type":"object","properties":{"file_paths":{"type":"array","minItems":1,"items":{"type":"string","minLength":1,"maxLength":1024,"pattern":"^[^\\0]*$"}},"asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},"hex_nul":{"type":"string","pattern":"^[^\\x00]*$"}},"required":["file_paths"]}}]}"#,
        );
        assert_eq!(
            normalized(&body),
            v(
                r#"{"tools":[{"type":"function","name":"Artifact","parameters":{"type":"object","properties":{"file_paths":{"type":"array","minItems":1,"items":{"type":"string","minLength":1,"maxLength":1024}},"asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},"hex_nul":{"type":"string","pattern":"^[^\\x00]*$"}},"required":["file_paths"]}}]}"#
            )
        );
    }

    // port of TestNormalizeCodexToolSchemas_PreservesNonSchemaPatternKeys
    #[test]
    fn tool_schema_preserves_non_schema_pattern_keys() {
        let body = v(
            r#"{"tools":[{"type":"function","name":"config_tool","parameters":{"type":"object","properties":{"regex_config":{"type":"object","default":{"pattern":"\\p{L}+"},"enum":[{"pattern":"\\p{N}+"}]},"real_schema":{"type":"string","pattern":"\\p{L}+"}}}}]}"#,
        );
        assert_eq!(
            normalized(&body),
            v(
                r#"{"tools":[{"type":"function","name":"config_tool","parameters":{"type":"object","properties":{"regex_config":{"type":"object","default":{"pattern":"\\p{L}+"},"enum":[{"pattern":"\\p{N}+"}]},"real_schema":{"type":"string"}}}}]}"#
            )
        );
    }

    // port of TestNormalizeCodexToolSchemas_CoversAllSchemaKeywordLocations
    #[test]
    fn tool_schema_covers_all_schema_keyword_locations() {
        let body = v(
            r#"{"tools":[{"type":"function","name":"deep_tool","parameters":{"type":"object","$defs":{"custom_type":{"type":"string","pattern":"\\p{L}+"}},"additionalProperties":{"type":"string","pattern":"\\p{N}+"},"patternProperties":{"^s_":{"type":"string","pattern":"\\p{M}+"}},"if":{"properties":{"flag":{"type":"string","pattern":"\\p{P}+"}}},"then":{"properties":{"val":{"type":"string","pattern":"\\p{S}+"}}},"else":{"properties":{"other":{"type":"string","pattern":"\\p{Z}+"}}}}}]}"#,
        );
        assert_eq!(
            normalized(&body),
            v(
                r#"{"tools":[{"type":"function","name":"deep_tool","parameters":{"type":"object","$defs":{"custom_type":{"type":"string"}},"additionalProperties":{"type":"string"},"patternProperties":{"^s_":{"type":"string"}},"if":{"properties":{"flag":{"type":"string"}}},"then":{"properties":{"val":{"type":"string"}}},"else":{"properties":{"other":{"type":"string"}}}}}]}"#
            )
        );
    }

    // port of TestNormalizeCodexToolSchemas_MalformedOrEmptyParametersFallback
    #[test]
    fn tool_schema_malformed_or_empty_parameters_fallback() {
        for raw in [
            r#"{"model":"gpt-5.6","tools":[{"type":"function","name":"t","parameters":null}]}"#,
            r#"{"model":"gpt-5.6","tools":[{"type":"function","name":"t","parameters":"not_an_object"}]}"#,
            r#"{"model":"gpt-5.6","tools":[{"type":"function","name":"t","parameters":{"type":"object"}}]}"#,
            r#"{"model":"gpt-5.6","tools":[]}"#,
            r#"{"model":"gpt-5.6"}"#,
        ] {
            assert_eq!(normalized(&v(raw)), v(raw));
        }
    }

    // port of TestNormalizeCodexToolSchemas_JSONUnicodeEscapeBypassPrevention
    #[test]
    fn tool_schema_json_unicode_escape_bypass_prevention() {
        let body = v(
            r#"{"tools":[{"type":"function","name":"t","parameters":{"type":"object","properties":{"p1":{"type":"string","pattern":"\u005c\u0070{L}+"},"p2":{"type":"string","pattern":"\u005cp{Cc}"},"p3":{"type":"string","pattern":"\u005c\u0050{N}+"},"valid":{"type":"string","pattern":"^[0-9a-f]{32}$"}}}}]}"#,
        );
        assert_eq!(
            normalized(&body),
            v(
                r#"{"tools":[{"type":"function","name":"t","parameters":{"type":"object","properties":{"p1":{"type":"string"},"p2":{"type":"string"},"p3":{"type":"string"},"valid":{"type":"string","pattern":"^[0-9a-f]{32}$"}}}}]}"#
            )
        );
    }

    // port of TestNormalizeCodexToolSchemas_PatternPropertiesKeySanitization
    #[test]
    fn tool_schema_pattern_properties_key_sanitization() {
        let body = v(
            r#"{"tools":[{"type":"function","name":"t","parameters":{"type":"object","patternProperties":{"^\\p{L}+$":{"type":"string"},"^[a-z]+$":{"type":"number"}}}}]}"#,
        );
        assert_eq!(
            normalized(&body),
            v(
                r#"{"tools":[{"type":"function","name":"t","parameters":{"type":"object","patternProperties":{"^[a-z]+$":{"type":"number"}}}}]}"#
            )
        );
    }

    #[test]
    fn canonical_number_equates_spellings() {
        assert_eq!(canonical_number("1"), canonical_number("1.0"));
        assert_eq!(canonical_number("100"), canonical_number("1e2"));
        assert_eq!(canonical_number("0.50"), canonical_number("5e-1"));
        assert_eq!(canonical_number("-0"), "0");
        assert_ne!(canonical_number("1"), canonical_number("-1"));
    }

    // ---- terminal incomplete (helps/codex_terminal_incomplete_test.go) ----

    // port of TestHasMeaningfulCodexOutputDelta
    #[test]
    fn has_meaningful_output_delta() {
        for raw in [
            r#"{"type":"response.output_text.delta","delta":""}"#,
            r#"{"type":"response.output_text.delta","delta":"   "}"#,
            r#"{"type":"response.reasoning_text.delta","delta":""}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":""}"#,
        ] {
            assert!(!has_meaningful_codex_output_delta(&v(raw)), "{raw}");
        }
        for raw in [
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            r#"{"type":"response.reasoning_text.delta","delta":"thinking"}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"id\":1}"}"#,
        ] {
            assert!(has_meaningful_codex_output_delta(&v(raw)), "{raw}");
        }
    }

    // port of TestIsCodexTerminalEmptyIncomplete
    #[test]
    fn terminal_empty_incomplete() {
        let true_empty = v(
            r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
        );
        assert!(is_codex_terminal_empty_incomplete(&true_empty, 0, false));
        assert!(!is_codex_terminal_empty_incomplete(&true_empty, 0, true));
        assert!(!is_codex_terminal_empty_incomplete(&true_empty, 1, false));
        for raw in [
            r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":5}}}"#,
            r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":0.5}}}"#,
            r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{}}}"#,
            r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":null}}}"#,
            r#"{"type":"response.incomplete","response":{"id":"r1","output":[{"type":"message"}],"usage":{"output_tokens":0}}}"#,
            r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
        ] {
            assert!(
                !is_codex_terminal_empty_incomplete(&v(raw), 0, false),
                "{raw}"
            );
        }
    }

    // ---- non-stream assembly (codex_executor_stream_output_test.go) -------

    // port of TestCodexExecutorExecute_NonEmptyCompletionOutputHydratesMissingItemID
    #[test]
    fn non_stream_hydrates_missing_item_id() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_123\",\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"weather\",\"arguments\":\"{}\"},\"output_index\":0}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_done_existing\",\"type\":\"function_call\",\"call_id\":\"call_existing\",\"name\":\"other\",\"arguments\":\"{}\"},\"output_index\":1}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"id\":null,\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"weather-terminal\",\"arguments\":\"{}\"},{\"id\":\"fc_existing\",\"type\":\"function_call\",\"call_id\":\"call_existing\",\"name\":\"preserved\",\"arguments\":\"{}\"}]}}\n\n",
        );
        assert_eq!(
            assemble_non_stream(sse).unwrap(),
            v(
                r#"{"id":"resp_1","object":"response","status":"completed","output":[{"id":"fc_123","type":"function_call","call_id":"call_123","name":"weather-terminal","arguments":"{}"},{"id":"fc_existing","type":"function_call","call_id":"call_existing","name":"preserved","arguments":"{}"}]}"#
            )
        );
    }

    // port of TestCodexExecutorExecute_EmptyStreamCompletionOutputUsesOutputItemDone
    #[test]
    fn non_stream_empty_output_uses_output_item_done() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]},\"output_index\":0}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1775555723,\"status\":\"completed\",\"model\":\"gpt-5.4-mini-2026-03-17\",\"output\":[],\"usage\":{\"input_tokens\":8,\"output_tokens\":28,\"total_tokens\":36}}}\n\n",
        );
        assert_eq!(
            assemble_non_stream(sse).unwrap(),
            v(
                r#"{"id":"resp_1","object":"response","created_at":1775555723,"status":"completed","model":"gpt-5.4-mini-2026-03-17","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":8,"output_tokens":28,"total_tokens":36,"output_tokens_details":{"reasoning_tokens":0},"input_tokens_details":{"cached_tokens":0}}}"#
            )
        );
    }

    #[test]
    fn non_stream_orders_items_by_index_then_fallback() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"c\"}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"b\"},\"output_index\":1}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"a\"},\"output_index\":0}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":\"not-json\",\"output_index\":2}\n",
            "data:{\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n",
        );
        assert_eq!(
            assemble_non_stream(sse).unwrap(),
            v(r#"{"id":"r","output":[{"id":"a"},{"id":"b"},{"id":"c"}]}"#)
        );
    }

    // port of TestCodexExecutorExecuteSurfacesTerminalStreamError
    #[test]
    fn non_stream_surfaces_terminal_stream_error() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.5\"}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"code\":\"context_length_exceeded\",\"message\":\"Your input exceeds the context window of this model. Please adjust your input and try again.\",\"param\":\"input\"},\"sequence_number\":2}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"x\"}}}\n\n",
        );
        let err = assemble_non_stream_detailed(sse).unwrap_err();
        assert_eq!(err.status, 400);
        assert!(!err.request_scoped);
        assert_eq!(
            v(&err.message),
            v(
                r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","code":"context_too_large"}}"#
            )
        );
        assert_eq!(assemble_non_stream(sse).unwrap_err(), err.message);
    }

    // port of TestCodexExecutorExecuteIncompleteResponseIsSuccessful (Responses output)
    #[test]
    fn non_stream_incomplete_response_is_successful() {
        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.5\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n";
        assert_eq!(
            assemble_non_stream(sse).unwrap(),
            v(
                r#"{"id":"resp_1","model":"gpt-5.5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,"output_tokens_details":{"reasoning_tokens":0},"input_tokens_details":{"cached_tokens":0}}}"#
            )
        );
    }

    // port of TestCodexExecutorExecuteExplicitTerminalFailureIsNotRequestScoped
    #[test]
    fn non_stream_explicit_terminal_failure_is_not_request_scoped() {
        let sse = "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"code\":\"invalid_value\",\"message\":\"Invalid input.\"}}\n\n";
        let err = assemble_non_stream_detailed(sse).unwrap_err();
        assert_eq!(
            err,
            CodexError {
                status: 400,
                message: r#"{"error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."}}"#.to_string(),
                request_scoped: false,
                credential_scoped: false,
                retry_after: None,
            }
        );
    }

    // port of TestCodexExecutorExecuteMissingCompletionIsRequestScoped
    #[test]
    fn non_stream_missing_completion_is_request_scoped() {
        let sse = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.5\"}}\n\n";
        let err = assemble_non_stream_detailed(sse).unwrap_err();
        assert_eq!(err, codex_incomplete_stream_error());
        assert_eq!(err.status, 408);
        assert!(err.request_scoped);
        assert_eq!(
            assemble_non_stream(sse),
            Err(CODEX_INCOMPLETE_STREAM_MESSAGE.to_string())
        );
        // response.done is NOT terminal on the non-stream path.
        assert_eq!(
            assemble_non_stream("data: {\"type\":\"response.done\",\"response\":{}}\n"),
            Err(CODEX_INCOMPLETE_STREAM_MESSAGE.to_string())
        );
    }

    #[test]
    fn non_stream_zero_token_incomplete_is_failure() {
        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"r\",\"output\":[],\"usage\":{\"output_tokens\":0}}}\n";
        let err = assemble_non_stream_detailed(sse).unwrap_err();
        assert_eq!(err, codex_empty_incomplete_stream_error());
        assert_eq!(err.status, 502);
    }

    // ---- terminal error classification ------------------------------------

    // port of TestCodexTerminalStreamContextLengthErrFromResponseFailed
    #[test]
    fn context_length_err_from_response_failed() {
        let err = codex_terminal_stream_context_length_err(&v(r#"{"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."}}}"#)).unwrap();
        assert_eq!(err.status, 400);
        assert_eq!(
            v(&err.message),
            v(
                r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","code":"context_too_large"}}"#
            )
        );
    }

    // port of TestCodexTerminalStreamContextLengthErrFromTopLevelError
    #[test]
    fn context_length_err_from_top_level_error() {
        let err = codex_terminal_stream_context_length_err(&v(r#"{"type":"error","code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again.","sequence_number":2}"#)).unwrap();
        assert_eq!(err.status, 400);
        assert_eq!(
            v(&err.message),
            v(
                r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","code":"context_too_large"}}"#
            )
        );
    }

    // port of TestCodexTerminalStreamContextLengthErrIgnoresOtherTerminalErrors /
    // TestCodexTerminalStreamErrIgnoresRateLimitTerminalErrors
    #[test]
    fn stream_err_ignores_rate_limit_terminal_errors() {
        let event = v(
            r#"{"type":"error","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"Rate limit reached."}}"#,
        );
        assert!(codex_terminal_stream_context_length_err(&event).is_none());
        assert!(codex_terminal_stream_err(&event).is_none());
    }

    // port of TestCodexTerminalFailureErrClassifiesStatus
    #[test]
    fn terminal_failure_err_classifies_status() {
        for (event, want) in [
            (
                r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."}}"#,
                400,
            ),
            (
                r#"{"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk."}}"#,
                400,
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"type":"authentication_error","code":"invalid_api_key","message":"Invalid token."}}}"#,
                401,
            ),
            (
                r#"{"type":"error","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"Rate limit reached."}}"#,
                429,
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"type":"upstream_error","code":"unknown","message":"Upstream failed."}}}"#,
                502,
            ),
            (
                r#"{"type":"error","error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}}"#,
                502,
            ),
            (
                r#"{"type":"error","error":{"type":"invalid_request_error","code":"model_not_found","message":"The model gpt-5.5 does not exist or you do not have access to it."}}"#,
                404,
            ),
        ] {
            let (err, _) = codex_terminal_failure_err(&v(event)).unwrap();
            assert_eq!(err.status, want, "{event}");
        }
        assert!(codex_terminal_failure_err(&v(r#"{"type":"response.completed"}"#)).is_none());
    }

    // port of TestCodexTerminalStreamErrHandlesUsageLimitErrorEvent
    #[test]
    fn stream_err_handles_usage_limit_error_event() {
        let (err, body) = codex_terminal_stream_err(&v(r#"{"type":"error","error":{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":300}}"#)).unwrap();
        assert_eq!(err.status, 429);
        assert!(err.credential_scoped);
        assert_eq!(err.retry_after, Some(Duration::from_secs(300)));
        assert_eq!(
            body,
            r#"{"error":{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":300}}"#
        );
    }

    // port of TestCodexTerminalStreamErrHandlesUsageLimitResponseFailed
    #[test]
    fn stream_err_handles_usage_limit_response_failed() {
        let (err, _) = codex_terminal_stream_err(&v(r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","message":"usage limit reached","resets_in_seconds":60}}}"#)).unwrap();
        assert_eq!(err.status, 429);
        assert_eq!(err.retry_after, Some(Duration::from_secs(60)));
    }

    #[test]
    fn retry_after_prefers_future_resets_at() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let err = codex_status_error_at(
            429,
            r#"{"error":{"type":"usage_limit_reached","resets_at":1300,"resets_in_seconds":5}}"#,
            now,
        );
        assert_eq!(err.retry_after, Some(Duration::from_secs(300)));
        let err = codex_status_error_at(
            429,
            r#"{"error":{"type":"usage_limit_reached","resets_at":900,"resets_in_seconds":5}}"#,
            now,
        );
        assert_eq!(err.retry_after, Some(Duration::from_secs(5)));
    }

    #[test]
    fn terminal_failure_body_shapes() {
        assert_eq!(
            codex_terminal_failure_body(&v(r#"{"type":"error","sequence_number":7}"#)).unwrap(),
            r#"{"error":{"message":"upstream stream failed without error details"},"sequence_number":7}"#
        );
        assert_eq!(
            codex_terminal_failure_body(&v(r#"{"type":"error","error":"boom"}"#)).unwrap(),
            r#"{"error":{"message":"boom"}}"#
        );
        assert_eq!(
            codex_terminal_failure_body(&v(
                r#"{"type":"response.failed","response":{"error":{"code":"server_error"}}}"#
            ))
            .unwrap(),
            r#"{"error":{"code":"server_error","message":"server_error"}}"#
        );
    }

    #[test]
    fn http_status_error_classification() {
        let err = codex_status_error(401, "invalid or expired token");
        assert_eq!(
            err,
            CodexError {
                status: 401,
                message: r#"{"error":{"message":"invalid or expired token","type":"authentication_error","code":"auth_unavailable"}}"#.to_string(),
                request_scoped: false,
                credential_scoped: false,
                retry_after: None,
            }
        );
        let err = codex_status_error(500, r#"{"error":{"message":"The model is at capacity"}}"#);
        assert_eq!(err.status, 429);
        assert_eq!(
            err.message,
            r#"{"error":{"message":"The model is at capacity"}}"#
        );
        assert_eq!(
            codex_status_error(503, "upstream down").message,
            "upstream down"
        );
    }

    // ---- stream relay ------------------------------------------------------

    // port of TestCodexExecutorExecuteStream_EmptyStreamCompletionOutputUsesOutputItemDone
    #[test]
    fn stream_empty_completion_output_uses_output_item_done() {
        let mut relay = CodexStreamRelay::new("gpt-5.4-mini", false);
        let item = v(
            r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":0}"#,
        );
        assert_eq!(relay.on_event(&item), RelayStep::Forward(item.clone()));
        let done = v(
            r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":1775555723,"status":"completed","model":"gpt-5.4-mini-2026-03-17","output":[],"usage":{"input_tokens":8,"output_tokens":28,"total_tokens":36}}}"#,
        );
        assert_eq!(
            relay.on_event(&done),
            RelayStep::Terminal(v(
                r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":1775555723,"status":"completed","model":"gpt-5.4-mini-2026-03-17","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":8,"output_tokens":28,"total_tokens":36}}}"#
            ))
        );
        assert_eq!(relay.finish(), None);
    }

    #[test]
    fn stream_response_done_becomes_completed_and_native_output_is_preserved() {
        let mut relay = CodexStreamRelay::new("m", true);
        relay.on_event(&v(
            r#"{"type":"response.output_item.done","item":{"id":"x"},"output_index":0}"#,
        ));
        assert_eq!(
            relay.on_event(&v(r#"{"type":"response.done","response":{"output":[]}}"#)),
            RelayStep::Terminal(v(
                r#"{"type":"response.completed","response":{"output":[]}}"#
            ))
        );
    }

    // port of TestCodexExecutorExecuteStreamMissingCompletionIsRequestScoped
    #[test]
    fn stream_missing_completion_is_request_scoped() {
        let mut relay = CodexStreamRelay::new("gpt-5.5", false);
        relay.on_event(&v(
            r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#,
        ));
        assert_eq!(relay.finish(), Some(codex_incomplete_stream_error()));
    }

    // port of TestCodexExecutorExecuteStreamExplicitTerminalFailureIsNotSuccessful
    #[test]
    fn stream_explicit_terminal_failure_is_not_successful() {
        let mut relay = CodexStreamRelay::new("gpt-5.5", false);
        relay.on_event(&v(
            r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#,
        ));
        let step = relay.on_event(&v(r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."}}"#));
        let RelayStep::Failed(err) = step else {
            panic!("expected failure, got {step:?}");
        };
        assert_eq!(err.status, 400);
        assert!(!err.request_scoped);
        assert_eq!(relay.finish(), None);
    }

    // port of TestCodexExecutorExecuteStreamSurfacesTerminalStreamError
    #[test]
    fn stream_surfaces_context_length_terminal_error() {
        let mut relay = CodexStreamRelay::new("gpt-5.5", false);
        let step = relay.on_event(&v(r#"{"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}}"#));
        let RelayStep::Failed(err) = step else {
            panic!("expected failure, got {step:?}");
        };
        assert_eq!(err.status, 400);
        assert_eq!(
            v(&err.message),
            v(
                r#"{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error","code":"context_too_large"}}"#
            )
        );
    }

    // port of TestCodexExecutorExecuteStream_ZeroTokenIncompleteResponseIsFailure /
    // _PartialDeltasIncompleteResponseIsSuccessful / _EmptyDeltaDoesNotBypassZeroTokenFailure
    #[test]
    fn stream_zero_token_incomplete() {
        let incomplete = v(
            r#"{"type":"response.incomplete","response":{"id":"r","output":[],"usage":{"output_tokens":0}}}"#,
        );
        let mut relay = CodexStreamRelay::new("m", false);
        relay.on_event(&v(r#"{"type":"response.output_text.delta","delta":""}"#));
        assert_eq!(
            relay.on_event(&incomplete),
            RelayStep::Failed(codex_empty_incomplete_stream_error())
        );
        let mut relay = CodexStreamRelay::new("m", false);
        relay.on_event(&v(
            r#"{"type":"response.output_text.delta","delta":"partial"}"#,
        ));
        assert_eq!(
            relay.on_event(&incomplete),
            RelayStep::Terminal(incomplete.clone())
        );
    }

    // ---- Responses translator (codex_openai-responses_response_test.go) ----

    // port of TestConvertCodexResponseToOpenAIResponses_CreatedIncludesOriginalRequestModel
    #[test]
    fn rewrite_created_includes_original_request_model() {
        let model = request_model_name(&v(r#"{"model":"original-codex-model"}"#));
        for event in [
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            r#"{"type":"response.in_progress","response":{"id":"resp_1"}}"#,
        ] {
            let out = rewrite_stream_event(&v(event), &model);
            assert_eq!(
                jget(&out, "response.model"),
                Some(&json!("original-codex-model"))
            );
        }
        let keep = v(r#"{"type":"response.created","response":{"id":"r","model":"upstream"}}"#);
        assert_eq!(rewrite_stream_event(&keep, "client"), keep);
        let other = v(r#"{"type":"response.output_text.delta","delta":"x"}"#);
        assert_eq!(rewrite_stream_event(&other, "client"), other);
    }

    // port of TestConvertCodexResponseToOpenAIResponsesNonStreamIncomplete
    #[test]
    fn convert_non_stream_incomplete() {
        let out = convert_codex_response_to_openai_responses_non_stream(&v(
            r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
        ));
        assert_eq!(
            out,
            v(
                r#"{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#
            )
        );
    }

    // ---- usage details (helps/responses_usage_helpers_test.go) -------------

    // port of TestEnsureResponsesUsageDetails_NonStreamJSON / _WithDataSubstring
    #[test]
    fn usage_details_non_stream_json() {
        let mut body = v(
            r#"{"id":"resp_1","object":"response","status":"completed","usage":{"input_tokens":84,"output_tokens":16,"total_tokens":100}}"#,
        );
        ensure_responses_usage_details(&mut body);
        assert_eq!(
            body,
            v(
                r#"{"id":"resp_1","object":"response","status":"completed","usage":{"input_tokens":84,"output_tokens":16,"total_tokens":100,"output_tokens_details":{"reasoning_tokens":0},"input_tokens_details":{"cached_tokens":0}}}"#
            )
        );
    }

    // port of TestEnsureResponsesUsageDetails_PreservesExistingDetails (JSON form)
    #[test]
    fn usage_details_preserves_existing() {
        let raw = r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":3},"output_tokens":4,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":14}}}"#;
        let mut body = v(raw);
        ensure_responses_usage_details(&mut body);
        assert_eq!(body, v(raw));
    }

    // port of TestEnsureResponsesUsageDetails_HandlesNullOrEmptyDetails
    #[test]
    fn usage_details_handles_null_or_empty() {
        let mut body = v(
            r#"{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":null,"output_tokens":4,"output_tokens_details":{},"total_tokens":14}}"#,
        );
        ensure_responses_usage_details(&mut body);
        assert_eq!(
            body,
            v(
                r#"{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":4,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":14}}"#
            )
        );
    }

    // port of TestEnsureResponsesUsageDetails_NonJSONAndDone (JSON form)
    #[test]
    fn usage_details_leaves_non_usage_payloads() {
        for raw in [
            r#"{"type":"response.output_item.added"}"#,
            r#"{"object":"response.compaction","usage":{}}"#,
        ] {
            let mut body = v(raw);
            ensure_responses_usage_details(&mut body);
            assert_eq!(body, v(raw));
        }
    }

    // ---- image generation tool (ported, not applied) ----------------------

    #[test]
    fn image_generation_tool_rules() {
        let mut body = v(r#"{"model":"gpt-5.5"}"#);
        ensure_image_generation_tool(&mut body, "gpt-5.5", false, &[]);
        assert_eq!(
            body,
            v(r#"{"model":"gpt-5.5","tools":[{"type":"image_generation","output_format":"png"}]}"#)
        );
        let mut body = v(r#"{"tools":[{"type":"function","name":"x"}]}"#);
        ensure_image_generation_tool(&mut body, "gpt-5.5", false, &[]);
        assert_eq!(
            body,
            v(
                r#"{"tools":[{"type":"function","name":"x"},{"type":"image_generation","output_format":"png"}]}"#
            )
        );
        for (model, free) in [("gpt-5.3-codex-spark", false), ("gpt-5.5", true)] {
            let mut body = v(r#"{"tools":[]}"#);
            ensure_image_generation_tool(&mut body, model, free, &[]);
            assert_eq!(body, v(r#"{"tools":[]}"#));
        }
        let mut body = v(
            r#"{"tools":[{"type":"namespace","name":"image_gen","tools":[{"type":"function","name":"imagegen"}]}]}"#,
        );
        let before = body.clone();
        ensure_image_generation_tool(&mut body, "gpt-5.5", false, &[]);
        assert_eq!(body, before);
    }
}
