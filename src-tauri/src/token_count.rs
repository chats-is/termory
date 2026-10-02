// Faithful port: the per-executor counters and the beta helpers are public
// for the router; not every item is reached until it is wired in.
#![allow(dead_code)]

//! Token counting for the local router (`/v1/messages/count_tokens` and
//! `/v1beta/models/{model}:countTokens`), ported from CLIProxyAPI (upstream
//! revision `ed980be`):
//!
//! - `internal/runtime/executor/codex_executor_tokens.go` — `CodexExecutor.CountTokens`,
//!   `tokenizerForCodexModel`, `countCodexInputTokens`;
//! - `internal/runtime/executor/openai_compat_executor.go` — `OpenAICompatExecutor.CountTokens`;
//! - `internal/runtime/executor/helps/token_helpers.go` — `TokenizerForModel`,
//!   `CountOpenAIChatTokens`, `BuildOpenAIUsageJSON` and their collectors;
//! - `internal/runtime/executor/xai_executor_tokens.go` — `XAIExecutor.CountTokens`,
//!   `countXAIInputTokens` and its collectors;
//! - `internal/runtime/executor/claude_executor_tokens.go` — `ClaudeExecutor.CountTokens`
//!   (local estimate), `countTokensUpstream` (body/URL half),
//!   `validateClaudeTokenCountRequest`, `shouldUseClaudeUpstreamTokenCount`;
//! - `internal/runtime/executor/helps/claude_input_tokens.go` — `CountClaudeInputTokens`
//!   and its collectors;
//! - `internal/runtime/executor/claude_executor_request.go` — `isAnthropicUpstreamBase`,
//!   `extractAndRemoveBetas`, `claudeCountTokensBetasForCredential`,
//!   `withClaudeCountTokensOAuthBeta`; `helps/claude_upstream.go` — `IsAnthropicUpstreamURL`;
//! - `internal/runtime/executor/gemini_executor.go` — `GeminiExecutor.CountTokens`
//!   (body/URL half), `resolveGeminiBaseURL`; `helps/gemini_content_turns.go` —
//!   `EnsureGeminiLeadingUserContent`;
//! - `sdk/translator/registry.go` — `TranslateTokenCount`, with the `TokenCount`
//!   translators registered in `internal/translator/*/init.go`
//!   (`ClaudeTokenCount` / `GeminiTokenCount` → `translator/common/bytes.go`
//!   `ClaudeInputTokensJSON` / `GeminiTokenCountJSON`);
//! - `internal/thinking/suffix.go` — `ParseSuffix` (model name half).
//!
//! # How Go counts, per upstream kind
//!
//! The client endpoints (sdk/api/handlers): `POST /v1/messages/count_tokens`
//! (Claude format, model from the body's `model`) and
//! `POST /v1beta/models/{model}:countTokens` (Gemini format, model from the
//! path). There is NO OpenAI client count endpoint. The handler writes the
//! executor's payload verbatim with `Content-Type: application/json`.
//!
//! The executor of the selected upstream then either counts LOCALLY with
//! tiktoken ([`LocalCounter`], [`count_locally`]) on the request ALREADY
//! TRANSLATED into that executor's format, or forwards the translated body to
//! the upstream's own count endpoint ([`RemoteCounter`],
//! [`upstream_count_request`], [`parse_upstream_count`]):
//!
//! | upstream | Go executor | how | body format | encoding |
//! |---|---|---|---|---|
//! | Codex (ChatGPT backend) | `CodexExecutor` | local | `codex` (Responses) | [`tokenizer_for_codex_model`] — cl100k default |
//! | OpenAI-compatible (any Chat/Responses provider) | `OpenAICompatExecutor` | local | `openai` (Chat — Go ALWAYS translates to Chat for counting) | [`tokenizer_for_model`] — o200k default |
//! | xAI / Grok | `XAIExecutor` | local | `codex` (Responses) | o200k always |
//! | Claude, base NOT `https://api.anthropic.com` or no key | `ClaudeExecutor` | local (+ request validation) | `claude` | o200k always |
//! | Claude, `https://api.anthropic.com` with a key | `ClaudeExecutor` | remote `/v1/messages/count_tokens?beta=true` | `claude` | — |
//! | Gemini (API key) | `GeminiExecutor` | remote `/v1beta/models/{m}:countTokens` | `gemini` | — |
//!
//! The executor's own usage payload ([`local_usage_json`], or the upstream's
//! answer verbatim for a remote count) then goes through `TranslateTokenCount`
//! ([`client_count_response`]): a Claude client gets `{"input_tokens":N}` and a
//! Gemini client gets `{"totalTokens":N,"promptTokensDetails":[...]}` whenever
//! a `TokenCount` translator is registered for the pair; otherwise the raw
//! payload is returned unchanged.
//!
//! # Deliberate differences from the Go source
//!
//! - Bodies are `serde_json::Value`s. Where Go appends a value's RAW JSON text
//!   (`params.Raw`, `content.Raw`, …) this module appends its COMPACT
//!   serialization (`Value::to_string`), so the count equals Go's for a
//!   compact body; insignificant whitespace inside a raw sub-document (which
//!   Go would tokenize) is not reproduced. Without serde_json's
//!   `arbitrary_precision` a float is re-printed (`1.50` → `1.5`). Key order
//!   is kept only with `preserve_order` (Termory enables it). Go's
//!   invalid-JSON branches are unreachable.
//! - Everything the executors do to the body BEFORE counting that cannot
//!   change a counted field is left to the caller's translation step and not
//!   repeated here (Codex: deleting `previous_response_id` / `generate` /
//!   `prompt_cache_retention` / `safety_identifier` / `stream_options`,
//!   `stream=false`, `normalizeCodexInstructions` — a null `instructions`
//!   counts as empty either way; thinking application; xAI's
//!   `prepareResponsesRequest` tool normalization, which the router's own xAI
//!   request builder owns).
//! - NOT ported (they belong to a Claude Messages executor, which Termory does
//!   not have): `sanitizeClaudeMessagesForClaudeUpstreamWithDebug` (signature
//!   sanitizing — Go drops e.g. OpenAI-encrypted thinking blocks before both
//!   the local and the remote Claude count), mid-conversation system rebuild,
//!   cloaking / system relocation / sensitive-word obfuscation / MCP aliasing
//!   (all off by default for an API-key credential), `enforceCacheControlLimit`,
//!   `normalizeCacheControlTTL`, `validateClaudeMidSystemMessageModel`,
//!   `StripClaudeCodeAttributionSystem` (claude-code-cli profile only) and
//!   the full `applyClaudeHeaders`. [`UpstreamCountRequest::extra_betas`]
//!   carries what Go folds into `anthropic-beta`; the two count-specific beta
//!   helpers are ported for the header builder.
//! - NOT ported for Gemini: `fixGeminiImageAspectRatio` (image models) and
//!   `SanitizeGeminiRequestThoughtSignatures` (lives, private, in
//!   `gemini_exec.rs`; the caller should run it on the body first to match Go).
//! - Counts are `i64` like Go's `int64`; tiktoken counting cannot fail, so the
//!   only error is the Claude request validation (Go: HTTP 400,
//!   request-scoped).

use serde_json::{json, Value};
use tiktoken_rs::CoreBPE;

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// A tiktoken encoding (tiktoken-go `tokenizer.Encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Cl100kBase,
    O200kBase,
}

impl Encoding {
    fn bpe(self) -> &'static CoreBPE {
        match self {
            Encoding::Cl100kBase => tiktoken_rs::cl100k_base_singleton(),
            Encoding::O200kBase => tiktoken_rs::o200k_base_singleton(),
        }
    }

    /// tiktoken-go `Codec.Count`: ordinary encoding (special-token text is
    /// split like any other text, never mapped to a special token).
    pub fn count(self, text: &str) -> i64 {
        self.bpe().encode_ordinary(text).len() as i64
    }
}

/// The Go executors that estimate locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalCounter {
    /// `CodexExecutor` — the ChatGPT Codex backend.
    Codex,
    /// `OpenAICompatExecutor` — any OpenAI-compatible provider.
    OpenAICompat,
    /// `XAIExecutor` — xAI / Grok Responses.
    Xai,
    /// `ClaudeExecutor` without Anthropic's own origin + key
    /// ([`should_use_claude_upstream_token_count`] is false).
    Claude,
}

impl LocalCounter {
    /// The format the body passed to [`count_locally`] must already be in
    /// (the translator target Go uses before counting).
    pub fn body_format(self) -> &'static str {
        match self {
            LocalCounter::Codex | LocalCounter::Xai => "codex",
            LocalCounter::OpenAICompat => "openai",
            LocalCounter::Claude => "claude",
        }
    }
}

/// The Go executors that ask the upstream to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCounter {
    /// `ClaudeExecutor.countTokensUpstream` — body in `claude` format.
    Claude,
    /// `GeminiExecutor.CountTokens` — body in `gemini` format.
    Gemini,
}

impl RemoteCounter {
    /// The upstream (= body) format.
    pub fn format(self) -> &'static str {
        match self {
            RemoteCounter::Claude => "claude",
            RemoteCounter::Gemini => "gemini",
        }
    }
}

/// A count that Go refuses (`claudeTokenCountValidationError`: HTTP 400,
/// request-scoped — no failover).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CountError {
    BadRequest(String),
}

impl std::fmt::Display for CountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CountError::BadRequest(msg) => f.write_str(msg),
        }
    }
}

/// Count `body` (already in [`LocalCounter::body_format`]) the way the Go
/// executor does. `model` is the requested model; a thinking suffix
/// (`gpt-5(high)`) is stripped first, as Go does with `ParseSuffix`.
///
/// port of the counting half of CodexExecutor.CountTokens
/// (codex_executor_tokens.go), OpenAICompatExecutor.CountTokens
/// (openai_compat_executor.go), XAIExecutor.CountTokens
/// (xai_executor_tokens.go) and the local branch of ClaudeExecutor.CountTokens
/// (claude_executor_tokens.go)
pub fn count_locally(counter: LocalCounter, model: &str, body: &Value) -> Result<i64, CountError> {
    let base_model = parse_suffix_model_name(model);
    Ok(match counter {
        LocalCounter::Codex => {
            count_codex_input_tokens(tokenizer_for_codex_model(base_model), body)
        }
        LocalCounter::OpenAICompat => {
            count_openai_chat_tokens(tokenizer_for_model(base_model), body)
        }
        LocalCounter::Xai => count_xai_input_tokens(Encoding::O200kBase, body),
        LocalCounter::Claude => {
            validate_claude_token_count_request(body)?;
            count_claude_input_tokens(body)
        }
    })
}

/// The usage payload each local executor hands to `TranslateTokenCount` —
/// what a client with no registered `TokenCount` translator receives.
///
/// port of the `usageJSON` literals in CodexExecutor.CountTokens /
/// XAIExecutor.CountTokens / ClaudeExecutor.CountTokens and of
/// BuildOpenAIUsageJSON (helps/token_helpers.go)
pub fn local_usage_json(counter: LocalCounter, count: i64) -> Value {
    match counter {
        LocalCounter::Codex | LocalCounter::Xai => json!({
            "response": {"usage": {"input_tokens": count, "output_tokens": 0, "total_tokens": count}}
        }),
        LocalCounter::OpenAICompat => build_openai_usage_json(count),
        LocalCounter::Claude => json!({"input_tokens": count}),
    }
}

/// The request Go sends to an upstream's own count endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamCountRequest {
    /// Full URL.
    pub url: String,
    /// JSON body.
    pub body: Value,
    /// Claude only: betas lifted out of the body plus
    /// `token-counting-2024-11-01`, which Go merges into `anthropic-beta`.
    pub extra_betas: Vec<String>,
}

/// Build the upstream count request from a body already translated into the
/// upstream's format. `base_url` is the credential's base (empty = the
/// vendor's default); `model` is the requested model (suffix stripped here).
///
/// port of the request half of ClaudeExecutor.countTokensUpstream
/// (claude_executor_tokens.go) and GeminiExecutor.CountTokens
/// (gemini_executor.go)
pub fn upstream_count_request(
    counter: RemoteCounter,
    base_url: &str,
    model: &str,
    body: &Value,
) -> UpstreamCountRequest {
    let base_model = parse_suffix_model_name(model);
    let mut body = body.clone();
    match counter {
        RemoteCounter::Claude => {
            let base = if base_url.is_empty() {
                "https://api.anthropic.com"
            } else {
                base_url
            };
            let url = format!("{base}/v1/messages/count_tokens?beta=true");
            set_string_if_different(&mut body, "model", base_model);
            let mut extra_betas = extract_and_remove_betas(&mut body);
            // Claude Code 2.1.220's beta.messages.countTokens() always appends this beta.
            extra_betas.push(CLAUDE_TOKEN_COUNTING_BETA.to_string());
            // api.anthropic.com rejects these on count_tokens ("Extra inputs are
            // not permitted"); this path is only taken for that origin.
            if is_anthropic_upstream_base(base) {
                sj_delete(&mut body, "metadata");
                sj_delete(&mut body, "context_management");
                sj_delete(&mut body, "diagnostics");
            }
            UpstreamCountRequest {
                url,
                body,
                extra_betas,
            }
        }
        RemoteCounter::Gemini => {
            sj_delete(&mut body, "tools");
            sj_delete(&mut body, "generationConfig");
            sj_delete(&mut body, "safetySettings");
            set_string_if_different(&mut body, "model", base_model);
            ensure_gemini_leading_user_content(&mut body, "contents");
            let base = resolve_gemini_base_url(base_url);
            let url = format!("{base}/{GL_API_VERSION}/models/{base_model}:countTokens");
            UpstreamCountRequest {
                url,
                body,
                extra_betas: Vec::new(),
            }
        }
    }
}

/// The count read from a 2xx upstream answer (`gjson ... .Int()`: a missing
/// field reads 0 — Go does not fail on it). The answer itself is what Go
/// passes to [`client_count_response`] as `raw`.
///
/// port of `gjson.GetBytes(data, "input_tokens").Int()` (countTokensUpstream)
/// and `gjson.GetBytes(data, "totalTokens").Int()` (GeminiExecutor.CountTokens)
pub fn parse_upstream_count(counter: RemoteCounter, answer: &Value) -> i64 {
    let field = match counter {
        RemoteCounter::Claude => "input_tokens",
        RemoteCounter::Gemini => "totalTokens",
    };
    gj_int(gj_get(answer, field))
}

/// The client-format answer. `upstream_format` is the executor's format
/// (`codex` for Codex and xAI, `openai` for OpenAI-compatible, `claude`,
/// `gemini`); `raw` is [`local_usage_json`] for a local count or the
/// upstream's answer for a remote one, returned unchanged when no
/// `TokenCount` translator is registered for `(client_format,
/// upstream_format)`.
///
/// port of Registry.TranslateTokenCount (sdk/translator/registry.go) over the
/// `TokenCount` registrations in internal/translator/*/init.go
pub fn client_count_response(
    upstream_format: &str,
    client_format: &str,
    count: i64,
    raw: &Value,
) -> Value {
    // translator.Register(<client>, <upstream>, …, TokenCount: …): the
    // antigravity pairs are omitted (no such upstream here).
    match (client_format, upstream_format) {
        // codex/claude, gemini/claude, openai/claude init.go
        ("claude", "codex") | ("claude", "gemini") | ("claude", "openai") => {
            claude_token_count(count)
        }
        // gemini/gemini, claude/gemini, codex/gemini, openai/gemini init.go
        ("gemini", "gemini")
        | ("gemini", "claude")
        | ("gemini", "codex")
        | ("gemini", "openai") => gemini_token_count(count),
        _ => raw.clone(),
    }
}

// ---------------------------------------------------------------------------
// translators (internal/translator/*)
// ---------------------------------------------------------------------------

/// port of ClaudeTokenCount (translator/{codex,gemini,openai}/claude/*_response.go)
/// → ClaudeInputTokensJSON (translator/common/bytes.go)
pub fn claude_token_count(count: i64) -> Value {
    json!({"input_tokens": count})
}

/// port of GeminiTokenCount (translator/{gemini,claude,codex,openai}/gemini/*_response.go)
/// → GeminiTokenCountJSON (translator/common/bytes.go)
pub fn gemini_token_count(count: i64) -> Value {
    json!({
        "totalTokens": count,
        "promptTokensDetails": [{"modality": "TEXT", "tokenCount": count}]
    })
}

// ---------------------------------------------------------------------------
// tokenizer selection
// ---------------------------------------------------------------------------

/// tiktoken-go `tokenizer.ForModel` for the models these functions name:
/// O1/GPT5/GPT41/GPT4o/O3/O4Mini → o200k_base; GPT4/GPT35Turbo → cl100k_base.
///
/// port of tokenizerForCodexModel (codex_executor_tokens.go)
pub fn tokenizer_for_codex_model(model: &str) -> Encoding {
    let sanitized = model.trim().to_lowercase();
    let s = sanitized.as_str();
    if !s.is_empty()
        && (s.starts_with("gpt-5") || s.starts_with("gpt-4.1") || s.starts_with("gpt-4o"))
    {
        return Encoding::O200kBase;
    }
    // "", gpt-4*, gpt-3.5*, gpt-3* and the default are all cl100k_base.
    Encoding::Cl100kBase
}

/// port of TokenizerForModel (helps/token_helpers.go)
pub fn tokenizer_for_model(model: &str) -> Encoding {
    let sanitized = model.trim().to_lowercase();
    let s = sanitized.as_str();
    if s.is_empty() {
        return Encoding::Cl100kBase;
    }
    if s.starts_with("gpt-5") || s.starts_with("gpt-4.1") || s.starts_with("gpt-4o") {
        return Encoding::O200kBase;
    }
    if s.starts_with("gpt-4") || s.starts_with("gpt-3.5") || s.starts_with("gpt-3") {
        return Encoding::Cl100kBase;
    }
    // o1 / o3 / o4 (tokenizer.O1 / O3 / O4Mini) and the default: o200k_base.
    Encoding::O200kBase
}

/// port of ParseSuffix (internal/thinking/suffix.go), ModelName only
fn parse_suffix_model_name(model: &str) -> &str {
    let Some(last_open) = model.rfind('(') else {
        return model;
    };
    if !model.ends_with(')') {
        return model;
    }
    &model[..last_open]
}

// ---------------------------------------------------------------------------
// Codex (codex_executor_tokens.go)
// ---------------------------------------------------------------------------

/// port of countCodexInputTokens (codex_executor_tokens.go)
pub fn count_codex_input_tokens(enc: Encoding, body: &Value) -> i64 {
    let mut segments: Vec<String> = Vec::new();

    let inst = gj_string(gj_get(body, "instructions"));
    let inst = inst.trim();
    if !inst.is_empty() {
        segments.push(inst.to_string());
    }

    if let Some(Value::Array(items)) = gj_get(body, "input") {
        for item in items {
            match gj_string(gj_get(item, "type")).as_str() {
                "message" => {
                    if let Some(Value::Array(parts)) = gj_get(item, "content") {
                        for part in parts {
                            push_trimmed(&mut segments, &gj_string(gj_get(part, "text")));
                        }
                    }
                }
                "function_call" => {
                    push_trimmed(&mut segments, &gj_string(gj_get(item, "name")));
                    push_trimmed(&mut segments, &gj_string(gj_get(item, "arguments")));
                }
                "function_call_output" => {
                    push_trimmed(&mut segments, &gj_string(gj_get(item, "output")));
                }
                _ => {
                    push_trimmed(&mut segments, &gj_string(gj_get(item, "text")));
                }
            }
        }
    }

    if let Some(Value::Array(tools)) = gj_get(body, "tools") {
        for tool in tools {
            push_trimmed(&mut segments, &gj_string(gj_get(tool, "name")));
            push_trimmed(&mut segments, &gj_string(gj_get(tool, "description")));
            if let Some(params) = gj_get(tool, "parameters") {
                push_trimmed(&mut segments, &gj_string_or_raw(params));
            }
        }
    }

    if let Some(text_format) = gj_get(body, "text.format") {
        push_trimmed(&mut segments, &gj_string(gj_get(text_format, "name")));
        if let Some(schema) = gj_get(text_format, "schema") {
            push_trimmed(&mut segments, &gj_string_or_raw(schema));
        }
    }

    let text = segments.join("\n");
    if text.is_empty() {
        return 0;
    }
    enc.count(&text)
}

// ---------------------------------------------------------------------------
// OpenAI Chat (helps/token_helpers.go)
// ---------------------------------------------------------------------------

/// port of CountOpenAIChatTokens (helps/token_helpers.go)
pub fn count_openai_chat_tokens(enc: Encoding, payload: &Value) -> i64 {
    let mut segments: Vec<String> = Vec::new();

    collect_openai_messages(gj_get(payload, "messages"), &mut segments);
    collect_openai_tools(gj_get(payload, "tools"), &mut segments);
    collect_openai_functions(gj_get(payload, "functions"), &mut segments);
    collect_openai_tool_choice(gj_get(payload, "tool_choice"), &mut segments);
    collect_openai_response_format(gj_get(payload, "response_format"), &mut segments);
    push_trimmed(&mut segments, &gj_string(gj_get(payload, "input")));
    push_trimmed(&mut segments, &gj_string(gj_get(payload, "prompt")));

    let joined = segments.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        return 0;
    }
    enc.count(joined)
}

/// port of BuildOpenAIUsageJSON (helps/token_helpers.go)
pub fn build_openai_usage_json(count: i64) -> Value {
    json!({"usage": {"prompt_tokens": count, "completion_tokens": 0, "total_tokens": count}})
}

/// port of collectOpenAIMessages (helps/token_helpers.go)
fn collect_openai_messages(messages: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(messages)) = messages else {
        return;
    };
    for message in messages {
        push_trimmed(segments, &gj_string(gj_get(message, "role")));
        push_trimmed(segments, &gj_string(gj_get(message, "name")));
        collect_openai_content(gj_get(message, "content"), segments);
        collect_openai_tool_calls(gj_get(message, "tool_calls"), segments);
        collect_openai_function_call(gj_get(message, "function_call"), segments);
    }
}

/// port of collectOpenAIContent (helps/token_helpers.go)
fn collect_openai_content(content: Option<&Value>, segments: &mut Vec<String>) {
    let Some(content) = content else {
        return;
    };
    match content {
        Value::String(s) => push_trimmed(segments, s),
        Value::Array(parts) => {
            for part in parts {
                match gj_string(gj_get(part, "type")).as_str() {
                    "text" | "input_text" | "output_text" => {
                        push_trimmed(segments, &gj_string(gj_get(part, "text")));
                    }
                    "image_url" => {
                        push_trimmed(segments, &gj_string(gj_get(part, "image_url.url")));
                    }
                    "input_audio" | "output_audio" | "audio" => {
                        push_trimmed(segments, &gj_string(gj_get(part, "id")));
                    }
                    "tool_result" => {
                        push_trimmed(segments, &gj_string(gj_get(part, "name")));
                        collect_openai_content(gj_get(part, "content"), segments);
                    }
                    _ => {
                        if part.is_array() {
                            collect_openai_content(Some(part), segments);
                        } else if part.is_object() {
                            push_trimmed(segments, &part.to_string());
                        } else {
                            push_trimmed(segments, &gj_string(Some(part)));
                        }
                    }
                }
            }
        }
        Value::Object(_) => push_trimmed(segments, &content.to_string()),
        _ => {}
    }
}

/// port of collectOpenAIToolCalls (helps/token_helpers.go)
fn collect_openai_tool_calls(calls: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(calls)) = calls else {
        return;
    };
    for call in calls {
        push_trimmed(segments, &gj_string(gj_get(call, "id")));
        push_trimmed(segments, &gj_string(gj_get(call, "type")));
        if let Some(function) = gj_get(call, "function") {
            push_trimmed(segments, &gj_string(gj_get(function, "name")));
            push_trimmed(segments, &gj_string(gj_get(function, "description")));
            push_trimmed(segments, &gj_string(gj_get(function, "arguments")));
            if let Some(params) = gj_get(function, "parameters") {
                push_trimmed(segments, &gj_raw(params));
            }
        }
    }
}

/// port of collectOpenAIFunctionCall (helps/token_helpers.go)
fn collect_openai_function_call(call: Option<&Value>, segments: &mut Vec<String>) {
    let Some(call) = call else {
        return;
    };
    push_trimmed(segments, &gj_string(gj_get(call, "name")));
    push_trimmed(segments, &gj_string(gj_get(call, "arguments")));
}

/// port of collectOpenAITools (helps/token_helpers.go)
fn collect_openai_tools(tools: Option<&Value>, segments: &mut Vec<String>) {
    let Some(tools) = tools else {
        return;
    };
    if let Value::Array(items) = tools {
        for tool in items {
            append_tool_payload(tool, segments);
        }
        return;
    }
    append_tool_payload(tools, segments);
}

/// port of collectOpenAIFunctions (helps/token_helpers.go)
fn collect_openai_functions(functions: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(functions)) = functions else {
        return;
    };
    for function in functions {
        push_trimmed(segments, &gj_string(gj_get(function, "name")));
        push_trimmed(segments, &gj_string(gj_get(function, "description")));
        if let Some(params) = gj_get(function, "parameters") {
            push_trimmed(segments, &gj_raw(params));
        }
    }
}

/// port of collectOpenAIToolChoice (helps/token_helpers.go)
fn collect_openai_tool_choice(choice: Option<&Value>, segments: &mut Vec<String>) {
    let Some(choice) = choice else {
        return;
    };
    if let Value::String(s) = choice {
        push_trimmed(segments, s);
        return;
    }
    push_trimmed(segments, &gj_raw(choice));
}

/// port of collectOpenAIResponseFormat (helps/token_helpers.go)
fn collect_openai_response_format(format: Option<&Value>, segments: &mut Vec<String>) {
    let Some(format) = format else {
        return;
    };
    push_trimmed(segments, &gj_string(gj_get(format, "type")));
    push_trimmed(segments, &gj_string(gj_get(format, "name")));
    if let Some(schema) = gj_get(format, "json_schema") {
        push_trimmed(segments, &gj_raw(schema));
    }
    if let Some(schema) = gj_get(format, "schema") {
        push_trimmed(segments, &gj_raw(schema));
    }
}

/// port of appendToolPayload (helps/token_helpers.go)
fn append_tool_payload(tool: &Value, segments: &mut Vec<String>) {
    push_trimmed(segments, &gj_string(gj_get(tool, "type")));
    push_trimmed(segments, &gj_string(gj_get(tool, "name")));
    push_trimmed(segments, &gj_string(gj_get(tool, "description")));
    if let Some(function) = gj_get(tool, "function") {
        push_trimmed(segments, &gj_string(gj_get(function, "name")));
        push_trimmed(segments, &gj_string(gj_get(function, "description")));
        if let Some(params) = gj_get(function, "parameters") {
            push_trimmed(segments, &gj_raw(params));
        }
    }
}

// ---------------------------------------------------------------------------
// xAI (xai_executor_tokens.go)
// ---------------------------------------------------------------------------

/// port of xaiFunctionToolType (xai_executor.go)
const XAI_FUNCTION_TOOL_TYPE: &str = "function";

/// port of countXAIInputTokens (xai_executor_tokens.go)
pub fn count_xai_input_tokens(enc: Encoding, body: &Value) -> i64 {
    let mut segments: Vec<String> = Vec::new();
    xai_append_token_string(&mut segments, gj_get(body, "instructions"));
    xai_collect_input_token_segments(gj_get(body, "input"), &mut segments);
    xai_collect_tool_token_segments(gj_get(body, "tools"), &mut segments);

    if let Some(text_format) = gj_get(body, "text.format") {
        xai_append_token_string(&mut segments, gj_get(text_format, "name"));
        xai_append_token_json(&mut segments, gj_get(text_format, "schema"));
    }

    if segments.is_empty() {
        return 0;
    }
    enc.count(&segments.join("\n"))
}

/// port of xaiCollectInputTokenSegments (xai_executor_tokens.go)
fn xai_collect_input_token_segments(input: Option<&Value>, segments: &mut Vec<String>) {
    match input {
        Some(Value::String(_)) => xai_append_token_string(segments, input),
        Some(Value::Array(items)) => {
            for item in items {
                match gj_string(gj_get(item, "type")).as_str() {
                    "message" => {
                        xai_collect_content_token_segments(gj_get(item, "content"), segments)
                    }
                    "function_call" => {
                        xai_append_token_string(segments, gj_get(item, "name"));
                        xai_append_token_json(segments, gj_get(item, "arguments"));
                    }
                    "function_call_output" => {
                        xai_append_token_json(segments, gj_get(item, "output"));
                    }
                    "reasoning" => {
                        for part in gj_array(gj_get(item, "summary")) {
                            xai_append_token_string(segments, gj_get(part, "text"));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// port of xaiCollectContentTokenSegments (xai_executor_tokens.go)
fn xai_collect_content_token_segments(content: Option<&Value>, segments: &mut Vec<String>) {
    match content {
        Some(Value::String(_)) => xai_append_token_string(segments, content),
        Some(Value::Array(parts)) => {
            for part in parts {
                match gj_string(gj_get(part, "type")).as_str() {
                    "text" | "input_text" | "output_text" => {
                        xai_append_token_string(segments, gj_get(part, "text"));
                    }
                    "refusal" => xai_append_token_string(segments, gj_get(part, "refusal")),
                    "input_image" => {
                        xai_append_token_string(segments, gj_get(part, "image_url"));
                        xai_append_token_string(segments, gj_get(part, "file_id"));
                    }
                    "input_file" => {
                        xai_append_token_string(segments, gj_get(part, "file_data"));
                        xai_append_token_string(segments, gj_get(part, "file_url"));
                        xai_append_token_string(segments, gj_get(part, "file_id"));
                        xai_append_token_string(segments, gj_get(part, "filename"));
                    }
                    "input_audio" => {
                        xai_append_token_string(segments, gj_get(part, "data"));
                        xai_append_token_string(segments, gj_get(part, "input_audio.data"));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// port of xaiCollectToolTokenSegments (xai_executor_tokens.go)
fn xai_collect_tool_token_segments(tools: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    for tool in tools {
        if gj_string(gj_get(tool, "type")) != XAI_FUNCTION_TOOL_TYPE {
            continue;
        }
        xai_append_token_string(segments, gj_get(tool, "name"));
        xai_append_token_string(segments, gj_get(tool, "description"));
        xai_append_token_json(segments, gj_get(tool, "parameters"));
    }
}

/// port of xaiAppendTokenString (xai_executor_tokens.go)
fn xai_append_token_string(segments: &mut Vec<String>, value: Option<&Value>) {
    push_trimmed(segments, &gj_string(value));
}

/// port of xaiAppendTokenJSON (xai_executor_tokens.go)
fn xai_append_token_json(segments: &mut Vec<String>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    push_trimmed(segments, &gj_string_or_raw(value));
}

// ---------------------------------------------------------------------------
// Claude (claude_executor_tokens.go, helps/claude_input_tokens.go)
// ---------------------------------------------------------------------------

/// port of claudeTokenCountingBeta (claude_executor_request.go)
const CLAUDE_TOKEN_COUNTING_BETA: &str = "token-counting-2024-11-01";
/// port of claudeOAuthBeta (claude_executor_request.go)
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// port of claudeCodeBeta (claude_executor_request.go)
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
/// port of claudeCountTokensBetas (claude_executor_request.go): the fixed
/// profile Claude Code 2.1.220 sends to /v1/messages/count_tokens.
const CLAUDE_COUNT_TOKENS_BETAS: [&str; 4] = [
    CLAUDE_CODE_BETA,
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    CLAUDE_TOKEN_COUNTING_BETA,
];

/// port of CountClaudeInputTokens (helps/claude_input_tokens.go) — O200kBase
pub fn count_claude_input_tokens(payload: &Value) -> i64 {
    count_claude_input_tokens_with(Encoding::O200kBase, payload)
}

/// port of countClaudeInputTokens (helps/claude_input_tokens.go)
fn count_claude_input_tokens_with(enc: Encoding, payload: &Value) -> i64 {
    let segments = collect_claude_input_token_segments(payload);
    if segments.is_empty() {
        return 0;
    }
    enc.count(&segments.join("\n"))
}

/// port of collectClaudeInputTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_input_token_segments(payload: &Value) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    collect_claude_system_token_segments(gj_get(payload, "system"), &mut segments);
    collect_claude_message_token_segments(gj_get(payload, "messages"), &mut segments);
    collect_claude_tool_token_segments(gj_get(payload, "tools"), &mut segments);
    collect_claude_tool_choice_token_segments(gj_get(payload, "tool_choice"), &mut segments);
    segments
}

/// port of collectClaudeSystemTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_system_token_segments(system: Option<&Value>, segments: &mut Vec<String>) {
    match system {
        Some(Value::String(s)) => push_trimmed(segments, s),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Value::String(s) = part {
                    push_trimmed(segments, s);
                } else if gj_string(gj_get(part, "type")) == "text" {
                    push_trimmed(segments, &gj_string(gj_get(part, "text")));
                }
            }
        }
        _ => {}
    }
}

/// port of collectClaudeMessageTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_message_token_segments(messages: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(messages)) = messages else {
        return;
    };
    for message in messages {
        push_trimmed(segments, &gj_string(gj_get(message, "role")));
        collect_claude_content_token_segments(gj_get(message, "content"), segments);
    }
}

/// port of collectClaudeContentTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_content_token_segments(content: Option<&Value>, segments: &mut Vec<String>) {
    let Some(content) = content else {
        return;
    };
    match content {
        Value::String(s) => {
            push_trimmed(segments, s);
            return;
        }
        Value::Array(parts) => {
            for part in parts {
                collect_claude_content_token_segments(Some(part), segments);
            }
            return;
        }
        Value::Object(_) => {}
        _ => return,
    }

    let field = |name: &str| gj_string(gj_get(content, name));
    match field("type").as_str() {
        "text" => push_trimmed(segments, &field("text")),
        "thinking" => push_trimmed(segments, &field("thinking")),
        "document" => collect_claude_document_token_segments(content, segments),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            push_trimmed(segments, &field("id"));
            push_trimmed(segments, &field("name"));
            append_claude_token_json(segments, gj_get(content, "input"));
        }
        "tool_result"
        | "mcp_tool_result"
        | "web_search_tool_result"
        | "web_fetch_tool_result"
        | "code_execution_tool_result"
        | "bash_code_execution_tool_result"
        | "text_editor_code_execution_tool_result" => {
            push_trimmed(segments, &field("tool_use_id"));
            push_trimmed(segments, &field("tool_call_id"));
            collect_claude_content_token_segments(gj_get(content, "content"), segments);
        }
        "web_search_result" | "search_result" => {
            if let Some(Value::String(source)) = gj_get(content, "source") {
                push_trimmed(segments, source);
            }
            push_trimmed(segments, &field("title"));
            push_trimmed(segments, &field("url"));
            push_trimmed(segments, &field("page_age"));
            collect_claude_content_token_segments(gj_get(content, "content"), segments);
        }
        "web_fetch_result" => {
            push_trimmed(segments, &field("url"));
            push_trimmed(segments, &field("retrieved_at"));
            collect_claude_content_token_segments(gj_get(content, "content"), segments);
        }
        "code_execution_result"
        | "bash_code_execution_result"
        | "text_editor_code_execution_result" => {
            push_trimmed(segments, &field("stdout"));
            push_trimmed(segments, &field("stderr"));
            push_trimmed(segments, &field("return_code"));
            collect_claude_content_token_segments(gj_get(content, "content"), segments);
            collect_claude_content_token_segments(gj_get(content, "output"), segments);
        }
        "tool_reference" => push_trimmed(segments, &field("tool_name")),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => append_claude_token_json(segments, Some(content)),
        _ => push_trimmed(segments, &field("text")),
    }
}

/// port of collectClaudeDocumentTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_document_token_segments(document: &Value, segments: &mut Vec<String>) {
    let source = gj_get(document, "source");
    if gj_string(source.and_then(|s| gj_get(s, "type"))) != "text" {
        return;
    }
    push_trimmed(segments, &gj_string(gj_get(document, "title")));
    push_trimmed(segments, &gj_string(gj_get(document, "context")));
    push_trimmed(segments, &gj_string(source.and_then(|s| gj_get(s, "data"))));
    push_trimmed(
        segments,
        &gj_string(source.and_then(|s| gj_get(s, "content"))),
    );
}

/// port of collectClaudeToolTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_tool_token_segments(tools: Option<&Value>, segments: &mut Vec<String>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    for tool in tools {
        push_trimmed(segments, &gj_string(gj_get(tool, "type")));
        push_trimmed(segments, &gj_string(gj_get(tool, "name")));
        push_trimmed(segments, &gj_string(gj_get(tool, "description")));
        append_claude_token_json(segments, gj_get(tool, "input_schema"));
    }
}

/// port of collectClaudeToolChoiceTokenSegments (helps/claude_input_tokens.go)
fn collect_claude_tool_choice_token_segments(choice: Option<&Value>, segments: &mut Vec<String>) {
    let Some(choice) = choice else {
        return;
    };
    if let Value::String(s) = choice {
        push_trimmed(segments, s);
        return;
    }
    push_trimmed(segments, &gj_string(gj_get(choice, "type")));
    push_trimmed(segments, &gj_string(gj_get(choice, "name")));
}

/// port of appendClaudeTokenJSON (helps/claude_input_tokens.go) — Go
/// compacts the raw JSON; `Value::to_string` is compact already.
fn append_claude_token_json(segments: &mut Vec<String>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    push_trimmed(segments, &gj_string_or_raw(value));
}

/// port of validateClaudeTokenCountRequest (claude_executor_tokens.go)
pub fn validate_claude_token_count_request(body: &Value) -> Result<(), CountError> {
    let bad = |msg: &str| Err(CountError::BadRequest(msg.to_string()));
    if !body.is_object() {
        return bad("Claude token count request must be a JSON object");
    }
    let messages = match gj_get(body, "messages") {
        Some(Value::Array(items)) if !items.is_empty() => items,
        _ => return bad("Claude token count request messages must be a non-empty array"),
    };
    for message in messages {
        if !message.is_object() {
            return bad("Claude token count request messages must contain objects");
        }
        let role = gj_string(gj_get(message, "role"));
        if role != "user" && role != "assistant" {
            return bad("Claude token count request message role must be user or assistant");
        }
        let blocks = match gj_get(message, "content") {
            Some(Value::String(_)) => continue,
            Some(Value::Array(blocks)) => blocks,
            _ => {
                return bad("Claude token count request message content must be a string or array")
            }
        };
        for block in blocks {
            let typed = matches!(gj_get(block, "type"), Some(Value::String(t)) if !t.is_empty());
            if !block.is_object() || !typed {
                return bad("Claude token count request content blocks must be typed objects");
            }
        }
    }
    Ok(())
}

/// Whether Go sends the count to Anthropic instead of estimating: only
/// Anthropic's first-party origin with a credential. An empty `base_url` is
/// `https://api.anthropic.com`, as ClaudeExecutor.CountTokens defaults it
/// before asking.
///
/// port of shouldUseClaudeUpstreamTokenCount (claude_executor_tokens.go)
pub fn should_use_claude_upstream_token_count(api_key: &str, base_url: &str) -> bool {
    let base_url = if base_url.is_empty() {
        "https://api.anthropic.com"
    } else {
        base_url
    };
    !api_key.trim().is_empty() && is_anthropic_upstream_base(base_url)
}

/// port of isAnthropicUpstreamBase (claude_executor_request.go) →
/// IsAnthropicUpstreamURL (helps/claude_upstream.go): `https`, no userinfo,
/// host `api.anthropic.com`, port empty or 443. The `url.Parse` it relies on
/// is reduced to the authority split this check needs.
fn is_anthropic_upstream_base(base_url: &str) -> bool {
    let raw = base_url.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (authority, ""),
    };
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    host.eq_ignore_ascii_case("api.anthropic.com") && (port.is_empty() || port == "443")
}

/// port of extractAndRemoveBetas (claude_executor_request.go)
fn extract_and_remove_betas(body: &mut Value) -> Vec<String> {
    let Some(value) = gj_get(body, "betas") else {
        return Vec::new();
    };
    let mut betas = Vec::new();
    if let Value::Array(items) = value {
        for item in items {
            let s = gj_string(Some(item));
            let s = s.trim();
            if !s.is_empty() {
                betas.push(s.to_string());
            }
        }
    } else {
        let s = gj_string(Some(value));
        let s = s.trim();
        if !s.is_empty() {
            betas.push(s.to_string());
        }
    }
    sj_delete(body, "betas");
    betas
}

/// The `anthropic-beta` base value Go sends to count_tokens when it does not
/// preserve the caller's fingerprint.
///
/// port of claudeCountTokensBetasForCredential (claude_executor_request.go)
pub fn claude_count_tokens_betas_for_credential(oauth_token: bool) -> String {
    let mut betas: Vec<&str> = vec![CLAUDE_CODE_BETA];
    if oauth_token {
        betas.push(CLAUDE_OAUTH_BETA);
    }
    betas.extend_from_slice(&CLAUDE_COUNT_TOKENS_BETAS[1..]);
    betas.join(",")
}

/// port of withClaudeCountTokensOAuthBeta (claude_executor_request.go)
pub fn with_claude_count_tokens_oauth_beta(betas: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for beta in betas.split(',') {
        let beta = beta.trim();
        if !beta.is_empty() && !parts.iter().any(|p| p == beta) {
            parts.push(beta.to_string());
        }
    }
    if parts.iter().any(|p| p == CLAUDE_OAUTH_BETA) {
        return parts.join(",");
    }
    let insert_at = usize::from(parts.first().is_some_and(|p| p == CLAUDE_CODE_BETA));
    parts.insert(insert_at, CLAUDE_OAUTH_BETA.to_string());
    parts.join(",")
}

// ---------------------------------------------------------------------------
// Gemini (gemini_executor.go, helps/gemini_content_turns.go)
// ---------------------------------------------------------------------------

/// port of glEndpoint (gemini_executor.go)
const GL_ENDPOINT: &str = "https://generativelanguage.googleapis.com";
/// port of glAPIVersion (gemini_executor.go)
const GL_API_VERSION: &str = "v1beta";

/// port of resolveGeminiBaseURL (gemini_executor.go); the auth attribute is
/// the `base_url` argument.
fn resolve_gemini_base_url(base_url: &str) -> String {
    let custom = base_url.trim();
    let base = if custom.is_empty() {
        GL_ENDPOINT
    } else {
        custom.trim_end_matches('/')
    };
    if base.is_empty() {
        return GL_ENDPOINT.to_string();
    }
    base.to_string()
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
    // port of emptyGeminiUserTurnJSON (helps/gemini_content_turns.go)
    contents.insert(0, json!({"role":"user","parts":[{"text":""}]}));
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

/// gjson.Result.String(): strings verbatim, null/missing empty, numbers and
/// booleans as text, objects/arrays as (compact) raw JSON.
fn gj_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// gjson.Result.Raw for an EXISTING value (an explicit null is "null").
fn gj_raw(value: &Value) -> String {
    value.to_string()
}

/// The `if v.Type == gjson.String { v.String() } else { v.Raw }` idiom.
fn gj_string_or_raw(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => gj_raw(other),
    }
}

/// gjson.Result.Array(): an array's items, nothing for null/missing, and the
/// value itself as a one-item list otherwise.
fn gj_array(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// gjson.Result.Int(): numbers truncated, numeric strings parsed, true = 1.
fn gj_int(value: Option<&Value>) -> i64 {
    match value {
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

/// sjson.Delete of a top-level key, keeping the order of its siblings.
fn sj_delete(root: &mut Value, key: &str) {
    if let Value::Object(map) = root {
        map.shift_remove(key);
    }
}

/// port of SetStringIfDifferent (helps/payload_mutations.go), top-level key.
fn set_string_if_different(payload: &mut Value, key: &str, value: &str) {
    let Value::Object(map) = payload else {
        return;
    };
    if let Some(Value::String(current)) = map.get(key) {
        if current == value {
            return;
        }
    }
    map.insert(key.to_string(), Value::from(value));
}

/// addIfNotEmpty / appendClaudeTokenString / the `strings.TrimSpace(...) != ""`
/// idiom: append the trimmed value when non-empty.
fn push_trimmed(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Value {
        serde_json::from_str(raw).expect("test JSON")
    }

    // --- helps/claude_input_tokens_test.go ---

    // port of TestCollectClaudeInputTokenSegments (helps/claude_input_tokens_test.go)
    #[test]
    fn collect_claude_input_token_segments_matches_go() {
        let payload = parse(
            r#"{
            "model":"claude-test",
            "system":[
                {"type":"text","text":"Follow repository rules.","cache_control":{"type":"ephemeral"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ignored-system-image"}}
            ],
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"Review the implementation."},
                    {"type":"document","source":{"type":"text","data":"Reference document text."}},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ignored-image"}}
                ]},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"Inspect the relevant files.","signature":"ignored-signature"},
                    {"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"main.go"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":[
                        {"type":"text","text":"package main"},
                        {"type":"image","source":{"type":"base64","data":"ignored-tool-image"}}
                    ]}
                ]}
            ],
            "tools":[{
                "name":"read_file",
                "description":"Reads a repository file.",
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}}},
                "cache_control":{"type":"ephemeral"}
            }],
            "tool_choice":{"type":"tool","name":"read_file"},
            "metadata":{"user_id":"ignored-metadata"},
            "max_tokens":4096,
            "stream":true
        }"#,
        );
        let got = collect_claude_input_token_segments(&payload);
        let want = vec![
            "Follow repository rules.",
            "user",
            "Review the implementation.",
            "Reference document text.",
            "assistant",
            "Inspect the relevant files.",
            "toolu_1",
            "read_file",
            r#"{"path":"main.go"}"#,
            "user",
            "toolu_1",
            "package main",
            "read_file",
            "Reads a repository file.",
            r#"{"type":"object","properties":{"path":{"type":"string"}}}"#,
            "tool",
            "read_file",
        ];
        assert_eq!(got, want);
    }

    // port of TestCollectClaudeInputTokenSegmentsIncludesKnownToolResults (helps/claude_input_tokens_test.go)
    #[test]
    fn collect_claude_input_token_segments_includes_known_tool_results() {
        let payload = parse(
            r#"{
            "messages":[{"role":"user","content":[
                {"type":"web_search_tool_result","tool_use_id":"ws_tool_1","content":[
                    {"type":"web_search_result","source":"Search source","title":"Search result title","url":"https://search.example/result","page_age":"1 day","encrypted_content":"ignored-secret"}
                ]},
                {"type":"web_fetch_tool_result","tool_use_id":"fetch_tool_1","content":{
                    "type":"web_fetch_result","url":"https://docs.example/page","retrieved_at":"2026-07-22T00:00:00Z","content":{
                        "type":"document","title":"Fetched document","source":{"type":"text","data":"Fetched body"}
                    }
                }},
                {"type":"bash_code_execution_tool_result","tool_use_id":"bash_tool_1","content":{
                    "type":"bash_code_execution_result","stdout":"command output","stderr":"command error","return_code":1,
                    "content":[{"type":"text","text":"additional output"}]
                }},
                {"type":"tool_result","tool_use_id":"toolu_1","content":[
                    {"type":"tool_reference","tool_name":"proxy_mcp__nia__manage_resource"}
                ]}
            ]}]
        }"#,
        );
        let segments = collect_claude_input_token_segments(&payload);
        for want in [
            "ws_tool_1",
            "Search source",
            "Search result title",
            "https://search.example/result",
            "1 day",
            "fetch_tool_1",
            "https://docs.example/page",
            "2026-07-22T00:00:00Z",
            "Fetched document",
            "Fetched body",
            "bash_tool_1",
            "command output",
            "command error",
            "1",
            "additional output",
            "toolu_1",
            "proxy_mcp__nia__manage_resource",
        ] {
            assert!(
                segments.iter().any(|s| s == want),
                "missing {want:?}: {segments:?}"
            );
        }
        assert!(
            !segments.iter().any(|s| s.contains("ignored-secret")),
            "{segments:?}"
        );
    }

    // port of TestCountClaudeInputTokensExcludesMultimediaAndControlFields (helps/claude_input_tokens_test.go)
    #[test]
    fn count_claude_input_tokens_excludes_multimedia_and_control_fields() {
        let base = parse(
            r#"{
            "system":"System text.",
            "messages":[{"role":"user","content":[{"type":"text","text":"User text."}]}],
            "tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"}}]
        }"#,
        );
        let with_excluded_fields = parse(
            r#"{
            "model":"claude-test",
            "system":"System text.",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"User text."},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"very-large-image-data"}},
                {"type":"input_audio","source":{"type":"base64","data":"very-large-audio-data"}},
                {"type":"video","source":{"type":"url","url":"https://example.com/video.mp4"}},
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"very-large-pdf-data"}}
            ]}],
            "tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],
            "metadata":{"large_wrapper":"ignored"},
            "max_tokens":8192,
            "temperature":0.8,
            "top_p":0.9,
            "thinking":{"type":"enabled","budget_tokens":4096},
            "stream":true
        }"#,
        );
        let base_count = count_claude_input_tokens_with(Encoding::O200kBase, &base);
        assert!(base_count > 0);
        assert_eq!(
            count_claude_input_tokens_with(Encoding::O200kBase, &with_excluded_fields),
            base_count
        );
    }

    // --- claude_executor_test.go ---

    // port of TestClaudeExecutor_CountTokensCountsLocallyWithoutUpstreamRequest (claude_executor_test.go)
    #[test]
    fn claude_counts_locally_without_upstream_request() {
        let payload = parse(
            r#"{
            "system":"client system instructions",
            "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]
        }"#,
        );
        const EXPECTED: i64 = 7;
        // The Go test points the credential at an httptest (http://127.0.0.1) server.
        for api_key in ["key-123", "sk-ant-oat-custom"] {
            assert!(!should_use_claude_upstream_token_count(
                api_key,
                "http://127.0.0.1:41234"
            ));
        }
        let count = count_locally(LocalCounter::Claude, "claude-sonnet-4-5", &payload).unwrap();
        assert_eq!(count, EXPECTED);
        let raw = local_usage_json(LocalCounter::Claude, count);
        let claude = client_count_response("claude", "claude", count, &raw);
        assert_eq!(gj_int(gj_get(&claude, "input_tokens")), EXPECTED);

        let gemini = client_count_response("claude", "gemini", count, &raw);
        assert_eq!(gj_int(gj_get(&gemini, "totalTokens")), EXPECTED);
        assert_eq!(
            gj_int(gj_get(&gemini, "promptTokensDetails.0.tokenCount")),
            EXPECTED
        );
    }

    // port of TestClaudeExecutor_CountTokensRejectsInvalidRequests (claude_executor_test.go);
    // the "invalid JSON" case is unrepresentable as a Value.
    #[test]
    fn claude_count_rejects_invalid_requests() {
        for (name, payload) in [
            ("non-object", r#"[]"#),
            ("missing messages", r#"{}"#),
            ("empty messages", r#"{"messages":[]}"#),
            ("non-array messages", r#"{"messages":"invalid"}"#),
            (
                "invalid role",
                r#"{"messages":[{"role":"system","content":"hello"}]}"#,
            ),
            (
                "invalid content",
                r#"{"messages":[{"role":"user","content":42}]}"#,
            ),
            (
                "non-object content block",
                r#"{"messages":[{"role":"user","content":[42]}]}"#,
            ),
            (
                "untyped content block",
                r#"{"messages":[{"role":"user","content":[{"text":"hello"}]}]}"#,
            ),
        ] {
            let result = count_locally(LocalCounter::Claude, "claude-sonnet-4-5", &parse(payload));
            assert!(
                matches!(result, Err(CountError::BadRequest(_))),
                "{name}: {result:?}"
            );
        }
    }

    // port of TestShouldUseClaudeUpstreamTokenCount (claude_executor_test.go)
    #[test]
    fn should_use_claude_upstream_token_count_matches_go() {
        for (name, api_key, base_url, want) in [
            (
                "official OAuth",
                "sk-ant-oat-official",
                "https://api.anthropic.com",
                true,
            ),
            (
                "official API key",
                "key-official",
                "https://api.anthropic.com:443",
                true,
            ),
            (
                "custom OAuth",
                "sk-ant-oat-custom",
                "https://gateway.example",
                false,
            ),
            (
                "custom API key",
                "key-custom",
                "https://gateway.example",
                false,
            ),
            (
                "lookalike host",
                "sk-ant-oat-lookalike",
                "https://api.anthropic.com.example",
                false,
            ),
            (
                "insecure official host",
                "sk-ant-oat-http",
                "http://api.anthropic.com",
                false,
            ),
            ("missing credential", "", "https://api.anthropic.com", false),
        ] {
            assert_eq!(
                should_use_claude_upstream_token_count(api_key, base_url),
                want,
                "{name}"
            );
        }
    }

    // port of TestClaudeCountTokensBetasForCredentialMatchesNativeOAuth220 (claude_executor_test.go)
    #[test]
    fn claude_count_tokens_betas_match_native_oauth_220() {
        let want = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01";
        assert_eq!(claude_count_tokens_betas_for_credential(true), want);
        let want_api_key = "claude-code-20250219,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01";
        assert_eq!(
            claude_count_tokens_betas_for_credential(false),
            want_api_key
        );
        assert_eq!(with_claude_count_tokens_oauth_beta(want_api_key), want);
    }

    // port of TestClaudeExecutor_DefaultCountTokensStillStripsAnthropicRejectedFields
    // (claude_fingerprint_policy_test.go): body half.
    #[test]
    fn claude_upstream_count_strips_anthropic_rejected_fields() {
        let payload = parse(
            r#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"hello"}],"metadata":{"user_id":"caller-user"},"context_management":{"edits":[]},"diagnostics":{"previous_message_id":null}}"#,
        );
        assert!(should_use_claude_upstream_token_count(
            "key-default-count",
            ""
        ));
        let req = upstream_count_request(RemoteCounter::Claude, "", "claude-opus-4-6", &payload);
        assert_eq!(
            req.url,
            "https://api.anthropic.com/v1/messages/count_tokens?beta=true"
        );
        assert_eq!(gj_array(gj_get(&req.body, "messages")).len(), 1);
        for field in ["metadata", "context_management", "diagnostics"] {
            assert!(gj_get(&req.body, field).is_none(), "{field} kept");
        }
        assert_eq!(
            req.extra_betas,
            vec!["token-counting-2024-11-01".to_string()]
        );
        // Answer {"input_tokens":11} → Claude client gets it verbatim.
        let answer = parse(r#"{"input_tokens":11}"#);
        let count = parse_upstream_count(RemoteCounter::Claude, &answer);
        assert_eq!(count, 11);
        assert_eq!(
            client_count_response("claude", "claude", count, &answer),
            answer
        );
    }

    // Body betas move to the header list (CountTokensCloakMatchesMeasuredDirectAnthropicShape
    // asserts `betas` is absent upstream; extractAndRemoveBetas).
    #[test]
    fn claude_upstream_count_lifts_body_betas() {
        let payload = parse(
            r#"{"model":"claude-opus-5(high)","betas":["a-beta"," b-beta ",""],"messages":[{"role":"user","content":"x"}]}"#,
        );
        let req = upstream_count_request(
            RemoteCounter::Claude,
            "https://api.anthropic.com",
            "claude-opus-5(high)",
            &payload,
        );
        assert!(gj_get(&req.body, "betas").is_none());
        assert_eq!(gj_string(gj_get(&req.body, "model")), "claude-opus-5");
        assert_eq!(
            req.extra_betas,
            vec!["a-beta", "b-beta", "token-counting-2024-11-01"]
        );
    }

    // --- gemini_executor_test.go ---

    // port of TestGeminiExecutorCountTokensPrependsLeadingUser (gemini_executor_test.go)
    #[test]
    fn gemini_count_prepends_leading_user() {
        let payload = parse(r#"{"contents":[{"role":"model","parts":[{"text":"prior output"}]}]}"#);
        let req = upstream_count_request(
            RemoteCounter::Gemini,
            "http://127.0.0.1:5555/",
            "gemini-3.7-flash",
            &payload,
        );
        assert_eq!(
            req.url,
            "http://127.0.0.1:5555/v1beta/models/gemini-3.7-flash:countTokens"
        );
        let contents = gj_array(gj_get(&req.body, "contents"));
        assert_eq!(contents.len(), 2);
        assert_eq!(gj_string(gj_get(contents[0], "role")), "user");
        assert_eq!(gj_string(gj_get(contents[1], "role")), "model");
        assert_eq!(gj_get(contents[0], "parts.0.text"), Some(&Value::from("")));
        assert_eq!(
            gj_string(gj_get(contents[1], "parts.0.text")),
            "prior output"
        );

        let answer = parse(r#"{"totalTokens":7}"#);
        assert_eq!(parse_upstream_count(RemoteCounter::Gemini, &answer), 7);
    }

    // GeminiExecutor.CountTokens deletions + model + default base.
    #[test]
    fn gemini_count_strips_generation_fields() {
        let payload = parse(
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{}],"generationConfig":{"temperature":1},"safetySettings":[],"systemInstruction":{"parts":[{"text":"s"}]}}"#,
        );
        let req = upstream_count_request(RemoteCounter::Gemini, "", "gemini-2.5-pro", &payload);
        assert_eq!(
            req.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:countTokens"
        );
        assert_eq!(
            req.body,
            parse(
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"systemInstruction":{"parts":[{"text":"s"}]},"model":"gemini-2.5-pro"}"#
            )
        );
        assert!(req.extra_betas.is_empty());
    }

    // --- xai_executor_test.go ---

    // port of TestCountXAIInputTokensExcludesRequestStructure (xai_executor_test.go)
    #[test]
    fn count_xai_input_tokens_excludes_request_structure() {
        let enc = Encoding::O200kBase;
        let semantic = parse(
            r#"{
            "instructions":"Follow the repository instructions.",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Review this implementation."}]},
                {"type":"function_call","name":"read_file","arguments":"{\"path\":\"main.go\"}"},
                {"type":"function_call_output","output":"package main"},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"I will inspect the file."}]}
            ],
            "tools":[{"type":"function","name":"read_file","description":"Reads a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],
            "text":{"format":{"name":"result","schema":{"type":"object"}}}
        }"#,
        );
        let structural = parse(
            r#"{
            "model":"grok-4.5", "stream":false, "reasoning":{"effort":"high"},
            "metadata":{"large_wrapper":"this metadata must not affect estimated input tokens"},
            "prompt_cache_key":"session-123", "max_output_tokens":4096,
            "instructions":"Follow the repository instructions.",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Review this implementation."}]},
                {"type":"function_call","name":"read_file","arguments":"{\"path\":\"main.go\"}"},
                {"type":"function_call_output","output":"package main"},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"I will inspect the file."}]}
            ],
            "tools":[{"type":"function","name":"read_file","description":"Reads a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],
            "text":{"format":{"name":"result","schema":{"type":"object"}}}
        }"#,
        );
        assert_eq!(
            count_xai_input_tokens(enc, &structural),
            count_xai_input_tokens(enc, &semantic)
        );

        for (name, body, expected) in [
            ("instructions", r#"{"instructions":"unique instruction text"}"#, "unique instruction text"),
            ("string input", r#"{"input":"unique input text"}"#, "unique input text"),
            (
                "message content",
                r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"unique message text"}]}]}"#,
                "unique message text",
            ),
            (
                "refusal",
                r#"{"input":[{"type":"message","content":[{"type":"refusal","refusal":"unique refusal text"}]}]}"#,
                "unique refusal text",
            ),
            (
                "input image",
                r#"{"input":[{"type":"message","content":[{"type":"input_image","image_url":"https://example.com/unique.png"}]}]}"#,
                "https://example.com/unique.png",
            ),
            (
                "input file",
                r#"{"input":[{"type":"message","content":[{"type":"input_file","file_data":"unique file data","filename":"unique.txt"}]}]}"#,
                "unique file data\nunique.txt",
            ),
            (
                "input audio",
                r#"{"input":[{"type":"message","content":[{"type":"input_audio","data":"unique audio data"}]}]}"#,
                "unique audio data",
            ),
            (
                "function call",
                r#"{"input":[{"type":"function_call","call_id":"call-1","name":"unique_function","arguments":"{\"value\":\"unique argument\"}"}]}"#,
                "unique_function\n{\"value\":\"unique argument\"}",
            ),
            (
                "function call output",
                r#"{"input":[{"type":"function_call_output","call_id":"call-1","output":"unique tool output"}]}"#,
                "unique tool output",
            ),
            (
                "reasoning summary",
                r#"{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"unique summary text"}]}]}"#,
                "unique summary text",
            ),
            (
                "function tool",
                r#"{"tools":[{"type":"function","name":"unique_tool","description":"unique tool description","parameters":{"type":"object","properties":{"value":{"type":"string"}}}}]}"#,
                "unique_tool\nunique tool description\n{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"string\"}}}",
            ),
            (
                "structured text format",
                r#"{"text":{"format":{"name":"unique_format","schema":{"type":"object","properties":{"value":{"type":"string"}}}}}}"#,
                "unique_format\n{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"string\"}}}",
            ),
        ] {
            assert_eq!(
                count_xai_input_tokens(enc, &parse(body)),
                enc.count(expected),
                "{name}"
            );
        }
    }

    // --- codex_executor_instructions_test.go ---

    // port of TestCodexExecutorCountTokensTreatsNullInstructionsAsEmpty (codex_executor_instructions_test.go)
    #[test]
    fn codex_count_treats_null_instructions_as_empty() {
        let null_body = parse(r#"{"model":"gpt-5.4","instructions":null,"input":"hello"}"#);
        let empty_body = parse(r#"{"model":"gpt-5.4","instructions":"","input":"hello"}"#);
        let null_count = count_locally(LocalCounter::Codex, "gpt-5.4", &null_body).unwrap();
        let empty_count = count_locally(LocalCounter::Codex, "gpt-5.4", &empty_body).unwrap();
        let null_resp = client_count_response(
            "codex",
            "openai-response",
            null_count,
            &local_usage_json(LocalCounter::Codex, null_count),
        );
        let empty_resp = client_count_response(
            "codex",
            "openai-response",
            empty_count,
            &local_usage_json(LocalCounter::Codex, empty_count),
        );
        assert_eq!(null_resp, empty_resp);
    }

    // --- sdk/translator/registry_bytes_test.go + init.go registrations ---

    // TestRegistryTranslateTokenCountReturnsBytes (registry_bytes_test.go) registers a
    // custom pair; here the static registrations: a registered pair answers with its
    // TokenCount translator, any other returns the raw payload.
    #[test]
    fn client_count_response_follows_registrations() {
        let raw = parse(r#"{"fallback":true}"#);
        let gemini = parse(
            r#"{"totalTokens":7,"promptTokensDetails":[{"modality":"TEXT","tokenCount":7}]}"#,
        );
        let claude = parse(r#"{"input_tokens":7}"#);
        for upstream in ["gemini", "claude", "codex", "openai"] {
            assert_eq!(
                client_count_response(upstream, "gemini", 7, &raw),
                gemini,
                "{upstream}"
            );
        }
        for upstream in ["gemini", "codex", "openai"] {
            assert_eq!(
                client_count_response(upstream, "claude", 7, &raw),
                claude,
                "{upstream}"
            );
        }
        // Claude→Claude has no TokenCount: the executor's payload passes through.
        assert_eq!(client_count_response("claude", "claude", 7, &raw), raw);
        assert_eq!(client_count_response("openai", "openai", 7, &raw), raw);
        assert_eq!(
            client_count_response("codex", "openai-response", 7, &raw),
            raw
        );
        // Exact bytes of GeminiTokenCountJSON / ClaudeInputTokensJSON (preserve_order).
        assert_eq!(
            gemini_token_count(7).to_string(),
            r#"{"totalTokens":7,"promptTokensDetails":[{"modality":"TEXT","tokenCount":7}]}"#
        );
        assert_eq!(claude_token_count(7).to_string(), r#"{"input_tokens":7}"#);
        assert_eq!(
            local_usage_json(LocalCounter::OpenAICompat, 7).to_string(),
            r#"{"usage":{"prompt_tokens":7,"completion_tokens":0,"total_tokens":7}}"#
        );
        assert_eq!(
            local_usage_json(LocalCounter::Xai, 7).to_string(),
            r#"{"response":{"usage":{"input_tokens":7,"output_tokens":0,"total_tokens":7}}}"#
        );
    }

    // --- tokenizer selection (tiktoken-go v0.8.1 tokenizer.ForModel) ---

    #[test]
    fn tokenizer_selection_matches_go() {
        for (model, codex, compat) in [
            ("", Encoding::Cl100kBase, Encoding::Cl100kBase),
            ("  GPT-5.4  ", Encoding::O200kBase, Encoding::O200kBase),
            ("gpt-4.1-mini", Encoding::O200kBase, Encoding::O200kBase),
            ("gpt-4o", Encoding::O200kBase, Encoding::O200kBase),
            ("gpt-4-turbo", Encoding::Cl100kBase, Encoding::Cl100kBase),
            ("gpt-3.5-turbo", Encoding::Cl100kBase, Encoding::Cl100kBase),
            ("o3-mini", Encoding::Cl100kBase, Encoding::O200kBase),
            ("o4-mini", Encoding::Cl100kBase, Encoding::O200kBase),
            (
                "codex-mini-latest",
                Encoding::Cl100kBase,
                Encoding::O200kBase,
            ),
            ("deepseek-chat", Encoding::Cl100kBase, Encoding::O200kBase),
        ] {
            assert_eq!(tokenizer_for_codex_model(model), codex, "codex {model:?}");
            assert_eq!(tokenizer_for_model(model), compat, "compat {model:?}");
        }
        assert_eq!(parse_suffix_model_name("gpt-5(high)"), "gpt-5");
        assert_eq!(parse_suffix_model_name("gpt-5(high"), "gpt-5(high");
    }

    // Counts cross-checked against tiktoken-go v0.8.1 (`Codec.Count`).
    #[test]
    fn encodings_match_tiktoken_go() {
        for (text, cl100k, o200k) in GO_REFERENCE_COUNTS {
            assert_eq!(Encoding::Cl100kBase.count(text), *cl100k, "cl100k {text:?}");
            assert_eq!(Encoding::O200kBase.count(text), *o200k, "o200k {text:?}");
        }
    }

    const GO_REFERENCE_COUNTS: &[(&str, i64, i64)] = &[
        ("client system instructions\nuser\nhello", 7, 7),
        ("Hello, world!", 4, 4),
        ("  leading and trailing  \n\n\tspaces   ", 8, 8),
        ("<|endoftext|> is plain text here <|fim_prefix|>", 17, 17),
        ("func main() {\n\tfmt.Println(\"héllo wörld\")\n}", 15, 14),
        ("中文分词测试，日本語のテキスト、한국어 텍스트", 23, 16),
        ("emoji 👩\u{200d}👩\u{200d}👧\u{200d}👦 and 🚀🔥 and combining é", 29, 20),
        ("{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}}}", 14, 14),
        ("numbers 1234567890 3.14159 -42 1e10", 17, 17),
        ("I'm can't won't they'll we've DON'T", 12, 6),
        ("unique_function\n{\"value\":\"unique argument\"}", 9, 9),
        ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 13, 13),
        ("Ünïcödé ÀÉÎÕÜ ß ﬁ ℕ 𝔘𝔫𝔦𝔠𝔬𝔡𝔢", 39, 37),
        ("line1\r\nline2\r\n\r\n   \n", 7, 7),
    ];

    // Collector shapes for Codex and OpenAI-compat (no Go test exists): each
    // segment the Go collector takes, joined with "\n".
    #[test]
    fn codex_collector_segments() {
        let body = parse(
            r#"{"instructions":" sys ","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"},{"type":"input_image","image_url":"x"}]},
                {"type":"message","role":"user","content":"string content is not counted"},
                {"type":"function_call","name":"f","arguments":"{\"a\":1}"},
                {"type":"function_call_output","output":"out"},
                {"type":"reasoning","text":"r"}
            ],"tools":[{"type":"function","name":"t","description":"d","parameters":{"type":"object"}}],
            "text":{"format":{"name":"fmt","schema":"{\"s\":1}"}}}"#,
        );
        let enc = Encoding::Cl100kBase;
        assert_eq!(
            count_codex_input_tokens(enc, &body),
            enc.count("sys\nhi\nf\n{\"a\":1}\nout\nr\nt\nd\n{\"type\":\"object\"}\nfmt\n{\"s\":1}")
        );
        assert_eq!(
            count_codex_input_tokens(enc, &parse(r#"{"input":"plain"}"#)),
            0
        );
    }

    #[test]
    fn openai_chat_collector_segments() {
        let body = parse(
            r#"{"messages":[
                {"role":"system","content":"S"},
                {"role":"user","name":"u","content":[{"type":"text","text":"T"},{"type":"image_url","image_url":{"url":"http://i"}},{"type":"input_audio","id":"aud"},{"foo":1}]},
                {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"fn","arguments":"{}"}}]}
            ],
            "tools":[{"type":"function","function":{"name":"fn","description":"D","parameters":{"type":"object"}}}],
            "tool_choice":"auto",
            "response_format":{"type":"json_schema","json_schema":{"name":"n"}}}"#,
        );
        let enc = Encoding::O200kBase;
        let expected = "system\nS\nuser\nu\nT\nhttp://i\naud\n{\"foo\":1}\nassistant\nc1\nfunction\nfn\n{}\nfunction\nfn\nD\n{\"type\":\"object\"}\nauto\njson_schema\n{\"name\":\"n\"}";
        assert_eq!(count_openai_chat_tokens(enc, &body), enc.count(expected));
        assert_eq!(
            count_locally(LocalCounter::OpenAICompat, "deepseek-chat", &body).unwrap(),
            enc.count(expected)
        );
    }
}
