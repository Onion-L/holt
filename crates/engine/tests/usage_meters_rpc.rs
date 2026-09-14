//! Handle-seam tests for the metered call sites (usage-ledger spec, ticket
//! 02): every metered provider round-trip a chat causes lands in its usage
//! ledger with the right attribution `kind` — the main loop's own rounds
//! (`turn`), auto-review passes (`auto-review`), Compaction summaries
//! (`compaction`, batched inside a Turn and immediate outside one), the
//! Title task (`title`), and every round-trip a Subagent causes (its own
//! Compaction and auto-review calls included, one `subagent` kind stamped
//! with the child doc id, booked into the PARENT ledger). Nothing here
//! changes what the unmetered behavior produces: verdicts, summaries, and
//! titles are asserted alongside the records.

mod common;

use std::time::Duration;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService as _, methods};
use pi_core::ai::types::{Usage, UsageCost};
use serde_json::{Value, json};

/// Distinctive usage so a record's origin is unambiguous — every scripted
/// reply reports this unless a case overrides it.
fn pinned_usage() -> Usage {
    Usage {
        input: 700,
        output: 70,
        cache_read: 7,
        cache_write: 3,
        total_tokens: 780,
        cost: UsageCost {
            input: pi_core::ai::types::JsF64(0.01),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Past the compaction threshold for openai/gpt-5.4 (272k window − 16384
/// reserve) but under the window, so automatic Compaction fires and no
/// silent-overflow fallback muddies the kind.
fn overflowing_usage() -> Usage {
    Usage {
        input: 260_000,
        output: 500,
        total_tokens: 260_500,
        ..Default::default()
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

/// The ledger's records, header skipped — the on-disk JSONL contract.
fn read_ledger(data_dir: &std::path::Path, chat_id: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(data_dir.join("usage").join(format!("{chat_id}.jsonl")))
        .unwrap_or_else(|error| panic!("usage ledger for {chat_id}: {error}"));
    let mut lines = text.lines();
    assert!(
        lines.next().is_some(),
        "the ledger carries a version header"
    );
    lines
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The records of one kind, in write order.
fn kind<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["kind"] == kind)
        .collect()
}

/// Poll the ledger until it holds a record of `kind` (the title task and a
/// manual Compaction write outside the Turn's batch, so their records can
/// land after the Turn's terminal event).
async fn wait_for_kind(data_dir: &std::path::Path, chat_id: &str, wanted: &str) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let records = read_ledger(data_dir, chat_id);
        if !kind(&records, wanted).is_empty() {
            return records;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {wanted} record appeared; ledger: {records:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Queue a `run` command with an explicit message id.
async fn queue_run(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    cwd: &str,
    message_id: &str,
    prompt: &str,
) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": message_id,
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

async fn set_permission_mode(engine: &holt_engine::LocalEngine, chat_id: &str, mode: &str) {
    engine
        .handle(
            methods::MUTATE,
            json!({ "op": "setChatPermissionMode", "chatId": chat_id, "mode": mode }),
        )
        .await
        .unwrap();
}

async fn subscribe_events(
    engine: &holt_engine::LocalEngine,
) -> futures::stream::BoxStream<'static, Value> {
    let holt_rpc::RpcReply::Stream(events) = engine
        .handle(methods::WATCH_TURN_TERMINAL_EVENTS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchTurnTerminalEvents did not return a stream");
    };
    events
}

// ---------------------------------------------------------------------------
// Auto-review
// ---------------------------------------------------------------------------

/// One Turn with two mutating calls: the reviewer passes the first and
/// rejects the second. Both passes are metered round-trips — approved and
/// rejected alike — while the verdict behavior itself is unchanged.
#[tokio::test]
async fn auto_review_passes_and_rejections_are_metered() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "bash", json!({ "command": "echo hi" })),
        ScriptedReply::text("APPROVE"),
        ScriptedReply::tool_call(
            "call-2",
            "write",
            json!({ "path": "nope.txt", "content": "x" }),
        ),
        ScriptedReply::text("REJECT: use pnpm, not npm"),
        ScriptedReply::text("done"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    set_permission_mode(&engine, "chat-1", "auto-review").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "go").await;
    // The verdicts are unchanged: the first call executed, the second did
    // not.
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewPassed").await;
    common::wait_for_gate(&engine, "chat-1", "call-2", "settled:reviewRejected").await;
    assert!(!fixture.project_dir.path().join("nope.txt").exists());
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");
    common::wait_for_requests(&provider, 5).await;

    // The reviewer's two round-trips are booked as `auto-review` records,
    // the run's three as `turn` records — all in one settlement batch.
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    let reviews = kind(&records, "auto-review");
    assert_eq!(reviews.len(), 2, "{records:?}");
    assert_eq!(kind(&records, "turn").len(), 3, "{records:?}");
    for review in &reviews {
        assert_eq!(review["provider"], "openai");
        assert_eq!(review["input"], 700);
        assert_eq!(review["cost"]["input"], 0.01);
        // A review belongs to the Turn's batch but carries no Turn stamp:
        // the kind is its attribution, the message id would be a lie (a
        // review is not the Turn's own round-trip).
        assert!(review.get("messageId").is_none());
        assert!(review.get("turnOutcome").is_none());
    }
    for turn in kind(&records, "turn") {
        assert_eq!(turn["messageId"], "m-1");
        assert_eq!(turn["turnOutcome"], "succeeded");
    }
}

/// A review pass the provider fails still books its round-trip — the pass
/// happened, the chat paid for it — while the gate keeps failing closed.
#[tokio::test]
async fn a_failed_review_still_books_its_round_trip() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "bash", json!({ "command": "echo hi" })),
        ScriptedReply::Failed("the reviewer exploded".into()),
        ScriptedReply::text("carrying on"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    set_permission_mode(&engine, "chat-1", "auto-review").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "go").await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewRejected").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    let reviews = kind(&records, "auto-review");
    assert_eq!(reviews.len(), 1, "{records:?}");
    assert_eq!(reviews[0]["input"], 700);
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// An automatic Compaction before a Turn books its summary round-trip into
/// that Turn's batch as `compaction`, and leaves the compaction behavior
/// itself untouched.
#[tokio::test]
async fn an_automatic_compaction_is_metered() {
    let fixture = Fixture::new();
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
    // Past the threshold now: the second Turn opens with a summary request.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::wait_for_transcript_text(&mut transcript, "checkpoint summary text").await;

    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    let compactions = kind(&records, "compaction");
    assert_eq!(compactions.len(), 1, "{records:?}");
    // The summary reply reports the provider's pinned usage for that reply
    // (the fixture pins one usage for every reply), and carries no Turn
    // stamp of its own.
    assert_eq!(compactions[0]["provider"], "openai");
    assert!(compactions[0].get("messageId").is_none());
    assert!(compactions[0].get("turnOutcome").is_none());
    // Both Turns still booked their own rounds.
    assert!(kind(&records, "turn").len() >= 2, "{records:?}");
}

/// A manual Compaction runs outside the Turn model: its summary round-trip
/// is booked IMMEDIATELY, and a FAILED compaction still books — the model
/// work happened even though no summary was produced.
#[tokio::test]
async fn a_failed_manual_compaction_still_books_its_record_immediately() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::Failed("the summarizer refused".into()),
        ScriptedReply::text("kept the full conversation"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a long conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "compact",
                    "messageId": "m-compact",
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": fixture.cwd(),
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
    // The existing failure behavior is unchanged: the queue settles with a
    // notice and the History keeps the full conversation.
    common::wait_for_transcript_text(&mut transcript, "Compaction failed").await;

    // Booked despite the failure, and immediately — the Compaction never
    // belonged to a Turn's batch.
    let records = wait_for_kind(fixture.data_dir.path(), "chat-1", "compaction").await;
    assert_eq!(kind(&records, "compaction").len(), 1, "{records:?}");
    assert_eq!(kind(&records, "compaction")[0]["input"], 700);
    assert!(kind(&records, "compaction")[0].get("turnOutcome").is_none());
}

/// A manual Compaction that succeeds books too — the success path shares
/// the meter with the failure path, and the compaction behavior itself is
/// unchanged (the divider lands, the History shrinks).
#[tokio::test]
async fn a_successful_manual_compaction_is_metered() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        ScriptedReply::text("the manual summary"),
        ScriptedReply::text("next turn reply"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a long conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "compact",
                    "messageId": "m-compact",
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": fixture.cwd(),
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
    // The Compaction behavior is unchanged: the manual divider lands.
    common::wait_for_transcript_text(&mut transcript, "compactionDivider").await;

    let records = wait_for_kind(fixture.data_dir.path(), "chat-1", "compaction").await;
    assert_eq!(kind(&records, "compaction").len(), 1, "{records:?}");
    assert_eq!(kind(&records, "compaction")[0]["input"], 700);
    assert_eq!(
        kind(&records, "compaction")[0]["model"],
        "gpt-5.4",
        "{records:?}"
    );
}

/// A Compaction whose summary response the provider ABORTED is still a
/// reported round-trip: the usage it carried is kept, even though the
/// compaction itself fails and the History stays intact.
#[tokio::test]
async fn an_aborted_manual_compaction_keeps_the_reported_usage() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(big_text(30_000)),
        // The summarizer's stream cut mid-flight — a response with usage.
        ScriptedReply::aborted("half a summ"),
        ScriptedReply::text("kept the full conversation"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "a long conversation").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "compact",
                    "messageId": "m-compact",
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": fixture.cwd(),
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
    common::wait_for_transcript_text(&mut transcript, "Compaction failed").await;

    // The provider reported usage on the aborted response, so the record is
    // kept — the model work happened.
    let records = wait_for_kind(fixture.data_dir.path(), "chat-1", "compaction").await;
    let compactions = kind(&records, "compaction");
    assert_eq!(compactions.len(), 1, "{records:?}");
    assert_eq!(compactions[0]["input"], 700);
    assert_eq!(compactions[0]["output"], 70);
    // The aborted compaction changed nothing: no divider entered the
    // transcript.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        !snapshot.to_string().contains("compactionDivider"),
        "{snapshot}"
    );
}

// ---------------------------------------------------------------------------
// Title task
// ---------------------------------------------------------------------------

/// The Title task's round-trip is booked as `title`, immediately at
/// completion, in the chat's own ledger.
#[tokio::test]
async fn the_title_task_is_metered() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("the reply")])
        .with_usage(pinned_usage())
        .with_title_script(
            &common::default_title_system_prompt(),
            vec![ScriptedReply::text("A Better Title")],
        );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(
            methods::SAVE_TITLE_SETTINGS,
            json!({ "modelId": "openai/gpt-5.4", "instruction": holt_proto::DEFAULT_TITLE_INSTRUCTION }),
        )
        .await
        .unwrap();
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "title me").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    let records = wait_for_kind(fixture.data_dir.path(), "chat-1", "title").await;
    let titles = kind(&records, "title");
    assert_eq!(titles.len(), 1, "{records:?}");
    assert_eq!(titles[0]["input"], 700);
    assert_eq!(titles[0]["model"], "gpt-5.4");
    // The title record belongs to no Turn, and the Turn's own record is
    // separately stamped.
    assert!(titles[0].get("messageId").is_none());
    assert!(titles[0].get("turnOutcome").is_none());
    assert_eq!(kind(&records, "turn").len(), 1, "{records:?}");
}

// ---------------------------------------------------------------------------
// Subagents
// ---------------------------------------------------------------------------

/// A Worker child under auto-review: its own mutating call is judged by a
/// child review pass. Both the child's run round-trip and its review
/// round-trip book as `subagent` records in the PARENT ledger, stamped with
/// the child doc id — the child never gets a ledger file, and its internal
/// review is not subdivided into an `auto-review` kind.
#[tokio::test]
async fn a_subagents_own_round_trips_book_as_subagent_records() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            json!({
                "subagent_type": "worker",
                "description": "Write the file",
                "prompt": "write child.txt"
            }),
        ),
        // The child's first round: one mutating call, judged by a review
        // pass through the child's metered transport.
        ScriptedReply::tool_call(
            "child-1",
            "write",
            json!({ "path": "child.txt", "content": "hi" }),
        ),
        ScriptedReply::text("APPROVE"),
        ScriptedReply::text("child done"),
        ScriptedReply::text("parent done"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    set_permission_mode(&engine, "chat-1", "auto-review").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "delegate").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    let subagents = kind(&records, "subagent");
    // The child caused three round-trips: its first call round, the review
    // pass, and its closing summary round.
    assert_eq!(subagents.len(), 3, "{records:?}");
    let doc_ids: Vec<&str> = subagents
        .iter()
        .map(|record| record["subagentDocId"].as_str().unwrap())
        .collect();
    assert!(
        doc_ids.iter().all(|id| id.starts_with("chat-1--sub--")),
        "{doc_ids:?}"
    );
    assert!(
        doc_ids.windows(2).all(|pair| pair[0] == pair[1]),
        "one child, one doc id: {doc_ids:?}"
    );
    // The child's review is `subagent`, never `auto-review`; the parent's
    // own rounds stay `turn`.
    assert!(kind(&records, "auto-review").is_empty(), "{records:?}");
    assert!(!kind(&records, "turn").is_empty(), "{records:?}");
    // No child ledger file anywhere.
    assert!(
        !fixture
            .data_dir
            .path()
            .join("subagents/chat-1/usage")
            .exists()
    );
    // The child really ran and its review really approved.
    assert!(fixture.project_dir.path().join("child.txt").exists());
}

/// A child whose History outgrows the window compacts mid-Turn through its
/// metered transport: the child's Compaction summary books as a `subagent`
/// record in the parent ledger, never a `compaction` one (the child's
/// internal split is not a kind — the doc id is).
///
/// The estimator anchors on the last assistant usage, so the CHILD's own
/// first reply — and only that one — reports the overflowing number: the
/// parent's rounds stay small and never compact.
#[tokio::test]
async fn a_childs_compaction_books_as_a_subagent_record() {
    let fixture = Fixture::new();
    let brief = big_text(30_000);
    let provider = ScriptedProvider::new(vec![
        // Parent round 1: the delegation call (small pinned usage).
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            json!({
                "subagent_type": "explorer",
                "description": "Inspect files",
                "prompt": brief
            }),
        ),
        // Child round 1: a tool call whose usage anchor puts the child's
        // context past the threshold, so the next round compacts.
        ScriptedReply::tool_call_with_usage(
            "child-1",
            "ls",
            json!({ "path": "." }),
            overflowing_usage(),
        ),
        // The child's summary request.
        ScriptedReply::text("child checkpoint summary"),
        // Child round 2: the closing summary.
        ScriptedReply::text("child done"),
        // Parent round 2.
        ScriptedReply::text("parent done"),
    ])
    .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "delegate").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    // The child really compacted: one request rode the upstream
    // summarization prompt (bare — no tools).
    let summaries: Vec<_> = provider
        .requests()
        .into_iter()
        .filter(|request| {
            request
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("context summarization assistant"))
        })
        .collect();
    assert_eq!(summaries.len(), 1, "the child's Compaction never ran");
    assert_eq!(summaries[0].tools, 0);

    // Its round-trip is a `subagent` record: the child's internal
    // Compaction is not a `compaction` kind in the parent's ledger.
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    assert!(
        kind(&records, "compaction").is_empty(),
        "a child's Compaction must book under the child's doc id, not a kind: {records:?}"
    );
    // The child caused three round-trips: its two agent rounds and the
    // summary in between.
    let subagents = kind(&records, "subagent");
    assert_eq!(subagents.len(), 3, "{records:?}");
    assert!(
        subagents.iter().all(|record| record["subagentDocId"]
            .as_str()
            .is_some_and(|id| id.starts_with("chat-1--sub--"))),
        "{subagents:?}"
    );
    // The parent's own two rounds stayed `turn`.
    assert_eq!(kind(&records, "turn").len(), 2, "{records:?}");
}

// ---------------------------------------------------------------------------
// The meters never change behavior
// ---------------------------------------------------------------------------

/// The three metered call sites keep their existing outcomes with the
/// meters mounted: a rejection still blocks, a failed Turn still fails,
/// and a provider failure on the main loop still surfaces. Belt-and-braces
/// against a meter that swallows or alters its call site's result.
#[tokio::test]
async fn metering_leaves_the_call_sites_verdicts_untouched() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed("provider exploded".into())])
        .with_usage(pinned_usage());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "go").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "failed", "the failure still surfaces");
    assert!(
        event["internalReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("provider exploded")),
        "{event}"
    );
    // The failed round-trip is still billed.
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    let turns = kind(&records, "turn");
    assert_eq!(turns.len(), 1, "{records:?}");
    assert_eq!(turns[0]["turnOutcome"], "failed");
    assert_eq!(turns[0]["input"], 700);
}
