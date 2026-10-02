//! OpenAI Chat Completions (client) ⇄ Gemini generateContent (upstream).
//!
//! A faithful port of CLIProxyAPI's
//! `internal/translator/gemini/openai/chat-completions` pair (commit ed980be),
//! registered upstream as
//! `translator.Register(OpenAI, Gemini, ConvertOpenAIRequestToGemini,
//! {Stream: ConvertGeminiResponseToOpenAI, NonStream: ConvertGeminiResponseToOpenAINonStream})`.
//! The CLIENT speaks OpenAI Chat Completions (`/v1/chat/completions`); the
//! UPSTREAM speaks Gemini `generateContent` / `streamGenerateContent` (SSE).
//!
//! Each ported function carries a `// port of <GoFunc> (<file>)` comment so it
//! can be diffed against the Go source. Helpers the pair reaches into other
//! packages for (`internal/util` — including the Gemini JSON-schema cleaner —,
//! `internal/signature`, `internal/misc`, `internal/translator/common`,
//! `internal/translator/gemini/common`) are ported here as far as the pair
//! needs them.
//!
//! Go reads and writes with gjson/sjson, addressed by dotted paths whose keys
//! escape `.`, `*` and `?` with a backslash. The path helpers below reproduce
//! those semantics (a missing path reads as the empty string / zero / false,
//! a write creates the missing parents, a delete keeps sibling order) so the
//! ported branches — the schema cleaner above all, which is written entirely
//! in terms of such paths — read like the original.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use serde_json::{json, Map, Value};

// ───────────────────────────── gjson/sjson-like paths ─────────────────────────────

/// port of escapeGJSONPathKey (util/gemini_schema.go)
fn escape_key(key: &str) -> String {
    if !key.contains(['.', '*', '?']) {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len() + 2);
    for c in key.chars() {
        if matches!(c, '.' | '*' | '?') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// port of unescapeGJSONPathKey (util/gemini_schema.go)
fn unescape_key(key: &str) -> String {
    if !key.contains('\\') {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len());
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// port of splitGJSONPath (util/gemini_schema.go) — segments keep their escapes.
fn split_gjson_path(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                cur.push('\\');
                cur.push(next);
                continue;
            }
        }
        if c == '.' {
            parts.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(c);
    }
    parts.push(cur);
    parts
}

/// The real (unescaped) keys of a gjson path.
fn path_keys(path: &str) -> Vec<String> {
    split_gjson_path(path)
        .iter()
        .map(|s| unescape_key(s))
        .collect()
}

/// gjson `Get`: `None` when the path does not exist (an empty path never
/// exists, as in gjson). `Some(Value::Null)` is an EXISTING null.
fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return None;
    }
    let mut cur = v;
    for key in path_keys(path) {
        cur = match cur {
            Value::Object(m) => m.get(&key)?,
            Value::Array(a) => a.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn set_in(cur: &mut Value, keys: &[String], val: Value) {
    let Some((key, rest)) = keys.split_first() else {
        *cur = val;
        return;
    };
    if !cur.is_object() && !cur.is_array() {
        *cur = Value::Object(Map::new());
    }
    match cur {
        Value::Array(arr) => {
            let idx = if key == "-1" {
                arr.len()
            } else {
                match key.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => return,
                }
            };
            while arr.len() <= idx {
                arr.push(Value::Null);
            }
            set_in(&mut arr[idx], rest, val);
        }
        Value::Object(m) => {
            if rest.is_empty() {
                m.insert(key.clone(), val);
            } else {
                let child = m.entry(key.clone()).or_insert(Value::Null);
                set_in(child, rest, val);
            }
        }
        _ => {}
    }
}

/// sjson `Set`: missing parents are created, an existing key keeps its place,
/// a new key is appended, `-1` appends to an array. An empty path is a no-op.
fn set_path(v: &mut Value, path: &str, val: Value) {
    if path.is_empty() {
        return;
    }
    set_in(v, &path_keys(path), val);
}

/// sjson `Delete`, keeping sibling order.
fn delete_path(v: &mut Value, path: &str) {
    let keys = path_keys(path);
    let Some((last, parents)) = keys.split_last() else {
        return;
    };
    let mut cur = v;
    for key in parents {
        cur = match cur {
            Value::Object(m) => match m.get_mut(key) {
                Some(c) => c,
                None => return,
            },
            Value::Array(a) => match key.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                Some(c) => c,
                None => return,
            },
            _ => return,
        };
    }
    match cur {
        Value::Object(m) => {
            m.shift_remove(last);
        }
        Value::Array(a) => {
            if let Ok(i) = last.parse::<usize>() {
                if i < a.len() {
                    a.remove(i);
                }
            }
        }
        _ => {}
    }
}

/// gjson `Result.String()`: strings verbatim, integers as written, other
/// numbers via `FormatFloat(f, 'f', -1, 64)`, booleans as words, null as "",
/// objects/arrays as their JSON text.
fn gstr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => {
            if n.is_i64() || n.is_u64() {
                n.to_string()
            } else {
                n.as_f64().map(|f| format!("{f}")).unwrap_or_default()
            }
        }
        Some(other) => other.to_string(),
    }
}

/// gjson `Result.Int()`.
fn gint(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Bool(true)) => 1,
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
        _ => 0,
    }
}

/// gjson `Result.Bool()`.
fn gbool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "t" | "T" | "true" | "TRUE" | "True"),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

/// gjson `Result.Num` for a JSON number.
fn gnum(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

/// sjson writes a float64 through `json.Marshal`, so an integral value goes
/// out as `1`, not `1.0`.
fn float_value(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

/// `json.Marshal(gjson.Result.Value())`: numbers become float64, objects
/// become Go maps (keys sorted on output).
fn go_value(v: &Value) -> Value {
    match v {
        Value::Number(n) => float_value(n.as_f64().unwrap_or(0.0)),
        Value::Array(a) => Value::Array(a.iter().map(go_value).collect()),
        Value::Object(_) => sort_keys(v),
        other => other.clone(),
    }
}

/// A Go `map[string]any` marshals with its keys sorted, at every level.
fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sort_keys(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// A Go `[]string` marshals as `null` when nil (never appended to).
fn strs_value(v: Vec<String>) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        Value::Array(v.into_iter().map(Value::String).collect())
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Go `fmt.Sprintf("%s-%d-%d", name, time.Now().UnixNano(), atomic.AddUint64(&functionCallIDCounter, 1))`.
fn function_call_id(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{name}-{nanos}-{n}")
}

/// Go `time.Parse(time.RFC3339Nano, s)` → Unix seconds.
fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}

// ───────────────────────────── util helpers ─────────────────────────────

// port of SanitizeFunctionName (util/util.go)
fn sanitize_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    // [^a-zA-Z0-9_.:-] → "_", one per rune.
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if let Some(first) = sanitized.bytes().next() {
        if !(first.is_ascii_alphabetic() || first == b'_') {
            if sanitized.len() >= 64 {
                sanitized.truncate(63);
            }
            sanitized = format!("_{sanitized}");
        }
    } else {
        sanitized = "_".to_string();
    }
    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }
    sanitized
}

// port of SanitizedToolNameMap (util/translator.go)
fn sanitized_tool_name_map(raw: &Value) -> Option<HashMap<String, String>> {
    let Some(Value::Array(tools)) = raw.get("tools") else {
        return None;
    };
    let mut out = HashMap::new();
    for tool in tools {
        let name = gstr(tool.get("name")).trim().to_string();
        if name.is_empty() {
            continue;
        }
        let sanitized = sanitize_function_name(&name);
        if sanitized == name {
            continue;
        }
        out.entry(sanitized).or_insert(name);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// port of RestoreSanitizedToolName (util/translator.go)
fn restore_sanitized_tool_name(map: Option<&HashMap<String, String>>, sanitized: &str) -> String {
    if sanitized.is_empty() {
        return String::new();
    }
    match map.and_then(|m| m.get(sanitized)) {
        Some(original) => original.clone(),
        None => sanitized.to_string(),
    }
}

// ───────────────────────────── translator/common ─────────────────────────────

const CLAUDE_SYSTEM_REMINDER_START: &str = "<system-reminder>";
const CLAUDE_SYSTEM_REMINDER_END: &str = "</system-reminder>";

// port of SystemReminderText (translator/common/claude_system.go)
fn system_reminder_text(text: &str) -> String {
    format!("{CLAUDE_SYSTEM_REMINDER_START}\n{text}\n{CLAUDE_SYSTEM_REMINDER_END}")
}

/// Go `filepath.Ext` (unix separator).
fn file_ext(filename: &str) -> &str {
    for (i, b) in filename.bytes().enumerate().rev() {
        if b == b'/' {
            break;
        }
        if b == b'.' {
            return &filename[i..];
        }
    }
    ""
}

// port of NormalizeOpenAIFileData (translator/common/file_data.go)
fn normalize_openai_file_data(
    filename: &str,
    fallback_mime_type: &str,
    file_data: &str,
) -> Option<(String, String)> {
    if file_data.is_empty() {
        return None;
    }
    let mut fallback = fallback_mime_type.to_string();
    if fallback.is_empty() {
        let ext = file_ext(filename)
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if let Ok(i) = MIME_TYPES.binary_search_by(|(k, _)| (*k).cmp(ext.as_str())) {
            fallback = MIME_TYPES[i].1.to_string();
        }
    }
    let bytes = file_data.as_bytes();
    if bytes.len() < 5 || !bytes[..5].eq_ignore_ascii_case(b"data:") {
        if fallback.is_empty() {
            return None;
        }
        return Some((fallback, file_data.to_string()));
    }
    let rest = &file_data[5..];
    let (metadata, payload) = rest.split_once(',')?;
    if payload.is_empty() {
        return None;
    }
    let mut fields = metadata.split(';');
    let mime_type = fields.next().unwrap_or("").trim();
    if mime_type.is_empty() {
        return None;
    }
    for field in fields {
        if field.trim().eq_ignore_ascii_case("base64") {
            return Some((mime_type.to_string(), payload.to_string()));
        }
    }
    None
}

// ───────────────────────────── gemini/common ─────────────────────────────

// port of AttachDefaultSafetySettings + DefaultSafetySettings (translator/gemini/common/safety.go)
fn attach_default_safety_settings(out: &mut Value, path: &str) {
    if get_path(out, path).is_some() {
        return;
    }
    set_path(
        out,
        path,
        json!([
            {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "BLOCK_NONE"},
        ]),
    );
}

// ───────────────────────────── signature (Gemini replay) ─────────────────────────────

const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";
const MAX_GEMINI_THOUGHT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

const fn lenient(padding: DecodePaddingMode) -> GeneralPurpose {
    GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(padding)
            .with_decode_allow_trailing_bits(true),
    )
}
/// Go `base64.StdEncoding` (non-strict: trailing bits tolerated).
const B64_STD: GeneralPurpose = lenient(DecodePaddingMode::RequireCanonical);
/// Go `base64.RawStdEncoding`.
const B64_RAW_STD: GeneralPurpose = lenient(DecodePaddingMode::RequireNone);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SigProvider {
    Claude,
    Gemini,
    Gpt,
    Swe,
}

// port of IsGeminiThoughtSignatureBypass (signature/gemini_validation.go)
fn is_gemini_thought_signature_bypass(raw: &str) -> bool {
    matches!(
        raw.trim(),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR | GEMINI_CONTEXT_ENGINEERING_BYPASS
    )
}

// port of SignatureProviderFromCachePrefix (signature/provider_compatibility.go)
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

// port of SplitSignatureProviderPrefix (signature/provider_compatibility.go)
fn split_signature_provider_prefix(raw: &str) -> Option<(SigProvider, String)> {
    let (prefix, rest) = raw.trim().split_once('#')?;
    let provider = signature_provider_from_cache_prefix(prefix)?;
    Some((provider, rest.trim().to_string()))
}

// port of SignaturePayloadWithoutProviderPrefix (signature/provider_compatibility.go)
fn signature_payload_without_provider_prefix(raw: &str) -> String {
    match split_signature_provider_prefix(raw) {
        Some((_, unprefixed)) => unprefixed,
        None => raw.trim().to_string(),
    }
}

/// protowire.ConsumeVarint
fn consume_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..10 {
        let byte = *b.get(i)?;
        if i == 9 && byte > 1 {
            return None;
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

/// protowire.ConsumeTag → (field number, wire type, consumed)
fn consume_tag(b: &[u8]) -> Option<(u64, u8, usize)> {
    let (v, n) = consume_varint(b)?;
    let num = v >> 3;
    if num > i32::MAX as u64 || num < 1 {
        return None;
    }
    Some((num, (v & 7) as u8, n))
}

/// protowire.ConsumeBytes → (value, consumed)
fn consume_bytes(b: &[u8]) -> Option<(&[u8], usize)> {
    let (len, n) = consume_varint(b)?;
    let len = usize::try_from(len).ok()?;
    let end = n.checked_add(len)?;
    if end > b.len() {
        return None;
    }
    Some((&b[n..end], end))
}

// port of consumeGeminiField2Field1Value (signature/gemini_validation.go)
fn consume_gemini_field2_field1_value(decoded: &[u8]) -> Option<&[u8]> {
    let (num, typ, n) = consume_tag(decoded)?;
    if num != 2 || typ != 2 {
        return None;
    }
    let (container, m) = consume_bytes(&decoded[n..])?;
    if n + m != decoded.len() {
        return None;
    }
    let (num, typ, n) = consume_tag(container)?;
    if num != 1 || typ != 2 {
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
        match typ {
            0 => match consume_varint(&value[offset..]) {
                Some((_, n)) => offset += n,
                None => return false,
            },
            2 => match consume_bytes(&value[offset..]) {
                Some((bytes, n)) => {
                    offset += n;
                    if is_likely_gemini_opaque_payload(bytes) {
                        has_tink_field = true;
                    }
                }
                None => return false,
            },
            5 => {
                if value.len() - offset < 4 {
                    return false;
                }
                offset += 4;
            }
            1 => {
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

// port of isGeminiField2Envelope + inspectGeminiField2Envelope (signature/gemini_validation.go)
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

// port of InspectGeminiThoughtSignature with {RequireKnownEnvelope: true}
// (signature/gemini_validation.go), reduced to its verdict. Its Claude-CAIS and
// bypass-sentinel rejections cannot change the verdict here: a CAIS payload
// starts with 0x08 and the only known envelope starts with 0x12, and the
// sentinels are not base64 of a field-2 envelope.
fn is_valid_gemini_known_envelope_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || is_gemini_thought_signature_bypass(sig) {
        return false;
    }
    if sig.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN {
        return false;
    }
    let decoded = match B64_STD.decode(sig) {
        Ok(d) => d,
        Err(_) => match B64_RAW_STD.decode(sig) {
            Ok(d) => d,
            Err(_) => return false,
        },
    };
    if decoded.is_empty() || is_ascii_uuid_bytes(&decoded) {
        return false;
    }
    is_gemini_field2_envelope(&decoded)
}

/// What DetectSignatureProviderForBlock returns, reduced to the two outcomes a
/// Gemini target cares about.
#[derive(PartialEq, Eq, Debug)]
enum GeminiDetected {
    Gemini,
    GeminiBypass,
    Other,
}

// port of DetectSignatureProviderForBlock (signature/provider_compatibility.go),
// reduced to "is it Gemini / the Gemini bypass sentinel". The GPT, Claude and
// Kimi probes it runs first only decide WHICH non-Gemini family a signature
// belongs to: a known Gemini envelope decodes to 0x12 (base64 'E'), which no
// GPT ("gAAAA") or CAIS (0x08) payload can, and the Go suite pins that no
// Claude single/double-layer signature is ever a Gemini envelope
// (TestGeminiEnvelopeNeverClaimsClaudeSignatures) — so the order of the probes
// cannot move a signature into or out of the Gemini family.
fn detect_gemini_signature(raw: &str) -> GeminiDetected {
    let sig = raw.trim();
    if sig.is_empty() {
        return GeminiDetected::Other;
    }
    if let Some((provider, unprefixed)) = split_signature_provider_prefix(sig) {
        if provider == SigProvider::Gemini {
            if is_gemini_thought_signature_bypass(&unprefixed) {
                return GeminiDetected::GeminiBypass;
            }
            if is_valid_gemini_known_envelope_signature(&unprefixed) {
                return GeminiDetected::Gemini;
            }
        }
        return GeminiDetected::Other;
    }
    if sig.contains('#') {
        return GeminiDetected::Other;
    }
    if is_gemini_thought_signature_bypass(sig) {
        return GeminiDetected::GeminiBypass;
    }
    if sig.starts_with("sealed.v1.") {
        return GeminiDetected::Other;
    }
    // maybeSelfDescribingSignatureEnvelope: first char in "CERg".
    if matches!(sig.as_bytes()[0], b'C' | b'E' | b'R' | b'g')
        && is_valid_gemini_known_envelope_signature(sig)
    {
        return GeminiDetected::Gemini;
    }
    GeminiDetected::Other
}

// port of GeminiReplaySignatureOrBypass (signature/gemini_sanitize.go) for
// SignatureBlockKindGeminiFunctionCall: a compatible Gemini signature (or the
// bypass sentinel itself) is replayed without its provider prefix; anything
// else becomes the bypass sentinel.
fn gemini_replay_signature_or_bypass(raw: &str) -> String {
    match detect_gemini_signature(raw) {
        GeminiDetected::Gemini | GeminiDetected::GeminiBypass => {
            // normalizeCompatibleSignatureForProvider
            let payload = signature_payload_without_provider_prefix(raw);
            if is_gemini_thought_signature_bypass(&payload)
                || is_valid_gemini_known_envelope_signature(&payload)
            {
                return payload;
            }
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
        }
        GeminiDetected::Other => GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string(),
    }
}

// ───────────────────────────── Gemini JSON-schema cleaner ─────────────────────────────
// port of util/gemini_schema.go, restricted to the option set of
// CleanJSONSchemaForGeminiJSONSchema: addMissingArrayItems, removeGeminiMetadata,
// flattenUnions, forceEnumStringType, preserveAllAdditionalProperties,
// preserveStandardConstraints (everything else false). Under those options
// inlineLocalRefs, dropIgnoredEnumsToHints, addAdditionalPropertiesHints,
// moveConstraintsToDescription, moveNotToDescription and
// addEmptySchemaPlaceholder are no-ops and are not ported.

const PLACEHOLDER_REASON_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";

// port of CleanJSONSchemaForGeminiJSONSchema + cleanJSONSchema (util/gemini_schema.go)
fn clean_json_schema_for_gemini_json_schema(schema: &Value) -> Value {
    // Phase 0
    let mut s = normalize_malformed_schema_objects(schema);
    // Phase 1
    convert_refs_to_hints(&mut s);
    convert_const_to_enum(&mut s);
    convert_enum_values_to_strings(&mut s);
    add_enum_hints(&mut s);
    // Phase 2
    merge_conditionals(&mut s);
    merge_all_of(&mut s);
    flatten_any_of_one_of(&mut s);
    flatten_type_arrays(&mut s);
    // Phase 3
    remove_unsupported_keywords(&mut s);
    remove_keywords(&mut s, &["nullable", "title"]);
    remove_placeholder_fields(&mut s);
    cleanup_required_fields(&mut s);
    sanitize_array_items(&mut s);
    s
}

// port of Walk (util/translator.go) via findPaths (util/gemini_schema.go)
fn find_paths(v: &Value, field: &str) -> Vec<String> {
    fn walk(v: &Value, path: &str, field: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, child) in m {
                    let child_path = join_path(path, &escape_key(k));
                    if k == field {
                        out.push(child_path.clone());
                    }
                    walk(child, &child_path, field, out);
                }
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    let key = i.to_string();
                    let child_path = join_path(path, &key);
                    if key == field {
                        out.push(child_path.clone());
                    }
                    walk(child, &child_path, field, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, "", field, &mut out);
    out
}

// port of findPathsByFields + walkForFields (util/gemini_schema.go)
fn find_paths_by_fields(v: &Value, fields: &[&str]) -> HashMap<String, Vec<String>> {
    fn walk(v: &Value, path: &str, fields: &HashSet<&str>, out: &mut HashMap<String, Vec<String>>) {
        let children: Vec<(String, &Value)> = match v {
            Value::Object(m) => m.iter().map(|(k, c)| (k.clone(), c)).collect(),
            Value::Array(a) => a
                .iter()
                .enumerate()
                .map(|(i, c)| (i.to_string(), c))
                .collect(),
            _ => return,
        };
        for (k, child) in children {
            let child_path = join_path(path, &escape_key(&k));
            if fields.contains(k.as_str()) {
                out.entry(k.clone()).or_default().push(child_path.clone());
            }
            walk(child, &child_path, fields, out);
        }
    }
    let set: HashSet<&str> = fields.iter().copied().collect();
    let mut out = HashMap::new();
    walk(v, "", &set, &mut out);
    out
}

// port of sortByDepth (util/gemini_schema.go) — stable, deepest first.
fn sort_by_depth(paths: &mut [String]) {
    paths.sort_by_key(|p| std::cmp::Reverse(split_gjson_path(p).len()));
}

// port of trimSuffix (util/gemini_schema.go)
fn trim_suffix(path: &str, suffix: &str) -> String {
    if path == suffix.strip_prefix('.').unwrap_or(suffix) {
        return String::new();
    }
    path.strip_suffix(suffix).unwrap_or(path).to_string()
}

// port of joinPath (util/gemini_schema.go)
fn join_path(base: &str, suffix: &str) -> String {
    if base.is_empty() {
        suffix.to_string()
    } else {
        format!("{base}.{suffix}")
    }
}

// port of setRawAt (util/gemini_schema.go)
fn set_raw_at(s: &mut Value, path: &str, value: Value) {
    if path.is_empty() {
        *s = value;
    } else {
        set_path(s, path, value);
    }
}

// port of isPropertyDefinition (util/gemini_schema.go)
fn is_property_definition(path: &str) -> bool {
    const NAME_MAPS: [&str; 5] = [
        "properties",
        "patternProperties",
        "dependentSchemas",
        "$defs",
        "definitions",
    ];
    let segments = split_gjson_path(path);
    let trailing = segments
        .iter()
        .rev()
        .take_while(|s| NAME_MAPS.contains(&unescape_key(s).as_str()))
        .count();
    trailing % 2 == 1
}

// port of descriptionPath (util/gemini_schema.go)
fn description_path(parent: &str) -> String {
    if parent.is_empty() || parent == "@this" {
        "description".to_string()
    } else {
        format!("{parent}.description")
    }
}

// port of mergeHint (util/gemini_schema.go)
fn merge_hint(existing: &str, hint: &str) -> String {
    if existing.is_empty() {
        return hint.to_string();
    }
    if existing == hint
        || existing.starts_with(&format!("{hint} ("))
        || existing.contains(&format!("({hint})"))
    {
        return existing.to_string();
    }
    format!("{existing} ({hint})")
}

// port of appendHint (util/gemini_schema.go)
fn append_hint(s: &mut Value, parent: &str, hint: &str) {
    let desc = description_path(parent);
    let merged = merge_hint(&gstr(get_path(s, &desc)), hint);
    set_path(s, &desc, Value::String(merged));
}

// port of appendHintRaw (util/gemini_schema.go)
fn append_hint_raw(raw: &mut Value, hint: &str) {
    if let Value::Object(m) = raw {
        let merged = merge_hint(&gstr(m.get("description")), hint);
        m.insert("description".into(), Value::String(merged));
    }
}

// port of mergeDescriptionRaw (util/gemini_schema.go)
fn merge_description_raw(raw: &mut Value, parent_desc: &str) {
    let Value::Object(m) = raw else {
        return;
    };
    let child = gstr(m.get("description"));
    if child.is_empty() {
        m.insert("description".into(), Value::String(parent_desc.to_string()));
    } else if child != parent_desc {
        m.insert(
            "description".into(),
            Value::String(format!("{parent_desc} ({child})")),
        );
    }
}

// port of getStrings (util/gemini_schema.go)
fn get_strings(s: &Value, path: &str) -> Vec<String> {
    match get_path(s, path) {
        Some(Value::Array(a)) => a.iter().map(|r| gstr(Some(r))).collect(),
        _ => Vec::new(),
    }
}

// port of normalizeMalformedSchemaObjects (util/gemini_schema.go). When the
// repair changes anything, Go re-marshals its `map[string]any`, so the whole
// result comes out with sorted keys.
fn normalize_malformed_schema_objects(schema: &Value) -> Value {
    match schema {
        Value::Bool(true) => json!({}),
        Value::Object(root) => {
            if is_api_request_document(root) {
                return schema.clone();
            }
            if root.len() == 1 {
                match root.get("schema") {
                    Some(Value::Object(inner)) => {
                        let (repaired, modified) = repair_schema_node(inner);
                        if !modified {
                            return schema.clone();
                        }
                        return sort_keys(&json!({"schema": Value::Object(repaired)}));
                    }
                    Some(Value::Bool(true)) => return json!({"schema": {}}),
                    _ => {}
                }
            }
            let (repaired, modified) = repair_schema_node(root);
            if !modified {
                return schema.clone();
            }
            sort_keys(&Value::Object(repaired))
        }
        _ => schema.clone(),
    }
}

// port of isKnownSchemaKeywordOrExtension (util/gemini_schema.go)
fn is_known_schema_keyword_or_extension(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

// port of isNonObjectDeclaredType (util/gemini_schema.go)
fn is_non_object_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("object"),
        Some(Value::Array(a)) => {
            if a.iter()
                .any(|i| matches!(i, Value::String(s) if s.eq_ignore_ascii_case("object")))
            {
                return false;
            }
            !a.is_empty()
        }
        _ => false,
    }
}

// port of isArrayDeclaredType (util/gemini_schema.go)
fn is_array_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => s.eq_ignore_ascii_case("array"),
        Some(Value::Array(a)) => a
            .iter()
            .any(|i| matches!(i, Value::String(s) if s.eq_ignore_ascii_case("array"))),
        _ => false,
    }
}

// port of isAPIRequestDocument (util/gemini_schema.go)
fn is_api_request_document(m: &Map<String, Value>) -> bool {
    for key in [
        "tools",
        "contents",
        "messages",
        "functionDeclarations",
        "function_declarations",
    ] {
        if matches!(m.get(key), Some(Value::Array(_))) {
            return true;
        }
    }
    matches!(m.get("request"), Some(Value::Object(r)) if is_api_request_document(r))
}

// port of extractStringArray (util/gemini_schema.go)
fn extract_string_array(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

// port of mergeStringSlices (util/gemini_schema.go)
fn merge_string_slices(existing: &[String], promoted: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for s in existing.iter().chain(promoted) {
        if !s.is_empty() && seen.insert(s.clone()) {
            out.push(s.clone());
        }
    }
    out
}

// port of repairSchemaNode (util/gemini_schema.go)
fn repair_schema_node(node: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut modified = false;
    let mut clone = node.clone();

    // 1. Bare property maps on a node that is not a primitive/array type.
    if !is_non_object_declared_type(clone.get("type")) {
        let mut bare = Map::new();
        for (k, v) in &clone {
            if v.is_object() && !is_known_schema_keyword_or_extension(k) {
                bare.insert(k.clone(), v.clone());
            }
        }
        if !bare.is_empty() {
            let (repaired, promoted, _) = repair_property_map(&bare);
            for k in bare.keys() {
                clone.shift_remove(k);
            }
            if let Some(Value::Object(existing)) = clone.get("properties") {
                let mut merged = existing.clone();
                for (k, v) in repaired {
                    merged.insert(k, v);
                }
                clone.insert("properties".into(), Value::Object(merged));
            } else {
                clone.insert("properties".into(), Value::Object(repaired));
                if !clone.contains_key("type") {
                    clone.insert("type".into(), json!("object"));
                }
            }
            if !promoted.is_empty() {
                let existing = extract_string_array(clone.get("required"));
                clone.insert(
                    "required".into(),
                    strs_value(merge_string_slices(&existing, &promoted)),
                );
            }
            modified = true;
        }
    }

    // 2. Repair every property definition.
    if let Some(Value::Object(props)) = clone.get("properties") {
        let (repaired, promoted, props_mod) = repair_property_map(props);
        if props_mod {
            clone.insert("properties".into(), Value::Object(repaired));
            modified = true;
        }
        if !promoted.is_empty() {
            let existing = extract_string_array(clone.get("required"));
            clone.insert(
                "required".into(),
                strs_value(merge_string_slices(&existing, &promoted)),
            );
            modified = true;
        }
    }

    // Tool array schemas need items; items imply an array type.
    if is_array_declared_type(clone.get("type")) {
        if !clone.contains_key("items") {
            clone.insert("items".into(), json!({"type": "string"}));
            modified = true;
        }
    } else if clone.contains_key("items") {
        // Go: clone["type"] == nil || clone["type"] == ""
        let untyped = match clone.get("type") {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.is_empty(),
            _ => false,
        };
        if untyped {
            clone.insert("type".into(), json!("array"));
            modified = true;
        }
    }

    // 3. Recurse into the standard schema containers.
    match clone.get("items") {
        Some(Value::Object(items)) => {
            let (repaired, m) = repair_schema_node(items);
            if m {
                clone.insert("items".into(), Value::Object(repaired));
                modified = true;
            }
        }
        Some(Value::Array(list)) => {
            let (repaired, m) = repair_schema_list(list);
            if m {
                clone.insert("items".into(), Value::Array(repaired));
                modified = true;
            }
        }
        Some(Value::Bool(true)) => {
            clone.insert("items".into(), json!({}));
            modified = true;
        }
        _ => {}
    }

    if let Some(Value::Object(add)) = clone.get("additionalProperties") {
        let (repaired, m) = repair_schema_node(add);
        if m {
            clone.insert("additionalProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    if let Some(Value::Object(pat)) = clone.get("patternProperties") {
        let (repaired, _, m) = repair_property_map(pat);
        if m {
            clone.insert("patternProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    for key in [
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "additionalItems",
    ] {
        match clone.get(key) {
            Some(Value::Object(sub)) => {
                let (repaired, m) = repair_schema_node(sub);
                if m {
                    clone.insert(key.into(), Value::Object(repaired));
                    modified = true;
                }
            }
            Some(Value::Bool(true)) => {
                clone.insert(key.into(), json!({}));
                modified = true;
            }
            _ => {}
        }
    }

    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list)) = clone.get(key) {
            let (repaired, m) = repair_schema_list(list);
            if m {
                clone.insert(key.into(), Value::Array(repaired));
                modified = true;
            }
        }
    }

    for key in ["$defs", "definitions", "dependentSchemas", "dependencies"] {
        if let Some(Value::Object(defs)) = clone.get(key) {
            let mut repaired_defs = Map::new();
            let mut defs_modified = false;
            for (dk, dv) in defs {
                match dv {
                    Value::Object(def) => {
                        let (repaired, m) = repair_schema_node(def);
                        repaired_defs.insert(dk.clone(), Value::Object(repaired));
                        if m {
                            defs_modified = true;
                        }
                    }
                    Value::Bool(true) => {
                        repaired_defs.insert(dk.clone(), json!({}));
                        defs_modified = true;
                    }
                    other => {
                        repaired_defs.insert(dk.clone(), other.clone());
                    }
                }
            }
            if defs_modified {
                clone.insert(key.into(), Value::Object(repaired_defs));
                modified = true;
            }
        }
    }

    (clone, modified)
}

// port of repairSchemaList (util/gemini_schema.go)
fn repair_schema_list(list: &[Value]) -> (Vec<Value>, bool) {
    let mut out = Vec::with_capacity(list.len());
    let mut modified = false;
    for item in list {
        match item {
            Value::Object(m) => {
                let (repaired, item_mod) = repair_schema_node(m);
                out.push(Value::Object(repaired));
                if item_mod {
                    modified = true;
                }
            }
            Value::Bool(true) => {
                out.push(json!({}));
                modified = true;
            }
            other => out.push(other.clone()),
        }
    }
    (out, modified)
}

// port of repairPropertyMap (util/gemini_schema.go)
fn repair_property_map(props: &Map<String, Value>) -> (Map<String, Value>, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted = Vec::new();
    let mut modified = false;
    for (k, v) in props {
        match v {
            Value::Bool(true) => {
                out.insert(k.clone(), json!({}));
                modified = true;
            }
            Value::Object(child) => {
                let mut child = child.clone();
                if let Some(Value::Bool(req)) = child.get("required").cloned() {
                    child.shift_remove("required");
                    modified = true;
                    if req {
                        promoted.push(k.clone());
                    }
                }
                let (repaired, child_mod) = repair_schema_node(&child);
                if child_mod {
                    modified = true;
                }
                out.insert(k.clone(), Value::Object(repaired));
            }
            other => {
                out.insert(k.clone(), other.clone());
            }
        }
    }
    promoted.sort();
    (out, promoted, modified)
}

// port of refName (util/gemini_schema.go)
fn ref_name(r: &str) -> String {
    match r.rfind('/') {
        Some(i) if i + 1 < r.len() => r[i + 1..].replace("~1", "/").replace("~0", "~"),
        _ => r.to_string(),
    }
}

// port of convertRefsToHints (util/gemini_schema.go) with preserveSiblings=false
fn convert_refs_to_hints(s: &mut Value) {
    let mut paths = find_paths(s, "$ref");
    sort_by_depth(&mut paths);
    for p in paths {
        let def_name = ref_name(&gstr(get_path(s, &p)));
        let parent = trim_suffix(&p, ".$ref");
        let mut hint = format!("See: {def_name}");
        let existing = gstr(get_path(s, &description_path(&parent)));
        if !existing.is_empty() {
            hint = format!("{existing} ({hint})");
        }
        set_raw_at(s, &parent, json!({"type": "object", "description": hint}));
    }
}

// port of convertConstToEnum (util/gemini_schema.go)
fn convert_const_to_enum(s: &mut Value) {
    for p in find_paths(s, "const") {
        let Some(val) = get_path(s, &p).cloned() else {
            continue;
        };
        let enum_path = format!("{}.enum", trim_suffix(&p, ".const"));
        if get_path(s, &enum_path).is_none() {
            set_path(s, &enum_path, Value::Array(vec![go_value(&val)]));
        }
    }
}

// port of convertEnumValuesToStrings (util/gemini_schema.go) with forceStringType=true
fn convert_enum_values_to_strings(s: &mut Value) {
    for p in find_paths(s, "enum") {
        let Some(Value::Array(items)) = get_path(s, &p) else {
            continue;
        };
        let vals: Vec<String> = items.iter().map(|i| gstr(Some(i))).collect();
        set_path(s, &p, strs_value(vals));
        let parent = trim_suffix(&p, ".enum");
        set_path(s, &join_path(&parent, "type"), json!("string"));
    }
}

// port of addEnumHints (util/gemini_schema.go)
fn add_enum_hints(s: &mut Value) {
    for p in find_paths(s, "enum") {
        let Some(Value::Array(items)) = get_path(s, &p) else {
            continue;
        };
        if items.len() <= 1 || items.len() > 10 {
            continue;
        }
        let vals: Vec<String> = items.iter().map(|i| gstr(Some(i))).collect();
        let parent = trim_suffix(&p, ".enum");
        append_hint(s, &parent, &format!("Allowed: {}", vals.join(", ")));
    }
}

// port of mergeConditionals (util/gemini_schema.go)
fn merge_conditionals(s: &mut Value) {
    let by_field = find_paths_by_fields(s, &["then", "else"]);
    let mut paths = Vec::new();
    for key in ["then", "else"] {
        for p in by_field.get(key).into_iter().flatten() {
            if is_property_definition(&trim_suffix(p, &format!(".{key}"))) {
                continue;
            }
            paths.push(p.clone());
        }
    }
    sort_by_depth(&mut paths);
    for p in paths {
        let Some(Value::Object(props)) = get_path(s, &join_path(&p, "properties")).cloned() else {
            continue;
        };
        let parent = if p.ends_with(".then") {
            trim_suffix(&p, ".then")
        } else if p.ends_with(".else") {
            trim_suffix(&p, ".else")
        } else if p == "then" || p == "else" {
            String::new()
        } else {
            continue;
        };
        for (k, v) in props {
            let dest = join_path(&parent, &format!("properties.{}", escape_key(&k)));
            if get_path(s, &dest).is_none() {
                set_path(s, &dest, v);
            }
        }
    }
}

// port of mergeAllOf (util/gemini_schema.go)
fn merge_all_of(s: &mut Value) {
    let mut paths = find_paths(s, "allOf");
    sort_by_depth(&mut paths);
    for p in paths {
        let Some(Value::Array(items)) = get_path(s, &p).cloned() else {
            continue;
        };
        let parent = trim_suffix(&p, ".allOf");
        for item in items {
            let Value::Object(fields) = item else {
                continue;
            };
            for (field, value) in fields {
                match field.as_str() {
                    "required" => {
                        let Value::Array(req) = &value else {
                            continue;
                        };
                        let req_path = join_path(&parent, "required");
                        let mut current = get_strings(s, &req_path);
                        for r in req {
                            let name = gstr(Some(r));
                            if !current.contains(&name) {
                                current.push(name);
                            }
                        }
                        set_path(s, &req_path, strs_value(current));
                    }
                    "if" | "then" | "else" | "allOf" => {}
                    _ => {
                        let dest = join_path(&parent, &escape_key(&field));
                        merge_missing_schema_at_path(s, &dest, &value);
                    }
                }
            }
        }
        delete_path(s, &p);
    }
}

// port of mergeMissingSchemaAtPath (util/gemini_schema.go)
fn merge_missing_schema_at_path(s: &mut Value, dest: &str, incoming: &Value) {
    let Some(existing) = get_path(s, dest) else {
        set_path(s, dest, incoming.clone());
        return;
    };
    let (true, Value::Object(inc)) = (existing.is_object(), incoming) else {
        return;
    };
    for (k, v) in inc {
        let child = join_path(dest, &escape_key(k));
        merge_missing_schema_at_path(s, &child, v);
    }
}

// port of flattenAnyOfOneOf (util/gemini_schema.go)
fn flatten_any_of_one_of(s: &mut Value) {
    for key in ["anyOf", "oneOf"] {
        let mut paths = find_paths(s, key);
        sort_by_depth(&mut paths);
        for p in paths {
            let items = match get_path(s, &p) {
                Some(Value::Array(a)) if !a.is_empty() => a.clone(),
                _ => continue,
            };
            let parent_path = trim_suffix(&p, &format!(".{key}"));
            let parent = if parent_path.is_empty() {
                Some(&*s)
            } else {
                get_path(s, &parent_path)
            };
            let parent_has_props = matches!(
                parent.and_then(|v| v.get("properties")),
                Some(Value::Object(_))
            );

            if parent_has_props {
                let mut has_null = false;
                for item in &items {
                    if gstr(item.get("type")) == "null" {
                        has_null = true;
                    }
                    if let Some(Value::Object(branch)) = item.get("properties") {
                        for (k, v) in branch {
                            let dest =
                                join_path(&parent_path, &format!("properties.{}", escape_key(k)));
                            merge_missing_schema_at_path(s, &dest, v);
                        }
                    }
                }
                if has_null {
                    set_path(s, &join_path(&parent_path, "nullable"), Value::Bool(true));
                }
                delete_path(s, &p);
                continue;
            }

            let parent_desc = gstr(get_path(s, &description_path(&parent_path)));
            let (best, all_types) = select_best(&items);
            let mut selected = items[best].clone();
            let has_null = items.iter().any(|i| gstr(i.get("type")) == "null");
            if has_null && gstr(items[best].get("type")) != "null" {
                if let Value::Object(m) = &mut selected {
                    m.insert("nullable".into(), Value::Bool(true));
                }
            }
            if !parent_desc.is_empty() {
                merge_description_raw(&mut selected, &parent_desc);
            }
            if all_types.len() > 1 {
                append_hint_raw(
                    &mut selected,
                    &format!("Accepts: {}", all_types.join(" | ")),
                );
            }
            set_raw_at(s, &parent_path, selected);
        }
    }
}

// port of selectBest (util/gemini_schema.go)
fn select_best(items: &[Value]) -> (usize, Vec<String>) {
    let mut best_score = -1;
    let mut best_idx = 0;
    let mut types = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut t = gstr(item.get("type"));
        let score;
        if t == "object" || item.get("properties").is_some() {
            score = 3;
            if t.is_empty() {
                t = "object".into();
            }
        } else if t == "array" || item.get("items").is_some() {
            score = 2;
            if t.is_empty() {
                t = "array".into();
            }
        } else if !t.is_empty() && t != "null" {
            score = 1;
        } else if t == "null" {
            score = 0;
        } else {
            score = 0;
            t = String::new();
        }
        if !t.is_empty() {
            types.push(t);
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    (best_idx, types)
}

// port of flattenTypeArrays (util/gemini_schema.go) with preserveNativeNullable=false
fn flatten_type_arrays(s: &mut Value) {
    let mut paths = find_paths(s, "type");
    sort_by_depth(&mut paths);
    let mut nullable_fields: Vec<(String, Vec<String>)> = Vec::new();

    for p in paths {
        let types = match get_path(s, &p) {
            Some(Value::Array(a)) if !a.is_empty() => a.clone(),
            _ => continue,
        };
        let mut has_null = false;
        let mut non_null = Vec::new();
        for item in &types {
            let t = gstr(Some(item));
            if t == "null" {
                has_null = true;
            } else if !t.is_empty() {
                non_null.push(t);
            }
        }
        let parent = trim_suffix(&p, ".type");
        let items_path = join_path(&parent, "items");
        let mut first = "string".to_string();
        if !non_null.is_empty() {
            if get_path(s, &items_path).is_some() && non_null.iter().any(|t| t == "array") {
                first = "array".into();
            } else {
                first = non_null[0].clone();
            }
        }
        set_path(s, &p, Value::String(first.clone()));
        if first != "array" && get_path(s, &items_path).is_some() {
            delete_path(s, &items_path);
        }
        if non_null.len() > 1 {
            append_hint(s, &parent, &format!("Accepts: {}", non_null.join(" | ")));
        }
        if has_null {
            let parts = split_gjson_path(&p);
            if parts.len() >= 3 && parts[parts.len() - 3] == "properties" {
                let field_escaped = parts[parts.len() - 2].clone();
                let field_name = unescape_key(&field_escaped);
                let object_path = parts[..parts.len() - 3].join(".");
                match nullable_fields.iter_mut().find(|(o, _)| *o == object_path) {
                    Some((_, fields)) => fields.push(field_name),
                    None => nullable_fields.push((object_path.clone(), vec![field_name])),
                }
                append_hint(
                    s,
                    &join_path(&object_path, &format!("properties.{field_escaped}")),
                    "(nullable)",
                );
            }
        }
    }

    for (object_path, fields) in nullable_fields {
        let req_path = join_path(&object_path, "required");
        let Some(Value::Array(req)) = get_path(s, &req_path) else {
            continue;
        };
        let filtered: Vec<String> = req
            .iter()
            .map(|r| gstr(Some(r)))
            .filter(|r| !fields.contains(r))
            .collect();
        if filtered.is_empty() {
            delete_path(s, &req_path);
        } else {
            set_path(s, &req_path, strs_value(filtered));
        }
    }
}

// port of removeUnsupportedKeywords (util/gemini_schema.go); constraintKeywords
// is empty under preserveStandardConstraints and additionalProperties is kept
// under preserveAllAdditionalProperties.
fn remove_unsupported_keywords(s: &mut Value) {
    const KEYWORDS: [&str; 26] = [
        "$schema",
        "$defs",
        "definitions",
        "const",
        "$ref",
        "$id",
        "id",
        "additionalProperties",
        "$anchor",
        "$vocabulary",
        "$dynamicRef",
        "$dynamicAnchor",
        "propertyNames",
        "patternProperties",
        "if",
        "then",
        "else",
        "$comment",
        "enumDescriptions",
        "enumTitles",
        "prefill",
        "deprecated",
        "encrypted",
        "additionalItems",
        "unevaluatedProperties",
        "unevaluatedItems",
    ];
    const EXTRA: [&str; 1] = ["contentSchema"];
    let keywords: Vec<&str> = KEYWORDS.iter().chain(EXTRA.iter()).copied().collect();
    let by_field = find_paths_by_fields(s, &keywords);
    let mut delete = Vec::new();
    for key in &keywords {
        for p in by_field.get(*key).into_iter().flatten() {
            if is_property_definition(&trim_suffix(p, &format!(".{key}"))) {
                continue;
            }
            if *key == "additionalProperties" {
                continue;
            }
            delete.push(p.clone());
        }
    }
    sort_by_depth(&mut delete);
    for p in delete {
        delete_path(s, &p);
    }
    remove_extension_fields(s);
}

// port of removeExtensionFields + walkForExtensions (util/gemini_schema.go)
fn remove_extension_fields(s: &mut Value) {
    fn walk(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Array(a) => {
                for i in (0..a.len()).rev() {
                    walk(&a[i], &join_path(path, &i.to_string()), out);
                }
            }
            Value::Object(m) => {
                for (k, child) in m {
                    let child_path = join_path(path, &escape_key(k));
                    if k.starts_with("x-") && !is_property_definition(path) {
                        out.push(child_path);
                        continue;
                    }
                    walk(child, &child_path, out);
                }
            }
            _ => {}
        }
    }
    let mut paths = Vec::new();
    walk(s, "", &mut paths);
    for p in paths {
        delete_path(s, &p);
    }
}

// port of removeKeywords (util/gemini_schema.go)
fn remove_keywords(s: &mut Value, keywords: &[&str]) {
    let by_field = find_paths_by_fields(s, keywords);
    let mut delete = Vec::new();
    for key in keywords {
        for p in by_field.get(*key).into_iter().flatten() {
            if is_property_definition(&trim_suffix(p, &format!(".{key}"))) {
                continue;
            }
            delete.push(p.clone());
        }
    }
    sort_by_depth(&mut delete);
    for p in delete {
        delete_path(s, &p);
    }
}

/// The `required` filter shared by removePlaceholderFields' two passes.
fn drop_required_entry(s: &mut Value, req_path: &str, name: &str) {
    let Some(Value::Array(req)) = get_path(s, req_path) else {
        return;
    };
    let filtered: Vec<String> = req
        .iter()
        .map(|r| gstr(Some(r)))
        .filter(|r| r != name)
        .collect();
    if filtered.is_empty() {
        delete_path(s, req_path);
    } else {
        set_path(s, req_path, strs_value(filtered));
    }
}

// port of removePlaceholderFields (util/gemini_schema.go)
fn remove_placeholder_fields(s: &mut Value) {
    let mut paths = find_paths(s, "_");
    sort_by_depth(&mut paths);
    for p in paths {
        if !p.ends_with(".properties._") {
            continue;
        }
        delete_path(s, &p);
        let parent = trim_suffix(&p, ".properties._");
        drop_required_entry(s, &join_path(&parent, "required"), "_");
    }

    let mut paths = find_paths(s, "reason");
    sort_by_depth(&mut paths);
    for p in paths {
        if !p.ends_with(".properties.reason") {
            continue;
        }
        let parent = trim_suffix(&p, ".properties.reason");
        match get_path(s, &join_path(&parent, "properties")) {
            Some(Value::Object(props)) if props.len() == 1 => {}
            _ => continue,
        }
        if gstr(get_path(s, &format!("{p}.description"))) != PLACEHOLDER_REASON_DESCRIPTION {
            continue;
        }
        delete_path(s, &p);
        drop_required_entry(s, &join_path(&parent, "required"), "reason");
    }
}

// port of cleanupRequiredFields (util/gemini_schema.go)
fn cleanup_required_fields(s: &mut Value) {
    for p in find_paths(s, "required") {
        let parent = trim_suffix(&p, ".required");
        let props_path = join_path(&parent, "properties");
        let Some(Value::Array(req)) = get_path(s, &p).cloned() else {
            continue;
        };
        let Some(Value::Object(props)) = get_path(s, &props_path) else {
            delete_path(s, &p);
            continue;
        };
        let valid: Vec<String> = req
            .iter()
            .map(|r| gstr(Some(r)))
            .filter(|k| !k.is_empty() && props.contains_key(k))
            .collect();
        if valid.len() != req.len() {
            if valid.is_empty() {
                delete_path(s, &p);
            } else {
                set_path(s, &p, strs_value(valid));
            }
        }
    }
}

// port of sanitizeArrayItems (util/gemini_schema.go)
fn sanitize_array_items(s: &mut Value) {
    let mut paths = find_paths(s, "items");
    sort_by_depth(&mut paths);
    for p in paths {
        let parent = trim_suffix(&p, ".items");
        if is_property_definition(&parent) {
            continue;
        }
        let type_path = join_path(&parent, "type");
        let t = gstr(get_path(s, &type_path));
        if t.is_empty() {
            set_path(s, &type_path, json!("array"));
        } else if !t.eq_ignore_ascii_case("array") {
            delete_path(s, &p);
        }
    }
}

// ───────────────────────────── request ─────────────────────────────

const GEMINI_FUNCTION_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

// port of geminiTextPart (gemini_openai_request.go)
fn gemini_text_part(text: &str) -> Value {
    json!({"text": text})
}

// port of geminiInlineDataPart (gemini_openai_request.go)
fn gemini_inline_data_part(mime_type: &str, data: &str, thought_signature: &str) -> Value {
    let mut part = json!({"inlineData": {"mime_type": mime_type, "data": data}});
    if !thought_signature.is_empty() {
        part["thoughtSignature"] = Value::String(thought_signature.to_string());
    }
    part
}

// port of geminiContentNode (gemini_openai_request.go)
fn gemini_content_node(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

// port of openAIToolCallGeminiThoughtSignature (gemini_openai_request.go)
fn openai_tool_call_gemini_thought_signature(tool_call: &Value) -> String {
    for path in [
        "extra_content.google.thought_signature",
        "function.extra_content.google.thought_signature",
        "thoughtSignature",
        "thought_signature",
    ] {
        if let Some(sig) = get_path(tool_call, path) {
            return gemini_replay_signature_or_bypass(&gstr(Some(sig)));
        }
    }
    GEMINI_FUNCTION_THOUGHT_SIGNATURE.to_string()
}

// port of openAIInputAudioMimeType (gemini_openai_request.go)
fn openai_input_audio_mime_type(format: &str) -> String {
    match format {
        "" | "wav" => "audio/wav".into(),
        "mp3" => "audio/mpeg".into(),
        "ogg" => "audio/ogg".into(),
        "flac" => "audio/flac".into(),
        "aac" => "audio/aac".into(),
        "webm" => "audio/webm".into(),
        "pcm16" => "audio/pcm".into(),
        "g711_ulaw" | "g711_alaw" => "audio/basic".into(),
        other => format!("audio/{other}"),
    }
}

// port of applyOpenAIResponseFormatToGemini (gemini_openai_request.go)
fn apply_openai_response_format_to_gemini(out: &mut Value, raw: &Value) {
    let Some(rf) = raw.get("response_format") else {
        return;
    };
    match gstr(rf.get("type")).trim().to_lowercase().as_str() {
        "json_object" => {
            set_path(
                out,
                "generationConfig.responseMimeType",
                json!("application/json"),
            );
        }
        "json_schema" => {
            set_path(
                out,
                "generationConfig.responseMimeType",
                json!("application/json"),
            );
            delete_path(out, "generationConfig.responseSchema");
            if let Some(schema) = get_path(rf, "json_schema.schema") {
                set_path(out, "generationConfig.responseJsonSchema", schema.clone());
            }
        }
        _ => {}
    }
}

// port of geminiDemotedSystemText (gemini_openai_request.go)
fn gemini_demoted_system_text(text: &str, is_demoted: bool) -> String {
    if !is_demoted || text.trim().is_empty() {
        return text.to_string();
    }
    system_reminder_text(text)
}

/// The data-URL split both image_url and video_url use: `url[5:]` split once
/// on ";", the payload being `pieces[1][7:]` (past "base64,"). Byte-wise, as
/// in Go.
fn data_url_inline_part(url: &str) -> Option<Value> {
    let bytes = url.as_bytes();
    if bytes.len() <= 5 {
        return None;
    }
    let rest = &bytes[5..];
    let semi = rest.iter().position(|b| *b == b';')?;
    let (mime, after) = (&rest[..semi], &rest[semi + 1..]);
    if after.len() <= 7 {
        return None;
    }
    Some(gemini_inline_data_part(
        &String::from_utf8_lossy(mime),
        &String::from_utf8_lossy(&after[7..]),
        "",
    ))
}

/// sjson `SetRawBytes(part, "functionCall.args", arguments)`: the arguments
/// STRING is spliced in as raw JSON. A string that is not JSON would make Go
/// emit an invalid document; here an empty one becomes `{}` and any other
/// non-JSON text is kept as a JSON string.
fn raw_json_arguments(arguments: &str) -> Value {
    match serde_json::from_str::<Value>(arguments) {
        Ok(v) => v,
        Err(_) if arguments.trim().is_empty() => json!({}),
        Err(_) => Value::String(arguments.to_string()),
    }
}

/// Client request (OpenAI Chat Completions) → upstream request (Gemini).
// port of ConvertOpenAIRequestToGemini (gemini_openai_request.go)
pub fn translate_request(model: &str, body: &Value, _stream: bool) -> Value {
    let raw = body;
    let mut out = json!({"contents": []});
    set_path(&mut out, "model", json!(model));

    if let Some(gc) = raw.get("generationConfig") {
        set_path(&mut out, "generationConfig", gc.clone());
    }

    if let Some(re) = raw.get("reasoning_effort") {
        let effort = gstr(Some(re)).trim().to_lowercase();
        if !effort.is_empty() {
            if effort == "auto" {
                set_path(
                    &mut out,
                    "generationConfig.thinkingConfig.thinkingBudget",
                    json!(-1),
                );
            } else {
                set_path(
                    &mut out,
                    "generationConfig.thinkingConfig.thinkingLevel",
                    json!(effort),
                );
            }
        }
    }

    if let Some(v @ Value::Number(_)) = raw.get("temperature") {
        set_path(
            &mut out,
            "generationConfig.temperature",
            float_value(gnum(Some(v))),
        );
    }
    if let Some(v @ Value::Number(_)) = raw.get("top_p") {
        set_path(
            &mut out,
            "generationConfig.topP",
            float_value(gnum(Some(v))),
        );
    }
    if let Some(v @ Value::Number(_)) = raw.get("top_k") {
        set_path(
            &mut out,
            "generationConfig.topK",
            float_value(gnum(Some(v))),
        );
    }

    if let Some(v @ Value::Number(_)) = raw.get("max_tokens") {
        set_path(
            &mut out,
            "generationConfig.maxOutputTokens",
            float_value(gnum(Some(v))),
        );
    } else if let Some(v @ Value::Number(_)) = raw.get("max_completion_tokens") {
        set_path(
            &mut out,
            "generationConfig.maxOutputTokens",
            float_value(gnum(Some(v))),
        );
    }

    if let Some(n @ Value::Number(_)) = raw.get("n") {
        let val = gint(Some(n));
        if val > 1 {
            set_path(&mut out, "generationConfig.candidateCount", json!(val));
        }
    }

    apply_openai_response_format_to_gemini(&mut out, raw);

    if let Some(Value::Array(mods)) = raw.get("modalities") {
        let mut response_mods = Vec::new();
        for m in mods {
            match gstr(Some(m)).to_lowercase().as_str() {
                "text" => response_mods.push("TEXT"),
                "image" => response_mods.push("IMAGE"),
                _ => {}
            }
        }
        if !response_mods.is_empty() {
            set_path(
                &mut out,
                "generationConfig.responseModalities",
                json!(response_mods),
            );
        }
    }

    if let Some(img_cfg @ Value::Object(_)) = raw.get("image_config") {
        if let Some(Value::String(ar)) = img_cfg.get("aspect_ratio") {
            set_path(
                &mut out,
                "generationConfig.imageConfig.aspectRatio",
                json!(ar),
            );
        }
        if let Some(Value::String(size)) = img_cfg.get("image_size") {
            set_path(
                &mut out,
                "generationConfig.imageConfig.imageSize",
                json!(size),
            );
        }
    }

    // messages -> systemInstruction + contents
    if let Some(Value::Array(arr)) = raw.get("messages") {
        let mut system_parts: Vec<Value> = Vec::new();
        let mut content_items: Vec<Value> = Vec::new();
        let mut has_encountered_conversation = false;

        for (i, m) in arr.iter().enumerate() {
            let role = gstr(m.get("role"));
            let content = m.get("content");
            let content_text_obj =
                matches!(content, Some(c @ Value::Object(_)) if gstr(c.get("type")) == "text");

            if (role == "system" || role == "developer")
                && arr.len() > 1
                && !has_encountered_conversation
            {
                match content {
                    Some(Value::String(s)) => system_parts.push(gemini_text_part(s)),
                    Some(c) if content_text_obj => {
                        system_parts.push(gemini_text_part(&gstr(c.get("text"))))
                    }
                    Some(Value::Array(items)) => {
                        for item in items {
                            system_parts.push(gemini_text_part(&gstr(item.get("text"))));
                        }
                    }
                    _ => {}
                }
            } else if role == "user" || role == "system" || role == "developer" {
                has_encountered_conversation = true;
                let demoted = role == "system" || role == "developer";
                let mut parts: Vec<Value> = Vec::new();
                match content {
                    Some(Value::String(s)) => {
                        parts.push(gemini_text_part(&gemini_demoted_system_text(s, demoted)))
                    }
                    Some(c) if content_text_obj => parts.push(gemini_text_part(
                        &gemini_demoted_system_text(&gstr(c.get("text")), demoted),
                    )),
                    Some(Value::Array(items)) => {
                        for item in items {
                            match gstr(item.get("type")).as_str() {
                                "text" => {
                                    let text = gstr(item.get("text"));
                                    if !text.is_empty() {
                                        parts.push(gemini_text_part(&gemini_demoted_system_text(
                                            &text, demoted,
                                        )));
                                    }
                                }
                                "image_url" => {
                                    if let Some(p) =
                                        data_url_inline_part(&gstr(get_path(item, "image_url.url")))
                                    {
                                        parts.push(p);
                                    }
                                }
                                "video_url" => {
                                    if let Some(p) =
                                        data_url_inline_part(&gstr(get_path(item, "video_url.url")))
                                    {
                                        parts.push(p);
                                    }
                                }
                                "file" => {
                                    let filename = gstr(get_path(item, "file.filename"));
                                    let file_data = gstr(get_path(item, "file.file_data"));
                                    if let Some((mime, data)) =
                                        normalize_openai_file_data(&filename, "", &file_data)
                                    {
                                        parts.push(gemini_inline_data_part(&mime, &data, ""));
                                    }
                                    // else: Go logs "Invalid file data or unknown file name extension" and skips.
                                }
                                "input_audio" => {
                                    let data = gstr(get_path(item, "input_audio.data"));
                                    if !data.is_empty() {
                                        let mime = openai_input_audio_mime_type(&gstr(get_path(
                                            item,
                                            "input_audio.format",
                                        )));
                                        parts.push(gemini_inline_data_part(&mime, &data, ""));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !parts.is_empty() {
                    content_items.push(gemini_content_node("user", parts));
                }
            } else if role == "assistant" {
                has_encountered_conversation = true;
                let mut parts: Vec<Value> = Vec::new();
                if let Some(Value::String(rc)) = m.get("reasoning_content") {
                    if !rc.is_empty() {
                        parts.push(json!({"text": rc, "thought": true}));
                    }
                }
                match content {
                    Some(Value::String(s)) if !s.is_empty() => parts.push(gemini_text_part(s)),
                    Some(Value::Array(items)) => {
                        for item in items {
                            match gstr(item.get("type")).as_str() {
                                "text" => {
                                    let text = gstr(item.get("text"));
                                    if !text.is_empty() {
                                        parts.push(gemini_text_part(&text));
                                    }
                                }
                                "image_url" => {
                                    if let Some(p) =
                                        data_url_inline_part(&gstr(get_path(item, "image_url.url")))
                                    {
                                        parts.push(p);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }

                if let Some(Value::Array(tcs)) = m.get("tool_calls") {
                    // (id, sanitized name) of every function call in this turn.
                    let mut tool_calls: Vec<(String, String)> = Vec::new();
                    for tc in tcs {
                        if gstr(tc.get("type")) != "function" {
                            continue;
                        }
                        let function_id = gstr(tc.get("id"));
                        let function_name =
                            sanitize_function_name(&gstr(get_path(tc, "function.name")));
                        if function_name.is_empty() {
                            continue;
                        }
                        let args = raw_json_arguments(&gstr(get_path(tc, "function.arguments")));
                        parts.push(json!({
                            "functionCall": {"name": function_name, "args": args},
                            "thoughtSignature": openai_tool_call_gemini_thought_signature(tc),
                        }));
                        tool_calls.push((function_id, function_name));
                    }
                    if !parts.is_empty() {
                        content_items.push(gemini_content_node("model", parts));
                    }

                    // Tool responses scoped to this assistant turn, keyed by
                    // call id → the RAW JSON text of the tool message content.
                    let mut turn_responses: HashMap<String, String> = HashMap::new();
                    for next in &arr[i + 1..] {
                        let next_role = gstr(next.get("role"));
                        if next_role == "assistant" {
                            break;
                        }
                        if next_role == "tool" {
                            let call_id = gstr(next.get("tool_call_id"));
                            if !call_id.is_empty() {
                                let raw_content = next
                                    .get("content")
                                    .map(Value::to_string)
                                    .unwrap_or_default();
                                turn_responses.insert(call_id, raw_content);
                            }
                        }
                    }

                    let mut response_parts = Vec::new();
                    for (id, name) in &tool_calls {
                        let mut response = turn_responses.get(id).cloned().unwrap_or_default();
                        if response.is_empty() {
                            response = "{}".into();
                        }
                        response_parts.push(json!({
                            "functionResponse": {"name": name, "response": {"result": response}}
                        }));
                    }
                    if !response_parts.is_empty() {
                        content_items.push(gemini_content_node("user", response_parts));
                    }
                } else if !parts.is_empty() {
                    content_items.push(gemini_content_node("model", parts));
                }
            }
        }

        if !system_parts.is_empty() {
            set_path(
                &mut out,
                "systemInstruction",
                gemini_content_node("user", system_parts),
            );
        }
        if content_items
            .last()
            .is_some_and(|c| gstr(c.get("role")) == "model")
        {
            content_items.pop();
        }
        // port of SetRawArrayItems (translator/common/bytes.go)
        if !content_items.is_empty() {
            set_path(&mut out, "contents", Value::Array(content_items));
        }
    }

    // tools -> tools[].functionDeclarations + googleSearch/codeExecution/urlContext passthrough
    let mut allowed_tool_names: HashSet<String> = HashSet::new();
    let mut is_allowed_tools = false;
    let mut allowed_mode = "auto".to_string();
    if let Some(tc @ Value::Object(_)) = raw.get("tool_choice") {
        if gstr(tc.get("type")) == "allowed_tools" {
            is_allowed_tools = true;
            let mut list = match get_path(tc, "allowed_tools.tools") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            if list.is_empty() {
                list = match tc.get("tools") {
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
            }
            for t in &list {
                let mut name = gstr(get_path(t, "function.name")).trim().to_string();
                if name.is_empty() {
                    name = gstr(t.get("name")).trim().to_string();
                }
                if !name.is_empty() {
                    allowed_tool_names.insert(name);
                }
            }
            let mut mode = gstr(get_path(tc, "allowed_tools.mode"))
                .trim()
                .to_lowercase();
            if mode.is_empty() {
                mode = gstr(tc.get("mode")).trim().to_lowercase();
            }
            if !mode.is_empty() {
                allowed_mode = mode;
            }
        }
    }

    let mut declared_original_to_sanitized: HashMap<String, String> = HashMap::new();
    let mut sanitized_counts: HashMap<String, usize> = HashMap::new();
    let mut function_declarations: Vec<Value> = Vec::new();
    let mut has_strict_tool = false;
    if let Some(Value::Array(tools)) = raw.get("tools") {
        if !tools.is_empty() {
            let mut google_search_nodes = Vec::new();
            let mut code_execution_nodes = Vec::new();
            let mut url_context_nodes = Vec::new();
            for t in tools {
                if gstr(t.get("type")) == "function" {
                    if let Some(Value::Object(fn_map)) = t.get("function") {
                        let name_result = fn_map.get("name");
                        let original_name = gstr(name_result);
                        if !is_allowed_tools || allowed_tool_names.contains(&original_name) {
                            let sanitized = sanitize_function_name(&original_name);
                            *sanitized_counts.entry(sanitized.clone()).or_insert(0) += 1;
                            declared_original_to_sanitized
                                .insert(original_name.clone(), sanitized.clone());

                            let mut f = Value::Object(fn_map.clone());
                            if fn_map.contains_key("parameters") {
                                // port of RenameKey (util/translator.go)
                                if let Value::Object(m) = &mut f {
                                    let params =
                                        m.get("parameters").cloned().unwrap_or(Value::Null);
                                    m.insert("parametersJsonSchema".into(), params);
                                    m.shift_remove("parameters");
                                }
                            } else {
                                set_path(&mut f, "parametersJsonSchema.type", json!("object"));
                                set_path(&mut f, "parametersJsonSchema.properties", json!({}));
                            }
                            if !matches!(name_result, Some(Value::String(_)))
                                || sanitized != original_name
                            {
                                set_path(&mut f, "name", json!(sanitized));
                            }
                            if let Some(params) = f.get("parametersJsonSchema") {
                                let cleaned = clean_json_schema_for_gemini_json_schema(params);
                                set_path(&mut f, "parametersJsonSchema", cleaned);
                            }
                            let strict_val = f
                                .get("strict")
                                .cloned()
                                .or_else(|| fn_map.get("strict").cloned())
                                .or_else(|| t.get("strict").cloned());
                            if let Some(sv) = strict_val {
                                if sv == Value::Bool(true) {
                                    has_strict_tool = true;
                                }
                                if let Value::Object(m) = &mut f {
                                    m.shift_remove("strict");
                                }
                            }
                            function_declarations.push(f);
                        }
                    }
                }
                if let Some(gs) = t.get("google_search") {
                    google_search_nodes.push(json!({"googleSearch": gs}));
                }
                if let Some(ce) = t.get("code_execution") {
                    code_execution_nodes.push(json!({"codeExecution": ce}));
                }
                if let Some(uc) = t.get("url_context") {
                    url_context_nodes.push(json!({"urlContext": uc}));
                }
            }
            if !function_declarations.is_empty()
                || !google_search_nodes.is_empty()
                || !code_execution_nodes.is_empty()
                || !url_context_nodes.is_empty()
            {
                let mut tool_items = Vec::new();
                if !function_declarations.is_empty() {
                    tool_items.push(json!({"functionDeclarations": function_declarations.clone()}));
                }
                tool_items.extend(google_search_nodes);
                tool_items.extend(code_execution_nodes);
                tool_items.extend(url_context_nodes);
                set_path(&mut out, "tools", Value::Array(tool_items));
            }
        }
    }

    let has_sanitized_collision = sanitized_counts.values().any(|c| *c > 1);
    const MODE: &str = "toolConfig.functionCallingConfig.mode";
    const ALLOWED: &str = "toolConfig.functionCallingConfig.allowedFunctionNames";

    if has_sanitized_collision {
        set_path(&mut out, MODE, json!("NONE"));
    } else if is_allowed_tools {
        if function_declarations.is_empty() {
            set_path(&mut out, MODE, json!("NONE"));
        } else if allowed_mode == "required" || allowed_mode == "any" {
            set_path(&mut out, MODE, json!("ANY"));
            let names: Vec<String> = function_declarations
                .iter()
                .map(|f| gstr(f.get("name")))
                .collect();
            set_path(&mut out, ALLOWED, json!(names));
        } else if has_strict_tool {
            set_path(&mut out, MODE, json!("VALIDATED"));
        } else {
            set_path(&mut out, MODE, json!("AUTO"));
        }
    } else if let Some(tc) = raw.get("tool_choice").filter(|v| !v.is_null()) {
        let tc_type = match tc {
            Value::String(s) => s.trim().to_lowercase(),
            Value::Object(_) => gstr(tc.get("type")).trim().to_lowercase(),
            _ => String::new(),
        };
        match tc_type.as_str() {
            "auto" => {
                let mode = if has_strict_tool { "VALIDATED" } else { "AUTO" };
                set_path(&mut out, MODE, json!(mode));
            }
            "none" => set_path(&mut out, MODE, json!("NONE")),
            "required" | "any" => set_path(&mut out, MODE, json!("ANY")),
            "function" | "tool" => {
                let mut name = gstr(get_path(tc, "function.name")).trim().to_string();
                if name.is_empty() {
                    name = gstr(tc.get("name")).trim().to_string();
                }
                match declared_original_to_sanitized.get(&name) {
                    Some(sanitized) if sanitized_counts.get(sanitized) == Some(&1) => {
                        set_path(&mut out, MODE, json!("ANY"));
                        set_path(&mut out, ALLOWED, json!([sanitized]));
                    }
                    _ => set_path(&mut out, MODE, json!("NONE")),
                }
            }
            _ => set_path(&mut out, MODE, json!("NONE")),
        }
    } else if has_strict_tool && !function_declarations.is_empty() {
        set_path(&mut out, MODE, json!("VALIDATED"));
    }

    // parallel_tool_calls: false cannot be expressed while keeping tools on — fail closed.
    if raw.get("parallel_tool_calls") == Some(&Value::Bool(false)) {
        set_path(&mut out, MODE, json!("NONE"));
        delete_path(&mut out, ALLOWED);
    }

    attach_default_safety_settings(&mut out, "safetySettings");
    out
}

// ───────────────────────────── response ─────────────────────────────

/// Usage block shared by the stream and non-stream converters.
fn apply_usage(template: &mut Value, usage: &Value) {
    let thoughts = gint(usage.get("thoughtsTokenCount"));
    set_path(
        template,
        "usage.completion_tokens",
        json!(gint(usage.get("candidatesTokenCount")) + thoughts),
    );
    if let Some(total) = usage.get("totalTokenCount") {
        set_path(template, "usage.total_tokens", json!(gint(Some(total))));
    }
    set_path(
        template,
        "usage.prompt_tokens",
        json!(gint(usage.get("promptTokenCount"))),
    );
    if thoughts > 0 {
        set_path(
            template,
            "usage.completion_tokens_details.reasoning_tokens",
            json!(thoughts),
        );
    }
    let cached = gint(usage.get("cachedContentTokenCount"));
    if cached > 0 {
        set_path(
            template,
            "usage.prompt_tokens_details.cached_tokens",
            json!(cached),
        );
    }
}

/// `inlineData`, else `inline_data`.
fn inline_data_of(part: &Value) -> Option<&Value> {
    part.get("inlineData").or_else(|| part.get("inline_data"))
}

/// `data:<mime>;base64,<data>`, mime from `mimeType` / `mime_type`, default image/png.
fn inline_data_url(inline: &Value) -> Option<String> {
    let data = gstr(inline.get("data"));
    if data.is_empty() {
        return None;
    }
    let mut mime = gstr(inline.get("mimeType"));
    if mime.is_empty() {
        mime = gstr(inline.get("mime_type"));
    }
    if mime.is_empty() {
        mime = "image/png".into();
    }
    Some(format!("data:{mime};base64,{data}"))
}

/// Speech-to-text models deliver the transcript in `audioTranscription`
/// instead of `text`; an explicit `text` wins.
fn part_text(part: &Value) -> Option<&Value> {
    match part.get("text") {
        Some(t) => Some(t),
        None => part.get("audioTranscription").and_then(|a| a.get("text")),
    }
}

/// Per-stream state.
// port of convertGeminiResponseToOpenAIChatParams (gemini_openai_response.go)
pub struct StreamTranslator {
    unix_timestamp: i64,
    function_index: HashMap<i64, i64>,
    saw_tool_call: HashMap<i64, bool>,
    upstream_finish_reason: HashMap<i64, String>,
    sanitized_name_map: Option<HashMap<String, String>>,
}

impl StreamTranslator {
    /// `original_request` = the client's ORIGINAL body.
    pub fn new(original_request: &Value) -> Self {
        Self {
            unix_timestamp: 0,
            function_index: HashMap::new(),
            saw_tool_call: HashMap::new(),
            upstream_finish_reason: HashMap::new(),
            sanitized_name_map: sanitized_tool_name_map(original_request),
        }
    }

    /// One upstream SSE event (a Gemini `GenerateContentResponse` chunk).
    pub fn push(&mut self, _event: Option<&str>, data: &Value) -> Vec<String> {
        self.convert(data)
            .into_iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect()
    }

    /// Upstream stream ended: the Chat stream terminator.
    pub fn finish(&mut self) -> Vec<String> {
        vec!["data: [DONE]\n\n".to_string()]
    }

    // port of ConvertGeminiResponseToOpenAI (gemini_openai_response.go)
    fn convert(&mut self, raw: &Value) -> Vec<Value> {
        let mut base = json!({
            "id": "",
            "object": "chat.completion.chunk",
            "created": 12345,
            "model": "model",
            "choices": [{
                "index": 0,
                "delta": {"role": null, "content": null, "reasoning_content": null, "tool_calls": null},
                "finish_reason": null,
                "native_finish_reason": null
            }]
        });

        if let Some(mv) = raw.get("modelVersion") {
            base["model"] = Value::String(gstr(Some(mv)));
        }
        if let Some(ct) = raw.get("createTime") {
            if let Some(ts) = parse_rfc3339_unix(&gstr(Some(ct))) {
                self.unix_timestamp = ts;
            }
        }
        base["created"] = json!(self.unix_timestamp);
        if let Some(id) = raw.get("responseId") {
            base["id"] = Value::String(gstr(Some(id)));
        }
        let usage = raw.get("usageMetadata");
        if let Some(u) = usage {
            apply_usage(&mut base, u);
        }

        let mut out = Vec::new();
        let Some(Value::Array(candidates)) = raw.get("candidates") else {
            // A pure usageMetadata chunk.
            if usage.is_some() {
                out.push(base);
            }
            return out;
        };

        for candidate in candidates {
            let mut template = base.clone();
            let cand_idx = gint(candidate.get("index"));
            template["choices"][0]["index"] = json!(cand_idx);

            if let Some(fr) = candidate.get("finishReason") {
                self.upstream_finish_reason
                    .insert(cand_idx, gstr(Some(fr)).to_uppercase());
            }

            let mut role_set = false;
            let mut set_role = |t: &mut Value| {
                if !role_set {
                    t["choices"][0]["delta"]["role"] = json!("assistant");
                    role_set = true;
                }
            };

            if let Some(Value::Array(parts)) = get_path(candidate, "content.parts") {
                for part in parts {
                    let text = part_text(part);
                    let function_call = part.get("functionCall");
                    let inline = inline_data_of(part);
                    let sig = part
                        .get("thoughtSignature")
                        .or_else(|| part.get("thought_signature"));
                    let has_sig = sig.is_some() && !gstr(sig).is_empty();
                    let has_payload = text.is_some() || function_call.is_some() || inline.is_some();
                    // Skip pure thoughtSignature parts.
                    if has_sig && !has_payload {
                        continue;
                    }

                    if let Some(t) = text {
                        set_role(&mut template);
                        let key = if gbool(part.get("thought")) {
                            "reasoning_content"
                        } else {
                            "content"
                        };
                        template["choices"][0]["delta"][key] = Value::String(gstr(Some(t)));
                    } else if let Some(fc) = function_call {
                        self.saw_tool_call.insert(cand_idx, true);
                        let counter = self.function_index.entry(cand_idx).or_insert(0);
                        let mut fc_index = *counter;
                        *counter += 1;
                        match &template["choices"][0]["delta"]["tool_calls"] {
                            Value::Array(a) => fc_index = a.len() as i64,
                            _ => template["choices"][0]["delta"]["tool_calls"] = json!([]),
                        }
                        let name = restore_sanitized_tool_name(
                            self.sanitized_name_map.as_ref(),
                            &gstr(fc.get("name")),
                        );
                        let arguments = fc.get("args").map(Value::to_string).unwrap_or_default();
                        let call = json!({
                            "id": function_call_id(&name),
                            "index": fc_index,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        });
                        set_role(&mut template);
                        if let Value::Array(a) = &mut template["choices"][0]["delta"]["tool_calls"]
                        {
                            a.push(call);
                        }
                    } else if let Some(inl) = inline {
                        let Some(url) = inline_data_url(inl) else {
                            continue;
                        };
                        let delta = &mut template["choices"][0]["delta"];
                        if !delta.get("images").is_some_and(Value::is_array) {
                            delta["images"] = json!([]);
                        }
                        let image_index = delta["images"].as_array().map_or(0, Vec::len);
                        let payload = json!({
                            "type": "image_url",
                            "image_url": {"url": url},
                            "index": image_index
                        });
                        set_role(&mut template);
                        if let Value::Array(a) = &mut template["choices"][0]["delta"]["images"] {
                            a.push(payload);
                        }
                    }
                }
            }

            let upstream_reason = self
                .upstream_finish_reason
                .get(&cand_idx)
                .cloned()
                .unwrap_or_default();
            let saw_tool_call = self.saw_tool_call.get(&cand_idx).copied().unwrap_or(false);
            if !upstream_reason.is_empty() && usage.is_some() {
                let finish = if saw_tool_call {
                    "tool_calls"
                } else if upstream_reason == "MAX_TOKENS" {
                    "max_tokens"
                } else {
                    "stop"
                };
                template["choices"][0]["finish_reason"] = json!(finish);
                template["choices"][0]["native_finish_reason"] =
                    json!(upstream_reason.to_lowercase());
            }
            out.push(template);
        }
        out
    }
}

/// Complete upstream response (Gemini) → client response (Chat Completion).
// port of ConvertGeminiResponseToOpenAINonStream (gemini_openai_response.go)
pub fn translate_non_stream(upstream: &Value, original_request: &Value) -> Value {
    let raw = upstream;
    let name_map = sanitized_tool_name_map(original_request);
    let mut template = json!({
        "id": "",
        "object": "chat.completion",
        "created": 123456,
        "model": "model",
        "choices": []
    });

    if let Some(mv) = raw.get("modelVersion") {
        template["model"] = Value::String(gstr(Some(mv)));
    }
    let mut unix_timestamp = 0i64;
    if let Some(ct) = raw.get("createTime") {
        if let Some(ts) = parse_rfc3339_unix(&gstr(Some(ct))) {
            unix_timestamp = ts;
        }
    }
    template["created"] = json!(unix_timestamp);
    if let Some(id) = raw.get("responseId") {
        template["id"] = Value::String(gstr(Some(id)));
    }
    if let Some(u) = raw.get("usageMetadata") {
        apply_usage(&mut template, u);
    }

    if let Some(Value::Array(candidates)) = raw.get("candidates") {
        let mut choices = Vec::new();
        for candidate in candidates {
            let mut choice = json!({
                "index": 0,
                "message": {"role": "assistant", "content": null, "reasoning_content": null, "tool_calls": null},
                "finish_reason": null,
                "native_finish_reason": null
            });
            choice["index"] = json!(gint(candidate.get("index")));
            if let Some(fr) = candidate.get("finishReason") {
                let reason = gstr(Some(fr)).to_lowercase();
                choice["finish_reason"] = json!(reason);
                choice["native_finish_reason"] = json!(reason);
            }

            let mut has_function_call = false;
            if let Some(Value::Array(parts)) = get_path(candidate, "content.parts") {
                let mut tool_calls = Vec::new();
                let mut images = Vec::new();
                let mut text_content = String::new();
                let mut reasoning_content = String::new();
                let mut has_text = false;
                let mut has_reasoning = false;

                for part in parts {
                    if let Some(t) = part_text(part) {
                        if gbool(part.get("thought")) {
                            has_reasoning = true;
                            reasoning_content.push_str(&gstr(Some(t)));
                        } else {
                            has_text = true;
                            text_content.push_str(&gstr(Some(t)));
                        }
                    } else if let Some(fc) = part.get("functionCall") {
                        has_function_call = true;
                        let name =
                            restore_sanitized_tool_name(name_map.as_ref(), &gstr(fc.get("name")));
                        let arguments = fc.get("args").map(Value::to_string).unwrap_or_default();
                        tool_calls.push(json!({
                            "id": function_call_id(&name),
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        }));
                    } else if let Some(inl) = inline_data_of(part) {
                        if let Some(url) = inline_data_url(inl) {
                            images.push(json!({
                                "type": "image_url",
                                "image_url": {"url": url},
                                "index": images.len()
                            }));
                        }
                    }
                }

                if has_text {
                    choice["message"]["content"] = json!(text_content);
                }
                if has_reasoning {
                    choice["message"]["reasoning_content"] = json!(reasoning_content);
                }
                if !tool_calls.is_empty() {
                    choice["message"]["tool_calls"] = Value::Array(tool_calls);
                }
                if !images.is_empty() {
                    choice["message"]["images"] = Value::Array(images);
                }
            }

            if has_function_call {
                choice["finish_reason"] = json!("tool_calls");
                choice["native_finish_reason"] = json!("tool_calls");
            }
            choices.push(choice);
        }
        if !choices.is_empty() {
            template["choices"] = Value::Array(choices);
        }
    }
    template
}

// ───────────────────────────── misc.MimeTypes ─────────────────────────────

/// port of misc.MimeTypes (misc/mime-type.go), sorted by extension for binary search.
static MIME_TYPES: &[(&str, &str)] = &[
    ("123", "application/vnd.lotus-1-2-3"),
    ("3dml", "text/vnd.in3d.3dml"),
    ("3ds", "image/x-3ds"),
    ("3g2", "video/3gpp2"),
    ("3gp", "video/3gpp"),
    ("7z", "application/x-7z-compressed"),
    ("aab", "application/x-authorware-bin"),
    ("aac", "audio/x-aac"),
    ("aam", "application/x-authorware-map"),
    ("aas", "application/x-authorware-seg"),
    ("abw", "application/x-abiword"),
    ("ac", "application/pkix-attr-cert"),
    ("acc", "application/vnd.americandynamics.acc"),
    ("ace", "application/x-ace-compressed"),
    ("acu", "application/vnd.acucobol"),
    ("acutc", "application/vnd.acucorp"),
    ("adp", "audio/adpcm"),
    ("aep", "application/vnd.audiograph"),
    ("afm", "application/x-font-type1"),
    ("afp", "application/vnd.ibm.modcap"),
    ("ahead", "application/vnd.ahead.space"),
    ("ai", "application/postscript"),
    ("aiff", "audio/x-aiff"),
    (
        "air",
        "application/vnd.adobe.air-application-installer-package+zip",
    ),
    ("ait", "application/vnd.dvb.ait"),
    ("ami", "application/vnd.amiga.ami"),
    ("apk", "application/vnd.android.package-archive"),
    ("appcache", "text/cache-manifest"),
    ("application", "application/x-ms-application"),
    ("apr", "application/vnd.lotus-approach"),
    ("arc", "application/x-freearc"),
    ("asc", "application/pgp-signature"),
    ("asf", "video/x-ms-asf"),
    ("asm", "text/x-asm"),
    ("aso", "application/vnd.accpac.simply.aso"),
    ("atom", "application/atom+xml"),
    ("atomcat", "application/atomcat+xml"),
    ("atomsvc", "application/atomsvc+xml"),
    ("atx", "application/vnd.antix.game-component"),
    ("au", "audio/basic"),
    ("avi", "video/x-msvideo"),
    ("aw", "application/applixware"),
    ("azf", "application/vnd.airzip.filesecure.azf"),
    ("azs", "application/vnd.airzip.filesecure.azs"),
    ("azw", "application/vnd.amazon.ebook"),
    ("bcpio", "application/x-bcpio"),
    ("bdf", "application/x-font-bdf"),
    ("bdm", "application/vnd.syncml.dm+wbxml"),
    ("bed", "application/vnd.realvnc.bed"),
    ("bh2", "application/vnd.fujitsu.oasysprs"),
    ("bin", "application/octet-stream"),
    ("blb", "application/x-blorb"),
    ("bmi", "application/vnd.bmi"),
    ("bmp", "image/bmp"),
    ("book", "application/vnd.framemaker"),
    ("box", "application/vnd.previewsystems.box"),
    ("btif", "image/prs.btif"),
    ("bz", "application/x-bzip"),
    ("bz2", "application/x-bzip2"),
    ("c", "text/x-c"),
    ("c11amc", "application/vnd.cluetrust.cartomobile-config"),
    ("c11amz", "application/vnd.cluetrust.cartomobile-config-pkg"),
    ("c4d", "application/vnd.clonk.c4group"),
    ("cab", "application/vnd.ms-cab-compressed"),
    ("caf", "audio/x-caf"),
    ("cap", "application/vnd.tcpdump.pcap"),
    ("car", "application/vnd.curl.car"),
    ("cat", "application/vnd.ms-pki.seccat"),
    ("cbr", "application/x-cbr"),
    ("cct", "application/x-director"),
    ("ccxml", "application/ccxml+xml"),
    ("cdbcmsg", "application/vnd.contact.cmsg"),
    ("cdkey", "application/vnd.mediastation.cdkey"),
    ("cdmia", "application/cdmi-capability"),
    ("cdmic", "application/cdmi-container"),
    ("cdmid", "application/cdmi-domain"),
    ("cdmio", "application/cdmi-object"),
    ("cdmiq", "application/cdmi-queue"),
    ("cdx", "chemical/x-cdx"),
    ("cdxml", "application/vnd.chemdraw+xml"),
    ("cdy", "application/vnd.cinderella"),
    ("cer", "application/pkix-cert"),
    ("cfs", "application/x-cfs-compressed"),
    ("cgm", "image/cgm"),
    ("chat", "application/x-chat"),
    ("chm", "application/vnd.ms-htmlhelp"),
    ("chrt", "application/vnd.kde.kchart"),
    ("cif", "chemical/x-cif"),
    (
        "cii",
        "application/vnd.anser-web-certificate-issue-initiation",
    ),
    ("cil", "application/vnd.ms-artgalry"),
    ("cla", "application/vnd.claymore"),
    ("class", "application/java-vm"),
    ("clkk", "application/vnd.crick.clicker.keyboard"),
    ("clkp", "application/vnd.crick.clicker.palette"),
    ("clkt", "application/vnd.crick.clicker.template"),
    ("clkw", "application/vnd.crick.clicker.wordbank"),
    ("clkx", "application/vnd.crick.clicker"),
    ("clp", "application/x-msclip"),
    ("cmc", "application/vnd.cosmocaller"),
    ("cmdf", "chemical/x-cmdf"),
    ("cml", "chemical/x-cml"),
    ("cmp", "application/vnd.yellowriver-custom-menu"),
    ("cmx", "image/x-cmx"),
    ("cod", "application/vnd.rim.cod"),
    ("cpio", "application/x-cpio"),
    ("cpt", "application/mac-compactpro"),
    ("crd", "application/x-mscardfile"),
    ("crl", "application/pkix-crl"),
    ("crt", "application/x-x509-ca-cert"),
    ("cryptonote", "application/vnd.rig.cryptonote"),
    ("csh", "application/x-csh"),
    ("csml", "chemical/x-csml"),
    ("csp", "application/vnd.commonspace"),
    ("css", "text/css"),
    ("csv", "text/csv"),
    ("cu", "application/cu-seeme"),
    ("curl", "text/vnd.curl"),
    ("cww", "application/prs.cww"),
    ("dae", "model/vnd.collada+xml"),
    ("daf", "application/vnd.mobius.daf"),
    ("dart", "application/vnd.dart"),
    ("dataless", "application/vnd.fdsn.seed"),
    ("davmount", "application/davmount+xml"),
    ("dbk", "application/docbook+xml"),
    ("dcurl", "text/vnd.curl.dcurl"),
    ("dd2", "application/vnd.oma.dd2+xml"),
    ("ddd", "application/vnd.fujixerox.ddd"),
    ("deb", "application/x-debian-package"),
    ("dfac", "application/vnd.dreamfactory"),
    ("dgc", "application/x-dgc-compressed"),
    ("dis", "application/vnd.mobius.dis"),
    ("dmg", "application/x-apple-diskimage"),
    ("dna", "application/vnd.dna"),
    ("doc", "application/msword"),
    ("docm", "application/vnd.ms-word.document.macroenabled.12"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("dotm", "application/vnd.ms-word.template.macroenabled.12"),
    (
        "dotx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
    ),
    ("dp", "application/vnd.osgi.dp"),
    ("dpg", "application/vnd.dpgraph"),
    ("dra", "audio/vnd.dra"),
    ("dsc", "text/prs.lines.tag"),
    ("dssc", "application/dssc+der"),
    ("dtb", "application/x-dtbook+xml"),
    ("dtd", "application/xml-dtd"),
    ("dts", "audio/vnd.dts"),
    ("dtshd", "audio/vnd.dts.hd"),
    ("dvb", "video/vnd.dvb.file"),
    ("dvi", "application/x-dvi"),
    ("dwf", "model/vnd.dwf"),
    ("dwg", "image/vnd.dwg"),
    ("dxf", "image/vnd.dxf"),
    ("dxp", "application/vnd.spotfire.dxp"),
    ("ecelp4800", "audio/vnd.nuera.ecelp4800"),
    ("ecelp7470", "audio/vnd.nuera.ecelp7470"),
    ("ecelp9600", "audio/vnd.nuera.ecelp9600"),
    ("ecma", "application/ecmascript"),
    ("edm", "application/vnd.novadigm.edm"),
    ("edx", "application/vnd.novadigm.edx"),
    ("efif", "application/vnd.picsel"),
    ("ei6", "application/vnd.pg.osasli"),
    ("emma", "application/emma+xml"),
    ("eol", "audio/vnd.digital-winds"),
    ("eot", "application/vnd.ms-fontobject"),
    ("epub", "application/epub+zip"),
    ("es3", "application/vnd.eszigno3+xml"),
    ("esa", "application/vnd.osgi.subsystem"),
    ("esf", "application/vnd.epson.esf"),
    ("etx", "text/x-setext"),
    ("eva", "application/x-eva"),
    ("evy", "application/x-envoy"),
    ("exi", "application/exi"),
    ("ext", "application/vnd.novadigm.ext"),
    ("ez", "application/andrew-inset"),
    ("ez2", "application/vnd.ezpix-album"),
    ("ez3", "application/vnd.ezpix-package"),
    ("f4v", "video/x-f4v"),
    ("fbs", "image/vnd.fastbidsheet"),
    ("fcdt", "application/vnd.adobe.formscentral.fcdt"),
    ("fcs", "application/vnd.isac.fcs"),
    ("fdf", "application/vnd.fdf"),
    ("fe_launch", "application/vnd.denovo.fcselayout-link"),
    ("fg5", "application/vnd.fujitsu.oasysgp"),
    ("fig", "application/x-xfig"),
    ("flac", "audio/x-flac"),
    ("fli", "video/x-fli"),
    ("flo", "application/vnd.micrografx.flo"),
    ("flv", "video/x-flv"),
    ("flw", "application/vnd.kde.kivio"),
    ("flx", "text/vnd.fmi.flexstor"),
    ("fly", "text/vnd.fly"),
    ("fnc", "application/vnd.frogans.fnc"),
    ("fpx", "image/vnd.fpx"),
    ("fsc", "application/vnd.fsc.weblaunch"),
    ("fst", "image/vnd.fst"),
    ("ftc", "application/vnd.fluxtime.clip"),
    ("fti", "application/vnd.anser-web-funds-transfer-initiation"),
    ("fvt", "video/vnd.fvt"),
    ("fxp", "application/vnd.adobe.fxp"),
    ("fzs", "application/vnd.fuzzysheet"),
    ("g2w", "application/vnd.geoplan"),
    ("g3", "image/g3fax"),
    ("g3w", "application/vnd.geospace"),
    ("gac", "application/vnd.groove-account"),
    ("gam", "application/x-tads"),
    ("gbr", "application/rpki-ghostbusters"),
    ("gca", "application/x-gca-compressed"),
    ("gdl", "model/vnd.gdl"),
    ("geo", "application/vnd.dynageo"),
    ("gex", "application/vnd.geometry-explorer"),
    ("ggb", "application/vnd.geogebra.file"),
    ("ggt", "application/vnd.geogebra.tool"),
    ("ghf", "application/vnd.groove-help"),
    ("gif", "image/gif"),
    ("gim", "application/vnd.groove-identity-message"),
    ("gml", "application/gml+xml"),
    ("gmx", "application/vnd.gmx"),
    ("gnumeric", "application/x-gnumeric"),
    ("gph", "application/vnd.flographit"),
    ("gpx", "application/gpx+xml"),
    ("gqf", "application/vnd.grafeq"),
    ("gram", "application/srgs"),
    ("gramps", "application/x-gramps-xml"),
    ("grv", "application/vnd.groove-injector"),
    ("grxml", "application/srgs+xml"),
    ("gsf", "application/x-font-ghostscript"),
    ("gtar", "application/x-gtar"),
    ("gtm", "application/vnd.groove-tool-message"),
    ("gtw", "model/vnd.gtw"),
    ("gv", "text/vnd.graphviz"),
    ("gxf", "application/gxf"),
    ("gxt", "application/vnd.geonext"),
    ("h261", "video/h261"),
    ("h263", "video/h263"),
    ("h264", "video/h264"),
    ("hal", "application/vnd.hal+xml"),
    ("hbci", "application/vnd.hbci"),
    ("hdf", "application/x-hdf"),
    ("hlp", "application/winhlp"),
    ("hpgl", "application/vnd.hp-hpgl"),
    ("hpid", "application/vnd.hp-hpid"),
    ("hps", "application/vnd.hp-hps"),
    ("hqx", "application/mac-binhex40"),
    ("htke", "application/vnd.kenameaapp"),
    ("html", "text/html"),
    ("hvd", "application/vnd.yamaha.hv-dic"),
    ("hvp", "application/vnd.yamaha.hv-voice"),
    ("hvs", "application/vnd.yamaha.hv-script"),
    ("i2g", "application/vnd.intergeo"),
    ("icc", "application/vnd.iccprofile"),
    ("ice", "x-conference/x-cooltalk"),
    ("ico", "image/x-icon"),
    ("ics", "text/calendar"),
    ("ief", "image/ief"),
    ("ifm", "application/vnd.shana.informed.formdata"),
    ("igl", "application/vnd.igloader"),
    ("igm", "application/vnd.insors.igm"),
    ("igx", "application/vnd.micrografx.igx"),
    ("iif", "application/vnd.shana.informed.interchange"),
    ("imp", "application/vnd.accpac.simply.imp"),
    ("ims", "application/vnd.ms-ims"),
    ("ink", "application/inkml+xml"),
    ("install", "application/x-install-instructions"),
    ("iota", "application/vnd.astraea-software.iota"),
    ("ipfix", "application/ipfix"),
    ("ipk", "application/vnd.shana.informed.package"),
    ("irm", "application/vnd.ibm.rights-management"),
    ("irp", "application/vnd.irepository.package+xml"),
    ("iso", "application/x-iso9660-image"),
    ("itp", "application/vnd.shana.informed.formtemplate"),
    ("ivp", "application/vnd.immervision-ivp"),
    ("ivu", "application/vnd.immervision-ivu"),
    ("jad", "text/vnd.sun.j2me.app-descriptor"),
    ("jam", "application/vnd.jam"),
    ("jar", "application/java-archive"),
    ("java", "text/x-java-source"),
    ("jisp", "application/vnd.jisp"),
    ("jlt", "application/vnd.hp-jlyt"),
    ("jnlp", "application/x-java-jnlp-file"),
    ("joda", "application/vnd.joost.joda-archive"),
    ("jpg", "image/jpeg"),
    ("jpgv", "video/jpeg"),
    ("js", "application/javascript"),
    ("json", "application/json"),
    ("jsonml", "application/jsonml+json"),
    ("karbon", "application/vnd.kde.karbon"),
    ("kfo", "application/vnd.kde.kformula"),
    ("kia", "application/vnd.kidspiration"),
    ("kml", "application/vnd.google-earth.kml+xml"),
    ("kmz", "application/vnd.google-earth.kmz"),
    ("kne", "application/vnd.kinar"),
    ("kon", "application/vnd.kde.kontour"),
    ("kpr", "application/vnd.kde.kpresenter"),
    ("kpxx", "application/vnd.ds-keypoint"),
    ("ksp", "application/vnd.kde.kspread"),
    ("ktr", "application/vnd.kahootz"),
    ("ktx", "image/ktx"),
    ("kwd", "application/vnd.kde.kword"),
    ("lasxml", "application/vnd.las.las+xml"),
    ("latex", "application/x-latex"),
    ("lbd", "application/vnd.llamagraphics.life-balance.desktop"),
    (
        "lbe",
        "application/vnd.llamagraphics.life-balance.exchange+xml",
    ),
    ("les", "application/vnd.hhe.lesson-player"),
    ("link66", "application/vnd.route66.link66+xml"),
    ("lnk", "application/x-ms-shortcut"),
    ("lostxml", "application/lost+xml"),
    ("lrm", "application/vnd.ms-lrm"),
    ("ltf", "application/vnd.frogans.ltf"),
    ("lvp", "audio/vnd.lucent.voice"),
    ("lwp", "application/vnd.lotus-wordpro"),
    ("lzh", "application/x-lzh-compressed"),
    ("m21", "application/mp21"),
    ("m3u", "audio/x-mpegurl"),
    ("m3u8", "application/vnd.apple.mpegurl"),
    ("m4a", "audio/mp4"),
    ("m4v", "video/x-m4v"),
    ("ma", "application/mathematica"),
    ("mads", "application/mads+xml"),
    ("mag", "application/vnd.ecowin.chart"),
    ("mathml", "application/mathml+xml"),
    ("mbk", "application/vnd.mobius.mbk"),
    ("mbox", "application/mbox"),
    ("mc1", "application/vnd.medcalcdata"),
    ("mcd", "application/vnd.mcd"),
    ("mcurl", "text/vnd.curl.mcurl"),
    ("mdb", "application/x-msaccess"),
    ("mdi", "image/vnd.ms-modi"),
    ("meta4", "application/metalink4+xml"),
    ("metalink", "application/metalink+xml"),
    ("mets", "application/mets+xml"),
    ("mfm", "application/vnd.mfmp"),
    ("mft", "application/rpki-manifest"),
    ("mgp", "application/vnd.osgeo.mapguide.package"),
    ("mgz", "application/vnd.proteus.magazine"),
    ("mid", "audio/midi"),
    ("mie", "application/x-mie"),
    ("mif", "application/vnd.mif"),
    ("mka", "audio/x-matroska"),
    ("mkv", "video/x-matroska"),
    ("mlp", "application/vnd.dolby.mlp"),
    ("mmd", "application/vnd.chipnuts.karaoke-mmd"),
    ("mmf", "application/vnd.smaf"),
    ("mmr", "image/vnd.fujixerox.edmics-mmr"),
    ("mng", "video/x-mng"),
    ("mny", "application/x-msmoney"),
    ("mobi", "application/x-mobipocket-ebook"),
    ("mods", "application/mods+xml"),
    ("movie", "video/x-sgi-movie"),
    ("mp3", "audio/mpeg"),
    ("mp4", "video/mp4"),
    ("mp4s", "application/mp4"),
    ("mpc", "application/vnd.mophun.certificate"),
    ("mpeg", "video/mpeg"),
    ("mpkg", "application/vnd.apple.installer+xml"),
    ("mpm", "application/vnd.blueice.multipass"),
    ("mpn", "application/vnd.mophun.application"),
    ("mpp", "application/vnd.ms-project"),
    ("mpy", "application/vnd.ibm.minipay"),
    ("mqy", "application/vnd.mobius.mqy"),
    ("mrc", "application/marc"),
    ("mrcx", "application/marcxml+xml"),
    ("mscml", "application/mediaservercontrol+xml"),
    ("mseed", "application/vnd.fdsn.mseed"),
    ("mseq", "application/vnd.mseq"),
    ("msf", "application/vnd.epson.msf"),
    ("msl", "application/vnd.mobius.msl"),
    ("msty", "application/vnd.muvee.style"),
    ("mts", "model/vnd.mts"),
    ("mus", "application/vnd.musician"),
    ("musicxml", "application/vnd.recordare.musicxml+xml"),
    ("mwf", "application/vnd.mfer"),
    ("mxf", "application/mxf"),
    ("mxl", "application/vnd.recordare.musicxml"),
    ("mxml", "application/xv+xml"),
    ("mxs", "application/vnd.triscape.mxs"),
    ("n-gage", "application/vnd.nokia.n-gage.symbian.install"),
    ("n3", "text/n3"),
    ("nbp", "application/vnd.wolfram.player"),
    ("ncx", "application/x-dtbncx+xml"),
    ("nfo", "text/x-nfo"),
    ("ngdat", "application/vnd.nokia.n-gage.data"),
    ("nitf", "application/vnd.nitf"),
    ("nlu", "application/vnd.neurolanguage.nlu"),
    ("nml", "application/vnd.enliven"),
    ("nnd", "application/vnd.noblenet-directory"),
    ("nns", "application/vnd.noblenet-sealer"),
    ("nnw", "application/vnd.noblenet-web"),
    ("npx", "image/vnd.net-fpx"),
    ("nsc", "application/x-conference"),
    ("nsf", "application/vnd.lotus-notes"),
    ("nzb", "application/x-nzb"),
    ("oa2", "application/vnd.fujitsu.oasys2"),
    ("oa3", "application/vnd.fujitsu.oasys3"),
    ("oas", "application/vnd.fujitsu.oasys"),
    ("obd", "application/x-msbinder"),
    ("obj", "application/x-tgif"),
    ("oda", "application/oda"),
    ("odb", "application/vnd.oasis.opendocument.database"),
    ("odc", "application/vnd.oasis.opendocument.chart"),
    ("odf", "application/vnd.oasis.opendocument.formula"),
    (
        "odft",
        "application/vnd.oasis.opendocument.formula-template",
    ),
    ("odg", "application/vnd.oasis.opendocument.graphics"),
    ("odi", "application/vnd.oasis.opendocument.image"),
    ("odm", "application/vnd.oasis.opendocument.text-master"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ogg", "audio/ogg"),
    ("ogv", "video/ogg"),
    ("ogx", "application/ogg"),
    ("omdoc", "application/omdoc+xml"),
    ("onepkg", "application/onenote"),
    ("opf", "application/oebps-package+xml"),
    ("opml", "text/x-opml"),
    ("oprc", "application/vnd.palm"),
    ("org", "application/vnd.lotus-organizer"),
    ("osf", "application/vnd.yamaha.openscoreformat"),
    (
        "osfpvg",
        "application/vnd.yamaha.openscoreformat.osfpvg+xml",
    ),
    ("otc", "application/vnd.oasis.opendocument.chart-template"),
    ("otf", "font/otf"),
    (
        "otg",
        "application/vnd.oasis.opendocument.graphics-template",
    ),
    ("oth", "application/vnd.oasis.opendocument.text-web"),
    ("oti", "application/vnd.oasis.opendocument.image-template"),
    (
        "otp",
        "application/vnd.oasis.opendocument.presentation-template",
    ),
    (
        "ots",
        "application/vnd.oasis.opendocument.spreadsheet-template",
    ),
    ("ott", "application/vnd.oasis.opendocument.text-template"),
    ("oxps", "application/oxps"),
    ("oxt", "application/vnd.openofficeorg.extension"),
    ("p10", "application/pkcs10"),
    ("p12", "application/x-pkcs12"),
    ("p7b", "application/x-pkcs7-certificates"),
    ("p7c", "application/pkcs7-mime"),
    ("p7r", "application/x-pkcs7-certreqresp"),
    ("p7s", "application/pkcs7-signature"),
    ("p8", "application/pkcs8"),
    ("pas", "text/x-pascal"),
    ("paw", "application/vnd.pawaafile"),
    ("pbd", "application/vnd.powerbuilder6"),
    ("pbm", "image/x-portable-bitmap"),
    ("pcf", "application/x-font-pcf"),
    ("pcl", "application/vnd.hp-pcl"),
    ("pclxl", "application/vnd.hp-pclxl"),
    ("pcurl", "application/vnd.curl.pcurl"),
    ("pcx", "image/x-pcx"),
    ("pdf", "application/pdf"),
    ("pfr", "application/font-tdpfr"),
    ("pgm", "image/x-portable-graymap"),
    ("pgn", "application/x-chess-pgn"),
    ("pgp", "application/pgp-encrypted"),
    ("pki", "application/pkixcmp"),
    ("pkipath", "application/pkix-pkipath"),
    ("plb", "application/vnd.3gpp.pic-bw-large"),
    ("plc", "application/vnd.mobius.plc"),
    ("plf", "application/vnd.pocketlearn"),
    ("pls", "application/pls+xml"),
    ("pml", "application/vnd.ctc-posml"),
    ("png", "image/png"),
    ("pnm", "image/x-portable-anymap"),
    ("portpkg", "application/vnd.macports.portpkg"),
    (
        "potm",
        "application/vnd.ms-powerpoint.template.macroenabled.12",
    ),
    (
        "potx",
        "application/vnd.openxmlformats-officedocument.presentationml.template",
    ),
    (
        "ppam",
        "application/vnd.ms-powerpoint.addin.macroenabled.12",
    ),
    ("ppd", "application/vnd.cups-ppd"),
    ("ppm", "image/x-portable-pixmap"),
    (
        "ppsm",
        "application/vnd.ms-powerpoint.slideshow.macroenabled.12",
    ),
    (
        "ppsx",
        "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
    ),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptm",
        "application/vnd.ms-powerpoint.presentation.macroenabled.12",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("pre", "application/vnd.lotus-freelance"),
    ("prf", "application/pics-rules"),
    ("psb", "application/vnd.3gpp.pic-bw-small"),
    ("psd", "image/vnd.adobe.photoshop"),
    ("psf", "application/x-font-linux-psf"),
    ("pskcxml", "application/pskc+xml"),
    ("ptid", "application/vnd.pvi.ptid1"),
    ("pub", "application/x-mspublisher"),
    ("pvb", "application/vnd.3gpp.pic-bw-var"),
    ("pwn", "application/vnd.3m.post-it-notes"),
    ("pya", "audio/vnd.ms-playready.media.pya"),
    ("pyv", "video/vnd.ms-playready.media.pyv"),
    ("qam", "application/vnd.epson.quickanime"),
    ("qbo", "application/vnd.intu.qbo"),
    ("qfx", "application/vnd.intu.qfx"),
    ("qps", "application/vnd.publishare-delta-tree"),
    ("qwd", "application/vnd.quark.quarkxpress"),
    ("rar", "application/x-rar-compressed"),
    ("ras", "image/x-cmu-raster"),
    ("rcprofile", "application/vnd.ipunplugged.rcprofile"),
    ("rdf", "application/rdf+xml"),
    ("rdz", "application/vnd.data-vision.rdz"),
    ("rep", "application/vnd.businessobjects"),
    ("res", "application/x-dtbresource+xml"),
    ("rgb", "image/x-rgb"),
    ("rif", "application/reginfo+xml"),
    ("rip", "audio/vnd.rip"),
    ("ris", "application/x-research-info-systems"),
    ("rl", "application/resource-lists+xml"),
    ("rlc", "image/vnd.fujixerox.edmics-rlc"),
    ("rld", "application/resource-lists-diff+xml"),
    ("rm", "application/vnd.rn-realmedia"),
    ("rmp", "audio/x-pn-realaudio-plugin"),
    ("rms", "application/vnd.jcp.javame.midlet-rms"),
    ("rmvb", "application/vnd.rn-realmedia-vbr"),
    ("rnc", "application/relax-ng-compact-syntax"),
    ("roa", "application/rpki-roa"),
    ("rp9", "application/vnd.cloanto.rp9"),
    ("rpss", "application/vnd.nokia.radio-presets"),
    ("rpst", "application/vnd.nokia.radio-preset"),
    ("rq", "application/sparql-query"),
    ("rs", "application/rls-services+xml"),
    ("rsd", "application/rsd+xml"),
    ("rss", "application/rss+xml"),
    ("rtf", "application/rtf"),
    ("rtx", "text/richtext"),
    ("s3m", "audio/s3m"),
    ("saf", "application/vnd.yamaha.smaf-audio"),
    ("sbml", "application/sbml+xml"),
    ("sc", "application/vnd.ibm.secure-container"),
    ("scd", "application/x-msschedule"),
    ("scm", "application/vnd.lotus-screencam"),
    ("scq", "application/scvp-cv-request"),
    ("scs", "application/scvp-cv-response"),
    ("scurl", "text/vnd.curl.scurl"),
    ("sda", "application/vnd.stardivision.draw"),
    ("sdc", "application/vnd.stardivision.calc"),
    ("sdd", "application/vnd.stardivision.impress"),
    ("sdkd", "application/vnd.solent.sdkm+xml"),
    ("sdp", "application/sdp"),
    ("sdw", "application/vnd.stardivision.writer"),
    ("see", "application/vnd.seemail"),
    ("sema", "application/vnd.sema"),
    ("semd", "application/vnd.semd"),
    ("semf", "application/vnd.semf"),
    ("ser", "application/java-serialized-object"),
    ("setpay", "application/set-payment-initiation"),
    ("setreg", "application/set-registration-initiation"),
    ("sfd-hdstx", "application/vnd.hydrostatix.sof-data"),
    ("sfs", "application/vnd.spotfire.sfs"),
    ("sfv", "text/x-sfv"),
    ("sgi", "image/sgi"),
    ("sgl", "application/vnd.stardivision.writer-global"),
    ("sh", "application/x-sh"),
    ("shar", "application/x-shar"),
    ("shf", "application/shf+xml"),
    ("sid", "image/x-mrsid-image"),
    ("sil", "audio/silk"),
    ("sis", "application/vnd.symbian.install"),
    ("sit", "application/x-stuffit"),
    ("sitx", "application/x-stuffitx"),
    ("skd", "application/vnd.koan"),
    (
        "sldm",
        "application/vnd.ms-powerpoint.slide.macroenabled.12",
    ),
    (
        "sldx",
        "application/vnd.openxmlformats-officedocument.presentationml.slide",
    ),
    ("slt", "application/vnd.epson.salt"),
    ("sm", "application/vnd.stepmania.stepchart"),
    ("smf", "application/vnd.stardivision.math"),
    ("smi", "application/smil+xml"),
    ("smv", "video/x-smv"),
    ("smzip", "application/vnd.stepmania.package"),
    ("snf", "application/x-font-snf"),
    ("spf", "application/vnd.yamaha.smaf-phrase"),
    ("spl", "application/x-futuresplash"),
    ("spot", "text/vnd.in3d.spot"),
    ("spp", "application/scvp-vp-response"),
    ("spq", "application/scvp-vp-request"),
    ("sql", "application/x-sql"),
    ("src", "application/x-wais-source"),
    ("srt", "application/x-subrip"),
    ("sru", "application/sru+xml"),
    ("srx", "application/sparql-results+xml"),
    ("ssdl", "application/ssdl+xml"),
    ("sse", "application/vnd.kodak-descriptor"),
    ("ssf", "application/vnd.epson.ssf"),
    ("ssml", "application/ssml+xml"),
    ("st", "application/vnd.sailingtracker.track"),
    ("stc", "application/vnd.sun.xml.calc.template"),
    ("std", "application/vnd.sun.xml.draw.template"),
    ("stf", "application/vnd.wt.stf"),
    ("sti", "application/vnd.sun.xml.impress.template"),
    ("stk", "application/hyperstudio"),
    ("stl", "application/vnd.ms-pki.stl"),
    ("str", "application/vnd.pg.format"),
    ("stw", "application/vnd.sun.xml.writer.template"),
    ("sub", "text/vnd.dvb.subtitle"),
    ("sus", "application/vnd.sus-calendar"),
    ("sv4cpio", "application/x-sv4cpio"),
    ("sv4crc", "application/x-sv4crc"),
    ("svc", "application/vnd.dvb.service"),
    ("svd", "application/vnd.svd"),
    ("svg", "image/svg+xml"),
    ("swf", "application/x-shockwave-flash"),
    ("swi", "application/vnd.aristanetworks.swi"),
    ("sxc", "application/vnd.sun.xml.calc"),
    ("sxd", "application/vnd.sun.xml.draw"),
    ("sxg", "application/vnd.sun.xml.writer.global"),
    ("sxi", "application/vnd.sun.xml.impress"),
    ("sxm", "application/vnd.sun.xml.math"),
    ("sxw", "application/vnd.sun.xml.writer"),
    ("t3", "application/x-t3vm-image"),
    ("taglet", "application/vnd.mynfc"),
    ("tao", "application/vnd.tao.intent-module-archive"),
    ("tar", "application/x-tar"),
    ("tcap", "application/vnd.3gpp2.tcap"),
    ("tcl", "application/x-tcl"),
    ("teacher", "application/vnd.smart.teacher"),
    ("tei", "application/tei+xml"),
    ("tex", "application/x-tex"),
    ("texi", "application/x-texinfo"),
    ("tfi", "application/thraud+xml"),
    ("tfm", "application/x-tex-tfm"),
    ("tga", "image/x-tga"),
    ("thmx", "application/vnd.ms-officetheme"),
    ("tiff", "image/tiff"),
    ("tmo", "application/vnd.tmobile-livetv"),
    ("torrent", "application/x-bittorrent"),
    ("tpl", "application/vnd.groove-tool-template"),
    ("tpt", "application/vnd.trid.tpt"),
    ("tra", "application/vnd.trueapp"),
    ("trm", "application/x-msterminal"),
    ("tsd", "application/timestamped-data"),
    ("tsv", "text/tab-separated-values"),
    ("ttc", "font/collection"),
    ("ttf", "font/ttf"),
    ("ttl", "text/turtle"),
    ("twd", "application/vnd.simtech-mindmapper"),
    ("txd", "application/vnd.genomatix.tuxedo"),
    ("txf", "application/vnd.mobius.txf"),
    ("txt", "text/plain"),
    ("ufd", "application/vnd.ufdl"),
    ("ulx", "application/x-glulx"),
    ("umj", "application/vnd.umajin"),
    ("unityweb", "application/vnd.unity"),
    ("uoml", "application/vnd.uoml+xml"),
    ("ustar", "application/x-ustar"),
    ("utz", "application/vnd.uiq.theme"),
    ("uu", "text/x-uuencode"),
    ("uva", "audio/vnd.dece.audio"),
    ("uvd", "application/vnd.dece.data"),
    ("vcard", "text/vcard"),
    ("vcd", "application/x-cdlink"),
    ("vcf", "text/x-vcard"),
    ("vcg", "application/vnd.groove-vcard"),
    ("vcs", "text/x-vcalendar"),
    ("vcx", "application/vnd.vcx"),
    ("vis", "application/vnd.visionary"),
    ("viv", "video/vnd.vivo"),
    ("vob", "video/x-ms-vob"),
    ("vsf", "application/vnd.vsf"),
    ("vss", "application/vnd.visio"),
    ("vtu", "model/vnd.vtu"),
    ("vxml", "application/voicexml+xml"),
    ("wad", "application/x-doom"),
    ("wav", "audio/x-wav"),
    ("wax", "audio/x-ms-wax"),
    ("wbmp", "image/vnd.wap.wbmp"),
    ("wbs", "application/vnd.criticaltools.wbs+xml"),
    ("wbxml", "application/vnd.wap.wbxml"),
    ("wdp", "image/vnd.ms-photo"),
    ("weba", "audio/webm"),
    ("webm", "video/webm"),
    ("webp", "image/webp"),
    ("wg", "application/vnd.pmi.widget"),
    ("wgt", "application/widget"),
    ("wm", "video/x-ms-wm"),
    ("wma", "audio/x-ms-wma"),
    ("wmd", "application/x-ms-wmd"),
    ("wml", "text/vnd.wap.wml"),
    ("wmlc", "application/vnd.wap.wmlc"),
    ("wmls", "text/vnd.wap.wmlscript"),
    ("wmlsc", "application/vnd.wap.wmlscriptc"),
    ("wmv", "video/x-ms-wmv"),
    ("wmx", "video/x-ms-wmx"),
    ("wmz", "application/x-ms-wmz"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("wpd", "application/vnd.wordperfect"),
    ("wpl", "application/vnd.ms-wpl"),
    ("wps", "application/vnd.ms-works"),
    ("wqd", "application/vnd.wqd"),
    ("wri", "application/x-mswrite"),
    ("wsdl", "application/wsdl+xml"),
    ("wspolicy", "application/wspolicy+xml"),
    ("wtb", "application/vnd.webturbo"),
    ("wvx", "video/x-ms-wvx"),
    ("xaml", "application/xaml+xml"),
    ("xap", "application/x-silverlight-app"),
    ("xar", "application/vnd.xara"),
    ("xbap", "application/x-ms-xbap"),
    ("xbd", "application/vnd.fujixerox.docuworks.binder"),
    ("xbm", "image/x-xbitmap"),
    ("xdf", "application/xcap-diff+xml"),
    ("xdm", "application/vnd.syncml.dm+xml"),
    ("xdp", "application/vnd.adobe.xdp+xml"),
    ("xdssc", "application/dssc+xml"),
    ("xdw", "application/vnd.fujixerox.docuworks"),
    ("xenc", "application/xenc+xml"),
    ("xer", "application/patch-ops-error+xml"),
    ("xfdf", "application/vnd.adobe.xfdf"),
    ("xfdl", "application/vnd.xfdl"),
    ("xhtml", "application/xhtml+xml"),
    ("xif", "image/vnd.xiff"),
    ("xlam", "application/vnd.ms-excel.addin.macroenabled.12"),
    ("xlf", "application/x-xliff+xml"),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsb",
        "application/vnd.ms-excel.sheet.binary.macroenabled.12",
    ),
    ("xlsm", "application/vnd.ms-excel.sheet.macroenabled.12"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("xltm", "application/vnd.ms-excel.template.macroenabled.12"),
    (
        "xltx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
    ),
    ("xm", "audio/xm"),
    ("xml", "application/xml"),
    ("xo", "application/vnd.olpc-sugar"),
    ("xop", "application/xop+xml"),
    ("xpi", "application/x-xpinstall"),
    ("xpl", "application/xproc+xml"),
    ("xpm", "image/x-xpixmap"),
    ("xpr", "application/vnd.is-xpr"),
    ("xps", "application/vnd.ms-xpsdocument"),
    ("xpw", "application/vnd.intercon.formnet"),
    ("xslt", "application/xslt+xml"),
    ("xsm", "application/vnd.syncml+xml"),
    ("xspf", "application/xspf+xml"),
    ("xul", "application/vnd.mozilla.xul+xml"),
    ("xwd", "image/x-xwindowdump"),
    ("xyz", "chemical/x-xyz"),
    ("xz", "application/x-xz"),
    ("yang", "application/yang"),
    ("yin", "application/yin+xml"),
    ("zaz", "application/vnd.zzazz.deck+xml"),
    ("zip", "application/zip"),
    ("zir", "application/vnd.zul"),
    ("zmm", "application/vnd.handheld-entertainment+xml"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn req(body: Value) -> Value {
        translate_request("gemini-3-flash", &body, false)
    }

    fn mode(out: &Value) -> Value {
        get_path(out, "toolConfig.functionCallingConfig.mode")
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Replace each generated `<name>-<nanos>-<counter>` tool-call id with
    /// `<name>-ID` after checking its shape.
    fn norm_ids(v: &mut Value) {
        match v {
            Value::Object(m) => {
                if let (Some(Value::String(id)), Some(f)) =
                    (m.get("id").cloned(), m.get("function"))
                {
                    let name = gstr(f.get("name"));
                    let rest = id.strip_prefix(&format!("{name}-")).expect("id prefix");
                    let (nanos, counter) = rest.split_once('-').expect("two numbers");
                    assert!(nanos.bytes().all(|b| b.is_ascii_digit()) && !nanos.is_empty());
                    assert!(counter.bytes().all(|b| b.is_ascii_digit()) && !counter.is_empty());
                    m.insert("id".into(), Value::String(format!("{name}-ID")));
                }
                for (_, c) in m.iter_mut() {
                    norm_ids(c);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(norm_ids),
            _ => {}
        }
    }

    /// Parse the `data: <json>\n\n` frames of one push.
    fn frames(out: Vec<String>) -> Vec<Value> {
        out.into_iter()
            .map(|f| {
                let body = f
                    .strip_prefix("data: ")
                    .and_then(|b| b.strip_suffix("\n\n"))
                    .expect("chat SSE frame");
                let mut v: Value = serde_json::from_str(body).unwrap();
                norm_ids(&mut v);
                v
            })
            .collect()
    }

    // ── gemini_openai_file_data_test.go ──

    #[test]
    fn normalizes_file_data_url() {
        let out = req(
            json!({"model":"gemini-2.5-pro","messages":[{"role":"user","content":[{"type":"file","file":{"filename":"test.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}]}]}),
        );
        assert_eq!(
            out["contents"][0]["parts"][0]["inlineData"],
            json!({"mime_type":"application/pdf","data":"JVBERi0xLjQK"})
        );
    }

    // ── gemini_openai_signature_test.go ──

    const CAPTURED_SIG: &str =
        "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

    #[test]
    fn tool_call_signature_compatibility() {
        for (raw, want) in [
            (format!("gemini#{CAPTURED_SIG}"), CAPTURED_SIG.to_string()),
            (
                "not-a-provider-signature".to_string(),
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string(),
            ),
        ] {
            let out = req(json!({
                "model": "gemini-3.5-flash",
                "messages": [{"role": "assistant", "tool_calls": [{
                    "id": "call_123", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": raw}}
                }]}]
            }));
            assert_eq!(
                out["contents"][0]["parts"][0]["thoughtSignature"],
                json!(want)
            );
        }
    }

    // ── noop_optimization_test.go ──

    #[test]
    fn normalizes_tool_name_and_strict() {
        let out = req(
            json!({"messages":[],"tools":[{"type":"function","function":{"name":true,"strict":true,"parameters":{"type":"object"}}}]}),
        );
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0],
            json!({"name":"true","parametersJsonSchema":{"type":"object"}})
        );
    }

    #[test]
    fn non_stream_keeps_assistant_role() {
        let out = translate_non_stream(
            &json!({"candidates":[{"index":0,"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}),
            &Value::Null,
        );
        assert_eq!(out["choices"][0]["message"]["role"], json!("assistant"));
    }

    #[test]
    fn streaming_sets_assistant_role_once() {
        let mut t = StreamTranslator::new(&Value::Null);
        let out = frames(t.push(None, &json!({"candidates":[{"index":0,"content":{"parts":[{"text":"hello"},{"functionCall":{"name":"lookup","args":{}}},{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}}]})));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"],
            json!({
                "role": "assistant",
                "content": "hello",
                "reasoning_content": null,
                "tool_calls": [{"id":"lookup-ID","index":0,"type":"function","function":{"name":"lookup","arguments":"{}"}}],
                "images": [{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="},"index":0}]
            })
        );
    }

    // ── gemini_openai_response_test.go ──

    #[test]
    fn stream_completion_tokens_include_thoughts() {
        let mut t = StreamTranslator::new(&Value::Null);
        let out = frames(t.push(None, &json!({"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":5,"thoughtsTokenCount":42,"totalTokenCount":63}})));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["usage"]["completion_tokens"], json!(47));
    }

    #[test]
    fn non_stream_completion_tokens_include_thoughts() {
        let out = translate_non_stream(
            &json!({"usageMetadata":{"promptTokenCount":16,"thoughtsTokenCount":42,"totalTokenCount":58}}),
            &Value::Null,
        );
        assert_eq!(out["usage"]["completion_tokens"], json!(42));
    }

    #[test]
    fn finish_reason_only_on_final_chunk() {
        let mut t = StreamTranslator::new(&Value::Null);
        let r1 = frames(t.push(None, &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"list_dir","args":{"path":"C:/"}}}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"}})));
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0]["choices"][0]["finish_reason"], Value::Null);
        t.push(None, &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"list_dir","args":{"path":"D:/"}}}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"}}));
        let r3 = frames(t.push(None, &json!({"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}})));
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0]["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(r3[0]["choices"][0]["native_finish_reason"], json!("stop"));
    }

    #[test]
    fn non_stream_empty_text_produces_empty_string() {
        let out = translate_non_stream(
            &json!({"candidates":[{"content":{"parts":[{"text":""},{"text":"","thought":true}]},"finishReason":"STOP"}]}),
            &Value::Null,
        );
        assert_eq!(out["choices"][0]["message"]["content"], json!(""));
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], json!(""));
    }

    #[test]
    fn non_stream_audio_transcription_part_produces_content() {
        let out = translate_non_stream(
            &json!({"candidates":[{"content":{"parts":[{"text":""},{"audioTranscription":{"text":"Hello world"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"totalTokenCount":185}}),
            &Value::Null,
        );
        assert_eq!(
            out["choices"][0]["message"]["content"],
            json!("Hello world")
        );
    }

    #[test]
    fn non_stream_single_audio_transcription_part() {
        let out = translate_non_stream(
            &json!({"candidates":[{"content":{"parts":[{"audioTranscription":{"text":"Single transcription part"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"totalTokenCount":185}}),
            &Value::Null,
        );
        assert_eq!(
            out["choices"][0]["message"]["content"],
            json!("Single transcription part")
        );
    }

    #[test]
    fn stream_audio_transcription_part_streams_content() {
        let mut t = StreamTranslator::new(&Value::Null);
        let out = frames(t.push(None, &json!({"candidates":[{"content":{"parts":[{"text":""},{"audioTranscription":{"text":"Testing one two three."}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"candidatesTokenCount":8,"totalTokenCount":193}})));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"]["content"],
            json!("Testing one two three.")
        );
    }

    #[test]
    fn non_stream_text_precedence_over_audio_transcription() {
        let out = translate_non_stream(
            &json!({"candidates":[{"content":{"parts":[{"text":"explicit text","audioTranscription":{"text":"ignored transcription"}}]},"finishReason":"STOP"}]}),
            &Value::Null,
        );
        assert_eq!(
            out["choices"][0]["message"]["content"],
            json!("explicit text")
        );
    }

    // ── gemini_openai_request_test.go ──

    #[test]
    fn strips_trailing_assistant_prefill() {
        let out = translate_request(
            "gemini-3.1-pro-high",
            &json!({"model":"gpt-5.4","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"previous answer"}]}),
            false,
        );
        assert_eq!(
            out["contents"],
            json!([{"role":"user","parts":[{"text":"hello"}]}])
        );
    }

    #[test]
    fn preserves_input_audio() {
        let out = req(
            json!({"messages":[{"role":"user","content":[{"type":"text","text":"Transcribe this audio verbatim."},{"type":"input_audio","input_audio":{"data":"SUQzBA==","format":"mp3"}}]}]}),
        );
        assert_eq!(
            out["contents"][0]["parts"],
            json!([{"text":"Transcribe this audio verbatim."},{"inlineData":{"mime_type":"audio/mpeg","data":"SUQzBA=="}}])
        );
    }

    #[test]
    fn preserves_video_url() {
        let out = req(
            json!({"messages":[{"role":"user","content":[{"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAAIGZ0eXBtcDQy"}},{"type":"text","text":"Describe the video"}]}]}),
        );
        assert_eq!(
            out["contents"][0]["parts"],
            json!([{"inlineData":{"mime_type":"video/mp4","data":"AAAAIGZ0eXBtcDQy"}},{"text":"Describe the video"}])
        );
    }

    #[test]
    fn skips_empty_text_parts_without_nulls() {
        let out = req(json!({"messages":[
            {"role":"user","content":[{"type":"text","text":""},{"type":"input_audio","input_audio":{"data":"SUQzBA==","format":"mp3"}}]},
            {"role":"assistant","content":[{"type":"text","text":""}],"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"{\"output\":\"ok\"}"},
            {"role":"user","content":"done"}
        ]}));
        assert_eq!(
            out["contents"][0]["parts"],
            json!([{"inlineData":{"mime_type":"audio/mpeg","data":"SUQzBA=="}}])
        );
        assert_eq!(
            out["contents"][1]["parts"],
            json!([{"functionCall":{"name":"read_file","args":{"path":"a.txt"}},"thoughtSignature":"skip_thought_signature_validator"}])
        );
    }

    #[test]
    fn preserves_reasoning_content() {
        let out = translate_request(
            "gemini-3-flash",
            &json!({"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"","reasoning_content":"thinking only"},{"role":"user","content":"say ok"}]}),
            true,
        );
        assert_eq!(
            out["contents"],
            json!([
                {"role":"user","parts":[{"text":"hi"}]},
                {"role":"model","parts":[{"text":"thinking only","thought":true}]},
                {"role":"user","parts":[{"text":"say ok"}]}
            ])
        );
    }

    #[test]
    fn preserves_reasoning_before_visible_content_and_tool_call() {
        let out = translate_request(
            "gemini-3-flash",
            &json!({"messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"visible answer","reasoning_content":"thinking only","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"{\"output\":\"ok\"}"},
                {"role":"user","content":"say ok"}
            ]}),
            true,
        );
        assert_eq!(
            out["contents"],
            json!([
                {"role":"user","parts":[{"text":"hi"}]},
                {"role":"model","parts":[
                    {"text":"thinking only","thought":true},
                    {"text":"visible answer"},
                    {"functionCall":{"name":"read_file","args":{}},"thoughtSignature":GEMINI_FUNCTION_THOUGHT_SIGNATURE}
                ]},
                {"role":"user","parts":[{"functionResponse":{"name":"read_file","response":{"result":"\"{\\\"output\\\":\\\"ok\\\"}\""}}}]},
                {"role":"user","parts":[{"text":"say ok"}]}
            ])
        );
    }

    #[test]
    fn skips_empty_assistant_messages() {
        let out = translate_request(
            "gemini-3-flash",
            &json!({"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"","tool_calls":[{"type":"function","function":{"name":"","arguments":"{}"}},{"type":"custom"}]},{"role":"user","content":"say ok"}]}),
            true,
        );
        assert_eq!(
            out["contents"],
            json!([{"role":"user","parts":[{"text":"hi"}]},{"role":"user","parts":[{"text":"say ok"}]}])
        );
    }

    #[test]
    fn mid_session_developer_message_does_not_mutate_system_instruction() {
        let out = req(json!({"messages":[
            {"role":"system","content":"You are a helpful assistant"},
            {"role":"user","content":"Turn 1 user"},
            {"role":"assistant","content":"Turn 1 assistant"},
            {"role":"developer","content":"<image_resize_notice>Image 1 was resized to 800x600</image_resize_notice>"},
            {"role":"user","content":"Turn 2 user"}
        ]}));
        assert_eq!(
            out["systemInstruction"],
            json!({"role":"user","parts":[{"text":"You are a helpful assistant"}]})
        );
        assert_eq!(
            out["contents"],
            json!([
                {"role":"user","parts":[{"text":"Turn 1 user"}]},
                {"role":"model","parts":[{"text":"Turn 1 assistant"}]},
                {"role":"user","parts":[{"text":"<system-reminder>\n<image_resize_notice>Image 1 was resized to 800x600</image_resize_notice>\n</system-reminder>"}]},
                {"role":"user","parts":[{"text":"Turn 2 user"}]}
            ])
        );
    }

    #[test]
    fn mid_session_system_reminder_envelope() {
        let out = req(json!({"messages":[
            {"role":"system","content":"You are a helpful assistant"},
            {"role":"user","content":"Hello"},
            {"role":"assistant","content":"Hi there"},
            {"role":"system","content":"Please decide which tool to call next."},
            {"role":"user","content":"Search for news"}
        ]}));
        assert_eq!(out["contents"].as_array().unwrap().len(), 4);
        assert_eq!(
            out["contents"][2]["parts"][0]["text"],
            json!("<system-reminder>\nPlease decide which tool to call next.\n</system-reminder>")
        );
    }

    #[test]
    fn mid_session_transient_system_instruction_preserves_turn_boundaries() {
        let with = req(json!({"messages":[
            {"role":"system","content":"System prompt"},
            {"role":"user","content":"Turn 1 user"},
            {"role":"assistant","content":"Turn 1 assistant"},
            {"role":"system","content":"Call tool now"},
            {"role":"user","content":"Turn 2 user"}
        ]}));
        let without = req(json!({"messages":[
            {"role":"system","content":"System prompt"},
            {"role":"user","content":"Turn 1 user"},
            {"role":"assistant","content":"Turn 1 assistant"},
            {"role":"user","content":"Turn 2 user"},
            {"role":"assistant","content":"Turn 2 assistant"}
        ]}));
        assert_eq!(with["contents"].as_array().unwrap().len(), 4);
        assert_eq!(
            with["contents"][2],
            json!({"role":"user","parts":[{"text":"<system-reminder>\nCall tool now\n</system-reminder>"}]})
        );
        assert_eq!(
            with["contents"][3],
            json!({"role":"user","parts":[{"text":"Turn 2 user"}]})
        );
        assert_eq!(with["contents"][0], without["contents"][0]);
        assert_eq!(with["contents"][1], without["contents"][1]);
        assert_eq!(with["contents"][3], without["contents"][2]);
    }

    #[test]
    fn mid_session_system_reminder_object_and_array_content() {
        let out = req(json!({"messages":[
            {"role":"user","content":"Hello"},
            {"role":"assistant","content":"Hi"},
            {"role":"system","content":{"type":"text","text":"Object instruction"}},
            {"role":"developer","content":[{"type":"text","text":"Array instruction"}]}
        ]}));
        assert_eq!(
            out["contents"][2]["parts"][0]["text"],
            json!("<system-reminder>\nObject instruction\n</system-reminder>")
        );
        assert_eq!(
            out["contents"][3]["parts"][0]["text"],
            json!("<system-reminder>\nArray instruction\n</system-reminder>")
        );
    }

    #[test]
    fn maps_max_tokens() {
        for (body, want) in [
            (
                json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":30}),
                30,
            ),
            (
                json!({"messages":[{"role":"user","content":"hi"}],"max_completion_tokens":40}),
                40,
            ),
            (
                json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":30,"max_completion_tokens":40}),
                30,
            ),
        ] {
            let out = translate_request("gemini-2.0-flash", &body, false);
            assert_eq!(out["generationConfig"], json!({"maxOutputTokens": want}));
        }
    }

    #[test]
    fn cleans_tool_schema_required_fields() {
        let out = translate_request(
            "gemini-2.0-flash",
            &json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"search_company","description":"Search","parameters":{"type":"object","title":"SearchCompany","properties":{"country":{"type":"string"},"industry":{"type":"string"}},"required":["country","industry","stale_field","another_stale"]}}}]}),
            false,
        );
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            json!({"type":"object","properties":{"country":{"type":"string"},"industry":{"type":"string"}},"required":["country","industry"]})
        );
    }

    #[test]
    fn response_format_json_schema() {
        let out = translate_request(
            "gemini-3.1-flash-lite",
            &json!({
                "generationConfig": {"temperature": 0.2, "responseSchema": {"type": "string"}},
                "messages": [{"role": "user", "content": "Return structured JSON."}],
                "response_format": {"type": "json_schema", "json_schema": {"name": "response", "strict": true, "schema": {
                    "type": "object", "properties": {"cleanedContent": {"type": "string"}},
                    "required": ["cleanedContent"], "additionalProperties": false
                }}}
            }),
            false,
        );
        assert_eq!(
            out["generationConfig"],
            json!({
                "temperature": 0.2,
                "responseMimeType": "application/json",
                "responseJsonSchema": {"type": "object", "properties": {"cleanedContent": {"type": "string"}}, "required": ["cleanedContent"], "additionalProperties": false}
            })
        );
    }

    #[test]
    fn response_format_json_object() {
        let out = translate_request(
            "gemini-3.1-flash-lite",
            &json!({"generationConfig":{"temperature":0.6},"messages":[{"role":"user","content":"Return a JSON object."}],"response_format":{"type":"json_object"}}),
            false,
        );
        assert_eq!(
            out["generationConfig"],
            json!({"temperature":0.6,"responseMimeType":"application/json"})
        );
    }

    #[test]
    fn response_format_json_schema_without_schema() {
        let out = translate_request(
            "gemini-3.1-flash-lite",
            &json!({"messages":[{"role":"user","content":"Return structured JSON."}],"response_format":{"type":"json_schema","json_schema":{"name":"response"}}}),
            false,
        );
        assert_eq!(
            out["generationConfig"],
            json!({"responseMimeType":"application/json"})
        );
    }

    #[test]
    fn response_format_no_op() {
        for body in [
            json!({"messages":[{"role":"user","content":"plain text"}],"temperature":0.5}),
            json!({"messages":[{"role":"user","content":"plain text"}],"temperature":0.5,"response_format":{"type":"text"}}),
        ] {
            let out = translate_request("gemini-3.1-flash-lite", &body, false);
            assert_eq!(out["generationConfig"], json!({"temperature":0.5}));
        }
    }

    #[test]
    fn multi_turn_repeated_tool_call_id_issue_5933() {
        let out = req(json!({"messages":[
            {"role":"user","content":"list files"},
            {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"glob","arguments":"{\"pattern\":\"*.go\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"[\"main.go\"]"},
            {"role":"user","content":"read main.go"},
            {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"main.go\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"package main"}
        ]}));
        assert_eq!(
            out["contents"][1]["parts"][0]["functionCall"]["name"],
            json!("glob")
        );
        assert_eq!(
            out["contents"][2]["parts"][0]["functionResponse"],
            json!({"name":"glob","response":{"result":"\"[\\\"main.go\\\"]\""}})
        );
        assert_eq!(
            out["contents"][4]["parts"][0]["functionCall"]["name"],
            json!("read")
        );
        assert_eq!(
            out["contents"][5]["parts"][0]["functionResponse"],
            json!({"name":"read","response":{"result":"\"package main\""}})
        );
    }

    #[test]
    fn parallel_and_out_of_order_tool_responses() {
        let out = req(json!({"messages":[
            {"role":"user","content":"run parallel tools"},
            {"role":"assistant","tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"tool_a","arguments":"{}"}},
                {"id":"call_2","type":"function","function":{"name":"tool_b","arguments":"{}"}}
            ]},
            {"role":"tool","tool_call_id":"call_2","content":"res_b"},
            {"role":"tool","tool_call_id":"call_1","content":"res_a"}
        ]}));
        assert_eq!(
            out["contents"][2],
            json!({"role":"user","parts":[
                {"functionResponse":{"name":"tool_a","response":{"result":"\"res_a\""}}},
                {"functionResponse":{"name":"tool_b","response":{"result":"\"res_b\""}}}
            ]})
        );
    }

    fn tool_a() -> Value {
        json!([{"type":"function","function":{"name":"tool_a","parameters":{"type":"object","properties":{}}}}])
    }

    #[test]
    fn tool_choice_mapping() {
        let msgs = json!([{"role":"user","content":"test"}]);
        // named function → ANY + allowedFunctionNames
        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"function","function":{"name":"tool_a"}},"tools":tool_a()}),
        );
        assert_eq!(
            out["toolConfig"],
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["tool_a"]}})
        );
        // none / auto / required
        for (choice, want) in [("none", "NONE"), ("auto", "AUTO"), ("required", "ANY")] {
            let out = req(json!({"messages":msgs,"tool_choice":choice,"tools":tool_a()}));
            assert_eq!(mode(&out), json!(want), "{choice}");
        }
        // parallel_tool_calls false fails closed, null/true do not
        for (choice, parallel, want) in [
            ("auto", json!(false), "NONE"),
            ("required", json!(false), "NONE"),
            ("auto", Value::Null, "AUTO"),
            ("auto", json!(true), "AUTO"),
        ] {
            let out = req(
                json!({"messages":msgs,"tool_choice":choice,"parallel_tool_calls":parallel,"tools":tool_a()}),
            );
            assert_eq!(mode(&out), json!(want), "{choice} {parallel}");
        }
    }

    #[test]
    fn tool_choice_allowed_tools() {
        let msgs = json!([{"role":"user","content":"test"}]);
        let two = json!([
            {"type":"function","function":{"name":"tool_a","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"tool_b","parameters":{"type":"object","properties":{}}}}
        ]);
        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"tool_b"}}]}},"tools":two}),
        );
        assert_eq!(
            out["toolConfig"],
            json!({"functionCallingConfig":{"mode":"AUTO"}})
        );
        assert_eq!(
            out["tools"][0]["functionDeclarations"],
            json!([{"name":"tool_b","parametersJsonSchema":{"type":"object","properties":{}}}])
        );

        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"tool_b"}}]}},"tools":two}),
        );
        assert_eq!(
            out["toolConfig"],
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["tool_b"]}})
        );

        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"allowed_tools","allowed_tools":{"tools":[]}},"tools":tool_a()}),
        );
        assert_eq!(mode(&out), json!("NONE"));

        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"function","function":{}},"tools":tool_a()}),
        );
        assert_eq!(mode(&out), json!("NONE"));

        // exact original name match, no sanitization conflation
        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"allowed_tools","allowed_tools":{"tools":[{"type":"function","function":{"name":"1tool"}}]}},"tools":[{"type":"function","function":{"name":"_1tool","parameters":{"type":"object","properties":{}}}}]}),
        );
        assert_eq!(mode(&out), json!("NONE"));

        // sanitized collision
        let colliding = json!([
            {"type":"function","function":{"name":"1tool","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"_1tool","parameters":{"type":"object","properties":{}}}}
        ]);
        let out = req(json!({"messages":msgs,"tool_choice":"auto","tools":colliding}));
        assert_eq!(mode(&out), json!("NONE"));

        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"function","function":{"name":"undeclared_tool"}},"tools":tool_a()}),
        );
        assert_eq!(mode(&out), json!("NONE"));

        // filtering avoids a false collision with an excluded tool
        let out = req(
            json!({"messages":msgs,"tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"_1tool"}}]}},"tools":colliding}),
        );
        assert_eq!(mode(&out), json!("AUTO"));
        assert_eq!(
            out["tools"][0]["functionDeclarations"],
            json!([{"name":"_1tool","parametersJsonSchema":{"type":"object","properties":{}}}])
        );

        let out = req(json!({"messages":msgs,"tool_choice":null,"tools":tool_a()}));
        assert_eq!(out.get("toolConfig"), None);
    }

    #[test]
    fn tool_strict_maps_to_validated_mode() {
        let cases: Vec<(Option<Value>, &str, Option<Value>)> = vec![
            (None, "VALIDATED", None),
            (Some(json!("auto")), "VALIDATED", None),
            (Some(Value::Null), "VALIDATED", None),
            (Some(json!("required")), "ANY", None),
            (Some(json!("none")), "NONE", None),
            (
                Some(json!({"type":"function","function":{"name":"tool_a"}})),
                "ANY",
                Some(json!(["tool_a"])),
            ),
            (
                Some(
                    json!({"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"tool_a"}}]}}),
                ),
                "VALIDATED",
                None,
            ),
            (
                Some(
                    json!({"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"tool_a"}}]}}),
                ),
                "ANY",
                Some(json!(["tool_a"])),
            ),
        ];
        for (choice, want_mode, want_names) in cases {
            let mut body = json!({
                "messages":[{"role":"user","content":"hi"}],
                "tools":[{"type":"function","function":{"name":"tool_a","description":"Controlled tool.","strict":true,"parameters":{"type":"object","properties":{}}}}]
            });
            if let Some(c) = &choice {
                body["tool_choice"] = c.clone();
            }
            let out = translate_request("gemini-3.1-pro-high", &body, false);
            assert_eq!(
                out["tools"][0]["functionDeclarations"][0],
                json!({"name":"tool_a","description":"Controlled tool.","parametersJsonSchema":{"type":"object","properties":{}}})
            );
            let mut want = json!({"mode": want_mode});
            if let Some(names) = want_names {
                want["allowedFunctionNames"] = names;
            }
            assert_eq!(
                out["toolConfig"]["functionCallingConfig"], want,
                "{choice:?}"
            );
        }

        let out = req(json!({"messages":[{"role":"user","content":"hi"}],"tools":[
            {"type":"function","function":{"name":"tool_a","description":"Loose tool.","strict":false,"parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"tool_b","description":"Strict tool.","strict":true,"parameters":{"type":"object","properties":{}}}}
        ]}));
        assert_eq!(mode(&out), json!("VALIDATED"));

        let out = req(json!({"messages":[{"role":"user","content":"hi"}],"tools":[
            {"type":"function","function":{"name":"tool_a","description":"Loose tool.","strict":false,"parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"tool_b","description":"Unspecified tool.","parameters":{"type":"object","properties":{}}}}
        ]}));
        assert_eq!(out.get("toolConfig"), None);
    }

    #[test]
    fn parameters_json_schema_preserves_additional_properties_and_pattern_issue_5959() {
        let out = translate_request(
            "gemini-2.5-flash",
            &json!({"messages":[{"role":"user","content":"Use the submit tool."}],"tools":[{"type":"function","function":{"name":"submit","description":"Submit a bounded schema test value.","parameters":{
                "$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,
                "properties":{"recipient":{"type":"string","pattern":"^(alice|bob)$"},"amount":{"type":"number"}},
                "required":["recipient","amount"]}}}]}),
            false,
        );
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            json!({"type":"object","additionalProperties":false,"properties":{"recipient":{"type":"string","pattern":"^(alice|bob)$"},"amount":{"type":"number"}},"required":["recipient","amount"]})
        );
    }

    // ── golden outputs recorded from the Go implementation (commit ed980be) ──

    /// (name, stream flag, client body, Go output)
    const GOLDEN_REQUESTS: &[(&str, bool, &str, &str)] = &[
        (
            "full",
            true,
            r###"{"model":"gpt-x","temperature":1,"top_p":0.9,"top_k":40,"max_completion_tokens":100,"n":2,"reasoning_effort":" High ","modalities":["text","image","audio"],"image_config":{"aspect_ratio":"16:9","image_size":"2K"},"response_format":{"type":"json_schema","json_schema":{"name":"r","schema":{"type":"object","properties":{"a":{"type":"string"}}}}},"messages":[{"role":"system","content":"You are helpful"},{"role":"developer","content":[{"type":"text","text":"Dev rule"},{"type":"image_url"}]},{"role":"user","content":[{"type":"text","text":"look"},{"type":"text","text":""},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}},{"type":"image_url","image_url":{"url":"https://example.com/x.png"}},{"type":"file","file":{"filename":"doc.pdf","file_data":"JVBERi0="}},{"type":"file","file":{"filename":"noext","file_data":"AAAA"}},{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"flac"}},{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"opus"}},{"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAAIGZ0eXA="}}]},{"role":"assistant","content":"Let me check","reasoning_content":"I should call tools","tool_calls":[{"id":"call_a","type":"function","function":{"name":"get weather!","arguments":"{\"city\":\"Paris\",\"n\":1}"},"extra_content":{"google":{"thought_signature":"gemini#EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"}}},{"id":"call_b","type":"function","function":{"name":"lookup","arguments":"{}","extra_content":{"google":{"thought_signature":"skip_thought_signature_validator"}}}},{"id":"call_c","type":"function","function":{"name":"noresp","arguments":"[1,2]"},"thought_signature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"id":"call_d","type":"custom","custom":{"name":"x"}}]},{"role":"tool","tool_call_id":"call_b","content":[{"type":"text","text":"found it"}]},{"role":"tool","tool_call_id":"call_a","content":"sunny"},{"role":"user","content":"thanks"}],"tools":[{"type":"function","strict":true,"function":{"name":"get weather!","description":"Weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}},{"type":"function","function":{"name":"lookup"}},{"type":"function","function":{"name":"noresp","strict":false,"parameters":{"type":"object"}}},{"google_search":{}},{"code_execution":{}},{"url_context":{"x":1}}],"tool_choice":"auto","parallel_tool_calls":true}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"look"},{"inlineData":{"mime_type":"image/png","data":"iVBORw0KGgo="}},{"inlineData":{"mime_type":"application/pdf","data":"JVBERi0="}},{"inlineData":{"mime_type":"audio/flac","data":"UklGRg=="}},{"inlineData":{"mime_type":"audio/opus","data":"UklGRg=="}},{"inlineData":{"mime_type":"video/mp4","data":"AAAAIGZ0eXA="}}]},{"role":"model","parts":[{"text":"I should call tools","thought":true},{"text":"Let me check"},{"functionCall":{"name":"get_weather_","args":{"city":"Paris","n":1}},"thoughtSignature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"functionCall":{"name":"lookup","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"noresp","args":[1,2]},"thoughtSignature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"}]},{"role":"user","parts":[{"functionResponse":{"name":"get_weather_","response":{"result":"\"sunny\""}}},{"functionResponse":{"name":"lookup","response":{"result":"[{\"type\":\"text\",\"text\":\"found it\"}]"}}},{"functionResponse":{"name":"noresp","response":{"result":"{}"}}}]},{"role":"user","parts":[{"text":"thanks"}]}],"model":"m","generationConfig":{"thinkingConfig":{"thinkingLevel":"high"},"temperature":1,"topP":0.9,"topK":40,"maxOutputTokens":100,"candidateCount":2,"responseMimeType":"application/json","responseJsonSchema":{"type":"object","properties":{"a":{"type":"string"}}},"responseModalities":["TEXT","IMAGE"],"imageConfig":{"aspectRatio":"16:9","imageSize":"2K"}},"systemInstruction":{"role":"user","parts":[{"text":"You are helpful"},{"text":"Dev rule"},{"text":""}]},"tools":[{"functionDeclarations":[{"name":"get_weather_","description":"Weather","parametersJsonSchema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}},{"name":"lookup","parametersJsonSchema":{"type":"object","properties":{}}},{"name":"noresp","parametersJsonSchema":{"type":"object"}}]},{"googleSearch":{}},{"codeExecution":{}},{"urlContext":{"x":1}}],"toolConfig":{"functionCallingConfig":{"mode":"VALIDATED"}},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "auto_effort",
            false,
            r###"{"generationConfig":{"temperature":0.3,"thinkingConfig":{"includeThoughts":true}},"reasoning_effort":"auto","temperature":0.7,"max_tokens":50,"max_completion_tokens":60,"n":1,"response_format":{"type":"json_object"},"messages":[{"role":"user","content":{"type":"text","text":"hi"}},{"role":"assistant","content":"prefill"}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"model":"m","generationConfig":{"temperature":0.7,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":-1},"maxOutputTokens":50,"responseMimeType":"application/json"},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "sole_system",
            false,
            r###"{"messages":[{"role":"system","content":"only system"}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"<system-reminder>\nonly system\n</system-reminder>"}]}],"model":"m","safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "blank_system_demoted",
            false,
            r###"{"messages":[{"role":"user","content":"a"},{"role":"system","content":"   "},{"role":"developer","content":{"type":"text","text":"obj"}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"a"}]},{"role":"user","parts":[{"text":"   "}]},{"role":"user","parts":[{"text":"<system-reminder>\nobj\n</system-reminder>"}]}],"model":"m","safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "tool_choice_named_sanitized",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tool_choice":{"type":"tool","name":"1tool"},"tools":[{"type":"function","function":{"name":"1tool","parameters":{"type":"object","properties":{}}}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"_1tool","parametersJsonSchema":{"type":"object","properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["_1tool"]}},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "allowed_tools_fallback",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tool_choice":{"type":"allowed_tools","mode":"ANY","tools":[{"name":"a"},{"function":{"name":"c"}}]},"tools":[{"type":"function","function":{"name":"a"}},{"type":"function","function":{"name":"b"}},{"type":"function","function":{"name":"c"}}],"parallel_tool_calls":false}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","properties":{}}},{"name":"c","parametersJsonSchema":{"type":"object","properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"NONE"}},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "tool_choice_number",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tool_choice":5,"tools":[{"type":"function","function":{"name":"a"}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"NONE"}},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "parallel_false_no_tools",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"parallel_tool_calls":false}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","toolConfig":{"functionCallingConfig":{"mode":"NONE"}},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "name_not_string",
            false,
            r###"{"messages":[],"tools":[{"type":"function","function":{"description":"no name"}},{"type":"function","function":{"name":123,"parameters":{"type":"object"}}}]}"###,
            r###"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"description":"no name","parametersJsonSchema":{"type":"object","properties":{}},"name":""},{"name":"_123","parametersJsonSchema":{"type":"object"}}]}],"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "assistant_array_and_tool_null",
            false,
            r###"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"text","text":"see"},{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j/4AAQ"}},{"type":"refusal","refusal":"no"}],"tool_calls":[{"id":"t1","type":"function","function":{"name":"f","arguments":"{\"a\":[1,2]}"}}]},{"role":"tool","tool_call_id":"t1","content":null},{"role":"tool","tool_call_id":"","content":"ignored"},{"role":"user","content":"next"}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"hi"}]},{"role":"model","parts":[{"text":"see"},{"inlineData":{"mime_type":"image/jpeg","data":"/9j/4AAQ"}},{"functionCall":{"name":"f","args":{"a":[1,2]}},"thoughtSignature":"skip_thought_signature_validator"}]},{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"result":"null"}}}]},{"role":"user","parts":[{"text":"next"}]}],"model":"m","safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "schema_heavy",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"heavy","parameters":{"$schema":"https://json-schema.org/draft/2020-12/schema","$id":"x","title":"Heavy","type":"object","additionalProperties":false,"$defs":{"Addr":{"type":"object","properties":{"street":{"type":"string"}}}},"properties":{"addr":{"$ref":"#/$defs/Addr","description":"Address"},"mode":{"const":"fast"},"level":{"type":"integer","enum":[1,2,3]},"one":{"enum":["only"]},"maybe":{"anyOf":[{"type":"string"},{"type":"null"}],"description":"Maybe str"},"choice":{"oneOf":[{"type":"string","description":"s"},{"type":"object","properties":{"k":{"type":"number"}}},{"type":"array","items":{"type":"string"}}]},"multi":{"type":["string","number","null"],"title":"Multi","nullable":true},"arr":{"type":["array","null"],"items":{"type":"string"}},"noitems":{"type":"array"},"itemsonly":{"items":{"type":"integer"},"minItems":1},"badtype":{"type":"string","items":{"type":"string"}},"merged":{"allOf":[{"properties":{"p1":{"type":"string"}},"required":["p1"]},{"properties":{"p2":{"type":"string"}},"description":"from allOf"}]},"cond":{"type":"object","properties":{"kind":{"type":"string"}},"if":{"properties":{"kind":{"const":"a"}}},"then":{"properties":{"extra":{"type":"string"}}},"else":{"properties":{"other":{"type":"boolean"}}}},"ext":{"type":"string","x-google-enum-descriptions":["a"],"pattern":"^a$","format":"email","default":"q","deprecated":true,"$comment":"c"},"obj":{"type":"object","additionalProperties":{"type":"string"},"propertyNames":{"pattern":"^[a-z]+$"},"patternProperties":{"^x":{"type":"string"}}},"title":{"type":"string","description":"a property named title"},"properties":{"type":"object","properties":{"inner":{"type":"string","title":"T"}}},"_":{"type":"boolean"}},"required":["addr","mode","maybe","multi","ghost","_"],"x-root-ext":true}}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"heavy","parametersJsonSchema":{"additionalProperties":false,"properties":{"_":{"type":"boolean"},"addr":{"type":"object","description":"Address (See: Addr)"},"arr":{"items":{"type":"string"},"type":"array","description":"(nullable)"},"badtype":{"type":"string"},"choice":{"properties":{"k":{"type":"number"}},"type":"object","description":"Accepts: string | object | array"},"cond":{"properties":{"kind":{"type":"string"},"extra":{"type":"string"},"other":{"type":"boolean"}},"type":"object"},"ext":{"default":"q","format":"email","pattern":"^a$","type":"string"},"itemsonly":{"items":{"type":"integer"},"minItems":1,"type":"array"},"level":{"enum":["1","2","3"],"type":"string","description":"Allowed: 1, 2, 3"},"maybe":{"type":"string","description":"Maybe str (Accepts: string | null)"},"merged":{"properties":{"p1":{"type":"string"},"p2":{"type":"string"}},"required":["p1"],"description":"from allOf"},"mode":{"enum":["fast"],"type":"string"},"multi":{"type":"string","description":"Accepts: string | number ((nullable))"},"noitems":{"items":{"type":"string"},"type":"array"},"obj":{"additionalProperties":{"type":"string"},"type":"object"},"one":{"enum":["only"],"type":"string"},"properties":{"properties":{"inner":{"type":"string"}},"type":"object"},"title":{"description":"a property named title","type":"string"}},"required":["addr","mode","maybe","_"],"type":"object"}}]}],"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "schema_malformed",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"mal","parameters":{"type":"object","zeta":{"type":"string","required":true},"alpha":{"type":"number","required":false},"properties":{"list":{"type":"array"},"flag":true,"nested":{"type":"object","properties":{"deep":{"type":"string","required":true}}}}}}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"mal","parametersJsonSchema":{"properties":{"alpha":{"type":"number"},"flag":{},"list":{"items":{"type":"string"},"type":"array"},"nested":{"properties":{"deep":{"type":"string"}},"required":["deep"],"type":"object"},"zeta":{"type":"string"}},"required":["zeta"],"type":"object"}}]}],"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "schema_ref_root_desc",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"r","parameters":{"type":"object","properties":{"a":{"$ref":"#/definitions/Thing","description":"An a"},"b":{"type":"object","properties":{"_":{"type":"boolean"}},"required":["_"]},"c":{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}},"required":["reason"]}},"definitions":{"Thing":{"type":"string"}}}}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"r","parametersJsonSchema":{"type":"object","properties":{"a":{"type":"object","description":"An a (See: Thing)"},"b":{"type":"object","properties":{}},"c":{"type":"object","properties":{}}}}}]}],"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "sigs",
            false,
            r###"{"messages":[{"role":"user","content":"x"},{"role":"assistant","tool_calls":[{"id":"c0","type":"function","function":{"name":"f0","arguments":"{}"},"thought_signature":"google# EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"id":"c1","type":"function","function":{"name":"f1","arguments":"{}"},"thought_signature":" EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA "},{"id":"c2","type":"function","function":{"name":"f2","arguments":"{}"},"thought_signature":"gemini#context_engineering_is_the_way_to_go"},{"id":"c3","type":"function","function":{"name":"f3","arguments":"{}"},"thought_signature":"claude#EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"id":"c4","type":"function","function":{"name":"f4","arguments":"{}"},"thought_signature":"foo#EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"id":"c5","type":"function","function":{"name":"f5","arguments":"{}"},"thought_signature":"bm90IGEgc2ln"},{"id":"c6","type":"function","function":{"name":"f6","arguments":"{}"},"thought_signature":null},{"id":"c7","type":"function","function":{"name":"f7","arguments":"{}"},"thought_signature":123},{"id":"c8","type":"function","function":{"name":"f8","arguments":"{}"},"thought_signature":"gpt#gAAAAB"},{"id":"c9","type":"function","function":{"name":"f9","arguments":"{}"},"thought_signature":"gemini#bm90"},{"id":"c10","type":"function","function":{"name":"f10","arguments":"{}"},"thought_signature":"sealed.v1.x"},{"id":"c11","type":"function","function":{"name":"f11","arguments":"{}"},"thought_signature":"MDAwMDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwMDAw"}]},{"role":"user","content":"y"}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]},{"role":"model","parts":[{"functionCall":{"name":"f0","args":{}},"thoughtSignature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"functionCall":{"name":"f1","args":{}},"thoughtSignature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},{"functionCall":{"name":"f2","args":{}},"thoughtSignature":"context_engineering_is_the_way_to_go"},{"functionCall":{"name":"f3","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f4","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f5","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f6","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f7","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f8","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f9","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f10","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"f11","args":{}},"thoughtSignature":"skip_thought_signature_validator"}]},{"role":"user","parts":[{"functionResponse":{"name":"f0","response":{"result":"{}"}}},{"functionResponse":{"name":"f1","response":{"result":"{}"}}},{"functionResponse":{"name":"f2","response":{"result":"{}"}}},{"functionResponse":{"name":"f3","response":{"result":"{}"}}},{"functionResponse":{"name":"f4","response":{"result":"{}"}}},{"functionResponse":{"name":"f5","response":{"result":"{}"}}},{"functionResponse":{"name":"f6","response":{"result":"{}"}}},{"functionResponse":{"name":"f7","response":{"result":"{}"}}},{"functionResponse":{"name":"f8","response":{"result":"{}"}}},{"functionResponse":{"name":"f9","response":{"result":"{}"}}},{"functionResponse":{"name":"f10","response":{"result":"{}"}}},{"functionResponse":{"name":"f11","response":{"result":"{}"}}}]},{"role":"user","parts":[{"text":"y"}]}],"model":"m","safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "strict_named",
            false,
            r###"{"messages":[{"role":"user","content":"x"}],"tool_choice":{"type":"function","function":{"name":"my.tool"}},"tools":[{"type":"function","function":{"name":"my.tool","strict":true,"parameters":{"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":false}}}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"model":"m","tools":[{"functionDeclarations":[{"name":"my.tool","parametersJsonSchema":{"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":false}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["my.tool"]}},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
        (
            "user_file_dataurl_and_text_obj_assistant",
            false,
            r###"{"messages":[{"role":"user","content":[{"type":"file","file":{"filename":"a.TXT","file_data":"DATA:text/plain;charset=utf-8;BASE64,aGk="}},{"type":"file","file":{"file_data":"data:;base64,aGk="}},{"type":"file","file":{"filename":"x.csv","file_data":"aGk="}}]},{"role":"assistant","content":{"type":"text","text":"obj is ignored"}},{"role":"user","content":"z"}]}"###,
            r###"{"contents":[{"role":"user","parts":[{"inlineData":{"mime_type":"text/plain","data":"aGk="}},{"inlineData":{"mime_type":"text/csv","data":"aGk="}}]},{"role":"user","parts":[{"text":"z"}]}],"model":"m","safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"###,
        ),
    ];

    /// (schema, Go CleanJSONSchemaForGeminiJSONSchema output)
    const GOLDEN_SCHEMAS: &[(&str, &str)] = &[
        (
            r###"{"type":"object","properties":{"a":{"enum":[]},"b":{"const":2.5},"c":{"anyOf":[{"type":"integer"},{"type":"string"}]}}}"###,
            r###"{"type":"object","properties":{"a":{"enum":null,"type":"string"},"b":{"enum":["2.5"],"type":"string"},"c":{"type":"integer","description":"Accepts: integer | string"}}}"###,
        ),
        (
            r###"{"type":"object","properties":{"o":{"type":"object","properties":{"x":{"type":"string"}},"anyOf":[{"properties":{"y":{"type":"number"}}},{"type":"null"}]}}}"###,
            r###"{"type":"object","properties":{"o":{"type":"object","properties":{"x":{"type":"string"},"y":{"type":"number"}}}}}"###,
        ),
        (
            r###"{"anyOf":[{"type":"string"},{"type":"integer"}],"description":"root"}"###,
            r###"{"type":"string","description":"root (Accepts: string | integer)"}"###,
        ),
        (r###"true"###, r###"{}"###),
        (
            r###"{"type":"object","properties":{"n":{"type":["null"]},"e":{"enum":[true,null,1.0,{"z":1,"a":2}]}}}"###,
            r###"{"type":"object","properties":{"n":{"type":"string","description":"(nullable)"},"e":{"enum":["true","","1","{\"z\":1,\"a\":2}"],"type":"string","description":"Allowed: true, , 1, {\"z\":1,\"a\":2}"}}}"###,
        ),
        (
            r###"{"allOf":[{"required":["a"]},{"required":["a","b"],"properties":{"a":{"type":"string"},"b":{"type":"integer"}}}],"then":{"properties":{"c":{"type":"string"}}}}"###,
            r###"{"properties":{"c":{"type":"string"},"a":{"type":"string"},"b":{"type":"integer"}},"required":["a","b"]}"###,
        ),
        (
            r###"{"$ref":"#/definitions/Root","description":"root desc"}"###,
            r###"{"type":"object","description":"root desc (See: Root)"}"###,
        ),
        (
            r###"{"type":"object","properties":{"items":{"type":"string"},"enum":{"type":"array","items":{"type":"string"}},"x-keep":{"type":"string","x-drop":1},"a.b":{"type":["integer","null"]},"c":{"type":["null","boolean"]}},"required":["items","a.b","c","enum"]}"###,
            r###"{"type":"object","properties":{"items":{"type":"string"},"enum":{"type":"array","items":{"type":"string"}},"x-keep":{"type":"string"},"a.b":{"type":"integer","description":"(nullable)"},"c":{"type":"boolean","description":"(nullable)"}},"required":["items","enum"]}"###,
        ),
        (
            r###"{"type":"array","items":[{"type":"string"},true],"prefixItems":[true]}"###,
            r###"{"items":[{"type":"string"},{}],"prefixItems":[{}],"type":"array"}"###,
        ),
        (
            r###"{"type":"object","properties":{"p":{"type":"object","properties":{"_":{"type":"boolean"}},"required":["_","q"]},"r":{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"},"z":{"type":"string"}},"required":["reason"]}}}"###,
            r###"{"type":"object","properties":{"p":{"type":"object","properties":{}},"r":{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"},"z":{"type":"string"}},"required":["reason"]}}}"###,
        ),
        (
            r###"{"schema":{"type":"object","properties":{"a":{"type":"string","required":true}}}}"###,
            r###"{"schema":{"properties":{"a":{"type":"string"}},"required":["a"],"type":"object"}}"###,
        ),
        (
            r###"{"type":"object","properties":{"u":{"anyOf":[{"type":"null"}]},"v":{"oneOf":[{"enum":["a","b"]},{"type":"null"}],"description":"V"},"w":{"type":["array","string"],"items":{"type":"string"}}}}"###,
            r###"{"type":"object","properties":{"u":{"type":"null"},"v":{"enum":["a","b"],"type":"string","description":"V (Allowed: a, b) (Accepts: string | null)"},"w":{"type":"array","items":{"type":"string"},"description":"Accepts: array | string"}}}"###,
        ),
        (
            r###"{"type":"object","properties":{"k":{"type":"string","enum":["a","b","c","d","e","f","g","h","i","j","k"]},"d":{"type":"string","description":"keep (Allowed: x, y)","enum":["x","y"]}}}"###,
            r###"{"type":"object","properties":{"k":{"type":"string","enum":["a","b","c","d","e","f","g","h","i","j","k"]},"d":{"type":"string","description":"keep (Allowed: x, y)","enum":["x","y"]}}}"###,
        ),
    ];

    /// (name, original request, upstream chunks, Go output frames per chunk)
    type GoldenStream = (
        &'static str,
        &'static str,
        &'static [&'static str],
        &'static [&'static [&'static str]],
    );
    const GOLDEN_STREAMS: &[GoldenStream] = &[
        (
            "usage_only",
            r###"{}"###,
            &[
                r###"{"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":5,"thoughtsTokenCount":42,"totalTokenCount":63},"createTime":"2024-05-01T12:00:00.123456Z"}"###,
            ],
            &[&[
                r###"{"id":"","object":"chat.completion.chunk","created":1714564800,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}],"usage":{"completion_tokens":47,"total_tokens":63,"prompt_tokens":16,"completion_tokens_details":{"reasoning_tokens":42}}}"###,
            ]],
        ),
        (
            "multi",
            r###"{"tools":[{"name":"my tool"}]}"###,
            &[
                r###"{"createTime":"bogus","candidates":[{"index":0,"content":{"parts":[{"thoughtSignature":"abc"},{"functionCall":{"name":"my_tool","args":{"a":1}}},{"functionCall":{"name":"other"}},{"inline_data":{"mime_type":"image/jpeg","data":"/9j/"}},{"inlineData":{"data":""}},{"inlineData":{"data":"iVBO"}}]}},{"index":1,"content":{"parts":[{"text":"cand1","thought_signature":"zz"}]}}]}"###,
                r###"{"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"name":"third","args":{}}}]},"finishReason":"MAX_TOKENS"},{"index":1,"content":{"parts":[{"text":"end"}]},"finishReason":"max_tokens"}],"usageMetadata":{"promptTokenCount":1}}"###,
                r###"{"candidates":[]}"###,
                r###"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"SAFETY"}]}"###,
            ],
            &[
                &[
                    r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"id":"my tool-ID","index":0,"type":"function","function":{"name":"my tool","arguments":"{\"a\":1}"}},{"id":"other-ID","index":1,"type":"function","function":{"name":"other","arguments":""}}],"images":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j/"},"index":0},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBO"},"index":1}]},"finish_reason":null,"native_finish_reason":null}]}"###,
                    r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":1,"delta":{"role":"assistant","content":"cand1","reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"###,
                ],
                &[
                    r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"id":"third-ID","index":2,"type":"function","function":{"name":"third","arguments":"{}"}}]},"finish_reason":"tool_calls","native_finish_reason":"max_tokens"}],"usage":{"completion_tokens":0,"prompt_tokens":1}}"###,
                    r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":1,"delta":{"role":"assistant","content":"end","reasoning_content":null,"tool_calls":null},"finish_reason":"max_tokens","native_finish_reason":"max_tokens"}],"usage":{"completion_tokens":0,"prompt_tokens":1}}"###,
                ],
                &[],
                &[
                    r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"x","reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"###,
                ],
            ],
        ),
        (
            "audio",
            r###"{}"###,
            &[
                r###"{"candidates":[{"content":{"parts":[{"text":""},{"audioTranscription":{"text":"Testing one two three."}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"candidatesTokenCount":8,"totalTokenCount":193}}"###,
            ],
            &[&[
                r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"Testing one two three.","reasoning_content":null,"tool_calls":null},"finish_reason":"stop","native_finish_reason":"stop"}],"usage":{"completion_tokens":8,"total_tokens":193,"prompt_tokens":185}}"###,
            ]],
        ),
        (
            "text_after_fc_same_chunk",
            r###"{}"###,
            &[
                r###"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{"x":[1,{"y":2}]}}},{"text":"after","thought":"true"},{"functionCall":{"name":"b","args":{}}}]},"finishReason":"STOP"}],"usageMetadata":{}}"###,
            ],
            &[&[
                r###"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":"after","tool_calls":[{"id":"a-ID","index":0,"type":"function","function":{"name":"a","arguments":"{\"x\":[1,{\"y\":2}]}"}},{"id":"b-ID","index":1,"type":"function","function":{"name":"b","arguments":"{}"}}]},"finish_reason":"tool_calls","native_finish_reason":"stop"}],"usage":{"completion_tokens":0,"prompt_tokens":0}}"###,
            ]],
        ),
    ];

    /// (name, original request, upstream response, Go output)
    const GOLDEN_NON_STREAM: &[(&str, &str, &str, &str)] = &[
        (
            "full",
            r###"{"tools":[{"name":"my tool"}]}"###,
            r###"{"candidates":[{"index":0,"content":{"parts":[{"text":"think ","thought":true},{"text":"more","thought":true},{"text":"Hello"},{"text":" world"},{"functionCall":{"name":"my_tool","args":{"q":"x"}}},{"functionCall":{"name":"noargs"}},{"inlineData":{"mimeType":"image/png","data":"iVBO"}},{"inline_data":{"data":"R0lG"}},{"inlineData":{"data":""}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":3,"totalTokenCount":18,"cachedContentTokenCount":2},"modelVersion":"gemini-2.5-flash","responseId":"r1","createTime":"2025-01-02T03:04:05Z"}"###,
            r###"{"id":"r1","object":"chat.completion","created":1735787045,"model":"gemini-2.5-flash","choices":[{"index":0,"message":{"role":"assistant","content":"Hello world","reasoning_content":"think more","tool_calls":[{"id":"my tool-ID","type":"function","function":{"name":"my tool","arguments":"{\"q\":\"x\"}"}},{"id":"noargs-ID","type":"function","function":{"name":"noargs","arguments":""}}],"images":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBO"},"index":0},{"type":"image_url","image_url":{"url":"data:image/png;base64,R0lG"},"index":1}]},"finish_reason":"tool_calls","native_finish_reason":"tool_calls"}],"usage":{"completion_tokens":8,"total_tokens":18,"prompt_tokens":10,"completion_tokens_details":{"reasoning_tokens":3},"prompt_tokens_details":{"cached_tokens":2}}}"###,
        ),
        (
            "multi",
            r###"{}"###,
            r###"{"candidates":[{"index":0,"content":{"parts":[{"text":"a"}]},"finishReason":"MAX_TOKENS"},{"index":1,"content":{"parts":[{"audioTranscription":{"text":"b"}}]}}]}"###,
            r###"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"a","reasoning_content":null,"tool_calls":null},"finish_reason":"max_tokens","native_finish_reason":"max_tokens"},{"index":1,"message":{"role":"assistant","content":"b","reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"###,
        ),
        (
            "empty",
            r###"{}"###,
            r###"{}"###,
            r###"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[]}"###,
        ),
        (
            "empty_text",
            r###"{}"###,
            r###"{"candidates":[{"content":{"parts":[{"text":""},{"text":"","thought":true}]},"finishReason":"STOP"}]}"###,
            r###"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":null},"finish_reason":"stop","native_finish_reason":"stop"}]}"###,
        ),
    ];

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Equal as values AND as serialized text, so key order is checked too.
    fn assert_same(got: &Value, want: &Value, ctx: &str) {
        assert_eq!(got, want, "{ctx}");
        assert_eq!(got.to_string(), want.to_string(), "{ctx}: key order");
    }

    #[test]
    fn golden_requests_match_go() {
        for (name, stream, body, want) in GOLDEN_REQUESTS {
            let got = translate_request("m", &parse(body), *stream);
            assert_same(&got, &parse(want), name);
        }
    }

    #[test]
    fn golden_schemas_match_go() {
        for (schema, want) in GOLDEN_SCHEMAS {
            let got = clean_json_schema_for_gemini_json_schema(&parse(schema));
            assert_same(&got, &parse(want), schema);
        }
    }

    #[test]
    fn golden_streams_match_go() {
        for (name, original, chunks, want) in GOLDEN_STREAMS {
            let mut t = StreamTranslator::new(&parse(original));
            assert_eq!(chunks.len(), want.len());
            for (chunk, want_frames) in chunks.iter().zip(want.iter()) {
                let got = frames(t.push(None, &parse(chunk)));
                let want_frames: Vec<Value> = want_frames.iter().map(|f| parse(f)).collect();
                assert_eq!(got.len(), want_frames.len(), "{name}");
                for (g, w) in got.iter().zip(&want_frames) {
                    assert_same(g, w, name);
                }
            }
            assert_eq!(t.finish(), vec!["data: [DONE]\n\n".to_string()]);
        }
    }

    #[test]
    fn golden_non_stream_match_go() {
        for (name, original, upstream, want) in GOLDEN_NON_STREAM {
            let mut got = translate_non_stream(&parse(upstream), &parse(original));
            norm_ids(&mut got);
            assert_same(&got, &parse(want), name);
        }
    }

    /// A realistic Gemini stream (thought → text → function call with a
    /// signature → final chunk with usage) produces exactly these client frames.
    #[test]
    fn end_to_end_stream_frames() {
        let original = json!({"model":"gpt-x","messages":[{"role":"user","content":"weather?"}],"tools":[{"type":"function","function":{"name":"get_weather"}}],"stream":true});
        let upstream = [
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Thinking about weather","thought":true}]},"index":0}],"usageMetadata":{"promptTokenCount":12,"totalTokenCount":20,"thoughtsTokenCount":8},"modelVersion":"gemini-2.5-pro","responseId":"resp-1"}),
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Let me check."}]},"index":0}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3,"totalTokenCount":23,"thoughtsTokenCount":8},"modelVersion":"gemini-2.5-pro","responseId":"resp-1"}),
            json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}},"thoughtSignature":"EjQKMgEM"}]},"index":0}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":9,"totalTokenCount":29,"thoughtsTokenCount":8},"modelVersion":"gemini-2.5-pro","responseId":"resp-1"}),
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":9,"totalTokenCount":29,"thoughtsTokenCount":8,"cachedContentTokenCount":4},"modelVersion":"gemini-2.5-pro","responseId":"resp-1"}),
        ];
        let mut t = StreamTranslator::new(&original);
        let mut got: Vec<String> = Vec::new();
        for chunk in &upstream {
            got.extend(t.push(None, chunk));
        }
        got.extend(t.finish());

        // The generated tool-call id is `<name>-<unix nanos>-<counter>`.
        let call: Value =
            serde_json::from_str(got[2].strip_prefix("data: ").unwrap().trim_end()).unwrap();
        let id = call["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut normalized = call.clone();
        norm_ids(&mut normalized);
        let got: Vec<String> = got
            .into_iter()
            .map(|f| f.replace(&id, "get_weather-ID"))
            .collect();

        let want = vec![
            "data: {\"id\":\"resp-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gemini-2.5-pro\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning_content\":\"Thinking about weather\",\"tool_calls\":null},\"finish_reason\":null,\"native_finish_reason\":null}],\"usage\":{\"completion_tokens\":8,\"total_tokens\":20,\"prompt_tokens\":12,\"completion_tokens_details\":{\"reasoning_tokens\":8}}}\n\n",
            "data: {\"id\":\"resp-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gemini-2.5-pro\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Let me check.\",\"reasoning_content\":null,\"tool_calls\":null},\"finish_reason\":null,\"native_finish_reason\":null}],\"usage\":{\"completion_tokens\":11,\"total_tokens\":23,\"prompt_tokens\":12,\"completion_tokens_details\":{\"reasoning_tokens\":8}}}\n\n",
            "data: {\"id\":\"resp-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gemini-2.5-pro\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning_content\":null,\"tool_calls\":[{\"id\":\"get_weather-ID\",\"index\":0,\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]},\"finish_reason\":null,\"native_finish_reason\":null}],\"usage\":{\"completion_tokens\":17,\"total_tokens\":29,\"prompt_tokens\":12,\"completion_tokens_details\":{\"reasoning_tokens\":8}}}\n\n",
            "data: {\"id\":\"resp-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gemini-2.5-pro\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning_content\":null,\"tool_calls\":null},\"finish_reason\":\"tool_calls\",\"native_finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":17,\"total_tokens\":29,\"prompt_tokens\":12,\"completion_tokens_details\":{\"reasoning_tokens\":8},\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n",
        ];
        assert_eq!(got, want);
        assert_eq!(
            normalized["choices"][0]["delta"]["tool_calls"][0]["id"],
            json!("get_weather-ID")
        );
    }

    #[test]
    fn non_json_tool_arguments() {
        let out = req(json!({"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","tool_calls":[
                {"id":"a","type":"function","function":{"name":"empty","arguments":""}},
                {"id":"b","type":"function","function":{"name":"bad","arguments":"not json"}}
            ]}
        ]}));
        assert_eq!(
            out["contents"][1]["parts"][0]["functionCall"]["args"],
            json!({})
        );
        assert_eq!(
            out["contents"][1]["parts"][1]["functionCall"]["args"],
            json!("not json")
        );
    }

    #[test]
    fn mime_table_is_sorted_and_unique() {
        assert!(MIME_TYPES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(MIME_TYPES.len(), 732);
    }
}
