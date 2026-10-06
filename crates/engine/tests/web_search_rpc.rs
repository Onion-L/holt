//! Web-search settings RPCs (ADR-0023): the `web-search.json` record's
//! get/save/reveal/remove surface, masked-get semantics, validation — and
//! the admission-time backend resolution that mounts the `web_search`
//! tool, driven end to end through `RpcService::handle` with a scripted
//! provider and either an injected backend resolver or the real adapter
//! table (mounting only — no test reaches a real search API).

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

#[tokio::test]
async fn a_fresh_engine_defaults_to_keyless_exa() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["active"], json!("exa"));
    assert_eq!(
        state["entries"],
        json!([{ "id": "exa", "kind": "exa", "name": "Exa" }])
    );
    for id in ["exa", "zhipu"] {
        let revealed = value(&engine, methods::REVEAL_WEB_SEARCH_KEY, json!({ "id": id })).await;
        assert_eq!(revealed["key"], json!(null));
    }
    // The default is not written until the user changes something.
    assert!(!fixture.data_dir.path().join("web-search.json").exists());
}

#[tokio::test]
async fn the_state_lists_the_picker_options() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(
        state["backends"],
        json!([
            { "id": "exa", "name": "Exa", "needsKey": false },
            { "id": "zhipu", "name": "Zhipu", "needsKey": true },
            { "id": "bocha", "name": "Bocha", "needsKey": true },
            { "id": "brave", "name": "Brave", "needsKey": true },
        ])
    );
}

#[tokio::test]
async fn configured_records_mount_through_the_builtin_table() {
    let fixture = Fixture::new();
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
    let mounted = |index: usize| {
        provider.requests()[index]
            .tool_names
            .contains(&"web_search".into())
    };

    // The keyless default mounts with no setup at all.
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(mounted(0));

    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "zhipu", "apiKey": "sk-1234567890" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(mounted(1));

    // Off unmounts it from the next admission, entries kept…
    value(
        &engine,
        methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
        json!({ "id": null }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "third").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(!mounted(2));

    // …and switching back to a stored entry mounts it again.
    value(
        &engine,
        methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
        json!({ "id": "zhipu" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "fourth").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(mounted(3));
}

#[tokio::test]
async fn custom_definitions_join_the_picker_and_mount() {
    let fixture = Fixture::new();
    let file = fixture.data_dir.path().join("search-backends.json");
    std::fs::write(
        &file,
        json!({ "backends": [{
            "id": "tavily", "name": "Tavily", "needsKey": true, "type": "http",
            "request": { "method": "POST", "url": "http://127.0.0.1:9/search",
                         "body": { "query": "{query}" } },
            "response": { "results": "/results", "title": "/title", "url": "/url" }
        }]})
        .to_string(),
    )
    .unwrap();
    let provider =
        ScriptedProvider::new(vec![ScriptedReply::text("one"), ScriptedReply::text("two")]);
    let engine = fixture.engine(&provider);

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(
        state["backends"][4],
        json!({ "id": "tavily", "name": "Tavily", "needsKey": true })
    );
    assert_eq!(state["customFile"], json!(file.display().to_string()));
    assert_eq!(state["customError"], json!(null));

    let error = handle(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "tavily" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("apiKey is required"), "unexpected: {error}");
    let state = value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "tavily", "apiKey": "tvly-1234567890" }),
    )
    .await;
    assert_eq!(state["active"], json!("tavily"));
    assert_eq!(state["entries"][1]["name"], json!("Tavily"));

    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[0]
            .tool_names
            .contains(&"web_search".into())
    );

    // A broken file surfaces in the state and unmounts its backend; the
    // entry and its key survive for when the file is fixed.
    std::fs::write(&file, b"{ \"backends\": [").unwrap();
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["backends"].as_array().unwrap().len(), 4);
    assert_eq!(state["active"], json!(null));
    assert_eq!(
        state["entries"],
        json!([{ "id": "exa", "kind": "exa", "name": "Exa" }])
    );
    assert!(
        state["customError"]
            .as_str()
            .unwrap()
            .contains("search-backends.json"),
        "unexpected: {state}"
    );
    let revealed = value(
        &engine,
        methods::REVEAL_WEB_SEARCH_KEY,
        json!({ "id": "tavily" }),
    )
    .await;
    assert_eq!(revealed["key"], json!("tvly-1234567890"));
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "second").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        !provider.requests()[1]
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
        saved["entries"][1],
        json!({ "id": "zhipu", "kind": "zhipu", "name": "Zhipu", "apiKeyMasked": "sk-a…1234" })
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
async fn keyless_saves_and_off_persist_across_restarts() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "brave", "apiKey": "sk-1234567890" }),
    )
    .await;

    // A keyless backend saves without a key; a stray one is ignored.
    let saved = value(
        &engine,
        methods::SAVE_WEB_SEARCH_BACKEND,
        json!({ "kind": "exa", "apiKey": "ignored" }),
    )
    .await;
    assert_eq!(saved["active"], json!("exa"));
    assert_eq!(saved["entries"][0]["apiKeyMasked"], json!(null));
    let revealed = value(
        &engine,
        methods::REVEAL_WEB_SEARCH_KEY,
        json!({ "id": "exa" }),
    )
    .await;
    assert_eq!(revealed["key"], json!(null));

    // A null id turns web search off and keeps every entry.
    let off = value(
        &engine,
        methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
        json!({ "id": null }),
    )
    .await;
    assert_eq!(off["active"], json!(null));
    assert_eq!(off["entries"], saved["entries"]);

    // Off survives a restart instead of falling back to the default.
    drop(engine);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state, off);
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
        assert_eq!(saved["entries"][1]["apiKeyMasked"], json!("…"));
    }
}

#[tokio::test]
async fn save_validates_kind_and_key() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let cases = [
        (
            json!({ "kind": "google", "apiKey": "sk-1234567890" }),
            "unknown search backend",
        ),
        (
            json!({ "kind": "mcp", "server": "fixture", "tool": "echo" }),
            "unknown search backend",
        ),
        (json!({ "kind": "zhipu" }), "apiKey is required"),
        (
            json!({ "kind": "zhipu", "apiKey": "   " }),
            "apiKey is required",
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
async fn removing_the_active_entry_turns_search_off() {
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
    assert_eq!(
        removed["entries"],
        json!([{ "id": "exa", "kind": "exa", "name": "Exa" }])
    );

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
        clock: None,
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

    // The default (Exa) resolves to nothing under this resolver: the
    // tool is absent, never erroring.
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
