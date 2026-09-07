use std::{sync::Arc, time::Duration};

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use pi_core::ai::types::Message;
use serde_json::{Value, json};

mod common;

fn spawn(id: &str, role: &str) -> ScriptedReply {
    ScriptedReply::tool_call(id, "Agent", brief(role))
}

fn brief(role: &str) -> Value {
    json!({"subagent_type": role, "description": "Inspect assigned files", "prompt": "Only the assigned task brief"})
}

fn parts(snapshot: &Value) -> Vec<&Value> {
    snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().unwrap())
        .collect()
}

async fn child_id(engine: &holt_engine::LocalEngine, tool: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = common::transcript_snapshot(engine, "chat-1").await;
            if let Some(id) = parts(&snapshot)
                .into_iter()
                .find(|p| p["id"] == tool)
                .and_then(|p| p["subagentRef"].as_str())
            {
                return id.to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child reference")
}

async fn frozen(engine: &holt_engine::LocalEngine, id: &str) -> Value {
    let RpcReply::Value(value) = engine
        .handle(
            methods::FETCH_TOOL_BLOB,
            json!({"blobRef": format!("chat-1/{id}")}),
        )
        .await
        .unwrap()
    else {
        panic!("blob value")
    };
    serde_json::from_str(value["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn explorer_is_independent_and_returns_a_summary_with_durable_records() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project_dir.path().join("AGENTS.md"),
        "Project instruction sentinel",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("Parent-only secret from earlier history"),
        spawn("spawn-1", "explorer"),
        ScriptedReply::text("Child findings"),
        ScriptedReply::text("Parent conclusion"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Remember parent secret").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    common::run_prompt(
        &engine,
        "chat-1",
        &fixture.cwd(),
        "Delegate an investigation",
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let child = &requests[2];
    assert_eq!(child.messages.len(), 1);
    assert!(
        !common::summarize(&child.messages)
            .join("\n")
            .contains("secret")
    );
    assert!(
        child
            .system_prompt
            .as_ref()
            .unwrap()
            .contains("Project instruction sentinel")
    );
    assert_eq!(child.tool_names, ["read", "grep"]);
    assert_eq!(child.model, requests[1].model);
    assert_eq!(child.reasoning, requests[1].reasoning);
    let result = requests[3]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "spawn-1" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(
        result.usage.as_ref().unwrap().total_tokens,
        common::fixed_usage().total_tokens
    );
    let id = child_id(&engine, "spawn-1").await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let chip = parts(&snapshot)
        .into_iter()
        .find(|p| p["id"] == "spawn-1")
        .unwrap();
    assert_eq!(chip["subagentStatus"], "done");
    assert_eq!(chip["resolved"], true);
    assert!(chip["output"].as_str().unwrap().contains("Child findings"));
    assert!(
        frozen(&engine, &id)
            .await
            .to_string()
            .contains("Child findings")
    );
    let child_dir = fixture.data_dir.path().join("subagents/chat-1");
    assert!(!child_dir.join("queues").exists());
    assert!(child_dir.join("results").join(format!("{id}.txt")).exists());
    drop(engine);
    let fresh = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&fresh);
    assert!(
        frozen(&engine, &id)
            .await
            .to_string()
            .contains("Child findings")
    );
    assert!(fresh.requests().is_empty());
}

#[tokio::test]
async fn explorer_cannot_write_or_delegate_again() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        spawn("spawn-1", "explorer"),
        ScriptedReply::tool_call(
            "write-1",
            "write",
            json!({"path":"forbidden.txt", "content":"no"}),
        ),
        spawn("recursive", "explorer"),
        ScriptedReply::text("Reported limitations"),
        ScriptedReply::text("Parent done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(!fixture.project_dir.path().join("forbidden.txt").exists());
    let requests = provider.requests();
    for (index, tool) in [(2, "write-1"), (3, "recursive")] {
        assert!(
            requests[index].messages.iter().any(
                |m| matches!(m, Message::ToolResult(r) if r.tool_call_id == tool && r.is_error)
            )
        );
    }
    assert_eq!(
        std::fs::read_dir(fixture.data_dir.path().join("subagents/chat-1/results"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn worker_approval_is_visible_in_parent_and_grants_are_shared() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        spawn("spawn-1", "worker"),
        ScriptedReply::tool_call(
            "write-1",
            "write",
            json!({"path":"assigned.txt", "content":"child"}),
        ),
        ScriptedReply::text("Child done"),
        ScriptedReply::tool_call(
            "parent-write",
            "write",
            json!({"path":"assigned.txt", "content":"parent"}),
        ),
        ScriptedReply::text("Parent done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate implementation").await;
    let id = child_id(&engine, "spawn-1").await;
    let gate = common::wait_for_gate(&engine, "chat-1", &format!("{id}:write-1"), "pending").await;
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let approval = parts(&snapshot)
        .into_iter()
        .find(|p| p["gate"]["id"] == gate)
        .unwrap();
    assert_eq!(approval["gate"]["origin"]["docId"], id);
    assert!(!fixture.project_dir.path().join("assigned.txt").exists());
    common::resolve_approval(&engine, &gate, json!({"kind":"alwaysAllow"})).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(
        std::fs::read_to_string(fixture.project_dir.path().join("assigned.txt")).unwrap(),
        "parent"
    );
    common::wait_for_gate(&engine, "chat-1", "parent-write", "settled:exempted").await;
    assert!(provider.requests()[1].tool_names.contains(&"write".into()));
    assert!(!provider.requests()[1].tool_names.contains(&"Agent".into()));
}

#[tokio::test]
async fn interrupt_cancels_child_approval_and_restart_does_not_resume_it() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        spawn("spawn-1", "worker"),
        ScriptedReply::tool_call(
            "write-1",
            "write",
            json!({"path":"assigned.txt", "content":"child"}),
        ),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    let id = child_id(&engine, "spawn-1").await;
    common::wait_for_gate(&engine, "chat-1", &format!("{id}:write-1"), "pending").await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(!fixture.project_dir.path().join("assigned.txt").exists());
    common::wait_for_gate(
        &engine,
        "chat-1",
        &format!("{id}:write-1"),
        "settled:aborted",
    )
    .await;
    drop(engine);
    let fresh = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&fresh);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(
        parts(&snapshot)
            .into_iter()
            .find(|p| p["id"] == "spawn-1")
            .unwrap()["subagentStatus"],
        "failed"
    );
    assert!(frozen(&engine, &id).await.to_string().contains("aborted"));
    assert!(fresh.requests().is_empty());
}

#[tokio::test]
async fn concurrent_children_are_limited_and_a_failure_does_not_cancel_siblings() {
    let fixture = Fixture::new();
    let gates: Vec<_> = (0..4)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let mut script = vec![ScriptedReply::ToolCalls(
        (0..5)
            .map(|i| common::tool_call(&format!("spawn-{i}"), "Agent", brief("explorer")))
            .collect(),
    )];
    script.extend(
        gates
            .iter()
            .map(|g| ScriptedReply::gated(g.clone(), "Child done")),
    );
    script.push(ScriptedReply::Failed("Child provider failure".into()));
    script.push(ScriptedReply::text("Parent handled all results"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Investigate in parallel").await;
    common::wait_for_requests(&provider, 5).await;
    assert_eq!(
        provider.requests().len(),
        5,
        "fifth child must wait for a slot"
    );
    gates[0].notify_one();
    common::wait_for_requests(&provider, 6).await;
    for gate in &gates[1..] {
        gate.notify_one();
    }
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 7);
    let results: Vec<_> = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 5);
    assert_eq!(results.iter().filter(|r| r.is_error).count(), 1);
}

#[tokio::test]
async fn ninth_child_is_rejected_without_a_provider_request() {
    let fixture = Fixture::new();
    let mut script = vec![ScriptedReply::ToolCalls(
        (0..9)
            .map(|i| common::tool_call(&format!("spawn-{i}"), "Agent", brief("explorer")))
            .collect(),
    )];
    script.extend((0..8).map(|_| ScriptedReply::text("Child done")));
    script.push(ScriptedReply::text("Parent done"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate nine").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 10);
    assert!(
        requests.last().unwrap().messages.iter().any(
            |m| matches!(m, Message::ToolResult(r) if r.tool_call_id == "spawn-8" && r.is_error)
        )
    );
}

#[tokio::test]
async fn long_child_results_have_a_token_budget_and_a_readable_full_file() {
    let fixture = Fixture::new();
    let long = "many detailed findings with evidence\n".repeat(5000);
    let provider = ScriptedProvider::new(vec![
        spawn("spawn-1", "explorer"),
        ScriptedReply::text(&long),
        ScriptedReply::text("Parent done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let id = child_id(&engine, "spawn-1").await;
    let file = fixture
        .data_dir
        .path()
        .join("subagents/chat-1/results")
        .join(format!("{id}.txt"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), long);
    let requests = provider.requests();
    let result = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .unwrap();
    let text = result
        .content
        .iter()
        .filter_map(|b| match b {
            pi_core::ai::types::BlockContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Result truncated"));
    assert!(text.contains(file.to_str().unwrap()));
    assert!(
        tiktoken_rs::o200k_base()
            .unwrap()
            .encode_ordinary(&text)
            .len()
            <= 12_000
    );
}

#[tokio::test]
async fn a_child_can_make_more_than_32_requests() {
    let fixture = Fixture::new();
    std::fs::write(fixture.project_dir.path().join("notes.txt"), "evidence").unwrap();
    let mut script = vec![spawn("spawn-1", "explorer")];
    script.extend((0..34).map(|i| {
        ScriptedReply::tool_call(&format!("read-{i}"), "read", json!({"path":"notes.txt"}))
    }));
    script.push(ScriptedReply::text("Completed a long investigation"));
    script.push(ScriptedReply::text("Parent done"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert_eq!(provider.requests().len(), 37);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(
        parts(&snapshot)
            .into_iter()
            .find(|p| p["id"] == "spawn-1")
            .unwrap()["subagentStatus"],
        "done"
    );
}

#[tokio::test]
async fn restart_settles_a_live_child_without_reexecution() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![spawn("spawn-1", "explorer"), ScriptedReply::Silent]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_requests(&provider, 2).await;
    let id = child_id(&engine, "spawn-1").await;
    drop(engine);
    let fresh = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&fresh);
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(
        parts(&snapshot)
            .into_iter()
            .find(|p| p["id"] == "spawn-1")
            .unwrap()["subagentStatus"],
        "failed"
    );
    assert!(frozen(&engine, &id).await.to_string().contains("aborted"));
    assert!(fresh.requests().is_empty());
}

#[tokio::test]
async fn child_compaction_is_independent_and_its_usage_is_included() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project_dir.path().join("notes.txt"),
        "read evidence",
    )
    .unwrap();
    let mut args = brief("explorer");
    args["prompt"] = json!("Child task context. ".repeat(6500));
    let high_usage = pi_core::ai::types::Usage {
        input: 260_000,
        output: 500,
        total_tokens: 260_500,
        ..Default::default()
    };
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("spawn-1", "Agent", args),
        ScriptedReply::tool_call_with_usage(
            "read-1",
            "read",
            json!({"path":"notes.txt"}),
            high_usage.clone(),
        ),
        ScriptedReply::text("Child compaction summary"),
        ScriptedReply::text("Child final findings"),
        ScriptedReply::text("Parent final answer"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[2].tools, 0);
    assert!(
        serde_json::to_string(&requests[3].messages)
            .unwrap()
            .contains("Child compaction summary")
    );
    let result = requests[4]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "spawn-1" => Some(r),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        result.usage.as_ref().unwrap().total_tokens,
        high_usage.total_tokens + 2 * common::fixed_usage().total_tokens
    );
    assert!(
        !serde_json::to_string(&requests[4].messages)
            .unwrap()
            .contains("Child compaction summary")
    );
    let id = child_id(&engine, "spawn-1").await;
    assert!(
        frozen(&engine, &id)
            .await
            .to_string()
            .contains("compactionDivider")
    );
    let parent = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        !parts(&parent)
            .into_iter()
            .any(|p| p["type"] == "compactionDivider")
    );
}

#[tokio::test]
async fn auto_review_uses_the_inherited_model_and_counts_its_usage() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        spawn("spawn-1", "worker"),
        ScriptedReply::tool_call(
            "write-1",
            "write",
            json!({"path":"assigned.txt", "content":"child"}),
        ),
        ScriptedReply::text("REJECT: outside assigned scope"),
        ScriptedReply::text("Child reported the rejection"),
        ScriptedReply::text("Parent done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(
            methods::MUTATE,
            json!({"op":"setChatPermissionMode","chatId":"chat-1","mode":"auto-review"}),
        )
        .await
        .unwrap();
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[2].tools, 0);
    assert_eq!(requests[2].model, requests[0].model);
    assert!(!fixture.project_dir.path().join("assigned.txt").exists());
    let result = requests[4]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        result.usage.as_ref().unwrap().total_tokens,
        3 * common::fixed_usage().total_tokens
    );
    assert!(
        common::transcript_snapshot(&engine, "chat-1")
            .await
            .to_string()
            .contains("outside assigned scope")
    );
}

#[tokio::test]
async fn steer_waits_for_child_cleanup_before_starting_the_next_turn() {
    let fixture = Fixture::new();
    let observed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        spawn("spawn-1", "explorer"),
        ScriptedReply::Cancelling {
            observed: observed.clone(),
            finish: finish.clone(),
        },
        ScriptedReply::text("New direction completed"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_requests(&provider, 2).await;
    engine.handle(methods::QUEUE_COMMAND, json!({"chatId":"chat-1","command":{
        "kind":"steer","prompt":"New direction","request":{
            "prompt":"New direction","provider":"openai","model":"openai/gpt-5.4","cwd":fixture.cwd()
        }
    }})).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    assert_eq!(
        provider.requests().len(),
        2,
        "next Turn must wait for the child transport to finish"
    );
    finish.notify_one();
    common::wait_for_requests(&provider, 3).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        common::summarize(&requests[2].messages)
            .join("\n")
            .contains("New direction")
    );
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let chip = parts(&snapshot)
        .into_iter()
        .find(|p| p["id"] == "spawn-1")
        .unwrap();
    assert_eq!(chip["subagentStatus"], "failed");
    assert!(chip["output"].as_str().unwrap().contains("partial A"));
}

#[tokio::test]
async fn concurrency_limit_is_shared_between_parent_chats() {
    let fixture = Fixture::new();
    let gates: Vec<_> = (0..4)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let mut script = vec![ScriptedReply::ToolCalls(
        (0..4)
            .map(|i| common::tool_call(&format!("spawn-{i}"), "Agent", brief("explorer")))
            .collect(),
    )];
    script.extend(
        gates
            .iter()
            .map(|g| ScriptedReply::gated(g.clone(), "First chat child done")),
    );
    script.push(spawn("other-spawn", "explorer"));
    script.push(ScriptedReply::text("Other chat child done"));
    script.push(ScriptedReply::text("Other chat parent done"));
    script.push(ScriptedReply::text("First chat parent done"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::setup_chat(&engine, "chat-2").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate four").await;
    common::wait_for_requests(&provider, 5).await;
    common::run_prompt(&engine, "chat-2", &fixture.cwd(), "Delegate one").await;
    common::wait_for_requests(&provider, 6).await;
    assert_eq!(provider.requests().len(), 6);
    gates[0].notify_one();
    common::wait_for_requests(&provider, 8).await;
    for gate in &gates[1..] {
        gate.notify_one();
    }
    common::wait_for_requests(&provider, 9).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
}
