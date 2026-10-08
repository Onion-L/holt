//! Port of `pi-core/ai/test/anthropic-oauth.test.ts`. The callback server
//! binds a fixed port (53692), so logins are serialized behind a lock like
//! the TypeScript `describe.sequential`.

use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::anthropic::AnthropicOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, OAuthAuth,
    OAuthCredential,
};
use pi_core::ai::types::{FetchFunction, ProviderEnv};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

const FROZEN_NOW_MS: i64 = 1_784_524_800_000;

/// Serializes tests that bind the fixed callback port (async-aware so the
/// guard can be held across the login await).
static CALLBACK_PORT_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

type FetchHandler = Arc<dyn Fn(&str) -> (u16, serde_json::Value) + Send + Sync>;

struct ScriptedFetch {
    handler: FetchHandler,
    requests: Mutex<Vec<(String, serde_json::Value)>>,
}

impl HttpFetch for ScriptedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let body = match &request.body {
            HttpBody::Json(value) => value.clone(),
            other => panic!("expected JSON body, got {other:?}"),
        };
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), body));
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

fn flow(fetch: Arc<ScriptedFetch>) -> AnthropicOAuth {
    AnthropicOAuth::new(
        fetch as FetchFunction,
        Arc::new(|| FROZEN_NOW_MS),
        ProviderEnv::new(),
    )
}

fn request_body(fetch: &ScriptedFetch) -> serde_json::Value {
    fetch.requests.lock().unwrap()[0].1.clone()
}

struct ManualLoginInteraction {
    auth_url: Mutex<Option<String>>,
    manual_signals: Arc<Mutex<Vec<CancellationToken>>>,
    events: Arc<Mutex<Vec<AuthEvent>>>,
    /// Builds the manual input from the captured authorize URL.
    manual_answer: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl AuthInteraction for ManualLoginInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        None
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<String, AuthStorageError>> {
        assert!(matches!(prompt.kind, AuthPromptKind::ManualCode { .. }));
        if let Some(signal) = &prompt.signal {
            self.manual_signals.lock().unwrap().push(signal.clone());
        }
        let auth_url = self.auth_url.lock().unwrap().clone();
        let answer = Arc::clone(&self.manual_answer);
        Box::pin(async move {
            Ok(answer(
                auth_url
                    .as_deref()
                    .expect("auth url captured before prompt"),
            ))
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = &event {
            *self.auth_url.lock().unwrap() = Some(url.clone());
        }
        self.events.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn keeps_the_localhost_redirect_uri_for_manual_callback_login() {
    let _guard = CALLBACK_PORT_LOCK.lock().await;
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                serde_json::json!({
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "expires_in": 3600,
                }),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let interaction = ManualLoginInteraction {
        auth_url: Mutex::new(None),
        manual_signals: Arc::new(Mutex::new(Vec::new())),
        events: Arc::new(Mutex::new(Vec::new())),
        manual_answer: Arc::new(|auth_url| {
            // Reply with the redirect URL carrying the code plus the state
            // from the authorize URL.
            let url = url::Url::parse(auth_url).unwrap();
            let param = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .unwrap()
                    .1
                    .to_string()
            };
            format!(
                "{}?code=manual-code&state={}",
                param("redirect_uri"),
                param("state")
            )
        }),
    };

    let interaction_object: Arc<dyn AuthInteraction> = Arc::new(interaction);
    let credentials = flow(Arc::clone(&fetch))
        .login(interaction_object)
        .await
        .unwrap();

    assert_eq!(credentials.access, "access-token");
    assert_eq!(credentials.refresh, "refresh-token");
    assert_eq!(
        credentials.expires,
        FROZEN_NOW_MS + 3600 * 1000 - 5 * 60 * 1000
    );
    let body = request_body(&fetch);
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "manual-code");
    assert_eq!(body["redirect_uri"], "http://localhost:53692/callback");
    assert_eq!(fetch.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn omits_scope_from_refresh_token_requests() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                serde_json::json!({
                    "access_token": "new-access-token",
                    "refresh_token": "new-refresh-token",
                    "expires_in": 3600,
                }),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let credential = OAuthCredential {
        access: "old-access-token".to_string(),
        refresh: "refresh-token".to_string(),
        expires: 0,
        extra: Default::default(),
    };

    let credentials = flow(Arc::clone(&fetch))
        .refresh(&credential, CancellationToken::new())
        .await
        .unwrap();

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests[0].0, "https://platform.claude.com/v1/oauth/token");
    let body = &requests[0].1;
    assert_eq!(body["grant_type"], "refresh_token");
    assert!(body["client_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(body["refresh_token"], "refresh-token");
    assert!(body.get("scope").is_none());
    assert_eq!(credentials.access, "new-access-token");
    assert_eq!(credentials.refresh, "new-refresh-token");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn login_resolves_through_the_manual_prompt_and_aborts_it_after_settling() {
    let _guard = CALLBACK_PORT_LOCK.lock().await;
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                serde_json::json!({"access_token": "access", "refresh_token": "refresh", "expires_in": 3600}),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let manual_signals = Arc::new(Mutex::new(Vec::new()));
    let interaction = ManualLoginInteraction {
        auth_url: Mutex::new(None),
        manual_signals: Arc::clone(&manual_signals),
        events: Arc::clone(&events),
        manual_answer: Arc::new(|_| "the-code".to_string()),
    };

    let credential = flow(Arc::clone(&fetch))
        .login(Arc::new(interaction))
        .await
        .unwrap();

    assert_eq!(credential.access, "access");
    assert_eq!(credential.refresh, "refresh");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AuthEvent::AuthUrl { .. }))
    );
    // The prompt's signal is aborted once login settles, so UIs can dismiss
    // it.
    let manual_signals = manual_signals.lock().unwrap();
    assert_eq!(manual_signals.len(), 1);
    assert!(manual_signals[0].is_cancelled());
}
