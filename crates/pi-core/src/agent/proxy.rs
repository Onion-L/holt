//! Port of `pi-core/agent/src/proxy.ts`.
//!
//! Stream function for apps that route LLM calls through a server. The
//! server manages auth and proxies requests to LLM providers, sending
//! partial-field-stripped delta events to reduce bandwidth; the client
//! reconstructs the partial message locally.
//!
//! Deviation: TypeScript issues the request through `globalThis.fetch`;
//! the Rust port routes it through the crate `HttpFetch` transport
//! (injectable via `ProxyStreamOptions.fetch`, defaulting to the reqwest
//! implementation). Cancellation is observed between body chunks.

use std::collections::HashMap;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    Model, StopReason, TextContent, ThinkingContent, ToolCall, Usage,
};
use crate::ai::utils::event_stream::AssistantMessageEventStream;
use crate::ai::utils::http::{HttpBody, HttpFetchError, HttpMethod, HttpRequest};
use crate::ai::utils::json_parse::parse_streaming_json;

use super::types::{ProxySerializableStreamOptions, now_millis};

/// Port of `ProxyAssistantMessageEvent`: the event types the server sends
/// with the partial field stripped.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyAssistantMessageEvent {
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "contentSignature", skip_serializing_if = "Option::is_none")]
        content_signature: Option<String>,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "contentSignature", skip_serializing_if = "Option::is_none")]
        content_signature: Option<String>,
    },
    #[serde(rename = "toolcall_start")]
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    #[serde(rename = "toolcall_delta")]
    ToolcallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
    ToolcallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
    },
    #[serde(rename = "done")]
    Done { reason: DoneReason, usage: Usage },
    #[serde(rename = "error")]
    Error {
        reason: ErrorReason,
        #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        usage: Usage,
    },
}

/// Port of `ProxyStreamOptions`.
#[derive(Clone, Default)]
pub struct ProxyStreamOptions {
    /// Serializable options forwarded with the request
    /// (`ProxySerializableStreamOptions`).
    pub base: ProxySerializableStreamOptions,
    /// Local abort signal for the proxy request.
    pub signal: Option<CancellationToken>,
    /// Auth token for the proxy server.
    pub auth_token: String,
    /// Proxy server URL (e.g. `"https://genai.example.com"`).
    pub proxy_url: String,
    /// Injectable HTTP transport; defaults to the reqwest-backed fetch.
    pub fetch: Option<crate::ai::types::FetchFunction>,
}

fn status_text(status: u16) -> String {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| {
            status
                .canonical_reason()
                .map(std::borrow::ToOwned::to_owned)
        })
        .unwrap_or_default()
}

/// Serializable request options (port of `buildProxyRequestOptions`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyRequestBodyOptions<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sampling_params: Option<&'a std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<crate::ai::types::ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_retention: Option<crate::ai::types::CacheRetention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<&'a std::collections::BTreeMap<String, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<crate::ai::types::Transport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budgets: Option<&'a crate::ai::types::ThinkingBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retry_delay_ms: Option<u64>,
}

/// Port of `streamProxy`: a stream function that proxies through a server
/// instead of calling LLM providers directly.
pub fn stream_proxy(
    model: &Model,
    context: &Context,
    options: ProxyStreamOptions,
) -> AssistantMessageEventStream {
    let stream = crate::ai::utils::event_stream::create_assistant_message_event_stream();
    let run_stream = stream.clone();

    let model = model.clone();
    let context = context.clone();
    tokio::spawn(async move {
        // Initialize the partial message that we'll build up from events.
        let mut partial = AssistantMessage {
            stop_reason: StopReason::Pending,
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Usage::default(),
            timestamp: now_millis(),
            ..Default::default()
        };
        let mut partial_tool_json: HashMap<usize, String> = HashMap::new();

        let result = run_proxy_request(
            &model,
            &context,
            &options,
            &mut partial,
            &mut partial_tool_json,
            &run_stream,
        )
        .await;

        match result {
            Ok(()) => {
                run_stream.end(None);
            }
            Err(error_message) => {
                let reason = if options
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_cancelled())
                {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                partial.stop_reason = reason.into();
                partial.error_message = Some(error_message);
                run_stream.push(AssistantMessageEvent::Error {
                    reason,
                    error: partial,
                });
                run_stream.end(None);
            }
        }
    });

    stream
}

async fn run_proxy_request(
    model: &Model,
    context: &Context,
    options: &ProxyStreamOptions,
    partial: &mut AssistantMessage,
    partial_tool_json: &mut HashMap<usize, String>,
    stream: &AssistantMessageEventStream,
) -> Result<(), String> {
    let fetch = options.fetch.clone().unwrap_or_else(default_proxy_fetch);
    let request_options = ProxyRequestBodyOptions {
        temperature: options.base.temperature,
        sampling_params: options.base.sampling_params.as_ref(),
        max_tokens: options.base.max_tokens,
        reasoning: options.base.reasoning,
        cache_retention: options.base.cache_retention,
        session_id: options.base.session_id.as_deref(),
        headers: options.base.headers.as_ref(),
        metadata: options.base.metadata.as_ref(),
        transport: options.base.transport,
        thinking_budgets: options.base.thinking_budgets.as_ref(),
        max_retry_delay_ms: options.base.max_retry_delay_ms,
    };
    let body = serde_json::json!({
        "model": model,
        "context": context,
        "options": request_options,
    });
    let request = HttpRequest {
        signal: options.signal.clone(),
        method: HttpMethod::Post,
        url: format!("{}/api/stream", options.proxy_url),
        headers: vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", options.auth_token),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        body: HttpBody::Json(body),
    };

    let response = fetch.fetch(request).await.map_err(|error| match error {
        HttpFetchError::Cancelled => "The operation was aborted".to_string(),
        other => other.to_string(),
    })?;

    if !(200..300).contains(&response.status) {
        let mut error_message = format!(
            "Proxy error: {} {}",
            response.status,
            status_text(response.status)
        );
        if let Ok(error_data) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(
            &response.bytes().await.unwrap_or_default(),
        )) && let Some(error) = error_data.get("error").and_then(|value| value.as_str())
        {
            error_message = format!("Proxy error: {error}");
        }
        return Err(error_message);
    }

    let mut response = response;
    let mut buffer = String::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return Err("Request aborted by user".to_string());
        }

        buffer.push_str(&String::from_utf8_lossy(&chunk));
        let mut lines: Vec<String> = buffer.split('\n').map(str::to_string).collect();
        buffer = lines.pop().unwrap_or_default();

        for line in lines {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            let proxy_event: ProxyAssistantMessageEvent =
                serde_json::from_str(data).map_err(|error| error.to_string())?;
            if let Some(event) = process_proxy_event(partial, partial_tool_json, proxy_event)? {
                stream.push(event);
            }
        }
    }

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_cancelled())
    {
        return Err("Request aborted by user".to_string());
    }

    Ok(())
}

fn default_proxy_fetch() -> crate::ai::types::FetchFunction {
    crate::ai::utils::reqwest_fetch::default_fetch()
}

/// Places a content block at `index`, padding with empty text blocks for
/// the sparse-array assignments the TypeScript implementation makes.
fn set_content(partial: &mut AssistantMessage, index: usize, block: AssistantContent) {
    if index >= partial.content.len() {
        partial
            .content
            .resize_with(index + 1, || AssistantContent::Text(TextContent::default()));
    }
    partial.content[index] = block;
}

/// Port of `processProxyEvent`: applies a proxy event to the partial
/// message and produces the full assistant message event.
fn process_proxy_event(
    partial: &mut AssistantMessage,
    partial_tool_json: &mut HashMap<usize, String>,
    proxy_event: ProxyAssistantMessageEvent,
) -> Result<Option<AssistantMessageEvent>, String> {
    Ok(match proxy_event {
        ProxyAssistantMessageEvent::Start => Some(AssistantMessageEvent::Start {
            partial: partial.clone(),
        }),

        ProxyAssistantMessageEvent::TextStart { content_index } => {
            set_content(
                partial,
                content_index,
                AssistantContent::Text(TextContent::default()),
            );
            Some(AssistantMessageEvent::TextStart {
                content_index,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::TextDelta {
            content_index,
            delta,
        } => {
            if let Some(AssistantContent::Text(content)) = partial.content.get_mut(content_index) {
                content.text.push_str(&delta);
                Some(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                    partial: partial.clone(),
                })
            } else {
                return Err("Received text_delta for non-text content".to_string());
            }
        }

        ProxyAssistantMessageEvent::TextEnd {
            content_index,
            content_signature,
        } => {
            if let Some(AssistantContent::Text(content)) = partial.content.get_mut(content_index) {
                content.text_signature = content_signature;
                let text = content.text.clone();
                Some(AssistantMessageEvent::TextEnd {
                    content_index,
                    content: text,
                    partial: partial.clone(),
                })
            } else {
                return Err("Received text_end for non-text content".to_string());
            }
        }

        ProxyAssistantMessageEvent::ThinkingStart { content_index } => {
            set_content(
                partial,
                content_index,
                AssistantContent::Thinking(ThinkingContent::default()),
            );
            Some(AssistantMessageEvent::ThinkingStart {
                content_index,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
        } => {
            if let Some(AssistantContent::Thinking(content)) =
                partial.content.get_mut(content_index)
            {
                content.thinking.push_str(&delta);
                Some(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: partial.clone(),
                })
            } else {
                return Err("Received thinking_delta for non-thinking content".to_string());
            }
        }

        ProxyAssistantMessageEvent::ThinkingEnd {
            content_index,
            content_signature,
        } => {
            if let Some(AssistantContent::Thinking(content)) =
                partial.content.get_mut(content_index)
            {
                content.thinking_signature = content_signature;
                let thinking = content.thinking.clone();
                Some(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: thinking,
                    partial: partial.clone(),
                })
            } else {
                return Err("Received thinking_end for non-thinking content".to_string());
            }
        }

        ProxyAssistantMessageEvent::ToolcallStart {
            content_index,
            id,
            tool_name,
        } => {
            partial_tool_json.insert(content_index, String::new());
            set_content(
                partial,
                content_index,
                AssistantContent::ToolCall(ToolCall {
                    id,
                    name: tool_name,
                    arguments: Default::default(),
                    ..Default::default()
                }),
            );
            Some(AssistantMessageEvent::ToolcallStart {
                content_index,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::ToolcallDelta {
            content_index,
            delta,
        } => {
            if let Some(AssistantContent::ToolCall(content)) =
                partial.content.get_mut(content_index)
            {
                let partial_json = partial_tool_json.entry(content_index).or_default();
                partial_json.push_str(&delta);
                content.arguments = match parse_streaming_json(Some(partial_json)) {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                Some(AssistantMessageEvent::ToolcallDelta {
                    content_index,
                    delta,
                    partial: partial.clone(),
                })
            } else {
                return Err("Received toolcall_delta for non-toolCall content".to_string());
            }
        }

        ProxyAssistantMessageEvent::ToolcallEnd {
            content_index,
            tool_call,
        } => {
            // TypeScript merges the finalized tool call over the partial
            // block and removes the ad-hoc partialJson field.
            partial_tool_json.remove(&content_index);
            if let Some(AssistantContent::ToolCall(content)) =
                partial.content.get_mut(content_index)
            {
                content.id = tool_call.id;
                content.name = tool_call.name;
                content.arguments = tool_call.arguments;
                if tool_call.thought_signature.is_some() {
                    content.thought_signature = tool_call.thought_signature;
                }
                if tool_call.namespace.is_some() {
                    content.namespace = tool_call.namespace.clone();
                }
                let merged = content.clone();
                Some(AssistantMessageEvent::ToolcallEnd {
                    content_index,
                    tool_call: merged,
                    partial: partial.clone(),
                })
            } else {
                None
            }
        }

        ProxyAssistantMessageEvent::Done { reason, usage } => {
            partial.stop_reason = reason.into();
            partial.usage = usage;
            Some(AssistantMessageEvent::Done {
                reason,
                message: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::Error {
            reason,
            error_message,
            usage,
        } => {
            partial.stop_reason = reason.into();
            partial.error_message = error_message;
            partial.usage = usage;
            Some(AssistantMessageEvent::Error {
                reason,
                error: partial.clone(),
            })
        }
    })
}
