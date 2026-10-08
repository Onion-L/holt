//! Port of `pi-core/ai/test/xai-oauth.test.ts`. The TypeScript suite stubs
//! global fetch and fake timers; here a scripted `HttpFetch` transport plus
//! an injected frozen clock and paused tokio time reproduce the same
//! scheduling and expiry arithmetic.

use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::xai::XaiOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthStorageError, OAuthAuth, OAuthCredential,
};
use pi_core::ai::types::FetchFunction;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

/// Frozen wall clock, standing in for `vi.setSystemTime`.
const FROZEN_NOW_MS: i64 = 1_783_929_600_000; // 2026-07-09T20:00:00Z

fn device_code_response(overrides: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut body = serde_json::json!({
        "device_code": "device-code",
        "user_code": "ABCD-1234",
        "verification_uri": "https://accounts.x.ai/oauth2/device",
        "expires_in": 900,
        "interval": 5,
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

fn token_response(overrides: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut body = serde_json::json!({
        "access_token": "access-token",
        "refresh_token": "refresh-token",
        "expires_in": 21_600,
        "token_type": "Bearer",
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
type NotifyHook = Arc<dyn Fn(&AuthEvent) + Send + Sync>;

/// Routes URLs to scripted JSON responses, capturing request bodies.
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

fn flow(fetch: Arc<ScriptedFetch>) -> XaiOAuth {
    XaiOAuth::new(fetch as FetchFunction, Arc::new(|| FROZEN_NOW_MS))
}

fn form_of(requests: &[(String, String)], index: usize) -> serde_json::Value {
    let form = url::form_urlencoded::parse(requests[index].1.as_bytes());
    let mut map = serde_json::Map::new();
    for (key, value) in form {
        map.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    serde_json::Value::Object(map)
}

#[derive(Clone, Default)]
struct RecordingInteraction {
    signal: Option<CancellationToken>,
    device_codes: Arc<Mutex<Vec<AuthEvent>>>,
    on_notify: Option<NotifyHook>,
}

impl AuthInteraction for RecordingInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        self.signal.clone()
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
        unreachable!("the xAI flow never prompts")
    }

    fn notify(&self, event: AuthEvent) {
        if let Some(on_notify) = &self.on_notify {
            on_notify(&event);
        }
        self.device_codes.lock().unwrap().push(event);
    }
}

fn interaction(signal: Option<CancellationToken>) -> RecordingInteraction {
    RecordingInteraction {
        signal,
        device_codes: Arc::new(Mutex::new(Vec::new())),
        on_notify: None,
    }
}

async fn refresh(flow: &XaiOAuth, token: &str) -> Result<OAuthCredential, AuthStorageError> {
    let credential = OAuthCredential {
        access: "old-access".to_string(),
        refresh: token.to_string(),
        expires: 0,
        extra: Default::default(),
    };
    flow.refresh(&credential, CancellationToken::new()).await
}

async fn login(
    flow: &XaiOAuth,
    interaction: &RecordingInteraction,
) -> Result<OAuthCredential, AuthStorageError> {
    flow.login(Arc::new(interaction.clone())).await
}

#[tokio::test(start_paused = true)]
async fn uses_the_device_grant_delays_polling_and_handles_pending_and_slow_down() {
    let token_replies = Arc::new(Mutex::new(vec![
        (
            400u16,
            serde_json::json!({"error": "authorization_pending"}),
        ),
        (
            400u16,
            serde_json::json!({"error": "slow_down", "interval": 10}),
        ),
        (200u16, token_response(&[])),
    ]));
    let token_replies_for_handler = Arc::clone(&token_replies);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url == "https://auth.x.ai/oauth2/device/code" {
                (200, device_code_response(&[]))
            } else if url == "https://auth.x.ai/oauth2/token" {
                token_replies_for_handler.lock().unwrap().remove(0)
            } else {
                panic!("unexpected request: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let flow = flow(Arc::clone(&fetch));
    let interaction = interaction(None);

    let credentials = login(&flow, &interaction).await.unwrap();

    // Device authorization form.
    let requests = fetch.requests.lock().unwrap();
    let device_form = form_of(&requests, 0);
    assert_eq!(requests[0].0, "https://auth.x.ai/oauth2/device/code");
    assert_eq!(
        device_form["client_id"],
        "b1a00492-073a-47ea-816f-4c329264a828"
    );
    assert_eq!(
        device_form["scope"],
        "openid profile email offline_access grok-cli:access api:access"
    );
    assert_eq!(device_form["referrer"], "pi");

    // Token polls use the device grant and the same client id.
    for index in 1..requests.len() {
        assert_eq!(requests[index].0, "https://auth.x.ai/oauth2/token");
        let form = form_of(&requests, index);
        assert_eq!(
            form["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(form["client_id"], "b1a00492-073a-47ea-816f-4c329264a828");
        assert_eq!(form["device_code"], "device-code");
    }
    // Pending, slow_down (10s interval), complete: three polls total.
    assert_eq!(requests.len(), 4);

    let events = interaction.device_codes.lock().unwrap();
    assert_eq!(
        events.as_slice(),
        [AuthEvent::DeviceCode {
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://accounts.x.ai/oauth2/device".to_string(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(900),
        }]
    );

    assert_eq!(
        credentials,
        OAuthCredential {
            access: "access-token".to_string(),
            refresh: "refresh-token".to_string(),
            expires: FROZEN_NOW_MS + 21_600_000 - 300_000,
            extra: Default::default(),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn falls_back_to_the_default_poll_interval_when_interval_is_zero() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.x.ai/oauth2/device/code" {
                (
                    200,
                    device_code_response(&[("interval", serde_json::json!(0))]),
                )
            } else {
                (200, token_response(&[]))
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let flow = flow(Arc::clone(&fetch));

    let credentials = login(&flow, &interaction(None)).await.unwrap();

    assert_eq!(credentials.access, "access-token");
    // Two requests: device code + exactly one token poll.
    assert_eq!(fetch.requests.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn prefers_verification_uri_complete_when_provided() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.x.ai/oauth2/device/code" {
                (
                    200,
                    device_code_response(&[(
                        "verification_uri_complete",
                        serde_json::json!(
                            "https://accounts.x.ai/oauth2/device?user_code=ABCD-1234"
                        ),
                    )]),
                )
            } else {
                (200, token_response(&[]))
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let flow = flow(Arc::clone(&fetch));
    let interaction = interaction(None);

    login(&flow, &interaction).await.unwrap();

    let events = interaction.device_codes.lock().unwrap();
    let AuthEvent::DeviceCode {
        verification_uri, ..
    } = &events[0]
    else {
        panic!("expected device code event");
    };
    assert_eq!(
        verification_uri,
        "https://accounts.x.ai/oauth2/device?user_code=ABCD-1234"
    );
}

#[tokio::test]
async fn rejects_a_non_https_verification_uri_complete() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                device_code_response(&[(
                    "verification_uri_complete",
                    serde_json::json!("http://accounts.x.ai/oauth2/device?user_code=ABCD-1234"),
                )]),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = login(&flow(Arc::clone(&fetch)), &interaction(None))
        .await
        .unwrap_err();
    assert!(
        error.0.contains("Untrusted verification URI"),
        "{}",
        error.0
    );
}

#[tokio::test]
async fn rejects_non_https_verification_uris() {
    for uri in [
        "http://accounts.x.ai/oauth2/device",
        "file:///etc/passwd",
        "not a url",
    ] {
        let fetch = Arc::new(ScriptedFetch {
            handler: Arc::new(move |_| {
                (
                    200,
                    device_code_response(&[("verification_uri", serde_json::json!(uri))]),
                )
            }),
            requests: Mutex::new(Vec::new()),
        });

        let error = login(&flow(Arc::clone(&fetch)), &interaction(None))
            .await
            .unwrap_err();
        assert!(
            error.0.contains("Untrusted verification URI"),
            "{}",
            error.0
        );
    }
}

#[tokio::test(start_paused = true)]
async fn fails_when_device_authorization_is_denied() {
    for error_code in ["access_denied", "authorization_denied"] {
        let request_count = Arc::new(Mutex::new(0u32));
        let error_code = error_code.to_string();
        let fetch = Arc::new(ScriptedFetch {
            handler: Arc::new(move |url| {
                let mut count = request_count.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    (
                        200,
                        device_code_response(&[("interval", serde_json::json!(1))]),
                    )
                } else if url == "https://auth.x.ai/oauth2/token" {
                    (400, serde_json::json!({"error": error_code}))
                } else {
                    panic!("unexpected request")
                }
            }),
            requests: Mutex::new(Vec::new()),
        });

        let error = login(&flow(Arc::clone(&fetch)), &interaction(None))
            .await
            .unwrap_err();
        assert_eq!(error.0, "xAI device authorization was denied");
    }
}

#[tokio::test(start_paused = true)]
async fn cancels_while_waiting_for_the_first_token_poll() {
    let controller = CancellationToken::new();
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, device_code_response(&[]))),
        requests: Mutex::new(Vec::new()),
    });
    let flow = flow(Arc::clone(&fetch));
    let mut interaction = interaction(Some(controller.clone()));
    interaction.on_notify = Some(Arc::new({
        let controller = controller.clone();
        move |_| controller.cancel()
    }));

    let error = login(&flow, &interaction).await.unwrap_err();

    assert_eq!(error.0, "Login cancelled");
    assert_eq!(fetch.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refreshes_tokens_and_preserves_an_unrotated_refresh_token() {
    let request_count = Arc::new(Mutex::new(0u32));
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |_| {
            let mut count = request_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                (
                    200,
                    token_response(&[
                        ("access_token", serde_json::json!("new-access")),
                        ("refresh_token", serde_json::json!("new-refresh")),
                    ]),
                )
            } else {
                (
                    200,
                    token_response(&[
                        ("access_token", serde_json::json!("newer-access")),
                        ("refresh_token", serde_json::Value::Null),
                    ]),
                )
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let flow = flow(Arc::clone(&fetch));

    let rotated = refresh(&flow, "old-refresh").await.unwrap();
    let preserved = refresh(&flow, "keep-refresh").await.unwrap();

    {
        let requests = fetch.requests.lock().unwrap();
        for (index, expected_refresh) in [(0usize, "old-refresh"), (1, "keep-refresh")] {
            let form = form_of(&requests, index);
            assert_eq!(form["grant_type"], "refresh_token");
            assert_eq!(form["client_id"], "b1a00492-073a-47ea-816f-4c329264a828");
            assert_eq!(form["refresh_token"], expected_refresh);
        }
    }
    assert_eq!(rotated.access, "new-access");
    assert_eq!(rotated.refresh, "new-refresh");
    assert_eq!(preserved.access, "newer-access");
    assert_eq!(preserved.refresh, "keep-refresh");

    assert_eq!(flow.name(), "xAI (Grok/X subscription)");
    let auth = flow.to_auth(&preserved).await.unwrap();
    assert_eq!(auth.api_key.as_deref(), Some("newer-access"));
}

#[tokio::test]
async fn assumes_a_one_hour_lifetime_when_expires_in_is_missing() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                token_response(&[("expires_in", serde_json::Value::Null)]),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });

    let credentials = refresh(&flow(Arc::clone(&fetch)), "old-refresh")
        .await
        .unwrap();

    assert_eq!(credentials.expires, FROZEN_NOW_MS + 3_600_000 - 300_000);
}

#[tokio::test]
async fn rejects_token_responses_with_missing_fields() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                token_response(&[("access_token", serde_json::Value::Null)]),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = refresh(&flow(Arc::clone(&fetch)), "old-refresh")
        .await
        .unwrap_err();
    assert_eq!(error.0, "Invalid xAI OAuth response field: access_token");
}

#[tokio::test]
async fn surfaces_the_upstream_error_code_and_description_on_refresh_failure() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                400,
                serde_json::json!({"error": "invalid_grant", "error_description": "refresh token revoked"}),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = refresh(&flow(Arc::clone(&fetch)), "old-refresh")
        .await
        .unwrap_err();
    assert_eq!(
        error.0,
        "xAI OAuth token refresh failed (HTTP 400): invalid_grant: refresh token revoked"
    );
}
