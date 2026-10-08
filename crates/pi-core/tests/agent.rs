//! Port of `pi-core/agent/test/agent.test.ts`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use pi_core::agent::agent::{
    Agent, AgentEventListener, AgentInitialState, AgentOptions, AgentPromptInput,
    AgentShouldStopAfterTurnFn, LegacyPrepareNextTurnFn,
};
use pi_core::agent::stream_fn::set_default_stream_fn;
use pi_core::agent::types::{
    AgentEvent, AgentLoopTurnUpdate, AgentMessage, AgentTool, AgentToolResult,
    AgentToolUpdateCallback, CustomAgentMessage, StreamFn, ThinkingLevel,
};
use pi_core::ai::compat::get_model;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent, DoneReason,
    ErrorReason, Model, RoleAssistant, RoleUser, SimpleStreamOptions, StopReason, TextContent,
    ToolCall, Usage,
};

fn create_usage() -> Usage {
    Usage::default()
}

fn create_assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "mock".to_string(),
        usage: create_usage(),
        stop_reason: StopReason::Stop,
        timestamp: 0,
        ..Default::default()
    }
}

fn create_assistant_tool_use_message(content: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: content
            .into_iter()
            .map(AssistantContent::ToolCall)
            .collect(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "mock".to_string(),
        usage: create_usage(),
        stop_reason: StopReason::ToolUse,
        timestamp: 0,
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    }
}

/// A stream function that replies with a scripted assistant message.
fn done_stream_fn(message: AssistantMessage) -> StreamFn {
    Arc::new(move |_model, _context, _options| {
        let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
        stream.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: message.clone(),
        });
        Ok(stream)
    })
}

fn unused_stream_fn() -> StreamFn {
    // The TypeScript mock throws when called; the Rust mock panics, but no
    // test in this suite ever invokes it.
    Arc::new(|_model, _context, _options| panic!("Unexpected stream call"))
}

fn noop_tool(name: &str) -> AgentTool {
    AgentTool {
        name: name.to_string(),
        label: name.to_string(),
        description: format!("{name} tool"),
        parameters: json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_tool_call_id, _params, _signal, _on_update| {
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![BlockContent::Text(TextContent {
                        text: "ok".to_string(),
                        ..Default::default()
                    })],
                    details: json!({}),
                    ..Default::default()
                })
            })
        }),
    }
}

fn event_collector() -> (Arc<Mutex<Vec<String>>>, AgentEventListener) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let events = Arc::clone(&events);
        Arc::new(move |event: &AgentEvent, _signal: &CancellationToken| {
            let events = Arc::clone(&events);
            let event_name = event_type(event).to_string();
            Box::pin(async move {
                events.lock().unwrap().push(event_name);
            }) as BoxFuture<'static, ()>
        }) as AgentEventListener
    };
    (events, listener)
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

fn message_roles(agent: &Agent) -> Vec<String> {
    agent
        .messages()
        .iter()
        .map(|message| message.role().to_string())
        .collect()
}

fn user_text_message(text: &str) -> AgentMessage {
    AgentMessage::User(pi_core::ai::types::UserMessage {
        role: RoleUser,
        content: pi_core::ai::types::UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

/// Barrier mirroring the TypeScript `createDeferred()` helper.
struct Deferred {
    notify: Arc<tokio::sync::Notify>,
}

impl Deferred {
    fn new() -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn resolve(&self) {
        self.notify.notify_one();
    }

    async fn wait(&self) {
        self.notify.notified().await;
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
                  _options: Option<&SimpleStreamOptions>| {
                *calls.lock().unwrap() += 1;
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message("fallback"),
                });
                Ok(stream)
            },
        )
    };
    set_default_stream_fn(Some(default_fn));

    let agent = Agent::new(AgentOptions::default());
    agent.prompt("Hello").await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
    set_default_stream_fn(None);
}

#[tokio::test]
async fn creates_an_agent_instance_with_default_state() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        ..Default::default()
    });

    assert_eq!(agent.system_prompt(), "");
    assert_eq!(agent.model().id, "unknown");
    assert_eq!(agent.thinking_level(), ThinkingLevel::Off);
    assert!(agent.tools().is_empty());
    assert!(agent.messages().is_empty());
    assert!(!agent.is_streaming());
    assert!(agent.streaming_message().is_none());
    assert!(agent.pending_tool_calls().is_empty());
    assert!(agent.error_message().is_none());

    let state = agent.state();
    assert_eq!(state.system_prompt, "");
    assert_eq!(state.model.id, "unknown");
    assert_eq!(state.thinking_level, ThinkingLevel::Off);
    assert!(state.tools.is_empty());
    assert!(state.messages.is_empty());
    assert!(!state.is_streaming);
    assert!(state.streaming_message.is_none());
    assert!(state.pending_tool_calls.is_empty());
    assert!(state.error_message.is_none());
}

#[tokio::test]
async fn creates_an_agent_instance_with_custom_initial_state() {
    let custom_model = get_model("openai", "gpt-4o-mini").expect("catalog model");
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            model: Some(custom_model.clone()),
            thinking_level: Some(ThinkingLevel::Low),
            ..Default::default()
        }),
        ..Default::default()
    });

    assert_eq!(agent.system_prompt(), "You are a helpful assistant.");
    assert_eq!(agent.model(), custom_model);
    assert_eq!(agent.thinking_level(), ThinkingLevel::Low);
}

#[tokio::test]
async fn subscribes_to_events() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        ..Default::default()
    });

    let event_count = Arc::new(Mutex::new(0));
    let subscription = agent.subscribe({
        let event_count = Arc::clone(&event_count);
        Arc::new(move |_event: &AgentEvent, _signal: &CancellationToken| {
            let event_count = Arc::clone(&event_count);
            Box::pin(async move {
                *event_count.lock().unwrap() += 1;
            }) as BoxFuture<'static, ()>
        }) as AgentEventListener
    });

    assert_eq!(*event_count.lock().unwrap(), 0);

    // State mutators don't emit events.
    agent.set_system_prompt("Test prompt");
    assert_eq!(*event_count.lock().unwrap(), 0);
    assert_eq!(agent.system_prompt(), "Test prompt");

    subscription.unsubscribe();
    agent.set_system_prompt("Another prompt");
    assert_eq!(*event_count.lock().unwrap(), 0);
}

#[tokio::test]
async fn emits_full_lifecycle_events_for_thrown_run_failures() {
    let failing: StreamFn =
        Arc::new(|_model, _context, _options| Err("provider exploded".to_string()));
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(failing),
        ..Default::default()
    });
    let (events, _subscription) = {
        let (events, listener) = event_collector();
        let subscription = agent.subscribe(listener);
        (events, subscription)
    };

    agent.prompt("hello").await.unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    let messages = agent.messages();
    let last_message = messages.last().expect("messages");
    let AgentMessage::Assistant(last_message) = last_message else {
        panic!("expected assistant message");
    };
    assert_eq!(last_message.stop_reason, StopReason::Error);
    assert_eq!(
        last_message.error_message.as_deref(),
        Some("provider exploded")
    );
    assert_eq!(agent.error_message().as_deref(), Some("provider exploded"));
}

#[tokio::test]
async fn awaits_async_subscribers_before_prompt_resolves() {
    let barrier = Arc::new(Deferred::new());
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(done_stream_fn(create_assistant_message("ok"))),
        ..Default::default()
    });

    let listener_finished = Arc::new(Mutex::new(false));
    agent.subscribe({
        let barrier = Arc::clone(&barrier);
        let listener_finished = Arc::clone(&listener_finished);
        Arc::new(move |event: &AgentEvent, _signal: &CancellationToken| {
            let barrier = Arc::clone(&barrier);
            let listener_finished = Arc::clone(&listener_finished);
            let is_agent_end = matches!(event, AgentEvent::AgentEnd { .. });
            Box::pin(async move {
                if is_agent_end {
                    barrier.wait().await;
                    *listener_finished.lock().unwrap() = true;
                }
            }) as BoxFuture<'static, ()>
        }) as AgentEventListener
    });

    let prompt_resolved = Arc::new(Mutex::new(false));
    let agent = Arc::new(agent);
    let prompt_agent = Arc::clone(&agent);
    let prompt_resolved_clone = Arc::clone(&prompt_resolved);
    let prompt_promise = tokio::spawn(async move {
        prompt_agent.prompt("hello").await.unwrap();
        *prompt_resolved_clone.lock().unwrap() = true;
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!*prompt_resolved.lock().unwrap());
    assert!(!*listener_finished.lock().unwrap());
    assert!(agent.is_streaming());

    barrier.resolve();
    prompt_promise.await.unwrap();

    assert!(*listener_finished.lock().unwrap());
    assert!(*prompt_resolved.lock().unwrap());
    assert!(!agent.is_streaming());
}

#[tokio::test]
async fn wait_for_idle_waits_for_async_subscribers() {
    let barrier = Arc::new(Deferred::new());
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(done_stream_fn(create_assistant_message("ok"))),
        ..Default::default()
    });

    agent.subscribe({
        let barrier = Arc::clone(&barrier);
        Arc::new(
            move |event: &AgentEvent, _signal: &CancellationToken| {
                let barrier = Arc::clone(&barrier);
                let is_message_end = matches!(
                    event,
                    AgentEvent::MessageEnd { message } if matches!(&**message, AgentMessage::Assistant(_))
                );
                Box::pin(async move {
                    if is_message_end {
                        barrier.wait().await;
                    }
                }) as BoxFuture<'static, ()>
            },
        ) as AgentEventListener
    });

    let agent = Arc::new(agent);
    let prompt_agent = Arc::clone(&agent);
    let prompt_promise = tokio::spawn(async move {
        prompt_agent.prompt("hello").await.unwrap();
    });
    let idle_agent = Arc::clone(&agent);
    let idle_resolved = Arc::new(Mutex::new(false));
    let idle_resolved_clone = Arc::clone(&idle_resolved);
    let idle_promise = tokio::spawn(async move {
        idle_agent.wait_for_idle().await;
        *idle_resolved_clone.lock().unwrap() = true;
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!*idle_resolved.lock().unwrap());
    assert!(agent.is_streaming());

    barrier.resolve();
    let (prompt_result, idle_result) = tokio::join!(prompt_promise, idle_promise);
    prompt_result.unwrap();
    idle_result.unwrap();

    assert!(*idle_resolved.lock().unwrap());
    assert!(!agent.is_streaming());
}

/// A stream function that keeps streaming until the request signal is
/// aborted (the `checkAbort` polling mock).
fn abortable_stream_fn() -> StreamFn {
    Arc::new(
        move |_model: &Model,
              _context: &pi_core::ai::types::Context,
              options: Option<&SimpleStreamOptions>| {
            let signal = options.and_then(|options| options.base.base.signal.clone());
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Start {
                partial: create_assistant_message(""),
            });
            let poll_stream = stream.clone();
            tokio::spawn(async move {
                while !poll_stream.is_done() {
                    if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                        poll_stream.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Aborted,
                            error: create_assistant_message("Aborted"),
                        });
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            Ok(stream)
        },
    )
}

#[tokio::test]
async fn passes_the_active_abort_signal_to_subscribers() {
    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(abortable_stream_fn()),
        ..Default::default()
    }));

    let received_signal = Arc::new(Mutex::new(None::<CancellationToken>));
    agent.subscribe({
        let received_signal = Arc::clone(&received_signal);
        Arc::new(move |event: &AgentEvent, signal: &CancellationToken| {
            let received_signal = Arc::clone(&received_signal);
            let signal = signal.clone();
            let is_agent_start = matches!(event, AgentEvent::AgentStart);
            Box::pin(async move {
                if is_agent_start {
                    *received_signal.lock().unwrap() = Some(signal);
                }
            }) as BoxFuture<'static, ()>
        }) as AgentEventListener
    });

    let prompt_agent = Arc::clone(&agent);
    let prompt_promise = tokio::spawn(async move {
        prompt_agent.prompt("hello").await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let signal = received_signal
        .lock()
        .unwrap()
        .clone()
        .expect("signal received");
    assert!(!signal.is_cancelled());

    agent.abort();
    prompt_promise.await.unwrap();

    assert!(signal.is_cancelled());
}

#[tokio::test]
async fn ignores_tool_updates_after_the_tool_execution_settles() {
    let delayed_update: Arc<Mutex<Option<AgentToolUpdateCallback>>> = Arc::new(Mutex::new(None));

    let tool = AgentTool {
        name: "delayed_tool".to_string(),
        label: "Delayed Tool".to_string(),
        description: "Captures progress callbacks".to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: {
            let delayed_update = Arc::clone(&delayed_update);
            Arc::new(move |_tool_call_id, _params, _signal, on_update| {
                let delayed_update = Arc::clone(&delayed_update);
                let on_update = on_update.cloned();
                Box::pin(async move {
                    if let Some(on_update) = on_update {
                        *delayed_update.lock().unwrap() = Some(Arc::clone(&on_update));
                        on_update(AgentToolResult {
                            content: vec![BlockContent::Text(TextContent {
                                text: "running".to_string(),
                                ..Default::default()
                            })],
                            details: json!({ "status": "running" }),
                            ..Default::default()
                        });
                    }
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: "ok".to_string(),
                            ..Default::default()
                        })],
                        details: json!({ "status": "done" }),
                        terminate: Some(true),
                        ..Default::default()
                    })
                })
            })
        },
    };

    let agent = Agent::new(AgentOptions {
        initial_state: Some(AgentInitialState {
            tools: Some(vec![tool]),
            ..Default::default()
        }),
        stream_fn: Some(done_stream_fn(create_assistant_tool_use_message(vec![
            tool_call("call-1", "delayed_tool", json!({})),
        ]))),
        ..Default::default()
    });
    let (typed_events, listener) = event_collector();
    let _subscription = agent.subscribe(listener);

    agent.prompt("run tool").await.unwrap();
    let event_count_after_prompt = typed_events.lock().unwrap().len();

    let on_update = delayed_update
        .lock()
        .unwrap()
        .clone()
        .expect("update callback");
    on_update(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: "late".to_string(),
            ..Default::default()
        })],
        details: json!({ "status": "late" }),
        ..Default::default()
    });
    tokio::time::sleep(Duration::from_millis(1)).await;

    let events = typed_events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "tool_execution_update")
            .count(),
        1
    );
    assert_eq!(events.len(), event_count_after_prompt);
    drop(events);
}

#[tokio::test]
async fn ignores_a_settled_parallel_tool_update_while_another_tool_runs() {
    let slow_started = Arc::new(Deferred::new());
    let settled_tool_ended = Arc::new(Deferred::new());
    let release_slow = Arc::new(Deferred::new());
    let settled_tool_update: Arc<Mutex<Option<AgentToolUpdateCallback>>> =
        Arc::new(Mutex::new(None));

    let settled_tool = AgentTool {
        name: "settled_tool".to_string(),
        label: "Settled Tool".to_string(),
        description: "Captures progress callbacks".to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: {
            let settled_tool_update = Arc::clone(&settled_tool_update);
            Arc::new(move |_tool_call_id, _params, _signal, on_update| {
                let settled_tool_update = Arc::clone(&settled_tool_update);
                let on_update = on_update.cloned();
                Box::pin(async move {
                    if let Some(on_update) = on_update {
                        *settled_tool_update.lock().unwrap() = Some(Arc::clone(&on_update));
                    }
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: "done".to_string(),
                            ..Default::default()
                        })],
                        details: json!({ "status": "done" }),
                        terminate: Some(true),
                        ..Default::default()
                    })
                })
            })
        },
    };
    let slow_tool = AgentTool {
        name: "slow_tool".to_string(),
        label: "Slow Tool".to_string(),
        description: "Keeps the agent run active".to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: {
            let slow_started = Arc::clone(&slow_started);
            let release_slow = Arc::clone(&release_slow);
            Arc::new(move |_tool_call_id, _params, _signal, _on_update| {
                let slow_started = Arc::clone(&slow_started);
                let release_slow = Arc::clone(&release_slow);
                Box::pin(async move {
                    slow_started.resolve();
                    release_slow.wait().await;
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: "done".to_string(),
                            ..Default::default()
                        })],
                        details: json!({ "status": "done" }),
                        terminate: Some(true),
                        ..Default::default()
                    })
                })
            })
        },
    };

    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: Some(AgentInitialState {
            tools: Some(vec![settled_tool, slow_tool]),
            ..Default::default()
        }),
        stream_fn: Some(done_stream_fn(create_assistant_tool_use_message(vec![
            tool_call("call-1", "settled_tool", json!({})),
            tool_call("call-2", "slow_tool", json!({})),
        ]))),
        ..Default::default()
    }));

    let settled_tool_ended_flag = Arc::clone(&settled_tool_ended);
    let (typed_events, _) = event_collector();
    let listener: AgentEventListener = {
        let settled_tool_ended_flag = Arc::clone(&settled_tool_ended_flag);
        let captured_events = Arc::clone(&typed_events);
        Arc::new(move |event: &AgentEvent, _signal: &CancellationToken| {
            let typed_events = Arc::clone(&captured_events);
            let settled_tool_ended_flag = Arc::clone(&settled_tool_ended_flag);
            let event_name = event_type(event).to_string();
            let is_call_1_end = matches!(
                event,
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call-1"
            );
            Box::pin(async move {
                typed_events.lock().unwrap().push(event_name);
                if is_call_1_end {
                    settled_tool_ended_flag.resolve();
                }
            }) as BoxFuture<'static, ()>
        })
    };
    agent.subscribe(listener);

    let prompt_agent = Arc::clone(&agent);
    let prompt_promise = tokio::spawn(async move {
        prompt_agent.prompt("run tools").await.unwrap();
    });
    let started = Arc::clone(&slow_started);
    let ended = Arc::clone(&settled_tool_ended);
    let both = tokio::spawn(async move {
        started.wait().await;
        ended.wait().await;
    });
    both.await.unwrap();
    let event_count_before_late_update = typed_events.lock().unwrap().len();

    let on_update = settled_tool_update
        .lock()
        .unwrap()
        .clone()
        .expect("update callback");
    on_update(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: "late".to_string(),
            ..Default::default()
        })],
        details: json!({ "status": "late" }),
        ..Default::default()
    });
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        typed_events.lock().unwrap().len(),
        event_count_before_late_update
    );

    release_slow.resolve();
    prompt_promise.await.unwrap();
    assert_eq!(
        typed_events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "tool_execution_update")
            .count(),
        0
    );
}

#[tokio::test]
async fn updates_state_with_mutators() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        ..Default::default()
    });

    agent.set_system_prompt("Custom prompt");
    assert_eq!(agent.system_prompt(), "Custom prompt");

    let new_model = get_model("google", "gemini-2.5-flash").expect("catalog model");
    agent.set_model(new_model.clone());
    assert_eq!(agent.model(), new_model);

    agent.set_thinking_level(ThinkingLevel::High);
    assert_eq!(agent.thinking_level(), ThinkingLevel::High);

    let tools = vec![noop_tool("test")];
    agent.set_tools(tools.clone());
    assert_eq!(agent.tools().len(), tools.len());
    assert_eq!(agent.tools()[0].name, "test");

    agent.set_messages(vec![user_text_message("Hello")]);
    assert_eq!(agent.messages().len(), 1);
    agent.push_message(AgentMessage::Assistant(Box::new(create_assistant_message(
        "Hi",
    ))));
    assert_eq!(agent.messages().len(), 2);
    assert_eq!(agent.messages()[1].role(), "assistant");

    agent.set_messages(Vec::new());
    assert!(agent.messages().is_empty());
}

#[tokio::test]
async fn supports_steering_message_queue() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        ..Default::default()
    });

    let message = user_text_message("Steering message");
    agent.steer(message.clone());

    let messages = agent.messages();
    assert!(!messages.contains(&message));
    assert!(agent.has_queued_messages());
}

#[tokio::test]
async fn supports_follow_up_message_queue() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        ..Default::default()
    });

    let message = user_text_message("Follow-up message");
    agent.follow_up(message.clone());

    assert!(!agent.messages().contains(&message));
    assert!(agent.has_queued_messages());
}

#[tokio::test]
async fn handles_abort_controller() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(unused_stream_fn()),
        ..Default::default()
    });
    // Should not panic even if nothing is running.
    agent.abort();
}

#[tokio::test]
async fn rejects_reset_while_processing_without_corrupting_the_transcript() {
    let stream_started = Arc::new(Deferred::new());
    let release_response = Arc::new(Deferred::new());

    let stream_fn: StreamFn = {
        let stream_started = Arc::clone(&stream_started);
        let release_response = Arc::clone(&release_response);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  _options: Option<&SimpleStreamOptions>| {
                let stream_started = Arc::clone(&stream_started);
                let release_response = Arc::clone(&release_response);
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Start {
                    partial: create_assistant_message(""),
                });
                let push_stream = stream.clone();
                tokio::spawn(async move {
                    stream_started.resolve();
                    release_response.wait().await;
                    push_stream.push(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message: create_assistant_message("Done"),
                    });
                });
                Ok(stream)
            },
        )
    };

    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(stream_fn),
        ..Default::default()
    }));

    let prompt_agent = Arc::clone(&agent);
    let prompt_promise = tokio::spawn(async move {
        prompt_agent.prompt("Hello").await.unwrap();
    });
    stream_started.wait().await;

    assert!(agent.is_streaming());
    assert_eq!(message_roles(&agent), ["user"]);
    assert_eq!(
        agent.reset().unwrap_err(),
        "Agent is already processing. Wait for completion before resetting."
    );
    assert!(agent.is_streaming());
    assert_eq!(message_roles(&agent), ["user"]);

    release_response.resolve();
    prompt_promise.await.unwrap();

    assert!(!agent.is_streaming());
    assert_eq!(message_roles(&agent), ["user", "assistant"]);
}

#[tokio::test]
async fn throws_when_prompt_called_while_streaming() {
    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(abortable_stream_fn()),
        ..Default::default()
    }));

    let prompt_agent = Arc::clone(&agent);
    let first_prompt = tokio::spawn(async move {
        prompt_agent.prompt("First message").await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(agent.is_streaming());

    let error = agent.prompt("Second message").await.unwrap_err();
    assert_eq!(
        error,
        "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
    );

    agent.abort();
    first_prompt.await.unwrap();
}

#[tokio::test]
async fn throws_when_continue_called_while_streaming() {
    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(abortable_stream_fn()),
        ..Default::default()
    }));

    let prompt_agent = Arc::clone(&agent);
    let first_prompt = tokio::spawn(async move {
        prompt_agent.prompt("First message").await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(agent.is_streaming());

    let error = agent.continue_().await.unwrap_err();
    assert_eq!(
        error,
        "Agent is already processing. Wait for completion before continuing."
    );

    agent.abort();
    first_prompt.await.unwrap();
}

#[tokio::test]
async fn continue_processes_queued_follow_up_messages_after_an_assistant_turn() {
    let response_count = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let response_count = Arc::clone(&response_count);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  _options: Option<&SimpleStreamOptions>| {
                let count = {
                    let mut response_count = response_count.lock().unwrap();
                    *response_count += 1;
                    *response_count
                };
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(&format!("Processed {count}")),
                });
                Ok(stream)
            },
        )
    };

    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    agent.set_messages(vec![
        AgentMessage::User(pi_core::ai::types::UserMessage {
            role: RoleUser,
            content: pi_core::ai::types::UserContent::Blocks(vec![BlockContent::Text(
                TextContent {
                    text: "Initial".to_string(),
                    ..Default::default()
                },
            )]),
            timestamp: 0,
        }),
        AgentMessage::Assistant(Box::new(create_assistant_message("Initial response"))),
    ]);

    agent.follow_up(AgentMessage::User(pi_core::ai::types::UserMessage {
        role: RoleUser,
        content: pi_core::ai::types::UserContent::Blocks(vec![BlockContent::Text(TextContent {
            text: "Queued follow-up".to_string(),
            ..Default::default()
        })]),
        timestamp: 0,
    }));

    agent.continue_().await.unwrap();

    let messages = agent.messages();
    let has_queued_follow_up = messages.iter().any(|message| {
        match message {
        AgentMessage::User(user) => match &user.content {
            pi_core::ai::types::UserContent::Text(text) => text == "Queued follow-up",
            pi_core::ai::types::UserContent::Blocks(blocks) => blocks.iter().any(|block| {
                matches!(block, BlockContent::Text(text) if text.text == "Queued follow-up")
            }),
        },
        _ => false,
    }
    });
    assert!(has_queued_follow_up);
    assert_eq!(messages.last().unwrap().role(), "assistant");
}

#[tokio::test]
async fn continue_keeps_one_at_a_time_steering_semantics_from_assistant_tail() {
    let response_count = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let response_count = Arc::clone(&response_count);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  _options: Option<&SimpleStreamOptions>| {
                let count = {
                    let mut response_count = response_count.lock().unwrap();
                    *response_count += 1;
                    *response_count
                };
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message(&format!("Processed {count}")),
                });
                Ok(stream)
            },
        )
    };

    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    agent.set_messages(vec![
        AgentMessage::User(pi_core::ai::types::UserMessage {
            role: RoleUser,
            content: pi_core::ai::types::UserContent::Blocks(vec![BlockContent::Text(
                TextContent {
                    text: "Initial".to_string(),
                    ..Default::default()
                },
            )]),
            timestamp: 0,
        }),
        AgentMessage::Assistant(Box::new(create_assistant_message("Initial response"))),
    ]);

    for text in ["Steering 1", "Steering 2"] {
        agent.steer(AgentMessage::User(pi_core::ai::types::UserMessage {
            role: RoleUser,
            content: pi_core::ai::types::UserContent::Blocks(vec![BlockContent::Text(
                TextContent {
                    text: text.to_string(),
                    ..Default::default()
                },
            )]),
            timestamp: 0,
        }));
    }

    agent.continue_().await.unwrap();

    let messages = agent.messages();
    let recent: Vec<String> = messages
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|message| message.role().to_string())
        .collect();
    assert_eq!(recent, ["user", "assistant", "user", "assistant"]);
    assert_eq!(*response_count.lock().unwrap(), 2);
}

#[tokio::test]
async fn keeps_legacy_prepare_next_turn_signal_callback_behavior() {
    let saw_abort_signal = Arc::new(Mutex::new(false));
    let prepare_next_turn: LegacyPrepareNextTurnFn = {
        let saw_abort_signal = Arc::clone(&saw_abort_signal);
        Arc::new(move |signal| {
            *saw_abort_signal.lock().unwrap() = signal.is_some();
            Box::pin(async move { None })
        })
    };

    let request_count = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let request_count = Arc::clone(&request_count);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  _options: Option<&SimpleStreamOptions>| {
                let count = {
                    let mut request_count = request_count.lock().unwrap();
                    *request_count += 1;
                    *request_count
                };
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                if count == 1 {
                    stream.push(AssistantMessageEvent::Done {
                        reason: DoneReason::ToolUse,
                        message: create_assistant_tool_use_message(vec![tool_call(
                            "tool-1",
                            "noop",
                            json!({}),
                        )]),
                    });
                } else {
                    stream.push(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message: create_assistant_message("done"),
                    });
                }
                Ok(stream)
            },
        )
    };

    let agent = Agent::new(AgentOptions {
        initial_state: Some(AgentInitialState {
            tools: Some(vec![noop_tool("noop")]),
            ..Default::default()
        }),
        prepare_next_turn: Some(prepare_next_turn),
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    agent.prompt("start").await.unwrap();

    assert_eq!(*request_count.lock().unwrap(), 2);
    assert!(*saw_abort_signal.lock().unwrap());
}

#[tokio::test]
async fn forwards_should_stop_after_turn_through_agent_options() {
    let saw_abort_signal = Arc::new(Mutex::new(false));
    let callback_context_roles: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let should_stop: AgentShouldStopAfterTurnFn = {
        let saw_abort_signal = Arc::clone(&saw_abort_signal);
        let callback_context_roles = Arc::clone(&callback_context_roles);
        Arc::new(move |context, signal| {
            *saw_abort_signal.lock().unwrap() = signal.is_some();
            *callback_context_roles.lock().unwrap() = context
                .context
                .messages
                .iter()
                .map(|message| message.role().to_string())
                .collect();
            Box::pin(async move { true })
        })
    };

    let request_count = Arc::new(Mutex::new(0));
    let stream_fn: StreamFn = {
        let request_count = Arc::clone(&request_count);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  _options: Option<&SimpleStreamOptions>| {
                let count = {
                    let mut request_count = request_count.lock().unwrap();
                    *request_count += 1;
                    *request_count
                };
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                if count == 1 {
                    stream.push(AssistantMessageEvent::Done {
                        reason: DoneReason::ToolUse,
                        message: create_assistant_tool_use_message(vec![tool_call(
                            "tool-1",
                            "noop",
                            json!({}),
                        )]),
                    });
                } else {
                    stream.push(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message: create_assistant_message("should not run"),
                    });
                }
                Ok(stream)
            },
        )
    };

    let agent = Agent::new(AgentOptions {
        initial_state: Some(AgentInitialState {
            tools: Some(vec![noop_tool("noop")]),
            ..Default::default()
        }),
        should_stop_after_turn: Some(should_stop),
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    agent.prompt("start").await.unwrap();

    assert_eq!(*request_count.lock().unwrap(), 1);
    assert!(*saw_abort_signal.lock().unwrap());
    assert_eq!(
        *callback_context_roles.lock().unwrap(),
        ["user", "assistant", "toolResult"]
    );
}

#[tokio::test]
async fn forwards_session_id_to_stream_function_options() {
    let received_session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stream_fn: StreamFn = {
        let received_session_id = Arc::clone(&received_session_id);
        Arc::new(
            move |_model: &Model,
                  _context: &pi_core::ai::types::Context,
                  options: Option<&SimpleStreamOptions>| {
                *received_session_id.lock().unwrap() =
                    options.and_then(|options| options.base.session_id.clone());
                let stream =
                    pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: create_assistant_message("ok"),
                });
                Ok(stream)
            },
        )
    };

    let agent = Agent::new(AgentOptions {
        session_id: Some("session-abc".to_string()),
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    agent.prompt("hello").await.unwrap();
    assert_eq!(
        received_session_id.lock().unwrap().as_deref(),
        Some("session-abc")
    );

    agent.set_session_id(Some("session-def".to_string()));
    assert_eq!(agent.session_id().as_deref(), Some("session-def"));

    agent.prompt("hello again").await.unwrap();
    assert_eq!(
        received_session_id.lock().unwrap().as_deref(),
        Some("session-def")
    );
}

#[tokio::test]
async fn normalizes_prompt_input_variants() {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(done_stream_fn(create_assistant_message("ok"))),
        ..Default::default()
    });
    agent.prompt("plain text").await.unwrap();
    assert_eq!(message_roles(&agent), ["user", "assistant"]);

    agent.set_messages(Vec::new());
    agent.prompt(user_text_message("single")).await.unwrap();
    assert_eq!(message_roles(&agent), ["user", "assistant"]);

    agent.set_messages(Vec::new());
    agent
        .prompt(AgentPromptInput::Messages(vec![user_text_message("batch")]))
        .await
        .unwrap();
    assert_eq!(message_roles(&agent), ["user", "assistant"]);

    // Custom messages ride through untouched and are filtered by the
    // default convertToLlm.
    agent.set_messages(Vec::new());
    agent
        .prompt(AgentMessage::Custom(CustomAgentMessage::new(
            "notification",
            json!({ "text": "note", "timestamp": 0 }),
        )))
        .await
        .unwrap();
    assert_eq!(message_roles(&agent), ["notification", "assistant"]);
}

#[tokio::test]
async fn agent_loop_turn_update_carries_model_and_thinking_level() {
    // Exercises the AgentLoopTurnUpdate plumbing through the public type.
    let update = AgentLoopTurnUpdate::default();
    assert!(update.context.is_none());
    assert!(update.model.is_none());
    assert!(update.thinking_level.is_none());
}
