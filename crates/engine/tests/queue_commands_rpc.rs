//! Handle-seam tests for typed queue commands (message-queue ticket 04):
//! `/skill` invocations and manual Compaction join the same ordered,
//! durable execution channel as ordinary messages. A skill invocation
//! starts its own Turn — resolved from the live filesystem catalog at
//! admission, with its model and reasoning captured at submission — while
//! manual Compaction occupies the channel without becoming a Turn
//! (ADR-0011): `Compacting` status, no transcript echo, no Turn baseline.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::json;

// -- helpers -----------------------------------------------------------------

fn skill(root: &std::path::Path, name: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: the {name} skill.\n---\n{body}\n"),
    )
    .unwrap();
}

fn long_conversation() -> String {
    let mut text = String::new();
    while text.len() < 120_000 {
        text.push_str("queued compaction conversation body ");
    }
    text
}

async fn queue_state(engine: &holt_engine::LocalEngine) -> serde_json::Value {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    common::next_frame(&mut watch).await
}

async fn wait_for_queue(
    engine: &holt_engine::LocalEngine,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    loop {
        let frame = common::next_frame(&mut watch).await;
        if ready(&frame) {
            return frame;
        }
    }
}

async fn wait_drained(engine: &holt_engine::LocalEngine) {
    wait_for_queue(engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
}

async fn queue_command(engine: &holt_engine::LocalEngine, command: serde_json::Value) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":command}),
        )
        .await
        .unwrap();
}

fn request(prompt: &str, cwd: &str) -> serde_json::Value {
    json!({
        "prompt": prompt,
        "provider": "openai",
        "model": "openai/gpt-5.4",
        "reasoning": "high",
        "cwd": cwd,
    })
}

async fn queue_run(engine: &holt_engine::LocalEngine, cwd: &str, message_id: &str, prompt: &str) {
    queue_command(
        engine,
        json!({"kind":"run","messageId":message_id,"request":request(prompt, cwd)}),
    )
    .await;
}

/// Queue an `invokeSkill` command exactly as the composer serializes it.
async fn invoke_skill(
    engine: &holt_engine::LocalEngine,
    cwd: &str,
    name: &str,
    extra: Option<&str>,
    message_id: &str,
) {
    queue_command(
        engine,
        json!({
            "kind": "invokeSkill",
            "name": name,
            "extraInstructions": extra,
            "messageId": message_id,
            "request": request("", cwd),
        }),
    )
    .await;
}

async fn queue_compact(engine: &holt_engine::LocalEngine, cwd: &str, message_id: &str) {
    queue_command(
        engine,
        json!({"kind":"compact","messageId":message_id,"request":request("", cwd)}),
    )
    .await;
}

fn user_text(request: &common::RecordedRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_core::ai::types::Message::User(message) => Some(message.content.text().to_string()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

fn user_entry_count(snapshot: &serde_json::Value) -> usize {
    snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["role"] == "user")
        .count()
}

/// User entries whose opening part is a skill chip (`{"kind":"skill",…}`).
fn user_skill_entry_count(snapshot: &serde_json::Value) -> usize {
    snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["role"] == "user" && entry["parts"][0]["kind"] == json!("skill"))
        .count()
}

// -- mixed ordering ------------------------------------------------------------

#[tokio::test]
async fn mixed_items_serialize_through_one_channel_in_submission_order() {
    let fixture = Fixture::new();
    skill(fixture.personal_dir.path(), "grill", "GRILL-BODY");
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), long_conversation()),
        ScriptedReply::text("skill answer"),
        ScriptedReply::text("answer B"),
        ScriptedReply::text("the summary"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;

    invoke_skill(
        &engine,
        &fixture.cwd(),
        "grill",
        Some("focus on data"),
        "m-skill",
    )
    .await;
    queue_run(&engine, &fixture.cwd(), "m-b", "B").await;
    queue_compact(&engine, &fixture.cwd(), "m-compact").await;

    let state = queue_state(&engine).await;
    let pending = state["pending"].as_array().unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|item| item["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["skill", "ordinary", "compact"]
    );
    assert_eq!(pending[0]["skillName"], "grill");
    assert_eq!(pending[0]["extraInstructions"], "focus on data");
    // Nothing pending has entered the Transcript.
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(user_entry_count(&transcript), 1);

    gate.notify_one();
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    // The skill Turn carries the engine-formatted block plus the extra
    // instructions — never the raw `/skill` directive.
    let skill_prompt = user_text(&requests[1]);
    assert!(
        skill_prompt.contains("<skill name=\"grill\""),
        "{skill_prompt}"
    );
    assert!(skill_prompt.contains("GRILL-BODY"), "{skill_prompt}");
    assert!(skill_prompt.ends_with("focus on data"), "{skill_prompt}");
    assert!(!skill_prompt.contains("/skill grill"), "{skill_prompt}");
    assert_eq!(user_text(&requests[2]), "B");
    // Compaction got only its summary request: no tools, no `/compact`.
    assert_eq!(requests[3].tools, 0);
    let summary_text = serde_json::to_string(&requests[3].messages).unwrap();
    assert!(!summary_text.contains("/compact"), "{summary_text}");

    // One user entry per Turn-starting item; Compaction echoed nothing.
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(user_entry_count(&transcript), 3);
    assert_eq!(user_skill_entry_count(&transcript), 1);
    let transcript = transcript.to_string();
    assert!(transcript.contains("compactionDivider"), "{transcript}");
    assert!(transcript.contains("\"manual\""), "{transcript}");
}

// -- skill resolution at admission ---------------------------------------------

#[tokio::test]
async fn a_changed_skill_is_resolved_at_execution_with_captured_configuration() {
    let fixture = Fixture::new();
    let skill_file = fixture.personal_dir.path().join("grill").join("SKILL.md");
    skill(fixture.personal_dir.path(), "grill", "VERSION-1");
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("skill answer"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;

    invoke_skill(&engine, &fixture.cwd(), "grill", None, "m-skill").await;
    // The body is NOT frozen at submission: edits land in the Turn.
    std::fs::write(
        &skill_file,
        "---\nname: grill\ndescription: the grill skill.\n---\nVERSION-2\n",
    )
    .unwrap();
    // Later picker changes do not move the captured model or reasoning.
    engine
        .handle(
            methods::MUTATE,
            json!({"op":"setChatConfig","chatId":"chat-1","config":{
                "provider":"openai","model":"openai/gpt-5.4-mini","reasoning":"low"
            }}),
        )
        .await
        .unwrap();

    gate.notify_one();
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let skill_prompt = user_text(&requests[1]);
    assert!(skill_prompt.contains("VERSION-2"), "{skill_prompt}");
    assert!(!skill_prompt.contains("VERSION-1"), "{skill_prompt}");
    assert_eq!(requests[1].model, "gpt-5.4");
    assert_eq!(requests[1].reasoning.as_deref(), Some("High"));
}

#[tokio::test]
async fn a_missing_skill_retains_the_head_and_continue_admits_after_repair() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("skill answer"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;

    // No `grill` on disk: the failure is detected at admission, so the
    // pending item is retained with an error and the queue pauses BEFORE
    // any Turn exists.
    invoke_skill(&engine, &fixture.cwd(), "grill", None, "m-skill").await;
    gate.notify_one();
    let state = wait_for_queue(&engine, |q| q["paused"] == true).await;
    assert_eq!(state["pending"][0]["messageId"], "m-skill");
    assert_eq!(state["pending"][0]["kind"], "skill");
    assert!(
        state["pending"][0]["error"]
            .as_str()
            .unwrap()
            .contains("unknown skill"),
        "{state}"
    );
    assert_eq!(provider.requests().len(), 1, "no Turn was created");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(user_entry_count(&transcript), 1);

    // Restoring the skill and continuing admits that same item.
    skill(fixture.personal_dir.path(), "grill", "GRILL-BODY");
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(user_text(&requests[1]).contains("GRILL-BODY"));
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(user_skill_entry_count(&transcript), 1);
}

// -- pending-item actions per kind ----------------------------------------------

#[tokio::test]
async fn a_skill_edit_changes_only_the_extra_instructions() {
    let fixture = Fixture::new();
    skill(fixture.personal_dir.path(), "grill", "GRILL-BODY");
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("skill answer"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    invoke_skill(
        &engine,
        &fixture.cwd(),
        "grill",
        Some("old focus"),
        "m-skill",
    )
    .await;

    let RpcReply::Value(snapshot) = engine
        .handle(
            methods::EDIT_QUEUED_MESSAGE,
            json!({"chatId":"chat-1","messageId":"m-skill","prompt":"new focus"}),
        )
        .await
        .unwrap()
    else {
        panic!("expected a mutation reply")
    };
    let item = &snapshot["pending"][0];
    assert_eq!(item["kind"], "skill");
    assert_eq!(item["skillName"], "grill");
    assert_eq!(item["extraInstructions"], "new focus");
    assert_eq!(item["request"]["model"], "openai/gpt-5.4");
    assert_eq!(item["request"]["reasoning"], "high");

    gate.notify_one();
    wait_drained(&engine).await;
    let prompt = user_text(&provider.requests()[1]);
    assert!(prompt.ends_with("new focus"), "{prompt}");
    assert!(!prompt.contains("old focus"), "{prompt}");
    // The transcript entry shows the chip and the edited extra, once.
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert_eq!(transcript.matches("new focus").count(), 1);
    assert!(!transcript.contains("old focus"), "{transcript}");
}

#[tokio::test]
async fn a_pending_compaction_rejects_edit_and_run_now_but_deletes() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::gated(gate.clone(), "answer A")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_compact(&engine, &fixture.cwd(), "m-compact").await;

    let error = match engine
        .handle(
            methods::EDIT_QUEUED_MESSAGE,
            json!({"chatId":"chat-1","messageId":"m-compact","prompt":"nope"}),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("editing a pending Compaction was accepted"),
    };
    assert!(error.to_string().contains("cannot be edited"), "{error}");
    // Steer of the pending Compaction (the Run now path) is refused too.
    let error = match engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{
                "kind":"steer","prompt":"","messageId":"m-compact"
            }}),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("promoting a pending Compaction was accepted"),
    };
    assert!(error.to_string().contains("cannot run now"), "{error}");

    engine
        .handle(
            methods::DELETE_QUEUED_MESSAGE,
            json!({"chatId":"chat-1","messageId":"m-compact"}),
        )
        .await
        .unwrap();
    assert_eq!(queue_state(&engine).await["pending"], json!([]));
    gate.notify_one();
    wait_drained(&engine).await;
    assert_eq!(provider.requests().len(), 1);
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!transcript.to_string().contains("compactionDivider"));
}

#[tokio::test]
async fn run_now_promotes_a_pending_skill_without_duplication() {
    let fixture = Fixture::new();
    skill(fixture.personal_dir.path(), "grill", "GRILL-BODY");
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("skill answer"),
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "B").await;
    invoke_skill(&engine, &fixture.cwd(), "grill", None, "m-skill").await;

    // Run now on the skill: the SAME item moves ahead of B.
    queue_command(
        &engine,
        json!({"kind":"steer","prompt":"","messageId":"m-skill"}),
    )
    .await;
    gate.notify_one();
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "no duplicated execution");
    assert!(user_text(&requests[1]).contains("GRILL-BODY"));
    assert_eq!(user_text(&requests[2]), "B");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(
        user_skill_entry_count(&transcript),
        1,
        "the promoted skill produced exactly one invocation entry"
    );
}

// -- Steer during Compaction ------------------------------------------------------

#[tokio::test]
async fn steer_during_compaction_waits_for_cleanup_then_runs_the_message() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(long_conversation()),
        // The summary request hangs until interrupted.
        ScriptedReply::Silent,
        ScriptedReply::text("answer D"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    wait_drained(&engine).await;

    queue_compact(&engine, &fixture.cwd(), "m-compact").await;
    common::wait_for_requests(&provider, 2).await;
    common::wait_for_session_status(&mut sessions, "chat-1", "compacting").await;

    // Steering with a new message interrupts the Compaction and waits for
    // its termination before starting D's Turn.
    queue_command(
        &engine,
        json!({"kind":"steer","prompt":"D","request":request("D", &fixture.cwd())}),
    )
    .await;
    common::wait_for_requests(&provider, 3).await;
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(user_text(&requests[2]), "D");
    // The interrupted Compaction left the History untouched: D's request
    // carries the full, uncompacted conversation.
    let d_text = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(
        !d_text.contains("history before this point was compacted"),
        "{d_text}"
    );
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(!transcript.contains("compactionDivider"), "{transcript}");
    assert!(transcript.contains("answer D"), "{transcript}");
}

// -- restart -----------------------------------------------------------------------

#[tokio::test]
async fn restart_restores_mixed_items_paused_in_submission_order() {
    let fixture = Fixture::new();
    skill(fixture.personal_dir.path(), "grill", "GRILL-BODY");
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Silent,
        ScriptedReply::text(long_conversation()),
        ScriptedReply::text("answer B"),
        // The cut splits the long turn with a non-empty history before it,
        // so the Compaction makes its two summary requests.
        ScriptedReply::text("the summary"),
        ScriptedReply::text("the turn prefix summary"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    invoke_skill(&engine, &fixture.cwd(), "grill", Some("focus"), "m-skill").await;
    queue_run(&engine, &fixture.cwd(), "m-b", "B").await;
    queue_compact(&engine, &fixture.cwd(), "m-compact").await;
    queue_run(&engine, &fixture.cwd(), "m-gone", "gone").await;
    // An accepted edit survives the restart too.
    engine
        .handle(
            methods::EDIT_QUEUED_MESSAGE,
            json!({"chatId":"chat-1","messageId":"m-skill","prompt":"edited focus"}),
        )
        .await
        .unwrap();
    // …a deletion…
    engine
        .handle(
            methods::DELETE_QUEUED_MESSAGE,
            json!({"chatId":"chat-1","messageId":"m-gone"}),
        )
        .await
        .unwrap();
    // …and a priority change (Run now on B while A occupies the channel).
    queue_command(
        &engine,
        json!({"kind":"steer","prompt":"","messageId":"m-b"}),
    )
    .await;
    drop(engine);

    let engine = fixture.engine(&provider);
    let state = queue_state(&engine).await;
    assert_eq!(state["paused"], true);
    let pending = state["pending"].as_array().unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|item| item["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["skill", "ordinary", "compact"]
    );
    assert_eq!(pending[0]["extraInstructions"], "edited focus");
    assert_eq!(pending[0]["request"]["model"], "openai/gpt-5.4");
    assert_eq!(provider.requests().len(), 1, "restart executes nothing");

    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    // The priority survived the restart: B runs ahead of the skill.
    assert_eq!(user_text(&requests[1]), "B");
    assert!(user_text(&requests[2]).contains("edited focus"));
    assert_eq!(requests[3].tools, 0, "the tail item is the Compaction");
    assert_eq!(requests[4].tools, 0, "split-turn prefix summary");
}

#[tokio::test]
async fn a_started_compaction_is_never_retried_after_restart() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text(long_conversation()),
        // The summary request hangs; the restart lands mid-Compaction.
        ScriptedReply::Silent,
        ScriptedReply::text("after restart"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    wait_drained(&engine).await;
    queue_compact(&engine, &fixture.cwd(), "m-compact").await;
    common::wait_for_requests(&provider, 2).await;
    drop(engine);

    let engine = fixture.engine(&provider);
    let state = queue_state(&engine).await;
    // The started Compaction is settled, never requeued or retried; the
    // (empty) queue restores paused as usual.
    assert_eq!(state["pending"], json!([]));
    assert_eq!(provider.requests().len(), 2);
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(!transcript.contains("compactionDivider"), "{transcript}");
    // It was never a Turn: no interrupted-Turn repair record either.
    assert!(
        !transcript.contains("Turn interrupted by restart"),
        "{transcript}"
    );

    // The History is untouched: the next Turn carries the full conversation.
    queue_run(&engine, &fixture.cwd(), "m-next", "next").await;
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_drained(&engine).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let next_text = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(
        !next_text.contains("history before this point was compacted"),
        "{next_text}"
    );
}
