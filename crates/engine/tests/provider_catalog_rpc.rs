//! Live provider-catalog writes through the RPC surface (model setup, P1):
//! `SaveModelRecord` / `SetHiddenModels` / `SaveCustomProvider` /
//! `ResetProviderCatalog` operate on the settings layer only — every change
//! is visible immediately (ADR-0028: the boot catalog answers underneath,
//! the live layer on top, no restart).

mod common;

use common::{Fixture, ScriptedProvider};
use holt_engine::LocalEngine;
use holt_rpc::{RpcReply, RpcService, methods};

/// Keeps the fixture alive alongside the engine — dropping it would delete
/// the data dir mid-test.
fn setup() -> (Fixture, LocalEngine) {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    (fixture, engine)
}

async fn list_models(engine: &LocalEngine, provider: &str) -> Vec<serde_json::Value> {
    let RpcReply::Value(models) = engine
        .handle(
            methods::LIST_MODELS,
            serde_json::json!({ "providerId": provider }),
        )
        .await
        .unwrap()
    else {
        panic!("ListModels did not return a value");
    };
    models.as_array().unwrap().clone()
}

/// A complete record derived from a real compiled one (the fixture every
/// test mutates, so records stay valid against the live catalog).
fn record(provider: &str, id: &str, base_url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "api": "openai-completions",
        "provider": provider,
        "baseUrl": base_url,
        "reasoning": true,
        "input": ["text", "image"],
        "cost": { "input": 1.5, "output": 3.0, "cacheRead": 0.1, "cacheWrite": 0.2 },
        "contextWindow": 321_000,
        "maxTokens": 16_384,
    })
}

#[tokio::test]
async fn a_saved_record_replaces_and_appends_without_a_restart() {
    let (_fixture, engine) = setup();
    let first = list_models(&engine, "openai").await[0].clone();
    let bare_id = first["id"]
        .as_str()
        .unwrap()
        .strip_prefix("openai/")
        .unwrap();

    // Replace an existing id's metadata outright (the metadata-fix case).
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({
                "providerId": "openai",
                "record": record("openai", bare_id, "https://api.openai.com/v1"),
            }),
        )
        .await
        .unwrap();
    // Append a fresh id (the new-model case).
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({
                "providerId": "openai",
                "record": record("openai", "gpt-via-record", "https://api.openai.com/v1"),
            }),
        )
        .await
        .unwrap();

    let rows = list_models(&engine, "openai").await;
    let replaced = rows.iter().find(|row| row["id"] == first["id"]).unwrap();
    assert_eq!(replaced["contextWindow"], 321_000);
    assert_eq!(replaced["imageCapability"], "supported");
    assert_eq!(replaced["custom"], true);
    let appended = rows
        .iter()
        .find(|row| row["id"] == "openai/gpt-via-record")
        .unwrap();
    assert_eq!(appended["contextWindow"], 321_000);

    // Removal restores the compiled entry the record replaced.
    engine
        .handle(
            methods::REMOVE_MODEL_RECORD,
            serde_json::json!({ "providerId": "openai", "modelId": "openai/gpt-via-record" }),
        )
        .await
        .unwrap();
    let rows = list_models(&engine, "openai").await;
    assert!(rows.iter().all(|row| row["id"] != "openai/gpt-via-record"));
}

#[tokio::test]
async fn unservable_records_and_unknown_providers_are_rejected() {
    let (_fixture, engine) = setup();
    for (label, provider, rec) in [
        (
            "unknown provider",
            "ghost",
            record("ghost", "ghost-1", "https://ghost.example/v1"),
        ),
        (
            "baseUrl not http(s)",
            "openai",
            record("openai", "gpt-bad", "not-a-url"),
        ),
        (
            "parent mismatch",
            "openai",
            record("anthropic", "gpt-bad", "https://api.openai.com/v1"),
        ),
        ("unregistered dialect", "openai", {
            let mut bad = record("openai", "gpt-bad", "https://api.openai.com/v1");
            bad["api"] = "carrier-pigeon".into();
            bad
        }),
        ("zero context window", "openai", {
            let mut bad = record("openai", "gpt-bad", "https://api.openai.com/v1");
            bad["contextWindow"] = 0.into();
            bad
        }),
    ] {
        assert!(
            engine
                .handle(
                    methods::SAVE_MODEL_RECORD,
                    serde_json::json!({ "providerId": provider, "record": rec }),
                )
                .await
                .is_err(),
            "{label} was accepted"
        );
    }
    // Nothing from the rejected batch reached the file or the listing.
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .all(|row| row["id"] != "openai/gpt-bad")
    );
}

#[tokio::test]
async fn a_custom_provider_flows_from_definition_to_key_to_models() {
    let (_fixture, engine) = setup();
    engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "acme",
                "name": "Acme Gateway",
                "baseUrl": "https://acme.example/v1",
                "defaultApi": "openai-completions",
            }),
        )
        .await
        .unwrap();

    // Its own organization row, unconfigured until a key exists.
    let RpcReply::Value(providers) = engine
        .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("ListProviders did not return a value");
    };
    let row = providers
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "acme")
        .expect("custom provider row missing");
    assert_eq!(row["name"], "Acme Gateway");
    assert_eq!(row["configured"], false);
    // The row is marked user-owned; builtin rows are not.
    assert_eq!(row["custom"], true);
    assert!(
        providers
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "openai")
            .is_some_and(|row| row["custom"] == false)
    );

    // The key path opens for it (Settings is the only key path — ADR-0029).
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "acme", "key": "acme-key" }),
        )
        .await
        .unwrap();

    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({
                "providerId": "acme",
                "record": record("acme", "acme-1", "https://acme.example/v1"),
            }),
        )
        .await
        .unwrap();
    let rows = list_models(&engine, "acme").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "acme/acme-1");
    assert_eq!(rows[0]["contextWindow"], 321_000);
}

/// A record without `baseUrl` inherits the provider's default endpoint —
/// the custom definition's for user-defined providers, the catalog entry's
/// (else its first model's) for built-ins. An explicit value still wins.
#[tokio::test]
async fn a_record_without_a_base_url_rides_the_provider_endpoint() {
    let (_fixture, engine) = setup();
    engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "acme",
                "name": "Acme Gateway",
                "baseUrl": "https://acme.example/v1",
                "defaultApi": "openai-completions",
            }),
        )
        .await
        .unwrap();

    let mut acme_record = record("acme", "acme-1", "https://acme.example/v1");
    acme_record.as_object_mut().unwrap().remove("baseUrl");
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({ "providerId": "acme", "record": acme_record }),
        )
        .await
        .unwrap();
    let mut openai_record = record("openai", "gpt-via-record", "");
    openai_record.as_object_mut().unwrap().remove("baseUrl");
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({ "providerId": "openai", "record": openai_record }),
        )
        .await
        .unwrap();

    // Both records landed with the provider's endpoint filled in.
    let stored =
        std::fs::read_to_string(_fixture.data_dir.path().join("provider-settings.json")).unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(
        stored["modelRecords"]["acme"]["acme-1"]["baseUrl"],
        "https://acme.example/v1"
    );
    assert_eq!(
        stored["modelRecords"]["openai"]["gpt-via-record"]["baseUrl"],
        "https://api.openai.com/v1"
    );
}

#[tokio::test]
async fn a_custom_provider_cannot_steal_a_builtin_id() {
    let (_fixture, engine) = setup();
    let reply = engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "openai",
                "name": "Hostile Twin",
                "baseUrl": "https://evil.example/v1",
                "defaultApi": "openai-completions",
            }),
        )
        .await;
    assert!(reply.is_err());
    // An id with a slash would break provider-qualified model ids.
    let reply = engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "a/b",
                "name": "Slash",
                "baseUrl": "https://evil.example/v1",
                "defaultApi": "openai-completions",
            }),
        )
        .await;
    assert!(reply.is_err());
}

#[tokio::test]
async fn hidden_models_leave_listings_but_stay_resolvable() {
    let (_fixture, engine) = setup();
    let first = list_models(&engine, "openai").await[0].clone();

    engine
        .handle(
            methods::SET_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai", "modelIds": [first["id"].clone()] }),
        )
        .await
        .unwrap();
    let rows = list_models(&engine, "openai").await;
    assert!(rows.iter().all(|row| row["id"] != first["id"]));

    // The hidden listing is the Settings page's greyed section: ids plus
    // labels, never the picker's ListModels.
    let RpcReply::Value(hidden) = engine
        .handle(
            methods::LIST_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai" }),
        )
        .await
        .unwrap()
    else {
        panic!("ListHiddenModels did not return a value");
    };
    let hidden_rows = hidden.as_array().unwrap();
    assert_eq!(hidden_rows.len(), 1);
    assert_eq!(hidden_rows[0]["id"], first["id"]);
    assert_eq!(hidden_rows[0]["label"], first["label"]);

    // Unhiding is the same call with an empty set.
    engine
        .handle(
            methods::SET_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai", "modelIds": [] }),
        )
        .await
        .unwrap();
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .any(|row| row["id"] == first["id"])
    );
    let RpcReply::Value(hidden) = engine
        .handle(
            methods::LIST_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai" }),
        )
        .await
        .unwrap()
    else {
        panic!("ListHiddenModels did not return a value");
    };
    assert!(hidden.as_array().unwrap().is_empty());

    // Unknown ids are rejected rather than silently ignored.
    let reply = engine
        .handle(
            methods::SET_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai", "modelIds": ["gpt-nope"] }),
        )
        .await;
    assert!(reply.is_err());
}

#[tokio::test]
async fn reset_drops_the_live_layer_and_keeps_the_catalog() {
    let fixture = Fixture::new();
    // A bare custom model rides a legacy provider-settings.json — its add
    // RPC is gone.
    std::fs::write(
        fixture.data_dir.path().join("provider-settings.json"),
        serde_json::to_vec(&serde_json::json!({
            "customModels": { "openai": ["gpt-bare"] }
        }))
        .unwrap(),
    )
    .unwrap();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({
                "providerId": "openai",
                "record": record("openai", "gpt-via-record", "https://api.openai.com/v1"),
            }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "acme",
                "name": "Acme Gateway",
                "baseUrl": "https://acme.example/v1",
                "defaultApi": "openai-completions",
            }),
        )
        .await
        .unwrap();
    let compiled_count = list_models(&engine, "openai").await.len() - 2; // record + bare id
    let hidden_id = list_models(&engine, "openai").await[0]["id"].clone();
    engine
        .handle(
            methods::SET_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai", "modelIds": [hidden_id] }),
        )
        .await
        .unwrap();

    // Per-provider reset: the custom provider survives, openai is pristine.
    engine
        .handle(
            methods::RESET_PROVIDER_CATALOG,
            serde_json::json!({ "providerId": "openai" }),
        )
        .await
        .unwrap();
    let rows = list_models(&engine, "openai").await;
    // The hidden id is back too: the whole live layer is gone.
    assert_eq!(rows.len(), compiled_count);
    assert!(
        rows.iter()
            .all(|row| row["id"] != "openai/gpt-via-record" && row["id"] != "openai/gpt-bare")
    );
    assert_eq!(list_models(&engine, "acme").await.len(), 0);

    // Global reset: the custom provider goes too; a key is not a catalog
    // entry and survives (it stays in the credential store, inert).
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "acme", "key": "acme-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(methods::RESET_PROVIDER_CATALOG, serde_json::json!({}))
        .await
        .unwrap();
    let RpcReply::Value(providers) = engine
        .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("ListProviders did not return a value");
    };
    assert!(
        providers
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != "acme")
    );
    let RpcReply::Value(key) = engine
        .handle(
            methods::REVEAL_PROVIDER_KEY,
            serde_json::json!({ "providerId": "acme" }),
        )
        .await
        .unwrap()
    else {
        panic!("RevealProviderKey did not return a value");
    };
    assert_eq!(key["key"], "acme-key");
}
