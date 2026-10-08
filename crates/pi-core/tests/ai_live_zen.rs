//! Credential-gated entry for `zen.test.ts`.

mod common;

use common::live::{LiveOptions, get_builtin_models, live_complete, live_env, now_millis, skip};
use pi_core::ai::types::{Context, Message, RoleUser, StopReason, UserContent, UserMessage};

#[tokio::test]
async fn opencode_model_smoke_matrix() {
    if live_env("OPENCODE_API_KEY").is_none() {
        skip("OpenCode Models Smoke Test", "OPENCODE_API_KEY");
        return;
    }
    for provider in ["opencode", "opencode-go"] {
        for model in get_builtin_models(provider) {
            let response = live_complete(
                &model,
                &Context {
                    system_prompt: None,
                    messages: vec![Message::User(UserMessage {
                        role: RoleUser,
                        content: UserContent::Text("Say hello.".to_string()),
                        timestamp: now_millis(),
                    })],
                    tools: None,
                },
                &LiveOptions::default(),
            )
            .await;
            assert!(!response.content.is_empty(), "{provider}/{}", model.id);
            assert_eq!(
                response.stop_reason,
                StopReason::Stop,
                "{provider}/{}: {:?}",
                model.id,
                response.error_message
            );
        }
    }
}
