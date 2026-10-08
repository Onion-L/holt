//! Ports of the Mistral adapter suites (offline, via mock `HttpFetch`
//! transports standing in for the TS tests' `fetch` fakes):
//!
//! - `mistral-http-transport.test.ts` (8 cases)
//! - `mistral-raw-stop-reason.test.ts` (3 cases)
//! - `mistral-reasoning-mode.test.ts` (7 cases)
//! - `mistral-tool-schema.test.ts` (1 case)
//! - `fetch-option.test.ts` (the Mistral leg of
//!   "uses fetch for Mistral, Codex SSE, and pi-messages HTTP requests")

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::mistral_conversations::{MistralOptions, stream, stream_simple};
use pi_core::ai::compat;
use pi_core::ai::providers::builtin::get_builtin_model;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, CacheRetention, ConstrainedSamplingConfig,
    ConstrainedSamplingStrict, Context, ImageContent, Message, Model, ModelCost, ModelInput,
    OnPayloadCallback, OnResponseCallback, ProviderHeaders, ProviderRequestOptions,
    ProviderResponse, RoleToolResult, RoleUser, SimpleStreamOptions, StopReason, StreamOptions,
    TextContent, ThinkingContent, ThinkingLevel, Tool, ToolCall, ToolConstrainedSampling,
    ToolResultMessage, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// One scripted transport answer, mirroring the TS fixtures.
#[derive(Clone)]
enum ScriptedBody {
    /// `createSseResponse`: `data: <json>` frames joined with `\r\n\r\n` and a
    /// trailing `[DONE]` frame.
    SseFrames(Vec<Value>),
    /// A raw SSE body (the raw-stop-reason suite joins frames with `\n\n`).
    RawSse(String),
    /// `createBytewiseSseResponse`: a single event streamed one byte at a
    /// time so UTF-8 sequences split across transport chunks.
    BytewiseSse(Value),
    /// The never-yielding SSE body (`new ReadableStream({ start() {} })`).
    Hanging,
    /// A plain HTTP response (the error-body and fetch-option mocks).
    Raw { status: u16, body: String },
}

/// Mock transport standing in for the TS `FetchFunction` fakes: it records
/// every outgoing request and answers with the scripted body.
struct MistralFetch {
    body: ScriptedBody,
    response_headers: Vec<(String, String)>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl MistralFetch {
    fn with_body(body: ScriptedBody) -> Arc<Self> {
        Arc::new(Self {
            body,
            response_headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            requests: Mutex::new(Vec::new()),
        })
    }

    /// `createSseResponse(events)` with the default SSE content type.
    fn sse(events: Vec<Value>) -> Arc<Self> {
        Self::with_body(ScriptedBody::SseFrames(events))
    }

    /// `createSseResponse(events, headers)`: adds an extra response header.
    fn with_response_header(mut self: Arc<Self>, name: &str, value: &str) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("sole owner")
            .response_headers
            .push((name.to_string(), value.to_string()));
        self
    }

    fn error(status: u16, body: &str) -> Arc<Self> {
        Arc::new(Self {
            body: ScriptedBody::Raw {
                status,
                body: body.to_string(),
            },
            response_headers: Vec::new(),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn request(&self) -> HttpRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("request captured")
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn sse_frame_text(events: &[Value]) -> String {
    let mut body = events
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>();
    body.push_str("data: [DONE]\r\n\r\n");
    body
}

impl HttpFetch for MistralFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = self.body.clone();
        let headers = self.response_headers.clone();
        Box::pin(async move {
            match body {
                ScriptedBody::SseFrames(events) => Ok(HttpResponse {
                    status: 200,
                    headers,
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                        sse_frame_text(&events),
                    ))])),
                }),
                ScriptedBody::RawSse(text) => Ok(HttpResponse {
                    status: 200,
                    headers,
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(text))])),
                }),
                ScriptedBody::BytewiseSse(event) => {
                    let text = format!("data: {event}\r\n\r\ndata: [DONE]\r\n\r\n");
                    let bytes = bytes::Bytes::from(text);
                    let chunks = bytes
                        .iter()
                        .map(|byte| Ok(bytes::Bytes::copy_from_slice(&[*byte])))
                        .collect::<Vec<_>>();
                    Ok(HttpResponse {
                        status: 200,
                        headers,
                        body: Box::pin(futures::stream::iter(chunks)),
                    })
                }
                ScriptedBody::Hanging => Ok(HttpResponse {
                    status: 200,
                    headers,
                    body: Box::pin(futures::stream::pending()),
                }),
                ScriptedBody::Raw { status, body } => Ok(HttpResponse {
                    status,
                    headers,
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
                }),
            }
        })
    }
}

fn builtin(id: &str) -> Model {
    get_builtin_model("mistral", id).unwrap_or_else(|| panic!("missing mistral model {id}"))
}

fn user_message(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp,
    })
}

fn text_block(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.to_string(),
        ..Default::default()
    })
}

fn thinking_block(text: &str) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: text.to_string(),
        ..Default::default()
    })
}

fn tool_call_block(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.as_object().cloned().expect("object arguments"),
        ..Default::default()
    })
}

/// `createTerminalEvent(finishReason = "stop")`.
fn terminal_event(finish_reason: &str) -> Value {
    json!({
        "id": "mistral-response-id",
        "model": "mistral-large-latest",
        "choices": [{"index": 0, "finish_reason": finish_reason, "delta": {}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
}

fn mistral_options(fetch: Arc<MistralFetch>) -> MistralOptions {
    MistralOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test".to_string()),
                fetch: Some(fetch),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn body_of(request: &HttpRequest) -> Value {
    match &request.body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    }
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn headers_from(pairs: &[(&str, &str)]) -> ProviderHeaders {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), Some(value.to_string())))
        .collect()
}

fn payload_capture() -> (Arc<Mutex<Option<Value>>>, OnPayloadCallback) {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let on_payload: OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload.clone());
            Box::pin(std::future::ready(None))
        })
    };
    (captured, on_payload)
}

// ---------------------------------------------------------------------------
// mistral-http-transport.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serializes_sdk_style_payloads_to_the_mistral_wire_format() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        system_prompt: Some("Be precise".to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![
                BlockContent::Text(TextContent {
                    text: "describe".to_string(),
                    ..Default::default()
                }),
                BlockContent::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                    ..Default::default()
                }),
            ]),
            timestamp: 1,
        })],
        tools: Some(vec![Tool {
            name: "lookup".to_string(),
            description: "Look something up".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["query"],
                "properties": {"query": {"type": "string"}},
            }),
            constrained_sampling: None,
        }]),
    };
    let fetch = MistralFetch::with_body(ScriptedBody::SseFrames(vec![terminal_event("stop")]))
        // The TS `createSseResponse(..., { "x-request-id": "request-1" })`
        // extra response header.
        .with_response_header("x-request-id", "request-1");
    let captured_payload: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_response: Arc<Mutex<Option<ProviderResponse>>> = Arc::new(Mutex::new(None));
    let on_payload: OnPayloadCallback = {
        let captured = Arc::clone(&captured_payload);
        Arc::new(move |mut payload, _model| {
            *captured.lock().unwrap() = Some(payload.clone());
            payload["topP"] = json!(0.9);
            payload["randomSeed"] = json!(42);
            payload["responseFormat"] = json!({
                "type": "json_schema",
                "jsonSchema": {
                    "name": "result",
                    "schemaDefinition": {
                        "type": "object",
                        "properties": {"maxTokens": {"type": "number"}},
                    },
                },
            });
            payload["presencePenalty"] = json!(0.1);
            payload["frequencyPenalty"] = json!(0.2);
            payload["parallelToolCalls"] = json!(true);
            payload["safePrompt"] = json!(true);
            Box::pin(std::future::ready(Some(payload)))
        })
    };
    let on_response: OnResponseCallback = {
        let captured = Arc::clone(&captured_response);
        Arc::new(move |response, _model| {
            *captured.lock().unwrap() = Some(response.clone());
            Box::pin(async {})
        })
    };
    let options = MistralOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("secret".to_string()),
                fetch: Some(fetch.clone()),
                headers: Some(headers_from(&[("x-custom", "value")])),
                on_payload: Some(on_payload),
                on_response: Some(on_response),
                ..Default::default()
            },
            max_tokens: Some(123),
            session_id: Some("session-1".to_string()),
            ..Default::default()
        },
        tool_choice: Some(json!({"type": "function", "function": {"name": "lookup"}})),
        prompt_mode: Some("reasoning".to_string()),
        reasoning_effort: Some("high".to_string()),
    };

    let message = stream(&model, &context, Some(&options)).result().await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    let request = fetch.request();
    assert_eq!(request.url, "https://api.mistral.ai/v1/chat/completions");
    assert_eq!(header(&request, "authorization"), Some("Bearer secret"));
    assert_eq!(header(&request, "accept"), Some("text/event-stream"));
    assert_eq!(header(&request, "x-affinity"), Some("session-1"));
    assert_eq!(header(&request, "x-custom"), Some("value"));
    assert_eq!(
        header(&request, "user-agent"),
        Some(pi_core::ai::session_resources::get_pi_user_agent().as_str())
    );

    // The onPayload hook sees the SDK-style camelCase payload.
    let callback_payload = captured_payload
        .lock()
        .unwrap()
        .clone()
        .expect("payload captured");
    assert_eq!(callback_payload["maxTokens"], json!(123));
    assert_eq!(callback_payload["promptMode"], json!("reasoning"));
    assert_eq!(callback_payload["promptCacheKey"], json!("session-1"));
    assert_eq!(
        captured_response.lock().unwrap().clone(),
        Some(ProviderResponse {
            status: 200,
            headers: BTreeMap::from([
                ("content-type".to_string(), "text/event-stream".to_string()),
                ("x-request-id".to_string(), "request-1".to_string()),
            ]),
        })
    );

    // The request body carries the wire snake_case form.
    let wire_payload = body_of(&request);
    assert_eq!(wire_payload["max_tokens"], json!(123));
    assert_eq!(wire_payload["prompt_mode"], json!("reasoning"));
    assert_eq!(wire_payload["reasoning_effort"], json!("high"));
    assert_eq!(
        wire_payload["tool_choice"],
        json!({"type": "function", "function": {"name": "lookup"}})
    );
    assert_eq!(wire_payload["prompt_cache_key"], json!("session-1"));
    assert_eq!(wire_payload["top_p"], json!(0.9));
    assert_eq!(wire_payload["random_seed"], json!(42));
    assert_eq!(wire_payload["presence_penalty"], json!(0.1));
    assert_eq!(wire_payload["frequency_penalty"], json!(0.2));
    assert_eq!(wire_payload["parallel_tool_calls"], json!(true));
    assert_eq!(wire_payload["safe_prompt"], json!(true));
    assert_eq!(
        wire_payload["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "result",
                "schema": {
                    "type": "object",
                    "properties": {"maxTokens": {"type": "number"}},
                },
            },
        })
    );
    assert!(wire_payload.get("maxTokens").is_none());
    assert!(wire_payload.get("promptMode").is_none());
    assert!(wire_payload.get("promptCacheKey").is_none());
    assert_eq!(
        wire_payload["messages"],
        json!([
            {"role": "system", "content": "Be precise"},
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image_url", "image_url": "data:image/png;base64,aGVsbG8="},
                ],
            },
        ])
    );
}

#[tokio::test]
async fn serializes_assistant_thinking_tool_calls_and_tool_results_for_replay() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        messages: vec![
            Message::Assistant(Box::new(AssistantMessage {
                api: "mistral-conversations".to_string(),
                provider: "mistral".to_string(),
                model: model.id.clone(),
                content: vec![
                    thinking_block("reason"),
                    text_block("answer"),
                    tool_call_block("abc123456", "lookup", json!({"query": "pi"})),
                ],
                stop_reason: StopReason::ToolUse,
                timestamp: 1,
                ..Default::default()
            })),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: RoleToolResult,
                tool_call_id: "abc123456".to_string(),
                tool_name: "lookup".to_string(),
                content: vec![
                    BlockContent::Text(TextContent {
                        text: "found".to_string(),
                        ..Default::default()
                    }),
                    BlockContent::Image(ImageContent {
                        data: "aGVsbG8=".to_string(),
                        mime_type: "image/png".to_string(),
                        ..Default::default()
                    }),
                ],
                is_error: false,
                timestamp: 2,
                ..Default::default()
            })),
        ],
        ..Default::default()
    };
    let fetch = MistralFetch::sse(vec![terminal_event("stop")]);

    let message = stream(&model, &context, Some(&mistral_options(fetch.clone())))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    let wire_payload = body_of(&fetch.request());
    assert_eq!(
        wire_payload["messages"],
        json!([
            {
                "role": "assistant",
                "prefix": false,
                "content": [
                    {"type": "thinking", "thinking": [{"type": "text", "text": "reason"}]},
                    {"type": "text", "text": "answer"},
                ],
                "tool_calls": [
                    {
                        "id": "abc123456",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"query\":\"pi\"}"},
                        "index": 0,
                    },
                ],
            },
            {
                "role": "tool",
                "tool_call_id": "abc123456",
                "name": "lookup",
                "content": [
                    {"type": "text", "text": "found"},
                    {"type": "image_url", "image_url": "data:image/png;base64,aGVsbG8="},
                ],
            },
        ])
    );
}

#[tokio::test]
async fn parses_native_thinking_text_fragmented_tool_calls_and_cached_token_usage() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let events = vec![
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": null,
                "delta": {"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "reason"}]}]},
            }],
        }),
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": null,
                "delta": {"content": [{"type": "text", "text": "answer"}]},
            }],
        }),
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": null,
                "delta": {
                    "tool_calls": [{
                        "id": "abc123456",
                        "index": 0,
                        "function": {"name": "lookup", "arguments": "{\"query\":"},
                    }],
                },
            }],
        }),
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"name": "", "arguments": "\"pi\"}"},
                    }],
                },
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "total_tokens": 14,
                "prompt_tokens_details": {"cached_tokens": 3},
            },
        }),
    ];
    let fetch = MistralFetch::sse(events);

    let message = stream(&model, &context, Some(&mistral_options(fetch)))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(message.response_id.as_deref(), Some("response-1"));
    assert_eq!(
        message.content,
        vec![
            thinking_block("reason"),
            text_block("answer"),
            tool_call_block("abc123456", "lookup", json!({"query": "pi"})),
        ]
    );
    // `toMatchObject({ input: 7, output: 4, cacheRead: 3, cacheWrite: 0,
    // totalTokens: 14 })`: cached prompt tokens count as reads.
    assert_eq!(message.usage.input, 7);
    assert_eq!(message.usage.output, 4);
    assert_eq!(message.usage.cache_read, 3);
    assert_eq!(message.usage.cache_write, 0);
    assert_eq!(message.usage.total_tokens, 14);
}

#[tokio::test]
async fn parses_sse_and_utf8_sequences_split_across_transport_chunks() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let fetch = MistralFetch::with_body(ScriptedBody::BytewiseSse(json!({
        "id": "response-bytewise",
        "model": model.id,
        "choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "héllo 🌍"}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
    })));

    let message = stream(&model, &context, Some(&mistral_options(fetch)))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![text_block("héllo 🌍")]);
}

#[tokio::test]
async fn honors_case_insensitive_header_overrides_and_explicit_affinity_suppression() {
    let mut model = builtin("mistral-large-latest");
    model.headers = Some(BTreeMap::from([
        ("Authorization".to_string(), "Bearer model-key".to_string()),
        ("X-Affinity".to_string(), "model-affinity".to_string()),
    ]));
    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let fetch = MistralFetch::sse(vec![terminal_event("stop")]);
    let mut options = mistral_options(fetch.clone());
    options.base.base.api_key = Some("request-key".to_string());
    options.base.session_id = Some("automatic-affinity".to_string());
    options.base.base.headers = Some(BTreeMap::from([
        ("authorization".to_string(), None),
        ("x-affinity".to_string(), None),
        ("User-Agent".to_string(), Some("custom-agent".to_string())),
    ]));

    let _ = stream(&model, &context, Some(&options)).result().await;

    let request = fetch.request();
    assert!(header(&request, "authorization").is_none());
    assert!(header(&request, "x-affinity").is_none());
    assert_eq!(header(&request, "user-agent"), Some("custom-agent"));
}

#[tokio::test(start_paused = true)]
async fn aborts_while_waiting_for_an_sse_chunk() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let signal = CancellationToken::new();
    let mut options = mistral_options(MistralFetch::with_body(ScriptedBody::Hanging));
    options.base.base.signal = Some(signal.clone());

    let result = stream(&model, &context, Some(&options));
    signal.cancel();
    let message = result.result().await;

    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test(start_paused = true)]
async fn applies_the_request_timeout_while_waiting_for_an_sse_chunk() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let mut options = mistral_options(MistralFetch::with_body(ScriptedBody::Hanging));
    options.base.base.timeout_ms = Some(5);

    let message = stream(&model, &context, Some(&options)).result().await;

    assert_eq!(message.stop_reason, StopReason::Error);
    // The TS suite asserts `/timeout/i`; the oracle message is Node's
    // `AbortSignal.timeout` TimeoutError message.
    let error = message.error_message.expect("error message");
    assert!(error.to_lowercase().contains("timeout"), "error: {error}");
    assert_eq!(error, "The operation was aborted due to timeout");
}

#[tokio::test]
async fn preserves_http_status_and_response_bodies_in_errors() {
    let model = builtin("mistral-large-latest");
    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let fetch = MistralFetch::error(403, r#"{"message":"blocked by gateway"}"#);

    let message = stream(&model, &context, Some(&mistral_options(fetch)))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(r#"Mistral API error (403): {"message":"blocked by gateway"}"#)
    );
}

// ---------------------------------------------------------------------------
// mistral-raw-stop-reason.test.ts
// ---------------------------------------------------------------------------

fn raw_stop_reason_model() -> Model {
    builtin("devstral-medium-latest")
}

fn raw_stop_reason_context() -> Context {
    Context {
        messages: vec![user_message("hello", 0)],
        ..Default::default()
    }
}

/// The suite's `createFetch`: a single terminal chunk in an `\n\n` SSE body.
fn raw_stop_reason_fetch(finish_reason: &str) -> Arc<MistralFetch> {
    let event = json!({
        "id": "mistral-response-id",
        "model": raw_stop_reason_model().id,
        "choices": [{"index": 0, "finish_reason": finish_reason, "delta": {}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1},
    });
    MistralFetch::with_body(ScriptedBody::RawSse(format!(
        "data: {event}\n\ndata: [DONE]\n\n"
    )))
}

#[tokio::test]
async fn preserves_raw_mistral_finish_reasons_for_successful_stops() {
    let message = stream(
        &raw_stop_reason_model(),
        &raw_stop_reason_context(),
        Some(&mistral_options(raw_stop_reason_fetch("stop"))),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("stop"));
    assert!(message.error_message.is_none());
}

#[tokio::test]
async fn preserves_raw_mistral_finish_reasons_for_provider_error_stops() {
    let message = stream(
        &raw_stop_reason_model(),
        &raw_stop_reason_context(),
        Some(&mistral_options(raw_stop_reason_fetch("error"))),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("error"));
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: error")
    );
}

#[tokio::test]
async fn treats_unknown_mistral_finish_reasons_as_provider_error_stops() {
    let message = stream(
        &raw_stop_reason_model(),
        &raw_stop_reason_context(),
        Some(&mistral_options(raw_stop_reason_fetch("unmapped_error"))),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("unmapped_error"));
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: unmapped_error")
    );
}

// ---------------------------------------------------------------------------
// mistral-reasoning-mode.test.ts
// ---------------------------------------------------------------------------

/// The TS `capturePayload`: points the model at an unreachable base URL, runs
/// `streamSimple` with an `onPayload` capture, and returns the SDK-style
/// payload observed before the request fails. The port keeps the fixture but
/// answers from a failing mock transport so the test stays offline.
async fn capture_reasoning_payload(model: &Model, options: SimpleStreamOptions) -> Value {
    let mut model = model.clone();
    model.base_url = "http://127.0.0.1:9".to_string();
    let (captured, on_payload) = payload_capture();
    let fetch = MistralFetch::error(503, r#"{"message":"upstream offline"}"#);

    let mut options = options;
    let base = options.base.clone();
    options.base = StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some("fake-key".to_string()),
            fetch: Some(fetch),
            on_payload: Some(on_payload),
            ..base.base
        },
        ..base
    };
    let context = Context {
        messages: vec![user_message("Hello", 0)],
        ..Default::default()
    };
    let _ = stream_simple(&model, &context, Some(&options))
        .result()
        .await;

    captured
        .lock()
        .unwrap()
        .clone()
        .expect("Expected payload to be captured before request failure")
}

#[tokio::test]
async fn uses_reasoning_effort_for_mistral_small_4() {
    let payload = capture_reasoning_payload(
        &builtin("mistral-small-2603"),
        SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(payload["reasoningEffort"], json!("high"));
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn omits_reasoning_controls_for_mistral_small_4_when_thinking_is_off() {
    let payload = capture_reasoning_payload(
        &builtin("mistral-small-2603"),
        SimpleStreamOptions::default(),
    )
    .await;

    assert!(payload.get("reasoningEffort").is_none());
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn uses_prompt_mode_for_magistral_reasoning_models() {
    let payload = capture_reasoning_payload(
        &builtin("magistral-medium-latest"),
        SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(payload["promptMode"], json!("reasoning"));
    assert!(payload.get("reasoningEffort").is_none());
}

#[tokio::test]
async fn uses_reasoning_effort_for_mistral_medium_3_5() {
    let payload = capture_reasoning_payload(
        &builtin("mistral-medium-3.5"),
        SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(payload["reasoningEffort"], json!("high"));
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn omits_reasoning_controls_for_mistral_medium_3_5_when_thinking_is_off() {
    let payload = capture_reasoning_payload(
        &builtin("mistral-medium-3.5"),
        SimpleStreamOptions::default(),
    )
    .await;

    assert!(payload.get("reasoningEffort").is_none());
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn uses_the_session_id_as_prompt_cache_key() {
    let payload = capture_reasoning_payload(
        &builtin("mistral-large-latest"),
        SimpleStreamOptions {
            base: StreamOptions {
                session_id: Some("session-123".to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;

    assert_eq!(payload["promptCacheKey"], json!("session-123"));
}

#[tokio::test]
async fn omits_prompt_cache_key_when_cache_retention_is_disabled() {
    let payload = capture_reasoning_payload(
        &builtin("mistral-large-latest"),
        SimpleStreamOptions {
            base: StreamOptions {
                session_id: Some("session-123".to_string()),
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;

    assert!(payload.get("promptCacheKey").is_none());
}

// ---------------------------------------------------------------------------
// mistral-tool-schema.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strips_internal_schema_keys_before_the_sdk_validates_tool_schemas() {
    let mut model = builtin("devstral-medium-latest");
    model.base_url = "http://127.0.0.1:9".to_string();
    let context = Context {
        messages: vec![user_message("Hi", 0)],
        tools: Some(vec![Tool {
            name: "inspect_schema".to_string(),
            description: "Inspect the schema".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["nested"],
                "properties": {
                    "nested": {
                        "type": "object",
                        "required": ["value"],
                        "properties": {"value": {"type": "string"}},
                    },
                },
            }),
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::JsonSchema {
                    strict: ConstrainedSamplingStrict::Require,
                },
            )),
        }]),
        ..Default::default()
    };
    let (captured, on_payload) = payload_capture();
    // The TS suite lets the request to 127.0.0.1:9 fail; the port answers
    // from a failing mock transport instead.
    let fetch = MistralFetch::error(502, r#"{"message":"upstream offline"}"#);

    let response = compat::complete(
        &model,
        &context,
        Some(StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("fake-key".to_string()),
                fetch: Some(fetch),
                on_payload: Some(on_payload),
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .await;

    let payload = captured.lock().unwrap().clone().expect("payload captured");
    let tools = payload["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["strict"], json!(true));
    // The TS suite asserts no TypeBox symbol keys survive on the parameters,
    // properties, and nested schemas; the Rust equivalent is an exact
    // comparison against the strict JSON-schema form (any leaked internal
    // key would break equality).
    assert_eq!(
        tools[0]["function"]["parameters"],
        json!({
            "type": "object",
            "required": ["nested"],
            "properties": {
                "nested": {
                    "type": "object",
                    "required": ["value"],
                    "properties": {"value": {"type": "string"}},
                    "additionalProperties": false,
                },
            },
            "additionalProperties": false,
        })
    );
    assert_eq!(response.stop_reason, StopReason::Error);
    let error = response.error_message.expect("error message");
    assert!(!error.contains("Input validation failed"), "error: {error}");
}

// ---------------------------------------------------------------------------
// fetch-option.test.ts (Mistral leg)
// ---------------------------------------------------------------------------

/// The suite's `createModel("mistral-conversations")`.
fn fetch_option_model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "mistral-conversations".to_string(),
        provider: "test-provider".to_string(),
        base_url: "https://upstream.test/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 10_000,
        max_tokens: 1_000,
        ..Default::default()
    }
}

#[tokio::test]
async fn passes_fetch_through_stream_simple_to_the_mistral_adapter() {
    // The TS suite stubs globalThis.fetch with a throwing fallback and
    // asserts only the injected `fetch` runs; Rust has no ambient fetch to
    // stub, so the ported observable is the injected transport receiving the
    // request (the `custom` mock returns the suite's 401 rejection).
    let custom = MistralFetch::error(401, r#"{"error":{"message":"upstream rejected request"}}"#);

    let context = Context {
        messages: vec![user_message("hello", 1)],
        ..Default::default()
    };
    let _ = stream_simple(
        &fetch_option_model(),
        &context,
        Some(&SimpleStreamOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("test-key".to_string()),
                    fetch: Some(custom.clone()),
                    max_retries: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(custom.request_count(), 1);
    assert_eq!(
        custom.request().url,
        "https://upstream.test/v1/v1/chat/completions"
    );
}
