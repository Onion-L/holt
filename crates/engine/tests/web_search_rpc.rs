//! Web-search settings RPCs (ADR-0023): the `web-search.json` record's
//! get/save/reveal/remove surface, masked-get semantics, validation — and
//! the admission-time backend resolution that mounts the `web_search`
//! tool, driven end to end through `RpcService::handle` with a scripted
//! provider and an injected backend resolver standing in for the adapter
//! table (the real adapters land as their own slices).

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::future::BoxFuture;
use holt_engine::{LocalEngine, SearchBackend, SearchBackendResolver, SearchHit};
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
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>> {
        Box::pin(async { Ok(Vec::new()) })
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
async fn an_unconfigured_engine_reports_empty_state() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["backend"], json!(null));
    assert_eq!(state["apiKeyMasked"], json!(null));

    let revealed = value(&engine, methods::REVEAL_WEB_SEARCH_KEY, json!({})).await;
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
async fn a_configured_zhipu_record_mounts_through_the_builtin_table() {
    let fixture = Fixture::new();
    // A plain engine — no injected resolver: the built-in adapter table
    // resolves the configured record itself.
    let provider =
        ScriptedProvider::new(vec![ScriptedReply::text("one"), ScriptedReply::text("two")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    value(
        &engine,
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "zhipu", "apiKey": "sk-1234567890" }),
    )
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "first").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    assert!(
        provider.requests()[0]
            .tool_names
            .contains(&"web_search".into())
    );

    // An id whose adapter has not shipped saves fine but mounts nothing.
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "bocha", "apiKey": "sk-1234567890" }),
    )
    .await;
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
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "zhipu", "apiKey": "sk-abcdefgh1234" }),
    )
    .await;
    assert_eq!(saved["backend"], json!("zhipu"));
    assert_eq!(saved["apiKeyMasked"], json!("sk-a…1234"));

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state, saved);

    let revealed = value(&engine, methods::REVEAL_WEB_SEARCH_KEY, json!({})).await;
    assert_eq!(revealed["key"], json!("sk-abcdefgh1234"));

    // The record survives a restart; the file stays user-only.
    drop(engine);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["backend"], json!("zhipu"));
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
async fn a_short_key_masks_to_nothing() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    // Eight characters would be first-four + last-four with nothing hidden;
    // the boundary masks fully, and so does anything shorter.
    for key in ["12345678", "short"] {
        let saved = value(
            &engine,
            methods::SAVE_WEB_SEARCH_SETTINGS,
            json!({ "backend": "bocha", "apiKey": key }),
        )
        .await;
        assert_eq!(saved["backend"], json!("bocha"));
        assert_eq!(saved["apiKeyMasked"], json!("…"));
    }
}

#[tokio::test]
async fn save_validates_the_backend_id_and_key() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));

    let error = handle(
        &engine,
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "google", "apiKey": "sk-1234567890" }),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("unknown search backend"),
        "unexpected: {error}"
    );

    for params in [
        json!({ "backend": "zhipu" }),
        json!({ "backend": "zhipu", "apiKey": "   " }),
    ] {
        let error = handle(&engine, methods::SAVE_WEB_SEARCH_SETTINGS, params)
            .await
            .unwrap_err();
        assert!(error.contains("apiKey is required"), "unexpected: {error}");
    }

    // Nothing was written.
    assert!(!fixture.data_dir.path().join("web-search.json").exists());
}

#[tokio::test]
async fn remove_clears_the_state_and_deletes_the_file() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    value(
        &engine,
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "brave", "apiKey": "sk-1234567890" }),
    )
    .await;

    let removed = value(&engine, methods::REMOVE_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(removed, json!({}));

    let state = value(&engine, methods::GET_WEB_SEARCH_SETTINGS, json!({})).await;
    assert_eq!(state["backend"], json!(null));
    assert!(!fixture.data_dir.path().join("web-search.json").exists());

    let revealed = value(&engine, methods::REVEAL_WEB_SEARCH_KEY, json!({})).await;
    assert_eq!(revealed["key"], json!(null));
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
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "zhipu", "apiKey": "sk-1234567890" }),
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
    value(&engine, methods::REMOVE_WEB_SEARCH_SETTINGS, json!({})).await;
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
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "zhipu", "apiKey": "sk-1234567890" }),
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
    // The Explorer's whole toolset: read, grep, web_fetch, web_search,
    // read_chat — and nothing else.
    assert_eq!(
        requests[1].tool_names,
        ["read", "grep", "web_fetch", "web_search", "read_chat"]
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
        methods::SAVE_WEB_SEARCH_SETTINGS,
        json!({ "backend": "zhipu", "apiKey": "sk-1234567890" }),
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
    value(&engine, methods::REMOVE_WEB_SEARCH_SETTINGS, json!({})).await;
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
