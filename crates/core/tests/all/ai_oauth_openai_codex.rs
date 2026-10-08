//! Port of `pi-core/ai/test/openai-codex-oauth.test.ts`. The injected clock
//! derives from paused tokio time so token expiry arithmetic advances with
//! the driven schedule exactly like the TypeScript fake timers.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_core::ai::auth::oauth::openai_codex::OpenAICodexOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, OAuthAuth,
    OAuthCredential,
};
use pi_core::ai::types::{FetchFunction, ProviderEnv};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

const BASE_NOW_MS: i64 = 1_778_476_800_000; // 2026-05-20T00:00:00Z

type RouteHandler = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

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
        let (status, body) = (self.handler)(&request.url);
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn json_body(body: serde_json::Value) -> String {
    serde_json::to_string(&body).unwrap()
}

fn create_access_token(account_id: &str) -> String {
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut buffer = [0u8; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let word = ((buffer[0] as u32) << 16) | ((buffer[1] as u32) << 8) | buffer[2] as u32;
            out.push(ALPHABET[(word >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(word >> 12) as usize & 0x3f] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(word >> 6) as usize & 0x3f] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[word as usize & 0x3f] as char
            } else {
                '='
            });
        }
        out
    }
    let header = base64(json_body(serde_json::json!({"alg": "none"})).as_bytes());
    let payload = base64(
        json_body(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
        }))
        .as_bytes(),
    );
    format!("{header}.{payload}.signature")
}

fn account_id_of(credential: &OAuthCredential) -> Option<&str> {
    credential
        .extra
        .get("accountId")
        .and_then(|value| value.as_str())
}

struct DeviceLoginInteraction {
    signal: Option<CancellationToken>,
    events: Arc<Mutex<Vec<AuthEvent>>>,
}

impl AuthInteraction for DeviceLoginInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        self.signal.clone()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
        assert!(matches!(prompt.kind, AuthPromptKind::Select { .. }));
        Box::pin(std::future::ready(Ok("device_code".to_string())))
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[tokio::test(start_paused = true)]
async fn logs_in_with_the_openai_codex_device_code_flow() {
    let start = tokio::time::Instant::now();
    let access_token = create_access_token("account-123");
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let poll_responses = Arc::new(Mutex::new(vec![
        (
            403u16,
            json_body(serde_json::json!({
                "error": {
                    "message": "Device authorization is pending. Please try again.",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "deviceauth_authorization_pending",
                }
            })),
        ),
        (
            200,
            json_body(serde_json::json!({
                "authorization_code": "oauth-code",
                "code_challenge": "device-code-challenge",
                "code_verifier": "device-code-verifier",
            })),
        ),
    ]));
    let poll_times_for_handler = Arc::clone(&poll_times);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
                (
                    200,
                    json_body(serde_json::json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "ABCD-1234",
                        "interval": "5",
                    })),
                )
            } else if url == "https://auth.openai.com/api/accounts/deviceauth/token" {
                poll_times_for_handler.lock().unwrap().push(start.elapsed());
                poll_responses.lock().unwrap().remove(0)
            } else if url == "https://auth.openai.com/oauth/token" {
                (
                    200,
                    json_body(serde_json::json!({
                        "access_token": access_token,
                        "refresh_token": "refresh-token",
                        "expires_in": 3600,
                    })),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let now_ms: pi_core::ai::auth::oauth::NowMs =
        Arc::new(move || BASE_NOW_MS + start.elapsed().as_millis() as i64);
    let flow = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        now_ms,
        ProviderEnv::new(),
    );

    let credential = flow
        .login(Arc::new(DeviceLoginInteraction {
            signal: None,
            events: Arc::clone(&events),
        }))
        .await
        .unwrap();

    assert_eq!(credential.access, create_access_token("account-123"));
    assert_eq!(credential.refresh, "refresh-token");
    assert_eq!(account_id_of(&credential), Some("account-123"));
    // The token exchange ran at the +5s poll.
    assert_eq!(credential.expires, BASE_NOW_MS + 5_000 + 3600 * 1000);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode {
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://auth.openai.com/codex/device".to_string(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(900),
        }]
    );
    // The first poll is immediate; the second after the 5s interval.
    assert_eq!(
        poll_times.lock().unwrap().as_slice(),
        [Duration::from_secs(0), Duration::from_secs(5)]
    );

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(
        requests[0],
        (
            "https://auth.openai.com/api/accounts/deviceauth/usercode".to_string(),
            r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#.to_string()
        )
    );
    assert_eq!(
        requests[1].1,
        r#"{"device_auth_id":"device-auth-id","user_code":"ABCD-1234"}"#
    );
}

#[tokio::test(start_paused = true)]
async fn offers_browser_login_first_and_uses_the_selected_device_code_flow() {
    let access_token = create_access_token("account-456");
    struct SelectInteraction {
        select_prompts: Arc<Mutex<Vec<AuthPromptKind>>>,
        events: Arc<Mutex<Vec<AuthEvent>>>,
    }
    impl AuthInteraction for SelectInteraction {
        fn signal(&self) -> Option<CancellationToken> {
            None
        }
        fn prompt(
            &self,
            prompt: AuthPrompt,
        ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
            self.select_prompts.lock().unwrap().push(prompt.kind);
            Box::pin(std::future::ready(Ok("device_code".to_string())))
        }
        fn notify(&self, event: AuthEvent) {
            if matches!(event, AuthEvent::AuthUrl { .. }) {
                panic!("Browser login should not start");
            }
            self.events.lock().unwrap().push(event);
        }
    }

    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
                (
                    200,
                    json_body(serde_json::json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "WXYZ-7890",
                        "interval": "5",
                    })),
                )
            } else if url == "https://auth.openai.com/api/accounts/deviceauth/token" {
                (
                    200,
                    json_body(serde_json::json!({
                        "authorization_code": "oauth-code",
                        "code_challenge": "device-code-challenge",
                        "code_verifier": "device-code-verifier",
                    })),
                )
            } else if url == "https://auth.openai.com/oauth/token" {
                (
                    200,
                    json_body(serde_json::json!({
                        "access_token": access_token,
                        "refresh_token": "refresh-token",
                        "expires_in": 3600,
                    })),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let select_prompts = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));

    let credential = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    )
    .login(Arc::new(SelectInteraction {
        select_prompts: Arc::clone(&select_prompts),
        events: Arc::clone(&events),
    }))
    .await
    .unwrap();

    assert_eq!(credential.access, create_access_token("account-456"));
    assert_eq!(credential.refresh, "refresh-token");
    assert_eq!(account_id_of(&credential), Some("account-456"));

    let prompts = select_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    match &prompts[0] {
        AuthPromptKind::Select { message, options } => {
            assert_eq!(message, "Select OpenAI Codex login method:");
            assert_eq!(options.len(), 2);
            assert_eq!(options[0].id, "browser");
            assert_eq!(options[0].label, "Browser login (default)");
            assert_eq!(options[1].id, "device_code");
            assert_eq!(options[1].label, "Device code login (headless)");
        }
        other => panic!("expected select prompt, got {other:?}"),
    }
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode {
            user_code: "WXYZ-7890".to_string(),
            verification_uri: "https://auth.openai.com/codex/device".to_string(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(900),
        }]
    );
}

#[tokio::test]
async fn cancels_when_login_method_selection_is_cancelled() {
    struct CancelledInteraction;
    impl AuthInteraction for CancelledInteraction {
        fn signal(&self) -> Option<CancellationToken> {
            None
        }
        fn prompt(
            &self,
            _prompt: AuthPrompt,
        ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
            Box::pin(std::future::ready(Err(AuthStorageError(
                "Login cancelled".to_string(),
            ))))
        }
        fn notify(&self, _event: AuthEvent) {}
    }

    let error = OpenAICodexOAuth::new(
        Arc::new(ScriptedFetch {
            handler: Arc::new(|_| (200, String::new())),
            requests: Mutex::new(Vec::new()),
        }) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    )
    .login(Arc::new(CancelledInteraction))
    .await
    .unwrap_err();
    assert_eq!(error.0, "Login cancelled");
}

#[tokio::test(start_paused = true)]
async fn cancels_the_device_code_flow_while_waiting() {
    let controller = CancellationToken::new();
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let poll_times_for_handler = Arc::clone(&poll_times);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
                (
                    200,
                    json_body(serde_json::json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "ABCD-1234",
                        "interval": "5",
                    })),
                )
            } else if url == "https://auth.openai.com/api/accounts/deviceauth/token" {
                poll_times_for_handler.lock().unwrap().push(());
                (
                    403,
                    json_body(serde_json::json!({
                        "error": {
                            "message": "Device authorization is pending. Please try again.",
                            "type": "invalid_request_error",
                            "param": null,
                            "code": "deviceauth_authorization_pending",
                        }
                    })),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let flow = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    );
    let login = tokio::spawn(flow.login(Arc::new(DeviceLoginInteraction {
        signal: Some(controller.clone()),
        events: Arc::new(Mutex::new(Vec::new())),
    })));

    // The first poll is immediate; cancel while the flow waits.
    while poll_times.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    controller.cancel();

    let error = login.await.unwrap().unwrap_err();
    assert_eq!(error.0, "Login cancelled");
}

#[tokio::test(start_paused = true)]
async fn times_out_the_device_code_flow_after_fifteen_minutes() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
                (
                    200,
                    json_body(serde_json::json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "ABCD-1234",
                        "interval": "60",
                    })),
                )
            } else if url == "https://auth.openai.com/api/accounts/deviceauth/token" {
                (
                    403,
                    json_body(serde_json::json!({
                        "error": {
                            "message": "Device authorization is pending. Please try again.",
                            "type": "invalid_request_error",
                            "param": null,
                            "code": "deviceauth_authorization_pending",
                        }
                    })),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    )
    .login(Arc::new(DeviceLoginInteraction {
        signal: None,
        events: Arc::new(Mutex::new(Vec::new())),
    }))
    .await
    .unwrap_err();

    assert_eq!(error.0, "Device flow timed out");
}

#[tokio::test(start_paused = true)]
async fn treats_device_auth_403_and_404_responses_as_pending() {
    let start = tokio::time::Instant::now();
    let access_token = create_access_token("account-403-404");
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let poll_responses = Arc::new(Mutex::new(vec![
        (
            403u16,
            json_body(serde_json::json!({"error": "access_denied", "error_description": "denied"})),
        ),
        (404, "not ready".to_string()),
        (
            200,
            json_body(serde_json::json!({
                "authorization_code": "oauth-code",
                "code_challenge": "device-code-challenge",
                "code_verifier": "device-code-verifier",
            })),
        ),
    ]));
    let poll_times_for_handler = Arc::clone(&poll_times);
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(move |url| {
            if url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
                (
                    200,
                    json_body(serde_json::json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "ABCD-1234",
                        "interval": "1",
                    })),
                )
            } else if url == "https://auth.openai.com/api/accounts/deviceauth/token" {
                poll_times_for_handler.lock().unwrap().push(start.elapsed());
                poll_responses.lock().unwrap().remove(0)
            } else if url == "https://auth.openai.com/oauth/token" {
                (
                    200,
                    json_body(serde_json::json!({
                        "access_token": access_token,
                        "refresh_token": "refresh-token",
                        "expires_in": 3600,
                    })),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let credential = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    )
    .login(Arc::new(DeviceLoginInteraction {
        signal: None,
        events: Arc::new(Mutex::new(Vec::new())),
    }))
    .await
    .unwrap();

    assert_eq!(account_id_of(&credential), Some("account-403-404"));
    assert_eq!(credential.refresh, "refresh-token");
    assert_eq!(poll_times.lock().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn includes_the_response_body_in_device_auth_poll_failures() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
                (
                    200,
                    json_body(serde_json::json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "ABCD-1234",
                        "interval": "5",
                    })),
                )
            } else if url == "https://auth.openai.com/api/accounts/deviceauth/token" {
                (
                    500,
                    json_body(
                        serde_json::json!({"error": "server_error", "error_description": "try again later"}),
                    ),
                )
            } else {
                panic!("unexpected fetch URL: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });

    let error = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    )
    .login(Arc::new(DeviceLoginInteraction {
        signal: None,
        events: Arc::new(Mutex::new(Vec::new())),
    }))
    .await
    .unwrap_err();

    assert_eq!(
        error.0,
        "OpenAI Codex device auth failed with status 500: {\"error\":\"server_error\",\"error_description\":\"try again later\"}"
    );
}

#[tokio::test]
async fn surfaces_token_refresh_failures_without_stderr_noise() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                401,
                json_body(serde_json::json!({
                    "error": {
                        "message": "Could not validate your token. Please try signing in again.",
                        "type": "invalid_request_error",
                    }
                })),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let credential = OAuthCredential {
        access: "invalid-access-token".to_string(),
        refresh: "invalid-refresh-token".to_string(),
        expires: 0,
        extra: Default::default(),
    };

    let error = OpenAICodexOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(|| BASE_NOW_MS),
        ProviderEnv::new(),
    )
    .refresh(&credential, CancellationToken::new())
    .await
    .unwrap_err();

    assert!(
        error
            .0
            .starts_with("OpenAI Codex token refresh failed (401)"),
        "{}",
        error.0
    );
    assert!(
        error
            .0
            .contains("Could not validate your token. Please try signing in again."),
        "{}",
        error.0
    );
}
