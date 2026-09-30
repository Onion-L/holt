//! The agent's `web_search` tool plus the [`SearchBackend`] contract it
//! sits on (ADR-0023). One query in, a result list out — the
//! tool finds pages and never reads them (that is `web_fetch`'s job), and
//! it exists only while a backend is active: with none, the
//! tool is absent from the model's toolset, never registered-and-erroring.
//!
//! The backend is user-chosen — keyless Exa (the default), or Zhipu,
//! Bocha, Brave with the user's own key — one adapter module each,
//! sharing the [`transport`] scaffolding — or one the user defines in
//! `search-backends.json` ([`custom`]); this module owns the trait, the
//! tool, the output shape, and the adapter table. Whatever the backend
//! returns, the model sees at most [`OUTPUT_BYTE_CAP`] of it — the
//! web_fetch envelope, with an in-band notice when it was cut. The
//! tool races the run's cancellation token around the backend call,
//! exactly like grep and web_fetch.

mod bocha;
mod brave;
pub(crate) mod custom;
mod exa;
mod transport;
mod zhipu;

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// One launch backend (ADR-0023) as the Settings picker offers it.
pub(crate) struct Backend {
    /// The record and save-RPC id.
    pub(crate) id: &'static str,
    /// The display name.
    pub(crate) name: &'static str,
    /// Whether the backend runs on the user's own API key.
    pub(crate) needs_key: bool,
}

/// The built-in backends (ADR-0023) — the save RPC's validation set and
/// the Settings picker's option list, in picker order. Every id here
/// mounts its adapter through [`adapter`].
pub(crate) const BACKENDS: [Backend; 4] = [
    Backend {
        id: "exa",
        name: "Exa",
        needs_key: false,
    },
    Backend {
        id: "zhipu",
        name: "Zhipu",
        needs_key: true,
    },
    Backend {
        id: "bocha",
        name: "Bocha",
        needs_key: true,
    },
    Backend {
        id: "brave",
        name: "Brave",
        needs_key: true,
    },
];

/// The backend a fresh install starts on: keyless, so search works
/// before any setup.
pub(crate) const DEFAULT_BACKEND: &str = "exa";

/// The adapter table behind the engine's Turn-admission resolution (the
/// injected test resolver aside): a built-in kind, else a definition from
/// `search-backends.json`. `None` for a kind neither knows.
pub(crate) fn adapter(
    entry: &crate::web_search_settings::WebSearchEntry,
    custom: &[custom::CustomBackend],
) -> Option<Arc<dyn SearchBackend>> {
    let api_key = entry.api_key.clone();
    match entry.kind.as_str() {
        "exa" => Some(Arc::new(exa::ExaBackend::new())),
        "zhipu" => Some(Arc::new(zhipu::ZhipuBackend::new(api_key))),
        "bocha" => Some(Arc::new(bocha::BochaBackend::new(api_key))),
        "brave" => Some(Arc::new(brave::BraveBackend::new(api_key))),
        kind => custom
            .iter()
            .find(|backend| backend.id == kind)
            .map(|backend| backend.adapter(api_key)),
    }
}

/// Hit count used when the model omits `max_results`.
const DEFAULT_MAX_RESULTS: usize = 5;
/// Lower clamp for `max_results`: a zero ask yields one hit, not an error
/// (a negative ask fails deserialization as an invalid parameter).
const MIN_MAX_RESULTS: usize = 1;
/// Upper clamp for `max_results`.
const MAX_MAX_RESULTS: usize = 10;

/// Byte envelope for the text handed back to the model (the web_fetch
/// contract): a backend's result — hit list or raw text — is untrusted
/// remote content, replayed into the model's prompt and persisted to
/// the chat History, so it never arrives unbounded.
const OUTPUT_BYTE_CAP: usize = 50 * 1024;
/// The in-band notice appended when the envelope cut the output.
const TRUNCATION_NOTICE: &str = "\n\n[truncated: output capped at 50 KB]";

const DESCRIPTION: &str = "Search the web via the configured search backend and return a \
list of results, each with a title, URL, and snippet. Parameters: `query` (required) and \
optional `max_results` (default 5, clamped to 1–10). The backend's name is stated in the output \
header. Backend responses are capped at 5 MB and the returned text at 50 KB (a truncation \
notice is included). This tool only finds pages — it never opens, crawls, or summarizes them; fetch a result's \
content with web_fetch. Backend failures (missing key, quota, network) return as errors naming the \
backend. Results are not cached.";

/// One search result as the model sees it. Adapters map their API's
/// fields onto this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// What one query returned: structured hits the tool renders as a
/// numbered list, or a backend's own text result list passed to the model
/// as-is (Exa).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchResults {
    Hits(Vec<SearchHit>),
    Text(String),
}

/// The pluggable web-search service behind the `web_search` tool: the
/// user's Settings choice, carried as its own key (never a provider
/// credential). Implemented by the backend adapters; mounted only when
/// configured.
pub trait SearchBackend: Send + Sync {
    /// The backend's display name — the tool's output header states it so
    /// the model knows where its results came from, and errors name it.
    fn name(&self) -> &str;
    /// Run one query asking for at most `max_results` hits (already
    /// clamped by the tool; a text result is trusted to honor it). `cancel` fires when the owning Turn ends;
    /// adapters race their request against it.
    fn search<'a>(
        &'a self,
        query: &'a str,
        max_results: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<SearchResults, String>>;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSearchInput {
    query: String,
    max_results: Option<usize>,
}

/// The effective hit count for a call: the default when the model omits
/// `max_results`, clamped into 1–10 otherwise.
fn result_cap(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(MIN_MAX_RESULTS, MAX_MAX_RESULTS)
}

/// Render the hits as the numbered `title\nurl\nsnippet` list the model
/// reads, under a header naming the backend and the query.
fn render(backend: &str, query: &str, hits: &[SearchHit]) -> String {
    let mut out = format!("Web search results from {backend} for \"{query}\"\n\n");
    if hits.is_empty() {
        out.push_str("No results.");
        return out;
    }
    for (index, hit) in hits.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}",
            index + 1,
            hit.title,
            hit.url,
            hit.snippet
        ));
    }
    out
}

fn into_result(
    backend: &str,
    query: &str,
    max_results: usize,
    results: SearchResults,
) -> AgentToolResult {
    let (rendered, count) = match results {
        SearchResults::Hits(mut hits) => {
            hits.truncate(max_results);
            (render(backend, query, &hits), json!(hits.len()))
        }
        SearchResults::Text(text) => (
            format!("Web search results from {backend} for \"{query}\"\n\n{text}"),
            serde_json::Value::Null,
        ),
    };
    // The output envelope bounds every backend-authored byte — the
    // model's prompt and the persisted History both stop here, with an
    // in-band notice when the cap bit.
    let (body, truncated) = crate::tools::clamp_utf8(&rendered, OUTPUT_BYTE_CAP);
    let mut text = body.to_owned();
    if truncated {
        text.push_str(TRUNCATION_NOTICE);
    }
    AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text,
            ..Default::default()
        })],
        details: json!({
            "backend": backend,
            "query": query,
            "max_results": max_results,
            "results": count,
            "truncated": truncated,
        }),
        ..Default::default()
    }
}

fn parameters_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query"
            },
            "max_results": {
                "type": "integer",
                "minimum": MIN_MAX_RESULTS,
                "maximum": MAX_MAX_RESULTS,
                "description": "Maximum number of results to return (default 5)"
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

pub(crate) fn create_web_search_tool(backend: Arc<dyn SearchBackend>) -> AgentTool {
    let execute = Arc::new(
        move |_tool_call_id: &str,
              params: &serde_json::Value,
              signal: Option<&CancellationToken>,
              _on_update: Option<&AgentToolUpdateCallback>| {
            let backend = Arc::clone(&backend);
            let input = serde_json::from_value::<WebSearchInput>(params.clone())
                .map_err(|error| format!("invalid web_search parameters: {error}"));
            let cancel = signal.cloned().unwrap_or_default();
            Box::pin(async move {
                let input = input?;
                let max_results = result_cap(input.max_results);
                // The token races at the tool boundary (a stuck backend
                // cannot outlive the Turn) and is handed to the backend so
                // well-behaved adapters drop their in-flight request.
                let search = backend.search(&input.query, max_results, cancel.clone());
                let results = tokio::select! {
                    _ = cancel.cancelled() => return Err("search cancelled".to_string()),
                    result = search => result?,
                };
                Ok(into_result(
                    backend.name(),
                    &input.query,
                    max_results,
                    results,
                ))
            }) as BoxFuture<'static, Result<AgentToolResult, String>>
        },
    );
    AgentTool {
        name: "web_search".to_string(),
        label: "Web Search".to_string(),
        description: DESCRIPTION.to_string(),
        parameters: parameters_schema(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn hit(title: &str, url: &str, snippet: &str) -> SearchHit {
        SearchHit {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
        }
    }

    /// Test-only backend covering the trait's contract: canned hits, a
    /// canned error, and a hang that ignores its token (pinning the
    /// tool-level cancellation race). Records the asks it received.
    struct StubBackend {
        name: &'static str,
        hits: Vec<SearchHit>,
        error: Option<String>,
        hang: bool,
        seen: Mutex<Vec<(String, usize)>>,
    }

    impl StubBackend {
        fn new(name: &'static str, hits: Vec<SearchHit>) -> Self {
            Self {
                name,
                hits,
                error: None,
                hang: false,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn failing(name: &'static str, error: &str) -> Self {
            Self {
                error: Some(error.into()),
                ..Self::new(name, Vec::new())
            }
        }

        fn hanging(name: &'static str) -> Self {
            Self {
                hang: true,
                ..Self::new(name, Vec::new())
            }
        }

        fn seen(&self) -> Vec<(String, usize)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl SearchBackend for StubBackend {
        fn name(&self) -> &str {
            self.name
        }

        fn search<'a>(
            &'a self,
            query: &'a str,
            max_results: usize,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<SearchResults, String>> {
            Box::pin(async move {
                self.seen
                    .lock()
                    .unwrap()
                    .push((query.to_string(), max_results));
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                if self.hang {
                    std::future::pending::<()>().await;
                }
                Ok(SearchResults::Hits(self.hits.clone()))
            })
        }
    }

    /// A backend that ends its own query when the token fires — the
    /// adapter-side half of the cancellation contract.
    struct TokenRacingBackend;

    impl SearchBackend for TokenRacingBackend {
        fn name(&self) -> &str {
            "Racer"
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _max_results: usize,
            cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<SearchResults, String>> {
            Box::pin(async move {
                cancel.cancelled().await;
                Err("cancelled by token".into())
            })
        }
    }

    async fn run_tool(
        backend: Arc<dyn SearchBackend>,
        params: &serde_json::Value,
        signal: Option<&CancellationToken>,
    ) -> Result<AgentToolResult, String> {
        let tool = create_web_search_tool(backend);
        (tool.execute)("call-1", params, signal, None).await
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first().unwrap() {
            BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn result_cap_defaults_and_clamps() {
        assert_eq!(result_cap(None), 5);
        assert_eq!(result_cap(Some(3)), 3);
        assert_eq!(result_cap(Some(0)), 1);
        assert_eq!(result_cap(Some(1)), 1);
        assert_eq!(result_cap(Some(10)), 10);
        assert_eq!(result_cap(Some(99)), 10);
    }

    #[test]
    fn hits_render_as_a_numbered_list_under_a_backend_header() {
        let hits = vec![
            hit(
                "Async Rust",
                "https://example.com/async",
                "Async in Rust, explained.",
            ),
            hit("Tokio", "https://tokio.rs", "The async runtime."),
        ];
        let text = render("Brave", "rust async", &hits);
        assert_eq!(
            text,
            "Web search results from Brave for \"rust async\"\n\n\
             1. Async Rust\n   https://example.com/async\n   Async in Rust, explained.\n\n\
             2. Tokio\n   https://tokio.rs\n   The async runtime."
        );
    }

    #[test]
    fn empty_results_render_an_explicit_notice() {
        let text = render("Zhipu", "nothing here", &[]);
        assert_eq!(
            text,
            "Web search results from Zhipu for \"nothing here\"\n\nNo results."
        );
    }

    #[tokio::test]
    async fn a_successful_query_renders_the_backend_name_and_clamped_count() {
        let backend = Arc::new(StubBackend::new(
            "Bocha",
            vec![
                hit("One", "https://one", "first"),
                hit("Two", "https://two", "second"),
                hit("Three", "https://three", "third"),
            ],
        ));
        let result = run_tool(
            backend.clone(),
            &json!({ "query": "holt", "max_results": 2 }),
            None,
        )
        .await
        .unwrap();

        let text = text_of(&result);
        assert!(text.starts_with("Web search results from Bocha for \"holt\""));
        assert!(text.contains("1. One\n   https://one\n   first"));
        assert!(!text.contains("Three"), "hits not capped: {text}");
        assert_eq!(
            result.details,
            json!({
                "backend": "Bocha",
                "query": "holt",
                "max_results": 2,
                "results": 2,
                "truncated": false,
            })
        );
        assert_eq!(backend.seen(), vec![("holt".into(), 2)]);
    }

    #[tokio::test]
    async fn an_omitted_max_results_asks_for_five_and_out_of_range_clamps() {
        let backend = Arc::new(StubBackend::new("Stub", Vec::new()));
        run_tool(backend.clone(), &json!({ "query": "a" }), None)
            .await
            .unwrap();
        run_tool(
            backend.clone(),
            &json!({ "query": "b", "max_results": 0 }),
            None,
        )
        .await
        .unwrap();
        run_tool(
            backend.clone(),
            &json!({ "query": "c", "max_results": 99 }),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            backend.seen(),
            vec![("a".into(), 5), ("b".into(), 1), ("c".into(), 10),]
        );
    }

    #[tokio::test]
    async fn backend_hits_over_the_cap_are_truncated_by_the_tool() {
        let backend = Arc::new(StubBackend::new(
            "Stub",
            vec![
                hit("One", "https://one", "1"),
                hit("Two", "https://two", "2"),
                hit("Three", "https://three", "3"),
            ],
        ));
        let result = run_tool(
            backend.clone(),
            &json!({ "query": "q", "max_results": 2 }),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.details["results"], json!(2));
        assert!(!text_of(&result).contains("Three"));
    }

    #[tokio::test]
    async fn an_empty_result_page_is_a_success_not_an_error() {
        let backend = Arc::new(StubBackend::new("Zhipu", Vec::new()));
        let result = run_tool(backend.clone(), &json!({ "query": "void" }), None)
            .await
            .unwrap();
        let text = text_of(&result);
        assert!(text.contains("No results."), "unexpected: {text}");
        assert_eq!(result.details["results"], json!(0));
        assert_eq!(backend.seen(), vec![("void".into(), 5)]);
    }

    #[tokio::test]
    async fn a_backend_error_surfaces_as_a_tool_error_verbatim() {
        let backend = Arc::new(StubBackend::failing(
            "Brave",
            "Brave API error: 429 rate limited",
        ));
        let error = run_tool(backend, &json!({ "query": "q" }), None)
            .await
            .unwrap_err();
        assert_eq!(error, "Brave API error: 429 rate limited");
    }

    #[tokio::test]
    async fn cancellation_races_the_run_token_at_the_tool_boundary() {
        let backend = Arc::new(StubBackend::hanging("Stub"));
        let cancel = CancellationToken::new();
        let task_token = cancel.clone();
        let params = json!({ "query": "slow" });
        let task = tokio::spawn(async move { run_tool(backend, &params, Some(&task_token)).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();

        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error, "search cancelled");
    }

    #[tokio::test]
    async fn a_backend_may_end_its_own_query_on_the_token() {
        let backend = TokenRacingBackend;
        let cancel = CancellationToken::new();
        let search = backend.search("q", 5, cancel.clone());
        tokio::pin!(search);
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut search)
            .await
            .expect_err("search finished without the token firing");
        cancel.cancel();
        let error = search.await.unwrap_err();
        assert_eq!(error, "cancelled by token");
    }

    #[tokio::test]
    async fn parameters_are_query_plus_optional_max_results() {
        let backend = Arc::new(StubBackend::new("Stub", Vec::new()));
        let missing = run_tool(backend.clone(), &json!({}), None)
            .await
            .unwrap_err();
        assert!(
            missing.contains("invalid web_search parameters"),
            "unexpected: {missing}"
        );

        let extra = run_tool(backend, &json!({ "query": "q", "sort": "date" }), None)
            .await
            .unwrap_err();
        assert!(extra.contains("unknown field"), "unexpected: {extra}");
    }

    #[test]
    fn description_names_the_contract() {
        for expected in [
            "web_fetch",
            "title, URL, and snippet",
            "default 5",
            "1–10",
            "backend's name",
            "capped at 5 MB",
            "50 KB",
            "truncation",
            "not cached",
            "never opens",
        ] {
            assert!(
                DESCRIPTION.contains(expected),
                "description is missing {expected:?}"
            );
        }
    }

    #[test]
    fn execution_tools_mount_web_search_only_with_a_backend() {
        let unconfigured = crate::tools::execution_tools(".");
        assert!(
            !unconfigured.iter().any(|tool| tool.name == "web_search"),
            "web_search mounted without a configured backend"
        );

        let configured = crate::tools::execution_tools_for_model(
            ".",
            true,
            Some(Arc::new(StubBackend::new("Stub", Vec::new()))),
            None,
        );
        let tool = configured
            .iter()
            .find(|tool| tool.name == "web_search")
            .expect("web_search is mounted with a configured backend");
        assert_eq!(tool.label, "Web Search");
        assert_eq!(tool.parameters["required"], json!(["query"]));
        assert_eq!(
            tool.parameters["properties"]["max_results"]["maximum"],
            json!(10)
        );
    }

    fn entry(kind: &str) -> crate::web_search_settings::WebSearchEntry {
        crate::web_search_settings::WebSearchEntry {
            id: kind.into(),
            kind: kind.into(),
            api_key: "sk-key".into(),
        }
    }

    #[tokio::test]
    async fn a_text_result_passes_through_under_the_header() {
        struct TextBackend;
        impl SearchBackend for TextBackend {
            fn name(&self) -> &str {
                "Exa"
            }
            fn search<'a>(
                &'a self,
                _query: &'a str,
                _max_results: usize,
                _cancel: CancellationToken,
            ) -> BoxFuture<'a, Result<SearchResults, String>> {
                Box::pin(async { Ok(SearchResults::Text("1. Holt — https://holt.dev".into())) })
            }
        }
        let result = run_tool(Arc::new(TextBackend), &json!({ "query": "holt" }), None)
            .await
            .unwrap();
        assert_eq!(
            text_of(&result),
            "Web search results from Exa for \"holt\"\n\n1. Holt — https://holt.dev"
        );
        assert_eq!(result.details["results"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_text_result_over_the_envelope_is_truncated_with_a_notice() {
        struct FloodBackend;
        impl SearchBackend for FloodBackend {
            fn name(&self) -> &str {
                "Exa"
            }
            fn search<'a>(
                &'a self,
                _query: &'a str,
                _max_results: usize,
                _cancel: CancellationToken,
            ) -> BoxFuture<'a, Result<SearchResults, String>> {
                Box::pin(async { Ok(SearchResults::Text("x".repeat(OUTPUT_BYTE_CAP + 4096))) })
            }
        }
        let result = run_tool(Arc::new(FloodBackend), &json!({ "query": "holt" }), None)
            .await
            .unwrap();
        let text = text_of(&result);
        assert_eq!(result.details["truncated"], json!(true));
        assert!(
            text.ends_with("[truncated: output capped at 50 KB]"),
            "unexpected tail"
        );
        assert!(
            text.len() <= OUTPUT_BYTE_CAP + TRUNCATION_NOTICE.len(),
            "text exceeds the envelope: {} bytes",
            text.len()
        );
    }

    #[tokio::test]
    async fn hit_fields_over_the_envelope_are_truncated_too() {
        // One hit whose snippet alone dwarfs the envelope — the count
        // clamp bounds hits, never their field sizes.
        let backend = Arc::new(StubBackend::new(
            "Stub",
            vec![hit(
                "Flood",
                "https://flood",
                &"s".repeat(OUTPUT_BYTE_CAP + 4096),
            )],
        ));
        let result = run_tool(backend, &json!({ "query": "q" }), None)
            .await
            .unwrap();
        let text = text_of(&result);
        assert_eq!(result.details["truncated"], json!(true));
        assert_eq!(result.details["results"], json!(1));
        assert!(
            text.ends_with("[truncated: output capped at 50 KB]"),
            "unexpected tail"
        );
        assert!(
            text.len() <= OUTPUT_BYTE_CAP + TRUNCATION_NOTICE.len(),
            "text exceeds the envelope: {} bytes",
            text.len()
        );
    }

    #[test]
    fn the_adapter_table_mounts_exactly_the_shipped_backends() {
        for backend in &BACKENDS {
            let mounted = adapter(&entry(backend.id), &[])
                .map(|adapter| adapter.name().to_string())
                .expect("every built-in backend mounts its adapter");
            assert_eq!(mounted, backend.name);
        }
        assert!(adapter(&entry("mcp"), &[]).is_none());
        assert!(BACKENDS.iter().any(|backend| backend.id == DEFAULT_BACKEND));
    }
}
