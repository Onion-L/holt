//! Ports of the offline Anthropic payload tests that capture the outgoing
//! request. The TypeScript suites either capture the payload through the
//! `onPayload` hook (which throws to short-circuit) or run a local HTTP
//! server; the Rust port captures the JSON body and headers through a mock
//! `HttpFetch` transport, which is the same observable surface.
//!
//! - `anthropic-adaptive-thinking-models.test.ts` (1 case)
//! - `anthropic-force-adaptive-thinking.test.ts` (6 offline cases)
//! - `anthropic-thinking-disable.test.ts` (7 offline cases; the trailing
//!   "Anthropic thinking disable E2E" describe is live-credential-gated
//!   upstream and stays unported)
//! - `anthropic-temperature-compat.test.ts` (6 cases)
//! - `anthropic-empty-thinking-signature-compat.test.ts` (4 cases)
//! - `anthropic-eager-tool-input-compat.test.ts` (4 cases)
//! - `anthropic-cache-write-1h-cost.test.ts` (2 cases)
//! - `cache-retention.test.ts` (the Anthropic describe, 7 cases; the two
//!   PI_CACHE_RETENTION env cases inject a scoped `ProviderEnv` instead of
//!   mutating process env)
//! - `fireworks-models.test.ts` (only the Anthropic payload describe)
//! - `github-copilot-anthropic.test.ts` (3 cases)

use std::sync::{Arc, Mutex};

use pi_core::ai::api::anthropic_messages::{
    AnthropicOptions, stream as stream_anthropic, stream_simple,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, CacheRetention, Context, Message, Model, ModelCompat,
    ModelCost, ModelInput, ModelThinkingLevel, ProviderEnv, ProviderRequestOptions, RoleUser,
    SimpleStreamOptions, StopReason, StreamOptions, ThinkingContent, ThinkingLevel, Tool,
    UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

fn builtin(provider: &str, id: &str) -> Model {
    pi_core::ai::providers::builtin::get_builtin_model(provider, id)
        .unwrap_or_else(|| panic!("missing model {provider}/{id}"))
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn hello_context() -> Context {
    Context {
        messages: vec![user_message("Hello")],
        ..Default::default()
    }
}

/// The mock transport standing in for the TS `onPayload` captures and local
/// HTTP servers: it records the outgoing request and answers with a minimal
/// SSE stream.
struct CaptureFetch {
    requests: Mutex<Vec<HttpRequest>>,
}

impl CaptureFetch {
    fn new() -> Self {
        CaptureFetch {
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

impl HttpFetch for CaptureFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = [
            format!(
                "event: message_start\ndata: {}\n",
                json!({
                    "type": "message_start",
                    "message": { "id": "msg_test", "usage": { "input_tokens": 10, "output_tokens": 0 } },
                })
            ),
            format!(
                "event: message_delta\ndata: {}\n",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 5 },
                })
            ),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n".to_string(),
        ]
        .join("\n");
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn body_of(request: &HttpRequest) -> Value {
    match &request.body {
        HttpBody::Json(value) => value.clone(),
        _ => panic!("expected JSON body"),
    }
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn request_options(fetch: Arc<CaptureFetch>) -> ProviderRequestOptions {
    ProviderRequestOptions {
        api_key: Some("fake-key".to_string()),
        fetch: Some(fetch),
        ..Default::default()
    }
}

/// The `capturePayload` helper for the `streamSimple`-based suites: returns
/// the JSON payload that left the process.
async fn capture_simple_payload(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> Value {
    let fetch = Arc::new(CaptureFetch::new());
    let env = options.base.base.env.clone();
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env,
                ..request_options(fetch.clone())
            },
            ..options.base
        },
        ..options
    };
    let _ = stream_simple(model, context, Some(&options)).result().await;
    body_of(&fetch.request())
}

/// The `capturePayload` helper for the direct `streamAnthropic` suites.
async fn capture_anthropic_payload(
    model: &Model,
    context: &Context,
    options: AnthropicOptions,
) -> Value {
    body_of(&capture_anthropic_request(model, context, options).await)
}

async fn capture_anthropic_request(
    model: &Model,
    context: &Context,
    options: AnthropicOptions,
) -> HttpRequest {
    let fetch = Arc::new(CaptureFetch::new());
    let env = options.base.base.env.clone();
    let options = AnthropicOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env,
                ..request_options(fetch.clone())
            },
            ..options.base
        },
        ..options
    };
    let _ = stream_anthropic(model, context, Some(&options))
        .result()
        .await;
    fetch.request()
}

fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn lookup_tool() -> Tool {
    Tool {
        name: "lookup".to_string(),
        description: "Look up a value".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
        }),
        constrained_sampling: None,
    }
}

// ---------------------------------------------------------------------------
// Anthropic adaptive thinking model metadata
// (anthropic-adaptive-thinking-models.test.ts)

#[test]
fn marks_builtin_anthropic_messages_models_that_use_adaptive_thinking() {
    let expected_current_adaptive_thinking_models = [
        "anthropic/claude-fable-5",
        "anthropic/claude-opus-4-8",
        "anthropic/claude-opus-5",
        "anthropic/claude-sonnet-5",
        "cloudflare-ai-gateway/claude-fable-5",
        "kimi-coding/kimi-for-coding",
        "kimi-coding/k3",
        "kimi-coding/kimi-for-coding-highspeed",
        "opencode/claude-opus-4-8",
        "opencode/claude-opus-5",
        "vercel-ai-gateway/anthropic/claude-opus-4.8",
        "vercel-ai-gateway/anthropic/claude-opus-5",
        "vercel-ai-gateway/anthropic/claude-sonnet-5",
    ];

    let mut flagged_models: Vec<String> = pi_core::ai::providers::builtin::get_builtin_providers()
        .into_iter()
        .flat_map(|provider| pi_core::ai::providers::builtin::get_builtin_models(&provider))
        .filter(|model| model.api == "anthropic-messages")
        .filter(|model| {
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.force_adaptive_thinking)
                == Some(true)
        })
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect();
    flagged_models.sort();

    for expected in expected_current_adaptive_thinking_models {
        assert!(
            flagged_models.contains(&expected.to_string()),
            "expected {expected} among flagged adaptive-thinking models"
        );
    }

    let adaptive_families = regex::Regex::new(
        r"(opus[-.](4[-.][678]|5)|sonnet[-.]4[-.]6|sonnet[-.]5|fable[-.]5|kimi-coding/)",
    )
    .unwrap();
    for model_id in &flagged_models {
        assert!(
            adaptive_families.is_match(model_id),
            "{model_id} is flagged but outside the adaptive families"
        );
    }
}

// ---------------------------------------------------------------------------
// Anthropic forceAdaptiveThinking compat override
// (anthropic-force-adaptive-thinking.test.ts)

fn vendor_proxy_model(compat: Option<ModelCompat>) -> Model {
    Model {
        // Id intentionally does not match any built-in adaptive substring.
        // This mirrors corporate proxy schemes such as
        // `anthropic--claude-opus-latest`.
        id: "vendor--claude-opus-latest".to_string(),
        name: "Vendor Proxy Opus Latest".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "vendor-proxy".to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        compat,
        ..Default::default()
    }
}

fn reasoning_option(reasoning: ThinkingLevel) -> SimpleStreamOptions {
    SimpleStreamOptions {
        reasoning: Some(reasoning),
        ..Default::default()
    }
}

#[tokio::test]
async fn sends_legacy_thinking_payload_for_custom_model_ids_by_default() {
    let payload = capture_simple_payload(
        &vendor_proxy_model(None),
        &hello_context(),
        reasoning_option(ThinkingLevel::Medium),
    )
    .await;

    assert_eq!(payload["thinking"]["type"], json!("enabled"));
    assert!(payload.get("output_config").is_none());
}

#[tokio::test]
async fn sends_adaptive_thinking_payload_when_compat_force_adaptive_thinking_is_true() {
    let model = vendor_proxy_model(Some(ModelCompat {
        force_adaptive_thinking: Some(true),
        ..Default::default()
    }));
    let payload = capture_simple_payload(
        &model,
        &hello_context(),
        reasoning_option(ThinkingLevel::Medium),
    )
    .await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "medium" }));
}

#[tokio::test]
async fn uses_adaptive_thinking_with_native_xhigh_effort_for_claude_fable_5() {
    let model = builtin("anthropic", "claude-fable-5");
    let payload = capture_simple_payload(
        &model,
        &hello_context(),
        reasoning_option(ThinkingLevel::Xhigh),
    )
    .await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "xhigh" }));
}

async fn kimi_coding_payload(model_id: &str, reasoning: ThinkingLevel) -> Value {
    let model = builtin("kimi-coding", model_id);
    capture_simple_payload(&model, &hello_context(), reasoning_option(reasoning)).await
}

#[tokio::test]
async fn uses_adaptive_thinking_effort_without_a_token_budget_for_kimi_coding_kimi_for_coding() {
    let payload = kimi_coding_payload("kimi-for-coding", ThinkingLevel::Medium).await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "medium" }));
}

#[tokio::test]
async fn uses_adaptive_thinking_effort_without_a_token_budget_for_kimi_coding_k3() {
    let payload = kimi_coding_payload("k3", ThinkingLevel::Max).await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "max" }));
}

#[tokio::test]
async fn uses_adaptive_thinking_effort_without_a_token_budget_for_kimi_coding_kimi_for_coding_highspeed()
 {
    let payload = kimi_coding_payload("kimi-for-coding-highspeed", ThinkingLevel::Medium).await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "medium" }));
}

#[tokio::test]
async fn allows_builtin_adaptive_models_to_opt_out_with_compat_force_adaptive_thinking_false() {
    let mut model = builtin("anthropic", "claude-opus-4-8");
    model.compat = Some(ModelCompat {
        force_adaptive_thinking: Some(false),
        ..Default::default()
    });
    let payload = capture_simple_payload(
        &model,
        &hello_context(),
        reasoning_option(ThinkingLevel::Medium),
    )
    .await;

    assert_eq!(payload["thinking"]["type"], json!("enabled"));
    assert!(payload.get("output_config").is_none());
}

#[tokio::test]
async fn preserves_thinking_type_disabled_when_reasoning_is_off_regardless_of_override() {
    let model = vendor_proxy_model(Some(ModelCompat {
        force_adaptive_thinking: Some(true),
        ..Default::default()
    }));
    let payload =
        capture_simple_payload(&model, &hello_context(), SimpleStreamOptions::default()).await;

    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert!(payload.get("output_config").is_none());
}

// ---------------------------------------------------------------------------
// Anthropic thinking disable payload
// (anthropic-thinking-disable.test.ts; the E2E describe is live-gated)

async fn thinking_payload(
    provider: &str,
    model_id: &str,
    reasoning: Option<ThinkingLevel>,
) -> Value {
    let model = builtin(provider, model_id);
    let options = SimpleStreamOptions {
        reasoning,
        ..Default::default()
    };
    capture_simple_payload(&model, &hello_context(), options).await
}

#[tokio::test]
async fn sends_thinking_type_disabled_for_budget_based_reasoning_models_when_thinking_is_off() {
    let payload = thinking_payload("anthropic", "claude-sonnet-4-5", None).await;

    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert!(payload.get("output_config").is_none());
}

#[tokio::test]
async fn sends_thinking_type_disabled_for_adaptive_reasoning_models_when_thinking_is_off() {
    let payload = thinking_payload("anthropic", "claude-opus-4-6", None).await;

    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert!(payload.get("output_config").is_none());
}

#[tokio::test]
async fn sends_thinking_type_disabled_for_claude_opus_48_when_thinking_is_off() {
    let payload = thinking_payload("anthropic", "claude-opus-4-8", None).await;

    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert!(payload.get("output_config").is_none());
}

#[tokio::test]
async fn omits_thinking_type_disabled_for_claude_fable_5_when_thinking_is_off() {
    let payload = thinking_payload("anthropic", "claude-fable-5", None).await;

    assert!(payload.get("thinking").is_none());
    assert!(payload.get("output_config").is_none());
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_opus_48_when_reasoning_is_enabled() {
    let payload = thinking_payload("anthropic", "claude-opus-4-8", Some(ThinkingLevel::High)).await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "high" }));
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_sonnet_5_when_reasoning_is_enabled() {
    let payload = thinking_payload("anthropic", "claude-sonnet-5", Some(ThinkingLevel::High)).await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "high" }));
}

#[tokio::test]
async fn maps_xhigh_reasoning_to_effort_xhigh_for_claude_opus_48() {
    let payload =
        thinking_payload("anthropic", "claude-opus-4-8", Some(ThinkingLevel::Xhigh)).await;

    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "xhigh" }));
}

// ---------------------------------------------------------------------------
// Anthropic temperature compatibility
// (anthropic-temperature-compat.test.ts)

fn temperature_option(temperature: f64) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            temperature: Some(temperature),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn temperature_payload(provider: &str, model_id: &str, temperature: f64) -> Value {
    let model = builtin(provider, model_id);
    capture_simple_payload(&model, &hello_context(), temperature_option(temperature)).await
}

#[tokio::test]
async fn omits_temperature_for_claude_opus_47() {
    let payload = temperature_payload("anthropic", "claude-opus-4-7", 0.0).await;
    assert!(payload.get("temperature").is_none());
}

#[tokio::test]
async fn omits_temperature_for_claude_opus_48() {
    let payload = temperature_payload("anthropic", "claude-opus-4-8", 0.0).await;
    assert!(payload.get("temperature").is_none());
}

#[tokio::test]
async fn omits_default_temperature_for_claude_opus_47() {
    let payload = temperature_payload("anthropic", "claude-opus-4-7", 1.0).await;
    assert!(payload.get("temperature").is_none());
}

#[tokio::test]
async fn keeps_temperature_for_claude_opus_46() {
    let payload = temperature_payload("anthropic", "claude-opus-4-6", 0.0).await;
    assert_eq!(payload["temperature"].as_f64(), Some(0.0));
}

#[tokio::test]
async fn keeps_temperature_for_claude_sonnet_46() {
    let payload = temperature_payload("anthropic", "claude-sonnet-4-6", 0.0).await;
    assert_eq!(payload["temperature"].as_f64(), Some(0.0));
}

#[tokio::test]
async fn omits_temperature_for_custom_models_with_supports_temperature_disabled() {
    let model = Model {
        id: "vendor--claude-opus-4-7".to_string(),
        name: "Vendor Proxy Opus 4.7".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "vendor-proxy".to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        compat: Some(ModelCompat {
            supports_temperature: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = capture_simple_payload(&model, &hello_context(), temperature_option(0.0)).await;

    assert!(payload.get("temperature").is_none());
}

// ---------------------------------------------------------------------------
// Anthropic empty thinking signature compat
// (anthropic-empty-thinking-signature-compat.test.ts)

fn mimo_model(allow_empty_signature: Option<bool>) -> Model {
    Model {
        id: "mimo-v2.5-pro".to_string(),
        name: "MiMo-V2.5-Pro".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "xiaomi-token-plan-ams".to_string(),
        base_url: "http://127.0.0.1:9/anthropic".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 1_048_576,
        max_tokens: 1024,
        compat: allow_empty_signature.map(|allow| ModelCompat {
            allow_empty_signature: Some(allow),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn signature_context(
    thinking_signature: &str,
    thinking: &str,
    provider: &str,
    model_id: &str,
) -> Context {
    let assistant = AssistantMessage {
        content: vec![AssistantContent::Thinking(ThinkingContent {
            thinking: thinking.to_string(),
            thinking_signature: Some(thinking_signature.to_string()),
            ..Default::default()
        })],
        provider: provider.to_string(),
        api: "anthropic-messages".to_string(),
        model: model_id.to_string(),
        timestamp: 0,
        stop_reason: StopReason::Stop,
        ..Default::default()
    };
    Context {
        messages: vec![
            user_message("first"),
            Message::Assistant(Box::new(assistant)),
            user_message("second"),
        ],
        ..Default::default()
    }
}

async fn assistant_content_in_payload(model: &Model, context: &Context) -> Value {
    let payload = capture_simple_payload(model, context, SimpleStreamOptions::default()).await;
    payload["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == json!("assistant"))
        .map(|message| message["content"].clone())
        .expect("assistant message in payload")
}

#[tokio::test]
async fn converts_empty_signature_thinking_to_text_by_default() {
    let content = assistant_content_in_payload(
        &mimo_model(None),
        &signature_context(
            "",
            "internal reasoning",
            "xiaomi-token-plan-ams",
            "mimo-v2.5-pro",
        ),
    )
    .await;
    assert_eq!(
        content,
        json!([{ "type": "text", "text": "internal reasoning" }])
    );
}

#[tokio::test]
async fn preserves_empty_thinking_text_when_the_signature_is_present() {
    let content = assistant_content_in_payload(
        &mimo_model(None),
        &signature_context(
            "signed-thinking",
            "",
            "xiaomi-token-plan-ams",
            "mimo-v2.5-pro",
        ),
    )
    .await;
    assert_eq!(
        content,
        json!([{ "type": "thinking", "thinking": "", "signature": "signed-thinking" }])
    );
}

#[tokio::test]
async fn preserves_empty_signature_thinking_when_allow_empty_signature_is_enabled() {
    let content = assistant_content_in_payload(
        &mimo_model(Some(true)),
        &signature_context(
            " ",
            "internal reasoning",
            "xiaomi-token-plan-ams",
            "mimo-v2.5-pro",
        ),
    )
    .await;
    assert_eq!(
        content,
        json!([{ "type": "thinking", "thinking": "internal reasoning", "signature": "" }])
    );
}

#[tokio::test]
async fn allows_empty_signatures_for_kimi_coding_k3() {
    let model = builtin("kimi-coding", "k3");
    assert_eq!(
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.allow_empty_signature),
        Some(true)
    );

    let content = assistant_content_in_payload(
        &model,
        &signature_context(" ", "internal reasoning", "kimi-coding", "k3"),
    )
    .await;
    assert_eq!(
        content,
        json!([{ "type": "thinking", "thinking": "internal reasoning", "signature": "" }])
    );
}

// ---------------------------------------------------------------------------
// Anthropic eager tool input streaming compatibility
// (anthropic-eager-tool-input-compat.test.ts)

fn eager_compat_model(compat: ModelCompat) -> Model {
    Model {
        id: "claude-opus-4-8".to_string(),
        name: "Claude Opus 4.8".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "test-anthropic".to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        compat: Some(ModelCompat {
            force_adaptive_thinking: Some(true),
            ..compat
        }),
        ..Default::default()
    }
}

fn tools_context(tools: &[Tool]) -> Context {
    Context {
        messages: vec![user_message("Use the tool")],
        tools: (!tools.is_empty()).then(|| tools.to_vec()),
        ..Default::default()
    }
}

async fn capture_eager_request(compat: ModelCompat, context: &Context) -> HttpRequest {
    capture_anthropic_request(
        &eager_compat_model(compat),
        context,
        AnthropicOptions {
            base: StreamOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
}

fn first_tool(body: &Value) -> &Value {
    body["tools"]
        .as_array()
        .and_then(|tools| tools.first())
        .expect("first tool in request body")
}

fn first_tool_input_schema(body: &Value) -> &Value {
    first_tool(body).get("input_schema").expect("input schema")
}

#[tokio::test]
async fn sends_per_tool_eager_input_streaming_by_default() {
    let request =
        capture_eager_request(ModelCompat::default(), &tools_context(&[lookup_tool()])).await;

    assert_eq!(
        first_tool(&body_of(&request))["eager_input_streaming"],
        json!(true)
    );
    assert!(header(&request, "anthropic-beta").is_none());
}

#[tokio::test]
async fn uses_the_legacy_fine_grained_tool_streaming_beta_when_eager_tool_input_streaming_is_disabled()
 {
    let request = capture_eager_request(
        ModelCompat {
            supports_eager_tool_input_streaming: Some(false),
            ..Default::default()
        },
        &tools_context(&[lookup_tool()]),
    )
    .await;

    assert!(
        first_tool(&body_of(&request))
            .get("eager_input_streaming")
            .is_none()
    );
    assert_eq!(
        header(&request, "anthropic-beta"),
        Some("fine-grained-tool-streaming-2025-05-14")
    );
}

#[tokio::test]
async fn does_not_send_the_legacy_fine_grained_tool_streaming_beta_when_there_are_no_tools() {
    let request = capture_eager_request(
        ModelCompat {
            supports_eager_tool_input_streaming: Some(false),
            ..Default::default()
        },
        &tools_context(&[]),
    )
    .await;

    assert!(body_of(&request).get("tools").is_none());
    assert!(header(&request, "anthropic-beta").is_none());
}

#[tokio::test]
async fn only_sends_the_full_input_schema_for_strict_json_schema_tools() {
    // The schema-compatibility tool carries additionalProperties/title in its
    // parameters, which the legacy path must drop.
    let mut schema_compatibility_tool = lookup_tool();
    schema_compatibility_tool.parameters = json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"],
        "additionalProperties": false,
        "title": "LookupInput",
    });
    let legacy_request = capture_eager_request(
        ModelCompat {
            supports_strict_tools: Some(true),
            ..Default::default()
        },
        &tools_context(&[schema_compatibility_tool.clone()]),
    )
    .await;
    assert_eq!(
        first_tool_input_schema(&body_of(&legacy_request)),
        &json!({
            "type": "object",
            "properties": schema_compatibility_tool.parameters["properties"],
            "required": schema_compatibility_tool.parameters["required"],
        })
    );

    let mut strict_tool = lookup_tool();
    strict_tool.parameters = json!({
        "type": "object",
        "properties": {
            "value": { "type": "string" },
            "optional": { "type": "number" },
        },
        "required": ["value"],
        "title": "StrictLookupInput",
    });
    strict_tool.constrained_sampling = Some(pi_core::ai::types::ToolConstrainedSampling::Config(
        pi_core::ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: pi_core::ai::types::ConstrainedSamplingStrict::Prefer,
        },
    ));
    let strict_request = capture_eager_request(
        ModelCompat {
            supports_strict_tools: Some(true),
            ..Default::default()
        },
        &tools_context(&[strict_tool]),
    )
    .await;
    let body = body_of(&strict_request);
    assert_eq!(first_tool(&body)["strict"], json!(true));
    let input_schema = first_tool_input_schema(&body);
    assert_eq!(input_schema["additionalProperties"], json!(false));
    assert_eq!(input_schema["required"], json!(["value", "optional"]));
    assert_eq!(
        input_schema["properties"]["optional"],
        json!({ "anyOf": [{ "type": "number" }, { "type": "null" }] })
    );
    assert_eq!(input_schema["title"], json!("StrictLookupInput"));
}

// ---------------------------------------------------------------------------
// Anthropic 1h cache write cost
// (anthropic-cache-write-1h-cost.test.ts)

fn cache_creation_events(cache_creation: Option<Value>) -> Vec<(String, String)> {
    let mut start_usage = json!({
        "input_tokens": 100,
        "output_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 1_000_000,
    });
    if let Some(cache_creation) = cache_creation {
        start_usage["cache_creation"] = cache_creation;
    }
    vec![
        (
            "message_start".to_string(),
            json!({
                "type": "message_start",
                "message": { "id": "msg_test", "usage": start_usage },
            })
            .to_string(),
        ),
        (
            "content_block_start".to_string(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            })
            .to_string(),
        ),
        (
            "content_block_delta".to_string(),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hi" },
            })
            .to_string(),
        ),
        (
            "content_block_stop".to_string(),
            "{\"type\":\"content_block_stop\",\"index\":0}".to_string(),
        ),
        (
            "message_delta".to_string(),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 5,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 1_000_000,
                },
            })
            .to_string(),
        ),
        (
            "message_stop".to_string(),
            "{\"type\":\"message_stop\"}".to_string(),
        ),
    ]
}

/// Drives `stream` against a scripted cache-write SSE fixture.
async fn cache_write_result(cache_creation: Option<Value>) -> pi_core::ai::types::AssistantMessage {
    struct CacheWriteFetch {
        body: String,
    }
    impl HttpFetch for CacheWriteFetch {
        fn fetch<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
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

    let events = cache_creation_events(cache_creation);
    let body = events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n"))
        .collect::<Vec<_>>()
        .join("\n");
    let model = builtin("anthropic", "claude-opus-4-8");
    stream_anthropic(
        &model,
        &Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        Some(&AnthropicOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("fake-key".to_string()),
                    fetch: Some(Arc::new(CacheWriteFetch { body })),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .result()
    .await
}

#[tokio::test]
async fn prices_the_1h_portion_at_2x_input_and_the_rest_at_the_5m_rate() {
    // claude-opus-4-8: input 5, cacheWrite (5m) 6.25 per Mtok. 1h write = 2x
    // input = 10.
    let result = cache_write_result(Some(json!({
        "ephemeral_5m_input_tokens": 600_000,
        "ephemeral_1h_input_tokens": 400_000,
    })))
    .await;

    assert_eq!(result.usage.cache_write, 1_000_000);
    assert_eq!(result.usage.cache_write_1h, Some(400_000));
    // 600k * 6.25/Mtok + 400k * 10/Mtok = 3.75 + 4.0 = 7.75
    assert!(
        (f64::from(result.usage.cost.cache_write) - 7.75).abs() < 1e-10,
        "cache write cost {}",
        f64::from(result.usage.cost.cache_write)
    );
}

#[tokio::test]
async fn falls_back_to_the_5m_rate_when_no_breakdown_is_reported() {
    let result = cache_write_result(None).await;

    assert_eq!(result.usage.cache_write, 1_000_000);
    assert_eq!(result.usage.cache_write_1h.unwrap_or(0), 0);
    // 1M * 6.25/Mtok = 6.25
    assert!(
        (f64::from(result.usage.cost.cache_write) - 6.25).abs() < 1e-10,
        "cache write cost {}",
        f64::from(result.usage.cost.cache_write)
    );
}

// ---------------------------------------------------------------------------
// Cache Retention (PI_CACHE_RETENTION), Anthropic describe
// (cache-retention.test.ts)

fn retention_context() -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message("Hello")],
        ..Default::default()
    }
}

fn retention_options(
    env: Option<ProviderEnv>,
    cache_retention: Option<CacheRetention>,
) -> AnthropicOptions {
    AnthropicOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env,
                ..Default::default()
            },
            cache_retention,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn uses_the_default_cache_ttl_when_pi_cache_retention_is_not_set() {
    // The TS case is live-gated (`skipIf(!ANTHROPIC_API_KEY)`); the offline
    // Rust port injects an empty scoped env and the mock transport.
    let model = builtin("anthropic", "claude-haiku-4-5");
    let payload = capture_anthropic_payload(
        &model,
        &retention_context(),
        retention_options(Some(ProviderEnv::new()), None),
    )
    .await;

    let system = payload["system"].as_array().expect("system blocks");
    assert_eq!(system[0]["cache_control"], json!({ "type": "ephemeral" }));
}

#[tokio::test]
async fn uses_the_1h_cache_ttl_when_pi_cache_retention_is_long() {
    // The TS case is live-gated (`skipIf(!ANTHROPIC_API_KEY)`); the offline
    // Rust port injects the scoped env the TS test set on process.env.
    let model = builtin("anthropic", "claude-haiku-4-5");
    let payload = capture_anthropic_payload(
        &model,
        &retention_context(),
        retention_options(Some(scoped_env(&[("PI_CACHE_RETENTION", "long")])), None),
    )
    .await;

    let system = payload["system"].as_array().expect("system blocks");
    assert_eq!(
        system[0]["cache_control"],
        json!({ "type": "ephemeral", "ttl": "1h" })
    );
}

#[tokio::test]
async fn adds_ttl_for_non_api_anthropic_com_baseurl_by_default() {
    let mut proxy_model = builtin("anthropic", "claude-haiku-4-5");
    proxy_model.base_url = "https://my-proxy.example.com/v1".to_string();
    let payload = capture_anthropic_payload(
        &proxy_model,
        &retention_context(),
        retention_options(Some(scoped_env(&[("PI_CACHE_RETENTION", "long")])), None),
    )
    .await;

    let system = payload["system"].as_array().expect("system blocks");
    assert_eq!(
        system[0]["cache_control"],
        json!({ "type": "ephemeral", "ttl": "1h" })
    );
}

#[tokio::test]
async fn omits_ttl_when_supports_long_cache_retention_is_false() {
    let mut proxy_model = builtin("anthropic", "claude-haiku-4-5");
    proxy_model.base_url = "https://my-proxy.example.com/v1".to_string();
    proxy_model.compat = Some(ModelCompat {
        supports_long_cache_retention: Some(false),
        ..Default::default()
    });
    let payload = capture_anthropic_payload(
        &proxy_model,
        &retention_context(),
        retention_options(None, Some(CacheRetention::Long)),
    )
    .await;

    let system = payload["system"].as_array().expect("system blocks");
    assert_eq!(system[0]["cache_control"], json!({ "type": "ephemeral" }));
}

#[tokio::test]
async fn omits_cache_control_when_cache_retention_is_none() {
    let model = builtin("anthropic", "claude-haiku-4-5");
    let payload = capture_anthropic_payload(
        &model,
        &retention_context(),
        retention_options(None, Some(CacheRetention::None)),
    )
    .await;

    let system = payload["system"].as_array().expect("system blocks");
    assert!(system[0].get("cache_control").is_none());
}

#[tokio::test]
async fn adds_cache_control_to_string_user_messages() {
    let model = builtin("anthropic", "claude-haiku-4-5");
    let payload =
        capture_anthropic_payload(&model, &retention_context(), retention_options(None, None))
            .await;

    let messages = payload["messages"].as_array().expect("messages");
    let last_message = messages.last().unwrap();
    let content = last_message["content"].as_array().expect("array content");
    let last_block = content.last().unwrap();
    assert_eq!(last_block["cache_control"], json!({ "type": "ephemeral" }));
}

#[tokio::test]
async fn sets_the_1h_cache_ttl_when_cache_retention_is_long() {
    let model = builtin("anthropic", "claude-haiku-4-5");
    let payload = capture_anthropic_payload(
        &model,
        &retention_context(),
        retention_options(None, Some(CacheRetention::Long)),
    )
    .await;

    let system = payload["system"].as_array().expect("system blocks");
    assert_eq!(
        system[0]["cache_control"],
        json!({ "type": "ephemeral", "ttl": "1h" })
    );
}

// ---------------------------------------------------------------------------
// Fireworks Anthropic session affinity and tool compat
// (fireworks-models.test.ts, the payload describe only)

fn fireworks_compat() -> ModelCompat {
    ModelCompat {
        send_session_affinity_headers: Some(true),
        supports_eager_tool_input_streaming: Some(false),
        supports_cache_control_on_tools: Some(false),
        supports_long_cache_retention: Some(false),
        ..Default::default()
    }
}

fn fireworks_anthropic_model() -> Model {
    Model {
        id: "accounts/fireworks/models/kimi-k2p6".to_string(),
        name: "Kimi K2.6".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "fireworks".to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost {
            rates: pi_core::ai::types::ModelCostRates {
                input: 0.95.into(),
                output: 4.0.into(),
                cache_read: 0.16.into(),
                cache_write: 0.0.into(),
            },
            tiers: None,
        },
        context_window: 262_000,
        max_tokens: 262_000,
        compat: Some(fireworks_compat()),
        ..Default::default()
    }
}

fn native_anthropic_model() -> Model {
    Model {
        id: "claude-opus-4-8".to_string(),
        name: "Claude Opus 4.8".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        compat: None,
        ..Default::default()
    }
}

async fn capture_fireworks_request(
    model: &Model,
    session_id: Option<&str>,
    cache_retention: Option<CacheRetention>,
) -> HttpRequest {
    capture_anthropic_request(
        model,
        &tools_context(&[lookup_tool()]),
        AnthropicOptions {
            base: StreamOptions {
                cache_retention,
                session_id: session_id.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn sends_x_session_affinity_header_for_fireworks_models() {
    let request = capture_fireworks_request(
        &fireworks_anthropic_model(),
        Some("fireworks-session-1"),
        None,
    )
    .await;

    assert_eq!(
        header(&request, "x-session-affinity"),
        Some("fireworks-session-1")
    );
}

#[tokio::test]
async fn omits_x_session_affinity_header_for_native_anthropic_models() {
    let request =
        capture_fireworks_request(&native_anthropic_model(), Some("anthropic-session-1"), None)
            .await;

    assert!(header(&request, "x-session-affinity").is_none());
}

#[tokio::test]
async fn omits_x_session_affinity_header_when_cache_retention_is_none() {
    let request = capture_fireworks_request(
        &fireworks_anthropic_model(),
        Some("fireworks-session-2"),
        Some(CacheRetention::None),
    )
    .await;

    assert!(header(&request, "x-session-affinity").is_none());
}

#[tokio::test]
async fn omits_cache_control_on_tools_for_fireworks_models() {
    let request = capture_fireworks_request(&fireworks_anthropic_model(), None, None).await;
    let body = body_of(&request);
    let tools = body["tools"].as_array().expect("tools");

    let last_tool = tools.last().unwrap();
    assert!(last_tool.get("cache_control").is_none());
}

#[tokio::test]
async fn omits_eager_input_streaming_on_tools_for_fireworks_models() {
    let request = capture_fireworks_request(&fireworks_anthropic_model(), None, None).await;
    let body = body_of(&request);
    let tools = body["tools"].as_array().expect("tools");

    for tool in tools {
        assert!(tool.get("eager_input_streaming").is_none());
    }
}

#[tokio::test]
async fn sends_cache_control_on_tools_for_native_anthropic_models() {
    let request = capture_fireworks_request(&native_anthropic_model(), None, None).await;
    let body = body_of(&request);
    let tools = body["tools"].as_array().expect("tools");

    let last_tool = tools.last().unwrap();
    let cache_control = last_tool.get("cache_control").expect("cache_control");
    assert_eq!(cache_control["type"], json!("ephemeral"));
}

#[tokio::test]
async fn sends_eager_input_streaming_on_tools_for_native_anthropic_models() {
    let request = capture_fireworks_request(&native_anthropic_model(), None, None).await;
    let body = body_of(&request);
    let tools = body["tools"].as_array().expect("tools");

    assert_eq!(tools[0]["eager_input_streaming"], json!(true));
}

// ---------------------------------------------------------------------------
// Copilot Claude via Anthropic Messages
// (github-copilot-anthropic.test.ts)

fn copilot_context() -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message("Hello")],
        ..Default::default()
    }
}

fn thinking_level_map(model: &Model, level: ModelThinkingLevel) -> Option<Option<String>> {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level).cloned())
}

#[test]
fn applies_copilot_specific_adaptive_thinking_effort_overrides() {
    let opus47 = builtin("github-copilot", "claude-opus-4.7");
    assert_eq!(
        thinking_level_map(&opus47, ModelThinkingLevel::Minimal),
        Some(Some("low".to_string()))
    );
    assert_eq!(
        thinking_level_map(&opus47, ModelThinkingLevel::Xhigh),
        Some(Some("xhigh".to_string()))
    );
    assert_eq!(
        thinking_level_map(&opus47, ModelThinkingLevel::Max),
        Some(Some("max".to_string()))
    );
    let levels = pi_core::ai::models::get_supported_thinking_levels(&opus47);
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));

    let opus5 = builtin("github-copilot", "claude-opus-5");
    assert_eq!(opus5.api, "anthropic-messages");
    assert_eq!(opus5.context_window, 1_000_000);
    assert_eq!(
        thinking_level_map(&opus5, ModelThinkingLevel::Minimal),
        Some(Some("low".to_string()))
    );
    assert_eq!(
        thinking_level_map(&opus5, ModelThinkingLevel::Xhigh),
        Some(Some("xhigh".to_string()))
    );
    assert_eq!(
        thinking_level_map(&opus5, ModelThinkingLevel::Max),
        Some(Some("max".to_string()))
    );
    let levels = pi_core::ai::models::get_supported_thinking_levels(&opus5);
    assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    assert!(levels.contains(&ModelThinkingLevel::Max));

    let sonnet46 = builtin("github-copilot", "claude-sonnet-4.6");
    assert_eq!(
        thinking_level_map(&sonnet46, ModelThinkingLevel::Minimal),
        Some(Some("low".to_string()))
    );
    assert_eq!(
        thinking_level_map(&sonnet46, ModelThinkingLevel::Max),
        Some(Some("max".to_string()))
    );
    let levels = pi_core::ai::models::get_supported_thinking_levels(&sonnet46);
    assert!(levels.contains(&ModelThinkingLevel::Max));
    assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
}

async fn capture_copilot_request(interleaved_thinking: Option<bool>) -> HttpRequest {
    let model = builtin("github-copilot", "claude-sonnet-4.6");
    let fetch = Arc::new(CaptureFetch::new());
    let options = AnthropicOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("tid_copilot_session_test_token".to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        interleaved_thinking,
        ..Default::default()
    };
    let _ = stream_anthropic(&model, &copilot_context(), Some(&options))
        .result()
        .await;
    fetch.request()
}

#[tokio::test]
async fn uses_bearer_auth_copilot_headers_and_a_valid_anthropic_messages_payload() {
    let model = builtin("github-copilot", "claude-sonnet-4.6");
    assert_eq!(model.api, "anthropic-messages");

    let request = capture_copilot_request(None).await;

    // Auth: Bearer token, no x-api-key (the TS asserts apiKey null at the
    // SDK constructor).
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer tid_copilot_session_test_token")
    );
    assert!(header(&request, "x-api-key").is_none());

    // Copilot static headers from model.headers.
    let user_agent = header(&request, "User-Agent").expect("User-Agent");
    assert!(user_agent.contains("GitHubCopilotChat"));
    assert_eq!(
        header(&request, "Copilot-Integration-Id"),
        Some("vscode-chat")
    );

    // Dynamic headers.
    assert_eq!(header(&request, "X-Initiator"), Some("user"));
    assert_eq!(
        header(&request, "Openai-Intent"),
        Some("conversation-edits")
    );

    // No fine-grained-tool-streaming (Copilot doesn't support it).
    let beta = header(&request, "anthropic-beta").unwrap_or("");
    assert!(!beta.contains("fine-grained-tool-streaming"));

    // Payload is valid Anthropic Messages format.
    let body = body_of(&request);
    assert_eq!(body["model"], json!("claude-sonnet-4.6"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["max_tokens"], json!(model.max_tokens));
    assert!(body["messages"].as_array().is_some());
}

#[tokio::test]
async fn omits_interleaved_thinking_beta_for_adaptive_thinking_models() {
    let request = capture_copilot_request(Some(true)).await;

    let beta = header(&request, "anthropic-beta").unwrap_or("");
    assert!(!beta.contains("interleaved-thinking-2025-05-14"));
}

// --- anthropic-tool-name-normalization.test.ts -----------------------------------
//
// The TS suite is gated on a live Anthropic OAuth token
// (`describe.skipIf(!oauthToken)`), but the behavior under test is pure
// name mapping applied whenever the api key is OAuth-shaped
// (`sk-ant-oat`), so the port exercises it offline with a mock transport.

fn named_tool(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: format!("The {name} tool"),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        constrained_sampling: None,
    }
}

async fn capture_oauth_tool_names(tools: &[Tool]) -> Vec<Value> {
    let model = builtin("anthropic", "claude-haiku-4-5");
    let fetch = Arc::new(CaptureFetch::new());
    let options = AnthropicOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("sk-ant-oat-test-token".to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = stream_anthropic(&model, &tools_context(tools), Some(&options))
        .result()
        .await;
    body_of(&fetch.request())["tools"]
        .as_array()
        .expect("tools array")
        .clone()
}

#[tokio::test]
async fn normalizes_user_defined_tools_matching_claude_code_names() {
    let tools = capture_oauth_tool_names(&[named_tool("todowrite")]).await;
    assert_eq!(tools[0]["name"], json!("TodoWrite"));
}

#[tokio::test]
async fn handles_pi_builtin_tool_names() {
    let tools = capture_oauth_tool_names(&[
        named_tool("read"),
        named_tool("write"),
        named_tool("edit"),
        named_tool("bash"),
    ])
    .await;
    let names: Vec<Value> = tools.iter().map(|tool| tool["name"].clone()).collect();
    assert_eq!(
        names,
        vec![json!("Read"), json!("Write"), json!("Edit"), json!("Bash")]
    );
}

#[tokio::test]
async fn does_not_map_find_to_glob() {
    // `find` is not a Claude Code tool name, so it must pass through.
    let tools = capture_oauth_tool_names(&[named_tool("find")]).await;
    assert_eq!(tools[0]["name"], json!("find"));
}

#[tokio::test]
async fn keeps_custom_tool_names_without_a_claude_code_match() {
    let tools = capture_oauth_tool_names(&[named_tool("my_custom_query")]).await;
    assert_eq!(tools[0]["name"], json!("my_custom_query"));
}

/// A stream whose assistant turn calls the `TodoWrite` tool; the emitted
/// tool call must map back onto the context's `todowrite` tool.
struct ToolCallSseFetch {
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for ToolCallSseFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let body = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}".to_string(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"TodoWrite\",\"input\":{}}}".to_string(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}".to_string(),
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}".to_string(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}".to_string(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}".to_string(),
        ]
        .join("\n\n");
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

#[tokio::test]
async fn round_trips_claude_code_tool_call_names_onto_the_context_tools() {
    let model = builtin("anthropic", "claude-haiku-4-5");
    let fetch = Arc::new(ToolCallSseFetch {
        requests: Mutex::new(Vec::new()),
    });
    let options = AnthropicOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("sk-ant-oat-test-token".to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let result = stream_anthropic(
        &model,
        &tools_context(&[named_tool("todowrite")]),
        Some(&options),
    )
    .result()
    .await;
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    let tool_calls: Vec<&AssistantContent> = result
        .content
        .iter()
        .filter(|block| matches!(block, AssistantContent::ToolCall(_)))
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert!(matches!(tool_calls[0], AssistantContent::ToolCall(call) if call.name == "todowrite"));
}
