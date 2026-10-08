//! Port of `pi-core/ai/test/qwen-token-plan-models.test.ts`.
//!
//! The TypeScript suite captures request payloads through the `onPayload`
//! hook with the `openai` SDK client mocked; the Rust port runs the global
//! `streamSimple` (the compat face) against a mock `HttpFetch` transport and
//! asserts on the captured JSON body, which is the same observable surface.
//! The per-case `it.each` tables are looped inside one test function per
//! table, following the convention in `tests/ai_openai_completions.rs`. The
//! user-message `timestamp` (`Date.now()` in TS) is frozen to 0; it is not
//! part of any asserted payload.

use std::sync::{Arc, Mutex};

use pi_core::ai::compat::get_models;
use pi_core::ai::compat::stream_simple as compat_stream_simple;
use pi_core::ai::env_api_keys::find_env_keys;
use pi_core::ai::types::{
    Context, Message, Model, ModelThinkingLevel, ProviderEnv, ProviderRequestOptions, RoleUser,
    SimpleStreamOptions, StreamOptions, ThinkingLevel, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

const TEXT_MODELS: &[&str] = &[
    "MiniMax-M2.5",
    "deepseek-v3.2",
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "glm-5",
    "glm-5.1",
    "glm-5.2",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "qwen3.6-flash",
    "qwen3.6-plus",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.8-flash",
    "qwen3.8-max",
];

const INDIVIDUAL_TEXT_MODELS: &[&str] = &[
    "deepseek-v4-flash-0731",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "glm-5.2",
    "qwen3.6-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.8-flash",
    "qwen3.8-max",
];

const IMAGE_MODELS: &[&str] = &[
    "qwen-image-2.0",
    "qwen-image-2.0-pro",
    "wan2.7-image",
    "wan2.7-image-pro",
];

const QWEN_THINKING_MODELS: &[&str] = &[
    "deepseek-v3.2",
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "glm-5",
    "glm-5.1",
    "glm-5.2",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "qwen3.6-flash",
    "qwen3.6-plus",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.8-flash",
    "qwen3.8-max",
];

const QWEN_REASONING_EFFORT_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "glm-5",
    "glm-5.1",
    "glm-5.2",
];

const INDIVIDUAL_REASONING_EFFORT_MODELS: &[&str] = &[
    "deepseek-v4-flash-0731",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "glm-5.2",
];

const QWEN38_MODELS: &[&str] = &["qwen3.8-flash", "qwen3.8-max"];

const TOKEN_PLAN_PROVIDERS: &[&str] = &["qwen-token-plan", "qwen-token-plan-cn"];

const ALL_TOKEN_PLAN_PROVIDERS: &[&str] = &[
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
];

/// `QWEN_THINKING_MODEL_CASES`: the shared text models on both Token Plan
/// endpoints plus the Individual catalog.
fn qwen_thinking_model_cases() -> Vec<(&'static str, &'static str)> {
    let mut cases = Vec::new();
    for provider in TOKEN_PLAN_PROVIDERS {
        for model_id in QWEN_THINKING_MODELS {
            cases.push((*provider, *model_id));
        }
    }
    for model_id in INDIVIDUAL_TEXT_MODELS {
        cases.push(("qwen-token-plan-individual", *model_id));
    }
    cases
}

/// `QWEN_REASONING_EFFORT_MODEL_CASES`: the reasoning-effort capable models
/// on both Token Plan endpoints plus the Individual subset.
fn qwen_reasoning_effort_model_cases() -> Vec<(&'static str, &'static str)> {
    let mut cases = Vec::new();
    for provider in TOKEN_PLAN_PROVIDERS {
        for model_id in QWEN_REASONING_EFFORT_MODELS {
            cases.push((*provider, *model_id));
        }
    }
    for model_id in INDIVIDUAL_REASONING_EFFORT_MODELS {
        cases.push(("qwen-token-plan-individual", *model_id));
    }
    cases
}

/// `QWEN38_MODEL_CASES`: the qwen3.8 models on all three Token Plan
/// providers.
fn qwen38_model_cases() -> Vec<(&'static str, &'static str)> {
    let mut cases = Vec::new();
    for provider in ALL_TOKEN_PLAN_PROVIDERS {
        for model_id in QWEN38_MODELS {
            cases.push((*provider, *model_id));
        }
    }
    cases
}

fn chunk_events(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

/// The chunk yielded by the TS SDK mock.
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

/// Mock transport returning canned SSE data and capturing requests.
struct RecordingFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
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

fn ok_fetch() -> Arc<RecordingFetch> {
    Arc::new(RecordingFetch {
        body: chunk_events(&[ok_chunk()]),
        requests: Mutex::new(Vec::new()),
    })
}

fn builtin(provider: &str, id: &str) -> Model {
    get_models(provider)
        .into_iter()
        .find(|model| model.id == id)
        .unwrap_or_else(|| panic!("Missing model: {provider}/{id}"))
}

fn model_ids(provider: &str) -> Vec<String> {
    get_models(provider)
        .into_iter()
        .map(|model| model.id)
        .collect()
}

/// The `{ messages: [{ role: "user", content: "Hi", timestamp: ... }] }`
/// context from the TS cases, with the timestamp frozen.
fn hi_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("Hi".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

/// Runs the global `streamSimple` with `apiKey: "test"` and the requested
/// reasoning level, returning the captured request payload.
async fn capture_payload(model: &Model, reasoning: Option<ThinkingLevel>) -> Value {
    let fetch = ok_fetch();
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test".to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        reasoning,
        ..Default::default()
    };

    let _ = compat_stream_simple(model, &hi_context(), Some(&options))
        .result()
        .await;

    let requests = fetch.requests.lock().unwrap();
    match &requests.last().expect("request captured").body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    }
}

/// Asserts a `thinkingLevelMap` fragment the way `toMatchObject` does: every
/// listed key must be present with exactly the expected value.
fn expect_thinking_level_map(
    provider: &str,
    model_id: &str,
    model: &Model,
    expected: &[(ModelThinkingLevel, Option<&str>)],
) {
    let map = model
        .thinking_level_map
        .as_ref()
        .unwrap_or_else(|| panic!("Missing model: {provider}/{model_id}"));
    for (level, value) in expected {
        let expected = value.map(str::to_string);
        assert_eq!(
            map.get(level),
            Some(&expected),
            "{provider}/{model_id} level {level:?}"
        );
    }
}

#[test]
fn exposes_exactly_the_documented_individual_text_models() {
    let mut model_ids = model_ids("qwen-token-plan-individual");
    model_ids.sort();

    let mut expected: Vec<String> = INDIVIDUAL_TEXT_MODELS
        .iter()
        .map(|id| (*id).to_string())
        .collect();
    expected.sort();

    assert_eq!(model_ids, expected);
}

#[test]
fn reuses_the_international_token_plan_environment_variable() {
    let env: ProviderEnv =
        ProviderEnv::from([("QWEN_TOKEN_PLAN_API_KEY".to_string(), "test".to_string())]);
    assert_eq!(
        find_env_keys("qwen-token-plan-individual", Some(&env)),
        Some(vec!["QWEN_TOKEN_PLAN_API_KEY".to_string()])
    );
}

#[test]
fn exposes_all_text_models_on_token_plan_providers() {
    for provider in TOKEN_PLAN_PROVIDERS {
        let model_ids = model_ids(provider);
        for expected in TEXT_MODELS {
            assert!(
                model_ids.iter().any(|id| id == expected),
                "{provider} should include {expected}"
            );
        }
    }
}

#[test]
fn omits_image_models_from_token_plan_providers() {
    for provider in TOKEN_PLAN_PROVIDERS {
        let model_ids = model_ids(provider);
        for excluded in IMAGE_MODELS {
            assert!(
                !model_ids.iter().any(|id| id == excluded),
                "{provider} should not include {excluded}"
            );
        }
    }
}

// docs: https://modelstudio.console.alibabacloud.com/ap-southeast-1?tab=api&commonbuy=1#/api/?type=model&url=3016807
#[tokio::test]
async fn sends_qwen_thinking_fields_for_token_plan_model_cases() {
    for (provider, model_id) in qwen_thinking_model_cases() {
        let model = builtin(provider, model_id);

        let payload = capture_payload(&model, Some(ThinkingLevel::High)).await;

        assert_eq!(
            payload.get("enable_thinking"),
            Some(&json!(true)),
            "{provider}/{model_id}"
        );
        assert!(payload.get("thinking").is_none(), "{provider}/{model_id}");
    }
}

#[test]
fn exposes_qwen_reasoning_effort_levels_for_token_plan_model_cases() {
    let expected: &[(ModelThinkingLevel, Option<&str>)] = &[
        (ModelThinkingLevel::Minimal, None),
        (ModelThinkingLevel::Low, None),
        (ModelThinkingLevel::Medium, None),
        (ModelThinkingLevel::High, Some("high")),
        (ModelThinkingLevel::Xhigh, None),
        (ModelThinkingLevel::Max, Some("max")),
    ];

    for (provider, model_id) in qwen_reasoning_effort_model_cases() {
        let model = builtin(provider, model_id);
        expect_thinking_level_map(provider, model_id, &model, expected);
    }
}

#[test]
fn exposes_qwen3_8_reasoning_effort_levels_for_qwen38_model_cases() {
    let expected: &[(ModelThinkingLevel, Option<&str>)] = &[
        (ModelThinkingLevel::Minimal, None),
        (ModelThinkingLevel::Low, Some("low")),
        (ModelThinkingLevel::Medium, Some("medium")),
        (ModelThinkingLevel::High, None),
        (ModelThinkingLevel::Xhigh, Some("xhigh")),
        (ModelThinkingLevel::Max, None),
    ];

    for (provider, model_id) in qwen38_model_cases() {
        let model = builtin(provider, model_id);
        expect_thinking_level_map(provider, model_id, &model, expected);
    }
}

#[test]
fn omits_retired_qwen3_8_max_preview_on_all_token_plan_providers() {
    for provider in ALL_TOKEN_PLAN_PROVIDERS {
        let model_ids = model_ids(provider);
        assert!(
            !model_ids.iter().any(|id| id == "qwen3.8-max-preview"),
            "{provider}"
        );
    }
}

#[tokio::test]
async fn sends_qwen_reasoning_effort_for_token_plan_model_cases() {
    for (provider, model_id) in qwen_reasoning_effort_model_cases() {
        let model = builtin(provider, model_id);

        let payload = capture_payload(&model, Some(ThinkingLevel::High)).await;

        assert_eq!(
            payload.get("reasoning_effort"),
            Some(&json!("high")),
            "{provider}/{model_id}"
        );
    }
}

#[tokio::test]
async fn sends_qwen3_8_xhigh_reasoning_effort_for_qwen38_model_cases() {
    for (provider, model_id) in qwen38_model_cases() {
        let model = builtin(provider, model_id);

        let payload = capture_payload(&model, Some(ThinkingLevel::Xhigh)).await;

        assert_eq!(
            payload.get("enable_thinking"),
            Some(&json!(true)),
            "{provider}/{model_id}"
        );
        assert_eq!(
            payload.get("reasoning_effort"),
            Some(&json!("xhigh")),
            "{provider}/{model_id}"
        );
        assert!(payload.get("thinking").is_none(), "{provider}/{model_id}");
    }
}
