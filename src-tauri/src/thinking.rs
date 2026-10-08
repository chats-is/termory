// A faithful port of CLIProxyAPI's internal/thinking: parts that only the
// registry-backed (validated) path or other providers reach are kept for
// parity even though the router, which has no model registry, never calls them.
#![allow(dead_code)]

//! Reasoning-effort ("thinking") handling, ported from CLIProxyAPI
//! (`internal/thinking/`, its `provider/{codex,openai,claude,xai,gemini}` appliers and
//! the executor wrapper `internal/runtime/executor/helps/{thinking,model_capabilities}.go`),
//! upstream revision `ed980be` (2026-09-26).
//!
//! A client may carry a thinking setting in the model name — `gpt-5(high)`,
//! `claude-x(8192)`, `m(none)`, `m(auto)`, `m(-1)` — or in the request body.
//! The router strips the suffix for routing ([`parse_suffix`]), translates the
//! request, then calls [`apply_thinking`] on the TRANSLATED body so the setting
//! lands in the upstream's own field (`reasoning.effort`, `reasoning_effort`,
//! `thinking.*` / `output_config.effort`).
//!
//! # No model registry
//!
//! CLIProxyAPI resolves each model's thinking capabilities from its static
//! registry (`registry.LookupModelInfo`). Termory has no such registry, so
//! [`lookup_model_info`] always answers `None`. Go's `applyThinking` treats a
//! `nil` model the same as a user-defined one (`IsUserDefinedModel(nil) ==
//! true`) and routes it to `applyUserDefinedModel`: the config is NOT validated
//! or clamped, it is written into the target format as-is and the upstream is
//! left to accept or reject it. That is the behaviour of [`apply_thinking`].
//! A caller that does know a model's limits can pass them through
//! [`apply_thinking_with_model`] / [`ModelBinding::Bound`], which takes Go's
//! validated path (`ValidateConfig`, level/budget clamping).
//!
//! # Deliberate differences from the Go source
//!
//! - Bodies are `serde_json::Value`, so the Go branches for empty / invalid JSON
//!   bytes are unreachable and not ported; a non-object root is left unchanged
//!   by every write.
//! - On a validation error Go returns the target body AND the error; here only
//!   the [`ThinkingError`] is returned (the executor answers 400 either way).
//! - Debug/warn logging is omitted.
//! - Out of scope and absent: the antigravity / interactions / kimi
//!   appliers, config extractors and summary arms (those formats behave as an
//!   unknown provider — pass-through), plugin applier registration, and
//!   `GetThinkingText`. The gemini applier, extractor and summary arms ARE
//!   ported (format name "gemini").

use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// types.go
// ---------------------------------------------------------------------------

/// port of ThinkingMode (types.go)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    #[default]
    Budget,
    Level,
    None,
    Auto,
}

impl ThinkingMode {
    // port of ThinkingMode.String (types.go)
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingMode::Budget => "budget",
            ThinkingMode::Level => "level",
            ThinkingMode::None => "none",
            ThinkingMode::Auto => "auto",
        }
    }
}

// port of the ThinkingLevel constants (types.go)
pub const LEVEL_NONE: &str = "none";
pub const LEVEL_AUTO: &str = "auto";
pub const LEVEL_MINIMAL: &str = "minimal";
pub const LEVEL_LOW: &str = "low";
pub const LEVEL_MEDIUM: &str = "medium";
pub const LEVEL_HIGH: &str = "high";
pub const LEVEL_XHIGH: &str = "xhigh";
pub const LEVEL_MAX: &str = "max";

/// port of ThinkingConfig (types.go). `level` is Go's `ThinkingLevel` string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkingConfig {
    pub mode: ThinkingMode,
    pub budget: i64,
    pub level: String,
}

impl ThinkingConfig {
    fn none() -> Self {
        ThinkingConfig {
            mode: ThinkingMode::None,
            budget: 0,
            level: String::new(),
        }
    }
    fn auto() -> Self {
        ThinkingConfig {
            mode: ThinkingMode::Auto,
            budget: -1,
            level: String::new(),
        }
    }
    fn with_level(level: impl Into<String>) -> Self {
        ThinkingConfig {
            mode: ThinkingMode::Level,
            budget: 0,
            level: level.into(),
        }
    }
    fn with_budget(budget: i64) -> Self {
        ThinkingConfig {
            mode: ThinkingMode::Budget,
            budget,
            level: String::new(),
        }
    }
}

/// port of SuffixResult (types.go). `suffix` is `Some(RawSuffix)` exactly when
/// Go's `HasSuffix` is true (the raw text may be empty: `m()`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedModel {
    pub model_name: String,
    pub suffix: Option<String>,
}

impl ParsedModel {
    pub fn has_suffix(&self) -> bool {
        self.suffix.is_some()
    }
    fn raw_suffix(&self) -> &str {
        self.suffix.as_deref().unwrap_or("")
    }
}

/// port of registry.ThinkingSupport (internal/registry/model_registry.go)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkingSupport {
    pub min: i64,
    pub max: i64,
    pub zero_allowed: bool,
    pub dynamic_allowed: bool,
    pub levels: Vec<String>,
}

/// The subset of registry.ModelInfo (internal/registry/model_registry.go) the
/// thinking package reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    /// Go's `Type` ("claude", "openai", "codex", "openai-compatibility", ...).
    pub model_type: String,
    pub max_completion_tokens: i64,
    pub support_configuration_update: bool,
    pub user_defined: bool,
    pub thinking: Option<ThinkingSupport>,
}

/// Stand-in for registry.LookupModelInfo (internal/registry/model_registry.go).
/// Termory has no model registry, so every model is unknown.
pub fn lookup_model_info(_model_id: &str, _provider: &str) -> Option<ModelInfo> {
    None
}

// ---------------------------------------------------------------------------
// errors.go
// ---------------------------------------------------------------------------

/// port of ErrorCode (errors.go)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidSuffix,
    UnknownLevel,
    ThinkingNotSupported,
    LevelNotSupported,
    BudgetOutOfRange,
    ProviderMismatch,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidSuffix => "INVALID_SUFFIX",
            ErrorCode::UnknownLevel => "UNKNOWN_LEVEL",
            ErrorCode::ThinkingNotSupported => "THINKING_NOT_SUPPORTED",
            ErrorCode::LevelNotSupported => "LEVEL_NOT_SUPPORTED",
            ErrorCode::BudgetOutOfRange => "BUDGET_OUT_OF_RANGE",
            ErrorCode::ProviderMismatch => "PROVIDER_MISMATCH",
        }
    }
}

/// port of ThinkingError (errors.go)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingError {
    pub code: ErrorCode,
    pub message: String,
    pub model: String,
}

impl ThinkingError {
    // port of NewThinkingError (errors.go)
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ThinkingError {
            code,
            message: message.into(),
            model: String::new(),
        }
    }
    // port of NewThinkingErrorWithModel (errors.go)
    fn with_model(code: ErrorCode, message: impl Into<String>, model: &str) -> Self {
        ThinkingError {
            code,
            message: message.into(),
            model: model.to_string(),
        }
    }
    // port of ThinkingError.StatusCode (errors.go)
    pub fn status_code(&self) -> u16 {
        400
    }
}

// port of ThinkingError.Error (errors.go)
impl std::fmt::Display for ThinkingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ThinkingError {}

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

/// gjson.Result.Int(): numbers truncate, strings parse as a plain integer
/// (0 when they are not one), true is 1.
fn gj_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Bool(true)) => 1,
        Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// sjson.Set: creates missing objects along the path, replaces a value in
/// place (key order kept). A non-object root or intermediate array is left as is.
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

/// `if v := gjson.Get(path); v.Exists() && v.IsObject() && len(v.Map()) == 0 { sjson.Delete(path) }`
fn sj_delete_if_empty_object(root: &mut Value, path: &str) {
    if matches!(gj_get(root, path), Some(Value::Object(map)) if map.is_empty()) {
        sj_delete(root, path);
    }
}

fn lower_trim(s: &str) -> String {
    s.trim().to_lowercase()
}

// ---------------------------------------------------------------------------
// suffix.go
// ---------------------------------------------------------------------------

/// port of ParseSuffix (suffix.go)
pub fn parse_suffix(model: &str) -> ParsedModel {
    let Some(last_open) = model.rfind('(') else {
        return ParsedModel {
            model_name: model.to_string(),
            suffix: None,
        };
    };
    if !model.ends_with(')') {
        return ParsedModel {
            model_name: model.to_string(),
            suffix: None,
        };
    }
    ParsedModel {
        model_name: model[..last_open].to_string(),
        suffix: Some(model[last_open + 1..model.len() - 1].to_string()),
    }
}

/// port of ParseNumericSuffix (suffix.go)
pub fn parse_numeric_suffix(raw_suffix: &str) -> Option<i64> {
    if raw_suffix.is_empty() {
        return None;
    }
    let value = raw_suffix.parse::<i64>().ok()?;
    if value < 0 {
        return None;
    }
    Some(value)
}

/// port of ParseSpecialSuffix (suffix.go)
pub fn parse_special_suffix(raw_suffix: &str) -> Option<ThinkingMode> {
    if raw_suffix.is_empty() {
        return None;
    }
    match raw_suffix.to_lowercase().as_str() {
        "none" => Some(ThinkingMode::None),
        "auto" | "-1" => Some(ThinkingMode::Auto),
        _ => None,
    }
}

/// port of ParseLevelSuffix (suffix.go)
pub fn parse_level_suffix(raw_suffix: &str) -> Option<&'static str> {
    if raw_suffix.is_empty() {
        return None;
    }
    match raw_suffix.to_lowercase().as_str() {
        "minimal" => Some(LEVEL_MINIMAL),
        "low" => Some(LEVEL_LOW),
        "medium" => Some(LEVEL_MEDIUM),
        "high" => Some(LEVEL_HIGH),
        "xhigh" => Some(LEVEL_XHIGH),
        "max" => Some(LEVEL_MAX),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// convert.go
// ---------------------------------------------------------------------------

/// port of ConvertLevelToBudget (convert.go)
pub fn convert_level_to_budget(level: &str) -> Option<i64> {
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

pub const THRESHOLD_MINIMAL: i64 = 512;
pub const THRESHOLD_LOW: i64 = 1024;
pub const THRESHOLD_MEDIUM: i64 = 8192;
pub const THRESHOLD_HIGH: i64 = 24576;

/// port of ConvertBudgetToLevel (convert.go)
pub fn convert_budget_to_level(budget: i64) -> Option<&'static str> {
    if budget < -1 {
        None
    } else if budget == -1 {
        Some(LEVEL_AUTO)
    } else if budget == 0 {
        Some(LEVEL_NONE)
    } else if budget <= THRESHOLD_MINIMAL {
        Some(LEVEL_MINIMAL)
    } else if budget <= THRESHOLD_LOW {
        Some(LEVEL_LOW)
    } else if budget <= THRESHOLD_MEDIUM {
        Some(LEVEL_MEDIUM)
    } else if budget <= THRESHOLD_HIGH {
        Some(LEVEL_HIGH)
    } else {
        Some(LEVEL_XHIGH)
    }
}

/// port of HasLevel (convert.go)
pub fn has_level(levels: &[String], target: &str) -> bool {
    levels
        .iter()
        .any(|level| level.trim().eq_ignore_ascii_case(target))
}

/// port of MapToClaudeEffort (convert.go)
pub fn map_to_claude_effort(level: &str, supports_max: bool) -> Option<&'static str> {
    match lower_trim(level).as_str() {
        "minimal" => Some("low"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" => Some(if supports_max { "max" } else { "high" }),
        "auto" => Some("high"),
        _ => None,
    }
}

/// port of ModelCapability (convert.go)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCapability {
    Unknown,
    None,
    BudgetOnly,
    LevelOnly,
    Hybrid,
}

/// port of detectModelCapability (convert.go)
pub fn detect_model_capability(model_info: Option<&ModelInfo>) -> ModelCapability {
    let Some(info) = model_info else {
        return ModelCapability::Unknown;
    };
    let Some(support) = &info.thinking else {
        return ModelCapability::None;
    };
    let has_budget = support.min > 0 || support.max > 0;
    let has_levels = !support.levels.is_empty();
    match (has_budget, has_levels) {
        (true, true) => ModelCapability::Hybrid,
        (true, false) => ModelCapability::BudgetOnly,
        (false, true) => ModelCapability::LevelOnly,
        (false, false) => ModelCapability::None,
    }
}

// ---------------------------------------------------------------------------
// strip.go
// ---------------------------------------------------------------------------

/// port of StripThinkingConfig (strip.go)
pub fn strip_thinking_config(body: &Value, provider: &str) -> Value {
    let paths: &[&str] = match provider {
        "claude" => &["thinking", "output_config.effort"],
        "gemini" => &["generationConfig.thinkingConfig"],
        "antigravity" => &["request.generationConfig.thinkingConfig"],
        "interactions" => &[
            "generation_config.thinking_level",
            "generation_config.thinkingLevel",
            "generation_config.thinking_budget",
            "generation_config.thinkingBudget",
            "generation_config.thinking_summaries",
            "generation_config.thinkingSummaries",
            "generation_config.thinking_config",
            "generation_config.thinkingConfig",
        ],
        "openai" => &["reasoning_effort", "reasoning"],
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => &["reasoning_effort", "thinking"],
        "codex" | "xai" => &["reasoning"],
        _ => return body.clone(),
    };
    let mut result = body.clone();
    for path in paths {
        sj_delete(&mut result, path);
    }
    if provider == "claude" {
        sj_delete_if_empty_object(&mut result, "output_config");
    }
    result
}

// ---------------------------------------------------------------------------
// configuration_update.go
// ---------------------------------------------------------------------------

/// port of isResponsesFormat (configuration_update.go)
fn is_responses_format(format: &str) -> bool {
    format == "codex" || format == "openai-response"
}

/// port of extractConfigurationUpdateConfig (configuration_update.go)
fn extract_configuration_update_config(body: &Value) -> ThinkingConfig {
    let Some(Value::Array(input)) = gj_get(body, "input") else {
        return ThinkingConfig::default();
    };
    let mut effort = String::new();
    for item in input {
        if gj_string(gj_get(item, "type")) == "configuration_update" {
            if let Some(Value::String(value)) = gj_get(item, "reasoning.effort") {
                let normalized = lower_trim(value);
                if !normalized.is_empty() {
                    effort = normalized;
                }
            }
        }
    }
    match effort.as_str() {
        "" => ThinkingConfig::default(),
        "none" => ThinkingConfig::none(),
        "auto" => ThinkingConfig::auto(),
        _ => ThinkingConfig::with_level(effort),
    }
}

/// port of stripConfigurationUpdates (configuration_update.go)
fn strip_configuration_updates(mut body: Value) -> Value {
    let Some(Value::Array(input)) = gj_get(&body, "input") else {
        return body;
    };
    let mut removed = false;
    let mut kept = Vec::with_capacity(input.len());
    for item in input {
        if gj_string(gj_get(item, "type")) == "configuration_update" {
            removed = true;
        } else {
            kept.push(item.clone());
        }
    }
    if !removed {
        return body;
    }
    sj_set(&mut body, "input", Value::Array(kept));
    body
}

/// port of stripResponsesEffort (configuration_update.go)
fn strip_responses_effort(mut body: Value) -> Value {
    if gj_get(&body, "reasoning.effort").is_none() {
        return body;
    }
    sj_delete(&mut body, "reasoning.effort");
    sj_delete_if_empty_object(&mut body, "reasoning");
    body
}

// ---------------------------------------------------------------------------
// validate.go
// ---------------------------------------------------------------------------

/// port of ValidateConfig (validate.go)
pub fn validate_config(
    mut config: ThinkingConfig,
    model_info: Option<&ModelInfo>,
    from_format: &str,
    to_format: &str,
    from_suffix: bool,
) -> Result<ThinkingConfig, ThinkingError> {
    let from_format = lower_trim(from_format);
    let to_format = lower_trim(to_format);
    let mut model = "unknown";
    let mut support: Option<&ThinkingSupport> = None;
    if let Some(info) = model_info {
        if !info.id.is_empty() {
            model = &info.id;
        }
        support = info.thinking.as_ref();
    }

    let Some(support) = support else {
        if config.mode != ThinkingMode::None {
            return Err(ThinkingError::with_model(
                ErrorCode::ThinkingNotSupported,
                "thinking not supported for this model",
                model,
            ));
        }
        return Ok(config);
    };

    let to_capability = detect_model_capability(model_info);
    let to_has_level_support =
        to_capability == ModelCapability::LevelOnly || to_capability == ModelCapability::Hybrid;
    let mut model_family_mismatch = false;
    if let Some(info) = model_info {
        let model_type = lower_trim(&info.model_type);
        if !model_type.is_empty()
            && ((!from_format.is_empty() && !is_same_provider_family(&from_format, &model_type))
                || (!to_format.is_empty() && !is_same_provider_family(&to_format, &model_type)))
        {
            model_family_mismatch = true;
        }
    }
    let allow_clamp_unsupported = to_has_level_support
        && (!is_same_provider_family(&from_format, &to_format) || model_family_mismatch);

    let strict_budget = !from_suffix
        && !from_format.is_empty()
        && is_same_provider_family(&from_format, &to_format)
        && !model_family_mismatch;
    let mut budget_derived_from_level = false;

    match detect_model_capability(model_info) {
        ModelCapability::BudgetOnly
            if config.mode == ThinkingMode::Level && config.level != LEVEL_AUTO =>
        {
            let Some(budget) = convert_level_to_budget(&config.level) else {
                return Err(ThinkingError::new(
                    ErrorCode::UnknownLevel,
                    format!("unknown level: {}", config.level),
                ));
            };
            config.mode = ThinkingMode::Budget;
            config.budget = budget;
            config.level = String::new();
            budget_derived_from_level = true;
        }
        ModelCapability::LevelOnly if config.mode == ThinkingMode::Budget => {
            let Some(level) = convert_budget_to_level(config.budget) else {
                return Err(ThinkingError::new(
                    ErrorCode::UnknownLevel,
                    format!(
                        "budget {} cannot be converted to a valid level",
                        config.budget
                    ),
                ));
            };
            config.mode = ThinkingMode::Level;
            config.level = clamp_level(level, model_info);
            config.budget = 0;
        }
        _ => {}
    }

    if config.mode == ThinkingMode::Level && config.level == LEVEL_NONE {
        config.mode = ThinkingMode::None;
        config.budget = 0;
        config.level = String::new();
    }
    if config.mode == ThinkingMode::Level && config.level == LEVEL_AUTO {
        config.mode = ThinkingMode::Auto;
        config.budget = -1;
        config.level = String::new();
    }
    if config.mode == ThinkingMode::Budget && config.budget == 0 {
        config.mode = ThinkingMode::None;
        config.level = String::new();
    }

    if !support.levels.is_empty()
        && config.mode == ThinkingMode::Level
        && !is_level_supported(&config.level, &support.levels)
    {
        if allow_clamp_unsupported {
            config.level = clamp_level(&config.level, model_info);
        }
        if !is_level_supported(&config.level, &support.levels) {
            let valid_levels = normalize_levels(&support.levels);
            let message = format!(
                "level {:?} not supported, valid levels: {}",
                config.level.to_lowercase(),
                valid_levels.join(", ")
            );
            return Err(ThinkingError::new(ErrorCode::LevelNotSupported, message));
        }
    }

    if strict_budget && config.mode == ThinkingMode::Budget && !budget_derived_from_level {
        let (min, max) = (support.min, support.max);
        if (min != 0 || max != 0)
            && (config.budget < min
                || config.budget > max
                || (config.budget == 0 && !support.zero_allowed))
        {
            let message = format!("budget {} out of range [{},{}]", config.budget, min, max);
            return Err(ThinkingError::new(ErrorCode::BudgetOutOfRange, message));
        }
    }

    if config.mode == ThinkingMode::Auto && !support.dynamic_allowed {
        config = convert_auto_to_mid_range(config, support);
        if config.mode == ThinkingMode::Level
            && !support.levels.is_empty()
            && !is_level_supported(&config.level, &support.levels)
        {
            config.level = clamp_level(&config.level, model_info);
        }
    }

    if config.mode == ThinkingMode::None && to_format == "claude" {
        config.budget = 0;
        config.level = String::new();
    } else {
        if matches!(
            config.mode,
            ThinkingMode::Budget | ThinkingMode::Auto | ThinkingMode::None
        ) {
            config.budget = clamp_budget(config.budget, model_info);
        }
        let cannot_disable_level_model =
            !support.zero_allowed && !is_level_supported(LEVEL_NONE, &support.levels);
        if config.mode == ThinkingMode::None
            && !support.levels.is_empty()
            && (config.budget > 0 || cannot_disable_level_model)
        {
            config.level = support.levels[0].clone();
        }
    }

    Ok(config)
}

/// port of convertAutoToMidRange (validate.go); logging-only params dropped.
fn convert_auto_to_mid_range(
    mut config: ThinkingConfig,
    support: &ThinkingSupport,
) -> ThinkingConfig {
    if !support.levels.is_empty() && support.min == 0 && support.max == 0 {
        config.mode = ThinkingMode::Level;
        config.level = LEVEL_MEDIUM.to_string();
        config.budget = 0;
        return config;
    }
    let mid = (support.min + support.max) / 2;
    if mid <= 0 && support.zero_allowed {
        config.mode = ThinkingMode::None;
        config.budget = 0;
    } else if mid <= 0 {
        config.mode = ThinkingMode::Budget;
        config.budget = support.min;
    } else {
        config.mode = ThinkingMode::Budget;
        config.budget = mid;
    }
    config
}

/// port of standardLevelOrder (validate.go)
const STANDARD_LEVEL_ORDER: [&str; 6] = [
    LEVEL_MINIMAL,
    LEVEL_LOW,
    LEVEL_MEDIUM,
    LEVEL_HIGH,
    LEVEL_XHIGH,
    LEVEL_MAX,
];

/// port of clampLevel (validate.go): nearest supported level, ties go lower.
pub fn clamp_level(level: &str, model_info: Option<&ModelInfo>) -> String {
    let supported: &[String] = model_info
        .and_then(|info| info.thinking.as_ref())
        .map(|support| support.levels.as_slice())
        .unwrap_or(&[]);
    if supported.is_empty() || is_level_supported(level, supported) {
        return level.to_string();
    }
    let Some(pos) = level_index(level) else {
        return level.to_string();
    };
    let mut best: Option<usize> = None;
    let mut best_dist = STANDARD_LEVEL_ORDER.len() + 1;
    for s in supported {
        if let Some(idx) = level_index(s.trim()) {
            let dist = pos.abs_diff(idx);
            if dist < best_dist || (dist == best_dist && best.is_some_and(|b| idx < b)) {
                best = Some(idx);
                best_dist = dist;
            }
        }
    }
    match best {
        Some(idx) => STANDARD_LEVEL_ORDER[idx].to_string(),
        None => level.to_string(),
    }
}

/// port of clampBudget (validate.go)
pub fn clamp_budget(value: i64, model_info: Option<&ModelInfo>) -> i64 {
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return value;
    };
    if value == -1 {
        return value;
    }
    let (min, max) = (support.min, support.max);
    if value == 0 && !support.zero_allowed {
        return min;
    }
    if min == 0 && max == 0 {
        return value;
    }
    if value < min {
        if value == 0 && support.zero_allowed {
            return 0;
        }
        return min;
    }
    if value > max {
        return max;
    }
    value
}

/// port of isLevelSupported (validate.go)
fn is_level_supported(level: &str, supported: &[String]) -> bool {
    supported
        .iter()
        .any(|s| level.eq_ignore_ascii_case(s.trim()))
}

/// port of levelIndex (validate.go)
fn level_index(level: &str) -> Option<usize> {
    STANDARD_LEVEL_ORDER
        .iter()
        .position(|l| level.eq_ignore_ascii_case(l))
}

/// port of normalizeLevels (validate.go)
fn normalize_levels(levels: &[String]) -> Vec<String> {
    levels.iter().map(|l| lower_trim(l)).collect()
}

/// port of isBudgetCapableProvider (validate.go)
fn is_budget_capable_provider(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity" | "claude")
}

/// port of isGeminiFamily (validate.go)
fn is_gemini_family(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity")
}

/// port of isOpenAIFamily (validate.go)
fn is_openai_family(provider: &str) -> bool {
    matches!(provider, "openai" | "openai-response" | "codex")
}

/// port of isSameProviderFamily (validate.go)
fn is_same_provider_family(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    (is_gemini_family(from) && is_gemini_family(to))
        || (is_openai_family(from) && is_openai_family(to))
}

// ---------------------------------------------------------------------------
// summary.go (openai / openai-response / codex / claude arms)
// ---------------------------------------------------------------------------

/// port of SummaryMode (summary.go)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SummaryMode {
    #[default]
    Unspecified,
    Disabled,
    Enabled,
}

/// port of SummaryConfig (summary.go)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SummaryConfig {
    pub mode: SummaryMode,
    pub detail: String,
}

impl SummaryConfig {
    fn enabled(detail: &str) -> Self {
        SummaryConfig {
            mode: SummaryMode::Enabled,
            detail: detail.to_string(),
        }
    }
    fn disabled() -> Self {
        SummaryConfig {
            mode: SummaryMode::Disabled,
            detail: String::new(),
        }
    }
}

/// port of ExtractSummaryConfig (summary.go)
pub fn extract_summary_config(body: &Value, format: &str) -> SummaryConfig {
    let normalized = lower_trim(format);
    if !summary_format_supported(&normalized) {
        return SummaryConfig::default();
    }
    match normalized.as_str() {
        "openai" => {
            if let Some(config) = extract_openai_explicit_summary_config(body) {
                return config;
            }
            if let Some(Value::String(effort)) = gj_get(body, "reasoning_effort") {
                let value = lower_trim(effort);
                if value.is_empty() {
                    return SummaryConfig::default();
                }
                if value == "none" {
                    return SummaryConfig::disabled();
                }
                return SummaryConfig::enabled("auto");
            }
        }
        "openai-response" | "codex" => {
            if let Some(config) = responses_summary_config(body, "reasoning.summary") {
                return config;
            }
            if let Some(config) = responses_summary_config(body, "reasoning.generate_summary") {
                return config;
            }
        }
        "claude" => {
            if !claude_thinking_accepts_display(body) {
                return SummaryConfig::default();
            }
            if let Some(config) = claude_summary_config(body, "thinking.display") {
                return config;
            }
        }
        "gemini" => {
            if let Some(config) = first_summary_bool_config(
                body,
                &[
                    "generationConfig.thinkingConfig.includeThoughts",
                    "generationConfig.thinkingConfig.include_thoughts",
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                ],
            ) {
                return config;
            }
        }
        _ => {}
    }
    SummaryConfig::default()
}

/// port of ExtractExplicitSummaryConfig (summary.go)
pub fn extract_explicit_summary_config(body: &Value, format: &str) -> SummaryConfig {
    let normalized = lower_trim(format);
    if normalized != "openai" {
        return extract_summary_config(body, &normalized);
    }
    extract_openai_explicit_summary_config(body).unwrap_or_default()
}

/// port of ExtractTranslatedSummaryConfig (summary.go). `None` is Go's empty body.
pub fn extract_translated_summary_config(
    body: Option<&Value>,
    source_format: &str,
    target_format: &str,
) -> SummaryConfig {
    let Some(body) = body else {
        return SummaryConfig::default();
    };
    let source = lower_trim(source_format);
    let target = lower_trim(target_format);
    if target == "claude" && source == "openai" {
        return extract_explicit_summary_config(body, &source);
    }
    extract_summary_config(body, &source)
}

/// port of ApplyTranslatedSummaryToClaude (summary.go)
pub fn apply_translated_summary_to_claude(
    out: &Value,
    source: &Value,
    source_format: &str,
    model: &str,
) -> Value {
    let config = extract_translated_summary_config(Some(source), source_format, "claude");
    if config.mode == SummaryMode::Unspecified {
        return out.clone();
    }
    apply_summary_config_for_model(out, "claude", model, &config)
}

/// port of ApplySummaryConfig (summary.go)
pub fn apply_summary_config(body: &Value, format: &str, config: &SummaryConfig) -> Value {
    apply_summary_config_for_model(body, format, "", config)
}

/// port of ApplySummaryConfigForModel (summary.go)
pub fn apply_summary_config_for_model(
    body: &Value,
    format: &str,
    model: &str,
    config: &SummaryConfig,
) -> Value {
    apply_summary_config_for_provider(body.clone(), format, model, "", None, config)
}

/// port of applySummaryConfigForProvider (summary.go)
fn apply_summary_config_for_provider(
    mut body: Value,
    format: &str,
    model: &str,
    provider: &str,
    model_info: Option<&ModelInfo>,
    config: &SummaryConfig,
) -> Value {
    let normalized = lower_trim(format);
    if config.mode == SummaryMode::Unspecified || !summary_format_supported(&normalized) {
        return body;
    }
    let enabled = config.mode == SummaryMode::Enabled;
    match normalized.as_str() {
        "openai" => {
            body = apply_openai_chat_summary_config(body, provider, enabled);
        }
        "claude" => {
            if enabled && gj_get(&body, "thinking.type").is_none() {
                body = enable_claude_thinking_for_summary(body, model, model_info);
            }
            if !claude_thinking_accepts_display(&body) {
                return body;
            }
            let value = if enabled { "summarized" } else { "omitted" };
            sj_set(&mut body, "thinking.display", Value::from(value));
        }
        "gemini" => {
            sj_set(
                &mut body,
                "generationConfig.thinkingConfig.includeThoughts",
                Value::Bool(enabled),
            );
            for path in [
                "generationConfig.thinkingConfig.include_thoughts",
                "generation_config.thinking_config.include_thoughts",
                "generation_config.thinking_config.includeThoughts",
            ] {
                sj_delete(&mut body, path);
            }
        }
        "openai-response" | "codex" => {
            if enabled {
                sj_set(
                    &mut body,
                    "reasoning.summary",
                    Value::from(normalized_summary_detail(&config.detail)),
                );
                sj_delete(&mut body, "reasoning.generate_summary");
            } else {
                sj_delete(&mut body, "reasoning.summary");
                sj_delete(&mut body, "reasoning.generate_summary");
                sj_delete_if_empty_object(&mut body, "reasoning");
            }
        }
        _ => {}
    }
    body
}

/// port of summaryFormatSupported (summary.go), restricted to the in-scope
/// formats (antigravity / interactions are not ported).
fn summary_format_supported(format: &str) -> bool {
    matches!(
        format,
        "openai" | "openai-response" | "codex" | "claude" | "gemini"
    )
}

/// port of claudeThinkingAcceptsDisplay (summary.go)
fn claude_thinking_accepts_display(body: &Value) -> bool {
    match lower_trim(&gj_string(gj_get(body, "thinking.type"))).as_str() {
        "adaptive" => true,
        "enabled" => match gj_get(body, "thinking.budget_tokens") {
            Some(budget @ Value::Number(_)) => {
                let value = gj_int(Some(budget));
                value == -1 || value > 0
            }
            _ => true,
        },
        _ => false,
    }
}

/// port of applyOpenAIChatSummaryConfig (summary.go)
fn apply_openai_chat_summary_config(mut body: Value, provider: &str, enabled: bool) -> Value {
    if is_open_router_provider(provider)
        || matches!(gj_get(&body, "reasoning.exclude"), Some(Value::Bool(_)))
    {
        sj_set(&mut body, "reasoning.exclude", Value::Bool(!enabled));
    }
    if matches!(gj_get(&body, "include_reasoning"), Some(Value::Bool(_))) {
        sj_set(&mut body, "include_reasoning", Value::Bool(enabled));
    }
    body
}

/// port of isOpenRouterProvider (summary.go)
fn is_open_router_provider(provider: &str) -> bool {
    let provider = lower_trim(provider);
    if provider == "openrouter" {
        return true;
    }
    provider
        .split(['-', '_', '/', '.', ':'])
        .any(|part| part == "openrouter")
}

/// port of extractOpenAIExplicitSummaryConfig (summary.go)
fn extract_openai_explicit_summary_config(body: &Value) -> Option<SummaryConfig> {
    for path in [
        "extra_body.google.thinking_config.include_thoughts",
        "extra_body.google.thinking_config.includeThoughts",
        "extra_body.google.thinkingConfig.include_thoughts",
        "extra_body.google.thinkingConfig.includeThoughts",
        "extra_body.extra_body.google.thinking_config.include_thoughts",
        "extra_body.extra_body.google.thinking_config.includeThoughts",
        "google.thinking_config.include_thoughts",
        "google.thinking_config.includeThoughts",
        "thinking.includeThoughts",
        "thinking.include_thoughts",
        "reasoning.includeThoughts",
        "reasoning.include_thoughts",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
    ] {
        if let Some(config) = summary_bool_config(body, path) {
            return Some(config);
        }
    }
    for path in ["reasoning.summary", "reasoning.generate_summary"] {
        if let Some(config) = responses_summary_config(body, path) {
            return Some(config);
        }
    }
    if let Some(Value::Bool(exclude)) = gj_get(body, "reasoning.exclude") {
        return Some(if *exclude {
            SummaryConfig::disabled()
        } else {
            SummaryConfig::enabled("auto")
        });
    }
    if let Some(Value::Bool(include)) = gj_get(body, "include_reasoning") {
        return Some(if *include {
            SummaryConfig::enabled("auto")
        } else {
            SummaryConfig::disabled()
        });
    }
    if let Some(Value::Bool(enabled)) = gj_get(body, "reasoning.enabled") {
        return Some(if *enabled {
            SummaryConfig::enabled("auto")
        } else {
            SummaryConfig::disabled()
        });
    }
    None
}

/// port of firstSummaryBoolConfig (summary.go)
fn first_summary_bool_config(body: &Value, paths: &[&str]) -> Option<SummaryConfig> {
    paths
        .iter()
        .find_map(|path| summary_bool_config(body, path))
}

/// port of summaryBoolConfig (summary.go)
fn summary_bool_config(body: &Value, path: &str) -> Option<SummaryConfig> {
    match gj_get(body, path) {
        Some(Value::Bool(true)) => Some(SummaryConfig::enabled("auto")),
        Some(Value::Bool(false)) => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

/// port of responsesSummaryConfig (summary.go)
fn responses_summary_config(body: &Value, path: &str) -> Option<SummaryConfig> {
    match gj_get(body, path)? {
        Value::Null => Some(SummaryConfig::disabled()),
        Value::String(s) => match lower_trim(s).as_str() {
            raw @ ("auto" | "concise" | "detailed") => Some(SummaryConfig::enabled(raw)),
            "none" => Some(SummaryConfig::disabled()),
            _ => None,
        },
        _ => None,
    }
}

/// port of claudeSummaryConfig (summary.go)
fn claude_summary_config(body: &Value, path: &str) -> Option<SummaryConfig> {
    let Some(Value::String(s)) = gj_get(body, path) else {
        return None;
    };
    match lower_trim(s).as_str() {
        "summarized" => Some(SummaryConfig::enabled("auto")),
        "omitted" => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

/// port of stripInferredClaudeSummaryActivation (summary.go)
fn strip_inferred_claude_summary_activation(
    mut body: Value,
    model_info: Option<&ModelInfo>,
) -> Value {
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return body;
    };
    if !support.levels.is_empty() || support.min <= 0 {
        return body;
    }
    if !gj_string(gj_get(&body, "thinking.type"))
        .trim()
        .eq_ignore_ascii_case("adaptive")
    {
        return body;
    }
    for path in [
        "thinking.type",
        "thinking.budget_tokens",
        "thinking.display",
        "output_config.effort",
    ] {
        sj_delete(&mut body, path);
    }
    for path in ["thinking", "output_config"] {
        sj_delete_if_empty_object(&mut body, path);
    }
    body
}

/// port of enableClaudeThinkingForSummary (summary.go)
fn enable_claude_thinking_for_summary(
    mut body: Value,
    model: &str,
    resolved_model_info: Option<&ModelInfo>,
) -> Value {
    let looked_up;
    let model_info = match resolved_model_info {
        Some(info) => Some(info),
        None => {
            let mut base_model = parse_suffix(model).model_name;
            if base_model.is_empty() {
                base_model = parse_suffix(&gj_string(gj_get(&body, "model"))).model_name;
            }
            looked_up = lookup_model_info(&base_model, "claude");
            looked_up.as_ref()
        }
    };
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return body;
    };
    if !support.levels.is_empty() {
        sj_set(&mut body, "thinking.type", Value::from("adaptive"));
        sj_delete(&mut body, "thinking.budget_tokens");
        return body;
    }
    let budget = support.min;
    if budget <= 0 {
        return body;
    }
    if let Some(max_tokens) = gj_get(&body, "max_tokens") {
        if gj_int(Some(max_tokens)) <= budget {
            return body;
        }
    }
    sj_set(&mut body, "thinking.type", Value::from("enabled"));
    sj_set(&mut body, "thinking.budget_tokens", Value::from(budget));
    body
}

/// port of normalizedSummaryDetail (summary.go)
fn normalized_summary_detail(detail: &str) -> &'static str {
    match lower_trim(detail).as_str() {
        "concise" => "concise",
        "detailed" => "detailed",
        _ => "auto",
    }
}

// ---------------------------------------------------------------------------
// provider appliers (provider/{codex,openai,claude,xai}/apply.go)
// ---------------------------------------------------------------------------

/// The registered native appliers that are in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Applier {
    Claude,
    OpenAI,
    /// Codex and xAI (xai.Applier embeds codex.Applier).
    Codex,
    Gemini,
}

/// port of GetProviderApplier + normalizedProviderName (apply.go). Native
/// providers outside scope (antigravity, kimi*) answer `None`, as an
/// unregistered provider does in Go.
fn get_provider_applier(provider: &str) -> Option<Applier> {
    match lower_trim(provider).as_str() {
        "claude" => Some(Applier::Claude),
        "openai" => Some(Applier::OpenAI),
        "codex" | "xai" => Some(Applier::Codex),
        "gemini" => Some(Applier::Gemini),
        _ => None,
    }
}

impl Applier {
    fn apply(
        self,
        body: Value,
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Value, ThinkingError> {
        Ok(match self {
            Applier::Codex => apply_level_effort(body, config, model_info, "reasoning.effort"),
            Applier::OpenAI => apply_level_effort(body, config, model_info, "reasoning_effort"),
            Applier::Claude => apply_claude(body, config, model_info),
            Applier::Gemini => apply_gemini(body, config, model_info),
        })
    }
}

/// port of codex.Applier.Apply (provider/codex/apply.go) and
/// openai.Applier.Apply (provider/openai/apply.go) — identical apart from the
/// field they write (`reasoning.effort` vs `reasoning_effort`).
fn apply_level_effort(
    mut body: Value,
    config: &ThinkingConfig,
    model_info: Option<&ModelInfo>,
    field: &str,
) -> Value {
    if is_user_defined_model(model_info) {
        return apply_compatible_level_effort(body, config, field);
    }
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return body;
    };
    if config.mode != ThinkingMode::Level && config.mode != ThinkingMode::None {
        return body;
    }
    if config.mode == ThinkingMode::Level {
        sj_set(&mut body, field, Value::from(config.level.clone()));
        return body;
    }
    let mut effort = String::new();
    if config.budget == 0 && (support.zero_allowed || has_level(&support.levels, LEVEL_NONE)) {
        effort = LEVEL_NONE.to_string();
    }
    if effort.is_empty() && !config.level.is_empty() {
        effort = config.level.clone();
    }
    if effort.is_empty() {
        if let Some(first) = support.levels.first() {
            effort = first.clone();
        }
    }
    if effort.is_empty() {
        return body;
    }
    sj_set(&mut body, field, Value::from(effort));
    body
}

/// port of applyCompatibleCodex (provider/codex/apply.go) and
/// applyCompatibleOpenAI (provider/openai/apply.go).
fn apply_compatible_level_effort(mut body: Value, config: &ThinkingConfig, field: &str) -> Value {
    let effort = match config.mode {
        ThinkingMode::Level => {
            if config.level.is_empty() {
                return body;
            }
            config.level.clone()
        }
        ThinkingMode::None => {
            if config.level.is_empty() {
                LEVEL_NONE.to_string()
            } else {
                config.level.clone()
            }
        }
        ThinkingMode::Auto => LEVEL_AUTO.to_string(),
        ThinkingMode::Budget => match convert_budget_to_level(config.budget) {
            Some(level) => level.to_string(),
            None => return body,
        },
    };
    sj_set(&mut body, field, Value::from(effort));
    body
}

/// The disabled-thinking block shared by claude.Applier.Apply and
/// applyCompatibleClaude (provider/claude/apply.go), ModeNone arm.
fn claude_set_disabled_with_display_cleanup(mut body: Value) -> Value {
    sj_set(&mut body, "thinking.type", Value::from("disabled"));
    sj_delete(&mut body, "thinking.budget_tokens");
    sj_delete(&mut body, "thinking.display");
    sj_delete(&mut body, "output_config.effort");
    sj_delete_if_empty_object(&mut body, "output_config");
    body
}

/// port of claude.Applier.Apply (provider/claude/apply.go)
fn apply_claude(mut body: Value, config: &ThinkingConfig, model_info: Option<&ModelInfo>) -> Value {
    if is_user_defined_model(model_info) {
        return apply_compatible_claude(body, config);
    }
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return body;
    };
    let supports_adaptive = !support.levels.is_empty();
    let mut config = config.clone();

    match config.mode {
        ThinkingMode::None => return claude_set_disabled_with_display_cleanup(body),
        ThinkingMode::Level => {
            if supports_adaptive && !config.level.is_empty() {
                sj_set(&mut body, "thinking.type", Value::from("adaptive"));
                sj_delete(&mut body, "thinking.budget_tokens");
                sj_set(
                    &mut body,
                    "output_config.effort",
                    Value::from(config.level.clone()),
                );
                return body;
            }
            let Some(budget) = convert_level_to_budget(&config.level) else {
                return body;
            };
            config.mode = ThinkingMode::Budget;
            config.budget = budget;
            config.level = String::new();
            // fallthrough to ModeBudget
        }
        ThinkingMode::Budget => {}
        ThinkingMode::Auto => {
            if supports_adaptive {
                sj_set(&mut body, "thinking.type", Value::from("adaptive"));
                sj_delete(&mut body, "thinking.budget_tokens");
                sj_delete(&mut body, "output_config.effort");
                sj_delete_if_empty_object(&mut body, "output_config");
                return body;
            }
            sj_set(&mut body, "thinking.type", Value::from("enabled"));
            sj_delete(&mut body, "thinking.budget_tokens");
            sj_delete(&mut body, "output_config.effort");
            sj_delete_if_empty_object(&mut body, "output_config");
            return body;
        }
    }

    // ModeBudget
    if config.budget == 0 {
        sj_set(&mut body, "thinking.type", Value::from("disabled"));
        sj_delete(&mut body, "thinking.budget_tokens");
        sj_delete(&mut body, "output_config.effort");
        sj_delete_if_empty_object(&mut body, "output_config");
        return body;
    }
    sj_set(&mut body, "thinking.type", Value::from("enabled"));
    sj_set(
        &mut body,
        "thinking.budget_tokens",
        Value::from(config.budget),
    );
    sj_delete(&mut body, "output_config.effort");
    sj_delete_if_empty_object(&mut body, "output_config");
    normalize_claude_budget(body, config.budget, model_info)
}

/// port of claude.Applier.normalizeClaudeBudget (provider/claude/apply.go)
fn normalize_claude_budget(
    mut body: Value,
    budget_tokens: i64,
    model_info: Option<&ModelInfo>,
) -> Value {
    if budget_tokens <= 0 {
        return body;
    }
    let (effective_max, set_default_max) = effective_max_tokens(&body, model_info);
    if set_default_max && effective_max > 0 {
        sj_set(&mut body, "max_tokens", Value::from(effective_max));
    }
    let mut adjusted_budget = budget_tokens;
    if effective_max > 0 && adjusted_budget >= effective_max {
        adjusted_budget = effective_max - 1;
    }
    let min_budget = model_info
        .and_then(|info| info.thinking.as_ref())
        .map_or(0, |support| support.min);
    if min_budget > 0 && adjusted_budget > 0 && adjusted_budget < min_budget {
        return body;
    }
    if adjusted_budget != budget_tokens {
        sj_set(
            &mut body,
            "thinking.budget_tokens",
            Value::from(adjusted_budget),
        );
    }
    body
}

/// port of claude.Applier.effectiveMaxTokens (provider/claude/apply.go)
fn effective_max_tokens(body: &Value, model_info: Option<&ModelInfo>) -> (i64, bool) {
    if let Some(max_tokens) = gj_get(body, "max_tokens") {
        let value = gj_int(Some(max_tokens));
        if value > 0 {
            return (value, false);
        }
    }
    if let Some(info) = model_info {
        if info.max_completion_tokens > 0 {
            return (info.max_completion_tokens, true);
        }
    }
    (0, false)
}

/// port of applyCompatibleClaude (provider/claude/apply.go)
fn apply_compatible_claude(mut body: Value, config: &ThinkingConfig) -> Value {
    match config.mode {
        ThinkingMode::None => claude_set_disabled_with_display_cleanup(body),
        ThinkingMode::Auto => {
            sj_set(&mut body, "thinking.type", Value::from("enabled"));
            sj_delete(&mut body, "thinking.budget_tokens");
            sj_delete(&mut body, "output_config.effort");
            sj_delete_if_empty_object(&mut body, "output_config");
            body
        }
        ThinkingMode::Level => {
            if config.level.is_empty() {
                return body;
            }
            sj_set(&mut body, "thinking.type", Value::from("adaptive"));
            sj_delete(&mut body, "thinking.budget_tokens");
            sj_set(
                &mut body,
                "output_config.effort",
                Value::from(config.level.clone()),
            );
            body
        }
        ThinkingMode::Budget => {
            // Anthropic requires `budget_tokens < max_tokens`: a suffix like
            // `claude-x(8192)` on a request with a small `max_tokens`
            // (Claude Code's title and summary calls) is capped under it,
            // else every such request is a 400.
            let max_tokens = body.get("max_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            let budget = if max_tokens > 1 && config.budget >= max_tokens {
                max_tokens - 1
            } else {
                config.budget
            };
            sj_set(&mut body, "thinking.type", Value::from("enabled"));
            sj_set(&mut body, "thinking.budget_tokens", Value::from(budget));
            sj_delete(&mut body, "output_config.effort");
            sj_delete_if_empty_object(&mut body, "output_config");
            body
        }
    }
}

/// port of gemini.Applier.Apply (provider/gemini/apply.go). Go's empty /
/// invalid-body guard is unreachable with a parsed `Value`.
fn apply_gemini(body: Value, config: &ThinkingConfig, model_info: Option<&ModelInfo>) -> Value {
    if is_user_defined_model(model_info) {
        return apply_compatible_gemini(body, config);
    }
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return body;
    };
    match config.mode {
        ThinkingMode::Level => gemini_apply_level_format(body, config),
        ThinkingMode::None => {
            if !support.levels.is_empty() {
                gemini_apply_level_format(body, config)
            } else {
                gemini_apply_budget_format(body, config)
            }
        }
        ThinkingMode::Budget | ThinkingMode::Auto => gemini_apply_budget_format(body, config),
    }
}

/// port of gemini.Applier.applyCompatible (provider/gemini/apply.go)
fn apply_compatible_gemini(body: Value, config: &ThinkingConfig) -> Value {
    if config.mode == ThinkingMode::Auto {
        return gemini_apply_budget_format(body, config);
    }
    if config.mode == ThinkingMode::Level
        || (config.mode == ThinkingMode::None && !config.level.is_empty())
    {
        return gemini_apply_level_format(body, config);
    }
    gemini_apply_budget_format(body, config)
}

/// port of gemini.Applier.applyLevelFormat (provider/gemini/apply.go)
fn gemini_apply_level_format(body: Value, config: &ThinkingConfig) -> Value {
    let mut result = body.clone();
    for path in [
        "generationConfig.thinkingConfig.thinkingBudget",
        "generationConfig.thinkingConfig.thinking_budget",
        "generationConfig.thinkingConfig.thinking_level",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
    ] {
        sj_delete(&mut result, path);
    }

    if config.mode == ThinkingMode::None {
        if config.budget == 0 && config.level.is_empty() {
            sj_delete(&mut result, "generationConfig.thinkingConfig");
            return result;
        }
        if !config.level.is_empty() {
            sj_set(
                &mut result,
                "generationConfig.thinkingConfig.thinkingLevel",
                Value::from(config.level.clone()),
            );
        }
        return apply_gemini_include_thoughts(result, &body);
    }

    if config.mode != ThinkingMode::Level {
        return body;
    }
    sj_set(
        &mut result,
        "generationConfig.thinkingConfig.thinkingLevel",
        Value::from(config.level.clone()),
    );
    apply_gemini_include_thoughts(result, &body)
}

/// port of gemini.Applier.applyBudgetFormat (provider/gemini/apply.go)
fn gemini_apply_budget_format(body: Value, config: &ThinkingConfig) -> Value {
    let mut result = body.clone();
    for path in [
        "generationConfig.thinkingConfig.thinkingLevel",
        "generationConfig.thinkingConfig.thinking_level",
        "generationConfig.thinkingConfig.thinking_budget",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
    ] {
        sj_delete(&mut result, path);
    }
    sj_set(
        &mut result,
        "generationConfig.thinkingConfig.thinkingBudget",
        Value::from(config.budget),
    );
    apply_gemini_include_thoughts(result, &body)
}

/// port of applyGeminiIncludeThoughts (provider/gemini/apply.go)
fn apply_gemini_include_thoughts(mut result: Value, original: &Value) -> Value {
    for path in [
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
    ] {
        if let Some(Value::Bool(value)) = gj_get(original, path) {
            sj_set(
                &mut result,
                "generationConfig.thinkingConfig.includeThoughts",
                Value::Bool(*value),
            );
            return result;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// apply.go
// ---------------------------------------------------------------------------

/// port of IsUserDefinedModel (apply.go): an unknown (`None`) model counts as
/// user-defined, so its thinking config is applied without validation.
pub fn is_user_defined_model(model_info: Option<&ModelInfo>) -> bool {
    model_info.is_none_or(|info| info.user_defined)
}

/// port of ApplyThinking (apply.go): no source body, summary from the target body.
pub fn apply_thinking_body_only(
    body: &Value,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
) -> Result<Value, ThinkingError> {
    let summary_config = extract_summary_config(body, to_format);
    apply_thinking_impl(
        body.clone(),
        None,
        model,
        from_format,
        to_format,
        provider_key,
        None,
        false,
        &summary_config,
        false,
    )
}

/// port of ApplyThinkingWithSummary (apply.go)
pub fn apply_thinking_with_summary(
    body: &Value,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    summary_config: &SummaryConfig,
) -> Result<Value, ThinkingError> {
    apply_thinking_impl(
        body.clone(),
        None,
        model,
        from_format,
        to_format,
        provider_key,
        None,
        false,
        summary_config,
        false,
    )
}

/// port of ApplyThinkingWithSourceAndSummary (apply.go)
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_source_and_summary(
    body: &Value,
    source_body: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    summary_config: &SummaryConfig,
    normalized_updates_changed: bool,
) -> Result<Value, ThinkingError> {
    apply_thinking_impl(
        body.clone(),
        source_body,
        model,
        from_format,
        to_format,
        provider_key,
        None,
        false,
        summary_config,
        normalized_updates_changed,
    )
}

/// port of ApplyThinkingWithModelInfo (apply.go)
pub fn apply_thinking_with_model_info(
    body: &Value,
    source_body: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    model_info: Option<&ModelInfo>,
) -> Result<Value, ThinkingError> {
    let summary_config = match source_body {
        Some(source) => extract_summary_config(source, from_format),
        None => extract_summary_config(body, to_format),
    };
    apply_thinking_with_model_info_and_summary(
        body,
        source_body,
        model,
        from_format,
        to_format,
        provider_key,
        model_info,
        &summary_config,
        false,
    )
}

/// port of ApplyThinkingWithModelInfoAndSummary (apply.go)
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_model_info_and_summary(
    body: &Value,
    source_body: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    model_info: Option<&ModelInfo>,
    summary_config: &SummaryConfig,
    normalized_updates_changed: bool,
) -> Result<Value, ThinkingError> {
    apply_thinking_impl(
        body.clone(),
        source_body,
        model,
        from_format,
        to_format,
        provider_key,
        model_info,
        true,
        summary_config,
        normalized_updates_changed,
    )
}

/// port of applyThinking (apply.go)
#[allow(clippy::too_many_arguments)]
fn apply_thinking_impl(
    mut body: Value,
    source_body: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    resolved_model_info: Option<&ModelInfo>,
    model_info_resolved: bool,
    summary_config: &SummaryConfig,
    updates_changed: bool,
) -> Result<Value, ThinkingError> {
    let mut provider_format = lower_trim(to_format);
    if provider_format == "openai-response" {
        provider_format = "codex".to_string();
    }
    let mut provider_key = lower_trim(provider_key);
    if provider_key.is_empty() {
        provider_key = provider_format.clone();
    }
    let mut from_format = lower_trim(from_format);
    if from_format.is_empty() {
        from_format = provider_format.clone();
    }

    // 1. Parse suffix and get modelInfo
    let suffix_result = parse_suffix(model);
    let base_model = suffix_result.model_name.clone();
    let looked_up;
    let model_info = if model_info_resolved {
        resolved_model_info
    } else {
        looked_up = lookup_model_info(&base_model, &provider_key);
        looked_up.as_ref()
    };

    // Resolve source intent before stripping unsupported target input items.
    let mut source_config = ThinkingConfig::default();
    if is_responses_format(&from_format) {
        let source_request = match source_body {
            Some(source) if !updates_changed => source,
            _ => &body,
        };
        if !updates_changed || provider_format == "codex" || provider_format == "xai" {
            source_config = extract_codex_usage_config(source_request);
        }
    }
    let response_target = provider_format == "codex" || provider_format == "xai";
    let supports_updates = model_info.is_some_and(|info| info.support_configuration_update);
    if response_target && !supports_updates {
        body = strip_configuration_updates(body);
    }
    let native_responses = response_target && is_responses_format(&from_format) && supports_updates;

    // 2. Route check
    let Some(applier) = get_provider_applier(&provider_format) else {
        return Ok(body);
    };

    // 3. Model capability check. (Go's "malformed target" guard needs invalid
    // JSON bytes and cannot occur with a parsed Value.)
    if is_user_defined_model(model_info) {
        if native_responses && !suffix_result.has_suffix() {
            return Ok(body);
        }
        return apply_user_defined_model(
            body,
            model_info,
            &from_format,
            &provider_format,
            &provider_key,
            &suffix_result,
            source_config,
            native_responses,
            summary_config,
        );
    }
    if native_responses && !suffix_result.has_suffix() {
        return Ok(body);
    }
    let Some(info) = model_info else {
        return Ok(body);
    };
    if info.thinking.is_none() {
        let config = extract_thinking_config(&body, &provider_format);
        if has_thinking_config(&config) || summary_config.mode != SummaryMode::Unspecified {
            if response_target {
                return Ok(strip_responses_effort(body));
            }
            return Ok(strip_thinking_config(&body, &provider_format));
        }
        return Ok(body);
    }

    // 4. Get config: suffix priority over body
    let mut config;
    if suffix_result.has_suffix() {
        config = parse_suffix_to_config(suffix_result.raw_suffix());
    } else {
        config = source_config;
        if !has_thinking_config(&config) && !updates_changed && model_info_resolved {
            if let Some(source) = source_body {
                config = extract_source_thinking_config(source, &from_format);
            }
        }
        if !has_thinking_config(&config) {
            config = extract_thinking_config(&body, &provider_format);
        }
    }

    if !has_thinking_config(&config) {
        if native_responses {
            return Ok(body);
        }
        if model_info_resolved
            && provider_format == "claude"
            && from_format != provider_format
            && source_body.is_some_and(|source| {
                extract_summary_config(source, &from_format).mode == SummaryMode::Enabled
            })
        {
            body = strip_inferred_claude_summary_activation(body, model_info);
        }
        return Ok(apply_summary_config_for_provider(
            body,
            &provider_format,
            &base_model,
            &provider_key,
            model_info,
            summary_config,
        ));
    }
    if model_info_resolved
        && config.mode == ThinkingMode::Level
        && should_map_configured_high_intent(&from_format, &provider_format, Some(info))
    {
        config.level = map_configured_high_intent(&config.level, Some(info));
    }

    // 5. Validate and normalize configuration
    let validated = validate_config(
        config,
        model_info,
        &from_format,
        &provider_format,
        suffix_result.has_suffix(),
    )?;

    // 6. Apply, then restore the summary intent.
    let applied = applier.apply(body, &validated, model_info)?;
    if thinking_is_fully_disabled(&validated) || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary_config_for_provider(
        applied,
        &provider_format,
        &base_model,
        &provider_key,
        model_info,
        summary_config,
    ))
}

/// port of thinkingIsFullyDisabled (apply.go)
fn thinking_is_fully_disabled(config: &ThinkingConfig) -> bool {
    config.mode == ThinkingMode::None && config.budget == 0 && config.level.is_empty()
}

/// port of shouldMapConfiguredHighIntent (apply.go)
fn should_map_configured_high_intent(
    from_format: &str,
    to_format: &str,
    model_info: Option<&ModelInfo>,
) -> bool {
    let from_format = lower_trim(from_format);
    let to_format = lower_trim(to_format);
    if from_format != to_format {
        return true;
    }
    let Some(info) = model_info else {
        return false;
    };
    let model_type = lower_trim(&info.model_type);
    !model_type.is_empty() && !is_same_provider_family(&to_format, &model_type)
}

/// port of mapConfiguredHighIntent (apply.go)
fn map_configured_high_intent(level: &str, model_info: Option<&ModelInfo>) -> String {
    let Some(support) = model_info.and_then(|info| info.thinking.as_ref()) else {
        return level.to_string();
    };
    if support.levels.is_empty() {
        return level.to_string();
    }
    let level = lower_trim(level);
    let candidates: [&str; 3] = match level.as_str() {
        LEVEL_XHIGH => [LEVEL_XHIGH, LEVEL_MAX, LEVEL_HIGH],
        LEVEL_MAX => [LEVEL_MAX, LEVEL_XHIGH, LEVEL_HIGH],
        _ => return level,
    };
    for candidate in candidates {
        if is_level_supported(candidate, &support.levels) {
            return candidate.to_string();
        }
    }
    level
}

/// port of extractSourceThinkingConfig (apply.go)
fn extract_source_thinking_config(body: &Value, provider: &str) -> ThinkingConfig {
    let provider = lower_trim(provider);
    if provider == "openai-response" {
        return extract_codex_config(body);
    }
    extract_thinking_config(body, &provider)
}

/// port of parseSuffixToConfig (apply.go); logging-only params dropped.
pub fn parse_suffix_to_config(raw_suffix: &str) -> ThinkingConfig {
    if let Some(mode) = parse_special_suffix(raw_suffix) {
        match mode {
            ThinkingMode::None => return ThinkingConfig::none(),
            ThinkingMode::Auto => return ThinkingConfig::auto(),
            _ => {}
        }
    }
    if let Some(level) = parse_level_suffix(raw_suffix) {
        return ThinkingConfig::with_level(level);
    }
    if let Some(budget) = parse_numeric_suffix(raw_suffix) {
        if budget == 0 {
            return ThinkingConfig::none();
        }
        return ThinkingConfig::with_budget(budget);
    }
    ThinkingConfig::default()
}

/// port of applyUserDefinedModel (apply.go) — the path every model takes in
/// Termory, since there is no registry: no validation, no clamping.
#[allow(clippy::too_many_arguments)]
fn apply_user_defined_model(
    body: Value,
    model_info: Option<&ModelInfo>,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    suffix_result: &ParsedModel,
    source_config: ThinkingConfig,
    native_responses: bool,
    summary_config: &SummaryConfig,
) -> Result<Value, ThinkingError> {
    let model_id = match model_info {
        Some(info) => info.id.clone(),
        None => suffix_result.model_name.clone(),
    };

    let mut config;
    if suffix_result.has_suffix() {
        config = parse_suffix_to_config(suffix_result.raw_suffix());
    } else {
        config = source_config;
        if !has_thinking_config(&config) {
            config = extract_thinking_config(&body, from_format);
        }
        if !has_thinking_config(&config) && from_format != to_format {
            config = extract_thinking_config(&body, to_format);
        }
    }

    if !has_thinking_config(&config) {
        return Ok(apply_summary_config_for_provider(
            body,
            to_format,
            &model_id,
            provider_key,
            model_info,
            summary_config,
        ));
    }

    let Some(applier) = get_provider_applier(to_format) else {
        return Ok(body);
    };

    let config = normalize_user_defined_config(config, to_format);
    let applied = applier.apply(body, &config, model_info)?;
    if thinking_is_fully_disabled(&config) || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary_config_for_provider(
        applied,
        to_format,
        &model_id,
        provider_key,
        model_info,
        summary_config,
    ))
}

/// port of normalizeUserDefinedConfig (apply.go); the unused fromFormat param dropped.
fn normalize_user_defined_config(mut config: ThinkingConfig, to_format: &str) -> ThinkingConfig {
    if config.mode != ThinkingMode::Level {
        return config;
    }
    if to_format == "claude" {
        return config;
    }
    if !is_budget_capable_provider(to_format) {
        return config;
    }
    let Some(budget) = convert_level_to_budget(&config.level) else {
        return config;
    };
    config.mode = ThinkingMode::Budget;
    config.budget = budget;
    config.level = String::new();
    config
}

/// port of extractThinkingConfig (apply.go); antigravity/interactions/kimi
/// extractors are out of scope and answer an empty config.
pub fn extract_thinking_config(body: &Value, provider: &str) -> ThinkingConfig {
    match provider {
        "claude" => extract_claude_config(body),
        "gemini" => extract_gemini_config(body, provider),
        "openai" => extract_openai_config(body),
        "codex" | "xai" => extract_codex_config(body),
        _ => ThinkingConfig::default(),
    }
}

/// port of hasThinkingConfig (apply.go)
pub fn has_thinking_config(config: &ThinkingConfig) -> bool {
    config.mode != ThinkingMode::Budget || config.budget != 0 || !config.level.is_empty()
}

/// port of ExtractReasoningEffort (apply.go): the source request's effort as a
/// canonical label, for usage logging.
pub fn extract_reasoning_effort(body: &Value, provider: &str, model: &str) -> String {
    let provider = lower_trim(provider);
    if is_responses_format(&provider) {
        let effort = reasoning_effort_from_config(&extract_configuration_update_config(body));
        if !effort.is_empty() {
            return effort;
        }
    }
    let effort = reasoning_effort_from_suffix(&parse_suffix(model));
    if !effort.is_empty() {
        return effort;
    }
    let mut config = extract_thinking_config_for_usage(body, &provider);
    if !has_thinking_config(&config) && (provider == "openai-response" || provider == "openai") {
        config = extract_codex_usage_config(body);
    }
    reasoning_effort_from_config(&config)
}

/// port of ExtractTranslatedReasoningEffort (apply.go)
pub fn extract_translated_reasoning_effort(body: &Value, provider: &str) -> String {
    let provider = lower_trim(provider);
    let mut config = extract_thinking_config_for_usage(body, &provider);
    if !has_thinking_config(&config) && (provider == "openai" || provider == "openai-response") {
        config = extract_codex_usage_config(body);
        if !has_thinking_config(&config) {
            config = extract_openai_config(body);
        }
    }
    reasoning_effort_from_config(&config)
}

/// port of extractThinkingConfigForUsage (apply.go)
fn extract_thinking_config_for_usage(body: &Value, provider: &str) -> ThinkingConfig {
    match lower_trim(provider).as_str() {
        "codex" | "xai" | "openai-response" => extract_codex_usage_config(body),
        _ => extract_thinking_config(body, provider),
    }
}

/// port of reasoningEffortFromSuffix (apply.go)
fn reasoning_effort_from_suffix(suffix: &ParsedModel) -> String {
    match &suffix.suffix {
        None => String::new(),
        Some(raw) => reasoning_effort_from_config(&parse_suffix_to_config(raw)),
    }
}

/// port of reasoningEffortFromConfig (apply.go)
fn reasoning_effort_from_config(config: &ThinkingConfig) -> String {
    if !has_thinking_config(config) {
        return String::new();
    }
    match config.mode {
        ThinkingMode::None => LEVEL_NONE.to_string(),
        ThinkingMode::Auto => LEVEL_AUTO.to_string(),
        ThinkingMode::Level => lower_trim(&config.level),
        ThinkingMode::Budget => convert_budget_to_level(config.budget)
            .unwrap_or("")
            .to_string(),
    }
}

/// port of extractClaudeConfig (apply.go)
fn extract_claude_config(body: &Value) -> ThinkingConfig {
    let thinking_type = gj_string(gj_get(body, "thinking.type"));
    if thinking_type == "disabled" {
        return ThinkingConfig::none();
    }
    if thinking_type == "adaptive" || thinking_type == "auto" {
        if let Some(Value::String(effort)) = gj_get(body, "output_config.effort") {
            let value = lower_trim(effort);
            return match value.as_str() {
                "" => ThinkingConfig::default(),
                "none" => ThinkingConfig::none(),
                "auto" => ThinkingConfig::auto(),
                _ => ThinkingConfig::with_level(value),
            };
        }
        return ThinkingConfig::default();
    }
    if let Some(budget) = gj_get(body, "thinking.budget_tokens") {
        return match gj_int(Some(budget)) {
            0 => ThinkingConfig::none(),
            -1 => ThinkingConfig::auto(),
            value => ThinkingConfig::with_budget(value),
        };
    }
    if thinking_type == "enabled" {
        return ThinkingConfig::auto();
    }
    ThinkingConfig::default()
}

/// port of extractOpenAIConfig (apply.go)
fn extract_openai_config(body: &Value) -> ThinkingConfig {
    if let Some(effort) = gj_get(body, "reasoning_effort") {
        let value = gj_string(Some(effort));
        if value == "none" {
            return ThinkingConfig::none();
        }
        return ThinkingConfig::with_level(value);
    }
    ThinkingConfig::default()
}

/// port of extractGeminiConfig (apply.go). The antigravity prefix is kept for
/// parity; only "gemini" is routed here.
fn extract_gemini_config(body: &Value, provider: &str) -> ThinkingConfig {
    let prefix = if provider == "antigravity" {
        "request.generationConfig.thinkingConfig"
    } else {
        "generationConfig.thinkingConfig"
    };
    let level = gj_get(body, &format!("{prefix}.thinkingLevel"))
        .or_else(|| gj_get(body, &format!("{prefix}.thinking_level")));
    if let Some(level) = level {
        let value = gj_string(Some(level));
        return match value.as_str() {
            "none" => ThinkingConfig::none(),
            "auto" => ThinkingConfig::auto(),
            _ => ThinkingConfig::with_level(value),
        };
    }
    let budget = gj_get(body, &format!("{prefix}.thinkingBudget"))
        .or_else(|| gj_get(body, &format!("{prefix}.thinking_budget")));
    if let Some(budget) = budget {
        return match gj_int(Some(budget)) {
            0 => ThinkingConfig::none(),
            -1 => ThinkingConfig::auto(),
            value => ThinkingConfig::with_budget(value),
        };
    }
    ThinkingConfig::default()
}

/// port of extractCodexConfig (apply.go)
fn extract_codex_config(body: &Value) -> ThinkingConfig {
    if let Some(effort) = gj_get(body, "reasoning.effort") {
        let value = gj_string(Some(effort));
        if value == "none" {
            return ThinkingConfig::none();
        }
        return ThinkingConfig::with_level(value);
    }
    ThinkingConfig::default()
}

/// port of extractCodexUsageConfig (apply.go)
fn extract_codex_usage_config(body: &Value) -> ThinkingConfig {
    let config = extract_configuration_update_config(body);
    if has_thinking_config(&config) {
        return config;
    }
    extract_codex_config(body)
}

// ---------------------------------------------------------------------------
// executor wrapper (runtime/executor/helps/{thinking,model_capabilities}.go)
// ---------------------------------------------------------------------------

/// Whether the auth manager bound authoritative model capabilities to this
/// attempt — Go's `cliproxyauth.ResolvedModelInfo(req)` `(info, ok)`.
#[derive(Clone, Copy, Debug)]
pub enum ModelBinding<'a> {
    /// `ok == false`: Go falls back to the registry, which Termory does not
    /// have, so the model is unknown and its config is applied unvalidated.
    Unbound,
    /// `ok == true`; `None` is a bound-but-nil model (still unvalidated).
    Bound(Option<&'a ModelInfo>),
}

/// The router's entry point. `model` may carry a suffix; `body` is already in
/// `to_format`; `source_body` is the client's untranslated request. Equivalent
/// to Go's executor `helps.ApplyRequestThinking` with `req.Payload ==
/// opts.OriginalRequest == source_body` and no bound model — i.e. every model
/// is "unknown" and takes `applyUserDefinedModel` (see the module docs).
/// Formats: "claude", "openai", "openai-response" / "codex", "xai", "gemini".
pub fn apply_thinking(
    body: &Value,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    source_body: Option<&Value>,
) -> Result<Value, ThinkingError> {
    apply_request_thinking(
        body,
        source_body,
        source_body,
        model,
        from_format,
        to_format,
        provider_key,
        ModelBinding::Unbound,
        false,
    )
}

/// As [`apply_thinking`], with caller-supplied model capabilities: Go's bound
/// path (`ApplyThinkingWithModelInfoAndSummary`), which validates and clamps.
pub fn apply_thinking_with_model(
    body: &Value,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
    source_body: Option<&Value>,
    model_info: Option<&ModelInfo>,
) -> Result<Value, ThinkingError> {
    apply_request_thinking(
        body,
        source_body,
        source_body,
        model,
        from_format,
        to_format,
        provider_key,
        ModelBinding::Bound(model_info),
        false,
    )
}

/// port of ApplyRequestThinking (helps/model_capabilities.go).
/// `current_source` is Go's `req.Payload`, `original_source` its
/// `opts.OriginalRequest`; `None` is an empty payload.
#[allow(clippy::too_many_arguments)]
pub fn apply_request_thinking(
    body: &Value,
    current_source: Option<&Value>,
    original_source: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider: &str,
    binding: ModelBinding<'_>,
    normalized_updates_changed: bool,
) -> Result<Value, ThinkingError> {
    let original = original_source.or(current_source);
    let source = current_source.or(original_source);
    let summary_config = translated_request_summary_config(
        body,
        current_source,
        original,
        model,
        from_format,
        to_format,
    );
    match binding {
        ModelBinding::Bound(model_info) => apply_thinking_with_model_info_and_summary(
            body,
            source,
            model,
            from_format,
            to_format,
            provider,
            model_info,
            &summary_config,
            normalized_updates_changed,
        ),
        ModelBinding::Unbound => apply_thinking_with_source_and_summary(
            body,
            source,
            model,
            from_format,
            to_format,
            provider,
            &summary_config,
            normalized_updates_changed,
        ),
    }
}

/// port of ApplyThinkingWithSourcePayload (helps/thinking.go)
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_source_payload(
    body: &Value,
    current_source: Option<&Value>,
    original_source: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
) -> Result<Value, ThinkingError> {
    let summary = translated_request_summary_config(
        body,
        current_source,
        original_source,
        model,
        from_format,
        to_format,
    );
    apply_thinking_with_summary(body, model, from_format, to_format, provider_key, &summary)
}

/// port of translatedRequestSummaryConfig (helps/thinking.go)
fn translated_request_summary_config(
    body: &Value,
    current_source: Option<&Value>,
    original_source: Option<&Value>,
    model: &str,
    from_format: &str,
    to_format: &str,
) -> SummaryConfig {
    let from_format = lower_trim(from_format);
    let to_format = lower_trim(to_format);

    let target_summary = if from_format == to_format {
        extract_summary_config(body, &to_format)
    } else {
        extract_explicit_summary_config(body, &to_format)
    };
    if target_summary.mode != SummaryMode::Unspecified {
        return target_summary;
    }

    let current_summary =
        extract_translated_summary_config(current_source, &from_format, &to_format);
    let original_summary =
        extract_translated_summary_config(original_source, &from_format, &to_format);
    if current_summary.mode == SummaryMode::Unspecified {
        return original_summary;
    }

    if !has_request_transformer(&from_format, &to_format) {
        return SummaryConfig::default();
    }

    let candidate = apply_summary_config_for_model(body, &to_format, model, &current_summary);
    if extract_explicit_summary_config(&candidate, &to_format).mode != SummaryMode::Unspecified {
        return SummaryConfig::default();
    }
    current_summary
}

/// sdktranslator.HasRequestTransformer (sdk/translator/registry.go) over the
/// pairs `internal/translator/**/init.go` registers (upstream `ed980be`).
fn has_request_transformer(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        (
            "claude",
            "antigravity" | "codex" | "gemini" | "interactions" | "openai"
        ) | (
            "gemini",
            "antigravity" | "claude" | "codex" | "gemini" | "interactions" | "openai"
        ) | (
            "interactions",
            "antigravity"
                | "claude"
                | "codex"
                | "gemini"
                | "interactions"
                | "openai"
                | "openai-response"
        ) | (
            "openai",
            "antigravity" | "claude" | "codex" | "gemini" | "interactions" | "openai"
        ) | (
            "openai-response",
            "antigravity" | "claude" | "codex" | "gemini" | "interactions" | "openai"
        )
    )
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).expect("test JSON")
    }

    fn s(body: &Value, path: &str) -> String {
        gj_string(gj_get(body, path))
    }

    fn levels(list: &[&str]) -> Vec<String> {
        list.iter().map(|l| l.to_string()).collect()
    }

    fn info(id: &str, model_type: &str, support: Option<ThinkingSupport>) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            model_type: model_type.to_string(),
            thinking: support,
            ..ModelInfo::default()
        }
    }

    fn level_support(list: &[&str]) -> Option<ThinkingSupport> {
        Some(ThinkingSupport {
            levels: levels(list),
            ..ThinkingSupport::default()
        })
    }

    // --- suffix.go doc examples -------------------------------------------

    #[test]
    fn parse_suffix_examples() {
        assert_eq!(
            parse_suffix("claude-sonnet-4-5(16384)"),
            ParsedModel {
                model_name: "claude-sonnet-4-5".into(),
                suffix: Some("16384".into())
            }
        );
        assert_eq!(
            parse_suffix("gpt-5.2(high)"),
            ParsedModel {
                model_name: "gpt-5.2".into(),
                suffix: Some("high".into())
            }
        );
        assert_eq!(
            parse_suffix("gemini-2.5-pro"),
            ParsedModel {
                model_name: "gemini-2.5-pro".into(),
                suffix: None
            }
        );
        assert_eq!(parse_suffix("model(abc").suffix, None);
        assert_eq!(parse_numeric_suffix("08192"), Some(8192));
        assert_eq!(parse_numeric_suffix("0"), Some(0));
        assert_eq!(parse_numeric_suffix("-1"), None);
        assert_eq!(parse_numeric_suffix("high"), None);
        assert_eq!(parse_numeric_suffix("9223372036854775808"), None);
        assert_eq!(parse_special_suffix("NONE"), Some(ThinkingMode::None));
        assert_eq!(parse_special_suffix("-1"), Some(ThinkingMode::Auto));
        assert_eq!(parse_level_suffix("HIGH"), Some(LEVEL_HIGH));
        assert_eq!(parse_level_suffix("none"), None);
        assert_eq!(parse_level_suffix("ultra"), None);
    }

    // --- apply_codex_usage_test.go ----------------------------------------

    // port of TestExtractCodexReasoningEffortWithConfigurationUpdate. The
    // "invalid json" case needs unparsable bytes and does not apply to Value.
    #[test]
    fn extract_codex_reasoning_effort_with_configuration_update() {
        let cases: &[(&str, &str, &str, &str, &str)] = &[
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
                "low",
                "low",
            ),
            (
                "openai-response",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
                "low",
                "low",
            ),
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"second turn"},{"type":"configuration_update","reasoning":{"effort":"medium"}}]}"#,
                "medium",
                "medium",
            ),
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","tools":[]}]}"#,
                "xhigh",
                "xhigh",
            ),
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"none"}}]}"#,
                "none",
                "none",
            ),
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"auto"}}]}"#,
                "auto",
                "auto",
            ),
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","tools":[]}]}"#,
                "low",
                "low",
            ),
            (
                "codex",
                "gpt-6-astra",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"role":"user","content":"hello"}]}"#,
                "xhigh",
                "xhigh",
            ),
            (
                "codex",
                "gpt-6-astra(high)",
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
                "low",
                "low",
            ),
            (
                "openai-response",
                "gpt-6-astra(high)",
                r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","tools":[]}]}"#,
                "high",
                "xhigh",
            ),
        ];
        for (provider, model, body, want_request, want_translated) in cases {
            let body = j(body);
            assert_eq!(
                extract_reasoning_effort(&body, provider, model),
                *want_request,
                "{body}"
            );
            assert_eq!(
                extract_translated_reasoning_effort(&body, provider),
                *want_translated,
                "{body}"
            );
        }
    }

    // port of TestExtractCodexReasoningEffortWithConfigurationUpdateTargetRouting
    #[test]
    fn extract_codex_reasoning_effort_with_configuration_update_target_routing() {
        let source = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"medium"}},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":null}},{"role":"user","content":"ok"}]}"#,
        );
        for supported in [true, false] {
            let mut model = info(
                "opaque-route",
                "codex",
                level_support(&["low", "high", "xhigh"]),
            );
            model.support_configuration_update = supported;
            let body = apply_thinking_with_model_info_and_summary(
                &source,
                Some(&source),
                "opaque-route(high)",
                "openai-response",
                "codex",
                "codex",
                Some(&model),
                &SummaryConfig::default(),
                false,
            )
            .unwrap();
            assert_eq!(
                extract_reasoning_effort(&source, "openai-response", "opaque-route(high)"),
                "low"
            );
            let want = if supported { "low" } else { "high" };
            assert_eq!(extract_translated_reasoning_effort(&body, "codex"), want);
            assert_eq!(s(&body, "reasoning.effort"), "high");
            assert_eq!(
                s(&body, "input.0.type") == "configuration_update",
                supported,
                "{body}"
            );
        }
    }

    // port of TestApplyConfigurationUpdateRouting. The "invalid JSON" case needs
    // unparsable bytes and does not apply to Value.
    #[test]
    fn apply_configuration_update_routing() {
        struct Case {
            body: &'static str,
            format: &'static str,
            suffix: &'static str,
            supported: bool,
            no_thinking: bool,
            want_effort: &'static str,
            want_input: &'static str,
            want_same: bool,
        }
        let base = Case {
            body: "",
            format: "",
            suffix: "",
            supported: false,
            no_thinking: false,
            want_effort: "",
            want_input: "",
            want_same: false,
        };
        let cases = vec![
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                supported: true,
                want_effort: "xhigh",
                want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
                want_same: true,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
                supported: true,
                want_input: r#"[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]"#,
                want_same: true,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                supported: true,
                no_thinking: true,
                want_effort: "xhigh",
                want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
                want_same: true,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                supported: true,
                no_thinking: true,
                suffix: "high",
                want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                supported: true,
                suffix: "high",
                want_effort: "high",
                want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                supported: true,
                suffix: "high",
                want_effort: "high",
                want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"generate_summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                supported: true,
                suffix: "invalid",
                want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
                want_same: true,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"role":"user","content":"first"},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":"  "}},{"role":"assistant","content":"reply"},{"type":"configuration_update","reasoning":{"effort":"medium"}},{"type":"configuration_update","tools":[]},{"role":"user","content":"last"}]}"#,
                want_effort: "medium",
                want_input: r#"[{"role":"user","content":"first"},{"role":"assistant","content":"reply"},{"role":"user","content":"last"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":42}},{"type":"configuration_update","reasoning":{"effort":null}},{"type":"configuration_update","reasoning":{"effort":"  "}},{"role":"user","content":"ok"}]}"#,
                want_effort: "low",
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                suffix: "high",
                want_effort: "high",
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                want_effort: "low",
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
                no_thinking: true,
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                no_thinking: true,
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
                format: "openai-response",
                want_effort: "low",
                want_input: r#"[{"role":"user","content":"ok"}]"#,
                ..base
            },
            Case {
                body: r#"{"reasoning":{"summary":"auto"},"input":{"type":"configuration_update","reasoning":{"effort":"low"}}}"#,
                want_input: r#"{"type":"configuration_update","reasoning":{"effort":"low"}}"#,
                want_same: true,
                ..base
            },
        ];
        for case in cases {
            let format = if case.format.is_empty() {
                "codex"
            } else {
                case.format
            };
            let support = if case.no_thinking {
                None
            } else {
                level_support(&["low", "medium", "high", "xhigh"])
            };
            let mut model = info("configured-responses", "codex", support);
            model.support_configuration_update = case.supported;
            let model_name = if case.suffix.is_empty() {
                "configured-responses".to_string()
            } else {
                format!("configured-responses({})", case.suffix)
            };
            let body = j(case.body);
            let applied = apply_thinking_with_model_info(
                &body,
                Some(&body),
                &model_name,
                format,
                format,
                "codex",
                Some(&model),
            )
            .unwrap();
            if case.want_same {
                assert_eq!(applied, body);
            }
            assert_eq!(
                s(&applied, "reasoning.effort"),
                case.want_effort,
                "{applied}"
            );
            if !case.want_input.is_empty() {
                assert_eq!(gj_get(&applied, "input"), Some(&j(case.want_input)));
            }
            if gj_get(&body, "reasoning.summary").is_some() {
                assert_eq!(s(&applied, "reasoning.summary"), "auto", "{applied}");
            }
            if gj_get(&body, "reasoning.other").is_some() {
                assert_eq!(gj_get(&applied, "reasoning.other"), Some(&json!(7)));
            }
            if case.supported && !case.suffix.is_empty() {
                assert_eq!(extract_translated_reasoning_effort(&applied, format), "low");
            }
        }
    }

    // Termory-specific counterpart of TestApplyThinkingPreservesCodexTopLevelReasoningEffortBaseline:
    // Go finds gpt-6-astra (SupportConfigurationUpdate) in its static registry
    // and keeps the update item; with no registry the model is unknown, so the
    // update is promoted to the top level and removed.
    #[test]
    fn apply_thinking_unknown_codex_model_promotes_configuration_update() {
        let body = j(
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
        );
        let applied =
            apply_thinking_body_only(&body, "gpt-6-astra", "codex", "codex", "codex").unwrap();
        assert_eq!(
            applied,
            j(
                r#"{"model":"gpt-6-astra","reasoning":{"effort":"low","summary":"auto"},"input":[]}"#
            )
        );
    }

    // --- apply_configured_api_key_test.go ---------------------------------

    // port of TestApplyThinkingWithModelInfoMapsCrossFamilyHighIntent
    #[test]
    fn apply_thinking_with_model_info_maps_cross_family_high_intent() {
        let cases: &[(&str, &[&str], &str)] = &[
            ("xhigh", &["high", "max", "xhigh"], "xhigh"),
            ("xhigh", &["high", "max"], "max"),
            ("xhigh", &["high"], "high"),
            ("max", &["high", "xhigh", "max"], "max"),
            ("max", &["high", "xhigh"], "xhigh"),
            ("max", &["high"], "high"),
        ];
        for (source_level, supported, want) in cases {
            let model = info("claude-upstream", "claude", level_support(supported));
            let body = j(r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#);
            let source = json!({ "reasoning_effort": source_level });
            let out = apply_thinking_with_model_info(
                &body,
                Some(&source),
                "claude-upstream",
                "openai",
                "claude",
                "claude",
                Some(&model),
            )
            .unwrap();
            assert_eq!(s(&out, "output_config.effort"), *want, "{out}");
        }
    }

    // port of TestApplyThinkingWithModelInfoMapsOpenAICompatibilityHighIntent
    #[test]
    fn apply_thinking_with_model_info_maps_openai_compatibility_high_intent() {
        let model = info(
            "compat-upstream",
            "openai-compatibility",
            level_support(&["high", "max"]),
        );
        let out = apply_thinking_with_model_info(
            &j(r#"{"reasoning_effort":"high"}"#),
            Some(&j(r#"{"reasoning_effort":"xhigh"}"#)),
            "compat-upstream",
            "openai",
            "openai",
            "compat-provider",
            Some(&model),
        )
        .unwrap();
        assert_eq!(s(&out, "reasoning_effort"), "max");
    }

    // port of TestApplyThinkingWithModelInfoMapsResponsesToCodexHighIntent
    #[test]
    fn apply_thinking_with_model_info_maps_responses_to_codex_high_intent() {
        let model = info("codex-upstream", "codex", level_support(&["high", "xhigh"]));
        let out = apply_thinking_with_model_info(
            &j(r#"{"reasoning":{"effort":"high"}}"#),
            Some(&j(r#"{"reasoning":{"effort":"max"}}"#)),
            "codex-upstream",
            "openai-response",
            "codex",
            "codex",
            Some(&model),
        )
        .unwrap();
        assert_eq!(s(&out, "reasoning.effort"), "xhigh");
    }

    // port of TestApplyThinkingWithModelInfoKeepsSameFamilyValidationStrict
    #[test]
    fn apply_thinking_with_model_info_keeps_same_family_validation_strict() {
        let model = info(
            "openai-upstream",
            "openai",
            level_support(&["low", "medium", "high"]),
        );
        let body = j(r#"{"reasoning_effort":"xhigh"}"#);
        let err = apply_thinking_with_model_info(
            &body,
            Some(&body),
            "openai-upstream",
            "openai",
            "openai",
            "openai",
            Some(&model),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::LevelNotSupported);
        assert_eq!(
            err.message,
            "level \"xhigh\" not supported, valid levels: low, medium, high"
        );
    }

    // port of TestApplyThinkingWithModelInfoAppliesEnabledSummaryOnlyClaudeVisibility
    #[test]
    fn apply_thinking_with_model_info_applies_enabled_summary_only_claude_visibility() {
        let model = info("private-claude", "claude", level_support(&["high"]));
        let out = apply_thinking_with_model_info(
            &j(r#"{"model":"private-claude","max_tokens":32000}"#),
            Some(&j(r#"{"reasoning":{"summary":"auto"}}"#)),
            "private-claude",
            "openai-response",
            "claude",
            "claude",
            Some(&model),
        )
        .unwrap();
        assert_eq!(
            out,
            j(
                r#"{"model":"private-claude","max_tokens":32000,"thinking":{"type":"adaptive","display":"summarized"}}"#
            )
        );
    }

    // port of TestApplyThinkingWithModelInfoAndSummaryDropsInferredClaudeModeWhenSummaryRemoved
    #[test]
    fn apply_thinking_with_model_info_and_summary_drops_inferred_claude_mode_when_summary_removed()
    {
        let model = info(
            "private-manual-claude",
            "claude",
            Some(ThinkingSupport {
                min: 1024,
                max: 16000,
                ..ThinkingSupport::default()
            }),
        );
        let out = apply_thinking_with_model_info_and_summary(
            &j(r#"{"model":"private-manual-claude","max_tokens":32000,"thinking":{"type":"adaptive"}}"#),
            Some(&j(r#"{"reasoning":{"summary":"auto"}}"#)),
            "private-manual-claude",
            "openai-response",
            "claude",
            "claude",
            Some(&model),
            &SummaryConfig::default(),
            false,
        )
        .unwrap();
        assert_eq!(
            out,
            j(r#"{"model":"private-manual-claude","max_tokens":32000}"#)
        );
    }

    // port of TestApplyThinkingWithModelInfoDoesNotActivateClaudeForDisabledSummary
    #[test]
    fn apply_thinking_with_model_info_does_not_activate_claude_for_disabled_summary() {
        let model = info("private-claude", "claude", level_support(&["high"]));
        let body = j(r#"{"model":"private-claude","max_tokens":32000}"#);
        let out = apply_thinking_with_model_info(
            &body,
            Some(&j(r#"{"reasoning":{"summary":null}}"#)),
            "private-claude",
            "openai-response",
            "claude",
            "claude",
            Some(&model),
        )
        .unwrap();
        assert_eq!(out, body);
    }

    // port of TestApplyThinkingWithModelInfoSummaryOnlyDoesNotInventOpenAIEffort
    #[test]
    fn apply_thinking_with_model_info_summary_only_does_not_invent_openai_effort() {
        let model = info("private-openai", "openai", level_support(&["high", "max"]));
        let body = j(r#"{"model":"private-openai","messages":[{"role":"user","content":"hi"}]}"#);
        let out = apply_thinking_with_model_info(
            &body,
            Some(&j(
                r#"{"model":"private-openai","reasoning":{"summary":"auto"},"input":"hi"}"#,
            )),
            "private-openai",
            "openai-response",
            "openai",
            "openai",
            Some(&model),
        )
        .unwrap();
        assert_eq!(out, body);
    }

    // port of TestApplyThinkingWithSummaryKeepsOpenAIChatSuffixNone
    #[test]
    fn apply_thinking_with_summary_keeps_openai_chat_suffix_none() {
        let out = apply_thinking_with_summary(
            &j(r#"{"model":"private-openai","messages":[{"role":"user","content":"hi"}]}"#),
            "private-openai(none)",
            "openai-response",
            "openai",
            "openai",
            &SummaryConfig::enabled("auto"),
        )
        .unwrap();
        assert_eq!(
            out,
            j(
                r#"{"model":"private-openai","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"none"}"#
            )
        );
    }

    // port of TestApplyThinkingWithModelInfoUsesOpenRouterVisibility
    #[test]
    fn apply_thinking_with_model_info_uses_open_router_visibility() {
        let model = info(
            "openrouter-model",
            "openai-compatibility",
            level_support(&["high", "max"]),
        );
        let out = apply_thinking_with_model_info(
            &j(r#"{"model":"openrouter-model","messages":[{"role":"user","content":"hi"}]}"#),
            Some(&j(
                r#"{"model":"openrouter-model","reasoning":{"summary":"auto"},"input":"hi"}"#,
            )),
            "openrouter-model",
            "openai-response",
            "openai",
            "openrouter",
            Some(&model),
        )
        .unwrap();
        assert_eq!(
            out,
            j(
                r#"{"model":"openrouter-model","messages":[{"role":"user","content":"hi"}],"reasoning":{"exclude":false}}"#
            )
        );
    }

    // port of TestApplyConfigurationUpdateCrossProtocol
    #[test]
    fn apply_configuration_update_cross_protocol() {
        let source = j(
            r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"},{"type":"configuration_update","reasoning":{"effort":"high"}}]}"#,
        );
        let cases: &[(&str, &str, &str, &str, bool, &str, &str)] = &[
            (
                r#"{"messages":[{"role":"user","content":"ok"}],"reasoning_effort":"medium","other":true}"#,
                "private-chat",
                "openai",
                "openai",
                false,
                "reasoning_effort",
                "high",
            ),
            (
                r#"{"messages":[{"role":"user","content":"ok"}],"reasoning_effort":"medium","other":true}"#,
                "private-chat",
                "openai",
                "openai",
                true,
                "reasoning_effort",
                "high",
            ),
            (
                r#"{"max_tokens":4096,"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"},"other":true}"#,
                "private-claude",
                "claude",
                "claude",
                false,
                "output_config.effort",
                "high",
            ),
            (
                r#"{"messages":[{"role":"user","content":"ok"}],"reasoning_effort":"medium","other":true}"#,
                "private-chat(low)",
                "openai",
                "openai",
                false,
                "reasoning_effort",
                "low",
            ),
        ];
        for (body, model_name, format, type_name, supported, path, want) in cases {
            let mut model = info(
                "private",
                type_name,
                level_support(&["low", "medium", "high", "xhigh"]),
            );
            model.support_configuration_update = *supported;
            let out = apply_thinking_with_model_info(
                &j(body),
                Some(&source),
                model_name,
                "openai-response",
                format,
                format,
                Some(&model),
            )
            .unwrap();
            assert_eq!(s(&out, path), *want, "{out}");
            assert_eq!(gj_get(&out, "other"), Some(&json!(true)));
        }
    }

    // port of TestApplyConfigurationUpdateSourceEntry. The "registry capability
    // preserves native Responses" case depends on Go's static registry entry for
    // gpt-6-astra and is not portable.
    #[test]
    fn apply_configuration_update_source_entry() {
        let source = j(
            r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
        );
        let target = r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"medium"}},{"role":"user","content":"ok"}]}"#;
        let source_text = source.to_string();
        let cases: Vec<(&str, &str, &str, &str)> = vec![
            (
                source_text.as_str(),
                "codex",
                "low",
                r#"[{"role":"user","content":"ok"}]"#,
            ),
            (
                target,
                "codex",
                "low",
                r#"[{"role":"user","content":"ok"}]"#,
            ),
            (
                r#"{"reasoning_effort":"xhigh","messages":[{"role":"user","content":"ok"}]}"#,
                "openai",
                "low",
                "",
            ),
            (
                r#"{"reasoning":{"summary":"auto"},"input":{"type":"configuration_update"}}"#,
                "codex",
                "low",
                r#"{"type":"configuration_update"}"#,
            ),
        ];
        for (body, format, want, want_input) in cases {
            let out = apply_thinking_with_source_and_summary(
                &j(body),
                Some(&source),
                "gpt-6-unknown-routed",
                "openai-response",
                format,
                format,
                &extract_summary_config(&source, "openai-response"),
                false,
            )
            .unwrap();
            let path = if format == "openai" {
                "reasoning_effort"
            } else {
                "reasoning.effort"
            };
            assert_eq!(s(&out, path), want, "{out}");
            if !want_input.is_empty() {
                assert_eq!(gj_get(&out, "input"), Some(&j(want_input)));
            }
        }
    }

    // port of TestApplyConfigurationUpdateBoundModelWithoutThinking
    #[test]
    fn apply_configuration_update_bound_model_without_thinking() {
        let body = j(
            r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
        );
        let summary = extract_summary_config(&body, "codex");
        let out = apply_thinking_with_model_info_and_summary(
            &body,
            Some(&body),
            "gpt-6-astra",
            "codex",
            "codex",
            "codex",
            None,
            &summary,
            false,
        )
        .unwrap();
        assert_eq!(s(&out, "reasoning.effort"), "low");
        assert_eq!(
            gj_get(&out, "input")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        let mut custom = ModelInfo {
            id: "custom".into(),
            user_defined: true,
            ..ModelInfo::default()
        };
        let out = apply_thinking_with_model_info_and_summary(
            &body,
            Some(&body),
            "custom",
            "codex",
            "codex",
            "codex",
            Some(&custom),
            &summary,
            false,
        )
        .unwrap();
        assert_eq!(s(&out, "reasoning.effort"), "low");
        assert_eq!(
            gj_get(&out, "input")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        custom.support_configuration_update = true;
        for (model_name, want) in [("custom", "xhigh"), ("custom(high)", "high")] {
            let out = apply_thinking_with_model_info_and_summary(
                &body,
                Some(&body),
                model_name,
                "codex",
                "codex",
                "codex",
                Some(&custom),
                &summary,
                false,
            )
            .unwrap();
            assert_eq!(s(&out, "reasoning.effort"), want, "{out}");
            assert_eq!(s(&out, "input.0.reasoning.effort"), "low");
            assert_eq!(s(&out, "reasoning.summary"), "auto");
        }
    }

    // port of TestApplyThinkingWithModelInfoUsesOriginalResponsesEffort
    #[test]
    fn apply_thinking_with_model_info_uses_original_responses_effort() {
        let model = info("claude-upstream", "claude", level_support(&["high", "max"]));
        let out = apply_thinking_with_model_info(
            &j(r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#),
            Some(&j(r#"{"reasoning":{"effort":"xhigh"}}"#)),
            "claude-upstream",
            "openai-response",
            "claude",
            "claude",
            Some(&model),
        )
        .unwrap();
        assert_eq!(s(&out, "output_config.effort"), "max");
    }

    // --- summary_test.go (openai / responses / claude cases) --------------

    // port of TestExtractSummaryConfig
    #[test]
    fn extract_summary_config_cases() {
        use SummaryMode::{Disabled as D, Enabled as E, Unspecified as U};
        let cases: &[(&str, &str, SummaryMode, &str)] = &[
            ("openai", r#"{"reasoning_effort":"high"}"#, E, "auto"),
            ("openai", r#"{"reasoning_effort":"none"}"#, D, ""),
            ("openai", r#"{}"#, U, ""),
            ("openai", r#"{"reasoning_effort":null}"#, U, ""),
            ("openai", r#"{"reasoning_effort":17}"#, U, ""),
            (
                "openai",
                r#"{"reasoning_effort":"high","extra_body":{"google":{"thinking_config":{"include_thoughts":false}}}}"#,
                D,
                "",
            ),
            (
                "openai",
                r#"{"extra_body":{"google":{"thinking_config":{"include_thoughts":true}}}}"#,
                E,
                "auto",
            ),
            (
                "openai",
                r#"{"reasoning_effort":"high","reasoning":{"exclude":true}}"#,
                D,
                "",
            ),
            (
                "openai",
                r#"{"reasoning":{"effort":"high","exclude":false}}"#,
                E,
                "auto",
            ),
            (
                "openai",
                r#"{"reasoning_effort":"high","include_reasoning":false}"#,
                D,
                "",
            ),
            ("openai", r#"{"include_reasoning":true}"#, E, "auto"),
            ("openai", r#"{"reasoning":{"enabled":false}}"#, D, ""),
            ("openai", r#"{"reasoning":{"enabled":true}}"#, E, "auto"),
            (
                "openai",
                r#"{"reasoning":{"exclude":true},"include_reasoning":true}"#,
                D,
                "",
            ),
            ("openai", r#"{"include_reasoning":"false"}"#, U, ""),
            (
                "openai-response",
                r#"{"reasoning":{"effort":"high"}}"#,
                U,
                "",
            ),
            (
                "openai-response",
                r#"{"reasoning":{"effort":"high","summary":"auto"}}"#,
                E,
                "auto",
            ),
            (
                "openai-response",
                r#"{"reasoning":{"summary":"concise"}}"#,
                E,
                "concise",
            ),
            (
                "openai-response",
                r#"{"reasoning":{"summary":null}}"#,
                D,
                "",
            ),
            (
                "openai-response",
                r#"{"reasoning":{"summary":true}}"#,
                U,
                "",
            ),
            (
                "openai-response",
                r#"{"reasoning":{"generate_summary":"detailed"}}"#,
                E,
                "detailed",
            ),
            (
                "claude",
                r#"{"thinking":{"type":"adaptive","display":"summarized"}}"#,
                E,
                "auto",
            ),
            (
                "claude",
                r#"{"thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"}}"#,
                D,
                "",
            ),
            ("claude", r#"{"thinking":{"display":"summarized"}}"#, U, ""),
            (
                "claude",
                r#"{"thinking":{"type":"auto","display":"summarized"}}"#,
                U,
                "",
            ),
            (
                "claude",
                r#"{"thinking":{"type":"enabled","display":"summarized"}}"#,
                E,
                "auto",
            ),
            (
                "claude",
                r#"{"thinking":{"type":"enabled","budget_tokens":0,"display":"summarized"}}"#,
                U,
                "",
            ),
            (
                "claude",
                r#"{"thinking":{"type":"enabled","budget_tokens":-1,"display":"summarized"}}"#,
                E,
                "auto",
            ),
            (
                "claude",
                r#"{"thinking":{"type":"enabled","budget_tokens":-1,"display":"omitted"}}"#,
                D,
                "",
            ),
        ];
        for (format, body, mode, detail) in cases {
            assert_eq!(
                extract_summary_config(&j(body), format),
                SummaryConfig {
                    mode: *mode,
                    detail: detail.to_string()
                },
                "{format} {body}"
            );
        }
    }

    // port of TestExtractExplicitSummaryConfigDoesNotUseChatEffort
    #[test]
    fn extract_explicit_summary_config_does_not_use_chat_effort() {
        assert_eq!(
            extract_explicit_summary_config(&j(r#"{"reasoning_effort":"high"}"#), "openai").mode,
            SummaryMode::Unspecified
        );
        assert_eq!(
            extract_explicit_summary_config(
                &j(r#"{"reasoning_effort":"high","reasoning":{"exclude":true}}"#),
                "openai"
            )
            .mode,
            SummaryMode::Disabled
        );
    }

    // port of TestApplySummaryConfig (in-scope formats)
    #[test]
    fn apply_summary_config_cases() {
        let en = SummaryConfig {
            mode: SummaryMode::Enabled,
            detail: String::new(),
        };
        let dis = SummaryConfig::disabled();
        let concise = SummaryConfig::enabled("concise");
        let cases: Vec<(&str, &str, &SummaryConfig, &str)> = vec![
            ("openai", "{}", &en, "{}"),
            (
                "openai",
                r#"{"reasoning_effort":"high"}"#,
                &en,
                r#"{"reasoning_effort":"high"}"#,
            ),
            (
                "openai",
                r#"{"reasoning_effort":"none"}"#,
                &en,
                r#"{"reasoning_effort":"none"}"#,
            ),
            (
                "openai",
                r#"{"reasoning_effort":"high"}"#,
                &dis,
                r#"{"reasoning_effort":"high"}"#,
            ),
            (
                "openai",
                r#"{"reasoning":{"effort":"high","exclude":false}}"#,
                &dis,
                r#"{"reasoning":{"effort":"high","exclude":true}}"#,
            ),
            (
                "openai",
                r#"{"reasoning":{"effort":"high","exclude":true}}"#,
                &en,
                r#"{"reasoning":{"effort":"high","exclude":false}}"#,
            ),
            (
                "openai",
                r#"{"reasoning_effort":"high","include_reasoning":true}"#,
                &dis,
                r#"{"reasoning_effort":"high","include_reasoning":false}"#,
            ),
            (
                "claude",
                r#"{"thinking":{"type":"adaptive"}}"#,
                &en,
                r#"{"thinking":{"type":"adaptive","display":"summarized"}}"#,
            ),
            (
                "claude",
                r#"{"thinking":{"type":"enabled","budget_tokens":2048}}"#,
                &dis,
                r#"{"thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"}}"#,
            ),
            (
                "openai-response",
                "{}",
                &concise,
                r#"{"reasoning":{"summary":"concise"}}"#,
            ),
        ];
        for (format, body, config, want) in cases {
            assert_eq!(
                apply_summary_config(&j(body), format, config),
                j(want),
                "{format} {body}"
            );
        }
    }

    // port of TestApplySummaryConfig_OpenAIChatProviderDialects
    #[test]
    fn apply_summary_config_openai_chat_provider_dialects() {
        let cases: &[(&str, &str, SummaryMode, &str)] = &[
            ("openai", "{}", SummaryMode::Enabled, "{}"),
            (
                "openrouter",
                "{}",
                SummaryMode::Enabled,
                r#"{"reasoning":{"exclude":false}}"#,
            ),
            (
                "prod-openrouter",
                "{}",
                SummaryMode::Disabled,
                r#"{"reasoning":{"exclude":true}}"#,
            ),
            (
                "deepseek",
                r#"{"reasoning_effort":"high"}"#,
                SummaryMode::Disabled,
                r#"{"reasoning_effort":"high"}"#,
            ),
            (
                "kimi",
                r#"{"reasoning_effort":"max"}"#,
                SummaryMode::Enabled,
                r#"{"reasoning_effort":"max"}"#,
            ),
            (
                "moonshot",
                r#"{"thinking":{"type":"enabled"}}"#,
                SummaryMode::Enabled,
                r#"{"thinking":{"type":"enabled"}}"#,
            ),
            (
                "openai-compatibility",
                r#"{"reasoning":{"exclude":false}}"#,
                SummaryMode::Disabled,
                r#"{"reasoning":{"exclude":true}}"#,
            ),
        ];
        for (provider, body, mode, want) in cases {
            let out = apply_summary_config_for_provider(
                j(body),
                "openai",
                "model",
                provider,
                None,
                &SummaryConfig {
                    mode: *mode,
                    detail: String::new(),
                },
            );
            assert_eq!(out, j(want), "{provider}");
        }
    }

    // port of TestApplySummaryConfig_ClaudeDisplayRequiresActiveThinking
    #[test]
    fn apply_summary_config_claude_display_requires_active_thinking() {
        for mode in [SummaryMode::Enabled, SummaryMode::Disabled] {
            for body in [
                "{}",
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                r#"{"thinking":{"type":"disabled"}}"#,
            ] {
                let body = j(body);
                let out = apply_summary_config(
                    &body,
                    "claude",
                    &SummaryConfig {
                        mode,
                        detail: String::new(),
                    },
                );
                assert_eq!(out, body);
            }
        }
    }

    // port of TestApplySummaryConfigForModel_ClaudeDisabledSummaryDoesNotEnableThinking
    #[test]
    fn apply_summary_config_for_model_claude_disabled_summary_does_not_enable_thinking() {
        for model in ["claude-opus-5", "claude-haiku-4-5-20251001"] {
            let body = json!({ "model": model, "max_tokens": 32000 });
            let out =
                apply_summary_config_for_model(&body, "claude", model, &SummaryConfig::disabled());
            assert_eq!(out, body);
        }
    }

    // port of TestApplySummaryConfig_ResponsesNormalizesDeprecatedGenerateSummary
    #[test]
    fn apply_summary_config_responses_normalizes_deprecated_generate_summary() {
        let out = apply_summary_config(
            &j(r#"{"reasoning":{"generate_summary":"detailed"}}"#),
            "openai-response",
            &SummaryConfig::enabled("detailed"),
        );
        assert_eq!(out, j(r#"{"reasoning":{"summary":"detailed"}}"#));
    }

    // port of TestApplySummaryConfig_ResponsesDisabledOmitsSummary
    #[test]
    fn apply_summary_config_responses_disabled_omits_summary() {
        let out = apply_summary_config(
            &j(r#"{"reasoning":{"effort":"high","summary":"auto"}}"#),
            "openai-response",
            &SummaryConfig::disabled(),
        );
        assert_eq!(out, j(r#"{"reasoning":{"effort":"high"}}"#));
    }

    // port of TestApplySummaryConfig_ResponsesDisabledDropsEmptyReasoning
    #[test]
    fn apply_summary_config_responses_disabled_drops_empty_reasoning() {
        let out = apply_summary_config(
            &j(r#"{"model":"gpt-5.4","reasoning":{"summary":"auto"}}"#),
            "openai-response",
            &SummaryConfig::disabled(),
        );
        assert_eq!(out, j(r#"{"model":"gpt-5.4"}"#));
    }

    // port of TestApplySummaryConfig_UnspecifiedLeavesBodyUnchanged
    #[test]
    fn apply_summary_config_unspecified_leaves_body_unchanged() {
        let body = j(r#"{"thinking":{"type":"adaptive"}}"#);
        assert_eq!(
            apply_summary_config(&body, "claude", &SummaryConfig::default()),
            body
        );
    }

    // --- helps/model_capabilities_test.go ---------------------------------

    // port of TestApplyRequestThinkingConfigurationUpdateSelectedCodexModel: the
    // auth manager's alias resolution is replaced by binding the selected model
    // ("opaque-route") directly.
    #[test]
    fn apply_request_thinking_configuration_update_selected_codex_model() {
        let current = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":"high"}},{"role":"user","content":"ok"}]}"#,
        );
        let original = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
        );
        let translated =
            j(r#"{"reasoning":{"effort":"xhigh"},"input":[{"role":"user","content":"ok"}]}"#);
        let cases: Vec<(bool, &str, &Value, &str, usize)> = vec![
            (true, "opaque-route", &current, "xhigh", 2),
            (true, "opaque-route", &translated, "xhigh", 0),
            (false, "opaque-route", &translated, "high", 0),
            (false, "opaque-route", &original, "high", 0),
            (false, "opaque-route(medium)", &translated, "medium", 0),
        ];
        for (supported, model_name, body, want_top, want_updates) in cases {
            let mut model = info(
                "opaque-route",
                "codex",
                level_support(&["low", "medium", "high", "xhigh"]),
            );
            model.support_configuration_update = supported;
            let out = apply_request_thinking(
                body,
                Some(&current),
                Some(&original),
                model_name,
                "openai-response",
                "codex",
                "codex",
                ModelBinding::Bound(Some(&model)),
                false,
            )
            .unwrap();
            assert_eq!(s(&out, "reasoning.effort"), want_top, "{out}");
            let updates = gj_get(&out, "input")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| s(item, "type") == "configuration_update")
                        .count()
                })
                .unwrap_or(0);
            assert_eq!(updates, want_updates, "{out}");
            if supported {
                assert_eq!(&out, body);
            }
        }
    }

    // port of TestApplyRequestThinkingConfigurationUpdateCrossProtocol (the
    // gemini case is out of scope).
    #[test]
    fn apply_request_thinking_configuration_update_cross_protocol() {
        let current = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":"high"}}]}"#,
        );
        let original = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
        );
        let cases: &[(&str, &str, &str, &str, bool)] = &[
            (
                "codex",
                r#"{"reasoning":{"effort":"xhigh"},"input":[{"role":"user","content":"ok"}]}"#,
                "reasoning.effort",
                "high",
                false,
            ),
            (
                "openai",
                r#"{"reasoning_effort":"medium","messages":[]}"#,
                "reasoning_effort",
                "high",
                false,
            ),
            (
                "claude",
                r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"},"max_tokens":4096}"#,
                "output_config.effort",
                "high",
                false,
            ),
            (
                "openai",
                r#"{"reasoning_effort":"medium","messages":[]}"#,
                "reasoning_effort",
                "low",
                true,
            ),
        ];
        for (format, body, path, want, no_source) in cases {
            let current_source = if *no_source { None } else { Some(&current) };
            let out = apply_request_thinking(
                &j(body),
                current_source,
                Some(&original),
                "task5-unknown-route",
                "openai-response",
                format,
                format,
                ModelBinding::Unbound,
                false,
            )
            .unwrap();
            assert_eq!(s(&out, path), *want, "{out}");
        }
    }

    // --- unknown-model behaviour of the router entry point ----------------

    #[test]
    fn apply_thinking_unknown_model_writes_suffix_unvalidated() {
        // codex / Responses: the level goes straight into reasoning.effort.
        let out = apply_thinking(
            &j(r#"{"model":"gpt-5.6-terra","input":[]}"#),
            "gpt-5.6-terra(high)",
            "openai-response",
            "codex",
            "codex",
            None,
        )
        .unwrap();
        assert_eq!(
            out,
            j(r#"{"model":"gpt-5.6-terra","input":[],"reasoning":{"effort":"high"}}"#)
        );

        // OpenAI chat: a numeric budget is bucketed to a level.
        let out = apply_thinking(
            &j(r#"{"messages":[]}"#),
            "m(8192)",
            "openai",
            "openai",
            "openai",
            None,
        )
        .unwrap();
        assert_eq!(out, j(r#"{"messages":[],"reasoning_effort":"medium"}"#));

        // Claude: a budget becomes manual thinking, a level becomes adaptive
        // effort, none disables, auto enables without a budget.
        let claude = |model: &str| {
            apply_thinking(
                &j(r#"{"max_tokens":4096}"#),
                model,
                "claude",
                "claude",
                "claude",
                None,
            )
            .unwrap()
        };
        // Anthropic wants `budget_tokens < max_tokens`: a suffix larger than
        // the request's `max_tokens` (Claude Code's small helper calls) is
        // capped under it instead of earning a 400 on every such request.
        assert_eq!(
            claude("claude-x(8192)"),
            j(r#"{"max_tokens":4096,"thinking":{"type":"enabled","budget_tokens":4095}}"#)
        );
        assert_eq!(
            claude("claude-x(2048)"),
            j(r#"{"max_tokens":4096,"thinking":{"type":"enabled","budget_tokens":2048}}"#)
        );
        assert_eq!(
            claude("claude-x(high)"),
            j(
                r#"{"max_tokens":4096,"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}"#
            )
        );
        assert_eq!(
            claude("claude-x(none)"),
            j(r#"{"max_tokens":4096,"thinking":{"type":"disabled"}}"#)
        );
        assert_eq!(
            claude("claude-x(-1)"),
            j(r#"{"max_tokens":4096,"thinking":{"type":"enabled"}}"#)
        );

        // xAI uses the codex applier; auto passes through as "auto".
        let out = apply_thinking(
            &j("{}"),
            "grok-4(auto)",
            "openai-response",
            "xai",
            "xai",
            None,
        )
        .unwrap();
        assert_eq!(out, j(r#"{"reasoning":{"effort":"auto"}}"#));

        // No suffix and no body config: unchanged.
        let body = j(r#"{"messages":[]}"#);
        assert_eq!(
            apply_thinking(&body, "m", "openai", "openai", "openai", Some(&body)).unwrap(),
            body
        );
    }

    #[test]
    fn apply_thinking_with_model_clamps_level() {
        let model = info("m", "codex", level_support(&["low", "medium", "high"]));
        let out = apply_thinking_with_model(
            &j("{}"),
            "m(xhigh)",
            "claude",
            "codex",
            "codex",
            None,
            Some(&model),
        )
        .unwrap();
        assert_eq!(out, j(r#"{"reasoning":{"effort":"high"}}"#));
    }

    #[test]
    fn convert_helpers() {
        assert_eq!(convert_level_to_budget("HIGH"), Some(24576));
        assert_eq!(convert_level_to_budget("ultra"), None);
        assert_eq!(convert_budget_to_level(-2), None);
        assert_eq!(convert_budget_to_level(513), Some(LEVEL_LOW));
        assert_eq!(convert_budget_to_level(24577), Some(LEVEL_XHIGH));
        assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
        assert_eq!(map_to_claude_effort("xhigh", true), Some("max"));
        assert_eq!(map_to_claude_effort("", true), None);
        let support = ThinkingSupport {
            min: 1024,
            max: 16000,
            ..ThinkingSupport::default()
        };
        let model = info("m", "claude", Some(support));
        assert_eq!(clamp_budget(64000, Some(&model)), 16000);
        assert_eq!(clamp_budget(0, Some(&model)), 1024);
        assert_eq!(clamp_budget(-1, Some(&model)), -1);
        let model = info("m", "codex", level_support(&["low", "high"]));
        assert_eq!(clamp_level("medium", Some(&model)), "low");
        assert_eq!(clamp_level("xhigh", Some(&model)), "high");
    }

    // --- gemini (provider/gemini/apply.go, summary.go, apply.go) ----------

    fn gemini_info(id: &str, support: ThinkingSupport) -> ModelInfo {
        info(id, "gemini", Some(support))
    }

    /// The Gemini models of test/thinking_conversion_test.go getTestModels().
    fn e2e_model(base: &str) -> ModelInfo {
        match base {
            "level-subset-model" => gemini_info(
                base,
                ThinkingSupport {
                    levels: levels(&["low", "high"]),
                    ..ThinkingSupport::default()
                },
            ),
            "gemini-budget-model" => gemini_info(
                base,
                ThinkingSupport {
                    min: 128,
                    max: 20000,
                    dynamic_allowed: true,
                    ..ThinkingSupport::default()
                },
            ),
            "gemini-mixed-model" => gemini_info(
                base,
                ThinkingSupport {
                    min: 128,
                    max: 32768,
                    levels: levels(&["low", "high"]),
                    dynamic_allowed: true,
                    ..ThinkingSupport::default()
                },
            ),
            "user-defined-model" => ModelInfo {
                id: base.to_string(),
                model_type: "openai".to_string(),
                user_defined: true,
                ..ModelInfo::default()
            },
            other => panic!("unknown e2e model {other}"),
        }
    }

    // port of the gemini-target cases of TestThinkingE2EMatrix_Suffix
    // (test/thinking_conversion_test.go). The registry model is bound
    // explicitly; the translated body is the translator's thinking-free
    // envelope, and the whole generationConfig is compared exactly.
    #[test]
    fn thinking_e2e_matrix_suffix_gemini_targets() {
        let cases: &[(&str, &str, &str, Option<&str>)] = &[
            (
                "17",
                "claude",
                "level-subset-model(1)",
                Some(r#"{"thinkingConfig":{"thinkingLevel":"low"}}"#),
            ),
            ("18", "openai", "gemini-budget-model", None),
            (
                "19",
                "openai",
                "gemini-budget-model(medium)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":8192}}"#),
            ),
            (
                "20",
                "openai",
                "gemini-budget-model(xhigh)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":20000}}"#),
            ),
            (
                "21",
                "openai",
                "gemini-budget-model(none)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":128}}"#),
            ),
            (
                "22",
                "openai",
                "gemini-budget-model(auto)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":-1}}"#),
            ),
            ("23", "claude", "gemini-budget-model", None),
            (
                "24",
                "claude",
                "gemini-budget-model(8192)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":8192}}"#),
            ),
            (
                "25",
                "claude",
                "gemini-budget-model(64000)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":20000}}"#),
            ),
            (
                "26",
                "claude",
                "gemini-budget-model(0)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":128}}"#),
            ),
            (
                "27",
                "claude",
                "gemini-budget-model(-1)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":-1}}"#),
            ),
            ("28", "openai", "gemini-mixed-model", None),
            (
                "29",
                "openai",
                "gemini-mixed-model(high)",
                Some(r#"{"thinkingConfig":{"thinkingLevel":"high"}}"#),
            ),
            (
                "30",
                "openai",
                "gemini-mixed-model(xhigh)",
                Some(r#"{"thinkingConfig":{"thinkingLevel":"high"}}"#),
            ),
            (
                "31",
                "openai",
                "gemini-mixed-model(none)",
                Some(r#"{"thinkingConfig":{"thinkingLevel":"low"}}"#),
            ),
            (
                "32",
                "openai",
                "gemini-mixed-model(auto)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":-1}}"#),
            ),
            ("33", "claude", "gemini-mixed-model", None),
            (
                "34",
                "claude",
                "gemini-mixed-model(8192)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":8192}}"#),
            ),
            (
                "35",
                "claude",
                "gemini-mixed-model(64000)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":32768}}"#),
            ),
            (
                "36",
                "claude",
                "gemini-mixed-model(0)",
                Some(r#"{"thinkingConfig":{"thinkingLevel":"low"}}"#),
            ),
            (
                "37",
                "claude",
                "gemini-mixed-model(-1)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":-1}}"#),
            ),
            (
                "76",
                "openai",
                "user-defined-model(8192)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":8192}}"#),
            ),
            (
                "78",
                "openai-response",
                "user-defined-model(8192)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":8192}}"#),
            ),
            (
                "84",
                "gemini",
                "gemini-budget-model(8192)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":8192}}"#),
            ),
            (
                "85",
                "gemini",
                "gemini-budget-model(64000)",
                Some(r#"{"thinkingConfig":{"thinkingBudget":20000}}"#),
            ),
        ];
        for (name, from, model, want) in cases {
            let base = parse_suffix(model).model_name;
            let body = json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],"model":base});
            let model_info = e2e_model(&base);
            let out = apply_thinking_with_model_info(
                &body,
                None,
                model,
                from,
                "gemini",
                "gemini",
                Some(&model_info),
            )
            .unwrap_or_else(|e| panic!("case {name}: {e}"));
            let mut expected = body.clone();
            if let Some(config) = want {
                expected["generationConfig"] = j(config);
            }
            assert_eq!(out, expected, "case {name}");
        }
    }

    // port of the gemini→gemini cases (84, 85) of TestThinkingE2EMatrix_Body
    #[test]
    fn thinking_e2e_matrix_body_gemini_to_gemini() {
        let model_info = e2e_model("gemini-budget-model");
        let body = j(
            r#"{"model":"gemini-budget-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#,
        );
        let out = apply_thinking_with_model_info(
            &body,
            None,
            "gemini-budget-model",
            "gemini",
            "gemini",
            "gemini",
            Some(&model_info),
        )
        .unwrap();
        assert_eq!(out, body);

        let body = j(
            r#"{"model":"gemini-budget-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":64000}}}"#,
        );
        let err = apply_thinking_with_model_info(
            &body,
            None,
            "gemini-budget-model",
            "gemini",
            "gemini",
            "gemini",
            Some(&model_info),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BudgetOutOfRange);
        assert_eq!(err.message, "budget 64000 out of range [128,20000]");
    }

    // port of the gemini case of TestApplyRequestThinkingConfigurationUpdateCrossProtocol
    // (helps/model_capabilities_test.go)
    #[test]
    fn apply_request_thinking_configuration_update_cross_protocol_gemini() {
        let current = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":"high"}}]}"#,
        );
        let original = j(
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
        );
        let out = apply_request_thinking(
            &j(r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#),
            Some(&current),
            Some(&original),
            "task5-unknown-route",
            "openai-response",
            "gemini",
            "gemini",
            ModelBinding::Unbound,
            false,
        )
        .unwrap();
        assert_eq!(
            out,
            j(r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":24576}}}"#)
        );
    }

    // port of the gemini cases of TestExtractSummaryConfig (summary_test.go)
    #[test]
    fn extract_summary_config_gemini_cases() {
        let cases: &[(&str, SummaryConfig)] = &[
            (
                r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}"#,
                SummaryConfig::enabled("auto"),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":false}}}"#,
                SummaryConfig::disabled(),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":"true"}}}"#,
                SummaryConfig::default(),
            ),
            (
                r#"{"generation_config":{"thinking_config":{"include_thoughts":false}}}"#,
                SummaryConfig::disabled(),
            ),
        ];
        for (body, want) in cases {
            assert_eq!(extract_summary_config(&j(body), "gemini"), *want, "{body}");
        }
    }

    // port of the gemini cases of TestApplySummaryConfig and
    // TestApplySummaryConfigNormalizesTargetAliases (summary_test.go)
    #[test]
    fn apply_summary_config_gemini_cases() {
        assert_eq!(
            apply_summary_config(&json!({}), "gemini", &SummaryConfig::enabled("")),
            j(r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}"#)
        );
        assert_eq!(
            apply_summary_config(&json!({}), "gemini", &SummaryConfig::disabled()),
            j(r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":false}}}"#)
        );
        assert_eq!(
            apply_summary_config(
                &j(r#"{"generationConfig":{"thinkingConfig":{"include_thoughts":true}}}"#),
                "gemini",
                &SummaryConfig::enabled("")
            ),
            j(r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}"#)
        );
    }

    #[test]
    fn extract_gemini_thinking_config() {
        let cases: &[(&str, ThinkingConfig)] = &[
            (
                r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"high","thinkingBudget":10}}}"#,
                ThinkingConfig::with_level("high"),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"thinking_level":"none"}}}"#,
                ThinkingConfig::none(),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"auto"}}}"#,
                ThinkingConfig::auto(),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"thinking_budget":4096}}}"#,
                ThinkingConfig::with_budget(4096),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":0}}}"#,
                ThinkingConfig::none(),
            ),
            (
                r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":-1}}}"#,
                ThinkingConfig::auto(),
            ),
            (r#"{"generationConfig":{}}"#, ThinkingConfig::default()),
        ];
        for (body, want) in cases {
            assert_eq!(extract_thinking_config(&j(body), "gemini"), *want, "{body}");
        }
        assert_eq!(
            reasoning_effort_from_config(&extract_thinking_config(
                &j(r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#),
                "gemini"
            )),
            "medium"
        );
    }

    // provider/gemini/apply.go: the unknown-model (applyCompatible) path the
    // router takes, through the public entry point.
    #[test]
    fn apply_thinking_unknown_model_gemini_target() {
        let base = r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingLevel":"low","thinking_budget":1,"include_thoughts":true}}}"#;
        let cases: &[(&str, &str)] = &[
            // A level becomes a budget (gemini is budget-capable); the snake
            // include_thoughts is normalised to includeThoughts.
            (
                "m(high)",
                r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":24576,"includeThoughts":true}}}"#,
            ),
            (
                "m(4096)",
                r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":4096,"includeThoughts":true}}}"#,
            ),
            (
                "m(auto)",
                r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":-1,"includeThoughts":true}}}"#,
            ),
            // none: budget 0 (ModeNone without a level takes the budget format,
            // which restores the explicit include_thoughts as includeThoughts).
            (
                "m(none)",
                r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":0,"includeThoughts":true}}}"#,
            ),
        ];
        for (model, want) in cases {
            let out = apply_thinking(&j(base), model, "gemini", "gemini", "gemini", None).unwrap();
            assert_eq!(out, j(want), "{model}");
        }
        // No suffix: the body's own level is re-applied as a budget.
        let out = apply_thinking(&j(base), "m", "gemini", "gemini", "gemini", None).unwrap();
        assert_eq!(
            out,
            j(
                r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":1024,"includeThoughts":true}}}"#
            )
        );
        // Cross-format: an OpenAI Chat source effort lands as a Gemini budget.
        // The source's summary intent is NOT restored: the translated body
        // could represent includeThoughts but does not, which
        // translatedRequestSummaryConfig reads as a deliberate removal.
        let source = j(r#"{"reasoning_effort":"medium","messages":[]}"#);
        let out = apply_thinking(
            &j(r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingLevel":"medium"}}}"#),
            "m",
            "openai",
            "gemini",
            "gemini",
            Some(&source),
        )
        .unwrap();
        assert_eq!(
            out,
            j(r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#)
        );
    }

    // provider/gemini/apply.go: the validated (registry-bound) level / none paths.
    #[test]
    fn apply_gemini_validated_level_and_none() {
        let level_model = gemini_info(
            "g3",
            ThinkingSupport {
                levels: levels(&["low", "high"]),
                zero_allowed: true,
                ..ThinkingSupport::default()
            },
        );
        let body = j(
            r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":5,"includeThoughts":false}}}"#,
        );
        assert_eq!(
            apply_gemini(
                body.clone(),
                &ThinkingConfig::with_level("high"),
                Some(&level_model)
            ),
            j(
                r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"high","includeThoughts":false}}}"#
            )
        );
        // ModeNone with budget 0 and no level removes thinkingConfig entirely.
        assert_eq!(
            apply_gemini(body.clone(), &ThinkingConfig::none(), Some(&level_model)),
            j(r#"{"generationConfig":{}}"#)
        );
        // ModeNone carrying a level keeps that level.
        let none_low = ThinkingConfig {
            mode: ThinkingMode::None,
            budget: 128,
            level: "low".to_string(),
        };
        assert_eq!(
            apply_gemini(body.clone(), &none_low, Some(&level_model)),
            j(
                r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low","includeThoughts":false}}}"#
            )
        );
        // A model without thinking support is left unchanged.
        let plain = info("g", "gemini", None);
        assert_eq!(
            apply_gemini(body.clone(), &ThinkingConfig::with_budget(10), Some(&plain)),
            body
        );
    }
}
