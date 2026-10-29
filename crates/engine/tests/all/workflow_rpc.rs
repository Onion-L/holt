use std::time::Duration;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_core::ai::types::Message;
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::{Value, json};

use crate::common;

/// The one-step plan every simple script here names as `s`.
fn single() -> Value {
    json!({"steps": [{"id": "s", "title": "Run"}]})
}

fn launch(id: &str, name: &str, plan: Value, script: &str, args: Value) -> ScriptedReply {
    ScriptedReply::tool_call(
        id,
        "Workflow",
        json!({"name": name, "plan": plan, "script": script, "args": args}),
    )
}

fn parts(snapshot: &Value) -> Vec<Value> {
    snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().cloned().unwrap_or_default())
        .collect()
}

fn part(snapshot: &Value, id: &str) -> Option<Value> {
    parts(snapshot).into_iter().find(|p| p["id"] == id)
}

/// Poll the transcript until `check` holds (the run lives off the Turn, so
/// no session status marks its progress).
async fn wait_for(engine: &holt_engine::LocalEngine, check: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = common::transcript_snapshot(engine, "chat-1").await;
            if check(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("transcript reached the expected state")
}

fn last_user_text(request: &common::RecordedRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(user) => Some(user.content.text()),
            _ => None,
        })
        .unwrap_or_default()
}

fn workflow_dirs(fixture: &Fixture) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(fixture.data_dir.path().join("subagents/chat-1/workflows"))
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

const AUDIT: &str = "\
const found = await parallel(args.files.map(f => () => agent('Audit ' + f, { step: 'find', label: f })));
const verdict = await agent('Verify ' + found.join(','), {
  step: 'verify',
  schema: { type: 'object', properties: { verdict: { type: 'string' } }, required: ['verdict'] },
});
return { found, verdict };";

#[tokio::test]
async fn a_workflow_runs_in_the_background_and_reports_back() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        launch(
            "wf-1",
            "audit",
            json!({"steps": [
                {"id": "find", "title": "Find", "agents": 2},
                {"id": "verify", "title": "Verify", "after": ["find"]},
            ]}),
            AUDIT,
            json!({"files": ["a", "b"]}),
        ),
        ScriptedReply::text("Started; I'll wait for the report."),
        ScriptedReply::text("The audit came back clean."),
    ])
    .with_chat_script("Audit a", vec![ScriptedReply::text("A ok")])
    .with_chat_script("Audit b", vec![ScriptedReply::text("B ok")])
    .with_chat_script(
        "Verify A ok,B ok",
        vec![ScriptedReply::tool_call(
            "submit",
            "submit_result",
            json!({"verdict": "fine"}),
        )],
    );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Audit the repo").await;

    // Parent (launch + reply), three children, then the report Turn.
    common::wait_for_requests(&provider, 6).await;
    let snapshot = wait_for(&engine, |s| {
        parts(s).iter().any(|p| p["kind"] == "workflowResult")
            && s["reset"].as_array().unwrap().last().unwrap()["status"] == "complete"
    })
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    // The launch returned at once: the parent's second round saw a
    // "started" result, not the workflow's value.
    let launched = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "wf-1" => Some(r.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(format!("{launched:?}").contains("started in the background"));
    let report = last_user_text(requests.last().unwrap());
    assert!(report.starts_with("Workflow \"audit\" (run "), "{report}");
    let value: Value = serde_json::from_str(report.split_once("\n\n").unwrap().1)
        .expect("the report body is JSON");
    assert_eq!(
        value,
        json!({"found": ["A ok", "B ok"], "verdict": {"verdict": "fine"}})
    );

    // The call's chip carries the run and its plan; each agent is a display
    // chip under it, tagged with its step.
    let container = part(&snapshot, "wf-1").unwrap();
    assert_eq!(container["call"]["name"], "Workflow: audit");
    assert_eq!(
        container["call"]["input"]["plan"]["steps"][1]["after"],
        json!(["find"])
    );
    assert_eq!(container["subagentStatus"], "done");
    let ids = common::transcript_tool_ids(&snapshot);
    let at = ids.iter().position(|id| id == "wf-1").unwrap();
    assert_eq!(
        ids[at + 1..at + 4],
        ["wf-1#item-0", "wf-1#item-1", "wf-1#item-2"]
    );
    let first = part(&snapshot, "wf-1#item-0").unwrap();
    assert_eq!(first["call"]["name"], "Agent: a");
    assert_eq!(first["call"]["input"]["step"], "find");
    assert_eq!(first["subagentStatus"], "done");
    assert!(first["subagentRef"].as_str().is_some());
    let verify = part(&snapshot, "wf-1#item-2").unwrap();
    assert_eq!(verify["call"]["input"]["step"], "verify");
    let result = parts(&snapshot)
        .into_iter()
        .find(|p| p["kind"] == "workflowResult")
        .unwrap();
    assert_eq!(result["name"], "audit");
    assert_eq!(result["failed"], false);

    let dirs = workflow_dirs(&fixture);
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].join("script.js").exists());
    assert!(dirs[0].join("plan.json").exists());
    assert_eq!(
        serde_json::from_str::<Value>(
            &std::fs::read_to_string(dirs[0].join("result.json")).unwrap()
        )
        .unwrap(),
        value
    );
}

#[tokio::test]
async fn a_failed_run_reports_its_error_and_a_resume_reuses_finished_agents() {
    let fixture = Fixture::new();
    let script = "const a = await agent('Audit a', { step: 's' }); \
                  const b = await agent('Audit b', { step: 's' }); \
                  if (b === null) throw new Error('audit b failed'); return [a, b];";
    let provider = ScriptedProvider::new(vec![
        launch("wf-1", "pair", single(), script, Value::Null),
        ScriptedReply::text("Waiting."),
        ScriptedReply::text("It failed; I'll resume."),
    ])
    .with_chat_script("Audit a", vec![ScriptedReply::text("A ok")])
    .with_chat_script(
        "Audit b",
        vec![
            ScriptedReply::Failed("Child provider failure".into()),
            ScriptedReply::text("B ok"),
        ],
    );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Audit both").await;
    common::wait_for_requests(&provider, 5).await;
    wait_for(&engine, |s| {
        parts(s)
            .iter()
            .any(|p| p["kind"] == "workflowResult" && p["failed"] == true)
    })
    .await;
    let requests = provider.requests();
    let report = last_user_text(requests.last().unwrap());
    assert!(report.contains("failed: "), "{report}");
    assert!(report.contains("audit b failed"), "{report}");
    let dirs = workflow_dirs(&fixture);
    let run_id = dirs[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(
        report.contains(&format!("resume: \"{run_id}\"")),
        "{report}"
    );
    wait_for(&engine, |s| {
        s["reset"].as_array().unwrap().last().unwrap()["status"] == "complete"
    })
    .await;

    // Relaunch the saved plan and script: "Audit a" answers from the journal.
    provider.push(ScriptedReply::tool_call(
        "wf-2",
        "Workflow",
        json!({"name": "pair", "resume": run_id}),
    ));
    provider.push(ScriptedReply::text("Resumed."));
    provider.push(ScriptedReply::text("Both audits are in."));
    let before = provider.requests().len();
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Resume it").await;
    common::wait_for_requests(&provider, before + 4).await;
    let snapshot = wait_for(&engine, |s| {
        part(s, "wf-2").is_some_and(|p| p["subagentStatus"] == "done")
            && parts(s)
                .iter()
                .filter(|p| p["kind"] == "workflowResult")
                .count()
                == 2
    })
    .await;
    let requests = provider.requests();
    let audits_of_a = requests
        .iter()
        .filter(|r| last_user_text(r) == "Audit a")
        .count();
    assert_eq!(audits_of_a, 1, "the finished agent was not run again");
    let replayed = part(&snapshot, "wf-2#item-0").unwrap();
    assert_eq!(replayed["subagentStatus"], "done");
    assert!(replayed["subagentRef"].as_str().is_some());
    wait_for_requests_exact(&provider, before + 4).await;
    let report = last_user_text(provider.requests().last().unwrap());
    assert!(report.contains("[\n  \"A ok\",\n  \"B ok\"\n]"), "{report}");
}

/// The count settles at `count` and stays there briefly.
async fn wait_for_requests_exact(provider: &ScriptedProvider, count: usize) {
    common::wait_for_requests(provider, count).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(provider.requests().len(), count);
}

#[tokio::test]
async fn stopping_a_run_cancels_its_agents_without_a_report() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        launch(
            "wf-1",
            "slow",
            single(),
            "return await agent('Take forever', { step: 's' });",
            Value::Null,
        ),
        ScriptedReply::text("Waiting."),
    ])
    .with_chat_script("Take forever", vec![ScriptedReply::Silent]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Go slow").await;
    common::wait_for_requests(&provider, 3).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let RpcReply::Value(reply) = engine
        .handle(
            methods::STOP_WORKFLOW,
            json!({"chatId": "chat-1", "callId": "wf-1"}),
        )
        .await
        .unwrap()
    else {
        panic!("value reply")
    };
    assert_eq!(reply["stopped"], true);
    let snapshot = wait_for(&engine, |s| {
        part(s, "wf-1").is_some_and(|p| p["subagentStatus"] == "failed")
    })
    .await;
    assert_eq!(
        part(&snapshot, "wf-1#item-0").unwrap()["subagentStatus"],
        "failed"
    );
    wait_for_requests_exact(&provider, 3).await;
    assert!(
        !parts(&common::transcript_snapshot(&engine, "chat-1").await)
            .iter()
            .any(|p| p["kind"] == "workflowResult")
    );
    // A second stop finds nothing live.
    let RpcReply::Value(reply) = engine
        .handle(
            methods::STOP_WORKFLOW,
            json!({"chatId": "chat-1", "callId": "wf-1"}),
        )
        .await
        .unwrap()
    else {
        panic!("value reply")
    };
    assert_eq!(reply["stopped"], false);
}

#[tokio::test]
async fn a_restart_settles_a_live_run_as_failed() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        launch(
            "wf-1",
            "slow",
            single(),
            "return await agent('Take forever', { step: 's' });",
            Value::Null,
        ),
        ScriptedReply::text("Waiting."),
    ])
    .with_chat_script("Take forever", vec![ScriptedReply::Silent]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Go slow").await;
    common::wait_for_requests(&provider, 3).await;
    wait_for(&engine, |s| {
        part(s, "wf-1#item-0").is_some_and(|p| p["subagentStatus"] == "running")
    })
    .await;
    drop(engine);

    let fresh = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&fresh);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(part(&snapshot, "wf-1").unwrap()["subagentStatus"], "failed");
    assert!(
        fresh.requests().is_empty(),
        "a run is never resumed on its own"
    );
}

#[tokio::test]
async fn a_script_that_does_not_parse_fails_the_call() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        launch("wf-1", "broken", single(), "const = ;", Value::Null),
        ScriptedReply::text("Fixing it."),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Run it").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let result = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "wf-1" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(result.is_error);
    assert!(format!("{:?}", result.content).contains("does not parse"));
    assert!(workflow_dirs(&fixture).is_empty());
}

/// The launch's own error result, for a call refused before any run starts.
async fn refused_launch(arguments: Value) -> String {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("wf-1", "Workflow", arguments),
        ScriptedReply::text("Fixing it."),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Run it").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let result = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "wf-1" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(result.is_error);
    assert!(workflow_dirs(&fixture).is_empty());
    format!("{:?}", result.content)
}

#[tokio::test]
async fn a_missing_or_malformed_plan_fails_the_call() {
    let script = "return await agent('x', { step: 's' });";
    let missing = refused_launch(json!({"name": "n", "script": script})).await;
    assert!(missing.contains("plan is required"), "{missing}");
    let cyclic = refused_launch(json!({"name": "n", "script": script, "plan": {"steps": [
        {"id": "a", "title": "A", "after": ["b"]},
        {"id": "b", "title": "B", "after": ["a"]},
    ]}}))
    .await;
    assert!(cyclic.contains("cycle"), "{cyclic}");
}

#[tokio::test]
async fn a_script_that_leaves_its_plan_fails_the_run() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        launch(
            "wf-1",
            "drift",
            single(),
            "await agent('Audit a', { step: 's' }); return await agent('Extra', { step: 'extra' });",
            Value::Null,
        ),
        ScriptedReply::text("Waiting."),
        ScriptedReply::text("It drifted."),
    ])
    .with_chat_script("Audit a", vec![ScriptedReply::text("A ok")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Run it").await;
    common::wait_for_requests(&provider, 4).await;
    let snapshot = wait_for(&engine, |s| {
        parts(s)
            .iter()
            .any(|p| p["kind"] == "workflowResult" && p["failed"] == true)
    })
    .await;
    let report = last_user_text(provider.requests().last().unwrap());
    assert!(report.contains("\"extra\" is not in the plan"), "{report}");
    assert!(
        part(&snapshot, "wf-1#item-1").is_none(),
        "the refused agent never got a chip"
    );
}
