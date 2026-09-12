//! Always-allow grants at the RPC seam (ADR-0014, issue 04): an
//! always-allow verdict executes the call and records a session grant —
//! bash by command prefix, write/edit by exact resolved path — that passes
//! matching calls through the gate without asking, holds across mode
//! switches, and disappears on restart.

use std::sync::Arc;

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

/// Grants are chat-scoped under concurrent Turns: both chats run at once,
/// each gate carries its own approval id, and one chat's verdict or grant
/// never reaches into the other chat. The fixture holds chat-1's Turn open
/// past its own resolution across real elapsed time — the same window a
/// slow bash preparation (the login-shell PATH probe) produces — so chat-2's
/// whole approval flow runs while chat-1 is mid-Turn.
#[tokio::test]
async fn grants_are_scoped_to_their_chat() {
    let fixture = Fixture::new();
    // Holds chat-1's first-Turn reply: its gate has resolved and the tool
    // has run, but the Turn stays open until the test releases the reply.
    let hold = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(Vec::new())
        .with_chat_script(
            "go",
            vec![
                ScriptedReply::tool_call(
                    "call-1",
                    "bash",
                    serde_json::json!({ "command": "echo one" }),
                ),
                ScriptedReply::gated(hold.clone(), "done"),
            ],
        )
        .with_chat_script(
            "me too",
            vec![
                ScriptedReply::tool_call(
                    "call-2",
                    "bash",
                    serde_json::json!({ "command": "echo two" }),
                ),
                ScriptedReply::text("done"),
            ],
        )
        .with_chat_script(
            "go again",
            vec![
                // The command CHAT-2 always-allowed: chat-1 must still pause.
                ScriptedReply::tool_call(
                    "call-3",
                    "bash",
                    serde_json::json!({ "command": "echo two" }),
                ),
                ScriptedReply::text("done"),
            ],
        )
        .with_chat_script(
            "me too again",
            vec![
                // The command CHAT-1 always-allowed: chat-2 must still pause.
                ScriptedReply::tool_call(
                    "call-4",
                    "bash",
                    serde_json::json!({ "command": "echo one" }),
                ),
                ScriptedReply::text("done"),
            ],
        );
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::setup_chat(&engine, "chat-2").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-2").await;

    // Both Turns run concurrently: chat-2 queues while chat-1's gate is
    // still open.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::run_prompt(&engine, "chat-2", &fixture.cwd(), "me too").await;
    let first = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    let second = common::wait_for_gate(&engine, "chat-2", "call-2", "pending").await;
    assert_ne!(
        first, second,
        "each chat's gate carries its own approval id"
    );

    // chat-1 always-allows `echo one`: the grant belongs to chat-1 alone.
    common::resolve_approval(
        &engine,
        &first,
        serde_json::json!({ "kind": "alwaysAllow" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:alwaysAllowed").await;

    // The held reply keeps chat-1 mid-Turn. Its approval id is spent:
    // replaying it is rejected and must not release chat-2's gate.
    let replay = engine
        .handle(
            methods::RESOLVE_APPROVAL,
            serde_json::json!({ "approvalId": first, "verdict": { "kind": "allow" } }),
        )
        .await;
    assert!(replay.is_err(), "a settled approval must not resolve again");
    assert_eq!(
        common::gate_state(&engine, "chat-2", "call-2").await,
        "pending"
    );

    // chat-2 resolves on its own approval while chat-1 is still mid-Turn —
    // and its verdict leaves chat-1's settled chip untouched.
    common::resolve_approval(
        &engine,
        &second,
        serde_json::json!({ "kind": "alwaysAllow" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-2", "call-2", "settled:alwaysAllowed").await;
    assert_eq!(
        common::gate_state(&engine, "chat-1", "call-1").await,
        "settled:alwaysAllowed"
    );

    // Release chat-1's first Turn and run both second Turns.
    hold.notify_one();
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go again").await;
    common::run_prompt(&engine, "chat-2", &fixture.cwd(), "me too again").await;

    // Grants never crossed chats: each chat's second call runs the OTHER
    // chat's always-allowed command, and both still pause.
    let third = common::wait_for_gate(&engine, "chat-1", "call-3", "pending").await;
    let fourth = common::wait_for_gate(&engine, "chat-2", "call-4", "pending").await;
    assert_ne!(third, fourth);
    common::resolve_approval(&engine, &third, serde_json::json!({ "kind": "allow" })).await;
    common::resolve_approval(&engine, &fourth, serde_json::json!({ "kind": "allow" })).await;
    common::wait_for_gate(&engine, "chat-1", "call-3", "settled:allowed").await;
    common::wait_for_gate(&engine, "chat-2", "call-4", "settled:allowed").await;
}
