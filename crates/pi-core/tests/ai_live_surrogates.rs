//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/unicode-surrogate.test.ts`
//! ("AI Providers Unicode Surrogate Pair Tests", 87 TS cases: 29 provider
//! suites x 3 cases each).
//!
//! Two of the three shared TS bodies are ported verbatim:
//! `testEmojiInToolResults` and `testRealWorldLinkedInData`. The third,
//! `testUnpairedHighSurrogate`, is NOT representable in Rust: it constructs
//! `String.fromCharCode(0xd83d)` — a lone UTF-16 high surrogate — and asserts
//! the provider sanitizes it before sending. A Rust `String` is always valid
//! UTF-8 and cannot hold an unpaired surrogate (the input the case depends on
//! cannot be built; sanitization happens at a different layer — see
//! `pi_core::ai::utils::sanitize_unicode`). Those 29 matrix items are
//! therefore skipped with a printed reason instead of carrying a fake
//! equivalent assertion.
//!
//! Gates translated verbatim; without credentials each suite prints
//! `SKIP: <suite> requires <ENV>` and the test passes.
//!
//! Deviation: vitest `{ retry: 3, timeout: 30000 }` has no Rust equivalent.

mod common;

use common::live::{
    self, LiveEffort, LiveOptions, as_openai_completions, empty_object_schema, get_model_or_panic,
    live_complete, live_env, now_millis, skip,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, Context, Message, Model, RoleAssistant, RoleToolResult,
    RoleUser, StopReason, Tool, ToolCall, ToolResultMessage, UserContent, UserMessage,
};
use pi_core::ai::types::{BlockContent, TextContent};

fn user_message(content: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp: now_millis(),
    })
}

/// The synthetic assistant tool-call message the TS suites splice into the
/// context (zero usage, stopReason "toolUse").
fn tool_call_assistant_message(
    llm: &Model,
    tool_call_id: &str,
    tool_name: &str,
) -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: tool_call_id.to_string(),
            name: tool_name.to_string(),
            arguments: serde_json::Map::new(),
            ..Default::default()
        })],
        api: llm.api.clone(),
        provider: llm.provider.clone(),
        model: llm.id.clone(),
        usage: pi_core::ai::types::Usage::default(),
        stop_reason: StopReason::ToolUse,
        timestamp: now_millis(),
        ..Default::default()
    }
}

fn tool_result(tool_call_id: &str, tool_name: &str, text: &str) -> ToolResultMessage {
    ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: now_millis(),
        ..Default::default()
    }
}

/// testEmojiInToolResults from unicode-surrogate.test.ts.
async fn test_emoji_in_tool_results(llm: &Model, case: &str, options: &LiveOptions) {
    let tool_call_id = if llm.provider == "mistral" {
        "testtool1"
    } else {
        "test_1"
    };
    // Simulate a tool that returns emoji
    let mut context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![
            user_message("Use the test tool"),
            Message::Assistant(Box::new(tool_call_assistant_message(
                llm,
                tool_call_id,
                "test_tool",
            ))),
        ],
        tools: Some(vec![Tool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            parameters: empty_object_schema(),
            constrained_sampling: None,
        }]),
    };

    // Add tool result with various problematic Unicode characters
    let text = "Test with emoji 🙈 and other characters:\n- Monkey emoji: 🙈\n- Thumbs up: 👍\n- Heart: ❤️\n- Thinking face: 🤔\n- Rocket: 🚀\n- Mixed text: Mario Zechner wann? Wo? Bin grad äußersr eventuninformiert 🙈\n- Japanese: こんにちは\n- Chinese: 你好\n- Mathematical symbols: ∑∫∂√\n- Special quotes: \"curly\" 'quotes'";
    context
        .messages
        .push(Message::ToolResult(Box::new(tool_result(
            tool_call_id,
            "test_tool",
            text,
        ))));

    // Add follow-up user message
    context
        .messages
        .push(user_message("Summarize the tool result briefly."));

    // This should not throw a surrogate pair error
    let response = live_complete(llm, &context, options).await;

    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(
        response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        response.error_message
    );
    assert!(!response.content.is_empty(), "{case}: content");
}

/// testRealWorldLinkedInData from unicode-surrogate.test.ts.
async fn test_real_world_linkedin_data(llm: &Model, case: &str, options: &LiveOptions) {
    let tool_call_id = if llm.provider == "mistral" {
        "linkedin1"
    } else {
        "linkedin_1"
    };
    let mut context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![
            user_message("Use the linkedin tool to get comments"),
            Message::Assistant(Box::new(tool_call_assistant_message(
                llm,
                tool_call_id,
                "linkedin_skill",
            ))),
        ],
        tools: Some(vec![Tool {
            name: "linkedin_skill".to_string(),
            description: "Get LinkedIn comments".to_string(),
            parameters: empty_object_schema(),
            constrained_sampling: None,
        }]),
    };

    // Real-world tool result from LinkedIn with emoji
    let text = "Post: Hab einen \"Generative KI für Nicht-Techniker\" Workshop gebaut.\nUnanswered Comments: 2\n\n=> {\n  \"comments\": [\n    {\n      \"author\": \"Matthias Neumayer's  graphic link\",\n      \"text\": \"Leider nehmen das viel zu wenige Leute ernst\"\n    },\n    {\n      \"author\": \"Matthias Neumayer's  graphic link\",\n      \"text\": \"Mario Zechner wann? Wo? Bin grad äußersr eventuninformiert 🙈\"\n    }\n  ]\n}";
    context
        .messages
        .push(Message::ToolResult(Box::new(tool_result(
            tool_call_id,
            "linkedin_skill",
            text,
        ))));

    context
        .messages
        .push(user_message("How many comments are there?"));

    // This should not throw a surrogate pair error
    let response = live_complete(llm, &context, options).await;

    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(
        response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        response.error_message
    );
    assert!(
        response
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Text(_))),
        "{case}: text content"
    );
}

/// testUnpairedHighSurrogate from unicode-surrogate.test.ts — NOT
/// representable: a Rust `String` cannot hold the lone high surrogate
/// (U+D83D) the case injects. Skipped per matrix entry with a printed reason.
fn skip_unpaired_surrogate_case(label: &str) {
    eprintln!(
        "SKIP: {label}: should handle unpaired high surrogate (0xD83D) in tool results — not representable: Rust String cannot hold an unpaired UTF-16 surrogate"
    );
}

struct SurrogateSuite {
    label: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    reasoning_effort_high: bool,
    /// `{ ...baseModel, api: "openai-completions" }` (stream.test.ts style).
    openai_completions_api: bool,
    /// OAuth token resolution instead of an env gate.
    oauth: Option<&'static str>,
}

async fn run_surrogate_suite(suite: SurrogateSuite) {
    // Resolve the gate (OAuth suites resolve their token; env suites read the
    // env var; azure/bedrock/cloudflare use the helpers).
    let mut token: Option<String> = None;
    let gate_description;
    let gated = if let Some(oauth_provider) = suite.oauth {
        token = live::resolve_api_key(oauth_provider).await;
        gate_description = format!("~/.pi/agent/auth.json {oauth_provider} credentials");
        token.is_some()
    } else if suite.provider == "azure-openai-responses" {
        gate_description = suite.env.to_string();
        live::has_azure_openai_credentials()
    } else if suite.provider == "amazon-bedrock" {
        gate_description = suite.env.to_string();
        live::has_bedrock_credentials()
    } else if suite.provider == "cloudflare-workers-ai" {
        gate_description = suite.env.to_string();
        live::has_cloudflare_workers_ai_credentials()
    } else if suite.provider == "cloudflare-ai-gateway" {
        gate_description = suite.env.to_string();
        live::has_cloudflare_ai_gateway_credentials()
    } else {
        gate_description = suite.env.to_string();
        live_env(suite.env).is_some()
    };
    if !gated {
        skip(
            &format!("{} Unicode Handling", suite.label),
            &gate_description,
        );
        return;
    }
    let model = get_model_or_panic(suite.provider, suite.model);
    let model = if suite.openai_completions_api {
        as_openai_completions(&model)
    } else {
        model
    };
    let mut options = LiveOptions::default();
    if let Some(token) = token.as_deref() {
        options = LiveOptions::with_api_key(token);
    }
    if suite.reasoning_effort_high {
        options.reasoning_effort = Some(LiveEffort::High);
    }
    if suite.provider == "azure-openai-responses" {
        options.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
    }

    let label = format!("{} Unicode Handling", suite.label);
    test_emoji_in_tool_results(
        &model,
        &format!("{label}: should handle emoji in tool results"),
        &options,
    )
    .await;
    test_real_world_linkedin_data(
        &model,
        &format!("{label}: should handle real-world LinkedIn comment data with emoji"),
        &options,
    )
    .await;
    skip_unpaired_surrogate_case(&label);
}

#[tokio::test]
async fn unicode_surrogate_env_provider_matrix() {
    let suites = [
        SurrogateSuite {
            label: "Google Provider",
            provider: "google",
            model: "gemini-2.5-flash",
            env: "GEMINI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            // TS uses getModel("openai", "gpt-4o-mini") without the
            // completions api override here (openai-responses api).
            label: "OpenAI Completions Provider",
            provider: "openai",
            model: "gpt-4o-mini",
            env: "OPENAI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "OpenAI Responses Provider",
            provider: "openai",
            model: "gpt-5-mini",
            env: "OPENAI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Azure OpenAI Responses Provider",
            provider: "azure-openai-responses",
            model: "gpt-4o-mini",
            env: "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Anthropic Provider",
            provider: "anthropic",
            model: "claude-haiku-4-5",
            env: "ANTHROPIC_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "xAI Provider",
            provider: "xai",
            model: "grok-4.3",
            env: "XAI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Groq Provider",
            provider: "groq",
            model: "openai/gpt-oss-20b",
            env: "GROQ_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Cerebras Provider",
            provider: "cerebras",
            model: "gpt-oss-120b",
            env: "CEREBRAS_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Cloudflare Workers AI Provider",
            provider: "cloudflare-workers-ai",
            model: "@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Cloudflare AI Gateway Provider",
            provider: "cloudflare-ai-gateway",
            model: "workers-ai/@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Hugging Face Provider",
            provider: "huggingface",
            model: "moonshotai/Kimi-K2.5",
            env: "HF_TOKEN",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Together AI Provider",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            reasoning_effort_high: true,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Baseten Provider",
            provider: "baseten",
            model: "zai-org/GLM-5.2",
            env: "BASETEN_API_KEY",
            reasoning_effort_high: true,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "zAI Provider",
            provider: "zai",
            model: "glm-5.2",
            env: "ZAI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Mistral Provider",
            provider: "mistral",
            model: "devstral-medium-latest",
            env: "MISTRAL_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "MiniMax Provider",
            provider: "minimax",
            model: "MiniMax-M2.7",
            env: "MINIMAX_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Xiaomi MiMo (API billing) Provider",
            provider: "xiaomi",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Xiaomi MiMo Token Plan (CN) Provider",
            provider: "xiaomi-token-plan-cn",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Xiaomi MiMo Token Plan (AMS) Provider",
            provider: "xiaomi-token-plan-ams",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Xiaomi MiMo Token Plan (SGP) Provider",
            provider: "xiaomi-token-plan-sgp",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Qwen Token Plan Provider",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Qwen Token Plan Individual Provider",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Qwen Token Plan (CN) Provider",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Kimi For Coding Provider",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Vercel AI Gateway Provider",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
        SurrogateSuite {
            label: "Amazon Bedrock Provider",
            provider: "amazon-bedrock",
            model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            env: "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: None,
        },
    ];

    for suite in suites {
        run_surrogate_suite(suite).await;
    }
}

#[tokio::test]
async fn unicode_surrogate_oauth_providers() {
    let suites = [
        SurrogateSuite {
            label: "Anthropic OAuth Provider",
            provider: "anthropic",
            model: "claude-haiku-4-5",
            env: "",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: Some("anthropic"),
        },
        SurrogateSuite {
            label: "GitHub Copilot Provider (claude-haiku-4.5)",
            provider: "github-copilot",
            model: "claude-haiku-4.5",
            env: "",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: Some("github-copilot"),
        },
        SurrogateSuite {
            label: "GitHub Copilot Provider (claude-sonnet-4)",
            provider: "github-copilot",
            model: "claude-sonnet-4.6",
            env: "",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: Some("github-copilot"),
        },
        SurrogateSuite {
            label: "OpenAI Codex Provider (gpt-5.5)",
            provider: "openai-codex",
            model: "gpt-5.5",
            env: "",
            reasoning_effort_high: false,
            openai_completions_api: false,
            oauth: Some("openai-codex"),
        },
    ];

    for suite in suites {
        run_surrogate_suite(suite).await;
    }
}
