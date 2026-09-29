//! Web-search settings RPCs (ADR-0023): the `web-search.json` record's
//! get/save/reveal/remove surface, masked-get semantics, validation — and
//! the admission-time backend resolution that mounts the `web_search`
//! tool, driven end to end through `RpcService::handle` with a scripted
//! provider and either an injected backend resolver or the real adapter
//! table (the MCP kind runs against the stdio fixture server).

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::future::BoxFuture;
use holt_engine::{LocalEngine, SearchBackend, SearchBackendResolver, SearchResults};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

mod common;

/// A backend that resolves every query to nothing — the tests assert on
/// tool mounting and settings flow, never on search results.
struct StubBackend;

impl SearchBackend for StubBackend {
    fn name(&self) -> &str {
        "Stub"
    }

    fn search<'a>(
        &'a self,
        _query: &'a str,
        _max_results: usize,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<SearchResults, String>> {
        Box::pin(async { Ok(SearchResults::Hits(Vec::new())) })
    }
}

/// Resolves the configured id `zhipu` to the stub, mirroring what the
/// built-in adapter table will do once the backend slices land.
fn zhipu_stub() -> SearchBackendResolver {
    Arc::new(|id: &str| (id == "zhipu").then(|| Arc::new(StubBackend) as Arc<dyn SearchBackend>))
}

async fn handle(engine: &LocalEngine, method: &str, params: Value) -> Result<Value, String> {
    match engine.handle(method, params).await {
        Ok(RpcReply::Value(value)) => Ok(value),
        Ok(_) => Err("unexpected reply kind".into()),
        Err(error) => Err(error.to_string()),
    }
}

async fn value(engine: &LocalEngine, method: &str, params: Value) -> Value {
    handle(engine, method, params).await.expect("a value reply")
}

/// `mcp.json` with the stdio fixture as server `fixture`.
fn write_mcp_config(data_dir: &std::path::Path, enabled: bool) {
    let config = json!({
        "mcpServers": {
            "fixture": {
                "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
                "enabled": enabled,
            }
        }
    });
    std::fs::write(data_dir.join("mcp.json"), config.to_string()).unwrap();
}

/// The settled result text of `tool_call_id`, off the provider's last
/// request.
fn tool_result(provider: &ScriptedProvider, tool_call_id: &str) -> String {
    let prefix = format!("toolresult:{tool_call_id}:");
    common::summarize(&provider.requests().last().unwrap().messages)
        .into_iter()
        .find(|row| row.starts_with(&prefix))
        .expect("the call settled")
}

#[tokio::test]
async fn an_mcp_entry_searches_through_the_server_tool() {
    let fixture = Fixture::new();
    // Disabled for chat: its tools stay out of the toolset, yet it still
    // serves search.
    write_mcp_config(fixture.data_dir.path(), false);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "web_search", json!({ "query": "rust async" })),
        ScriptedReply::text("found"),
        ScriptedReply::tool_call("call-2", "web_search", json!({ "query": "boom" })),
        ScriptedReply::text("failed"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "mcp", "server": "fixture", "tool": "echo" }),
    )
    .await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "search").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let tools = &provider.requests()[0].tool_names;
    assert!(tools.contains(&"web_search".into()));
    assert!(
        !tools.iter().any(|name| name.starts_with("mcp__")),
        "a disabled server mounted chat tools: {tools:?}"
    );
    // The query lands in the tool's required string parameter; its text
    // reaches the model as-is under the search header.
    let result = tool_result(&provider, "call-1");
    assert!(
        result.contains("Web search results from fixture / echo for \"rust async\""),
        "{result}"
    );
    assert!(result.contains("echo: rust async"), "{result}");

    // An `isError` result surfaces as the tool's error, naming the entry.
    let id = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await["active"].clone();
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "mcp", "id": id, "server": "fixture", "tool": "fail" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "again").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let result = tool_result(&provider, "call-2");
    assert!(
        result.contains("fixture / fail search failed: fixture error: boom"),
        "{result}"
    );
}

#[tokio::test]
async fn an_unconfigured_engine_reports_empty_state() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["active"], json!(null));
    assert_eq!(state["entries"], json!([]));

    let revealed = value(
        &engine,
        methods::REVEAL_WEB_SEARCH_KEY,
        json!({ "id": "zhipu" }),
    )
    .await;
    assert_eq!(revealed["key"], json!(null));
}

#[tokio::test]
async fn the_state_lists_the_picker_options() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(
        state["backends"],
        json!([
            { "id": "zhipu", "name": "Zhipu" },
            { "id": "bocha", "name": "Bocha" },
            { "id": "brave", "name": "Brave" },
        ])
    );
}

#[tokio::test]
async fn configured_records_mount_through_the_builtin_table() {
    let fixture = Fixture::new();
    write_mcp_config(fixture.data_dir.path(), true);
    // A plain engine — no injected resolver: the adapter table resolves
    // the active entry itself.
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("one"),
        ScriptedReply::text("two"),
        ScriptedReply::text("three"),
        ScriptedReply::text("four"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "zhipu", "apiKey": "sk-1234567890" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[0]
            .tool_names
            .contains(&"web_search".into())
    );

    // An MCP search tool mounts too.
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "mcp", "server": "fixture", "tool": "echo" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[1]
            .tool_names
            .contains(&"web_search".into())
    );

    // Switching back to a stored entry keeps the tool mounted…
    value(
        &engine,
        methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
        json!({ "id": "zhipu" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "third").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[2]
            .tool_names
            .contains(&"web_search".into())
    );

    // …while removing the active entry unmounts it from the next
    // admission, even with another entry still stored.
    value(
        &engine,
        methods::REMOVE_WEB_SEARCH_BACKEND,
        json!({ "id": "zhipu" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "fourth").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[3]
            .tool_names
            .contains(&"web_search".into())
    );
}

#[tokio::test]
async fn save_replies_the_masked_state_and_persists_the_record() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let saved = value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "zhipu", "apiKey": "sk-abcdefgh1234" }),
    )
    .await;
    assert_eq!(saved["active"], json!("zhipu"));
    assert_eq!(
        saved["entries"],
        json!([{ "id": "zhipu", "kind": "zhipu", "name": "Zhipu", "apiKeyMasked": "sk-a…1234" }])
    );

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state, saved);

    let revealed = value(
        &engine,
        methods::REVEAL_WEB_SEARCH_KEY,
        json!({ "id": "zhipu" }),
    )
    .await;
    assert_eq!(revealed["key"], json!("sk-abcdefgh1234"));

    // The record survives a restart; the file stays user-only.
    drop(engine);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["active"], json!("zhipu"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = fixture.data_dir.path().join("web-search.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn mcp_entries_are_created_updated_and_listed_beside_builtins() {
    let fixture = Fixture::new();
    write_mcp_config(fixture.data_dir.path(), true);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "brave", "apiKey": "sk-1234567890" }),
    )
    .await;

    let saved = value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "mcp", "server": " fixture ", "tool": " echo ", "apiKey": "ignored" }),
    )
    .await;
    let entry = &saved["entries"][1];
    let id = entry["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("mcp-"), "unexpected id {id}");
    assert_eq!(saved["active"], json!(id));
    assert_eq!(entry["server"], json!("fixture"));
    assert_eq!(entry["tool"], json!("echo"));
    // The server's own config carries its auth: no key is stored.
    assert_eq!(entry["apiKeyMasked"], json!(null));
    let revealed = value(&engine, methods::REVEAL_WEB_SEARCH_KEY, json!({ "id": id })).await;
    assert_eq!(revealed["key"], json!(null));

    // Saving with its id updates in place.
    let updated = value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "mcp", "id": id, "server": "fixture", "tool": "fail" }),
    )
    .await;
    assert_eq!(updated["entries"].as_array().unwrap().len(), 2);
    assert_eq!(updated["entries"][1]["tool"], json!("fail"));

    // Set-active flips between stored entries without touching them.
    let switched = value(
        &engine,
        methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
        json!({ "id": "brave" }),
    )
    .await;
    assert_eq!(switched["active"], json!("brave"));
    assert_eq!(switched["entries"], updated["entries"]);
}

#[tokio::test]
async fn a_short_key_masks_to_nothing() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    // Eight characters would be first-four + last-four with nothing hidden;
    // the boundary masks fully, and so does anything shorter.
    for key in ["12345678", "short"] {
        let saved = value(
            &engine,
            methods::SAVE_WEB_SEARCH_BACKEND,
            json!({ "kind": "bocha", "apiKey": key }),
        )
        .await;
        assert_eq!(saved["entries"][0]["apiKeyMasked"], json!("…"));
    }
}

#[tokio::test]
async fn save_validates_kind_key_server_and_tool() {
    let fixture = Fixture::new();
    write_mcp_config(fixture.data_dir.path(), true);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let cases = [
        (
            json!({ "kind": "google", "apiKey": "sk-1234567890" }),
            "unknown search backend",
        ),
        (json!({ "kind": "zhipu" }), "apiKey is required"),
        (
            json!({ "kind": "zhipu", "apiKey": "   " }),
            "apiKey is required",
        ),
        (
            json!({ "kind": "mcp", "tool": "echo" }),
            "server is required",
        ),
        (
            json!({ "kind": "mcp", "server": "fixture" }),
            "tool is required",
        ),
        (
            json!({ "kind": "mcp", "server": "fixture", "tool": "  " }),
            "tool is required",
        ),
        (
            json!({ "kind": "mcp", "server": "nope", "tool": "echo" }),
            "no mcp server",
        ),
        (
            json!({ "kind": "mcp", "id": "mcp-nope", "server": "fixture", "tool": "echo" }),
            "no mcp search backend",
        ),
    ];
    for (params, expected) in cases {
        let error = handle(&engine, methods::SAVE_WEB_SEARCH_BACKEND, params)
            .await
            .unwrap_err();
        assert!(
            error.contains(expected),
            "expected {expected:?}, got: {error}"
        );
    }

    let error = handle(
        &engine,
        methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
        json!({ "id": "zhipu" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("no search backend"), "unexpected: {error}");

    // Nothing was written.
    assert!(!fixture.data_dir.path().join("web-search.json").exists());
}

#[tokio::test]
async fn removing_the_last_entry_clears_the_state_and_deletes_the_file() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "brave", "apiKey": "sk-1234567890" }),
    )
    .await;

    let removed = value(
        &engine,
        methods::REMOVE_WEB_SEARCH_BACKEND,
        json!({ "id": "brave" }),
    )
    .await;
    assert_eq!(removed["active"], json!(null));
    assert_eq!(removed["entries"], json!([]));
    assert!(!fixture.data_dir.path().join("web-search.json").exists());

    let revealed = value(
        &engine,
        methods::REVEAL_WEB_SEARCH_KEY,
        json!({ "id": "brave" }),
    )
    .await;
    assert_eq!(revealed["key"], json!(null));
}

#[tokio::test]
async fn a_legacy_single_record_still_loads() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.data_dir.path().join("web-search.json"),
        br#"{"backend": "bocha", "apiKey": "sk-legacy-1234"}"#,
    )
    .unwrap();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["active"], json!("bocha"));
    assert_eq!(state["entries"][0]["name"], json!("Bocha"));
}

#[tokio::test]
async fn a_corrupt_record_fails_engine_startup() {
    let fixture = Fixture::new();
    let path = fixture.data_dir.path().join("web-search.json");
    std::fs::write(&path, b"{broken").unwrap();

    let error = match LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: None,
        search_backend_resolver: None,
    }) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a corrupt web-search record must fail engine startup"),
    };
    assert!(
        error.contains("web-search.json is malformed"),
        "unexpected: {error}"
    );
    // Loud, but non-destructive: the file waits for manual repair.
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
}

#[tokio::test]
async fn the_backend_resolves_once_per_turn_admission() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("one"),
        ScriptedReply::text("two"),
        ScriptedReply::text("three"),
    ]);
    let engine = fixture.engine_with_search_backend(&provider, zhipu_stub());
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // Unconfigured: the tool is absent, never erroring.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[0]
            .tool_names
            .contains(&"web_search".into())
    );

    // Configuring zhipu mounts the tool — from the NEXT Turn's admission.
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "zhipu", "apiKey": "sk-1234567890" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[1]
            .tool_names
            .contains(&"web_search".into())
    );

    // Removing the record unmounts it again, from the next admission.
    value(
        &engine,
        methods::REMOVE_WEB_SEARCH_BACKEND,
        json!({ "id": "zhipu" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "third").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[2]
            .tool_names
            .contains(&"web_search".into())
    );
}

#[tokio::test]
async fn an_explorer_child_mounts_the_configured_backend() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            json!({"subagent_type": "explorer", "description": "Look around", "prompt": "Report"}),
        ),
        ScriptedReply::text("Child findings"),
        ScriptedReply::text("Parent conclusion"),
    ]);
    let engine = fixture.engine_with_search_backend(&provider, zhipu_stub());
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "zhipu", "apiKey": "sk-1234567890" }),
    )
    .await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "Delegate").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    // The parent Turns and the Explorer child all carry the tool: the
    // admission-time resolution threads through the Delegation, and the
    // Explorer's read-only whitelist keeps it (ADR-0023).
    for index in [0, 1, 2] {
        assert!(
            requests[index].tool_names.contains(&"web_search".into()),
            "request {index} lacks web_search: {:?}",
            requests[index].tool_names
        );
    }
    // The Explorer's whole toolset: read, grep, ls, web_fetch, web_search,
    // read_chat — and nothing else.
    assert_eq!(
        requests[1].tool_names,
        ["read", "grep", "ls", "web_fetch", "web_search", "read_chat"]
    );
}

#[tokio::test]
async fn a_settings_change_mid_turn_leaves_the_running_turn_mounted() {
    let fixture = Fixture::new();
    std::fs::write(fixture.project_dir.path().join("note.txt"), "hi").unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "read", json!({ "path": "note.txt" })),
        ScriptedReply::text("mid done"),
        ScriptedReply::text("next turn"),
    ]);
    let engine = fixture.engine_with_search_backend(&provider, zhipu_stub());
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "zhipu", "apiKey": "sk-1234567890" }),
    )
    .await;

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read it").await;
    // Wait until the read tool has settled — the Turn is mid-flight, its
    // continuation request still pending.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
            let resolved = snapshot["reset"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|entry| entry["parts"].as_array().unwrap())
                .any(|part| part["id"] == "call-1" && part["resolved"] == true);
            if resolved {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the read tool never settled");

    // Unconfigure mid-Turn: the running Turn's toolset was fixed at its
    // admission, so its continuation request keeps web_search…
    value(
        &engine,
        methods::REMOVE_WEB_SEARCH_BACKEND,
        json!({ "id": "zhipu" }),
    )
    .await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[1]
            .tool_names
            .contains(&"web_search".into())
    );

    // …and the next Turn, admitted after the change, drops it.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "and then").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[2]
            .tool_names
            .contains(&"web_search".into())
    );
}
