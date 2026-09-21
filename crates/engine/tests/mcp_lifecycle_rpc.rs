//! MCP lifecycle and failure semantics at the RPC seam (ADR-0034, ticket
//! 03): a server that cannot start or connect within its startup timeout
//! is skipped while the Turn proceeds; a dead connection's calls settle as
//! error tool results the model reads; the next Turn retries lazily and a
//! restarted server reconnects without an app restart; tool lists are
//! snapshotted at Turn start with `listChanged` landing on the next Turn;
//! the per-call timeout aborts and settles an error result.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use std::path::Path;

fn write_mcp_config(data_dir: &Path, server: serde_json::Value) {
    std::fs::write(
        data_dir.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": { "fixture": server } }))
            .unwrap(),
    )
    .unwrap();
}

fn fixture_server(extra: serde_json::Value) -> serde_json::Value {
    let mut server = serde_json::json!({
        "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
    });
    let object = server.as_object_mut().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        object.insert(key.clone(), value.clone());
    }
    server
}

fn echo_call(id: &str, message: &str) -> ScriptedReply {
    ScriptedReply::tool_call(
        id,
        "mcp__fixture__echo",
        serde_json::json!({ "message": message }),
    )
}

/// A server that hangs its initialize handshake is skipped within its
/// startup timeout while the Turn proceeds on the built-ins alone.
#[tokio::test]
async fn a_hung_startup_is_skipped_within_its_timeout() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": { "FIXTURE_STALL_INITIALIZE": "1" },
            "startupTimeoutMs": 400,
        })),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("kept going")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    let started = std::time::Instant::now();
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;

    common::wait_for_transcript_text(&mut transcript, "kept going").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    // The Turn waited out the startup timeout and no longer.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "the hung startup stalled the Turn for {}s",
        started.elapsed().as_secs()
    );
    let requests = provider.requests();
    assert!(
        !requests[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "the hung server's tools must be absent: {:?}",
        requests[0].tool_names
    );
}

/// A per-call timeout aborts the call and settles an error result the
/// model reads.
#[tokio::test]
async fn a_slow_call_times_out_and_settles_an_error_result() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": { "FIXTURE_SLOW_CALL_MS": "5000" },
            "toolTimeoutMs": 400,
        })),
    );
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "slow"),
        ScriptedReply::text("moved on"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo slowly").await;

    common::wait_for_transcript_text(&mut transcript, "moved on").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let last = common::summarize(&requests.last().unwrap().messages);
    assert!(
        last.iter().any(|row| row.starts_with("toolresult:call-1:")
            && row.contains("exceeded its call timeout")),
        "the timeout error never reached the model: {last:?}"
    );
}

/// A connection dying mid-Turn leaves subsequent calls settling as error
/// tool results — not hangs — and the Turn finishes.
#[tokio::test]
async fn a_mid_turn_death_settles_calls_as_error_results() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": { "FIXTURE_EXIT_ON_CALL": "1" },
        })),
    );
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "dies here"),
        echo_call("call-2", "already dead"),
        ScriptedReply::text("survived the crash"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "echo twice").await;

    common::wait_for_transcript_text(&mut transcript, "survived the crash").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let last = common::summarize(&requests.last().unwrap().messages);
    // The first call's reply landed before the child died — an ordinary
    // result. The second call hit the dead transport and settled as an
    // error result the model reads (not a hang).
    assert!(
        last.iter()
            .any(|row| row.starts_with("toolresult:call-1:") && row.contains("echo: dies here")),
        "the pre-death call must have answered: {last:?}"
    );
    assert!(
        last.iter()
            .any(|row| row.starts_with("toolresult:call-2:") && row.contains("mcp server error")),
        "the post-death call must settle as an error result: {last:?}"
    );
}

/// A server that died is retried the same lazy way on a later Turn: the
/// Turn after the death reconnects (a fresh child), and its calls work
/// again — without an app restart.
#[tokio::test]
async fn a_restarted_server_reconnects_on_a_later_turn() {
    let fixture = common::Fixture::new();
    // The child exits on its second call: Turn 1 works, the death happens
    // inside Turn 2, and Turn 3 reconnects.
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": { "FIXTURE_EXIT_ON_CALL": "2" },
        })),
    );
    let provider = ScriptedProvider::new(vec![
        echo_call("call-1", "first turn"),
        ScriptedReply::text("turn one done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn one").await;
    common::wait_for_transcript_text(&mut transcript, "turn one done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Turn 2: the second call kills the child mid-Turn.
    provider.push(echo_call("call-2", "dies now"));
    provider.push(ScriptedReply::text("turn two done"));
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn two").await;
    common::wait_for_transcript_text(&mut transcript, "turn two done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Turn 3: a fresh connection serves the call again.
    provider.push(echo_call("call-3", "back again"));
    provider.push(ScriptedReply::text("turn three done"));
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn three").await;
    common::wait_for_transcript_text(&mut transcript, "turn three done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let last = common::summarize(&requests.last().unwrap().messages);
    assert!(
        last.iter()
            .any(|row| row.starts_with("toolresult:call-3:") && row.contains("echo: back again")),
        "the reconnected server must answer again: {last:?}"
    );
}

/// Tool lists are snapshotted at Turn start: a `listChanged` notification
/// (or any mid-Turn change) lands on the next Turn, never inside the
/// running one.
#[tokio::test]
async fn list_changed_lands_on_the_next_turn() {
    let fixture = common::Fixture::new();
    // The first list answers with only `echo`, then notifies listChanged;
    // every later list answers with the full set.
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": {
                "FIXTURE_ONE_TOOL_FIRST_LIST": "1",
                "FIXTURE_LIST_CHANGED_AFTER": "1",
            }
        })),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("turn one done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn one").await;
    common::wait_for_transcript_text(&mut transcript, "turn one done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Turn 1 saw only `echo`.
    let first = provider.requests()[0].tool_names.clone();
    assert!(first.contains(&"mcp__fixture__echo".to_string()));
    assert!(!first.contains(&"mcp__fixture__fail".to_string()));

    // Turn 2 — after the notification — sees the full list.
    provider.push(ScriptedReply::text("turn two done"));
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn two").await;
    common::wait_for_transcript_text(&mut transcript, "turn two done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let second = provider.requests().last().unwrap().tool_names.clone();
    assert!(second.contains(&"mcp__fixture__echo".to_string()));
    assert!(second.contains(&"mcp__fixture__fail".to_string()));
}
