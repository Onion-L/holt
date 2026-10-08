//! Port of `pi-core/ai/src/api/azure-openai-responses.ts`.
//!
//! The `AzureOpenAI` SDK client is replaced by a direct request to
//! `{baseUrl}/deployments/{deploymentName}/responses?api-version={version}`
//! (the URL shape the SDK produces), with the api-key header for auth.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::ai::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::ai::api::openai_completions::clamp_openai_prompt_cache_key;
use crate::ai::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ResponsesStreamOptions, convert_responses_messages,
    convert_responses_tools, process_responses_stream,
};
use crate::ai::api::simple_options::build_base_options;
use crate::ai::models::clamp_thinking_level;
use crate::ai::types::{Context, Model, SimpleStreamOptions, StreamOptions, ThinkingLevel};
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

const DEFAULT_AZURE_API_VERSION: &str = "v1";

fn azure_tool_call_providers() -> BTreeSet<String> {
    [
        "openai",
        "openai-codex",
        "opencode",
        "azure-openai-responses",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// OpenAI Responses rejects max_output_tokens below 16.
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;

/// Port of `parseDeploymentNameMap`.
pub fn parse_deployment_name_map(value: Option<&str>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(value) = value else {
        return map;
    };
    for entry in value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        // `split("=", 2)` keeps at most two segments and discards the rest.
        let mut parts = trimmed.split('=');
        let (Some(model_id), Some(deployment_name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if model_id.is_empty() || deployment_name.is_empty() {
            continue;
        }
        map.insert(
            model_id.trim().to_string(),
            deployment_name.trim().to_string(),
        );
    }
    map
}

/// Port of `resolveDeploymentName`.
pub fn resolve_deployment_name(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> String {
    // TS checks truthiness, so an empty-string override falls through.
    if let Some(name) = options
        .and_then(|options| options.azure_deployment_name.as_ref())
        .filter(|name| !name.is_empty())
    {
        return name.clone();
    }
    let env = options.and_then(|options| options.base.base.env.as_ref());
    let map_value = env
        .and_then(|env| get_provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", Some(env)))
        .or_else(|| get_provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", None));
    let mapped = parse_deployment_name_map(map_value.as_deref())
        .get(&model.id)
        .cloned()
        // TS: `mappedDeployment || model.id` treats an empty name as unset.
        .filter(|name| !name.is_empty());
    mapped.unwrap_or_else(|| model.id.clone())
}

/// Port of `AzureOpenAIResponsesOptions`.
#[derive(Clone, Default)]
pub struct AzureOpenAIResponsesOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<ThinkingLevel>,
    pub tool_choice: Option<Value>,
    pub reasoning_summary: Option<String>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

/// Port of `normalizeAzureBaseUrl`.
fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let parsed = url::Url::parse(trimmed)
        .map_err(|_| format!("Invalid Azure OpenAI base URL: {base_url}"))?;

    let host = parsed.host_str().unwrap_or_default();
    let is_azure_host = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let normalized_path = parsed.path().trim_end_matches('/').to_string();

    let mut url = parsed;
    // Ensure Azure hosts have /openai/v1 as base path so deployments and
    // api-version append correctly.
    if is_azure_host
        && matches!(
            normalized_path.as_str(),
            "" | "/" | "/openai" | "/openai/v1/responses"
        )
    {
        url.set_path("/openai/v1");
        url.set_query(None);
    }

    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn build_default_base_url(resource_name: &str) -> String {
    format!("https://{resource_name}.openai.azure.com/openai/v1")
}

/// Port of `resolveAzureConfig`.
fn resolve_azure_config(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> Result<(String, String), String> {
    let env = options.and_then(|options| options.base.base.env.as_ref());
    // TS resolves these with `||`, so empty-string options fall through to the
    // next source exactly like an unset option.
    let api_version = options
        .and_then(|options| options.azure_api_version.clone())
        .filter(|version| !version.is_empty())
        .or_else(|| get_provider_env_value("AZURE_OPENAI_API_VERSION", env))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());

    let base_url = options
        .and_then(|options| options.azure_base_url.clone())
        .filter(|url| !url.trim().is_empty())
        .or_else(|| {
            get_provider_env_value("AZURE_OPENAI_BASE_URL", env)
                .filter(|url| !url.trim().is_empty())
        });
    let resource_name = options
        .and_then(|options| options.azure_resource_name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| get_provider_env_value("AZURE_OPENAI_RESOURCE_NAME", env));

    let resolved = base_url
        .or_else(|| resource_name.as_ref().map(|name| build_default_base_url(name)))
        .or_else(|| (!model.base_url.is_empty()).then(|| model.base_url.clone()))
        .ok_or_else(|| {
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.".to_string()
        })?;

    Ok((normalize_azure_base_url(&resolved)?, api_version))
}

/// Port of `buildParams`.
pub fn build_params(
    model: &Model,
    context: &Context,
    options: Option<&AzureOpenAIResponsesOptions>,
    deployment_name: &str,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let options = options.cloned().unwrap_or_default();
    let messages = convert_responses_messages(
        model,
        context,
        &azure_tool_call_providers(),
        Some(&ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            ..Default::default()
        }),
    )?;

    let mut params = json!({
        "model": deployment_name,
        "input": messages,
        "stream": true,
        "store": false,
    });
    if let Some(key) = clamp_openai_prompt_cache_key(options.base.session_id.as_deref()) {
        params["prompt_cache_key"] = json!(key);
    }

    if let Some(max_tokens) = options.base.max_tokens
        && max_tokens > 0
    {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }

    if let Some(temperature) = options.base.temperature {
        params["temperature"] = json!(temperature);
    }

    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        params["tools"] = Value::Array(convert_responses_tools(
            tools,
            Some(
                &crate::ai::api::openai_responses_shared::ConvertResponsesToolsOptions {
                    strict: None,
                    supports_strict_mode: Some(
                        model
                            .compat
                            .as_ref()
                            .and_then(|compat| compat.supports_strict_mode)
                            .unwrap_or(true),
                    ),
                    supports_openai_grammar_tools: Some(
                        model
                            .compat
                            .as_ref()
                            .and_then(|compat| compat.supports_open_ai_grammar_tools)
                            .unwrap_or(false),
                    ),
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
        } else {
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
    }

    // Last so custom keys override the named request fields.
    if let Some(sampling_params) = &options.base.sampling_params {
        for (key, value) in sampling_params {
            params[key.clone()] = value.clone();
        }
    }

    Ok(params)
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let model = model.clone();
    let context = context.clone();
    let options = options.cloned();
    let producer = stream.clone();
    tokio::spawn(async move {
        let deployment_name = resolve_deployment_name(&model, options.as_ref());

        let mut output = crate::ai::types::AssistantMessage {
            role: crate::ai::types::RoleAssistant,
            content: Vec::new(),
            api: "azure-openai-responses".to_string(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Default::default(),
            stop_reason: crate::ai::types::StopReason::Pending,
            timestamp: crate::ai::auth::resolve::now_millis(),
            ..Default::default()
        };

        let result = run_stream(
            &model,
            &context,
            options.as_ref(),
            &deployment_name,
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
                Some("Azure OpenAI API error"),
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
    options: Option<&AzureOpenAIResponsesOptions>,
    deployment_name: &str,
    output: &mut crate::ai::types::AssistantMessage,
    producer: &AssistantMessageEventStream,
) -> Result<(), String> {
    let Some(api_key) = options.and_then(|options| options.base.base.api_key.clone()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_open_ai_grammar_tools)
            .unwrap_or(false),
    )?;
    let mut params = build_params(
        model,
        context,
        options,
        deployment_name,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_params) = on_payload(params.clone(), model).await
    {
        params = next_params;
    }

    let (base_url, api_version) = resolve_azure_config(model, options)?;
    let mut merged: crate::ai::types::ProviderHeaders = Default::default();
    merged.insert(
        "User-Agent".to_string(),
        Some(crate::ai::session_resources::get_pi_user_agent()),
    );
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            merged.insert(name.clone(), Some(value.clone()));
        }
    }
    if let Some(options_headers) = options.and_then(|options| options.base.base.headers.as_ref()) {
        for (name, value) in options_headers {
            merged.insert(name.clone(), value.clone());
        }
    }

    // The AzureOpenAI SDK deletes a default header named by a null entry —
    // including its own `api-key` auth header — and a non-null entry
    // replaces it, so SDK auth is only sent when the merged headers leave
    // `api-key` unset. The SDK matches header names case-insensitively.
    let mut headers: Vec<(String, String)> = Vec::new();
    if !merged
        .keys()
        .any(|name| name.eq_ignore_ascii_case("api-key"))
    {
        headers.push(("api-key".to_string(), api_key));
    }
    for (name, value) in merged {
        if let Some(value) = value {
            headers.push((name, value));
        }
    }
    headers.push(("content-type".to_string(), "application/json".to_string()));

    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);
    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url: format!(
            "{base_url}/deployments/{deployment_name}/responses?api-version={api_version}"
        ),
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
        return Err(format_provider_error(
            &normalized,
            Some("Azure OpenAI API error"),
        ));
    }

    if let Some(on_response) = options.and_then(|options| options.base.base.on_response.as_ref()) {
        let response_headers: BTreeMap<String, String> = response
            .headers
            .iter()
            .map(|(name, value)| (name.to_lowercase(), value.clone()))
            .collect();
        on_response(
            &crate::ai::types::ProviderResponse {
                status: response.status,
                headers: response_headers,
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

    let stream_options = ResponsesStreamOptions {
        grammar_tool_input_properties: Some(grammar_tool_input_properties),
        ..Default::default()
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
        return Err("Azure OpenAI Responses stream ended without a stop reason".to_string());
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
    let api_key = options.and_then(|options| options.base.base.api_key.clone());
    if api_key.is_none() {
        return error_stream(
            model,
            &format!("No API key for provider: {}", model.provider),
        );
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
    // TS: `toolChoice: options?.toolChoice` is carried into the provider
    // options; the neutral choice value is forwarded verbatim.
    let tool_choice = options
        .tool_choice
        .as_ref()
        .and_then(|choice| serde_json::to_value(choice).ok());

    stream(
        model,
        context,
        Some(&AzureOpenAIResponsesOptions {
            base,
            reasoning_effort,
            tool_choice,
            ..Default::default()
        }),
    )
}

fn error_stream(model: &Model, error: &str) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let producer = stream.clone();
    let model = model.clone();
    let error = error.to_string();
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
    stream
}
