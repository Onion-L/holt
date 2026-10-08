//! Port of `pi-core/ai/src/api/mistral-conversations.ts`.
//!
//! The Mistral SDK's HTTP layer becomes a direct POST to
//! `{baseUrl}/v1/chat/completions` (camelCase payload keys remapped to the
//! wire snake_case form exactly like `toMistralWirePayload`), reading the
//! raw `data:` frame protocol with its multi-boundary splitting
//! (`\r\n\r\n`, `\r\n\r`, `\r\r`, …) rather than the shared SSE decoder.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::ai::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::ai::api::simple_options::build_base_options;
use crate::ai::api::transform_messages::transform_messages;
use crate::ai::models::{calculate_cost, clamp_thinking_level};
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    Message, Model, SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent,
    ThinkingLevel, Tool, ToolCall, ToolCallArguments, UserContent,
};
use crate::ai::utils::error_body::truncate_error_text;
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::json_parse::parse_streaming_json;
use crate::ai::utils::provider_retry::retry_http_request;
use crate::ai::utils::reqwest_fetch::default_fetch;
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;
use crate::ai::utils::text::short_hash;

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;

/// Port of `MistralReasoningEffort`.
pub type MistralReasoningEffort = str;

/// Port of `MistralOptions`.
#[derive(Clone, Default)]
pub struct MistralOptions {
    pub base: StreamOptions,
    /// "auto" | "none" | "any" | "required" | {"type":"function","function":{"name":...}}.
    pub tool_choice: Option<Value>,
    pub prompt_mode: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// Payload-level message (`MistralChatMessage`): role plus optional content
/// chunks / tool calls / tool-call id / name / prefix, in camelCase.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MistralChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MistralContent>,
    #[serde(rename = "toolCalls", skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<MistralRequestToolCall>,
    #[serde(rename = "toolCallId", skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<bool>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(untagged)]
enum MistralContent {
    Text(String),
    Chunks(Vec<MistralContentChunk>),
}

/// The `{type: "text", text}` part inside a `thinking` chunk array.
#[derive(Clone, Debug, serde::Serialize)]
struct MistralThinkingChunk {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

impl MistralThinkingChunk {
    fn new(text: String) -> Self {
        Self { kind: "text", text }
    }
}

/// Port of `MistralContentChunk` in its pre-wire camelCase form (`imageUrl`
/// is renamed to `image_url` by `to_mistral_wire_payload`).
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum MistralContentChunk {
    Text {
        text: String,
    },
    #[serde(rename = "image_url")]
    ImageUrl {
        #[serde(rename = "imageUrl")]
        image_url: String,
    },
    Thinking {
        thinking: Vec<MistralThinkingChunk>,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
struct MistralRequestToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: MistralRequestToolCallFunction,
    index: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
struct MistralRequestToolCallFunction {
    name: String,
    arguments: String,
}

/// Port of `buildChatPayload` output (camelCase) carried as JSON after wire
/// remapping.
type MistralPayload = Value;

/// Port of `createMistralToolCallIdNormalizer` + `deriveMistralToolCallId`:
/// deterministic 9-char alphanumeric ids with collision resolution.
struct MistralToolCallIdNormalizer {
    id_map: BTreeMap<String, String>,
    reverse_map: BTreeMap<String, String>,
}

impl MistralToolCallIdNormalizer {
    fn new() -> Self {
        Self {
            id_map: BTreeMap::new(),
            reverse_map: BTreeMap::new(),
        }
    }

    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.id_map.get(id) {
            return existing.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            match self.reverse_map.get(&candidate) {
                Some(owner) if owner != id => {
                    attempt += 1;
                }
                _ => {
                    self.id_map.insert(id.to_string(), candidate.clone());
                    self.reverse_map.insert(candidate.clone(), id.to_string());
                    return candidate;
                }
            }
        }
    }
}

fn derive_mistral_tool_call_id(id: &str, attempt: u32) -> String {
    let normalized: String = id.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.chars().count() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id.to_string()
    } else {
        normalized
    };
    let seed = if attempt == 0 {
        seed_base
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

/// Port of `formatMistralError`: composes the display string from status and
/// body parts (the SDK error fields become explicit inputs).
fn format_mistral_error(status: Option<u16>, body: Option<&str>, message: &str) -> String {
    let body_text = body.map(str::trim).filter(|body| !body.is_empty());
    if let Some(status) = status {
        if let Some(body_text) = body_text {
            return format!(
                "Mistral API error ({status}): {}",
                truncate_error_text(body_text, MAX_MISTRAL_ERROR_BODY_CHARS)
            );
        }
        return format!("Mistral API error ({status}): {message}");
    }
    message.to_string()
}

fn should_use_prompt_caching(options: Option<&MistralOptions>) -> Option<String> {
    let options = options?;
    if options.base.cache_retention == Some(crate::ai::types::CacheRetention::None) {
        return None;
    }
    options.base.session_id.clone().filter(|id| !id.is_empty())
}

/// Port of `getMistralCachedPromptTokens`: probes the four placement
/// spellings and clamps to the prompt token count.
fn get_mistral_cached_prompt_tokens(usage: &Value, prompt_tokens: u64) -> u64 {
    let raw_cached = usage
        .pointer("/promptTokensDetails/cachedTokens")
        .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
        .or_else(|| usage.pointer("/promptTokenDetails/cachedTokens"))
        .or_else(|| usage.pointer("/prompt_token_details/cached_tokens"))
        .or_else(|| usage.get("numCachedTokens"))
        .or_else(|| usage.get("num_cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    prompt_tokens.min(raw_cached)
}

/// Port of `mapChatStopReason`.
fn map_chat_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    let Some(reason) = reason else {
        return (StopReason::Stop, None);
    };
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        "error" => (
            StopReason::Error,
            Some("Provider stopped with: error".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

/// Port of `toFunctionTools`.
fn to_function_tools(tools: &[Tool]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, true)?;
            let parameters = get_json_schema_tool_parameters(tool, strict == Some(true))?;
            Ok(json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters,
                    "strict": strict.unwrap_or(false),
                },
            }))
        })
        .collect()
}

/// Port of `buildToolResultText`.
fn build_tool_result_text(
    text: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };

    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }

    if has_images {
        if supports_images {
            return if is_error {
                "[tool error] (see attached image)".to_string()
            } else {
                "(see attached image)".to_string()
            };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".to_string()
        } else {
            "(image omitted: model does not support images)".to_string()
        };
    }

    if is_error {
        "[tool error] (no tool output)".to_string()
    } else {
        "(no tool output)".to_string()
    }
}

/// Port of `toChatMessages`.
fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<MistralChatMessage> {
    let mut messages_out: Vec<MistralChatMessage> = Vec::new();

    for message in messages {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    messages_out.push(MistralChatMessage {
                        role: "user".to_string(),
                        content: Some(MistralContent::Text(sanitize_surrogates(text))),
                        ..Default::default()
                    });
                }
                UserContent::Blocks(blocks) => {
                    let had_images = blocks
                        .iter()
                        .any(|item| matches!(item, crate::ai::types::BlockContent::Image(_)));
                    let content: Vec<MistralContentChunk> = blocks
                        .iter()
                        .filter(|item| {
                            matches!(item, crate::ai::types::BlockContent::Text(_))
                                || supports_images
                        })
                        .map(|item| match item {
                            crate::ai::types::BlockContent::Text(text) => {
                                MistralContentChunk::Text {
                                    text: sanitize_surrogates(&text.text),
                                }
                            }
                            crate::ai::types::BlockContent::Image(image) => {
                                MistralContentChunk::ImageUrl {
                                    image_url: format!(
                                        "data:{};base64,{}",
                                        image.mime_type, image.data
                                    ),
                                }
                            }
                        })
                        .collect();
                    if !content.is_empty() {
                        messages_out.push(MistralChatMessage {
                            role: "user".to_string(),
                            content: Some(MistralContent::Chunks(content)),
                            ..Default::default()
                        });
                        continue;
                    }
                    if had_images && !supports_images {
                        messages_out.push(MistralChatMessage {
                            role: "user".to_string(),
                            content: Some(MistralContent::Text(
                                "(image omitted: model does not support images)".to_string(),
                            )),
                            ..Default::default()
                        });
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut content_parts: Vec<MistralContentChunk> = Vec::new();
                let mut tool_calls: Vec<MistralRequestToolCall> = Vec::new();

                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content_parts.push(MistralContentChunk::Text {
                                    text: sanitize_surrogates(&text.text),
                                });
                            }
                        }
                        AssistantContent::Thinking(thinking) => {
                            if !thinking.thinking.trim().is_empty() {
                                content_parts.push(MistralContentChunk::Thinking {
                                    thinking: vec![MistralThinkingChunk::new(sanitize_surrogates(
                                        &thinking.thinking,
                                    ))],
                                });
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            tool_calls.push(MistralRequestToolCall {
                                id: tool_call.id.clone(),
                                kind: "function".to_string(),
                                function: MistralRequestToolCallFunction {
                                    name: tool_call.name.clone(),
                                    arguments: serde_json::to_string(&tool_call.arguments)
                                        .unwrap_or_default(),
                                },
                                index: 0,
                            });
                        }
                    }
                }

                let mut assistant_message = MistralChatMessage {
                    role: "assistant".to_string(),
                    prefix: Some(false),
                    ..Default::default()
                };
                if !content_parts.is_empty() {
                    assistant_message.content = Some(MistralContent::Chunks(content_parts.clone()));
                }
                if !tool_calls.is_empty() {
                    assistant_message.tool_calls = tool_calls.clone();
                }
                if !content_parts.is_empty() || !tool_calls.is_empty() {
                    messages_out.push(assistant_message);
                }
            }
            Message::ToolResult(tool_result) => {
                let result = &**tool_result;
                let text_result = result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        crate::ai::types::BlockContent::Text(text) => {
                            Some(sanitize_surrogates(&text.text))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = result
                    .content
                    .iter()
                    .any(|part| matches!(part, crate::ai::types::BlockContent::Image(_)));
                let tool_text = build_tool_result_text(
                    &text_result,
                    has_images,
                    supports_images,
                    result.is_error,
                );
                let mut tool_content = vec![MistralContentChunk::Text { text: tool_text }];
                for part in &result.content {
                    if !supports_images {
                        continue;
                    }
                    if let crate::ai::types::BlockContent::Image(image) = part {
                        tool_content.push(MistralContentChunk::ImageUrl {
                            image_url: format!("data:{};base64,{}", image.mime_type, image.data),
                        });
                    }
                }
                messages_out.push(MistralChatMessage {
                    role: "tool".to_string(),
                    tool_call_id: Some(result.tool_call_id.clone()),
                    name: Some(result.tool_name.clone()),
                    content: Some(MistralContent::Chunks(tool_content)),
                    ..Default::default()
                });
            }
        }
    }

    messages_out
}

/// Port of `toMistralWirePayload`: camelCase payload keys to the wire
/// snake_case form.
fn to_mistral_wire_payload(payload: &mut Value) {
    let remap = |object: &mut Map<String, Value>, source: &str, target: &str| {
        if let Some(value) = object.remove(source) {
            object.insert(target.to_string(), value);
        }
    };
    if let Some(object) = payload.as_object_mut() {
        for (source, target) in [
            ("topP", "top_p"),
            ("maxTokens", "max_tokens"),
            ("randomSeed", "random_seed"),
            ("responseFormat", "response_format"),
            ("toolChoice", "tool_choice"),
            ("presencePenalty", "presence_penalty"),
            ("frequencyPenalty", "frequency_penalty"),
            ("parallelToolCalls", "parallel_tool_calls"),
            ("reasoningEffort", "reasoning_effort"),
            ("promptMode", "prompt_mode"),
            ("promptCacheKey", "prompt_cache_key"),
            ("safePrompt", "safe_prompt"),
        ] {
            remap(object, source, target);
        }
    }
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            if let Some(object) = message.as_object_mut() {
                remap(object, "toolCalls", "tool_calls");
                remap(object, "toolCallId", "tool_call_id");
                if let Some(content) = object.get_mut("content") {
                    remap_content_chunks(content);
                }
            }
        }
    }
    if let Some(response_format) = payload
        .get_mut("response_format")
        .and_then(Value::as_object_mut)
    {
        remap(response_format, "jsonSchema", "json_schema");
        if let Some(json_schema) = response_format
            .get_mut("json_schema")
            .and_then(Value::as_object_mut)
        {
            remap(json_schema, "schemaDefinition", "schema");
        }
    }
}

fn remap_content_chunks(content: &mut Value) {
    let Some(chunks) = content.as_array_mut() else {
        return;
    };
    for chunk in chunks.iter_mut() {
        let Some(object) = chunk.as_object_mut() else {
            continue;
        };
        for (source, target) in [
            ("imageUrl", "image_url"),
            ("documentUrl", "document_url"),
            ("documentName", "document_name"),
            ("fileId", "file_id"),
            ("referenceIds", "reference_ids"),
            ("inputAudio", "input_audio"),
        ] {
            if let Some(value) = object.remove(source) {
                object.insert(target.to_string(), value);
            }
        }
    }
}

/// Port of `buildChatPayload`: the camelCase SDK-style payload (the wire
/// snake_case remap happens in `request_mistral_stream`, after `onPayload`).
fn build_chat_payload(
    model: &Model,
    context: &Context,
    messages: &[Message],
    options: Option<&MistralOptions>,
) -> Result<MistralPayload, String> {
    let options = options.cloned().unwrap_or_default();
    let chat_messages = to_chat_messages(
        messages,
        model.input.contains(&crate::ai::types::ModelInput::Image),
    );
    let message_values: Vec<Value> = chat_messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
        .collect();
    let mut payload = json!({
        "model": model.id,
        "stream": true,
        "messages": message_values,
    });

    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        payload["tools"] = json!(to_function_tools(tools)?);
    }
    if let Some(temperature) = options.base.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.base.max_tokens {
        payload["maxTokens"] = json!(max_tokens);
    }
    if let Some(tool_choice) = &options.tool_choice {
        payload["toolChoice"] = tool_choice.clone();
    }
    if let Some(prompt_mode) = &options.prompt_mode {
        payload["promptMode"] = json!(prompt_mode);
    }
    if let Some(reasoning_effort) = &options.reasoning_effort {
        payload["reasoningEffort"] = json!(reasoning_effort);
    }
    if let Some(session_id) = should_use_prompt_caching(Some(&options)) {
        payload["promptCacheKey"] = json!(session_id);
    }

    if let Some(system_prompt) = &context.system_prompt
        && let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut)
    {
        messages.insert(
            0,
            json!({
                "role": "system",
                "content": sanitize_surrogates(system_prompt),
            }),
        );
    }

    Ok(payload)
}

/// Port of `buildMistralHeaders`.
fn build_mistral_headers(
    model: &Model,
    api_key: &str,
    options: Option<&MistralOptions>,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = vec![
        (
            "User-Agent".to_string(),
            crate::ai::session_resources::get_pi_user_agent(),
        ),
        ("accept".to_string(), "text/event-stream".to_string()),
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    // Overrides: null deletes, values replace (matched case-insensitively).
    let apply_overrides =
        |headers: &mut Vec<(String, String)>,
         overrides: Option<&crate::ai::types::ProviderHeaders>| {
            if let Some(overrides) = overrides {
                for (name, value) in overrides {
                    let lower = name.to_lowercase();
                    headers.retain(|(key, _)| key.to_lowercase() != lower);
                    if let Some(value) = value {
                        headers.push((name.clone(), value.clone()));
                    }
                }
            }
        };
    if let Some(model_headers) = &model.headers {
        let converted: crate::ai::types::ProviderHeaders = model_headers
            .iter()
            .map(|(name, value)| (name.clone(), Some(value.clone())))
            .collect();
        apply_overrides(&mut headers, Some(&converted));
    }
    apply_overrides(
        &mut headers,
        options.and_then(|options| options.base.base.headers.as_ref()),
    );

    let has_explicit_affinity = model.headers.as_ref().is_some_and(|headers| {
        headers
            .keys()
            .any(|name| name.to_lowercase() == "x-affinity")
    }) || options
        .and_then(|options| options.base.base.headers.as_ref())
        .is_some_and(|headers| {
            headers
                .keys()
                .any(|name| name.to_lowercase() == "x-affinity")
        });
    if let Some(session_id) = should_use_prompt_caching(options)
        && !has_explicit_affinity
    {
        headers.retain(|(key, _)| key.to_lowercase() != "x-affinity");
        headers.push(("x-affinity".to_string(), session_id));
    }
    let _ = session_id;
    headers
}

/// Mistral event-frame boundary search. Port of `findMistralEventBoundary`:
/// the longest of `\r\n\r\n | \r\n\r | \r\n\n | \r\r\n | \n\r\n | \r\r | \n\r | \n\n`.
const MISTRAL_EVENT_BOUNDARIES: &[&str] = &[
    "\r\n\r\n", "\r\n\r", "\r\n\n", "\r\r\n", "\n\r\n", "\r\r", "\n\r", "\n\n",
];

/// Port of `parseMistralEvent`: `Ok(Some(event))`, `Ok(None)` for no data,
/// `Ok(DONE)` for `[DONE]`.
enum ParsedMistralEvent {
    Event(Value),
    Done,
    Empty,
}

fn parse_mistral_event(raw: &str) -> Result<ParsedMistralEvent, String> {
    let data = raw
        .split(['\r', '\n'])
        .filter(|line| line.starts_with("data:"))
        .map(|line| line[5..].trim_start())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if data.is_empty() {
        return Ok(ParsedMistralEvent::Empty);
    }
    if data == "[DONE]" {
        return Ok(ParsedMistralEvent::Done);
    }

    let parsed: Value = serde_json::from_str(&data).map_err(|error| error.to_string())?;
    if !parsed.is_object() || parsed.get("choices").and_then(Value::as_array).is_none() {
        return Err("Invalid Mistral streaming event".to_string());
    }
    Ok(ParsedMistralEvent::Event(parsed))
}

/// Issues the streaming request (port of `requestMistralStream`).
async fn request_mistral_stream(
    model: &Model,
    payload: MistralPayload,
    api_key: &str,
    options: Option<&MistralOptions>,
) -> Result<crate::ai::utils::http::HttpResponse, String> {
    let base_url = model.base_url.trim_end_matches('/');
    let headers = {
        let session_id = should_use_prompt_caching(options);
        build_mistral_headers(model, api_key, options, session_id.as_deref())
    };
    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);
    let mut payload = payload;
    to_mistral_wire_payload(&mut payload);
    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url: format!("{base_url}/v1/chat/completions"),
        headers,
        body: HttpBody::Json(payload),
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

    if !(200..300).contains(&response.status) {
        let status = response.status;
        let body = crate::ai::utils::http::collect_text(response).await;
        return Err(format_mistral_error(
            Some(status),
            Some(&body),
            &format!("Request failed with status {status}"),
        ));
    }
    Ok(response)
}

/// Incremental `readMistralEvents`, including its whole-body timeout.
fn read_mistral_events(
    response: crate::ai::utils::http::HttpResponse,
    timeout_ms: Option<u64>,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> futures::stream::BoxStream<'static, Result<Value, String>> {
    let frames = crate::ai::utils::http::text_frames(response.body, MISTRAL_EVENT_BOUNDARIES);
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(timeout_ms.unwrap_or(60_000));
    Box::pin(futures::stream::unfold(
        Some((frames, signal, deadline)),
        |state| async {
            let (mut frames, signal, deadline) = state?;
            loop {
                let frame = tokio::select! {
                    frame = futures::StreamExt::next(&mut frames) => frame,
                    () = tokio::time::sleep_until(deadline) => return Some((Err("The operation was aborted due to timeout".into()), None)),
                    () = async {
                        match &signal {
                            Some(signal) => signal.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => return Some((Err("This operation was aborted".into()), None)),
                }?;
                match frame
                    .map_err(|error| error.to_string())
                    .and_then(|raw| parse_mistral_event(&raw))
                {
                    Ok(ParsedMistralEvent::Done) => return None,
                    Ok(ParsedMistralEvent::Event(event)) => {
                        return Some((Ok(event), Some((frames, signal, deadline))));
                    }
                    Ok(ParsedMistralEvent::Empty) => {}
                    Err(error) => return Some((Err(error), None)),
                }
            }
        },
    ))
}

/// Port of the `stream` stream function.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&MistralOptions>,
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
            api: "mistral-conversations".to_string(),
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
                .is_some_and(|token| token.is_cancelled())
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

enum MistralBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
}

#[allow(clippy::too_many_lines)]
async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&MistralOptions>,
    output: &mut AssistantMessage,
    producer: &AssistantMessageEventStream,
) -> Result<(), String> {
    let Some(api_key) = options.and_then(|options| options.base.base.api_key.clone()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };

    let mut normalizer = MistralToolCallIdNormalizer::new();
    let normalized_ids: Vec<String> = context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call.id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            ),
            Message::ToolResult(result) => Some(vec![result.tool_call_id.clone()]),
            Message::User(_) => None,
        })
        .flatten()
        .collect();
    let mut id_map: BTreeMap<String, String> = BTreeMap::new();
    for id in normalized_ids {
        let normalized = normalizer.normalize(&id);
        id_map.insert(id, normalized);
    }
    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(&|id: &str, _source: &crate::ai::types::AssistantMessage| {
            id_map.get(id).cloned().unwrap_or_else(|| id.to_string())
        }),
    );

    let mut payload = build_chat_payload(model, context, &transformed_messages, options)?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_payload) = on_payload(payload.clone(), model).await
    {
        payload = next_payload;
    }
    let response = request_mistral_stream(model, payload, &api_key, options).await?;
    producer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });
    let events = read_mistral_events(
        response,
        options.and_then(|options| options.base.base.timeout_ms),
        options.and_then(|options| options.base.base.signal.clone()),
    );
    let mut scratch_args: BTreeMap<usize, String> = BTreeMap::new();
    consume_chat_stream(model, output, producer, events, &mut scratch_args).await?;

    let signal = options.and_then(|options| options.base.base.signal.clone());
    if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason == StopReason::Pending {
        return Err("Mistral stream ended without a finish reason".to_string());
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

/// Port of `usesReasoningEffort`.
fn uses_reasoning_effort(model: &Model) -> bool {
    model.id == "mistral-small-2603"
        || model.id == "mistral-small-latest"
        || model.id == "mistral-medium-3.5"
}

/// Port of `usesPromptModeReasoning`.
fn uses_prompt_mode_reasoning(model: &Model) -> bool {
    model.reasoning && !uses_reasoning_effort(model)
}

/// Port of `mapReasoningEffort`.
fn map_reasoning_effort(model: &Model, level: ThinkingLevel) -> String {
    let key = crate::ai::types::ModelThinkingLevel::from(level);
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&key))
        .cloned()
        .flatten()
        .unwrap_or_else(|| "high".to_string())
}

/// Port of `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options.and_then(|options| options.base.base.api_key.clone());
    let Some(api_key) = api_key else {
        return crate::ai::api::openai_completions::error_stream(
            model,
            &format!("No API key for provider: {}", model.provider),
        );
    };
    let _ = api_key;

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
    let reasoning = clamped_reasoning.and_then(|level| level.as_thinking_level());
    let should_use_reasoning = model.reasoning && reasoning.is_some();

    stream(
        model,
        context,
        Some(&MistralOptions {
            base,
            prompt_mode: if should_use_reasoning && uses_prompt_mode_reasoning(model) {
                Some("reasoning".to_string())
            } else {
                None
            },
            reasoning_effort: if should_use_reasoning && uses_reasoning_effort(model) {
                reasoning.map(|level| map_reasoning_effort(model, level))
            } else {
                None
            },
            ..Default::default()
        }),
    )
}

/// Port of `consumeChatStream`: applies parsed events to the output and
/// emits the streaming protocol events.
#[allow(clippy::too_many_lines)]
async fn consume_chat_stream(
    model: &Model,
    output: &mut AssistantMessage,
    producer: &AssistantMessageEventStream,
    mut events: futures::stream::BoxStream<'static, Result<Value, String>>,
    scratch_args: &mut BTreeMap<usize, String>,
) -> Result<(), String> {
    let mut current_block: Option<MistralBlock> = None;
    let block_index = |output: &AssistantMessage| output.content.len().saturating_sub(1);
    let mut tool_blocks_by_key: BTreeMap<String, usize> = BTreeMap::new();

    let finish_current_block = |output: &AssistantMessage,
                                producer: &AssistantMessageEventStream,
                                block: Option<&MistralBlock>| {
        match block {
            Some(MistralBlock::Text(text)) => {
                producer.push(AssistantMessageEvent::TextEnd {
                    content_index: block_index(output),
                    content: text.text.clone(),
                    partial: output.clone(),
                });
            }
            Some(MistralBlock::Thinking(thinking)) => {
                producer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: block_index(output),
                    content: thinking.thinking.clone(),
                    partial: output.clone(),
                });
            }
            None => {}
        }
    };
    // TypeScript keeps one shared `currentBlock` object in `output.content`;
    // the Rust scratch copy is written back into the message after every
    // delta so the final content carries the accumulated text.
    let sync_current_block = |output: &mut AssistantMessage, block: Option<&MistralBlock>| {
        let Some(block) = block else {
            return;
        };
        let index = output.content.len().saturating_sub(1);
        match (block, output.content.get_mut(index)) {
            (MistralBlock::Text(text), Some(AssistantContent::Text(target))) => {
                target.text = text.text.clone();
            }
            (MistralBlock::Thinking(thinking), Some(AssistantContent::Thinking(target))) => {
                target.thinking = thinking.thinking.clone();
            }
            _ => {}
        }
    };

    while let Some(event) = futures::StreamExt::next(&mut events).await {
        let event = event?;
        // Mistral's streamed CompletionChunk carries an id field; keep the
        // first non-empty one.
        if output.response_id.is_none()
            && let Some(id) = event.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            output.response_id = Some(id.to_string());
        }

        if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached_prompt_tokens = get_mistral_cached_prompt_tokens(usage, prompt_tokens);
            let input = prompt_tokens.saturating_sub(cached_prompt_tokens);
            output.usage.input = input;
            output.usage.output = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            output.usage.cache_read = cached_prompt_tokens;
            output.usage.cache_write = 0;
            output.usage.total_tokens =
                usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(
                    input
                        + output.usage.output
                        + output.usage.cache_read
                        + output.usage.cache_write,
                );
            calculate_cost(model, &mut output.usage);
        }

        let Some(choice) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            continue;
        };

        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            output.raw_stop_reason = Some(finish_reason.to_string());
            let (stop_reason, error_message) = map_chat_stop_reason(Some(finish_reason));
            output.stop_reason = stop_reason;
            if let Some(error_message) = error_message {
                output.error_message = Some(error_message);
            }
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };

        if let Some(content) = delta.get("content").filter(|content| !content.is_null()) {
            let content_items: Vec<Value> = match content {
                Value::String(text) => vec![Value::String(text.clone())],
                Value::Array(items) => items.clone(),
                _ => Vec::new(),
            };
            for item in content_items {
                if let Some(text) = item.as_str() {
                    let text_delta = sanitize_surrogates(text);
                    let is_text = matches!(current_block, Some(MistralBlock::Text(_)));
                    if !is_text {
                        finish_current_block(output, producer, current_block.as_ref());
                        output
                            .content
                            .push(AssistantContent::Text(TextContent::default()));
                        producer.push(AssistantMessageEvent::TextStart {
                            content_index: block_index(output),
                            partial: output.clone(),
                        });
                        current_block = Some(MistralBlock::Text(TextContent::default()));
                    }
                    if let Some(MistralBlock::Text(block)) = &mut current_block {
                        block.text.push_str(&text_delta);
                    }
                    sync_current_block(output, current_block.as_ref());
                    producer.push(AssistantMessageEvent::TextDelta {
                        content_index: block_index(output),
                        delta: text_delta,
                        partial: output.clone(),
                    });
                    continue;
                }

                if item.get("type").and_then(Value::as_str) == Some("thinking") {
                    let delta_text = item
                        .get("thinking")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    let thinking_delta = sanitize_surrogates(&delta_text);
                    if thinking_delta.is_empty() {
                        continue;
                    }
                    let is_thinking = matches!(current_block, Some(MistralBlock::Thinking(_)));
                    if !is_thinking {
                        finish_current_block(output, producer, current_block.as_ref());
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent::default()));
                        producer.push(AssistantMessageEvent::ThinkingStart {
                            content_index: block_index(output),
                            partial: output.clone(),
                        });
                        current_block = Some(MistralBlock::Thinking(ThinkingContent::default()));
                    }
                    if let Some(MistralBlock::Thinking(block)) = &mut current_block {
                        block.thinking.push_str(&thinking_delta);
                    }
                    sync_current_block(output, current_block.as_ref());
                    producer.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: block_index(output),
                        delta: thinking_delta,
                        partial: output.clone(),
                    });
                    continue;
                }

                if item.get("type").and_then(Value::as_str) == Some("text") {
                    let text_delta = sanitize_surrogates(
                        item.get("text").and_then(Value::as_str).unwrap_or_default(),
                    );
                    let is_text = matches!(current_block, Some(MistralBlock::Text(_)));
                    if !is_text {
                        finish_current_block(output, producer, current_block.as_ref());
                        output
                            .content
                            .push(AssistantContent::Text(TextContent::default()));
                        producer.push(AssistantMessageEvent::TextStart {
                            content_index: block_index(output),
                            partial: output.clone(),
                        });
                        current_block = Some(MistralBlock::Text(TextContent::default()));
                    }
                    if let Some(MistralBlock::Text(block)) = &mut current_block {
                        block.text.push_str(&text_delta);
                    }
                    sync_current_block(output, current_block.as_ref());
                    producer.push(AssistantMessageEvent::TextDelta {
                        content_index: block_index(output),
                        delta: text_delta,
                        partial: output.clone(),
                    });
                }
            }
        }

        let tool_calls = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for tool_call in tool_calls {
            if current_block.is_some() {
                finish_current_block(output, producer, current_block.as_ref());
                current_block = None;
            }
            let provided_id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let call_id = if !provided_id.is_empty() && provided_id != "null" {
                provided_id.to_string()
            } else {
                derive_mistral_tool_call_id(
                    &format!(
                        "toolcall:{}",
                        tool_call.get("index").and_then(Value::as_u64).unwrap_or(0)
                    ),
                    0,
                )
            };
            let key = tool_call
                .get("index")
                .and_then(Value::as_u64)
                .map(|index| index.to_string())
                .unwrap_or_else(|| call_id.clone());

            let block_position = match tool_blocks_by_key.get(&key).copied() {
                Some(position) => position,
                None => {
                    let block = ToolCall {
                        content_type: Default::default(),
                        id: call_id.clone(),
                        name: tool_call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments: ToolCallArguments::new(),
                        ..Default::default()
                    };
                    output.content.push(AssistantContent::ToolCall(block));
                    let position = output.content.len() - 1;
                    tool_blocks_by_key.insert(key.clone(), position);
                    producer.push(AssistantMessageEvent::ToolcallStart {
                        content_index: position,
                        partial: output.clone(),
                    });
                    position
                }
            };

            let args_delta = match tool_call.pointer("/function/arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(other @ Value::Object(_)) => serde_json::to_string(other).unwrap_or_default(),
                _ => serde_json::to_string(&Value::Object(Map::new())).unwrap_or_default(),
            };
            // Append to the scratch partial args held alongside the block.
            let existing = scratch_args
                .get(&block_position)
                .cloned()
                .unwrap_or_default();
            let combined = format!("{existing}{args_delta}");
            scratch_args.insert(block_position, combined.clone());
            if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(block_position)
            {
                block.arguments = parse_streaming_json(Some(&combined))
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
            }
            producer.push(AssistantMessageEvent::ToolcallDelta {
                content_index: block_position,
                delta: args_delta,
                partial: output.clone(),
            });
        }
    }

    finish_current_block(output, producer, current_block.as_ref());
    for (_, position) in tool_blocks_by_key {
        let Some(AssistantContent::ToolCall(tool_call)) = output.content.get(position) else {
            continue;
        };
        let mut tool_call = tool_call.clone();
        let partial_args = scratch_args.get(&position).cloned().unwrap_or_default();
        tool_call.arguments = parse_streaming_json(Some(&partial_args))
            .as_object()
            .cloned()
            .unwrap_or_default();
        output.content[position] = AssistantContent::ToolCall(tool_call.clone());
        producer.push(AssistantMessageEvent::ToolcallEnd {
            content_index: position,
            tool_call,
            partial: output.clone(),
        });
    }
    Ok(())
}
