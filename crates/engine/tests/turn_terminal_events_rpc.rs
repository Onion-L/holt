//! Handle-seam tests for the Turn terminal event watch (ADR-0019, spec
//! ticket 02): the engine publishes exactly one typed `TurnTerminalEvent`
//! per real main-chat Turn over `WatchTurnTerminalEvents`, only after the
//! Turn's Transcript, History, and queue completion are durably settled.
//! Events are live-only — no synthetic initial frame, no replay after
//! restart — and work outside the Turn model (manual Compaction, Title
//! tasks, Subagents, admission failures, failed settlement persistence)
//! publishes nothing.

mod common;

use std::time::Duration;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_rpc::{RpcReply, RpcService as _, methods};
use serde_json::{Value, json};

/// How long "no event arrives" waits before passing.
const NO_EVENT: Duration = Duration::from_millis(300);

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

async fn subscribe_events(
    engine: &holt_engine::LocalEngine,
) -> futures::stream::BoxStream<'static, Value> {
    let RpcReply::Stream(events) = engine
        .handle(methods::WATCH_TURN_TERMINAL_EVENTS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchTurnTerminalEvents did not return a stream");
    };
    events
}

/// Assert the watch stays silent: no synthetic initial frame, no duplicate,
/// no event for work outside the Turn model.
async fn assert_no_event<S>(events: &mut S)
where
    S: futures::Stream<Item = Value> + Unpin,
{
    assert!(
        tokio::time::timeout(NO_EVENT, events.next()).await.is_err(),
        "unexpected Turn terminal event"
    );
}

/// Queue a `run` command with an explicit message id.
async fn queue_run(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    cwd: &str,
    message_id: &str,
    prompt: &str,
) {
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

/// The current `WatchMessageQueue` frame (the watch opens with a snapshot).
async fn queue_snapshot(engine: &holt_engine::LocalEngine, chat_id: &str) -> Value {
    let RpcReply::Stream(mut queue) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId": chat_id}))
        .await
        .unwrap()
    else {
        panic!("WatchMessageQueue did not return a stream");
    };
    queue.next().await.expect("queue watch ended")
}

/// Pump queue frames until `pred` matches and return that frame.
async fn wait_for_queue(
    engine: &holt_engine::LocalEngine,
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

#[tokio::test]
async fn an_ordinary_turn_publishes_one_succeeded_event_after_settlement() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer A")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    // No synthetic initial event on subscribe.
    assert_no_event(&mut events).await;

    let submitted_at = now_millis();
    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["chatId"], "chat-1");
    assert_eq!(event["messageId"], "m-1");
    assert_eq!(event["outcome"], "succeeded");
    assert!(
        event["eventId"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && id != "m-1")
    );
    let finished_at = event["finishedAt"].as_i64().expect("finishedAt");
    assert!(finished_at >= submitted_at && finished_at <= now_millis());
    assert!(event.get("internalReason").is_none());

    // The durable records already describe the final state when the event
    // arrives: the settled transcript, and the on-disk queue with its
    // completion recorded.
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(transcript.to_string().contains("answer A"));
    let on_disk: Value = serde_json::from_slice(
        &std::fs::read(fixture.data_dir.path().join("queues/chat-1.json")).unwrap(),
    )
    .unwrap();
    assert!(on_disk["started"].is_null(), "{on_disk}");
    let queue = queue_snapshot(&engine, "chat-1").await;
    assert!(queue["activeMessageId"].is_null());
    assert_eq!(queue["pending"], json!([]));

    // Exactly one event per Turn — no duplicate.
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_skill_invocation_turn_publishes_its_own_succeeded_event() {
    let fixture = Fixture::new();
    let skill_dir = fixture.personal_dir.path().join("grill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: grill\ndescription: Grill a plan.\n---\n# grill\n",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("skill answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "invokeSkill",
                    "name": "grill",
                    "extraInstructions": "focus on the data layer",
                    "messageId": "m-skill",
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": fixture.cwd(),
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();

    let event = common::next_frame(&mut events).await;
    assert_eq!(event["chatId"], "chat-1");
    assert_eq!(event["messageId"], "m-skill");
    assert_eq!(event["outcome"], "succeeded");
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_provider_failure_publishes_a_failed_event() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed("provider exploded".into())]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-1");
    assert_eq!(event["outcome"], "failed");
    assert!(
        event["internalReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("provider exploded")),
        "{event}"
    );

    // Existing failure behavior is unchanged: the session errors. The queue
    // settles with no work left and no queue-level error, so the clean-state
    // invariant leaves it unpaused (ADR-0021).
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    let queue = queue_snapshot(&engine, "chat-1").await;
    assert_eq!(queue["paused"], json!(false));
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_context_overflow_publishes_a_failed_event() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed(
        "The input is too long: prompt is too long for the requested model".into(),
    )]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "push it over").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-1");
    assert_eq!(event["outcome"], "failed");
    assert!(
        event["internalReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("too long")),
        "{event}"
    );
    // The overflow notice settled before the event was published.
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        transcript
            .to_string()
            .contains("outgrew the model's context window")
    );
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn an_interrupted_turn_publishes_an_interrupted_event() {
    let fixture = Fixture::new();
    let observed = std::sync::Arc::new(tokio::sync::Notify::new());
    let finish = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::Cancelling {
        observed: observed.clone(),
        finish: finish.clone(),
    }]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    common::wait_for_requests(&provider, 1).await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    finish.notify_one();

    let event = common::next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-1");
    assert_eq!(event["outcome"], "interrupted");
    assert!(event.get("internalReason").is_none());
    // An interruption settles Idle like a clean end, and the queue is not
    // paused by it beyond Stop's own pause.
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_steered_turn_publishes_interrupted_then_the_new_turns_event() {
    let fixture = Fixture::new();
    let observed = std::sync::Arc::new(tokio::sync::Notify::new());
    let finish = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Cancelling {
            observed: observed.clone(),
            finish: finish.clone(),
        },
        ScriptedReply::text("answer D"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    common::wait_for_requests(&provider, 1).await;
    // Steer interrupts the active Turn and starts its own message as a new
    // Turn ahead of ordinary pending work (ADR-0015).
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "steer",
                    "prompt": "D",
                    "request": {
                        "prompt": "D",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": fixture.cwd(),
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    finish.notify_one();

    let interrupted = common::next_frame(&mut events).await;
    assert_eq!(interrupted["messageId"], "m-1");
    assert_eq!(interrupted["outcome"], "interrupted");

    // The steered message settles as its own Turn with its own identity.
    common::wait_for_requests(&provider, 2).await;
    let steered = common::next_frame(&mut events).await;
    assert_eq!(steered["outcome"], "succeeded");
    assert_eq!(steered["chatId"], "chat-1");
    assert_ne!(steered["messageId"], "m-1");
    assert_ne!(steered["eventId"], interrupted["eventId"]);
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn sequential_queue_items_publish_one_event_each() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("answer one"),
        ScriptedReply::text("answer two"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "one").await;
    queue_run(&engine, "chat-1", &fixture.cwd(), "m-2", "two").await;

    let first = common::next_frame(&mut events).await;
    let second = common::next_frame(&mut events).await;
    assert_eq!(first["messageId"], "m-1");
    assert_eq!(first["outcome"], "succeeded");
    assert_eq!(second["messageId"], "m-2");
    assert_eq!(second["outcome"], "succeeded");
    assert_ne!(first["eventId"], second["eventId"]);
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_manual_compaction_publishes_no_event() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;
    let (mut transcript, _) = common::subscribe(&engine, "chat-1").await;

    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "compact",
                    "messageId": "m-compact",
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": fixture.cwd(),
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();

    // A fresh chat has nothing to compact: the command completes with a
    // Transcript notice and the queue advances — still never a Turn.
    common::wait_for_transcript_text(&mut transcript, "nothing to compact").await;
    let queue = queue_snapshot(&engine, "chat-1").await;
    assert!(queue["activeMessageId"].is_null());
    assert_eq!(queue["pending"], json!([]));
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_title_task_publishes_no_event() {
    const INSTRUCTION: &str = holt_proto::DEFAULT_TITLE_INSTRUCTION;
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::text("A Better Title")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(
            methods::SAVE_TITLE_SETTINGS,
            json!({ "modelId": "openai/gpt-5.4", "instruction": INSTRUCTION }),
        )
        .await
        .unwrap();
    let mut events = subscribe_events(&engine).await;
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "title me").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-1");
    assert_eq!(event["outcome"], "succeeded");

    // The one-shot Title task ran to completion alongside the Turn — its
    // model work is not a Turn and publishes nothing.
    loop {
        let frame = common::next_frame(&mut chats).await;
        if frame[0]["title"] == "A Better Title" {
            break;
        }
    }
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_subagent_run_publishes_no_event_of_its_own() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            json!({"subagent_type": "explorer", "description": "Inspect files", "prompt": "brief"}),
        ),
        ScriptedReply::text("Child findings"),
        ScriptedReply::text("Parent conclusion"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "delegate").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-1");
    assert_eq!(event["outcome"], "succeeded");
    // The child really ran (parent request, child request, parent resume) —
    // and published nothing of its own.
    common::wait_for_requests(&provider, 3).await;
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn an_admission_failure_publishes_no_event() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    // A chat WITHOUT a configured provider key: the run never passes
    // admission, so no Turn exists to terminate.
    engine
        .handle(
            methods::MUTATE,
            json!({"op": "createChat", "chatId": "chat-1"}),
        )
        .await
        .unwrap();
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    let queue = wait_for_queue(&engine, "chat-1", |frame| {
        frame["pending"][0]["error"].is_string()
    })
    .await;
    assert!(
        queue["pending"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not configured"),
        "{queue}"
    );
    assert_no_event(&mut events).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_queue_completion_persistence_failure_publishes_no_event() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::gated(gate.clone(), "answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    // Admission committed before the model request; now break the queue
    // directory so the completion write cannot persist.
    common::wait_for_requests(&provider, 1).await;
    let queues = fixture.data_dir.path().join("queues");
    std::fs::set_permissions(&queues, std::fs::Permissions::from_mode(0o500)).unwrap();
    gate.notify_one();

    // The existing queue error surfaces; no terminal event is published for
    // a completion whose durable recording failed.
    let queue = wait_for_queue(&engine, "chat-1", |frame| frame["error"].is_string()).await;
    assert!(
        queue["error"]
            .as_str()
            .unwrap()
            .contains("Could not save the message queue"),
        "{queue}"
    );
    assert_no_event(&mut events).await;
    std::fs::set_permissions(&queues, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn events_are_not_replayed_after_restart() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;
    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "A").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");
    drop(events);
    drop(engine);

    // A fresh engine on the same data dir has no terminal-event history:
    // the watch opens silent and stays silent.
    let engine = fixture.engine(&provider);
    let mut events = subscribe_events(&engine).await;
    assert_no_event(&mut events).await;
}
