//! Planning-turn shaping (ADR-0025, issue 02): a Turn admitted under Plan
//! Mode runs with the read-only exploration tools plus the two plan tools,
//! a system prompt that makes `submit_plan` the only submission channel,
//! and one model-only corrective continuation when the Turn ends without a
//! submission. Ordinary turns are untouched.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply, run_prompt, tool_call, wait_for_requests};
use holt_rpc::{RpcReply, RpcService, methods};

async fn enter_plan_mode(engine: &holt_engine::LocalEngine, chat_id: &str) {
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_planning_turn_mounts_only_read_only_and_plan_tools() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("ordinary reply"),
        ScriptedReply::text("planning reply"),
        // The planning reply is text-only, so the corrective continuation
        // fires — and its own text-only reply ends the retry (one nudge
        // only; Plan Mode then waits for the user).
        ScriptedReply::text("still no submission"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // The ordinary turn mounts the full toolset, bash included — the
    // default confirm-changes tier, but nothing here is mutating.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "ordinary").await;
    wait_for_requests(&provider, 1).await;
    let ordinary = &provider.requests()[0];
    assert!(ordinary.tool_names.iter().any(|name| name == "bash"));
    assert!(ordinary.tool_names.iter().any(|name| name == "Agent"));

    // The turn admitted after Plan Mode was entered runs PLANNED: only the
    // read-only exploration tools plus write_plan and submit_plan. bash,
    // write, edit, and delegation are absent, not gated — there is nothing
    // to approve, so the turn completes without an approval pause.
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    enter_plan_mode(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    wait_for_requests(&provider, 2).await;
    let planned = &provider.requests()[1];
    for name in ["bash", "write", "edit", "Agent"] {
        assert!(
            !planned.tool_names.iter().any(|candidate| candidate == name),
            "planning turns must not mount {name}: {:?}",
            planned.tool_names
        );
    }
    for name in [
        "read",
        "grep",
        "read_chat",
        "web_fetch",
        "write_plan",
        "submit_plan",
    ] {
        assert!(
            planned.tool_names.iter().any(|candidate| candidate == name),
            "planning turns must mount {name}: {:?}",
            planned.tool_names
        );
    }
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
}

#[tokio::test]
async fn the_planning_prompt_makes_submit_plan_the_only_submission_channel() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("ordinary reply"),
        ScriptedReply::text("planning reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "ordinary").await;
    wait_for_requests(&provider, 1).await;
    assert!(
        !provider.requests()[0]
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .contains("Plan Mode (active)"),
        "ordinary turns must not carry the planning block"
    );

    enter_plan_mode(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    wait_for_requests(&provider, 2).await;
    let requests = provider.requests();
    let prompt = requests[1].system_prompt.as_deref().unwrap_or_default();
    assert!(prompt.contains("## Plan Mode (active)"));
    // The document the user reviews is named, under the chat working
    // directory's .holt/plans.
    assert!(prompt.contains(".holt/plans/chat-1-"));
    assert!(prompt.ends_with(".md\n") || prompt.contains(".md"));
    // The strong requirement: ordinary text is never a submission.
    assert!(prompt.contains("ONLY through the `submit_plan` tool call"));
    assert!(prompt.contains("does NOT submit the plan"));
}

#[tokio::test]
async fn a_text_only_planning_reply_gets_one_corrective_continuation_then_stops() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("I have decided on the approach."),
        ScriptedReply::text("Still just talking, no submission."),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    wait_for_requests(&provider, 2).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        provider.requests().len(),
        2,
        "exactly one corrective continuation: the initial request and the nudge"
    );

    // The continuation rode one nudge user message telling the model the
    // text ending was not a submission.
    let nudge_request = &provider.requests()[1];
    let rendered = format!("{:?}", nudge_request.messages);
    assert!(rendered.contains("cannot submit the plan"));
    assert!(
        nudge_request
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .contains("Plan Mode (active)"),
        "the continuation keeps the planning prompt"
    );

    // Plan Mode stays put awaiting user input; no plan revision was
    // promoted to await approval by plain text.
    let RpcReply::Value(state) = engine
        .handle(
            methods::GET_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("GetPlanMode did not return a value");
    };
    assert_eq!(state["active"], serde_json::json!(true));
    assert_eq!(state.get("activePlan"), None);
}

#[tokio::test]
async fn a_submit_plan_call_ends_the_planning_turn_without_a_continuation() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::ToolCalls(vec![tool_call(
        "submit-1",
        "submit_plan",
        serde_json::json!({}),
    )])]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        provider.requests().len(),
        1,
        "submission ends the planning Turn at the close of the tool round: no continuation"
    );
}

#[tokio::test]
async fn the_model_writes_then_submits_the_plan_through_the_gate() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![tool_call(
            "write-1",
            "write_plan",
            serde_json::json!({ "content": "# Plan\n1. do the thing" }),
        )]),
        ScriptedReply::ToolCalls(vec![tool_call(
            "submit-1",
            "submit_plan",
            serde_json::json!({}),
        )]),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    // Default confirm-changes tier: the plan write path must NOT meet the
    // mutating-call gate — the turn runs to submission with no approval.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The write round, then the submission round — which ends the turn.
    assert_eq!(provider.requests().len(), 2);
    let plans_dir = std::path::Path::new(&fixture.cwd()).join(".holt/plans");
    let mut plan_files = std::fs::read_dir(&plans_dir)
        .expect("the plans directory exists under the chat working directory");
    let only = plan_files.next().expect("one plan document").unwrap();
    assert!(
        only.file_name().to_string_lossy().starts_with("chat-1-"),
        "plan documents are named <chatId>-<planId>.md"
    );
    assert_eq!(
        std::fs::read_to_string(only.path()).unwrap(),
        "# Plan\n1. do the thing"
    );
}

#[tokio::test]
async fn an_interrupted_planning_turn_gets_no_continuation() {
    let fixture = Fixture::new();
    // A transport that observes the cancellation and then delivers its
    // (delayed) abort: the interrupt settles the turn as interrupted.
    let observed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::Cancelling {
        observed: Arc::clone(&observed),
        finish: Arc::clone(&finish),
    }]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    wait_for_requests(&provider, 1).await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": "chat-1", "command": { "kind": "interrupt" } }),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), observed.notified())
        .await
        .expect("the interrupt never reached the transport");
    finish.notify_one();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        provider.requests().len(),
        1,
        "an interrupted planning turn gets no corrective continuation"
    );
}
