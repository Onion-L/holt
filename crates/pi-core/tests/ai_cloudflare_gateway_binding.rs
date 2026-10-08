//! Port of `pi-core/ai/test/cloudflare-gateway-binding.test.ts` against the
//! Rust transport model. The TypeScript test mocks `env.AI`; here the fake
//! implements the [`AiGatewayBinding`] trait and the requests stand in for
//! fetch `Request`/`init` pairs.
//!
//! TypeScript cases without a Rust equivalent (single final `HttpRequest`
//! form, transport without an abort signal): init-headers replacing a Request
//! input's headers, Request-input handling, abort-signal forwarding, and
//! `signal: null` clearing.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::cloudflare_gateway_binding::{
    AiGatewayBinding, AiGatewayBindingGateway, AiGatewayRunOptions, AiGatewayUniversalRequest,
    CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL, GatewayBindingFetchOptions,
    create_gateway_binding_fetch,
};
use pi_core::ai::types::{
    Context, FetchFunction, Message, Model, RoleUser, StopReason, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetchError, HttpMethod, HttpRequest, HttpResponse};
use serde_json::json;

const BASE_URL: &str = "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway";

struct CapturedRun {
    gateway_id: String,
    data: AiGatewayUniversalRequest,
    signal: Option<tokio_util::sync::CancellationToken>,
}

type ResponseFactory = Arc<dyn Fn() -> HttpResponse + Send + Sync>;

struct FakeBinding {
    runs: Arc<Mutex<Vec<CapturedRun>>>,
    response_factory: ResponseFactory,
}

struct FakeGateway {
    gateway_id: String,
    runs: Arc<Mutex<Vec<CapturedRun>>>,
    response_factory: ResponseFactory,
}

impl AiGatewayBinding for FakeBinding {
    fn gateway(&self, id: &str) -> Arc<dyn AiGatewayBindingGateway> {
        Arc::new(FakeGateway {
            gateway_id: id.to_string(),
            runs: Arc::clone(&self.runs),
            response_factory: Arc::clone(&self.response_factory),
        })
    }
}

impl AiGatewayBindingGateway for FakeGateway {
    fn run<'a>(
        &'a self,
        data: AiGatewayUniversalRequest,
        options: AiGatewayRunOptions,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.runs.lock().unwrap().push(CapturedRun {
            gateway_id: self.gateway_id.clone(),
            data,
            signal: options.signal,
        });
        let response = (self.response_factory)();
        Box::pin(async move { Ok(response) })
    }
}

fn json_response(body: &'static str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
    }
}

fn fake_binding() -> FakeBinding {
    FakeBinding {
        runs: Arc::new(Mutex::new(Vec::new())),
        response_factory: Arc::new(|| json_response("{}")),
    }
}

fn fake_binding_with(response_factory: ResponseFactory) -> FakeBinding {
    FakeBinding {
        runs: Arc::new(Mutex::new(Vec::new())),
        response_factory,
    }
}

fn fetch_fn(binding: &FakeBinding) -> FetchFunction {
    create_gateway_binding_fetch(GatewayBindingFetchOptions {
        binding: Arc::new(FakeBinding {
            runs: Arc::clone(&binding.runs),
            response_factory: Arc::clone(&binding.response_factory),
        }),
        base_url: BASE_URL.to_string(),
        gateway: "my-gateway".to_string(),
    })
    .expect("valid gateway binding options")
}

fn post(url: &str, body: HttpBody) -> HttpRequest {
    HttpRequest {
        signal: None,
        method: HttpMethod::Post,
        url: url.to_string(),
        headers: Vec::new(),
        body,
    }
}

fn runs_of(binding: &FakeBinding) -> Vec<(String, String)> {
    binding
        .runs
        .lock()
        .unwrap()
        .iter()
        .map(|run| (run.data.provider.clone(), run.data.endpoint.clone()))
        .collect()
}

/// `unwrap_err` in message form: `HttpResponse` is not `Debug`.
async fn expect_error(fetch: &FetchFunction, request: HttpRequest) -> String {
    match fetch.fetch(request).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("expected the request to reject"),
    }
}

#[tokio::test]
async fn derives_provider_and_endpoint_from_gateway_passthrough_urls() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    fetch
        .fetch(post(
            &format!("{BASE_URL}/anthropic/v1/messages"),
            HttpBody::Json(json!({"model": "claude"})),
        ))
        .await
        .unwrap();
    fetch
        .fetch(post(
            &format!("{BASE_URL}/openai/responses"),
            HttpBody::Json(json!({"model": "gpt"})),
        ))
        .await
        .unwrap();
    fetch
        .fetch(post(
            &format!("{BASE_URL}/workers-ai/v1/chat/completions"),
            HttpBody::Json(json!({"model": "@cf/meta/llama"})),
        ))
        .await
        .unwrap();

    assert_eq!(
        runs_of(&binding),
        vec![
            ("anthropic".to_string(), "v1/messages".to_string()),
            ("openai".to_string(), "responses".to_string()),
            ("workers-ai".to_string(), "v1/chat/completions".to_string()),
        ]
    );
    assert_eq!(
        binding
            .runs
            .lock()
            .unwrap()
            .iter()
            .map(|run| run.gateway_id.as_str())
            .collect::<Vec<_>>(),
        vec!["my-gateway", "my-gateway", "my-gateway"]
    );
    assert_eq!(
        binding.runs.lock().unwrap()[0].data.query,
        json!({"model": "claude"})
    );
}

#[tokio::test]
async fn keeps_the_query_string_in_the_endpoint() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    fetch
        .fetch(post(
            &format!("{BASE_URL}/openai/responses?beta=true"),
            HttpBody::Text("{}".to_string()),
        ))
        .await
        .unwrap();

    assert_eq!(
        binding.runs.lock().unwrap()[0].data.endpoint,
        "responses?beta=true"
    );
}

#[tokio::test]
async fn lowercases_header_names_so_case_variants_collapse() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    let mut request = post(
        &format!("{BASE_URL}/anthropic/v1/messages"),
        HttpBody::Text("{}".to_string()),
    );
    request
        .headers
        .push(("Anthropic-Version".to_string(), "2023-06-01".to_string()));
    fetch.fetch(request).await.unwrap();

    assert_eq!(
        binding.runs.lock().unwrap()[0].data.headers,
        [("anthropic-version".to_string(), "2023-06-01".to_string())]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn strips_gateway_auth_and_derived_headers_forwards_the_rest() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    let mut request = post(
        &format!("{BASE_URL}/anthropic/v1/messages"),
        HttpBody::Text("{}".to_string()),
    );
    request.headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Content-Length".to_string(), "17".to_string()),
        (
            "CF-AIG-Authorization".to_string(),
            format!("Bearer {CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}"),
        ),
        (
            "cf-aig-metadata".to_string(),
            r#"{"user":"42"}"#.to_string(),
        ),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ("x-api-key".to_string(), "provider-key".to_string()),
    ];
    fetch.fetch(request).await.unwrap();

    let headers = &binding.runs.lock().unwrap()[0].data.headers;
    assert!(!headers.contains_key("cf-aig-authorization"));
    assert!(!headers.contains_key("content-length"));
    assert_eq!(
        headers.get("cf-aig-metadata").map(String::as_str),
        Some(r#"{"user":"42"}"#)
    );
    assert_eq!(
        headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
    // Provider auth headers pass through: that is how request-supplied (BYOK)
    // keys ride.
    assert_eq!(
        headers.get("x-api-key").map(String::as_str),
        Some("provider-key")
    );
}

#[tokio::test]
async fn returns_the_binding_response_untouched_including_streaming_bodies() {
    let binding = fake_binding_with(Arc::new(|| HttpResponse {
        status: 200,
        headers: vec![
            ("content-type".to_string(), "text/event-stream".to_string()),
            ("cf-aig-log-id".to_string(), "log-1".to_string()),
        ],
        body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
            "data: {}\n\n",
        ))])),
    }));
    let fetch = fetch_fn(&binding);

    let response = fetch
        .fetch(post(
            &format!("{BASE_URL}/workers-ai/v1/chat/completions"),
            HttpBody::Text("{}".to_string()),
        ))
        .await
        .unwrap();

    assert_eq!(response.status, 200);
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name == "cf-aig-log-id" && value == "log-1")
    );
    assert_eq!(response.text().await.unwrap(), "data: {}\n\n");
}

#[tokio::test]
async fn rejects_in_prefix_requests_the_universal_endpoint_cannot_express() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    let get = HttpRequest {
        signal: None,
        method: HttpMethod::Get,
        url: format!("{BASE_URL}/anthropic/v1/messages"),
        headers: Vec::new(),
        body: HttpBody::Empty,
    };
    let error = expect_error(&fetch, get).await;
    assert!(error.contains("cannot express GET"), "{error}");

    let error = expect_error(
        &fetch,
        post(
            &format!("{BASE_URL}/anthropic/v1/messages"),
            HttpBody::Text("not json".to_string()),
        ),
    )
    .await;
    assert!(error.contains("non-JSON body"), "{error}");

    let error = expect_error(
        &fetch,
        post(
            &format!("{BASE_URL}/anthropic"),
            HttpBody::Text("{}".to_string()),
        ),
    )
    .await;
    assert!(error.contains("missing provider/endpoint path"), "{error}");

    assert_eq!(binding.runs.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn rejects_urls_outside_the_gateway_prefix() {
    // Silent passthrough would ship the auth sentinel to whatever host the URL
    // names; a misconfigured baseUrl must fail loudly instead.
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    let error = expect_error(
        &fetch,
        post(
            "https://api.openai.com/v1/chat/completions",
            HttpBody::Text("{}".to_string()),
        ),
    )
    .await;
    assert!(
        error.contains("outside the configured gateway prefix"),
        "{error}"
    );

    // Same origin, different path (another account's gateway) is just as
    // out-of-prefix.
    let error = expect_error(
        &fetch,
        post(
            "https://gateway.ai.cloudflare.com/v1/other-account/my-gateway/anthropic/v1/messages",
            HttpBody::Text("{}".to_string()),
        ),
    )
    .await;
    assert!(
        error.contains("outside the configured gateway prefix"),
        "{error}"
    );
    assert_eq!(binding.runs.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn matches_and_splits_on_the_url_normalized_path() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    // Dot segments normalize away before the provider/endpoint split, so a
    // lexical variant routes exactly like its normal form (raw string
    // prefixing would split it differently).
    fetch
        .fetch(post(
            &format!("{BASE_URL}/anthropic/../anthropic/v1/./messages"),
            HttpBody::Json(json!({"model": "claude"})),
        ))
        .await
        .unwrap();
    assert_eq!(
        runs_of(&binding),
        vec![("anthropic".to_string(), "v1/messages".to_string())]
    );

    // A dot-segment URL that resolves outside the prefix is rejected even
    // though it starts with the prefix as a raw string.
    let error = expect_error(
        &fetch,
        post(
            &format!("{BASE_URL}/../other-gateway/anthropic/v1/messages"),
            HttpBody::Text("{}".to_string()),
        ),
    )
    .await;
    assert!(
        error.contains("outside the configured gateway prefix"),
        "{error}"
    );
    assert_eq!(binding.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn parses_bytes_bodies_for_the_json_probe() {
    // The TypeScript probe consumes one-shot stream bodies; the Rust transport
    // hands over replayable bytes, which reach the binding as parsed query.
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    fetch
        .fetch(post(
            &format!("{BASE_URL}/anthropic/v1/messages"),
            HttpBody::Bytes(bytes::Bytes::from(r#"{"model":"claude"}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(binding.runs.lock().unwrap().len(), 1);
    assert_eq!(
        binding.runs.lock().unwrap()[0].data.query,
        json!({"model": "claude"})
    );
}

#[tokio::test]
async fn keeps_sdk_placeholder_auth_out_of_entries_when_paired_with_null_auth_headers() {
    // The full header contract from the module docs: the sentinel satisfies
    // pi's request-auth check, and the explicit nulls make the OpenAI SDK
    // drop its own `Authorization: Bearer unused` placeholder before the
    // request reaches the shim.
    let binding = fake_binding_with(Arc::new(|| HttpResponse {
        status: 400,
        headers: Vec::new(),
        body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
            r#"{"error": {"type": "bad_request", "message": "stubbed"}}"#,
        ))])),
    }));
    let fetch = fetch_fn(&binding);
    let model = Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        base_url: format!("{BASE_URL}/openai"),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        cost: pi_core::ai::types::ModelCost::default(),
        context_window: 10_000,
        max_tokens: 1_000,
        ..Default::default()
    };
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: pi_core::ai::types::UserContent::Text("hello".to_string()),
            timestamp: 1,
        })],
        ..Default::default()
    };
    let options = pi_core::ai::api::openai_completions::OpenAICompletionsOptions {
        base: pi_core::ai::types::StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                fetch: Some(fetch),
                headers: Some(
                    [
                        (
                            "cf-aig-authorization".to_string(),
                            Some(format!("Bearer {CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}")),
                        ),
                        ("Authorization".to_string(), None),
                        ("x-api-key".to_string(), None),
                    ]
                    .into_iter()
                    .collect(),
                ),
                max_retries: Some(0),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let result = pi_core::ai::api::openai_completions::stream(&model, &context, Some(&options))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    let runs = binding.runs.lock().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].data.provider, "openai");
    let header_names: Vec<&str> = runs[0].data.headers.keys().map(String::as_str).collect();
    assert!(!header_names.contains(&"authorization"));
    assert!(!header_names.contains(&"x-api-key"));
    assert!(!header_names.contains(&"cf-aig-authorization"));
}

/// Port of "forwards the abort signal": the request's abort signal reaches
/// the binding run options. (The companion `signal: null`-clears case is
/// unrepresentable: the Rust `HttpRequest` is the single final request form
/// with no `Request`/`init` split.)
#[tokio::test]
async fn forwards_the_abort_signal() {
    let binding = fake_binding();
    let fetch = fetch_fn(&binding);

    let signal = tokio_util::sync::CancellationToken::new();
    let mut request = post(
        &format!("{BASE_URL}/anthropic/v1/messages"),
        HttpBody::Json(serde_json::json!({"model": "claude"})),
    );
    request.signal = Some(signal.clone());

    let response = fetch.fetch(request).await.expect("binding run ok");
    assert_eq!(response.status, 200);

    let runs = binding.runs.lock().unwrap();
    assert_eq!(runs.len(), 1);
    let forwarded = runs[0].signal.as_ref().expect("signal forwarded to run");
    // CancellationToken equality is token identity.
    assert_eq!(*forwarded, signal);
}
