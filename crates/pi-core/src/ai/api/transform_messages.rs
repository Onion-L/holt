//! Port of `pi-core/ai/src/api/transform-messages.ts`: cross-provider
//! message normalization before replaying history to an API.

use std::collections::BTreeMap;

use crate::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, Message, Model, StopReason, TextContent,
    ToolCall, ToolResultMessage, UserContent,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

fn replace_images_with_placeholder(
    content: &[BlockContent],
    placeholder: &str,
) -> Vec<BlockContent> {
    let mut result: Vec<BlockContent> = Vec::new();
    let mut previous_was_placeholder = false;

    for block in content {
        if let BlockContent::Image(_) = block {
            if !previous_was_placeholder {
                result.push(BlockContent::Text(TextContent {
                    text: placeholder.to_string(),
                    ..Default::default()
                }));
            }
            previous_was_placeholder = true;
            continue;
        }

        let is_placeholder = matches!(block, BlockContent::Text(text) if text.text == placeholder);
        result.push(block.clone());
        previous_was_placeholder = is_placeholder;
    }

    result
}

fn downgrade_unsupported_images(messages: &[Message], model: &Model) -> Vec<Message> {
    if model.input.contains(&crate::ai::types::ModelInput::Image) {
        return messages.to_vec();
    }

    messages
        .iter()
        .map(|message| match message {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => UserContent::Text(text.clone()),
                    UserContent::Blocks(blocks) => UserContent::Blocks(
                        replace_images_with_placeholder(blocks, NON_VISION_USER_IMAGE_PLACEHOLDER),
                    ),
                };
                Message::User(crate::ai::types::UserMessage {
                    role: user.role,
                    content,
                    timestamp: user.timestamp,
                })
            }
            Message::ToolResult(result) => Message::ToolResult(Box::new(ToolResultMessage {
                role: result.role,
                tool_call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
                content: replace_images_with_placeholder(
                    &result.content,
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                ),
                details: result.details.clone(),
                usage: result.usage.clone(),
                added_tool_names: result.added_tool_names.clone(),
                is_error: result.is_error,
                timestamp: result.timestamp,
            })),
            other => other.clone(),
        })
        .collect()
}

/// Normalizer for tool call IDs, receiving the raw id and the owning
/// assistant message.
pub type ToolCallIdNormalizer<'a> = dyn Fn(&str, &AssistantMessage) -> String + 'a;

/// Port of `transformMessages`: normalizes tool call IDs for cross-provider
/// compatibility (Anthropic requires `^[a-zA-Z0-9_-]+$`, max 64 chars),
/// downgrades images for non-vision models, converts cross-model thinking
/// blocks to text, and inserts synthetic tool results for orphaned calls.
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<&ToolCallIdNormalizer>,
) -> Vec<Message> {
    // Map of original tool call IDs to normalized IDs.
    let mut tool_call_id_map: BTreeMap<String, String> = BTreeMap::new();
    let image_aware_messages = downgrade_unsupported_images(messages, model);

    // First pass: transform messages.
    let transformed: Vec<Message> = image_aware_messages
        .into_iter()
        .map(|message| match message {
            Message::User(user) => Message::User(user),
            Message::ToolResult(result) => {
                // Normalize toolCallId if we have a mapping.
                match tool_call_id_map.get(&result.tool_call_id) {
                    Some(normalized_id) if *normalized_id != result.tool_call_id => {
                        Message::ToolResult(Box::new(ToolResultMessage {
                            tool_call_id: normalized_id.clone(),
                            ..*result
                        }))
                    }
                    _ => Message::ToolResult(result),
                }
            }
            Message::Assistant(assistant) => {
                let assistant: &AssistantMessage = &assistant;
                let is_same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;

                let mut transformed_content: Vec<AssistantContent> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            // Redacted thinking is opaque encrypted content,
                            // only valid for the same model.
                            if thinking.redacted == Some(true) {
                                if is_same_model {
                                    transformed_content.push(block.clone());
                                }
                                continue;
                            }
                            // Same-model thinking with a signature replays.
                            if is_same_model && thinking.thinking_signature.is_some() {
                                transformed_content.push(block.clone());
                                continue;
                            }
                            // Skip empty thinking blocks; convert others.
                            if thinking.thinking.trim().is_empty() {
                                continue;
                            }
                            if is_same_model {
                                transformed_content.push(block.clone());
                            } else {
                                transformed_content.push(AssistantContent::Text(TextContent {
                                    text: thinking.thinking.clone(),
                                    ..Default::default()
                                }));
                            }
                        }
                        AssistantContent::Text(text) => {
                            if is_same_model {
                                transformed_content.push(block.clone());
                            } else {
                                transformed_content.push(AssistantContent::Text(TextContent {
                                    text: text.text.clone(),
                                    ..Default::default()
                                }));
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let mut normalized = tool_call.clone();
                            if !is_same_model && tool_call.thought_signature.is_some() {
                                normalized.thought_signature = None;
                            }
                            if !is_same_model && let Some(normalize) = normalize_tool_call_id {
                                let normalized_id = normalize(&tool_call.id, assistant);
                                if normalized_id != tool_call.id {
                                    tool_call_id_map
                                        .insert(tool_call.id.clone(), normalized_id.clone());
                                    normalized.id = normalized_id;
                                }
                            }
                            transformed_content.push(AssistantContent::ToolCall(normalized));
                        }
                    }
                }

                Message::Assistant(Box::new(AssistantMessage {
                    content: transformed_content,
                    ..assistant.clone()
                }))
            }
        })
        .collect();

    // Second pass: insert synthetic empty tool results for orphaned tool
    // calls. This preserves thinking signatures and satisfies API
    // requirements.
    let mut result: Vec<Message> = Vec::new();
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut existing_tool_result_ids: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let insert_synthetic_tool_results =
        |result: &mut Vec<Message>,
         pending: &mut Vec<ToolCall>,
         existing: &mut std::collections::BTreeSet<String>| {
            if pending.is_empty() {
                return;
            }
            for tool_call in pending.iter() {
                if !existing.contains(&tool_call.id) {
                    result.push(Message::ToolResult(Box::new(ToolResultMessage {
                        role: crate::ai::types::RoleToolResult,
                        tool_call_id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                        content: vec![BlockContent::Text(TextContent {
                            text: "No result provided".to_string(),
                            ..Default::default()
                        })],
                        is_error: true,
                        timestamp: crate::ai::auth::resolve::now_millis(),
                        ..Default::default()
                    })));
                }
            }
            pending.clear();
            existing.clear();
        };

    for message in &transformed {
        match message {
            Message::Assistant(assistant) => {
                // Insert synthetic results for previously orphaned calls.
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );

                // Skip errored/aborted assistant messages entirely: they are
                // incomplete turns that should not be replayed.
                if assistant.stop_reason == StopReason::Error
                    || assistant.stop_reason == StopReason::Aborted
                {
                    continue;
                }

                // Track tool calls from this assistant message.
                let tool_calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    pending_tool_calls = tool_calls;
                    existing_tool_result_ids = std::collections::BTreeSet::new();
                }

                result.push(message.clone());
            }
            Message::ToolResult(tool_result) => {
                existing_tool_result_ids.insert(tool_result.tool_call_id.clone());
                result.push(message.clone());
            }
            Message::User(_) => {
                // User message interrupts tool flow.
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(message.clone());
            }
        }
    }

    // Synthesize results for unresolved tool calls at the end.
    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );

    result
}
