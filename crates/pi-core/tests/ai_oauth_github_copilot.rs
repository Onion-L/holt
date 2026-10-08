//! Port of `pi-core/ai/test/github-copilot-oauth.test.ts`. The
//! `createModels`/provider-integration portions of the catalog cases land
//! with the provider factories; the flow behavior is fully covered here.
//! Paused tokio time drives the fake-timer scheduling.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_core::ai::auth::oauth::github_copilot::{
    GitHubCopilotOAuth, available_model_ids, github_copilot_oauth,
};
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, CredentialStore,
    OAuthAuth, OAuthCredential,
};
use pi_core::ai::types::FetchFunction;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

const TEST_COPILOT_ACCESS_TOKEN: &str =
    "tid=test;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;";
const TEST_COPILOT_MODELS_URL: &str = "https://api.individual.githubcopilot.com/models";
const FROZEN_NOW_MS: i64 = 1_772_966_400_000; // 2026-03-09T00:00:00Z

/// One scripted reply: status, headers, body. Status 0 models a transport
/// failure carrying the error message.
type Reply = (u16, Vec<(String, String)>, String);

type RouteHandler = Arc<dyn Fn(&str) -> Reply + Send + Sync>;
type PolicyHandler = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

struct ScriptedFetch {
    handler: RouteHandler,
    requests: Mutex<Vec<(String, String)>>,
}

impl HttpFetch for ScriptedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let body_text = match &request.body {
            HttpBody::Text(text) => text.clone(),
            HttpBody::Json(value) => value.to_string(),
            HttpBody::Empty => String::new(),
            other => panic!("unexpected body: {other:?}"),
        };
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), body_text));
        let (status, headers, body) = (self.handler)(&request.url);
        if status == 0 {
            return Box::pin(async move { Err(HttpFetchError::Request(body)) });
        }
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn json_reply(body: serde_json::Value) -> Reply {
    (
        200,
        vec![("content-type".to_string(), "application/json".to_string())],
        serde_json::to_string(&body).unwrap(),
    )
}

fn throttled(retry_after: &str) -> Reply {
    (
        429,
        vec![("retry-after".to_string(), retry_after.to_string())],
        r#"{"error":"too many requests"}"#.to_string(),
    )
}

fn empty_ok() -> Reply {
    (200, Vec::new(), String::new())
}

fn flow(fetch: Arc<ScriptedFetch>) -> GitHubCopilotOAuth {
    GitHubCopilotOAuth::new(fetch as FetchFunction, Arc::new(|| FROZEN_NOW_MS))
}

struct LoginInteraction {
    events: Arc<Mutex<Vec<AuthEvent>>>,
}

impl AuthInteraction for LoginInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        None
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
        assert!(matches!(prompt.kind, AuthPromptKind::Text { .. }));
        Box::pin(std::future::ready(Ok(String::new())))
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Standard login routes with injectable models/policy behavior.
fn login_routes(
    models: Arc<dyn Fn() -> Reply + Send + Sync>,
    policy: Option<PolicyHandler>,
) -> RouteHandler {
    Arc::new(move |url| {
        if url.ends_with("/login/device/code") {
            json_reply(serde_json::json!({
                "device_code": "device-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://github.com/login/device",
                "interval": 1,
                "expires_in": 900,
            }))
        } else if url.ends_with("/login/oauth/access_token") {
            json_reply(serde_json::json!({"access_token": "ghu_refresh_token"}))
        } else if url.contains("/copilot_internal/v2/token") {
            json_reply(serde_json::json!({
                "token": TEST_COPILOT_ACCESS_TOKEN,
                "expires_at": 9_999_999_999i64,
            }))
        } else if url == TEST_COPILOT_MODELS_URL {
            models()
        } else if url.starts_with(&format!("{TEST_COPILOT_MODELS_URL}/"))
            && url.ends_with("/policy")
        {
            let model_id = &url[TEST_COPILOT_MODELS_URL.len() + 1..url.len() - "/policy".len()];
            policy.as_ref().expect("unexpected policy request")(model_id)
        } else {
            panic!("unexpected fetch URL: {url}")
        }
    })
}

/// Login with scriptable token-poll replies; records poll offsets.
fn device_login_routes(
    token_replies: Arc<Mutex<Vec<serde_json::Value>>>,
    poll_times: Arc<Mutex<Vec<Duration>>>,
    start: tokio::time::Instant,
    device_interval: serde_json::Value,
    expires_in: serde_json::Value,
) -> RouteHandler {
    Arc::new(move |url| {
        if url.ends_with("/login/device/code") {
            let mut body = serde_json::json!({
                "device_code": "device-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://github.com/login/device",
            });
            body["interval"] = device_interval.clone();
            body["expires_in"] = expires_in.clone();
            json_reply(body)
        } else if url.ends_with("/login/oauth/access_token") {
            poll_times.lock().unwrap().push(start.elapsed());
            let reply = token_replies.lock().unwrap().remove(0);
            json_reply(reply)
        } else if url.contains("/copilot_internal/v2/token") {
            json_reply(serde_json::json!({
                "token": TEST_COPILOT_ACCESS_TOKEN,
                "expires_at": 9_999_999_999i64,
            }))
        } else if url.ends_with("/models") {
            json_reply(serde_json::json!({"data": []}))
        } else if url.contains("/models/") && url.ends_with("/policy") {
            empty_ok()
        } else {
            panic!("unexpected fetch URL: {url}")
        }
    })
}

async fn login(fetch: Arc<ScriptedFetch>) -> Result<OAuthCredential, AuthStorageError> {
    flow(Arc::clone(&fetch))
        .login(Arc::new(LoginInteraction {
            events: Arc::new(Mutex::new(Vec::new())),
        }))
        .await
}

async fn refresh(fetch: Arc<ScriptedFetch>) -> Result<OAuthCredential, AuthStorageError> {
    let credential = OAuthCredential {
        access: "old-access-token".to_string(),
        refresh: "ghu_refresh_token".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    flow(Arc::clone(&fetch))
        .refresh(&credential, CancellationToken::new())
        .await
}

fn refresh_models_routes(
    data: serde_json::Value,
    proxy_host: &str,
) -> (Arc<ScriptedFetch>, String) {
    let models_url = format!(
        "https://{}/models",
        proxy_host.replacen("proxy.", "api.", 1)
    );
    let access_token = format!("tid=test;exp=9999999999;proxy-ep={proxy_host};");
    let models_url_for_handler = models_url.clone();
    let access_token_for_handler = access_token.clone();
    (
        Arc::new(ScriptedFetch {
            handler: Arc::new(move |url| {
                if url.contains("/copilot_internal/v2/token") {
                    json_reply(serde_json::json!({
                        "token": access_token_for_handler,
                        "expires_at": 9_999_999_999i64,
                    }))
                } else if url == models_url_for_handler {
                    json_reply(serde_json::json!({"data": data}))
                } else {
                    panic!("unexpected fetch URL: {url}")
                }
            }),
            requests: Mutex::new(Vec::new()),
        }),
        models_url,
    )
}

/// Mirrors the TS `requireModelId(providerModels, index)` helper: model ids
/// come from the generated github-copilot catalog so upstream catalog
/// refreshes do not break these tests.
fn provider_model_id(index: usize) -> String {
    pi_core::ai::models_generated::models_for_provider("github-copilot")[index]
        .id
        .clone()
}

#[tokio::test]
async fn filters_models_to_the_authenticated_account_picker_catalog() {
    let picker_model_id = provider_model_id(0);
    let disabled_model_id = provider_model_id(1);
    let hidden_model_id = provider_model_id(2);
    let (fetch, _) = refresh_models_routes(
        serde_json::json!([
            { "id": picker_model_id.clone(), "model_picker_enabled": true, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": disabled_model_id, "model_picker_enabled": true, "policy": { "state": "disabled" }, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": hidden_model_id, "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": true } } },
        ]),
        "proxy.individual.githubcopilot.com",
    );

    let credentials = refresh(Arc::clone(&fetch)).await.unwrap();

    assert_eq!(available_model_ids(&credentials), vec![picker_model_id]);
}

#[tokio::test]
async fn falls_back_to_explicitly_enabled_policy_models_when_picker_is_empty() {
    let enabled_model_id = provider_model_id(0);
    let (fetch, _) = refresh_models_routes(
        serde_json::json!([
            { "id": enabled_model_id.clone(), "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "policy-disabled-model", "model_picker_enabled": false, "policy": { "state": "disabled" }, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "unconfigured-model", "model_picker_enabled": false, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "tool-incapable-model", "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": false } } },
        ]),
        "proxy.individual.githubcopilot.com",
    );

    let credentials = refresh(Arc::clone(&fetch)).await.unwrap();

    assert_eq!(available_model_ids(&credentials), vec![enabled_model_id]);
}

#[tokio::test]
async fn does_not_fall_back_to_policy_models_for_non_individual_accounts() {
    let (fetch, _) = refresh_models_routes(
        serde_json::json!([
            { "id": "gpt-4.1", "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": true } } },
        ]),
        "proxy.business.githubcopilot.com",
    );

    let credentials = refresh(Arc::clone(&fetch)).await.unwrap();

    assert_eq!(available_model_ids(&credentials), Vec::<String>::new());
}

#[tokio::test]
async fn does_not_retry_model_catalog_throttling_during_credential_refresh() {
    let catalog_request_count = Arc::new(Mutex::new(0u32));
    let count_for_handler = Arc::clone(&catalog_request_count);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url.contains("/copilot_internal/v2/token") {
                json_reply(serde_json::json!({
                    "token": TEST_COPILOT_ACCESS_TOKEN,
                    "expires_at": 9_999_999_999i64,
                }))
            } else if url == TEST_COPILOT_MODELS_URL {
                *count_for_handler.lock().unwrap() += 1;
                throttled("0")
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = refresh(Arc::clone(&fetch)).await.unwrap_err();

    assert!(error.0.contains("429"), "{}", error.0);
    assert_eq!(*catalog_request_count.lock().unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn reports_device_code_details_through_notify() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url.ends_with("/login/device/code") {
                json_reply(serde_json::json!({
                    "device_code": "device-code",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://github.com/login/device",
                    "interval": 1,
                    "expires_in": 900,
                }))
            } else if url.ends_with("/login/oauth/access_token") {
                json_reply(serde_json::json!({"access_token": "ghu_refresh_token"}))
            } else if url.contains("/copilot_internal/v2/token") {
                json_reply(serde_json::json!({
                    "token": TEST_COPILOT_ACCESS_TOKEN,
                    "expires_at": 9_999_999_999i64,
                }))
            } else if url.ends_with("/models") {
                json_reply(serde_json::json!({"data": []}))
            } else if url.contains("/models/") && url.ends_with("/policy") {
                empty_ok()
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));

    let credential = flow(Arc::clone(&fetch))
        .login(Arc::new(LoginInteraction {
            events: Arc::clone(&events),
        }))
        .await
        .unwrap();

    assert_eq!(credential.access, TEST_COPILOT_ACCESS_TOKEN);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode {
            user_code: "ABCD-EFGH".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            interval_seconds: Some(1),
            expires_in_seconds: Some(900),
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn updates_only_known_tool_capable_unconfigured_account_model_policies() {
    let configured_model_id = provider_model_id(0);
    let unconfigured_model_id = provider_model_id(1);
    let tool_incapable_model_id = provider_model_id(2);
    let expected_unconfigured = unconfigured_model_id.clone();
    let policy_model_ids = Arc::new(Mutex::new(Vec::new()));
    let policy_for_handler = Arc::clone(&policy_model_ids);
    let fetch = Arc::new(ScriptedFetch {
        handler: login_routes(
            Arc::new(move || {
                json_reply(serde_json::json!({"data": [
                    { "id": configured_model_id, "model_picker_enabled": true, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": true } } },
                    { "id": unconfigured_model_id, "model_picker_enabled": true, "policy": { "state": "unconfigured" }, "capabilities": { "supports": { "tool_calls": true } } },
                    { "id": "remote-only-model", "model_picker_enabled": true, "policy": { "state": "unconfigured" }, "capabilities": { "supports": { "tool_calls": true } } },
                    { "id": tool_incapable_model_id, "model_picker_enabled": true, "policy": { "state": "unconfigured" }, "capabilities": { "supports": { "tool_calls": false } } },
                ]}))
            }),
            Some(Arc::new(move |model_id| {
                policy_for_handler
                    .lock()
                    .unwrap()
                    .push(model_id.to_string());
                empty_ok()
            })),
        ),
        requests: Mutex::new(Vec::new()),
    });

    login(Arc::clone(&fetch)).await.unwrap();

    assert_eq!(
        policy_model_ids.lock().unwrap().as_slice(),
        [expected_unconfigured]
    );
    // The catalog was requested exactly once.
    let catalog_requests = fetch
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(url, _)| url.ends_with("/models") && !url.contains("/policy"))
        .count();
    assert_eq!(catalog_requests, 1);
}

#[tokio::test(start_paused = true)]
async fn retries_a_throttled_policy_update_after_retry_after() {
    let model_id = provider_model_id(0);
    let policy_request_count = Arc::new(Mutex::new(0u32));
    let count_for_handler = Arc::clone(&policy_request_count);
    let fetch = Arc::new(ScriptedFetch {
        handler: login_routes(
            Arc::new(move || {
                json_reply(serde_json::json!({"data": [
                    { "id": model_id, "model_picker_enabled": true, "policy": { "state": "unconfigured" } },
                ]}))
            }),
            Some(Arc::new(move |_| {
                let mut count = count_for_handler.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    throttled("1")
                } else {
                    empty_ok()
                }
            })),
        ),
        requests: Mutex::new(Vec::new()),
    });

    login(Arc::clone(&fetch)).await.unwrap();

    assert_eq!(*policy_request_count.lock().unwrap(), 2);
}

#[tokio::test(start_paused = true)]
async fn continues_policy_updates_after_a_transport_failure() {
    let model_ids = [provider_model_id(0), provider_model_id(1)];
    let ids_for_catalog = model_ids.clone();
    let policy_model_ids = Arc::new(Mutex::new(Vec::new()));
    let policy_for_handler = Arc::clone(&policy_model_ids);
    let fetch = Arc::new(ScriptedFetch {
        handler: login_routes(
            Arc::new(move || {
                let data: Vec<serde_json::Value> = ids_for_catalog
                    .iter()
                    .map(|id| {
                        serde_json::json!({ "id": id, "model_picker_enabled": true, "policy": { "state": "unconfigured" } })
                    })
                    .collect();
                json_reply(serde_json::json!({"data": data}))
            }),
            Some(Arc::new(move |model_id| {
                let mut requested = policy_for_handler.lock().unwrap();
                requested.push(model_id.to_string());
                if requested.len() == 1 {
                    // A transport failure is swallowed per model.
                    (0, Vec::new(), "fetch failed".to_string())
                } else {
                    empty_ok()
                }
            })),
        ),
        requests: Mutex::new(Vec::new()),
    });

    let result = login(Arc::clone(&fetch)).await;

    assert_eq!(policy_model_ids.lock().unwrap().as_slice(), model_ids);
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn rejects_a_non_http_verification_uri_before_it_reaches_notify() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url.ends_with("/login/device/code") {
                json_reply(serde_json::json!({
                    "device_code": "device-code",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "$(id>/tmp/pwned)",
                    "interval": 1,
                    "expires_in": 900,
                }))
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));

    let error = flow(Arc::clone(&fetch))
        .login(Arc::new(LoginInteraction {
            events: Arc::clone(&events),
        }))
        .await
        .unwrap_err();

    assert!(
        error.0.contains("Untrusted verification_uri"),
        "{}",
        error.0
    );
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn normalizes_verification_uri_before_it_reaches_notify() {
    let raw_verification_uri = "https://github.com/login/\u{1b}]8;;evil";
    let normalized = url::Url::parse(raw_verification_uri).unwrap().to_string();
    assert_ne!(normalized, raw_verification_uri);
    let raw_for_handler = raw_verification_uri.to_string();
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url.ends_with("/login/device/code") {
                json_reply(serde_json::json!({
                    "device_code": "device-code",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": raw_for_handler,
                    "interval": 1,
                    "expires_in": 900,
                }))
            } else if url.ends_with("/login/oauth/access_token") {
                json_reply(serde_json::json!({"access_token": "ghu_refresh_token"}))
            } else if url.contains("/copilot_internal/v2/token") {
                json_reply(serde_json::json!({
                    "token": TEST_COPILOT_ACCESS_TOKEN,
                    "expires_at": 9_999_999_999i64,
                }))
            } else if url.ends_with("/models") {
                json_reply(serde_json::json!({"data": []}))
            } else if url.contains("/models/") && url.ends_with("/policy") {
                empty_ok()
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));

    let credential = flow(Arc::clone(&fetch))
        .login(Arc::new(LoginInteraction {
            events: Arc::clone(&events),
        }))
        .await
        .unwrap();

    let AuthEvent::DeviceCode {
        verification_uri, ..
    } = &events.lock().unwrap()[0]
    else {
        panic!("expected device code event");
    };
    assert_eq!(verification_uri, &normalized);
    assert_eq!(credential.access, TEST_COPILOT_ACCESS_TOKEN);
}

#[tokio::test(start_paused = true)]
async fn waits_before_polling_and_increases_the_interval_after_slow_down() {
    let start = tokio::time::Instant::now();
    let token_replies = Arc::new(Mutex::new(vec![
        serde_json::json!({"error": "authorization_pending", "error_description": "pending"}),
        serde_json::json!({"error": "slow_down", "error_description": "slow down", "interval": 7}),
        serde_json::json!({"access_token": "ghu_refresh_token"}),
    ]));
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let fetch = Arc::new(ScriptedFetch {
        handler: device_login_routes(
            Arc::clone(&token_replies),
            Arc::clone(&poll_times),
            start,
            serde_json::json!(5),
            serde_json::json!(900),
        ),
        requests: Mutex::new(Vec::new()),
    });

    let credential = login(Arc::clone(&fetch)).await.unwrap();

    assert_eq!(credential.access, TEST_COPILOT_ACCESS_TOKEN);
    // First poll after the 5s interval, then 5s again, then the
    // server-provided 7s interval after slow_down.
    assert_eq!(
        poll_times.lock().unwrap().as_slice(),
        [
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(17)
        ]
    );

    let requests = fetch.requests.lock().unwrap();
    assert!(requests.iter().any(
        |(url, body)| url.ends_with("/login/device/code") && body.contains("scope=read%3Auser")
    ));
    let poll = requests
        .iter()
        .find(|(url, _)| url.ends_with("/login/oauth/access_token"))
        .unwrap();
    assert!(poll.1.contains("device_code=device-code"));
    assert!(
        poll.1
            .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
    );
}

#[tokio::test(start_paused = true)]
async fn times_out_after_repeated_slow_down_responses() {
    let start = tokio::time::Instant::now();
    let token_replies = Arc::new(Mutex::new(vec![
        serde_json::json!({"error": "slow_down", "error_description": "slow down"}),
        serde_json::json!({"error": "slow_down", "error_description": "still too fast"}),
        serde_json::json!({"error": "authorization_pending", "error_description": "pending"}),
    ]));
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let fetch = Arc::new(ScriptedFetch {
        handler: device_login_routes(
            Arc::clone(&token_replies),
            Arc::clone(&poll_times),
            start,
            serde_json::json!(5),
            serde_json::json!(25),
        ),
        requests: Mutex::new(Vec::new()),
    });

    let error = login(Arc::clone(&fetch)).await.unwrap_err();

    assert!(
        error
            .0
            .starts_with("Device flow timed out after one or more slow_down responses"),
        "{}",
        error.0
    );
    assert_eq!(
        poll_times.lock().unwrap().as_slice(),
        [Duration::from_secs(5), Duration::from_secs(15)]
    );
}

#[tokio::test]
async fn to_auth_derives_the_proxy_endpoint_base_url() {
    // Mirrors the `toAuth` derivation cases from `oauth-auth.test.ts`.
    let access = "tid=abc;exp=123;proxy-ep=proxy.enterprise.example;rest";
    let credential = OAuthCredential {
        access: access.to_string(),
        refresh: "r".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    let auth = github_copilot_oauth().to_auth(&credential).await.unwrap();
    assert_eq!(auth.api_key.as_deref(), Some(access));
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://api.enterprise.example")
    );

    let enterprise = OAuthCredential {
        access: "no-proxy-ep".to_string(),
        refresh: "r".to_string(),
        expires: 0,
        extra: [(
            "enterpriseUrl".to_string(),
            serde_json::json!("https://company.ghe.com"),
        )]
        .into_iter()
        .collect(),
    };
    let auth = github_copilot_oauth().to_auth(&enterprise).await.unwrap();
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://copilot-api.company.ghe.com")
    );

    let individual = OAuthCredential {
        access: "no-proxy-ep".to_string(),
        refresh: "r".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    let auth = github_copilot_oauth().to_auth(&individual).await.unwrap();
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://api.individual.githubcopilot.com")
    );
}

// ---------------------------------------------------------------------------
// Store integration (the Models halves of the picker-catalog cases and the
// login-budget case, landed with the provider factories).

async fn copilot_models_with_credential(
    credential: OAuthCredential,
) -> Arc<pi_core::ai::models::Models> {
    let store = pi_core::ai::auth::credential_store::InMemoryCredentialStore::new();
    let for_store = credential;
    store
        .modify(
            "github-copilot",
            Box::new(move |_| {
                Box::pin(std::future::ready(Ok(Some(
                    pi_core::ai::auth::types::Credential::OAuth(for_store),
                ))))
                    as pi_core::ai::auth::types::AuthFuture<
                        Result<
                            Option<pi_core::ai::auth::types::Credential>,
                            pi_core::ai::auth::types::BoxedAuthError,
                        >,
                    >
            }),
            None,
        )
        .await
        .unwrap();
    let models = Arc::new(pi_core::ai::models::Models::new(
        pi_core::ai::models::CreateModelsOptions {
            credentials: Some(Arc::new(store)),
            ..Default::default()
        },
    ));
    models.set_provider(pi_core::ai::providers::builtin::github_copilot_provider());
    models
}

#[tokio::test]
async fn get_available_filters_models_to_the_authenticated_account_picker_catalog() {
    let picker_model_id = provider_model_id(0);
    let disabled_model_id = provider_model_id(1);
    let hidden_model_id = provider_model_id(2);
    let (fetch, _) = refresh_models_routes(
        serde_json::json!([
            { "id": picker_model_id.clone(), "model_picker_enabled": true, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": disabled_model_id, "model_picker_enabled": true, "policy": { "state": "disabled" }, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": hidden_model_id, "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": true } } },
        ]),
        "proxy.individual.githubcopilot.com",
    );
    let credentials = refresh(Arc::clone(&fetch)).await.unwrap();
    assert_eq!(
        available_model_ids(&credentials),
        vec![picker_model_id.clone()]
    );

    let models = copilot_models_with_credential(credentials).await;
    let available = models
        .get_available(Some("github-copilot"), None)
        .await
        .unwrap();
    let ids: Vec<&str> = available.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec![picker_model_id.as_str()]);
}

#[tokio::test]
async fn get_available_falls_back_to_explicitly_enabled_policy_models() {
    let enabled_model_id = provider_model_id(0);
    let (fetch, _) = refresh_models_routes(
        serde_json::json!([
            { "id": enabled_model_id.clone(), "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "policy-disabled-model", "model_picker_enabled": false, "policy": { "state": "disabled" }, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "unconfigured-model", "model_picker_enabled": false, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "tool-incapable-model", "model_picker_enabled": false, "policy": { "state": "enabled" }, "capabilities": { "supports": { "tool_calls": false } } },
        ]),
        "proxy.individual.githubcopilot.com",
    );
    let credentials = refresh(Arc::clone(&fetch)).await.unwrap();
    assert_eq!(
        available_model_ids(&credentials),
        vec![enabled_model_id.clone()]
    );

    let models = copilot_models_with_credential(credentials).await;
    let available = models
        .get_available(Some("github-copilot"), None)
        .await
        .unwrap();
    let ids: Vec<&str> = available.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec![enabled_model_id.as_str()]);
}

#[tokio::test(start_paused = true)]
async fn stops_policy_updates_and_persists_authentication_when_the_retry_delay_exceeds_the_login_budget()
 {
    let first_model_id = provider_model_id(0);
    let second_model_id = provider_model_id(1);
    let expected_first = first_model_id.clone();
    let policy_model_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let ids = Arc::clone(&policy_model_ids);
    let handler = login_routes(
        Arc::new(move || {
            json_reply(serde_json::json!({"data": [
                { "id": first_model_id, "model_picker_enabled": true, "policy": { "state": "unconfigured" } },
                { "id": second_model_id, "model_picker_enabled": true, "policy": { "state": "unconfigured" } },
            ]}))
        }),
        Some(Arc::new(move |model_id| {
            ids.lock().unwrap().push(model_id.to_string());
            throttled("5")
        })),
    );
    let fetch = Arc::new(ScriptedFetch {
        handler,
        requests: Mutex::new(Vec::new()),
    });
    // A clock that advances with the (paused, auto-advancing) scheduler, like
    // the TS suite's fake timers.
    let start = tokio::time::Instant::now();
    let now_ms = Arc::new(move || FROZEN_NOW_MS + start.elapsed().as_millis() as i64);
    let injected_flow = GitHubCopilotOAuth::new(fetch as FetchFunction, now_ms);

    // The provider is github-copilot's definition with the scripted flow;
    // the TS case achieves the same by stubbing the global fetch.
    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "github-copilot".to_string(),
            organization_id: None,
            name: Some("GitHub Copilot".to_string()),
            base_url: Some("https://api.individual.githubcopilot.com".to_string()),
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth {
                api_key: Some(pi_core::ai::auth::helpers::env_api_key_auth(
                    "GitHub Copilot token",
                    &["COPILOT_GITHUB_TOKEN"],
                )),
                oauth: Some(Arc::new(injected_flow)),
            },
            models: pi_core::ai::models_generated::models_for_provider("github-copilot"),
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::Single(
                pi_core::ai::providers::apis::anthropic_messages_api(),
            ),
        });

    let store = Arc::new(pi_core::ai::auth::credential_store::InMemoryCredentialStore::new());
    let models = Arc::new(pi_core::ai::models::Models::new(
        pi_core::ai::models::CreateModelsOptions {
            credentials: Some(Arc::clone(&store) as Arc<dyn CredentialStore>),
            ..Default::default()
        },
    ));
    models.set_provider(provider);

    let credential = models
        .login(
            "github-copilot",
            pi_core::ai::auth::types::AuthType::OAuth,
            Arc::new(LoginInteraction {
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .await
        .unwrap();

    // TS asserts with toMatchObject: the OAuth type and access token.
    let pi_core::ai::auth::types::Credential::OAuth(oauth) = &credential else {
        panic!("expected an OAuth credential, got {credential:?}");
    };
    assert_eq!(oauth.access, TEST_COPILOT_ACCESS_TOKEN);
    assert_eq!(*policy_model_ids.lock().unwrap(), vec![expected_first]);
    assert_eq!(
        store.read("github-copilot", None).await.unwrap(),
        Some(credential)
    );
}
