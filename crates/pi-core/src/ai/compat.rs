//! Port of `pi-core/ai/src/compat/extension-oauth-types.ts` and
//! `pi-core/ai/src/oauth.ts`: the legacy extension OAuth declaration surface.
//!
//! TypeScript exports these as type-only compatibility shapes for the
//! coding-agent extension API. The Rust port mirrors the payload structs;
//! the callback surface (`OAuthLoginCallbacks`) becomes a trait.

use crate::ai::types::ProviderEnv;
use std::future::Future;
use std::pin::Pin;

/// Boxed future returned by extension OAuth callbacks.
pub type BoxedCompatFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
use tokio_util::sync::CancellationToken;

/// Port of `OAuthCredentials` re-export: the extension-shaped OAuth token.
/// The TS shape has an open index signature; unknown fields are preserved in
/// `extra`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExtensionOAuthCredentials {
    pub refresh: String,
    pub access: String,
    pub expires: i64,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Port of the legacy extension `OAuthPrompt`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthPrompt {
    pub message: String,
    pub placeholder: Option<String>,
    pub allow_empty: Option<bool>,
}

/// Port of the legacy extension `OAuthAuthInfo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthAuthInfo {
    pub url: String,
    pub instructions: Option<String>,
}

/// Port of the legacy extension `OAuthDeviceCodeInfo`.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthDeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: Option<u64>,
    pub expires_in_seconds: Option<u64>,
}

/// Port of the legacy extension `OAuthSelectOption`.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthSelectOption {
    pub id: String,
    pub label: String,
}

/// Port of the legacy extension `OAuthSelectPrompt`.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthSelectPrompt {
    pub message: String,
    pub options: Vec<OAuthSelectOption>,
}

/// Port of `OAuthLoginCallbacks`: the callback surface retained for
/// coding-agent extension compatibility.
pub trait OAuthLoginCallbacks: Send + Sync {
    /// Authorization URL notification.
    fn on_auth(&self, info: OAuthAuthInfo);

    /// Device-code notification.
    fn on_device_code(&self, info: OAuthDeviceCodeInfo);

    /// Prompts the user; errors on cancel/abort.
    fn on_prompt(&self, prompt: OAuthPrompt) -> BoxedCompatFuture<Result<String, String>>;

    /// Optional progress notification.
    fn on_progress(&self, _message: &str) {}

    /// Optional manual code input.
    fn on_manual_code_input(&self) -> BoxedCompatFuture<Result<String, String>> {
        Box::pin(async { Err("manual code input is not supported".to_string()) })
    }

    /// Optional select prompt; `None` cancels the selection.
    fn on_select(
        &self,
        prompt: OAuthSelectPrompt,
    ) -> BoxedCompatFuture<Result<Option<String>, String>>;

    /// Optional flow cancellation signal.
    fn signal(&self) -> Option<CancellationToken> {
        None
    }
}

/// Scoped environment values used by extension OAuth flows.
pub type ExtensionOAuthEnv = ProviderEnv;

// ---------------------------------------------------------------------------
// The global legacy API face (compat.ts): the api-provider registry, the
// deprecated static catalog reads, and the api-dispatch `stream()`/
// `complete()` functions with env API-key injection.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::ai::env_api_keys::get_env_api_key;
use crate::ai::models::{Models, Provider, ProviderStreams};
use crate::ai::types::{AssistantMessage, Context, Model, SimpleStreamOptions, StreamOptions};
use crate::ai::utils::event_stream::AssistantMessageEventStream;

/// The api-dispatch stream function registered per api.
pub type ApiStreamFn = Arc<
    dyn Fn(&Model, &Context, Option<&StreamOptions>) -> AssistantMessageEventStream + Send + Sync,
>;

/// The api-dispatch simple-stream function registered per api.
pub type ApiStreamSimpleFn = Arc<
    dyn Fn(&Model, &Context, Option<&SimpleStreamOptions>) -> AssistantMessageEventStream
        + Send
        + Sync,
>;

/// Port of `ApiProvider`.
#[derive(Clone)]
pub struct ApiProvider {
    pub api: String,
    pub stream: ApiStreamFn,
    pub stream_simple: ApiStreamSimpleFn,
}

struct RegisteredApiProvider {
    provider: ApiProvider,
    source_id: Option<String>,
}

#[derive(Default)]
struct RegistryState {
    providers: BTreeMap<String, RegisteredApiProvider>,
    next_id: u64,
    /// Registration id per api, bumped on every insert.
    entry_ids: BTreeMap<String, u64>,
    /// Registration ids of the built-in instances, for the identity check in
    /// `getBuiltinProviderForModel`.
    builtin_ids: BTreeMap<String, u64>,
    initialized: bool,
}

fn registry() -> &'static Mutex<RegistryState> {
    static REGISTRY: OnceLock<Mutex<RegistryState>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(RegistryState::default()))
}

/// The shared `builtinModels()` collection the compat entry dispatches
/// built-in models through.
fn compat_models() -> &'static Arc<Models> {
    static MODELS: OnceLock<Arc<Models>> = OnceLock::new();
    MODELS.get_or_init(|| crate::ai::providers::builtin::builtin_models(Default::default()))
}

fn ensure_builtin_registered() {
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.initialized {
        return;
    }
    state.initialized = true;
    register_builtin_api_providers_locked(&mut state);
}

fn register_builtin_api_providers_locked(state: &mut RegistryState) {
    use crate::ai::providers::apis::*;
    let builtins: &[(&str, Arc<dyn ProviderStreams>)] = &[
        ("anthropic-messages", anthropic_messages_api()),
        ("openai-completions", openai_completions_api()),
        ("openai-responses", openai_responses_api()),
        ("openai-codex-responses", openai_codex_responses_api()),
        ("azure-openai-responses", azure_openai_responses_api()),
        ("google-generative-ai", google_generative_ai_api()),
        ("google-vertex", google_vertex_api()),
        ("mistral-conversations", mistral_conversations_api()),
        ("bedrock-converse-stream", bedrock_converse_stream_api()),
        ("pi-messages", pi_messages_api()),
    ];
    for (api, streams) in builtins {
        let api = api.to_string();
        if !state.providers.contains_key(&api) {
            let entry = RegisteredApiProvider {
                provider: ApiProvider {
                    api: api.clone(),
                    stream: Arc::new({
                        let streams = Arc::clone(streams);
                        move |model, context, options| streams.stream(model, context, options)
                    }),
                    stream_simple: Arc::new({
                        let streams = Arc::clone(streams);
                        move |model, context, options| {
                            streams.stream_simple(model, context, options)
                        }
                    }),
                },
                source_id: None,
            };
            state.next_id += 1;
            let id = state.next_id;
            state.entry_ids.insert(api.clone(), id);
            state.providers.insert(api.clone(), entry);
        }
        // The TS loop re-reads `getApiProvider(api)` unconditionally after the
        // conditional registration, so an entry that already existed (a test
        // or extension override) is recorded as the built-in instance too.
        if let Some(id) = state.entry_ids.get(&api) {
            state.builtin_ids.insert(api, *id);
        }
    }
}

/// Port of `registerBuiltInApiProviders`: registers the builtin API
/// implementations without clobbering existing entries, then records each
/// builtin api's current entry as the built-in instance. Safe to call after
/// an override has been registered (matching the exported TS function).
pub fn register_built_in_api_providers() {
    ensure_builtin_registered();
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    register_builtin_api_providers_locked(&mut state);
}

/// Port of `registerApiProvider`.
pub fn register_api_provider(provider: ApiProvider, source_id: Option<&str>) {
    ensure_builtin_registered();
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.next_id += 1;
    let id = state.next_id;
    state.entry_ids.insert(provider.api.clone(), id);
    state.providers.insert(
        provider.api.clone(),
        RegisteredApiProvider {
            provider,
            source_id: source_id.map(str::to_string),
        },
    );
}

/// Port of `getApiProvider`.
pub fn get_api_provider(api: &str) -> Option<ApiProvider> {
    ensure_builtin_registered();
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .providers
        .get(api)
        .map(|entry| entry.provider.clone())
}

/// Port of `getApiProviders`.
pub fn get_api_providers() -> Vec<ApiProvider> {
    ensure_builtin_registered();
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .providers
        .values()
        .map(|entry| entry.provider.clone())
        .collect()
}

/// Port of `unregisterApiProviders`.
pub fn unregister_api_providers(source_id: &str) {
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .providers
        .retain(|_, entry| entry.source_id.as_deref() != Some(source_id));
}

/// Port of `resetApiProviders`.
pub fn reset_api_providers() {
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.providers.clear();
    state.builtin_ids.clear();
    state.entry_ids.clear();
    register_builtin_api_providers_locked(&mut state);
}

/// Port of `getBuiltinProviderForModel`: the provider whose api entry is
/// still the built-in instance and whose catalog covers the model.
fn get_builtin_provider_for_model(model: &Model) -> Option<Arc<dyn Provider>> {
    let state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Identity: the current entry must be the one recorded at builtin
    // registration time (a user override replaces it without a new id).
    let is_builtin = state
        .builtin_ids
        .get(&model.api)
        .is_some_and(|id| state.entry_ids.get(&model.api) == Some(id));
    if !is_builtin {
        return None;
    }
    drop(state);
    let provider = compat_models().get_provider(&model.provider)?;
    provider
        .get_models()
        .iter()
        .any(|candidate| candidate.api == model.api)
        .then(|| Arc::clone(&provider))
}

fn resolve_api_provider(api: &str) -> ApiProvider {
    get_api_provider(api).unwrap_or_else(|| panic!("No API provider registered for api: {api}"))
}

const AMBIENT_AUTH_MARKER: &str = "<authenticated>";

fn with_env_api_key(
    model: &Model,
    api_key: Option<String>,
    env: Option<&ProviderEnv>,
) -> Option<String> {
    if api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
        return api_key;
    }
    match get_env_api_key(&model.provider, env) {
        Some(key) if key != AMBIENT_AUTH_MARKER => Some(key),
        _ => api_key,
    }
}

fn has_resolved_cloudflare_auth(
    api_key: Option<&str>,
    headers: Option<&crate::ai::types::ProviderHeaders>,
) -> bool {
    api_key.is_some_and(|key| !key.trim().is_empty())
        || headers.is_some_and(|headers| {
            headers
                .get("cf-aig-authorization")
                .is_some_and(|value| value.is_some())
        })
}

/// Port of the global `stream()`.
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> AssistantMessageEventStream {
    ensure_builtin_registered();
    if let Some(provider) = get_builtin_provider_for_model(model) {
        if model.provider.starts_with("cloudflare-")
            && !has_resolved_cloudflare_auth(
                options.and_then(|options| options.base.api_key.as_deref()),
                options.and_then(|options| options.base.headers.as_ref()),
            )
        {
            return compat_models().stream(model, context, options.cloned());
        }
        let options = options.map(|options| StreamOptions {
            base: crate::ai::types::ProviderRequestOptions {
                api_key: with_env_api_key(
                    model,
                    options.base.api_key.clone(),
                    options.base.env.as_ref(),
                ),
                ..options.base.clone()
            },
            ..options.clone()
        });
        return provider.stream(model, context, options.as_ref());
    }
    let provider = resolve_api_provider(&model.api);
    let options = options.map(|options| StreamOptions {
        base: crate::ai::types::ProviderRequestOptions {
            api_key: with_env_api_key(
                model,
                options.base.api_key.clone(),
                options.base.env.as_ref(),
            ),
            ..options.base.clone()
        },
        ..options.clone()
    });
    (provider.stream)(model, context, options.as_ref())
}

/// Port of the global `complete()`.
pub async fn complete(
    model: &Model,
    context: &Context,
    options: Option<StreamOptions>,
) -> AssistantMessage {
    stream(model, context, options.as_ref()).result().await
}

/// Port of the global `streamSimple()`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    ensure_builtin_registered();
    if let Some(provider) = get_builtin_provider_for_model(model) {
        if model.provider.starts_with("cloudflare-")
            && !has_resolved_cloudflare_auth(
                options.and_then(|options| options.base.base.api_key.as_deref()),
                options.and_then(|options| options.base.base.headers.as_ref()),
            )
        {
            return compat_models().stream_simple(model, context, options.cloned());
        }
        let options = options.map(|options| SimpleStreamOptions {
            base: StreamOptions {
                base: crate::ai::types::ProviderRequestOptions {
                    api_key: with_env_api_key(
                        model,
                        options.base.base.api_key.clone(),
                        options.base.base.env.as_ref(),
                    ),
                    ..options.base.base.clone()
                },
                ..options.base.clone()
            },
            ..options.clone()
        });
        return provider.stream_simple(model, context, options.as_ref());
    }
    let provider = resolve_api_provider(&model.api);
    let options = options.map(|options| SimpleStreamOptions {
        base: StreamOptions {
            base: crate::ai::types::ProviderRequestOptions {
                api_key: with_env_api_key(
                    model,
                    options.base.base.api_key.clone(),
                    options.base.base.env.as_ref(),
                ),
                ..options.base.base.clone()
            },
            ..options.base.clone()
        },
        ..options.clone()
    });
    (provider.stream_simple)(model, context, options.as_ref())
}

/// Port of the global `completeSimple()`.
pub async fn complete_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessage {
    stream_simple(model, context, options.as_ref())
        .result()
        .await
}

/// The `registerFauxProvider` registration handle.
pub struct CompatFauxRegistration {
    handle: crate::ai::providers::faux::FauxProviderHandle,
    source_id: String,
}

impl std::ops::Deref for CompatFauxRegistration {
    type Target = crate::ai::providers::faux::FauxProviderHandle;
    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl CompatFauxRegistration {
    pub fn unregister(&self) {
        unregister_api_providers(&self.source_id);
    }
}

/// Port of `registerFauxProvider`.
pub fn register_faux_provider(
    options: crate::ai::providers::faux::RegisterFauxProviderOptions,
) -> CompatFauxRegistration {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FAUX_COUNT: AtomicU64 = AtomicU64::new(0);
    ensure_builtin_registered();
    let handle = crate::ai::providers::faux::faux_provider(options);
    let source_id = format!(
        "faux-provider-{}",
        FAUX_COUNT.fetch_add(1, Ordering::SeqCst)
    );
    let api = handle.api.clone();
    let streams = Arc::clone(&handle.streams);
    let api_for_stream = api.clone();
    let streams_for_stream = Arc::clone(&streams);
    register_api_provider(
        ApiProvider {
            api: api.clone(),
            stream: Arc::new(move |model, context, options| {
                assert_api_matches(&api_for_stream, model);
                streams_for_stream.stream(model, context, options)
            }),
            stream_simple: Arc::new(move |model, context, options| {
                assert_api_matches(&api, model);
                streams.stream_simple(model, context, options)
            }),
        },
        Some(&source_id),
    );
    CompatFauxRegistration { handle, source_id }
}

fn assert_api_matches(api: &str, model: &Model) {
    if model.api != api {
        panic!("Mismatched api: {} expected {}", model.api, api);
    }
}

// ---------------------------------------------------------------------------
// Deprecated static catalog reads and per-api stream aliases
// (legacy-api-aliases.ts).

/// @deprecated Static catalog read. Use `get_builtin_model`/`Models::get_model`.
pub fn get_model(provider: &str, model_id: &str) -> Option<Model> {
    crate::ai::providers::builtin::get_builtin_model(provider, model_id)
}

/// @deprecated Static catalog read. Use `get_builtin_models`.
pub fn get_models(provider: &str) -> Vec<Model> {
    crate::ai::providers::builtin::get_builtin_models(provider)
}

/// @deprecated Static catalog read. Use `get_builtin_providers`.
pub fn get_providers() -> Vec<String> {
    crate::ai::providers::builtin::get_builtin_providers()
}

/// @deprecated Use `api::anthropic_messages::stream`.
pub fn stream_anthropic(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::anthropic_messages::AnthropicOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::anthropic_messages::stream(model, context, options)
}

/// @deprecated Use `api::anthropic_messages::stream_simple`.
pub fn stream_simple_anthropic(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::anthropic_messages::stream_simple(model, context, options)
}

/// @deprecated Use `api::azure_openai_responses::stream`.
pub fn stream_azure_openai_responses(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::azure_openai_responses::AzureOpenAIResponsesOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::azure_openai_responses::stream(model, context, options)
}

/// @deprecated Use `api::azure_openai_responses::stream_simple`.
pub fn stream_simple_azure_openai_responses(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::azure_openai_responses::stream_simple(model, context, options)
}

/// @deprecated Use `api::google_generative_ai::stream`.
pub fn stream_google(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::google_generative_ai::GoogleOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::google_generative_ai::stream(model, context, options)
}

/// @deprecated Use `api::google_generative_ai::stream_simple`.
pub fn stream_simple_google(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::google_generative_ai::stream_simple(model, context, options)
}

/// @deprecated Use `api::google_vertex::stream`.
pub fn stream_google_vertex(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::google_vertex::GoogleVertexOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::google_vertex::stream(model, context, options)
}

/// @deprecated Use `api::google_vertex::stream_simple`.
pub fn stream_simple_google_vertex(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::google_vertex::stream_simple(model, context, options)
}

/// @deprecated Use `api::mistral_conversations::stream`.
pub fn stream_mistral(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::mistral_conversations::MistralOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::mistral_conversations::stream(model, context, options)
}

/// @deprecated Use `api::mistral_conversations::stream_simple`.
pub fn stream_simple_mistral(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::mistral_conversations::stream_simple(model, context, options)
}

/// @deprecated Use `api::openai_codex_responses::stream`.
pub fn stream_openai_codex_responses(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::openai_codex_responses::OpenAICodexResponsesOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::openai_codex_responses::stream(model, context, options)
}

/// @deprecated Use `api::openai_codex_responses::stream_simple`.
pub fn stream_simple_openai_codex_responses(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::openai_codex_responses::stream_simple(model, context, options)
}

/// @deprecated Use `api::openai_completions::stream`.
pub fn stream_openai_completions(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::openai_completions::OpenAICompletionsOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::openai_completions::stream(model, context, options)
}

/// @deprecated Use `api::openai_completions::stream_simple`.
pub fn stream_simple_openai_completions(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::openai_completions::stream_simple(model, context, options)
}

/// @deprecated Use `api::openai_responses::stream`.
pub fn stream_openai_responses(
    model: &Model,
    context: &Context,
    options: Option<&crate::ai::api::openai_responses::OpenAIResponsesOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::openai_responses::stream(model, context, options)
}

/// @deprecated Use `api::openai_responses::stream_simple`.
pub fn stream_simple_openai_responses(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    crate::ai::api::openai_responses::stream_simple(model, context, options)
}
