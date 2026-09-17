//! Jev review at the RPC seam (ADR-0026): in jev-review every mutating
//! call is judged by one decision request through the injected stub
//! (standing in for the TypeSafe HTTP client the settings record mounts).
//! A pass executes; a veto blocks with the judge's reason; an unsure or
//! failed judgment escalates to an ordinary Approval; an unconfigured
//! judge gates as confirm-changes, silently, keeping the mode. Judged
//! calls book `jev-review` usage records whatever the verdict.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_engine::{JevCall, JevJudge, JevJudgment, JevVerdict};
use holt_rpc::{RpcService, methods};
use serde_json::{Value, json};

mod common;

/// The canned judge: one fixed outcome per call, recording what it saw.
struct StubJudge {
    outcome: Result<JevVerdict, String>,
    calls: AtomicUsize,
    seen: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl StubJudge {
    fn verdict(verdict: JevVerdict) -> Arc<Self> {
        Arc::new(Self {
            outcome: Ok(verdict),
            calls: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            outcome: Err("the Jev judge request failed: connection reset".into()),
            calls: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl JevJudge for StubJudge {
    fn judge<'a>(
        &'a self,
        call: JevCall<'a>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<JevJudgment, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push((
                call.tool.to_string(),
                call.cwd.to_string(),
                call.request.to_string(),
            ));
            self.outcome.clone().map(|verdict| JevJudgment {
                verdict,
                input_tokens: 330,
                output_tokens: 34,
            })
        })
    }
}

fn judge_resolver(judge: Arc<StubJudge>) -> holt_engine::JevJudgeResolver {
    Arc::new(move |_key| Some(judge.clone() as Arc<dyn JevJudge>))
}

async fn setup_jev_chat(engine: &holt_engine::LocalEngine, chat_id: &str) {
    common::setup_chat(engine, chat_id).await;
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "setChatPermissionMode",
                "chatId": chat_id,
                "mode": "jev-review",
            }),
        )
        .await
        .unwrap();
}

async fn save_key(engine: &holt_engine::LocalEngine) {
    engine
        .handle(
            methods::SAVE_JEV_SETTINGS,
            json!({ "apiKey": "sk-jev-test-key-1234" }),
        )
        .await
        .unwrap();
}

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

/// A passing judgment executes the call with no model round-trip for the
/// gate (unlike auto-review) and books the decision request's tokens.
#[tokio::test]
async fn a_passing_judgment_executes_the_call() {
    let fixture = Fixture::new();
    let judge = StubJudge::verdict(JevVerdict::Allow);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            json!({ "command": "echo judged > out.txt" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge.clone()));
    setup_jev_chat(&engine, "chat-1").await;
    save_key(&engine).await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "clean the build").await;

    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewPassed").await;
    common::wait_for_requests(&provider, 2).await;
    assert!(fixture.project_dir.path().join("out.txt").exists());
    // One decision request per judged call, weighed against the user's
    // latest message — never a bare model completion like auto-review.
    assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
    let seen = judge.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.0, "bash");
    assert_eq!(seen.1, fixture.cwd());
    assert_eq!(seen.2, "clean the build");

    // The judged call books a jev-review usage record with the TypeSafe
    // identity, whatever the verdict. The ledger settles with the Turn,
    // so poll for it.
    common::wait_for_requests(&provider, 2).await;
    let records = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let records = read_ledger(fixture.data_dir.path(), "chat-1");
            if records
                .iter()
                .any(|record| record["kind"] == json!("jev-review"))
            {
                break records;
            }
            assert!(deadline.elapsed().is_zero(), "no jev-review record settled");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    let jev: Vec<_> = records
        .iter()
        .filter(|record| record["kind"] == json!("jev-review"))
        .collect();
    assert_eq!(jev.len(), 1, "{records:?}");
    assert_eq!(jev[0]["provider"], json!("typesafe"));
    assert_eq!(jev[0]["model"], json!("jev-latest"));
    assert_eq!(jev[0]["input"], json!(330));
    assert_eq!(jev[0]["output"], json!(34));
}

/// A clear veto blocks the call with the prefixed reason; the Turn
/// continues on the error tool result.
#[tokio::test]
async fn a_veto_blocks_with_the_jev_reason() {
    let fixture = Fixture::new();
    let judge = StubJudge::verdict(JevVerdict::Deny {
        reason: "the call looks destructive or irreversible".into(),
    });
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "bash", json!({ "command": "rm -rf /" })),
        ScriptedReply::text("understood"),
    ]);
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge));
    setup_jev_chat(&engine, "chat-1").await;
    save_key(&engine).await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "clean up").await;

    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewRejected").await;
    common::wait_for_requests(&provider, 2).await;
    let requests = provider.requests();
    assert!(
        common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row
                == "toolresult:call-1:Jev review: the call looks destructive or irreversible"),
        "the prefixed reason never reached the model: {:?}",
        common::summarize(&requests[1].messages)
    );
    // The chip carries the prefixed reason for the transcript record.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(snapshot.to_string().contains("Jev review: the call looks"));
}

/// An unsure judgment escalates to an ordinary Approval whose note says
/// so; the user's allow settles it and executes the call.
#[tokio::test]
async fn an_unsure_judgment_escalates_to_the_user() {
    let fixture = Fixture::new();
    let judge = StubJudge::verdict(JevVerdict::Unsure);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "write",
            json!({ "path": "maybe.txt", "content": "x" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge));
    setup_jev_chat(&engine, "chat-1").await;
    save_key(&engine).await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "draft it").await;

    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    // The escalation note rides the pending gate.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        snapshot.to_string().contains("Jev review was unsure"),
        "{snapshot}"
    );
    common::resolve_approval(&engine, &approval_id, json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:allowed").await;
    wait_for_file(&fixture.project_dir.path().join("maybe.txt")).await;
}

/// A failed judgment (transport error, invalid key, exhausted retries)
/// escalates the same way — the gate never fails open and never masks the
/// failure as a model rejection.
#[tokio::test]
async fn a_failed_judgment_escalates_to_the_user() {
    let fixture = Fixture::new();
    let judge = StubJudge::failing();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "bash", json!({ "command": "echo hi > f.txt" })),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge));
    setup_jev_chat(&engine, "chat-1").await;
    save_key(&engine).await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        snapshot.to_string().contains("could not reach a verdict"),
        "{snapshot}"
    );
    common::resolve_approval(
        &engine,
        &approval_id,
        json!({ "kind": "deny", "note": "not now" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:denied").await;
    assert!(!fixture.project_dir.path().join("f.txt").exists());
}

/// Without a key the judge never mounts: the Turn gates as
/// confirm-changes, silently, the chat keeps its jev-review mode, and a
/// saved key revives the judge from the next Turn.
#[tokio::test]
async fn an_unconfigured_judge_gates_as_confirm_changes_and_revives_with_a_key() {
    let fixture = Fixture::new();
    let judge = StubJudge::verdict(JevVerdict::Unsure);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            json!({ "command": "echo fallback > f.txt" }),
        ),
        ScriptedReply::text("done"),
        // The revived Turn: the judge (unsure) escalates again.
        ScriptedReply::tool_call(
            "call-2",
            "bash",
            json!({ "command": "echo revived > g.txt" }),
        ),
        ScriptedReply::text("done"),
    ]);
    // The resolver is mounted but the record is not: admission resolves
    // no judge, which is the unconfigured state.
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge.clone()));
    setup_jev_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    // Every mutating call asks the user — the confirm-changes behavior —
    // with no escalation note (this is the ordinary approval, not a Jev
    // escalation) and no judge round-trip.
    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        !snapshot.to_string().contains("Jev review"),
        "no Jev note on the ordinary approval: {snapshot}"
    );
    assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    common::resolve_approval(&engine, &approval_id, json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:allowed").await;
    wait_for_file(&fixture.project_dir.path().join("f.txt")).await;

    // The mode choice survives the keyless Turn.
    let mode = watched_mode(&engine, "chat-1").await;
    assert_eq!(mode, "jev-review");

    // A key revives the judge from the next Turn's admission.
    save_key(&engine).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "again").await;
    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-2", "pending").await;
    assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        snapshot.to_string().contains("Jev review was unsure"),
        "{snapshot}"
    );
    common::resolve_approval(&engine, &approval_id, json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-2", "settled:allowed").await;
    wait_for_file(&fixture.project_dir.path().join("g.txt")).await;
}

async fn wait_for_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "file never landed: {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

async fn watched_mode(engine: &holt_engine::LocalEngine, chat_id: &str) -> String {
    let holt_rpc::RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream")
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let frame = common::next_frame(&mut chats).await;
        let mode = frame
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == chat_id)
            .unwrap_or_else(|| panic!("chat {chat_id} missing from watch frame"))
            ["config"]["permissionMode"]
            .as_str()
            .unwrap()
            .to_string();
        if mode == "jev-review" || std::time::Instant::now() > deadline {
            return mode;
        }
    }
}

/// A session grant skips the judge entirely — grants hold across modes.
#[tokio::test]
async fn grants_skip_the_judge() {
    let fixture = Fixture::new();
    // Unsure: the first call escalates, and the user's always-allow both
    // settles it and records the session grant for the command prefix.
    let judge = StubJudge::verdict(JevVerdict::Unsure);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            json!({ "command": "echo granted > a.txt" }),
        ),
        ScriptedReply::tool_call(
            "call-2",
            "bash",
            json!({ "command": "echo granted > a.txt" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge.clone()));
    setup_jev_chat(&engine, "chat-1").await;
    save_key(&engine).await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;

    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    common::resolve_approval(&engine, &approval_id, json!({ "kind": "alwaysAllow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:alwaysAllowed").await;
    // The second, prefix-matching call is exempted without a judge round.
    common::wait_for_gate(&engine, "chat-1", "call-2", "settled:exempted").await;
    assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
}

/// A worker subagent's mutating call is judged under the parent Turn's
/// mode, and its judged tokens book into the parent chat's ledger under
/// the delegation's `subagent` kind.
#[tokio::test]
async fn a_worker_subagents_edit_is_judged_under_the_parent_mode() {
    let fixture = Fixture::new();
    let judge = StubJudge::verdict(JevVerdict::Allow);
    let provider = ScriptedProvider::new(vec![
        // The parent delegates a task to a worker.
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            json!({
                "subagent_type": "worker",
                "description": "Write the file",
                "prompt": "Create g.txt"
            }),
        ),
        // The worker's own run: a mutating call, judged by the stub.
        ScriptedReply::tool_call(
            "child-call-1",
            "write",
            json!({ "path": "g.txt", "content": "made by the worker" }),
        ),
        ScriptedReply::text("child done"),
        // The parent's continuation after the delegation returns.
        ScriptedReply::text("parent done"),
    ]);
    let engine = fixture.engine_with_jev_judge(&provider, judge_resolver(judge.clone()));
    setup_jev_chat(&engine, "chat-1").await;
    save_key(&engine).await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "delegate it").await;

    // The child's chip rides the parent transcript namespaced by its doc
    // id — find the delegation's child id, then wait on the composed id.
    let child = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
            let text = snapshot.to_string();
            if let Some(id) = text
                .split("\"subagentRef\":\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(str::to_string)
            {
                break id;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "delegation never opened: {text}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let chip_id = format!("{child}:child-call-1");
    common::wait_for_gate(&engine, "chat-1", &chip_id, "settled:reviewPassed").await;
    wait_for_file(&fixture.project_dir.path().join("g.txt")).await;
    assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
    // The worker was judged against its Task brief, not the parent's
    // prompt.
    let seen = judge.seen.lock().unwrap();
    assert_eq!(seen[0].0, "write");
    assert!(seen[0].2.contains("Create g.txt"), "{:?}", seen[0]);

    // The judged tokens book into the parent's ledger under the
    // delegation's kind, with the child doc id as attribution.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let records = loop {
        let records = read_ledger(fixture.data_dir.path(), "chat-1");
        if records
            .iter()
            .any(|record| record["provider"] == json!("typesafe"))
        {
            break records;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no typesafe record settled: {records:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let jev: Vec<_> = records
        .iter()
        .filter(|record| record["provider"] == json!("typesafe"))
        .collect();
    assert_eq!(jev.len(), 1, "{records:?}");
    assert_eq!(jev[0]["kind"], json!("subagent"));
    assert!(jev[0]["subagentDocId"].is_string(), "{jev:?}");
    assert_eq!(jev[0]["input"], json!(330));
}
