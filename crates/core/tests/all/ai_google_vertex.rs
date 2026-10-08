//! Port of `pi-core/ai/test/google-vertex-api-key-resolution.test.ts`: how
//! the Vertex adapter resolves API keys (placeholder markers fall back to
//! ADC) and dispatches the resulting client configuration.
//!
//! The TS suite mocks the `@google/genai` SDK constructor; the Rust port
//! observes the resolved dispatch through the captured `HttpRequest` (URL,
//! headers) plus the public `resolve_api_key` / `base_url_includes_api_version`
//! helpers:
//! - ADC client (`vertexai`, `project`, `location`, `apiVersion: "v1"`, no
//!   `apiKey`) becomes the regional URL with no `x-goog-api-key` header;
//! - API-key client (`vertexai`, `apiKey`, no `project`/`location`) becomes
//!   the global endpoint with the `x-goog-api-key` header;
//! - `httpOptions.baseUrl` + `baseUrlResourceScope: COLLECTION` become the
//!   URL prefix, with `apiVersion` appended unless the base URL already
//!   includes one.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::google_vertex::{
    GoogleVertexOptions, base_url_includes_api_version, resolve_api_key,
    stream_with_transport as stream_google_vertex,
};
use pi_core::ai::types::{
    Context, Message, Model, ProviderEnv, ProviderHeaders, ProviderRequestOptions, RoleUser,
    StopReason, StreamOptions, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::json;

const REAL_API_KEY: &str = "AIzaSyExampleRealisticLookingApiKey123456";
const MODEL_ID: &str = "gemini-3-flash-preview";

/// The ADC client dispatch: regional endpoint, no API-key header.
const ADC_URL: &str = "https://us-central1-aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse";
/// The API-key client dispatch: global endpoint.
const API_KEY_URL: &str = "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse";
/// A custom (COLLECTION-scoped) base URL with `apiVersion: "v1"` appended.
const PROXY_URL: &str = "https://proxy.example.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse";
/// A custom base URL that already includes a version (`apiVersion: ""`).
const VERSIONED_PROXY_URL: &str = "https://proxy.example.com/v1/projects/test-project/locations/global/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse";

fn vertex_model() -> Model {
    pi_core::ai::providers::builtin::get_builtin_model("google-vertex", MODEL_ID)
        .unwrap_or_else(|| panic!("catalog model google-vertex/{MODEL_ID}"))
}

fn vertex_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

/// The mock transport standing in for the mocked `GoogleGenAI` client: records
/// every dispatch and yields the mocked "ok" generateContentStream chunk.
struct VertexMockFetch {
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for VertexMockFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let chunk = json!({
            "responseId": "vertex-response-id",
            "candidates": [
                {
                    "content": { "parts": [{ "text": "ok" }] },
                    "finishReason": "STOP",
                }
            ],
            "usageMetadata": {
                "promptTokenCount": 1,
                "candidatesTokenCount": 1,
                "totalTokenCount": 2,
            },
        });
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                    format!("data: {chunk}\n\n"),
                ))])),
            })
        })
    }
}

fn vertex_options() -> GoogleVertexOptions {
    GoogleVertexOptions {
        base: StreamOptions {
            base: ProviderRequestOptions::default(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn adc_options() -> GoogleVertexOptions {
    let mut options = vertex_options();
    options.project = Some("test-project".to_string());
    options.location = Some("us-central1".to_string());
    options
}

/// Runs the stream with the mock transport and returns the final message plus
/// every dispatched request (the TS `googleGenAiMock.constructorCalls`).
async fn run_vertex(
    model: &Model,
    options: GoogleVertexOptions,
) -> (pi_core::ai::types::AssistantMessage, Vec<HttpRequest>) {
    let fetch = Arc::new(VertexMockFetch {
        requests: Mutex::new(Vec::new()),
    });
    let mut options = options;
    options.base.base.fetch = Some(fetch.clone());

    let message = stream_google_vertex(model, &vertex_context(), Some(&options), None)
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(message.error_message.is_none());

    let requests = fetch.requests.lock().unwrap().clone();
    (message, requests)
}

fn pi_user_agent() -> String {
    pi_core::ai::session_resources::get_pi_user_agent()
}

// ---------------------------------------------------------------------------
// "google-vertex api key resolution"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn falls_back_to_adc_when_options_api_key_is_a_placeholder_marker() {
    let mut options = adc_options();
    options.base.base.api_key = Some("<authenticated>".to_string());
    assert_eq!(resolve_api_key(Some(&options)), None);

    let (_message, requests) = run_vertex(&vertex_model(), options).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, ADC_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), pi_user_agent())]
    );
}

#[tokio::test]
async fn falls_back_to_adc_when_options_api_key_is_the_gcp_vertex_credentials_marker() {
    let mut options = adc_options();
    options.base.base.api_key = Some("gcp-vertex-credentials".to_string());
    assert_eq!(resolve_api_key(Some(&options)), None);

    let (_message, requests) = run_vertex(&vertex_model(), options).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, ADC_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), pi_user_agent())]
    );
}

#[tokio::test]
async fn falls_back_to_adc_when_google_cloud_api_key_is_a_placeholder_marker() {
    let mut options = adc_options();
    options.base.base.env = Some(ProviderEnv::from([(
        "GOOGLE_CLOUD_API_KEY".to_string(),
        "<authenticated>".to_string(),
    )]));
    assert_eq!(resolve_api_key(Some(&options)), None);

    let (_message, requests) = run_vertex(&vertex_model(), options).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, ADC_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), pi_user_agent())]
    );
}

#[tokio::test]
async fn still_uses_the_api_key_client_for_real_api_keys() {
    let mut options = vertex_options();
    options.base.base.api_key = Some(REAL_API_KEY.to_string());
    assert_eq!(
        resolve_api_key(Some(&options)),
        Some(REAL_API_KEY.to_string())
    );

    let (_message, requests) = run_vertex(&vertex_model(), options).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, API_KEY_URL);
    assert_eq!(
        requests[0].headers,
        vec![
            ("x-goog-api-key".to_string(), REAL_API_KEY.to_string()),
            ("User-Agent".to_string(), pi_user_agent()),
        ]
    );
}

#[tokio::test]
async fn does_not_forward_generated_vertex_base_url_placeholders() {
    // The catalog base URL contains the `{location}` placeholder; it is not
    // forwarded as a custom base URL, so the regional URL is generated and
    // httpOptions carries only the default User-Agent header.
    let (_message, requests) = run_vertex(&vertex_model(), adc_options()).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, ADC_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), pi_user_agent())]
    );
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_user_agent() {
    let mut options = adc_options();
    options.base.base.headers = Some(ProviderHeaders::from([(
        "User-Agent".to_string(),
        Some("custom-agent".to_string()),
    )]));

    let (_message, requests) = run_vertex(&vertex_model(), options).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, ADC_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), "custom-agent".to_string())]
    );
}

#[tokio::test]
async fn forwards_custom_base_url_to_the_adc_client() {
    let mut model = vertex_model();
    model.base_url = "https://proxy.example.com".to_string();
    assert!(!base_url_includes_api_version("https://proxy.example.com"));

    let (_message, requests) = run_vertex(&model, adc_options()).await;

    assert_eq!(requests.len(), 1);
    // The custom base URL replaces the regional host; apiVersion "v1" is
    // still appended (COLLECTION resource scope).
    assert_eq!(requests[0].url, PROXY_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), pi_user_agent())]
    );
}

#[tokio::test]
async fn forwards_custom_base_url_to_the_api_key_client() {
    let mut model = vertex_model();
    model.base_url = "https://proxy.example.com".to_string();

    let mut options = vertex_options();
    options.base.base.api_key = Some(REAL_API_KEY.to_string());

    let (_message, requests) = run_vertex(&model, options).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, PROXY_URL);
    assert_eq!(
        requests[0].headers,
        vec![
            ("x-goog-api-key".to_string(), REAL_API_KEY.to_string()),
            ("User-Agent".to_string(), pi_user_agent()),
        ]
    );
}

#[tokio::test]
async fn does_not_append_api_version_when_custom_base_url_already_includes_one() {
    let versioned_base = "https://proxy.example.com/v1/projects/test-project/locations/global";
    let mut model = vertex_model();
    model.base_url = versioned_base.to_string();
    assert!(base_url_includes_api_version(versioned_base));

    let (_message, requests) = run_vertex(&model, adc_options()).await;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, VERSIONED_PROXY_URL);
    assert_eq!(
        requests[0].headers,
        vec![("User-Agent".to_string(), pi_user_agent())]
    );
}
