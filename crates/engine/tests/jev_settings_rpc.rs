//! Jev settings RPCs (ADR-0026): the `jev.json` record's
//! get/save/reveal/remove surface, masked-get semantics, and empty-key
//! validation, driven end to end through `RpcService::handle`.

use common::{Fixture, ScriptedProvider};
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::{Value, json};

mod common;

async fn handle(engine: &LocalEngine, method: &str, params: Value) -> Result<Value, String> {
    match engine.handle(method, params).await {
        Ok(RpcReply::Value(value)) => Ok(value),
        Ok(_) => Err("unexpected reply kind".into()),
        Err(error) => Err(error.to_string()),
    }
}

async fn value(engine: &LocalEngine, method: &str, params: Value) -> Value {
    handle(engine, method, params).await.expect("a value reply")
}

#[tokio::test]
async fn an_unconfigured_engine_reports_empty_state() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let state = value(&engine, methods::GET_JEV_SETTINGS, json!({})).await;
    assert_eq!(state["apiKeyMasked"], json!(null));

    let revealed = value(&engine, methods::REVEAL_JEV_KEY, json!({})).await;
    assert_eq!(revealed["key"], json!(null));
}

#[tokio::test]
async fn save_replies_the_masked_state_and_persists_the_record() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let saved = value(
        &engine,
        methods::SAVE_JEV_SETTINGS,
        json!({ "apiKey": " sk-abcdefgh1234 " }),
    )
    .await;
    // The key is trimmed on save; the state carries only the mask.
    assert_eq!(saved["apiKeyMasked"], json!("sk-a…1234"));

    let state = value(&engine, methods::GET_JEV_SETTINGS, json!({})).await;
    assert_eq!(state, saved);

    let revealed = value(&engine, methods::REVEAL_JEV_KEY, json!({})).await;
    assert_eq!(revealed["key"], json!("sk-abcdefgh1234"));

    // The record survives a restart.
    drop(engine);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_JEV_SETTINGS, json!({})).await;
    assert_eq!(state["apiKeyMasked"], json!("sk-a…1234"));
}

#[tokio::test]
async fn a_short_key_masks_to_nothing() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    for key in ["12345678", "short"] {
        let saved = value(
            &engine,
            methods::SAVE_JEV_SETTINGS,
            json!({ "apiKey": key }),
        )
        .await;
        assert_eq!(saved["apiKeyMasked"], json!("…"), "key {key}");
    }
}

#[tokio::test]
async fn save_refuses_an_empty_key() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let error = handle(
        &engine,
        methods::SAVE_JEV_SETTINGS,
        json!({ "apiKey": "   " }),
    )
    .await
    .unwrap_err();
    // The RPC layer refuses a blank key as a bad param, mirroring the
    // web-search flow; the store-level empty refusal is module-tested.
    assert!(error.contains("apiKey is required"), "unexpected: {error}");

    // Nothing moved: the record stays unconfigured.
    let state = value(&engine, methods::GET_JEV_SETTINGS, json!({})).await;
    assert_eq!(state["apiKeyMasked"], json!(null));
}

#[tokio::test]
async fn remove_clears_the_state_and_deletes_the_file() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    value(
        &engine,
        methods::SAVE_JEV_SETTINGS,
        json!({ "apiKey": "sk-abcdefgh1234" }),
    )
    .await;

    let removed = value(&engine, methods::REMOVE_JEV_SETTINGS, json!({})).await;
    assert_eq!(removed, json!({}));

    let state = value(&engine, methods::GET_JEV_SETTINGS, json!({})).await;
    assert_eq!(state["apiKeyMasked"], json!(null));
    let revealed = value(&engine, methods::REVEAL_JEV_KEY, json!({})).await;
    assert_eq!(revealed["key"], json!(null));
    assert!(!fixture.data_dir.path().join("jev.json").exists());
}

#[tokio::test]
async fn a_corrupt_record_fails_engine_startup() {
    let fixture = Fixture::new();
    let path = fixture.data_dir.path().join("jev.json");
    std::fs::write(&path, b"{broken").unwrap();

    let error = match LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: None,
        search_backend_resolver: None,
        jev_judge_resolver: None,
    }) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a corrupt jev record must fail engine startup"),
    };
    assert!(
        error.contains("jev.json is malformed"),
        "unexpected: {error}"
    );
    // Loud, but non-destructive: the file waits for manual repair.
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
}
