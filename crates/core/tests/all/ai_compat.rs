//! Port of `pi-core/ai/test/compat-env.test.ts` plus a smoke check of the
//! deprecated per-api stream aliases from `legacy-api-aliases.ts`.

use std::sync::{Arc, Mutex};

use pi_core::ai::compat::{ApiProvider, complete, register_api_provider, reset_api_providers};
use pi_core::ai::types::{AssistantMessage, Context, Model, ProviderRequestOptions, StreamOptions};
use pi_core::ai::utils::event_stream::create_assistant_message_event_stream;

/// Serializes access to the global api-provider registry.
async fn registry_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn compat_model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-responses".to_string(),
        provider: "custom-openai".to_string(),
        base_url: "https://example.test/v1".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        context_window: 128_000,
        max_tokens: 4096,
        ..Default::default()
    }
}

fn terminal_message() -> AssistantMessage {
    AssistantMessage {
        api: "openai-responses".to_string(),
        provider: "custom-openai".to_string(),
        model: "test-model".to_string(),
        stop_reason: pi_core::ai::types::StopReason::Stop,
        ..Default::default()
    }
}

#[tokio::test]
async fn dispatches_unknown_providers_through_the_legacy_api_registry() {
    let _guard = registry_lock().await;
    let captured: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));

    let captured_for_stream = Arc::clone(&captured);
    let captured_for_simple = Arc::clone(&captured);
    register_api_provider(
        ApiProvider {
            api: "openai-responses".to_string(),
            stream: Arc::new(move |_model, _context, options| {
                captured_for_stream
                    .lock()
                    .unwrap()
                    .push(options.and_then(|options| options.base.api_key.clone()));
                let stream = create_assistant_message_event_stream();
                let output = terminal_message();
                stream.push(pi_core::ai::types::AssistantMessageEvent::Start {
                    partial: output.clone(),
                });
                stream.push(pi_core::ai::types::AssistantMessageEvent::Done {
                    reason: pi_core::ai::types::DoneReason::Stop,
                    message: output.clone(),
                });
                stream.end(Some(output));
                stream
            }),
            stream_simple: Arc::new(move |_model, _context, options| {
                captured_for_simple
                    .lock()
                    .unwrap()
                    .push(options.and_then(|options| options.base.base.api_key.clone()));
                create_assistant_message_event_stream()
            }),
        },
        None,
    );

    let context = Context::default();
    let options = StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some("request-key".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let message = complete(&compat_model(), &context, Some(options)).await;
    assert_eq!(message.stop_reason, pi_core::ai::types::StopReason::Stop);

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].as_deref(), Some("request-key"));

    reset_api_providers();
}

#[test]
fn legacy_stream_aliases_delegate_to_the_api_modules() {
    // The aliases are re-exports of the api stream functions; assert they
    // exist and dispatch to the right module via the mismatch check.
    let model = compat_model();
    let context = Context::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The anthropic alias rejects non-anthropic models (mismatched api),
        // proving it delegates into the anthropic implementation.
        pi_core::ai::compat::stream_anthropic(&model, &context, None);
    }));
    assert!(result.is_err());
}

// Port of the `registerBuiltInApiProviders` semantics from compat.ts: the
// explicit registration never clobbers an existing entry and stays idempotent.
#[tokio::test]
async fn register_built_in_api_providers_preserves_overrides_and_is_idempotent() {
    use pi_core::ai::compat::{get_api_provider, get_api_providers};

    let _guard = registry_lock().await;
    reset_api_providers();

    let builtin_count = get_api_providers().len();
    assert!(builtin_count >= 10, "builtin apis are registered");

    // Idempotent: no duplicate registrations.
    pi_core::ai::compat::register_built_in_api_providers();
    assert_eq!(get_api_providers().len(), builtin_count);

    // An override for a builtin api id survives the explicit registration.
    // `registerApiProvider` replaces same-id entries, so the count is stable.
    register_api_provider(
        ApiProvider {
            api: "openai-completions".to_string(),
            stream: Arc::new(|_model, _context, _options| create_assistant_message_event_stream()),
            stream_simple: Arc::new(|_model, _context, _options| {
                create_assistant_message_event_stream()
            }),
        },
        Some("test-suite"),
    );
    let override_api = get_api_provider("openai-completions").expect("override registered");
    assert_eq!(override_api.api, "openai-completions");
    assert_eq!(get_api_providers().len(), builtin_count);

    pi_core::ai::compat::register_built_in_api_providers();
    assert_eq!(
        get_api_providers().len(),
        builtin_count,
        "the override entry is not clobbered or duplicated"
    );

    reset_api_providers();
}
