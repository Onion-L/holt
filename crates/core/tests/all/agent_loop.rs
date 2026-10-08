//! Port of `pi-core/agent/test/agent-loop.test.ts`.
//!
//! Mock stream functions push their scripted events synchronously before
//! returning the stream, which yields the same event sequence as the
//! TypeScript `queueMicrotask` mocks.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::json;

use pi_core::agent::agent_loop::{
    agent_loop, agent_loop_continue, pass_through_llm_messages, run_agent_loop,
    run_agent_loop_continue,
};
use pi_core::agent::stream_fn::set_default_stream_fn;
use pi_core::agent::types::{
    AfterToolCallResult, AgentContext, AgentEvent, AgentLoopConfig, AgentLoopTurnUpdate,
    AgentMessage, AgentTool, AgentToolResult, BeforeToolCallResult, StreamFn, ToolExecutionMode,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent, DoneReason, Message,
    Model, ModelInput, RoleUser, StopReason, TextContent, ToolCall, Usage,
};
use pi_core::ai::utils::event_stream::{EventStream, collect_events};

fn create_usage() -> Usage {
    Usage::default()
}

fn create_model() -> Model {
    Model {
        id: "mock".to_string(),
        name: "mock".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://example.invalid".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        context_window: 8192,
        max_tokens: 2048,
        ..Default::default()
    }
}

fn create_assistant_message(
    content: Vec<AssistantContent>,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        role: Default::default(),
        content,
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "mock".to_string(),
        usage: create_usage(),
        stop_reason,
        timestamp: 0,
        ..Default::default()
    }
}

fn create_user_message(text: &str) -> AgentMessage {
    AgentMessage::User(pi_core::ai::types::UserMessage {
        role: RoleUser,
        content: pi_core::ai::types::UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn identity_converter(messages: Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> {
    Box::pin(async move { pass_through_llm_messages(messages) })
}

/// A stream function that answers each call with the messages produced by
/// `script(index)`; the shared counter tracks how many LLM calls happened.
fn scripted_stream_fn(
    script: impl Fn(usize) -> Vec<AssistantMessage> + Send + Sync + 'static,
) -> (StreamFn, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let calls = Arc::clone(&calls);
        Arc::new(move |_model, _context, _options| {
            let index = {
                let mut calls = calls.lock().unwrap();
                let index = *calls;
                *calls += 1;
                index
            };
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            for message in script(index) {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message,
                });
            }
            Ok(stream)
        })
    };
    (stream_fn, calls)
}

fn tool_call_block(id: &str, name: &str, arguments: serde_json::Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    })
}

fn text_content(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.to_string(),
        ..Default::default()
    })
}

/// Builds an `AgentTool` whose execute records each `value` argument.
fn echo_tool(
    parameters: serde_json::Value,
    executed: Arc<Mutex<Vec<serde_json::Value>>>,
) -> AgentTool {
    AgentTool {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echo tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |_tool_call_id, params, _signal, _on_update| {
            let executed = Arc::clone(&executed);
            let value = params
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Box::pin(async move {
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult {
                    content: vec![BlockContent::Text(TextContent {
                        text: format!("echoed: {value}"),
                        ..Default::default()
                    })],
                    details: json!({ "value": value }),
                    ..Default::default()
                })
            })
        }),
    }
}

fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

fn event_type(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
    }
}

fn message_roles(messages: &[AgentMessage]) -> Vec<&str> {
    messages.iter().map(|message| message.role()).collect()
}

async fn run_collect(
    stream: &EventStream<AgentEvent, Vec<AgentMessage>>,
) -> (Vec<AgentEvent>, Vec<AgentMessage>) {
    let events = collect_events(stream).await;
    let messages = stream.result().await;
    (events, messages)
}

/// An `AgentLoopConfig` with the identity converter and mock model; tests
/// override individual fields from this base.
fn base_config() -> AgentLoopConfig {
    AgentLoopConfig {
        stream_options: Default::default(),
        model: create_model(),
        convert_to_llm: Arc::new(identity_converter),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        tool_execution: None,
        before_tool_call: None,
        after_tool_call: None,
    }
}

#[tokio::test]
async fn uses_the_configured_default_when_a_legacy_caller_omits_stream_fn() {
    let calls = Arc::new(Mutex::new(0));
    let default_fn: StreamFn = {
        let calls = Arc::clone(&calls);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  _options: Option<&pi_core::ai::types::SimpleStreamOptions>| {
                *calls.lock().unwrap() += 1;
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(
                        vec![text_content("fallback")],
                        StopReason::Stop,
                    ),
                });
                Ok(stream)
            },
        )
    };
    set_default_stream_fn(Some(default_fn));

    let context = AgentContext::default();
    let config = base_config();
    let stream = agent_loop(
        vec![create_user_message("Hello")],
        context,
        config,
        None,
        None,
    );

    stream.result().await;
    assert_eq!(*calls.lock().unwrap(), 1);
    set_default_stream_fn(None);
}

#[tokio::test]
async fn emits_events_with_agent_message_types() {
    let context = AgentContext::default();
    let config = base_config();
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("Hi there!")],
            StopReason::Stop,
        )]
    });

    let stream = agent_loop(
        vec![create_user_message("Hello")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, messages) = run_collect(&stream).await;

    assert_eq!(message_roles(&messages), ["user", "assistant"]);
    let event_types = events.iter().map(event_type).collect::<Vec<_>>();
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(
            event_types.contains(&expected),
            "missing {expected}: {event_types:?}"
        );
    }
}

#[tokio::test]
async fn handles_custom_message_types_via_convert_to_llm() {
    let notification = AgentMessage::Custom(pi_core::agent::types::CustomAgentMessage::new(
        "notification",
        json!({ "text": "This is a notification", "timestamp": 0 }),
    ));
    let context = AgentContext {
        messages: vec![notification],
        ..Default::default()
    };

    let converted: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let config = AgentLoopConfig {
        convert_to_llm: {
            let converted = Arc::clone(&converted);
            Arc::new(move |messages| {
                let filtered = pass_through_llm_messages(messages);
                *converted.lock().unwrap() = filtered.clone();
                Box::pin(async move { filtered })
            })
        },
        ..base_config()
    };
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("Response")],
            StopReason::Stop,
        )]
    });

    let stream = agent_loop(
        vec![create_user_message("Hello")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    let converted = converted.lock().unwrap().clone();
    assert_eq!(converted.len(), 1);
    assert!(matches!(converted[0], Message::User(_)));
}

#[tokio::test]
async fn applies_transform_context_before_convert_to_llm() {
    let context = AgentContext {
        messages: vec![
            create_user_message("old message 1"),
            AgentMessage::Assistant(Box::new(create_assistant_message(
                vec![text_content("old response 1")],
                StopReason::Stop,
            ))),
            create_user_message("old message 2"),
            AgentMessage::Assistant(Box::new(create_assistant_message(
                vec![text_content("old response 2")],
                StopReason::Stop,
            ))),
        ],
        ..Default::default()
    };

    let transformed: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let converted: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));

    let config = AgentLoopConfig {
        transform_context: Some(Arc::new({
            let transformed = Arc::clone(&transformed);
            move |messages, _signal| {
                // Keep only last 2 messages (prune old ones)
                let pruned: Vec<AgentMessage> = messages.into_iter().rev().take(2).rev().collect();
                *transformed.lock().unwrap() = pruned.clone();
                Box::pin(async move { pruned })
            }
        })),
        convert_to_llm: {
            let converted = Arc::clone(&converted);
            Arc::new(move |messages| {
                let llm = pass_through_llm_messages(messages);
                *converted.lock().unwrap() = llm.clone();
                Box::pin(async move { llm })
            })
        },
        ..base_config()
    };
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("Response")],
            StopReason::Stop,
        )]
    });

    let stream = agent_loop(
        vec![create_user_message("new message")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    assert_eq!(transformed.lock().unwrap().len(), 2);
    assert_eq!(converted.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn handles_tool_calls_and_results() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let tool_usage = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        total_tokens: 10,
        cost: pi_core::ai::types::UsageCost {
            input: 0.1.into(),
            output: 0.2.into(),
            cache_read: 0.3.into(),
            cache_write: 0.4.into(),
            total: 1.0.into(),
        },
        ..Default::default()
    };
    let patched_tool_usage = Usage {
        input: 5,
        output: 6,
        cache_read: 7,
        cache_write: 8,
        total_tokens: 26,
        cost: pi_core::ai::types::UsageCost {
            input: 0.5.into(),
            output: 0.6.into(),
            cache_read: 0.7.into(),
            cache_write: 0.8.into(),
            total: 2.6.into(),
        },
        ..Default::default()
    };
    let observed_tool_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));

    let tool = AgentTool {
        execute: {
            let executed = Arc::clone(&executed);
            let tool_usage = tool_usage.clone();
            Arc::new(move |_tool_call_id, params, _signal, _on_update| {
                let executed = Arc::clone(&executed);
                let tool_usage = tool_usage.clone();
                let value = params
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Box::pin(async move {
                    executed.lock().unwrap().push(value.clone());
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: format!("echoed: {}", value.as_str().unwrap_or_default()),
                            ..Default::default()
                        })],
                        details: json!({ "value": value }),
                        usage: Some(tool_usage),
                        ..Default::default()
                    })
                })
            })
        },
        ..echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )
    };

    let context = AgentContext {
        tools: Some(vec![tool]),
        ..Default::default()
    };

    let config = AgentLoopConfig {
        after_tool_call: Some(Arc::new({
            let observed_tool_usage = Arc::clone(&observed_tool_usage);
            let patched = patched_tool_usage.clone();
            move |context, _signal| {
                *observed_tool_usage.lock().unwrap() = context.result.usage.clone();
                let patched = patched.clone();
                Box::pin(async move {
                    Some(AfterToolCallResult {
                        usage: Some(patched),
                        ..Default::default()
                    })
                })
            }
        })),
        ..base_config()
    };

    let (stream_fn, _calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![tool_call_block(
                    "tool-1",
                    "echo",
                    json!({ "value": "hello" }),
                )],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, messages) = run_collect(&stream).await;

    let executed = executed.lock().unwrap().clone();
    assert_eq!(executed, [json!("hello")]);

    let tool_start = events
        .iter()
        .find(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }));
    let tool_end = events
        .iter()
        .find(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }));
    assert!(tool_start.is_some());
    assert!(tool_end.is_some());
    if let Some(AgentEvent::ToolExecutionEnd { is_error, .. }) = tool_end {
        assert!(!is_error);
    }
    assert_eq!(
        observed_tool_usage.lock().unwrap().clone(),
        Some(tool_usage)
    );
    let tool_result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some((**result).clone()),
            _ => None,
        })
        .expect("tool result message");
    assert_eq!(tool_result.usage, Some(patched_tool_usage));
}

#[tokio::test]
async fn does_not_execute_tool_calls_from_a_length_truncated_message() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        tools: Some(vec![echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )]),
        ..Default::default()
    };
    let config = base_config();

    let (stream_fn, calls) = scripted_stream_fn(|index| {
        if index == 0 {
            // Output hit the token limit mid tool call; nothing in this
            // message may execute.
            vec![create_assistant_message(
                vec![tool_call_block("tool-1", "echo", json!({ "value": "hel" }))],
                StopReason::Length,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, messages) = run_collect(&stream).await;

    assert!(executed.lock().unwrap().is_empty());

    let tool_end = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => Some((result.clone(), *is_error)),
            _ => None,
        })
        .expect("tool_execution_end");
    assert!(tool_end.1);
    let text = tool_end
        .0
        .content
        .iter()
        .filter_map(|block| match block {
            BlockContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("output token limit"),
        "unexpected text: {text}"
    );

    // The loop continues so the model can re-issue the tool call.
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(messages.last().unwrap().role(), "assistant");
}

#[tokio::test]
async fn executes_mutated_before_tool_call_args_without_revalidation() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        tools: Some(vec![echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )]),
        ..Default::default()
    };

    let config = AgentLoopConfig {
        before_tool_call: Some(Arc::new(|context, _signal| {
            // Mutate the shared validated-arguments object in place.
            {
                let mut args = context.args.lock().unwrap();
                if let Some(value) = args.get_mut("value") {
                    *value = json!(123);
                }
            }
            Box::pin(async move { None })
        })),
        ..base_config()
    };

    let (stream_fn, _calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![tool_call_block(
                    "tool-1",
                    "echo",
                    json!({ "value": "hello" }),
                )],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    assert_eq!(*executed.lock().unwrap(), [json!(123)]);
}

#[tokio::test]
async fn prepares_tool_arguments_for_validation() {
    let executed: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let parameters = json!({
        "type": "object",
        "required": ["edits"],
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["oldText", "newText"],
                    "properties": {
                        "oldText": { "type": "string" },
                        "newText": { "type": "string" }
                    }
                }
            }
        }
    });
    let tool = AgentTool {
        name: "edit".to_string(),
        label: "Edit".to_string(),
        description: "Edit tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: Some(Arc::new(|args| {
            let Some(old_text) = args.get("oldText").and_then(|value| value.as_str()) else {
                return args.clone();
            };
            let Some(new_text) = args.get("newText").and_then(|value| value.as_str()) else {
                return args.clone();
            };
            let mut edits = args
                .get("edits")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            edits.push(json!({ "oldText": old_text, "newText": new_text }));
            json!({ "edits": edits })
        })),
        execution_mode: None,
        execute: {
            let executed = Arc::clone(&executed);
            Arc::new(move |_tool_call_id, params, _signal, _on_update| {
                let executed = Arc::clone(&executed);
                let edits = params
                    .get("edits")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Box::pin(async move {
                    let count = edits.as_array().map(|list| list.len()).unwrap_or(0);
                    executed.lock().unwrap().push(edits);
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: format!("edited {count}"),
                            ..Default::default()
                        })],
                        details: json!({ "count": count }),
                        ..Default::default()
                    })
                })
            })
        },
    };

    let context = AgentContext {
        tools: Some(vec![tool]),
        ..Default::default()
    };
    let config = base_config();

    let (stream_fn, _calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![tool_call_block(
                    "tool-1",
                    "edit",
                    json!({ "oldText": "before", "newText": "after" }),
                )],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("edit something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    assert_eq!(
        *executed.lock().unwrap(),
        [json!([{ "oldText": "before", "newText": "after" }])]
    );
}

#[tokio::test]
async fn emits_tool_execution_end_in_completion_order_persisting_results_in_source_order() {
    let first_resolved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let parallel_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_first = Arc::new(tokio::sync::Notify::new());

    let tool = AgentTool {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: object_schema(json!({ "value": { "type": "string" } }), &["value"]),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: {
            let first_resolved = Arc::clone(&first_resolved);
            let parallel_observed = Arc::clone(&parallel_observed);
            let release_first = Arc::clone(&release_first);
            Arc::new(move |_tool_call_id, params, _signal, _on_update| {
                let first_resolved = Arc::clone(&first_resolved);
                let parallel_observed = Arc::clone(&parallel_observed);
                let release_first = Arc::clone(&release_first);
                let value = params
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Box::pin(async move {
                    if value == json!("first") {
                        release_first.notified().await;
                        first_resolved.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    if value == json!("second")
                        && !first_resolved.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        parallel_observed.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: format!("echoed: {}", value.as_str().unwrap_or_default()),
                            ..Default::default()
                        })],
                        details: json!({ "value": value }),
                        ..Default::default()
                    })
                })
            })
        },
    };

    let context = AgentContext {
        tools: Some(vec![tool]),
        ..Default::default()
    };
    let config = AgentLoopConfig {
        tool_execution: Some(ToolExecutionMode::Parallel),
        ..base_config()
    };

    let calls = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let calls = Arc::clone(&calls);
        let release_first = Arc::clone(&release_first);
        Arc::new(move |_model, _context, _options| {
            let index = {
                let mut calls = calls.lock().unwrap();
                let index = *calls;
                *calls += 1;
                index
            };
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            if index == 0 {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::ToolUse,
                    message: create_assistant_message(
                        vec![
                            tool_call_block("tool-1", "echo", json!({ "value": "first" })),
                            tool_call_block("tool-2", "echo", json!({ "value": "second" })),
                        ],
                        StopReason::ToolUse,
                    ),
                });
                let release_first = Arc::clone(&release_first);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    release_first.notify_one();
                });
            } else {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(vec![text_content("done")], StopReason::Stop),
                });
            }
            Ok(stream)
        })
    };

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, _messages) = run_collect(&stream).await;

    let tool_execution_end_ids = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let tool_result_ids = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message } => match &**message {
                AgentMessage::ToolResult(result) => Some(result.tool_call_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    let turn_tool_result_ids = events
        .iter()
        .flat_map(|event| match event {
            AgentEvent::TurnEnd { tool_results, .. } => tool_results
                .iter()
                .map(|result| result.tool_call_id.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();

    assert!(parallel_observed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(tool_execution_end_ids, ["tool-2", "tool-1"]);
    assert_eq!(tool_result_ids, ["tool-1", "tool-2"]);
    assert_eq!(turn_tool_result_ids, ["tool-1", "tool-2"]);
}

#[tokio::test]
async fn injects_queued_messages_after_all_tool_calls_complete() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        tools: Some(vec![echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )]),
        ..Default::default()
    };

    let queued_delivered = Arc::new(Mutex::new(false));
    let saw_interrupt_in_context = Arc::new(Mutex::new(false));
    let config = AgentLoopConfig {
        tool_execution: Some(ToolExecutionMode::Sequential),
        get_steering_messages: Some({
            let executed = Arc::clone(&executed);
            let queued_delivered = Arc::clone(&queued_delivered);
            Arc::new(move || {
                let executed = executed.lock().unwrap().len();
                let mut queued_delivered = queued_delivered.lock().unwrap();
                let messages = if executed >= 1 && !*queued_delivered {
                    *queued_delivered = true;
                    vec![create_user_message("interrupt")]
                } else {
                    Vec::new()
                };
                Box::pin(async move { messages })
            })
        }),
        ..base_config()
    };

    let calls = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let calls = Arc::clone(&calls);
        let saw_interrupt_in_context = Arc::clone(&saw_interrupt_in_context);
        Arc::new(move |_model, context, _options| {
            let index = {
                let mut calls = calls.lock().unwrap();
                let index = *calls;
                *calls += 1;
                index
            };
            if index == 1 {
                let saw = context.messages.iter().any(|message| {
                    matches!(
                        message,
                        Message::User(user)
                            if matches!(&user.content, pi_core::ai::types::UserContent::Text(text) if text == "interrupt")
                    )
                });
                *saw_interrupt_in_context.lock().unwrap() = saw;
            }
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            if index == 0 {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::ToolUse,
                    message: create_assistant_message(
                        vec![
                            tool_call_block("tool-1", "echo", json!({ "value": "first" })),
                            tool_call_block("tool-2", "echo", json!({ "value": "second" })),
                        ],
                        StopReason::ToolUse,
                    ),
                });
            } else {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(vec![text_content("done")], StopReason::Stop),
                });
            }
            Ok(stream)
        })
    };

    let stream = agent_loop(
        vec![create_user_message("start")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, _messages) = run_collect(&stream).await;

    let executed_values = executed
        .lock()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(executed_values, ["first", "second"]);

    let tool_ends = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_ends, [false, false]);

    let event_sequence = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart { message } => match &**message {
                AgentMessage::ToolResult(result) => Some(format!("tool:{}", result.tool_call_id)),
                AgentMessage::User(user) => match &user.content {
                    pi_core::ai::types::UserContent::Text(text) => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    let interrupt_at = event_sequence
        .iter()
        .position(|entry| entry == "interrupt")
        .expect("interrupt delivered");
    assert!(
        event_sequence
            .iter()
            .position(|entry| entry == "tool:tool-1")
            .unwrap()
            < interrupt_at
    );
    assert!(
        event_sequence
            .iter()
            .position(|entry| entry == "tool:tool-2")
            .unwrap()
            < interrupt_at
    );

    assert!(*saw_interrupt_in_context.lock().unwrap());
}

fn slow_fast_sequential_tools(
    execution_order: Arc<Mutex<Vec<String>>>,
) -> (AgentTool, AgentTool, Arc<tokio::sync::Notify>) {
    let parameters = object_schema(json!({ "value": { "type": "string" } }), &["value"]);
    let slow_done = Arc::new(tokio::sync::Notify::new());
    let slow = AgentTool {
        name: "slow".to_string(),
        label: "Slow".to_string(),
        description: "Slow tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: Some(ToolExecutionMode::Sequential),
        execute: {
            let execution_order = Arc::clone(&execution_order);
            let slow_done = Arc::clone(&slow_done);
            Arc::new(move |_tool_call_id, params, _signal, _on_update| {
                let execution_order = Arc::clone(&execution_order);
                let slow_done = Arc::clone(&slow_done);
                let value = params
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Box::pin(async move {
                    execution_order
                        .lock()
                        .unwrap()
                        .push(format!("slow:{}", value.as_str().unwrap_or_default()));
                    if value == json!("a") {
                        slow_done.notified().await;
                    }
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: format!("slow: {}", value.as_str().unwrap_or_default()),
                            ..Default::default()
                        })],
                        details: json!({ "value": value }),
                        ..Default::default()
                    })
                })
            })
        },
    };
    let fast = AgentTool {
        name: "fast".to_string(),
        label: "Fast".to_string(),
        description: "Fast tool".to_string(),
        parameters: object_schema(json!({ "value": { "type": "string" } }), &["value"]),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: {
            let execution_order = Arc::clone(&execution_order);
            Arc::new(move |_tool_call_id, params, _signal, _on_update| {
                let execution_order = Arc::clone(&execution_order);
                let value = params
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Box::pin(async move {
                    execution_order
                        .lock()
                        .unwrap()
                        .push(format!("fast:{}", value.as_str().unwrap_or_default()));
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: format!("fast: {}", value.as_str().unwrap_or_default()),
                            ..Default::default()
                        })],
                        details: json!({ "value": value }),
                        ..Default::default()
                    })
                })
            })
        },
    };
    (slow, fast, slow_done)
}

#[tokio::test]
async fn forces_sequential_execution_when_one_of_multiple_tools_is_sequential() {
    let execution_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (slow, fast, slow_done) = slow_fast_sequential_tools(Arc::clone(&execution_order));
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        slow_done.notify_one();
    });

    let context = AgentContext {
        tools: Some(vec![slow, fast]),
        ..Default::default()
    };
    let config = base_config();

    let calls = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let calls = Arc::clone(&calls);
        Arc::new(move |_model, _context, _options| {
            let index = {
                let mut calls = calls.lock().unwrap();
                let index = *calls;
                *calls += 1;
                index
            };
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            if index == 0 {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::ToolUse,
                    message: create_assistant_message(
                        vec![
                            tool_call_block("tool-1", "slow", json!({ "value": "a" })),
                            tool_call_block("tool-2", "fast", json!({ "value": "b" })),
                        ],
                        StopReason::ToolUse,
                    ),
                });
            } else {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(vec![text_content("done")], StopReason::Stop),
                });
            }
            Ok(stream)
        })
    };

    let stream = agent_loop(
        vec![create_user_message("run both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    let order = execution_order.lock().unwrap().clone();
    assert_eq!(order.first().map(String::as_str), Some("slow:a"));
    assert!(order.contains(&"fast:b".to_string()));
}

#[tokio::test]
async fn uses_prepare_next_turn_snapshot_before_continuing() {
    let parameters = object_schema(json!({ "value": { "type": "string" } }), &["value"]);
    let tool = AgentTool {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echo tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_tool_call_id, params, _signal, _on_update| {
            let value = params
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![BlockContent::Text(TextContent {
                        text: format!("echoed: {}", value.as_str().unwrap_or_default()),
                        ..Default::default()
                    })],
                    details: json!({ "value": value }),
                    ..Default::default()
                })
            })
        }),
    };
    let context = AgentContext {
        system_prompt: "first prompt".to_string(),
        tools: Some(vec![tool]),
        ..Default::default()
    };

    let converted_second_turn_system_prompt = Arc::new(Mutex::new(String::new()));
    let prepare_calls = Arc::new(Mutex::new(0));
    let prepared = Arc::new(Mutex::new(false));

    let config = AgentLoopConfig {
        prepare_next_turn: Some({
            let prepare_calls = Arc::clone(&prepare_calls);
            let prepared = Arc::clone(&prepared);
            Arc::new(move |turn| {
                *prepare_calls.lock().unwrap() += 1;
                if *prepared.lock().unwrap() {
                    return Box::pin(async move { None });
                }
                *prepared.lock().unwrap() = true;
                let context = pi_core::agent::types::AgentContext {
                    system_prompt: "second prompt".to_string(),
                    messages: turn.context.messages.clone(),
                    tools: turn.context.tools.clone(),
                };
                Box::pin(async move {
                    Some(AgentLoopTurnUpdate {
                        context: Some(context),
                        ..Default::default()
                    })
                })
            })
        }),
        ..base_config()
    };

    let llm_calls = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let llm_calls = Arc::clone(&llm_calls);
        let converted = Arc::clone(&converted_second_turn_system_prompt);
        Arc::new(move |_model, context, _options| {
            let call = {
                let mut llm_calls = llm_calls.lock().unwrap();
                *llm_calls += 1;
                *llm_calls
            };
            if call == 2 {
                *converted.lock().unwrap() = context.system_prompt.clone().unwrap_or_default();
            }
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            if call == 1 {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::ToolUse,
                    message: create_assistant_message(
                        vec![tool_call_block(
                            "tool-1",
                            "echo",
                            json!({ "value": "hello" }),
                        )],
                        StopReason::ToolUse,
                    ),
                });
            } else {
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(vec![text_content("done")], StopReason::Stop),
                });
            }
            Ok(stream)
        })
    };

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    assert_eq!(*llm_calls.lock().unwrap(), 2);
    assert_eq!(*prepare_calls.lock().unwrap(), 1);
    assert_eq!(
        *converted_second_turn_system_prompt.lock().unwrap(),
        "second prompt"
    );
}

#[tokio::test]
async fn stops_after_the_current_turn_when_should_stop_after_turn_returns_true() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        tools: Some(vec![echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )]),
        ..Default::default()
    };

    let steering_polls = Arc::new(Mutex::new(0));
    let follow_up_polls = Arc::new(Mutex::new(0));
    let callback_tool_result_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let callback_context_roles: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let config = AgentLoopConfig {
        get_steering_messages: Some({
            let steering_polls = Arc::clone(&steering_polls);
            Arc::new(move || {
                *steering_polls.lock().unwrap() += 1;
                Box::pin(async move { Vec::new() })
            })
        }),
        get_follow_up_messages: Some({
            let follow_up_polls = Arc::clone(&follow_up_polls);
            Arc::new(move || {
                *follow_up_polls.lock().unwrap() += 1;
                let messages = vec![create_user_message("follow up should stay queued")];
                Box::pin(async move { messages })
            })
        }),
        should_stop_after_turn: Some({
            let callback_tool_result_ids = Arc::clone(&callback_tool_result_ids);
            let callback_context_roles = Arc::clone(&callback_context_roles);
            Arc::new(move |turn| {
                *callback_tool_result_ids.lock().unwrap() = turn
                    .tool_results
                    .iter()
                    .map(|result| result.tool_call_id.clone())
                    .collect();
                *callback_context_roles.lock().unwrap() = turn
                    .context
                    .messages
                    .iter()
                    .map(|m| m.role().to_string())
                    .collect();
                Box::pin(async move { true })
            })
        }),
        ..base_config()
    };

    let (stream_fn, llm_calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![tool_call_block(
                    "tool-1",
                    "echo",
                    json!({ "value": "hello" }),
                )],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("should not run")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, messages) = run_collect(&stream).await;

    assert_eq!(*llm_calls.lock().unwrap(), 1);
    let executed_values = executed
        .lock()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(executed_values, ["hello"]);
    assert_eq!(*steering_polls.lock().unwrap(), 1);
    assert_eq!(*follow_up_polls.lock().unwrap(), 0);
    assert_eq!(*callback_tool_result_ids.lock().unwrap(), ["tool-1"]);
    assert_eq!(
        *callback_context_roles.lock().unwrap(),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(
        message_roles(&messages),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(
        events.iter().map(event_type).collect::<Vec<_>>(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn stops_after_a_tool_batch_when_every_tool_result_sets_terminate() {
    let parameters = object_schema(json!({ "value": { "type": "string" } }), &["value"]);
    let tool = AgentTool {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echo tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_tool_call_id, params, _signal, _on_update| {
            let value = params
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![BlockContent::Text(TextContent {
                        text: format!("echoed: {}", value.as_str().unwrap_or_default()),
                        ..Default::default()
                    })],
                    details: json!({ "value": value }),
                    terminate: Some(true),
                    ..Default::default()
                })
            })
        }),
    };
    let context = AgentContext {
        tools: Some(vec![tool]),
        ..Default::default()
    };
    let config = base_config();

    let (stream_fn, llm_calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![tool_call_block(
                "tool-1",
                "echo",
                json!({ "value": "hello" }),
            )],
            StopReason::ToolUse,
        )]
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (events, messages) = run_collect(&stream).await;

    assert_eq!(*llm_calls.lock().unwrap(), 1);
    assert_eq!(
        message_roles(&messages),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn stops_after_a_blocked_tool_call_when_before_tool_call_sets_terminate() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        tools: Some(vec![echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )]),
        ..Default::default()
    };
    let config = AgentLoopConfig {
        before_tool_call: Some(Arc::new(|_context, _signal| {
            Box::pin(async move {
                Some(BeforeToolCallResult {
                    block: Some(true),
                    reason: Some("Blocked by policy".to_string()),
                    terminate: Some(true),
                })
            })
        })),
        ..base_config()
    };

    let (stream_fn, llm_calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![tool_call_block(
                    "tool-1",
                    "echo",
                    json!({ "value": "hello" }),
                )],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("should not run")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (_events, messages) = run_collect(&stream).await;

    let tool_result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some((**result).clone()),
            _ => None,
        })
        .expect("tool result message");
    assert!(executed.lock().unwrap().is_empty());
    assert_eq!(*llm_calls.lock().unwrap(), 1);
    assert!(tool_result.is_error);
    assert!(tool_result.content.iter().any(
        |block| matches!(block, BlockContent::Text(text) if text.text == "Blocked by policy")
    ));
}

#[tokio::test]
async fn continues_after_a_mixed_batch_with_one_terminating_blocked_call() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        tools: Some(vec![echo_tool(
            object_schema(json!({ "value": { "type": "string" } }), &["value"]),
            Arc::clone(&executed),
        )]),
        ..Default::default()
    };
    let config = AgentLoopConfig {
        model: create_model(),
        convert_to_llm: Arc::new(identity_converter),
        tool_execution: Some(ToolExecutionMode::Parallel),
        before_tool_call: Some(Arc::new(|context, _signal| {
            let blocked = context
                .args
                .lock()
                .unwrap()
                .get("value")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .unwrap_or_default();
            let result = if blocked == "first" {
                Some(BeforeToolCallResult {
                    block: Some(true),
                    reason: Some("Blocked first".to_string()),
                    terminate: Some(true),
                })
            } else {
                None
            };
            Box::pin(async move { result })
        })),
        ..base_config()
    };

    let (stream_fn, llm_calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![
                    tool_call_block("tool-1", "echo", json!({ "value": "first" })),
                    tool_call_block("tool-2", "echo", json!({ "value": "second" })),
                ],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    let executed_values = executed
        .lock()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(executed_values, ["second"]);
    assert_eq!(*llm_calls.lock().unwrap(), 2);
}

#[tokio::test]
async fn continues_after_parallel_tool_calls_when_not_all_tool_results_terminate() {
    let parameters = object_schema(json!({ "value": { "type": "string" } }), &["value"]);
    let tool = AgentTool {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echo tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_tool_call_id, params, _signal, _on_update| {
            let value = params
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![BlockContent::Text(TextContent {
                        text: format!("echoed: {}", value.as_str().unwrap_or_default()),
                        ..Default::default()
                    })],
                    details: json!({ "value": value }),
                    terminate: Some(value == json!("first")),
                    ..Default::default()
                })
            })
        }),
    };
    let context = AgentContext {
        tools: Some(vec![tool]),
        ..Default::default()
    };
    let config = AgentLoopConfig {
        tool_execution: Some(ToolExecutionMode::Parallel),
        ..base_config()
    };

    let (stream_fn, calls) = scripted_stream_fn(|index| {
        if index == 0 {
            vec![create_assistant_message(
                vec![
                    tool_call_block("tool-1", "echo", json!({ "value": "first" })),
                    tool_call_block("tool-2", "echo", json!({ "value": "second" })),
                ],
                StopReason::ToolUse,
            )]
        } else {
            vec![create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )]
        }
    });

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let (_events, messages) = run_collect(&stream).await;

    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(
        message_roles(&messages),
        ["user", "assistant", "toolResult", "toolResult", "assistant"]
    );
}

#[tokio::test]
async fn allows_after_tool_call_to_mark_a_tool_batch_as_terminating() {
    let parameters = object_schema(json!({ "value": { "type": "string" } }), &["value"]);
    let tool = AgentTool {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echo tool".to_string(),
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_tool_call_id, params, _signal, _on_update| {
            let value = params
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![BlockContent::Text(TextContent {
                        text: format!("echoed: {}", value.as_str().unwrap_or_default()),
                        ..Default::default()
                    })],
                    details: json!({ "value": value }),
                    ..Default::default()
                })
            })
        }),
    };
    let context = AgentContext {
        tools: Some(vec![tool]),
        ..Default::default()
    };
    let config = AgentLoopConfig {
        after_tool_call: Some(Arc::new(|_context, _signal| {
            Box::pin(async move {
                Some(AfterToolCallResult {
                    terminate: Some(true),
                    ..Default::default()
                })
            })
        })),
        ..base_config()
    };

    let (stream_fn, llm_calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![tool_call_block(
                "tool-1",
                "echo",
                json!({ "value": "hello" }),
            )],
            StopReason::ToolUse,
        )]
    });

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    run_collect(&stream).await;

    assert_eq!(*llm_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn agent_loop_continue_throws_when_context_has_no_messages() {
    let config = base_config();
    let Err(error) = agent_loop_continue(AgentContext::default(), config, None, None) else {
        panic!("expected an error");
    };
    assert_eq!(error, "Cannot continue: no messages in context");
}

#[tokio::test]
async fn continues_from_existing_context_without_emitting_user_message_events() {
    let context = AgentContext {
        messages: vec![create_user_message("Hello")],
        ..Default::default()
    };
    let config = base_config();
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("Response")],
            StopReason::Stop,
        )]
    });

    let stream = agent_loop_continue(context, config, None, Some(stream_fn)).unwrap();
    let (events, messages) = run_collect(&stream).await;

    assert_eq!(message_roles(&messages), ["assistant"]);
    let message_end_events = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message } => Some(message.role().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(message_end_events, ["assistant"]);
}

#[tokio::test]
async fn allows_custom_message_types_as_last_message() {
    let custom = AgentMessage::Custom(pi_core::agent::types::CustomAgentMessage::new(
        "custom",
        json!({ "text": "Hook content", "timestamp": 0 }),
    ));
    let context = AgentContext {
        messages: vec![custom],
        ..Default::default()
    };
    let config = AgentLoopConfig {
        model: create_model(),
        convert_to_llm: Arc::new(move |messages| {
            Box::pin(async move {
                messages
                    .into_iter()
                    .map(|message| match message {
                        AgentMessage::User(user) => Message::User(user),
                        AgentMessage::Assistant(assistant) => Message::Assistant(assistant),
                        AgentMessage::ToolResult(tool_result) => Message::ToolResult(tool_result),
                        AgentMessage::Custom(custom) => {
                            Message::User(pi_core::ai::types::UserMessage {
                                role: RoleUser,
                                content: pi_core::ai::types::UserContent::Text(
                                    custom
                                        .value
                                        .get("text")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                ),
                                timestamp: 0,
                            })
                        }
                    })
                    .collect::<Vec<_>>()
            })
        }),
        ..base_config()
    };
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("Response to custom message")],
            StopReason::Stop,
        )]
    });

    let stream = agent_loop_continue(context, config, None, Some(stream_fn)).unwrap();
    let (_events, messages) = run_collect(&stream).await;
    assert_eq!(message_roles(&messages), ["assistant"]);
}

/// Mirrors the low-level `runAgentLoop`/`runAgentLoopContinue` signatures
/// used by the `Agent` wrapper (emit sink instead of an event stream).
fn sink() -> (
    Arc<Mutex<Vec<AgentEvent>>>,
    pi_core::agent::agent_loop::AgentEventSink,
) {
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let events = Arc::clone(&events);
        Arc::new(move |event: AgentEvent| {
            let events = Arc::clone(&events);
            Box::pin(async move {
                events.lock().unwrap().push(event);
            }) as BoxFuture<'static, ()>
        })
    };
    (events, sink)
}

#[tokio::test]
async fn run_agent_loop_helpers_drive_the_emit_sink() {
    let (events, sink) = sink();
    let config = base_config();
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("ok")],
            StopReason::Stop,
        )]
    });
    let messages = run_agent_loop(
        vec![create_user_message("Hello")],
        AgentContext::default(),
        config,
        Arc::clone(&sink),
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();
    assert_eq!(message_roles(&messages), ["user", "assistant"]);
    assert_eq!(events.lock().unwrap().len(), 8);

    let config = base_config();
    let (stream_fn, _calls) = scripted_stream_fn(|_| {
        vec![create_assistant_message(
            vec![text_content("again")],
            StopReason::Stop,
        )]
    });
    let messages = run_agent_loop_continue(
        AgentContext {
            messages: vec![create_user_message("Hello")],
            ..Default::default()
        },
        config,
        Arc::clone(&sink),
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();
    assert_eq!(message_roles(&messages), ["assistant"]);
}

#[tokio::test]
async fn run_agent_loop_continue_validates_context() {
    let config = base_config();
    let (_events, empty_sink) = sink();
    let result = run_agent_loop_continue(
        AgentContext::default(),
        config.clone(),
        empty_sink,
        None,
        None,
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        "Cannot continue: no messages in context"
    );

    let (_events, assistant_sink) = sink();
    let result = run_agent_loop_continue(
        AgentContext {
            messages: vec![AgentMessage::Assistant(Box::new(create_assistant_message(
                vec![text_content("done")],
                StopReason::Stop,
            )))],
            ..Default::default()
        },
        config,
        assistant_sink,
        None,
        None,
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        "Cannot continue from message role: assistant"
    );
}
