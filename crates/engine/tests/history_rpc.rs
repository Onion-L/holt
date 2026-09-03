//! Handle-seam tests for History persistence (ADR-0010, spec ticket 02):
//! the model-facing record survives a restart (a fresh engine assembled on
//! the same data dir), each message lands on disk as it completes (a crash
//! mid-Turn keeps completed tool calls), and the record dies with the chat.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService as _, methods};

#[tokio::test]
async fn the_history_survives_a_restart_and_feeds_the_next_turn() {
    let fixture = common::Fixture::new();
    std::fs::write(fixture.project_dir.path().join("notes.txt"), "disk notes\n").unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "read", serde_json::json!({ "path": "notes.txt" })),
        ScriptedReply::text("first reply"),
        ScriptedReply::text("second reply"),
        ScriptedReply::text("third reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // Turn one: a prompt, a tool round-trip, a closing reply. The first
    // request starts from an empty History — no file, no error.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read the notes").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(
        common::summarize(&provider.requests()[0]),
        ["user:read the notes"]
    );

    // Turn two on the same engine: the in-memory carry-over.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "and again").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Restart: a fresh engine on the same data dir. The third Turn's first
    // request must carry everything — both prompts, both replies, and the
    // tool call with its result — replayed from the History file.
    drop(engine);
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "third prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let restarted = common::summarize(&requests[3]);
    assert_eq!(restarted.len(), 7);
    assert_eq!(restarted[0], "user:read the notes");
    assert_eq!(restarted[1], "assistant:toolcall:call-1");
    assert!(
        restarted[2].starts_with("toolresult:call-1:"),
        "unexpected tool result: {}",
        restarted[2]
    );
    assert!(restarted[2].contains("disk notes"));
    assert_eq!(restarted[3], "assistant:text:first reply");
    assert_eq!(restarted[4], "user:and again");
    assert_eq!(restarted[5], "assistant:text:second reply");
    assert_eq!(restarted[6], "user:third prompt");
}

#[tokio::test]
async fn a_crash_mid_turn_loses_no_completed_tool_result() {
    let fixture = common::Fixture::new();
    std::fs::write(
        fixture.project_dir.path().join("notes.txt"),
        "survivor content\n",
    )
    .unwrap();
    // Tool call, then a stream that never terminates: the Turn hangs after
    // the read completed — the window a force-quit hits.
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-9", "read", serde_json::json!({ "path": "notes.txt" })),
        ScriptedReply::Silent,
        ScriptedReply::text("post-crash reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read before the crash").await;
    // The second request only happens AFTER the tool result was appended —
    // wait for it, then kill the "process".
    common::wait_for_requests(&provider, 2).await;
    drop(engine);

    // A fresh engine replays the record: the tool call and its result are
    // there even though the Turn never ended.
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "after the crash").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let summary = common::summarize(&requests[2]);
    assert_eq!(summary[0], "user:read before the crash");
    assert_eq!(summary[1], "assistant:toolcall:call-9");
    assert!(
        summary[2].starts_with("toolresult:call-9:"),
        "{}",
        summary[2]
    );
    assert!(summary[2].contains("survivor content"));
    assert_eq!(summary[3], "user:after the crash");
}

#[tokio::test]
async fn deleting_a_chat_removes_its_history_file() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let history_file = fixture.data_dir.path().join("history/chat-1.jsonl");
    assert!(history_file.exists(), "the run left no History file");

    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    assert!(!history_file.exists());
}

#[tokio::test]
async fn a_path_hostile_chat_id_runs_without_a_history_file() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": "../escape" }),
        )
        .await
        .unwrap();
    let (_, mut sessions) = common::subscribe(&engine, "../escape").await;

    // The Turn runs and replies — the id disables the file, not the chat.
    common::run_prompt(&engine, "../escape", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "../escape", "idle").await;

    // No History (and no transcript) escaped the data dir, matching the
    // Transcript's behavior for hostile ids.
    assert!(!fixture.data_dir.path().join("escape.jsonl").exists());
    let dir_listing = std::fs::read_dir(fixture.data_dir.path().join("history"))
        .map(|entries| entries.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        dir_listing.is_empty(),
        "unexpected history files: {:?}",
        dir_listing
    );
}
