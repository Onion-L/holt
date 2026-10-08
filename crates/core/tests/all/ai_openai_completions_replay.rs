//! Ports of the openai-completions message-replay and stream-decoding suites
//! (offline, via a scripted SSE transport standing in for the TS `openai`
//! SDK mock and local HTTP servers):
//!
//! - `openai-completions-reasoning-details.test.ts` (4 cases)
//! - `openai-completions-thinking-as-text.test.ts` (3 cases)
//! - `openai-completions-tool-result-images.test.ts` (2 cases)
//! - `openai-completions-response-model.test.ts` (3 cases)
//! - `openai-completions-tool-choice.test.ts` (the stream/replay cases only;
//!   pure payload/options-construction cases live elsewhere)
//! - `openai-completions-retry.test.ts` (the 2 stream-level cases)

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_core::ai::api::openai_completions::{
    OpenAICompletionsOptions, ResolvedCompletionsCompat, convert_messages, stream, stream_simple,
};
use pi_core::ai::providers::builtin::get_builtin_model;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent, CacheControlFormat,
    ChatTemplateKwargs, Context, ImageContent, MaxTokensField, Message, Model, ModelCompat,
    ModelCost, ModelInput, OpenRouterRouting, ProviderRequestOptions, RoleToolResult, RoleUser,
    SessionAffinityFormat, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ThinkingContent, ThinkingFormat, ThinkingLevel, Tool, ToolCall, ToolCallArguments,
    ToolResultMessage, UserContent, UserMessage, VercelGatewayRouting,
};
use pi_core::ai::utils::event_stream::collect_events;
use pi_core::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// One scripted transport answer: a canned SSE chunk list, a raw SSE body, or
/// a transport failure.
#[derive(Clone)]
enum ScriptedResponse {
    Sse(Vec<Value>),
    RawSse(String),
    Error(&'static str),
}

/// Mock transport mirroring the vitest `vi.mock("openai")` fakes: it records
/// every outgoing request and answers from a shift-once script (the final
/// entry repeats, like `mockState.chunks` reused across `create` calls).
struct ScriptedFetch {
    responses: Mutex<Vec<ScriptedResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedFetch {
    fn new(responses: Vec<ScriptedResponse>) -> Arc<Self> {
        Arc::new(ScriptedFetch {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request(&self, index: usize) -> HttpRequest {
        self.requests.lock().unwrap()[index].clone()
    }

    fn body(&self, index: usize) -> Value {
        match &self.request(index).body {
            HttpBody::Json(value) => value.clone(),
            _ => panic!("expected JSON body"),
        }
    }
}

fn sse_response(body: String) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
    }
}

impl HttpFetch for ScriptedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        self.requests.lock().unwrap().push(request);
        let mut responses = self.responses.lock().unwrap();
        let response = if responses.len() > 1 {
            responses.remove(0)
        } else {
            responses
                .first()
                .cloned()
                .unwrap_or(ScriptedResponse::Sse(Vec::new()))
        };
        drop(responses);
        Box::pin(async move {
            match response {
                ScriptedResponse::Sse(chunks) => {
                    let body = chunks
                        .iter()
                        .map(|chunk| format!("data: {chunk}\n\n"))
                        .collect::<String>();
                    Ok(sse_response(body))
                }
                ScriptedResponse::RawSse(body) => Ok(sse_response(body)),
                ScriptedResponse::Error(message) => {
                    Err(HttpFetchError::Request(message.to_string()))
                }
            }
        })
    }
}

fn request_options(fetch: Arc<ScriptedFetch>) -> ProviderRequestOptions {
    ProviderRequestOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch),
        ..Default::default()
    }
}

fn completions_options(fetch: Arc<ScriptedFetch>) -> OpenAICompletionsOptions {
    OpenAICompletionsOptions {
        base: StreamOptions {
            base: request_options(fetch),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn simple_options(
    fetch: Arc<ScriptedFetch>,
    reasoning: Option<ThinkingLevel>,
) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: request_options(fetch),
            ..Default::default()
        },
        reasoning,
        ..Default::default()
    }
}

/// The TS mock's default chunk set, used by every `streamSimple` call that
/// only captures the outgoing payload.
fn default_chunk() -> Value {
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

/// Captures the JSON payload that left the process through `streamSimple`
/// (the `onPayload` capture helper of the TS suites), preserving any scoped
/// env carried by the options.
async fn capture_simple_payload(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> Value {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![default_chunk()])]);
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
    fetch.body(0)
}

fn user_message(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp,
    })
}

fn tool_arguments(value: Value) -> ToolCallArguments {
    value.as_object().cloned().expect("object arguments")
}

fn find_thinking(message: &AssistantMessage) -> Option<&ThinkingContent> {
    message.content.iter().find_map(|block| match block {
        AssistantContent::Thinking(thinking) => Some(thinking),
        _ => None,
    })
}

fn find_tool_call(message: &AssistantMessage) -> Option<&ToolCall> {
    message.content.iter().find_map(|block| match block {
        AssistantContent::ToolCall(call) => Some(call),
        _ => None,
    })
}

fn text_block(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.to_string(),
        ..Default::default()
    })
}

/// The TS `getModel(provider, id)` with `compat` stripped and the API forced
/// to `openai-completions` (`const { compat: _compat, ...baseModel } = ...`).
fn openai_builtin(provider: &str, id: &str) -> Model {
    let mut model =
        get_builtin_model(provider, id).unwrap_or_else(|| panic!("missing model {provider}/{id}"));
    model.api = "openai-completions".to_string();
    model.compat = None;
    model
}

fn empty_usage() -> pi_core::ai::types::Usage {
    pi_core::ai::types::Usage::default()
}

// ---------------------------------------------------------------------------
// openai-completions-reasoning-details.test.ts
// ---------------------------------------------------------------------------

fn reasoning_details_model() -> Model {
    Model {
        id: "google/gemini-test".to_string(),
        name: "Gemini Test".to_string(),
        api: "openai-completions".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_tokens: 4_096,
        ..Default::default()
    }
}

fn reasoning_detail() -> Value {
    json!({
        "type": "reasoning.encrypted",
        "id": "call_1",
        "data": "encrypted-signature",
    })
}

fn signed_reasoning_text_detail() -> Value {
    json!({
        "type": "reasoning.text",
        "text": "I should call the read tool.",
        "signature": "sha256:signed-text",
        "id": "reasoning-text-1",
        "format": "anthropic-claude-v1",
        "index": 0,
    })
}

fn reasoning_summary_detail() -> Value {
    json!({
        "type": "reasoning.summary",
        "summary": "Decided to inspect the requested file.",
        "id": "reasoning-summary-1",
        "format": "anthropic-claude-v1",
        "index": 1,
    })
}

fn read_tool() -> Tool {
    Tool {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        }),
        constrained_sampling: None,
    }
}

fn reasoning_chunk(delta: Value, finish_reason: Value) -> Value {
    json!({
        "id": "chatcmpl-test",
        "model": "google/gemini-test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
}

fn tool_call_chunk() -> Value {
    reasoning_chunk(
        json!({
            "tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": "read", "arguments": r#"{"path":"README.md"}"#},
            }],
        }),
        json!(null),
    )
}

async fn run_reasoning_stream(
    model: &Model,
    fetch: Arc<ScriptedFetch>,
    messages: Vec<Message>,
) -> AssistantMessage {
    let context = Context {
        messages,
        tools: Some(vec![read_tool()]),
        ..Default::default()
    };
    stream(model, &context, Some(&completions_options(fetch)))
        .result()
        .await
}

fn assistant_payload(payload: &Value) -> Value {
    payload["messages"]
        .as_array()
        .expect("messages in payload")
        .iter()
        .find(|message| message["role"] == json!("assistant"))
        .cloned()
        .expect("assistant message in payload")
}

#[tokio::test]
async fn preserves_reasoning_details_in_the_thinking_signature() {
    let fetch = ScriptedFetch::new(vec![
        ScriptedResponse::Sse(vec![
            reasoning_chunk(
                json!({"reasoning_details": [reasoning_detail()]}),
                json!(null),
            ),
            tool_call_chunk(),
            reasoning_chunk(json!({}), json!("tool_calls")),
        ]),
        ScriptedResponse::Sse(vec![
            reasoning_chunk(json!({"content": "ok"}), json!(null)),
            reasoning_chunk(json!({}), json!("stop")),
        ]),
    ]);

    let assistant = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), vec![]).await;
    let thinking = find_thinking(&assistant).expect("thinking block");
    assert_eq!(thinking.thinking, "");
    let expected_signature = json!([reasoning_detail()]).to_string();
    assert_eq!(
        thinking.thinking_signature.as_deref(),
        Some(expected_signature.as_str())
    );
    let tool_call = find_tool_call(&assistant).expect("tool call block");
    assert_eq!(tool_call.id, "call_1");
    assert_eq!(tool_call.name, "read");
    assert_eq!(
        tool_call.arguments,
        tool_arguments(json!({"path": "README.md"}))
    );

    let replay = vec![Message::Assistant(Box::new(assistant))];
    let _ = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), replay).await;

    assert_eq!(
        assistant_payload(&fetch.body(1))["reasoning_details"],
        json!([reasoning_detail()])
    );
}

#[tokio::test]
async fn falls_back_to_encrypted_tool_call_signatures_for_older_stored_assistant_messages() {
    let fetch = ScriptedFetch::new(vec![
        ScriptedResponse::Sse(vec![
            reasoning_chunk(
                json!({"reasoning_details": [reasoning_detail()]}),
                json!(null),
            ),
            tool_call_chunk(),
            reasoning_chunk(json!({}), json!("tool_calls")),
        ]),
        ScriptedResponse::Sse(vec![
            reasoning_chunk(json!({"content": "ok"}), json!(null)),
            reasoning_chunk(json!({}), json!("stop")),
        ]),
    ]);

    let mut assistant =
        run_reasoning_stream(&reasoning_details_model(), fetch.clone(), vec![]).await;
    // Older stored assistant messages carry the encrypted detail on the tool
    // call's thoughtSignature instead of a thinking block.
    assistant
        .content
        .retain(|block| !matches!(block, AssistantContent::Thinking(_)));
    let tool_call = find_tool_call(&assistant).expect("tool call block");
    assert!(tool_call.thought_signature.is_none());
    let legacy_signature = json!(reasoning_detail()).to_string();
    for block in assistant.content.iter_mut() {
        if let AssistantContent::ToolCall(call) = block {
            call.thought_signature = Some(legacy_signature.clone());
        }
    }

    let replay = vec![Message::Assistant(Box::new(assistant))];
    let _ = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), replay).await;

    assert_eq!(
        assistant_payload(&fetch.body(1))["reasoning_details"],
        json!([reasoning_detail()])
    );
}

#[tokio::test]
async fn preserves_signed_text_and_summary_reasoning_details_in_their_original_sequence() {
    let expected = json!([
        signed_reasoning_text_detail(),
        reasoning_detail(),
        reasoning_summary_detail(),
    ]);
    let fetch = ScriptedFetch::new(vec![
        ScriptedResponse::Sse(vec![
            reasoning_chunk(
                json!({
                    "reasoning": "I should call the read tool.",
                    "reasoning_details": [signed_reasoning_text_detail()],
                }),
                json!(null),
            ),
            reasoning_chunk(
                json!({"reasoning_details": [reasoning_detail(), reasoning_summary_detail()]}),
                json!(null),
            ),
            tool_call_chunk(),
            reasoning_chunk(json!({}), json!("tool_calls")),
        ]),
        ScriptedResponse::Sse(vec![
            reasoning_chunk(json!({"content": "ok"}), json!(null)),
            reasoning_chunk(json!({}), json!("stop")),
        ]),
    ]);

    let assistant = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), vec![]).await;
    let thinking = find_thinking(&assistant).expect("thinking block");
    assert_eq!(thinking.thinking, "I should call the read tool.");
    assert_eq!(
        thinking.thinking_signature.as_deref(),
        Some(expected.to_string().as_str())
    );

    let replay = vec![Message::Assistant(Box::new(assistant))];
    let _ = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), replay).await;

    let replayed = assistant_payload(&fetch.body(1));
    assert_eq!(replayed["reasoning_details"], expected);
    assert!(replayed.get("reasoning").is_none());
}

#[tokio::test]
async fn merges_consecutive_text_and_summary_reasoning_details_deltas_before_replay() {
    let text_delta = json!({"type": "reasoning.text", "text": "The", "index": 0});
    let text_delta_with_signature = json!({
        "type": "reasoning.text",
        "text": " user wants the time.",
        "signature": "sha256:text-signature",
        "format": "openai-responses-v1",
        "index": 0,
    });
    let summary_delta = json!({"type": "reasoning.summary", "summary": "Looked", "index": 0});
    let summary_delta_with_format = json!({
        "type": "reasoning.summary",
        "summary": " up time.",
        "format": "openai-responses-v1",
        "index": 0,
    });
    let later_summary_delta = json!({
        "type": "reasoning.summary",
        "summary": "After encrypted block.",
        "format": "openai-responses-v1",
        "index": 0,
    });
    let expected = json!([
        {
            "type": "reasoning.text",
            "text": "The user wants the time.",
            "index": 0,
            "signature": "sha256:text-signature",
            "format": "openai-responses-v1",
        },
        {
            "type": "reasoning.summary",
            "summary": "Looked up time.",
            "index": 0,
            "format": "openai-responses-v1",
        },
        reasoning_detail(),
        later_summary_delta,
    ]);

    let fetch = ScriptedFetch::new(vec![
        ScriptedResponse::Sse(vec![
            reasoning_chunk(json!({"reasoning_details": [text_delta]}), json!(null)),
            reasoning_chunk(
                json!({"reasoning_details": [text_delta_with_signature]}),
                json!(null),
            ),
            reasoning_chunk(json!({"reasoning_details": [summary_delta]}), json!(null)),
            reasoning_chunk(
                json!({"reasoning_details": [summary_delta_with_format]}),
                json!(null),
            ),
            reasoning_chunk(
                json!({"reasoning_details": [reasoning_detail()]}),
                json!(null),
            ),
            reasoning_chunk(
                json!({"reasoning_details": [later_summary_delta]}),
                json!(null),
            ),
            tool_call_chunk(),
            reasoning_chunk(json!({}), json!("tool_calls")),
        ]),
        ScriptedResponse::Sse(vec![
            reasoning_chunk(json!({"content": "ok"}), json!(null)),
            reasoning_chunk(json!({}), json!("stop")),
        ]),
    ]);

    let assistant = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), vec![]).await;
    let thinking = find_thinking(&assistant).expect("thinking block");
    assert_eq!(thinking.thinking, "");
    assert_eq!(
        thinking.thinking_signature.as_deref(),
        Some(expected.to_string().as_str())
    );

    let replay = vec![Message::Assistant(Box::new(assistant))];
    let _ = run_reasoning_stream(&reasoning_details_model(), fetch.clone(), replay).await;

    assert_eq!(
        assistant_payload(&fetch.body(1))["reasoning_details"],
        expected
    );
}

// ---------------------------------------------------------------------------
// openai-completions-thinking-as-text.test.ts
// ---------------------------------------------------------------------------

/// The TS `compat` literal (model-level `ModelCompat` mirror).
fn thinking_as_text_model_compat() -> ModelCompat {
    ModelCompat {
        supports_store: Some(true),
        supports_developer_role: Some(true),
        supports_reasoning_effort: Some(true),
        supports_usage_in_streaming: Some(true),
        supports_finish_reason: Some(true),
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(true),
        requires_reasoning_content_on_assistant_messages: Some(false),
        thinking_format: Some(ThinkingFormat::Openai),
        chat_template_kwargs: Some(ChatTemplateKwargs::new()),
        chat_template_args: Some(ChatTemplateKwargs::new()),
        open_router_routing: Some(OpenRouterRouting::default()),
        vercel_gateway_routing: Some(VercelGatewayRouting::default()),
        zai_tool_stream: Some(false),
        thinking_token_budget_field: None,
        supports_thinking_token_budget: Some(false),
        supports_strict_mode: Some(true),
        supports_open_ai_grammar_tools: Some(false),
        cache_control_format: None,
        send_session_affinity_headers: Some(false),
        deferred_tools_mode: None,
        session_affinity_format: Some(SessionAffinityFormat::Openai),
        supports_long_cache_retention: Some(true),
        ..Default::default()
    }
}

fn thinking_as_text_resolved_compat() -> ResolvedCompletionsCompat {
    ResolvedCompletionsCompat {
        supports_store: true,
        supports_developer_role: true,
        supports_reasoning_effort: true,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: true,
        requires_reasoning_content_on_assistant_messages: false,
        thinking_format: ThinkingFormat::Openai,
        open_router_routing: Some(OpenRouterRouting::default()),
        vercel_gateway_routing: Some(VercelGatewayRouting::default()),
        chat_template_kwargs: Some(ChatTemplateKwargs::new()),
        chat_template_args: Some(ChatTemplateKwargs::new()),
        zai_tool_stream: false,
        supports_thinking_token_budget: Some(false),
        thinking_token_budget_field: None,
        supports_strict_mode: true,
        supports_openai_grammar_tools: false,
        cache_control_format: None,
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: SessionAffinityFormat::Openai,
        supports_long_cache_retention: true,
    }
}

fn thinking_as_text_model() -> Model {
    Model {
        id: "repro-model".to_string(),
        name: "Repro Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "repro-provider".to_string(),
        base_url: "http://127.0.0.1:1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        compat: Some(thinking_as_text_model_compat()),
        ..Default::default()
    }
}

fn thinking_as_text_assistant(content: Vec<AssistantContent>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "openai-completions".to_string(),
        provider: "repro-provider".to_string(),
        model: "repro-model".to_string(),
        usage: empty_usage(),
        stop_reason: StopReason::Stop,
        timestamp: 2,
        ..Default::default()
    }
}

fn thinking_as_text_context(assistant: AssistantMessage) -> Context {
    Context {
        messages: vec![
            user_message("hello", 1),
            Message::Assistant(Box::new(assistant)),
            user_message("continue", 3),
        ],
        ..Default::default()
    }
}

fn empty_grammar_properties() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[test]
fn serializes_same_model_thinking_plus_text_replay_as_assistant_text_parts() {
    let messages = convert_messages(
        &thinking_as_text_model(),
        &thinking_as_text_context(thinking_as_text_assistant(vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "internal reasoning".to_string(),
                ..Default::default()
            }),
            text_block("visible answer"),
        ])),
        &thinking_as_text_resolved_compat(),
        &empty_grammar_properties(),
    )
    .expect("convertMessages");

    assert_eq!(
        messages[1],
        json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "internal reasoning"},
                {"type": "text", "text": "visible answer"},
            ],
        })
    );
}

#[test]
fn serializes_same_model_thinking_only_replay_as_assistant_text_parts() {
    let messages = convert_messages(
        &thinking_as_text_model(),
        &thinking_as_text_context(thinking_as_text_assistant(vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "internal reasoning".to_string(),
                ..Default::default()
            }),
        ])),
        &thinking_as_text_resolved_compat(),
        &empty_grammar_properties(),
    )
    .expect("convertMessages");

    assert_eq!(
        messages[1],
        json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "internal reasoning"}],
        })
    );
}

#[tokio::test]
async fn reaches_the_endpoint_when_replay_contains_both_thinking_and_text() {
    let body = [
        json!({
            "id": "chatcmpl-repro",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "repro-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}],
        }),
        json!({
            "id": "chatcmpl-repro",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "repro-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        }),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect::<String>();
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::RawSse(body)]);

    let context = thinking_as_text_context(thinking_as_text_assistant(vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "internal reasoning".to_string(),
            ..Default::default()
        }),
        text_block("visible answer"),
    ]));
    let event_stream = stream(
        &thinking_as_text_model(),
        &context,
        Some(&completions_options(fetch.clone())),
    );
    let events = collect_events(&event_stream).await;
    let _ = event_stream.result().await;

    assert_eq!(fetch.request_count(), 1);
    assert_eq!(
        fetch.body(0)["messages"][1],
        json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "internal reasoning"},
                {"type": "text", "text": "visible answer"},
            ],
        })
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
}

// ---------------------------------------------------------------------------
// openai-completions-tool-result-images.test.ts
// ---------------------------------------------------------------------------

fn tool_result_images_resolved_compat() -> ResolvedCompletionsCompat {
    ResolvedCompletionsCompat {
        supports_store: true,
        supports_developer_role: true,
        supports_reasoning_effort: true,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: false,
        thinking_format: ThinkingFormat::Openai,
        open_router_routing: Some(OpenRouterRouting::default()),
        vercel_gateway_routing: Some(VercelGatewayRouting::default()),
        chat_template_kwargs: Some(ChatTemplateKwargs::new()),
        chat_template_args: Some(ChatTemplateKwargs::new()),
        zai_tool_stream: false,
        supports_thinking_token_budget: Some(false),
        thinking_token_budget_field: None,
        supports_strict_mode: true,
        supports_openai_grammar_tools: false,
        cache_control_format: Some(CacheControlFormat::Anthropic),
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: SessionAffinityFormat::Openai,
        supports_long_cache_retention: true,
    }
}

fn image_tool_result_model() -> Model {
    let mut model = openai_builtin("openai", "gpt-4o-mini");
    model.input = vec![ModelInput::Text, ModelInput::Image];
    model
}

fn image_tool_result(tool_call_id: &str, timestamp: i64) -> ToolResultMessage {
    ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: tool_call_id.to_string(),
        tool_name: "read".to_string(),
        content: vec![
            BlockContent::Text(TextContent {
                text: "Read image file [image/png]".to_string(),
                ..Default::default()
            }),
            BlockContent::Image(ImageContent {
                data: "ZmFrZQ==".to_string(),
                mime_type: "image/png".to_string(),
                ..Default::default()
            }),
        ],
        is_error: false,
        timestamp,
        ..Default::default()
    }
}

#[test]
fn batches_tool_result_images_after_consecutive_tool_results() {
    let now = 1_000_000i64;
    let assistant = AssistantMessage {
        content: vec![
            AssistantContent::ToolCall(ToolCall {
                id: "tool-1".to_string(),
                name: "read".to_string(),
                arguments: tool_arguments(json!({"path": "img-1.png"})),
                ..Default::default()
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "tool-2".to_string(),
                name: "read".to_string(),
                arguments: tool_arguments(json!({"path": "img-2.png"})),
                ..Default::default()
            }),
        ],
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        usage: empty_usage(),
        stop_reason: StopReason::ToolUse,
        timestamp: now,
        ..Default::default()
    };
    let context = Context {
        messages: vec![
            user_message("Read the images", now - 2),
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(image_tool_result("tool-1", now + 1))),
            Message::ToolResult(Box::new(image_tool_result("tool-2", now + 2))),
        ],
        ..Default::default()
    };

    let messages = convert_messages(
        &image_tool_result_model(),
        &context,
        &tool_result_images_resolved_compat(),
        &empty_grammar_properties(),
    )
    .expect("convertMessages");

    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().expect("role"))
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "tool", "user"]);

    let image_message = messages.last().expect("image message");
    assert_eq!(image_message["role"], json!("user"));
    let content = image_message["content"].as_array().expect("array content");
    let image_parts: Vec<&Value> = content
        .iter()
        .filter(|part| part["type"] == json!("image_url"))
        .collect();
    assert_eq!(image_parts.len(), 2);
}

#[test]
fn uses_no_tool_output_placeholder_for_empty_tool_results_without_images() {
    let now = 1_000_000i64;
    let assistant = AssistantMessage {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: "tool-1".to_string(),
            name: "bash".to_string(),
            arguments: tool_arguments(json!({"command": "true"})),
            ..Default::default()
        })],
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        usage: empty_usage(),
        stop_reason: StopReason::ToolUse,
        timestamp: now,
        ..Default::default()
    };
    let empty_result = ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: "tool-1".to_string(),
        tool_name: "bash".to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: String::new(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: now + 1,
        ..Default::default()
    };
    let context = Context {
        messages: vec![
            user_message("Run the command", now - 1),
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(empty_result)),
        ],
        ..Default::default()
    };

    let messages = convert_messages(
        &image_tool_result_model(),
        &context,
        &tool_result_images_resolved_compat(),
        &empty_grammar_properties(),
    )
    .expect("convertMessages");

    let tool_message = messages
        .iter()
        .find(|message| message["role"] == json!("tool"))
        .expect("tool message");
    let content = tool_message["content"].as_str().expect("string content");
    assert_eq!(content, "(no tool output)");
    assert!(!content.contains("see attached image"));
}

// ---------------------------------------------------------------------------
// openai-completions-response-model.test.ts
// ---------------------------------------------------------------------------

fn openrouter_auto() -> Model {
    Model {
        id: "openrouter/auto".to_string(),
        name: "OpenRouter Auto".to_string(),
        api: "openai-completions".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 8_192,
        ..Default::default()
    }
}

#[tokio::test]
async fn surfaces_routed_chunk_model_on_response_model_without_changing_model() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-1",
            "model": "anthropic/claude-opus-4.8",
            "choices": [{"index": 0, "delta": {"content": "hi"}}],
        }),
        json!({
            "id": "chatcmpl-1",
            "model": "anthropic/claude-opus-4.8",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ])]);

    let context = Context {
        messages: vec![user_message("hi", 0)],
        ..Default::default()
    };
    let message = stream(
        &openrouter_auto(),
        &context,
        Some(&completions_options(fetch)),
    )
    .result()
    .await;

    assert_eq!(message.model, "openrouter/auto");
    assert_eq!(
        message.response_model.as_deref(),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(message.provider, "openrouter");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn leaves_response_model_undefined_when_chunks_echo_the_requested_id() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-2",
            "model": "openrouter/auto",
            "choices": [{"index": 0, "delta": {"content": "hi"}}],
        }),
        json!({
            "id": "chatcmpl-2",
            "model": "openrouter/auto",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ])]);

    let context = Context {
        messages: vec![user_message("hi", 0)],
        ..Default::default()
    };
    let message = stream(
        &openrouter_auto(),
        &context,
        Some(&completions_options(fetch)),
    )
    .result()
    .await;

    assert_eq!(message.model, "openrouter/auto");
    assert!(message.response_model.is_none());
}

#[tokio::test]
async fn ignores_empty_or_missing_chunk_model() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-3",
            "choices": [{"index": 0, "delta": {"content": "hi"}}],
        }),
        json!({
            "id": "chatcmpl-3",
            "model": "",
            "choices": [{"index": 0, "delta": {"content": "!"}}],
        }),
        json!({
            "id": "chatcmpl-3",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ])]);

    let context = Context {
        messages: vec![user_message("hi", 0)],
        ..Default::default()
    };
    let message = stream(
        &openrouter_auto(),
        &context,
        Some(&completions_options(fetch)),
    )
    .result()
    .await;

    assert_eq!(message.model, "openrouter/auto");
    assert!(message.response_model.is_none());
}

// ---------------------------------------------------------------------------
// openai-completions-tool-choice.test.ts (stream/replay cases)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ignores_null_stream_chunks_from_openai_compatible_providers() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!(null),
        json!({
            "id": "chatcmpl-test",
            "choices": [{"delta": {"content": "OK"}, "finish_reason": null}],
        }),
        json!({
            "id": "chatcmpl-test",
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 3,
                "completion_tokens": 1,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Reply with exactly OK", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(response.stop_reason, StopReason::Stop);
    assert!(response.error_message.is_none());
    assert_eq!(response.response_id.as_deref(), Some("chatcmpl-test"));
    assert_eq!(response.usage.total_tokens, 4);
    assert_eq!(response.content, vec![text_block("OK")]);
}

#[tokio::test]
async fn errors_when_a_stream_ends_after_only_null_finish_reason_chunks() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-truncated",
            "choices": [{"delta": {"content": "partial answer"}, "finish_reason": null}],
        }),
        json!({
            "id": "chatcmpl-truncated",
            "choices": [{"delta": {"content": "partial answer"}, "finish_reason": null}],
        }),
    ])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Reply with a longer sentence", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(
        response.error_message.as_deref(),
        Some("Stream ended without finish_reason")
    );
}

#[tokio::test]
async fn accepts_streams_without_finish_reason_when_compat_disables_it() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![json!({
        "id": "chatcmpl-no-finish-reason",
        "choices": [{"delta": {"content": "complete answer"}, "finish_reason": null}],
    })])]);

    let mut model = openai_builtin("openai", "gpt-4o-mini");
    model.compat = Some(ModelCompat {
        supports_finish_reason: Some(false),
        ..Default::default()
    });
    let context = Context {
        messages: vec![user_message("Reply with a complete answer", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(response.stop_reason, StopReason::Stop);
    assert!(response.error_message.is_none());
    assert_eq!(response.content, vec![text_block("complete answer")]);
}

#[tokio::test]
async fn ignores_empty_custom_objects_on_function_tool_call_deltas() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![json!({
        "id": "chatcmpl-empty-custom",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": r#"{"path":"README.md"}"#},
                    "custom": {},
                }],
            },
            "finish_reason": "tool_calls",
        }],
    })])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Read README.md", 0)],
        tools: Some(vec![read_tool()]),
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(
        response.content,
        vec![AssistantContent::ToolCall(ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: tool_arguments(json!({"path": "README.md"})),
            ..Default::default()
        })]
    );
}

#[tokio::test]
async fn coalesces_tool_call_deltas_by_stable_index_when_provider_mutates_ids_mid_stream() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-kimi-bad-stream",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "functions.read:0",
                        "type": "function",
                        "function": {"name": "read", "arguments": ""},
                    }],
                },
                "finish_reason": null,
            }],
        }),
        json!({
            "id": "chatcmpl-kimi-bad-stream",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "chatcmpl-tool-a",
                        "type": "function",
                        "function": {"name": null, "arguments": r#"{"path":"README"#},
                    }],
                },
                "finish_reason": null,
            }],
        }),
        json!({
            "id": "chatcmpl-kimi-bad-stream",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "chatcmpl-tool-b",
                        "type": "function",
                        "function": {"name": null, "arguments": r#".md"}"#},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Read README.md", 0)],
        tools: Some(vec![read_tool()]),
        ..Default::default()
    };
    let event_stream = stream_simple(&model, &context, Some(&simple_options(fetch, None)));

    let mut tool_call_content_indexes: Vec<usize> = Vec::new();
    while let Some(event) = event_stream.next().await {
        match event {
            AssistantMessageEvent::ToolcallStart { content_index, .. }
            | AssistantMessageEvent::ToolcallDelta { content_index, .. }
            | AssistantMessageEvent::ToolcallEnd { content_index, .. } => {
                tool_call_content_indexes.push(content_index);
            }
            _ => {}
        }
    }
    let response = event_stream.result().await;

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(tool_call_content_indexes, vec![0, 0, 0, 0, 0]);
    assert_eq!(response.content.len(), 1);
    let tool_call = find_tool_call(&response).expect("tool call block");
    assert_eq!(tool_call.id, "functions.read:0");
    assert_eq!(tool_call.name, "read");
    assert_eq!(
        tool_call.arguments,
        tool_arguments(json!({"path": "README.md"}))
    );
    let serialized = serde_json::to_value(tool_call).expect("serialized tool call");
    assert!(serialized.get("streamIndex").is_none());
    assert!(serialized.get("partialArgs").is_none());
}

#[tokio::test]
async fn accumulates_mixed_content_reasoning_and_parallel_tool_call_deltas_independently() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-mixed-deltas",
            "choices": [{
                "delta": {
                    "content": "answer 1",
                    "reasoning_content": "think 1",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "tc_read_initial",
                            "type": "function",
                            "function": {"name": "read", "arguments": r#"{"path":"README"#},
                        },
                        {
                            "index": 1,
                            "id": "tc_grep_initial",
                            "type": "function",
                            "function": {"name": "grep", "arguments": r#"{"pattern":"TODO"#},
                        },
                        {
                            "id": "tc_list_no_index",
                            "type": "function",
                            "function": {"name": "list", "arguments": r#"{"path":"packages"#},
                        },
                        {
                            "id": "tc_write_no_index",
                            "type": "function",
                            "function": {"name": "write", "arguments": r#"{"path":"out"#},
                        },
                    ],
                },
                "finish_reason": null,
            }],
        }),
        json!({
            "id": "chatcmpl-mixed-deltas",
            "choices": [{
                "delta": {
                    "content": " answer 2",
                    "tool_calls": [
                        {
                            "index": 1,
                            "id": "tc_grep_changed",
                            "type": "function",
                            "function": {"arguments": r#"","path":"src"#},
                        },
                        {
                            "id": "tc_write_no_index",
                            "type": "function",
                            "function": {"arguments": r#".txt","content":"ok"}"#},
                        },
                        {
                            "id": "tc_list_no_index",
                            "type": "function",
                            "function": {"arguments": r#"/ai"}"#},
                        },
                    ],
                },
                "finish_reason": null,
            }],
        }),
        json!({
            "id": "chatcmpl-mixed-deltas",
            "choices": [{
                "delta": {
                    "content": "\n",
                    "reasoning_content": " think 2",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "tc_read_changed",
                            "type": "function",
                            "function": {"arguments": r#".md"}"#},
                        },
                        {
                            "index": 1,
                            "type": "function",
                            "function": {"arguments": r#""}"#},
                        },
                    ],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 2},
            },
        }),
    ])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let tool_fixtures: [(&str, &str, Value, Vec<&str>); 4] = [
        (
            "read",
            "Read a file",
            json!({"path": {"type": "string"}}),
            vec!["path"],
        ),
        (
            "grep",
            "Search a file",
            json!({"pattern": {"type": "string"}, "path": {"type": "string"}}),
            vec!["pattern", "path"],
        ),
        (
            "list",
            "List a directory",
            json!({"path": {"type": "string"}}),
            vec!["path"],
        ),
        (
            "write",
            "Write a file",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            vec!["path", "content"],
        ),
    ];
    let tools: Vec<Tool> = tool_fixtures
        .into_iter()
        .map(|(name, description, properties, required)| Tool {
            name: name.to_string(),
            description: description.to_string(),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
            constrained_sampling: None,
        })
        .collect();
    let context = Context {
        messages: vec![user_message("Think, answer, and use tools.", 0)],
        tools: Some(tools),
        ..Default::default()
    };
    let event_stream = stream_simple(&model, &context, Some(&simple_options(fetch, None)));

    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut tool_events_by_content_index: BTreeMap<usize, Vec<&'static str>> = BTreeMap::new();
    while let Some(event) = event_stream.next().await {
        let name = match &event {
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
            _ => continue,
        };
        *counts.entry(name).or_default() += 1;
        if let Some(content_index) = match &event {
            AssistantMessageEvent::ToolcallStart { content_index, .. }
            | AssistantMessageEvent::ToolcallDelta { content_index, .. }
            | AssistantMessageEvent::ToolcallEnd { content_index, .. } => Some(*content_index),
            _ => None,
        } {
            tool_events_by_content_index
                .entry(content_index)
                .or_default()
                .push(name);
        }
    }
    let response = event_stream.result().await;

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(counts.get("text_start"), Some(&1));
    assert_eq!(counts.get("text_delta"), Some(&3));
    assert_eq!(counts.get("text_end"), Some(&1));
    assert_eq!(counts.get("thinking_start"), Some(&1));
    assert_eq!(counts.get("thinking_delta"), Some(&2));
    assert_eq!(counts.get("thinking_end"), Some(&1));
    assert_eq!(counts.get("toolcall_start"), Some(&4));
    assert_eq!(counts.get("toolcall_delta"), Some(&9));
    assert_eq!(counts.get("toolcall_end"), Some(&4));
    assert_eq!(
        tool_events_by_content_index.get(&2).map(Vec::as_slice),
        Some(
            [
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end"
            ]
            .as_slice()
        )
    );
    assert_eq!(
        tool_events_by_content_index.get(&3).map(Vec::as_slice),
        Some(
            [
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end"
            ]
            .as_slice()
        )
    );
    assert_eq!(
        tool_events_by_content_index.get(&4).map(Vec::as_slice),
        Some(
            [
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end"
            ]
            .as_slice()
        )
    );
    assert_eq!(
        tool_events_by_content_index.get(&5).map(Vec::as_slice),
        Some(
            [
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end"
            ]
            .as_slice()
        )
    );

    assert_eq!(response.content.len(), 6);
    assert_eq!(response.content[0], text_block("answer 1 answer 2\n"));
    assert_eq!(
        response.content[1],
        AssistantContent::Thinking(ThinkingContent {
            thinking: "think 1 think 2".to_string(),
            thinking_signature: Some("reasoning_content".to_string()),
            ..Default::default()
        })
    );
    let expected_calls = [
        ("tc_read_initial", "read", json!({"path": "README.md"})),
        (
            "tc_grep_initial",
            "grep",
            json!({"pattern": "TODO", "path": "src"}),
        ),
        ("tc_list_no_index", "list", json!({"path": "packages/ai"})),
        (
            "tc_write_no_index",
            "write",
            json!({"path": "out.txt", "content": "ok"}),
        ),
    ];
    for (slot, (id, name, arguments)) in expected_calls.iter().enumerate() {
        let block = &response.content[2 + slot];
        assert_eq!(
            block,
            &AssistantContent::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: tool_arguments(arguments.clone()),
                ..Default::default()
            })
        );
        let serialized = match block {
            AssistantContent::ToolCall(call) => {
                serde_json::to_value(call).expect("serialized tool call")
            }
            _ => unreachable!("tool call block"),
        };
        assert!(serialized.get("streamIndex").is_none());
        assert!(serialized.get("partialArgs").is_none());
    }
}

#[tokio::test]
async fn uses_system_messages_for_non_openai_anthropic_openrouter_reasoning_model_instructions() {
    let model = get_builtin_model("openrouter", "deepseek/deepseek-v4-pro")
        .expect("openrouter deepseek model");
    let context = Context {
        system_prompt: Some("Follow instructions.".to_string()),
        messages: vec![user_message("Hi", 0)],
        ..Default::default()
    };
    let payload = capture_simple_payload(&model, &context, SimpleStreamOptions::default()).await;

    assert_eq!(payload["messages"][0]["role"], json!("system"));
}

#[tokio::test]
async fn keeps_developer_messages_for_openai_and_anthropic_openrouter_reasoning_model_instructions()
{
    for model_id in ["openai/gpt-5.2-codex", "anthropic/claude-sonnet-4.5"] {
        let model =
            get_builtin_model("openrouter", model_id).unwrap_or_else(|| panic!("{model_id}"));
        let context = Context {
            system_prompt: Some("Follow instructions.".to_string()),
            messages: vec![user_message("Hi", 0)],
            ..Default::default()
        };
        let payload =
            capture_simple_payload(&model, &context, SimpleStreamOptions::default()).await;

        assert_eq!(
            payload["messages"][0]["role"],
            json!("developer"),
            "{model_id}"
        );
    }
}

#[tokio::test]
async fn keeps_developer_messages_for_openai_reasoning_model_instructions() {
    let model = openai_builtin("openai", "gpt-5.5");
    let context = Context {
        system_prompt: Some("Follow instructions.".to_string()),
        messages: vec![user_message("Hi", 0)],
        ..Default::default()
    };
    let payload = capture_simple_payload(&model, &context, SimpleStreamOptions::default()).await;

    assert_eq!(payload["messages"][0]["role"], json!("developer"));
}

#[test]
fn stores_openrouter_kimi_k2_6_reasoning_replay_compat_in_built_in_metadata() {
    let model = get_builtin_model("openrouter", "moonshotai/kimi-k2.6")
        .expect("openrouter kimi-k2.6 model");
    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_developer_role, Some(false));
    assert_eq!(
        compat.requires_reasoning_content_on_assistant_messages,
        Some(true)
    );
}

#[test]
fn stores_xiaomi_mimo_reasoning_replay_compat_in_built_in_metadata() {
    for provider in [
        "xiaomi",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
    ] {
        let model =
            get_builtin_model(provider, "mimo-v2.5-pro").unwrap_or_else(|| panic!("{provider}"));
        let compat = model.compat.as_ref().expect("compat");
        assert_eq!(
            compat.requires_reasoning_content_on_assistant_messages,
            Some(true),
            "{provider}"
        );
        assert_eq!(
            compat.thinking_format,
            Some(ThinkingFormat::Deepseek),
            "{provider}"
        );
        assert!(compat.max_tokens_field.is_none(), "{provider}");
        assert!(compat.supports_developer_role.is_none(), "{provider}");
    }
}

#[test]
fn stores_qwen_token_plan_reasoning_replay_compat_in_built_in_metadata() {
    for provider in [
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
    ] {
        let model =
            get_builtin_model(provider, "qwen3.7-max").unwrap_or_else(|| panic!("{provider}"));
        let compat = model.compat.as_ref().expect("compat");
        assert_eq!(
            compat.thinking_format,
            Some(ThinkingFormat::Qwen),
            "{provider}"
        );
        assert!(
            compat
                .requires_reasoning_content_on_assistant_messages
                .is_none(),
            "{provider}"
        );
        assert_eq!(compat.supports_developer_role, Some(false), "{provider}");
        assert_eq!(compat.supports_store, Some(false), "{provider}");
    }
}

#[tokio::test]
async fn replays_xiaomi_mimo_assistant_tool_calls_with_empty_reasoning_content_when_thinking_is_missing()
 {
    let model = get_builtin_model("xiaomi", "mimo-v2.5-pro").expect("xiaomi mimo model");
    let assistant = AssistantMessage {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: tool_arguments(json!({"path": "README.md"})),
            ..Default::default()
        })],
        api: "openai-completions".to_string(),
        provider: "xiaomi".to_string(),
        model: "mimo-v2.5-pro".to_string(),
        usage: empty_usage(),
        stop_reason: StopReason::ToolUse,
        timestamp: 0,
        ..Default::default()
    };
    let tool_result = ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: "call_1".to_string(),
        tool_name: "read".to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: "contents".to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: 1,
        ..Default::default()
    };
    let context = Context {
        messages: vec![
            user_message("Read README.md", 0),
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(tool_result)),
        ],
        ..Default::default()
    };
    let payload = capture_simple_payload(
        &model,
        &context,
        SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        },
    )
    .await;

    let replayed_assistant = payload["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == json!("assistant"))
        .cloned()
        .expect("replayed assistant");
    assert_eq!(replayed_assistant["role"], json!("assistant"));
    assert_eq!(replayed_assistant["reasoning_content"], json!(""));
    assert_eq!(payload["thinking"], json!({"type": "enabled"}));
    assert_eq!(payload["reasoning_effort"], json!("high"));
}

#[tokio::test]
async fn normalizes_opencode_go_reasoning_deltas_to_reasoning_content_for_replay() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![json!({
        "id": "chatcmpl-opencode-go-reasoning",
        "choices": [{"delta": {"reasoning": "think"}, "finish_reason": "stop"}],
    })])]);

    let model = openai_builtin("opencode-go", "kimi-k2.6");
    let context = Context {
        messages: vec![user_message("Use reasoning.", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(
        response.content,
        vec![AssistantContent::Thinking(ThinkingContent {
            thinking: "think".to_string(),
            thinking_signature: Some("reasoning_content".to_string()),
            ..Default::default()
        })]
    );
}

#[tokio::test]
async fn keeps_non_opencode_go_reasoning_deltas_on_the_original_reasoning_field() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![json!({
        "id": "chatcmpl-reasoning",
        "choices": [{"delta": {"reasoning": "think"}, "finish_reason": "stop"}],
    })])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Use reasoning.", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(
        response.content,
        vec![AssistantContent::Thinking(ThinkingContent {
            thinking: "think".to_string(),
            thinking_signature: Some("reasoning".to_string()),
            ..Default::default()
        })]
    );
}

fn opencode_replay_resolved_compat() -> ResolvedCompletionsCompat {
    ResolvedCompletionsCompat {
        supports_store: false,
        supports_developer_role: false,
        supports_reasoning_effort: true,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: false,
        thinking_format: ThinkingFormat::Openai,
        open_router_routing: None,
        vercel_gateway_routing: Some(VercelGatewayRouting::default()),
        chat_template_kwargs: Some(ChatTemplateKwargs::new()),
        chat_template_args: Some(ChatTemplateKwargs::new()),
        zai_tool_stream: false,
        supports_thinking_token_budget: None,
        thinking_token_budget_field: None,
        supports_strict_mode: true,
        supports_openai_grammar_tools: false,
        cache_control_format: None,
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: SessionAffinityFormat::Openai,
        supports_long_cache_retention: true,
    }
}

#[test]
fn replays_opencode_go_reasoning_thinking_blocks_as_reasoning_content() {
    let model = openai_builtin("opencode-go", "kimi-k2.6");
    let assistant = AssistantMessage {
        content: vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "think".to_string(),
                thinking_signature: Some("reasoning".to_string()),
                ..Default::default()
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: tool_arguments(json!({"path": "README.md"})),
                ..Default::default()
            }),
        ],
        api: "openai-completions".to_string(),
        provider: "opencode-go".to_string(),
        model: "kimi-k2.6".to_string(),
        usage: empty_usage(),
        stop_reason: StopReason::Stop,
        timestamp: 0,
        ..Default::default()
    };
    let context = Context {
        messages: vec![Message::Assistant(Box::new(assistant))],
        ..Default::default()
    };

    let messages = convert_messages(
        &model,
        &context,
        &opencode_replay_resolved_compat(),
        &empty_grammar_properties(),
    )
    .expect("convertMessages");

    assert_eq!(messages[0]["role"], json!("assistant"));
    assert_eq!(messages[0]["reasoning_content"], json!("think"));
    assert!(messages[0].get("reasoning").is_none());
}

#[tokio::test]
async fn sends_thinking_disabled_for_opencode_go_kimi_k2_6_when_thinking_is_off() {
    let model = get_builtin_model("opencode-go", "kimi-k2.6").expect("opencode-go model");
    let context = Context {
        messages: vec![user_message("Hi", 0)],
        ..Default::default()
    };
    let payload = capture_simple_payload(&model, &context, SimpleStreamOptions::default()).await;

    assert_eq!(payload["thinking"], json!({"type": "disabled"}));
    assert!(payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn sends_thinking_enabled_for_opencode_go_kimi_k2_6_when_thinking_is_enabled() {
    let model = get_builtin_model("opencode-go", "kimi-k2.6").expect("opencode-go model");
    let context = Context {
        messages: vec![user_message("Hi", 0)],
        ..Default::default()
    };
    let payload = capture_simple_payload(
        &model,
        &context,
        SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(payload["thinking"], json!({"type": "enabled"}));
    assert!(payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn omits_disabled_thinking_for_moonshot_kimi_k2_7_code_models() {
    for provider in ["moonshotai", "moonshotai-cn"] {
        let model =
            get_builtin_model(provider, "kimi-k2.7-code").unwrap_or_else(|| panic!("{provider}"));
        let context = Context {
            messages: vec![user_message("Hi", 0)],
            ..Default::default()
        };
        let payload =
            capture_simple_payload(&model, &context, SimpleStreamOptions::default()).await;

        assert!(payload.get("thinking").is_none(), "{provider}");
        assert!(payload.get("reasoning_effort").is_none(), "{provider}");
    }
}

#[tokio::test]
async fn keeps_disabled_thinking_for_moonshot_kimi_k2_6_when_thinking_is_off() {
    let model = get_builtin_model("moonshotai-cn", "kimi-k2.6").expect("moonshot kimi-k2.6");
    let context = Context {
        messages: vec![user_message("Hi", 0)],
        ..Default::default()
    };
    let payload = capture_simple_payload(&model, &context, SimpleStreamOptions::default()).await;

    assert_eq!(payload["thinking"], json!({"type": "disabled"}));
    assert!(payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn does_not_double_count_reasoning_tokens_in_completion_usage() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![json!({
        "id": "chatcmpl-reasoning-usage",
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 33,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 21},
        },
    })])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Use reasoning.", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    assert_eq!(response.usage.input, 10);
    assert_eq!(response.usage.output, 33);
    assert_eq!(response.usage.total_tokens, 43);
}

#[tokio::test]
async fn preserves_prompt_tokens_details_cache_read_write_fields_from_chunk_usage() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-cache-write",
            "choices": [{"delta": {"content": "OK"}, "finish_reason": null}],
        }),
        json!({
            "id": "chatcmpl-cache-write",
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 50, "cache_write_tokens": 30},
                "completion_tokens_details": {"reasoning_tokens": 0},
            },
        }),
    ])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Reply with exactly OK", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    // cached_tokens is documented as cache reads; cache_write_tokens is separate.
    assert_eq!(response.usage.input, 20);
    assert_eq!(response.usage.cache_read, 50);
    assert_eq!(response.usage.cache_write, 30);
    assert_eq!(response.usage.total_tokens, 105);
}

#[tokio::test]
async fn preserves_prompt_tokens_details_cache_read_write_fields_from_choice_usage_fallback() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Sse(vec![
        json!({
            "id": "chatcmpl-cache-write-choice",
            "choices": [{"delta": {"content": "OK"}, "finish_reason": null}],
        }),
        json!({
            "id": "chatcmpl-cache-write-choice",
            "choices": [{
                "delta": {},
                "finish_reason": "stop",
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 50, "cache_write_tokens": 30},
                    "completion_tokens_details": {"reasoning_tokens": 0},
                },
            }],
        }),
    ])]);

    let model = openai_builtin("openai", "gpt-4o-mini");
    let context = Context {
        messages: vec![user_message("Reply with exactly OK", 0)],
        ..Default::default()
    };
    let response = stream_simple(&model, &context, Some(&simple_options(fetch, None)))
        .result()
        .await;

    // cached_tokens is documented as cache reads; cache_write_tokens is separate.
    assert_eq!(response.usage.input, 20);
    assert_eq!(response.usage.cache_read, 50);
    assert_eq!(response.usage.cache_write, 30);
    assert_eq!(response.usage.total_tokens, 105);
}

// ---------------------------------------------------------------------------
// openai-completions-retry.test.ts (stream-level cases)
// ---------------------------------------------------------------------------

fn retry_model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-completions".to_string(),
        provider: "opencode-go".to_string(),
        base_url: "https://opencode.ai/zen/go/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 1_000,
        max_tokens: 100,
        ..Default::default()
    }
}

fn retry_context() -> Context {
    Context {
        system_prompt: Some(String::new()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
                text: "hi".to_string(),
                ..Default::default()
            })]),
            timestamp: 0,
        })],
        tools: Some(Vec::new()),
    }
}

fn retry_options(
    fetch: Arc<ScriptedFetch>,
    max_retries: Option<u32>,
    max_retry_delay_ms: Option<u64>,
) -> OpenAICompletionsOptions {
    OpenAICompletionsOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("test".to_string()),
                fetch: Some(fetch),
                max_retries,
                max_retry_delay_ms,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn consume(options: &OpenAICompletionsOptions) -> AssistantMessage {
    let event_stream = stream(&retry_model(), &retry_context(), Some(options));
    let _ = collect_events(&event_stream).await;
    event_stream.result().await
}

/// TS "disables SDK retries by default": the options handed to the transport
/// carry `maxRetries: 0`, so a transient 5xx is attempted exactly once. The
/// Rust transport surfaces transient failures as fetch errors, and the
/// default retry policy leaves `max_retries` at 0.
#[tokio::test]
async fn disables_transport_retries_by_default() {
    let fetch = ScriptedFetch::new(vec![ScriptedResponse::Error("503 Service Unavailable")]);

    let result = consume(&retry_options(fetch.clone(), None, None)).await;

    assert_eq!(fetch.request_count(), 1, "no transport-level retry");
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(
        result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("503")),
        "unexpected error message: {:?}",
        result.error_message
    );
}

/// TS "honors provider retries while keeping SDK retries disabled": two
/// transient failures (429 then 500, both `retry-after-ms: 100`) followed by
/// success with `maxRetries: 2`. The paused clock makes the exponential
/// backoff between attempts advance instantly (the TS fake-timer boundary
/// assertions at 0/99/100ms are covered by `ai_retry.rs` at the helper
/// level).
#[tokio::test(start_paused = true)]
async fn honors_provider_retries_while_keeping_transport_retries_disabled() {
    let fetch = ScriptedFetch::new(vec![
        ScriptedResponse::Error("rate limited"),
        ScriptedResponse::Error("server error"),
        ScriptedResponse::Sse(vec![
            json!({"id": "chatcmpl-test", "choices": [{"index": 0, "delta": {"content": "ok"}}]}),
            json!({
                "id": "chatcmpl-test",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            }),
        ]),
    ]);

    let result = consume(&retry_options(fetch.clone(), Some(2), Some(100))).await;

    assert_eq!(fetch.request_count(), 3);
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.content, vec![text_block("ok")]);
}
