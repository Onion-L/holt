//! The confirm-changes gate at the RPC seam (ADR-0014, issue 03): mutating
//! calls pause the Turn on a pending Approval the transcript watch
//! delivers; verdicts arrive through `ResolveApproval`; denials reach the
//! model as error tool results while the Turn continues; interrupt cancels
//! a pending Approval; full-access never gates; the mode is snapshotted at
//! Turn start and settled verdicts persist and replay.

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use std::time::Duration;

mod common;

/// A chat's whole transcript gate map: tool_call_id → state string, read
/// off a fresh watch snapshot.
async fn gates(engine: &holt_engine::LocalEngine, chat_id: &str) -> Vec<(String, String)> {
    let snapshot = common::transcript_snapshot(engine, chat_id).await;
    let mut out = Vec::new();
    for entry in snapshot["reset"].as_array().unwrap_or(&vec![]) {
        for part in entry["parts"].as_array().unwrap_or(&vec![]) {
            let gate = &part["gate"];
            if !gate.is_null() {
                out.push((
                    part["id"].as_str().unwrap_or("?").to_string(),
                    common::gate_state_raw(gate),
                ));
            }
        }
    }
    out
}

/// A confirm-changes bash call pauses behind a pending Approval; allow-once
/// executes it and the Turn continues.
#[tokio::test]
async fn allow_once_releases_the_gate_and_executes() {
    let fixture = Fixture::new();
    std::fs::write(fixture.project_dir.path().join("gate.txt"), "x\n").unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "cat gate.txt" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read it").await;

    // The gate opens before execution: no second provider request yet.
    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    assert_eq!(
        provider.requests().len(),
        1,
        "the Turn must pause before the mutating call executes"
    );
    // The pending chip carries the call the user is judging.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("cat gate.txt"));

    common::resolve_approval(
        &engine,
        &approval_id,
        serde_json::json!({ "kind": "allow" }),
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Executed: the tool result reached the model's next request, and the
    // chip settled to allowed.
    let requests = provider.requests();
    assert!(requests.len() >= 2);
    assert!(
        common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:") && row.contains("x")),
        "the allowed call's output never reached the model"
    );
    assert_eq!(
        gates(&engine, "chat-1").await,
        vec![("call-1".into(), "settled:allowed".into())]
    );
}

/// Denials settle as error tool results the model reads; the Turn
/// continues. A note becomes the reason verbatim.
#[tokio::test]
async fn deny_and_deny_with_note_block_with_a_model_visible_reason() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "write",
            serde_json::json!({ "path": "a.txt", "content": "nope" }),
        ),
        ScriptedReply::tool_call(
            "call-2",
            "bash",
            serde_json::json!({ "command": "rm -rf /" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "do things").await;

    // Plain deny: the standard denial is the reason.
    let first = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    common::resolve_approval(&engine, &first, serde_json::json!({ "kind": "deny" })).await;
    // The Turn continues after a denial: the model is asked again.
    common::wait_for_requests(&provider, 2).await;
    let requests = provider.requests();
    let second = common::wait_for_gate(&engine, "chat-1", "call-2", "pending").await;
    assert!(
        common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row == &format!("toolresult:call-1:{}", "The user denied this operation.")),
        "the standard denial never reached the model: {:?}",
        common::summarize(&requests[1].messages)
    );

    // Deny with a note: the note is the reason the model reads.
    common::resolve_approval(
        &engine,
        &second,
        serde_json::json!({ "kind": "deny", "note": "use pnpm, not npm" }),
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        common::summarize(&requests[2].messages)
            .iter()
            .any(|row| row == "toolresult:call-2:use pnpm, not npm"),
        "the noted denial never reached the model: {:?}",
        common::summarize(&requests[2].messages)
    );
    // Neither file was touched.
    assert!(!fixture.project_dir.path().join("a.txt").exists());
    // Both chips settled to their verdicts, note preserved.
    assert_eq!(
        gates(&engine, "chat-1").await,
        vec![
            ("call-1".into(), "settled:denied".into()),
            ("call-2".into(), "settled:denied".into()),
        ]
    );
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("use pnpm, not npm"));
}

/// Reads and content search never pause; full-access runs mutating calls
/// with no pause and no gate artifacts.
#[tokio::test]
async fn reads_never_gate_and_full_access_gates_nothing() {
    let fixture = Fixture::new();
    std::fs::write(fixture.project_dir.path().join("notes.txt"), "n\n").unwrap();
    {
        let provider = ScriptedProvider::new(vec![
            ScriptedReply::tool_call("call-1", "read", serde_json::json!({ "path": "notes.txt" })),
            ScriptedReply::tool_call("call-2", "grep", serde_json::json!({ "pattern": "n" })),
            ScriptedReply::text("done"),
        ]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        // The default mode is confirm-changes and reads still never pause.
        let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "look").await;
        common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
        assert_eq!(gates(&engine, "chat-1").await, Vec::new());
        assert!(
            provider.requests().len() >= 3,
            "the read and grep rounds completed without any verdict"
        );
    }

    // A mutating call in full-access: no pause, no artifacts. A fresh
    // engine (new scripted stream) on the same data dir.
    let provider2 = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-3",
            "bash",
            serde_json::json!({ "command": "echo hi" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine2 = fixture.engine(&provider2);
    common::setup_chat(&engine2, "chat-2").await;
    engine2
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": "chat-2",
                "mode": "full-access",
            }),
        )
        .await
        .unwrap();
    let (_, mut sessions2) = common::subscribe(&engine2, "chat-2").await;
    common::run_prompt(&engine2, "chat-2", &fixture.cwd(), "mutate").await;
    common::wait_for_session_status(&mut sessions2, "chat-2", "idle").await;
    assert_eq!(gates(&engine2, "chat-2").await, Vec::new());
    assert!(provider2.requests().len() >= 2);
}

/// Interrupt during a pending Approval cancels it: the call settles as
/// aborted, only interrupt stops the run, and the History stays a valid
/// provider request through the interrupted-run repair.
#[tokio::test]
async fn interrupt_cancels_a_pending_approval() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "sleep 5" }),
        ),
        ScriptedReply::text("never reached"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
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
    assert_eq!(
        gates(&engine, "chat-1").await,
        vec![("call-1".into(), "settled:aborted".into())]
    );
    // The approval is spent: a late verdict fails instead of landing.
    let error = match engine
        .handle(
            methods::RESOLVE_APPROVAL,
            serde_json::json!({
                "approvalId": approval_id,
                "verdict": { "kind": "allow" }
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a spent approval must not resolve twice"),
    };
    assert!(error.to_string().contains("unknown or already-resolved"));

    // The interrupted-run repair kept the History valid: the next Turn's
    // first request carries the call with a synthetic error result.
    let follow_up = ScriptedProvider::new(vec![ScriptedReply::text("next")]);
    drop(engine);
    let engine = fixture.engine(&follow_up);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "again").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let request = &follow_up.requests()[0];
    let summarized = common::summarize(&request.messages);
    assert!(
        summarized
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:"))
    );
    // And the reloaded transcript replays the gate settled, not pending.
    assert_eq!(
        gates(&engine, "chat-1").await,
        vec![("call-1".into(), "settled:aborted".into())]
    );
}

/// The mode is snapshotted at Turn start: a switch while a gate is open
/// leaves the running Turn under confirm-changes for its later calls.
#[tokio::test]
async fn a_mode_switch_mid_turn_leaves_the_running_turn_gated() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "echo one" }),
        ),
        ScriptedReply::tool_call(
            "call-2",
            "bash",
            serde_json::json!({ "command": "echo two" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    let first = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    // Mid-Turn switch to full-access…
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": "chat-1",
                "mode": "full-access",
            }),
        )
        .await
        .unwrap();
    common::resolve_approval(&engine, &first, serde_json::json!({ "kind": "allow" })).await;

    // …the SAME Turn's second call still pauses under its snapshot.
    let second = common::wait_for_gate(&engine, "chat-1", "call-2", "pending").await;
    common::resolve_approval(&engine, &second, serde_json::json!({ "kind": "deny" })).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(
        gates(&engine, "chat-1").await,
        vec![
            ("call-1".into(), "settled:allowed".into()),
            ("call-2".into(), "settled:denied".into()),
        ]
    );
    // The NEXT Turn runs ungated under the switched mode.
    let provider2 = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-3",
            "bash",
            serde_json::json!({ "command": "echo three" }),
        ),
        ScriptedReply::text("done"),
    ]);
    drop(engine);
    let engine = fixture.engine(&provider2);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "more").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        gates(&engine, "chat-1")
            .await
            .iter()
            .all(|(id, _)| id != "call-3"),
        "the post-switch Turn must not gate"
    );
}

/// A restart mid-approval settles the persisted gate as aborted on load —
/// the replayed transcript never poses an unanswerable card.
#[tokio::test]
async fn a_restart_mid_approval_settles_the_gate_on_load() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "sleep 5" }),
        ),
        ScriptedReply::text("never"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;

    // The app dies mid-approval. The reloaded transcript settles aborted…
    drop(engine);
    let provider2 = ScriptedProvider::new(vec![ScriptedReply::text("next")]);
    let engine = fixture.engine(&provider2);
    assert_eq!(
        gates(&engine, "chat-1").await,
        vec![("call-1".into(), "settled:aborted".into())]
    );
    // …and the interrupted-run repair kept the History a valid provider
    // request: the next Turn's first request carries the call with its
    // synthetic error result.
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "again").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        common::summarize(&provider2.requests()[0].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:")),
        "the restarted chat's History lost the aborted call"
    );
}

/// The title task runs beside a gated Turn and is untouched by
/// permissions: its request never pauses and its result still lands.
#[tokio::test]
async fn the_title_task_is_unaffected_by_an_open_gate() {
    let fixture = Fixture::new();
    let instruction = "Name this chat.";
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "bash", serde_json::json!({ "command": "echo x" })),
        ScriptedReply::text("done"),
    ])
    .with_title_script(
        &holt_engine::title_system_prompt(instruction),
        vec![ScriptedReply::text("A tidy title")],
    );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(
            methods::SAVE_TITLE_SETTINGS,
            serde_json::json!({
                "modelId": "openai/gpt-5.4",
                "instruction": instruction,
            }),
        )
        .await
        .unwrap();
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;

    // The gate is open and the Turn paused — the title task still runs on
    // its own request and lands on the chat row.
    let _approval = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let RpcReply::Stream(mut chats) = engine
            .handle(methods::WATCH_CHATS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchChats did not return a stream");
        };
        let frame = common::next_frame(&mut chats).await;
        let titled = frame.to_string().contains("A tidy title");
        drop(chats);
        if titled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the title never landed while the gate was open"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Two mutating calls in ONE assistant message each get their own gate;
/// verdicts settle on the right chips and both results reach the model.
#[tokio::test]
async fn two_mutating_calls_in_one_message_gate_independently() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![
            common::tool_call("call-a", "bash", serde_json::json!({ "command": "echo a" })),
            common::tool_call(
                "call-b",
                "write",
                serde_json::json!({ "path": "b.txt", "content": "b" }),
            ),
        ]),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    // pi-core prepares a batch's calls one at a time even in the parallel
    // path, so the gates open sequentially: deny the bash with a note, and
    // only then does the write's gate open — allow it.
    let a = common::wait_for_gate(&engine, "chat-1", "call-a", "pending").await;
    common::resolve_approval(
        &engine,
        &a,
        serde_json::json!({ "kind": "deny", "note": "not a" }),
    )
    .await;
    let b = common::wait_for_gate(&engine, "chat-1", "call-b", "pending").await;
    assert_ne!(a, b, "each opening has its own approval id");
    common::resolve_approval(&engine, &b, serde_json::json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-b", "settled:allowed").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let settled = gates(&engine, "chat-1").await;
        if settled.len() == 2 {
            assert_eq!(
                settled,
                vec![
                    ("call-a".into(), "settled:denied".into()),
                    ("call-b".into(), "settled:allowed".into()),
                ]
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gates never settled: {settled:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Both results — the denial reason and the write's outcome — reached
    // the model's next request.
    common::wait_for_requests(&provider, 2).await;
    let summarized = common::summarize(&provider.requests()[1].messages);
    assert!(
        summarized
            .iter()
            .any(|row| row == "toolresult:call-a:not a")
    );
    assert!(
        summarized
            .iter()
            .any(|row| row.starts_with("toolresult:call-b:"))
    );
    assert!(fixture.project_dir.path().join("b.txt").exists());
}

/// Deleting a chat with an open gate actively stops its run: the call
/// settles aborted, nothing executes for the deleted chat, and neither the
/// transcript file nor a session row is resurrected by the settle pass.
#[tokio::test]
async fn deleting_a_chat_with_an_open_gate_stops_the_run() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "echo boom > deleted.txt" }),
        ),
        ScriptedReply::text("never"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    let approval = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;

    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();

    // The delete cancelled the run's token: the aborted run's settle pass
    // lands, its final session update DROPS the deleted chat's row (it
    // must not resurrect one), and the mutating call never executed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = common::next_frame(&mut sessions).await;
        let gone = frame
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["chatId"] != "chat-1"));
        if gone {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the deleted chat's session row never went away"
        );
    }
    assert!(!fixture.project_dir.path().join("deleted.txt").exists());
    assert!(
        !fixture
            .data_dir
            .path()
            .join("transcripts/chat-1.json")
            .exists(),
        "the deleted transcript was resurrected by the settle pass"
    );
    // The run is over, so the approval is spent: a late verdict fails.
    let error = match engine
        .handle(
            methods::RESOLVE_APPROVAL,
            serde_json::json!({
                "approvalId": approval,
                "verdict": { "kind": "allow" }
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a deleted chat's approval must not resolve"),
    };
    assert!(error.to_string().contains("unknown or already-resolved"));
    assert!(!fixture.project_dir.path().join("deleted.txt").exists());
}
