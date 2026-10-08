//! Port of `pi-core/ai/test/faux-provider.test.ts` in full, driven through
//! the compat global API (`registerFauxProvider` + `stream`/`complete`) like
//! the TypeScript suite. The registry is global, so the whole file
//! serializes on one lock.

use std::sync::Arc;

use pi_core::ai::compat::{complete, register_faux_provider, stream};
use pi_core::ai::providers::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, FauxTokenSize,
    RegisterFauxProviderOptions, faux_assistant_message, faux_text, faux_thinking, faux_tool_call,
};
use pi_core::ai::types::{
    AssistantMessageEvent, CacheRetention, Context, Message, ProviderRequestOptions, RoleUser,
    SimpleStreamOptions, StreamOptions, ToolResultMessage, UserContent, UserMessage,
};
use pi_core::ai::utils::event_stream::AssistantMessageEventStream;

async fn registry_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn user_context(content: &str) -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(content.to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

async fn collect_events(stream: &AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    pi_core::ai::utils::event_stream::collect_events(stream).await
}

fn event_kind(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

fn default_registration() -> pi_core::ai::compat::CompatFauxRegistration {
    register_faux_provider(RegisterFauxProviderOptions::default())
}

#[tokio::test]
async fn registers_a_custom_provider_and_estimates_usage() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("hello world", FauxMessageOptions::default()),
    ))]);

    let context = Context {
        system_prompt: Some("Be concise.".to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hi there".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    };

    let response = complete(&registration.get_model(), &context, None).await;
    match &response.content[0] {
        pi_core::ai::types::AssistantContent::Text(text) => {
            assert_eq!(text.text, "hello world")
        }
        other => panic!("expected text, got {other:?}"),
    }
    assert!(response.usage.input > 0);
    assert!(response.usage.output > 0);
    assert_eq!(
        response.usage.total_tokens,
        response.usage.input + response.usage.output
    );
    assert_eq!(registration.state.lock().unwrap().call_count, 1);
    registration.unregister();
}

#[tokio::test]
async fn supports_helper_blocks_for_text_thinking_and_tool_calls() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            vec![
                faux_thinking("think"),
                faux_tool_call("echo", serde_json::json!({ "text": "hi" }), None),
                faux_text("done"),
            ],
            FauxMessageOptions {
                stop_reason: Some(pi_core::ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        ),
    ))]);

    let response = complete(&registration.get_model(), &user_context("hi"), None).await;
    let thinking = matches!(
        &response.content[0],
        pi_core::ai::types::AssistantContent::Thinking(t) if t.thinking == "think"
    );
    assert!(thinking);
    match &response.content[1] {
        pi_core::ai::types::AssistantContent::ToolCall(call) => {
            assert_eq!(call.name, "echo");
            assert_eq!(
                serde_json::Value::Object(call.arguments.clone()),
                serde_json::json!({ "text": "hi" })
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    match &response.content[2] {
        pi_core::ai::types::AssistantContent::Text(text) => assert_eq!(text.text, "done"),
        other => panic!("expected text, got {other:?}"),
    }
    assert_eq!(
        response.stop_reason,
        pi_core::ai::types::StopReason::ToolUse
    );
    registration.unregister();
}

#[tokio::test]
async fn supports_multiple_models_with_per_model_reasoning_and_model_aware_factories() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        models: vec![
            FauxModelDefinition {
                id: "faux-fast".to_string(),
                name: Some("Faux Fast".to_string()),
                reasoning: Some(false),
                ..Default::default()
            },
            FauxModelDefinition {
                id: "faux-thinker".to_string(),
                name: Some("Faux Thinker".to_string()),
                reasoning: Some(true),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let factory = |_context: &Context,
                   _options: Option<&SimpleStreamOptions>,
                   _state: &pi_core::ai::providers::faux::FauxProviderState,
                   model: &pi_core::ai::types::Model| {
        Ok(faux_assistant_message(
            format!("{}:{}", model.id, model.reasoning),
            FauxMessageOptions::default(),
        ))
    };
    registration.set_responses(vec![
        FauxResponseStep::Factory(Arc::new(factory)),
        FauxResponseStep::Factory(Arc::new(factory)),
    ]);

    let ids: Vec<&str> = registration.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["faux-fast", "faux-thinker"]);
    assert_eq!(registration.get_model().id, "faux-fast");
    assert!(!registration.get_model_by_id("faux-fast").unwrap().reasoning);
    assert!(
        registration
            .get_model_by_id("faux-thinker")
            .unwrap()
            .reasoning
    );

    let fast = complete(
        &registration.get_model_by_id("faux-fast").unwrap(),
        &user_context("hi"),
        None,
    )
    .await;
    let thinker = complete(
        &registration.get_model_by_id("faux-thinker").unwrap(),
        &user_context("hi"),
        None,
    )
    .await;
    assert_eq!(text_of(&fast), "faux-fast:false");
    assert_eq!(text_of(&thinker), "faux-thinker:true");
    registration.unregister();
}

#[tokio::test]
async fn rewrites_api_provider_and_model_on_returned_messages() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        api: Some("faux:test".to_string()),
        provider: Some("faux-provider".to_string()),
        models: vec![FauxModelDefinition {
            id: "faux-model".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    });
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("hello", FauxMessageOptions::default()),
    ))]);

    let response = complete(&registration.get_model(), &user_context("hi"), None).await;
    assert_eq!(response.api, "faux:test");
    assert_eq!(response.provider, "faux-provider");
    assert_eq!(response.model, "faux-model");
    registration.unregister();
}

#[tokio::test]
async fn consumes_queued_responses_in_order_and_errors_when_exhausted() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![
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
    let first = complete(&registration.get_model(), &context, None).await;
    let second = complete(&registration.get_model(), &context, None).await;
    let exhausted = complete(&registration.get_model(), &context, None).await;

    assert_eq!(text_of(&first), "first");
    assert_eq!(text_of(&second), "second");
    assert_eq!(exhausted.stop_reason, pi_core::ai::types::StopReason::Error);
    assert_eq!(
        exhausted.error_message.as_deref(),
        Some("No more faux responses queued")
    );
    assert_eq!(registration.get_pending_response_count(), 0);
    assert_eq!(registration.state.lock().unwrap().call_count, 3);
    registration.unregister();
}

#[tokio::test]
async fn can_replace_and_append_queued_responses() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("first", FauxMessageOptions::default()),
    ))]);

    let context = user_context("hi");
    assert_eq!(
        text_of(&complete(&registration.get_model(), &context, None).await),
        "first"
    );
    assert_eq!(registration.get_pending_response_count(), 0);

    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("second", FauxMessageOptions::default()),
    ))]);
    assert_eq!(registration.get_pending_response_count(), 1);
    assert_eq!(
        text_of(&complete(&registration.get_model(), &context, None).await),
        "second"
    );

    registration.append_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "third",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "fourth",
            FauxMessageOptions::default(),
        ))),
    ]);
    assert_eq!(registration.get_pending_response_count(), 2);
    assert_eq!(
        text_of(&complete(&registration.get_model(), &context, None).await),
        "third"
    );
    assert_eq!(
        text_of(&complete(&registration.get_model(), &context, None).await),
        "fourth"
    );
    assert_eq!(registration.get_pending_response_count(), 0);
    registration.unregister();
}

#[tokio::test]
async fn supports_model_aware_factories_with_context_and_state() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Factory(Arc::new(
        |context: &Context,
         _options: Option<&SimpleStreamOptions>,
         state: &pi_core::ai::providers::faux::FauxProviderState,
         _model| {
            Ok(faux_assistant_message(
                format!("{}:{}", context.messages.len(), state.call_count),
                FauxMessageOptions::default(),
            ))
        },
    ))]);

    let response = complete(&registration.get_model(), &user_context("hi"), None).await;
    assert_eq!(text_of(&response), "1:1");
    registration.unregister();
}

#[tokio::test]
async fn emits_an_error_when_a_response_factory_fails() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    // The TS factory throws; the Rust factory port models the throw as Err.
    registration.set_responses(vec![FauxResponseStep::Factory(Arc::new(
        |_context, _options, _state, _model| Err("boom".to_string()),
    ))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;

    assert_eq!(events.len(), 1);
    match &events[0] {
        AssistantMessageEvent::Error { error, .. } => {
            assert_eq!(error.stop_reason, pi_core::ai::types::StopReason::Error);
            assert_eq!(error.error_message.as_deref(), Some("boom"));
        }
        other => panic!("expected error event, got {}", event_kind(other)),
    }
    registration.unregister();
}

#[tokio::test]
async fn rejects_a_queued_response_without_a_terminal_stop_reason() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "partial",
            FauxMessageOptions {
                stop_reason: Some(pi_core::ai::types::StopReason::Pending),
                ..Default::default()
            },
        ),
    ))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Done { .. }))
    );
    match events.last() {
        Some(AssistantMessageEvent::Error { error, .. }) => {
            assert_eq!(error.stop_reason, pi_core::ai::types::StopReason::Error);
            assert_eq!(
                error.error_message.as_deref(),
                Some("Faux response ended without a stop reason")
            );
        }
        other => panic!("expected terminal error, got {other:?}"),
    }
    registration.unregister();
}

#[tokio::test]
async fn estimates_prompt_and_output_tokens_from_serialized_context() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("done", FauxMessageOptions::default()),
    ))]);

    let tool = pi_core::ai::types::Tool {
        name: "echo".to_string(),
        description: "Echo back text".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        }),
        constrained_sampling: None,
    };
    let mut blocks = vec![pi_core::ai::types::BlockContent::Text(
        pi_core::ai::types::TextContent {
            text: "hello".to_string(),
            ..Default::default()
        },
    )];
    blocks.push(pi_core::ai::types::BlockContent::Image(
        pi_core::ai::types::ImageContent {
            data: "abcd".to_string(),
            mime_type: "image/png".to_string(),
            ..Default::default()
        },
    ));
    let context = Context {
        system_prompt: Some("sys".to_string()),
        messages: vec![
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Blocks(blocks),
                timestamp: 1,
            }),
            Message::Assistant(Box::new(faux_assistant_message(
                "prior",
                FauxMessageOptions::default(),
            ))),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: pi_core::ai::types::RoleToolResult,
                tool_call_id: "tool-1".to_string(),
                tool_name: "echo".to_string(),
                content: vec![pi_core::ai::types::BlockContent::Text(
                    pi_core::ai::types::TextContent {
                        text: "tool out".to_string(),
                        ..Default::default()
                    },
                )],
                is_error: false,
                timestamp: 2,
                ..Default::default()
            })),
        ],
        tools: Some(vec![tool.clone()]),
    };

    let response = complete(&registration.get_model(), &context, None).await;
    let prompt_text = [
        "system:sys".to_string(),
        "user:hello\n[image:image/png:4]".to_string(),
        "assistant:prior".to_string(),
        "toolResult:echo\ntool out".to_string(),
        format!(
            "tools:{}",
            serde_json::to_string(&serde_json::json!([tool])).unwrap()
        ),
    ]
    .join("\n\n");
    let expected_prompt_tokens = (prompt_text.chars().count() as f64 / 4.0).ceil() as u64;
    let expected_output_tokens = ("done".chars().count() as f64 / 4.0).ceil() as u64;

    assert_eq!(response.usage.input, expected_prompt_tokens);
    assert_eq!(response.usage.output, expected_output_tokens);
    assert_eq!(response.usage.cache_read, 0);
    assert_eq!(response.usage.cache_write, 0);
    assert_eq!(
        response.usage.total_tokens,
        expected_prompt_tokens + expected_output_tokens
    );
    registration.unregister();
}

fn cache_options(session_id: &str, retention: CacheRetention) -> StreamOptions {
    StreamOptions {
        base: ProviderRequestOptions::default(),
        session_id: Some(session_id.to_string()),
        cache_retention: Some(retention),
        ..Default::default()
    }
}

#[tokio::test]
async fn does_not_share_cache_across_sessions_or_requests_without_session_id() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "first",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "second",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "third",
            FauxMessageOptions::default(),
        ))),
    ]);

    let mut context = user_context("hello");
    let first = complete(
        &registration.get_model(),
        &context,
        Some(cache_options("session-1", CacheRetention::Short)),
    )
    .await;
    assert!(first.usage.cache_write > 0);
    context.messages.push(Message::Assistant(Box::new(first)));
    context.messages.push(Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text("follow up".to_string()),
        timestamp: 1,
    }));

    let second = complete(
        &registration.get_model(),
        &context,
        Some(cache_options("session-2", CacheRetention::Short)),
    )
    .await;
    assert_eq!(second.usage.cache_read, 0);
    assert!(second.usage.cache_write > 0);

    let third = complete(&registration.get_model(), &context, None).await;
    assert_eq!(third.usage.cache_read, 0);
    assert_eq!(third.usage.cache_write, 0);
    registration.unregister();
}

#[tokio::test]
async fn simulates_prompt_caching_per_session_id() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "first",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "second",
            FauxMessageOptions::default(),
        ))),
    ]);

    let mut context = Context {
        system_prompt: Some("Be concise.".to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    };
    let first = complete(
        &registration.get_model(),
        &context,
        Some(cache_options("session-1", CacheRetention::Short)),
    )
    .await;
    assert_eq!(first.usage.cache_read, 0);
    assert!(first.usage.cache_write > 0);

    context.messages.push(Message::Assistant(Box::new(first)));
    context.messages.push(Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text("follow up".to_string()),
        timestamp: 1,
    }));
    let second = complete(
        &registration.get_model(),
        &context,
        Some(cache_options("session-1", CacheRetention::Short)),
    )
    .await;
    assert!(second.usage.cache_read > 0);
    assert!(second.usage.input + second.usage.cache_read > second.usage.input);
    registration.unregister();
}

#[tokio::test]
async fn does_not_simulate_caching_when_cache_retention_is_none() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "first",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "second",
            FauxMessageOptions::default(),
        ))),
    ]);

    let mut context = user_context("hello");
    complete(
        &registration.get_model(),
        &context,
        Some(cache_options("session-1", CacheRetention::None)),
    )
    .await;
    context
        .messages
        .push(Message::Assistant(Box::new(faux_assistant_message(
            "first",
            FauxMessageOptions::default(),
        ))));
    context.messages.push(Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text("follow up".to_string()),
        timestamp: 1,
    }));
    let second = complete(
        &registration.get_model(),
        &context,
        Some(cache_options("session-1", CacheRetention::None)),
    )
    .await;
    assert_eq!(second.usage.cache_read, 0);
    assert_eq!(second.usage.cache_write, 0);
    registration.unregister();
}

#[tokio::test]
async fn streams_thinking_text_and_partial_tool_call_deltas() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            vec![
                faux_thinking("thinking text"),
                faux_text("answer text"),
                faux_tool_call(
                    "echo",
                    serde_json::json!({ "text": "hi", "count": 12 }),
                    None,
                ),
            ],
            FauxMessageOptions {
                stop_reason: Some(pi_core::ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        ),
    ))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    for expected in [
        "thinking_start",
        "thinking_delta",
        "text_start",
        "text_delta",
        "toolcall_start",
        "toolcall_delta",
        "toolcall_end",
    ] {
        assert!(kinds.contains(&expected), "missing {expected} in {kinds:?}");
    }
    let tool_deltas: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert!(tool_deltas.len() > 1);
    let joined: String = tool_deltas.concat();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
        serde_json::json!({ "text": "hi", "count": 12 })
    );
    registration.unregister();
}

#[tokio::test]
async fn streams_an_exact_event_order_for_fixed_size_chunks() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        token_size: Some(FauxTokenSize {
            min: Some(1),
            max: Some(1),
        }),
        ..Default::default()
    });
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            vec![
                faux_thinking("go"),
                faux_text("ok"),
                faux_tool_call("echo", serde_json::json!({}), None),
            ],
            FauxMessageOptions {
                stop_reason: Some(pi_core::ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        ),
    ))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(kinds[0], "start");
    assert_eq!(
        kinds,
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done",
        ]
    );
    registration.unregister();
}

#[tokio::test]
async fn streams_multiple_tool_calls_in_one_message() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            vec![
                faux_tool_call("echo", serde_json::json!({ "text": "one" }), None),
                faux_tool_call("echo", serde_json::json!({ "text": "two" }), None),
            ],
            FauxMessageOptions {
                stop_reason: Some(pi_core::ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        ),
    ))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;
    let starts = events
        .iter()
        .filter(|event| event_kind(event) == "toolcall_start")
        .count();
    let ends = events
        .iter()
        .filter(|event| event_kind(event) == "toolcall_end")
        .count();
    assert_eq!(starts, 2);
    assert_eq!(ends, 2);
    registration.unregister();
}

fn explicit_message(
    text: &str,
    stop_reason: pi_core::ai::types::StopReason,
    error: &str,
) -> pi_core::ai::types::AssistantMessage {
    let mut message = faux_assistant_message(text, FauxMessageOptions::default());
    message.stop_reason = stop_reason;
    message.error_message = Some(error.to_string());
    message
}

#[tokio::test]
async fn streams_an_explicit_assistant_error_message_as_a_terminal_error() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        token_size: Some(FauxTokenSize {
            min: Some(2),
            max: Some(2),
        }),
        ..Default::default()
    });
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(explicit_message(
        "partial",
        pi_core::ai::types::StopReason::Error,
        "upstream failed",
    )))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec!["start", "text_start", "text_delta", "text_end", "error"]
    );
    match events.last() {
        Some(AssistantMessageEvent::Error { reason, error, .. }) => {
            assert!(matches!(reason, pi_core::ai::types::ErrorReason::Error));
            assert_eq!(error.stop_reason, pi_core::ai::types::StopReason::Error);
            assert_eq!(error.error_message.as_deref(), Some("upstream failed"));
        }
        other => panic!("expected terminal error, got {other:?}"),
    }
    registration.unregister();
}

#[tokio::test]
async fn streams_an_explicit_assistant_aborted_message_as_a_terminal_error() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        token_size: Some(FauxTokenSize {
            min: Some(2),
            max: Some(2),
        }),
        ..Default::default()
    });
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(explicit_message(
        "partial",
        pi_core::ai::types::StopReason::Aborted,
        "Request was aborted",
    )))]);

    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        None,
    ))
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec!["start", "text_start", "text_delta", "text_end", "error"]
    );
    match events.last() {
        Some(AssistantMessageEvent::Error { reason, error, .. }) => {
            assert!(matches!(reason, pi_core::ai::types::ErrorReason::Aborted));
            assert_eq!(error.stop_reason, pi_core::ai::types::StopReason::Aborted);
            assert_eq!(error.error_message.as_deref(), Some("Request was aborted"));
        }
        other => panic!("expected terminal error, got {other:?}"),
    }
    registration.unregister();
}

#[tokio::test(start_paused = true)]
async fn supports_aborting_before_the_first_chunk() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(50.0),
        token_size: Some(FauxTokenSize {
            min: Some(3),
            max: Some(3),
        }),
        ..Default::default()
    });
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("abcdefghijklmnopqrstuvwxyz", FauxMessageOptions::default()),
    ))]);

    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let options = StreamOptions {
        base: ProviderRequestOptions {
            signal: Some(signal),
            ..Default::default()
        },
        ..Default::default()
    };
    let events = collect_events(&stream(
        &registration.get_model(),
        &user_context("hi"),
        Some(&options),
    ))
    .await;

    assert_eq!(events.len(), 1);
    match &events[0] {
        AssistantMessageEvent::Error { reason, error, .. } => {
            assert!(matches!(reason, pi_core::ai::types::ErrorReason::Aborted));
            assert_eq!(error.stop_reason, pi_core::ai::types::StopReason::Aborted);
        }
        other => panic!("expected error event, got {}", event_kind(other)),
    }
    registration.unregister();
}

async fn collect_until_abort(
    registration: &pi_core::ai::compat::CompatFauxRegistration,
    abort_kind: &str,
) -> Vec<String> {
    let signal = tokio_util::sync::CancellationToken::new();
    let options = StreamOptions {
        base: ProviderRequestOptions {
            signal: Some(signal.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let s = stream(
        &registration.get_model(),
        &user_context("hi"),
        Some(&options),
    );
    let mut kinds = Vec::new();
    let mut delta_count = 0usize;
    while let Some(event) = s.next().await {
        let kind = event_kind(&event).to_string();
        if kind == abort_kind {
            delta_count += 1;
            signal.cancel();
        }
        kinds.push(kind);
    }
    assert_eq!(delta_count, 1);
    kinds
}

#[tokio::test(start_paused = true)]
async fn supports_aborting_mid_text_stream_when_paced() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(100.0),
        token_size: Some(FauxTokenSize {
            min: Some(3),
            max: Some(3),
        }),
        ..Default::default()
    });
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("abcdefghijklmnopqrstuvwxyz", FauxMessageOptions::default()),
    ))]);

    let kinds = collect_until_abort(&registration, "text_delta").await;
    assert!(kinds.contains(&"text_start".to_string()));
    assert!(kinds.contains(&"text_delta".to_string()));
    assert!(kinds.contains(&"error".to_string()));
    assert!(!kinds.contains(&"text_end".to_string()));
    registration.unregister();
}

#[tokio::test(start_paused = true)]
async fn supports_aborting_mid_thinking_stream_when_paced() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(100.0),
        token_size: Some(FauxTokenSize {
            min: Some(3),
            max: Some(3),
        }),
        ..Default::default()
    });
    let mut message = faux_assistant_message("ignored", FauxMessageOptions::default());
    message.content = vec![pi_core::ai::types::AssistantContent::Thinking(
        pi_core::ai::types::ThinkingContent {
            thinking: "abcdefghijklmnopqrstuvwxyz".to_string(),
            ..Default::default()
        },
    )];
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(message))]);

    let kinds = collect_until_abort(&registration, "thinking_delta").await;
    assert!(kinds.contains(&"thinking_start".to_string()));
    assert!(kinds.contains(&"thinking_delta".to_string()));
    assert!(kinds.contains(&"error".to_string()));
    assert!(!kinds.contains(&"thinking_end".to_string()));
    registration.unregister();
}

#[tokio::test(start_paused = true)]
async fn supports_aborting_mid_toolcall_stream_when_paced() {
    let _guard = registry_lock().await;
    let registration = register_faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(100.0),
        token_size: Some(FauxTokenSize {
            min: Some(3),
            max: Some(3),
        }),
        ..Default::default()
    });
    let mut message = faux_assistant_message(
        "done",
        FauxMessageOptions {
            stop_reason: Some(pi_core::ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    );
    message.content = vec![pi_core::ai::types::AssistantContent::ToolCall(
        pi_core::ai::types::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: json_args(&serde_json::json!({
                "text": "abcdefghijklmnopqrstuvwxyz",
                "count": 123456789,
            })),
            ..Default::default()
        },
    )];
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(message))]);

    let kinds = collect_until_abort(&registration, "toolcall_delta").await;
    assert!(kinds.contains(&"toolcall_start".to_string()));
    assert!(kinds.contains(&"toolcall_delta".to_string()));
    assert!(kinds.contains(&"error".to_string()));
    assert!(!kinds.contains(&"toolcall_end".to_string()));
    registration.unregister();
}

#[tokio::test]
async fn unregisters_the_provider() {
    let _guard = registry_lock().await;
    let registration = default_registration();
    registration.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("hello", FauxMessageOptions::default()),
    ))]);
    registration.unregister();

    // The dispatch panics on first poll inside the spawned task; the join
    // error carries the TS-exact panic message.
    let model = registration.get_model();
    let api = registration.api.clone();
    let handle = tokio::spawn(async move {
        let _ = complete(&model, &user_context("hi"), None).await;
    });
    let join_error = handle.await.expect_err("expected the dispatch to panic");
    assert!(join_error.is_panic());
    let panic = join_error.into_panic();
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_default();
    assert_eq!(
        message,
        format!("No API provider registered for api: {api}")
    );
}

fn json_args(value: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn text_of(message: &pi_core::ai::types::AssistantMessage) -> String {
    match &message.content[0] {
        pi_core::ai::types::AssistantContent::Text(text) => text.text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}
