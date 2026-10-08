//! Port of `pi-core/ai/src/api/pi-messages.ts`.
//!
//! Streams pi's own message protocol directly: a single POST of
//! `{ model, context, options }` to `<baseUrl>/messages`, the response an
//! SSE stream of serialized assistant-message events plus a terminal
//! `done`/`error` event (the Radius gateway wire protocol).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    Model, SimpleStreamOptions, StopReason, StreamOptions, ThinkingLevel, ToolCall, Usage,
};
use crate::ai::utils::diagnostics::append_assistant_message_diagnostic;
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::json_parse::parse_streaming_json;
use crate::ai::utils::provider_env::get_provider_env_value;
use crate::ai::utils::reqwest_fetch::default_fetch;

/// Port of `PiMessagesOptions`.
#[derive(Clone, Default)]
pub struct PiMessagesOptions {
    pub base: StreamOptions,
    pub reasoning: Option<ThinkingLevel>,
    /// Tool choice as raw JSON (string or function object).
    pub tool_choice: Option<Value>,
    /// Ask the backend for debug metadata.
    pub debug: Option<bool>,
}

/// Port of `PiMessagesRewriteImpact`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiMessagesRewriteImpact {
    pub policy_id: String,
    pub policy_version: i64,
    pub changed: bool,
    pub token_count_change: i64,
    pub message_count_change: i64,
    pub system_prompt_changed: bool,
}

/// Terminal reason accepted by a successful pi-messages event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PiMessagesDoneReason {
    Stop,
    Length,
    ToolUse,
}

/// Terminal reason accepted by an error pi-messages event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PiMessagesErrorReason {
    Aborted,
    Error,
}

/// Serialized assistant-message event sent by a pi-messages backend.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PiMessagesEvent {
    Start,
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(
            rename = "contentSignature",
            skip_serializing_if = "Option::is_none",
            default
        )]
        content_signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(
            rename = "contentSignature",
            skip_serializing_if = "Option::is_none",
            default
        )]
        content_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        redacted: Option<bool>,
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
    Done {
        reason: PiMessagesDoneReason,
        usage: Usage,
        #[serde(
            rename = "responseId",
            skip_serializing_if = "Option::is_none",
            default
        )]
        response_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rewrite: Option<PiMessagesRewriteImpact>,
    },
    Error {
        reason: PiMessagesErrorReason,
        usage: Usage,
        #[serde(
            rename = "errorMessage",
            skip_serializing_if = "Option::is_none",
            default
        )]
        error_message: Option<String>,
        #[serde(
            rename = "responseId",
            skip_serializing_if = "Option::is_none",
            default
        )]
        response_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rewrite: Option<PiMessagesRewriteImpact>,
    },
}

/// Port of `resolveCacheRetention`: backend defaults apply when unset; only
/// the legacy env opt-in is mapped.
fn resolve_cache_retention(
    cache_retention: Option<crate::ai::types::CacheRetention>,
    env: Option<&crate::ai::types::ProviderEnv>,
) -> Option<crate::ai::types::CacheRetention> {
    if let Some(retention) = cache_retention {
        return Some(retention);
    }
    (get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long"))
        .then_some(crate::ai::types::CacheRetention::Long)
}

/// Port of `parsePiMessagesEvent`: extracts the first `data:` line and parses
/// it; `[DONE]` and missing data yield `None`.
fn parse_pi_messages_event(raw: &str) -> Option<Value> {
    let data = raw
        .lines()
        .find(|line| line.starts_with("data:"))
        .map(|line| line[5..].trim())?;
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

/// Port of `readPiMessagesEvents`: splits the body on `\n\n` (after CRLF
/// normalization) and parses each frame; the trailing remainder is decoded.
const PI_EVENT_BOUNDARIES: &[&str] = &["\r\n\r\n", "\r\n\n", "\n\r\n", "\n\n"];

/// Port of `parsePiMessagesErrorBody`.
fn parse_pi_messages_error_body(body: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let error = parsed.get("error")?;
    error.is_object().then_some(parsed)
}

fn truncate_diagnostic_string(value: &str) -> String {
    const MAX_LENGTH: usize = 8192;
    if value.len() > MAX_LENGTH {
        let mut end = MAX_LENGTH;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
    } else {
        value.to_string()
    }
}

/// Port of `createPiMessagesResponseError` + `formatPiMessagesResponseError`:
/// returns (message, code, diagnostic details).
fn create_pi_messages_response_error(
    model: &Model,
    url: &str,
    status: u16,
    status_text: &str,
    body: &str,
) -> (String, Option<String>, Value) {
    let error_body = parse_pi_messages_error_body(body);
    let message = error_body
        .as_ref()
        .and_then(|parsed| parsed.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let code = error_body
        .as_ref()
        .and_then(|parsed| parsed.pointer("/error/code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let suffix = message.clone().unwrap_or_else(|| body.to_string());
    let code_suffix = code
        .as_ref()
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();
    let formatted = format!("{status} {status_text}: {suffix}{code_suffix}");
    let details = json!({
        "version": 1,
        "provider": model.provider,
        "model": model.id,
        "url": url,
        "status": status,
        "statusText": status_text,
        "error": error_body.as_ref().and_then(|parsed| parsed.get("error")).cloned().unwrap_or(Value::Null),
        "body": if error_body.is_none() { Value::String(truncate_diagnostic_string(body)) } else { Value::Null },
        "timestampMs": crate::ai::auth::resolve::now_millis(),
    });
    (formatted, code, details)
}

/// Port of `createEventConverter`: folds wire events into the partial
/// assistant message and emits the standard event protocol.
struct EventConverter {
    partial: AssistantMessage,
    tool_json: BTreeMap<usize, String>,
}

impl EventConverter {
    fn new(model: &Model) -> Self {
        Self {
            partial: AssistantMessage {
                role: crate::ai::types::RoleAssistant,
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: Default::default(),
                stop_reason: StopReason::Pending,
                timestamp: crate::ai::auth::resolve::now_millis(),
                ..Default::default()
            },
            tool_json: BTreeMap::new(),
        }
    }

    fn append_rewrite_diagnostic(&mut self, rewrite: Option<&Value>) {
        let Some(rewrite) = rewrite else {
            return;
        };
        let diagnostic = crate::ai::types::AssistantMessageDiagnostic {
            kind: "pi_messages_rewrite".to_string(),
            timestamp: crate::ai::auth::resolve::now_millis(),
            error: None,
            details: Some(rewrite.clone()),
        };
        append_assistant_message_diagnostic(&mut self.partial, diagnostic);
    }

    /// Applies a wire event, returning the protocol event to emit.
    fn convert(&mut self, event: &Value) -> AssistantMessageEvent {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match event_type.as_str() {
            "done" => {
                self.partial.stop_reason = match event.get("reason").and_then(Value::as_str) {
                    Some("length") => StopReason::Length,
                    Some("toolUse") => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
                if let Some(usage) = event.get("usage") {
                    self.partial.usage = serde_json::from_value(usage.clone()).unwrap_or_default();
                }
                if let Some(response_id) = event.get("responseId").and_then(Value::as_str) {
                    self.partial.response_id = Some(response_id.to_string());
                }
                self.append_rewrite_diagnostic(event.get("rewrite"));
                AssistantMessageEvent::Done {
                    reason: match self.partial.stop_reason {
                        StopReason::Length => DoneReason::Length,
                        StopReason::ToolUse => DoneReason::ToolUse,
                        _ => DoneReason::Stop,
                    },
                    message: self.partial.clone(),
                }
            }
            "error" => {
                self.partial.stop_reason = match event.get("reason").and_then(Value::as_str) {
                    Some("aborted") => StopReason::Aborted,
                    _ => StopReason::Error,
                };
                if let Some(usage) = event.get("usage") {
                    self.partial.usage = serde_json::from_value(usage.clone()).unwrap_or_default();
                }
                if let Some(error_message) = event.get("errorMessage").and_then(Value::as_str) {
                    self.partial.error_message = Some(error_message.to_string());
                }
                if let Some(response_id) = event.get("responseId").and_then(Value::as_str) {
                    self.partial.response_id = Some(response_id.to_string());
                }
                self.append_rewrite_diagnostic(event.get("rewrite"));
                AssistantMessageEvent::Error {
                    reason: if self.partial.stop_reason == StopReason::Aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: self.partial.clone(),
                }
            }
            "start" => AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            },
            "text_start" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if self.partial.content.len() <= index {
                    self.partial
                        .content
                        .resize_with(index + 1, || AssistantContent::Text(Default::default()));
                }
                self.partial.content[index] = AssistantContent::Text(Default::default());
                AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: self.partial.clone(),
                }
            }
            "text_delta" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                    block.text.push_str(delta);
                }
                AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: delta.to_string(),
                    partial: self.partial.clone(),
                }
            }
            "text_end" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let signature = event
                    .get("contentSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                    block.text = content.clone();
                    block.text_signature = signature;
                }
                AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.partial.clone(),
                }
            }
            "thinking_start" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if self.partial.content.len() <= index {
                    self.partial
                        .content
                        .resize_with(index + 1, || AssistantContent::Thinking(Default::default()));
                }
                self.partial.content[index] = AssistantContent::Thinking(Default::default());
                AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.partial.clone(),
                }
            }
            "thinking_delta" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index)
                {
                    block.thinking.push_str(delta);
                }
                AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: delta.to_string(),
                    partial: self.partial.clone(),
                }
            }
            "thinking_end" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let signature = event
                    .get("contentSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let redacted = event.get("redacted").and_then(Value::as_bool);
                if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index)
                {
                    block.thinking = content.clone();
                    block.thinking_signature = signature;
                    block.redacted = redacted;
                }
                AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.partial.clone(),
                }
            }
            "toolcall_start" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let tool_name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if self.partial.content.len() <= index {
                    self.partial
                        .content
                        .resize_with(index + 1, || AssistantContent::ToolCall(Default::default()));
                }
                self.partial.content[index] = AssistantContent::ToolCall(ToolCall {
                    content_type: Default::default(),
                    id,
                    name: tool_name,
                    arguments: Default::default(),
                    ..Default::default()
                });
                self.tool_json.insert(index, String::new());
                AssistantMessageEvent::ToolcallStart {
                    content_index: index,
                    partial: self.partial.clone(),
                }
            }
            "toolcall_delta" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let json = {
                    let existing = self.tool_json.get(&index).cloned().unwrap_or_default();
                    format!("{existing}{delta}")
                };
                self.tool_json.insert(index, json.clone());
                if let Some(AssistantContent::ToolCall(block)) = self.partial.content.get_mut(index)
                {
                    block.arguments = parse_streaming_json(Some(&json))
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                }
                AssistantMessageEvent::ToolcallDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                }
            }
            "toolcall_end" => {
                let index = event
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if let Some(tool_call) = event.get("toolCall")
                    && let Some(AssistantContent::ToolCall(block)) =
                        self.partial.content.get_mut(index)
                {
                    if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                        block.id = id.to_string();
                    }
                    if let Some(name) = tool_call.get("name").and_then(Value::as_str) {
                        block.name = name.to_string();
                    }
                    if let Some(arguments) = tool_call.get("arguments").and_then(Value::as_object) {
                        block.arguments = arguments.clone();
                    }
                    if let Some(thought_signature) =
                        tool_call.get("thoughtSignature").and_then(Value::as_str)
                    {
                        block.thought_signature = Some(thought_signature.to_string());
                    }
                }
                self.tool_json.remove(&index);
                let tool_call = match self.partial.content.get(index) {
                    Some(AssistantContent::ToolCall(tool_call)) => tool_call.clone(),
                    _ => ToolCall::default(),
                };
                AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call,
                    partial: self.partial.clone(),
                }
            }
            _ => AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            },
        }
    }
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&PiMessagesOptions>,
) -> AssistantMessageEventStream {
    let event_stream = create_assistant_message_event_stream();
    let model = model.clone();
    let context = context.clone();
    let options = options.cloned();
    let producer = event_stream.clone();
    tokio::spawn(async move {
        let mut converter = EventConverter::new(&model);

        let result = run_stream(
            &model,
            &context,
            options.as_ref(),
            &mut converter,
            &producer,
        )
        .await;
        if let Err(error) = result {
            let aborted = options
                .as_ref()
                .and_then(|options| options.base.base.signal.as_ref())
                .is_some_and(|token| token.is_cancelled());
            let mut assistant_message = AssistantMessage {
                role: crate::ai::types::RoleAssistant,
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: Default::default(),
                stop_reason: if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                },
                error_message: Some(error.message),
                timestamp: crate::ai::auth::resolve::now_millis(),
                ..Default::default()
            };
            if !aborted && let Some(details) = error.diagnostic_details {
                let diagnostic = crate::ai::types::AssistantMessageDiagnostic {
                    kind: "pi_messages_response_failure".to_string(),
                    timestamp: crate::ai::auth::resolve::now_millis(),
                    error: Some(crate::ai::types::DiagnosticErrorInfo {
                        name: Some("PiMessagesResponseError".to_string()),
                        message: assistant_message.error_message.clone().unwrap_or_default(),
                        stack: None,
                        code: error.code.map(crate::ai::types::DiagnosticErrorCode::Text),
                    }),
                    details: Some(details),
                };
                append_assistant_message_diagnostic(&mut assistant_message, diagnostic);
            }
            producer.push(AssistantMessageEvent::Error {
                reason: if assistant_message.stop_reason == StopReason::Aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                },
                error: assistant_message,
            });
        }
    });
    event_stream
}

/// Port of the `PiMessagesResponseError` shape (message, code, diagnostic
/// details). Rather than throwing mid-stream, the Rust port threads this
/// payload from `run_stream` into the stream's error event.
pub struct StreamFailure {
    message: String,
    code: Option<String>,
    diagnostic_details: Option<Value>,
}

#[allow(clippy::too_many_lines)]
async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&PiMessagesOptions>,
    converter: &mut EventConverter,
    producer: &AssistantMessageEventStream,
) -> Result<(), StreamFailure> {
    let fail = |message: String| StreamFailure {
        message,
        code: None,
        diagnostic_details: None,
    };
    let Some(api_key) = options.and_then(|options| options.base.base.api_key.clone()) else {
        return Err(fail(format!(
            "No API key provided for provider \"{}\"",
            model.provider
        )));
    };

    let base_url = model.base_url.trim_end_matches('/');
    let mut url = format!("{base_url}/messages");
    if options.and_then(|options| options.debug).unwrap_or(false) {
        url.push_str("?debug=1");
    }

    let cache_retention = resolve_cache_retention(
        options.and_then(|options| options.base.cache_retention),
        options.and_then(|options| options.base.base.env.as_ref()),
    );
    let mut payload = json!({
        "model": model.id,
        // The context is embedded verbatim (TS spreads the `Context` object in,
        // whose `JSON.stringify` skips `undefined` fields).
        "context": context,
        "options": {
            "temperature": options.and_then(|options| options.base.temperature),
            "maxTokens": options.and_then(|options| options.base.max_tokens),
            "reasoning": options.and_then(|options| options.reasoning).map(|level| match level {
                ThinkingLevel::Minimal => "minimal",
                ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::High => "high",
                ThinkingLevel::Xhigh => "xhigh",
                ThinkingLevel::Max => "max",
            }),
            "cacheRetention": cache_retention,
            "sessionId": options.and_then(|options| options.base.session_id.clone()),
            "toolChoice": options.and_then(|options| options.tool_choice.clone()),
        },
    });
    // TS `JSON.stringify` omits `undefined` option fields; the Rust JSON
    // literal materializes unset ones as null, so they are dropped here.
    if let Some(options) = payload.get_mut("options").and_then(Value::as_object_mut) {
        options.retain(|_, value| !value.is_null());
    }
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_payload) = on_payload(payload.clone(), model).await
    {
        payload = next_payload;
    }

    let mut headers: Vec<(String, String)> = vec![
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("accept".to_string(), "text/event-stream".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    if let Some(options_headers) = options.and_then(|options| options.base.base.headers.as_ref()) {
        for (name, value) in options_headers {
            if let Some(value) = value {
                headers.push((name.clone(), value.clone()));
            }
        }
    }

    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);
    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url: url.clone(),
        headers,
        body: HttpBody::Json(payload),
    };

    let response = fetch
        .fetch(request)
        .await
        .map_err(|error| fail(error.to_string()))?;
    let status = response.status;

    if let Some(on_response) = options.and_then(|options| options.base.base.on_response.as_ref()) {
        let response_headers: BTreeMap<String, String> = response
            .headers
            .iter()
            .map(|(name, value)| (name.to_lowercase(), value.clone()))
            .collect();
        on_response(
            &crate::ai::types::ProviderResponse {
                status,
                headers: response_headers,
            },
            model,
        )
        .await;
    }

    if !(200..300).contains(&status) {
        let body = crate::ai::utils::http::collect_text(response).await;
        // TS reads `response.statusText`; the HTTP response carries only the
        // numeric status, so the canonical reason phrase stands in (the
        // Node/undici default for a missing reason phrase).
        let status_text = reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or_default();
        let (message, code, details) =
            create_pi_messages_response_error(model, &url, status, status_text, &body);
        return Err(StreamFailure {
            message,
            code,
            diagnostic_details: Some(details),
        });
    }

    let body = crate::ai::utils::http::abortable_body(
        response.body,
        options.and_then(|options| options.base.base.signal.clone()),
    );
    let mut frames = crate::ai::utils::http::text_frames(body, PI_EVENT_BOUNDARIES);
    while let Some(frame) = futures::StreamExt::next(&mut frames).await {
        let frame = frame.map_err(|error| fail(error.to_string()))?;
        let Some(pi_event) = parse_pi_messages_event(&frame.replace("\r\n", "\n")) else {
            continue;
        };
        let event = converter.convert(&pi_event);
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        producer.push(event);
        if terminal {
            return Ok(());
        }
    }

    Err(fail(format!(
        "{} stream ended without a terminal event",
        model.provider
    )))
}

/// Port of `streamSimple`: forwards the shared `toolChoice`/`reasoning` and
/// the `debug` flag attached to the options object (TS casts
/// `options as PiMessagesOptions` to read `debug`; Rust callers carry it in
/// the request options' `extra` map).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned().unwrap_or_default();
    let tool_choice = options
        .tool_choice
        .as_ref()
        .and_then(|choice| serde_json::to_value(choice).ok());
    let debug = options
        .base
        .base
        .extra
        .get("debug")
        .and_then(Value::as_bool);
    stream(
        model,
        context,
        Some(&PiMessagesOptions {
            base: options.base,
            reasoning: options.reasoning,
            tool_choice,
            debug,
        }),
    )
}
