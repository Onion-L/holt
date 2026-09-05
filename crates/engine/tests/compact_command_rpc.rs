//! Handle-seam tests for the manual `/compact` slash command (ADR-0011,
//! spec ticket 07): a typed command on the existing queue path — never
//! prompt text — with a `Compacting` session status (interruptible like a
//! run), a `manual` divider on success, "nothing to compact" and
//! "already running" refusals, and History-untouched failures.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt;
use holt_rpc::{RpcError, RpcReply, RpcService as _, methods};
use pi_core::ai::types::Usage;

/// Queue a `/compact` exactly as the composer serializes it.
async fn compact(engine: &holt_engine::LocalEngine, cwd: &str) -> Result<RpcReply, RpcError> {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "compact",
                    "request": {
                        "prompt": "",
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
}

/// Every status the chat-1 session row passed through, in order (the
/// opening snapshot stripped).
async fn status_history<S>(sessions: &mut S, until: &str) -> Vec<String>
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    let mut statuses = Vec::new();
    loop {
        let frame = common::next_frame(sessions).await;
        if let Some(rows) = frame.as_array() {
            for row in rows {
                if row["chatId"] == "chat-1" {
                    let status = row["status"].as_str().unwrap_or_default().to_string();
                    if status != statuses.last().cloned().unwrap_or_default() {
                        statuses.push(status);
                    }
                }
            }
        }
        if statuses.last().is_some_and(|status| status == until) {
            return statuses;
        }
    }
}

#[tokio::test]
async fn compact_runs_as_a_typed_command_with_a_manual_divider() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        // A first Turn big enough that the whole History cannot fit the
        // retained tail (the manual gate is the cut, not the threshold).
        ScriptedReply::text_with_usage(
            {
                let mut text = String::new();
                while text.len() < 120_000 {
                    text.push_str("manual compaction conversation body ");
                }
                text
            },
            Usage {
                input: 1_000,
                output: 1_000,
                total_tokens: 2_000,
                ..common::fixed_usage()
            },
        ),
        ScriptedReply::text("the manual summary"),
        ScriptedReply::text("next turn reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a long conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    compact(&engine, &fixture.cwd()).await.unwrap();
    let statuses = status_history(&mut sessions, "idle").await;
    assert_eq!(statuses, ["compacting", "idle"], "unexpected status trail");

    // The provider saw exactly ONE request for the compaction — a summary
    // request; the raw `/compact` text appears nowhere.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tools, 0);
    let serialized = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(!serialized.contains("/compact"), "{serialized}");

    // The divider carries trigger `manual`, and the next Turn's request
    // leads with the templated summary.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    assert!(snapshot.contains("compactionDivider"), "{snapshot}");
    assert!(snapshot.contains("\"manual\""), "{snapshot}");

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "after compacting").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let run = requests.last().unwrap();
    let summary = common::summarize(&run.messages);
    assert!(
        summary[0].starts_with("user:"),
        "the templated summary should lead: {summary:?}"
    );
    assert!(
        summary.len() < 4,
        "the history should have shrunk: {summary:?}"
    );
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(
        run_text.contains("history before this point was compacted"),
        "{run_text}"
    );
}

#[tokio::test]
async fn compact_is_refused_while_a_turn_runs() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::text("never")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a hanging turn").await;
    common::wait_for_requests(&provider, 1).await;
    let error = match compact(&engine, &fixture.cwd()).await {
        Err(error) => error,
        Ok(_) => panic!("/compact was accepted while a Turn runs"),
    };
    assert!(error.to_string().contains("already running"), "{error}");
}

#[tokio::test]
async fn compact_on_a_short_chat_reports_nothing_to_compact() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("a short reply")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hi").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests_before = provider.requests().len();

    let error = match compact(&engine, &fixture.cwd()).await {
        Err(error) => error,
        Ok(_) => panic!("short chat compacted"),
    };
    assert!(error.to_string().contains("nothing to compact"), "{error}");
    // No model request, no status change, no divider.
    assert_eq!(provider.requests().len(), requests_before);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("compactionDivider"));
}

#[tokio::test]
async fn a_failed_manual_compaction_leaves_the_history_untouched() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text({
            let mut text = String::new();
            while text.len() < 120_000 {
                text.push_str("manual compaction conversation body ");
            }
            text
        }),
        ScriptedReply::Failed("the summarizer refused".into()),
        ScriptedReply::text("kept the full conversation"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a long conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    compact(&engine, &fixture.cwd()).await.unwrap();
    let statuses = status_history(&mut sessions, "idle").await;
    assert_eq!(statuses, ["compacting", "idle"]);
    common::wait_for_transcript_text(&mut transcript, "Compaction failed").await;

    // The failure surfaces, and the next Turn carries the UNCOMPACTED
    // History — nothing was replaced.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "continue").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let run = requests.last().unwrap();
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(
        !run_text.contains("history before this point was compacted"),
        "{run_text}"
    );
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("compactionDivider"));
}

#[tokio::test]
async fn interrupting_a_manual_compaction_settles_idle_with_history_intact() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text({
            let mut text = String::new();
            while text.len() < 120_000 {
                text.push_str("manual compaction conversation body ");
            }
            text
        }),
        // The summary request hangs — interrupted from the UI's Stop.
        ScriptedReply::Silent,
        ScriptedReply::text("still uncompacted"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a long conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    compact(&engine, &fixture.cwd()).await.unwrap();
    // Wait for the Compacting status, then interrupt like the composer's
    // Stop does.
    let statuses = status_history(&mut sessions, "compacting").await;
    assert_eq!(statuses, ["compacting"]);
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-1",
                "command": { "kind": "interrupt" }
            }),
        )
        .await
        .unwrap();
    let statuses = status_history(&mut sessions, "idle").await;
    assert_eq!(statuses, ["idle"]);

    // The History is unchanged — the next request is the full,
    // uncompacted conversation.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "after the interrupt").await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let run = requests.last().unwrap();
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(
        !run_text.contains("history before this point was compacted"),
        "{run_text}"
    );
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("compactionDivider"));
}
