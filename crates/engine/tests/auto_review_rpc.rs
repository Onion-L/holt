//! Auto-review at the RPC seam (ADR-0014, issue 05): in auto-review every
//! mutating call is first judged by one extra model pass through the same
//! transport the run uses — recorded as a bare request (no tools) between
//! the run's own requests. A pass executes; a rejection blocks with the
//! reviewer's reason as the error tool result the agent reads, and the
//! Turn continues. No Approval is created; interrupt during a review pass
//! stops the Turn; verdict chips persist and replay.

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService, methods};

mod common;

async fn setup_auto_review_chat(engine: &holt_engine::LocalEngine, chat_id: &str) {
    common::setup_chat(engine, chat_id).await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": chat_id,
                "mode": "auto-review",
            }),
        )
        .await
        .unwrap();
}

/// A passing review executes the call; the extra review call is visible in
/// the recorded request sequence as the bare, reviewer-prompted request.
#[tokio::test]
async fn a_passing_review_executes_the_call() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "echo reviewed > out.txt" }),
        ),
        // The review pass's reply — the next request the provider sees.
        ScriptedReply::text("APPROVE"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    setup_auto_review_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewPassed").await;
    common::wait_for_requests(&provider, 3).await;

    // The recorded sequence: the run's request, the REVIEW pass (bare — no
    // tools — and prompted by the reviewer's system prompt), then the run's
    // continuation carrying the executed call's result.
    let requests = provider.requests();
    assert!(requests[0].tools > 0, "the run's request advertises tools");
    assert_eq!(
        requests[1].tools, 0,
        "the review pass is a bare completion, not an agent round"
    );
    assert!(
        requests[1]
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("permission reviewer")),
        "the review request rides the reviewer's prompt"
    );
    assert!(requests[0].system_prompt != requests[1].system_prompt);
    assert!(
        common::summarize(&requests[2].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:")),
        "the approved call's result never reached the model"
    );
    assert!(fixture.project_dir.path().join("out.txt").exists());
}

/// A rejection blocks the call with the reviewer's reason as the error
/// tool result; the Turn continues and the chip persists across a reload.
#[tokio::test]
async fn a_rejection_blocks_with_the_reviewers_reason() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "write",
            serde_json::json!({ "path": "nope.txt", "content": "x" }),
        ),
        ScriptedReply::text("REJECT: use pnpm, not npm"),
        ScriptedReply::text("understood"),
    ]);
    let engine = fixture.engine(&provider);
    setup_auto_review_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewRejected").await;
    common::wait_for_requests(&provider, 3).await;
    let requests = provider.requests();
    assert!(
        common::summarize(&requests[2].messages)
            .iter()
            .any(|row| row == "toolresult:call-1:use pnpm, not npm"),
        "the reviewer's reason never reached the model: {:?}",
        common::summarize(&requests[2].messages)
    );
    assert!(!fixture.project_dir.path().join("nope.txt").exists());
    // The chip carries the reason for the transcript record.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("use pnpm, not npm"));

    // The verdict data replays across a chat reload.
    drop(engine);
    let provider2 = ScriptedProvider::new(Vec::new());
    let engine = fixture.engine(&provider2);
    assert_eq!(
        common::gate_state(&engine, "chat-1", "call-1").await,
        "settled:reviewRejected"
    );
}

/// Reads and content search are never reviewed; no Approval ever exists in
/// this mode (chips settle straight to their verdict).
#[tokio::test]
async fn reads_are_never_reviewed_and_no_approval_exists() {
    let fixture = Fixture::new();
    std::fs::write(fixture.project_dir.path().join("notes.txt"), "n\n").unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "read", serde_json::json!({ "path": "notes.txt" })),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    setup_auto_review_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "look").await;
    common::wait_for_requests(&provider, 2).await;
    // Exactly the run's two requests — no review pass between them.
    assert_eq!(provider.requests().len(), 2);
    assert!(
        provider.requests().iter().all(|request| request.tools > 0),
        "no bare review request fired for a read"
    );
    assert_eq!(
        common::gate_state(&engine, "chat-1", "call-1").await,
        "none"
    );
    // No approval was ever registered: nothing resolves.
    let error = match engine
        .handle(
            methods::RESOLVE_APPROVAL,
            serde_json::json!({
                "approvalId": "00000000-0000-0000-0000-000000000000",
                "verdict": { "kind": "allow" }
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("auto-review must not create approvals"),
    };
    assert!(error.to_string().contains("unknown or already-resolved"));
}

/// Interrupt during a review pass stops the Turn through the existing
/// cancellation path — no verdict, no chip.
#[tokio::test]
async fn interrupt_during_a_review_pass_stops_the_turn() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "bash", serde_json::json!({ "command": "echo x" })),
        // The review pass never answers — the Turn parks mid-review. The
        // trailing reply lets the interrupted Turn settle like the
        // confirm-changes interrupt test (the scripted transport ignores
        // cancellation, so the post-abort round completes normally).
        ScriptedReply::Silent,
        ScriptedReply::text("wrapped"),
    ]);
    let engine = fixture.engine(&provider);
    setup_auto_review_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    // Two requests seen (run + review) means the review is in flight.
    common::wait_for_requests(&provider, 2).await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-1",
                "command": { "kind": "interrupt" }
            }),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    // No verdict was recorded for the interrupted review.
    assert_eq!(
        common::gate_state(&engine, "chat-1", "call-1").await,
        "none"
    );
}
