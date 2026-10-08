//! Port of `pi-core/agent/src/agent-loop.ts`.
//!
//! The loop works with [`AgentMessage`] throughout and transforms to LLM
//! messages only at the provider-call boundary. Callback hooks are
//! infallible, matching the TypeScript callback types (a hook that must
//! fail should encode the failure in its return value); a failing
//! [`StreamFn`] is the Rust analog of a throwing stream function and
//! escapes the loop as an `Err`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, join_all};
use tokio_util::sync::CancellationToken;

use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent, Context, Message,
    StopReason, ToolCall, ToolResultMessage,
};
use crate::ai::utils::event_stream::{AssistantMessageEventStream, EventStream};
use crate::ai::utils::validation::validate_tool_arguments;
use crate::telemetry::lock;

use super::stream_fn::get_default_stream_fn;
use super::types::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, AgentToolCall,
    AgentToolResult, PrepareNextTurnContext, StreamFn, now_millis,
};

/// Port of `AgentEventSink`.
pub type AgentEventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;

/// Port of `agentLoop`: start an agent loop with new prompt messages.
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> EventStream<AgentEvent, Vec<AgentMessage>> {
    let stream = create_agent_stream();
    let sink_stream = stream.clone();
    let end_stream = stream.clone();
    tokio::spawn(async move {
        let emit: AgentEventSink = Arc::new(move |event: AgentEvent| {
            let stream = sink_stream.clone();
            Box::pin(async move {
                stream.push(event);
            }) as BoxFuture<'static, ()>
        });
        // A rejected run leaves the stream dangling, like the TypeScript
        // promise chain that never reaches `stream.end`.
        if let Ok(messages) =
            run_agent_loop(prompts, context, config, emit, signal, stream_fn).await
        {
            end_stream.end(Some(messages));
        }
    });
    stream
}

/// Port of `agentLoopContinue`: continue from the current context without
/// adding a new message.
///
/// The synchronous TypeScript throws for invalid contexts become the
/// returned error.
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<EventStream<AgentEvent, Vec<AgentMessage>>, String> {
    if context.messages.is_empty() {
        return Err("Cannot continue: no messages in context".to_string());
    }
    if context
        .messages
        .last()
        .is_some_and(|message| message.role() == "assistant")
    {
        return Err("Cannot continue from message role: assistant".to_string());
    }

    let stream = create_agent_stream();
    let sink_stream = stream.clone();
    let end_stream = stream.clone();
    tokio::spawn(async move {
        let emit: AgentEventSink = Arc::new(move |event: AgentEvent| {
            let stream = sink_stream.clone();
            Box::pin(async move {
                stream.push(event);
            }) as BoxFuture<'static, ()>
        });
        if let Ok(messages) =
            run_agent_loop_continue(context, config, emit, signal, stream_fn).await
        {
            end_stream.end(Some(messages));
        }
    });
    Ok(stream)
}

/// Port of `runAgentLoop`.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Vec<AgentMessage>, String> {
    let mut new_messages = prompts.clone();
    let mut current_context = context;
    current_context.messages.append(&mut prompts.clone());

    (emit)(AgentEvent::AgentStart).await;
    (emit)(AgentEvent::TurnStart).await;
    for prompt in &new_messages {
        (emit)(AgentEvent::MessageStart {
            message: Box::new(prompt.clone()),
        })
        .await;
        (emit)(AgentEvent::MessageEnd {
            message: Box::new(prompt.clone()),
        })
        .await;
    }

    run_loop(
        current_context,
        &mut new_messages,
        config,
        signal,
        &emit,
        stream_fn,
    )
    .await?;
    Ok(new_messages)
}

/// Port of `runAgentLoopContinue`.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
) -> Result<Vec<AgentMessage>, String> {
    if context.messages.is_empty() {
        return Err("Cannot continue: no messages in context".to_string());
    }
    if context
        .messages
        .last()
        .is_some_and(|message| message.role() == "assistant")
    {
        return Err("Cannot continue from message role: assistant".to_string());
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();

    (emit)(AgentEvent::AgentStart).await;
    (emit)(AgentEvent::TurnStart).await;

    run_loop(context, &mut new_messages, config, signal, &emit, stream_fn).await?;
    Ok(new_messages)
}

fn create_agent_stream() -> EventStream<AgentEvent, Vec<AgentMessage>> {
    EventStream::new(
        |event: &AgentEvent| matches!(event, AgentEvent::AgentEnd { .. }),
        |event: AgentEvent| match event {
            AgentEvent::AgentEnd { messages } => messages,
            _ => Vec::new(),
        },
    )
}

/// Main loop logic shared by `agentLoop` and `agentLoopContinue`.
async fn run_loop(
    initial_context: AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    initial_config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: Option<StreamFn>,
) -> Result<(), String> {
    let stream_fn = match stream_fn {
        Some(stream_fn) => stream_fn,
        None => get_default_stream_fn()?,
    };

    let mut current_context = initial_context;
    let mut config = initial_config;
    let mut last_completed_turn: Option<PrepareNextTurnContext> = None;
    // Check for steering messages at start (user may have typed while waiting)
    let mut pending_messages = match &config.get_steering_messages {
        Some(get) => (get)().await,
        None => Vec::new(),
    };

    // Outer loop: continues when queued follow-up messages arrive after the
    // agent would stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if let Some(last_turn) = &last_completed_turn {
                if let Some(prepare) = &config.prepare_next_turn
                    && let Some(snapshot) = (prepare)(last_turn.clone()).await
                {
                    if let Some(context) = snapshot.context {
                        current_context = context;
                    }
                    if let Some(model) = snapshot.model {
                        config.model = model;
                    }
                    if let Some(thinking_level) = snapshot.thinking_level {
                        config.stream_options.reasoning = thinking_level.as_provider_reasoning();
                    }
                }
                // Preparation can be long-running (for example, compaction).
                // Pick up steering queued while it ran. Only poll again if the
                // earlier poll returned nothing; otherwise one-at-a-time mode
                // would deliver two messages in this turn.
                if pending_messages.is_empty()
                    && let Some(get) = &config.get_steering_messages
                {
                    pending_messages = (get)().await;
                }
                (emit)(AgentEvent::TurnStart).await;
            }

            // Process pending messages (inject before next assistant response).
            for message in pending_messages.drain(..) {
                (emit)(AgentEvent::MessageStart {
                    message: Box::new(message.clone()),
                })
                .await;
                (emit)(AgentEvent::MessageEnd {
                    message: Box::new(message.clone()),
                })
                .await;
                current_context.messages.push(message.clone());
                new_messages.push(message);
            }

            // Stream assistant response.
            let message = stream_assistant_response(
                &mut current_context,
                &config,
                signal.clone(),
                emit,
                &stream_fn,
            )
            .await?;
            new_messages.push(AgentMessage::Assistant(Box::new(message.clone())));

            if message.stop_reason == StopReason::Error
                || message.stop_reason == StopReason::Aborted
            {
                (emit)(AgentEvent::TurnEnd {
                    message: Box::new(AgentMessage::Assistant(Box::new(message))),
                    tool_results: Vec::new(),
                })
                .await;
                (emit)(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(());
            }

            // Check for tool calls.
            let tool_calls: Vec<AgentToolCall> = message
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                // A "length" stop means the output was cut off by the token
                // limit, so every tool call in the message may carry truncated
                // arguments. Fail them all instead of executing potentially
                // borked calls.
                let executed_batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, emit).await
                } else {
                    execute_tool_calls(&current_context, &message, &config, signal.clone(), emit)
                        .await
                };
                tool_results.extend(executed_batch.messages);
                has_more_tool_calls = !executed_batch.terminate;

                for result in &tool_results {
                    current_context
                        .messages
                        .push(AgentMessage::ToolResult(Box::new(result.clone())));
                    new_messages.push(AgentMessage::ToolResult(Box::new(result.clone())));
                }
            }

            (emit)(AgentEvent::TurnEnd {
                message: Box::new(AgentMessage::Assistant(Box::new(message.clone()))),
                tool_results: tool_results.clone(),
            })
            .await;

            last_completed_turn = Some(PrepareNextTurnContext {
                message: message.clone(),
                tool_results,
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            });

            if let Some(should_stop) = &config.should_stop_after_turn
                && (should_stop)(last_completed_turn.clone().expect("just assigned")).await
            {
                (emit)(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(());
            }

            pending_messages = match &config.get_steering_messages {
                Some(get) => (get)().await,
                None => Vec::new(),
            };
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_up_messages = match &config.get_follow_up_messages {
            Some(get) => (get)().await,
            None => Vec::new(),
        };
        if !follow_up_messages.is_empty() {
            // Set as pending so the inner loop processes them.
            pending_messages = follow_up_messages;
            continue;
        }

        // No more messages, exit.
        break;
    }

    (emit)(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
    Ok(())
}

/// Stream an assistant response from the LLM.
/// This is where `AgentMessage` lists get transformed to `Message` lists.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> Result<AssistantMessage, String> {
    // Apply context transform if configured (AgentMessage[] → AgentMessage[]).
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = (transform)(messages, signal.clone()).await;
    }

    // Convert to LLM-compatible messages (AgentMessage[] → Message[]).
    let llm_messages = (config.convert_to_llm)(messages).await;

    // Build LLM context.
    let llm_context = Context {
        system_prompt: Some(context.system_prompt.clone()),
        messages: llm_messages,
        tools: context
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(|tool| tool.to_tool()).collect()),
    };

    // Resolve API key (important for expiring tokens).
    let mut resolved_api_key = config.stream_options.base.base.api_key.clone();
    if let Some(get_api_key) = &config.get_api_key
        && let Some(key) = (get_api_key)(&config.model.provider).await
        && !key.is_empty()
    {
        resolved_api_key = Some(key);
    }

    let mut options = config.stream_options.clone();
    options.base.base.api_key = resolved_api_key;
    options.base.base.signal = signal.clone();

    let response: AssistantMessageEventStream =
        (stream_fn)(&config.model, &llm_context, Some(&options))?;

    let mut partial_message: Option<AssistantMessage> = None;
    let mut added_partial = false;

    while let Some(event) = response.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                context
                    .messages
                    .push(AgentMessage::Assistant(Box::new(partial.clone())));
                added_partial = true;
                partial_message = Some(partial.clone());
                (emit)(AgentEvent::MessageStart {
                    message: Box::new(AgentMessage::Assistant(Box::new(partial.clone()))),
                })
                .await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => {
                if partial_message.is_some() {
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::Assistant(Box::new(partial.clone()));
                    }
                    partial_message = Some(partial.clone());
                    (emit)(AgentEvent::MessageUpdate {
                        message: Box::new(AgentMessage::Assistant(Box::new(partial.clone()))),
                        assistant_message_event: event.clone(),
                    })
                    .await;
                }
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                let final_message =
                    finalize_streamed_message(context, emit, added_partial, &response).await;
                return Ok(final_message);
            }
        }
    }

    let final_message = finalize_streamed_message(context, emit, added_partial, &response).await;
    Ok(final_message)
}

/// Shared tail of `streamAssistantResponse`: resolve the final message,
/// place it in the context, and emit the message lifecycle events.
async fn finalize_streamed_message(
    context: &mut AgentContext,
    emit: &AgentEventSink,
    added_partial: bool,
    response: &AssistantMessageEventStream,
) -> AssistantMessage {
    let final_message = response.result().await;
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = AgentMessage::Assistant(Box::new(final_message.clone()));
        }
    } else {
        context
            .messages
            .push(AgentMessage::Assistant(Box::new(final_message.clone())));
        (emit)(AgentEvent::MessageStart {
            message: Box::new(AgentMessage::Assistant(Box::new(final_message.clone()))),
        })
        .await;
    }
    (emit)(AgentEvent::MessageEnd {
        message: Box::new(AgentMessage::Assistant(Box::new(final_message.clone()))),
    })
    .await;
    final_message
}

struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

/// Fail all tool calls from an assistant message truncated by the output
/// token limit: streamed arguments can parse and validate while silently
/// incomplete, so none of them are safe to execute.
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[AgentToolCall],
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tool_call in tool_calls {
        (emit)(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: serde_json::Value::Object(tool_call.arguments.clone()),
        })
        .await;
        let finalized = FinalizedToolCallOutcome {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

/// Execute tool calls from an assistant message.
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let tool_calls: Vec<AgentToolCall> = assistant_message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect();
    let has_sequential_tool_call = tool_calls.iter().any(|tool_call| {
        current_context
            .tools
            .as_ref()
            .and_then(|tools| tools.iter().find(|tool| tool.name == tool_call.name))
            .and_then(|tool| tool.execution_mode)
            == Some(super::types::ToolExecutionMode::Sequential)
    });
    if config.tool_execution == Some(super::types::ToolExecutionMode::Sequential)
        || has_sequential_tool_call
    {
        return execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await;
    }
    execute_tool_calls_parallel(
        current_context,
        assistant_message,
        &tool_calls,
        config,
        signal,
        emit,
    )
    .await
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        (emit)(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: serde_json::Value::Object(tool_call.arguments.clone()),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            &signal,
        )
        .await;
        let finalized = match preparation {
            ToolCallPreparation::Immediate { result, is_error } => FinalizedToolCallOutcome {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            ToolCallPreparation::Prepared(prepared) => {
                let executed = execute_prepared_tool_call(&prepared, signal.clone(), emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &prepared,
                    executed,
                    config,
                    &signal,
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        finalized_calls.push(finalized);
        messages.push(tool_result_message);

        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

enum ParallelToolCallEntry {
    Done(FinalizedToolCallOutcome),
    Prepared(PreparedToolCall),
}

async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut entries: Vec<ParallelToolCallEntry> = Vec::new();

    for tool_call in tool_calls {
        (emit)(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: serde_json::Value::Object(tool_call.arguments.clone()),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            &signal,
        )
        .await;
        match preparation {
            ToolCallPreparation::Immediate { result, is_error } => {
                let finalized = FinalizedToolCallOutcome {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await;
                entries.push(ParallelToolCallEntry::Done(finalized));
                if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                    break;
                }
            }
            ToolCallPreparation::Prepared(prepared) => {
                entries.push(ParallelToolCallEntry::Prepared(prepared));
                if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                    break;
                }
            }
        }
    }

    let ordered_finalized_calls: Vec<FinalizedToolCallOutcome> =
        join_all(entries.into_iter().map(|entry| {
            let emit = Arc::clone(emit);
            let config = config.clone();
            let signal = signal.clone();
            let current_context = current_context.clone();
            let assistant_message = assistant_message.clone();
            async move {
                match entry {
                    ParallelToolCallEntry::Done(finalized) => finalized,
                    ParallelToolCallEntry::Prepared(prepared) => {
                        let executed =
                            execute_prepared_tool_call(&prepared, signal.clone(), &emit).await;
                        let finalized = finalize_executed_tool_call(
                            &current_context,
                            &assistant_message,
                            &prepared,
                            executed,
                            &config,
                            &signal,
                        )
                        .await;
                        emit_tool_execution_end(&finalized, &emit).await;
                        finalized
                    }
                }
            }
        }))
        .await;

    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &ordered_finalized_calls {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&ordered_finalized_calls),
    }
}

struct PreparedToolCall {
    tool_call: AgentToolCall,
    tool: AgentTool,
    args: Arc<Mutex<serde_json::Value>>,
}

enum ToolCallPreparation {
    Prepared(PreparedToolCall),
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

struct FinalizedToolCallOutcome {
    tool_call: AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
}

fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCallOutcome]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate == Some(true))
}

/// Port of `prepareToolCallArguments`: applies the tool's
/// `prepareArguments` shim before validation.
fn prepare_tool_call_arguments(tool: &AgentTool, tool_call: &AgentToolCall) -> AgentToolCall {
    let Some(prepare_arguments) = &tool.prepare_arguments else {
        return tool_call.clone();
    };
    let raw = serde_json::Value::Object(tool_call.arguments.clone());
    let prepared_arguments = prepare_arguments(&raw);
    if prepared_arguments == raw {
        return tool_call.clone();
    }
    let arguments = match prepared_arguments {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    ToolCall {
        arguments,
        ..tool_call.clone()
    }
}

async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &AgentToolCall,
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
) -> ToolCallPreparation {
    let Some(tool) = current_context
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|tool| tool.name == tool_call.name))
        .cloned()
    else {
        return ToolCallPreparation::Immediate {
            result: create_error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    // The TypeScript try/catch maps validation failures (and throwing
    // hooks) to immediate error results.
    let prepared_tool_call = prepare_tool_call_arguments(&tool, tool_call);
    let validated_args = match validate_tool_arguments(&tool.to_tool(), &prepared_tool_call) {
        Ok(args) => args,
        Err(error) => {
            return ToolCallPreparation::Immediate {
                result: create_error_tool_result(error),
                is_error: true,
            };
        }
    };

    let args = Arc::new(Mutex::new(serde_json::Value::Object(validated_args)));
    if let Some(before_tool_call) = &config.before_tool_call {
        let before_result = (before_tool_call)(
            super::types::BeforeToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: Arc::clone(&args),
                context: current_context.clone(),
            },
            signal.clone(),
        )
        .await;
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return ToolCallPreparation::Immediate {
                result: create_error_tool_result("Operation aborted".to_string()),
                is_error: true,
            };
        }
        if before_result
            .as_ref()
            .is_some_and(|result| result.block == Some(true))
        {
            let blocked = before_result.expect("checked above");
            let reason = blocked
                .reason
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "Tool execution was blocked".to_string());
            let mut result = create_error_tool_result(reason);
            if blocked.terminate == Some(true) {
                result.terminate = Some(true);
            }
            return ToolCallPreparation::Immediate {
                result,
                is_error: true,
            };
        }
    }
    if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
        return ToolCallPreparation::Immediate {
            result: create_error_tool_result("Operation aborted".to_string()),
            is_error: true,
        };
    }
    ToolCallPreparation::Prepared(PreparedToolCall {
        tool_call: tool_call.clone(),
        tool,
        args,
    })
}

async fn execute_prepared_tool_call(
    prepared: &PreparedToolCall,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallOutcome {
    // Update events queue up while the tool runs and are awaited (in call
    // order) once it settles; later calls are ignored.
    let update_events: Arc<Mutex<VecDeque<AgentEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
    let accepting_updates = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let on_update: super::types::AgentToolUpdateCallback = {
        let update_events = Arc::clone(&update_events);
        let accepting_updates = Arc::clone(&accepting_updates);
        let tool_call_id = prepared.tool_call.id.clone();
        let tool_name = prepared.tool_call.name.clone();
        let args = serde_json::Value::Object(prepared.tool_call.arguments.clone());
        Arc::new(move |partial_result| {
            if !accepting_updates.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            lock(&update_events).push_back(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
                partial_result: Box::new(partial_result),
            });
        })
    };

    let execute_args = lock(&prepared.args).clone();
    let outcome = (prepared.tool.execute)(
        &prepared.tool_call.id,
        &execute_args,
        signal.as_ref(),
        Some(&on_update),
    )
    .await;

    accepting_updates.store(false, std::sync::atomic::Ordering::SeqCst);
    loop {
        let event = lock(&update_events).pop_front();
        let Some(event) = event else { break };
        (emit)(event).await;
    }

    match outcome {
        Ok(result) => ExecutedToolCallOutcome {
            result,
            is_error: false,
        },
        Err(message) => ExecutedToolCallOutcome {
            result: create_error_tool_result(message),
            is_error: true,
        },
    }
}

async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
) -> FinalizedToolCallOutcome {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after_tool_call) = &config.after_tool_call {
        let args_snapshot = lock(&prepared.args).clone();
        let after_result = (after_tool_call)(
            super::types::AfterToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: prepared.tool_call.clone(),
                args: args_snapshot,
                result: result.clone(),
                is_error,
                context: current_context.clone(),
            },
            signal.clone(),
        )
        .await;
        if let Some(after_result) = after_result {
            if let Some(content) = after_result.content {
                result.content = content;
            }
            if let Some(details) = after_result.details {
                result.details = details;
            }
            if let Some(usage) = after_result.usage {
                result.usage = Some(usage);
            }
            if let Some(terminate) = after_result.terminate {
                result.terminate = Some(terminate);
            }
            if let Some(after_is_error) = after_result.is_error {
                is_error = after_is_error;
            }
        }
    }

    FinalizedToolCallOutcome {
        tool_call: prepared.tool_call.clone(),
        result,
        is_error,
    }
}

fn create_error_tool_result(message: String) -> AgentToolResult {
    AgentToolResult {
        content: vec![BlockContent::Text(crate::ai::types::TextContent {
            text: message,
            ..Default::default()
        })],
        details: serde_json::Value::Object(serde_json::Map::new()),
        usage: None,
        added_tool_names: None,
        terminate: None,
    }
}

async fn emit_tool_execution_end(finalized: &FinalizedToolCallOutcome, emit: &AgentEventSink) {
    (emit)(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: Box::new(finalized.result.clone()),
        is_error: finalized.is_error,
    })
    .await;
}

fn create_tool_result_message(finalized: &FinalizedToolCallOutcome) -> ToolResultMessage {
    // Untyped tools (JS extensions) can return results without content; the
    // Rust `AgentToolResult.content` is always a vector, so the `?? []`
    // normalization is enforced by the type.
    ToolResultMessage {
        role: Default::default(),
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: Some(finalized.result.details.clone()),
        usage: finalized.result.usage.clone(),
        added_tool_names: finalized
            .result
            .added_tool_names
            .as_ref()
            .filter(|names| !names.is_empty())
            .cloned(),
        is_error: finalized.is_error,
        timestamp: now_millis(),
    }
}

async fn emit_tool_result_message(tool_result_message: &ToolResultMessage, emit: &AgentEventSink) {
    let message = AgentMessage::ToolResult(Box::new(tool_result_message.clone()));
    (emit)(AgentEvent::MessageStart {
        message: Box::new(message.clone()),
    })
    .await;
    (emit)(AgentEvent::MessageEnd {
        message: Box::new(message),
    })
    .await;
}

/// Converts agent messages that are already LLM messages, dropping custom
/// application messages — the rule used by the default `convertToLlm`
/// (`agent.ts`).
pub fn pass_through_llm_messages(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                AgentMessage::User(_) | AgentMessage::Assistant(_) | AgentMessage::ToolResult(_)
            )
        })
        .filter_map(AgentMessage::into_llm_message)
        .collect()
}
