//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/empty.test.ts` ("AI Providers Empty Message Tests").
//!
//! The TypeScript suite is a parameterized matrix: 26 env-gated describes
//! plus 3 OAuth describes, each running the same four shared cases
//! (empty content array / empty string / whitespace-only / empty assistant
//! message in conversation) — 120 `it` cases in total. The Rust port keeps
//! the same matrix in a loop (`EMPTY_SUITES`) while executing every TS case's
//! assertions; suite names, models, gates, and option extras match the TS
//! source exactly.
//!
//! Gates (translated verbatim from the TS `describe.skipIf`/`it.skipIf`):
//! env vars via `common::live::live_env`, the azure/bedrock/cloudflare
//! helpers, and `resolveApiKey` from `oauth.ts` for the OAuth describes.
//! Without credentials each suite prints `SKIP: <suite> requires <ENV>` and
//! the test passes.
//!
//! Deviation: vitest `{ retry: 3, timeout: 30000 }` metadata has no Rust
//! equivalent; each case runs once.

mod common;

use common::live::{
    self, LiveEffort, LiveOptions, as_openai_completions, get_model_or_panic, live_complete,
    live_env, now_millis, skip,
};
use pi_core::ai::types::{
    AssistantMessage, Context, Message, RoleAssistant, RoleUser, StopReason, Usage, UsageCost,
    UserContent, UserMessage,
};

/// The gate condition of one TS describe.
enum Gate {
    /// `!process.env.<NAME>`
    Env(&'static str),
    /// `!hasAzureOpenAICredentials()`
    Azure,
    /// `!hasCloudflareWorkersAICredentials()`
    CloudflareWorkersAi,
    /// `!hasCloudflareAiGatewayCredentials()`
    CloudflareAiGateway,
    /// `!hasBedrockCredentials()`
    Bedrock,
}

impl Gate {
    fn env_description(&self) -> String {
        match self {
            Gate::Env(name) => name.to_string(),
            Gate::Azure => {
                "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME"
                    .to_string()
            }
            Gate::CloudflareWorkersAi => "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID".to_string(),
            Gate::CloudflareAiGateway => {
                "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID".to_string()
            }
            Gate::Bedrock => {
                "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK"
                    .to_string()
            }
        }
    }

    async fn check(&self) -> bool {
        match self {
            Gate::Env(name) => live_env(name).is_some(),
            Gate::Azure => live::has_azure_openai_credentials(),
            Gate::CloudflareWorkersAi => live::has_cloudflare_workers_ai_credentials(),
            Gate::CloudflareAiGateway => live::has_cloudflare_ai_gateway_credentials(),
            Gate::Bedrock => live::has_bedrock_credentials(),
        }
    }
}

/// One TS describe: a suite name, its model matrix entry, gate, and options.
struct EmptySuite {
    name: &'static str,
    provider: &'static str,
    model: &'static str,
    gate: Gate,
    /// `{ ...baseModel, api: "openai-completions" }` override.
    openai_completions_api: bool,
    /// Pass `{ azureDeploymentName }` options.
    azure: bool,
    /// Pass `{ reasoningEffort: "high" }` options (Baseten).
    reasoning_effort_high: bool,
}

const EMPTY_SUITES: &[EmptySuite] = &[
    EmptySuite {
        name: "Google Provider Empty Messages",
        provider: "google",
        model: "gemini-2.5-flash",
        gate: Gate::Env("GEMINI_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "OpenAI Completions Provider Empty Messages",
        provider: "openai",
        model: "gpt-4o-mini",
        gate: Gate::Env("OPENAI_API_KEY"),
        openai_completions_api: true,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "OpenAI Responses Provider Empty Messages",
        provider: "openai",
        model: "gpt-5-mini",
        gate: Gate::Env("OPENAI_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Azure OpenAI Responses Provider Empty Messages",
        provider: "azure-openai-responses",
        model: "gpt-4o-mini",
        gate: Gate::Azure,
        openai_completions_api: false,
        azure: true,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Anthropic Provider Empty Messages",
        provider: "anthropic",
        model: "claude-haiku-4-5",
        gate: Gate::Env("ANTHROPIC_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "xAI Provider Empty Messages",
        provider: "xai",
        model: "grok-4.3",
        gate: Gate::Env("XAI_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Groq Provider Empty Messages",
        provider: "groq",
        model: "openai/gpt-oss-20b",
        gate: Gate::Env("GROQ_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Cerebras Provider Empty Messages",
        provider: "cerebras",
        model: "gpt-oss-120b",
        gate: Gate::Env("CEREBRAS_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Cloudflare Workers AI Provider Empty Messages",
        provider: "cloudflare-workers-ai",
        model: "@cf/moonshotai/kimi-k2.6",
        gate: Gate::CloudflareWorkersAi,
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Cloudflare AI Gateway Provider Empty Messages",
        provider: "cloudflare-ai-gateway",
        model: "workers-ai/@cf/moonshotai/kimi-k2.6",
        gate: Gate::CloudflareAiGateway,
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Hugging Face Provider Empty Messages",
        provider: "huggingface",
        model: "moonshotai/Kimi-K2.5",
        gate: Gate::Env("HF_TOKEN"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Together AI Provider Empty Messages",
        provider: "together",
        model: "moonshotai/Kimi-K2.6",
        gate: Gate::Env("TOGETHER_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Baseten Provider Empty Messages",
        provider: "baseten",
        model: "zai-org/GLM-5.2",
        gate: Gate::Env("BASETEN_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: true,
    },
    EmptySuite {
        name: "zAI Provider Empty Messages",
        provider: "zai",
        model: "glm-5.2",
        gate: Gate::Env("ZAI_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Mistral Provider Empty Messages",
        provider: "mistral",
        model: "devstral-medium-latest",
        gate: Gate::Env("MISTRAL_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "MiniMax Provider Empty Messages",
        provider: "minimax",
        model: "MiniMax-M2.7",
        gate: Gate::Env("MINIMAX_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Xiaomi MiMo (API billing) Provider Empty Messages",
        provider: "xiaomi",
        model: "mimo-v2.5-pro",
        gate: Gate::Env("XIAOMI_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Xiaomi MiMo Token Plan (CN) Provider Empty Messages",
        provider: "xiaomi-token-plan-cn",
        model: "mimo-v2.5-pro",
        gate: Gate::Env("XIAOMI_TOKEN_PLAN_CN_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Xiaomi MiMo Token Plan (AMS) Provider Empty Messages",
        provider: "xiaomi-token-plan-ams",
        model: "mimo-v2.5-pro",
        gate: Gate::Env("XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Xiaomi MiMo Token Plan (SGP) Provider Empty Messages",
        provider: "xiaomi-token-plan-sgp",
        model: "mimo-v2.5-pro",
        gate: Gate::Env("XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Qwen Token Plan Provider Empty Messages",
        provider: "qwen-token-plan",
        model: "qwen3.7-max",
        gate: Gate::Env("QWEN_TOKEN_PLAN_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Qwen Token Plan Individual Provider Empty Messages",
        provider: "qwen-token-plan-individual",
        model: "qwen3.8-max",
        gate: Gate::Env("QWEN_TOKEN_PLAN_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Qwen Token Plan (CN) Provider Empty Messages",
        provider: "qwen-token-plan-cn",
        model: "qwen3.7-max",
        gate: Gate::Env("QWEN_TOKEN_PLAN_CN_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Kimi For Coding Provider Empty Messages",
        provider: "kimi-coding",
        model: "kimi-for-coding",
        gate: Gate::Env("KIMI_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Vercel AI Gateway Provider Empty Messages",
        provider: "vercel-ai-gateway",
        model: "google/gemini-2.5-flash",
        gate: Gate::Env("AI_GATEWAY_API_KEY"),
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
    EmptySuite {
        name: "Amazon Bedrock Provider Empty Messages",
        provider: "amazon-bedrock",
        model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
        gate: Gate::Bedrock,
        openai_completions_api: false,
        azure: false,
        reasoning_effort_high: false,
    },
];

/// The four shared TS case bodies. `case` is "<suite> (<model>): <it name>".
async fn test_empty_message(model: &pi_core::ai::types::Model, case: &str, options: &LiveOptions) {
    // Test with completely empty content array
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(Vec::new()),
            timestamp: now_millis(),
        })],
        tools: None,
    };

    let response = live_complete(model, &context, options).await;

    // Should either handle gracefully or return an error
    assert_eq!(response.role, RoleAssistant, "{case}: role");
    if response.stop_reason == StopReason::Error {
        assert!(response.error_message.is_some(), "{case}: errorMessage");
    }
    // else: `expect(response.content).toBeDefined()` — the Rust content field
    // is always defined (an owned Vec).
}

async fn test_empty_string_message(
    model: &pi_core::ai::types::Model,
    case: &str,
    options: &LiveOptions,
) {
    // Test with empty string content
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(String::new()),
            timestamp: now_millis(),
        })],
        tools: None,
    };

    let response = live_complete(model, &context, options).await;

    assert_eq!(response.role, RoleAssistant, "{case}: role");
    if response.stop_reason == StopReason::Error {
        assert!(response.error_message.is_some(), "{case}: errorMessage");
    }
}

async fn test_whitespace_only_message(
    model: &pi_core::ai::types::Model,
    case: &str,
    options: &LiveOptions,
) {
    // Test with whitespace-only content
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("   \n\t  ".to_string()),
            timestamp: now_millis(),
        })],
        tools: None,
    };

    let response = live_complete(model, &context, options).await;

    assert_eq!(response.role, RoleAssistant, "{case}: role");
    if response.stop_reason == StopReason::Error {
        assert!(response.error_message.is_some(), "{case}: errorMessage");
    }
}

async fn test_empty_assistant_message(
    model: &pi_core::ai::types::Model,
    case: &str,
    options: &LiveOptions,
) {
    // Test with empty assistant message in conversation flow
    // User -> Empty Assistant -> User
    let empty_assistant = AssistantMessage {
        role: RoleAssistant,
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage {
            input: 10,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 10,
            cost: UsageCost::default(),
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        timestamp: now_millis(),
        ..Default::default()
    };

    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("Hello, how are you?".to_string()),
                timestamp: now_millis(),
            }),
            Message::Assistant(Box::new(empty_assistant)),
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("Please respond this time.".to_string()),
                timestamp: now_millis(),
            }),
        ],
        tools: None,
    };

    let response = live_complete(model, &context, options).await;

    assert_eq!(response.role, RoleAssistant, "{case}: role");
    if response.stop_reason == StopReason::Error {
        assert!(response.error_message.is_some(), "{case}: errorMessage");
    } else {
        assert!(!response.content.is_empty(), "{case}: content.length > 0");
    }
}

async fn run_empty_cases(model: &pi_core::ai::types::Model, label: &str, options: &LiveOptions) {
    test_empty_message(
        model,
        &format!("{label}: should handle empty content array"),
        options,
    )
    .await;
    test_empty_string_message(
        model,
        &format!("{label}: should handle empty string content"),
        options,
    )
    .await;
    test_whitespace_only_message(
        model,
        &format!("{label}: should handle whitespace-only content"),
        options,
    )
    .await;
    test_empty_assistant_message(
        model,
        &format!("{label}: should handle empty assistant message in conversation"),
        options,
    )
    .await;
}

fn suite_options(suite: &EmptySuite, model: &pi_core::ai::types::Model) -> LiveOptions {
    let mut options = LiveOptions::default();
    if suite.azure {
        options.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
    }
    if suite.reasoning_effort_high {
        options.reasoning_effort = Some(LiveEffort::High);
    }
    options
}

#[tokio::test]
async fn empty_message_env_provider_matrix() {
    // The 26 env/credential-gated TS describes.
    for suite in EMPTY_SUITES {
        if !suite.gate.check().await {
            skip(suite.name, &suite.gate.env_description());
            continue;
        }
        let model = get_model_or_panic(suite.provider, suite.model);
        let model = if suite.openai_completions_api {
            as_openai_completions(&model)
        } else {
            model
        };
        let options = suite_options(suite, &model);
        run_empty_cases(&model, suite.name, &options).await;
    }
}

#[tokio::test]
async fn anthropic_oauth_provider_empty_messages() {
    // TS: describe("Anthropic OAuth Provider Empty Messages") with
    // it.skipIf(!anthropicOAuthToken).
    let Some(token) = live::resolve_api_key("anthropic").await else {
        skip(
            "Anthropic OAuth Provider Empty Messages",
            "~/.pi/agent/auth.json anthropic credentials",
        );
        return;
    };
    let llm = get_model_or_panic("anthropic", "claude-haiku-4-5");
    let options = LiveOptions::with_api_key(&token);
    run_empty_cases(&llm, "Anthropic OAuth Provider Empty Messages", &options).await;
}

#[tokio::test]
async fn github_copilot_provider_empty_messages() {
    // TS: describe("GitHub Copilot Provider Empty Messages") — two models,
    // four cases each, with per-it names prefixed by the model label.
    let Some(token) = live::resolve_api_key("github-copilot").await else {
        skip(
            "GitHub Copilot Provider Empty Messages",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
        return;
    };
    let options = LiveOptions::with_api_key(&token);
    for (label, model_id) in [
        ("claude-haiku-4.5", "claude-haiku-4.5"),
        ("claude-sonnet-4", "claude-sonnet-4.6"),
    ] {
        let llm = get_model_or_panic("github-copilot", model_id);
        run_empty_cases(
            &llm,
            &format!("GitHub Copilot Provider Empty Messages ({label})"),
            &options,
        )
        .await;
    }
}

#[tokio::test]
async fn openai_codex_provider_empty_messages() {
    // TS: describe("OpenAI Codex Provider Empty Messages") — gpt-5.5.
    let Some(token) = live::resolve_api_key("openai-codex").await else {
        skip(
            "OpenAI Codex Provider Empty Messages",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
        return;
    };
    let llm = get_model_or_panic("openai-codex", "gpt-5.5");
    let options = LiveOptions::with_api_key(&token);
    run_empty_cases(
        &llm,
        "OpenAI Codex Provider Empty Messages (gpt-5.5)",
        &options,
    )
    .await;
}
