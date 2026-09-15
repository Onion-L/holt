//! Handle-seam tests for the chat usage frame (`WatchChatUsage`, spec ticket
//! 04). Three contracts live here: the occupancy numerator is the latest
//! main-run provider report — its request input plus both cache fields, and
//! never the run accumulator that the same message also feeds — the
//! denominator is the window of the model the chat's queue runs next (the
//! chat's own selection only while the queue holds nothing, and absent when
//! that window is unknown to the catalog, as a custom model's is), and the
//! frame's record count is the runtime's own replay state rather than a
//! fresh read of the ledger file.

mod common;

use std::sync::Arc;

use common::{
    Fixture, ScriptedProvider, ScriptedReply, next_frame, run_prompt, setup_chat,
    wait_for_requests, wait_for_session_status,
};
use futures::StreamExt;
use holt_rpc::{RpcReply, RpcService as _, methods};
use pi_core::ai::types::Usage;
use serde_json::{Value, json};
use tokio::sync::Notify;

/// One frame off the usage watch, pumping until `ready` accepts one — the
/// harness's timeout bounds a frame that never arrives.
async fn frame_until<S>(stream: &mut S, ready: impl Fn(&Value) -> bool) -> Value
where
    S: StreamExt<Item = Value> + Unpin,
{
    loop {
        let frame = next_frame(stream).await;
        if ready(&frame) {
            return frame;
        }
    }
}

async fn usage_watch(engine: &holt_engine::LocalEngine) -> impl StreamExt<Item = Value> + Unpin {
    let RpcReply::Stream(stream) = engine
        .handle(methods::WATCH_CHAT_USAGE, json!({"chatId": "chat-1"}))
        .await
        .unwrap()
    else {
        panic!("WatchChatUsage did not return a stream");
    };
    stream
}

/// The window the engine's own catalog reports for a model — the tests never
/// hardcode a catalog number.
async fn window_of(engine: &holt_engine::LocalEngine, model_id: &str) -> Option<u64> {
    let RpcReply::Value(models) = engine
        .handle(methods::LIST_MODELS, json!({"providerId": "openai"}))
        .await
        .unwrap()
    else {
        panic!("ListModels did not return a value");
    };
    models
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == model_id)
        .unwrap_or_else(|| panic!("no catalog row for {model_id}"))["contextWindow"]
        .as_u64()
}

/// Set the chat's selected model exactly as the model picker does.
async fn select_model(engine: &holt_engine::LocalEngine, model: &str) {
    engine
        .handle(
            methods::MUTATE,
            json!({"op": "setChatConfig", "chatId": "chat-1", "config": {
                "provider": "openai",
                "model": model,
                "reasoning": null,
                "modelOptions": {},
            }}),
        )
        .await
        .unwrap();
}

/// Queue one run with an explicit model — the composer's own shape, and the
/// only way a queue item's model can differ from the chat's selection.
async fn queue_run_on(
    engine: &holt_engine::LocalEngine,
    cwd: &str,
    message_id: &str,
    model: &str,
    prompt: &str,
) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "run",
                    "messageId": message_id,
                    "request": {
                        "prompt": prompt,
                        "provider": "openai",
                        "model": model,
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": cwd,
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn occupancy_is_the_latest_reports_request_input_not_the_run_accumulator() {
    let fixture = Fixture::new();
    // Distinct in every field, so a doubled numerator (the report counted
    // once by the ledger and once by the run accumulator) stands out from
    // the honest 700 + 7 + 3.
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]).with_usage(Usage {
        input: 700,
        output: 70,
        cache_read: 7,
        cache_write: 3,
        total_tokens: 780,
        ..Default::default()
    });
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let mut usage = usage_watch(&engine).await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    let frame = frame_until(&mut usage, |frame| frame["gross"] == json!(780)).await;
    assert_eq!(frame["occupancy"]["tokens"], json!(710), "{frame}");
    assert_eq!(frame["occupancy"]["estimated"], json!(false), "{frame}");
}

#[tokio::test]
async fn occupancy_estimates_from_history_until_a_report_arrives_then_measures() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("first"),
        ScriptedReply::text("second"),
    ])
    .with_usage(Usage {
        input: 300,
        output: 30,
        cache_read: 5,
        cache_write: 2,
        total_tokens: 337,
        ..Default::default()
    });
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    drop(engine);

    // A restart has seen no report for this chat yet: the opening frame says
    // so and replays History through the estimator instead of pretending.
    let engine = fixture.engine(&provider);
    let mut usage = usage_watch(&engine).await;
    let estimated = next_frame(&mut usage).await;
    assert_eq!(
        estimated["occupancy"]["estimated"],
        json!(true),
        "{estimated}"
    );
    assert!(
        estimated["occupancy"]["tokens"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "the estimate replays the persisted History: {estimated}"
    );
    assert_eq!(
        estimated["gross"],
        json!(337),
        "the ledger's total survives the restart even while occupancy falls back to an estimate: {estimated}"
    );

    // The first report of the process flips it to measured, and the number
    // is that report's own request input — cache fields included.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    let measured = frame_until(&mut usage, |frame| {
        frame["occupancy"]["estimated"] == json!(false)
    })
    .await;
    assert_eq!(measured["occupancy"]["tokens"], json!(307), "{measured}");
}

#[tokio::test]
async fn the_denominator_follows_the_queue_head_then_the_selection() {
    let fixture = Fixture::new();
    let gate = Arc::new(Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::gated(gate.clone(), "done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;

    let selected_model = "openai/gpt-5.4";
    let queued_model = "openai/gpt-5.4-pro";
    let selected = window_of(&engine, selected_model)
        .await
        .expect("the catalog reports the selected model's window");
    let queued = window_of(&engine, queued_model)
        .await
        .expect("the catalog reports the queued model's window");
    assert_ne!(
        selected, queued,
        "the fixture needs two models with distinct windows"
    );

    let mut usage = usage_watch(&engine).await;
    // Nothing queued and no selection yet: the frame has no denominator, so
    // the UI falls back to absolute tokens instead of inventing a percentage.
    assert_eq!(
        next_frame(&mut usage).await["occupancy"]["contextWindow"],
        json!(null)
    );

    queue_run_on(&engine, &fixture.cwd(), "m-1", queued_model, "go").await;
    wait_for_requests(&provider, 1).await;
    // The run is running on the queued model; moving the picker elsewhere
    // must not move the denominator out from under it.
    select_model(&engine, selected_model).await;
    let frame = frame_until(&mut usage, |frame| {
        frame["occupancy"]["contextWindow"].as_u64() == Some(queued)
    })
    .await;
    assert_eq!(
        frame["occupancy"]["contextWindow"],
        json!(queued),
        "{frame}"
    );

    // The queue settles empty, so the chat's own selection answers again.
    gate.notify_one();
    let settled = frame_until(&mut usage, |frame| {
        frame["occupancy"]["contextWindow"].as_u64() == Some(selected)
    })
    .await;
    assert_eq!(
        settled["occupancy"]["contextWindow"],
        json!(selected),
        "{settled}"
    );
}

#[tokio::test]
async fn a_custom_model_has_no_denominator() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let custom = "openai/gpt-private-2026-09-01";
    engine
        .handle(
            methods::ADD_PROVIDER_MODEL,
            json!({"providerId": "openai", "modelId": custom}),
        )
        .await
        .unwrap();
    select_model(&engine, custom).await;

    let mut usage = usage_watch(&engine).await;
    let frame = next_frame(&mut usage).await;
    assert_eq!(frame["occupancy"]["contextWindow"], json!(null), "{frame}");
}

#[tokio::test]
async fn the_record_count_is_replay_state_not_a_ledger_read() {
    let fixture = Fixture::new();
    let provider =
        ScriptedProvider::new(vec![ScriptedReply::text("one"), ScriptedReply::text("two")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    for prompt in ["first prompt", "second prompt"] {
        run_prompt(&engine, "chat-1", &fixture.cwd(), prompt).await;
        wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    }
    let mut usage = usage_watch(&engine).await;
    let frame = frame_until(&mut usage, |frame| frame["recordCount"] == json!(2)).await;
    assert_eq!(frame["recordCount"], json!(2), "{frame}");

    // The ledger file goes away: the count is the runtime's own replay
    // state, so a fresh subscription still reports both records rather than
    // re-reading an absent file as zero.
    let ledger = fixture.data_dir.path().join("usage").join("chat-1.jsonl");
    std::fs::rename(&ledger, ledger.with_extension("jsonl.moved")).unwrap();
    let mut reopened = usage_watch(&engine).await;
    let after = next_frame(&mut reopened).await;
    assert_eq!(after["recordCount"], json!(2), "{after}");
    assert_eq!(after["gross"], json!(2 * 122), "{after}");
}
