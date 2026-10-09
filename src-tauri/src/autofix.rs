//! Request repairs the router applies after an upstream refuses a request
//! for something it can fix (magpie's gateway, `internal/gateway`): resend
//! the SAME member a corrected body instead of failing the request.
//!
//! - Foreign sealed reasoning — `foreignReasoning` (affinity.go): drop the
//!   Responses `reasoning` input items, then the `compaction` ones.
//! - Token floor — `tooFewTokens` / `withTokenFloor` (limit.go).
//! - Unknown optional field — `refusedOptional` / `optionalFields`
//!   (gateway.go): drop the fields the error names.
//! - Reasoning turned off refused — `effortLevelsNamed` (gateway.go):
//!   resend with `low`.
//! - Gemini built-in search beside function tools — NOT in magpie (Gemini is
//!   never its upstream); Termory's own repair, same shape: drop the
//!   built-in tools and resend.
//!
//! `is_shape_refusal` is magpie's `shapeWords` + `wrongEndpoint`: the member
//! does not take this request's shape, another may — next member, no rest.

use regex::Regex;
use serde_json::Value as JsonValue;
use std::sync::LazyLock;

static FOREIGN_REASONING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)invalid_encrypted_content|encrypted[ _]content.{0,80}could not be (verified|decrypted)|could not (decrypt|verify).{0,40}encrypted[ _]content").unwrap()
});
static TOO_FEW_TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)max_(?:completion_|output_)?tokens.{0,60}?(greater than|more than|larger than|at least|(?:>|\\u003e)=?)\s*(\d+)").unwrap()
});
static EFFORT_LEVELS_NAMED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\blow\b\W+(?:medium|high)\b").unwrap());
static BUILTIN_WITH_FUNCTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)built-in tools.{0,80}function calling.{0,40}cannot be combined").unwrap()
});
/// OpenAI's refusal of `max_tokens` on its reasoning models: "Unsupported
/// parameter: 'max_tokens' is not supported with this model. Use
/// 'max_completion_tokens' instead." — the body is resent with the field
/// renamed. Not in magpie (it never speaks Chat Completions to OpenAI).
static MAX_COMPLETION_TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)max_tokens.{0,120}max_completion_tokens|max_completion_tokens.{0,120}max_tokens"#,
    )
    .unwrap()
});
/// OpenAI's refusal of `reasoning` on a model without it ("Unsupported
/// parameter: 'reasoning.effort' is not supported with this model."): the
/// field is dropped and the body resent.
static UNSUPPORTED_REASONING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)unsupported parameter:?\s*'?(reasoning(\.effort)?|reasoning_effort)\b")
        .unwrap()
});
/// OpenAI's refusal of an effort level the model lacks ("Invalid value:
/// 'xhigh'. Supported values are: 'minimal', 'low', 'medium', and 'high'."):
/// the effort is lowered to the highest value named.
static EFFORT_SUPPORTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)invalid value:?\s*'(?:minimal|low|medium|high|xhigh|max)'.{0,160}?supported values are:?\s*([^.]+)").unwrap()
});
/// A Chat member refusing the `reasoning_content` the Responses→Chat
/// translator puts on assistant turns (DeepSeek's thinking mode requires it;
/// strict OpenAI-compatible servers reject the unknown field). Only a
/// REFUSAL matches — DeepSeek asking for a MISSING one must not strip it.
static REASONING_CONTENT_REFUSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(unknown|unrecognized|unexpected|extra|not permitted|not allowed|unsupported|additional propert).{0,80}?reasoning_content|reasoning_content.{0,80}?(unknown|unrecognized|unexpected|extra|not permitted|not allowed|unsupported)").unwrap()
});
const REASONING_CONTENT: &str = "reasoning_content";
/// A refusal of the effort VALUE `auto` ("Invalid value: 'auto'. Supported
/// values are: 'low', 'medium', and 'high'."). "Let the model decide" reaches
/// an OpenAI-shaped body as `"auto"` (Gemini CLI's default
/// `thinkingBudget: -1`, a Claude `thinking` enabled without a budget, a
/// `model(auto)` suffix); OpenAI rejects it, Kimi and others take it — so it
/// is sent, and left out only for a member that refused it.
static AUTO_EFFORT_REFUSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(invalid|unsupported|unknown|not supported|not a valid|not one of|must be one of|supported values)[^\n]{0,120}?\bauto\b|\bauto\b[^\n]{0,120}?(invalid|unsupported|not supported|not a valid|not one of|supported values)").unwrap()
});
const EFFORT_ORDER: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];
/// xAI's refusal of one argument it does not take ("Argument not supported:
/// external_web_access" — Codex's `web_search` tool carries it). Repaired
/// only when the named field sits on a TOOL: the tool is kept, the field
/// left out.
static ARGUMENT_NOT_SUPPORTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)argument not supported:?\s*['"`]?([A-Za-z_][A-Za-z0-9_]*)"#).unwrap()
});
/// new-api (the relay behind many gateways) refusing a model on THIS API
/// while it serves the model on another: `convert_request_failed`, or a bare
/// "not implemented" / "not available" message. Observed on one gateway:
/// deepseek and claude answer `/v1/messages` and `/v1/chat/completions` but
/// refuse `/v1/responses`; grok answers `/v1/responses` but refuses
/// `/v1/messages`.
static API_REFUSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"code"\s*:\s*"convert_request_failed"|"message"\s*:\s*"not (implemented|available)\b"#)
        .unwrap()
});
static SHAPE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)failed to deserialize|unknown (item |content |input )?(type|variant|field|parameter)|unknown_parameter|unrecognized (request argument|field|parameter)|extra (inputs|fields) are not permitted|additional properties are not allowed").unwrap()
});

/// magpie `optionalFields`.
const OPTIONAL_FIELDS: &[&str] = &[
    "store",
    "metadata",
    "service_tier",
    "prompt_cache_key",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
    "parallel_tool_calls",
    "verbosity",
    "thinking",
    "enable_thinking",
];

/// magpie `wrongEndpoint` phrases.
const WRONG_ENDPOINT: &[&str] = &[
    "not a chat model",
    "not supported in the v1/chat/completions",
    "not supported in /v1/chat/completions",
    "not supported in the v1/responses",
    "not supported in /v1/responses",
    "only supported in v1/responses",
    "only supported in /v1/responses",
    "use v1/completions",
    "use /v1/completions",
    "use v1/responses",
    "use /v1/responses",
    "use v1/chat/completions",
    "use /v1/chat/completions",
    "not accessible via the",
    "unsupported_api_for_model",
    "is not supported for format",
];

/// Gemini's built-in tools (a `Tool` object's keys besides
/// `functionDeclarations`).
const GEMINI_BUILTINS: &[&str] = &[
    "googleSearch",
    "google_search",
    "googleSearchRetrieval",
    "google_search_retrieval",
    "codeExecution",
    "code_execution",
    "urlContext",
    "url_context",
];

/// The repairs in force for one member's sends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BodyFixes {
    pub drop_reasoning: bool,
    pub drop_compaction: bool,
    pub drop_fields: Vec<String>,
    pub token_floor: Option<u64>,
    pub effort_low: bool,
    pub drop_builtin_tools: bool,
    /// `max_tokens` → `max_completion_tokens` (Chat Completions).
    pub rename_max_tokens: bool,
    /// The highest reasoning effort the member takes; anything above it
    /// is lowered to it.
    pub effort_cap: Option<String>,
    /// The member refused a reasoning effort of `"auto"`: an `"auto"`
    /// effort is left out (the model's own default). A real level is kept.
    pub drop_auto_effort: bool,
    /// The member refused Anthropic's prompt-cache markers: every
    /// `cache_control` is left out.
    pub drop_cache_control: bool,
    /// The member refused Codex's `namespace` tools (how Codex sends its MCP
    /// servers; xAI's Responses API takes no such type): they are left out,
    /// so the request goes through without the MCP tools.
    pub drop_namespace_tools: bool,
    /// Fields the member refused on a tool (xAI: Codex's `web_search`
    /// `external_web_access`): left out of every tool, the tools kept.
    pub drop_tool_fields: Vec<String>,
}

impl BodyFixes {
    pub fn is_empty(&self) -> bool {
        *self == BodyFixes::default()
    }

    /// Apply every repair to an upstream body (any API's shape — each
    /// repair only touches the fields it names).
    pub fn apply(&self, body: &mut JsonValue) {
        if self.drop_cache_control {
            fn strip(v: &mut JsonValue) {
                match v {
                    JsonValue::Object(o) => {
                        o.remove("cache_control");
                        o.values_mut().for_each(strip);
                    }
                    JsonValue::Array(a) => a.iter_mut().for_each(strip),
                    _ => {}
                }
            }
            strip(body);
        }
        let Some(o) = body.as_object_mut() else {
            return;
        };
        if self.drop_reasoning || self.drop_compaction {
            if let Some(items) = o.get_mut("input").and_then(|v| v.as_array_mut()) {
                items.retain(|it| {
                    let t = it.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    !(self.drop_reasoning && t == "reasoning"
                        || self.drop_compaction && (t == "compaction" || t == "compaction_summary"))
                });
            }
        }
        for f in &self.drop_fields {
            o.remove(f);
            // Lives on the assistant MESSAGES, not at the top level.
            if f == REASONING_CONTENT {
                if let Some(msgs) = o.get_mut("messages").and_then(|v| v.as_array_mut()) {
                    for m in msgs {
                        if let Some(mo) = m.as_object_mut() {
                            mo.remove(REASONING_CONTENT);
                        }
                    }
                }
            }
        }
        if self.rename_max_tokens {
            if let Some(v) = o.remove("max_tokens") {
                if !o.contains_key("max_completion_tokens") {
                    o.insert("max_completion_tokens".into(), v);
                }
            }
        }
        if let Some(floor) = self.token_floor {
            for k in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                raise(o, k, floor);
            }
            if let Some(gc) = o
                .get_mut("generationConfig")
                .and_then(|v| v.as_object_mut())
            {
                raise(gc, "maxOutputTokens", floor);
            }
        }
        if self.effort_low {
            let off = |v: &JsonValue| matches!(v.as_str(), Some("none") | Some("minimal"));
            if o.get("reasoning_effort").is_some_and(off) {
                o.insert("reasoning_effort".into(), "low".into());
            }
            for parent in ["reasoning", "output_config"] {
                if let Some(p) = o.get_mut(parent).and_then(|v| v.as_object_mut()) {
                    if p.get("effort").is_some_and(off) {
                        p.insert("effort".into(), "low".into());
                    }
                }
            }
        }
        if self.drop_auto_effort {
            let is_auto = |v: Option<&JsonValue>| {
                v.and_then(|v| v.as_str())
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("auto"))
            };
            if is_auto(o.get("reasoning_effort")) {
                o.remove("reasoning_effort");
            }
            let mut empty = false;
            if let Some(r) = o.get_mut("reasoning").and_then(|r| r.as_object_mut()) {
                if is_auto(r.get("effort")) {
                    r.remove("effort");
                }
                empty = r.is_empty();
            }
            if empty {
                o.remove("reasoning");
            }
        }
        if let Some(cap) = &self.effort_cap {
            let rank = |v: &str| EFFORT_ORDER.iter().position(|e| *e == v);
            let cap_rank = rank(cap);
            let lower = |v: &mut JsonValue| {
                if let (Some(cur), Some(c)) = (v.as_str().and_then(rank), cap_rank) {
                    if cur > c {
                        *v = JsonValue::from(cap.as_str());
                    }
                }
            };
            if let Some(v) = o.get_mut("reasoning_effort") {
                lower(v);
            }
            for parent in ["reasoning", "output_config"] {
                if let Some(v) = o
                    .get_mut(parent)
                    .and_then(|p| p.as_object_mut())
                    .and_then(|p| p.get_mut("effort"))
                {
                    lower(v);
                }
            }
        }
        if self.drop_namespace_tools {
            if let Some(tools) = o.get_mut("tools").and_then(|v| v.as_array_mut()) {
                tools.retain(|t| t.get("type").and_then(|v| v.as_str()) != Some("namespace"));
            }
        }
        if !self.drop_tool_fields.is_empty() {
            if let Some(tools) = o.get_mut("tools").and_then(|v| v.as_array_mut()) {
                for t in tools.iter_mut().filter_map(|t| t.as_object_mut()) {
                    for f in &self.drop_tool_fields {
                        t.remove(f);
                    }
                }
            }
        }
        if self.drop_builtin_tools {
            if let Some(tools) = o.get_mut("tools").and_then(|v| v.as_array_mut()) {
                for t in tools.iter_mut() {
                    if let Some(to) = t.as_object_mut() {
                        for k in GEMINI_BUILTINS {
                            to.remove(*k);
                        }
                    }
                }
                tools.retain(|t| t.as_object().is_none_or(|o| !o.is_empty()));
            }
        }
    }
}

fn raise(o: &mut serde_json::Map<String, JsonValue>, key: &str, floor: u64) {
    if o.get(key)
        .and_then(|v| v.as_u64())
        .is_some_and(|n| n < floor)
    {
        o.insert(key.into(), floor.into());
    }
}

/// magpie `tokenFloor`: the least the provider said it takes, or `None`.
fn token_floor(msg: &str) -> Option<u64> {
    let m = TOO_FEW_TOKENS.captures(msg)?;
    let n: u64 = m.get(2)?.as_str().parse().ok()?;
    if n > 1024 {
        return None;
    }
    let sign = m.get(1)?.as_str();
    Some(
        if sign.eq_ignore_ascii_case("at least") || sign.ends_with('=') {
            n
        } else {
            n + 1
        },
    )
}

fn bad_request(status: u16) -> bool {
    status == 400 || status == 422
}

/// The repairs after this refusal: `cur` plus the one the error calls for,
/// or `None` when no repair applies or it would not change the body that
/// was sent (`sent`, the upstream body with `cur` already applied).
/// `allow_fields`: magpie never drops optional fields for Anthropic.
pub fn next_fix(
    status: u16,
    text: &str,
    sent: &JsonValue,
    cur: &BodyFixes,
    allow_fields: bool,
) -> Option<BodyFixes> {
    let changes = |next: BodyFixes| -> Option<BodyFixes> {
        let mut b = sent.clone();
        next.apply(&mut b);
        (b != *sent).then_some(next)
    };
    if status >= 400 && FOREIGN_REASONING.is_match(text) {
        if !cur.drop_reasoning {
            if let Some(n) = changes(BodyFixes {
                drop_reasoning: true,
                ..cur.clone()
            }) {
                return Some(n);
            }
        }
        if !cur.drop_compaction {
            if let Some(n) = changes(BodyFixes {
                drop_reasoning: true,
                drop_compaction: true,
                ..cur.clone()
            }) {
                return Some(n);
            }
        }
    }
    if status == 400 && cur.token_floor.is_none() {
        if let Some(floor) = token_floor(text) {
            if let Some(n) = changes(BodyFixes {
                token_floor: Some(floor),
                ..cur.clone()
            }) {
                return Some(n);
            }
        }
    }
    if allow_fields && bad_request(status) {
        let named: Vec<String> = OPTIONAL_FIELDS
            .iter()
            .filter(|f| sent.get(**f).is_some())
            .filter(|f| {
                [
                    format!("\"{f}\""),
                    format!("\"{f}\\\""),
                    format!("'{f}'"),
                    format!("`{f}`"),
                ]
                .iter()
                .any(|q| text.contains(q.as_str()))
            })
            .map(|f| f.to_string())
            .collect();
        if !named.is_empty() {
            let mut next = cur.clone();
            next.drop_fields.extend(named);
            if let Some(n) = changes(next) {
                return Some(n);
            }
        }
    }
    if bad_request(status)
        && !cur.drop_fields.iter().any(|f| f == REASONING_CONTENT)
        && REASONING_CONTENT_REFUSED.is_match(text)
    {
        let mut next = cur.clone();
        next.drop_fields.push(REASONING_CONTENT.to_string());
        if let Some(n) = changes(next) {
            return Some(n);
        }
    }
    if bad_request(status) && UNSUPPORTED_REASONING.is_match(text) {
        let mut next = cur.clone();
        for f in ["reasoning", "reasoning_effort"] {
            if sent.get(f).is_some() && !next.drop_fields.iter().any(|d| d == f) {
                next.drop_fields.push(f.to_string());
            }
        }
        if let Some(n) = changes(next) {
            return Some(n);
        }
    }
    // An Anthropic-compatible endpoint refusing the prompt-cache markers
    // the router adds to a translated request ("cache_control: Extra inputs
    // are not permitted"): resent without any.
    if bad_request(status) && !cur.drop_cache_control && text.contains("cache_control") {
        if let Some(n) = changes(BodyFixes {
            drop_cache_control: true,
            ..cur.clone()
        }) {
            return Some(n);
        }
    }
    // "tools[7].type: unknown variant `namespace`, expected one of
    // `function`, …" (xAI, reached through a gateway or an API key): resent
    // without Codex's `namespace` tools.
    if bad_request(status) && !cur.drop_namespace_tools && text.contains("namespace") {
        if let Some(n) = changes(BodyFixes {
            drop_namespace_tools: true,
            ..cur.clone()
        }) {
            return Some(n);
        }
    }
    if bad_request(status) {
        if let Some(field) = ARGUMENT_NOT_SUPPORTED
            .captures(text)
            .and_then(|m| m.get(1))
            .map(|m| m.as_str().to_string())
        {
            // `type` / `name` identify the tool: never stripped.
            let on_a_tool = !matches!(field.as_str(), "type" | "name")
                && sent
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .is_some_and(|ts| ts.iter().any(|t| t.get(&field).is_some()));
            if on_a_tool && !cur.drop_tool_fields.contains(&field) {
                let mut next = cur.clone();
                next.drop_tool_fields.push(field);
                if let Some(n) = changes(next) {
                    return Some(n);
                }
            }
        }
    }
    if bad_request(status)
        && !cur.drop_auto_effort
        && text.to_ascii_lowercase().contains("effort")
        && AUTO_EFFORT_REFUSED.is_match(text)
    {
        if let Some(n) = changes(BodyFixes {
            drop_auto_effort: true,
            ..cur.clone()
        }) {
            return Some(n);
        }
    }
    if bad_request(status) && cur.effort_cap.is_none() {
        if let Some(m) = EFFORT_SUPPORTED.captures(text) {
            let listed = m
                .get(1)
                .map(|g| g.as_str().to_ascii_lowercase())
                .unwrap_or_default();
            let cap = EFFORT_ORDER
                .iter()
                .rev()
                .find(|e| {
                    listed.contains(&format!("'{e}'")) || listed.contains(&format!("\"{e}\""))
                })
                .map(|e| e.to_string());
            if let Some(cap) = cap {
                if let Some(n) = changes(BodyFixes {
                    effort_cap: Some(cap),
                    ..cur.clone()
                }) {
                    return Some(n);
                }
            }
        }
    }
    if bad_request(status) && !cur.rename_max_tokens && MAX_COMPLETION_TOKENS.is_match(text) {
        if let Some(n) = changes(BodyFixes {
            rename_max_tokens: true,
            ..cur.clone()
        }) {
            return Some(n);
        }
    }
    if status == 400 && !cur.effort_low && EFFORT_LEVELS_NAMED.is_match(text) {
        if let Some(n) = changes(BodyFixes {
            effort_low: true,
            ..cur.clone()
        }) {
            return Some(n);
        }
    }
    if bad_request(status) && !cur.drop_builtin_tools && BUILTIN_WITH_FUNCTIONS.is_match(text) {
        if let Some(n) = changes(BodyFixes {
            drop_builtin_tools: true,
            ..cur.clone()
        }) {
            return Some(n);
        }
    }
    None
}

/// magpie `shapeWords` / `wrongEndpoint`: this member does not take the
/// request's shape or endpoint, which says nothing about the others.
pub fn is_shape_refusal(status: u16, text: &str) -> bool {
    if !(400..500).contains(&status) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    WRONG_ENDPOINT.iter().any(|p| lower.contains(p))
        || (bad_request(status) && SHAPE_WORDS.is_match(text))
}

/// The member does not serve this MODEL over this API, though it may over
/// another: new-api's refusals (any status — it answers 500) and the
/// wrong-endpoint phrases. The router tries the member's next API before
/// giving up on it; nothing about the model's availability is learned.
pub fn is_api_refusal(status: u16, text: &str) -> bool {
    if status < 400 {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    API_REFUSED.is_match(text) || WRONG_ENDPOINT.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_refused_tool_argument_is_dropped_from_the_tools_only() {
        let sent = json!({
            "external_web_access": 1,
            "tools": [
                {"type": "function", "name": "shell"},
                {"type": "web_search", "external_web_access": true}
            ]
        });
        let err = r#"{"error":{"message":"Argument not supported: external_web_access","type":"bad_response_status_code","param":"","code":"bad_response_status_code"}}"#;
        let f = next_fix(400, err, &sent, &BodyFixes::default(), true).unwrap();
        assert_eq!(f.drop_tool_fields, vec!["external_web_access".to_string()]);
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(
            b,
            json!({
                "external_web_access": 1,
                "tools": [
                    {"type": "function", "name": "shell"},
                    {"type": "web_search"}
                ]
            })
        );
        // Already applied: nothing more to repair.
        assert!(next_fix(400, err, &b, &f, true).is_none());
    }

    #[test]
    fn a_refused_argument_not_on_a_tool_is_not_repaired() {
        let sent = json!({"logprobs": true, "tools": [{"type": "function", "name": "shell"}]});
        let err = "Argument not supported: logprobs";
        assert!(next_fix(400, err, &sent, &BodyFixes::default(), true).is_none());
        let err = "Argument not supported: type";
        assert!(next_fix(400, err, &sent, &BodyFixes::default(), true).is_none());
    }

    #[test]
    fn new_api_refusals_of_an_api_are_recognised() {
        // Verbatim from a new-api gateway.
        assert!(is_api_refusal(
            500,
            r#"{"error":{"message":"not implemented (request id: 2026100901)","type":"new_api_error","param":"","code":"convert_request_failed"}}"#
        ));
        assert!(is_api_refusal(
            500,
            r#"{"error":{"type":"new_api_error","message":"not available (request id: 2026100901)"},"type":"error"}"#
        ));
        assert!(is_api_refusal(
            400,
            "This model is only supported in v1/responses"
        ));
        // An outage or a missing model is not an API refusal.
        assert!(!is_api_refusal(
            500,
            r#"{"error":{"message":"internal error"}}"#
        ));
        assert!(!is_api_refusal(
            503,
            r#"{"error":{"message":"service temporarily not available"}}"#
        ));
        assert!(!is_api_refusal(200, r#"{"message":"not implemented"}"#));
    }

    #[test]
    fn foreign_reasoning_drops_reasoning_then_compaction() {
        let sent = json!({"input": [
            {"type": "reasoning", "encrypted_content": "x"},
            {"type": "compaction", "encrypted_content": "y"},
            {"type": "message", "role": "user", "content": "hi"}
        ]});
        let err = "The encrypted content for item rs_1 could not be verified.";
        let f1 = next_fix(400, err, &sent, &BodyFixes::default(), true).unwrap();
        assert!(f1.drop_reasoning && !f1.drop_compaction);
        let mut b = sent.clone();
        f1.apply(&mut b);
        assert_eq!(b["input"].as_array().unwrap().len(), 2);
        let f2 = next_fix(400, err, &b, &f1, true).unwrap();
        assert!(f2.drop_compaction);
        let mut c = b.clone();
        f2.apply(&mut c);
        assert_eq!(
            c["input"],
            json!([{"type": "message", "role": "user", "content": "hi"}])
        );
        assert_eq!(next_fix(400, err, &c, &f2, true), None);
    }

    // OpenAI's own wording for `max_tokens` on a reasoning model: the field
    // is renamed and the member resent — not cooled down, not skipped.
    #[test]
    fn max_tokens_is_renamed_when_the_error_names_max_completion_tokens() {
        let sent = json!({"model": "o3", "max_tokens": 4096, "messages": []});
        let err = r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","param":"max_tokens","code":"unsupported_parameter"}}"#;
        let f = next_fix(400, err, &sent, &BodyFixes::default(), true).unwrap();
        assert!(f.rename_max_tokens);
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(
            b,
            json!({"model": "o3", "max_completion_tokens": 4096, "messages": []})
        );
        // Already renamed: nothing more to do.
        assert_eq!(next_fix(400, err, &b, &f, true), None);
        // A body without the field is not resent.
        let no_field = json!({"model": "o3", "messages": []});
        assert_eq!(
            next_fix(400, err, &no_field, &BodyFixes::default(), true),
            None
        );
    }

    // OpenAI's own wording for `reasoning` on a model without it (gpt-4.1)
    // and for an effort level the model lacks (xhigh on gpt-5): the field
    // is dropped / lowered and the member resent instead of the 400 reaching
    // the client on every turn.
    #[test]
    fn openai_reasoning_refusals_are_repaired() {
        let sent = json!({"model": "gpt-4.1", "input": "hi", "reasoning": {"effort": "medium"}});
        let err = r#"{"error":{"message":"Unsupported parameter: 'reasoning.effort' is not supported with this model.","type":"invalid_request_error","param":"reasoning.effort","code":"unsupported_parameter"}}"#;
        let f = next_fix(400, err, &sent, &BodyFixes::default(), true).unwrap();
        assert_eq!(f.drop_fields, vec!["reasoning".to_string()]);
        let mut b = sent.clone();
        f.apply(&mut b);
        assert!(b.get("reasoning").is_none());

        let chat = json!({"model": "gpt-4.1", "messages": [], "reasoning_effort": "medium"});
        let err = r#"{"error":{"message":"Unsupported parameter: 'reasoning_effort' is not supported with this model.","type":"invalid_request_error","param":"reasoning_effort","code":"unsupported_parameter"}}"#;
        let f = next_fix(400, err, &chat, &BodyFixes::default(), true).unwrap();
        assert_eq!(f.drop_fields, vec!["reasoning_effort".to_string()]);

        let sent = json!({"model": "gpt-5", "input": "hi", "reasoning": {"effort": "xhigh"}});
        let err = r#"{"error":{"message":"Invalid value: 'xhigh'. Supported values are: 'minimal', 'low', 'medium', and 'high'.","type":"invalid_request_error","param":"reasoning.effort","code":"invalid_value"}}"#;
        let f = next_fix(400, err, &sent, &BodyFixes::default(), true).unwrap();
        assert_eq!(f.effort_cap.as_deref(), Some("high"));
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(b["reasoning"]["effort"], json!("high"));
        // Already at or under the cap: nothing more to do.
        assert_eq!(next_fix(400, err, &b, &f, true), None);
    }

    // A strict Chat server refusing the placeholder `reasoning_content` on
    // assistant turns: stripped from the messages and resent. DeepSeek
    // asking for a MISSING one is not a refusal and strips nothing.
    #[test]
    fn refused_reasoning_content_is_stripped_from_messages() {
        let sent = json!({"model": "gpt-4.1", "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [], "reasoning_content": "[reasoning unavailable]"},
            {"role": "tool", "tool_call_id": "c", "content": "ok"}
        ]});
        let err = r#"{"error":{"message":"Additional properties are not allowed ('reasoning_content' was unexpected)","type":"invalid_request_error"}}"#;
        let f = next_fix(400, err, &sent, &BodyFixes::default(), true).unwrap();
        let mut b = sent.clone();
        f.apply(&mut b);
        assert!(b["messages"][1].get("reasoning_content").is_none());
        assert_eq!(b["messages"][1]["role"], json!("assistant"));
        assert_eq!(next_fix(400, err, &b, &f, true), None);
        let unknown = r#"{"error":{"message":"Unrecognized request argument supplied: messages.1.reasoning_content"}}"#;
        assert!(next_fix(400, unknown, &sent, &BodyFixes::default(), true).is_some());

        let missing = r#"{"error":{"message":"Missing `reasoning_content` field in the assistant message at message index 1.","type":"invalid_request_error"}}"#;
        assert_eq!(
            next_fix(400, missing, &sent, &BodyFixes::default(), true),
            None
        );
    }

    // An "auto" effort refused by the member (OpenAI's wording): left out
    // and resent. A real level is never touched, and an "auto" elsewhere
    // (tool_choice) is not read as the effort.
    #[test]
    fn a_refused_auto_effort_is_left_out() {
        let err = r#"{"error":{"message":"Invalid value: 'auto'. Supported values are: 'low', 'medium', and 'high'.","type":"invalid_request_error","param":"reasoning_effort","code":"invalid_value"}}"#;
        let chat = json!({"model": "gpt-5", "messages": [], "reasoning_effort": "auto"});
        let f = next_fix(400, err, &chat, &BodyFixes::default(), true).unwrap();
        assert!(f.drop_auto_effort);
        let mut b = chat.clone();
        f.apply(&mut b);
        assert!(b.get("reasoning_effort").is_none());
        assert_eq!(next_fix(400, err, &b, &f, true), None);

        let resp = json!({"model": "gpt-5", "input": "hi", "reasoning": {"effort": "auto", "summary": "auto"}});
        let mut b = resp.clone();
        f.apply(&mut b);
        assert_eq!(b["reasoning"], json!({"summary": "auto"}));
        let mut only = json!({"input": "hi", "reasoning": {"effort": "auto"}});
        f.apply(&mut only);
        assert!(only.get("reasoning").is_none());

        // A real level stays, even with the fix in force.
        let mut real = json!({"messages": [], "reasoning_effort": "high"});
        f.apply(&mut real);
        assert_eq!(real["reasoning_effort"], json!("high"));

        // "auto" refused for tool_choice is not the effort.
        let tc = r#"{"error":{"message":"Invalid value: 'auto' for tool_choice.","param":"tool_choice"}}"#;
        assert_eq!(next_fix(400, tc, &chat, &BodyFixes::default(), true), None);
    }

    #[test]
    fn refused_cache_markers_are_stripped_everywhere() {
        let sent = json!({"system": [{"type": "text", "text": "r", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]}]});
        let err = r#"{"type":"error","error":{"type":"invalid_request_error","message":"system.0.cache_control: Extra inputs are not permitted"}}"#;
        // Anthropic bodies never get the optional-field repairs, but this
        // one applies to them.
        let f = next_fix(400, err, &sent, &BodyFixes::default(), false).unwrap();
        assert!(f.drop_cache_control);
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(
            b,
            json!({"system": [{"type": "text", "text": "r"}], "tools": [{"name": "a"}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]})
        );
        assert_eq!(next_fix(400, err, &b, &f, false), None);
    }

    #[test]
    fn refused_namespace_tools_are_left_out() {
        // xAI's 422, verbatim, for a Codex request carrying an MCP server.
        let sent = json!({"model": "grok-4.5", "tools": [
            {"type": "function", "name": "shell"},
            {"type": "namespace", "name": "mcp__docs", "tools": []},
            {"type": "web_search"}]});
        let err = "Failed to deserialize the JSON body into the target type: tools[1].type: unknown variant `namespace`, expected one of `function`, `web_search`, `x_search`, `image_generation`, `collections_search`, `file_search`, `code_execution`, `code_interpreter`, `mcp`, `shell`, `tool_search`";
        let f = next_fix(422, err, &sent, &BodyFixes::default(), true).unwrap();
        assert!(f.drop_namespace_tools);
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(
            b,
            json!({"model": "grok-4.5", "tools": [
                {"type": "function", "name": "shell"}, {"type": "web_search"}]})
        );
        assert_eq!(next_fix(422, err, &b, &f, true), None);
        // No namespace tool sent: nothing to repair.
        let plain = json!({"model": "m", "tools": [{"type": "function", "name": "a"}]});
        assert_eq!(
            next_fix(422, err, &plain, &BodyFixes::default(), true),
            None
        );
    }

    #[test]
    fn token_floor_raises_every_max_field_once() {
        let sent = json!({"max_tokens": 1, "generationConfig": {"maxOutputTokens": 1}});
        let f = next_fix(
            400,
            r#"{"error":"max_tokens must be greater than 2"}"#,
            &sent,
            &BodyFixes::default(),
            true,
        )
        .unwrap();
        assert_eq!(f.token_floor, Some(3));
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(
            b,
            json!({"max_tokens": 3, "generationConfig": {"maxOutputTokens": 3}})
        );
        assert_eq!(
            token_floor("Expected max_output_tokens \\u003e= 16"),
            Some(16)
        );
        assert_eq!(token_floor("max_tokens: at least 2048"), None);
    }

    #[test]
    fn named_optional_fields_are_dropped() {
        let sent = json!({"model": "m", "store": false, "metadata": {}});
        let f = next_fix(
            400,
            r#"Unknown name "store": Cannot find field."#,
            &sent,
            &BodyFixes::default(),
            true,
        )
        .unwrap();
        assert_eq!(f.drop_fields, vec!["store".to_string()]);
        assert_eq!(
            next_fix(
                400,
                r#"Unknown name "store""#,
                &sent,
                &BodyFixes::default(),
                false
            ),
            None
        );
    }

    #[test]
    fn effort_none_becomes_low() {
        let sent = json!({"reasoning": {"effort": "none"}});
        let f = next_fix(
            400,
            r#"expected one of "low"|"medium"|"high""#,
            &sent,
            &BodyFixes::default(),
            true,
        )
        .unwrap();
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(b, json!({"reasoning": {"effort": "low"}}));
    }

    #[test]
    fn gemini_builtin_search_is_dropped_beside_functions() {
        let sent = json!({"tools": [
            {"functionDeclarations": [{"name": "f"}]},
            {"googleSearch": {}}
        ]});
        let f = next_fix(
            400,
            "Built-in tools (google_search) and Function Calling cannot be combined in the same request.",
            &sent,
            &BodyFixes::default(),
            true,
        )
        .unwrap();
        let mut b = sent.clone();
        f.apply(&mut b);
        assert_eq!(
            b,
            json!({"tools": [{"functionDeclarations": [{"name": "f"}]}]})
        );
    }

    #[test]
    fn shape_refusals() {
        assert!(is_shape_refusal(
            400,
            "Model x is not supported for format anthropic"
        ));
        assert!(is_shape_refusal(400, "unknown field `foo`"));
        assert!(!is_shape_refusal(400, "context_length_exceeded"));
        assert!(!is_shape_refusal(500, "unknown field"));
    }
}
