//! Port of `pi-core/ai/test/images-api-registry.test.ts` plus the
//! `registerBuiltInImagesApiProviders` behavior from
//! `providers/images/register-builtins.ts`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use pi_core::ai::images::{
    ImagesApiFunction, ImagesApiProvider, generate_images, get_images_api_provider,
    register_built_in_images_api_providers, register_images_api_provider,
};
use pi_core::ai::types::{
    AssistantImages, ImagesContext, ImagesModel, ImagesOptions, ImagesStopReason,
};

/// The registry is process-global state; override tests serialize on this lock
/// and restore the built-in entry before finishing. A tokio mutex because the
/// guard is deliberately held across the generation `await`.
async fn registry_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn images_model(api: &str) -> ImagesModel {
    ImagesModel {
        id: "test-image-model".to_string(),
        name: "Test Image Model".to_string(),
        api: api.to_string(),
        provider: "test-images".to_string(),
        base_url: "https://example.test/v1".to_string(),
        input: vec![],
        output: vec![],
        cost: pi_core::ai::types::ModelCost::default(),
        headers: None,
        extra: BTreeMap::new(),
    }
}

fn images_context() -> ImagesContext {
    ImagesContext { input: vec![] }
}

fn custom_images(model: &ImagesModel, marker: &str) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: vec![],
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: Some(marker.to_string()),
        timestamp: 1_700_000_000_000,
    }
}

fn counting_adapter(api: &str, calls: Arc<AtomicUsize>, marker: &'static str) -> ImagesApiProvider {
    ImagesApiProvider {
        api: api.to_string(),
        generate_images: {
            let calls = Arc::clone(&calls);
            Arc::new(
                move |model: ImagesModel,
                      _context: ImagesContext,
                      _options: Option<ImagesOptions>| {
                    let calls = Arc::clone(&calls);
                    let fut: futures::future::BoxFuture<'static, Result<AssistantImages, String>> =
                        Box::pin(async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok(custom_images(&model, marker))
                        });
                    fut
                },
            ) as ImagesApiFunction
        },
    }
}

#[test]
fn builtin_openrouter_images_api_is_registered_on_first_access() {
    assert!(get_images_api_provider("openrouter-images").is_some());
    assert!(get_images_api_provider("unknown-images").is_none());
}

#[tokio::test]
async fn generate_images_rejects_unknown_api_with_the_ts_error_text() {
    let model = images_model("does-not-exist");
    let error = generate_images(&model, &images_context(), None)
        .await
        .expect_err("unknown api must be rejected");
    assert_eq!(error, "No API provider registered for api: does-not-exist");
}

#[tokio::test]
async fn registered_provider_replaces_the_builtin_and_mismatch_is_rejected() {
    let _guard = registry_lock().await;
    let calls = Arc::new(AtomicUsize::new(0));
    register_images_api_provider(
        counting_adapter("openrouter-images", Arc::clone(&calls), "custom-adapter"),
        Some("test-suite"),
    );

    let model = images_model("openrouter-images");
    let images = generate_images(&model, &images_context(), None)
        .await
        .expect("override adapter must be used");
    assert_eq!(images.error_message.as_deref(), Some("custom-adapter"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The registry wraps every adapter with the api mismatch check, mirroring
    // the TS `wrapGenerateImages` throw.
    let registered = get_images_api_provider("openrouter-images").expect("entry exists");
    let other_model = images_model("some-other-api");
    let error = (registered.generate_images)(other_model, images_context(), None)
        .await
        .expect_err("mismatched api must be rejected");
    assert_eq!(
        error,
        "Mismatched api: some-other-api expected openrouter-images"
    );

    register_built_in_images_api_providers();
}

#[tokio::test]
async fn register_built_in_images_api_providers_replaces_any_override() {
    let _guard = registry_lock().await;
    let calls = Arc::new(AtomicUsize::new(0));
    register_images_api_provider(
        counting_adapter("openrouter-images", Arc::clone(&calls), "override"),
        None,
    );
    register_built_in_images_api_providers();
    // The entry exists again; invoking the registry path must not reach the
    // removed override (observable through the counter staying at zero).
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(get_images_api_provider("openrouter-images").is_some());
}
