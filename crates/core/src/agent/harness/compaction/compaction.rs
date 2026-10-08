//! Port of `pi-core/agent/src/harness/compaction/compaction.ts`.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use crate::agent::harness::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::agent::harness::session::context::build_session_context;
use crate::agent::harness::session::types::{Entry, EntryType};
use crate::agent::harness::types::{CompactionError, CompactionErrorCode};
use crate::agent::types::{AgentMessage, ThinkingLevel};
use crate::ai::models::Models;
use crate::ai::types::{
    AssistantContent, AssistantMessage, CacheRetention, Context, Message, Model, RoleUser,
    SimpleStreamOptions, StopReason, Usage, UserContent,
};
use crate::ai::utils::retry::{RetryCallbacks, RetryPolicy, retry_assistant_call};
use crate::ai::utils::text::content_text;
use crate::ai::utils::uuid::uuidv7;

/// File-operation details stored on generated compaction entries.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

fn safe_json_stringify(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "undefined".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "[unserializable]".to_string()),
    }
}

fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[Entry],
    prev_compaction_index: isize,
) -> FileOperations {
    let mut file_ops = create_file_ops();
    if prev_compaction_index >= 0
        && let Some(Entry::Compaction {
            details: Some(details),
            ..
        }) = entries.get(prev_compaction_index as usize)
        && let Ok(details) = serde_json::from_value::<CompactionDetails>(details.clone())
    {
        for file in details.read_files {
            file_ops.read.insert(file);
        }
        for file in details.modified_files {
            file_ops.edited.insert(file);
        }
    }
    for message in messages {
        extract_file_ops_from_message(message, &mut file_ops);
    }
    file_ops
}

fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => Some(message.clone()),
        Entry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } => Some(create_branch_summary_message(
            summary.clone(),
            from_id.clone(),
            *timestamp,
        )),
        Entry::Compaction {
            summary,
            tokens_before,
            timestamp,
            ..
        } => Some(create_compaction_summary_message(
            summary.clone(),
            *tokens_before,
            *timestamp,
        )),
        _ => None,
    }
}

fn get_message_from_entry_for_compaction(entry: &Entry) -> Option<AgentMessage> {
    if matches!(entry, Entry::Compaction { .. }) {
        return None;
    }
    get_message_from_entry(entry)
}

/// Generated compaction data ready to be persisted as a compaction entry.
#[derive(Clone, Debug)]
pub struct CompactResult {
    pub summary: String,
    pub tokens_before: u64,
    pub usage: Usage,
    pub retained_tail: Vec<AgentMessage>,
    pub details: CompactionDetails,
}

/// Port of `completeSimpleWithRetries`: summaries are standalone requests,
/// so isolate routing and avoid cache writes that cannot be reused.
pub async fn complete_simple_with_retries(
    models: &Arc<Models>,
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks>,
) -> AssistantMessage {
    let request_options = SimpleStreamOptions {
        base: crate::ai::types::StreamOptions {
            cache_retention: Some(CacheRetention::None),
            session_id: Some(uuidv7()),
            ..options.base
        },
        ..options
    };
    let signal = request_options.base.base.signal.clone();
    retry_assistant_call(
        || async {
            models
                .complete_simple(model, context, Some(request_options.clone()))
                .await
        },
        retry,
        signal.as_ref(),
        callbacks,
    )
    .await
}

fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let sum = |a: u64, b: u64| a + b;
    Usage {
        input: sum(first.input, second.input),
        output: sum(first.output, second.output),
        cache_read: sum(first.cache_read, second.cache_read),
        cache_write: sum(first.cache_write, second.cache_write),
        cache_write_1h: match (first.cache_write_1h, second.cache_write_1h) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
        },
        reasoning: match (first.reasoning, second.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
        },
        total_tokens: sum(first.total_tokens, second.total_tokens),
        cost: crate::ai::types::UsageCost {
            input: crate::ai::types::JsF64(first.cost.input.0 + second.cost.input.0),
            output: crate::ai::types::JsF64(first.cost.output.0 + second.cost.output.0),
            cache_read: crate::ai::types::JsF64(first.cost.cache_read.0 + second.cost.cache_read.0),
            cache_write: crate::ai::types::JsF64(
                first.cost.cache_write.0 + second.cost.cache_write.0,
            ),
            total: crate::ai::types::JsF64(first.cost.total.0 + second.cost.total.0),
        },
    }
}

/// Compaction thresholds and retention settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

/// Default compaction settings used by the harness.
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

/// Calculate total context tokens from provider usage.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    let total = usage.total_tokens;
    if total != 0 {
        total
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn get_assistant_usage(message: &AgentMessage) -> Option<Usage> {
    let AgentMessage::Assistant(assistant) = message else {
        return None;
    };
    if assistant.stop_reason != StopReason::Aborted
        && assistant.stop_reason != StopReason::Error
        && calculate_context_tokens(&assistant.usage) > 0
    {
        Some(assistant.usage.clone())
    } else {
        None
    }
}

/// Return usage from the last valid assistant message in session entries.
pub fn get_last_assistant_usage(entries: &[Entry]) -> Option<Usage> {
    for entry in entries.iter().rev() {
        if let Entry::Message { message, .. } = entry
            && let Some(usage) = get_assistant_usage(message)
        {
            return Some(usage);
        }
    }
    None
}

/// Estimated context-token usage for a message list.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

fn get_last_assistant_usage_info(messages: &[AgentMessage]) -> Option<(Usage, usize)> {
    for (index, message) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(message) {
            return Some((usage, index));
        }
    }
    None
}

/// Estimate context tokens for messages using provider usage when available.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let Some((usage, index)) = get_last_assistant_usage_info(messages) else {
        let estimated = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(&usage);
    let trailing_tokens: u64 = messages[index + 1..].iter().map(estimate_tokens).sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// Return whether context usage exceeds the configured compaction threshold.
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn estimate_text_and_image_user_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => text.len(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                crate::ai::types::BlockContent::Text(text) => text.text.len(),
                crate::ai::types::BlockContent::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
    }
}

fn custom_message_role(message: &AgentMessage) -> &str {
    match message {
        AgentMessage::Custom(custom) => custom.role.as_str(),
        _ => "",
    }
}

fn custom_field_str(message: &AgentMessage, field: &str) -> Option<String> {
    let AgentMessage::Custom(custom) = message else {
        return None;
    };
    custom
        .value
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn custom_content_chars(message: &AgentMessage) -> usize {
    let AgentMessage::Custom(custom) = message else {
        return 0;
    };
    match custom.value.get("content") {
        Some(serde_json::Value::String(text)) => text.len(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block {
                serde_json::Value::Object(map) => {
                    let text = map
                        .get("text")
                        .and_then(|value| value.as_str())
                        .map(str::len)
                        .unwrap_or_default();
                    let kind = map.get("type").and_then(|value| value.as_str());
                    if kind == Some("image") {
                        ESTIMATED_IMAGE_CHARS
                    } else {
                        text
                    }
                }
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Estimate token count for one message using a conservative character
/// heuristic.
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars: usize = match message {
        AgentMessage::User(user) => estimate_text_and_image_user_chars(&user.content),
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => text.text.len(),
                AssistantContent::Thinking(thinking) => thinking.thinking.len(),
                AssistantContent::ToolCall(tool_call) => {
                    tool_call.name.len()
                        + safe_json_stringify(&serde_json::Value::Object(
                            tool_call.arguments.clone(),
                        ))
                        .len()
                }
            })
            .sum(),
        AgentMessage::ToolResult(tool_result) => tool_result
            .content
            .iter()
            .map(|block| match block {
                crate::ai::types::BlockContent::Text(text) => text.text.len(),
                crate::ai::types::BlockContent::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        AgentMessage::Custom(_) => match custom_message_role(message) {
            "custom" | "toolResult" => custom_content_chars(message),
            "bashExecution" => {
                let command = custom_field_str(message, "command").unwrap_or_default();
                let output = custom_field_str(message, "output").unwrap_or_default();
                command.len() + output.len()
            }
            "branchSummary" | "compactionSummary" => custom_field_str(message, "summary")
                .unwrap_or_default()
                .len(),
            _ => 0,
        },
    };
    (chars as u64).div_ceil(4)
}

/// Message roles that are valid compaction cut points.
fn is_valid_cut_message_role(message: &AgentMessage) -> bool {
    match message {
        AgentMessage::User(_) | AgentMessage::Assistant(_) => true,
        AgentMessage::Custom(custom) => matches!(
            custom.role.as_str(),
            "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
        ),
        AgentMessage::ToolResult(_) => false,
    }
}

fn find_valid_cut_points(entries: &[Entry], start_index: usize, end_index: usize) -> Vec<usize> {
    let mut cut_points: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate().take(end_index).skip(start_index) {
        if let Entry::Message { message, .. } = entry
            && is_valid_cut_message_role(message)
        {
            cut_points.push(index);
        }
        if entry.entry_type() == EntryType::BranchSummary {
            cut_points.push(index);
        }
    }
    cut_points
}

/// Find the user-visible message that starts the turn containing an entry.
pub fn find_turn_start_index(entries: &[Entry], entry_index: usize, start_index: usize) -> isize {
    for index in (start_index..=entry_index).rev() {
        let entry = &entries[index];
        if entry.entry_type() == EntryType::BranchSummary {
            return index as isize;
        }
        if let Entry::Message { message, .. } = entry {
            let starts_turn = matches!(message, AgentMessage::User(_))
                || custom_message_role(message) == "bashExecution";
            if starts_turn {
                return index as isize;
            }
        }
    }
    -1
}

/// Cut point selected for compaction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutPointResult {
    pub first_kept_entry_index: usize,
    pub turn_start_index: isize,
    pub is_split_turn: bool,
}

/// Find the compaction cut point that keeps approximately the requested
/// recent-token budget.
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: -1,
            is_split_turn: false,
        };
    }
    let mut accumulated_tokens: u64 = 0;
    let mut cut_index = cut_points[0];

    for index in (start_index..end_index).rev() {
        let entry = &entries[index];
        let Entry::Message { message, .. } = entry else {
            continue;
        };
        accumulated_tokens += estimate_tokens(message);
        if accumulated_tokens >= keep_recent_tokens {
            for cut_point in &cut_points {
                if *cut_point >= index {
                    cut_index = *cut_point;
                    break;
                }
            }
            break;
        }
    }
    while cut_index > start_index {
        let prev_entry = &entries[cut_index - 1];
        if matches!(
            prev_entry.entry_type(),
            EntryType::Compaction | EntryType::Message
        ) {
            break;
        }
        cut_index -= 1;
    }
    let cut_entry = &entries[cut_index];
    let is_user_message = matches!(cut_entry, Entry::Message { message, .. } if matches!(message, AgentMessage::User(_)));
    let turn_start_index = if is_user_message {
        -1
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !is_user_message && turn_start_index != -1,
    }
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Options for the summary generators.
#[derive(Clone, Default)]
pub struct SummaryOptions<'a> {
    pub signal: Option<&'a CancellationToken>,
    pub custom_instructions: Option<&'a str>,
    pub previous_summary: Option<&'a str>,
    pub thinking_level: Option<ThinkingLevel>,
    pub retry: Option<&'a RetryPolicy>,
    pub callbacks: Option<&'a RetryCallbacks>,
}

/// Generate or update a conversation summary for compaction.
pub async fn generate_summary(
    current_messages: &[AgentMessage],
    models: &Arc<Models>,
    model: &Model,
    reserve_tokens: u64,
    options: &SummaryOptions<'_>,
) -> Result<String, CompactionError> {
    generate_summary_with_usage(current_messages, models, model, reserve_tokens, options)
        .await
        .map(|result| result.0)
}

/// Generate or update a conversation summary and return its provider usage.
pub async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    models: &Arc<Models>,
    model: &Model,
    reserve_tokens: u64,
    options: &SummaryOptions<'_>,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = (0.8 * reserve_tokens as f64).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        max_tokens.min(model.max_tokens)
    } else {
        max_tokens
    };
    let base_prompt = if options.previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };
    let base_prompt_owned = match options.custom_instructions {
        Some(custom_instructions) => {
            format!("{base_prompt}\n\nAdditional focus: {custom_instructions}")
        }
        None => base_prompt.to_string(),
    };
    let llm_messages: Vec<Message> = convert_to_llm(current_messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous_summary) = options.previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&base_prompt_owned);

    let summarization_messages = vec![Message::User(crate::ai::types::UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![crate::ai::types::BlockContent::Text(
            crate::ai::types::TextContent {
                text: prompt_text,
                ..Default::default()
            },
        )]),
        timestamp: 0,
    })];

    let reasoning = if model.reasoning
        && let Some(level) = options.thinking_level
        && level != ThinkingLevel::Off
    {
        level.as_provider_reasoning()
    } else {
        None
    };
    let completion_options = SimpleStreamOptions {
        base: crate::ai::types::StreamOptions {
            base: crate::ai::types::ProviderRequestOptions {
                signal: options.signal.cloned(),
                ..Default::default()
            },
            max_tokens: Some(max_tokens),
            ..Default::default()
        },
        reasoning,
        ..Default::default()
    };

    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages,
        tools: None,
    };
    let response = complete_simple_with_retries(
        models,
        model,
        &context,
        completion_options,
        options.retry,
        options.callbacks,
    )
    .await;
    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::new(
            CompactionErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Summarization aborted".to_string()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::new(
            CompactionErrorCode::SummarizationFailed,
            format!(
                "Summarization failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_string())
            ),
        ));
    }

    let text_content = content_text(&response.content, "\n");
    Ok((text_content, response.usage))
}

/// Prepared inputs for a compaction run.
#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub retained_tail: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// Prepare session entries for compaction, or return `None` when
/// compaction is not applicable.
pub fn prepare_compaction(
    path_entries: &[Entry],
    settings: CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries.is_empty()
        || path_entries[path_entries.len() - 1].entry_type() == EntryType::Compaction
    {
        return Ok(None);
    }

    let mut prev_compaction_index: isize = -1;
    for (index, entry) in path_entries.iter().enumerate().rev() {
        if entry.entry_type() == EntryType::Compaction {
            prev_compaction_index = index as isize;
            break;
        }
    }

    let mut previous_summary: Option<String> = None;
    let mut compactable_entries: Vec<Entry> = path_entries.to_vec();
    if prev_compaction_index >= 0 {
        let index = prev_compaction_index as usize;
        if let Entry::Compaction {
            id,
            seq,
            summary,
            retained_tail,
            ..
        } = &path_entries[index]
        {
            previous_summary = Some(summary.clone());
            let mut virtual_entries: Vec<Entry> = Vec::new();
            for (tail_index, message) in retained_tail.iter().enumerate() {
                virtual_entries.push(Entry::Message {
                    id: format!("{id}:retained:{tail_index}"),
                    seq: *seq,
                    parent_id: Some(if tail_index == 0 {
                        id.clone()
                    } else {
                        format!("{id}:retained:{}", tail_index - 1)
                    }),
                    timestamp: message_timestamp(message),
                    message: message.clone(),
                    terminate: None,
                });
            }
            compactable_entries = virtual_entries;
            compactable_entries.extend(path_entries[index + 1..].to_vec());
        }
    }
    let boundary_end = compactable_entries.len();

    let tokens_before =
        estimate_context_tokens(&build_session_context(path_entries, &Default::default()).messages)
            .tokens;

    let cut_point = find_cut_point(
        &compactable_entries,
        0,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index.max(0) as usize
    } else {
        cut_point.first_kept_entry_index
    };
    let mut messages_to_summarize: Vec<AgentMessage> = Vec::new();
    for entry in &compactable_entries[..history_end] {
        if let Some(message) = get_message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(message);
        }
    }
    let mut turn_prefix_messages: Vec<AgentMessage> = Vec::new();
    if cut_point.is_split_turn {
        for entry in &compactable_entries
            [cut_point.turn_start_index.max(0) as usize..cut_point.first_kept_entry_index]
        {
            if let Some(message) = get_message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(message);
            }
        }
    }
    let mut retained_tail: Vec<AgentMessage> = Vec::new();
    for entry in &compactable_entries[cut_point.first_kept_entry_index..boundary_end] {
        if let Some(message) = get_message_from_entry_for_compaction(entry) {
            retained_tail.push(message);
        }
    }
    let mut file_ops =
        extract_file_operations(&messages_to_summarize, path_entries, prev_compaction_index);
    if cut_point.is_split_turn {
        for message in &turn_prefix_messages {
            extract_file_ops_from_message(message, &mut file_ops);
        }
    }

    Ok(Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    }))
}

fn message_timestamp(message: &AgentMessage) -> i64 {
    match message {
        AgentMessage::User(user) => user.timestamp,
        AgentMessage::Assistant(assistant) => assistant.timestamp,
        AgentMessage::ToolResult(tool_result) => tool_result.timestamp,
        AgentMessage::Custom(custom) => custom
            .value
            .get("timestamp")
            .and_then(|value| value.as_i64())
            .unwrap_or_default(),
    }
}

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

/// Options for [`compact`] beyond the preparation itself.
pub struct CompactOptions<'a> {
    pub models: &'a Arc<Models>,
    pub model: &'a Model,
    pub custom_instructions: Option<&'a str>,
    pub signal: Option<&'a CancellationToken>,
    pub thinking_level: Option<ThinkingLevel>,
    pub retry: Option<&'a RetryPolicy>,
    pub callbacks: Option<&'a RetryCallbacks>,
}

/// Generate compaction summary data from prepared session history.
pub async fn compact(
    preparation: CompactionPreparation,
    options: &CompactOptions<'_>,
) -> Result<CompactResult, CompactionError> {
    let CompactOptions {
        models,
        model,
        custom_instructions,
        signal,
        thinking_level,
        retry,
        callbacks,
    } = *options;
    let CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    } = preparation;

    let summary_options = SummaryOptions {
        signal,
        custom_instructions,
        previous_summary: previous_summary.as_deref(),
        thinking_level,
        retry,
        callbacks,
    };

    let summary: String;
    let summary_usage: Usage;

    if is_split_turn && !turn_prefix_messages.is_empty() {
        let mut history_text = "No prior history.".to_string();
        let mut history_usage: Option<Usage> = None;
        if !messages_to_summarize.is_empty() {
            let (text, usage) = generate_summary_with_usage(
                &messages_to_summarize,
                models,
                model,
                settings.reserve_tokens,
                &summary_options,
            )
            .await?;
            history_text = text;
            history_usage = Some(usage);
        }
        let prefix_options = SummaryOptions {
            signal,
            custom_instructions: None,
            previous_summary: None,
            thinking_level,
            retry,
            callbacks,
        };
        let (turn_prefix_text, turn_prefix_usage) = generate_turn_prefix_summary(
            &turn_prefix_messages,
            models,
            model,
            settings.reserve_tokens,
            &prefix_options,
        )
        .await?;
        summary = format!(
            "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_text}"
        );
        summary_usage = match history_usage {
            Some(history_usage) => combine_usage(&history_usage, &turn_prefix_usage),
            None => turn_prefix_usage,
        };
    } else {
        let (text, usage) = generate_summary_with_usage(
            &messages_to_summarize,
            models,
            model,
            settings.reserve_tokens,
            &summary_options,
        )
        .await?;
        summary = text;
        summary_usage = usage;
    }

    let (read_files, modified_files) = compute_file_lists(&file_ops);
    let summary = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );

    Ok(CompactResult {
        summary,
        tokens_before,
        usage: summary_usage,
        retained_tail,
        details: CompactionDetails {
            read_files,
            modified_files,
        },
    })
}

async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    models: &Arc<Models>,
    model: &Model,
    reserve_tokens: u64,
    options: &SummaryOptions<'_>,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = (0.5 * reserve_tokens as f64).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        max_tokens.min(model.max_tokens)
    } else {
        max_tokens
    };
    let llm_messages = convert_to_llm(messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    let summarization_messages = vec![Message::User(crate::ai::types::UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![crate::ai::types::BlockContent::Text(
            crate::ai::types::TextContent {
                text: prompt_text,
                ..Default::default()
            },
        )]),
        timestamp: 0,
    })];

    let reasoning = if model.reasoning
        && let Some(level) = options.thinking_level
        && level != ThinkingLevel::Off
    {
        level.as_provider_reasoning()
    } else {
        None
    };
    let completion_options = SimpleStreamOptions {
        base: crate::ai::types::StreamOptions {
            base: crate::ai::types::ProviderRequestOptions {
                signal: options.signal.cloned(),
                ..Default::default()
            },
            max_tokens: Some(max_tokens),
            ..Default::default()
        },
        reasoning,
        ..Default::default()
    };

    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages,
        tools: None,
    };
    let response = complete_simple_with_retries(
        models,
        model,
        &context,
        completion_options,
        options.retry,
        options.callbacks,
    )
    .await;
    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::new(
            CompactionErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Turn prefix summarization aborted".to_string()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::new(
            CompactionErrorCode::SummarizationFailed,
            format!(
                "Turn prefix summarization failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_string())
            ),
        ));
    }

    Ok((content_text(&response.content, "\n"), response.usage))
}
