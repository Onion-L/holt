//! Credential-gated entry for `tool-call-id-normalization.test.ts`.

mod common;

use common::live::{get_model_or_panic, live_env, now_millis, resolve_api_key, skip};
use pi_core::ai::compat::complete_simple;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, Context, Message, ProviderRequestOptions,
    RoleAssistant, RoleToolResult, RoleUser, SimpleStreamOptions, StopReason, StreamOptions,
    TextContent, Tool, ToolCall, ToolResultMessage, UserContent, UserMessage,
};

const FAILING_TOOL_CALL_ID: &str = "call_pAYbIr76hXIjncD9UE4eGfnS|t5nnb2qYMFWGSsr13fhCd1CaCu3t3qONEPuOudu4HSVEtA8YJSL6FAZUxvoOoD792VIJWl91g87EdqsCWp9krVsdBysQoDaf9lMCLb8BS4EYi4gQd5kBQBYLlgD71PYwvf+TbMD9J9/5OMD42oxSRj8H+vRf78/l2Xla33LWz4nOgsddBlbvabICRs8GHt5C9PK5keFtzyi3lsyVKNlfduK3iphsZqs4MLv4zyGJnvZo/+QzShyk5xnMSQX/f98+aEoNflEApCdEOXipipgeiNWnpFSHbcwmMkZoJhURNu+JEz3xCh1mrXeYoN5o+trLL3IXJacSsLYXDrYTipZZbJFRPAucgbnjYBC+/ZzJOfkwCs+Gkw7EoZR7ZQgJ8ma+9586n4tT4cI8DEhBSZsWMjrCt8dxKg==";

fn echo_tool() -> Tool {
    Tool {
        name: "echo".to_string(),
        description: "Echoes the message back".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["message"],
            "properties": { "message": { "type": "string", "description": "Message to echo back" } }
        }),
        constrained_sampling: None,
    }
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: now_millis(),
    })
}

fn options(key: String) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(key),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn result_for(call: &ToolCall, text: &str) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: now_millis(),
        ..Default::default()
    }))
}

async fn generated_handoff(target_provider: &str, target_model: &str, target_key: String) {
    let copilot_key = resolve_api_key("github-copilot")
        .await
        .expect("copilot gate checked");
    let source = get_model_or_panic("github-copilot", "gpt-5.2-codex");
    let initial = user("Use the echo tool to echo 'hello world'");
    let assistant = complete_simple(
        &source,
        &Context {
            system_prompt: Some(
                "You are a helpful assistant. Use the echo tool when asked.".to_string(),
            ),
            messages: vec![initial.clone()],
            tools: Some(vec![echo_tool()]),
        },
        Some(options(copilot_key)),
    )
    .await;
    assert_eq!(
        assistant.stop_reason,
        StopReason::ToolUse,
        "{:?}",
        assistant.error_message
    );
    let call = assistant
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("Copilot tool call");
    assert!(call.id.contains('|'));
    let response = complete_simple(
        &get_model_or_panic(target_provider, target_model),
        &Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![
                initial,
                Message::Assistant(Box::new(assistant)),
                result_for(&call, "hello world"),
                user("Say hi"),
            ],
            tools: Some(vec![echo_tool()]),
        },
        Some(options(target_key)),
    )
    .await;
    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{:?}",
        response.error_message
    );
    assert!(response.error_message.is_none());
}

fn prefilled_messages() -> Vec<Message> {
    let call = ToolCall {
        id: FAILING_TOOL_CALL_ID.to_string(),
        name: "echo".to_string(),
        arguments: serde_json::Map::from_iter([(
            "message".to_string(),
            serde_json::json!("hello"),
        )]),
        ..Default::default()
    };
    let assistant = AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::ToolCall(call.clone())],
        api: "openai-responses".to_string(),
        provider: "github-copilot".to_string(),
        model: "gpt-5.2-codex".to_string(),
        stop_reason: StopReason::ToolUse,
        timestamp: now_millis() - 1500,
        ..Default::default()
    };
    vec![
        user("Use the echo tool to echo 'hello'"),
        Message::Assistant(Box::new(assistant)),
        result_for(&call, "hello"),
        user("Say hi"),
    ]
}

async fn prefilled_handoff(provider: &str, model: &str, key: String) {
    let response = complete_simple(
        &get_model_or_panic(provider, model),
        &Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: prefilled_messages(),
            tools: Some(vec![echo_tool()]),
        },
        Some(options(key)),
    )
    .await;
    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{:?}",
        response.error_message
    );
    if let Some(error) = response.error_message {
        assert!(!error.contains("call_id"));
        assert!(!error.contains("too long"));
        assert!(!error.contains("additional characters"));
    }
}

#[tokio::test]
async fn tool_call_id_normalization_handoffs() {
    let copilot = resolve_api_key("github-copilot").await;
    let openrouter = live_env("OPENROUTER_API_KEY");
    let codex = resolve_api_key("openai-codex").await;

    if copilot.is_some() && openrouter.is_some() {
        generated_handoff(
            "openrouter",
            "openai/gpt-5.2-codex",
            openrouter.clone().unwrap(),
        )
        .await;
    } else {
        skip(
            "github-copilot -> openrouter tool-call ID normalization",
            "Copilot OAuth + OPENROUTER_API_KEY",
        );
    }
    if copilot.is_some() && codex.is_some() {
        generated_handoff("openai-codex", "gpt-5.5", codex.clone().unwrap()).await;
    } else {
        skip(
            "github-copilot -> openai-codex tool-call ID normalization",
            "Copilot OAuth + Codex OAuth",
        );
    }
    if let Some(key) = openrouter {
        prefilled_handoff("openrouter", "openai/gpt-5.2-codex", key).await;
    } else {
        skip(
            "openrouter prefilled long tool-call ID",
            "OPENROUTER_API_KEY",
        );
    }
    if let Some(key) = codex {
        prefilled_handoff("openai-codex", "gpt-5.5", key).await;
    } else {
        skip("openai-codex prefilled long tool-call ID", "Codex OAuth");
    }
}
