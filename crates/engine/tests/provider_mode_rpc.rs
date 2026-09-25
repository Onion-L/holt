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
    for name in ["model_proposal", "request_provider_key", "choose_provider"] {
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
        [
            "choose_provider",
            "model_proposal",
            "request_provider_key",
            "web_fetch"
        ]
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

// ---------------------------------------------------------------------------
// Cards in the transcript (Step 5)
// ---------------------------------------------------------------------------

fn custom_provider_change(id: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "upsert_custom_provider",
        "provider": {
            "id": id,
            "name": id,
            "baseUrl": format!("https://{id}.example.com/v1"),
            "defaultApi": "openai-completions",
        },
    })
}

fn propose_custom(call_id: &str, provider_id: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        call_id,
        "model_proposal",
        serde_json::json!({ "changes": [custom_provider_change(provider_id)] }),
    )
}

/// Every card of `kind` in the chat's transcript, as the card JSON.
async fn cards(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    kind: &str,
) -> Vec<serde_json::Value> {
    let snapshot = common::transcript_snapshot(engine, chat_id).await;
    let empty = Vec::new();
    snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().unwrap_or(&empty))
        .filter(|part| part["kind"] == kind)
        .map(|part| part.get("card").cloned().unwrap_or_else(|| part.clone()))
        .collect()
}

/// A provider-mode chat that ran one Turn over `replies`.
async fn mode_turn(
    fixture: &Fixture,
    replies: Vec<ScriptedReply>,
) -> (holt_engine::LocalEngine, ScriptedProvider) {
    let provider = ScriptedProvider::new(replies);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    run_turn(&engine, fixture, "set it up").await;
    (engine, provider)
}

async fn run_turn(engine: &holt_engine::LocalEngine, fixture: &Fixture, prompt: &str) {
    let (_, mut sessions) = common::subscribe(engine, "chat-1").await;
    run_prompt(engine, "chat-1", &fixture.cwd(), prompt).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
}

async fn proposal_rpc(
    engine: &holt_engine::LocalEngine,
    method: &str,
    proposal_id: &str,
) -> Result<RpcReply, holt_rpc::RpcError> {
    engine
        .handle(
            method,
            serde_json::json!({ "chatId": "chat-1", "proposalId": proposal_id }),
        )
        .await
}

async fn custom_provider_ids(engine: &holt_engine::LocalEngine) -> Vec<String> {
    let RpcReply::Value(rows) = engine
        .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("ListProviders did not return a value");
    };
    rows.as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_proposal_lands_as_a_pending_card_and_write_stamps_it() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_custom("call-1", "acme"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;

    let found = cards(&engine, "chat-1", "modelProposal").await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["state"], "pending");
    assert!(!found[0]["lines"].as_array().unwrap().is_empty());
    let proposal_id = found[0]["proposalId"].as_str().unwrap().to_string();

    proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &proposal_id)
        .await
        .unwrap();
    assert!(
        custom_provider_ids(&engine)
            .await
            .contains(&"acme".to_string())
    );
    let found = cards(&engine, "chat-1", "modelProposal").await;
    assert_eq!(found[0]["state"], "written");

    // A second Write finds nothing stored and fails; the card stays written.
    assert!(
        proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &proposal_id)
            .await
            .is_err()
    );
    assert_eq!(
        cards(&engine, "chat-1", "modelProposal").await[0]["state"],
        "written"
    );
}

#[tokio::test]
async fn discard_stamps_the_card_discarded() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_custom("call-1", "acme"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let proposal_id = cards(&engine, "chat-1", "modelProposal").await[0]["proposalId"]
        .as_str()
        .unwrap()
        .to_string();
    proposal_rpc(&engine, methods::DISCARD_MODEL_PROPOSAL, &proposal_id)
        .await
        .unwrap();
    assert_eq!(
        cards(&engine, "chat-1", "modelProposal").await[0]["state"],
        "discarded"
    );
    assert!(
        !custom_provider_ids(&engine)
            .await
            .contains(&"acme".to_string())
    );
}

#[tokio::test]
async fn a_newer_proposal_on_the_same_provider_supersedes_only_that_card() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_custom("call-1", "acme"),
            propose_custom("call-2", "globex"),
            ScriptedReply::tool_call(
                "call-3",
                "model_proposal",
                serde_json::json!({ "changes": [{
                    "action": "upsert_custom_provider",
                    "provider": {
                        "id": "acme",
                        "name": "Acme Two",
                        "baseUrl": "https://acme.example.com/v2",
                        "defaultApi": "openai-completions",
                    },
                }] }),
            ),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let found = cards(&engine, "chat-1", "modelProposal").await;
    let states: Vec<&str> = found
        .iter()
        .map(|card| card["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["superseded", "pending", "pending"]);

    // The superseded card's proposal is gone from the store.
    let first = found[0]["proposalId"].as_str().unwrap();
    assert!(
        proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, first)
            .await
            .is_err()
    );
    for card in &found[1..] {
        proposal_rpc(
            &engine,
            methods::APPLY_MODEL_PROPOSAL,
            card["proposalId"].as_str().unwrap(),
        )
        .await
        .unwrap();
    }
    let ids = custom_provider_ids(&engine).await;
    assert!(ids.contains(&"acme".to_string()) && ids.contains(&"globex".to_string()));
}

#[tokio::test]
async fn the_cap_evicts_the_oldest_proposal_and_stamps_its_card() {
    let fixture = Fixture::new();
    let mut replies: Vec<ScriptedReply> = (0..6)
        .map(|ix| propose_custom(&format!("call-{ix}"), &format!("vendor{ix}")))
        .collect();
    replies.push(ScriptedReply::text("proposed"));
    let (engine, _provider) = mode_turn(&fixture, replies).await;
    let states: Vec<String> = cards(&engine, "chat-1", "modelProposal")
        .await
        .iter()
        .map(|card| card["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        states,
        [
            "superseded",
            "pending",
            "pending",
            "pending",
            "pending",
            "pending"
        ]
    );
}

#[tokio::test]
async fn card_states_and_pending_proposals_survive_a_restart() {
    let fixture = Fixture::new();
    let config_dir = fixture.data_dir.path().to_path_buf();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_custom("call-1", "acme"),
            propose_custom("call-2", "globex"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let found = cards(&engine, "chat-1", "modelProposal").await;
    let (written, pending) = (
        found[0]["proposalId"].as_str().unwrap().to_string(),
        found[1]["proposalId"].as_str().unwrap().to_string(),
    );
    proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &written)
        .await
        .unwrap();
    drop(engine);

    let provider = ScriptedProvider::new(vec![]);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: config_dir,
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    let states: Vec<String> = cards(&engine, "chat-1", "modelProposal")
        .await
        .iter()
        .map(|card| card["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(states, ["written", "pending"]);
    // The pending card from before the restart still writes.
    proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &pending)
        .await
        .unwrap();
    assert!(
        custom_provider_ids(&engine)
            .await
            .contains(&"globex".to_string())
    );
    assert_eq!(
        cards(&engine, "chat-1", "modelProposal").await[1]["state"],
        "written"
    );
}

#[tokio::test]
async fn a_key_request_lands_as_a_card_and_the_settle_stamps_it() {
    let fixture = Fixture::new();
    let (engine, provider) = mode_turn(
        &fixture,
        vec![
            ScriptedReply::tool_call(
                "call-1",
                "request_provider_key",
                serde_json::json!({ "providerId": "zai-coding-cn" }),
            ),
            ScriptedReply::tool_call(
                "call-2",
                "request_provider_key",
                serde_json::json!({ "providerId": "zai-coding-cn" }),
            ),
            ScriptedReply::text("asked"),
            ScriptedReply::text("re-probed"),
        ],
    )
    .await;
    let found = cards(&engine, "chat-1", "keyRequest").await;
    let states: Vec<&str> = found
        .iter()
        .map(|card| card["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["superseded", "pending"]);
    assert_eq!(found[1]["providerId"], "zai-coding-cn");
    assert!(
        found[1]["destination"]
            .as_str()
            .unwrap()
            .starts_with("https://")
    );

    engine
        .handle(
            methods::SETTLE_PROVIDER_KEY_REQUEST,
            serde_json::json!({ "chatId": "chat-1", "key": "sk-card-secret" }),
        )
        .await
        .unwrap();
    wait_for_requests(&provider, 4).await;
    let found = cards(&engine, "chat-1", "keyRequest").await;
    assert_eq!(found[1]["state"], "saved");
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("sk-card-secret"));
}

// ---------------------------------------------------------------------------
// Draft providers (Step 6)
// ---------------------------------------------------------------------------

fn draft_key_call(call_id: &str, base_url: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        call_id,
        "request_provider_key",
        serde_json::json!({
            "providerId": "acme",
            "provider": {
                "id": "acme",
                "name": "Acme",
                "baseUrl": base_url,
                "defaultApi": "openai-completions",
            },
        }),
    )
}

fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[tokio::test]
async fn a_draft_key_request_saves_under_the_draft_and_never_leaks_the_key() {
    let fixture = Fixture::new();
    let (engine, provider) = mode_turn(
        &fixture,
        vec![
            // A loopback draft is refused: a tool error, no card.
            draft_key_call("call-1", "http://127.0.0.1:8080/v1"),
            draft_key_call("call-2", "https://8.8.8.8/v1"),
            ScriptedReply::text("asked"),
            ScriptedReply::text("re-probed"),
        ],
    )
    .await;
    let found = cards(&engine, "chat-1", "keyRequest").await;
    assert_eq!(found.len(), 1, "the refused draft left no card");
    assert_eq!(found[0]["providerId"], "acme");
    assert_eq!(found[0]["providerName"], "Acme");
    assert_eq!(found[0]["destination"], "https://8.8.8.8/v1");
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("is not a public address"));

    engine
        .handle(
            methods::SETTLE_PROVIDER_KEY_REQUEST,
            serde_json::json!({ "chatId": "chat-1", "key": "sk-draft-secret" }),
        )
        .await
        .unwrap();
    wait_for_requests(&provider, 4).await;
    let RpcReply::Value(revealed) = engine
        .handle(
            methods::REVEAL_PROVIDER_KEY,
            serde_json::json!({ "providerId": "acme" }),
        )
        .await
        .unwrap()
    else {
        panic!("RevealProviderKey did not return a value");
    };
    assert_eq!(revealed["key"], "sk-draft-secret");
    assert_eq!(
        cards(&engine, "chat-1", "keyRequest").await[0]["state"],
        "saved"
    );

    // Nothing but the credential store holds the key: not the transcript,
    // History, chat state, or any request the model saw.
    for request in provider.requests() {
        let messages = serde_json::to_string(&request.messages).unwrap();
        assert!(!messages.contains("sk-draft-secret"));
    }
    let mut files = Vec::new();
    files_under(fixture.data_dir.path(), &mut files);
    let mut approved = false;
    for path in files {
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        if path.file_name().unwrap() == "provider-credentials.json" {
            continue;
        }
        assert!(
            !text.contains("sk-draft-secret"),
            "{} holds the key",
            path.display()
        );
        approved |= text.contains("https://8.8.8.8/v1") && text.contains("approvedKeyDestinations");
    }
    assert!(approved, "the draft destination was approved on the chat");
}

// ---------------------------------------------------------------------------
// The review and key seams, ported from the dialog-era suite (Step 8)
// ---------------------------------------------------------------------------

/// One complete, servable record.
fn record_json(provider: &str, id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "api": "openai-completions",
        "provider": provider,
        "baseUrl": "https://api.openai.com/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 1.0, "output": 2.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
        "contextWindow": 321_000,
        "maxTokens": 16_384,
    })
}

fn propose_record(call_id: &str, provider: &str, model: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        call_id,
        "model_proposal",
        serde_json::json!({
            "changes": [{
                "action": "upsert_model_record",
                "providerId": provider,
                "record": record_json(provider, model),
            }],
        }),
    )
}

fn key_call(call_id: &str, provider_id: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        call_id,
        "request_provider_key",
        serde_json::json!({ "providerId": provider_id }),
    )
}

/// A provider-mode chat with no Turn run yet.
async fn mode_chat(
    fixture: &Fixture,
    replies: Vec<ScriptedReply>,
) -> (holt_engine::LocalEngine, ScriptedProvider) {
    let provider = ScriptedProvider::new(replies);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    call(&engine, methods::ENTER_PROVIDER_MODE, "chat-1").await;
    (engine, provider)
}

async fn first_proposal_id(engine: &holt_engine::LocalEngine) -> String {
    cards(engine, "chat-1", "modelProposal").await[0]["proposalId"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn list_models(engine: &holt_engine::LocalEngine, provider: &str) -> Vec<serde_json::Value> {
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

async fn reveal_key(engine: &holt_engine::LocalEngine, provider: &str) -> Option<String> {
    let RpcReply::Value(reply) = engine
        .handle(
            methods::REVEAL_PROVIDER_KEY,
            serde_json::json!({ "providerId": provider }),
        )
        .await
        .unwrap()
    else {
        panic!("RevealProviderKey did not return a value");
    };
    reply["key"].as_str().map(str::to_string)
}

async fn settle(engine: &holt_engine::LocalEngine, key: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "chatId": "chat-1" });
    if let Some(key) = key {
        params["key"] = key.into();
    }
    let RpcReply::Value(settled) = engine
        .handle(methods::SETTLE_PROVIDER_KEY_REQUEST, params)
        .await
        .unwrap()
    else {
        panic!("SettleProviderKeyRequest did not return a value");
    };
    settled
}

#[tokio::test]
async fn a_record_proposal_writes_a_live_model_record() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_record("call-1", "openai", "gpt-via-mode"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let proposal_id = first_proposal_id(&engine).await;
    // Stored, not written.
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .all(|row| row["id"] != "openai/gpt-via-mode")
    );

    let RpcReply::Value(applied) =
        proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &proposal_id)
            .await
            .unwrap()
    else {
        panic!("ApplyModelProposal did not return a value");
    };
    assert!(
        applied["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row == "openai/gpt-via-mode")
    );
    let row = list_models(&engine, "openai")
        .await
        .into_iter()
        .find(|row| row["id"] == "openai/gpt-via-mode")
        .expect("the applied record is live");
    assert_eq!(row["contextWindow"], 321_000);
}

#[tokio::test]
async fn a_stale_proposal_is_refused_and_changes_nothing() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_record("call-1", "openai", "gpt-stale"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let proposal_id = first_proposal_id(&engine).await;

    // The catalog moves under the proposal.
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({
                "providerId": "openai",
                "record": record_json("openai", "gpt-stale"),
            }),
        )
        .await
        .unwrap();

    let Err(error) = proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &proposal_id).await
    else {
        panic!("the stale apply must fail");
    };
    assert!(
        error.to_string().contains("changed since this proposal"),
        "the staleness refusal surfaced: {error}"
    );
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .any(|row| row["id"] == "openai/gpt-stale")
    );
}

#[tokio::test]
async fn unknown_proposals_and_chats_are_refused() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_chat(&fixture, vec![]).await;
    assert!(
        proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, "not-a-proposal")
            .await
            .is_err()
    );
    for method in [
        methods::APPLY_MODEL_PROPOSAL,
        methods::DISCARD_MODEL_PROPOSAL,
    ] {
        assert!(
            engine
                .handle(
                    method,
                    serde_json::json!({ "chatId": "../escape", "proposalId": "x" }),
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn a_discarded_proposal_cannot_be_written() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_record("call-1", "openai", "gpt-discarded"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let proposal_id = first_proposal_id(&engine).await;
    let RpcReply::Value(reply) =
        proposal_rpc(&engine, methods::DISCARD_MODEL_PROPOSAL, &proposal_id)
            .await
            .unwrap()
    else {
        panic!("DiscardModelProposal did not return a value");
    };
    assert_eq!(reply["discarded"], true);
    assert!(
        proposal_rpc(&engine, methods::APPLY_MODEL_PROPOSAL, &proposal_id)
            .await
            .is_err()
    );
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .all(|row| row["id"] != "openai/gpt-discarded")
    );
}

#[tokio::test]
async fn a_settled_key_is_saved_and_the_chat_is_told() {
    let fixture = Fixture::new();
    let (engine, provider) = mode_chat(
        &fixture,
        vec![
            key_call("call-1", "zai-coding-cn"),
            ScriptedReply::text("I've asked for the key"),
            ScriptedReply::text("re-probed"),
        ],
    )
    .await;
    let (mut transcript, _) = common::subscribe(&engine, "chat-1").await;
    run_turn(&engine, &fixture, "list the models").await;

    let settled = settle(&engine, Some("sk-test-secret")).await;
    assert_eq!(settled["settled"], "saved");
    assert_eq!(
        reveal_key(&engine, "zai-coding-cn").await.as_deref(),
        Some("sk-test-secret")
    );
    common::wait_for_transcript_text(&mut transcript, "API key saved for zai-coding-cn").await;
    wait_for_requests(&provider, 3).await;
    // Nothing is pending any more: a second settle fails.
    assert!(
        engine
            .handle(
                methods::SETTLE_PROVIDER_KEY_REQUEST,
                serde_json::json!({ "chatId": "chat-1" }),
            )
            .await
            .is_err()
    );
    for request in provider.requests() {
        let summary = serde_json::to_string(&request.messages).unwrap_or_default();
        assert!(
            !summary.contains("sk-test-secret"),
            "the key leaked to the model"
        );
    }
}

/// A key settled through the card rides the next probe as the
/// Authorization header, and never reaches anything the model saw.
#[tokio::test]
async fn a_settled_key_reaches_the_wire_on_a_stored_probe() {
    let fixture = Fixture::new();
    let (server, heads) =
        common::serve_loopback_with_capture("application/json", br#"{"data":[{"id":"acme-1"}]}"#)
            .await;
    let (engine, provider) = mode_chat(
        &fixture,
        vec![
            key_call("call-1", "acme-custom"),
            ScriptedReply::text("asked"),
            ScriptedReply::tool_call(
                "call-2",
                "model_proposal",
                serde_json::json!({ "providerId": "acme-custom", "probe": true }),
            ),
            ScriptedReply::text("probed"),
        ],
    )
    .await;
    engine
        .handle(
            methods::SAVE_CUSTOM_PROVIDER,
            serde_json::json!({
                "id": "acme-custom",
                "name": "Acme custom",
                "baseUrl": server.base,
                "defaultApi": "openai-completions",
            }),
        )
        .await
        .unwrap();
    let (mut transcript, _) = common::subscribe(&engine, "chat-1").await;
    run_turn(&engine, &fixture, "list the models").await;
    assert_eq!(
        settle(&engine, Some("sk-wire-secret")).await["settled"],
        "saved"
    );

    common::wait_for_transcript_text(&mut transcript, "acme-1").await;
    wait_for_requests(&provider, 4).await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(snapshot.contains("key attached"));
    let heads = heads.lock().unwrap().clone();
    assert!(
        heads.iter().any(|head| {
            head.to_ascii_lowercase()
                .contains("authorization: bearer sk-wire-secret")
        }),
        "the settled key rode the probe as the Authorization header"
    );
    for request in provider.requests() {
        let summary = serde_json::to_string(&request.messages).unwrap_or_default();
        assert!(!summary.contains("sk-wire-secret"));
    }
}

/// A settle's approval never softens the SSRF gate: a planned loopback
/// baseUrl stays refused, key or no key.
#[tokio::test]
async fn a_planned_loopback_target_stays_refused_key_or_not() {
    let fixture = Fixture::new();
    let server = common::serve_loopback("application/json", br#"{"data":[]}"#).await;
    let propose = || {
        ScriptedReply::tool_call(
            "call-probe",
            "model_proposal",
            serde_json::json!({
                "changes": [{
                    "action": "upsert_custom_provider",
                    "provider": {
                        "id": "acme-planned",
                        "name": "Acme planned",
                        "baseUrl": server.base,
                        "defaultApi": "openai-completions",
                    },
                }],
                "probe": true,
            }),
        )
    };
    let (engine, _provider) = mode_chat(
        &fixture,
        vec![
            propose(),
            ScriptedReply::text("proposed"),
            key_call("call-key", "acme-planned"),
            ScriptedReply::text("asked"),
            ScriptedReply::text("continuing"),
            propose(),
            ScriptedReply::text("probed"),
        ],
    )
    .await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "add it").await;
    common::wait_for_transcript_text(&mut transcript, "refused").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    run_turn(&engine, &fixture, "list the models").await;
    settle(&engine, Some("sk-planned")).await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "probe again").await;
    common::wait_for_transcript_text(&mut transcript, "is not a public address").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(
        snapshot.to_string().matches("refused").count(),
        2,
        "both planned probes were refused, key or not"
    );
}

#[tokio::test]
async fn a_dismissed_key_request_notifies_without_saving() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_chat(
        &fixture,
        vec![
            key_call("call-1", "zai-coding-cn"),
            ScriptedReply::text("I've asked for the key"),
            ScriptedReply::text("researching instead"),
        ],
    )
    .await;
    let (mut transcript, _) = common::subscribe(&engine, "chat-1").await;
    run_turn(&engine, &fixture, "list the models").await;

    assert_eq!(settle(&engine, None).await["settled"], "dismissed");
    common::wait_for_transcript_text(
        &mut transcript,
        "The user dismissed the key request for zai-coding-cn",
    )
    .await;
    assert!(reveal_key(&engine, "zai-coding-cn").await.is_none());
    assert_eq!(
        cards(&engine, "chat-1", "keyRequest").await[0]["state"],
        "dismissed"
    );
}

#[tokio::test]
async fn settling_without_a_pending_request_fails() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_chat(&fixture, vec![]).await;
    for chat_id in ["chat-1", "../escape"] {
        assert!(
            engine
                .handle(
                    methods::SETTLE_PROVIDER_KEY_REQUEST,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn chats_outside_provider_mode_reject_the_catalog_tools() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "model_proposal",
            serde_json::json!({ "providerId": "openai" }),
        ),
        key_call("call-2", "openai"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    run_turn(&engine, &fixture, "set it up").await;

    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let empty = Vec::new();
    for call in ["call-1", "call-2"] {
        let part = snapshot["reset"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["parts"].as_array().unwrap_or(&empty))
            .find(|part| part["id"] == call)
            .expect("the tool part exists");
        assert_eq!(part["isError"], true, "{call} must settle as an error");
    }
    assert!(provider.requests().iter().any(|request| {
        common::summarize(&request.messages)
            .iter()
            .any(|row| row.contains("Tool model_proposal not found"))
    }));
    assert!(cards(&engine, "chat-1", "modelProposal").await.is_empty());
    assert!(cards(&engine, "chat-1", "keyRequest").await.is_empty());
}

/// Rows the retired Settings setup chat (ADR-0030) left in chats.json are
/// deleted on startup; ordinary chats stay.
#[tokio::test]
async fn legacy_model_setup_chats_are_deleted_on_startup() {
    let fixture = Fixture::new();
    let data_dir = fixture.data_dir.path().to_path_buf();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;
    common::setup_chat(&engine, "legacy-setup").await;
    drop(engine);

    let path = data_dir.join("chats.json");
    let mut rows: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for row in rows.as_array_mut().unwrap() {
        if row["id"] == "legacy-setup" {
            row["config"] = serde_json::json!({
                "provider": "openai",
                "model": "openai/gpt-5.4",
                "reasoning": null,
                "scope": "model-setup",
            });
        }
    }
    std::fs::write(&path, serde_json::to_string(&rows).unwrap()).unwrap();

    let provider = ScriptedProvider::new(vec![]);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: data_dir.clone(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let frame = common::next_frame(&mut chats).await;
    let ids: Vec<&str> = frame
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert!(ids.contains(&"chat-1"));
    assert!(!ids.contains(&"legacy-setup"));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(!stored.contains("legacy-setup"));
}

// ---------------------------------------------------------------------------
// Organizations resolve to one provider (plan 011)
// ---------------------------------------------------------------------------

fn choose_call(call_id: &str, ids: &[&str]) -> ScriptedReply {
    ScriptedReply::tool_call(
        call_id,
        "choose_provider",
        serde_json::json!({ "providerIds": ids }),
    )
}

async fn settle_choice(
    engine: &holt_engine::LocalEngine,
    card_id: &str,
    provider_id: &str,
) -> Result<RpcReply, holt_rpc::RpcError> {
    engine
        .handle(
            methods::SETTLE_PROVIDER_CHOICE,
            serde_json::json!({
                "chatId": "chat-1",
                "cardId": card_id,
                "providerId": provider_id,
            }),
        )
        .await
}

#[tokio::test]
async fn a_provider_choice_card_carries_engine_filled_options() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            choose_call("call-1", &["xiaomi", "xiaomi-token-plan-cn", "xiaomi"]),
            ScriptedReply::text("pick one"),
        ],
    )
    .await;
    let found = cards(&engine, "chat-1", "providerChoice").await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["state"], "pending");
    let options = found[0]["options"].as_array().unwrap();
    let ids: Vec<&str> = options
        .iter()
        .map(|option| option["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["xiaomi", "xiaomi-token-plan-cn"]);
    for option in options {
        assert!(!option["name"].as_str().unwrap().is_empty());
        assert!(!option["detail"].as_str().unwrap_or_default().is_empty());
    }
}

#[tokio::test]
async fn unknown_or_single_ids_build_no_choice_card() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            choose_call("call-1", &["xiaomi", "not-a-provider"]),
            choose_call("call-2", &["xiaomi"]),
            ScriptedReply::text("gave up"),
        ],
    )
    .await;
    assert!(cards(&engine, "chat-1", "providerChoice").await.is_empty());
}

#[tokio::test]
async fn settling_a_choice_stamps_it_and_tells_the_chat() {
    let fixture = Fixture::new();
    let (engine, provider) = mode_chat(
        &fixture,
        vec![
            choose_call("call-1", &["xiaomi", "xiaomi-token-plan-cn"]),
            ScriptedReply::text("pick one"),
            ScriptedReply::text("using it"),
        ],
    )
    .await;
    let (mut transcript, _) = common::subscribe(&engine, "chat-1").await;
    run_turn(&engine, &fixture, "update xiaomi").await;
    let card_id = cards(&engine, "chat-1", "providerChoice").await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Not one of the card's options: refused, the card stays pending.
    assert!(settle_choice(&engine, &card_id, "openai").await.is_err());
    assert!(
        settle_choice(&engine, "no-such-card", "xiaomi")
            .await
            .is_err()
    );
    assert_eq!(
        cards(&engine, "chat-1", "providerChoice").await[0]["state"],
        "pending"
    );

    let RpcReply::Value(reply) = settle_choice(&engine, &card_id, "xiaomi-token-plan-cn")
        .await
        .unwrap()
    else {
        panic!("SettleProviderChoice did not return a value");
    };
    assert_eq!(reply["providerId"], "xiaomi-token-plan-cn");
    common::wait_for_transcript_text(&mut transcript, "Use provider xiaomi-token-plan-cn").await;
    wait_for_requests(&provider, 3).await;
    let card = &cards(&engine, "chat-1", "providerChoice").await[0];
    assert_eq!(card["state"], "chosen");
    assert_eq!(card["chosen"], "xiaomi-token-plan-cn");

    // Settled once: a second click is refused.
    assert!(settle_choice(&engine, &card_id, "xiaomi").await.is_err());
}

#[tokio::test]
async fn a_new_turn_supersedes_a_pending_choice() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            choose_call("call-1", &["xiaomi", "xiaomi-token-plan-cn"]),
            ScriptedReply::text("pick one"),
            ScriptedReply::text("the token plan then"),
        ],
    )
    .await;
    run_turn(&engine, &fixture, "the cn token plan").await;
    let card = &cards(&engine, "chat-1", "providerChoice").await[0];
    assert_eq!(card["state"], "superseded");
    let card_id = card["id"].as_str().unwrap().to_string();
    assert!(settle_choice(&engine, &card_id, "xiaomi").await.is_err());
}

#[tokio::test]
async fn a_choice_is_refused_outside_provider_mode() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            choose_call("call-1", &["xiaomi", "xiaomi-token-plan-cn"]),
            ScriptedReply::text("pick one"),
        ],
    )
    .await;
    let card_id = cards(&engine, "chat-1", "providerChoice").await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    call(&engine, methods::EXIT_PROVIDER_MODE, "chat-1").await;
    assert!(settle_choice(&engine, &card_id, "xiaomi").await.is_err());
}

#[tokio::test]
async fn a_proposal_card_names_its_target_provider() {
    let fixture = Fixture::new();
    let (engine, _provider) = mode_turn(
        &fixture,
        vec![
            propose_custom("call-1", "acme"),
            ScriptedReply::text("proposed"),
        ],
    )
    .await;
    let card = &cards(&engine, "chat-1", "modelProposal").await[0];
    let targets = card["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["id"], "acme");
    assert_eq!(targets[0]["name"], "acme");
}
