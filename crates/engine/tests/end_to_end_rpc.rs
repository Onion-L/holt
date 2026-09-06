//! One chat, the whole story (spec ticket 09): two Turns, a restart, an
//! interrupted Turn, automatic compaction before a Turn, in-Turn
//! compaction, manual `/compact`, overflow and recovery, and another
//! restart — asserting on the provider's request payloads and the
//! Transcript frames at each step. Every trigger value (automatic,
//! manual, after overflow) and every notice kind rides the same chat.
//!
//! Usage pins: the one big reply reports 260k tokens — past the
//! compaction threshold (272k window − 16384 reserve) but under the
//! window, so it never trips the silent-overflow detector.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_rpc::{RpcReply, RpcService as _, methods};
use pi_core::ai::types::{Message, Usage};

fn big_text() -> String {
    let mut text = String::new();
    while text.len() < 120_000 {
        text.push_str("end to end conversation content keeps going ");
    }
    text
}

/// Past the compaction threshold, under the window.
fn near_window_usage() -> Usage {
    Usage {
        input: 260_000,
        output: 500,
        total_tokens: 260_500,
        ..common::fixed_usage()
    }
}

fn user_text(message: &Message) -> &str {
    match message {
        Message::User(user) => match &user.content {
            pi_core::ai::types::UserContent::Text(text) => text,
            pi_core::ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .find_map(|block| match block {
                    pi_core::ai::types::BlockContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
        },
        _ => "",
    }
}

async fn compact_flag(engine: &holt_engine::LocalEngine) -> bool {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    chats.next().await.unwrap()[0]["compactBeforeNextTurn"]
        .as_bool()
        .unwrap_or(false)
}

#[tokio::test]
async fn one_chat_walks_the_whole_history_and_compaction_story() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        // Turn 1.
        ScriptedReply::text("first reply"),
        // Turn 2 — the big one that nears the window.
        ScriptedReply::text_with_usage(big_text(), near_window_usage()),
        // Turn 3: the turn-start compaction is a SPLIT cut over the small
        // history — two summary requests (history, turn prefix) — then the
        // run.
        ScriptedReply::text("auto history summary"),
        ScriptedReply::text("auto turn-prefix summary"),
        ScriptedReply::text("third reply"),
        // Turn 4 — interrupted mid-tool.
        ScriptedReply::aborted_with_tool_calls(
            "i was about to ",
            vec![common::tool_call(
                "call-i",
                "bash",
                serde_json::json!({ "command": "echo interrupted-work" }),
            )],
        ),
        // Turn 5 — steered elsewhere, closes with big small-usage content
        // so a later manual compaction has something outside its tail.
        ScriptedReply::text_with_usage(big_text(), common::fixed_usage()),
        // Manual /compact — again a split cut: two summary requests.
        ScriptedReply::text("manual history summary"),
        ScriptedReply::text("manual turn-prefix summary"),
        // Turn 6 — a tool round whose own usage crosses the threshold
        // mid-Turn, then the closing reply.
        ScriptedReply::tool_call_with_usage(
            "call-m",
            "bash",
            serde_json::json!({ "command": "echo mid-turn-round" }),
            near_window_usage(),
        ),
        ScriptedReply::text("mid checkpoint"),
        ScriptedReply::text("the loop reply"),
        // Turn 7 — the provider rejects for size.
        ScriptedReply::Failed(
            "The input is too long: prompt is too long for the requested model".into(),
        ),
        // Turn 8 — the overflow recovery.
        ScriptedReply::text("recovery checkpoint"),
        ScriptedReply::text("the recovered reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    let cwd = fixture.cwd();

    // ── Two Turns ────────────────────────────────────────────────────────
    common::run_prompt(&engine, "chat-1", &cwd, "the story begins").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::run_prompt(&engine, "chat-1", &cwd, "grow the conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(provider.requests().len(), 2);

    // ── Restart ──────────────────────────────────────────────────────────
    drop(engine);
    let engine = fixture.engine(&provider);
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // ── Automatic compaction before Turn 3 (trigger `automatic`) ────────
    common::run_prompt(&engine, "chat-1", &cwd, "continue the story").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        requests[2].tools == 0,
        "turn 3 did not open with a summary round"
    );
    let run = &requests[4];
    assert!(user_text(&run.messages[0]).contains("history before this point was compacted"));
    common::wait_for_transcript_text(&mut transcript, "auto history summary").await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("\"automatic\""));

    // ── Turn 4: interrupted mid-tool; Turn 5 steers ─────────────────────
    common::run_prompt(&engine, "chat-1", &cwd, "now interrupt me").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    common::wait_for_transcript_text(&mut transcript, "Request was aborted").await;
    common::run_prompt(&engine, "chat-1", &cwd, "don't do that").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let steered = &requests[6];
    let steered_text = serde_json::to_string(&steered.messages).unwrap();
    assert!(
        steered_text.contains("interrupted by the user"),
        "the interrupted Turn left no honest record: {steered_text}"
    );
    assert!(steered_text.contains("interrupted-work"));

    // ── Manual /compact (trigger `manual`) ───────────────────────────────
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "compact",
                    "messageId": "manual-compact",
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": cwd,
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
    // The typed command joins the queue (ticket 04); the divider landing is
    // the deterministic end of its execution.
    common::wait_for_requests(&provider, 8).await;
    common::wait_for_transcript_text(&mut transcript, "compactionDivider").await;
    let requests = provider.requests();
    let manual_history_summary = user_text(&requests[7].messages[0]);
    assert!(
        manual_history_summary.contains("<previous-summary>"),
        "{manual_history_summary}"
    );
    assert!(
        manual_history_summary.contains("auto history summary"),
        "the manual compaction did not chain: {manual_history_summary}"
    );
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("\"manual\""));

    // ── Turn 6: in-Turn compaction between tool rounds ──────────────────
    common::run_prompt(&engine, "chat-1", &cwd, "loop over the files").await;
    // round one → the summary round between rounds → round two.
    common::wait_for_requests(&provider, 12).await;
    common::wait_for_transcript_text(&mut transcript, "mid-turn-round").await;
    let requests = provider.requests();
    assert!(requests[9].tools > 0, "unexpected request order");
    assert_eq!(requests[10].tools, 0, "no mid-turn summary round");
    let round_two = &requests[11];
    let round_two_text = serde_json::to_string(&round_two.messages).unwrap();
    assert!(
        round_two_text.contains("mid checkpoint"),
        "{round_two_text}"
    );
    assert!(
        round_two_text.contains("mid-turn-round"),
        "{round_two_text}"
    );

    // ── Turn 7: the overflow; restart; Turn 8 recovers ──────────────────
    common::run_prompt(&engine, "chat-1", &cwd, "push it over the window").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    common::wait_for_transcript_text(&mut transcript, "outgrew the model's context window").await;
    assert!(compact_flag(&engine).await);
    drop(engine);

    let engine = fixture.engine(&provider);
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    assert!(
        compact_flag(&engine).await,
        "the overflow flag did not survive the restart"
    );
    common::run_prompt(&engine, "chat-1", &cwd, "and recover").await;
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
        requests[13].tools, 0,
        "the recovery did not open with a summary round"
    );
    let run = &requests[14];
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(run_text.contains("the recovered reply") || run_text.contains("recovery checkpoint"));
    assert!(!compact_flag(&engine).await, "the flag was not consumed");

    // The Transcript kept everything: all three trigger values, the
    // interrupted record's rows, and the overflow notice.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    for expected in [
        "\"automatic\"",
        "\"manual\"",
        "\"afterOverflow\"",
        "outgrew the model's context window",
        // Rows from before every compaction still exist — the Transcript
        // never shrinks.
        "the story begins",
        "grow the conversation",
        "now interrupt me",
    ] {
        assert!(
            snapshot.contains(expected),
            "missing {expected} in the transcript"
        );
    }
    let _ = &mut transcript;
}
