//! Plan Mode state RPC (ADR-0025, issue 01): entering, exiting, and
//! querying a chat's planning checkpoint through the public RPC surface —
//! the entry permission mode is recorded (never moved), the state
//! persists across restart, and recovery never starts a Turn.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, next_frame, run_prompt, wait_for_requests};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};

async fn plan_state(engine: &holt_engine::LocalEngine, chat_id: &str) -> serde_json::Value {
    let RpcReply::Value(state) = engine
        .handle(
            methods::GET_PLAN_MODE,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("GetPlanMode did not return a value");
    };
    state
}

async fn set_mode(engine: &holt_engine::LocalEngine, chat_id: &str, mode: &str) {
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": chat_id,
                "mode": mode,
            }),
        )
        .await
        .unwrap();
}

/// The stored permission mode of a chat row, read through the WatchChats
/// surface (the same frames the UI's mode chip renders).
async fn stored_mode(engine: &holt_engine::LocalEngine, chat_id: &str) -> serde_json::Value {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let frame = next_frame(&mut chats).await;
    frame
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == chat_id)
        .unwrap_or_else(|| panic!("chat {chat_id} missing from the watch"))
        .pointer("/config/permissionMode")
        .cloned()
        .expect("the chat carries a config with a permission mode")
}

#[tokio::test]
async fn enter_exit_and_query_round_trip() {
    let fixture = Fixture::new();
    // One seeded Turn gives the chat a stored config, so the mode switch
    // below lands on the chat row instead of only the sticky default.
    let provider = ScriptedProvider::new(vec![common::ScriptedReply::text("seed")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "seed").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // A fresh chat is not planning.
    let state = plan_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(false));
    assert_eq!(state.get("entryPermissionMode"), None);

    // Entering records the chat's current permission mode as the entry
    // mode. The stored mode itself does not move (Plan Mode is orthogonal
    // to the permission tiers, ADR-0025).
    set_mode(&engine, "chat-1", "auto-review").await;
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let state = plan_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    assert_eq!(
        state["entryPermissionMode"],
        serde_json::json!("auto-review")
    );
    assert_eq!(
        stored_mode(&engine, "chat-1").await,
        serde_json::json!("auto-review"),
        "entering Plan Mode must not move the stored permission mode"
    );

    // Exiting returns to inactive; the state query reflects it.
    engine
        .handle(
            methods::EXIT_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let state = plan_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(false));

    // Re-entering works and re-records the CURRENT mode: the user switched
    // to full-access while inactive, so that is the new entry mode.
    set_mode(&engine, "chat-1", "full-access").await;
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let state = plan_state(&engine, "chat-1").await;
    assert_eq!(
        state["entryPermissionMode"],
        serde_json::json!("full-access")
    );
}

#[tokio::test]
async fn entering_twice_is_idempotent_and_keeps_the_first_entry_mode() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;
    set_mode(&engine, "chat-1", "confirm-changes").await;
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    // A mode switch while planning is the user's own tier choice (the
    // picker still works); a second EnterPlanMode must not re-stamp the
    // entry mode from it.
    set_mode(&engine, "chat-1", "full-access").await;
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let state = plan_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    assert_eq!(
        state["entryPermissionMode"],
        serde_json::json!("confirm-changes"),
        "the entry mode is the mode active on FIRST entry"
    );
}

#[tokio::test]
async fn unknown_chats_fail_the_plan_mode_rpcs() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    for method in [
        methods::ENTER_PLAN_MODE,
        methods::EXIT_PLAN_MODE,
        methods::GET_PLAN_MODE,
    ] {
        let error = match engine
            .handle(method, serde_json::json!({ "chatId": "missing" }))
            .await
        {
            Ok(_) => panic!("{method} on an unknown chat must fail"),
            Err(error) => error,
        };
        assert!(
            matches!(error, RpcError::BadParams(_)),
            "{method} on an unknown chat must be a param error, got {error}"
        );
    }
}

#[tokio::test]
async fn entering_mid_turn_is_accepted_and_leaves_the_running_turn_alone() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![common::ScriptedReply::gated(
        gate.clone(),
        "held reply",
    )]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "investigate").await;
    wait_for_requests(&provider, 1).await;

    // The switch lands while the Turn runs: the RPC accepts it and the
    // state is visible at once, but the running Turn is left alone — no
    // second request, no interruption.
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    assert_eq!(
        plan_state(&engine, "chat-1").await["active"],
        serde_json::json!(true)
    );

    gate.notify_one();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_transcript_text(&mut transcript, "held reply").await;
    assert_eq!(
        provider.requests().len(),
        1,
        "entering Plan Mode mid-Turn must not inject a request into the running Turn"
    );
    assert_eq!(
        plan_state(&engine, "chat-1").await["active"],
        serde_json::json!(true)
    );
}

#[tokio::test]
async fn plan_mode_state_survives_restart_without_starting_a_turn() {
    let fixture = Fixture::new();
    let config_dir = fixture.data_dir.path().to_path_buf();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;
    set_mode(&engine, "chat-1", "auto-review").await;
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    drop(engine);

    // Reassembly restores the planning state — including the recorded
    // entry mode — and starts nothing: no session goes Working, and the
    // (absent) provider never sees a request.
    let provider = ScriptedProvider::new(vec![]);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: config_dir,
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    let RpcReply::Stream(mut sessions) = engine
        .handle(methods::WATCH_SESSIONS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSessions did not return a stream");
    };
    let frame = next_frame(&mut sessions).await;
    assert!(
        !frame
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["chatId"] == "chat-1" && row["status"] == "working"),
        "recovery must not auto-start a Turn: sessions were {frame}"
    );

    let state = plan_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    assert_eq!(
        state["entryPermissionMode"],
        serde_json::json!("auto-review")
    );
    assert_eq!(provider.requests().len(), 0);
}
