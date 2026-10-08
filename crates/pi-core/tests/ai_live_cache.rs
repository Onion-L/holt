//! Credential-gated entries for the Codex/OpenAI cache-affinity E2E suites
//! and `openrouter-cache-write-repro.test.ts`.

mod common;

use std::sync::Arc;

use common::live::{
    LiveOptions, get_model_or_panic, live_complete, live_env, now_millis, resolve_api_key, skip,
};
use pi_core::ai::compat::complete_simple;
use pi_core::ai::types::{
    AssistantContent, Context, Message, OnPayloadCallback, ProviderRequestOptions, RoleUser,
    SimpleStreamOptions, StopReason, StreamOptions, Transport, UserContent, UserMessage,
};

const SESSION_ID: &str = "0195d6e4-4cf9-7f44-a2d8-f8f7f49ee9d3";

fn context(system: &str, prompt: &str) -> Context {
    Context {
        system_prompt: Some(system.to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(prompt.to_string()),
            timestamp: now_millis(),
        })],
        tools: None,
    }
}

fn text(response: &pi_core::ai::types::AssistantMessage) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn openai_codex_cache_affinity_e2e() {
    let Some(token) = resolve_api_key("openai-codex").await else {
        skip(
            "openai-codex cache affinity e2e",
            "~/.pi/agent/auth.json openai-codex credentials",
        );
        return;
    };
    let response = live_complete(
        &get_model_or_panic("openai-codex", "gpt-5.5"),
        &context(
            "You are a helpful assistant. Reply exactly as requested.",
            "Reply with exactly: cache affinity e2e success",
        ),
        &LiveOptions {
            api_key: Some(token),
            session_id: Some(SESSION_ID.to_string()),
            transport: Some(Transport::Sse),
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
    assert!(response.error_message.is_none());
    assert!(text(&response).contains("cache affinity e2e success"));
}

#[tokio::test]
async fn openai_responses_cache_affinity_e2e() {
    let Some(key) = live_env("OPENAI_API_KEY") else {
        skip("openai responses cache affinity e2e", "OPENAI_API_KEY");
        return;
    };
    let response = live_complete(
        &get_model_or_panic("openai", "gpt-5.4"),
        &context(
            "You are a helpful assistant. Reply exactly as requested.",
            "Reply with exactly: openai cache affinity e2e success",
        ),
        &LiveOptions {
            api_key: Some(key),
            session_id: Some(SESSION_ID.to_string()),
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
    assert!(response.error_message.is_none());
    assert!(text(&response).contains("openai cache affinity e2e success"));
}

fn cache_marker_callback() -> OnPayloadCallback {
    Arc::new(|mut payload, _model| {
        let messages = payload
            .get_mut("messages")
            .and_then(serde_json::Value::as_array_mut);
        if let Some(messages) = messages {
            for message in messages.iter_mut().rev() {
                if message.get("role") != Some(&serde_json::Value::String("user".to_string())) {
                    continue;
                }
                let Some(content) = message.get_mut("content") else {
                    break;
                };
                if let Some(text) = content.as_str().map(str::to_string) {
                    *content = serde_json::json!([{
                        "type": "text",
                        "text": text,
                        "cache_control": { "type": "ephemeral" }
                    }]);
                    break;
                }
                if let Some(parts) = content.as_array_mut()
                    && let Some(part) = parts.iter_mut().rev().find(|part| part["type"] == "text")
                    && let Some(object) = part.as_object_mut()
                {
                    object.insert(
                        "cache_control".to_string(),
                        serde_json::json!({ "type": "ephemeral" }),
                    );
                }
                break;
            }
        }
        Box::pin(async move { Some(payload) })
    })
}

#[tokio::test]
async fn openrouter_preserves_cache_write_usage() {
    let Some(key) = live_env("OPENROUTER_API_KEY") else {
        skip("OpenRouter cache_write repro E2E", "OPENROUTER_API_KEY");
        return;
    };
    let nonce = now_millis();
    let repeated = "Prompt-caching probe content. Keep this exact text stable across requests so the provider can reuse prefix tokens and report cache read and cache write usage.";
    let system = format!(
        "You are a concise assistant.\nCache nonce: {nonce}\n\n{}",
        std::iter::repeat_n(repeated, 80)
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let request_context = context(&system, "Reply with exactly: OK");
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some(key),
                on_payload: Some(cache_marker_callback()),
                ..Default::default()
            },
            max_tokens: Some(32),
            temperature: Some(0.0),
            ..Default::default()
        },
        ..Default::default()
    };
    let model = get_model_or_panic("openrouter", "google/gemini-2.5-flash");
    let first = complete_simple(&model, &request_context, Some(options.clone())).await;
    assert_eq!(
        first.stop_reason,
        StopReason::Stop,
        "{:?}",
        first.error_message
    );
    let second = complete_simple(&model, &request_context, Some(options)).await;
    assert_eq!(
        second.stop_reason,
        StopReason::Stop,
        "{:?}",
        second.error_message
    );
    assert!(first.usage.cache_write > 0 || second.usage.cache_write > 0);
}
