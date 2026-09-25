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

// ---------------------------------------------------------------------------
// ProbeProvider — the settings-side /models probe (Test button, fetch list)
// ---------------------------------------------------------------------------

async fn probe(engine: &LocalEngine, provider: &str) -> serde_json::Value {
    let RpcReply::Value(reply) = engine
        .handle(
            methods::PROBE_PROVIDER,
            serde_json::json!({ "providerId": provider }),
        )
        .await
        .unwrap()
    else {
        panic!("ProbeProvider did not return a value");
    };
    reply
}

async fn loopback_custom_provider(engine: &LocalEngine, base: &str) {
    engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "acme",
                "name": "Acme Gateway",
                "baseUrl": base,
                "defaultApi": "openai-completions",
            }),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_probe_lists_the_vendor_models_riding_the_stored_key() {
    let fixture = Fixture::new();
    let (server, heads) = common::serve_loopback_with_capture(
        "application/json",
        br#"{"data":[{"id":"acme-1"},{"id":"acme-2"}]}"#,
    )
    .await;
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    loopback_custom_provider(&engine, &server.base).await;
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "acme", "key": "sk-probe-secret" }),
        )
        .await
        .unwrap();

    let reply = probe(&engine, "acme").await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["status"], "ok");
    assert_eq!(reply["modelIds"], serde_json::json!(["acme-1", "acme-2"]));
    assert_eq!(reply["dialect"], "openai-completions");
    assert!(reply["latencyMs"].is_u64(), "latency is measured");
    assert!(reply["error"].is_null());
    let heads = heads.lock().unwrap().clone();
    assert!(
        heads.iter().any(|head| head
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-probe-secret")),
        "the stored key rode the probe"
    );
}

#[tokio::test]
async fn an_open_endpoint_probes_ok_without_a_key() {
    // The OpenRouter shape: /models answers 200 with no (or a wrong) key.
    // The reply stays `ok` — the probe verifies the endpoint, never the
    // key; the UI copy must not claim the key was validated.
    let fixture = Fixture::new();
    let (server, heads) =
        common::serve_loopback_with_capture("application/json", br#"{"data":[{"id":"open-1"}]}"#)
            .await;
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    loopback_custom_provider(&engine, &server.base).await;

    let reply = probe(&engine, "acme").await;
    assert_eq!(reply["status"], "ok");
    assert_eq!(reply["modelIds"], serde_json::json!(["open-1"]));
    let heads = heads.lock().unwrap().clone();
    assert!(
        heads
            .iter()
            .all(|head| !head.to_ascii_lowercase().contains("authorization:")),
        "no key stored, no auth header sent"
    );
}

#[tokio::test]
async fn a_401_is_a_key_verdict_and_other_failures_verify_nothing() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    // 401: every probeable dialect's header matches probe_models' auth, so
    // a challenge is the one verdict that says the stored key is wrong.
    let rejected = common::serve_loopback_status(401, "application/json", br#"{}"#).await;
    loopback_custom_provider(&engine, &rejected.base).await;
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "acme", "key": "sk-stale" }),
        )
        .await
        .unwrap();
    let reply = probe(&engine, "acme").await;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["status"], "key_rejected");
    assert_eq!(reply["modelIds"], serde_json::Value::Array(Vec::new()));

    // 404: the endpoint may simply not expose /models. That must read as
    // "could not verify", never as "the key is wrong".
    let missing = common::serve_loopback_status(404, "application/json", br#"{}"#).await;
    loopback_custom_provider(&engine, &missing.base).await;
    let reply = probe(&engine, "acme").await;
    assert_eq!(reply["status"], "unverifiable");
}

#[tokio::test]
async fn an_unknown_provider_is_refused_before_any_request() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let error = match engine
        .handle(
            methods::PROBE_PROVIDER,
            serde_json::json!({ "providerId": "ghost" }),
        )
        .await
    {
        Ok(_) => panic!("ProbeProvider accepted an unknown provider"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("unknown or unsupported"),
        "the SaveProviderKey guard rejects the probe too"
    );
}
