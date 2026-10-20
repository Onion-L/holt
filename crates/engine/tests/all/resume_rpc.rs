//! Handle-seam tests for Turn resumption (ADR-turns-resume-from-an-intent-journal):
//! a Turn a restart cut off resumes at launch from its History, except when
//! the user stopped it, it already ended, or it keeps crashing the app.

use crate::common;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::json;

async fn wait_for_queue(
    engine: &holt_engine::LocalEngine,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    loop {
        let frame = common::next_frame(&mut watch).await;
        if ready(&frame) {
            return frame;
        }
    }
}

/// Run one prompt into a stream that never ends, then kill the engine: the
/// started checkpoint and the recorded prompt are on disk, nothing else.
async fn cut_off_a_turn(fixture: &Fixture, provider: &ScriptedProvider) {
    let engine = fixture.engine(provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "long task").await;
    common::wait_for_requests(provider, 1).await;
    drop(engine);
}

/// Rewrite the saved queue's started item.
fn edit_started(fixture: &Fixture, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = fixture.data_dir.path().join("queues/chat-1.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut record["started"]);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
}

fn append_history_line(fixture: &Fixture, line: serde_json::Value) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.data_dir.path().join("history/chat-1.jsonl"))
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

#[tokio::test]
async fn a_turn_the_user_stopped_never_resumes_even_if_the_quit_beat_its_settle() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::text("no")]);
    cut_off_a_turn(&fixture, &provider).await;
    // Stop recorded, the cancel never settled: the quit won the race.
    edit_started(&fixture, |started| started["stopped"] = json!(true));

    let engine = fixture.engine(&provider);
    let state = wait_for_queue(&engine, |q| q["activeMessageId"].is_null()).await;
    assert_eq!(state["paused"], false);
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        transcript
            .to_string()
            .contains("Turn interrupted by restart")
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_turn_that_keeps_crashing_stops_resuming_at_the_limit() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::text("no")]);
    cut_off_a_turn(&fixture, &provider).await;
    edit_started(&fixture, |started| started["resumes"] = json!(3));

    let engine = fixture.engine(&provider);
    wait_for_queue(&engine, |q| q["activeMessageId"].is_null()).await;
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        transcript
            .to_string()
            .contains("it stopped resuming after 3 restarts")
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn each_resumption_is_counted_before_the_turn_relaunches() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::Silent]);
    cut_off_a_turn(&fixture, &provider).await;

    // The resumed run hangs too; the count is already durable.
    let engine = fixture.engine(&provider);
    common::wait_for_requests(&provider, 2).await;
    drop(engine);
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture.data_dir.path().join("queues/chat-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["started"]["resumes"], 1);
}

#[tokio::test]
async fn a_turn_whose_reply_ended_settles_without_running_again() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::text("no")]);
    cut_off_a_turn(&fixture, &provider).await;
    // The reply landed in the History; only the settle was lost.
    append_history_line(
        &fixture,
        json!({"kind":"message","entry":{
            "role":"assistant","content":[{"type":"text","text":"all done"}],
            "api":"openai-responses","provider":"openai","model":"gpt-5.4",
            "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
            "stopReason":"stop","timestamp":1
        }}),
    );

    let engine = fixture.engine(&provider);
    let state = wait_for_queue(&engine, |q| q["activeMessageId"].is_null()).await;
    assert_eq!(state["paused"], false);
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        !transcript
            .to_string()
            .contains("Turn interrupted by restart")
    );
    assert!(
        !transcript
            .to_string()
            .contains("Resumed after Holt restarted")
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_turn_whose_prompt_was_compacted_away_still_resumes_once() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "read",
            serde_json::json!({ "path": "missing.txt" }),
        ),
        ScriptedReply::Silent,
        ScriptedReply::text("resumed reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "long task").await;
    common::wait_for_requests(&provider, 2).await;
    drop(engine);
    // A mid-Turn compaction whose retained tail no longer holds the prompt.
    append_history_line(
        &fixture,
        json!({"kind":"compaction","entry":{
            "summary":"the summary","tokensBefore":10,"tokensAfter":5,
            "trigger":"automatic","timestamp":2,"retainedTail":2
        }}),
    );

    let engine = fixture.engine(&provider);
    let (mut transcript, _) = common::subscribe(&engine, "chat-1").await;
    common::wait_for_transcript_text(&mut transcript, "resumed reply").await;
    let requests = provider.requests();
    let resumed = common::summarize(&requests[2].messages);
    assert!(
        !resumed.iter().any(|line| line == "user:long task"),
        "{resumed:?}"
    );
    assert_eq!(
        resumed.last().unwrap().split(':').next(),
        Some("toolresult")
    );
}
