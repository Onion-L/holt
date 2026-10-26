//! The agent-facing Routine tool at the Turn seam: a scripted model creates
//! a relative one-shot Routine through the ordinary approval gate, and the
//! existing scheduler fires it when the manual clock reaches its deadline.

use crate::common;

use chrono::{TimeZone, Utc};
use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_engine::{Clock, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::{Value, json};

const PROMPT: &str = "Read https://github.com/Onion-L/holt/issues and summarize the open issues.";

async fn list(engine: &LocalEngine) -> Vec<Value> {
    let RpcReply::Value(value) = engine
        .handle(methods::LIST_ROUTINES, json!({}))
        .await
        .unwrap()
    else {
        panic!("ListRoutines did not return a value");
    };
    value.as_array().unwrap().clone()
}

async fn wait_for_routine(engine: &LocalEngine, id: &str, done: impl Fn(&Value) -> bool) -> Value {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_ROUTINES, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchRoutines did not return a stream");
    };
    loop {
        let frame = common::next_frame(&mut watch).await;
        if let Some(routine) = frame
            .as_array()
            .unwrap()
            .iter()
            .find(|routine| routine["id"] == id)
            && done(routine)
        {
            return routine.clone();
        }
    }
}

async fn chat_row(engine: &LocalEngine, chat_id: &str) -> Value {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    common::next_frame(&mut chats)
        .await
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == chat_id)
        .cloned()
        .expect("routine run Chat is listed")
}

#[tokio::test]
async fn an_agent_schedule_is_approved_persisted_and_fired() {
    let fixture = Fixture::new();
    let clock = Clock::manual(Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "schedule-1",
            "schedule_routine",
            json!({
                "name": "Check GitHub issues",
                "prompt": PROMPT,
                "delayMinutes": 120,
                "timeZone": "UTC"
            }),
        ),
        ScriptedReply::text("scheduled"),
        ScriptedReply::text("issues checked"),
    ]);
    let engine = fixture.engine_with_clock(&provider, clock.clone());
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(
        &engine,
        "chat-1",
        &fixture.cwd(),
        "check the issues in two hours",
    )
    .await;

    let approval = common::wait_for_gate(&engine, "chat-1", "schedule-1", "pending").await;
    assert!(
        list(&engine).await.is_empty(),
        "nothing writes before approval"
    );
    let pending = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(pending.contains("Check GitHub issues"), "{pending}");
    assert!(pending.contains("in 120 minutes"), "{pending}");
    assert!(pending.contains(PROMPT), "{pending}");

    common::resolve_approval(&engine, &approval, json!({ "kind": "allow" })).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let routines = list(&engine).await;
    assert_eq!(routines.len(), 1);
    let routine = &routines[0];
    let id = routine["id"].as_str().unwrap();
    assert_eq!(routine["name"], "Check GitHub issues");
    assert_eq!(routine["prompt"], PROMPT);
    assert_eq!(routine["at"], "2026-10-07T11:00:00");
    assert_eq!(routine["nextFireAt"], "2026-10-07T11:00:00Z");
    assert_eq!(routine["timeZone"], "UTC");
    assert_eq!(routine["config"]["permissionMode"], "confirm-changes");

    clock.set(Utc.with_ymd_and_hms(2026, 10, 7, 11, 0, 0).unwrap());
    let fired = wait_for_routine(&engine, id, |routine| {
        routine["runs"][0]["outcome"] == "succeeded"
    })
    .await;
    assert_eq!(fired["lastFiredAt"], "2026-10-07T11:00:00Z");
    assert!(fired["nextFireAt"].is_null());
    let run = &fired["runs"][0];
    assert_eq!(run["missedFires"], 0);
    assert_eq!(run["manual"], false);
    let run_chat = chat_row(&engine, run["chatId"].as_str().unwrap()).await;
    assert_eq!(run_chat["title"], "Check GitHub issues");
    assert_eq!(run_chat["routineRun"]["routineId"], id);
    assert_eq!(run_chat["config"]["permissionMode"], "confirm-changes");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].tool_names.contains(&"schedule_routine".into()));
    assert!(
        common::summarize(&requests[2].messages)
            .iter()
            .any(|row| row.contains(PROMPT)),
        "the future Chat received the self-contained prompt"
    );
}

#[tokio::test]
async fn schedule_routine_mounts_on_main_chat_turns_not_planning_turns() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("ordinary reply"),
        ScriptedReply::text("planning reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "ordinary").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[0]
            .tool_names
            .contains(&"schedule_routine".into())
    );

    engine
        .handle(methods::ENTER_PLAN_MODE, json!({ "chatId": "chat-1" }))
        .await
        .unwrap();
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "plan it").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_requests(&provider, 2).await;
    assert!(
        !provider.requests()[1]
            .tool_names
            .contains(&"schedule_routine".into())
    );
}
