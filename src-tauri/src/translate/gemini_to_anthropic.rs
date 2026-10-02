//! Gemini generateContent (client) ⇄ Anthropic Messages (upstream) translator.
//!
//! Faithful port of CLIProxyAPI `internal/translator/claude/gemini/` at commit
//! `ed980be` (`claude_gemini_request.go`, `claude_gemini_response.go`), plus the
//! helpers they call in other packages (`translator/common`, `util`, `thinking`,
//! `signature`). Each ported function carries a `// port of <GoFunc> (<file>)`
//! comment so the two can be diffed when upstream moves.
//!
//! The Go code works on raw bytes through gjson/sjson; this port works on
//! `serde_json::Value`. The gjson coercion rules the Go code relies on
//! (`.String()` / `.Int()` / `.Float()` / `.Bool()` on any JSON type,
//! `.Exists()` being true for an explicit `null`) are reproduced by the `g*`
//! helpers below so edge cases behave the same.
//!
//! Deliberately NOT ported: the `model(level)` thinking-suffix parsing, the
//! count_tokens response (`GeminiTokenCount`), metrics/logging and config hooks.
//! Termory has no model registry, so `registry.LookupModelInfo` is a stand-in
//! that knows no model (the same choice `thinking.rs` makes): every Claude model
//! takes the manual (`enabled` + `budget_tokens`) thinking branch.
//!
//! Deviations from the Go code, each deliberate:
//! - `lowercaseClaudeToolSchemaTypes` leaves an object/array value under a
//!   `"type"` key alone (that is a schema PROPERTY named `type`); Go would
//!   stringify it and corrupt the schema.
//! - A tool call's accumulated `partial_json` that does not parse is emitted
//!   as a JSON string; Go splices it in raw and emits an invalid frame.
//! - `translate_non_stream` takes a complete Messages object (replayed as the
//!   events Anthropic would have streamed), and the model name comes from the
//!   original request's `model` or else the upstream's, since Gemini carries it
//!   in the URL.

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// gjson / sjson-compatible accessors
// ---------------------------------------------------------------------------

/// gjson path lookup (`a.b.0.c`): object keys, numeric segments index arrays.
fn gget<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
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

/// gjson `Result.String()`: strings verbatim, scalars as their literal, null /
/// missing as "", objects and arrays as their JSON text.
fn gstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// gjson `Result.Int()`.
fn gint(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().map(|f| f as i64).unwrap_or(0)),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// gjson `Result.Float()`.
fn gfloat(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        Some(Value::Bool(true)) => 1.0,
        _ => 0.0,
    }
}

/// gjson `Result.Bool()`: true, a non-zero number, or a string
/// `strconv.ParseBool` accepts after lower-casing.
fn gbool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "1" | "t" | "true"),
        _ => false,
    }
}

/// sjson writes a float64 with `strconv.FormatFloat(f, 'f', -1, 64)`, so an
/// integral value goes out as `1`, not `1.0`.
fn float_value(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

/// sjson `SetBytes` on a dotted object path: missing (or non-object)
/// intermediates become objects, an existing key keeps its position, a new
/// key is appended.
fn sj_set(root: &mut Value, path: &str, value: Value) {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let map = cur.as_object_mut().expect("object");
        if i == segs.len() - 1 {
            map.insert((*seg).to_string(), value);
            return;
        }
        cur = map
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// sjson `DeleteBytes` on a dotted object path (no-op when absent).
fn sj_delete(root: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (Some(p), l),
        None => (None, path),
    };
    let target = match parent {
        Some(p) => {
            let mut cur = &mut *root;
            for seg in p.split('.') {
                cur = match cur.get_mut(seg) {
                    Some(v) => v,
                    None => return,
                };
            }
            cur
        }
        None => root,
    };
    if let Some(m) = target.as_object_mut() {
        m.shift_remove(last);
    }
}

/// `sjson.SetRawBytes(x, "candidates.0.content.parts.-1", part)`.
fn append_candidate_part(template: &mut Value, part: Value) {
    if let Some(Value::Array(parts)) = template
        .get_mut("candidates")
        .and_then(|c| c.get_mut(0))
        .and_then(|c| c.get_mut("content"))
        .and_then(|c| c.get_mut("parts"))
    {
        parts.push(part);
    }
}

/// `sjson.SetBytes(x, "candidates.0.finishReason", reason)`.
fn set_finish_reason(template: &mut Value, reason: &str) {
    if let Some(c) = template.get_mut("candidates").and_then(|c| c.get_mut(0)) {
        sj_set(c, "finishReason", Value::from(reason));
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `time.Unix(secs, 0).Format(time.RFC3339Nano)` — Go's `time.Unix` is in the
/// LOCAL zone, and RFC3339Nano drops a zero fraction and writes `Z` for UTC.
fn format_create_time(secs: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(secs, 0).single() {
        Some(t) => t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        None => String::new(),
    }
}

fn sha256_hex(input: &str) -> String {
    let sum = Sha256::digest(input.as_bytes());
    sum.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// translator/common
// ---------------------------------------------------------------------------

// port of IsGeminiThoughtPart (translator/common/gemini.go)
fn is_gemini_thought_part(part: &Value) -> bool {
    gbool(part.get("thought"))
}

// port of DeriveClaudeUserID (translator/common/claude_user_id.go)
fn derive_claude_user_id(root: &Value) -> String {
    if let Some(Value::String(raw)) = gget(root, "metadata.user_id") {
        if !raw.trim().is_empty() {
            return raw.clone();
        }
    }
    if let Some(Value::String(raw)) = root.get("user") {
        if !raw.trim().is_empty() {
            return raw.clone();
        }
    }

    let mut seed = String::new();

    if let Some(v) = root.get("prompt_cache_key") {
        let value = gstr(Some(v)).trim().to_string();
        if !value.is_empty() {
            seed.push_str("prompt_cache_key:");
            seed.push_str(&value);
        }
    }

    if seed.is_empty() {
        for path in ["session_id", "sessionId"] {
            if let Some(v) = root.get(path) {
                let value = gstr(Some(v)).trim().to_string();
                if !value.is_empty() {
                    seed.push_str("session_id:");
                    seed.push_str(&value);
                    break;
                }
            }
        }
    }

    if seed.is_empty() {
        let conversation = root.get("conversation");
        let sid = gstr(conversation.and_then(|c| c.get("id")))
            .trim()
            .to_string();
        if !sid.is_empty() {
            seed.push_str("conversation_id:");
            seed.push_str(&sid);
        } else if let Some(Value::String(s)) = conversation {
            let sid = s.trim();
            if !sid.is_empty() {
                seed.push_str("conversation_id:");
                seed.push_str(sid);
            }
        } else if let Some(v) = root.get("conversation_id") {
            let sid = gstr(Some(v)).trim().to_string();
            if !sid.is_empty() {
                seed.push_str("conversation_id:");
                seed.push_str(&sid);
            }
        }
    }

    if seed.is_empty() {
        let content = first_stable_request_content(root);
        if !content.is_empty() {
            seed.push_str("content:");
            seed.push_str(&content);
        }
    }

    if seed.is_empty() {
        if let Some(v) = root.get("model") {
            let value = gstr(Some(v)).trim().to_string();
            if !value.is_empty() {
                seed.push_str("model:");
                seed.push_str(&value);
            }
        }
        for key in [
            "instructions",
            "system",
            "systemInstruction",
            "system_instruction",
        ] {
            if let Some(v) = root.get(key) {
                seed.push(';');
                seed.push_str(key);
                seed.push(':');
                seed.push_str(&gstr(Some(v)));
            }
        }
    }

    if seed.is_empty() {
        return "unknown".to_string();
    }
    sha256_hex(&seed)
}

// port of firstStableRequestContent (translator/common/claude_user_id.go)
fn first_stable_request_content(root: &Value) -> String {
    if let Some(Value::Array(messages)) = root.get("messages") {
        for message in messages {
            let role = gstr(message.get("role")).trim().to_lowercase();
            if role == "user" {
                let content = extract_text_content(message.get("content"));
                if !content.is_empty() {
                    return content;
                }
            }
        }
    }

    if let Some(input) = root.get("input") {
        match input {
            Value::String(s) => {
                let text = s.trim();
                if !text.is_empty() {
                    return text.to_string();
                }
            }
            Value::Array(items) => {
                for item in items {
                    if is_responses_user_item(item) {
                        let content = extract_responses_item_text(item.get("content"));
                        if !content.is_empty() {
                            return content;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(Value::Array(contents)) = root.get("contents") {
        for content_item in contents {
            let role = gstr(content_item.get("role")).trim().to_lowercase();
            // In Gemini API format, missing role defaults to "user"
            if role.is_empty() || role == "user" {
                if let Some(Value::Array(parts)) = content_item.get("parts") {
                    let mut texts: Vec<String> = Vec::new();
                    for part in parts {
                        if is_gemini_thought_part(part) {
                            continue;
                        }
                        if let Some(text) = part.get("text") {
                            let val = gstr(Some(text)).trim().to_string();
                            if !val.is_empty() {
                                texts.push(val);
                            }
                        }
                    }
                    if !texts.is_empty() {
                        return texts.join("\n");
                    }
                }
            }
        }
    }

    String::new()
}

// port of extractTextContent (translator/common/claude_user_id.go)
fn extract_text_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<String> = Vec::new();
            for part in parts {
                if gstr(part.get("type")) == "text" {
                    if let Some(text) = part.get("text") {
                        let val = gstr(Some(text)).trim().to_string();
                        if !val.is_empty() {
                            texts.push(val);
                        }
                    }
                }
            }
            texts.join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

// port of isResponsesUserItem (translator/common/claude_user_id.go)
fn is_responses_user_item(item: &Value) -> bool {
    let role = gstr(item.get("role")).trim().to_lowercase();
    if role == "user" {
        return true;
    }
    if role == "system" || role == "developer" || role == "assistant" {
        return false;
    }
    gstr(item.get("type")).trim().to_lowercase() == "message"
}

// port of extractResponsesItemText (translator/common/claude_user_id.go)
fn extract_responses_item_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<String> = Vec::new();
            for part in parts {
                if matches!(
                    gstr(part.get("type")).as_str(),
                    "input_text" | "output_text" | "text"
                ) {
                    if let Some(text) = part.get("text") {
                        let val = gstr(Some(text)).trim().to_string();
                        if !val.is_empty() {
                            texts.push(val);
                        }
                    }
                }
            }
            texts.join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

// port of ClaudeMessageAccumulator (translator/common/claude_messages.go)
#[derive(Default)]
struct ClaudeMessageAccumulator {
    messages: Vec<Value>,
    role: String,
    content: Vec<Value>,
    tool_use_parts: Vec<Value>,
}

impl ClaudeMessageAccumulator {
    // port of ClaudeMessageAccumulator.Append (translator/common/claude_messages.go)
    fn append(&mut self, message: Value) {
        let role = gstr(message.get("role"));
        if role != "user" && role != "assistant" {
            return;
        }
        let parts = claude_message_content_parts(message.get("content"));
        if parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        self.role = role.clone();
        for part in parts {
            if role == "assistant" && gstr(part.get("type")) == "tool_use" {
                self.tool_use_parts.push(part);
                continue;
            }
            self.content.push(part);
        }
    }

    // port of ClaudeMessageAccumulator.Flush (translator/common/claude_messages.go)
    fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.content);
        parts.append(&mut self.tool_use_parts);
        if !parts.is_empty() {
            self.messages
                .push(json!({"role": self.role, "content": parts}));
        }
        self.role.clear();
    }

    // port of ClaudeMessageAccumulator.Messages (translator/common/claude_messages.go)
    fn into_messages(mut self) -> Vec<Value> {
        self.flush();
        self.messages
    }
}

// port of claudeMessageContentParts (translator/common/claude_messages.go)
fn claude_message_content_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({"type": "text", "text": s})]
            }
        }
        Some(Value::Array(parts)) => parts.iter().filter(|p| p.is_object()).cloned().collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// util
// ---------------------------------------------------------------------------

// port of SanitizeClaudeFunctionName (util/claude_tool_id.go)
// regexp `[^a-zA-Z0-9_-]` → "_", then cut to 64 bytes.
fn sanitize_claude_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Every char is ASCII now, so a byte cut is a char cut.
    s.truncate(64);
    if s.is_empty() {
        s.push('_');
    }
    s
}

// ---------------------------------------------------------------------------
// thinking
// ---------------------------------------------------------------------------

/// The parts of `registry.ThinkingSupport` this pair reads: `(Min, Levels)`.
type ThinkingSupport = (i64, Vec<String>);

/// Stand-in for `registry.LookupModelInfo(model, "claude").Thinking`
/// (internal/registry/model_registry.go). Termory has no model registry.
fn lookup_claude_thinking_support(_model: &str) -> Option<ThinkingSupport> {
    None
}

// port of ConvertLevelToBudget (thinking/convert.go)
fn convert_level_to_budget(level: &str) -> Option<i64> {
    match level.to_lowercase().as_str() {
        "none" => Some(0),
        "auto" => Some(-1),
        "minimal" => Some(512),
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        "xhigh" => Some(32768),
        "max" => Some(128000),
        _ => None,
    }
}

// port of ConvertBudgetToLevel (thinking/convert.go)
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

// port of HasLevel (thinking/convert.go)
fn has_level(levels: &[String], target: &str) -> bool {
    levels
        .iter()
        .any(|level| level.trim().eq_ignore_ascii_case(target))
}

// port of MapToClaudeEffort (thinking/convert.go)
fn map_to_claude_effort(level: &str, supports_max: bool) -> Option<&'static str> {
    match level.trim().to_lowercase().as_str() {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "auto" => Some("high"),
        "xhigh" | "max" => Some(if supports_max { "max" } else { "high" }),
        _ => None,
    }
}

/// port of SummaryMode (thinking/summary.go); `None` is SummaryUnspecified.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SummaryMode {
    Disabled,
    Enabled,
}

// port of ApplyTranslatedSummaryToClaude (thinking/summary.go), with
// ExtractTranslatedSummaryConfig fixed to source "gemini" and
// applySummaryConfigForProvider fixed to target "claude".
fn apply_translated_summary_to_claude(out: &mut Value, source: &Value, model: &str) {
    // port of ExtractSummaryConfig (thinking/summary.go), case "gemini"
    let mode = [
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
    ]
    .iter()
    .find_map(|path| summary_bool_config(source, path));
    let Some(mode) = mode else {
        return;
    };

    let enabled = mode == SummaryMode::Enabled;
    if enabled && gget(out, "thinking.type").is_none() {
        enable_claude_thinking_for_summary(out, model);
    }
    if !claude_thinking_accepts_display(out) {
        return;
    }
    let value = if enabled { "summarized" } else { "omitted" };
    sj_set(out, "thinking.display", Value::from(value));
}

// port of summaryBoolConfig (thinking/summary.go)
fn summary_bool_config(body: &Value, path: &str) -> Option<SummaryMode> {
    match gget(body, path) {
        Some(Value::Bool(true)) => Some(SummaryMode::Enabled),
        Some(Value::Bool(false)) => Some(SummaryMode::Disabled),
        _ => None,
    }
}

// port of claudeThinkingAcceptsDisplay (thinking/summary.go)
fn claude_thinking_accepts_display(body: &Value) -> bool {
    match gstr(gget(body, "thinking.type"))
        .trim()
        .to_lowercase()
        .as_str()
    {
        "adaptive" => true,
        "enabled" => match gget(body, "thinking.budget_tokens") {
            Some(budget @ Value::Number(_)) => {
                let value = gint(Some(budget));
                value == -1 || value > 0
            }
            _ => true,
        },
        _ => false,
    }
}

// port of enableClaudeThinkingForSummary (thinking/summary.go); the
// ParseSuffix step is out of scope, the registry lookup is the stand-in.
fn enable_claude_thinking_for_summary(body: &mut Value, model: &str) {
    let model_info = if model.is_empty() {
        lookup_claude_thinking_support(&gstr(body.get("model")))
    } else {
        lookup_claude_thinking_support(model)
    };
    let Some((min, levels)) = model_info else {
        return;
    };
    if !levels.is_empty() {
        sj_set(body, "thinking.type", Value::from("adaptive"));
        sj_delete(body, "thinking.budget_tokens");
        return;
    }
    let budget = min;
    if budget <= 0 {
        return;
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        if gint(Some(max_tokens)) <= budget {
            return;
        }
    }
    sj_set(body, "thinking.type", Value::from("enabled"));
    sj_set(body, "thinking.budget_tokens", Value::from(budget));
}

// ---------------------------------------------------------------------------
// signature (Gemini replay)
// ---------------------------------------------------------------------------

const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";
const MAX_GEMINI_THOUGHT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
const SELF_DESCRIBING_SIGNATURE_FIRST_CHARS: &str = "CERg";

// port of GeminiReplaySignatureOrBypass (signature/gemini_sanitize.go), for
// SignatureBlockKindGeminiModelPart: CompatibleSignatureForProviderBlock
// returns the normalized payload when the signature is Gemini (or a Gemini
// bypass sentinel); every other decision for a Gemini model part is
// SignatureActionReplaceWithGeminiBypass.
fn gemini_replay_signature_or_bypass(raw_signature: &str) -> String {
    if detects_as_gemini(raw_signature) {
        // port of normalizeCompatibleSignatureForProvider, case Gemini
        let payload = signature_payload_without_provider_prefix(raw_signature);
        if is_gemini_thought_signature_bypass(&payload)
            || is_recognized_gemini_provider_signature(&payload)
        {
            return payload;
        }
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
}

// port of DetectSignatureProviderForBlock + signatureProviderMatchesTarget
// (signature/provider_compatibility.go), answering only "Gemini or Gemini
// bypass?". The GPT / Claude CAIS / Claude strict probes that run before the
// Gemini one cannot claim a signature the Gemini envelope check accepts (GPT
// decodes to 0x80, CAIS to 0x08, a Gemini field-2 envelope to 0x12; Claude's
// 0x12 envelopes fail Gemini's single-record shape — upstream pins that in
// TestGeminiEnvelopeNeverClaimsClaudeSignatures), so they are not ported, and
// neither is the Kimi size probe that runs after it.
fn detects_as_gemini(raw_signature: &str) -> bool {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return false;
    }
    if let Some((provider, unprefixed)) = split_signature_provider_prefix(sig) {
        return provider == "gemini"
            && (is_gemini_thought_signature_bypass(&unprefixed)
                || is_recognized_gemini_provider_signature(&unprefixed));
    }
    if sig.contains('#') {
        return false;
    }
    if is_gemini_thought_signature_bypass(sig) {
        return true;
    }
    if sig.starts_with("sealed.v1.") {
        return false;
    }
    // port of maybeSelfDescribingSignatureEnvelope
    let first = sig.as_bytes()[0];
    if SELF_DESCRIBING_SIGNATURE_FIRST_CHARS
        .as_bytes()
        .contains(&first)
    {
        return is_recognized_gemini_provider_signature(sig);
    }
    false
}

// port of SplitSignatureProviderPrefix (signature/provider_compatibility.go)
fn split_signature_provider_prefix(raw_signature: &str) -> Option<(&'static str, String)> {
    let (prefix, rest) = raw_signature.trim().split_once('#')?;
    let provider = signature_provider_from_cache_prefix(prefix)?;
    Some((provider, rest.trim().to_string()))
}

// port of SignatureProviderFromCachePrefix (signature/provider_compatibility.go)
fn signature_provider_from_cache_prefix(prefix: &str) -> Option<&'static str> {
    match prefix.trim().to_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
        | "claude-code-max" | "claude_code_max" => Some("claude"),
        "gemini" | "google" => Some("gemini"),
        "openai" | "gpt" | "codex" => Some("gpt"),
        "swe" | "sealed" => Some("swe"),
        _ => None,
    }
}

// port of SignaturePayloadWithoutProviderPrefix (signature/provider_compatibility.go)
fn signature_payload_without_provider_prefix(raw_signature: &str) -> String {
    match split_signature_provider_prefix(raw_signature) {
        Some((_, unprefixed)) => unprefixed,
        None => raw_signature.trim().to_string(),
    }
}

// port of IsGeminiThoughtSignatureBypass (signature/gemini_validation.go)
fn is_gemini_thought_signature_bypass(raw_signature: &str) -> bool {
    matches!(
        raw_signature.trim(),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR | GEMINI_CONTEXT_ENGINEERING_BYPASS
    )
}

// port of isRecognizedGeminiProviderSignature (signature/provider_compatibility.go)
// + InspectGeminiThoughtSignature with RequireKnownEnvelope
// (signature/gemini_validation.go). The leading IsValidClaudeCAISSignature
// rejection is implied: CAIS needs a std-base64 payload starting 0x08, the
// known Gemini envelope starts 0x12.
fn is_recognized_gemini_provider_signature(raw_signature: &str) -> bool {
    let sig = raw_signature.trim();
    if sig.is_empty() || is_gemini_thought_signature_bypass(sig) {
        return false;
    }
    let Some(decoded) = decode_gemini_thought_signature(sig) else {
        return false;
    };
    if decoded.is_empty() {
        return false;
    }
    // port of classifyGeminiThoughtSignatureEnvelope
    if is_ascii_uuid_bytes(&decoded) {
        return false;
    }
    is_gemini_field2_envelope(&decoded)
}

// port of decodeGeminiThoughtSignature (signature/gemini_validation.go).
// Go's decoder skips '\r'/'\n' and tolerates non-zero trailing bits.
fn decode_gemini_thought_signature(sig: &str) -> Option<Vec<u8>> {
    if sig.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN {
        return None;
    }
    let cleaned: String = sig.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    let config = GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true);
    let std = GeneralPurpose::new(
        &alphabet::STANDARD,
        config.with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    if let Ok(decoded) = std.decode(&cleaned) {
        return Some(decoded);
    }
    let raw = GeneralPurpose::new(
        &alphabet::STANDARD,
        config.with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    raw.decode(&cleaned).ok()
}

// port of isGeminiField2Envelope + inspectGeminiField2Envelope
// (signature/gemini_validation.go)
fn is_gemini_field2_envelope(decoded: &[u8]) -> bool {
    match consume_gemini_field2_field1_value(decoded) {
        Some(value) => {
            (is_likely_gemini_opaque_payload(value)
                || is_ascii_uuid_bytes(value)
                || is_likely_gemini_tool_invocation_payload(value))
                && !value.is_empty()
        }
        None => false,
    }
}

// port of consumeGeminiField2Field1Value (signature/gemini_validation.go)
fn consume_gemini_field2_field1_value(decoded: &[u8]) -> Option<&[u8]> {
    let (num, typ, n) = consume_tag(decoded)?;
    if num != 2 || typ != WIRE_BYTES {
        return None;
    }
    let (container, m) = consume_bytes(&decoded[n..])?;
    if n + m != decoded.len() {
        return None;
    }
    let (num, typ, n) = consume_tag(container)?;
    if num != 1 || typ != WIRE_BYTES {
        return None;
    }
    let (value, m) = consume_bytes(&container[n..])?;
    if n + m != container.len() {
        return None;
    }
    Some(value)
}

// port of isLikelyGeminiOpaquePayload (signature/gemini_validation.go)
fn is_likely_gemini_opaque_payload(value: &[u8]) -> bool {
    value.first() == Some(&0x01)
}

// port of isLikelyGeminiToolInvocationPayload (signature/gemini_validation.go)
fn is_likely_gemini_tool_invocation_payload(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut offset = 0;
    let mut has_tink_field = false;
    while offset < value.len() {
        let Some((_, typ, n)) = consume_tag(&value[offset..]) else {
            return false;
        };
        offset += n;
        let consumed = match typ {
            WIRE_VARINT => consume_varint(&value[offset..]).map(|(_, n)| n),
            WIRE_BYTES => consume_bytes(&value[offset..]).map(|(bytes_val, n)| {
                if is_likely_gemini_opaque_payload(bytes_val) {
                    has_tink_field = true;
                }
                n
            }),
            WIRE_FIXED32 => (value.len() - offset >= 4).then_some(4),
            WIRE_FIXED64 => (value.len() - offset >= 8).then_some(8),
            _ => None,
        };
        match consumed {
            Some(n) => offset += n,
            None => return false,
        }
    }
    has_tink_field && offset == value.len()
}

// port of isASCIIUUIDBytes (signature/gemini_validation.go)
fn is_ascii_uuid_bytes(decoded: &[u8]) -> bool {
    if decoded.len() != 36 {
        return false;
    }
    decoded.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

// protowire (google.golang.org/protobuf/encoding/protowire) — the subset the
// Gemini envelope walk needs.
const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_BYTES: u64 = 2;
const WIRE_FIXED32: u64 = 5;

// port of protowire.ConsumeVarint
fn consume_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &byte) in b.iter().enumerate().take(10) {
        if i == 9 {
            if byte > 1 {
                return None;
            }
            return Some((v | (u64::from(byte) << 63), 10));
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

// port of protowire.ConsumeTag (field number must be 1..=MaxInt32)
fn consume_tag(b: &[u8]) -> Option<(u64, u64, usize)> {
    let (v, n) = consume_varint(b)?;
    let num = v >> 3;
    if num < 1 || num > i32::MAX as u64 {
        return None;
    }
    Some((num, v & 7, n))
}

// port of protowire.ConsumeBytes
fn consume_bytes(b: &[u8]) -> Option<(&[u8], usize)> {
    let (m, n) = consume_varint(b)?;
    if m > (b.len() - n) as u64 {
        return None;
    }
    let m = m as usize;
    Some((&b[n..n + m], n + m))
}

// ---------------------------------------------------------------------------
// Request: Gemini generateContent → Anthropic Messages
// ---------------------------------------------------------------------------

// port of ConvertGeminiRequestToClaude (claude_gemini_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    let root = body;
    let user_id = derive_claude_user_id(root);

    // Base Claude message payload
    let mut out = json!({"model": "", "max_tokens": 32000, "messages": [], "metadata": {}});
    sj_set(&mut out, "metadata.user_id", Value::from(user_id));

    let mut accumulator = ClaudeMessageAccumulator::default();

    // FIFO queue of tool ids, consumed in order when functionResponses arrive.
    let mut pending_tool_ids: Vec<String> = Vec::new();
    let mut tool_call_counter: u64 = 0;

    sj_set(&mut out, "model", Value::from(model));
    if let Some(Value::String(tier)) = root.get("service_tier") {
        sj_set(&mut out, "service_tier", Value::from(tier.clone()));
    }

    // Generation config extraction from Gemini format
    if let Some(gen_config) = root.get("generationConfig") {
        if let Some(max_tokens) = gen_config.get("maxOutputTokens") {
            sj_set(&mut out, "max_tokens", Value::from(gint(Some(max_tokens))));
        }
        if let Some(top_p) = gen_config.get("topP") {
            sj_set(&mut out, "top_p", float_value(gfloat(Some(top_p))));
        }
        if let Some(Value::Array(stop_seqs)) = gen_config.get("stopSequences") {
            let stop_sequences: Vec<Value> = stop_seqs
                .iter()
                .map(|v| Value::from(gstr(Some(v))))
                .collect();
            if !stop_sequences.is_empty() {
                sj_set(&mut out, "stop_sequences", Value::Array(stop_sequences));
            }
        }
        if let Some(thinking_config @ Value::Object(_)) = gen_config.get("thinkingConfig") {
            apply_gemini_thinking_config(&mut out, thinking_config, model);
        }
    }

    // System instruction conversion (a user turn of its own)
    if let Some(sys_instr) = root.get("system_instruction") {
        if let Some(Value::Array(parts)) = sys_instr.get("parts") {
            let mut system_text = String::new();
            for part in parts {
                if is_gemini_thought_part(part) {
                    continue;
                }
                if let Some(text) = part.get("text") {
                    if !system_text.is_empty() {
                        system_text.push('\n');
                    }
                    system_text.push_str(&gstr(Some(text)));
                }
            }
            if !system_text.is_empty() {
                accumulator.append(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": system_text}]
                }));
                accumulator.flush();
            }
        }
    }

    // Contents conversion to messages with proper role mapping
    if let Some(Value::Array(contents)) = root.get("contents") {
        for content in contents {
            let mut role = gstr(content.get("role"));
            if role == "model" {
                role = "assistant".to_string();
            }
            if role == "function" || role == "tool" {
                role = "user".to_string();
            }

            let mut content_items: Vec<Value> = Vec::new();
            if let Some(Value::Array(parts)) = content.get("parts") {
                for part in parts {
                    if is_gemini_thought_part(part) {
                        continue;
                    }

                    if let Some(text) = part.get("text") {
                        content_items.push(json!({"type": "text", "text": gstr(Some(text))}));
                        continue;
                    }

                    if let (Some(fc), true) = (part.get("functionCall"), role == "assistant") {
                        let mut tool_use =
                            json!({"type": "tool_use", "id": "", "name": "", "input": {}});
                        // Reuse gateway-provided IDs when present, otherwise generate one.
                        let mut tool_id = get_gemini_tool_id(fc);
                        if tool_id.is_empty() {
                            tool_call_counter += 1;
                            tool_id = format!("toolu_gemini_{tool_call_counter:016}");
                        }
                        pending_tool_ids.push(tool_id.clone());
                        tool_use["id"] = Value::from(tool_id);
                        if let Some(name) = fc.get("name") {
                            tool_use["name"] =
                                Value::from(sanitize_claude_function_name(&gstr(Some(name))));
                        }
                        if let Some(args @ Value::Object(_)) = fc.get("args") {
                            tool_use["input"] = args.clone();
                        }
                        content_items.push(tool_use);
                        continue;
                    }

                    if let Some(fr) = part.get("functionResponse") {
                        let mut tool_result =
                            json!({"type": "tool_result", "tool_use_id": "", "content": ""});
                        let custom_id = get_gemini_tool_id(fr);
                        let tool_id = if !custom_id.is_empty() {
                            if let Some(pos) = pending_tool_ids.iter().position(|p| *p == custom_id)
                            {
                                pending_tool_ids.remove(pos);
                            }
                            custom_id
                        } else if !pending_tool_ids.is_empty() {
                            pending_tool_ids.remove(0)
                        } else {
                            tool_call_counter += 1;
                            format!("toolu_gemini_{tool_call_counter:016}")
                        };
                        tool_result["tool_use_id"] = Value::from(tool_id);

                        if let Some(result) = gget(fr, "response.result") {
                            tool_result["content"] = Value::from(gstr(Some(result)));
                        } else if let Some(response) = fr.get("response") {
                            // gjson `.Raw`: the JSON text of the value
                            tool_result["content"] = Value::from(response.to_string());
                        }
                        content_items.push(tool_result);
                        continue;
                    }

                    if let Some(inline_data) = gemini_claude_inline_data(part) {
                        if let Some(content_part) =
                            claude_content_part_from_gemini_inline_data(inline_data)
                        {
                            content_items.push(content_part);
                        }
                        continue;
                    }

                    if let Some(file_data) = gemini_claude_file_data(part) {
                        if let Some(content_part) =
                            claude_content_part_from_gemini_file_data(file_data)
                        {
                            content_items.push(content_part);
                        }
                        continue;
                    }
                }
            }

            // Only add message if it has content.
            if !content_items.is_empty() {
                accumulator.append(json!({"role": role, "content": content_items}));
            }
        }
    }
    let messages = accumulator.into_messages();
    if !messages.is_empty() {
        out["messages"] = Value::Array(messages);
    }

    // Tools mapping: Gemini functionDeclarations -> Claude tools
    if let Some(Value::Array(tools)) = root.get("tools") {
        let mut anthropic_tools: Vec<Value> = Vec::new();
        for tool in tools {
            if let Some(Value::Array(func_decls)) = tool.get("functionDeclarations") {
                for func_decl in func_decls {
                    let mut anthropic_tool = json!({
                        "name": "",
                        "description": "",
                        "input_schema": {"type": "object", "properties": {}}
                    });
                    if let Some(name) = func_decl.get("name") {
                        anthropic_tool["name"] =
                            Value::from(sanitize_claude_function_name(&gstr(Some(name))));
                    }
                    if let Some(desc) = func_decl.get("description") {
                        anthropic_tool["description"] = Value::from(gstr(Some(desc)));
                    }
                    if let Some(params) = func_decl.get("parameters") {
                        anthropic_tool["input_schema"] = normalize_claude_tool_schema(params);
                    } else if let Some(params) = func_decl.get("parametersJsonSchema") {
                        anthropic_tool["input_schema"] = normalize_claude_tool_schema(params);
                    }
                    lowercase_claude_tool_schema_types(&mut anthropic_tool);
                    anthropic_tools.push(anthropic_tool);
                }
            }
        }
        if !anthropic_tools.is_empty() {
            sj_set(&mut out, "tools", Value::Array(anthropic_tools));
        }
    }

    // Tool config mapping from Gemini format to Claude format
    if let Some(tool_config) = root.get("tool_config") {
        set_claude_tool_choice_from_gemini_tool_config(
            &mut out,
            tool_config.get("function_calling_config"),
        );
    } else if let Some(tool_config) = root.get("toolConfig") {
        set_claude_tool_choice_from_gemini_tool_config(
            &mut out,
            tool_config.get("functionCallingConfig"),
        );
    }

    sj_set(&mut out, "stream", Value::Bool(stream));

    apply_translated_summary_to_claude(&mut out, root, model);
    out
}

/// The `generationConfig.thinkingConfig` block of ConvertGeminiRequestToClaude
/// (claude_gemini_request.go), split out for readability.
fn apply_gemini_thinking_config(out: &mut Value, thinking_config: &Value, model: &str) {
    let mi = lookup_claude_thinking_support(model);
    let supports_adaptive = mi.as_ref().is_some_and(|(_, levels)| !levels.is_empty());
    let supports_max = supports_adaptive
        && mi
            .as_ref()
            .is_some_and(|(_, levels)| has_level(levels, "max"));

    let thinking_level = thinking_config
        .get("thinkingLevel")
        .or_else(|| thinking_config.get("thinking_level"));
    if let Some(thinking_level) = thinking_level {
        let level = gstr(Some(thinking_level)).trim().to_lowercase();
        if supports_adaptive {
            match level.as_str() {
                "" => {}
                "none" => {
                    sj_set(out, "thinking.type", Value::from("disabled"));
                    sj_delete(out, "thinking.budget_tokens");
                    sj_delete(out, "output_config.effort");
                }
                _ => {
                    let mapped = map_to_claude_effort(&level, supports_max)
                        .map(str::to_string)
                        .unwrap_or(level);
                    sj_set(out, "thinking.type", Value::from("adaptive"));
                    sj_delete(out, "thinking.budget_tokens");
                    sj_set(out, "output_config.effort", Value::from(mapped));
                }
            }
        } else {
            match level.as_str() {
                "" => {}
                "none" => {
                    sj_set(out, "thinking.type", Value::from("disabled"));
                    sj_delete(out, "thinking.budget_tokens");
                }
                "auto" => {
                    sj_set(out, "thinking.type", Value::from("enabled"));
                    sj_delete(out, "thinking.budget_tokens");
                }
                _ => {
                    if let Some(budget) = convert_level_to_budget(&level) {
                        sj_set(out, "thinking.type", Value::from("enabled"));
                        sj_set(out, "thinking.budget_tokens", Value::from(budget));
                    }
                }
            }
        }
        return;
    }

    let thinking_budget = thinking_config
        .get("thinkingBudget")
        .or_else(|| thinking_config.get("thinking_budget"));
    let Some(thinking_budget) = thinking_budget else {
        return;
    };
    let budget = gint(Some(thinking_budget));
    if supports_adaptive {
        match budget {
            0 => {
                sj_set(out, "thinking.type", Value::from("disabled"));
                sj_delete(out, "thinking.budget_tokens");
                sj_delete(out, "output_config.effort");
            }
            _ => {
                if let Some(level) = convert_budget_to_level(budget) {
                    let level = map_to_claude_effort(level, supports_max).unwrap_or(level);
                    sj_set(out, "thinking.type", Value::from("adaptive"));
                    sj_delete(out, "thinking.budget_tokens");
                    sj_set(out, "output_config.effort", Value::from(level));
                }
            }
        }
    } else {
        match budget {
            0 => {
                sj_set(out, "thinking.type", Value::from("disabled"));
                sj_delete(out, "thinking.budget_tokens");
            }
            -1 => {
                sj_set(out, "thinking.type", Value::from("enabled"));
                sj_delete(out, "thinking.budget_tokens");
            }
            _ => {
                sj_set(out, "thinking.type", Value::from("enabled"));
                sj_set(out, "thinking.budget_tokens", Value::from(budget));
            }
        }
    }
}

// port of the getGeminiToolID closure (claude_gemini_request.go)
fn get_gemini_tool_id(value: &Value) -> String {
    let tool_id = gstr(value.get("id")).trim().to_string();
    if !tool_id.is_empty() {
        return tool_id;
    }
    gstr(value.get("call_id")).trim().to_string()
}

// port of normalizeClaudeToolSchema (claude_gemini_request.go)
fn normalize_claude_tool_schema(parameters: &Value) -> Value {
    // sjson cannot set a key on a non-object document; an object is the only
    // shape a schema can take here.
    let mut cleaned = if parameters.is_object() {
        parameters.clone()
    } else {
        Value::Object(Map::new())
    };
    if parameters.get("additionalProperties") != Some(&Value::Bool(false)) {
        sj_set(&mut cleaned, "additionalProperties", Value::Bool(false));
    }
    const SCHEMA: &str = "http://json-schema.org/draft-07/schema#";
    if parameters.get("$schema").and_then(Value::as_str) != Some(SCHEMA) {
        sj_set(&mut cleaned, "$schema", Value::from(SCHEMA));
    }
    cleaned
}

// port of lowercaseClaudeToolSchemaTypes (claude_gemini_request.go) with
// util.Walk (util/translator.go): every key named "type", at any depth, whose
// value is not already a lowercase string is rewritten to the lowercased
// gjson `.String()` of that value.
fn lowercase_claude_tool_schema_types(tool: &mut Value) {
    match tool {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key == "type" {
                    match val {
                        Value::String(s) => {
                            let lower = s.to_lowercase();
                            if lower != *s {
                                *s = lower;
                            }
                        }
                        Value::Null | Value::Bool(_) | Value::Number(_) => {
                            *val = Value::from(gstr(Some(val)).to_lowercase());
                        }
                        // An object/array under "type" is a schema PROPERTY
                        // named "type"; see the deviation note in the module docs.
                        Value::Object(_) | Value::Array(_) => {}
                    }
                }
                lowercase_claude_tool_schema_types(val);
            }
        }
        Value::Array(items) => {
            for item in items {
                lowercase_claude_tool_schema_types(item);
            }
        }
        _ => {}
    }
}

// port of setClaudeToolChoiceFromGeminiToolConfig (claude_gemini_request.go)
fn set_claude_tool_choice_from_gemini_tool_config(out: &mut Value, func_calling: Option<&Value>) {
    let Some(func_calling) = func_calling else {
        return;
    };
    let Some(mode) = func_calling.get("mode") else {
        return;
    };
    match gstr(Some(mode)).as_str() {
        "AUTO" => sj_set(out, "tool_choice", json!({"type": "auto"})),
        "NONE" => sj_set(out, "tool_choice", json!({"type": "none"})),
        "ANY" => {
            let allowed_names = func_calling
                .get("allowedFunctionNames")
                .or_else(|| func_calling.get("allowed_function_names"));
            match allowed_names {
                Some(Value::Array(items)) if items.len() == 1 => {
                    let name = sanitize_claude_function_name(&gstr(Some(&items[0])));
                    sj_set(out, "tool_choice", json!({"type": "tool", "name": name}));
                }
                _ => sj_set(out, "tool_choice", json!({"type": "any"})),
            }
        }
        _ => {}
    }
}

// port of geminiClaudeInlineData (claude_gemini_request.go)
fn gemini_claude_inline_data(part: &Value) -> Option<&Value> {
    part.get("inlineData").or_else(|| part.get("inline_data"))
}

// port of geminiClaudeFileData (claude_gemini_request.go)
fn gemini_claude_file_data(part: &Value) -> Option<&Value> {
    part.get("fileData").or_else(|| part.get("file_data"))
}

fn mime_of(data: &Value) -> String {
    let mime_type = gstr(data.get("mimeType"));
    if mime_type.is_empty() {
        gstr(data.get("mime_type"))
    } else {
        mime_type
    }
}

// port of claudeContentPartFromGeminiInlineData (claude_gemini_request.go)
fn claude_content_part_from_gemini_inline_data(inline_data: &Value) -> Option<Value> {
    let mime_type = mime_of(inline_data);
    let data = gstr(inline_data.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime_type, "data": data}
        }))
    } else if lower.starts_with("application/") || lower.starts_with("text/") {
        Some(json!({
            "type": "document",
            "source": {"type": "base64", "media_type": mime_type, "data": data}
        }))
    } else {
        Some(claude_text_content_part(&format!(
            "Media content: inline data (Type: {mime_type})"
        )))
    }
}

// port of claudeContentPartFromGeminiFileData (claude_gemini_request.go)
fn claude_content_part_from_gemini_file_data(file_data: &Value) -> Option<Value> {
    let mut file_uri = gstr(file_data.get("fileUri"));
    if file_uri.is_empty() {
        file_uri = gstr(file_data.get("file_uri"));
    }
    if file_uri.is_empty() {
        return None;
    }
    let mime_type = mime_of(file_data);
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        Some(json!({"type": "image", "source": {"type": "url", "url": file_uri}}))
    } else if lower.starts_with("application/") || lower.starts_with("text/") {
        let mut document = json!({"type": "document", "source": {"type": "url", "url": file_uri}});
        if !mime_type.is_empty() {
            document["source"]["media_type"] = Value::from(mime_type);
        }
        Some(document)
    } else {
        let mut file_info = format!("File: {file_uri}");
        if !mime_type.is_empty() {
            file_info.push_str(&format!(" (Type: {mime_type})"));
        }
        Some(claude_text_content_part(&file_info))
    }
}

// port of claudeTextContentPart (claude_gemini_request.go)
fn claude_text_content_part(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

// ---------------------------------------------------------------------------
// Response: Anthropic Messages → Gemini generateContent
// ---------------------------------------------------------------------------

/// port of ConvertAnthropicResponseToGeminiParams (claude_gemini_response.go).
/// `LastStorageOutput` / `IsStreaming` are written but never read in Go and
/// are left out.
pub struct StreamTranslator {
    model: String,
    created_at: i64,
    response_id: String,
    tool_use_names: HashMap<i64, String>,
    tool_use_args: HashMap<i64, String>,
    tool_use_ids: HashMap<i64, String>,
}

/// `functionCall.args` is spliced in raw by Go (`sjson.SetRawBytes`); a
/// payload that is not JSON is kept as a string instead of emitting an
/// invalid frame.
fn parse_raw_args(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| Value::from(args))
}

/// The functionCall part ConvertClaudeResponseToGemini assembles at
/// content_block_stop (claude_gemini_response.go).
fn function_call_part(name: &str, args_trim: &str, tool_id: &str) -> Value {
    let mut function_call = json!({"functionCall": {"name": "", "args": {}}});
    if !name.is_empty() {
        function_call["functionCall"]["name"] = Value::from(name);
    }
    if !args_trim.is_empty() {
        function_call["functionCall"]["args"] = parse_raw_args(args_trim);
    }
    if !tool_id.is_empty() {
        function_call["functionCall"]["id"] = Value::from(tool_id);
    }
    function_call
}

/// The usage block both directions of claude_gemini_response.go build from a
/// message_delta, written into `target` under `prefix`.
fn write_usage(target: &mut Value, prefix: &str, usage: &Value) {
    let key = |k: &str| {
        if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{prefix}.{k}")
        }
    };
    let input_tokens = gint(usage.get("input_tokens"));
    let output_tokens = gint(usage.get("output_tokens"));
    sj_set(target, &key("promptTokenCount"), Value::from(input_tokens));
    sj_set(
        target,
        &key("candidatesTokenCount"),
        Value::from(output_tokens),
    );
    sj_set(
        target,
        &key("totalTokenCount"),
        Value::from(input_tokens + output_tokens),
    );
    if let Some(cache_creation) = usage.get("cache_creation_input_tokens") {
        sj_set(
            target,
            &key("cachedContentTokenCount"),
            Value::from(gint(Some(cache_creation))),
        );
    }
    if let Some(cache_read) = usage.get("cache_read_input_tokens") {
        let existing = gint(usage.get("cache_creation_input_tokens"));
        sj_set(
            target,
            &key("cachedContentTokenCount"),
            Value::from(existing + gint(Some(cache_read))),
        );
    }
    if let Some(thinking_tokens) = usage.get("thinking_tokens") {
        sj_set(
            target,
            &key("thoughtsTokenCount"),
            Value::from(gint(Some(thinking_tokens))),
        );
    }
    sj_set(
        target,
        &key("trafficType"),
        Value::from("PROVISIONED_THROUGHPUT"),
    );
}

/// The client's model name: Gemini carries it in the URL, so the body only
/// has it when the router put it there.
fn request_model_name(original_request: &Value) -> String {
    gstr(original_request.get("model"))
}

impl StreamTranslator {
    // port of the param initialisation in ConvertClaudeResponseToGemini
    pub fn new(original_request: &Value) -> Self {
        Self {
            model: request_model_name(original_request),
            created_at: 0,
            response_id: String::new(),
            tool_use_names: HashMap::new(),
            tool_use_args: HashMap::new(),
            tool_use_ids: HashMap::new(),
        }
    }

    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert(data)
            .into_iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect()
    }

    /// Gemini streams have no terminal sentinel.
    pub fn finish(&mut self) -> Vec<String> {
        Vec::new()
    }

    // port of ConvertClaudeResponseToGemini (claude_gemini_response.go)
    fn convert(&mut self, root: &Value) -> Vec<Value> {
        let event_type = gstr(root.get("type"));

        // Base Gemini response template with default values
        let mut template = json!({
            "candidates": [{"content": {"role": "model", "parts": []}}],
            "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
            "modelVersion": "",
            "createTime": "",
            "responseId": ""
        });
        if !self.model.is_empty() {
            template["modelVersion"] = Value::from(self.model.clone());
        }
        if !self.response_id.is_empty() {
            template["responseId"] = Value::from(self.response_id.clone());
        }
        if self.created_at == 0 {
            self.created_at = now_unix();
        }
        template["createTime"] = Value::from(format_create_time(self.created_at));

        match event_type.as_str() {
            "message_start" => {
                if let Some(message) = root.get("message") {
                    self.response_id = gstr(message.get("id"));
                    self.model = gstr(message.get("model"));
                }
                Vec::new()
            }

            "content_block_start" => {
                if let Some(cb) = root.get("content_block") {
                    let cb_type = gstr(cb.get("type"));
                    if cb_type == "tool_use" {
                        let idx = gint(root.get("index"));
                        if let Some(name) = cb.get("name") {
                            self.tool_use_names.insert(idx, gstr(Some(name)));
                        }
                        let tool_id = gstr(cb.get("id"));
                        if !tool_id.is_empty() {
                            self.tool_use_ids.insert(idx, tool_id);
                        }
                    } else if cb_type == "thinking" {
                        let sig = gstr(cb.get("signature"));
                        if !sig.is_empty() {
                            append_candidate_part(
                                &mut template,
                                json!({
                                    "thought": true,
                                    "thoughtSignature": gemini_replay_signature_or_bypass(&sig)
                                }),
                            );
                            return vec![template];
                        }
                    }
                }
                Vec::new()
            }

            "content_block_delta" => {
                if let Some(delta) = root.get("delta") {
                    match gstr(delta.get("type")).as_str() {
                        "text_delta" => {
                            let text = gstr(delta.get("text"));
                            if !text.is_empty() {
                                append_candidate_part(&mut template, json!({"text": text}));
                            }
                        }
                        "thinking_delta" => {
                            let text = gstr(delta.get("thinking"));
                            if !text.is_empty() {
                                append_candidate_part(
                                    &mut template,
                                    json!({"thought": true, "text": text}),
                                );
                            }
                        }
                        "signature_delta" => {
                            let sig = gstr(delta.get("signature"));
                            if !sig.is_empty() {
                                append_candidate_part(
                                    &mut template,
                                    json!({
                                        "thought": true,
                                        "thoughtSignature": gemini_replay_signature_or_bypass(&sig)
                                    }),
                                );
                            }
                        }
                        "input_json_delta" => {
                            let idx = gint(root.get("index"));
                            let args = self.tool_use_args.entry(idx).or_default();
                            if let Some(pj) = delta.get("partial_json") {
                                args.push_str(&gstr(Some(pj)));
                            }
                            return Vec::new();
                        }
                        _ => {}
                    }
                }
                vec![template]
            }

            "content_block_stop" => {
                let idx = gint(root.get("index"));
                let name = self.tool_use_names.get(&idx).cloned().unwrap_or_default();
                let args_trim = self
                    .tool_use_args
                    .get(&idx)
                    .map(|b| b.trim().to_string())
                    .unwrap_or_default();
                let tool_id = self.tool_use_ids.get(&idx).cloned().unwrap_or_default();
                if !name.is_empty() || !args_trim.is_empty() {
                    append_candidate_part(
                        &mut template,
                        function_call_part(&name, &args_trim, &tool_id),
                    );
                    set_finish_reason(&mut template, "STOP");
                    self.tool_use_args.remove(&idx);
                    self.tool_use_names.remove(&idx);
                    self.tool_use_ids.remove(&idx);
                    return vec![template];
                }
                Vec::new()
            }

            "message_delta" => {
                if let Some(delta) = root.get("delta") {
                    if let Some(stop_reason) = delta.get("stop_reason") {
                        let reason = match gstr(Some(stop_reason)).as_str() {
                            "max_tokens" => "MAX_TOKENS",
                            // end_turn, tool_use, stop_sequence and the rest
                            _ => "STOP",
                        };
                        set_finish_reason(&mut template, reason);
                    }
                }
                if let Some(usage) = root.get("usage") {
                    write_usage(&mut template, "usageMetadata", usage);
                }
                // Go overwrites any mapped reason with STOP here.
                set_finish_reason(&mut template, "STOP");
                vec![template]
            }

            "message_stop" => Vec::new(),

            "error" => {
                let mut error_msg = gstr(gget(root, "error.message"));
                if error_msg.is_empty() {
                    error_msg = "Unknown error occurred".to_string();
                }
                vec![
                    json!({"error": {"code": 400, "message": error_msg, "status": "INVALID_ARGUMENT"}}),
                ]
            }

            _ => Vec::new(),
        }
    }
}

/// Complete upstream response → Gemini generateContent response.
///
/// Go's ConvertClaudeResponseToGeminiNonStream reads the upstream body as SSE
/// text; the router hands over the complete Messages object instead. A string
/// body is read as SSE exactly like Go; an array is taken as the already-parsed
/// event list; an object (a Messages response) is first replayed as the event
/// sequence Anthropic would have streamed for it.
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let mut model_name = request_model_name(original_request);
    if model_name.is_empty() {
        model_name = gstr(upstream.get("model"));
    }
    let events: Vec<Value> = match upstream {
        Value::String(text) => sse_data_events(text),
        Value::Array(events) => events.clone(),
        Value::Object(_) => message_to_events(upstream),
        _ => Vec::new(),
    };
    convert_claude_response_to_gemini_non_stream(&model_name, &events)
}

/// The SSE line split at the top of ConvertClaudeResponseToGeminiNonStream
/// (claude_gemini_response.go): every `data:` line, trimmed. A payload that
/// is not JSON parses as nothing in gjson and is dropped here.
fn sse_data_events(text: &str) -> Vec<Value> {
    text.split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| !data.is_empty())
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

/// The stream events Anthropic emits for a complete Messages response — not a
/// port; it adapts the router's non-stream body to the Go event loop.
fn message_to_events(message: &Value) -> Vec<Value> {
    if gstr(message.get("type")) == "error" {
        return vec![message.clone()];
    }
    let mut start = Map::new();
    for key in ["id", "model"] {
        if let Some(v) = message.get(key) {
            start.insert(key.to_string(), v.clone());
        }
    }
    let mut events = vec![json!({"type": "message_start", "message": start})];
    if let Some(Value::Array(blocks)) = message.get("content") {
        for (index, block) in blocks.iter().enumerate() {
            let block_type = gstr(block.get("type"));
            let mut deltas: Vec<Value> = Vec::new();
            let start_block = match block_type.as_str() {
                "text" => {
                    deltas.push(json!({"type": "text_delta", "text": gstr(block.get("text"))}));
                    json!({"type": "text", "text": ""})
                }
                "thinking" => {
                    deltas.push(
                        json!({"type": "thinking_delta", "thinking": gstr(block.get("thinking"))}),
                    );
                    let sig = gstr(block.get("signature"));
                    if !sig.is_empty() {
                        deltas.push(json!({"type": "signature_delta", "signature": sig}));
                    }
                    json!({"type": "thinking", "thinking": ""})
                }
                "tool_use" => {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    deltas.push(
                        json!({"type": "input_json_delta", "partial_json": input.to_string()}),
                    );
                    json!({"type": "tool_use", "id": block.get("id").cloned().unwrap_or(Value::Null), "name": block.get("name").cloned().unwrap_or(Value::Null), "input": {}})
                }
                _ => block.clone(),
            };
            events.push(
                json!({"type": "content_block_start", "index": index, "content_block": start_block}),
            );
            for delta in deltas {
                events.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
            }
            events.push(json!({"type": "content_block_stop", "index": index}));
        }
    }
    let mut message_delta = json!({
        "type": "message_delta",
        "delta": {"stop_reason": message.get("stop_reason").cloned().unwrap_or(Value::Null)}
    });
    if let Some(usage) = message.get("usage") {
        message_delta["usage"] = usage.clone();
    }
    events.push(message_delta);
    events.push(json!({"type": "message_stop"}));
    events
}

// port of ConvertClaudeResponseToGeminiNonStream (claude_gemini_response.go),
// from the point where the SSE body has been split into events.
fn convert_claude_response_to_gemini_non_stream(model_name: &str, events: &[Value]) -> Value {
    let mut template = json!({
        "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
        "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
        "modelVersion": "",
        "createTime": "",
        "responseId": ""
    });
    template["modelVersion"] = Value::from(model_name);

    let mut tool_use_names: HashMap<i64, String> = HashMap::new();
    let mut tool_use_args: HashMap<i64, String> = HashMap::new();
    let mut tool_use_ids: HashMap<i64, String> = HashMap::new();

    let mut all_parts: Vec<Value> = Vec::new();
    let mut final_usage: Option<Value> = None;
    let mut response_id = String::new();
    let mut created_at: i64 = 0;

    for root in events {
        match gstr(root.get("type")).as_str() {
            "message_start" => {
                if let Some(message) = root.get("message") {
                    response_id = gstr(message.get("id"));
                    created_at = now_unix();
                }
            }

            "content_block_start" => {
                let idx = gint(root.get("index"));
                if let Some(cb) = root.get("content_block") {
                    let cb_type = gstr(cb.get("type"));
                    if cb_type == "tool_use" {
                        if let Some(name) = cb.get("name") {
                            tool_use_names.insert(idx, gstr(Some(name)));
                        }
                        let tool_id = gstr(cb.get("id"));
                        if !tool_id.is_empty() {
                            tool_use_ids.insert(idx, tool_id);
                        }
                    } else if cb_type == "thinking" {
                        let sig = gstr(cb.get("signature"));
                        if !sig.is_empty() {
                            all_parts.push(json!({
                                "thought": true,
                                "thoughtSignature": gemini_replay_signature_or_bypass(&sig)
                            }));
                        }
                    }
                }
            }

            "content_block_delta" => {
                if let Some(delta) = root.get("delta") {
                    match gstr(delta.get("type")).as_str() {
                        "text_delta" => {
                            let text = gstr(delta.get("text"));
                            if !text.is_empty() {
                                all_parts.push(json!({"text": text}));
                            }
                        }
                        "thinking_delta" => {
                            let text = gstr(delta.get("thinking"));
                            if !text.is_empty() {
                                all_parts.push(json!({"thought": true, "text": text}));
                            }
                        }
                        "signature_delta" => {
                            let sig = gstr(delta.get("signature"));
                            if !sig.is_empty() {
                                all_parts.push(json!({
                                    "thought": true,
                                    "thoughtSignature": gemini_replay_signature_or_bypass(&sig)
                                }));
                            }
                        }
                        "input_json_delta" => {
                            let idx = gint(root.get("index"));
                            let args = tool_use_args.entry(idx).or_default();
                            if let Some(pj) = delta.get("partial_json") {
                                args.push_str(&gstr(Some(pj)));
                            }
                        }
                        _ => {}
                    }
                }
            }

            "content_block_stop" => {
                let idx = gint(root.get("index"));
                let name = tool_use_names.get(&idx).cloned().unwrap_or_default();
                let args_trim = tool_use_args
                    .get(&idx)
                    .map(|b| b.trim().to_string())
                    .unwrap_or_default();
                let tool_id = tool_use_ids.get(&idx).cloned().unwrap_or_default();
                if !name.is_empty() || !args_trim.is_empty() {
                    all_parts.push(function_call_part(&name, &args_trim, &tool_id));
                    tool_use_args.remove(&idx);
                    tool_use_names.remove(&idx);
                    tool_use_ids.remove(&idx);
                }
            }

            "message_delta" => {
                if let Some(usage) = root.get("usage") {
                    let mut usage_json = json!({});
                    write_usage(&mut usage_json, "", usage);
                    final_usage = Some(usage_json);
                }
            }

            _ => {}
        }
    }

    if !response_id.is_empty() {
        template["responseId"] = Value::from(response_id);
    }
    if created_at > 0 {
        template["createTime"] = Value::from(format_create_time(created_at));
    }

    let consolidated = consolidate_parts(all_parts);
    if !consolidated.is_empty() {
        template["candidates"][0]["content"]["parts"] = Value::Array(consolidated);
    }
    if let Some(usage) = final_usage {
        template["usageMetadata"] = usage;
    }
    template
}

// port of consolidateParts (claude_gemini_response.go)
fn consolidate_parts(parts: Vec<Value>) -> Vec<Value> {
    if parts.is_empty() {
        return parts;
    }

    struct State {
        consolidated: Vec<Value>,
        text: String,
        thought: String,
        thought_signature: String,
        has_text: bool,
        has_thought: bool,
    }
    impl State {
        fn flush_text(&mut self) {
            if self.has_text && !self.text.is_empty() {
                self.consolidated
                    .push(json!({"text": std::mem::take(&mut self.text)}));
                self.has_text = false;
            }
        }
        fn flush_thought(&mut self) {
            if self.has_thought && (!self.thought.is_empty() || !self.thought_signature.is_empty())
            {
                let mut part = json!({"thought": true, "text": std::mem::take(&mut self.thought)});
                if !self.thought_signature.is_empty() {
                    part["thoughtSignature"] =
                        Value::from(std::mem::take(&mut self.thought_signature));
                }
                self.consolidated.push(part);
                self.has_thought = false;
            }
        }
    }

    let mut st = State {
        consolidated: Vec::new(),
        text: String::new(),
        thought: String::new(),
        thought_signature: String::new(),
        has_text: false,
        has_thought: false,
    };

    for part in parts {
        if !part.is_object() {
            st.flush_text();
            st.flush_thought();
            st.consolidated.push(part);
            continue;
        }
        if part.get("thought") == Some(&Value::Bool(true)) {
            st.flush_text();
            if let Some(Value::String(text)) = part.get("text") {
                st.thought.push_str(text);
                st.has_thought = true;
            }
            if let Some(Value::String(sig)) = part.get("thoughtSignature") {
                if !sig.is_empty() {
                    st.thought_signature = sig.clone();
                    st.has_thought = true;
                }
            }
        } else if let Some(Value::String(text)) = part.get("text") {
            st.flush_thought();
            st.text.push_str(text);
            st.has_text = true;
        } else {
            st.flush_text();
            st.flush_thought();
            st.consolidated.push(part);
        }
    }

    st.flush_thought();
    st.flush_text();
    st.consolidated
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn req(model: &str, raw: &str) -> Value {
        translate_request(model, &serde_json::from_str(raw).unwrap(), false)
    }

    fn frame(v: Value) -> String {
        format!("data: {v}\n\n")
    }

    const VALID_GEMINI_SIGNATURE: &str =
        "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

    // port of TestConvertGeminiRequestToClaude_ThinkingSummaryVisibility (claude_gemini_request_test.go)
    #[test]
    fn thinking_summary_visibility() {
        for (include, display) in [(true, "summarized"), (false, "omitted")] {
            let input = json!({
                "generationConfig": {"thinkingConfig": {"thinkingLevel": "high", "includeThoughts": include}},
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
            });
            let out = translate_request("claude-opus-5-5", &input, false);
            assert_eq!(
                out,
                json!({
                    "model": "claude-opus-5-5",
                    "max_tokens": 32000,
                    "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                    "metadata": {"user_id": sha256_hex("content:hi")},
                    "thinking": {"type": "enabled", "budget_tokens": 24576, "display": display},
                    "stream": false
                })
            );
        }
    }

    #[test]
    fn thinking_budget_and_level_mapping() {
        let cases = [
            (json!({"thinkingBudget": 0}), json!({"type": "disabled"})),
            (json!({"thinking_budget": -1}), json!({"type": "enabled"})),
            (
                json!({"thinkingBudget": 4096}),
                json!({"type": "enabled", "budget_tokens": 4096}),
            ),
            (
                json!({"thinkingLevel": "NONE"}),
                json!({"type": "disabled"}),
            ),
            (
                json!({"thinking_level": "auto"}),
                json!({"type": "enabled"}),
            ),
            (
                json!({"thinkingLevel": "low"}),
                json!({"type": "enabled", "budget_tokens": 1024}),
            ),
        ];
        for (cfg, want) in cases {
            let input = json!({"generationConfig": {"thinkingConfig": cfg}});
            let out = translate_request("claude-x", &input, true);
            assert_eq!(out["thinking"], want, "{cfg}");
            assert_eq!(out["stream"], json!(true));
        }
        // An unknown level leaves thinking absent.
        let out = translate_request(
            "claude-x",
            &json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "bogus"}}}),
            false,
        );
        assert_eq!(out.get("thinking"), None);
    }

    // port of TestConvertGeminiRequestToClaude_PreservesCustomToolIDs (claude_gemini_request_test.go)
    #[test]
    fn preserves_custom_tool_ids() {
        for (field, want) in [
            ("id", "call_gateway_id"),
            ("call_id", "call_gateway_call_id"),
        ] {
            let input = json!({"contents": [
                {"role": "model", "parts": [{"functionCall": {"name": "lookup", field: want, "args": {"query": "status"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "lookup", field: want, "response": {"result": "ok"}}}]}
            ]});
            let out = translate_request("claude-sonnet-4", &input, false);
            assert_eq!(
                out["messages"],
                json!([
                    {"role": "assistant", "content": [{"type": "tool_use", "id": want, "name": "lookup", "input": {"query": "status"}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": want, "content": "ok"}]}
                ])
            );
        }
    }

    // port of TestConvertGeminiRequestToClaude_GroupsConsecutiveRoleTurns (claude_gemini_request_test.go)
    #[test]
    fn groups_consecutive_role_turns() {
        let out = req(
            "claude-test",
            r#"{"contents":[
                {"role":"model","parts":[{"text":"answer"}]},
                {"role":"model","parts":[{"functionCall":{"name":"first","id":"call_1","args":{}}}]},
                {"role":"model","parts":[{"functionCall":{"name":"second","id":"call_2","args":{}}}]},
                {"role":"user","parts":[{"functionResponse":{"name":"first","id":"call_1","response":{"result":"one"}}}]},
                {"role":"user","parts":[{"functionResponse":{"name":"second","id":"call_2","response":{"result":"two"}}}]}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "call_1", "name": "first", "input": {}},
                    {"type": "tool_use", "id": "call_2", "name": "second", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "two"}
                ]}
            ])
        );
    }

    // port of TestConvertGeminiRequestToClaude_KeepsSystemInstructionUserSeparate (claude_gemini_request_test.go)
    #[test]
    fn keeps_system_instruction_user_separate() {
        let out = req(
            "claude-test",
            r#"{"system_instruction":{"parts":[{"text":"system rule"}]},
                "contents":[{"role":"user","parts":[{"text":"question"}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "system rule"}]},
                {"role": "user", "content": [{"type": "text", "text": "question"}]}
            ])
        );
    }

    // port of TestConvertGeminiRequestToClaude_DropsTemperature (claude_gemini_request_test.go)
    #[test]
    fn drops_temperature() {
        let out = req(
            "claude-sonnet-5",
            r#"{"generationConfig":{"temperature":0.2,"topP":0.8,"maxOutputTokens":1024,"stopSequences":["END"]},
                "service_tier":"auto",
                "contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
        );
        assert_eq!(
            out,
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "metadata": {"user_id": sha256_hex("content:hi")},
                "service_tier": "auto",
                "top_p": 0.8,
                "stop_sequences": ["END"],
                "stream": false
            })
        );
    }

    // port of TestConvertGeminiRequestToClaude_AcceptsCamelInlineData (claude_gemini_request_test.go)
    #[test]
    fn accepts_camel_inline_data() {
        let out = req(
            "claude-sonnet-4",
            r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
            ]}])
        );
    }

    // port of TestConvertGeminiRequestToClaude_SplitsNonImageInlineDataByMIME (claude_gemini_request_test.go)
    #[test]
    fn splits_non_image_inline_data_by_mime() {
        let out = req(
            "claude-sonnet-4",
            r#"{"contents":[{"role":"user","parts":[
                {"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}},
                {"inlineData":{"mimeType":"video/mp4","data":"AAAAIGZ0eXA="}},
                {"inline_data":{"mime_type":"application/pdf","data":"JVBERi0="}},
                {"fileData":{"fileUri":"gs://b/x.png","mimeType":"image/png"}},
                {"file_data":{"file_uri":"gs://b/y.txt","mime_type":"text/plain"}},
                {"fileData":{"fileUri":"gs://b/z.bin"}}
            ]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "Media content: inline data (Type: audio/wav)"},
                {"type": "text", "text": "Media content: inline data (Type: video/mp4)"},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="}},
                {"type": "image", "source": {"type": "url", "url": "gs://b/x.png"}},
                {"type": "document", "source": {"type": "url", "url": "gs://b/y.txt", "media_type": "text/plain"}},
                {"type": "text", "text": "File: gs://b/z.bin"}
            ]}])
        );
    }

    // port of TestConvertGeminiRequestToClaude_DropsHiddenThoughtParts (claude_gemini_request_test.go)
    #[test]
    fn drops_hidden_thought_parts() {
        let out = req(
            "claude-test",
            r#"{"contents":[
                {"role":"model","parts":[{"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"}]},
                {"role":"user","parts":[{"text":"continue"}]}
            ]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "continue"}]}])
        );

        let out = req(
            "claude-test",
            r#"{"contents":[{"role":"model","parts":[
                {"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"},
                {"text":"visible answer"}
            ]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "assistant", "content": [{"type": "text", "text": "visible answer"}]}])
        );
    }

    // port of TestConvertGeminiRequestToClaude_DeterministicToolIDs (claude_gemini_request_test.go)
    #[test]
    fn deterministic_tool_ids() {
        let raw = r#"{"contents":[
            {"role":"model","parts":[{"functionCall":{"name":"first_tool","args":{"q":"one"}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"first_tool","response":{"result":"ok1"}}}]},
            {"role":"model","parts":[{"functionCall":{"name":"second_tool","args":{"q":"two"}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"second_tool","response":{"result":"ok2"}}}]}
        ]}"#;
        let out1 = req("claude-sonnet-4", raw);
        let out2 = req("claude-sonnet-4", raw);
        assert_eq!(out1, out2);
        let id1 = "toolu_gemini_0000000000000001";
        let id2 = "toolu_gemini_0000000000000002";
        assert_eq!(
            out1["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "tool_use", "id": id1, "name": "first_tool", "input": {"q": "one"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id1, "content": "ok1"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": id2, "name": "second_tool", "input": {"q": "two"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id2, "content": "ok2"}]}
            ])
        );
    }

    #[test]
    fn function_response_without_result_uses_raw_response() {
        let out = req(
            "claude-test",
            r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"a":1}}}]}]}"#,
        );
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_gemini_0000000000000001", "content": "{\"a\":1}"}
            ]}])
        );
    }

    // port of TestConvertGeminiRequestToClaude_PreservesCallerSuppliedMetadataUserID (claude_gemini_request_test.go)
    #[test]
    fn preserves_caller_supplied_metadata_user_id() {
        let cases = [
            "custom-gemini-user-123",
            "foo\"bar\nbaz\\qux",
            r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#,
        ];
        for uid in cases {
            let input = json!({
                "model": "claude-test",
                "metadata": {"user_id": uid},
                "contents": [{"role": "user", "parts": [{"text": "hello"}]}]
            });
            let out = translate_request("claude-test", &input, false);
            assert_eq!(out["metadata"], json!({"user_id": uid}));
        }
    }

    // port of TestConvertGeminiRequestToClaude_DifferentSessionsProduceDifferentUserIDs (claude_gemini_request_test.go)
    #[test]
    fn different_sessions_produce_different_user_ids() {
        let a = req(
            "claude-test",
            r#"{"model":"claude-test","prompt_cache_key":"gemini-session-a","contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
        );
        let b = req(
            "claude-test",
            r#"{"model":"claude-test","prompt_cache_key":"gemini-session-b","contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
        );
        assert_eq!(
            a["metadata"]["user_id"],
            json!(sha256_hex("prompt_cache_key:gemini-session-a"))
        );
        assert_eq!(
            b["metadata"]["user_id"],
            json!(sha256_hex("prompt_cache_key:gemini-session-b"))
        );
        assert_ne!(a["metadata"]["user_id"], b["metadata"]["user_id"]);
    }

    // port of TestConvertGeminiRequestToClaude_DefaultRoleDifferentContentProducesDifferentUserIDs (claude_gemini_request_test.go)
    #[test]
    fn default_role_different_content_produces_different_user_ids() {
        let a = req(
            "claude-test",
            r#"{"contents":[{"parts":[{"text":"first prompt"}]}]}"#,
        );
        let b = req(
            "claude-test",
            r#"{"contents":[{"parts":[{"text":"second prompt"}]}]}"#,
        );
        assert_eq!(
            a["metadata"]["user_id"],
            json!(sha256_hex("content:first prompt"))
        );
        assert_eq!(
            b["metadata"]["user_id"],
            json!(sha256_hex("content:second prompt"))
        );
        // A role-less content is dropped by the accumulator, as in Go.
        assert_eq!(a["messages"], json!([]));
    }

    // port of TestConvertGeminiRequestToClaude_SanitizesToolNamesAndProvidesFallbackSchema (claude_gemini_request_test.go)
    #[test]
    fn sanitizes_tool_names_and_provides_fallback_schema() {
        let out = req(
            "claude-test",
            r#"{
                "contents": [
                    {"role": "model", "parts": [{"functionCall": {"name": "mcp.server:get_data", "args": {}}}]},
                    {"role": "user", "parts": [{"functionResponse": {"name": "mcp.server:get_data", "response": {"result": "ok"}}}]}
                ],
                "tools": [{"functionDeclarations": [{"name": "mcp.server:get_data", "description": "parameterless mcp tool"}]}],
                "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["mcp.server:get_data"]}}
            }"#,
        );
        let id = "toolu_gemini_0000000000000001";
        assert_eq!(
            out,
            json!({
                "model": "claude-test",
                "max_tokens": 32000,
                "messages": [
                    {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "mcp_server_get_data", "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]}
                ],
                "metadata": {"user_id": "unknown"},
                "tools": [{"name": "mcp_server_get_data", "description": "parameterless mcp tool", "input_schema": {"type": "object", "properties": {}}}],
                "tool_choice": {"type": "tool", "name": "mcp_server_get_data"},
                "stream": false
            })
        );
    }

    #[test]
    fn tool_declarations_and_tool_choice_modes() {
        let out = req(
            "claude-test",
            r#"{"tools":[{"functionDeclarations":[
                    {"name":"a","parameters":{"type":"OBJECT","properties":{"q":{"type":"STRING"}}}},
                    {"name":"b","parametersJsonSchema":{"type":"object","additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"}}
                ]}],
                "tool_config":{"function_calling_config":{"mode":"ANY","allowed_function_names":["a","b"]}}}"#,
        );
        assert_eq!(
            out["tools"],
            json!([
                {"name": "a", "description": "", "input_schema": {
                    "type": "object", "properties": {"q": {"type": "string"}},
                    "additionalProperties": false, "$schema": "http://json-schema.org/draft-07/schema#"}},
                {"name": "b", "description": "", "input_schema": {
                    "type": "object", "additionalProperties": false, "$schema": "http://json-schema.org/draft-07/schema#"}}
            ])
        );
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
        for (mode, want) in [
            ("AUTO", json!({"type": "auto"})),
            ("NONE", json!({"type": "none"})),
        ] {
            let out = translate_request(
                "claude-test",
                &json!({"toolConfig": {"functionCallingConfig": {"mode": mode}}}),
                false,
            );
            assert_eq!(out["tool_choice"], want);
        }
    }

    // port of TestNormalizeClaudeToolSchemaPreservesCanonicalSchema (noop_optimization_test.go)
    #[test]
    fn normalize_schema_preserves_canonical_schema() {
        let input = json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"});
        let out = normalize_claude_tool_schema(&input);
        assert_eq!(out, input);
        assert_eq!(out.to_string(), input.to_string());
    }

    // port of TestNormalizeClaudeToolSchemaCorrectsWrongTypes (noop_optimization_test.go)
    #[test]
    fn normalize_schema_corrects_wrong_types() {
        let out = normalize_claude_tool_schema(
            &json!({"type":"object","additionalProperties":"false","$schema":123}),
        );
        assert_eq!(
            out,
            json!({"type":"object","additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"})
        );
    }

    // port of TestLowercaseClaudeToolSchemaTypesReusesLowercaseSchema (noop_optimization_test.go)
    #[test]
    fn lowercase_types_leaves_lowercase_schema_unchanged() {
        let input = json!({"name":"lookup","input_schema":{"type":"object","properties":{"value":{"type":"string"}}}});
        let mut out = input.clone();
        lowercase_claude_tool_schema_types(&mut out);
        assert_eq!(out, input);
    }

    // port of TestLowercaseClaudeToolSchemaTypesNormalizesNonStringType (noop_optimization_test.go)
    #[test]
    fn lowercase_types_normalizes_non_string_type() {
        let mut out = json!({"input_schema":{"type":123}});
        lowercase_claude_tool_schema_types(&mut out);
        assert_eq!(out, json!({"input_schema":{"type":"123"}}));
    }

    // port of TestLowercaseClaudeToolSchemaTypesNormalizesUppercaseTypes (noop_optimization_test.go)
    #[test]
    fn lowercase_types_normalizes_uppercase_types() {
        let mut out =
            json!({"input_schema":{"type":"OBJECT","properties":{"value":{"type":"STRING"}}}});
        lowercase_claude_tool_schema_types(&mut out);
        assert_eq!(
            out,
            json!({"input_schema":{"type":"object","properties":{"value":{"type":"string"}}}})
        );
    }

    #[test]
    fn lowercase_types_keeps_a_property_named_type() {
        let mut out =
            json!({"input_schema":{"type":"OBJECT","properties":{"type":{"type":"STRING"}}}});
        lowercase_claude_tool_schema_types(&mut out);
        assert_eq!(
            out,
            json!({"input_schema":{"type":"object","properties":{"type":{"type":"string"}}}})
        );
    }

    // port of TestConvertClaudeResponseToGemini_StreamPreservesToolUseID (claude_gemini_response_test.go)
    #[test]
    fn stream_preserves_tool_use_id() {
        let mut tr = StreamTranslator::new(&json!({}));
        tr.created_at = 1_700_000_000;
        assert!(tr
            .push(
                None,
                &json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_gateway","name":"lookup"}})
            )
            .is_empty());
        assert!(tr
            .push(
                None,
                &json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"status\"}"}})
            )
            .is_empty());
        let out = tr.push(None, &json!({"type":"content_block_stop","index":0}));
        assert_eq!(
            out,
            vec![frame(json!({
                "candidates": [{"content": {"role": "model", "parts": [
                    {"functionCall": {"name": "lookup", "args": {"query": "status"}, "id": "toolu_gateway"}}
                ]}, "finishReason": "STOP"}],
                "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
                "modelVersion": "",
                "createTime": format_create_time(1_700_000_000),
                "responseId": ""
            }))]
        );
    }

    // port of TestConvertClaudeResponseToGeminiNonStreamPreservesToolUseID (claude_gemini_response_test.go)
    #[test]
    fn non_stream_preserves_tool_use_id() {
        let raw = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_gateway","name":"lookup"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"status\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
        ]
        .join("\n");
        let out = translate_non_stream(&Value::from(raw), &json!({"model": "gemini-2.5-pro"}));
        assert_eq!(
            out,
            json!({
                "candidates": [{"content": {"role": "model", "parts": [
                    {"functionCall": {"name": "lookup", "args": {"query": "status"}, "id": "toolu_gateway"}}
                ]}, "finishReason": "STOP"}],
                "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
                "modelVersion": "gemini-2.5-pro",
                "createTime": "",
                "responseId": ""
            })
        );
    }

    fn thinking_signature_cases() -> Vec<(String, &'static str)> {
        vec![
            (
                "foreign_claude_sig_123".to_string(),
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
            ),
            (
                format!("gemini#{VALID_GEMINI_SIGNATURE}"),
                VALID_GEMINI_SIGNATURE,
            ),
        ]
    }

    // port of TestConvertClaudeResponseToGemini_StreamThinkingSignature (claude_gemini_response_test.go)
    #[test]
    fn stream_thinking_signature() {
        for (sig, want) in thinking_signature_cases() {
            let mut tr = StreamTranslator::new(&json!({}));
            let chunks = [
                json!({"type":"message_start","message":{"id":"msg_123","model":"claude-3-7-sonnet-20250219"}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thinking text"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":sig}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"final answer"}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_stop"}),
            ];
            let mut parts: Vec<Value> = Vec::new();
            for chunk in &chunks {
                for f in tr.convert(chunk) {
                    parts.extend(
                        f["candidates"][0]["content"]["parts"]
                            .as_array()
                            .unwrap()
                            .clone(),
                    );
                }
            }
            assert_eq!(
                parts,
                vec![
                    json!({"thought": true, "text": "thinking text"}),
                    json!({"thought": true, "thoughtSignature": want}),
                    json!({"text": "final answer"}),
                ],
                "{sig}"
            );
        }
    }

    // port of TestConvertClaudeResponseToGeminiNonStream_ThinkingSignature (claude_gemini_response_test.go)
    #[test]
    fn non_stream_thinking_signature() {
        for (sig, want) in thinking_signature_cases() {
            let raw = [
                r#"data: {"type":"message_start","message":{"id":"msg_123","model":"claude-3-7-sonnet-20250219"}}"#.to_string(),
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#.to_string(),
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thinking text"}}"#.to_string(),
                format!(r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"signature_delta","signature":"{sig}"}}}}"#),
                r#"data: {"type":"content_block_stop","index":0}"#.to_string(),
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#.to_string(),
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"final answer"}}"#.to_string(),
                r#"data: {"type":"content_block_stop","index":1}"#.to_string(),
                r#"data: {"type":"message_stop"}"#.to_string(),
            ]
            .join("\n");
            let out = translate_non_stream(&Value::from(raw), &json!({"model": "gemini-2.5-pro"}));
            assert_eq!(
                out["candidates"],
                json!([{"content": {"role": "model", "parts": [
                    {"thought": true, "text": "thinking text", "thoughtSignature": want},
                    {"text": "final answer"}
                ]}, "finishReason": "STOP"}]),
                "{sig}"
            );
            assert_eq!(out["responseId"], json!("msg_123"));
            assert_eq!(out["modelVersion"], json!("gemini-2.5-pro"));
        }
    }

    fn gemini_envelope(inner: &[u8]) -> String {
        // field 2 (bytes) → field 1 (bytes) → inner
        let mut container = vec![0x0a, inner.len() as u8];
        container.extend_from_slice(inner);
        let mut outer = vec![0x12, container.len() as u8];
        outer.extend_from_slice(&container);
        STANDARD.encode(outer)
    }

    #[test]
    fn gemini_signature_replay_policy() {
        let tink = gemini_envelope(&[0x01, 0x0c, 0x39, 0xd6, 0xc7, 0xaa]);
        let tool = gemini_envelope(&[0x08, 0x01, 0x12, 0x02, 0x01, 0xff]);
        let uuid = gemini_envelope(b"123e4567-e89b-12d3-a456-426614174000");
        let bare_uuid = STANDARD.encode(b"123e4567-e89b-12d3-a456-426614174000");
        let not_tink = gemini_envelope(&[0x02, 0x00]);
        let cases: Vec<(String, String)> = vec![
            (tink.clone(), tink.clone()),
            (format!(" google#{tink} "), tink.clone()),
            (tool.clone(), tool.clone()),
            (uuid.clone(), uuid.clone()),
            (bare_uuid, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into()),
            (not_tink, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into()),
            (
                format!("claude#{tink}"),
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into(),
            ),
            (
                format!("other#{tink}"),
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into(),
            ),
            (
                GEMINI_CONTEXT_ENGINEERING_BYPASS.into(),
                GEMINI_CONTEXT_ENGINEERING_BYPASS.into(),
            ),
            (
                format!("gemini#{GEMINI_CONTEXT_ENGINEERING_BYPASS}"),
                GEMINI_CONTEXT_ENGINEERING_BYPASS.into(),
            ),
            (
                "sealed.v1.abc".into(),
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into(),
            ),
            (VALID_GEMINI_SIGNATURE.into(), VALID_GEMINI_SIGNATURE.into()),
        ];
        for (sig, want) in cases {
            assert_eq!(gemini_replay_signature_or_bypass(&sig), want, "{sig}");
        }
    }

    #[test]
    fn stream_error_and_message_delta_usage() {
        let mut tr = StreamTranslator::new(&json!({}));
        tr.created_at = 1_700_000_000;
        assert_eq!(
            tr.push(
                Some("error"),
                &json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})
            ),
            vec![frame(
                json!({"error": {"code": 400, "message": "Overloaded", "status": "INVALID_ARGUMENT"}})
            )]
        );
        let out = tr.push(
            None,
            &json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},
                    "usage":{"input_tokens":5,"output_tokens":7,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"thinking_tokens":4}}),
        );
        assert_eq!(
            out,
            vec![frame(json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                "usageMetadata": {
                    "trafficType": "PROVISIONED_THROUGHPUT",
                    "promptTokenCount": 5,
                    "candidatesTokenCount": 7,
                    "totalTokenCount": 12,
                    "cachedContentTokenCount": 5,
                    "thoughtsTokenCount": 4
                },
                "modelVersion": "",
                "createTime": format_create_time(1_700_000_000),
                "responseId": ""
            }))]
        );
        assert!(tr.finish().is_empty());
    }

    /// End to end: a realistic Anthropic stream with thinking, text and a tool
    /// call, checked frame by frame.
    #[test]
    fn end_to_end_stream() {
        let mut tr = StreamTranslator::new(&json!({"contents": []}));
        tr.created_at = 1_700_000_000;
        let ct = format_create_time(1_700_000_000);
        let events: Vec<(&str, Value)> = vec![
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            ),
            ("ping", json!({"type":"ping"})),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need weather."}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCkYIBxgCKkBclaude"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Checking Paris."}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01A","name":"get_weather","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\": "}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":42}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let mut frames: Vec<String> = Vec::new();
        for (ev, data) in &events {
            frames.extend(tr.push(Some(ev), data));
        }
        frames.extend(tr.finish());

        let chunk = |parts: Value, finish: Option<&str>, usage: Value| {
            let mut candidate = json!({"content": {"role": "model", "parts": parts}});
            if let Some(f) = finish {
                candidate["finishReason"] = json!(f);
            }
            frame(json!({
                "candidates": [candidate],
                "usageMetadata": usage,
                "modelVersion": "claude-sonnet-4-5",
                "createTime": ct,
                "responseId": "msg_01"
            }))
        };
        let traffic = json!({"trafficType": "PROVISIONED_THROUGHPUT"});
        assert_eq!(
            frames,
            vec![
                chunk(
                    json!([{"thought": true, "text": "Need weather."}]),
                    None,
                    traffic.clone()
                ),
                chunk(
                    json!([{"thought": true, "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}]),
                    None,
                    traffic.clone()
                ),
                chunk(json!([{"text": "Checking Paris."}]), None, traffic.clone()),
                chunk(
                    json!([{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "toolu_01A"}}]),
                    Some("STOP"),
                    traffic.clone()
                ),
                chunk(
                    json!([]),
                    Some("STOP"),
                    json!({
                        "trafficType": "PROVISIONED_THROUGHPUT",
                        "promptTokenCount": 0,
                        "candidatesTokenCount": 42,
                        "totalTokenCount": 42
                    })
                ),
            ]
        );
    }

    #[test]
    fn non_stream_from_messages_object() {
        let upstream = json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "thinking", "thinking": "Plan.", "signature": "abc"},
                {"type": "text", "text": "Calling."},
                {"type": "tool_use", "id": "toolu_02", "name": "lookup", "input": {"q": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 8, "cache_read_input_tokens": 4}
        });
        let mut out = translate_non_stream(&upstream, &json!({"contents": []}));
        assert!(!out["createTime"].as_str().unwrap().is_empty());
        out["createTime"] = json!("");
        assert_eq!(
            out,
            json!({
                "candidates": [{"content": {"role": "model", "parts": [
                    {"thought": true, "text": "Plan.", "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR},
                    {"text": "Calling."},
                    {"functionCall": {"name": "lookup", "args": {"q": 1}, "id": "toolu_02"}}
                ]}, "finishReason": "STOP"}],
                "usageMetadata": {
                    "promptTokenCount": 20,
                    "candidatesTokenCount": 8,
                    "totalTokenCount": 28,
                    "cachedContentTokenCount": 4,
                    "trafficType": "PROVISIONED_THROUGHPUT"
                },
                "modelVersion": "claude-sonnet-4-5",
                "createTime": "",
                "responseId": "msg_02"
            })
        );
    }

    #[test]
    fn consolidate_parts_merges_runs() {
        let parts = vec![
            json!({"text": "a"}),
            json!({"text": "b"}),
            json!({"thought": true, "text": "x"}),
            json!({"thought": true, "thoughtSignature": "s"}),
            json!({"functionCall": {"name": "f", "args": {}}}),
            json!({"text": "c"}),
        ];
        assert_eq!(
            consolidate_parts(parts),
            vec![
                json!({"text": "ab"}),
                json!({"thought": true, "text": "x", "thoughtSignature": "s"}),
                json!({"functionCall": {"name": "f", "args": {}}}),
                json!({"text": "c"}),
            ]
        );
    }
}
