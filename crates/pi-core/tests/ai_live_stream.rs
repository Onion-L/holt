//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/stream.test.ts` ("Generate E2E Tests").
//!
//! Each TypeScript `describe(provider + model)` becomes one `#[tokio::test]`;
//! the gate is the exact `describe.skipIf` / `it.skipIf` condition from the
//! TypeScript source (read via `std::env::var` through `common::live`).
//! Without credentials the suite prints `SKIP: <suite> requires <ENV>` and
//! returns (the test passes). With credentials the same real requests and
//! assertions run through `common::live::live_stream`/`live_complete`, the
//! `compat.ts` `stream()`/`complete()` dispatch plus the TypeScript
//! `StreamOptions` extras.
//!
//! Deviations from the TypeScript suite (none affect the SKIP behavior):
//! - vitest `{ retry: N }` test retries have no Rust harness equivalent; each
//!   case runs once.
//! - On the Cloudflare dispatch path (`builtinModels().stream`) the Rust
//!   `Models::stream` cannot carry the TS option extras, so the
//!   thinking-related flags are dropped for the Cloudflare thinking/multi-turn
//!   cases (noted inline).
//! - The Ollama `beforeAll` server readiness poll uses a raw HTTP GET over
//!   `TcpStream` instead of `fetch`, and a bounded wait.

mod common;

use common::live::{
    self, LiveEffort, LiveOptions, LiveThinking, Scenario, as_openai_completions, calculator_tool,
    get_model_or_panic, js_number_to_string, live_complete, live_env, live_stream, now_millis,
    skip,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessageEvent, BlockContent, Context, ImageContent, Message, Model,
    ModelInput, RoleAssistant, RoleToolResult, RoleUser, StopReason, TextContent,
    ToolResultMessage, UserContent, UserMessage,
};

fn user_message(content: impl Into<String>) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.into()),
        timestamp: now_millis(),
    })
}

/// `content.map((b) => (b.type === "text" ? b.text : "")).join("")`.
fn text_of(content: &[AssistantContent]) -> String {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text(text) => text.text.as_str(),
            _ => "",
        })
        .collect()
}

/// basicTextGeneration from stream.test.ts.
async fn basic_text_generation(model: &Model, case: &str, options: &LiveOptions) {
    let mut context = Context {
        system_prompt: Some("You are a helpful assistant. Be concise.".to_string()),
        messages: vec![user_message("Reply with exactly: 'Hello test successful'")],
        tools: None,
    };
    let response = live_complete(model, &context, options).await;

    assert_eq!(response.role, RoleAssistant, "{case}: role");
    assert!(!response.content.is_empty(), "{case}: content");
    assert!(
        response.usage.input + response.usage.cache_read > 0,
        "{case}: usage.input + usage.cacheRead > 0"
    );
    assert!(response.usage.output > 0, "{case}: usage.output > 0");
    assert!(
        response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        response.error_message
    );
    assert!(
        text_of(&response.content).contains("Hello test successful"),
        "{case}: text contains 'Hello test successful': {}",
        text_of(&response.content)
    );

    context
        .messages
        .push(Message::Assistant(Box::new(response)));
    context
        .messages
        .push(user_message("Now say 'Goodbye test successful'"));

    let second_response = live_complete(model, &context, options).await;

    assert_eq!(second_response.role, RoleAssistant, "{case}: role");
    assert!(!second_response.content.is_empty(), "{case}: content");
    assert!(
        second_response.usage.input + second_response.usage.cache_read > 0,
        "{case}: usage.input + usage.cacheRead > 0"
    );
    assert!(second_response.usage.output > 0, "{case}: usage.output > 0");
    assert!(
        second_response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        second_response.error_message
    );
    assert!(
        text_of(&second_response.content).contains("Goodbye test successful"),
        "{case}: text contains 'Goodbye test successful': {}",
        text_of(&second_response.content)
    );
}

/// handleToolCall from stream.test.ts.
async fn handle_tool_call(model: &Model, case: &str, options: &LiveOptions) {
    let context = Context {
        system_prompt: Some("You are a helpful assistant that uses tools when asked.".to_string()),
        messages: vec![user_message(
            "Calculate 15 + 27 using the math_operation tool.",
        )],
        tools: Some(vec![calculator_tool()]),
    };

    let stream = live_stream(model, &context, options);
    let mut has_tool_start = false;
    let mut has_tool_delta = false;
    let mut has_tool_end = false;
    let mut accumulated_tool_args = String::new();
    let mut index = 0usize;
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::ToolcallStart {
                content_index,
                partial,
            } => {
                has_tool_start = true;
                let tool_call = partial
                    .content
                    .get(*content_index)
                    .unwrap_or_else(|| panic!("{case}: toolcall_start partial missing content"));
                index = *content_index;
                if let AssistantContent::ToolCall(tool_call) = tool_call {
                    assert_eq!(tool_call.name, "math_operation", "{case}: name");
                    assert!(!tool_call.id.is_empty(), "{case}: id");
                } else {
                    panic!("{case}: toolcall_start content is not a toolCall");
                }
            }
            AssistantMessageEvent::ToolcallDelta {
                content_index,
                delta,
                partial,
            } => {
                has_tool_delta = true;
                let tool_call = partial
                    .content
                    .get(*content_index)
                    .unwrap_or_else(|| panic!("{case}: toolcall_delta partial missing content"));
                assert_eq!(*content_index, index, "{case}: contentIndex");
                if let AssistantContent::ToolCall(tool_call) = tool_call {
                    assert_eq!(tool_call.name, "math_operation", "{case}: name");
                    accumulated_tool_args.push_str(delta);
                    // TS asserts `arguments` is a defined, non-null object
                    // during streaming; the Rust `ToolCall.arguments` is a
                    // parsed JSON object by construction.
                } else {
                    panic!("{case}: toolcall_delta content is not a toolCall");
                }
            }
            AssistantMessageEvent::ToolcallEnd {
                content_index,
                partial,
                ..
            } => {
                has_tool_end = true;
                let tool_call = partial
                    .content
                    .get(*content_index)
                    .unwrap_or_else(|| panic!("{case}: toolcall_end partial missing content"));
                assert_eq!(*content_index, index, "{case}: contentIndex");
                if let AssistantContent::ToolCall(tool_call) = tool_call {
                    assert_eq!(tool_call.name, "math_operation", "{case}: name");
                    serde_json::from_str::<serde_json::Value>(&accumulated_tool_args)
                        .unwrap_or_else(|error| {
                            panic!("{case}: accumulated tool args are not JSON ({error})")
                        });
                    // TS asserts `arguments` is not undefined; the Rust field
                    // is a parsed object by construction.
                    assert_eq!(
                        tool_call
                            .arguments
                            .get("a")
                            .and_then(|value| value.as_f64()),
                        Some(15.0),
                        "{case}: arguments.a"
                    );
                    assert_eq!(
                        tool_call
                            .arguments
                            .get("b")
                            .and_then(|value| value.as_f64()),
                        Some(27.0),
                        "{case}: arguments.b"
                    );
                    let operation = tool_call
                        .arguments
                        .get("operation")
                        .and_then(|value| value.as_str());
                    assert!(
                        matches!(operation, Some("add" | "subtract" | "multiply" | "divide")),
                        "{case}: arguments.operation: {operation:?}"
                    );
                } else {
                    panic!("{case}: toolcall_end content is not a toolCall");
                }
            }
            _ => {}
        }
    }

    assert!(has_tool_start, "{case}: hasToolStart");
    assert!(has_tool_delta, "{case}: hasToolDelta");
    assert!(has_tool_end, "{case}: hasToolEnd");

    let response = stream.result().await;
    assert_eq!(
        response.stop_reason,
        StopReason::ToolUse,
        "{case}: stopReason"
    );
    let tool_call = response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(tool_call) => Some(tool_call),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{case}: No tool call found in response"));
    assert_eq!(tool_call.name, "math_operation", "{case}: name");
    assert!(!tool_call.id.is_empty(), "{case}: id");
}

/// handleStreaming from stream.test.ts.
async fn handle_streaming(model: &Model, case: &str, options: &LiveOptions) {
    let mut text_started = false;
    let mut text_chunks = String::new();
    let mut text_completed = false;

    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message("Count from 1 to 3")],
        tools: None,
    };

    let stream = live_stream(model, &context, options);
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::TextStart { .. } => text_started = true,
            AssistantMessageEvent::TextDelta { delta, .. } => text_chunks.push_str(delta),
            AssistantMessageEvent::TextEnd { .. } => text_completed = true,
            _ => {}
        }
    }
    let response = stream.result().await;

    assert!(text_started, "{case}: textStarted");
    assert!(!text_chunks.is_empty(), "{case}: textChunks");
    assert!(text_completed, "{case}: textCompleted");
    assert!(
        response
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Text(_))),
        "{case}: response has text content"
    );
}

/// handleThinking from stream.test.ts.
async fn handle_thinking(model: &Model, case: &str, options: &LiveOptions) {
    let mut thinking_started = false;
    let mut thinking_chunks = String::new();
    let mut thinking_completed = false;

    // TS embeds `(Math.random() * 255) | 0`; any small number works.
    let prompt_number = now_millis().rem_euclid(255);
    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message(format!(
            "Think long and hard about {prompt_number} + 27. Think step by step. Then output the result."
        ))],
        tools: None,
    };

    let stream = live_stream(model, &context, options);
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::ThinkingStart { .. } => thinking_started = true,
            AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking_chunks.push_str(delta),
            AssistantMessageEvent::ThinkingEnd { .. } => thinking_completed = true,
            _ => {}
        }
    }
    let response = stream.result().await;

    assert_eq!(
        response.stop_reason,
        StopReason::Stop,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(thinking_started, "{case}: thinkingStarted");
    assert!(!thinking_chunks.is_empty(), "{case}: thinkingChunks");
    assert!(thinking_completed, "{case}: thinkingCompleted");
    assert!(
        response
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Thinking(_))),
        "{case}: response has thinking content"
    );
}

/// handleImage from stream.test.ts.
async fn handle_image(model: &Model, case: &str, options: &LiveOptions) {
    if !model.input.contains(&ModelInput::Image) {
        println!(
            "Skipping image test - model {} doesn't support images",
            model.id
        );
        return;
    }

    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![
                BlockContent::Text(TextContent {
                    text: "What do you see in this image? Please describe the shape (circle, rectangle, square, triangle, ...) and color (red, blue, green, ...). You MUST reply in English.".to_string(),
                    ..Default::default()
                }),
                BlockContent::Image(ImageContent {
                    data: live::red_circle_base64(),
                    mime_type: "image/png".to_string(),
                    ..Default::default()
                }),
            ]),
            timestamp: now_millis(),
        })],
        tools: None,
    };

    let response = live_complete(model, &context, options).await;

    assert!(!response.content.is_empty(), "{case}: content");
    // TS only checks the text block when one exists.
    if let Some(AssistantContent::Text(text_content)) = response
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::Text(_)))
    {
        let lower_content = text_content.text.to_lowercase();
        assert!(lower_content.contains("red"), "{case}: contains 'red'");
        assert!(
            lower_content.contains("circle"),
            "{case}: contains 'circle'"
        );
    }
}

/// multiTurn from stream.test.ts.
async fn multi_turn(model: &Model, case: &str, options: &LiveOptions) {
    let mut context = Context {
        system_prompt: Some(
            "You are a helpful assistant that can use tools to answer questions.".to_string(),
        ),
        messages: vec![user_message(
            "Think about this briefly, then calculate 42 * 17 and 453 + 434 using the math_operation tool.",
        )],
        tools: Some(vec![calculator_tool()]),
    };

    let mut all_text_content = String::new();
    let mut has_seen_thinking = false;
    let mut has_seen_tool_calls = false;
    let max_turns = 5;

    for _turn in 0..max_turns {
        let response = live_complete(model, &context, options).await;

        context
            .messages
            .push(Message::Assistant(Box::new(response.clone())));

        let mut results: Vec<ToolResultMessage> = Vec::new();
        for block in &response.content {
            match block {
                AssistantContent::Text(text) => all_text_content.push_str(&text.text),
                AssistantContent::Thinking(_) => has_seen_thinking = true,
                AssistantContent::ToolCall(tool_call) => {
                    has_seen_tool_calls = true;
                    assert_eq!(tool_call.name, "math_operation", "{case}: name");
                    assert!(!tool_call.id.is_empty(), "{case}: id");
                    let a = tool_call.arguments.get("a").and_then(|v| v.as_f64());
                    let b = tool_call.arguments.get("b").and_then(|v| v.as_f64());
                    let operation = tool_call
                        .arguments
                        .get("operation")
                        .and_then(|v| v.as_str());
                    let result = match (a, b, operation) {
                        (Some(a), Some(b), Some("add")) => a + b,
                        (Some(a), Some(b), Some("multiply")) => a * b,
                        _ => 0.0,
                    };
                    results.push(ToolResultMessage {
                        role: RoleToolResult,
                        tool_call_id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                        content: vec![BlockContent::Text(TextContent {
                            text: js_number_to_string(result),
                            ..Default::default()
                        })],
                        is_error: false,
                        timestamp: now_millis(),
                        ..Default::default()
                    });
                }
            }
        }
        for result in results {
            context.messages.push(Message::ToolResult(Box::new(result)));
        }

        assert_ne!(
            response.stop_reason,
            StopReason::Error,
            "{case}: stopReason, error: {:?}",
            response.error_message
        );
        if response.stop_reason == StopReason::Stop {
            break;
        }
    }

    assert!(
        has_seen_thinking || has_seen_tool_calls,
        "{case}: hasSeenThinking || hasSeenToolCalls"
    );
    assert!(!all_text_content.is_empty(), "{case}: allTextContent");
    assert!(
        all_text_content.contains("714"),
        "{case}: text includes 714: {all_text_content}"
    );
    assert!(
        all_text_content.contains("887"),
        "{case}: text includes 887: {all_text_content}"
    );
}

/// Runs one TS `it` body against the model.
async fn run_case(kind: Scenario, model: &Model, case: &str, options: &LiveOptions) {
    match kind {
        Scenario::Basic => basic_text_generation(model, case, options).await,
        Scenario::ToolCall => handle_tool_call(model, case, options).await,
        Scenario::Streaming => handle_streaming(model, case, options).await,
        Scenario::Thinking => handle_thinking(model, case, options).await,
        Scenario::MultiTurn => multi_turn(model, case, options).await,
        Scenario::Image => handle_image(model, case, options).await,
    }
}

fn case(name: &str, kind: Scenario) -> Case {
    Case {
        name: name.to_string(),
        kind,
        options: LiveOptions::default(),
    }
}

struct Case {
    name: String,
    kind: Scenario,
    options: LiveOptions,
}

async fn run_cases(model: &Model, label: &str, cases: Vec<Case>) {
    for entry in cases {
        let case_name = format!("{label}: {}", entry.name);
        run_case(entry.kind, model, &case_name, &entry.options).await;
    }
}

fn thinking_options(enabled: bool, budget_tokens: u64) -> LiveOptions {
    LiveOptions {
        thinking: Some(LiveThinking {
            enabled,
            budget_tokens: Some(budget_tokens as i64),
            level: None,
        }),
        ..Default::default()
    }
}

fn effort_options(effort: LiveEffort) -> LiveOptions {
    LiveOptions {
        reasoning_effort: Some(effort),
        ..Default::default()
    }
}

#[tokio::test]
async fn gemini_provider_gemini_2_5_flash() {
    // TS: describe.skipIf(!process.env.GEMINI_API_KEY)
    if live_env("GEMINI_API_KEY").is_none() {
        skip("Gemini Provider (gemini-2.5-flash)", "GEMINI_API_KEY");
        return;
    }
    let llm = get_model_or_panic("google", "gemini-2.5-flash");
    run_cases(
        &llm,
        "google/gemini-2.5-flash",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: thinking_options(true, 1024),
                ..case("should handle thinking", Scenario::Thinking)
            },
            Case {
                options: thinking_options(true, 2048),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn google_vertex_provider_gemini_3_flash_preview() {
    // TS: Google Vertex describe with per-it skipIf gates.
    let llm = get_model_or_panic("google-vertex", "gemini-3-flash-preview");
    let vertex_project = live_env("GOOGLE_CLOUD_PROJECT").or_else(|| live_env("GCLOUD_PROJECT"));
    let vertex_location = live_env("GOOGLE_CLOUD_LOCATION");
    let vertex_api_key = live_env("GOOGLE_CLOUD_API_KEY");
    let is_vertex_configured = vertex_project.is_some() && vertex_location.is_some();
    let label = "google-vertex/gemini-3-flash-preview";
    let vertex_options = || LiveOptions {
        project: vertex_project.clone(),
        location: vertex_location.clone(),
        ..Default::default()
    };

    if !is_vertex_configured {
        skip(
            "Google Vertex Provider (gemini-3-flash-preview) [ADC cases]",
            "GOOGLE_CLOUD_PROJECT|GCLOUD_PROJECT + GOOGLE_CLOUD_LOCATION",
        );
    } else {
        run_cases(
            &llm,
            label,
            vec![
                Case {
                    options: vertex_options(),
                    ..case("should complete basic text generation", Scenario::Basic)
                },
                Case {
                    options: vertex_options(),
                    ..case("should handle tool calling", Scenario::ToolCall)
                },
                Case {
                    // thinking: { enabled: true, budgetTokens: 1024, level: ThinkingLevel.LOW }
                    options: LiveOptions {
                        thinking: Some(LiveThinking {
                            enabled: true,
                            budget_tokens: Some(1024),
                            level: Some("LOW".to_string()),
                        }),
                        ..vertex_options()
                    },
                    ..case("should handle thinking", Scenario::Thinking)
                },
                Case {
                    options: vertex_options(),
                    ..case("should handle streaming", Scenario::Streaming)
                },
                Case {
                    // thinking: { enabled: true, budgetTokens: 1024, level: ThinkingLevel.MEDIUM }
                    options: LiveOptions {
                        thinking: Some(LiveThinking {
                            enabled: true,
                            budget_tokens: Some(1024),
                            level: Some("MEDIUM".to_string()),
                        }),
                        ..vertex_options()
                    },
                    ..case(
                        "should handle multi-turn with thinking and tools",
                        Scenario::MultiTurn,
                    )
                },
                Case {
                    options: vertex_options(),
                    ..case("should handle image input", Scenario::Image)
                },
            ],
        )
        .await;
    }

    if vertex_api_key.is_none() {
        skip(
            "Google Vertex Provider (gemini-3-flash-preview) [API key case]",
            "GOOGLE_CLOUD_API_KEY",
        );
    } else {
        let options = LiveOptions::with_api_key(vertex_api_key.as_deref().unwrap_or_default());
        run_case(
            Scenario::Basic,
            &llm,
            &format!("{label}: should complete basic text generation with Vertex API key"),
            &options,
        )
        .await;
    }
}

#[tokio::test]
async fn openai_completions_provider_gpt_4o_mini() {
    // TS: describe.skipIf(!process.env.OPENAI_API_KEY)
    if live_env("OPENAI_API_KEY").is_none() {
        skip(
            "OpenAI Completions Provider (gpt-4o-mini)",
            "OPENAI_API_KEY",
        );
        return;
    }
    let llm = as_openai_completions(&get_model_or_panic("openai", "gpt-4o-mini"));
    run_cases(
        &llm,
        "openai-completions/gpt-4o-mini",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn deepseek_provider_deepseek_v4_flash() {
    // TS: describe.skipIf(!process.env.DEEPSEEK_API_KEY)
    if live_env("DEEPSEEK_API_KEY").is_none() {
        skip(
            "DeepSeek Provider (deepseek-v4-flash via OpenAI Completions)",
            "DEEPSEEK_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("deepseek", "deepseek-v4-flash");
    run_cases(
        &llm,
        "deepseek/deepseek-v4-flash",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::High),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::High),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn openai_responses_provider_gpt_5_4() {
    // TS: describe.skipIf(!process.env.OPENAI_API_KEY)
    if live_env("OPENAI_API_KEY").is_none() {
        skip("OpenAI Responses Provider (gpt-5.4)", "OPENAI_API_KEY");
        return;
    }
    let llm = get_model_or_panic("openai", "gpt-5.4");
    run_cases(
        &llm,
        "openai/gpt-5.4",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::High),
                ..case("should handle thinking", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::High),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn anthropic_provider_claude_haiku_4_5() {
    // TS: describe.skipIf(!process.env.ANTHROPIC_API_KEY)
    if live_env("ANTHROPIC_API_KEY").is_none() {
        skip("Anthropic Provider (claude-haiku-4-5)", "ANTHROPIC_API_KEY");
        return;
    }
    let model = get_model_or_panic("anthropic", "claude-haiku-4-5");
    run_cases(
        &model,
        "anthropic/claude-haiku-4-5",
        vec![
            Case {
                options: LiveOptions {
                    thinking_enabled: Some(true),
                    ..Default::default()
                },
                ..case("should complete basic text generation", Scenario::Basic)
            },
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn azure_openai_responses_provider_gpt_4o_mini() {
    // TS: describe.skipIf(!hasAzureOpenAICredentials())
    if !live::has_azure_openai_credentials() {
        skip(
            "Azure OpenAI Responses Provider (gpt-4o-mini)",
            "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
        );
        return;
    }
    let llm = get_model_or_panic("azure-openai-responses", "gpt-4o-mini");
    let azure_options = LiveOptions {
        azure_deployment_name: live::resolve_azure_deployment_name(&llm.id),
        ..Default::default()
    };
    run_cases(
        &llm,
        "azure-openai-responses/gpt-4o-mini",
        vec![
            Case {
                options: azure_options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: azure_options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: azure_options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: azure_options.clone(),
                ..case("should handle image input", Scenario::Image)
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn xai_provider_grok_4_3() {
    // TS: describe.skipIf(!process.env.XAI_API_KEY)
    if live_env("XAI_API_KEY").is_none() {
        skip(
            "xAI Provider (grok-4.3 via OpenAI Responses)",
            "XAI_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("xai", "grok-4.3");
    run_cases(
        &llm,
        "xai/grok-4.3",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn groq_provider_gpt_oss_20b() {
    // TS: describe.skipIf(!process.env.GROQ_API_KEY)
    if live_env("GROQ_API_KEY").is_none() {
        skip(
            "Groq Provider (gpt-oss-20b via OpenAI Completions)",
            "GROQ_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("groq", "openai/gpt-oss-20b");
    run_cases(
        &llm,
        "groq/openai/gpt-oss-20b",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn cerebras_provider_gpt_oss_120b() {
    // TS: describe.skipIf(!process.env.CEREBRAS_API_KEY)
    if live_env("CEREBRAS_API_KEY").is_none() {
        skip(
            "Cerebras Provider (gpt-oss-120b via OpenAI Completions)",
            "CEREBRAS_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("cerebras", "gpt-oss-120b");
    run_cases(
        &llm,
        "cerebras/gpt-oss-120b",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn cloudflare_workers_ai_provider_kimi_k2_6() {
    // TS: describe.skipIf(!hasCloudflareWorkersAICredentials())
    if !live::has_cloudflare_workers_ai_credentials() {
        skip(
            "Cloudflare Workers AI Provider (Kimi K2.6 via OpenAI Completions)",
            "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID",
        );
        return;
    }
    let llm = get_model_or_panic("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6");
    // Thinking/multi-turn extras ride the Cloudflare Models path in TS; the
    // Rust Models::stream drops them (documented deviation, see file header).
    run_cases(
        &llm,
        "cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle thinking mode", Scenario::Thinking),
            case(
                "should handle multi-turn with thinking and tools",
                Scenario::MultiTurn,
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cloudflare_ai_gateway_provider_workers_ai_kimi_k2_6() {
    // TS: describe.skipIf(!hasCloudflareAiGatewayCredentials())
    if !live::has_cloudflare_ai_gateway_credentials() {
        skip(
            "Cloudflare AI Gateway → Workers AI (Kimi K2.6 via /compat)",
            "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID",
        );
        return;
    }
    let llm = get_model_or_panic(
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    );
    run_cases(
        &llm,
        "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle thinking mode", Scenario::Thinking),
            case(
                "should handle multi-turn with thinking and tools",
                Scenario::MultiTurn,
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cloudflare_ai_gateway_openai_byok_gpt_5_1() {
    // TS: describe.skipIf(!hasCloudflareAiGatewayCredentials() || !process.env.OPENAI_API_KEY)
    if !live::has_cloudflare_ai_gateway_credentials() || live_env("OPENAI_API_KEY").is_none() {
        skip(
            "Cloudflare AI Gateway → OpenAI BYOK (gpt-5.1 via /openai responses)",
            "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID + OPENAI_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("cloudflare-ai-gateway", "gpt-5.1");
    let options = LiveOptions {
        headers: Some(
            [(
                "Authorization".to_string(),
                Some(format!(
                    "Bearer {}",
                    live_env("OPENAI_API_KEY").unwrap_or_default()
                )),
            )]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    };
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        reasoning_effort: Some(LiveEffort::Medium),
        ..options.clone()
    };
    run_cases(
        &llm,
        "cloudflare-ai-gateway/gpt-5.1",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn cloudflare_ai_gateway_anthropic_byok_claude_sonnet_4_5() {
    // TS: describe.skipIf(!hasCloudflareAiGatewayCredentials() || !process.env.ANTHROPIC_API_KEY)
    if !live::has_cloudflare_ai_gateway_credentials() || live_env("ANTHROPIC_API_KEY").is_none() {
        skip(
            "Cloudflare AI Gateway → Anthropic BYOK (claude-sonnet-4.5 via /anthropic messages)",
            "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID + ANTHROPIC_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("cloudflare-ai-gateway", "claude-sonnet-4.5");
    let options = LiveOptions {
        headers: Some(
            [(
                "Authorization".to_string(),
                Some(format!(
                    "Bearer {}",
                    live_env("ANTHROPIC_API_KEY").unwrap_or_default()
                )),
            )]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    };
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        reasoning_effort: Some(LiveEffort::High),
        ..options.clone()
    };
    run_cases(
        &llm,
        "cloudflare-ai-gateway/claude-sonnet-4.5",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn hugging_face_provider_kimi_k2_5() {
    // TS: describe.skipIf(!process.env.HF_TOKEN)
    if live_env("HF_TOKEN").is_none() {
        skip(
            "Hugging Face Provider (Kimi-K2.5 via OpenAI Completions)",
            "HF_TOKEN",
        );
        return;
    }
    let llm = get_model_or_panic("huggingface", "moonshotai/Kimi-K2.5");
    run_cases(
        &llm,
        "huggingface/moonshotai/Kimi-K2.5",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn together_ai_provider_kimi_k2_6() {
    // TS: describe.skipIf(!process.env.TOGETHER_API_KEY)
    if live_env("TOGETHER_API_KEY").is_none() {
        skip(
            "Together AI Provider (Kimi-K2.6 via OpenAI Completions)",
            "TOGETHER_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("together", "moonshotai/Kimi-K2.6");
    run_cases(
        &llm,
        "together/moonshotai/Kimi-K2.6",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::High),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::High),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn baseten_provider_glm_5_2() {
    // TS: describe.skipIf(!process.env.BASETEN_API_KEY)
    if live_env("BASETEN_API_KEY").is_none() {
        skip(
            "Baseten Provider (GLM 5.2 via OpenAI Completions)",
            "BASETEN_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("baseten", "zai-org/GLM-5.2");
    let options = effort_options(LiveEffort::High);
    run_cases(
        &llm,
        "baseten/zai-org/GLM-5.2",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn nvidia_nim_provider_nemotron_3_super() {
    // TS: describe.skipIf(!process.env.NVIDIA_API_KEY)
    if live_env("NVIDIA_API_KEY").is_none() {
        skip(
            "NVIDIA NIM Provider (Nemotron 3 Super via OpenAI Completions)",
            "NVIDIA_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("nvidia", "nvidia/nemotron-3-super-120b-a12b");
    run_cases(
        &llm,
        "nvidia/nvidia/nemotron-3-super-120b-a12b",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::High),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::High),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn openrouter_provider_glm_4_5v() {
    // TS: describe.skipIf(!process.env.OPENROUTER_API_KEY)
    if live_env("OPENROUTER_API_KEY").is_none() {
        skip(
            "OpenRouter Provider (glm-4.5v via OpenAI Completions)",
            "OPENROUTER_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("openrouter", "z-ai/glm-4.5v");
    run_cases(
        &llm,
        "openrouter/z-ai/glm-4.5v",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn vercel_ai_gateway_google_gemini_2_5_flash() {
    // TS: describe.skipIf(!process.env.AI_GATEWAY_API_KEY)
    if live_env("AI_GATEWAY_API_KEY").is_none() {
        skip(
            "Vercel AI Gateway Provider (google/gemini-2.5-flash via Anthropic Messages)",
            "AI_GATEWAY_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("vercel-ai-gateway", "google/gemini-2.5-flash");
    run_cases(
        &llm,
        "vercel-ai-gateway/google/gemini-2.5-flash",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle image input", Scenario::Image),
            case("should handle multi-turn with tools", Scenario::MultiTurn),
        ],
    )
    .await;
}

#[tokio::test]
async fn vercel_ai_gateway_anthropic_claude_opus_4_5() {
    // TS: describe.skipIf(!process.env.AI_GATEWAY_API_KEY)
    if live_env("AI_GATEWAY_API_KEY").is_none() {
        skip(
            "Vercel AI Gateway Provider (anthropic/claude-opus-4.5 via Anthropic Messages)",
            "AI_GATEWAY_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("vercel-ai-gateway", "anthropic/claude-opus-4.5");
    run_cases(
        &llm,
        "vercel-ai-gateway/anthropic/claude-opus-4.5",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle image input", Scenario::Image),
            case("should handle multi-turn with tools", Scenario::MultiTurn),
        ],
    )
    .await;
}

#[tokio::test]
async fn vercel_ai_gateway_openai_gpt_5_1_codex_max() {
    // TS: describe.skipIf(!process.env.AI_GATEWAY_API_KEY)
    if live_env("AI_GATEWAY_API_KEY").is_none() {
        skip(
            "Vercel AI Gateway Provider (openai/gpt-5.1-codex-max via Anthropic Messages)",
            "AI_GATEWAY_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("vercel-ai-gateway", "openai/gpt-5.1-codex-max");
    run_cases(
        &llm,
        "vercel-ai-gateway/openai/gpt-5.1-codex-max",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle image input", Scenario::Image),
            case("should handle multi-turn with tools", Scenario::MultiTurn),
        ],
    )
    .await;
}

#[tokio::test]
async fn zai_provider_glm_5_2() {
    // TS: describe.skipIf(!process.env.ZAI_API_KEY)
    if live_env("ZAI_API_KEY").is_none() {
        skip(
            "zAI Provider (glm-5.2 via OpenAI Completions)",
            "ZAI_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("zai", "glm-5.2");
    run_cases(
        &llm,
        "zai/glm-5.2",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::Medium),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn mistral_provider_devstral_medium_latest() {
    // TS: describe.skipIf(!process.env.MISTRAL_API_KEY)
    if live_env("MISTRAL_API_KEY").is_none() {
        skip(
            "Mistral Provider (devstral-medium-latest)",
            "MISTRAL_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("mistral", "devstral-medium-latest");
    run_cases(
        &llm,
        "mistral/devstral-medium-latest",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
        ],
    )
    .await;
    // "should handle thinking mode" and the multi-turn case switch to
    // mistral-small-2603 in the TS suite.
    let thinking_model = get_model_or_panic("mistral", "mistral-small-2603");
    run_cases(
        &thinking_model,
        "mistral/mistral-small-2603",
        vec![
            Case {
                options: effort_options(LiveEffort::High),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: effort_options(LiveEffort::High),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn mistral_provider_pixtral_12b() {
    // TS: describe.skipIf(!process.env.MISTRAL_API_KEY)
    if live_env("MISTRAL_API_KEY").is_none() {
        skip(
            "Mistral Provider (pixtral-12b with image support)",
            "MISTRAL_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("mistral", "pixtral-12b");
    run_cases(
        &llm,
        "mistral/pixtral-12b",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn minimax_provider_minimax_m2_7() {
    // TS: describe.skipIf(!process.env.MINIMAX_API_KEY)
    if live_env("MINIMAX_API_KEY").is_none() {
        skip(
            "MiniMax Provider (MiniMax-M2.7 via Anthropic Messages)",
            "MINIMAX_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("minimax", "MiniMax-M2.7");
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        thinking_budget_tokens: Some(2048),
        ..Default::default()
    };
    run_cases(
        &llm,
        "minimax/MiniMax-M2.7",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn kimi_for_coding_provider_kimi_for_coding() {
    // TS: describe.skipIf(!process.env.KIMI_API_KEY)
    if live_env("KIMI_API_KEY").is_none() {
        skip(
            "Kimi For Coding Provider (kimi-for-coding via Anthropic Messages)",
            "KIMI_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("kimi-coding", "kimi-for-coding");
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        thinking_budget_tokens: Some(2048),
        ..Default::default()
    };
    run_cases(
        &llm,
        "kimi-coding/kimi-for-coding",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

/// The shared Xiaomi MiMo thinking options
/// (`{ thinkingEnabled: true, reasoningEffort: "high" }`).
fn xiaomi_thinking_options() -> LiveOptions {
    LiveOptions {
        thinking_enabled: Some(true),
        reasoning_effort: Some(LiveEffort::High),
        ..Default::default()
    }
}

async fn run_xiaomi_suite(provider: &str, suite: &str, env: &str) {
    if live_env(env).is_none() {
        skip(suite, env);
        return;
    }
    let llm = get_model_or_panic(provider, "mimo-v2.5-pro");
    let thinking_options = xiaomi_thinking_options();
    run_cases(
        &llm,
        &format!("{provider}/mimo-v2.5-pro"),
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn xiaomi_mimo_api_billing_provider() {
    // TS: describe.skipIf(!process.env.XIAOMI_API_KEY)
    run_xiaomi_suite(
        "xiaomi",
        "Xiaomi MiMo (API billing) Provider (Xiaomi MiMo-V2.5-Pro via Anthropic Messages)",
        "XIAOMI_API_KEY",
    )
    .await;
}

#[tokio::test]
async fn xiaomi_mimo_token_plan_cn_provider() {
    // TS: describe.skipIf(!process.env.XIAOMI_TOKEN_PLAN_CN_API_KEY)
    run_xiaomi_suite(
        "xiaomi-token-plan-cn",
        "Xiaomi MiMo Token Plan Provider (Xiaomi MiMo-V2.5-Pro via Anthropic Messages, CN region)",
        "XIAOMI_TOKEN_PLAN_CN_API_KEY",
    )
    .await;
}

#[tokio::test]
async fn xiaomi_mimo_token_plan_ams_provider() {
    // TS: describe.skipIf(!process.env.XIAOMI_TOKEN_PLAN_AMS_API_KEY)
    run_xiaomi_suite(
        "xiaomi-token-plan-ams",
        "Xiaomi MiMo Token Plan Provider (Xiaomi MiMo-V2.5-Pro via Anthropic Messages, AMS region)",
        "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    )
    .await;
}

#[tokio::test]
async fn xiaomi_mimo_token_plan_sgp_provider() {
    // TS: describe.skipIf(!process.env.XIAOMI_TOKEN_PLAN_SGP_API_KEY)
    run_xiaomi_suite(
        "xiaomi-token-plan-sgp",
        "Xiaomi MiMo Token Plan Provider (Xiaomi MiMo-V2.5-Pro via Anthropic Messages, SGP region)",
        "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
    )
    .await;
}

async fn run_qwen_suite(provider: &str, model_id: &str, suite: &str, env: &str) {
    if live_env(env).is_none() {
        skip(suite, env);
        return;
    }
    let llm = get_model_or_panic(provider, model_id);
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        reasoning_effort: Some(LiveEffort::High),
        ..Default::default()
    };
    run_cases(
        &llm,
        &format!("{provider}/{model_id}"),
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn qwen_token_plan_provider_qwen3_7_max() {
    // TS: describe.skipIf(!process.env.QWEN_TOKEN_PLAN_API_KEY)
    run_qwen_suite(
        "qwen-token-plan",
        "qwen3.7-max",
        "Qwen Token Plan Provider (Qwen3.7-Max, international)",
        "QWEN_TOKEN_PLAN_API_KEY",
    )
    .await;
}

#[tokio::test]
async fn qwen_token_plan_individual_provider_qwen3_8_max() {
    // TS: describe.skipIf(!process.env.QWEN_TOKEN_PLAN_API_KEY)
    run_qwen_suite(
        "qwen-token-plan-individual",
        "qwen3.8-max",
        "Qwen Token Plan Individual Provider (Qwen3.8-Max, international)",
        "QWEN_TOKEN_PLAN_API_KEY",
    )
    .await;
}

#[tokio::test]
async fn qwen_token_plan_cn_provider_qwen3_7_max() {
    // TS: describe.skipIf(!process.env.QWEN_TOKEN_PLAN_CN_API_KEY)
    run_qwen_suite(
        "qwen-token-plan-cn",
        "qwen3.7-max",
        "Qwen Token Plan Provider (Qwen3.7-Max, CN region)",
        "QWEN_TOKEN_PLAN_CN_API_KEY",
    )
    .await;
}

#[tokio::test]
async fn ant_ling_provider_ling_2_6_flash() {
    // TS: describe.skipIf(!process.env.ANT_LING_API_KEY)
    if live_env("ANT_LING_API_KEY").is_none() {
        skip(
            "Ant Ling Provider (Ling 2.6 Flash via OpenAI Completions)",
            "ANT_LING_API_KEY",
        );
        return;
    }
    let llm = get_model_or_panic("ant-ling", "Ling-2.6-flash");
    run_cases(
        &llm,
        "ant-ling/Ling-2.6-flash",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
        ],
    )
    .await;
    // "should handle thinking mode" switches to Ring-2.6-1T in the TS suite.
    let ring_model = get_model_or_panic("ant-ling", "Ring-2.6-1T");
    run_case(
        Scenario::Thinking,
        &ring_model,
        "ant-ling/Ring-2.6-1T: should handle thinking mode",
        &effort_options(LiveEffort::High),
    )
    .await;
}

#[tokio::test]
async fn anthropic_oauth_provider_claude_sonnet_4_6() {
    // TS: it.skipIf(!anthropicOAuthToken) with resolveApiKey("anthropic")
    let Some(token) = live::resolve_api_key("anthropic").await else {
        skip(
            "Anthropic OAuth Provider (claude-sonnet-4-6)",
            "~/.pi/agent/auth.json anthropic credentials",
        );
        return;
    };
    let model = get_model_or_panic("anthropic", "claude-sonnet-4-6");
    let options = LiveOptions::with_api_key(&token);
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        ..options.clone()
    };
    run_cases(
        &model,
        "anthropic-oauth/claude-sonnet-4-6",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            Case {
                options: options.clone(),
                ..case("should handle image input", Scenario::Image)
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn anthropic_oauth_provider_claude_opus_4_6_adaptive_thinking() {
    // TS: it.skipIf(!anthropicOAuthToken) with resolveApiKey("anthropic")
    let Some(token) = live::resolve_api_key("anthropic").await else {
        skip(
            "Anthropic OAuth Provider (claude-opus-4-6 with adaptive thinking)",
            "~/.pi/agent/auth.json anthropic credentials",
        );
        return;
    };
    let model = get_model_or_panic("anthropic", "claude-opus-4-6");
    let options = LiveOptions::with_api_key(&token);
    let adaptive = |effort: LiveEffort| LiveOptions {
        thinking_enabled: Some(true),
        effort: Some(effort),
        ..options.clone()
    };
    run_cases(
        &model,
        "anthropic-oauth/claude-opus-4-6",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: adaptive(LiveEffort::High),
                ..case(
                    "should handle adaptive thinking with effort high",
                    Scenario::Thinking,
                )
            },
            Case {
                options: adaptive(LiveEffort::Medium),
                ..case(
                    "should handle adaptive thinking with effort medium",
                    Scenario::Thinking,
                )
            },
            Case {
                options: adaptive(LiveEffort::High),
                ..case(
                    "should handle multi-turn with adaptive thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            Case {
                options: options.clone(),
                ..case("should handle image input", Scenario::Image)
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn github_copilot_provider_gpt_5_3_codex() {
    // TS: it.skipIf(!githubCopilotToken) with resolveApiKey("github-copilot")
    let Some(token) = live::resolve_api_key("github-copilot").await else {
        skip(
            "GitHub Copilot Provider (gpt-5.3-codex via OpenAI Completions)",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
        return;
    };
    let llm = get_model_or_panic("github-copilot", "gpt-5.3-codex");
    let options = LiveOptions::with_api_key(&token);
    run_cases(
        &llm,
        "github-copilot/gpt-5.3-codex",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: options.clone(),
                ..case("should handle image input", Scenario::Image)
            },
        ],
    )
    .await;
    // "should handle thinking" and the multi-turn case switch to gpt-5-mini.
    let thinking_model = get_model_or_panic("github-copilot", "gpt-5-mini");
    let thinking_options = LiveOptions {
        reasoning_effort: Some(LiveEffort::High),
        ..options.clone()
    };
    run_cases(
        &thinking_model,
        "github-copilot/gpt-5-mini",
        vec![
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn github_copilot_provider_claude_sonnet_4() {
    // TS: it.skipIf(!githubCopilotToken) with resolveApiKey("github-copilot")
    let Some(token) = live::resolve_api_key("github-copilot").await else {
        skip(
            "GitHub Copilot Provider (claude-sonnet-4 via Anthropic Messages)",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
        return;
    };
    let llm = get_model_or_panic("github-copilot", "claude-sonnet-4.6");
    let options = LiveOptions::with_api_key(&token);
    let thinking_options = LiveOptions {
        thinking_enabled: Some(true),
        ..options.clone()
    };
    run_cases(
        &llm,
        "github-copilot/claude-sonnet-4.6",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            Case {
                options: options.clone(),
                ..case("should handle image input", Scenario::Image)
            },
        ],
    )
    .await;
}

async fn run_codex_suite(
    model_id: &str,
    suite: &str,
    websocket: bool,
    thinking_effort: LiveEffort,
    multi_turn_effort: Option<LiveEffort>,
) {
    // TS: it.skipIf(!openaiCodexToken) with resolveApiKey("openai-codex")
    let Some(token) = live::resolve_api_key("openai-codex").await else {
        skip(suite, "~/.pi/agent/auth.json openai-codex credentials");
        return;
    };
    let llm = get_model_or_panic("openai-codex", model_id);
    let mut options = LiveOptions::with_api_key(&token);
    if websocket {
        options.transport = Some(pi_core::ai::types::Transport::Websocket);
    }
    let thinking_options = LiveOptions {
        reasoning_effort: Some(thinking_effort),
        ..options.clone()
    };
    let multi_turn_options = match multi_turn_effort {
        Some(effort) => LiveOptions {
            reasoning_effort: Some(effort),
            ..options.clone()
        },
        None => options.clone(),
    };
    let label = format!(
        "openai-codex/{model_id}{}",
        if websocket { " (websocket)" } else { "" }
    );
    run_cases(
        &llm,
        &label,
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle thinking with reasoningEffort xhigh",
                    Scenario::Thinking,
                )
            },
            Case {
                options: multi_turn_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            Case {
                options: options.clone(),
                ..case("should handle image input", Scenario::Image)
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn openai_codex_provider_gpt_5_4() {
    // TS: thinking uses reasoningEffort "high"; the multi-turn case passes
    // only the apiKey.
    run_codex_suite(
        "gpt-5.4",
        "OpenAI Codex Provider (gpt-5.4)",
        false,
        LiveEffort::High,
        None,
    )
    .await;
}

#[tokio::test]
async fn openai_codex_provider_gpt_5_5() {
    // TS: thinking and multi-turn use reasoningEffort "xhigh".
    run_codex_suite(
        "gpt-5.5",
        "OpenAI Codex Provider (gpt-5.5)",
        false,
        LiveEffort::Xhigh,
        Some(LiveEffort::Xhigh),
    )
    .await;
}

#[tokio::test]
async fn openai_codex_provider_gpt_5_5_websocket() {
    // TS: the WebSocket options ({ apiKey, transport: "websocket" }) plus
    // reasoningEffort "xhigh" for thinking and multi-turn.
    run_codex_suite(
        "gpt-5.5",
        "OpenAI Codex Provider (gpt-5.5 via WebSocket)",
        true,
        LiveEffort::Xhigh,
        Some(LiveEffort::Xhigh),
    )
    .await;
}

#[tokio::test]
async fn amazon_bedrock_provider_claude_sonnet_4_5() {
    // TS: describe.skipIf(!hasBedrockCredentials())
    if !live::has_bedrock_credentials() {
        skip(
            "Amazon Bedrock Provider (claude-sonnet-4-5)",
            "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
        );
        return;
    }
    let llm = get_model_or_panic(
        "amazon-bedrock",
        "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
    );
    run_cases(
        &llm,
        "amazon-bedrock/global.anthropic.claude-sonnet-4-5-20250929-v1:0",
        vec![
            case("should complete basic text generation", Scenario::Basic),
            case("should handle tool calling", Scenario::ToolCall),
            case("should handle streaming", Scenario::Streaming),
            Case {
                // { reasoning: "medium" }
                options: LiveOptions {
                    reasoning: Some(pi_core::ai::types::ThinkingLevel::Medium),
                    ..Default::default()
                },
                ..case("should handle thinking", Scenario::Thinking)
            },
            Case {
                // { reasoning: "high" }
                options: LiveOptions {
                    reasoning: Some(pi_core::ai::types::ThinkingLevel::High),
                    ..Default::default()
                },
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
            case("should handle image input", Scenario::Image),
        ],
    )
    .await;
}

#[tokio::test]
async fn amazon_bedrock_provider_claude_opus_4_6_interleaved_thinking() {
    // TS: describe.skipIf(!hasBedrockCredentials()) — the three payload-
    // inspection cases.
    if !live::has_bedrock_credentials() {
        skip(
            "Amazon Bedrock Provider (claude-opus-4-6 interleaved thinking)",
            "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
        );
        return;
    }
    let llm = get_model_or_panic("amazon-bedrock", "global.anthropic.claude-opus-4-6-v1");

    // "should use adaptive thinking without anthropic_beta"
    {
        let (captured, on_payload) = live::payload_capture();
        let options = LiveOptions {
            reasoning: Some(pi_core::ai::types::ThinkingLevel::Xhigh),
            interleaved_thinking: Some(true),
            on_payload: Some(on_payload),
            ..Default::default()
        };
        let context = Context {
            system_prompt: Some(
                "You are a helpful assistant that uses tools when asked.".to_string(),
            ),
            messages: vec![user_message(
                "Think first, then calculate 15 + 27 using the math_operation tool.",
            )],
            tools: Some(vec![calculator_tool()]),
        };
        let response = live_complete(&llm, &context, &options).await;
        let case =
            "amazon-bedrock/claude-opus-4-6: should use adaptive thinking without anthropic_beta";
        assert_ne!(
            response.stop_reason,
            StopReason::Error,
            "{case}, error: {:?}",
            response.error_message
        );
        let payload = captured
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| panic!("{case}: capturedPayload"));
        let additional = payload
            .get("additionalModelRequestFields")
            .cloned()
            .unwrap_or_else(|| panic!("{case}: additionalModelRequestFields"));
        assert_eq!(
            additional.get("thinking"),
            Some(&serde_json::json!({"type": "adaptive", "display": "summarized"})),
            "{case}: thinking"
        );
        assert_eq!(
            additional.get("output_config"),
            Some(&serde_json::json!({"effort": "max"})),
            "{case}: output_config"
        );
        assert!(
            additional.get("anthropic_beta").is_none(),
            "{case}: anthropic_beta must be absent"
        );
    }

    let llm_sonnet = get_model_or_panic(
        "amazon-bedrock",
        "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
    );

    // "should pass requestMetadata to the SDK payload"
    {
        let (captured, on_payload) = live::payload_capture();
        let metadata: std::collections::BTreeMap<String, String> = [
            ("app".to_string(), "pi-test".to_string()),
            ("env".to_string(), "ci".to_string()),
        ]
        .into_iter()
        .collect();
        let options = LiveOptions {
            request_metadata: Some(metadata.clone()),
            on_payload: Some(on_payload),
            ..Default::default()
        };
        let context = Context {
            system_prompt: None,
            messages: vec![user_message("Say hi.")],
            tools: None,
        };
        let response = live_complete(&llm_sonnet, &context, &options).await;
        let case =
            "amazon-bedrock/claude-sonnet-4-5: should pass requestMetadata to the SDK payload";
        assert_ne!(
            response.stop_reason,
            StopReason::Error,
            "{case}, error: {:?}",
            response.error_message
        );
        let payload = captured
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| panic!("{case}: capturedPayload"));
        let expected: serde_json::Map<String, serde_json::Value> = metadata
            .into_iter()
            .map(|(key, value)| (key, serde_json::Value::String(value)))
            .collect();
        assert_eq!(
            payload.get("requestMetadata"),
            Some(&serde_json::Value::Object(expected)),
            "{case}: requestMetadata"
        );
    }

    // "should omit requestMetadata from payload when not provided"
    {
        let (captured, on_payload) = live::payload_capture();
        let options = LiveOptions {
            on_payload: Some(on_payload),
            ..Default::default()
        };
        let context = Context {
            system_prompt: None,
            messages: vec![user_message("Say hi.")],
            tools: None,
        };
        let response = live_complete(&llm_sonnet, &context, &options).await;
        let case = "amazon-bedrock/claude-sonnet-4-5: should omit requestMetadata from payload when not provided";
        assert_ne!(
            response.stop_reason,
            StopReason::Error,
            "{case}, error: {:?}",
            response.error_message
        );
        let payload = captured
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| panic!("{case}: capturedPayload"));
        assert!(
            payload.get("requestMetadata").is_none(),
            "{case}: requestMetadata key must be absent"
        );
    }
}

// ---------------------------------------------------------------------------
// Ollama (local)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ollama_provider_gpt_oss_20b() {
    // TS: describe.skipIf(!ollamaInstalled) with a beforeAll that pulls the
    // model and starts `ollama serve`.
    let server = match live::setup_ollama().await {
        live::OllamaSetup::NotInstalled => {
            skip(
                "Ollama Provider (gpt-oss-20b via OpenAI Completions)",
                "ollama binary",
            );
            return;
        }
        live::OllamaSetup::PullFailed => {
            // TS warns "tests will be skipped" and leaves the suite's model
            // undefined in beforeAll.
            eprintln!("SKIP: Ollama Provider cases require pulling gpt-oss:20b to succeed");
            return;
        }
        live::OllamaSetup::Running(server) => server,
    };

    let llm = Model {
        id: "gpt-oss:20b".to_string(),
        api: "openai-completions".to_string(),
        provider: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        context_window: 128_000,
        max_tokens: 16_000,
        cost: pi_core::ai::types::ModelCost::default(),
        name: "Ollama GPT-OSS 20B".to_string(),
        ..Default::default()
    };
    let options = LiveOptions::with_api_key("test");
    let thinking_options = LiveOptions {
        reasoning_effort: Some(LiveEffort::Medium),
        ..options.clone()
    };
    run_cases(
        &llm,
        "ollama/gpt-oss:20b",
        vec![
            Case {
                options: options.clone(),
                ..case("should complete basic text generation", Scenario::Basic)
            },
            Case {
                options: options.clone(),
                ..case("should handle tool calling", Scenario::ToolCall)
            },
            Case {
                options: options.clone(),
                ..case("should handle streaming", Scenario::Streaming)
            },
            Case {
                options: thinking_options.clone(),
                ..case("should handle thinking mode", Scenario::Thinking)
            },
            Case {
                options: thinking_options.clone(),
                ..case(
                    "should handle multi-turn with thinking and tools",
                    Scenario::MultiTurn,
                )
            },
        ],
    )
    .await;
    drop(server);
}
