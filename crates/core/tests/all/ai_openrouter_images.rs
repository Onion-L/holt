//! Port of `pi-core/ai/test/openrouter-images.test.ts` against a canned HTTP
//! transport. The TypeScript test mocks the OpenAI SDK; here the mock sits at
//! the `HttpFetch` transport and the captured request body stands in for the
//! SDK `create()` params.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::openrouter_images::{ImagesOptions, generate_images};
use pi_core::ai::types::{
    BlockContent, ImagesContext, ImagesModel, ImagesStopReason, JsF64, ModelCost, ModelCostRates,
    ModelInput, TextContent,
};
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::Value;

const RESPONSE_BODY: &str = r#"{
	"id": "img-1",
	"usage": {
		"prompt_tokens": 12,
		"completion_tokens": 34,
		"prompt_tokens_details": { "cached_tokens": 0 }
	},
	"choices": [
		{
			"message": {
				"content": "Here is your image.",
				"images": [{ "image_url": "data:image/png;base64,ZmFrZS1wbmc=" }]
			}
		}
	]
}"#;

/// Mock transport mirroring the TypeScript FakeOpenAI: records requests and
/// returns the canned image-generation response.
struct RecordingFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for RecordingFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request.clone());
        // The TS mock stands in for the OpenAI SDK, which rejects when the
        // signal is already aborted.
        if request
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return Box::pin(std::future::ready(Err(HttpFetchError::Request(
                "Request aborted".to_string(),
            ))));
        }
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn text_image_model() -> ImagesModel {
    ImagesModel {
        id: "google/gemini-3.1-flash-image-preview".to_string(),
        name: "Gemini 3.1 Flash Image Preview".to_string(),
        api: "openrouter-images".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        input: vec![ModelInput::Text, ModelInput::Image],
        output: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost {
            rates: ModelCostRates {
                input: JsF64(0.015),
                output: JsF64(0.03),
                cache_read: JsF64(0.0),
                cache_write: JsF64(0.0),
            },
            tiers: None,
        },
        headers: Some(
            [(
                "HTTP-Referer".to_string(),
                "https://example.com".to_string(),
            )]
            .into_iter()
            .collect(),
        ),
        extra: Default::default(),
    }
}

fn image_only_model() -> ImagesModel {
    ImagesModel {
        id: "black-forest-labs/flux.2-pro".to_string(),
        name: "FLUX.2 Pro".to_string(),
        output: vec![ModelInput::Image],
        headers: None,
        ..text_image_model()
    }
}

fn context() -> ImagesContext {
    ImagesContext {
        input: vec![BlockContent::Text(TextContent {
            text: "Generate a dog".to_string(),
            ..Default::default()
        })],
    }
}

fn options_with(fetch: Arc<RecordingFetch>) -> ImagesOptions {
    ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch),
        ..Default::default()
    }
}

fn captured_body(fetch: &RecordingFetch) -> Value {
    let requests = fetch.requests.lock().unwrap();
    let request = requests.last().expect("expected a captured request");
    match &request.body {
        pi_core::ai::utils::http::HttpBody::Json(value) => value.clone(),
        other => panic!("expected JSON request body, got {other:?}"),
    }
}

#[tokio::test]
async fn returns_text_plus_images_in_final_output() {
    let fetch = Arc::new(RecordingFetch {
        body: RESPONSE_BODY.to_string(),
        requests: Mutex::new(Vec::new()),
    });

    let output = generate_images(
        &text_image_model(),
        &context(),
        Some(&options_with(Arc::clone(&fetch))),
    )
    .await;

    assert_eq!(output.stop_reason, ImagesStopReason::Stop);
    assert_eq!(output.response_id.as_deref(), Some("img-1"));
    assert_eq!(
        output.output[0],
        BlockContent::Text(TextContent {
            text: "Here is your image.".to_string(),
            ..Default::default()
        })
    );
    assert_eq!(
        output.output[1],
        BlockContent::Image(pi_core::ai::types::ImageContent {
            content_type: Default::default(),
            data: "ZmFrZS1wbmc=".to_string(),
            mime_type: "image/png".to_string(),
        })
    );

    let usage = output.usage.as_ref().expect("expected usage");
    assert_eq!(usage.input, 12);
    assert_eq!(usage.output, 34);
    assert_eq!(usage.cache_read, 0);
    assert_eq!(usage.cache_write, 0);
    assert_eq!(usage.total_tokens, 46);

    let params = captured_body(&fetch);
    assert_eq!(params["stream"], false);
    assert_eq!(params["modalities"], serde_json::json!(["image", "text"]));
    assert_eq!(
        params["messages"][0]["content"][0],
        serde_json::json!({ "type": "text", "text": "Generate a dog" })
    );

    // The model headers ride along as SDK default headers next to the bearer.
    let requests = fetch.requests.lock().unwrap();
    let request = requests.last().unwrap();
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("authorization").as_deref(), Some("Bearer test"));
    assert_eq!(
        header("HTTP-Referer").as_deref(),
        Some("https://example.com")
    );
    assert_eq!(request.url, "https://openrouter.ai/api/v1/chat/completions");
}

#[tokio::test]
async fn passes_through_abort_signal_and_returns_aborted_result() {
    let fetch = Arc::new(RecordingFetch {
        body: RESPONSE_BODY.to_string(),
        requests: Mutex::new(Vec::new()),
    });
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let options = ImagesOptions {
        signal: Some(signal),
        ..options_with(Arc::clone(&fetch))
    };

    let output = generate_images(&image_only_model(), &context(), Some(&options)).await;

    assert_eq!(output.stop_reason, ImagesStopReason::Aborted);
    assert_eq!(output.error_message.as_deref(), Some("Request aborted"));
    // The TS case asserts the SDK requestOptions carried the signal; the
    // Rust transport sees it on the request.
    let requests = fetch.requests.lock().unwrap();
    let signal = requests[0]
        .signal
        .as_ref()
        .expect("signal forwarded with the request");
    assert!(signal.is_cancelled());
}

#[tokio::test]
async fn generate_images_resolves_the_final_assistant_images_result() {
    let fetch = Arc::new(RecordingFetch {
        body: RESPONSE_BODY.to_string(),
        requests: Mutex::new(Vec::new()),
    });

    let output = generate_images(
        &image_only_model(),
        &context(),
        Some(&options_with(Arc::clone(&fetch))),
    )
    .await;

    assert!(
        output
            .output
            .iter()
            .any(|item| matches!(item, BlockContent::Image(_)))
    );
    let requests = fetch.requests.lock().unwrap();
    let request = requests.last().unwrap();
    match &request.body {
        pi_core::ai::utils::http::HttpBody::Json(value) => {
            assert_eq!(value["modalities"], serde_json::json!(["image"]));
        }
        other => panic!("expected JSON request body, got {other:?}"),
    }
}

// Port of `provider-error-body-passthrough.test.ts`: a 403 from a gateway or
// proxy carrying the real reason in the body. The TypeScript test mocks the
// OpenAI SDK so `withResponse()` rejects with a `FakeAPIError` whose message
// is the opaque `"403 status code (no body)"` while the parsed body stays on
// `error.error`; here the gateway 403 with body flows through the stub
// transport and the provider normalizes status + body. Dispatch runs through
// `images::generate_images`, the port of the `images.ts` entry point the
// TypeScript test imports.
struct GatewayErrorFetch {
    status: u16,
    body: String,
}

impl HttpFetch for GatewayErrorFetch {
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

#[tokio::test]
async fn surfaces_the_http_body_reason_instead_of_the_opaque_sdk_message() {
    let fetch = Arc::new(GatewayErrorFetch {
        status: 403,
        body: r#"{"error":"blocked by gateway WAF"}"#.to_string(),
    });
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch),
        ..Default::default()
    };

    let output =
        pi_core::ai::images::generate_images(&image_only_model(), &context(), Some(&options))
            .await
            .expect("dispatch resolves");

    assert_eq!(output.stop_reason, ImagesStopReason::Error);
    let error_message = output.error_message.expect("error message present");
    // The status should be surfaced.
    assert!(error_message.contains("403"));
    // The body reason must not be swallowed by the opaque SDK message.
    assert!(error_message.contains("blocked by gateway WAF"));
    assert_ne!(error_message, "403 status code (no body)");
}
