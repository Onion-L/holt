//! Port of `pi-core/agent/src/harness/compaction/branch-summarization.ts`.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::harness::compaction::compaction::{
    SUMMARIZATION_SYSTEM_PROMPT, complete_simple_with_retries, estimate_tokens,
};
use crate::agent::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use crate::agent::harness::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::agent::harness::session::memory::Session;
use crate::agent::harness::session::types::{Entry, EntryType, SessionError, SessionErrorCode};
use crate::agent::harness::types::{BranchSummaryError, BranchSummaryErrorCode};
use crate::agent::types::AgentMessage;
use crate::ai::models::Models;
use crate::ai::types::{
    BlockContent, Context, Message, Model, RoleUser, SimpleStreamOptions, StopReason, TextContent,
    Usage, UserContent,
};
use crate::ai::utils::retry::{RetryCallbacks, RetryPolicy};
use crate::ai::utils::text::content_text;

/// Generated branch summary data ready to be persisted.
#[derive(Clone, Debug)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Usage,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// File-operation details stored on generated branch summary entries.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Prepared branch content for summarization.
#[derive(Clone, Debug)]
pub struct BranchPreparation {
    pub messages: Vec<AgentMessage>,
    pub file_ops: FileOperations,
    pub total_tokens: u64,
}

/// Entries selected for branch summarization.
#[derive(Clone, Debug)]
pub struct CollectEntriesResult {
    pub entries: Vec<Entry>,
    pub common_ancestor_id: Option<String>,
}

/// Collect entries that should be summarized before navigating to a
/// different session tree entry.
pub async fn collect_entries_for_branch_summary(
    session: &Session,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> Result<CollectEntriesResult, SessionError> {
    let Some(old_leaf_id) = old_leaf_id else {
        return Ok(CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        });
    };
    let old_path_entries = session
        .find_entries_on_branch(
            Default::default(),
            crate::agent::harness::session::types::BranchBounds {
                start: Some(old_leaf_id.to_string()),
                ..Default::default()
            },
        )
        .await?;
    let old_path: std::collections::HashSet<String> = old_path_entries
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    let target_path = session
        .find_entries_on_branch(
            Default::default(),
            crate::agent::harness::session::types::BranchBounds {
                start: Some(target_id.to_string()),
                ..Default::default()
            },
        )
        .await?;
    let mut common_ancestor_id: Option<String> = None;
    for entry in &target_path {
        if old_path.contains(entry.id()) {
            common_ancestor_id = Some(entry.id().to_string());
            break;
        }
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<String> = Some(old_leaf_id.to_string());

    while let Some(id) = current.clone()
        && Some(id.as_str()) != common_ancestor_id.as_deref()
    {
        let entry = session.get_entry(&id).await.ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::InvalidEntry,
                format!("Entry {id} not found"),
            )
        })?;
        current = entry.parent_id().map(str::to_string);
        entries.push(entry);
    }
    entries.reverse();

    Ok(CollectEntriesResult {
        entries,
        common_ancestor_id,
    })
}

fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => {
            // Tool results are excluded from branch summaries.
            if matches!(message, AgentMessage::ToolResult(_)) {
                None
            } else {
                Some(message.clone())
            }
        }
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

/// Prepare branch entries for summarization within an optional token
/// budget.
pub fn prepare_branch_entries(entries: &[Entry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens: u64 = 0;
    for entry in entries {
        if let Entry::BranchSummary {
            details: Some(details),
            ..
        } = entry
            && let Ok(details) = serde_json::from_value::<BranchSummaryDetails>(details.clone())
        {
            for file in details.read_files {
                file_ops.read.insert(file);
            }
            for file in details.modified_files {
                file_ops.edited.insert(file);
            }
        }
    }
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);

        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            if matches!(
                entry.entry_type(),
                EntryType::Compaction | EntryType::BranchSummary
            ) && total_tokens < token_budget * 9 / 10
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }

        messages.insert(0, message);
        total_tokens += tokens;
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Options for generating a branch summary.
pub struct GenerateBranchSummaryOptions<'a> {
    pub models: Arc<Models>,
    pub model: Model,
    pub signal: Option<&'a CancellationToken>,
    pub custom_instructions: Option<&'a str>,
    pub replace_instructions: bool,
    pub reserve_tokens: Option<u64>,
    pub retry: Option<&'a RetryPolicy>,
    pub callbacks: Option<&'a RetryCallbacks>,
}

/// Generate a summary for abandoned branch entries.
pub async fn generate_branch_summary(
    entries: &[Entry],
    options: &GenerateBranchSummaryOptions<'_>,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    let reserve_tokens = options.reserve_tokens.unwrap_or(16384);
    let context_window = if options.model.context_window > 0 {
        options.model.context_window
    } else {
        128_000
    };
    let token_budget = context_window.saturating_sub(reserve_tokens);

    let preparation = prepare_branch_entries(entries, token_budget);

    if preparation.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_string(),
            usage: Usage::default(),
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }
    let llm_messages = convert_to_llm(preparation.messages.clone());
    let conversation_text = serialize_conversation(&llm_messages);
    let instructions = match (options.replace_instructions, options.custom_instructions) {
        (true, Some(custom)) => custom.to_string(),
        (false, Some(custom)) => {
            format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}")
        }
        _ => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let summarization_messages = vec![Message::User(crate::ai::types::UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
            text: prompt_text,
            ..Default::default()
        })]),
        timestamp: 0,
    })];
    let completion_options = SimpleStreamOptions {
        base: crate::ai::types::StreamOptions {
            base: crate::ai::types::ProviderRequestOptions {
                signal: options.signal.cloned(),
                ..Default::default()
            },
            max_tokens: Some(2048),
            ..Default::default()
        },
        ..Default::default()
    };
    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages,
        tools: None,
    };
    let response = complete_simple_with_retries(
        &options.models,
        &options.model,
        &context,
        completion_options,
        options.retry,
        options.callbacks,
    )
    .await;
    if response.stop_reason == StopReason::Aborted {
        return Err(BranchSummaryError::new(
            BranchSummaryErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Branch summary aborted".to_string()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(BranchSummaryError::new(
            BranchSummaryErrorCode::SummarizationFailed,
            format!(
                "Branch summary failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_string())
            ),
        ));
    }

    let mut summary = content_text(&response.content, "\n");
    summary = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(BranchSummaryResult {
        summary: if summary.is_empty() {
            "No summary generated".to_string()
        } else {
            summary
        },
        usage: response.usage,
        read_files,
        modified_files,
    })
}
