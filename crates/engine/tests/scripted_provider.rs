//! Handle-seam tests for the scripted provider (History persistence and
//! Compaction, ticket 01): a real engine assembled on a temp data dir with
//! `EngineConfig::stream_fn` injecting the fake transport, driven through
//! the `RpcService` trait exactly as the UI drives it. The scripted
//! provider's recorded requests are what the model would receive — the
//! assertion surface every later History/Compaction test builds on.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use futures::StreamExt;
use holt_doc::{MessagePart, SessionMessageEntry, TranscriptFrame};

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
        let frame: TranscriptFrame =
            serde_json::from_value(common::next_frame(transcript).await).unwrap();
        if frame_mentions(&frame, needle) {
            return;
        }
    }
}

#[tokio::test]
async fn a_scripted_text_reply_streams_into_the_transcript_and_returns_to_idle() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("scripted reply text")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_transcript_text(&mut transcript, "scripted reply text").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Exactly one request reached the "model", and it carried the prompt —
    // an empty prior History, exactly one message.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].len(), 1);
    assert_eq!(
        common::summarize(&requests[0]),
        vec!["user:hello".to_string()]
    );
}

#[tokio::test]
async fn a_scripted_tool_call_runs_against_the_cwd_and_feeds_the_result_back() {
    let fixture = common::Fixture::new();
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
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read the notes").await;
    wait_for_transcript_text(&mut transcript, "done after reading").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Two rounds: the tool-call reply, then the follow-up after the result.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let summary = common::summarize(&requests[1]);
    assert_eq!(summary[0], "user:read the notes");
    assert_eq!(summary[1], "assistant:toolcall:call-1");
    // The executed read rides back as the tool result for call-1 — read
    // from the chat's working directory, not scripted anywhere.
    assert!(
        summary[2].starts_with("toolresult:call-1:"),
        "unexpected tool result: {}",
        summary[2]
    );
    assert!(summary[2].contains("content read from disk"));
    // The closing text reply is not part of any request — it is the answer
    // to this one, and the transcript assertion above already saw it.
}

#[tokio::test]
async fn a_scripted_aborted_stream_lands_its_partial_text() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Aborted {
        partial: "cut short mid-sentence".into(),
    }]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_transcript_text(&mut transcript, "cut short mid-sentence").await;
    // The abort carries an error message, so the session settles on errored
    // — never stuck working.
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_scripted_error_string_surfaces_in_the_transcript() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed("provider exploded".into())]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_transcript_text(&mut transcript, "provider exploded").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "errored").await;
    assert_eq!(provider.requests().len(), 1);
}
