//! Port of `pi-core/ai/test/radius-oauth.test.ts`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::radius::RadiusOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError, OAuthAuth,
    OAuthCredential,
};
use pi_core::ai::types::FetchFunction;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

const GATEWAY: &str = "https://radius.example";
const FROZEN_NOW_MS: i64 = 1_785_139_200_000; // 2026-07-24T00:00:00Z

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
            HttpBody::Empty => String::new(),
            other => panic!("unexpected body: {other:?}"),
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

fn flow(fetch: Arc<ScriptedFetch>) -> RadiusOAuth {
    RadiusOAuth::new(
        "Radius",
        GATEWAY,
        fetch as FetchFunction,
        Arc::new(|| FROZEN_NOW_MS),
    )
}

struct SelectInteraction {
    login_method: &'static str,
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
        assert!(matches!(prompt.kind, AuthPromptKind::Select { .. }));
        Box::pin(std::future::ready(Ok(self.login_method.to_string())))
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

#[tokio::test(start_paused = true)]
async fn uses_gateway_endpoints_directly_for_device_login() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url == "https://radius.example/v1/oauth/device" {
                (
                    200,
                    serde_json::json!({
                        "device_code": "device-code",
                        "user_code": "ABCD-1234",
                        "verification_uri": "https://radius-ui.example/pair",
                        "expires_in": 600,
                        "interval": 5,
                    }),
                )
            } else if url == "https://radius.example/v1/oauth/token" {
                (
                    200,
                    serde_json::json!({
                        "access_token": "access-token",
                        "refresh_token": "refresh-token",
                        "expires_in": 3600,
                        "scope": "gateway offline_access",
                    }),
                )
            } else {
                panic!("unexpected request: {url}")
            }
        }),
        requests: Mutex::new(Vec::new()),
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let interaction = SelectInteraction {
        login_method: "device-code",
        events: Arc::clone(&events),
    };

    let credential = flow(Arc::clone(&fetch))
        .login(Arc::new(interaction))
        .await
        .unwrap();

    let requests = fetch.requests.lock().unwrap();
    let device_form = form_of(&requests, 0);
    assert_eq!(requests[0].0, "https://radius.example/v1/oauth/device");
    assert_eq!(device_form["client_id"], "pi-gateway");
    assert_eq!(device_form["scope"], "gateway offline_access");
    let token_form = form_of(&requests, 1);
    assert_eq!(requests[1].0, "https://radius.example/v1/oauth/token");
    assert_eq!(
        token_form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(token_form["client_id"], "pi-gateway");
    assert_eq!(token_form["device_code"], "device-code");
    drop(requests);

    assert_eq!(
        credential,
        OAuthCredential {
            access: "access-token".to_string(),
            refresh: "refresh-token".to_string(),
            expires: FROZEN_NOW_MS + 3600 * 1000 - 60_000,
            extra: [(
                "scope".to_string(),
                serde_json::json!("gateway offline_access")
            )]
            .into_iter()
            .collect(),
        }
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode {
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://radius-ui.example/pair".to_string(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(600),
        }]
    );
}

#[tokio::test]
async fn refreshes_directly_through_the_gateway_without_discovery() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": "new-refresh",
                    "expires_in": 3600,
                }),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let credential = OAuthCredential {
        access: "old-access".to_string(),
        refresh: "old-refresh".to_string(),
        expires: 0,
        extra: Default::default(),
    };

    let refreshed = flow(Arc::clone(&fetch))
        .refresh(&credential, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(refreshed.access, "new-access");
    assert_eq!(refreshed.refresh, "new-refresh");
    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests[0].0, "https://radius.example/v1/oauth/token");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn discovers_only_the_interactive_browser_authorization_endpoint() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|_| {
            (
                200,
                serde_json::json!({"issuer": "https://radius-ui.example"}),
            )
        }),
        requests: Mutex::new(Vec::new()),
    });
    let interaction = SelectInteraction {
        login_method: "browser",
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let error = flow(Arc::clone(&fetch))
        .login(Arc::new(interaction))
        .await
        .unwrap_err();

    assert_eq!(
        error.0,
        "Invalid Radius OAuth config from https://radius.example"
    );
    assert_eq!(fetch.requests.lock().unwrap().len(), 1);
}
