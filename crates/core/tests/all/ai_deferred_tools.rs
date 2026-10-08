//! Port of `pi-core/ai/test/deferred-tools.test.ts` (all 24 cases: the
//! `it.each` model list becomes three focused tests, matching the file's
//! per-case style).
//!
//! The TypeScript suite captures the outgoing payload through the `onPayload`
//! hook (which throws to short-circuit the request); the Rust port captures
//! the same JSON body through a mock `HttpFetch` transport, which is the same
//! observable surface. The `convertMessages` case calls the Rust conversion
//! helper directly with the resolved compat literal, and the estimation case
//! calls `estimate_context_tokens` directly, exactly like the TypeScript
//! suite. Everything runs offline against `http://127.0.0.1:9`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use pi_core::ai::api::openai_completions::{ResolvedCompletionsCompat, convert_messages};
use pi_core::ai::compat::stream_simple as compat_stream_simple;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, ChatTemplateKwargs, Context,
    DeferredToolsMode, ImageContent, MaxTokensField, Message, Model, ModelCompat,
    OpenRouterRouting, ProviderRequestOptions, RoleAssistant, RoleToolResult, RoleUser,
    SessionAffinityFormat, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ThinkingFormat, Tool, ToolCall, ToolResultMessage, Transport, Usage, UsageCost, UserContent,
    UserMessage, VercelGatewayRouting,
};
use pi_core::ai::utils::estimate::estimate_context_tokens;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures (the `makeTool`/`makeUserMessage`/... helpers)
// ---------------------------------------------------------------------------

/// The `makeTool` fixture: `parameters: Type.Object({ value: Type.String() })`.
fn make_tool(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: format!("The {name} tool"),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
        }),
        constrained_sampling: None,
    }
}

fn make_user_message(timestamp: i64) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text("Hello".to_string()),
        timestamp,
    })
}

fn tool_call(id: &str, name: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: Default::default(),
        ..Default::default()
    })
}

fn make_assistant_tool_call() -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: vec![tool_call("call_1", "base_tool")],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-opus-4-6".to_string(),
        usage: Usage {
            cost: UsageCost::default(),
            ..Default::default()
        },
        stop_reason: StopReason::ToolUse,
        timestamp: 2,
        ..Default::default()
    }
}

fn make_tool_result(added_tool_names: &[&str]) -> ToolResultMessage {
    ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: "call_1".to_string(),
        tool_name: "base_tool".to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: "done".to_string(),
            ..Default::default()
        })],
        added_tool_names: Some(
            added_tool_names
                .iter()
                .map(|name| name.to_string())
                .collect(),
        ),
        is_error: false,
        timestamp: 3,
        ..Default::default()
    }
}

fn make_context_with(tools: &[Tool], added_tool_names: &[&str]) -> Context {
    Context {
        system_prompt: None,
        messages: vec![
            make_user_message(1),
            Message::Assistant(Box::new(make_assistant_tool_call())),
            Message::ToolResult(Box::new(make_tool_result(added_tool_names))),
            make_user_message(4),
        ],
        tools: Some(tools.to_vec()),
    }
}

/// `makeContext` with its default `addedToolNames: ["late_tool"]` marker.
fn make_context(tools: &[Tool]) -> Context {
    make_context_with(tools, &["late_tool"])
}

/// The `makeKimiModel` fixture.
fn make_kimi_model(deferred_tools_mode: bool) -> Model {
    Model {
        id: "deferred-tools-model".to_string(),
        name: "Deferred Tools Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "moonshotai".to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        cost: pi_core::ai::types::ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        compat: deferred_tools_mode.then(|| ModelCompat {
            deferred_tools_mode: Some(DeferredToolsMode::Kimi),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The compat.ts `getModel` helper.
fn get_model(provider: &str, id: &str) -> Model {
    pi_core::ai::compat::get_model(provider, id)
        .unwrap_or_else(|| panic!("missing model {provider}/{id}"))
}

// ---------------------------------------------------------------------------
// Payload capture (the `capturePayload` helper)
// ---------------------------------------------------------------------------

/// A minimal terminal SSE body per api so the captured streams finish
/// cleanly; the payload is recorded before the response is parsed.
fn canned_sse(api: &str) -> String {
    match api {
        "anthropic-messages" => [
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
        .join("\n"),
        "openai-completions" => format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "prompt_tokens_details": {"cached_tokens": 0},
                    "completion_tokens_details": {"reasoning_tokens": 0},
                },
            })
        ),
        // openai-responses / openai-codex-responses.
        _ => "data: [DONE]\n\n".to_string(),
    }
}

/// Mock transport standing in for the TS `onPayload` captures: records the
/// outgoing request and answers with a minimal SSE stream.
struct CaptureFetch {
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl CaptureFetch {
    fn new(body: String) -> Self {
        CaptureFetch {
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

    fn payload(&self) -> Value {
        match &self.request().body {
            HttpBody::Json(value) => value.clone(),
            // The Codex SSE path compresses its request body (the Node
            // oracle behavior).
            HttpBody::Bytes(bytes) => {
                let decompressed = zstd::bulk::decompress(bytes, 16 * 1024 * 1024)
                    .expect("zstd-compressed request body");
                serde_json::from_slice(&decompressed).expect("valid JSON in compressed body")
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }
}

impl HttpFetch for CaptureFetch {
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

/// The `capturePayload` helper: runs the compat `streamSimple` with the given
/// API key and returns the JSON payload that left the process.
async fn capture_payload(model: &Model, context: &Context, api_key: &str) -> Value {
    let fetch = Arc::new(CaptureFetch::new(canned_sse(&model.api)));
    // The TS helper forces baseUrl to a black-hole address; the mock
    // transport intercepts the request either way.
    let model = Model {
        base_url: "http://127.0.0.1:9".to_string(),
        ..model.clone()
    };
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(api_key.to_string()),
                fetch: Some(fetch.clone()),
                ..Default::default()
            },
            // The TS helper forces baseUrl to a black-hole address so the
            // codex WebSocket attempt fails immediately; the Rust helper
            // pins the SSE transport instead.
            transport: Some(Transport::Sse),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = compat_stream_simple(&model, context, Some(&options))
        .result()
        .await;
    fetch.payload()
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

/// `expect(actual).toMatchObject(expected)`: objects match subsets, arrays
/// match pairwise by index (equal lengths), everything else is exact.
fn assert_matches_object(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Object(actual_map), Value::Object(expected_map)) => {
            for (key, expected_value) in expected_map {
                match actual_map.get(key) {
                    Some(actual_value) => assert_matches_object(actual_value, expected_value),
                    None => panic!(
                        "toMatchObject: missing key {key:?}\nexpected: {expected}\nactual:   {actual}"
                    ),
                }
            }
        }
        (Value::Array(actual_items), Value::Array(expected_items)) => {
            assert_eq!(
                actual_items.len(),
                expected_items.len(),
                "toMatchObject array length\nexpected: {expected}\nactual:   {actual}"
            );
            for (actual_item, expected_item) in actual_items.iter().zip(expected_items) {
                assert_matches_object(actual_item, expected_item);
            }
        }
        _ => assert_eq!(actual, expected, "toMatchObject leaf mismatch"),
    }
}

/// `findAnthropicToolResultContent`: the content of the payload message that
/// carries a `tool_result` block.
fn find_anthropic_tool_result_content(payload: &Value) -> &Vec<Value> {
    payload["messages"]
        .as_array()
        .expect("payload messages")
        .iter()
        .find(|message| {
            message["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|block| block["type"] == json!("tool_result"))
            })
        })
        .and_then(|message| message["content"].as_array())
        .expect("no tool result in payload")
}

/// `findAnthropicToolResult`: the first `tool_result` block itself.
fn find_anthropic_tool_result(payload: &Value) -> &Value {
    find_anthropic_tool_result_content(payload)
        .iter()
        .find(|block| block["type"] == json!("tool_result"))
        .expect("no tool result in payload")
}

/// Whether a tool-result content holds a `tool_reference` block (the TS
/// `Array.isArray(content) && content.some(...)` checks).
fn has_tool_reference(tool_result: &Value) -> bool {
    tool_result["content"].as_array().is_some_and(|content| {
        content
            .iter()
            .any(|block| block["type"] == json!("tool_reference"))
    })
}

fn anthropic_tool_names(payload: &Value) -> Vec<String> {
    payload["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// `openAIToolNames`: `tool.name ?? tool.function?.name ?? ""`.
fn openai_tool_names(payload: &Value) -> Vec<String> {
    payload["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    tool["name"]
                        .as_str()
                        .or_else(|| tool["function"]["name"].as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `payload.tools?.map((tool) => tool.function.name)` for Kimi payloads.
fn kimi_tool_names(payload: &Value) -> Vec<String> {
    payload["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `makeCodexToken`: a JWT-shaped token carrying the ChatGPT account id.
fn make_codex_token() -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_string(&json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "account" },
        }))
        .unwrap(),
    );
    format!("header.{payload}.signature")
}

// ---------------------------------------------------------------------------
// Anthropic Messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loads_an_anthropic_tool_at_its_tool_result_marker() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "fake-key",
    )
    .await;

    assert_matches_object(
        &payload["tools"],
        &json!([
            { "name": "base_tool" },
            { "name": "late_tool", "defer_loading": true },
        ]),
    );
    assert_eq!(
        find_anthropic_tool_result(&payload)["content"],
        json!([{ "type": "tool_reference", "tool_name": "late_tool" }])
    );
}

#[tokio::test]
async fn preserves_tool_output_as_sibling_content_after_emitting_references() {
    let mut context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let Message::Assistant(assistant) = &mut context.messages[1] else {
        panic!("expected assistant message");
    };
    assistant.content = vec![
        tool_call("call_1", "base_tool"),
        tool_call("call_2", "base_tool"),
    ];
    let Message::ToolResult(first_result) = &mut context.messages[2] else {
        panic!("expected tool result message");
    };
    first_result.content = vec![
        BlockContent::Text(TextContent {
            text: "work completed".to_string(),
            ..Default::default()
        }),
        BlockContent::Image(ImageContent {
            data: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            ..Default::default()
        }),
    ];
    context.messages.insert(
        3,
        Message::ToolResult(Box::new(ToolResultMessage {
            tool_call_id: "call_2".to_string(),
            content: vec![BlockContent::Text(TextContent {
                text: "second result".to_string(),
                ..Default::default()
            })],
            ..make_tool_result(&[])
        })),
    );

    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "fake-key",
    )
    .await;

    assert_matches_object(
        &Value::Array(find_anthropic_tool_result_content(&payload).clone()),
        &json!([
            {
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": [{ "type": "tool_reference", "tool_name": "late_tool" }],
            },
            { "type": "tool_result", "tool_use_id": "call_2", "content": "second result" },
            { "type": "text", "text": "work completed" },
            {
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "aW1hZ2U=" },
            },
        ]),
    );
}

#[tokio::test]
async fn loads_a_tool_introduced_by_openai_history_after_switching_to_anthropic() {
    let mut context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let Message::Assistant(assistant) = &mut context.messages[1] else {
        panic!("expected assistant message");
    };
    assistant.api = "openai-responses".to_string();
    assistant.provider = "openai".to_string();
    assistant.model = "gpt-5.4".to_string();

    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-8"),
        &context,
        "fake-key",
    )
    .await;

    assert_matches_object(
        &payload["tools"],
        &json!([
            { "name": "base_tool" },
            { "name": "late_tool", "defer_loading": true },
        ]),
    );
    assert_eq!(
        find_anthropic_tool_result(&payload)["content"],
        json!([{ "type": "tool_reference", "tool_name": "late_tool" }])
    );
}

#[tokio::test]
async fn does_not_resurrect_a_marked_tool_missing_from_context_tools() {
    let context = make_context(&[make_tool("base_tool")]);
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "fake-key",
    )
    .await;

    assert_eq!(anthropic_tool_names(&payload), vec!["base_tool"]);
    assert!(!has_tool_reference(find_anthropic_tool_result(&payload)));
}

#[tokio::test]
async fn keeps_a_tool_immediate_when_it_was_used_before_its_marker() {
    let mut context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let Message::Assistant(assistant) = &mut context.messages[1] else {
        panic!("expected assistant message");
    };
    assistant.content = vec![tool_call("call_1", "late_tool")];
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "fake-key",
    )
    .await;

    assert_eq!(
        anthropic_tool_names(&payload),
        vec!["base_tool", "late_tool"]
    );
    assert!(
        payload["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .all(|tool| tool.get("defer_loading").is_none())
    );
}

#[tokio::test]
async fn normalizes_oauth_names_before_checking_prior_tool_usage() {
    let mut context = make_context_with(&[make_tool("base_tool"), make_tool("read")], &["read"]);
    let Message::Assistant(assistant) = &mut context.messages[1] else {
        panic!("expected assistant message");
    };
    assistant.content = vec![tool_call("call_1", "Read")];
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "sk-ant-oat-fake",
    )
    .await;

    assert_eq!(anthropic_tool_names(&payload), vec!["base_tool", "Read"]);
    assert!(
        payload["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .all(|tool| tool.get("defer_loading").is_none())
    );
    assert!(!has_tool_reference(find_anthropic_tool_result(&payload)));
}

#[tokio::test]
async fn matches_oauth_canonicalized_markers_to_active_tools() {
    let context = make_context_with(&[make_tool("base_tool"), make_tool("read")], &["Read"]);
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "sk-ant-oat-fake",
    )
    .await;

    assert_matches_object(
        &payload["tools"],
        &json!([
            { "name": "base_tool" },
            { "name": "Read", "defer_loading": true },
        ]),
    );
    assert!(
        find_anthropic_tool_result(&payload)["content"]
            .as_array()
            .expect("reference content")
            .iter()
            .any(|block| block["type"] == json!("tool_reference")
                && block["tool_name"] == json!("Read"))
    );
}

#[tokio::test]
async fn deduplicates_active_tools_after_oauth_canonicalization() {
    let mut canonical = make_tool("Read");
    canonical.description = "Canonical definition".to_string();
    let context = Context {
        system_prompt: None,
        messages: vec![make_user_message(1)],
        tools: Some(vec![make_tool("read"), canonical]),
    };
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "sk-ant-oat-fake",
    )
    .await;

    assert_matches_object(
        &payload["tools"],
        &json!([{ "name": "Read", "description": "Canonical definition" }]),
    );
}

#[tokio::test]
async fn uses_the_normal_tool_list_when_anthropic_tool_references_are_unsupported() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let models = vec![
        get_model("anthropic", "claude-haiku-4-5"),
        Model {
            id: "claude-sonnet-4-20250514".to_string(),
            ..get_model("anthropic", "claude-opus-4-6")
        },
    ];

    for model in &models {
        let payload = capture_payload(model, &context, "fake-key").await;
        assert_eq!(
            anthropic_tool_names(&payload),
            vec!["base_tool", "late_tool"],
            "model {}",
            model.id
        );
        assert!(
            payload["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .all(|tool| tool.get("defer_loading").is_none()),
            "model {}",
            model.id
        );
    }
}

#[tokio::test]
async fn keeps_one_immediate_anthropic_tool_when_every_current_tool_is_marked() {
    let context = make_context(&[make_tool("late_tool")]);
    let payload = capture_payload(
        &get_model("anthropic", "claude-opus-4-6"),
        &context,
        "fake-key",
    )
    .await;

    assert_matches_object(&payload["tools"], &json!([{ "name": "late_tool" }]));
    assert!(payload["tools"][0].get("defer_loading").is_none());
    assert!(!has_tool_reference(find_anthropic_tool_result(&payload)));
}

#[tokio::test]
async fn supports_explicit_anthropic_compatibility_overrides() {
    let model = Model {
        provider: "anthropic-proxy".to_string(),
        compat: Some(ModelCompat {
            supports_tool_references: Some(true),
            ..Default::default()
        }),
        ..get_model("anthropic", "claude-opus-4-6")
    };
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(&model, &context, "fake-key").await;

    let late_tool = payload["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == json!("late_tool"))
        .expect("late_tool definition")
        .clone();
    assert_eq!(late_tool["defer_loading"], json!(true));
}

// ---------------------------------------------------------------------------
// Kimi (OpenAI Completions)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serializes_kimi_deferred_tools_as_system_tool_definitions() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(&make_kimi_model(true), &context, "fake-key").await;

    assert_eq!(kimi_tool_names(&payload), vec!["base_tool"]);
    let messages = payload["messages"].as_array().expect("messages");
    let tool_result_index = messages
        .iter()
        .position(|message| message["role"] == json!("tool"))
        .expect("tool result message");
    let system_tool_index = messages
        .iter()
        .position(|message| message.get("tools").is_some())
        .expect("system tool message");
    assert!(system_tool_index > tool_result_index);
    let system_tools = messages[system_tool_index]["tools"]
        .as_array()
        .expect("tools");
    let names = system_tools
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["late_tool"]);
}

/// The resolved compat literal the TS test passes to `convertMessages`.
fn kimi_resolved_compat() -> ResolvedCompletionsCompat {
    ResolvedCompletionsCompat {
        supports_store: false,
        supports_developer_role: false,
        supports_reasoning_effort: false,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: MaxTokensField::MaxTokens,
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: false,
        thinking_format: ThinkingFormat::Openai,
        open_router_routing: Some(OpenRouterRouting::default()),
        vercel_gateway_routing: Some(VercelGatewayRouting::default()),
        chat_template_kwargs: Some(ChatTemplateKwargs::default()),
        chat_template_args: Some(ChatTemplateKwargs::default()),
        zai_tool_stream: false,
        supports_thinking_token_budget: None,
        thinking_token_budget_field: None,
        supports_strict_mode: false,
        supports_openai_grammar_tools: false,
        cache_control_format: None,
        send_session_affinity_headers: false,
        deferred_tools_mode: Some(DeferredToolsMode::Kimi),
        session_affinity_format: SessionAffinityFormat::Openai,
        supports_long_cache_retention: false,
    }
}

#[test]
fn emits_kimi_deferred_schemas_after_all_tool_results_in_a_batch() {
    let mut context = make_context(&[
        make_tool("base_tool"),
        make_tool("late_tool"),
        make_tool("later_tool"),
    ]);
    let second_result = ToolResultMessage {
        tool_call_id: "call_2".to_string(),
        ..make_tool_result(&["later_tool"])
    };
    context
        .messages
        .insert(3, Message::ToolResult(Box::new(second_result)));

    let messages = convert_messages(
        &make_kimi_model(true),
        &context,
        &kimi_resolved_compat(),
        &BTreeMap::new(),
    )
    .expect("convertMessages");

    let roles = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec!["user", "assistant", "tool", "tool", "system", "user"]
    );
    let system_tools = messages[4]["tools"].as_array().expect("system tools");
    let names = system_tools
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["late_tool", "later_tool"]);
}

#[tokio::test]
async fn leaves_openai_completions_tools_unchanged_without_kimi_mode() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(&make_kimi_model(false), &context, "fake-key").await;

    assert_eq!(kimi_tool_names(&payload), vec!["base_tool", "late_tool"]);
    assert!(
        payload["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .all(|message| message.get("tools").is_none())
    );
}

// ---------------------------------------------------------------------------
// OpenAI Responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loads_an_openai_responses_tool_through_additional_tools() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(&get_model("openai", "gpt-5.4"), &context, "fake-key").await;
    let input = payload["input"].as_array().expect("input");
    let additional_tools = input
        .iter()
        .find(|item| item["type"] == json!("additional_tools"))
        .expect("additional_tools item")
        .clone();

    assert_eq!(openai_tool_names(&payload), vec!["base_tool"]);
    assert_eq!(additional_tools["role"], json!("developer"));
    let tools = additional_tools["tools"]
        .as_array()
        .expect("additional tools");
    assert_eq!(tools.len(), 1);
    assert_matches_object(
        &tools[0],
        &json!({ "type": "function", "name": "late_tool" }),
    );
    assert!(tools.iter().all(|tool| tool.get("defer_loading").is_none()));
    assert!(
        !input
            .iter()
            .any(|item| item["type"] == json!("tool_search_call"))
    );
    assert!(
        !input
            .iter()
            .any(|item| item["type"] == json!("tool_search_output"))
    );
}

#[tokio::test]
async fn preserves_an_additional_tools_marker_after_the_loaded_tool_is_used() {
    let mut context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let late_call = AssistantMessage {
        content: vec![tool_call("call_late|fc_late", "late_tool")],
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "gpt-5.4".to_string(),
        ..make_assistant_tool_call()
    };
    let late_result = ToolResultMessage {
        tool_call_id: "call_late|fc_late".to_string(),
        tool_name: "late_tool".to_string(),
        ..make_tool_result(&["late_tool"])
    };
    context
        .messages
        .insert(3, Message::Assistant(Box::new(late_call)));
    context
        .messages
        .insert(4, Message::ToolResult(Box::new(late_result)));

    let payload = capture_payload(&get_model("openai", "gpt-5.4"), &context, "fake-key").await;
    let input = payload["input"].as_array().expect("input");
    let additional_tool_indexes = input
        .iter()
        .enumerate()
        .filter(|(_, item)| item["type"] == json!("additional_tools"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let late_call_index = input
        .iter()
        .position(|item| {
            item["type"] == json!("function_call") && item["name"] == json!("late_tool")
        })
        .expect("late tool function call");

    assert_eq!(additional_tool_indexes.len(), 1);
    assert!(additional_tool_indexes[0] < late_call_index);
    assert_eq!(openai_tool_names(&payload), vec!["base_tool"]);
}

#[tokio::test]
async fn falls_back_to_client_tool_search_when_additional_tools_is_unsupported() {
    let model = Model {
        provider: "openai-proxy".to_string(),
        compat: Some(ModelCompat {
            supports_additional_tools: Some(false),
            supports_tool_search: Some(true),
            ..Default::default()
        }),
        ..get_model("openai", "gpt-5.4")
    };
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(&model, &context, "fake-key").await;
    let input = payload["input"].as_array().expect("input");
    let search_call = input
        .iter()
        .find(|item| item["type"] == json!("tool_search_call"))
        .expect("tool_search_call")
        .clone();
    let search_output = input
        .iter()
        .find(|item| item["type"] == json!("tool_search_output"))
        .expect("tool_search_output")
        .clone();

    assert_eq!(openai_tool_names(&payload), vec!["base_tool"]);
    assert_matches_object(
        &search_call,
        &json!({ "execution": "client", "status": "completed" }),
    );
    assert_eq!(search_output["call_id"], search_call["call_id"]);
    let tools = search_output["tools"].as_array().expect("searched tools");
    assert_eq!(tools.len(), 1);
    assert_matches_object(
        &tools[0],
        &json!({ "type": "function", "name": "late_tool", "defer_loading": true }),
    );
    assert!(
        !input
            .iter()
            .any(|item| item["type"] == json!("additional_tools"))
    );
}

/// The `it.each` body: the normal tool list for models without deferred
/// loading support.
async fn assert_normal_openai_tool_list(model: &Model) {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(model, &context, "fake-key").await;

    assert_eq!(openai_tool_names(&payload), vec!["base_tool", "late_tool"]);
    let input = payload["input"].as_array().expect("input");
    assert!(
        !input
            .iter()
            .any(|item| item["type"] == json!("tool_search_output"))
    );
}

#[tokio::test]
async fn uses_the_normal_tool_list_for_unsupported_openai_model_gpt_5_2() {
    assert_normal_openai_tool_list(&get_model("openai", "gpt-5.2")).await;
}

#[tokio::test]
async fn uses_the_normal_tool_list_for_unsupported_openai_model_gpt_5_4_nano() {
    assert_normal_openai_tool_list(&get_model("openai", "gpt-5.4-nano")).await;
}

#[tokio::test]
async fn uses_the_normal_tool_list_for_unsupported_openai_model_gpt_5_5_pro() {
    assert_normal_openai_tool_list(&get_model("openai", "gpt-5.5-pro")).await;
}

#[tokio::test]
async fn uses_the_normal_tool_list_when_openai_tool_search_is_explicitly_disabled() {
    let model = Model {
        provider: "openai-proxy".to_string(),
        compat: Some(ModelCompat {
            supports_tool_search: Some(false),
            ..Default::default()
        }),
        ..get_model("openai", "gpt-5.4")
    };
    assert_normal_openai_tool_list(&model).await;
}

#[tokio::test]
async fn selects_additional_tools_tool_search_or_top_level_tools_for_codex_models() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let token = make_codex_token();
    let additional_tools =
        capture_payload(&get_model("openai-codex", "gpt-5.6-sol"), &context, &token).await;
    let tool_search =
        capture_payload(&get_model("openai-codex", "gpt-5.4"), &context, &token).await;
    let top_level = capture_payload(
        &get_model("openai-codex", "gpt-5.3-codex-spark"),
        &context,
        &token,
    )
    .await;

    let additional_input = additional_tools["input"].as_array().expect("input");
    let tool_search_input = tool_search["input"].as_array().expect("input");
    let top_level_input = top_level["input"].as_array().expect("input");

    assert_eq!(openai_tool_names(&additional_tools), vec!["base_tool"]);
    assert!(
        additional_input
            .iter()
            .any(|item| item["type"] == json!("additional_tools"))
    );
    assert!(
        !additional_input
            .iter()
            .any(|item| item["type"] == json!("tool_search_output"))
    );

    assert_eq!(openai_tool_names(&tool_search), vec!["base_tool"]);
    assert!(
        tool_search_input
            .iter()
            .any(|item| item["type"] == json!("tool_search_output"))
    );

    assert_eq!(
        openai_tool_names(&top_level),
        vec!["base_tool", "late_tool"]
    );
    assert!(
        !top_level_input
            .iter()
            .any(|item| item["type"] == json!("additional_tools"))
    );
    assert!(
        !top_level_input
            .iter()
            .any(|item| item["type"] == json!("tool_search_output"))
    );
}

#[tokio::test]
async fn leaves_providers_without_deferred_loading_unchanged() {
    let context = make_context(&[make_tool("base_tool"), make_tool("late_tool")]);
    let payload = capture_payload(
        &get_model("groq", "llama-3.3-70b-versatile"),
        &context,
        "fake-key",
    )
    .await;

    assert_eq!(openai_tool_names(&payload), vec!["base_tool", "late_tool"]);
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

#[test]
fn counts_definitions_marked_after_the_latest_usage_checkpoint() {
    let assistant = AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: "done".to_string(),
            ..Default::default()
        })],
        usage: Usage {
            input: 50,
            output: 50,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 100,
            cost: UsageCost::default(),
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        ..make_assistant_tool_call()
    };
    let plain = estimate_context_tokens(&Context {
        system_prompt: None,
        messages: vec![
            Message::Assistant(Box::new(assistant.clone())),
            make_user_message(4),
        ],
        tools: Some(vec![]),
    });
    let mut late_tool = make_tool("late_tool");
    late_tool.description = "x".repeat(4000);
    let marked = estimate_context_tokens(&Context {
        system_prompt: None,
        messages: vec![
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(make_tool_result(&["late_tool"]))),
        ],
        tools: Some(vec![late_tool]),
    });

    assert!(marked.tokens > plain.tokens + 500);
    assert!(marked.trailing_tokens > plain.trailing_tokens + 500);
}
