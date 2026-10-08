//! Port of `pi-core/agent/src/harness/compaction/utils.ts`.

use std::collections::BTreeSet;

use crate::agent::types::AgentMessage;
use crate::ai::types::{AssistantContent, Message, UserContent};
use crate::ai::utils::text::content_text;

/// File paths touched by a session branch or compaction range.
#[derive(Clone, Debug, Default)]
pub struct FileOperations {
    /// Files read but not necessarily modified.
    pub read: BTreeSet<String>,
    /// Files written by full-file write operations.
    pub written: BTreeSet<String>,
    /// Files modified by edit operations.
    pub edited: BTreeSet<String>,
}

/// Create an empty file-operation accumulator.
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

/// Add file operations from assistant tool calls to an accumulator.
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Assistant(assistant) = message else {
        return;
    };
    for block in &assistant.content {
        let AssistantContent::ToolCall(tool_call) = block else {
            continue;
        };
        let Some(path) = tool_call.arguments.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let path = path.to_string();
        match tool_call.name.as_str() {
            "read" => {
                file_ops.read.insert(path);
            }
            "write" => {
                file_ops.written.insert(path);
            }
            "edit" => {
                file_ops.edited.insert(path);
            }
            _ => {}
        }
    }
}

/// Compute sorted read-only and modified file lists.
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops.edited.union(&file_ops.written).cloned().collect();
    let read_only: Vec<String> = file_ops
        .read
        .iter()
        .filter(|path| !modified.contains(*path))
        .cloned()
        .collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_only, modified_files)
}

/// Format file lists as summary metadata tags.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn safe_json_stringify(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "undefined".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "[unserializable]".to_string()),
    }
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut end = max_chars;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let truncated_chars = text.len() - end;
    format!(
        "{}\n\n[... {truncated_chars} more characters truncated]",
        &text[..end]
    )
}

fn user_content_chars(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_text(blocks, ""),
    }
}

/// Serialize LLM messages to plain text for summarization prompts.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for message in messages {
        match message {
            Message::User(user) => {
                let content = user_content_chars(&user.content);
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();

                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            thinking_parts.push(thinking.thinking.clone());
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let args = tool_call
                                .arguments
                                .iter()
                                .map(|(key, value)| format!("{key}={}", safe_json_stringify(value)))
                                .collect::<Vec<_>>()
                                .join(", ");
                            tool_calls.push(format!("{}({args})", tool_call.name));
                        }
                        AssistantContent::Text(_) => {}
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::Text(_)))
                {
                    parts.push(format!(
                        "[Assistant]: {}",
                        content_text(&assistant.content, "\n")
                    ));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(tool_result) => {
                let content = content_text(&tool_result.content, "");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_for_summary_backs_off_to_char_boundary() {
        // "参" is 3 bytes; byte 2000 falls inside the 667th one.
        let text = "参".repeat(700);
        let out = truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS);
        assert!(out.starts_with(&"参".repeat(666)));
        assert!(out.contains("[... 102 more characters truncated]"));
    }
}
