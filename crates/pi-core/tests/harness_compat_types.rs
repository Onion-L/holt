use std::sync::Arc;

use pi_core::agent::harness::messages::BashExecutionMessage;
use pi_core::agent::harness::session::memory::InMemorySessionRepo;
use pi_core::agent::harness::session::testing::{
    SessionBackendFixture, SessionBackendFixtureFactory,
};
use pi_core::agent::harness::session::types::{
    CompactionReason, Entry, MessageEntry, SessionStopReason,
};
use pi_core::agent::harness::tools::bash::{BashToolDetails, BashToolInput};
use pi_core::agent::harness::tools::edit::{EditToolDetails, EditToolInput};
use pi_core::agent::harness::tools::edit_diff::{
    Edit, TextReplacement, apply_replacements_preserving_unchanged_lines,
};
use pi_core::agent::harness::tools::read::{ReadToolDetails, ReadToolInput};
use pi_core::agent::harness::tools::write::WriteToolInput;
use pi_core::agent::types::{AgentMessage, CustomAgentMessage};

#[test]
fn exposes_tool_input_and_detail_shapes() {
    let bash = BashToolInput {
        command: "pwd".to_string(),
        timeout: Some(1.5),
    };
    assert_eq!(serde_json::to_value(bash).unwrap()["timeout"], 1.5);

    let edit = EditToolInput {
        path: "file.txt".to_string(),
        edits: vec![Edit {
            old_text: "before".to_string(),
            new_text: "after".to_string(),
        }],
    };
    assert_eq!(
        serde_json::to_value(edit).unwrap()["edits"][0]["oldText"],
        "before"
    );

    let read = ReadToolInput {
        path: "file.txt".to_string(),
        offset: Some(2.0),
        limit: None,
    };
    assert_eq!(serde_json::to_value(read).unwrap()["offset"], 2.0);

    let write = WriteToolInput {
        path: "file.txt".to_string(),
        content: "content".to_string(),
    };
    assert_eq!(serde_json::to_value(write).unwrap()["content"], "content");

    let _ = BashToolDetails {
        truncation: None,
        full_output_path: None,
    };
    let _ = EditToolDetails {
        diff: String::new(),
        patch: String::new(),
        first_changed_line: None,
    };
    let _ = ReadToolDetails { truncation: None };
}

#[test]
fn exposes_session_aliases_constructors_and_reason_types() {
    let message = AgentMessage::Custom(CustomAgentMessage::new(
        "custom",
        serde_json::json!({"role": "custom"}),
    ));
    let entry: MessageEntry = Entry::message("entry", message);
    assert_eq!(entry.id(), "entry");
    assert_eq!(entry.seq(), 0);
    assert_eq!(SessionStopReason::Deferred, SessionStopReason::Deferred);
    assert_eq!(CompactionReason::Threshold, CompactionReason::Threshold);

    let factory: SessionBackendFixtureFactory = Arc::new(|| {
        Box::pin(async { SessionBackendFixture::InMemory(InMemorySessionRepo::new()) })
    });
    let fixture = futures::executor::block_on(factory());
    assert!(matches!(fixture, SessionBackendFixture::InMemory(_)));
}

#[test]
fn exposes_typed_bash_messages_and_edit_replacement_helper() {
    let message = BashExecutionMessage {
        role: "bashExecution".to_string(),
        command: "pwd".to_string(),
        output: "/tmp".to_string(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 1,
        exclude_from_context: None,
    };
    assert_eq!(serde_json::to_value(message).unwrap()["exitCode"], 0);

    let output = apply_replacements_preserving_unchanged_lines(
        "a\r\nb\r\n",
        "a\nb\n",
        &[TextReplacement {
            match_index: 2,
            match_length: 1,
            new_text: "c".to_string(),
        }],
    );
    assert_eq!(output, "a\r\nc\n");
}
