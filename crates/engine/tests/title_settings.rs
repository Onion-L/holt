//! Title settings end to end (automatic-chat-titles ticket 02): the
//! engine-owned record reads as defaults before anything is saved, saves
//! validate the provider-qualified model and the instruction bounds,
//! missing credentials surface as a warning that never blocks a Turn, and
//! the record survives a restart while a corrupt file falls back to
//! defaults.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcError, RpcReply, RpcService as _, methods};

fn reassemble(fixture: &common::Fixture) -> LocalEngine {
    LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: None,
    })
    .unwrap()
}

async fn get_settings(engine: &LocalEngine) -> serde_json::Value {
    let RpcReply::Value(value) = engine
        .handle(methods::GET_TITLE_SETTINGS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("GetTitleSettings did not return a value");
    };
    value
}

async fn save_settings(
    engine: &LocalEngine,
    model_id: serde_json::Value,
    instruction: &str,
) -> Result<serde_json::Value, RpcError> {
    let RpcReply::Value(value) = engine
        .handle(
            methods::SAVE_TITLE_SETTINGS,
            serde_json::json!({ "modelId": model_id, "instruction": instruction }),
        )
        .await?
    else {
        panic!("SaveTitleSettings did not return a value");
    };
    Ok(value)
}

#[tokio::test]
async fn title_settings_read_as_defaults_before_anything_is_saved() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);

    let state = get_settings(&engine).await;
    assert_eq!(state["settings"]["modelId"], serde_json::Value::Null);
    assert_eq!(
        state["settings"]["instruction"],
        holt_proto::DEFAULT_TITLE_INSTRUCTION
    );
    assert_eq!(state["warning"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_valid_save_round_trips_and_missing_credentials_become_a_warning() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);

    // No openai key saved: the model resolves, the warning reports the gap.
    let state = save_settings(&engine, serde_json::json!("openai/gpt-5.4"), "name it")
        .await
        .unwrap();
    assert_eq!(state["settings"]["modelId"], "openai/gpt-5.4");
    assert_eq!(state["settings"]["instruction"], "name it");
    let warning = state["warning"].as_str().expect("credential warning");
    assert!(warning.contains("openai"), "warning names the provider");

    let state = get_settings(&engine).await;
    assert_eq!(state["settings"]["modelId"], "openai/gpt-5.4");
    assert!(state["warning"].is_string());

    // Saving the key clears the warning on the next read.
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    let state = get_settings(&engine).await;
    assert_eq!(state["warning"], serde_json::Value::Null);
}

#[tokio::test]
async fn unresolvable_models_and_bad_instructions_are_rejected_without_touching_state() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);

    let result = save_settings(&engine, serde_json::json!("openai/gpt-nope"), "name it").await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));
    let result = save_settings(
        &engine,
        serde_json::json!("nosuchprovider/model"),
        "name it",
    )
    .await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));
    let result = save_settings(&engine, serde_json::json!("gpt-5.4"), "name it").await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));
    let result = save_settings(&engine, serde_json::Value::Null, "   ").await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));
    let too_long = "x".repeat(2001);
    let result = save_settings(&engine, serde_json::Value::Null, &too_long).await;
    assert!(matches!(result, Err(RpcError::BadParams(_))));

    // Every rejection left the persisted record at its defaults.
    let state = get_settings(&engine).await;
    assert_eq!(state["settings"]["modelId"], serde_json::Value::Null);
    assert_eq!(
        state["settings"]["instruction"],
        holt_proto::DEFAULT_TITLE_INSTRUCTION
    );
}

#[tokio::test]
async fn an_empty_model_id_disables_automatic_titles() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);

    save_settings(&engine, serde_json::json!("openai/gpt-5.4"), "name it")
        .await
        .unwrap();
    let state = save_settings(&engine, serde_json::json!("  "), "name it")
        .await
        .unwrap();
    assert_eq!(state["settings"]["modelId"], serde_json::Value::Null);
    assert_eq!(state["warning"], serde_json::Value::Null);
}

#[tokio::test]
async fn saved_settings_survive_a_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    save_settings(&engine, serde_json::json!("openai/gpt-5.4"), "name it")
        .await
        .unwrap();
    drop(engine);

    let engine = reassemble(&fixture);
    let state = get_settings(&engine).await;
    assert_eq!(state["settings"]["modelId"], "openai/gpt-5.4");
    assert_eq!(state["settings"]["instruction"], "name it");
}

#[tokio::test]
async fn a_corrupt_settings_file_loads_as_defaults() {
    let fixture = common::Fixture::new();
    std::fs::write(
        fixture.data_dir.path().join("title-settings.json"),
        "{broken",
    )
    .unwrap();

    let engine = reassemble(&fixture);
    let state = get_settings(&engine).await;
    assert_eq!(state["settings"]["modelId"], serde_json::Value::Null);
    assert_eq!(
        state["settings"]["instruction"],
        holt_proto::DEFAULT_TITLE_INSTRUCTION
    );
}

#[tokio::test]
async fn a_credential_warning_does_not_block_a_normal_turn() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("scripted reply text")]);
    let engine = fixture.engine(&provider);

    // The title model resolves (a custom id on a real provider) but that
    // provider has no key — a warning, not an error.
    engine
        .handle(
            methods::ADD_PROVIDER_MODEL,
            serde_json::json!({ "providerId": "anthropic", "modelId": "claude-fake" }),
        )
        .await
        .unwrap();
    let state = save_settings(
        &engine,
        serde_json::json!("anthropic/claude-fake"),
        "name it",
    )
    .await
    .unwrap();
    assert!(state["warning"].is_string());

    // The openai Turn runs unaffected.
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "scripted reply text").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
}
