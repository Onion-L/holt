//! Rust entry points for the credential-gated live suites
//! `pi-core/ai/test/tokens.test.ts` ("Token Statistics on Abort", 26 active
//! TS cases plus the 4 `it.skip` Xiaomi FIXME cases kept visible) and
//! `pi-core/ai/test/total-tokens.test.ts` ("totalTokens field", 35 TS cases).
//!
//! The shared TS bodies (`testTokensOnAbort`, `testTotalTokensWithCache`,
//! `logUsage`, `assertTotalTokensEqualsComponents`) are ported below and
//! driven through `common::live::live_stream`/`live_complete` (the compat
//! dispatch plus the TS option extras).
//!
//! Gates translated verbatim; the Xiaomi abort cases are skipped with the
//! TS FIXME reason (upstream does not send usage in `message_start`).
//! Without credentials each suite prints `SKIP: <suite> requires <ENV>` and
//! the test passes.
//!
//! Deviations: vitest `{ retry: 3, timeout: 30000 }` has no Rust equivalent;
//! the abort threshold uses UTF-16 code-unit length (`js_length`).

mod common;

use common::live::{
    self, LiveEffort, LiveOptions, LiveThinking, as_openai_completions, get_builtin_models,
    get_model_or_panic, js_length, live_complete, live_env, live_stream, now_millis, skip,
};
use pi_core::ai::types::{Context, Message, RoleUser, StopReason, Usage, UserContent, UserMessage};
use tokio_util::sync::CancellationToken;

fn user_message(content: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp: now_millis(),
    })
}

/// testTokensOnAbort from tokens.test.ts.
async fn test_tokens_on_abort(
    model: &pi_core::ai::types::Model,
    case: &str,
    options: &LiveOptions,
) {
    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message(
            "Write a long poem with 20 stanzas about the beauty of nature.",
        )],
        tools: None,
    };

    let controller = CancellationToken::new();
    let mut signaled = options.clone();
    signaled.signal = Some(controller.clone());
    let response = live_stream(model, &context, &signaled);

    let mut abort_fired = false;
    let mut text = String::new();
    while let Some(event) = response.next().await {
        if !abort_fired {
            match &event {
                pi_core::ai::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                    text.push_str(delta)
                }
                pi_core::ai::types::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    text.push_str(delta)
                }
                _ => {}
            }
            if js_length(&text) >= 1000 {
                abort_fired = true;
                controller.cancel();
            }
        }
    }

    let msg = response.result().await;

    assert_eq!(msg.stop_reason, StopReason::Aborted, "{case}: stopReason");

    // OpenAI providers, OpenAI Codex, zai, and Amazon Bedrock only send usage
    // in the final chunk, so when aborted they have no token stats. Anthropic
    // and Google send usage information early in the stream. MiniMax and Kimi
    // report input tokens but not output tokens differently on aborted
    // requests.
    let usage_zero = matches!(
        model.api.as_str(),
        "openai-completions"
            | "mistral-conversations"
            | "openai-responses"
            | "azure-openai-responses"
            | "openai-codex-responses"
    ) || matches!(
        model.provider.as_str(),
        "zai" | "amazon-bedrock" | "vercel-ai-gateway"
    );
    if usage_zero || model.provider == "minimax" {
        // MiniMax M2.7 does not report token usage for aborted requests.
        assert_eq!(msg.usage.input, 0, "{case}: usage.input");
        assert_eq!(msg.usage.output, 0, "{case}: usage.output");
    } else if model.provider == "kimi-coding" {
        // Kimi reports input tokens early but output tokens only in the
        // final chunk.
        assert!(msg.usage.input > 0, "{case}: usage.input");
        assert_eq!(msg.usage.output, 0, "{case}: usage.output");
    } else {
        assert!(msg.usage.input > 0, "{case}: usage.input");
        assert!(msg.usage.output > 0, "{case}: usage.output");

        // Some providers (Copilot) have zero cost rates
        if model.cost.rates.input.0 > 0.0 {
            assert!(msg.usage.cost.input.0 > 0.0, "{case}: usage.cost.input");
            assert!(msg.usage.cost.total.0 > 0.0, "{case}: usage.cost.total");
        }
    }
}

/// One TS describe of tokens.test.ts.
struct TokenSuite {
    case: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    options: LiveOptions,
}

#[tokio::test]
async fn tokens_on_abort_env_provider_matrix() {
    let default = LiveOptions::default();
    let google_thinking = LiveOptions {
        // { thinking: { enabled: true } }
        thinking: Some(LiveThinking {
            enabled: true,
            budget_tokens: None,
            level: None,
        }),
        ..Default::default()
    };
    let suites = vec![
        TokenSuite {
            case: "Google Provider: should include token stats when aborted mid-stream",
            provider: "google",
            model: "gemini-2.5-flash",
            env: "GEMINI_API_KEY",
            options: google_thinking,
        },
        TokenSuite {
            case: "OpenAI Completions Provider: should include token stats when aborted mid-stream",
            provider: "openai",
            model: "gpt-4o-mini",
            env: "OPENAI_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "OpenAI Responses Provider: should include token stats when aborted mid-stream",
            provider: "openai",
            model: "gpt-5.4-mini",
            env: "OPENAI_API_KEY",
            options: LiveOptions {
                reasoning_effort: Some(LiveEffort::Low),
                ..Default::default()
            },
        },
        TokenSuite {
            case: "Azure OpenAI Responses Provider: should include token stats when aborted mid-stream",
            provider: "azure-openai-responses",
            model: "gpt-4o-mini",
            env: "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
            options: LiveOptions {
                azure_deployment_name: live::resolve_azure_deployment_name("gpt-4o-mini"),
                ..Default::default()
            },
        },
        TokenSuite {
            case: "Anthropic Provider: should include token stats when aborted mid-stream",
            provider: "anthropic",
            model: "claude-sonnet-4-6",
            env: "ANTHROPIC_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "xAI Provider: should include token stats when aborted mid-stream",
            provider: "xai",
            model: "grok-4.3",
            env: "XAI_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Groq Provider: should include token stats when aborted mid-stream",
            provider: "groq",
            model: "openai/gpt-oss-20b",
            env: "GROQ_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Cloudflare Workers AI Provider: should include token stats when aborted mid-stream",
            provider: "cloudflare-workers-ai",
            model: "@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID",
            options: default.clone(),
        },
        TokenSuite {
            case: "Cloudflare AI Gateway Provider: should include token stats when aborted mid-stream",
            provider: "cloudflare-ai-gateway",
            model: "workers-ai/@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID",
            options: default.clone(),
        },
        TokenSuite {
            case: "Hugging Face Provider: should include token stats when aborted mid-stream",
            provider: "huggingface",
            model: "moonshotai/Kimi-K2.5",
            env: "HF_TOKEN",
            options: default.clone(),
        },
        TokenSuite {
            case: "Together AI Provider: should include token stats when aborted mid-stream",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Baseten Provider: should include token stats when aborted mid-stream",
            provider: "baseten",
            model: "zai-org/GLM-5.2",
            env: "BASETEN_API_KEY",
            options: LiveOptions {
                reasoning_effort: Some(LiveEffort::High),
                ..Default::default()
            },
        },
        TokenSuite {
            case: "zAI Provider: should include token stats when aborted mid-stream",
            provider: "zai",
            model: "glm-5.2",
            env: "ZAI_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Mistral Provider: should include token stats when aborted mid-stream",
            provider: "mistral",
            model: "devstral-medium-latest",
            env: "MISTRAL_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "MiniMax Provider: should include token stats when aborted mid-stream",
            provider: "minimax",
            model: "MiniMax-M2.7",
            env: "MINIMAX_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Kimi For Coding Provider: should include token stats when aborted mid-stream",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Vercel AI Gateway Provider: should include token stats when aborted mid-stream",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Qwen Token Plan Provider: should include token stats when aborted mid-stream",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Qwen Token Plan Individual Provider: should include token stats when aborted mid-stream",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Qwen Token Plan (CN) Provider: should include token stats when aborted mid-stream",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            options: default.clone(),
        },
        TokenSuite {
            case: "Amazon Bedrock Provider: should include token stats when aborted mid-stream",
            provider: "amazon-bedrock",
            model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            env: "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
            options: default,
        },
    ];

    for suite in suites {
        let gated = if suite.provider == "azure-openai-responses" {
            live::has_azure_openai_credentials()
        } else if suite.provider == "amazon-bedrock" {
            live::has_bedrock_credentials()
        } else if suite.provider == "cloudflare-workers-ai" {
            live::has_cloudflare_workers_ai_credentials()
        } else if suite.provider == "cloudflare-ai-gateway" {
            live::has_cloudflare_ai_gateway_credentials()
        } else {
            live_env(suite.env).is_some()
        };
        if !gated {
            skip(suite.case, suite.env);
            continue;
        }
        let model = get_model_or_panic(suite.provider, suite.model);
        let model = if suite.provider == "openai" && suite.model == "gpt-4o-mini" {
            as_openai_completions(&model)
        } else {
            model
        };
        test_tokens_on_abort(&model, suite.case, &suite.options).await;
    }

    // Cerebras picks a preferred model from the generated catalog.
    if live_env("CEREBRAS_API_KEY").is_none() {
        skip(
            "Cerebras Provider: should include token stats when aborted mid-stream",
            "CEREBRAS_API_KEY",
        );
    } else {
        let preferred_cerebras_model_ids = ["gpt-oss-120b", "zai-glm-4.7", "llama3.1-8b"];
        let cerebras_models = get_builtin_models("cerebras");
        let llm = cerebras_models
            .iter()
            .find(|model| preferred_cerebras_model_ids.contains(&model.id.as_str()))
            .unwrap_or_else(|| {
                cerebras_models
                    .first()
                    .expect("No Cerebras models available")
            });
        test_tokens_on_abort(
            llm,
            "Cerebras Provider: should include token stats when aborted mid-stream",
            &LiveOptions::default(),
        )
        .await;
    }

    // FIXME(xiaomi) — the TS suite skips all four Xiaomi abort cases:
    // Xiaomi's Anthropic-compatible stream does not populate usage in the
    // message_start event the way Anthropic does — usage only arrives at
    // message_stop. Aborting mid-stream therefore loses input/output token
    // counts. Re-enable once upstream sends usage in message_start.
    for provider in [
        "xiaomi",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
    ] {
        eprintln!(
            "SKIP: Xiaomi MiMo ({provider}) token stats on aborted mid-stream — TS it.skip FIXME(xiaomi): upstream sends usage only at message_stop"
        );
    }
}

#[tokio::test]
async fn tokens_on_abort_oauth_providers() {
    // Anthropic OAuth
    let anthropic_token = live::resolve_api_key("anthropic").await;
    if anthropic_token.is_none() {
        skip(
            "Anthropic OAuth Provider: should include token stats when aborted mid-stream",
            "~/.pi/agent/auth.json anthropic credentials",
        );
    } else {
        let llm = get_model_or_panic("anthropic", "claude-sonnet-4-6");
        test_tokens_on_abort(
            &llm,
            "Anthropic OAuth Provider: should include token stats when aborted mid-stream",
            &LiveOptions::with_api_key(anthropic_token.as_deref().unwrap_or_default()),
        )
        .await;
    }

    // GitHub Copilot (two models)
    let copilot_token = live::resolve_api_key("github-copilot").await;
    if copilot_token.is_none() {
        skip(
            "GitHub Copilot Provider: should include token stats when aborted mid-stream",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
    } else {
        for model_id in ["claude-haiku-4.5", "claude-sonnet-4.6"] {
            let llm = get_model_or_panic("github-copilot", model_id);
            test_tokens_on_abort(
                &llm,
                &format!(
                    "GitHub Copilot Provider ({model_id}): should include token stats when aborted mid-stream"
                ),
                &LiveOptions::with_api_key(copilot_token.as_deref().unwrap_or_default()),
            )
            .await;
        }
    }

    // OpenAI Codex
    let codex_token = live::resolve_api_key("openai-codex").await;
    if codex_token.is_none() {
        skip(
            "OpenAI Codex Provider: should include token stats when aborted mid-stream",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
    } else {
        let llm = get_model_or_panic("openai-codex", "gpt-5.5");
        test_tokens_on_abort(
            &llm,
            "OpenAI Codex Provider (gpt-5.5): should include token stats when aborted mid-stream",
            &LiveOptions::with_api_key(codex_token.as_deref().unwrap_or_default()),
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// total-tokens.test.ts
// ---------------------------------------------------------------------------

// Generate a long system prompt to trigger caching (>2k bytes for most
// providers) — verbatim from the TS suite.
fn long_system_prompt() -> String {
    let paragraph = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris.";
    format!(
        "You are a helpful assistant. Be concise in your responses.\n\nHere is some additional context that makes this system prompt long enough to trigger caching:\n\n{}\n\nRemember: Always be helpful and concise.",
        vec![paragraph; 50].join("\n\n")
    )
}

/// testTotalTokensWithCache from total-tokens.test.ts.
async fn test_total_tokens_with_cache(
    model: &pi_core::ai::types::Model,
    case: &str,
    options: &LiveOptions,
) -> (Usage, Usage) {
    // First request - no cache
    let context1 = Context {
        system_prompt: Some(long_system_prompt()),
        messages: vec![user_message("What is 2 + 2? Reply with just the number.")],
        tools: None,
    };

    let response1 = live_complete(model, &context1, options).await;
    assert_eq!(
        response1.stop_reason,
        StopReason::Stop,
        "{case}: stopReason"
    );

    // Second request - should trigger cache read (same system prompt, add
    // conversation)
    let mut context2 = Context {
        system_prompt: Some(long_system_prompt()),
        messages: context1.messages.clone(),
        tools: None,
    };
    context2
        .messages
        .push(Message::Assistant(Box::new(response1.clone())));
    context2
        .messages
        .push(user_message("What is 3 + 3? Reply with just the number."));

    let response2 = live_complete(model, &context2, options).await;
    assert_eq!(
        response2.stop_reason,
        StopReason::Stop,
        "{case}: stopReason"
    );

    (response1.usage, response2.usage)
}

/// logUsage from total-tokens.test.ts.
fn log_usage(label: &str, usage: &Usage) {
    let computed = usage.input + usage.output + usage.cache_read + usage.cache_write;
    println!(
        "  {label}:\n    input: {}, output: {}, cacheRead: {}, cacheWrite: {}\n    totalTokens: {}, computed: {}",
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.total_tokens,
        computed
    );
}

/// assertTotalTokensEqualsComponents from total-tokens.test.ts.
fn assert_total_tokens_equals_components(usage: &Usage, case: &str) {
    let computed = usage.input + usage.output + usage.cache_read + usage.cache_write;
    assert_eq!(usage.total_tokens, computed, "{case}: totalTokens");
}

struct TotalTokenSuite {
    case: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    /// Pass `apiKey: process.env.<ENV>` explicitly (as the TS suites do).
    explicit_key_env: Option<&'static str>,
    /// `{ reasoningEffort: "high" }` extras.
    reasoning_effort_high: bool,
    /// Anthropic suites additionally assert cache activity.
    expect_cache: bool,
}

async fn run_total_token_suite(suite: TotalTokenSuite) {
    let gated = if suite.provider == "azure-openai-responses" {
        live::has_azure_openai_credentials()
    } else if suite.provider == "cloudflare-workers-ai" {
        live::has_cloudflare_workers_ai_credentials()
    } else if suite.provider == "cloudflare-ai-gateway" {
        live::has_cloudflare_ai_gateway_credentials()
    } else if suite.provider == "amazon-bedrock" {
        live::has_bedrock_credentials()
    } else {
        live_env(suite.env).is_some()
    };
    if !gated {
        skip(suite.case, suite.env);
        return;
    }
    let model = get_model_or_panic(suite.provider, suite.model);
    let model = if suite.provider == "openai" && suite.model == "gpt-4o-mini" {
        as_openai_completions(&model)
    } else {
        model
    };
    let mut options = LiveOptions::default();
    if let Some(key_env) = suite.explicit_key_env {
        options.api_key = live_env(key_env);
    }
    if suite.reasoning_effort_high {
        options.reasoning_effort = Some(LiveEffort::High);
    }

    println!("\n{} / {}:", model.provider, model.id);
    let (first, second) = test_total_tokens_with_cache(&model, suite.case, &options).await;
    log_usage("First request", &first);
    log_usage("Second request", &second);
    assert_total_tokens_equals_components(&first, suite.case);
    assert_total_tokens_equals_components(&second, suite.case);
    if suite.expect_cache {
        // Anthropic should have cache activity
        let has_cache = second.cache_read > 0 || second.cache_write > 0 || first.cache_write > 0;
        assert!(has_cache, "{}: cache activity", suite.case);
    }
}

#[tokio::test]
async fn total_tokens_env_provider_matrix() {
    let suites = vec![
        TotalTokenSuite {
            case: "Anthropic (API Key): claude-sonnet-4-5 - should return totalTokens equal to sum of components",
            provider: "anthropic",
            model: "claude-sonnet-4-5",
            env: "ANTHROPIC_API_KEY",
            explicit_key_env: Some("ANTHROPIC_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: true,
        },
        TotalTokenSuite {
            case: "OpenAI Completions: gpt-4o-mini - should return totalTokens equal to sum of components",
            provider: "openai",
            model: "gpt-4o-mini",
            env: "OPENAI_API_KEY",
            explicit_key_env: None,
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "OpenAI Responses: gpt-4o - should return totalTokens equal to sum of components",
            provider: "openai",
            model: "gpt-4o",
            env: "OPENAI_API_KEY",
            explicit_key_env: None,
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Azure OpenAI Responses: gpt-4o-mini - should return totalTokens equal to sum of components",
            provider: "azure-openai-responses",
            model: "gpt-4o-mini",
            env: "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
            explicit_key_env: None,
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Google: gemini-2.5-flash - should return totalTokens equal to sum of components",
            provider: "google",
            model: "gemini-2.5-flash",
            env: "GEMINI_API_KEY",
            explicit_key_env: None,
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "xAI: grok-4.3 - should return totalTokens equal to sum of components",
            provider: "xai",
            model: "grok-4.3",
            env: "XAI_API_KEY",
            explicit_key_env: Some("XAI_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Groq: openai/gpt-oss-120b - should return totalTokens equal to sum of components",
            provider: "groq",
            model: "openai/gpt-oss-120b",
            env: "GROQ_API_KEY",
            explicit_key_env: Some("GROQ_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Cerebras: gpt-oss-120b - should return totalTokens equal to sum of components",
            provider: "cerebras",
            model: "gpt-oss-120b",
            env: "CEREBRAS_API_KEY",
            explicit_key_env: Some("CEREBRAS_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Cloudflare Workers AI: @cf/moonshotai/kimi-k2.6 - should return totalTokens equal to sum of components",
            provider: "cloudflare-workers-ai",
            model: "@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID",
            explicit_key_env: Some("CLOUDFLARE_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Cloudflare AI Gateway: workers-ai/@cf/moonshotai/kimi-k2.6 - should return totalTokens equal to sum of components",
            provider: "cloudflare-ai-gateway",
            model: "workers-ai/@cf/moonshotai/kimi-k2.6",
            env: "CLOUDFLARE_API_KEY + CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_GATEWAY_ID",
            explicit_key_env: Some("CLOUDFLARE_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Hugging Face: Kimi-K2.5 - should return totalTokens equal to sum of components",
            provider: "huggingface",
            model: "moonshotai/Kimi-K2.5",
            env: "HF_TOKEN",
            explicit_key_env: Some("HF_TOKEN"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Together AI: Kimi-K2.6 - should return totalTokens equal to sum of components",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            explicit_key_env: Some("TOGETHER_API_KEY"),
            reasoning_effort_high: true,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Baseten: GLM 5.2 - should return totalTokens equal to sum of components",
            provider: "baseten",
            model: "zai-org/GLM-5.2",
            env: "BASETEN_API_KEY",
            explicit_key_env: Some("BASETEN_API_KEY"),
            reasoning_effort_high: true,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "z.ai: glm-5.2 - should return totalTokens equal to sum of components",
            provider: "zai",
            model: "glm-5.2",
            env: "ZAI_API_KEY",
            explicit_key_env: Some("ZAI_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Mistral: devstral-medium-latest - should return totalTokens equal to sum of components",
            provider: "mistral",
            model: "devstral-medium-latest",
            env: "MISTRAL_API_KEY",
            explicit_key_env: Some("MISTRAL_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "MiniMax: MiniMax-M2.7 - should return totalTokens equal to sum of components",
            provider: "minimax",
            model: "MiniMax-M2.7",
            env: "MINIMAX_API_KEY",
            explicit_key_env: Some("MINIMAX_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Xiaomi MiMo (API billing): mimo-v2.5-pro - should return totalTokens equal to sum of components",
            provider: "xiaomi",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_API_KEY",
            explicit_key_env: Some("XIAOMI_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Xiaomi MiMo Token Plan (CN): mimo-v2.5-pro - should return totalTokens equal to sum of components",
            provider: "xiaomi-token-plan-cn",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            explicit_key_env: Some("XIAOMI_TOKEN_PLAN_CN_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Xiaomi MiMo Token Plan (AMS): mimo-v2.5-pro - should return totalTokens equal to sum of components",
            provider: "xiaomi-token-plan-ams",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            explicit_key_env: Some("XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Xiaomi MiMo Token Plan (SGP): mimo-v2.5-pro - should return totalTokens equal to sum of components",
            provider: "xiaomi-token-plan-sgp",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            explicit_key_env: Some("XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Qwen Token Plan: qwen3.7-max - should return totalTokens equal to sum of components",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            explicit_key_env: Some("QWEN_TOKEN_PLAN_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Qwen Token Plan Individual: qwen3.8-max - should return totalTokens equal to sum of components",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            explicit_key_env: Some("QWEN_TOKEN_PLAN_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Qwen Token Plan (CN): qwen3.7-max - should return totalTokens equal to sum of components",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            explicit_key_env: Some("QWEN_TOKEN_PLAN_CN_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Kimi For Coding: kimi-for-coding - should return totalTokens equal to sum of components",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            explicit_key_env: Some("KIMI_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Vercel AI Gateway: google/gemini-2.5-flash - should return totalTokens equal to sum of components",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            explicit_key_env: Some("AI_GATEWAY_API_KEY"),
            reasoning_effort_high: false,
            expect_cache: false,
        },
        TotalTokenSuite {
            case: "Amazon Bedrock: claude-sonnet-4-5 - should return totalTokens equal to sum of components",
            provider: "amazon-bedrock",
            model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            env: "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
            explicit_key_env: None,
            reasoning_effort_high: false,
            expect_cache: false,
        },
    ];

    for suite in suites {
        run_total_token_suite(suite).await;
    }
}

#[tokio::test]
async fn total_tokens_openrouter() {
    // TS: describe.skipIf(!process.env.OPENROUTER_API_KEY) — five cases
    // (deepseek/deepseek-chat appears twice in the TS source).
    let Some(key) = live_env("OPENROUTER_API_KEY") else {
        skip("OpenRouter totalTokens", "OPENROUTER_API_KEY");
        return;
    };
    let options = LiveOptions::with_api_key(&key);
    for model_id in [
        "anthropic/claude-sonnet-4",
        "deepseek/deepseek-chat",
        "mistralai/mistral-small-3.2-24b-instruct",
        "google/gemini-2.5-flash",
        "deepseek/deepseek-chat",
    ] {
        let model = get_model_or_panic("openrouter", model_id);
        let case = format!(
            "OpenRouter: {model_id} - should return totalTokens equal to sum of components"
        );
        println!("\nOpenRouter / {model_id}:");
        let (first, second) = test_total_tokens_with_cache(&model, &case, &options).await;
        log_usage("First request", &first);
        log_usage("Second request", &second);
        assert_total_tokens_equals_components(&first, &case);
        assert_total_tokens_equals_components(&second, &case);
    }
}

#[tokio::test]
async fn total_tokens_oauth_providers() {
    // Anthropic OAuth
    let anthropic_token = live::resolve_api_key("anthropic").await;
    if anthropic_token.is_none() {
        skip(
            "Anthropic (OAuth): claude-sonnet-4 - should return totalTokens equal to sum of components",
            "~/.pi/agent/auth.json anthropic credentials",
        );
    } else {
        let llm = get_model_or_panic("anthropic", "claude-sonnet-4-6");
        let options = LiveOptions::with_api_key(anthropic_token.as_deref().unwrap_or_default());
        let case = "Anthropic (OAuth): claude-sonnet-4 - should return totalTokens equal to sum of components";
        println!("\nAnthropic OAuth / {}:", llm.id);
        let (first, second) = test_total_tokens_with_cache(&llm, case, &options).await;
        log_usage("First request", &first);
        log_usage("Second request", &second);
        assert_total_tokens_equals_components(&first, case);
        assert_total_tokens_equals_components(&second, case);
        let has_cache = second.cache_read > 0 || second.cache_write > 0 || first.cache_write > 0;
        assert!(has_cache, "{case}: cache activity");
    }

    // GitHub Copilot (two models)
    let copilot_token = live::resolve_api_key("github-copilot").await;
    if copilot_token.is_none() {
        skip(
            "GitHub Copilot (OAuth): totalTokens equal to sum of components",
            "~/.pi/agent/auth.json github-copilot credentials",
        );
    } else {
        for model_id in ["claude-haiku-4.5", "claude-sonnet-4.6"] {
            let llm = get_model_or_panic("github-copilot", model_id);
            let options = LiveOptions::with_api_key(copilot_token.as_deref().unwrap_or_default());
            let case = format!(
                "GitHub Copilot ({model_id}): should return totalTokens equal to sum of components"
            );
            println!("\nGitHub Copilot / {}:", llm.id);
            let (first, second) = test_total_tokens_with_cache(&llm, &case, &options).await;
            log_usage("First request", &first);
            log_usage("Second request", &second);
            assert_total_tokens_equals_components(&first, &case);
            assert_total_tokens_equals_components(&second, &case);
        }
    }

    // OpenAI Codex
    let codex_token = live::resolve_api_key("openai-codex").await;
    if codex_token.is_none() {
        skip(
            "OpenAI Codex (OAuth): gpt-5.5 - should return totalTokens equal to sum of components",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
    } else {
        let llm = get_model_or_panic("openai-codex", "gpt-5.5");
        let options = LiveOptions::with_api_key(codex_token.as_deref().unwrap_or_default());
        let case =
            "OpenAI Codex (OAuth): gpt-5.5 - should return totalTokens equal to sum of components";
        println!("\nOpenAI Codex / {}:", llm.id);
        let (first, second) = test_total_tokens_with_cache(&llm, case, &options).await;
        log_usage("First request", &first);
        log_usage("Second request", &second);
        assert_total_tokens_equals_components(&first, case);
        assert_total_tokens_equals_components(&second, case);
    }
}
