//! Parity tests for OpenAI-SDK default-header merge semantics: a null entry
//! in provider headers deletes the header it names — including the SDK's own
//! auth header (`authorization` / Azure `api-key`) — and a non-null entry
//! replaces it. Mirrors the contract exercised by the TypeScript
//! `cloudflare-gateway-binding.test.ts` sentinel scenario.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::azure_openai_responses::{
    AzureOpenAIResponsesOptions, stream as stream_azure,
};
use pi_core::ai::api::openai_completions::{
    OpenAICompletionsOptions, stream as stream_completions,
};
use pi_core::ai::api::openai_responses::{OpenAIResponsesOptions, stream as stream_responses};
use pi_core::ai::types::ProviderHeaders;
use pi_core::ai::types::{Context, Message, Model, RoleUser, UserMessage};
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};

/// Mock transport capturing requests; a 400 reply is enough to terminate the
/// stream in an error result without valid SSE.
struct RecordingFetch {
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for RecordingFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(HttpResponse {
                status: 400,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                    r#"{"error": {"type": "bad_request", "message": "stubbed"}}"#,
                ))])),
            })
        })
    }
}

fn model(api: &str) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: api.to_string(),
        provider: "openai".to_string(),
        base_url: "https://example.openai.azure.com/openai".to_string(),
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
            role: RoleUser,
            content: pi_core::ai::types::UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

fn provider_request_options(
    fetch: Arc<RecordingFetch>,
    headers: ProviderHeaders,
) -> pi_core::ai::types::ProviderRequestOptions {
    pi_core::ai::types::ProviderRequestOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch),
        headers: Some(headers),
        ..Default::default()
    }
}

fn captured_headers(fetch: &RecordingFetch) -> Vec<(String, String)> {
    fetch.requests.lock().unwrap()[0].headers.clone()
}

fn values_of<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

#[tokio::test]
async fn completions_sdk_auth_merge_semantics() {
    // Default: exactly one SDK auth header.
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = OpenAICompletionsOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(Arc::clone(&fetch), Default::default()),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_completions(&model("openai-completions"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "authorization"),
        vec!["Bearer test"]
    );

    // `authorization: null` deletes the SDK auth header.
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = OpenAICompletionsOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(
                Arc::clone(&fetch),
                [("authorization".to_string(), None)].into_iter().collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_completions(&model("openai-completions"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "authorization"),
        Vec::<&str>::new()
    );

    // A non-null entry replaces the SDK auth header (no duplicate).
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = OpenAICompletionsOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(
                Arc::clone(&fetch),
                [(
                    "authorization".to_string(),
                    Some("Bearer custom".to_string()),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_completions(&model("openai-completions"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "authorization"),
        vec!["Bearer custom"]
    );
}

#[tokio::test]
async fn responses_sdk_auth_merge_semantics() {
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = OpenAIResponsesOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(Arc::clone(&fetch), Default::default()),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_responses(&model("openai-responses"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "authorization"),
        vec!["Bearer test"]
    );

    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = OpenAIResponsesOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(
                Arc::clone(&fetch),
                [("authorization".to_string(), None)].into_iter().collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_responses(&model("openai-responses"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "authorization"),
        Vec::<&str>::new()
    );
}

#[tokio::test]
async fn azure_sdk_api_key_merge_semantics() {
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = AzureOpenAIResponsesOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(Arc::clone(&fetch), Default::default()),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_azure(&model("azure-openai-responses"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "api-key"),
        vec!["test"]
    );

    // `api-key: null` deletes the SDK auth header.
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = AzureOpenAIResponsesOptions {
        base: pi_core::ai::types::StreamOptions {
            base: provider_request_options(
                Arc::clone(&fetch),
                [("api-key".to_string(), None)].into_iter().collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    stream_azure(&model("azure-openai-responses"), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(
        values_of(&captured_headers(&fetch), "api-key"),
        Vec::<&str>::new()
    );
}
