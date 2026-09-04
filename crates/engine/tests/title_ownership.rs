//! Title ownership and the rename contract (automatic-chat-titles ticket
//! 01): new chats start with an automatic title and no started Title task,
//! the first-line fallback keeps that state, `renameChat` stores the title,
//! locks it to user-manual (even when the text is unchanged) and publishes
//! the chat watch, and rows that predate the ownership fields load with the
//! safe user-manual default.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt;
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcError, RpcReply, RpcService as _, methods};

/// The whole-chat-list snapshot a fresh `WatchChats` subscription opens with.
async fn chats_snapshot(engine: &LocalEngine) -> serde_json::Value {
    let mut chats = open_chats_watch(engine).await;
    chats.next().await.expect("chats watch ended")
}

/// A `WatchChats` subscription whose opening frame is still pending.
async fn open_chats_watch(
    engine: &LocalEngine,
) -> impl StreamExt<Item = serde_json::Value> + Unpin + use<> {
    let RpcReply::Stream(chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    chats
}

/// Re-assemble an engine over the fixture's data dir without a provider —
/// the restart path for persistence assertions.
fn reassemble(fixture: &common::Fixture) -> LocalEngine {
    LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: None,
    })
    .unwrap()
}

async fn rename_chat(
    engine: &LocalEngine,
    chat_id: &str,
    title: &str,
) -> Result<RpcReply, RpcError> {
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "renameChat", "chatId": chat_id, "title": title }),
        )
        .await
}

#[tokio::test]
async fn a_new_chat_starts_with_an_automatic_title_and_no_started_task() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let frame = chats_snapshot(&engine).await;
    let row = &frame[0];
    assert_eq!(row["title"], serde_json::Value::Null);
    assert_eq!(row["titleSource"], "automatic");
    assert_eq!(row["titleTaskStarted"], false);
}

#[tokio::test]
async fn the_first_turn_keeps_the_capped_first_line_fallback_under_automatic_ownership() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let mut chats = open_chats_watch(&engine).await;
    let prompt = format!("{}\nsecond line", "x".repeat(80));
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), &prompt).await;

    let mut titled = None;
    while titled.is_none() {
        let frame = common::next_frame(&mut chats).await;
        if frame[0]["title"].is_string() {
            titled = Some(frame);
        }
    }
    let row = &titled.unwrap()[0];
    let title = row["title"].as_str().unwrap();
    assert_eq!(title, "x".repeat(60));
    assert_eq!(row["titleSource"], "automatic");
    assert_eq!(row["titleTaskStarted"], false);
}

#[tokio::test]
async fn rename_chat_locks_the_title_publishes_the_watch_and_survives_a_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;

    let mut chats = open_chats_watch(&engine).await;
    let _ = chats.next().await.expect("chats watch ended");
    rename_chat(&engine, "chat-1", "My chosen name")
        .await
        .unwrap();

    let frame = common::next_frame(&mut chats).await;
    assert_eq!(frame[0]["title"], "My chosen name");
    assert_eq!(frame[0]["titleSource"], "userManual");
    drop(chats);
    drop(engine);

    let engine = reassemble(&fixture);
    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "My chosen name");
    assert_eq!(frame[0]["titleSource"], "userManual");
    assert_eq!(frame[0]["titleTaskStarted"], false);
}

#[tokio::test]
async fn a_rename_identical_to_the_fallback_still_locks_the_title() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;

    rename_chat(&engine, "chat-1", "hello").await.unwrap();

    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "hello");
    assert_eq!(frame[0]["titleSource"], "userManual");
}

#[tokio::test]
async fn a_manual_rename_is_trimmed_and_capped_at_sixty_characters() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    rename_chat(&engine, "chat-1", &format!("  {}  ", "y".repeat(80)))
        .await
        .unwrap();

    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "y".repeat(60).as_str());
    assert_eq!(frame[0]["titleSource"], "userManual");
}

#[tokio::test]
async fn rename_chat_rejects_empty_ids_and_titles_without_touching_the_row() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let result = rename_chat(&engine, "  ", "name").await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));
    let result = rename_chat(&engine, "chat-1", "   ").await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));

    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], serde_json::Value::Null);
    assert_eq!(frame[0]["titleSource"], "automatic");
}

#[tokio::test]
async fn renaming_an_unknown_chat_is_a_silent_no_op() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let mut chats = open_chats_watch(&engine).await;
    let _ = chats.next().await.expect("chats watch ended");
    rename_chat(&engine, "ghost", "name").await.unwrap();

    let published = tokio::time::timeout(std::time::Duration::from_millis(200), chats.next()).await;
    assert!(
        published.is_err(),
        "an unknown-chat rename must not publish"
    );
}

#[tokio::test]
async fn legacy_rows_without_title_fields_load_as_user_manual() {
    let fixture = common::Fixture::new();
    let legacy_row = |id: &str, title: serde_json::Value| {
        serde_json::json!({
            "id": id,
            "deviceId": "dev",
            "title": title,
            "archived": false,
            "cwd": null,
            "branch": null,
            "checkoutId": null,
            "config": null,
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": "2026-01-01T00:00:00Z"
        })
    };
    std::fs::write(
        fixture.data_dir.path().join("chats.json"),
        serde_json::to_vec(&serde_json::json!([
            legacy_row("legacy-titled", serde_json::json!("Legacy name")),
            legacy_row("legacy-untitled", serde_json::Value::Null),
        ]))
        .unwrap(),
    )
    .unwrap();

    let engine = reassemble(&fixture);
    let frame = chats_snapshot(&engine).await;
    let rows = frame.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["titleSource"], "userManual");
        assert_eq!(row["titleTaskStarted"], false);
    }
    assert_eq!(rows[0]["title"], "Legacy name");
    assert_eq!(rows[1]["title"], serde_json::Value::Null);
}
