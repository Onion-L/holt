//! MCP resource ceilings at the RPC seam (ADR-0034, ticket 05): a verbose
//! or hostile server cannot flood the model's context — descriptions
//! truncate at 2 KB, results cap at 100k characters with in-band markers,
//! non-text content drops with a notice, `tools/list` pagination is
//! followed completely, and a server listing an illegal or overlong tool
//! name is refused at connect rather than truncated.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use std::path::Path;

fn write_mcp_config(data_dir: &Path, env: serde_json::Value) {
    std::fs::write(
        data_dir.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "fixture": {
                    "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
                    "env": env,
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn mcp_call(id: &str, tool: &str) -> ScriptedReply {
    ScriptedReply::tool_call(id, tool, serde_json::json!({}))
}

#[tokio::test]
async fn verbose_descriptions_truncate_before_reaching_the_model() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({ "FIXTURE_VERBOSE_DESCRIPTION": "1" }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let echo = provider.requests()[0]
        .tool_names
        .iter()
        .position(|name| name == "mcp__fixture__echo")
        .expect("the echo tool mounted");
    let description = &provider.requests()[0].tool_descriptions[echo];
    assert!(
        description.len() <= 2 * 1024 + 64,
        "a 3 KB description reached the model at {} bytes",
        description.len()
    );
    assert!(
        description.contains("description truncated at 2048 bytes"),
        "marker missing: {description:?}"
    );
}

#[tokio::test]
async fn oversized_results_cap_with_an_in_band_marker() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({ "FIXTURE_BIG_RESULT": "1" }),
    );
    let provider = ScriptedProvider::new(vec![
        mcp_call("call-1", "mcp__fixture__big"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "go").await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    let last = common::summarize(&requests.last().unwrap().messages);
    let result = last
        .iter()
        .find(|row| row.starts_with("toolresult:call-1:"))
        .expect("the big call settled");
    // The 120k-character body plus tail block is capped at 100k with the
    // marker, and the image block dropped with its notice.
    assert!(
        result.contains("result truncated at 100000 characters"),
        "{result:.160}"
    );
    assert!(
        !result.contains("tail block"),
        "post-cap text must not ride along"
    );
    assert!(
        result.contains("[non-text content blocks were dropped]"),
        "the dropped image needs its in-band notice: {result:.160}"
    );
    let text = result.trim_start_matches("toolresult:call-1:");
    assert!(
        text.chars().count() < 101_000,
        "the cap leaked: {}",
        text.chars().count()
    );
}

#[tokio::test]
async fn paginated_listings_are_followed_to_completion() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "FIXTURE_PAGINATE": "1",
            "FIXTURE_BIG_RESULT": "1",
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // One tool per page: every page's tool made it into the toolset.
    let toolset = &provider.requests()[0].tool_names;
    for tool in [
        "mcp__fixture__echo",
        "mcp__fixture__fail",
        "mcp__fixture__big",
    ] {
        assert!(
            toolset.contains(&tool.to_string()),
            "{tool} missing from a one-per-page listing: {toolset:?}"
        );
    }
}

#[tokio::test]
async fn a_server_listing_an_illegal_tool_name_is_refused_at_connect() {
    let fixture = common::Fixture::new();
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({ "FIXTURE_BAD_TOOL_NAME": "1" }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("kept going")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    common::wait_for_transcript_text(&mut transcript, "kept going").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The whole server is skipped — no truncated name, no partial mount.
    let toolset = &provider.requests()[0].tool_names;
    assert!(
        !toolset
            .iter()
            .any(|name| name.starts_with("mcp__fixture__")),
        "the server with an illegal tool name must be refused entirely: {toolset:?}"
    );
}
