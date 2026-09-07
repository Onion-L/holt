//! Handle-seam tests for History persistence (ADR-0010, spec tickets 02
//! and 03): the model-facing record survives a restart (a fresh engine
//! assembled on the same data dir), each message lands on disk as it
//! completes (a crash mid-Turn keeps completed tool calls), the record
//! dies with the chat, and interrupted or errored Turns leave a History
//! the provider can always accept.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService as _, methods};
use pi_core::ai::types::{Message, StopReason};

/// Whether the chat's damaged History was set aside (a timestamped
/// `.corrupt` sibling in the history dir).
fn quarantined(fixture: &common::Fixture) -> bool {
    fixture
        .data_dir
        .path()
        .join("history")
        .read_dir()
        .map(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("chat-1.jsonl.") && name.ends_with(".corrupt")
            })
        })
        .unwrap_or(false)
}

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
        common::summarize(&provider.requests()[0].messages),
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
    let restarted = common::summarize(&requests[3].messages);
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
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let summary = common::summarize(&requests[2].messages);
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
async fn a_path_hostile_chat_id_cannot_be_durably_enqueued() {
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
    let error = engine.handle(methods::QUEUE_COMMAND, serde_json::json!({"chatId":"../escape","command":{
        "kind":"run","messageId":"m","request":{"prompt":"hello","provider":"openai","model":"openai/gpt-5.4","cwd":fixture.cwd()}
    }})).await.err().expect("unsafe path must fail admission");
    assert!(error.to_string().contains("invalid chatId"));
    assert!(provider.requests().is_empty());

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

#[tokio::test]
async fn an_interrupted_turn_leaves_an_honest_record() {
    let fixture = common::Fixture::new();
    // The stream cut after a tool call had streamed but before it could
    // run — the "interrupt before the tool runs" shape.
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::aborted_with_tool_calls(
            "i was about to ",
            vec![common::tool_call(
                "call-7",
                "bash",
                serde_json::json!({ "command": "sleep 30" }),
            )],
        ),
        ScriptedReply::text("steered elsewhere"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "please tidy up").await;
    // The Transcript keeps the interrupted Turn's existing rendering: the
    // partial text and the abort error.
    common::wait_for_transcript_text(&mut transcript, "Request was aborted").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;

    // The next Turn's request: the assistant message with the tool call
    // (as a NORMAL end — no aborted stop reason) and holt's synthetic
    // interrupted result, not upstream's "No result provided".
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "actually, don't").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let next = &requests[1].messages;
    let summary = common::summarize(next);
    assert_eq!(summary[0], "user:please tidy up");
    assert_eq!(summary[1], "assistant:text:i was about to +toolcall:call-7");
    assert!(
        summary[2].starts_with("toolresult:call-7:"),
        "{}",
        summary[2]
    );
    assert_eq!(summary[3], "user:actually, don't");
    let assistant_stops: Vec<StopReason> = next
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.stop_reason),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_stops, [StopReason::Stop]);
    let serialized = serde_json::to_string(next).unwrap();
    assert!(!serialized.contains("No result provided"));
    assert!(serialized.contains("interrupted by the user"));
}

#[tokio::test]
async fn an_errored_turn_keeps_the_prompt_and_drops_the_failed_answer() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Failed("provider exploded".into()),
        ScriptedReply::text("retry worked"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "try this").await;
    common::wait_for_transcript_text(&mut transcript, "provider exploded").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;

    // "try again" without retyping: the prompt is still in the model's
    // memory; the failed answer is not.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "try again").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(
        common::summarize(&requests[1].messages),
        ["user:try this", "user:try again"]
    );
}

#[tokio::test]
async fn a_truncated_history_tail_opens_clean_and_repairs() {
    let fixture = common::Fixture::new();
    std::fs::write(fixture.project_dir.path().join("notes.txt"), "tail notes\n").unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "read", serde_json::json!({ "path": "notes.txt" })),
        ScriptedReply::text("the final reply"),
        ScriptedReply::text("after the truncation"),
        ScriptedReply::text("after the second restart"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "leave a full record").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Cut the file's last line mid-JSON — the crash-mid-append shape.
    let history_file = fixture.data_dir.path().join("history/chat-1.jsonl");
    let bytes = std::fs::read(&history_file).unwrap();
    let cut = bytes.len() - 20;
    std::fs::write(&history_file, &bytes[..cut]).unwrap();

    // A fresh engine opens the chat; the truncated entry is absent and the
    // invariant holds for the next request.
    drop(engine);
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "after the truncation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let summary = common::summarize(&requests[2].messages);
    assert_eq!(summary[0], "user:leave a full record");
    assert_eq!(summary[1], "assistant:toolcall:call-1");
    assert!(
        summary[2].starts_with("toolresult:call-1:"),
        "{}",
        summary[2]
    );
    // The truncated closing reply is gone; the new prompt follows.
    assert_eq!(summary[3], "user:after the truncation");

    drop(engine);
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "remember the new turn").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let reloaded = common::summarize(&requests[3].messages);
    assert_eq!(reloaded[3], "user:after the truncation");
    assert_eq!(reloaded[4], "assistant:text:after the truncation");
    assert_eq!(reloaded[5], "user:remember the new turn");
}

#[tokio::test]
async fn a_load_time_repair_stays_next_to_its_call_across_a_second_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "echo hi" }),
        ),
        ScriptedReply::Silent,
        ScriptedReply::text("after the first restart"),
        ScriptedReply::text("after the second restart"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    // The tool-call message lands; the run hangs on the next request —
    // kill it there, then cut the tool result off the file so the record
    // ends on a dangling call (the crash-before-the-result shape).
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "crash me").await;
    common::wait_for_requests(&provider, 2).await;
    drop(engine);
    let history_file = fixture.data_dir.path().join("history/chat-1.jsonl");
    let text = std::fs::read_to_string(&history_file).unwrap();
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.contains("\"toolResult\""))
        .collect();
    std::fs::write(&history_file, format!("{}\n", kept.join("\n"))).unwrap();

    // Restart 1: the load-time repair answers call-1, a Turn runs on top.
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first restart").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    drop(engine);

    // Restart 2: the synthetic result is still right after its call, not
    // re-derived at the tail behind the later Turn — and there is exactly
    // one of it.
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second restart").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let summary = common::summarize(&requests.last().unwrap().messages);
    assert_eq!(summary[0], "user:crash me");
    assert_eq!(summary[1], "assistant:toolcall:call-1");
    assert!(
        summary[2].starts_with("toolresult:call-1:"),
        "{}",
        summary[2]
    );
    assert!(
        summary[2].contains("interrupted by the user"),
        "{}",
        summary[2]
    );
    assert_eq!(summary[3], "user:first restart");
    assert_eq!(summary[4], "assistant:text:after the first restart");
    assert_eq!(summary[5], "user:second restart");
    assert_eq!(summary.len(), 6);
    let serialized = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
    assert!(!serialized.contains("No result provided"));
}

#[tokio::test]
async fn a_legacy_chat_opens_with_one_persisted_notice_and_a_fresh_memory() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("legacy-era reply"),
        ScriptedReply::text("post-notice reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "before the feature").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // A transcript with no History beside it — the pre-feature shape.
    std::fs::remove_file(fixture.data_dir.path().join("history/chat-1.jsonl")).unwrap();
    drop(engine);

    // The chat opens normally, the Transcript ends with the notice, and the
    // model's memory starts after it.
    let engine = fixture.engine(&provider);
    let opening = common::transcript_snapshot(&engine, "chat-1").await;
    let opening = opening.to_string();
    assert!(
        opening.contains("before holt saved the model's conversation"),
        "no legacy notice in the opening frame: {opening}"
    );
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello again").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(
        common::summarize(&provider.requests()[1].messages),
        ["user:hello again"]
    );

    // The notice persists — reopening replays it exactly once, never a
    // second copy.
    drop(engine);
    let engine = fixture.engine(&provider);
    let reopened = common::transcript_snapshot(&engine, "chat-1").await;
    let reopened = reopened.to_string();
    assert_eq!(
        reopened
            .matches("before holt saved the model's conversation")
            .count(),
        1,
        "legacy notice not persisted exactly once: {reopened}"
    );
}

#[tokio::test]
async fn a_damaged_history_is_quarantined_with_the_reason_on_the_notice() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("first reply"),
        ScriptedReply::text("starts over"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "leave a record").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // A garbage header — one bad file must not lock the chat or the app.
    let history_file = fixture.data_dir.path().join("history/chat-1.jsonl");
    std::fs::write(&history_file, "definitely not a header\n").unwrap();
    drop(engine);

    let engine = fixture.engine(&provider);
    let opening = common::transcript_snapshot(&engine, "chat-1").await;
    let opening = opening.to_string();
    assert!(
        opening.contains("could not be read"),
        "no damaged-file notice in the opening frame: {opening}"
    );
    // Set aside, never overwritten or deleted.
    assert!(quarantined(&fixture), "no quarantined history file");
    assert!(!history_file.exists());

    // A new History starts on the next Turn: the request carries only the
    // new prompt, and the file exists again.
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "start over").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(
        common::summarize(&provider.requests()[1].messages),
        ["user:start over"]
    );
    assert!(history_file.exists());
}

#[tokio::test]
async fn an_unknown_history_version_is_quarantined_like_a_damaged_file() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "leave a record").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let history_file = fixture.data_dir.path().join("history/chat-1.jsonl");
    std::fs::write(&history_file, "{\"version\":99}\n").unwrap();
    drop(engine);

    let engine = fixture.engine(&provider);
    let opening = common::transcript_snapshot(&engine, "chat-1").await;
    let opening = opening.to_string();
    assert!(
        opening.contains("could not be read"),
        "no damaged-file notice in the opening frame: {opening}"
    );
    assert!(
        opening.contains("unknown history format version 99"),
        "{opening}"
    );
    assert!(quarantined(&fixture), "no quarantined history file");
}
