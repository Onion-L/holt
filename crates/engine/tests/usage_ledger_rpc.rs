//! Handle-seam tests for the per-chat usage ledger (usage-ledger spec,
//! ticket 01): every scripted Turn's provider round-trips land in
//! `usage/<chatId>.jsonl` as ONE settlement batch stamped with the Turn's
//! outcome — buffered in memory until the queue completes — a damaged
//! ledger is quarantined without blocking the chat, deleting a chat
//! archives its whole segment into the device-level `usage/archive.jsonl`
//! before removing the per-chat files, and no ledger write failure ever
//! touches the Turn result, the queue, or the terminal event.

mod common;

use std::time::Duration;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::StreamExt as _;
use holt_rpc::{RpcReply, RpcService as _, methods};
use pi_core::ai::types::{JsF64, Usage, UsageCost};
use serde_json::{Value, json};

/// How long "no event arrives" waits before passing.
const NO_EVENT: Duration = Duration::from_millis(300);

/// The usage pinned on the Turn's first (tool-call) round — distinct in
/// every field so a leaked default would stand out.
fn first_round_usage() -> Usage {
    Usage {
        input: 210,
        output: 21,
        cache_read: 3,
        cache_write: 4,
        cache_write_1h: Some(5),
        reasoning: Some(6),
        total_tokens: 238,
        cost: UsageCost {
            input: JsF64(0.001),
            output: JsF64(0.002),
            cache_read: JsF64(0.0001),
            cache_write: JsF64(0.0002),
            total: JsF64(0.0033),
        },
    }
}

/// A quick `ls` round the loop executes without a permission gate.
fn ls_round() -> ScriptedReply {
    ScriptedReply::tool_call_with_usage("c-1", "ls", json!({ "path": "." }), first_round_usage())
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

async fn subscribe_events(
    engine: &holt_engine::LocalEngine,
) -> futures::stream::BoxStream<'static, Value> {
    let RpcReply::Stream(events) = engine
        .handle(methods::WATCH_TURN_TERMINAL_EVENTS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchTurnTerminalEvents did not return a stream");
    };
    events
}

/// The ledger's raw JSON lines (header skipped) — what the JSONL contract
/// actually holds on disk.
fn read_ledger(data_dir: &std::path::Path, chat_id: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(data_dir.join("usage").join(format!("{chat_id}.jsonl")))
        .unwrap_or_else(|error| panic!("usage ledger for {chat_id}: {error}"));
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["version"], json!(1));
    lines
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn a_completed_turn_settles_its_round_trips_as_one_batch_after_the_queue() {
    let fixture = Fixture::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let second_round = Usage {
        input: 50,
        output: 5,
        total_tokens: 55,
        ..Default::default()
    };
    let provider =
        ScriptedProvider::new(vec![ls_round(), ScriptedReply::gated(gate.clone(), "done")])
            .with_usage(second_round.clone());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "list it").await;
    // The Turn's first round already completed (the tool ran, the second
    // request is in flight) — and nothing is on disk yet: the batch is
    // buffered until settlement, so a crash here costs only the record.
    common::wait_for_requests(&provider, 2).await;
    assert!(
        !fixture.data_dir.path().join("usage/chat-1.jsonl").exists(),
        "round-trips must buffer until the Turn settles"
    );

    gate.notify_one();
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    // Settlement wrote the whole Turn as one contiguous segment: both
    // rounds, stamped with the Turn's identity and outcome, tokens and
    // upstream cost verbatim.
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    assert_eq!(records.len(), 2, "{records:?}");
    for record in &records {
        assert_eq!(record["kind"], "turn");
        assert_eq!(record["messageId"], "m-1");
        assert_eq!(record["turnOutcome"], "succeeded");
        assert_eq!(record["provider"], "openai");
        // The provider-reported model id, not holt's provider-qualified one.
        assert_eq!(record["model"], "gpt-5.4");
    }
    assert_eq!(records[0]["input"], 210);
    assert_eq!(records[0]["output"], 21);
    assert_eq!(records[0]["cacheRead"], 3);
    assert_eq!(records[0]["cacheWrite"], 4);
    assert_eq!(records[0]["cacheWrite1h"], 5);
    assert_eq!(records[0]["reasoning"], 6);
    assert_eq!(records[0]["cost"]["input"], 0.001);
    assert_eq!(records[1]["input"], 50);
    assert_eq!(records[1]["output"], 5);
    assert!(records[0]["timestamp"].as_i64().unwrap() > 0);
    assert_no_event(&mut events).await;
}

/// Assert the watch stays silent.
async fn assert_no_event<S>(events: &mut S)
where
    S: futures::Stream<Item = Value> + Unpin,
{
    assert!(
        tokio::time::timeout(NO_EVENT, events.next()).await.is_err(),
        "unexpected Turn terminal event"
    );
}

#[tokio::test]
async fn a_failed_turn_stamps_failed_on_its_records() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ls_round(),
        ScriptedReply::Failed("provider exploded".into()),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "list it").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "failed");

    // Every round-trip the provider managed to report — including the one
    // the error rode on — settles under the failure outcome.
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    assert!(!records.is_empty(), "{records:?}");
    for record in &records {
        assert_eq!(record["kind"], "turn");
        assert_eq!(record["messageId"], "m-1");
        assert_eq!(record["turnOutcome"], "failed");
    }
    assert!(records.iter().any(|record| record["input"] == 210));
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn an_interrupted_turn_keeps_the_provider_reported_part() {
    let fixture = Fixture::new();
    let observed = std::sync::Arc::new(tokio::sync::Notify::new());
    let finish = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ls_round(),
        ScriptedReply::Cancelling {
            observed: observed.clone(),
            finish: finish.clone(),
        },
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "list it").await;
    common::wait_for_requests(&provider, 2).await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    finish.notify_one();

    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "interrupted");

    // Everything the provider reported stays billed under the interruption
    // outcome: the completed tool round, and the aborted round's own report
    // (the transport's abort artifact carried usage).
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    assert_eq!(records.len(), 2, "{records:?}");
    for record in &records {
        assert_eq!(record["messageId"], "m-1");
        assert_eq!(record["turnOutcome"], "interrupted");
    }
    assert_eq!(records[0]["input"], 210);
    assert_eq!(records[0]["output"], 21);
    assert_eq!(records[1]["input"], 111);
    assert_no_event(&mut events).await;
}

#[tokio::test]
async fn a_subagent_books_into_the_parent_ledger_and_writes_no_child_file() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            json!({"subagent_type": "explorer", "description": "Inspect files", "prompt": "brief"}),
        ),
        ScriptedReply::text("child summary"),
        ScriptedReply::text("parent done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "delegate").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");
    common::wait_for_requests(&provider, 3).await;

    // The parent's own rounds are `turn` records; the child's round-trip is
    // booked from the delegation's billing vector as ONE `subagent` record
    // in the parent ledger, stamped with the child doc id. No child ledger
    // file exists anywhere — the parent's file is the only one.
    let ledger_path = fixture.data_dir.path().join("usage/chat-1.jsonl");
    let records: Vec<Value> = {
        let text = std::fs::read_to_string(&ledger_path).unwrap();
        text.lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    assert_eq!(records.len(), 3, "{records:?}");
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.iter().filter(|kind| **kind == "turn").count(), 2);
    assert_eq!(kinds.iter().filter(|kind| **kind == "subagent").count(), 1);
    let subagent = records
        .iter()
        .find(|record| record["kind"] == "subagent")
        .unwrap();
    let doc_id = subagent["subagentDocId"].as_str().unwrap();
    assert!(doc_id.starts_with("chat-1--sub--"), "{doc_id}");
    // The spawn chip's aggregate is unchanged: fixed usage per round-trip.
    assert_eq!(subagent["input"], 111);
    // Subagent records carry no Turn stamp — the child doc id is their
    // attribution.
    assert!(subagent.get("messageId").is_none());
    assert!(subagent.get("turnOutcome").is_none());
    // No child usage file, and the spawn tool result did not double-book the
    // child's total into a parent turn record.
    assert!(
        !fixture
            .data_dir
            .path()
            .join("subagents/chat-1/usage")
            .exists()
    );
    let turns: Vec<&Value> = records
        .iter()
        .filter(|record| record["kind"] == "turn")
        .collect();
    assert!(
        turns.iter().all(|record| record["input"] == 111),
        "the delegation result's aggregate must not double-book: {turns:?}"
    );
}

#[tokio::test]
async fn a_damaged_ledger_is_quarantined_and_the_chat_still_works() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.data_dir.path().join("usage")).unwrap();
    std::fs::write(
        fixture.data_dir.path().join("usage/chat-1.jsonl"),
        "garbage, not a header\n",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "hello").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    // The damaged file was set aside (kept, never overwritten) and the chat
    // worked as though it had never been billed: a fresh ledger carries the
    // new Turn.
    let usage_dir = fixture.data_dir.path().join("usage");
    let aside: Vec<String> = std::fs::read_dir(&usage_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("chat-1.jsonl") && name.ends_with(".corrupt"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    let records = read_ledger(fixture.data_dir.path(), "chat-1");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["turnOutcome"], "succeeded");
}

#[tokio::test]
async fn deleting_a_chat_archives_its_whole_segment_then_removes_the_files() {
    let fixture = Fixture::new();
    let first = Usage {
        input: 210,
        output: 21,
        cache_read: 3,
        cache_write: 4,
        total_tokens: 238,
        ..Default::default()
    };
    let provider = ScriptedProvider::new(vec![])
        .with_chat_script(
            "chat one prompt",
            vec![ScriptedReply::text_with_usage("a", first)],
        )
        .with_chat_script("chat two prompt", vec![ScriptedReply::text("b")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::setup_chat(&engine, "chat-2").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "chat one prompt").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");
    queue_run(&engine, "chat-2", &fixture.cwd(), "m-2", "chat two prompt").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");

    let original = read_ledger(fixture.data_dir.path(), "chat-1");
    assert_eq!(original.len(), 1);

    engine
        .handle(
            methods::MUTATE,
            json!({"op": "deleteChat", "chatId": "chat-1"}),
        )
        .await
        .unwrap();

    // The whole segment moved into the device stream — chat-attributed, and
    // lossless down to kind, token fields, and timestamps.
    let archive_text =
        std::fs::read_to_string(fixture.data_dir.path().join("usage/archive.jsonl")).unwrap();
    let mut lines = archive_text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["version"], json!(1));
    let archived: Vec<Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(archived.len(), 1, "{archived:?}");
    assert_eq!(archived[0]["chatId"], "chat-1");
    for field in [
        "kind",
        "provider",
        "model",
        "messageId",
        "turnOutcome",
        "input",
        "output",
        "cacheRead",
        "cacheWrite",
        "timestamp",
    ] {
        assert_eq!(archived[0][field], original[0][field], "{field}");
    }

    // The chat's own files are gone (`jsonl*` covers quarantined copies);
    // the other chat keeps its ledger.
    let usage_dir = fixture.data_dir.path().join("usage");
    let remaining: Vec<String> = std::fs::read_dir(&usage_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !remaining
            .iter()
            .any(|name| name.starts_with("chat-1.jsonl")),
        "{remaining:?}"
    );
    assert!(remaining.contains(&"chat-2.jsonl".to_string()));
    assert!(remaining.contains(&"archive.jsonl".to_string()));
}

#[tokio::test]
async fn an_archive_write_failure_still_deletes_the_chat() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "hello").await;
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");
    assert!(fixture.data_dir.path().join("usage/chat-1.jsonl").exists());

    // Break the archive: a directory where the file would be appended.
    std::fs::create_dir_all(fixture.data_dir.path().join("usage/archive.jsonl")).unwrap();
    engine
        .handle(
            methods::MUTATE,
            json!({"op": "deleteChat", "chatId": "chat-1"}),
        )
        .await
        .unwrap();

    // The delete completed anyway — bookkeeping never blocks it.
    assert!(!fixture.data_dir.path().join("usage/chat-1.jsonl").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_ledger_write_failure_never_fails_the_turn_queue_or_terminal_event() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::gated(gate.clone(), "answer")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let mut events = subscribe_events(&engine).await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    queue_run(&engine, "chat-1", &fixture.cwd(), "m-1", "hello").await;
    // Admission committed and the request is in flight; now make the usage
    // directory unwritable so the settlement append cannot land.
    common::wait_for_requests(&provider, 1).await;
    let usage = fixture.data_dir.path().join("usage");
    std::fs::create_dir_all(&usage).unwrap();
    std::fs::set_permissions(&usage, std::fs::Permissions::from_mode(0o500)).unwrap();
    gate.notify_one();

    // The Turn still completes, the session still settles idle, and the
    // terminal event still publishes — the failed append costs only the
    // record.
    let event = common::next_frame(&mut events).await;
    assert_eq!(event["outcome"], "succeeded");
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(!fixture.data_dir.path().join("usage/chat-1.jsonl").exists());
    assert_no_event(&mut events).await;
    std::fs::set_permissions(&usage, std::fs::Permissions::from_mode(0o700)).unwrap();
}
