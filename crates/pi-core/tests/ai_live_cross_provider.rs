//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/cross-provider-handoff.test.ts`
//! ("Cross-Provider Handoff").
//!
//! The suite generates a fresh fixture per provider/model pair (user →
//! assistant tool-call → tool result → final assistant, via `completeSimple`
//! with `reasoning: "high"` on reasoning-capable models), then sends every
//! other provider's context to each target and requires zero failures. The
//! TS `beforeAll` fixture generation becomes a helper both tests call (the TS
//! source notes fixtures are generated fresh on each run).
//!
//! Requests run through `pi_core::ai::compat::complete_simple` — the same
//! global entry the TypeScript suite uses; `SimpleStreamOptions` carries all
//! the options the suite passes (apiKey, reasoning, headers, onPayload).
//!
//! Gates translated verbatim: the describe-level `hasAnyApiKey()` is the
//! synchronous env-only check over the provider pairs (`getEnvApiKey`, the
//! azure/cloudflare helpers, plus the gateway upstream-key env vars). Without
//! any key the suite prints `SKIP: ...` and the test passes.

mod common;

use common::live::{self, now_millis, skip};
use pi_core::ai::compat::complete_simple;
use pi_core::ai::env_api_keys::get_env_api_key;
use pi_core::ai::providers::builtin::get_builtin_model;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, Context, Message, Model, ProviderHeaders,
    ProviderRequestOptions, RoleToolResult, RoleUser, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, Tool, ToolResultMessage, UserContent, UserMessage,
};
use std::collections::BTreeMap;

/// The double_number test tool (TypeBox `Type.Object({ value: Type.Number() })`).
fn test_tool() -> Tool {
    Tool {
        name: "double_number".to_string(),
        description: "Doubles a number and returns the result".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": {
                "value": { "type": "number", "description": "A number to double" }
            }
        }),
        constrained_sampling: None,
    }
}

struct ProviderModelPair {
    provider: &'static str,
    model: &'static str,
    label: &'static str,
    api_override: Option<&'static str>,
    upstream_api_key_env: Option<&'static str>,
}

const PROVIDER_MODEL_PAIRS: &[ProviderModelPair] = &[
    // Anthropic
    ProviderModelPair {
        provider: "anthropic",
        model: "claude-sonnet-4-5",
        label: "anthropic-claude-sonnet-4-5",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Google
    ProviderModelPair {
        provider: "google",
        model: "gemini-3-flash-preview",
        label: "google-gemini-3-flash-preview",
        api_override: None,
        upstream_api_key_env: None,
    },
    // OpenAI
    ProviderModelPair {
        provider: "openai",
        model: "gpt-4o-mini",
        label: "openai-completions-gpt-4o-mini",
        api_override: Some("openai-completions"),
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "openai",
        model: "gpt-5-mini",
        label: "openai-responses-gpt-5-mini",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "azure-openai-responses",
        model: "gpt-4o-mini",
        label: "azure-openai-responses-gpt-4o-mini",
        api_override: None,
        upstream_api_key_env: None,
    },
    // OpenAI Codex
    ProviderModelPair {
        provider: "openai-codex",
        model: "gpt-5.5",
        label: "openai-codex-gpt-5.5",
        api_override: None,
        upstream_api_key_env: None,
    },
    // GitHub Copilot
    ProviderModelPair {
        provider: "github-copilot",
        model: "claude-sonnet-4.5",
        label: "copilot-claude-sonnet-4.5",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "github-copilot",
        model: "gpt-5.1-codex",
        label: "copilot-gpt-5.1-codex",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "github-copilot",
        model: "gemini-3-flash-preview",
        label: "copilot-gemini-3-flash-preview",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "github-copilot",
        model: "grok-code-fast-1",
        label: "copilot-grok-code-fast-1",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Amazon Bedrock
    ProviderModelPair {
        provider: "amazon-bedrock",
        model: "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
        label: "bedrock-claude-sonnet-4-5",
        api_override: None,
        upstream_api_key_env: None,
    },
    // xAI
    ProviderModelPair {
        provider: "xai",
        model: "grok-4.3",
        label: "xai-grok-4.3",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Cerebras
    ProviderModelPair {
        provider: "cerebras",
        model: "zai-glm-4.7",
        label: "cerebras-zai-glm-4.7",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Cloudflare Workers AI
    ProviderModelPair {
        provider: "cloudflare-workers-ai",
        model: "@cf/moonshotai/kimi-k2.6",
        label: "cloudflare-kimi-k2.6",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Cloudflare AI Gateway
    ProviderModelPair {
        provider: "cloudflare-ai-gateway",
        model: "workers-ai/@cf/moonshotai/kimi-k2.6",
        label: "cloudflare-gateway-kimi-k2.6",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "cloudflare-ai-gateway",
        model: "claude-sonnet-4-5",
        label: "cloudflare-gateway-claude-sonnet-4-5",
        api_override: None,
        upstream_api_key_env: Some("ANTHROPIC_API_KEY"),
    },
    ProviderModelPair {
        provider: "cloudflare-ai-gateway",
        model: "gpt-5.1",
        label: "cloudflare-gateway-gpt-5.1",
        api_override: None,
        upstream_api_key_env: Some("OPENAI_API_KEY"),
    },
    // Groq
    ProviderModelPair {
        provider: "groq",
        model: "openai/gpt-oss-120b",
        label: "groq-gpt-oss-120b",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Hugging Face
    ProviderModelPair {
        provider: "huggingface",
        model: "moonshotai/Kimi-K2.5",
        label: "huggingface-kimi-k2.5",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Together AI
    ProviderModelPair {
        provider: "together",
        model: "moonshotai/Kimi-K2.6",
        label: "together-kimi-k2.6",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Baseten
    ProviderModelPair {
        provider: "baseten",
        model: "zai-org/GLM-5.2",
        label: "baseten-glm-5.2",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Kimi For Coding
    ProviderModelPair {
        provider: "kimi-coding",
        model: "kimi-for-coding",
        label: "kimi-for-coding",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Mistral
    ProviderModelPair {
        provider: "mistral",
        model: "devstral-medium-latest",
        label: "mistral-devstral-medium",
        api_override: None,
        upstream_api_key_env: None,
    },
    // MiniMax
    // Note: the TS source reuses the "minimax-m2.7" label for both MiniMax
    // suites, so the second fixture overwrites the first in the context map.
    ProviderModelPair {
        provider: "minimax",
        model: "MiniMax-M2.7",
        label: "minimax-m2.7",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "minimax-cn",
        model: "MiniMax-M2.7",
        label: "minimax-m2.7",
        api_override: None,
        upstream_api_key_env: None,
    },
    // OpenCode Zen
    ProviderModelPair {
        provider: "opencode",
        model: "big-pickle",
        label: "zen-big-pickle",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "opencode",
        model: "claude-sonnet-4-5",
        label: "zen-claude-sonnet-4-5",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "opencode",
        model: "gemini-3-flash",
        label: "zen-gemini-3-flash",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "opencode",
        model: "glm-4.7-free",
        label: "zen-glm-4.7-free",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "opencode",
        model: "gpt-5.2-codex",
        label: "zen-gpt-5.2-codex",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "opencode",
        model: "minimax-m2.1-free",
        label: "zen-minimax-m2.1-free",
        api_override: None,
        upstream_api_key_env: None,
    },
    // OpenCode Go
    ProviderModelPair {
        provider: "opencode-go",
        model: "kimi-k2.5",
        label: "go-kimi-k2.5",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "opencode-go",
        model: "minimax-m2.5",
        label: "go-minimax-m2.5",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Xiaomi MiMo
    ProviderModelPair {
        provider: "xiaomi",
        model: "mimo-v2.5-pro",
        label: "xiaomi-mimo-v2.5-pro",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "xiaomi-token-plan-cn",
        model: "mimo-v2.5-pro",
        label: "xiaomi-token-plan-cn-mimo-v2.5-pro",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "xiaomi-token-plan-ams",
        model: "mimo-v2.5-pro",
        label: "xiaomi-token-plan-ams-mimo-v2.5-pro",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "xiaomi-token-plan-sgp",
        model: "mimo-v2.5-pro",
        label: "xiaomi-token-plan-sgp-mimo-v2.5-pro",
        api_override: None,
        upstream_api_key_env: None,
    },
    // Qwen Token Plan
    ProviderModelPair {
        provider: "qwen-token-plan",
        model: "qwen3.7-max",
        label: "qwen-token-plan-qwen3.7-max",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "qwen-token-plan-cn",
        model: "qwen3.7-max",
        label: "qwen-token-plan-cn-qwen3.7-max",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "qwen-token-plan-individual",
        model: "qwen3.8-max",
        label: "qwen-token-plan-individual-qwen3.8-max",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "qwen-token-plan-individual",
        model: "deepseek-v4-flash-0731",
        label: "qwen-token-plan-individual-deepseek-v4-flash-0731",
        api_override: None,
        upstream_api_key_env: None,
    },
    ProviderModelPair {
        provider: "qwen-token-plan-individual",
        model: "glm-5.2",
        label: "qwen-token-plan-individual-glm-5.2",
        api_override: None,
        upstream_api_key_env: None,
    },
];

/// The generated fixture messages for one provider pair (the TS
/// `CachedContext` minus the display-only metadata fields).
struct CachedContext {
    messages: Vec<Message>,
}

fn user_message(content: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp: now_millis(),
    })
}

/// getApiKey from cross-provider-handoff.test.ts: OAuth storage first, then
/// env vars.
async fn get_api_key(provider: &str) -> Option<String> {
    if let Some(oauth_key) = live::resolve_api_key(provider).await {
        return Some(oauth_key);
    }
    get_env_api_key(provider, None)
}

/// hasApiKey from cross-provider-handoff.test.ts: synchronous check for API
/// key availability (env vars only, for skipIf).
fn has_api_key(pair: &ProviderModelPair) -> bool {
    if pair.provider == "azure-openai-responses" {
        return live::has_azure_openai_credentials();
    }
    if pair.provider == "cloudflare-workers-ai" {
        return live::has_cloudflare_workers_ai_credentials();
    }
    if pair.provider == "cloudflare-ai-gateway" {
        if !live::has_cloudflare_ai_gateway_credentials() {
            return false;
        }
        return match pair.upstream_api_key_env {
            Some(env) => common::live::live_env(env).is_some(),
            None => true,
        };
    }
    get_env_api_key(pair.provider, None).is_some()
}

/// getHeaders from cross-provider-handoff.test.ts.
fn get_headers(pair: &ProviderModelPair) -> Option<ProviderHeaders> {
    let env = pair.upstream_api_key_env?;
    let upstream_api_key = common::live::live_env(env)?;
    Some(
        [(
            "Authorization".to_string(),
            Some(format!("Bearer {upstream_api_key}")),
        )]
        .into_iter()
        .collect(),
    )
}

/// dumpFailurePayload from cross-provider-handoff.test.ts.
fn dump_failure_payload(
    label: &str,
    error: &str,
    payload: Option<&serde_json::Value>,
    messages: &[Message],
) {
    let filename = format!("/tmp/pi-handoff-{label}-{}.json", now_millis());
    let body = serde_json::json!({
        "label": label,
        "error": error,
        "payload": payload,
        "messages": messages,
    });
    if let Ok(body) = serde_json::to_string_pretty(&body)
        && std::fs::write(&filename, body).is_ok()
    {
        println!("Wrote failure payload to {filename}");
    }
}

fn simple_options(
    api_key: &str,
    headers: Option<ProviderHeaders>,
    supports_reasoning: bool,
    on_payload: Option<pi_core::ai::types::OnPayloadCallback>,
) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(api_key.to_string()),
                headers,
                on_payload,
                ..Default::default()
            },
            ..Default::default()
        },
        reasoning: supports_reasoning.then_some(pi_core::ai::types::ThinkingLevel::High),
        ..Default::default()
    }
}

/// generateContext from cross-provider-handoff.test.ts: makes a real API
/// call to get authentic tool call IDs and thinking blocks.
async fn generate_context(
    pair: &ProviderModelPair,
    api_key: &str,
) -> Option<(Vec<Message>, String)> {
    let base_model = get_builtin_model(pair.provider, pair.model)?;
    let model = match pair.api_override {
        Some(api) => Model {
            api: api.to_string(),
            ..base_model
        },
        None => base_model,
    };

    let user_message_value =
        user_message("Please double the number 21 using the double_number tool.");
    let supports_reasoning = model.reasoning;
    let headers = get_headers(pair);
    let (captured, on_payload) = live::payload_capture();

    let assistant_response: AssistantMessage = complete_simple(
        &model,
        &Context {
            system_prompt: Some(
                "You are a helpful assistant. Use the provided tool to complete the task."
                    .to_string(),
            ),
            messages: vec![user_message_value.clone()],
            tools: Some(vec![test_tool()]),
        },
        Some(simple_options(
            api_key,
            headers.clone(),
            supports_reasoning,
            Some(on_payload),
        )),
    )
    .await;

    if assistant_response.stop_reason == StopReason::Error {
        println!(
            "  Initial request error: {:?}",
            assistant_response.error_message
        );
        dump_failure_payload(
            &format!("{}-initial", pair.label),
            assistant_response
                .error_message
                .as_deref()
                .unwrap_or("Unknown error"),
            captured.lock().unwrap().as_ref(),
            std::slice::from_ref(&user_message_value),
        );
        return None;
    }

    let tool_call = assistant_response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        });
    let Some(tool_call) = tool_call else {
        println!(
            "  No tool call in response (stopReason: {:?})",
            assistant_response.stop_reason
        );
        return Some((
            vec![
                user_message_value,
                Message::Assistant(Box::new(assistant_response)),
            ],
            model.api,
        ));
    };

    println!("  Tool call ID: {}", tool_call.id);

    let tool_result = ToolResultMessage {
        role: RoleToolResult,
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content: vec![pi_core::ai::types::BlockContent::Text(TextContent {
            text: "42".to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: now_millis(),
        ..Default::default()
    };

    let messages_for_final = vec![
        user_message_value.clone(),
        Message::Assistant(Box::new(assistant_response)),
        Message::ToolResult(Box::new(tool_result)),
    ];
    let (captured, on_payload) = live::payload_capture();
    let final_response = complete_simple(
        &model,
        &Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: messages_for_final.clone(),
            tools: Some(vec![test_tool()]),
        },
        Some(simple_options(
            api_key,
            headers,
            supports_reasoning,
            Some(on_payload),
        )),
    )
    .await;

    if final_response.stop_reason == StopReason::Error {
        println!("  Final request error: {:?}", final_response.error_message);
        dump_failure_payload(
            &format!("{}-final", pair.label),
            final_response
                .error_message
                .as_deref()
                .unwrap_or("Unknown error"),
            captured.lock().unwrap().as_ref(),
            &messages_for_final,
        );
        return None;
    }

    let mut messages = messages_for_final;
    messages.push(Message::Assistant(Box::new(final_response)));
    Some((messages, model.api))
}

/// The TS beforeAll: generate fixtures for every authenticated pair.
async fn generate_all_contexts() -> (
    BTreeMap<&'static str, CachedContext>,
    Vec<&'static ProviderModelPair>,
) {
    let mut contexts: BTreeMap<&'static str, CachedContext> = BTreeMap::new();
    let mut available_pairs: Vec<&'static ProviderModelPair> = Vec::new();

    println!("\n=== Generating Fixtures ===\n");

    for pair in PROVIDER_MODEL_PAIRS {
        let api_key = get_api_key(pair.provider).await;
        if api_key.is_none() || !has_api_key(pair) {
            println!("[{}] Skipping - no auth for {}", pair.label, pair.provider);
            continue;
        }

        println!("[{}] Generating fixture...", pair.label);
        let result = generate_context(pair, api_key.as_deref().unwrap_or_default()).await;

        let Some((messages, _api)) = result else {
            println!("[{}] Failed to generate fixture, skipping", pair.label);
            continue;
        };
        if messages.len() < 4 {
            println!("[{}] Failed to generate fixture, skipping", pair.label);
            continue;
        }

        contexts.insert(pair.label, CachedContext { messages });
        available_pairs.push(pair);
        println!("[{}] Generated fixture", pair.label);
    }

    println!(
        "\n=== {}/{} contexts available ===\n",
        available_pairs.len(),
        PROVIDER_MODEL_PAIRS.len()
    );
    (contexts, available_pairs)
}

fn has_any_api_key() -> bool {
    PROVIDER_MODEL_PAIRS.iter().any(has_api_key)
}

#[tokio::test]
async fn should_have_at_least_2_fixtures_to_test_handoffs() {
    // TS: it.skipIf(!hasAnyApiKey())
    if !has_any_api_key() {
        skip(
            "Cross-Provider Handoff: should have at least 2 fixtures to test handoffs",
            "any provider API key (env or ~/.pi/agent/auth.json is not consulted by the sync gate)",
        );
        return;
    }
    let (contexts, _) = generate_all_contexts().await;
    assert!(
        contexts.len() >= 2,
        "should have at least 2 fixtures to test handoffs (got {})",
        contexts.len()
    );
}

#[tokio::test]
async fn should_handle_cross_provider_handoffs_for_each_target() {
    // TS: it.skipIf(!hasAnyApiKey())
    if !has_any_api_key() {
        skip(
            "Cross-Provider Handoff: should handle cross-provider handoffs for each target",
            "any provider API key (env or ~/.pi/agent/auth.json is not consulted by the sync gate)",
        );
        return;
    }
    let (contexts, available_pairs) = generate_all_contexts().await;

    if contexts.len() < 2 {
        println!("Not enough fixtures for handoff test, skipping");
        return;
    }

    println!("\n=== Testing Cross-Provider Handoffs ===\n");

    struct HandoffResult {
        target: &'static str,
        error: Option<String>,
    }
    let mut results: Vec<HandoffResult> = Vec::new();

    for target_pair in available_pairs {
        let api_key = get_api_key(target_pair.provider).await;
        if api_key.is_none() || !has_api_key(target_pair) {
            println!("[Target: {}] Skipping - no auth", target_pair.label);
            continue;
        }

        // Collect messages from ALL OTHER contexts
        let mut other_messages: Vec<Message> = Vec::new();
        for (label, ctx) in &contexts {
            if *label == target_pair.label {
                continue;
            }
            other_messages.extend(ctx.messages.iter().cloned());
        }

        if other_messages.is_empty() {
            println!(
                "[Target: {}] Skipping - no other contexts",
                target_pair.label
            );
            continue;
        }

        let mut all_messages = other_messages.clone();
        all_messages.push(user_message(
            "Great, thanks for all that help! Now just say 'Hello, handoff successful!' to confirm you received everything.",
        ));

        let Some(base_model) = get_builtin_model(target_pair.provider, target_pair.model) else {
            println!("[Target: {}] Model not found", target_pair.label);
            continue;
        };
        let model = match target_pair.api_override {
            Some(api) => Model {
                api: api.to_string(),
                ..base_model
            },
            None => base_model,
        };
        let supports_reasoning = model.reasoning;
        let headers = get_headers(target_pair);

        println!(
            "[Target: {}] Testing with {} messages from other providers...",
            target_pair.label,
            other_messages.len()
        );

        let (captured, on_payload) = live::payload_capture();
        let response = complete_simple(
            &model,
            &Context {
                system_prompt: Some("You are a helpful assistant.".to_string()),
                messages: all_messages.clone(),
                tools: Some(vec![test_tool()]),
            },
            Some(simple_options(
                api_key.as_deref().unwrap_or_default(),
                headers,
                supports_reasoning,
                Some(on_payload),
            )),
        )
        .await;

        if response.stop_reason == StopReason::Error {
            println!(
                "[Target: {}] FAILED: {:?}",
                target_pair.label, response.error_message
            );
            dump_failure_payload(
                target_pair.label,
                response.error_message.as_deref().unwrap_or("Unknown error"),
                captured.lock().unwrap().as_ref(),
                &all_messages,
            );
            results.push(HandoffResult {
                target: target_pair.label,
                error: response.error_message,
            });
        } else {
            let text = response
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            let preview: String = text
                .chars()
                .take(100)
                .collect::<String>()
                .replace('\n', " ");
            println!("[Target: {}] SUCCESS: {preview}...", target_pair.label);
            results.push(HandoffResult {
                target: target_pair.label,
                error: None,
            });
        }
    }

    println!("\n=== Results Summary ===\n");
    let successes = results
        .iter()
        .filter(|result| result.error.is_none())
        .count();
    let failures: Vec<&HandoffResult> = results
        .iter()
        .filter(|result| result.error.is_some())
        .collect();
    println!("Passed: {successes}/{}", results.len());
    if !failures.is_empty() {
        println!("\nFailures:");
        for failure in &failures {
            println!("  - {}: {:?}", failure.target, failure.error);
        }
    }

    assert!(
        failures.is_empty(),
        "cross-provider handoff failures: {}",
        failures
            .iter()
            .map(|failure| format!("{}: {:?}", failure.target, failure.error))
            .collect::<Vec<_>>()
            .join("; ")
    );
}
