//! Port of `pi-core/agent/src/harness/reducer.ts`.

use std::collections::{HashMap, HashSet};

use crate::agent::types::AgentMessage;
use crate::ai::types::ToolCall;
use crate::ai::types::{DeferredHandle, StopReason};

use super::session::types::{Entry, EntryType, LaneRecord, OperationIntent, UsageCause};

/// Port of `RecordLogCorruptionReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordLogCorruptionReason {
    MultipleOpenOperations,
    UnknownOperation,
    RecordAfterFinish,
    NonConsecutiveAttempt,
    InvalidCompactionReason,
    QueueAfterAbort,
    InvalidQueueCancellation,
    InconsistentStep,
    ToolCallMismatch,
    DuplicateToolInvocation,
    ProvisionedEntryMismatch,
    InvalidDeferredHandle,
}

impl RecordLogCorruptionReason {
    /// The machine-readable TypeScript discriminant.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordLogCorruptionReason::MultipleOpenOperations => "multiple_open_operations",
            RecordLogCorruptionReason::UnknownOperation => "unknown_operation",
            RecordLogCorruptionReason::RecordAfterFinish => "record_after_finish",
            RecordLogCorruptionReason::NonConsecutiveAttempt => "non_consecutive_attempt",
            RecordLogCorruptionReason::InvalidCompactionReason => "invalid_compaction_reason",
            RecordLogCorruptionReason::QueueAfterAbort => "queue_after_abort",
            RecordLogCorruptionReason::InvalidQueueCancellation => "invalid_queue_cancellation",
            RecordLogCorruptionReason::InconsistentStep => "inconsistent_step",
            RecordLogCorruptionReason::ToolCallMismatch => "tool_call_mismatch",
            RecordLogCorruptionReason::DuplicateToolInvocation => "duplicate_tool_invocation",
            RecordLogCorruptionReason::ProvisionedEntryMismatch => "provisioned_entry_mismatch",
            RecordLogCorruptionReason::InvalidDeferredHandle => "invalid_deferred_handle",
        }
    }
}

/// Port of `RecordLogCorruption`.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordLogCorruption {
    pub reason: RecordLogCorruptionReason,
    pub message: String,
}

impl std::fmt::Display for RecordLogCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RecordLogCorruption {}

/// Port of `RecordLogSlice`.
pub struct RecordLogSlice {
    pub lane: String,
    pub open_operations: Vec<LaneRecord>,
    pub records: Vec<LaneRecord>,
    /// Operation-owned entries plus entries fetched directly by
    /// provisioned or referenced ids.
    pub entries: Vec<Entry>,
}

/// Port of `EffectiveLaneConfiguration`.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveLaneConfiguration {
    pub model: EffectiveModel,
    pub thinking_level: String,
    pub active_tool_names: Vec<String>,
}

/// The provider/model pointer inside the effective configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveModel {
    pub provider: String,
    pub model_id: String,
}

/// Port of `TerminalFailureState`.
#[derive(Clone, Debug)]
pub struct TerminalFailureState {
    pub entry_id: String,
    /// `"step"` or `"deferred_fetch"`.
    pub source: &'static str,
    pub message: crate::ai::types::AssistantMessage,
}

/// Port of `ToolBatchState`.
#[derive(Clone, Debug)]
pub struct ToolBatchState {
    pub assistant_entry_id: String,
    pub calls: Vec<ToolCallState>,
    pub truncated: bool,
    pub unresolved: bool,
}

/// One tool call inside a [`ToolBatchState`].
#[derive(Clone, Debug)]
pub struct ToolCallState {
    pub tool_index: usize,
    pub tool_call: ToolCall,
    pub started: Option<LaneRecord>,
    pub result_exists: bool,
    pub terminate: bool,
}

/// Port of `LaneState`.
pub struct LaneState {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub operation: Option<OperationState>,
    pub pending_next_run: Vec<Entry>,
}

/// The open-operation slice of [`LaneState`].
pub struct OperationState {
    pub id: String,
    /// `"run"`, `"compaction"`, or `"navigation"`.
    pub kind: &'static str,
    pub intent: OperationIntent,
    pub aborting: bool,
    pub step: Option<StepState>,
    pub tool_batch: Option<ToolBatchState>,
    pub missing_initial_messages: Vec<Entry>,
    pub pending_steer: Vec<Entry>,
    pub pending_follow_up: Vec<Entry>,
    pub pending_writes: Vec<Entry>,
    pub deferred: Option<DeferredHandle>,
    pub overflow_recovery_used: bool,
    pub newest_own: Option<NewestOwn>,
    pub targets: Targets,
}

/// The in-flight step of an operation.
pub struct StepState {
    /// `"assistant"`, `"compaction"`, or `"branch_summary"`.
    pub kind: &'static str,
    pub attempts: u32,
    pub result_entry_id: String,
    pub compaction_reason: Option<String>,
}

/// Port of `newestOwn`.
pub struct NewestOwn {
    pub entry_id: String,
    pub entry_type: EntryType,
    pub role: Option<String>,
    pub stop_reason: Option<StopReason>,
}

/// Port of `targets`.
#[derive(Default)]
pub struct Targets {
    pub result: bool,
    pub summary: bool,
}

/// Port of `LaneReductionInput`.
pub struct LaneReductionInput {
    pub lane: String,
    pub open_operations: Vec<LaneRecord>,
    pub records: Vec<LaneRecord>,
    pub entries: Vec<Entry>,
    pub leaf_id: Option<String>,
    /// Entries appended by the open operation, oldest first.
    pub own_entries: Vec<Entry>,
    /// Effective-state lookups at the operation anchor or idle leaf.
    pub configuration_entries: Vec<Entry>,
    pub defaults: EffectiveLaneConfiguration,
}

/// Port of `LaneReductionResult`.
pub struct LaneReductionResult {
    pub lane_state: LaneState,
    pub effective_configuration: EffectiveLaneConfiguration,
    pub terminal_failure: Option<TerminalFailureState>,
}

struct AttemptSeries {
    record_step: &'static str,
    attempt: u32,
    result_entry_id: String,
    compaction_reason: Option<String>,
}

/// Flattened `StepAttempt` fields used across the validators.
pub type AttemptFields<'a> = (&'static str, &'a str, u32, &'a str, Option<&'a str>, u64);

fn corrupt(reason: RecordLogCorruptionReason, message: impl Into<String>) -> RecordLogCorruption {
    RecordLogCorruption {
        reason,
        message: message.into(),
    }
}

/// Compares a stored entry against its provisioned intent, ignoring the
/// storage-assigned fields (deep-equality on the serialized payload).
fn matches_provisioned_entry(entry: &Entry, target: &Entry) -> bool {
    let strip = |entry: &Entry| {
        let mut value = serde_json::to_value(entry).unwrap_or_default();
        if let Some(object) = value.as_object_mut() {
            object.remove("parentId");
            object.remove("seq");
            object.remove("timestamp");
        }
        value
    };
    strip(entry) == strip(target)
}

fn validate_exact_provisioned_entry(
    entries_by_id: &HashMap<String, Entry>,
    target: &Entry,
) -> Result<(), RecordLogCorruption> {
    if let Some(entry) = entries_by_id.get(target.id())
        && !matches_provisioned_entry(entry, target)
    {
        return Err(corrupt(
            RecordLogCorruptionReason::ProvisionedEntryMismatch,
            format!(
                "Provisioned entry {} exists with content different from its intent",
                target.id()
            ),
        ));
    }
    Ok(())
}

fn validate_result_entry(
    entries_by_id: &HashMap<String, &Entry>,
    result_entry_id: &str,
    matches: impl Fn(&Entry) -> bool,
    description: &str,
) -> Result<(), RecordLogCorruption> {
    if let Some(entry) = entries_by_id.get(result_entry_id)
        && !matches(entry)
    {
        return Err(corrupt(
            RecordLogCorruptionReason::ProvisionedEntryMismatch,
            format!(
                "Provisioned {description} entry {result_entry_id} exists with different content"
            ),
        ));
    }
    Ok(())
}

fn step_kind(step: &str) -> &'static str {
    match step {
        "assistant" => "assistant",
        "compaction" => "compaction",
        _ => "branch_summary",
    }
}

fn attempt_fields(record: &LaneRecord) -> Option<AttemptFields<'_>> {
    if let LaneRecord::StepAttempt {
        step,
        id,
        attempt,
        result_entry_id,
        compaction_reason,
        seq,
        ..
    } = record
    {
        Some((
            step_kind(step),
            id,
            *attempt,
            result_entry_id,
            compaction_reason.as_deref(),
            *seq,
        ))
    } else {
        None
    }
}

fn validate_attempt_reason(record: &LaneRecord) -> Result<(), RecordLogCorruption> {
    let Some((kind, id, _, _, reason, _)) = attempt_fields(record) else {
        return Ok(());
    };
    let reason = reason.map(str::to_string);
    match (kind, reason) {
        ("compaction", Some(reason))
            if matches!(reason.as_str(), "manual" | "threshold" | "overflow") =>
        {
            Ok(())
        }
        ("compaction", _) => Err(corrupt(
            RecordLogCorruptionReason::InvalidCompactionReason,
            format!("Compaction attempt {id} has no valid compaction reason"),
        )),
        (_, Some(_)) => Err(corrupt(
            RecordLogCorruptionReason::InvalidCompactionReason,
            format!("{kind} attempt {id} has a compaction reason"),
        )),
        (_, None) => Ok(()),
    }
}

fn validate_attempt_sequence(
    record: &LaneRecord,
    previous: Option<&AttemptSeries>,
    entries_by_id: &HashMap<String, Entry>,
) -> Result<(), RecordLogCorruption> {
    let Some((kind, id, attempt, result_entry_id, reason, seq)) = attempt_fields(record) else {
        return Ok(());
    };
    let reason = reason.map(str::to_string);
    let previous_result =
        previous.and_then(|previous| entries_by_id.get(&previous.result_entry_id));
    let continues_series = match previous {
        Some(previous) => {
            previous.record_step == kind && previous_result.is_none_or(|result| result.seq() >= seq)
        }
        None => false,
    };
    let expected_attempt = if continues_series {
        previous.map(|previous| previous.attempt + 1).unwrap_or(1)
    } else {
        1
    };
    if attempt != expected_attempt {
        return Err(corrupt(
            RecordLogCorruptionReason::NonConsecutiveAttempt,
            format!("{kind} attempt {id} is {attempt}; expected {expected_attempt}"),
        ));
    }
    if !continues_series || kind == "assistant" {
        return Ok(());
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    if result_entry_id != previous.result_entry_id {
        return Err(corrupt(
            RecordLogCorruptionReason::InconsistentStep,
            format!("{kind} attempts disagree on their result entry id"),
        ));
    }
    if reason != previous.compaction_reason {
        return Err(corrupt(
            RecordLogCorruptionReason::InconsistentStep,
            format!("{kind} attempts disagree on their compaction reason"),
        ));
    }
    Ok(())
}

fn entry_is_message_with_role(entry: &Entry, role: &str) -> bool {
    matches!(entry, Entry::Message { message, .. } if message.role() == role)
}

fn validate_attempt_result(
    entries_by_id: &HashMap<String, Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogCorruption> {
    let lookup: HashMap<String, &Entry> = entries_by_id
        .iter()
        .map(|(id, entry)| (id.clone(), entry))
        .collect();
    let Some((kind, _, _, result_entry_id, _, _)) = attempt_fields(record) else {
        return Ok(());
    };
    match kind {
        "assistant" => validate_result_entry(
            &lookup,
            result_entry_id,
            |entry| entry_is_message_with_role(entry, "assistant"),
            "assistant result",
        ),
        "compaction" => validate_result_entry(
            &lookup,
            result_entry_id,
            |entry| entry.entry_type() == EntryType::Compaction,
            "compaction result",
        ),
        _ => validate_result_entry(
            &lookup,
            result_entry_id,
            |entry| entry.entry_type() == EntryType::BranchSummary,
            "branch-summary result",
        ),
    }
}

fn validate_tool_start(
    record: &LaneRecord,
    entries_by_id: &HashMap<String, Entry>,
    invocations: &mut HashSet<String>,
) -> Result<(), RecordLogCorruption> {
    let LaneRecord::ToolStarted {
        id,
        assistant_entry_id,
        tool_index,
        tool_call_id,
        tool_name,
        result_entry_id,
        ..
    } = record
    else {
        return Ok(());
    };
    let invocation = format!("{assistant_entry_id}\0{tool_index}");
    if invocations.contains(&invocation) {
        return Err(corrupt(
            RecordLogCorruptionReason::DuplicateToolInvocation,
            format!("Tool invocation {assistant_entry_id}:{tool_index} is duplicated"),
        ));
    }
    invocations.insert(invocation);

    let assistant_entry = entries_by_id.get(assistant_entry_id).ok_or_else(|| {
        corrupt(
            RecordLogCorruptionReason::ToolCallMismatch,
            format!("Tool start {id} does not reference an assistant entry"),
        )
    })?;
    let tool_calls: Vec<&ToolCall> = match assistant_entry {
        Entry::Message {
            message: AgentMessage::Assistant(assistant),
            ..
        } => assistant
            .content
            .iter()
            .filter_map(|content| match content {
                crate::ai::types::AssistantContent::ToolCall(tool_call) => Some(tool_call),
                _ => None,
            })
            .collect(),
        _ => {
            return Err(corrupt(
                RecordLogCorruptionReason::ToolCallMismatch,
                format!("Tool start {id} does not reference an assistant entry"),
            ));
        }
    };
    let Some(tool_call) = tool_calls.get(*tool_index as usize) else {
        return Err(corrupt(
            RecordLogCorruptionReason::ToolCallMismatch,
            format!("Tool start {id} does not match its assistant tool-call ordinal"),
        ));
    };
    if tool_call.id != *tool_call_id || tool_call.name != *tool_name {
        return Err(corrupt(
            RecordLogCorruptionReason::ToolCallMismatch,
            format!("Tool start {id} does not match its assistant tool-call ordinal"),
        ));
    }

    let lookup: HashMap<String, &Entry> = entries_by_id
        .iter()
        .map(|(id, entry)| (id.clone(), entry))
        .collect();
    validate_result_entry(
        &lookup,
        result_entry_id,
        |entry| match entry {
            Entry::Message {
                message: AgentMessage::ToolResult(tool_result),
                ..
            } => tool_result.tool_call_id == *tool_call_id && tool_result.tool_name == *tool_name,
            _ => false,
        },
        "tool result",
    )
}

fn validate_deferred_handles(entries: &[Entry]) -> Result<(), RecordLogCorruption> {
    for entry in entries {
        if let Entry::Message {
            message: AgentMessage::Assistant(assistant),
            id,
            ..
        } = entry
            && assistant.stop_reason == StopReason::Deferred
            && assistant.deferred.is_none()
        {
            return Err(corrupt(
                RecordLogCorruptionReason::InvalidDeferredHandle,
                format!("Deferred assistant entry {id} does not carry a handle"),
            ));
        }
    }
    Ok(())
}

fn validate_operation_result(
    entries_by_id: &HashMap<String, Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogCorruption> {
    let LaneRecord::OperationStarted { intent, .. } = record else {
        return Ok(());
    };
    match intent {
        OperationIntent::Run {
            initial_messages, ..
        } => {
            // The intent carries provisioned entries; the Rust slice models
            // them as full entries with placeholder storage fields.
            for target in initial_messages {
                validate_exact_provisioned_entry(entries_by_id, target)?;
            }
            Ok(())
        }
        OperationIntent::Compaction {
            result_entry_id, ..
        } => {
            let lookup: HashMap<String, &Entry> = entries_by_id
                .iter()
                .map(|(id, entry)| (id.clone(), entry))
                .collect();
            validate_result_entry(
                &lookup,
                result_entry_id,
                |entry| entry.entry_type() == EntryType::Compaction,
                "manual compaction",
            )
        }
        OperationIntent::Navigation {
            summary_entry_id, ..
        } => {
            let Some(summary_entry_id) = summary_entry_id else {
                return Ok(());
            };
            let lookup: HashMap<String, &Entry> = entries_by_id
                .iter()
                .map(|(id, entry)| (id.clone(), entry))
                .collect();
            validate_result_entry(
                &lookup,
                summary_entry_id,
                |entry| entry.entry_type() == EntryType::BranchSummary,
                "navigation summary",
            )
        }
    }
}

/// Validates a bounded lane recovery slice without reading or mutating
/// session state (port of `validateRecordLog`).
pub fn validate_record_log(input: &RecordLogSlice) -> Result<(), RecordLogCorruption> {
    if input.open_operations.len() > 1 {
        return Err(corrupt(
            RecordLogCorruptionReason::MultipleOpenOperations,
            format!("Lane {} has at least two open operations", input.lane),
        ));
    }

    let entries_by_id: HashMap<String, Entry> = input
        .entries
        .iter()
        .map(|entry| (entry.id().to_string(), entry.clone()))
        .collect();
    validate_deferred_handles(&input.entries)?;
    let mut starts: HashMap<String, LaneRecord> = HashMap::new();
    let mut finished_at: HashMap<String, u64> = HashMap::new();
    let mut aborted_at: HashMap<String, u64> = HashMap::new();
    let mut queue_enqueues: HashMap<String, LaneRecord> = HashMap::new();
    let mut latest_attempt: HashMap<String, AttemptSeries> = HashMap::new();
    let mut tool_invocations: HashSet<String> = HashSet::new();
    let mut records = input.records.clone();
    records.sort_by_key(|record| record.seq());

    for record in records {
        if let LaneRecord::OperationStarted { .. } = &record {
            starts.insert(record.id().to_string(), record.clone());
            validate_operation_result(&entries_by_id, &record)?;
            continue;
        }

        if let Some(run_id) = record.run_id() {
            let run_id = run_id.to_string();
            if !starts.contains_key(&run_id) {
                return Err(corrupt(
                    RecordLogCorruptionReason::UnknownOperation,
                    format!(
                        "Record {} references unknown operation {run_id}",
                        record.id()
                    ),
                ));
            }
            if let Some(finish_seq) = finished_at.get(&run_id)
                && record.seq() > *finish_seq
            {
                return Err(corrupt(
                    RecordLogCorruptionReason::RecordAfterFinish,
                    format!(
                        "Record {} follows the finish of operation {run_id}",
                        record.id()
                    ),
                ));
            }
        }

        match &record {
            LaneRecord::OperationFinished { run_id, seq, .. } => {
                finished_at.insert(run_id.clone(), *seq);
            }
            LaneRecord::AbortRequested { run_id, seq, .. } => {
                aborted_at.insert(run_id.clone(), *seq);
            }
            LaneRecord::StepAttempt { .. } => {
                validate_attempt_reason(&record)?;
                let run_id = record.run_id().unwrap_or_default().to_string();
                validate_attempt_sequence(&record, latest_attempt.get(&run_id), &entries_by_id)?;
                validate_attempt_result(&entries_by_id, &record)?;
                if let Some((kind, _id, attempt, result_entry_id, reason, _seq)) =
                    attempt_fields(&record)
                {
                    latest_attempt.insert(
                        run_id,
                        AttemptSeries {
                            record_step: kind,
                            attempt,
                            result_entry_id: result_entry_id.to_string(),
                            compaction_reason: reason.map(str::to_string),
                        },
                    );
                }
            }
            LaneRecord::ToolStarted { .. } => {
                validate_tool_start(&record, &entries_by_id, &mut tool_invocations)?;
            }
            LaneRecord::QueueEnqueued {
                queue,
                target,
                run_id,
                seq,
                ..
            } => {
                let queue = queue.clone();
                if queue != "nextRun"
                    && let Some(aborted_seq) = aborted_at.get(run_id.as_deref().unwrap_or_default())
                    && *seq > *aborted_seq
                {
                    return Err(corrupt(
                        RecordLogCorruptionReason::QueueAfterAbort,
                        format!("{queue} item {} was enqueued after abort", target.id()),
                    ));
                }
                queue_enqueues.insert(target.id().to_string(), record.clone());
                validate_exact_provisioned_entry(&entries_by_id, target)?;
            }
            LaneRecord::QueueCancelled {
                entry_id,
                run_id,
                seq,
                ..
            } => {
                let enqueue = queue_enqueues.get(entry_id);
                let invalid = match enqueue {
                    None => true,
                    Some(enqueue) => {
                        enqueue.seq() >= *seq
                            || enqueue.run_id().map(str::to_string) != run_id.clone()
                            || entries_by_id.contains_key(entry_id)
                    }
                };
                if invalid {
                    return Err(corrupt(
                        RecordLogCorruptionReason::InvalidQueueCancellation,
                        format!(
                            "Queue cancellation {} has no pending matching enqueue",
                            record.id()
                        ),
                    ));
                }
            }
            LaneRecord::WriteDeferred { target, .. } => {
                validate_exact_provisioned_entry(&entries_by_id, target)?;
            }
            LaneRecord::Usage { .. } => {}
            LaneRecord::OperationStarted { .. } => unreachable!("handled above"),
        }
    }
    Ok(())
}

fn by_sequence_entries(entries: &[Entry]) -> Vec<Entry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|entry| entry.seq());
    sorted
}

fn by_sequence_records(records: &[LaneRecord]) -> Vec<LaneRecord> {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|record| record.seq());
    sorted
}

fn derive_effective_configuration(input: &LaneReductionInput) -> EffectiveLaneConfiguration {
    let mut configuration = input.defaults.clone();
    let mut entries_by_id: HashMap<String, Entry> = HashMap::new();
    for entry in input
        .configuration_entries
        .iter()
        .chain(input.own_entries.iter())
    {
        entries_by_id.insert(entry.id().to_string(), entry.clone());
    }

    let mut ordered: Vec<Entry> = entries_by_id.values().cloned().collect();
    ordered.sort_by_key(|entry| entry.seq());
    for entry in ordered {
        match entry {
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                configuration.model = EffectiveModel { provider, model_id };
            }
            Entry::ThinkingLevelChange { thinking_level, .. } => {
                configuration.thinking_level = thinking_level;
            }
            Entry::ActiveToolsChange {
                active_tool_names, ..
            } => {
                configuration.active_tool_names = active_tool_names;
            }
            Entry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                configuration.model = EffectiveModel {
                    provider: assistant.provider,
                    model_id: assistant.model,
                };
            }
            _ => {}
        }
    }
    configuration
}

fn derive_newest_own(entry: Option<&Entry>) -> Option<NewestOwn> {
    let entry = entry?;
    match entry {
        Entry::Message { message, id, .. } => {
            let role = message.role().to_string();
            if let AgentMessage::Assistant(assistant) = message {
                Some(NewestOwn {
                    entry_id: id.clone(),
                    entry_type: EntryType::Message,
                    role: Some(role),
                    stop_reason: Some(assistant.stop_reason),
                })
            } else {
                Some(NewestOwn {
                    entry_id: id.clone(),
                    entry_type: EntryType::Message,
                    role: Some(role),
                    stop_reason: None,
                })
            }
        }
        other => Some(NewestOwn {
            entry_id: other.id().to_string(),
            entry_type: other.entry_type(),
            role: None,
            stop_reason: None,
        }),
    }
}

fn derive_tool_batch(
    operation_id: &str,
    records: &[LaneRecord],
    own_entries: &[Entry],
    entries_by_id: &HashMap<String, Entry>,
    deferred_write_ids: &HashSet<String>,
) -> Option<ToolBatchState> {
    let assistant_entry = own_entries.iter().rev().find(|entry| {
        matches!(
            entry,
            Entry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } if assistant.content.iter().any(|content| {
                matches!(content, crate::ai::types::AssistantContent::ToolCall(_))
            })
        )
    })?;
    let assistant = match assistant_entry {
        Entry::Message {
            message: AgentMessage::Assistant(assistant),
            ..
        } => assistant,
        _ => return None,
    };

    let tool_calls: Vec<&ToolCall> = assistant
        .content
        .iter()
        .filter_map(|content| match content {
            crate::ai::types::AssistantContent::ToolCall(tool_call) => Some(tool_call),
            _ => None,
        })
        .collect();
    let mut starts: HashMap<u32, LaneRecord> = HashMap::new();
    for record in records {
        if let LaneRecord::ToolStarted {
            run_id,
            assistant_entry_id,
            tool_index,
            ..
        } = record
            && run_id == operation_id
            && assistant_entry_id == assistant_entry.id()
        {
            starts.insert(*tool_index, record.clone());
        }
    }

    let mut calls: Vec<ToolCallState> = Vec::new();
    for (tool_index, tool_call) in tool_calls.iter().enumerate() {
        let tool_index = tool_index as u32;
        let started = starts.get(&tool_index);
        let started_result =
            started.and_then(|started| entries_by_id.get(&record_result_entry_id(started)));
        let blocked_result = own_entries.iter().find(|entry| {
            entry.seq() > assistant_entry.seq()
                && !deferred_write_ids.contains(entry.id())
                && matches!(
                    entry,
                    Entry::Message {
                        message: AgentMessage::ToolResult(tool_result),
                        ..
                    } if tool_result.tool_call_id == tool_call.id
                )
        });
        let result = started_result.or(blocked_result);
        let terminate = result.is_some_and(|result| {
            matches!(
                result,
                Entry::Message {
                    terminate: Some(true),
                    ..
                }
            )
        });
        calls.push(ToolCallState {
            tool_index: tool_index as usize,
            tool_call: (*tool_call).clone(),
            started: started.cloned(),
            result_exists: result.is_some(),
            terminate,
        });
    }

    let unresolved = calls.iter().any(|call| !call.result_exists);
    Some(ToolBatchState {
        assistant_entry_id: assistant_entry.id().to_string(),
        calls,
        truncated: assistant.stop_reason == StopReason::Length,
        unresolved,
    })
}

fn record_result_entry_id(record: &LaneRecord) -> String {
    match record {
        LaneRecord::ToolStarted {
            result_entry_id, ..
        } => result_entry_id.clone(),
        _ => String::new(),
    }
}

/// Purely reconstructs one lane's orchestration state from its bounded
/// recovery inputs (port of `reduceLaneState`).
pub fn reduce_lane_state(
    input: LaneReductionInput,
) -> Result<LaneReductionResult, RecordLogCorruption> {
    validate_record_log(&RecordLogSlice {
        lane: input.lane.clone(),
        open_operations: input.open_operations.clone(),
        records: input.records.clone(),
        entries: input.entries.clone(),
    })?;

    let records = by_sequence_records(&input.records);
    let own_entries = by_sequence_entries(&input.own_entries);
    let mut entries_by_id: HashMap<String, Entry> = HashMap::new();
    for entry in input.entries.iter().chain(own_entries.iter()) {
        entries_by_id.insert(entry.id().to_string(), entry.clone());
    }
    let cancelled_queue_ids: HashSet<String> = records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::QueueCancelled { entry_id, .. } => Some(entry_id.clone()),
            _ => None,
        })
        .collect();
    let pending_queue_records: Vec<&LaneRecord> = records
        .iter()
        .filter(|record| {
            matches!(record, LaneRecord::QueueEnqueued { target, .. }
                if !entries_by_id.contains_key(target.id())
                    && !cancelled_queue_ids.contains(target.id()))
        })
        .collect();
    let started = input.open_operations.first().cloned();
    let captured_initial_message_ids: HashSet<String> = match &started {
        Some(LaneRecord::OperationStarted {
            intent: OperationIntent::Run {
                initial_messages, ..
            },
            ..
        }) => initial_messages
            .iter()
            .map(|target| target.id().to_string())
            .collect(),
        _ => HashSet::new(),
    };
    let pending_next_run: Vec<Entry> = pending_queue_records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::QueueEnqueued {
                queue,
                target,
                run_id,
                ..
            } if queue == "nextRun"
                && run_id.is_none()
                && !captured_initial_message_ids.contains(target.id()) =>
            {
                Some(target.clone())
            }
            _ => None,
        })
        .collect();
    let effective_configuration = derive_effective_configuration(&input);

    let Some(started) = started else {
        return Ok(LaneReductionResult {
            lane_state: LaneState {
                lane: input.lane,
                leaf_id: input.leaf_id,
                operation: None,
                pending_next_run,
            },
            effective_configuration,
            terminal_failure: None,
        });
    };

    let started_id = started.id().to_string();
    let operation_records: Vec<&LaneRecord> = records
        .iter()
        .filter(|record| match record {
            LaneRecord::OperationStarted { id, .. } => *id == started_id,
            other => other.run_id() == Some(started_id.as_str()),
        })
        .collect();
    let aborting = operation_records
        .iter()
        .any(|record| matches!(record, LaneRecord::AbortRequested { .. }));
    let pending_steer: Vec<Entry> = if aborting {
        Vec::new()
    } else {
        pending_queue_records
            .iter()
            .filter_map(|record| match record {
                LaneRecord::QueueEnqueued {
                    queue,
                    target,
                    run_id,
                    ..
                } if queue == "steer" && run_id.as_deref() == Some(started_id.as_str()) => {
                    Some(target.clone())
                }
                _ => None,
            })
            .collect()
    };
    let pending_follow_up: Vec<Entry> = if aborting {
        Vec::new()
    } else {
        pending_queue_records
            .iter()
            .filter_map(|record| match record {
                LaneRecord::QueueEnqueued {
                    queue,
                    target,
                    run_id,
                    ..
                } if queue == "followUp" && run_id.as_deref() == Some(started_id.as_str()) => {
                    Some(target.clone())
                }
                _ => None,
            })
            .collect()
    };
    let pending_writes: Vec<Entry> = operation_records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::WriteDeferred { target, .. }
                if !entries_by_id.contains_key(target.id()) =>
            {
                Some(target.clone())
            }
            _ => None,
        })
        .collect();
    let missing_initial_messages: Vec<Entry> = match &started {
        LaneRecord::OperationStarted {
            intent: OperationIntent::Run {
                initial_messages, ..
            },
            ..
        } => initial_messages
            .iter()
            .filter(|target| !entries_by_id.contains_key(target.id()))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };

    let newest_attempt = operation_records
        .iter()
        .rev()
        .find(|record| matches!(record, LaneRecord::StepAttempt { .. }));
    let step = newest_attempt.and_then(|record| {
        let (kind, _, attempt, result_entry_id, compaction_reason, _) = attempt_fields(record)?;
        if entries_by_id.contains_key(result_entry_id) {
            return None;
        }
        Some(StepState {
            kind,
            attempts: attempt,
            result_entry_id: result_entry_id.to_string(),
            compaction_reason: if kind == "compaction" {
                compaction_reason.map(str::to_string)
            } else {
                None
            },
        })
    });

    let mut consumed_input_ids: HashSet<String> = HashSet::new();
    if let LaneRecord::OperationStarted {
        intent: OperationIntent::Run {
            initial_messages, ..
        },
        ..
    } = &started
    {
        for target in initial_messages {
            consumed_input_ids.insert(target.id().to_string());
        }
    }
    for record in &operation_records {
        if let LaneRecord::QueueEnqueued { queue, target, .. } = record
            && queue != "nextRun"
        {
            consumed_input_ids.insert(target.id().to_string());
        }
    }
    let mut newest_consumed_input_sequence = u64::MIN;
    for id in &consumed_input_ids {
        if let Some(Entry::Message { seq, .. }) = entries_by_id.get(id) {
            newest_consumed_input_sequence = newest_consumed_input_sequence.max(*seq);
        }
    }
    let overflow_recovery_used = operation_records.iter().any(|record| {
        matches!(record,
            LaneRecord::StepAttempt {
                step,
                compaction_reason: Some(reason),
                seq,
                ..
            } if step == "compaction" && reason == "overflow" && *seq > newest_consumed_input_sequence)
    });

    let newest_own_entry = own_entries.last().cloned();
    let newest_own = derive_newest_own(newest_own_entry.as_ref());
    let deferred = match &newest_own_entry {
        Some(Entry::Message {
            message: AgentMessage::Assistant(assistant),
            ..
        }) if assistant.stop_reason == StopReason::Deferred => assistant.deferred.clone(),
        _ => None,
    };
    let mut targets = Targets::default();
    match &started {
        LaneRecord::OperationStarted {
            intent: OperationIntent::Compaction {
                result_entry_id, ..
            },
            ..
        } => {
            targets.result = entries_by_id.contains_key(result_entry_id);
        }
        LaneRecord::OperationStarted {
            intent:
                OperationIntent::Navigation {
                    summary_entry_id: Some(summary_entry_id),
                    ..
                },
            ..
        } => {
            targets.summary = entries_by_id.contains_key(summary_entry_id);
        }
        _ => {}
    }

    let deferred_write_ids: HashSet<String> = operation_records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::WriteDeferred { target, .. } => Some(target.id().to_string()),
            _ => None,
        })
        .collect();
    let mut terminal_failure: Option<TerminalFailureState> = None;
    if let Some(Entry::Message {
        message: AgentMessage::Assistant(assistant),
        id,
        ..
    }) = &newest_own_entry
        && assistant.stop_reason == StopReason::Error
        && !deferred_write_ids.contains(id)
    {
        let produced_by_step = operation_records.iter().any(|record| {
            matches!(record, LaneRecord::StepAttempt { result_entry_id, .. } if result_entry_id == id)
        });
        let previous_own_entry = own_entries
            .len()
            .checked_sub(2)
            .map(|index| &own_entries[index]);
        let produced_by_deferred_fetch = operation_records.iter().any(|record| {
            matches!(record,
                LaneRecord::Usage { cause: UsageCause::DeferredFetch { entry_id, .. }, .. } if entry_id == id)
        }) || matches!(previous_own_entry,
            Some(Entry::Message { message: AgentMessage::Assistant(previous), .. })
                if previous.stop_reason == StopReason::Deferred);
        if produced_by_step || produced_by_deferred_fetch {
            terminal_failure = Some(TerminalFailureState {
                entry_id: id.clone(),
                source: if produced_by_step {
                    "step"
                } else {
                    "deferred_fetch"
                },
                message: (**assistant).clone(),
            });
        }
    }

    let (kind, intent) = match &started {
        LaneRecord::OperationStarted { intent, .. } => match intent {
            OperationIntent::Run { .. } => ("run", intent.clone()),
            OperationIntent::Compaction { .. } => ("compaction", intent.clone()),
            OperationIntent::Navigation { .. } => ("navigation", intent.clone()),
        },
        _ => unreachable!("open operations are operation_started records"),
    };
    let tool_batch = derive_tool_batch(
        &started_id,
        &operation_records
            .iter()
            .map(|record| (*record).clone())
            .collect::<Vec<_>>(),
        &own_entries,
        &entries_by_id,
        &deferred_write_ids,
    );

    Ok(LaneReductionResult {
        lane_state: LaneState {
            lane: input.lane,
            leaf_id: input.leaf_id,
            operation: Some(OperationState {
                id: started_id,
                kind,
                intent,
                aborting,
                step,
                tool_batch,
                missing_initial_messages,
                pending_steer,
                pending_follow_up,
                pending_writes,
                deferred,
                overflow_recovery_used,
                newest_own,
                targets,
            }),
            pending_next_run,
        },
        effective_configuration,
        terminal_failure,
    })
}
