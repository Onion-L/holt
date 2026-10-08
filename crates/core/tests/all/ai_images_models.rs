//! Port of `pi-core/ai/test/images-models.test.ts` (the `builtinImagesModels`
//! case lands with the image provider factories) plus the offline dispatch
//! behavior of `images.ts`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_core::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, AuthContext, AuthFuture, AuthResult, AuthStorageError, ModelAuth,
    ProviderAuth,
};
use pi_core::ai::images::generate_images as dispatch_generate_images;
use pi_core::ai::images_models::{
    CreateImagesProviderOptions, ProviderImages, create_images_models, create_images_provider,
};
use pi_core::ai::models::CreateModelsOptions;
use pi_core::ai::types::{
    AssistantImages, BlockContent, ImageContent, ImagesContext, ImagesModel, ImagesOptions,
    ImagesStopReason, ModelCost, ModelInput,
};

struct FakeAuthContext {
    env: BTreeMap<String, String>,
}

impl AuthContext for FakeAuthContext {
    fn env(&self, name: &str) -> AuthFuture<Option<String>> {
        Box::pin(std::future::ready(self.env.get(name).cloned()))
    }

    fn file_exists(&self, _path: &str) -> AuthFuture<bool> {
        Box::pin(std::future::ready(false))
    }
}

fn fake_auth_context(env: &[(&str, &str)]) -> Arc<dyn AuthContext> {
    Arc::new(FakeAuthContext {
        env: env
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
    })
}

fn test_image_model(provider: &str, id: &str) -> ImagesModel {
    ImagesModel {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-images".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.test/v1".to_string(),
        input: vec![ModelInput::Text],
        output: vec![ModelInput::Image],
        cost: ModelCost::default(),
        headers: None,
        extra: Default::default(),
    }
}

fn ok_result(model: &ImagesModel) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: vec![BlockContent::Image(ImageContent {
            content_type: Default::default(),
            data: "aGk=".to_string(),
            mime_type: "image/png".to_string(),
        })],
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: pi_core::ai::auth::resolve::now_millis(),
    }
}

/// Captures `(model, effective options)` per generate call.
struct RecordingImagesApi {
    calls: Arc<Mutex<Vec<(ImagesModel, ImagesOptions)>>>,
}

impl ProviderImages for RecordingImagesApi {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        _context: &'a ImagesContext,
        options: Option<&'a ImagesOptions>,
    ) -> BoxFuture<'a, AssistantImages> {
        self.calls
            .lock()
            .unwrap()
            .push((model.clone(), options.cloned().unwrap_or_default()));
        let model = model.clone();
        Box::pin(std::future::ready(ok_result(&model)))
    }
}

/// Port of the test provider's `apiKey` auth: stored credential key first,
/// then the provider env var; `{ auth: {} }` when the provider has no env
/// var; `None` (unconfigured) when the env var is unset.
struct TestKeyAuth {
    env_var: Option<String>,
}

impl ApiKeyAuth for TestKeyAuth {
    fn name(&self) -> &str {
        "Test key"
    }

    fn resolve(
        &self,
        input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        let Some(env_var) = self.env_var.clone() else {
            return Box::pin(std::future::ready(Ok(Some(AuthResult::default()))));
        };
        Box::pin(async move {
            if let Some(key) = input.credential.as_ref().and_then(|c| c.key.clone()) {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key),
                        ..Default::default()
                    },
                    env: None,
                    source: Some("stored".to_string()),
                }));
            }
            let key = input.ctx.env(&env_var).await;
            Ok(key.map(|key| AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    ..Default::default()
                },
                env: None,
                source: Some(env_var),
            }))
        })
    }
}

struct TestProviderInput {
    id: &'static str,
    models: Vec<ImagesModel>,
    env_var: Option<String>,
    calls: Arc<Mutex<Vec<(ImagesModel, ImagesOptions)>>>,
}

fn test_provider(input: TestProviderInput) -> Arc<dyn pi_core::ai::images_models::ImagesProvider> {
    create_images_provider(CreateImagesProviderOptions {
        id: input.id.to_string(),
        name: None,
        auth: ProviderAuth::api_key(Arc::new(TestKeyAuth {
            env_var: input.env_var,
        })),
        models: if input.models.is_empty() {
            vec![test_image_model(input.id, "model-a")]
        } else {
            input.models
        },
        refresh_models: None,
        api: Arc::new(RecordingImagesApi { calls: input.calls }),
    })
}

fn context() -> ImagesContext {
    ImagesContext {
        input: vec![BlockContent::Text(pi_core::ai::types::TextContent {
            text: "a red circle".to_string(),
            ..Default::default()
        })],
    }
}

#[tokio::test]
async fn registers_providers_and_reads_models_synchronously() {
    let models = create_images_models(CreateModelsOptions::default());
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: vec![test_image_model("p1", "m1"), test_image_model("p1", "m2")],
        env_var: None,
        calls: Arc::new(Mutex::new(Vec::new())),
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p2",
        models: vec![test_image_model("p2", "m3")],
        env_var: None,
        calls: Arc::new(Mutex::new(Vec::new())),
    }));

    assert_eq!(
        models
            .get_providers()
            .iter()
            .map(|provider| provider.id().to_string())
            .collect::<Vec<_>>(),
        vec!["p1", "p2"]
    );
    assert_eq!(
        models
            .get_models(None)
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m1", "m2", "m3"]
    );
    assert_eq!(
        models
            .get_models(Some("p1"))
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m1", "m2"]
    );
    assert_eq!(
        models.get_model("p2", "m3").map(|m| m.id),
        Some("m3".into())
    );
    assert!(models.get_model("p2", "missing").is_none());

    models.delete_provider("p1");
    assert!(models.get_provider("p1").is_none());
}

#[tokio::test]
async fn resolves_auth_and_merges_it_into_requests_explicit_options_win() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let models = create_images_models(CreateModelsOptions {
        auth_context: Some(fake_auth_context(&[("TEST_KEY", "env-key")])),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: Vec::new(),
        env_var: Some("TEST_KEY".to_string()),
        calls: Arc::clone(&calls),
    }));
    let model = models.get_model("p1", "model-a").unwrap();

    let api_key = |result: &Option<AuthResult>| {
        result
            .as_ref()
            .and_then(|result| result.auth.api_key.clone())
    };
    assert_eq!(
        models
            .get_auth_for_model(&model, None)
            .await
            .unwrap()
            .as_ref()
            .and_then(|r| r.auth.api_key.clone()),
        Some("env-key".to_string())
    );
    assert_eq!(
        api_key(&models.get_auth("p1", None).await.unwrap()),
        Some("env-key".to_string())
    );
    assert_eq!(
        api_key(
            &models
                .get_auth(
                    "p1",
                    Some(&pi_core::ai::auth::resolve::AuthResolutionOverrides {
                        api_key: Some("explicit-key".to_string()),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap()
        ),
        Some("explicit-key".to_string())
    );

    let result = models.generate_images(&model, &context(), None).await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(calls.lock().unwrap()[0].1.api_key, Some("env-key".into()));

    models
        .generate_images(
            &model,
            &context(),
            Some(&ImagesOptions {
                api_key: Some("explicit".to_string()),
                ..Default::default()
            }),
        )
        .await;
    assert_eq!(calls.lock().unwrap()[1].1.api_key, Some("explicit".into()));
}

#[tokio::test]
async fn merges_provider_resolved_env_into_image_options() {
    let calls = Arc::new(Mutex::new(Vec::new()));

    /// Auth that resolves with a provider-scoped env overlay.
    struct EnvOverlayAuth;

    impl ApiKeyAuth for EnvOverlayAuth {
        fn name(&self) -> &str {
            "Test key"
        }

        fn resolve(
            &self,
            _input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
            Box::pin(std::future::ready(Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("provider-key".to_string()),
                    ..Default::default()
                },
                env: Some(
                    [
                        ("PROVIDER_ONLY".to_string(), "provider".to_string()),
                        ("SHARED".to_string(), "provider".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                source: None,
            }))))
        }
    }

    let models = create_images_models(CreateModelsOptions::default());
    models.set_provider(create_images_provider(CreateImagesProviderOptions {
        id: "p1".to_string(),
        name: None,
        auth: ProviderAuth::api_key(Arc::new(EnvOverlayAuth)),
        models: vec![test_image_model("p1", "model-a")],
        refresh_models: None,
        api: Arc::new(RecordingImagesApi {
            calls: Arc::clone(&calls),
        }),
    }));
    let model = models.get_model("p1", "model-a").unwrap();

    models
        .generate_images(
            &model,
            &context(),
            Some(&ImagesOptions {
                api_key: Some("request-key".to_string()),
                env: Some(
                    [
                        ("REQUEST_ONLY".to_string(), "request".to_string()),
                        ("SHARED".to_string(), "request".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }),
        )
        .await;

    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].1.api_key, Some("request-key".into()));
    let expected_env: BTreeMap<String, String> = [
        ("PROVIDER_ONLY".to_string(), "provider".to_string()),
        ("REQUEST_ONLY".to_string(), "request".to_string()),
        ("SHARED".to_string(), "request".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(calls[0].1.env, Some(expected_env));
}

#[tokio::test]
async fn returns_an_error_result_for_unknown_providers_and_dispatches_unconfigured_auth() {
    let models = create_images_models(CreateModelsOptions {
        auth_context: Some(fake_auth_context(&[])),
        ..Default::default()
    });
    let ghost = models
        .generate_images(&test_image_model("ghost", "m"), &context(), None)
        .await;
    assert_eq!(ghost.stop_reason, ImagesStopReason::Error);
    assert!(
        ghost
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("Unknown provider: ghost"))
    );

    // Unconfigured (resolve -> None) still dispatches; the provider decides
    // what to do.
    let calls = Arc::new(Mutex::new(Vec::new()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: Vec::new(),
        env_var: Some("MISSING".to_string()),
        calls: Arc::clone(&calls),
    }));
    let model = models.get_model("p1", "model-a").unwrap();
    assert!(
        models
            .get_auth_for_model(&model, None)
            .await
            .unwrap()
            .is_none()
    );
    models.generate_images(&model, &context(), None).await;
    assert_eq!(calls.lock().unwrap()[0].1.api_key, None);
}

#[tokio::test]
async fn supports_dynamic_providers_via_refresh_with_in_flight_dedupe() {
    let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let refresh = {
        let fetches = Arc::clone(&fetches);
        Arc::new(move || {
            let fetches = Arc::clone(&fetches);
            Box::pin(async move {
                fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Ok(vec![test_image_model("dyn", "listed")])
            }) as BoxFuture<'static, Result<Vec<ImagesModel>, String>>
        })
    };
    let provider = create_images_provider(CreateImagesProviderOptions {
        id: "dyn".to_string(),
        name: None,
        auth: ProviderAuth::api_key(Arc::new(TestKeyAuth { env_var: None })),
        models: Vec::new(),
        refresh_models: Some(Arc::clone(&refresh) as _),
        api: Arc::new(RecordingImagesApi {
            calls: Arc::new(Mutex::new(Vec::new())),
        }),
    });
    let models = create_images_models(CreateModelsOptions::default());
    models.set_provider(provider);

    assert!(models.get_models(Some("dyn")).is_empty());
    let (first, second) = tokio::join!(models.refresh(Some("dyn")), models.refresh(Some("dyn")));
    first.unwrap();
    second.unwrap();
    assert_eq!(
        fetches.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "concurrent refreshes share one in-flight fetch"
    );
    assert!(models.get_model("dyn", "listed").is_some());

    // Failures reject with ModelsError ("model_source") for a single provider.
    let flaky = create_images_provider(CreateImagesProviderOptions {
        id: "flaky".to_string(),
        name: None,
        auth: ProviderAuth::api_key(Arc::new(TestKeyAuth { env_var: None })),
        models: Vec::new(),
        refresh_models: Some(Arc::new(move || {
            Box::pin(async { Err("fetch failed".to_string()) })
                as BoxFuture<'static, Result<Vec<ImagesModel>, String>>
        })),
        api: Arc::new(RecordingImagesApi {
            calls: Arc::new(Mutex::new(Vec::new())),
        }),
    });
    models.set_provider(flaky);
    let error = models.refresh(Some("flaky")).await.unwrap_err();
    assert_eq!(
        error.code,
        pi_core::ai::auth::resolve::ModelsErrorCode::ModelSource
    );
    assert_eq!(
        error.message,
        "Model refresh failed for flaky: fetch failed"
    );
    models.refresh(None).await.unwrap();
}

#[tokio::test]
async fn images_dispatch_rejects_unknown_apis() {
    // Port of the `resolveImagesApiProvider` throw surfaced by `images.ts`.
    let model = test_image_model("openrouter", "google/gemini-2.5-flash-image");
    let error = dispatch_generate_images(&model, &context(), None)
        .await
        .unwrap_err();
    assert_eq!(error, "No API provider registered for api: test-images");
}
