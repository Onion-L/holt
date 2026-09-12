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
    assert_eq!(state["activePlan"]["state"], serde_json::json!("planning"));
}

#[tokio::test]
async fn a_submit_plan_call_ends_the_planning_turn_without_a_continuation() {
    let fixture = Fixture::new();
    // The model writes the document, then submits: the loop stops at the
    // close of the submitting round (no corrective continuation, no third
    // request).
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![tool_call(
            "write-1",
            "write_plan",
            serde_json::json!({ "content": "# Plan" }),
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

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        provider.requests().len(),
        2,
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

#[tokio::test]
async fn the_submitted_revision_persists_and_survives_restart() {
    let fixture = Fixture::new();
    let data_dir = fixture.data_dir.path().to_path_buf();
    let personal = fixture.personal_dir.path().to_path_buf();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![tool_call(
            "write-1",
            "write_plan",
            serde_json::json!({ "content": "# The plan" }),
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

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

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
    assert_eq!(
        state["activePlan"]["state"],
        serde_json::json!("awaitingApproval")
    );
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();
    let plan_path = state["planPath"].as_str().unwrap().to_string();
    assert!(plan_path.contains(&format!(".holt/plans/chat-1-{plan_id}.md")));
    assert!(std::path::Path::new(&plan_path).exists());
    drop(engine);

    // Restart: the awaiting-approval state (and its revision id) is
    // restored, and recovery still starts no Turn.
    let restarted = ScriptedProvider::new(vec![]);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir,
        personal_skills_dir: Some(personal),
        stream_fn: Some(restarted.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    let RpcReply::Stream(mut sessions) = engine
        .handle(methods::WATCH_SESSIONS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSessions did not return a stream");
    };
    let frame = common::next_frame(&mut sessions).await;
    assert!(
        !frame
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["chatId"] == "chat-1" && row["status"] == "working")
    );
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
    assert_eq!(
        state["activePlan"]["state"],
        serde_json::json!("awaitingApproval")
    );
    assert_eq!(state["activePlan"]["planId"], serde_json::json!(plan_id));
}

#[tokio::test]
async fn a_follow_up_planning_turn_revises_the_same_revision() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![tool_call(
            "write-1",
            "write_plan",
            serde_json::json!({ "content": "# The plan" }),
        )]),
        ScriptedReply::ToolCalls(vec![tool_call(
            "submit-1",
            "submit_plan",
            serde_json::json!({}),
        )]),
        // The user sent another message without resolving: a continuation
        // planning turn on the SAME revision. Its text-only reply earns the
        // one corrective continuation, whose reply ends the turn.
        ScriptedReply::text("refining"),
        ScriptedReply::text("still text"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
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
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();

    run_prompt(&engine, "chat-1", &fixture.cwd(), "also consider X").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
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
    assert_eq!(
        state["activePlan"]["planId"],
        serde_json::json!(plan_id),
        "the planning cycle keeps ONE revision id across its turns"
    );
    let plans_dir = std::path::Path::new(&fixture.cwd()).join(".holt/plans");
    assert_eq!(
        std::fs::read_dir(&plans_dir).unwrap().count(),
        1,
        "a continuation turn never mints a second document"
    );
}

#[tokio::test]
async fn exit_then_reenter_mints_a_new_revision_keeping_old_documents() {
    let fixture = Fixture::new();
    let write_submit = || {
        vec![
            ScriptedReply::ToolCalls(vec![tool_call(
                "write-x",
                "write_plan",
                serde_json::json!({ "content": "# The plan" }),
            )]),
            ScriptedReply::ToolCalls(vec![tool_call(
                "submit-x",
                "submit_plan",
                serde_json::json!({}),
            )]),
        ]
    };
    let mut script = write_submit();
    script.extend(write_submit());
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let RpcReply::Value(first) = engine
        .handle(
            methods::GET_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("GetPlanMode did not return a value");
    };
    let first_id = first["activePlan"]["planId"].as_str().unwrap().to_string();

    // Leaving Plan Mode retires the revision; re-entering mints a new one
    // for the next cycle. The old document stays on disk.
    engine
        .handle(
            methods::EXIT_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    enter_plan_mode(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan differently").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let RpcReply::Value(second) = engine
        .handle(
            methods::GET_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("GetPlanMode did not return a value");
    };
    let second_id = second["activePlan"]["planId"].as_str().unwrap().to_string();
    assert_ne!(first_id, second_id, "a new cycle mints a new revision");
    let plans_dir = std::path::Path::new(&fixture.cwd()).join(".holt/plans");
    let files: Vec<String> = std::fs::read_dir(&plans_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 2, "older revisions are retained: {files:?}");
    assert!(files.iter().any(|name| name.contains(&first_id)));
    assert!(files.iter().any(|name| name.contains(&second_id)));
}

async fn get_state(engine: &holt_engine::LocalEngine, chat_id: &str) -> serde_json::Value {
    let RpcReply::Value(state) = engine
        .handle(
            methods::GET_PLAN_MODE,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("GetPlanMode did not return a value");
    };
    state
}

async fn set_mode(engine: &holt_engine::LocalEngine, chat_id: &str, mode: &str) {
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": chat_id,
                "mode": mode,
            }),
        )
        .await
        .unwrap();
}

/// A write_plan→submit_plan script pair.
fn write_submit() -> Vec<ScriptedReply> {
    vec![
        ScriptedReply::ToolCalls(vec![tool_call(
            "write-x",
            "write_plan",
            serde_json::json!({ "content": "# The plan\n- step one" }),
        )]),
        ScriptedReply::ToolCalls(vec![tool_call(
            "submit-x",
            "submit_plan",
            serde_json::json!({}),
        )]),
    ]
}

async fn resolve(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    plan_id: &str,
    verdict: &str,
    feedback: Option<&str>,
) -> Result<serde_json::Value, holt_rpc::RpcError> {
    let mut params = serde_json::json!({
        "chatId": chat_id,
        "planId": plan_id,
        "verdict": verdict,
    });
    if let Some(feedback) = feedback {
        params["feedback"] = serde_json::Value::String(feedback.into());
    }
    match engine.handle(methods::RESOLVE_PLAN_APPROVAL, params).await {
        Ok(RpcReply::Value(state)) => Ok(state),
        Ok(_) => panic!("ResolvePlanApproval did not return a value"),
        Err(error) => Err(error),
    }
}

#[tokio::test]
async fn approve_exits_plan_mode_restores_the_entry_mode_and_injects_the_plan() {
    let fixture = Fixture::new();
    let mut script = vec![ScriptedReply::text("seed reply")]; // the seed turn
    script.extend(write_submit());
    script.push(ScriptedReply::text("implementing")); // the next turn
    script.push(ScriptedReply::text("and again")); // a further turn
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // Seed the config, then enter: full-access is the entry mode.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "seed").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    set_mode(&engine, "chat-1", "full-access").await;
    enter_plan_mode(&engine, "chat-1").await;
    // Planning under a different tier must not matter: the entry mode is
    // what approval restores.
    set_mode(&engine, "chat-1", "confirm-changes").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let state = get_state(&engine, "chat-1").await;
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();
    // The card is in the transcript, pending, carrying the submitted
    // plan's text as a snapshot (the card renders what was reviewed).
    let card = common::transcript_snapshot(&engine, "chat-1").await;
    let card = card.to_string();
    assert!(
        card.contains("planApproval") && card.contains(&plan_id),
        "the submitted plan carries a transcript card: {card}"
    );
    assert!(
        card.contains("step one"),
        "the card embeds the submitted plan text: {card}"
    );

    resolve(&engine, "chat-1", &plan_id, "approve", None)
        .await
        .unwrap();

    // Approval exits Plan Mode and restores the ENTRY mode (full-access),
    // not whatever tier happened to be set during planning.
    let state = get_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(false));
    assert_eq!(
        common::watched_permission_mode(&engine, "chat-1").await,
        serde_json::json!("full-access")
    );

    // The pinned plan survives a restart: a fresh engine still carries the
    // reference (the injection below is the same admission path it feeds).
    let data_dir = fixture.data_dir.path().to_path_buf();
    drop(engine);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("implementing"), // the "go" Turn
        ScriptedReply::text("and again"),    // the "continue" Turn
    ]);
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir,
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let frame = common::next_frame(&mut chats).await;
    assert!(
        frame[0]["approvedPlanPath"]
            .as_str()
            .unwrap_or_default()
            .contains(".holt/plans/chat-1-"),
        "the approved plan reference survives restart: {frame}"
    );
    drop(chats);

    // The old engine's watches died with it — subscribe fresh ones.
    let RpcReply::Stream(mut sessions) = engine
        .handle(methods::WATCH_SESSIONS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSessions did not return a stream");
    };
    let _ = common::next_frame(&mut sessions).await;

    // The next implementation Turn rides the approved plan whole.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let implementing = requests.last().unwrap();
    // The NEW prompt (the request's last message) carries the plan.
    let fresh_prompt = format!("{:?}", implementing.messages.last().unwrap());
    assert!(
        fresh_prompt.contains("<approved-plan>"),
        "the approved plan is injected into the implementation Turn"
    );
    assert!(fresh_prompt.contains("step one"));

    // …and the reference is consumed: the turn after that injects nothing.
    // (The plan text legitimately remains in the conversation HISTORY —
    // the check is the NEW prompt, the request's last message.)
    run_prompt(&engine, "chat-1", &fixture.cwd(), "continue").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let fresh_prompt = format!("{:?}", requests.last().unwrap().messages.last().unwrap());
    assert!(!fresh_prompt.contains("<approved-plan>"));
}

#[tokio::test]
async fn reject_retires_the_plan_and_the_feedback_drives_a_new_revision() {
    let fixture = Fixture::new();
    let mut script = write_submit();
    // The feedback enqueues the revision's planning input; its text-only
    // reply earns the one corrective continuation, then the turn ends.
    script.push(ScriptedReply::text("thinking about the feedback"));
    script.push(ScriptedReply::text("still nothing"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let state = get_state(&engine, "chat-1").await;
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();

    resolve(
        &engine,
        "chat-1",
        &plan_id,
        "reject",
        Some("use the database layer, not raw SQL"),
    )
    .await
    .unwrap();

    // Rejection keeps the chat planning; the revision was retired so the
    // next cycle mints a fresh id.
    let state = get_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    assert_eq!(state.get("activePlan"), None);

    // The feedback was enqueued as the revision loop's next planning input:
    // a new planning turn starts on its own, with a NEW plan id.
    common::wait_for_requests(&provider, 3).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        format!("{:?}", requests[2].messages).contains("use the database layer"),
        "the feedback is the new planning turn's input"
    );
    assert!(
        requests[2]
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .contains("Plan Mode (active)"),
        "the revision runs as a planning turn"
    );
    let state = get_state(&engine, "chat-1").await;
    let new_id = state["activePlan"]["planId"].as_str().unwrap().to_string();
    assert_ne!(new_id, plan_id, "the revision loop mints a fresh revision");

    // The original document stays on disk.
    let plans_dir = std::path::Path::new(&fixture.cwd()).join(".holt/plans");
    assert!(plans_dir.join(format!("chat-1-{plan_id}.md")).exists());
}

#[tokio::test]
async fn remain_keeps_the_chat_planning_without_starting_a_turn() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(write_submit());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let state = get_state(&engine, "chat-1").await;
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();

    resolve(&engine, "chat-1", &plan_id, "remain", None)
        .await
        .unwrap();

    let state = get_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    assert_eq!(state["activePlan"]["planId"], serde_json::json!(plan_id));
    assert_eq!(state["activePlan"]["state"], serde_json::json!("planning"));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        provider.requests().len(),
        2,
        "remaining in Plan Mode starts no execution Turn"
    );
}

#[tokio::test]
async fn resolution_refuses_stale_or_wrong_targets() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(write_submit());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    // Nothing submitted yet: every verdict fails.
    for verdict in ["approve", "reject", "remain"] {
        let error = match resolve(&engine, "chat-1", "whatever", verdict, None).await {
            Ok(_) => panic!("{verdict} must fail without an awaiting plan"),
            Err(error) => error,
        };
        assert!(matches!(error, holt_rpc::RpcError::Failed(_)));
    }

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let state = get_state(&engine, "chat-1").await;
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();

    // A wrong plan id fails; the right one approves exactly once.
    let error = match resolve(&engine, "chat-1", "other-plan", "approve", None).await {
        Ok(_) => panic!("a wrong plan id must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, holt_rpc::RpcError::Failed(_)));
    resolve(&engine, "chat-1", &plan_id, "approve", None)
        .await
        .unwrap();
    // Plan Mode is over: a second verdict for the same plan fails.
    let error = match resolve(&engine, "chat-1", &plan_id, "reject", None).await {
        Ok(_) => panic!("a resolved plan must not resolve again"),
        Err(error) => error,
    };
    assert!(matches!(error, holt_rpc::RpcError::Failed(_)));
}

#[tokio::test]
async fn exiting_with_a_pending_card_settles_it_as_dismissed() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(write_submit());
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let state = get_state(&engine, "chat-1").await;
    let plan_id = state["activePlan"]["planId"].as_str().unwrap().to_string();

    engine
        .handle(
            methods::EXIT_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();

    // The card settles as dismissed, never left answerable.
    let snapshot = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(
        snapshot.contains("dismissed") && snapshot.contains(&plan_id),
        "the pending card settles as dismissed on exit: {snapshot}"
    );
}

#[tokio::test]
async fn the_full_planning_cycle_end_to_end() {
    // The whole ADR-0025 loop in one flow: enter → planning turn (write +
    // submit) → reject with feedback → revision turn on a NEW revision →
    // submit again → approve → implementation turn with the plan injected.
    let fixture = Fixture::new();
    let mut script: Vec<ScriptedReply> = Vec::new();
    // Cycle 1 (the /plan <task> input arrives as an ordinary run).
    script.extend(write_submit());
    // Cycle 2 (the feedback drives the revision; text-only reply earns the
    // corrective continuation, whose second text-only reply ends it — wait,
    // the revision WRITES and SUBMITS: no continuation).
    script.extend(write_submit());
    // The implementation turn after approval.
    script.push(ScriptedReply::text("implementing"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan the refactor").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let first_id = get_state(&engine, "chat-1").await["activePlan"]["planId"]
        .as_str()
        .unwrap()
        .to_string();
    resolve(
        &engine,
        "chat-1",
        &first_id,
        "reject",
        Some("split the plan into two phases"),
    )
    .await
    .unwrap();

    // The feedback's revision turn runs to a second submission.
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let state = get_state(&engine, "chat-1").await;
    let second_id = state["activePlan"]["planId"].as_str().unwrap().to_string();
    assert_ne!(second_id, first_id);
    assert_eq!(
        state["activePlan"]["state"],
        serde_json::json!("awaitingApproval"),
        "the revision is submitted and awaiting approval"
    );
    // Both documents exist: the rejected one and the revision.
    let plans_dir = std::path::Path::new(&fixture.cwd()).join(".holt/plans");
    assert_eq!(std::fs::read_dir(&plans_dir).unwrap().count(), 2);

    resolve(&engine, "chat-1", &second_id, "approve", None)
        .await
        .unwrap();
    assert_eq!(
        get_state(&engine, "chat-1").await["active"],
        serde_json::json!(false)
    );

    // The implementation turn carries the revision's plan.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let fresh_prompt = format!("{:?}", requests.last().unwrap().messages.last().unwrap());
    assert!(fresh_prompt.contains("<approved-plan>"));
    assert!(fresh_prompt.contains("step one"));
}
