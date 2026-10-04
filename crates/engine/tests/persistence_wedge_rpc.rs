//! Issue #16 regression: a transcript/History persist failure fences only
//! the Turn that hit it. The next attempt starts clean (the driver clears
//! `persistence_error` at dispatch), the admission persist re-establishes
//! the fence when storage is still broken — refusing before any model call
//! and parking the actionable message on the item — and a Turn that runs
//! clean settles succeeded with its terminal event published.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_engine::LocalEngine;
use holt_rpc::{RpcReply, RpcService as _, methods};
use serde_json::{Value, json};

/// How long "no event arrives" waits before passing.
const NO_EVENT: Duration = Duration::from_millis(300);

async fn queue_run(engine: &LocalEngine, chat_id: &str, cwd: &str, message_id: &str, prompt: &str) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": message_id,
                    "request": {
                        "prompt": prompt,
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
}

async fn continue_queue(engine: &LocalEngine, chat_id: &str) {
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId": chat_id}))
        .await
        .unwrap();
}

async fn wait_for_queue(
    engine: &LocalEngine,
    chat_id: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let RpcReply::Stream(mut queue) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId": chat_id}))
        .await
        .unwrap()
    else {
        panic!("WatchMessageQueue did not return a stream");
    };
    loop {
        let frame = common::next_frame(&mut queue).await;
        if pred(&frame) {
            return frame;
        }
    }
}

async fn subscribe_events(engine: &LocalEngine) -> futures::stream::BoxStream<'static, Value> {
    let RpcReply::Stream(events) = engine
        .handle(methods::WATCH_TURN_TERMINAL_EVENTS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchTurnTerminalEvents did not return a stream");
    };
    events
}

/// Run `m-1` until its model request is out, then break transcript
/// appends (the ENOSPC stand-in) and let the gated reply land: the Turn
/// settles failed on the CURRENT persist error and the queue pauses.
#[cfg(unix)]
async fn break_storage_mid_turn(
    fixture: &Fixture,
    engine: &LocalEngine,
    provider: &ScriptedProvider,
    gate: &Arc<tokio::sync::Notify>,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    queue_run(engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    common::wait_for_requests(provider, 1).await;
    let log = fixture.data_dir.path().join("transcripts/chat-1.jsonl");
    assert!(log.exists(), "admission must have persisted the user entry");
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).unwrap();
    gate.notify_one();

    let frame = wait_for_queue(engine, "chat-1", |f| f["error"].is_string()).await;
    assert!(
        frame["error"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{frame}"
    );
    assert_eq!(frame["paused"], json!(true), "{frame}");
    log
}

#[cfg(unix)]
fn restore(log: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(log, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// The wedge is gone: once storage recovers, the next attempt runs to
/// completion — it settles succeeded (not flipped to failed by the old
/// flag), the queue drains unpaused, and the terminal event publishes.
#[cfg(unix)]
#[tokio::test]
async fn storage_recovery_lifts_the_persistence_fence() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer"),
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    let log = break_storage_mid_turn(&fixture, &engine, &provider, &gate).await;
    restore(&log);

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-2", "continue").await;
    continue_queue(&engine, "chat-1").await;
    common::wait_for_requests(&provider, 2).await;
    let frame = wait_for_queue(&engine, "chat-1", |f| {
        f["paused"] == json!(false) && f["pending"].as_array().is_some_and(|p| p.is_empty())
    })
    .await;
    assert!(frame["error"].is_null(), "{frame}");

    // The failed m-1 published nothing (its completion was never durable);
    // the clean m-2 publishes exactly one succeeded event.
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-2", "{event}");
    assert_eq!(event["outcome"], "succeeded", "{event}");
    assert!(
        tokio::time::timeout(NO_EVENT, events.next()).await.is_err(),
        "unexpected second terminal event"
    );
}

/// While storage is still broken the retry is fenced at admission — before
/// any model call — and the parked item carries the actionable refusal
/// verbatim, not the bare stale error; once storage recovers, the SAME
/// parked item runs to success without a restart.
#[cfg(unix)]
#[tokio::test]
async fn a_broken_storage_retry_parks_the_actionable_refusal() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer"),
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let log = break_storage_mid_turn(&fixture, &engine, &provider, &gate).await;

    // Storage is still broken: the send is accepted, the run attempt is
    // fenced at its own admission persist, and the refusal — with its
    // actionable half — is what the parked item reports.
    queue_run(&engine, "chat-1", &fixture.cwd(), "m-2", "continue").await;
    continue_queue(&engine, "chat-1").await;
    let frame = wait_for_queue(&engine, "chat-1", |f| f["pending"][0]["error"].is_string()).await;
    let error = frame["pending"][0]["error"].as_str().unwrap();
    assert!(
        error.contains("Conversation could not be saved"),
        "the refusal must reach the parked item: {frame}"
    );
    assert!(
        error.contains("Restore storage and reopen Holt"),
        "the actionable half must survive: {frame}"
    );
    assert_eq!(frame["paused"], json!(true), "{frame}");
    assert!(frame["activeMessageId"].is_null(), "{frame}");
    // The fence fired before any model work.
    assert_eq!(provider.requests().len(), 1);

    // Storage recovers: the same parked item now runs and settles clean.
    restore(&log);
    continue_queue(&engine, "chat-1").await;
    common::wait_for_requests(&provider, 2).await;
    let frame = wait_for_queue(&engine, "chat-1", |f| {
        f["paused"] == json!(false) && f["pending"].as_array().is_some_and(|p| p.is_empty())
    })
    .await;
    assert!(frame["error"].is_null(), "{frame}");
}
