//! Port of `pi-core/ai/src/api/openai-completions.ts`.
//!
//! The TypeScript implementation drives the `openai` SDK client; the Rust
//! port issues the same `POST {baseUrl}/chat/completions` request through the
//! injectable transport, reproducing the SDK wire behavior (authorization
//! bearer header, default headers, JSON body) while keeping the streaming
//! chunk handling 1:1. Compatibility auto-detection (`detectCompat`) and the
//! model.compat overrides merge exactly like the TypeScript helpers.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::ai::api::constrained_sampling::{
    GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta,
    create_grammar_tool_input_properties, get_grammar_tool_input, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::ai::api::github_copilot_headers::{
    build_copilot_dynamic_headers, has_copilot_vision_input,
};
use crate::ai::api::simple_options::{
    build_base_options, clamp_thinking_budget_to_answer_room, thinking_budget_for_level,
};
use crate::ai::api::transform_messages::transform_messages;
use crate::ai::models::{calculate_cost, clamp_thinking_level};
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DoneReason,
    ErrorReason, Message, Model, ProviderEnv, ProviderHeaders, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, ThinkingBudgets, ThinkingContent, ThinkingLevel,
    ThinkingTokenBudgetField, Tool, ToolCall, ToolCallArguments, UserContent,
};
use crate::ai::utils::error_body::{
    ProviderErrorParts, format_provider_error, normalize_provider_error,
};
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::json_parse::parse_streaming_json;
use crate::ai::utils::provider_env::get_provider_env_value;
use crate::ai::utils::provider_retry::retry_http_request;
use crate::ai::utils::reqwest_fetch::default_fetch;
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;
use crate::ai::utils::text::short_hash;

/// Port of `OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH` and
/// `clampOpenAIPromptCacheKey` (openai-prompt-cache.ts).
pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

pub fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    let key = key?;
    Some(
        key.chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect(),
    )
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

fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<String, String> {
    if let Some(api_key) = api_key {
        return Ok(api_key.to_string());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    Err(format!("No API key for provider: {provider}"))
}

/// Port of `hasToolHistory`: Anthropic via proxy requires the tools param
/// when messages include tool calls or tool results.
fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_))),
        Message::User(_) => false,
    })
}

fn get_deferred_tool_names(messages: &[Message]) -> BTreeSet<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => result.added_tool_names.clone(),
            _ => None,
        })
        .flatten()
        .collect()
}

fn get_tools_by_name(tools: Option<&[Tool]>, names: &BTreeSet<String>) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(|name| tools.iter().find(|tool| &tool.name == name).cloned())
        .collect()
}

/// Port of `OpenAICompletionsOptions`.
#[derive(Clone, Default)]
pub struct OpenAICompletionsOptions {
    pub base: StreamOptions,
    /// Tool choice: `"none"`, `"auto"`, `"required"`, or a
    /// `{"type":"function","function":{"name":...}}` object (carried as JSON).
    pub tool_choice: Option<Value>,
    pub reasoning_effort: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Port of `ResolvedOpenAICompletionsCompat` with detection defaults
/// applied.
#[derive(Clone, Debug)]
pub struct ResolvedCompletionsCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: crate::ai::types::MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: crate::ai::types::ThinkingFormat,
    pub open_router_routing: Option<crate::ai::types::OpenRouterRouting>,
    pub vercel_gateway_routing: Option<crate::ai::types::VercelGatewayRouting>,
    pub chat_template_kwargs: Option<crate::ai::types::ChatTemplateKwargs>,
    pub chat_template_args: Option<crate::ai::types::ChatTemplateKwargs>,
    pub zai_tool_stream: bool,
    pub supports_thinking_token_budget: Option<bool>,
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub cache_control_format: Option<crate::ai::types::CacheControlFormat>,
    pub send_session_affinity_headers: bool,
    pub deferred_tools_mode: Option<crate::ai::types::DeferredToolsMode>,
    pub session_affinity_format: crate::ai::types::SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
}

/// Port of `detectCompat`: auto-detects compatibility settings from provider
/// name and baseUrl.
pub fn detect_compat(model: &Model) -> ResolvedCompletionsCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();

    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_deepseek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");

    let is_non_standard = is_nvidia
        || provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_openrouter_developer_role_model =
        is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let cache_control_format = if provider == "openrouter" && model.id.starts_with("anthropic/") {
        Some(crate::ai::types::CacheControlFormat::Anthropic)
    } else {
        None
    };

    ResolvedCompletionsCompat {
        supports_store: !is_non_standard,
        supports_developer_role: is_openrouter_developer_role_model
            || (!is_non_standard && !is_openrouter),
        supports_reasoning_effort: !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia
            && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens {
            crate::ai::types::MaxTokensField::MaxTokens
        } else {
            crate::ai::types::MaxTokensField::MaxCompletionTokens
        },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format: if is_deepseek {
            crate::ai::types::ThinkingFormat::Deepseek
        } else if is_zai {
            crate::ai::types::ThinkingFormat::Zai
        } else if is_together {
            crate::ai::types::ThinkingFormat::Together
        } else if is_ant_ling {
            crate::ai::types::ThinkingFormat::AntLing
        } else if is_openrouter {
            crate::ai::types::ThinkingFormat::Openrouter
        } else {
            crate::ai::types::ThinkingFormat::Openai
        },
        open_router_routing: Some(crate::ai::types::OpenRouterRouting::default()),
        vercel_gateway_routing: Some(crate::ai::types::VercelGatewayRouting::default()),
        chat_template_kwargs: Some(crate::ai::types::ChatTemplateKwargs::default()),
        chat_template_args: Some(crate::ai::types::ChatTemplateKwargs::default()),
        zai_tool_stream: false,
        supports_thinking_token_budget: Some(false),
        thinking_token_budget_field: None,
        supports_strict_mode: !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia,
        supports_openai_grammar_tools: false,
        cache_control_format,
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: if is_openrouter {
            crate::ai::types::SessionAffinityFormat::Openrouter
        } else {
            crate::ai::types::SessionAffinityFormat::Openai
        },
        supports_long_cache_retention: !(is_together
            || is_cloudflare_workers_ai
            || is_cloudflare_ai_gateway
            || is_nvidia
            || is_ant_ling),
    }
}

/// Port of `getCompat`: auto-detect then override with explicit model.compat.
pub fn get_compat(model: &Model) -> ResolvedCompletionsCompat {
    let detected = detect_compat(model);
    let Some(compat) = model.compat.as_ref() else {
        return detected;
    };
    ResolvedCompletionsCompat {
        supports_store: compat.supports_store.unwrap_or(detected.supports_store),
        supports_developer_role: compat
            .supports_developer_role
            .unwrap_or(detected.supports_developer_role),
        supports_reasoning_effort: compat
            .supports_reasoning_effort
            .unwrap_or(detected.supports_reasoning_effort),
        supports_usage_in_streaming: compat
            .supports_usage_in_streaming
            .unwrap_or(detected.supports_usage_in_streaming),
        supports_finish_reason: compat
            .supports_finish_reason
            .unwrap_or(detected.supports_finish_reason),
        max_tokens_field: compat.max_tokens_field.unwrap_or(detected.max_tokens_field),
        requires_tool_result_name: compat
            .requires_tool_result_name
            .unwrap_or(detected.requires_tool_result_name),
        requires_assistant_after_tool_result: compat
            .requires_assistant_after_tool_result
            .unwrap_or(detected.requires_assistant_after_tool_result),
        requires_thinking_as_text: compat
            .requires_thinking_as_text
            .unwrap_or(detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: compat
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
        thinking_format: compat.thinking_format.unwrap_or(detected.thinking_format),
        open_router_routing: match &compat.open_router_routing {
            Some(routing) => Some(routing.clone()),
            None => Some(crate::ai::types::OpenRouterRouting::default()),
        },
        vercel_gateway_routing: compat
            .vercel_gateway_routing
            .clone()
            .or(detected.vercel_gateway_routing),
        chat_template_kwargs: compat
            .chat_template_kwargs
            .clone()
            .or(detected.chat_template_kwargs),
        chat_template_args: compat
            .chat_template_args
            .clone()
            .or(detected.chat_template_args),
        zai_tool_stream: compat.zai_tool_stream.unwrap_or(detected.zai_tool_stream),
        supports_thinking_token_budget: compat
            .supports_thinking_token_budget
            .or(detected.supports_thinking_token_budget),
        thinking_token_budget_field: compat
            .thinking_token_budget_field
            .or(detected.thinking_token_budget_field),
        supports_strict_mode: compat
            .supports_strict_mode
            .unwrap_or(detected.supports_strict_mode),
        supports_openai_grammar_tools: compat
            .supports_open_ai_grammar_tools
            .unwrap_or(detected.supports_openai_grammar_tools),
        cache_control_format: compat
            .cache_control_format
            .or(detected.cache_control_format),
        send_session_affinity_headers: compat
            .send_session_affinity_headers
            .unwrap_or(detected.send_session_affinity_headers),
        deferred_tools_mode: compat.deferred_tools_mode.or(detected.deferred_tools_mode),
        session_affinity_format: compat
            .session_affinity_format
            .unwrap_or(detected.session_affinity_format),
        supports_long_cache_retention: compat
            .supports_long_cache_retention
            .unwrap_or(detected.supports_long_cache_retention),
    }
}

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

/// Port of `parseChunkUsage`. Cache-read placement varies by provider
/// (prompt_tokens_details.cached_tokens, DeepSeek's
/// prompt_cache_hit_tokens, Kimi's top-level cached_tokens); writes come
/// from prompt_tokens_details.cache_write_tokens.
fn parse_chunk_usage(raw_usage: &Value, model: &Model) -> crate::ai::types::Usage {
    let get_u64 = |key: &str| raw_usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let prompt_tokens = get_u64("prompt_tokens");
    let details = raw_usage.get("prompt_tokens_details");
    let cache_read_tokens = details
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            raw_usage
                .get("prompt_cache_hit_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| raw_usage.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write_tokens = details
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let input = prompt_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens);
    let output_tokens = get_u64("completion_tokens");
    let reasoning = raw_usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut usage = crate::ai::types::Usage {
        input,
        output: output_tokens,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        reasoning: Some(reasoning),
        total_tokens: input + output_tokens + cache_read_tokens + cache_write_tokens,
        ..Default::default()
    };
    calculate_cost(model, &mut usage);
    usage
}

/// Port of `mapStopReason` for finish reasons.
fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (
            StopReason::Error,
            Some("Provider finish_reason: content_filter".to_string()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("Provider finish_reason: network_error".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

/// Builds the request headers (port of `createClient`'s header assembly).
#[allow(clippy::too_many_arguments)]
fn build_request_headers(
    model: &Model,
    context: &Context,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &ResolvedCompletionsCompat,
) -> Vec<(String, String)> {
    let mut headers: ProviderHeaders = ProviderHeaders::new();
    headers.insert(
        "User-Agent".to_string(),
        Some(crate::ai::session_resources::get_pi_user_agent()),
    );
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            headers.insert(name.clone(), Some(value.clone()));
        }
    }
    if model.provider == "github-copilot" {
        let has_images = has_copilot_vision_input(&context.messages);
        for (name, value) in build_copilot_dynamic_headers(&context.messages, has_images) {
            headers.insert(name, Some(value));
        }
    }

    if let Some(session_id) = session_id
        && compat.send_session_affinity_headers
    {
        match compat.session_affinity_format {
            crate::ai::types::SessionAffinityFormat::Openrouter => {
                headers.insert("x-session-id".to_string(), Some(session_id.to_string()));
            }
            crate::ai::types::SessionAffinityFormat::Openai => {
                headers.insert("session_id".to_string(), Some(session_id.to_string()));
                headers.insert(
                    "x-client-request-id".to_string(),
                    Some(session_id.to_string()),
                );
                headers.insert(
                    "x-session-affinity".to_string(),
                    Some(session_id.to_string()),
                );
            }
            crate::ai::types::SessionAffinityFormat::OpenaiNosession => {
                headers.insert(
                    "x-client-request-id".to_string(),
                    Some(session_id.to_string()),
                );
                headers.insert(
                    "x-session-affinity".to_string(),
                    Some(session_id.to_string()),
                );
            }
        }
    }

    // Merge options headers last so they can override defaults.
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            headers.insert(name.clone(), value.clone());
        }
    }

    // The OpenAI SDK deletes a default header named by a null entry —
    // including its own `Authorization: Bearer <key>` auth header — and a
    // non-null entry replaces it, so SDK auth is only sent when the merged
    // headers leave `authorization` unset. The SDK matches header names
    // case-insensitively.
    let mut flat: Vec<(String, String)> = Vec::new();
    if !headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        flat.push(("authorization".to_string(), format!("Bearer {api_key}")));
    }
    for (name, value) in headers {
        if let Some(value) = value {
            flat.push((name, value));
        }
    }
    flat
}

/// Grammar tool input properties keyed by tool name.
type GrammarToolInputProperties = BTreeMap<String, String>;

/// A streaming block ordering entry: the position in `output.content` plus
/// tool-call scratch state. Port of the TS `StreamingBlock` union with its
/// scratch fields.
#[derive(Clone, Debug)]
enum StreamingBlock {
    Text {
        position: usize,
    },
    Thinking {
        position: usize,
    },
    ToolCall {
        position: usize,
        scratch: ToolCallScratch,
    },
}

/// A streaming tool-call block's scratch state, keyed by the block's
/// position in `output.content`. Port of the TS `StreamingToolCallBlock`.
#[derive(Clone, Debug)]
struct ToolCallScratch {
    partial_args: Option<String>,
    custom_input: Option<(String, GrammarToolInputJsonBuffer)>,
}

/// Per-run streaming scratch. Port of the TS closure state.
struct StreamScratch {
    blocks: Vec<StreamingBlock>,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    has_finish_reason: bool,
    tool_call_blocks_by_index: BTreeMap<u64, usize>,
    tool_call_blocks_by_id: BTreeMap<String, usize>,
    streamed_reasoning_details: Option<Vec<Value>>,
}

impl StreamScratch {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            text_block: None,
            thinking_block: None,
            has_finish_reason: false,
            tool_call_blocks_by_index: BTreeMap::new(),
            tool_call_blocks_by_id: BTreeMap::new(),
            streamed_reasoning_details: None,
        }
    }

    fn content_index(&self, position: usize) -> usize {
        position
    }

    fn tool_call_scratch(&mut self, position: usize) -> Option<&mut ToolCallScratch> {
        self.blocks.iter_mut().find_map(|block| match block {
            StreamingBlock::ToolCall {
                position: p,
                scratch,
            } if *p == position => Some(scratch),
            _ => None,
        })
    }

    fn tool_call_scratch_at(&self, position: usize) -> Option<&ToolCallScratch> {
        self.blocks.iter().find_map(|block| match block {
            StreamingBlock::ToolCall {
                position: p,
                scratch,
            } if *p == position => Some(scratch),
            _ => None,
        })
    }
}

fn parse_openai_reasoning_details_from_signature(signature: Option<&str>) -> Option<Vec<Value>> {
    let signature = signature?;
    let parsed: Value = serde_json::from_str(signature).ok()?;
    let values = parsed.as_array()?;
    if values.is_empty() || !values.iter().all(is_openai_reasoning_detail) {
        return None;
    }
    Some(values.clone())
}

/// Port of `isOpenAIReasoningDetail`.
fn is_openai_reasoning_detail(detail: &Value) -> bool {
    let Some(object) = detail.as_object() else {
        return false;
    };
    let common_valid = object
        .get("id")
        .is_none_or(|id| id.is_null() || id.is_string())
        && object.get("format").is_none_or(|format| format.is_string())
        && object.get("index").is_none_or(|index| index.is_number());
    if !common_valid {
        return false;
    }
    match object.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => object.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => object.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            object.get("text").is_some_and(Value::is_string)
                && object
                    .get("signature")
                    .is_none_or(|signature| signature.is_null() || signature.is_string())
        }
        _ => false,
    }
}

/// Port of `parseOpenAIReasoningDetails`.
fn parse_openai_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    parse_openai_reasoning_details_from_signature(signature)
}

/// Port of `parseLegacyEncryptedReasoningDetail`.
fn parse_legacy_encrypted_reasoning_detail(signature: Option<&str>) -> Option<Value> {
    let signature = signature?;
    let parsed: Value = serde_json::from_str(signature).ok()?;
    let object = parsed.as_object()?;
    let valid = is_openai_reasoning_detail(&parsed)
        && object.get("type").and_then(Value::as_str) == Some("reasoning.encrypted")
        && object
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && object
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|data| !data.is_empty());
    valid.then_some(parsed)
}

fn fill_missing_common_fields(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    if !target.contains_key("id")
        && let Some(id) = source.get("id")
    {
        target.insert("id".to_string(), id.clone());
    }
    if !target.contains_key("format")
        && let Some(format) = source.get("format")
    {
        target.insert("format".to_string(), format.clone());
    }
    if !target.contains_key("index")
        && let Some(index) = source.get("index")
    {
        target.insert("index".to_string(), index.clone());
    }
}

fn append_openai_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let Some(detail_object) = detail.as_object() else {
        return;
    };
    let detail_type = detail_object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(last) = details.last_mut()
        && let Some(last_object) = last.as_object_mut()
    {
        let last_type = last_object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if detail_type == "reasoning.text" && last_type == "reasoning.text" {
            let text = detail_object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let combined = format!(
                "{}{text}",
                last_object
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            );
            last_object.insert("text".to_string(), Value::String(combined));
            if let Some(signature) = detail_object.get("signature")
                && last_object
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .is_empty()
            {
                last_object.insert("signature".to_string(), signature.clone());
            }
            fill_missing_common_fields(last_object, detail_object);
            return;
        }
        if detail_type == "reasoning.summary" && last_type == "reasoning.summary" {
            let summary = detail_object
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let combined = format!(
                "{}{summary}",
                last_object
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            );
            last_object.insert("summary".to_string(), Value::String(combined));
            fill_missing_common_fields(last_object, detail_object);
            return;
        }
    }
    details.push(detail.clone());
}

/// Port of `convertTools`.
fn convert_tools(tools: &[Tool], compat: &ResolvedCompletionsCompat) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let grammar =
                resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)?;
            if let Some(grammar) = grammar {
                let syntax = if grammar.format
                    == crate::ai::api::constrained_sampling::GrammarFormat::Lark
                {
                    "lark"
                } else {
                    "regex"
                };
                return Ok(json!({
                    "type": "custom",
                    "custom": {
                        "name": tool.name,
                        "description": tool.description,
                        "format": {
                            "type": "grammar",
                            "grammar": {
                                "syntax": syntax,
                                "definition": grammar.definition,
                            },
                        },
                    },
                }));
            }

            let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)?;
            let parameters = get_json_schema_tool_parameters(tool, strict == Some(true))?;
            let mut function = Map::new();
            function.insert("name".to_string(), json!(tool.name));
            function.insert("description".to_string(), json!(tool.description));
            function.insert("parameters".to_string(), parameters);
            // Only include strict if the provider supports it; some reject
            // unknown fields.
            if compat.supports_strict_mode {
                function.insert("strict".to_string(), json!(strict.unwrap_or(false)));
            }
            Ok(json!({
                "type": "function",
                "function": Value::Object(function),
            }))
        })
        .collect()
}

/// Normalizes tool call IDs for Chat Completions replay.
fn normalize_tool_call_id_fn(
    model: &Model,
) -> impl Fn(&str, &crate::ai::types::AssistantMessage) -> String + '_ {
    move |id: &str, _assistant: &crate::ai::types::AssistantMessage| {
        if id.contains('|') {
            let separator_index = id.find('|').unwrap_or(0);
            let sanitize = |text: &str| -> String {
                text.chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                            ch
                        } else {
                            '_'
                        }
                    })
                    .collect()
            };
            let call_id = sanitize(&id[..separator_index]);
            let item_id = sanitize(&id[separator_index + 1..]);
            let combined_id = if item_id.is_empty() {
                call_id.clone()
            } else {
                format!("{call_id}_{item_id}")
            };
            if combined_id.chars().count() <= 40 {
                return combined_id;
            }
            let hash: String = short_hash(id).chars().take(8).collect();
            let keep = (40usize)
                .saturating_sub(hash.chars().count())
                .saturating_sub(1)
                .max(1);
            let prefix: String = call_id.chars().take(keep).collect();
            return format!("{prefix}_{hash}");
        }

        if model.provider == "openai" {
            return id.chars().take(40).collect();
        }
        id.to_string()
    }
}

/// Port of `convertMessages`.
#[allow(clippy::too_many_lines)]
pub fn convert_messages(
    model: &Model,
    context: &Context,
    compat: &ResolvedCompletionsCompat,
    grammar_tool_input_properties: &GrammarToolInputProperties,
) -> Result<Vec<Value>, String> {
    let mut params: Vec<Value> = Vec::new();

    let normalizer = normalize_tool_call_id_fn(model);
    let transformed_messages = transform_messages(&context.messages, model, Some(&normalizer));

    if let Some(system_prompt) = &context.system_prompt {
        let use_developer_role = model.reasoning && compat.supports_developer_role;
        let role = if use_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({
            "role": role,
            "content": sanitize_surrogates(system_prompt),
        }));
    }

    let mut last_role: Option<String> = None;
    let mut index = 0;
    while index < transformed_messages.len() {
        let message = &transformed_messages[index];

        // Some providers don't allow user messages directly after tool
        // results; insert a synthetic assistant message to bridge the gap.
        if compat.requires_assistant_after_tool_result
            && last_role.as_deref() == Some("toolResult")
            && matches!(message, Message::User(_))
        {
            params.push(json!({
                "role": "assistant",
                "content": "I have processed the tool results.",
            }));
        }

        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    params.push(json!({
                        "role": "user",
                        "content": sanitize_surrogates(text),
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            crate::ai::types::BlockContent::Text(text) => json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            crate::ai::types::BlockContent::Image(image) => json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", image.mime_type, image.data),
                                },
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        index += 1;
                        continue;
                    }
                    params.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut assistant_message = Map::new();
                assistant_message.insert("role".to_string(), json!("assistant"));
                // Some providers don't accept null content; empty string is
                // used when bridging is required.
                assistant_message.insert(
                    "content".to_string(),
                    if compat.requires_assistant_after_tool_result {
                        Value::String(String::new())
                    } else {
                        Value::Null
                    },
                );

                let assistant_text_parts: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                            Some(sanitize_surrogates(&text.text))
                        }
                        _ => None,
                    })
                    .collect();
                let assistant_text = assistant_text_parts.join("");

                let thinking_blocks: Vec<&ThinkingContent> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(thinking) => Some(thinking),
                        _ => None,
                    })
                    .collect();
                let tool_calls: Vec<&ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call),
                        _ => None,
                    })
                    .collect();
                let signed_reasoning_details = thinking_blocks.iter().find_map(|block| {
                    parse_openai_reasoning_details(block.thinking_signature.as_deref())
                });
                let legacy_reasoning_details: Vec<Value> = tool_calls
                    .iter()
                    .filter_map(|tool_call| {
                        parse_legacy_encrypted_reasoning_detail(
                            tool_call.thought_signature.as_deref(),
                        )
                    })
                    .collect();
                let preserved_reasoning_details = signed_reasoning_details.or_else(|| {
                    (!legacy_reasoning_details.is_empty())
                        .then_some(legacy_reasoning_details.clone())
                });

                let non_empty_thinking_blocks: Vec<&ThinkingContent> = thinking_blocks
                    .iter()
                    .filter(|block| !block.thinking.trim().is_empty())
                    .copied()
                    .collect();
                if !non_empty_thinking_blocks.is_empty() {
                    if compat.requires_thinking_as_text {
                        // Convert thinking blocks to plain text (no tags to
                        // avoid model mimicking them).
                        let thinking_text = non_empty_thinking_blocks
                            .iter()
                            .map(|block| sanitize_surrogates(&block.thinking))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut content = vec![json!({"type": "text", "text": thinking_text})];
                        for part in &assistant_text_parts {
                            content.push(json!({"type": "text", "text": part}));
                        }
                        assistant_message.insert("content".to_string(), Value::Array(content));
                    } else {
                        // Always send assistant content as a plain string.
                        if !assistant_text.is_empty() {
                            assistant_message.insert("content".to_string(), json!(assistant_text));
                        }

                        // reasoning_details is the structured alternative to
                        // a raw reasoning field.
                        if preserved_reasoning_details.is_none() {
                            let mut signature = non_empty_thinking_blocks
                                .first()
                                .and_then(|block| block.thinking_signature.clone());
                            if model.provider == "opencode-go"
                                && signature.as_deref() == Some("reasoning")
                            {
                                signature = Some("reasoning_content".to_string());
                            }
                            if let Some(signature) = signature
                                && matches!(
                                    signature.as_str(),
                                    "reasoning" | "reasoning_content" | "reasoning_text"
                                )
                            {
                                let joined = non_empty_thinking_blocks
                                    .iter()
                                    .map(|block| block.thinking.clone())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                assistant_message.insert(signature, json!(joined));
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_message.insert("content".to_string(), json!(assistant_text));
                }

                if !tool_calls.is_empty() {
                    let converted_calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|tool_call| {
                            let custom_input_property = grammar_tool_input_properties.get(&tool_call.name);
                            if let Some(custom_input_property) = custom_input_property {
                                let input = get_grammar_tool_input(
                                    &tool_call.name,
                                    &tool_call.arguments,
                                    custom_input_property,
                                )?;
                                return Ok(json!({
                                    "id": tool_call.id,
                                    "type": "custom",
                                    "custom": {
                                        "name": tool_call.name,
                                        "input": sanitize_surrogates(&input),
                                    },
                                }));
                            }
                            Ok(json!({
                                "id": tool_call.id,
                                "type": "function",
                                "function": {
                                    "name": tool_call.name,
                                    "arguments": serde_json::to_string(&tool_call.arguments).unwrap_or_default(),
                                },
                            }))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    assistant_message
                        .insert("tool_calls".to_string(), Value::Array(converted_calls));
                }
                if let Some(preserved) = preserved_reasoning_details {
                    assistant_message
                        .insert("reasoning_details".to_string(), Value::Array(preserved));
                }
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !assistant_message.contains_key("reasoning_content")
                {
                    assistant_message.insert("reasoning_content".to_string(), json!(""));
                }

                // Skip assistant messages with no content and no tool calls.
                let has_content = match assistant_message.get("content") {
                    Some(Value::String(text)) => !text.is_empty(),
                    Some(Value::Array(parts)) => !parts.is_empty(),
                    _ => false,
                };
                if !has_content && !assistant_message.contains_key("tool_calls") {
                    index += 1;
                    continue;
                }
                params.push(Value::Object(assistant_message));
            }
            Message::ToolResult(_) => {
                let mut image_blocks: Vec<Value> = Vec::new();
                let mut deferred_tool_names = BTreeSet::new();
                let mut cursor = index;

                while cursor < transformed_messages.len()
                    && matches!(transformed_messages[cursor], Message::ToolResult(_))
                {
                    if let Message::ToolResult(tool_message) = &transformed_messages[cursor] {
                        let text_result = tool_message
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                crate::ai::types::BlockContent::Text(text) => {
                                    Some(text.text.as_str())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let has_images = tool_message
                            .content
                            .iter()
                            .any(|block| matches!(block, crate::ai::types::BlockContent::Image(_)));

                        let has_text = !text_result.is_empty();
                        let tool_result_text = if has_text {
                            text_result
                        } else if has_images {
                            "(see attached image)".to_string()
                        } else {
                            "(no tool output)".to_string()
                        };
                        let mut tool_result_message = Map::new();
                        tool_result_message.insert("role".to_string(), json!("tool"));
                        tool_result_message.insert(
                            "content".to_string(),
                            json!(sanitize_surrogates(&tool_result_text)),
                        );
                        tool_result_message
                            .insert("tool_call_id".to_string(), json!(tool_message.tool_call_id));
                        if compat.requires_tool_result_name && !tool_message.tool_name.is_empty() {
                            tool_result_message
                                .insert("name".to_string(), json!(tool_message.tool_name));
                        }
                        params.push(Value::Object(tool_result_message));

                        if compat.deferred_tools_mode
                            == Some(crate::ai::types::DeferredToolsMode::Kimi)
                        {
                            for name in tool_message.added_tool_names.iter().flatten() {
                                deferred_tool_names.insert(name.clone());
                            }
                        }

                        if has_images && model.input.contains(&crate::ai::types::ModelInput::Image)
                        {
                            for block in &tool_message.content {
                                if let crate::ai::types::BlockContent::Image(image) = block {
                                    image_blocks.push(json!({
                                        "type": "image_url",
                                        "image_url": {
                                            "url": format!("data:{};base64,{}", image.mime_type, image.data),
                                        },
                                    }));
                                }
                            }
                        }
                    }
                    cursor += 1;
                }

                index = cursor - 1;

                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }));
                    }

                    let mut content = vec![
                        json!({"type": "text", "text": "Attached image(s) from tool result:"}),
                    ];
                    content.extend(image_blocks);
                    params.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                    last_role = Some("user".to_string());
                } else {
                    last_role = Some("toolResult".to_string());
                }

                if !deferred_tool_names.is_empty() {
                    let deferred_tools =
                        get_tools_by_name(context.tools.as_deref(), &deferred_tool_names);
                    if !deferred_tools.is_empty() {
                        let kimi_tool_message = json!({
                            "role": "system",
                            "tools": convert_tools(&deferred_tools, compat)?,
                        });
                        // Kimi accepts a system message with tools but omits
                        // the standard content field.
                        params.push(kimi_tool_message);
                    }
                }
                index += 1;
                continue;
            }
        }

        last_role = Some(
            match message {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
            }
            .to_string(),
        );
        index += 1;
    }

    Ok(params)
}

/// Port of `resolveChatTemplateKwargValue`.
fn resolve_chat_template_kwarg_value(
    model: &Model,
    reasoning_effort: Option<ThinkingLevel>,
    value: &crate::ai::types::ChatTemplateKwargValue,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    use crate::ai::types::ChatTemplateKwargValue as K;
    match value {
        K::Text(text) => Some(json!(text)),
        K::Number(number) => Some(json!(number)),
        K::Boolean(flag) => Some(json!(flag)),
        K::Null => Some(Value::Null),
        K::Variable {
            variable,
            omit_when_off,
        } => {
            let omit_when_off = omit_when_off.unwrap_or(false);
            if reasoning_effort.is_none() && omit_when_off {
                return None;
            }
            match variable {
                crate::ai::types::ThinkingVariable::Enabled => {
                    Some(json!(reasoning_effort.is_some()))
                }
                crate::ai::types::ThinkingVariable::Budget => {
                    thinking_budget.map(|budget| json!(budget))
                }
                crate::ai::types::ThinkingVariable::Effort => {
                    let level_key =
                        reasoning_effort.map(crate::ai::types::ModelThinkingLevel::from);
                    let mapped_value = level_key
                        .as_ref()
                        .and_then(|level| {
                            model
                                .thinking_level_map
                                .as_ref()
                                .and_then(|map| map.get(level))
                        })
                        .cloned()
                        .flatten()
                        .or_else(|| {
                            model
                                .thinking_level_map
                                .as_ref()
                                .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
                                .cloned()
                                .flatten()
                        });
                    match mapped_value {
                        Some(mapped) => Some(json!(mapped)),
                        None => reasoning_effort.map(|level| {
                            json!(match level {
                                ThinkingLevel::Minimal => "minimal",
                                ThinkingLevel::Low => "low",
                                ThinkingLevel::Medium => "medium",
                                ThinkingLevel::High => "high",
                                ThinkingLevel::Xhigh => "xhigh",
                                ThinkingLevel::Max => "max",
                            })
                        }),
                    }
                }
            }
        }
    }
}

fn build_chat_template_values(
    model: &Model,
    reasoning_effort: Option<ThinkingLevel>,
    values: Option<&crate::ai::types::ChatTemplateKwargs>,
    thinking_budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let values = values?;
    let mut resolved_values = Map::new();
    for (key, value) in values {
        if let Some(resolved) =
            resolve_chat_template_kwarg_value(model, reasoning_effort, value, thinking_budget)
        {
            resolved_values.insert(key.clone(), resolved);
        }
    }
    (!resolved_values.is_empty()).then_some(resolved_values)
}

fn resolve_thinking_token_budget_field(
    compat: &ResolvedCompletionsCompat,
) -> Option<ThinkingTokenBudgetField> {
    if let Some(field) = compat.thinking_token_budget_field {
        return Some(field);
    }
    if compat.supports_thinking_token_budget == Some(true) {
        return Some(ThinkingTokenBudgetField::ThinkingTokenBudget);
    }
    None
}

/// Resolves the mapped reasoning-effort string for a level, honoring the
/// model's thinkingLevelMap.
fn mapped_effort(model: &Model, level: ThinkingLevel) -> Option<String> {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::from(level)))
        .cloned()
        .flatten()
        .or_else(|| {
            Some(
                match level {
                    ThinkingLevel::Minimal => "minimal",
                    ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    ThinkingLevel::High => "high",
                    ThinkingLevel::Xhigh => "xhigh",
                    ThinkingLevel::Max => "max",
                }
                .to_string(),
            )
        })
}

fn off_level_value(model: &Model) -> Option<String> {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
        .cloned()
        .flatten()
}

fn off_level_supported(model: &Model) -> bool {
    // `thinkingLevelMap.off === null` marks "cannot disable thinking".
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
        .map(|value| value.is_some())
        .unwrap_or(true)
}

fn get_compat_cache_control(
    compat: &ResolvedCompletionsCompat,
    cache_retention: CacheRetention,
) -> Option<Value> {
    if compat.cache_control_format != Some(crate::ai::types::CacheControlFormat::Anthropic)
        || cache_retention == CacheRetention::None
    {
        return None;
    }

    let ttl = (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
        .then(|| json!("1h"));
    Some(match ttl {
        Some(ttl) => json!({"type": "ephemeral", "ttl": ttl}),
        None => json!({"type": "ephemeral"}),
    })
}

/// Port of the thinking-format request-field switch in `buildParams`.
#[allow(clippy::too_many_lines)]
fn apply_thinking_format(
    model: &Model,
    compat: &ResolvedCompletionsCompat,
    params: &mut Value,
    reasoning_effort: Option<ThinkingLevel>,
    thinking_budget: Option<u64>,
) {
    let has_effort = reasoning_effort.is_some();
    if !model.reasoning {
        return;
    }
    match compat.thinking_format {
        crate::ai::types::ThinkingFormat::Zai => {
            params["thinking"] = if has_effort {
                json!({"type": "enabled", "clear_thinking": false})
            } else {
                json!({"type": "disabled"})
            };
            if has_effort
                && compat.supports_reasoning_effort
                && let Some(effort) = mapped_effort(model, reasoning_effort.expect("checked"))
            {
                params["reasoning_effort"] = json!(effort);
            }
        }
        crate::ai::types::ThinkingFormat::Qwen => {
            params["enable_thinking"] = json!(has_effort);
            if has_effort
                && compat.supports_reasoning_effort
                && let Some(effort) = mapped_effort(model, reasoning_effort.expect("checked"))
            {
                params["reasoning_effort"] = json!(effort);
            }
        }
        crate::ai::types::ThinkingFormat::QwenChatTemplate => {
            params["chat_template_kwargs"] =
                json!({"enable_thinking": has_effort, "preserve_thinking": true});
        }
        crate::ai::types::ThinkingFormat::ChatTemplate => {
            if let Some(kwargs) = build_chat_template_values(
                model,
                reasoning_effort,
                compat.chat_template_kwargs.as_ref(),
                thinking_budget,
            ) {
                params["chat_template_kwargs"] = Value::Object(kwargs);
            }
        }
        crate::ai::types::ThinkingFormat::Baseten => {
            if let Some(args) = build_chat_template_values(
                model,
                reasoning_effort,
                compat.chat_template_args.as_ref(),
                thinking_budget,
            ) {
                params["chat_template_args"] = Value::Object(args);
            }
            if compat.supports_reasoning_effort {
                let mapped = match reasoning_effort {
                    Some(level) => {
                        let key = crate::ai::types::ModelThinkingLevel::from(level);
                        model
                            .thinking_level_map
                            .as_ref()
                            .and_then(|map| map.get(&key))
                            .cloned()
                            .flatten()
                            .or_else(|| mapped_effort(model, level))
                    }
                    None => off_level_value(model),
                };
                if let Some(effort) = mapped {
                    params["reasoning_effort"] = json!(effort);
                }
            }
        }
        crate::ai::types::ThinkingFormat::Deepseek => {
            if has_effort {
                params["thinking"] = json!({"type": "enabled"});
            } else if off_level_supported(model) {
                params["thinking"] = json!({"type": "disabled"});
            }
            if has_effort
                && compat.supports_reasoning_effort
                && let Some(effort) = mapped_effort(model, reasoning_effort.expect("checked"))
            {
                params["reasoning_effort"] = json!(effort);
            }
        }
        crate::ai::types::ThinkingFormat::Openrouter => {
            // OpenRouter normalizes reasoning across providers via a nested
            // reasoning object.
            if has_effort {
                params["reasoning"] =
                    json!({"effort": mapped_effort(model, reasoning_effort.expect("checked"))});
            } else if off_level_supported(model) {
                let off = off_level_value(model).unwrap_or_else(|| "none".to_string());
                params["reasoning"] = json!({"effort": off});
            }
        }
        crate::ai::types::ThinkingFormat::AntLing => {
            if let Some(level) = reasoning_effort {
                let key = crate::ai::types::ModelThinkingLevel::from(level);
                let effort = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&key))
                    .cloned()
                    .flatten();
                if let Some(effort) = effort {
                    params["reasoning"] = json!({"effort": effort});
                }
            }
        }
        crate::ai::types::ThinkingFormat::Together => {
            params["reasoning"] = json!({"enabled": has_effort});
            if has_effort
                && compat.supports_reasoning_effort
                && let Some(effort) = mapped_effort(model, reasoning_effort.expect("checked"))
            {
                params["reasoning_effort"] = json!(effort);
            }
        }
        crate::ai::types::ThinkingFormat::StringThinking => {
            if has_effort {
                if let Some(effort) = mapped_effort(model, reasoning_effort.expect("checked")) {
                    params["thinking"] = json!(effort);
                }
            } else if off_level_supported(model) {
                let off = off_level_value(model).unwrap_or_else(|| "none".to_string());
                params["thinking"] = json!(off);
            }
        }
        crate::ai::types::ThinkingFormat::Openai => {
            if has_effort
                && compat.supports_reasoning_effort
                && let Some(effort) = mapped_effort(model, reasoning_effort.expect("checked"))
            {
                params["reasoning_effort"] = json!(effort);
            } else if !has_effort
                && compat.supports_reasoning_effort
                && let Some(off_value) = off_level_value(model)
            {
                params["reasoning_effort"] = json!(off_value);
            }
        }
    }
}

/// Port of `applyAnthropicCacheControl`: cache_control markers on the system
/// prompt, last tool definition, and last conversation message.
fn apply_anthropic_cache_control(params: &mut Value, cache_control: Value) {
    add_cache_control_to_system_prompt(params, &cache_control);
    add_cache_control_to_last_tool(params, &cache_control);
    add_cache_control_to_last_conversation_message(params, &cache_control);
}

fn add_cache_control_to_system_prompt(params: &mut Value, cache_control: &Value) {
    let Some(messages) = params.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if matches!(
            message.get("role").and_then(Value::as_str),
            Some("system") | Some("developer")
        ) {
            add_cache_control_to_text_content(message, cache_control);
            return;
        }
    }
}

fn add_cache_control_to_last_conversation_message(params: &mut Value, cache_control: &Value) {
    let Some(messages) = params.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut().rev() {
        if matches!(
            message.get("role").and_then(Value::as_str),
            Some("user") | Some("assistant") | Some("tool")
        ) && add_cache_control_to_text_content(message, cache_control)
        {
            return;
        }
    }
}

fn add_cache_control_to_last_tool(params: &mut Value, cache_control: &Value) {
    let Some(tools) = params.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    if let Some(last_tool) = tools.last_mut()
        && let Some(object) = last_tool.as_object_mut()
    {
        object.insert("cache_control".to_string(), cache_control.clone());
    }
}

fn add_cache_control_to_text_content(message: &mut Value, cache_control: &Value) -> bool {
    match message.get_mut("content") {
        Some(Value::String(text)) => {
            if text.is_empty() {
                return false;
            }
            let text = text.clone();
            message["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": cache_control,
            }]);
            true
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(object) = part.as_object_mut()
                {
                    object.insert("cache_control".to_string(), cache_control.clone());
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Port of `buildParams`.
#[allow(clippy::too_many_lines)]
pub fn build_params(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICompletionsOptions>,
    compat: &ResolvedCompletionsCompat,
    cache_retention: CacheRetention,
    grammar_tool_input_properties: &GrammarToolInputProperties,
) -> Result<Value, String> {
    let options = options.cloned().unwrap_or_default();
    let messages = convert_messages(model, context, compat, grammar_tool_input_properties)?;
    let cache_control = get_compat_cache_control(compat, cache_retention);

    let mut params = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
    });
    let prompt_cache_key = (model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention);
    if prompt_cache_key
        && let Some(key) = clamp_openai_prompt_cache_key(options.base.session_id.as_deref())
    {
        params["prompt_cache_key"] = json!(key);
    }
    if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        params["prompt_cache_retention"] = json!("24h");
    }

    if compat.supports_usage_in_streaming {
        params["stream_options"] = json!({ "include_usage": true });
    }

    if compat.supports_store {
        params["store"] = json!(false);
    }

    if let Some(max_tokens) = options.base.max_tokens
        && max_tokens > 0
    {
        if compat.max_tokens_field == crate::ai::types::MaxTokensField::MaxTokens {
            params["max_tokens"] = json!(max_tokens);
        } else {
            params["max_completion_tokens"] = json!(max_tokens);
        }
    }

    if let Some(temperature) = options.base.temperature {
        params["temperature"] = json!(temperature);
    }

    let deferred_tool_names =
        if compat.deferred_tools_mode == Some(crate::ai::types::DeferredToolsMode::Kimi) {
            get_deferred_tool_names(&context.messages)
        } else {
            BTreeSet::new()
        };
    let active_tools: Vec<Tool> = context
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter(|tool| !deferred_tool_names.contains(&tool.name))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !active_tools.is_empty() {
        params["tools"] = Value::Array(convert_tools(&active_tools, compat)?);
        if compat.zai_tool_stream {
            params["tool_stream"] = json!(true);
        }
    } else if has_tool_history(&context.messages) {
        // Anthropic (via LiteLLM/proxy) requires tools param when the
        // conversation has tool_calls/tool_results.
        params["tools"] = json!([]);
    }

    if let Some(cache_control) = cache_control {
        apply_anthropic_cache_control(&mut params, cache_control);
    }

    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = tool_choice.clone();
    }

    let reasoning_effort = options.reasoning_effort;
    let thinking_token_budget_field = resolve_thinking_token_budget_field(compat);
    // Port of resolveClampedThinkingBudget.
    let thinking_budget = if reasoning_effort.is_some() && model.reasoning {
        let ceiling = params
            .get("max_tokens")
            .and_then(Value::as_u64)
            .or_else(|| params.get("max_completion_tokens").and_then(Value::as_u64))
            .unwrap_or(model.max_tokens);
        let budget = reasoning_effort
            .map(|level| thinking_budget_for_level(level, options.thinking_budgets.as_ref()))
            .unwrap_or(0);
        let budget = clamp_thinking_budget_to_answer_room(budget, ceiling);
        (budget > 0).then_some(budget)
    } else {
        None
    };

    apply_thinking_format(
        model,
        compat,
        &mut params,
        reasoning_effort,
        thinking_budget,
    );

    // Cap reasoning with a top-level budget field. Independent of
    // thinkingFormat: reasoning and the answer share max_tokens here.
    if let Some(field) = thinking_token_budget_field
        && let Some(budget) = thinking_budget
    {
        let field_name = match field {
            ThinkingTokenBudgetField::ThinkingTokenBudget => "thinking_token_budget",
            ThinkingTokenBudgetField::ThinkingBudget => "thinking_budget",
            ThinkingTokenBudgetField::ThinkingBudgetTokens => "thinking_budget_tokens",
        };
        params[field_name] = json!(budget);
    }

    // OpenRouter provider routing preferences.
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.open_router_routing.as_ref())
    {
        params["provider"] = serde_json::to_value(routing).unwrap_or(Value::Null);
    }

    // Vercel AI Gateway provider routing preferences.
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.vercel_gateway_routing.as_ref())
        && (routing.only.is_some() || routing.order.is_some())
    {
        let mut gateway_options = Map::new();
        if let Some(only) = &routing.only {
            gateway_options.insert("only".to_string(), json!(only));
        }
        if let Some(order) = &routing.order {
            gateway_options.insert("order".to_string(), json!(order));
        }
        params["providerOptions"] = json!({ "gateway": Value::Object(gateway_options) });
    }

    // Last so custom keys override the named request fields.
    if let Some(sampling_params) = &options.base.sampling_params {
        for (key, value) in sampling_params {
            params[key.clone()] = value.clone();
        }
    }

    Ok(params)
}

/// The Rust replacement for the SDK client: assembles and issues the HTTP
/// request, returning the raw response.
async fn request_completions(
    model: &Model,
    context: &Context,
    api_key: &str,
    options: Option<&OpenAICompletionsOptions>,
    session_id: Option<&str>,
    compat: &ResolvedCompletionsCompat,
    params: Value,
) -> Result<crate::ai::utils::http::HttpResponse, String> {
    let headers = build_request_headers(
        model,
        context,
        api_key,
        options.and_then(|options| options.base.base.headers.as_ref()),
        session_id,
        compat,
    );
    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);
    let base_url = model.base_url.trim_end_matches('/');
    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url: format!("{base_url}/chat/completions"),
        headers,
        body: HttpBody::Json(params),
    };

    retry_http_request(
        &fetch,
        &request,
        crate::ai::utils::provider_retry::ProviderRetryOptions {
            max_retries: options.and_then(|options| options.base.base.max_retries),
            on_retry: options.and_then(|options| options.base.base.on_retry.clone()),
            max_retry_delay_ms: options.and_then(|options| options.base.base.max_retry_delay_ms),
            signal: options.and_then(|options| options.base.base.signal.clone()),
        },
    )
    .await
    .map_err(|error| error.message)
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICompletionsOptions>,
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

        let mut failure_details: Option<String> = None;
        let result = run_stream(
            &model,
            &context,
            options.as_ref(),
            &mut output,
            &producer,
            &mut failure_details,
        )
        .await;
        if let Err(error) = result {
            let aborted = options
                .as_ref()
                .and_then(|options| options.base.base.signal.as_ref())
                .is_some_and(|token| token.is_cancelled());
            // Apply streamed reasoning details to thinking blocks on failure.
            if let Some(details) = failure_details {
                for block in output.content.iter_mut() {
                    if let AssistantContent::Thinking(thinking) = block {
                        thinking.thinking_signature = Some(details.clone());
                    }
                }
            }
            output.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            let mut formatted = format_provider_error(
                &normalize_provider_error(ProviderErrorParts {
                    status: None,
                    body: None,
                    message: error.clone(),
                }),
                None,
            );
            // Some providers via OpenRouter add raw metadata; avoid printing
            // it twice.
            if let Some(raw_metadata) = error_raw_metadata(&error)
                && !formatted.contains(&raw_metadata)
            {
                formatted.push('\n');
                formatted.push_str(&raw_metadata);
            }
            output.error_message = Some(formatted);
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

/// Extracts `error.error.metadata.raw` from a serialized provider error body.
fn error_raw_metadata(error: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(error).ok()?;
    let raw = parsed.get("error")?.get("metadata")?.get("raw")?.clone();
    match raw {
        Value::String(text) => Some(text),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

#[allow(clippy::too_many_lines)]
async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICompletionsOptions>,
    output: &mut AssistantMessage,
    producer: &AssistantMessageEventStream,
    failure_details: &mut Option<String>,
) -> Result<(), String> {
    let api_key = get_client_api_key(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
    )?;
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_openai_grammar_tools,
    )?;
    let cache_retention = resolve_cache_retention(
        options.and_then(|options| options.base.cache_retention),
        options.and_then(|options| options.base.base.env.as_ref()),
    );
    let cache_session_id = if cache_retention == CacheRetention::None {
        None
    } else {
        options.and_then(|options| options.base.session_id.clone())
    };

    let mut params = build_params(
        model,
        context,
        options,
        &compat,
        cache_retention,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_params) = on_payload(params.clone(), model).await
    {
        params = next_params;
    }

    let response = request_completions(
        model,
        context,
        &api_key,
        options,
        cache_session_id.as_deref(),
        &compat,
        params,
    )
    .await?;

    if !(200..300).contains(&response.status) {
        let status = response.status;
        let body = crate::ai::utils::http::collect_text(response).await;
        let normalized = normalize_provider_error(ProviderErrorParts {
            status: Some(status),
            body: Some(body),
            message: format!("{status} status code"),
        });
        return Err(format_provider_error(&normalized, None));
    }

    if let Some(on_response) = options.and_then(|options| options.base.base.on_response.as_ref()) {
        let headers: BTreeMap<String, String> = response
            .headers
            .iter()
            .map(|(name, value)| (name.to_lowercase(), value.clone()))
            .collect();
        on_response(
            &crate::ai::types::ProviderResponse {
                status: response.status,
                headers,
            },
            model,
        )
        .await;
    }

    producer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let mut scratch = StreamScratch::new();

    let mut sse = crate::ai::utils::sse::SseStream::with_signal(
        response.body,
        options.and_then(|options| options.base.base.signal.clone()),
    );
    while let Some(sse_event) = futures::StreamExt::next(&mut sse).await {
        if sse_event.event.as_deref() == Some("__error__") {
            return Err(sse_event.data);
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&sse_event.data) else {
            continue;
        };
        process_chunk(
            &chunk,
            model,
            output,
            &mut scratch,
            &grammar_tool_input_properties,
            producer,
        );
    }

    // Finish all blocks in order.
    let positions: Vec<usize> = scratch
        .blocks
        .iter()
        .map(|block| match block {
            StreamingBlock::Text { position }
            | StreamingBlock::Thinking { position }
            | StreamingBlock::ToolCall { position, .. } => *position,
        })
        .collect();
    for position in positions {
        finish_block(output, &mut scratch, position, producer);
    }

    let signal = options.and_then(|options| options.base.base.signal.clone());
    if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason == StopReason::Aborted {
        return Err("Request was aborted".to_string());
    }
    if !scratch.has_finish_reason && !compat.supports_finish_reason {
        output.stop_reason = if output
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    }
    if output.stop_reason == StopReason::Error {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "Provider returned an error stop reason".to_string()));
    }
    if (compat.supports_finish_reason && !scratch.has_finish_reason)
        || output.stop_reason == StopReason::Pending
    {
        return Err("Stream ended without finish_reason".to_string());
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
    let _ = failure_details;
    Ok(())
}

/// Port of `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if let Err(error) = get_client_api_key(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
    ) {
        return error_stream(model, &error);
    }

    let options = options.cloned().unwrap_or_default();
    let base = build_base_options(
        model,
        context,
        Some(&options),
        options.base.base.api_key.as_deref(),
    );
    let clamped_reasoning = options.reasoning.map(|reasoning| {
        clamp_thinking_level(model, crate::ai::types::ModelThinkingLevel::from(reasoning))
    });
    let reasoning_effort = clamped_reasoning.and_then(|level| level.as_thinking_level());
    // TS forwards the shared tool choice verbatim (`toolChoice:
    // options?.toolChoice`); the serialized union matches that pass-through.
    let tool_choice = options
        .tool_choice
        .as_ref()
        .and_then(|choice| serde_json::to_value(choice).ok());

    stream(
        model,
        context,
        Some(&OpenAICompletionsOptions {
            base,
            tool_choice,
            reasoning_effort,
            thinking_budgets: options.thinking_budgets,
        }),
    )
}

/// Emits a setup failure as an error stream (for early auth failures).
pub(crate) fn error_stream(model: &Model, error: &str) -> AssistantMessageEventStream {
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

/// Port of the per-chunk streaming body: text, reasoning fields, tool call
/// deltas (function + custom grammar), and reasoning_details.
#[allow(clippy::too_many_lines)]
fn process_chunk(
    chunk: &Value,
    model: &Model,
    output: &mut AssistantMessage,
    scratch: &mut StreamScratch,
    grammar_tool_input_properties: &GrammarToolInputProperties,
    producer: &AssistantMessageEventStream,
) {
    if !chunk.is_object() {
        return;
    }

    // Each chunk in a streamed completion carries the same id.
    if output.response_id.is_none()
        && let Some(id) = chunk.get("id").and_then(Value::as_str)
    {
        output.response_id = Some(id.to_string());
    }
    if let Some(response_model) = chunk.get("model").and_then(Value::as_str)
        && !response_model.is_empty()
        && response_model != model.id
        && output.response_model.is_none()
    {
        output.response_model = Some(response_model.to_string());
    }
    if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
        output.usage = parse_chunk_usage(usage, model);
    }

    let choice = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let Some(choice) = choice else {
        return;
    };

    // Fallback: some providers (e.g., Moonshot) return usage in choice.usage.
    if chunk.get("usage").is_none()
        && let Some(usage) = choice.get("usage").filter(|usage| usage.is_object())
    {
        output.usage = parse_chunk_usage(usage, model);
    }

    if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
        output.raw_stop_reason = Some(finish_reason.to_string());
        let (stop_reason, error_message) = map_stop_reason(finish_reason);
        output.stop_reason = stop_reason;
        if let Some(error_message) = error_message {
            output.error_message = Some(error_message);
        }
        scratch.has_finish_reason = true;
    }

    let Some(delta) = choice.get("delta") else {
        return;
    };

    if let Some(content) = delta.get("content").and_then(Value::as_str)
        && !content.is_empty()
    {
        let position = ensure_text_block(output, scratch, producer);
        if let Some(AssistantContent::Text(block)) = output.content.get_mut(position) {
            block.text.push_str(content);
        }
        producer.push(AssistantMessageEvent::TextDelta {
            content_index: scratch.content_index(position),
            delta: content.to_string(),
            partial: output.clone(),
        });
    }

    // Some endpoints return reasoning in reasoning_content (llama.cpp),
    // reasoning (other OpenAI-compatible endpoints), or reasoning_text. Use
    // the first non-empty field to avoid duplication (chutes.ai returns two
    // with the same content).
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        let Some(value) = delta.get(field).and_then(Value::as_str) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let thinking_signature = if model.provider == "opencode-go" && field == "reasoning" {
            "reasoning_content"
        } else {
            field
        };
        let position = ensure_thinking_block(output, scratch, producer, thinking_signature);
        if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
            block.thinking.push_str(value);
        }
        producer.push(AssistantMessageEvent::ThinkingDelta {
            content_index: scratch.content_index(position),
            delta: value.to_string(),
            partial: output.clone(),
        });
        break;
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let block_position = ensure_tool_call_block(
                output,
                scratch,
                tool_call,
                producer,
                grammar_tool_input_properties,
            );

            // Adopt id/name deltas.
            if let Some(id) = tool_call.get("id").and_then(Value::as_str)
                && let Some(AssistantContent::ToolCall(block)) =
                    output.content.get_mut(block_position)
                && block.id.is_empty()
            {
                block.id = id.to_string();
                scratch
                    .tool_call_blocks_by_id
                    .entry(id.to_string())
                    .or_insert(block_position);
            }
            let name = tool_call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .or_else(|| {
                    tool_call
                        .get("custom")
                        .and_then(|custom| custom.get("name"))
                        .and_then(Value::as_str)
                });
            if let Some(name) = name
                && let Some(AssistantContent::ToolCall(block)) =
                    output.content.get_mut(block_position)
                && block.name.is_empty()
            {
                block.name = name.to_string();
            }

            let mut delta_text = String::new();
            let function_arguments = tool_call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str);
            let custom_input = tool_call
                .get("custom")
                .and_then(|custom| custom.get("input"))
                .and_then(Value::as_str);
            if let Some(arguments) = function_arguments {
                delta_text = arguments.to_string();
                let partial_args = {
                    let existing = scratch
                        .tool_call_scratch(block_position)
                        .and_then(|state| state.partial_args.clone())
                        .unwrap_or_default();
                    format!("{existing}{arguments}")
                };
                if let Some(state) = scratch.tool_call_scratch(block_position) {
                    state.partial_args = Some(partial_args.clone());
                }
                if let Some(AssistantContent::ToolCall(block)) =
                    output.content.get_mut(block_position)
                {
                    block.arguments = parse_streaming_json(Some(&partial_args))
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                }
            } else if let Some(input) = custom_input {
                // Custom (grammar) tool call input delta.
                let property = scratch.tool_call_scratch(block_position).and_then(|state| {
                    state
                        .custom_input
                        .as_ref()
                        .map(|(property, _)| property.clone())
                });
                let current = property
                    .as_ref()
                    .and_then(|property| {
                        output
                            .content
                            .get(block_position)
                            .and_then(|block| match block {
                                AssistantContent::ToolCall(tool_call) => tool_call
                                    .arguments
                                    .get(property)
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                _ => None,
                            })
                    })
                    .unwrap_or_default();
                let next_input = format!("{current}{input}");
                if let Some(state) = scratch.tool_call_scratch(block_position)
                    && let Some((prop, buffer)) = &mut state.custom_input
                {
                    let prop = prop.clone();
                    if let Ok(Some(d)) =
                        append_grammar_tool_input_json_delta(buffer, &prop, &next_input, false)
                    {
                        delta_text = d;
                    }
                }
                if let Some(prop) = property
                    && let Some(AssistantContent::ToolCall(block)) =
                        output.content.get_mut(block_position)
                {
                    block.arguments = [(prop, Value::String(next_input))].into_iter().collect();
                }
            }
            producer.push(AssistantMessageEvent::ToolcallDelta {
                content_index: scratch.content_index(block_position),
                delta: delta_text,
                partial: output.clone(),
            });
        }
    }

    if let Some(reasoning_details) = delta.get("reasoning_details").and_then(Value::as_array) {
        for detail in reasoning_details {
            if !is_openai_reasoning_detail(detail) {
                continue;
            }
            ensure_thinking_block(output, scratch, producer, "");
            scratch
                .streamed_reasoning_details
                .get_or_insert_with(Vec::new);
            // Keep provider replay data in the existing signature slot:
            // consecutive text/summary deltas merge, encrypted entries stay
            // opaque and discrete.
            let details = scratch
                .streamed_reasoning_details
                .get_or_insert_with(Vec::new);
            append_openai_reasoning_detail(details, detail);
        }
    }
}

fn ensure_text_block(
    output: &mut AssistantMessage,
    scratch: &mut StreamScratch,
    producer: &AssistantMessageEventStream,
) -> usize {
    if let Some(position) = scratch.text_block {
        return position;
    }
    output
        .content
        .push(AssistantContent::Text(TextContent::default()));
    let position = output.content.len() - 1;
    scratch.text_block = Some(position);
    scratch.blocks.push(StreamingBlock::Text { position });
    producer.push(AssistantMessageEvent::TextStart {
        content_index: scratch.content_index(position),
        partial: output.clone(),
    });
    position
}

fn ensure_thinking_block(
    output: &mut AssistantMessage,
    scratch: &mut StreamScratch,
    producer: &AssistantMessageEventStream,
    thinking_signature: &str,
) -> usize {
    if let Some(position) = scratch.thinking_block {
        return position;
    }
    output
        .content
        .push(AssistantContent::Thinking(ThinkingContent {
            thinking_signature: Some(thinking_signature.to_string()),
            ..Default::default()
        }));
    let position = output.content.len() - 1;
    scratch.thinking_block = Some(position);
    scratch.blocks.push(StreamingBlock::Thinking { position });
    producer.push(AssistantMessageEvent::ThinkingStart {
        content_index: scratch.content_index(position),
        partial: output.clone(),
    });
    position
}

#[allow(clippy::too_many_lines)]
fn ensure_tool_call_block(
    output: &mut AssistantMessage,
    scratch: &mut StreamScratch,
    tool_call: &Value,
    producer: &AssistantMessageEventStream,
    grammar_tool_input_properties: &GrammarToolInputProperties,
) -> usize {
    let stream_index = tool_call.get("index").and_then(Value::as_u64);
    let name = tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            tool_call
                .get("custom")
                .and_then(|custom| custom.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string();
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut existing_position: Option<usize> = None;
    if let Some(stream_index) = stream_index
        && let Some(position) = scratch.tool_call_blocks_by_index.get(&stream_index)
    {
        existing_position = Some(*position);
    }
    if existing_position.is_none()
        && !id.is_empty()
        && let Some(position) = scratch.tool_call_blocks_by_id.get(&id)
    {
        existing_position = Some(*position);
    }

    let position = match existing_position {
        Some(position) => position,
        None => {
            // Note: the "input" fallback here should/must not be taken. In
            // case the LLM makes up a tool we don't know about, we at least
            // have a place to stash our stuff.
            let is_custom =
                tool_call.get("custom").is_some() && tool_call.get("function").is_none();
            let custom_input_property = if is_custom {
                grammar_tool_input_properties
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| "input".to_string())
            } else {
                "input".to_string()
            };
            let has_custom_input = is_custom;
            let mut arguments = ToolCallArguments::new();
            let scratch_state = if has_custom_input {
                arguments.insert(custom_input_property.clone(), Value::String(String::new()));
                ToolCallScratch {
                    partial_args: None,
                    custom_input: Some((
                        custom_input_property.clone(),
                        GrammarToolInputJsonBuffer::default(),
                    )),
                }
            } else {
                ToolCallScratch {
                    partial_args: Some(String::new()),
                    custom_input: None,
                }
            };
            let block = ToolCall {
                content_type: Default::default(),
                id: id.clone(),
                name: name.clone(),
                arguments,
                ..Default::default()
            };
            output.content.push(AssistantContent::ToolCall(block));
            let position = output.content.len() - 1;
            if let Some(stream_index) = stream_index {
                scratch
                    .tool_call_blocks_by_index
                    .insert(stream_index, position);
            }
            if !id.is_empty() {
                scratch.tool_call_blocks_by_id.insert(id.clone(), position);
            }
            scratch.blocks.push(StreamingBlock::ToolCall {
                position,
                scratch: scratch_state,
            });
            producer.push(AssistantMessageEvent::ToolcallStart {
                content_index: scratch.content_index(position),
                partial: output.clone(),
            });
            position
        }
    };

    // Adopt the stream index and name on the existing block.
    if let Some(stream_index) = stream_index {
        let needs_index = !scratch.blocks.iter().any(|block| {
            matches!(block, StreamingBlock::ToolCall { position: p, .. } if *p == position)
                && match block {
                    StreamingBlock::ToolCall {
                        scratch:
                            ToolCallScratch {
                                custom_input,
                                partial_args,
                            },
                        ..
                    } => {
                        let _ = (custom_input, partial_args);
                        false
                    }
                    _ => false,
                }
        });
        let _ = needs_index;
        scratch
            .tool_call_blocks_by_index
            .entry(stream_index)
            .or_insert(position);
    }
    if !id.is_empty() {
        scratch.tool_call_blocks_by_id.entry(id).or_insert(position);
    }
    if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position)
        && block.name.is_empty()
        && !name.is_empty()
    {
        block.name = name;
    }
    // A late custom delta on a block without custom state (TS: delete
    // partialArgs, set customInput).
    if tool_call.get("custom").is_some()
        && tool_call.get("function").is_none()
        && let Some(block_name) = output.content.get(position).and_then(|block| match block {
            AssistantContent::ToolCall(tool_call) => Some(tool_call.name.clone()),
            _ => None,
        })
    {
        let needs_custom = scratch
            .blocks
            .iter_mut()
            .find_map(|block| match block {
                StreamingBlock::ToolCall {
                    position: p,
                    scratch: state,
                } if *p == position => Some(state.custom_input.is_none()),
                _ => None,
            })
            .unwrap_or(false);
        if needs_custom {
            let custom_input_property = grammar_tool_input_properties
                .get(&block_name)
                .cloned()
                .unwrap_or_else(|| "input".to_string());
            if let Some(AssistantContent::ToolCall(tool_call_block)) =
                output.content.get_mut(position)
            {
                let mut arguments = ToolCallArguments::new();
                arguments.insert(custom_input_property.clone(), Value::String(String::new()));
                tool_call_block.arguments = arguments;
            }
            let property = custom_input_property.clone();
            for block in scratch.blocks.iter_mut() {
                if let StreamingBlock::ToolCall {
                    position: p,
                    scratch: state,
                } = block
                    && *p == position
                {
                    state.custom_input =
                        Some((property.clone(), GrammarToolInputJsonBuffer::default()));
                    state.partial_args = None;
                }
            }
        }
    }
    position
}

/// Port of `finishBlock`: closes a streaming block, emitting the terminal
/// event and applying streamed reasoning details.
fn finish_block(
    output: &mut AssistantMessage,
    scratch: &mut StreamScratch,
    position: usize,
    producer: &AssistantMessageEventStream,
) {
    let content_index = scratch.content_index(position);
    let Some(block) = output.content.get(position).cloned() else {
        return;
    };
    match block {
        AssistantContent::Text(text) => {
            producer.push(AssistantMessageEvent::TextEnd {
                content_index,
                content: text.text,
                partial: output.clone(),
            });
        }
        AssistantContent::Thinking(mut thinking) => {
            if let Some(details) = &scratch.streamed_reasoning_details {
                thinking.thinking_signature =
                    Some(serde_json::to_string(details).unwrap_or_default());
                // Port of `applyStreamedReasoningDetails`: the serialized
                // details must land on the block in `output.content`, not just
                // the terminal-event copy.
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                    *block = thinking.clone();
                }
            }
            producer.push(AssistantMessageEvent::ThinkingEnd {
                content_index,
                content: thinking.thinking.clone(),
                partial: output.clone(),
            });
        }
        AssistantContent::ToolCall(_) => {
            let state = scratch.tool_call_scratch_at(position);
            if let Some(state) = state {
                if let Some((custom_property, buffer)) = &state.custom_input {
                    let close_delta = state.custom_input.as_ref().and_then(|(property, _)| {
                        let input = output.content.get(position).and_then(|block| match block {
                            AssistantContent::ToolCall(call) => call
                                .arguments
                                .get(property)
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            _ => None,
                        });
                        input.map(|input| (property.clone(), input))
                    });
                    let _ = custom_property;
                    if let Some((property, input)) = close_delta {
                        let mut buffer_clone = buffer.clone();
                        let delta = append_grammar_tool_input_json_delta(
                            &mut buffer_clone,
                            &property,
                            &input,
                            true,
                        )
                        .unwrap_or_default();
                        if let Some(delta) = delta {
                            producer.push(AssistantMessageEvent::ToolcallDelta {
                                content_index,
                                delta,
                                partial: output.clone(),
                            });
                        }
                    }
                } else if let Some(partial_args) = &state.partial_args {
                    let arguments = parse_streaming_json(Some(partial_args));
                    if let Some(AssistantContent::ToolCall(args_block)) =
                        output.content.get_mut(position)
                    {
                        args_block.arguments = arguments.as_object().cloned().unwrap_or_default();
                    }
                }
            }
            if let Some(AssistantContent::ToolCall(tool_call)) = output.content.get(position) {
                let tool_call = tool_call.clone();
                producer.push(AssistantMessageEvent::ToolcallEnd {
                    content_index,
                    tool_call,
                    partial: output.clone(),
                });
            }
        }
    }
}
