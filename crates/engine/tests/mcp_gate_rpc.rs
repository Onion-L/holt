//! The gate's MCP inversion at the RPC seam (ADR-0034, ticket 02): every
//! `mcp__`-prefixed call is presumed mutating, so confirm-changes pauses
//! it behind the ordinary Approval, auto-review's model pass judges it,
//! and full-access passes it through. Always-allow records the exact
//! two-level tool name — a sibling tool of the same server still asks.
//! Subagent runs never mount MCP tools at all.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService, methods};
use std::path::Path;

fn write_mcp_config(data_dir: &Path) {
    std::fs::write(
        data_dir.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "fixture": {
                    "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn echo_call(id: &str, message: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        id,
        "mcp__fixture__echo",
        serde_json::json!({ "message": message }),
    )
}

/// In confirm-changes an MCP call pauses behind the ordinary Approval;
/// allow executes it and the Turn continues.
#[tokio::test]
async fn confirm_changes_gates_mcp_calls_behind_the_approval() {
    let fixture = common::Fixture::new();
    write_mcp_config(fixture.data_dir.path());
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "gated"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo something").await;

    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    assert_eq!(
        provider.requests().len(),
        1,
        "the Turn must pause before the MCP call executes"
    );

    common::resolve_approval(
        &engine,
        &approval_id,
        serde_json::json!({ "kind": "allow" }),
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    assert!(
        common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:") && row.contains("echo: gated")),
        "the allowed MCP call's output never reached the model: {:?}",
        common::summarize(&requests[1].messages)
    );
    assert_eq!(
        common::gate_state(&engine, "chat-1", "call-1").await,
        "settled:allowed"
    );
}

/// A denial settles as an error tool result the model reads while the
/// Turn continues.
#[tokio::test]
async fn a_denied_mcp_call_settles_as_an_error_result_the_model_reads() {
    let fixture = common::Fixture::new();
    write_mcp_config(fixture.data_dir.path());
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "nope"),
        ScriptedReply::text("handled the denial"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo something").await;

    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    common::resolve_approval(
        &engine,
        &approval_id,
        serde_json::json!({ "kind": "deny", "note": "not this one" }),
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    assert!(
        common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:") && row.contains("not this one")),
        "the denial reason never reached the model: {:?}",
        common::summarize(&requests[1].messages)
    );
    // The fixture never ran: its output is nowhere in the conversation.
    assert!(
        !common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row.contains("echo: nope")),
        "the denied call must not have executed"
    );
}

/// Always-allow records the exact two-level name: the same tool stops
/// prompting, a sibling tool of the same server still asks.
#[tokio::test]
async fn always_allow_grants_the_exact_tool_and_siblings_still_ask() {
    let fixture = common::Fixture::new();
    write_mcp_config(fixture.data_dir.path());
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "first"),
        ScriptedReply::tool_call(
            "call-2",
            "mcp__fixture__echo",
            serde_json::json!({ "message": "second" }),
        ),
        ScriptedReply::tool_call(
            "call-3",
            "mcp__fixture__fail",
            serde_json::json!({ "message": "sibling" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo twice then fail").await;

    // First call prompts; always-allow releases it.
    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-1", "pending").await;
    common::resolve_approval(
        &engine,
        &approval_id,
        serde_json::json!({ "kind": "alwaysAllow" }),
    )
    .await;
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:alwaysAllowed").await;

    // The same tool's second call passes without a new gate…
    common::wait_for_gate(&engine, "chat-1", "call-2", "settled:exempted").await;

    // …but the sibling tool of the same server prompts again.
    let approval_id = common::wait_for_gate(&engine, "chat-1", "call-3", "pending").await;
    common::resolve_approval(
        &engine,
        &approval_id,
        serde_json::json!({ "kind": "allow" }),
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let last = common::summarize(&requests.last().unwrap().messages);
    assert!(
        last.iter()
            .any(|row| row.starts_with("toolresult:call-3:")
                && row.contains("fixture error: sibling")),
        "the allowed sibling's output never reached the model: {last:?}"
    );
}

/// In full-access MCP calls execute without pausing.
#[tokio::test]
async fn full_access_runs_mcp_calls_without_pausing() {
    let fixture = common::Fixture::new();
    write_mcp_config(fixture.data_dir.path());
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "free"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo something").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // No gate ever opened on the MCP call.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    for entry in snapshot["reset"].as_array().unwrap() {
        for part in entry["parts"].as_array().unwrap() {
            assert!(
                part["gate"].is_null(),
                "full-access must not gate MCP calls: {snapshot}"
            );
        }
    }
    let requests = provider.requests();
    assert!(
        common::summarize(&requests[1].messages)
            .iter()
            .any(|row| row.starts_with("toolresult:call-1:") && row.contains("echo: free")),
        "the call's output never reached the model"
    );
}

/// In auto-review the reviewer model judges MCP calls like any mutating
/// call: a pass executes, a rejection blocks with the reason.
#[tokio::test]
async fn auto_review_judges_mcp_calls_like_any_mutating_call() {
    let fixture = common::Fixture::new();
    write_mcp_config(fixture.data_dir.path());
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "reviewed"),
        ScriptedReply::text("APPROVE"),
        ScriptedReply::tool_call(
            "call-2",
            "mcp__fixture__echo",
            serde_json::json!({ "message": "blocked" }),
        ),
        // The second review pass rejects with a reason.
        ScriptedReply::text("REJECT: too risky for this chat"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": "chat-1",
                "mode": "auto-review",
            }),
        )
        .await
        .unwrap();
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo twice").await;

    // A pass executes the call.
    common::wait_for_gate(&engine, "chat-1", "call-1", "settled:reviewPassed").await;
    // A rejection blocks it with the reviewer's reason.
    common::wait_for_gate(&engine, "chat-1", "call-2", "settled:reviewRejected").await;
    common::wait_for_requests(&provider, 5).await;

    let requests = provider.requests();
    let last = common::summarize(&requests.last().unwrap().messages);
    assert!(
        last.iter()
            .any(|row| row.starts_with("toolresult:call-1:") && row.contains("echo: reviewed")),
        "the approved call's output never reached the model: {last:?}"
    );
    assert!(
        last.iter()
            .any(|row| row.starts_with("toolresult:call-2:") && row.contains("too risky")),
        "the reviewer's reason never reached the model: {last:?}"
    );
    assert!(
        !last.iter().any(|row| row.contains("echo: blocked")),
        "the rejected call must not have executed"
    );
}

/// Subagent runs never mount MCP tools — explorer and worker children
/// keep their curated sets.
#[tokio::test]
async fn subagent_runs_never_mount_mcp_tools() {
    let fixture = common::Fixture::new();
    write_mcp_config(fixture.data_dir.path());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            serde_json::json!({
                "subagent_type": "explorer",
                "description": "Look around",
                "prompt": "Inspect the project"
            }),
        ),
        // The child's own reply — its request lands between the parent's.
        ScriptedReply::text("child done"),
        ScriptedReply::text("parent done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let _ = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "delegate then finish").await;

    common::wait_for_requests(&provider, 3).await;
    let requests = provider.requests();
    // The parent's request carries the MCP tool; the child's (the middle
    // request, the explorer's own system prompt) carries none.
    assert!(
        requests[0]
            .tool_names
            .contains(&"mcp__fixture__echo".to_string()),
        "parent toolset: {:?}",
        requests[0].tool_names
    );
    let child = &requests[1];
    assert!(
        child.tool_names.iter().any(|name| name == "read"),
        "child toolset: {:?}",
        child.tool_names
    );
    assert!(
        !child
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "subagents must not mount MCP tools: {:?}",
        child.tool_names
    );
}
