//! Handle-seam tests for the Last-message edit (ADR-0033): submission
//! cancels the active Turn, prunes its later Transcript and History while
//! keeping the message identity, and reruns the replacement on the
//! repaired records.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::json;

/// The latest user entry's id in a transcript watch frame.
fn latest_user_id(snapshot: &serde_json::Value) -> String {
    snapshot["reset"]
        .as_array()
        .expect("transcript frame carries a reset array")
        .iter()
        .rev()
        .find(|entry| entry["role"] == "user")
        .and_then(|entry| entry["id"].as_str())
        .expect("a user entry")
        .to_string()
}

#[tokio::test]
async fn editing_the_latest_message_cancels_its_turn_and_reruns_on_the_repaired_history() {
    let fixture = Fixture::new();
    let observed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Cancelling {
            observed: observed.clone(),
            finish: finish.clone(),
        },
        ScriptedReply::text("fresh answer"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, _sessions) = common::subscribe(&engine, "chat-1").await;

    // Hold the first Turn mid-run so the edit has a live Turn to cancel.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first draft").await;
    common::wait_for_requests(&provider, 1).await;
    let message_id = latest_user_id(&common::transcript_snapshot(&engine, "chat-1").await);

    // The edit blocks on the old Turn's cleanup, which the harness holds
    // until `finish` — so drive both concurrently, exactly like an
    // interrupt racing a stalled transport.
    let mut edit = std::pin::pin!(engine.handle(
        methods::EDIT_LAST_MESSAGE,
        json!({
            "chatId": "chat-1",
            "messageId": message_id,
            "prompt": "edited prompt",
        }),
    ));
    let edit_reply = tokio::select! {
        reply = &mut edit => reply.expect("edit accepted"),
        _ = observed.notified() => {
            finish.notify_one();
            (&mut edit).await.expect("edit accepted")
        }
    };
    let RpcReply::Value(reply) = edit_reply else {
        panic!("EditLastMessage did not return a value");
    };
    assert_eq!(reply["messageId"], json!(message_id));

    // The replacement Turn ran, and the cancelled Turn's rows are gone:
    // the transcript keeps the identity under the edited text.
    common::wait_for_transcript_text(&mut transcript, "fresh answer").await;
    let settled = common::transcript_snapshot(&engine, "chat-1").await;
    let text = settled.to_string();
    assert!(text.contains("edited prompt"));
    assert!(!text.contains("first draft"));
    assert!(!text.contains("partial A"));
    assert_eq!(latest_user_id(&settled), message_id);

    // The repair reached the model: the replacement request carries the
    // edited prompt as its last user message and no draft anywhere.
    common::wait_for_requests(&provider, 2).await;
    let replacement = &provider.requests()[1];
    let mut user_texts = replacement
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_core::ai::types::Message::User(message) => Some(message.content.text().to_string()),
            _ => None,
        });
    assert_eq!(user_texts.next_back().as_deref(), Some("edited prompt"));
    assert!(user_texts.all(|text| !text.contains("first draft")));
}

#[tokio::test]
async fn an_id_that_is_not_the_latest_user_message_is_rejected() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_requests(&provider, 1).await;

    let error = match engine
        .handle(
            methods::EDIT_LAST_MESSAGE,
            json!({
                "chatId": "chat-1",
                "messageId": "message-never-queued",
                "prompt": "edited prompt",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a stale id is rejected"),
    };
    assert!(
        error.to_string().contains("only the latest user message"),
        "unexpected error: {error}"
    );

    // The rejected edit left the conversation and the queue untouched.
    let settled = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(settled.to_string().contains("hello"));
    assert!(settled.to_string().contains("answer"));
}
