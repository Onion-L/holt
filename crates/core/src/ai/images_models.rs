//! Port of `pi-core/ai/src/images-models.ts`: the runtime collection of
//! image-generation providers plus auth application and generation
//! convenience — the image-side counterpart of [`crate::ai::models::Models`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared};

use crate::ai::auth::context::default_provider_auth_context;
use crate::ai::auth::credential_store::InMemoryCredentialStore;
use crate::ai::auth::resolve::{
    AuthResolutionOverrides, ModelsError, ModelsErrorCode, ResolveError, resolve_provider_auth,
};
use crate::ai::auth::types::{AuthContext, AuthResult, CredentialStore, ProviderAuth};
use crate::ai::models::CreateModelsOptions;
use crate::ai::types::{
    AssistantImages, ImagesContext, ImagesModel, ImagesOptions, ImagesStopReason, ProviderEnv,
    ProviderHeaders,
};

/// Port of `ProviderImages`: the image API adapter surface (every image API
/// module exports exactly `generateImages`).
pub trait ProviderImages: Send + Sync {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: Option<&'a ImagesOptions>,
    ) -> BoxFuture<'a, AssistantImages>;
}

/// Dynamic image model refresh: fetches the current list. May fail; the
/// stored list then stays at its last-known state and a later call retries.
pub type RefreshImagesModelsFn =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<ImagesModel>, String>> + Send + Sync>;

/// Port of `ImagesProvider`: an image-generation provider owning id/name
/// metadata, auth, model listing, and generation behavior.
pub trait ImagesProvider: Send + Sync {
    fn id(&self) -> &str;

    fn name(&self) -> &str;

    /// Required: at least one of `apiKey`/`oauth`.
    fn auth(&self) -> &ProviderAuth;

    /// Current known models, sync. The TypeScript contract lets ill-behaved
    /// providers throw (treated as no models); Rust providers express that
    /// by returning an empty list.
    fn get_models(&self) -> Vec<ImagesModel>;

    /// Dynamic providers only: fetch and update the model list. Concurrent
    /// calls share one in-flight fetch. May fail with a `ModelsError`
    /// ("model_source") on fetch failure.
    fn refresh_models(&self) -> BoxFuture<'static, Result<(), ModelsError>> {
        Box::pin(async { Ok(()) })
    }

    fn has_refresh_models(&self) -> bool {
        false
    }

    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: Option<&'a ImagesOptions>,
    ) -> BoxFuture<'a, AssistantImages>;
}

/// Port of `CreateImagesProviderOptions`.
pub struct CreateImagesProviderOptions {
    pub id: String,
    /// Display name. Default: `id`.
    pub name: Option<String>,
    /// Required — every provider has auth semantics, even ambient/keyless
    /// ones.
    pub auth: ProviderAuth,
    /// Initial model list (empty for purely dynamic providers).
    pub models: Vec<ImagesModel>,
    /// Dynamic providers: fetch the current list. Stored on success;
    /// concurrent calls share one in-flight fetch.
    pub refresh_models: Option<RefreshImagesModelsFn>,
    pub api: Arc<dyn ProviderImages>,
}

type SharedRefresh = Shared<BoxFuture<'static, Result<(), Arc<ModelsError>>>>;

struct ImagesProviderState {
    models: Mutex<Vec<ImagesModel>>,
    inflight: Mutex<Option<SharedRefresh>>,
}

/// Port of `createImagesModels`.
pub fn create_images_models(options: CreateModelsOptions) -> ImagesModels {
    ImagesModels::new(options)
}

/// Builds an image-generation provider from parts. Port of
/// `createImagesProvider`.
pub fn create_images_provider(input: CreateImagesProviderOptions) -> Arc<dyn ImagesProvider> {
    let name = input.name.unwrap_or_else(|| input.id.clone());
    Arc::new(FactoryImagesProvider {
        id: input.id,
        name,
        auth: input.auth,
        state: Arc::new(ImagesProviderState {
            models: Mutex::new(input.models),
            inflight: Mutex::new(None),
        }),
        refresh_models: input.refresh_models,
        api: input.api,
    })
}

struct FactoryImagesProvider {
    id: String,
    name: String,
    auth: ProviderAuth,
    state: Arc<ImagesProviderState>,
    refresh_models: Option<RefreshImagesModelsFn>,
    api: Arc<dyn ProviderImages>,
}

impl ImagesProvider for FactoryImagesProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<ImagesModel> {
        self.state.models.lock().unwrap().clone()
    }

    fn refresh_models(&self) -> BoxFuture<'static, Result<(), ModelsError>> {
        let Some(refresh) = self.refresh_models.clone() else {
            return Box::pin(async { Ok(()) });
        };
        let state = Arc::clone(&self.state);
        let id = self.id.clone();
        Box::pin(async move {
            // Share one in-flight fetch across concurrent callers; a fresh
            // call after completion fetches again.
            let shared = {
                let mut inflight = state.inflight.lock().unwrap();
                if let Some(existing) = inflight.as_ref() {
                    existing.clone()
                } else {
                    let run = {
                        let state = Arc::clone(&state);
                        let refresh = Arc::clone(&refresh);
                        let id = id.clone();
                        async move {
                            let result = match refresh().await {
                                Ok(models) => {
                                    *state.models.lock().unwrap() = models;
                                    Ok(())
                                }
                                Err(error) => Err(Arc::new(ModelsError::with_cause(
                                    ModelsErrorCode::ModelSource,
                                    format!("Model refresh failed for {id}"),
                                    &error,
                                ))),
                            };
                            *state.inflight.lock().unwrap() = None;
                            result
                        }
                    };
                    let shared: SharedRefresh = run.boxed().shared();
                    *inflight = Some(shared.clone());
                    shared
                }
            };
            match shared.await {
                Ok(()) => Ok(()),
                Err(error) => Err((*error).clone()),
            }
        })
    }

    fn has_refresh_models(&self) -> bool {
        self.refresh_models.is_some()
    }

    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: Option<&'a ImagesOptions>,
    ) -> BoxFuture<'a, AssistantImages> {
        self.api.generate_images(model, context, options)
    }
}

/// Port of `ImagesModelsImpl` (exposed as `MutableImagesModels`).
pub struct ImagesModels {
    providers: Mutex<BTreeMap<String, Arc<dyn ImagesProvider>>>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
}

impl ImagesModels {
    /// Port of `createImagesModels`.
    pub fn new(options: CreateModelsOptions) -> Self {
        Self {
            providers: Mutex::new(BTreeMap::new()),
            credentials: options
                .credentials
                .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
            auth_context: options
                .auth_context
                .unwrap_or_else(default_provider_auth_context),
        }
    }

    /// Port of `MutableImagesModels.setProvider`: upsert/replace by provider
    /// id. Provider ids are unique.
    pub fn set_provider(&self, provider: Arc<dyn ImagesProvider>) {
        self.providers
            .lock()
            .unwrap()
            .insert(provider.id().to_string(), provider);
    }

    pub fn delete_provider(&self, id: &str) {
        self.providers.lock().unwrap().remove(id);
    }

    pub fn clear_providers(&self) {
        self.providers.lock().unwrap().clear();
    }

    pub fn get_providers(&self) -> Vec<Arc<dyn ImagesProvider>> {
        self.providers.lock().unwrap().values().cloned().collect()
    }

    pub fn get_provider(&self, id: &str) -> Option<Arc<dyn ImagesProvider>> {
        self.providers.lock().unwrap().get(id).cloned()
    }

    /// Sync read of last-known models from one provider or all providers.
    /// Best-effort: providers report no models rather than failing.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<ImagesModel> {
        match provider {
            Some(provider) => self
                .get_provider(provider)
                .map(|entry| entry.get_models())
                .unwrap_or_default(),
            None => self
                .get_providers()
                .into_iter()
                .flat_map(|entry| entry.get_models())
                .collect(),
        }
    }

    /// Sync runtime model lookup against last-known lists.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<ImagesModel> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|model| model.id == id)
    }

    /// Ask dynamic providers to re-fetch their model lists. With a provider
    /// id, fails with `ModelsError` ("model_source") on that provider's
    /// fetch failure; without one, refreshes all providers concurrently
    /// best-effort and never fails. Static providers are no-ops.
    pub async fn refresh(&self, provider: Option<&str>) -> Result<(), ModelsError> {
        match provider {
            Some(provider) => {
                let Some(entry) = self
                    .get_provider(provider)
                    .filter(|entry| entry.has_refresh_models())
                else {
                    return Ok(());
                };
                entry.refresh_models().await
            }
            None => {
                let refreshes = self
                    .get_providers()
                    .into_iter()
                    .filter(|entry| entry.has_refresh_models())
                    .map(|entry| entry.refresh_models())
                    .collect::<Vec<_>>();
                for result in futures::future::join_all(refreshes).await {
                    let _ = result;
                }
                Ok(())
            }
        }
    }

    /// Resolve request auth by provider id. Same contract as
    /// `Models.getAuth()`: `None` when unknown/unconfigured, errors on real
    /// failures.
    pub async fn get_auth(
        &self,
        provider_id: &str,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, ResolveError> {
        let Some(provider) = self.get_provider(provider_id) else {
            return Ok(None);
        };
        resolve_provider_auth(
            provider_id,
            provider.auth(),
            self.credentials.as_ref(),
            Arc::clone(&self.auth_context),
            overrides,
        )
        .await
    }

    /// `getAuth` by image model (resolves through the owning provider).
    pub async fn get_auth_for_model(
        &self,
        model: &ImagesModel,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, ResolveError> {
        self.get_auth(&model.provider, overrides).await
    }

    /// Generate images through the owning provider with auth resolved and
    /// merged (explicit options win per field). Never fails; failures are
    /// returned as an `AssistantImages` with `stopReason: "error"`.
    pub async fn generate_images(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: Option<&ImagesOptions>,
    ) -> AssistantImages {
        match self.generate_images_inner(model, context, options).await {
            Ok(result) => result,
            Err(error_message) => AssistantImages {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                output: Vec::new(),
                response_id: None,
                usage: None,
                stop_reason: ImagesStopReason::Error,
                error_message: Some(error_message),
                timestamp: crate::ai::auth::resolve::now_millis(),
            },
        }
    }

    async fn generate_images_inner(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: Option<&ImagesOptions>,
    ) -> Result<AssistantImages, String> {
        let provider = self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {}", model.provider),
            )
            .message
        })?;

        let resolution = self
            .get_auth(
                &model.provider,
                Some(&AuthResolutionOverrides {
                    api_key: options.and_then(|options| options.api_key.clone()),
                    env: options.and_then(|options| options.env.clone()),
                    signal: options.and_then(|options| options.signal.clone()),
                    min_oauth_validity_ms: None,
                }),
            )
            .await
            .map_err(|error| error.to_string())?;

        let Some(resolution) = resolution else {
            // Unconfigured: dispatch unchanged; the provider decides what to
            // do.
            return Ok(provider.generate_images(model, context, options).await);
        };
        let auth = resolution.auth;

        let mut request_model = model.clone();
        if let Some(base_url) = &auth.base_url {
            request_model.base_url = base_url.clone();
        }

        // Explicit request options win per-field; headers/env merge per key.
        let mut merged = options.cloned().unwrap_or_default();
        merged.api_key = options
            .and_then(|options| options.api_key.clone())
            .or(auth.api_key);
        merged.headers = merge_auth_headers(auth.headers.as_ref(), options);
        merged.env = merge_auth_env(resolution.env.as_ref(), options);
        Ok(provider
            .generate_images(&request_model, context, Some(&merged))
            .await)
    }
}

/// Port of `{...auth.headers, ...options.headers}` — `None` when both sides
/// are absent.
fn merge_auth_headers(
    auth_headers: Option<&ProviderHeaders>,
    options: Option<&ImagesOptions>,
) -> Option<ProviderHeaders> {
    let options_headers = options.and_then(|options| options.headers.as_ref());
    if auth_headers.is_none() && options_headers.is_none() {
        return None;
    }
    let mut merged = ProviderHeaders::new();
    if let Some(auth_headers) = auth_headers {
        merged.extend(auth_headers.clone());
    }
    if let Some(options_headers) = options_headers {
        merged.extend(options_headers.clone());
    }
    Some(merged)
}

/// Port of `{...(resolution.env ?? {}), ...(options.env ?? {})}` — `None`
/// when both sides are absent.
fn merge_auth_env(
    env: Option<&ProviderEnv>,
    options: Option<&ImagesOptions>,
) -> Option<ProviderEnv> {
    let options_env = options.and_then(|options| options.env.as_ref());
    if env.is_none() && options_env.is_none() {
        return None;
    }
    let mut merged = env.cloned().unwrap_or_default();
    if let Some(options_env) = options_env {
        merged.extend(options_env.clone());
    }
    Some(merged)
}
