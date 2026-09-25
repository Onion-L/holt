//! Handle-seam tests for overflow recovery (ADR-0011, spec ticket 08):
//! when estimation misses and the provider rejects the request for size,
//! the Turn compacts (trigger `after overflow`) and continues in place,
//! once. When that cannot absorb it — a second overflow, a failed summary,
//! or a silent overflow — the Turn ends with a readable notice and the chat
//! row carries a persisted "compact before next Turn" flag that survives a
//! restart.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_rpc::{RpcReply, RpcService as _, methods};
use pi_core::ai::types::Usage;

/// The chat row's flag, read off a fresh chats snapshot.
async fn compact_flag(engine: &holt_engine::LocalEngine) -> bool {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let frame = chats.next().await.unwrap();
    frame[0]["compactBeforeNextTurn"].as_bool().unwrap_or(false)
}

fn overflow_usage() -> Usage {
    Usage {
        input: 299_000,
        output: 1_000,
        total_tokens: 300_000,
        ..common::fixed_usage()
    }
}

#[tokio::test]
async fn an_overflow_error_compacts_and_continues_the_same_turn() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Failed(
            "The input is too long: prompt is too long for the requested model".into(),
        ),
        ScriptedReply::text("recovery summary"),
        ScriptedReply::text("the recovered turn"),
        ScriptedReply::text("after the restart"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // One Turn: the rejected request, a summary round, then the same Turn
    // continues on the compacted History.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "push it over").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].tools, 0, "no summary round before the retry");
    let run_text = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(
        run_text.contains("history before this point was compacted"),
        "{run_text}"
    );
    assert!(run_text.contains("push it over"), "{run_text}");
    // The divider replaces the overflow error; nothing is owed.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    assert!(snapshot.contains("\"afterOverflow\""), "{snapshot}");
    assert!(snapshot.contains("the recovered turn"), "{snapshot}");
    assert!(!snapshot.contains("prompt is too long"), "{snapshot}");
    assert!(!snapshot.contains("outgrew the model's context window"));
    assert!(!compact_flag(&engine).await);
    drop(engine);

    // The History replays as summary, prompt, and the recovered answer.
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "and then").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let next_text = serde_json::to_string(&requests[3].messages).unwrap();
    for expected in [
        "history before this point was compacted",
        "push it over",
        "the recovered turn",
        "and then",
    ] {
        assert_eq!(
            next_text.matches(expected).count(),
            1,
            "{expected}: {next_text}"
        );
    }
}

#[tokio::test]
async fn a_second_overflow_flags_the_chat_across_a_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Failed("exceeds the context window of the model".into()),
        ScriptedReply::text("in-turn summary"),
        ScriptedReply::Failed("exceeds the context window of the model".into()),
        ScriptedReply::text("recovery summary"),
        ScriptedReply::text("the recovered turn"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // The in-Turn recovery runs once; overflowing again ends the Turn.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "push it over").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    common::wait_for_transcript_text(&mut transcript, "outgrew the model's context window").await;
    assert_eq!(provider.requests().len(), 3);
    assert!(compact_flag(&engine).await);
    drop(engine);

    // The flag persisted with the chat row: the fresh engine compacts
    // FIRST on the next Turn, then runs.
    let engine = fixture.engine(&provider);
    assert!(compact_flag(&engine).await);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "continue").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3].tools, 0, "no summary round before the run");
    assert!(!compact_flag(&engine).await);
}

#[tokio::test]
async fn an_ordinary_error_sets_no_flag() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed("provider exploded".into())]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    assert!(!compact_flag(&engine).await);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    assert!(!snapshot.contains("outgrew the model's context window"));
}

#[tokio::test]
async fn a_silent_overflow_is_detected_the_same_way() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        // A SUCCESSFUL reply whose reported input already exceeded the
        // window — the z.ai-style silent overflow.
        ScriptedReply::text_with_usage("a suspiciously full reply", overflow_usage()),
        ScriptedReply::text("recovery summary"),
        ScriptedReply::text("the recovered turn"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "push it over").await;
    // The Turn itself SUCCEEDED — the overflow is only visible in the
    // usage — but the flag and notice still land.
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_transcript_text(&mut transcript, "outgrew the model's context window").await;
    assert!(compact_flag(&engine).await);

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "continue").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].tools, 0);
}

#[tokio::test]
async fn a_failed_recovery_keeps_the_flag_for_the_turn_after() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Failed(
            "The input is too long: prompt is too long for the requested model".into(),
        ),
        // The in-Turn summary fails: the Turn ends, flagged.
        ScriptedReply::Failed("summarizer broke".into()),
        // The next Turn's recovery fails too: it runs uncompacted.
        ScriptedReply::Failed("summarizer broke".into()),
        ScriptedReply::text("ran uncompacted"),
        ScriptedReply::text("recovery summary"),
        ScriptedReply::text("the recovered turn"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "push it over").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    common::wait_for_transcript_text(&mut transcript, "outgrew the model's context window").await;
    assert!(compact_flag(&engine).await);

    // The recovery's summary fails: the Turn proceeds uncompacted, and the
    // debt is still owed.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "try once").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_transcript_text(&mut transcript, "Automatic compaction failed").await;
    assert!(
        compact_flag(&engine).await,
        "the failed recovery spent the flag"
    );

    // The Turn after compacts unconditionally and clears it.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "try again").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        requests[4].tools, 0,
        "no summary round before the second try"
    );
    assert!(!compact_flag(&engine).await);
}
