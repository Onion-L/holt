//! Handle-seam tests for automatic Compaction before a Turn (ADR-0011,
//! spec ticket 05): when the History nears the model's context window the
//! next Turn's first request is the summary request (upstream system
//! prompt, no tools, no reasoning), the run after it carries the templated
//! summary message plus the retained tail, the Transcript gains a divider
//! that never removes rows, the compacted record survives a restart, and a
//! failed compaction never blocks the Turn. Usage numbers are fixed by the
//! scripted provider so the threshold (272k window − 16384 reserve) is
//! crossed deterministically; the small fixtures mean every cut point
//! splits a turn, so the summary requests exercise the upstream
//! turn-prefix flavor.

mod common;

use common::{RecordedRequest, ScriptedProvider, ScriptedReply};
use holt_rpc::RpcService as _;
use pi_core::ai::types::{Message, Usage};

/// Past the threshold for openai/gpt-5.4 (272k window − 16384 reserve).
fn overflowing_usage() -> Usage {
    Usage {
        input: 299_000,
        output: 1_000,
        total_tokens: 300_000,
        ..common::fixed_usage()
    }
}

/// A text body of roughly `tokens` estimated tokens (the upstream
/// estimator divides characters by four).
fn big_text(tokens: usize) -> String {
    let chars = tokens * 4;
    let mut text = String::with_capacity(chars);
    while text.len() < chars {
        text.push_str("compactable conversation content keeps going ");
    }
    text
}

fn user_text(message: &Message) -> &str {
    match message {
        Message::User(user) => match &user.content {
            pi_core::ai::types::UserContent::Text(text) => text,
            pi_core::ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .find_map(|block| match block {
                    pi_core::ai::types::BlockContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
        },
        _ => "",
    }
}

/// The summary requests among the recorded ones (by upstream system
/// prompt).
fn summary_requests(requests: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    requests
        .iter()
        .filter(|request| {
            request
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("context summarization assistant"))
        })
        .collect()
}

#[tokio::test]
async fn a_history_past_the_window_compacts_before_the_turn() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::text("checkpoint summary text"),
        ScriptedReply::text("reply after compaction"),
    ])
    .with_usage(overflowing_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Past the window now: the next Turn's FIRST request is the summary
    // request, not an agent request. The small history makes the cut split
    // the turn, so the one summary request is the turn-prefix flavor.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_transcript_text(&mut transcript, "checkpoint summary text").await;

    let requests = provider.requests();
    let summaries = summary_requests(&requests);
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        requests[1].tools, 0,
        "the summary round is not the first request after the turn"
    );
    let summary_request = summaries[0];
    assert_eq!(summary_request.tools, 0, "summary request advertised tools");
    assert_eq!(summary_request.messages.len(), 1);
    let summary_prompt = user_text(&summary_request.messages[0]);
    assert!(
        summary_prompt.contains("<conversation>"),
        "{summary_prompt}"
    );
    assert!(
        summary_prompt.contains("[User]: first prompt"),
        "{summary_prompt}"
    );
    assert!(
        summary_prompt.contains("PREFIX of a turn"),
        "{summary_prompt}"
    );

    // The run's request: the templated summary user message, the retained
    // tail (the big first reply), and the new prompt — nothing else.
    let run = requests.last().unwrap();
    assert_eq!(run.messages.len(), 3);
    let templated = user_text(&run.messages[0]);
    assert!(
        templated.contains("history before this point was compacted"),
        "{templated}"
    );
    assert!(templated.contains("checkpoint summary text"), "{templated}");
    assert!(matches!(&run.messages[1], Message::Assistant(_)));
    assert_eq!(
        common::summarize(std::slice::from_ref(&run.messages[2])),
        ["user:second prompt"]
    );

    // The divider row: summary, tokens before/after, trigger `automatic`.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    assert!(snapshot.contains("compactionDivider"), "{snapshot}");
    assert!(snapshot.contains("tokensBefore"), "{snapshot}");
    assert!(snapshot.contains("tokensAfter"), "{snapshot}");
    assert!(snapshot.contains("\"automatic\""), "{snapshot}");
}

#[tokio::test]
async fn below_the_threshold_no_summary_request_is_made() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("first reply"),
        ScriptedReply::text("second reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "one").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "two").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.tools > 0));
    assert!(summary_requests(&requests).is_empty());
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("compactionDivider"));
}

#[tokio::test]
async fn the_retained_tail_keeps_tool_results_with_their_calls() {
    let fixture = common::Fixture::new();
    // One round, two wall-sized results (each truncates near 50KB, so a
    // single one stays under the recent-tokens budget).
    let wall = serde_json::json!({ "command": "head -c 120000 /dev/zero | tr '\\0' 'a'" });
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![
            common::tool_call("call-1", "bash", wall.clone()),
            common::tool_call("call-2", "bash", wall),
        ]),
        ScriptedReply::text("done with the walls of text"),
        ScriptedReply::text("summary of the early work"),
        ScriptedReply::text("reply after compaction"),
    ])
    .with_usage(overflowing_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // One tool round with huge results, closed by a small reply that
    // carries the overflowing usage.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "capture the walls").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "next prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    // The summarized conversation carries the tool calls AND their results
    // together; the retained tail starts with the closing assistant
    // message — never a tool result — and the walls are gone from the
    // model's verbatim memory.
    let summaries = summary_requests(&requests);
    assert_eq!(summaries.len(), 1);
    let summary_prompt = user_text(&summaries[0].messages[0]);
    assert!(
        summary_prompt.contains("[User]: capture the walls"),
        "{summary_prompt}"
    );
    let run = requests.last().unwrap();
    assert!(matches!(&run.messages[1], Message::Assistant(_)));
    assert_eq!(run.messages.len(), 3);
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(
        !run_text.contains("aaaaaaaaaaaaaaaaaa"),
        "tail kept the walls"
    );
}

#[tokio::test]
async fn a_second_compaction_chains_the_previous_summary() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::text("checkpoint summary text"),
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::text("history summary two"),
        ScriptedReply::text("turn-prefix summary two"),
        ScriptedReply::text("third reply"),
    ])
    .with_usage(overflowing_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    for prompt in ["first prompt", "second prompt", "third prompt"] {
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), prompt).await;
        common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    }

    // The second compaction's history-summary request chains the first
    // summary in <previous-summary>.
    let requests = provider.requests();
    let chained: Vec<&RecordedRequest> = summary_requests(&requests)
        .into_iter()
        .filter(|request| user_text(&request.messages[0]).contains("<previous-summary>"))
        .collect();
    assert_eq!(chained.len(), 1, "expected exactly one chained summary");
    let chained_prompt = user_text(&chained[0].messages[0]);
    assert!(
        chained_prompt.contains("checkpoint summary text"),
        "{chained_prompt}"
    );
}

#[tokio::test]
async fn the_compacted_history_survives_a_restart() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::text("the one summary"),
        ScriptedReply::text("second reply"),
        ScriptedReply::text("history summary after restart"),
        ScriptedReply::text("turn-prefix after restart"),
        ScriptedReply::text("third reply"),
    ])
    .with_usage(overflowing_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    drop(engine);

    // A fresh engine replays the compacted record: the restarted
    // compaction chains the persisted summary, and the run request carries
    // the summary message and the tail — not the pre-compaction History.
    // (The pinned usage fires one more compaction first; that is part of
    // the assertion.)
    let engine = fixture.engine(&provider);
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "third prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let chained_prompt = summary_requests(&requests)
        .into_iter()
        .map(|request| user_text(&request.messages[0]))
        .find(|prompt| prompt.contains("<previous-summary>"))
        .expect("the restart's compaction did not chain a previous summary");
    assert!(
        chained_prompt.contains("the one summary"),
        "{chained_prompt}"
    );

    let run = requests.last().unwrap();
    let templated = user_text(&run.messages[0]);
    assert!(
        templated.contains("history before this point was compacted"),
        "{templated}"
    );
    let run_text = serde_json::to_string(&run.messages).unwrap();
    assert!(
        !run_text.contains("first prompt"),
        "the summarized-away prompt leaked back: {run_text}"
    );
}

#[tokio::test]
async fn a_failed_summary_lets_the_turn_proceed_with_a_notice() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::Failed("summarizer exploded".into()),
        ScriptedReply::text("the turn still ran"),
    ])
    .with_usage(overflowing_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The failure is visible, and the Turn ran uncompacted: the run
    // request carries the FULL history plus the new prompt.
    common::wait_for_transcript_text(&mut transcript, "Automatic compaction failed").await;
    let requests = provider.requests();
    let run = requests.last().unwrap();
    let summary = common::summarize(&run.messages);
    assert_eq!(summary[0], "user:first prompt");
    assert_eq!(summary.last().unwrap(), "user:second prompt");
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!snapshot.to_string().contains("compactionDivider"));
}

#[tokio::test]
async fn compaction_stamps_nothing_and_keeps_the_turns_diff_baseline() {
    // A real repo, so the Turn's baseline and turn diff are live: the
    // compacting Turn edits a tracked file mid-run, and the turn diff must
    // still see it — proof compaction reset nothing Turn-scoped (ADR-0011).
    let fixture = common::Fixture::new();
    let repo = git2::Repository::init(fixture.project_dir.path()).unwrap();
    repo.set_head("refs/heads/main").unwrap();
    {
        let mut index = repo.index().unwrap();
        std::fs::write(fixture.project_dir.path().join("tracked.txt"), "before\n").unwrap();
        index.add_path(std::path::Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Holt Test", "holt-test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
    }
    drop(repo);

    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::text("checkpoint summary text"),
        ScriptedReply::tool_call(
            "call-1",
            "write",
            serde_json::json!({ "path": "tracked.txt", "content": "edited during the compacting turn\n" }),
        ),
        ScriptedReply::text("edited"),
    ])
    .with_usage(overflowing_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    // The compacting Turn runs a write after the compaction.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "edit the file").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(summary_requests(&provider.requests()).len(), 1);

    // The chat row keeps the branch the Turn stamped — compaction did not
    // restamp it — and the turn diff (captured at acceptance, before the
    // compaction and the edit) still shows the edit.
    let holt_rpc::RpcReply::Value(diff) = engine
        .handle(
            holt_rpc::methods::GET_CHECKOUT_DIFF,
            serde_json::json!({ "cwd": fixture.cwd(), "mode": "turn", "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("GetCheckoutDiff did not return a value");
    };
    let diff = diff.to_string();
    assert!(
        diff.contains("tracked.txt"),
        "turn diff lost the edit: {diff}"
    );
}
