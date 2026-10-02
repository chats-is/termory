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
}

impl BodyFixes {
    pub fn is_empty(&self) -> bool {
        *self == BodyFixes::default()
    }

    /// Apply every repair to an upstream body (any API's shape — each
    /// repair only touches the fields it names).
    pub fn apply(&self, body: &mut JsonValue) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
