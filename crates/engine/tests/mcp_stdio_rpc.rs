//! Whole-Turn MCP tests (ADR-0034, ticket 01 — the stdio tracer bullet):
//! a real engine assembled on a temp data dir whose `mcp.json` points at
//! the in-repo stdio fixture binary, driven through the `RpcService`
//! trait exactly as the UI drives it. The scripted provider's recorded
//! tool names are the agent-tool surface; the fed-back tool results and
//! transcript frames are the transcript landing.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::RpcService;
use std::path::Path;

fn assemble_unwrapped(
    fixture: &common::Fixture,
    provider: &ScriptedProvider,
) -> Result<LocalEngine, holt_engine::EngineError> {
    LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: None,
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
}

fn write_mcp_config(data_dir: &Path, servers: serde_json::Value) {
    std::fs::write(
        data_dir.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": servers })).unwrap(),
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

/// A marker file path the fixture touches when it starts — the spawn
/// detector for lazy-start and never-started assertions.
fn marker_path(data_dir: &Path) -> String {
    data_dir.join("fixture-started").display().to_string()
}

#[tokio::test]
async fn a_stdio_servers_tools_join_the_toolset_and_run_in_a_turn() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({ "fixture": fixture_server(serde_json::json!({})) }),
    );
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "mcp__fixture__echo",
            serde_json::json!({ "message": "hi there" }),
        ),
        ScriptedReply::text("done echoing"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "use the echo tool").await;
    common::wait_for_transcript_text(&mut transcript, "done echoing").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The Turn's request carried the two-level tool name alongside the
    // built-ins — the agent-tool surface.
    let requests = provider.requests();
    assert!(
        requests[0]
            .tool_names
            .contains(&"mcp__fixture__echo".to_string()),
        "toolset: {:?}",
        requests[0].tool_names
    );
    // The call forwarded to tools/call and the text result fed back to
    // the model as the tool result for call-1.
    let summary = common::summarize(&requests[1].messages);
    assert!(
        summary.iter().any(
            |entry| entry.starts_with("toolresult:call-1:") && entry.contains("echo: hi there")
        ),
        "results: {summary:?}"
    );
    // The transcript folds the call onto the structured Mcp chip
    // (server + tool), not the raw two-level name.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let snapshot = snapshot.to_string();
    assert!(snapshot.contains("\"kind\":\"mcp\""), "{snapshot}");
    assert!(snapshot.contains("\"server\":\"fixture\""), "{snapshot}");
    assert!(snapshot.contains("\"tool\":\"echo\""), "{snapshot}");
}

#[tokio::test]
async fn a_disabled_server_never_connects() {
    let fixture = common::Fixture::new();
    let marker = marker_path(fixture.data_dir.path());
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "parked": fixture_server(serde_json::json!({
                "enabled": false,
                "env": { "FIXTURE_MARKER": marker }
            }))
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("plain reply")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "plain reply").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    assert!(
        !requests[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "toolset: {:?}",
        requests[0].tool_names
    );
    // Never spawned: the marker the fixture touches on start is absent.
    assert!(!Path::new(&marker).exists());
}

#[tokio::test]
async fn connections_start_lazily_and_only_when_a_turn_needs_tools() {
    let fixture = common::Fixture::new();
    let marker = marker_path(fixture.data_dir.path());
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "fixture": fixture_server(serde_json::json!({
                "env": { "FIXTURE_MARKER": marker }
            }))
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("plain reply")]);
    let engine = fixture.engine(&provider);

    // Assembling the engine spawns nothing — the marker is still absent.
    assert!(!Path::new(&marker).exists());

    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "plain reply").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The Turn started the connection even though the model never called
    // an MCP tool — the toolset snapshot is what connects.
    assert!(Path::new(&marker).exists());
}

#[tokio::test]
async fn a_broken_mcp_config_fails_engine_assembly() {
    let fixture = common::Fixture::new();
    std::fs::write(fixture.data_dir.path().join("mcp.json"), b"{broken").unwrap();
    let provider = ScriptedProvider::new(vec![]);
    let error = match assemble_unwrapped(&fixture, &provider) {
        Err(error) => error,
        Ok(_) => panic!("a malformed mcp.json must fail engine assembly"),
    };
    assert!(error.to_string().contains("mcp config"), "{error}");
    // The malformed file is left in place for manual repair.
    assert_eq!(
        std::fs::read(fixture.data_dir.path().join("mcp.json")).unwrap(),
        b"{broken"
    );
}

#[tokio::test]
async fn an_unknown_config_key_fails_engine_assembly() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "typo": fixture_server(serde_json::json!({ "commandd": "x" }))
        }),
    );
    let provider = ScriptedProvider::new(vec![]);
    let error = match assemble_unwrapped(&fixture, &provider) {
        Err(error) => error,
        Ok(_) => panic!("a malformed mcp.json must fail engine assembly"),
    };
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[tokio::test]
async fn a_server_that_cannot_start_is_skipped_and_the_turn_proceeds() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "broken": { "command": "definitely-not-a-real-command-xyz" },
            "fixture": fixture_server(serde_json::json!({})),
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("kept going")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "kept going").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The healthy server still mounted; the broken one is simply absent.
    let requests = provider.requests();
    assert!(
        requests[0]
            .tool_names
            .contains(&"mcp__fixture__echo".to_string()),
        "toolset: {:?}",
        requests[0].tool_names
    );
    assert!(
        !requests[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__broken__")),
        "toolset: {:?}",
        requests[0].tool_names
    );
}

/// A planning Turn never queries the pool — its read-only toolset would
/// drop the tools anyway, so no MCP child may spawn for it.
#[tokio::test]
async fn planning_turns_spawn_no_mcp_children() {
    let fixture = common::Fixture::new();
    let marker = marker_path(fixture.data_dir.path());
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "fixture": fixture_server(serde_json::json!({
                "env": { "FIXTURE_MARKER": marker }
            }))
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("planned")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    engine
        .handle(
            holt_rpc::methods::ENTER_PLAN_MODE,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "plan something").await;
    common::wait_for_transcript_text(&mut transcript, "planned").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The planning Turn's request carried no MCP tools…
    let requests = provider.requests();
    assert!(
        !requests[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "toolset: {:?}",
        requests[0].tool_names
    );
    // …and nothing ever spawned — the marker the fixture touches on start
    // is absent.
    assert!(!Path::new(&marker).exists());
}
