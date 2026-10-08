//! Port of `pi-core/ai/test/cloudflare-stream.test.ts`.

use std::sync::{Arc, Mutex};

use pi_core::ai::models::ProviderStreams;
use pi_core::ai::providers::cloudflare_stream::cloudflare_streams;
use pi_core::ai::types::{
    Context, Model, ModelInput, ProviderEnv, ProviderRequestOptions, SimpleStreamOptions,
    StreamOptions,
};
use pi_core::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

fn test_model() -> Model {
    Model {
        id: "model".to_string(),
        name: "model".to_string(),
        api: "openai-completions".to_string(),
        provider: "cloudflare-ai-gateway".to_string(),
        base_url: "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 1000,
        max_tokens: 100,
        ..Default::default()
    }
}

/// The inline fake from the TS test: records the base URL each method sees.
#[derive(Default)]
struct CapturingStreams {
    captured: Mutex<Vec<String>>,
}

impl ProviderStreams for CapturingStreams {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.captured.lock().unwrap().push(model.base_url.clone());
        create_assistant_message_event_stream()
    }

    fn stream_simple(
        &self,
        model: &Model,
        _context: &Context,
        _options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.captured.lock().unwrap().push(model.base_url.clone());
        create_assistant_message_event_stream()
    }
}

fn env_from(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn materializes_the_model_endpoint_before_dispatch() {
    let streams = Arc::new(CapturingStreams::default());
    let wrapper = cloudflare_streams(Arc::clone(&streams) as Arc<dyn ProviderStreams>);
    let model = test_model();
    let context = Context::default();
    let env = env_from(&[
        ("CLOUDFLARE_ACCOUNT_ID", "account"),
        ("CLOUDFLARE_GATEWAY_ID", "gateway"),
    ]);
    let stream_options = StreamOptions {
        base: ProviderRequestOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let simple_options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env: Some(env),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = wrapper.stream(&model, &context, Some(&stream_options));
    let _ = wrapper.stream_simple(&model, &context, Some(&simple_options));

    let captured = streams.captured.lock().unwrap().clone();
    assert_eq!(
        captured,
        vec![
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai".to_string(),
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai".to_string(),
        ]
    );
}

#[test]
fn keeps_placeholders_when_the_provider_env_does_not_resolve_them() {
    let streams = Arc::new(CapturingStreams::default());
    let wrapper = cloudflare_streams(Arc::clone(&streams) as Arc<dyn ProviderStreams>);
    let model = test_model();
    let context = Context::default();
    let options = SimpleStreamOptions::default();

    let _ = wrapper.stream_simple(&model, &context, Some(&options));

    let captured = streams.captured.lock().unwrap().clone();
    assert_eq!(captured, vec![model.base_url.clone()]);
}

/// Covers the TS `env[name] ?? "{...}"` fallback: a present env map that
/// lacks an entry leaves that placeholder in the base URL.
#[test]
fn keeps_missing_env_entries_as_placeholders() {
    let streams = Arc::new(CapturingStreams::default());
    let wrapper = cloudflare_streams(Arc::clone(&streams) as Arc<dyn ProviderStreams>);
    let model = test_model();
    let context = Context::default();
    let options = StreamOptions {
        base: ProviderRequestOptions {
            env: Some(env_from(&[("CLOUDFLARE_ACCOUNT_ID", "account")])),
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = wrapper.stream(&model, &context, Some(&options));

    let captured = streams.captured.lock().unwrap().clone();
    assert_eq!(
        captured,
        vec![
            "https://gateway.ai.cloudflare.com/v1/account/{CLOUDFLARE_GATEWAY_ID}/openai"
                .to_string()
        ]
    );
}

// Direct coverage of the now-public `resolveCloudflareModel` port: env values
// replace placeholders, absent entries keep them, and an unchanged base URL
// returns the model untouched.
#[test]
fn resolve_cloudflare_model_substitutes_env_placeholders() {
    use pi_core::ai::providers::cloudflare_stream::resolve_cloudflare_model;

    let model = test_model();

    let resolved = resolve_cloudflare_model(
        model.clone(),
        Some(&env_from(&[
            ("CLOUDFLARE_ACCOUNT_ID", "acct"),
            ("CLOUDFLARE_GATEWAY_ID", "gw"),
        ])),
    );
    assert_eq!(
        resolved.base_url,
        "https://gateway.ai.cloudflare.com/v1/acct/gw/openai"
    );

    let passthrough = resolve_cloudflare_model(model.clone(), None);
    assert_eq!(passthrough.base_url, model.base_url);
}
