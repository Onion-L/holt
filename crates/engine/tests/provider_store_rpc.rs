//! Provider-store tests through the RPC surface (ticket 02): a
//! hand-edited `provider-store.json` overlay reaches `ListModels` and the
//! request path's stream seam, and a non-built-in provider id changes
//! nothing. A first assembly bootstraps the file (boot writes it when
//! missing), the test edits it like a user would, and a restart picks the
//! edit up — no network, no real keys.

mod common;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};

/// Bootstraps the store with a first assembly (the file is created when
/// missing), hands `edit` the parsed document, writes it back, and
/// assembles the engine over the edited file.
fn engine_over_edited_store<F>(
    fixture: &Fixture,
    provider: &ScriptedProvider,
    edit: F,
) -> LocalEngine
where
    F: FnOnce(&mut serde_json::Value),
{
    let bootstrap = LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: None,
        stream_fn: None,
        search_backend_resolver: None,
        jev_judge_resolver: None,
    })
    .unwrap();
    drop(bootstrap);
    let path = fixture.data_dir.path().join("provider-store.json");
    let mut store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut store);
    std::fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
    fixture.engine(provider)
}

fn openai_entry(store: &mut serde_json::Value) -> &mut serde_json::Value {
    store["providers"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|provider| provider["id"] == "openai")
        .unwrap()
}

#[tokio::test]
async fn an_override_and_a_file_only_model_reach_list_models() {
    let fixture = Fixture::new();
    let engine = engine_over_edited_store(&fixture, &ScriptedProvider::new(vec![]), |store| {
        let openai = openai_entry(store);
        for model in openai["models"].as_array_mut().unwrap().iter_mut() {
            if model["id"] == "gpt-5.4" {
                model["contextWindow"] = 123_456.into();
                model["name"] = "GPT 5.4 Overridden".into();
            }
        }
        let mut fresh = openai["models"][0].clone();
        fresh["id"] = "gpt-fresh".into();
        fresh["name"] = "GPT Fresh".into();
        fresh["contextWindow"] = 7_777.into();
        fresh["reasoning"] = true.into();
        openai["models"].as_array_mut().unwrap().push(fresh);
    });

    let RpcReply::Value(models) = engine
        .handle(
            methods::LIST_MODELS,
            serde_json::json!({ "providerId": "openai" }),
        )
        .await
        .unwrap()
    else {
        panic!("ListModels did not return a value");
    };
    let rows = models.as_array().unwrap();
    let overridden = rows
        .iter()
        .find(|row| row["id"] == "openai/gpt-5.4")
        .unwrap();
    assert_eq!(overridden["label"], "GPT 5.4 Overridden");
    assert_eq!(overridden["contextWindow"], 123_456);
    let fresh = rows
        .iter()
        .find(|row| row["id"] == "openai/gpt-fresh")
        .unwrap();
    assert_eq!(fresh["label"], "GPT Fresh");
    assert_eq!(fresh["contextWindow"], 7_777);
    // The file-only model is catalog data, not a Settings custom id: its
    // record is the file's own, and the window it carries is real.
    assert_eq!(fresh["custom"], false);
}

#[tokio::test]
async fn a_non_builtin_provider_id_changes_nothing() {
    let fixture = Fixture::new();
    let engine = engine_over_edited_store(&fixture, &ScriptedProvider::new(vec![]), |store| {
        let mut acme = openai_entry(store).clone();
        acme["id"] = "acme-gateway".into();
        store["providers"].as_array_mut().unwrap().push(acme);
    });

    let RpcReply::Value(providers) = engine
        .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("ListProviders did not return a value");
    };
    let ids: Vec<&str> = providers
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"openai"));
    assert!(!ids.contains(&"acme-gateway"));

    // The eligibility gates stay shut for the dropped id, exactly as before.
    assert!(
        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({ "providerId": "acme-gateway", "key": "secret" }),
            )
            .await
            .is_err()
    );
    let RpcReply::Value(models) = engine
        .handle(
            methods::LIST_MODELS,
            serde_json::json!({ "providerId": "acme-gateway" }),
        )
        .await
        .unwrap()
    else {
        panic!("ListModels did not return a value");
    };
    assert!(models.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn an_override_rides_the_model_handed_to_the_stream_seam() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("overridden model reply")]);
    let engine = engine_over_edited_store(&fixture, &provider, |store| {
        let openai = openai_entry(store);
        for model in openai["models"].as_array_mut().unwrap().iter_mut() {
            if model["id"] == "gpt-5.4" {
                model["api"] = "openai-completions".into();
                model["baseUrl"] = "https://override.example/v1".into();
                model["contextWindow"] = 4_321.into();
                model["cost"]["input"] = 12.5.into();
                model["cost"]["output"] = 25.0.into();
            }
        }
    });
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "overridden model reply").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The merged model is exactly what the transport receives: its baseUrl,
    // api, context window, and cost are the file's. In the real (unscripted)
    // request path that model is also what pi-core's calculate_cost bills
    // from, so the file's rates are what the usage ledger records.
    let model = &provider.requests()[0].core_model;
    assert_eq!(model.id, "gpt-5.4");
    assert_eq!(model.base_url, "https://override.example/v1");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.context_window, 4_321);
    assert_eq!(model.cost.rates.input.0, 12.5);
    assert_eq!(model.cost.rates.output.0, 25.0);
}
