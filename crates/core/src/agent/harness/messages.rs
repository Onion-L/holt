//! Port of `pi-core/agent/src/harness/messages.ts`.
//!
//! The TypeScript custom message roles (registered through declaration
//! merging on `CustomAgentMessages`) ride as `AgentMessage::Custom`
//! payloads whose JSON keeps the original field names, so session JSONL
//! and provider conversion see identical shapes.

use crate::ai::types::{BlockContent, Message, TextContent, UserContent, UserMessage};

use super::super::types::AgentMessage;

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Port of `BashExecutionMessage`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub role: String,
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full_output_path: Option<String>,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exclude_from_context: Option<bool>,
}

/// Builds a harness custom message payload (`{ role, ...fields }`) as raw
/// JSON plus its role discriminator.
pub fn custom_message(role: &str, fields: serde_json::Value) -> AgentMessage {
    AgentMessage::Custom(crate::agent::types::CustomAgentMessage {
        role: role.to_string(),
        value: fields,
    })
}

fn user_message(content: Vec<BlockContent>, timestamp: i64) -> Message {
    Message::User(UserMessage {
        role: Default::default(),
        content: UserContent::Blocks(content),
        timestamp,
    })
}

fn text_block(text: String) -> BlockContent {
    BlockContent::Text(TextContent {
        text,
        ..Default::default()
    })
}

fn message_timestamp(value: &serde_json::Value) -> i64 {
    value
        .get("timestamp")
        .and_then(|timestamp| timestamp.as_i64())
        .unwrap_or_default()
}

/// Port of `bashExecutionToText`.
pub fn bash_execution_to_text(message: &serde_json::Value) -> String {
    let command = message
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let output = message
        .get("output")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let mut text = format!("Ran `{command}`\n");
    if !output.is_empty() {
        text.push_str(&format!("```\n{output}\n```"));
    } else {
        text.push_str("(no output)");
    }
    let cancelled = message
        .get("cancelled")
        .and_then(|value| value.as_bool())
        .unwrap_or_default();
    let exit_code = message.get("exitCode").and_then(|value| value.as_i64());
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = exit_code
        && exit_code != 0
    {
        text.push_str(&format!("\n\nCommand exited with code {exit_code}"));
    }
    let truncated = message
        .get("truncated")
        .and_then(|value| value.as_bool())
        .unwrap_or_default();
    let full_output_path = message
        .get("fullOutputPath")
        .and_then(|value| value.as_str());
    if truncated && let Some(full_output_path) = full_output_path {
        text.push_str(&format!(
            "\n\n[Output truncated. Full output: {full_output_path}]"
        ));
    }
    text
}

/// Port of `createBranchSummaryMessage`.
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: i64,
) -> AgentMessage {
    custom_message(
        "branchSummary",
        serde_json::json!({
            "role": "branchSummary",
            "summary": summary.into(),
            "fromId": from_id.into(),
            "timestamp": timestamp,
        }),
    )
}

/// Port of `createCompactionSummaryMessage`.
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: u64,
    timestamp: i64,
) -> AgentMessage {
    custom_message(
        "compactionSummary",
        serde_json::json!({
            "role": "compactionSummary",
            "summary": summary.into(),
            "tokensBefore": tokens_before,
            "timestamp": timestamp,
        }),
    )
}

/// Port of `createCustomMessage`.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: CustomMessageContent,
    display: bool,
    details: serde_json::Value,
    timestamp: i64,
) -> AgentMessage {
    let content = match content {
        CustomMessageContent::Text(text) => serde_json::Value::String(text),
        CustomMessageContent::Blocks(blocks) => serde_json::to_value(blocks).unwrap_or_default(),
    };
    custom_message(
        "custom",
        serde_json::json!({
            "role": "custom",
            "customType": custom_type.into(),
            "content": content,
            "display": display,
            "details": details,
            "timestamp": timestamp,
        }),
    )
}

/// Content accepted by [`create_custom_message`] (string or blocks).
pub enum CustomMessageContent {
    Text(String),
    Blocks(Vec<BlockContent>),
}

/// Port of `convertToLlm`: converts harness custom messages to user
/// messages and passes standard messages through.
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(Message::User(user)),
            AgentMessage::Assistant(assistant) => Some(Message::Assistant(assistant)),
            AgentMessage::ToolResult(tool_result) => Some(Message::ToolResult(tool_result)),
            AgentMessage::Custom(custom) => match custom.role.as_str() {
                "bashExecution" => {
                    let exclude = custom
                        .value
                        .get("excludeFromContext")
                        .and_then(|value| value.as_bool())
                        .unwrap_or_default();
                    if exclude {
                        None
                    } else {
                        Some(user_message(
                            vec![text_block(bash_execution_to_text(&custom.value))],
                            message_timestamp(&custom.value),
                        ))
                    }
                }
                "custom" => {
                    let content = custom.value.get("content").cloned().unwrap_or_default();
                    let blocks = match &content {
                        serde_json::Value::String(text) => {
                            vec![text_block(text.clone())]
                        }
                        serde_json::Value::Array(_) => {
                            serde_json::from_value::<Vec<BlockContent>>(content.clone())
                                .unwrap_or_default()
                        }
                        _ => Vec::new(),
                    };
                    Some(user_message(blocks, message_timestamp(&custom.value)))
                }
                "branchSummary" => {
                    let summary = custom
                        .value
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    Some(user_message(
                        vec![text_block(format!(
                            "{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
                        ))],
                        message_timestamp(&custom.value),
                    ))
                }
                "compactionSummary" => {
                    let summary = custom
                        .value
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    Some(user_message(
                        vec![text_block(format!(
                            "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
                        ))],
                        message_timestamp(&custom.value),
                    ))
                }
                _ => None,
            },
        })
        .collect()
}
