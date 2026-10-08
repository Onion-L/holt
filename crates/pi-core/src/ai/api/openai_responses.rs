//! Port of `pi-core/ai/src/api/openai-responses.ts`.
//!
//! The `openai` SDK client is replaced by a direct
//! `POST {baseUrl}/responses` through the injectable transport; request
//! assembly and streaming event handling otherwise mirror the TypeScript
//! implementation (see [`openai_responses_shared`]).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::ai::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::ai::api::github_copilot_headers::{
    build_copilot_dynamic_headers, has_copilot_vision_input,
};
use crate::ai::api::openai_completions::clamp_openai_prompt_cache_key;
use crate::ai::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, DeferredToolsMode, ResponsesStreamOptions,
    convert_responses_messages, convert_responses_tools, process_responses_stream,
};
use crate::ai::api::simple_options::build_base_options;
use crate::ai::models::clamp_thinking_level;
use crate::ai::types::{
    CacheRetention, Context, Model, ProviderEnv, ProviderHeaders, SimpleStreamOptions,
    StreamOptions, ThinkingLevel, Usage,
};
use crate::ai::utils::deferred_tools::split_deferred_tools;
use crate::ai::utils::error_body::{
    ProviderErrorParts, format_provider_error, normalize_provider_error,
};
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::provider_env::get_provider_env_value;
use crate::ai::utils::provider_retry::retry_http_request;
use crate::ai::utils::reqwest_fetch::default_fetch;

/// Providers allowed to keep pipe-separated Responses tool-call ids on
/// replay.
fn openai_tool_call_providers() -> BTreeSet<String> {
    ["openai", "openai-codex", "opencode"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// OpenAI Responses rejects max_output_tokens below 16.
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;

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

fn detect_session_affinity_format(model: &Model) -> crate::ai::types::SessionAffinityFormat {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        crate::ai::types::SessionAffinityFormat::Openrouter
    } else {
        crate::ai::types::SessionAffinityFormat::Openai
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

/// Port of `getCompat`: resolved Responses compat flags.
pub struct ResolvedResponsesCompat {
    pub supports_developer_role: bool,
    pub session_affinity_format: crate::ai::types::SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub supports_explicit_prompt_cache_mode: bool,
}

pub fn get_compat(model: &Model) -> ResolvedResponsesCompat {
    let compat = model.compat.as_ref();
    ResolvedResponsesCompat {
        supports_developer_role: compat
            .and_then(|compat| compat.supports_developer_role)
            .unwrap_or(true),
        session_affinity_format: compat
            .and_then(|compat| compat.session_affinity_format)
            .unwrap_or_else(|| detect_session_affinity_format(model)),
        supports_long_cache_retention: compat
            .and_then(|compat| compat.supports_long_cache_retention)
            .unwrap_or(true),
        supports_strict_mode: compat
            .and_then(|compat| compat.supports_strict_mode)
            .unwrap_or(false),
        supports_openai_grammar_tools: compat
            .and_then(|compat| compat.supports_open_ai_grammar_tools)
            .unwrap_or(false),
        supports_additional_tools: compat
            .and_then(|compat| compat.supports_additional_tools)
            .unwrap_or(false),
        supports_tool_search: compat
            .and_then(|compat| compat.supports_tool_search)
            .unwrap_or(false),
        supports_explicit_prompt_cache_mode: compat
            .and_then(|compat| compat.supports_explicit_prompt_cache_mode)
            .unwrap_or(false),
    }
}

fn get_prompt_cache_retention(
    compat: &ResolvedResponsesCompat,
    cache_retention: CacheRetention,
) -> Option<String> {
    (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
        .then(|| "24h".to_string())
}

/// Port of `OpenAIResponsesOptions`.
#[derive(Clone, Default)]
pub struct OpenAIResponsesOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<ThinkingLevel>,
    /// "auto" | "detailed" | "concise" (null handled as None).
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    /// Tool choice carried as raw JSON (string or object).
    pub tool_choice: Option<Value>,
}

/// Builds request headers (port of `createClient`'s header assembly).
fn build_request_headers(
    model: &Model,
    context: &Context,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &ResolvedResponsesCompat,
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

    if let Some(session_id) = session_id {
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
            }
            crate::ai::types::SessionAffinityFormat::OpenaiNosession => {
                headers.insert(
                    "x-client-request-id".to_string(),
                    Some(session_id.to_string()),
                );
            }
        }
    }

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

/// Port of `buildParams`.
#[allow(clippy::too_many_lines)]
pub fn build_params(
    model: &Model,
    context: &Context,
    options: Option<&OpenAIResponsesOptions>,
    compat: &ResolvedResponsesCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let options = options.cloned().unwrap_or_default();
    let deferred_tools_mode = if compat.supports_additional_tools {
        Some(DeferredToolsMode::AdditionalTools)
    } else if compat.supports_tool_search {
        Some(DeferredToolsMode::ToolSearch)
    } else {
        None
    };
    let tool_placement = split_deferred_tools(context, deferred_tools_mode.is_some(), |name| {
        name.to_string()
    });
    let messages = convert_responses_messages(
        model,
        context,
        &openai_tool_call_providers(),
        Some(&ConvertResponsesMessagesOptions {
            include_system_prompt: None,
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            deferred_tools: Some(&tool_placement.deferred),
            deferred_tools_mode,
            tool_options: Some(
                crate::ai::api::openai_responses_shared::ConvertResponsesToolsOptions {
                    strict: None,
                    supports_strict_mode: Some(compat.supports_strict_mode),
                    supports_openai_grammar_tools: Some(compat.supports_openai_grammar_tools),
                    defer_loading: None,
                },
            ),
        }),
    )?;

    let cache_retention =
        resolve_cache_retention(options.base.cache_retention, options.base.base.env.as_ref());
    let disable_implicit_prompt_cache =
        cache_retention == CacheRetention::None && compat.supports_explicit_prompt_cache_mode;
    let mut params = json!({
        "model": model.id,
        "input": messages,
        "stream": true,
        "store": false,
    });
    if cache_retention != CacheRetention::None
        && let Some(key) = clamp_openai_prompt_cache_key(options.base.session_id.as_deref())
    {
        params["prompt_cache_key"] = json!(key);
    }
    if let Some(retention) = get_prompt_cache_retention(compat, cache_retention) {
        params["prompt_cache_retention"] = json!(retention);
    }
    if disable_implicit_prompt_cache {
        params["prompt_cache_options"] = json!({"mode": "explicit"});
    }

    if let Some(max_tokens) = options.base.max_tokens
        && max_tokens > 0
    {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }

    if let Some(temperature) = options.base.temperature {
        params["temperature"] = json!(temperature);
    }

    if let Some(service_tier) = &options.service_tier {
        params["service_tier"] = json!(service_tier);
    }

    if !tool_placement.immediate.is_empty() {
        params["tools"] = Value::Array(convert_responses_tools(
            &tool_placement.immediate,
            Some(
                &crate::ai::api::openai_responses_shared::ConvertResponsesToolsOptions {
                    strict: None,
                    supports_strict_mode: Some(compat.supports_strict_mode),
                    supports_openai_grammar_tools: Some(compat.supports_openai_grammar_tools),
                    defer_loading: None,
                },
            ),
        )?);
    }

    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = tool_choice.clone();
    }

    if model.reasoning {
        if options.reasoning_effort.is_some() || options.reasoning_summary.is_some() {
            let effort = match options.reasoning_effort {
                Some(level) => {
                    let key = crate::ai::types::ModelThinkingLevel::from(level);
                    model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|map| map.get(&key))
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| match level {
                            ThinkingLevel::Minimal => "minimal".to_string(),
                            ThinkingLevel::Low => "low".to_string(),
                            ThinkingLevel::Medium => "medium".to_string(),
                            ThinkingLevel::High => "high".to_string(),
                            ThinkingLevel::Xhigh => "xhigh".to_string(),
                            ThinkingLevel::Max => "max".to_string(),
                        })
                }
                None => "medium".to_string(),
            };
            let summary = options
                .reasoning_summary
                .clone()
                .unwrap_or_else(|| "auto".to_string());
            params["reasoning"] = json!({"effort": effort, "summary": summary});
            params["include"] = json!(["reasoning.encrypted_content"]);
        } else if model.provider != "github-copilot" {
            let off_supported = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
                .map(|value| value.is_some())
                .unwrap_or(true);
            if off_supported {
                let off = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "none".to_string());
                params["reasoning"] = json!({"effort": off});
            }
        }
        if model.provider == "xai" {
            params["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    // Last so custom keys override the named request fields.
    if let Some(sampling_params) = &options.base.sampling_params {
        for (key, value) in sampling_params {
            params[key.clone()] = value.clone();
        }
    }

    Ok(params)
}

/// Port of `getServiceTierCostMultiplier`.
fn get_service_tier_cost_multiplier(model: &Model, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model.id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// Port of `applyServiceTierPricing`.
fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model: &Model) {
    let multiplier = get_service_tier_cost_multiplier(model, service_tier);
    if multiplier == 1.0 {
        return;
    }

    usage.cost.input = (f64::from(usage.cost.input) * multiplier).into();
    usage.cost.output = (f64::from(usage.cost.output) * multiplier).into();
    usage.cost.cache_read = (f64::from(usage.cost.cache_read) * multiplier).into();
    usage.cost.cache_write = (f64::from(usage.cost.cache_write) * multiplier).into();
    usage.cost.total = (f64::from(usage.cost.input)
        + f64::from(usage.cost.output)
        + f64::from(usage.cost.cache_read)
        + f64::from(usage.cost.cache_write))
    .into();
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&OpenAIResponsesOptions>,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let model = model.clone();
    let context = context.clone();
    let options = options.cloned();
    let producer = stream.clone();
    tokio::spawn(async move {
        let mut output = crate::ai::types::AssistantMessage {
            role: crate::ai::types::RoleAssistant,
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Default::default(),
            stop_reason: crate::ai::types::StopReason::Pending,
            timestamp: crate::ai::auth::resolve::now_millis(),
            ..Default::default()
        };

        let result = run_stream(&model, &context, options.as_ref(), &mut output, &producer).await;
        if let Err(error) = result {
            let aborted = options
                .as_ref()
                .and_then(|options| options.base.base.signal.as_ref())
                .is_some_and(|token| token.is_cancelled());
            output.stop_reason = if aborted {
                crate::ai::types::StopReason::Aborted
            } else {
                crate::ai::types::StopReason::Error
            };
            let formatted = format_provider_error(
                &normalize_provider_error(ProviderErrorParts {
                    status: None,
                    body: None,
                    message: error,
                }),
                Some("OpenAI API error"),
            );
            output.error_message = Some(formatted);
            producer.push(crate::ai::types::AssistantMessageEvent::Error {
                reason: if output.stop_reason == crate::ai::types::StopReason::Aborted {
                    crate::ai::types::ErrorReason::Aborted
                } else {
                    crate::ai::types::ErrorReason::Error
                },
                error: output.clone(),
            });
            producer.end(None);
        }
    });
    stream
}

async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&OpenAIResponsesOptions>,
    output: &mut crate::ai::types::AssistantMessage,
    producer: &AssistantMessageEventStream,
) -> Result<(), String> {
    let api_key = get_client_api_key(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
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
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_openai_grammar_tools,
    )?;
    let mut params = build_params(
        model,
        context,
        options,
        &compat,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_params) = on_payload(params.clone(), model).await
    {
        params = next_params;
    }

    let headers = build_request_headers(
        model,
        context,
        &api_key,
        options.and_then(|options| options.base.base.headers.as_ref()),
        cache_session_id.as_deref(),
        &compat,
    );
    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);
    let base_url = model.base_url.trim_end_matches('/');
    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url: format!("{base_url}/responses"),
        headers,
        body: HttpBody::Json(params),
    };

    let response = retry_http_request(
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
    .map_err(|error| error.message)?;

    if !(200..300).contains(&response.status) {
        let status = response.status;
        let body = crate::ai::utils::http::collect_text(response).await;
        let normalized = normalize_provider_error(ProviderErrorParts {
            status: Some(status),
            body: Some(body),
            message: format!("{status} status code"),
        });
        return Err(format_provider_error(&normalized, Some("OpenAI API error")));
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

    producer.push(crate::ai::types::AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let sse = crate::ai::utils::sse::SseStream::with_signal(
        response.body,
        options.and_then(|options| options.base.base.signal.clone()),
    );
    let events = futures::StreamExt::filter_map(sse, |sse_event| async move {
        if sse_event.event.as_deref() == Some("__error__") {
            return Some(Err(sse_event.data));
        }
        serde_json::from_str::<Value>(&sse_event.data).ok().map(Ok)
    });

    let model_owned = model.clone();
    let stream_options = ResponsesStreamOptions {
        service_tier: options.and_then(|options| options.service_tier.clone()),
        grammar_tool_input_properties: Some(grammar_tool_input_properties),
        resolve_service_tier: None,
        apply_service_tier_pricing: Some(Box::new(
            move |usage: &mut Usage, service_tier: Option<&str>| {
                apply_service_tier_pricing(usage, service_tier, &model_owned);
            },
        )),
    };
    process_responses_stream(
        Box::pin(events),
        output,
        producer,
        model,
        Some(&stream_options),
    )
    .await?;

    let signal = options.and_then(|options| options.base.base.signal.clone());
    if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason == crate::ai::types::StopReason::Pending {
        return Err("OpenAI Responses stream ended without a stop reason".to_string());
    }
    if output.stop_reason == crate::ai::types::StopReason::Aborted
        || output.stop_reason == crate::ai::types::StopReason::Error
    {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }

    let reason = match output.stop_reason {
        crate::ai::types::StopReason::Length => crate::ai::types::DoneReason::Length,
        crate::ai::types::StopReason::ToolUse => crate::ai::types::DoneReason::ToolUse,
        crate::ai::types::StopReason::Deferred => crate::ai::types::DoneReason::Deferred,
        _ => crate::ai::types::DoneReason::Stop,
    };
    producer.push(crate::ai::types::AssistantMessageEvent::Done {
        reason,
        message: output.clone(),
    });
    producer.end(None);
    Ok(())
}

/// Port of `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if get_client_api_key(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
    )
    .is_err()
    {
        // TS: the call throws synchronously; surface via the stream error path.
        let stream = create_assistant_message_event_stream();
        let producer = stream.clone();
        let model = model.clone();
        let error = "No API key".to_string();
        tokio::spawn(async move {
            let message = crate::ai::types::AssistantMessage {
                role: crate::ai::types::RoleAssistant,
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: Default::default(),
                stop_reason: crate::ai::types::StopReason::Error,
                error_message: Some(error),
                timestamp: crate::ai::auth::resolve::now_millis(),
                ..Default::default()
            };
            producer.push(crate::ai::types::AssistantMessageEvent::Error {
                reason: crate::ai::types::ErrorReason::Error,
                error: message.clone(),
            });
            producer.end(Some(message));
        });
        return stream;
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

    stream(
        model,
        context,
        Some(&OpenAIResponsesOptions {
            base,
            reasoning_effort,
            ..Default::default()
        }),
    )
}
