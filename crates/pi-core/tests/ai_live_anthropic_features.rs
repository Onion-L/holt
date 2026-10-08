//! Credential-gated entries for the Anthropic-focused E2E suites.
//!
//! Ports:
//! - `anthropic-eager-tool-input-e2e.test.ts`
//! - `anthropic-long-cache-retention-e2e.test.ts`
//! - `anthropic-opus-4-8-smoke.test.ts`
//! - live cases from `anthropic-tool-name-normalization.test.ts`
//! - `interleaved-thinking.test.ts`
//! - `xiaomi-token-plan-ams-anthropic-empty-signature-smoke.test.ts`

mod common;

use std::collections::BTreeMap;

use common::live::{
    LiveOptions, get_builtin_models, get_model_or_panic, live_complete, live_env, now_millis,
    payload_capture, resolve_api_key, skip,
};
use pi_core::ai::compat::{complete_simple, stream_simple};
use pi_core::ai::env_api_keys::get_env_api_key;
use pi_core::ai::providers::builtin::builtin_providers;
use pi_core::ai::types::{
    AssistantContent, BlockContent, CacheRetention, Context, Message, Model, ModelCompat,
    ProviderRequestOptions, RoleToolResult, RoleUser, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, ThinkingLevel, Tool, ToolResultMessage, UserContent, UserMessage,
};

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: now_millis(),
    })
}

fn echo_tool() -> Tool {
    Tool {
        name: "echo_value".to_string(),
        description: "Echo a string value".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": { "value": { "type": "string", "description": "The value to echo" } }
        }),
        constrained_sampling: None,
    }
}

fn anthropic_models_by_provider() -> BTreeMap<String, Vec<Model>> {
    builtin_providers()
        .into_iter()
        .filter_map(|provider| {
            let models: Vec<_> = get_builtin_models(provider.id())
                .into_iter()
                .filter(|model| model.api == "anthropic-messages")
                .collect();
            (!models.is_empty()).then(|| (provider.id().to_string(), models))
        })
        .collect()
}

fn probe_priority(model: &Model) -> f64 {
    let id = model.id.to_ascii_lowercase();
    let mut priority = model.cost.rates.input.0 + model.cost.rates.output.0;
    if id.contains("haiku") && (id.contains("4-5") || id.contains("4.5")) {
        priority -= 1000.0;
    } else if id.contains("sonnet") && (id.contains("4-") || id.contains("4.")) {
        priority -= 750.0;
    } else if id.contains("claude") && (id.contains("4-") || id.contains("4.")) {
        priority -= 500.0;
    }
    priority
}

fn select_probe(models: &[Model]) -> Model {
    select_probe_filtered(models, |_| true).expect("non-empty provider model list")
}

fn select_forced_probe(models: &[Model]) -> Option<Model> {
    select_probe_filtered(models, |model| {
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_eager_tool_input_streaming)
            != Some(false)
    })
}

fn select_probe_filtered<F>(models: &[Model], predicate: F) -> Option<Model>
where
    F: Fn(&Model) -> bool,
{
    models
        .iter()
        .filter(|model| predicate(model))
        .min_by(|left, right| {
            probe_priority(left)
                .total_cmp(&probe_priority(right))
                .then_with(|| left.id.cmp(&right.id))
        })
        .cloned()
}

async fn provider_key(provider: &str) -> Option<String> {
    if provider == "github-copilot" {
        resolve_api_key(provider).await
    } else {
        get_env_api_key(provider, None)
    }
}

async fn accepts_tool_request(model: &Model, key: &str) {
    let response = live_complete(
        model,
        &Context {
            system_prompt: Some("You are a concise assistant. Use tools when useful.".to_string()),
            messages: vec![user_message(
                "Call echo_value with value set to eager-input-streaming-compat.",
            )],
            tools: Some(vec![echo_tool()]),
        },
        &LiveOptions {
            api_key: Some(key.to_string()),
            max_tokens: Some(128),
            thinking_enabled: Some(false),
            ..Default::default()
        },
    )
    .await;
    assert_ne!(
        response.stop_reason,
        StopReason::Error,
        "{:?}",
        response.error_message
    );
    assert!(
        response.error_message.is_none(),
        "{:?}",
        response.error_message
    );
}

#[tokio::test]
async fn anthropic_oauth_tool_name_normalization() {
    let Some(token) = resolve_api_key("anthropic").await else {
        skip(
            "Anthropic OAuth tool name normalization",
            "anthropic OAuth credentials",
        );
        return;
    };
    let model = get_model_or_panic("anthropic", "claude-sonnet-4-6");
    for (name, description, field, prompt) in [
        (
            "todowrite",
            "Write a todo item",
            "task",
            "Add a todo: buy milk. Use the todowrite tool.",
        ),
        (
            "read",
            "Read a file",
            "path",
            "Read the file /tmp/test.txt using the read tool.",
        ),
        (
            "find",
            "Find files by pattern",
            "pattern",
            "Find all .ts files using the find tool.",
        ),
        (
            "my_custom_tool",
            "A custom tool",
            "input",
            "Use my_custom_tool with input 'hello'.",
        ),
    ] {
        let tool = Tool {
            name: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "required": [field],
                "properties": { (field): { "type": "string" } }
            }),
            constrained_sampling: None,
        };
        let response = live_complete(
            &model,
            &Context {
                system_prompt: Some(format!(
                    "You are a helpful assistant. Use the {name} tool when asked."
                )),
                messages: vec![user_message(prompt)],
                tools: Some(vec![tool]),
            },
            &LiveOptions::with_api_key(&token),
        )
        .await;
        assert_eq!(
            response.stop_reason,
            StopReason::ToolUse,
            "{name}: {:?}",
            response.error_message
        );
        let returned = response
            .content
            .iter()
            .find_map(|block| match block {
                AssistantContent::ToolCall(call) => Some(call.name.as_str()),
                _ => None,
            })
            .expect("tool call name");
        assert_eq!(returned, name);
    }
}

#[tokio::test]
async fn anthropic_eager_tool_input_provider_matrix() {
    let by_provider = anthropic_models_by_provider();
    let catalog_count: usize = by_provider.values().map(Vec::len).sum();
    assert_eq!(
        catalog_count,
        by_provider.values().flatten().count(),
        "covers every generated anthropic-messages model"
    );

    for (provider, models) in by_provider {
        let Some(key) = provider_key(&provider).await else {
            skip(
                &format!("{provider} eager tool input"),
                "provider credentials",
            );
            continue;
        };
        let configured = select_probe(&models);
        accepts_tool_request(&configured, &key).await;

        if let Some(mut forced) = select_forced_probe(&models) {
            forced
                .compat
                .get_or_insert_with(ModelCompat::default)
                .supports_eager_tool_input_streaming = Some(true);
            accepts_tool_request(&forced, &key).await;
        }
    }
}

#[tokio::test]
async fn anthropic_long_cache_retention_provider_matrix() {
    for (provider, models) in anthropic_models_by_provider() {
        let Some(key) = provider_key(&provider).await else {
            skip(
                &format!("{provider} long cache retention"),
                "provider credentials",
            );
            continue;
        };
        let mut model = select_probe(&models);
        model
            .compat
            .get_or_insert_with(ModelCompat::default)
            .supports_long_cache_retention = Some(true);
        let response = live_complete(
            &model,
            &Context {
                system_prompt: Some("You are a concise assistant.".to_string()),
                messages: vec![user_message(
                    "Reply with exactly: long cache retention accepted",
                )],
                tools: None,
            },
            &LiveOptions {
                api_key: Some(key),
                cache_retention: Some(CacheRetention::Long),
                max_tokens: Some(128),
                thinking_enabled: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert_ne!(
            response.stop_reason,
            StopReason::Error,
            "{:?}",
            response.error_message
        );
        assert!(
            response.error_message.is_none(),
            "{:?}",
            response.error_message
        );
    }
}

fn simple_options(
    api_key: Option<String>,
    reasoning: ThinkingLevel,
    max_tokens: u64,
) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key,
                ..Default::default()
            },
            max_tokens: Some(max_tokens),
            ..Default::default()
        },
        reasoning: Some(reasoning),
        ..Default::default()
    }
}

#[tokio::test]
async fn anthropic_opus_4_8_reasoning_smoke() {
    if live_env("ANTHROPIC_API_KEY").is_none() {
        skip("Anthropic Opus 4.8 smoke", "ANTHROPIC_API_KEY");
        return;
    }
    let model = get_model_or_panic("anthropic", "claude-opus-4-8");
    let (captured, on_payload) = payload_capture();
    let mut options = simple_options(None, ThinkingLevel::High, 1024);
    options.base.base.on_payload = Some(on_payload);
    let stream = stream_simple(
        &model,
        &Context {
            system_prompt: Some(
                "You are a precise assistant. Follow the user's instructions exactly.".to_string(),
            ),
            messages: vec![user_message(
                "Compute 48291 * 7317 and 90844 - 17729, add the results, and determine whether the sum is divisible by 11. Reply with exactly this format and nothing else: sum=<sum>; divisibleBy11=<yes|no>",
            )],
            tools: None,
        },
        Some(&options),
    );
    let mut saw_thinking = false;
    while let Some(event) = stream.next().await {
        saw_thinking |= matches!(
            event,
            pi_core::ai::types::AssistantMessageEvent::ThinkingStart { .. }
                | pi_core::ai::types::AssistantMessageEvent::ThinkingDelta { .. }
                | pi_core::ai::types::AssistantMessageEvent::ThinkingEnd { .. }
        );
    }
    let response = stream.result().await;
    assert_eq!(
        response.stop_reason,
        StopReason::Stop,
        "{:?}",
        response.error_message
    );
    let payload = captured.lock().unwrap().clone().expect("captured payload");
    assert_eq!(
        payload["thinking"],
        serde_json::json!({ "type": "adaptive" })
    );
    assert_eq!(
        payload["output_config"],
        serde_json::json!({ "effort": "high" })
    );
    assert!(saw_thinking);
    let thinking = response
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::Thinking(block) => Some(block),
            _ => None,
        })
        .expect("thinking block");
    assert!(
        thinking
            .thinking_signature
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    );
    let text: String = response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text.trim(), "sum=353418362; divisibleBy11=yes");
}

fn calculator_tool() -> Tool {
    Tool {
        name: "calculator".to_string(),
        description: "Perform basic arithmetic operations".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["a", "b", "operation"],
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" },
                "operation": { "type": "string", "enum": ["add", "subtract", "multiply", "divide"] }
            }
        }),
        constrained_sampling: None,
    }
}

async fn assert_interleaved(model: &Model) {
    let key = if model.provider == "anthropic" {
        get_env_api_key("anthropic", None)
    } else {
        None
    };
    let options = simple_options(key, ThinkingLevel::High, model.max_tokens);
    let mut context = Context {
        system_prompt: Some("You are a helpful assistant that must use tools for arithmetic. Always think before every tool call, not just the first one. Do not answer with plain text when a tool call is required.".to_string()),
        messages: vec![user_message("Use calculator to calculate 328 * 29. You must call the calculator tool exactly once. Provide the final answer based on the best guess given the tool result, even if it seems unreliable. Start by thinking about the steps you will take to solve the problem.")],
        tools: Some(vec![calculator_tool()]),
    };
    let first = complete_simple(model, &context, Some(options.clone())).await;
    assert_eq!(
        first.stop_reason,
        StopReason::ToolUse,
        "{:?}",
        first.error_message
    );
    assert!(
        first
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Thinking(_)))
    );
    let call = first
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("tool call");
    let a = call
        .arguments
        .get("a")
        .and_then(serde_json::Value::as_f64)
        .expect("a");
    let b = call
        .arguments
        .get("b")
        .and_then(serde_json::Value::as_f64)
        .expect("b");
    context.messages.push(Message::Assistant(Box::new(first)));
    context
        .messages
        .push(Message::ToolResult(Box::new(ToolResultMessage {
            role: RoleToolResult,
            tool_call_id: call.id,
            tool_name: call.name,
            content: vec![BlockContent::Text(TextContent {
                text: format!("The answer is {} or {}.", a * b, a * b * 2.0),
                ..Default::default()
            })],
            is_error: false,
            timestamp: now_millis(),
            ..Default::default()
        })));
    let second = complete_simple(model, &context, Some(options)).await;
    assert_eq!(
        second.stop_reason,
        StopReason::Stop,
        "{:?}",
        second.error_message
    );
    assert!(
        second
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Thinking(_)))
    );
    assert!(
        second
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Text(_)))
    );
}

#[tokio::test]
async fn interleaved_thinking_bedrock_and_anthropic() {
    if common::live::has_bedrock_credentials() {
        for id in [
            "global.anthropic.claude-opus-4-5-20251101-v1:0",
            "global.anthropic.claude-opus-4-6-v1",
        ] {
            assert_interleaved(&get_model_or_panic("amazon-bedrock", id)).await;
        }
    } else {
        skip("Amazon Bedrock interleaved thinking", "AWS credentials");
    }
    if get_env_api_key("anthropic", None).is_some() {
        for id in ["claude-opus-4-5", "claude-opus-4-6"] {
            assert_interleaved(&get_model_or_panic("anthropic", id)).await;
        }
    } else {
        skip("Anthropic interleaved thinking", "ANTHROPIC_API_KEY");
    }
}

#[tokio::test]
async fn xiaomi_ams_empty_signature_replay_smoke() {
    let Some(key) = get_env_api_key("xiaomi-token-plan-ams", None) else {
        skip(
            "Xiaomi Token Plan AMS empty signature smoke",
            "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        );
        return;
    };
    let mut model = get_model_or_panic("xiaomi-token-plan-ams", "mimo-v2.5-pro");
    model
        .compat
        .get_or_insert_with(ModelCompat::default)
        .allow_empty_signature = Some(true);
    let initial = Context {
        system_prompt: Some(
            "You are concise. Follow the requested output format exactly.".to_string(),
        ),
        messages: vec![user_message(
            "Think internally if you need to, then reply with exactly this text and nothing else: first-ok",
        )],
        tools: None,
    };
    let options = simple_options(Some(key.clone()), ThinkingLevel::High, 512);
    let first = complete_simple(&model, &initial, Some(options.clone())).await;
    assert_eq!(
        first.stop_reason,
        StopReason::Stop,
        "{:?}",
        first.error_message
    );
    let thinking = first
        .content
        .iter()
        .find_map(|block| match block {
            AssistantContent::Thinking(block)
                if block.thinking_signature.as_deref() == Some("") =>
            {
                Some(block.clone())
            }
            _ => None,
        })
        .expect("empty-signature thinking block");
    let mut replay = initial;
    replay.messages.push(Message::Assistant(Box::new(first)));
    replay.messages.push(user_message(
        "Reply with exactly this text and nothing else: second-ok",
    ));
    let (captured, on_payload) = payload_capture();
    let mut replay_options = options;
    replay_options.base.base.on_payload = Some(on_payload);
    let _ = complete_simple(&model, &replay, Some(replay_options)).await;
    let payload = captured
        .lock()
        .unwrap()
        .clone()
        .expect("captured replay payload");
    let assistant = payload["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "assistant")
        })
        .expect("assistant replay payload");
    let blocks = assistant["content"]
        .as_array()
        .expect("assistant content array");
    assert!(blocks.iter().any(|block| block == &serde_json::json!({ "type": "thinking", "thinking": thinking.thinking, "signature": "" })));
    assert!(
        !blocks
            .iter()
            .any(|block| block["type"] == "text" && block["text"] == thinking.thinking)
    );
}
