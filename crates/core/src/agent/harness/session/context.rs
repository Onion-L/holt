//! Port of `pi-core/agent/src/harness/session/context.ts`.

use std::sync::Arc;

use crate::agent::types::AgentMessage;

use super::super::messages::{create_branch_summary_message, create_compaction_summary_message};
use super::types::{Entry, EntryType};

/// Port of `SessionContext`.
#[derive(Clone, Debug, Default)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<SessionContextModel>,
    pub active_tool_names: Option<Vec<String>>,
}

/// The derived model pointer.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionContextModel {
    pub provider: String,
    pub model_id: String,
}

/// Port of `ContextEntryTransform`.
pub type ContextEntryTransform = Arc<dyn Fn(&[Entry]) -> Vec<Entry> + Send + Sync>;

/// Port of `CustomEntryContextMessageProjector`.
pub type CustomEntryContextMessageProjector =
    Arc<dyn Fn(&Entry, usize, &[Entry]) -> Option<Vec<AgentMessage>> + Send + Sync>;

/// Port of `SessionContextBuildOptions`.
#[derive(Clone, Default)]
pub struct SessionContextBuildOptions {
    pub entry_transforms: Vec<ContextEntryTransform>,
    pub entry_projectors: std::collections::HashMap<String, CustomEntryContextMessageProjector>,
}

fn derive_session_context_state(path_entries: &[Entry]) -> SessionContext {
    let mut thinking_level = "off".to_string();
    let mut model: Option<SessionContextModel> = None;
    let mut active_tool_names: Option<Vec<String>> = None;

    for entry in path_entries {
        match entry {
            Entry::ThinkingLevelChange {
                thinking_level: level,
                ..
            } => thinking_level = level.clone(),
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(SessionContextModel {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            Entry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                model = Some(SessionContextModel {
                    provider: assistant.provider.clone(),
                    model_id: assistant.model.clone(),
                });
            }
            Entry::ActiveToolsChange {
                active_tool_names: names,
                ..
            } => active_tool_names = Some(names.clone()),
            _ => {}
        }
    }

    SessionContext {
        messages: Vec::new(),
        thinking_level,
        model,
        active_tool_names,
    }
}

/// Port of `defaultContextEntryTransform`: keeps the last compaction entry
/// plus everything after it.
pub fn default_context_entry_transform(path_entries: &[Entry]) -> Vec<Entry> {
    let mut compaction_index: Option<usize> = None;
    for index in (0..path_entries.len()).rev() {
        if path_entries[index].entry_type() == EntryType::Compaction {
            compaction_index = Some(index);
            break;
        }
    }
    match compaction_index {
        None => path_entries.to_vec(),
        Some(index) => {
            let mut entries = vec![path_entries[index].clone()];
            entries.extend(path_entries[index + 1..].to_vec());
            entries
        }
    }
}

/// Port of `buildContextEntries`.
pub fn build_context_entries(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<Entry> {
    let mut entries = default_context_entry_transform(path_entries);
    for transform in &options.entry_transforms {
        entries = transform(&entries);
    }
    entries
}

/// Port of `sessionEntryToContextMessages`.
pub fn session_entry_to_context_messages(
    entry: &Entry,
    index: usize,
    entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => {
            if let AgentMessage::Assistant(assistant) = message
                && assistant.stop_reason == crate::ai::types::StopReason::Deferred
            {
                return Vec::new();
            }
            vec![message.clone()]
        }
        Entry::Compaction {
            summary,
            retained_tail,
            tokens_before,
            timestamp,
            ..
        } => {
            let mut messages = vec![create_compaction_summary_message(
                summary.clone(),
                *tokens_before,
                *timestamp,
            )];
            messages.extend(retained_tail.iter().cloned());
            messages
        }
        Entry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } if !summary.is_empty() => {
            vec![create_branch_summary_message(
                summary.clone(),
                from_id.clone(),
                *timestamp,
            )]
        }
        Entry::Custom { custom_type, .. } => options
            .entry_projectors
            .get(custom_type)
            .and_then(|projector| projector(entry, index, entries))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Port of `buildSessionContext`.
pub fn build_session_context(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> SessionContext {
    let mut context = derive_session_context_state(path_entries);
    let context_entries = build_context_entries(path_entries, options);
    let mut messages: Vec<AgentMessage> = Vec::new();
    for (index, entry) in context_entries.iter().enumerate() {
        messages.extend(session_entry_to_context_messages(
            entry,
            index,
            &context_entries,
            options,
        ));
    }
    context.messages = messages;
    context
}
