//! The agent's `web_search` tool plus the [`SearchBackend`] contract it
//! sits on (ADR-0023). One query in, a title/url/snippet list out — the
//! tool finds pages and never reads them (that is `web_fetch`'s job), and
//! it exists only when the user has configured a backend: with none, the
//! tool is absent from the model's toolset, never registered-and-erroring.
//!
//! The backend is user-chosen (Zhipu, Bocha, Brave — one adapter module
//! each, sharing the [`transport`] scaffolding); this module owns the
//! trait, the tool, the output shape, and the built-in adapter table. The
//! tool races the run's cancellation token around the backend call,
//! exactly like grep and web_fetch.

mod bocha;
mod brave;
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
    /// Settings-group copy flagging an access requirement (Brave needs
    /// international access); `None` for backends with nothing to flag.
    pub(crate) note: Option<&'static str>,
}

/// The launch backends (ADR-0023) — the save RPC's validation set and
/// the Settings picker's option list. Every id here mounts its adapter
/// through [`builtin`].
pub(crate) const BACKENDS: [Backend; 3] = [
    Backend {
        id: "zhipu",
        name: "Zhipu",
        note: None,
    },
    Backend {
        id: "bocha",
        name: "Bocha",
        note: None,
    },
    Backend {
        id: "brave",
        name: "Brave",
        note: Some("Needs international access"),
    },
];

/// The built-in adapter table behind the engine's Turn-admission
/// resolution (the injected test resolver aside). `None` for an unknown
/// id.
pub(crate) fn builtin(id: &str, api_key: &str) -> Option<Arc<dyn SearchBackend>> {
    match id {
        "zhipu" => Some(Arc::new(zhipu::ZhipuBackend::new(api_key.to_string()))),
        "bocha" => Some(Arc::new(bocha::BochaBackend::new(api_key.to_string()))),
        "brave" => Some(Arc::new(brave::BraveBackend::new(api_key.to_string()))),
        _ => None,
    }
}

/// Hit count used when the model omits `max_results`.
const DEFAULT_MAX_RESULTS: usize = 5;
/// Lower clamp for `max_results`: a zero ask yields one hit, not an error
/// (a negative ask fails deserialization as an invalid parameter).
const MIN_MAX_RESULTS: usize = 1;
/// Upper clamp for `max_results`.
const MAX_MAX_RESULTS: usize = 10;

const DESCRIPTION: &str = "Search the web via the configured search backend and return a \
numbered list of results, each with a title, URL, and snippet. Parameters: `query` (required) and \
optional `max_results` (default 5, clamped to 1–10). The backend's name is stated in the output \
header. This tool only finds pages — it never opens, crawls, or summarizes them; fetch a result's \
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

/// The pluggable web-search service behind the `web_search` tool: the
/// user's Settings choice, carried as its own key (never a provider
/// credential). Implemented by the backend adapters; mounted only when
/// configured.
pub trait SearchBackend: Send + Sync {
    /// The backend's display name — the tool's output header states it so
    /// the model knows where its results came from, and errors name it.
    fn name(&self) -> &str;
    /// Run one query, returning at most `max_results` hits (already
    /// clamped by the tool). `cancel` fires when the owning Turn ends;
    /// adapters race their request against it.
    fn search<'a>(
        &'a self,
        query: &'a str,
        max_results: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>>;
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
    hits: &[SearchHit],
) -> AgentToolResult {
    AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: render(backend, query, hits),
            ..Default::default()
        })],
        details: json!({
            "backend": backend,
            "query": query,
            "max_results": max_results,
            "results": hits.len(),
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
                let mut hits = tokio::select! {
                    _ = cancel.cancelled() => return Err("search cancelled".to_string()),
                    result = search => result?,
                };
                hits.truncate(max_results);
                Ok(into_result(
                    backend.name(),
                    &input.query,
                    max_results,
                    &hits,
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
        ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>> {
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
                Ok(self.hits[..].to_vec())
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
        ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>> {
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

    #[test]
    fn the_builtin_table_mounts_exactly_the_shipped_backends() {
        for backend in &BACKENDS {
            let mounted = builtin(backend.id, "sk-key")
                .map(|adapter| adapter.name().to_string())
                .expect("every launch backend mounts its adapter");
            assert_eq!(mounted, backend.name);
        }
        assert!(builtin("nope", "sk-key").is_none());
    }
}
