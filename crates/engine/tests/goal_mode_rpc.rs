//! Goal mode end-to-end (ADR-0044): a real engine on a temp data dir with
//! the scripted provider as the model, driven through the `RpcService`
//! trait. Continuations are ordinary queue rows; the verifier is one more
//! scripted request per settled Turn (the chat's own transport), judged
//! from the History tail and the frozen change set.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};

async fn set_goal(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    text: &str,
) -> serde_json::Value {
    let RpcReply::Value(value) = engine
        .handle(
            methods::SET_GOAL,
            serde_json::json!({ "chatId": chat_id, "text": text }),
        )
        .await
        .unwrap()
    else {
        panic!("SetGoal did not return a value");
    };
    value
}

/// The chat row's `goal` field as a WatchChats frame carries it (`null`
/// once cleared).
async fn watched_goal(engine: &holt_engine::LocalEngine, chat_id: &str) -> serde_json::Value {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let frame = common::next_frame(&mut chats).await;
    frame
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == chat_id)
        .unwrap_or_else(|| panic!("chat {chat_id} missing from the watch"))["goal"]
        .clone()
}

async fn queue_snapshot(engine: &holt_engine::LocalEngine, chat_id: &str) -> serde_json::Value {
    let RpcReply::Stream(mut queue) = engine
        .handle(
            methods::WATCH_MESSAGE_QUEUE,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchMessageQueue did not return a stream");
    };
    common::next_frame(&mut queue).await
}

#[tokio::test]
async fn the_loop_runs_until_the_verifier_is_satisfied() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("wrote the feature"),
        ScriptedReply::text("CONTINUE: the tests are still missing"),
        ScriptedReply::text("added the tests"),
        ScriptedReply::text("COMPLETE: cargo test passes"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    let goal = set_goal(&engine, "chat-1", "ship the login page").await;
    assert_eq!(goal["status"], "active");
    assert_eq!(goal["iteration"], 0);
    // The user's next Turn starts the loop (a fresh chat has no captured
    // model settings to build an engine-side run from).
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "start working").await;

    // The Turn settles, the verifier says CONTINUE, the continuation runs,
    // the verifier says COMPLETE: the loop ends with a Notice and the row
    // cleared.
    let end_notice = loop {
        let frame = common::next_frame(&mut transcript).await;
        if frame.to_string().contains("Goal achieved ·") {
            break frame.to_string();
        }
    };
    // The end row is the one-line status: rounds, wall time, and the
    // goal's gross tokens — never the verifier's evidence paragraph.
    assert!(end_notice.contains("1 round"), "rounds: {end_notice}");
    assert!(end_notice.contains("tokens"), "tokens: {end_notice}");
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_requests(&provider, 4).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    // The verifier rides the chat's transport with no tools and the
    // evidence protocol.
    let verifier = &requests[1];
    assert!(
        verifier
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("goal verifier")),
        "unexpected verifier system prompt: {:?}",
        verifier.system_prompt
    );
    assert_eq!(verifier.tools, 0);
    let verifier_prompt = common::summarize(&requests[1].messages);
    assert!(
        verifier_prompt
            .iter()
            .any(|line| line.contains("ship the login page")),
        "verifier prompt lacks the goal: {verifier_prompt:?}"
    );
    // The continuation carries the verdict's reason into the next Turn.
    let continuation_prompt = common::summarize(&requests[2].messages);
    assert!(
        continuation_prompt
            .iter()
            .any(|line| line.contains("the tests are still missing")),
        "continuation prompt lacks the reason: {continuation_prompt:?}"
    );

    assert_eq!(
        watched_goal(&engine, "chat-1").await,
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn three_empty_turns_pause_the_loop() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("nothing yet"),
        ScriptedReply::text("CONTINUE: nothing happened"),
        ScriptedReply::text("still nothing"),
        ScriptedReply::text("CONTINUE: nothing happened"),
        ScriptedReply::text("and again"),
        ScriptedReply::text("CONTINUE: nothing happened"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    set_goal(&engine, "chat-1", "do something real").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "start").await;

    // Text-only Turns never call a tool and never touch the (non-Git)
    // workspace: the no-progress counter trips at three.
    common::wait_for_transcript_text(&mut transcript, "no progress").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_requests(&provider, 6).await;
    assert_eq!(provider.requests().len(), 6);
    assert_eq!(watched_goal(&engine, "chat-1").await["status"], "paused");
}

#[tokio::test]
async fn a_paused_queue_pauses_the_goal_and_deleting_the_continuation_stops_the_loop() {
    let fixture = common::Fixture::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("seeded the config"),
        ScriptedReply::text("did some work"),
        ScriptedReply::gated(gate.clone(), "CONTINUE: more to do"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // One plain Turn so the chat captures its model settings; no goal yet,
    // so no verifier follows it.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "seed").await;
    common::wait_for_transcript_text(&mut transcript, "seeded the config").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    set_goal(&engine, "chat-1", "finish the migration").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "work").await;
    common::wait_for_transcript_text(&mut transcript, "did some work").await;
    // The verifier is now in flight, blocked on the gate. Stop pauses the
    // queue and cannot reach the check (its token is not the Turn's).
    common::wait_for_requests(&provider, 3).await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": "chat-1", "command": { "kind": "interrupt" } }),
        )
        .await
        .unwrap();
    gate.notify_one();

    // The continuation lands in the paused queue and the loop stands down.
    common::wait_for_transcript_text(&mut transcript, "the queue is paused").await;
    let goal = watched_goal(&engine, "chat-1").await;
    assert_eq!(goal["status"], "paused");
    let snapshot = queue_snapshot(&engine, "chat-1").await;
    assert_eq!(snapshot["paused"], true);
    let pending = snapshot["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["goalContinuation"], true);
    let continuation_id = pending[0]["messageId"].as_str().unwrap().to_string();

    // Resuming keeps the queued step (the queue is not idle) and re-arms
    // the loop with a fresh budget.
    let RpcReply::Value(goal) = engine
        .handle(
            methods::SET_GOAL_PAUSED,
            serde_json::json!({ "chatId": "chat-1", "paused": false }),
        )
        .await
        .unwrap()
    else {
        panic!("SetGoalPaused did not return a value");
    };
    assert_eq!(goal["status"], "active");
    let snapshot = queue_snapshot(&engine, "chat-1").await;
    assert_eq!(snapshot["pending"].as_array().unwrap().len(), 1);

    // Deleting the loop's continuation is a stop request.
    engine
        .handle(
            methods::DELETE_QUEUED_MESSAGE,
            serde_json::json!({ "chatId": "chat-1", "messageId": continuation_id }),
        )
        .await
        .unwrap();
    assert_eq!(watched_goal(&engine, "chat-1").await["status"], "paused");
    let snapshot = queue_snapshot(&engine, "chat-1").await;
    assert!(snapshot["pending"].as_array().unwrap().is_empty());
    // The continuation never ran: seed Turn, work Turn, one verifier pass.
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn a_failing_verifier_keeps_the_loop_until_three_strikes() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("work one"),
        ScriptedReply::text("I cannot decide"), // not a verdict
        ScriptedReply::text("work two"),
        ScriptedReply::text("COMPLETE"), // no evidence: not a verdict
        ScriptedReply::text("work three"),
        ScriptedReply::text("huh?"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    set_goal(&engine, "chat-1", "keep at it").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "start").await;

    // Each failed check queues the next step instead of stalling active;
    // the third consecutive failure pauses the loop.
    common::wait_for_transcript_text(&mut transcript, "the verifier failed 3 times").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_requests(&provider, 6).await;
    assert_eq!(provider.requests().len(), 6);
    let goal = watched_goal(&engine, "chat-1").await;
    assert_eq!(goal["status"], "paused");
    assert_eq!(goal["evalFailures"], 3);
}

#[tokio::test]
async fn set_goal_rejects_the_wrong_chat_and_empty_text() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let empty = engine
        .handle(
            methods::SET_GOAL,
            serde_json::json!({ "chatId": "chat-1", "text": "   " }),
        )
        .await;
    assert!(empty.is_err());

    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let planning = engine
        .handle(
            methods::SET_GOAL,
            serde_json::json!({ "chatId": "chat-1", "text": "conflicting" }),
        )
        .await;
    assert!(planning.is_err());
    // And Plan Mode cannot open over a goal either — but with no goal set,
    // pausing one reports it.
    let no_goal = engine
        .handle(
            methods::SET_GOAL_PAUSED,
            serde_json::json!({ "chatId": "chat-1", "paused": true }),
        )
        .await;
    assert!(no_goal.is_err());
}
