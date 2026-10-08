//! Port of `pi-core/ai/test/kimi-coding-oauth.test.ts`. Scripted `HttpFetch`
//! transport + injected clock/env, paused tokio time.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::kimi_coding::KimiCodingOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthStorageError, OAuthAuth, OAuthCredential,
};
use pi_core::ai::types::{FetchFunction, ProviderEnv};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const FROZEN_NOW_MS: i64 = 1_784_524_800_000; // 2026-07-20T00:00:00Z

fn device_authorization_response(overrides: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut body = serde_json::json!({
        "user_code": "ABCD-1234",
        "device_code": "device-code-123",
        "verification_uri": "https://www.kimi.com/code",
        "verification_uri_complete": "https://www.kimi.com/code?user_code=ABCD-1234",
        "interval": 5,
        "expires_in": 600,
    });
    for (key, value) in overrides {
        if value.is_null() {
            body.as_object_mut().unwrap().remove(*key);
        } else {
            body[*key] = value.clone();
        }
    }
    body
}

type FetchHandler = Arc<dyn Fn(&str) -> (u16, serde_json::Value) + Send + Sync>;

struct ScriptedFetch {
    handler: FetchHandler,
    requests: Mutex<Vec<(String, String)>>,
}

impl HttpFetch for ScriptedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let body_text = match &request.body {
            HttpBody::Text(text) => text.clone(),
            other => panic!("expected form body, got {other:?}"),
        };
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), body_text));
        let (status, body) = (self.handler)(&request.url);
        let payload = serde_json::to_string(&body).unwrap();
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(payload))])),
            })
        })
    }
}

fn flow(fetch: Arc<ScriptedFetch>, env: ProviderEnv) -> KimiCodingOAuth {
    KimiCodingOAuth::new(fetch as FetchFunction, Arc::new(|| FROZEN_NOW_MS), env)
}

fn env_with(entries: &[(&str, &str)]) -> ProviderEnv {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<BTreeMap<_, _>>()
}

struct RecordingInteraction {
    events: Arc<Mutex<Vec<AuthEvent>>>,
}

impl AuthInteraction for RecordingInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        None
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
        unreachable!("Kimi Code login should not prompt")
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn form_of(requests: &[(String, String)], index: usize) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(requests[index].1.as_bytes())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

async fn login(flow: &KimiCodingOAuth) -> Result<OAuthCredential, AuthStorageError> {
    flow.login(Arc::new(RecordingInteraction {
        events: Arc::new(Mutex::new(Vec::new())),
    }))
    .await
}

async fn refresh(flow: &KimiCodingOAuth, token: &str) -> Result<OAuthCredential, AuthStorageError> {
    let credential = OAuthCredential {
        access: "old-access".to_string(),
        refresh: token.to_string(),
        expires: 0,
        extra: Default::default(),
    };
    flow.refresh(&credential, CancellationToken::new()).await
}

#[tokio::test(start_paused = true)]
async fn logs_in_with_the_device_authorization_flow() {
    let poll_responses = Arc::new(Mutex::new(vec![
        (
            400u16,
            serde_json::json!({"error": "authorization_pending"}),
        ),
        (
            200u16,
            serde_json::json!({"access_token": "access-token", "refresh_token": "refresh-token", "expires_in": 3600}),
        ),
    ]));
    let poll_responses_for_handler = Arc::clone(&poll_responses);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url == "https://auth.kimi.com/api/oauth/device_authorization" {
                (200, device_authorization_response(&[]))
            } else if url == "https://auth.kimi.com/api/oauth/token" {
                poll_responses_for_handler.lock().unwrap().remove(0)
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let interaction = RecordingInteraction {
        events: Arc::clone(&events),
    };

    let credential = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction))
        .await
        .unwrap();

    // Device authorization request form.
    let requests = fetch.requests.lock().unwrap();
    assert_eq!(
        requests[0].0,
        "https://auth.kimi.com/api/oauth/device_authorization"
    );
    assert_eq!(form_of(&requests, 0)["client_id"], CLIENT_ID);
    let poll_form = form_of(&requests, 1);
    assert_eq!(
        poll_form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(poll_form["client_id"], CLIENT_ID);
    assert_eq!(poll_form["device_code"], "device-code-123");

    // waitBeforeFirstPoll plus a pending round: two token polls.
    assert_eq!(requests.len(), 3);

    assert_eq!(
        events.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode {
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://www.kimi.com/code?user_code=ABCD-1234".to_string(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(600),
        }]
    );

    // The second poll happens one interval after the first, and expires has
    // no skew in this flow.
    assert_eq!(
        credential,
        OAuthCredential {
            access: "access-token".to_string(),
            refresh: "refresh-token".to_string(),
            expires: FROZEN_NOW_MS + 3600 * 1000,
            extra: Default::default(),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn fails_when_the_device_code_expires() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.kimi.com/api/oauth/device_authorization" {
                (200, device_authorization_response(&[]))
            } else {
                (400, serde_json::json!({"error": "expired_token"}))
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = login(&flow(Arc::clone(&fetch), ProviderEnv::new()))
        .await
        .unwrap_err();
    assert!(error.0.contains("expired"), "{}", error.0);
    assert_eq!(
        error.0,
        "Kimi Code device authorization expired. Please restart login."
    );
}

#[tokio::test(start_paused = true)]
async fn fails_when_the_user_denies_the_login() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.kimi.com/api/oauth/device_authorization" {
                (200, device_authorization_response(&[]))
            } else {
                (400, serde_json::json!({"error": "access_denied"}))
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = login(&flow(Arc::clone(&fetch), ProviderEnv::new()))
        .await
        .unwrap_err();
    assert_eq!(error.0, "Kimi Code login was denied.");
}

#[tokio::test(start_paused = true)]
async fn honors_the_kimi_code_oauth_host_override() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.example.com/api/oauth/device_authorization" {
                (
                    200,
                    device_authorization_response(&[("interval", serde_json::json!(1))]),
                )
            } else if url == "https://auth.example.com/api/oauth/token" {
                (
                    200,
                    serde_json::json!({"access_token": "a", "refresh_token": "r", "expires_in": 60}),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let credential = login(&flow(
        Arc::clone(&fetch),
        env_with(&[("KIMI_CODE_OAUTH_HOST", "https://auth.example.com/")]),
    ))
    .await
    .unwrap();

    assert_eq!(credential.access, "a");
    assert_eq!(credential.refresh, "r");
    let requests = fetch.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|(url, _)| url.as_str())
            .collect::<Vec<_>>(),
        vec![
            "https://auth.example.com/api/oauth/device_authorization",
            "https://auth.example.com/api/oauth/token",
        ]
    );
}

#[tokio::test]
async fn refreshes_tokens_and_returns_a_bearer_header_for_requests() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                serde_json::json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600}),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let flow = flow(Arc::clone(&fetch), ProviderEnv::new());

    let credential = refresh(&flow, "old-refresh").await.unwrap();

    assert_eq!(credential.access, "new-access");
    assert_eq!(credential.refresh, "new-refresh");
    assert_eq!(credential.expires, FROZEN_NOW_MS + 3600 * 1000);

    let (url, form) = {
        let requests = fetch.requests.lock().unwrap();
        (requests[0].0.clone(), form_of(&requests, 0))
    };
    assert_eq!(url, "https://auth.kimi.com/api/oauth/token");
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "old-refresh");
    assert_eq!(form["client_id"], CLIENT_ID);

    let auth = flow.to_auth(&credential).await.unwrap();
    let headers = auth.headers.unwrap();
    assert_eq!(
        headers
            .get("Authorization")
            .and_then(|value| value.as_deref()),
        Some("Bearer new-access")
    );
}

#[tokio::test(start_paused = true)]
async fn retries_refresh_on_429_and_fails_unauthorized_on_invalid_grant() {
    // 429 once, then success.
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_handler = Arc::clone(&calls);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |_| {
            let mut count = calls_for_handler.lock().unwrap();
            *count += 1;
            if *count == 1 {
                (429, serde_json::json!({"error": "temporarily_unavailable"}))
            } else {
                (
                    200,
                    serde_json::json!({"access_token": "a", "refresh_token": "r", "expires_in": 60}),
                )
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let credential = refresh(&flow(Arc::clone(&fetch), ProviderEnv::new()), "old")
        .await
        .unwrap();
    assert_eq!(credential.access, "a");
    assert_eq!(*calls.lock().unwrap(), 2);

    // invalid_grant is not retried.
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (400, serde_json::json!({"error": "invalid_grant"}))),
        requests: Mutex::new(Vec::new()),
    });

    let error = refresh(&flow(Arc::clone(&fetch), ProviderEnv::new()), "old")
        .await
        .unwrap_err();
    assert!(error.0.contains("unauthorized"), "{}", error.0);
    assert_eq!(error.0, "Kimi Code token refresh unauthorized (status 400)");
}
