//! Compaction (ADR-0011): shrink a chat's History into a model-written
//! summary plus a verbatim recent tail, using pi-core's compaction
//! primitives with their defaults — `should_compact` against the model's
//! context window minus the reserve (16384), a tail of roughly the
//! recent-tokens budget (20000), the upstream estimator (last real usage
//! as anchor plus a character heuristic), and a cut point that never
//! separates a tool call from its result. The summary request is made
//! through the SAME injected stream function the agent loop uses — with
//! the chat's current model, the upstream summarization system prompt and
//! template, no extended reasoning, no custom instructions, and the
//! previous summary when one exists — so credentials flow like any agent
//! request and the scripted provider covers summaries in tests. It does
//! NOT go through the upstream model registry.

use pi_core::agent::harness::compaction::compaction::{
    self, CompactionPreparation, DEFAULT_COMPACTION_SETTINGS,
};
use pi_core::agent::harness::compaction::utils::serialize_conversation;
use pi_core::agent::harness::messages::{convert_to_llm, create_compaction_summary_message};
use pi_core::agent::harness::session::types::Entry;
use pi_core::agent::types::{AgentMessage, StreamFn};
use pi_core::ai::types::{
    BlockContent, CacheRetention, Context, Message, Model, RoleUser, SimpleStreamOptions,
    StopReason, StreamOptions, TextContent, UserContent, UserMessage,
};

use crate::history::CompactionRecord;

// The summarization BODY templates are private upstream (only the system
// prompt is exported); these mirror them verbatim — pinned by the
// scripted-provider tests' shape assertions. If upstream's change, these
// must follow.
const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

/// What one compaction produced, ready to persist and to swap into the
/// in-memory History.
pub(crate) struct CompactionOutcome {
    pub record: CompactionRecord,
    /// The compacted History: the `compactionSummary` custom message
    /// followed by the retained tail.
    pub messages: Vec<AgentMessage>,
}

/// Whether the History is past the compaction threshold for `model`.
pub(crate) fn needed(history: &[AgentMessage], model: &Model) -> bool {
    compaction::should_compact(
        compaction::estimate_context_tokens(history).tokens,
        model.context_window,
        &DEFAULT_COMPACTION_SETTINGS,
    )
}

/// Compact `history` through `stream_fn`, or return `None` when the
/// upstream rule says there is nothing to do. The flat History is adapted
/// into a linear session-entry list for the upstream cut-point
/// preparation (`compactionSummary` custom messages become compaction
/// entries whose embedded tail is empty — the messages that follow them
/// in the flat list ARE the tail); the summarization itself operates on
/// plain message vectors.
pub(crate) async fn compact(
    history: &[AgentMessage],
    model: &Model,
    stream_fn: &StreamFn,
    api_key: &str,
    trigger: holt_doc::parts::CompactionTrigger,
) -> Result<Option<CompactionOutcome>, String> {
    if !needed(history, model) {
        return Ok(None);
    }
    let Some(preparation) =
        compaction::prepare_compaction(&flat_entries(history), DEFAULT_COMPACTION_SETTINGS)
            .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn,
        tokens_before,
        previous_summary,
        ..
    } = preparation;

    // The summary text, mirroring upstream `compact`'s two shapes exactly:
    // normally one request over the summarized prefix; when the cut split
    // a turn, the (possibly empty) history summary plus a turn-prefix
    // request, so the retained suffix stays understandable.
    let summary = if is_split_turn && !turn_prefix_messages.is_empty() {
        let history_text = if messages_to_summarize.is_empty() {
            "No prior history.".to_string()
        } else {
            summarize(
                &messages_to_summarize,
                previous_summary.as_deref(),
                model,
                stream_fn,
                api_key,
            )
            .await?
        };
        let prefix =
            summarize_turn_prefix(&turn_prefix_messages, model, stream_fn, api_key).await?;
        format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}")
    } else {
        summarize(
            &messages_to_summarize,
            previous_summary.as_deref(),
            model,
            stream_fn,
            api_key,
        )
        .await?
    };

    let timestamp = chrono::Utc::now().timestamp_millis();
    let mut messages = vec![create_compaction_summary_message(
        summary.clone(),
        tokens_before,
        timestamp,
    )];
    messages.extend(retained_tail.iter().cloned());
    let tokens_after = compaction::estimate_context_tokens(&messages).tokens;
    Ok(Some(CompactionOutcome {
        record: CompactionRecord {
            summary,
            tokens_before,
            tokens_after,
            trigger,
            timestamp,
            retained_tail: (messages.len() - 1) as u64,
        },
        messages,
    }))
}

/// One summary request through the injected stream function — upstream
/// `generate_summary`'s prompt and options, minus its model registry.
async fn summarize(
    messages_to_summarize: &[AgentMessage],
    previous_summary: Option<&str>,
    model: &Model,
    stream_fn: &StreamFn,
    api_key: &str,
) -> Result<String, String> {
    let base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };
    let conversation = serialize_conversation(&convert_to_llm(messages_to_summarize.to_vec()));
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous_summary) = previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>\n\n"
        ));
    }
    prompt.push_str(base_prompt);
    complete_summary(&prompt, model, stream_fn, api_key, 0.8).await
}

/// The split-turn prefix request — upstream `generate_turn_prefix_summary`
/// through the same seam.
async fn summarize_turn_prefix(
    messages: &[AgentMessage],
    model: &Model,
    stream_fn: &StreamFn,
    api_key: &str,
) -> Result<String, String> {
    let conversation = serialize_conversation(&convert_to_llm(messages.to_vec()));
    let prompt = format!(
        "<conversation>\n{conversation}\n</conversation>\n\n{}",
        TURN_PREFIX_SUMMARIZATION_PROMPT
    );
    complete_summary(&prompt, model, stream_fn, api_key, 0.5).await
}

/// Run one summary completion and return its text. `max_tokens_factor`
/// mirrors upstream: a fraction of the reserve, capped by the model.
async fn complete_summary(
    prompt: &str,
    model: &Model,
    stream_fn: &StreamFn,
    api_key: &str,
    max_tokens_factor: f64,
) -> Result<String, String> {
    let max_tokens =
        (max_tokens_factor * DEFAULT_COMPACTION_SETTINGS.reserve_tokens as f64).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        max_tokens.min(model.max_tokens)
    } else {
        max_tokens
    };
    let mut options = SimpleStreamOptions {
        base: StreamOptions {
            max_tokens: Some(max_tokens),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
        // No extended reasoning, no custom instructions (ADR-0011).
        reasoning: None,
        ..Default::default()
    };
    options.base.base.api_key = Some(api_key.to_string());
    let context = Context {
        system_prompt: Some(compaction::SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
                text: prompt.to_string(),
                ..Default::default()
            })]),
            timestamp: 0,
        })],
        tools: None,
    };
    let stream = stream_fn(model, &context, Some(&options))?;
    while let Some(event) = stream.next().await {
        if event.is_terminal() {
            break;
        }
    }
    let response = stream.result().await;
    match response.stop_reason {
        StopReason::Aborted => Err(response
            .error_message
            .unwrap_or_else(|| "Summarization aborted".into())),
        StopReason::Error => Err(format!(
            "Summarization failed: {}",
            response
                .error_message
                .unwrap_or_else(|| "Unknown error".into())
        )),
        _ => Ok(pi_core::ai::utils::text::content_text(
            &response.content,
            "\n",
        )),
    }
}

/// The flat History as a linear session-entry list: sequential ids and
/// parents, one entry per message, with each `compactionSummary` custom
/// message mapped back to a compaction entry (empty embedded tail — the
/// messages following it in the flat list are the tail).
fn flat_entries(history: &[AgentMessage]) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(history.len());
    for (index, message) in history.iter().enumerate() {
        let id = format!("e{index}");
        let parent_id = (index > 0).then(|| format!("e{}", index - 1));
        let timestamp = message_timestamp(message);
        let entry = match message {
            AgentMessage::Custom(custom) if custom.role == "compactionSummary" => {
                let tokens_before = custom
                    .value
                    .get("tokensBefore")
                    .and_then(|value| value.as_u64())
                    .unwrap_or_default();
                Entry::Compaction {
                    summary: custom
                        .value
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    retained_tail: Vec::new(),
                    tokens_before,
                    details: None,
                    usage: None,
                    parent_id,
                    seq: index as u64,
                    timestamp,
                    id,
                }
            }
            message => Entry::Message {
                id,
                message: message.clone(),
                terminate: None,
                parent_id,
                seq: index as u64,
                timestamp,
            },
        };
        entries.push(entry);
    }
    entries
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
