//! Credential-gated entries from `bedrock-models.test.ts` and
//! `bedrock-thinking-payload.test.ts`.

mod common;

use common::live::{
    LiveOptions, get_builtin_models, get_model_or_panic, live_complete, live_env, now_millis, skip,
};
use pi_core::ai::types::{
    AssistantContent, Context, Message, RoleAssistant, RoleUser, StopReason, ThinkingLevel,
    UserContent, UserMessage,
};

fn context(prompt: &str) -> Context {
    Context {
        system_prompt: Some(
            "You are a deterministic text generator. Follow the requested output format exactly."
                .to_string(),
        ),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(prompt.to_string()),
            timestamp: now_millis(),
        })],
        tools: None,
    }
}

#[tokio::test]
async fn bedrock_extensive_model_matrix() {
    let models = get_builtin_models("amazon-bedrock");
    assert!(!models.is_empty());
    assert!(
        models
            .iter()
            .any(|model| model.id == "global.anthropic.claude-opus-5")
    );
    assert!(
        !models
            .iter()
            .any(|model| model.id == "anthropic.claude-opus-5")
    );

    if !common::live::has_bedrock_credentials()
        || live_env("BEDROCK_EXTENSIVE_MODEL_TEST").is_none()
    {
        skip(
            "Amazon Bedrock extensive model matrix",
            "AWS credentials + BEDROCK_EXTENSIVE_MODEL_TEST",
        );
        return;
    }
    for model in models {
        let response = live_complete(
            &model,
            &Context {
                system_prompt: Some(
                    "You are a helpful assistant. Be extremely concise.".to_string(),
                ),
                messages: context("Reply with exactly: 'OK'").messages,
                tools: None,
            },
            &LiveOptions::default(),
        )
        .await;
        assert_eq!(response.role, RoleAssistant, "{}", model.id);
        assert!(!response.content.is_empty(), "{}", model.id);
        assert!(
            response.usage.input + response.usage.cache_read > 0,
            "{}",
            model.id
        );
        assert!(response.usage.output > 0, "{}", model.id);
        assert!(
            response.error_message.is_none(),
            "{}: {:?}",
            model.id,
            response.error_message
        );
        assert!(response.content.iter().any(
            |block| matches!(block, AssistantContent::Text(text) if !text.text.trim().is_empty())
        ));
    }
}

#[tokio::test]
async fn bedrock_adaptive_claude_uses_model_max_tokens() {
    if !common::live::has_bedrock_credentials() {
        skip("Bedrock Claude max tokens E2E", "AWS credentials");
        return;
    }
    let mut model = get_model_or_panic("amazon-bedrock", "global.anthropic.claude-sonnet-4-6");
    model.max_tokens = 6000;
    let response = live_complete(
        &model,
        &context("Output exactly 5200 repetitions of the token alpha, separated by single spaces. Do not number them. Do not use markdown. Do not add any other text."),
        &LiveOptions {
            reasoning: Some(ThinkingLevel::Low),
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
        response.usage.output > 4096,
        "output tokens: {}",
        response.usage.output
    );
}
