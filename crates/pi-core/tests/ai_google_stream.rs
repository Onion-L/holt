//! Port of the offline google stream TS suites:
//! - `google-raw-stop-reason.test.ts`
//! - `google-shared-retry.test.ts`
//!
//! The TS suites mock the `@google/genai` SDK: the mocked
//! `generateContentStream` generator becomes an SSE response served by the
//! mock transport, and the mocked `GoogleGenAI` constructor config
//! (`httpOptions.headers`) becomes the captured `HttpRequest` headers.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pi_core::ai::api::google_generative_ai::{
    GoogleOptions, stream_with_transport as stream_google_generative_ai,
};
use pi_core::ai::api::google_vertex::{
    GoogleVertexOptions, stream_with_transport as stream_google_vertex,
};
use pi_core::ai::types::{
    AssistantContent, Context, Message, Model, ProviderHeaders, ProviderRequestOptions, RoleUser,
    StopReason, StreamOptions, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use pi_core::ai::utils::provider_retry::{
    ProviderHttpError, ProviderRetryOptions, retry_provider_request,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn generative_ai_model() -> Model {
    pi_core::ai::providers::builtin::get_builtin_model("google", "gemini-2.5-flash")
        .expect("catalog model google/gemini-2.5-flash")
}

fn vertex_model() -> Model {
    pi_core::ai::providers::builtin::get_builtin_model("google-vertex", "gemini-3-flash-preview")
        .expect("catalog model google-vertex/gemini-3-flash-preview")
}

fn raw_stop_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

/// The mock transport standing in for the mocked `@google/genai` SDK client.
struct GoogleSseMockFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for GoogleSseMockFetch {
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

/// The chunk yielded by the mocked `generateContentStream` generator.
fn google_stream_chunk(finish_reason: &str, include_function_call: bool) -> Value {
    let mut candidate = json!({ "finishReason": finish_reason });
    if include_function_call {
        candidate["content"] = json!({
            "parts": [
                {
                    "functionCall": {
                        "id": "call-1",
                        "name": "echo",
                        "args": { "value": "truncated" },
                    }
                }
            ]
        });
    }
    json!({
        "responseId": "google-response-id",
        "candidates": [candidate],
        "usageMetadata": {
            "promptTokenCount": 1,
            "candidatesTokenCount": 0,
            "totalTokenCount": 1,
        },
    })
}

fn google_sse_fetch(finish_reason: &str, include_function_call: bool) -> Arc<GoogleSseMockFetch> {
    Arc::new(GoogleSseMockFetch {
        body: format!(
            "data: {}\n\n",
            google_stream_chunk(finish_reason, include_function_call)
        ),
        requests: Mutex::new(Vec::new()),
    })
}

fn google_options(fetch: Arc<GoogleSseMockFetch>) -> GoogleOptions {
    GoogleOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test-api-key".to_string()),
                fetch: Some(fetch),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn vertex_options(fetch: Arc<GoogleSseMockFetch>) -> GoogleVertexOptions {
    GoogleVertexOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                fetch: Some(fetch),
                ..Default::default()
            },
            ..Default::default()
        },
        project: Some("test-project".to_string()),
        location: Some("us-central1".to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// google-raw-stop-reason.test.ts — "Google raw stop reasons"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preserves_raw_gemini_finish_reasons_for_google_generative_ai_errors() {
    let fetch = google_sse_fetch("MALFORMED_FUNCTION_CALL", false);

    let message = stream_google_generative_ai(
        &generative_ai_model(),
        &raw_stop_context(),
        Some(&google_options(fetch)),
        None,
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.raw_stop_reason.as_deref(),
        Some("MALFORMED_FUNCTION_CALL")
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: MALFORMED_FUNCTION_CALL")
    );
}

#[tokio::test]
async fn preserves_raw_gemini_finish_reasons_for_google_vertex_errors() {
    let fetch = google_sse_fetch("SAFETY", false);

    let message = stream_google_vertex(
        &vertex_model(),
        &raw_stop_context(),
        Some(&vertex_options(fetch)),
        None,
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("SAFETY"));
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: SAFETY")
    );
}

#[tokio::test]
async fn preserves_max_tokens_with_a_tool_call_as_length_for_google_generative_ai() {
    let fetch = google_sse_fetch("MAX_TOKENS", true);

    let message = stream_google_generative_ai(
        &generative_ai_model(),
        &raw_stop_context(),
        Some(&google_options(fetch)),
        None,
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("MAX_TOKENS"));
    assert!(
        message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    );
}

#[tokio::test]
async fn preserves_max_tokens_with_a_tool_call_as_length_for_google_vertex() {
    let fetch = google_sse_fetch("MAX_TOKENS", true);

    let message = stream_google_vertex(
        &vertex_model(),
        &raw_stop_context(),
        Some(&vertex_options(fetch)),
        None,
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("MAX_TOKENS"));
    assert!(
        message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    );
}

#[tokio::test]
async fn maps_stop_with_a_tool_call_to_tool_use_for_google_generative_ai() {
    let fetch = google_sse_fetch("STOP", true);

    let message = stream_google_generative_ai(
        &generative_ai_model(),
        &raw_stop_context(),
        Some(&google_options(fetch)),
        None,
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("STOP"));
    assert!(
        message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    );
}

#[tokio::test]
async fn maps_stop_with_a_tool_call_to_tool_use_for_google_vertex() {
    let fetch = google_sse_fetch("STOP", true);

    let message = stream_google_vertex(
        &vertex_model(),
        &raw_stop_context(),
        Some(&vertex_options(fetch)),
        None,
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("STOP"));
    assert!(
        message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    );
}

// ---------------------------------------------------------------------------
// google-raw-stop-reason.test.ts — "Google Generative AI user agent"
// ---------------------------------------------------------------------------

/// Port of `captureGoogleHeaders`: streams a plain STOP response with the
/// given request headers and returns the dispatched request headers (the SDK
/// constructor's `httpOptions.headers`).
async fn capture_google_headers(headers: Option<ProviderHeaders>) -> Vec<(String, String)> {
    let fetch = google_sse_fetch("STOP", false);
    let mut options = google_options(fetch.clone());
    options.base.base.headers = headers;

    let message = stream_google_generative_ai(
        &generative_ai_model(),
        &raw_stop_context(),
        Some(&options),
        None,
    )
    .result()
    .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(message.error_message.is_none());

    let requests = fetch.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    requests[0].headers.clone()
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn uses_pi_user_agent_by_default() {
    let headers = capture_google_headers(None).await;
    assert_eq!(
        header_value(&headers, "User-Agent"),
        Some(pi_core::ai::session_resources::get_pi_user_agent().as_str())
    );
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_user_agent() {
    let headers = capture_google_headers(Some(ProviderHeaders::from([(
        "User-Agent".to_string(),
        Some("custom-agent".to_string()),
    )])))
    .await;
    assert_eq!(header_value(&headers, "User-Agent"), Some("custom-agent"));
}

// ---------------------------------------------------------------------------
// google-shared-retry.test.ts — "google request retries"
// ---------------------------------------------------------------------------

/// Shaped like `@google/genai`'s ApiError: has `status`, but no `headers`.
/// The TS `retryGoogleRequest` normalizes such errors by adding
/// `headers = undefined` before `retryProviderRequest` inspects them; the
/// Rust `ProviderHttpError` with an empty header list is that shape.
fn google_api_error(status: u16) -> ProviderHttpError {
    ProviderHttpError::new(format!("got status: {status}"), Some(status), Vec::new())
}

#[tokio::test(start_paused = true)]
async fn retries_a_headers_less_sdk_error_with_a_retryable_status() {
    let calls = Arc::new(AtomicU32::new(0));
    let result = retry_provider_request(
        || {
            let calls = Arc::clone(&calls);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(google_api_error(429))
                } else {
                    Ok("ok".to_string())
                }
            }
        },
        ProviderRetryOptions {
            max_retries: Some(1),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn does_not_retry_when_max_retries_is_unset() {
    let error = google_api_error(429);
    let calls = Arc::new(AtomicU32::new(0));
    let result = retry_provider_request(
        || {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<String, _>(google_api_error(429))
            }
        },
        ProviderRetryOptions::default(),
    )
    .await;

    assert_eq!(result.unwrap_err(), error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn does_not_retry_a_non_retryable_status() {
    let error = google_api_error(400);
    let calls = Arc::new(AtomicU32::new(0));
    let result = retry_provider_request(
        || {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<String, _>(google_api_error(400))
            }
        },
        ProviderRetryOptions {
            max_retries: Some(2),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(result.unwrap_err(), error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Port of the fetch-option.test.ts leg "rejects custom fetch for Google
/// adapters instead of silently bypassing it": the `fetch` option carries a
/// custom transport in the Rust port, which is exactly the case the
/// TypeScript adapters reject. (The companion "allows Google adapters to
/// receive globalThis.fetch explicitly" leg is unrepresentable — there is no
/// ambient global fetch to pass identically.)
#[tokio::test]
async fn rejects_custom_fetch_for_google_adapters() {
    let model = generative_ai_model();
    let context = raw_stop_context();
    // Any custom transport triggers the rejection; the mock never answers,
    // so a bypass would hang the test.
    let options = google_options(google_sse_fetch("STOP", false));

    let message = pi_core::ai::api::google_generative_ai::stream(&model, &context, Some(&options))
        .result()
        .await;
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("Custom fetch is not supported by the Google Generative AI adapter"),
        "unexpected error: {:?}",
        message.error_message
    );

    let model = vertex_model();
    let options = vertex_options(google_sse_fetch("STOP", false));
    let _ = &options;
    let message = pi_core::ai::api::google_vertex::stream(&model, &context, Some(&options))
        .result()
        .await;
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("Custom fetch is not supported by the Google Vertex adapter"),
        "unexpected error: {:?}",
        message.error_message
    );
}
