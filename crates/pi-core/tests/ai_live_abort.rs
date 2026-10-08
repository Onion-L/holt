//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/abort.test.ts` ("AI Providers Abort Tests").
//!
//! 41 TS `it` cases: nineteen provider suites run "should abort mid-stream"
//! and "should handle immediate abort", and the Amazon Bedrock suite adds
//! "should handle abort then new message". The shared TS bodies
//! (`testAbortSignal`, `testImmediateAbort`, `testAbortThenNewMessage`) are
//! ported as provider-agnostic async fns driven through
//! `common::live::live_stream`/`live_complete` (the compat `stream`/
//! `complete` dispatch with the TS option extras).
//!
//! Gates translated verbatim: env vars via `live_env`, the azure/bedrock
//! helpers, and `resolveApiKey("openai-codex")` for the Codex OAuth suite.
//! Note the Anthropic suite in the TS source is gated on the
//! `ANTHROPIC_OAUTH_TOKEN` *env var* (not auth.json). Without credentials
//! each suite prints `SKIP: <suite> requires <ENV>` and the test passes.
//!
//! Deviations: vitest `{ retry: 3 }` has no Rust equivalent; the JS abort
//! thresholds use UTF-16 code-unit lengths (`common::live::js_length`).

mod common;

use common::live::{
    self, LiveEffort, LiveOptions, LiveThinking, as_openai_completions, get_model_or_panic,
    js_length, live_complete, live_env, live_stream, now_millis, skip,
};
use pi_core::ai::types::{Context, Message, Model, RoleUser, StopReason, UserContent, UserMessage};
use tokio_util::sync::CancellationToken;

fn user_message(content: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp: now_millis(),
    })
}

fn set_signal(options: &mut LiveOptions, signal: CancellationToken) {
    options.signal = Some(signal);
}

/// testAbortSignal from abort.test.ts.
async fn test_abort_signal(model: &Model, case: &str, options: LiveOptions) {
    let mut context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![user_message(
            "What is 15 + 27? Think step by step. Then list 50 first names.",
        )],
        tools: None,
    };

    let mut abort_fired = false;
    let mut text = String::new();
    let controller = CancellationToken::new();
    // TS streams with `{ ...options, signal }`; the follow-up request below
    // reuses the original `options` without the signal.
    let mut signaled = options.clone();
    set_signal(&mut signaled, controller.clone());
    let response = live_stream(model, &context, &signaled);
    while let Some(event) = response.next().await {
        // TS: `if (abortFired) return;` — the next event after the abort
        // exits the whole helper without further assertions.
        if abort_fired {
            return;
        }
        match &event {
            pi_core::ai::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                text.push_str(delta)
            }
            pi_core::ai::types::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                text.push_str(delta)
            }
            _ => {}
        }
        if js_length(&text) >= 50 {
            controller.cancel();
            abort_fired = true;
        }
    }
    let msg = response.result().await;

    // If we get here without throwing, the abort didn't work
    assert_eq!(msg.stop_reason, StopReason::Aborted, "{case}: stopReason");
    assert!(!msg.content.is_empty(), "{case}: content");

    context.messages.push(Message::Assistant(Box::new(msg)));
    context
        .messages
        .push(user_message("Please continue, but only generate 5 names."));

    let follow_up = live_complete(model, &context, &options).await;
    assert_eq!(
        follow_up.stop_reason,
        StopReason::Stop,
        "{case}: followUp stopReason"
    );
    assert!(!follow_up.content.is_empty(), "{case}: followUp content");
}

/// testImmediateAbort from abort.test.ts.
async fn test_immediate_abort(model: &Model, case: &str, mut options: LiveOptions) {
    let controller = CancellationToken::new();
    controller.cancel();
    set_signal(&mut options, controller);

    let context = Context {
        system_prompt: None,
        messages: vec![user_message("Hello")],
        tools: None,
    };

    let response = live_complete(model, &context, &options).await;
    assert_eq!(
        response.stop_reason,
        StopReason::Aborted,
        "{case}: stopReason"
    );
}

/// testAbortThenNewMessage from abort.test.ts.
async fn test_abort_then_new_message(model: &Model, case: &str, options: LiveOptions) {
    // First request: abort immediately before any response content arrives
    let controller = CancellationToken::new();
    controller.cancel();
    let mut abort_options = options.clone();
    set_signal(&mut abort_options, controller);

    let mut context = Context {
        system_prompt: None,
        messages: vec![user_message("Hello, how are you?")],
        tools: None,
    };

    let aborted_response = live_complete(model, &context, &abort_options).await;
    assert_eq!(
        aborted_response.stop_reason,
        StopReason::Aborted,
        "{case}: stopReason"
    );
    // The aborted message has empty content since we aborted before anything
    // arrived
    assert!(aborted_response.content.is_empty(), "{case}: content");

    // Add the aborted assistant message to context (this is what happens in
    // the real coding agent)
    context
        .messages
        .push(Message::Assistant(Box::new(aborted_response)));

    // Second request: send a new message - this should work even with the
    // aborted message in context
    context.messages.push(user_message("What is 2 + 2?"));

    let follow_up = live_complete(model, &context, &options).await;
    assert_eq!(
        follow_up.stop_reason,
        StopReason::Stop,
        "{case}: stopReason"
    );
    assert!(!follow_up.content.is_empty(), "{case}: content");
}

struct AbortSuite {
    name: &'static str,
    provider: &'static str,
    model: &'static str,
    env: &'static str,
    /// `{ ...baseModel, api: "openai-completions" }` override.
    openai_completions_api: bool,
    /// Pass `{ azureDeploymentName }` options.
    azure: bool,
    /// Options for the "abort mid-stream" case.
    mid: LiveOptions,
    /// Options for the remaining cases (also the follow-up request options
    /// of the mid-stream case).
    rest: LiveOptions,
}

async fn run_abort_suite(suite: AbortSuite) {
    // Resolve the gate the way the TS describe does.
    let gate_open = match suite.provider {
        "azure-openai-responses" => live::has_azure_openai_credentials(),
        "amazon-bedrock" => live::has_bedrock_credentials(),
        "openai-codex" => live::resolve_api_key("openai-codex").await.is_some(),
        _ => live_env(suite.env).is_some(),
    };
    if !gate_open {
        skip(suite.name, suite.env);
        return;
    }
    let model = get_model_or_panic(suite.provider, suite.model);
    let model = if suite.openai_completions_api {
        as_openai_completions(&model)
    } else {
        model
    };
    let mut rest = suite.rest.clone();
    if suite.azure {
        rest.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
    }
    let mut mid = suite.mid.clone();
    if suite.azure {
        mid.azure_deployment_name = live::resolve_azure_deployment_name(&model.id);
    }

    test_abort_signal(
        &model,
        &format!("{}: should abort mid-stream", suite.name),
        mid,
    )
    .await;
    test_immediate_abort(
        &model,
        &format!("{}: should handle immediate abort", suite.name),
        rest.clone(),
    )
    .await;
    if suite.provider == "amazon-bedrock" {
        test_abort_then_new_message(
            &model,
            &format!("{}: should handle abort then new message", suite.name),
            rest,
        )
        .await;
    }
}

#[tokio::test]
async fn abort_env_provider_matrix() {
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
    let anthropic_thinking = LiveOptions {
        // { thinkingEnabled: true, thinkingBudgetTokens: 2048 }
        thinking_enabled: Some(true),
        thinking_budget_tokens: Some(2048),
        ..Default::default()
    };
    let effort_high = LiveOptions {
        reasoning_effort: Some(LiveEffort::High),
        ..Default::default()
    };

    let suites = vec![
        AbortSuite {
            name: "Google Provider Abort",
            provider: "google",
            model: "gemini-2.5-flash",
            env: "GEMINI_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: google_thinking.clone(),
            rest: google_thinking,
        },
        AbortSuite {
            name: "OpenAI Completions Provider Abort",
            provider: "openai",
            model: "gpt-4o-mini",
            env: "OPENAI_API_KEY",
            openai_completions_api: true,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "OpenAI Responses Provider Abort",
            provider: "openai",
            model: "gpt-5-mini",
            env: "OPENAI_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Azure OpenAI Responses Provider Abort",
            provider: "azure-openai-responses",
            model: "gpt-4o-mini",
            env: "AZURE_OPENAI_API_KEY + AZURE_OPENAI_BASE_URL|AZURE_OPENAI_RESOURCE_NAME",
            openai_completions_api: false,
            azure: true,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            // TS gates this suite on the ANTHROPIC_OAUTH_TOKEN env var (the
            // compat env-key injection passes it as the request apiKey).
            name: "Anthropic Provider Abort",
            provider: "anthropic",
            model: "claude-sonnet-4-6",
            env: "ANTHROPIC_OAUTH_TOKEN",
            openai_completions_api: false,
            azure: false,
            mid: anthropic_thinking.clone(),
            rest: anthropic_thinking,
        },
        AbortSuite {
            name: "Mistral Provider Abort",
            provider: "mistral",
            model: "devstral-medium-latest",
            env: "MISTRAL_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Together AI Provider Abort",
            provider: "together",
            model: "moonshotai/Kimi-K2.6",
            env: "TOGETHER_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: effort_high.clone(),
            rest: effort_high,
        },
        AbortSuite {
            name: "Baseten Provider Abort",
            provider: "baseten",
            model: "zai-org/GLM-5.2",
            env: "BASETEN_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: LiveOptions {
                reasoning_effort: Some(LiveEffort::High),
                ..Default::default()
            },
            rest: LiveOptions {
                reasoning_effort: Some(LiveEffort::High),
                ..Default::default()
            },
        },
        AbortSuite {
            name: "MiniMax Provider Abort",
            provider: "minimax",
            model: "MiniMax-M2.7",
            env: "MINIMAX_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Xiaomi MiMo (API billing) Provider Abort",
            provider: "xiaomi",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Xiaomi MiMo Token Plan (CN) Provider Abort",
            provider: "xiaomi-token-plan-cn",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Xiaomi MiMo Token Plan (AMS) Provider Abort",
            provider: "xiaomi-token-plan-ams",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Xiaomi MiMo Token Plan (SGP) Provider Abort",
            provider: "xiaomi-token-plan-sgp",
            model: "mimo-v2.5-pro",
            env: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Qwen Token Plan Provider Abort",
            provider: "qwen-token-plan",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Qwen Token Plan Individual Provider Abort",
            provider: "qwen-token-plan-individual",
            model: "qwen3.8-max",
            env: "QWEN_TOKEN_PLAN_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Qwen Token Plan (CN) Provider Abort",
            provider: "qwen-token-plan-cn",
            model: "qwen3.7-max",
            env: "QWEN_TOKEN_PLAN_CN_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Kimi For Coding Provider Abort",
            provider: "kimi-coding",
            model: "kimi-for-coding",
            env: "KIMI_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Vercel AI Gateway Provider Abort",
            provider: "vercel-ai-gateway",
            model: "google/gemini-2.5-flash",
            env: "AI_GATEWAY_API_KEY",
            openai_completions_api: false,
            azure: false,
            mid: default.clone(),
            rest: default.clone(),
        },
        AbortSuite {
            name: "Amazon Bedrock Provider Abort",
            provider: "amazon-bedrock",
            model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            env: "AWS_PROFILE | AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY | AWS_BEARER_TOKEN_BEDROCK",
            openai_completions_api: false,
            azure: false,
            // "should abort mid-stream" passes { reasoning: "medium" }
            mid: LiveOptions {
                reasoning: Some(pi_core::ai::types::ThinkingLevel::Medium),
                ..Default::default()
            },
            rest: default,
        },
    ];

    for suite in suites {
        run_abort_suite(suite).await;
    }
}

#[tokio::test]
async fn openai_codex_provider_abort() {
    // TS: describe("OpenAI Codex Provider Abort") with
    // it.skipIf(!openaiCodexToken).
    let Some(token) = live::resolve_api_key("openai-codex").await else {
        skip(
            "OpenAI Codex Provider Abort",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
        return;
    };
    let model = get_model_or_panic("openai-codex", "gpt-5.5");
    let options = LiveOptions::with_api_key(&token);
    test_abort_signal(
        &model,
        "OpenAI Codex Provider Abort: should abort mid-stream",
        options.clone(),
    )
    .await;
    test_immediate_abort(
        &model,
        "OpenAI Codex Provider Abort: should handle immediate abort",
        options,
    )
    .await;
}
