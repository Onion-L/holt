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
    for name in ["model_proposal", "request_provider_key"] {
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
        ["model_proposal", "request_provider_key", "web_fetch"]
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
