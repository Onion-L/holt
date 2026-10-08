//! Port of `pi-core/ai/test/anthropic-sse-parsing.test.ts`: raw SSE stream
//! parsing for the Anthropic provider, including malformed JSON repair and
//! streamed tool-JSON repair.

use std::sync::Arc;

use pi_core::ai::api::anthropic_messages::{AnthropicOptions, stream};
use pi_core::ai::types::{
    AssistantContent, Context, Message, Model, RoleUser, StopReason, Tool, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};

fn model() -> Model {
    Model {
        id: "claude-haiku-4-5".to_string(),
        name: "Claude Haiku 4.5".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning: true,
        input: vec![
            pi_core::ai::types::ModelInput::Text,
            pi_core::ai::types::ModelInput::Image,
        ],
        cost: pi_core::ai::types::ModelCost::default(),
        context_window: 200_000,
        max_tokens: 64_000,
        ..Default::default()
    }
}

fn context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("Say hello.".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

/// The mock transport standing in for the TS tests' fake SDK client.
struct SseMockFetch {
    body: String,
    requests: std::sync::Mutex<Vec<HttpRequest>>,
}

impl HttpFetch for SseMockFetch {
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

fn sse_options(fetch: Arc<SseMockFetch>) -> AnthropicOptions {
    AnthropicOptions {
        base: pi_core::ai::types::StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("sk-ant-test".to_string()),
                fetch: Some(fetch),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn create_sse_response(events: &[(String, String)]) -> String {
    events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_start(id: &str, input_tokens: u64) -> (String, String) {
    (
        "message_start".to_string(),
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": id,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                },
            },
        })
        .to_string(),
    )
}

fn message_delta(stop_reason: &str, output_tokens: u64) -> (String, String) {
    (
        "message_delta".to_string(),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason },
            "usage": {
                "input_tokens": 12,
                "output_tokens": output_tokens,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            },
        })
        .to_string(),
    )
}

fn message_stop() -> (String, String) {
    (
        "message_stop".to_string(),
        "{\"type\":\"message_stop\"}".to_string(),
    )
}

/// The shared `minimalAnthropicEvents` fixture: a text block assembled from
/// an empty start event plus one "Hello" delta.
fn minimal_anthropic_events() -> Vec<(String, String)> {
    vec![
        message_start("msg_test", 12),
        (
            "content_block_start".to_string(),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            })
            .to_string(),
        ),
        (
            "content_block_delta".to_string(),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hello" },
            })
            .to_string(),
        ),
        (
            "content_block_stop".to_string(),
            "{\"type\":\"content_block_stop\",\"index\":0}".to_string(),
        ),
        message_delta("end_turn", 5),
        message_stop(),
    ]
}

#[tokio::test]
async fn parses_minimal_stream_and_repairs_malformed_tool_json() {
    // The malformed delta embeds an invalid escape (`A\H`) and a raw tab; the
    // streamed-JSON repair path must recover {"path":"A\\H","text":"col1\tcol2"}.
    let malformed_tool_json_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#;

    let events = vec![
        message_start("msg_test", 12),
        (
            "content_block_start".to_string(),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": "toolu_test", "name": "edit", "input": {} },
            })
            .to_string(),
        ),
        ("content_block_delta".to_string(), malformed_tool_json_delta.to_string()),
        (
            "content_block_stop".to_string(),
            "{\"type\":\"content_block_stop\",\"index\":0}".to_string(),
        ),
        message_delta("tool_use", 5),
        message_stop(),
    ];

    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("Use the edit tool.".to_string()),
            timestamp: 0,
        })],
        tools: Some(vec![Tool {
            name: "edit".to_string(),
            description: "Edit a file.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "text": {"type": "string"}},
            }),
            constrained_sampling: None,
        }]),
        ..Default::default()
    };

    let result = stream(&model(), &context, Some(&sse_options(fetch)))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert!(result.error_message.is_none());

    let tool_call = result
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(tool_call) => Some(tool_call),
            _ => None,
        })
        .expect("tool call block");
    let arguments: serde_json::Value = serde_json::to_value(&tool_call.arguments).unwrap();
    assert_eq!(
        arguments,
        serde_json::json!({"path": "A\\H", "text": "col1\tcol2"})
    );
}

/// Fails the first request with an overloaded 529, then serves `body`.
struct OverloadedOnceFetch {
    body: String,
    calls: std::sync::atomic::AtomicU32,
}

impl HttpFetch for OverloadedOnceFetch {
    fn fetch<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let (status, body) = if first {
            (
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                    .to_string(),
            )
        } else {
            (200, self.body.clone())
        };
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: vec![("retry-after-ms".to_string(), "0".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

#[tokio::test]
async fn retries_an_overloaded_response_before_streaming() {
    let fetch = Arc::new(OverloadedOnceFetch {
        body: create_sse_response(&minimal_anthropic_events()),
        calls: std::sync::atomic::AtomicU32::new(0),
    });
    let mut options = AnthropicOptions {
        base: pi_core::ai::types::StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("sk-ant-test".to_string()),
                fetch: Some(fetch.clone()),
                max_retries: Some(1),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let result = stream(&model(), &context(), Some(&options)).result().await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(result.error_message.is_none());
    assert_eq!(fetch.calls.load(std::sync::atomic::Ordering::SeqCst), 2);

    // Without a retry budget the 529 still surfaces with its body.
    fetch.calls.store(0, std::sync::atomic::Ordering::SeqCst);
    options.base.base.max_retries = None;
    let result = stream(&model(), &context(), Some(&options)).result().await;
    assert_eq!(result.stop_reason, StopReason::Error);
    let message = result.error_message.unwrap();
    assert!(
        message.starts_with("Anthropic API error (529)"),
        "{message}"
    );
    assert!(message.contains("overloaded_error"), "{message}");
}

#[tokio::test]
async fn preserves_content_from_content_block_start_events() {
    let events = vec![
        message_start("msg_initial_content", 12),
        (
            "content_block_start".to_string(),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "Initial text" },
            })
            .to_string(),
        ),
        (
            "content_block_delta".to_string(),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": " plus delta" },
            })
            .to_string(),
        ),
        (
            "content_block_stop".to_string(),
            "{\"type\":\"content_block_stop\",\"index\":0}".to_string(),
        ),
        (
            "content_block_start".to_string(),
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "thinking",
                    "thinking": "Initial thinking",
                    "signature": "initial signature",
                },
            })
            .to_string(),
        ),
        (
            "content_block_delta".to_string(),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "thinking_delta", "thinking": " plus delta" },
            })
            .to_string(),
        ),
        (
            "content_block_delta".to_string(),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "signature_delta", "signature": " plus delta" },
            })
            .to_string(),
        ),
        (
            "content_block_stop".to_string(),
            "{\"type\":\"content_block_stop\",\"index\":1}".to_string(),
        ),
        message_delta("end_turn", 5),
        message_stop(),
    ];

    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = stream(&model(), &context(), Some(&sse_options(fetch)))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(
        result.content,
        vec![
            AssistantContent::Text(pi_core::ai::types::TextContent {
                text: "Initial text plus delta".to_string(),
                ..Default::default()
            }),
            AssistantContent::Thinking(pi_core::ai::types::ThinkingContent {
                thinking: "Initial thinking plus delta".to_string(),
                thinking_signature: Some("initial signature plus delta".to_string()),
                ..Default::default()
            }),
        ]
    );
    assert_eq!(result.response_id.as_deref(), Some("msg_initial_content"));
    assert_eq!(result.usage.input, 12);
    assert_eq!(result.usage.output, 5);
}

#[tokio::test]
async fn preserves_refusal_stop_details_from_message_delta() {
    let model = pi_core::ai::providers::builtin::get_builtin_model("anthropic", "claude-fable-5")
        .expect("claude-fable-5 model");
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("blocked request".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    };
    let explanation = "This request triggered restrictions on violative cyber content and was blocked under Anthropic's Usage Policy. To learn more, provide feedback, or request an exemption based on how you use Claude, visit our help center: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude.";
    let events = vec![
        (
            "message_start".to_string(),
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "msg_01XFUDYJgAACzvnptvVoYEL",
                    "usage": {
                        "input_tokens": 412,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                },
            })
            .to_string(),
        ),
        (
            "message_delta".to_string(),
            serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "refusal",
                    "stop_details": {
                        "type": "refusal",
                        "category": "cyber",
                        "explanation": explanation,
                    },
                },
                "usage": {
                    "input_tokens": 412,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                },
            })
            .to_string(),
        ),
        message_stop(),
    ];

    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = stream(&model, &context, Some(&sse_options(fetch)))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("refusal"));
    assert_eq!(result.error_message.as_deref(), Some(explanation));
}

#[tokio::test]
async fn preserves_sensitive_stop_reasons_with_a_descriptive_error_message() {
    let context = Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("blocked request".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    };
    let events = vec![
        message_start("msg_sensitive", 12),
        (
            "message_delta".to_string(),
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": "sensitive" },
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                },
            })
            .to_string(),
        ),
        message_stop(),
    ];

    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = stream(&model(), &context, Some(&sse_options(fetch)))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("sensitive"));
    assert_eq!(
        result.error_message.as_deref(),
        Some("Provider stopped with: sensitive")
    );
}

#[tokio::test]
async fn treats_message_delta_without_usage_as_a_no_op_for_usage_accumulation() {
    // The message_delta fixture drops its usage object entirely; input tokens
    // captured at message_start must survive.
    let events: Vec<(String, String)> = minimal_anthropic_events()
        .into_iter()
        .map(|event| {
            if event.0 == "message_delta" {
                (
                    "message_delta".to_string(),
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": "end_turn" },
                    })
                    .to_string(),
                )
            } else {
                event
            }
        })
        .collect();

    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = stream(&model(), &context(), Some(&sse_options(fetch)))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(result.error_message.is_none());
    assert_eq!(
        result.content,
        vec![AssistantContent::Text(pi_core::ai::types::TextContent {
            text: "Hello".to_string(),
            ..Default::default()
        })]
    );
    assert_eq!(result.usage.input, 12);
    assert_eq!(result.usage.total_tokens, 12);
}

#[tokio::test]
async fn ignores_unknown_sse_events_after_message_stop() {
    let mut events = minimal_anthropic_events();
    events.push(("done".to_string(), "[DONE]".to_string()));
    events.push(("proxy.stats".to_string(), "not json".to_string()));

    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = stream(&model(), &context(), Some(&sse_options(fetch)))
        .result()
        .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(result.error_message.is_none());
    assert_eq!(
        result.content,
        vec![AssistantContent::Text(pi_core::ai::types::TextContent {
            text: "Hello".to_string(),
            ..Default::default()
        })]
    );
}

#[tokio::test]
async fn request_targets_the_messages_endpoint_with_sdk_headers() {
    let events = vec![
        message_start("msg_x", 1),
        message_delta("end_turn", 1),
        message_stop(),
    ];
    let fetch = Arc::new(SseMockFetch {
        body: create_sse_response(&events),
        requests: std::sync::Mutex::new(Vec::new()),
    });

    let result = stream(&model(), &context(), Some(&sse_options(fetch.clone())))
        .result()
        .await;
    assert_eq!(result.stop_reason, StopReason::Stop);

    let requests = fetch.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.url, "https://api.anthropic.com/v1/messages");
    assert!(matches!(request.body, HttpBody::Json(_)));
    let headers = &request.headers;
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("x-api-key").as_deref(), Some("sk-ant-test"));
    assert_eq!(header("anthropic-version"), Some("2023-06-01".to_string()));
    assert_eq!(header("content-type").as_deref(), Some("application/json"));
    let body = match &request.body {
        HttpBody::Json(value) => value.clone(),
        _ => unreachable!(),
    };
    assert_eq!(body.get("stream"), Some(&serde_json::json!(true)));
    assert_eq!(
        body.get("model"),
        Some(&serde_json::json!("claude-haiku-4-5"))
    );
}
