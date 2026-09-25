//! Handle-seam tests for the provider-retry watch: a Turn's provider
//! requests carry the engine retry budget (`max_retries`) and an `on_retry`
//! callback, and each scheduled retry fans out as a typed
//! `TurnRetryNotice` over `WatchTurnRetry` while the Turn is still live.
//! Live-only: no synthetic initial frame, no replay across restart.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_rpc::{RpcReply, RpcService as _, methods};
use pi_core::agent::types::StreamFn;
use serde_json::{Value, json};

async fn subscribe_notices(
    engine: &holt_engine::LocalEngine,
) -> futures::stream::BoxStream<'static, Value> {
    let RpcReply::Stream(notices) = engine
        .handle(methods::WATCH_TURN_RETRY, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchTurnRetry did not return a stream");
    };
    notices
}

async fn queue_run(engine: &holt_engine::LocalEngine, chat_id: &str, cwd: &str, prompt: &str) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": "m-1",
                    "request": {
                        "prompt": prompt,
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
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

/// Wrap the scripted transport so every Turn request first asserts the
/// engine's retry budget, then simulates two transient provider failures by
/// firing the `on_retry` callback the engine mounted.
fn retrying_transport(provider: &ScriptedProvider) -> (StreamFn, Arc<AtomicU32>) {
    let inner = provider.stream_fn();
    let fired = Arc::new(AtomicU32::new(0));
    let transport_fired = Arc::clone(&fired);
    let stream_fn: StreamFn = Arc::new(move |model, context, options| {
        assert_eq!(
            options.and_then(|options| options.base.base.max_retries),
            Some(10),
            "the Turn's provider requests must carry the engine retry budget"
        );
        // One transport call, two simulated transient failures: the request
        // is sent once and the retry layer re-dials it invisibly to the loop.
        if transport_fired.fetch_add(1, Ordering::SeqCst) == 0 {
            let on_retry = options
                .and_then(|options| options.base.base.on_retry.clone())
                .expect("the engine mounted an on_retry callback");
            on_retry(1, 5, 250, "connection reset by peer");
            on_retry(2, 5, 500, "connection reset by peer");
        }
        inner(model, context, options)
    });
    (stream_fn, fired)
}

#[tokio::test]
async fn a_turns_provider_retries_fan_out_as_live_notices() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer after retries")]);
    let (stream_fn, fired) = retrying_transport(&provider);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(stream_fn),
        search_backend_resolver: None,
    })
    .unwrap();
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    let mut notices = subscribe_notices(&engine).await;

    // No synthetic initial notice on subscribe.
    tokio::time::timeout(std::time::Duration::from_millis(300), notices.next())
        .await
        .expect_err("WatchTurnRetry must open silent");

    queue_run(&engine, "chat-1", &fixture.cwd(), "A").await;

    let first = common::next_frame(&mut notices).await;
    assert_eq!(first["chatId"], "chat-1");
    assert_eq!(first["attempt"], 1);
    assert_eq!(first["maxRetries"], 5);
    assert_eq!(first["delayMs"], 250);
    assert_eq!(first["error"], "connection reset by peer");
    let second = common::next_frame(&mut notices).await;
    assert_eq!(second["attempt"], 2);

    // The Turn still settles normally once the simulated transport stops
    // firing: the retry layer is invisible to the loop and the transcript.
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(transcript.to_string().contains("answer after retries"));
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    tokio::time::timeout(std::time::Duration::from_millis(300), notices.next())
        .await
        .expect_err("no further notices after the Turn settled");
}
