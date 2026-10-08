//! Credential-gated entry for
//! `openai-responses-reasoning-replay-e2e.test.ts`.

mod common;

use common::live::{
    LiveEffort, LiveOptions, get_model_or_panic, live_complete, live_env, now_millis,
    payload_capture, skip,
};
use pi_core::ai::types::{
    AssistantContent, BlockContent, Context, Message, RoleToolResult, RoleUser, StopReason,
    TextContent, Tool, ToolResultMessage, UserContent, UserMessage,
};

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: now_millis(),
    })
}

fn tool() -> Tool {
    Tool {
        name: "double_number".to_string(),
        description: "Doubles a number and returns the result".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": { "value": { "type": "number", "description": "A number to double" } }
        }),
        constrained_sampling: None,
    }
}

fn tool_result(call: &pi_core::ai::types::ToolCall) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![BlockContent::Text(TextContent {
            text: "42".to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: now_millis(),
        ..Default::default()
    }))
}

fn response_text(message: &pi_core::ai::types::AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn openai_responses_reasoning_replay_e2e() {
    let (Some(openai_key), Some(anthropic_key)) =
        (live_env("OPENAI_API_KEY"), live_env("ANTHROPIC_API_KEY"))
    else {
        skip(
            "OpenAI Responses reasoning replay e2e",
            "OPENAI_API_KEY + ANTHROPIC_API_KEY",
        );
        return;
    };
    let openai_options = LiveOptions {
        api_key: Some(openai_key.clone()),
        reasoning_effort: Some(LiveEffort::High),
        ..Default::default()
    };
    let user_message = user("Use the double_number tool to double 21.");
    let tools = Some(vec![tool()]);
    let model_a = get_model_or_panic("openai", "gpt-5-mini");

    let mut aborted_source = live_complete(
        &model_a,
        &Context {
            system_prompt: Some("You are a helpful assistant. Use the tool.".to_string()),
            messages: vec![user_message.clone()],
            tools: tools.clone(),
        },
        &openai_options,
    )
    .await;
    let thinking = aborted_source
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::Thinking(value) if value.thinking_signature.is_some()))
        .cloned()
        .expect("thinking signature from OpenAI Responses");
    aborted_source.content = vec![thinking];
    aborted_source.stop_reason = StopReason::Aborted;
    let response = live_complete(
        &model_a,
        &Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![
                user_message.clone(),
                Message::Assistant(Box::new(aborted_source)),
                user("Say hello to confirm you can continue."),
            ],
            tools: tools.clone(),
        },
        &openai_options,
    )
    .await;
    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{:?}",
        response.error_message
    );
    assert!(response.error_message.is_none());
    assert!(!response.content.is_empty());

    let generated = live_complete(
        &model_a,
        &Context {
            system_prompt: Some(
                "You are a helpful assistant. Always use the tool when asked.".to_string(),
            ),
            messages: vec![user_message.clone()],
            tools: tools.clone(),
        },
        &openai_options,
    )
    .await;
    let call = generated
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("OpenAI Responses tool call");
    let model_b = get_model_or_panic("openai", "gpt-5.5");
    let (captured, on_payload) = payload_capture();
    let response = live_complete(
        &model_b,
        &Context {
            system_prompt: Some("You are a helpful assistant. Answer concisely.".to_string()),
            messages: vec![
                user_message.clone(),
                Message::Assistant(Box::new(generated)),
                tool_result(&call),
                user("What was the result? Answer with just the number."),
            ],
            tools: tools.clone(),
        },
        &LiveOptions {
            on_payload: Some(on_payload),
            ..openai_options.clone()
        },
    )
    .await;
    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{:?}",
        response.error_message
    );
    assert!(response.error_message.is_none());
    assert!(!response.content.is_empty());
    assert!(response_text(&response).contains("42"));
    assert!(
        captured
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|payload| payload["input"].is_array())
    );

    let anthropic_model = get_model_or_panic("anthropic", "claude-sonnet-4-5");
    let anthropic_response = live_complete(
        &anthropic_model,
        &Context {
            system_prompt: Some(
                "You are a helpful assistant. Always use the tool when asked.".to_string(),
            ),
            messages: vec![user_message.clone()],
            tools: tools.clone(),
        },
        &LiveOptions {
            api_key: Some(anthropic_key),
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(5000),
            ..Default::default()
        },
    )
    .await;
    let call = anthropic_response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("Anthropic tool call");
    let response = live_complete(
        &model_b,
        &Context {
            system_prompt: Some("You are a helpful assistant. Answer concisely.".to_string()),
            messages: vec![
                user_message,
                Message::Assistant(Box::new(anthropic_response)),
                tool_result(&call),
                user("What was the result? Answer with just the number."),
            ],
            tools,
        },
        &openai_options,
    )
    .await;
    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{:?}",
        response.error_message
    );
    assert!(response.error_message.is_none());
    assert!(!response.content.is_empty());
    assert!(response_text(&response).contains("42"));
}
