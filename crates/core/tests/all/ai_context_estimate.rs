//! Port of `pi-core/ai/test/context-estimate.test.ts`: context token
//! estimation with stale assistant usage after out-of-order inserts.

use pi_core::ai::api::simple_options::build_base_options;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, Context, Message, Model, ModelCost, ModelInput,
    RoleAssistant, RoleUser, StopReason, TextContent, Usage, UsageCost, UserContent, UserMessage,
};
use pi_core::ai::utils::estimate::{ContextUsageEstimate, estimate_context_tokens};

/// Port of `createUsage`.
fn create_usage(total_tokens: u64) -> Usage {
    Usage {
        input: total_tokens,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        total_tokens,
        cost: UsageCost::default(),
        ..Default::default()
    }
}

/// Port of `createAssistant`.
fn create_assistant(timestamp: i64, total_tokens: u64) -> Message {
    Message::Assistant(Box::new(AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: "kept".to_string(),
            ..Default::default()
        })],
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "test-model".to_string(),
        usage: create_usage(total_tokens),
        stop_reason: StopReason::Stop,
        timestamp,
        ..Default::default()
    }))
}

fn user_message(content: &str, timestamp: i64) -> Message {
    Message::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(content.to_string()),
        timestamp,
    })
}

fn model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 10_000,
        max_tokens: 8_000,
        ..Default::default()
    }
}

#[test]
fn ignores_stale_assistant_usage_after_a_newer_message_is_inserted_before_it() {
    let context = Context {
        system_prompt: Some("system".to_string()),
        messages: vec![
            user_message("summary", 200),
            create_assistant(100, 9_500),
            user_message(&"x".repeat(4_000), 300),
        ],
        ..Default::default()
    };

    assert_eq!(
        estimate_context_tokens(&context),
        ContextUsageEstimate {
            tokens: 1_005,
            usage_tokens: 0,
            trailing_tokens: 1_005,
            last_usage_index: None,
        }
    );
    assert_eq!(
        build_base_options(&model(), &context, None, None).max_tokens,
        Some(4_899)
    );
}

#[test]
fn uses_assistant_usage_again_after_a_response_to_the_inserted_context() {
    let context = Context {
        messages: vec![
            user_message("summary", 200),
            create_assistant(100, 9_500),
            user_message("new prompt", 300),
            create_assistant(400, 2_000),
            user_message("tail", 500),
        ],
        ..Default::default()
    };

    assert_eq!(
        estimate_context_tokens(&context),
        ContextUsageEstimate {
            tokens: 2_001,
            usage_tokens: 2_000,
            trailing_tokens: 1,
            last_usage_index: Some(3),
        }
    );
}
