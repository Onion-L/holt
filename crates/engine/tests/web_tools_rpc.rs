//! The web tools' whole-Turn path (ADR-0023, ticket 08): a scripted model
//! calling `web_fetch` (against the in-process loopback server) and
//! `web_search` (against an injected backend) folds onto the sync-era
//! WebFetch/WebSearch chips with `prompt` never populated, travels the
//! ordinary transcript/History persistence, and never meets the
//! confirm-changes gate in any Permission mode.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use futures::future::BoxFuture;
use holt_engine::{LocalEngine, SearchBackend, SearchBackendResolver, SearchHit};
use holt_rpc::{RpcService as _, methods};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A search backend that finds nothing — these tests assert the chip, the
/// gate, and persistence, never what a search returned.
struct EmptyBackend;

impl SearchBackend for EmptyBackend {
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

/// Resolves the configured id `zhipu` to the empty backend, standing in for
/// the built-in adapter table.
fn empty_backend_resolver() -> SearchBackendResolver {
    Arc::new(|id: &str| (id == "zhipu").then(|| Arc::new(EmptyBackend) as Arc<dyn SearchBackend>))
}

/// The transcript part carrying `tool_call_id`, off a fresh watch snapshot.
fn part(snapshot: &Value, tool_call_id: &str) -> Value {
    snapshot["reset"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().cloned().unwrap_or_default())
        .find(|part| part["id"] == tool_call_id)
        .unwrap_or_else(|| panic!("no part {tool_call_id} in {snapshot}"))
}

/// Every gate in a chat's transcript, as (tool call id, state).
async fn gates(engine: &LocalEngine, chat_id: &str) -> Vec<(String, String)> {
    let snapshot = common::transcript_snapshot(engine, chat_id).await;
    snapshot["reset"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().cloned().unwrap_or_default())
        .filter(|part| !part["gate"].is_null())
        .map(|part| {
            (
                part["id"].as_str().unwrap_or("?").to_string(),
                common::gate_state_raw(&part["gate"]),
            )
        })
        .collect()
}

/// Run one scripted Turn on `chat_id` in `mode`, returning the engine so the
/// caller can assert on both the provider's requests (through its own
/// handle) and the transcript.
async fn run_turn(
    fixture: &Fixture,
    provider: &ScriptedProvider,
    chat_id: &str,
    mode: &str,
    prompt: &str,
) -> LocalEngine {
    let engine = fixture.engine(provider);
    common::setup_chat(&engine, chat_id).await;
    engine
        .handle(
            methods::MUTATE,
            json!({ "op": "setChatPermissionMode", "chatId": chat_id, "mode": mode }),
        )
        .await
        .unwrap();
    let (_, mut sessions) = common::subscribe(&engine, chat_id).await;
    common::run_prompt(&engine, chat_id, &fixture.cwd(), prompt).await;
    common::wait_for_session_status(&mut sessions, chat_id, "idle").await;
    engine
}

/// A `web_fetch` Turn in every Permission mode: the call reaches the
/// loopback page, its text rides back into the model's continuation
/// request, the chip folds onto `ToolCall::WebFetch` with no `prompt`, and
/// no mode produces a gate — the call is read-tier (ADR-0023).
#[tokio::test]
async fn a_scripted_web_fetch_call_folds_a_chip_gate_free_in_every_mode() {
    let fixture = Fixture::new();
    let server = common::serve_loopback(
        "text/html; charset=utf-8",
        b"<html><body><h1>Loopback</h1><p>Fetched body text</p></body></html>",
    )
    .await;
    let url = format!("{}/page", server.base);

    for (chat_id, mode) in [
        ("chat-confirm", "confirm-changes"),
        ("chat-auto", "auto-review"),
        ("chat-full", "full-access"),
    ] {
        let provider = ScriptedProvider::new(vec![
            ScriptedReply::tool_call("call-1", "web_fetch", json!({ "url": url })),
            ScriptedReply::text("fetched"),
        ]);
        let engine = run_turn(&fixture, &provider, chat_id, mode, "fetch the page").await;

        // The call ran ungated, and the fetched text is the tool result the
        // model's second round was served.
        let summary = common::summarize(&provider.requests()[1].messages);
        assert!(
            summary
                .iter()
                .any(|line| line.starts_with("toolresult:call-1:")
                    && line.contains("Fetched body text")),
            "[{mode}] unexpected tool result: {summary:?}"
        );

        let snapshot = common::transcript_snapshot(&engine, chat_id).await;
        let chip = part(&snapshot, "call-1");
        assert_eq!(chip["call"]["kind"], json!("webFetch"), "[{mode}] {chip}");
        assert_eq!(chip["call"]["url"], json!(url), "[{mode}] {chip}");
        assert!(
            chip["call"].get("prompt").is_none(),
            "[{mode}] prompt must never be populated: {chip}"
        );
        assert_eq!(chip["isError"], json!(false), "[{mode}] {chip}");
        assert_eq!(chip["resolved"], json!(true), "[{mode}] {chip}");
        assert!(
            chip["output"]
                .as_str()
                .is_some_and(|output| output.contains("Fetched body text")),
            "[{mode}] the fetched text must ride the chip: {chip}"
        );
        // No gate artifact in any mode.
        assert!(chip["gate"].is_null(), "[{mode}] {chip}");
        assert!(gates(&engine, chat_id).await.is_empty(), "[{mode}] gated");
    }
}

/// The chip is transcript state: a restart on the same data dir replays it,
/// fold and output intact — no run state is needed to render it.
#[tokio::test]
async fn the_web_fetch_chip_survives_a_restart() {
    let fixture = Fixture::new();
    let server = common::serve_loopback(
        "text/plain",
        b"plain loopback body exported for the follow-up round",
    )
    .await;
    let url = format!("{}/notes", server.base);
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "web_fetch", json!({ "url": url })),
        ScriptedReply::text("fetched"),
    ]);
    // Keep the handle alive only for the turn; the assertions below read the
    // persisted transcript, which is the same record the model would get.
    let engine = run_turn(&fixture, &provider, "chat-1", "confirm-changes", "fetch").await;
    assert!(gates(&engine, "chat-1").await.is_empty());

    // Restart: a fresh engine on the same data dir.
    drop(engine);
    let engine = fixture.engine(&ScriptedProvider::new(vec![]));
    let replayed = part(
        &common::transcript_snapshot(&engine, "chat-1").await,
        "call-1",
    );
    assert_eq!(replayed["call"]["kind"], json!("webFetch"));
    assert_eq!(replayed["call"]["url"], json!(url));
    assert!(
        replayed["call"].get("prompt").is_none(),
        "prompt must stay absent across a restart: {replayed}"
    );
    assert!(
        replayed["output"]
            .as_str()
            .is_some_and(|output| output.contains("plain loopback body exported")),
        "{replayed}"
    );
    assert!(replayed["gate"].is_null(), "{replayed}");
}

/// A `web_search` Turn folds onto `ToolCall::WebSearch` with the query, and
/// is equally ungated.
#[tokio::test]
async fn a_scripted_web_search_call_folds_a_chip_and_never_gates() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "web_search",
            json!({ "query": "holt web tools", "max_results": 3 }),
        ),
        ScriptedReply::text("searched"),
    ]);
    let engine = fixture.engine_with_search_backend(&provider, empty_backend_resolver());
    common::setup_chat(&engine, "chat-1").await;
    // Configured before the prompt: admission resolves the backend once.
    engine
        .handle(
            methods::SAVE_WEB_SEARCH_SETTINGS,
            json!({ "backend": "zhipu", "apiKey": "sk-1234567890" }),
        )
        .await
        .unwrap();
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "search for it").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The configured backend mounted the tool for this Turn's admission.
    assert!(
        provider.requests()[0]
            .tool_names
            .contains(&"web_search".into())
    );
    // The search ran ungated: the backend's rendered result is the tool
    // result the model's second round was served.
    let summary = common::summarize(&provider.requests()[1].messages);
    assert!(
        summary
            .iter()
            .any(|line| line.starts_with("toolresult:call-1:")
                && line.contains("Web search results from Stub for \"holt web tools\"")),
        "unexpected tool result: {summary:?}"
    );
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    let chip = part(&snapshot, "call-1");
    assert_eq!(chip["call"]["kind"], json!("webSearch"), "{chip}");
    assert_eq!(chip["call"]["query"], json!("holt web tools"), "{chip}");
    assert_eq!(chip["isError"], json!(false), "{chip}");
    assert_eq!(chip["resolved"], json!(true), "{chip}");
    assert!(chip["gate"].is_null(), "{chip}");
    assert!(gates(&engine, "chat-1").await.is_empty());
}
