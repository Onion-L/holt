//! Permission-mode lifecycle at the RPC seam (ADR-0014, issue 02): the
//! per-chat mode round-trips through `Mutate setChatPermissionMode` and the
//! chat watch payload, new chats inherit the engine-owned sticky default,
//! and both the chat's mode and the default survive an engine restart.

use holt_engine::LocalEngine;
use holt_rpc::{RpcReply, RpcService, methods};

mod common;

use common::{Fixture, ScriptedProvider, next_frame, wait_for_session_status};

async fn create_chat_with_config(engine: &LocalEngine, chat_id: &str, mode: &str) {
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": chat_id,
                "config": {
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                    "reasoning": null,
                    "modelOptions": {},
                    "permissionMode": mode,
                }
            }),
        )
        .await
        .unwrap();
}

async fn switch_mode(engine: &LocalEngine, chat_id: &str, mode: &str) {
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

/// The mode a chat's watch row currently carries, read straight off a
/// `WatchChats` subscription.
async fn watched_mode(engine: &LocalEngine, chat_id: &str) -> String {
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
        .unwrap_or_else(|| panic!("chat {chat_id} missing from watch frame"))
        ["config"]["permissionMode"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn first_launch_defaults_new_chats_to_confirm_changes() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(Vec::new()));

    // Even a creating client that sends another tier is overridden: the
    // first-launch default is engine-owned, read at chat creation.
    create_chat_with_config(&engine, "chat-1", "full-access").await;
    assert_eq!(watched_mode(&engine, "chat-1").await, "confirm-changes");
}

#[tokio::test]
async fn a_switched_mode_is_visible_and_inherited_by_new_chats() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(Vec::new()));

    create_chat_with_config(&engine, "chat-1", "confirm-changes").await;
    switch_mode(&engine, "chat-1", "full-access").await;
    assert_eq!(watched_mode(&engine, "chat-1").await, "full-access");

    // A NEW chat inherits the last mode used on the device — whatever its
    // own creating payload said.
    create_chat_with_config(&engine, "chat-2", "confirm-changes").await;
    assert_eq!(watched_mode(&engine, "chat-2").await, "full-access");

    // Unknown chat is a loud error, not a silent no-op.
    let error = match engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": "missing",
                "mode": "auto-review",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("switching the mode of an unknown chat must fail"),
    };
    assert!(error.to_string().contains("unknown chat"));
}

#[tokio::test]
async fn switched_mode_and_sticky_default_survive_restart() {
    let fixture = Fixture::new();
    {
        let engine = fixture.engine(&ScriptedProvider::new(Vec::new()));
        create_chat_with_config(&engine, "chat-1", "confirm-changes").await;
        switch_mode(&engine, "chat-1", "auto-review").await;
    }

    // The chat's config persisted (chats.json) and the sticky default
    // persisted (its own record): a restarted engine serves the switched
    // mode back and still hands it to new chats.
    let engine = fixture.engine(&ScriptedProvider::new(Vec::new()));
    assert_eq!(watched_mode(&engine, "chat-1").await, "auto-review");
    create_chat_with_config(&engine, "chat-2", "confirm-changes").await;
    assert_eq!(watched_mode(&engine, "chat-2").await, "auto-review");
}

#[tokio::test]
async fn a_whole_config_rewrite_keeps_the_stored_mode() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(Vec::new()));

    // A configured chat keeps its switched mode through a setChatConfig
    // whose payload carries a different tier…
    create_chat_with_config(&engine, "chat-1", "confirm-changes").await;
    switch_mode(&engine, "chat-1", "auto-review").await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatConfig",
                "chatId": "chat-1",
                "config": {
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                    "reasoning": null,
                    "modelOptions": {},
                    "permissionMode": "full-access",
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(watched_mode(&engine, "chat-1").await, "auto-review");

    // …and a config-less row inherits the sticky default through the same
    // rewrite instead of the writer's hardcoded tier (the model picker's
    // path — the row predates a first run).
    switch_mode(&engine, "chat-1", "full-access").await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": "chat-2" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatConfig",
                "chatId": "chat-2",
                "config": {
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                    "reasoning": null,
                    "modelOptions": {},
                    "permissionMode": "confirm-changes",
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(watched_mode(&engine, "chat-2").await, "full-access");
}

#[tokio::test]
async fn a_run_keeps_the_stored_mode_and_a_config_less_row_inherits_the_default() {
    let fixture = Fixture::new();
    // Two scripted replies: one Turn for the switched chat, one for the
    // config-less row whose first Turn seeds its config.
    let provider = ScriptedProvider::new(vec![
        common::ScriptedReply::text("done"),
        common::ScriptedReply::text("done too"),
    ]);
    let engine = fixture.engine(&provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    // A chat WITH a stored config: the switch moves the stored mode, and
    // the Turn below must keep it — the request carries the legacy
    // workspace-write default, so only the preservation rule can explain
    // the outcome.
    create_chat_with_config(&engine, "chat-1", "confirm-changes").await;
    switch_mode(&engine, "chat-1", "full-access").await;

    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The Turn records the run's config without moving the mode: the
    // request carried the legacy default and the stored full-access wins —
    // a switch takes effect from the NEXT Turn, never retroactively.
    assert_eq!(watched_mode(&engine, "chat-1").await, "full-access");

    // A row that never had a config (engine-side creation, no model picked
    // yet) inherits the sticky default on its first Turn, like a newly
    // created chat.
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": "chat-2" }),
        )
        .await
        .unwrap();
    let (_transcript2, mut sessions2) = common::subscribe(&engine, "chat-2").await;
    common::run_prompt(&engine, "chat-2", &fixture.cwd(), "hello again").await;
    wait_for_session_status(&mut sessions2, "chat-2", "idle").await;
    assert_eq!(watched_mode(&engine, "chat-2").await, "full-access");
}
