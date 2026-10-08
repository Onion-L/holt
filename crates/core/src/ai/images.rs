//! Port of `pi-core/ai/src/images.ts`, `images-api-registry.ts`, and
//! `providers/images/register-builtins.ts`: image API dispatch and the
//! image-API provider registry.
//!
//! TypeScript lazy-loads API adapter modules and keeps a runtime registry so
//! extensions can register or override image APIs. Rust links the built-in
//! adapter at compile time, but the registry itself is real: entries are
//! mutable global state with TS's replace, lookup, `sourceId`, and error
//! semantics, and the built-in OpenRouter adapter is registered on first
//! access (the TS module-load side effect).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::ai::api::openrouter_images;
use crate::ai::types::{AssistantImages, ImagesContext, ImagesModel, ImagesOptions};

/// Port of the `ImagesApiFunction` type: an async image generation call for
/// one `api`. The error half mirrors the TypeScript thrown errors
/// (`Mismatched api`, adapter failures surfaced as `stopReason: "error"`).
/// Inputs are taken by value so adapters can hold them across await points
/// without self-referential borrows; [`generate_images`] clones the call
/// inputs into the adapter.
pub type ImagesApiFunction = Arc<
    dyn Fn(
            ImagesModel,
            ImagesContext,
            Option<ImagesOptions>,
        ) -> futures::future::BoxFuture<'static, Result<AssistantImages, String>>
        + Send
        + Sync,
>;

/// Port of the `ImagesApiProvider` interface.
#[derive(Clone)]
pub struct ImagesApiProvider {
    pub api: String,
    pub generate_images: ImagesApiFunction,
}

struct RegisteredImagesApiProvider {
    provider: ImagesApiProvider,
    /// Kept for parity with the TS registry record; `getImagesApiProvider`
    /// does not expose it.
    #[allow(dead_code)]
    source_id: Option<String>,
}

/// Port of `wrapGenerateImages`: rejects a call whose model `api` does not
/// match the registered adapter (the TS wrapper throws before calling).
fn wrap_generate_images(api: &str, generate_images: ImagesApiFunction) -> ImagesApiFunction {
    let api = api.to_string();
    Arc::new(move |model, context, options| {
        if model.api != api {
            return Box::pin(std::future::ready(Err(format!(
                "Mismatched api: {} expected {api}",
                model.api
            ))));
        }
        generate_images(model, context, options)
    })
}

fn openrouter_images_api_function() -> ImagesApiFunction {
    Arc::new(|model, context, options| {
        Box::pin(async move {
            Ok(openrouter_images::generate_images(&model, &context, options.as_ref()).await)
        })
    })
}

fn registry() -> &'static Mutex<HashMap<String, RegisteredImagesApiProvider>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, RegisteredImagesApiProvider>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut map = HashMap::new();
        register_built_in_images_api_providers_locked(&mut map);
        Mutex::new(map)
    })
}

fn register_built_in_images_api_providers_locked(
    map: &mut HashMap<String, RegisteredImagesApiProvider>,
) {
    map.insert(
        "openrouter-images".to_string(),
        RegisteredImagesApiProvider {
            provider: ImagesApiProvider {
                api: "openrouter-images".to_string(),
                generate_images: wrap_generate_images(
                    "openrouter-images",
                    openrouter_images_api_function(),
                ),
            },
            source_id: None,
        },
    );
}

/// Port of `registerBuiltInImagesApiProviders` in
/// `providers/images/register-builtins.ts`.
pub fn register_built_in_images_api_providers() {
    let mut map = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    register_built_in_images_api_providers_locked(&mut map);
}

/// Port of `registerImagesApiProvider`: wraps the adapter with the api
/// mismatch check and stores it, replacing any existing entry for the api
/// (including the built-in).
pub fn register_images_api_provider(provider: ImagesApiProvider, source_id: Option<&str>) {
    let mut map = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.insert(
        provider.api.clone(),
        RegisteredImagesApiProvider {
            provider: ImagesApiProvider {
                api: provider.api.clone(),
                generate_images: wrap_generate_images(&provider.api, provider.generate_images),
            },
            source_id: source_id.map(str::to_string),
        },
    );
}

/// Port of `getImagesApiProvider`.
pub fn get_images_api_provider(api: &str) -> Option<ImagesApiProvider> {
    let map = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.get(api).map(|registered| registered.provider.clone())
}

/// Port of `generateImages` from `images.ts`. Rejects (returns `Err`) when no
/// API provider is registered for the model's api, matching the TypeScript
/// async throw.
pub async fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: Option<&ImagesOptions>,
) -> Result<AssistantImages, String> {
    let provider = get_images_api_provider(&model.api)
        .ok_or_else(|| format!("No API provider registered for api: {}", model.api))?;
    (provider.generate_images)(model.clone(), context.clone(), options.cloned()).await
}
