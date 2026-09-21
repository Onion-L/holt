//! The MCP Settings quartet at the RPC seam (ADR-0034, ticket 07):
//! `GetMcpSettings` returns every definition plus any file-level
//! validation error, `SaveMcpServer` strictly validates and upserts (the
//! `[A-Za-z0-9_-]` name rule, atomic 0600 write, next-Turn effect),
//! `RemoveMcpServer` deletes, and `TestMcpServer` probes both transports
//! on demand — no standing watch.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use std::path::Path;

async fn call(
    engine: &holt_engine::LocalEngine,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    match engine.handle(method, params).await.unwrap() {
        RpcReply::Value(value) => value,
        RpcReply::Stream(_) => panic!("{method} replied with a stream"),
    }
}

async fn call_err(
    engine: &holt_engine::LocalEngine,
    method: &str,
    params: serde_json::Value,
) -> String {
    match engine.handle(method, params).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("{method} unexpectedly succeeded"),
    }
}

#[tokio::test]
async fn get_upsert_remove_round_trip() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);

    // Empty state: no servers, no validation error.
    let state = call(&engine, methods::GET_MCP_SETTINGS, serde_json::json!({})).await;
    assert_eq!(state["servers"], serde_json::json!([]));
    assert!(state["validationError"].is_null());

    // Upsert a stdio definition.
    let state = call(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "name": "fixture",
            "server": {
                "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
                "enabledTools": ["echo"],
            }
        }),
    )
    .await;
    assert_eq!(state["servers"].as_array().unwrap().len(), 1);
    assert_eq!(state["servers"][0]["name"], "fixture");
    assert_eq!(
        state["servers"][0]["command"],
        env!("CARGO_BIN_EXE_mcp_stdio_fixture")
    );

    // The write persisted under the credentials pattern.
    let path = fixture.data_dir.path().join("mcp.json");
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("\"fixture\""), "{persisted}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // The saved definition serves the next Turn.
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let toolset = provider.requests()[0].tool_names.clone();
    assert!(
        toolset.contains(&"mcp__fixture__echo".to_string()),
        "{toolset:?}"
    );
    assert!(
        !toolset.contains(&"mcp__fixture__fail".to_string()),
        "{toolset:?}"
    );

    // Remove deletes by name; an unknown name refuses.
    let state = call(
        &engine,
        methods::REMOVE_MCP_SERVER,
        serde_json::json!({ "name": "fixture" }),
    )
    .await;
    assert_eq!(state["servers"], serde_json::json!([]));
    let error = call_err(
        &engine,
        methods::REMOVE_MCP_SERVER,
        serde_json::json!({ "name": "fixture" }),
    )
    .await;
    assert!(error.contains("unknown"), "{error}");
}

#[tokio::test]
async fn upsert_rejects_illegal_names_and_strict_definitions() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);

    for name in ["my server", "a/b", "名前"] {
        let error = call_err(
            &engine,
            methods::SAVE_MCP_SERVER,
            serde_json::json!({ "name": name, "server": { "command": "x" } }),
        )
        .await;
        assert!(error.contains("[A-Za-z0-9_-]"), "{name:?}: {error}");
    }
    // An empty name is refused before the shape ever validates.
    assert!(
        call_err(
            &engine,
            methods::SAVE_MCP_SERVER,
            serde_json::json!({ "name": "", "server": { "command": "x" } }),
        )
        .await
        .contains("required")
    );
    // The definition survives the same strict parse a hand edit would.
    let error = call_err(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({ "name": "ok", "server": { "command": "x", "ur1": 1 } }),
    )
    .await;
    assert!(error.contains("unknown field"), "{error}");
    let error = call_err(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({ "name": "ok", "server": { "enabled": true } }),
    )
    .await;
    assert!(error.contains("either"), "{error}");
    // Nothing landed.
    let state = call(&engine, methods::GET_MCP_SETTINGS, serde_json::json!({})).await;
    assert_eq!(state["servers"], serde_json::json!([]));
}

#[tokio::test]
async fn get_surfaces_file_level_validation_errors() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    // A hand edit breaks the file after startup.
    std::fs::write(fixture.data_dir.path().join("mcp.json"), b"{broken").unwrap();

    let state = call(&engine, methods::GET_MCP_SETTINGS, serde_json::json!({})).await;
    let error = state["validationError"].as_str().unwrap();
    assert!(error.contains("mcp.json"), "{error}");
    // The last-good set (empty here) keeps serving — no crash, no wipe.
    assert_eq!(state["servers"], serde_json::json!([]));
}

#[tokio::test]
async fn the_probe_reports_status_tool_count_and_failures() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    call(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "name": "fixture",
            "server": { "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture") }
        }),
    )
    .await;
    call(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "name": "broken",
            "server": { "command": "definitely-not-a-real-command-xyz" }
        }),
    )
    .await;

    // A healthy stdio server probes ok with its tool list.
    let probe = call(
        &engine,
        methods::TEST_MCP_SERVER,
        serde_json::json!({ "name": "fixture" }),
    )
    .await;
    assert_eq!(probe["status"], "ok");
    assert_eq!(probe["toolCount"], 2);
    let names = probe["toolNames"].as_array().unwrap();
    assert!(names.contains(&serde_json::json!("echo")));
    assert!(names.contains(&serde_json::json!("fail")));

    // A broken one reports the failure reason.
    let probe = call(
        &engine,
        methods::TEST_MCP_SERVER,
        serde_json::json!({ "name": "broken" }),
    )
    .await;
    assert_eq!(probe["status"], "failed");
    assert!(
        probe["reason"].as_str().unwrap().contains("spawn"),
        "{probe}"
    );

    // So does an unknown one.
    let probe = call(
        &engine,
        methods::TEST_MCP_SERVER,
        serde_json::json!({ "name": "ghost" }),
    )
    .await;
    assert_eq!(probe["status"], "failed");
    assert!(probe["reason"].as_str().unwrap().contains("unknown"));
}

#[tokio::test]
async fn the_probe_serves_http_servers_too() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    // An unreachable http entry: the probe reports the connection failure
    // instead of hanging.
    let probe_port = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    };
    call(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "name": "remote",
            "server": { "url": format!("http://127.0.0.1:{probe_port}/mcp") }
        }),
    )
    .await;
    let probe = call(
        &engine,
        methods::TEST_MCP_SERVER,
        serde_json::json!({ "name": "remote" }),
    )
    .await;
    assert_eq!(probe["status"], "failed");
    assert!(
        probe["reason"].as_str().unwrap().contains("initialize"),
        "{probe}"
    );
}

#[tokio::test]
async fn an_upsert_invalidates_the_cached_connection() {
    let fixture = common::Fixture::new();
    let provider =
        ScriptedProvider::new(vec![ScriptedReply::text("one"), ScriptedReply::text("two")]);
    let engine = fixture.engine(&provider);
    // First definition: a broken command.
    call(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "name": "fixture",
            "server": { "command": "definitely-not-a-real-command-xyz" }
        }),
    )
    .await;
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn one").await;
    common::wait_for_transcript_text(&mut transcript, "one").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__"))
    );

    // Fix the definition through the upsert: the next Turn serves it
    // without an app restart.
    call(
        &engine,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "name": "fixture",
            "server": { "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture") }
        }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "turn two").await;
    common::wait_for_transcript_text(&mut transcript, "two").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider
            .requests()
            .last()
            .unwrap()
            .tool_names
            .contains(&"mcp__fixture__echo".to_string())
    );
}

#[tokio::test]
async fn hand_edits_land_after_restart_and_live() {
    // A file written by hand (the ticket-01 path) reads back through Get.
    let fixture = common::Fixture::new();
    std::fs::write(
        fixture.data_dir.path().join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "hand": {
                    "command": "npx",
                    "args": ["-y", "server"],
                    "disabledTools": ["noisy"],
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    let state = call(&engine, methods::GET_MCP_SETTINGS, serde_json::json!({})).await;
    let server = &state["servers"][0];
    assert_eq!(server["name"], "hand");
    assert_eq!(server["command"], "npx");
    assert_eq!(server["disabledTools"], serde_json::json!(["noisy"]));
    assert_eq!(server["enabled"], serde_json::json!(true));
    let _ = Path::new("");
}
