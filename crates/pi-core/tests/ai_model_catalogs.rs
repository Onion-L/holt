//! Ports of the generated model-catalog metadata suites:
//!
//! - `baseten-models.test.ts` (catalog cases; the `chat_template_args`
//!   payload cases live in `tests/ai_openai_completions.rs`)
//! - `fireworks-models.test.ts` (catalog cases; the Anthropic payload
//!   describe lives in `tests/ai_anthropic_payload.rs`)
//! - `together-models.test.ts` (3 cases)
//! - `xiaomi-models.test.ts` (2 it.each blocks x 4 providers)
//! - `zai-coding-plan-models.test.ts` (3 cases)
//! - `model-catalog-types.test.ts` (the runtime case; the `expectTypeOf`
//!   half is a compile-time TypeScript type-level check — N/A in Rust)
//! - `openrouter-cache-control-models.test.ts` (1 block, it.each x 4)
//! - `model-data-validation.test.ts` (ported as embedded-catalog integrity
//!   checks; the dir-reading/manifest-hash/script-stamp mechanics are
//!   TypeScript codegen-script concerns — N/A, see the section comment)
//!
//! The TS suites read the generated catalogs (`getModel`/`getModels`/
//! `getBuiltinModel`); the Rust port reads the same data through the
//! embedded catalog (`src/ai/data/models.generated.json`, exported by the
//! TypeScript oracle via `scripts/oracle/export-model-catalog.mts`). The
//! env cases set `process.env` in TS; the port injects a scoped
//! `ProviderEnv` instead of mutating process env. The two Fireworks payload
//! assertions (`prompt_cache_retention`, `reasoning_effort`) capture the
//! outgoing JSON body through a mock `HttpFetch` transport, which is the
//! same observable surface as the TS `onPayload` hook.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use pi_core::ai::api::openai_completions::stream_simple;
use pi_core::ai::env_api_keys::{find_env_keys, get_env_api_key};
use pi_core::ai::models::get_supported_thinking_levels;
use pi_core::ai::types::{
    CacheControlFormat, CacheRetention, ChatTemplateKwargValue, ChatTemplateKwargs, Context,
    DeferredToolsMode, MaxTokensField, Message, Model, ModelCompat, ModelCost, ModelCostRates,
    ModelInput, ModelThinkingLevel, ProviderEnv, ProviderRequestOptions, SimpleStreamOptions,
    StreamOptions, ThinkingFormat, ThinkingLevel, ThinkingLevelMap, ThinkingVariable, UserContent,
    UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Port of the TS `getModel`/`getBuiltinModel` catalog lookups (both collapse
/// to the same built-in catalog read in Rust).
fn get_model(provider: &str, model_id: &str) -> Model {
    pi_core::ai::providers::builtin::get_builtin_model(provider, model_id)
        .unwrap_or_else(|| panic!("missing model {provider}/{model_id}"))
}

/// Port of the TS `getModels(provider)` catalog read.
fn get_models(provider: &str) -> Vec<Model> {
    pi_core::ai::providers::builtin::get_builtin_models(provider)
}

/// The `cost: { input, output, cacheRead, cacheWrite }` fixture shape.
fn cost(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelCost {
    ModelCost {
        rates: ModelCostRates {
            input: input.into(),
            output: output.into(),
            cache_read: cache_read.into(),
            cache_write: cache_write.into(),
        },
        tiers: None,
    }
}

/// Builds a `thinkingLevelMap` from explicit entries (absent keys stay
/// absent, `None` marks a level unsupported with a `null` value).
fn level_map(entries: &[(ModelThinkingLevel, Option<&str>)]) -> ThinkingLevelMap {
    entries
        .iter()
        .map(|(level, value)| (*level, value.map(str::to_string)))
        .collect()
}

fn chat_template_args(pairs: &[(&str, ChatTemplateKwargValue)]) -> ChatTemplateKwargs {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

/// `{ enable_thinking: { $var: "thinking.enabled" } }`.
fn enable_thinking_var() -> ChatTemplateKwargValue {
    ChatTemplateKwargValue::Variable {
        variable: ThinkingVariable::Enabled,
        omit_when_off: None,
    }
}

/// Scoped env standing in for the TS `process.env.X = ...` mutations.
fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn test_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("test".to_string()),
            timestamp: 0,
            role: pi_core::ai::types::RoleUser,
        })],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Mock openai-completions transport (copied from tests/ai_openai_completions.rs)
// ---------------------------------------------------------------------------

/// The chunk yielded by every TS SDK mock / `onPayload` capture.
fn ok_chunk() -> Value {
    json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 0},
        },
    })
}

fn chunk_events(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

/// Mock transport returning canned SSE data and capturing requests.
struct RecordingFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl RecordingFetch {
    fn new(body: String) -> Self {
        Self {
            body,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request(&self) -> HttpRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("request captured")
    }
}

impl HttpFetch for RecordingFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

/// The `streamSimple` + `onPayload` capture of the TS suites: returns the
/// JSON payload that left the process.
async fn capture_simple_payload(model: &Model, options: SimpleStreamOptions) -> Value {
    let fetch = Arc::new(RecordingFetch::new(chunk_events(&[ok_chunk()])));
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                fetch: Some(fetch.clone()),
                ..options.base.base
            },
            ..options.base
        },
        ..options
    };
    let _ = stream_simple(model, &test_context(), Some(&options))
        .result()
        .await;
    match &fetch.request().body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    }
}

// ---------------------------------------------------------------------------
// Baseten models (baseten-models.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn registers_glm_5_2_as_the_default_openai_compatible_reasoning_model() {
    let model = get_model("baseten", "zai-org/GLM-5.2");

    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.provider, "baseten");
    assert_eq!(model.base_url, "https://inference.baseten.co/v1");
    assert!(model.reasoning);
    assert_eq!(
        model.thinking_level_map,
        Some(level_map(&[
            (ModelThinkingLevel::Off, Some("none")),
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, None),
            (ModelThinkingLevel::High, Some("high")),
            (ModelThinkingLevel::Xhigh, None),
            (ModelThinkingLevel::Max, Some("max")),
        ]))
    );
    assert_eq!(model.input, vec![ModelInput::Text]);
    assert_eq!(model.context_window, 1_048_576);
    assert_eq!(model.max_tokens, 262_144);
    assert_eq!(model.cost, cost(1.4, 4.4, 0.3, 0.0));

    // The TS `toMatchObject` checks this compat subset.
    let compat = model.compat.expect("baseten GLM-5.2 compat");
    assert_eq!(compat.supports_store, Some(false));
    assert_eq!(compat.supports_developer_role, Some(false));
    assert_eq!(compat.supports_reasoning_effort, Some(true));
    assert_eq!(compat.supports_usage_in_streaming, Some(true));
    assert_eq!(compat.max_tokens_field, Some(MaxTokensField::MaxTokens));
    assert_eq!(compat.supports_strict_mode, Some(true));
    assert_eq!(compat.supports_long_cache_retention, Some(false));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Baseten));
    assert_eq!(
        compat.chat_template_args,
        Some(chat_template_args(&[(
            "enable_thinking",
            enable_thinking_var()
        )]))
    );
}

#[test]
fn models_kimi_k2_6_reasoning_as_an_explicit_off_on_toggle() {
    let model = get_model("baseten", "moonshotai/Kimi-K2.6");

    assert_eq!(
        model.thinking_level_map,
        Some(level_map(&[
            (ModelThinkingLevel::Off, Some("off")),
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, None),
            (ModelThinkingLevel::High, Some("high")),
            (ModelThinkingLevel::Xhigh, None),
            (ModelThinkingLevel::Max, None),
        ]))
    );
    let compat = model.compat.clone().expect("baseten Kimi K2.6 compat");
    assert_eq!(compat.supports_reasoning_effort, Some(false));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Baseten));
    assert_eq!(
        compat.chat_template_args,
        Some(chat_template_args(&[(
            "enable_thinking",
            enable_thinking_var()
        )]))
    );
    assert_eq!(
        get_supported_thinking_levels(&model),
        vec![ModelThinkingLevel::Off, ModelThinkingLevel::High]
    );

    // The trailing `streamSimple` payload assertions (`chat_template_args`
    // / `reasoning_effort`) are the Baseten payload cases ported in
    // tests/ai_openai_completions.rs.
}

#[test]
fn resolves_baseten_api_key_from_the_environment() {
    let env = scoped_env(&[("BASETEN_API_KEY", "test-baseten-key")]);

    assert_eq!(
        find_env_keys("baseten", Some(&env)),
        Some(vec!["BASETEN_API_KEY".to_string()])
    );
    assert_eq!(
        get_env_api_key("baseten", Some(&env)),
        Some("test-baseten-key".to_string())
    );
}

// ---------------------------------------------------------------------------
// Fireworks models (fireworks-models.test.ts, catalog cases)
// ---------------------------------------------------------------------------

#[test]
fn registers_the_default_kimi_k2_6_model_via_anthropic_compatible_messages_api() {
    let model = get_model("fireworks", "accounts/fireworks/models/kimi-k2p6");

    assert_eq!(model.api, "anthropic-messages");
    assert_eq!(model.provider, "fireworks");
    assert_eq!(model.base_url, "https://api.fireworks.ai/inference");
    assert!(model.reasoning);
    assert_eq!(model.input, vec![ModelInput::Text, ModelInput::Image]);
    assert_eq!(model.context_window, 262_000);
    assert_eq!(model.max_tokens, 262_000);
    assert_eq!(model.cost, cost(0.95, 4.0, 0.16, 0.0));
}

#[test]
fn aligns_glm_5_2_fast_with_glm_5_2s_openai_compatible_config() {
    let base = get_model("fireworks", "accounts/fireworks/models/glm-5p2");
    let fast = get_model("fireworks", "accounts/fireworks/routers/glm-5p2-fast");

    assert_eq!(fast.api, base.api);
    assert_eq!(fast.base_url, base.base_url);
    assert_eq!(fast.compat, base.compat);
    assert_eq!(fast.thinking_level_map, base.thinking_level_map);
}

#[tokio::test]
async fn omits_unsupported_long_cache_retention_for_glm_5p2() {
    for model_id in [
        "accounts/fireworks/models/glm-5p2",
        "accounts/fireworks/routers/glm-5p2-fast",
    ] {
        let model = get_model("fireworks", model_id);
        let payload = capture_simple_payload(
            &model,
            SimpleStreamOptions {
                base: StreamOptions {
                    base: ProviderRequestOptions {
                        api_key: Some("test-fireworks-key".to_string()),
                        ..Default::default()
                    },
                    cache_retention: Some(CacheRetention::Long),
                    session_id: Some("test-fireworks-session".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;

        assert!(payload.get("prompt_cache_retention").is_none());
    }
}

#[tokio::test]
async fn routes_kimi_k3_through_the_openai_compatible_api_with_native_effort_controls() {
    let base = get_model("fireworks", "accounts/fireworks/models/kimi-k3");
    let fast = get_model("fireworks", "accounts/fireworks/routers/kimi-k3-fast");
    let expected_compat = ModelCompat {
        supports_store: Some(false),
        supports_developer_role: Some(false),
        requires_reasoning_content_on_assistant_messages: Some(true),
        thinking_format: Some(ThinkingFormat::Openai),
        deferred_tools_mode: Some(DeferredToolsMode::Kimi),
        send_session_affinity_headers: Some(true),
        supports_long_cache_retention: Some(false),
        ..Default::default()
    };
    let expected_map = level_map(&[
        (ModelThinkingLevel::Off, None),
        (ModelThinkingLevel::Minimal, None),
        (ModelThinkingLevel::Low, Some("low")),
        (ModelThinkingLevel::Medium, Some("medium")),
        (ModelThinkingLevel::High, Some("high")),
        (ModelThinkingLevel::Xhigh, None),
        (ModelThinkingLevel::Max, Some("max")),
    ]);

    assert_eq!(base.api, "openai-completions");
    assert_eq!(base.base_url, "https://api.fireworks.ai/inference/v1");
    assert_eq!(base.compat, Some(expected_compat.clone()));
    assert_eq!(base.thinking_level_map, Some(expected_map.clone()));
    assert_eq!(fast.api, base.api);
    assert_eq!(fast.base_url, base.base_url);
    assert_eq!(fast.compat, Some(expected_compat));
    assert_eq!(fast.thinking_level_map, Some(expected_map));

    let payload = capture_simple_payload(
        &base,
        SimpleStreamOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("test-fireworks-key".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
            reasoning: Some(ThinkingLevel::Max),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(payload["reasoning_effort"], json!("max"));
}

#[test]
fn resolves_fireworks_api_key_from_the_environment() {
    let env = scoped_env(&[("FIREWORKS_API_KEY", "test-fireworks-key")]);

    assert_eq!(
        find_env_keys("fireworks", Some(&env)),
        Some(vec!["FIREWORKS_API_KEY".to_string()])
    );
    assert_eq!(
        get_env_api_key("fireworks", Some(&env)),
        Some("test-fireworks-key".to_string())
    );
}

#[test]
fn sets_fireworks_specific_compat_for_session_affinity_and_unsupported_tool_fields() {
    let model = get_model("fireworks", "accounts/fireworks/models/kimi-k2p6");

    let compat = model.compat.expect("fireworks kimi-k2p6 compat");
    assert_eq!(compat.send_session_affinity_headers, Some(true));
    assert_eq!(compat.supports_eager_tool_input_streaming, Some(false));
    assert_eq!(compat.supports_cache_control_on_tools, Some(false));
    assert_eq!(compat.supports_long_cache_retention, Some(false));
}

// ---------------------------------------------------------------------------
// Together models (together-models.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn registers_the_default_kimi_k2_6_model_via_openai_compatible_chat_completions_api() {
    let model = get_model("together", "moonshotai/Kimi-K2.6");

    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.provider, "together");
    assert_eq!(model.base_url, "https://api.together.ai/v1");
    assert!(model.reasoning);
    assert_eq!(
        model.thinking_level_map,
        Some(level_map(&[
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, None),
        ]))
    );
    assert_eq!(model.input, vec![ModelInput::Text, ModelInput::Image]);
    assert_eq!(model.context_window, 262_144);
    assert_eq!(model.max_tokens, 131_000);
    assert_eq!(model.cost, cost(1.2, 4.5, 0.2, 0.0));
    assert_eq!(
        model.compat,
        Some(ModelCompat {
            supports_store: Some(false),
            supports_developer_role: Some(false),
            supports_reasoning_effort: Some(false),
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            thinking_format: Some(ThinkingFormat::Together),
            supports_strict_mode: Some(false),
            supports_long_cache_retention: Some(false),
            ..Default::default()
        })
    );
}

#[test]
fn models_together_reasoning_controls_from_the_together_api_surface() {
    let gpt_oss = get_model("together", "openai/gpt-oss-120b");
    assert_eq!(
        gpt_oss.thinking_level_map,
        Some(level_map(&[
            (ModelThinkingLevel::Off, None),
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, Some("low")),
            (ModelThinkingLevel::Medium, Some("medium")),
            (ModelThinkingLevel::High, Some("high")),
            (ModelThinkingLevel::Max, None),
            (ModelThinkingLevel::Xhigh, None),
        ]))
    );
    let gpt_oss_compat = gpt_oss.compat.expect("gpt-oss-120b compat");
    assert_eq!(gpt_oss_compat.supports_reasoning_effort, Some(true));
    assert_eq!(gpt_oss_compat.thinking_format, Some(ThinkingFormat::Openai));

    let deep_seek_v4 = get_model("together", "deepseek-ai/DeepSeek-V4-Pro");
    assert_eq!(
        deep_seek_v4.thinking_level_map,
        Some(level_map(&[
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, None),
            (ModelThinkingLevel::High, Some("high")),
            (ModelThinkingLevel::Xhigh, None),
        ]))
    );
    let deep_seek_compat = deep_seek_v4.compat.expect("DeepSeek-V4-Pro compat");
    assert_eq!(deep_seek_compat.supports_reasoning_effort, Some(true));
    assert_eq!(
        deep_seek_compat.thinking_format,
        Some(ThinkingFormat::Together)
    );

    let minimax = get_model("together", "MiniMaxAI/MiniMax-M2.7");
    assert_eq!(
        minimax.thinking_level_map,
        Some(level_map(&[
            (ModelThinkingLevel::Off, None),
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, None),
        ]))
    );
    let minimax_compat = minimax.compat.expect("MiniMax-M2.7 compat");
    assert_eq!(minimax_compat.thinking_format, None);
    assert_eq!(minimax_compat.supports_reasoning_effort, Some(false));
}

#[test]
fn resolves_together_api_key_from_the_environment() {
    let env = scoped_env(&[("TOGETHER_API_KEY", "test-together-key")]);

    assert_eq!(
        find_env_keys("together", Some(&env)),
        Some(vec!["TOGETHER_API_KEY".to_string()])
    );
    assert_eq!(
        get_env_api_key("together", Some(&env)),
        Some("test-together-key".to_string())
    );
}

// ---------------------------------------------------------------------------
// Xiaomi MiMo models (xiaomi-models.test.ts)
// ---------------------------------------------------------------------------

const XIAOMI_PROVIDERS: [&str; 4] = [
    "xiaomi",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-sgp",
];
const DEPRECATED_MODEL_IDS: [&str; 3] = ["mimo-v2-flash", "mimo-v2-omni", "mimo-v2-pro"];
const REPLACEMENT_MODEL_IDS: [&str; 2] = ["mimo-v2.5", "mimo-v2.5-pro"];

#[test]
fn omits_deprecated_models_from_xiaomi_catalogs() {
    for provider in XIAOMI_PROVIDERS {
        let models = get_models(provider);
        let model_ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        for model_id in DEPRECATED_MODEL_IDS {
            assert!(
                !model_ids.contains(&model_id),
                "{provider} catalog must omit {model_id}"
            );
        }
    }
}

#[test]
fn keeps_replacement_models_on_xiaomi_catalogs() {
    for provider in XIAOMI_PROVIDERS {
        let models = get_models(provider);
        let model_ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        for model_id in REPLACEMENT_MODEL_IDS {
            assert!(
                model_ids.contains(&model_id),
                "{provider} catalog must keep {model_id}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ZAI Coding Plan models (zai-coding-plan-models.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn exposes_glm_4_6v_on_the_china_coding_plan_catalog() {
    let model = get_model("zai-coding-cn", "glm-4.6v");

    // The TS `toMatchObject` checks this subset.
    assert_eq!(model.id, "glm-4.6v");
    assert_eq!(model.provider, "zai-coding-cn");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(
        model.base_url,
        "https://open.bigmodel.cn/api/coding/paas/v4"
    );
    assert!(model.reasoning);
    assert_eq!(model.input, vec![ModelInput::Text, ModelInput::Image]);
    assert_eq!(model.cost, cost(0.3, 0.9, 0.0, 0.0));
    assert_eq!(model.context_window, 128_000);
    assert_eq!(model.max_tokens, 32_768);
    let compat = model.compat.expect("glm-4.6v compat");
    assert_eq!(compat.max_tokens_field, Some(MaxTokensField::MaxTokens));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Zai));
    assert_eq!(compat.zai_tool_stream, Some(true));
}

#[test]
fn uses_api_equivalent_reference_costs_for_coding_plan_models() {
    assert_eq!(get_model("zai", "glm-5.2").cost, cost(1.4, 4.4, 0.26, 0.0));
    assert_eq!(
        get_model("zai-coding-cn", "glm-5.1").cost,
        cost(1.4, 4.4, 0.26, 0.0)
    );
    assert_eq!(
        get_model("zai-coding-cn", "glm-5v-turbo").cost,
        cost(1.2, 4.0, 0.24, 0.0)
    );
    for provider in ["zai", "zai-coding-cn"] {
        assert_eq!(
            get_model(provider, "glm-5.3").cost,
            cost(1.4, 4.4, 0.26, 0.0),
            "{provider} glm-5.3 cost"
        );
    }
}

#[test]
fn keeps_zero_costs_for_coding_plan_models_without_a_matching_api_price() {
    let zero_cost = cost(0.0, 0.0, 0.0, 0.0);

    for provider in ["zai", "zai-coding-cn"] {
        assert_eq!(
            get_model(provider, "glm-5.2-highspeed").cost,
            zero_cost,
            "{provider} glm-5.2-highspeed cost"
        );
    }
}

// ---------------------------------------------------------------------------
// Model catalog types (model-catalog-types.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn routes_github_copilot_grok_4_5_through_the_responses_api() {
    // The `expectTypeOf` assertions (including the whole
    // "derives model API, ID, and provider literals" case over XAI_MODELS)
    // are compile-time TypeScript type-level checks with no runtime
    // behavior; only this runtime assertion is portable.
    let model = get_model("github-copilot", "grok-4.5");
    assert_eq!(model.api, "openai-responses");
}

// ---------------------------------------------------------------------------
// OpenRouter Anthropic cache control metadata
// (openrouter-cache-control-models.test.ts)
// ---------------------------------------------------------------------------

#[test]
fn enables_cache_control_for_anthropic_latest_models() {
    let model_ids = [
        "~anthropic/claude-fable-latest",
        "~anthropic/claude-haiku-latest",
        "~anthropic/claude-opus-latest",
        "~anthropic/claude-sonnet-latest",
    ];

    for model_id in model_ids {
        let model = get_model("openrouter", model_id);
        assert_eq!(
            model.compat.and_then(|compat| compat.cache_control_format),
            Some(CacheControlFormat::Anthropic),
            "{model_id} cacheControlFormat"
        );
    }
}

// ---------------------------------------------------------------------------
// Generated model data validation (model-data-validation.test.ts)
//
// The TS suite exercises `scripts/model-data.ts` (the codegen helper that
// reads the per-provider API-grouped JSON shards from disk and checks the
// manifest hashes/schema stamps). The Rust port embeds the generated output
// directly (`src/ai/data/models.generated.json`), so the dir-reading,
// manifest-hash, and generation-stamp mechanics are TypeScript
// codegen-script concerns with no Rust counterpart (N/A). The port keeps the
// validation intent as integrity checks over the embedded catalog.
// ---------------------------------------------------------------------------

fn raw_catalog_models() -> serde_json::Map<String, Value> {
    let raw: Value =
        serde_json::from_str(include_str!("../src/ai/data/models.generated.json")).unwrap();
    raw["models"].as_object().expect("models object").clone()
}

/// The API group keys the generator emits (the group names of the
/// TypeScript per-provider data shards).
const GENERATED_API_GROUPS: [&str; 9] = [
    "anthropic-messages",
    "azure-openai-responses",
    "bedrock-converse-stream",
    "google-generative-ai",
    "google-vertex",
    "mistral-conversations",
    "openai-codex-responses",
    "openai-completions",
    "openai-responses",
];

/// Port of the "rejects duplicate model IDs across API groups" intent: every
/// model id belongs to exactly one API group per provider.
#[test]
fn embedded_catalog_keeps_each_model_in_a_single_api_group() {
    let models = raw_catalog_models();
    assert!(!models.is_empty(), "contains generated model data");

    for (provider, entries) in &models {
        let models_of_provider = entries.as_object().expect("provider entries object");
        assert!(
            !models_of_provider.is_empty(),
            "{provider} contains no generated model data"
        );
        let mut id_apis: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (model_id, model) in models_of_provider {
            id_apis
                .entry(model_id)
                .or_default()
                .insert(model["api"].as_str().expect("api string"));
        }
        for (model_id, apis) in id_apis {
            assert_eq!(
                apis.len(),
                1,
                "{provider}/{model_id} appears in more than one API group"
            );
        }
    }
}

/// Port of the "rejects a wrong model id/provider/api" intent
/// (`validateModelValue`): every model matches its group keys and the value
/// shape the generator guarantees.
#[test]
fn embedded_catalog_models_match_their_group() {
    let models = raw_catalog_models();

    for (provider, entries) in &models {
        for (model_id, model) in entries.as_object().expect("provider entries object") {
            let label = format!("{provider}/{model_id}");
            assert!(model.is_object(), "{label} must be an object");
            assert_eq!(
                model.get("id").and_then(Value::as_str),
                Some(model_id.as_str()),
                "{label} has id {:?}, expected {model_id:?}",
                model.get("id")
            );
            assert_eq!(
                model.get("provider").and_then(Value::as_str),
                Some(provider.as_str()),
                "{label} has provider {:?}, expected {provider:?}",
                model.get("provider")
            );
            let api = model.get("api").and_then(Value::as_str);
            assert!(
                GENERATED_API_GROUPS.contains(&api.unwrap_or_default()),
                "{label} has api {api:?}, which is not a generated API group"
            );

            let name = model.get("name").and_then(Value::as_str);
            assert!(
                name.is_some_and(|name| !name.is_empty()),
                "{label} has no model name"
            );
            assert!(
                model.get("baseUrl").and_then(Value::as_str).is_some(),
                "{label} has no baseUrl string"
            );
            assert!(
                model.get("reasoning").and_then(Value::as_bool).is_some(),
                "{label} has no reasoning boolean"
            );
            let input = model.get("input").and_then(Value::as_array);
            assert!(
                input.is_some_and(|input| {
                    !input.is_empty()
                        && input
                            .iter()
                            .all(|entry| entry == &json!("text") || entry == &json!("image"))
                }),
                "{label} has invalid input modalities"
            );
            let context_window = model.get("contextWindow").and_then(Value::as_f64);
            assert!(
                context_window.is_some_and(|value| value.is_finite() && value > 0.0),
                "{label} has invalid contextWindow"
            );
            let max_tokens = model.get("maxTokens").and_then(Value::as_f64);
            assert!(
                max_tokens.is_some_and(|value| value.is_finite() && value > 0.0),
                "{label} has invalid maxTokens"
            );
            let cost = model.get("cost").and_then(Value::as_object);
            assert!(cost.is_some(), "{label} has invalid cost metadata");
            for field in ["input", "output", "cacheRead", "cacheWrite"] {
                assert!(
                    cost.and_then(|cost| cost.get(field))
                        .and_then(Value::as_f64)
                        .is_some_and(f64::is_finite),
                    "{label} has invalid cost.{field}"
                );
            }
        }
    }

    // The whole embedded file also parses into the typed catalog (unknown
    // api/group values would fail deserialization).
    let typed = pi_core::ai::models_generated::models();
    for (provider, entries) in &models {
        let typed_entries = typed
            .get(provider)
            .unwrap_or_else(|| panic!("provider {provider} missing from the typed catalog"));
        let entries = entries.as_object().expect("provider entries object");
        assert_eq!(typed_entries.len(), entries.len());
    }
}

/// Port of the exact generated allowlist assertions (`assertExactModelIds`):
/// the pinned id sets the generator enforces for representative providers.
#[test]
fn embedded_catalog_matches_exact_generated_allowlists() {
    // qwen-token-plan-individual is pinned by QWEN_TOKEN_PLAN_INDIVIDUAL_MODEL_IDS
    // in scripts/generate-models.ts (asserted via assertExactModelIds in
    // strict mode); xiaomi and zai-coding-cn pin the suites above.
    let allowlists = [
        (
            "qwen-token-plan-individual",
            vec![
                "deepseek-v4-flash-0731",
                "deepseek-v4-pro",
                "deepseek-v4-pro-0813",
                "glm-5.2",
                "qwen3.6-flash",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.8-flash",
                "qwen3.8-max",
            ],
        ),
        (
            "xiaomi",
            vec!["mimo-v2.5", "mimo-v2.5-pro", "mimo-v2.5-pro-ultraspeed"],
        ),
        (
            "zai-coding-cn",
            vec![
                "glm-4.6v",
                "glm-4.7",
                "glm-5-turbo",
                "glm-5.1",
                "glm-5.2",
                "glm-5.2-highspeed",
                "glm-5.3",
                "glm-5.3-flash",
                "glm-5.3-highspeed",
                "glm-5v-turbo",
            ],
        ),
    ];

    let models = raw_catalog_models();
    for (provider, expected) in allowlists {
        let entries = models
            .get(provider)
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("missing generated provider {provider}"));
        let actual: BTreeSet<&str> = entries.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = expected.into_iter().collect();
        let missing: Vec<&str> = expected.difference(&actual).copied().collect();
        let extra: Vec<&str> = actual.difference(&expected).copied().collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{provider} model IDs do not match (missing: {missing:?}; extra: {extra:?})"
        );
    }
}
