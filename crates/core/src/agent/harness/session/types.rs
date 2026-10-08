//! Port of `pi-core/agent/src/harness/session/types.ts`.

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::agent::types::AgentMessage;
use crate::ai::types::Usage;

/// Port of `SessionStopReason` (`StopReason` excluding `pending`, plus
/// `deferred`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// Port of `CompactionReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// The entry discriminators (`Entry["type"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryType {
    Message,
    ModelChange,
    ThinkingLevelChange,
    ActiveToolsChange,
    Compaction,
    BranchSummary,
    Custom,
}

impl EntryType {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryType::Message => "message",
            EntryType::ModelChange => "model_change",
            EntryType::ThinkingLevelChange => "thinking_level_change",
            EntryType::ActiveToolsChange => "active_tools_change",
            EntryType::Compaction => "compaction",
            EntryType::BranchSummary => "branch_summary",
            EntryType::Custom => "custom",
        }
    }
}

/// Port of `Entry`: the append-only session tree node.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Entry {
    #[serde(rename = "message")]
    Message {
        id: String,
        message: AgentMessage,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        terminate: Option<bool>,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        id: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange {
        id: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "active_tools_change")]
    ActiveToolsChange {
        id: String,
        #[serde(rename = "activeToolNames")]
        active_tool_names: Vec<String>,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: String,
        summary: String,
        #[serde(rename = "retainedTail")]
        retained_tail: Vec<AgentMessage>,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        details: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        usage: Option<Usage>,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        id: String,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        details: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        usage: Option<Usage>,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "custom")]
    Custom {
        id: String,
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        data: Option<serde_json::Value>,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
    },
}

impl Entry {
    /// Construct a provisioned `MessageEntry`. Storage fills the parent,
    /// sequence, and timestamp fields when it accepts the entry.
    pub fn message(id: impl Into<String>, message: AgentMessage) -> Self {
        Self::Message {
            id: id.into(),
            message,
            terminate: None,
            parent_id: None,
            seq: 0,
            timestamp: 0,
        }
    }

    /// The entry discriminator.
    pub fn entry_type(&self) -> EntryType {
        match self {
            Entry::Message { .. } => EntryType::Message,
            Entry::ModelChange { .. } => EntryType::ModelChange,
            Entry::ThinkingLevelChange { .. } => EntryType::ThinkingLevelChange,
            Entry::ActiveToolsChange { .. } => EntryType::ActiveToolsChange,
            Entry::Compaction { .. } => EntryType::Compaction,
            Entry::BranchSummary { .. } => EntryType::BranchSummary,
            Entry::Custom { .. } => EntryType::Custom,
        }
    }

    /// The entry id.
    pub fn id(&self) -> &str {
        match self {
            Entry::Message { id, .. }
            | Entry::ModelChange { id, .. }
            | Entry::ThinkingLevelChange { id, .. }
            | Entry::ActiveToolsChange { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::BranchSummary { id, .. }
            | Entry::Custom { id, .. } => id,
        }
    }

    /// The parent id (the appending lane's leaf at append time).
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Message { parent_id, .. }
            | Entry::ModelChange { parent_id, .. }
            | Entry::ThinkingLevelChange { parent_id, .. }
            | Entry::ActiveToolsChange { parent_id, .. }
            | Entry::Compaction { parent_id, .. }
            | Entry::BranchSummary { parent_id, .. }
            | Entry::Custom { parent_id, .. } => parent_id.as_deref(),
        }
    }

    /// The `terminate` flag of message entries.
    pub fn message_terminate(&self) -> Option<bool> {
        match self {
            Entry::Message { terminate, .. } => *terminate,
            _ => None,
        }
    }

    /// The shared sequence number.
    pub fn seq(&self) -> u64 {
        match self {
            Entry::Message { seq, .. }
            | Entry::ModelChange { seq, .. }
            | Entry::ThinkingLevelChange { seq, .. }
            | Entry::ActiveToolsChange { seq, .. }
            | Entry::Compaction { seq, .. }
            | Entry::BranchSummary { seq, .. }
            | Entry::Custom { seq, .. } => *seq,
        }
    }

    /// The entry timestamp.
    pub fn timestamp(&self) -> i64 {
        match self {
            Entry::Message { timestamp, .. }
            | Entry::ModelChange { timestamp, .. }
            | Entry::ThinkingLevelChange { timestamp, .. }
            | Entry::ActiveToolsChange { timestamp, .. }
            | Entry::Compaction { timestamp, .. }
            | Entry::BranchSummary { timestamp, .. }
            | Entry::Custom { timestamp, .. } => *timestamp,
        }
    }

    /// Assigns the storage-assigned fields (parent, sequence, timestamp),
    /// returning the stored entry.
    pub fn with_storage_fields(self, parent_id: Option<String>, seq: u64, timestamp: i64) -> Entry {
        fn assign(entry: Entry, parent_id: Option<String>, seq: u64, timestamp: i64) -> Entry {
            match entry {
                Entry::Message {
                    id,
                    message,
                    terminate,
                    ..
                } => Entry::Message {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    message,
                    terminate,
                },
                Entry::ModelChange {
                    id,
                    provider,
                    model_id,
                    ..
                } => Entry::ModelChange {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    provider,
                    model_id,
                },
                Entry::ThinkingLevelChange {
                    id, thinking_level, ..
                } => Entry::ThinkingLevelChange {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    thinking_level,
                },
                Entry::ActiveToolsChange {
                    id,
                    active_tool_names,
                    ..
                } => Entry::ActiveToolsChange {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    active_tool_names,
                },
                Entry::Compaction {
                    id,
                    summary,
                    retained_tail,
                    tokens_before,
                    details,
                    usage,
                    ..
                } => Entry::Compaction {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    summary,
                    retained_tail,
                    tokens_before,
                    details,
                    usage,
                },
                Entry::BranchSummary {
                    id,
                    from_id,
                    summary,
                    details,
                    usage,
                    ..
                } => Entry::BranchSummary {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    from_id,
                    summary,
                    details,
                    usage,
                },
                Entry::Custom {
                    id,
                    custom_type,
                    data,
                    ..
                } => Entry::Custom {
                    id,
                    seq,
                    parent_id,
                    timestamp,
                    custom_type,
                    data,
                },
            }
        }
        assign(self, parent_id, seq, timestamp)
    }
}

pub type EntryBase = Entry;
pub type MessageEntry = Entry;
pub type ModelChangeEntry = Entry;
pub type ThinkingLevelEntry = Entry;
pub type ActiveToolsEntry = Entry;
pub type CompactionEntry = Entry;
pub type BranchSummaryEntry = Entry;
pub type CustomEntry = Entry;
pub type ProvisionedEntry = Entry;

// ---------------------------------------------------------------------------
// Lane records
// ---------------------------------------------------------------------------

/// The record discriminators (`LaneRecord["type"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordType {
    OperationStarted,
    AbortRequested,
    OperationFinished,
    StepAttempt,
    ToolStarted,
    QueueEnqueued,
    QueueCancelled,
    WriteDeferred,
    Usage,
}

impl RecordType {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordType::OperationStarted => "operation_started",
            RecordType::AbortRequested => "abort_requested",
            RecordType::OperationFinished => "operation_finished",
            RecordType::StepAttempt => "step_attempt",
            RecordType::ToolStarted => "tool_started",
            RecordType::QueueEnqueued => "queue_enqueued",
            RecordType::QueueCancelled => "queue_cancelled",
            RecordType::WriteDeferred => "write_deferred",
            RecordType::Usage => "usage",
        }
    }
}

/// Port of `OperationStartedRecord["intent"]`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum OperationIntent {
    #[serde(rename = "run")]
    Run {
        #[serde(rename = "originalPrompt")]
        original_prompt: Vec<AgentMessage>,
        #[serde(rename = "initialMessages")]
        initial_messages: Vec<Entry>,
        #[serde(
            rename = "systemPromptOverride",
            skip_serializing_if = "Option::is_none",
            default
        )]
        system_prompt_override: Option<String>,
        #[serde(
            rename = "resumeData",
            skip_serializing_if = "Option::is_none",
            default
        )]
        resume_data: Option<serde_json::Value>,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(
            rename = "customInstructions",
            skip_serializing_if = "Option::is_none",
            default
        )]
        custom_instructions: Option<String>,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
    },
    #[serde(rename = "navigation")]
    Navigation {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
        summarize: bool,
        #[serde(
            rename = "customInstructions",
            skip_serializing_if = "Option::is_none",
            default
        )]
        custom_instructions: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        label: Option<String>,
        #[serde(
            rename = "summaryEntryId",
            skip_serializing_if = "Option::is_none",
            default
        )]
        summary_entry_id: Option<String>,
    },
}

/// The usage-record cause payloads.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cause", rename_all = "camelCase")]
pub enum UsageCause {
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        attempt: u32,
        #[serde(rename = "stopReason")]
        stop_reason: String,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        attempt: u32,
        #[serde(rename = "stopReason")]
        stop_reason: String,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        attempt: u32,
        #[serde(rename = "stopReason")]
        stop_reason: String,
    },
    #[serde(rename = "deferred_fetch")]
    DeferredFetch {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        attempt: u32,
        #[serde(rename = "stopReason")]
        stop_reason: String,
    },
    #[serde(rename = "tool")]
    Tool {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
    },
    #[serde(rename = "hook")]
    Hook {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    #[serde(rename = "adjustment")]
    Adjustment {
        #[serde(rename = "runId", skip_serializing_if = "Option::is_none", default)]
        run_id: Option<String>,
        #[serde(rename = "entryId", skip_serializing_if = "Option::is_none", default)]
        entry_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        details: Option<serde_json::Value>,
    },
}

/// The error payload of `operation_finished`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecordError {
    pub code: String,
    pub message: String,
}

/// Port of `LaneRecord`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaneRecord {
    #[serde(rename = "operation_started")]
    OperationStarted {
        id: String,
        lane: String,
        #[serde(rename = "sourceLeafId")]
        source_leaf_id: Option<String>,
        intent: OperationIntent,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "abort_requested")]
    AbortRequested {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "operation_finished")]
    OperationFinished {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        error: Option<RecordError>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "step_attempt")]
    StepAttempt {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        step: String,
        attempt: u32,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        #[serde(
            rename = "compactionReason",
            skip_serializing_if = "Option::is_none",
            default
        )]
        compaction_reason: Option<String>,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "tool_started")]
    ToolStarted {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "assistantEntryId")]
        assistant_entry_id: String,
        #[serde(rename = "toolIndex")]
        tool_index: u32,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "effectiveArgs")]
        effective_args: serde_json::Value,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        replay: String,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "queue_enqueued")]
    QueueEnqueued {
        id: String,
        lane: String,
        queue: String,
        #[serde(rename = "runId", skip_serializing_if = "Option::is_none", default)]
        run_id: Option<String>,
        target: Entry,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "queue_cancelled")]
    QueueCancelled {
        id: String,
        lane: String,
        #[serde(rename = "runId", skip_serializing_if = "Option::is_none", default)]
        run_id: Option<String>,
        #[serde(rename = "entryId")]
        entry_id: String,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "write_deferred")]
    WriteDeferred {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        target: Entry,
        seq: u64,
        timestamp: i64,
    },
    #[serde(rename = "usage")]
    Usage {
        id: String,
        lane: String,
        usage: Usage,
        #[serde(flatten)]
        cause: UsageCause,
        seq: u64,
        timestamp: i64,
    },
}

impl LaneRecord {
    /// The record discriminator.
    pub fn record_type(&self) -> RecordType {
        match self {
            LaneRecord::OperationStarted { .. } => RecordType::OperationStarted,
            LaneRecord::AbortRequested { .. } => RecordType::AbortRequested,
            LaneRecord::OperationFinished { .. } => RecordType::OperationFinished,
            LaneRecord::StepAttempt { .. } => RecordType::StepAttempt,
            LaneRecord::ToolStarted { .. } => RecordType::ToolStarted,
            LaneRecord::QueueEnqueued { .. } => RecordType::QueueEnqueued,
            LaneRecord::QueueCancelled { .. } => RecordType::QueueCancelled,
            LaneRecord::WriteDeferred { .. } => RecordType::WriteDeferred,
            LaneRecord::Usage { .. } => RecordType::Usage,
        }
    }

    /// The record id.
    pub fn id(&self) -> &str {
        match self {
            LaneRecord::OperationStarted { id, .. }
            | LaneRecord::AbortRequested { id, .. }
            | LaneRecord::OperationFinished { id, .. }
            | LaneRecord::StepAttempt { id, .. }
            | LaneRecord::ToolStarted { id, .. }
            | LaneRecord::QueueEnqueued { id, .. }
            | LaneRecord::QueueCancelled { id, .. }
            | LaneRecord::WriteDeferred { id, .. }
            | LaneRecord::Usage { id, .. } => id,
        }
    }

    /// The record sequence.
    pub fn seq(&self) -> u64 {
        match self {
            LaneRecord::OperationStarted { seq, .. }
            | LaneRecord::AbortRequested { seq, .. }
            | LaneRecord::OperationFinished { seq, .. }
            | LaneRecord::StepAttempt { seq, .. }
            | LaneRecord::ToolStarted { seq, .. }
            | LaneRecord::QueueEnqueued { seq, .. }
            | LaneRecord::QueueCancelled { seq, .. }
            | LaneRecord::WriteDeferred { seq, .. }
            | LaneRecord::Usage { seq, .. } => *seq,
        }
    }

    /// The owning lane.
    pub fn lane(&self) -> &str {
        match self {
            LaneRecord::OperationStarted { lane, .. }
            | LaneRecord::AbortRequested { lane, .. }
            | LaneRecord::OperationFinished { lane, .. }
            | LaneRecord::StepAttempt { lane, .. }
            | LaneRecord::ToolStarted { lane, .. }
            | LaneRecord::QueueEnqueued { lane, .. }
            | LaneRecord::QueueCancelled { lane, .. }
            | LaneRecord::WriteDeferred { lane, .. }
            | LaneRecord::Usage { lane, .. } => lane,
        }
    }

    /// The record timestamp.
    pub fn timestamp(&self) -> i64 {
        match self {
            LaneRecord::OperationStarted { timestamp, .. }
            | LaneRecord::AbortRequested { timestamp, .. }
            | LaneRecord::OperationFinished { timestamp, .. }
            | LaneRecord::StepAttempt { timestamp, .. }
            | LaneRecord::ToolStarted { timestamp, .. }
            | LaneRecord::QueueEnqueued { timestamp, .. }
            | LaneRecord::QueueCancelled { timestamp, .. }
            | LaneRecord::WriteDeferred { timestamp, .. }
            | LaneRecord::Usage { timestamp, .. } => *timestamp,
        }
    }

    /// The operation identity, when the record has one.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            LaneRecord::OperationStarted { id, .. } => Some(id),
            LaneRecord::AbortRequested { run_id, .. }
            | LaneRecord::OperationFinished { run_id, .. }
            | LaneRecord::StepAttempt { run_id, .. }
            | LaneRecord::ToolStarted { run_id, .. }
            | LaneRecord::WriteDeferred { run_id, .. } => Some(run_id),
            LaneRecord::QueueEnqueued { run_id, queue, .. }
                if queue == "steer" || queue == "followUp" =>
            {
                run_id.as_deref()
            }
            LaneRecord::QueueEnqueued { .. } | LaneRecord::QueueCancelled { .. } => None,
            LaneRecord::Usage { cause, .. } => match cause {
                UsageCause::Adjustment { run_id, .. } => run_id.as_deref(),
                _ => cause_run_id(cause),
            },
        }
    }
}

pub type RecordBase = LaneRecord;
pub type OperationStartedRecord = LaneRecord;
pub type AbortRequestedRecord = LaneRecord;
pub type OperationFinishedRecord = LaneRecord;
pub type StepAttemptRecord = LaneRecord;
pub type ToolStartedRecord = LaneRecord;
pub type QueueEnqueuedRecord = LaneRecord;
pub type QueueCancelledRecord = LaneRecord;
pub type WriteDeferredRecord = LaneRecord;
pub type UsageRecord = LaneRecord;
pub type NewRecord = LaneRecord;

fn cause_run_id(cause: &UsageCause) -> Option<&str> {
    match cause {
        UsageCause::Assistant { run_id, .. }
        | UsageCause::Compaction { run_id, .. }
        | UsageCause::BranchSummary { run_id, .. }
        | UsageCause::DeferredFetch { run_id, .. }
        | UsageCause::Tool { run_id, .. }
        | UsageCause::Hook { run_id, .. } => Some(run_id),
        UsageCause::Adjustment { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Queries, metadata, errors
// ---------------------------------------------------------------------------

/// Port of `EntryOrder`. Defaults to newest-first, matching the TypeScript
/// `EntryQuery.order` default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryOrder {
    NewestFirst,
    #[default]
    OldestFirst,
}

/// Port of `EntryCursor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryCursor {
    pub after_seq: u64,
}

/// Port of `EntryQuery`.
#[derive(Clone, Debug, Default)]
pub struct EntryQuery {
    pub entry_type: Option<EntryType>,
    pub custom_type: Option<String>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
    pub cursor: Option<EntryCursor>,
}

/// Port of `BranchBounds`.
#[derive(Clone, Debug, Default)]
pub struct BranchBounds {
    pub start: Option<String>,
    pub stop_at_type: Option<EntryType>,
    pub stop_at_id: Option<String>,
}

/// Port of `RecordQuery`.
#[derive(Clone, Debug, Default)]
pub struct RecordQuery {
    pub lane: Option<String>,
    pub record_type: Option<RecordType>,
    pub run_id: Option<String>,
    pub operation_kind: Option<String>,
    pub after_seq: Option<u64>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
}

/// Port of `SessionMetadata`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(
        rename = "parentSessionId",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub parent_session_id: Option<String>,
}

/// Port of `SessionStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub message_count: u64,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub total_tokens: u64,
    pub cost_total: f64,
}

/// Port of `LanePointer`.
#[derive(Clone, Debug, PartialEq)]
pub struct LanePointer {
    pub lane: String,
    pub leaf_id: Option<String>,
}

/// Port of `LogItem`.
#[derive(Clone, Debug, PartialEq)]
pub enum LogItem {
    Entry {
        seq: u64,
        entry: Entry,
    },
    Record {
        seq: u64,
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    NameFact {
        seq: u64,
        name: Option<String>,
    },
    LabelFact {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

/// Port of `LogOptions`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogOptions {
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

/// Port of `SessionError`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

/// Port of `SessionErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionErrorCode {
    NotFound,
    AlreadyExists,
    InvalidEntry,
    InvalidPayload,
    InvalidLane,
    InvalidQuery,
    InvalidForkTarget,
    Storage,
}

impl SessionError {
    pub fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for SessionError {}

/// Port of `SessionStorage`. All methods are infallible in the type
/// system; failures return [`SessionError`] through the result.
pub trait SessionStorage: Send + Sync {
    fn get_metadata(&self) -> BoxFuture<'static, SessionMetadata>;

    fn get_lanes(&self) -> BoxFuture<'static, Vec<LanePointer>>;

    fn create_lane(
        &self,
        lane: String,
        at: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>>;

    fn move_lane(
        &self,
        lane: String,
        to: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>>;

    fn append_entry(
        &self,
        entry: Entry,
        lane: String,
    ) -> BoxFuture<'static, Result<Entry, SessionError>>;

    fn append_record(
        &self,
        record: LaneRecord,
    ) -> BoxFuture<'static, Result<LaneRecord, SessionError>>;

    fn get_entry(&self, id: String) -> BoxFuture<'static, Option<Entry>>;

    fn find_entries(&self, query: EntryQuery) -> BoxFuture<'static, Vec<Entry>>;

    fn find_entries_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
        start: String,
    ) -> BoxFuture<'static, Vec<Entry>>;

    fn find_records(&self, query: RecordQuery) -> BoxFuture<'static, Vec<LaneRecord>>;

    fn find_open_operations(
        &self,
        lane: String,
        limit: Option<usize>,
    ) -> BoxFuture<'static, Vec<LaneRecord>>;

    fn get_log(&self, options: LogOptions) -> BoxFuture<'static, Vec<LogItem>>;

    fn get_name(&self) -> BoxFuture<'static, Option<String>>;

    fn set_name(&self, name: Option<String>) -> BoxFuture<'static, Result<(), SessionError>>;

    fn get_label(&self, id: String) -> BoxFuture<'static, Option<String>>;

    fn set_label(
        &self,
        id: String,
        label: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>>;

    fn get_stats(&self) -> BoxFuture<'static, SessionStats>;

    /// Unused abort-signal hook kept for signature parity with future
    /// storages; the in-memory and JSONL backends ignore it.
    fn signal(&self) -> Option<CancellationToken> {
        None
    }
}

/// Port of `SessionCreateOptions`.
#[derive(Clone, Debug, Default)]
pub struct SessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
}

/// Port of `ForkOptions`.
#[derive(Clone, Debug)]
pub enum ForkOptions {
    Branch {
        entry_id: Option<String>,
        position: Option<ForkPosition>,
    },
    Tree,
}

/// Port of the fork position values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}
