//! Port of `pi-core/ai/test/transform-messages-copilot-openai-to-anthropic.test.ts`:
//! OpenAI to Anthropic session migration for Copilot Claude.

use pi_core::ai::api::transform_messages::{ToolCallIdNormalizer, transform_messages};
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, Message, Model, ModelInput, RoleAssistant,
    RoleToolResult, RoleUser, StopReason, TextContent, ThinkingContent, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
};

/// The normalize function matching what anthropic.ts uses:
/// `id.replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 64)`.
fn anthropic_normalize_tool_call_id(id: &str, _source: &AssistantMessage) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .chars()
        .take(64)
        .collect()
}

fn copilot_claude_model() -> Model {
    Model {
        id: "claude-sonnet-4.6".to_string(),
        name: "Claude Sonnet 4.6".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "github-copilot".to_string(),
        base_url: "https://api.individual.githubcopilot.com".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 16_000,
        ..Default::default()
    }
}

/// Port of `makeAssistantMessage`: an OpenAI Responses turn from Copilot.
fn make_assistant_message(content: Vec<AssistantContent>) -> Message {
    Message::Assistant(Box::new(AssistantMessage {
        role: RoleAssistant,
        content,
        api: "openai-responses".to_string(),
        provider: "github-copilot".to_string(),
        model: "gpt-5".to_string(),
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        timestamp: 0,
        ..Default::default()
    }))
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    })
}

fn tool_result(tool_call_id: &str, tool_name: &str, text: &str) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: 0,
        ..Default::default()
    }))
}

fn assistant_of(message: &Message) -> &AssistantMessage {
    match message {
        Message::Assistant(assistant) => assistant,
        other => panic!("expected assistant message, got {other:?}"),
    }
}

#[test]
fn converts_thinking_blocks_to_plain_text_when_source_model_differs() {
    let model = copilot_claude_model();
    // The TypeScript normalizer also receives the model; the Rust port drops
    // the unused parameter, and this fixture ignores it either way.
    let normalize: &ToolCallIdNormalizer = &anthropic_normalize_tool_call_id;
    let messages = vec![
        user_message("hello"),
        Message::Assistant(Box::new(AssistantMessage {
            role: RoleAssistant,
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "Let me think about this...".to_string(),
                    thinking_signature: Some("reasoning_content".to_string()),
                    ..Default::default()
                }),
                AssistantContent::Text(TextContent {
                    text: "Hi there!".to_string(),
                    ..Default::default()
                }),
            ],
            api: "openai-completions".to_string(),
            provider: "github-copilot".to_string(),
            model: "gpt-4o".to_string(),
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            timestamp: 0,
            ..Default::default()
        })),
    ];

    let result = transform_messages(&messages, &model, Some(normalize));
    let assistant_msg = result
        .iter()
        .find(|message| matches!(message, Message::Assistant(_)))
        .expect("assistant message");
    let assistant_msg = assistant_of(assistant_msg);

    // Thinking block should be converted to text since models differ.
    let thinking_blocks = assistant_msg
        .content
        .iter()
        .filter(|block| matches!(block, AssistantContent::Thinking(_)))
        .count();
    let text_blocks = assistant_msg
        .content
        .iter()
        .filter(|block| matches!(block, AssistantContent::Text(_)))
        .count();
    assert_eq!(thinking_blocks, 0);
    assert!(text_blocks >= 2);
}

#[test]
fn removes_thought_signature_from_tool_calls_when_migrating_between_models() {
    let model = copilot_claude_model();
    // The TypeScript normalizer also receives the model; the Rust port drops
    // the unused parameter, and this fixture ignores it either way.
    let normalize: &ToolCallIdNormalizer = &anthropic_normalize_tool_call_id;
    let messages = vec![
        user_message("run a command"),
        Message::Assistant(Box::new(AssistantMessage {
            role: RoleAssistant,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "call_123".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "ls" })
                    .as_object()
                    .cloned()
                    .unwrap(),
                thought_signature: Some(
                    r#"{"type":"reasoning.encrypted","id":"call_123","data":"encrypted"}"#
                        .to_string(),
                ),
                ..Default::default()
            })],
            api: "openai-responses".to_string(),
            provider: "github-copilot".to_string(),
            model: "gpt-5".to_string(),
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            ..Default::default()
        })),
        tool_result("call_123", "bash", "output"),
    ];

    let result = transform_messages(&messages, &model, Some(normalize));
    let assistant_msg = result
        .iter()
        .find(|message| matches!(message, Message::Assistant(_)))
        .expect("assistant message");
    let tool_call = assistant_of(assistant_msg)
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(tool_call) => Some(tool_call),
            _ => None,
        })
        .expect("tool call block");

    assert!(tool_call.thought_signature.is_none());
}

/// The TypeScript `toMatchObject` on the last message.
fn assert_synthetic_result(message: &Message, tool_call_id: &str, tool_name: &str) {
    let Message::ToolResult(result) = message else {
        panic!("expected toolResult message, got {message:?}");
    };
    assert_eq!(result.tool_call_id, tool_call_id);
    assert_eq!(result.tool_name, tool_name);
    assert!(result.is_error);
    assert_eq!(
        result.content,
        vec![BlockContent::Text(TextContent {
            text: "No result provided".to_string(),
            ..Default::default()
        })]
    );
}

#[test]
fn adds_synthetic_tool_results_for_trailing_orphaned_tool_calls() {
    let model = copilot_claude_model();
    // The TypeScript normalizer also receives the model; the Rust port drops
    // the unused parameter, and this fixture ignores it either way.
    let normalize: &ToolCallIdNormalizer = &anthropic_normalize_tool_call_id;
    let messages = vec![
        user_message("read the file"),
        make_assistant_message(vec![tool_call(
            "call_123|fc_123",
            "read",
            serde_json::json!({ "path": "README.md" }),
        )]),
    ];

    let result = transform_messages(&messages, &model, Some(normalize));
    let last_message = result.last().expect("result message");

    assert_synthetic_result(last_message, "call_123_fc_123", "read");
}

#[test]
fn adds_synthetic_results_only_for_trailing_tool_calls_that_are_still_missing_results() {
    let model = copilot_claude_model();
    // The TypeScript normalizer also receives the model; the Rust port drops
    // the unused parameter, and this fixture ignores it either way.
    let normalize: &ToolCallIdNormalizer = &anthropic_normalize_tool_call_id;
    let messages = vec![
        user_message("run commands"),
        make_assistant_message(vec![
            tool_call(
                "call_1|fc_1",
                "read",
                serde_json::json!({ "path": "README.md" }),
            ),
            tool_call(
                "call_2|fc_2",
                "bash",
                serde_json::json!({ "command": "pwd" }),
            ),
        ]),
        tool_result("call_1|fc_1", "read", "done"),
    ];

    let result = transform_messages(&messages, &model, Some(normalize));
    let synthetic_results: Vec<&Message> = result
        .iter()
        .filter(|message| match message {
            Message::ToolResult(result) => result.is_error,
            _ => false,
        })
        .collect();

    assert_eq!(synthetic_results.len(), 1);
    assert_synthetic_result(synthetic_results[0], "call_2_fc_2", "bash");
}
