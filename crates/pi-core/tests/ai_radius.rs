//! Focused tests for the radius provider port (`providers/radius.ts` and
//! `loadRadiusGatewayConfig` from `providers/radius-config.ts`); the
//! upstream suite covers the oauth flow separately (see ai_oauth_radius.rs)
//! and has no offline tests for the provider itself.

use std::sync::{Arc, Mutex};

use pi_core::ai::auth::types::{ApiKeyCredential, Credential, OAuthCredential};
use pi_core::ai::models::{ModelsPublication, RefreshModelsContext};
use pi_core::ai::models_store::ModelsStoreEntry;
use pi_core::ai::providers::radius::{RadiusProviderOptions, radius_provider};
use pi_core::ai::providers::radius_config::load_radius_gateway_config;
use pi_core::ai::types::Model;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpMethod, HttpRequest, HttpResponse};

const GATEWAY: &str = "https://radius.example";

fn config_body() -> String {
    serde_json::json!({
        "baseUrl": "https://gw.example",
        "models": [{
            "id": "m1",
            "name": "M1",
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000,
            "maxTokens": 100,
        }],
    })
    .to_string()
}

/// The model `config_body()` describes.
fn remote_model() -> Model {
    Model {
        id: "m1".to_string(),
        name: "M1".to_string(),
        api: "pi-messages".to_string(),
        provider: "radius".to_string(),
        base_url: "https://gw.example".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        cost: pi_core::ai::types::ModelCost {
            rates: pi_core::ai::types::ModelCostRates {
                input: pi_core::ai::types::JsF64(1.0),
                output: pi_core::ai::types::JsF64(2.0),
                cache_read: pi_core::ai::types::JsF64(0.0),
                cache_write: pi_core::ai::types::JsF64(0.0),
            },
            tiers: None,
        },
        context_window: 1000,
        max_tokens: 100,
        ..Default::default()
    }
}

/// A stored-catalog entry model that is not necessarily the remote shape.
fn stored_model(id: &str, provider: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "pi-messages".to_string(),
        provider: provider.to_string(),
        base_url: "https://gw.example".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        context_window: 1000,
        max_tokens: 100,
        ..Default::default()
    }
}

struct CannedFetch {
    status: u16,
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for CannedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<
        'a,
        Result<HttpResponse, pi_core::ai::utils::http::HttpFetchError>,
    > {
        self.requests.lock().unwrap().push(request);
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

fn canned_fetch(status: u16, body: String) -> Arc<CannedFetch> {
    Arc::new(CannedFetch {
        status,
        body,
        requests: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn loads_gateway_config_with_accept_and_bearer_headers() {
    let fetch = canned_fetch(200, config_body());
    let config = load_radius_gateway_config(
        GATEWAY,
        Some("radius-key"),
        None,
        Some(fetch.clone() as Arc<dyn HttpFetch>),
    )
    .await
    .unwrap();

    assert_eq!(config.base_url, "https://gw.example");
    assert_eq!(config.models.len(), 1);
    assert_eq!(config.models[0].id, "m1");

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, HttpMethod::Get);
    assert_eq!(requests[0].url, "https://radius.example/v1/config");
    let header = |name: &str| {
        requests[0]
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("accept").as_deref(), Some("application/json"));
    assert_eq!(
        header("authorization").as_deref(),
        Some("Bearer radius-key")
    );
    assert!(matches!(requests[0].body, HttpBody::Empty));
}

#[tokio::test]
async fn surfaces_http_and_shape_errors_with_ts_messages() {
    let fetch = canned_fetch(500, "  upstream exploded  ".to_string());
    let error = load_radius_gateway_config(GATEWAY, None, None, Some(fetch as Arc<dyn HttpFetch>))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "Could not load Radius config from https://radius.example: 500: upstream exploded"
    );

    let fetch = canned_fetch(200, serde_json::json!({ "unexpected": true }).to_string());
    let error = load_radius_gateway_config(GATEWAY, None, None, Some(fetch as Arc<dyn HttpFetch>))
        .await
        .unwrap_err();
    assert_eq!(error, "Invalid Radius config from https://radius.example");
}

/// Records persisted catalogs and runs publication updates, like the real
/// orchestrator.
#[derive(Default)]
struct PublishRecorder {
    persisted: Mutex<Vec<Vec<Model>>>,
    result: bool,
}

fn refresh_context(
    recorder: Arc<PublishRecorder>,
    credential: Option<Credential>,
    stored: Option<ModelsStoreEntry>,
    allow_network: bool,
) -> RefreshModelsContext {
    RefreshModelsContext {
        credential,
        stored,
        publish: Arc::new(move |publication: ModelsPublication| {
            if let Some(Some(entry)) = publication.persist {
                recorder
                    .persisted
                    .lock()
                    .unwrap()
                    .push(entry.models.clone());
            }
            if let Some(update) = publication.update {
                update();
            }
            let result = recorder.result;
            Box::pin(async move { Ok(result) })
        }),
        allow_network,
        force: None,
        signal: tokio_util::sync::CancellationToken::new(),
    }
}

fn provider(fetch: Arc<CannedFetch>) -> Arc<dyn pi_core::ai::models::Provider> {
    radius_provider(RadiusProviderOptions {
        gateway: Some(GATEWAY.to_string()),
        fetch: Some(fetch as Arc<dyn HttpFetch>),
        ..Default::default()
    })
}

#[tokio::test]
async fn defaults_to_radius_id_and_empty_catalog() {
    let provider = radius_provider(RadiusProviderOptions::default());
    assert_eq!(provider.id(), "radius");
    assert_eq!(provider.name(), "Radius");
    assert!(provider.get_models().is_empty());
    assert!(provider.has_refresh_models());
}

#[tokio::test]
async fn restores_stored_models_without_network() {
    let fetch = canned_fetch(200, config_body());
    let provider = provider(fetch.clone());
    let recorder = PublishRecorder::default().into();
    let stored = ModelsStoreEntry {
        models: vec![
            stored_model("stored-1", "radius"),
            stored_model("other", "someone-else"),
        ],
        ..Default::default()
    };

    provider
        .refresh_models(refresh_context(
            recorder,
            None,
            Some(stored),
            false, // allow_network off: the transport must stay untouched
        ))
        .await
        .unwrap();

    assert_eq!(
        provider.get_models(),
        vec![stored_model("stored-1", "radius")]
    );
    assert!(fetch.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn imports_legacy_oauth_credential_catalog() {
    let fetch = canned_fetch(200, config_body());
    let provider = provider(fetch.clone());
    let mut credential = OAuthCredential::default();
    credential.extra.insert(
        "gatewayConfig".to_string(),
        serde_json::from_str(&config_body()).unwrap(),
    );
    let credential = Credential::OAuth(credential);
    let recorder: Arc<PublishRecorder> = PublishRecorder::default().into();

    provider
        .refresh_models(refresh_context(
            Arc::clone(&recorder),
            Some(credential),
            None,
            false,
        ))
        .await
        .unwrap();

    // The legacy import both persists and updates; no network happened.
    let persisted = recorder.persisted.lock().unwrap().clone();
    assert_eq!(persisted, vec![vec![remote_model()]]);
    assert_eq!(provider.get_models(), vec![remote_model()]);
    assert!(fetch.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refreshes_from_the_gateway_with_the_credential_key() {
    let fetch = canned_fetch(200, config_body());
    let provider = provider(fetch.clone());
    let recorder: Arc<PublishRecorder> = PublishRecorder::default().into();

    provider
        .refresh_models(refresh_context(
            Arc::clone(&recorder),
            Some(Credential::ApiKey(ApiKeyCredential {
                key: Some("radius-key".to_string()),
                env: None,
            })),
            None,
            true,
        ))
        .await
        .unwrap();

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://radius.example/v1/config");
    let bearer = requests[0]
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone());
    assert_eq!(bearer.as_deref(), Some("Bearer radius-key"));

    let persisted = recorder.persisted.lock().unwrap().clone();
    assert_eq!(persisted, vec![vec![remote_model()]]);
    assert_eq!(provider.get_models(), vec![remote_model()]);
}

#[tokio::test]
async fn oauth_credentials_refresh_with_the_access_token() {
    let fetch = canned_fetch(200, config_body());
    let provider = provider(fetch.clone());
    let credential = Credential::OAuth(OAuthCredential {
        access: "access-token".to_string(),
        ..Default::default()
    });

    provider
        .refresh_models(refresh_context(
            PublishRecorder::default().into(),
            Some(credential),
            None,
            true,
        ))
        .await
        .unwrap();

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let bearer = requests[0]
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone());
    assert_eq!(bearer.as_deref(), Some("Bearer access-token"));
    assert_eq!(provider.get_models(), vec![remote_model()]);
}
