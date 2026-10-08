//! Port of `pi-core/agent/test/e2e.test.ts`: the `Agent` integration suite
//! driven against the faux provider through `streamSimple`.
//!
//! Also ports `test/utils/calculate.ts`. The TypeScript helper evaluates
//! expressions with `new Function`; the Rust port ships a small
//! arithmetic evaluator (+, -, *, /, %, parentheses, integers/decimals)
//! that covers the expression grammar exercised by the suite.

use std::sync::{Arc, Mutex};

use serde_json::json;

use pi_core::agent::agent::{Agent, AgentInitialState, AgentOptions};
use pi_core::agent::types::{
    AgentEvent, AgentMessage, AgentTool, AgentToolResult, StreamFn, ThinkingLevel,
};
use pi_core::ai::compat::{register_faux_provider, stream_simple};
use pi_core::ai::providers::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, FauxTokenSize,
    RegisterFauxProviderOptions, faux_assistant_message, faux_text, faux_thinking, faux_tool_call,
};
use pi_core::ai::types::{
    AssistantContent, BlockContent, RoleAssistant, RoleUser, StopReason, TextContent,
    ToolResultMessage, Usage,
};

// ---------------------------------------------------------------------------
// calculate.ts port
// ---------------------------------------------------------------------------

/// Evaluates a decimal arithmetic expression (`+ - * / %` and parentheses).
fn eval_expression(expression: &str) -> Result<f64, String> {
    let tokens: Vec<char> = expression.chars().collect();
    let mut pos = 0usize;
    let value = parse_additive(&tokens, &mut pos)?;
    while pos < tokens.len() && tokens[pos].is_whitespace() {
        pos += 1;
    }
    if pos != tokens.len() {
        return Err(format!("unexpected token: {}", tokens[pos]));
    }
    Ok(value)
}

fn skip_whitespace(tokens: &[char], pos: &mut usize) {
    while *pos < tokens.len() && tokens[*pos].is_whitespace() {
        *pos += 1;
    }
}

fn parse_additive(tokens: &[char], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_multiplicative(tokens, pos)?;
    loop {
        skip_whitespace(tokens, pos);
        match tokens.get(*pos) {
            Some('+') => {
                *pos += 1;
                value += parse_multiplicative(tokens, pos)?;
            }
            Some('-') => {
                *pos += 1;
                value -= parse_multiplicative(tokens, pos)?;
            }
            _ => return Ok(value),
        }
    }
}

fn parse_multiplicative(tokens: &[char], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_primary(tokens, pos)?;
    loop {
        skip_whitespace(tokens, pos);
        match tokens.get(*pos) {
            Some('*') => {
                *pos += 1;
                value *= parse_primary(tokens, pos)?;
            }
            Some('/') => {
                *pos += 1;
                value /= parse_primary(tokens, pos)?;
            }
            Some('%') => {
                *pos += 1;
                value %= parse_primary(tokens, pos)?;
            }
            _ => return Ok(value),
        }
    }
}

fn parse_primary(tokens: &[char], pos: &mut usize) -> Result<f64, String> {
    skip_whitespace(tokens, pos);
    match tokens.get(*pos) {
        Some('(') => {
            *pos += 1;
            let value = parse_additive(tokens, pos)?;
            skip_whitespace(tokens, pos);
            if tokens.get(*pos) != Some(&')') {
                return Err("expected )".to_string());
            }
            *pos += 1;
            Ok(value)
        }
        Some('-') => {
            *pos += 1;
            Ok(-parse_primary(tokens, pos)?)
        }
        Some(c) if c.is_ascii_digit() || *c == '.' => {
            let start = *pos;
            while *pos < tokens.len() && (tokens[*pos].is_ascii_digit() || tokens[*pos] == '.') {
                *pos += 1;
            }
            let number: String = tokens[start..*pos].iter().collect();
            number.parse::<f64>().map_err(|error| error.to_string())
        }
        other => Err(format!("unexpected token: {:?}", other)),
    }
}

/// Formats a computed value the way JavaScript `${result}` does for the
/// whole numbers used in the suite (56088, 8).
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn calculate(expression: &str) -> Result<AgentToolResult, String> {
    let result = eval_expression(expression)?;
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: format!("{expression} = {}", format_number(result)),
            ..Default::default()
        })],
        details: serde_json::Value::Null,
        ..Default::default()
    })
}

fn calculate_tool() -> AgentTool {
    AgentTool {
        label: "Calculator".to_string(),
        name: "calculate".to_string(),
        description: "Evaluate mathematical expressions".to_string(),
        parameters: json!({
            "type": "object",
            "required": ["expression"],
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "The mathematical expression to evaluate"
                }
            }
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_tool_call_id, params, _signal, _on_update| {
            let expression = params
                .get("expression")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            Box::pin(async move { calculate(&expression) })
        }),
    }
}

// ---------------------------------------------------------------------------
// e2e.test.ts port
// ---------------------------------------------------------------------------

async fn registry_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn stream_simple_fn() -> StreamFn {
    Arc::new(|model, context, options| Ok(stream_simple(model, context, options)))
}

fn get_text_content_assistant(message: &pi_core::ai::types::AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn get_text_content_tool_result(message: &ToolResultMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            BlockContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_text(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::Assistant(assistant) => Some(get_text_content_assistant(assistant)),
        AgentMessage::ToolResult(tool_result) => Some(get_text_content_tool_result(tool_result)),
        _ => None,
    }
}

async fn basic_prompt(model: pi_core::ai::types::Model) {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some(
                "You are a helpful assistant. Keep your responses concise.".to_string(),
            ),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    });

    agent
        .prompt("What is 2+2? Answer with just the number.")
        .await
        .unwrap();

    assert!(!agent.is_streaming());
    let messages = agent.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role(), "user");
    assert_eq!(messages[1].role(), "assistant");
    assert!(
        message_text(&messages[1])
            .expect("assistant text")
            .contains('4')
    );
}

async fn tool_execution(model: pi_core::ai::types::Model) {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some(
                "You are a helpful assistant. Always use the calculator tool for math.".to_string(),
            ),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![calculate_tool()]),
            ..Default::default()
        }),
        ..Default::default()
    });

    type PendingSnapshot = Vec<(&'static str, Vec<String>)>;
    let pending_tool_calls_during_events: Arc<Mutex<PendingSnapshot>> =
        Arc::new(Mutex::new(Vec::new()));
    let agent = Arc::new(agent);
    let tracked_agent = Arc::clone(&agent);
    let tracked = Arc::clone(&pending_tool_calls_during_events);
    let _subscription = agent.subscribe(Arc::new(
        move |event: &AgentEvent, _signal: &tokio_util::sync::CancellationToken| {
            let tracked_agent = Arc::clone(&tracked_agent);
            let tracked = Arc::clone(&tracked);
            let observed = match event {
                AgentEvent::ToolExecutionStart { .. } => Some("tool_execution_start"),
                AgentEvent::ToolExecutionEnd { .. } => Some("tool_execution_end"),
                _ => None,
            }
            .map(|kind| {
                (
                    kind,
                    tracked_agent
                        .pending_tool_calls()
                        .into_iter()
                        .collect::<Vec<_>>(),
                )
            });
            Box::pin(async move {
                if let Some(observed) = observed {
                    tracked.lock().unwrap().push(observed);
                }
            })
        },
    ));

    agent
        .prompt("Calculate 123 * 456 using the calculator tool.")
        .await
        .unwrap();

    assert!(!agent.is_streaming());
    let messages = agent.messages();
    assert!(messages.len() >= 4);
    let tool_result_msg = messages
        .iter()
        .find(|message| matches!(message, AgentMessage::ToolResult(_)))
        .expect("tool result message");
    assert!(
        message_text(tool_result_msg)
            .expect("tool result text")
            .contains("123 * 456 = 56088")
    );

    let final_message = messages.last().expect("final message");
    assert_eq!(final_message.role(), "assistant");
    assert!(
        message_text(final_message)
            .expect("final text")
            .contains("56088")
    );
    assert!(agent.pending_tool_calls().is_empty());
    let observed = pending_tool_calls_during_events.lock().unwrap().clone();
    assert_eq!(
        observed,
        vec![
            ("tool_execution_start", vec!["calc-1".to_string()]),
            ("tool_execution_end", Vec::<String>::new()),
        ]
    );
}

async fn abort_execution(model: pi_core::ai::types::Model) {
    let agent = Arc::new(Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    }));

    let prompt_agent = Arc::clone(&agent);
    let prompt_promise = tokio::spawn(async move {
        prompt_agent
            .prompt("Count slowly from 1 to 20.")
            .await
            .unwrap();
    });
    tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            agent.abort();
        }
    })
    .await
    .unwrap();
    prompt_promise.await.unwrap();

    assert!(!agent.is_streaming());
    let messages = agent.messages();
    assert!(messages.len() >= 2);

    let last_message = messages.last().expect("last message");
    assert_eq!(last_message.role(), "assistant");
    let AgentMessage::Assistant(last_message) = last_message else {
        panic!("expected assistant message");
    };
    assert_eq!(last_message.stop_reason, StopReason::Aborted);
    assert!(last_message.error_message.is_some());
    assert_eq!(agent.error_message(), last_message.error_message);
}

async fn state_updates(model: pi_core::ai::types::Model) {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    });

    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_events = Arc::clone(&events);
    let _subscription = agent.subscribe(Arc::new(
        move |event: &AgentEvent, _signal: &tokio_util::sync::CancellationToken| {
            let events = Arc::clone(&captured_events);
            let kind = match event {
                AgentEvent::AgentStart => "agent_start",
                AgentEvent::TurnStart => "turn_start",
                AgentEvent::MessageStart { .. } => "message_start",
                AgentEvent::MessageUpdate { .. } => "message_update",
                AgentEvent::MessageEnd { .. } => "message_end",
                AgentEvent::TurnEnd { .. } => "turn_end",
                AgentEvent::AgentEnd { .. } => "agent_end",
                AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
                AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
                AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            };
            Box::pin(async move {
                events.lock().unwrap().push(kind);
            })
        },
    ));

    agent.prompt("Count from 1 to 5.").await.unwrap();

    let events = events.lock().unwrap().clone();
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(events.contains(&expected), "missing {expected}: {events:?}");
    }
    assert!(
        events.iter().position(|e| *e == "agent_start").unwrap()
            < events.iter().position(|e| *e == "message_start").unwrap()
    );
    assert!(
        events.iter().position(|e| *e == "message_start").unwrap()
            < events.iter().position(|e| *e == "message_end").unwrap()
    );
    assert!(
        events.iter().position(|e| *e == "message_end").unwrap()
            < events.iter().rposition(|e| *e == "agent_end").unwrap()
    );

    assert!(!agent.is_streaming());
    assert_eq!(agent.messages().len(), 2);
}

async fn multi_turn_conversation(model: pi_core::ai::types::Model) {
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    });

    agent.prompt("My name is Alice.").await.unwrap();
    assert_eq!(agent.messages().len(), 2);

    agent.prompt("What is my name?").await.unwrap();
    assert_eq!(agent.messages().len(), 4);

    let last_message = &agent.messages()[3];
    assert_eq!(last_message.role(), "assistant");
    assert!(
        message_text(last_message)
            .expect("assistant text")
            .to_lowercase()
            .contains("alice")
    );
}

fn faux_assistant(
    content: Vec<AssistantContent>,
    stop_reason: StopReason,
) -> pi_core::ai::types::AssistantMessage {
    faux_assistant_message(
        content,
        FauxMessageOptions {
            stop_reason: Some(stop_reason),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn handles_a_basic_text_prompt() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("4", FauxMessageOptions::default()),
    ))]);
    basic_prompt(faux.get_model()).await;
    faux.unregister();
}

#[tokio::test]
async fn executes_tools_and_tracks_pending_tool_calls() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant(
            vec![
                faux_text("Let me calculate that."),
                faux_tool_call(
                    "calculate",
                    json!({ "expression": "123 * 456" }),
                    Some("calc-1".to_string()),
                ),
            ],
            StopReason::ToolUse,
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "The result is 56088.",
            FauxMessageOptions::default(),
        ))),
    ]);
    tool_execution(faux.get_model()).await;
    faux.unregister();
}

#[tokio::test]
async fn handles_abort_during_streaming() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(20.0),
        token_size: Some(FauxTokenSize {
            min: Some(2),
            max: Some(2),
        }),
        ..Default::default()
    });
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen",
            FauxMessageOptions::default(),
        ),
    ))]);
    abort_execution(faux.get_model()).await;
    faux.unregister();
}

#[tokio::test]
async fn emits_lifecycle_updates_while_streaming() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        token_size: Some(FauxTokenSize {
            min: Some(1),
            max: Some(1),
        }),
        ..Default::default()
    });
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("1 2 3 4 5", FauxMessageOptions::default()),
    ))]);
    state_updates(faux.get_model()).await;
    faux.unregister();
}

#[tokio::test]
async fn maintains_context_across_multiple_turns() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "Nice to meet you, Alice.",
            FauxMessageOptions::default(),
        ))),
        FauxResponseStep::Factory(Arc::new(|context, _options, _state, _model| {
            let has_alice = context.messages.iter().any(|message| match message {
                pi_core::ai::types::Message::User(user) => match &user.content {
                    pi_core::ai::types::UserContent::Text(text) => text.contains("Alice"),
                    pi_core::ai::types::UserContent::Blocks(blocks) => blocks.iter().any(
                        |block| matches!(block, pi_core::ai::types::BlockContent::Text(text) if text.text.contains("Alice")),
                    ),
                },
                _ => false,
            });
            Ok(faux_assistant_message(
                if has_alice {
                    "Your name is Alice."
                } else {
                    "I do not know your name."
                },
                FauxMessageOptions::default(),
            ))
        })),
    ]);
    multi_turn_conversation(faux.get_model()).await;
    faux.unregister();
}

#[tokio::test]
async fn preserves_thinking_content_blocks() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        models: vec![FauxModelDefinition {
            id: "faux-reasoning".to_string(),
            reasoning: Some(true),
            ..Default::default()
        }],
        ..Default::default()
    });
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(faux_assistant(
        vec![faux_thinking("step by step"), faux_text("4")],
        StopReason::Stop,
    )))]);

    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            model: Some(faux.get_model()),
            thinking_level: Some(ThinkingLevel::Low),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    });

    agent.prompt("What is 2+2?").await.unwrap();

    let messages = agent.messages();
    let AgentMessage::Assistant(assistant) = &messages[1] else {
        panic!("expected assistant message");
    };
    assert_eq!(
        assistant.content,
        vec![
            pi_core::ai::types::AssistantContent::Thinking(pi_core::ai::types::ThinkingContent {
                thinking: "step by step".to_string(),
                ..Default::default()
            }),
            pi_core::ai::types::AssistantContent::Text(TextContent {
                text: "4".to_string(),
                ..Default::default()
            }),
        ]
    );
    faux.unregister();
}

// ---------------------------------------------------------------------------
// Agent.continue() suites
// ---------------------------------------------------------------------------

#[tokio::test]
async fn continue_throws_when_no_messages_in_context() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("Test".to_string()),
            model: Some(faux.get_model()),
            ..Default::default()
        }),
        ..Default::default()
    });

    assert_eq!(
        agent.continue_().await.unwrap_err(),
        "No messages to continue from"
    );
    faux.unregister();
}

#[tokio::test]
async fn continue_throws_when_last_message_is_assistant() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    let model = faux.get_model();
    let assistant_message = pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: "Hello".to_string(),
            ..Default::default()
        })],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        timestamp: 0,
        ..Default::default()
    };
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some("Test".to_string()),
            model: Some(model),
            ..Default::default()
        }),
        ..Default::default()
    });
    agent.set_messages(vec![AgentMessage::Assistant(Box::new(assistant_message))]);

    assert_eq!(
        agent.continue_().await.unwrap_err(),
        "Cannot continue from message role: assistant"
    );
    faux.unregister();
}

#[tokio::test]
async fn continues_and_gets_response_when_last_message_is_user() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("HELLO WORLD", FauxMessageOptions::default()),
    ))]);
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some(
                "You are a helpful assistant. Follow instructions exactly.".to_string(),
            ),
            model: Some(faux.get_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    });

    agent.set_messages(vec![AgentMessage::User(pi_core::ai::types::UserMessage {
        role: RoleUser,
        content: pi_core::ai::types::UserContent::Blocks(vec![BlockContent::Text(TextContent {
            text: "Say exactly: HELLO WORLD".to_string(),
            ..Default::default()
        })]),
        timestamp: 0,
    })]);

    agent.continue_().await.unwrap();

    assert!(!agent.is_streaming());
    let messages = agent.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role(), "user");
    assert_eq!(messages[1].role(), "assistant");
    assert!(
        message_text(&messages[1])
            .expect("assistant text")
            .to_uppercase()
            .contains("HELLO WORLD")
    );
    faux.unregister();
}

#[tokio::test]
async fn continues_and_processes_tool_results() {
    let _guard = registry_lock().await;
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    let model = faux.get_model();
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("The answer is 8.", FauxMessageOptions::default()),
    ))]);
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            system_prompt: Some(
                "You are a helpful assistant. After getting a calculation result, state the answer clearly."
                    .to_string(),
            ),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![calculate_tool()]),
            ..Default::default()
        }),
        ..Default::default()
    });

    agent.set_messages(vec![
        AgentMessage::User(pi_core::ai::types::UserMessage {
            role: RoleUser,
            content: pi_core::ai::types::UserContent::Blocks(vec![BlockContent::Text(
                TextContent {
                    text: "What is 5 + 3?".to_string(),
                    ..Default::default()
                },
            )]),
            timestamp: 0,
        }),
        AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
            role: RoleAssistant,
            content: vec![
                AssistantContent::Text(TextContent {
                    text: "Let me calculate that.".to_string(),
                    ..Default::default()
                }),
                AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
                    id: "calc-1".to_string(),
                    name: "calculate".to_string(),
                    arguments: json!({ "expression": "5 + 3" })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    ..Default::default()
                }),
            ],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            ..Default::default()
        })),
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: Default::default(),
            tool_call_id: "calc-1".to_string(),
            tool_name: "calculate".to_string(),
            content: vec![BlockContent::Text(TextContent {
                text: "5 + 3 = 8".to_string(),
                ..Default::default()
            })],
            details: Some(serde_json::Value::Null),
            is_error: false,
            timestamp: 0,
            ..Default::default()
        })),
    ]);

    agent.continue_().await.unwrap();

    assert!(!agent.is_streaming());
    let messages = agent.messages();
    assert!(messages.len() >= 4);

    let last_message = messages.last().expect("last message");
    assert_eq!(last_message.role(), "assistant");
    assert!(
        message_text(last_message)
            .expect("assistant text")
            .contains('8')
    );
    faux.unregister();
}
