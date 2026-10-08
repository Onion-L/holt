//! Port of `pi-core/ai/src/api/github-copilot-headers.ts`.

use crate::ai::types::Message;

/// Port of `inferCopilotInitiator`: whether the request is user-initiated or
/// agent-initiated.
pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        Some(last) if !matches!(last, Message::User(_)) => "agent",
        _ => "user",
    }
}

/// Port of `hasCopilotVisionInput`: Copilot requires the
/// Copilot-Vision-Request header when sending images.
pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(message) => matches!(
            &message.content,
            crate::ai::types::UserContent::Blocks(blocks)
                if blocks.iter().any(|block| matches!(block, crate::ai::types::BlockContent::Image(_)))
        ),
        Message::ToolResult(message) => message
            .content
            .iter()
            .any(|block| matches!(block, crate::ai::types::BlockContent::Image(_))),
        Message::Assistant(_) => false,
    })
}

/// Port of `buildCopilotDynamicHeaders`.
pub fn build_copilot_dynamic_headers(
    messages: &[Message],
    has_images: bool,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "X-Initiator".to_string(),
            infer_copilot_initiator(messages).to_string(),
        ),
        (
            "Openai-Intent".to_string(),
            "conversation-edits".to_string(),
        ),
    ];
    if has_images {
        headers.push(("Copilot-Vision-Request".to_string(), "true".to_string()));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::{BlockContent, RoleUser, TextContent, UserContent, UserMessage};

    fn user_message(content: UserContent) -> Message {
        Message::User(UserMessage {
            role: RoleUser,
            content,
            timestamp: 0,
        })
    }

    #[test]
    fn infers_initiator_from_last_message() {
        let user_last = vec![user_message(UserContent::Text("hi".to_string()))];
        assert_eq!(infer_copilot_initiator(&user_last), "user");

        let agent_last = vec![
            user_message(UserContent::Text("hi".to_string())),
            Message::Assistant(Box::new(
                crate::ai::providers::faux::faux_assistant_message("hello", Default::default()),
            )),
        ];
        assert_eq!(infer_copilot_initiator(&agent_last), "agent");
    }

    #[test]
    fn detects_vision_input_and_builds_headers() {
        let messages = vec![user_message(UserContent::Blocks(vec![BlockContent::Text(
            TextContent {
                text: "see image".to_string(),
                ..Default::default()
            },
        )]))];
        assert!(!has_copilot_vision_input(&messages));
        let headers = build_copilot_dynamic_headers(&messages, false);
        assert_eq!(
            headers,
            vec![
                ("X-Initiator".to_string(), "user".to_string()),
                (
                    "Openai-Intent".to_string(),
                    "conversation-edits".to_string()
                ),
            ]
        );
    }
}
