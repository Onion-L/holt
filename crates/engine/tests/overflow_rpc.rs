//! Handle-seam tests for the overflow fallback (ADR-0011, spec ticket 08):
//! when estimation misses and the provider rejects the request for size,
//! the Turn ends with a readable notice, the chat row carries a persisted
//! "compact before next Turn" flag, and the next Turn recovers on its own
//! — an unconditional compaction (trigger `after overflow`) whose flag
//! survives a restart. No in-Turn retry.

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
async fn an_overflow_error_flags_the_chat_and_the_next_turn_recovers() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Failed(
            "The input is too long: prompt is too long for the requested model".into(),
        ),
        ScriptedReply::text("recovery summary"),
        ScriptedReply::text("the recovered turn"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // The overflowing Turn: errored, with the readable notice, and flagged.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "push it over").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    common::wait_for_transcript_text(&mut transcript, "outgrew the model's context window").await;
    assert!(compact_flag(&engine).await);

    // The next Turn compacts FIRST — unconditionally, below the threshold
    // — then runs on the compacted History.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "continue").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].tools, 0, "no summary round before the run");
    let run = &requests[2];
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(
        run_text.contains("history before this point was compacted"),
        "{run_text}"
    );
    // The divider carries the after-overflow trigger; the flag is spent.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    assert!(snapshot.contains("compactionDivider"), "{snapshot}");
    assert!(snapshot.contains("\"afterOverflow\""), "{snapshot}");
    assert!(!compact_flag(&engine).await);
}

#[tokio::test]
async fn the_overflow_flag_survives_a_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Failed("exceeds the context window of the model".into()),
        ScriptedReply::text("recovery summary"),
        ScriptedReply::text("the recovered turn"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "push it over").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    drop(engine);

    // The flag persisted with the chat row: the fresh engine still
    // recovers on the next Turn.
    let engine = fixture.engine(&provider);
    assert!(compact_flag(&engine).await);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "continue").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].tools, 0);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("\"afterOverflow\""));
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
