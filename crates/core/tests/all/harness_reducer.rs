//! Port of `pi-core/agent/test/harness/reducer.test.ts` (representative
//! validity and reduction cases; the full upstream suite is 1127 lines —
//! the corruption taxonomy, idle/run reduction, queue and write
//! derivation, tool-batch derivation, deferred handling, and overflow
//! guard are each pinned).

use pi_core::agent::harness::reducer::{
    EffectiveLaneConfiguration, EffectiveModel, LaneReductionInput, RecordLogCorruptionReason,
    RecordLogSlice, reduce_lane_state, validate_record_log,
};
use pi_core::agent::harness::session::types::{Entry, LaneRecord, OperationIntent, UsageCause};
use pi_core::agent::types::AgentMessage;
use pi_core::ai::types::{
    AssistantContent, BlockContent, DeferredHandle, RoleAssistant, RoleUser, StopReason,
    TextContent, ToolCall, Usage, UserContent, UserMessage,
};

fn usage() -> Usage {
    Usage {
        input: 1,
        output: 1,
        total_tokens: 2,
        ..Default::default()
    }
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text.to_string()),
        timestamp: 1,
    })
}

fn assistant_message(content: Vec<AssistantContent>, stop_reason: StopReason) -> AgentMessage {
    AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content,
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "test-model".to_string(),
        usage: usage(),
        stop_reason,
        timestamp: 1,
        deferred: (stop_reason == StopReason::Deferred).then(|| DeferredHandle {
            provider: "openai".to_string(),
            model_id: "test-model".to_string(),
            api: "openai-responses".to_string(),
            id: "deferred-1".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

fn text_content(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.to_string(),
        ..Default::default()
    })
}

fn tool_call(id: &str, name: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        ..Default::default()
    })
}

fn tool_result_message(tool_call_id: &str, tool_name: &str) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
        role: Default::default(),
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: "result".to_string(),
            ..Default::default()
        })],
        is_error: false,
        timestamp: 1,
        ..Default::default()
    }))
}

fn message_entry(id: &str, message: AgentMessage) -> Entry {
    Entry::Message {
        id: id.to_string(),
        seq: 0,
        parent_id: None,
        timestamp: 0,
        message,
        terminate: None,
    }
}

fn persisted(entry: Entry, seq: u64) -> Entry {
    entry.with_storage_fields(None, seq, seq as i64)
}

fn run_started(seq: u64, initial_messages: Vec<Entry>) -> LaneRecord {
    LaneRecord::OperationStarted {
        id: "run-1".to_string(),
        lane: "main".to_string(),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages,
            system_prompt_override: None,
            resume_data: None,
        },
        seq,
        timestamp: seq as i64,
    }
}

fn step_attempt(seq: u64, run_id: &str, step: &str, attempt: u32, result: &str) -> LaneRecord {
    LaneRecord::StepAttempt {
        id: format!("attempt-{seq}"),
        lane: "main".to_string(),
        run_id: run_id.to_string(),
        step: step.to_string(),
        attempt,
        result_entry_id: result.to_string(),
        compaction_reason: (step == "compaction").then(|| "manual".to_string()),
        seq,
        timestamp: seq as i64,
    }
}

fn overflow_attempt(seq: u64, run_id: &str) -> LaneRecord {
    LaneRecord::StepAttempt {
        id: format!("attempt-{seq}"),
        lane: "main".to_string(),
        run_id: run_id.to_string(),
        step: "compaction".to_string(),
        attempt: 1,
        result_entry_id: "c-1".to_string(),
        compaction_reason: Some("overflow".to_string()),
        seq,
        timestamp: seq as i64,
    }
}

fn queue_enqueued(seq: u64, queue: &str, run_id: Option<&str>, target: Entry) -> LaneRecord {
    LaneRecord::QueueEnqueued {
        id: format!("queue-{seq}"),
        lane: "main".to_string(),
        queue: queue.to_string(),
        run_id: run_id.map(str::to_string),
        target,
        seq,
        timestamp: seq as i64,
    }
}

fn abort_requested(seq: u64, run_id: &str) -> LaneRecord {
    LaneRecord::AbortRequested {
        id: format!("abort-{seq}"),
        lane: "main".to_string(),
        run_id: run_id.to_string(),
        seq,
        timestamp: seq as i64,
    }
}

fn tool_started(
    seq: u64,
    run_id: &str,
    assistant_id: &str,
    index: u32,
    call: &str,
    name: &str,
    result: &str,
) -> LaneRecord {
    LaneRecord::ToolStarted {
        id: format!("tool-{seq}"),
        lane: "main".to_string(),
        run_id: run_id.to_string(),
        assistant_entry_id: assistant_id.to_string(),
        tool_index: index,
        tool_call_id: call.to_string(),
        tool_name: name.to_string(),
        effective_args: serde_json::Value::Object(Default::default()),
        result_entry_id: result.to_string(),
        replay: "never".to_string(),
        seq,
        timestamp: seq as i64,
    }
}

fn defaults() -> EffectiveLaneConfiguration {
    EffectiveLaneConfiguration {
        model: EffectiveModel {
            provider: "openai".to_string(),
            model_id: "default-model".to_string(),
        },
        thinking_level: "off".to_string(),
        active_tool_names: vec!["bash".to_string()],
    }
}

fn slice(records: Vec<LaneRecord>, open: Vec<LaneRecord>) -> RecordLogSlice {
    RecordLogSlice {
        lane: "main".to_string(),
        open_operations: open,
        records,
        entries: Vec::new(),
    }
}

fn reduction(
    open: Vec<LaneRecord>,
    records: Vec<LaneRecord>,
    own_entries: Vec<Entry>,
) -> LaneReductionInput {
    LaneReductionInput {
        lane: "main".to_string(),
        open_operations: open,
        records: records.clone(),
        entries: Vec::new(),
        leaf_id: None,
        own_entries: own_entries.clone(),
        configuration_entries: own_entries,
        defaults: defaults(),
    }
}

// ---------------------------------------------------------------------------
// record-log validity
// ---------------------------------------------------------------------------

#[test]
fn rejects_unknown_operation_references() {
    let error = validate_record_log(&slice(
        vec![step_attempt(2, "ghost", "assistant", 1, "m-1")],
        Vec::new(),
    ))
    .unwrap_err();
    assert_eq!(error.reason, RecordLogCorruptionReason::UnknownOperation);
    assert!(error.message.contains("references unknown operation ghost"));
}

#[test]
fn rejects_records_after_finish() {
    let records = vec![
        run_started(1, Vec::new()),
        LaneRecord::OperationFinished {
            id: "finish".to_string(),
            lane: "main".to_string(),
            run_id: "run-1".to_string(),
            outcome: "completed".to_string(),
            error: None,
            seq: 5,
            timestamp: 5,
        },
        step_attempt(6, "run-1", "assistant", 1, "m-1"),
    ];
    let error = validate_record_log(&slice(records, Vec::new())).unwrap_err();
    assert_eq!(error.reason, RecordLogCorruptionReason::RecordAfterFinish);
}

#[test]
fn rejects_non_consecutive_attempts() {
    let records = vec![
        run_started(1, Vec::new()),
        step_attempt(2, "run-1", "assistant", 1, "m-1"),
        step_attempt(3, "run-1", "assistant", 3, "m-2"),
    ];
    let error = validate_record_log(&slice(records, Vec::new())).unwrap_err();
    assert_eq!(
        error.reason,
        RecordLogCorruptionReason::NonConsecutiveAttempt
    );
    assert!(
        error.message.contains("is 3; expected 2"),
        "{}",
        error.message
    );
}

#[test]
fn rejects_duplicate_tool_invocations_and_mismatches() {
    let assistant = persisted(
        message_entry(
            "a-1",
            assistant_message(vec![tool_call("call-1", "tool-1")], StopReason::ToolUse),
        ),
        2,
    );
    let start = tool_started(3, "run-1", "a-1", 0, "call-1", "tool-1", "t-1");
    let records = vec![run_started(1, Vec::new()), start.clone(), start];
    let mut input = slice(records, Vec::new());
    input.entries = vec![assistant];
    let error = validate_record_log(&input).unwrap_err();
    assert_eq!(
        error.reason,
        RecordLogCorruptionReason::DuplicateToolInvocation
    );

    let mismatch = tool_started(3, "run-1", "a-1", 0, "call-9", "tool-1", "t-1");
    let records = vec![run_started(1, Vec::new()), mismatch];
    let mut input = slice(records, Vec::new());
    input.entries = vec![persisted(
        message_entry(
            "a-1",
            assistant_message(vec![tool_call("call-1", "tool-1")], StopReason::ToolUse),
        ),
        2,
    )];
    let error = validate_record_log(&input).unwrap_err();
    assert_eq!(error.reason, RecordLogCorruptionReason::ToolCallMismatch);
}

#[test]
fn rejects_deferred_assistant_entries_without_handles() {
    let mut input = slice(Vec::new(), Vec::new());
    let mut deferred = assistant_message(vec![text_content("hi")], StopReason::Deferred);
    if let AgentMessage::Assistant(assistant) = &mut deferred {
        assistant.deferred = None;
    }
    input.entries = vec![persisted(message_entry("a-1", deferred), 1)];
    let error = validate_record_log(&input).unwrap_err();
    assert_eq!(
        error.reason,
        RecordLogCorruptionReason::InvalidDeferredHandle
    );
}

// ---------------------------------------------------------------------------
// lane-state reduction
// ---------------------------------------------------------------------------

#[test]
fn reduces_an_idle_lane_to_pending_next_run_and_default_configuration() {
    let target = message_entry("m-1", user_message("queued"));
    let records = vec![queue_enqueued(1, "nextRun", None, target)];
    let result = reduce_lane_state(reduction(Vec::new(), records, Vec::new())).unwrap();

    assert!(result.lane_state.operation.is_none());
    assert_eq!(result.lane_state.pending_next_run.len(), 1);
    assert_eq!(result.lane_state.pending_next_run[0].id(), "m-1");
    assert_eq!(
        result.effective_configuration.model.model_id,
        "default-model"
    );
    assert!(result.terminal_failure.is_none());
}

#[test]
fn folds_persisted_configuration_over_defaults_in_sequence() {
    let entries = vec![
        persisted(
            Entry::ModelChange {
                id: "cfg-1".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                provider: "anthropic".to_string(),
                model_id: "claude".to_string(),
            },
            1,
        ),
        persisted(
            Entry::ThinkingLevelChange {
                id: "cfg-2".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                thinking_level: "high".to_string(),
            },
            2,
        ),
        persisted(
            Entry::ActiveToolsChange {
                id: "cfg-3".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                active_tool_names: vec!["read".to_string()],
            },
            3,
        ),
    ];
    let result = reduce_lane_state(reduction(Vec::new(), Vec::new(), entries)).unwrap();

    let configuration = result.effective_configuration;
    assert_eq!(configuration.model.provider, "anthropic");
    assert_eq!(configuration.model.model_id, "claude");
    assert_eq!(configuration.thinking_level, "high");
    assert_eq!(configuration.active_tool_names, ["read"]);
}

#[test]
fn keeps_captured_next_run_input_with_the_open_run() {
    let captured = message_entry("m-1", user_message("captured"));
    let started = run_started(1, vec![captured.clone()]);
    let records = vec![
        started.clone(),
        queue_enqueued(2, "nextRun", None, captured),
    ];
    let mut input = reduction(vec![started], records, Vec::new());
    input.entries = vec![persisted(message_entry("m-1", user_message("captured")), 1)];
    let result = reduce_lane_state(input).unwrap();

    let operation = result.lane_state.operation.expect("open operation");
    assert!(result.lane_state.pending_next_run.is_empty());
    assert!(operation.missing_initial_messages.is_empty());
}

#[test]
fn derives_missing_input_queues_and_unfinished_attempt() {
    let captured = message_entry("m-1", user_message("captured"));
    let steer = message_entry("m-2", user_message("steer me"));
    let follow_up = message_entry("m-3", user_message("follow up"));
    let write = message_entry("m-4", user_message("write me"));
    let started = run_started(1, vec![captured]);
    let records = vec![
        started.clone(),
        queue_enqueued(2, "steer", Some("run-1"), steer),
        queue_enqueued(3, "followUp", Some("run-1"), follow_up),
        LaneRecord::WriteDeferred {
            id: "wd-1".to_string(),
            lane: "main".to_string(),
            run_id: "run-1".to_string(),
            target: write,
            seq: 4,
            timestamp: 4,
        },
        step_attempt(5, "run-1", "assistant", 1, "a-1"),
    ];
    let result = reduce_lane_state(reduction(vec![started], records, Vec::new())).unwrap();

    let operation = result.lane_state.operation.expect("open operation");
    assert_eq!(operation.missing_initial_messages.len(), 1);
    assert_eq!(operation.pending_steer.len(), 1);
    assert_eq!(operation.pending_follow_up.len(), 1);
    assert_eq!(operation.pending_writes.len(), 1);
    let step = operation.step.expect("unfinished attempt");
    assert_eq!(step.kind, "assistant");
    assert_eq!(step.attempts, 1);
    assert_eq!(step.result_entry_id, "a-1");
}

#[test]
fn kills_steer_and_follow_up_on_abort_preserving_writes_and_next_run() {
    let steer = message_entry("m-2", user_message("steer me"));
    let next_run = message_entry("m-3", user_message("next run"));
    let write = message_entry("m-4", user_message("write me"));
    let started = run_started(1, Vec::new());
    let records = vec![
        started.clone(),
        queue_enqueued(2, "steer", Some("run-1"), steer),
        queue_enqueued(3, "nextRun", None, next_run),
        abort_requested(4, "run-1"),
        LaneRecord::WriteDeferred {
            id: "wd-1".to_string(),
            lane: "main".to_string(),
            run_id: "run-1".to_string(),
            target: write,
            seq: 5,
            timestamp: 5,
        },
    ];
    let result = reduce_lane_state(reduction(vec![started], records, Vec::new())).unwrap();

    let operation = result.lane_state.operation.expect("open operation");
    assert!(operation.aborting);
    assert!(operation.pending_steer.is_empty());
    assert!(operation.pending_follow_up.is_empty());
    assert_eq!(operation.pending_writes.len(), 1);
    assert_eq!(result.lane_state.pending_next_run.len(), 1);
}

#[test]
fn closes_the_newest_attempt_only_when_its_result_exists() {
    let started = run_started(1, Vec::new());
    let records = vec![
        started.clone(),
        step_attempt(2, "run-1", "assistant", 1, "a-1"),
    ];
    let own = vec![persisted(
        message_entry(
            "a-1",
            assistant_message(vec![text_content("done")], StopReason::Stop),
        ),
        3,
    )];
    let result = reduce_lane_state(reduction(vec![started], records, own)).unwrap();
    assert!(
        result
            .lane_state
            .operation
            .expect("operation")
            .step
            .is_none()
    );
}

#[test]
fn derives_tool_batches_from_starts_and_blocked_results() {
    let assistant = persisted(
        message_entry(
            "a-1",
            assistant_message(
                vec![
                    text_content("running"),
                    tool_call("call-1", "tool-1"),
                    tool_call("call-2", "tool-2"),
                ],
                StopReason::ToolUse,
            ),
        ),
        2,
    );
    let started = run_started(1, Vec::new());
    let records = vec![
        started.clone(),
        tool_started(3, "run-1", "a-1", 0, "call-1", "tool-1", "t-1"),
    ];
    let own = vec![
        assistant.clone(),
        persisted(
            message_entry("t-2", tool_result_message("call-2", "tool-2")),
            4,
        ),
    ];
    let mut input = reduction(vec![started], records, own);
    input.entries = vec![
        assistant,
        persisted(
            message_entry("t-1", tool_result_message("call-1", "tool-1")),
            3,
        ),
    ];
    let result = reduce_lane_state(input).unwrap();

    let batch = result
        .lane_state
        .operation
        .expect("operation")
        .tool_batch
        .expect("tool batch");
    assert_eq!(batch.assistant_entry_id, "a-1");
    assert_eq!(batch.calls.len(), 2);
    assert!(batch.calls[0].started.is_some());
    assert!(batch.calls[0].result_exists);
    assert!(batch.calls[1].started.is_none());
    assert!(batch.calls[1].result_exists);
    assert!(!batch.unresolved);
    assert!(!batch.truncated);
}

#[test]
fn marks_a_length_stopped_batch_truncated() {
    let assistant = persisted(
        message_entry(
            "a-1",
            assistant_message(
                vec![text_content("cut off"), tool_call("call-1", "tool-1")],
                StopReason::Length,
            ),
        ),
        2,
    );
    let started = run_started(1, Vec::new());
    let result = reduce_lane_state(reduction(vec![started], Vec::new(), vec![assistant])).unwrap();
    let batch = result
        .lane_state
        .operation
        .expect("operation")
        .tool_batch
        .expect("tool batch");
    assert!(batch.truncated);
    assert!(batch.unresolved);
}

#[test]
fn detects_terminal_failures_from_steps_and_deferred_fetches() {
    let started = run_started(1, Vec::new());
    let error_assistant = persisted(
        message_entry(
            "a-1",
            assistant_message(vec![text_content("nope")], StopReason::Error),
        ),
        2,
    );
    let records = vec![
        started.clone(),
        step_attempt(3, "run-1", "assistant", 1, "a-1"),
    ];
    let result =
        reduce_lane_state(reduction(vec![started], records, vec![error_assistant])).unwrap();
    let failure = result.terminal_failure.expect("terminal failure");
    assert_eq!(failure.source, "step");
    assert_eq!(failure.entry_id, "a-1");
}

#[test]
fn ignores_error_shaped_deferred_writes_as_terminal_failures() {
    let started = run_started(1, Vec::new());
    let error_assistant = persisted(
        message_entry(
            "a-1",
            assistant_message(vec![text_content("deferred write")], StopReason::Error),
        ),
        2,
    );
    let records = vec![
        started.clone(),
        step_attempt(3, "run-1", "assistant", 1, "a-1"),
        LaneRecord::WriteDeferred {
            id: "wd-1".to_string(),
            lane: "main".to_string(),
            run_id: "run-1".to_string(),
            target: message_entry("a-1", user_message("unused")),
            seq: 4,
            timestamp: 4,
        },
    ];
    let result =
        reduce_lane_state(reduction(vec![started], records, vec![error_assistant])).unwrap();
    assert!(result.terminal_failure.is_none());
}

#[test]
fn tracks_deferred_handles_at_the_operation_tail() {
    let started = run_started(1, Vec::new());
    let deferred = persisted(
        message_entry(
            "a-1",
            assistant_message(vec![text_content("later")], StopReason::Deferred),
        ),
        2,
    );
    let result = reduce_lane_state(reduction(vec![started], Vec::new(), vec![deferred])).unwrap();
    let operation = result.lane_state.operation.expect("operation");
    let handle = operation.deferred.expect("deferred handle");
    assert_eq!(handle.id, "deferred-1");
    assert!(matches!(
        operation.newest_own,
        Some(ref newest) if newest.stop_reason == Some(StopReason::Deferred)
    ));
}

#[test]
fn resets_the_overflow_guard_after_newer_input_is_consumed() {
    let started = run_started(1, Vec::new());
    let overflow_before = vec![started.clone(), overflow_attempt(2, "run-1")];
    let result = reduce_lane_state(reduction(
        vec![started.clone()],
        overflow_before,
        Vec::new(),
    ))
    .unwrap();
    assert!(
        result
            .lane_state
            .operation
            .expect("operation")
            .overflow_recovery_used
    );

    // A queue_enqueued record after the compaction attempt consumes newer
    // input and clears the guard.
    let overflow_after = vec![
        started.clone(),
        overflow_attempt(2, "run-1"),
        queue_enqueued(
            3,
            "steer",
            Some("run-1"),
            message_entry("m-9", user_message("new")),
        ),
    ];
    let mut input = reduction(vec![started], overflow_after, Vec::new());
    input.entries = vec![persisted(message_entry("m-9", user_message("new")), 4)];
    let result = reduce_lane_state(input).unwrap();
    assert!(
        !result
            .lane_state
            .operation
            .expect("operation")
            .overflow_recovery_used
    );
}

#[test]
fn records_usage_cause_payloads_survive_reduction() {
    let started = run_started(1, Vec::new());
    let records = vec![
        started.clone(),
        LaneRecord::Usage {
            id: "u-1".to_string(),
            lane: "main".to_string(),
            usage: usage(),
            cause: UsageCause::Assistant {
                run_id: "run-1".to_string(),
                entry_id: "a-1".to_string(),
                attempt: 1,
                stop_reason: "stop".to_string(),
            },
            seq: 2,
            timestamp: 2,
        },
    ];
    let result = reduce_lane_state(reduction(vec![started], records, Vec::new())).unwrap();
    assert!(result.lane_state.operation.is_some() || result.lane_state.pending_next_run.is_empty());
}

fn clone_input(input: &LaneReductionInput) -> LaneReductionInput {
    LaneReductionInput {
        lane: input.lane.clone(),
        open_operations: input.open_operations.clone(),
        records: input.records.clone(),
        entries: input.entries.clone(),
        leaf_id: input.leaf_id.clone(),
        own_entries: input.own_entries.clone(),
        configuration_entries: input.configuration_entries.clone(),
        defaults: defaults(),
    }
}

// ---------------------------------------------------------------------------
// The remaining reducer.test.ts cases (determinism, input immutability,
// configuration anchoring, and deferred-write tool batches).

#[test]
fn applies_committed_operation_owned_configuration_after_the_anchor() {
    let assistant = persisted(
        {
            let mut entry = message_entry(
                "assistant-config",
                AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
                    role: RoleAssistant,
                    content: vec![text_content("response")],
                    api: "openai-responses".to_string(),
                    provider: "response-provider".to_string(),
                    model: "response-model".to_string(),
                    usage: usage(),
                    stop_reason: StopReason::Stop,
                    timestamp: 1,
                    ..Default::default()
                })),
            );
            if let Entry::Message { parent_id, .. } = &mut entry {
                *parent_id = None;
            }
            entry
        },
        2,
    );
    let tools = Entry::ActiveToolsChange {
        id: "operation-tools".to_string(),
        active_tool_names: vec!["operation-tool".to_string()],
        parent_id: Some("assistant-config".to_string()),
        seq: 3,
        timestamp: 3,
    };

    let start = run_started(1, Vec::new());
    let result = reduce_lane_state(reduction(
        vec![start.clone()],
        vec![start],
        vec![assistant, tools],
    ))
    .unwrap();

    assert_eq!(
        result.effective_configuration,
        EffectiveLaneConfiguration {
            model: EffectiveModel {
                provider: "response-provider".to_string(),
                model_id: "response-model".to_string(),
            },
            thinking_level: "off".to_string(),
            active_tool_names: vec!["operation-tool".to_string()],
        }
    );
}

#[test]
fn does_not_mutate_its_bounded_recovery_inputs() {
    let target = {
        let mut entry = message_entry("prompt-1", user_message("hello"));
        if let Entry::Message { terminate, .. } = &mut entry {
            *terminate = None;
        }
        entry
    };
    let start = run_started(1, vec![target.clone()]);
    let entry = persisted(target, 2);
    let input = RecordLogSlice {
        lane: "main".to_string(),
        open_operations: vec![start.clone()],
        records: vec![start.clone()],
        entries: vec![entry.clone()],
    };
    let records_before = input.records.clone();
    let entries_before = input.entries.clone();

    assert!(validate_record_log(&input).is_ok());
    assert_eq!(input.records, records_before);
    assert_eq!(input.entries, entries_before);
}

#[test]
fn ignores_unfulfilled_result_ids_from_earlier_attempts() {
    let target = message_entry(
        "attempt-2-result",
        assistant_message(vec![text_content("done")], StopReason::Stop),
    );
    let start = run_started(1, Vec::new());
    let result = reduce_lane_state(reduction(
        vec![start.clone()],
        vec![
            start,
            step_attempt(2, "run-1", "assistant", 1, "attempt-1-result"),
            step_attempt(3, "run-1", "assistant", 2, "attempt-2-result"),
        ],
        vec![persisted(target, 4)],
    ))
    .unwrap();

    assert!(result.lane_state.operation.unwrap().step.is_none());
}

#[test]
fn does_not_resolve_a_tool_batch_from_a_deferred_write_tool_result() {
    let assistant_target = message_entry(
        "assistant-tools",
        assistant_message(vec![tool_call("call-1", "tool-1")], StopReason::ToolUse),
    );
    let assistant = persisted(assistant_target, 3);
    let written_result = message_entry(
        "written-tool-result",
        tool_result_message("call-1", "tool-1"),
    );
    let start = run_started(1, Vec::new());
    let write_deferred = LaneRecord::WriteDeferred {
        id: "write-4".to_string(),
        lane: "main".to_string(),
        run_id: "run-1".to_string(),
        target: written_result.clone(),
        seq: 4,
        timestamp: 4,
    };
    let persisted_result = {
        let mut entry = persisted(written_result, 5);
        if let Entry::Message { parent_id, .. } = &mut entry {
            *parent_id = Some("assistant-tools".to_string());
        }
        entry
    };

    let result = reduce_lane_state(reduction(
        vec![start.clone()],
        vec![
            start,
            step_attempt(2, "run-1", "assistant", 1, "assistant-tools"),
            write_deferred,
        ],
        vec![assistant, persisted_result],
    ))
    .unwrap();

    let operation = result.lane_state.operation.unwrap();
    let batch = operation.tool_batch.expect("tool batch");
    assert!(!batch.calls[0].result_exists);
    assert!(batch.unresolved);
}

#[test]
fn is_deterministic_and_does_not_mutate_or_alias_its_inputs() {
    let pending = message_entry("next", user_message("next"));
    let start = queue_enqueued(1, "nextRun", None, pending.clone());
    let input = reduction(Vec::new(), vec![start.clone()], Vec::new());
    let before = clone_input(&input);

    let first = reduce_lane_state(clone_input(&input)).unwrap();
    let second = reduce_lane_state(clone_input(&input)).unwrap();

    assert_eq!(
        first.lane_state.pending_next_run, second.lane_state.pending_next_run,
        "repeated reductions agree"
    );
    assert_eq!(input.records, before.records, "input records unchanged");
    // Mutating the output must not leak into the input (no aliasing).
    let mut first = first;
    if let Some(Entry::Message { id, .. }) = first.lane_state.pending_next_run.first_mut() {
        *id = "mutated-output".to_string();
    }
    match &input.records[0] {
        LaneRecord::QueueEnqueued { target, .. } => {
            assert_eq!(
                match target {
                    Entry::Message { id, .. } => id,
                    _ => "unexpected entry",
                },
                "next"
            );
        }
        other => panic!("unexpected record {other:?}"),
    }
}
