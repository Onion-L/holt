//! Handle-seam tests for the scripted provider (History persistence and
//! Compaction, ticket 01): a real engine assembled on a temp data dir with
//! `EngineConfig::stream_fn` injecting the fake transport, driven through
//! the `RpcService` trait exactly as the UI drives it. The scripted
//! provider's recorded requests are what the model would receive — the
//! assertion surface every later History/Compaction test builds on.

mod common;

use std::time::Duration;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt;
use holt_doc::{MessagePart, SessionMessageEntry, TranscriptFrame};
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};
use pi_core::ai::types::Message;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(10);

struct Fixture {
    /// The chat's working directory.
    project_dir: TempDir,
    /// The personal skill root override — pinned empty so the system prompt
    /// stays fixture-driven, not machine-driven.
    personal_dir: TempDir,
    data_dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            project_dir: TempDir::new().unwrap(),
            personal_dir: TempDir::new().unwrap(),
            data_dir: TempDir::new().unwrap(),
        }
    }

    fn engine(&self, provider: &ScriptedProvider) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: Some(self.personal_dir.path().to_path_buf()),
            stream_fn: Some(provider.stream_fn()),
        })
        .unwrap()
    }

    fn cwd(&self) -> String {
        self.project_dir.path().display().to_string()
    }
}

/// One frame off a watch, with a timeout so a silent engine fails the test
/// instead of hanging it.
async fn next_frame<S>(stream: &mut S) -> serde_json::Value
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("timed out waiting for a watch frame")
        .expect("watch stream ended")
}

/// Configure the provider key and create the chat the runs target.
async fn setup_chat(engine: &LocalEngine, chat_id: &str) {
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": chat_id }),
        )
        .await
        .unwrap();
}

/// Queue a run command exactly as the composer serializes it.
async fn run_prompt(engine: &LocalEngine, chat_id: &str, cwd: &str, prompt: &str) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": format!("message-{prompt}"),
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

/// Pump session frames until `chat_id` carries `status`.
async fn wait_for_session_status<S>(sessions: &mut S, chat_id: &str, status: &str)
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    loop {
        let frame = next_frame(sessions).await;
        let hit = frame.as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["chatId"] == chat_id && row["status"] == status)
        });
        if hit {
            return;
        }
    }
}

fn entry_mentions(entry: &SessionMessageEntry, needle: &str) -> bool {
    entry.parts.iter().any(|part| match part {
        MessagePart::Text { text, .. } => text.contains(needle),
        MessagePart::Error { message, .. } => message.contains(needle),
        _ => false,
    })
}

fn frame_mentions(frame: &TranscriptFrame, needle: &str) -> bool {
    match frame {
        TranscriptFrame::Reset { reset } => reset.iter().any(|e| entry_mentions(e, needle)),
        TranscriptFrame::Delta { upsert, append, .. } => {
            upsert.iter().any(|u| entry_mentions(&u.entry, needle))
                || append.iter().any(|a| a.text.contains(needle))
        }
    }
}

/// Pump transcript frames until some entry or append carries `needle`.
async fn wait_for_transcript_text<S>(transcript: &mut S, needle: &str)
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    loop {
        let frame: TranscriptFrame = serde_json::from_value(next_frame(transcript).await).unwrap();
        if frame_mentions(&frame, needle) {
            return;
        }
    }
}

/// Subscribe both watches and drain their opening frames (empty reset, empty
/// sessions) so the first frame after a queued command is signal, not noise.
async fn subscribe(
    engine: &LocalEngine,
    chat_id: &str,
) -> (
    impl StreamExt<Item = serde_json::Value> + Unpin,
    impl StreamExt<Item = serde_json::Value> + Unpin,
) {
    let RpcReply::Stream(mut transcript) = engine
        .handle(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchDocMessages did not return a stream");
    };
    assert_eq!(
        transcript.next().await.unwrap(),
        serde_json::json!({ "reset": [] })
    );
    let RpcReply::Stream(mut sessions) = engine
        .handle(methods::WATCH_SESSIONS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSessions did not return a stream");
    };
    assert_eq!(sessions.next().await.unwrap(), serde_json::json!([]));
    (transcript, sessions)
}

#[tokio::test]
async fn a_scripted_text_reply_streams_into_the_transcript_and_returns_to_idle() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("scripted reply text")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_transcript_text(&mut transcript, "scripted reply text").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Exactly one request reached the "model", and it carried the prompt.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(matches!(&requests[0][requests[0].len() - 1],
        Message::User(message) if message.content.text().contains("hello")));
}

#[tokio::test]
async fn a_scripted_tool_call_runs_against_the_cwd_and_feeds_the_result_back() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project_dir.path().join("notes.txt"),
        "content read from disk\n",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "read", serde_json::json!({ "path": "notes.txt" })),
        ScriptedReply::text("done after reading"),
    ]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "read the notes").await;
    wait_for_transcript_text(&mut transcript, "done after reading").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Two rounds: the tool-call reply, then the follow-up after the result.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    // The second request repeats the prompt and the tool call…
    assert!(second.iter().any(|message| matches!(message,
        Message::User(user) if user.content.text().contains("read the notes"))));
    assert!(second.iter().any(|message| matches!(message,
        Message::Assistant(assistant) if assistant
            .content
            .iter()
            .any(|block| matches!(block,
                pi_core::ai::types::AssistantContent::ToolCall(call) if call.id == "call-1")))));
    // …and carries the executed read as the tool result for call-1.
    assert!(second.iter().any(|message| matches!(message,
        Message::ToolResult(result)
            if result.tool_call_id == "call-1"
                && result.tool_name == "read"
                && result
                    .content
                    .iter()
                    .any(|block| matches!(block,
                        pi_core::ai::types::BlockContent::Text(text)
                            if text.text.contains("content read from disk"))))));
}

#[tokio::test]
async fn a_scripted_aborted_stream_lands_its_partial_text() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Aborted {
        partial: "cut short mid-sentence".into(),
    }]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_transcript_text(&mut transcript, "cut short mid-sentence").await;
    // The abort carries an error message, so the session settles on errored
    // — never stuck working.
    wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_scripted_error_string_surfaces_in_the_transcript() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed("provider exploded".into())]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_transcript_text(&mut transcript, "provider exploded").await;
    wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    assert_eq!(provider.requests().len(), 1);
}
