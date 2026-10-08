//! Port of `pi-core/ai/test/faux-provider.test.ts` (the parts testable
//! without a global provider registry) plus `models-runtime.test.ts`
//! coverage for the `Models` collection over the faux provider.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use pi_core::ai::auth::credential_store::InMemoryCredentialStore;
use pi_core::ai::auth::resolve::{
    AuthResolutionOverrides, ModelsError, ModelsErrorCode, ResolveError,
};
use pi_core::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthCheck, AuthEvent, AuthFuture,
    AuthInteraction, AuthOperationOptions, AuthPrompt, AuthResult, AuthStorageError, AuthType,
    BoxedAuthError, Credential, CredentialInfo, CredentialStore, ModelAuth, OAuthAuth,
    OAuthCredential, ProviderAuth,
};
use pi_core::ai::models::{
    CreateModelsOptions, CreateProviderOptions, FetchModelsFn, Models, ModelsPublication,
    ModelsRefreshOptions, Provider, ProviderApi, ProviderStreams, RefreshModelsContext,
    calculate_cost, clamp_thinking_level, create_provider, get_supported_thinking_levels,
    models_are_equal,
};
use pi_core::ai::models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreError,
};
use pi_core::ai::providers::faux::{
    FauxContent, FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_provider,
    faux_text, faux_thinking, faux_tool_call,
};
use pi_core::ai::types::{
    AssistantContent, Context, DoneReason, Message, Model, ModelCost, ModelCostRates,
    ModelCostTier, ModelInput, ModelThinkingLevel, ProviderHeaders, ProviderRequestOptions,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent, ToolCall,
    TransformHeadersFn, Usage, UserContent, UserMessage,
};
use pi_core::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

fn user_context(content: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Text(content.to_string()),
            timestamp: 1_735_689_600_000,
        })],
        tools: None,
    }
}

#[tokio::test]
async fn faux_provider_registers_and_estimates_usage() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("hello world", FauxMessageOptions::default()),
    ))]);

    let context = Context {
        system_prompt: Some("Be concise.".to_string()),
        messages: vec![Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Text("hi there".to_string()),
            timestamp: 1_735_689_600_000,
        })],
        tools: None,
    };

    let response = models.complete(&faux.get_model(), &context, None).await;
    assert_eq!(
        response.content,
        vec![AssistantContent::Text(TextContent {
            text: "hello world".to_string(),
            ..Default::default()
        })]
    );
    assert!(response.usage.input > 0);
    assert!(response.usage.output > 0);
    assert_eq!(
        response.usage.total_tokens,
        response.usage.input + response.usage.output
    );
    let state = faux.state.lock().unwrap();
    assert_eq!(state.call_count, 1);
}

#[tokio::test]
async fn faux_provider_supports_helper_blocks() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    let mut scripted = faux_assistant_message("unused", FauxMessageOptions::default());
    scripted.content = vec![
        faux_thinking("think"),
        faux_tool_call("echo", serde_json::json!({"text": "hi"}), None),
        faux_text("done"),
    ];
    scripted.stop_reason = StopReason::ToolUse;
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(scripted))]);

    let response = models
        .complete(&faux.get_model(), &user_context("hi"), None)
        .await;

    assert_eq!(response.content.len(), 3);
    assert_eq!(
        response.content[0],
        AssistantContent::Thinking(ThinkingContent {
            thinking: "think".to_string(),
            ..Default::default()
        })
    );
    match &response.content[1] {
        AssistantContent::ToolCall(tool_call) => {
            assert!(!tool_call.id.is_empty());
            assert_eq!(tool_call.name, "echo");
            let arguments: serde_json::Value = serde_json::to_value(&tool_call.arguments).unwrap();
            assert_eq!(arguments, serde_json::json!({"text": "hi"}));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    assert_eq!(
        response.content[2],
        AssistantContent::Text(TextContent {
            text: "done".to_string(),
            ..Default::default()
        })
    );
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn faux_provider_supports_multiple_models_and_factories() {
    let faux = faux_provider(pi_core::ai::providers::faux::RegisterFauxProviderOptions {
        models: vec![
            pi_core::ai::providers::faux::FauxModelDefinition {
                id: "faux-fast".to_string(),
                name: Some("Faux Fast".to_string()),
                reasoning: Some(false),
                ..Default::default()
            },
            pi_core::ai::providers::faux::FauxModelDefinition {
                id: "faux-thinker".to_string(),
                name: Some("Faux Thinker".to_string()),
                reasoning: Some(true),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![
        FauxResponseStep::Factory(Arc::new(|_context, _options, _state, model| {
            Ok(faux_assistant_message(
                format!("{}:{}", model.id, model.reasoning),
                FauxMessageOptions::default(),
            ))
        })),
        FauxResponseStep::Factory(Arc::new(|_context, _options, _state, model| {
            Ok(faux_assistant_message(
                format!("{}:{}", model.id, model.reasoning),
                FauxMessageOptions::default(),
            ))
        })),
    ]);

    assert_eq!(
        faux.models
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>(),
        vec!["faux-fast".to_string(), "faux-thinker".to_string()]
    );
    assert_eq!(faux.get_model().id, "faux-fast");
    assert!(!faux.get_model_by_id("faux-fast").unwrap().reasoning);
    assert!(faux.get_model_by_id("faux-thinker").unwrap().reasoning);

    let fast = models
        .complete(
            &faux.get_model_by_id("faux-fast").unwrap(),
            &user_context("hi"),
            None,
        )
        .await;
    let thinker = models
        .complete(
            &faux.get_model_by_id("faux-thinker").unwrap(),
            &user_context("hi"),
            None,
        )
        .await;

    assert_eq!(
        fast.content,
        vec![AssistantContent::Text(TextContent {
            text: "faux-fast:false".to_string(),
            ..Default::default()
        })]
    );
    assert_eq!(
        thinker.content,
        vec![AssistantContent::Text(TextContent {
            text: "faux-thinker:true".to_string(),
            ..Default::default()
        })]
    );
}

#[tokio::test]
async fn faux_provider_rewrites_api_provider_and_model() {
    let faux = faux_provider(pi_core::ai::providers::faux::RegisterFauxProviderOptions {
        api: Some("faux:test".to_string()),
        provider: Some("faux-provider".to_string()),
        models: vec![pi_core::ai::providers::faux::FauxModelDefinition {
            id: "faux-model".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("hello", FauxMessageOptions::default()),
    ))]);

    let response = models
        .complete(&faux.get_model(), &user_context("hi"), None)
        .await;

    assert_eq!(response.api, "faux:test");
    assert_eq!(response.provider, "faux-provider");
    assert_eq!(response.model, "faux-model");
}

#[tokio::test]
async fn faux_provider_consumes_responses_in_order_and_errors_when_exhausted() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "first",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "second",
            FauxMessageOptions::default(),
        ))),
    ]);

    let context = user_context("hi");
    let first = models.complete(&faux.get_model(), &context, None).await;
    let second = models.complete(&faux.get_model(), &context, None).await;
    let third = models.complete(&faux.get_model(), &context, None).await;

    assert_eq!(first_text(&first), "first");
    assert_eq!(first_text(&second), "second");
    assert_eq!(third.stop_reason, StopReason::Error);
    assert_eq!(
        third.error_message.as_deref(),
        Some("No more faux responses queued")
    );
    assert_eq!(faux.get_pending_response_count(), 0);
}

fn first_text(message: &pi_core::ai::types::AssistantMessage) -> String {
    match &message.content[0] {
        AssistantContent::Text(text) => text.text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn faux_provider_streams_events_in_protocol_order() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    let mut scripted = faux_assistant_message("unused", FauxMessageOptions::default());
    scripted.content = vec![faux_text("hello"), faux_thinking("deep")];
    scripted.stop_reason = StopReason::Stop;
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(scripted))]);

    let stream = models.stream(&faux.get_model(), &user_context("hi"), None);
    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        kinds.push(match event {
            pi_core::ai::types::AssistantMessageEvent::Start { .. } => "start",
            pi_core::ai::types::AssistantMessageEvent::TextStart { .. } => "text_start",
            pi_core::ai::types::AssistantMessageEvent::TextDelta { .. } => "text_delta",
            pi_core::ai::types::AssistantMessageEvent::TextEnd { .. } => "text_end",
            pi_core::ai::types::AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            pi_core::ai::types::AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            pi_core::ai::types::AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            pi_core::ai::types::AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
            pi_core::ai::types::AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
            pi_core::ai::types::AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
            pi_core::ai::types::AssistantMessageEvent::Done { .. } => "done",
            pi_core::ai::types::AssistantMessageEvent::Error { .. } => "error",
        });
    }
    let result = stream.result().await;

    assert_eq!(
        kinds,
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_end",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "done",
        ]
    );
    assert_eq!(result.stop_reason, StopReason::Stop);
}

// ---------------------------------------------------------------------------
// models-runtime.test.ts port (runtime collection behavior)
// ---------------------------------------------------------------------------

#[test]
fn models_runtime_model_lookup_and_equality() {
    let faux = faux_provider(Default::default());
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));

    assert_eq!(models.get_providers().len(), 1);
    assert!(models.get_provider("faux").is_some());
    assert!(models.get_provider("missing").is_none());
    assert!(!models.get_models(None).is_empty());
    assert!(models.get_model("faux", "faux-1").is_some());
    assert!(models.get_model("faux", "missing").is_none());
    assert!(models.get_model("missing", "faux-1").is_none());

    let model = models.get_model("faux", "faux-1").unwrap();
    assert!(models_are_equal(Some(&model), Some(&model)));
    let other = Model {
        id: "other".to_string(),
        ..model.clone()
    };
    assert!(!models_are_equal(Some(&model), Some(&other)));
    assert!(!models_are_equal(Some(&model), None));
}

#[test]
fn models_runtime_thinking_levels() {
    let mut model = faux_provider(Default::default()).get_model();
    model.reasoning = false;
    assert_eq!(
        get_supported_thinking_levels(&model),
        vec![ModelThinkingLevel::Off]
    );

    model.reasoning = true;
    let levels = get_supported_thinking_levels(&model);
    assert_eq!(
        levels,
        vec![
            ModelThinkingLevel::Off,
            ModelThinkingLevel::Minimal,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
        ]
    );
    assert_eq!(
        clamp_thinking_level(&model, ModelThinkingLevel::Xhigh),
        ModelThinkingLevel::High
    );

    // null marks a level as unsupported.
    let mut map = std::collections::BTreeMap::new();
    map.insert(ModelThinkingLevel::High, Some("adaptive".to_string()));
    map.insert(ModelThinkingLevel::Medium, None);
    model.thinking_level_map = Some(map);
    let levels = get_supported_thinking_levels(&model);
    assert!(!levels.contains(&ModelThinkingLevel::Medium));
    assert!(levels.contains(&ModelThinkingLevel::High));
    // TS clamps upward first: medium is unsupported (null), high maps to a
    // provider value, so medium clamps to high.
    assert_eq!(
        clamp_thinking_level(&model, ModelThinkingLevel::Medium),
        ModelThinkingLevel::High
    );
}

#[tokio::test]
async fn models_runtime_requires_known_provider() {
    let faux = faux_provider(Default::default());
    let model = faux.get_model();
    let models = Arc::new(Models::new(Default::default()));
    // Provider not registered: the lazy stream surfaces the setup error.
    let response = models.complete(&model, &user_context("hi"), None).await;
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(
        response.error_message.as_deref(),
        Some("Unknown provider: faux")
    );
}

// ---------------------------------------------------------------------------
// Multi-api dispatch: port of the "produces a stream error for a model whose
// api has no implementation" case from `pi-core/ai/test/providers.test.ts`.

/// The `testModel` fixture from providers.test.ts.
fn provider_test_model(api: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: api.to_string(),
        provider: "mixed".to_string(),
        base_url: "https://example.test/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 10_000,
        max_tokens: 1000,
        ..Default::default()
    }
}

#[derive(Default)]
struct NoopStreams;

impl pi_core::ai::models::ProviderStreams for NoopStreams {
    fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        create_assistant_message_event_stream()
    }

    fn stream_simple(
        &self,
        _model: &Model,
        _context: &Context,
        _options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        create_assistant_message_event_stream()
    }
}

struct TestApiKeyAuth;

impl pi_core::ai::auth::types::ApiKeyAuth for TestApiKeyAuth {
    fn name(&self) -> &str {
        "Test"
    }

    fn resolve(
        &self,
        _input: pi_core::ai::auth::types::ApiKeyAuthInput,
    ) -> pi_core::ai::auth::types::AuthFuture<
        Result<
            Option<pi_core::ai::auth::types::AuthResult>,
            pi_core::ai::auth::types::AuthStorageError,
        >,
    > {
        Box::pin(std::future::ready(Ok(Some(
            pi_core::ai::auth::types::AuthResult::default(),
        ))))
    }
}

#[tokio::test]
async fn produces_a_stream_error_for_a_model_whose_api_has_no_implementation() {
    let mut by_api = std::collections::BTreeMap::new();
    by_api.insert(
        "api-a".to_string(),
        Arc::new(NoopStreams) as Arc<dyn pi_core::ai::models::ProviderStreams>,
    );
    let provider =
        pi_core::ai::models::create_provider(pi_core::ai::models::CreateProviderOptions {
            id: "mixed".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(TestApiKeyAuth)),
            models: vec![provider_test_model("api-a", "model-a")],
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::ByApi(by_api),
        });

    let context = Context::default();
    let result = provider
        .stream_simple(&provider_test_model("api-ghost", "model-x"), &context, None)
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(
        result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("no API implementation"))
    );
}

#[allow(unused)]
fn helper_type_check(_: FauxContent, _: ToolCall) {}

// ---------------------------------------------------------------------------
// models-runtime.test.ts port, part 2: the `testProvider`/`envKeyAuth`/
// `testOAuth` fixtures and the remaining runtime cases.
// ---------------------------------------------------------------------------

/// The `testModel(provider, id)` fixture from models-runtime.test.ts.
fn runtime_test_model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.test/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0.into(),
                output: 0.0.into(),
                cache_read: 0.0.into(),
                cache_write: 0.0.into(),
            },
            tiers: None,
        },
        context_window: 10_000,
        max_tokens: 1000,
        ..Default::default()
    }
}

/// The `doneMessage(model, text)` fixture from models-runtime.test.ts.
fn runtime_done_message(model: &Model, text: &str) -> pi_core::ai::types::AssistantMessage {
    pi_core::ai::types::AssistantMessage {
        role: pi_core::ai::types::RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        timestamp: pi_core::ai::auth::resolve::now_millis(),
        ..Default::default()
    }
}

/// The `ProviderCall` recorder from models-runtime.test.ts (`model` is read
/// by the request-options assertions of the provider-auth cases).
#[allow(dead_code)]
struct ProviderCall {
    model: Model,
    options: Option<StreamOptions>,
}

type GetModelsFn = Arc<dyn Fn() -> Vec<Model> + Send + Sync>;
type RefreshModelsFn =
    Arc<dyn Fn(RefreshModelsContext) -> BoxFuture<'static, Result<(), ModelsError>> + Send + Sync>;

/// Inputs of the `testProvider` factory (`models` defaults to one
/// `model-a`; `auth` defaults to the ambient keyless handler).
#[derive(Default)]
struct TestProviderInput {
    id: String,
    models: Option<Vec<Model>>,
    get_models: Option<GetModelsFn>,
    auth: Option<ProviderAuth>,
    refresh_models: Option<RefreshModelsFn>,
    calls: Option<Arc<Mutex<Vec<ProviderCall>>>>,
}

/// The `testProvider` factory: streams a `start`/`done` "ok" message and
/// records the request model plus options.
struct TestProviderImpl {
    id: String,
    models: Vec<Model>,
    get_models_fn: Option<GetModelsFn>,
    auth: ProviderAuth,
    refresh: Option<RefreshModelsFn>,
    calls: Option<Arc<Mutex<Vec<ProviderCall>>>>,
}

fn test_provider(input: TestProviderInput) -> Arc<dyn Provider> {
    let id = input.id;
    Arc::new(TestProviderImpl {
        models: input
            .models
            .unwrap_or_else(|| vec![runtime_test_model(&id, "model-a")]),
        id,
        get_models_fn: input.get_models,
        auth: input
            .auth
            .unwrap_or_else(|| ProviderAuth::api_key(Arc::new(AmbientAuth))),
        refresh: input.refresh_models,
        calls: input.calls,
    })
}

fn respond(
    model: &Model,
    options: Option<StreamOptions>,
    calls: Option<&Arc<Mutex<Vec<ProviderCall>>>>,
) -> AssistantMessageEventStream {
    if let Some(calls) = calls {
        calls.lock().unwrap().push(ProviderCall {
            model: model.clone(),
            options,
        });
    }
    let stream = create_assistant_message_event_stream();
    let message = runtime_done_message(model, "ok");
    stream.push(pi_core::ai::types::AssistantMessageEvent::Start {
        partial: message.clone(),
    });
    stream.push(pi_core::ai::types::AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: message.clone(),
    });
    stream.end(Some(message));
    stream
}

impl Provider for TestProviderImpl {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.id
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        match &self.get_models_fn {
            Some(get_models) => get_models(),
            None => self.models.clone(),
        }
    }

    fn has_refresh_models(&self) -> bool {
        self.refresh.is_some()
    }

    fn refresh_models<'a>(
        &'a self,
        context: RefreshModelsContext,
    ) -> BoxFuture<'a, Result<(), ModelsError>> {
        match &self.refresh {
            Some(refresh) => {
                let refresh = Arc::clone(refresh);
                Box::pin(async move { (refresh)(context).await })
            }
            None => Box::pin(async { Ok(()) }),
        }
    }

    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        respond(model, options.cloned(), self.calls.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        _context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        respond(
            model,
            options.map(|options| options.base.clone()),
            self.calls.as_ref(),
        )
    }
}

/// The `ambientAuth` fixture: keyless test auth reporting "configured".
struct AmbientAuth;

impl ApiKeyAuth for AmbientAuth {
    fn name(&self) -> &str {
        "Ambient"
    }

    fn resolve(
        &self,
        _input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        Box::pin(std::future::ready(Ok(Some(AuthResult::default()))))
    }
}

/// The `envKeyAuth(key)` fixture: stored credential key first, ambient key
/// second, reporting the matching source label.
struct EnvKeyAuth {
    key: Option<String>,
    login_credential: Option<ApiKeyCredential>,
}

fn env_key_auth(key: Option<&str>) -> Arc<EnvKeyAuth> {
    Arc::new(EnvKeyAuth {
        key: key.map(str::to_string),
        login_credential: None,
    })
}

impl ApiKeyAuth for EnvKeyAuth {
    fn name(&self) -> &str {
        "Test API key"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
    ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
        self.login_credential.clone().map(|credential| {
            Box::pin(std::future::ready(Ok(credential)))
                as AuthFuture<Result<ApiKeyCredential, AuthStorageError>>
        })
    }

    fn resolve(
        &self,
        input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        let from_stored = input.credential.is_some();
        let resolved = input
            .credential
            .and_then(|credential| credential.key)
            .or_else(|| self.key.clone());
        let result = resolved.map(|key| AuthResult {
            auth: ModelAuth {
                api_key: Some(key),
                ..Default::default()
            },
            env: None,
            source: Some(if from_stored { "stored" } else { "env" }.to_string()),
        });
        Box::pin(std::future::ready(Ok(result)))
    }
}

type OAuthRefreshOverride = Arc<
    dyn Fn(
            OAuthCredential,
            CancellationToken,
        ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>>
        + Send
        + Sync,
>;

/// The `testOAuth(overrides)` fixture.
#[derive(Default)]
struct TestOAuth {
    refresh: Option<OAuthRefreshOverride>,
}

impl OAuthAuth for TestOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
    ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "not used".to_string(),
        ))))
    }

    fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: CancellationToken,
    ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        match &self.refresh {
            Some(refresh) => refresh(credential.clone(), signal),
            None => Box::pin(std::future::ready(Ok(credential.clone()))),
        }
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> AuthFuture<Result<ModelAuth, AuthStorageError>> {
        Box::pin(std::future::ready(Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })))
    }
}

fn oauth_credential(access: &str, refresh: &str, expires: i64) -> Credential {
    Credential::OAuth(OAuthCredential {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires,
        extra: Default::default(),
    })
}

async fn store_credential(
    credentials: &InMemoryCredentialStore,
    provider_id: &str,
    credential: Credential,
) {
    credentials
        .modify(
            provider_id,
            Box::new(move |_| {
                Box::pin(std::future::ready(Ok(Some(credential))))
                    as AuthFuture<Result<Option<Credential>, BoxedAuthError>>
            }),
            None,
        )
        .await
        .unwrap();
}

/// The `prompt: async () => "unused"` login interaction fixture.
struct UnusedInteraction {
    signal: Option<CancellationToken>,
}

impl AuthInteraction for UnusedInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        self.signal.clone()
    }

    fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<Result<String, AuthStorageError>> {
        Box::pin(std::future::ready(Ok("unused".to_string())))
    }

    fn notify(&self, _event: AuthEvent) {}
}

/// A start/finish gate a blocked callback waits on (the TS tests' pending
/// promises resolved from the test body).
struct BlockedGate {
    started: tokio::sync::oneshot::Sender<()>,
    finish: tokio::sync::oneshot::Receiver<()>,
}

fn blocked_gate() -> (
    BlockedGate,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish) = tokio::sync::oneshot::channel();
    (BlockedGate { started, finish }, started_rx, finish_tx)
}

async fn run_blocked_gate(gate: Option<BlockedGate>) {
    if let Some(gate) = gate {
        let _ = gate.started.send(());
        let _ = gate.finish.await;
    }
}

/// The "Blocked auth" fixture from models-runtime.test.ts.
#[derive(Default)]
struct BlockedApiKeyAuth {
    check: Mutex<Option<BlockedGate>>,
    resolve: Mutex<Option<BlockedGate>>,
}

impl ApiKeyAuth for BlockedApiKeyAuth {
    fn name(&self) -> &str {
        "Blocked auth"
    }

    fn check(
        &self,
        _input: ApiKeyAuthInput,
    ) -> Option<AuthFuture<Result<Option<AuthCheck>, AuthStorageError>>> {
        let gate = self.check.lock().unwrap().take();
        Some(Box::pin(async move {
            run_blocked_gate(gate).await;
            Ok(Some(AuthCheck {
                source: None,
                auth_type: AuthType::ApiKey,
            }))
        }))
    }

    fn resolve(
        &self,
        _input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        let gate = self.resolve.lock().unwrap().take();
        Box::pin(async move {
            run_blocked_gate(gate).await;
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("key".to_string()),
                    ..Default::default()
                },
                env: None,
                source: None,
            }))
        })
    }
}

/// The blocked `oauth.refresh` from models-runtime.test.ts: records the
/// refresh signal and never settles until the test finishes it.
struct BlockedOAuth {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    finish: Mutex<Option<tokio::sync::oneshot::Receiver<OAuthCredential>>>,
    received: Mutex<Option<CancellationToken>>,
}

impl OAuthAuth for BlockedOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
    ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "not used".to_string(),
        ))))
    }

    fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: CancellationToken,
    ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        self.received.lock().unwrap().replace(signal);
        let started = self.started.lock().unwrap().take();
        let finish = self.finish.lock().unwrap().take();
        let fallback = credential.clone();
        Box::pin(async move {
            if let Some(started) = started {
                let _ = started.send(());
            }
            match finish {
                Some(finish) => finish
                    .await
                    .map_err(|_| AuthStorageError("refresh dropped before finishing".to_string())),
                None => Ok(fallback),
            }
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> AuthFuture<Result<ModelAuth, AuthStorageError>> {
        Box::pin(std::future::ready(Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })))
    }
}

/// Maps a publish failure into the error shape `BasicProvider` reports.
fn publish_error(provider_id: &str) -> impl Fn(ModelsStoreError) -> ModelsError + '_ {
    move |error: ModelsStoreError| {
        ModelsError::with_cause(
            ModelsErrorCode::ModelSource,
            format!("Model store write failed for {provider_id}"),
            &error,
        )
    }
}

/// The inline `ModelsStore` of the atomic-deletion case: one shared entry.
#[derive(Default)]
struct SharedEntryStore {
    entry: Mutex<Option<ModelsStoreEntry>>,
}

impl ModelsStore for SharedEntryStore {
    fn read(
        &self,
        _provider_id: &str,
        _options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>,
    ) -> AuthFuture<Result<Option<ModelsStoreEntry>, ModelsStoreError>> {
        let entry = self.entry.lock().unwrap().clone();
        Box::pin(std::future::ready(Ok(entry)))
    }

    fn write(
        &self,
        _provider_id: &str,
        entry: ModelsStoreEntry,
        _options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>,
    ) -> AuthFuture<Result<(), ModelsStoreError>> {
        *self.entry.lock().unwrap() = Some(entry);
        Box::pin(std::future::ready(Ok(())))
    }

    fn delete(
        &self,
        _provider_id: &str,
        _options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>,
    ) -> AuthFuture<Result<(), ModelsStoreError>> {
        *self.entry.lock().unwrap() = None;
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The signal-recording `ModelsStore` of the model-store-wait binding case.
#[derive(Default)]
struct SignalRecordingStore {
    signals: Mutex<Vec<Option<CancellationToken>>>,
}

impl SignalRecordingStore {
    fn record(&self, options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>) {
        self.signals
            .lock()
            .unwrap()
            .push(options.and_then(|options| options.signal.clone()));
    }
}

impl ModelsStore for SignalRecordingStore {
    fn read(
        &self,
        _provider_id: &str,
        options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>,
    ) -> AuthFuture<Result<Option<ModelsStoreEntry>, ModelsStoreError>> {
        self.record(options);
        Box::pin(std::future::ready(Ok(None)))
    }

    fn write(
        &self,
        _provider_id: &str,
        _entry: ModelsStoreEntry,
        options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>,
    ) -> AuthFuture<Result<(), ModelsStoreError>> {
        self.record(options);
        Box::pin(std::future::ready(Ok(())))
    }

    fn delete(
        &self,
        _provider_id: &str,
        options: Option<&pi_core::ai::models_store::ModelsStoreOperationOptions>,
    ) -> AuthFuture<Result<(), ModelsStoreError>> {
        self.record(options);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The modify-counting wrapper store of the "without touching modify" case.
struct CountingCredentialStore {
    base: InMemoryCredentialStore,
    modifies: Arc<AtomicUsize>,
}

impl CredentialStore for CountingCredentialStore {
    fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, AuthStorageError>> {
        self.base.read(provider_id, options)
    }

    fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Vec<CredentialInfo>, AuthStorageError>> {
        self.base.list(options)
    }

    fn modify(
        &self,
        provider_id: &str,
        modify: pi_core::ai::auth::types::ModifyFn,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, BoxedAuthError>> {
        self.modifies.fetch_add(1, Ordering::SeqCst);
        self.base.modify(provider_id, modify, options)
    }

    fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<(), AuthStorageError>> {
        self.base.delete(provider_id, options)
    }
}

/// The read-failing store of the credential-store-failure case.
struct ReadFailingCredentialStore;

impl CredentialStore for ReadFailingCredentialStore {
    fn read(
        &self,
        _provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "disk on fire".to_string(),
        ))))
    }

    fn list(
        &self,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Vec<CredentialInfo>, AuthStorageError>> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    fn modify(
        &self,
        _provider_id: &str,
        _modify: pi_core::ai::auth::types::ModifyFn,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, BoxedAuthError>> {
        Box::pin(std::future::ready(Ok(None)))
    }

    fn delete(
        &self,
        _provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<(), AuthStorageError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The modify-failing store of the credential-store-failure case.
struct ModifyFailingCredentialStore {
    stored: Credential,
}

impl CredentialStore for ModifyFailingCredentialStore {
    fn read(
        &self,
        _provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, AuthStorageError>> {
        let stored = self.stored.clone();
        Box::pin(std::future::ready(Ok(Some(stored))))
    }

    fn list(
        &self,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Vec<CredentialInfo>, AuthStorageError>> {
        Box::pin(std::future::ready(Ok(vec![CredentialInfo {
            provider_id: "p1".to_string(),
            credential_type: "oauth".to_string(),
        }])))
    }

    fn modify(
        &self,
        _provider_id: &str,
        _modify: pi_core::ai::auth::types::ModifyFn,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, BoxedAuthError>> {
        Box::pin(std::future::ready(Err(
            Box::new(AuthStorageError("disk on fire".to_string())) as BoxedAuthError,
        )))
    }

    fn delete(
        &self,
        _provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<(), AuthStorageError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The throwing `apiKey.resolve` of the api-key-failure case.
struct FailingApiKeyAuth;

impl ApiKeyAuth for FailingApiKeyAuth {
    fn name(&self) -> &str {
        "Failing"
    }

    fn resolve(
        &self,
        _input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "nope".to_string(),
        ))))
    }
}

#[test]
fn applies_request_wide_pricing_tiers_above_the_configured_input_threshold() {
    let mut model = runtime_test_model("openai", "gpt-5.6-sol");
    model.cost = ModelCost {
        rates: ModelCostRates {
            input: 5.0.into(),
            output: 30.0.into(),
            cache_read: 0.5.into(),
            cache_write: 6.25.into(),
        },
        tiers: Some(vec![ModelCostTier {
            rates: ModelCostRates {
                input: 10.0.into(),
                output: 45.0.into(),
                cache_read: 1.0.into(),
                cache_write: 12.5.into(),
            },
            input_tokens_above: 272_000,
        }]),
    };
    let create_usage = |cache_write: u64| Usage {
        input: 200_000,
        output: 100_000,
        cache_read: 72_000,
        cache_write,
        total_tokens: 372_000 + cache_write,
        ..Default::default()
    };

    let mut short_usage = create_usage(0);
    let short = calculate_cost(&model, &mut short_usage);
    assert!((short.input.0 - 1.0).abs() < 1e-9);
    assert!((short.output.0 - 3.0).abs() < 1e-9);
    assert!((short.cache_read.0 - 0.036).abs() < 1e-9);
    assert_eq!(short.cache_write.0, 0.0);

    // One token above the tier threshold switches to the tier rates.
    let mut long_usage = create_usage(1);
    let long = calculate_cost(&model, &mut long_usage);
    assert!((long.input.0 - 2.0).abs() < 1e-9);
    assert!((long.output.0 - 4.5).abs() < 1e-9);
    assert!((long.cache_read.0 - 0.072).abs() < 1e-9);
    assert!((long.cache_write.0 - 0.0000125).abs() < 1e-12);
}

#[test]
fn registers_replaces_and_deletes_providers() {
    let models = Models::new(Default::default());
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p2".to_string(),
        ..Default::default()
    }));
    assert_eq!(
        models
            .get_providers()
            .iter()
            .map(|provider| provider.id().to_string())
            .collect::<Vec<_>>(),
        vec!["p1".to_string(), "p2".to_string()]
    );

    let replacement = test_provider(TestProviderInput {
        id: "p1".to_string(),
        ..Default::default()
    });
    models.set_provider(Arc::clone(&replacement));
    assert!(Arc::ptr_eq(
        &models.get_provider("p1").unwrap(),
        &replacement
    ));
    assert_eq!(models.get_providers().len(), 2);

    models.delete_provider("p1");
    assert!(models.get_provider("p1").is_none());

    models.clear_providers();
    assert_eq!(models.get_providers().len(), 0);
}

#[test]
fn swallows_provider_source_failures_for_both_all_provider_and_single_provider_listing() {
    let models = Models::new(Default::default());
    models.set_provider(test_provider(TestProviderInput {
        id: "broken".to_string(),
        get_models: Some(Arc::new(|| -> Vec<Model> { panic!("boom") }) as GetModelsFn),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "ok".to_string(),
        models: Some(vec![runtime_test_model("ok", "m1")]),
        ..Default::default()
    }));

    // Silence the expected panic output, mirroring the TS throw.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    assert_eq!(
        models
            .get_models(None)
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>(),
        vec!["m1".to_string()]
    );
    assert!(models.get_models(Some("broken")).is_empty());
    // Precise failures come from the provider directly.
    let direct = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        models.get_provider("broken").unwrap().get_models()
    }));
    let payload = direct.expect_err("provider panics");
    let message = if let Some(message) = payload.downcast_ref::<&str>() {
        Some(message.to_string())
    } else {
        payload.downcast_ref::<String>().cloned()
    };
    std::panic::set_hook(previous_hook);
    assert_eq!(message.as_deref(), Some("boom"));
}

#[tokio::test]
async fn refresh_updates_every_configured_dynamic_provider_and_reports_failures() {
    let list = Arc::new(Mutex::new(vec![runtime_test_model("dyn", "before")]));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Models::new(Default::default()));
    let list_for_get = Arc::clone(&list);
    models.set_provider(test_provider(TestProviderInput {
        id: "dyn".to_string(),
        get_models: Some(Arc::new(move || list_for_get.lock().unwrap().clone())),
        refresh_models: Some({
            let list = Arc::clone(&list);
            let refreshes = Arc::clone(&refreshes);
            Arc::new(move |context| {
                let list = Arc::clone(&list);
                let refreshes = Arc::clone(&refreshes);
                Box::pin(async move {
                    if !context.allow_network {
                        return Ok(());
                    }
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    let after = runtime_test_model("dyn", "after");
                    (context.publish)(ModelsPublication {
                        persist: None,
                        update: Some(Box::new(move || {
                            *list.lock().unwrap() = vec![after];
                        })),
                    })
                    .await
                    .map_err(publish_error("dyn"))?;
                    Ok(())
                }) as BoxFuture<'static, Result<(), ModelsError>>
            })
        }),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "static".to_string(),
        models: Some(vec![runtime_test_model("static", "s1")]),
        ..Default::default()
    }));

    assert!(models.get_model("dyn", "before").is_some());
    let first = models.refresh(Default::default()).await;
    assert_eq!(first.errors.len(), 0);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert!(models.get_model("dyn", "after").is_some());
    assert!(models.get_model("dyn", "before").is_none());

    models.set_provider(test_provider(TestProviderInput {
        id: "flaky".to_string(),
        refresh_models: Some(Arc::new(|context| {
            Box::pin(async move {
                if context.allow_network {
                    return Err(ModelsError::new(
                        ModelsErrorCode::ModelSource,
                        "fetch failed",
                    ));
                }
                Ok(())
            })
        })),
        ..Default::default()
    }));
    let second = models.refresh(Default::default()).await;
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    assert_eq!(
        second
            .errors
            .get("flaky")
            .map(|error| error.message.as_str()),
        Some("fetch failed")
    );
}

#[tokio::test]
async fn restricts_refresh_work_to_selected_providers() {
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let models = Arc::new(Models::new(Default::default()));
    for id in ["one", "two"] {
        let calls = Arc::clone(&calls);
        models.set_provider(test_provider(TestProviderInput {
            id: id.to_string(),
            refresh_models: Some(Arc::new(move |context| {
                let entry = format!(
                    "{id}:{}",
                    if context.allow_network {
                        "network"
                    } else {
                        "cache"
                    }
                );
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.lock().unwrap().push(entry);
                    Ok(())
                })
            })),
            ..Default::default()
        }));
    }

    let result = models
        .refresh(ModelsRefreshOptions {
            providers: Some(vec!["two".to_string(), "unknown".to_string()]),
            ..Default::default()
        })
        .await;

    assert_eq!(result.errors.len(), 0);
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["two:cache".to_string(), "two:network".to_string()]
    );
}

#[tokio::test]
async fn restores_cached_models_before_waiting_for_network_auth() {
    let store = Arc::new(InMemoryModelsStore::new());
    ModelsStore::write(
        store.as_ref(),
        "dynamic",
        ModelsStoreEntry {
            models: vec![runtime_test_model("dynamic", "cached")],
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    let auth = Arc::new(BlockedApiKeyAuth::default());
    let (resolve_gate, auth_started, _finish_auth) = blocked_gate();
    auth.resolve.lock().unwrap().replace(resolve_gate);
    let provider = create_provider(CreateProviderOptions {
        id: "dynamic".to_string(),
        organization_id: None,
        name: None,
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(Arc::clone(&auth) as Arc<dyn ApiKeyAuth>),
        models: Vec::new(),
        fetch_models: Some(Arc::new(|_context| {
            Box::pin(async move {
                Err(ModelsError::new(
                    ModelsErrorCode::ModelSource,
                    "must not fetch",
                ))
            })
        })),
        filter_models: None,
        api: ProviderApi::Single(Arc::new(NoopStreams) as Arc<dyn ProviderStreams>),
    });
    let models = Arc::new(Models::new(CreateModelsOptions {
        models_store: Some(Arc::clone(&store) as Arc<dyn ModelsStore>),
        ..Default::default()
    }));
    models.set_provider(provider);
    let controller = CancellationToken::new();
    let pending_models = Arc::clone(&models);
    let pending_signal = controller.clone();
    let pending = tokio::spawn(async move {
        pending_models
            .refresh(ModelsRefreshOptions {
                providers: Some(vec!["dynamic".to_string()]),
                signal: Some(pending_signal),
                ..Default::default()
            })
            .await
    });
    auth_started.await.unwrap();

    assert!(models.get_model("dynamic", "cached").is_some());
    controller.cancel();
    let result = pending.await.unwrap();
    assert!(result.aborted);
}

#[tokio::test]
async fn lets_providers_choose_persistent_deletion_and_ephemeral_publication_atomically() {
    let store = Arc::new(SharedEntryStore {
        entry: Mutex::new(Some(ModelsStoreEntry {
            models: vec![runtime_test_model("dynamic", "stored")],
            ..Default::default()
        })),
    });
    let state = Arc::new(Mutex::new("initial".to_string()));
    let models = Arc::new(Models::new(CreateModelsOptions {
        models_store: Some(Arc::clone(&store) as Arc<dyn ModelsStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "dynamic".to_string(),
        refresh_models: Some({
            let store = Arc::clone(&store);
            let state = Arc::clone(&state);
            Arc::new(move |context| {
                let store = Arc::clone(&store);
                let state = Arc::clone(&state);
                Box::pin(async move {
                    assert_eq!(
                        context
                            .stored
                            .as_ref()
                            .and_then(|stored| stored.models.first())
                            .map(|model| model.id.as_str()),
                        Some("stored")
                    );
                    (context.publish)(ModelsPublication {
                        persist: Some(None),
                        update: Some({
                            let store = Arc::clone(&store);
                            let state = Arc::clone(&state);
                            Box::new(move || {
                                assert_eq!(*store.entry.lock().unwrap(), None);
                                *state.lock().unwrap() = "deleted".to_string();
                            })
                        }),
                    })
                    .await
                    .unwrap();
                    (context.publish)(ModelsPublication {
                        persist: None,
                        update: Some({
                            let state = Arc::clone(&state);
                            Box::new(move || {
                                *state.lock().unwrap() = "ephemeral".to_string();
                            })
                        }),
                    })
                    .await
                    .unwrap();
                    Ok(())
                }) as BoxFuture<'static, Result<(), ModelsError>>
            })
        }),
        ..Default::default()
    }));

    let result = models
        .refresh(ModelsRefreshOptions {
            allow_network: Some(false),
            ..Default::default()
        })
        .await;

    assert_eq!(result.errors.len(), 0);
    assert_eq!(*store.entry.lock().unwrap(), None);
    assert_eq!(*state.lock().unwrap(), "ephemeral");
}

#[tokio::test]
async fn persists_dynamic_catalogs_and_restores_them_without_network_access() {
    let credentials = InMemoryCredentialStore::new();
    let models_store = InMemoryModelsStore::new();
    store_credential(
        &credentials,
        "dynamic",
        Credential::ApiKey(ApiKeyCredential {
            key: Some("key".to_string()),
            env: None,
        }),
    )
    .await;
    let create_dynamic_provider = |fetch_models: Option<FetchModelsFn>| {
        create_provider(CreateProviderOptions {
            id: "dynamic".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth::api_key(env_key_auth(None)),
            models: Vec::new(),
            fetch_models,
            filter_models: None,
            api: ProviderApi::Single(Arc::new(NoopStreams) as Arc<dyn ProviderStreams>),
        })
    };

    let online = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        models_store: Some(Arc::new(models_store.clone()) as Arc<dyn ModelsStore>),
        ..Default::default()
    }));
    online.set_provider(create_dynamic_provider(Some(Arc::new(|_context| {
        Box::pin(async move { Ok(vec![runtime_test_model("dynamic", "fetched")]) })
    }))));
    let online_refresh = online.refresh(Default::default()).await;
    assert_eq!(online_refresh.errors.len(), 0);
    assert!(online.get_model("dynamic", "fetched").is_some());

    let offline = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        models_store: Some(Arc::new(models_store.clone()) as Arc<dyn ModelsStore>),
        ..Default::default()
    }));
    offline.set_provider(create_dynamic_provider(Some(Arc::new(|_context| {
        Box::pin(async move {
            Err(ModelsError::new(
                ModelsErrorCode::ModelSource,
                "must not fetch",
            ))
        })
    }))));
    let offline_refresh = offline
        .refresh(ModelsRefreshOptions {
            allow_network: Some(false),
            ..Default::default()
        })
        .await;
    assert_eq!(offline_refresh.errors.len(), 0);
    assert!(offline.get_model("dynamic", "fetched").is_some());
}

#[tokio::test]
async fn passes_effective_api_key_credentials_and_refresh_options_while_skipping_unconfigured_providers()
 {
    let effective_credential = Arc::new(Mutex::new(None::<Credential>));
    let force_refresh = Arc::new(Mutex::new(None::<bool>));
    let unconfigured_refreshes = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "configured".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(Some("ambient-key")))),
        refresh_models: Some({
            let effective_credential = Arc::clone(&effective_credential);
            let force_refresh = Arc::clone(&force_refresh);
            Arc::new(move |context| {
                let effective_credential = Arc::clone(&effective_credential);
                let force_refresh = Arc::clone(&force_refresh);
                Box::pin(async move {
                    if !context.allow_network {
                        return Ok(());
                    }
                    *effective_credential.lock().unwrap() = context.credential.clone();
                    *force_refresh.lock().unwrap() = context.force;
                    Ok(())
                })
            })
        }),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "unconfigured".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(None))),
        refresh_models: Some({
            let unconfigured_refreshes = Arc::clone(&unconfigured_refreshes);
            Arc::new(move |context| {
                let unconfigured_refreshes = Arc::clone(&unconfigured_refreshes);
                Box::pin(async move {
                    if context.allow_network {
                        unconfigured_refreshes.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                })
            })
        }),
        ..Default::default()
    }));

    models
        .refresh(ModelsRefreshOptions {
            force: Some(true),
            ..Default::default()
        })
        .await;
    assert_eq!(
        *effective_credential.lock().unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("ambient-key".to_string()),
            env: None,
        }))
    );
    assert_eq!(*force_refresh.lock().unwrap(), Some(true));
    assert_eq!(unconfigured_refreshes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn refreshes_expired_oauth_before_refreshing_models() {
    let credentials = InMemoryCredentialStore::new();
    let model_refresh_credential = Arc::new(Mutex::new(None::<Credential>));
    store_credential(
        &credentials,
        "oauth-dynamic",
        oauth_credential("expired", "refresh", 0),
    )
    .await;
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "oauth-dynamic".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some(
                Arc::new(|credential: OAuthCredential, _signal: CancellationToken| {
                    Box::pin(async move {
                        Ok(OAuthCredential {
                            access: "fresh".to_string(),
                            refresh: "rotated".to_string(),
                            expires: pi_core::ai::auth::resolve::now_millis() + 60_000,
                            ..credential
                        })
                    }) as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                }) as OAuthRefreshOverride,
            ),
        }))),
        refresh_models: Some({
            let model_refresh_credential = Arc::clone(&model_refresh_credential);
            Arc::new(move |context| {
                let model_refresh_credential = Arc::clone(&model_refresh_credential);
                Box::pin(async move {
                    if context.allow_network {
                        *model_refresh_credential.lock().unwrap() = context.credential.clone();
                    }
                    Ok(())
                })
            })
        }),
        ..Default::default()
    }));

    let result = models.refresh(Default::default()).await;
    assert_eq!(result.errors.len(), 0);
    match &*model_refresh_credential.lock().unwrap() {
        Some(Credential::OAuth(credential)) => {
            assert_eq!(credential.access, "fresh");
            assert_eq!(credential.refresh, "rotated");
        }
        other => panic!("expected a refreshed oauth credential, got {other:?}"),
    }
    match credentials.read("oauth-dynamic", None).await.unwrap() {
        Some(Credential::OAuth(credential)) => {
            assert_eq!(credential.access, "fresh");
            assert_eq!(credential.refresh, "rotated");
        }
        other => panic!("expected a stored oauth credential, got {other:?}"),
    }
}

#[tokio::test]
async fn always_gives_providers_a_concrete_signal() {
    let received_signal = Arc::new(Mutex::new(None::<CancellationToken>));
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "dynamic".to_string(),
        refresh_models: Some({
            let received_signal = Arc::clone(&received_signal);
            Arc::new(move |context| {
                let received_signal = Arc::clone(&received_signal);
                Box::pin(async move {
                    *received_signal.lock().unwrap() = Some(context.signal.clone());
                    Ok(())
                })
            })
        }),
        ..Default::default()
    }));

    let result = models.refresh(Default::default()).await;
    assert!(!result.aborted);
    // The concrete signal stand-in: a live, uncancelled token (TS checks
    // `instanceof AbortSignal`, which the CancellationToken type guarantees).
    let signal = received_signal
        .lock()
        .unwrap()
        .clone()
        .expect("provider received a signal");
    assert!(!signal.is_cancelled());
}

#[tokio::test]
async fn binds_model_store_waits_to_the_provider_refresh_signal() {
    let store = Arc::new(SignalRecordingStore::default());
    let provider_signal = Arc::new(Mutex::new(None::<CancellationToken>));
    let models = Arc::new(Models::new(CreateModelsOptions {
        models_store: Some(Arc::clone(&store) as Arc<dyn ModelsStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "dynamic".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(Some("key")))),
        refresh_models: Some({
            let provider_signal = Arc::clone(&provider_signal);
            Arc::new(move |context| {
                let provider_signal = Arc::clone(&provider_signal);
                let fresh = runtime_test_model("dynamic", "fresh");
                Box::pin(async move {
                    *provider_signal.lock().unwrap() = Some(context.signal.clone());
                    if !context.allow_network {
                        return Ok(());
                    }
                    (context.publish)(ModelsPublication {
                        persist: Some(Some(ModelsStoreEntry {
                            models: vec![fresh],
                            ..Default::default()
                        })),
                        update: None,
                    })
                    .await
                    .map_err(publish_error("dynamic"))?;
                    Ok(())
                }) as BoxFuture<'static, Result<(), ModelsError>>
            })
        }),
        ..Default::default()
    }));

    let result = models
        .refresh(ModelsRefreshOptions {
            providers: Some(vec!["dynamic".to_string()]),
            ..Default::default()
        })
        .await;

    assert_eq!(result.errors.len(), 0);
    let signals = store.signals.lock().unwrap().clone();
    assert_eq!(signals.len(), 3);
    let provider_signal = provider_signal
        .lock()
        .unwrap()
        .clone()
        .expect("provider signal");
    // Identity stand-in for `signal === providerSignal`: clones share state,
    // so cancelling the provider token cancels every recorded storage wait.
    provider_signal.cancel();
    assert!(
        signals
            .iter()
            .all(|signal| signal.as_ref().is_some_and(|signal| signal.is_cancelled()))
    );
}

#[tokio::test]
async fn returns_aborted_state_without_reporting_cancellation_as_a_provider_error() {
    let controller = CancellationToken::new();
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "dynamic".to_string(),
        refresh_models: Some({
            let controller = controller.clone();
            Arc::new(move |context| {
                let controller = controller.clone();
                Box::pin(async move {
                    controller.cancel();
                    if context.signal.is_cancelled() {
                        return Ok(());
                    }
                    Ok(())
                })
            })
        }),
        ..Default::default()
    }));

    let result = models
        .refresh(ModelsRefreshOptions {
            signal: Some(controller),
            ..Default::default()
        })
        .await;
    assert!(result.aborted);
    assert_eq!(result.errors.len(), 0);
}

#[tokio::test]
async fn stops_waiting_on_abort_when_a_provider_ignores_its_signal() {
    let controller = CancellationToken::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (reject_tx, reject_rx) = tokio::sync::oneshot::channel::<Result<(), ModelsError>>();
    let stall = Arc::new(Mutex::new(Some(reject_rx)));
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Mutex::new(Some(started_tx)));
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "dynamic".to_string(),
        refresh_models: Some({
            let stall = Arc::clone(&stall);
            let calls = Arc::clone(&calls);
            let started = Arc::clone(&started);
            Arc::new(move |_context| {
                let stall = Arc::clone(&stall);
                let calls = Arc::clone(&calls);
                let started = Arc::clone(&started);
                Box::pin(async move {
                    if calls.fetch_add(1, Ordering::SeqCst) != 0 {
                        return Ok(());
                    }
                    if let Some(started) = started.lock().unwrap().take() {
                        let _ = started.send(());
                    }
                    let reject = stall.lock().unwrap().take();
                    match reject {
                        Some(reject) => reject.await.unwrap_or(Ok(())),
                        None => Ok(()),
                    }
                }) as BoxFuture<'static, Result<(), ModelsError>>
            })
        }),
        ..Default::default()
    }));

    let pending_models = Arc::clone(&models);
    let pending_signal = controller.clone();
    let pending = tokio::spawn(async move {
        pending_models
            .refresh(ModelsRefreshOptions {
                signal: Some(pending_signal),
                ..Default::default()
            })
            .await
    });
    started_rx.await.unwrap();
    controller.cancel();

    let result = pending.await.unwrap();
    assert!(result.aborted);
    assert_eq!(result.errors.len(), 0);

    // A late provider failure is not reported for the aborted refresh.
    let _ = reject_tx.send(Err(ModelsError::new(
        ModelsErrorCode::ModelSource,
        "late provider failure",
    )));
    tokio::task::yield_now().await;
    assert_eq!(result.errors.len(), 0);
}

#[tokio::test]
async fn passes_caller_signals_to_provider_auth_callbacks() {
    let controller = CancellationToken::new();
    let received: Arc<Mutex<Vec<CancellationToken>>> = Arc::default();
    let models = Arc::new(Models::new(Default::default()));

    struct SignalApiKeyAuth {
        received: Arc<Mutex<Vec<CancellationToken>>>,
    }

    impl ApiKeyAuth for SignalApiKeyAuth {
        fn name(&self) -> &str {
            "Signal auth"
        }

        fn login(
            &self,
            interaction: Arc<dyn AuthInteraction>,
        ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
            if let Some(signal) = interaction.signal() {
                self.received.lock().unwrap().push(signal);
            }
            Some(Box::pin(std::future::ready(Ok(ApiKeyCredential {
                key: Some("saved".to_string()),
                env: None,
            }))))
        }

        fn check(
            &self,
            input: ApiKeyAuthInput,
        ) -> Option<AuthFuture<Result<Option<AuthCheck>, AuthStorageError>>> {
            self.received.lock().unwrap().push(input.signal.clone());
            Some(Box::pin(std::future::ready(Ok(Some(AuthCheck {
                source: None,
                auth_type: AuthType::ApiKey,
            })))))
        }

        fn resolve(
            &self,
            input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
            self.received.lock().unwrap().push(input.signal.clone());
            Box::pin(std::future::ready(Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("resolved".to_string()),
                    ..Default::default()
                },
                env: None,
                source: None,
            }))))
        }
    }

    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::api_key(Arc::new(SignalApiKeyAuth {
            received: Arc::clone(&received),
        }))),
        ..Default::default()
    }));

    models
        .check_auth(
            "p1",
            Some(&AuthOperationOptions {
                signal: Some(controller.clone()),
            }),
        )
        .await
        .unwrap();
    models
        .get_auth(
            "p1",
            None,
            Some(&AuthResolutionOverrides {
                signal: Some(controller.clone()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    models
        .login(
            "p1",
            AuthType::ApiKey,
            Arc::new(UnusedInteraction {
                signal: Some(controller.clone()),
            }),
        )
        .await
        .unwrap();

    assert_eq!(received.lock().unwrap().len(), 3);
    // Reference-equality stand-in: the three recorded tokens share the
    // caller's token state, so cancelling the caller cancels each of them.
    controller.cancel();
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .all(|signal| signal.is_cancelled())
    );
}

#[tokio::test]
async fn stops_waiting_for_non_cooperative_auth_callbacks() {
    let auth = Arc::new(BlockedApiKeyAuth::default());
    let (check_gate, check_started, finish_check) = blocked_gate();
    auth.check.lock().unwrap().replace(check_gate);
    let (resolve_gate, resolve_started, finish_resolve) = blocked_gate();
    auth.resolve.lock().unwrap().replace(resolve_gate);
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::api_key(
            Arc::clone(&auth) as Arc<dyn ApiKeyAuth>
        )),
        ..Default::default()
    }));

    let available_controller = CancellationToken::new();
    let available_models = Arc::clone(&models);
    let available_signal = available_controller.clone();
    let available = tokio::spawn(async move {
        available_models
            .get_available(
                None,
                Some(&AuthOperationOptions {
                    signal: Some(available_signal),
                }),
            )
            .await
    });
    check_started.await.unwrap();
    available_controller.cancel();
    assert!(matches!(
        available.await.unwrap().unwrap_err(),
        ResolveError::Aborted
    ));

    let auth_controller = CancellationToken::new();
    let auth_models = Arc::clone(&models);
    let auth_signal = auth_controller.clone();
    let auth = tokio::spawn(async move {
        auth_models
            .get_auth(
                "p1",
                None,
                Some(&AuthResolutionOverrides {
                    signal: Some(auth_signal),
                    ..Default::default()
                }),
            )
            .await
    });
    resolve_started.await.unwrap();
    auth_controller.cancel();
    assert!(matches!(
        auth.await.unwrap().unwrap_err(),
        ResolveError::Aborted
    ));

    let _ = finish_check.send(());
    let _ = finish_resolve.send(());
}

#[tokio::test]
async fn cancels_queued_credential_mutations_without_running_them_later() {
    let credentials = InMemoryCredentialStore::new();
    let (finish_first_tx, finish_first_rx) = tokio::sync::oneshot::channel::<()>();
    let second_ran = Arc::new(AtomicUsize::new(0));

    let first_store = credentials.clone();
    let first = tokio::spawn(async move {
        first_store
            .modify(
                "p1",
                Box::new(move |_| {
                    Box::pin(async move {
                        let _ = finish_first_rx.await;
                        Ok(Some(Credential::ApiKey(ApiKeyCredential {
                            key: Some("first".to_string()),
                            env: None,
                        })))
                    }) as AuthFuture<Result<Option<Credential>, BoxedAuthError>>
                }),
                None,
            )
            .await
    });

    let controller = CancellationToken::new();
    let second_ran_for_modify = Arc::clone(&second_ran);
    let second = credentials.modify(
        "p1",
        Box::new(move |_| {
            let second_ran = Arc::clone(&second_ran_for_modify);
            Box::pin(async move {
                second_ran.fetch_add(1, Ordering::SeqCst);
                Ok(Some(Credential::ApiKey(ApiKeyCredential {
                    key: Some("second".to_string()),
                    env: None,
                })))
            }) as AuthFuture<Result<Option<Credential>, BoxedAuthError>>
        }),
        Some(&AuthOperationOptions {
            signal: Some(controller.clone()),
        }),
    );

    controller.cancel();
    assert!(second.await.is_err());
    let _ = finish_first_tx.send(());
    first.await.unwrap().unwrap();
    tokio::task::yield_now().await;

    assert_eq!(second_ran.load(Ordering::SeqCst), 0);
    assert_eq!(
        credentials.read("p1", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("first".to_string()),
            env: None,
        }))
    );
}

#[tokio::test]
async fn passes_cancellation_to_oauth_refresh_and_preserves_the_previous_credential() {
    let credentials = InMemoryCredentialStore::new();
    let previous = oauth_credential("old", "old-refresh", 0);
    store_credential(&credentials, "p1", previous.clone()).await;
    let (refresh_started_tx, refresh_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_refresh_tx, finish_refresh_rx) = tokio::sync::oneshot::channel::<OAuthCredential>();
    let oauth = Arc::new(BlockedOAuth {
        started: Mutex::new(Some(refresh_started_tx)),
        finish: Mutex::new(Some(finish_refresh_rx)),
        received: Mutex::new(None),
    });
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::clone(&oauth) as Arc<dyn OAuthAuth>)),
        ..Default::default()
    }));
    let controller = CancellationToken::new();
    let auth_models = Arc::clone(&models);
    let auth_signal = controller.clone();
    let auth = tokio::spawn(async move {
        auth_models
            .get_auth(
                "p1",
                None,
                Some(&AuthResolutionOverrides {
                    signal: Some(auth_signal),
                    ..Default::default()
                }),
            )
            .await
    });
    refresh_started_rx.await.unwrap();
    controller.cancel();

    assert!(matches!(
        auth.await.unwrap().unwrap_err(),
        ResolveError::Aborted
    ));
    let received_signal = oauth
        .received
        .lock()
        .unwrap()
        .clone()
        .expect("refresh received a signal");
    // The refresh signal is derived from the caller's (AbortSignal.any in
    // TS); cancelling the caller propagates to it. Rust tokens carry no
    // abort reason, so the `reason` identity check has no equivalent.
    for _ in 0..64 {
        if received_signal.is_cancelled() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(received_signal.is_cancelled());
    let mut rotated = match previous.clone() {
        Credential::OAuth(credential) => credential,
        other => panic!("expected an oauth credential, got {other:?}"),
    };
    rotated.access = "new".to_string();
    rotated.expires = pi_core::ai::auth::resolve::now_millis() + 60_000;
    let _ = finish_refresh_tx.send(rotated);
    tokio::task::yield_now().await;
    assert_eq!(credentials.read("p1", None).await.unwrap(), Some(previous));
}

#[tokio::test]
async fn checks_provider_auth_without_refreshing_oauth_and_filters_available_models() {
    let credentials = InMemoryCredentialStore::new();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "ambient".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(Some("env-key")))),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "missing".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(None))),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "oauth".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some({
                let refreshes = Arc::clone(&refreshes);
                Arc::new(
                    move |credential: OAuthCredential, _signal: CancellationToken| {
                        let refreshes = Arc::clone(&refreshes);
                        Box::pin(async move {
                            refreshes.fetch_add(1, Ordering::SeqCst);
                            Ok(credential)
                        })
                            as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                    },
                ) as OAuthRefreshOverride
            }),
        }))),
        ..Default::default()
    }));
    store_credential(
        &credentials,
        "oauth",
        oauth_credential("expired", "refresh", 0),
    )
    .await;

    assert_eq!(
        models.check_auth("ambient", None).await.unwrap(),
        Some(AuthCheck {
            source: Some("env".to_string()),
            auth_type: AuthType::ApiKey,
        })
    );
    assert_eq!(models.check_auth("missing", None).await.unwrap(), None);
    assert_eq!(
        models.check_auth("oauth", None).await.unwrap(),
        Some(AuthCheck {
            source: Some("OAuth".to_string()),
            auth_type: AuthType::OAuth,
        })
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    let available = models.get_available(None, None).await.unwrap();
    assert_eq!(
        available
            .iter()
            .map(|model| model.provider.clone())
            .collect::<Vec<_>>(),
        vec!["ambient".to_string(), "oauth".to_string()]
    );
    let available = models.get_available(Some("ambient"), None).await.unwrap();
    assert_eq!(
        available
            .iter()
            .map(|model| model.provider.clone())
            .collect::<Vec<_>>(),
        vec!["ambient".to_string()]
    );
}

#[tokio::test]
async fn runs_provider_login_and_logout_through_the_credential_store() {
    let credentials = InMemoryCredentialStore::new();
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::api_key(Arc::new(EnvKeyAuth {
            key: None,
            login_credential: Some(ApiKeyCredential {
                key: Some("logged-in".to_string()),
                env: None,
            }),
        }))),
        ..Default::default()
    }));

    let credential = models
        .login(
            "p1",
            AuthType::ApiKey,
            Arc::new(UnusedInteraction { signal: None }),
        )
        .await
        .unwrap();
    assert_eq!(
        credential,
        Credential::ApiKey(ApiKeyCredential {
            key: Some("logged-in".to_string()),
            env: None,
        })
    );
    assert_eq!(
        credentials.read("p1", None).await.unwrap(),
        Some(credential)
    );

    models.logout("p1", None).await.unwrap();
    assert_eq!(credentials.read("p1", None).await.unwrap(), None);
}

#[tokio::test]
async fn refreshes_expired_oauth_credentials_and_persists_the_rotated_credential() {
    let credentials = InMemoryCredentialStore::new();
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some(
                Arc::new(|credential: OAuthCredential, _signal: CancellationToken| {
                    Box::pin(async move {
                        Ok(OAuthCredential {
                            access: "new-token".to_string(),
                            expires: pi_core::ai::auth::resolve::now_millis() + 60 * 60_000,
                            ..credential
                        })
                    }) as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                }) as OAuthRefreshOverride,
            ),
        }))),
        ..Default::default()
    }));
    store_credential(&credentials, "p1", oauth_credential("old-token", "r", 0)).await;

    let resolution = models
        .get_auth("p1", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(resolution.auth.api_key.as_deref(), Some("new-token"));
    match credentials.read("p1", None).await.unwrap() {
        Some(Credential::OAuth(credential)) => assert_eq!(credential.access, "new-token"),
        other => panic!("expected a stored oauth credential, got {other:?}"),
    }
}

#[tokio::test]
async fn refreshes_oauth_credentials_with_less_than_five_minutes_remaining() {
    let credentials = InMemoryCredentialStore::new();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some({
                let refreshes = Arc::clone(&refreshes);
                Arc::new(
                    move |credential: OAuthCredential, _signal: CancellationToken| {
                        let refreshes = Arc::clone(&refreshes);
                        Box::pin(async move {
                            refreshes.fetch_add(1, Ordering::SeqCst);
                            Ok(OAuthCredential {
                                access: "new-token".to_string(),
                                expires: pi_core::ai::auth::resolve::now_millis() + 60 * 60_000,
                                ..credential
                            })
                        })
                            as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                    },
                ) as OAuthRefreshOverride
            }),
        }))),
        ..Default::default()
    }));
    store_credential(
        &credentials,
        "p1",
        oauth_credential(
            "old-token",
            "r",
            pi_core::ai::auth::resolve::now_millis() + 60_000,
        ),
    )
    .await;

    let resolution = models
        .get_auth("p1", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(resolution.auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn honors_a_callers_longer_oauth_minimum_validity() {
    let credentials = InMemoryCredentialStore::new();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some({
                let refreshes = Arc::clone(&refreshes);
                Arc::new(
                    move |credential: OAuthCredential, _signal: CancellationToken| {
                        let refreshes = Arc::clone(&refreshes);
                        Box::pin(async move {
                            refreshes.fetch_add(1, Ordering::SeqCst);
                            Ok(OAuthCredential {
                                access: "new-token".to_string(),
                                expires: pi_core::ai::auth::resolve::now_millis() + 60 * 60_000,
                                ..credential
                            })
                        })
                            as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                    },
                ) as OAuthRefreshOverride
            }),
        }))),
        ..Default::default()
    }));
    store_credential(
        &credentials,
        "p1",
        oauth_credential(
            "old-token",
            "r",
            pi_core::ai::auth::resolve::now_millis() + 10 * 60_000,
        ),
    )
    .await;

    let resolution = models
        .get_auth(
            "p1",
            None,
            Some(&AuthResolutionOverrides {
                min_oauth_validity_ms: Some(30 * 60_000),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(resolution.auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejects_with_code_oauth_when_refresh_fails_preserving_the_stored_credential() {
    let credentials = InMemoryCredentialStore::new();
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some(
                Arc::new(|_credential: OAuthCredential, _signal: CancellationToken| {
                    Box::pin(std::future::ready(Err(AuthStorageError(
                        "invalid_grant".to_string(),
                    ))))
                        as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                }) as OAuthRefreshOverride,
            ),
        }))),
        ..Default::default()
    }));
    store_credential(&credentials, "p1", oauth_credential("old", "r", 0)).await;

    match models.get_auth("p1", None, None).await.unwrap_err() {
        ResolveError::Models(error) => assert_eq!(error.code, ModelsErrorCode::OAuth),
        other => panic!("expected a models error, got {other:?}"),
    }
    // Credential preserved for retry / re-login.
    match credentials.read("p1", None).await.unwrap() {
        Some(Credential::OAuth(credential)) => assert_eq!(credential.access, "old"),
        other => panic!("expected a stored oauth credential, got {other:?}"),
    }
}

#[tokio::test]
async fn serializes_concurrent_oauth_refreshes_through_store_modify() {
    let credentials = InMemoryCredentialStore::new();
    store_credential(&credentials, "p1", oauth_credential("old", "r1", 0)).await;

    let refreshes = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some({
                let refreshes = Arc::clone(&refreshes);
                Arc::new(
                    move |_credential: OAuthCredential, _signal: CancellationToken| {
                        let refreshes = Arc::clone(&refreshes);
                        Box::pin(async move {
                            let count = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Ok(OAuthCredential {
                                access: format!("new-{count}"),
                                refresh: "r2".to_string(),
                                expires: pi_core::ai::auth::resolve::now_millis() + 60 * 60_000,
                                extra: Default::default(),
                            })
                        })
                            as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                    },
                ) as OAuthRefreshOverride
            }),
        }))),
        ..Default::default()
    }));

    let (a, b) = tokio::join!(
        models.get_auth("p1", None, None),
        models.get_auth("p1", None, None),
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(a.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
    assert_eq!(b.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
}

#[tokio::test]
async fn valid_oauth_tokens_resolve_without_touching_modify() {
    let base = InMemoryCredentialStore::new();
    let modifies = Arc::new(AtomicUsize::new(0));
    let credentials = CountingCredentialStore {
        base: base.clone(),
        modifies: Arc::clone(&modifies),
    };
    store_credential(
        &base,
        "p1",
        oauth_credential(
            "valid",
            "r",
            pi_core::ai::auth::resolve::now_millis() + 10 * 60_000,
        ),
    )
    .await;
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth::default()))),
        ..Default::default()
    }));

    let resolution = models
        .get_auth("p1", None, None)
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(resolution.auth.api_key.as_deref(), Some("valid"));
    assert_eq!(
        modifies.load(Ordering::SeqCst),
        0,
        "valid tokens must not go through modify"
    );
}

#[tokio::test]
async fn wraps_credential_store_failures_in_models_error() {
    // read failure
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(ReadFailingCredentialStore) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(Some("env-key")))),
        ..Default::default()
    }));
    match models.get_auth("p1", None, None).await.unwrap_err() {
        ResolveError::Models(error) => assert_eq!(error.code, ModelsErrorCode::Auth),
        other => panic!("expected a models error, got {other:?}"),
    }

    // modify failure during refresh
    let oauth_models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(ModifyFailingCredentialStore {
            stored: oauth_credential("old", "r", 0),
        }) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    oauth_models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth::default()))),
        ..Default::default()
    }));
    match oauth_models.get_auth("p1", None, None).await.unwrap_err() {
        ResolveError::Models(error) => assert_eq!(error.code, ModelsErrorCode::Auth),
        other => panic!("expected a models error, got {other:?}"),
    }
}

#[tokio::test]
async fn keeps_the_underlying_reason_in_wrapped_oauth_refresh_errors() {
    let credentials = InMemoryCredentialStore::new();
    store_credential(&credentials, "p1", oauth_credential("old", "r", 0)).await;
    let models = Arc::new(Models::new(CreateModelsOptions {
        credentials: Some(Arc::new(credentials.clone()) as Arc<dyn CredentialStore>),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::oauth(Arc::new(TestOAuth {
            refresh: Some(
                Arc::new(|_credential: OAuthCredential, _signal: CancellationToken| {
                    Box::pin(std::future::ready(Err(AuthStorageError(
                        "token refresh failed (400): invalid_grant".to_string(),
                    ))))
                        as AuthFuture<Result<OAuthCredential, AuthStorageError>>
                }) as OAuthRefreshOverride,
            ),
        }))),
        ..Default::default()
    }));

    match models.get_auth("p1", None, None).await.unwrap_err() {
        ResolveError::Models(error) => assert_eq!(
            error.message,
            "OAuth refresh failed for p1: token refresh failed (400): invalid_grant"
        ),
        other => panic!("expected a models error, got {other:?}"),
    }
}

#[tokio::test]
async fn wraps_api_key_auth_failures_in_models_error() {
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::api_key(Arc::new(FailingApiKeyAuth))),
        ..Default::default()
    }));
    match models.get_auth("p1", None, None).await.unwrap_err() {
        ResolveError::Models(error) => {
            assert_eq!(error.code, ModelsErrorCode::Auth);
            assert_eq!(error.message, "API key auth failed for provider p1: nope");
        }
        other => panic!("expected a models error, got {other:?}"),
    }
}

#[tokio::test]
async fn adds_model_headers_only_for_model_auth_and_transforms_assembled_headers_once() {
    let calls = Arc::new(Mutex::new(Vec::<ProviderCall>::new()));
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1".to_string(),
        auth: Some(ProviderAuth::api_key(env_key_auth(Some("key")))),
        calls: Some(Arc::clone(&calls)),
        ..Default::default()
    }));
    let mut model = runtime_test_model("p1", "model-a");
    model.headers = Some(BTreeMap::from([
        ("x-model".to_string(), "model".to_string()),
        ("x-shared".to_string(), "model".to_string()),
    ]));

    assert_eq!(
        models
            .get_auth("p1", None, None)
            .await
            .unwrap()
            .unwrap()
            .auth
            .headers,
        None
    );
    let expected_model_headers: ProviderHeaders = BTreeMap::from([
        ("x-model".to_string(), Some("model".to_string())),
        ("x-shared".to_string(), Some("model".to_string())),
    ]);
    assert_eq!(
        models
            .get_auth("p1", Some(&model), None)
            .await
            .unwrap()
            .unwrap()
            .auth
            .headers,
        Some(expected_model_headers)
    );

    let transforms = Arc::new(AtomicUsize::new(0));
    let transform: TransformHeadersFn = {
        let transforms = Arc::clone(&transforms);
        Arc::new(move |headers| {
            transforms.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                headers,
                BTreeMap::from([
                    ("X-Shared".to_string(), Some("explicit".to_string())),
                    ("x-explicit".to_string(), Some("explicit".to_string())),
                    ("x-model".to_string(), Some("model".to_string())),
                ])
            );
            Box::pin(async move {
                let mut headers = headers;
                headers.insert("x-transformed".to_string(), Some("yes".to_string()));
                headers
            })
        })
    };
    let result = models
        .complete_simple(
            &model,
            &user_context("hi"),
            Some(SimpleStreamOptions {
                base: StreamOptions {
                    base: ProviderRequestOptions {
                        headers: Some(BTreeMap::from([
                            ("x-explicit".to_string(), Some("explicit".to_string())),
                            ("X-Shared".to_string(), Some("explicit".to_string())),
                        ])),
                        ..Default::default()
                    },
                    transform_headers: Some(transform),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(transforms.load(Ordering::SeqCst), 1);
    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let options = recorded[0].options.as_ref().unwrap();
    assert_eq!(
        options.base.headers.as_ref().unwrap(),
        &BTreeMap::from([
            ("X-Shared".to_string(), Some("explicit".to_string())),
            ("x-explicit".to_string(), Some("explicit".to_string())),
            ("x-model".to_string(), Some("model".to_string())),
            ("x-transformed".to_string(), Some("yes".to_string())),
        ])
    );
    assert!(options.transform_headers.is_none());
}
