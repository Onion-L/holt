//! Always-allow grants at the RPC seam (ADR-0014, issue 04): an
//! always-allow verdict executes the call and records a session grant —
//! bash by command prefix, write/edit by exact resolved path — that passes
//! matching calls through the gate without asking, holds across mode
//! switches, and disappears on restart.

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService, methods};

mod common;

async fn switch_mode(engine: &holt_engine::LocalEngine, chat_id: &str, mode: &str) {
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

/// Always-allow executes the call, records the grant, and later matching
/// calls auto-pass — prefix for bash, exact path for write/edit — while
/// anything else still pauses.
#[tokio::test]
async fn always_allow_grants_pass_matching_calls_without_asking() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "cargo test" }),
        ),
        ScriptedReply::tool_call(
            "call-2",
            "bash",
            serde_json::json!({ "command": "cargo test -- --nocapture" }),
        ),
        ScriptedReply::tool_call(
            "call-3",
            "bash",
            serde_json::json!({ "command": "cargo build" }),
        ),
        ScriptedReply::tool_call(
            "call-4",
            "write",
            serde_json::json!({ "path": "src/lib.rs", "content": "pub fn f() {}\n" }),
        ),
        ScriptedReply::tool_call(
            "call-5",
            "write",
            serde_json::json!({ "path": "src/lib.rs", "content": "pub fn g() {}\n" }),
        ),
        ScriptedReply::tool_call(
            "call-6",
            "write",
            serde_json::json!({ "path": "src/main.rs", "content": "fn main() {}\n" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "work").await;

    // call-1: always-allow `cargo test` — executes, chip settles
    // always-allowed.
    let first = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    common::resolve_approval(
        &engine,
        &first,
        serde_json::json!({ "kind": "alwaysAllow" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:alwaysAllowed").await;
    common::wait_for_requests(&provider, 2).await;
    assert!(
        common::summarize(&provider.requests()[1].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:"))
    );

    // call-2 (same prefix, longer args): auto-passed as an exemption, no
    // approval, executed.
    common::wait_for_gate(&engine, "chat-1", "call-2", "settled:exempted").await;
    common::wait_for_requests(&provider, 3).await;

    // call-3 (different command): still pauses; allow it once to move on.
    let third = common::wait_for_gate(&engine, "chat-1", "call-3", "pending").await;
    common::resolve_approval(&engine, &third, serde_json::json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-3", "settled:allowed").await;

    // call-4 (write to src/lib.rs): always-allow the exact path.
    let fourth = common::wait_for_gate(&engine, "chat-1", "call-4", "pending").await;
    common::resolve_approval(
        &engine,
        &fourth,
        serde_json::json!({ "kind": "alwaysAllow" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-1", "call-4", "settled:alwaysAllowed").await;
    assert!(fixture.project_dir.path().join("src/lib.rs").exists());

    // call-5 (the SAME file, reached relative again): exempted.
    common::wait_for_gate(&engine, "chat-1", "call-5", "settled:exempted").await;

    // call-6 (a different file): still pauses.
    let sixth = common::wait_for_gate(&engine, "chat-1", "call-6", "pending").await;
    common::resolve_approval(&engine, &sixth, serde_json::json!({ "kind": "deny" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-6", "settled:denied").await;
    assert!(!fixture.project_dir.path().join("src/main.rs").exists());
}

/// Grants are checked BEFORE the gatekeeper: they hold after the chat's
/// mode switches, but they are session-scoped — a restart starts empty.
#[tokio::test]
async fn grants_hold_across_mode_switches_and_die_on_restart() {
    let fixture = Fixture::new();
    {
        // Both Turns on ONE engine: the grant lives on the chat runtime.
        let provider = ScriptedProvider::new(vec![
            ScriptedReply::tool_call(
                "call-1",
                "bash",
                serde_json::json!({ "command": "echo hi" }),
            ),
            ScriptedReply::text("done"),
            ScriptedReply::tool_call(
                "call-2",
                "bash",
                serde_json::json!({ "command": "echo hi again" }),
            ),
            ScriptedReply::text("done too"),
        ]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        let _ = common::subscribe(&engine, "chat-1").await;
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
        let first = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
        common::resolve_approval(
            &engine,
            &first,
            serde_json::json!({ "kind": "alwaysAllow" }),
        )
        .await;
        common::wait_for_gate(&engine, "chat-1", "call-1", "settled:alwaysAllowed").await;

        // Flip modes both ways between the Turns: the grant is checked
        // before the gatekeeper, so it outlives the switch.
        switch_mode(&engine, "chat-1", "full-access").await;
        switch_mode(&engine, "chat-1", "confirm-changes").await;
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "more").await;
        common::wait_for_gate(&engine, "chat-1", "call-2", "settled:exempted").await;
    }

    // Restart: the grant is gone — the same command pauses again.
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-3",
            "bash",
            serde_json::json!({ "command": "echo hi" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "again").await;
    engine
        .handle(
            holt_rpc::methods::CONTINUE_MESSAGE_QUEUE,
            serde_json::json!({"chatId":"chat-1"}),
        )
        .await
        .unwrap();
    let third = common::wait_for_gate(&engine, "chat-1", "call-3", "pending").await;
    common::resolve_approval(&engine, &third, serde_json::json!({ "kind": "deny" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-3", "settled:denied").await;
    // The old chips replay settled — the grant itself never persisted.
    assert_eq!(
        common::gate_state(&engine, "chat-1", "call-1").await,
        "settled:alwaysAllowed"
    );
}

/// Grants are chat-scoped: one chat's always-allow never passes another
/// chat's calls.
#[tokio::test]
async fn grants_are_scoped_to_their_chat() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "bash",
            serde_json::json!({ "command": "echo shared" }),
        ),
        ScriptedReply::text("done"),
        ScriptedReply::tool_call(
            "call-2",
            "bash",
            serde_json::json!({ "command": "echo shared" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::setup_chat(&engine, "chat-2").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-2").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    let first = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    common::resolve_approval(
        &engine,
        &first,
        serde_json::json!({ "kind": "alwaysAllow" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:alwaysAllowed").await;

    // The SAME command in the other chat still pauses.
    common::run_prompt(&engine, "chat-2", &fixture.cwd(), "me too").await;
    let second = common::wait_for_gate(&engine, "chat-2", "call-2", "pending").await;
    common::resolve_approval(&engine, &second, serde_json::json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-2", "call-2", "settled:allowed").await;
}
