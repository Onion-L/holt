//! Planning turns and the `<proposed_plan>` flow (ADR-0025): a Turn
//! admitted under Plan Mode runs with the read-only exploration tools and
//! a prompt that makes a complete `<proposed_plan>` Markdown block the
//! only submission channel. Blocks fold into pending approval cards the
//! user resolves with approve / reject-with-feedback / remain — approve
//! exits Plan Mode restoring the entry mode (the plan is already in the
//! conversation History), reject keeps planning with the feedback as the
//! revision loop's next input.

mod common;

use common::{Fixture, ScriptedProvider, ScriptedReply, run_prompt, wait_for_requests};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};

async fn enter_plan_mode(engine: &holt_engine::LocalEngine, chat_id: &str) {
    engine
        .handle(
            methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap();
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

async fn resolve(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    verdict: &str,
    feedback: Option<&str>,
) -> Result<serde_json::Value, RpcError> {
    let mut params = serde_json::json!({ "chatId": chat_id, "verdict": verdict });
    if let Some(feedback) = feedback {
        params["feedback"] = serde_json::Value::String(feedback.into());
    }
    match engine.handle(methods::RESOLVE_PLAN_APPROVAL, params).await {
        Ok(RpcReply::Value(state)) => Ok(state),
        Ok(_) => panic!("ResolvePlanApproval did not return a value"),
        Err(error) => Err(error),
    }
}

/// One scripted reply proposing a complete plan.
fn propose(what: &str) -> ScriptedReply {
    ScriptedReply::text(format!(
        "I explored the code.\n\n<proposed_plan>\n# Plan\n- {what}\n</proposed_plan>"
    ))
}

#[tokio::test]
async fn a_planning_turn_mounts_only_read_only_tools() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("ordinary reply"),
        ScriptedReply::text("planning reply"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // The ordinary turn mounts the full toolset, bash and delegation
    // included.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "ordinary").await;
    wait_for_requests(&provider, 1).await;
    assert!(
        provider.requests()[0]
            .tool_names
            .iter()
            .any(|name| name == "bash")
    );
    assert!(
        provider.requests()[0]
            .tool_names
            .iter()
            .any(|name| name == "Agent")
    );

    // The turn admitted after Plan Mode was entered runs PLANNED: the
    // read-only exploration surface only. bash, write, edit, delegation —
    // and the plan itself is text, so no plan tools exist at all.
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    enter_plan_mode(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    wait_for_requests(&provider, 2).await;
    let planned = &provider.requests()[1];
    for name in [
        "bash",
        "write",
        "edit",
        "Agent",
        "write_plan",
        "submit_plan",
    ] {
        assert!(
            !planned.tool_names.iter().any(|candidate| candidate == name),
            "planning turns must not mount {name}: {:?}",
            planned.tool_names
        );
    }
    for name in ["read", "grep", "read_chat", "web_fetch"] {
        assert!(planned.tool_names.iter().any(|candidate| candidate == name));
    }
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
}

#[tokio::test]
async fn the_planning_prompt_makes_a_proposed_plan_block_the_only_submission_channel() {
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
    assert!(prompt.contains("<proposed_plan>"));
    assert!(prompt.contains("Only a complete block reaches approval"));
    assert!(prompt.contains("read-only exploration"));
}

#[tokio::test]
async fn a_proposed_plan_block_folds_into_a_pending_card() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![propose("step one")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The block folded into a pending card carrying the plan's Markdown;
    // the tags themselves are gone from the transcript.
    let card = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(card.contains("planApproval"), "card present: {card}");
    assert!(card.contains("# Plan") && card.contains("step one"));
    assert!(
        !card.contains("<proposed_plan>"),
        "tags are stripped: {card}"
    );

    // The card is display state only: the resolve guard counts pending
    // cards, which this test's sibling (resolution_refuses…) exercises.
}

#[tokio::test]
async fn plain_text_without_a_block_gets_no_card() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("I have questions before proposing."),
        // An unterminated block is not a proposal either.
        ScriptedReply::text("<proposed_plan>\nhalf a thought"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    for prompt in ["first", "second"] {
        run_prompt(&engine, "chat-1", &fixture.cwd(), prompt).await;
        common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    }
    let card = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(
        !card.contains("planApproval"),
        "no card without a complete block"
    );
    // Ordinary conversation continues normally: two turns, two requests,
    // no extra nudges or continuation rounds.
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn approve_exits_plan_mode_restores_the_entry_mode_and_history_carries_the_plan() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("seed"),
        propose("step one"),
        ScriptedReply::text("implementing"),
    ]);
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

    resolve(&engine, "chat-1", "approve", None).await.unwrap();

    let state = get_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(false));
    assert_eq!(
        common::watched_permission_mode(&engine, "chat-1").await,
        serde_json::json!("full-access"),
        "approval restores the ENTRY mode, not the tier set during planning"
    );

    // The implementation Turn carries the plan naturally — it is already
    // in the conversation History; there is no injection machinery.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        format!("{:?}", requests.last().unwrap().messages).contains("step one"),
        "the implementation turn reads the approved plan from History"
    );
}

#[tokio::test]
async fn reject_with_feedback_keeps_planning_and_the_feedback_drives_a_revision() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![propose("step one"), propose("step one, revised")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    resolve(
        &engine,
        "chat-1",
        "reject",
        Some("split the plan into two phases"),
    )
    .await
    .unwrap();

    // The chat keeps planning and the feedback was enqueued as the
    // revision loop's next planning input.
    let state = get_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    common::wait_for_requests(&provider, 2).await;
    let requests = provider.requests();
    assert!(
        format!("{:?}", requests[1].messages.last().unwrap())
            .contains("split the plan into two phases"),
        "the feedback is the new planning turn's input"
    );
    assert!(
        requests[1]
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .contains("Plan Mode (active)"),
        "the revision runs as a planning turn"
    );
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Both cards are in the transcript: the rejected first proposal and
    // the pending revision.
    let card = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(card.contains("step one, revised") && card.contains("planApproval"));
}

#[tokio::test]
async fn remain_settles_the_cards_without_starting_a_turn() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![propose("step one")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    resolve(&engine, "chat-1", "remain", None).await.unwrap();

    let state = get_state(&engine, "chat-1").await;
    assert_eq!(state["active"], serde_json::json!(true));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        provider.requests().len(),
        1,
        "remaining in Plan Mode starts no execution Turn"
    );
    let card = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(card.contains("remained"), "the card settles as remained");
}

#[tokio::test]
async fn resolution_refuses_without_plan_mode_or_a_pending_card() {
    let fixture = Fixture::new();
    // The first planning turn just talks — nothing is proposed yet.
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("I have questions before proposing."),
        propose("step one"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // Not planning at all: every verdict fails.
    for verdict in ["approve", "reject", "remain"] {
        let error = match resolve(&engine, "chat-1", verdict, None).await {
            Ok(_) => panic!("{verdict} must fail without Plan Mode"),
            Err(error) => error,
        };
        assert!(matches!(error, RpcError::Failed(_)));
    }

    enter_plan_mode(&engine, "chat-1").await;
    // Planning but nothing proposed yet: still nothing to resolve.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let error = match resolve(&engine, "chat-1", "approve", None).await {
        Ok(_) => panic!("approving without a proposed plan must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, RpcError::Failed(_)));

    // An unknown verdict is a param error.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "go again").await;
    wait_for_requests(&provider, 2).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let error = match resolve(&engine, "chat-1", "maybe", None).await {
        Ok(_) => panic!("an unknown verdict must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, RpcError::BadParams(_)));

    // Approve works exactly once: Plan Mode is over afterwards.
    resolve(&engine, "chat-1", "approve", None).await.unwrap();
    let error = match resolve(&engine, "chat-1", "reject", None).await {
        Ok(_) => panic!("a resolved checkpoint must not resolve again"),
        Err(error) => error,
    };
    assert!(matches!(error, RpcError::Failed(_)));
}

#[tokio::test]
async fn exiting_settles_pending_cards_as_dismissed() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![propose("step one")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan this").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    engine
        .handle(
            methods::EXIT_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();

    let snapshot = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(
        snapshot.contains("dismissed"),
        "pending cards settle as dismissed on exit: {snapshot}"
    );
}

#[tokio::test]
async fn the_full_planning_cycle_end_to_end() {
    // The whole ADR-0025 loop in one flow: enter → propose → reject with
    // feedback → revise → approve → the implementation turn reads the
    // revised plan from History.
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        propose("step one"),
        propose("step one split into two phases"),
        ScriptedReply::text("implementing"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    enter_plan_mode(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "plan the refactor").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    resolve(
        &engine,
        "chat-1",
        "reject",
        Some("split the plan into two phases"),
    )
    .await
    .unwrap();

    // The feedback's revision turn proposes the replacement.
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let card = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(card.contains("step one split into two phases"));

    resolve(&engine, "chat-1", "approve", None).await.unwrap();
    assert_eq!(
        get_state(&engine, "chat-1").await["active"],
        serde_json::json!(false)
    );

    run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    let implementation = format!("{:?}", requests.last().unwrap().messages);
    assert!(implementation.contains("split into two phases"));
}
