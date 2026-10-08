//! Port of `pi-core/ai/src/api/anthropic-messages.ts`.
//!
//! The TypeScript implementation drives the `@anthropic-ai/sdk` client; the
//! Rust port issues the same `POST {baseUrl}/v1/messages` request directly
//! through the injectable [`HttpFetch`](crate::ai::utils::http::HttpFetch)
//! transport, reproducing the SDK's wire behavior (auth headers, beta
//! headers, `anthropic-version`) while keeping the streaming event handling
//! 1:1. Tests inject canned responses through the `fetch` option, matching
//! the TS tests that inject mock SDK clients.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::ai::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::ai::api::github_copilot_headers::{
    build_copilot_dynamic_headers, has_copilot_vision_input,
};
use crate::ai::api::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::ai::api::transform_messages::transform_messages;
use crate::ai::models::calculate_cost;
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DoneReason,
    ErrorReason, Message, Model, ModelCompat, ProviderEnv, ProviderHeaders, SimpleStreamOptions,
    StopReason, StreamOptions, TextContent, ThinkingContent, Tool, ToolCall, ToolResultMessage,
    UserContent,
};
use crate::ai::utils::deferred_tools::split_deferred_tools;
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::ai::utils::provider_env::get_provider_env_value;
use crate::ai::utils::provider_retry::retry_http_request;
use crate::ai::utils::reqwest_fetch::default_fetch;
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;
use crate::ai::utils::sse::SseStream;

/// Resolve cache retention preference. Defaults to "short" and uses
/// PI_CACHE_RETENTION for backward compatibility.
fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// The `cache_control` block value: `{"type":"ephemeral"}` with an optional
/// `"ttl": "1h"`.
fn cache_control_value(retention: CacheRetention, model: &Model) -> Option<Value> {
    if retention == CacheRetention::None {
        return None;
    }
    let long_retention = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_long_cache_retention)
        .unwrap_or(true);
    let ttl = (retention == CacheRetention::Long && long_retention).then(|| json!("1h"));
    Some(match ttl {
        Some(ttl) => json!({"type": "ephemeral", "ttl": ttl}),
        None => json!({"type": "ephemeral"}),
    })
}

// Stealth mode: Mimic Claude Code's tool naming exactly
const CLAUDE_CODE_VERSION: &str = "2.1.75";

/// Claude Code 2.x tool names (canonical casing).
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
        .map(|tool| tool.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: Option<&[Tool]>) -> String {
    if let Some(tools) = tools
        && !tools.is_empty()
        && let Some(tool) = tools
            .iter()
            .find(|tool| tool.name.eq_ignore_ascii_case(name))
    {
        return tool.name.clone();
    }
    name.to_string()
}

/// Converts content blocks to the Anthropic API format: plain string when
/// there are no images, content blocks otherwise (with a placeholder text
/// block when only images are present). Port of `convertContentBlocks`.
fn convert_content_blocks(content: &[crate::ai::types::BlockContent]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, crate::ai::types::BlockContent::Image(_)));
    if !has_images {
        let text = content
            .iter()
            .filter_map(|block| match block {
                crate::ai::types::BlockContent::Text(text) => Some(text.text.as_str()),
                crate::ai::types::BlockContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Value::String(sanitize_surrogates(&text));
    }

    let mut blocks: Vec<Value> = content
        .iter()
        .map(|block| match block {
            crate::ai::types::BlockContent::Text(text) => json!({
                "type": "text",
                "text": sanitize_surrogates(&text.text),
            }),
            crate::ai::types::BlockContent::Image(image) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": image.data,
                },
            }),
        })
        .collect();

    let has_text = blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("text"));
    if !has_text {
        blocks.insert(
            0,
            json!({
                "type": "text",
                "text": "(see attached image)",
            }),
        );
    }

    Value::Array(blocks)
}

/// Port of `AnthropicEffort`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnthropicEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl AnthropicEffort {
    fn as_str(self) -> &'static str {
        match self {
            AnthropicEffort::Low => "low",
            AnthropicEffort::Medium => "medium",
            AnthropicEffort::High => "high",
            AnthropicEffort::Xhigh => "xhigh",
            AnthropicEffort::Max => "max",
        }
    }
}

/// Port of `AnthropicThinkingDisplay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}

impl AnthropicThinkingDisplay {
    fn as_str(self) -> &'static str {
        match self {
            AnthropicThinkingDisplay::Summarized => "summarized",
            AnthropicThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// Port of `AnthropicToolChoice`.
#[derive(Clone, Debug, PartialEq)]
pub enum AnthropicToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

/// Port of `AnthropicOptions`.
#[derive(Clone, Default)]
pub struct AnthropicOptions {
    pub base: StreamOptions,
    /// Enable extended thinking (adaptive or budget-based per model).
    pub thinking_enabled: Option<bool>,
    /// Token budget for extended thinking (older models only).
    pub thinking_budget_tokens: Option<u64>,
    /// Effort level for adaptive thinking models.
    pub effort: Option<AnthropicEffort>,
    /// How thinking content is returned.
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    /// Whether to request the interleaved thinking beta header.
    pub interleaved_thinking: Option<bool>,
    /// Anthropic tool choice behavior.
    pub tool_choice: Option<AnthropicToolChoice>,
}

const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

fn should_use_server_side_fallback_beta(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.allowed_fallback_models.as_ref())
        .is_some_and(|fallbacks| !fallbacks.is_empty())
}

/// The fully-resolved Anthropic compat flags with their defaults. Port of
/// `getAnthropicCompat`.
pub struct AnthropicCompatResolved {
    pub supports_eager_tool_input_streaming: bool,
    pub supports_long_cache_retention: bool,
    pub send_session_affinity_headers: bool,
    pub supports_cache_control_on_tools: bool,
    pub supports_temperature: bool,
    pub allow_empty_signature: bool,
    pub supports_strict_tools: bool,
    pub supports_tool_references: bool,
}

fn get_anthropic_compat(model: &Model) -> AnthropicCompatResolved {
    let compat = model.compat.as_ref();
    let flag = |value: Option<bool>| value.unwrap_or(true);
    AnthropicCompatResolved {
        supports_eager_tool_input_streaming: compat
            .and_then(|compat: &ModelCompat| compat.supports_eager_tool_input_streaming)
            .unwrap_or(true),
        supports_long_cache_retention: flag(
            compat.and_then(|compat| compat.supports_long_cache_retention),
        ),
        send_session_affinity_headers: compat
            .and_then(|compat| compat.send_session_affinity_headers)
            .unwrap_or(false),
        supports_cache_control_on_tools: flag(
            compat.and_then(|compat| compat.supports_cache_control_on_tools),
        ),
        supports_temperature: flag(compat.and_then(|compat| compat.supports_temperature)),
        allow_empty_signature: compat
            .and_then(|compat| compat.allow_empty_signature)
            .unwrap_or(false),
        supports_strict_tools: compat
            .and_then(|compat| compat.supports_strict_tools)
            .unwrap_or(false),
        supports_tool_references: compat
            .and_then(|compat| compat.supports_tool_references)
            .unwrap_or_else(|| default_supports_tool_references(model)),
    }
}

/// Port of `defaultSupportsToolReferences`: first-party Anthropic models
/// except Haiku and models predating tool search.
fn default_supports_tool_references(model: &Model) -> bool {
    if model.provider != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    // Port of /^claude-(?:opus|sonnet|fable)-(\d+)(?:-(\d+))?(?:-|$)/
    let Some(rest) = model.id.strip_prefix("claude-") else {
        return false;
    };
    let Some((family, remainder)) = rest.split_once('-') else {
        return false;
    };
    if !matches!(family, "opus" | "sonnet" | "fable") {
        return false;
    }
    let mut parts = remainder.splitn(2, '-');
    let major_text = parts.next().unwrap_or_default();
    let minor_text = parts.next();
    // The regex anchors the remainder at '-' or end; e.g. "4-5-20250929".
    let Ok(major) = major_text.parse::<u32>() else {
        return false;
    };
    let minor = match minor_text {
        Some(minor_text) => {
            let minor_text = minor_text.split('-').next().unwrap_or_default();
            if minor_text.len() < 8 {
                minor_text.parse::<u32>().unwrap_or(0)
            } else {
                0
            }
        }
        None => 0,
    };
    major > 4 || (major == 4 && minor >= 5)
}

fn merge_client_headers(sources: Vec<Option<&ProviderHeaders>>) -> ProviderHeaders {
    let mut merged = ProviderHeaders::new();
    merged.insert(
        "User-Agent".to_string(),
        Some(crate::ai::session_resources::get_pi_user_agent()),
    );
    for headers in sources.into_iter().flatten() {
        for (name, value) in headers {
            merged.insert(name.clone(), value.clone());
        }
    }
    merged
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else {
        return false;
    };
    let expected = name.to_lowercase();
    headers.iter().any(|(key, value)| {
        key.to_lowercase() == expected
            && value.as_ref().is_some_and(|value| !value.trim().is_empty())
    })
}

fn assert_request_auth(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<(), String> {
    if api_key.is_some() {
        return Ok(());
    }
    if has_header(headers, "authorization")
        || has_header(headers, "x-api-key")
        || has_header(headers, "cf-aig-authorization")
    {
        return Ok(());
    }
    Err(format!("No API key for provider: {provider}"))
}

const ANTHROPIC_MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// The resolved HTTP request plan standing in for the TypeScript SDK client.
struct AnthropicRequestPlan {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// Port of `createClient` + the SDK's `messages.create(..., { stream: true })`
/// request assembly.
#[allow(clippy::too_many_arguments)]
fn build_request_plan(
    model: &Model,
    api_key: Option<&str>,
    interleaved_thinking: bool,
    use_fine_grained_tool_streaming_beta: bool,
    use_server_side_fallback_beta: bool,
    options_headers: Option<&ProviderHeaders>,
    dynamic_headers: Option<Vec<(String, String)>>,
    session_id: Option<&str>,
    params: Value,
) -> Result<AnthropicRequestPlan, String> {
    // Adaptive thinking models have interleaved thinking built in, so skip
    // the beta header.
    let needs_interleaved_beta = interleaved_thinking
        && model
            .compat
            .as_ref()
            .and_then(|compat| compat.force_adaptive_thinking)
            != Some(true);
    let mut beta_features: Vec<&str> = Vec::new();
    if use_fine_grained_tool_streaming_beta {
        beta_features.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if needs_interleaved_beta {
        beta_features.push(INTERLEAVED_THINKING_BETA);
    }
    if use_server_side_fallback_beta {
        beta_features.push(SERVER_SIDE_FALLBACK_BETA);
    }

    let model_headers: ProviderHeaders = model
        .headers
        .as_ref()
        .map(|headers| {
            headers
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.clone())))
                .collect()
        })
        .unwrap_or_default();
    let dynamic: ProviderHeaders = dynamic_headers
        .map(|headers| {
            headers
                .into_iter()
                .map(|(name, value)| (name, Some(value)))
                .collect()
        })
        .unwrap_or_default();

    let mut base: ProviderHeaders = ProviderHeaders::new();
    base.insert("accept".to_string(), Some("application/json".to_string()));
    base.insert(
        "anthropic-dangerous-direct-browser-access".to_string(),
        Some("true".to_string()),
    );
    if !beta_features.is_empty() {
        base.insert("anthropic-beta".to_string(), Some(beta_features.join(",")));
    }

    // Copilot: Bearer auth, selective betas.
    let (auth_headers, _is_oauth) = if model.provider == "github-copilot" {
        let mut headers = base;
        headers.insert(
            "authorization".to_string(),
            api_key.map(|key| format!("Bearer {key}")),
        );
        (
            merge_client_headers(vec![
                Some(&headers),
                Some(&model_headers),
                Some(&dynamic),
                options_headers,
            ]),
            false,
        )
    } else if api_key.is_some_and(is_oauth_token) {
        // OAuth: Bearer auth, Claude Code identity headers.
        let mut headers = base;
        let betas = headers
            .remove("anthropic-beta")
            .flatten()
            .map(|value| value.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        headers.insert(
            "anthropic-beta".to_string(),
            Some(
                [
                    "claude-code-20250219".to_string(),
                    "oauth-2025-04-20".to_string(),
                ]
                .into_iter()
                .chain(betas)
                .collect::<Vec<_>>()
                .join(","),
            ),
        );
        headers.insert(
            "user-agent".to_string(),
            Some(format!("claude-cli/{CLAUDE_CODE_VERSION}")),
        );
        headers.insert("x-app".to_string(), Some("cli".to_string()));
        headers.insert(
            "authorization".to_string(),
            api_key.map(|key| format!("Bearer {key}")),
        );
        (
            merge_client_headers(vec![Some(&headers), Some(&model_headers), options_headers]),
            true,
        )
    } else {
        // API key or header-owned auth.
        let session_affinity_headers: ProviderHeaders = match session_id {
            Some(session_id) if get_anthropic_compat(model).send_session_affinity_headers => {
                let mut headers = ProviderHeaders::new();
                headers.insert(
                    "x-session-affinity".to_string(),
                    Some(session_id.to_string()),
                );
                headers
            }
            _ => ProviderHeaders::new(),
        };
        base.insert("x-api-key".to_string(), api_key.map(str::to_string));
        (
            merge_client_headers(vec![
                Some(&base),
                Some(&session_affinity_headers),
                Some(&model_headers),
                options_headers,
            ]),
            false,
        )
    };

    // The SDK sends x-api-key (or authorization) plus anthropic-version and
    // the JSON content type.
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in auth_headers {
        if let Some(value) = value {
            headers.push((name, value));
        }
    }
    headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
    headers.push(("content-type".to_string(), "application/json".to_string()));

    let base_url = model.base_url.trim_end_matches('/');
    Ok(AnthropicRequestPlan {
        url: format!("{base_url}/v1/messages"),
        headers,
        body: params,
    })
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

/// Normalizes tool call IDs to match Anthropic's required pattern and length.
fn normalize_tool_call_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    cleaned.chars().take(64).collect()
}

/// Port of `mapStopReason`.
fn map_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(StopReason, Option<String>), String> {
    match reason {
        "end_turn" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => Ok((
            StopReason::Error,
            Some(
                stop_details
                    .and_then(|details| details.get("explanation"))
                    .and_then(Value::as_str)
                    .unwrap_or("The model refused to complete the request")
                    .to_string(),
            ),
        )),
        // Stop is good enough -> resubmit
        "pause_turn" => Ok((StopReason::Stop, None)),
        // We don't supply stop sequences, so this should never happen.
        "stop_sequence" => Ok((StopReason::Stop, None)),
        // Content flagged by safety filters (not yet in SDK types)
        "sensitive" => Ok((
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_string()),
        )),
        // Handle unknown stop reasons gracefully (API may add new values)
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

/// Port of `convertToolResult`.
fn convert_tool_result(
    message: &ToolResultMessage,
    is_oauth_token: bool,
    deferred_tool_names: &std::collections::BTreeSet<String>,
    loaded_tool_names: &mut std::collections::BTreeSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> (Value, Vec<Value>) {
    let mut references: Vec<Value> = Vec::new();
    for name in message.added_tool_names.iter().flatten() {
        let normalized_name = normalize_tool_name(name);
        if !deferred_tool_names.contains(&normalized_name)
            || loaded_tool_names.contains(&normalized_name)
        {
            continue;
        }
        loaded_tool_names.insert(normalized_name);
        references.push(json!({
            "type": "tool_reference",
            "tool_name": if is_oauth_token { to_claude_code_name(name) } else { name.clone() },
        }));
    }
    let converted_content = convert_content_blocks(&message.content);
    // Anthropic rejects tool references mixed with ordinary tool-result
    // content.
    let tool_result = json!({
        "type": "tool_result",
        "tool_use_id": message.tool_call_id,
        "content": if references.is_empty() { converted_content.clone() } else { Value::Array(references.clone()) },
        "is_error": message.is_error,
    });
    let sibling_content: Vec<Value> = if references.is_empty() {
        Vec::new()
    } else if converted_content.is_string() {
        vec![json!({"type": "text", "text": converted_content.as_str().unwrap_or_default()})]
    } else {
        converted_content.as_array().cloned().unwrap_or_default()
    };
    (tool_result, sibling_content)
}

/// Port of `convertMessages`.
#[allow(clippy::too_many_arguments)]
fn convert_messages(
    transformed_messages: &[Message],
    is_oauth_token: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    deferred_tool_names: &std::collections::BTreeSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let mut loaded_tool_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    let mut index = 0;
    while index < transformed_messages.len() {
        let message = &transformed_messages[index];
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({
                            "role": "user",
                            "content": sanitize_surrogates(text),
                        }));
                    }
                }
                UserContent::Blocks(blocks) => {
                    let blocks: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            crate::ai::types::BlockContent::Text(text) => json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            crate::ai::types::BlockContent::Image(image) => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": image.mime_type,
                                    "data": image.data,
                                },
                            }),
                        })
                        .collect();
                    let filtered_blocks: Vec<Value> = blocks
                        .into_iter()
                        .filter(|block| {
                            if block.get("type").and_then(Value::as_str) == Some("text") {
                                block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .map(str::is_empty)
                                    == Some(false)
                            } else {
                                true
                            }
                        })
                        .collect();
                    if filtered_blocks.is_empty() {
                        index += 1;
                        continue;
                    }
                    params.push(json!({
                        "role": "user",
                        "content": filtered_blocks,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();

                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if text.text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }));
                        }
                        AssistantContent::Thinking(thinking) => {
                            // Redacted thinking: pass the opaque payload back
                            // as redacted_thinking.
                            if thinking.redacted == Some(true) {
                                blocks.push(json!({
                                    "type": "redacted_thinking",
                                    "data": thinking.thinking_signature.clone().unwrap_or_default(),
                                }));
                                continue;
                            }
                            let thinking_signature =
                                thinking.thinking_signature.as_deref().unwrap_or("");
                            let has_thinking_signature = !thinking_signature.is_empty()
                                && !thinking_signature.trim().is_empty();
                            if thinking.thinking.trim().is_empty() && !has_thinking_signature {
                                continue;
                            }
                            // If thinking signature is missing/empty (e.g.,
                            // from aborted stream), convert to plain text for
                            // Anthropic. Some compatible providers emit and
                            // accept empty signatures, so let marked models
                            // preserve the block.
                            if !has_thinking_signature {
                                blocks.push(if allow_empty_signature {
                                    json!({
                                        "type": "thinking",
                                        "thinking": sanitize_surrogates(&thinking.thinking),
                                        "signature": "",
                                    })
                                } else {
                                    json!({
                                        "type": "text",
                                        "text": sanitize_surrogates(&thinking.thinking),
                                    })
                                });
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": sanitize_surrogates(&thinking.thinking),
                                    "signature": thinking_signature,
                                }));
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tool_call.id,
                                "name": if is_oauth_token { to_claude_code_name(&tool_call.name) } else { tool_call.name.clone() },
                                "input": Value::Object(tool_call.arguments.clone()),
                            }));
                        }
                    }
                }
                if blocks.is_empty() {
                    index += 1;
                    continue;
                }
                params.push(json!({
                    "role": "assistant",
                    "content": blocks,
                }));
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages, needed for z.ai
                // Anthropic endpoint.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut sibling_content: Vec<Value> = Vec::new();
                let mut cursor = index;
                while cursor < transformed_messages.len()
                    && matches!(transformed_messages[cursor], Message::ToolResult(_))
                {
                    if let Message::ToolResult(result) = &transformed_messages[cursor] {
                        let (tool_result, siblings) = convert_tool_result(
                            result,
                            is_oauth_token,
                            deferred_tool_names,
                            &mut loaded_tool_names,
                            normalize_tool_name,
                        );
                        tool_results.push(tool_result);
                        sibling_content.extend(siblings);
                    }
                    cursor += 1;
                }

                // Skip the messages we've already processed.
                index = cursor - 1;

                // Displaced reference-bearing results must follow every
                // tool_result block.
                let mut content = tool_results.clone();
                content.extend(sibling_content.clone());
                params.push(json!({
                    "role": "user",
                    "content": content,
                }));
            }
        }
        index += 1;
    }

    // Add cache_control to the last user message to cache conversation
    // history.
    if let Some(cache_control) = cache_control
        && let Some(last_message) = params.last_mut()
        && last_message.get("role").and_then(Value::as_str) == Some("user")
    {
        let content = last_message.get_mut("content");
        match content {
            Some(Value::Array(blocks)) => {
                if let Some(last_block) = blocks.last_mut() {
                    let block_type = last_block.get("type").and_then(Value::as_str);
                    if matches!(
                        block_type,
                        Some("text") | Some("image") | Some("tool_result")
                    ) && let Some(object) = last_block.as_object_mut()
                    {
                        object.insert("cache_control".to_string(), cache_control.clone());
                    }
                }
            }
            Some(Value::String(text)) => {
                let text = text.clone();
                last_message["content"] = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": cache_control,
                }]);
            }
            _ => {}
        }
    }

    params
}

/// Port of `shouldUseFineGrainedToolStreamingBeta`.
fn should_use_fine_grained_tool_streaming_beta(model: &Model, context: &Context) -> bool {
    context
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        && !get_anthropic_compat(model).supports_eager_tool_input_streaming
}

/// Port of `convertTools`.
fn convert_tools(
    tools: &[Tool],
    is_oauth_token: bool,
    supports_eager_tool_input_streaming: bool,
    supports_strict_tools: bool,
    cache_control: Option<&Value>,
    defer_loading: bool,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict_tools)?;
            let parameters = get_json_schema_tool_parameters(tool, strict == Some(true))?;
            let schema = parameters.as_object().cloned().unwrap_or_default();
            let mut input_schema = Map::new();
            if strict == Some(true) {
                for (key, value) in &schema {
                    input_schema.insert(key.clone(), value.clone());
                }
            }
            input_schema.insert("type".to_string(), json!("object"));
            input_schema.insert(
                "properties".to_string(),
                schema
                    .get("properties")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
            input_schema.insert(
                "required".to_string(),
                schema.get("required").cloned().unwrap_or_else(|| json!([])),
            );

            let mut converted = Map::new();
            converted.insert(
                "name".to_string(),
                json!(if is_oauth_token {
                    to_claude_code_name(&tool.name)
                } else {
                    tool.name.clone()
                }),
            );
            converted.insert("description".to_string(), json!(tool.description));
            if supports_eager_tool_input_streaming {
                converted.insert("eager_input_streaming".to_string(), json!(true));
            }
            if strict == Some(true) {
                converted.insert("strict".to_string(), json!(true));
            }
            converted.insert("input_schema".to_string(), Value::Object(input_schema));
            if defer_loading {
                converted.insert("defer_loading".to_string(), json!(true));
            }
            if let Some(cache_control) = cache_control
                && index + 1 == tools.len()
            {
                converted.insert("cache_control".to_string(), cache_control.clone());
            }
            Ok(Value::Object(converted))
        })
        .collect()
}

/// Port of `buildParams`.
fn build_params(
    model: &Model,
    context: &Context,
    is_oauth_token: bool,
    options: Option<&AnthropicOptions>,
) -> Result<Value, String> {
    let options = options.cloned().unwrap_or_default();
    let retention =
        resolve_cache_retention(options.base.cache_retention, options.base.base.env.as_ref());
    let cache_control = cache_control_value(retention, model);
    let compat = get_anthropic_compat(model);
    let normalize_tool_call_id_fn =
        |id: &str, _assistant: &AssistantMessage| normalize_tool_call_id(id);
    let transformed_messages =
        transform_messages(&context.messages, model, Some(&normalize_tool_call_id_fn));
    let normalize_tool_name = |name: &str| -> String {
        if is_oauth_token {
            to_claude_code_name(name)
        } else {
            name.to_string()
        }
    };
    let tool_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages: transformed_messages.clone(),
        tools: context.tools.clone(),
    };
    let tool_placement =
        split_deferred_tools(&tool_context, compat.supports_tool_references, |name| {
            normalize_tool_name(name)
        });
    let mut immediate_tools = tool_placement.immediate;
    let mut deferred_tools: Vec<Tool> = tool_placement.deferred.into_values().collect();
    if immediate_tools.is_empty() && !deferred_tools.is_empty() {
        immediate_tools = deferred_tools.clone();
        deferred_tools.clear();
    }
    let deferred_tool_names: std::collections::BTreeSet<String> = deferred_tools
        .iter()
        .map(|tool| normalize_tool_name(&tool.name))
        .collect();

    let mut params = json!({
        "model": model.id,
        "messages": convert_messages(
            &transformed_messages,
            is_oauth_token,
            cache_control.as_ref(),
            compat.allow_empty_signature,
            &deferred_tool_names,
            &normalize_tool_name,
        ),
        "max_tokens": options.base.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });

    // For OAuth tokens, we MUST include Claude Code identity.
    if is_oauth_token {
        let mut system = vec![{
            let mut block = json!({
                "type": "text",
                "text": "You are Claude Code, Anthropic's official CLI for Claude.",
            });
            if let Some(cache_control) = &cache_control {
                block["cache_control"] = cache_control.clone();
            }
            block
        }];
        if let Some(system_prompt) = &context.system_prompt {
            let mut block = json!({
                "type": "text",
                "text": sanitize_surrogates(system_prompt),
            });
            if let Some(cache_control) = &cache_control {
                block["cache_control"] = cache_control.clone();
            }
            system.push(block);
        }
        params["system"] = Value::Array(system);
    } else if let Some(system_prompt) = &context.system_prompt {
        // Add cache control to system prompt for non-OAuth tokens.
        let mut block = json!({
            "type": "text",
            "text": sanitize_surrogates(system_prompt),
        });
        if let Some(cache_control) = &cache_control {
            block["cache_control"] = cache_control.clone();
        }
        params["system"] = json!([block]);
    }

    // Temperature is incompatible with extended thinking and unsupported on
    // Claude Opus 4.7+.
    if let Some(temperature) = options.base.temperature
        && options.thinking_enabled != Some(true)
        && compat.supports_temperature
    {
        params["temperature"] = json!(temperature);
    }

    if !immediate_tools.is_empty() || !deferred_tools.is_empty() {
        let mut tools = convert_tools(
            &immediate_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            if compat.supports_cache_control_on_tools {
                cache_control.as_ref()
            } else {
                None
            },
            false,
        )?;
        tools.extend(convert_tools(
            &deferred_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            None,
            true,
        )?);
        params["tools"] = Value::Array(tools);
    }

    // Configure thinking mode: adaptive, budget-based, or explicitly
    // disabled.
    if model.reasoning {
        if options.thinking_enabled == Some(true) {
            // Default to "summarized" so Opus 4.7 and Mythos Preview behave
            // like older Claude 4 models.
            let display = options
                .thinking_display
                .unwrap_or(AnthropicThinkingDisplay::Summarized);
            if model
                .compat
                .as_ref()
                .and_then(|compat| compat.force_adaptive_thinking)
                == Some(true)
            {
                // Adaptive thinking: Claude decides when and how much to
                // think.
                params["thinking"] = json!({"type": "adaptive", "display": display.as_str()});
                if let Some(effort) = options.effort {
                    params["output_config"] = json!({"effort": effort.as_str()});
                }
            } else {
                // Budget-based thinking for older models.
                params["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": options.thinking_budget_tokens.unwrap_or(1024),
                    "display": display.as_str(),
                });
            }
        } else if options.thinking_enabled == Some(false) {
            let off_supported = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
                .is_none_or(|value| value.is_some());
            if off_supported {
                params["thinking"] = json!({"type": "disabled"});
            }
        }
    }

    if let Some(metadata) = &options.base.metadata
        && let Some(user_id) = metadata.get("user_id").and_then(Value::as_str)
    {
        params["metadata"] = json!({ "user_id": user_id });
    }

    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = match tool_choice {
            AnthropicToolChoice::Auto => json!({"type": "auto"}),
            AnthropicToolChoice::Any => json!({"type": "any"}),
            AnthropicToolChoice::None => json!({"type": "none"}),
            AnthropicToolChoice::Tool { name } => json!({"type": "tool", "name": name}),
        };
    }

    let allowed_fallback_models = model
        .compat
        .as_ref()
        .and_then(|compat| compat.allowed_fallback_models.clone());
    if let Some(fallbacks) = allowed_fallback_models
        && !fallbacks.is_empty()
    {
        params["fallbacks"] = json!(
            fallbacks
                .iter()
                .map(|fallback| json!({ "model": fallback.model }))
                .collect::<Vec<_>>()
        );
    }

    Ok(params)
}

/// Extracts the raw assistant output (headers included) from a response.
fn response_headers(response: &crate::ai::utils::http::HttpResponse) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    for (name, value) in &response.headers {
        headers
            .entry(name.to_lowercase())
            .or_insert_with(|| value.clone());
    }
    headers
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&AnthropicOptions>,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let model = model.clone();
    let context = context.clone();
    let options = options.cloned();
    let producer = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage {
            role: crate::ai::types::RoleAssistant,
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Default::default(),
            stop_reason: StopReason::Pending,
            timestamp: crate::ai::auth::resolve::now_millis(),
            ..Default::default()
        };

        let result = run_stream(&model, &context, options.as_ref(), &mut output, &producer).await;
        if let Err(error) = result {
            output.stop_reason = if options
                .as_ref()
                .and_then(|options| options.base.base.signal.as_ref())
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            output.error_message = Some(error);
            producer.push(AssistantMessageEvent::Error {
                reason: if output.stop_reason == StopReason::Aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                },
                error: output.clone(),
            });
            producer.end(None);
        }
    });
    stream
}

type Block = (usize, AssistantContent, String);

async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&AnthropicOptions>,
    output: &mut AssistantMessage,
    producer: &AssistantMessageEventStream,
) -> Result<(), String> {
    let signal = options.and_then(|options| options.base.base.signal.clone());
    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);

    let mut usage_model = model.clone();

    let api_key = options.and_then(|options| options.base.base.api_key.clone());
    let headers = options.and_then(|options| options.base.base.headers.clone());
    assert_request_auth(&model.provider, api_key.as_deref(), headers.as_ref())?;

    let mut copilot_dynamic_headers: Option<Vec<(String, String)>> = None;
    if model.provider == "github-copilot" {
        let has_images = has_copilot_vision_input(&context.messages);
        copilot_dynamic_headers =
            Some(build_copilot_dynamic_headers(&context.messages, has_images));
    }

    let cache_retention = resolve_cache_retention(
        options.and_then(|options| options.base.cache_retention),
        options.and_then(|options| options.base.base.env.as_ref()),
    );
    let cache_session_id = if cache_retention == CacheRetention::None {
        None
    } else {
        options.and_then(|options| options.base.session_id.clone())
    };

    let interleaved_thinking = options
        .and_then(|options| options.interleaved_thinking)
        .unwrap_or(true);
    let is_oauth = is_oauth_token_inner(api_key.as_deref());
    let mut params = build_params(model, context, is_oauth, options)?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_params) = on_payload(params.clone(), model).await
    {
        params = next_params;
    }

    let plan = build_request_plan(
        model,
        api_key.as_deref(),
        interleaved_thinking,
        should_use_fine_grained_tool_streaming_beta(model, context),
        should_use_server_side_fallback_beta(model),
        headers.as_ref(),
        copilot_dynamic_headers,
        cache_session_id.as_deref(),
        params,
    )?;

    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url: plan.url.clone(),
        headers: plan.headers.clone(),
        body: HttpBody::Json(plan.body.clone()),
    };
    let response = retry_http_request(
        &fetch,
        &request,
        crate::ai::utils::provider_retry::ProviderRetryOptions {
            max_retries: options.and_then(|options| options.base.base.max_retries),
            on_retry: options.and_then(|options| options.base.base.on_retry.clone()),
            max_retry_delay_ms: options.and_then(|options| options.base.base.max_retry_delay_ms),
            signal: signal.clone(),
        },
    )
    .await
    .map_err(|error| error.message)?;

    if !(200..300).contains(&response.status) {
        let status = response.status;
        let body = crate::ai::utils::http::collect_text(response).await;
        return Err(crate::ai::utils::error_body::format_provider_error(
            &crate::ai::utils::error_body::normalize_provider_error(
                crate::ai::utils::error_body::ProviderErrorParts {
                    status: Some(status),
                    body: Some(body),
                    message: format!("{status} status code"),
                },
            ),
            Some("Anthropic API error"),
        ));
    }

    if let Some(on_response) = options.and_then(|options| options.base.base.on_response.as_ref()) {
        on_response(
            &crate::ai::types::ProviderResponse {
                status: response.status,
                headers: response_headers(&response),
            },
            model,
        )
        .await;
    }

    producer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    // Streaming blocks carry their Anthropic event index and a scratch
    // partial-JSON buffer for tool calls.
    let mut blocks: Vec<Block> = Vec::new();

    // Incremental: each SSE event is validated, parsed, and pushed to the
    // producer AS IT ARRIVES. Draining the response into a Vec first (the
    // previous shape) held every delta until the stream closed — the consumer
    // saw `Start` at once and then the entire reply in one burst, which read
    // as "no streaming" for every provider on this transport.
    let sse_stream = SseStream::new(response.body);
    futures::pin_mut!(sse_stream);
    let mut saw_message_start = false;
    let mut saw_message_stop = false;
    loop {
        if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
            return Err("Request was aborted".to_string());
        }
        let Some(sse) = futures::StreamExt::next(&mut sse_stream).await else {
            break;
        };
        // Transport errors arrive as synthetic error events.
        if sse.event.as_deref() == Some("__error__") {
            return Err(sse.data);
        }
        if sse.event.as_deref() == Some("error") {
            return Err(sse.data);
        }
        if !ANTHROPIC_MESSAGE_EVENTS.contains(&sse.event.as_deref().unwrap_or("")) {
            continue;
        }
        let event: Value = parse_json_with_repair(&sse.data).map_err(|error| {
            format!(
                "Could not parse Anthropic SSE event {}: {}; data={}; raw={}",
                sse.event.clone().unwrap_or_default(),
                error,
                sse.data,
                serde_json::to_string(&sse.raw).unwrap_or_default()
            )
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => saw_message_start = true,
            Some("message_stop") => saw_message_stop = true,
            _ => {}
        }
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match event_type.as_str() {
            "message_start" => {
                let message = event.get("message").cloned().unwrap_or(Value::Null);
                output.response_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                output.model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&model.id)
                    .to_string();
                let fallback_cost = if output.model == model.id {
                    None
                } else {
                    model
                        .compat
                        .as_ref()
                        .and_then(|compat| compat.allowed_fallback_models.as_ref())
                        .and_then(|fallbacks| {
                            fallbacks
                                .iter()
                                .find(|fallback| {
                                    fallback.provider == model.provider
                                        && fallback.model == output.model
                                })
                                .map(|fallback| fallback.cost.clone())
                        })
                };
                if let Some(cost) = fallback_cost {
                    usage_model = Model {
                        id: output.model.clone(),
                        cost,
                        ..model.clone()
                    };
                } else {
                    usage_model = model.clone();
                }
                // Capture initial token usage from message_start.
                let usage = message.get("usage").cloned().unwrap_or(Value::Null);
                output.usage.input = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                output.usage.output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                output.usage.cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                output.usage.cache_write = usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                output.usage.cache_write_1h = usage
                    .get("cache_creation")
                    .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
                    .and_then(Value::as_u64);
                // Anthropic doesn't provide total_tokens; compute.
                output.usage.total_tokens = output
                    .usage
                    .input
                    .saturating_add(output.usage.output)
                    .saturating_add(output.usage.cache_read)
                    .saturating_add(output.usage.cache_write);
                calculate_cost(&usage_model, &mut output.usage);
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let content_block = event.get("content_block").cloned().unwrap_or(Value::Null);
                let block_type = content_block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match block_type {
                    "text" => {
                        let block = AssistantContent::Text(TextContent {
                            text: content_block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            ..Default::default()
                        });
                        blocks.push((index, block.clone(), String::new()));
                        output.content.push(block);
                        producer.push(AssistantMessageEvent::TextStart {
                            content_index: output.content.len() - 1,
                            partial: output.clone(),
                        });
                    }
                    "thinking" => {
                        let block = AssistantContent::Thinking(ThinkingContent {
                            thinking: content_block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            thinking_signature: Some(
                                content_block
                                    .get("signature")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            ),
                            ..Default::default()
                        });
                        blocks.push((index, block.clone(), String::new()));
                        output.content.push(block);
                        producer.push(AssistantMessageEvent::ThinkingStart {
                            content_index: output.content.len() - 1,
                            partial: output.clone(),
                        });
                    }
                    "redacted_thinking" => {
                        let block = AssistantContent::Thinking(ThinkingContent {
                            thinking: "[Reasoning redacted]".to_string(),
                            thinking_signature: Some(
                                content_block
                                    .get("data")
                                    .cloned()
                                    .map(|data| match data {
                                        Value::String(text) => text,
                                        other => other.to_string(),
                                    })
                                    .unwrap_or_default(),
                            ),
                            redacted: Some(true),
                            ..Default::default()
                        });
                        blocks.push((index, block.clone(), String::new()));
                        output.content.push(block);
                        producer.push(AssistantMessageEvent::ThinkingStart {
                            content_index: output.content.len() - 1,
                            partial: output.clone(),
                        });
                    }
                    "tool_use" => {
                        let block = AssistantContent::ToolCall(ToolCall {
                            content_type: Default::default(),
                            id: content_block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: if is_oauth {
                                from_claude_code_name(
                                    content_block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default(),
                                    context.tools.as_deref(),
                                )
                            } else {
                                content_block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            },
                            arguments: content_block
                                .get("input")
                                .and_then(Value::as_object)
                                .cloned()
                                .unwrap_or_default(),
                            ..Default::default()
                        });
                        blocks.push((index, block.clone(), String::new()));
                        output.content.push(block);
                        producer.push(AssistantMessageEvent::ToolcallStart {
                            content_index: output.content.len() - 1,
                            partial: output.clone(),
                        });
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match delta_type {
                    "text_delta" => {
                        if let Some(position) = blocks.iter().position(|(block_index, block, _)| {
                            *block_index == index && matches!(block, AssistantContent::Text(_))
                        }) {
                            let text = delta
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if let Some((_, AssistantContent::Text(block), _)) =
                                blocks.get_mut(position)
                            {
                                block.text.push_str(text);
                            }
                            if let Some(AssistantContent::Text(block)) =
                                output.content.get_mut(position)
                            {
                                block.text.push_str(text);
                            }
                            producer.push(AssistantMessageEvent::TextDelta {
                                content_index: position,
                                delta: text.to_string(),
                                partial: output.clone(),
                            });
                        }
                    }
                    "thinking_delta" => {
                        if let Some(position) = blocks.iter().position(|(block_index, block, _)| {
                            *block_index == index && matches!(block, AssistantContent::Thinking(_))
                        }) {
                            let thinking = delta
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if let Some((_, AssistantContent::Thinking(block), _)) =
                                blocks.get_mut(position)
                            {
                                block.thinking.push_str(thinking);
                            }
                            if let Some(AssistantContent::Thinking(block)) =
                                output.content.get_mut(position)
                            {
                                block.thinking.push_str(thinking);
                            }
                            producer.push(AssistantMessageEvent::ThinkingDelta {
                                content_index: position,
                                delta: thinking.to_string(),
                                partial: output.clone(),
                            });
                        }
                    }
                    "input_json_delta" => {
                        if let Some(position) = blocks.iter().position(|(block_index, block, _)| {
                            *block_index == index && matches!(block, AssistantContent::ToolCall(_))
                        }) {
                            let partial_json = delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let mut scratch = blocks[position].2.clone();
                            scratch.push_str(partial_json);
                            let arguments = parse_streaming_json(Some(&scratch));
                            blocks[position].2 = scratch;
                            if let Some((_, AssistantContent::ToolCall(block), _)) =
                                blocks.get_mut(position)
                            {
                                block.arguments =
                                    arguments.as_object().cloned().unwrap_or_default();
                            }
                            if let Some(AssistantContent::ToolCall(block)) =
                                output.content.get_mut(position)
                            {
                                let scratch = blocks[position].2.clone();
                                block.arguments = parse_streaming_json(Some(&scratch))
                                    .as_object()
                                    .cloned()
                                    .unwrap_or_default();
                            }
                            producer.push(AssistantMessageEvent::ToolcallDelta {
                                content_index: position,
                                delta: partial_json.to_string(),
                                partial: output.clone(),
                            });
                        }
                    }
                    "signature_delta" => {
                        if let Some(position) = blocks.iter().position(|(block_index, block, _)| {
                            *block_index == index && matches!(block, AssistantContent::Thinking(_))
                        }) {
                            let signature = delta
                                .get("signature")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            // The TS blocks array aliases `output.content`, so
                            // the appended signature is observable on the
                            // final message; mirror that in both copies.
                            if let Some((_, AssistantContent::Thinking(block), _)) =
                                blocks.get_mut(position)
                            {
                                let existing = block.thinking_signature.clone().unwrap_or_default();
                                let mut combined = existing;
                                combined.push_str(signature);
                                block.thinking_signature = Some(combined);
                            }
                            if let Some(AssistantContent::Thinking(block)) =
                                output.content.get_mut(position)
                            {
                                let existing = block.thinking_signature.clone().unwrap_or_default();
                                let mut combined = existing;
                                combined.push_str(signature);
                                block.thinking_signature = Some(combined);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(position) = blocks
                    .iter()
                    .position(|(block_index, _, _)| *block_index == index)
                {
                    let (_, block, scratch) = blocks[position].clone();
                    match block {
                        AssistantContent::Text(text) => {
                            producer.push(AssistantMessageEvent::TextEnd {
                                content_index: position,
                                content: text.text.clone(),
                                partial: output.clone(),
                            });
                        }
                        AssistantContent::Thinking(thinking) => {
                            producer.push(AssistantMessageEvent::ThinkingEnd {
                                content_index: position,
                                content: thinking.thinking.clone(),
                                partial: output.clone(),
                            });
                        }
                        AssistantContent::ToolCall(mut tool_call) => {
                            let arguments = parse_streaming_json(Some(&scratch));
                            tool_call.arguments =
                                arguments.as_object().cloned().unwrap_or_default();
                            blocks[position].1 = AssistantContent::ToolCall(tool_call.clone());
                            output.content[position] =
                                AssistantContent::ToolCall(tool_call.clone());
                            producer.push(AssistantMessageEvent::ToolcallEnd {
                                content_index: position,
                                tool_call,
                                partial: output.clone(),
                            });
                        }
                    }
                }
            }
            "message_delta" => {
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(stop_reason) = delta.get("stop_reason").and_then(Value::as_str) {
                    output.raw_stop_reason = Some(stop_reason.to_string());
                    let stop_details = delta.get("stop_details");
                    let (stop_reason, error_message) = map_stop_reason(stop_reason, stop_details)?;
                    output.stop_reason = stop_reason;
                    if let Some(error_message) = error_message {
                        output.error_message = Some(error_message);
                    }
                }
                // Only update usage fields if present (not null).
                if let Some(usage) = event.get("usage") {
                    if let Some(input_tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
                        output.usage.input = input_tokens;
                    }
                    if let Some(output_tokens) = usage.get("output_tokens").and_then(Value::as_u64)
                    {
                        output.usage.output = output_tokens;
                    }
                    if let Some(cache_read) =
                        usage.get("cache_read_input_tokens").and_then(Value::as_u64)
                    {
                        output.usage.cache_read = cache_read;
                    }
                    if let Some(cache_write) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                    {
                        output.usage.cache_write = cache_write;
                    }
                    // Anthropic reports reasoning tokens in
                    // output_tokens_details.thinking_tokens on the final
                    // message_delta usage.
                    if let Some(thinking_tokens) = usage
                        .get("output_tokens_details")
                        .and_then(|details| details.get("thinking_tokens"))
                        .and_then(Value::as_u64)
                    {
                        output.usage.reasoning = Some(thinking_tokens);
                    }
                }
                // Anthropic doesn't provide total_tokens; compute.
                output.usage.total_tokens = output
                    .usage
                    .input
                    .saturating_add(output.usage.output)
                    .saturating_add(output.usage.cache_read)
                    .saturating_add(output.usage.cache_write);
                calculate_cost(&usage_model, &mut output.usage);
            }
            _ => {}
        }
    }

    if saw_message_start && !saw_message_stop {
        return Err("Anthropic stream ended before message_stop".to_string());
    }

    if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason == StopReason::Pending {
        return Err("Anthropic stream ended without a stop reason".to_string());
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }

    let reason = match output.stop_reason {
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Deferred => DoneReason::Deferred,
        _ => DoneReason::Stop,
    };
    producer.push(AssistantMessageEvent::Done {
        reason,
        message: output.clone(),
    });
    producer.end(None);
    Ok(())
}

fn is_oauth_token_inner(api_key: Option<&str>) -> bool {
    api_key.is_some_and(is_oauth_token)
}

/// Maps `SimpleStreamOptions.toolChoice` onto the Anthropic tool choice.
fn anthropic_tool_choice_from_simple(choice: crate::ai::types::ToolChoice) -> AnthropicToolChoice {
    match choice {
        crate::ai::types::ToolChoice::Auto => AnthropicToolChoice::Auto,
        crate::ai::types::ToolChoice::Any => AnthropicToolChoice::Any,
        crate::ai::types::ToolChoice::None => AnthropicToolChoice::None,
        crate::ai::types::ToolChoice::Tool { name } => AnthropicToolChoice::Tool { name },
    }
}

/// Port of `mapThinkingLevelToEffort`.
fn map_thinking_level_to_effort(
    model: &Model,
    level: Option<crate::ai::types::ThinkingLevel>,
) -> AnthropicEffort {
    let mapped = level.and_then(|level| {
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::from(level)))
            .cloned()
            .flatten()
    });
    if let Some(mapped) = mapped {
        return match mapped.as_str() {
            "low" => AnthropicEffort::Low,
            "medium" => AnthropicEffort::Medium,
            "high" => AnthropicEffort::High,
            "xhigh" => AnthropicEffort::Xhigh,
            "max" => AnthropicEffort::Max,
            _ => AnthropicEffort::High,
        };
    }
    match level {
        Some(crate::ai::types::ThinkingLevel::Minimal)
        | Some(crate::ai::types::ThinkingLevel::Low) => AnthropicEffort::Low,
        Some(crate::ai::types::ThinkingLevel::Medium) => AnthropicEffort::Medium,
        Some(crate::ai::types::ThinkingLevel::High)
        | Some(crate::ai::types::ThinkingLevel::Xhigh)
        | Some(crate::ai::types::ThinkingLevel::Max) => AnthropicEffort::High,
        None => AnthropicEffort::High,
    }
}

/// Port of `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if let Err(error) = assert_request_auth(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
    ) {
        return stream_error(model, &error);
    }

    let options = options.cloned().unwrap_or_default();
    let mut base = build_base_options(
        model,
        context,
        Some(&options),
        options.base.base.api_key.as_deref(),
    );
    let tool_choice = options.tool_choice;
    if options.reasoning.is_none() {
        let anthropic_options = AnthropicOptions {
            base,
            thinking_enabled: Some(false),
            tool_choice: tool_choice.map(anthropic_tool_choice_from_simple),
            ..Default::default()
        };
        return stream(model, context, Some(&anthropic_options));
    }
    let reasoning = options.reasoning.expect("reasoning checked");

    // For models with adaptive thinking: use an effort level. For older
    // models: use budget-based thinking.
    if model
        .compat
        .as_ref()
        .and_then(|compat| compat.force_adaptive_thinking)
        == Some(true)
    {
        let effort = map_thinking_level_to_effort(model, Some(reasoning));
        return stream(
            model,
            context,
            Some(&AnthropicOptions {
                base,
                thinking_enabled: Some(true),
                effort: Some(effort),
                tool_choice: tool_choice.map(anthropic_tool_choice_from_simple),
                ..Default::default()
            }),
        );
    }

    // Undefined means the caller did not request an output cap; let the
    // helper use the model cap.
    let model_max_tokens = options.base.max_tokens.unwrap_or(model.max_tokens);
    let (adjusted_max_tokens, thinking_budget) = adjust_max_tokens_for_thinking(
        base.max_tokens,
        model.max_tokens,
        reasoning,
        options.thinking_budgets.as_ref(),
    );
    let _ = model_max_tokens;

    let max_tokens = clamp_max_tokens_to_context(model, context, adjusted_max_tokens);

    base.max_tokens = Some(max_tokens);
    stream(
        model,
        context,
        Some(&AnthropicOptions {
            base,
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(thinking_budget.min(max_tokens.saturating_sub(1024))),
            tool_choice: tool_choice.map(anthropic_tool_choice_from_simple),
            ..Default::default()
        }),
    )
}

/// Emits a setup failure as an error stream (mirror of lazyStream's error
/// path for early `assertRequestAuth` failures).
fn stream_error(model: &Model, error: &str) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let producer = stream.clone();
    let model = model.clone();
    let error = error.to_string();
    tokio::spawn(async move {
        let message = AssistantMessage {
            role: crate::ai::types::RoleAssistant,
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Default::default(),
            stop_reason: StopReason::Error,
            error_message: Some(error),
            timestamp: crate::ai::auth::resolve::now_millis(),
            ..Default::default()
        };
        producer.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: message.clone(),
        });
        producer.end(Some(message));
    });
    stream
}
