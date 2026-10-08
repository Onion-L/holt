//! Credential-gated entries for `google-thinking-disable.test.ts` and
//! `xhigh.test.ts`.

mod common;

use common::live::{
    LiveEffort, LiveOptions, LiveThinking, as_openai_completions, get_model_or_panic, live_env,
    live_stream, now_millis, skip,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessageEvent, Context, Message, Model, RoleUser, StopReason,
    UserContent, UserMessage,
};

fn context(prompt: &str) -> Context {
    Context {
        system_prompt: Some(
            "You are a precise assistant. Follow the requested output format exactly.".to_string(),
        ),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(prompt.to_string()),
            timestamp: now_millis(),
        })],
        tools: None,
    }
}

fn count_pongs(text: &str) -> usize {
    text.split(|ch: char| !ch.is_ascii_alphabetic())
        .filter(|word| word.eq_ignore_ascii_case("pong"))
        .count()
}

async fn assert_thinking_disabled(
    model: &Model,
    mut options: LiveOptions,
    min_pongs: usize,
    max_output_tokens: Option<u64>,
) {
    match model.api.as_str() {
        "anthropic-messages" => options.thinking_enabled = Some(false),
        "google-generative-ai" | "google-vertex" => {
            options.thinking = Some(LiveThinking {
                enabled: false,
                budget_tokens: None,
                level: None,
            });
        }
        _ => {}
    }
    let stream = live_stream(
        model,
        &context(
            "Before replying, carefully solve 36863 * 5279 internally. Then reply with the word pong repeated exactly 40 times, separated by single spaces. Do not add any other text.",
        ),
        &options,
    );
    let mut thinking_events = 0;
    let mut thinking_chars = 0;
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::ThinkingStart { .. }
            | AssistantMessageEvent::ThinkingEnd { .. } => thinking_events += 1,
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                thinking_events += 1;
                thinking_chars += delta.chars().count();
            }
            _ => {}
        }
    }
    let response = stream.result().await;
    assert_eq!(
        response.stop_reason,
        StopReason::Stop,
        "{:?}",
        response.error_message
    );
    assert_eq!(thinking_events, 0);
    assert_eq!(thinking_chars, 0);
    assert!(
        !response
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Thinking(_)))
    );
    let text: String = response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(count_pongs(&text) >= min_pongs, "response: {text}");
    if let Some(limit) = max_output_tokens {
        assert!(response.usage.output < limit);
    }
}

#[tokio::test]
async fn provider_thinking_disable_matrix() {
    if live_env("ANTHROPIC_API_KEY").is_some() {
        for id in ["claude-sonnet-4-5", "claude-sonnet-4-6"] {
            assert_thinking_disabled(
                &get_model_or_panic("anthropic", id),
                LiveOptions {
                    max_tokens: Some(320),
                    temperature: Some(0.0),
                    ..Default::default()
                },
                35,
                None,
            )
            .await;
        }
    } else {
        skip("Anthropic thinking disable E2E", "ANTHROPIC_API_KEY");
    }

    if live_env("GEMINI_API_KEY").is_some() {
        for (id, max_tokens, min_pongs) in [
            ("gemini-2.5-flash", 160, 35),
            ("gemini-3-flash-preview", 160, 35),
            ("gemini-3.1-pro-preview", 512, 20),
        ] {
            assert_thinking_disabled(
                &get_model_or_panic("google", id),
                LiveOptions {
                    max_tokens: Some(max_tokens),
                    temperature: Some(0.0),
                    ..Default::default()
                },
                min_pongs,
                None,
            )
            .await;
        }
    } else {
        skip("Google thinking disable E2E", "GEMINI_API_KEY");
    }

    let vertex_key = live_env("GOOGLE_CLOUD_API_KEY");
    let vertex_project = live_env("GOOGLE_CLOUD_PROJECT").or_else(|| live_env("GCLOUD_PROJECT"));
    let vertex_location = live_env("GOOGLE_CLOUD_LOCATION");
    if vertex_key.is_some() || (vertex_project.is_some() && vertex_location.is_some()) {
        for id in ["gemini-2.5-flash", "gemini-3-flash-preview"] {
            assert_thinking_disabled(
                &get_model_or_panic("google-vertex", id),
                LiveOptions {
                    api_key: vertex_key.clone(),
                    project: vertex_project.clone(),
                    location: vertex_location.clone(),
                    max_tokens: Some(160),
                    temperature: Some(0.0),
                    ..Default::default()
                },
                35,
                None,
            )
            .await;
        }
    } else {
        skip(
            "Google Vertex thinking disable E2E",
            "GOOGLE_CLOUD_API_KEY or project+location",
        );
    }

    if live_env("OPENAI_API_KEY").is_some() {
        assert_thinking_disabled(
            &get_model_or_panic("openai", "gpt-5.4-mini"),
            LiveOptions {
                max_tokens: Some(160),
                ..Default::default()
            },
            35,
            None,
        )
        .await;
    } else {
        skip("OpenAI thinking disable E2E", "OPENAI_API_KEY");
    }

    if live_env("OPENROUTER_API_KEY").is_some() {
        assert_thinking_disabled(
            &get_model_or_panic("openrouter", "qwen/qwen3.5-plus-02-15"),
            LiveOptions {
                max_tokens: Some(160),
                temperature: Some(0.0),
                ..Default::default()
            },
            35,
            Some(100),
        )
        .await;
    } else {
        skip("OpenRouter thinking disable E2E", "OPENROUTER_API_KEY");
    }
}

fn math_context() -> Context {
    context("What is 37 + 68? Think step by step.")
}

async fn drain_xhigh(model: &Model) -> (bool, pi_core::ai::types::AssistantMessage) {
    let stream = live_stream(
        model,
        &math_context(),
        &LiveOptions {
            reasoning_effort: Some(LiveEffort::Xhigh),
            ..Default::default()
        },
    );
    let mut thinking = false;
    while let Some(event) = stream.next().await {
        thinking |= matches!(
            event,
            AssistantMessageEvent::ThinkingStart { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
        );
    }
    let response = stream.result().await;
    (thinking, response)
}

#[tokio::test]
async fn xhigh_reasoning_support_and_rejection() {
    if live_env("OPENAI_API_KEY").is_none() {
        skip("xhigh reasoning", "OPENAI_API_KEY");
        return;
    }
    let (saw_thinking, response) = drain_xhigh(&get_model_or_panic("openai", "gpt-5.5")).await;
    assert_eq!(
        response.stop_reason,
        StopReason::Stop,
        "{:?}",
        response.error_message
    );
    assert!(
        response
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::Text(_)))
    );
    assert!(
        saw_thinking
            || response
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::Thinking(_)))
    );

    for model in [
        get_model_or_panic("openai", "gpt-5-mini"),
        as_openai_completions(&get_model_or_panic("openai", "gpt-5-mini")),
    ] {
        let (_, response) = drain_xhigh(&model).await;
        assert_eq!(response.stop_reason, StopReason::Error);
        assert!(
            response
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("xhigh"))
        );
    }
}
