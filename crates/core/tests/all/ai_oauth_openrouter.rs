//! Port of `pi-core/ai/test/openrouter-oauth.test.ts` (the two
//! provider-integration cases land with the provider factories).
//!
//! The TypeScript suite stubs global fetch but exercises the callback server
//! over real loopback HTTP; here a scripted `HttpFetch` transport handles the
//! token exchange and a raw TCP client drives the callback endpoints.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::openrouter::OpenRouterOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthStorageError, OAuthAuth, OAuthCredential,
};
use pi_core::ai::types::{FetchFunction, ProviderEnv};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

type FetchHandler = Arc<dyn Fn(&str) -> (u16, serde_json::Value) + Send + Sync>;
type AuthUrlHook = Arc<dyn Fn(&str) + Send + Sync>;

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
            HttpBody::Json(value) => value.to_string(),
            other => panic!("expected JSON body, got {other:?}"),
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

fn flow(fetch: Arc<ScriptedFetch>, env: ProviderEnv) -> OpenRouterOAuth {
    OpenRouterOAuth::new(fetch as FetchFunction, env)
}

fn env_with(entries: &[(&str, &str)]) -> ProviderEnv {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<BTreeMap<_, _>>()
}

#[derive(Clone, Default)]
struct LoginInteraction {
    signal: Option<CancellationToken>,
    /// Never-resolving manual prompt by default.
    prompt_result: Option<Result<String, String>>,
    manual_signals: Arc<Mutex<Vec<CancellationToken>>>,
    events: Arc<Mutex<Vec<AuthEvent>>>,
    /// Runs on each `auth_url` event; receives the callback URL.
    on_auth_url: Option<AuthUrlHook>,
}

impl AuthInteraction for LoginInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        self.signal.clone()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
        if let Some(signal) = &prompt.signal {
            self.manual_signals.lock().unwrap().push(signal.clone());
        }
        match self.prompt_result.clone() {
            Some(Ok(input)) => Box::pin(std::future::ready(Ok(input))),
            Some(Err(message)) => Box::pin(std::future::ready(Err(AuthStorageError(message)))),
            // Pending forever: the manual prompt races the browser callback.
            None => Box::pin(std::future::pending()),
        }
    }

    fn notify(&self, event: AuthEvent) {
        if let (AuthEvent::AuthUrl { url, .. }, Some(on_auth_url)) = (&event, &self.on_auth_url) {
            let callback_url = url::Url::parse(url)
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "callback_url")
                .unwrap()
                .1
                .to_string();
            on_auth_url(&callback_url);
        }
        self.events.lock().unwrap().push(event);
    }
}

/// Appends `?code=<code>` to the callback URL like the TypeScript test's
/// `callbackUrl.searchParams.set("code", ...)`.
fn with_code(callback_url: &str, code: &str) -> String {
    let mut url = url::Url::parse(callback_url).expect("callback url parses");
    url.query_pairs_mut().append_pair("code", code);
    url.to_string()
}

/// Minimal HTTP/1.1 GET over TCP against the loopback callback server;
/// returns the response status.
async fn http_get_status(url: &str) -> Result<u16, String> {
    let parsed = url::Url::parse(url).map_err(|error| error.to_string())?;
    let host = parsed.host_str().ok_or("missing host")?.to_string();
    let port = parsed.port_or_known_default().ok_or("missing port")?;
    let mut path = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|error| error.to_string())?;
    let request = format!("GET {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut buffer = Vec::new();
    stream
        .read_to_end(&mut buffer)
        .await
        .map_err(|error| error.to_string())?;
    let head = String::from_utf8_lossy(&buffer);
    head.lines()
        .next()
        .and_then(|status_line| status_line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed response: {head}"))
}

fn exchange_body(fetch: &ScriptedFetch) -> serde_json::Value {
    let requests = fetch.requests.lock().unwrap();
    serde_json::from_str(&requests[0].1).unwrap()
}

fn authorize_url(interaction: &LoginInteraction) -> url::Url {
    let events = interaction.events.lock().unwrap();
    events
        .iter()
        .find_map(|event| match event {
            AuthEvent::AuthUrl { url, .. } => Some(url::Url::parse(url).unwrap()),
            _ => None,
        })
        .expect("auth_url event")
}

#[tokio::test]
async fn runs_pkce_on_a_one_shot_loopback_callback_and_exchanges_the_code() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, serde_json::json!({"key": "sk-or-test"}))),
        requests: Mutex::new(Vec::new()),
    });
    let callback_task = Arc::new(Mutex::new(None));
    let interaction = LoginInteraction {
        signal: None,
        prompt_result: None,
        manual_signals: Arc::new(Mutex::new(Vec::new())),
        events: Arc::new(Mutex::new(Vec::new())),
        on_auth_url: Some(Arc::new({
            let callback_task = Arc::clone(&callback_task);
            move |callback_url| {
                let url = with_code(callback_url, "authorization-code");
                let task = tokio::spawn(async move { http_get_status(&url).await });
                *callback_task.lock().unwrap() = Some(task);
            }
        })),
    };

    let credential = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction.clone()))
        .await
        .unwrap();

    assert_eq!(
        credential,
        OAuthCredential {
            access: "sk-or-test".to_string(),
            refresh: String::new(),
            expires: MAX_SAFE_INTEGER,
            extra: Default::default(),
        }
    );
    let callback_task = callback_task.lock().unwrap().take().unwrap();
    let callback_status = callback_task.await.unwrap();
    assert_eq!(callback_status, Ok(200));
    // The manual prompt was aborted after the callback claimed the login.
    let manual_signals = interaction.manual_signals.lock().unwrap();
    assert_eq!(manual_signals.len(), 1);
    assert!(manual_signals[0].is_cancelled());

    let authorize = authorize_url(&interaction);
    assert_eq!(
        authorize.origin().ascii_serialization(),
        "https://openrouter.ai"
    );
    assert_eq!(authorize.path(), "/auth");
    assert_eq!(
        authorize
            .query_pairs()
            .find(|(key, _)| key == "code_challenge_method")
            .map(|(_, value)| value.to_string()),
        Some("S256".to_string())
    );
    let callback_url = url::Url::parse(
        &authorize
            .query_pairs()
            .find(|(key, _)| key == "callback_url")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(callback_url.host_str(), Some("127.0.0.1"));
    assert!(
        callback_url.path().starts_with("/oauth/callback/"),
        "{}",
        callback_url.path()
    );
    assert!(
        callback_url.path()["/oauth/callback/".len()..]
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-')
    );

    let body = exchange_body(&fetch);
    assert_eq!(body["code"], "authorization-code");
    assert_eq!(body["code_challenge_method"], "S256");
    let verifier = body["code_verifier"].as_str().unwrap();
    let digest = {
        use sha2::Digest as _;
        sha2::Sha256::digest(verifier.as_bytes())
    };
    let challenge = base64url(&digest);
    assert_eq!(
        authorize
            .query_pairs()
            .find(|(key, _)| key == "code_challenge")
            .map(|(_, value)| value.to_string()),
        Some(challenge)
    );
    assert_eq!(fetch.requests.lock().unwrap().len(), 1);
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let word = ((buffer[0] as u32) << 16) | ((buffer[1] as u32) << 8) | buffer[2] as u32;
        out.push(ALPHABET[(word >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(word >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(word >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[word as usize & 0x3f] as char);
        }
    }
    out
}

#[tokio::test]
async fn reports_token_exchange_failures_through_the_callback_page_and_login() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                403,
                serde_json::json!({"error": {"message": "invalid code"}}),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let callback_task = Arc::new(Mutex::new(None));
    let interaction = LoginInteraction {
        on_auth_url: Some(Arc::new({
            let callback_task = Arc::clone(&callback_task);
            move |callback_url| {
                let url = with_code(callback_url, "bad-code");
                let task = tokio::spawn(async move { http_get_status(&url).await });
                *callback_task.lock().unwrap() = Some(task);
            }
        })),
        ..Default::default()
    };

    let error = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction))
        .await
        .unwrap_err();

    assert_eq!(
        error.0,
        "OpenRouter OAuth key exchange failed (HTTP 403): invalid code"
    );
    let callback_task = callback_task.lock().unwrap().take().unwrap();
    let callback_status = callback_task.await.unwrap();
    assert_eq!(callback_status, Ok(502));
}

#[tokio::test]
async fn rejects_a_successful_response_without_a_key() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, serde_json::json!({"user_id": "user-1"}))),
        requests: Mutex::new(Vec::new()),
    });
    let callback_task = Arc::new(Mutex::new(None));
    let interaction = LoginInteraction {
        on_auth_url: Some(Arc::new({
            let callback_task = Arc::clone(&callback_task);
            move |callback_url| {
                let url = with_code(callback_url, "code-without-key");
                let task = tokio::spawn(async move { http_get_status(&url).await });
                *callback_task.lock().unwrap() = Some(task);
            }
        })),
        ..Default::default()
    };

    let error = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction))
        .await
        .unwrap_err();

    assert_eq!(error.0, "OpenRouter OAuth response carries no \"key\"");
    let callback_task = callback_task.lock().unwrap().take().unwrap();
    let callback_status = callback_task.await.unwrap();
    assert_eq!(callback_status, Ok(502));
}

/// Transport whose single token exchange stays pending until released.
struct DeferredExchangeFetch {
    requests: Mutex<Vec<(String, String)>>,
    release: tokio::sync::Notify,
}

impl HttpFetch for DeferredExchangeFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let body_text = match &request.body {
            HttpBody::Json(value) => value.to_string(),
            other => panic!("expected JSON body, got {other:?}"),
        };
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), body_text));
        Box::pin(async move {
            self.release.notified().await;
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                    r#"{"key": "sk-or-test"}"#,
                ))])),
            })
        })
    }
}

#[tokio::test]
async fn allows_only_one_token_exchange_for_a_callback() {
    let fetch = Arc::new(DeferredExchangeFetch {
        requests: Mutex::new(Vec::new()),
        release: tokio::sync::Notify::new(),
    });
    let interaction = Arc::new(LoginInteraction {
        on_auth_url: Some(Arc::new({
            let fetch = Arc::clone(&fetch);
            move |callback_url| {
                let url = with_code(callback_url, "authorization-code");
                let fetch = Arc::clone(&fetch);
                tokio::spawn(async move {
                    // The first callback claims the exchange; its response is
                    // held pending until the exchange is released.
                    let first_url = url.clone();
                    let first = tokio::spawn(async move { http_get_status(&first_url).await });

                    // Wait for the claimed exchange to reach the transport.
                    while fetch.requests.lock().unwrap().is_empty() {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }

                    // A second callback is rejected without another exchange.
                    assert_eq!(
                        http_get_status(&url).await.unwrap(),
                        409,
                        "claimed callback must 409"
                    );
                    assert_eq!(fetch.requests.lock().unwrap().len(), 1);

                    // Completing the exchange settles the login and the
                    // first callback.
                    fetch.release.notify_waiters();
                    assert_eq!(first.await.unwrap().unwrap(), 200);
                });
            }
        })),
        ..Default::default()
    });

    let credential = OpenRouterOAuth::new(Arc::clone(&fetch) as FetchFunction, ProviderEnv::new())
        .login(interaction)
        .await
        .unwrap();

    assert_eq!(credential.access, "sk-or-test");
}

#[tokio::test]
async fn mints_a_key_from_a_pasted_redirect_url_when_the_loopback_callback_never_arrives() {
    struct ManualRedirectInteraction {
        callback_url: Arc<Mutex<Option<String>>>,
    }

    impl AuthInteraction for ManualRedirectInteraction {
        fn signal(&self) -> Option<CancellationToken> {
            None
        }

        fn prompt(
            &self,
            prompt: AuthPrompt,
        ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
            assert!(matches!(
                prompt.kind,
                pi_core::ai::auth::types::AuthPromptKind::ManualCode { .. }
            ));
            let callback_url = self
                .callback_url
                .lock()
                .unwrap()
                .clone()
                .expect("callback url captured before prompt");
            Box::pin(std::future::ready(Ok(format!(
                "{callback_url}?code=manual-code"
            ))))
        }

        fn notify(&self, event: AuthEvent) {
            if let AuthEvent::AuthUrl { url, .. } = event {
                let callback_url = url::Url::parse(&url)
                    .unwrap()
                    .query_pairs()
                    .find(|(key, _)| key == "callback_url")
                    .unwrap()
                    .1
                    .to_string();
                *self.callback_url.lock().unwrap() = Some(callback_url);
            }
        }
    }

    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, serde_json::json!({"key": "sk-or-manual"}))),
        requests: Mutex::new(Vec::new()),
    });

    let credential = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(ManualRedirectInteraction {
            callback_url: Arc::new(Mutex::new(None)),
        }))
        .await
        .unwrap();

    assert_eq!(
        credential,
        OAuthCredential {
            access: "sk-or-manual".to_string(),
            refresh: String::new(),
            expires: MAX_SAFE_INTEGER,
            extra: Default::default(),
        }
    );
    assert_eq!(exchange_body(&fetch)["code"], "manual-code");
    assert_eq!(exchange_body(&fetch)["code_challenge_method"], "S256");
    assert_eq!(fetch.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn accepts_a_bare_authorization_code_from_the_manual_prompt() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, serde_json::json!({"key": "sk-or-manual"}))),
        requests: Mutex::new(Vec::new()),
    });
    let interaction = LoginInteraction {
        prompt_result: Some(Ok("  manual-code  ".to_string())),
        ..Default::default()
    };

    let credential = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction))
        .await
        .unwrap();

    assert_eq!(credential.access, "sk-or-manual");
    assert_eq!(exchange_body(&fetch)["code"], "manual-code");
}

#[tokio::test]
async fn fails_login_when_the_manual_prompt_is_cancelled() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, serde_json::json!({"key": "sk-or-unexpected"}))),
        requests: Mutex::new(Vec::new()),
    });
    let interaction = LoginInteraction {
        prompt_result: Some(Err("Login cancelled".to_string())),
        ..Default::default()
    };

    let error = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction))
        .await
        .unwrap_err();
    assert_eq!(error.0, "Login cancelled");
    assert!(fetch.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejects_empty_manual_input_without_exchanging_a_code() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| (200, serde_json::json!({"key": "sk-or-unexpected"}))),
        requests: Mutex::new(Vec::new()),
    });
    let interaction = LoginInteraction {
        prompt_result: Some(Ok("   ".to_string())),
        ..Default::default()
    };

    let error = flow(Arc::clone(&fetch), ProviderEnv::new())
        .login(Arc::new(interaction))
        .await
        .unwrap_err();
    assert_eq!(error.0, "Missing authorization code");
    assert!(fetch.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn closes_the_pending_callback_when_login_is_cancelled() {
    let controller = CancellationToken::new();
    let callback_url_slot = Arc::new(Mutex::new(None::<String>));
    let interaction = LoginInteraction {
        signal: Some(controller.clone()),
        on_auth_url: Some(Arc::new({
            let controller = controller.clone();
            let callback_url_slot = Arc::clone(&callback_url_slot);
            move |callback_url| {
                *callback_url_slot.lock().unwrap() = Some(callback_url.to_string());
                controller.cancel();
            }
        })),
        ..Default::default()
    };

    let error = flow(
        Arc::new(ScriptedFetch {
            handler: Arc::new(|_| (200, serde_json::json!({"key": "unexpected"}))),
            requests: Mutex::new(Vec::new()),
        }),
        ProviderEnv::new(),
    )
    .login(Arc::new(interaction))
    .await
    .unwrap_err();
    assert_eq!(error.0, "Login cancelled");

    let callback_url = callback_url_slot.lock().unwrap().clone().unwrap();
    assert!(http_get_status(&callback_url).await.is_err());
}

#[tokio::test]
async fn rejects_before_opening_a_callback_server_when_login_is_already_cancelled() {
    let controller = CancellationToken::new();
    controller.cancel();

    struct NoEventsInteraction;
    impl AuthInteraction for NoEventsInteraction {
        fn signal(&self) -> Option<CancellationToken> {
            let controller = CancellationToken::new();
            controller.cancel();
            Some(controller)
        }
        fn prompt(
            &self,
            _prompt: AuthPrompt,
        ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
            Box::pin(std::future::ready(Ok(String::new())))
        }
        fn notify(&self, _event: AuthEvent) {
            panic!("Cancelled login must not emit events");
        }
    }

    let error = flow(
        Arc::new(ScriptedFetch {
            handler: Arc::new(|_| (200, serde_json::json!({"key": "unexpected"}))),
            requests: Mutex::new(Vec::new()),
        }),
        ProviderEnv::new(),
    )
    .login(Arc::new(NoEventsInteraction))
    .await
    .unwrap_err();
    assert_eq!(error.0, "Login cancelled");
}

#[tokio::test]
async fn uses_the_configured_oauth_callback_host() {
    let controller = CancellationToken::new();
    let callback_url_slot = Arc::new(Mutex::new(None::<String>));
    let interaction = LoginInteraction {
        signal: Some(controller.clone()),
        on_auth_url: Some(Arc::new({
            let controller = controller.clone();
            let callback_url_slot = Arc::clone(&callback_url_slot);
            move |callback_url| {
                *callback_url_slot.lock().unwrap() = Some(callback_url.to_string());
                controller.cancel();
            }
        })),
        ..Default::default()
    };

    let error = flow(
        Arc::new(ScriptedFetch {
            handler: Arc::new(|_| (200, serde_json::json!({"key": "unexpected"}))),
            requests: Mutex::new(Vec::new()),
        }),
        env_with(&[("PI_OAUTH_CALLBACK_HOST", "localhost")]),
    )
    .login(Arc::new(interaction))
    .await
    .unwrap_err();
    assert_eq!(error.0, "Login cancelled");
    let callback_url =
        url::Url::parse(&callback_url_slot.lock().unwrap().clone().unwrap()).unwrap();
    assert_eq!(callback_url.host_str(), Some("localhost"));
}

// ---------------------------------------------------------------------------
// Provider integration (the two cases that landed with the factories).

#[test]
fn is_exposed_by_both_openrouter_providers_alongside_api_key_auth() {
    let text = pi_core::ai::providers::builtin::openrouter_provider();
    let images = pi_core::ai::providers::builtin::openrouter_images_provider();
    let text_auth = text.auth();
    let images_auth = images.auth();
    for (id, auth) in [("openrouter", text_auth), ("openrouter", images_auth)] {
        assert!(auth.api_key.is_some(), "{id}");
        let oauth = auth.oauth.as_ref().expect("oauth auth");
        assert_eq!(oauth.login_label(), Some("Sign in with OpenRouter"));
    }
}

#[tokio::test]
async fn resolves_the_same_stored_oauth_key_for_text_and_image_providers() {
    use pi_core::ai::auth::credential_store::InMemoryCredentialStore;
    use pi_core::ai::auth::types::{AuthFuture, CredentialStore};
    use pi_core::ai::images_models::ImagesModels;
    use pi_core::ai::models::{CreateModelsOptions, Models};
    use pi_core::ai::providers::builtin::{openrouter_images_provider, openrouter_provider};

    let credentials = Arc::new(InMemoryCredentialStore::new());
    credentials
        .modify(
            "openrouter",
            Box::new(|_| {
                Box::pin(std::future::ready(Ok(Some(
                    pi_core::ai::auth::types::Credential::OAuth(OAuthCredential {
                        access: "sk-or-stored".to_string(),
                        refresh: String::new(),
                        expires: MAX_SAFE_INTEGER,
                        ..Default::default()
                    }),
                ))))
                    as AuthFuture<
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

    let text_models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    text_models.set_provider(openrouter_provider());
    let image_models = ImagesModels::new(CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    image_models.set_provider(openrouter_images_provider());

    let text_auth = text_models
        .get_auth("openrouter", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(text_auth.auth.api_key.as_deref(), Some("sk-or-stored"));
    let image_auth = image_models
        .get_auth("openrouter", None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(image_auth.auth.api_key.as_deref(), Some("sk-or-stored"));
}
