//! The automatic Title task (automatic-chat-titles ticket 03): the first
//! prompt keeps its first-line fallback immediately while one independent
//! background request asks the configured title model for a better name. A
//! valid short result replaces the fallback through the normal chat watch
//! and persists; every failure mode — disabled settings, provider errors,
//! empty output, missing credentials, deletion, interruption, restart —
//! leaves the chat safe and quiet.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt;
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService as _, methods};
use std::sync::Arc;

const INSTRUCTION: &str = holt_proto::DEFAULT_TITLE_INSTRUCTION;

fn reassemble(fixture: &common::Fixture) -> LocalEngine {
    LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: None,
        search_backend_resolver: None,
    })
    .unwrap()
}

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

async fn chats_snapshot(engine: &LocalEngine) -> serde_json::Value {
    let mut chats = open_chats_watch(engine).await;
    chats.next().await.expect("chats watch ended")
}

/// Save title settings selecting a resolvable model (openai's key is saved
/// by `setup_chat`).
async fn save_title_model(engine: &LocalEngine) {
    engine
        .handle(
            methods::SAVE_TITLE_SETTINGS,
            serde_json::json!({ "modelId": "openai/gpt-5.4", "instruction": INSTRUCTION }),
        )
        .await
        .unwrap();
}

/// The recorded requests that were Title-task requests.
fn title_requests(provider: &ScriptedProvider) -> Vec<common::RecordedRequest> {
    provider
        .requests()
        .into_iter()
        .filter(|request| request.system_prompt.as_deref() == Some(INSTRUCTION))
        .collect()
}

fn first_user_text(request: &common::RecordedRequest) -> String {
    match &request.messages[0] {
        pi_core::ai::types::Message::User(user) => match &user.content {
            pi_core::ai::types::UserContent::Text(text) => text.clone(),
            pi_core::ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .find_map(|block| match block {
                    pi_core::ai::types::BlockContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
        },
        _ => String::new(),
    }
}

/// Pump the chats watch until chat `chat_id` carries `expected` as its title.
async fn wait_for_title<S>(chats: &mut S, chat_id: &str, expected: &str) -> serde_json::Value
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    loop {
        let frame = common::next_frame(chats).await;
        let hit = frame.as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["id"] == chat_id && row["title"] == expected)
        });
        if hit {
            return frame;
        }
    }
}

#[tokio::test]
async fn the_first_prompt_falls_back_then_a_valid_title_replaces_and_persists() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::text("A Better Title")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    let mut chats = open_chats_watch(&engine).await;
    let (transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello world").await;

    let frame = wait_for_title(&mut chats, "chat-1", "A Better Title").await;
    let row = &frame[0];
    // The source stays automatic — the task's own result may still be
    // superseded by a later manual rename.
    assert_eq!(row["titleSource"], "automatic");
    assert_eq!(row["titleTaskStarted"], true);
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The request carried only the instruction and the first prompt.
    let titles = title_requests(&provider);
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0].tools, 0);
    assert_eq!(titles[0].messages.len(), 1);
    assert_eq!(first_user_text(&titles[0]), "hello world");

    // No Transcript or History pollution from the auxiliary request.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("A Better Title"));
    drop(transcript);
    let history = std::fs::read_to_string(fixture.data_dir.path().join("history/chat-1.jsonl"))
        .expect("history record for a completed turn");
    assert!(!history.contains("A Better Title"));
    assert!(!history.contains(INSTRUCTION));

    drop(chats);
    drop(engine);
    let engine = reassemble(&fixture);
    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "A Better Title");
    assert_eq!(frame[0]["titleTaskStarted"], true);
}

#[tokio::test]
async fn disabled_settings_start_no_task() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    assert!(title_requests(&provider).is_empty());
    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "hello");
    assert_eq!(frame[0]["titleTaskStarted"], false);
}

#[tokio::test]
async fn a_failed_title_request_keeps_the_fallback_quietly() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::Failed("boom".into())]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_requests(&provider, 2).await;
    // Give the finished title task a beat to (not) write.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "hello");
    // The failure never surfaces in the conversation.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("boom"));
}

#[tokio::test]
async fn an_empty_title_reply_keeps_the_fallback() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::text("   ")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_requests(&provider, 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "hello");
}

#[tokio::test]
async fn title_output_is_flattened_and_capped() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("reply one"),
        ScriptedReply::text("reply two"),
    ])
    .with_title_script(
        INSTRUCTION,
        vec![
            ScriptedReply::text("  first line\nsecond line  "),
            ScriptedReply::text("y".repeat(100)),
        ],
    );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::setup_chat(&engine, "chat-2").await;
    save_title_model(&engine).await;

    let mut chats = open_chats_watch(&engine).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "prompt one").await;
    wait_for_title(&mut chats, "chat-1", "first line second line").await;
    common::run_prompt(&engine, "chat-2", &fixture.cwd(), "prompt two").await;
    wait_for_title(&mut chats, "chat-2", &"y".repeat(60)).await;
}

#[tokio::test]
async fn a_manual_rename_wins_over_a_late_title_result() {
    let fixture = common::Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")]).with_title_script(
        INSTRUCTION,
        vec![ScriptedReply::gated(gate.clone(), "Late Title")],
    );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_requests(&provider, 2).await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "renameChat", "chatId": "chat-1", "title": "My name" }),
        )
        .await
        .unwrap();
    gate.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "My name");
    assert_eq!(frame[0]["titleSource"], "userManual");
}

#[tokio::test]
async fn deleting_the_chat_discards_a_pending_title_result() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::Silent]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_requests(&provider, 2).await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    // The cancellation lands; the engine stays healthy and silent.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    engine
        .handle(methods::GET_TITLE_SETTINGS, serde_json::json!({}))
        .await
        .unwrap();

    let frame = chats_snapshot(&engine).await;
    assert!(frame.as_array().unwrap().is_empty());
    drop(engine);
    let engine = reassemble(&fixture);
    let frame = chats_snapshot(&engine).await;
    assert!(frame.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn interrupting_the_turn_does_not_cancel_the_title_task() {
    let fixture = common::Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent]).with_title_script(
        INSTRUCTION,
        vec![ScriptedReply::gated(gate.clone(), "Calm Title")],
    );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    let mut chats = open_chats_watch(&engine).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_requests(&provider, 2).await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": "chat-1", "command": { "kind": "interrupt" } }),
        )
        .await
        .unwrap();
    // Let the turn's cancellation propagate (the scripted turn hangs, so
    // the session may never settle — irrelevant to the title task).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    gate.notify_one();
    let frame = wait_for_title(&mut chats, "chat-1", "Calm Title").await;
    assert_eq!(frame[0]["titleSource"], "automatic");
}

#[tokio::test]
async fn later_prompts_start_no_second_task() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("reply one"),
        ScriptedReply::text("reply two"),
    ])
    .with_title_script(INSTRUCTION, vec![ScriptedReply::text("Title One")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    let mut chats = open_chats_watch(&engine).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_title(&mut chats, "chat-1", "Title One").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    common::wait_for_requests(&provider, 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(title_requests(&provider).len(), 1);
    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "Title One");
}

#[tokio::test]
async fn a_started_task_is_not_retried_after_a_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::Silent]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_requests(&provider, 2).await;
    drop(engine);

    // The restart assembles with the title model still configured; nothing
    // re-fires — the fallback stays and the started marker persists.
    let engine = reassemble(&fixture);
    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "hello");
    assert_eq!(frame[0]["titleTaskStarted"], true);
}

#[tokio::test]
async fn an_existing_user_prompt_blocks_title_generation_after_reload() {
    let fixture = common::Fixture::new();
    let legacy_row = serde_json::json!({
        "id": "chat-1",
        "deviceId": "dev",
        "title": null,
        "titleSource": "automatic",
        "titleTaskStarted": false,
        "archived": false,
        "cwd": null,
        "branch": null,
        "checkoutId": null,
        "config": null,
        "lastMessagePreview": null,
        "lastMessageAt": null,
        "createdAt": "2026-01-01T00:00:00Z"
    });
    std::fs::write(
        fixture.data_dir.path().join("chats.json"),
        serde_json::to_vec(&serde_json::json!([legacy_row])).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(fixture.data_dir.path().join("transcripts")).unwrap();
    std::fs::write(
        fixture.data_dir.path().join("transcripts/chat-1.json"),
        serde_json::to_vec(&vec![serde_json::json!({
            "id": "user-1",
            "role": "user",
            "parts": [{ "id": "t0", "kind": "text", "text": "old prompt" }],
            "createdAt": 1,
            "deviceId": "dev"
        })])
        .unwrap(),
    )
    .unwrap();

    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::text("Should Not Appear")]);
    let engine = LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    save_title_model(&engine).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "new prompt").await;
    common::wait_for_requests(&provider, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(title_requests(&provider).is_empty());
}

#[tokio::test]
async fn a_manually_titled_chat_never_starts_a_task() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("reply")])
        .with_title_script(INSTRUCTION, vec![ScriptedReply::text("Should Not Appear")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    save_title_model(&engine).await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "renameChat", "chatId": "chat-1", "title": "mine" }),
        )
        .await
        .unwrap();

    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    assert!(title_requests(&provider).is_empty());
    let frame = chats_snapshot(&engine).await;
    assert_eq!(frame[0]["title"], "mine");
    assert_eq!(frame[0]["titleTaskStarted"], false);
}
