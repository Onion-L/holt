//! Handle-seam tests for the `todo` task list (issue #47): a scripted
//! Turn writes the list twice and the transcript keeps ONE checklist card,
//! rewritten in place and persisted; a cap hit is a typed error the model
//! reads, not a silent truncation; and the tool mounts on main-chat Turns
//! only (the MCP policy).

use crate::common;

use common::{Fixture, ScriptedProvider, ScriptedReply, run_prompt, wait_for_requests};
use holt_rpc::{RpcService, methods};

/// Every part of the assistant entries in the chat's current transcript.
async fn assistant_parts(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
) -> Vec<serde_json::Value> {
    let snapshot = common::transcript_snapshot(engine, chat_id).await;
    snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["role"] == "assistant")
        .flat_map(|entry| entry["parts"].as_array().unwrap().clone())
        .collect()
}

fn todo_call(id: &str, items: serde_json::Value) -> ScriptedReply {
    ScriptedReply::tool_call(id, "todo", serde_json::json!({ "items": items }))
}

#[tokio::test]
async fn todo_writes_share_one_card_that_settles_on_the_last_write() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        todo_call(
            "todo-1",
            serde_json::json!([
                { "title": "Map the fold path", "status": "in_progress" },
                { "title": "Ship it", "status": "pending" }
            ]),
        ),
        todo_call(
            "todo-2",
            serde_json::json!([
                { "title": "Map the fold path", "status": "done" },
                { "title": "Ship it", "status": "in_progress" },
                { "title": "Cancel the footer rework", "status": "cancelled" }
            ]),
        ),
        ScriptedReply::text("all set"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "track this work").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Three rounds: write, rewrite, closing reply. The model advertised
    // the tool from the first request on, and read each ack back.
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].tool_names.contains(&"todo".into()));
    let round3 = common::summarize(&requests[2].messages).join("\n");
    assert!(round3.contains("toolresult:todo-1:Task list updated — 0 of 2 done, 1 in progress."));
    assert!(
        round3.contains(
            "toolresult:todo-2:Task list updated — 1 of 3 done, 1 in progress, 1 cancelled."
        ),
        "unexpected round 3: {round3}"
    );

    // The transcript: both calls appear as ordinary Todo chips, and ONE
    // card — landed where the first write put it, carrying the last
    // write's state.
    let parts = assistant_parts(&engine, "chat-1").await;
    let kinds: Vec<&str> = parts
        .iter()
        .map(|part| part["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["tool", "todoCard", "tool", "text"], "{kinds:?}");
    assert_eq!(parts[0]["id"], "todo-1");
    assert_eq!(parts[0]["call"]["kind"], "todo");
    assert_eq!(parts[0]["call"]["items"][0]["text"], "Map the fold path");
    assert_eq!(parts[0]["isError"], false);
    let card = &parts[1];
    assert_eq!(card["id"], "todo-1-card");
    let items = card["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["status"], "done");
    assert_eq!(items[1]["status"], "inProgress");
    assert_eq!(items[2]["status"], "cancelled");

    // The card is the persisted record: a restarted engine replays it with
    // the final state (the in-place rewrite's `part` line).
    drop(engine);
    let engine = fixture.engine(&provider);
    let parts = assistant_parts(&engine, "chat-1").await;
    let cards: Vec<&serde_json::Value> = parts
        .iter()
        .filter(|part| part["kind"] == "todoCard")
        .collect();
    assert_eq!(cards.len(), 1);
    let items = cards[0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[1]["status"], "inProgress");
}

#[tokio::test]
async fn a_cap_hit_is_an_error_result_and_lands_no_card() {
    let fixture = Fixture::new();
    let fat = (0..21)
        .map(|i| serde_json::json!({ "title": format!("task {i}"), "status": "pending" }))
        .collect::<Vec<_>>();
    let provider = ScriptedProvider::new(vec![
        todo_call("todo-1", serde_json::Value::Array(fat)),
        ScriptedReply::text("trimmed my ambitions"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "track everything").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The model reads the typed error and recovers; no card ever lands.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let round2 = common::summarize(&requests[1].messages).join("\n");
    assert!(
        round2.contains("toolresult:todo-1:") && round2.contains("at most 20 items"),
        "unexpected round 2: {round2}"
    );
    let parts = assistant_parts(&engine, "chat-1").await;
    assert!(parts.iter().all(|part| part["kind"] != "todoCard"));
    let chip = parts
        .iter()
        .find(|part| part["id"] == "todo-1")
        .expect("the call still appears as a chip");
    assert_eq!(chip["isError"], true);
}

#[tokio::test]
async fn the_todo_tool_mounts_on_main_chat_turns_only() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("ordinary reply"),
        ScriptedReply::text("planning reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "ordinary").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(provider.requests()[0].tool_names.contains(&"todo".into()));

    // A planning Turn keeps its read-only surface — no task list (the MCP
    // policy: main-chat Turns only).
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan it").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    wait_for_requests(&provider, 2).await;
    assert!(!provider.requests()[1].tool_names.contains(&"todo".into()));
}
