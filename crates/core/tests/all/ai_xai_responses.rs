//! Port of `pi-core/ai/test/xai-responses.test.ts`: the xAI provider's
//! built-in catalog routing and its `/responses` request shape against a
//! mocked transport.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::openai_completions::{
    OpenAICompletionsOptions, stream as stream_completions,
};
use pi_core::ai::api::openai_responses::{OpenAIResponsesOptions, stream as stream_responses};
use pi_core::ai::models::get_supported_thinking_levels;
use pi_core::ai::models_generated::models_for_provider;
use pi_core::ai::providers::builtin::xai_provider;
use pi_core::ai::session_resources::get_pi_user_agent;
use pi_core::ai::types::{
    CacheRetention, Context, Message, Model, ModelCost, ModelInput, ModelThinkingLevel,
    ProviderHeaders, ProviderRequestOptions, RoleUser, StopReason, StreamOptions, ThinkingLevel,
    UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

// The TS suite pins `pi (${platform()} ${release()}; ${arch()})` from
// node:os; the Rust equivalent of that constant is getPiUserAgent's port.
// The platform/arch segments are checked against the same runtime constants
// the port reads, and the release segment comes from the OS itself
// (node reads the kernel release, the Rust port reads the product version),
// so the helper's exact value is the source of truth for the tail.
fn pi_user_agent() -> String {
    get_pi_user_agent()
}

/// Guard mirroring the user agent shape `pi-core-rs (<platform> <release>;
/// <arch>)` (deliberately prefixed `pi-core-rs` instead of upstream `pi`),
/// then equality with the helper's value.
fn assert_is_pi_user_agent(value: Option<&str>) {
    let value = value.expect("user-agent header present");
    assert!(
        value.starts_with(&format!("pi-core-rs ({} ", std::env::consts::OS)),
        "unexpected user-agent shape: {value}"
    );
    assert!(
        value.ends_with(&format!("; {})", std::env::consts::ARCH)),
        "unexpected user-agent shape: {value}"
    );
    assert_eq!(value, pi_user_agent());
}

/// The generated xAI catalog — the Rust equivalent of `XAI_MODELS`.
fn xai_models() -> Vec<Model> {
    models_for_provider("xai")
}

fn xai_model(id: &str) -> Model {
    xai_models()
        .into_iter()
        .find(|model| model.id == id)
        .unwrap_or_else(|| panic!("missing built-in xAI model: {id}"))
}

fn context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 1,
        })],
        ..Default::default()
    }
}

fn system_context() -> Context {
    Context {
        system_prompt: Some("You are a careful coding assistant.".to_string()),
        ..context()
    }
}

/// The canned `response.completed` SSE body served by the TS mock fetch.
fn completed_response_body() -> String {
    let event = json!({
        "type": "response.completed",
        "sequence_number": 0,
        "response": {
            "id": "resp_xai_test",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": { "cached_tokens": 0 },
            },
        },
    });
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

/// The canned chat-completions SSE body served by the TS mock fetch.
fn completions_user_agent_body() -> String {
    let chunks = [
        json!({
            "id": "chatcmpl-ua",
            "choices": [{"delta": {"content": "ok"}, "finish_reason": null, "index": 0}],
        }),
        json!({
            "id": "chatcmpl-ua",
            "choices": [{"delta": {}, "finish_reason": "stop", "index": 0}],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ];
    let events = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>();
    format!("{events}data: [DONE]\n\n")
}

/// Mock transport standing in for the TS tests' mocked global fetch: serves a
/// canned SSE body and captures every outgoing request.
struct CaptureFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl CaptureFetch {
    fn new(body: String) -> Arc<Self> {
        Arc::new(Self {
            body,
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl HttpFetch for CaptureFetch {
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

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

struct CapturedRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

fn responses_options(api_key: &str) -> OpenAIResponsesOptions {
    OpenAIResponsesOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(api_key.to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Port of the TS `captureRequest` helper: streams through the xAI provider
/// and returns the captured request. The TS suite always dispatches via
/// `xaiProvider().stream`, which forwards to the openai-responses stream;
/// the Rust `Provider` trait erases API-specific options (`reasoningEffort`
/// and friends), so plain requests go through `xai_provider().stream` while
/// requests carrying API-specific options call the responses stream directly
/// — the same dispatch target the provider routes to.
async fn capture_request(
    model: &Model,
    context: &Context,
    options: OpenAIResponsesOptions,
) -> CapturedRequest {
    let has_api_specific_options = options.reasoning_effort.is_some()
        || options.reasoning_summary.is_some()
        || options.service_tier.is_some()
        || options.tool_choice.is_some();
    let fetch = CaptureFetch::new(completed_response_body());
    let mut options = options;
    options.base.base.fetch = Some(fetch.clone());

    let result = if has_api_specific_options {
        stream_responses(model, context, Some(&options))
            .result()
            .await
    } else {
        xai_provider()
            .stream(model, context, Some(&options.base))
            .result()
            .await
    };
    assert_eq!(
        result.stop_reason,
        StopReason::Stop,
        "stream failed: {:?}",
        result.error_message
    );

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let body = match &request.body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    };
    CapturedRequest {
        url: request.url.clone(),
        headers: request.headers.clone(),
        body,
    }
}

/// The hand-rolled `openai-completions` model the TS suite uses to probe the
/// Completions User-Agent.
fn custom_completions_model() -> Model {
    Model {
        id: "grok-custom".to_string(),
        name: "Grok Custom".to_string(),
        api: "openai-completions".to_string(),
        provider: "xai".to_string(),
        base_url: "https://api.x.ai/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 16_384,
        ..Default::default()
    }
}

/// Port of the TS `captureCompletionsUserAgent` helper.
async fn capture_completions_user_agent(headers: Option<ProviderHeaders>) -> Option<String> {
    let fetch = CaptureFetch::new(completions_user_agent_body());
    let options = OpenAICompletionsOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("xai-test-token".to_string()),
                fetch: Some(fetch.clone()),
                headers,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let result = stream_completions(&custom_completions_model(), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        result.stop_reason,
        StopReason::Stop,
        "stream failed: {:?}",
        result.error_message
    );

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    header_value(&requests[0].headers, "user-agent").map(str::to_string)
}

#[test]
fn excludes_retired_and_redundant_models_from_the_builtin_catalog() {
    for model_id in [
        "grok-3",
        "grok-3-fast",
        "grok-4.20-0309-non-reasoning",
        "grok-4.20-0309-reasoning",
        "grok-build-0.1",
        "grok-code-fast-1",
    ] {
        assert!(
            !xai_models().iter().any(|model| model.id == model_id),
            "built-in xAI catalog must not contain {model_id}"
        );
    }
}

#[test]
fn routes_every_builtin_xai_model_through_responses() {
    for model in xai_models() {
        assert_eq!(model.api, "openai-responses", "model {}", model.id);
    }
    assert_eq!(
        get_supported_thinking_levels(&xai_model("grok-4.5")),
        vec![
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
        ]
    );
    assert_eq!(
        get_supported_thinking_levels(&xai_model("grok-4.6")),
        vec![
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Xhigh,
        ]
    );
    assert_eq!(
        get_supported_thinking_levels(&xai_model("grok-4.3")),
        vec![
            ModelThinkingLevel::Off,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
        ]
    );
}

#[tokio::test]
async fn uses_responses_with_bearer_auth_and_xai_compatible_request_fields() {
    let mut options = responses_options("xai-test-token");
    options.base.session_id = Some("pi-session-123".to_string());
    options.base.cache_retention = Some(CacheRetention::Long);
    options.reasoning_effort = Some(ThinkingLevel::Medium);

    let captured = capture_request(&xai_model("grok-4.5"), &system_context(), options).await;

    assert_eq!(captured.url, "https://api.x.ai/v1/responses");
    assert_eq!(
        captured.header("authorization"),
        Some("Bearer xai-test-token")
    );
    assert_eq!(
        captured.header("user-agent"),
        Some(pi_user_agent().as_str())
    );
    assert_eq!(captured.header("session_id"), Some("pi-session-123"));

    let body = &captured.body;
    assert_eq!(body["model"], json!("grok-4.5"));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["prompt_cache_key"], json!("pi-session-123"));
    assert_eq!(body["reasoning"]["effort"], json!("medium"));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("prompt_cache_retention").is_none());

    let input = body["input"].as_array().expect("input array");
    assert!(input.iter().any(|item| {
        item["role"] == json!("developer")
            && item["content"] == json!("You are a careful coding assistant.")
    }));
}

#[tokio::test]
async fn requests_encrypted_reasoning_without_an_effort_override() {
    let captured = capture_request(
        &xai_model("grok-4.5"),
        &context(),
        responses_options("xai-test-token"),
    )
    .await;

    let body = &captured.body;
    assert_eq!(body["model"], json!("grok-4.5"));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn uses_responses_for_grok_4_6_with_xhigh_effort_and_encrypted_reasoning() {
    let mut options = responses_options("xai-test-token");
    options.reasoning_effort = Some(ThinkingLevel::Xhigh);

    let captured = capture_request(&xai_model("grok-4.6"), &system_context(), options).await;

    assert_eq!(captured.url, "https://api.x.ai/v1/responses");
    let body = &captured.body;
    assert_eq!(body["model"], json!("grok-4.6"));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["reasoning"]["effort"], json!("xhigh"));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
}

#[tokio::test]
async fn uses_responses_for_grok_4_3() {
    let mut options = responses_options("xai-test-token");
    options.reasoning_effort = Some(ThinkingLevel::Low);

    let captured = capture_request(&xai_model("grok-4.3"), &context(), options).await;

    assert_eq!(captured.url, "https://api.x.ai/v1/responses");
    let body = &captured.body;
    assert_eq!(body["model"], json!("grok-4.3"));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["reasoning"]["effort"], json!("low"));
}

#[tokio::test]
async fn uses_pi_user_agent_by_default_for_responses_requests() {
    let mut openai_model = xai_model("grok-4.5");
    openai_model.provider = "openai".to_string();
    openai_model.base_url = "https://api.openai.com/v1".to_string();

    let fetch = CaptureFetch::new(completed_response_body());
    let mut options = responses_options("test-token");
    options.base.base.fetch = Some(fetch.clone());

    let result = stream_responses(&openai_model, &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        result.stop_reason,
        StopReason::Stop,
        "stream failed: {:?}",
        result.error_message
    );

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_is_pi_user_agent(header_value(&requests[0].headers, "user-agent"));
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_responses_user_agent() {
    let mut options = responses_options("xai-test-token");
    options.base.base.headers = Some(
        [("User-Agent".to_string(), Some("custom-agent".to_string()))]
            .into_iter()
            .collect(),
    );

    let captured = capture_request(&xai_model("grok-4.5"), &context(), options).await;

    assert_eq!(captured.header("user-agent"), Some("custom-agent"));
}

#[tokio::test]
async fn uses_pi_user_agent_by_default_for_completions_requests() {
    let user_agent = capture_completions_user_agent(None).await;
    assert_is_pi_user_agent(user_agent.as_deref());
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_completions_user_agent() {
    let user_agent = capture_completions_user_agent(Some(
        [("User-Agent".to_string(), Some("custom-agent".to_string()))]
            .into_iter()
            .collect(),
    ))
    .await;
    assert_eq!(user_agent.as_deref(), Some("custom-agent"));
}
