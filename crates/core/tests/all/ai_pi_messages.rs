//! Port of `pi-core/ai/test/pi-messages.test.ts` (all 8 cases) plus the
//! pi-messages leg of `pi-core/ai/test/fetch-option.test.ts` ("uses fetch for
//! pi-messages HTTP requests").
//!
//! The TypeScript suite stands up a local HTTP server that records requests
//! and answers with canned SSE frames; the Rust port replaces it with a mock
//! `HttpFetch` transport recording the same observable surface (URL, headers,
//! JSON body). The TS server's response headers and `data:` frames are
//! reproduced verbatim.
//!
//! TS `startServer` notes preserved here:
//! - a non-200 status answers with `content-type: application/json` and the
//!   raw body (`"{}"` when unset);
//! - a 200 answer carries `content-type: text/event-stream`, the extra
//!   headers, and one `data: <json>\n\n` frame per event.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::pi_messages::{
    PiMessagesDoneReason, PiMessagesEvent, PiMessagesOptions, stream, stream_simple,
};
use pi_core::ai::compat;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, Model, ModelCost,
    ModelCostRates, ModelInput, OnResponseCallback, ProviderHeaders, ProviderRequestOptions,
    ProviderResponse, RoleUser, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ToolCall, Usage, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{
    HttpBody, HttpFetch, HttpFetchError, HttpMethod, HttpRequest, HttpResponse,
};
use serde_json::{Value, json};

#[test]
fn pi_messages_event_matches_the_public_wire_shape() {
    let event = PiMessagesEvent::TextEnd {
        content_index: 2,
        content: "done".to_string(),
        content_signature: Some("signature".to_string()),
    };
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({
            "type": "text_end",
            "contentIndex": 2,
            "content": "done",
            "contentSignature": "signature"
        })
    );

    let done: PiMessagesEvent = serde_json::from_value(json!({
        "type": "done",
        "reason": "toolUse",
        "usage": Usage::default()
    }))
    .unwrap();
    assert!(matches!(
        done,
        PiMessagesEvent::Done {
            reason: PiMessagesDoneReason::ToolUse,
            ..
        }
    ));
}

/// The mock transport standing in for the TS tests' HTTP server.
struct PiMessagesServerFetch {
    status: u16,
    raw_body: String,
    headers: Vec<(String, String)>,
    events: Vec<Value>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl PiMessagesServerFetch {
    /// A 200 responder streaming `events` as SSE frames.
    fn new(events: Vec<Value>) -> Self {
        Self {
            status: 200,
            raw_body: "{}".to_string(),
            headers: Vec::new(),
            events,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A non-200 responder answering with a raw JSON error body.
    fn with_status(status: u16, raw_body: &str) -> Self {
        Self {
            status,
            raw_body: raw_body.to_string(),
            headers: Vec::new(),
            events: Vec::new(),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The TS `options.headers` extra response headers.
    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpFetch for PiMessagesServerFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let status = self.status;
        let raw_body = self.raw_body.clone();
        let headers = self.headers.clone();
        let events = self.events.clone();
        Box::pin(async move {
            if status != 200 {
                return Ok(HttpResponse {
                    status,
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                        raw_body,
                    ))])),
                });
            }
            let mut response_headers =
                vec![("content-type".to_string(), "text/event-stream".to_string())];
            response_headers.extend(headers);
            let body = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            Ok(HttpResponse {
                status: 200,
                headers: response_headers,
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

/// The TS suite's `createModel`; the mock transport replaces the server, so
/// the base URL points at a placeholder host (the TS `request.url` assertions
/// check the `/v1/messages` path portion of the same URL).
const BASE_URL: &str = "http://pi-messages.test/v1";

fn create_model(base_url: &str) -> Model {
    Model {
        id: "auto".to_string(),
        name: "Radius Auto".to_string(),
        api: "pi-messages".to_string(),
        provider: "radius".to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: pi_core::ai::types::JsF64(1.0),
                output: pi_core::ai::types::JsF64(2.0),
                cache_read: pi_core::ai::types::JsF64(0.1),
                cache_write: pi_core::ai::types::JsF64(0.2),
            },
            tiers: None,
        },
        context_window: 128_000,
        max_tokens: 16_384,
        ..Default::default()
    }
}

/// The TS suite's shared `context` fixture.
fn context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

/// The TS suite's shared `usage` fixture.
fn usage() -> Value {
    json!({
        "input": 10,
        "output": 5,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 15,
        "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3 },
    })
}

fn options(fetch: Arc<PiMessagesServerFetch>, api_key: &str) -> PiMessagesOptions {
    PiMessagesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(api_key.to_string()),
                fetch: Some(fetch),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn simple_options(fetch: Arc<PiMessagesServerFetch>, api_key: &str) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(api_key.to_string()),
                fetch: Some(fetch),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn headers_from(pairs: &[(&str, &str)]) -> ProviderHeaders {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), Some(value.to_string())))
        .collect()
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn body_of(request: &HttpRequest) -> Value {
    match &request.body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    }
}

/// The TS loop's `"partial" in event` check: every non-terminal event carries
/// the partial assistant message.
fn partial_of(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolcallStart { partial, .. }
        | AssistantMessageEvent::ToolcallDelta { partial, .. }
        | AssistantMessageEvent::ToolcallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

#[tokio::test]
async fn streams_text_and_tool_calls_and_resolves_the_terminal_message() {
    let fetch = Arc::new(PiMessagesServerFetch::new(vec![
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "Hel"}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "lo"}),
        json!({"type": "text_end", "contentIndex": 0, "content": "Hello"}),
        json!({"type": "toolcall_start", "contentIndex": 1, "id": "call_1", "toolName": "read"}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "{\"path\":"}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "\"a.txt\"}"}),
        json!({
            "type": "toolcall_end",
            "contentIndex": 1,
            "toolCall": {"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "a.txt"}},
        }),
        json!({"type": "done", "reason": "toolUse", "usage": usage(), "responseId": "resp_1"}),
    ]));
    let mut stream_options = options(fetch.clone(), "test-key");
    stream_options.base.session_id = Some("session-1".to_string());
    stream_options.base.max_tokens = Some(100);
    stream_options.tool_choice = Some(json!("auto"));
    stream_options.base.base.headers = Some(headers_from(&[("x-custom", "1")]));

    let mut partial_stop_reasons = Vec::new();
    let mut saw_text_delta = false;
    let mut toolcall_ends = 0;
    let event_stream = stream(&create_model(BASE_URL), &context(), Some(&stream_options));
    while let Some(event) = event_stream.next().await {
        if let Some(partial) = partial_of(&event) {
            partial_stop_reasons.push(partial.stop_reason);
        }
        match &event {
            AssistantMessageEvent::TextDelta { .. } => saw_text_delta = true,
            AssistantMessageEvent::ToolcallEnd { .. } => toolcall_ends += 1,
            _ => {}
        }
    }
    let message = event_stream.result().await;

    assert_eq!(partial_stop_reasons.first(), Some(&StopReason::Pending));
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(serde_json::to_value(&message.usage).unwrap(), usage());
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(message.model, "auto");
    assert_eq!(message.provider, "radius");
    assert_eq!(
        message.content,
        vec![
            AssistantContent::Text(TextContent {
                text: "Hello".to_string(),
                ..Default::default()
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "a.txt"}).as_object().cloned().unwrap(),
                ..Default::default()
            }),
        ]
    );
    assert!(saw_text_delta);
    assert_eq!(toolcall_ends, 1);

    let requests = fetch.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.url, "http://pi-messages.test/v1/messages");
    assert_eq!(header(request, "authorization"), Some("Bearer test-key"));
    assert_eq!(header(request, "x-custom"), Some("1"));
    assert_eq!(
        body_of(request),
        json!({
            "model": "auto",
            "context": {
                "messages": [{"role": "user", "content": "Hello", "timestamp": 0}],
            },
            "options": {"maxTokens": 100, "sessionId": "session-1", "toolChoice": "auto"},
        })
    );
}

#[tokio::test]
async fn appends_debug_1_and_reports_response_headers_via_on_response() {
    let fetch = Arc::new(
        PiMessagesServerFetch::new(vec![
            json!({"type": "done", "reason": "stop", "usage": usage()}),
        ])
        .with_header("x-pi-gateway-upstream-provider", "anthropic"),
    );
    let observed: Arc<Mutex<Option<ProviderResponse>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&observed);
    let on_response: OnResponseCallback = Arc::new(move |response, _model| {
        *sink.lock().unwrap() = Some(response.clone());
        Box::pin(async {})
    });
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test-key".to_string()),
                fetch: Some(fetch.clone()),
                on_response: Some(on_response),
                // TS attaches `debug: true` to the streamSimple options object;
                // the Rust shared options carry it in the preserved `extra`
                // fields, which `streamSimple` forwards.
                extra: BTreeMap::from([("debug".to_string(), json!(true))]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let message = stream_simple(&create_model(BASE_URL), &context(), Some(&options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = fetch.requests();
    assert_eq!(
        requests[0].url,
        "http://pi-messages.test/v1/messages?debug=1"
    );
    let observed = observed.lock().unwrap().clone();
    assert_eq!(
        observed
            .expect("onResponse observed the response")
            .headers
            .get("x-pi-gateway-upstream-provider")
            .map(String::as_str),
        Some("anthropic")
    );
}

#[tokio::test]
async fn surfaces_backend_error_responses_with_diagnostics() {
    let fetch = Arc::new(PiMessagesServerFetch::with_status(
        401,
        &json!({"error": {"message": "Token expired", "code": "unauthorized"}}).to_string(),
    ));

    let message = stream(
        &create_model(BASE_URL),
        &context(),
        Some(&options(fetch, "stale")),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.as_deref().unwrap();
    assert!(error_message.contains("401"), "{error_message}");
    assert!(error_message.contains("Token expired"), "{error_message}");
    assert!(error_message.contains("unauthorized"), "{error_message}");
    // Exact oracle string (verified against the TS suite's 401 responder,
    // whose status text defaults to the canonical reason phrase).
    assert_eq!(
        error_message,
        "401 Unauthorized: Token expired (unauthorized)"
    );

    let diagnostics = message
        .diagnostics
        .as_ref()
        .and_then(|diagnostics| diagnostics.first())
        .expect("response failure diagnostic");
    assert_eq!(diagnostics.kind, "pi_messages_response_failure");
    assert_eq!(
        diagnostics
            .details
            .as_ref()
            .and_then(|details| details.get("status")),
        Some(&json!(401))
    );
}

#[tokio::test]
async fn propagates_server_sent_error_events() {
    let fetch = Arc::new(PiMessagesServerFetch::new(vec![
        json!({"type": "start"}),
        json!({"type": "error", "reason": "error", "usage": usage(), "errorMessage": "Upstream failed"}),
    ]));

    let message = stream(
        &create_model(BASE_URL),
        &context(),
        Some(&options(fetch, "test-key")),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Upstream failed"));
    assert_eq!(serde_json::to_value(&message.usage).unwrap(), usage());
}

#[tokio::test]
async fn errors_when_no_api_key_is_provided() {
    // TS points the model at an unroutable port; the mock transport proves the
    // request is never even attempted.
    let fetch = Arc::new(PiMessagesServerFetch::new(vec![json!({
        "type": "done",
        "reason": "stop",
        "usage": usage(),
    })]));
    let options = PiMessagesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let message = stream(
        &create_model("http://127.0.0.1:1/v1"),
        &context(),
        Some(&options),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("No API key provided")),
        "{:?}",
        message.error_message
    );
    assert!(fetch.requests().is_empty());
}

#[tokio::test]
async fn errors_when_the_stream_ends_without_a_terminal_event() {
    let fetch = Arc::new(PiMessagesServerFetch::new(vec![
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "partial"}),
    ]));

    let message = stream(
        &create_model(BASE_URL),
        &context(),
        Some(&options(fetch, "test-key")),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("stream ended without a terminal event")),
        "{:?}",
        message.error_message
    );
}

/// TS: `getApiProvider("pi-messages")` is defined.
#[test]
fn is_registered_as_a_builtin_api_provider() {
    let provider = compat::get_api_provider("pi-messages");
    assert!(provider.is_some());
    assert_eq!(
        provider.expect("pi-messages api provider").api,
        "pi-messages"
    );
}

/// The TS case is a type-level check (`const api: Api = "pi-messages"`); the
/// Rust `Api` type is a plain string alias, so the same fact is demonstrated
/// at runtime: a `Model` with api "pi-messages" (on a provider without a
/// builtin model) streams through the api-provider dispatch.
#[tokio::test]
async fn is_a_known_api_usable_on_models() {
    let fetch = Arc::new(PiMessagesServerFetch::new(vec![json!({
        "type": "done",
        "reason": "stop",
        "usage": usage(),
    })]));
    let mut model = create_model(BASE_URL);
    model.provider = "custom-gateway".to_string();

    let message = compat::stream_simple(
        &model,
        &context(),
        Some(&simple_options(fetch.clone(), "test-key")),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.provider, "custom-gateway");
    let requests = fetch.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "http://pi-messages.test/v1/messages");
    assert_eq!(
        header(&requests[0], "authorization"),
        Some("Bearer test-key")
    );
}

/// Port of the pi-messages leg of fetch-option.test.ts's "uses fetch for
/// Mistral, Codex SSE, and pi-messages HTTP requests" (the Mistral and Codex
/// legs belong to their own adapters' suites).
///
/// TS stubs `globalThis.fetch` with a throwing fallback and asserts only the
/// injected fetch is called; Rust has no stubbable global fetch, and the
/// pi-messages adapter consults `options.fetch` first, so capturing the
/// request on the injected transport proves the same fact offline.
#[tokio::test]
async fn uses_fetch_for_pi_messages_http_requests() {
    let fetch = Arc::new(PiMessagesServerFetch::with_status(
        401,
        &json!({"error": {"message": "upstream rejected request"}}).to_string(),
    ));
    let model = Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "pi-messages".to_string(),
        provider: "test-provider".to_string(),
        base_url: "https://upstream.test/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: pi_core::ai::types::JsF64(0.0),
                output: pi_core::ai::types::JsF64(0.0),
                cache_read: pi_core::ai::types::JsF64(0.0),
                cache_write: pi_core::ai::types::JsF64(0.0),
            },
            tiers: None,
        },
        context_window: 10_000,
        max_tokens: 1_000,
        ..Default::default()
    };
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 1,
        })],
        ..Default::default()
    };

    let message = stream_simple(
        &model,
        &context,
        Some(&simple_options(fetch.clone(), "test-key")),
    )
    .result()
    .await;

    let requests = fetch.requests();
    assert_eq!(requests.len(), 1, "the injected fetch served the request");
    assert_eq!(requests[0].method, HttpMethod::Post);
    assert_eq!(requests[0].url, "https://upstream.test/v1/messages");
    assert_eq!(
        header(&requests[0], "authorization"),
        Some("Bearer test-key")
    );
    let error_message = message.error_message.as_deref().unwrap();
    assert!(
        error_message.contains("upstream rejected request"),
        "{error_message}"
    );
}
