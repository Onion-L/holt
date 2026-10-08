//! Ports of the offline OpenAI Responses API tests against the shared
//! conversion/streaming helpers and a canned SSE transport:
//!
//! - `openai-responses-compat.test.ts` (16 cases; the `it.each` model lists
//!   loop inside one test each)
//! - `openai-responses-empty-tool-result.test.ts` (1 case)
//! - `openai-responses-foreign-toolcall-id.test.ts` (1 case)
//! - `openai-responses-message-id.test.ts` (1 case)
//! - `openai-responses-namespace.test.ts` (4 cases)
//! - `openai-responses-partial-json-cleanup.test.ts` (1 case)
//! - `openai-responses-terminal-event.test.ts` (9 cases)
//! - `cache-retention.test.ts` (the OpenAI Responses describe; the two
//!   PI_CACHE_RETENTION env cases inject a scoped `ProviderEnv` instead of
//!   mutating process env)
//! - `provider-error-body-regression.test.ts` (the openai-responses case)
//!
//! The TypeScript suites either capture the payload through `onPayload` or
//! mock `globalThis.fetch` / the `openai` SDK; the Rust port captures the
//! request through a mock `HttpFetch` transport, which is the same observable
//! surface.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::openai_responses::{
    OpenAIResponsesOptions, stream as stream_openai_responses,
};
use pi_core::ai::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ResponsesStreamOptions, convert_responses_messages,
    process_responses_stream,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent, CacheRetention,
    ConstrainedSamplingConfig, ConstrainedSamplingStrict, Context, Message, Model, ModelCompat,
    ModelInput, ProviderEnv, ProviderRequestOptions, RoleAssistant, RoleToolResult, RoleUser,
    SessionAffinityFormat, StopReason, StreamOptions, TextContent, TextSignaturePhase,
    TextSignatureV1, ThinkingContent, Tool, ToolCall, ToolConstrainedSampling, ToolResultMessage,
    Usage, UserContent, UserMessage,
};
use pi_core::ai::utils::event_stream::{collect_events, create_assistant_message_event_stream};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use pi_core::ai::utils::text::short_hash;
use serde_json::{Value, json};

#[test]
fn text_signature_v1_matches_the_public_json_shape() {
    let signature = TextSignatureV1::new("msg_123", Some(TextSignaturePhase::FinalAnswer));
    assert_eq!(
        serde_json::to_string(&signature).unwrap(),
        r#"{"v":1,"id":"msg_123","phase":"final_answer"}"#
    );
    assert_eq!(
        serde_json::from_str::<TextSignatureV1>(r#"{"v":1,"id":"msg_123"}"#).unwrap(),
        TextSignatureV1::new("msg_123", None)
    );
}

fn builtin(provider: &str, id: &str) -> Model {
    pi_core::ai::providers::builtin::get_builtin_model(provider, id)
        .unwrap_or_else(|| panic!("missing model {provider}/{id}"))
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

/// The hand-built model fixture from the namespace/terminal-event suites.
fn responses_test_model(id: &str, name: &str) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        context_window: 400_000,
        max_tokens: 128_000,
        ..Default::default()
    }
}

fn create_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        timestamp: 1_000,
        ..Default::default()
    }
}

fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// Mock transport standing in for the TS `vi.spyOn(globalThis, "fetch")` and
/// SDK mocks: records the outgoing request and answers with canned SSE data.
struct CaptureFetch {
    status: u16,
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl CaptureFetch {
    fn sse(body: String) -> Arc<Self> {
        Arc::new(Self {
            status: 200,
            body,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn error(status: u16, body: String) -> Arc<Self> {
        Arc::new(Self {
            status,
            body,
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
}

impl HttpFetch for CaptureFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = self.body.clone();
        let status = self.status;
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
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

/// The `captureOpenAIResponseHeaders` fixture context.
fn affinity_context() -> Context {
    Context {
        system_prompt: Some("sys".to_string()),
        messages: vec![user_message("hi")],
        ..Default::default()
    }
}

/// Runs the provider against the mock transport, preserving any scoped env
/// configured on the options while injecting the mock fetch.
async fn capture_responses_request_with(
    model: &Model,
    context: &Context,
    options: OpenAIResponsesOptions,
    fetch: Arc<CaptureFetch>,
) -> AssistantMessage {
    let env = options.base.base.env.clone();
    let headers = options.base.base.headers.clone();
    let options = OpenAIResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test-key".to_string()),
                fetch: Some(fetch),
                env,
                headers,
                ..Default::default()
            },
            ..options.base
        },
        ..options
    };
    stream_openai_responses(model, context, Some(&options))
        .result()
        .await
}

async fn capture_responses_request(
    model: &Model,
    context: &Context,
    options: OpenAIResponsesOptions,
) -> (HttpRequest, AssistantMessage) {
    let fetch = CaptureFetch::sse("data: [DONE]\n\n".to_string());
    let result = capture_responses_request_with(model, context, options, fetch.clone()).await;
    (fetch.request(), result)
}

/// The `CapturedResponsesPayload` header triple from the TS helper.
struct CapturedAffinity {
    session_id: Option<String>,
    client_request_id: Option<String>,
    x_session_id: Option<String>,
}

async fn capture_openai_response_headers(
    model: &Model,
    options: OpenAIResponsesOptions,
) -> (CapturedAffinity, Value) {
    let (request, _result) = capture_responses_request(model, &affinity_context(), options).await;
    (
        CapturedAffinity {
            session_id: header(&request, "session_id").map(str::to_string),
            client_request_id: header(&request, "x-client-request-id").map(str::to_string),
            x_session_id: header(&request, "x-session-id").map(str::to_string),
        },
        body_of(&request),
    )
}

fn affinity_options(session_id: Option<&str>) -> OpenAIResponsesOptions {
    OpenAIResponsesOptions {
        base: StreamOptions {
            session_id: session_id.map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn openai_tool_call_providers() -> BTreeSet<String> {
    ["openai", "openai-codex", "opencode"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn tool_arguments(value: Value) -> pi_core::ai::types::ToolCallArguments {
    value.as_object().expect("object").clone()
}

fn assistant_message(
    content: Vec<AssistantContent>,
    api: &str,
    provider: &str,
    model: &str,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content,
        api: api.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        usage: Usage::default(),
        stop_reason,
        timestamp: 1_000,
        ..Default::default()
    }
}

/// Drains the events pushed during `processResponsesStream` and returns the
/// stop reasons observed on every event carrying a partial message (the TS
/// suites wrap `stream.push` to record these).
async fn partial_stop_reasons_after(
    events: Vec<Value>,
    output: &mut AssistantMessage,
    model: &Model,
    options: Option<&ResponsesStreamOptions>,
) -> Result<Vec<StopReason>, String> {
    let stream = create_assistant_message_event_stream();
    process_responses_stream(
        futures::stream::iter(events.into_iter().map(Ok::<Value, String>)),
        output,
        &stream,
        model,
        options,
    )
    .await?;
    stream.end(None);
    let pushed = collect_events(&stream).await;
    Ok(pushed
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => Some(partial.stop_reason),
            _ => None,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// openai-responses-compat.test.ts — "openai-responses provider defaults"

#[tokio::test]
async fn omits_reasoning_when_no_reasoning_is_requested() {
    let model = builtin("github-copilot", "gpt-5-mini");
    let (request, _result) =
        capture_responses_request(&model, &affinity_context(), affinity_options(None)).await;

    let payload = body_of(&request);
    assert!(payload.get("reasoning").is_none());
}

#[tokio::test]
async fn forwards_required_tool_choice() {
    let model = builtin("openai", "gpt-5.4");
    let context = Context {
        messages: vec![user_message("Do not call ping. Respond with text instead.")],
        tools: Some(vec![Tool {
            name: "ping".to_string(),
            description: "Ping".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
            }),
            constrained_sampling: None,
        }]),
        ..Default::default()
    };
    let options = OpenAIResponsesOptions {
        tool_choice: Some(json!("required")),
        ..affinity_options(None)
    };

    let (request, _result) = capture_responses_request(&model, &context, options).await;

    let payload = body_of(&request);
    assert_eq!(payload["tool_choice"], json!("required"));
    let tools = payload["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["name"], json!("ping"));
}

#[tokio::test]
async fn sets_strict_mode_explicitly_for_cloudflare_openai_responses_tools() {
    let model = builtin("cloudflare-ai-gateway", "gpt-5.6-sol");
    let context = Context {
        messages: vec![user_message("Use a tool.")],
        tools: Some(vec![
            Tool {
                name: "ordinary".to_string(),
                description: "An ordinary tool".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "number" },
                    },
                    "required": ["path"],
                }),
                constrained_sampling: None,
            },
            Tool {
                name: "constrained".to_string(),
                description: "A constrained tool".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"],
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

    let (request, _result) =
        capture_responses_request(&model, &context, affinity_options(None)).await;

    assert_eq!(
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_strict_mode),
        Some(true)
    );
    let payload = body_of(&request);
    let tools = payload["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["name"], json!("ordinary"));
    assert_eq!(tools[0]["strict"], json!(false));
    assert_eq!(tools[1]["name"], json!("constrained"));
    assert_eq!(tools[1]["strict"], json!(true));
}

#[tokio::test]
async fn sends_none_reasoning_effort_for_openai_models_when_no_reasoning_is_requested() {
    let model_ids = [
        "gpt-5.1",
        "gpt-5.2",
        "gpt-5.3-codex",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.4-nano",
        "gpt-5.5",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ];
    for model_id in model_ids {
        let model = builtin("openai", model_id);
        let (request, _result) =
            capture_responses_request(&model, &affinity_context(), affinity_options(None)).await;

        let payload = body_of(&request);
        assert_eq!(
            payload["reasoning"],
            json!({ "effort": "none" }),
            "reasoning effort for {model_id}"
        );
    }
}

#[tokio::test]
async fn omits_reasoning_effort_for_openai_models_when_off_is_unsupported() {
    let model_ids = [
        "gpt-5",
        "gpt-5-mini",
        "gpt-5-nano",
        "gpt-5-pro",
        "gpt-5.2-pro",
        "gpt-5.4-pro",
        "gpt-5.5-pro",
    ];
    for model_id in model_ids {
        let model = builtin("openai", model_id);
        let (request, _result) =
            capture_responses_request(&model, &affinity_context(), affinity_options(None)).await;

        let payload = body_of(&request);
        assert!(
            payload.get("reasoning").is_none(),
            "reasoning for {model_id}"
        );
    }
}

#[tokio::test]
async fn sets_cache_affinity_headers_for_official_openai_responses_requests_with_a_session_id() {
    let model = builtin("openai", "gpt-5.4");
    let (captured, _payload) =
        capture_openai_response_headers(&model, affinity_options(Some("session-123"))).await;

    assert_eq!(captured.session_id.as_deref(), Some("session-123"));
    assert_eq!(captured.client_request_id.as_deref(), Some("session-123"));
}

#[tokio::test]
async fn clamps_prompt_cache_key_to_openais_64_character_limit() {
    let model = builtin("openai", "gpt-5.4");
    let session_id = "x".repeat(67);
    let options = OpenAIResponsesOptions {
        base: StreamOptions {
            session_id: Some(session_id),
            ..Default::default()
        },
        ..Default::default()
    };
    let (request, _result) = capture_responses_request(&model, &affinity_context(), options).await;

    let payload = body_of(&request);
    assert_eq!(payload["prompt_cache_key"], json!("x".repeat(64)));
}

#[tokio::test]
async fn sets_cache_affinity_headers_for_proxy_openai_responses_requests_with_a_session_id() {
    let mut proxy_model = builtin("openai", "gpt-5.4");
    proxy_model.provider = "opencode".to_string();
    proxy_model.base_url = "https://proxy.example.com/v1".to_string();
    let (captured, _payload) =
        capture_openai_response_headers(&proxy_model, affinity_options(Some("session-123"))).await;

    assert_eq!(captured.session_id.as_deref(), Some("session-123"));
    assert_eq!(captured.client_request_id.as_deref(), Some("session-123"));
}

#[tokio::test]
async fn uses_openrouter_session_affinity_header_when_configured() {
    let mut proxy_model = builtin("openai", "gpt-5.4");
    proxy_model.provider = "proxy".to_string();
    proxy_model.base_url = "https://proxy.example.com/v1".to_string();
    proxy_model.compat = Some(ModelCompat {
        session_affinity_format: Some(SessionAffinityFormat::Openrouter),
        ..Default::default()
    });
    let (captured, payload) =
        capture_openai_response_headers(&proxy_model, affinity_options(Some("session-proxy")))
            .await;

    assert_eq!(captured.session_id, None);
    assert_eq!(captured.client_request_id, None);
    assert_eq!(captured.x_session_id.as_deref(), Some("session-proxy"));
    assert!(payload.get("session_id").is_none());
    assert_eq!(payload["prompt_cache_key"], json!("session-proxy"));
}

#[tokio::test]
async fn auto_detects_openrouter_session_affinity_header_for_openrouter_responses_endpoints() {
    let mut open_router_model = builtin("openai", "gpt-5.4");
    open_router_model.provider = "openrouter".to_string();
    open_router_model.base_url = "https://openrouter.ai/api/v1".to_string();
    let (captured, payload) = capture_openai_response_headers(
        &open_router_model,
        affinity_options(Some("session-openrouter")),
    )
    .await;

    assert_eq!(captured.session_id, None);
    assert_eq!(captured.client_request_id, None);
    assert_eq!(captured.x_session_id.as_deref(), Some("session-openrouter"));
    assert!(payload.get("session_id").is_none());
    assert_eq!(payload["prompt_cache_key"], json!("session-openrouter"));
}

#[tokio::test]
async fn uses_openai_no_session_format_when_configured() {
    let mut proxy_model = builtin("openai", "gpt-5.4");
    proxy_model.provider = "proxy".to_string();
    proxy_model.base_url = "https://proxy.example.com/v1".to_string();
    proxy_model.compat = Some(ModelCompat {
        session_affinity_format: Some(SessionAffinityFormat::OpenaiNosession),
        ..Default::default()
    });
    let (captured, payload) =
        capture_openai_response_headers(&proxy_model, affinity_options(Some("session-proxy")))
            .await;

    assert_eq!(captured.session_id, None);
    assert_eq!(captured.client_request_id.as_deref(), Some("session-proxy"));
    assert_eq!(captured.x_session_id, None);
    assert!(payload.get("session_id").is_none());
    assert_eq!(payload["prompt_cache_key"], json!("session-proxy"));
}

#[tokio::test]
async fn uses_openai_no_session_format_for_opencode_responses_models() {
    let model = builtin("opencode", "gpt-5.4");
    let (captured, payload) =
        capture_openai_response_headers(&model, affinity_options(Some("session-opencode"))).await;

    assert_eq!(
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.session_affinity_format),
        Some(SessionAffinityFormat::OpenaiNosession)
    );
    assert_eq!(captured.session_id, None);
    assert_eq!(
        captured.client_request_id.as_deref(),
        Some("session-opencode")
    );
    assert_eq!(captured.x_session_id, None);
    assert_eq!(payload["prompt_cache_key"], json!("session-opencode"));
}

#[tokio::test]
async fn can_omit_openai_session_id_header_while_preserving_other_affinity_data() {
    let mut proxy_model = builtin("openai", "gpt-5.4");
    proxy_model.provider = "opencode".to_string();
    proxy_model.base_url = "https://proxy.example.com/v1".to_string();
    proxy_model.compat = Some(ModelCompat {
        session_affinity_format: Some(SessionAffinityFormat::OpenaiNosession),
        ..Default::default()
    });
    let (captured, payload) =
        capture_openai_response_headers(&proxy_model, affinity_options(Some("session-123"))).await;

    assert_eq!(captured.session_id, None);
    assert_eq!(captured.client_request_id.as_deref(), Some("session-123"));
    assert_eq!(payload["prompt_cache_key"], json!("session-123"));
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_openai_cache_affinity_headers() {
    let model = builtin("openai", "gpt-5.4");
    let options = OpenAIResponsesOptions {
        base: StreamOptions {
            session_id: Some("session-123".to_string()),
            base: ProviderRequestOptions {
                headers: Some(
                    [
                        (
                            "session_id".to_string(),
                            Some("override-session".to_string()),
                        ),
                        (
                            "x-client-request-id".to_string(),
                            Some("override-request".to_string()),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let (captured, _payload) = capture_openai_response_headers(&model, options).await;

    assert_eq!(captured.session_id.as_deref(), Some("override-session"));
    assert_eq!(
        captured.client_request_id.as_deref(),
        Some("override-request")
    );
}

#[tokio::test]
async fn omits_openai_cache_affinity_headers_when_cache_retention_is_none() {
    let model = builtin("openai", "gpt-5.4");
    let options = OpenAIResponsesOptions {
        base: StreamOptions {
            session_id: Some("session-123".to_string()),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
        ..Default::default()
    };
    let (captured, _payload) = capture_openai_response_headers(&model, options).await;

    assert_eq!(captured.session_id, None);
    assert_eq!(captured.client_request_id, None);
}

async fn assert_service_tier_pricing(model_id: &str, service_tier: &str, multiplier: f64) {
    let model = builtin("openai", model_id);
    let token_count = 100_000_u64;
    let token_scale = token_count as f64 / 1_000_000.0;
    let sse = sse_body(&[json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "service_tier": service_tier,
            "usage": {
                "input_tokens": token_count,
                "output_tokens": token_count,
                "total_tokens": token_count * 2,
                "input_tokens_details": { "cached_tokens": 0 },
            },
        },
    })]);

    let options = OpenAIResponsesOptions {
        service_tier: Some(service_tier.to_string()),
        ..affinity_options(None)
    };
    let fetch = CaptureFetch::sse(sse);
    let result = capture_responses_request_with(&model, &affinity_context(), options, fetch).await;

    let cost_input: f64 = model.cost.rates.input.into();
    let cost_output: f64 = model.cost.rates.output.into();
    assert_eq!(
        f64::from(result.usage.cost.input),
        cost_input * multiplier * token_scale
    );
    assert_eq!(
        f64::from(result.usage.cost.output),
        cost_output * multiplier * token_scale
    );
    assert_eq!(
        f64::from(result.usage.cost.total),
        (cost_input + cost_output) * multiplier * token_scale
    );
}

#[tokio::test]
async fn applies_gpt_5_4_priority_service_tier_cost_multiplier() {
    assert_service_tier_pricing("gpt-5.4", "priority", 2.0).await;
}

#[tokio::test]
async fn applies_gpt_5_5_priority_service_tier_cost_multiplier() {
    assert_service_tier_pricing("gpt-5.5", "priority", 2.5).await;
}

#[tokio::test]
async fn applies_gpt_5_5_flex_service_tier_cost_multiplier() {
    assert_service_tier_pricing("gpt-5.5", "flex", 0.5).await;
}

// ---------------------------------------------------------------------------
// openai-responses-empty-tool-result.test.ts

#[tokio::test]
async fn uses_no_tool_output_placeholder_for_empty_tool_results_without_images() {
    let model = builtin("openai", "gpt-4o-mini");
    let assistant = assistant_message(
        vec![AssistantContent::ToolCall(ToolCall {
            content_type: Default::default(),
            id: "tool-1".to_string(),
            name: "bash".to_string(),
            arguments: tool_arguments(json!({ "command": "true" })),
            ..Default::default()
        })],
        &model.api,
        &model.provider,
        &model.id,
        StopReason::ToolUse,
    );
    let tool_result = Message::ToolResult(Box::new(ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: "tool-1".to_string(),
        tool_name: "bash".to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: String::new(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: 2_000,
        ..Default::default()
    }));
    let context = Context {
        messages: vec![
            user_message("Run the command"),
            Message::Assistant(Box::new(assistant)),
            tool_result,
        ],
        ..Default::default()
    };

    let input = convert_responses_messages(&model, &context, &openai_tool_call_providers(), None)
        .expect("conversion");
    let function_call_output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("function_call_output item");

    let output = function_call_output["output"]
        .as_str()
        .expect("output text");
    assert_eq!(output, "(no tool output)");
    assert!(!output.contains("see attached image"));
}

// ---------------------------------------------------------------------------
// openai-responses-foreign-toolcall-id.test.ts

#[tokio::test]
async fn hashes_foreign_copilot_tool_item_ids_into_a_bounded_codex_safe_fc_hash_shape() {
    const COPILOT_RAW_TOOL_CALL_ID: &str = "call_4VnzVawQXPB9MgYib7CiQFEY|I9b95oN1wD/cHXKTw3PpRkL6KkCtzTJhUxMouMWYwHeTo2j3htzfSk7YPx2vifiIM4g3A8XXyOj8q4Bt6SLUG7gqY1E3ELkrkVQNHglRfUmWj84lqxJY+Puieb3VKyX0FB+83TUzn91cDMF/4gzt990IzqVrc+nIb9RRscRD070Du16q1glydVjWR0SBJsE6TbY/esOjFpqplogQqrajm1eI++f3eLi73R6q7hVusY0QbeFySVxABCjhN0lXB04caBe1rzHjYzul6MAXj7uq+0r17VLq+yrtyYhN12wkmFqHeqTyEei6EFPbMy24Nc+IbJlkP0OCg02W+gOnyBFcbi2ctvJFSOhSjt1CqBdqCnnhwUqXjbWiT0wh3DmLScRgTHmGkaI+oAcQQjfic65nxj+TnEkReA==";

    let model = builtin("openai-codex", "gpt-5.5");
    let assistant = assistant_message(
        vec![AssistantContent::ToolCall(ToolCall {
            content_type: Default::default(),
            id: COPILOT_RAW_TOOL_CALL_ID.to_string(),
            name: "edit".to_string(),
            arguments: tool_arguments(json!({ "path": "src/styles/app.css" })),
            ..Default::default()
        })],
        "openai-responses",
        "github-copilot",
        "gpt-5.5",
        StopReason::ToolUse,
    );
    let tool_result = Message::ToolResult(Box::new(ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: COPILOT_RAW_TOOL_CALL_ID.to_string(),
        tool_name: "edit".to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: "ok".to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: 2_000,
        ..Default::default()
    }));
    let context = Context {
        system_prompt: Some("You are concise.".to_string()),
        messages: vec![
            user_message("Use the tool."),
            Message::Assistant(Box::new(assistant)),
            tool_result,
        ],
        ..Default::default()
    };

    let input = convert_responses_messages(&model, &context, &openai_tool_call_providers(), None)
        .expect("conversion");
    let function_call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call item");

    let item_id = COPILOT_RAW_TOOL_CALL_ID
        .split('|')
        .nth(1)
        .expect("item id part");
    let expected_item_id = format!("fc_{}", short_hash(item_id));
    assert_eq!(function_call["id"], json!(expected_item_id));
    assert!(expected_item_id.chars().count() <= 64);
    assert!(expected_item_id.starts_with("fc_"));
    assert!(
        expected_item_id[3..]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric())
    );
}

// ---------------------------------------------------------------------------
// openai-responses-message-id.test.ts

#[tokio::test]
async fn generates_unique_fallback_message_ids_for_multiple_text_blocks_in_one_assistant_turn() {
    let model = builtin("openai-codex", "gpt-5.5");
    let assistant = assistant_message(
        vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "private reasoning".to_string(),
                ..Default::default()
            }),
            AssistantContent::Text(TextContent {
                text: "visible answer".to_string(),
                ..Default::default()
            }),
        ],
        "anthropic-messages",
        "anthropic",
        "claude-opus-4-8",
        StopReason::Stop,
    );
    let context = Context {
        system_prompt: Some("You are concise.".to_string()),
        messages: vec![
            user_message("hello"),
            Message::Assistant(Box::new(assistant)),
        ],
        ..Default::default()
    };

    let input = convert_responses_messages(&model, &context, &openai_tool_call_providers(), None)
        .expect("conversion");
    let message_ids: Vec<String> = input
        .iter()
        .filter(|item| item["type"] == "message")
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    assert_eq!(message_ids, vec!["msg_pi_1", "msg_pi_1_1"]);
    let unique: BTreeSet<&String> = message_ids.iter().collect();
    assert_eq!(unique.len(), message_ids.len());
}

// ---------------------------------------------------------------------------
// openai-responses-namespace.test.ts — "OpenAI Responses tool-call namespaces"

fn namespace_model() -> Model {
    responses_test_model("gpt-5.4", "GPT-5.4")
}

fn function_call_events() -> Vec<Value> {
    vec![
        json!({
            "type": "response.output_item.added",
            "sequence_number": 0,
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_test",
                "call_id": "call_test",
                "name": "lookup",
                "arguments": "",
            },
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_test",
                "call_id": "call_test",
                "name": "lookup",
                "arguments": "{\"value\":\"hello\"}",
                "namespace": "dynamic_tools",
            },
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": { "id": "resp_test", "status": "completed" },
        }),
    ]
}

fn custom_tool_call_events() -> Vec<Value> {
    vec![
        json!({
            "type": "response.output_item.added",
            "sequence_number": 0,
            "output_index": 0,
            "item": {
                "type": "custom_tool_call",
                "id": "ctc_test",
                "call_id": "call_test",
                "name": "query",
                "input": "",
            },
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "custom_tool_call",
                "id": "ctc_test",
                "call_id": "call_test",
                "name": "query",
                "input": "hello",
                "namespace": "dynamic_tools",
            },
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": { "id": "resp_test", "status": "completed" },
        }),
    ]
}

fn get_tool_call(output: &AssistantMessage) -> ToolCall {
    match output.content.first() {
        Some(AssistantContent::ToolCall(tool_call)) => tool_call.clone(),
        _ => panic!("Expected toolCall block"),
    }
}

fn replay_context(output: AssistantMessage) -> Context {
    Context {
        messages: vec![Message::Assistant(Box::new(output))],
        ..Default::default()
    }
}

#[tokio::test]
async fn round_trips_a_function_namespace_received_only_on_output_item_done() {
    let model = namespace_model();
    let mut output = create_output(&model);
    let stream = create_assistant_message_event_stream();
    process_responses_stream(
        futures::stream::iter(function_call_events().into_iter().map(Ok::<Value, String>)),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processed");
    stream.end(None);

    let tool_call = get_tool_call(&output);
    assert_eq!(tool_call.id, "call_test|fc_test");
    assert_eq!(tool_call.name, "lookup");
    assert_eq!(
        tool_call.arguments,
        tool_arguments(json!({ "value": "hello" }))
    );
    assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));

    let replayed = convert_responses_messages(
        &model,
        &replay_context(output),
        &["openai".to_string()].into_iter().collect(),
        None,
    )
    .expect("conversion");
    let function_call = replayed
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call item");
    assert_eq!(function_call["type"], json!("function_call"));
    assert_eq!(function_call["id"], json!("fc_test"));
    assert_eq!(function_call["call_id"], json!("call_test"));
    assert_eq!(function_call["name"], json!("lookup"));
    assert_eq!(function_call["arguments"], json!("{\"value\":\"hello\"}"));
    assert_eq!(function_call["namespace"], json!("dynamic_tools"));
}

#[tokio::test]
async fn round_trips_a_custom_tool_namespace_received_only_on_output_item_done() {
    let model = namespace_model();
    let mut grammar_tool_input_properties = std::collections::BTreeMap::new();
    grammar_tool_input_properties.insert("query".to_string(), "input".to_string());

    let mut output = create_output(&model);
    let stream = create_assistant_message_event_stream();
    let stream_options = ResponsesStreamOptions {
        grammar_tool_input_properties: Some(grammar_tool_input_properties.clone()),
        ..Default::default()
    };
    process_responses_stream(
        futures::stream::iter(
            custom_tool_call_events()
                .into_iter()
                .map(Ok::<Value, String>),
        ),
        &mut output,
        &stream,
        &model,
        Some(&stream_options),
    )
    .await
    .expect("stream processed");
    stream.end(None);

    let tool_call = get_tool_call(&output);
    assert_eq!(tool_call.id, "call_test|ctc_test");
    assert_eq!(tool_call.name, "query");
    assert_eq!(
        tool_call.arguments,
        tool_arguments(json!({ "input": "hello" }))
    );
    assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));

    let replayed = convert_responses_messages(
        &model,
        &replay_context(output),
        &["openai".to_string()].into_iter().collect(),
        Some(&ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(&grammar_tool_input_properties),
            ..Default::default()
        }),
    )
    .expect("conversion");
    let custom_tool_call = replayed
        .iter()
        .find(|item| item["type"] == "custom_tool_call")
        .expect("custom_tool_call item");
    assert_eq!(custom_tool_call["type"], json!("custom_tool_call"));
    assert_eq!(custom_tool_call["id"], json!("ctc_test"));
    assert_eq!(custom_tool_call["call_id"], json!("call_test"));
    assert_eq!(custom_tool_call["name"], json!("query"));
    assert_eq!(custom_tool_call["input"], json!("hello"));
    assert_eq!(custom_tool_call["namespace"], json!("dynamic_tools"));
}

#[tokio::test]
async fn drops_namespaces_when_the_target_cannot_replay_their_load_items() {
    let model = namespace_model();
    let mut output = create_output(&model);
    output.content.push(AssistantContent::ToolCall(ToolCall {
        content_type: Default::default(),
        id: "call_function|fc_test".to_string(),
        name: "lookup".to_string(),
        arguments: tool_arguments(json!({ "value": "hello" })),
        namespace: Some("dynamic_tools".to_string()),
        ..Default::default()
    }));
    output.content.push(AssistantContent::ToolCall(ToolCall {
        content_type: Default::default(),
        id: "call_custom|ctc_test".to_string(),
        name: "query".to_string(),
        arguments: tool_arguments(json!({ "input": "hello" })),
        namespace: Some("dynamic_tools".to_string()),
        ..Default::default()
    }));

    let mut target_models = vec![
        Model {
            id: "gpt-5.2".to_string(),
            name: "GPT-5.2".to_string(),
            ..model.clone()
        },
        Model {
            provider: "azure-openai-responses".to_string(),
            ..model.clone()
        },
        Model {
            api: "openai-codex-responses".to_string(),
            provider: "openai-codex".to_string(),
            id: "gpt-5.3-codex-spark".to_string(),
            name: "GPT-5.3 Codex Spark".to_string(),
            ..model.clone()
        },
    ];

    let mut grammar_tool_input_properties = std::collections::BTreeMap::new();
    grammar_tool_input_properties.insert("query".to_string(), "input".to_string());

    for target_model in &mut target_models {
        let replayed = convert_responses_messages(
            target_model,
            &replay_context(output.clone()),
            &["openai".to_string()].into_iter().collect(),
            Some(&ConvertResponsesMessagesOptions {
                grammar_tool_input_properties: Some(&grammar_tool_input_properties),
                ..Default::default()
            }),
        )
        .expect("conversion");
        let function_call = replayed
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("function_call item");
        let custom_tool_call = replayed
            .iter()
            .find(|item| item["type"] == "custom_tool_call")
            .expect("custom_tool_call item");
        assert!(
            function_call.get("namespace").is_none(),
            "namespace dropped for {}",
            target_model.id
        );
        assert!(
            custom_tool_call.get("namespace").is_none(),
            "namespace dropped for {}",
            target_model.id
        );
    }
}

#[tokio::test]
async fn does_not_add_a_namespace_to_ordinary_function_calls() {
    let model = namespace_model();
    let mut output = create_output(&model);
    output.content.push(AssistantContent::ToolCall(ToolCall {
        content_type: Default::default(),
        id: "call_test|fc_test".to_string(),
        name: "lookup".to_string(),
        arguments: tool_arguments(json!({ "value": "hello" })),
        ..Default::default()
    }));

    let replayed = convert_responses_messages(
        &model,
        &replay_context(output),
        &["openai".to_string()].into_iter().collect(),
        None,
    )
    .expect("conversion");
    let function_call = replayed
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call item");
    assert!(function_call.get("namespace").is_none());
}

// ---------------------------------------------------------------------------
// openai-responses-partial-json-cleanup.test.ts

#[tokio::test]
async fn removes_partial_json_from_persisted_tool_call_blocks_at_output_item_done() {
    let model = responses_test_model("gpt-5-mini", "GPT-5 Mini");
    let arguments_json = r#"{"path":"README.md","content":"updated"}"#;
    let events = vec![
        json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": "fc_test",
                "call_id": "call_test",
                "name": "edit",
                "arguments": "",
            },
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "delta": "{\"path\":\"README.md\"",
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "delta": ",\"content\":\"updated\"}",
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "arguments": arguments_json,
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": "fc_test",
                "call_id": "call_test",
                "name": "edit",
                "arguments": arguments_json,
            },
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 5,
            "response": { "id": "resp_test", "status": "completed" },
        }),
    ];

    let mut output = create_output(&model);
    let stream = create_assistant_message_event_stream();
    process_responses_stream(
        futures::stream::iter(events.into_iter().map(Ok::<Value, String>)),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processed");
    stream.end(None);
    let pushed = collect_events(&stream).await;

    assert_eq!(output.content.len(), 1);
    let persisted_tool_call = get_tool_call(&output);
    assert_eq!(
        persisted_tool_call.arguments,
        tool_arguments(json!({ "path": "README.md", "content": "updated" }))
    );

    let tool_call_end = pushed
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolcallEnd { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        })
        .expect("toolcall_end event");
    assert_eq!(tool_call_end, persisted_tool_call);
}

// ---------------------------------------------------------------------------
// openai-responses-terminal-event.test.ts — "OpenAI Responses terminal event
// handling"

fn terminal_model() -> Model {
    responses_test_model("gpt-5-mini", "GPT-5 Mini")
}

fn early_eof_events() -> Vec<Value> {
    vec![
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": { "id": "resp_early_eof" },
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": { "type": "reasoning", "id": "rs_early_eof", "summary": [] },
        }),
        json!({
            "type": "response.reasoning_text.delta",
            "sequence_number": 2,
            "output_index": 0,
            "content_index": 0,
            "item_id": "rs_early_eof",
            "delta": "partial reasoning before the stream ends",
        }),
    ]
}

fn completed_events() -> Vec<Value> {
    vec![json!({
        "type": "response.completed",
        "sequence_number": 0,
        "response": {
            "id": "resp_completed",
            "status": "completed",
            "usage": {
                "input_tokens": 20,
                "output_tokens": 7,
                "total_tokens": 27,
                "input_tokens_details": { "cached_tokens": 2, "cache_write_tokens": 3 },
            },
        },
    })]
}

fn incomplete_events(reason: &str) -> Vec<Value> {
    vec![json!({
        "type": "response.incomplete",
        "sequence_number": 0,
        "response": {
            "id": "resp_incomplete",
            "status": "incomplete",
            "incomplete_details": { "reason": reason },
            "usage": {
                "input_tokens": 30,
                "output_tokens": 12,
                "total_tokens": 42,
                "input_tokens_details": { "cached_tokens": 5 },
            },
        },
    })]
}

fn failed_events() -> Vec<Value> {
    vec![json!({
        "type": "response.failed",
        "sequence_number": 0,
        "response": {
            "id": "resp_failed",
            "status": "failed",
            "error": { "code": "server_error", "message": "boom" },
        },
    })]
}

fn phased_message_events(phases: [&str; 2], terminal_incomplete: bool) -> Vec<Value> {
    let mut events = vec![
        json!({
            "type": "response.output_item.added",
            "sequence_number": 0,
            "output_index": 0,
            "item": {
                "type": "message",
                "id": "msg_phase",
                "role": "assistant",
                "status": "in_progress",
                "content": [],
                "phase": phases[0],
            },
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "message",
                "id": "msg_phase",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "answer", "annotations": [] }],
                "phase": phases[1],
            },
        }),
    ];
    if terminal_incomplete {
        events.push(json!({
            "type": "response.incomplete",
            "sequence_number": 2,
            "response": {
                "id": "resp_phase",
                "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" },
            },
        }));
    } else {
        events.push(json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": { "id": "resp_phase", "status": "completed" },
        }));
    }
    events
}

async fn process_ignoring_events(
    events: Vec<Value>,
    output: &mut AssistantMessage,
    model: &Model,
) -> Result<(), String> {
    let stream = create_assistant_message_event_stream();
    let result = process_responses_stream(
        futures::stream::iter(events.into_iter().map(Ok::<Value, String>)),
        output,
        &stream,
        model,
        None,
    )
    .await;
    stream.end(None);
    let _ = collect_events(&stream).await;
    result
}

#[tokio::test]
async fn rejects_streams_that_end_before_a_terminal_response_event() {
    let model = terminal_model();
    let mut output = create_output(&model);

    let error = process_ignoring_events(early_eof_events(), &mut output, &model)
        .await
        .expect_err("stream must fail");
    assert_eq!(
        error,
        "OpenAI Responses stream ended before a terminal response event"
    );
}

#[tokio::test]
async fn emits_an_error_final_result_when_the_wrapper_stream_ends_before_a_terminal_response_event()
{
    let model = terminal_model();
    let context = Context {
        system_prompt: Some(String::new()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
                text: "hi".to_string(),
                ..Default::default()
            })]),
            timestamp: 0,
        })],
        tools: Some(Vec::new()),
    };
    let fetch = CaptureFetch::sse(sse_body(&[
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": { "id": "resp_wrapper_early_eof" },
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": { "type": "reasoning", "id": "rs_wrapper_early_eof", "summary": [] },
        }),
        json!({
            "type": "response.reasoning_text.delta",
            "sequence_number": 2,
            "output_index": 0,
            "content_index": 0,
            "item_id": "rs_wrapper_early_eof",
            "delta": "partial reasoning before the wrapper stream ends",
        }),
    ]));
    let stream = stream_openai_responses(
        &model,
        &context,
        Some(&OpenAIResponsesOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("test".to_string()),
                    fetch: Some(fetch),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
    );

    let mut initial_stop_reason = None;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if let AssistantMessageEvent::Start { partial } = &event {
            initial_stop_reason = Some(partial.stop_reason);
        }
        events.push(event);
    }
    let result = stream.result().await;

    assert_eq!(initial_stop_reason, Some(StopReason::Pending));
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("OpenAI Responses stream ended before a terminal response event")
    );
}

#[tokio::test]
async fn tracks_commentary_commentary_message_phases() {
    let model = terminal_model();
    let mut output = create_output(&model);
    let observed = partial_stop_reasons_after(
        phased_message_events(["commentary", "commentary"], false),
        &mut output,
        &model,
        None,
    )
    .await
    .expect("stream processed");

    assert_eq!(observed, vec![StopReason::Pending, StopReason::Pending]);
    assert_eq!(output.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn tracks_final_answer_final_answer_message_phases() {
    let model = terminal_model();
    let mut output = create_output(&model);
    let observed = partial_stop_reasons_after(
        phased_message_events(["final_answer", "final_answer"], false),
        &mut output,
        &model,
        None,
    )
    .await
    .expect("stream processed");

    assert_eq!(observed, vec![StopReason::Stop, StopReason::Stop]);
    assert_eq!(output.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn tracks_commentary_final_answer_message_phases() {
    let model = terminal_model();
    let mut output = create_output(&model);
    let observed = partial_stop_reasons_after(
        phased_message_events(["commentary", "final_answer"], false),
        &mut output,
        &model,
        None,
    )
    .await
    .expect("stream processed");

    assert_eq!(observed, vec![StopReason::Pending, StopReason::Stop]);
    assert_eq!(output.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn replaces_a_provisional_final_answer_stop_with_an_incomplete_terminal_reason() {
    let model = terminal_model();
    let mut output = create_output(&model);
    let observed = partial_stop_reasons_after(
        phased_message_events(["final_answer", "final_answer"], true),
        &mut output,
        &model,
        None,
    )
    .await
    .expect("stream processed");

    assert_eq!(observed, vec![StopReason::Stop, StopReason::Stop]);
    assert_eq!(output.stop_reason, StopReason::Length);
}

#[tokio::test]
async fn finalizes_completed_terminal_events_as_stop() {
    let model = terminal_model();
    let mut output = create_output(&model);
    process_ignoring_events(completed_events(), &mut output, &model)
        .await
        .expect("stream processed");

    assert_eq!(output.response_id.as_deref(), Some("resp_completed"));
    assert_eq!(output.stop_reason, StopReason::Stop);
    assert_eq!(output.raw_stop_reason.as_deref(), Some("completed"));
    assert_eq!(output.usage.input, 15);
    assert_eq!(output.usage.output, 7);
    assert_eq!(output.usage.cache_read, 2);
    assert_eq!(output.usage.cache_write, 3);
    assert_eq!(output.usage.total_tokens, 27);
}

#[tokio::test]
async fn finalizes_incomplete_terminal_events_as_length_stops() {
    let model = terminal_model();
    let mut output = create_output(&model);
    process_ignoring_events(incomplete_events("max_output_tokens"), &mut output, &model)
        .await
        .expect("stream processed");

    assert_eq!(output.response_id.as_deref(), Some("resp_incomplete"));
    assert_eq!(output.stop_reason, StopReason::Length);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("incomplete.max_output_tokens")
    );
    assert_eq!(output.usage.input, 25);
    assert_eq!(output.usage.output, 12);
    assert_eq!(output.usage.cache_read, 5);
    assert_eq!(output.usage.cache_write, 0);
    assert_eq!(output.usage.total_tokens, 42);
}

#[tokio::test]
async fn finalizes_content_filtered_incomplete_responses_as_non_retryable_errors() {
    let model = terminal_model();
    let mut output = create_output(&model);
    process_ignoring_events(incomplete_events("content_filter"), &mut output, &model)
        .await
        .expect("stream processed");

    assert_eq!(output.stop_reason, StopReason::Error);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("incomplete.content_filter")
    );
    assert_eq!(
        output.error_message.as_deref(),
        Some("Response incomplete: content_filter")
    );
}

#[tokio::test]
async fn preserves_unknown_provider_incomplete_reasons_as_non_retryable_errors() {
    let model = terminal_model();
    let mut output = create_output(&model);
    process_ignoring_events(incomplete_events("max_time_limit"), &mut output, &model)
        .await
        .expect("stream processed");

    assert_eq!(output.stop_reason, StopReason::Error);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("incomplete.max_time_limit")
    );
    assert_eq!(
        output.error_message.as_deref(),
        Some("Response incomplete: max_time_limit")
    );
}

#[tokio::test]
async fn rejects_failed_terminal_events_with_the_provider_error() {
    let model = terminal_model();
    let mut output = create_output(&model);

    let error = process_ignoring_events(failed_events(), &mut output, &model)
        .await
        .expect_err("stream must fail");
    assert_eq!(error, "server_error: boom");
    assert_eq!(output.raw_stop_reason.as_deref(), Some("failed"));
}

// ---------------------------------------------------------------------------
// cache-retention.test.ts — "Cache Retention (PI_CACHE_RETENTION)", the
// OpenAI Responses describe

fn retention_context() -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message("Hello")],
        ..Default::default()
    }
}

fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn retention_options(
    env: Option<ProviderEnv>,
    cache_retention: Option<CacheRetention>,
    session_id: Option<&str>,
) -> OpenAIResponsesOptions {
    OpenAIResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env,
                ..Default::default()
            },
            cache_retention,
            session_id: session_id.map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn capture_retention_payload(model: &Model, options: OpenAIResponsesOptions) -> Value {
    let (request, _result) = capture_responses_request(model, &retention_context(), options).await;
    body_of(&request)
}

#[tokio::test]
async fn does_not_set_prompt_cache_retention_when_pi_cache_retention_is_not_set() {
    // The TS case is live-gated (`skipIf(!OPENAI_API_KEY)`); the offline Rust
    // port injects an empty scoped env and the mock transport.
    let model = builtin("openai", "gpt-4o-mini");
    let payload = capture_retention_payload(
        &model,
        retention_options(Some(ProviderEnv::new()), None, None),
    )
    .await;

    assert!(payload.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn sets_prompt_cache_retention_to_24h_when_pi_cache_retention_is_long() {
    // The TS case is live-gated (`skipIf(!OPENAI_API_KEY)`); the offline Rust
    // port injects the scoped env the TS test set on process.env.
    let model = builtin("openai", "gpt-4o-mini");
    let payload = capture_retention_payload(
        &model,
        retention_options(
            Some(scoped_env(&[("PI_CACHE_RETENTION", "long")])),
            None,
            None,
        ),
    )
    .await;

    assert_eq!(payload["prompt_cache_retention"], json!("24h"));
}

#[tokio::test]
async fn sets_prompt_cache_retention_for_non_api_openai_com_baseurl_by_default() {
    let mut proxy_model = builtin("openai", "gpt-4o-mini");
    proxy_model.base_url = "https://my-proxy.example.com/v1".to_string();
    let payload = capture_retention_payload(
        &proxy_model,
        retention_options(
            Some(scoped_env(&[("PI_CACHE_RETENTION", "long")])),
            None,
            None,
        ),
    )
    .await;

    assert_eq!(payload["prompt_cache_retention"], json!("24h"));
}

#[tokio::test]
async fn omits_prompt_cache_retention_when_supports_long_cache_retention_is_false() {
    let mut model = builtin("openai", "gpt-4o-mini");
    model.compat = Some(ModelCompat {
        supports_long_cache_retention: Some(false),
        ..Default::default()
    });
    let payload = capture_retention_payload(
        &model,
        retention_options(
            None,
            Some(CacheRetention::Long),
            Some("session-compat-false"),
        ),
    )
    .await;

    assert!(payload.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn omits_prompt_cache_key_and_disables_implicit_writes_when_cache_retention_is_none() {
    let model = builtin("openai", "gpt-5.6-sol");
    let payload = capture_retention_payload(
        &model,
        retention_options(None, Some(CacheRetention::None), Some("session-1")),
    )
    .await;

    assert!(payload.get("prompt_cache_key").is_none());
    assert!(payload.get("prompt_cache_retention").is_none());
    assert_eq!(
        payload["prompt_cache_options"],
        json!({ "mode": "explicit" })
    );
}

#[tokio::test]
async fn omits_prompt_cache_options_for_models_that_reject_it() {
    let model = builtin("openai", "gpt-4o-mini");
    let payload = capture_retention_payload(
        &model,
        retention_options(None, Some(CacheRetention::None), Some("session-1")),
    )
    .await;

    assert!(payload.get("prompt_cache_key").is_none());
    assert!(payload.get("prompt_cache_options").is_none());
}

#[tokio::test]
async fn sets_prompt_cache_retention_when_cache_retention_is_long() {
    let model = builtin("openai", "gpt-4o-mini");
    let payload = capture_retention_payload(
        &model,
        retention_options(None, Some(CacheRetention::Long), Some("session-2")),
    )
    .await;

    assert_eq!(payload["prompt_cache_key"], json!("session-2"));
    assert_eq!(payload["prompt_cache_retention"], json!("24h"));
}

// ---------------------------------------------------------------------------
// provider-error-body-regression.test.ts — the openai-responses case

#[tokio::test]
async fn openai_responses_status_only_keeps_the_prefix_and_surfaces_the_body() {
    let model = Model {
        id: "gpt-test".to_string(),
        name: "GPT Test".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 1_000,
        max_tokens: 100,
        ..Default::default()
    };
    let context = Context {
        system_prompt: Some(String::new()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
                text: "hi".to_string(),
                ..Default::default()
            })]),
            timestamp: 0,
        })],
        tools: Some(Vec::new()),
    };
    let fetch = CaptureFetch::error(403, r#"{"error":"blocked by gateway WAF"}"#.to_string());
    let result =
        capture_responses_request_with(&model, &context, OpenAIResponsesOptions::default(), fetch)
            .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    let error_message = result.error_message.expect("error message");
    assert!(
        error_message.contains("OpenAI API error (403)"),
        "{error_message}"
    );
    assert!(
        error_message.contains("blocked by gateway WAF"),
        "{error_message}"
    );
}
