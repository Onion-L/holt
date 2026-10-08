//! Rust entry points for the credential-gated live suites
//! `pi-core/ai/test/tool-call-without-result.test.ts`
//! ("Tool Call Without Result Tests", 30 TS cases) and
//! `pi-core/ai/test/image-tool-result.test.ts`
//! ("Tool Results with Images", 42 active TS cases plus the 4 Xiaomi
//! `it.skip` FIXME cases kept visible).
//!
//! The shared TS bodies (`testToolCallWithoutResult`,
//! `handleToolWithImageResult`, `handleToolWithTextAndImageResult`) are
//! ported below and driven through `common::live::live_complete` (the compat
//! `complete` dispatch plus the TS option extras). Tool schemas are the
//! TypeBox JSON outputs of the TS definitions (`Type.Object({})` becomes
//! `{"type":"object","properties":{}}`).
//!
//! Gates translated verbatim; without credentials each case prints
//! `SKIP: <suite> requires <ENV>` and the test passes.
//!
//! Deviation: vitest `{ retry: 3..5, timeout: 30000 }` has no Rust
//! equivalent.

mod common;

use common::live::{
    self, LiveEffort, LiveOptions, as_openai_completions, empty_object_schema, get_model_or_panic,
    live_complete, live_env, now_millis, red_circle_base64, skip,
};
use pi_core::ai::types::{
    AssistantContent, BlockContent, Context, ImageContent, Message, Model, ModelInput,
    RoleToolResult, RoleUser, StopReason, TextContent, Tool, ToolResultMessage, UserContent,
    UserMessage,
};

fn user_message(content: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp: now_millis(),
    })
}

/// The calculate tool from tool-call-without-result.test.ts.
fn calculate_tool() -> Tool {
    Tool {
        name: "calculate".to_string(),
        description: "Evaluate mathematical expressions".to_string(),
        parameters: serde_json::json!({
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
    }
}

/// testToolCallWithoutResult from tool-call-without-result.test.ts.
async fn test_tool_call_without_result(model: &Model, case: &str, options: &LiveOptions) {
    // Step 1: Create context with the calculate tool
    let mut context = Context {
        system_prompt: Some(
            "You are a helpful assistant. Use the calculate tool when asked to perform calculations."
                .to_string(),
        ),
        messages: Vec::new(),
        tools: Some(vec![calculate_tool()]),
    };

    // Step 2: Ask the LLM to make a tool call
    context.messages.push(user_message(
        "Please calculate 25 * 18 using the calculate tool.",
    ));

    // Step 3: Get the assistant's response (should contain a tool call)
    let first_response = live_complete(model, &context, options).await;
    println!(
        "First response: {}",
        serde_json::to_string_pretty(&first_response).unwrap_or_default()
    );
    context
        .messages
        .push(Message::Assistant(Box::new(first_response.clone())));

    // Verify the response contains a tool call
    let has_tool_call = first_response
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)));
    assert!(
        has_tool_call,
        "{case}: Expected assistant to make a tool call, but none was found"
    );

    // Step 4: Send a user message WITHOUT providing tool result. This
    // simulates the scenario where a tool call was aborted/cancelled.
    context
        .messages
        .push(user_message("Never mind, just tell me what is 2+2?"));

    // Step 5: The fix should filter out the orphaned tool call, and the
    // request should succeed
    let second_response = live_complete(model, &context, options).await;
    println!(
        "Second response: {}",
        serde_json::to_string_pretty(&second_response).unwrap_or_default()
    );

    // The request should succeed (not error) - that's the main thing we're
    // testing
    assert_ne!(
        second_response.stop_reason,
        StopReason::Error,
        "{case}: stopReason"
    );

    // Should have some content in the response
    assert!(!second_response.content.is_empty(), "{case}: content");

    // The LLM may choose to answer directly or make a new tool call - either
    // is fine; the important thing is it didn't fail with the orphaned tool
    // call error.
    let text_content: String = second_response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let tool_calls = second_response
        .content
        .iter()
        .filter(|block| matches!(block, AssistantContent::ToolCall(_)))
        .count();
    assert!(
        tool_calls > 0 || !text_content.is_empty(),
        "{case}: toolCalls || textContent"
    );
    println!("Answer: {text_content}");

    // Verify the stop reason is either "stop" or "toolUse" (new tool call)
    assert!(
        matches!(
            second_response.stop_reason,
            StopReason::Stop | StopReason::ToolUse
        ),
        "{case}: stopReason in [stop, toolUse]"
    );
}

/// handleToolWithImageResult from image-tool-result.test.ts.
async fn handle_tool_with_image_result(model: &Model, case: &str, options: &LiveOptions) {
    // Check if the model supports images
    if !model.input.contains(&ModelInput::Image) {
        println!(
            "Skipping tool image result test - model {} doesn't support images",
            model.id
        );
        return;
    }

    let get_image_tool = Tool {
        name: "get_circle".to_string(),
        description: "Returns a circle image for visualization".to_string(),
        parameters: empty_object_schema(),
        constrained_sampling: None,
    };

    let mut context = Context {
        system_prompt: Some("You are a helpful assistant that uses tools when asked.".to_string()),
        messages: vec![user_message(
            "Call the get_circle tool to get an image, and describe what you see, shapes, colors, etc.",
        )],
        tools: Some(vec![get_image_tool]),
    };

    // First request - LLM should call the tool
    let first_response = live_complete(model, &context, options).await;
    assert_eq!(
        first_response.stop_reason,
        StopReason::ToolUse,
        "{case}: stopReason"
    );

    // Find the tool call
    let tool_call = first_response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{case}: Expected tool call"));
    assert_eq!(tool_call.name, "get_circle", "{case}: name");
    let (tool_call_id, tool_name) = (tool_call.id.clone(), tool_call.name.clone());

    // Add the tool call to context
    context
        .messages
        .push(Message::Assistant(Box::new(first_response)));

    // Create tool result with ONLY an image (no text)
    let tool_result = ToolResultMessage {
        role: RoleToolResult,
        tool_call_id,
        tool_name,
        content: vec![BlockContent::Image(ImageContent {
            data: red_circle_base64(),
            mime_type: "image/png".to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: now_millis(),
        ..Default::default()
    };
    context
        .messages
        .push(Message::ToolResult(Box::new(tool_result)));

    // Second request - LLM should describe the image from the tool result
    let second_response = live_complete(model, &context, options).await;
    assert_eq!(
        second_response.stop_reason,
        StopReason::Stop,
        "{case}: stopReason"
    );
    assert!(
        second_response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        second_response.error_message
    );

    // Verify the LLM can see and describe the image
    let text_content = second_response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::Text(text) => Some(text),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{case}: text content"));
    let lower_content = text_content.text.to_lowercase();
    // Should mention red and circle since that's what the image shows
    assert!(lower_content.contains("red"), "{case}: contains 'red'");
    assert!(
        lower_content.contains("circle"),
        "{case}: contains 'circle'"
    );
}

/// handleToolWithTextAndImageResult from image-tool-result.test.ts.
async fn handle_tool_with_text_and_image_result(model: &Model, case: &str, options: &LiveOptions) {
    // Check if the model supports images
    if !model.input.contains(&ModelInput::Image) {
        println!(
            "Skipping tool text+image result test - model {} doesn't support images",
            model.id
        );
        return;
    }

    let get_image_tool = Tool {
        name: "get_circle_with_description".to_string(),
        description: "Returns a circle image with a text description".to_string(),
        parameters: empty_object_schema(),
        constrained_sampling: None,
    };

    let mut context = Context {
        system_prompt: Some("You are a helpful assistant that uses tools when asked.".to_string()),
        messages: vec![user_message(
            "Use the get_circle_with_description tool and tell me what you learned. Also say what color the shape is.",
        )],
        tools: Some(vec![get_image_tool]),
    };

    // First request - LLM should call the tool
    let first_response = live_complete(model, &context, options).await;
    assert_eq!(
        first_response.stop_reason,
        StopReason::ToolUse,
        "{case}: stopReason"
    );

    // Find the tool call
    let tool_call = first_response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{case}: Expected tool call"));
    assert_eq!(
        tool_call.name, "get_circle_with_description",
        "{case}: name"
    );
    let (tool_call_id, tool_name) = (tool_call.id.clone(), tool_call.name.clone());

    // Add the tool call to context
    context
        .messages
        .push(Message::Assistant(Box::new(first_response)));

    // Create tool result with BOTH text and image
    let tool_result = ToolResultMessage {
        role: RoleToolResult,
        tool_call_id,
        tool_name,
        content: vec![
            BlockContent::Text(TextContent {
                text: "This is a geometric shape with specific properties: it has a diameter of 100 pixels."
                    .to_string(),
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
    };
    context
        .messages
        .push(Message::ToolResult(Box::new(tool_result)));

    // Second request - LLM should describe both the text and image from the
    // tool result
    let second_response = live_complete(model, &context, options).await;
    assert_eq!(
        second_response.stop_reason,
        StopReason::Stop,
        "{case}: stopReason"
    );
    assert!(
        second_response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        second_response.error_message
    );

    // Verify the LLM can see both text and image
    let text_content = second_response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::Text(text) => Some(text),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{case}: text content"));
    let lower_content = text_content.text.to_lowercase();
    // Should mention details from the text (diameter/pixels)
    assert!(
        lower_content.contains("diameter")
            || lower_content.contains("100")
            || lower_content.contains("pixel"),
        "{case}: mentions diameter|100|pixel"
    );
    // Should also mention the visual properties (red and circle)
    assert!(lower_content.contains("red"), "{case}: contains 'red'");
    assert!(
        lower_content.contains("circle"),
        "{case}: contains 'circle'"
    );
}

// ---------------------------------------------------------------------------
// tool-call-without-result.test.ts (30 cases)
// ---------------------------------------------------------------------------

struct WithoutResultSuite {
    case: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    reasoning_effort_high: bool,
}

fn env_gated(provider: &str, env: &str) -> bool {
    if provider == "azure-openai-responses" {
        live::has_azure_openai_credentials()
    } else if provider == "amazon-bedrock" {
        live::has_bedrock_credentials()
    } else if provider == "cloudflare-workers-ai" {
        live::has_cloudflare_workers_ai_credentials()
    } else if provider == "cloudflare-ai-gateway" {
        live::has_cloudflare_ai_gateway_credentials()
    } else {
        live_env(env).is_some()
    }
}

fn resolve_model(provider: &str, model_id: &str) -> Model {
    let model = get_model_or_panic(provider, model_id);
    if provider == "openai" && model_id == "gpt-4o-mini" {
        as_openai_completions(&model)
    } else {
        model
    }
}

#[tokio::test]
async fn tool_call_without_result_env_provider_matrix() {
    let suites = [
        WithoutResultSuite {
            case: "Google Provider",
            provider: "google",
            model: "gemini-2.5-flash",
            env: "GEMINI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "OpenAI Completions Provider",
            provider: "openai",
            model: "gpt-4o-mini",
            env: "OPENAI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "OpenAI Responses Provider",
            provider: "openai",
            model: "gpt-5-mini",
            env: "OPENAI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Azure OpenAI Responses Provider",
            provider: "azure-openai-responses",
            model: "gpt-4o-mini",
            env: "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Anthropic Provider",
            provider: "anthropic",
            model: "claude-haiku-4-5",
            env: "ANTHROPIC_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "xAI Provider",
            provider: "xai",
            model: "grok-4.3",
            env: "XAI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Groq Provider",
            provider: "groq",
            model: "openai/gpt-oss-20b",
            env: "GROQ_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Cerebras Provider",
            provider: "cerebras",
            model: "gpt-oss-120b",
            env: "CEREBRAS_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Cloudflare Workers AI Provider",
            provider: "cloudflare-workers-ai",
            model: "@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Cloudflare AI Gateway Provider",
            provider: "cloudflare-ai-gateway",
            model: "workers-ai/@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Hugging Face Provider",
            provider: "huggingface",
            model: "moonshotai/Kimi-K2.5",
            env: "HF_TOKEN",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Together AI Provider",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            reasoning_effort_high: true,
        },
        WithoutResultSuite {
            case: "Baseten Provider",
            provider: "baseten",
            model: "zai-org/GLM-5.2",
            env: "BASETEN_API_KEY",
            reasoning_effort_high: true,
        },
        WithoutResultSuite {
            case: "zAI Provider",
            provider: "zai",
            model: "glm-5.2",
            env: "ZAI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Mistral Provider",
            provider: "mistral",
            model: "devstral-medium-latest",
            env: "MISTRAL_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "MiniMax Provider",
            provider: "minimax",
            model: "MiniMax-M2.7",
            env: "MINIMAX_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Xiaomi MiMo (API billing) Provider",
            provider: "xiaomi",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Xiaomi MiMo Token Plan (CN) Provider",
            provider: "xiaomi-token-plan-cn",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Xiaomi MiMo Token Plan (AMS) Provider",
            provider: "xiaomi-token-plan-ams",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Xiaomi MiMo Token Plan (SGP) Provider",
            provider: "xiaomi-token-plan-sgp",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Qwen Token Plan Provider",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Qwen Token Plan Individual Provider",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Qwen Token Plan (CN) Provider",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Kimi For Coding Provider",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Vercel AI Gateway Provider",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            reasoning_effort_high: false,
        },
        WithoutResultSuite {
            case: "Amazon Bedrock Provider",
            provider: "amazon-bedrock",
            model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            env: "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
            reasoning_effort_high: false,
        },
    ];

    for suite in suites {
        if !env_gated(suite.provider, suite.env) {
            skip(
                &format!(
                    "{}: should filter out tool calls without corresponding tool results",
                    suite.case
                ),
                suite.env,
            );
            continue;
        }
        let model = resolve_model(suite.provider, suite.model);
        let mut options = LiveOptions::default();
        if suite.reasoning_effort_high {
            options.reasoning_effort = Some(LiveEffort::High);
        }
        if suite.provider == "azure-openai-responses" {
            options.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
        }
        test_tool_call_without_result(
            &model,
            &format!(
                "{}: should filter out tool calls without corresponding tool results",
                suite.case
            ),
            &options,
        )
        .await;
    }
}

#[tokio::test]
async fn tool_call_without_result_oauth_providers() {
    // Anthropic OAuth
    let anthropic_token = live::resolve_api_key("anthropic").await;
    if anthropic_token.is_none() {
        skip(
            "Anthropic OAuth Provider: should filter out tool calls without corresponding tool results",
            "~/.pi/agent/auth.json anthropic credentials",
        );
    } else {
        let model = get_model_or_panic("anthropic", "claude-haiku-4-5");
        let options = LiveOptions::with_api_key(anthropic_token.as_deref().unwrap_or_default());
        test_tool_call_without_result(
            &model,
            "Anthropic OAuth Provider: should filter out tool calls without corresponding tool results",
            &options,
        )
        .await;
    }

    // GitHub Copilot (two models)
    let copilot_token = live::resolve_api_key("github-copilot").await;
    if copilot_token.is_none() {
        skip(
            "GitHub Copilot Provider: should filter out tool calls without corresponding tool results",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
    } else {
        for (label, model_id) in [
            ("claude-haiku-4.5", "claude-haiku-4.5"),
            ("claude-sonnet-4", "claude-sonnet-4.6"),
        ] {
            let model = get_model_or_panic("github-copilot", model_id);
            let options = LiveOptions::with_api_key(copilot_token.as_deref().unwrap_or_default());
            test_tool_call_without_result(
                &model,
                &format!(
                    "GitHub Copilot Provider ({label}): should filter out tool calls without corresponding tool results"
                ),
                &options,
            )
            .await;
        }
    }

    // OpenAI Codex
    let codex_token = live::resolve_api_key("openai-codex").await;
    if codex_token.is_none() {
        skip(
            "OpenAI Codex Provider: should filter out tool calls without corresponding tool results",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
    } else {
        let model = get_model_or_panic("openai-codex", "gpt-5.5");
        let options = LiveOptions::with_api_key(codex_token.as_deref().unwrap_or_default());
        test_tool_call_without_result(
            &model,
            "OpenAI Codex Provider (gpt-5.5): should filter out tool calls without corresponding tool results",
            &options,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// image-tool-result.test.ts (42 active cases)
// ---------------------------------------------------------------------------

struct ImageResultSuite {
    case: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    reasoning_effort_high: bool,
}

async fn run_image_suite(suite: ImageResultSuite) {
    if !env_gated(suite.provider, suite.env) {
        skip(suite.case, suite.env);
        return;
    }
    let model = resolve_model(suite.provider, suite.model);
    let mut options = LiveOptions::default();
    if suite.reasoning_effort_high {
        options.reasoning_effort = Some(LiveEffort::High);
    }
    if suite.provider == "azure-openai-responses" {
        options.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
    }
    handle_tool_with_image_result(
        &model,
        &format!("{}: should handle tool result with only image", suite.case),
        &options,
    )
    .await;
    handle_tool_with_text_and_image_result(
        &model,
        &format!(
            "{}: should handle tool result with text and image",
            suite.case
        ),
        &options,
    )
    .await;
}

#[tokio::test]
async fn image_tool_result_env_provider_matrix() {
    let suites = [
        ImageResultSuite {
            case: "Google Provider (gemini-2.5-flash)",
            provider: "google",
            model: "gemini-2.5-flash",
            env: "GEMINI_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "OpenAI Completions Provider (gpt-4o-mini)",
            provider: "openai",
            model: "gpt-4o-mini",
            env: "OPENAI_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "OpenAI Responses Provider (gpt-5-mini)",
            provider: "openai",
            model: "gpt-5-mini",
            env: "OPENAI_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Azure OpenAI Responses Provider (gpt-4o-mini)",
            provider: "azure-openai-responses",
            model: "gpt-4o-mini",
            env: "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Anthropic Provider (claude-haiku-4-5)",
            provider: "anthropic",
            model: "claude-haiku-4-5",
            env: "ANTHROPIC_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "OpenRouter Provider (glm-4.5v)",
            provider: "openrouter",
            model: "z-ai/glm-4.5v",
            env: "OPENROUTER_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Mistral Provider (pixtral-12b)",
            provider: "mistral",
            model: "pixtral-12b",
            env: "MISTRAL_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Together AI Provider (Kimi-K2.6)",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            reasoning_effort_high: true,
        },
        ImageResultSuite {
            case: "Baseten Provider (Kimi-K2.6)",
            provider: "baseten",
            model: "moonshotai/Kimi-K2.6",
            env: "BASETEN_API_KEY",
            reasoning_effort_high: true,
        },
        ImageResultSuite {
            case: "Qwen Token Plan Provider (qwen3.7-max)",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Qwen Token Plan Individual Provider (qwen3.8-max)",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Qwen Token Plan (CN) Provider (qwen3.7-max)",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Kimi For Coding Provider (kimi-for-coding)",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Vercel AI Gateway Provider (google/gemini-2.5-flash)",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            reasoning_effort_high: false,
        },
        ImageResultSuite {
            case: "Amazon Bedrock Provider (claude-sonnet-4-5)",
            provider: "amazon-bedrock",
            model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            env: "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
            reasoning_effort_high: false,
        },
    ];

    for suite in suites {
        run_image_suite(suite).await;
    }

    // Xiaomi suites: only the image-only case is active in the TS source; the
    // text+image case is it.skip with the FIXME(xiaomi) multimodal-fusion
    // note (MiMo locks onto the text and ignores the image).
    let xiaomi_suites = [
        ("xiaomi", "XIAOMI_API_KEY"),
        ("xiaomi-token-plan-cn", "XIAOMI_TOKEN_PLAN_CN_API_KEY"),
        ("xiaomi-token-plan-ams", "XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
        ("xiaomi-token-plan-sgp", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
    ];
    for (provider, env) in xiaomi_suites {
        if live_env(env).is_none() {
            skip(&format!("{provider} image tool result"), env);
            continue;
        }
        let model = get_model_or_panic(provider, "mimo-v2.5-pro");
        handle_tool_with_image_result(
            &model,
            &format!("{provider}: should handle tool result with only image"),
            &LiveOptions::default(),
        )
        .await;
        eprintln!(
            "SKIP: {provider}: should handle tool result with text and image — TS it.skip FIXME(xiaomi): MiMo ignores the image when the tool result mixes text and image"
        );
    }
}

#[tokio::test]
async fn image_tool_result_oauth_providers() {
    // Anthropic OAuth (claude-sonnet-4-5)
    let anthropic_token = live::resolve_api_key("anthropic").await;
    if anthropic_token.is_none() {
        skip(
            "Anthropic OAuth Provider (claude-sonnet-4-5): tool result images",
            "~/.pi/agent/auth.json anthropic credentials",
        );
    } else {
        let model = get_model_or_panic("anthropic", "claude-sonnet-4-5");
        let options = LiveOptions::with_api_key(anthropic_token.as_deref().unwrap_or_default());
        handle_tool_with_image_result(
            &model,
            "Anthropic OAuth Provider (claude-sonnet-4-5): should handle tool result with only image",
            &options,
        )
        .await;
        handle_tool_with_text_and_image_result(
            &model,
            "Anthropic OAuth Provider (claude-sonnet-4-5): should handle tool result with text and image",
            &options,
        )
        .await;
    }

    // GitHub Copilot (two models, both cases each)
    let copilot_token = live::resolve_api_key("github-copilot").await;
    if copilot_token.is_none() {
        skip(
            "GitHub Copilot Provider: tool result images",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
    } else {
        let options = LiveOptions::with_api_key(copilot_token.as_deref().unwrap_or_default());
        for (label, model_id) in [
            ("claude-haiku-4.5", "claude-haiku-4.5"),
            ("claude-sonnet-4", "claude-sonnet-4.6"),
        ] {
            let model = get_model_or_panic("github-copilot", model_id);
            handle_tool_with_image_result(
                &model,
                &format!(
                    "GitHub Copilot Provider ({label}): should handle tool result with only image"
                ),
                &options,
            )
            .await;
            handle_tool_with_text_and_image_result(
                &model,
                &format!(
                    "GitHub Copilot Provider ({label}): should handle tool result with text and image"
                ),
                &options,
            )
            .await;
        }
    }

    // OpenAI Codex (gpt-5.5, both cases)
    let codex_token = live::resolve_api_key("openai-codex").await;
    if codex_token.is_none() {
        skip(
            "OpenAI Codex Provider (gpt-5.5): tool result images",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
    } else {
        let model = get_model_or_panic("openai-codex", "gpt-5.5");
        let options = LiveOptions::with_api_key(codex_token.as_deref().unwrap_or_default());
        handle_tool_with_image_result(
            &model,
            "OpenAI Codex Provider (gpt-5.5): should handle tool result with only image",
            &options,
        )
        .await;
        handle_tool_with_text_and_image_result(
            &model,
            "OpenAI Codex Provider (gpt-5.5): should handle tool result with text and image",
            &options,
        )
        .await;
    }
}
