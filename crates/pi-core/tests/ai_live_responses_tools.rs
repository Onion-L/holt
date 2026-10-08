//! Credential-gated entry for
//! `openai-responses-tool-result-images.test.ts`.

mod common;

use common::live::{
    LiveEffort, LiveOptions, get_model_or_panic, live_complete, live_env, now_millis,
    payload_capture, red_circle_base64, resolve_api_key, skip,
};
use pi_core::ai::types::{
    AssistantContent, BlockContent, Context, ImageContent, Message, Model, ModelInput,
    RoleToolResult, RoleUser, StopReason, TextContent, Tool, ToolResultMessage, UserContent,
    UserMessage,
};

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: now_millis(),
    })
}

fn image_tool() -> Tool {
    Tool {
        name: "get_circle_with_description".to_string(),
        description: "Returns a red circle image with a short text description.".to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
    }
}

async fn verify_tool_result_image(model: &Model, options: LiveOptions) {
    if !model.input.contains(&ModelInput::Image) {
        println!(
            "Skipping responses tool-result image test. Model {} does not support images.",
            model.id
        );
        return;
    }
    let mut context = Context {
        system_prompt: Some(
            "You are a helpful assistant that always uses the provided tool when asked."
                .to_string(),
        ),
        messages: vec![user(
            "Call get_circle_with_description, then describe both the tool text and the image. Mention the color and shape.",
        )],
        tools: Some(vec![image_tool()]),
    };
    let first = live_complete(model, &context, &options).await;
    assert_eq!(
        first.stop_reason,
        StopReason::ToolUse,
        "{:?}",
        first.error_message
    );
    let call = first
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("tool call");
    context.messages.push(Message::Assistant(Box::new(first)));
    let tool_text = "A red circle with a diameter of 100 pixels.";
    context
        .messages
        .push(Message::ToolResult(Box::new(ToolResultMessage {
            role: RoleToolResult,
            tool_call_id: call.id,
            tool_name: call.name,
            content: vec![
                BlockContent::Text(TextContent {
                    text: tool_text.to_string(),
                    ..Default::default()
                }),
                BlockContent::Image(ImageContent {
                    data: red_circle_base64(),
                    mime_type: "image/png".to_string(),
                    ..Default::default()
                }),
            ],
            is_error: false,
            timestamp: now_millis(),
            ..Default::default()
        })));
    let (captured, on_payload) = payload_capture();
    let second = live_complete(
        model,
        &context,
        &LiveOptions {
            on_payload: Some(on_payload),
            ..options
        },
    )
    .await;
    assert_eq!(
        second.stop_reason,
        StopReason::Stop,
        "{:?}",
        second.error_message
    );
    assert!(second.error_message.is_none());
    let payload = captured.lock().unwrap().clone().expect("captured payload");
    let input = payload["input"].as_array().expect("Responses input array");
    let output_index = input
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .expect("function_call_output");
    let output = input[output_index]["output"]
        .as_array()
        .expect("content-array function output");
    assert!(output.iter().any(|item| {
        item["type"] == "input_text"
            && item["text"]
                .as_str()
                .is_some_and(|text| text.contains(tool_text))
    }));
    assert!(output.iter().any(|item| {
        item["type"] == "input_image"
            && item["image_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
    }));
    assert!(
        !input[output_index + 1..]
            .iter()
            .any(|item| item["role"] == "user")
    );
    let text = second
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(block) => Some(block.text.to_ascii_lowercase()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("red"));
    assert!(text.contains("circle"));
}

#[tokio::test]
async fn responses_tool_result_images_provider_matrix() {
    if live_env("OPENAI_API_KEY").is_some() {
        verify_tool_result_image(
            &get_model_or_panic("openai", "gpt-5-mini"),
            LiveOptions {
                reasoning_effort: Some(LiveEffort::Low),
                ..Default::default()
            },
        )
        .await;
    } else {
        skip("OpenAI Responses tool result images", "OPENAI_API_KEY");
    }

    if common::live::has_azure_openai_credentials() {
        let model = get_model_or_panic("azure-openai-responses", "gpt-4o-mini");
        verify_tool_result_image(
            &model,
            LiveOptions {
                azure_deployment_name: common::live::resolve_azure_deployment_name(&model.id),
                ..Default::default()
            },
        )
        .await;
    } else {
        skip(
            "Azure OpenAI Responses tool result images",
            "Azure OpenAI credentials",
        );
    }

    if let Some(token) = resolve_api_key("github-copilot").await {
        verify_tool_result_image(
            &get_model_or_panic("github-copilot", "gpt-5-mini"),
            LiveOptions {
                api_key: Some(token),
                reasoning_effort: Some(LiveEffort::Low),
                ..Default::default()
            },
        )
        .await;
    } else {
        skip(
            "GitHub Copilot Responses tool result images",
            "github-copilot OAuth",
        );
    }

    if let Some(token) = resolve_api_key("openai-codex").await {
        verify_tool_result_image(
            &get_model_or_panic("openai-codex", "gpt-5.5"),
            LiveOptions {
                api_key: Some(token),
                reasoning_effort: Some(LiveEffort::Low),
                ..Default::default()
            },
        )
        .await;
    } else {
        skip(
            "OpenAI Codex Responses tool result images",
            "openai-codex OAuth",
        );
    }
}
