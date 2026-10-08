//! Ports of the openai-completions payload/options construction suites
//! against a canned SSE transport:
//!
//! - `openai-completions-raw-stop-reason.test.ts`
//! - `openai-completions-prompt-cache.test.ts`
//! - `openai-completions-empty-tools.test.ts`
//! - `openai-completions-cache-control-format.test.ts`
//! - `openai-completions-tool-choice.test.ts` (payload/options cases only)
//! - `openai-completions-thinking-token-budget.test.ts`
//! - `sampling-options.test.ts`
//! - `openrouter-reasoning-options.test.ts` (the 3 streamSimple payload
//!   cases; the getOpenRouterThinkingLevelMap codegen-script cases are N/A)
//! - `provider-error-body-regression.test.ts` (the 2 openai-completions
//!   cases)
//! - `cache-retention.test.ts` (the OpenAI Completions describe)
//!
//! The TypeScript suites capture payloads through the mocked `openai` SDK
//! client (or the `onPayload` hook); the Rust port captures the outgoing JSON
//! body and headers through a mock `HttpFetch` transport, which is the same
//! observable surface.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::anthropic_messages::stream_simple as stream_simple_anthropic;
use pi_core::ai::api::openai_completions::{OpenAICompletionsOptions, stream, stream_simple};
use pi_core::ai::compat::stream_simple as compat_stream_simple;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, CacheControlFormat, CacheRetention, ChatTemplateKwargValue,
    ChatTemplateKwargs, Context, MaxTokensField, Message, Model, ModelCompat, ModelInput,
    ProviderEnv, ProviderHeaders, ProviderRequestOptions, SessionAffinityFormat,
    SimpleStreamOptions, StopReason, StreamOptions, ThinkingBudgets, ThinkingFormat, ThinkingLevel,
    ThinkingTokenBudgetField, ThinkingVariable, Tool, ToolCall, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

fn model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        cost: pi_core::ai::types::ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        ..Default::default()
    }
}

fn context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

fn chunk_events(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

/// The chunk yielded by every TS SDK mock.
fn ok_chunk() -> Value {
    json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 0},
        },
    })
}

/// Mock transport returning canned SSE data and capturing requests.
struct RecordingFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl RecordingFetch {
    fn new(body: String) -> Self {
        Self {
            body,
            requests: Mutex::new(Vec::new()),
        }
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

impl HttpFetch for RecordingFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

/// Mock transport answering with an HTTP error body (the TS suite's throwing
/// `withResponse` path).
struct ErroringFetch {
    status: u16,
    body: String,
}

impl HttpFetch for ErroringFetch {
    fn fetch<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let status = self.status;
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn options_with(fetch: Arc<RecordingFetch>, session_id: Option<&str>) -> OpenAICompletionsOptions {
    OpenAICompletionsOptions {
        base: pi_core::ai::types::StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("test".to_string()),
                fetch: Some(fetch),
                ..Default::default()
            },
            session_id: session_id.map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Shared porting helpers
// ---------------------------------------------------------------------------

fn builtin(provider: &str, id: &str) -> Model {
    pi_core::ai::providers::builtin::get_builtin_model(provider, id)
        .unwrap_or_else(|| panic!("missing model {provider}/{id}"))
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
        role: pi_core::ai::types::RoleUser,
    })
}

fn ok_fetch() -> Arc<RecordingFetch> {
    Arc::new(RecordingFetch::new(chunk_events(&[ok_chunk()])))
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

fn env_from(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn headers_from(pairs: &[(&str, &str)]) -> ProviderHeaders {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), Some(value.to_string())))
        .collect()
}

fn chat_template_kwargs(pairs: &[(&str, ChatTemplateKwargValue)]) -> ChatTemplateKwargs {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

fn direct_options() -> OpenAICompletionsOptions {
    OpenAICompletionsOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test".to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn simple_test_options() -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test".to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Runs `stream` with a mock transport and returns the captured request.
async fn capture_direct(
    model: &Model,
    context: &Context,
    mut options: OpenAICompletionsOptions,
) -> HttpRequest {
    let fetch = ok_fetch();
    options.base.base.fetch = Some(fetch.clone());
    let _ = stream(model, context, Some(&options)).result().await;
    fetch.request()
}

async fn capture_direct_body(
    model: &Model,
    context: &Context,
    options: OpenAICompletionsOptions,
) -> Value {
    body_of(&capture_direct(model, context, options).await)
}

/// Runs `streamSimple` with a mock transport and returns the captured
/// request.
async fn capture_simple(
    model: &Model,
    context: &Context,
    mut options: SimpleStreamOptions,
) -> HttpRequest {
    let fetch = ok_fetch();
    options.base.base.fetch = Some(fetch.clone());
    let _ = stream_simple(model, context, Some(&options)).result().await;
    fetch.request()
}

async fn capture_simple_body(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> Value {
    body_of(&capture_simple(model, context, options).await)
}

/// The Cloudflare suites dispatch through the global `streamSimple`, which
/// resolves provider auth and the gateway URL placeholders before the
/// openai-completions adapter runs.
async fn capture_compat_simple(
    model: &Model,
    context: &Context,
    mut options: SimpleStreamOptions,
) -> HttpRequest {
    let fetch = ok_fetch();
    options.base.base.fetch = Some(fetch.clone());
    let _ = compat_stream_simple(model, context, Some(&options))
        .result()
        .await;
    fetch.request()
}

fn read_tool() -> Tool {
    Tool {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        }),
        constrained_sampling: None,
    }
}

fn ping_tool() -> Tool {
    Tool {
        name: "ping".to_string(),
        description: "Ping tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
        }),
        constrained_sampling: None,
    }
}

/// The `localOpenAICompletionsModel` fixture from the TS suites.
fn local_completions_model(id: &str, name: &str, provider: &str, base_url: &str) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: "openai-completions".to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 8_192,
        ..Default::default()
    }
}

#[tokio::test]
async fn preserves_raw_finish_reasons_for_successful_stops() {
    let chunks = vec![
        serde_json::json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ];
    let fetch = Arc::new(RecordingFetch::new(chunk_events(&chunks)));

    let message = stream(&model(), &context(), Some(&options_with(fetch, None)))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("stop"));
    assert!(message.error_message.is_none());
}

#[tokio::test]
async fn preserves_raw_finish_reasons_for_provider_error_stops() {
    let chunks = vec![
        serde_json::json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {}, "finish_reason": "content_filter"}]}),
    ];
    let fetch = Arc::new(RecordingFetch::new(chunk_events(&chunks)));

    let message = stream(&model(), &context(), Some(&options_with(fetch, None)))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("content_filter"));
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider finish_reason: content_filter")
    );
}

#[tokio::test]
async fn sends_prompt_cache_key_for_openai_with_session() {
    let chunks = vec![serde_json::json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 0},
        },
    })];
    let fetch = Arc::new(RecordingFetch::new(chunk_events(&chunks)));

    let message = stream(
        &model(),
        &context(),
        Some(&options_with(fetch.clone(), Some("session-123"))),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    match &requests[0].body {
        HttpBody::Json(body) => {
            assert_eq!(
                body.get("prompt_cache_key")
                    .and_then(serde_json::Value::as_str),
                Some("session-123")
            );
            assert_eq!(
                body.get("stream_options"),
                Some(&serde_json::json!({"include_usage": true}))
            );
        }
        _ => panic!("expected JSON body"),
    }
    // Usage parsed from the final chunk: cached tokens counted as reads.
    assert_eq!(message.usage.input, 1);
    assert_eq!(message.usage.output, 1);
}

#[tokio::test]
async fn empty_tools_with_tool_history_sends_empty_tools_param() {
    let chunks = vec![
        serde_json::json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ];
    let fetch = Arc::new(RecordingFetch::new(chunk_events(&chunks)));

    let context_with_tool_result = Context {
        system_prompt: None,
        messages: vec![
            Message::Assistant(Box::new(
                pi_core::ai::providers::faux::faux_assistant_message(
                    "",
                    pi_core::ai::providers::faux::FauxMessageOptions {
                        stop_reason: Some(StopReason::ToolUse),
                        ..Default::default()
                    },
                ),
            )),
            Message::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
                tool_call_id: "call_1".to_string(),
                tool_name: "echo".to_string(),
                content: vec![pi_core::ai::types::BlockContent::Text(
                    pi_core::ai::types::TextContent {
                        text: "done".to_string(),
                        ..Default::default()
                    },
                )],
                is_error: false,
                timestamp: 0,
                ..Default::default()
            })),
        ],
        tools: None,
    };

    let message = stream(
        &model(),
        &context_with_tool_result,
        Some(&options_with(fetch.clone(), None)),
    )
    .result()
    .await;
    assert_eq!(message.stop_reason, StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    match &requests[0].body {
        HttpBody::Json(body) => {
            // Tool history present with no tools: empty tools param is sent.
            assert_eq!(body.get("tools"), Some(&serde_json::json!([])));
        }
        _ => panic!("expected JSON body"),
    }
}

#[tokio::test]
async fn streaming_text_deltas_produce_text_blocks() {
    let chunks = vec![
        serde_json::json!({"id": "chatcmpl-4", "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": null}]}),
        serde_json::json!({"id": "chatcmpl-4", "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": "stop"}]}),
    ];
    let fetch = Arc::new(RecordingFetch::new(chunk_events(&chunks)));

    let stream = stream(&model(), &context(), Some(&options_with(fetch, None)));
    let mut saw_text_delta = false;
    let mut saw_text_end = false;
    while let Some(event) = stream.next().await {
        match event {
            pi_core::ai::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                saw_text_delta = true;
                assert!(!delta.is_empty());
            }
            pi_core::ai::types::AssistantMessageEvent::TextEnd { .. } => saw_text_end = true,
            _ => {}
        }
    }
    let message = stream.result().await;
    assert!(saw_text_delta);
    assert!(saw_text_end);
    assert_eq!(
        message.content[0],
        pi_core::ai::types::AssistantContent::Text(pi_core::ai::types::TextContent {
            text: "Hello world".to_string(),
            ..Default::default()
        })
    );
}

// ---------------------------------------------------------------------------
// openai-completions-cache-control-format.test.ts
// ---------------------------------------------------------------------------

fn cache_control_context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: Some("System prompt".to_string()),
        messages,
        tools: Some(vec![read_tool()]),
    }
}

fn anthropic_cache_control_model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: "Custom Qwen".to_string(),
        api: "openai-completions".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://example.com/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 32_000,
        compat: Some(ModelCompat {
            cache_control_format: Some(CacheControlFormat::Anthropic),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn expect_anthropic_cache_markers(body: &Value) {
    let messages = body["messages"].as_array().expect("messages array");
    let instruction = messages
        .iter()
        .find(|message| matches!(message["role"].as_str(), Some("system") | Some("developer")))
        .expect("instruction message");
    let content = instruction["content"].as_array().expect("content array");
    assert_eq!(content[0]["cache_control"], json!({"type": "ephemeral"}));

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["cache_control"], json!({"type": "ephemeral"}));

    let last = messages.last().expect("last message");
    assert_eq!(last["role"], json!("user"));
    let content = last["content"].as_array().expect("last content array");
    assert_eq!(content[0]["cache_control"], json!({"type": "ephemeral"}));
}

#[tokio::test]
async fn applies_anthropic_style_cache_markers_when_compat_enables_them() {
    let model = anthropic_cache_control_model("custom-qwen");
    let body = capture_direct_body(
        &model,
        &cache_control_context(vec![user_message("Hello")]),
        direct_options(),
    )
    .await;
    expect_anthropic_cache_markers(&body);
}

#[tokio::test]
async fn preserves_anthropic_style_cache_markers_for_openrouter_anthropic_models() {
    let model = builtin("openrouter", "anthropic/claude-sonnet-4");
    let body = capture_direct_body(
        &model,
        &cache_control_context(vec![user_message("Hello")]),
        direct_options(),
    )
    .await;
    expect_anthropic_cache_markers(&body);
}

#[tokio::test]
async fn moves_the_conversation_cache_marker_to_a_tool_result() {
    let model = builtin("openrouter", "anthropic/claude-sonnet-4");
    let messages = vec![
        user_message("Read the file"),
        Message::Assistant(Box::new(AssistantMessage {
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            model: model.id.clone(),
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "README.md"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ..Default::default()
            })],
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            ..Default::default()
        })),
        Message::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
            tool_call_id: "call_1".to_string(),
            tool_name: "read".to_string(),
            content: vec![pi_core::ai::types::BlockContent::Text(
                pi_core::ai::types::TextContent {
                    text: "file contents".to_string(),
                    ..Default::default()
                },
            )],
            is_error: false,
            timestamp: 0,
            ..Default::default()
        })),
    ];

    let body =
        capture_direct_body(&model, &cache_control_context(messages), direct_options()).await;

    let user = body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == json!("user"))
        .expect("user message");
    assert_eq!(user["content"], json!("Read the file"));

    let tool_message = body["messages"].as_array().expect("messages array");
    let tool_message = tool_message.last().expect("tool message");
    assert_eq!(tool_message["role"], json!("tool"));
    let content = tool_message["content"]
        .as_array()
        .expect("tool content array");
    assert_eq!(content[0]["cache_control"], json!({"type": "ephemeral"}));
}

#[tokio::test]
async fn omits_anthropic_style_cache_markers_when_cache_retention_is_none() {
    let model = anthropic_cache_control_model("custom-qwen");
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::None);

    let body = capture_direct_body(
        &model,
        &cache_control_context(vec![user_message("Hello")]),
        options,
    )
    .await;

    let messages = body["messages"].as_array().expect("messages array");
    let instruction = messages
        .iter()
        .find(|message| matches!(message["role"].as_str(), Some("system") | Some("developer")))
        .expect("instruction message");
    assert!(instruction["content"].as_array().is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(
        messages.last().expect("last message")["content"]
            .as_str()
            .is_some()
    );
}

// ---------------------------------------------------------------------------
// openai-completions-prompt-cache.test.ts
// ---------------------------------------------------------------------------

/// TS `createModel`: the catalog compat is stripped so detection applies,
/// then explicit overrides are layered on top.
fn prompt_cache_model() -> Model {
    let mut model = builtin("openai", "gpt-4o-mini");
    model.compat = None;
    model
}

fn prompt_cache_context() -> Context {
    Context {
        system_prompt: Some("sys".to_string()),
        messages: vec![user_message("hi")],
        ..Default::default()
    }
}

#[tokio::test]
async fn sets_prompt_cache_retention_to_24h_when_cache_retention_is_long() {
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::Long);
    options.base.session_id = Some("session-456".to_string());

    let body = capture_direct_body(&prompt_cache_model(), &prompt_cache_context(), options).await;

    assert_eq!(body["prompt_cache_key"], json!("session-456"));
    assert_eq!(body["prompt_cache_retention"], json!("24h"));
}

#[tokio::test]
async fn clamps_prompt_cache_key_to_openai_64_character_limit() {
    let mut options = direct_options();
    options.base.session_id = Some("x".repeat(67));

    let body = capture_direct_body(&prompt_cache_model(), &prompt_cache_context(), options).await;

    assert_eq!(body["prompt_cache_key"], json!("x".repeat(64)));
}

#[tokio::test]
async fn omits_prompt_cache_fields_when_cache_retention_is_none() {
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::None);
    options.base.session_id = Some("session-789".to_string());

    let body = capture_direct_body(&prompt_cache_model(), &prompt_cache_context(), options).await;

    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn omits_prompt_cache_fields_for_non_openai_base_urls_without_compatible_long_retention() {
    let mut model = prompt_cache_model();
    model.base_url = "https://proxy.example.com/v1".to_string();
    model.compat = Some(ModelCompat {
        supports_long_cache_retention: Some(false),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::Long);
    options.base.session_id = Some("session-proxy".to_string());

    let body = capture_direct_body(&model, &prompt_cache_context(), options).await;

    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn uses_pi_cache_retention_for_direct_openai_requests() {
    // TS sets process.env.PI_CACHE_RETENTION; the port injects a scoped
    // ProviderEnv instead of mutating process env.
    let mut options = direct_options();
    options.base.base.env = Some(env_from(&[("PI_CACHE_RETENTION", "long")]));
    options.base.session_id = Some("session-env".to_string());

    let body = capture_direct_body(&prompt_cache_model(), &prompt_cache_context(), options).await;

    assert_eq!(body["prompt_cache_key"], json!("session-env"));
    assert_eq!(body["prompt_cache_retention"], json!("24h"));
}

#[tokio::test]
async fn sends_known_session_affinity_headers_when_compat_enables_them() {
    let mut model = prompt_cache_model();
    model.base_url = "https://proxy.example.com/v1".to_string();
    model.compat = Some(ModelCompat {
        send_session_affinity_headers: Some(true),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.session_id = Some("session-affinity".to_string());

    let request = capture_direct(&model, &prompt_cache_context(), options).await;

    assert_eq!(header(&request, "session_id"), Some("session-affinity"));
    assert_eq!(
        header(&request, "x-client-request-id"),
        Some("session-affinity")
    );
    assert_eq!(
        header(&request, "x-session-affinity"),
        Some("session-affinity")
    );
}

#[tokio::test]
async fn sends_fireworks_session_affinity_for_glm_models() {
    for id in [
        "accounts/fireworks/models/glm-5p2",
        "accounts/fireworks/routers/glm-5p2-fast",
    ] {
        let model = builtin("fireworks", id);
        let mut options = direct_options();
        options.base.session_id = Some("fireworks-session".to_string());

        let request = capture_direct(&model, &prompt_cache_context(), options).await;

        assert_eq!(
            header(&request, "x-session-affinity"),
            Some("fireworks-session")
        );
    }
}

#[tokio::test]
async fn uses_openai_no_session_format_when_configured() {
    let mut model = prompt_cache_model();
    model.compat = Some(ModelCompat {
        send_session_affinity_headers: Some(true),
        session_affinity_format: Some(SessionAffinityFormat::OpenaiNosession),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.session_id = Some("session-nosession".to_string());

    let request = capture_direct(&model, &prompt_cache_context(), options).await;
    let body = body_of(&request);

    assert!(body.get("session_id").is_none());
    assert_eq!(body["prompt_cache_key"], json!("session-nosession"));
    assert!(header(&request, "session_id").is_none());
    assert_eq!(
        header(&request, "x-client-request-id"),
        Some("session-nosession")
    );
    assert_eq!(
        header(&request, "x-session-affinity"),
        Some("session-nosession")
    );
    assert!(header(&request, "x-session-id").is_none());
}

#[tokio::test]
async fn uses_openrouter_session_affinity_header_when_configured() {
    let mut model = prompt_cache_model();
    model.base_url = "https://proxy.example.com/v1".to_string();
    model.compat = Some(ModelCompat {
        send_session_affinity_headers: Some(true),
        session_affinity_format: Some(SessionAffinityFormat::Openrouter),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.session_id = Some("session-proxy".to_string());

    let request = capture_direct(&model, &prompt_cache_context(), options).await;
    let body = body_of(&request);

    assert!(body.get("session_id").is_none());
    assert!(body.get("prompt_cache_key").is_none());
    assert_eq!(header(&request, "x-session-id"), Some("session-proxy"));
    assert!(header(&request, "session_id").is_none());
    assert!(header(&request, "x-client-request-id").is_none());
    assert!(header(&request, "x-session-affinity").is_none());
}

#[tokio::test]
async fn auto_detects_openrouter_session_affinity_header_for_openrouter_endpoints() {
    let mut model = prompt_cache_model();
    model.provider = "openrouter".to_string();
    model.base_url = "https://openrouter.ai/api/v1".to_string();
    model.compat = Some(ModelCompat {
        send_session_affinity_headers: Some(true),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.session_id = Some("session-openrouter".to_string());

    let request = capture_direct(&model, &prompt_cache_context(), options).await;
    let body = body_of(&request);

    assert!(body.get("session_id").is_none());
    assert!(body.get("prompt_cache_key").is_none());
    assert_eq!(header(&request, "x-session-id"), Some("session-openrouter"));
    assert!(header(&request, "session_id").is_none());
    assert!(header(&request, "x-client-request-id").is_none());
    assert!(header(&request, "x-session-affinity").is_none());
}

#[tokio::test]
async fn omits_openrouter_session_affinity_data_when_disabled() {
    let mut model = prompt_cache_model();
    model.provider = "openrouter".to_string();
    model.base_url = "https://openrouter.ai/api/v1".to_string();
    let mut options = direct_options();
    options.base.session_id = Some("session-openrouter".to_string());

    let request = capture_direct(&model, &prompt_cache_context(), options).await;
    let body = body_of(&request);

    assert!(body.get("session_id").is_none());
    assert!(body.get("prompt_cache_key").is_none());
    assert!(header(&request, "x-session-id").is_none());
}

#[tokio::test]
async fn omits_session_affinity_headers_when_cache_retention_is_none() {
    let mut model = prompt_cache_model();
    model.base_url = "https://proxy.example.com/v1".to_string();
    model.compat = Some(ModelCompat {
        send_session_affinity_headers: Some(true),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::None);
    options.base.session_id = Some("session-affinity".to_string());

    let request = capture_direct(&model, &prompt_cache_context(), options).await;

    assert!(header(&request, "session_id").is_none());
    assert!(header(&request, "x-client-request-id").is_none());
    assert!(header(&request, "x-session-affinity").is_none());
}

#[tokio::test]
async fn lets_explicit_headers_override_generated_session_affinity_headers() {
    let mut model = prompt_cache_model();
    model.base_url = "https://proxy.example.com/v1".to_string();
    model.compat = Some(ModelCompat {
        send_session_affinity_headers: Some(true),
        ..Default::default()
    });
    let mut options = direct_options();
    options.base.session_id = Some("session-affinity".to_string());
    options.base.base.headers = Some(headers_from(&[
        ("session_id", "override-session"),
        ("x-client-request-id", "override-request"),
        ("x-session-affinity", "override-affinity"),
    ]));

    let request = capture_direct(&model, &prompt_cache_context(), options).await;

    assert_eq!(header(&request, "session_id"), Some("override-session"));
    assert_eq!(
        header(&request, "x-client-request-id"),
        Some("override-request")
    );
    assert_eq!(
        header(&request, "x-session-affinity"),
        Some("override-affinity")
    );
}

// ---------------------------------------------------------------------------
// openai-completions-empty-tools.test.ts
// ---------------------------------------------------------------------------

fn empty_tools_context(tools: Option<Vec<Tool>>, message: &str) -> Context {
    Context {
        messages: vec![user_message(message)],
        tools,
        ..Default::default()
    }
}

#[tokio::test]
async fn omits_tools_field_when_context_tools_is_an_empty_array() {
    let model = prompt_cache_model();
    let body = capture_simple_body(
        &model,
        &empty_tools_context(Some(Vec::new()), "hi"),
        simple_test_options(),
    )
    .await;

    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn omits_tools_field_when_context_tools_is_undefined() {
    let model = prompt_cache_model();
    let body = capture_simple_body(
        &model,
        &empty_tools_context(None, "hi"),
        simple_test_options(),
    )
    .await;

    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn sends_default_max_tokens() {
    let model = prompt_cache_model();
    let body = capture_simple_body(
        &model,
        &empty_tools_context(None, "hi"),
        simple_test_options(),
    )
    .await;

    assert!(body.get("max_tokens").is_none());
    assert_eq!(
        body["max_completion_tokens"].as_u64(),
        Some(model.max_tokens)
    );
}

#[tokio::test]
async fn sends_explicit_max_tokens() {
    let model = prompt_cache_model();
    let mut options = simple_test_options();
    options.base.max_tokens = Some(1_234);

    let body = capture_simple_body(&model, &empty_tools_context(None, "hi"), options).await;

    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"].as_u64(), Some(1_234));
}

#[tokio::test]
async fn clamps_default_max_tokens_to_remaining_context() {
    let mut model = prompt_cache_model();
    model.context_window = 10_000;
    model.max_tokens = 8_000;

    let body = capture_simple_body(
        &model,
        &empty_tools_context(None, &"x".repeat(8_000)),
        simple_test_options(),
    )
    .await;

    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"].as_u64(), Some(3_904));
}

#[tokio::test]
async fn clamps_explicit_max_tokens_to_remaining_context() {
    let mut model = prompt_cache_model();
    model.context_window = 10_000;
    model.max_tokens = 8_000;
    let mut options = simple_test_options();
    options.base.max_tokens = Some(7_000);

    let body = capture_simple_body(
        &model,
        &empty_tools_context(None, &"x".repeat(8_000)),
        options,
    )
    .await;

    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"].as_u64(), Some(3_904));
}

fn cloudflare_env() -> ProviderEnv {
    env_from(&[
        ("CLOUDFLARE_API_KEY", "cf-token"),
        ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
        ("CLOUDFLARE_GATEWAY_ID", "gateway-id"),
    ])
}

/// The Cloudflare suites pass no apiKey so the global `streamSimple` routes
/// through provider auth (TS leaves `options.apiKey` unset).
fn cloudflare_options() -> SimpleStreamOptions {
    let mut options = simple_test_options();
    options.base.base.api_key = None;
    options
}

#[tokio::test]
async fn uses_conservative_fields_for_cloudflare_ai_gateway_compat_models() {
    let model = builtin(
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    );
    let context = Context {
        system_prompt: Some("You are helpful.".to_string()),
        messages: vec![user_message("hi")],
        ..Default::default()
    };
    let mut options = cloudflare_options();
    options.base.base.env = Some(cloudflare_env());
    options.base.max_tokens = Some(1_234);
    options.reasoning = Some(ThinkingLevel::High);

    let request = capture_compat_simple(&model, &context, options).await;
    let body = body_of(&request);

    assert_eq!(
        body["messages"][0]["role"],
        json!("system"),
        "messages: {body}"
    );
    assert_eq!(body["max_tokens"].as_u64(), Some(1_234));
    assert!(body.get("max_completion_tokens").is_none());
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("store").is_none());
    assert_eq!(
        request.url,
        "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/compat/chat/completions"
    );
    // The SDK-supplied Authorization placeholder is dropped (TS null header).
    assert!(header(&request, "authorization").is_none());
    assert_eq!(
        header(&request, "cf-aig-authorization"),
        Some("Bearer cf-token")
    );
}

#[tokio::test]
async fn resolves_cloudflare_ai_gateway_base_url_through_provider_auth() {
    let model = builtin(
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    );
    let context = empty_tools_context(None, "hi");
    let mut options = cloudflare_options();
    options.base.base.env = Some(cloudflare_env());

    let request = capture_compat_simple(&model, &context, options).await;

    assert_eq!(
        request.url,
        "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/compat/chat/completions"
    );
}

#[tokio::test]
async fn preserves_inline_upstream_authorization_for_cloudflare_byok_requests() {
    let model = builtin("cloudflare-ai-gateway", "gpt-5.1");
    let context = empty_tools_context(None, "hi");
    let mut options = cloudflare_options();
    options.base.base.env = Some(cloudflare_env());
    options.base.base.headers = Some(headers_from(&[("Authorization", "Bearer upstream-token")]));

    let request = capture_compat_simple(&model, &context, options).await;

    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer upstream-token")
    );
    assert_eq!(
        header(&request, "cf-aig-authorization"),
        Some("Bearer cf-token")
    );
}

#[tokio::test]
async fn sends_session_affinity_headers_for_workers_ai_through_cloudflare_gateway() {
    let model = builtin(
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    );
    let context = empty_tools_context(None, "hi");
    let mut options = cloudflare_options();
    options.base.base.env = Some(cloudflare_env());
    options.base.session_id = Some("session-1".to_string());

    let request = capture_compat_simple(&model, &context, options).await;

    assert_eq!(header(&request, "session_id"), Some("session-1"));
    assert_eq!(header(&request, "x-client-request-id"), Some("session-1"));
    assert_eq!(header(&request, "x-session-affinity"), Some("session-1"));
}

// ---------------------------------------------------------------------------
// openai-completions-tool-choice.test.ts (payload/options cases)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwards_tool_choice_from_simple_options_to_payload() {
    // TS passes the provider-specific "required" value through `streamSimple`
    // options via a type cast; the Rust shared ToolChoice union cannot carry
    // it, so the raw provider value is exercised through the API options
    // (the streamSimple forwarding itself is covered by the "none" case).
    let model = prompt_cache_model();
    let context = Context {
        messages: vec![user_message("Call ping with ok=true")],
        tools: Some(vec![ping_tool()]),
        ..Default::default()
    };
    let mut options = direct_options();
    options.tool_choice = Some(json!("required"));

    let body = capture_direct_body(&model, &context, options).await;

    assert_eq!(body["tool_choice"], json!("required"));
    let tools = body["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
}

#[tokio::test]
async fn includes_tool_choice_when_no_tools_are_provided() {
    let model = prompt_cache_model();
    let context = Context {
        messages: vec![user_message("Summarize the conversation")],
        ..Default::default()
    };
    let mut options = simple_test_options();
    options.tool_choice = Some(pi_core::ai::types::ToolChoice::None);

    let body = capture_simple_body(&model, &context, options).await;

    assert_eq!(body["tool_choice"], json!("none"));
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn omits_strict_when_compat_disables_strict_mode() {
    let mut model = prompt_cache_model();
    model.compat = Some(ModelCompat {
        supports_strict_mode: Some(false),
        ..Default::default()
    });
    let context = Context {
        messages: vec![user_message("Call ping with ok=true")],
        tools: Some(vec![ping_tool()]),
        ..Default::default()
    };

    let body = capture_simple_body(&model, &context, simple_test_options()).await;

    let tool = &body["tools"][0]["function"];
    assert!(tool.is_object());
    assert!(tool.get("strict").is_none());
}

#[tokio::test]
async fn maps_groq_qwen_reasoning_levels_to_default_reasoning_effort() {
    let model = builtin("groq", "qwen/qwen3.6-27b");
    let mut options = simple_test_options();
    options.reasoning = Some(ThinkingLevel::Medium);

    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert_eq!(body["reasoning_effort"], json!("default"));
}

#[tokio::test]
async fn keeps_normal_reasoning_effort_for_groq_models_without_compat_mapping() {
    let model = builtin("groq", "openai/gpt-oss-20b");
    let mut options = simple_test_options();
    options.reasoning = Some(ThinkingLevel::Medium);

    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert_eq!(body["reasoning_effort"], json!("medium"));
}

#[tokio::test]
async fn enables_tool_stream_for_supported_zai_models_with_tools() {
    let model = builtin("zai", "glm-5.2");
    let context = Context {
        messages: vec![user_message("Call ping with ok=true")],
        tools: Some(vec![ping_tool()]),
        ..Default::default()
    };

    let body = capture_simple_body(&model, &context, simple_test_options()).await;

    assert_eq!(body["tool_stream"], json!(true));
}

#[test]
fn stores_zai_tool_stream_support_in_model_compat_metadata() {
    for id in ["glm-4.7", "glm-5-turbo", "glm-5.2"] {
        let model = builtin("zai", id);
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.zai_tool_stream),
            Some(true),
            "{id}"
        );
    }
}

#[tokio::test]
async fn maps_zai_glm_5_2_thinking_levels_to_reasoning_effort() {
    let model = builtin("zai", "glm-5.2");
    let context = Context {
        messages: vec![user_message("Hi")],
        ..Default::default()
    };
    let cases = [
        (ThinkingLevel::Low, "high"),
        (ThinkingLevel::Medium, "high"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Max, "max"),
    ];

    for (level, effort) in cases {
        let mut options = simple_test_options();
        options.reasoning = Some(level);
        let body = capture_simple_body(&model, &context, options).await;

        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
        assert_eq!(body["reasoning_effort"], json!(effort));
    }
}

#[tokio::test]
async fn omits_zai_glm_5_2_reasoning_effort_when_thinking_is_off() {
    let model = builtin("zai", "glm-5.2");
    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        simple_test_options(),
    )
    .await;

    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert!(body.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn respects_explicit_zai_tool_stream_compat_override() {
    let mut model = builtin("zai", "glm-5.2");
    let compat = model.compat.clone().unwrap_or_default();
    model.compat = Some(ModelCompat {
        zai_tool_stream: Some(true),
        ..compat
    });
    let context = Context {
        messages: vec![user_message("Call ping with ok=true")],
        tools: Some(vec![ping_tool()]),
        ..Default::default()
    };

    let body = capture_simple_body(&model, &context, simple_test_options()).await;

    assert_eq!(body["tool_stream"], json!(true));
}

#[tokio::test]
async fn omits_tool_stream_when_no_tools_are_provided() {
    let model = builtin("zai", "glm-5.2");
    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        simple_test_options(),
    )
    .await;

    assert!(body.get("tool_stream").is_none());
}

#[tokio::test]
async fn sends_max_tokens_for_opencode_completions_models() {
    for provider in ["opencode-go", "opencode"] {
        let model = builtin(provider, "kimi-k2.6");
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.max_tokens_field),
            Some(MaxTokensField::MaxTokens),
            "{provider}"
        );

        let mut options = simple_test_options();
        options.base.max_tokens = Some(123);
        let body = capture_simple_body(
            &model,
            &Context {
                messages: vec![user_message("Hi")],
                ..Default::default()
            },
            options,
        )
        .await;

        assert_eq!(body["max_tokens"].as_u64(), Some(123));
        assert!(body.get("max_completion_tokens").is_none());
    }
}

#[tokio::test]
async fn sends_max_tokens_for_builtin_and_custom_deepseek_api_models() {
    let custom = local_completions_model(
        "custom-deepseek-model",
        "Custom DeepSeek Model",
        "custom-deepseek",
        "https://api.deepseek.com",
    );
    let custom_uppercase = local_completions_model(
        "custom-uppercase-deepseek-model",
        "Custom Uppercase DeepSeek Model",
        "custom-deepseek",
        "https://API.DeepSeek.COM",
    );
    let models = [
        builtin("deepseek", "deepseek-v4-flash"),
        builtin("deepseek", "deepseek-v4-pro"),
        custom,
        custom_uppercase,
    ];

    for model in &models[..2] {
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.max_tokens_field),
            Some(MaxTokensField::MaxTokens)
        );
    }
    for model in &models {
        let mut options = simple_test_options();
        options.base.max_tokens = Some(123);
        let body = capture_simple_body(
            model,
            &Context {
                messages: vec![user_message("Hi")],
                ..Default::default()
            },
            options,
        )
        .await;

        assert_eq!(body["max_tokens"].as_u64(), Some(123), "{}", model.id);
        assert!(body.get("max_completion_tokens").is_none());
    }
}

#[tokio::test]
async fn sends_max_tokens_for_zai_completions_models() {
    for id in ["glm-5-turbo", "glm-5.2"] {
        let model = builtin("zai", id);
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.max_tokens_field),
            Some(MaxTokensField::MaxTokens),
            "{id}"
        );

        let mut options = simple_test_options();
        options.base.max_tokens = Some(123);
        let body = capture_simple_body(
            &model,
            &Context {
                messages: vec![user_message("Hi")],
                ..Default::default()
            },
            options,
        )
        .await;

        assert_eq!(body["max_tokens"].as_u64(), Some(123));
        assert!(body.get("max_completion_tokens").is_none());
    }
}

#[tokio::test]
async fn omits_reasoning_effort_for_opencode_grok_build() {
    let model = builtin("opencode", "grok-build-0.1");
    let mut options = simple_test_options();
    options.reasoning = Some(ThinkingLevel::High);

    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert!(body.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn uses_openrouter_reasoning_object_instead_of_reasoning_effort() {
    let model = builtin("openrouter", "deepseek/deepseek-r1");
    let mut options = simple_test_options();
    options.reasoning = Some(ThinkingLevel::High);

    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    assert!(body.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn uses_configurable_chat_template_boolean_thinking_kwargs() {
    let model = local_completions_model(
        "deepseek-ai/DeepSeek-V3.1",
        "DeepSeek V3.1 via vLLM",
        "local-vllm",
        "http://localhost:8000/v1",
    );
    let mut model = model;
    model.compat = Some(ModelCompat {
        thinking_format: Some(ThinkingFormat::ChatTemplate),
        supports_reasoning_effort: Some(false),
        chat_template_kwargs: Some(chat_template_kwargs(&[(
            "thinking",
            ChatTemplateKwargValue::Variable {
                variable: ThinkingVariable::Enabled,
                omit_when_off: None,
            },
        )])),
        ..Default::default()
    });
    let context = Context {
        messages: vec![user_message("Hi")],
        ..Default::default()
    };

    for (reasoning, expected) in [(Some(ThinkingLevel::High), true), (None, false)] {
        let mut options = simple_test_options();
        options.reasoning = reasoning;
        let body = capture_simple_body(&model, &context, options).await;

        assert_eq!(body["chat_template_kwargs"], json!({"thinking": expected}));
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }
}

#[tokio::test]
async fn uses_qwen_chat_template_thinking_kwargs() {
    let mut model = local_completions_model(
        "Qwen/Qwen3-Coder",
        "Qwen3 Coder via vLLM",
        "local-vllm",
        "http://localhost:8000/v1",
    );
    model.compat = Some(ModelCompat {
        thinking_format: Some(ThinkingFormat::QwenChatTemplate),
        supports_reasoning_effort: Some(false),
        ..Default::default()
    });
    let context = Context {
        messages: vec![user_message("Hi")],
        ..Default::default()
    };

    for (reasoning, expected) in [(Some(ThinkingLevel::High), true), (None, false)] {
        let mut options = simple_test_options();
        options.reasoning = reasoning;
        let body = capture_simple_body(&model, &context, options).await;

        assert_eq!(
            body["chat_template_kwargs"],
            json!({"enable_thinking": expected, "preserve_thinking": true})
        );
        assert!(body.get("reasoning_effort").is_none());
    }
}

#[tokio::test]
async fn uses_configurable_chat_template_effort_kwargs_with_static_kwargs() {
    let mut model = local_completions_model(
        "unsloth/gpt-oss-120b-GGUF",
        "GPT OSS via vLLM",
        "local-vllm",
        "http://localhost:8000/v1",
    );
    model.thinking_level_map = Some(BTreeMap::from([(
        pi_core::ai::types::ModelThinkingLevel::Xhigh,
        Some("max".to_string()),
    )]));
    model.compat = Some(ModelCompat {
        thinking_format: Some(ThinkingFormat::ChatTemplate),
        supports_reasoning_effort: Some(false),
        chat_template_kwargs: Some(chat_template_kwargs(&[
            ("preserve_thinking", ChatTemplateKwargValue::Boolean(true)),
            (
                "reasoning_effort",
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Effort,
                    omit_when_off: Some(true),
                },
            ),
        ])),
        ..Default::default()
    });
    let mut options = simple_test_options();
    options.reasoning = Some(ThinkingLevel::Xhigh);

    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert_eq!(
        body["chat_template_kwargs"],
        json!({"preserve_thinking": true, "reasoning_effort": "max"})
    );
    assert!(body.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn uses_ant_ling_compatibility_metadata() {
    let model = builtin("ant-ling", "Ring-2.6-1T");
    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_store, Some(false));
    assert_eq!(compat.supports_developer_role, Some(false));
    assert_eq!(compat.supports_reasoning_effort, Some(false));
    assert_eq!(compat.max_tokens_field, Some(MaxTokensField::MaxTokens));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::AntLing));
    assert_eq!(compat.supports_long_cache_retention, Some(false));
    assert_eq!(compat.supports_strict_mode, None);
    assert_eq!(
        compat.requires_reasoning_content_on_assistant_messages,
        None
    );

    let mut options = simple_test_options();
    options.base.max_tokens = Some(123);
    options.reasoning = Some(ThinkingLevel::High);
    options.base.cache_retention = Some(CacheRetention::Long);
    options.base.session_id = Some("ant-ling-session".to_string());

    let body = capture_simple_body(
        &model,
        &Context {
            system_prompt: Some("Follow instructions.".to_string()),
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert_eq!(body["max_tokens"].as_u64(), Some(123));
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("store").is_none());
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn omits_ant_ling_reasoning_for_unmapped_efforts_and_non_reasoning_models() {
    let ring = builtin("ant-ling", "Ring-2.6-1T");
    let mut options = direct_options();
    options.reasoning_effort = Some(ThinkingLevel::Medium);
    let body = capture_direct_body(
        &ring,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await;
    assert!(body.get("reasoning").is_none());

    let ling = builtin("ant-ling", "Ling-2.6-flash");
    let mut simple = simple_test_options();
    simple.reasoning = Some(ThinkingLevel::High);
    let body = capture_simple_body(
        &ling,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        simple,
    )
    .await;
    assert!(body.get("reasoning").is_none());
}

// ---------------------------------------------------------------------------
// openai-completions-thinking-token-budget.test.ts
// ---------------------------------------------------------------------------

fn budget_vllm_model(compat: ModelCompat) -> Model {
    Model {
        id: "zai-org/glm-5.2".to_string(),
        name: "GLM 5.2 (local vLLM)".to_string(),
        api: "openai-completions".to_string(),
        provider: "local-vllm".to_string(),
        base_url: "http://localhost:8000/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 262_144,
        max_tokens: 16_384,
        compat: Some(compat),
        ..Default::default()
    }
}

fn default_budget_vllm_model() -> Model {
    budget_vllm_model(ModelCompat {
        thinking_format: Some(ThinkingFormat::Zai),
        supports_thinking_token_budget: Some(true),
        ..Default::default()
    })
}

async fn capture_budget(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    budgets: Option<ThinkingBudgets>,
    max_tokens: Option<u64>,
) -> Value {
    let mut options = simple_test_options();
    options.reasoning = reasoning;
    options.thinking_budgets = budgets;
    options.base.max_tokens = max_tokens;
    capture_simple_body(
        model,
        &Context {
            messages: vec![user_message("Hi")],
            ..Default::default()
        },
        options,
    )
    .await
}

#[tokio::test]
async fn sends_the_configured_budget_for_the_requested_level() {
    let body = capture_budget(
        &default_budget_vllm_model(),
        Some(ThinkingLevel::Medium),
        Some(ThinkingBudgets {
            medium: Some(4_096),
            ..Default::default()
        }),
        None,
    )
    .await;
    assert_eq!(body["thinking_token_budget"].as_u64(), Some(4_096));
}

#[tokio::test]
async fn omits_the_budget_when_neither_the_field_nor_the_alias_is_set() {
    let model = budget_vllm_model(ModelCompat {
        thinking_format: Some(ThinkingFormat::Zai),
        ..Default::default()
    });
    let body = capture_budget(
        &model,
        Some(ThinkingLevel::Medium),
        Some(ThinkingBudgets {
            medium: Some(4_096),
            ..Default::default()
        }),
        None,
    )
    .await;
    assert!(body.get("thinking_token_budget").is_none());
    assert!(body.get("thinking_budget").is_none());
    assert!(body.get("thinking_budget_tokens").is_none());
}

#[tokio::test]
async fn omits_the_budget_when_thinking_is_off() {
    let body = capture_budget(
        &default_budget_vllm_model(),
        None,
        Some(ThinkingBudgets {
            high: Some(8_192),
            ..Default::default()
        }),
        None,
    )
    .await;
    assert!(body.get("thinking_token_budget").is_none());
}

#[tokio::test]
async fn clamps_xhigh_and_max_to_the_high_budget() {
    let budgets = ThinkingBudgets {
        high: Some(8_192),
        ..Default::default()
    };
    let xhigh = capture_budget(
        &default_budget_vllm_model(),
        Some(ThinkingLevel::Xhigh),
        Some(budgets.clone()),
        None,
    )
    .await;
    let max = capture_budget(
        &default_budget_vllm_model(),
        Some(ThinkingLevel::Max),
        Some(budgets),
        None,
    )
    .await;
    assert_eq!(xhigh["thinking_token_budget"].as_u64(), Some(8_192));
    assert_eq!(max["thinking_token_budget"].as_u64(), Some(8_192));
}

#[tokio::test]
async fn leaves_room_for_the_answer_when_the_budget_meets_the_response_ceiling() {
    let body = capture_budget(
        &default_budget_vllm_model(),
        Some(ThinkingLevel::High),
        None,
        None,
    )
    .await;
    assert_eq!(body["thinking_token_budget"].as_u64(), Some(16_384 - 1_024));
}

#[tokio::test]
async fn uses_the_caller_max_tokens_as_the_ceiling_when_it_is_lower() {
    let body = capture_budget(
        &default_budget_vllm_model(),
        Some(ThinkingLevel::High),
        Some(ThinkingBudgets {
            high: Some(8_192),
            ..Default::default()
        }),
        Some(4_096),
    )
    .await;
    assert_eq!(body["thinking_token_budget"].as_u64(), Some(4_096 - 1_024));
}

#[tokio::test]
async fn sends_alias_fields_when_thinking_token_budget_field_is_set() {
    for (field, name) in [
        (ThinkingTokenBudgetField::ThinkingBudget, "thinking_budget"),
        (
            ThinkingTokenBudgetField::ThinkingBudgetTokens,
            "thinking_budget_tokens",
        ),
    ] {
        let model = budget_vllm_model(ModelCompat {
            thinking_format: Some(ThinkingFormat::Qwen),
            thinking_token_budget_field: Some(field),
            ..Default::default()
        });
        let body = capture_budget(
            &model,
            Some(ThinkingLevel::Medium),
            Some(ThinkingBudgets {
                medium: Some(4_096),
                ..Default::default()
            }),
            None,
        )
        .await;

        assert_eq!(body[name].as_u64(), Some(4_096));
        assert!(body.get("thinking_token_budget").is_none());
    }
}

#[tokio::test]
async fn lets_thinking_token_budget_field_win_over_the_boolean_alias() {
    let model = budget_vllm_model(ModelCompat {
        thinking_format: Some(ThinkingFormat::Zai),
        supports_thinking_token_budget: Some(true),
        thinking_token_budget_field: Some(ThinkingTokenBudgetField::ThinkingBudget),
        ..Default::default()
    });
    let body = capture_budget(
        &model,
        Some(ThinkingLevel::Medium),
        Some(ThinkingBudgets {
            medium: Some(4_096),
            ..Default::default()
        }),
        None,
    )
    .await;
    assert_eq!(body["thinking_budget"].as_u64(), Some(4_096));
    assert!(body.get("thinking_token_budget").is_none());
}

#[tokio::test]
async fn puts_the_clamped_budget_in_chat_template_kwargs_when_var_is_thinking_budget() {
    let model = budget_vllm_model(ModelCompat {
        thinking_format: Some(ThinkingFormat::ChatTemplate),
        chat_template_kwargs: Some(chat_template_kwargs(&[
            (
                "enable_thinking",
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Enabled,
                    omit_when_off: None,
                },
            ),
            (
                "thinking_budget",
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Budget,
                    omit_when_off: None,
                },
            ),
        ])),
        ..Default::default()
    });
    let body = capture_budget(&model, Some(ThinkingLevel::High), None, None).await;

    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": true, "thinking_budget": 16_384 - 1_024})
    );
    assert!(body.get("thinking_token_budget").is_none());
}

#[tokio::test]
async fn omits_thinking_budget_from_chat_template_kwargs_when_thinking_is_off() {
    let model = budget_vllm_model(ModelCompat {
        thinking_format: Some(ThinkingFormat::ChatTemplate),
        chat_template_kwargs: Some(chat_template_kwargs(&[
            (
                "enable_thinking",
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Enabled,
                    omit_when_off: None,
                },
            ),
            (
                "thinking_budget",
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Budget,
                    omit_when_off: None,
                },
            ),
        ])),
        ..Default::default()
    });
    let body = capture_budget(&model, None, None, None).await;

    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
}

// ---------------------------------------------------------------------------
// sampling-options.test.ts
// ---------------------------------------------------------------------------

fn sampling_completions_model() -> Model {
    Model {
        id: "custom-model".to_string(),
        name: "Custom Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "custom-provider".to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 16_384,
        ..Default::default()
    }
}

fn sampling_params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

#[tokio::test]
async fn merges_stream_option_sampling_params_into_the_request_body() {
    let mut options = simple_test_options();
    options.base.sampling_params = Some(sampling_params(&[
        ("top_p", json!(0.95)),
        ("top_k", json!(0)),
        ("min_p", json!(0)),
    ]));

    let body = capture_simple_body(&sampling_completions_model(), &context(), options).await;

    assert_eq!(body["top_p"].as_f64(), Some(0.95));
    assert_eq!(body["top_k"].as_f64(), Some(0.0));
    assert_eq!(body["min_p"].as_f64(), Some(0.0));
}

#[tokio::test]
async fn omits_sampling_params_when_neither_options_nor_model_set_them() {
    let body = capture_simple_body(
        &sampling_completions_model(),
        &context(),
        simple_test_options(),
    )
    .await;

    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
}

#[tokio::test]
async fn applies_model_level_sampling_params() {
    let mut model = sampling_completions_model();
    model.sampling_params = Some(sampling_params(&[
        ("temperature", json!(1)),
        ("top_p", json!(0.95)),
    ]));

    let body = capture_simple_body(&model, &context(), simple_test_options()).await;

    assert_eq!(body["temperature"].as_f64(), Some(1.0));
    assert_eq!(body["top_p"].as_f64(), Some(0.95));
}

#[tokio::test]
async fn merges_stream_option_keys_over_model_level_keys() {
    let mut model = sampling_completions_model();
    model.sampling_params = Some(sampling_params(&[
        ("top_p", json!(0.95)),
        ("min_p", json!(0.05)),
    ]));
    let mut options = simple_test_options();
    options.base.sampling_params = Some(sampling_params(&[("top_p", json!(0.5))]));

    let body = capture_simple_body(&model, &context(), options).await;

    assert_eq!(body["top_p"].as_f64(), Some(0.5));
    assert_eq!(body["min_p"].as_f64(), Some(0.05));
}

#[tokio::test]
async fn overrides_named_request_fields() {
    let mut options = simple_test_options();
    options.base.temperature = Some(0.0);
    options.base.sampling_params = Some(sampling_params(&[("temperature", json!(1))]));

    let body = capture_simple_body(&sampling_completions_model(), &context(), options).await;

    assert_eq!(body["temperature"].as_f64(), Some(1.0));
}

#[tokio::test]
async fn is_ignored_by_non_openai_compatible_apis() {
    let model = Model {
        id: "vendor--claude".to_string(),
        name: "Vendor Proxy Claude".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "vendor-proxy".to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        ..Default::default()
    };
    let anthropic_sse = [
        format!(
            "event: message_start\ndata: {}\n",
            json!({
                "type": "message_start",
                "message": {"id": "msg_test", "usage": {"input_tokens": 10, "output_tokens": 0}},
            })
        ),
        format!(
            "event: message_delta\ndata: {}\n",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 5},
            })
        ),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n".to_string(),
    ]
    .join("\n");
    let fetch = Arc::new(RecordingFetch::new(anthropic_sse));
    let mut options = simple_test_options();
    options.base.base.api_key = Some("fake-key".to_string());
    options.base.base.fetch = Some(fetch.clone());
    options.base.sampling_params = Some(sampling_params(&[
        ("top_p", json!(0.9)),
        ("top_k", json!(40)),
    ]));

    let _ = stream_simple_anthropic(&model, &context(), Some(&options))
        .result()
        .await;
    let body = body_of(&fetch.request());

    assert!(body.get("top_p").is_none());
    assert!(body.get("top_k").is_none());
}

// ---------------------------------------------------------------------------
// openrouter-reasoning-options.test.ts (streamSimple payload cases)
// ---------------------------------------------------------------------------

fn openrouter_reasoning_model(
    thinking_level_map: Option<pi_core::ai::types::ThinkingLevelMap>,
) -> Model {
    Model {
        id: "stealth/ox-alpha".to_string(),
        name: "Ox Alpha".to_string(),
        api: "openai-completions".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://example.invalid/v1".to_string(),
        reasoning: true,
        thinking_level_map,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        compat: Some(ModelCompat {
            thinking_format: Some(ThinkingFormat::Openrouter),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `getOpenRouterThinkingLevelMap({ mandatory: true, supported_efforts:
/// ["max","high","low"] })`.
fn mandatory_reasoning_map() -> pi_core::ai::types::ThinkingLevelMap {
    use pi_core::ai::types::ModelThinkingLevel as Level;
    BTreeMap::from([
        (Level::Off, None),
        (Level::Minimal, None),
        (Level::Low, Some("low".to_string())),
        (Level::Medium, None),
        (Level::High, Some("high".to_string())),
        (Level::Xhigh, None),
        (Level::Max, Some("max".to_string())),
    ])
}

#[tokio::test]
async fn omits_reasoning_when_a_background_call_does_not_request_it() {
    let model = openrouter_reasoning_model(Some(mandatory_reasoning_map()));
    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hello")],
            ..Default::default()
        },
        simple_test_options(),
    )
    .await;

    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn still_sends_an_explicitly_selected_supported_effort() {
    let model = openrouter_reasoning_model(Some(mandatory_reasoning_map()));
    let mut options = simple_test_options();
    options.reasoning = Some(ThinkingLevel::Low);

    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hello")],
            ..Default::default()
        },
        options,
    )
    .await;

    assert_eq!(body["reasoning"], json!({"effort": "low"}));
}

#[tokio::test]
async fn continues_to_explicitly_disable_reasoning_for_optional_models() {
    let model = openrouter_reasoning_model(None);
    let body = capture_simple_body(
        &model,
        &Context {
            messages: vec![user_message("Hello")],
            ..Default::default()
        },
        simple_test_options(),
    )
    .await;

    assert_eq!(body["reasoning"], json!({"effort": "none"}));
}

// ---------------------------------------------------------------------------
// provider-error-body-regression.test.ts (openai-completions cases)
// ---------------------------------------------------------------------------

fn error_regression_model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 1_000,
        max_tokens: 100,
        ..Default::default()
    }
}

fn error_regression_context() -> Context {
    Context {
        system_prompt: Some(String::new()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![pi_core::ai::types::BlockContent::Text(
                pi_core::ai::types::TextContent {
                    text: "hi".to_string(),
                    ..Default::default()
                },
            )]),
            timestamp: 0,
            role: pi_core::ai::types::RoleUser,
        })],
        tools: Some(Vec::new()),
    }
}

#[tokio::test]
async fn openai_completions_surfaces_status_and_body_for_body_blind_text_errors() {
    let fetch = Arc::new(ErroringFetch {
        status: 403,
        body: r#"{"error":"blocked by gateway WAF"}"#.to_string(),
    });
    let mut options = direct_options();
    options.base.base.api_key = Some("test".to_string());
    options.base.base.fetch = Some(fetch);

    let output = stream(
        &error_regression_model(),
        &error_regression_context(),
        Some(&options),
    )
    .result()
    .await;

    assert_eq!(output.stop_reason, StopReason::Error);
    let error = output.error_message.expect("error message");
    assert!(error.contains("403"), "error: {error}");
    assert!(error.contains("blocked by gateway WAF"), "error: {error}");
    assert_ne!(error, "403 status code (no body)");
}

#[tokio::test]
async fn openai_completions_does_not_double_print_the_openrouter_metadata_raw_extra() {
    let fetch = Arc::new(ErroringFetch {
        status: 403,
        body: r#"{"message":"Provider returned error","code":403,"metadata":{"raw":"upstream WAF blocked policy XYZ"}}"#
            .to_string(),
    });
    let mut options = direct_options();
    options.base.base.api_key = Some("test".to_string());
    options.base.base.fetch = Some(fetch);

    let output = stream(
        &error_regression_model(),
        &error_regression_context(),
        Some(&options),
    )
    .result()
    .await;

    let error = output.error_message.expect("error message");
    assert!(
        error.contains("upstream WAF blocked policy XYZ"),
        "error: {error}"
    );
    let occurrences = error
        .match_indices("upstream WAF blocked policy XYZ")
        .count();
    assert_eq!(occurrences, 1, "error: {error}");
}

// ---------------------------------------------------------------------------
// cache-retention.test.ts (OpenAI Completions describe)
// ---------------------------------------------------------------------------

fn cache_retention_model(compat: Option<ModelCompat>) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "test-openai-completions".to_string(),
        base_url: "https://my-proxy.example.com/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        compat,
        ..Default::default()
    }
}

fn cache_retention_context() -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message("Hello")],
        ..Default::default()
    }
}

#[tokio::test]
async fn sets_prompt_cache_retention_for_non_api_openai_base_url_by_default() {
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::Long);
    options.base.session_id = Some("session-completions".to_string());

    let body = capture_direct_body(
        &cache_retention_model(None),
        &cache_retention_context(),
        options,
    )
    .await;

    assert_eq!(body["prompt_cache_key"], json!("session-completions"));
    assert_eq!(body["prompt_cache_retention"], json!("24h"));
}

#[tokio::test]
async fn omits_prompt_cache_retention_when_supports_long_cache_retention_is_false() {
    let mut options = direct_options();
    options.base.cache_retention = Some(CacheRetention::Long);
    options.base.session_id = Some("session-completions-false".to_string());

    let body = capture_direct_body(
        &cache_retention_model(Some(ModelCompat {
            supports_long_cache_retention: Some(false),
            ..Default::default()
        })),
        &cache_retention_context(),
        options,
    )
    .await;

    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn omits_long_cache_retention_for_opencode_models() {
    let cases = [
        ("opencode", "deepseek-v4-flash"),
        ("opencode", "deepseek-v4-pro"),
        ("opencode", "kimi-k2.5"),
        ("opencode", "kimi-k2.6"),
        ("opencode", "minimax-m2.7"),
        ("opencode-go", "kimi-k2.6"),
    ];

    for (provider, id) in cases {
        let model = builtin(provider, id);
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.supports_long_cache_retention),
            Some(false),
            "{provider}/{id}"
        );

        let mut options = direct_options();
        options.base.cache_retention = Some(CacheRetention::Long);
        options.base.session_id = Some("session-opencode-long-cache-unsupported".to_string());

        let body = capture_direct_body(&model, &cache_retention_context(), options).await;

        assert!(body.get("prompt_cache_key").is_none(), "{provider}/{id}");
        assert!(
            body.get("prompt_cache_retention").is_none(),
            "{provider}/{id}"
        );
    }
}
