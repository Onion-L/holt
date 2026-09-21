//! MCP config field completeness at the RPC seam (ADR-0034, ticket 06):
//! `enabledTools`/`disabledTools` filter the mounted set (deny wins), a
//! stdio child runs in its own `cwd` — never the chat's — config values
//! expand `${VAR}`/`${VAR:-default}` at run time with unset-without-default
//! kept literal, and the child's inherited environment carries no
//! credential-shaped variables unless the server's own `env` grants them.

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

async fn run_plain_turn(engine: &holt_engine::LocalEngine, fixture: &common::Fixture) {
    common::run_prompt(engine, "chat-1", &fixture.cwd(), "hello").await;
}

/// An allowlist mounts only its entries.
#[tokio::test]
async fn enabled_tools_filters_the_mounted_set() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({ "enabledTools": ["echo"] })),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let toolset = &provider.requests()[0].tool_names;
    assert!(toolset.contains(&"mcp__fixture__echo".to_string()));
    assert!(
        !toolset.contains(&"mcp__fixture__fail".to_string()),
        "{toolset:?}"
    );
}

/// The denylist wins when both lists mention a tool.
#[tokio::test]
async fn disabled_tools_wins_over_enabled_tools() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "enabledTools": ["echo", "fail"],
            "disabledTools": ["fail"],
        })),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let toolset = &provider.requests()[0].tool_names;
    assert!(toolset.contains(&"mcp__fixture__echo".to_string()));
    assert!(
        !toolset.contains(&"mcp__fixture__fail".to_string()),
        "{toolset:?}"
    );
}

/// A configured cwd runs the child there; with none it runs in Holt's own
/// process cwd — never the chat's working directory.
#[tokio::test]
async fn the_child_runs_in_the_configured_cwd_or_holts_own() {
    let dir = tempfile::TempDir::new().unwrap();
    let configured = dir.path().join("server-home");
    std::fs::create_dir_all(&configured).unwrap();

    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "cwd": configured.display().to_string(),
            "env": { "FIXTURE_WRITE_CWD": fixture.data_dir.path().join("cwd-1").display().to_string() },
        })),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let seen = std::fs::read_to_string(fixture.data_dir.path().join("cwd-1")).unwrap();
    assert_eq!(
        seen,
        configured.canonicalize().unwrap().display().to_string(),
        "the child must run in the configured cwd"
    );
    assert_ne!(
        seen,
        fixture.cwd(),
        "the child must never run in the chat's cwd"
    );

    // With no cwd at all, the default is Holt's process cwd — still not
    // the chat's.
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": { "FIXTURE_WRITE_CWD": fixture.data_dir.path().join("cwd-2").display().to_string() },
        })),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let seen = std::fs::read_to_string(fixture.data_dir.path().join("cwd-2")).unwrap();
    assert_eq!(
        seen,
        std::env::current_dir().unwrap().display().to_string(),
        "the default cwd is holt's process cwd"
    );
    assert_ne!(seen, fixture.cwd());
}

/// `${VAR}` and `${VAR:-default}` expand at run time in command, args,
/// and env values; an unset variable without a default stays literal —
/// visibly breaking the launch rather than silently emptying it.
#[tokio::test]
async fn config_values_expand_at_run_time() {
    unsafe {
        std::env::set_var("HOLT_MCP_BIN", env!("CARGO_BIN_EXE_mcp_stdio_fixture"));
        std::env::remove_var("HOLT_MCP_UNSET_VAR");
    }
    let marker = fixture_marker("expand");
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "command": "${HOLT_MCP_BIN}",
            "args": ["${HOLT_MCP_UNSET_ARG:-unused}"],
            "env": {
                // A default fills in for an unset variable…
                "FIXTURE_MARKER": format!("${{HOLT_MCP_UNSET_VAR:-{}}}", marker.display()),
            },
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The expanded command spawned, and the defaulted marker env reached
    // the child — both expansions worked.
    assert!(marker.exists(), "the ${{VAR:-default}} env never expanded");
    assert!(
        provider.requests()[0]
            .tool_names
            .contains(&"mcp__fixture__echo".to_string()),
        "the ${{VAR}} command never expanded"
    );

    // An unset variable without a default keeps its literal: the command
    // stays unrunnable and the server is simply absent — visible, not
    // silently empty.
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "command": "definitely-${HOLT_MCP_UNSET_VAR}-shaped",
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("kept going")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "kept going").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "a literal ${{VAR}} command must not run"
    );
}

/// The stdio child inherits a sanitized environment — credential-shaped
/// variables stripped unless the server's own env sets them explicitly.
#[tokio::test]
async fn the_child_environment_is_sanitized_with_an_explicit_opt_in() {
    let dump = std::env::temp_dir().join(format!("holt-mcp-env-{}", std::process::id()));
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        fixture_server(serde_json::json!({
            "env": {
                "FIXTURE_ENV_DUMP": dump.display().to_string(),
                // The explicit opt-in: a credential-shaped variable the
                // server asked for by name.
                "HOLT_GRANTED_TOKEN": "granted-value",
            },
        })),
    );
    // A credential-shaped variable in holt's own environment must not
    // reach the child.
    unsafe {
        std::env::set_var("HOLT_SECRET_TO_STRIP", "must-not-leak");
        std::env::set_var("HOLT_PLAIN_VAR", "fine-to-see");
    }
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_plain_turn(&engine, &fixture).await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    unsafe {
        std::env::remove_var("HOLT_SECRET_TO_STRIP");
        std::env::remove_var("HOLT_PLAIN_VAR");
    }

    let dumped = std::fs::read_to_string(&dump).unwrap();
    assert!(
        !dumped.contains("must-not-leak"),
        "credential leaked:\n{dumped}"
    );
    assert!(dumped.contains("HOLT_PLAIN_VAR=fine-to-see"));
    assert!(dumped.contains("HOLT_GRANTED_TOKEN=granted-value"));
    let _ = std::fs::remove_file(&dump);
}

fn fixture_marker(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("holt-mcp-marker-{tag}-{}", std::process::id()))
}
