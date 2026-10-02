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
pub enum StreamTx {
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

impl StreamTx {
    pub fn new(client: Protocol, upstream: Protocol, original_request: &Value) -> Option<Self> {
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

    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
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

    pub fn finish(&mut self) -> Vec<String> {
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
