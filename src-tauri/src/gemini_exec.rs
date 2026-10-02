// A faithful port of CLIProxyAPI's Gemini API-key executor request/response
// handling. Helpers kept for parity (signature classification, header rules)
// are not all reached from the public entry points yet.
#![allow(dead_code)]

//! Gemini API-key upstream: request building and response post-processing,
//! ported from CLIProxyAPI (upstream revision `ed980be`, 2026-09-26):
//!
//! - `internal/runtime/executor/gemini_executor.go` — `Execute` /
//!   `ExecuteStream` (URL, headers, body normalisation), `resolveGeminiBaseURL`,
//!   `applyGeminiHeaders`, `capGeminiMaxOutputTokens`;
//! - `internal/runtime/executor/helps/{payload_mutations,gemini_content_turns,usage_helpers}.go`
//!   — `SetStringIfDifferent`, `EnsureGemini{Leading,Trailing,Boundary}UserContent`,
//!   `FilterSSEUsageMetadata`, `StripUsageMetadataFromJSON`, `JSONPayload`;
//! - `internal/signature/{gemini_sanitize,gemini_validation,provider_compatibility}.go`
//!   — `SanitizeGeminiRequestThoughtSignatures` and the Gemini half of the
//!   signature classifier it relies on;
//! - `internal/util/header_helpers.go` — `ApplyCustomHeadersFromAttrs`;
//! - `internal/translator/gemini/gemini/gemini_gemini_response.go` — the
//!   Gemini→Gemini response passthrough.
//!
//! The caller translates the client request to a Gemini `generateContent` body
//! and applies thinking (`thinking::apply_thinking(.., "gemini", "gemini", ..)`)
//! BEFORE [`build_request`], matching Go's order (translate → ApplyRequestThinking
//! → the steps ported here).
//!
//! # Deliberate differences from the Go source
//!
//! - Out of scope and absent: Vertex, the AI Studio websocket, the native
//!   Interactions executor, `CountTokens`, `fixGeminiImageAspectRatio` (image
//!   generation), payload-config rules (`ApplyPayloadConfigWithRequest`), usage
//!   reporting, request/response logging and `opts.Alt` other than empty (the
//!   default `alt=sse` stream and plain non-stream JSON).
//! - No model registry: [`lookup_model_output_limits`] answers `None`, so
//!   `capGeminiMaxOutputTokens` leaves the value unchanged for every model (Go's
//!   behaviour for an unknown model). [`cap_gemini_max_output_tokens_with`]
//!   takes the limits explicitly.
//! - No per-credential attributes: Go's `header:<Name>` auth attributes (custom
//!   headers, `$Client-Header` passthrough, `$CPA-SESSION-ID`) do not exist in
//!   Termory, so [`build_request`] applies an empty attribute set and the
//!   client headers are never forwarded — exactly Go's result for an auth with
//!   no `header:` attributes. The rule itself is ported as
//!   [`extract_custom_headers`]; `$CPA-SESSION-ID` always resolves empty (no
//!   session-affinity resolver), so such a header is omitted.
//! - Signature classification for a Gemini target ports only what decides the
//!   Gemini outcome: the provider prefix, the bypass sentinels, `sealed.v1.`
//!   and the Gemini protobuf field-2 envelope. Go additionally runs the GPT,
//!   Claude (strict / CAIS) and Kimi validators; for a Gemini target those can
//!   only turn "unknown" into another non-Gemini provider, which is handled
//!   identically. Go probes GPT/Claude BEFORE Gemini, so a string accepted by
//!   both a Claude validator and the Gemini envelope check would be "claude"
//!   there and "gemini" here; Go pins that the families are disjoint
//!   (`TestGeminiEnvelopeNeverClaimsClaudeSignatures`, and a Gemini envelope
//!   must start 0x12 / base64 'E' while CAIS starts 0x08 / 'C').
//! - `serde_json::Value` cannot hold duplicate object keys, so Go's handling
//!   of a repeated `"thoughtSignature"` key (count != 1 ⇒ rewrite) cannot
//!   arise; the last duplicate wins at parse time.
//! - Bodies are `Value`s: Go's branches for invalid JSON bytes are unreachable.
//! - Debug logging is omitted.
//! - [`rewrite_stream_event`] receives the event's JSON (the `data:` payload).
//!   Go filters each raw SSE line; the event is treated as having arrived on a
//!   `data:` line, which is how the Gemini API delivers it. A non-object event
//!   (which Go's `JSONPayload` drops) is answered with `Value::Null`, meaning
//!   "emit no frame".

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// gemini_executor.go constants
// ---------------------------------------------------------------------------

/// port of glEndpoint (gemini_executor.go)
const GL_ENDPOINT: &str = "https://generativelanguage.googleapis.com";
/// port of glAPIVersion (gemini_executor.go)
const GL_API_VERSION: &str = "v1beta";

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// Build the upstream request for a Gemini API-key upstream: returns (url,
/// headers, body bytes). `base_url` = e.g. https://generativelanguage.googleapis.com
/// (or a gateway root); `model` = upstream model id (suffix already stripped);
/// `stream` = `streamGenerateContent?alt=sse` vs `generateContent`; `body` = a
/// Gemini generateContent body (already translated, thinking applied);
/// `client_headers` = incoming client headers (lowercase name, value).
///
/// port of the request half of GeminiExecutor.Execute / ExecuteStream
/// (gemini_executor.go), from `SetStringIfDifferent(body, "model", ...)` to
/// the header setup.
pub fn build_request(
    base_url: &str,
    model: &str,
    stream: bool,
    body: &Value,
    api_key: &str,
    client_headers: &[(String, String)],
) -> (String, Vec<(String, String)>, Vec<u8>) {
    let mut body = body.clone();
    set_string_if_different(&mut body, "model", model);
    cap_gemini_max_output_tokens(&mut body, model);
    sanitize_gemini_request_thought_signatures(&mut body, "contents");

    let base = resolve_gemini_base_url(base_url);
    let url = if stream {
        // ExecuteStream: EnsureGeminiBoundaryUserContent, then `?alt=sse` (opts.Alt == "").
        ensure_gemini_boundary_user_content(&mut body, "contents");
        format!("{base}/{GL_API_VERSION}/models/{model}:streamGenerateContent?alt=sse")
    } else {
        // Execute with action "generateContent": leading, then trailing.
        ensure_gemini_leading_user_content(&mut body, "contents");
        ensure_gemini_trailing_user_content(&mut body, "contents");
        format!("{base}/{GL_API_VERSION}/models/{model}:generateContent")
    };

    sj_delete(&mut body, "session_id");

    let mut headers: Vec<(String, String)> = Vec::new();
    header_set(&mut headers, "Content-Type", "application/json");
    if !api_key.is_empty() {
        header_set(&mut headers, "x-goog-api-key", api_key);
    }
    apply_gemini_headers(&mut headers, &[], client_headers);

    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    (url, headers, bytes)
}

/// Non-stream: the final response JSON. Go relays the upstream body through
/// the Gemini→Gemini passthrough (`PassthroughGeminiResponseNonStream`), so
/// this is the identity.
pub fn finalize_non_stream(body: &Value) -> Value {
    passthrough_gemini_response_non_stream(body)
}

/// Stream: per-event rewrite. Port of the ExecuteStream loop body
/// (gemini_executor.go): `FilterSSEUsageMetadata` → `JSONPayload` →
/// `PassthroughGeminiResponseStream`. Non-terminal events carrying
/// `usageMetadata` have it renamed to `cpaUsageMetadata` (Go's behaviour, which
/// the Gemini client sees verbatim). `Value::Null` means "no frame".
pub fn rewrite_stream_event(event: &Value) -> Value {
    if !event.is_object() {
        // jsonPayload: only a `{...}` payload is forwarded.
        return Value::Null;
    }
    let filtered = filter_sse_usage_metadata(event);
    passthrough_gemini_response_stream(&filtered)
}

// ---------------------------------------------------------------------------
// gjson / sjson equivalents over serde_json::Value
// ---------------------------------------------------------------------------

/// gjson.Get: dotted path, numeric segments index arrays.
fn gj_get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn gj_get_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(map) => map.get_mut(seg)?,
            Value::Array(items) => items.get_mut(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// gjson.Result.String(): strings verbatim, null/missing empty, the rest raw.
fn gj_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// gjson.Result.Int() for a number.
fn gj_int(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// sjson.Set: creates missing objects along the path, replaces in place.
fn sj_set(root: &mut Value, path: &str, value: Value) {
    let segments: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for (i, seg) in segments.iter().enumerate() {
        let Value::Object(map) = cur else {
            return;
        };
        if i + 1 == segments.len() {
            map.insert((*seg).to_string(), value);
            return;
        }
        let child = map
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !child.is_object() {
            *child = Value::Object(Map::new());
        }
        cur = child;
    }
}

/// sjson.Delete: removes the key, keeping the order of its siblings.
fn sj_delete(root: &mut Value, path: &str) {
    let (parent, key) = match path.rsplit_once('.') {
        Some((parent_path, key)) => (gj_get_mut(root, parent_path), key),
        None => (Some(root), path),
    };
    if let Some(Value::Object(map)) = parent {
        map.shift_remove(key);
    }
}

// ---------------------------------------------------------------------------
// helps/payload_mutations.go
// ---------------------------------------------------------------------------

/// port of SetStringIfDifferent (helps/payload_mutations.go)
fn set_string_if_different(payload: &mut Value, path: &str, value: &str) {
    if let Some(Value::String(current)) = gj_get(payload, path) {
        if current == value {
            return;
        }
    }
    sj_set(payload, path, Value::from(value));
}

// ---------------------------------------------------------------------------
// gemini_executor.go helpers
// ---------------------------------------------------------------------------

/// port of resolveGeminiBaseURL (gemini_executor.go); the auth attribute is
/// the `base_url` argument.
fn resolve_gemini_base_url(base_url: &str) -> String {
    let mut base = GL_ENDPOINT.to_string();
    let custom = base_url.trim();
    if !custom.is_empty() {
        base = custom.trim_end_matches('/').to_string();
    }
    if base.is_empty() {
        return GL_ENDPOINT.to_string();
    }
    base
}

/// port of applyGeminiHeaders (gemini_executor.go) →
/// util.ApplyCustomHeadersFromAttrs (util/header_helpers.go). `attrs` are the
/// auth attributes as (key, value) pairs.
fn apply_gemini_headers(
    headers: &mut Vec<(String, String)>,
    attrs: &[(String, String)],
    client_headers: &[(String, String)],
) {
    for (name, value) in extract_custom_headers(attrs, client_headers) {
        // port of applyCustomHeaders (util/header_helpers.go)
        if name.is_empty() || value.is_empty() {
            continue;
        }
        header_set(headers, &name, &value);
    }
}

/// port of extractCustomHeaders (util/header_helpers.go). Go iterates a map
/// (random order); attribute order is kept here. `$CPA-SESSION-ID` has no
/// resolver in Termory and resolves empty, which omits the header.
fn extract_custom_headers(
    attrs: &[(String, String)],
    client_headers: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in attrs {
        let Some(name) = key.strip_prefix("header:") else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let mut val = value.trim().to_string();
        if val.is_empty() {
            continue;
        }
        if let Some(rest) = val.strip_prefix('$') {
            if rest.trim().eq_ignore_ascii_case("CPA-SESSION-ID") {
                continue;
            }
        }
        if val.to_uppercase().contains("$CPA-SESSION-ID") {
            continue;
        }
        if let Some(rest) = val.strip_prefix('$') {
            let var_name = rest.trim();
            if var_name.is_empty() {
                continue;
            }
            let client_val = client_headers
                .iter()
                .find(|(k, v)| k.eq_ignore_ascii_case(var_name) && !v.is_empty())
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            if client_val.is_empty() {
                continue;
            }
            val = client_val;
        }
        match out.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = val,
            None => out.push((name.to_string(), val)),
        }
    }
    out
}

/// http.Header.Set: replaces every value of the (case-insensitive) name.
fn header_set(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    headers.push((canonical_header_key(name), value.to_string()));
}

/// port of http.CanonicalHeaderKey (net/http): a key holding a byte outside the
/// token set is returned unchanged.
fn canonical_header_key(key: &str) -> String {
    let valid = key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid {
        return key.to_string();
    }
    let mut upper = true;
    key.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// Stand-in for registry.LookupModelInfo(model, "gemini")
/// (internal/registry/model_registry.go), reduced to the two fields
/// capGeminiMaxOutputTokens reads: `(OutputTokenLimit, MaxCompletionTokens)`.
/// Termory has no model registry, so every model is unknown.
fn lookup_model_output_limits(_model: &str) -> Option<(i64, i64)> {
    None
}

/// port of capGeminiMaxOutputTokens (gemini_executor.go)
fn cap_gemini_max_output_tokens(body: &mut Value, model_name: &str) {
    cap_gemini_max_output_tokens_with(body, lookup_model_output_limits(model_name));
}

/// capGeminiMaxOutputTokens with the registry answer passed in:
/// `limits = Some((OutputTokenLimit, MaxCompletionTokens))`.
fn cap_gemini_max_output_tokens_with(body: &mut Value, limits: Option<(i64, i64)>) {
    let max_out = match gj_get(body, "generationConfig.maxOutputTokens") {
        Some(v @ Value::Number(_)) => gj_int(v),
        _ => return,
    };
    let Some((output_limit, max_completion)) = limits else {
        return;
    };
    let mut limit = output_limit;
    if limit <= 0 {
        limit = max_completion;
    }
    if limit <= 0 || max_out <= limit {
        return;
    }
    sj_set(body, "generationConfig.maxOutputTokens", Value::from(limit));
}

// ---------------------------------------------------------------------------
// helps/gemini_content_turns.go
// ---------------------------------------------------------------------------

/// port of emptyGeminiUserTurnJSON (helps/gemini_content_turns.go)
fn empty_gemini_user_turn() -> Value {
    json!({"role":"user","parts":[{"text":""}]})
}

/// port of EnsureGeminiLeadingUserContent (helps/gemini_content_turns.go)
fn ensure_gemini_leading_user_content(payload: &mut Value, path: &str) {
    if gj_string(gj_get(payload, &format!("{path}.0.role"))) != "model" {
        return;
    }
    let Some(Value::Array(contents)) = gj_get_mut(payload, path) else {
        return;
    };
    if contents.is_empty() {
        return;
    }
    contents.insert(0, empty_gemini_user_turn());
}

/// port of contentHasFunctionResponse (helps/gemini_content_turns.go)
fn content_has_function_response(content: &Value) -> bool {
    let Some(Value::Array(parts)) = gj_get(content, "parts") else {
        return false;
    };
    parts
        .iter()
        .any(|part| gj_get(part, "functionResponse").is_some())
}

/// port of EnsureGeminiTrailingUserContent (helps/gemini_content_turns.go)
fn ensure_gemini_trailing_user_content(payload: &mut Value, path: &str) {
    let Some(Value::Array(contents)) = gj_get_mut(payload, path) else {
        return;
    };
    let Some(last) = contents.last() else {
        return;
    };
    let last_role = gj_string(gj_get(last, "role"));
    if (last_role != "model" && last_role != "assistant") || content_has_function_response(last) {
        return;
    }
    contents.push(empty_gemini_user_turn());
}

/// port of EnsureGeminiBoundaryUserContent (helps/gemini_content_turns.go)
fn ensure_gemini_boundary_user_content(payload: &mut Value, path: &str) {
    ensure_gemini_leading_user_content(payload, path);
    ensure_gemini_trailing_user_content(payload, path);
}

// ---------------------------------------------------------------------------
// helps/usage_helpers.go (stream usage filter)
// ---------------------------------------------------------------------------

/// Go's `stopChunkWithoutUsage` sync.Map: traceId → remembered at. Entries
/// expire after 10 minutes (`time.AfterFunc(10*time.Minute, Delete)`).
fn stop_chunk_without_usage() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

const STOP_CHUNK_TTL: Duration = Duration::from_secs(10 * 60);

/// port of rememberStopWithoutUsage (helps/usage_helpers.go)
fn remember_stop_without_usage(trace_id: &str) {
    let mut map = stop_chunk_without_usage()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.retain(|_, at| at.elapsed() < STOP_CHUNK_TTL);
    map.insert(trace_id.to_string(), Instant::now());
}

/// `stopChunkWithoutUsage.Load(traceID)` + `Delete` when `consume`.
fn stop_without_usage_seen(trace_id: &str, consume: bool) -> bool {
    let mut map = stop_chunk_without_usage()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.retain(|_, at| at.elapsed() < STOP_CHUNK_TTL);
    if !map.contains_key(trace_id) {
        return false;
    }
    if consume {
        map.remove(trace_id);
    }
    true
}

/// port of FilterSSEUsageMetadata (helps/usage_helpers.go), for one `data:`
/// line whose JSON is `raw`.
fn filter_sse_usage_metadata(raw: &Value) -> Value {
    let trace_id = gj_string(gj_get(raw, "traceId"));
    if is_stop_chunk_without_usage(raw) && !trace_id.is_empty() {
        remember_stop_without_usage(&trace_id);
        return raw.clone();
    }
    if !trace_id.is_empty() && has_usage_metadata(raw) && stop_without_usage_seen(&trace_id, true) {
        return raw.clone();
    }
    match strip_usage_metadata_from_json(raw) {
        Some(cleaned) => cleaned,
        None => raw.clone(),
    }
}

/// port of the finishReason probe shared by StripUsageMetadataFromJSON and
/// isStopChunkWithoutUsage (helps/usage_helpers.go).
fn finish_reason(json: &Value) -> Option<&Value> {
    gj_get(json, "candidates.0.finishReason")
        .or_else(|| gj_get(json, "response.candidates.0.finishReason"))
}

/// port of StripUsageMetadataFromJSON (helps/usage_helpers.go): `None` is Go's
/// `changed == false`.
fn strip_usage_metadata_from_json(raw: &Value) -> Option<Value> {
    let terminal = finish_reason(raw).is_some_and(|r| !gj_string(Some(r)).trim().is_empty());
    let has_usage =
        gj_get(raw, "usageMetadata").is_some() || gj_get(raw, "response.usageMetadata").is_some();
    if terminal || !has_usage {
        return None;
    }
    let mut cleaned = raw.clone();
    let mut changed = false;
    if let Some(usage) = gj_get(&cleaned, "usageMetadata").cloned() {
        sj_set(&mut cleaned, "cpaUsageMetadata", usage);
        sj_delete(&mut cleaned, "usageMetadata");
        changed = true;
    }
    if let Some(usage) = gj_get(&cleaned, "response.usageMetadata").cloned() {
        sj_set(&mut cleaned, "response.cpaUsageMetadata", usage);
        sj_delete(&mut cleaned, "response.usageMetadata");
        changed = true;
    }
    changed.then_some(cleaned)
}

/// port of hasUsageMetadata (helps/usage_helpers.go)
fn has_usage_metadata(json: &Value) -> bool {
    gj_get(json, "usageMetadata").is_some() || gj_get(json, "response.usageMetadata").is_some()
}

/// port of isStopChunkWithoutUsage (helps/usage_helpers.go)
fn is_stop_chunk_without_usage(json: &Value) -> bool {
    match finish_reason(json) {
        Some(reason) if !gj_string(Some(reason)).trim().is_empty() => !has_usage_metadata(json),
        _ => false,
    }
}

/// port of JSONPayload / jsonPayload (helps/usage_helpers.go): the JSON object
/// carried by one upstream SSE line, or `None` for blank / `[DONE]` /
/// `event:` / non-object lines. For a caller reading the upstream body.
pub fn json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = rest.trim_ascii();
    }
    if trimmed.first() != Some(&b'{') {
        return None;
    }
    Some(trimmed)
}

// ---------------------------------------------------------------------------
// translator/gemini/gemini/gemini_gemini_response.go
// ---------------------------------------------------------------------------

/// port of PassthroughGeminiResponseStream (gemini_gemini_response.go); the
/// `data:` / `[DONE]` stripping is done by the caller's SSE reader.
fn passthrough_gemini_response_stream(raw: &Value) -> Value {
    raw.clone()
}

/// port of PassthroughGeminiResponseNonStream (gemini_gemini_response.go)
fn passthrough_gemini_response_non_stream(raw: &Value) -> Value {
    raw.clone()
}

// ---------------------------------------------------------------------------
// signature/provider_compatibility.go (Gemini target)
// ---------------------------------------------------------------------------

/// port of SignatureProvider (provider_compatibility.go), the members a Gemini
/// target can tell apart (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignatureProvider {
    Unknown,
    Claude,
    Gemini,
    GeminiBypass,
    Gpt,
    Swe,
}

/// port of SignatureBlockKind (provider_compatibility.go)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignatureBlockKind {
    Unknown,
    GeminiModelPart,
    GeminiFunctionCall,
}

/// port of SignatureCompatibilityAction (provider_compatibility.go)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignatureAction {
    Preserve,
    DropSignature,
    ReplaceWithGeminiBypass,
    DropBlock,
}

/// port of SignatureCompatibilityDecision (provider_compatibility.go), the
/// fields the sanitizer reads.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SignatureDecision {
    detected: SignatureProvider,
    compatible: bool,
    action: SignatureAction,
    replacement_signature: String,
    normalized_signature: String,
}

/// port of GeminiSkipThoughtSignatureValidator (gemini_validation.go)
pub const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
/// port of GeminiContextEngineeringBypass (gemini_validation.go)
const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";
/// port of MaxGeminiThoughtSignatureLen (gemini_validation.go)
const MAX_GEMINI_THOUGHT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
/// port of selfDescribingSignatureFirstChars (provider_compatibility.go)
const SELF_DESCRIBING_SIGNATURE_FIRST_CHARS: &[u8] = b"CERg";

/// port of SignatureProviderFromCachePrefix (provider_compatibility.go)
fn signature_provider_from_cache_prefix(prefix: &str) -> SignatureProvider {
    match prefix.trim().to_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
        | "claude-code-max" | "claude_code_max" => SignatureProvider::Claude,
        "gemini" | "google" => SignatureProvider::Gemini,
        "openai" | "gpt" | "codex" => SignatureProvider::Gpt,
        "swe" | "sealed" => SignatureProvider::Swe,
        _ => SignatureProvider::Unknown,
    }
}

/// port of SplitSignatureProviderPrefix (provider_compatibility.go)
fn split_signature_provider_prefix(raw: &str) -> Option<(SignatureProvider, String)> {
    let (prefix, rest) = raw.trim().split_once('#')?;
    let provider = signature_provider_from_cache_prefix(prefix);
    if provider == SignatureProvider::Unknown {
        return None;
    }
    Some((provider, rest.trim().to_string()))
}

/// port of SignaturePayloadWithoutProviderPrefix (provider_compatibility.go)
fn signature_payload_without_provider_prefix(raw: &str) -> String {
    match split_signature_provider_prefix(raw) {
        Some((_, unprefixed)) => unprefixed,
        None => raw.trim().to_string(),
    }
}

/// port of maybeSelfDescribingSignatureEnvelope (provider_compatibility.go)
fn maybe_self_describing_signature_envelope(raw: &str) -> bool {
    raw.as_bytes()
        .first()
        .is_some_and(|b| SELF_DESCRIBING_SIGNATURE_FIRST_CHARS.contains(b))
}

/// port of DetectSignatureProviderForBlock (provider_compatibility.go), with
/// the GPT / Claude / Kimi validators omitted (see the module docs): a
/// `claude#` / `openai#` prefix answers `Unknown` where Go answers that
/// provider when its validator accepts the payload — neither matches Gemini.
fn detect_signature_provider_for_block(raw: &str, kind: SignatureBlockKind) -> SignatureProvider {
    let sig = raw.trim();
    if sig.is_empty() {
        return SignatureProvider::Unknown;
    }
    if let Some((prefixed, unprefixed)) = split_signature_provider_prefix(sig) {
        match prefixed {
            SignatureProvider::Gemini => {
                if is_gemini_thought_signature_bypass(&unprefixed) {
                    return SignatureProvider::GeminiBypass;
                }
                if is_recognized_gemini_provider_signature(&unprefixed, kind) {
                    return SignatureProvider::Gemini;
                }
            }
            SignatureProvider::Swe if unprefixed.starts_with("sealed.v1.") => {
                return SignatureProvider::Swe;
            }
            _ => {}
        }
        return SignatureProvider::Unknown;
    }
    if sig.contains('#') {
        return SignatureProvider::Unknown;
    }
    if is_gemini_thought_signature_bypass(sig) {
        return SignatureProvider::GeminiBypass;
    }
    if sig.starts_with("sealed.v1.") {
        return SignatureProvider::Swe;
    }
    if maybe_self_describing_signature_envelope(sig)
        && is_recognized_gemini_provider_signature(sig, kind)
    {
        return SignatureProvider::Gemini;
    }
    SignatureProvider::Unknown
}

/// port of DecideSignatureCompatibility / DecideSignatureCompatibilityForModel
/// (provider_compatibility.go) for `targetProvider == gemini`.
fn decide_gemini_signature_compatibility(raw: &str, kind: SignatureBlockKind) -> SignatureDecision {
    let detected = detect_signature_provider_for_block(raw, kind);
    if matches!(
        detected,
        SignatureProvider::Gemini | SignatureProvider::GeminiBypass
    ) {
        return SignatureDecision {
            detected,
            compatible: true,
            action: SignatureAction::Preserve,
            replacement_signature: String::new(),
            normalized_signature: normalize_compatible_gemini_signature(raw, kind),
        };
    }
    // Go's Gemini arm: GeminiFunctionCall / GeminiModelPart / Unknown are
    // bypass-safe (every kind a Gemini request can carry); any other kind would
    // be DropBlock.
    let _ = kind;
    SignatureDecision {
        detected,
        compatible: false,
        action: SignatureAction::ReplaceWithGeminiBypass,
        replacement_signature: GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string(),
        normalized_signature: String::new(),
    }
}

/// port of normalizeCompatibleSignatureForProvider (provider_compatibility.go),
/// Gemini arm.
fn normalize_compatible_gemini_signature(raw: &str, kind: SignatureBlockKind) -> String {
    let payload = signature_payload_without_provider_prefix(raw);
    if is_gemini_thought_signature_bypass(&payload)
        || is_recognized_gemini_provider_signature(&payload, kind)
    {
        return payload;
    }
    String::new()
}

/// port of isRecognizedGeminiProviderSignature (provider_compatibility.go).
/// The `IsValidClaudeCAISSignature` guard is subsumed: CAIS starts with 'C'
/// (decoded 0x08..0x0b, protobuf field 1), which can never be the field-2
/// envelope required below.
fn is_recognized_gemini_provider_signature(raw: &str, _kind: SignatureBlockKind) -> bool {
    inspect_gemini_thought_signature(raw, false, true).is_ok()
}

/// port of CompatibleSignatureForProviderBlock (provider_compatibility.go), Gemini target.
fn compatible_gemini_signature_for_block(raw: &str, kind: SignatureBlockKind) -> Option<String> {
    let decision = decide_gemini_signature_compatibility(raw, kind);
    if !decision.compatible || decision.normalized_signature.is_empty() {
        return None;
    }
    Some(decision.normalized_signature)
}

/// port of GeminiReplaySignatureOrBypass (gemini_sanitize.go)
fn gemini_replay_signature_or_bypass(raw: &str, kind: SignatureBlockKind) -> String {
    if let Some(signature) = compatible_gemini_signature_for_block(raw, kind) {
        return signature;
    }
    let decision = decide_gemini_signature_compatibility(raw, kind);
    if decision.action == SignatureAction::ReplaceWithGeminiBypass
        && !decision.replacement_signature.is_empty()
    {
        return decision.replacement_signature;
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
}

// ---------------------------------------------------------------------------
// signature/gemini_validation.go
// ---------------------------------------------------------------------------

/// port of IsGeminiThoughtSignatureBypass (gemini_validation.go)
fn is_gemini_thought_signature_bypass(raw: &str) -> bool {
    matches!(
        raw.trim(),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR | GEMINI_CONTEXT_ENGINEERING_BYPASS
    )
}

/// port of GeminiThoughtSignatureEnvelope (gemini_validation.go)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeminiEnvelope {
    Unknown,
    ProtobufField2,
    AsciiUuid,
}

/// port of InspectGeminiThoughtSignature (gemini_validation.go), reduced to its
/// verdict; options `AllowBypassSentinel` / `RequireKnownEnvelope` (Go's
/// `RequireObservedMarker` is unused by the sanitizer).
fn inspect_gemini_thought_signature(
    raw: &str,
    allow_bypass_sentinel: bool,
    require_known_envelope: bool,
) -> Result<GeminiEnvelope, String> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err("empty Gemini thought signature".into());
    }
    if is_gemini_thought_signature_bypass(sig) {
        if !allow_bypass_sentinel {
            return Err("Gemini thought signature bypass sentinel is not allowed".into());
        }
        return Ok(GeminiEnvelope::Unknown);
    }
    let decoded = decode_gemini_thought_signature(sig)?;
    if decoded.is_empty() {
        return Err("invalid Gemini thought signature: empty decoded payload".into());
    }
    let (envelope, known) = classify_gemini_thought_signature_envelope(&decoded);
    if require_known_envelope && !known {
        return Err(format!(
            "invalid Gemini thought signature: unknown envelope {envelope:?}"
        ));
    }
    Ok(envelope)
}

/// port of decodeGeminiThoughtSignature (gemini_validation.go): Go's
/// `base64.StdEncoding` (padded) then `RawStdEncoding`; both are non-strict
/// about trailing bits.
fn decode_gemini_thought_signature(sig: &str) -> Result<Vec<u8>, String> {
    if sig.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN {
        return Err(format!(
            "Gemini thought signature exceeds maximum length ({MAX_GEMINI_THOUGHT_SIGNATURE_LEN} bytes)"
        ));
    }
    let std = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let raw = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    match std.decode(sig) {
        Ok(decoded) => Ok(decoded),
        Err(err) => raw
            .decode(sig)
            .map_err(|_| format!("invalid Gemini thought signature: base64 decode failed: {err}")),
    }
}

/// port of classifyGeminiThoughtSignatureEnvelope (gemini_validation.go)
fn classify_gemini_thought_signature_envelope(decoded: &[u8]) -> (GeminiEnvelope, bool) {
    if decoded.is_empty() {
        return (GeminiEnvelope::Unknown, false);
    }
    if is_ascii_uuid_bytes(decoded) {
        return (GeminiEnvelope::AsciiUuid, false);
    }
    if is_gemini_field2_envelope(decoded) {
        return (GeminiEnvelope::ProtobufField2, true);
    }
    (GeminiEnvelope::Unknown, false)
}

/// port of isGeminiField2Envelope + inspectGeminiField2Envelope
/// (gemini_validation.go): exactly one record with a non-empty payload.
fn is_gemini_field2_envelope(decoded: &[u8]) -> bool {
    let Some(value) = consume_gemini_field2_field1_value(decoded) else {
        return false;
    };
    let shape_ok = is_likely_gemini_opaque_payload(value)
        || is_ascii_uuid_bytes(value)
        || is_likely_gemini_tool_invocation_payload(value);
    shape_ok && !value.is_empty()
}

/// port of consumeGeminiField2Field1Value (gemini_validation.go)
fn consume_gemini_field2_field1_value(decoded: &[u8]) -> Option<&[u8]> {
    let (num, typ, n) = protowire_consume_tag(decoded)?;
    if num != 2 || typ != WIRE_BYTES {
        return None;
    }
    let (container, m) = protowire_consume_bytes(&decoded[n..])?;
    if n + m != decoded.len() {
        return None;
    }
    let (num, typ, n) = protowire_consume_tag(container)?;
    if num != 1 || typ != WIRE_BYTES {
        return None;
    }
    let (value, m) = protowire_consume_bytes(&container[n..])?;
    if n + m != container.len() {
        return None;
    }
    Some(value)
}

/// port of isLikelyGeminiOpaquePayload (gemini_validation.go): a Tink output
/// whose prefix-type byte is 0x01.
fn is_likely_gemini_opaque_payload(value: &[u8]) -> bool {
    value.first() == Some(&0x01)
}

/// port of isLikelyGeminiToolInvocationPayload (gemini_validation.go)
fn is_likely_gemini_tool_invocation_payload(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut offset = 0;
    let mut has_tink_field = false;
    while offset < value.len() {
        let Some((_, typ, n)) = protowire_consume_tag(&value[offset..]) else {
            return false;
        };
        offset += n;
        match typ {
            WIRE_VARINT => {
                let Some((_, n)) = protowire_consume_varint(&value[offset..]) else {
                    return false;
                };
                offset += n;
            }
            WIRE_BYTES => {
                let Some((bytes_val, n)) = protowire_consume_bytes(&value[offset..]) else {
                    return false;
                };
                offset += n;
                if is_likely_gemini_opaque_payload(bytes_val) {
                    has_tink_field = true;
                }
            }
            WIRE_FIXED32 => {
                if value.len() - offset < 4 {
                    return false;
                }
                offset += 4;
            }
            WIRE_FIXED64 => {
                if value.len() - offset < 8 {
                    return false;
                }
                offset += 8;
            }
            _ => return false,
        }
    }
    has_tink_field && offset == value.len()
}

/// port of isASCIIUUIDBytes (gemini_validation.go)
fn is_ascii_uuid_bytes(decoded: &[u8]) -> bool {
    if decoded.len() != 36 {
        return false;
    }
    decoded.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

// --- google.golang.org/protobuf/encoding/protowire (the subset used) --------

const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_BYTES: u64 = 2;
const WIRE_FIXED32: u64 = 5;

/// protowire.ConsumeVarint: at most 10 bytes, the 10th may only carry 1 bit.
fn protowire_consume_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for (i, byte) in b.iter().take(10).enumerate() {
        if i == 9 && *byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// protowire.ConsumeTag: (field number, wire type, length); a field number
/// below 1 or above `MaxValidNumber` is an error.
fn protowire_consume_tag(b: &[u8]) -> Option<(u64, u64, usize)> {
    let (v, n) = protowire_consume_varint(b)?;
    let num = v >> 3;
    if !(1..=(1 << 29) - 1).contains(&num) {
        return None;
    }
    Some((num, v & 7, n))
}

/// protowire.ConsumeBytes: a varint length then that many bytes.
fn protowire_consume_bytes(b: &[u8]) -> Option<(&[u8], usize)> {
    let (len, n) = protowire_consume_varint(b)?;
    let len = usize::try_from(len).ok()?;
    if len > b.len() - n {
        return None;
    }
    Some((&b[n..n + len], n + len))
}

// ---------------------------------------------------------------------------
// signature/gemini_sanitize.go
// ---------------------------------------------------------------------------

/// port of geminiPartThoughtSignaturePaths (gemini_sanitize.go)
const GEMINI_PART_THOUGHT_SIGNATURE_PATHS: [&str; 7] = [
    "thoughtSignature",
    "thought_signature",
    "functionCall.thoughtSignature",
    "functionCall.thought_signature",
    "functionResponse.thoughtSignature",
    "functionResponse.thought_signature",
    "extra_content.google.thought_signature",
];

/// port of geminiPartThoughtSignature (gemini_sanitize.go)
fn gemini_part_thought_signature(part: &Value) -> Option<String> {
    GEMINI_PART_THOUGHT_SIGNATURE_PATHS
        .iter()
        .find_map(|path| gj_get(part, path))
        .map(|v| gj_string(Some(v)))
}

/// port of hasNormalizedGeminiPartThoughtSignature (gemini_sanitize.go)
fn has_normalized_gemini_part_thought_signature(part: &Value, replay: &str) -> bool {
    match gj_get(part, "thoughtSignature") {
        Some(Value::String(canonical)) if canonical == replay => {}
        _ => return false,
    }
    GEMINI_PART_THOUGHT_SIGNATURE_PATHS[1..]
        .iter()
        .all(|path| gj_get(part, path).is_none())
}

/// port of deleteGeminiPartThoughtSignatureFields (gemini_sanitize.go)
fn delete_gemini_part_thought_signature_fields(part: &mut Value) {
    for path in GEMINI_PART_THOUGHT_SIGNATURE_PATHS {
        sj_delete(part, path);
    }
}

/// `toolCall` / `tool_call` / `toolResponse` / `tool_response` parts are
/// echoed back untouched (gemini_sanitize.go).
fn is_server_tool_part(part: &Value) -> bool {
    ["toolCall", "tool_call", "toolResponse", "tool_response"]
        .iter()
        .any(|key| gj_get(part, key).is_some())
}

/// port of geminiContentsThoughtSignaturesNeedSanitize (gemini_sanitize.go)
fn gemini_contents_thought_signatures_need_sanitize(contents: &[Value]) -> bool {
    for content in contents {
        let Some(Value::Array(parts)) = gj_get(content, "parts") else {
            continue;
        };
        let is_model_turn = gj_string(gj_get(content, "role")) == "model";
        let mut first_function_call_seen = false;
        for part in parts {
            let signature = gemini_part_thought_signature(part);
            if gj_get(part, "functionResponse").is_some() {
                if signature.is_some() {
                    return true;
                }
                continue;
            }
            if !is_model_turn || is_server_tool_part(part) {
                continue;
            }
            let has_function_call = gj_get(part, "functionCall").is_some();
            let is_first_function_call = has_function_call && !first_function_call_seen;
            if has_function_call {
                first_function_call_seen = true;
            }
            let raw = signature.clone().unwrap_or_default();
            if is_first_function_call {
                let replay =
                    gemini_replay_signature_or_bypass(&raw, SignatureBlockKind::GeminiFunctionCall);
                if !has_normalized_gemini_part_thought_signature(part, &replay) {
                    return true;
                }
                continue;
            }
            if signature.is_none() {
                continue;
            }
            let kind = if has_function_call {
                SignatureBlockKind::GeminiFunctionCall
            } else {
                SignatureBlockKind::GeminiModelPart
            };
            let decision = decide_gemini_signature_compatibility(&raw, kind);
            if decision.action != SignatureAction::Preserve
                || is_gemini_thought_signature_bypass(&signature_payload_without_provider_prefix(
                    &raw,
                ))
            {
                return true;
            }
            if !has_normalized_gemini_part_thought_signature(part, &decision.normalized_signature) {
                return true;
            }
        }
    }
    false
}

/// port of SanitizeGeminiRequestThoughtSignatures (gemini_sanitize.go)
fn sanitize_gemini_request_thought_signatures(payload: &mut Value, contents_path: &str) {
    let contents_path = match contents_path.trim() {
        "" => "contents",
        path => path,
    };
    let Some(Value::Array(contents)) = gj_get_mut(payload, contents_path) else {
        return;
    };
    if !gemini_contents_thought_signatures_need_sanitize(contents) {
        return;
    }
    for content in contents.iter_mut() {
        let is_model_turn = gj_string(gj_get(content, "role")) == "model";
        let Some(Value::Array(parts)) = gj_get_mut(content, "parts") else {
            continue;
        };
        let mut first_function_call_seen = false;
        for part in parts.iter_mut() {
            let signature = gemini_part_thought_signature(part);
            let has_signature = signature.is_some();
            let raw = signature.unwrap_or_default();
            if gj_get(part, "functionResponse").is_some() {
                if has_signature {
                    delete_gemini_part_thought_signature_fields(part);
                }
                continue;
            }
            if !is_model_turn || is_server_tool_part(part) {
                continue;
            }
            let has_function_call = gj_get(part, "functionCall").is_some();
            let is_first_function_call = has_function_call && !first_function_call_seen;
            if has_function_call {
                first_function_call_seen = true;
            }
            if !has_function_call && !has_signature {
                continue;
            }
            let kind = if has_function_call {
                SignatureBlockKind::GeminiFunctionCall
            } else {
                SignatureBlockKind::GeminiModelPart
            };
            let decision = decide_gemini_signature_compatibility(&raw, kind);
            let replay = if is_first_function_call {
                gemini_replay_signature_or_bypass(&raw, kind)
            } else if has_signature
                && decision.action == SignatureAction::Preserve
                && !is_gemini_thought_signature_bypass(&signature_payload_without_provider_prefix(
                    &raw,
                ))
            {
                decision.normalized_signature.clone()
            } else {
                // hasSignature: Go marks the decision DropSignature and removes
                // it below; an unsigned non-first call stays unsigned.
                String::new()
            };
            if !replay.is_empty() {
                if !has_normalized_gemini_part_thought_signature(part, &replay) {
                    delete_gemini_part_thought_signature_fields(part);
                    sj_set(part, "thoughtSignature", Value::from(replay));
                }
            } else if has_signature {
                delete_gemini_part_thought_signature_fields(part);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).expect("test JSON")
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// port of protowire.AppendTag + AppendBytes for small fields.
    fn proto_bytes_field(num: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(num << 3) | 2];
        let mut len = payload.len();
        loop {
            let byte = (len & 0x7f) as u8;
            len >>= 7;
            if len == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out.extend_from_slice(payload);
        out
    }

    /// port of testGemini3ThoughtSignature (signature/gemini_validation_test.go)
    fn gemini3_signature(payload: &[u8]) -> String {
        b64(&proto_bytes_field(2, &proto_bytes_field(1, payload)))
    }

    /// port of testNativeGemini3ThoughtSignature (gemini_executor_signature_test.go)
    fn native_gemini3_signature() -> String {
        gemini3_signature(&[0x01, 0x0c, 0x39, 0xd6, 0xc7, 0x34])
    }

    const CLAUDE_CAIS_SAMPLE: &str = "CAISqwIKiAEIEBgCKkBHRlRBsNiptQUWfPoOhuQKwi5LnncZVO9bB5jqOs76D7uBtgktML0zqJtNmLHXHHcgD6lk4MQu4QBXzFd1lbC3Mg5jbGF1ZGUtZmFibGUtNTgBQgh0aGlua2luZ1okZDk3NDM5NzUtNGJiMC00OTM2LTllMjgtZDViMGQyMWJkYzQ4EgxCGh+XVFFFeySAjtAaDL/A1LltGu6MMJ+eXSIwsN0oBpDrqLv22UBfkMnTotnIbkvkOyb9xZHgigG6OZVHaI3gThm+maLKmgO5PrFLKlDFYp+YZksy/wKwszJlnLTPzAK+NUlfzagOE1ymtZTXhAYK260XyFYmg/te/C231+Fr/hoX+EJoUBnrn0gD7hqMISOT+TaFEuOXYsN517GfaxgB";

    const LIVE_TOOL_SIG: &str = "ErUDCrIDCAISrQMBEU0yD9ECvDhSY1DQJNUGafArdfd2mDfO8VQq7XjLx/91zESuo0QPSdkRFWkLeVIocSQmQULonYMOJcs6XDLV2LTRC9myb3MCCP9CUoWbEeqhAvXKTScyS3nwBDDVJYuDDbY3YvR4V86T/DnU3qufpaVZ3wQOiJVyBVZ515dYTN+XGq7SuUc3RpfAqVU06jgxaCM0WKV4Df5mGMJWb25e/aFG2Jc7upSqpf3n6aElj+4c/eWr4GdKd0TUIElXBZ0HEN/vNcWzD3F0S4MeVbk1LDakL6HG6oyaSS2gocxYNYxqm9mdMHaXYa4mIYqWqmqBEnbgcHp8H4fgqBxc3Cx8C3otV8IarO5OALaVDA3NaXB1zjLet1587kEpkCNr9OvrYOES2nCl/i4EgbPK01nlXo+Wwm5jsZU5nEG4/Z0bErzqC5TKwOsqpJ7afL2sPWI0IGrXhXL+QCumWCS5iUtwybSkL7CYSk9GC+iY+ev6FAmC4V5JEc4OaWOc9+m/29LniN/iPTSxtUQSZT94pUa3/irIIdH7ReAS3cpeM6OTvumR1PwNxXx3XM1mEGc=";

    fn sanitize(input: &str) -> Value {
        let mut v = j(input);
        sanitize_gemini_request_thought_signatures(&mut v, "contents");
        v
    }

    fn body_of(req: &(String, Vec<(String, String)>, Vec<u8>)) -> Value {
        serde_json::from_slice(&req.2).expect("request body JSON")
    }

    // --- build_request: URL / headers ------------------------------------

    #[test]
    fn build_request_url_and_headers() {
        let body = j(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#);
        let (url, headers, _) = build_request("", "gemini-3-pro", false, &body, "k1", &[]);
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:generateContent"
        );
        assert_eq!(
            headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Goog-Api-Key".to_string(), "k1".to_string()),
            ]
        );

        let (url, headers, _) = build_request(
            " https://gw.example.com/// ",
            "gemini-3-pro",
            true,
            &body,
            "",
            &[("x-goog-api-key".into(), "client".into())],
        );
        assert_eq!(
            url,
            "https://gw.example.com/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse"
        );
        // port of TestGeminiExecutor_PrepareRequest_EmptyAPIKey_OmitsAuthHeaders:
        // an empty key sends no key header, and client headers are not forwarded.
        assert_eq!(
            headers,
            vec![("Content-Type".to_string(), "application/json".to_string())]
        );

        // TrimRight("/") of "/" is empty → default endpoint.
        let (url, _, _) = build_request("/", "m", false, &body, "k", &[]);
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/m:generateContent"
        );
    }

    #[test]
    fn build_request_body_normalisation() {
        // model set (appended), session_id removed, user content untouched.
        let body = j(
            r#"{"contents":[{"role":"user","parts":[{"text":"Hello world"}]}],"session_id":"s1","generationConfig":{"maxOutputTokens":500000}}"#,
        );
        let req = build_request("", "gemini-2.5-flash", false, &body, "k", &[]);
        assert_eq!(
            body_of(&req),
            j(
                r#"{"contents":[{"role":"user","parts":[{"text":"Hello world"}]}],"generationConfig":{"maxOutputTokens":500000},"model":"gemini-2.5-flash"}"#
            )
        );
        // An existing model is replaced in place.
        let body = j(r#"{"model":"x","contents":[]}"#);
        let req = build_request("", "gemini-2.5-flash", true, &body, "k", &[]);
        assert_eq!(
            body_of(&req),
            j(r#"{"model":"gemini-2.5-flash","contents":[]}"#)
        );
    }

    // --- gemini_executor_test.go ----------------------------------------

    // port of TestCapGeminiMaxOutputTokensUsesOutputTokenLimit (the registry's
    // gemini-3.1-pro-preview limit, 65536, passed in explicitly).
    #[test]
    fn cap_gemini_max_output_tokens_uses_output_token_limit() {
        let mut body =
            j(r#"{"generationConfig":{"maxOutputTokens":500000,"temperature":0.2},"contents":[]}"#);
        cap_gemini_max_output_tokens_with(&mut body, Some((65536, 0)));
        assert_eq!(
            body,
            j(r#"{"generationConfig":{"maxOutputTokens":65536,"temperature":0.2},"contents":[]}"#)
        );
        // MaxCompletionTokens is the fallback when OutputTokenLimit is unset.
        let mut body = j(r#"{"generationConfig":{"maxOutputTokens":500000}}"#);
        cap_gemini_max_output_tokens_with(&mut body, Some((0, 8192)));
        assert_eq!(body, j(r#"{"generationConfig":{"maxOutputTokens":8192}}"#));
    }

    // port of TestCapGeminiMaxOutputTokensLeavesAllowedOrUnknown
    #[test]
    fn cap_gemini_max_output_tokens_leaves_allowed_or_unknown() {
        let mut body = j(r#"{"generationConfig":{"maxOutputTokens":64000}}"#);
        cap_gemini_max_output_tokens_with(&mut body, Some((65536, 0)));
        assert_eq!(body, j(r#"{"generationConfig":{"maxOutputTokens":64000}}"#));
        let mut body = j(r#"{"generationConfig":{"maxOutputTokens":500000}}"#);
        cap_gemini_max_output_tokens(&mut body, "custom-gemini-model");
        assert_eq!(
            body,
            j(r#"{"generationConfig":{"maxOutputTokens":500000}}"#)
        );
        // A non-number is left alone.
        let mut body = j(r#"{"generationConfig":{"maxOutputTokens":"500000"}}"#);
        cap_gemini_max_output_tokens_with(&mut body, Some((65536, 0)));
        assert_eq!(
            body,
            j(r#"{"generationConfig":{"maxOutputTokens":"500000"}}"#)
        );
    }

    // port of TestGeminiExecutorExecutePrependsLeadingUser
    #[test]
    fn execute_prepends_leading_user() {
        let body = j(
            r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"key":"value"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"result":"ok"}}}]}]}"#,
        );
        let req = build_request("", "gemini-3.7-flash", false, &body, "k", &[]);
        assert_eq!(
            body_of(&req),
            j(&format!(
                r#"{{"contents":[{{"role":"user","parts":[{{"text":""}}]}},{{"role":"model","parts":[{{"functionCall":{{"name":"lookup","args":{{"key":"value"}}}},"thoughtSignature":"{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"}}]}},{{"role":"user","parts":[{{"functionResponse":{{"name":"lookup","response":{{"result":"ok"}}}}}}]}}],"model":"gemini-3.7-flash"}}"#
            ))
        );
    }

    // port of TestGeminiExecutorExecuteAppendsTrailingUserForTrailingModelTurn
    #[test]
    fn execute_appends_trailing_user_for_trailing_model_turn() {
        let body = j(
            r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]}]}"#,
        );
        for stream in [false, true] {
            let req = build_request("", "gemini-3.7-flash", stream, &body, "k", &[]);
            assert_eq!(
                body_of(&req),
                j(
                    r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":""}]}],"model":"gemini-3.7-flash"}"#
                )
            );
        }
    }

    // --- gemini_executor_signature_test.go ------------------------------

    // port of TestGeminiExecutorExecute_FunctionCall_ReplacesClaudeSignatureWithBypass
    #[test]
    fn execute_function_call_replaces_claude_signature_with_bypass() {
        let body = j(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"search","args":{{"q":"go"}}}},"thoughtSignature":"{CLAUDE_CAIS_SAMPLE}"}}]}},{{"role":"user","parts":[{{"functionResponse":{{"name":"search","response":{{"result":"found"}}}}}}]}}]}}"#
        ));
        let req = build_request("", "gemini-2.5-flash", false, &body, "k", &[]);
        assert_eq!(
            body_of(&req),
            j(&format!(
                r#"{{"contents":[{{"role":"user","parts":[{{"text":""}}]}},{{"role":"model","parts":[{{"functionCall":{{"name":"search","args":{{"q":"go"}}}},"thoughtSignature":"{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"}}]}},{{"role":"user","parts":[{{"functionResponse":{{"name":"search","response":{{"result":"found"}}}}}}]}}],"model":"gemini-2.5-flash"}}"#
            ))
        );
    }

    // port of TestGeminiExecutorExecute_PreservesNativeGeminiSignature
    #[test]
    fn execute_preserves_native_gemini_signature() {
        let sig = native_gemini3_signature();
        let body = j(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"search","args":{{"q":"go"}}}},"thoughtSignature":"{sig}"}}]}},{{"role":"user","parts":[{{"functionResponse":{{"name":"search","response":{{"result":"found"}}}}}}]}}]}}"#
        ));
        let req = build_request("", "gemini-2.5-flash", false, &body, "k", &[]);
        let out = body_of(&req);
        assert_eq!(
            out["contents"][1],
            j(&format!(
                r#"{{"role":"model","parts":[{{"functionCall":{{"name":"search","args":{{"q":"go"}}}},"thoughtSignature":"{sig}"}}]}}"#
            ))
        );
    }

    // port of TestGeminiExecutorExecute_SanitizesClaudeCAISSignature, on the
    // Gemini body the Claude→Gemini translator produces for a signed thinking
    // block (a `thought` part carrying the Claude signature).
    #[test]
    fn execute_sanitizes_claude_cais_signature_on_thought_part() {
        let body = j(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"text":"Let me think...","thought":true,"thoughtSignature":"{CLAUDE_CAIS_SAMPLE}"}},{{"text":"Here is the response."}}]}},{{"role":"user","parts":[{{"text":"Follow up question."}}]}}]}}"#
        ));
        for stream in [false, true] {
            let req = build_request("", "gemini-2.5-flash", stream, &body, "k", &[]);
            assert_eq!(
                body_of(&req),
                j(
                    r#"{"contents":[{"role":"user","parts":[{"text":""}]},{"role":"model","parts":[{"text":"Let me think...","thought":true},{"text":"Here is the response."}]},{"role":"user","parts":[{"text":"Follow up question."}]}],"model":"gemini-2.5-flash"}"#
                )
            );
        }
    }

    // --- helps/gemini_content_turns_test.go -------------------------------

    fn roles(v: &Value, path: &str) -> String {
        match gj_get(v, path) {
            Some(Value::Array(items)) => items
                .iter()
                .map(|c| gj_string(gj_get(c, "role")))
                .collect::<Vec<_>>()
                .join(","),
            _ => String::new(),
        }
    }

    // port of TestEnsureGeminiLeadingUserContent
    #[test]
    fn ensure_gemini_leading_user_content_cases() {
        let cases: &[(&str, &str, &str)] = &[
            (
                r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
                "contents",
                r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
            ),
            (
                r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"run"}}]},{"role":"user","parts":[{"functionResponse":{"name":"run"}}]}]}"#,
                "contents",
                r#"{"contents":[{"role":"user","parts":[{"text":""}]},{"role":"model","parts":[{"functionCall":{"name":"run"}}]},{"role":"user","parts":[{"functionResponse":{"name":"run"}}]}]}"#,
            ),
            (
                r#"{"contents":[{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"continue"}]}]}"#,
                "contents",
                r#"{"contents":[{"role":"user","parts":[{"text":""}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"continue"}]}]}"#,
            ),
            (
                r#"{"request":{"contents":[{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"continue"}]}]}}"#,
                "request.contents",
                r#"{"request":{"contents":[{"role":"user","parts":[{"text":""}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"continue"}]}]}}"#,
            ),
            (r#"{"contents":[]}"#, "contents", r#"{"contents":[]}"#),
            (r#"{"model":"test"}"#, "contents", r#"{"model":"test"}"#),
        ];
        for (input, path, want) in cases {
            let mut v = j(input);
            ensure_gemini_leading_user_content(&mut v, path);
            assert_eq!(v, j(want), "{input}");
        }
    }

    // port of TestEnsureGeminiTrailingUserContent
    #[test]
    fn ensure_gemini_trailing_user_content_cases() {
        let cases: &[(&str, &str, &str)] = &[
            (
                r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
                "contents",
                "user",
            ),
            (
                r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"run"}}]},{"role":"model","parts":[{"functionResponse":{"name":"run","response":{"result":"ok"}}}]}]}"#,
                "contents",
                "model,model",
            ),
            (
                r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"functionCall":{"name":"run"}}]}]}"#,
                "contents",
                "user,model,user",
            ),
            (
                r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]}]}"#,
                "contents",
                "user,model,user",
            ),
            (
                r#"{"request":{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]}]}}"#,
                "request.contents",
                "user,model,user",
            ),
            (r#"{"contents":[]}"#, "contents", ""),
            (r#"{"model":"test"}"#, "contents", ""),
        ];
        for (input, path, want_roles) in cases {
            let mut v = j(input);
            ensure_gemini_trailing_user_content(&mut v, path);
            assert_eq!(roles(&v, path), *want_roles, "{input}");
            if want_roles.ends_with("model,user") {
                let items = gj_get(&v, path).and_then(Value::as_array).unwrap();
                assert_eq!(items.last().unwrap(), &empty_gemini_user_turn());
            }
        }
    }

    // port of TestEnsureGeminiBoundaryUserContent
    #[test]
    fn ensure_gemini_boundary_user_content_single_model_turn() {
        let mut v = j(r#"{"contents":[{"role":"model","parts":[{"text":"single answer"}]}]}"#);
        ensure_gemini_boundary_user_content(&mut v, "contents");
        assert_eq!(
            v,
            j(
                r#"{"contents":[{"role":"user","parts":[{"text":""}]},{"role":"model","parts":[{"text":"single answer"}]},{"role":"user","parts":[{"text":""}]}]}"#
            )
        );
    }

    // --- signature/gemini_sanitize_test.go --------------------------------

    // port of TestSanitizeGeminiRequestThoughtSignaturesPreservesGeminiSignature
    #[test]
    fn sanitize_preserves_gemini_signature() {
        let sig = gemini3_signature(&[0x01, 0x0c, 0x39]);
        let input = format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig}"}}]}}]}}"#
        );
        assert_eq!(sanitize(&input), j(&input));
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesParallelSyntheticOnlyFirstGetsBypass
    #[test]
    fn sanitize_parallel_synthetic_only_first_gets_bypass() {
        let out = sanitize(
            r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"first","args":{}}},{"functionCall":{"name":"second","args":{}}}]}]}"#,
        );
        assert_eq!(
            out,
            j(
                r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"first","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"second","args":{}}}]}]}"#
            )
        );
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesNativeParallelPreservesUnsignedSibling
    #[test]
    fn sanitize_native_parallel_preserves_unsigned_sibling() {
        let sig = gemini3_signature(&[0x01, 0x0c, 0x39]);
        let input = format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{sig}"}},{{"functionCall":{{"name":"second","args":{{}}}}}}]}}]}}"#
        );
        assert_eq!(sanitize(&input), j(&input));
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesRemovesPollutedSiblingBypass
    // and ...RemovesPrefixedSiblingBypass
    #[test]
    fn sanitize_removes_polluted_and_prefixed_sibling_bypass() {
        let sig = gemini3_signature(&[0x01, 0x0c, 0x39]);
        for sibling in [
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string(),
            format!("gemini#{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"),
            format!("google#{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"),
        ] {
            let out = sanitize(&format!(
                r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{sig}"}},{{"functionCall":{{"name":"second","args":{{}}}},"thoughtSignature":"{sibling}"}}]}}]}}"#
            ));
            assert_eq!(
                out,
                j(&format!(
                    r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{sig}"}},{{"functionCall":{{"name":"second","args":{{}}}}}}]}}]}}"#
                ))
            );
        }
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesLeavesUnsignedThoughtUnsigned
    // and ...ReusesUnsignedFunctionResponsePayload
    #[test]
    fn sanitize_leaves_unsigned_parts_alone() {
        for input in [
            r#"{"contents":[{"role":"model","parts":[{"text":"hidden","thought":true}]}]}"#,
            r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"result":"ok"}}}]}]}"#,
        ] {
            assert_eq!(sanitize(input), j(input));
        }
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesReplacesBase64UUIDFunctionCall
    #[test]
    fn sanitize_replaces_base64_uuid_function_call() {
        let sig = b64(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");
        let out = sanitize(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}},"thoughtSignature":"{sig}"}}}}]}}]}}"#
        ));
        assert_eq!(
            out,
            j(
                r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"f","args":{}},"thoughtSignature":"skip_thought_signature_validator"}]}]}"#
            )
        );
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesSuppressesRepeatedLogs /
    // ...DistinguishesDetectedProviders (the drop half; logging is omitted)
    #[test]
    fn sanitize_drops_foreign_signatures_on_text_parts() {
        let out = sanitize(
            r#"{"contents":[{"role":"model","parts":[{"text":"a","thoughtSignature":"sealed.v1.demo_signature_1"}]},{"role":"model","parts":[{"text":"b","thoughtSignature":"invalid_sig_1"}]},{"role":"model","parts":[{"text":"answer","thoughtSignature":"claude_or_invalid_signature"}]}]}"#,
        );
        assert_eq!(
            out,
            j(
                r#"{"contents":[{"role":"model","parts":[{"text":"a"}]},{"role":"model","parts":[{"text":"b"}]},{"role":"model","parts":[{"text":"answer"}]}]}"#
            )
        );
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesPreservesField2WrappedUUIDFunctionCall
    #[test]
    fn sanitize_preserves_field2_wrapped_uuid_function_call() {
        let sig = gemini3_signature(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");
        let input = format!(
            r#"{{"request":{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig}"}}]}}]}}}}"#
        );
        let mut v = j(&input);
        sanitize_gemini_request_thought_signatures(&mut v, "request.contents");
        assert_eq!(v, j(&input));
    }

    // port of TestSanitizeGeminiRequestThoughtSignaturesRemovesFunctionResponseSignature
    #[test]
    fn sanitize_removes_function_response_signature() {
        let out = sanitize(
            r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"result":"ok"},"thoughtSignature":"worse"},"thoughtSignature":"bad"}]}]}"#,
        );
        assert_eq!(
            out,
            j(
                r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"result":"ok"}}}]}]}"#
            )
        );
    }

    // port of TestSanitizeGeminiRequestThoughtSignatures_PreservesToolCallAndResponseSignatures
    #[test]
    fn sanitize_preserves_tool_call_and_response_signatures() {
        let sig_model = gemini3_signature(&[0x01, 0x0c, 0x39]);
        let input = format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"hello"}}]}},{{"role":"model","parts":[{{"toolCall":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{LIVE_TOOL_SIG}"}},{{"toolResponse":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{LIVE_TOOL_SIG}"}},{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig_model}"}}]}}]}}"#
        );
        assert_eq!(sanitize(&input), j(&input));
        // The live tool signature is itself a known field-2 envelope wrapping a
        // tool-invocation payload.
        assert!(is_recognized_gemini_provider_signature(
            LIVE_TOOL_SIG,
            SignatureBlockKind::Unknown
        ));
    }

    // port of TestSanitizeGeminiRequestThoughtSignatures_SkipsToolCallAndToolResponseParts
    // and ..._SkipsSnakeCaseToolCallAndResponseParts
    #[test]
    fn sanitize_skips_server_tool_parts() {
        for input in [
            r#"{"contents":[{"role":"model","parts":[{"toolCall":{"toolType":"GOOGLE_SEARCH_WEB","args":{}},"thoughtSignature":"arbitrary_opaque_tool_sig_value"},{"toolResponse":{"toolType":"GOOGLE_SEARCH_WEB","response":{}},"thoughtSignature":"arbitrary_opaque_tool_sig_value"}]}]}"#,
            r#"{"contents":[{"role":"model","parts":[{"tool_call":{"tool_type":"GOOGLE_SEARCH_WEB","args":{}},"thought_signature":"arbitrary_snake_tool_sig"},{"tool_response":{"tool_type":"GOOGLE_SEARCH_WEB","response":{}},"thought_signature":"arbitrary_snake_tool_sig"}]}]}"#,
        ] {
            assert_eq!(sanitize(input), j(input));
        }
    }

    // port of TestSanitizeGeminiRequestThoughtSignatures_MixedToolCallAndUnsignedFunctionCall
    #[test]
    fn sanitize_mixed_tool_call_and_unsigned_function_call() {
        let out = sanitize(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"toolCall":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{LIVE_TOOL_SIG}"}},{{"functionCall":{{"name":"my_func","args":{{}}}}}}]}}]}}"#
        ));
        assert_eq!(
            out,
            j(&format!(
                r#"{{"contents":[{{"role":"model","parts":[{{"toolCall":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{LIVE_TOOL_SIG}"}},{{"functionCall":{{"name":"my_func","args":{{}}}},"thoughtSignature":"skip_thought_signature_validator"}}]}}]}}"#
            ))
        );
    }

    #[test]
    fn sanitize_normalises_prefixed_and_nested_native_signature() {
        // A `gemini#` cache prefix is stripped, a nested copy moves to the
        // canonical top-level field, and the context-engineering sentinel on a
        // first call is kept as a compatible bypass.
        let sig = native_gemini3_signature();
        let out = sanitize(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"text":"t","thoughtSignature":"gemini#{sig}"}},{{"functionCall":{{"name":"f","args":{{}},"thought_signature":"{sig}"}}}}]}},{{"role":"model","parts":[{{"functionCall":{{"name":"g","args":{{}}}},"thoughtSignature":"context_engineering_is_the_way_to_go"}}]}}]}}"#
        ));
        assert_eq!(
            out,
            j(&format!(
                r#"{{"contents":[{{"role":"model","parts":[{{"text":"t","thoughtSignature":"{sig}"}},{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig}"}}]}},{{"role":"model","parts":[{{"functionCall":{{"name":"g","args":{{}}}},"thoughtSignature":"context_engineering_is_the_way_to_go"}}]}}]}}"#
            ))
        );
    }

    // --- signature detection (provider_compatibility_test.go, Gemini half) --

    #[test]
    fn detect_signature_provider_gemini_cases() {
        let gemini_sig = gemini3_signature(&[0x01, 0x0c, 0x39]);
        let k = SignatureBlockKind::Unknown;
        assert_eq!(
            detect_signature_provider_for_block(&format!("gemini#{gemini_sig}"), k),
            SignatureProvider::Gemini
        );
        assert_eq!(
            detect_signature_provider_for_block(&gemini_sig, k),
            SignatureProvider::Gemini
        );
        assert_eq!(
            detect_signature_provider_for_block(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, k),
            SignatureProvider::GeminiBypass
        );
        assert_eq!(
            detect_signature_provider_for_block("sealed.v1.x", k),
            SignatureProvider::Swe
        );
        // The Claude CAIS sample is never claimed by the Gemini envelope check.
        assert!(!is_recognized_gemini_provider_signature(
            CLAUDE_CAIS_SAMPLE,
            k
        ));
        assert_eq!(
            compatible_gemini_signature_for_block(CLAUDE_CAIS_SAMPLE, k),
            None
        );
        // Unknown prefix with '#' and a Gemini 2.5 repeated field-1 form.
        assert_eq!(
            detect_signature_provider_for_block(&format!("model#{gemini_sig}"), k),
            SignatureProvider::Unknown
        );
        assert_eq!(
            detect_signature_provider_for_block(&b64(&proto_bytes_field(1, &[0x01, 0x02])), k),
            SignatureProvider::Unknown
        );
        // Unpadded base64 decodes through the RawStdEncoding fallback.
        let unpadded = gemini_sig.trim_end_matches('=').to_string();
        assert_eq!(
            detect_signature_provider_for_block(&unpadded, k),
            SignatureProvider::Gemini
        );
    }

    // --- stream usage filter --------------------------------------------

    #[test]
    fn rewrite_stream_event_renames_non_terminal_usage() {
        // Non-terminal: usageMetadata → cpaUsageMetadata (appended last).
        let event = j(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":3},"modelVersion":"m"}"#,
        );
        assert_eq!(
            rewrite_stream_event(&event),
            j(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"a"}]}}],"modelVersion":"m","cpaUsageMetadata":{"promptTokenCount":3}}"#
            )
        );
        // Terminal (finishReason set): untouched. A blank finishReason is not terminal.
        let terminal =
            j(r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3}}"#);
        assert_eq!(rewrite_stream_event(&terminal), terminal);
        let blank = j(r#"{"candidates":[{"finishReason":" "}],"usageMetadata":{"a":1}}"#);
        assert_eq!(
            rewrite_stream_event(&blank),
            j(r#"{"candidates":[{"finishReason":" "}],"cpaUsageMetadata":{"a":1}}"#)
        );
        // The response.-wrapped form is handled too; no usage → untouched.
        let wrapped = j(r#"{"response":{"usageMetadata":{"a":1},"x":1}}"#);
        assert_eq!(
            rewrite_stream_event(&wrapped),
            j(r#"{"response":{"x":1,"cpaUsageMetadata":{"a":1}}}"#)
        );
        let plain = j(r#"{"candidates":[]}"#);
        assert_eq!(rewrite_stream_event(&plain), plain);
        // A non-object payload produces no frame.
        assert_eq!(rewrite_stream_event(&j("[1]")), Value::Null);
    }

    #[test]
    fn rewrite_stream_event_split_terminal_usage_by_trace_id() {
        // A stop chunk without usage remembers its traceId; the following
        // usage chunk with that traceId keeps usageMetadata (once).
        let stop =
            j(r#"{"candidates":[{"finishReason":"STOP"}],"traceId":"trace-gemini-exec-split"}"#);
        assert_eq!(rewrite_stream_event(&stop), stop);
        let usage = j(
            r#"{"candidates":[{"content":{"parts":[{"text":""}]}}],"usageMetadata":{"totalTokenCount":33},"traceId":"trace-gemini-exec-split"}"#,
        );
        assert_eq!(rewrite_stream_event(&usage), usage);
        assert_eq!(
            rewrite_stream_event(&usage),
            j(
                r#"{"candidates":[{"content":{"parts":[{"text":""}]}}],"traceId":"trace-gemini-exec-split","cpaUsageMetadata":{"totalTokenCount":33}}"#
            )
        );
    }

    #[test]
    fn json_payload_lines() {
        assert_eq!(json_payload(b"data: {\"a\":1}\r"), Some(&b"{\"a\":1}"[..]));
        assert_eq!(json_payload(b"{\"a\":1}"), Some(&b"{\"a\":1}"[..]));
        assert_eq!(json_payload(b"data: [DONE]"), None);
        assert_eq!(json_payload(b"[DONE]"), None);
        assert_eq!(json_payload(b"event: message"), None);
        assert_eq!(json_payload(b"   "), None);
        assert_eq!(json_payload(b"data: [1]"), None);
    }

    #[test]
    fn finalize_non_stream_is_identity() {
        let body = j(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        );
        assert_eq!(finalize_non_stream(&body), body);
    }

    #[test]
    fn extract_custom_headers_rules() {
        let attrs = vec![
            ("header:X-Static".to_string(), " v ".to_string()),
            ("header:X-From-Client".to_string(), "$X-Trace".to_string()),
            ("header:X-Missing".to_string(), "$X-Absent".to_string()),
            (
                "header:X-Session".to_string(),
                "$CPA-SESSION-ID".to_string(),
            ),
            (
                "header:X-Mixed".to_string(),
                "s-$cpa-session-id".to_string(),
            ),
            ("other".to_string(), "x".to_string()),
            ("header: ".to_string(), "x".to_string()),
        ];
        let client = vec![("x-trace".to_string(), "t1".to_string())];
        assert_eq!(
            extract_custom_headers(&attrs, &client),
            vec![
                ("X-Static".to_string(), "v".to_string()),
                ("X-From-Client".to_string(), "t1".to_string()),
            ]
        );
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        apply_gemini_headers(
            &mut headers,
            &[("header:content-type".to_string(), "text/plain".to_string())],
            &[],
        );
        assert_eq!(
            headers,
            vec![("Content-Type".to_string(), "text/plain".to_string())]
        );
    }

    // --- end to end: request + realistic upstream stream ------------------

    /// The Gemini handler's client framing for `alt == ""`
    /// (sdk/api/handlers/gemini/gemini_handlers.go: "data: " + chunk + "\n\n").
    fn client_frames(upstream_sse: &str) -> Vec<String> {
        upstream_sse
            .split('\n')
            .filter_map(|line| json_payload(line.as_bytes()))
            .map(|payload| serde_json::from_slice::<Value>(payload).unwrap())
            .map(|event| rewrite_stream_event(&event))
            .filter(|event| !event.is_null())
            .map(|event| format!("data: {}\n\n", serde_json::to_string(&event).unwrap()))
            .collect()
    }

    #[test]
    fn end_to_end_stream_text_thought_and_function_call() {
        let sig = native_gemini3_signature();
        // A follow-up turn replaying a prior tool round trip from a Claude client.
        let body = j(&format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"weather in Paris?"}}]}},{{"role":"model","parts":[{{"text":"checking","thought":true,"thoughtSignature":"{CLAUDE_CAIS_SAMPLE}"}},{{"functionCall":{{"name":"get_weather","args":{{"city":"Paris"}}}}}}]}},{{"role":"user","parts":[{{"functionResponse":{{"name":"get_weather","response":{{"temp":21}}}}}}]}}],"generationConfig":{{"thinkingConfig":{{"thinkingLevel":"high","includeThoughts":true}}}}}}"#
        ));
        let (url, headers, bytes) = build_request(
            "https://generativelanguage.googleapis.com/",
            "gemini-3-pro-preview",
            true,
            &body,
            "AIza-test",
            &[],
        );
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro-preview:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Goog-Api-Key".to_string(), "AIza-test".to_string()),
            ]
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            j(
                r#"{"contents":[{"role":"user","parts":[{"text":"weather in Paris?"}]},{"role":"model","parts":[{"text":"checking","thought":true},{"functionCall":{"name":"get_weather","args":{"city":"Paris"}},"thoughtSignature":"skip_thought_signature_validator"}]},{"role":"user","parts":[{"functionResponse":{"name":"get_weather","response":{"temp":21}}}]}],"generationConfig":{"thinkingConfig":{"thinkingLevel":"high","includeThoughts":true}},"model":"gemini-3-pro-preview"}"#
            )
        );

        let upstream = format!(
            "data: {{\"candidates\": [{{\"content\": {{\"parts\": [{{\"text\": \"The user wants a forecast.\",\"thought\": true}}],\"role\": \"model\"}},\"index\": 0}}],\"usageMetadata\": {{\"promptTokenCount\": 40,\"totalTokenCount\": 52,\"thoughtsTokenCount\": 12}},\"modelVersion\": \"gemini-3-pro-preview\",\"responseId\": \"r-1\"}}\r\n\r\n\
             data: {{\"candidates\": [{{\"content\": {{\"parts\": [{{\"text\": \"It is 21°C. \"}}],\"role\": \"model\"}},\"index\": 0}}],\"usageMetadata\": {{\"promptTokenCount\": 40,\"candidatesTokenCount\": 6,\"totalTokenCount\": 58,\"thoughtsTokenCount\": 12}},\"modelVersion\": \"gemini-3-pro-preview\",\"responseId\": \"r-1\"}}\r\n\r\n\
             data: {{\"candidates\": [{{\"content\": {{\"parts\": [{{\"functionCall\": {{\"name\": \"get_forecast\",\"args\": {{\"city\": \"Paris\",\"days\": 2}}}},\"thoughtSignature\": \"{sig}\"}}],\"role\": \"model\"}},\"finishReason\": \"STOP\",\"index\": 0}}],\"usageMetadata\": {{\"promptTokenCount\": 40,\"candidatesTokenCount\": 18,\"totalTokenCount\": 70,\"thoughtsTokenCount\": 12}},\"modelVersion\": \"gemini-3-pro-preview\",\"responseId\": \"r-1\"}}\r\n\r\n"
        );
        assert_eq!(
            client_frames(&upstream),
            vec![
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"The user wants a forecast.\",\"thought\":true}],\"role\":\"model\"},\"index\":0}],\"modelVersion\":\"gemini-3-pro-preview\",\"responseId\":\"r-1\",\"cpaUsageMetadata\":{\"promptTokenCount\":40,\"totalTokenCount\":52,\"thoughtsTokenCount\":12}}\n\n".to_string(),
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"It is 21°C. \"}],\"role\":\"model\"},\"index\":0}],\"modelVersion\":\"gemini-3-pro-preview\",\"responseId\":\"r-1\",\"cpaUsageMetadata\":{\"promptTokenCount\":40,\"candidatesTokenCount\":6,\"totalTokenCount\":58,\"thoughtsTokenCount\":12}}\n\n".to_string(),
                format!("data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"functionCall\":{{\"name\":\"get_forecast\",\"args\":{{\"city\":\"Paris\",\"days\":2}}}},\"thoughtSignature\":\"{sig}\"}}],\"role\":\"model\"}},\"finishReason\":\"STOP\",\"index\":0}}],\"usageMetadata\":{{\"promptTokenCount\":40,\"candidatesTokenCount\":18,\"totalTokenCount\":70,\"thoughtsTokenCount\":12}},\"modelVersion\":\"gemini-3-pro-preview\",\"responseId\":\"r-1\"}}\n\n"),
            ]
        );
    }
}
