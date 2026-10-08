//! Anthropic Messages (client) ⇄ OpenAI Responses (upstream) translation.
//!
//! A faithful port of CLIProxyAPI's `internal/translator/codex/claude` pair
//! (commit ed980be), registered upstream as
//! `translator.Register(Claude, Codex, ConvertClaudeRequestToCodex,
//! {Stream: ConvertCodexResponseToClaude, NonStream: ConvertCodexResponseToClaudeNonStream})`.
//! The CLIENT speaks Anthropic Messages (`/v1/messages`); the UPSTREAM speaks
//! OpenAI Responses (the ChatGPT Codex backend).
//!
//! Each ported function carries a `// port of <GoFunc> (<file>)` comment so it
//! can be diffed against the Go source. Helpers the pair reaches into other
//! packages for (`internal/util`, `internal/thinking`, `internal/signature`,
//! `internal/translator/common`) are ported in the same way, as far as the pair
//! needs them.
//!
//! Go reads the body with gjson, whose accessors never fail: a missing path is
//! the empty string / zero / false. The `gp` / `gs` / `gi` / `gb` helpers below
//! reproduce those semantics so the ported branches read like the original.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

// ───────────────────────────── gjson-like accessors ─────────────────────────────

/// Dotted-path lookup (`a.b.0.c`). `Some(Value::Null)` is an EXISTING null,
/// like gjson's `Exists()` on a JSON null.
fn gp<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
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

/// gjson `Result.String()`: strings verbatim, null/missing empty, everything
/// else its JSON text.
fn gs(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

/// `gs(gp(v, path))`.
fn gstr(v: &Value, path: &str) -> String {
    gs(gp(v, path))
}

/// gjson `Result.Int()`.
fn gi(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i
            } else if n.as_u64().is_some() {
                i64::MAX
            } else {
                n.as_f64().map(|f| f as i64).unwrap_or(0)
            }
        }
        Some(Value::String(s)) => {
            let t = s.trim();
            t.parse::<i64>()
                .ok()
                .or_else(|| t.parse::<f64>().ok().map(|f| f as i64))
                .unwrap_or(0)
        }
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// gjson `Result.Bool()`.
fn gb(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "t" | "T" | "true" | "TRUE" | "True"),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

/// Byte-prefix of `s` no longer than `n`, backed off to a char boundary (Go
/// slices bytes; Rust must not split a code point).
fn prefix_bytes(s: &str, n: usize) -> &str {
    if n >= s.len() {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One Anthropic SSE frame.
// port of AppendSSEEventBytes (internal/translator/common/bytes.go), trailingNewlines = 2
fn sse(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Module-private id source (time + atomic counter), used where Go generates one.
fn generated_suffix() -> (u128, u64) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (nanos, ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1)
}

// ─────────────────────────── internal/util helpers ───────────────────────────

const CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

// port of IsClaudeCodeAttributionSystemText (internal/util/claude_attribution.go)
fn is_claude_code_attribution_system_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX)
}

// port of SanitizeClaudeToolID (internal/util/claude_tool_id.go)
fn sanitize_claude_tool_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        let (nanos, n) = generated_suffix();
        return format!("toolu_{nanos}_{n}");
    }
    s
}

// port of HasUnsupportedUnicodePropertyEscape (internal/util/claude_schema.go)
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
        i += 2; // skip the escaped character (including escaped backslash)
    }
    false
}

// port of SchemaMapKeywords (internal/util/claude_schema.go)
const SCHEMA_MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

// port of SchemaValueKeywords (internal/util/claude_schema.go)
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

// ────────────────────────── internal/thinking helpers ──────────────────────────

// port of ParseSuffix (internal/thinking/suffix.go) — base model name only; the
// `model(level)` suffix itself is not interpreted here.
fn parse_suffix_model_name(model: &str) -> &str {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => &model[..open],
        _ => model,
    }
}

// port of ConvertBudgetToLevel (internal/thinking/convert.go)
fn convert_budget_to_level(budget: i64) -> Option<&'static str> {
    const THRESHOLD_MINIMAL: i64 = 512;
    const THRESHOLD_LOW: i64 = 1024;
    const THRESHOLD_MEDIUM: i64 = 8192;
    const THRESHOLD_HIGH: i64 = 24576;
    match budget {
        b if b < -1 => None,
        -1 => Some("auto"),
        0 => Some("none"),
        b if b <= THRESHOLD_MINIMAL => Some("minimal"),
        b if b <= THRESHOLD_LOW => Some("low"),
        b if b <= THRESHOLD_MEDIUM => Some("medium"),
        b if b <= THRESHOLD_HIGH => Some("high"),
        _ => Some("xhigh"),
    }
}

// ──────────────────────── internal/translator/common ────────────────────────

const CLAUDE_SYSTEM_REMINDER_START: &str = "<system-reminder>";
const CLAUDE_SYSTEM_REMINDER_END: &str = "</system-reminder>";

// port of SystemReminderText (internal/translator/common/claude_system.go)
fn system_reminder_text(text: &str) -> String {
    format!("{CLAUDE_SYSTEM_REMINDER_START}\n{text}\n{CLAUDE_SYSTEM_REMINDER_END}")
}

// port of claudeSystemTextParts (internal/translator/common/claude_system.go)
fn claude_system_text_parts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => {
            if text.is_empty() || is_claude_code_attribution_system_text(text) {
                vec![]
            } else {
                vec![text.clone()]
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| gstr(item, "type") == "text")
            .map(|item| gstr(item, "text"))
            .filter(|text| !text.is_empty() && !is_claude_code_attribution_system_text(text))
            .collect(),
        _ => vec![],
    }
}

// port of ClaudeMessageSystemReminderText (internal/translator/common/claude_system.go)
fn claude_message_system_reminder_text(content: Option<&Value>) -> Option<String> {
    let parts = claude_system_text_parts(content);
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(system_reminder_text(&text))
}

// port of AlignClaudeToolResults (internal/translator/common/claude_messages.go)
fn align_claude_tool_results(content: Value, tool_use_ids: &[String]) -> Value {
    let parts = match &content {
        Value::Array(parts) if !tool_use_ids.is_empty() => parts,
        _ => return content,
    };
    let mut results: Vec<&Value> = Vec::new();
    let mut result_indices: Vec<usize> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if gstr(part, "type") == "tool_result" {
            results.push(part);
            result_indices.push(i);
        }
    }
    if results.len() != tool_use_ids.len() {
        return content;
    }
    let mut used = vec![false; results.len()];
    let mut reordered: Vec<&Value> = Vec::with_capacity(tool_use_ids.len());
    for id in tool_use_ids {
        let matched = results
            .iter()
            .enumerate()
            .find(|(ri, r)| !used[*ri] && !id.is_empty() && gstr(r, "tool_use_id") == *id)
            .map(|(ri, _)| ri);
        match matched {
            Some(ri) => {
                used[ri] = true;
                reordered.push(results[ri]);
            }
            None => return content,
        }
    }
    let mut ordered: Vec<Value> = parts.clone();
    for (i, slot) in result_indices.iter().enumerate() {
        ordered[*slot] = reordered[i].clone();
    }
    Value::Array(ordered)
}

// ─────────────────────────── internal/signature ───────────────────────────

/// Provider families named by this repo's `provider#payload` cache prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SigProvider {
    Claude,
    Gemini,
    Gpt,
    Swe,
}

// port of SignatureProviderFromCachePrefix (internal/signature/provider_compatibility.go)
fn signature_provider_from_cache_prefix(prefix: &str) -> Option<SigProvider> {
    match prefix.trim().to_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
        | "claude-code-max" | "claude_code_max" => Some(SigProvider::Claude),
        "gemini" | "google" => Some(SigProvider::Gemini),
        "openai" | "gpt" | "codex" => Some(SigProvider::Gpt),
        "swe" | "sealed" => Some(SigProvider::Swe),
        _ => None,
    }
}

// port of SplitSignatureProviderPrefix (internal/signature/provider_compatibility.go)
fn split_signature_provider_prefix(raw: &str) -> Option<(SigProvider, String)> {
    let (prefix, rest) = raw.trim().split_once('#')?;
    let provider = signature_provider_from_cache_prefix(prefix)?;
    Some((provider, rest.trim().to_string()))
}

const fn lenient(alpha: &'static alphabet::Alphabet, padding: DecodePaddingMode) -> GeneralPurpose {
    GeneralPurpose::new(
        alpha,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(padding)
            .with_decode_allow_trailing_bits(true),
    )
}
/// Go `base64.RawURLEncoding` (non-strict: trailing bits tolerated).
const B64_RAW_URL: GeneralPurpose = lenient(&alphabet::URL_SAFE, DecodePaddingMode::RequireNone);
/// Go `base64.URLEncoding`.
const B64_URL: GeneralPurpose = lenient(&alphabet::URL_SAFE, DecodePaddingMode::RequireCanonical);
/// Go `base64.RawStdEncoding`.
const B64_RAW_STD: GeneralPurpose = lenient(&alphabet::STANDARD, DecodePaddingMode::RequireNone);
/// Standard alphabet, padding optional.
const B64_STD_ANY: GeneralPurpose = lenient(&alphabet::STANDARD, DecodePaddingMode::Indifferent);

const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

// port of IsValidGPTReasoningSignature / InspectGPTReasoningSignature (internal/signature/gpt_validation.go)
fn is_valid_gpt_reasoning_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return false;
    }
    if !sig.starts_with("gAAAA") {
        return false;
    }
    if !sig
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'=')
    {
        return false;
    }
    let decoded = match B64_RAW_URL.decode(sig).or_else(|_| B64_URL.decode(sig)) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if decoded.len() < 73 || decoded[0] != 0x80 {
        return false;
    }
    let ciphertext_len = decoded.len() as i64 - 1 - 8 - 16 - 32;
    ciphertext_len > 0 && ciphertext_len % 16 == 0
}

// port of CompatibleSignatureForProvider(SignatureProviderGPT, raw)
// (internal/signature/provider_compatibility.go). For a GPT target only the GPT
// probe of DetectSignatureProviderForBlock can match, so the Claude / Gemini /
// Kimi probes it also runs are irrelevant to the outcome and are not ported.
fn compatible_gpt_signature(raw: &str) -> Option<String> {
    let sig = raw.trim();
    if sig.is_empty() {
        return None;
    }
    if let Some((provider, unprefixed)) = split_signature_provider_prefix(sig) {
        if provider == SigProvider::Gpt && is_valid_gpt_reasoning_signature(&unprefixed) {
            return Some(unprefixed);
        }
        return None;
    }
    if sig.contains('#') {
        return None;
    }
    if is_valid_gpt_reasoning_signature(sig) {
        return Some(sig.to_string());
    }
    None
}

// port of maybeSelfDescribingSignatureEnvelope (internal/signature/provider_compatibility.go)
fn maybe_self_describing_signature_envelope(sig: &str) -> bool {
    matches!(sig.as_bytes().first(), Some(b'C' | b'E' | b'R' | b'g'))
}

/// APPROXIMATION of the Claude (strict single/double-layer, CAIS) and Gemini
/// (known-envelope) validators in internal/signature. Those are full protobuf
/// walkers; the Grok replay check only uses them to REJECT a foreign envelope,
/// so this keeps the side that matters: it flags any blob whose first decoded
/// byte is one of the envelope markers (0x08 CAIS, 0x12 single-layer / Gemini
/// field 2, 0x45 double-layer `R`). It is stricter than Go — a genuine Grok
/// blob that happens to start with one of those bytes is dropped from replay
/// (losing reasoning context) rather than risking an upstream "Could not
/// decrypt" 400.
fn looks_like_claude_or_gemini_envelope(sig: &str) -> bool {
    match B64_STD_ANY.decode(sig) {
        Ok(decoded) => matches!(decoded.first(), Some(0x08 | 0x12 | 0x45)),
        Err(_) => false,
    }
}

// port of byteEntropyRatio (internal/signature/grok_validation.go)
fn byte_entropy_ratio(buf: &[u8]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in buf {
        counts[b as usize] += 1;
    }
    let n = buf.len() as f64;
    let mut entropy = 0.0;
    for &count in counts.iter() {
        if count == 0 {
            continue;
        }
        let p = count as f64 / n;
        entropy -= p * p.log2();
    }
    let max_symbols = buf.len().min(256);
    if max_symbols <= 1 {
        return 0.0;
    }
    entropy / (max_symbols as f64).log2()
}

fn is_std_base64_charset(sig: &str) -> bool {
    sig.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
}

const KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN: usize = 12946;
const KIMI_THINKING_SIGNATURE_STREAMING_LEN: usize = 4340;
const MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO: f64 = 0.85;

// port of IsValidKimiThinkingSignature / InspectKimiThinkingSignature (internal/signature/kimi_validation.go)
fn is_valid_kimi_thinking_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig != raw {
        return false;
    }
    if sig.len() != KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN
        && sig.len() != KIMI_THINKING_SIGNATURE_STREAMING_LEN
    {
        return false;
    }
    if sig.contains('=') || !is_std_base64_charset(sig) {
        return false;
    }
    if split_signature_provider_prefix(sig).is_some() {
        return false;
    }
    if maybe_self_describing_signature_envelope(sig)
        && (sig.starts_with("gAAAA") || looks_like_claude_or_gemini_envelope(sig))
    {
        return false;
    }
    match B64_RAW_STD.decode(sig) {
        Ok(decoded) => byte_entropy_ratio(&decoded) >= MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO,
        Err(_) => false,
    }
}

const MAX_GROK_ENCRYPTED_CONTENT_LEN: usize = 8 * 1024 * 1024;
const MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN: usize = 32;
const MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO: f64 = 0.85;

// port of InspectGrokEncryptedContent (internal/signature/grok_validation.go)
fn is_valid_grok_encrypted_content(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig.len() > MAX_GROK_ENCRYPTED_CONTENT_LEN || sig != raw {
        return false;
    }
    if sig.contains('=') || !is_std_base64_charset(sig) {
        return false;
    }
    if split_signature_provider_prefix(sig).is_some() {
        return false;
    }
    if maybe_self_describing_signature_envelope(sig)
        && (sig.starts_with("gAAAA") || looks_like_claude_or_gemini_envelope(sig))
    {
        return false;
    }
    if is_valid_kimi_thinking_signature(sig) {
        return false;
    }
    let decoded = match B64_RAW_STD.decode(sig) {
        Ok(d) => d,
        Err(_) => return false,
    };
    decoded.len() >= MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN
        && byte_entropy_ratio(&decoded) >= MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO
}

// ───────────────────────────── request translation ─────────────────────────────

// port of codexClaudeTargetAcceptsGrokSignature (codex_claude_request.go)
fn codex_claude_target_accepts_grok_signature(model_name: &str) -> bool {
    parse_suffix_model_name(model_name)
        .trim()
        .to_lowercase()
        .contains("grok")
}

// port of normalizeCodexServiceTier (codex_claude_request.go)
fn normalize_codex_service_tier(result: Option<&Value>) -> &'static str {
    match result {
        Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "fast" | "priority" => "priority",
            _ => "",
        },
        _ => "",
    }
}

// port of shortenCodexCallIDIfNeeded (codex_claude_request.go)
fn shorten_codex_call_id_if_needed(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    let sum = Sha256::digest(id.as_bytes());
    let suffix: String = std::iter::once("_".to_string())
        .chain(sum[..8].iter().map(|b| format!("{b:02x}")))
        .collect();
    let prefix_len = LIMIT - suffix.len();
    format!("{}{}", prefix_bytes(id, prefix_len), suffix)
}

// port of isClaudeWebSearchToolType (codex_claude_request.go)
fn is_claude_web_search_tool_type(tool_type: &str) -> bool {
    tool_type == "web_search_20250305" || tool_type == "web_search_20260209"
}

// port of buildClaudeWebSearchToolNameSet (codex_claude_request.go)
fn build_claude_web_search_tool_name_set(tools: &[Value]) -> HashSet<String> {
    tools
        .iter()
        .filter(|tool| is_claude_web_search_tool_type(&gstr(tool, "type")))
        .map(|tool| gstr(tool, "name"))
        .filter(|name| !name.is_empty())
        .collect()
}

// port of convertClaudeToolChoiceToCodex (codex_claude_request.go)
fn convert_claude_tool_choice_to_codex(
    tool_choice: Option<&Value>,
    tool_name_map: &HashMap<String, String>,
    web_search_tool_names: &HashSet<String>,
) -> Value {
    let tool_choice = match tool_choice {
        None | Some(Value::Null) => return json!("auto"),
        Some(tc) => tc,
    };
    let mut choice_type = gstr(tool_choice, "type");
    if choice_type.is_empty() {
        if let Value::String(s) = tool_choice {
            choice_type = s.clone();
        }
    }
    match choice_type.as_str() {
        "auto" | "" => json!("auto"),
        "any" => json!("required"),
        "none" => json!("none"),
        "tool" => {
            let mut name = gstr(tool_choice, "name");
            if web_search_tool_names.contains(&name) {
                return json!({"type": "web_search"});
            }
            name = match tool_name_map.get(&name) {
                Some(short) => short.clone(),
                None => shorten_name_if_needed(&name),
            };
            if name.is_empty() {
                return json!("auto");
            }
            json!({"type": "function", "name": name})
        }
        _ => json!("auto"),
    }
}

// port of convertClaudeWebSearchToolToCodex (codex_claude_request.go)
fn convert_claude_web_search_tool_to_codex(tool: &Value) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), json!("web_search"));
    if let Some(allowed @ Value::Array(_)) = gp(tool, "allowed_domains") {
        out.insert("filters".into(), json!({"allowed_domains": allowed}));
    }
    if let Some(loc @ Value::Object(_)) = gp(tool, "user_location") {
        out.insert("user_location".into(), loc.clone());
    }
    Value::Object(out)
}

/// Shared body of shortenNameIfNeeded and buildShortNameMap's baseCandidate.
fn base_short_name(name: &str) -> String {
    const LIMIT: usize = 64;
    if name.len() <= LIMIT {
        return name.to_string();
    }
    if name.starts_with("mcp__") {
        if let Some(idx) = name.rfind("__") {
            if idx > 0 {
                let cand = format!("mcp__{}", &name[idx + 2..]);
                return prefix_bytes(&cand, LIMIT).to_string();
            }
        }
    }
    prefix_bytes(name, LIMIT).to_string()
}

// port of shortenNameIfNeeded (codex_claude_request.go)
fn shorten_name_if_needed(name: &str) -> String {
    base_short_name(name)
}

// port of buildShortNameMap (codex_claude_request.go)
fn build_short_name_map(names: &[String]) -> HashMap<String, String> {
    const LIMIT: usize = 64;
    let mut used: HashSet<String> = HashSet::new();
    let mut m = HashMap::new();
    for n in names {
        let cand = base_short_name(n);
        let uniq = if !used.contains(&cand) {
            cand
        } else {
            let mut i = 1usize;
            loop {
                let suffix = format!("_{i}");
                let allowed = LIMIT.saturating_sub(suffix.len());
                let tmp = format!("{}{}", prefix_bytes(&cand, allowed), suffix);
                if !used.contains(&tmp) {
                    break tmp;
                }
                i += 1;
            }
        };
        used.insert(uniq.clone());
        m.insert(n.clone(), uniq);
    }
    m
}

fn claude_tool_names(original: &Value) -> Vec<String> {
    match gp(original, "tools") {
        Some(Value::Array(tools)) => tools
            .iter()
            .map(|t| gstr(t, "name"))
            .filter(|n| !n.is_empty())
            .collect(),
        _ => vec![],
    }
}

// port of buildReverseMapFromClaudeOriginalToShort (codex_claude_request.go)
fn build_reverse_map_from_claude_original_to_short(original: &Value) -> HashMap<String, String> {
    let names = claude_tool_names(original);
    if names.is_empty() {
        return HashMap::new();
    }
    build_short_name_map(&names)
}

// port of normalizeToolParameters (codex_claude_request.go)
fn normalize_tool_parameters(raw: Option<&Value>) -> Value {
    let mut root_value = match raw {
        Some(v @ Value::Object(_)) => v.clone(),
        _ => return json!({"type": "object", "properties": {}}),
    };
    strip_dialect_keywords_from_schema(&mut root_value);
    let root = root_value.as_object_mut().expect("still an object");

    let is_object = match root.get("type") {
        None | Some(Value::Null) => {
            root.insert("type".into(), json!("object"));
            true
        }
        Some(Value::String(s)) if s.is_empty() => {
            root.insert("type".into(), json!("object"));
            true
        }
        Some(Value::String(s)) => s == "object",
        Some(Value::Array(items)) => items.iter().any(|e| e.as_str() == Some("object")),
        _ => false,
    };
    if is_object && matches!(root.get("properties"), None | Some(Value::Null)) {
        root.insert("properties".into(), json!({}));
    }
    root_value
}

// port of stripDialectKeywordsFromSchema (codex_claude_request.go)
fn strip_dialect_keywords_from_schema(v: &mut Value) {
    match v {
        Value::Object(schema) => {
            schema.shift_remove("$schema");
            schema.shift_remove("$id");
            if let Some(Value::String(p)) = schema.get("pattern") {
                if has_unsupported_unicode_property_escape(p) {
                    schema.shift_remove("pattern");
                }
            }
            if let Some(Value::Object(pattern_props)) = schema.get_mut("patternProperties") {
                pattern_props.retain(|key, _| !has_unsupported_unicode_property_escape(key));
                for sub in pattern_props.values_mut() {
                    strip_dialect_keywords_from_schema(sub);
                }
            }
            for key in SCHEMA_MAP_KEYWORDS {
                if key == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = schema.get_mut(key) {
                    for sub in sub_map.values_mut() {
                        strip_dialect_keywords_from_schema(sub);
                    }
                }
            }
            for key in SCHEMA_VALUE_KEYWORDS {
                match schema.get_mut(key) {
                    Some(sub @ Value::Object(_)) => strip_dialect_keywords_from_schema(sub),
                    Some(Value::Array(items)) => {
                        for item in items.iter_mut() {
                            strip_dialect_keywords_from_schema(item);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                strip_dialect_keywords_from_schema(item);
            }
        }
        _ => {}
    }
}

// port of codexSchemaMissesRequired (codex_claude_request.go)
fn codex_schema_misses_required(schema: &Value) -> bool {
    let obj = match schema {
        Value::Object(o) => o,
        Value::Array(items) => return items.iter().any(codex_schema_misses_required),
        _ => return false,
    };
    if let Some(Value::Object(properties)) = obj.get("properties") {
        match obj.get("required") {
            Some(Value::Array(required)) => {
                let names: HashSet<&str> = required.iter().filter_map(|r| r.as_str()).collect();
                if properties.keys().any(|name| !names.contains(name.as_str())) {
                    return true;
                }
            }
            _ => {
                if !properties.is_empty() {
                    return true;
                }
            }
        }
    }
    for key in SCHEMA_MAP_KEYWORDS {
        if let Some(Value::Object(children)) = obj.get(key) {
            if children.values().any(codex_schema_misses_required) {
                return true;
            }
        }
    }
    for key in SCHEMA_VALUE_KEYWORDS {
        if let Some(child) = obj.get(key) {
            if codex_schema_misses_required(child) {
                return true;
            }
        }
    }
    false
}

/// `data:<media>;base64,<data>` from an Anthropic image `source`, or None.
fn image_source_data_url(source: Option<&Value>) -> Option<String> {
    let source = source?;
    let mut data = gstr(source, "data");
    if data.is_empty() {
        data = gstr(source, "base64");
    }
    if data.is_empty() {
        return None;
    }
    let mut media_type = gstr(source, "media_type");
    if media_type.is_empty() {
        media_type = gstr(source, "mime_type");
    }
    if media_type.is_empty() {
        media_type = "application/octet-stream".into();
    }
    Some(format!("data:{media_type};base64,{data}"))
}

/// The `encrypted_content` an assistant thinking block replays as, or None
/// when the block must be dropped. Non-compat branch of `appendReasoningContent`
/// (preserveEmptyThinkingBlocks = false).
// port of the appendReasoningContent closure (codex_claude_request.go)
fn reasoning_signature_for_target(model_name: &str, part: &Value) -> Option<String> {
    let raw_signature = gstr(part, "signature");
    if let Some(sig) = compatible_gpt_signature(&raw_signature) {
        return Some(sig);
    }
    if !codex_claude_target_accepts_grok_signature(model_name) {
        return None;
    }
    if !is_valid_grok_encrypted_content(&raw_signature) {
        return None;
    }
    Some(raw_signature)
}

/// Client request (Anthropic Messages body) → upstream request (standard OpenAI
/// Responses body). `model` is the upstream model id to put in the body.
/// `stream` = whether the client asked to stream (unused, as in Go: the Codex
/// upstream is always streamed).
// port of ConvertClaudeRequestToCodex / convertClaudeRequestToCodex (codex_claude_request.go)
pub fn translate_request(model: &str, body: &Value, stream: bool) -> Value {
    let _ = stream;
    let tool_name_map = build_reverse_map_from_claude_original_to_short(body);
    let mut input_items: Vec<Value> = Vec::new();

    // Process system messages and convert them to input content format.
    if let Some(system) = gp(body, "system") {
        let mut content_items: Vec<Value> = Vec::new();
        let mut append_system_text = |text: String| {
            if text.is_empty() || is_claude_code_attribution_system_text(&text) {
                return;
            }
            content_items.push(json!({"type": "input_text", "text": text}));
        };
        match system {
            Value::String(s) => append_system_text(s.clone()),
            Value::Array(items) => {
                for item in items {
                    if gstr(item, "type") == "text" {
                        append_system_text(gstr(item, "text"));
                    }
                }
            }
            _ => {}
        }
        if !content_items.is_empty() {
            input_items
                .push(json!({"type": "message", "role": "developer", "content": content_items}));
        }
    }

    // Process messages and transform their contents to appropriate formats.
    if let Some(Value::Array(messages)) = gp(body, "messages") {
        let mut pending_tool_use_ids: Vec<String> = Vec::new();
        let mut pending_system_reminders: Vec<Value> = Vec::new();

        fn flush_message(role: &str, content_items: &mut Vec<Value>, input_items: &mut Vec<Value>) {
            if !content_items.is_empty() {
                input_items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": std::mem::take(content_items),
                }));
            }
        }

        for message in messages {
            let role = gstr(message, "role");
            if role == "system" {
                if let Some(text) = claude_message_system_reminder_text(gp(message, "content")) {
                    let reminder = json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": text}],
                    });
                    if !pending_tool_use_ids.is_empty() {
                        pending_system_reminders.push(reminder);
                    } else {
                        input_items.push(reminder);
                    }
                }
                continue;
            }

            let mut contents = gp(message, "content").cloned();
            if role == "user" && !pending_tool_use_ids.is_empty() {
                contents = contents.map(|c| align_claude_tool_results(c, &pending_tool_use_ids));
            }
            pending_tool_use_ids.clear();
            let mut content_items: Vec<Value> = Vec::new();
            let text_part_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };

            match &contents {
                Some(Value::Array(parts)) => {
                    for part in parts {
                        match gstr(part, "type").as_str() {
                            "text" => {
                                input_items.append(&mut pending_system_reminders);
                                content_items.push(
                                    json!({"type": text_part_type, "text": gstr(part, "text")}),
                                );
                            }
                            "thinking" => {
                                if role != "assistant" {
                                    continue;
                                }
                                if let Some(signature) = reasoning_signature_for_target(model, part)
                                {
                                    flush_message(&role, &mut content_items, &mut input_items);
                                    input_items.push(json!({
                                        "type": "reasoning",
                                        "summary": [],
                                        "content": null,
                                        "encrypted_content": signature,
                                    }));
                                }
                            }
                            "image" => {
                                input_items.append(&mut pending_system_reminders);
                                if let Some(url) = image_source_data_url(gp(part, "source")) {
                                    content_items
                                        .push(json!({"type": "input_image", "image_url": url}));
                                }
                            }
                            "document" => {
                                input_items.append(&mut pending_system_reminders);
                                let source = gp(part, "source").unwrap_or(&Value::Null);
                                if gstr(source, "type") != "base64" {
                                    continue;
                                }
                                let media_type = gstr(source, "media_type").trim().to_string();
                                if !media_type.eq_ignore_ascii_case("application/pdf") {
                                    continue;
                                }
                                let mut data = gstr(source, "data");
                                if data.is_empty() {
                                    data = gstr(source, "base64");
                                }
                                if !data.is_empty() {
                                    content_items.push(json!({
                                        "type": "input_file",
                                        "file_data": format!("data:{media_type};base64,{data}"),
                                        "filename": "document.pdf",
                                    }));
                                }
                            }
                            "tool_use" => {
                                flush_message(&role, &mut content_items, &mut input_items);
                                let id = gstr(part, "id");
                                if !id.is_empty() {
                                    pending_tool_use_ids.push(id.clone());
                                }
                                let raw_name = gstr(part, "name");
                                let name = match tool_name_map.get(&raw_name) {
                                    Some(short) => short.clone(),
                                    None => shorten_name_if_needed(&raw_name),
                                };
                                let arguments =
                                    gp(part, "input").map(|v| v.to_string()).unwrap_or_default();
                                input_items.push(json!({
                                    "type": "function_call",
                                    "call_id": shorten_codex_call_id_if_needed(&id),
                                    "name": name,
                                    "arguments": arguments,
                                }));
                            }
                            "tool_result" => {
                                flush_message(&role, &mut content_items, &mut input_items);
                                let call_id =
                                    shorten_codex_call_id_if_needed(&gstr(part, "tool_use_id"));
                                let output = match gp(part, "content") {
                                    Some(Value::Array(results)) => {
                                        let mut items: Vec<Value> =
                                            Vec::with_capacity(results.len());
                                        for r in results {
                                            match gstr(r, "type").as_str() {
                                                "image" => {
                                                    if let Some(url) =
                                                        image_source_data_url(gp(r, "source"))
                                                    {
                                                        items.push(json!({"type": "input_image", "image_url": url}));
                                                    }
                                                }
                                                "text" => {
                                                    items.push(json!({"type": "input_text", "text": gstr(r, "text")}));
                                                }
                                                _ => {}
                                            }
                                        }
                                        if items.is_empty() {
                                            Value::String(gstr(part, "content"))
                                        } else {
                                            Value::Array(items)
                                        }
                                    }
                                    _ => Value::String(gstr(part, "content")),
                                };
                                input_items.push(json!({
                                    "type": "function_call_output",
                                    "call_id": call_id,
                                    "output": output,
                                }));
                            }
                            _ => {}
                        }
                    }
                    flush_message(&role, &mut content_items, &mut input_items);
                    input_items.append(&mut pending_system_reminders);
                }
                Some(Value::String(text)) => {
                    content_items.push(json!({"type": text_part_type, "text": text}));
                    flush_message(&role, &mut content_items, &mut input_items);
                    input_items.append(&mut pending_system_reminders);
                }
                _ => {}
            }
        }
        input_items.append(&mut pending_system_reminders);
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("instructions".into(), json!(""));
    out.insert("input".into(), json!([]));

    // Convert tools declarations to the expected format for the Codex API.
    let mut tool_items: Option<Vec<Value>> = None;
    if let Some(Value::Array(tools)) = gp(body, "tools") {
        let web_search_tool_names = build_claude_web_search_tool_name_set(tools);
        out.insert(
            "tool_choice".into(),
            convert_claude_tool_choice_to_codex(
                gp(body, "tool_choice"),
                &tool_name_map,
                &web_search_tool_names,
            ),
        );
        let mut items = Vec::with_capacity(tools.len());
        for tool_result in tools {
            // Special handling: map Claude web search tool to Codex web_search
            if is_claude_web_search_tool_type(&gstr(tool_result, "type")) {
                items.push(convert_claude_web_search_tool_to_codex(tool_result));
                continue;
            }
            let mut tool = match tool_result {
                Value::Object(m) => m.clone(),
                _ => Map::new(),
            };
            if tool_result.get("type").and_then(Value::as_str) != Some("function") {
                tool.insert("type".into(), json!("function"));
            }
            // Apply shortened name if needed
            if let Some(v) = gp(tool_result, "name") {
                let original_name = gs(Some(v));
                let name = match tool_name_map.get(&original_name) {
                    Some(short) => short.clone(),
                    None => shorten_name_if_needed(&original_name),
                };
                if !v.is_string() || name != original_name {
                    tool.insert("name".into(), json!(name));
                }
            }
            tool.insert(
                "parameters".into(),
                normalize_tool_parameters(gp(tool_result, "input_schema")),
            );
            for key in ["input_schema", "cache_control", "defer_loading"] {
                tool.shift_remove(key);
            }
            if let Some(Value::Object(params)) = tool.get_mut("parameters") {
                params.shift_remove("$schema");
            }
            if tool.get("strict") != Some(&Value::Bool(false)) {
                tool.insert("strict".into(), json!(false));
            }
            items.push(Value::Object(tool));
        }
        tool_items = Some(items);
    }

    // Default to parallel tool calls unless tool_choice explicitly disables them.
    let mut parallel_tool_calls = true;
    if let Some(v) = gp(body, "tool_choice.disable_parallel_tool_use") {
        parallel_tool_calls = !gb(Some(v));
    }
    out.insert("parallel_tool_calls".into(), json!(parallel_tool_calls));

    // Convert thinking.budget_tokens to reasoning.effort.
    let mut reasoning_effort = "medium".to_string();
    if let Some(thinking @ Value::Object(_)) = gp(body, "thinking") {
        match gstr(thinking, "type").as_str() {
            "enabled" => {
                if let Some(budget) = gp(thinking, "budget_tokens") {
                    if let Some(effort) = convert_budget_to_level(gi(Some(budget))) {
                        reasoning_effort = effort.to_string();
                    }
                }
            }
            "adaptive" | "auto" => {
                let effort = match gp(body, "output_config.effort") {
                    Some(Value::String(s)) => s.trim().to_lowercase(),
                    _ => String::new(),
                };
                reasoning_effort = if effort.is_empty() {
                    "xhigh".to_string()
                } else {
                    effort
                };
            }
            "disabled" => {
                if let Some(effort) = convert_budget_to_level(0) {
                    reasoning_effort = effort.to_string();
                }
            }
            _ => {}
        }
    }
    out.insert("reasoning".into(), json!({"effort": reasoning_effort}));

    let mut service_tier = normalize_codex_service_tier(gp(body, "service_tier"));
    if let Some(Value::String(speed)) = gp(body, "speed") {
        if speed == "fast" {
            service_tier = "priority";
        }
    }
    if !service_tier.is_empty() {
        out.insert("service_tier".into(), json!(service_tier));
    }
    out.insert("stream".into(), json!(true));
    out.insert("store".into(), json!(false));
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));

    // Map Claude output_config.format to Codex Responses text.format.
    if let Some(format @ Value::Object(_)) = gp(body, "output_config.format") {
        if let (Some("json_schema"), Some(schema @ Value::Object(_))) = (
            format.get("type").and_then(Value::as_str),
            gp(format, "schema"),
        ) {
            let mut name = gstr(format, "name");
            if name.is_empty() {
                name = "cli_proxy_structured_output".into();
            }
            let mut strict = gp(format, "strict") != Some(&Value::Bool(false));
            if strict && codex_schema_misses_required(schema) {
                strict = false;
            }
            out.insert(
                "text".into(),
                json!({"format": {"type": "json_schema", "name": name, "strict": strict, "schema": schema}}),
            );
        }
    }

    if let Some(items) = tool_items {
        out.insert("tools".into(), Value::Array(items));
    }
    out.insert("input".into(), Value::Array(input_items));
    Value::Object(out)
}

// ──────────────────────────── response translation ────────────────────────────

/// codexThinkingSummaryPartSeparator (codex_claude_response.go)
const CODEX_THINKING_SUMMARY_PART_SEPARATOR: &str = "\n\n";

// port of codexFunctionCallStream (codex_claude_response.go)
#[derive(Default)]
struct FunctionCallStream {
    call_id: String,
    name: String,
    block_index: i64,
    arguments: String,
    emitted_arguments_length: usize,
    has_received_arguments_delta: bool,
    emit_initial_empty_delta: bool,
    started: bool,
    done: bool,
    closed: bool,
}

/// Port of ConvertCodexResponseToClaudeParams (codex_claude_response.go). The
/// Go pointers into `FunctionCalls` / `FunctionCallQueue` become indices into
/// the `calls` arena.
#[derive(Default)]
struct Params {
    has_emitted_tool_use: bool,
    block_index: i64,
    has_text_delta: bool,
    text_block_open: bool,
    thinking_block_open: bool,
    thinking_signature: String,
    thinking_summary_seen: bool,
    web_search_tool_use_ids: HashSet<String>,
    web_search_tool_result_ids: HashSet<String>,
    last_web_search_tool_use_id: String,
    calls: Vec<FunctionCallStream>,
    function_calls: HashMap<String, usize>,
    function_call_queue: Vec<usize>,
    active_function_call: Option<usize>,
    last_function_call: Option<usize>,
    deferred_stream_events: Vec<Value>,
}

/// Per-response streaming state (port of the Go `param *any` state,
/// ConvertCodexResponseToClaudeParams).
pub struct StreamTranslator {
    /// short → original tool names of the client's original request.
    rev_names: HashMap<String, String>,
    p: Params,
    /// Rust-side bookkeeping for `finish` (Go has no end-of-stream hook).
    emitted_any: bool,
    terminal_emitted: bool,
    errored: bool,
}

impl StreamTranslator {
    /// `original_request` = the client's ORIGINAL request body.
    pub fn new(original_request: &Value) -> Self {
        StreamTranslator {
            rev_names: build_reverse_map_from_claude_original_short_to_original(original_request),
            p: Params::default(),
            emitted_any: false,
            terminal_emitted: false,
            errored: false,
        }
    }

    /// One upstream SSE event: `event` = its `event:` field if present, `data` =
    /// parsed JSON of its `data:` payload. Returns zero or more COMPLETE client
    /// SSE frames, each ending in "\n\n".
    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<String> {
        let frames = match (event, gp(data, "type")) {
            (Some(ev), None) if data.is_object() => {
                let mut with_type = data.clone();
                if let Value::Object(m) = &mut with_type {
                    m.insert("type".into(), json!(ev));
                }
                self.convert(&with_type)
            }
            _ => self.convert(data),
        };
        if !frames.is_empty() {
            self.emitted_any = true;
        }
        frames
    }

    /// Upstream stream ended (EOF / `[DONE]`). Returns any trailing client
    /// frames needed to close the stream correctly (only if not already
    /// emitted). Not in Go: a stream that already produced frames but never saw
    /// `response.completed` / `response.incomplete` (and no `error`) is closed
    /// as if a terminal event with an empty response arrived — open blocks are
    /// stopped, pending tool calls flushed, then `message_delta` (`end_turn`, or
    /// `tool_use` when a tool_use was emitted) and `message_stop`.
    pub fn finish(&mut self) -> Vec<String> {
        if self.terminal_emitted || self.errored || !self.emitted_any {
            return Vec::new();
        }
        self.convert(&json!({"type": "response.completed", "response": {}}))
    }

    // port of ConvertCodexResponseToClaude (codex_claude_response.go)
    fn convert(&mut self, root: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let type_str = gstr(root, "type");
        if self.p.active_function_call.is_some() && should_defer_codex_stream_event(&type_str, root)
        {
            self.p.deferred_stream_events.push(root.clone());
            return out;
        }
        let rev = &self.rev_names;
        let p = &mut self.p;

        match type_str.as_str() {
            // `response.failed` is the terminal event of a failed answer: an
            // error for the client, like an `error` event — not a message
            // closed as `end_turn`.
            "error" | "response.failed" => {
                self.errored = true;
                out.push(codex_stream_error_to_claude_error(root));
            }
            "response.created" => {
                let template = json!({
                    "type": "message_start",
                    "message": {
                        "id": gstr(root, "response.id"),
                        "type": "message",
                        "role": "assistant",
                        "model": gstr(root, "response.model"),
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                        "content": [],
                        "stop_reason": null,
                    }
                });
                out.push(sse("message_start", &template));
            }
            "response.reasoning_summary_part.added" => {
                p.stop_text_block(&mut out);
                // Keep one thinking block open for the whole reasoning item and
                // separate the parts with a blank line, so the only signature
                // ever emitted is the final one.
                if p.thinking_block_open {
                    p.append_thinking_delta(&mut out, CODEX_THINKING_SUMMARY_PART_SEPARATOR);
                } else {
                    p.start_thinking_block(&mut out);
                }
                p.thinking_summary_seen = true;
            }
            "response.reasoning_summary_text.delta" => {
                p.stop_text_block(&mut out);
                p.start_thinking_block(&mut out);
                p.append_thinking_delta(&mut out, &gstr(root, "delta"));
            }
            "response.reasoning_summary_part.done" => {
                // Intentionally does not close the thinking block: it stays open
                // until output_item.done delivers the final encrypted_content.
            }
            "response.content_part.added" => {
                p.finalize_thinking_block(&mut out);
                if gstr(root, "part.type") == "output_text" {
                    p.start_text_block(&mut out);
                }
            }
            "response.output_text.delta" => {
                p.has_text_delta = true;
                p.finalize_thinking_block(&mut out);
                p.start_text_block(&mut out);
                out.push(sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": p.block_index,
                            "delta": {"type": "text_delta", "text": gstr(root, "delta")}}),
                ));
            }
            "response.content_part.done" if gstr(root, "part.type") == "output_text" => {
                p.stop_text_block(&mut out);
            }
            "response.web_search_call.searching"
            | "response.web_search_call.completed"
            | "response.web_search_call.in_progress" => {
                // Wait for populated web_search_call items on output_item.done.
            }
            "response.completed" | "response.incomplete" => {
                self.terminal_emitted = true;
                let response_data = gp(root, "response").unwrap_or(&Value::Null);
                p.finalize_thinking_block(&mut out);
                p.stop_text_block(&mut out);
                p.append_function_calls_from_terminal(&mut out, rev, response_data);
                self.append_deferred_stream_events(&mut out);
                let p = &mut self.p;
                p.finalize_thinking_block(&mut out);
                p.stop_text_block(&mut out);

                let stop_reason = map_codex_stop_reason_to_claude(
                    &codex_stop_reason(response_data),
                    p.has_emitted_tool_use,
                );
                let mut delta = Map::new();
                delta.insert("stop_reason".into(), json!(stop_reason));
                delta.insert("stop_sequence".into(), Value::Null);
                set_claude_stop_sequence(&mut delta, "stop_sequence", response_data);
                let usage = build_claude_usage(gp(response_data, "usage"));
                let template = json!({"type": "message_delta", "delta": delta, "usage": usage});
                out.push(sse("message_delta", &template));
                out.push(sse("message_stop", &json!({"type": "message_stop"})));
            }
            "response.output_item.added" => {
                let item = gp(root, "item").unwrap_or(&Value::Null);
                match gstr(item, "type").as_str() {
                    "function_call" => {
                        p.finalize_thinking_block(&mut out);
                        p.stop_text_block(&mut out);
                        let call = p.record_function_call(root, item);
                        p.update_function_call_identity(call, root, item);
                        if !p.calls[call].name.is_empty() {
                            p.calls[call].emit_initial_empty_delta = true;
                        }
                        p.append_function_call_queue(&mut out, rev);
                    }
                    "reasoning" => {
                        p.stop_text_block(&mut out);
                        // A previous reasoning item that never reported
                        // output_item.done must not leak its open block.
                        p.finalize_thinking_block(&mut out);
                        p.thinking_summary_seen = false;
                        // Kept only as a fallback for streams whose
                        // output_item.done omits encrypted_content.
                        p.thinking_signature = gstr(item, "encrypted_content");
                    }
                    _ => {
                        // web_search_call: defer server_tool_use until
                        // output_item.done carries action/query.
                    }
                }
            }
            "response.output_item.done" => {
                let item = gp(root, "item").unwrap_or(&Value::Null);
                match gstr(item, "type").as_str() {
                    "message" => {
                        if p.has_text_delta {
                            return out;
                        }
                        let parts = match gp(item, "content") {
                            Some(Value::Array(parts)) => parts,
                            _ => return out,
                        };
                        let text: String = parts
                            .iter()
                            .filter(|part| gstr(part, "type") == "output_text")
                            .map(|part| gstr(part, "text"))
                            .collect();
                        if text.is_empty() {
                            return out;
                        }
                        p.finalize_thinking_block(&mut out);
                        p.start_text_block(&mut out);
                        out.push(sse(
                            "content_block_delta",
                            &json!({"type": "content_block_delta", "index": p.block_index,
                                    "delta": {"type": "text_delta", "text": text}}),
                        ));
                        p.stop_text_block(&mut out);
                        p.has_text_delta = true;
                    }
                    "function_call" => {
                        p.finalize_thinking_block(&mut out);
                        p.stop_text_block(&mut out);
                        let call = match p.function_call_for_event(root, item) {
                            Some(c) => c,
                            None => p.record_function_call(root, item),
                        };
                        p.update_function_call_identity(call, root, item);
                        update_function_call_arguments(
                            &mut p.calls[call],
                            &gstr(item, "arguments"),
                            false,
                        );
                        p.calls[call].done = true;
                        p.append_function_call_queue(&mut out, rev);
                    }
                    "reasoning" => {
                        p.stop_text_block(&mut out);
                        let signature = gstr(item, "encrypted_content");
                        if !signature.is_empty() {
                            p.thinking_signature = signature;
                        }
                        if p.thinking_summary_seen {
                            p.finalize_thinking_block(&mut out);
                        } else {
                            p.finalize_signature_only_thinking_block(&mut out);
                        }
                        p.thinking_signature.clear();
                        p.thinking_summary_seen = false;
                    }
                    "web_search_call" => {
                        p.append_web_search_tool_result(&mut out, root, item);
                    }
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" => {
                let call = match p.function_call_for_event(root, &Value::Null) {
                    Some(c) => c,
                    None => p.record_function_call(root, &Value::Null),
                };
                update_function_call_arguments(&mut p.calls[call], &gstr(root, "delta"), true);
                p.append_function_call_buffered_arguments(&mut out, call);
            }
            "response.function_call_arguments.done" => {
                let call = match p.function_call_for_event(root, &Value::Null) {
                    Some(c) => c,
                    None => p.record_function_call(root, &Value::Null),
                };
                update_function_call_arguments(&mut p.calls[call], &gstr(root, "arguments"), false);
                p.append_function_call_buffered_arguments(&mut out, call);
            }
            _ => {}
        }

        if self.p.function_call_queue.is_empty() {
            self.append_deferred_stream_events(&mut out);
        }
        out
    }

    // port of appendDeferredCodexStreamEvents (codex_claude_response.go)
    fn append_deferred_stream_events(&mut self, out: &mut Vec<String>) {
        if self.p.deferred_stream_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.p.deferred_stream_events);
        for event in events {
            let translated = self.convert(&event);
            out.extend(translated);
        }
    }
}

// port of shouldDeferCodexStreamEvent (codex_claude_response.go)
fn should_defer_codex_stream_event(type_str: &str, root: &Value) -> bool {
    match type_str {
        "error"
        | "response.completed"
        | "response.incomplete"
        | "response.function_call_arguments.delta"
        | "response.function_call_arguments.done" => false,
        "response.output_item.added" | "response.output_item.done" => {
            gstr(root, "item.type") != "function_call"
        }
        _ => true,
    }
}

// port of codexStreamErrorToClaudeError (codex_claude_response.go)
fn codex_stream_error_to_claude_error(root: &Value) -> String {
    // An `error` event carries it at the root; `response.failed` under
    // `response.error`.
    let error = gp(root, "error")
        .or_else(|| gp(root, "response.error"))
        .unwrap_or(&Value::Null);
    let mut err_type = gstr(error, "type").trim().to_string();
    if err_type.is_empty() {
        err_type = gstr(root, "error_type").trim().to_string();
    }
    if err_type.is_empty() {
        err_type = "api_error".into();
    }
    let code = gstr(error, "code").trim().to_string();
    let mut message = gstr(error, "message").trim().to_string();
    if message.is_empty() {
        message = gstr(root, "message").trim().to_string();
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = err_type.clone();
    }
    if code == "cyber_policy" || err_type == "invalid_request" {
        err_type = "invalid_request_error".into();
    }
    sse(
        "error",
        &json!({"type": "error", "error": {"type": err_type, "message": message}}),
    )
}

impl Params {
    // port of startCodexTextBlock (codex_claude_response.go)
    fn start_text_block(&mut self, out: &mut Vec<String>) {
        if self.text_block_open {
            return;
        }
        self.text_block_open = true;
        out.push(sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": self.block_index,
                    "content_block": {"type": "text", "text": ""}}),
        ));
    }

    // port of stopCodexTextBlock (codex_claude_response.go)
    fn stop_text_block(&mut self, out: &mut Vec<String>) {
        if !self.text_block_open {
            return;
        }
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.block_index}),
        ));
        self.text_block_open = false;
        self.block_index += 1;
    }

    // port of startCodexThinkingBlock (codex_claude_response.go)
    fn start_thinking_block(&mut self, out: &mut Vec<String>) {
        if self.thinking_block_open {
            return;
        }
        self.thinking_block_open = true;
        out.push(sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": self.block_index,
                    "content_block": {"type": "thinking", "thinking": ""}}),
        ));
    }

    // port of appendCodexThinkingDelta (codex_claude_response.go)
    fn append_thinking_delta(&mut self, out: &mut Vec<String>, text: &str) {
        if text.is_empty() {
            return;
        }
        out.push(sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": self.block_index,
                    "delta": {"type": "thinking_delta", "thinking": text}}),
        ));
    }

    // port of finalizeCodexSignatureOnlyThinkingBlock (codex_claude_response.go)
    fn finalize_signature_only_thinking_block(&mut self, out: &mut Vec<String>) {
        if self.thinking_signature.is_empty() {
            return;
        }
        self.start_thinking_block(out);
        self.finalize_thinking_block(out);
    }

    // port of finalizeCodexThinkingBlock (codex_claude_response.go)
    fn finalize_thinking_block(&mut self, out: &mut Vec<String>) {
        if !self.thinking_block_open {
            return;
        }
        if !self.thinking_signature.is_empty() {
            out.push(sse(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": self.block_index,
                        "delta": {"type": "signature_delta", "signature": self.thinking_signature}}),
            ));
        }
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.block_index}),
        ));
        self.block_index += 1;
        self.thinking_block_open = false;
    }

    // port of codexFunctionCallForKeys (codex_claude_response.go)
    fn function_call_for_keys(&self, keys: &[String]) -> Option<usize> {
        keys.iter()
            .find_map(|k| self.function_calls.get(k).copied())
    }

    // port of codexFunctionCallForEvent (codex_claude_response.go)
    fn function_call_for_event(&self, root: &Value, item: &Value) -> Option<usize> {
        let keys = codex_function_call_keys(root, item);
        if !keys.is_empty() {
            return self.function_call_for_keys(&keys);
        }
        self.last_function_call
    }

    fn new_function_call(&mut self) -> usize {
        self.calls.push(FunctionCallStream {
            block_index: -1,
            ..Default::default()
        });
        let idx = self.calls.len() - 1;
        self.function_call_queue.push(idx);
        idx
    }

    // port of recordCodexFunctionCall (codex_claude_response.go)
    fn record_function_call(&mut self, root: &Value, item: &Value) -> usize {
        let keys = codex_function_call_keys(root, item);
        let call = match self.function_call_for_keys(&keys) {
            Some(c) => c,
            None => self.new_function_call(),
        };
        self.add_function_call_aliases(call, &keys);
        self.last_function_call = Some(call);
        call
    }

    // port of addCodexFunctionCallAliases (codex_claude_response.go)
    fn add_function_call_aliases(&mut self, call: usize, keys: &[String]) {
        for key in keys {
            self.function_calls.insert(key.clone(), call);
        }
    }

    // port of updateCodexFunctionCallIdentity (codex_claude_response.go)
    fn update_function_call_identity(&mut self, call: usize, root: &Value, item: &Value) {
        let call_id = gstr(item, "call_id");
        if !call_id.is_empty() {
            self.calls[call].call_id = call_id;
        }
        let name = gstr(item, "name");
        if !name.is_empty() {
            self.calls[call].name = name;
        }
        let keys = codex_function_call_keys(root, item);
        self.add_function_call_aliases(call, &keys);
    }

    // port of appendCodexFunctionCallBufferedArguments (codex_claude_response.go)
    fn append_function_call_buffered_arguments(&mut self, out: &mut Vec<String>, call: usize) {
        if self.active_function_call != Some(call) {
            return;
        }
        let c = &mut self.calls[call];
        if !c.started || c.closed || c.emitted_arguments_length >= c.arguments.len() {
            return;
        }
        let mut from = c.emitted_arguments_length;
        while from < c.arguments.len() && !c.arguments.is_char_boundary(from) {
            from += 1;
        }
        let partial = c.arguments[from..].to_string();
        out.push(function_call_argument_delta(&partial, c.block_index));
        c.emitted_arguments_length = c.arguments.len();
    }

    // port of appendCodexFunctionCallQueue (codex_claude_response.go)
    fn append_function_call_queue(&mut self, out: &mut Vec<String>, rev: &HashMap<String, String>) {
        loop {
            if let Some(active) = self.active_function_call {
                self.append_function_call_buffered_arguments(out, active);
                if !self.calls[active].done {
                    return;
                }
                let active_index = self.calls[active].block_index;
                out.push(function_call_stop(active_index));
                if self.block_index <= active_index {
                    self.block_index = active_index + 1;
                }
                self.calls[active].closed = true;
                self.active_function_call = None;
                self.remove_function_call_from_queue(active);
            }

            while let Some(&first) = self.function_call_queue.first() {
                if !self.calls[first].closed {
                    break;
                }
                self.function_call_queue.remove(0);
            }
            let call = match self.function_call_queue.first() {
                Some(&c) => c,
                None => return,
            };
            if self.calls[call].name.is_empty() {
                return;
            }

            self.calls[call].block_index = self.block_index;
            let (call_id, name, block_index, initial) = {
                let c = &self.calls[call];
                (
                    c.call_id.clone(),
                    c.name.clone(),
                    c.block_index,
                    c.emit_initial_empty_delta,
                )
            };
            out.push(function_call_start(rev, &call_id, &name, block_index));
            if initial {
                out.push(function_call_argument_delta("", block_index));
            }
            self.calls[call].started = true;
            self.active_function_call = Some(call);
            self.has_emitted_tool_use = true;
            self.append_function_call_buffered_arguments(out, call);
        }
    }

    // port of removeCodexFunctionCallFromQueue (codex_claude_response.go)
    fn remove_function_call_from_queue(&mut self, call: usize) {
        if let Some(pos) = self.function_call_queue.iter().position(|&c| c == call) {
            self.function_call_queue.remove(pos);
        }
    }

    // port of appendCodexFunctionCallsFromTerminal (codex_claude_response.go)
    fn append_function_calls_from_terminal(
        &mut self,
        out: &mut Vec<String>,
        rev: &HashMap<String, String>,
        response_data: &Value,
    ) {
        if let Some(Value::Array(output)) = gp(response_data, "output") {
            for (index, item) in output.iter().enumerate() {
                if gstr(item, "type") != "function_call" {
                    continue;
                }
                let mut keys = codex_function_call_keys(&Value::Null, item);
                if let Some(oi) = gp(item, "output_index") {
                    append_unique_key(&mut keys, format!("output:{oi}"));
                }
                append_unique_key(&mut keys, format!("output:{index}"));
                let call = match self.function_call_for_keys(&keys) {
                    Some(c) => c,
                    None => self.new_function_call(),
                };
                self.add_function_call_aliases(call, &keys);
                self.update_function_call_identity(call, &Value::Null, item);
                update_function_call_arguments(
                    &mut self.calls[call],
                    &gstr(item, "arguments"),
                    false,
                );
                self.calls[call].done = true;
            }
        }

        let queue = std::mem::take(&mut self.function_call_queue);
        let mut kept = Vec::with_capacity(queue.len());
        for call in queue {
            let c = &mut self.calls[call];
            if c.closed {
                continue;
            }
            if c.name.is_empty() {
                c.closed = true;
                continue;
            }
            c.done = true;
            kept.push(call);
        }
        self.function_call_queue = kept;
        self.append_function_call_queue(out, rev);
        self.clear_function_calls();
    }

    // port of clearCodexFunctionCalls (codex_claude_response.go)
    fn clear_function_calls(&mut self) {
        self.function_calls.clear();
        self.function_call_queue.clear();
        self.active_function_call = None;
        self.last_function_call = None;
        self.calls.clear();
    }

    // port of appendCodexWebSearchServerToolUse (codex_claude_response_web_search.go)
    fn append_web_search_server_tool_use(
        &mut self,
        out: &mut Vec<String>,
        root: &Value,
        item: &Value,
    ) {
        let tool_use_id = self.web_search_tool_use_id(root, item);
        if tool_use_id.is_empty() {
            return;
        }
        let query = codex_web_search_query(root, item);
        let already_started = self.web_search_tool_use_ids.contains(&tool_use_id);
        if already_started && query.is_empty() {
            return;
        }
        if !already_started {
            self.stop_text_block(out);
            self.finalize_thinking_block(out);
            out.push(sse(
                "content_block_start",
                &json!({"type": "content_block_start", "index": self.block_index,
                        "content_block": {"type": "server_tool_use", "id": tool_use_id,
                                          "name": "web_search", "input": {}}}),
            ));
        }
        if !query.is_empty() {
            let partial_json = json!({"query": query}).to_string();
            out.push(sse(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": self.block_index,
                        "delta": {"type": "input_json_delta", "partial_json": partial_json}}),
            ));
        }
        if !already_started {
            out.push(sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.block_index}),
            ));
            self.web_search_tool_use_ids.insert(tool_use_id);
            self.block_index += 1;
        }
    }

    // port of appendCodexWebSearchToolResult (codex_claude_response_web_search.go)
    fn append_web_search_tool_result(&mut self, out: &mut Vec<String>, root: &Value, item: &Value) {
        let tool_use_id = self.web_search_tool_use_id(root, item);
        if tool_use_id.is_empty() {
            return;
        }
        self.append_web_search_server_tool_use(out, root, item);
        if self.web_search_tool_result_ids.contains(&tool_use_id) {
            return;
        }
        let content = codex_web_search_result_content(root, item);
        if codex_web_search_query(root, item).is_empty()
            && content.is_none()
            && gp(item, "action").is_none()
        {
            return;
        }
        out.push(sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": self.block_index,
                    "content_block": {"type": "web_search_tool_result", "tool_use_id": tool_use_id,
                                      "content": content.unwrap_or_else(|| json!([]))}}),
        ));
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.block_index}),
        ));
        self.block_index += 1;
        if tool_use_id == self.last_web_search_tool_use_id {
            self.last_web_search_tool_use_id.clear();
        }
        self.web_search_tool_result_ids.insert(tool_use_id);
    }

    // port of codexWebSearchToolUseID (codex_claude_response_web_search.go)
    fn web_search_tool_use_id(&mut self, root: &Value, item: &Value) -> String {
        for path in ["id", "output_item_id", "call_id"] {
            for source in [item, root] {
                let value = gstr(source, path).trim().to_string();
                if !value.is_empty() {
                    return value;
                }
            }
        }
        if !self.last_web_search_tool_use_id.is_empty() {
            return self.last_web_search_tool_use_id.clone();
        }
        for source in [item, root] {
            let value = gstr(source, "item_id").trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
        let id = format!("web_search_{}", self.block_index);
        self.last_web_search_tool_use_id = id.clone();
        id
    }
}

// port of codexFunctionCallKeys (codex_claude_response.go)
fn codex_function_call_keys(root: &Value, item: &Value) -> Vec<String> {
    let mut keys = Vec::with_capacity(5);
    if let Some(output_index) = gp(root, "output_index") {
        append_unique_key(&mut keys, format!("output:{output_index}"));
    }
    let item_call_id = gstr(item, "call_id");
    if !item_call_id.is_empty() {
        append_unique_key(&mut keys, format!("call:{item_call_id}"));
    }
    let root_call_id = gstr(root, "call_id");
    if !root_call_id.is_empty() {
        append_unique_key(&mut keys, format!("call:{root_call_id}"));
    }
    let item_id = gstr(item, "id");
    if !item_id.is_empty() {
        append_unique_key(&mut keys, format!("item:{item_id}"));
    }
    let root_item_id = gstr(root, "item_id");
    if !root_item_id.is_empty() {
        append_unique_key(&mut keys, format!("item:{root_item_id}"));
    }
    keys
}

// port of appendUniqueCodexFunctionCallKey (codex_claude_response.go)
fn append_unique_key(keys: &mut Vec<String>, key: String) {
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}

// port of updateCodexFunctionCallArguments (codex_claude_response.go)
fn update_function_call_arguments(call: &mut FunctionCallStream, arguments: &str, delta: bool) {
    if arguments.is_empty() {
        return;
    }
    if delta {
        call.arguments.push_str(arguments);
        call.has_received_arguments_delta = true;
        return;
    }
    if !call.has_received_arguments_delta {
        call.arguments = arguments.to_string();
        return;
    }
    if arguments.starts_with(call.arguments.as_str()) {
        call.arguments = arguments.to_string();
    }
}

// port of appendCodexFunctionCallStart (codex_claude_response.go)
fn function_call_start(
    rev: &HashMap<String, String>,
    call_id: &str,
    name: &str,
    block_index: i64,
) -> String {
    sse(
        "content_block_start",
        &json!({"type": "content_block_start", "index": block_index,
                "content_block": {"type": "tool_use",
                                  "id": shorten_codex_call_id_if_needed(&sanitize_claude_tool_id(call_id)),
                                  "name": resolve_codex_claude_tool_use_name(rev, name),
                                  "input": {}}}),
    )
}

// port of appendCodexFunctionCallArgumentDelta (codex_claude_response.go)
fn function_call_argument_delta(partial_json: &str, block_index: i64) -> String {
    sse(
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": block_index,
                "delta": {"type": "input_json_delta", "partial_json": partial_json}}),
    )
}

// port of appendCodexFunctionCallStop (codex_claude_response.go)
fn function_call_stop(block_index: i64) -> String {
    sse(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": block_index}),
    )
}

// port of resolveCodexClaudeToolUseName (codex_claude_response.go)
fn resolve_codex_claude_tool_use_name(rev: &HashMap<String, String>, name: &str) -> String {
    rev.get(name).cloned().unwrap_or_else(|| name.to_string())
}

// port of buildReverseMapFromClaudeOriginalShortToOriginal (codex_claude_response.go)
fn build_reverse_map_from_claude_original_short_to_original(
    original: &Value,
) -> HashMap<String, String> {
    build_reverse_map_from_claude_original_to_short(original)
        .into_iter()
        .map(|(orig, short)| (short, orig))
        .collect()
}

// port of codexStopReason (codex_claude_response.go)
fn codex_stop_reason(response_data: &Value) -> String {
    let stop_reason = gstr(response_data, "stop_reason");
    if !stop_reason.is_empty() {
        if stop_reason == "stop" && !codex_stop_sequence(response_data).is_empty() {
            return "stop_sequence".into();
        }
        return stop_reason;
    }
    let reason = gstr(response_data, "incomplete_details.reason");
    if !reason.is_empty() {
        return reason;
    }
    if !codex_stop_sequence(response_data).is_empty() {
        return "stop_sequence".into();
    }
    String::new()
}

// port of mapCodexStopReasonToClaude (codex_claude_response.go)
fn map_codex_stop_reason_to_claude(stop_reason: &str, has_tool_call: bool) -> String {
    if has_tool_call {
        return "tool_use".into();
    }
    match stop_reason {
        "" | "stop" | "completed" => "end_turn",
        "max_tokens" | "max_output_tokens" => "max_tokens",
        "tool_use" | "tool_calls" | "function_call" => "end_turn",
        "end_turn"
        | "stop_sequence"
        | "pause_turn"
        | "refusal"
        | "model_context_window_exceeded" => stop_reason,
        "content_filter" => "refusal",
        _ => "end_turn",
    }
    .to_string()
}

// port of codexStopSequence (codex_claude_response.go) — its String() form.
fn codex_stop_sequence(response_data: &Value) -> String {
    gstr(response_data, "stop_sequence")
}

// port of setClaudeStopSequence (codex_claude_response.go)
fn set_claude_stop_sequence(out: &mut Map<String, Value>, key: &str, response_data: &Value) {
    if let Some(stop_sequence) = gp(response_data, "stop_sequence") {
        if !gs(Some(stop_sequence)).is_empty() {
            out.insert(key.into(), stop_sequence.clone());
        }
    }
}

// port of extractResponsesUsage (codex_claude_response.go)
fn extract_responses_usage(usage: Option<&Value>) -> (i64, i64, i64, i64) {
    let usage = match usage {
        None | Some(Value::Null) => return (0, 0, 0, 0),
        Some(u) => u,
    };
    let mut input_tokens = gi(gp(usage, "input_tokens"));
    let output_tokens = gi(gp(usage, "output_tokens"));
    let cached_tokens = gi(gp(usage, "input_tokens_details.cached_tokens"));
    let mut cache_write_tokens = gi(gp(usage, "input_tokens_details.cache_write_tokens"));
    if cache_write_tokens <= 0 {
        cache_write_tokens = gi(gp(usage, "input_tokens_details.cache_creation_tokens"));
    }

    let mut deduct_tokens: i64 = 0;
    if cached_tokens > 0 {
        deduct_tokens += cached_tokens;
    }
    if cache_write_tokens > 0 {
        deduct_tokens = deduct_tokens.saturating_add(cache_write_tokens);
    }
    if deduct_tokens > 0 {
        if input_tokens >= deduct_tokens {
            input_tokens -= deduct_tokens;
        } else {
            input_tokens = 0;
        }
    }
    if input_tokens < 0 {
        input_tokens = 0;
    }
    (
        input_tokens,
        output_tokens,
        cached_tokens,
        cache_write_tokens,
    )
}

// port of setClaudeReasoningUsage (codex_claude_response.go)
fn set_claude_reasoning_usage(out: &mut Map<String, Value>, usage: Option<&Value>) {
    let usage = match usage {
        Some(u) => u,
        None => return,
    };
    let detail = match gp(usage, "output_tokens_details.reasoning_tokens") {
        Some(Value::Number(n)) => n,
        _ => return,
    };
    let num = detail.as_f64().unwrap_or(0.0);
    if num < 0.0 || num.is_sign_negative() {
        return;
    }
    let output_tokens = gi(gp(usage, "output_tokens")).max(0);
    let tokens = if num >= output_tokens as f64 {
        output_tokens
    } else {
        gi(Some(&Value::Number(detail.clone())))
    };
    out.insert(
        "output_tokens_details".into(),
        json!({"thinking_tokens": tokens}),
    );
}

/// The Anthropic `usage` object both the stream and the non-stream path emit.
fn build_claude_usage(usage: Option<&Value>) -> Value {
    let (input_tokens, output_tokens, cached_tokens, cache_write_tokens) =
        extract_responses_usage(usage);
    let mut out = Map::new();
    out.insert("input_tokens".into(), json!(input_tokens));
    out.insert("output_tokens".into(), json!(output_tokens));
    if cached_tokens > 0 {
        out.insert("cache_read_input_tokens".into(), json!(cached_tokens));
    }
    if cache_write_tokens > 0 {
        out.insert(
            "cache_creation_input_tokens".into(),
            json!(cache_write_tokens),
        );
    }
    set_claude_reasoning_usage(&mut out, usage);
    Value::Object(out)
}

// port of codexWebSearchQuery (codex_claude_response_web_search.go)
fn codex_web_search_query(root: &Value, item: &Value) -> String {
    for path in ["action.query", "query", "input.query"] {
        for source in [item, root] {
            let value = gstr(source, path).trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}

/// None mirrors Go's nil (no results array at all); Some(`[]`) means a results
/// array that held no usable entry.
// port of codexWebSearchResultContent (codex_claude_response_web_search.go)
fn codex_web_search_result_content(root: &Value, item: &Value) -> Option<Value> {
    let results = match gp(item, "results") {
        Some(Value::Array(r)) => r,
        _ => match gp(root, "results") {
            Some(Value::Array(r)) => r,
            _ => return None,
        },
    };
    let blocks: Vec<Value> = results
        .iter()
        .filter_map(|result| {
            let url = gstr(result, "url").trim().to_string();
            if url.is_empty() {
                return None;
            }
            let mut title = gstr(result, "title").trim().to_string();
            if title.is_empty() {
                title = url.clone();
            }
            Some(json!({"type": "web_search_result", "title": title, "url": url, "page_age": null}))
        })
        .collect();
    Some(Value::Array(blocks))
}

// port of appendCodexWebSearchNonStreamBlocks (codex_claude_response_web_search.go)
fn append_web_search_non_stream_blocks(
    content_blocks: &mut Vec<Value>,
    item: &Value,
    seen: &mut HashSet<String>,
) {
    let id = gstr(item, "id").trim().to_string();
    if id.is_empty() || seen.contains(&id) {
        return;
    }
    let query = codex_web_search_query(&Value::Null, item);
    let result_content = codex_web_search_result_content(&Value::Null, item);
    if query.is_empty() && result_content.is_none() {
        return;
    }
    let input = if query.is_empty() {
        json!({})
    } else {
        json!({"query": query})
    };
    content_blocks
        .push(json!({"type": "server_tool_use", "id": id, "name": "web_search", "input": input}));
    content_blocks.push(json!({
        "type": "web_search_tool_result",
        "tool_use_id": id,
        "content": result_content.unwrap_or_else(|| json!([])),
    }));
    seen.insert(id);
}

/// Complete upstream non-stream response → client Anthropic Messages response.
///
/// `upstream` is either the final Responses `response` object (what the
/// router's SSE fold yields) or, as in Go, a `response.completed` /
/// `response.incomplete` event wrapping it. Any other `response.*` event (e.g.
/// `response.failed`) returns `Value::Null`, Go's empty output.
// port of ConvertCodexResponseToClaudeNonStream (codex_claude_response.go)
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let rev_names = build_reverse_map_from_claude_original_short_to_original(original_request);

    let type_str = gstr(upstream, "type");
    let response_data = if type_str.starts_with("response.") {
        if type_str != "response.completed" && type_str != "response.incomplete" {
            return Value::Null;
        }
        match gp(upstream, "response") {
            Some(r) => r,
            None => return Value::Null,
        }
    } else {
        upstream
    };

    let mut has_tool_call = false;
    let mut web_search_seen: HashSet<String> = HashSet::new();
    let mut content_blocks: Vec<Value> = Vec::new();

    if let Some(Value::Array(output)) = gp(response_data, "output") {
        for item in output {
            match gstr(item, "type").as_str() {
                "reasoning" => {
                    let mut thinking = String::new();
                    let signature = gstr(item, "encrypted_content");
                    let collect = |v: &Value, into: &mut String| match v {
                        Value::Array(parts) => {
                            for part in parts {
                                match gp(part, "text") {
                                    Some(t) => into.push_str(&gs(Some(t))),
                                    None => into.push_str(&gs(Some(part))),
                                }
                            }
                        }
                        other => into.push_str(&gs(Some(other))),
                    };
                    if let Some(summary) = gp(item, "summary") {
                        collect(summary, &mut thinking);
                    }
                    if thinking.is_empty() {
                        if let Some(content) = gp(item, "content") {
                            collect(content, &mut thinking);
                        }
                    }
                    if !thinking.is_empty() || !signature.is_empty() {
                        let mut block = Map::new();
                        block.insert("type".into(), json!("thinking"));
                        block.insert("thinking".into(), json!(thinking));
                        if !signature.is_empty() {
                            block.insert("signature".into(), json!(signature));
                        }
                        content_blocks.push(Value::Object(block));
                    }
                }
                "message" => match gp(item, "content") {
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            if gstr(part, "type") == "output_text" {
                                let text = gstr(part, "text");
                                if !text.is_empty() {
                                    content_blocks.push(json!({"type": "text", "text": text}));
                                }
                            }
                        }
                    }
                    Some(other) => {
                        let text = gs(Some(other));
                        if !text.is_empty() {
                            content_blocks.push(json!({"type": "text", "text": text}));
                        }
                    }
                    None => {}
                },
                "web_search_call" => {
                    append_web_search_non_stream_blocks(
                        &mut content_blocks,
                        item,
                        &mut web_search_seen,
                    );
                }
                "function_call" => {
                    has_tool_call = true;
                    let name = resolve_codex_claude_tool_use_name(&rev_names, &gstr(item, "name"));
                    let args_str = gstr(item, "arguments");
                    let input = match serde_json::from_str::<Value>(&args_str) {
                        Ok(v @ Value::Object(_)) => v,
                        _ => json!({}),
                    };
                    content_blocks.push(json!({
                        "type": "tool_use",
                        "id": shorten_codex_call_id_if_needed(&sanitize_claude_tool_id(&gstr(item, "call_id"))),
                        "name": name,
                        "input": input,
                    }));
                }
                _ => {}
            }
        }
    }

    let mut out = Map::new();
    out.insert("id".into(), json!(gstr(response_data, "id")));
    out.insert("type".into(), json!("message"));
    out.insert("role".into(), json!("assistant"));
    out.insert("model".into(), json!(gstr(response_data, "model")));
    out.insert("content".into(), Value::Array(content_blocks));
    out.insert(
        "stop_reason".into(),
        json!(map_codex_stop_reason_to_claude(
            &codex_stop_reason(response_data),
            has_tool_call
        )),
    );
    out.insert("stop_sequence".into(), Value::Null);
    set_claude_stop_sequence(&mut out, "stop_sequence", response_data);
    out.insert(
        "usage".into(),
        build_claude_usage(gp(response_data, "usage")),
    );
    Value::Object(out)
}

// ─────────────────────────────────── tests ───────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ───── request helpers ─────

    fn req(model: &str, body: &str) -> Value {
        let body: Value = serde_json::from_str(body).expect("valid test JSON");
        translate_request(model, &body, false)
    }

    fn input(out: &Value) -> &Vec<Value> {
        out["input"].as_array().expect("input array")
    }

    fn types(items: &[Value]) -> Vec<String> {
        items.iter().map(|i| gstr(i, "type")).collect()
    }

    /// port of validCodexReasoningSignature (codex_claude_request_test.go)
    fn valid_codex_reasoning_signature() -> String {
        let mut raw = vec![0u8; 1 + 8 + 16 + 16 + 32];
        raw[0] = 0x80;
        raw[8] = 1;
        base64::engine::general_purpose::URL_SAFE.encode(raw)
    }

    const GROK_SIG: &str = "HmlYdr2aCAqCYP/m9mr8PS6KOsdMs72FGDigmydR+Jsmuv8KX97yWPlbOwmXJgWn0CbHaCacdQD3+n5EvpgLfPNmafS3kdICBjRuDf4bzHy7uBiUhNVhqPtp/ee1y9q4imPE4LYgD1VZ4J+bp9mTeqA1+nC9Oue58CiNEMV9SVaGenCD+aBnVuSTzQhD32Y+68i6HLJW0Dx6ifaRfb8hxYtA/sPM+/FTvAMW11nRho5a2BBSkpnzfqqAz/e/vGJ77/bygpXM823QA9wL9i0X";

    // ───── request tests (codex_claude_request_test.go and friends) ─────

    // A `response.failed` mid-stream (a plain Responses provider — the Codex
    // relay reports it before the translator sees it) is an error event for
    // Claude Code, and nothing is closed as complete afterwards.
    #[test]
    fn stream_response_failed_is_an_error_event() {
        let mut t = StreamTranslator::new(&json!({}));
        let created = t.push(
            None,
            &json!({"type": "response.created", "response": {"id": "resp_4", "model": "gpt-5"}}),
        );
        assert!(created.join("").contains("message_start"));
        let out = t.push(
            None,
            &json!({"type": "response.failed", "response": {"id": "resp_4", "status": "failed",
            "error": {"code": "server_error", "message": "The model produced invalid content."}}}),
        );
        assert_eq!(
            out,
            vec!["event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"The model produced invalid content.\"}}\n\n"]
        );
        assert!(t.finish().is_empty());
    }

    #[test]
    fn request_full_body_exact() {
        let out = req(
            "gpt-5.5",
            r#"{
                "model": "claude-sonnet-4-6",
                "system": [{"type":"text","text":"x-anthropic-billing-header: cc"},{"type":"text","text":"Be terse."}],
                "thinking": {"type":"enabled","budget_tokens":2048},
                "tools": [{"name":"Read","description":"read a file","input_schema":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]},"cache_control":{"type":"ephemeral"}}],
                "messages": [
                    {"role":"user","content":"read a"},
                    {"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a"}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"hello"}]}
                ]
            }"#,
        );
        let want = json!({
            "model": "gpt-5.5",
            "instructions": "",
            "input": [
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"Be terse."}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"read a"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},
                {"type":"function_call","call_id":"toolu_1","name":"Read","arguments":"{\"file_path\":\"a\"}"},
                {"type":"function_call_output","call_id":"toolu_1","output":"hello"}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "medium"},
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "tools": [{"name":"Read","description":"read a file","type":"function","strict":false,
                       "parameters":{"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}}]
        });
        assert_eq!(out, want);
    }

    #[test]
    fn request_system_message_scenarios() {
        let cases: [(&str, Option<Vec<&str>>); 5] = [
            (r#"{"messages":[{"role":"user","content":"hello"}]}"#, None),
            (
                r#"{"system":"","messages":[{"role":"user","content":"hello"}]}"#,
                None,
            ),
            (
                r#"{"system":"Be helpful","messages":[{"role":"user","content":"hello"}]}"#,
                Some(vec!["Be helpful"]),
            ),
            (
                r#"{"messages":[{"role":"system","content":"Follow the project instructions"},{"role":"user","content":"hello"}]}"#,
                None,
            ),
            (
                r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: tenant-123"},{"type":"text","text":"Block 1"},{"type":"text","text":"Block 2"}],"messages":[{"role":"user","content":"hello"}]}"#,
                Some(vec!["Block 1", "Block 2"]),
            ),
        ];
        for (body, want) in cases {
            let out = req("test-model", body);
            let first = &input(&out)[0];
            match want {
                None => assert_ne!(first["role"], json!("developer"), "{body}"),
                Some(texts) => {
                    let content: Vec<Value> = texts
                        .iter()
                        .map(|t| json!({"type":"input_text","text":t}))
                        .collect();
                    assert_eq!(
                        first,
                        &json!({"type":"message","role":"developer","content":content})
                    );
                }
            }
        }
    }

    #[test]
    fn request_message_system_role_wraps_as_user_reminder() {
        let out = req(
            "test-model",
            r#"{"system":[{"type":"text","text":"Top-level rules"}],"messages":[
                {"role":"user","content":"hello"},
                {"role":"system","content":"Follow the project instructions"},
                {"role":"assistant","content":[{"type":"text","text":"ok"}]},
                {"role":"system","content":[{"type":"text","text":"Use the current repo"}]}]}"#,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"Top-level rules"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>\nFollow the project instructions\n</system-reminder>"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>\nUse the current repo\n</system-reminder>"}]}
            ])
        );
    }

    #[test]
    fn request_preserves_tool_adjacency_with_intervening_system_message() {
        let out = req(
            "gpt-5.4",
            r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"Execute tools"}]},
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"call_1","name":"tool_one","input":{"a":1}},
                    {"type":"tool_use","id":"call_2","name":"tool_two","input":{"b":2}}]},
                {"role":"system","content":"Context update between tool call and tool result"},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_2","content":"result 2"},
                    {"type":"tool_result","tool_use_id":"call_1","content":"result 1"},
                    {"type":"text","text":"Now summarize"}]}]}"#,
        );
        assert_eq!(
            out["input"],
            json!([
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Execute tools"}]},
                {"type":"function_call","call_id":"call_1","name":"tool_one","arguments":"{\"a\":1}"},
                {"type":"function_call","call_id":"call_2","name":"tool_two","arguments":"{\"b\":2}"},
                {"type":"function_call_output","call_id":"call_1","output":"result 1"},
                {"type":"function_call_output","call_id":"call_2","output":"result 2"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>\nContext update between tool call and tool result\n</system-reminder>"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Now summarize"}]}
            ])
        );
    }

    #[test]
    fn request_parallel_tool_calls() {
        for (choice, want) in [
            ("", true),
            (
                r#""tool_choice":{"disable_parallel_tool_use":true},"#,
                false,
            ),
            (
                r#""tool_choice":{"disable_parallel_tool_use":false},"#,
                true,
            ),
        ] {
            let out = req(
                "test-model",
                &format!(r#"{{{choice}"messages":[{{"role":"user","content":"hello"}}]}}"#),
            );
            assert_eq!(out["parallel_tool_calls"], json!(want), "{choice}");
        }
    }

    #[test]
    fn request_service_tier() {
        let cases: [(Option<&str>, Option<&str>, Option<&str>); 8] = [
            (Some(r#""priority""#), None, Some("priority")),
            (Some(r#""fast""#), None, Some("priority")),
            (Some(r#""default""#), None, None),
            (Some("true"), None, None),
            (None, Some(r#""fast""#), Some("priority")),
            (None, Some(r#""standard""#), None),
            (None, Some("true"), None),
            (Some(r#""auto""#), Some(r#""fast""#), Some("priority")),
        ];
        for (tier, speed, want) in cases {
            let mut body =
                json!({"model":"gpt-5.4","messages":[{"role":"user","content":"Reply with OK"}]});
            if let Some(t) = tier {
                body["service_tier"] = serde_json::from_str(t).unwrap();
            }
            if let Some(s) = speed {
                body["speed"] = serde_json::from_str(s).unwrap();
            }
            let out = translate_request("gpt-5.4", &body, false);
            assert_eq!(
                out.get("service_tier"),
                want.map(|w| json!(w)).as_ref(),
                "{tier:?} {speed:?}"
            );
        }
    }

    #[test]
    fn request_shortens_long_tool_use_ids() {
        let long_id = format!("toolu_{}", "a".repeat(62));
        let out = req(
            "test-model",
            &format!(
                r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"run pwd"}}]}},
                {{"role":"assistant","content":[{{"type":"tool_use","id":"{long_id}","name":"Bash","input":{{"cmd":"pwd"}}}}]}},
                {{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{long_id}","content":"ok"}}]}}]}}"#
            ),
        );
        let items = input(&out);
        let call_id = gstr(&items[1], "call_id");
        assert_eq!(gstr(&items[2], "call_id"), call_id);
        assert!(call_id.len() <= 64 && call_id != long_id);
        // Deterministic: prefix + "_" + first 8 bytes of sha256, hex.
        let sum = Sha256::digest(long_id.as_bytes());
        let hex: String = sum[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(call_id, format!("{}_{hex}", &long_id[..64 - 17]));
    }

    #[test]
    fn request_tool_choice_mode_mapping() {
        for (choice, want) in [
            (r#"{"type":"any"}"#, "required"),
            (r#"{"type":"none"}"#, "none"),
            (r#"{"type":"auto"}"#, "auto"),
        ] {
            let out = req(
                "test-model",
                &format!(
                    r#"{{"tools":[{{"name":"lookup","description":"Lookup","input_schema":{{"type":"object","properties":{{}}}}}}],"tool_choice":{choice},"messages":[{{"role":"user","content":"hello"}}]}}"#
                ),
            );
            assert_eq!(out["tool_choice"], json!(want));
        }
    }

    #[test]
    fn request_tool_choice_specific_function_uses_converted_name() {
        let long_name =
            "mcp__server_with_a_very_long_name_that_exceeds_sixty_four_characters__search";
        let out = req(
            "test-model",
            &format!(
                r#"{{"tools":[{{"name":"{long_name}","description":"Search","input_schema":{{"type":"object","properties":{{}}}}}}],"tool_choice":{{"type":"tool","name":"{long_name}"}},"messages":[{{"role":"user","content":"hello"}}]}}"#
            ),
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type":"function","name":"mcp__search"})
        );
        assert_eq!(out["tools"][0]["name"], json!("mcp__search"));
    }

    #[test]
    fn request_web_search_tool_mapping() {
        let out = req(
            "test-model",
            r#"{"tools":[{"type":"web_search_20260209","name":"web_search","allowed_domains":["example.com"],"blocked_domains":["blocked.example"],
                "user_location":{"type":"approximate","city":"Beijing","country":"CN","timezone":"Asia/Shanghai"}}],
                "tool_choice":{"type":"tool","name":"web_search"},"messages":[{"role":"user","content":"hello"}]}"#,
        );
        assert_eq!(
            out["tools"],
            json!([{"type":"web_search","filters":{"allowed_domains":["example.com"]},
                    "user_location":{"type":"approximate","city":"Beijing","country":"CN","timezone":"Asia/Shanghai"}}])
        );
        assert_eq!(out["tool_choice"], json!({"type":"web_search"}));
    }

    #[test]
    fn request_web_search_tool_choice_uses_declared_typed_tool_name() {
        let out = req(
            "test-model",
            r#"{"tools":[{"type":"web_search_20250305","name":"browser_search"},
                {"name":"web_search","description":"Local search","input_schema":{"type":"object","properties":{}}}],
                "tool_choice":{"type":"tool","name":"web_search"},"messages":[{"role":"user","content":"hello"}]}"#,
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type":"function","name":"web_search"})
        );
    }

    #[test]
    fn request_assistant_thinking_signature_to_reasoning_item() {
        let sig = valid_codex_reasoning_signature();
        let out = req(
            "test-model",
            &format!(
                r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"visible summary must not be replayed","signature":"{sig}"}},{{"type":"text","text":"visible answer"}}]}},{{"role":"user","content":"continue"}}]}}"#
            ),
        );
        assert_eq!(
            out["input"],
            json!([
                {"type":"reasoning","summary":[],"content":null,"encrypted_content":sig},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"visible answer"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
            ])
        );
    }

    #[test]
    fn request_gpt_prefixed_signature_is_unprefixed() {
        let sig = valid_codex_reasoning_signature();
        let out = req(
            "gpt-5",
            &format!(
                r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"r","signature":"gpt#{sig}"}}]}}]}}"#
            ),
        );
        assert_eq!(
            out["input"],
            json!([{"type":"reasoning","summary":[],"content":null,"encrypted_content":sig}])
        );
    }

    #[test]
    fn request_preserves_base64_pdf_document_content() {
        let out = req(
            "gpt-5.6-sol",
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"before"},
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0xLjQK"}},
                {"type":"text","text":"after"}]}]}"#,
        );
        assert_eq!(
            out["input"],
            json!([{"type":"message","role":"user","content":[
                {"type":"input_text","text":"before"},
                {"type":"input_file","file_data":"data:application/pdf;base64,JVBERi0xLjQK","filename":"document.pdf"},
                {"type":"input_text","text":"after"}]}])
        );
    }

    #[test]
    fn request_preserves_content_order_across_tool_and_reasoning_items() {
        let sig = valid_codex_reasoning_signature();
        let out = req(
            "gpt-5.4",
            &format!(
                r#"{{"system":"system rules","messages":[
                {{"role":"assistant","content":[
                    {{"type":"text","text":"before reasoning"}},
                    {{"type":"thinking","signature":"{sig}"}},
                    {{"type":"text","text":"before tool"}},
                    {{"type":"tool_use","id":"toolu_1","name":"lookup","input":{{"query":"test"}}}},
                    {{"type":"text","text":"after tool"}}]}},
                {{"role":"user","content":[
                    {{"type":"tool_result","tool_use_id":"toolu_1","content":[
                        {{"type":"text","text":"tool output"}},
                        {{"type":"image","source":{{"media_type":"image/png","data":"aW1hZ2U="}}}}]}},
                    {{"type":"text","text":"continue"}}]}}],
                "tools":[{{"name":"lookup","input_schema":{{"type":"object"}}}}]}}"#
            ),
        );
        assert_eq!(
            out["input"],
            json!([
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"system rules"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"before reasoning"}]},
                {"type":"reasoning","summary":[],"content":null,"encrypted_content":sig},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"before tool"}]},
                {"type":"function_call","call_id":"toolu_1","name":"lookup","arguments":"{\"query\":\"test\"}"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"after tool"}]},
                {"type":"function_call_output","call_id":"toolu_1","output":[
                    {"type":"input_text","text":"tool output"},
                    {"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U="}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
            ])
        );
        assert_eq!(
            out["tools"],
            json!([{"name":"lookup","type":"function","parameters":{"type":"object","properties":{}},"strict":false}])
        );
    }

    #[test]
    fn request_assistant_grok_signature_to_reasoning_item() {
        let body = json!({"model":"grok-4.5","messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"summary","signature":GROK_SIG},{"type":"text","text":"answer"}]},
            {"role":"user","content":"next"}]});
        let out = translate_request("grok-4.5", &body, false);
        assert_eq!(
            out["input"][0],
            json!({"type":"reasoning","summary":[],"content":null,"encrypted_content":GROK_SIG})
        );
    }

    #[test]
    fn request_ignores_grok_signature_for_non_grok_targets() {
        let body = json!({"messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"summary","signature":GROK_SIG},{"type":"text","text":"answer"}]},
            {"role":"user","content":"next"}]});
        for model in ["gpt-5.4", "claude-sonnet-4-6"] {
            let out = translate_request(model, &body, false);
            assert!(
                !types(input(&out)).contains(&"reasoning".to_string()),
                "{model}"
            );
        }
    }

    #[test]
    fn request_ignores_non_codex_thinking_signatures() {
        let sig = valid_codex_reasoning_signature();
        for body in [
            format!(r#"{{"messages":[{{"role":"user","content":[{{"type":"thinking","thinking":"user supplied thinking","signature":"{sig}"}},{{"type":"text","text":"hello"}}]}}]}}"#),
            r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"anthropic thinking","signature":"Eo8Canthropic-state"},{"type":"text","text":"visible answer"}]}]}"#.to_string(),
            r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":""}]}]}"#.to_string(),
        ] {
            let out = req("test-model", &body);
            assert!(!types(input(&out)).contains(&"reasoning".to_string()), "{body}");
        }
    }

    #[test]
    fn request_output_config_format() {
        let schema_ok = r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#;
        let out = req(
            "gpt-5.4",
            &format!(
                r#"{{"max_tokens":128,"messages":[{{"role":"user","content":"x"}}],"output_config":{{"format":{{"type":"json_schema","schema":{schema_ok}}}}}}}"#
            ),
        );
        assert_eq!(
            out["text"],
            json!({"format":{"type":"json_schema","name":"cli_proxy_structured_output","strict":true,"schema":serde_json::from_str::<Value>(schema_ok).unwrap()}})
        );

        let out = req(
            "gpt-5.4",
            r#"{"messages":[{"role":"user","content":"hello"}],"output_config":{"format":{"type":"json_schema","name":"custom_schema","strict":false,"schema":{"type":"object"}}}}"#,
        );
        assert_eq!(
            out["text"],
            json!({"format":{"type":"json_schema","name":"custom_schema","strict":false,"schema":{"type":"object"}}})
        );

        let out = req(
            "gpt-5.4",
            r#"{"messages":[{"role":"user","content":"hello"}]}"#,
        );
        assert!(out.get("text").is_none());

        let out = req(
            "gpt-5.4",
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"high"},"messages":[{"role":"user","content":"hello"}]}"#,
        );
        assert!(out.get("text").is_none());
        assert_eq!(out["reasoning"], json!({"effort":"high"}));

        let out = req(
            "gpt-5.4",
            r#"{"messages":[{"role":"user","content":"hello"}],"output_config":{"format":{"type":"json_schema","name":"cli_proxy_structured_output","strict":true,
               "schema":{"type":"object","properties":{"answer":{"type":"string"},"impossible":{"type":"string"}},"required":["answer"],"additionalProperties":false}}}}"#,
        );
        assert_eq!(out["text"]["format"]["strict"], json!(false));
        assert_eq!(
            out["text"]["format"]["name"],
            json!("cli_proxy_structured_output")
        );
    }

    #[test]
    fn request_thinking_effort_mapping() {
        let cases = [
            (r#"{"type":"enabled","budget_tokens":100}"#, "minimal"),
            (r#"{"type":"enabled","budget_tokens":1000}"#, "low"),
            (r#"{"type":"enabled","budget_tokens":30000}"#, "xhigh"),
            (r#"{"type":"enabled","budget_tokens":-5}"#, "medium"),
            (r#"{"type":"adaptive"}"#, "xhigh"),
            (r#"{"type":"disabled"}"#, "none"),
        ];
        for (thinking, want) in cases {
            let out = req(
                "gpt-5",
                &format!(r#"{{"thinking":{thinking},"messages":[]}}"#),
            );
            assert_eq!(out["reasoning"], json!({"effort": want}), "{thinking}");
        }
    }

    #[test]
    fn request_normalizes_non_string_tool_name() {
        let out = req(
            "gpt-test",
            r#"{"messages":[],"tools":[{"name":123,"input_schema":{"type":"object"}}]}"#,
        );
        assert_eq!(out["tools"][0]["name"], json!("123"));
    }

    #[test]
    fn normalize_tool_parameters_strips_nested_schema_and_id() {
        let got = normalize_tool_parameters(Some(&json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "https://example.invalid/root",
            "properties": {
                "q": {"type": "string", "$schema": "http://json-schema.org/draft-07/schema#", "$id": "https://example.invalid/q"},
                "tags": {"type": "array", "items": {"type": "string", "$id": "https://example.invalid/tag"}},
                "mode": {"anyOf": [{"type": "string", "$schema": "http://json-schema.org/draft-07/schema#"}, {"type": "null"}]},
                "refField": {"$ref": "#/$defs/hint"}
            },
            "$defs": {"hint": {"type": "string", "$id": "https://example.invalid/hint"}},
            "required": ["q"]
        })));
        assert_eq!(
            got,
            json!({
                "type": "object",
                "properties": {
                    "q": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "mode": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "refField": {"$ref": "#/$defs/hint"}
                },
                "$defs": {"hint": {"type": "string"}},
                "required": ["q"]
            })
        );
    }

    #[test]
    fn normalize_tool_parameters_preserves_property_names_and_literal_data() {
        let got = normalize_tool_parameters(Some(&json!({
            "type": "object",
            "properties": {
                "$schema": {"type": "string", "$schema": "http://json-schema.org/draft-07/schema#", "$id": "https://example.invalid/sub-schema"},
                "$id": {"type": "string"},
                "config": {"type": "object", "default": {"$id": "default-id-123"}}
            }
        })));
        assert_eq!(
            got,
            json!({"type": "object", "properties": {
                "$schema": {"type": "string"},
                "$id": {"type": "string"},
                "config": {"type": "object", "default": {"$id": "default-id-123"}}
            }})
        );
        let default = json!({"type": "object", "properties": {}});
        assert_eq!(normalize_tool_parameters(None), default);
        assert_eq!(normalize_tool_parameters(Some(&Value::Null)), default);
        assert_eq!(
            normalize_tool_parameters(Some(&json!({"type": ["object", "null"]}))),
            json!({"type": ["object", "null"], "properties": {}})
        );
    }

    #[test]
    fn request_strips_unsupported_unicode_property_escape_patterns() {
        let out = req(
            "gpt-5.6",
            r#"{"messages":[{"role":"user","content":"hello"}],"tools":[{"name":"Artifact","description":"Render","input_schema":{
                "type":"object","properties":{
                    "field":{"type":"string","description":"field to replace","pattern":"^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$"},
                    "asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},
                    "lookahead_safe":{"type":"string","pattern":"^(?!__.*__$).{1,200}$"},
                    "literal_p":{"type":"string","pattern":"^\\\\p{Cc}$"},
                    "nul_guard":{"type":"string","pattern":"^[^\\0]*$"},
                    "nested":{"type":"object","properties":{"inner_field":{"type":"string","pattern":"\\P{L}+"}}},
                    "union_field":{"anyOf":[{"type":"string","pattern":"\\p{N}+"},{"type":"null"}]}},
                "required":["field"]}}]}"#,
        );
        assert_eq!(
            out["tools"][0]["parameters"],
            json!({"type":"object","properties":{
                "field":{"type":"string","description":"field to replace"},
                "asset_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},
                "lookahead_safe":{"type":"string","pattern":"^(?!__.*__$).{1,200}$"},
                "literal_p":{"type":"string","pattern":"^\\\\p{Cc}$"},
                "nul_guard":{"type":"string"},
                "nested":{"type":"object","properties":{"inner_field":{"type":"string"}}},
                "union_field":{"anyOf":[{"type":"string"},{"type":"null"}]}},
              "required":["field"]})
        );
    }

    #[test]
    fn request_strips_pattern_properties_incompatible_keys() {
        let out = req(
            "gpt-5.6",
            r#"{"messages":[{"role":"user","content":"hello"}],"tools":[{"name":"pattern_tool","input_schema":{"type":"object",
                "patternProperties":{"^\\p{L}+$":{"type":"string"},"^[a-z]+$":{"type":"number"}}}}]}"#,
        );
        assert_eq!(
            out["tools"][0]["parameters"],
            json!({"type":"object","patternProperties":{"^[a-z]+$":{"type":"number"}},"properties":{}})
        );
    }

    #[test]
    fn build_short_name_map_uniquifies_collisions() {
        let a = format!("{}_alpha", "x".repeat(70));
        let b = format!("{}_beta", "x".repeat(70));
        let m = build_short_name_map(&[a.clone(), b.clone()]);
        assert_eq!(m[&a], "x".repeat(64));
        assert_eq!(m[&b], format!("{}_1", "x".repeat(62)));
    }

    // ───── stream helpers ─────

    fn stream(original: &str, chunks: &[&str]) -> Vec<String> {
        let original: Value = serde_json::from_str(original).unwrap();
        let mut t = StreamTranslator::new(&original);
        let mut out = Vec::new();
        for chunk in chunks {
            let data: Value = serde_json::from_str(chunk.trim_start_matches("data: ")).unwrap();
            out.extend(t.push(None, &data));
        }
        out
    }

    /// (event, data) for every frame, asserting the frame shape.
    fn frames(outputs: &[String]) -> Vec<(String, Value)> {
        outputs
            .iter()
            .map(|f| {
                assert!(
                    f.ends_with("\n\n"),
                    "frame must end with a blank line: {f:?}"
                );
                let mut lines = f.trim_end_matches('\n').split('\n');
                let event = lines
                    .next()
                    .unwrap()
                    .strip_prefix("event: ")
                    .expect("event line")
                    .to_string();
                let data = lines
                    .next()
                    .unwrap()
                    .strip_prefix("data: ")
                    .expect("data line");
                assert!(lines.next().is_none(), "one event per frame: {f:?}");
                let data: Value = serde_json::from_str(data).unwrap();
                assert_eq!(data["type"], json!(event));
                (event, data)
            })
            .collect()
    }

    fn datas(outputs: &[String]) -> Vec<Value> {
        frames(outputs).into_iter().map(|(_, d)| d).collect()
    }

    fn message_delta(outputs: &[String]) -> Value {
        datas(outputs)
            .into_iter()
            .find(|d| d["type"] == "message_delta")
            .expect("message_delta")
    }

    #[derive(Debug, Default)]
    struct Block {
        index: i64,
        kind: String,
        id: String,
        name: String,
        text: String,
        arguments: String,
    }

    /// port of assertCodexClaudeContentBlockLifecycle (codex_claude_parallel_function_calls_test.go)
    fn lifecycle(outputs: &[String]) -> Vec<Block> {
        let mut open: HashMap<i64, usize> = HashMap::new();
        let mut started: HashSet<i64> = HashSet::new();
        let mut blocks: Vec<Block> = Vec::new();
        let mut message_state = 0;
        for d in datas(outputs) {
            assert_ne!(message_state, 2, "event after message_stop: {d}");
            let index = gi(gp(&d, "index"));
            match gstr(&d, "type").as_str() {
                "content_block_start" => {
                    assert_eq!(message_state, 0);
                    assert!(open.is_empty(), "start while another block is open");
                    assert!(started.insert(index), "index {index} reused");
                    blocks.push(Block {
                        index,
                        kind: gstr(&d, "content_block.type"),
                        id: gstr(&d, "content_block.id"),
                        name: gstr(&d, "content_block.name"),
                        ..Default::default()
                    });
                    open.insert(index, blocks.len() - 1);
                }
                "content_block_delta" => {
                    let b = *open.get(&index).expect("delta targets an open block");
                    match gstr(&d, "delta.type").as_str() {
                        "input_json_delta" => {
                            blocks[b].arguments += &gstr(&d, "delta.partial_json")
                        }
                        "text_delta" => blocks[b].text += &gstr(&d, "delta.text"),
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    assert!(
                        open.remove(&index).is_some(),
                        "stop targets an unopened block"
                    );
                }
                "message_delta" => {
                    assert!(open.is_empty() && message_state == 0);
                    message_state = 1;
                }
                "message_stop" => {
                    assert!(open.is_empty() && message_state == 1);
                    message_state = 2;
                }
                _ => {}
            }
        }
        assert!(open.is_empty(), "content blocks remain open");
        blocks
    }

    // ───── stream tests ─────

    /// End-to-end: a realistic Codex SSE sequence (reasoning + text + a
    /// function call) and the EXACT Anthropic frames it must produce.
    #[test]
    fn stream_end_to_end_exact_frames() {
        let original = json!({"tools":[{"name":"Read","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"read a"}]});
        let mut t = StreamTranslator::new(&original);
        let upstream: Vec<(&str, Value)> = vec![
            (
                "response.created",
                json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_e2e","object":"response","model":"gpt-5.5","status":"in_progress","output":[]}}),
            ),
            (
                "response.in_progress",
                json!({"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_e2e","model":"gpt-5.5","status":"in_progress"}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","encrypted_content":"enc_pre","summary":[]}}),
            ),
            (
                "response.reasoning_summary_part.added",
                json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"Need the file."}),
            ),
            (
                "response.reasoning_summary_text.done",
                json!({"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"text":"Need the file."}),
            ),
            (
                "response.reasoning_summary_part.done",
                json!({"type":"response.reasoning_summary_part.done","item_id":"rs_1","output_index":0,"summary_index":0}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","encrypted_content":"enc_final","summary":[{"type":"summary_text","text":"Need the file."}]}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","status":"in_progress","role":"assistant","content":[]}}),
            ),
            (
                "response.content_part.added",
                json!({"type":"response.content_part.added","item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":""}}),
            ),
            (
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"Reading"}),
            ),
            (
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":" it."}),
            ),
            (
                "response.output_text.done",
                json!({"type":"response.output_text.done","item_id":"msg_1","output_index":1,"content_index":0,"text":"Reading it."}),
            ),
            (
                "response.content_part.done",
                json!({"type":"response.content_part.done","item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":"Reading it."}}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Reading it."}]}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":2,"item":{"id":"fc_1","type":"function_call","status":"in_progress","call_id":"call_1","name":"Read","arguments":""}}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"{\"file_path\":"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"\"/tmp/a\"}"}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":2,"arguments":"{\"file_path\":\"/tmp/a\"}"}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":2,"item":{"id":"fc_1","type":"function_call","status":"completed","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"/tmp/a\"}"}}),
            ),
            (
                "response.completed",
                json!({"type":"response.completed","response":{"id":"resp_e2e","object":"response","model":"gpt-5.5","status":"completed",
                "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":20},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":130},
                "output":[
                    {"id":"rs_1","type":"reasoning","encrypted_content":"enc_final","summary":[{"type":"summary_text","text":"Need the file."}]},
                    {"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading it."}]},
                    {"id":"fc_1","type":"function_call","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"/tmp/a\"}"}]}}),
            ),
        ];
        let mut out = Vec::new();
        for (event, data) in &upstream {
            out.extend(t.push(Some(event), data));
        }
        assert!(t.finish().is_empty(), "terminal already emitted");

        let got = frames(&out);
        let want: Vec<(&str, Value)> = vec![
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"resp_e2e","type":"message","role":"assistant","model":"gpt-5.5","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0},"content":[],"stop_reason":null}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need the file."}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"enc_final"}}),
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
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" it."}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_1","name":"Read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"/tmp/a\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},
                                     "usage":{"input_tokens":80,"output_tokens":30,"cache_read_input_tokens":20,"output_tokens_details":{"thinking_tokens":10}}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let want: Vec<(String, Value)> =
            want.into_iter().map(|(e, d)| (e.to_string(), d)).collect();
        assert_eq!(got, want);
        // Wire shape of one frame, byte for byte.
        assert_eq!(
            out[15],
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }

    #[test]
    fn stream_restores_shortened_tool_name() {
        let long_name =
            "mcp__server_with_a_very_long_name_that_exceeds_sixty_four_characters__search";
        let original =
            format!(r#"{{"tools":[{{"name":"{long_name}","input_schema":{{"type":"object"}}}}]}}"#);
        let out = stream(
            &original,
            &[
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"mcp__search"},"output_index":0}"#,
            ],
        );
        assert_eq!(datas(&out)[0]["content_block"]["name"], json!(long_name));
    }

    #[test]
    fn stream_finish_closes_an_unterminated_stream() {
        let mut t = StreamTranslator::new(&json!({"messages":[]}));
        t.push(
            None,
            &json!({"type":"response.created","response":{"id":"r","model":"m"}}),
        );
        t.push(
            None,
            &json!({"type":"response.content_part.added","part":{"type":"output_text"}}),
        );
        t.push(
            None,
            &json!({"type":"response.output_text.delta","delta":"partial"}),
        );
        let tail = datas(&t.finish());
        assert_eq!(
            tail,
            vec![
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}),
                json!({"type":"message_stop"}),
            ]
        );
        assert!(t.finish().is_empty());

        let mut empty = StreamTranslator::new(&json!({}));
        assert!(empty.finish().is_empty(), "nothing to close");

        let mut errored = StreamTranslator::new(&json!({}));
        errored.push(None, &json!({"type":"error","error":{"message":"boom"}}));
        assert!(
            errored.finish().is_empty(),
            "an error event already ended the stream"
        );
    }

    #[test]
    fn stream_thinking_includes_signature() {
        let out = stream(
            r#"{"messages":[]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_123","model":"gpt-5"}}"#,
                r#"{"type":"response.reasoning_summary_part.added"}"#,
                r#"{"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
                r#"{"type":"response.reasoning_summary_part.done"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_123"}}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"enc_sig_123"}}),
                json!({"type":"content_block_stop","index":0}),
            ]
        );
    }

    #[test]
    fn stream_cyber_policy_error() {
        let out = stream(
            r#"{"messages":[]}"#,
            &[
                r#"{"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk.","param":null},"sequence_number":3}"#,
            ],
        );
        assert_eq!(
            frames(&out),
            vec![(
                "error".to_string(),
                json!({"type":"error","error":{"type":"invalid_request_error","message":"This content was flagged for possible cybersecurity risk."}})
            )]
        );
    }

    #[test]
    fn stream_error_type_fallback_message() {
        let out = stream(
            r#"{"messages":[]}"#,
            &[r#"{"type":"error","error":{},"error_type":"overloaded_error"}"#],
        );
        assert_eq!(
            datas(&out),
            vec![
                json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded_error"}})
            ]
        );
    }

    #[test]
    fn stream_thinking_without_reasoning_item_has_no_signature() {
        let out = stream(
            r#"{"messages":[]}"#,
            &[
                r#"{"type":"response.reasoning_summary_part.added"}"#,
                r#"{"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
                r#"{"type":"response.reasoning_summary_part.done"}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        assert_eq!(
            datas(&out),
            vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ]
        );
    }

    /// port of digestCodexThinkingStream: (starts, stops, signatures, thinking, raw)
    fn digest(chunks: &[&str]) -> (usize, usize, Vec<String>, String, String) {
        let out = stream(r#"{"messages":[]}"#, chunks);
        let (mut starts, mut stops, mut sigs, mut thinking) = (0, 0, vec![], String::new());
        for d in datas(&out) {
            match gstr(&d, "type").as_str() {
                "content_block_start" if gstr(&d, "content_block.type") == "thinking" => {
                    starts += 1
                }
                "content_block_delta" => match gstr(&d, "delta.type").as_str() {
                    "thinking_delta" => thinking += &gstr(&d, "delta.thinking"),
                    "signature_delta" => sigs.push(gstr(&d, "delta.signature")),
                    _ => {}
                },
                "content_block_stop" => stops += 1,
                _ => {}
            }
        }
        (starts, stops, sigs, thinking, out.concat())
    }

    #[test]
    fn stream_thinking_keeps_single_block_across_summary_parts() {
        let (starts, stops, _, thinking, _) = digest(&[
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"First part"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Second part"}"#,
        ]);
        assert_eq!(
            (starts, stops, thinking.as_str()),
            (1, 0, "First part\n\nSecond part")
        );
    }

    #[test]
    fn stream_thinking_single_signature_across_multipart_reasoning() {
        let (starts, stops, sigs, thinking, _) = digest(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_multipart"}}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"First part"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Second part"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning"}}"#,
        ]);
        assert_eq!((starts, stops), (1, 1));
        assert_eq!(sigs, vec!["enc_sig_multipart"]);
        assert_eq!(thinking, "First part\n\nSecond part");
    }

    #[test]
    fn stream_thinking_never_emits_pre_content_encrypted_content() {
        let (starts, stops, sigs, thinking, raw) = digest(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_pre_content_snapshot"}}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Part A"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Part B"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Part C"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_final"}}"#,
        ]);
        assert_eq!((starts, stops), (1, 1));
        assert_eq!(sigs, vec!["enc_sig_final"]);
        assert!(!raw.contains("enc_sig_pre_content_snapshot"));
        assert_eq!(thinking, "Part A\n\nPart B\n\nPart C");
    }

    #[test]
    fn stream_thinking_one_block_per_reasoning_item() {
        let (starts, stops, sigs, _, raw) = digest(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_pre_1"}}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"First item"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_final_1"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_pre_2"}}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Second item"}"#,
            r#"{"type":"response.reasoning_summary_part.done"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_final_2"}}"#,
        ]);
        assert_eq!((starts, stops), (2, 2));
        assert_eq!(sigs, vec!["enc_final_1", "enc_final_2"]);
        assert!(!raw.contains("enc_pre_1") && !raw.contains("enc_pre_2"));
    }

    #[test]
    fn stream_thinking_uses_early_captured_signature_when_done_omits_it() {
        let (_, _, sigs, _, _) = digest(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_early"}}"#,
            r#"{"type":"response.reasoning_summary_part.added"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning"}}"#,
        ]);
        assert_eq!(sigs, vec!["enc_sig_early"]);
    }

    #[test]
    fn stream_signature_only_reasoning_emits_thinking_signature() {
        let out = stream(
            r#"{"messages":[]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_123","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_initial"}}"#,
                r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_only"}}"#,
                r#"{"type":"response.content_part.added"}"#,
                r#"{"type":"response.output_text.delta","delta":"ok"}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"enc_sig_only"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}),
            ]
        );
    }

    #[test]
    fn stream_text_before_tool_calls_does_not_emit_ghost_stop() {
        let out = stream(
            r#"{"tools":[{"name":"Read","description":"read"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"grok-composer-2.5-fast"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"message","status":"in_progress"},"output_index":1}"#,
                r#"{"type":"response.content_part.added","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                r#"{"type":"response.output_text.delta","delta":"查看项目的 README 和核心入口，以便准确说明项目用途。\n","output_index":1}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read","status":"in_progress"},"output_index":2}"#,
                r#"{"type":"response.function_call_arguments.delta","delta":"{\"path\":\"/tmp/README.md\"}","output_index":2}"#,
                r#"{"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp/README.md\"}","output_index":2}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"path\":\"/tmp/README.md\"}"},"output_index":2}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read","status":"in_progress"},"output_index":3}"#,
                r#"{"type":"response.function_call_arguments.delta","delta":"{\"path\":\"/tmp/main.go\"}","output_index":3}"#,
                r#"{"type":"response.content_part.done","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","status":"completed"},"output_index":1}"#,
                r#"{"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp/main.go\"}","output_index":3}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{\"path\":\"/tmp/main.go\"}"},"output_index":3}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        let ds = datas(&out);
        let starts: Vec<i64> = ds
            .iter()
            .filter(|d| d["type"] == "content_block_start")
            .map(|d| gi(gp(d, "index")))
            .collect();
        let stops: Vec<i64> = ds
            .iter()
            .filter(|d| d["type"] == "content_block_stop")
            .map(|d| gi(gp(d, "index")))
            .collect();
        assert_eq!(starts, vec![0, 1, 2]);
        assert_eq!(stops, vec![0, 1, 2]);
        lifecycle(&out);
    }

    #[test]
    fn stream_function_call_defers_start_until_done_name() {
        let original = json!({"tools":[{"name":"web_search","description":"search"}]});
        let mut t = StreamTranslator::new(&original);
        t.push(
            None,
            &json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}),
        );
        let added = t.push(None, &json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1"},"output_index":1}));
        let args = t.push(None, &json!({"type":"response.function_call_arguments.done","arguments":"{\"query\":\"example\"}","output_index":1}));
        let done = t.push(None, &json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"web_search","arguments":"{\"query\":\"example\"}"},"output_index":1}));
        assert!(added.is_empty() && args.is_empty());
        assert_eq!(
            datas(&done),
            vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"web_search","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"example\"}"}}),
                json!({"type":"content_block_stop","index":0}),
            ]
        );
    }

    #[test]
    fn stream_unnamed_function_call_done_by_call_id_keeps_pending_slots() {
        let out = stream(
            r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_first"},"output_index":1}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_second"},"output_index":2}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_first","name":"lookup","arguments":"{\"id\":1}"}}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_second","name":"lookup","arguments":"{\"id\":2}"}}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_first","name":"lookup","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"id\":1}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_second","name":"lookup","input":{}}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"id\":2}"}}),
                json!({"type":"content_block_stop","index":1}),
            ]
        );
    }

    #[test]
    fn stream_deferred_unnamed_function_call_does_not_reserve_block_index() {
        let out = stream(
            r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_hidden"},"output_index":1}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":2}"#,
            ],
        );
        let text_start = datas(&out)
            .into_iter()
            .find(|d| d["content_block"]["type"] == "text")
            .expect("text start");
        assert_eq!(text_start["index"], json!(0));
    }

    #[test]
    fn stream_terminal_output_hydrates_open_function_call_arguments() {
        let out = stream(
            r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"lookup"},"output_index":1}"#,
                r#"{"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"example\"}"}]}}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"lookup","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"example\"}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ]
        );
    }

    #[test]
    fn stream_terminal_output_emits_pending_unnamed_function_call() {
        let out = stream(
            r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1"},"output_index":1}"#,
                r#"{"type":"response.function_call_arguments.done","arguments":"{\"query\":\"example\"}","output_index":1}"#,
                r#"{"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"example\"}"}]}}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"lookup","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"example\"}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ]
        );
    }

    #[test]
    fn stream_unresolved_pending_function_call_does_not_force_tool_use() {
        let original = json!({"tools":[{"name":"lookup","description":"lookup"}]});
        let mut t = StreamTranslator::new(&original);
        let mut out = Vec::new();
        for c in [
            json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_hidden"},"output_index":1}),
            json!({"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[]}}),
        ] {
            out.extend(t.push(None, &c));
        }
        assert!(!out.concat().contains("\"tool_use\""));
        assert_eq!(
            message_delta(&out)["delta"]["stop_reason"],
            json!("end_turn")
        );
        assert!(
            t.p.function_calls.is_empty()
                && t.p.function_call_queue.is_empty()
                && t.p.last_function_call.is_none()
        );
    }

    #[test]
    fn stream_empty_output_uses_output_item_done_message_fallback() {
        let out = stream(
            r#"{"tools":[]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":0}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..4],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                json!({"type":"content_block_stop","index":0}),
            ]
        );
    }

    #[test]
    fn stream_web_search_call_emits_claude_server_tool_blocks() {
        let out = stream(
            r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"search weather"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.4"}}"#,
                r#"{"type":"response.output_item.added","item":{"id":"ws_123","type":"web_search_call","status":"in_progress"}}"#,
                r#"{"type":"response.web_search_call.searching","item_id":"ws_123"}"#,
                r#"{"type":"response.web_search_call.completed","item_id":"ws_123"}"#,
                r#"{"type":"response.output_item.done","item":{"id":"ws_123","type":"web_search_call","status":"completed","action":{"type":"search","query":"search weather"}}}"#,
                r#"{"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2}}}"#,
            ],
        );
        assert_eq!(
            datas(&out)[1..],
            [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"ws_123","name":"web_search","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"search weather\"}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"ws_123","content":[]}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":3,"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ]
        );
    }

    #[test]
    fn stream_web_search_call_reuses_fallback_tool_use_id() {
        let out = stream(
            r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"search weather"}]}"#,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.4"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"web_search_call","status":"in_progress"}}"#,
                r#"{"type":"response.web_search_call.completed","item_id":"ws_from_upstream"}"#,
                r#"{"type":"response.output_item.done","item":{"id":"ws_from_upstream","type":"web_search_call","status":"completed","action":{"type":"search","query":"search weather"}}}"#,
                r#"{"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2}}}"#,
            ],
        );
        let text = out.concat();
        assert_eq!(text.matches("\"type\":\"server_tool_use\"").count(), 1);
        assert!(text.contains("\"tool_use_id\":\"ws_from_upstream\""));
    }

    #[test]
    fn stream_and_non_stream_shorten_long_tool_use_ids() {
        let long_call_id = format!("call_{}", "a".repeat(62));
        let original =
            r#"{"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{}}}]}"#;
        let out = stream(
            original,
            &[&format!(
                r#"{{"type":"response.output_item.added","item":{{"type":"function_call","call_id":"{long_call_id}","name":"lookup"}}}}"#
            )],
        );
        let id = gstr(&datas(&out)[0], "content_block.id");
        assert!(id.len() <= 64 && id != long_call_id);
        assert_eq!(id, shorten_codex_call_id_if_needed(&long_call_id));

        let resp = json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","usage":{"input_tokens":1,"output_tokens":1},
            "output":[{"type":"function_call","call_id":long_call_id,"name":"lookup","arguments":"{}"}]}});
        let ns = translate_non_stream(&resp, &serde_json::from_str(original).unwrap());
        assert_eq!(ns["content"][0]["id"], json!(id));
    }

    #[test]
    fn stream_stop_reason_mapping() {
        let cases: [(&[&str], &str); 4] = [
            (
                &[
                    r#"{"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ],
                "end_turn",
            ),
            (
                &[
                    r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ],
                "max_tokens",
            ),
            (
                &[
                    r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"lookup"}}"#,
                    r#"{"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ],
                "tool_use",
            ),
            (
                &[
                    r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ],
                "refusal",
            ),
        ];
        for (chunks, want) in cases {
            let out = stream(
                r#"{"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{}}}]}"#,
                chunks,
            );
            assert_eq!(message_delta(&out)["delta"]["stop_reason"], json!(want));
        }
    }

    #[test]
    fn stream_stop_sequence_mapping() {
        let out = stream(
            r#"{"messages":[]}"#,
            &[
                r#"{"type":"response.completed","response":{"stop_reason":"stop","stop_sequence":"\nEND","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        assert_eq!(
            message_delta(&out)["delta"],
            json!({"stop_reason":"stop_sequence","stop_sequence":"\nEND"})
        );
    }

    const USAGE_CASES: [(&str, i64, i64, i64, i64); 6] = [
        (
            r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
            50,
            200,
            800,
            150,
        ),
        (
            r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_creation_tokens":150}}"#,
            50,
            200,
            800,
            150,
        ),
        (
            r#"{"input_tokens":500,"output_tokens":100,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":50}}"#,
            0,
            100,
            800,
            50,
        ),
        (
            r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":0}}"#,
            200,
            200,
            800,
            0,
        ),
        (
            r#"{"input_tokens":4022,"output_tokens":462,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":4019}}"#,
            3,
            462,
            0,
            4019,
        ),
        (
            r#"{"input_tokens":500,"output_tokens":100,"input_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
            0,
            100,
            300,
            300,
        ),
    ];

    fn want_usage(input: i64, output: i64, read: i64, write: i64) -> Value {
        let mut u = json!({"input_tokens": input, "output_tokens": output});
        if read > 0 {
            u["cache_read_input_tokens"] = json!(read);
        }
        if write > 0 {
            u["cache_creation_input_tokens"] = json!(write);
        }
        u
    }

    #[test]
    fn stream_and_non_stream_preserve_cache_write_usage() {
        for (usage, input, output, read, write) in USAGE_CASES {
            let out = stream(
                r#"{"messages":[]}"#,
                &[
                    r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                    r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#,
                    &format!(
                        r#"{{"type":"response.completed","response":{{"stop_reason":"stop","usage":{usage}}}}}"#
                    ),
                ],
            );
            assert_eq!(
                message_delta(&out)["usage"],
                want_usage(input, output, read, write),
                "{usage}"
            );

            let resp: Value = serde_json::from_str(&format!(
                r#"{{"type":"response.completed","response":{{"id":"resp_1","model":"gpt-5","stop_reason":"stop","usage":{usage},"output":[{{"type":"message","content":[{{"type":"output_text","text":"ok"}}]}}]}}}}"#
            ))
            .unwrap();
            assert_eq!(
                translate_non_stream(&resp, &json!({"messages":[]}))["usage"],
                want_usage(input, output, read, write),
                "{usage}"
            );
        }
    }

    #[test]
    fn stream_and_non_stream_preserve_reasoning_usage() {
        let cases: [(&str, Option<i64>); 10] = [
            (
                r#"{"input_tokens":420,"output_tokens":518,"output_tokens_details":{"reasoning_tokens":163},"total_tokens":938}"#,
                Some(163),
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":0}}"#,
                Some(0),
            ),
            (r#"{"input_tokens":100,"output_tokens":50}"#, None),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":999}}"#,
                Some(50),
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":-5}}"#,
                None,
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":-0.5}}"#,
                None,
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":9223372036854775808}}"#,
                Some(50),
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":"163"}}"#,
                None,
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":true}}"#,
                None,
            ),
            (
                r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":null}}"#,
                None,
            ),
        ];
        for (usage, want) in cases {
            let usage_v: Value = serde_json::from_str(usage).unwrap();
            let mut want_u = json!({"input_tokens": gi(gp(&usage_v, "input_tokens")), "output_tokens": gi(gp(&usage_v, "output_tokens"))});
            if let Some(t) = want {
                want_u["output_tokens_details"] = json!({"thinking_tokens": t});
            }
            let out = stream(
                r#"{"messages":[]}"#,
                &[
                    r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                    &format!(
                        r#"{{"type":"response.completed","response":{{"stop_reason":"stop","usage":{usage}}}}}"#
                    ),
                ],
            );
            assert_eq!(message_delta(&out)["usage"], want_u, "{usage}");
            let resp = json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","stop_reason":"stop","usage":usage_v,"output":[]}});
            assert_eq!(
                translate_non_stream(&resp, &json!({}))["usage"],
                want_u,
                "{usage}"
            );
        }
    }

    #[test]
    fn extract_responses_usage_table() {
        type Case<'a> = (Option<&'a str>, (i64, i64, i64, i64));
        let cases: [Case; 12] = [
            (None, (0, 0, 0, 0)),
            (Some("null"), (0, 0, 0, 0)),
            (
                Some(r#"{"input_tokens":100,"output_tokens":50}"#),
                (100, 50, 0, 0),
            ),
            (
                Some(
                    r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":30}}"#,
                ),
                (70, 50, 30, 0),
            ),
            (
                Some(
                    r#"{"input_tokens":4022,"output_tokens":462,"input_tokens_details":{"cache_write_tokens":4019}}"#,
                ),
                (3, 462, 0, 4019),
            ),
            (
                Some(
                    r#"{"input_tokens":4022,"output_tokens":462,"input_tokens_details":{"cache_creation_tokens":4019}}"#,
                ),
                (3, 462, 0, 4019),
            ),
            (
                Some(
                    r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
                ),
                (50, 200, 800, 150),
            ),
            (
                Some(
                    r#"{"input_tokens":500,"output_tokens":100,"input_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
                ),
                (0, 100, 300, 300),
            ),
            (
                Some(
                    r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":-10,"cache_write_tokens":-5}}"#,
                ),
                (100, 50, -10, 0),
            ),
            (
                Some(r#"{"input_tokens":-10,"output_tokens":50}"#),
                (0, 50, 0, 0),
            ),
            (
                Some(
                    r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cache_write_tokens":-1,"cache_creation_tokens":40}}"#,
                ),
                (60, 50, 0, 40),
            ),
            (
                Some(
                    r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":9223372036854775800,"cache_write_tokens":100}}"#,
                ),
                (0, 50, 9223372036854775800, 100),
            ),
        ];
        for (raw, want) in cases {
            let v = raw.map(|r| serde_json::from_str::<Value>(r).unwrap());
            assert_eq!(extract_responses_usage(v.as_ref()), want, "{raw:?}");
        }
    }

    // ───── parallel function calls (codex_claude_parallel_function_calls_test.go) ─────

    const READ_TOOLS: &str = r#"{"stream":true,"tools":[{"name":"Read"}]}"#;

    fn assert_parallel_read_calls(blocks: &[Block]) {
        assert_eq!(blocks.len(), 2);
        for (i, (id, args)) in [
            ("call_a", r#"{"file_path":"a"}"#),
            ("call_b", r#"{"file_path":"b"}"#),
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(blocks[i].index, i as i64);
            assert_eq!(
                (blocks[i].kind.as_str(), blocks[i].name.as_str()),
                ("tool_use", "Read")
            );
            assert_eq!(blocks[i].id, *id);
            assert_eq!(blocks[i].arguments, *args);
        }
    }

    #[test]
    fn stream_serializes_interleaved_named_function_calls() {
        let first_finishes_first: &[&str] = &[
            r#"{"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":1}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read"},"output_index":2}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"a\"}","output_index":1}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"b\"}","output_index":2}"#,
            r#"{"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":1}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":1}"#,
            r#"{"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"b\"}","output_index":2}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{\"file_path\":\"b\"}"},"output_index":2}"#,
        ];
        let second_finishes_first: &[&str] = &[
            r#"{"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":1}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read"},"output_index":2}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"b\"}","output_index":2}"#,
            r#"{"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"b\"}","output_index":2}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{\"file_path\":\"b\"}"},"output_index":2}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"a\"}","output_index":1}"#,
            r#"{"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":1}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":1}"#,
        ];
        for chunks in [first_finishes_first, second_finishes_first] {
            assert_parallel_read_calls(&lifecycle(&stream(READ_TOOLS, chunks)));
        }
    }

    #[test]
    fn stream_defers_other_content_until_function_calls_close() {
        for (function_call, first, second) in [
            (
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":0}"#,
                "tool_use",
                "text",
            ),
            (
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a"},"output_index":0}"#,
                "text",
                "tool_use",
            ),
        ] {
            let out = stream(
                READ_TOOLS,
                &[
                    r#"{"type":"response.created","response":{"id":"resp_mixed","model":"gpt-5"}}"#,
                    function_call,
                    r#"{"type":"response.output_item.added","item":{"type":"message","status":"in_progress"},"output_index":1}"#,
                    r#"{"type":"response.content_part.added","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                    r#"{"type":"response.output_text.delta","delta":"done","output_index":1}"#,
                    r#"{"type":"response.content_part.done","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                    r#"{"type":"response.output_item.done","item":{"type":"message","status":"completed"},"output_index":1}"#,
                    r#"{"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":0}"#,
                    r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":0}"#,
                    r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ],
            );
            let blocks = lifecycle(&out);
            assert_eq!(blocks.len(), 2);
            assert_eq!((blocks[0].index, blocks[0].kind.as_str()), (0, first));
            assert_eq!((blocks[1].index, blocks[1].kind.as_str()), (1, second));
            for b in &blocks {
                match b.kind.as_str() {
                    "tool_use" => assert_eq!(b.arguments, r#"{"file_path":"a"}"#),
                    "text" => assert_eq!(b.text, "done"),
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn stream_deferred_text_closes_before_thinking_starts() {
        let out = stream(
            READ_TOOLS,
            &[
                r#"{"type":"response.created","response":{"id":"resp_mixed","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":0}"#,
                r#"{"type":"response.content_part.added","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                r#"{"type":"response.output_text.delta","delta":"answer","output_index":1}"#,
                r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_initial"},"output_index":2}"#,
                r#"{"type":"response.reasoning_summary_part.added","output_index":2}"#,
                r#"{"type":"response.reasoning_summary_text.delta","delta":"thought","output_index":2}"#,
                r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_final"},"output_index":2}"#,
                r#"{"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":0}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":0}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        let blocks = lifecycle(&out);
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            (
                blocks[0].index,
                blocks[0].kind.as_str(),
                blocks[0].arguments.as_str()
            ),
            (0, "tool_use", r#"{"file_path":"a"}"#)
        );
        assert_eq!(
            (
                blocks[1].index,
                blocks[1].kind.as_str(),
                blocks[1].text.as_str()
            ),
            (1, "text", "answer")
        );
        assert_eq!((blocks[2].index, blocks[2].kind.as_str()), (2, "thinking"));
    }

    #[test]
    fn stream_terminal_matches_function_calls_by_output_index() {
        let out = stream(
            READ_TOOLS,
            &[
                r#"{"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","name":"Read"},"output_index":0}"#,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","name":"Read"},"output_index":1}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","name":"Read","arguments":"{\"file_path\":\"a\"}"},{"type":"function_call","name":"Read","arguments":"{\"file_path\":\"b\"}"}]}}"#,
            ],
        );
        let blocks = lifecycle(&out);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            (blocks[0].index, blocks[0].arguments.as_str()),
            (0, r#"{"file_path":"a"}"#)
        );
        assert_eq!(
            (blocks[1].index, blocks[1].arguments.as_str()),
            (1, r#"{"file_path":"b"}"#)
        );
    }

    #[test]
    fn stream_terminal_hydrates_interleaved_function_calls() {
        for terminal_type in ["response.completed", "response.incomplete"] {
            let terminal = format!(
                r#"{{"type":"{terminal_type}","response":{{"usage":{{"input_tokens":1,"output_tokens":1}},"output":[{{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{{\"file_path\":\"a\"}}"}},{{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{{\"file_path\":\"b\"}}"}}]}}}}"#
            );
            let out = stream(
                READ_TOOLS,
                &[
                    r#"{"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
                    r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":0}"#,
                    r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read"},"output_index":1}"#,
                    r#"{"type":"response.function_call_arguments.delta","delta":"{\"file_path\":","output_index":0}"#,
                    &terminal,
                ],
            );
            assert_parallel_read_calls(&lifecycle(&out));
        }
    }

    // ───── non-stream tests ─────

    #[test]
    fn non_stream_exact_with_thinking_text_and_tool() {
        let resp = json!({"id":"resp_123","object":"response","model":"gpt-5","status":"completed",
        "usage":{"input_tokens":10,"output_tokens":20},
        "output":[
            {"type":"reasoning","encrypted_content":"enc_sig_nonstream","summary":[{"type":"summary_text","text":"internal reasoning"}]},
            {"type":"message","content":[{"type":"output_text","text":"final answer"}]},
            {"type":"function_call","call_id":"call.1","name":"mcp__search","arguments":"{\"q\":\"x\"}"}
        ]});
        let long_name =
            "mcp__server_with_a_very_long_name_that_exceeds_sixty_four_characters__search";
        let original = json!({"tools":[{"name":long_name}]});
        // Bare response object (router fold) and the Go event wrapper agree.
        let wrapped = json!({"type":"response.completed","response":resp.clone()});
        let want = json!({
            "id":"resp_123","type":"message","role":"assistant","model":"gpt-5",
            "content":[
                {"type":"thinking","thinking":"internal reasoning","signature":"enc_sig_nonstream"},
                {"type":"text","text":"final answer"},
                {"type":"tool_use","id":"call_1","name":long_name,"input":{"q":"x"}}
            ],
            "stop_reason":"tool_use","stop_sequence":null,
            "usage":{"input_tokens":10,"output_tokens":20}
        });
        assert_eq!(translate_non_stream(&resp, &original), want);
        assert_eq!(translate_non_stream(&wrapped, &original), want);
        assert_eq!(
            translate_non_stream(&json!({"type":"response.failed","response":{}}), &original),
            Value::Null
        );
    }

    #[test]
    fn non_stream_web_search_call_emits_server_tool_blocks() {
        let resp = json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.3-codex-spark","stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2},
            "output":[{"type":"web_search_call","id":"ws_123","status":"completed","action":{"type":"search","query":"search weather"}},
                      {"type":"message","content":[{"type":"output_text","text":"done"}]}]}});
        let out = translate_non_stream(
            &resp,
            &json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
        );
        assert_eq!(
            out["content"],
            json!([
                {"type":"server_tool_use","id":"ws_123","name":"web_search","input":{"query":"search weather"}},
                {"type":"web_search_tool_result","tool_use_id":"ws_123","content":[]},
                {"type":"text","text":"done"}
            ])
        );
        assert_eq!(out["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn non_stream_web_search_dedupes_empty_open_page_items() {
        let resp = json!({"type":"response.completed","response":{"id":"resp_1","model":"m","stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},
            "output":[{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"open_page"}},
                      {"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"weather"},
                       "results":[{"url":"https://example.com","title":""}]},
                      {"type":"message","content":[{"type":"output_text","text":"ok"}]}]}});
        let out = translate_non_stream(&resp, &json!({}));
        assert_eq!(
            out["content"],
            json!([
                {"type":"server_tool_use","id":"ws_1","name":"web_search","input":{"query":"weather"}},
                {"type":"web_search_tool_result","tool_use_id":"ws_1","content":[
                    {"type":"web_search_result","title":"https://example.com","url":"https://example.com","page_age":null}]},
                {"type":"text","text":"ok"}
            ])
        );
    }

    #[test]
    fn non_stream_stop_reason_and_sequence_mapping() {
        let cases = [
            (
                json!({"type":"response.completed","response":{"id":"r","model":"m","stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[]}}),
                "end_turn",
            ),
            (
                json!({"type":"response.incomplete","response":{"id":"r","model":"m","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":1},"output":[]}}),
                "max_tokens",
            ),
            (
                json!({"type":"response.completed","response":{"id":"r","model":"m","stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}]}}),
                "tool_use",
            ),
            (
                json!({"type":"response.incomplete","response":{"id":"r","model":"m","incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":1,"output_tokens":1},"output":[]}}),
                "refusal",
            ),
        ];
        let original =
            json!({"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{}}}]});
        for (resp, want) in cases {
            assert_eq!(
                translate_non_stream(&resp, &original)["stop_reason"],
                json!(want)
            );
        }
        let out = translate_non_stream(
            &json!({"id":"r","model":"m","stop_reason":"stop","stop_sequence":"\nEND","usage":{"input_tokens":1,"output_tokens":1},"output":[]}),
            &json!({}),
        );
        assert_eq!(
            (out["stop_reason"].clone(), out["stop_sequence"].clone()),
            (json!("stop_sequence"), json!("\nEND"))
        );
    }

    // ───── signature helpers ─────

    #[test]
    fn signature_validators() {
        let gpt = valid_codex_reasoning_signature();
        assert!(is_valid_gpt_reasoning_signature(&gpt));
        assert!(is_valid_gpt_reasoning_signature(gpt.trim_end_matches('=')));
        assert!(!is_valid_gpt_reasoning_signature("gAAAAshort"));
        assert_eq!(
            compatible_gpt_signature(&format!("codex#{gpt}")),
            Some(gpt.clone())
        );
        assert_eq!(compatible_gpt_signature(&format!("claude#{gpt}")), None);
        assert_eq!(compatible_gpt_signature(&format!("unknown#{gpt}")), None);

        assert!(is_valid_grok_encrypted_content(GROK_SIG));
        assert!(!is_valid_grok_encrypted_content(&format!(" {GROK_SIG}")));
        assert!(
            !is_valid_grok_encrypted_content(&gpt),
            "GPT envelope is foreign to Grok"
        );
        assert!(
            !is_valid_grok_encrypted_content("QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB"),
            "low entropy"
        );
        assert!(codex_claude_target_accepts_grok_signature("grok-4.5(high)"));
        assert!(!codex_claude_target_accepts_grok_signature("gpt-5"));
    }
}
