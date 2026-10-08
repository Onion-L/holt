//! Port of `pi-core/ai/test/telemetry-options.test.ts`: `telemetryContext`
//! propagation through every request-option surface and dispatch path.
//!
//! TypeScript observes identity (`toBe`) of the shared `NOOP_TELEMETRY_CONTEXT`
//! object; the Rust port stores `Arc<dyn TelemetryContext>` and asserts
//! identity with `Arc::ptr_eq`.
//!
//! Divergence for the direct image dispatch: TypeScript registers a fake api
//! provider through `registerImagesApiProvider` and observes the context at
//! that boundary. The Rust port links image APIs statically (see
//! `src/ai/images.rs`), so the direct half runs against the real
//! openrouter-images adapter over a canned transport and asserts the dispatch
//! completes with telemetry-bearing options; the identity assertion lives in
//! the ImagesModels half, which dispatches through a fake provider.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_core::ai::api::simple_options::build_base_options;
use pi_core::ai::auth::resolve::ModelsError;
use pi_core::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, AuthFuture, AuthResult, AuthStorageError, ModelAuth, ProviderAuth,
};
use pi_core::ai::images_models::{
    CreateImagesProviderOptions, ProviderImages, create_images_models, create_images_provider,
};
use pi_core::ai::models::{
    CreateProviderOptions, Models, ModelsDeferredCancelOptions, ModelsDeferredFetchOptions,
    ProviderApi, ProviderStreams, create_provider,
};
use pi_core::ai::types::{
    AssistantImages, AssistantMessage, AssistantMessageEvent, BlockContent, Context,
    DeferredFetchOptions, DeferredHandle, DoneReason, ImagesContext, ImagesModel, ImagesOptions,
    ImagesStopReason, JsF64, Model, ModelCost, ModelCostRates, ModelInput, ProviderRequestOptions,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, Usage,
};
use pi_core::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use pi_core::telemetry::{NoopTelemetryContext, TelemetryContext};

type Observed = Arc<Mutex<Vec<Option<Arc<dyn TelemetryContext>>>>>;

fn telemetry_context() -> Arc<dyn TelemetryContext> {
    // Port of `NOOP_TELEMETRY_CONTEXT` shared across every dispatch.
    Arc::new(NoopTelemetryContext)
}

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    }
}

fn images_context() -> ImagesContext {
    ImagesContext {
        input: vec![BlockContent::Text(TextContent {
            text: "circle".to_string(),
            ..Default::default()
        })],
    }
}

fn model() -> Model {
    Model {
        id: "model".to_string(),
        name: "Model".to_string(),
        api: "telemetry-test".to_string(),
        provider: "telemetry-provider".to_string(),
        base_url: "https://example.test".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 1000,
        max_tokens: 100,
        ..Default::default()
    }
}

fn image_model() -> ImagesModel {
    ImagesModel {
        id: "image-model".to_string(),
        name: "Image Model".to_string(),
        api: "telemetry-test-images".to_string(),
        provider: "telemetry-image-provider".to_string(),
        base_url: "https://example.test".to_string(),
        input: vec![ModelInput::Text],
        output: vec![ModelInput::Image],
        cost: ModelCost {
            rates: ModelCostRates {
                input: JsF64(0.0),
                output: JsF64(0.0),
                cache_read: JsF64(0.0),
                cache_write: JsF64(0.0),
            },
            tiers: None,
        },
        headers: None,
        extra: Default::default(),
    }
}

fn request_options(telemetry: &Arc<dyn TelemetryContext>) -> ProviderRequestOptions {
    ProviderRequestOptions {
        telemetry_context: Some(Arc::clone(telemetry)),
        ..Default::default()
    }
}

fn stream_options(telemetry: &Arc<dyn TelemetryContext>) -> StreamOptions {
    StreamOptions {
        base: request_options(telemetry),
        ..Default::default()
    }
}

fn simple_stream_options(telemetry: &Arc<dyn TelemetryContext>) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: stream_options(telemetry),
        ..Default::default()
    }
}

/// Port of `completedStream`: a stream that immediately completes with a
/// `done` event carrying the model-tagged assistant message.
fn completed_stream(request_model: &Model) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let producer = stream.clone();
    let message = AssistantMessage {
        role: pi_core::ai::types::RoleAssistant,
        content: Vec::new(),
        api: request_model.api.clone(),
        provider: request_model.provider.clone(),
        model: request_model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        timestamp: 0,
        ..Default::default()
    };
    tokio::spawn(async move {
        producer.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        });
    });
    stream
}

/// The provider auth from the TypeScript fixture:
/// `auth: { apiKey: { name: "Test", resolve: async () => ({ auth: {} }) } }`.
struct TestApiKeyAuth;

impl ApiKeyAuth for TestApiKeyAuth {
    fn name(&self) -> &str {
        "Test"
    }

    fn resolve(
        &self,
        _input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        Box::pin(async {
            Ok(Some(AuthResult {
                auth: ModelAuth::default(),
                env: None,
                source: None,
            }))
        })
    }
}

/// The fake api from the TypeScript fixture: every dispatch records
/// `options.telemetryContext` and returns a completed stream.
struct TelemetryProbeStreams {
    observed: Observed,
}

impl ProviderStreams for TelemetryProbeStreams {
    fn stream(
        &self,
        request_model: &Model,
        _context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.observed
            .lock()
            .unwrap()
            .push(options.and_then(|options| options.base.telemetry_context.clone()));
        completed_stream(request_model)
    }

    fn stream_simple(
        &self,
        request_model: &Model,
        _context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.observed
            .lock()
            .unwrap()
            .push(options.and_then(|options| options.base.base.telemetry_context.clone()));
        completed_stream(request_model)
    }

    fn fetch_deferred(
        &self,
        request_model: &Model,
        _handle: &DeferredHandle,
        options: Option<&DeferredFetchOptions>,
    ) -> Option<AssistantMessageEventStream> {
        self.observed
            .lock()
            .unwrap()
            .push(options.and_then(|options| options.base.telemetry_context.clone()));
        Some(completed_stream(request_model))
    }

    fn cancel_deferred(
        &self,
        _request_model: &Model,
        _handle: &DeferredHandle,
        options: Option<&ProviderRequestOptions>,
    ) -> Option<BoxFuture<'static, Result<(), ModelsError>>> {
        self.observed
            .lock()
            .unwrap()
            .push(options.and_then(|options| options.telemetry_context.clone()));
        Some(Box::pin(async { Ok(()) }))
    }

    fn supports_deferred(&self) -> bool {
        true
    }

    fn supports_cancel_deferred(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn telemetry_context_is_inherited_by_every_request_option_surface() {
    let telemetry = telemetry_context();
    let options = request_options(&telemetry);
    assert!(
        options
            .telemetry_context
            .as_ref()
            .is_some_and(|observed| Arc::ptr_eq(observed, &telemetry))
    );
    let base = build_base_options(
        &model(),
        &context(),
        Some(&simple_stream_options(&telemetry)),
        None,
    );
    assert!(
        base.base
            .telemetry_context
            .as_ref()
            .is_some_and(|observed| Arc::ptr_eq(observed, &telemetry))
    );
}

#[tokio::test]
async fn telemetry_context_survives_provider_and_models_stream_deferred_dispatch() {
    let telemetry = telemetry_context();
    let observed: Observed = Arc::new(Mutex::new(Vec::new()));
    let model = model();
    let context = context();
    let handle = DeferredHandle {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
        api: model.api.clone(),
        id: "response".to_string(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };

    let provider = create_provider(CreateProviderOptions {
        id: model.provider.clone(),
        organization_id: None,
        name: None,
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(Arc::new(TestApiKeyAuth)),
        models: vec![model.clone()],
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(Arc::new(TelemetryProbeStreams {
            observed: Arc::clone(&observed),
        })),
    });

    provider
        .stream(&model, &context, Some(&stream_options(&telemetry)))
        .result()
        .await;
    provider
        .stream_simple(&model, &context, Some(&simple_stream_options(&telemetry)))
        .result()
        .await;
    provider
        .fetch_deferred(
            &model,
            &handle,
            Some(&DeferredFetchOptions {
                base: request_options(&telemetry),
                wait: None,
            }),
        )
        .expect("provider supports deferred fetch")
        .result()
        .await;
    provider
        .cancel_deferred(&model, &handle, Some(&request_options(&telemetry)))
        .expect("provider supports deferred cancel")
        .await
        .expect("cancel succeeds");

    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&provider));
    models
        .stream(&model, &context, Some(stream_options(&telemetry)))
        .result()
        .await;
    models
        .stream_simple(&model, &context, Some(simple_stream_options(&telemetry)))
        .result()
        .await;
    models
        .fetch_deferred(
            &model,
            &handle,
            Some(ModelsDeferredFetchOptions {
                base: request_options(&telemetry),
                wait: None,
                transform_headers: None,
            }),
        )
        .await;
    models
        .cancel_deferred(
            &model,
            &handle,
            Some(ModelsDeferredCancelOptions {
                base: request_options(&telemetry),
                transform_headers: None,
            }),
        )
        .await
        .expect("cancel succeeds");

    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 8);
    assert!(observed.iter().all(|value| {
        value
            .as_ref()
            .is_some_and(|observed| Arc::ptr_eq(observed, &telemetry))
    }));
}

/// The fake image api from the TypeScript fixture: records
/// `options.telemetryContext` and returns a stopped empty result.
struct TelemetryProbeImages {
    observed: Observed,
}

impl ProviderImages for TelemetryProbeImages {
    fn generate_images<'a>(
        &'a self,
        request_model: &'a ImagesModel,
        _context: &'a ImagesContext,
        options: Option<&'a ImagesOptions>,
    ) -> BoxFuture<'a, AssistantImages> {
        self.observed
            .lock()
            .unwrap()
            .push(options.and_then(|options| options.telemetry_context.clone()));
        Box::pin(async move {
            AssistantImages {
                api: request_model.api.clone(),
                provider: request_model.provider.clone(),
                model: request_model.id.clone(),
                output: Vec::new(),
                response_id: None,
                usage: None,
                stop_reason: ImagesStopReason::Stop,
                error_message: None,
                timestamp: 0,
            }
        })
    }
}

/// Canned transport for the direct-dispatch half (see the module comment).
struct RecordingFetch {
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for RecordingFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                    r#"{"id":"img-1","choices":[{"message":{"content":"ok","images":[]}}]}"#,
                ))])),
            })
        })
    }
}

#[tokio::test]
async fn telemetry_context_survives_direct_and_images_models_image_dispatch() {
    let telemetry = telemetry_context();
    let observed: Observed = Arc::new(Mutex::new(Vec::new()));

    // Direct dispatch (port of `generateImages` from images.ts). The Rust port
    // has no `registerImagesApiProvider`, so the request runs against the real
    // openrouter-images adapter over a canned transport.
    let fetch = Arc::new(RecordingFetch {
        requests: Mutex::new(Vec::new()),
    });
    let openrouter_model = ImagesModel {
        id: "black-forest-labs/flux.2-pro".to_string(),
        name: "FLUX.2 Pro".to_string(),
        api: "openrouter-images".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        input: vec![ModelInput::Text, ModelInput::Image],
        output: vec![ModelInput::Image],
        cost: ModelCost {
            rates: ModelCostRates {
                input: JsF64(0.015),
                output: JsF64(0.03),
                cache_read: JsF64(0.0),
                cache_write: JsF64(0.0),
            },
            tiers: None,
        },
        headers: None,
        extra: Default::default(),
    };
    let direct_options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone() as Arc<dyn HttpFetch>),
        telemetry_context: Some(Arc::clone(&telemetry)),
        ..Default::default()
    };
    let output = pi_core::ai::images::generate_images(
        &openrouter_model,
        &images_context(),
        Some(&direct_options),
    )
    .await
    .expect("direct dispatch succeeds");
    assert_eq!(output.stop_reason, ImagesStopReason::Stop);
    assert_eq!(fetch.requests.lock().unwrap().len(), 1);

    // ImagesModels dispatch through a fake provider: the identity assertion
    // from the TypeScript fixture.
    let models = create_images_models(Default::default());
    models.set_provider(create_images_provider(CreateImagesProviderOptions {
        id: image_model().provider.clone(),
        name: None,
        auth: ProviderAuth::api_key(Arc::new(TestApiKeyAuth)),
        models: vec![image_model()],
        refresh_models: None,
        api: Arc::new(TelemetryProbeImages {
            observed: Arc::clone(&observed),
        }),
    }));
    models
        .generate_images(
            &image_model(),
            &images_context(),
            Some(&ImagesOptions {
                telemetry_context: Some(Arc::clone(&telemetry)),
                ..Default::default()
            }),
        )
        .await;

    assert_eq!(observed.lock().unwrap().len(), 1);
    assert!(
        observed.lock().unwrap()[0]
            .as_ref()
            .is_some_and(|observed| Arc::ptr_eq(observed, &telemetry))
    );
}
