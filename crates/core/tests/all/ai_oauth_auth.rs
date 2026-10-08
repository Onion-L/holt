//! Port of `pi-core/ai/test/oauth-auth.test.ts` (the `OAuthAuth` adapter
//! cases; the `Models.getAuth` lazy-chain cases land with the provider
//! factories, and the module-barrel introspection has no Rust counterpart —
//! the compatibility barrel is type-only in `compat.rs`).

use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::anthropic::anthropic_oauth;
use pi_core::ai::auth::oauth::github_copilot::github_copilot_oauth;
use pi_core::ai::auth::oauth::kimi_coding::kimi_coding_oauth;
use pi_core::ai::auth::oauth::load::{
    load_anthropic_oauth, load_github_copilot_oauth, load_kimi_coding_oauth,
    load_open_router_oauth, load_openai_codex_oauth, load_xai_oauth,
};
use pi_core::ai::auth::oauth::openai_codex::openai_codex_oauth;
use pi_core::ai::auth::oauth::openrouter::open_router_oauth;
use pi_core::ai::auth::oauth::xai::xai_oauth;
use pi_core::ai::auth::types::{CredentialStore, OAuthAuth, OAuthCredential};
use pi_core::ai::types::FetchFunction;
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use tokio_util::sync::CancellationToken;

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

type RouteHandler = Arc<dyn Fn(&str) -> (u16, serde_json::Value) + Send + Sync>;

struct ScriptedFetch {
    handler: RouteHandler,
    urls: Mutex<Vec<String>>,
}

impl HttpFetch for ScriptedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.urls.lock().unwrap().push(request.url.clone());
        let (status, body) = (self.handler)(&request.url);
        let payload = serde_json::to_string(&body).unwrap();
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(payload))])),
            })
        })
    }
}

fn oauth_credential(access: &str, refresh: &str) -> OAuthCredential {
    OAuthCredential {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: 0,
        extra: Default::default(),
    }
}

#[test]
fn identifies_only_subscription_backed_oauth_flows_as_subscriptions() {
    for oauth in [
        anthropic_oauth(),
        openai_codex_oauth(),
        github_copilot_oauth(),
        kimi_coding_oauth(),
        xai_oauth(),
    ] {
        assert!(oauth.is_subscription(), "{}", oauth.name());
    }
    assert!(!open_router_oauth().is_subscription());
}

#[tokio::test]
async fn derives_api_keys_from_access_tokens() {
    let credential = oauth_credential("token", "r");
    for oauth in [anthropic_oauth(), openai_codex_oauth(), xai_oauth()] {
        let auth = oauth.to_auth(&credential).await.unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("token"), "{}", oauth.name());
    }
}

#[tokio::test]
async fn openrouter_keeps_the_permanent_credential_on_refresh() {
    let credential = OAuthCredential {
        access: "token".to_string(),
        refresh: String::new(),
        expires: MAX_SAFE_INTEGER,
        extra: Default::default(),
    };
    let auth = open_router_oauth().to_auth(&credential).await.unwrap();
    assert_eq!(auth.api_key.as_deref(), Some("token"));
    let refreshed = open_router_oauth()
        .refresh(&credential, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(refreshed, credential);
}

#[tokio::test]
async fn anthropic_refresh_exchanges_the_refresh_token() {
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
        urls: Mutex::new(Vec::new()),
    });

    let refreshed = AnthropicFlow(fetch as FetchFunction)
        .refresh(&oauth_credential("old", "old-r"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(refreshed.access, "new-access");
    assert_eq!(refreshed.refresh, "new-refresh");
    assert!(refreshed.expires > 0);
}

/// The default flow uses the real transport; this wrapper injects one.
struct AnthropicFlow(FetchFunction);

impl AnthropicFlow {
    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, pi_core::ai::auth::types::AuthStorageError> {
        pi_core::ai::auth::oauth::anthropic::AnthropicOAuth::new(
            Arc::clone(&self.0),
            Arc::new(pi_core::ai::auth::resolve::now_millis),
            Default::default(),
        )
        .refresh(credential, signal)
        .await
    }
}

#[tokio::test]
async fn github_copilot_refresh_preserves_the_enterprise_domain() {
    let fetch = Arc::new(ScriptedFetch {
        handler: Arc::new(|url| {
            if url.ends_with("/models") {
                (200, serde_json::json!({"data": []}))
            } else {
                (
                    200,
                    serde_json::json!({"token": "new-token", "expires_at": 9_999_999_999u64}),
                )
            }
        }),
        urls: Mutex::new(Vec::new()),
    });
    let credential = OAuthCredential {
        access: "old".to_string(),
        refresh: "gh-token".to_string(),
        expires: 0,
        extra: [(
            "enterpriseUrl".to_string(),
            serde_json::json!("company.ghe.com"),
        )]
        .into_iter()
        .collect(),
    };

    let refreshed = pi_core::ai::auth::oauth::github_copilot::GitHubCopilotOAuth::new(
        Arc::clone(&fetch) as FetchFunction,
        Arc::new(pi_core::ai::auth::resolve::now_millis),
    )
    .refresh(&credential, CancellationToken::new())
    .await
    .unwrap();

    assert_eq!(refreshed.access, "new-token");
    assert_eq!(
        refreshed.extra.get("enterpriseUrl"),
        Some(&serde_json::json!("company.ghe.com"))
    );
    let urls = fetch.urls.lock().unwrap();
    assert!(
        urls[0].contains("api.company.ghe.com"),
        "first url: {}",
        urls[0]
    );
}

#[test]
fn loaders_resolve_the_shared_flow_values() {
    // The TypeScript loaders dynamically import; the Rust loaders return the
    // statically linked flows under the same names.
    assert_eq!(load_anthropic_oauth().name(), "Anthropic (Claude Pro/Max)");
    assert_eq!(
        load_openai_codex_oauth().name(),
        "OpenAI (ChatGPT Plus/Pro)"
    );
    assert_eq!(load_github_copilot_oauth().name(), "GitHub Copilot");
    assert_eq!(load_open_router_oauth().name(), "OpenRouter OAuth");
    assert_eq!(load_kimi_coding_oauth().name(), "Kimi Code (subscription)");
    assert_eq!(load_xai_oauth().name(), "xAI (Grok/X subscription)");
}

#[tokio::test]
async fn github_copilot_to_auth_derives_urls_from_tokens() {
    let access = "tid=abc;exp=123;proxy-ep=proxy.enterprise.example;rest";
    let auth = github_copilot_oauth()
        .to_auth(&oauth_credential(access, "r"))
        .await
        .unwrap();
    assert_eq!(auth.api_key.as_deref(), Some(access));
    assert_eq!(
        auth.base_url.as_deref(),
        Some("https://api.enterprise.example")
    );
}

// ---------------------------------------------------------------------------
// OAuth through Models.getAuth (lazy load chain), landed with the provider
// factories.

#[tokio::test]
async fn resolves_stored_anthropic_oauth_credentials_via_the_lazy_flow_import() {
    let credentials = pi_core::ai::auth::credential_store::InMemoryCredentialStore::new();
    // Keep the expiry beyond getAuth()'s refresh window.
    let expires = pi_core::ai::auth::resolve::now_millis() + 10 * 60_000;
    credentials
        .modify(
            "anthropic",
            Box::new(move |_| {
                Box::pin(std::future::ready(Ok(Some(
                    pi_core::ai::auth::types::Credential::OAuth(OAuthCredential {
                        access: "oauth-access-token".to_string(),
                        refresh: "r".to_string(),
                        expires,
                        ..Default::default()
                    }),
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
            credentials: Some(Arc::new(credentials)),
            ..Default::default()
        },
    ));
    models.set_provider(pi_core::ai::providers::anthropic::anthropic_provider());

    let model = models.get_models(Some("anthropic"))[0].clone();
    let result = models
        .get_auth(&model.provider, None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth.api_key.as_deref(), Some("oauth-access-token"));
    assert_eq!(result.source.as_deref(), Some("OAuth"));
}

#[tokio::test]
async fn resolves_stored_github_copilot_oauth_credentials_including_per_credential_base_url() {
    let access = "tid=abc;exp=123;proxy-ep=proxy.business.githubcopilot.com;rest";
    let credentials = pi_core::ai::auth::credential_store::InMemoryCredentialStore::new();
    let expires = pi_core::ai::auth::resolve::now_millis() + 10 * 60_000;
    credentials
        .modify(
            "github-copilot",
            Box::new(move |_| {
                Box::pin(std::future::ready(Ok(Some(
                    pi_core::ai::auth::types::Credential::OAuth(OAuthCredential {
                        access: access.to_string(),
                        refresh: "r".to_string(),
                        expires,
                        ..Default::default()
                    }),
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
            credentials: Some(Arc::new(credentials)),
            ..Default::default()
        },
    ));
    models.set_provider(pi_core::ai::providers::builtin::github_copilot_provider());

    let model = models.get_models(Some("github-copilot"))[0].clone();
    let result = models
        .get_auth(&model.provider, None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth.api_key.as_deref(), Some(access));
    assert_eq!(
        result.auth.base_url.as_deref(),
        Some("https://api.business.githubcopilot.com")
    );
}
