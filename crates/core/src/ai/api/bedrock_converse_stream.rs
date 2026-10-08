//! Port of `pi-core/ai/src/api/bedrock-converse-stream.ts`: request building
//! and response-event handling for the Bedrock converse-stream API.
//!
//! The TypeScript implementation routes through the AWS SDK
//! (`@aws-sdk/client-bedrock-runtime`); the Rust port builds the same request
//! payload (observable through `onPayload`) and drives the same event-item
//! sequence. [`BedrockDispatchResponse`] plus [`stream_from_items`] replace
//! the SDK dispatch — the seam the TypeScript tests create by mocking the
//! SDK client. The wire transport composes the same driver.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::ai::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::ai::api::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
    clamp_reasoning,
};
use crate::ai::api::transform_messages::transform_messages;
use crate::ai::models::calculate_cost;
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent,
    BlockContent, CacheRetention, Context, Message, Model, ModelThinkingLevel, ProviderEnv,
    SimpleStreamOptions, StreamOptions, ThinkingBudgets, ThinkingContent, ThinkingLevel, Tool,
    ToolCall, ToolChoice, UserContent,
};
use crate::ai::utils::diagnostics::append_assistant_message_diagnostic;
use crate::ai::utils::error_body::{ProviderErrorParts, normalize_provider_error};
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::json_parse::parse_streaming_json;
use crate::ai::utils::provider_env::get_provider_env_value;
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;

/// Port of `BedrockThinkingDisplay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BedrockThinkingDisplay {
    Summarized,
    Omitted,
}

/// Port of `BedrockOptions`: `StreamOptions` plus the Bedrock fields.
#[derive(Clone, Default)]
pub struct BedrockOptions {
    pub base: StreamOptions,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub interleaved_thinking: Option<bool>,
    pub thinking_display: Option<BedrockThinkingDisplay>,
    pub request_metadata: Option<BTreeMap<String, String>>,
    pub bearer_token: Option<String>,
}

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

/// Matches the placeholder the Anthropic API path uses for redacted thinking.
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";

/// The error shape the TypeScript code inspects on caught SDK exceptions.
#[derive(Clone, Debug, Default)]
pub struct BedrockError {
    /// `error.name` — modeled service exceptions end in `Exception`.
    pub name: String,
    pub message: String,
    pub status: Option<u16>,
    pub body: Option<String>,
    /// `$metadata.httpStatusCode`.
    pub http_status_code: Option<u16>,
    /// `$metadata.requestId`.
    pub request_id: Option<String>,
    /// `error instanceof BedrockRuntimeServiceException`.
    pub service_exception: bool,
}

impl BedrockError {
    /// A transport error (not an SDK service exception).
    pub fn transport(message: impl Into<String>) -> Self {
        BedrockError {
            message: message.into(),
            ..Default::default()
        }
    }
}

/// What the AWS SDK's `client.send()` returns in TypeScript: response
/// `$metadata` plus the stream of ConverseStreamOutput items.
#[derive(Clone, Debug, Default)]
pub struct BedrockDispatchResponse {
    pub http_status_code: Option<u16>,
    pub request_id: Option<String>,
    /// Raw HTTP response headers (the deserialize-middleware path); absent
    /// means the synthesized `$metadata` fallback applies.
    pub raw_headers: Option<BTreeMap<String, String>>,
    pub items: Vec<Value>,
    /// `client.send()` rejecting.
    pub send_error: Option<BedrockError>,
    /// The item iterator throwing mid-stream, after yielding `items`.
    pub stream_error: Option<BedrockError>,
}

/// The streaming block scratch state; the TS code keeps these fields on the
/// content blocks themselves and strips them at finalize.
#[derive(Clone, Debug)]
enum StreamingBlock {
    Text {
        index: Option<u64>,
        text: String,
    },
    Thinking {
        index: Option<u64>,
        thinking: String,
        thinking_signature: String,
        redacted: bool,
        redacted_chunks: Vec<Vec<u8>>,
    },
    ToolCall {
        index: Option<u64>,
        id: String,
        name: String,
        arguments: serde_json::Map<String, Value>,
        partial_json: String,
    },
}

impl StreamingBlock {
    fn index(&self) -> Option<u64> {
        match self {
            StreamingBlock::Text { index, .. }
            | StreamingBlock::Thinking { index, .. }
            | StreamingBlock::ToolCall { index, .. } => *index,
        }
    }

    /// The finalized content block (scratch fields stripped).
    fn into_content(self) -> AssistantContent {
        match self {
            StreamingBlock::Text { text, .. } => {
                AssistantContent::Text(crate::ai::types::TextContent {
                    text,
                    ..Default::default()
                })
            }
            StreamingBlock::Thinking {
                thinking,
                thinking_signature,
                redacted,
                redacted_chunks,
                ..
            } => {
                // Encrypted reasoning encodes into `thinkingSignature`; the
                // scratch buffer must never reach a persisted message.
                let thinking_signature = if redacted && !redacted_chunks.is_empty() {
                    Some(bytes_to_base64(&redacted_chunks))
                } else if thinking_signature.is_empty() {
                    None
                } else {
                    Some(thinking_signature)
                };
                AssistantContent::Thinking(ThinkingContent {
                    thinking,
                    thinking_signature,
                    redacted: redacted.then_some(true),
                    ..Default::default()
                })
            }
            StreamingBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => AssistantContent::ToolCall(ToolCall {
                id,
                name,
                arguments,
                ..Default::default()
            }),
        }
    }
}

/// Port of `bytesToBase64` over the redacted chunk buffer.
fn bytes_to_base64(chunks: &[Vec<u8>]) -> String {
    use base64::Engine;
    let joined: Vec<u8> = chunks.concat();
    base64::engine::general_purpose::STANDARD.encode(joined)
}

/// Port of `base64ToBytes`; a non-base64 payload yields no bytes.
fn base64_to_bytes(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

fn now_millis() -> i64 {
    crate::ai::auth::resolve::now_millis()
}

/// `new URL(url).hostname` for the URL shapes Bedrock base URLs take.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_hostname(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split('/').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// Port of `getModelMatchCandidates`: the lowercased id/name plus the
/// separator-normalized variants (application inference profile support).
fn model_match_candidates(model_id: &str, model_name: Option<&str>) -> Vec<String> {
    let mut values = vec![model_id.to_string()];
    if let Some(name) = model_name {
        values.push(name.to_string());
    }
    values
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let normalized = lower
                .chars()
                .map(|c| {
                    if c.is_whitespace() || matches!(c, '_' | '.' | ':') {
                        '-'
                    } else {
                        c
                    }
                })
                .collect::<String>();
            vec![lower, normalized]
        })
        .collect()
}

fn supports_adaptive_thinking(model_id: &str, model_name: Option<&str>) -> bool {
    model_match_candidates(model_id, model_name)
        .iter()
        .any(|s| {
            s.contains("opus-4-6")
                || s.contains("opus-4-7")
                || s.contains("opus-4-8")
                || s.contains("opus-5")
                || s.contains("sonnet-4-6")
                || s.contains("sonnet-5")
                || s.contains("fable-5")
        })
}

fn model_thinking_level(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
    model_match_candidates(&model.id, Some(&model.name))
        .iter()
        .any(|s| {
            s.contains("opus-4-7")
                || s.contains("opus-4-8")
                || s.contains("opus-5")
                || s.contains("sonnet-5")
                || s.contains("fable-5")
        })
}

fn map_thinking_level_to_effort(model: &Model, level: Option<ThinkingLevel>) -> String {
    if level == Some(ThinkingLevel::Xhigh) && supports_native_xhigh_effort(model) {
        return "xhigh".to_string();
    }
    if let Some(level) = level
        && let Some(mapped) = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&model_thinking_level(level)))
        && let Some(mapped) = mapped.as_deref()
    {
        return match mapped {
            "low" | "medium" | "high" | "xhigh" | "max" => mapped.to_string(),
            _ => "high".to_string(),
        };
    }
    match level {
        Some(ThinkingLevel::Minimal) | Some(ThinkingLevel::Low) => "low".to_string(),
        Some(ThinkingLevel::Medium) => "medium".to_string(),
        _ => "high".to_string(),
    }
}

/// Port of `resolveCacheRetention`: defaults to "short", `PI_CACHE_RETENTION`
/// selects "long" for compatibility.
fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    if let Some(cache_retention) = cache_retention {
        return cache_retention;
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn supports_prompt_caching(model: &Model, env: Option<&ProviderEnv>) -> bool {
    let candidates = model_match_candidates(&model.id, Some(&model.name));
    let has_claude_ref = candidates.iter().any(|s| s.contains("claude"));
    if !has_claude_ref {
        return get_provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    candidates.iter().any(|s| {
        s.contains("fable-5")
            || s.contains("opus-5")
            || s.contains("sonnet-5")
            || s.contains("-4-")
            || s.contains("claude-3-7-sonnet")
            || s.contains("claude-3-5-haiku")
    })
}

fn supports_thinking_signature(model: &Model) -> bool {
    is_anthropic_claude_model(model)
}

fn cache_point_block(retention: CacheRetention) -> Value {
    match retention {
        CacheRetention::Long => json!({"cachePoint": {"type": "default", "ttl": "ONE_HOUR"}}),
        _ => json!({"cachePoint": {"type": "default"}}),
    }
}

/// Port of `buildSystemPrompt`.
fn build_system_prompt(
    system_prompt: Option<&str>,
    model: &Model,
    cache_retention: CacheRetention,
    env: Option<&ProviderEnv>,
) -> Option<Vec<Value>> {
    let system_prompt = system_prompt?;
    let mut blocks = vec![json!({ "text": sanitize_surrogates(system_prompt) })];
    if cache_retention != CacheRetention::None && supports_prompt_caching(model, env) {
        blocks.push(cache_point_block(cache_retention));
    }
    Some(blocks)
}

/// Port of `normalizeToolCallId`.
fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

fn create_non_blank_text_block(text: &str) -> Option<Value> {
    let sanitized = sanitize_surrogates(text);
    (!sanitized.trim().is_empty()).then(|| json!({ "text": sanitized }))
}

fn create_required_text_block(text: &str) -> Value {
    create_non_blank_text_block(text).unwrap_or(json!({ "text": EMPTY_TEXT_PLACEHOLDER }))
}

/// Port of `sanitizeBedrockDocument`: drops empty keys, recurses.
fn sanitize_bedrock_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_bedrock_document).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, nested)| (key.clone(), sanitize_bedrock_document(nested)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Port of `createImageBlock`; unknown mime types error like the TS path.
fn create_image_block(mime_type: &str, data: &str) -> Value {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => panic!("Unknown image type: {other}"),
    };
    let bytes = base64_to_bytes(data).unwrap_or_default();
    json!({ "format": format, "source": { "bytes": bytes } })
}

fn convert_tool_result_content(content: &[BlockContent]) -> Vec<Value> {
    let mut result = Vec::new();
    for c in content {
        match c {
            BlockContent::Image(image) => result.push(json!({
                "image": create_image_block(&image.mime_type, &image.data)
            })),
            BlockContent::Text(text) => {
                if let Some(block) = create_non_blank_text_block(&text.text) {
                    result.push(block);
                }
            }
        }
    }
    if result.is_empty() {
        result.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
    }
    result
}

/// Port of `convertMessages`.
fn convert_messages(
    context: &Context,
    model: &Model,
    cache_retention: CacheRetention,
    env: Option<&ProviderEnv>,
) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    let transformed = transform_messages(
        &context.messages,
        model,
        Some(&|id: &str, _message: &AssistantMessage| normalize_tool_call_id(id)),
    );

    let mut messages = transformed.into_iter().peekable();
    while let Some(message) = messages.next() {
        match &message {
            Message::User(user) => {
                let mut content: Vec<Value> = Vec::new();
                match &user.content {
                    UserContent::Text(text) => content.push(create_required_text_block(text)),
                    UserContent::Blocks(items) => {
                        for c in items {
                            match c {
                                BlockContent::Text(text) => {
                                    if let Some(block) = create_non_blank_text_block(&text.text) {
                                        content.push(block);
                                    }
                                }
                                BlockContent::Image(image) => {
                                    content.push(create_image_block(&image.mime_type, &image.data))
                                }
                            }
                        }
                        if content.is_empty() {
                            content.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                        }
                    }
                }
                result.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                // Bedrock rejects empty assistant content arrays.
                if assistant.content.is_empty() {
                    continue;
                }
                let mut blocks: Vec<Value> = Vec::new();
                for c in &assistant.content {
                    match c {
                        AssistantContent::Text(text) => {
                            if let Some(block) = create_non_blank_text_block(&text.text) {
                                blocks.push(block);
                            }
                        }
                        AssistantContent::ToolCall(call) => blocks.push(json!({
                            "toolUse": {
                                "toolUseId": call.id,
                                "name": call.name,
                                "input": sanitize_bedrock_document(&Value::Object(call.arguments.clone())),
                            }
                        })),
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted.unwrap_or(false) {
                                if let Some(bytes) = thinking
                                    .thinking_signature
                                    .as_deref()
                                    .and_then(base64_to_bytes)
                                    .filter(|bytes| !bytes.is_empty())
                                {
                                    blocks.push(json!({
                                        "reasoningContent": { "redactedContent": bytes }
                                    }));
                                }
                                continue;
                            }
                            let thinking_text = sanitize_surrogates(&thinking.thinking);
                            if thinking_text.trim().is_empty() {
                                continue;
                            }
                            if supports_thinking_signature(model) {
                                let signature = thinking
                                    .thinking_signature
                                    .as_deref()
                                    .unwrap_or_default()
                                    .trim()
                                    .to_string();
                                if signature.is_empty() {
                                    blocks.push(json!({ "text": thinking_text }));
                                } else {
                                    blocks.push(json!({
                                        "reasoningContent": {
                                            "reasoningText": {
                                                "text": thinking_text,
                                                "signature": signature,
                                            }
                                        }
                                    }));
                                }
                            } else {
                                blocks.push(json!({
                                    "reasoningContent": { "reasoningText": { "text": thinking_text } }
                                }));
                            }
                        }
                    }
                }
                if blocks.is_empty() {
                    continue;
                }
                result.push(json!({ "role": "assistant", "content": blocks }));
            }
            Message::ToolResult(tool_result) => {
                // Consecutive tool results merge into one user message.
                let mut tool_results = vec![tool_result_block(tool_result)];
                while let Some(next) = messages.peek() {
                    match next {
                        Message::ToolResult(next_result) => {
                            tool_results.push(tool_result_block(next_result));
                            messages.next();
                        }
                        _ => break,
                    }
                }
                result.push(json!({ "role": "user", "content": tool_results }));
            }
        }
    }

    // Cache point on the last user message for supported Claude models.
    if cache_retention != CacheRetention::None
        && supports_prompt_caching(model, env)
        && let Some(last) = result.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.push(cache_point_block(cache_retention));
    }
    result
}

fn tool_result_block(tool_result: &crate::ai::types::ToolResultMessage) -> Value {
    json!({
        "toolResult": {
            "toolUseId": tool_result.tool_call_id,
            "content": convert_tool_result_content(&tool_result.content),
            "status": if tool_result.is_error { "error" } else { "success" },
        }
    })
}

/// Port of `convertToolConfig`.
fn convert_tool_config(
    tools: Option<&Vec<Tool>>,
    tool_choice: Option<&ToolChoice>,
    supports_strict_mode: bool,
) -> Option<Value> {
    let tools = tools?;
    if tools.is_empty() {
        return None;
    }
    if matches!(tool_choice, Some(ToolChoice::None)) {
        return None;
    }
    let bedrock_tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)
                .ok()
                .flatten();
            let parameters = get_json_schema_tool_parameters(tool, strict.unwrap_or(false))
                .unwrap_or_else(|_| tool.parameters.clone());
            let mut tool_spec = json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": { "json": parameters },
            });
            if strict == Some(true) {
                tool_spec["strict"] = json!(true);
            }
            json!({ "toolSpec": tool_spec })
        })
        .collect();
    let tool_choice_value = match tool_choice {
        Some(ToolChoice::Auto) => json!({ "auto": {} }),
        Some(ToolChoice::Any) => json!({ "any": {} }),
        Some(ToolChoice::Tool { name }) => json!({ "tool": { "name": name } }),
        _ => Value::Null,
    };
    Some(json!({
        "tools": bedrock_tools,
        "toolChoice": tool_choice_value,
    }))
}

/// Port of `buildAdditionalModelRequestFields`.
fn build_additional_model_request_fields(model: &Model, options: &BedrockOptions) -> Option<Value> {
    options.reasoning?;
    if !model.reasoning {
        return None;
    }
    if !is_anthropic_claude_model(model) {
        return None;
    }
    // GovCloud Bedrock rejects the Claude thinking.display field.
    let display = if is_gov_cloud_bedrock_target(model, options) {
        None
    } else {
        Some(
            match options
                .thinking_display
                .unwrap_or(BedrockThinkingDisplay::Summarized)
            {
                BedrockThinkingDisplay::Summarized => "summarized",
                BedrockThinkingDisplay::Omitted => "omitted",
            },
        )
    };
    let mut result = serde_json::Map::new();
    if supports_adaptive_thinking(&model.id, Some(&model.name)) {
        let mut thinking = json!({ "type": "adaptive" });
        if let Some(display) = display {
            thinking["display"] = json!(display);
        }
        result.insert("thinking".to_string(), thinking);
        result.insert(
            "output_config".to_string(),
            json!({ "effort": map_thinking_level_to_effort(model, options.reasoning) }),
        );
    } else {
        let default_budget = |level: ThinkingLevel| match level {
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16384,
        };
        // Custom budgets only cover token-based levels through high.
        let budget = match options.reasoning {
            Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => options
                .thinking_budgets
                .as_ref()
                .and_then(|budgets| budgets.high)
                .unwrap_or(default_budget(options.reasoning.unwrap())),
            level => options
                .thinking_budgets
                .as_ref()
                .and_then(|budgets| budget_for_level(budgets, level.unwrap()))
                .unwrap_or_else(|| default_budget(level.unwrap())),
        };
        let mut thinking = json!({ "type": "enabled", "budget_tokens": budget });
        if let Some(display) = display {
            thinking["display"] = json!(display);
        }
        result.insert("thinking".to_string(), thinking);
    }
    if !supports_adaptive_thinking(&model.id, Some(&model.name))
        && options.interleaved_thinking.unwrap_or(true)
    {
        result.insert(
            "anthropic_beta".to_string(),
            json!(["interleaved-thinking-2025-05-14"]),
        );
    }
    Some(Value::Object(result))
}

fn budget_for_level(budgets: &ThinkingBudgets, level: ThinkingLevel) -> Option<u64> {
    match level {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High => budgets.high,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => None,
    }
}

/// Port of the region/config helpers shared with the wire layer.
pub(crate) fn get_configured_bedrock_region(
    region: Option<&str>,
    env: Option<&ProviderEnv>,
) -> Option<String> {
    region
        .map(str::to_string)
        .or_else(|| get_provider_env_value("AWS_REGION", env))
        .or_else(|| get_provider_env_value("AWS_DEFAULT_REGION", env))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn get_standard_bedrock_endpoint_region(base_url: Option<&str>) -> Option<String> {
    let base_url = base_url?;
    let hostname = parse_hostname(base_url)?;
    let lower = hostname.to_lowercase();
    let rest = lower
        .strip_prefix("bedrock-runtime")?
        .strip_suffix(".amazonaws.com")
        .or_else(|| {
            lower
                .strip_prefix("bedrock-runtime")?
                .strip_suffix(".amazonaws.com.cn")
        })?;
    // The optional -fips suffix precedes the region.
    let region = rest.strip_prefix("-fips").unwrap_or(rest);
    let region = region.strip_prefix('.').unwrap_or(region);
    (!region.is_empty())
        .then(|| region.to_string())
        .filter(|region| {
            region
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn should_use_explicit_bedrock_endpoint(
    base_url: &str,
    configured_region: Option<&str>,
    has_ambient_configured_profile: bool,
) -> bool {
    let endpoint_region = get_standard_bedrock_endpoint_region(Some(base_url));
    if endpoint_region.is_none() {
        return true;
    }
    configured_region.is_none() && !has_ambient_configured_profile
}

pub(crate) fn is_gov_cloud_bedrock_target(model: &Model, options: &BedrockOptions) -> bool {
    let region =
        get_configured_bedrock_region(options.region.as_deref(), options.base.base.env.as_ref());
    if region
        .as_deref()
        .is_some_and(|region| region.to_lowercase().starts_with("us-gov-"))
    {
        return true;
    }
    let model_id = model.id.to_lowercase();
    model_id.starts_with("us-gov.") || model_id.starts_with("arn:aws-us-gov:")
}

/// Port of the request-body assembly (`commandInput`).
pub(crate) fn build_command_input(
    model: &Model,
    context: &Context,
    options: &BedrockOptions,
) -> Value {
    let cache_retention =
        resolve_cache_retention(options.base.cache_retention, options.base.base.env.as_ref());
    let inference_max_tokens = options.base.max_tokens.or({
        if is_anthropic_claude_model(model) {
            Some(model.max_tokens)
        } else {
            None
        }
    });
    let mut inference_config = serde_json::Map::new();
    if let Some(max_tokens) = inference_max_tokens {
        inference_config.insert("maxTokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = options.base.temperature {
        inference_config.insert("temperature".to_string(), json!(temperature));
    }
    let mut input = serde_json::Map::new();
    input.insert("modelId".to_string(), json!(model.id));
    input.insert(
        "messages".to_string(),
        json!(convert_messages(
            context,
            model,
            cache_retention,
            options.base.base.env.as_ref()
        )),
    );
    if let Some(system) = build_system_prompt(
        context.system_prompt.as_deref(),
        model,
        cache_retention,
        options.base.base.env.as_ref(),
    ) {
        input.insert("system".to_string(), json!(system));
    }
    input.insert(
        "inferenceConfig".to_string(),
        Value::Object(inference_config),
    );
    if let Some(tool_config) = convert_tool_config(
        context.tools.as_ref(),
        options.tool_choice.as_ref(),
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_strict_mode)
            .unwrap_or(false),
    ) {
        input.insert("toolConfig".to_string(), tool_config);
    }
    if let Some(fields) = build_additional_model_request_fields(model, options) {
        input.insert("additionalModelRequestFields".to_string(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        input.insert(
            "requestMetadata".to_string(),
            json!(metadata.iter().collect::<BTreeMap<_, _>>()),
        );
    }
    Value::Object(input)
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

/// Port of `mapStopReason`.
fn map_stop_reason(reason: Option<&str>) -> (crate::ai::types::StopReason, Option<String>) {
    match reason {
        Some("end_turn") | Some("stop_sequence") => (crate::ai::types::StopReason::Stop, None),
        Some("max_tokens") | Some("model_context_window_exceeded") => {
            (crate::ai::types::StopReason::Length, None)
        }
        Some("tool_use") => (crate::ai::types::StopReason::ToolUse, None),
        Some(other) => (
            crate::ai::types::StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
        None => (crate::ai::types::StopReason::Error, None),
    }
}

/// Human-readable prefixes for the Bedrock SDK exception names.
fn bedrock_error_prefix(name: &str) -> &str {
    match name {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        other => other,
    }
}

/// Port of `formatBedrockError`.
pub(crate) fn format_bedrock_error(error: &BedrockError) -> String {
    let norm = normalize_provider_error(ProviderErrorParts {
        status: error.status,
        body: error.body.clone(),
        message: error.message.clone(),
    });
    let core = match (&norm.status, &norm.body) {
        (Some(status), Some(body)) if !norm.message_carries_body => format!("{status}: {body}"),
        _ => norm.message.clone(),
    };
    let data_retention_hint = if core.to_lowercase().contains("data retention mode") {
        " See https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html for supported data retention modes.".to_string()
    } else {
        String::new()
    };
    if error.service_exception {
        format!(
            "{}: {}{}",
            bedrock_error_prefix(&error.name),
            core,
            data_retention_hint
        )
    } else {
        format!("{core}{data_retention_hint}")
    }
}

/// Over-long header values are dropped rather than truncated.
const MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS: usize = 200;

fn normalize_diagnostic_value(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// Modeled Bedrock errors all end in `Exception`, unlike transport names.
fn extract_bedrock_error_code(error: &BedrockError) -> Option<String> {
    if !error.name.ends_with("Exception") {
        return None;
    }
    normalize_diagnostic_value(Some(&error.name))
}

/// Port of `appendBedrockFailureDiagnostic`.
fn append_bedrock_failure_diagnostic(
    output: &mut AssistantMessage,
    error: &BedrockError,
    fallback_request_id: Option<&str>,
) {
    let mut details = serde_json::Map::new();
    if let Some(status) = error.http_status_code {
        details.insert("status".to_string(), json!(status));
    }
    if let Some(error_code) = extract_bedrock_error_code(error) {
        details.insert("errorCode".to_string(), json!(error_code));
    }
    let request_id = normalize_diagnostic_value(error.request_id.as_deref())
        .or_else(|| normalize_diagnostic_value(fallback_request_id));
    if let Some(request_id) = request_id {
        details.insert("requestId".to_string(), json!(request_id));
    }
    if details.is_empty() {
        return;
    }
    append_assistant_message_diagnostic(
        output,
        AssistantMessageDiagnostic {
            kind: "bedrock_response_failure".to_string(),
            timestamp: now_millis(),
            error: None,
            details: Some(Value::Object(details)),
        },
    );
}

/// The spawned dispatch outcome: success carries the finished message (plus
/// response request id); failure carries the partial message, the error, and
/// the response request id for the failure diagnostic.
type DispatchOutcome =
    Result<(AssistantMessage, Option<String>), (AssistantMessage, BedrockError, Option<String>)>;

struct EventDriver<'a> {
    model: &'a Model,
    stream: AssistantMessageEventStream,
    output: AssistantMessage,
    blocks: Vec<StreamingBlock>,
}

impl<'a> EventDriver<'a> {
    fn partial(&self) -> AssistantMessage {
        let mut partial = self.output.clone();
        partial.content = self
            .blocks
            .iter()
            .map(|block| block.clone().into_content())
            .collect();
        partial
    }

    fn block_position(&self, index: u64) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| block.index() == Some(index))
    }

    /// Port of `handleContentBlockStart`.
    fn handle_content_block_start(&mut self, event: &Value) {
        let index = event["contentBlockIndex"].as_u64().unwrap_or_default();
        let start = &event["start"];
        if let Some(tool_use) = start.get("toolUse").filter(|value| !value.is_null()) {
            self.blocks.push(StreamingBlock::ToolCall {
                index: Some(index),
                id: tool_use["toolUseId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                name: tool_use["name"].as_str().unwrap_or_default().to_string(),
                arguments: serde_json::Map::new(),
                partial_json: String::new(),
            });
            let content_index = self.blocks.len() - 1;
            self.stream.push(AssistantMessageEvent::ToolcallStart {
                content_index,
                partial: self.partial(),
            });
        }
    }

    /// Port of `handleContentBlockDelta`.
    fn handle_content_block_delta(&mut self, event: &Value) {
        let content_block_index = event["contentBlockIndex"].as_u64().unwrap_or_default();
        let delta = &event["delta"];

        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            let position = self.block_position(content_block_index);
            let position = match position {
                Some(position) => position,
                None => {
                    // Text blocks get no contentBlockStart event.
                    self.blocks.push(StreamingBlock::Text {
                        index: Some(content_block_index),
                        text: String::new(),
                    });
                    let content_index = self.blocks.len() - 1;
                    self.stream.push(AssistantMessageEvent::TextStart {
                        content_index,
                        partial: self.partial(),
                    });
                    content_index
                }
            };
            if let StreamingBlock::Text {
                text: block_text, ..
            } = &mut self.blocks[position]
            {
                block_text.push_str(text);
            }
            self.stream.push(AssistantMessageEvent::TextDelta {
                content_index: position,
                delta: text.to_string(),
                partial: self.partial(),
            });
            return;
        }

        if let Some(tool_use) = delta.get("toolUse").filter(|value| !value.is_null()) {
            let input = tool_use["input"].as_str().unwrap_or_default().to_string();
            let mut pushed = None;
            if let Some(position) = self.block_position(content_block_index)
                && let StreamingBlock::ToolCall {
                    partial_json,
                    arguments,
                    ..
                } = &mut self.blocks[position]
            {
                partial_json.push_str(&input);
                *arguments = parse_streaming_json(Some(partial_json))
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                pushed = Some(position);
            }
            if let Some(position) = pushed {
                self.stream.push(AssistantMessageEvent::ToolcallDelta {
                    content_index: position,
                    delta: input,
                    partial: self.partial(),
                });
            }
            return;
        }

        if let Some(reasoning) = delta
            .get("reasoningContent")
            .filter(|value| !value.is_null())
        {
            let position = self.block_position(content_block_index);
            let position = match position {
                Some(position) => position,
                None => {
                    self.blocks.push(StreamingBlock::Thinking {
                        index: Some(content_block_index),
                        thinking: String::new(),
                        thinking_signature: String::new(),
                        redacted: false,
                        redacted_chunks: Vec::new(),
                    });
                    let content_index = self.blocks.len() - 1;
                    self.stream.push(AssistantMessageEvent::ThinkingStart {
                        content_index,
                        partial: self.partial(),
                    });
                    content_index
                }
            };
            // Collect the events first; the partial snapshots must include the
            // just-applied deltas, so the pushes happen after the mutation.
            let mut deltas: Vec<(usize, String)> = Vec::new();
            let StreamingBlock::Thinking {
                thinking,
                thinking_signature,
                redacted,
                redacted_chunks,
                ..
            } = &mut self.blocks[position]
            else {
                return;
            };
            if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                thinking.push_str(text);
                deltas.push((position, text.to_string()));
            }
            // `thinkingSignature` holds either an Anthropic signature or an
            // opaque redacted payload, never both.
            if let Some(signature) = reasoning.get("signature").and_then(Value::as_str)
                && !*redacted
            {
                thinking_signature.push_str(signature);
            }
            if let Some(redacted_content) = reasoning.get("redactedContent") {
                let bytes = redacted_content_bytes(redacted_content);
                if !bytes.is_empty() {
                    if !*redacted {
                        *redacted = true;
                        thinking_signature.clear();
                        thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                        deltas.push((position, REDACTED_THINKING_PLACEHOLDER.to_string()));
                    }
                    redacted_chunks.push(bytes);
                }
            }
            for (content_index, delta) in deltas {
                self.stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: self.partial(),
                });
            }
        }
    }

    /// Port of `handleContentBlockStop`.
    fn handle_content_block_stop(&mut self, event: &Value) {
        let index = event["contentBlockIndex"].as_u64().unwrap_or_default();
        let Some(position) = self.block_position(index) else {
            return;
        };
        match &mut self.blocks[position] {
            StreamingBlock::Text { index, text } => {
                *index = None;
                let content = text.clone();
                self.stream.push(AssistantMessageEvent::TextEnd {
                    content_index: position,
                    content,
                    partial: self.partial(),
                });
            }
            StreamingBlock::Thinking {
                index, thinking, ..
            } => {
                *index = None;
                let content = thinking.clone();
                self.stream.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: position,
                    content,
                    partial: self.partial(),
                });
            }
            StreamingBlock::ToolCall {
                index,
                arguments,
                partial_json,
                ..
            } => {
                *index = None;
                *arguments = parse_streaming_json(Some(partial_json))
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                let tool_call = match self.blocks[position].clone().into_content() {
                    AssistantContent::ToolCall(call) => call,
                    _ => unreachable!(),
                };
                self.stream.push(AssistantMessageEvent::ToolcallEnd {
                    content_index: position,
                    tool_call,
                    partial: self.partial(),
                });
            }
        }
    }

    /// Port of `handleMetadata`.
    fn handle_metadata(&mut self, event: &Value) {
        let Some(usage) = event.get("usage").filter(|value| !value.is_null()) else {
            return;
        };
        let input = usage["inputTokens"].as_u64().unwrap_or_default();
        let output = usage["outputTokens"].as_u64().unwrap_or_default();
        self.output.usage.input = input;
        self.output.usage.output = output;
        self.output.usage.cache_read = usage["cacheReadInputTokens"].as_u64().unwrap_or_default();
        self.output.usage.cache_write = usage["cacheWriteInputTokens"].as_u64().unwrap_or_default();
        self.output.usage.total_tokens = usage["totalTokens"]
            .as_u64()
            .unwrap_or(self.output.usage.input + self.output.usage.output);
        self.output.usage.cost = calculate_cost(self.model, &mut self.output.usage);
    }
}

/// The AWS SDK hands `redactedContent` over as base64 in JSON; accept either
/// the base64 string form or a raw byte array.
fn redacted_content_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::String(text) => base64_to_bytes(text).unwrap_or_default(),
        Value::Array(bytes) => bytes
            .iter()
            .filter_map(|byte| byte.as_u64().map(|byte| byte as u8))
            .collect(),
        _ => Vec::new(),
    }
}

/// The mid-stream exception members, in the TS dispatch order.
fn stream_item_error(item: &Value) -> Option<BedrockError> {
    for (member, name) in [
        ("internalServerException", "InternalServerException"),
        ("modelStreamErrorException", "ModelStreamErrorException"),
        ("validationException", "ValidationException"),
        ("throttlingException", "ThrottlingException"),
        ("serviceUnavailableException", "ServiceUnavailableException"),
    ] {
        if let Some(exception) = item.get(member).filter(|value| !value.is_null()) {
            return Some(BedrockError {
                name: name.to_string(),
                message: exception["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                http_status_code: exception["$metadata"]["httpStatusCode"]
                    .as_u64()
                    .map(|v| v as u16),
                request_id: exception["$metadata"]["requestId"]
                    .as_str()
                    .map(str::to_string),
                service_exception: true,
                ..Default::default()
            });
        }
    }
    None
}

/// Port of the shared stream driver: runs the built request through
/// `onPayload`, dispatches, and drives the response items. `dispatch`
/// replaces the AWS SDK `client.send()` — it receives the (possibly
/// onPayload-replaced) command input.
fn drive_stream(
    model: &Model,
    context: &Context,
    options: &BedrockOptions,
    dispatch: impl FnOnce(
        Value,
    ) -> futures::future::BoxFuture<
        'static,
        Result<BedrockDispatchResponse, BedrockError>,
    > + Send
    + 'static,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let producer = stream.clone();
    tokio::spawn(async move {
        let outcome: DispatchOutcome = async {
            let output = AssistantMessage {
                api: "bedrock-converse-stream".to_string(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                timestamp: now_millis(),
                ..Default::default()
            };
            let mut command_input = build_command_input(&model, &context, &options);
            if let Some(on_payload) = options.base.base.on_payload.clone()
                && let Some(replacement) = on_payload(command_input.clone(), &model).await
            {
                command_input = replacement;
            }
            let response = match dispatch(command_input).await {
                Ok(mut response) => {
                    if let Some(error) = response.send_error.take() {
                        return Err((output, error, None));
                    }
                    response
                }
                Err(error) => return Err((output, error, None)),
            };
            let response_request_id = normalize_diagnostic_value(response.request_id.as_deref());
            if let Some(on_response) = options.base.base.on_response.clone() {
                // Raw HTTP headers when the transport observed them (the
                // deserialize-middleware path); otherwise the synthesized
                // `$metadata` fallback.
                let provider_response = match &response.raw_headers {
                    Some(raw_headers) => crate::ai::types::ProviderResponse {
                        status: response.http_status_code.unwrap_or(200),
                        headers: raw_headers.clone(),
                    },
                    None => {
                        let mut headers = BTreeMap::new();
                        if let Some(request_id) = &response.request_id {
                            headers.insert("x-amzn-requestid".to_string(), request_id.clone());
                        }
                        crate::ai::types::ProviderResponse {
                            status: response.http_status_code.unwrap_or(200),
                            headers,
                        }
                    }
                };
                on_response(&provider_response, &model).await;
            }

            let mut driver = EventDriver {
                model: &model,
                stream: producer.clone(),
                output,
                blocks: Vec::new(),
            };
            let result = match drive_items(&mut driver, &response.items) {
                Err(error) => Err(error),
                Ok(()) => match response.stream_error.clone() {
                    // The iterator throwing skips the post-loop checks.
                    Some(error) => Err(error),
                    None => post_loop_checks(&mut driver, &options),
                },
            };
            driver.output.content = driver
                .blocks
                .iter()
                .map(|block| block.clone().into_content())
                .collect();
            let output = driver.output;
            match result {
                Ok(()) => Ok((output, response_request_id)),
                Err(error) => Err((output, error, response_request_id)),
            }
        }
        .await;

        match outcome {
            Ok((output, _)) => {
                producer.push(AssistantMessageEvent::Done {
                    reason: done_reason(output.stop_reason),
                    message: output.clone(),
                });
                producer.end(Some(output));
            }
            Err((mut output, error, response_request_id)) => {
                let aborted = options
                    .base
                    .base
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_cancelled());
                output.stop_reason = if aborted {
                    crate::ai::types::StopReason::Aborted
                } else {
                    crate::ai::types::StopReason::Error
                };
                output.error_message = Some(format_bedrock_error(&error));
                if output.stop_reason == crate::ai::types::StopReason::Error {
                    // The mid-stream request id must survive from the response
                    // metadata; the thrown error carries none of its own.
                    append_bedrock_failure_diagnostic(
                        &mut output,
                        &error,
                        response_request_id.as_deref(),
                    );
                }
                producer.push(AssistantMessageEvent::Error {
                    reason: if aborted {
                        crate::ai::types::ErrorReason::Aborted
                    } else {
                        crate::ai::types::ErrorReason::Error
                    },
                    error: output.clone(),
                });
                producer.end(Some(output));
            }
        }
    });
    stream
}

fn done_reason(stop_reason: crate::ai::types::StopReason) -> crate::ai::types::DoneReason {
    match stop_reason {
        crate::ai::types::StopReason::Length => crate::ai::types::DoneReason::Length,
        crate::ai::types::StopReason::ToolUse => crate::ai::types::DoneReason::ToolUse,
        _ => crate::ai::types::DoneReason::Stop,
    }
}

fn drive_items(driver: &mut EventDriver<'_>, items: &[Value]) -> Result<(), BedrockError> {
    for item in items {
        if item
            .get("messageStart")
            .is_some_and(|value| !value.is_null())
        {
            if item["messageStart"]["role"].as_str() != Some("assistant") {
                return Err(BedrockError::transport(
                    "Unexpected assistant message start but got user message start instead",
                ));
            }
            driver.stream.push(AssistantMessageEvent::Start {
                partial: driver.partial(),
            });
        } else if item
            .get("contentBlockStart")
            .is_some_and(|value| !value.is_null())
        {
            driver.handle_content_block_start(&item["contentBlockStart"]);
        } else if item
            .get("contentBlockDelta")
            .is_some_and(|value| !value.is_null())
        {
            driver.handle_content_block_delta(&item["contentBlockDelta"]);
        } else if item
            .get("contentBlockStop")
            .is_some_and(|value| !value.is_null())
        {
            driver.handle_content_block_stop(&item["contentBlockStop"]);
        } else if item
            .get("messageStop")
            .is_some_and(|value| !value.is_null())
        {
            let raw = item["messageStop"]["stopReason"].as_str();
            driver.output.raw_stop_reason = raw.map(str::to_string);
            let (stop_reason, error_message) = map_stop_reason(raw);
            driver.output.stop_reason = stop_reason;
            if let Some(error_message) = error_message {
                driver.output.error_message = Some(error_message);
            }
        } else if item.get("metadata").is_some_and(|value| !value.is_null()) {
            driver.handle_metadata(&item["metadata"]);
        } else if let Some(error) = stream_item_error(item) {
            return Err(error);
        }
    }

    Ok(())
}

/// The post-loop validations; a mid-stream iterator throw skips them, like
/// the TS `for await` exiting via the exception.
fn post_loop_checks(
    driver: &mut EventDriver<'_>,
    options: &BedrockOptions,
) -> Result<(), BedrockError> {
    let cancelled = options
        .base
        .base
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_cancelled());
    if cancelled {
        return Err(BedrockError::transport("Request was aborted"));
    }
    if driver.output.stop_reason == crate::ai::types::StopReason::Pending {
        return Err(BedrockError::transport(
            "Bedrock stream ended without a stop reason",
        ));
    }
    if driver.output.stop_reason == crate::ai::types::StopReason::Error
        || driver.output.stop_reason == crate::ai::types::StopReason::Aborted
    {
        return Err(BedrockError::transport(
            driver
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_string()),
        ));
    }
    Ok(())
}

/// Port of the exported `stream`: dispatches over the wire transport.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&BedrockOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned().unwrap_or_default();
    let model_for_dispatch = model.clone();
    let options_for_dispatch = options.clone();
    drive_stream(model, context, &options, move |input| {
        let model = model_for_dispatch;
        let options = options_for_dispatch;
        Box::pin(async move { dispatch_wire(&model, &options, input).await })
    })
}

/// The `SimpleStreamOptions` → `BedrockOptions` adaptation shared by the
/// wire and mocked dispatch paths.
fn adapt_simple_options(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> BedrockOptions {
    let base = build_base_options(model, context, options, None);
    let mut bedrock = BedrockOptions {
        base,
        tool_choice: options.and_then(|options| options.tool_choice.clone()),
        ..Default::default()
    };
    let Some(reasoning) = options.and_then(|options| options.reasoning) else {
        return bedrock;
    };
    bedrock.reasoning = Some(reasoning);
    bedrock.thinking_budgets = options.and_then(|options| options.thinking_budgets.clone());
    if is_anthropic_claude_model(model) && !supports_adaptive_thinking(&model.id, Some(&model.name))
    {
        // Budget-based Claude: reserve thinking headroom from the output cap.
        let adjusted = adjust_max_tokens_for_thinking(
            bedrock.base.max_tokens,
            model.max_tokens,
            reasoning,
            bedrock.thinking_budgets.as_ref(),
        );
        let max_tokens = clamp_max_tokens_to_context(model, context, adjusted.0);
        bedrock.base.max_tokens = Some(max_tokens);
        let level = clamp_reasoning(Some(reasoning)).unwrap_or(ThinkingLevel::High);
        let budget = adjusted.1.min(max_tokens.saturating_sub(1024));
        let mut budgets = bedrock.thinking_budgets.clone().unwrap_or_default();
        set_budget(&mut budgets, level, budget);
        bedrock.thinking_budgets = Some(budgets);
    }
    bedrock
}

/// Port of the exported `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    stream(
        model,
        context,
        Some(&adapt_simple_options(model, context, options)),
    )
}

/// Port of the TS test seam: drive the pipeline from a mocked SDK dispatch
/// response. This is the surface the TypeScript bedrock tests exercise by
/// mocking `@aws-sdk/client-bedrock-runtime`.
pub fn stream_from_items(
    model: &Model,
    context: &Context,
    options: Option<&BedrockOptions>,
    response: BedrockDispatchResponse,
) -> AssistantMessageEventStream {
    let options = options.cloned().unwrap_or_default();
    drive_stream(model, context, &options, move |_command_input| {
        Box::pin(std::future::ready(Ok(response)))
    })
}

/// Port of `streamSimple`: adapts `SimpleStreamOptions` into
/// `BedrockOptions` (the thinking-budget adjustments) and dispatches.
pub fn stream_simple_from_items(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    response: BedrockDispatchResponse,
) -> AssistantMessageEventStream {
    let bedrock = adapt_simple_options(model, context, options);
    stream_from_items(model, context, Some(&bedrock), response)
}

fn set_budget(budgets: &mut ThinkingBudgets, level: ThinkingLevel, budget: u64) {
    match level {
        ThinkingLevel::Minimal => budgets.minimal = Some(budget),
        ThinkingLevel::Low => budgets.low = Some(budget),
        ThinkingLevel::Medium => budgets.medium = Some(budget),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            budgets.high = Some(budget)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::ProviderRequestOptions;
    use crate::ai::utils::http::{HttpFetchError, HttpResponse};

    // ------------------------------------------------------------------
    // Endpoint resolution (bedrock-endpoint-resolution.test.ts cases,
    // observed on the resolved dispatch config instead of the SDK
    // constructor).
    //
    // Ambient env vars leak between tests, so the ambient-profile cases pin
    // them explicitly per test.

    fn dispatch_config(model: &Model, options: BedrockOptions) -> BedrockDispatchConfig {
        resolve_dispatch_config(model, &options)
    }

    fn base_options() -> BedrockOptions {
        BedrockOptions {
            base: StreamOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn bedrock_model(id: &str) -> Model {
        crate::ai::providers::builtin::get_builtin_model("amazon-bedrock", id).unwrap()
    }

    /// Serializes the tests that mutate the process environment.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::ai::test_env_lock()
    }

    fn set_env(name: &str, value: &str) {
        // SAFETY: each call holds `env_lock` for the whole test.
        unsafe { std::env::set_var(name, value) }
    }

    fn remove_env(name: &str) {
        // SAFETY: each call holds `env_lock` for the whole test.
        unsafe { std::env::remove_var(name) }
    }

    #[test]
    fn assigns_eu_central_1_runtime_urls_to_builtin_eu_inference_profiles() {
        let model = bedrock_model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0");
        assert_eq!(
            model.base_url,
            "https://bedrock-runtime.eu-central-1.amazonaws.com"
        );
    }

    #[test]
    fn does_not_pin_standard_endpoints_when_region_is_configured() {
        let _guard = env_lock();
        set_env("AWS_REGION", "us-east-2");
        let model = bedrock_model("us.anthropic.claude-opus-4-8");
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_REGION");
        assert_eq!(config.region.as_deref(), Some("us-east-2"));
        assert_eq!(config.endpoint, None);
    }

    #[test]
    fn derives_region_from_a_builtin_eu_endpoint_when_nothing_is_configured() {
        let _guard = env_lock();
        remove_env("AWS_REGION");
        remove_env("AWS_DEFAULT_REGION");
        remove_env("AWS_PROFILE");
        let model = bedrock_model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0");
        let config = dispatch_config(&model, base_options());
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://bedrock-runtime.eu-central-1.amazonaws.com")
        );
        assert_eq!(config.region.as_deref(), Some("eu-central-1"));
    }

    #[test]
    fn handles_missing_regions_for_explicit_scoped_and_ambient_profiles() {
        let _guard = env_lock();
        remove_env("AWS_REGION");
        remove_env("AWS_DEFAULT_REGION");
        remove_env("AWS_PROFILE");
        let model = bedrock_model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0");

        let options = BedrockOptions {
            profile: Some("bedrock-profile".to_string()),
            ..base_options()
        };
        let config = dispatch_config(&model, options);
        assert_eq!(config.profile.as_deref(), Some("bedrock-profile"));
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://bedrock-runtime.eu-central-1.amazonaws.com")
        );
        assert_eq!(config.region.as_deref(), Some("eu-central-1"));

        let mut env = ProviderEnv::new();
        env.insert(
            "AWS_PROFILE".to_string(),
            "scoped-bedrock-profile".to_string(),
        );
        let options = BedrockOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    env: Some(env),
                    ..Default::default()
                },
                ..base_options().base
            },
            ..base_options()
        };
        let config = dispatch_config(&model, options);
        assert_eq!(config.profile.as_deref(), Some("scoped-bedrock-profile"));
        assert_eq!(config.region.as_deref(), Some("eu-central-1"));

        set_env("AWS_PROFILE", "ambient-bedrock-profile");
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_PROFILE");
        assert_eq!(config.profile.as_deref(), Some("ambient-bedrock-profile"));
        assert_eq!(config.endpoint, None);
        assert_eq!(config.region, None);
    }

    #[test]
    fn still_passes_custom_bedrock_endpoints_through() {
        let _guard = env_lock();
        set_env("AWS_REGION", "us-west-2");
        let mut model = bedrock_model("us.anthropic.claude-opus-4-8");
        model.base_url = "https://bedrock-vpc.example.com".to_string();
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_REGION");
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://bedrock-vpc.example.com")
        );
        assert_eq!(config.region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn extracts_region_from_inference_profile_arn_regardless_of_region() {
        let _guard = env_lock();
        set_env("AWS_REGION", "us-east-1");
        let mut model = bedrock_model("us.anthropic.claude-opus-4-8");
        model.id = "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123"
            .to_string();
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_REGION");
        assert_eq!(config.region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn extracts_region_from_govcloud_inference_profile_arn() {
        let _guard = env_lock();
        set_env("AWS_REGION", "us-east-1");
        let mut model = bedrock_model("us.anthropic.claude-opus-4-8");
        model.id =
            "arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:application-inference-profile/abc123"
                .to_string();
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_REGION");
        assert_eq!(config.region.as_deref(), Some("us-gov-west-1"));
    }

    #[test]
    fn uses_the_generic_api_key_option_as_a_bedrock_bearer_token() {
        let model = bedrock_model("us.anthropic.claude-opus-4-8");
        let options = BedrockOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("bedrock-api-key".to_string()),
                    ..Default::default()
                },
                ..base_options().base
            },
            ..base_options()
        };
        let config = dispatch_config(&model, options);
        assert_eq!(config.bearer_token.as_deref(), Some("bedrock-api-key"));
    }

    // ------------------------------------------------------------------
    // Custom headers (bedrock-custom-headers.test.ts; the middleware's
    // apply behavior observed on the outgoing header list).

    fn apply(custom: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut headers = vec![
            ("authorization".to_string(), "real-auth".to_string()),
            ("x-amz-date".to_string(), "real-date".to_string()),
            ("host".to_string(), "real-host".to_string()),
        ];
        let custom: ProviderHeaders = custom
            .iter()
            .map(|(name, value)| (name.to_string(), Some(value.to_string())))
            .collect();
        apply_custom_headers(&mut headers, &custom);
        headers
    }

    #[test]
    fn injects_the_caller_header() {
        let headers = apply(&[("x-custom", "v")]);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(get("x-custom").as_deref(), Some("v"));
        assert_eq!(get("authorization").as_deref(), Some("real-auth"));
    }

    #[test]
    fn skips_reserved_headers_case_insensitively_while_applying_allowed_ones() {
        let headers = apply(&[
            ("authorization", "evil"),
            ("x-amz-date", "evil"),
            ("x-allowed", "ok"),
            ("Authorization", "evil2"),
            ("X-Amz-Date", "evil2"),
            ("HOST", "evil3"),
        ]);
        let mut names: Vec<String> = headers.iter().map(|(name, _)| name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "authorization".to_string(),
                "host".to_string(),
                "x-allowed".to_string(),
                "x-amz-date".to_string(),
            ]
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "x-allowed")
                .map(|(_, value)| value.clone())
                .as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn custom_header_entries_with_null_values_are_dropped() {
        let mut headers = vec![("x-real".to_string(), "1".to_string())];
        let mut custom = ProviderHeaders::new();
        custom.insert("x-suppressed".to_string(), None);
        apply_custom_headers(&mut headers, &custom);
        assert_eq!(headers, vec![("x-real".to_string(), "1".to_string())]);
    }

    // ------------------------------------------------------------------
    // Event-stream framing.

    /// Minimal encoder mirroring the AWS wire format.
    fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        fn put_u16(value: u16, out: &mut Vec<u8>) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn put_u32(value: u32, out: &mut Vec<u8>) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7); // string value
            put_u16(value.len() as u16, &mut header_bytes);
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let mut prelude = Vec::new();
        let total = 12 + header_bytes.len() + payload.len() + 4;
        put_u32(total as u32, &mut prelude);
        put_u32(header_bytes.len() as u32, &mut prelude);
        put_u32(0, &mut prelude); // prelude CRC (the decoder does not verify)
        let mut frame = Vec::new();
        frame.extend_from_slice(&prelude);
        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(payload);
        let mut crc = crc32fast::Hasher::new();
        crc.update(&frame);
        put_u32(crc.finalize(), &mut frame);
        frame
    }

    #[test]
    fn decodes_event_stream_frames_to_items_and_errors() {
        let event = encode_frame(
            &[(":message-type", "event"), (":event-type", "messageStart")],
            br#"{"messageStart":{"role":"assistant"}}"#,
        );
        let error = encode_frame(
            &[
                (":message-type", "error"),
                (":error-code", "ThrottlingException"),
                (":error-message", "slow down"),
            ],
            b"",
        );
        let mut body = event.clone();
        body.extend_from_slice(&error);
        let frames = decode_event_stream(&body).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(
            matches!(&frames[0], EventFrame::Event(payload) if payload["messageStart"]["role"] == json!("assistant"))
        );
        match &frames[1] {
            EventFrame::Error { code, message } => {
                assert_eq!(code, "ThrottlingException");
                assert_eq!(message.as_deref(), Some("slow down"));
            }
            other => panic!("expected error frame, got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_event_stream_frames() {
        assert!(decode_event_stream(&[0, 0, 0, 4]).is_err());
    }

    #[test]
    fn formats_amz_dates_from_epoch_millis() {
        let date = aws_amz_date_now();
        assert_eq!(date.len(), 16);
        assert!(date.ends_with('Z'));
        assert!(date.contains('T'));
    }

    // ------------------------------------------------------------------
    // Credential priority (bedrock-credentials.test.ts cases, observed on
    // the dispatch config).

    #[test]
    fn prefers_explicit_and_scoped_profiles_over_ambient_aws_access_keys() {
        let _guard = env_lock();
        set_env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        set_env("AWS_SECRET_ACCESS_KEY", "secretexample");
        let model = bedrock_model("us.anthropic.claude-opus-4-8");

        let options = BedrockOptions {
            profile: Some("explicit-profile".to_string()),
            ..base_options()
        };
        let config = dispatch_config(&model, options);
        assert_eq!(config.profile.as_deref(), Some("explicit-profile"));
        assert!(config.credentials.is_none());

        let mut env = ProviderEnv::new();
        env.insert("AWS_PROFILE".to_string(), "scoped-profile".to_string());
        let options = BedrockOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    env: Some(env),
                    ..Default::default()
                },
                ..base_options().base
            },
            ..base_options()
        };
        let config = dispatch_config(&model, options);
        assert_eq!(config.profile.as_deref(), Some("scoped-profile"));
        assert!(config.credentials.is_none());
        remove_env("AWS_ACCESS_KEY_ID");
        remove_env("AWS_SECRET_ACCESS_KEY");
    }

    #[test]
    fn uses_ambient_aws_access_keys_when_no_profile_is_configured() {
        let _guard = env_lock();
        remove_env("AWS_PROFILE");
        set_env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        set_env("AWS_SECRET_ACCESS_KEY", "secretexample");
        let model = bedrock_model("us.anthropic.claude-opus-4-8");
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_ACCESS_KEY_ID");
        remove_env("AWS_SECRET_ACCESS_KEY");
        assert_eq!(config.profile, None);
        assert_eq!(
            config.credentials,
            Some(("AKIAEXAMPLE".to_string(), "secretexample".to_string(), None))
        );
    }

    #[test]
    fn uses_ambient_aws_access_keys_when_only_an_ambient_profile_is_set() {
        let _guard = env_lock();
        set_env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        set_env("AWS_SECRET_ACCESS_KEY", "secretexample");
        set_env("AWS_PROFILE", "ambient-profile");
        let model = bedrock_model("us.anthropic.claude-opus-4-8");
        let config = dispatch_config(&model, base_options());
        remove_env("AWS_ACCESS_KEY_ID");
        remove_env("AWS_SECRET_ACCESS_KEY");
        remove_env("AWS_PROFILE");
        assert_eq!(config.profile.as_deref(), Some("ambient-profile"));
        assert_eq!(
            config.credentials,
            Some(("AKIAEXAMPLE".to_string(), "secretexample".to_string(), None))
        );
    }

    #[test]
    fn extracts_standard_endpoint_regions() {
        assert_eq!(
            get_standard_bedrock_endpoint_region(Some(
                "https://bedrock-runtime.us-east-1.amazonaws.com"
            )),
            Some("us-east-1".to_string())
        );
        assert_eq!(
            get_standard_bedrock_endpoint_region(Some(
                "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com"
            )),
            Some("us-gov-west-1".to_string())
        );
        assert_eq!(
            get_standard_bedrock_endpoint_region(Some(
                "https://bedrock-runtime.cn-north-1.amazonaws.com.cn"
            )),
            Some("cn-north-1".to_string())
        );
        assert_eq!(
            get_standard_bedrock_endpoint_region(Some("https://gateway.example/v1")),
            None
        );
        assert_eq!(get_standard_bedrock_endpoint_region(None), None);
    }

    #[test]
    fn pins_explicit_endpoints_only_without_region_or_profile() {
        assert!(should_use_explicit_bedrock_endpoint(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            None,
            false
        ));
        assert!(!should_use_explicit_bedrock_endpoint(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            Some("eu-west-1"),
            false
        ));
        assert!(!should_use_explicit_bedrock_endpoint(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            None,
            true
        ));
        // Custom endpoints are always pinned.
        assert!(should_use_explicit_bedrock_endpoint(
            "https://gateway.example",
            Some("us-east-1"),
            true
        ));
    }

    // ------------------------------------------------------------------
    // Remote credential providers (the AWS SDK default chain pieces:
    // web identity/IRSA and ECS container credentials).

    /// Serves one canned body per request and records the requests.
    struct CredFetch {
        body: String,
        requests: std::sync::Mutex<Vec<HttpRequest>>,
    }

    impl CredFetch {
        fn new(body: String) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                body,
                requests: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl HttpFetch for CredFetch {
        fn fetch<'a>(
            &'a self,
            request: HttpRequest,
        ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
            self.requests.lock().unwrap().push(request.clone());
            let body = self.body.clone();
            Box::pin(async move {
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
                })
            })
        }
    }

    fn clear_ambient_aws_env() {
        for name in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_BEDROCK_SKIP_AUTH",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_ROLE_ARN",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        ] {
            remove_env(name);
        }
    }

    fn irsa_env(token_file: &str) -> crate::ai::types::ProviderEnv {
        [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", token_file),
            ("AWS_ROLE_ARN", "arn:aws:iam::123:role/bedrock"),
            ("AWS_ROLE_SESSION_NAME", "pi-session"),
        ]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
    }

    fn write_web_identity_token() -> String {
        let path = std::env::temp_dir().join(format!(
            "pi-bedrock-irsa-{}-{}",
            std::process::id(),
            crate::ai::utils::uuid::uuidv7()
        ));
        std::fs::write(&path, "projected-token").expect("write token file");
        path.to_string_lossy().to_string()
    }

    fn wire_auth(
        model: &Model,
        options: &BedrockOptions,
        fetch: &std::sync::Arc<CredFetch>,
    ) -> impl std::future::Future<Output = WireAuth> {
        let model = model.clone();
        let options = options.clone();
        let fetch = std::sync::Arc::clone(fetch) as std::sync::Arc<dyn HttpFetch>;
        async move {
            resolve_wire_target(&model, &options, "20260830T000000Z", &fetch)
                .await
                .expect("wire target resolves")
                .auth
        }
    }

    // The env guard intentionally spans the await: the remote-credential
    // chain reads ambient env, so ambient-env tests must not interleave.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn web_identity_credentials_feed_the_sigv4_wire_auth() {
        let _guard = env_lock();
        clear_ambient_aws_env();
        crate::ai::utils::aws_credentials::clear_aws_credential_cache_for_tests();
        let token_file = write_web_identity_token();
        let sts_body = "<AssumeRoleWithWebIdentityResponse>\
             <AssumeRoleWithWebIdentityResult><Credentials>\
             <AccessKeyId>ASIAIRSA</AccessKeyId>\
             <SecretAccessKey>irsa-secret</SecretAccessKey>\
             <SessionToken>irsa-session</SessionToken>\
             <Expiration>2030-01-01T00:00:00Z</Expiration>\
             </Credentials></AssumeRoleWithWebIdentityResult>\
             </AssumeRoleWithWebIdentityResponse>"
            .to_string();
        let fetch = CredFetch::new(sts_body);
        let mut options = base_options();
        options.base.base.env = Some(irsa_env(&token_file));

        match wire_auth(
            &bedrock_model("us.anthropic.claude-opus-4-8"),
            &options,
            &fetch,
        )
        .await
        {
            WireAuth::SigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                assert_eq!(access_key_id, "ASIAIRSA");
                assert_eq!(secret_access_key, "irsa-secret");
                assert_eq!(session_token.as_deref(), Some("irsa-session"));
            }
            other => panic!("expected SigV4 auth, got {other:?}"),
        }
        let _ = std::fs::remove_file(&token_file);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn static_env_keys_win_over_web_identity() {
        let _guard = env_lock();
        clear_ambient_aws_env();
        crate::ai::utils::aws_credentials::clear_aws_credential_cache_for_tests();
        let token_file = write_web_identity_token();
        // The canned STS body would never satisfy a static-key resolution;
        // any fetch means the chain fell through incorrectly.
        let fetch = CredFetch::new("{\"AccessKeyId\":\"bad\"}".to_string());
        let mut options = base_options();
        options.base.base.env = Some(irsa_env(&token_file));
        options
            .base
            .base
            .env
            .as_mut()
            .unwrap()
            .insert("AWS_ACCESS_KEY_ID".to_string(), "AKIASTATIC".to_string());
        options.base.base.env.as_mut().unwrap().insert(
            "AWS_SECRET_ACCESS_KEY".to_string(),
            "static-secret".to_string(),
        );

        match wire_auth(
            &bedrock_model("us.anthropic.claude-opus-4-8"),
            &options,
            &fetch,
        )
        .await
        {
            WireAuth::SigV4 {
                access_key_id,
                secret_access_key,
                ..
            } => {
                assert_eq!(access_key_id, "AKIASTATIC");
                assert_eq!(secret_access_key, "static-secret");
            }
            other => panic!("expected SigV4 auth, got {other:?}"),
        }
        assert_eq!(fetch.request_count(), 0, "no remote provider consulted");
        let _ = std::fs::remove_file(&token_file);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn ecs_container_credentials_feed_the_sigv4_wire_auth() {
        let _guard = env_lock();
        clear_ambient_aws_env();
        crate::ai::utils::aws_credentials::clear_aws_credential_cache_for_tests();
        let ecs_body = serde_json::json!({
            "AccessKeyId": "ASIAECS",
            "SecretAccessKey": "ecs-secret",
            "Token": "ecs-session",
            "Expiration": "2030-01-01T00:00:00Z",
        })
        .to_string();
        let fetch = CredFetch::new(ecs_body);
        let mut options = base_options();
        options.base.base.env = Some(
            [("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds/abc")]
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        );

        match wire_auth(
            &bedrock_model("us.anthropic.claude-opus-4-8"),
            &options,
            &fetch,
        )
        .await
        {
            WireAuth::SigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                assert_eq!(access_key_id, "ASIAECS");
                assert_eq!(secret_access_key, "ecs-secret");
                assert_eq!(session_token.as_deref(), Some("ecs-session"));
            }
            other => panic!("expected SigV4 auth, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire transport
//
// The TypeScript implementation delegates to the AWS SDK (client config,
// SigV4/bearer auth, middleware, and the vnd.amazon.eventstream framing).
// The Rust port implements that protocol directly over the injectable
// `HttpFetch` transport.

use crate::ai::types::ProviderHeaders;
use crate::ai::utils::http::{HttpBody, HttpFetch, HttpRequest};
use crate::ai::utils::sigv4::{SigV4Credentials, SigV4Header, SigV4Request, host_of, sign_request};
use std::collections::BTreeMap as HeadersMap;

/// The client configuration the TS suite observes on the SDK constructor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BedrockDispatchConfig {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub bearer_token: Option<String>,
    /// Ambient static AWS keys applied to the client config. Absent when an
    /// explicit or scoped profile takes over the credential chain.
    pub credentials: Option<(String, String, Option<String>)>,
}

/// Port of the client-config resolution in `stream`.
pub(crate) fn resolve_dispatch_config(
    model: &Model,
    options: &BedrockOptions,
) -> BedrockDispatchConfig {
    let env = options.base.base.env.as_ref();
    let options_profile = options.profile.clone().or_else(|| {
        options
            .base
            .base
            .env
            .as_ref()
            .and_then(|env| env.get("AWS_PROFILE").cloned())
    });
    let mut config = BedrockDispatchConfig {
        profile: options_profile
            .clone()
            .or_else(|| get_provider_env_value("AWS_PROFILE", env)),
        ..Default::default()
    };
    let configured_region = get_configured_bedrock_region(options.region.as_deref(), env);
    let has_ambient_configured_profile = get_provider_env_value("AWS_PROFILE", None).is_some();
    let endpoint_region = get_standard_bedrock_endpoint_region(Some(&model.base_url));
    let use_explicit_endpoint = should_use_explicit_bedrock_endpoint(
        &model.base_url,
        configured_region.as_deref(),
        has_ambient_configured_profile,
    );
    if use_explicit_endpoint {
        config.endpoint = Some(model.base_url.clone());
    }
    // A profile explicitly configured through pi's auth flow must win over
    // ambient AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY (#6957).
    if options_profile.is_none() {
        config.credentials = static_credentials(env);
    }

    // Region resolution: ARN-embedded > explicit option > env vars >
    // endpoint-derived > us-east-1 default (unless an ambient profile owns
    // region resolution).
    if let Some(arn_region) = arn_region(&model.id) {
        config.region = Some(arn_region);
    } else if let Some(configured) = configured_region {
        config.region = Some(configured);
    } else if let Some(endpoint_region) = endpoint_region.filter(|_| use_explicit_endpoint) {
        config.region = Some(endpoint_region);
    } else if !has_ambient_configured_profile {
        config.region = Some("us-east-1".to_string());
    }

    let skip_auth = get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1");
    let bearer_token = options
        .bearer_token
        .clone()
        .or_else(|| options.base.base.api_key.clone())
        .or_else(|| get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env))
        .filter(|_| !skip_auth);
    config.bearer_token = bearer_token;
    config
}

/// The region embedded in a bedrock ARN model id
/// (`arn:aws[-gov]:bedrock:REGION:...`).
fn arn_region(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("arn:")?;
    let partition_end = rest.find(':')?;
    let rest = &rest[partition_end + 1..];
    if rest.split(':').next()? != "bedrock" {
        return None;
    }
    let region = rest.split(':').nth(1)?;
    (!region.is_empty()).then(|| region.to_string())
}

/// Port of the custom-headers middleware behavior: reserved SigV4/auth
/// headers are skipped case-insensitively; others are applied over the
/// request headers.
pub(crate) fn apply_custom_headers(
    request_headers: &mut Vec<(String, String)>,
    custom: &ProviderHeaders,
) {
    for (key, value) in custom {
        let Some(value) = value else { continue };
        let lower = key.to_lowercase();
        if lower.starts_with("x-amz-") || lower == "authorization" || lower == "host" {
            continue;
        }
        request_headers.retain(|(name, _)| name.to_lowercase() != lower);
        request_headers.push((key.clone(), value.clone()));
    }
}

/// Static credentials from the provider env.
fn static_credentials(env: Option<&ProviderEnv>) -> Option<(String, String, Option<String>)> {
    let access_key_id = get_provider_env_value("AWS_ACCESS_KEY_ID", env)?;
    let secret_access_key = get_provider_env_value("AWS_SECRET_ACCESS_KEY", env)?;
    Some((
        access_key_id,
        secret_access_key,
        get_provider_env_value("AWS_SESSION_TOKEN", env),
    ))
}

/// Credentials for the configured profile from `~/.aws/credentials`.
fn profile_credentials(
    profile: &str,
    home_env: Option<&str>,
) -> Option<(String, String, Option<String>)> {
    let home = match home_env {
        Some(home) => Some(home.to_string()),
        None => std::env::var("HOME").ok(),
    }?;
    let contents = std::fs::read_to_string(format!("{home}/.aws/credentials")).ok()?;
    let in_profile = |section: &str| section == profile || section == format!("profile {profile}");
    parse_credentials_ini(&contents, in_profile)
}

fn parse_credentials_ini(
    contents: &str,
    in_profile: impl Fn(&str) -> bool,
) -> Option<(String, String, Option<String>)> {
    let mut current = String::new();
    let mut access_key = None;
    let mut secret_key = None;
    let mut token = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if access_key.is_some() && secret_key.is_some() && in_profile(&current) {
                break;
            }
            current = line[1..line.len() - 1].trim().to_string();
        } else if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            if !in_profile(&current) {
                continue;
            }
            match key {
                "aws_access_key_id" => access_key = Some(value.to_string()),
                "aws_secret_access_key" => secret_key = Some(value.to_string()),
                "aws_session_token" => token = Some(value.to_string()),
                _ => {}
            }
        }
    }
    Some((access_key?, secret_key?, token))
}

/// The resolved request URL and auth inputs.
struct WireTarget {
    url: String,
    host: String,
    path: String,
    region: String,
    auth: WireAuth,
}

#[derive(Debug)]
enum WireAuth {
    Bearer(String),
    SigV4 {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

async fn resolve_wire_target(
    model: &Model,
    options: &BedrockOptions,
    now_amz_date: &str,
    fetch: &std::sync::Arc<dyn HttpFetch>,
) -> Result<WireTarget, BedrockError> {
    let env = options.base.base.env.as_ref();
    let config = resolve_dispatch_config(model, options);
    let region = config
        .region
        .clone()
        .or_else(|| {
            // An ambient profile owns region resolution; the shared config file
            // may pin one, else default.
            profile_region(config.profile.as_deref())
        })
        .unwrap_or_else(|| "us-east-1".to_string());
    let endpoint = config.endpoint.clone().unwrap_or_else(|| {
        let suffix = if region.starts_with("cn-") {
            "amazonaws.com.cn"
        } else {
            "amazonaws.com"
        };
        format!("https://bedrock-runtime.{region}.{suffix}")
    });
    let path = format!(
        "/model/{}/converse-stream",
        crate::ai::utils::sigv4::uri_encode(&model.id, true)
    );
    // An explicit/scoped profile owns the credential chain; an ambient
    // profile still lets ambient static keys through (TS #6957 semantics).
    let options_profile = options.profile.clone().or_else(|| {
        options
            .base
            .base
            .env
            .as_ref()
            .and_then(|env| env.get("AWS_PROFILE").cloned())
    });
    let auth = if let Some(token) = config.bearer_token {
        WireAuth::Bearer(token)
    } else if get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1") {
        WireAuth::SigV4 {
            access_key_id: "dummy-access-key".to_string(),
            secret_access_key: "dummy-secret-key".to_string(),
            session_token: None,
        }
    } else if options_profile.is_none()
        && let Some((access, secret, token)) = static_credentials(env)
    {
        WireAuth::SigV4 {
            access_key_id: access,
            secret_access_key: secret,
            session_token: token,
        }
    } else if options_profile.is_none()
        && let Some(result) =
            crate::ai::utils::aws_credentials::web_identity_credentials(fetch, env, &region).await
    {
        // The SDK default chain resolves web identity (IRSA) before the
        // shared config profiles and the container provider.
        let (access, secret, token) = result.map_err(BedrockError::transport)?;
        WireAuth::SigV4 {
            access_key_id: access,
            secret_access_key: secret,
            session_token: token,
        }
    } else if let Some(profile) = &config.profile
        && let Some((access, secret, token)) =
            profile_credentials(profile, std::env::var("HOME").ok().as_deref())
    {
        WireAuth::SigV4 {
            access_key_id: access,
            secret_access_key: secret,
            session_token: token,
        }
    } else if let Some(result) =
        crate::ai::utils::aws_credentials::ecs_container_credentials(fetch, env).await
    {
        let (access, secret, token) = result.map_err(BedrockError::transport)?;
        WireAuth::SigV4 {
            access_key_id: access,
            secret_access_key: secret,
            session_token: token,
        }
    } else {
        return Err(BedrockError::transport(
            "Could not resolve Bedrock credentials: configure AWS_BEARER_TOKEN_BEDROCK, \
             AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, an AWS profile, container/web-identity \
             credentials, or set AWS_BEDROCK_SKIP_AUTH=1",
        ));
    };
    let _ = now_amz_date;
    Ok(WireTarget {
        url: format!("{endpoint}{path}"),
        host: host_of(&endpoint),
        path,
        region,
        auth,
    })
}

fn profile_region(profile: Option<&str>) -> Option<String> {
    let profile = profile?;
    let home = std::env::var("HOME").ok()?;
    let contents = std::fs::read_to_string(format!("{home}/.aws/config")).ok()?;
    let mut current = String::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].trim().to_string();
        } else if current == format!("profile {profile}")
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "region"
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Dispatches the built command input over the wire: signs and sends the
/// converse-stream request, then decodes the vnd.amazon.eventstream frames.
async fn dispatch_wire(
    model: &Model,
    options: &BedrockOptions,
    input: Value,
) -> Result<BedrockDispatchResponse, BedrockError> {
    let fetch: std::sync::Arc<dyn HttpFetch> = options
        .base
        .base
        .fetch
        .clone()
        .unwrap_or_else(crate::ai::utils::reqwest_fetch::default_fetch);
    let amz_date = aws_amz_date_now();
    let target = resolve_wire_target(model, options, &amz_date, &fetch).await?;

    let body = serde_json::to_vec(&input)
        .map_err(|error| BedrockError::transport(format!("serialize request: {error}")))?;
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "accept".to_string(),
            "application/vnd.amazon.eventstream".to_string(),
        ),
        ("host".to_string(), target.host.clone()),
    ];
    if let Some(custom) = &options.base.base.headers {
        apply_custom_headers(&mut headers, custom);
    }
    match &target.auth {
        WireAuth::Bearer(token) => {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        WireAuth::SigV4 {
            access_key_id,
            secret_access_key,
            session_token,
        } => {
            let credentials = SigV4Credentials {
                access_key_id,
                secret_access_key,
                session_token: session_token.as_deref(),
            };
            let request = SigV4Request {
                method: "POST",
                path: &target.path,
                query: &[],
                headers: vec![
                    SigV4Header {
                        name: "content-type".to_string(),
                        value: "application/json".to_string(),
                    },
                    SigV4Header {
                        name: "host".to_string(),
                        value: target.host.clone(),
                    },
                ],
                body: &body,
            };
            let authorization =
                sign_request(&request, &credentials, &target.region, "bedrock", &amz_date);
            headers.push(("x-amz-date".to_string(), amz_date.clone()));
            if let Some(session_token) = session_token {
                headers.push(("x-amz-security-token".to_string(), session_token.clone()));
            }
            headers.push(("authorization".to_string(), authorization));
        }
    }

    let response = fetch
        .fetch(HttpRequest {
            signal: options.base.base.signal.clone(),
            method: crate::ai::utils::http::HttpMethod::Post,
            url: target.url,
            headers,
            body: HttpBody::Bytes(bytes::Bytes::from(body)),
        })
        .await
        .map_err(|error| BedrockError::transport(error.to_string()))?;

    let status = response.status;
    let raw_headers: HeadersMap<String, String> = response
        .headers
        .iter()
        .map(|(name, value)| (name.to_lowercase(), value.clone()))
        .collect();
    let body_bytes = response
        .bytes()
        .await
        .map_err(|error| BedrockError::transport(error.to_string()))?;
    if !(200..300).contains(&status) {
        let error_type = raw_headers
            .get("x-amzn-errortype")
            .map(|value| value.split(':').next().unwrap_or_default().to_string())
            .unwrap_or_default();
        let body_text = String::from_utf8_lossy(&body_bytes).to_string();
        return Err(BedrockError {
            name: error_type,
            message: body_text.clone(),
            body: (!body_text.trim().is_empty()).then_some(body_text),
            status: Some(status),
            http_status_code: Some(status),
            request_id: raw_headers.get("x-amzn-requestid").cloned(),
            service_exception: true,
        });
    }

    let request_id = raw_headers.get("x-amzn-requestid").cloned();
    let frames = decode_event_stream(&body_bytes)?;
    let mut items = Vec::new();
    let mut stream_error = None;
    for frame in frames {
        match frame {
            EventFrame::Event(payload) => items.push(payload),
            EventFrame::Error { code, message } => {
                stream_error = Some(BedrockError {
                    name: code,
                    message: message.unwrap_or_default(),
                    service_exception: true,
                    ..Default::default()
                });
            }
        }
    }
    Ok(BedrockDispatchResponse {
        http_status_code: Some(status),
        request_id,
        raw_headers: Some(raw_headers),
        items,
        send_error: None,
        stream_error,
    })
}

fn aws_amz_date_now() -> String {
    // UTC ISO-basic timestamp YYYYMMDDTHHMMSSZ from the epoch millis.
    let millis = crate::ai::auth::resolve::now_millis();
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 14_6096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u64, d as u64)
}

/// One decoded event-stream frame.
#[derive(Debug)]
enum EventFrame {
    Event(Value),
    Error {
        code: String,
        message: Option<String>,
    },
}

/// Decodes the AWS `application/vnd.amazon.eventstream` binary framing.
fn decode_event_stream(body: &[u8]) -> Result<Vec<EventFrame>, BedrockError> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        if offset + 16 > body.len() {
            return Err(BedrockError::transport("truncated event-stream prelude"));
        }
        let total = u32::from_be_bytes(body[offset..offset + 4].try_into().unwrap()) as usize;
        let headers_len =
            u32::from_be_bytes(body[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if total < 16 || offset + total > body.len() || headers_len + 16 > total {
            return Err(BedrockError::transport("invalid event-stream frame length"));
        }
        let headers_bytes = &body[offset + 12..offset + 12 + headers_len];
        let payload = &body[offset + 12 + headers_len..offset + total - 4];
        let headers = parse_event_headers(headers_bytes)?;
        let message_type = headers
            .get(":message-type")
            .cloned()
            .unwrap_or_else(|| "event".to_string());
        let frame = match message_type.as_str() {
            "error" => EventFrame::Error {
                code: headers.get(":error-code").cloned().unwrap_or_default(),
                message: headers.get(":error-message").cloned(),
            },
            _ => {
                let payload = String::from_utf8_lossy(payload);
                let json: Value = if payload.trim().is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&payload).map_err(|error| {
                        BedrockError::transport(format!("event payload: {error}"))
                    })?
                };
                EventFrame::Event(json)
            }
        };
        frames.push(frame);
        offset += total;
    }
    Ok(frames)
}

fn parse_event_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, BedrockError> {
    let mut headers = BTreeMap::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let name_len = bytes[offset] as usize;
        offset += 1;
        if offset + name_len + 1 > bytes.len() {
            return Err(BedrockError::transport("truncated event-stream header"));
        }
        let name = String::from_utf8_lossy(&bytes[offset..offset + name_len]).to_string();
        offset += name_len;
        let value_type = bytes[offset];
        offset += 1;
        let value = match value_type {
            7 => {
                let len =
                    u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
                offset += 2;
                if offset + len > bytes.len() {
                    return Err(BedrockError::transport(
                        "truncated event-stream header value",
                    ));
                }
                let value = String::from_utf8_lossy(&bytes[offset..offset + len]).to_string();
                offset += len;
                value
            }
            // Frame-level numeric headers (content-length etc.) are not
            // needed by the conversion.
            2 => {
                offset += 1;
                String::new()
            }
            3 => {
                offset += 2;
                String::new()
            }
            4 => {
                offset += 4;
                String::new()
            }
            5 | 8 => {
                offset += 8;
                String::new()
            }
            9 => {
                offset += 16;
                String::new()
            }
            6 => {
                let len =
                    u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
                offset += 2 + len;
                String::new()
            }
            0 | 1 => String::new(),
            other => {
                return Err(BedrockError::transport(format!(
                    "unknown event-stream header value type {other}"
                )));
            }
        };
        headers.insert(name, value);
    }
    Ok(headers)
}
