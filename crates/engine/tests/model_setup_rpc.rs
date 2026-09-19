//! The model-setup surface at the RPC seam (design-v2): the fixed flow runs
//! in a dedicated `model-setup` chat whose toolset touches no files —
//! `model_proposal` is the only catalog capability, and the write path is
//! the review panel's `ApplyModelProposal` RPC, never an agent tool.
//! Normal chats mount neither tool; an apply is a harmless no-op error.

mod common;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_rpc::{RpcReply, RpcService, methods};

/// One complete, servable record — the fixture every scripted proposal
/// carries.
fn record_json(provider: &str, id: &str, base_url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "api": "openai-completions",
        "provider": provider,
        "baseUrl": base_url,
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 1.0, "output": 2.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
        "contextWindow": 321_000,
        "maxTokens": 16_384,
    })
}

fn propose_call(call_id: &str, provider: &str, model: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        call_id,
        "model_proposal",
        serde_json::json!({
            "changes": [{
                "action": "upsert_model_record",
                "providerId": provider,
                "record": record_json(provider, model, "https://api.openai.com/v1"),
            }],
        }),
    )
}

/// Starts the session-scoped setup chat the dialog drives.
async fn start_setup_chat(
    engine: &holt_engine::LocalEngine,
    provider: &str,
    model: &str,
) -> String {
    let RpcReply::Value(reply) = engine
        .handle(
            methods::START_MODEL_SETUP_CHAT,
            serde_json::json!({ "provider": provider, "model": model }),
        )
        .await
        .unwrap()
    else {
        panic!("StartModelSetupChat did not return a value");
    };
    reply["chatId"].as_str().unwrap().to_string()
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        serde_json::Value::Object(map) => map.values().for_each(|item| collect_strings(item, out)),
        _ => {}
    }
}

fn extract_proposal_id(text: &str) -> Option<String> {
    let tail = text.split("proposalId: ").nth(1)?;
    let id: String = tail
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    (id.len() == 36).then_some(id)
}

/// Runs one propose Turn on the setup chat and lifts the stored proposal id.
async fn propose_and_extract_id(
    engine: &holt_engine::LocalEngine,
    fixture: &Fixture,
    chat_id: &str,
) -> String {
    let (_, mut sessions) = common::subscribe(engine, chat_id).await;
    common::run_prompt(engine, chat_id, &fixture.cwd(), "add it").await;
    common::wait_for_session_status(&mut sessions, chat_id, "idle").await;
    let snapshot = common::transcript_snapshot(engine, chat_id).await;
    let mut strings = Vec::new();
    collect_strings(&snapshot, &mut strings);
    strings
        .iter()
        .find_map(|text| extract_proposal_id(text))
        .expect("the proposal tool output carries a proposalId")
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

#[tokio::test]
async fn a_setup_chat_proposes_and_the_review_rpc_applies() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        propose_call("call-1", "openai", "gpt-via-setup"),
        ScriptedReply::text("proposed"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let setup_id = start_setup_chat(&engine, "openai", "openai/gpt-5.4").await;
    let id = propose_and_extract_id(&engine, &fixture, &setup_id).await;

    // The review panel's data: structured changes, newest first.
    let RpcReply::Value(proposals) = engine
        .handle(
            methods::LIST_MODEL_PROPOSALS,
            serde_json::json!({ "chatId": setup_id }),
        )
        .await
        .unwrap()
    else {
        panic!("ListModelProposals did not return a value");
    };
    let proposals = proposals.as_array().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0]["id"], id.as_str());
    assert!(proposals[0]["summary"].as_str().unwrap().contains("openai"));
    assert_eq!(proposals[0]["changes"][0]["action"], "upsert_model_record");
    assert_eq!(proposals[0]["changes"][0]["modelId"], "gpt-via-setup");
    // The panel's session cutoff rides the creation stamp: present, and
    // no later than now (a future stamp would leak into later sessions).
    assert!(
        proposals[0]["createdAt"]
            .as_i64()
            .is_some_and(|at| at > 0 && at <= chrono::Utc::now().timestamp_millis())
    );

    // Nothing applied yet: the proposal is stored, not written.
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .all(|row| row["id"] != "openai/gpt-via-setup")
    );

    // The write path is the button, and it lands live.
    let RpcReply::Value(applied) = engine
        .handle(
            methods::APPLY_MODEL_PROPOSAL,
            serde_json::json!({ "chatId": setup_id, "proposalId": id }),
        )
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
            .any(|row| row == "openai/gpt-via-setup")
    );
    let row = list_models(&engine, "openai")
        .await
        .into_iter()
        .find(|row| row["id"] == "openai/gpt-via-setup")
        .expect("the applied record is live");
    assert_eq!(row["contextWindow"], 321_000);

    // The apply consumed the proposal: the review panel must not keep
    // offering Write on an already-written change (a second Write is
    // rejected as a staleness no-op).
    let RpcReply::Value(proposals) = engine
        .handle(
            methods::LIST_MODEL_PROPOSALS,
            serde_json::json!({ "chatId": setup_id }),
        )
        .await
        .unwrap()
    else {
        panic!("ListModelProposals did not return a value");
    };
    assert!(proposals.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_stale_proposal_is_rejected_by_the_review_rpc() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        propose_call("call-1", "openai", "gpt-stale"),
        ScriptedReply::text("proposed"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let setup_id = start_setup_chat(&engine, "openai", "openai/gpt-5.4").await;
    let id = propose_and_extract_id(&engine, &fixture, &setup_id).await;

    // The catalog moves under the proposal: the same record lands via RPC.
    engine
        .handle(
            methods::SAVE_MODEL_RECORD,
            serde_json::json!({
                "providerId": "openai",
                "record": record_json("openai", "gpt-stale", "https://api.openai.com/v1"),
            }),
        )
        .await
        .unwrap();

    let reply = engine
        .handle(
            methods::APPLY_MODEL_PROPOSAL,
            serde_json::json!({ "chatId": setup_id, "proposalId": id }),
        )
        .await;
    let Err(error) = reply else {
        panic!("the stale apply must fail");
    };
    assert!(
        error
            .to_string()
            .contains("catalog changed since this proposal"),
        "the staleness rejection surfaced: {error}"
    );
    // The RPC-written record is intact — the stale apply changed nothing.
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .any(|row| row["id"] == "openai/gpt-stale")
    );
}

#[tokio::test]
async fn unknown_proposals_and_chats_are_rejected() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;
    let setup_id = start_setup_chat(&engine, "openai", "openai/gpt-5.4").await;

    assert!(
        engine
            .handle(
                methods::APPLY_MODEL_PROPOSAL,
                serde_json::json!({ "chatId": setup_id, "proposalId": "not-a-proposal" }),
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .handle(
                methods::LIST_MODEL_PROPOSALS,
                serde_json::json!({ "chatId": "../escape" }),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn each_setup_session_gets_a_fresh_chat_and_the_previous_one_is_deleted() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    common::setup_chat(&engine, "chat-1").await;

    let first = start_setup_chat(&engine, "openai", "openai/gpt-5.4").await;
    // The next dialog session starts a NEW chat — no conversation memory
    // carries across opens — and the previous one is deleted outright.
    let second = start_setup_chat(&engine, "zai-coding-cn", "zai-coding-cn/glm-5.3").await;
    assert_ne!(first, second);

    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let frame = common::next_frame(&mut chats).await;
    let rows = frame.as_array().unwrap();
    assert!(
        !rows.iter().any(|row| row["id"] == first),
        "the previous session's setup chat is deleted"
    );
    let current = rows
        .iter()
        .find(|row| row["id"] == second)
        .expect("the fresh setup chat is listed");
    assert_eq!(current["archived"], true);
    assert_eq!(current["config"]["scope"], "model-setup");
    assert_eq!(current["config"]["model"], "zai-coding-cn/glm-5.3");
}

#[tokio::test]
async fn a_discarded_proposal_cannot_be_applied() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        propose_call("call-1", "openai", "gpt-discarded"),
        ScriptedReply::text("proposed"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let setup_id = start_setup_chat(&engine, "openai", "openai/gpt-5.4").await;
    let id = propose_and_extract_id(&engine, &fixture, &setup_id).await;

    let RpcReply::Value(reply) = engine
        .handle(
            methods::DISCARD_MODEL_PROPOSAL,
            serde_json::json!({ "chatId": setup_id, "proposalId": id }),
        )
        .await
        .unwrap()
    else {
        panic!("DiscardModelProposal did not return a value");
    };
    assert_eq!(reply["discarded"], true);
    // The list is empty and the write path refuses the ghost.
    let RpcReply::Value(proposals) = engine
        .handle(
            methods::LIST_MODEL_PROPOSALS,
            serde_json::json!({ "chatId": setup_id }),
        )
        .await
        .unwrap()
    else {
        panic!("ListModelProposals did not return a value");
    };
    assert!(proposals.as_array().unwrap().is_empty());
    assert!(
        engine
            .handle(
                methods::APPLY_MODEL_PROPOSAL,
                serde_json::json!({ "chatId": setup_id, "proposalId": id }),
            )
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

/// The setup dialog's exact RPC sequence (issue 03): the doc watch is
/// subscribed BEFORE the send (the UI's `watch_subagent_doc`), and the send
/// rides `QueueCommand` with the picker's provider-qualified model. The
/// turn must land in the doc stream — the user entry first, then the
/// reply — or the dialog stays on its placeholder forever.
#[tokio::test]
async fn the_setup_dialogs_doc_watch_streams_the_sent_turn() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("researched")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let setup_id = start_setup_chat(&engine, "openai", "openai/gpt-5.4").await;

    // The dialog's feed opens first, exactly as `watch_subagent_doc` does —
    // the opening reset is drained, everything after is the live turn.
    let (mut transcript, mut sessions) = common::subscribe(&engine, &setup_id).await;

    // The dialog's send: the composer's `run` payload shape.
    common::run_prompt(&engine, &setup_id, &fixture.cwd(), "add it").await;

    common::wait_for_transcript_text(&mut transcript, "add it").await;
    common::wait_for_transcript_text(&mut transcript, "researched").await;
    common::wait_for_session_status(&mut sessions, &setup_id, "idle").await;
}

#[tokio::test]
async fn normal_chats_reject_the_model_setup_tools() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "model_apply",
            serde_json::json!({ "proposalId": "whatever" }),
        ),
        ScriptedReply::tool_call(
            "call-2",
            "model_proposal",
            serde_json::json!({ "providerId": "openai" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "set it up").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Neither tool is mounted on a normal chat: the calls settle as error
    // tool results (no gate, no proposal, no write), and the model reads
    // the not-found errors.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    for call in ["call-1", "call-2"] {
        let empty = Vec::new();
        let part = snapshot["reset"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["parts"].as_array().unwrap_or(&empty))
            .find(|part| part["id"] == call)
            .expect("the tool part exists");
        assert_eq!(part["isError"], true, "{call} must settle as an error");
    }
    let requests = provider.requests();
    assert!(requests.iter().any(|request| {
        common::summarize(&request.messages)
            .iter()
            .any(|row| row.contains("Tool model_apply not found"))
    }));
    assert!(
        list_models(&engine, "openai")
            .await
            .iter()
            .all(|row| row["id"] != "openai/gpt-via-setup")
    );
}
