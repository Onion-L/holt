//! Port of `pi-core/ai/test/providers.test.ts` (provider-auth cases; the
//! builtin-catalog and dispatch cases live in this file too as they land).

use std::sync::{Arc, Mutex};

use pi_core::ai::auth::types::{
    ApiKeyAuthInput, ApiKeyCredential, AuthContext, AuthEvent, AuthFuture, AuthInteraction,
    AuthPrompt, AuthStorageError,
};
use pi_core::ai::models::{CreateModelsOptions, Models};
use pi_core::ai::providers::cloudflare_ai_gateway::cloudflare_ai_gateway_provider;
use pi_core::ai::providers::cloudflare_workers_ai::cloudflare_workers_ai_provider;
use pi_core::ai::providers::google_vertex::google_vertex_provider;
use pi_core::ai::types::ProviderEnv;

/// The `fakeAuthContext(env, files)` helper from providers.test.ts.
struct FakeAuthContext {
    env: ProviderEnv,
    files: Vec<String>,
}

impl AuthContext for FakeAuthContext {
    fn env(&self, name: &str) -> AuthFuture<Option<String>> {
        Box::pin(std::future::ready(self.env.get(name).cloned()))
    }

    fn file_exists(&self, path: &str) -> AuthFuture<bool> {
        Box::pin(std::future::ready(
            self.files.iter().any(|file| file == path),
        ))
    }
}

fn fake_auth_context(env: &[(&str, &str)], files: &[&str]) -> Arc<FakeAuthContext> {
    Arc::new(FakeAuthContext {
        env: env
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        files: files.iter().map(|file| file.to_string()).collect(),
    })
}

fn auth_input(ctx: Arc<FakeAuthContext>, credential: Option<ApiKeyCredential>) -> ApiKeyAuthInput {
    ApiKeyAuthInput {
        ctx,
        credential,
        signal: tokio_util::sync::CancellationToken::new(),
    }
}

/// A prompt interaction answering from a scripted queue (the TS tests'
/// `prompt: async () => answers.shift()!`).
struct ScriptedInteraction {
    answers: Mutex<Vec<String>>,
    events: Mutex<Vec<AuthEvent>>,
}

impl AuthInteraction for ScriptedInteraction {
    fn signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        None
    }

    fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<Result<String, AuthStorageError>> {
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            (!answers.is_empty()).then(|| answers.remove(0))
        };
        Box::pin(std::future::ready(match answer {
            Some(answer) => Ok(answer),
            None => Err(AuthStorageError("no more answers".to_string())),
        }))
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn scripted(answers: &[&str]) -> Arc<ScriptedInteraction> {
    Arc::new(ScriptedInteraction {
        answers: Mutex::new(answers.iter().map(|answer| answer.to_string()).collect()),
        events: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn runs_provider_owned_vertex_api_key_and_adc_login_flows() {
    let auth = google_vertex_provider()
        .auth()
        .api_key
        .clone()
        .expect("api key auth");

    let key_interaction = scripted(&["api-key", "vertex-key"]);
    let credential = auth.login(key_interaction).expect("login").await.unwrap();
    assert_eq!(
        credential,
        ApiKeyCredential {
            key: Some("vertex-key".to_string()),
            env: None,
        }
    );

    let adc_interaction = scripted(&["adc", "project-id", "us-central1"]);
    let credential = auth
        .login(adc_interaction.clone())
        .expect("login")
        .await
        .unwrap();
    let mut expected_env = ProviderEnv::new();
    expected_env.insert("GOOGLE_CLOUD_PROJECT".to_string(), "project-id".to_string());
    expected_env.insert(
        "GOOGLE_CLOUD_LOCATION".to_string(),
        "us-central1".to_string(),
    );
    assert_eq!(
        credential,
        ApiKeyCredential {
            key: None,
            env: Some(expected_env),
        }
    );
    let events = adc_interaction.events.lock().unwrap().clone();
    match &events[0] {
        AuthEvent::Info { links, .. } => assert!(links.iter().any(|link| {
            link.label
                .as_deref()
                .is_some_and(|label| label.contains("Application Default Credentials"))
        })),
        other => panic!("expected info event, got {other:?}"),
    }

    let mut credential_env = ProviderEnv::new();
    credential_env.insert("GOOGLE_CLOUD_PROJECT".to_string(), "project-id".to_string());
    credential_env.insert(
        "GOOGLE_CLOUD_LOCATION".to_string(),
        "us-central1".to_string(),
    );
    let resolved = auth
        .resolve(auth_input(
            fake_auth_context(
                &[],
                &["~/.config/gcloud/application_default_credentials.json"],
            ),
            Some(ApiKeyCredential {
                key: None,
                env: Some(credential_env.clone()),
            }),
        ))
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(resolved.auth, Default::default());
    assert_eq!(resolved.env, Some(credential_env));
}

#[tokio::test]
async fn resolves_vertex_via_adc_file_plus_project_and_location() {
    let adc = "~/.config/gcloud/application_default_credentials.json";
    let configured = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[
                ("GOOGLE_CLOUD_PROJECT", "proj"),
                ("GOOGLE_CLOUD_LOCATION", "us-central1"),
            ],
            &[adc],
        )),
        ..Default::default()
    }));
    configured.set_provider(google_vertex_provider());

    let result = configured
        .get_auth("google-vertex", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth, Default::default());
    assert!(
        result
            .source
            .as_deref()
            .is_some_and(|source| source.contains("application default"))
    );

    // ADC without project/location is not configured.
    let partial = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[("GOOGLE_CLOUD_PROJECT", "proj")],
            &[adc],
        )),
        ..Default::default()
    }));
    partial.set_provider(google_vertex_provider());
    assert!(
        partial
            .get_auth("google-vertex", None, None)
            .await
            .unwrap()
            .is_none()
    );

    // An explicit key wins over ADC.
    let keyed = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[("GOOGLE_CLOUD_API_KEY", "vertex-key")],
            &[],
        )),
        ..Default::default()
    }));
    keyed.set_provider(google_vertex_provider());
    let result = keyed
        .get_auth("google-vertex", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth.api_key.as_deref(), Some("vertex-key"));
}

#[tokio::test]
async fn requires_cloudflare_workers_ai_account_config_and_returns_scoped_env() {
    let missing_account = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(&[("CLOUDFLARE_API_KEY", "cf-key")], &[])),
        ..Default::default()
    }));
    missing_account.set_provider(cloudflare_workers_ai_provider());
    assert!(
        missing_account
            .get_auth("cloudflare-workers-ai", None, None)
            .await
            .unwrap()
            .is_none()
    );

    let configured = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[
                ("CLOUDFLARE_API_KEY", "cf-key"),
                ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
            ],
            &[],
        )),
        ..Default::default()
    }));
    configured.set_provider(cloudflare_workers_ai_provider());
    let result = configured
        .get_auth("cloudflare-workers-ai", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth.api_key.as_deref(), Some("cf-key"));
    let mut expected_env = ProviderEnv::new();
    expected_env.insert(
        "CLOUDFLARE_ACCOUNT_ID".to_string(),
        "account-id".to_string(),
    );
    assert_eq!(result.env, Some(expected_env));
}

#[tokio::test]
async fn requires_cloudflare_ai_gateway_account_and_gateway_config_and_returns_scoped_env_headers()
{
    let missing_gateway = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[
                ("CLOUDFLARE_API_KEY", "cf-key"),
                ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
            ],
            &[],
        )),
        ..Default::default()
    }));
    missing_gateway.set_provider(cloudflare_ai_gateway_provider());
    assert!(
        missing_gateway
            .get_auth("cloudflare-ai-gateway", None, None)
            .await
            .unwrap()
            .is_none()
    );

    let configured = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[
                ("CLOUDFLARE_API_KEY", "cf-key"),
                ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
                ("CLOUDFLARE_GATEWAY_ID", "gateway-id"),
            ],
            &[],
        )),
        ..Default::default()
    }));
    configured.set_provider(cloudflare_ai_gateway_provider());
    let result = configured
        .get_auth("cloudflare-ai-gateway", None, None)
        .await
        .unwrap()
        .expect("configured");

    let mut expected_headers = pi_core::ai::types::ProviderHeaders::new();
    expected_headers.insert(
        "cf-aig-authorization".to_string(),
        Some("Bearer cf-key".to_string()),
    );
    expected_headers.insert("Authorization".to_string(), None);
    expected_headers.insert("x-api-key".to_string(), None);
    assert_eq!(result.auth.headers, Some(expected_headers));

    let mut expected_env = ProviderEnv::new();
    expected_env.insert(
        "CLOUDFLARE_ACCOUNT_ID".to_string(),
        "account-id".to_string(),
    );
    expected_env.insert(
        "CLOUDFLARE_GATEWAY_ID".to_string(),
        "gateway-id".to_string(),
    );
    assert_eq!(result.env, Some(expected_env));
}

// ---------------------------------------------------------------------------
// Builtin catalogue (providers.test.ts "builtin providers" cases).

#[tokio::test]
async fn builtin_models_registers_every_builtin_provider_with_models() {
    let models = pi_core::ai::providers::builtin::builtin_models(Default::default());
    let providers = models.get_providers();
    assert_eq!(
        providers.len(),
        pi_core::ai::providers::builtin::builtin_providers().len()
    );
    let ids: Vec<&str> = providers.iter().map(|p| p.id()).collect();
    assert!(ids.contains(&"anthropic"));

    let anthropic = models.get_model("anthropic", "claude-haiku-4-5").unwrap();
    assert_eq!(anthropic.api, "anthropic-messages");

    assert!(models.get_models(None).len() > 500);

    // Static providers list models immediately; radius is purely dynamic.
    for provider in &providers {
        let list = models.get_models(Some(provider.id()));
        if provider.id() == "radius" {
            assert!(list.is_empty());
            continue;
        }
        assert!(!list.is_empty(), "no models for {}", provider.id());
        assert!(list.iter().all(|m| m.provider == provider.id()));
    }
}

#[test]
fn stores_native_constrained_sampling_capabilities_in_model_metadata() {
    use pi_core::ai::providers::builtin::get_builtin_model;
    let gpt4o = get_builtin_model("openai", "gpt-4o").unwrap();
    assert_eq!(
        gpt4o.compat.as_ref().unwrap().supports_strict_mode,
        Some(true)
    );
    assert_eq!(
        gpt4o
            .compat
            .as_ref()
            .unwrap()
            .supports_open_ai_grammar_tools,
        None
    );
    let gpt54 = get_builtin_model("openai", "gpt-5.4").unwrap();
    let compat = gpt54.compat.as_ref().unwrap();
    assert_eq!(compat.supports_strict_mode, Some(true));
    assert_eq!(compat.supports_open_ai_grammar_tools, Some(true));
    let haiku = get_builtin_model("anthropic", "claude-haiku-4-5").unwrap();
    assert_eq!(
        haiku.compat.as_ref().unwrap().supports_strict_tools,
        Some(true)
    );
}

#[test]
fn uses_official_kimi_k3_pricing_for_moonshot_providers() {
    let models = pi_core::ai::providers::builtin::builtin_models(Default::default());
    for provider in ["moonshotai", "moonshotai-cn"] {
        let cost = models.get_model(provider, "kimi-k3").unwrap().cost;
        assert_eq!(cost.rates.input.0, 3.0, "{provider} input");
        assert_eq!(cost.rates.output.0, 15.0, "{provider} output");
        assert_eq!(cost.rates.cache_read.0, 0.3, "{provider} cacheRead");
        assert_eq!(cost.rates.cache_write.0, 0.0, "{provider} cacheWrite");
    }
}

#[test]
fn uses_api_equivalent_implied_pricing_for_kimi_coding_subscription_models() {
    let models = pi_core::ai::providers::builtin::builtin_models(Default::default());
    let expected: &[(&str, f64, f64, f64, f64)] = &[
        // (modelId, input, output, cacheRead, cacheWrite)
        ("k3", 3.0, 15.0, 0.3, 0.0),
        ("kimi-for-coding-highspeed", 1.9, 8.0, 0.38, 0.0),
    ];
    for (model_id, input, output, cache_read, cache_write) in expected {
        let cost = models
            .get_model("kimi-coding", model_id)
            .unwrap_or_else(|| panic!("missing kimi-coding/{model_id}"))
            .cost;
        assert_eq!(cost.rates.input.0, *input, "{model_id} input");
        assert_eq!(cost.rates.output.0, *output, "{model_id} output");
        assert_eq!(cost.rates.cache_read.0, *cache_read, "{model_id} cacheRead");
        assert_eq!(
            cost.rates.cache_write.0, *cache_write,
            "{model_id} cacheWrite"
        );
    }
}

#[tokio::test]
async fn resolves_anthropic_bearer_auth_from_env_with_auth_token_precedence() {
    let models = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[
                ("ANTHROPIC_AUTH_TOKEN", "auth-token"),
                ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
                ("ANTHROPIC_API_KEY", "api-key"),
            ],
            &[],
        )),
        ..Default::default()
    }));
    models.set_provider(pi_core::ai::providers::anthropic::anthropic_provider());

    let result = models
        .get_auth("anthropic", None, None)
        .await
        .unwrap()
        .expect("configured");
    let mut expected_headers = pi_core::ai::types::ProviderHeaders::new();
    expected_headers.insert(
        "Authorization".to_string(),
        Some("Bearer auth-token".to_string()),
    );
    assert_eq!(result.auth.headers, Some(expected_headers),);
    assert_eq!(result.source.as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));
}

#[tokio::test]
async fn preserves_anthropic_oauth_token_precedence_over_the_api_key() {
    let models = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(
            &[
                ("ANTHROPIC_API_KEY", "key"),
                ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
            ],
            &[],
        )),
        ..Default::default()
    }));
    models.set_provider(pi_core::ai::providers::anthropic::anthropic_provider());

    let result = models
        .get_auth("anthropic", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth.api_key.as_deref(), Some("oauth-token"));
    assert_eq!(result.source.as_deref(), Some("ANTHROPIC_OAUTH_TOKEN"));
}

// ---------------------------------------------------------------------------
// envApiKeyAuth (providers.test.ts cases).

#[tokio::test]
async fn env_api_key_auth_prefers_stored_credential_and_falls_back_in_order() {
    use pi_core::ai::auth::helpers::env_api_key_auth;
    let auth = env_api_key_auth("Test key", &["FIRST_KEY", "SECOND_KEY"]);

    let stored = auth
        .resolve(auth_input(
            fake_auth_context(&[("FIRST_KEY", "env")], &[]),
            Some(ApiKeyCredential {
                key: Some("stored".to_string()),
                env: None,
            }),
        ))
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(stored.auth.api_key.as_deref(), Some("stored"));
    assert_eq!(stored.source.as_deref(), Some("stored credential"));

    let second = auth
        .resolve(auth_input(
            fake_auth_context(&[("SECOND_KEY", "second")], &[]),
            None,
        ))
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(second.auth.api_key.as_deref(), Some("second"));
    assert_eq!(second.source.as_deref(), Some("SECOND_KEY"));

    assert!(
        auth.resolve(auth_input(fake_auth_context(&[], &[]), None))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn env_api_key_auth_login_prompts_for_a_secret() {
    use pi_core::ai::auth::helpers::env_api_key_auth;
    use pi_core::ai::auth::types::AuthPromptKind;
    let auth = env_api_key_auth("Test key", &["TEST_KEY"]);

    struct PromptInspectingInteraction;
    impl AuthInteraction for PromptInspectingInteraction {
        fn signal(&self) -> Option<tokio_util::sync::CancellationToken> {
            None
        }
        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<Result<String, AuthStorageError>> {
            assert!(matches!(prompt.kind, AuthPromptKind::Secret { .. }));
            Box::pin(std::future::ready(Ok("entered-key".to_string())))
        }
        fn notify(&self, _event: AuthEvent) {}
    }

    let credential = auth
        .login(Arc::new(PromptInspectingInteraction))
        .expect("login supported")
        .await
        .unwrap();
    assert_eq!(
        credential,
        ApiKeyCredential {
            key: Some("entered-key".to_string()),
            env: None,
        }
    );
}

// ---------------------------------------------------------------------------
// Builtin images models (images-models.test.ts case 6).

#[tokio::test]
async fn builtin_images_models_registers_the_openrouter_provider_with_its_catalog() {
    use pi_core::ai::providers::builtin::builtin_images_models;
    let models = builtin_images_models(CreateModelsOptions {
        auth_context: Some(fake_auth_context(&[("OPENROUTER_API_KEY", "or-key")], &[])),
        ..Default::default()
    });

    let providers = models.get_providers();
    let ids: Vec<&str> = providers.iter().map(|p| p.id()).collect();
    assert_eq!(ids, vec!["openrouter"]);

    let list = models.get_models(Some("openrouter"));
    assert!(!list.is_empty());
    assert!(list.iter().all(|m| m.api == "openrouter-images"));

    let auth = models
        .get_auth_for_model(&list[0], None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(auth.auth.api_key.as_deref(), Some("or-key"));
}

// ---------------------------------------------------------------------------
// createProvider (providers.test.ts cases).

/// The `recordingStreams` helper from providers.test.ts.
#[derive(Default)]
struct RecordingStreams {
    label: String,
    calls: Arc<Mutex<Vec<String>>>,
}

impl pi_core::ai::models::ProviderStreams for RecordingStreams {
    fn stream(
        &self,
        model: &pi_core::ai::types::Model,
        _context: &pi_core::ai::types::Context,
        _options: Option<&pi_core::ai::types::StreamOptions>,
    ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.label, model.id));
        completed_stream()
    }

    fn stream_simple(
        &self,
        model: &pi_core::ai::types::Model,
        _context: &pi_core::ai::types::Context,
        _options: Option<&pi_core::ai::types::SimpleStreamOptions>,
    ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.label, model.id));
        completed_stream()
    }
}

fn completed_stream() -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
    let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
    let message = pi_core::ai::providers::faux::faux_assistant_message(
        "ok",
        pi_core::ai::providers::faux::FauxMessageOptions::default(),
    );
    stream.push(pi_core::ai::types::AssistantMessageEvent::Start {
        partial: message.clone(),
    });
    stream.push(pi_core::ai::types::AssistantMessageEvent::Done {
        reason: pi_core::ai::types::DoneReason::Stop,
        message: message.clone(),
    });
    stream.end(Some(message));
    stream
}

/// The `{ apiKey: { name: "Test", resolve: async () => ({ auth: {} }) } }` auth.
struct UnconfiguredApiKeyAuth;

impl pi_core::ai::auth::types::ApiKeyAuth for UnconfiguredApiKeyAuth {
    fn name(&self) -> &str {
        "Test"
    }
    fn resolve(
        &self,
        _input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<pi_core::ai::auth::types::AuthResult>, AuthStorageError>> {
        Box::pin(std::future::ready(Ok(Some(
            pi_core::ai::auth::types::AuthResult::default(),
        ))))
    }
}

fn plain_test_model(api: &str, id: &str, provider: &str) -> pi_core::ai::types::Model {
    pi_core::ai::types::Model {
        id: id.to_string(),
        name: id.to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: "https://example.test/v1".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        context_window: 10_000,
        max_tokens: 1000,
        ..Default::default()
    }
}

#[tokio::test]
async fn lazily_exposes_only_declared_deferred_capabilities() {
    // The TS case counts lazy module loads; in Rust the adapter is linked
    // statically, so the observable contract is which capabilities exist.
    struct DeferredOnlyStreams;
    impl pi_core::ai::models::ProviderStreams for DeferredOnlyStreams {
        fn stream(
            &self,
            _model: &pi_core::ai::types::Model,
            _context: &pi_core::ai::types::Context,
            _options: Option<&pi_core::ai::types::StreamOptions>,
        ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
            completed_stream()
        }
        fn stream_simple(
            &self,
            _model: &pi_core::ai::types::Model,
            _context: &pi_core::ai::types::Context,
            _options: Option<&pi_core::ai::types::SimpleStreamOptions>,
        ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
            completed_stream()
        }
        fn fetch_deferred(
            &self,
            model: &pi_core::ai::types::Model,
            _handle: &pi_core::ai::types::DeferredHandle,
            _options: Option<&pi_core::ai::types::DeferredFetchOptions>,
        ) -> Option<pi_core::ai::utils::event_stream::AssistantMessageEventStream> {
            Some(self.stream_simple(model, &pi_core::ai::types::Context::default(), None))
        }
        fn supports_deferred(&self) -> bool {
            true
        }
    }

    let model = plain_test_model("api-a", "model-a", "mixed");
    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "mixed".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(UnconfiguredApiKeyAuth)),
            models: vec![model.clone()],
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::Single(Arc::new(DeferredOnlyStreams)),
        });
    let handle = pi_core::ai::types::DeferredHandle {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
        api: model.api.clone(),
        id: "response-1".to_string(),
        ..Default::default()
    };

    assert!(provider.supports_deferred());
    assert!(!provider.supports_cancel_deferred());
    let result = provider
        .fetch_deferred(&model, &handle, None)
        .expect("fetch deferred wired")
        .result()
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);
}

#[tokio::test]
async fn dispatches_on_model_api_for_mixed_api_providers() {
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut by_api = std::collections::BTreeMap::new();
    for (api, label) in [("api-a", "a"), ("api-b", "b")] {
        by_api.insert(
            api.to_string(),
            Arc::new(RecordingStreams {
                label: label.to_string(),
                calls: Arc::clone(&calls),
            }) as Arc<dyn pi_core::ai::models::ProviderStreams>,
        );
    }
    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "mixed".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(UnconfiguredApiKeyAuth)),
            models: vec![
                plain_test_model("api-a", "model-a", "mixed"),
                plain_test_model("api-b", "model-b", "mixed"),
            ],
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::ByApi(by_api),
        });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(provider);

    models
        .complete_simple(
            &plain_test_model("api-a", "model-a", "mixed"),
            &pi_core::ai::types::Context::default(),
            None,
        )
        .await;
    models
        .complete_simple(
            &plain_test_model("api-b", "model-b", "mixed"),
            &pi_core::ai::types::Context::default(),
            None,
        )
        .await;
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["a:model-a".to_string(), "b:model-b".to_string()]
    );
}

#[tokio::test]
async fn merges_provider_resolved_env_into_stream_options() {
    struct EnvAuth;
    impl pi_core::ai::auth::types::ApiKeyAuth for EnvAuth {
        fn name(&self) -> &str {
            "Test"
        }
        fn resolve(
            &self,
            _input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<pi_core::ai::auth::types::AuthResult>, AuthStorageError>>
        {
            Box::pin(std::future::ready(Ok(Some(
                pi_core::ai::auth::types::AuthResult {
                    auth: pi_core::ai::auth::types::ModelAuth {
                        api_key: Some("provider-key".to_string()),
                        ..Default::default()
                    },
                    env: Some(
                        [("PROVIDER_ONLY", "provider"), ("SHARED", "provider")]
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    ),
                    source: None,
                },
            ))))
        }
    }

    #[derive(Default)]
    struct CapturingStreams {
        env: Mutex<Option<pi_core::ai::types::ProviderEnv>>,
        api_key: Mutex<Option<String>>,
    }
    impl pi_core::ai::models::ProviderStreams for CapturingStreams {
        fn stream(
            &self,
            _model: &pi_core::ai::types::Model,
            _context: &pi_core::ai::types::Context,
            options: Option<&pi_core::ai::types::StreamOptions>,
        ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
            *self.env.lock().unwrap() = options.and_then(|o| o.base.env.clone());
            *self.api_key.lock().unwrap() = options.and_then(|o| o.base.api_key.clone());
            completed_stream()
        }
        fn stream_simple(
            &self,
            model: &pi_core::ai::types::Model,
            context: &pi_core::ai::types::Context,
            options: Option<&pi_core::ai::types::SimpleStreamOptions>,
        ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
            self.stream(model, context, options.map(|o| &o.base))
        }
    }

    let capturing = Arc::new(CapturingStreams::default());
    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "env-provider".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(EnvAuth)),
            models: vec![plain_test_model("api-a", "model-a", "env-provider")],
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::Single(
                Arc::clone(&capturing) as Arc<dyn pi_core::ai::models::ProviderStreams>
            ),
        });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(provider);

    let mut env = pi_core::ai::types::ProviderEnv::new();
    env.insert("REQUEST_ONLY".to_string(), "request".to_string());
    env.insert("SHARED".to_string(), "request".to_string());
    let options = pi_core::ai::types::SimpleStreamOptions {
        base: pi_core::ai::types::StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("request-key".to_string()),
                env: Some(env),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    models
        .complete_simple(
            &plain_test_model("api-a", "model-a", "env-provider"),
            &pi_core::ai::types::Context::default(),
            Some(options),
        )
        .await;

    assert_eq!(
        capturing.api_key.lock().unwrap().as_deref(),
        Some("request-key")
    );
    let mut expected = pi_core::ai::types::ProviderEnv::new();
    expected.insert("PROVIDER_ONLY".to_string(), "provider".to_string());
    expected.insert("REQUEST_ONLY".to_string(), "request".to_string());
    expected.insert("SHARED".to_string(), "request".to_string());
    assert_eq!(*capturing.env.lock().unwrap(), Some(expected));
}

#[tokio::test]
async fn applies_resolved_request_options_to_deferred_fetch_and_cancellation() {
    use pi_core::ai::models::{ModelsDeferredCancelOptions, ModelsDeferredFetchOptions};
    use pi_core::ai::types::{DeferredFetchOptions, TransformHeadersFn};

    struct DeferredAuth;
    impl pi_core::ai::auth::types::ApiKeyAuth for DeferredAuth {
        fn name(&self) -> &str {
            "Test"
        }
        fn resolve(
            &self,
            _input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<pi_core::ai::auth::types::AuthResult>, AuthStorageError>>
        {
            Box::pin(std::future::ready(Ok(Some(
                pi_core::ai::auth::types::AuthResult {
                    auth: pi_core::ai::auth::types::ModelAuth {
                        api_key: Some("provider-key".to_string()),
                        base_url: Some("https://resolved.test/v1".to_string()),
                        headers: Some(
                            [
                                ("Authorization", Some("Bearer provider")),
                                ("X-Shared", Some("provider")),
                            ]
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.map(|v| v.to_string())))
                            .collect(),
                        ),
                    },
                    env: Some(
                        [("PROVIDER_ONLY", "provider"), ("SHARED", "provider")]
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    ),
                    source: None,
                },
            ))))
        }
    }

    #[derive(Default)]
    struct DeferredCaptureStreams {
        fetched_model: Mutex<Option<pi_core::ai::types::Model>>,
        fetched_options: Mutex<Option<DeferredFetchOptions>>,
        cancelled_options: Mutex<Option<pi_core::ai::types::ProviderRequestOptions>>,
    }
    impl pi_core::ai::models::ProviderStreams for DeferredCaptureStreams {
        fn stream(
            &self,
            _model: &pi_core::ai::types::Model,
            _context: &pi_core::ai::types::Context,
            _options: Option<&pi_core::ai::types::StreamOptions>,
        ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
            completed_stream()
        }
        fn stream_simple(
            &self,
            _model: &pi_core::ai::types::Model,
            _context: &pi_core::ai::types::Context,
            _options: Option<&pi_core::ai::types::SimpleStreamOptions>,
        ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
            completed_stream()
        }
        fn fetch_deferred(
            &self,
            model: &pi_core::ai::types::Model,
            _handle: &pi_core::ai::types::DeferredHandle,
            options: Option<&DeferredFetchOptions>,
        ) -> Option<pi_core::ai::utils::event_stream::AssistantMessageEventStream> {
            *self.fetched_model.lock().unwrap() = Some(model.clone());
            *self.fetched_options.lock().unwrap() = options.cloned();
            Some(completed_stream())
        }
        fn cancel_deferred(
            &self,
            _model: &pi_core::ai::types::Model,
            _handle: &pi_core::ai::types::DeferredHandle,
            options: Option<&pi_core::ai::types::ProviderRequestOptions>,
        ) -> Option<
            futures::future::BoxFuture<
                'static,
                Result<(), pi_core::ai::auth::resolve::ModelsError>,
            >,
        > {
            *self.cancelled_options.lock().unwrap() = options.cloned();
            Some(Box::pin(std::future::ready(Ok(()))))
        }
        fn supports_deferred(&self) -> bool {
            true
        }
        fn supports_cancel_deferred(&self) -> bool {
            true
        }
    }

    let capturing = Arc::new(DeferredCaptureStreams::default());
    let model = plain_test_model("api-a", "model-a", "deferred-provider");
    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "deferred-provider".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(DeferredAuth)),
            models: vec![model.clone()],
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::Single(
                Arc::clone(&capturing) as Arc<dyn pi_core::ai::models::ProviderStreams>
            ),
        });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(provider);
    let handle = pi_core::ai::types::DeferredHandle {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
        api: model.api.clone(),
        id: "response-1".to_string(),
        ..Default::default()
    };

    let add_transformed: TransformHeadersFn = Arc::new(|mut headers| {
        Box::pin(async move {
            headers.insert("X-Transformed".to_string(), Some("yes".to_string()));
            headers
        })
    });
    let add_cancel: TransformHeadersFn = Arc::new(|mut headers| {
        Box::pin(async move {
            headers.insert("X-Cancel".to_string(), Some("yes".to_string()));
            headers
        })
    });
    let mut env = pi_core::ai::types::ProviderEnv::new();
    env.insert("REQUEST_ONLY".to_string(), "request".to_string());
    env.insert("SHARED".to_string(), "request".to_string());
    let mut headers = pi_core::ai::types::ProviderHeaders::new();
    headers.insert("X-Request".to_string(), Some("request".to_string()));
    headers.insert("x-shared".to_string(), Some("request".to_string()));

    models
        .fetch_deferred(
            &model,
            &handle,
            Some(ModelsDeferredFetchOptions {
                base: pi_core::ai::types::ProviderRequestOptions {
                    api_key: Some("request-key".to_string()),
                    headers: Some(headers),
                    env: Some(env),
                    timeout_ms: Some(100),
                    ..Default::default()
                },
                wait: Some(50),
                transform_headers: Some(add_transformed),
            }),
        )
        .await;
    models
        .cancel_deferred(
            &model,
            &handle,
            Some(ModelsDeferredCancelOptions {
                base: pi_core::ai::types::ProviderRequestOptions {
                    timeout_ms: Some(200),
                    ..Default::default()
                },
                transform_headers: Some(add_cancel),
            }),
        )
        .await
        .unwrap();

    let fetched_model = capturing.fetched_model.lock().unwrap().clone().unwrap();
    assert_eq!(fetched_model.base_url, "https://resolved.test/v1");
    let fetched = capturing.fetched_options.lock().unwrap().clone().unwrap();
    assert_eq!(fetched.wait, Some(50));
    assert_eq!(fetched.base.timeout_ms, Some(100));
    assert_eq!(fetched.base.api_key.as_deref(), Some("request-key"));
    let mut expected_headers = pi_core::ai::types::ProviderHeaders::new();
    expected_headers.insert(
        "Authorization".to_string(),
        Some("Bearer provider".to_string()),
    );
    expected_headers.insert("X-Request".to_string(), Some("request".to_string()));
    expected_headers.insert("x-shared".to_string(), Some("request".to_string()));
    expected_headers.insert("X-Transformed".to_string(), Some("yes".to_string()));
    assert_eq!(fetched.base.headers, Some(expected_headers));
    let mut expected_env = pi_core::ai::types::ProviderEnv::new();
    expected_env.insert("PROVIDER_ONLY".to_string(), "provider".to_string());
    expected_env.insert("REQUEST_ONLY".to_string(), "request".to_string());
    expected_env.insert("SHARED".to_string(), "request".to_string());
    assert_eq!(fetched.base.env, Some(expected_env));

    let cancelled = capturing.cancelled_options.lock().unwrap().clone().unwrap();
    assert_eq!(cancelled.timeout_ms, Some(200));
    assert_eq!(cancelled.api_key.as_deref(), Some("provider-key"));
    let mut expected_headers = pi_core::ai::types::ProviderHeaders::new();
    expected_headers.insert(
        "Authorization".to_string(),
        Some("Bearer provider".to_string()),
    );
    expected_headers.insert("X-Shared".to_string(), Some("provider".to_string()));
    expected_headers.insert("X-Cancel".to_string(), Some("yes".to_string()));
    assert_eq!(cancelled.headers, Some(expected_headers));
    let mut expected_env = pi_core::ai::types::ProviderEnv::new();
    expected_env.insert("PROVIDER_ONLY".to_string(), "provider".to_string());
    expected_env.insert("SHARED".to_string(), "provider".to_string());
    assert_eq!(cancelled.env, Some(expected_env));
}

// ---------------------------------------------------------------------------
// Faux provider through a Models collection (providers.test.ts cases).

use pi_core::ai::providers::faux::{
    FauxDeferredOptions, FauxMessageOptions, FauxResponseStep, faux_assistant_message,
    faux_provider,
};

fn text_of(message: &pi_core::ai::types::AssistantMessage) -> String {
    match &message.content[0] {
        pi_core::ai::types::AssistantContent::Text(text) => text.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

#[tokio::test]
async fn streams_queued_responses_through_a_models_collection() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("hello from faux", FauxMessageOptions::default()),
    ))]);

    let model = models.get_models(Some(faux.provider.id()))[0].clone();
    let result = models
        .complete_simple(&model, &pi_core::ai::types::Context::default(), None)
        .await;
    assert_eq!(result.stop_reason, pi_core::ai::types::StopReason::Stop);
    assert_eq!(text_of(&result), "hello from faux");
    assert_eq!(faux.state.lock().unwrap().call_count, 1);
}

#[tokio::test]
async fn submits_polls_and_redeems_deferred_responses() {
    let faux = faux_provider(pi_core::ai::providers::faux::RegisterFauxProviderOptions {
        deferred: Some(FauxDeferredOptions {
            pending_fetches: Some(1),
            poll_after_ms: Some(25),
        }),
        ..Default::default()
    });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("ready", FauxMessageOptions::default()),
    ))]);
    let model = faux.get_model();

    let submission = models.stream_simple(
        &model,
        &pi_core::ai::types::Context::default(),
        Some(pi_core::ai::types::SimpleStreamOptions {
            deferred: Some(pi_core::ai::types::DeferredPreference::Window(
                pi_core::ai::types::DeferredWindow::H1,
            )),
            ..Default::default()
        }),
    );
    let deferred = submission.result().await;
    assert_eq!(
        deferred.stop_reason,
        pi_core::ai::types::StopReason::Deferred
    );
    assert!(deferred.content.is_empty());
    let handle = deferred.deferred.clone().expect("deferred handle");
    assert_eq!(handle.provider, model.provider);
    assert_eq!(handle.model_id, model.id);
    assert_eq!(handle.api, model.api);
    assert!(!handle.id.is_empty());
    assert_eq!(handle.poll_after_ms, Some(25));

    let pending = models.fetch_deferred(&model, &handle, None).await;
    assert_eq!(
        pending.stop_reason,
        pi_core::ai::types::StopReason::Deferred
    );
    assert_eq!(pending.deferred, Some(handle.clone()));

    let ready = models
        .fetch_deferred(
            &model,
            &handle,
            Some(pi_core::ai::models::ModelsDeferredFetchOptions {
                wait: Some(0),
                ..Default::default()
            }),
        )
        .await;
    assert_eq!(ready.stop_reason, pi_core::ai::types::StopReason::Stop);
    assert_eq!(text_of(&ready), "ready");
    assert!(ready.usage.total_tokens > 0);
    let state = faux.state.lock().unwrap();
    assert_eq!(state.call_count, 1);
    assert_eq!(state.deferred_fetch_count, 2);
}

#[tokio::test]
async fn records_cancellation_and_returns_deferred_fetch_failures_in_band() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    // The TS case rejects from a response factory; the Rust factory port
    // returns the error-shaped message a rejection would surface.
    faux.set_responses(vec![
        FauxResponseStep::Factory(Arc::new(|_, _, _, _| Err("deferred failed".to_string()))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "cancelled",
            FauxMessageOptions::default(),
        ))),
    ]);
    let model = faux.get_model();

    let failed_submission = models
        .complete_simple(
            &model,
            &pi_core::ai::types::Context::default(),
            Some(pi_core::ai::types::SimpleStreamOptions {
                deferred: Some(pi_core::ai::types::DeferredPreference::Enabled),
                ..Default::default()
            }),
        )
        .await;
    let failed_handle = failed_submission.deferred.clone().expect("deferred handle");
    let failed = models.fetch_deferred(&model, &failed_handle, None).await;
    assert_eq!(failed.stop_reason, pi_core::ai::types::StopReason::Error);
    assert_eq!(failed.error_message.as_deref(), Some("deferred failed"));

    let cancelled_submission = models
        .complete_simple(
            &model,
            &pi_core::ai::types::Context::default(),
            Some(pi_core::ai::types::SimpleStreamOptions {
                deferred: Some(pi_core::ai::types::DeferredPreference::Enabled),
                ..Default::default()
            }),
        )
        .await;
    let cancelled_handle = cancelled_submission
        .deferred
        .clone()
        .expect("deferred handle");
    models
        .cancel_deferred(&model, &cancelled_handle, None)
        .await
        .unwrap();
    assert_eq!(
        faux.state.lock().unwrap().cancelled_deferred,
        vec![cancelled_handle.clone()]
    );
    let cancelled = models.fetch_deferred(&model, &cancelled_handle, None).await;
    assert_eq!(cancelled.stop_reason, pi_core::ai::types::StopReason::Error);
    assert!(
        cancelled
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("was cancelled"))
    );
}

#[tokio::test]
async fn lets_a_newer_dynamic_refresh_bypass_and_supersede_older_network_work() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fetches = Arc::new(AtomicUsize::new(0));
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_first_tx, finish_first_rx) = tokio::sync::oneshot::channel::<()>();
    let finish_gate = Arc::new(Mutex::new(Some(finish_first_rx)));

    let fetch_counter = Arc::clone(&fetches);
    let gate = Arc::clone(&finish_gate);
    let first_started = Mutex::new(Some(first_started_tx));
    let fetch_models: pi_core::ai::models::FetchModelsFn = Arc::new(move |_context| {
        let current = fetch_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let gate = Arc::clone(&gate);
        let started = (current == 1)
            .then(|| first_started.lock().unwrap().take())
            .flatten();
        Box::pin(async move {
            if let Some(started) = started {
                let _ = started.send(());
                let receiver = gate.lock().unwrap().take();
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
            }
            Ok(vec![plain_test_model(
                "api-a",
                &format!("listed-{current}"),
                "dynamic",
            )])
        })
    });

    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "dynamic".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(UnconfiguredApiKeyAuth)),
            models: Vec::new(),
            fetch_models: Some(fetch_models),
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::Single(Arc::new(RecordingStreams::default())
                as Arc<dyn pi_core::ai::models::ProviderStreams>),
        });

    let store = Arc::new(pi_core::ai::models_store::InMemoryModelsStore::new());
    let models = Arc::new(Models::new(CreateModelsOptions {
        models_store: Some(Arc::clone(&store) as Arc<dyn pi_core::ai::models_store::ModelsStore>),
        ..Default::default()
    }));
    models.set_provider(provider);
    assert!(models.get_models(Some("dynamic")).is_empty());

    let first_models = Arc::clone(&models);
    let first = tokio::spawn(async move {
        first_models
            .refresh(pi_core::ai::models::ModelsRefreshOptions {
                providers: Some(vec!["dynamic".to_string()]),
                ..Default::default()
            })
            .await
    });
    first_started_rx.await.unwrap();
    let second = models.refresh(pi_core::ai::models::ModelsRefreshOptions {
        providers: Some(vec!["dynamic".to_string()]),
        ..Default::default()
    });
    let second = second.await;
    let first = first.await.unwrap();
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert!(!second.aborted);
    assert!(!first.aborted);
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    assert_eq!(
        models
            .get_models(Some("dynamic"))
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["listed-2"]
    );
    let stored = pi_core::ai::models_store::ModelsStore::read(store.as_ref(), "dynamic", None)
        .await
        .unwrap();
    assert_eq!(
        stored
            .expect("stored entry")
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["listed-2"]
    );

    // The superseded fetch finishing later must not clobber the newer
    // publication (generation checks reject it).
    let _ = finish_first_tx.send(());
    tokio::task::yield_now().await;
    assert_eq!(
        models
            .get_models(Some("dynamic"))
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["listed-2"]
    );
}

#[tokio::test]
async fn runs_provider_owned_bedrock_bearer_token_and_aws_profile_login_flows() {
    use pi_core::ai::providers::amazon_bedrock::amazon_bedrock_provider;

    let auth = amazon_bedrock_provider()
        .auth()
        .api_key
        .clone()
        .expect("api key auth");

    let bearer_interaction = scripted(&["bearer-token", "bedrock-token"]);
    let credential = auth
        .login(bearer_interaction)
        .expect("login")
        .await
        .unwrap();
    assert_eq!(
        credential,
        ApiKeyCredential {
            key: Some("bedrock-token".to_string()),
            env: None,
        }
    );

    let profile_interaction = scripted(&["aws-profile", "work"]);
    let credential = auth
        .login(profile_interaction.clone())
        .expect("login")
        .await
        .unwrap();
    let mut expected_env = ProviderEnv::new();
    expected_env.insert("AWS_PROFILE".to_string(), "work".to_string());
    assert_eq!(
        credential,
        ApiKeyCredential {
            key: None,
            env: Some(expected_env),
        }
    );
    let events = profile_interaction.events.lock().unwrap().clone();
    match &events[0] {
        AuthEvent::Info { links, .. } => assert!(links.iter().any(|link| {
            link.label
                .as_deref()
                .is_some_and(|label| label.contains("AWS credential provider chain"))
        })),
        other => panic!("expected info event, got {other:?}"),
    }

    let mut credential_env = ProviderEnv::new();
    credential_env.insert("AWS_PROFILE".to_string(), "work".to_string());
    let resolved = auth
        .resolve(auth_input(
            fake_auth_context(&[], &[]),
            Some(ApiKeyCredential {
                key: None,
                env: Some(credential_env.clone()),
            }),
        ))
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(resolved.auth, Default::default());
    assert_eq!(resolved.env, Some(credential_env));
}

#[tokio::test]
async fn reports_bedrock_as_configured_from_ambient_aws_credentials_without_an_api_key() {
    use pi_core::ai::providers::amazon_bedrock::amazon_bedrock_provider;

    let models = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(&[("AWS_PROFILE", "dev")], &[])),
        ..Default::default()
    }));
    models.set_provider(amazon_bedrock_provider());
    let model = models.get_models(Some("amazon-bedrock"))[0].clone();

    let result = models
        .get_auth(&model.provider, None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(result.auth, Default::default());
    assert_eq!(result.source.as_deref(), Some("AWS_PROFILE"));

    let unconfigured = Arc::new(Models::new(CreateModelsOptions {
        auth_context: Some(fake_auth_context(&[], &[])),
        ..Default::default()
    }));
    unconfigured.set_provider(amazon_bedrock_provider());
    assert!(
        unconfigured
            .get_auth("amazon-bedrock", None, None)
            .await
            .unwrap()
            .is_none()
    );
}

// Per-provider factories (`all.ts` exports). The thin wrappers must produce
// exactly what the aggregate `builtinProviders()` path builds, so each case
// pins the provider id and a distinguishing field from the uniform spec.
#[test]
fn per_provider_factories_match_the_all_ts_registry() {
    use pi_core::ai::providers::builtin::{
        ant_ling_provider, azure_openai_responses_provider, baseten_provider, cerebras_provider,
        deepseek_provider, google_provider, groq_provider, huggingface_provider,
        minimax_cn_provider, minimax_provider, mistral_provider, moonshotai_cn_provider,
        moonshotai_provider, nvidia_provider, openai_provider, qwen_token_plan_cn_provider,
        qwen_token_plan_individual_provider, qwen_token_plan_provider, radius_provider,
        together_provider, vercel_ai_gateway_provider, xiaomi_provider,
        xiaomi_token_plan_ams_provider, xiaomi_token_plan_cn_provider,
        xiaomi_token_plan_sgp_provider, zai_coding_cn_provider, zai_provider,
    };
    use pi_core::ai::providers::radius::RadiusProviderOptions;

    type FactoryCase<'a> = (
        &'a str,
        &'a str,
        Option<&'a str>,
        Arc<dyn pi_core::ai::models::Provider>,
    );

    let cases: Vec<FactoryCase> = vec![
        (
            "ant-ling",
            "Ant Ling",
            Some("https://api.ant-ling.com/v1"),
            ant_ling_provider(),
        ),
        (
            "baseten",
            "Baseten",
            Some("https://inference.baseten.co/v1"),
            baseten_provider(),
        ),
        (
            "cerebras",
            "Cerebras",
            Some("https://api.cerebras.ai/v1"),
            cerebras_provider(),
        ),
        (
            "deepseek",
            "DeepSeek",
            Some("https://api.deepseek.com"),
            deepseek_provider(),
        ),
        (
            "groq",
            "Groq",
            Some("https://api.groq.com/openai/v1"),
            groq_provider(),
        ),
        (
            "huggingface",
            "Hugging Face",
            Some("https://router.huggingface.co/v1"),
            huggingface_provider(),
        ),
        (
            "moonshotai",
            "Moonshot AI",
            Some("https://api.moonshot.ai/v1"),
            moonshotai_provider(),
        ),
        (
            "moonshotai-cn",
            "Moonshot AI CN",
            Some("https://api.moonshot.cn/v1"),
            moonshotai_cn_provider(),
        ),
        (
            "nvidia",
            "NVIDIA",
            Some("https://integrate.api.nvidia.com/v1"),
            nvidia_provider(),
        ),
        (
            "together",
            "Together",
            Some("https://api.together.ai/v1"),
            together_provider(),
        ),
        (
            "xiaomi",
            "Xiaomi",
            Some("https://api.xiaomimimo.com/v1"),
            xiaomi_provider(),
        ),
        (
            "xiaomi-token-plan-ams",
            "Xiaomi Token Plan AMS",
            Some("https://token-plan-ams.xiaomimimo.com/v1"),
            xiaomi_token_plan_ams_provider(),
        ),
        (
            "xiaomi-token-plan-cn",
            "Xiaomi Token Plan CN",
            Some("https://token-plan-cn.xiaomimimo.com/v1"),
            xiaomi_token_plan_cn_provider(),
        ),
        (
            "xiaomi-token-plan-sgp",
            "Xiaomi Token Plan SGP",
            Some("https://token-plan-sgp.xiaomimimo.com/v1"),
            xiaomi_token_plan_sgp_provider(),
        ),
        (
            "zai",
            "Z.AI",
            Some("https://api.z.ai/api/coding/paas/v4"),
            zai_provider(),
        ),
        (
            "zai-coding-cn",
            "Z.AI Coding CN",
            Some("https://open.bigmodel.cn/api/coding/paas/v4"),
            zai_coding_cn_provider(),
        ),
        (
            "qwen-token-plan",
            "Qwen Token Plan",
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
            qwen_token_plan_provider(),
        ),
        (
            "qwen-token-plan-cn",
            "Qwen Token Plan CN",
            Some("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"),
            qwen_token_plan_cn_provider(),
        ),
        (
            "qwen-token-plan-individual",
            "Qwen Token Plan Individual",
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
            qwen_token_plan_individual_provider(),
        ),
        (
            "openai",
            "OpenAI",
            Some("https://api.openai.com/v1"),
            openai_provider(),
        ),
        (
            "azure-openai-responses",
            "Azure OpenAI",
            None,
            azure_openai_responses_provider(),
        ),
        (
            "mistral",
            "Mistral",
            Some("https://api.mistral.ai"),
            mistral_provider(),
        ),
        (
            "minimax",
            "MiniMax",
            Some("https://api.minimax.io/anthropic"),
            minimax_provider(),
        ),
        (
            "minimax-cn",
            "MiniMax CN",
            Some("https://api.minimaxi.com/anthropic"),
            minimax_cn_provider(),
        ),
        (
            "vercel-ai-gateway",
            "Vercel AI Gateway",
            Some("https://ai-gateway.vercel.sh"),
            vercel_ai_gateway_provider(),
        ),
        (
            "google",
            "Google",
            Some("https://generativelanguage.googleapis.com/v1beta"),
            google_provider(),
        ),
    ];

    let aggregate_ids: std::collections::BTreeSet<String> =
        pi_core::ai::providers::builtin::builtin_providers()
            .into_iter()
            .map(|provider| provider.id().to_string())
            .collect();

    for (id, name, base_url, provider) in cases {
        assert_eq!(provider.id(), id, "factory id for {id}");
        assert_eq!(provider.name(), name, "factory name for {id}");
        assert_eq!(provider.base_url(), base_url, "factory base_url for {id}");
        assert!(
            !provider.get_models().is_empty(),
            "factory {id} must carry its generated catalog models"
        );
        assert!(
            aggregate_ids.contains(id),
            "factory {id} must be part of builtin_providers()"
        );
    }

    let radius = radius_provider(RadiusProviderOptions::default());
    assert_eq!(radius.id(), "radius");
}

/// Rust-side extension: the hand-maintained organization grouping from
/// `builtin::organization_for_provider`, mirrored here so any drift between
/// the table and the constructed providers fails this test. Providers that
/// stand alone (e.g. `anthropic`) report `None` by convention.
#[test]
fn builtin_provider_organization_ids() {
    const EXPECTED: &[(&str, Option<&str>)] = &[
        ("amazon-bedrock", None),
        ("ant-ling", None),
        ("anthropic", None),
        ("azure-openai-responses", None),
        ("baseten", None),
        ("cerebras", None),
        ("cloudflare-ai-gateway", Some("cloudflare")),
        ("cloudflare-workers-ai", Some("cloudflare")),
        ("deepseek", None),
        ("fireworks", None),
        ("github-copilot", None),
        ("google", Some("google")),
        ("google-vertex", Some("google")),
        ("groq", None),
        ("huggingface", None),
        ("kimi-coding", Some("moonshot-kimi")),
        ("minimax", Some("minimax")),
        ("minimax-cn", Some("minimax")),
        ("mistral", None),
        ("moonshotai", Some("moonshot")),
        ("moonshotai-cn", Some("moonshot")),
        ("nvidia", None),
        ("openai", Some("openai")),
        ("openai-codex", Some("openai")),
        ("opencode", Some("opencode")),
        ("opencode-go", Some("opencode")),
        ("openrouter", None),
        ("qwen-token-plan", Some("qwen")),
        ("qwen-token-plan-cn", Some("qwen")),
        ("qwen-token-plan-individual", Some("qwen")),
        ("radius", None),
        ("together", None),
        ("vercel-ai-gateway", None),
        ("xai", None),
        ("xiaomi", Some("xiaomi")),
        ("xiaomi-token-plan-ams", Some("xiaomi")),
        ("xiaomi-token-plan-cn", Some("xiaomi")),
        ("xiaomi-token-plan-sgp", Some("xiaomi")),
        ("zai", Some("zai")),
        ("zai-coding-cn", Some("zai")),
    ];

    let actual: Vec<(String, Option<String>)> =
        pi_core::ai::providers::builtin::builtin_providers()
            .iter()
            .map(|provider| {
                (
                    provider.id().to_string(),
                    provider.organization_id().map(str::to_string),
                )
            })
            .collect();
    let expected: Vec<(String, Option<String>)> = EXPECTED
        .iter()
        .map(|(id, org)| (id.to_string(), org.map(str::to_string)))
        .collect();

    assert_eq!(actual, expected);
}
