//! Port of `pi-core/ai/src/api/google-vertex.ts`.
//!
//! The Vertex `GoogleGenAI` client becomes a direct POST to the Vertex
//! `streamGenerateContent` endpoint: with an API key,
//! `https://{location}-aiplatform.googleapis.com/{version}/publishers/google/models/{model}:streamGenerateContent`
//! (custom collection-scoped base URLs override the host); with ADC the SDK
//! handles auth in TypeScript — the Rust port issues the same request shape
//! and requires the caller to supply credentials via the injectable
//! transport or an express API key.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use crate::ai::api::google_generative_ai::GoogleThinkingConfig;
use crate::ai::api::google_shared::{
    convert_messages, convert_tools, is_thinking_part, map_stop_reason_string,
    resolve_google_function_calling_mode, resolve_google_thinking_level, retain_thought_signature,
    supports_google_strict_tool_sampling,
};
use crate::ai::api::simple_options::build_base_options;
use crate::ai::models::{calculate_cost, clamp_thinking_level};
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    Model, ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ThinkingBudgets, ThinkingContent, ThinkingLevel, ToolCall,
};
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
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;

/// Port of `GoogleVertexOptions`.
#[derive(Clone, Default)]
pub struct GoogleVertexOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<String>,
    pub thinking: Option<GoogleThinkingConfig>,
    pub project: Option<String>,
    pub location: Option<String>,
}

const API_VERSION: &str = "v1";
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";

/// Counter for generating unique tool call IDs.
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Port of `resolveApiKey`: ignores placeholder/marker keys.
pub fn resolve_api_key(options: Option<&GoogleVertexOptions>) -> Option<String> {
    let api_key = options
        .and_then(|options| options.base.base.api_key.as_deref())
        .map(str::trim)?;
    if api_key.is_empty()
        || api_key == GCP_VERTEX_CREDENTIALS_MARKER
        || is_placeholder_api_key(api_key)
    {
        return None;
    }
    Some(api_key.to_string())
}

fn is_placeholder_api_key(api_key: &str) -> bool {
    // Port of /^<[^>]+>$/: angle brackets around one or more non-'>' chars.
    api_key
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .is_some_and(|inner| !inner.is_empty() && !inner.contains('>'))
}

/// Port of `resolveProject`.
fn resolve_project(options: Option<&GoogleVertexOptions>) -> Result<String, String> {
    let env = options.and_then(|options| options.base.base.env.as_ref());
    let project = options
        .and_then(|options| options.project.clone())
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_PROJECT", env))
        .or_else(|| get_provider_env_value("GCLOUD_PROJECT", env));
    project.ok_or_else(|| {
        "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
            .to_string()
    })
}

/// Port of `resolveLocation`.
fn resolve_location(options: Option<&GoogleVertexOptions>) -> Result<String, String> {
    let env = options.and_then(|options| options.base.base.env.as_ref());
    options
        .and_then(|options| options.location.clone())
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_LOCATION", env))
        .ok_or_else(|| {
            "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
                .to_string()
        })
}

/// Port of `resolveCustomBaseUrl`.
fn resolve_custom_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() || trimmed.contains("{location}") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Port of `baseUrlIncludesApiVersion`: any path segment matching
/// `^v\d+(?:beta\d*)?$`.
pub fn base_url_includes_api_version(base_url: &str) -> bool {
    if let Ok(url) = url::Url::parse(base_url) {
        return url
            .path_segments()
            .is_some_and(|mut segments| segments.any(is_version_segment));
    }
    base_url.split('/').any(is_version_segment)
}

fn is_version_segment(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix('v') else {
        return false;
    };
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    let remainder = &rest[digits.len()..];
    remainder.is_empty()
        || (remainder.starts_with("beta") && remainder[4..].chars().all(|ch| ch.is_ascii_digit()))
}

/// Port of the model matchers (shared with generative-ai but defined there
/// privately; duplicate the two-character logic).
fn is_gemini3_pro_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    if !id.starts_with("gemini-3") {
        return false;
    }
    let rest = &id["gemini-3".len()..];
    let rest = if let Some(stripped) = rest.strip_prefix('.') {
        let digits: usize = stripped
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .count();
        if digits == 0 {
            return false;
        }
        &stripped[digits..]
    } else {
        rest
    };
    rest.starts_with("-pro")
}

fn is_gemini3_flash_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    if id == "gemini-flash-latest" || id == "gemini-flash-lite-latest" {
        return true;
    }
    if !id.starts_with("gemini-3") {
        return false;
    }
    let rest = &id["gemini-3".len()..];
    let rest = if let Some(stripped) = rest.strip_prefix('.') {
        let digits: usize = stripped
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .count();
        if digits == 0 {
            return false;
        }
        &stripped[digits..]
    } else {
        rest
    };
    rest.starts_with("-flash")
}

/// Port of `getDisabledThinkingConfig` (Vertex flavor: no Gemma branch).
fn get_disabled_thinking_config(model: &Model) -> Value {
    if is_gemini3_pro_model(model) {
        return json!({ "thinkingLevel": "LOW" });
    }
    if is_gemini3_flash_model(model) {
        return json!({ "thinkingLevel": "MINIMAL" });
    }
    json!({ "thinkingBudget": 0 })
}

/// Port of `getGemini3ThinkingLevel`.
fn get_gemini3_thinking_level(effort: ThinkingLevel, model: &Model) -> &'static str {
    if is_gemini3_pro_model(model) {
        return match effort {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "LOW",
            ThinkingLevel::Medium | ThinkingLevel::High => "HIGH",
            ThinkingLevel::Xhigh | ThinkingLevel::Max => "HIGH",
        };
    }
    match effort {
        ThinkingLevel::Minimal => "MINIMAL",
        ThinkingLevel::Low => "LOW",
        ThinkingLevel::Medium => "MEDIUM",
        ThinkingLevel::High => "HIGH",
        ThinkingLevel::Xhigh | ThinkingLevel::Max => "HIGH",
    }
}

/// Port of `getGoogleBudget` (Vertex flavor: no 2.5-flash-lite tier).
fn get_google_budget(
    model: &Model,
    level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> i64 {
    if let Some(custom) = custom_budgets {
        let custom_value = match level {
            ThinkingLevel::Minimal => custom.minimal,
            ThinkingLevel::Low => custom.low,
            ThinkingLevel::Medium => custom.medium,
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => custom.high,
        };
        if let Some(value) = custom_value {
            return value as i64;
        }
    }

    if model.id.contains("2.5-pro") {
        return match level {
            ThinkingLevel::Minimal => 128,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            _ => 32768,
        };
    }

    if model.id.contains("2.5-flash") {
        return match level {
            ThinkingLevel::Minimal => 128,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            _ => 24576,
        };
    }

    -1
}

/// Port of `buildParams` (same shape as generative-ai with the Vertex
/// THINKING_LEVEL_MAP pass-through).
pub fn build_params(
    model: &Model,
    context: &Context,
    options: Option<&GoogleVertexOptions>,
) -> Result<Value, String> {
    let options = options.cloned().unwrap_or_default();
    let contents = convert_messages(model, context);

    let mut generation_config = Map::new();
    if let Some(temperature) = options.base.temperature {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(max_tokens) = options.base.max_tokens {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }

    let supports_strict_mode = supports_google_strict_tool_sampling(&model.id);
    let function_calling_mode = if context
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
    {
        resolve_google_function_calling_mode(
            context.tools.as_deref().unwrap_or_default(),
            options.tool_choice.as_deref(),
            supports_strict_mode,
        )?
    } else {
        None
    };

    let mut config = Map::new();
    for (key, value) in generation_config {
        config.insert(key, value);
    }
    if let Some(system_prompt) = &context.system_prompt {
        config.insert(
            "systemInstruction".to_string(),
            json!(sanitize_surrogates(system_prompt)),
        );
    }
    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        let converted = convert_tools(tools, false, supports_strict_mode)?;
        if let Some(converted) = converted {
            config.insert("tools".to_string(), json!(converted));
        }
    }
    if let Some(mode) = function_calling_mode {
        config.insert(
            "toolConfig".to_string(),
            json!({ "functionCallingConfig": { "mode": mode.as_str() } }),
        );
    }

    let thinking_enabled = options
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.enabled);
    if thinking_enabled && model.reasoning {
        let mut thinking_config = json!({ "includeThoughts": true });
        if let Some(level) = options
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.level.as_deref())
        {
            // THINKING_LEVEL_MAP: the Google level strings pass through.
            thinking_config["thinkingLevel"] = json!(level);
        } else if let Some(budget) = options
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget_tokens)
        {
            thinking_config["thinkingBudget"] = json!(budget);
        }
        config.insert("thinkingConfig".to_string(), thinking_config);
    } else if model.reasoning
        && options
            .thinking
            .as_ref()
            .is_some_and(|thinking| !thinking.enabled)
    {
        let disabled = get_disabled_thinking_config(model);
        config.insert("thinkingConfig".to_string(), disabled);
    }

    Ok(json!({
        "model": model.id,
        "contents": contents,
        "config": Value::Object(config),
    }))
}

/// Builds the Vertex streaming request URL and headers.
#[allow(clippy::too_many_arguments)]
fn build_request(
    model: &Model,
    api_key: Option<&str>,
    _project: Option<&str>,
    location: Option<&str>,
    options_headers: Option<&ProviderHeaders>,
    params: Value,
) -> Result<HttpRequest, String> {
    // The SDK merges the default User-Agent with model/request headers into a
    // single record (`providerHeadersToRecord`); later sources override and
    // null-valued request headers suppress a default entirely.
    let mut header_map: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    header_map.insert(
        "User-Agent".to_string(),
        crate::ai::session_resources::get_pi_user_agent(),
    );
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            header_map.insert(name.clone(), value.clone());
        }
    }
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            match value {
                Some(value) => {
                    header_map.insert(name.clone(), value.clone());
                }
                None => {
                    header_map.remove(name);
                }
            }
        }
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(api_key) = api_key {
        headers.push(("x-goog-api-key".to_string(), api_key.to_string()));
    }
    headers.extend(header_map);

    let custom_base = resolve_custom_base_url(&model.base_url);
    let version_included = custom_base
        .as_ref()
        .is_some_and(|base| base_url_includes_api_version(base));
    let api_version = if version_included { "" } else { API_VERSION };

    let (url, effective_location) = match &custom_base {
        Some(base) => {
            // COLLECTION resource scope: the base URL replaces the
            // https://{location}-aiplatform.googleapis.com host. The apiVersion
            // is appended unless the base URL already includes one.
            let trimmed = base.trim_end_matches('/');
            let version = if api_version.is_empty() {
                String::new()
            } else {
                format!("{api_version}/")
            };
            (
                format!(
                    "{trimmed}/{version}publishers/google/models/{}:streamGenerateContent?alt=sse",
                    model.id
                ),
                None::<String>,
            )
        }
        None => {
            let version = if api_version.is_empty() {
                String::new()
            } else {
                format!("{api_version}/")
            };
            match api_key {
                // Vertex Express: API-key clients use the global endpoint,
                // not a location-prefixed regional one.
                Some(_) => (
                    format!(
                        "https://aiplatform.googleapis.com/{version}publishers/google/models/{}:streamGenerateContent?alt=sse",
                        model.id
                    ),
                    None::<String>,
                ),
                None => {
                    let location = location.ok_or_else(|| {
                        "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options.".to_string()
                    })?;
                    (
                        format!(
                            "https://{location}-aiplatform.googleapis.com/{version}publishers/google/models/{}:streamGenerateContent?alt=sse",
                            model.id
                        ),
                        Some(location.to_string()),
                    )
                }
            }
        }
    };
    let _ = effective_location;

    Ok(HttpRequest {
        signal: None,
        method: HttpMethod::Post,
        url,
        headers,
        body: HttpBody::Json(params),
    })
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&GoogleVertexOptions>,
) -> AssistantMessageEventStream {
    // The TypeScript adapter rejects any fetch that is not globalThis.fetch;
    // in the Rust port the `fetch` option only ever carries a custom
    // transport, so its presence is exactly that rejection case (passing the
    // ambient fetch explicitly has no Rust counterpart).
    if options
        .and_then(|options| options.base.base.fetch.as_ref())
        .is_some()
    {
        return crate::ai::api::openai_completions::error_stream(
            model,
            "Custom fetch is not supported by the Google Vertex adapter",
        );
    }
    stream_with_transport(model, context, options, None)
}

/// The adapter body with an explicitly provided transport: the test seam
/// standing in for the mocked `@google/genai` SDK (the TypeScript tests mock
/// the SDK module instead of passing a custom fetch).
pub fn stream_with_transport(
    model: &Model,
    context: &Context,
    options: Option<&GoogleVertexOptions>,
    transport: Option<std::sync::Arc<dyn crate::ai::utils::http::HttpFetch>>,
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
            api: "google-vertex".to_string(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Default::default(),
            stop_reason: StopReason::Pending,
            timestamp: crate::ai::auth::resolve::now_millis(),
            ..Default::default()
        };

        let result = run_stream(
            &model,
            &context,
            options.as_ref(),
            transport,
            &mut output,
            &producer,
        )
        .await;
        if let Err(error) = result {
            let aborted = options
                .as_ref()
                .and_then(|options| options.base.base.signal.as_ref())
                .is_some_and(|token| token.is_cancelled());
            output.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            let formatted = format_provider_error(
                &normalize_provider_error(ProviderErrorParts {
                    status: None,
                    body: None,
                    message: error,
                }),
                None,
            );
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

enum VertexBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
}

#[allow(clippy::too_many_lines)]
async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&GoogleVertexOptions>,
    transport: Option<std::sync::Arc<dyn crate::ai::utils::http::HttpFetch>>,
    output: &mut AssistantMessage,
    producer: &AssistantMessageEventStream,
) -> Result<(), String> {
    let api_key = resolve_api_key(options);
    let project = resolve_project(options).ok();
    let location = resolve_location(options).ok();

    let mut params = build_params(model, context, options)?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_params) = on_payload(params.clone(), model).await
    {
        params = next_params;
    }

    // ADC-based requests (no express key) require project + location, and the
    // OAuth bearer token arrives through the injectable transport's ambient
    // credential handling.
    if api_key.is_none() {
        resolve_project(options)?;
        resolve_location(options)?;
    }

    let mut request = build_request(
        model,
        api_key.as_deref(),
        project.as_deref(),
        location.as_deref(),
        options.and_then(|options| options.base.base.headers.as_ref()),
        params,
    )?;
    request.signal = options.and_then(|options| options.base.base.signal.clone());

    let fetch = transport
        .or_else(|| options.and_then(|options| options.base.base.fetch.clone()))
        .unwrap_or_else(default_fetch);
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
        return Err(format_provider_error(&normalized, None));
    }

    producer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let mut current_block: Option<VertexBlock> = None;
    let block_index = |output: &AssistantMessage| output.content.len().saturating_sub(1);

    let mut sse = crate::ai::utils::sse::SseStream::with_signal(
        response.body,
        options.and_then(|options| options.base.base.signal.clone()),
    );
    while let Some(sse_event) = futures::StreamExt::next(&mut sse).await {
        if sse_event.event.as_deref() == Some("__error__") {
            return Err(sse_event.data);
        }
        let chunk: Value = match serde_json::from_str(&sse_event.data) {
            Ok(chunk) => chunk,
            Err(_) => continue,
        };

        if output.response_id.is_none()
            && let Some(response_id) = chunk.get("responseId").and_then(Value::as_str)
            && !response_id.is_empty()
        {
            output.response_id = Some(response_id.to_string());
        }
        let candidate = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|list| list.first());
        if let Some(parts) = candidate
            .and_then(|candidate| candidate.pointer("/content/parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    let is_thinking = is_thinking_part(part);
                    let switch = match &current_block {
                        None => true,
                        Some(VertexBlock::Thinking(_)) if !is_thinking => true,
                        Some(VertexBlock::Text(_)) if is_thinking => true,
                        _ => false,
                    };
                    if switch {
                        if let Some(current) = current_block.take() {
                            match current {
                                VertexBlock::Text(block) => {
                                    producer.push(AssistantMessageEvent::TextEnd {
                                        content_index: block_index(output),
                                        content: block.text,
                                        partial: output.clone(),
                                    });
                                }
                                VertexBlock::Thinking(block) => {
                                    producer.push(AssistantMessageEvent::ThinkingEnd {
                                        content_index: block_index(output),
                                        content: block.thinking,
                                        partial: output.clone(),
                                    });
                                }
                            }
                        }
                        if is_thinking {
                            output
                                .content
                                .push(AssistantContent::Thinking(ThinkingContent::default()));
                            producer.push(AssistantMessageEvent::ThinkingStart {
                                content_index: block_index(output),
                                partial: output.clone(),
                            });
                            current_block = Some(VertexBlock::Thinking(ThinkingContent::default()));
                        } else {
                            output
                                .content
                                .push(AssistantContent::Text(TextContent::default()));
                            producer.push(AssistantMessageEvent::TextStart {
                                content_index: block_index(output),
                                partial: output.clone(),
                            });
                            current_block = Some(VertexBlock::Text(TextContent::default()));
                        }
                    }
                    let incoming_signature = part.get("thoughtSignature").and_then(Value::as_str);
                    match &mut current_block {
                        Some(VertexBlock::Thinking(block)) => {
                            block.thinking.push_str(text);
                            block.thinking_signature = retain_thought_signature(
                                block.thinking_signature.as_deref(),
                                incoming_signature,
                            );
                            producer.push(AssistantMessageEvent::ThinkingDelta {
                                content_index: block_index(output),
                                delta: text.to_string(),
                                partial: output.clone(),
                            });
                        }
                        Some(VertexBlock::Text(block)) => {
                            block.text.push_str(text);
                            block.text_signature = retain_thought_signature(
                                block.text_signature.as_deref(),
                                incoming_signature,
                            );
                            producer.push(AssistantMessageEvent::TextDelta {
                                content_index: block_index(output),
                                delta: text.to_string(),
                                partial: output.clone(),
                            });
                        }
                        None => {}
                    }
                }

                if let Some(function_call) = part.get("functionCall") {
                    if let Some(current) = current_block.take() {
                        match current {
                            VertexBlock::Text(block) => {
                                producer.push(AssistantMessageEvent::TextEnd {
                                    content_index: block_index(output),
                                    content: block.text,
                                    partial: output.clone(),
                                });
                            }
                            VertexBlock::Thinking(block) => {
                                producer.push(AssistantMessageEvent::ThinkingEnd {
                                    content_index: block_index(output),
                                    content: block.thinking,
                                    partial: output.clone(),
                                });
                            }
                        }
                    }

                    let provided_id = function_call.get("id").and_then(Value::as_str);
                    let is_duplicate = provided_id
                        .map(|id| {
                            output.content.iter().any(|block| match block {
                                AssistantContent::ToolCall(tool_call) => tool_call.id == id,
                                _ => false,
                            })
                        })
                        .unwrap_or(true);
                    let tool_call_id = match (provided_id, is_duplicate) {
                        (Some(id), false) => id.to_string(),
                        _ => {
                            let name = function_call
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let counter = TOOL_CALL_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                            format!(
                                "{name}_{}_{counter}",
                                crate::ai::auth::resolve::now_millis()
                            )
                        }
                    };

                    let mut tool_call = ToolCall {
                        content_type: Default::default(),
                        id: tool_call_id,
                        name: function_call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments: function_call
                            .get("args")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default(),
                        ..Default::default()
                    };
                    if let Some(thought_signature) =
                        part.get("thoughtSignature").and_then(Value::as_str)
                    {
                        tool_call.thought_signature = Some(thought_signature.to_string());
                    }

                    producer.push(AssistantMessageEvent::ToolcallStart {
                        content_index: block_index(output),
                        partial: output.clone(),
                    });
                    producer.push(AssistantMessageEvent::ToolcallDelta {
                        content_index: block_index(output),
                        delta: serde_json::to_string(&tool_call.arguments).unwrap_or_default(),
                        partial: output.clone(),
                    });
                    output
                        .content
                        .push(AssistantContent::ToolCall(tool_call.clone()));
                    producer.push(AssistantMessageEvent::ToolcallEnd {
                        content_index: block_index(output),
                        tool_call,
                        partial: output.clone(),
                    });
                }
            }
        }

        if let Some(finish_reason) = candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
        {
            output.raw_stop_reason = Some(finish_reason.to_string());
            output.stop_reason = map_stop_reason_string(finish_reason);
            if output
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
                && output.stop_reason == StopReason::Stop
            {
                output.stop_reason = StopReason::ToolUse;
            }
        }

        if let Some(usage_metadata) = chunk.get("usageMetadata") {
            let get_u64 = |key: &str| usage_metadata.get(key).and_then(Value::as_u64).unwrap_or(0);
            let cached = get_u64("cachedContentTokenCount");
            let thoughts = get_u64("thoughtsTokenCount");
            output.usage = crate::ai::types::Usage {
                input: get_u64("promptTokenCount").saturating_sub(cached),
                output: get_u64("candidatesTokenCount") + thoughts,
                cache_read: cached,
                cache_write: 0,
                reasoning: Some(thoughts),
                total_tokens: get_u64("totalTokenCount"),
                cost: crate::ai::types::UsageCost::default(),
                ..Default::default()
            };
            calculate_cost(model, &mut output.usage);
        }
    }

    if let Some(current) = current_block {
        match current {
            VertexBlock::Text(block) => {
                producer.push(AssistantMessageEvent::TextEnd {
                    content_index: block_index(output),
                    content: block.text,
                    partial: output.clone(),
                });
            }
            VertexBlock::Thinking(block) => {
                producer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: block_index(output),
                    content: block.thinking,
                    partial: output.clone(),
                });
            }
        }
    }

    let signal = options.and_then(|options| options.base.base.signal.clone());
    if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason == StopReason::Pending {
        return Err("Google Vertex stream ended without a finish reason".to_string());
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        let error_message = match &output.raw_stop_reason {
            Some(raw) => format!("Provider stopped with: {raw}"),
            None => "An unknown error occurred".to_string(),
        };
        return Err(error_message);
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

/// Port of `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    stream_simple_with_transport(model, context, options, None)
}

/// [`stream_simple`] with an explicit transport (the SDK-mock test seam).
pub fn stream_simple_with_transport(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    transport: Option<std::sync::Arc<dyn crate::ai::utils::http::HttpFetch>>,
) -> AssistantMessageEventStream {
    let options = options.cloned().unwrap_or_default();
    let base = build_base_options(model, context, Some(&options), None);
    let Some(reasoning) = options.reasoning else {
        return stream_with_transport(
            model,
            context,
            Some(&GoogleVertexOptions {
                base,
                thinking: Some(GoogleThinkingConfig {
                    enabled: false,
                    budget_tokens: None,
                    level: None,
                }),
                ..Default::default()
            }),
            transport,
        );
    };

    let clamped_reasoning =
        clamp_thinking_level(model, crate::ai::types::ModelThinkingLevel::from(reasoning));
    let resolved_level = match resolve_google_thinking_level(model, clamped_reasoning) {
        Ok(level) => level,
        Err(error) => return crate::ai::api::openai_completions::error_stream(model, &error),
    };

    if is_gemini3_pro_model(model) || is_gemini3_flash_model(model) {
        return stream_with_transport(
            model,
            context,
            Some(&GoogleVertexOptions {
                base,
                thinking: Some(GoogleThinkingConfig {
                    enabled: true,
                    budget_tokens: None,
                    level: Some(get_gemini3_thinking_level(resolved_level, model).to_string()),
                }),
                ..Default::default()
            }),
            transport,
        );
    }

    stream_with_transport(
        model,
        context,
        Some(&GoogleVertexOptions {
            base,
            thinking: Some(GoogleThinkingConfig {
                enabled: true,
                budget_tokens: Some(get_google_budget(
                    model,
                    resolved_level,
                    options.thinking_budgets.as_ref(),
                )),
                level: None,
            }),
            ..Default::default()
        }),
        transport,
    )
}
