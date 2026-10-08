//! Port of `pi-core/ai/src/models.ts`: the provider runtime unit, the
//! `Models` collection, and `createProvider`.
//!
//! TypeScript `Provider` objects are structural interfaces; the Rust port
//! exposes a [`Provider`] trait for concrete providers and
//! [`BasicProvider`]/[`create_provider`] for assembling one from parts, the
//! same surface `createProvider` offers. Lazy module loading (`lazyApi`,
//! `lazyStream`) collapses into the stream-based dispatch: setup runs inside
//! the returned stream and failures surface as error events, matching the
//! TypeScript observable behavior (see [`lazy_stream`]).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::ai::auth::context::default_provider_auth_context;
use crate::ai::auth::credential_store::InMemoryCredentialStore;
use crate::ai::auth::resolve::{
    AuthResolutionOverrides, ModelsError, ModelsErrorCode, ResolveError, resolve_provider_auth,
};
use crate::ai::auth::types::{
    AuthContext, AuthOperationOptions, AuthResult, AuthStorageError, Credential, CredentialStore,
};
use crate::ai::models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreError,
};
use crate::ai::types::{
    Api, AssistantMessage, Context, DeferredHandle, Model, ModelCostRates, ModelThinkingLevel,
    ProviderRequestOptions, SimpleStreamOptions, StreamOptions, Usage,
};
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

/// Port of `ProviderStreams`: the uniform stream contract of an API
/// implementation module.
pub trait ProviderStreams: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;

    fn fetch_deferred(
        &self,
        _model: &Model,
        _handle: &DeferredHandle,
        _options: Option<&crate::ai::types::DeferredFetchOptions>,
    ) -> Option<AssistantMessageEventStream> {
        None
    }

    fn cancel_deferred(
        &self,
        _model: &Model,
        _handle: &DeferredHandle,
        _options: Option<&ProviderRequestOptions>,
    ) -> Option<BoxFuture<'static, Result<(), ModelsError>>> {
        None
    }

    /// Capability probe standing in for the TypeScript
    /// `entry.fetchDeferred !== undefined` check in `createProvider`.
    fn supports_deferred(&self) -> bool {
        false
    }

    /// Capability probe standing in for the TypeScript
    /// `entry.cancelDeferred !== undefined` check in `createProvider`.
    fn supports_cancel_deferred(&self) -> bool {
        false
    }
}

/// Port of `RefreshModelsContext`.
pub struct RefreshModelsContext {
    /// Effective configured credential. OAuth credentials are refreshed
    /// before network access.
    pub credential: Option<Credential>,
    /// Immutable provider-scoped catalog snapshot from before this phase.
    pub stored: Option<ModelsStoreEntry>,
    /// Generation-checked publication. Persistence policy remains
    /// provider-owned.
    pub publish: Arc<
        dyn Fn(ModelsPublication) -> BoxFuture<'static, Result<bool, ModelsStoreError>>
            + Send
            + Sync,
    >,
    /// False during offline/cache-only initialization.
    pub allow_network: bool,
    /// Bypass provider freshness checks and fetch immediately.
    pub force: Option<bool>,
    /// Always present, including when the public refresh caller omits its
    /// optional signal.
    pub signal: CancellationToken,
}

/// Port of `ModelsPublication`.
#[derive(Default)]
pub struct ModelsPublication {
    /// Provider-selected persisted catalog. `None` leaves storage unchanged;
    /// `Some(None)` deletes it.
    pub persist: Option<Option<ModelsStoreEntry>>,
    /// Optional synchronous update of provider-private in-memory catalog
    /// state.
    pub update: Option<Box<dyn FnOnce() + Send>>,
}

/// Port of `ModelsRefreshOptions`.
#[derive(Clone, Default)]
pub struct ModelsRefreshOptions {
    pub allow_network: Option<bool>,
    /// Restrict refresh to these provider IDs. Unknown and static providers
    /// are ignored.
    pub providers: Option<Vec<String>>,
    pub force: Option<bool>,
    pub signal: Option<CancellationToken>,
}

/// Port of `ModelsRefreshResult`.
#[derive(Debug)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    pub errors: BTreeMap<String, ModelsError>,
}

/// Port of `Provider`: the concrete runtime unit owning id/name/base
/// metadata, auth methods, model listing, and stream behavior.
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;

    fn name(&self) -> &str;

    /// Rust-side extension: upstream TypeScript `Provider` has no
    /// organization concept. Groups providers operated by the same
    /// organization (for example `minimax` and `minimax-cn` both report
    /// `"minimax"`). `None` means the provider stands alone; consumers can
    /// fall back to [`Provider::id`] for a grouping key.
    fn organization_id(&self) -> Option<&str> {
        None
    }

    fn base_url(&self) -> Option<&str> {
        None
    }

    fn headers(&self) -> Option<&crate::ai::types::ProviderHeaders> {
        None
    }

    /// Required: at least one of `apiKey`/`oauth`.
    fn auth(&self) -> &crate::ai::auth::types::ProviderAuth;

    /// Current known models, sync. Must not fail; providers return their
    /// catalog (static) or the last refreshed list (dynamic).
    fn get_models(&self) -> Vec<Model>;

    /// Dynamic providers only: restore `context.stored` and optionally fetch
    /// a newer list.
    fn refresh_models<'a>(
        &'a self,
        _context: RefreshModelsContext,
    ) -> BoxFuture<'a, Result<(), ModelsError>> {
        Box::pin(async { Ok(()) })
    }

    fn has_refresh_models(&self) -> bool {
        false
    }

    /// Optional provider policy for credential-specific model availability.
    fn filter_models(&self, models: Vec<Model>, _credential: Option<&Credential>) -> Vec<Model> {
        models
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&crate::ai::types::DeferredFetchOptions>,
    ) -> Option<AssistantMessageEventStream> {
        let _ = (model, handle, options);
        None
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&ProviderRequestOptions>,
    ) -> Option<BoxFuture<'static, Result<(), ModelsError>>> {
        let _ = (model, handle, options);
        None
    }

    /// Capability probe standing in for the TypeScript
    /// `provider.fetchDeferred !== undefined` check in `Models.fetchDeferred`.
    fn supports_deferred(&self) -> bool {
        false
    }

    /// Capability probe standing in for the TypeScript
    /// `provider.cancelDeferred !== undefined` check in
    /// `Models.cancelDeferred`.
    fn supports_cancel_deferred(&self) -> bool {
        false
    }
}

/// Port of `CreateModelsOptions`.
#[derive(Default)]
pub struct CreateModelsOptions {
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub models_store: Option<Arc<dyn ModelsStore>>,
    pub auth_context: Option<Arc<dyn AuthContext>>,
}

struct ProviderRefreshState {
    generation: u64,
    controller: CancellationToken,
}

/// Port of `ModelsDeferredFetchOptions`:
/// `DeferredFetchOptions & ModelsRequestTransforms`.
#[derive(Clone, Default)]
pub struct ModelsDeferredFetchOptions {
    pub base: crate::ai::types::ProviderRequestOptions,
    pub wait: Option<u64>,
    pub transform_headers: Option<crate::ai::types::TransformHeadersFn>,
}

/// Port of `ModelsDeferredCancelOptions`:
/// `DeferredCancelOptions & ModelsRequestTransforms`.
#[derive(Clone, Default)]
pub struct ModelsDeferredCancelOptions {
    pub base: crate::ai::types::ProviderRequestOptions,
    pub transform_headers: Option<crate::ai::types::TransformHeadersFn>,
}

/// Port of `ModelsImpl`: runtime collection of providers plus auth
/// application and stream convenience.
pub struct Models {
    providers: Mutex<BTreeMap<String, Arc<dyn Provider>>>,
    credentials: Arc<dyn CredentialStore>,
    models_store: Arc<dyn ModelsStore>,
    auth_context: Arc<dyn AuthContext>,
    refresh_states: Arc<Mutex<HashMap<String, ProviderRefreshState>>>,
}

impl Default for Models {
    fn default() -> Self {
        Self::new(CreateModelsOptions::default())
    }
}

impl Models {
    /// Port of `createModels`.
    pub fn new(options: CreateModelsOptions) -> Self {
        Self {
            providers: Mutex::new(BTreeMap::new()),
            credentials: options
                .credentials
                .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
            models_store: options
                .models_store
                .unwrap_or_else(|| Arc::new(InMemoryModelsStore::new())),
            auth_context: options
                .auth_context
                .unwrap_or_else(default_provider_auth_context),
            refresh_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Port of `MutableModels.setProvider`: upsert/replace by provider id.
    pub fn set_provider(&self, provider: Arc<dyn Provider>) {
        self.supersede_provider_refresh(provider.id());
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.id().to_string(), provider);
    }

    /// Port of `MutableModels.deleteProvider`.
    pub fn delete_provider(&self, id: &str) {
        self.supersede_provider_refresh(id);
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// Port of `MutableModels.clearProviders`.
    pub fn clear_providers(&self) {
        let mut providers = self
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<String> = providers.keys().cloned().collect();
        for id in ids {
            self.supersede_provider_refresh(&id);
        }
        providers.clear();
    }

    /// Port of `getProviders`.
    pub fn get_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Port of `getProvider`.
    pub fn get_provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Port of `getModels`: sync read of last-known models from one provider
    /// or all providers. Best-effort: a provider whose `getModels()` throws
    /// (panics) yields no models.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let providers = self
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let provider_models = |entry: &Arc<dyn Provider>| {
            // Port of the per-provider try/catch: ill-behaved providers yield
            // no models instead of failing the whole listing.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry.get_models()))
                .unwrap_or_default()
        };
        if let Some(provider) = provider {
            return providers
                .get(provider)
                .map(provider_models)
                .unwrap_or_default();
        }
        let mut models = Vec::new();
        for entry in providers.values() {
            models.extend(provider_models(entry));
        }
        models
    }

    /// Port of `getModel`: sync runtime model lookup.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|model| model.id == id)
    }

    fn supersede_provider_refresh(&self, provider_id: &str) -> u64 {
        let mut states = self
            .refresh_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = states.get(provider_id).map_or(0, |state| state.generation) + 1;
        let previous = states.remove(provider_id);
        if let Some(previous) = previous {
            previous.controller.cancel();
        }
        states.insert(
            provider_id.to_string(),
            ProviderRefreshState {
                generation,
                controller: CancellationToken::new(),
            },
        );
        generation
    }

    /// Port of `refresh`: refreshes selected configured dynamic providers
    /// concurrently. Provider errors and cancellation are returned without
    /// failing; static, unknown, and unconfigured providers are skipped.
    pub async fn refresh(&self, options: ModelsRefreshOptions) -> ModelsRefreshResult {
        let allow_network = options.allow_network.unwrap_or(true);
        let caller_signal = options.signal.unwrap_or_default();
        let errors: BTreeMap<String, ModelsError> = BTreeMap::new();
        if caller_signal.is_cancelled() {
            return ModelsRefreshResult {
                aborted: true,
                errors,
            };
        }
        let selected = options.providers.clone();

        let refreshable: Vec<Arc<dyn Provider>> = self
            .get_providers()
            .into_iter()
            .filter(|provider| provider.has_refresh_models())
            .filter(|provider| {
                selected
                    .as_ref()
                    .is_none_or(|selected| selected.contains(&provider.id().to_string()))
            })
            .collect();

        let errors: Mutex<BTreeMap<String, ModelsError>> = Mutex::new(BTreeMap::new());
        let mut operations = Vec::with_capacity(refreshable.len());
        for provider in refreshable {
            let generation = self.supersede_provider_refresh(provider.id());
            let controller = self
                .refresh_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(provider.id())
                .map(|state| state.controller.clone())
                .unwrap_or_default();
            let signal = {
                let combined = CancellationToken::new();
                {
                    let combined = combined.clone();
                    let caller = caller_signal.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            () = caller.cancelled() => combined.cancel(),
                            () = combined.cancelled() => {}
                        }
                    });
                }
                {
                    let combined = combined.clone();
                    let controller = controller.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            () = controller.cancelled() => combined.cancel(),
                            () = combined.cancelled() => {}
                        }
                    });
                }
                combined
            };

            let credentials = Arc::clone(&self.credentials);
            let provider_id = provider.id().to_string();
            let provider_for_phase = Arc::clone(&provider);
            let force = options.force;
            let this = self;
            let errors_for_task = &errors;

            operations.push(async move {
                let operation = async {
                    let stored_credential = match credentials
                        .read(
                            &provider_id,
                            Some(&crate::ai::auth::types::AuthOperationOptions {
                                signal: Some(signal.clone()),
                            }),
                        )
                        .await
                    {
                        Ok(credential) => credential,
                        Err(error) => {
                            let error = ModelsError::with_cause(
                                ModelsErrorCode::Auth,
                                format!("Credential store read failed for {provider_id}"),
                                &error,
                            );
                            // Restore cached provider state before surfacing the
                            // read failure, mirroring the TypeScript ordering.
                            run_provider_refresh_phase(
                                provider_for_phase.as_ref(),
                                RefreshPhase {
                                    models: this,
                                    provider_id: &provider_id,
                                    generation,
                                    credential: None,
                                    allow_network: false,
                                    force: None,
                                    signal: &signal,
                                },
                            )
                            .await?;
                            return Err(error);
                        }
                    };

                    // Restore cached provider state before auth resolution or
                    // network access.
                    run_provider_refresh_phase(
                        provider_for_phase.as_ref(),
                        RefreshPhase {
                            models: this,
                            provider_id: &provider_id,
                            generation,
                            credential: stored_credential.clone(),
                            allow_network: false,
                            force: None,
                            signal: &signal,
                        },
                    )
                    .await?;
                    if !allow_network || signal.is_cancelled() {
                        return Ok(());
                    }

                    let credential = resolve_refresh_credential(
                        provider_for_phase.as_ref(),
                        credentials.as_ref(),
                        &provider_id,
                        stored_credential,
                        &signal,
                    )
                    .await?;
                    let Some(credential) = credential else {
                        return Ok(());
                    };
                    run_provider_refresh_phase(
                        provider_for_phase.as_ref(),
                        RefreshPhase {
                            models: this,
                            provider_id: &provider_id,
                            generation,
                            credential: Some(credential),
                            allow_network: true,
                            force,
                            signal: &signal,
                        },
                    )
                    .await
                };

                // Port of `raceWithAbortSignal`: a superseding refresh (or the
                // caller's signal) cuts the in-flight operation short; the
                // aborted result carries no error.
                tokio::select! {
                    () = signal.cancelled() => {}
                    result = operation => match result {
                        Ok(()) => {}
                        Err(error) => {
                            if !signal.is_cancelled() {
                                errors_for_task
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(provider_id, error);
                            }
                        }
                    },
                }
            });
        }

        futures::future::join_all(operations).await;

        ModelsRefreshResult {
            aborted: caller_signal.is_cancelled(),
            errors: errors
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    /// Port of `checkAuth`: sync-style auth check without refreshing OAuth.
    pub async fn check_auth(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<crate::ai::auth::types::AuthCheck>, ResolveError> {
        let signal = options
            .and_then(|options| options.signal.clone())
            .unwrap_or_default();
        if signal.is_cancelled() {
            return Err(ResolveError::Aborted);
        }
        let check = async {
            let Some(provider) = self.get_provider(provider_id) else {
                return Ok(None);
            };
            let credential = self.read_credential(provider_id, &signal).await?;
            self.check_provider_auth(provider.as_ref(), credential.as_ref(), &signal)
                .await
        };
        // Port of the `raceWithAbortSignal` wrapper around the check.
        tokio::select! {
            () = signal.cancelled() => Err(ResolveError::Aborted),
            result = check => result,
        }
    }

    /// Port of `getAvailable`: models whose providers have complete auth.
    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<Model>, ResolveError> {
        let signal = options
            .and_then(|options| options.signal.clone())
            .unwrap_or_default();
        if signal.is_cancelled() {
            return Err(ResolveError::Aborted);
        }
        let available = async {
            let providers: Vec<Arc<dyn Provider>> = match provider_id {
                Some(provider_id) => self.get_provider(provider_id).into_iter().collect(),
                None => self.get_providers(),
            };
            let mut available = Vec::new();
            for provider in providers {
                let credential = self.read_credential(provider.id(), &signal).await?;
                let auth = self
                    .check_provider_auth(provider.as_ref(), credential.as_ref(), &signal)
                    .await?;
                if auth.is_none() {
                    continue;
                }
                available
                    .extend(provider.filter_models(provider.get_models(), credential.as_ref()));
            }
            Ok(available)
        };
        // Port of the `raceWithAbortSignal` wrapper around the collection.
        tokio::select! {
            () = signal.cancelled() => Err(ResolveError::Aborted),
            result = available => result,
        }
    }

    /// Port of `getAuth` for a provider id or a model (model headers merge).
    pub async fn get_auth(
        &self,
        provider_id: &str,
        model: Option<&Model>,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, ResolveError> {
        let Some(provider) = self.get_provider(provider_id) else {
            return Ok(None);
        };
        let result = resolve_provider_auth(
            provider_id,
            provider.auth(),
            self.credentials.as_ref(),
            Arc::clone(&self.auth_context),
            overrides,
        )
        .await?;
        let Some(result) = result else {
            return Ok(None);
        };
        let Some(model) = model else {
            return Ok(Some(result));
        };
        let Some(model_headers) = model.headers.as_ref() else {
            return Ok(Some(result));
        };
        let mut merged = result.clone();
        merged.auth.headers = merge_headers(
            merged.auth.headers,
            Some(
                &model_headers
                    .iter()
                    .map(|(name, value)| (name.clone(), Some(value.clone())))
                    .collect(),
            ),
        );
        Ok(Some(merged))
    }

    /// Port of `login`: runs a provider-owned login flow and persists the
    /// returned credential.
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: crate::ai::auth::types::AuthType,
        interaction: Arc<dyn crate::ai::auth::types::AuthInteraction>,
    ) -> Result<Credential, ModelsError> {
        let Some(provider) = self.get_provider(provider_id) else {
            return Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {provider_id}"),
            ));
        };
        let auth = provider.auth();
        let credential: Result<Credential, AuthStorageError> = match auth_type {
            crate::ai::auth::types::AuthType::OAuth => match auth.oauth.as_ref() {
                Some(oauth) => oauth
                    .login(Arc::clone(&interaction))
                    .await
                    .map(Credential::OAuth),
                None => {
                    return Err(ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("{} does not support oauth login", provider.name()),
                    ));
                }
            },
            crate::ai::auth::types::AuthType::ApiKey => match auth.api_key.as_ref() {
                Some(api_key) => match api_key.login(Arc::clone(&interaction)) {
                    Some(login) => login.await.map(Credential::ApiKey),
                    None => {
                        return Err(ModelsError::new(
                            ModelsErrorCode::Auth,
                            format!("{} does not support api_key login", provider.name()),
                        ));
                    }
                },
                None => {
                    return Err(ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("{} does not support api_key login", provider.name()),
                    ));
                }
            },
        };
        let credential = credential.map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("Credential store modify failed for {provider_id}"),
                &error,
            )
        })?;
        let stored = credential.clone();
        let write = self.credentials.modify(
            provider_id,
            Box::new(move |_| {
                let stored = stored.clone();
                Box::pin(async move { Ok(Some(stored)) })
                    as crate::ai::auth::types::AuthFuture<
                        Result<Option<Credential>, crate::ai::auth::types::BoxedAuthError>,
                    >
            }),
            None,
        );
        match write.await {
            Ok(_) => Ok(credential),
            Err(error) => Err(ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("Credential store modify failed for {provider_id}"),
                &error,
            )),
        }
    }

    /// Port of `logout`.
    pub async fn logout(
        &self,
        provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<(), ModelsError> {
        self.credentials
            .delete(provider_id, None)
            .await
            .map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("Credential store delete failed for {provider_id}"),
                    &error,
                )
            })
    }

    /// Port of `stream`: resolves auth and dispatches lazily inside the
    /// returned stream.
    pub fn stream(
        self: &Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        lazy_stream(model, {
            let this = Arc::clone(self);
            let model = model.clone();
            let context = context.clone();
            move || {
                let this = Arc::clone(&this);
                let model = model.clone();
                let context = context.clone();
                async move {
                    let provider = this.require_provider(&model)?;
                    let (request_model, request_options) = this.apply_auth(&model, options).await?;
                    Ok(provider.stream(&request_model, &context, Some(&request_options)))
                        as Result<AssistantMessageEventStream, ModelsError>
                }
            }
        })
    }

    /// Port of `complete`.
    pub async fn complete(
        self: &Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessage {
        self.stream(model, context, options).result().await
    }

    /// Port of `streamSimple`.
    pub fn stream_simple(
        self: &Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        lazy_stream(model, {
            let this = Arc::clone(self);
            let model = model.clone();
            let context = context.clone();
            move || {
                let this = Arc::clone(&this);
                let model = model.clone();
                let context = context.clone();
                async move {
                    let provider = this.require_provider(&model)?;
                    let (request_model, request_options) =
                        this.apply_auth_simple(&model, options).await?;
                    Ok(provider.stream_simple(&request_model, &context, Some(&request_options)))
                        as Result<AssistantMessageEventStream, ModelsError>
                }
            }
        })
    }

    /// Port of `completeSimple`.
    pub async fn complete_simple(
        self: &Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessage {
        self.stream_simple(model, context, options).result().await
    }

    /// Port of `Models.fetchDeferred` (options:
    /// [`ModelsDeferredFetchOptions`]).
    pub async fn fetch_deferred(
        self: &Arc<Self>,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<ModelsDeferredFetchOptions>,
    ) -> AssistantMessage {
        lazy_stream(model, move || {
            let models = Arc::clone(self);
            let model = model.clone();
            let handle = handle.clone();
            let options = options.clone();
            async move {
                let provider = models.require_provider(&model)?;
                if !provider.supports_deferred() {
                    return Err(ModelsError::new(
                        ModelsErrorCode::Provider,
                        format!(
                            "Provider {} does not support deferred responses",
                            model.provider
                        ),
                    ));
                }
                let lifted = options
                    .as_ref()
                    .map(|options| crate::ai::types::StreamOptions {
                        base: options.base.clone(),
                        transform_headers: options.transform_headers.clone(),
                        ..Default::default()
                    });
                let (request_model, request_options) =
                    models.apply_auth_inner(&model, lifted.as_ref()).await?;
                let deferred_options = crate::ai::types::DeferredFetchOptions {
                    base: request_options.base,
                    wait: options.as_ref().and_then(|options| options.wait),
                };
                Ok(provider
                    .fetch_deferred(&request_model, &handle, Some(&deferred_options))
                    .unwrap_or_else(|| {
                        lazy_stream(&request_model, || {
                            std::future::ready(Err(ModelsError::new(
                                ModelsErrorCode::Provider,
                                format!(
                                    "Provider {} does not support deferred responses",
                                    model.provider
                                ),
                            )))
                        })
                    }))
            }
        })
        .result()
        .await
    }

    /// Port of `Models.cancelDeferred` (options:
    /// [`ModelsDeferredCancelOptions`]).
    pub async fn cancel_deferred(
        self: &Arc<Self>,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<ModelsDeferredCancelOptions>,
    ) -> Result<(), ModelsError> {
        let provider = self.require_provider(model)?;
        if !provider.supports_cancel_deferred() {
            return Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} does not support deferred responses",
                    model.provider
                ),
            ));
        }
        let lifted = options.as_ref().map(|options| StreamOptions {
            base: options.base.clone(),
            transform_headers: options.transform_headers.clone(),
            ..Default::default()
        });
        let (request_model, request_options) =
            self.apply_auth_inner(model, lifted.as_ref()).await?;
        match provider.cancel_deferred(&request_model, handle, Some(&request_options.base)) {
            Some(future) => future.await,
            None => Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} does not support deferred responses",
                    model.provider
                ),
            )),
        }
    }

    fn require_provider(&self, model: &Model) -> Result<Arc<dyn Provider>, ModelsError> {
        self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {}", model.provider),
            )
        })
    }

    async fn read_credential(
        &self,
        provider_id: &str,
        signal: &CancellationToken,
    ) -> Result<Option<Credential>, ResolveError> {
        if signal.is_cancelled() {
            return Err(ResolveError::Aborted);
        }
        self.credentials
            .read(
                provider_id,
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
            .map_err(|error| {
                ResolveError::Models(ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("Credential store read failed for {provider_id}"),
                    &error,
                ))
            })
    }

    async fn check_provider_auth(
        &self,
        provider: &dyn Provider,
        credential: Option<&Credential>,
        signal: &CancellationToken,
    ) -> Result<Option<crate::ai::auth::types::AuthCheck>, ResolveError> {
        let auth = provider.auth();
        if let Some(Credential::OAuth(_)) = credential {
            return Ok(auth
                .oauth
                .as_ref()
                .map(|_| crate::ai::auth::types::AuthCheck {
                    source: Some("OAuth".to_string()),
                    auth_type: crate::ai::auth::types::AuthType::OAuth,
                }));
        }
        let Some(api_key_auth) = auth.api_key.as_ref() else {
            return Ok(None);
        };
        if let Some(check) = api_key_auth.check(crate::ai::auth::types::ApiKeyAuthInput {
            ctx: Arc::clone(&self.auth_context),
            credential: match credential {
                Some(Credential::ApiKey(api_key)) => Some(api_key.clone()),
                _ => None,
            },
            signal: signal.clone(),
        }) {
            return check.await.map_err(|error| {
                ResolveError::Models(ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("API key auth check failed for provider {}", provider.id()),
                    &error,
                ))
            });
        }
        let resolution = resolve_provider_auth(
            provider.id(),
            auth,
            self.credentials.as_ref(),
            Arc::clone(&self.auth_context),
            Some(&AuthResolutionOverrides {
                signal: Some(signal.clone()),
                ..Default::default()
            }),
        )
        .await?;
        Ok(resolution.map(|result| crate::ai::auth::types::AuthCheck {
            source: result.source,
            auth_type: crate::ai::auth::types::AuthType::ApiKey,
        }))
    }

    async fn apply_auth(
        self: &Arc<Self>,
        model: &Model,
        options: Option<StreamOptions>,
    ) -> Result<(Model, StreamOptions), ModelsError> {
        self.apply_auth_inner(model, options.as_ref()).await
    }

    async fn apply_auth_simple(
        self: &Arc<Self>,
        model: &Model,
        options: Option<SimpleStreamOptions>,
    ) -> Result<(Model, SimpleStreamOptions), ModelsError> {
        let base_ref = options.as_ref().map(|options| &options.base);
        let (request_model, resolved_base) = self.apply_auth_inner(model, base_ref).await?;
        Ok((
            request_model,
            SimpleStreamOptions {
                base: resolved_base,
                ..options.unwrap_or_default()
            },
        ))
    }

    /// Applies resolved auth to the request model and options, mirroring the
    /// TypeScript `applyAuth`: explicit request options win per-field and the
    /// resolved `auth.baseUrl` replaces the model base URL.
    async fn apply_auth_inner(
        self: &Arc<Self>,
        model: &Model,
        options: Option<&StreamOptions>,
    ) -> Result<(Model, StreamOptions), ModelsError> {
        self.require_provider(model)?;
        let overrides = AuthResolutionOverrides {
            api_key: options.and_then(|options| options.base.api_key.clone()),
            env: options.and_then(|options| options.base.env.clone()),
            signal: options.and_then(|options| options.base.signal.clone()),
            min_oauth_validity_ms: None,
        };
        let resolution = self
            .get_auth(&model.provider, Some(model), Some(&overrides))
            .await
            .map_err(resolve_error_to_models_error)?
            .ok_or_else(|| {
                ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Provider is not configured: {}", model.provider),
                )
            })?;
        let auth = resolution.auth;

        // Explicit request options win per-field.
        let api_key = options
            .and_then(|options| options.base.api_key.clone())
            .or(auth.api_key);
        let mut headers = merge_headers(
            auth.headers,
            options.and_then(|options| options.base.headers.as_ref()),
        );
        // The Models-only transform runs last.
        if let Some(transform) = options.and_then(|options| options.transform_headers.as_ref()) {
            headers = Some(transform(headers.unwrap_or_default()).await);
        }
        let env = match (
            resolution.env,
            options.and_then(|options| options.base.env.clone()),
        ) {
            (None, None) => None,
            (resolution_env, request_env) => Some(
                resolution_env
                    .unwrap_or_default()
                    .into_iter()
                    .chain(request_env.unwrap_or_default())
                    .collect(),
            ),
        };

        let mut request_model = model.clone();
        if let Some(base_url) = auth.base_url {
            request_model.base_url = base_url;
        }
        let mut resolved = StreamOptions {
            base: ProviderRequestOptions {
                api_key,
                headers,
                env,
                ..Default::default()
            },
            ..Default::default()
        };

        // Preserve the caller's non-auth option fields. `transformHeaders` is
        // a Models-only option and is stripped before provider dispatch.
        if let Some(options) = options {
            resolved.temperature = options.temperature;
            resolved.sampling_params = options.sampling_params.clone();
            resolved.max_tokens = options.max_tokens;
            resolved.transport = options.transport;
            resolved.cache_retention = options.cache_retention;
            resolved.session_id = options.session_id.clone();
            resolved.websocket_connect_timeout_ms = options.websocket_connect_timeout_ms;
            resolved.metadata = options.metadata.clone();
            resolved.base.signal = options.base.signal.clone();
            resolved.base.telemetry_context = options.base.telemetry_context.clone();
            resolved.base.fetch = options.base.fetch.clone();
            resolved.base.on_payload = options.base.on_payload.clone();
            resolved.base.on_response = options.base.on_response.clone();
            resolved.base.timeout_ms = options.base.timeout_ms;
            resolved.base.max_retries = options.base.max_retries;
            resolved.base.max_retry_delay_ms = options.base.max_retry_delay_ms;
        }
        Ok((request_model, resolved))
    }
}

/// Converts auth resolution failures into stream-error models errors,
/// matching how request paths surface rejections.
fn resolve_error_to_models_error(error: ResolveError) -> ModelsError {
    match error {
        ResolveError::Models(models_error) => models_error,
        ResolveError::Storage(message) => ModelsError::new(ModelsErrorCode::Auth, message),
        ResolveError::Aborted => {
            ModelsError::new(ModelsErrorCode::Auth, "The operation was aborted")
        }
    }
}

fn merge_headers(
    base: Option<crate::ai::types::ProviderHeaders>,
    override_headers: Option<&crate::ai::types::ProviderHeaders>,
) -> Option<crate::ai::types::ProviderHeaders> {
    if base.is_none() && override_headers.is_none() {
        return None;
    }
    let mut merged = base.unwrap_or_default();
    if let Some(override_headers) = override_headers {
        for (name, value) in override_headers {
            let lower_name = name.to_lowercase();
            let existing: Vec<String> = merged
                .keys()
                .filter(|key| key.to_lowercase() == lower_name)
                .cloned()
                .collect();
            for existing_name in existing {
                merged.remove(&existing_name);
            }
            merged.insert(name.clone(), value.clone());
        }
    }
    Some(merged)
}

/// Resolves the refresh credential for a dynamic provider refresh phase.
async fn resolve_refresh_credential(
    provider: &dyn Provider,
    credentials: &dyn CredentialStore,
    provider_id: &str,
    stored: Option<Credential>,
    signal: &CancellationToken,
) -> Result<Option<Credential>, ModelsError> {
    if let Some(Credential::OAuth(oauth_credential)) = &stored {
        let Some(oauth) = provider.auth().oauth.clone() else {
            return Ok(None);
        };
        if crate::ai::auth::resolve::now_millis() < oauth_credential.expires {
            return Ok(Some(Credential::OAuth(oauth_credential.clone())));
        }
        if signal.is_cancelled() {
            return Ok(None);
        }
        let provider_id_owned = provider_id.to_string();
        let signal_for_modify = signal.clone();
        let post = credentials
            .modify(
                provider_id,
                Box::new(move |current| {
                    let oauth = Arc::clone(&oauth);
                    let provider_id_owned = provider_id_owned.clone();
                    let signal_for_modify = signal_for_modify.clone();
                    Box::pin(async move {
                        let Some(Credential::OAuth(current)) = current else {
                            return Ok(None);
                        };
                        if crate::ai::auth::resolve::now_millis() < current.expires {
                            return Ok(None);
                        }
                        match oauth.refresh(&current, signal_for_modify).await {
                            Ok(refreshed) => Ok(Some(Credential::OAuth(refreshed))),
                            Err(error) => Err(ModelsError::with_cause(
                                ModelsErrorCode::OAuth,
                                format!("OAuth refresh failed for {provider_id_owned}"),
                                &error,
                            )
                            .into_boxed()),
                        }
                    })
                        as crate::ai::auth::types::AuthFuture<
                            Result<Option<Credential>, crate::ai::auth::types::BoxedAuthError>,
                        >
                }),
                Some(&crate::ai::auth::types::AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
            .map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id}"),
                    &error,
                )
            })?;
        return Ok(match post {
            Some(Credential::OAuth(refreshed)) => Some(Credential::OAuth(refreshed)),
            _ => None,
        });
    }

    let Some(api_key_auth) = provider.auth().api_key.as_ref() else {
        return Ok(None);
    };
    let api_key_credential = match &stored {
        Some(Credential::ApiKey(api_key)) => Some(api_key.clone()),
        _ => None,
    };
    let result = api_key_auth
        .resolve(crate::ai::auth::types::ApiKeyAuthInput {
            ctx: default_provider_auth_context(),
            credential: api_key_credential,
            signal: signal.clone(),
        })
        .await
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("API key auth failed for provider {provider_id}"),
                &error,
            )
        })?;
    Ok(result.map(|result| {
        Credential::ApiKey(crate::ai::auth::types::ApiKeyCredential {
            key: result.auth.api_key,
            env: result.env,
        })
    }))
}

/// Parameters for a provider refresh phase (kept together to avoid an
/// unwieldy argument list).
struct RefreshPhase<'a> {
    models: &'a Models,
    provider_id: &'a str,
    generation: u64,
    credential: Option<Credential>,
    allow_network: bool,
    force: Option<bool>,
    signal: &'a CancellationToken,
}

async fn run_provider_refresh_phase(
    provider: &dyn Provider,
    phase: RefreshPhase<'_>,
) -> Result<(), ModelsError> {
    let RefreshPhase {
        models,
        provider_id,
        generation,
        credential,
        allow_network,
        force,
        signal,
    } = phase;
    let stored = models
        .models_store
        .read(
            provider_id,
            Some(&crate::ai::models_store::ModelsStoreOperationOptions {
                signal: Some(signal.clone()),
            }),
        )
        .await
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                format!("Model store read failed for {provider_id}"),
                &error,
            )
        })?;
    let store = Arc::clone(&models.models_store);
    let provider_id_owned = provider_id.to_string();
    let signal_for_publish = signal.clone();
    let refresh_states = Arc::clone(&models.refresh_states);
    let publish = move |publication: ModelsPublication| {
        let store = Arc::clone(&store);
        let provider_id = provider_id_owned.clone();
        let signal = signal_for_publish.clone();
        let refresh_states = Arc::clone(&refresh_states);
        Box::pin(async move {
            // Generation-checked publication: superseded refreshes neither
            // persist nor apply their in-memory update.
            let is_current = || {
                !signal.is_cancelled()
                    && refresh_states
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&provider_id)
                        .is_some_and(|state| state.generation == generation)
            };
            if !is_current() {
                return Ok(false);
            }
            // Port of `{ signal }`: model-store waits are bound to the
            // provider refresh signal.
            let options = crate::ai::models_store::ModelsStoreOperationOptions {
                signal: Some(signal.clone()),
            };
            if let Some(persist) = publication.persist {
                match persist {
                    Some(entry) => {
                        store.write(&provider_id, entry, Some(&options)).await?;
                    }
                    None => {
                        store.delete(&provider_id, Some(&options)).await?;
                    }
                }
            }
            if !is_current() {
                return Ok(false);
            }
            if let Some(update) = publication.update {
                update();
            }
            Ok(true)
        }) as BoxFuture<'static, Result<bool, ModelsStoreError>>
    };
    let context = RefreshModelsContext {
        credential,
        stored,
        publish: Arc::new(publish),
        allow_network,
        force: if allow_network { force } else { None },
        signal: signal.clone(),
    };
    provider.refresh_models(context).await
}

/// Type alias for the dynamic model overlay fetcher.
pub type FetchModelsFn = Arc<
    dyn Fn(RefreshModelsContext) -> BoxFuture<'static, Result<Vec<Model>, ModelsError>>
        + Send
        + Sync,
>;

/// Type alias for credential-specific model availability filters.
pub type FilterModelsFn = Arc<dyn Fn(Vec<Model>, Option<&Credential>) -> Vec<Model> + Send + Sync>;

/// Port of `CreateProviderOptions`.
pub struct CreateProviderOptions {
    pub id: String,
    /// Display name; defaults to `id`.
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub headers: Option<crate::ai::types::ProviderHeaders>,
    /// Required — every provider has auth semantics.
    pub auth: crate::ai::auth::types::ProviderAuth,
    /// Static baseline model list (empty for purely dynamic providers).
    pub models: Vec<Model>,
    /// Fetch a dynamic model overlay.
    pub fetch_models: Option<FetchModelsFn>,
    pub filter_models: Option<FilterModelsFn>,
    /// One implementation for all models, or an api-keyed map that dispatches
    /// on `model.api`.
    pub api: ProviderApi,
    /// Rust-side extension, see [`Provider::organization_id`].
    pub organization_id: Option<String>,
}

/// The `api` field of [`CreateProviderOptions`]: a single implementation or
/// an api-keyed map. Port of the `ProviderStreams | Record<Api, ProviderStreams>`
/// union from `createProvider`.
pub enum ProviderApi {
    Single(Arc<dyn ProviderStreams>),
    ByApi(std::collections::BTreeMap<String, Arc<dyn ProviderStreams>>),
}

impl From<Arc<dyn ProviderStreams>> for ProviderApi {
    fn from(api: Arc<dyn ProviderStreams>) -> Self {
        ProviderApi::Single(api)
    }
}

impl ProviderApi {
    fn for_model(&self, model: &Model) -> Option<Arc<dyn ProviderStreams>> {
        match self {
            ProviderApi::Single(api) => Some(Arc::clone(api)),
            ProviderApi::ByApi(by_api) => by_api.get(model.api.as_str()).cloned(),
        }
    }

    fn entries(&self) -> Vec<Arc<dyn ProviderStreams>> {
        match self {
            ProviderApi::Single(api) => vec![Arc::clone(api)],
            ProviderApi::ByApi(by_api) => by_api.values().cloned().collect(),
        }
    }
}

/// Port of `createProvider`: builds a provider from parts.
pub fn create_provider(input: CreateProviderOptions) -> Arc<dyn Provider> {
    Arc::new(BasicProvider::new(input))
}

/// The concrete provider built by [`create_provider`].
pub struct BasicProvider {
    id: String,
    name: String,
    base_url: Option<String>,
    headers: Option<crate::ai::types::ProviderHeaders>,
    auth: crate::ai::auth::types::ProviderAuth,
    baseline_models: Vec<Model>,
    dynamic_models: Arc<Mutex<Vec<Model>>>,
    fetch_models: Option<FetchModelsFn>,
    filter_models: Option<FilterModelsFn>,
    api: ProviderApi,
    organization_id: Option<String>,
}

impl BasicProvider {
    pub fn new(input: CreateProviderOptions) -> Self {
        Self {
            name: input.name.unwrap_or_else(|| input.id.clone()),
            id: input.id,
            base_url: input.base_url,
            headers: input.headers,
            auth: input.auth,
            baseline_models: input.models,
            dynamic_models: Arc::new(Mutex::new(Vec::new())),
            fetch_models: input.fetch_models,
            filter_models: input.filter_models,
            api: input.api,
            organization_id: input.organization_id,
        }
    }

    fn current_models(&self) -> Vec<Model> {
        let mut merged = self.baseline_models.clone();
        let dynamic = self
            .dynamic_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for model in dynamic.iter() {
            match merged.iter().position(|entry| entry.id == model.id) {
                Some(index) => merged[index] = model.clone(),
                None => merged.push(model.clone()),
            }
        }
        merged
    }

    /// Port of `dispatch`: an api map entry missing for the model's api
    /// terminates the stream with a `ModelsError` ("stream").
    fn dispatch(
        &self,
        model: &Model,
        run: impl FnOnce(&dyn ProviderStreams) -> AssistantMessageEventStream,
    ) -> AssistantMessageEventStream {
        match self.api.for_model(model) {
            Some(streams) => run(streams.as_ref()),
            None => lazy_stream(model, || {
                std::future::ready(Err(ModelsError::new(
                    ModelsErrorCode::Stream,
                    format!(
                        "Provider {} has no API implementation for \"{}\"",
                        self.id, model.api
                    ),
                )))
            }),
        }
    }
}

impl Provider for BasicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn organization_id(&self) -> Option<&str> {
        self.organization_id.as_deref()
    }

    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    fn headers(&self) -> Option<&crate::ai::types::ProviderHeaders> {
        self.headers.as_ref()
    }

    fn auth(&self) -> &crate::ai::auth::types::ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.current_models()
    }

    fn refresh_models<'a>(
        &'a self,
        context: RefreshModelsContext,
    ) -> BoxFuture<'a, Result<(), ModelsError>> {
        let Some(fetch_models) = self.fetch_models.as_ref() else {
            return Box::pin(async { Ok(()) });
        };
        let fetch_models = Arc::clone(fetch_models);
        let provider_id_for_filter = self.id.clone();
        let context_signal = context.signal.clone();
        let publish = Arc::clone(&context.publish);
        let allow_network = context.allow_network;
        let stored = context.stored.clone();
        let _ = &context;
        Box::pin(async move {
            let dynamic_models = Arc::clone(&self.dynamic_models);
            if let Some(stored) = &stored {
                let restored: Vec<Model> = stored
                    .models
                    .iter()
                    .filter(|model| model.provider == provider_id_for_filter)
                    .cloned()
                    .collect();
                let dynamic_models_for_restore = Arc::clone(&dynamic_models);
                let published = (publish)(ModelsPublication {
                    persist: None,
                    update: Some(Box::new(move || {
                        *dynamic_models_for_restore
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = restored;
                    })),
                })
                .await
                .map_err(|error| {
                    ModelsError::with_cause(
                        ModelsErrorCode::ModelSource,
                        format!("Model store write failed for {}", self.id),
                        &error,
                    )
                })?;
                if !published {
                    return Ok(());
                }
            }
            if !allow_network || context_signal.is_cancelled() {
                return Ok(());
            }
            let refreshed = fetch_models(context).await?;
            if context_signal.is_cancelled() {
                return Ok(());
            }
            let checked_at = crate::ai::auth::resolve::now_millis();
            let refreshed_for_update = refreshed.clone();
            (publish)(ModelsPublication {
                persist: Some(Some(ModelsStoreEntry {
                    models: refreshed.clone(),
                    checked_at: Some(checked_at),
                    ..Default::default()
                })),
                update: Some(Box::new(move || {
                    *dynamic_models
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = refreshed_for_update;
                })),
            })
            .await
            .map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::ModelSource,
                    format!("Model store write failed for {}", self.id),
                    &error,
                )
            })?;
            Ok(())
        })
    }

    fn supports_deferred(&self) -> bool {
        self.api
            .entries()
            .iter()
            .any(|entry| entry.supports_deferred())
    }

    fn supports_cancel_deferred(&self) -> bool {
        self.api
            .entries()
            .iter()
            .any(|entry| entry.supports_cancel_deferred())
    }

    fn has_refresh_models(&self) -> bool {
        self.fetch_models.is_some()
    }

    fn filter_models(&self, models: Vec<Model>, credential: Option<&Credential>) -> Vec<Model> {
        match &self.filter_models {
            Some(filter) => filter(models, credential),
            None => models,
        }
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.dispatch(model, |streams| streams.stream(model, context, options))
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.dispatch(model, |streams| {
            streams.stream_simple(model, context, options)
        })
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&crate::ai::types::DeferredFetchOptions>,
    ) -> Option<AssistantMessageEventStream> {
        // Only wired when at least one implementation supports deferred
        // responses, mirroring `createProvider`.
        if !self
            .api
            .entries()
            .iter()
            .any(|entry| entry.supports_deferred())
        {
            return None;
        }
        match self.api.for_model(model) {
            Some(implementation) if implementation.supports_deferred() => {
                implementation.fetch_deferred(model, handle, options)
            }
            _ => Some(lazy_stream(model, || {
                std::future::ready(Err(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!(
                        "Provider {} does not support deferred responses for \"{}\"",
                        self.id, model.api
                    ),
                )))
            })),
        }
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&ProviderRequestOptions>,
    ) -> Option<BoxFuture<'static, Result<(), ModelsError>>> {
        if !self
            .api
            .entries()
            .iter()
            .any(|entry| entry.supports_cancel_deferred())
        {
            return None;
        }
        let implementation = self.api.for_model(model)?;
        if !implementation.supports_cancel_deferred() {
            return Some(Box::pin(std::future::ready(Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} cannot cancel deferred responses for \"{}\"",
                    self.id, model.api
                ),
            )))));
        }
        let model = model.clone();
        let handle = handle.clone();
        let options = options.cloned();
        let provider_id = self.id.clone();
        Some(Box::pin(async move {
            match implementation.cancel_deferred(&model, &handle, options.as_ref()) {
                Some(future) => future.await,
                None => Err(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!(
                        "Provider {provider_id} cannot cancel deferred responses for \"{}\"",
                        model.api
                    ),
                )),
            }
        }))
    }
}

/// Port of `lazyStream`: returns a stream synchronously while running async
/// setup behind it; setup failures terminate the stream with an error event.
pub fn lazy_stream<F, Fut>(model: &Model, setup: F) -> AssistantMessageEventStream
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<AssistantMessageEventStream, ModelsError>> + Send + 'static,
{
    let outer = create_assistant_message_event_stream();
    let producer = outer.clone();
    let fut = setup();
    let model = model.clone();
    tokio::spawn(async move {
        match fut.await {
            Ok(inner) => {
                while let Some(event) = inner.next().await {
                    producer.push(event);
                }
                let result = inner.result().await;
                producer.end(Some(result));
            }
            Err(error) => {
                let message = create_setup_error_message(&model, error.message);
                producer.push(crate::ai::types::AssistantMessageEvent::Error {
                    reason: crate::ai::types::ErrorReason::Error,
                    error: message.clone(),
                });
                producer.end(Some(message));
            }
        }
    });
    outer
}

fn create_setup_error_message(model: &Model, error_message: String) -> AssistantMessage {
    AssistantMessage {
        role: crate::ai::types::RoleAssistant,
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: crate::ai::types::StopReason::Error,
        error_message: Some(error_message),
        timestamp: crate::ai::auth::resolve::now_millis(),
        ..Default::default()
    }
}

/// Port of `calculateCost`: computes usage cost from model rates, applying
/// tier pricing and the 2x Anthropic long-cache-write multiplier.
pub fn calculate_cost(model: &Model, usage: &mut Usage) -> crate::ai::types::UsageCost {
    let input_tokens = (usage.input + usage.cache_read + usage.cache_write) as f64;
    let mut rates: ModelCostRates = model.cost.rates;
    let mut matched_threshold = -1.0f64;
    for tier in model.cost.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above as f64
            && (tier.input_tokens_above as f64) > matched_threshold
        {
            rates = tier.rates;
            matched_threshold = tier.input_tokens_above as f64;
        }
    }

    let rates_input: f64 = rates.input.into();
    let rates_output: f64 = rates.output.into();
    let rates_cache_read: f64 = rates.cache_read.into();
    let rates_cache_write: f64 = rates.cache_write.into();
    let long_write = usage.cache_write_1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;
    let input = (rates_input / 1_000_000.0) * usage.input as f64;
    let output = (rates_output / 1_000_000.0) * usage.output as f64;
    let cache_read = (rates_cache_read / 1_000_000.0) * usage.cache_read as f64;
    let cache_write =
        (rates_cache_write * short_write + rates_input * 2.0 * long_write) / 1_000_000.0;
    usage.cost.input = input.into();
    usage.cost.output = output.into();
    usage.cost.cache_read = cache_read.into();
    usage.cost.cache_write = cache_write.into();
    usage.cost.total = (input + output + cache_read + cache_write).into();
    usage.cost
}

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// Port of `getSupportedThinkingLevels`.
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .into_iter()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level))
                .cloned();
            if mapped == Some(None) {
                return false;
            }
            if matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max) {
                return mapped.is_some();
            }
            true
        })
        .collect()
}

/// Port of `clampThinkingLevel`.
pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available_levels = get_supported_thinking_levels(model);
    if available_levels.contains(&level) {
        return level;
    }

    let requested_index = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level);
    let Some(requested_index) = requested_index else {
        return available_levels
            .first()
            .copied()
            .unwrap_or(ModelThinkingLevel::Off);
    };

    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if available_levels.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available_levels.contains(candidate) {
            return *candidate;
        }
    }
    available_levels
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

/// Port of `modelsAreEqual`.
pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}

/// Port of `hasApi`.
pub fn has_api(model: &Model, api: &Api) -> bool {
    model.api == *api
}
