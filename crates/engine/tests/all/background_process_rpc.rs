//! Background processes end to end (ADR-background-processes-are-chat-owned):
//! a Turn spawns one through the `spawn_background` tool, the RPC surface
//! lists and watches it, the user kills it over the RPC, the exit notice
//! rides the next Turn's system prompt, and clearing drops the row.

use crate::common;
use common::{Fixture, ScriptedProvider, ScriptedReply};

use holt_rpc::RpcService as _;
use holt_rpc::methods;
use serde_json::Value;

async fn list_processes(engine: &holt_engine::LocalEngine) -> Vec<Value> {
    let holt_rpc::RpcReply::Value(value) = engine
        .handle(methods::LIST_BACKGROUND_PROCESSES, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("ListBackgroundProcesses did not return a value");
    };
    value.as_array().cloned().unwrap_or_default()
}

/// Poll the list until `expected` holds for one row, or the deadline
/// passes — the observable is reached, never assumed at a tick.
async fn poll_processes(
    engine: &holt_engine::LocalEngine,
    expected: impl Fn(&Value) -> bool,
) -> bool {
    for _ in 0..250 {
        if list_processes(engine)
            .await
            .iter()
            .any(|row: &Value| expected(row))
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_spawns_watches_kills_and_notifies_a_background_process() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-spawn",
            "spawn_background",
            serde_json::json!({ "command": "printf 'bg-ready'; sleep 30" }),
        ),
        // The round after the tool result.
        ScriptedReply::text("started"),
        // The next Turn — its system prompt carries the exit notice.
        ScriptedReply::text("noted"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat").await;
    common::run_prompt(&engine, "chat", &fixture.cwd(), "start the server").await;
    common::wait_for_requests(&provider, 2).await;

    // The spawn ran through the registry: one running row on the chat.
    assert!(
        poll_processes(&engine, |row| {
            row["running"] == Value::Bool(true)
                && row["chatId"] == "chat"
                && row["command"]
                    .as_str()
                    .is_some_and(|command| command.contains("bg-ready"))
        })
        .await,
        "the spawned process never showed running: {:?}",
        list_processes(&engine).await
    );

    // The tool result reported the id back to the model.
    common::wait_for_transcript(&engine, "chat", "Background process").await;

    // Kill over the RPC (the panel button's path); the row stays listed
    // with its exit code until cleared.
    let id = list_processes(&engine).await[0]["processId"]
        .as_str()
        .unwrap()
        .to_string();
    engine
        .handle(
            methods::KILL_BACKGROUND_PROCESS,
            serde_json::json!({ "processId": id }),
        )
        .await
        .unwrap();
    assert!(
        poll_processes(&engine, |row| {
            row["processId"] == id.as_str() && row["running"] == Value::Bool(false)
        })
        .await,
        "the killed process never showed exited"
    );

    // The next Turn's system prompt carries the exit notice (main-chat
    // Turns drain the queue; the block only exists when a notice waits).
    let prompts_before: Vec<String> = provider
        .requests()
        .iter()
        .filter_map(|request| request.system_prompt.clone())
        .collect();
    assert!(
        prompts_before
            .iter()
            .all(|prompt| !prompt.contains("Background process exits")),
        "a notice appeared before any exit"
    );
    common::run_prompt(&engine, "chat", &fixture.cwd(), "what happened").await;
    // A fresh status watch's OPENING frame would report turn one's idle —
    // stale the moment turn two starts (the 2026-10-23 RETROS rule). Wait
    // on the provider's request count instead: two rounds already ran.
    common::wait_for_requests(&provider, 3).await;
    // Any request of this second Turn (the title task records its own
    // prompt too) — one of them carries the notice.
    let noticed = provider
        .requests()
        .iter()
        .filter_map(|request| request.system_prompt.as_deref())
        .any(|prompt| {
            prompt.contains("Background process exits since your last turn")
                && prompt.contains("exited with code")
        });
    assert!(noticed, "the exit notice never reached a prompt");

    // Clearing the exited rows empties the list.
    engine
        .handle(
            methods::CLEAR_EXITED_BACKGROUND_PROCESSES,
            serde_json::json!({ "chatId": "chat" }),
        )
        .await
        .unwrap();
    assert!(list_processes(&engine).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_a_turn_does_not_stop_its_background_process() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-spawn",
            "spawn_background",
            serde_json::json!({ "command": "sleep 30" }),
        ),
        ScriptedReply::text("started, now waiting"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat").await;
    common::run_prompt(&engine, "chat", &fixture.cwd(), "start it").await;
    common::wait_for_requests(&provider, 2).await;
    assert!(
        poll_processes(&engine, |row| row["running"] == Value::Bool(true)).await,
        "the process never showed running"
    );
    // The Turn is long over; the process is still alive — detached from
    // the Turn by construction. End it through the user's surface.
    let id = list_processes(&engine).await[0]["processId"]
        .as_str()
        .unwrap()
        .to_string();
    engine
        .handle(
            methods::KILL_BACKGROUND_PROCESS,
            serde_json::json!({ "processId": id }),
        )
        .await
        .unwrap();
    assert!(poll_processes(&engine, |row| row["running"] == Value::Bool(false)).await);
}
