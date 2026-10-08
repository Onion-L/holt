//! Port of `pi-core/ai/src/utils/estimate.ts`: context token estimation.

use crate::ai::types::{BlockContent, Context, Message, Tool, Usage};

/// Port of `ContextUsageEstimate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent applicable assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the most recent applicable assistant usage block.
    pub trailing_tokens: u64,
    /// Index of the applicable message that provided usage, or `None`.
    pub last_usage_index: Option<usize>,
}

const CHARS_PER_TOKEN: f64 = 4.0;
const ESTIMATED_IMAGE_CHARS: f64 = 4800.0;

pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

fn safe_json_stringify(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

fn chars_per_token_ceil(chars: f64) -> u64 {
    (chars / CHARS_PER_TOKEN).ceil() as u64
}

// Lengths count UTF-16 code units, matching JavaScript `String.length` so
// token estimates line up with the TypeScript oracle for non-BMP input.
fn utf16_len(text: &str) -> f64 {
    text.encode_utf16().count() as f64
}

fn estimate_text_and_image_content_chars(content: &str) -> f64 {
    utf16_len(content)
}

fn estimate_block_content_chars(blocks: &[BlockContent]) -> f64 {
    let mut chars = 0.0;
    for block in blocks {
        chars += match block {
            BlockContent::Text(text) => utf16_len(&text.text),
            BlockContent::Image(_) => ESTIMATED_IMAGE_CHARS,
        };
    }
    chars
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    chars_per_token_ceil(utf16_len(text))
}

pub fn estimate_text_and_image_content_tokens(content: &str) -> u64 {
    chars_per_token_ceil(estimate_text_and_image_content_chars(content))
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    let mut chars = 0.0;

    match message {
        Message::User(message) => match &message.content {
            crate::ai::types::UserContent::Text(text) => {
                chars += estimate_text_and_image_content_chars(text);
            }
            crate::ai::types::UserContent::Blocks(blocks) => {
                chars += estimate_block_content_chars(blocks);
            }
        },
        Message::ToolResult(message) => {
            chars += estimate_block_content_chars(&message.content);
        }
        Message::Assistant(message) => {
            for block in &message.content {
                match block {
                    crate::ai::types::AssistantContent::Text(text) => {
                        chars += utf16_len(&text.text);
                    }
                    crate::ai::types::AssistantContent::Thinking(thinking) => {
                        chars += utf16_len(&thinking.thinking);
                    }
                    crate::ai::types::AssistantContent::ToolCall(tool_call) => {
                        chars += utf16_len(&tool_call.name);
                        chars += utf16_len(&safe_json_stringify(&tool_call.arguments));
                    }
                }
            }
        }
    }

    chars_per_token_ceil(chars)
}

struct LastUsageInfo {
    usage: Usage,
    index: usize,
}

fn get_last_assistant_usage_info(messages: &[Message]) -> Option<LastUsageInfo> {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut usage_info: Option<LastUsageInfo> = None;

    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            // A newer prefix message was inserted after this response (for
            // example, a compaction summary), so its usage cannot describe
            // the current prefix.
            let usage_applies_to_prefix = assistant.timestamp >= latest_prefix_timestamp;
            if usage_applies_to_prefix
                && assistant.stop_reason != crate::ai::types::StopReason::Aborted
                && assistant.stop_reason != crate::ai::types::StopReason::Error
                && calculate_context_tokens(&assistant.usage) > 0
            {
                usage_info = Some(LastUsageInfo {
                    usage: assistant.usage.clone(),
                    index,
                });
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(message.timestamp());
    }

    usage_info
}

/// Port of `estimateContextTokens` over a bare message list.
pub fn estimate_messages_tokens(messages: &[Message]) -> ContextUsageEstimate {
    if let Some(usage_info) = get_last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(&usage_info.usage);
        let mut trailing_tokens = 0u64;
        for message in &messages[usage_info.index + 1..] {
            trailing_tokens += estimate_message_tokens(message);
        }
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(usage_info.index),
        };
    }

    let mut tokens = 0u64;
    for message in messages {
        tokens += estimate_message_tokens(message);
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens(tools: Option<&Vec<Tool>>) -> u64 {
    let Some(tools) = tools else {
        return 0;
    };
    if tools.is_empty() {
        return 0;
    }
    estimate_text_tokens(&safe_json_stringify(tools))
}

/// Port of `estimateContextTokens` over a full context.
pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let estimate = estimate_messages_tokens(&context.messages);
    if let Some(last_usage_index) = estimate.last_usage_index {
        let added_names: std::collections::BTreeSet<String> = context.messages
            [last_usage_index + 1..]
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => result.added_tool_names.clone(),
                _ => None,
            })
            .flatten()
            .collect();
        let added_tools: Vec<Tool> = context
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .filter(|tool| added_names.contains(&tool.name))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let added_tool_tokens = estimate_tools_tokens(Some(&added_tools));
        return ContextUsageEstimate {
            tokens: estimate.tokens + added_tool_tokens,
            usage_tokens: estimate.usage_tokens,
            trailing_tokens: estimate.trailing_tokens + added_tool_tokens,
            last_usage_index: estimate.last_usage_index,
        };
    }

    let prefix_tokens = context
        .system_prompt
        .as_ref()
        .map(|prompt| estimate_text_tokens(prompt))
        .unwrap_or_default()
        + estimate_tools_tokens(context.tools.as_ref());

    ContextUsageEstimate {
        tokens: estimate.tokens + prefix_tokens,
        usage_tokens: estimate.usage_tokens,
        trailing_tokens: estimate.trailing_tokens + prefix_tokens,
        last_usage_index: estimate.last_usage_index,
    }
}
