//! API-format translation for the local router — the CLIProxyAPI model
//! (`internal/translator/<target>/<source>`, ported per pair): a client may
//! speak any supported API and use any model; when the upstream serving that
//! model speaks a different API, the request is translated into it and the
//! response (stream or not) back into the client's.
//!
//! Each pair module exposes the same three entry points:
//! `translate_request`, `StreamTranslator { new, push, finish }`,
//! `translate_non_stream`. This module only dispatches on `(client, upstream)`.

pub mod anthropic_to_chat;
pub mod anthropic_to_gemini;
pub mod anthropic_to_responses;
pub mod chat_to_anthropic;
pub mod chat_to_gemini;
pub mod chat_to_responses;
pub mod gemini_to_anthropic;
pub mod gemini_to_chat;
pub mod gemini_to_responses;
pub mod responses_to_anthropic;
pub mod responses_to_chat;
pub mod responses_to_codex;
pub mod responses_to_gemini;

use crate::router::Protocol;
use serde_json::Value;

/// The upstream formats a client format can be TRANSLATED into, in order of
/// preference (used only when the upstream does not speak the client's own
/// format — a native match is always passed through untranslated).
pub fn targets_for(client: Protocol) -> &'static [Protocol] {
    match client {
        Protocol::Anthropic => &[
            Protocol::OpenaiResponses,
            Protocol::OpenaiChat,
            Protocol::Gemini,
        ],
        Protocol::OpenaiChat => &[
            Protocol::OpenaiResponses,
            Protocol::Anthropic,
            Protocol::Gemini,
        ],
        Protocol::OpenaiResponses => &[Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini],
        Protocol::Gemini => &[
            Protocol::OpenaiResponses,
            Protocol::OpenaiChat,
            Protocol::Anthropic,
        ],
    }
}

/// The request path an upstream format is sent to (the router joins it to
/// the upstream's base the same way it joins a client path).
pub fn upstream_path(upstream: Protocol) -> &'static str {
    match upstream {
        Protocol::Anthropic => "/v1/messages",
        Protocol::OpenaiResponses => "/v1/responses",
        Protocol::OpenaiChat => "/v1/chat/completions",
        Protocol::Gemini => "/v1beta",
    }
}

/// Client body → upstream body; `None` when the pair has no translator.
pub fn request(
    client: Protocol,
    upstream: Protocol,
    model: &str,
    body: &Value,
    stream: bool,
) -> Option<Value> {
    use Protocol::*;
    let mut out = match (client, upstream) {
        (Anthropic, OpenaiResponses) => {
            anthropic_to_responses::translate_request(model, body, stream)
        }
        (OpenaiChat, OpenaiResponses) => chat_to_responses::translate_request(model, body, stream),
        // The COMPAT form (CLIProxyAPI `ConvertClaudeRequestToOpenAIWithCompat`):
        // a Chat upstream here is a third-party API (DeepSeek & co.), whose
        // thinking mode needs the assistant's `reasoning_content` echoed back
        // on tool turns — the plain form drops unsigned thinking.
        (Anthropic, OpenaiChat) => {
            anthropic_to_chat::translate_request_with_compat(model, body, stream)
        }
        (OpenaiResponses, OpenaiChat) => responses_to_chat::translate_request(model, body, stream),
        (Gemini, OpenaiResponses) => gemini_to_responses::translate_request(model, body, stream),
        (Gemini, OpenaiChat) => gemini_to_chat::translate_request(model, body, stream),
        (Gemini, Anthropic) => gemini_to_anthropic::translate_request(model, body, stream),
        (Anthropic, Gemini) => anthropic_to_gemini::translate_request(model, body, stream),
        (OpenaiChat, Gemini) => chat_to_gemini::translate_request(model, body, stream),
        (OpenaiChat, Anthropic) => chat_to_anthropic::translate_request(model, body, stream),
        (OpenaiResponses, Gemini) => responses_to_gemini::translate_request(model, body, stream),
        (OpenaiResponses, Anthropic) => {
            responses_to_anthropic::translate_request(model, body, stream)
        }
        _ => return None,
    };
    // A request TRANSLATED into Claude's API carries no cache markers of
    // its own (Codex, OpenCode and Gemini CLI never write Anthropic's), so
    // Claude re-reads the whole system prompt and tool list at the full
    // input price every turn. A Claude client's own request is left as it
    // is — Claude Code places its own markers.
    if upstream == Anthropic && client != Anthropic {
        add_claude_cache_breakpoints(&mut out);
    }
    // A streamed Chat upstream sends usage only when asked; without it the
    // client's final usage reads 0/0.
    if upstream == OpenaiChat && stream {
        if let Some(obj) = out.as_object_mut() {
            let opts = obj
                .entry("stream_options")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(o) = opts.as_object_mut() {
                o.entry("include_usage").or_insert(Value::Bool(true));
            }
        }
    }
    Some(out)
}

/// A complete upstream response → the client's response shape.
pub fn non_stream(
    client: Protocol,
    upstream: Protocol,
    upstream_body: &Value,
    original_request: &Value,
) -> Option<Value> {
    use Protocol::*;
    Some(match (client, upstream) {
        (Anthropic, OpenaiResponses) => {
            anthropic_to_responses::translate_non_stream(upstream_body, original_request)
        }
        (OpenaiChat, OpenaiResponses) => {
            chat_to_responses::translate_non_stream(upstream_body, original_request)
        }
        (Anthropic, OpenaiChat) => {
            anthropic_to_chat::translate_non_stream(upstream_body, original_request)
        }
        (OpenaiResponses, OpenaiChat) => {
            responses_to_chat::translate_non_stream(upstream_body, original_request)
        }
        (Gemini, OpenaiResponses) => {
            gemini_to_responses::translate_non_stream(upstream_body, original_request)
        }
        (Gemini, OpenaiChat) => {
            gemini_to_chat::translate_non_stream(upstream_body, original_request)
        }
        (Gemini, Anthropic) => {
            gemini_to_anthropic::translate_non_stream(upstream_body, original_request)
        }
        (Anthropic, Gemini) => {
            anthropic_to_gemini::translate_non_stream(upstream_body, original_request)
        }
        (OpenaiChat, Gemini) => {
            chat_to_gemini::translate_non_stream(upstream_body, original_request)
        }
        (OpenaiChat, Anthropic) => {
            chat_to_anthropic::translate_non_stream(upstream_body, original_request)
        }
        (OpenaiResponses, Gemini) => {
            responses_to_gemini::translate_non_stream(upstream_body, original_request)
        }
        (OpenaiResponses, Anthropic) => {
            responses_to_anthropic::translate_non_stream(upstream_body, original_request)
        }
        _ => return None,
    })
}

/// Per-response stream state for whichever pair is in use.
enum PairTx {
    AnthropicFromResponses(anthropic_to_responses::StreamTranslator),
    ChatFromResponses(chat_to_responses::StreamTranslator),
    AnthropicFromChat(anthropic_to_chat::StreamTranslator),
    ResponsesFromChat(responses_to_chat::StreamTranslator),
    GeminiFromResponses(gemini_to_responses::StreamTranslator),
    GeminiFromChat(gemini_to_chat::StreamTranslator),
    GeminiFromAnthropic(gemini_to_anthropic::StreamTranslator),
    AnthropicFromGemini(anthropic_to_gemini::StreamTranslator),
    ChatFromGemini(chat_to_gemini::StreamTranslator),
    ChatFromAnthropic(chat_to_anthropic::StreamTranslator),
    ResponsesFromAnthropic(responses_to_anthropic::StreamTranslator),
    ResponsesFromGemini(responses_to_gemini::StreamTranslator),
}

/// Per-response stream state: the pair's translator plus whether the
/// upstream has reached a TERMINAL event. A stream that ends without one
/// (connection dropped, upstream crashed mid-answer) is reported to the
/// client as an error — never closed as if the answer were complete, which
/// would hand Claude Code a truncated tool call to run, or show half an
/// answer as the whole one.
pub struct StreamTx {
    pair: PairTx,
    client: Protocol,
    upstream: Protocol,
    terminal: bool,
    /// The cut error went out: nothing follows it.
    cut: bool,
}

impl StreamTx {
    pub fn new(client: Protocol, upstream: Protocol, original_request: &Value) -> Option<Self> {
        Some(StreamTx {
            pair: PairTx::new(client, upstream, original_request)?,
            client,
            upstream,
            terminal: false,
            cut: false,
        })
    }

    /// One upstream SSE event, translated into client frames.
    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
        if is_terminal_event(self.upstream, event, data) {
            self.terminal = true;
        }
        self.pair.push(event, data)
    }

    /// The upstream sent its `[DONE]` marker: the stream ended on purpose.
    pub fn done(&mut self) -> Vec<String> {
        if self.cut {
            return Vec::new();
        }
        self.terminal = true;
        self.pair.finish()
    }

    /// The upstream stream ended (its terminal event was seen, or `[DONE]`,
    /// or a relay already reported the failure): the pair's closing frames.
    pub fn finish(&mut self) -> Vec<String> {
        if self.cut {
            return Vec::new();
        }
        self.terminal = true;
        self.pair.finish()
    }

    /// The upstream connection ended (EOF). After a terminal event this is
    /// the normal close; before one the answer was CUT, and the client gets
    /// an error in its own shape instead of closing frames.
    pub fn eof(&mut self) -> Vec<String> {
        if self.cut {
            return Vec::new();
        }
        if self.terminal {
            return self.pair.finish();
        }
        self.cut = true;
        cut_error_frames(self.client)
    }
}

/// Whether an upstream event ends the response (success, failure or an
/// in-stream error), per upstream API.
fn is_terminal_event(upstream: Protocol, event: Option<&str>, data: &Value) -> bool {
    let kind = data
        .get("type")
        .and_then(|t| t.as_str())
        .or(event)
        .unwrap_or("");
    let has_error = data.get("error").is_some_and(|e| e.is_object());
    match upstream {
        Protocol::OpenaiResponses => matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed" | "error"
        ),
        Protocol::Anthropic => matches!(kind, "message_stop" | "error"),
        Protocol::OpenaiChat => {
            has_error
                || data
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .is_some_and(|choices| {
                        choices.iter().any(|c| {
                            c.get("finish_reason")
                                .and_then(|f| f.as_str())
                                .is_some_and(|f| !f.is_empty())
                        })
                    })
        }
        Protocol::Gemini => {
            has_error
                || data
                    .get("candidates")
                    .and_then(|c| c.as_array())
                    .is_some_and(|cands| {
                        cands.iter().any(|c| {
                            c.get("finishReason")
                                .and_then(|f| f.as_str())
                                .is_some_and(|f| !f.is_empty())
                        })
                    })
        }
    }
}

const CUT_MESSAGE: &str = "the upstream stream ended before the response completed";

/// The error a client is told when its answer was cut, in that API's own
/// in-stream error shape.
fn cut_error_frames(client: Protocol) -> Vec<String> {
    match client {
        Protocol::Anthropic => vec![format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"type": "error", "error": {"type": "api_error", "message": CUT_MESSAGE}})
        )],
        Protocol::OpenaiResponses => vec![format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"type": "error", "code": "server_error", "message": CUT_MESSAGE, "param": null})
        )],
        Protocol::OpenaiChat => vec![format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"message": CUT_MESSAGE, "type": "server_error", "code": null}})
        )],
        Protocol::Gemini => vec![format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"code": 500, "message": CUT_MESSAGE, "status": "INTERNAL"}})
        )],
    }
}

impl PairTx {
    fn new(client: Protocol, upstream: Protocol, original_request: &Value) -> Option<Self> {
        use Protocol::*;
        Some(match (client, upstream) {
            (Anthropic, OpenaiResponses) => Self::AnthropicFromResponses(
                anthropic_to_responses::StreamTranslator::new(original_request),
            ),
            (OpenaiChat, OpenaiResponses) => {
                Self::ChatFromResponses(chat_to_responses::StreamTranslator::new(original_request))
            }
            (Anthropic, OpenaiChat) => {
                Self::AnthropicFromChat(anthropic_to_chat::StreamTranslator::new(original_request))
            }
            (OpenaiResponses, OpenaiChat) => {
                Self::ResponsesFromChat(responses_to_chat::StreamTranslator::new(original_request))
            }
            (Gemini, OpenaiResponses) => Self::GeminiFromResponses(
                gemini_to_responses::StreamTranslator::new(original_request),
            ),
            (Gemini, OpenaiChat) => {
                Self::GeminiFromChat(gemini_to_chat::StreamTranslator::new(original_request))
            }
            (Gemini, Anthropic) => Self::GeminiFromAnthropic(
                gemini_to_anthropic::StreamTranslator::new(original_request),
            ),
            (Anthropic, Gemini) => Self::AnthropicFromGemini(
                anthropic_to_gemini::StreamTranslator::new(original_request),
            ),
            (OpenaiChat, Gemini) => {
                Self::ChatFromGemini(chat_to_gemini::StreamTranslator::new(original_request))
            }
            (OpenaiChat, Anthropic) => {
                Self::ChatFromAnthropic(chat_to_anthropic::StreamTranslator::new(original_request))
            }
            (OpenaiResponses, Anthropic) => Self::ResponsesFromAnthropic(
                responses_to_anthropic::StreamTranslator::new(original_request),
            ),
            (OpenaiResponses, Gemini) => Self::ResponsesFromGemini(
                responses_to_gemini::StreamTranslator::new(original_request),
            ),
            _ => return None,
        })
    }

    fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
        match self {
            Self::AnthropicFromResponses(t) => t.push(event, data),
            Self::ChatFromResponses(t) => t.push(event, data),
            Self::AnthropicFromChat(t) => t.push(event, data),
            Self::ResponsesFromChat(t) => t.push(event, data),
            Self::GeminiFromResponses(t) => t.push(event, data),
            Self::GeminiFromChat(t) => t.push(event, data),
            Self::GeminiFromAnthropic(t) => t.push(event, data),
            Self::AnthropicFromGemini(t) => t.push(event, data),
            Self::ChatFromGemini(t) => t.push(event, data),
            Self::ChatFromAnthropic(t) => t.push(event, data),
            Self::ResponsesFromAnthropic(t) => t.push(event, data),
            Self::ResponsesFromGemini(t) => t.push(event, data),
        }
    }

    fn finish(&mut self) -> Vec<String> {
        match self {
            Self::AnthropicFromResponses(t) => t.finish(),
            Self::ChatFromResponses(t) => t.finish(),
            Self::AnthropicFromChat(t) => t.finish(),
            Self::ResponsesFromChat(t) => t.finish(),
            Self::GeminiFromResponses(t) => t.finish(),
            Self::GeminiFromChat(t) => t.finish(),
            Self::GeminiFromAnthropic(t) => t.finish(),
            Self::AnthropicFromGemini(t) => t.finish(),
            Self::ChatFromGemini(t) => t.finish(),
            Self::ChatFromAnthropic(t) => t.finish(),
            Self::ResponsesFromAnthropic(t) => t.finish(),
            Self::ResponsesFromGemini(t) => t.finish(),
        }
    }
}

// ───────────────────────── Claude prompt-cache breakpoints ─────────────────────────
//
// Explicit block markers, the form Claude Code itself sends — so every
// Anthropic-compatible endpoint that serves Claude Code accepts them (the
// request-level automatic form is not available everywhere). Two markers of
// the four allowed: the end of the stable prefix (system, else tools — tools
// render first, so a system marker covers both) and the last block of the
// last turn, which each following turn of the conversation reads back. A
// prefix below the model's minimum simply is not cached; no error.

/// Content blocks a marker may sit on.
fn cacheable_block(block: &Value) -> bool {
    match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => block
            .get("text")
            .and_then(|t| t.as_str())
            .is_some_and(|t| !t.is_empty()),
        Some("image" | "tool_use" | "tool_result" | "document") => true,
        _ => false,
    }
}

fn has_cache_control(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.contains_key("cache_control") || o.values().any(has_cache_control),
        Value::Array(a) => a.iter().any(has_cache_control),
        _ => false,
    }
}

/// Mark the last cacheable block of `content` (a string becomes one text
/// block); `false` when there is none.
fn mark_last_block(content: &mut Value) -> bool {
    if let Value::String(s) = content {
        if s.is_empty() {
            return false;
        }
        *content = serde_json::json!([{
            "type": "text", "text": s.clone(), "cache_control": {"type": "ephemeral"}
        }]);
        return true;
    }
    let Some(blocks) = content.as_array_mut() else {
        return false;
    };
    for block in blocks.iter_mut().rev() {
        if cacheable_block(block) {
            if let Some(o) = block.as_object_mut() {
                o.insert(
                    "cache_control".into(),
                    serde_json::json!({"type": "ephemeral"}),
                );
                return true;
            }
        }
    }
    false
}

/// Cache breakpoints on a request translated into Claude's API. A body that
/// already carries any marker (the client placed its own) is left alone.
pub fn add_claude_cache_breakpoints(body: &mut Value) {
    if has_cache_control(body) {
        return;
    }
    let Some(o) = body.as_object_mut() else {
        return;
    };
    if !o.get_mut("system").is_some_and(mark_last_block) {
        if let Some(Value::Array(tools)) = o.get_mut("tools") {
            if let Some(Value::Object(last)) = tools.last_mut() {
                last.insert(
                    "cache_control".into(),
                    serde_json::json!({"type": "ephemeral"}),
                );
            }
        }
    }
    if let Some(Value::Array(msgs)) = o.get_mut("messages") {
        for msg in msgs.iter_mut().rev() {
            if msg.get_mut("content").is_some_and(mark_last_block) {
                break;
            }
        }
    }
}

// ───────────────────────── Claude request surface by model id ─────────────────────────
//
// Termory has no model registry (CLIProxyAPI's `registry.LookupModelInfo`
// is what the three "→ Claude" translators ported against), so what a Claude
// model accepts is decided from its id. The rules come from Anthropic's
// migration notes: from Opus 4.7, Sonnet 5 and the Fable/Mythos tier on,
// `thinking: {type: "enabled", budget_tokens}` and the sampling parameters
// (`temperature`, `top_p`, `top_k`) return 400 — thinking is `adaptive` with
// `output_config.effort`. Opus/Sonnet 4.6, Haiku 4.5 and older still take
// the budget form. An id that is not a Claude model keeps the budget form
// too (a relay serving another vendor under the Anthropic API).

/// `(family, major.minor)` of a Claude id — `claude-opus-4-7`,
/// `anthropic/claude-sonnet-5-5`, `us.anthropic.claude-haiku-4-5-20251001-v1:0`,
/// `claude-fable-5-1`. `None` for anything else (including the old
/// `claude-3-7-sonnet` order).
fn claude_family_version(model: &str) -> Option<(String, f64)> {
    let lower = model.trim().to_ascii_lowercase();
    let start = lower.find("claude-")?;
    let rest = &lower[start + "claude-".len()..];
    let mut parts = rest.split(|c| c == '-' || c == ':' || c == '@' || c == '/');
    let family = parts.next()?.to_string();
    if !["opus", "sonnet", "haiku", "fable", "mythos"].contains(&family.as_str()) {
        return None;
    }
    let major: u32 = parts.next()?.parse().ok()?;
    // A short second number is the minor version; a date (8 digits) or a
    // word is not.
    let minor: u32 = parts
        .next()
        .filter(|p| p.len() <= 2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Some((family, major as f64 + minor as f64 / 10.0))
}

/// Whether this Claude model takes ONLY adaptive thinking (and rejects
/// sampling parameters).
pub fn claude_adaptive_only(model: &str) -> bool {
    match claude_family_version(model) {
        Some((family, _)) if family == "fable" || family == "mythos" => true,
        Some((family, v)) if family == "opus" => v >= 4.7,
        Some((family, v)) if family == "sonnet" || family == "haiku" => v >= 5.0,
        _ => false,
    }
}

/// The thinking support the "→ Claude" translators read in place of the
/// registry: `(min_budget, effort levels)` — a non-empty level list means
/// adaptive thinking; `None` keeps the manual budget form.
pub fn claude_thinking_support(model: &str) -> Option<(i64, Vec<String>)> {
    claude_adaptive_only(model).then(|| {
        (
            1024,
            ["low", "medium", "high", "xhigh", "max"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    })
}

/// Manual thinking needs `budget_tokens < max_tokens` (an `xhigh` effort
/// maps to 32768 against the default 32000): cap the budget under it.
pub fn clamp_claude_budget(out: &mut Value) {
    let max_tokens = out.get("max_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let budget = out
        .pointer("/thinking/budget_tokens")
        .and_then(|v| v.as_i64());
    if let (Some(b), true) = (budget, max_tokens > 1) {
        if b >= max_tokens {
            if let Some(t) = out.get_mut("thinking").and_then(|t| t.as_object_mut()) {
                t.insert("budget_tokens".into(), Value::from(max_tokens - 1));
            }
        }
    }
}

/// One parsed upstream SSE event.
#[derive(Debug, PartialEq)]
pub enum SseEvent {
    /// `event:` field (if any) + the JSON of its `data:` payload.
    Data(Option<String>, Value),
    /// `data: [DONE]` — the OpenAI end-of-stream sentinel.
    Done,
}

/// Incremental SSE parser for an upstream byte stream: events may split
/// across chunks, lines may end in CRLF, and a payload that is not JSON
/// (comments, keep-alives) is skipped.
#[derive(Default)]
pub struct SseParser {
    /// Raw BYTES not yet part of a complete event. Decoding happens per
    /// complete block only: a chunk boundary can split a multibyte UTF-8
    /// character (or a `\r\n` pair), and decoding chunk by chunk turns the
    /// halves into U+FFFD (or hides the blank line that ends an event).
    buf: Vec<u8>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = blank_line_end(&self.buf) {
            let block: Vec<u8> = self.buf.drain(..end).collect();
            out.extend(parse_block(&decode_block(&block)));
        }
        out
    }

    /// Whatever is left when the stream ends without a trailing blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let rest = std::mem::take(&mut self.buf);
        parse_block(&decode_block(&rest)).into_iter().collect()
    }
}

/// The end (exclusive) of the first event in `buf`: just past the first
/// EMPTY line, where a line ends at `\n` and may carry a trailing `\r`.
/// `\n` never occurs inside a multibyte UTF-8 sequence, so a block cut
/// there is always whole UTF-8.
fn blank_line_end(buf: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        let line = &buf[line_start..i];
        if line.is_empty() || line == b"\r" {
            return Some(i + 1);
        }
        line_start = i + 1;
    }
    None
}

fn decode_block(block: &[u8]) -> String {
    String::from_utf8_lossy(block).replace("\r\n", "\n")
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    if data.is_empty() {
        return None;
    }
    let payload = data.join("\n");
    if payload.trim() == "[DONE]" {
        return Some(SseEvent::Done);
    }
    serde_json::from_str(&payload)
        .ok()
        .map(|v| SseEvent::Data(event, v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sse_parser_keeps_multibyte_characters_split_across_chunks() {
        let event = "data: {\"t\":\"你好😀\"}\n\n".as_bytes();
        // Split inside "你" and inside the emoji.
        let a = 10;
        let b = event.len() - 6;
        let mut p = SseParser::default();
        let mut got = p.feed(&event[..a]);
        got.extend(p.feed(&event[a..b]));
        got.extend(p.feed(&event[b..]));
        assert_eq!(got, vec![SseEvent::Data(None, json!({"t": "你好😀"}))]);
    }

    #[test]
    fn sse_parser_finds_the_boundary_of_a_crlf_split_across_chunks() {
        let mut p = SseParser::default();
        let mut got = p.feed(b"data: {\"a\":1}\r\n\r");
        got.extend(p.feed(b"\ndata: [DONE]\r\n\r\n"));
        assert_eq!(
            got,
            vec![SseEvent::Data(None, json!({"a":1})), SseEvent::Done]
        );
    }

    #[test]
    fn sse_parser_handles_split_chunks_crlf_and_done() {
        let mut p = SseParser::default();
        assert!(p
            .feed(b"event: response.created\r\ndata: {\"a\"")
            .is_empty());
        let ev = p.feed(b":1}\r\n\r\ndata: [DONE]\n\n: keep-alive\n\n");
        assert_eq!(
            ev,
            vec![
                SseEvent::Data(Some("response.created".into()), json!({"a":1})),
                SseEvent::Done
            ]
        );
        assert!(p.feed(b"data: {\"b\":2}").is_empty());
        assert_eq!(p.finish(), vec![SseEvent::Data(None, json!({"b":2}))]);
    }

    // A Responses provider's connection drops after a partial tool call:
    // Claude Code gets an `error` event, not a finished `tool_use` whose
    // input is a truncated command.
    #[test]
    fn eof_before_the_terminal_event_is_an_error_not_a_complete_message() {
        let req = json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}], "stream": true});
        let mut tx = StreamTx::new(Protocol::Anthropic, Protocol::OpenaiResponses, &req).unwrap();
        let resp = json!({"id": "resp_5", "model": "gpt-5", "status": "in_progress", "output": []});
        tx.push(
            Some("response.created"),
            &json!({"type": "response.created", "response": resp}),
        );
        tx.push(Some("response.output_item.added"), &json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_5", "type": "function_call", "arguments": "", "call_id": "call_cut", "name": "Bash"}}));
        tx.push(Some("response.function_call_arguments.delta"), &json!({"type": "response.function_call_arguments.delta",
            "item_id": "fc_5", "output_index": 0, "delta": "{\"command\":\"rm -rf ./build && make"}));
        let frames = tx.eof().join("");
        assert_eq!(
            frames,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"the upstream stream ended before the response completed\"}}\n\n"
        );
        // A second EOF emits nothing more.
        assert!(tx.eof().is_empty());
    }

    // After the terminal event, EOF is the normal close (here: the Chat
    // `finish_reason` arrived, `[DONE]` did not).
    #[test]
    fn eof_after_the_terminal_event_closes_normally() {
        let req = json!({"model": "deepseek-chat", "messages": [{"role": "user", "content": "hi"}], "stream": true});
        let mut tx = StreamTx::new(Protocol::Anthropic, Protocol::OpenaiChat, &req).unwrap();
        tx.push(None, &json!({"id": "c1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hello"}, "finish_reason": null}]}));
        tx.push(
            None,
            &json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        );
        let frames = tx.eof().join("");
        assert!(frames.contains("event: message_stop"), "{frames}");
        assert!(!frames.contains("event: error"), "{frames}");
    }

    // Every client API gets the cut reported in its own shape.
    #[test]
    fn cut_error_frames_per_client() {
        let mut chat =
            StreamTx::new(Protocol::OpenaiChat, Protocol::Anthropic, &json!({})).unwrap();
        assert_eq!(
            chat.eof(),
            vec!["data: {\"error\":{\"message\":\"the upstream stream ended before the response completed\",\"type\":\"server_error\",\"code\":null}}\n\n"]
        );
        let mut gem = StreamTx::new(Protocol::Gemini, Protocol::OpenaiChat, &json!({})).unwrap();
        assert_eq!(
            gem.eof(),
            vec!["data: {\"error\":{\"code\":500,\"message\":\"the upstream stream ended before the response completed\",\"status\":\"INTERNAL\"}}\n\n"]
        );
        let mut resp =
            StreamTx::new(Protocol::OpenaiResponses, Protocol::OpenaiChat, &json!({})).unwrap();
        assert_eq!(
            resp.eof(),
            vec!["event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"the upstream stream ended before the response completed\",\"param\":null}\n\n"]
        );
        // `[DONE]` is a deliberate end: no error.
        let mut done =
            StreamTx::new(Protocol::Anthropic, Protocol::OpenaiChat, &json!({})).unwrap();
        done.done();
        assert!(done.eof().is_empty());
    }

    #[test]
    fn terminal_events_per_upstream() {
        use Protocol::*;
        assert!(is_terminal_event(
            OpenaiResponses,
            None,
            &json!({"type": "response.failed"})
        ));
        assert!(!is_terminal_event(
            OpenaiResponses,
            None,
            &json!({"type": "response.output_text.delta"})
        ));
        assert!(is_terminal_event(
            Anthropic,
            Some("message_stop"),
            &json!({"type": "message_stop"})
        ));
        assert!(is_terminal_event(
            OpenaiChat,
            None,
            &json!({"error": {"message": "x"}})
        ));
        assert!(!is_terminal_event(
            OpenaiChat,
            None,
            &json!({"choices": [{"delta": {}, "finish_reason": null}]})
        ));
        assert!(is_terminal_event(
            Gemini,
            None,
            &json!({"candidates": [{"finishReason": "STOP"}]})
        ));
        assert!(!is_terminal_event(
            Gemini,
            None,
            &json!({"candidates": [{"content": {"parts": []}}]})
        ));
    }

    #[test]
    fn cache_breakpoints_on_a_translated_claude_request() {
        let mut b = json!({
            "system": [{"type": "text", "text": "rules"}],
            "tools": [{"name": "a", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "a", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}
            ]
        });
        add_claude_cache_breakpoints(&mut b);
        assert_eq!(
            b["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // The system marker covers the tools (they render first).
        assert!(b["tools"][0].get("cache_control").is_none());
        assert_eq!(
            b["messages"][2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(b["messages"][0].get("cache_control").is_none());

        // A string system and a string last turn become marked blocks; no
        // system: the last tool carries the prefix marker.
        let mut b = json!({"system": "rules", "messages": [{"role": "user", "content": "hi"}]});
        add_claude_cache_breakpoints(&mut b);
        assert_eq!(
            b["system"],
            json!([{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(
            b["messages"][0]["content"],
            json!([{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}])
        );
        let mut b = json!({"tools": [{"name": "a"}, {"name": "b"}], "messages": []});
        add_claude_cache_breakpoints(&mut b);
        assert_eq!(b["tools"][1]["cache_control"], json!({"type": "ephemeral"}));

        // Thinking blocks and empty text are never marked: the marker walks
        // back to the last block that can carry one.
        let mut b = json!({"messages": [{"role": "assistant", "content": [
            {"type": "text", "text": "answer"},
            {"type": "thinking", "thinking": "", "signature": "s"},
            {"type": "text", "text": ""}
        ]}]});
        add_claude_cache_breakpoints(&mut b);
        assert_eq!(
            b["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(b["messages"][0]["content"][1]
            .get("cache_control")
            .is_none());
        assert!(b["messages"][0]["content"][2]
            .get("cache_control")
            .is_none());

        // The client placed its own: untouched.
        let own = json!({"system": [{"type": "text", "text": "r"}], "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}]});
        let mut b = own.clone();
        add_claude_cache_breakpoints(&mut b);
        assert_eq!(b, own);
    }

    #[test]
    fn only_requests_translated_into_claude_get_markers() {
        let codex = json!({"model": "claude-sonnet-4-6", "instructions": "You are Codex.",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]});
        let out = request(
            Protocol::OpenaiResponses,
            Protocol::Anthropic,
            "claude-sonnet-4-6",
            &codex,
            true,
        )
        .unwrap();
        assert!(has_cache_control(&out), "{out}");
        let chat = request(
            Protocol::OpenaiResponses,
            Protocol::OpenaiChat,
            "gpt-5",
            &codex,
            true,
        )
        .unwrap();
        assert!(!has_cache_control(&chat), "{chat}");
    }

    #[test]
    fn claude_models_adaptive_only_by_id() {
        for m in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "anthropic/claude-sonnet-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "us.anthropic.claude-opus-5-5-v1:0",
        ] {
            assert!(claude_adaptive_only(m), "{m}");
            assert!(claude_thinking_support(m).is_some(), "{m}");
        }
        for m in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            "claude-3-7-sonnet-20250219",
            "deepseek-reasoner",
            "gpt-5",
        ] {
            assert!(!claude_adaptive_only(m), "{m}");
            assert!(claude_thinking_support(m).is_none(), "{m}");
        }
    }

    #[test]
    fn budget_is_clamped_under_max_tokens() {
        let mut out =
            json!({"max_tokens": 32000, "thinking": {"type": "enabled", "budget_tokens": 32768}});
        clamp_claude_budget(&mut out);
        assert_eq!(out["thinking"]["budget_tokens"], json!(31999));
        let mut ok =
            json!({"max_tokens": 32000, "thinking": {"type": "enabled", "budget_tokens": 8192}});
        clamp_claude_budget(&mut ok);
        assert_eq!(ok["thinking"]["budget_tokens"], json!(8192));
    }

    #[test]
    fn every_target_has_a_translator() {
        for client in [
            Protocol::Anthropic,
            Protocol::OpenaiChat,
            Protocol::OpenaiResponses,
            Protocol::Gemini,
        ] {
            for &up in targets_for(client) {
                assert!(
                    StreamTx::new(client, up, &json!({})).is_some(),
                    "{client:?}->{up:?}"
                );
            }
        }
    }
}
