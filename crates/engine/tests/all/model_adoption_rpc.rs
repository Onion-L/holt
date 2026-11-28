//! A vendor listing's models through the RPC surface
//! (ADR-fetched-models-adopt-the-catalogs-record): what the fetch dialog
//! previews for each id — the catalog's record, an estimate, or a fresh
//! start — and what `SaveFetchedModels` files for it. No network, no real
//! keys.

use crate::common::{Fixture, ScriptedProvider};
use holt_engine::LocalEngine;
use holt_rpc::{RpcReply, RpcService, methods};

/// A gateway speaking OpenAI completions, as a user defines one in Settings.
async fn gateway() -> (Fixture, LocalEngine) {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "gw",
                "name": "Company Gateway",
                "baseUrl": "https://gw.example/v1",
                "defaultApi": "openai-completions",
            }),
        )
        .await
        .unwrap();
    (fixture, engine)
}

async fn value(engine: &LocalEngine, method: &str, params: serde_json::Value) -> serde_json::Value {
    let RpcReply::Value(value) = engine.handle(method, params).await.unwrap() else {
        panic!("{method} did not return a value");
    };
    value
}

async fn preview(engine: &LocalEngine, provider: &str, ids: &[&str]) -> Vec<serde_json::Value> {
    value(
        engine,
        methods::PREVIEW_FETCHED_MODELS,
        serde_json::json!({ "providerId": provider, "modelIds": ids }),
    )
    .await
    .as_array()
    .unwrap()
    .clone()
}

async fn save(
    engine: &LocalEngine,
    provider: &str,
    models: serde_json::Value,
) -> Result<(), String> {
    match engine
        .handle(
            methods::SAVE_FETCHED_MODELS,
            serde_json::json!({ "providerId": provider, "models": models }),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

async fn listed(engine: &LocalEngine, provider: &str) -> Vec<serde_json::Value> {
    value(
        engine,
        methods::LIST_MODELS,
        serde_json::json!({ "providerId": provider }),
    )
    .await
    .as_array()
    .unwrap()
    .clone()
}

async fn record(engine: &LocalEngine, provider: &str, model: &str) -> serde_json::Value {
    value(
        engine,
        methods::GET_MODEL_RECORD,
        serde_json::json!({ "providerId": provider, "modelId": model }),
    )
    .await
}

fn row<'a>(rows: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    rows.iter().find(|row| row["id"] == id).unwrap()
}

#[tokio::test]
async fn a_listing_previews_one_row_per_id_in_listing_order() {
    let (_fixture, engine) = gateway().await;

    let rows = preview(
        &engine,
        "gw",
        &[
            "gpt-5.4",
            "acme-1",
            "openai/claude-opus-4-7",
            "glm-5.3",
            "glm-5.1",
            "gpt-5.4-2026-11-01",
        ],
    )
    .await;

    let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "gpt-5.4",
            "acme-1",
            "openai/claude-opus-4-7",
            "glm-5.3",
            "glm-5.1",
            "gpt-5.4-2026-11-01",
        ]
    );

    // GPT-5.4: OpenAI's own record in the gateway's dialect — the 1,050K
    // window and the ladder without `minimal`, which a blank record used to
    // offer.
    let gpt = row(&rows, "gpt-5.4");
    assert_eq!(gpt["source"], "openai/gpt-5.4");
    assert_eq!(gpt["name"], "GPT-5.4");
    assert_eq!(gpt["contextWindow"], 1_050_000);
    assert_eq!(gpt["maxTokens"], 128_000);
    assert_eq!(
        gpt["reasoningLevels"],
        serde_json::json!(["low", "medium", "high", "xhigh"])
    );
    assert_eq!(gpt["cost"]["input"].as_f64(), Some(2.5));
    assert_eq!(gpt["cost"]["output"].as_f64(), Some(15.0));
    assert_eq!(gpt["canTurnOff"], true);
    assert_eq!(gpt["levelsEstimated"], false);

    // The gateway's own id for a model stays its id.
    let claude = row(&rows, "openai/claude-opus-4-7");
    assert_eq!(claude["source"], "anthropic/claude-opus-4-7");
    assert_eq!(claude["name"], "Claude Opus 4.7");
    assert_eq!(claude["contextWindow"], 1_000_000);
    assert_eq!(
        claude["reasoningLevels"],
        serde_json::json!(["low", "medium", "high", "xhigh", "max"])
    );
    // Z.AI's record says GLM-5.3 cannot be turned off.
    let glm = row(&rows, "glm-5.3");
    assert_eq!(glm["source"], "zai/glm-5.3");
    assert_eq!(
        glm["reasoningLevels"],
        serde_json::json!(["low", "high", "max"])
    );
    assert_eq!(glm["canTurnOff"], false);

    // GLM-5.1 is in the catalog with no ladder written into it: the row
    // takes its nearest sibling's and says so.
    let glm_51 = row(&rows, "glm-5.1");
    assert_eq!(glm_51["source"], "zai-coding-cn/glm-5.1");
    assert_eq!(glm_51["levelsEstimated"], true);
    assert_eq!(glm_51["estimatedFrom"], "zai-coding-cn/glm-5.2");
    assert_eq!(
        glm_51["reasoningLevels"],
        serde_json::json!(["high", "max"])
    );

    // An id the catalog does not carry: the plain defaults, and a name off
    // the id in the model list's style.
    let acme = row(&rows, "acme-1");
    assert_eq!(acme["source"], serde_json::Value::Null);
    assert_eq!(acme["startsFrom"], serde_json::Value::Null);
    assert_eq!(acme["name"], "Acme-1");
    assert_eq!(acme["contextWindow"], 200_000);
    assert_eq!(acme["maxTokens"], 8_192);
    assert_eq!(acme["reasoningLevels"], serde_json::json!([]));

    // A dated snapshot of a model the same listing adopts starts from it,
    // but a price is the model's own, never its sibling's.
    let dated = row(&rows, "gpt-5.4-2026-11-01");
    assert_eq!(dated["source"], serde_json::Value::Null);
    assert_eq!(dated["startsFrom"], "gw/gpt-5.4");
    assert_eq!(dated["name"], "GPT-5.4 (2026-11-01)");
    assert_eq!(dated["contextWindow"], 1_050_000);
    assert_eq!(
        dated["reasoningLevels"],
        serde_json::json!(["low", "medium", "high", "xhigh"])
    );
    assert_eq!(dated["cost"]["input"].as_f64(), Some(0.0));
}

#[tokio::test]
async fn adopted_rows_carry_the_catalog_s_facts_into_the_listing() {
    let (_fixture, engine) = gateway().await;

    save(
        &engine,
        "gw",
        serde_json::json!([
            { "id": "gpt-5.4" },
            { "id": "openai/claude-opus-4-7", "name": "  Claude  " },
        ]),
    )
    .await
    .unwrap();

    let models = listed(&engine, "gw").await;
    assert_eq!(models.len(), 2);
    let gpt = row(&models, "gw/gpt-5.4");
    // A blank name takes the catalog's; the window and the ladder are the
    // catalog's and not the dialog's former 200,000 / generic four.
    assert_eq!(gpt["label"], "GPT-5.4");
    assert_eq!(gpt["contextWindow"], 1_050_000);
    assert_eq!(
        gpt["reasoningLevels"],
        serde_json::json!(["low", "medium", "high", "xhigh"])
    );
    assert_eq!(gpt["custom"], true);
    let claude = row(&models, "gw/openai/claude-opus-4-7");
    assert_eq!(claude["label"], "Claude");
    assert_eq!(claude["contextWindow"], 1_000_000);

    // The record as served, whole: the engine hands the request path this,
    // so the gateway's endpoint and dialect ride it, and the two levels
    // GPT-5.4 lacks are marked so the run loop never clamps into them.
    let record = record(&engine, "gw", "gw/gpt-5.4").await;
    assert_eq!(record["provider"], "gw");
    assert_eq!(record["baseUrl"], "https://gw.example/v1");
    assert_eq!(record["api"], "openai-completions");
    assert_eq!(record["adoptedFrom"], "openai/gpt-5.4");
    assert_eq!(
        record["thinkingLevelMap"]["minimal"],
        serde_json::Value::Null
    );
    assert_eq!(record["thinkingLevelMap"]["max"], serde_json::Value::Null);
    assert_eq!(record["thinkingLevelMap"]["xhigh"], "xhigh");
}

#[tokio::test]
async fn levels_chosen_by_hand_make_the_row_the_user_s() {
    let (_fixture, engine) = gateway().await;

    // GLM-5.1's ladder is an estimate; the dialog lets the user set it.
    save(
        &engine,
        "gw",
        serde_json::json!([{
            "id": "glm-5.1",
            "thinkingLevels": ["low", "high"],
            "canTurnOff": false,
        }]),
    )
    .await
    .unwrap();

    let record = record(&engine, "gw", "gw/glm-5.1").await;
    let ladder = &record["thinkingLevelMap"];
    assert_eq!(ladder["low"], "low");
    assert_eq!(ladder["high"], "high");
    assert_eq!(ladder["medium"], serde_json::Value::Null);
    assert_eq!(ladder["off"], serde_json::Value::Null);
    // Not an adoption any more: nothing regenerates over the choice.
    assert!(record.get("adoptedDigest").is_none());
    // The rest of the catalog's facts stay.
    assert_eq!(record["name"], "GLM-5.1");
    assert_eq!(record["contextWindow"], 200_000);
}

#[tokio::test]
async fn a_new_row_is_saved_with_what_it_was_given() {
    let (_fixture, engine) = gateway().await;

    save(
        &engine,
        "gw",
        serde_json::json!([{
            "id": "acme-1",
            "name": "",
            "contextWindow": 321_000,
            "maxTokens": 16_384,
            "thinkingLevels": ["low", "max"],
            "canTurnOff": false,
            "image": true,
            "cost": { "input": 1.5, "output": 3.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
        }]),
    )
    .await
    .unwrap();

    let record = record(&engine, "gw", "gw/acme-1").await;
    // A blank name falls back to the id, written the model list's way.
    assert_eq!(record["name"], "Acme-1");
    assert_eq!(record["provider"], "gw");
    assert_eq!(record["api"], "openai-completions");
    assert_eq!(record["baseUrl"], "https://gw.example/v1");
    assert_eq!(record["contextWindow"], 321_000);
    assert_eq!(record["maxTokens"], 16_384);
    assert_eq!(record["cost"]["input"].as_f64(), Some(1.5));
    assert_eq!(record["input"], serde_json::json!(["text", "image"]));
    assert_eq!(record["thinkingLevelMap"]["max"], "max");
    assert_eq!(record["thinkingLevelMap"]["off"], serde_json::Value::Null);
    assert!(record.get("adoptedFrom").is_none());
}

#[tokio::test]
async fn a_new_id_on_a_provider_that_carries_its_family_starts_from_that_row() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let rows = preview(&engine, "zai-coding-cn", &["glm-4.5x", "glm-9"]).await;

    // Nothing in the catalog carries `glm-4.5x`; the plan's own GLM-4.7 is
    // its nearest sibling there, so the row opens with that model's numbers
    // instead of the plain 200,000 / 8,192.
    let glm = row(&rows, "glm-4.5x");
    assert_eq!(glm["source"], serde_json::Value::Null);
    assert_eq!(glm["startsFrom"], "zai-coding-cn/glm-4.7");
    assert_eq!(glm["name"], "GLM-4.5x");
    assert_eq!(glm["contextWindow"], 204_800);
    assert_eq!(glm["maxTokens"], 131_072);
    assert_eq!(glm["levelsEstimated"], true);
    // GLM-9 is as near to GLM-5-Turbo (5.0) as to anything.
    assert_eq!(
        row(&rows, "glm-9")["startsFrom"],
        "zai-coding-cn/glm-5-turbo"
    );

    save(
        &engine,
        "zai-coding-cn",
        serde_json::json!([{
            "id": "glm-4.5x",
            "contextWindow": 204_800,
            "maxTokens": 131_072,
            "startsFrom": "zai-coding-cn/glm-4.7",
        }]),
    )
    .await
    .unwrap();

    // The provider's own row speaks Z.AI's protocol, and the new row speaks
    // it too: the sibling's `compat` came along, and nothing else of it.
    let saved = record(&engine, "zai-coding-cn", "zai-coding-cn/glm-4.5x").await;
    assert_eq!(saved["name"], "GLM-4.5x");
    assert_eq!(saved["cost"]["input"].as_f64(), Some(0.0));
    let protocol = record(&engine, "zai-coding-cn", "zai-coding-cn/glm-4.7").await;
    assert_eq!(saved["compat"], protocol["compat"]);
}

#[tokio::test]
async fn a_fetched_row_never_replaces_a_model_the_provider_already_offers() {
    let (_fixture, engine) = gateway().await;
    save(&engine, "gw", serde_json::json!([{ "id": "gpt-5.4" }]))
        .await
        .unwrap();
    let before = record(&engine, "openai", "openai/gpt-5.4").await;

    // The catalog's own OpenAI row...
    let error = save(
        &engine,
        "openai",
        serde_json::json!([{ "id": "gpt-5.4", "contextWindow": 1, "maxTokens": 1 }]),
    )
    .await
    .unwrap_err();
    assert!(error.contains("already offered"), "{error}");
    // ... and the row just filed under the gateway.
    let error = save(&engine, "gw", serde_json::json!([{ "id": "gpt-5.4" }]))
        .await
        .unwrap_err();
    assert!(error.contains("already offered"), "{error}");

    assert_eq!(record(&engine, "openai", "openai/gpt-5.4").await, before);
    assert_eq!(listed(&engine, "gw").await.len(), 1);
}

/// The preview's `offered` is the refusal's own test, so the dialog dims
/// what the save would refuse — a row the panel's listing hides included,
/// which used to open as a selectable one and fail the whole batch on Add.
#[tokio::test]
async fn the_preview_marks_an_id_the_provider_already_offers_however_it_is_listed() {
    let (_fixture, engine) = gateway().await;
    save(&engine, "gw", serde_json::json!([{ "id": "gpt-5.4" }]))
        .await
        .unwrap();

    let rows = preview(&engine, "gw", &["gpt-5.4", "acme-1", "glm-5.3"]).await;
    // The row just filed under the gateway is offered, with its facts still
    // answered; a new id and an id the catalog carries elsewhere are not —
    // the latter is adopted, never refused.
    assert_eq!(row(&rows, "gpt-5.4")["offered"], true);
    assert_eq!(row(&rows, "gpt-5.4")["source"], "openai/gpt-5.4");
    assert_eq!(row(&rows, "acme-1")["offered"], false);
    assert_eq!(row(&rows, "glm-5.3")["offered"], false);

    // Hidden from `ListModels`, and still the provider's own row: the
    // dialog cannot learn it from the panel's list.
    engine
        .handle(
            methods::SET_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "gw", "modelIds": ["gpt-5.4"] }),
        )
        .await
        .unwrap();
    assert!(listed(&engine, "gw").await.is_empty());
    let rows = preview(&engine, "gw", &["gpt-5.4", "acme-1"]).await;
    assert_eq!(row(&rows, "gpt-5.4")["offered"], true);
    assert_eq!(row(&rows, "acme-1")["offered"], false);

    // The refusal the flag speaks for.
    let error = save(
        &engine,
        "gw",
        serde_json::json!([{ "id": "gpt-5.4" }, { "id": "acme-1" }]),
    )
    .await
    .unwrap_err();
    assert!(error.contains("already offered"), "{error}");
}

/// Hiding is cosmetic: a built-in provider with every row hidden keeps its
/// transport, so its listing can still be previewed and filed — and the rows
/// it hides are `offered` and refused, as the page says.
#[tokio::test]
async fn a_provider_with_every_row_hidden_still_previews_and_files_fetched_models() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let shown = listed(&engine, "openai").await;
    assert!(!shown.is_empty());
    let ids: Vec<&str> = shown.iter().filter_map(|row| row["id"].as_str()).collect();
    engine
        .handle(
            methods::SET_HIDDEN_MODELS,
            serde_json::json!({ "providerId": "openai", "modelIds": ids }),
        )
        .await
        .unwrap();
    assert!(listed(&engine, "openai").await.is_empty());

    let rows = preview(&engine, "openai", &["gpt-5.4", "acme-1"]).await;
    // Hidden from the listing, and still the provider's own row.
    assert_eq!(row(&rows, "gpt-5.4")["offered"], true);
    assert_eq!(row(&rows, "acme-1")["offered"], false);
    let error = save(&engine, "openai", serde_json::json!([{ "id": "gpt-5.4" }]))
        .await
        .unwrap_err();
    assert!(error.contains("already offered"), "{error}");
    save(
        &engine,
        "openai",
        serde_json::json!([{ "id": "acme-1", "contextWindow": 321_000, "maxTokens": 16_384 }]),
    )
    .await
    .unwrap();
    assert_eq!(record(&engine, "openai", "acme-1").await["id"], "acme-1");
}

/// The record form saves what `GetModelRecord` served it, minus the fields
/// it edits: a cost's tiers, which the form has no field for, come back
/// whole under the rates.
#[tokio::test]
async fn a_record_keeps_its_cost_tiers_through_the_record_form_s_round_trip() {
    let (_fixture, engine) = gateway().await;
    save(&engine, "gw", serde_json::json!([{ "id": "gpt-5.4" }]))
        .await
        .unwrap();
    let tiers = serde_json::json!([{
        "input": 10.0,
        "output": 30.0,
        "cacheRead": 1.0,
        "cacheWrite": 0.5,
        "inputTokensAbove": 272_000,
    }]);
    let mut served = record(&engine, "gw", "gpt-5.4").await;
    served["cost"]["tiers"] = tiers.clone();
    value(
        &engine,
        methods::SAVE_MODEL_RECORD,
        serde_json::json!({ "providerId": "gw", "record": served }),
    )
    .await;

    let served = record(&engine, "gw", "gpt-5.4").await;
    // A rate comes back as the engine writes it — whole numbers without
    // the fraction — so each is read as a number, not as JSON.
    assert_eq!(served["cost"]["tiers"].as_array().map(Vec::len), Some(1));
    let tier = &served["cost"]["tiers"][0];
    assert_eq!(tier["input"].as_f64(), Some(10.0));
    assert_eq!(tier["output"].as_f64(), Some(30.0));
    assert_eq!(tier["cacheRead"].as_f64(), Some(1.0));
    assert_eq!(tier["cacheWrite"].as_f64(), Some(0.5));
    assert_eq!(tier["inputTokensAbove"], 272_000);
    assert_eq!(served["cost"]["input"].as_f64(), Some(2.5));
}

#[tokio::test]
async fn a_batch_that_cannot_be_filed_whole_writes_nothing() {
    let (_fixture, engine) = gateway().await;

    // A new row without its numbers; an unknown level.
    for models in [
        serde_json::json!([{ "id": "gpt-5.4" }, { "id": "acme-1" }]),
        serde_json::json!([{ "id": "gpt-5.4", "thinkingLevels": ["ultra"] }]),
    ] {
        let error = save(&engine, "gw", models).await.unwrap_err();
        assert!(listed(&engine, "gw").await.is_empty(), "{error}");
    }
    let error = save(&engine, "nowhere", serde_json::json!([{ "id": "gpt-5.4" }]))
        .await
        .unwrap_err();
    assert!(error.contains("unknown provider"), "{error}");
}

#[tokio::test]
async fn previewing_needs_a_provider_the_engine_addresses() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let error = engine
        .handle(
            methods::PREVIEW_FETCHED_MODELS,
            serde_json::json!({ "providerId": "nowhere", "modelIds": ["gpt-5.4"] }),
        )
        .await
        .err()
        .expect("a provider the engine does not address is refused");
    assert!(error.to_string().contains("unknown provider"), "{error}");
}
