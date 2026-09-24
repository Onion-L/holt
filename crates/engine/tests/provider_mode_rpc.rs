//! Provider Mode (ADR-0037) at the RPC seam: the mode flag on the chat row,
//! its mutual exclusion with Plan Mode, and the Turn shaping — a mode Turn
//! carries the workspace prompt plus the mode block and only the web and
//! catalog tools; a normal Turn mounts neither catalog tool.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply, run_prompt, wait_for_requests};
use holt_rpc::{RpcReply, RpcService, methods};

async fn call(engine: &holt_engine::LocalEngine, method: &str, chat_id: &str) -> serde_json::Value {
    let RpcReply::Value(state) = engine
        .handle(method, serde_json::json!({ "chatId": chat_id }))
        .await
        .unwrap()
    else {
        panic!("{method} did not return a value");
    };
    state
}

async fn provider_active(engine: &holt_engine::LocalEngine, chat_id: &str) -> bool {
    call(engine, methods::GET_PROVIDER_MODE, chat_id).await["active"] == true
}

async fn plan_active(engine: &holt_engine::LocalEngine, chat_id: &str) -> bool {
    call(engine, methods::GET_PLAN_MODE, chat_id).await["active"] == true
}

#[tokio::test]
async fn enter_exit_round_trip_and_survive_restart() {
    let fixture = Fixture::new();
    let config_dir = fixture.data_dir.path().to_path_buf();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;
    assert!(!provider_active(&engine, "chat-1").await);

    let state = call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    assert_eq!(state["active"], true);
    // Idempotent.
    let state = call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    assert_eq!(state["active"], true);
    let state = call(&engine, methods::EXIT_PROVIDER_MODE, "chat-1").await;
    assert_eq!(state["active"], false);
    let state = call(&engine, methods::EXIT_PROVIDER_MODE, "chat-1").await;
    assert_eq!(state["active"], false);

    call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    drop(engine);
    let provider = ScriptedProvider::new(vec![]);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: config_dir,
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    assert!(provider_active(&engine, "chat-1").await);
    assert_eq!(provider.requests().len(), 0);
}

#[tokio::test]
async fn unknown_chats_fail_the_provider_mode_rpcs() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    for method in [
        methods::ENTER_PROVIDER_MODE,
        methods::EXIT_PROVIDER_MODE,
        methods::GET_PROVIDER_MODE,
    ] {
        assert!(
            engine
                .handle(method, serde_json::json!({ "chatId": "nope" }))
                .await
                .is_err(),
            "{method} must reject an unknown chat"
        );
    }
}

#[tokio::test]
async fn plan_and_provider_mode_exclude_each_other() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;

    call(&engine, methods::ENTER_PLAN_MODE, "chat-1").await;
    call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    assert!(provider_active(&engine, "chat-1").await);
    assert!(!plan_active(&engine, "chat-1").await);

    call(&engine, methods::ENTER_PLAN_MODE, "chat-1").await;
    assert!(plan_active(&engine, "chat-1").await);
    assert!(!provider_active(&engine, "chat-1").await);
}

#[tokio::test]
async fn a_mode_turn_carries_the_block_and_only_the_catalog_surface() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("ordinary reply"),
        ScriptedReply::text("mode reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "ordinary").await;
    wait_for_requests(&provider, 1).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "add a provider").await;
    wait_for_requests(&provider, 2).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let (ordinary, mode) = (&requests[0], &requests[1]);
    for name in ["model_proposal", "request_provider_key"] {
        assert!(
            !ordinary.tool_names.iter().any(|tool| tool == name),
            "a normal Turn must not mount {name}"
        );
    }
    let mut names = mode.tool_names.clone();
    names.sort();
    // No search backend on the fixture: web_search is never mounted.
    assert_eq!(
        names,
        ["model_proposal", "request_provider_key", "web_fetch"]
    );
    let ordinary_prompt = ordinary.system_prompt.clone().unwrap();
    let mode_prompt = mode.system_prompt.clone().unwrap();
    assert!(
        mode_prompt.starts_with(&ordinary_prompt),
        "the mode block appends to the workspace prompt"
    );
    assert!(mode_prompt.contains("## Provider Mode (active)"));
    assert!(!ordinary_prompt.contains("Provider Mode"));
}

#[tokio::test]
async fn exiting_mid_turn_leaves_the_running_turn_alone() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "held reply"),
        ScriptedReply::text("after exit"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "add a provider").await;
    wait_for_requests(&provider, 1).await;
    call(&engine, methods::EXIT_PROVIDER_MODE, "chat-1").await;
    gate.notify_one();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_transcript_text(&mut transcript, "held reply").await;
    assert_eq!(provider.requests().len(), 1);
    assert!(
        provider.requests()[0]
            .tool_names
            .iter()
            .any(|name| name == "model_proposal")
    );

    // The next Turn is admitted outside the mode.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "thanks").await;
    wait_for_requests(&provider, 2).await;
    assert!(
        !provider.requests()[1]
            .tool_names
            .iter()
            .any(|name| name == "model_proposal")
    );
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
}
