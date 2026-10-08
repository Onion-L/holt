//! Port of the offline bedrock converse-stream tests:
//! `bedrock-thinking-payload.test.ts` (12 offline cases; the credentials-gated
//! E2E case skips like the TS `describe.skipIf`), `bedrock-convert-messages`,
//! `bedrock-raw-stop-reason.test.ts`, `bedrock-redacted-reasoning.test.ts`,
//! and `bedrock-error-metadata.test.ts`.
//!
//! The TS suites mock `@aws-sdk/client-bedrock-runtime`; the Rust port drives
//! the same pipeline through `stream_from_items`, which replaces the mocked
//! SDK dispatch. The two "skips unknown content blocks" cases have no Rust
//! counterpart: the closed content enums cannot carry unknown block types.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::bedrock_converse_stream::{
    BedrockDispatchResponse, BedrockError, BedrockOptions, stream_from_items,
};
use pi_core::ai::types::{
    AssistantContent, CacheRetention, Context, Message, Model, ProviderRequestOptions, StopReason,
    StreamOptions, ThinkingLevel, Tool, UserContent, UserMessage,
};
use serde_json::{Value, json};

fn base_model(id: &str) -> Model {
    pi_core::ai::providers::builtin::get_builtin_model("amazon-bedrock", id)
        .unwrap_or_else(|| panic!("missing bedrock model {id}"))
}

fn empty_items() -> BedrockDispatchResponse {
    BedrockDispatchResponse {
        http_status_code: Some(200),
        request_id: Some("request-id".to_string()),
        raw_headers: None,
        items: Vec::new(),
        send_error: None,
        stream_error: None,
    }
}

fn ok_items(items: Vec<Value>) -> BedrockDispatchResponse {
    BedrockDispatchResponse {
        items,
        ..empty_items()
    }
}

/// The `capturePayload` helper: an already-aborted signal so the pipeline
/// stops right after the payload is observed (the TS tests throw instead).
async fn capture_payload(model: &Model, options: BedrockOptions) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let mut options = options;
    options.base.base.signal = Some(signal);
    let on_payload: pi_core::ai::types::OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload);
            Box::pin(std::future::ready(None))
        })
    };
    options.base.base.on_payload = Some(on_payload);

    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    };
    let _ = stream_from_items(model, &context, Some(&options), empty_items())
        .result()
        .await;
    captured.lock().unwrap().take().expect("payload captured")
}

fn reasoning_options(reasoning: ThinkingLevel) -> BedrockOptions {
    BedrockOptions {
        base: StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
        reasoning: Some(reasoning),
        ..Default::default()
    }
}

async fn additional_fields(model: &Model, options: BedrockOptions) -> Value {
    capture_payload(model, options)
        .await
        .get("additionalModelRequestFields")
        .cloned()
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Bedrock thinking payload

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_opus_48_when_reasoning_is_enabled() {
    let mut model = base_model("global.anthropic.claude-opus-4-6-v1");
    model.id = "global.anthropic.claude-opus-4-8-v1".to_string();
    model.name = "Claude Opus 4.8 (Global)".to_string();

    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(
        fields["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(fields["output_config"], json!({ "effort": "high" }));
    assert!(fields.get("anthropic_beta").is_none());
}

#[tokio::test]
async fn maps_xhigh_reasoning_to_effort_xhigh_for_claude_opus_48() {
    let mut model = base_model("global.anthropic.claude-opus-4-6-v1");
    model.id = "global.anthropic.claude-opus-4-8-v1".to_string();
    model.name = "Claude Opus 4.8 (Global)".to_string();

    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::Xhigh)).await;
    assert_eq!(fields["output_config"], json!({ "effort": "xhigh" }));
    assert!(fields.get("anthropic_beta").is_none());
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_fable_5() {
    let model = base_model("global.anthropic.claude-fable-5");
    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(
        fields["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(fields["output_config"], json!({ "effort": "high" }));
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_sonnet_5() {
    let model = base_model("global.anthropic.claude-sonnet-5");
    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(fields["output_config"], json!({ "effort": "high" }));
    assert!(fields.get("anthropic_beta").is_none());
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_opus_5() {
    let model = base_model("global.anthropic.claude-opus-5");
    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(fields["output_config"], json!({ "effort": "high" }));
}

#[tokio::test]
async fn maps_xhigh_reasoning_to_effort_xhigh_for_claude_opus_5() {
    let model = base_model("global.anthropic.claude-opus-5");
    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::Xhigh)).await;
    assert_eq!(fields["output_config"], json!({ "effort": "xhigh" }));
}

#[tokio::test]
async fn maps_xhigh_reasoning_to_effort_xhigh_for_claude_fable_5() {
    let model = base_model("global.anthropic.claude-fable-5");
    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::Xhigh)).await;
    assert_eq!(fields["output_config"], json!({ "effort": "xhigh" }));
}

#[tokio::test]
async fn omits_display_for_govcloud_model_ids_on_non_adaptive_claude_thinking() {
    let mut model = base_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    model.id = "us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string();
    model.name = "Claude Sonnet 4.5 (GovCloud)".to_string();

    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(
        fields["thinking"],
        json!({ "type": "enabled", "budget_tokens": 16384 })
    );
    assert_eq!(
        fields["anthropic_beta"],
        json!(["interleaved-thinking-2025-05-14"])
    );
}

#[tokio::test]
async fn omits_display_for_govcloud_regions_on_adaptive_claude_thinking() {
    let mut model = base_model("global.anthropic.claude-opus-4-6-v1");
    model.id = "global.anthropic.claude-opus-4-8-v1".to_string();
    model.name = "Claude Opus 4.8 (Global)".to_string();

    let options = BedrockOptions {
        region: Some("us-gov-west-1".to_string()),
        ..reasoning_options(ThinkingLevel::High)
    };
    let fields = additional_fields(&model, options).await;
    assert_eq!(fields["thinking"], json!({ "type": "adaptive" }));
    assert_eq!(fields["output_config"], json!({ "effort": "high" }));
    assert!(fields.get("anthropic_beta").is_none());
}

#[tokio::test]
async fn uses_adaptive_thinking_when_model_name_contains_the_model_name_but_arn_does_not() {
    let mut model = base_model("global.anthropic.claude-opus-4-6-v1");
    model.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile"
        .to_string();
    model.name = "Claude Opus 4.6".to_string();

    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(
        fields["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(fields["output_config"], json!({ "effort": "high" }));
}

#[tokio::test]
async fn injects_cache_points_when_model_name_identifies_a_supported_claude_model() {
    let mut model = base_model("global.anthropic.claude-opus-4-6-v1");
    model.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile"
        .to_string();
    model.name = "Claude Sonnet 4.6".to_string();

    let context = Context {
        system_prompt: Some("You are helpful.".to_string()),
        messages: vec![Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    };
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let on_payload: pi_core::ai::types::OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload);
            Box::pin(std::future::ready(None))
        })
    };
    let options = BedrockOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                on_payload: Some(on_payload),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = stream_from_items(&model, &context, Some(&options), empty_items())
        .result()
        .await;
    let payload = captured.lock().unwrap().take().expect("payload captured");
    let system = payload["system"].as_array().unwrap();
    assert_eq!(system.len(), 2);
    assert!(system[1].get("cachePoint").is_some());
    let messages = payload["messages"].as_array().unwrap();
    let last = messages.last().unwrap()["content"].as_array().unwrap();
    assert!(last.last().unwrap().get("cachePoint").is_some());
}

#[tokio::test]
async fn falls_back_to_fixed_budget_thinking_for_non_adaptive_claude_via_model_name() {
    let mut model = base_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    model.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile"
        .to_string();
    model.name = "Claude Sonnet 4.5".to_string();

    let fields = additional_fields(&model, reasoning_options(ThinkingLevel::High)).await;
    assert_eq!(fields["thinking"]["type"], json!("enabled"));
    assert!(fields["thinking"]["budget_tokens"].as_u64().is_some());
    assert_eq!(
        fields["anthropic_beta"],
        json!(["interleaved-thinking-2025-05-14"])
    );
}

// ---------------------------------------------------------------------------
// Bedrock raw stop reasons

fn stop_reason_items(stop_reason: &str) -> Vec<Value> {
    vec![
        json!({ "messageStart": { "role": "assistant" } }),
        json!({ "messageStop": { "stopReason": stop_reason } }),
    ]
}

async fn run_stop_reason(model: &Model, stop_reason: &str) -> pi_core::ai::types::AssistantMessage {
    stream_from_items(
        model,
        &Context {
            messages: vec![Message::User(UserMessage {
                role: pi_core::ai::types::RoleUser,
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            ..Default::default()
        },
        Some(&BedrockOptions {
            base: StreamOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        }),
        ok_items(stop_reason_items(stop_reason)),
    )
    .result()
    .await
}

#[tokio::test]
async fn preserves_raw_bedrock_stop_reasons_for_successful_stops() {
    let model = base_model("us.anthropic.claude-opus-4-8");
    let message = run_stop_reason(&model, "end_turn").await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("end_turn"));
    assert!(message.error_message.is_none());
}

#[tokio::test]
async fn preserves_raw_bedrock_stop_reasons_for_provider_error_stops() {
    let model = base_model("us.anthropic.claude-opus-4-8");
    let message = run_stop_reason(&model, "guardrail_intervened").await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.raw_stop_reason.as_deref(),
        Some("guardrail_intervened")
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: guardrail_intervened")
    );
}

// ---------------------------------------------------------------------------
// Bedrock redacted reasoning

const REDACTED_BASE64: &str = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";

fn redacted_bytes() -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(REDACTED_BASE64)
        .unwrap()
}

fn gpt_model() -> Model {
    Model {
        id: "global.openai.gpt-5.6-terra".to_string(),
        name: "GPT-5.6 Terra (Global)".to_string(),
        api: "bedrock-converse-stream".to_string(),
        provider: "amazon-bedrock".to_string(),
        base_url: "https://bedrock-runtime.ap-northeast-1.amazonaws.com".to_string(),
        reasoning: true,
        input: vec![pi_core::ai::types::ModelInput::Text],
        context_window: 400_000,
        max_tokens: 128_000,
        ..Default::default()
    }
}

fn redacted_reasoning_events() -> Vec<Value> {
    vec![
        json!({ "messageStart": { "role": "assistant" } }),
        json!({
            "contentBlockDelta": {
                "contentBlockIndex": 0,
                "delta": { "reasoningContent": { "redactedContent": redacted_bytes() } },
            }
        }),
        json!({ "contentBlockStop": { "contentBlockIndex": 0 } }),
        json!({ "contentBlockDelta": { "contentBlockIndex": 1, "delta": { "text": "done" } } }),
        json!({ "contentBlockStop": { "contentBlockIndex": 1 } }),
        json!({ "messageStop": { "stopReason": "end_turn" } }),
    ]
}

async fn run_events(model: &Model, items: Vec<Value>) -> pi_core::ai::types::AssistantMessage {
    stream_from_items(
        model,
        &Context {
            messages: vec![Message::User(UserMessage {
                role: pi_core::ai::types::RoleUser,
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            ..Default::default()
        },
        Some(&BedrockOptions {
            base: StreamOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        }),
        ok_items(items),
    )
    .result()
    .await
}

fn thinking_of(
    message: &pi_core::ai::types::AssistantMessage,
) -> Option<&pi_core::ai::types::ThinkingContent> {
    message.content.iter().find_map(|c| match c {
        AssistantContent::Thinking(thinking) => Some(thinking),
        _ => None,
    })
}

#[tokio::test]
async fn does_not_fail_the_stream_when_reasoning_arrives_as_redacted_content() {
    let response = run_events(&gpt_model(), redacted_reasoning_events()).await;
    assert_ne!(response.stop_reason, StopReason::Error);
    let types: Vec<&str> = response
        .content
        .iter()
        .map(|c| match c {
            AssistantContent::Text(_) => "text",
            AssistantContent::Thinking(_) => "thinking",
            AssistantContent::ToolCall(_) => "toolCall",
        })
        .collect();
    assert_eq!(types, vec!["thinking", "text"]);
    assert_eq!(
        response.content[1],
        AssistantContent::Text(pi_core::ai::types::TextContent {
            text: "done".to_string(),
            ..Default::default()
        })
    );
}

#[tokio::test]
async fn preserves_the_encrypted_reasoning_payload_on_the_assistant_message() {
    let response = run_events(&gpt_model(), redacted_reasoning_events()).await;
    let thinking = thinking_of(&response).expect("thinking block");
    assert_eq!(thinking.redacted, Some(true));
    assert_eq!(
        thinking.thinking_signature.as_deref(),
        Some(REDACTED_BASE64)
    );
    assert_eq!(thinking.thinking, "[Reasoning redacted]");
}

#[tokio::test]
async fn encodes_the_payload_when_the_stream_never_sends_content_block_stop() {
    let items = vec![
        json!({ "messageStart": { "role": "assistant" } }),
        json!({
            "contentBlockDelta": {
                "contentBlockIndex": 0,
                "delta": { "reasoningContent": { "redactedContent": redacted_bytes() } },
            }
        }),
        json!({ "messageStop": { "stopReason": "end_turn" } }),
    ];
    let response = run_events(&gpt_model(), items).await;
    let thinking = thinking_of(&response).expect("thinking block");
    assert_eq!(
        thinking.thinking_signature.as_deref(),
        Some(REDACTED_BASE64)
    );
}

#[tokio::test]
async fn joins_encrypted_reasoning_split_across_deltas() {
    let bytes = redacted_bytes();
    let (head, tail) = bytes.split_at(7);
    let items = vec![
        json!({ "messageStart": { "role": "assistant" } }),
        json!({
            "contentBlockDelta": {
                "contentBlockIndex": 0,
                "delta": { "reasoningContent": { "redactedContent": head } },
            }
        }),
        json!({
            "contentBlockDelta": {
                "contentBlockIndex": 0,
                "delta": { "reasoningContent": { "redactedContent": tail } },
            }
        }),
        json!({ "contentBlockStop": { "contentBlockIndex": 0 } }),
        json!({ "messageStop": { "stopReason": "end_turn" } }),
    ];
    let response = run_events(&gpt_model(), items).await;
    let thinking = thinking_of(&response).expect("thinking block");
    assert_eq!(
        thinking.thinking_signature.as_deref(),
        Some(REDACTED_BASE64)
    );
    // The placeholder marks the block once, not once per delta.
    assert_eq!(thinking.thinking, "[Reasoning redacted]");
}

async fn capture_messages_payload(messages: Vec<Message>) -> Value {
    let model = gpt_model();
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let on_payload: pi_core::ai::types::OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload);
            Box::pin(std::future::ready(None))
        })
    };
    let options = BedrockOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                signal: Some(signal),
                on_payload: Some(on_payload),
                ..Default::default()
            },
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = stream_from_items(
        &model,
        &Context {
            messages,
            ..Default::default()
        },
        Some(&options),
        empty_items(),
    )
    .result()
    .await;
    captured.lock().unwrap().take().expect("payload captured")
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: pi_core::ai::types::RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn assistant_message(content: Vec<AssistantContent>) -> Message {
    Message::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        content,
        api: "bedrock-converse-stream".to_string(),
        provider: "amazon-bedrock".to_string(),
        model: "global.openai.gpt-5.6-terra".to_string(),
        stop_reason: StopReason::Stop,
        timestamp: 0,
        ..Default::default()
    }))
}

fn redacted_thinking() -> pi_core::ai::types::ThinkingContent {
    pi_core::ai::types::ThinkingContent {
        thinking: String::new(),
        thinking_signature: Some(REDACTED_BASE64.to_string()),
        redacted: Some(true),
        ..Default::default()
    }
}

#[tokio::test]
async fn replays_redacted_reasoning_as_reasoning_content_redacted_content() {
    let messages = vec![
        user_message("hello"),
        assistant_message(vec![
            AssistantContent::Thinking(redacted_thinking()),
            AssistantContent::Text(pi_core::ai::types::TextContent {
                text: "done".to_string(),
                ..Default::default()
            }),
        ]),
        user_message("continue"),
    ];
    let payload = capture_messages_payload(messages).await;
    let assistant = payload["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == json!("assistant"))
        .cloned()
        .unwrap();
    assert_eq!(
        assistant["content"],
        json!([
            { "reasoningContent": { "redactedContent": redacted_bytes() } },
            { "text": "done" },
        ])
    );
}

#[tokio::test]
async fn replays_redacted_reasoning_before_the_tool_use_block_it_belongs_to() {
    let messages = vec![
        user_message("read the file"),
        assistant_message(vec![
            AssistantContent::Thinking(redacted_thinking()),
            AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
                id: "tool-1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({ "path": "/tmp/a.txt" })
                    .as_object()
                    .cloned()
                    .unwrap(),
                ..Default::default()
            }),
        ]),
        Message::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
            role: pi_core::ai::types::RoleToolResult,
            tool_call_id: "tool-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![pi_core::ai::types::BlockContent::Text(
                pi_core::ai::types::TextContent {
                    text: "file body".to_string(),
                    ..Default::default()
                },
            )],
            is_error: false,
            timestamp: 0,
            details: None,
            usage: None,
            added_tool_names: None,
        })),
    ];
    let payload = capture_messages_payload(messages).await;
    let assistant = payload["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == json!("assistant"))
        .cloned()
        .unwrap();
    assert_eq!(
        assistant["content"],
        json!([
            { "reasoningContent": { "redactedContent": redacted_bytes() } },
            { "toolUse": { "toolUseId": "tool-1", "name": "read", "input": { "path": "/tmp/a.txt" } } },
        ])
    );
}

// ---------------------------------------------------------------------------
// Bedrock failure diagnostics (error-metadata)

const DIAGNOSTIC_TYPE: &str = "bedrock_response_failure";
const VALIDATION_MESSAGE: &str = "The provided model identifier is invalid.";
const REQUEST_ID: &str = "11111111-2222-3333-4444-555555555555";

fn metadata_model() -> Model {
    base_model("us.anthropic.claude-opus-4-8")
}

async fn run_dispatch(
    response: BedrockDispatchResponse,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> pi_core::ai::types::AssistantMessage {
    stream_from_items(
        &metadata_model(),
        &Context {
            messages: vec![user_message("hello")],
            ..Default::default()
        },
        Some(&BedrockOptions {
            base: StreamOptions {
                cache_retention: Some(CacheRetention::None),
                base: ProviderRequestOptions {
                    signal,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
        response,
    )
    .result()
    .await
}

/// The SDK's `handleError` shape for a non-2xx response.
fn service_exception(
    name: &str,
    http_status: Option<u16>,
    request_id: Option<&str>,
) -> BedrockError {
    BedrockError {
        name: name.to_string(),
        message: VALIDATION_MESSAGE.to_string(),
        http_status_code: http_status,
        request_id: request_id.map(str::to_string),
        service_exception: true,
        ..Default::default()
    }
}

/// Fails after `messageStart`, throwing from inside the iterator.
fn failing_stream(thrown: BedrockError) -> BedrockDispatchResponse {
    BedrockDispatchResponse {
        http_status_code: Some(200),
        request_id: Some(REQUEST_ID.to_string()),
        raw_headers: None,
        items: vec![json!({ "messageStart": { "role": "assistant" } })],
        send_error: None,
        stream_error: Some(thrown),
    }
}

fn find_diagnostic(message: &pi_core::ai::types::AssistantMessage) -> Option<Value> {
    message
        .diagnostics
        .as_ref()?
        .iter()
        .find(|diagnostic| diagnostic.kind == DIAGNOSTIC_TYPE)
        .and_then(|diagnostic| diagnostic.details.clone())
}

#[tokio::test]
async fn records_status_error_code_and_request_id_for_a_non_2xx_from_send() {
    let response = BedrockDispatchResponse {
        send_error: Some(service_exception(
            "ValidationException",
            Some(400),
            Some(REQUEST_ID),
        )),
        ..empty_items()
    };
    let message = run_dispatch(response, None).await;
    let diagnostic = find_diagnostic(&message).expect("diagnostic");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        diagnostic,
        json!({ "status": 400, "errorCode": "ValidationException", "requestId": REQUEST_ID })
    );
}

#[tokio::test]
async fn leaves_error_message_untouched_so_retry_classification_is_unaffected() {
    let response = BedrockDispatchResponse {
        send_error: Some(service_exception(
            "ValidationException",
            Some(400),
            Some(REQUEST_ID),
        )),
        ..empty_items()
    };
    let message = run_dispatch(response, None).await;
    assert_eq!(
        message.error_message.as_deref(),
        Some(format!("Validation error: {VALIDATION_MESSAGE}").as_str())
    );
}

#[tokio::test]
async fn reports_only_the_request_id_for_a_modeled_mid_stream_exception() {
    let thrown = BedrockError {
        message: "Too many requests, please wait.".to_string(),
        ..Default::default()
    };
    let message = run_dispatch(failing_stream(thrown), None).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        find_diagnostic(&message).expect("diagnostic"),
        json!({ "requestId": REQUEST_ID })
    );
}

#[tokio::test]
async fn captures_the_error_code_for_an_unmodeled_mid_stream_error() {
    let thrown = BedrockError {
        name: "ModelStreamErrorException".to_string(),
        message: "Model stream terminated unexpectedly.".to_string(),
        service_exception: true,
        ..Default::default()
    };
    let message = run_dispatch(failing_stream(thrown), None).await;
    assert_eq!(
        find_diagnostic(&message).expect("diagnostic"),
        json!({ "errorCode": "ModelStreamErrorException", "requestId": REQUEST_ID })
    );
}

#[tokio::test]
async fn does_not_report_a_transport_failure_name_as_a_provider_error_code() {
    let thrown = BedrockError {
        name: "TimeoutError".to_string(),
        message: "Connection timed out after 1000 ms".to_string(),
        ..Default::default()
    };
    let message = run_dispatch(failing_stream(thrown), None).await;
    assert_eq!(
        find_diagnostic(&message).expect("diagnostic"),
        json!({ "requestId": REQUEST_ID })
    );
}

#[tokio::test]
async fn emits_no_diagnostic_when_the_failure_carries_no_provider_metadata() {
    let response = BedrockDispatchResponse {
        send_error: Some(BedrockError::transport("socket hang up")),
        ..empty_items()
    };
    let message = run_dispatch(response, None).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("socket hang up"));
    assert!(find_diagnostic(&message).is_none());
}

#[tokio::test]
async fn emits_no_diagnostic_for_an_aborted_turn() {
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let response = BedrockDispatchResponse {
        send_error: Some(service_exception(
            "ValidationException",
            Some(400),
            Some(REQUEST_ID),
        )),
        ..empty_items()
    };
    let message = run_dispatch(response, Some(signal)).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert!(find_diagnostic(&message).is_none());
}

#[tokio::test]
async fn drops_header_derived_values_that_exceed_the_length_bound() {
    let long_code: String = "E".repeat(5000) + "Exception";
    let response = BedrockDispatchResponse {
        send_error: Some(service_exception(
            &long_code,
            Some(400),
            Some(&"R".repeat(5000)),
        )),
        ..empty_items()
    };
    let message = run_dispatch(response, None).await;
    assert_eq!(
        find_diagnostic(&message).expect("diagnostic"),
        json!({ "status": 400 })
    );
}

#[tokio::test]
async fn omits_the_sdk_unknown_placeholder_instead_of_reporting_it_as_a_code() {
    let response = BedrockDispatchResponse {
        send_error: Some(service_exception("Unknown", Some(403), Some(REQUEST_ID))),
        ..empty_items()
    };
    let message = run_dispatch(response, None).await;
    assert_eq!(
        find_diagnostic(&message).expect("diagnostic"),
        json!({ "status": 403, "requestId": REQUEST_ID })
    );
}

// ---------------------------------------------------------------------------
// Provider error body passthrough (provider-error-body-regression.test.ts,
// bedrock tier). The TypeScript suite mocks the AWS SDK so `client.send()`
// rejects with a ServiceException carrying `$metadata.httpStatusCode` and a
// `$response.body` string; the Rust port models the same SDK error as a
// `BedrockError` with extracted `status`/`body` fields.

/// `streamSimpleBedrock(model, { messages: context.messages }, {})` with a
/// `send()` rejection, as in the TypeScript regression suite.
async fn run_send_error(
    model_id: &str,
    error: BedrockError,
) -> pi_core::ai::types::AssistantMessage {
    stream_from_items(
        &base_model(model_id),
        &Context {
            messages: vec![Message::User(UserMessage {
                role: pi_core::ai::types::RoleUser,
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })],
            ..Default::default()
        },
        Some(&BedrockOptions {
            base: StreamOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        }),
        BedrockDispatchResponse {
            send_error: Some(error),
            ..empty_items()
        },
    )
    .result()
    .await
}

#[tokio::test]
async fn surfaces_the_gateway_body_instead_of_unknown_unknown_error() {
    // Object.assign(new Error("UnknownError"), { name: "UnknownError",
    // $metadata: { httpStatusCode: 403 }, $response: { statusCode: 403,
    // body: '{"message":"blocked by gateway WAF"}' } })
    let message = run_send_error(
        "us.anthropic.claude-opus-4-8",
        BedrockError {
            name: "UnknownError".to_string(),
            message: "UnknownError".to_string(),
            status: Some(403),
            body: Some(r#"{"message":"blocked by gateway WAF"}"#.to_string()),
            service_exception: true,
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.expect("error message present");
    assert!(error_message.contains("403"));
    assert!(error_message.contains("blocked by gateway WAF"));
    assert!(!error_message.contains("Unknown: UnknownError"));
}

// The TypeScript fixture's `$response.body` is a node stream (`pipe`
// function), which normalizeProviderError ignores so the SDK validation
// message survives. The stream-sniffing mechanic has no Rust analog: a typed
// `BedrockError` simply carries no body, which is the same observable
// outcome — the message is preserved verbatim.
#[tokio::test]
async fn preserves_the_sdk_validation_message_when_the_response_body_is_a_stream() {
    let message = run_send_error(
        "global.anthropic.claude-opus-5",
        BedrockError {
            name: "ValidationException".to_string(),
            message: "Invocation of model ID anthropic.claude-opus-5 with on-demand throughput isn't supported. Retry with an inference profile.".to_string(),
            status: Some(400),
            body: None,
            service_exception: true,
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.expect("error message present");
    assert!(error_message.contains("on-demand throughput isn't supported"));
    assert!(error_message.contains("inference profile"));
    assert!(!error_message.contains("_readableState"));
}

// ---------------------------------------------------------------------------
// Bedrock constrained sampling and message conversion

async fn capture_tool_payload(context: Context, model: &Model) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let on_payload: pi_core::ai::types::OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload);
            Box::pin(std::future::ready(None))
        })
    };
    let options = BedrockOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                signal: Some(signal),
                on_payload: Some(on_payload),
                ..Default::default()
            },
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = stream_from_items(model, &context, Some(&options), empty_items())
        .result()
        .await;
    captured.lock().unwrap().take().expect("payload captured")
}

fn lookup_tool(strict: &str) -> Tool {
    Tool {
        name: "lookup".to_string(),
        description: "Look up a value".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
        }),
        constrained_sampling: Some(pi_core::ai::types::ToolConstrainedSampling::Config(
            pi_core::ai::types::ConstrainedSamplingConfig::JsonSchema {
                strict: match strict {
                    "require" => pi_core::ai::types::ConstrainedSamplingStrict::Require,
                    _ => pi_core::ai::types::ConstrainedSamplingStrict::Prefer,
                },
            },
        )),
    }
}

#[tokio::test]
async fn gates_native_strict_tool_use_by_model_capability() {
    let model = base_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    let context = Context {
        messages: vec![user_message("Use the tool")],
        tools: Some(vec![lookup_tool("require")]),
        ..Default::default()
    };
    let payload = capture_tool_payload(context, &model).await;
    assert_eq!(
        payload["toolConfig"]["tools"][0]["toolSpec"]["strict"],
        json!(true)
    );

    let mut nova = base_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    nova.id = "amazon.nova-lite-v1:0".to_string();
    nova.name = "Nova Lite".to_string();
    nova.reasoning = false;
    nova.compat = None;
    let context = Context {
        messages: vec![user_message("Use the tool")],
        tools: Some(vec![lookup_tool("prefer")]),
        ..Default::default()
    };
    let payload = capture_tool_payload(context, &nova).await;
    assert!(
        payload["toolConfig"]["tools"][0]["toolSpec"]
            .get("strict")
            .is_none()
    );
}

#[tokio::test]
async fn preserves_empty_property_names_in_streamed_tool_arguments() {
    let model = base_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    let items = vec![
        json!({ "messageStart": { "role": "assistant" } }),
        json!({
            "contentBlockStart": {
                "contentBlockIndex": 0,
                "start": { "toolUse": { "toolUseId": "tool-1", "name": "edit" } },
            }
        }),
        json!({
            "contentBlockDelta": {
                "contentBlockIndex": 0,
                "delta": {
                    "toolUse": {
                        "input": "{\"path\":\"/workspace/foobar/file.js\",\"edits\":[{\"oldText\":\"first\",\"newText\":\"updated first\"},{\"oldText\":\"second\",\"newText\":\"updated second\",\"\":\"\"}]}"
                    }
                },
            }
        }),
        json!({ "contentBlockStop": { "contentBlockIndex": 0 } }),
        json!({ "messageStop": { "stopReason": "tool_use" } }),
    ];
    let message = run_events(&model, items).await;
    match &message.content[0] {
        AssistantContent::ToolCall(call) => {
            assert_eq!(call.id, "tool-1");
            assert_eq!(call.name, "edit");
            assert_eq!(
                serde_json::Value::Object(call.arguments.clone()),
                json!({
                    "path": "/workspace/foobar/file.js",
                    "edits": [
                        { "oldText": "first", "newText": "updated first" },
                        { "oldText": "second", "newText": "updated second", "": "" },
                    ],
                })
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

async fn converted_messages(context: Context, model: &Model) -> Vec<Value> {
    capture_tool_payload(context, model).await["messages"]
        .as_array()
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn replaces_user_messages_with_only_unknown_or_blank_content_with_a_placeholder() {
    let model = base_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    // Blank string content.
    let messages = converted_messages(
        Context {
            messages: vec![user_message("   ")],
            ..Default::default()
        },
        &model,
    )
    .await;
    assert_eq!(messages[0]["content"], json!([{ "text": "<empty>" }]));

    // Blank text blocks when other content remains are filtered.
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: pi_core::ai::types::RoleUser,
            content: UserContent::Blocks(vec![
                pi_core::ai::types::BlockContent::Text(pi_core::ai::types::TextContent {
                    text: String::new(),
                    ..Default::default()
                }),
                pi_core::ai::types::BlockContent::Text(pi_core::ai::types::TextContent {
                    text: "hello".to_string(),
                    ..Default::default()
                }),
            ]),
            timestamp: 0,
        })],
        ..Default::default()
    };
    let messages = converted_messages(context, &model).await;
    assert_eq!(messages[0]["content"], json!([{ "text": "hello" }]));
}

// ---------------------------------------------------------------------------
// Wire transport: response headers forwarded to onResponse
// (bedrock-response-headers.test.ts drives a local HTTP server with
// AWS_BEDROCK_SKIP_AUTH=1).

#[tokio::test]
async fn forwards_raw_response_headers_to_on_response() {
    use pi_core::ai::api::bedrock_converse_stream::stream as stream_bedrock;

    const MODEL_ID: &str = "us.anthropic.claude-haiku-4-5-20251001-v1:0";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 8192];
        use tokio::io::AsyncReadExt;
        let _ = socket.read(&mut buffer).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\n\
             x-bifrost-provider: bedrock\r\nx-bifrost-resolved-model: {MODEL_ID}\r\n\
             x-amzn-requestid: req-123\r\ncontent-length: 0\r\n\r\n"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let mut model = base_model(MODEL_ID);
    model.base_url = format!("http://{addr}");
    let responses: Arc<Mutex<Vec<pi_core::ai::types::ProviderResponse>>> =
        Arc::new(Mutex::new(Vec::new()));
    let on_response: pi_core::ai::types::OnResponseCallback = {
        let responses = Arc::clone(&responses);
        Arc::new(
            move |response: &pi_core::ai::types::ProviderResponse, _model| {
                let response = response.clone();
                let responses = Arc::clone(&responses);
                Box::pin(async move {
                    responses.lock().unwrap().push(response);
                })
            },
        )
    };
    let mut env = serde_json::Map::new();
    env.insert("AWS_BEDROCK_SKIP_AUTH".to_string(), serde_json::json!("1"));
    let env: pi_core::ai::types::ProviderEnv = env
        .into_iter()
        .map(|(key, value)| (key, value.as_str().unwrap_or_default().to_string()))
        .collect();

    let result = stream_bedrock(
        &model,
        &Context {
            messages: vec![user_message("hello")],
            ..Default::default()
        },
        Some(&BedrockOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    env: Some(env),
                    on_response: Some(on_response),
                    ..Default::default()
                },
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .result()
    .await;
    server.await.unwrap();

    // The fake server returns an empty event stream; the header callback
    // still fires before stream consumption.
    assert_eq!(result.stop_reason, StopReason::Error);
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].status, 200);
    assert_eq!(
        responses[0]
            .headers
            .get("x-amzn-requestid")
            .map(String::as_str),
        Some("req-123")
    );
    assert_eq!(
        responses[0]
            .headers
            .get("x-bifrost-provider")
            .map(String::as_str),
        Some("bedrock")
    );
    assert_eq!(
        responses[0]
            .headers
            .get("x-bifrost-resolved-model")
            .map(String::as_str),
        Some(MODEL_ID)
    );
}

#[tokio::test]
async fn wire_stream_decodes_framed_events_end_to_end() {
    use pi_core::ai::api::bedrock_converse_stream::stream as stream_bedrock;

    // A scripted transport speaking the event-stream wire format.
    struct FramedFetch;
    impl pi_core::ai::utils::http::HttpFetch for FramedFetch {
        fn fetch<'a>(
            &'a self,
            _request: pi_core::ai::utils::http::HttpRequest,
        ) -> futures::future::BoxFuture<
            'a,
            Result<
                pi_core::ai::utils::http::HttpResponse,
                pi_core::ai::utils::http::HttpFetchError,
            >,
        > {
            fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
                let mut header_bytes = Vec::new();
                for (name, value) in headers {
                    header_bytes.push(name.len() as u8);
                    header_bytes.extend_from_slice(name.as_bytes());
                    header_bytes.push(7);
                    header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
                    header_bytes.extend_from_slice(value.as_bytes());
                }
                let total = 12 + header_bytes.len() + payload.len() + 4;
                let mut frame = Vec::new();
                frame.extend_from_slice(&(total as u32).to_be_bytes());
                frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
                frame.extend_from_slice(&[0, 0, 0, 0]);
                frame.extend_from_slice(&header_bytes);
                frame.extend_from_slice(payload);
                frame.extend_from_slice(&[0, 0, 0, 0]);
                frame
            }
            let body = [
                encode_frame(
                    &[(":message-type", "event")],
                    br#"{"messageStart":{"role":"assistant"}}"#,
                ),
                encode_frame(
                    &[(":message-type", "event")],
                    br#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hi"}}}"#,
                ),
                encode_frame(
                    &[(":message-type", "event")],
                    br#"{"messageStop":{"stopReason":"end_turn"}}"#,
                ),
            ]
            .concat();
            Box::pin(async move {
                Ok(pi_core::ai::utils::http::HttpResponse {
                    status: 200,
                    headers: vec![(
                        "content-type".to_string(),
                        "application/vnd.amazon.eventstream".to_string(),
                    )],
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
                })
            })
        }
    }

    let model = base_model("us.anthropic.claude-opus-4-8");
    let result = stream_bedrock(
        &model,
        &Context {
            messages: vec![user_message("hello")],
            ..Default::default()
        },
        Some(&BedrockOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("bearer-token".to_string()),
                    fetch: Some(
                        Arc::new(FramedFetch) as Arc<dyn pi_core::ai::utils::http::HttpFetch>
                    ),
                    ..Default::default()
                },
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("end_turn"));
    match &result.content[0] {
        AssistantContent::Text(text) => assert_eq!(text.text, "hi"),
        other => panic!("expected text, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Bedrock catalog (bedrock-models.test.ts offline cases; the extensive
// per-model live suite is credentials-gated upstream too).

#[test]
fn bedrock_catalog_lists_models_via_inference_profiles() {
    let models = pi_core::ai::models_generated::models_for_provider("amazon-bedrock");
    assert!(!models.is_empty());
    assert!(
        models
            .iter()
            .any(|model| model.id == "global.anthropic.claude-opus-5")
    );
    assert!(
        !models
            .iter()
            .any(|model| model.id == "anthropic.claude-opus-5")
    );
}
