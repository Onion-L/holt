//! Port of `pi-core/ai/test/openai-codex-stream.test.ts` (SSE and WebSocket
//! transports) and the Codex payload case from
//! `pi-core/ai/test/max-thinking.test.ts`.
//!
//! The TypeScript suite mocks `globalThis.fetch` and `WebSocket`; the Rust
//! port drives the same observable surface through a mock `HttpFetch`
//! transport and the injectable WebSocket factory. The WebSocket tests
//! serialize on a shared lock because they own the global factory, session
//! cache, debug stats, and clock (vitest runs each file's tests
//! sequentially over its own stubbed globals).
//!
//! Harness differences (behavior-preserving):
//!
//! - The TS timeout tests freeze `vi.useFakeTimers` and advance timers; the
//!   Rust port uses real 50ms windows for the connect/idle timeouts.
//! - The TS age-limit test freezes `Date.now` with `vi.setSystemTime`; the
//!   Rust port overrides the WebSocket cache clock.
//! - The TS max-thinking test throws from `onPayload` to skip the request;
//!   the Rust `onPayload` cannot fail, so the mock answers a completed SSE
//!   stream instead (with `transport: "sse"` pinned).
//! - The TS retry tests freeze `Date.now()` and spy `setTimeout`; the Rust
//!   port uses paused tokio time for the deterministic headers and a tight
//!   wall-clock window for the HTTP-date case.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use pi_core::ai::api::openai_codex_responses::{
    OpenAICodexResponsesOptions, stream as stream_codex, stream_simple as stream_simple_codex,
};
use pi_core::ai::types::{
    AssistantMessage, AssistantMessageEvent, BlockContent, CacheRetention,
    ConstrainedSamplingConfig, ConstrainedSamplingStrict, Context, JsF64, Message, Model,
    ModelCost, ModelCostRates, ModelInput, ProviderRequestOptions, RoleUser, SimpleStreamOptions,
    StopReason, StreamOptions, ThinkingLevel, Tool, ToolConstrainedSampling, Transport,
    UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Port of `mockToken`: a JWT-shaped token carrying the ChatGPT account id.
fn mock_token() -> String {
    let payload = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acc_test"}});
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_string(&payload).unwrap());
    format!("aaa.{encoded}.bbb")
}

fn codex_model(id: &str, name: &str) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: "openai-codex-responses".to_string(),
        provider: "openai-codex".to_string(),
        base_url: "https://chatgpt.com/backend-api".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        ..Default::default()
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: 1,
    })
}

fn context(prompt: &str, message: &str) -> Context {
    Context {
        system_prompt: Some(prompt.to_string()),
        messages: vec![user_message(message)],
        ..Default::default()
    }
}

fn sse_from_events(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn output_item_added() -> Value {
    json!({
        "type": "response.output_item.added",
        "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] },
    })
}

fn content_part_added() -> Value {
    json!({"type": "response.content_part.added", "part": {"type": "output_text", "text": ""}})
}

fn output_text_delta(delta: &str) -> Value {
    json!({"type": "response.output_text.delta", "delta": delta})
}

fn output_item_done(text: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text}],
        },
    })
}

/// Port of `buildSSEPayload`: the standard five-event Codex fixture.
fn build_sse_payload(status: &str, include_done: bool, end_turn: Option<bool>) -> String {
    let terminal_type = if status == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    let mut terminal = Map::new();
    terminal.insert("status".to_string(), json!(status));
    if let Some(end_turn) = end_turn {
        terminal.insert("end_turn".to_string(), json!(end_turn));
    }
    terminal.insert(
        "incomplete_details".to_string(),
        if status == "incomplete" {
            json!({"reason": "max_output_tokens"})
        } else {
            Value::Null
        },
    );
    terminal.insert(
        "usage".to_string(),
        json!({
            "input_tokens": 5,
            "output_tokens": 3,
            "total_tokens": 8,
            "input_tokens_details": {"cached_tokens": 0},
        }),
    );

    let mut events = vec![
        output_item_added(),
        content_part_added(),
        output_text_delta("Hello"),
        output_item_done("Hello"),
        json!({"type": terminal_type, "response": terminal}),
    ];
    if include_done {
        events.push(json!("[DONE]"));
    }
    sse_from_events(&events)
}

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

enum ScriptedResponse {
    /// `200` SSE response whose full body arrives at once and closes.
    Sse(String),
    /// `200` SSE response whose body emits `chunk` once and stays open.
    SseOpen(String),
    /// `200` SSE response delivering chunks after delays; the flag is set
    /// when the body is dropped (the TS `ReadableStream.cancel()` callback).
    SseTimed(Vec<(u64, String)>, Option<Arc<AtomicBool>>),
    /// JSON error response with headers.
    Error(u16, String, Vec<(String, String)>),
    /// Response headers never arrive (the fetch promise never settles).
    Pending,
}

/// Mock transport standing in for the TS `vi.stubGlobal("fetch", ...)`.
struct CodexFetch {
    queue: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl CodexFetch {
    fn new(responses: Vec<ScriptedResponse>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request(&self) -> HttpRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("request captured")
    }
}

impl HttpFetch for CodexFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let next = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("no scripted response left");
        Box::pin(async move {
            match next {
                ScriptedResponse::Sse(body) => Ok(sse_response(futures::stream::iter(vec![Ok(
                    Bytes::from(body),
                )]))),
                ScriptedResponse::SseOpen(chunk) => Ok(sse_response(
                    futures::stream::once(async move { Ok(Bytes::from(chunk)) })
                        .chain(futures::stream::pending()),
                )),
                ScriptedResponse::SseTimed(chunks, dropped) => {
                    let body = futures::stream::unfold(chunks.into_iter(), |mut rest| async move {
                        let (delay, chunk) = rest.next()?;
                        if delay > 0 {
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                        }
                        Some((Ok(Bytes::from(chunk)), rest))
                    });
                    Ok(sse_response(DropFlagStream {
                        inner: Box::pin(body),
                        dropped,
                    }))
                }
                ScriptedResponse::Error(status, body, headers) => Ok(HttpResponse {
                    status,
                    headers,
                    body: Box::pin(futures::stream::iter(vec![Ok(Bytes::from(body))])),
                }),
                ScriptedResponse::Pending => {
                    std::future::pending::<Result<HttpResponse, HttpFetchError>>().await
                }
            }
        })
    }
}

fn sse_response(
    body: impl futures::Stream<Item = Result<Bytes, HttpFetchError>> + Send + 'static,
) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(body),
    }
}

/// A body stream that reports cancellation through a flag on drop, mirroring
/// the TS `ReadableStream.cancel()` callback.
struct DropFlagStream {
    inner: Pin<Box<dyn futures::Stream<Item = Result<Bytes, HttpFetchError>> + Send>>,
    dropped: Option<Arc<AtomicBool>>,
}

impl futures::Stream for DropFlagStream {
    type Item = Result<Bytes, HttpFetchError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for DropFlagStream {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.store(true, Ordering::SeqCst);
        }
    }
}

fn base_options(fetch: Arc<CodexFetch>) -> StreamOptions {
    StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some(mock_token()),
            fetch: Some(fetch),
            ..Default::default()
        },
        transport: Some(Transport::Sse),
        ..Default::default()
    }
}

fn codex_options(fetch: Arc<CodexFetch>) -> OpenAICodexResponsesOptions {
    OpenAICodexResponsesOptions {
        base: base_options(fetch),
        ..Default::default()
    }
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Port of `decodeCodexRequestBody`: string bodies parse directly, byte
/// bodies are zstd-compressed (the SSE path always compresses).
fn body_of(request: &HttpRequest) -> Value {
    match &request.body {
        HttpBody::Json(body) => body.clone(),
        HttpBody::Bytes(bytes) => {
            let decompressed = zstd::bulk::decompress(bytes, 16 * 1024 * 1024)
                .expect("zstd-compressed request body");
            serde_json::from_slice(&decompressed).expect("valid JSON in compressed body")
        }
        other => panic!("expected JSON body, got {other:?}"),
    }
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .find_map(|block| match block {
            pi_core::ai::types::AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn event_type(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

// ---------------------------------------------------------------------------
// openai-codex-stream.test.ts (SSE transport)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streams_sse_responses_into_assistant_message_event_stream() {
    // The TS test's inline SSE fixture: no end_turn / incomplete_details.
    let sse = sse_from_events(&[
        output_item_added(),
        content_part_added(),
        output_text_delta("Hello"),
        output_item_done("Hello"),
        json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {
                    "input_tokens": 5,
                    "output_tokens": 3,
                    "total_tokens": 8,
                    "input_tokens_details": {"cached_tokens": 0},
                },
            },
        }),
    ]);
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(sse)]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");

    let stream = stream_codex(&model, &context, Some(&codex_options(fetch.clone())));
    let mut saw_text_delta = false;
    let mut saw_done = false;
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::TextDelta { .. } => saw_text_delta = true,
            AssistantMessageEvent::Done { message, .. } => {
                saw_done = true;
                assert_eq!(text_of(message), "Hello");
            }
            _ => {}
        }
    }

    assert!(saw_text_delta);
    assert!(saw_done);

    let request = fetch.request();
    assert_eq!(
        request.url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    let token = mock_token();
    assert_eq!(
        header(&request, "Authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(header(&request, "chatgpt-account-id"), Some("acc_test"));
    assert_eq!(
        header(&request, "OpenAI-Beta"),
        Some("responses=experimental")
    );
    assert_eq!(header(&request, "originator"), Some("pi"));
    // TS: `pi (${platform()} ${release()}; ${arch})` from node:os; the Rust
    // port builds the same shape from the Rust target.
    assert_eq!(
        header(&request, "User-Agent"),
        Some(pi_core::ai::session_resources::get_pi_user_agent().as_str())
    );
    assert_eq!(header(&request, "accept"), Some("text/event-stream"));
    assert!(header(&request, "x-api-key").is_none());
}

#[tokio::test]
async fn completes_after_response_completed_even_when_the_sse_body_stays_open() {
    let sse = build_sse_payload("completed", true, Some(false));
    let fetch = CodexFetch::new(vec![ScriptedResponse::SseOpen(sse)]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");

    // The TS test races the result against a 1s timeout.
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        stream_codex(&model, &context, Some(&codex_options(fetch))).result(),
    )
    .await
    .expect("Timed out waiting for completed SSE stream");

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.end_turn, Some(false));
}

#[tokio::test]
async fn maps_response_incomplete_to_stop_reason_length_even_when_the_body_stays_open() {
    let sse = build_sse_payload("incomplete", false, None);
    let fetch = CodexFetch::new(vec![ScriptedResponse::SseOpen(sse)]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        stream_codex(&model, &context, Some(&codex_options(fetch))).result(),
    )
    .await
    .expect("Timed out waiting for incomplete SSE stream");

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(result.stop_reason, StopReason::Length);
}

#[tokio::test]
async fn aborts_sse_fetch_after_the_configured_http_timeout_when_response_headers_do_not_arrive() {
    // Headers never arrive: the fetch promise never settles.
    let fetch = CodexFetch::new(vec![ScriptedResponse::Pending]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch.clone());
    options.base.base.timeout_ms = Some(10);

    let result = stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(fetch.request_count(), 1);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Codex SSE response headers timed out after 10ms")
    );
}

#[tokio::test]
async fn aborts_sse_body_reads_after_response_headers_arrive() {
    let part_one = sse_from_events(&[
        output_item_added(),
        content_part_added(),
        output_text_delta("one"),
    ]);
    let part_two = sse_from_events(&[output_text_delta("two")]);
    let part_three = sse_from_events(&[
        output_item_done("onetwo"),
        json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {
                    "input_tokens": 5,
                    "output_tokens": 3,
                    "total_tokens": 8,
                    "input_tokens_details": {"cached_tokens": 0},
                },
            },
        }),
    ]);
    let dropped = Arc::new(AtomicBool::new(false));
    let fetch = CodexFetch::new(vec![ScriptedResponse::SseTimed(
        vec![(0, part_one), (10, part_two), (20, part_three)],
        Some(Arc::clone(&dropped)),
    )]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let controller = CancellationToken::new();
    let mut options = codex_options(fetch);
    options.base.base.signal = Some(controller.clone());

    let stream = stream_codex(&model, &context, Some(&options));
    let mut events: Vec<String> = Vec::new();
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                events.push(format!("text_delta:{delta}"));
                if delta == "one" {
                    controller.cancel();
                }
            }
            other => events.push(event_type(other).to_string()),
        }
    }

    let result = stream.result().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request was aborted"));
    assert!(events.contains(&"text_delta:one".to_string()));
    assert!(!events.contains(&"text_delta:two".to_string()));
    assert!(dropped.load(Ordering::SeqCst), "SSE body must be cancelled");
}

#[tokio::test]
async fn sets_session_id_x_client_request_id_headers_and_prompt_cache_key_when_session_id_is_provided()
 {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let session_id = "test-session-123";
    let mut options = codex_options(fetch.clone());
    options.base.session_id = Some(session_id.to_string());

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    let request = fetch.request();
    assert_eq!(header(&request, "session-id"), Some(session_id));
    assert!(header(&request, "session_id").is_none());
    assert_eq!(header(&request, "x-client-request-id"), Some(session_id));
    assert_eq!(
        body_of(&request).get("prompt_cache_key"),
        Some(&json!(session_id))
    );
}

#[tokio::test]
async fn omits_sse_cache_affinity_when_cache_retention_is_none() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch.clone());
    options.base.cache_retention = Some(CacheRetention::None);
    options.base.session_id = Some("one-off-summary".to_string());

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    let request = fetch.request();
    assert!(header(&request, "session-id").is_none());
    assert!(header(&request, "x-client-request-id").is_none());
    assert!(body_of(&request).get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn clamps_prompt_cache_key_to_openais_64_character_limit() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch);
    options.base.session_id = Some("x".repeat(67));
    let capture = Arc::clone(&captured);
    options.base.base.on_payload = Some(Arc::new(
        move |payload: Value, _model: &Model| -> BoxFuture<'static, Option<Value>> {
            let capture = Arc::clone(&capture);
            Box::pin(async move {
                *capture.lock().unwrap() = Some(payload);
                None
            })
        },
    ));

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    let payload = captured.lock().unwrap().clone().expect("payload captured");
    assert_eq!(
        payload.get("prompt_cache_key"),
        Some(&json!("x".repeat(64)))
    );
}

#[tokio::test]
async fn clamps_codex_session_id_header_to_64_characters() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch.clone());
    options.base.session_id = Some("x".repeat(67));

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    let request = fetch.request();
    assert_eq!(
        header(&request, "session-id"),
        Some("x".repeat(64).as_str())
    );
    assert_eq!(
        header(&request, "x-client-request-id"),
        Some("x".repeat(64).as_str())
    );
}

#[tokio::test]
async fn preserves_gpt_5_5_xhigh_reasoning_effort_from_simple_options() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);

    let mut model = codex_model("gpt-5.5", "GPT-5.5");
    model.thinking_level_map = Some(
        [(
            pi_core::ai::types::ModelThinkingLevel::Xhigh,
            Some("xhigh".to_string()),
        )]
        .into_iter()
        .collect(),
    );
    let context = context("You are a helpful assistant.", "Say hello");

    let options = SimpleStreamOptions {
        base: base_options(fetch.clone()),
        reasoning: Some(ThinkingLevel::Xhigh),
        ..Default::default()
    };
    stream_simple_codex(&model, &context, Some(&options))
        .result()
        .await;

    // The TS fetch mock captures `body.reasoning` from the request.
    let reasoning = body_of(&fetch.request()).get("reasoning").cloned();
    assert_eq!(
        reasoning,
        Some(json!({"effort": "xhigh", "summary": "auto"}))
    );
}

#[tokio::test]
async fn forwards_required_tool_choice() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);

    let model = codex_model("gpt-5.5", "GPT-5.5");
    let context = Context {
        messages: vec![user_message("Do not call ping. Respond with text instead.")],
        tools: Some(vec![Tool {
            name: "ping".to_string(),
            description: "Ping".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
            }),
            constrained_sampling: None,
        }]),
        ..Default::default()
    };
    let mut options = codex_options(fetch.clone());
    options.tool_choice = Some("required".to_string());

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(
        body_of(&fetch.request()).get("tool_choice"),
        Some(&json!("required"))
    );
}

#[tokio::test]
async fn sets_codex_strict_mode_explicitly_and_honors_constrained_sampling() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));

    let model = codex_model("gpt-5.5", "GPT-5.5");
    let context = Context {
        messages: vec![user_message("Use a tool")],
        tools: Some(vec![
            Tool {
                name: "optional".to_string(),
                description: "Optional constrained sampling".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"],
                }),
                constrained_sampling: Some(ToolConstrainedSampling::Disabled(false)),
            },
            Tool {
                name: "strict".to_string(),
                description: "Strict constrained sampling".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"],
                    "additionalProperties": false,
                }),
                constrained_sampling: Some(ToolConstrainedSampling::Config(
                    ConstrainedSamplingConfig::JsonSchema {
                        strict: ConstrainedSamplingStrict::Prefer,
                    },
                )),
            },
        ]),
        ..Default::default()
    };
    let mut options = codex_options(fetch);
    let capture = Arc::clone(&captured);
    options.base.base.on_payload = Some(Arc::new(
        move |payload: Value, _model: &Model| -> BoxFuture<'static, Option<Value>> {
            let capture = Arc::clone(&capture);
            Box::pin(async move {
                *capture.lock().unwrap() = Some(payload);
                None
            })
        },
    ));

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    let payload = captured.lock().unwrap().clone().expect("payload captured");
    let tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["type"], json!("function"));
    assert_eq!(tools[0]["name"], json!("optional"));
    assert_eq!(tools[0]["strict"], json!(null));
    assert_eq!(tools[1]["type"], json!("function"));
    assert_eq!(tools[1]["name"], json!("strict"));
    assert_eq!(tools[1]["strict"], json!(true));
}

/// it.each(["gpt-5.3-codex", "gpt-5.4", "gpt-5.5"]) — looped per the repo's
/// convention for it.each ports.
#[tokio::test]
async fn clamps_minimal_reasoning_effort_to_low() {
    for model_id in ["gpt-5.3-codex", "gpt-5.4", "gpt-5.5"] {
        let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
            "completed",
            false,
            None,
        ))]);

        let mut model = codex_model(model_id, model_id);
        model.thinking_level_map = Some(
            [(
                pi_core::ai::types::ModelThinkingLevel::Minimal,
                Some("low".to_string()),
            )]
            .into_iter()
            .collect(),
        );
        let context = context("You are a helpful assistant.", "Say hello");
        let mut options = codex_options(fetch.clone());
        options.reasoning_effort = Some("minimal".to_string());

        stream_codex(&model, &context, Some(&options))
            .result()
            .await;

        assert_eq!(
            body_of(&fetch.request()).get("reasoning"),
            Some(&json!({"effort": "low", "summary": "auto"})),
            "model {model_id}"
        );
    }
}

/// it.each over (model, tier, multiplier) — the service-tier fixture echoes
/// `service_tier: "default"` and expects the client-sent tier's pricing.
#[tokio::test]
async fn uses_the_client_sent_service_tier_when_codex_echoes_default() {
    let cases: &[(&str, &str, f64)] = &[
        ("gpt-5.1-codex", "flex", 0.5),
        ("gpt-5.1-codex", "priority", 2.0),
        ("gpt-5.5", "flex", 0.5),
        ("gpt-5.5", "priority", 2.5),
    ];

    for (model_id, service_tier, multiplier) in cases {
        let sse = sse_from_events(&[
            output_item_added(),
            content_part_added(),
            output_text_delta("Hello"),
            output_item_done("Hello"),
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "service_tier": "default",
                    "usage": {
                        "input_tokens": 1000000,
                        "output_tokens": 1000000,
                        "total_tokens": 2000000,
                        "input_tokens_details": {"cached_tokens": 0},
                    },
                },
            }),
        ]);
        let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(sse)]);

        let mut model = codex_model(model_id, "GPT-5.1 Codex");
        if *model_id == "gpt-5.5" {
            model.name = "GPT-5.5".to_string();
        }
        model.cost = ModelCost {
            rates: ModelCostRates {
                input: JsF64(1.0),
                output: JsF64(2.0),
                cache_read: JsF64(0.0),
                cache_write: JsF64(0.0),
            },
            tiers: None,
        };
        let context = context("You are a helpful assistant.", "Say hello");
        let mut options = codex_options(fetch);
        options.service_tier = Some(service_tier.to_string());

        let result = stream_codex(&model, &context, Some(&options))
            .result()
            .await;

        assert_eq!(
            result.usage.cost.input.0,
            1.0 * multiplier,
            "{model_id}/{service_tier}"
        );
        assert_eq!(
            result.usage.cost.output.0,
            2.0 * multiplier,
            "{model_id}/{service_tier}"
        );
        assert_eq!(
            result.usage.cost.total.0,
            3.0 * multiplier,
            "{model_id}/{service_tier}"
        );
    }
}

#[tokio::test]
async fn does_not_set_session_id_x_client_request_id_headers_when_session_id_is_not_provided() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");

    stream_codex(&model, &context, Some(&codex_options(fetch.clone())))
        .result()
        .await;

    let request = fetch.request();
    assert!(header(&request, "session-id").is_none());
    assert!(header(&request, "session_id").is_none());
    assert!(header(&request, "x-client-request-id").is_none());
}

/// Shared body of the it.each "uses %s for SSE retries" cases with a
/// deterministic Retry-After value: the retry must not fire before the delay
/// and the stream must complete right after it (the TS test asserts
/// `setTimeout(fn, expectedDelay)` under fake timers).
async fn assert_sse_retry_delay(expected_delay_ms: u64, retry_headers: Vec<(String, String)>) {
    let fetch = CodexFetch::new(vec![
        ScriptedResponse::Error(
            429,
            json!({"error": {"code": "rate_limit_exceeded", "message": "rate limited"}})
                .to_string(),
            retry_headers,
        ),
        ScriptedResponse::Sse(build_sse_payload("completed", false, None)),
    ]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch.clone());
    options.base.base.max_retries = Some(1);

    let stream = stream_codex(&model, &context, Some(&options));
    let mut result = std::pin::pin!(stream.result());
    tokio::time::sleep(Duration::from_millis(expected_delay_ms - 1)).await;
    assert_eq!(
        fetch.request_count(),
        1,
        "retry must not fire before {expected_delay_ms}ms"
    );
    let result = tokio::time::timeout(Duration::from_millis(2), result.as_mut())
        .await
        .expect("retry must fire at the Retry-After delay");

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(fetch.request_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn uses_retry_after_ms_for_sse_retries() {
    assert_sse_retry_delay(
        1500,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("retry-after-ms".to_string(), "1500".to_string()),
        ],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn uses_retry_after_seconds_for_sse_retries() {
    assert_sse_retry_delay(
        60_000,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("retry-after".to_string(), "60".to_string()),
        ],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn uses_retry_after_http_date_for_sse_retries() {
    // The TS test freezes Date.now() and sends now + 45s; the Rust port reads
    // the wall clock when parsing the HTTP date, so the observed delay is
    // 45s minus the test's own overhead — assert a tight window around it.
    let retry_at = std::time::SystemTime::now() + Duration::from_secs(45);
    let retry_after = httpdate::fmt_http_date(retry_at);

    let fetch = CodexFetch::new(vec![
        ScriptedResponse::Error(
            429,
            json!({"error": {"code": "rate_limit_exceeded", "message": "rate limited"}})
                .to_string(),
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("retry-after".to_string(), retry_after),
            ],
        ),
        ScriptedResponse::Sse(build_sse_payload("completed", false, None)),
    ]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch.clone());
    options.base.base.max_retries = Some(1);

    let stream = stream_codex(&model, &context, Some(&options));
    let mut result = std::pin::pin!(stream.result());
    tokio::time::sleep(Duration::from_millis(45_000 - 1_000)).await;
    assert_eq!(
        fetch.request_count(),
        1,
        "retry must not fire before the HTTP-date delay"
    );
    let result = tokio::time::timeout(Duration::from_millis(3_000), result.as_mut())
        .await
        .expect("retry must fire at the HTTP-date delay");

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(fetch.request_count(), 2);
}

/// it.each([429, 503]) — looped per the repo's convention for it.each ports.
#[tokio::test]
async fn fails_immediately_when_a_retry_delay_exceeds_the_limit() {
    for status in [429u16, 503] {
        let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
            status,
            json!({"error": {"code": "temporarily_unavailable", "message": "retry later"}})
                .to_string(),
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("retry-after".to_string(), "2".to_string()),
            ],
        )]);

        let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
        let context = context("You are a helpful assistant.", "Say hello");
        let mut options = codex_options(fetch.clone());
        options.base.base.max_retries = Some(3);
        options.base.base.max_retry_delay_ms = Some(1000);

        let result = stream_codex(&model, &context, Some(&options))
            .result()
            .await;

        assert_eq!(result.stop_reason, StopReason::Error, "status {status}");
        assert_eq!(
            result.error_message.as_deref(),
            Some("Server requested 2s retry delay (max: 1s)"),
            "status {status}"
        );
        assert_eq!(fetch.request_count(), 1, "status {status}");
    }
}

#[tokio::test(start_paused = true)]
async fn uses_exponential_backoff_across_repeated_sse_retries_without_retry_headers() {
    let rate_limited = || {
        ScriptedResponse::Error(
            429,
            json!({"error": {"code": "rate_limit_exceeded", "message": "rate limited"}})
                .to_string(),
            vec![("content-type".to_string(), "application/json".to_string())],
        )
    };
    let fetch = CodexFetch::new(vec![
        rate_limited(),
        rate_limited(),
        rate_limited(),
        ScriptedResponse::Sse(build_sse_payload("completed", false, None)),
    ]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = codex_options(fetch.clone());
    options.base.base.max_retries = Some(3);

    let stream = stream_codex(&model, &context, Some(&options));
    let mut result = std::pin::pin!(stream.result());

    // The TS test observes setTimeout delays of 1000, 2000, and 4000ms; pin
    // each delay from both sides of virtual time.
    tokio::time::sleep(Duration::from_millis(999)).await;
    assert_eq!(fetch.request_count(), 1, "first delay must be 1000ms");
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(fetch.request_count(), 2, "first retry fired after 1000ms");

    tokio::time::sleep(Duration::from_millis(1998)).await;
    assert_eq!(fetch.request_count(), 2, "second delay must be 2000ms");
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(fetch.request_count(), 3, "second retry fired after 2000ms");

    tokio::time::sleep(Duration::from_millis(3998)).await;
    assert_eq!(fetch.request_count(), 3, "third delay must be 4000ms");
    let result = tokio::time::timeout(Duration::from_millis(2), result.as_mut())
        .await
        .expect("third retry fired after 4000ms");

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(fetch.request_count(), 4);
}

// ---------------------------------------------------------------------------
// max-thinking.test.ts — "sends max to the Codex Responses API"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sends_max_to_the_codex_responses_api() {
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));

    let model = pi_core::ai::providers::builtin::get_builtin_model("openai-codex", "gpt-5.6-sol")
        .expect("gpt-5.6-sol model");
    let context = context("You are a helpful assistant.", "Hello");

    let capture = Arc::clone(&captured);
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(mock_token()),
                fetch: Some(fetch),
                on_payload: Some(Arc::new(
                    move |payload: Value, _model: &Model| -> BoxFuture<'static, Option<Value>> {
                        let capture = Arc::clone(&capture);
                        Box::pin(async move {
                            *capture.lock().unwrap() = Some(payload);
                            None
                        })
                    },
                )),
                ..Default::default()
            },
            transport: Some(Transport::Sse),
            ..Default::default()
        },
        reasoning: Some(ThinkingLevel::Max),
        ..Default::default()
    };
    stream_simple_codex(&model, &context, Some(&options))
        .result()
        .await;

    let payload = captured.lock().unwrap().clone().expect("payload captured");
    assert_eq!(
        payload.get("reasoning"),
        Some(&json!({"effort": "max", "summary": "auto"}))
    );
}

// ---------------------------------------------------------------------------
// openai-codex-stream.test.ts (websocket transport and zstd compression)
// ---------------------------------------------------------------------------

/// Port of `mockToken(accountId)`.
fn mock_token_for(account_id: &str) -> String {
    let payload = json!({"https://api.openai.com/auth": {"chatgpt_account_id": account_id}});
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_string(&payload).unwrap());
    format!("aaa.{encoded}.bbb")
}

/// Serializes the websocket tests: they share the global factory, session
/// cache, debug stats, and clock the way the TypeScript suite stubs globals
/// per test under vitest's sequential file execution.
async fn ws_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

async fn ws_test_cleanup() {
    pi_core::ai::api::openai_codex_responses::set_websocket_factory_for_tests(None);
    pi_core::ai::api::openai_codex_responses::set_websocket_clock_for_tests(None);
    pi_core::ai::api::openai_codex_responses::close_openai_codex_websocket_sessions(None);
    pi_core::ai::api::openai_codex_responses::reset_openai_codex_websocket_debug_stats(None);
}

/// The send-time context handed to a scripted socket's response behavior.
struct SendContext {
    connection: usize,
    /// The 1-based global send index (the TS `sentBodies.length`).
    send_index: usize,
    frame: Value,
    queue: pi_core::ai::api::openai_codex_websocket::WebSocketEventQueue,
}

type MockSendBehavior = Box<dyn Fn(&SendContext) + Send>;

/// Shared bookkeeping across every mock socket a test creates.
#[derive(Default)]
struct MockWebSocketShared {
    connections: AtomicUsize,
    closed: AtomicUsize,
    /// Whether constructors push the `open` event (the TS mocks that stay
    /// unopened exercise the connect timeout).
    opens_on_connect: AtomicBool,
    sent_frames: Mutex<Vec<Value>>,
    sent_connections: Mutex<Vec<usize>>,
    connection_headers: Mutex<Vec<Vec<(String, String)>>>,
    sends_before_open: AtomicUsize,
    on_send: Mutex<Option<MockSendBehavior>>,
}

impl MockWebSocketShared {
    fn install(self: &Arc<Self>) {
        let shared = Arc::clone(self);
        pi_core::ai::api::openai_codex_responses::set_websocket_factory_for_tests(Some(Arc::new(
            move |_url, headers| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    let connection = shared.connections.fetch_add(1, Ordering::SeqCst) + 1;
                    shared.connection_headers.lock().unwrap().push(headers);
                    let socket = Arc::new(MockWebSocket {
                        connection,
                        queue:
                            pi_core::ai::api::openai_codex_websocket::WebSocketEventQueue::default(),
                        reusable: AtomicBool::new(true),
                        shared: Arc::clone(&shared),
                    });
                    if shared.opens_on_connect.load(Ordering::SeqCst) {
                        socket
                            .queue
                            .push(pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Open);
                    }
                    Ok(socket as Arc<dyn pi_core::ai::api::openai_codex_websocket::WebSocketLike>)
                })
            },
        )));
    }

    fn sent_frames(&self) -> Vec<Value> {
        self.sent_frames.lock().unwrap().clone()
    }

    fn set_on_send<F>(&self, on_send: F)
    where
        F: Fn(&SendContext) + Send + 'static,
    {
        *self.on_send.lock().unwrap() = Some(Box::new(on_send));
    }
}

/// The mock socket standing in for the TS `MockWebSocket` classes.
struct MockWebSocket {
    connection: usize,
    queue: pi_core::ai::api::openai_codex_websocket::WebSocketEventQueue,
    reusable: AtomicBool,
    shared: Arc<MockWebSocketShared>,
}

impl pi_core::ai::api::openai_codex_websocket::WebSocketLike for MockWebSocket {
    fn send_text(&self, data: &str) {
        if !self.reusable.load(Ordering::SeqCst) {
            self.shared.sends_before_open.fetch_add(1, Ordering::SeqCst);
        }
        let frame: Value = serde_json::from_str(data).expect("mock frame is JSON");
        self.shared.sent_frames.lock().unwrap().push(frame.clone());
        self.shared
            .sent_connections
            .lock()
            .unwrap()
            .push(self.connection);
        let context = SendContext {
            connection: self.connection,
            send_index: self.shared.sent_frames.lock().unwrap().len(),
            frame,
            queue: self.queue.clone(),
        };
        if let Some(on_send) = self.shared.on_send.lock().unwrap().as_ref() {
            on_send(&context);
        }
    }

    fn close_silently(&self, _code: u16, _reason: &str) {
        self.shared.closed.fetch_add(1, Ordering::SeqCst);
        self.reusable.store(false, Ordering::SeqCst);
    }

    fn is_reusable(&self) -> bool {
        self.reusable.load(Ordering::SeqCst)
    }

    fn next_event(
        &self,
    ) -> BoxFuture<'static, Option<pi_core::ai::api::openai_codex_websocket::WebSocketEvent>> {
        let queue = self.queue.clone();
        Box::pin(async move { queue.next().await })
    }
}

fn completed_event(response_id: &str) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "status": "completed",
            "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8},
        },
    })
}

fn ws_options(
    fetch: Arc<CodexFetch>,
    session_id: Option<&str>,
    transport: Transport,
) -> OpenAICodexResponsesOptions {
    OpenAICodexResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(mock_token()),
                fetch: Some(fetch),
                ..Default::default()
            },
            transport: Some(transport),
            session_id: session_id.map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// "forwards auto transport from streamSimple options and uses cached
/// websocket context"
#[tokio::test]
async fn forwards_auto_transport_and_uses_cached_websocket_context() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    shared.set_on_send(|context| {
        for event in [
            output_item_added(),
            content_part_added(),
            output_text_delta("Hello"),
            output_item_done("Hello"),
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "end_turn": false,
                    "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8,
                              "input_tokens_details": {"cached_tokens": 0}},
                },
            }),
        ] {
            context.queue.push(
                pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                    event.to_string(),
                ),
            );
        }
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);
    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");

    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(mock_token()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            transport: Some(Transport::Auto),
            session_id: Some("session-auto".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let result = stream_simple_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(result.end_turn, Some(false));
    assert_eq!(shared.sent_frames().len(), 1);
    let headers = shared.connection_headers.lock().unwrap()[0].clone();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("session-id").as_deref(), Some("session-auto"));
    assert!(header("session_id").is_none());
    assert_eq!(
        header("x-client-request-id").as_deref(),
        Some("session-auto")
    );
    assert_eq!(fetch.request_count(), 0);
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        "session-auto",
    )
    .expect("stats recorded");
    assert_eq!(stats.cached_context_requests, 1);
    assert_eq!(stats.full_context_requests, 1);
    ws_test_cleanup().await;
}

/// "scopes cached websockets to the authenticated account" (regression for
/// upstream #7284: rotating accounts must not reuse a socket authenticated
/// by another account).
#[tokio::test]
async fn scopes_cached_websockets_to_the_authenticated_account() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    let response_counter = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&response_counter);
    shared.set_on_send(move |context| {
        let id = counter.fetch_add(1, Ordering::SeqCst) + 1;
        context.queue.push(
            pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": format!("resp_{id}"),
                        "status": "completed",
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                    },
                })
                .to_string(),
            ),
        );
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);
    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = Context {
        system_prompt: Some(String::new()),
        messages: Vec::new(),
        ..Default::default()
    };
    let base = |account: &str| OpenAICodexResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(mock_token_for(account)),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            transport: Some(Transport::WebsocketCached),
            session_id: Some("shared-session".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };

    stream_codex(&model, &context, Some(&base("account-a")))
        .result()
        .await;
    stream_codex(&model, &context, Some(&base("account-b")))
        .result()
        .await;
    stream_codex(&model, &context, Some(&base("account-a")))
        .result()
        .await;

    let headers = shared.connection_headers.lock().unwrap().clone();
    let account_ids: Vec<Option<&str>> = headers
        .iter()
        .map(|headers| {
            headers
                .iter()
                .find(|(key, _)| key == "chatgpt-account-id")
                .map(|(_, value)| value.as_str())
        })
        .collect();
    assert_eq!(account_ids, vec![Some("account-a"), Some("account-b")]);
    let authorizations: Vec<String> = headers
        .iter()
        .map(|headers| {
            headers
                .iter()
                .find(|(key, _)| key == "authorization")
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        })
        .collect();
    let token_a = mock_token_for("account-a");
    let token_b = mock_token_for("account-b");
    assert_eq!(
        authorizations,
        vec![format!("Bearer {token_a}"), format!("Bearer {token_b}"),]
    );
    assert_eq!(fetch.request_count(), 0);
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        "shared-session",
    )
    .expect("stats recorded");
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 1);
    ws_test_cleanup().await;
}

/// "closes one-shot websockets when cacheRetention is none"
#[tokio::test]
async fn closes_one_shot_websockets_when_cache_retention_is_none() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    shared.set_on_send(|context| {
        context.queue.push(
            pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                completed_event(&format!("resp_{}", context.connection)).to_string(),
            ),
        );
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);
    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let options = OpenAICodexResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(mock_token()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            transport: Some(Transport::Auto),
            session_id: Some("one-off-summary".to_string()),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
        ..Default::default()
    };

    stream_codex(&model, &context, Some(&options))
        .result()
        .await;
    stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(shared.connections.load(Ordering::SeqCst), 2);
    assert_eq!(shared.closed.load(Ordering::SeqCst), 2);
    let frames = shared.sent_frames();
    assert_eq!(frames.len(), 2);
    assert!(
        frames
            .iter()
            .all(|frame| frame.get("prompt_cache_key").is_none())
    );
    assert!(
        pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
            "one-off-summary"
        )
        .is_none()
    );
    assert_eq!(fetch.request_count(), 0);
    ws_test_cleanup().await;
}

/// "falls back to SSE when websocket connect does not open before the
/// connect timeout"
#[tokio::test]
async fn falls_back_to_sse_when_websocket_connect_times_out() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    // opens_on_connect stays false: the socket never opens.
    shared.install();

    let sse = build_sse_payload("completed", false, None);
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(sse)]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = ws_options(fetch.clone(), Some("ws-connect-timeout"), Transport::Auto);
    options.base.base.timeout_ms = Some(300_000);
    options.base.websocket_connect_timeout_ms = Some(50);

    let result = stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(fetch.request_count(), 1);
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        "ws-connect-timeout",
    )
    .expect("stats recorded");
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 1);
    assert_eq!(stats.websocket_fallback_active, Some(true));
    assert_eq!(
        stats.last_websocket_error.as_deref(),
        Some("WebSocket connect timeout after 50ms")
    );
    assert_eq!(shared.sends_before_open.load(Ordering::SeqCst), 0);
    ws_test_cleanup().await;
}

/// "reconnects once when the websocket connection limit is reached before
/// output starts"
#[tokio::test]
async fn reconnects_once_when_the_connection_limit_is_reached() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    shared.set_on_send(|context| {
        let event = if context.connection == 1 {
            json!({"type": "error", "error": {"code": "websocket_connection_limit_reached"}})
        } else {
            completed_event("resp_1")
        };
        context.queue.push(
            pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(event.to_string()),
        );
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);
    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");

    let result = stream_codex(
        &model,
        &Context {
            system_prompt: Some(String::new()),
            messages: Vec::new(),
            ..Default::default()
        },
        Some(&ws_options(fetch.clone(), None, Transport::Auto)),
    )
    .result()
    .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(shared.connections.load(Ordering::SeqCst), 2);
    assert_eq!(fetch.request_count(), 0);
    ws_test_cleanup().await;
}

/// "falls back to SSE when a websocket is idle before the first event"
#[tokio::test]
async fn falls_back_to_sse_when_idle_before_the_first_event() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    // send records the frame but never answers.
    shared.install();

    let sse = build_sse_payload("completed", false, None);
    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(sse)]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = ws_options(fetch.clone(), Some("ws-idle-before-start"), Transport::Auto);
    options.base.base.timeout_ms = Some(50);

    let result = stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(text_of(&result), "Hello");
    assert_eq!(shared.sent_frames().len(), 1);
    assert_eq!(fetch.request_count(), 1);
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        "ws-idle-before-start",
    )
    .expect("stats recorded");
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 1);
    assert_eq!(stats.websocket_fallback_active, Some(true));
    ws_test_cleanup().await;
}

/// "errors when a websocket is idle after the stream started"
#[tokio::test]
async fn errors_when_a_websocket_is_idle_after_the_stream_started() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    shared.set_on_send(|context| {
        context.queue.push(
            pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                output_item_added().to_string(),
            ),
        );
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let context = context("You are a helpful assistant.", "Say hello");
    let mut options = ws_options(fetch.clone(), None, Transport::Auto);
    options.base.base.timeout_ms = Some(50);

    let result = stream_codex(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("WebSocket idle timeout after 50ms")
    );
    assert_eq!(fetch.request_count(), 0);
    ws_test_cleanup().await;
}

/// "opens a fresh cached websocket before the backend connection age limit"
#[tokio::test]
async fn opens_a_fresh_cached_websocket_before_the_age_limit() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    shared.set_on_send(|context| {
        context.queue.push(
            pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                completed_event(&format!("resp_{}", context.connection)).to_string(),
            ),
        );
    });
    shared.install();

    // The TS test freezes and advances system time (`vi.setSystemTime`).
    let started_at: i64 = 1_782_000_000_000; // 2026-07-03T00:00:00Z in ms
    let clock_ms = Arc::new(std::sync::atomic::AtomicI64::new(started_at));
    let ws_clock = {
        let clock_ms = Arc::clone(&clock_ms);
        Arc::new(move || clock_ms.load(Ordering::SeqCst)) as Arc<dyn Fn() -> i64 + Send + Sync>
    };
    pi_core::ai::api::openai_codex_responses::set_websocket_clock_for_tests(Some(ws_clock));

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);
    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let first_context = context("You are a helpful assistant.", "Say hello");
    let session_id = "aged-ws-session";

    let first = stream_codex(
        &model,
        &first_context,
        Some(&ws_options(
            fetch.clone(),
            Some(session_id),
            Transport::WebsocketCached,
        )),
    )
    .result()
    .await;
    clock_ms.store(started_at + 56 * 60 * 1000, Ordering::SeqCst);

    let second_context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![
            user_message("Say hello"),
            Message::Assistant(Box::new(first)),
            user_message("Now finish"),
        ],
        ..Default::default()
    };
    stream_codex(
        &model,
        &second_context,
        Some(&ws_options(
            fetch.clone(),
            Some(session_id),
            Transport::WebsocketCached,
        )),
    )
    .result()
    .await;

    assert_eq!(shared.connections.load(Ordering::SeqCst), 2);
    assert_eq!(shared.sent_connections.lock().unwrap().clone(), vec![1, 2]);
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        session_id,
    )
    .expect("stats recorded");
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 0);
    ws_test_cleanup().await;
}

/// "sends only response input deltas in websocket-cached mode"
#[tokio::test]
async fn sends_only_response_input_deltas_in_websocket_cached_mode() {
    let _guard = ws_test_lock().await;
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    shared.set_on_send(|context| {
        let mut events: Vec<Value> = Vec::new();
        if context.send_index == 1 {
            events.extend([
                json!({
                    "type": "response.output_item.added",
                    "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
                             "name": "sample_tool", "input": ""},
                }),
                json!({"type": "response.custom_tool_call_input.delta", "item_id": "ctc_1", "delta": "abc"}),
                json!({"type": "response.custom_tool_call_input.done", "item_id": "ctc_1", "input": "abc"}),
                json!({
                    "type": "response.output_item.done",
                    "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
                             "name": "sample_tool", "input": "abc"},
                }),
            ]);
        }
        events.push(json!({"type": "response.created", "response": {"id": format!("resp_{}", context.send_index)}}));
        events.push(completed_event(&format!("resp_{}", context.send_index)));
        for event in events {
            context.queue.push(
                pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                    event.to_string(),
                ),
            );
        }
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Error(
        500,
        "unexpected fetch".to_string(),
        Vec::new(),
    )]);
    let mut model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    model.compat = Some(pi_core::ai::types::ModelCompat {
        supports_open_ai_grammar_tools: Some(true),
        ..Default::default()
    });
    let first_context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message("Use the tool")],
        tools: Some(vec![Tool {
            name: "sample_tool".to_string(),
            description: "Sample tool".to_string(),
            parameters: json!({"type": "object", "properties": {"payload": {"type": "string"}}, "required": ["payload"]}),
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::Grammar {
                    variants: [(
                        pi_core::ai::types::GrammarFormat::OpenaiLark,
                        "start: /[a-z]+/".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                },
            )),
        }]),
    };

    let first = stream_codex(
        &model,
        &first_context,
        Some(&ws_options(
            fetch.clone(),
            Some("session-1"),
            Transport::WebsocketCached,
        )),
    )
    .result()
    .await;

    let second_context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![
            user_message("Use the tool"),
            Message::Assistant(Box::new(first)),
            Message::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
                role: pi_core::ai::types::RoleToolResult,
                tool_call_id: "call_1|ctc_1".to_string(),
                tool_name: "sample_tool".to_string(),
                content: vec![BlockContent::Text(pi_core::ai::types::TextContent {
                    text: "real result".to_string(),
                    ..Default::default()
                })],
                is_error: false,
                timestamp: 2,
                ..Default::default()
            })),
            user_message("Now finish"),
        ],
        tools: first_context.tools.clone(),
    };
    stream_codex(
        &model,
        &second_context,
        Some(&ws_options(
            fetch.clone(),
            Some("session-1"),
            Transport::WebsocketCached,
        )),
    )
    .result()
    .await;

    let frames = shared.sent_frames();
    assert_eq!(frames.len(), 2);
    let first_body = &frames[0];
    let second_body = &frames[1];
    assert_eq!(first_body.get("store"), Some(&json!(false)));
    assert!(first_body.get("previous_response_id").is_none());
    assert_eq!(
        first_body.get("input"),
        Some(
            &json!([{"role": "user", "content": [{"type": "input_text", "text": "Use the tool"}]}])
        )
    );
    assert_eq!(second_body.get("store"), Some(&json!(false)));
    assert_eq!(
        second_body.get("previous_response_id"),
        Some(&json!("resp_1"))
    );
    assert_eq!(
        second_body.get("input"),
        Some(&json!([
            {"type": "custom_tool_call_output", "call_id": "call_1", "output": "real result"},
            {"role": "user", "content": [{"type": "input_text", "text": "Now finish"}]},
        ]))
    );
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        "session-1",
    )
    .expect("stats recorded");
    assert_eq!(stats.requests, 2);
    assert_eq!(stats.connections_created, 1);
    assert_eq!(stats.connections_reused, 1);
    assert_eq!(stats.cached_context_requests, 2);
    assert_eq!(stats.store_true_requests, 0);
    assert_eq!(stats.full_context_requests, 1);
    assert_eq!(stats.delta_requests, 1);
    assert_eq!(stats.last_delta_input_items, Some(2));
    assert_eq!(stats.last_previous_response_id.as_deref(), Some("resp_1"));
    ws_test_cleanup().await;
}

/// "recovers a missing cached websocket continuation via websocket"/"…via
/// sse" (the TS `it.each` pair).
async fn recover_missing_cached_websocket_continuation(recovery_transport: &'static str) {
    let _guard = ws_test_lock().await;
    let session_id = format!("missing-continuation-{recovery_transport}");
    let shared = Arc::new(MockWebSocketShared::default());
    shared.opens_on_connect.store(true, Ordering::SeqCst);
    let shared_for_send = Arc::clone(&shared);
    shared.set_on_send(move |context| {
        let shared = Arc::clone(&shared_for_send);
        let frame = context.frame.clone();
        let queue = context.queue.clone();
        let send_index = context.send_index;
        tokio::spawn(async move {
            if send_index == 2 {
                for event in [
                    json!({
                        "type": "codex.rate_limits",
                        "plan_type": "plus",
                        "rate_limits": {
                            "allowed": true,
                            "limit_reached": false,
                            "primary": {
                                "used_percent": 7,
                                "window_minutes": 10080,
                                "reset_after_seconds": 556112,
                                "reset_at": 1785269351,
                            },
                            "secondary": None::<Value>,
                        },
                        "code_review_rate_limits": None::<Value>,
                        "additional_rate_limits": None::<Value>,
                        "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
                        "promo": None::<Value>,
                    }),
                    json!({
                        "type": "error",
                        "status": 400,
                        "error": {
                            "code": "previous_response_not_found",
                            "message": "Previous response with id 'resp_1' not found.",
                            "param": "previous_response_id",
                        },
                    }),
                ] {
                    queue.push(
                        pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                            event.to_string(),
                        ),
                    );
                }
                let _ = shared;
                return;
            }
            if send_index == 3 && recovery_transport == "sse" {
                queue.push(
                    pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Error(
                        "retry websocket failed".to_string(),
                    ),
                );
                return;
            }

            let (response_id, message_id, text) = if send_index == 1 {
                ("resp_1", "msg_1", "Hello")
            } else {
                ("resp_2", "msg_2", "Recovered")
            };
            for event in [
                json!({"type": "response.created", "response": {"id": response_id}}),
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message", "id": message_id, "role": "assistant",
                             "status": "in_progress", "content": []},
                }),
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {"type": "message", "id": message_id, "role": "assistant",
                             "status": "completed",
                             "content": [{"type": "output_text", "text": text}]},
                }),
                completed_event(response_id),
            ] {
                queue.push(
                    pi_core::ai::api::openai_codex_websocket::WebSocketEvent::Message(
                        event.to_string(),
                    ),
                );
            }
            let _ = frame;
        });
    });
    shared.install();

    let fetch = CodexFetch::new(vec![ScriptedResponse::Sse(build_sse_payload(
        "completed",
        false,
        None,
    ))]);
    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let first_context = context("You are a helpful assistant.", "Say hello");

    let first = stream_codex(
        &model,
        &first_context,
        Some(&ws_options(
            fetch.clone(),
            Some(&session_id),
            Transport::WebsocketCached,
        )),
    )
    .result()
    .await;
    let second_context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![
            user_message("Say hello"),
            Message::Assistant(Box::new(first)),
            user_message("Now finish"),
        ],
        ..Default::default()
    };
    let mut event_types: Vec<&'static str> = Vec::new();
    let second_stream = stream_codex(
        &model,
        &second_context,
        Some(&ws_options(
            fetch.clone(),
            Some(&session_id),
            Transport::WebsocketCached,
        )),
    );
    let second_stream = second_stream;
    while let Some(event) = second_stream.next().await {
        event_types.push(event_type(&event));
    }
    let second = second_stream.result().await;

    assert_eq!(second.stop_reason, StopReason::Stop);
    assert_eq!(
        text_of(&second),
        if recovery_transport == "sse" {
            "Hello"
        } else {
            "Recovered"
        }
    );
    assert_eq!(
        event_types.iter().filter(|kind| **kind == "start").count(),
        1
    );
    assert!(!event_types.contains(&"error"));
    assert_eq!(shared.connections.load(Ordering::SeqCst), 2);
    let frames = shared.sent_frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(
        shared.sent_connections.lock().unwrap().clone(),
        vec![1, 1, 2]
    );
    assert_eq!(
        frames[1].get("previous_response_id"),
        Some(&json!("resp_1"))
    );
    assert_eq!(
        frames[1].get("input"),
        Some(&json!([{"role": "user", "content": [{"type": "input_text", "text": "Now finish"}]}]))
    );
    assert!(frames[2].get("previous_response_id").is_none());
    let third_input = frames[2]
        .get("input")
        .and_then(Value::as_array)
        .expect("input");
    assert_eq!(third_input.len(), 3);
    assert_eq!(
        third_input.last(),
        Some(&json!({"role": "user", "content": [{"type": "input_text", "text": "Now finish"}]}))
    );
    assert_eq!(
        fetch.request_count(),
        if recovery_transport == "sse" { 1 } else { 0 }
    );
    let stats = pi_core::ai::api::openai_codex_responses::get_openai_codex_websocket_debug_stats(
        &session_id,
    )
    .expect("stats recorded");
    assert_eq!(stats.requests, 3);
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 1);
    assert_eq!(stats.full_context_requests, 2);
    assert_eq!(stats.delta_requests, 1);
    assert_eq!(
        stats.websocket_failures,
        if recovery_transport == "sse" { 1 } else { 0 }
    );
    assert_eq!(
        stats.sse_fallbacks,
        if recovery_transport == "sse" { 1 } else { 0 }
    );
    ws_test_cleanup().await;
}

#[tokio::test]
async fn recovers_a_missing_cached_websocket_continuation_via_websocket() {
    recover_missing_cached_websocket_continuation("websocket").await;
}

#[tokio::test]
async fn recovers_a_missing_cached_websocket_continuation_via_sse() {
    recover_missing_cached_websocket_continuation("sse").await;
}

/// "zstd-compresses SSE request bodies"
#[tokio::test]
async fn zstd_compresses_sse_request_bodies() {
    let sse = build_sse_payload("completed", false, None);
    let fetch = CodexFetch::new(vec![
        ScriptedResponse::Sse(sse.clone()),
        ScriptedResponse::Sse(sse),
    ]);

    let model = codex_model("gpt-5.1-codex", "GPT-5.1 Codex");
    let large_text = "compress me ".repeat(400);
    stream_codex(
        &model,
        &Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text(large_text.clone()),
                timestamp: 1,
            })],
            ..Default::default()
        },
        Some(&ws_options(fetch.clone(), None, Transport::Sse)),
    )
    .result()
    .await;

    let request = fetch.request();
    assert_eq!(header(&request, "content-encoding"), Some("zstd"));
    let decoded = body_of(&request);
    assert_eq!(
        decoded.pointer("/input/0/content/0/text"),
        Some(&json!(large_text))
    );

    stream_codex(
        &model,
        &context("You are a helpful assistant.", "hi"),
        Some(&ws_options(fetch.clone(), None, Transport::Sse)),
    )
    .result()
    .await;

    let request = fetch.request();
    assert_eq!(header(&request, "content-encoding"), Some("zstd"));
    assert!(matches!(request.body, HttpBody::Bytes(_)));
}
