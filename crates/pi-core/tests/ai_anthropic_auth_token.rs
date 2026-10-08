//! Port of `pi-core/ai/test/anthropic-auth-token.test.ts`.
//!
//! The TS suite mocks `@anthropic-ai/sdk` and inspects SDK constructor
//! options; the Rust port replaces the SDK with a mock HTTP transport and
//! asserts on the captured request instead (the observable equivalent).

use std::sync::Arc;

use pi_core::ai::api::anthropic_messages::{AnthropicOptions, stream as stream_anthropic};
use pi_core::ai::auth::types::{ApiKeyAuthInput, AuthContext, AuthFuture, AuthResult};
use pi_core::ai::models::{CreateModelsOptions, Models};
use pi_core::ai::providers::anthropic::anthropic_provider;
use pi_core::ai::types::{
    Context, Model, ModelInput, ProviderEnv, ProviderHeaders, SimpleStreamOptions, StreamOptions,
    UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpRequest, HttpResponse};

fn test_model() -> Model {
    Model {
        id: "claude-test".to_string(),
        name: "Claude Test".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 100_000,
        max_tokens: 4096,
        ..Default::default()
    }
}

fn context() -> Context {
    Context {
        system_prompt: Some("System prompt.".to_string()),
        messages: vec![pi_core::ai::types::Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

struct SseMockFetch {
    requests: std::sync::Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for SseMockFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<
        'a,
        Result<HttpResponse, pi_core::ai::utils::http::HttpFetchError>,
    > {
        self.requests.lock().unwrap().push(request);
        let body = [
            format!(
                "event: message_start\ndata: {}\n",
                serde_json::json!({
                    "type": "message_start",
                    "message": { "id": "msg_test", "usage": { "input_tokens": 1, "output_tokens": 0 } },
                })
            ),
            format!(
                "event: message_delta\ndata: {}\n",
                serde_json::json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 1 },
                })
            ),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n".to_string(),
        ]
        .join("\n");
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

/// The per-test auth context standing in for the TS inline
/// `{ env: async (name) => ... }` contexts.
struct FixedEnvContext {
    env: ProviderEnv,
}

impl AuthContext for FixedEnvContext {
    fn env(&self, name: &str) -> AuthFuture<Option<String>> {
        Box::pin(std::future::ready(self.env.get(name).cloned()))
    }

    fn file_exists(&self, _path: &str) -> AuthFuture<bool> {
        Box::pin(std::future::ready(false))
    }
}

fn auth_input(env: ProviderEnv) -> ApiKeyAuthInput {
    ApiKeyAuthInput {
        ctx: Arc::new(FixedEnvContext { env }),
        credential: None,
        signal: tokio_util::sync::CancellationToken::new(),
    }
}

fn env_from(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn simple_options(
    fetch: Arc<SseMockFetch>,
    headers: Option<ProviderHeaders>,
) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                fetch: Some(fetch),
                headers,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn resolves_anthropic_auth_token_as_a_bearer_authorization_header() {
    let provider = anthropic_provider();
    let auth = provider
        .auth()
        .api_key
        .as_ref()
        .expect("api key auth")
        .resolve(auth_input(env_from(&[
            ("ANTHROPIC_AUTH_TOKEN", "auth-token"),
            ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
            ("ANTHROPIC_API_KEY", "api-key"),
        ])))
        .await
        .unwrap()
        .expect("configured");

    let mut expected_headers = ProviderHeaders::new();
    expected_headers.insert(
        "Authorization".to_string(),
        Some("Bearer auth-token".to_string()),
    );
    assert_eq!(
        auth,
        AuthResult {
            auth: pi_core::ai::auth::types::ModelAuth {
                headers: Some(expected_headers),
                ..Default::default()
            },
            env: None,
            source: Some("ANTHROPIC_AUTH_TOKEN".to_string()),
        }
    );
}

#[tokio::test]
async fn preserves_anthropic_oauth_token_as_oauth_shaped_api_auth() {
    let provider = anthropic_provider();
    let auth = provider
        .auth()
        .api_key
        .as_ref()
        .expect("api key auth")
        .resolve(auth_input(env_from(&[
            ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
            ("ANTHROPIC_API_KEY", "api-key"),
        ])))
        .await
        .unwrap()
        .expect("configured");

    assert_eq!(
        auth,
        AuthResult {
            auth: pi_core::ai::auth::types::ModelAuth {
                api_key: Some("oauth-token".to_string()),
                ..Default::default()
            },
            env: None,
            source: Some("ANTHROPIC_OAUTH_TOKEN".to_string()),
        }
    );
}

#[tokio::test]
async fn uses_authorization_headers_without_oauth_mode_request_shaping() {
    let fetch = Arc::new(SseMockFetch {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let mut headers = ProviderHeaders::new();
    headers.insert(
        "Authorization".to_string(),
        Some("Bearer gateway-token".to_string()),
    );
    let options = AnthropicOptions {
        base: StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                fetch: Some(fetch.clone()),
                headers: Some(headers),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let result = stream_anthropic(&test_model(), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(header(request, "x-api-key"), None);
    assert_eq!(
        header(request, "Authorization"),
        Some("Bearer gateway-token")
    );
    let beta = header(request, "anthropic-beta").unwrap_or("");
    assert!(!beta.contains("oauth-2025-04-20"));
    let HttpBody::Json(body) = &request.body else {
        unreachable!("json body");
    };
    let system = body
        .get("system")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(
        system[0].get("text"),
        Some(&serde_json::json!("System prompt."))
    );
}

#[tokio::test]
async fn threads_auth_context_anthropic_auth_token_through_request_headers() {
    let models = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(Arc::new(FixedEnvContext {
            env: env_from(&[("ANTHROPIC_AUTH_TOKEN", "ctx-token")]),
        })),
        ..Default::default()
    }));
    models.set_provider(anthropic_provider());
    let fetch = Arc::new(SseMockFetch {
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = models
        .stream_simple(
            &test_model(),
            &context(),
            Some(simple_options(fetch.clone(), None)),
        )
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(header(request, "x-api-key"), None);
    assert_eq!(header(request, "Authorization"), Some("Bearer ctx-token"));
    let beta = header(request, "anthropic-beta").unwrap_or("");
    assert!(!beta.contains("oauth-2025-04-20"));
    let HttpBody::Json(body) = &request.body else {
        unreachable!("json body");
    };
    let system = body
        .get("system")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(
        system[0].get("text"),
        Some(&serde_json::json!("System prompt."))
    );
}

#[tokio::test]
async fn preserves_oauth_request_shaping_for_anthropic_oauth_token() {
    let models = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(Arc::new(FixedEnvContext {
            env: env_from(&[("ANTHROPIC_OAUTH_TOKEN", "sk-ant-oat-test")]),
        })),
        ..Default::default()
    }));
    models.set_provider(anthropic_provider());
    let fetch = Arc::new(SseMockFetch {
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = models
        .stream_simple(
            &test_model(),
            &context(),
            Some(simple_options(fetch.clone(), None)),
        )
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(header(request, "x-api-key"), None);
    assert_eq!(
        header(request, "Authorization"),
        Some("Bearer sk-ant-oat-test")
    );
    assert!(
        header(request, "anthropic-beta")
            .unwrap_or("")
            .contains("oauth-2025-04-20")
    );
}

#[tokio::test]
async fn lets_explicit_request_headers_override_anthropic_auth_token() {
    let models = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(Arc::new(FixedEnvContext {
            env: env_from(&[("ANTHROPIC_AUTH_TOKEN", "ctx-token")]),
        })),
        ..Default::default()
    }));
    models.set_provider(anthropic_provider());
    let fetch = Arc::new(SseMockFetch {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let mut headers = ProviderHeaders::new();
    headers.insert(
        "Authorization".to_string(),
        Some("Bearer explicit-token".to_string()),
    );

    let result = models
        .stream_simple(
            &test_model(),
            &context(),
            Some(simple_options(fetch.clone(), Some(headers))),
        )
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(
        header(&requests[0], "Authorization"),
        Some("Bearer explicit-token")
    );
}

#[tokio::test]
async fn uses_pi_user_agent_by_default_for_anthropic_messages_requests() {
    let fetch = Arc::new(SseMockFetch {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let options = AnthropicOptions {
        base: StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("anthropic-key".to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let result = stream_anthropic(&test_model(), &context(), Some(&options))
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(
        header(&requests[0], "User-Agent"),
        Some(pi_core::ai::session_resources::get_pi_user_agent().as_str())
    );
}

#[tokio::test]
async fn lets_explicit_headers_override_the_default_user_agent() {
    let fetch = Arc::new(SseMockFetch {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let mut kimi_model = test_model();
    kimi_model.id = "kimi-for-coding".to_string();
    kimi_model.name = "Kimi For Coding".to_string();
    kimi_model.provider = "kimi-coding".to_string();
    kimi_model.base_url = "https://api.kimi.com/coding".to_string();
    let mut headers = ProviderHeaders::new();
    headers.insert("User-Agent".to_string(), Some("custom-client".to_string()));
    let options = AnthropicOptions {
        base: StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("kimi-key".to_string()),
                fetch: Some(fetch.clone()),
                headers: Some(headers),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let result = stream_anthropic(&kimi_model, &context(), Some(&options))
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(header(&requests[0], "User-Agent"), Some("custom-client"));
}
