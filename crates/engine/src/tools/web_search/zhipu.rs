//! The Zhipu search backend adapter (ADR-0023) — the first built-in
//! [`SearchBackend`]. Endpoint and shapes are pinned to the BigModel Web
//! Search API reference (docs.bigmodel.cn → API 参考 → 工具 API → 网络搜索,
//! checked 2026-09-10): `POST https://open.bigmodel.cn/api/paas/v4/web_search`
//! with `Authorization: Bearer <key>`; the request carries `search_query`,
//! `search_engine` (this adapter uses `search_std`, Zhipu's own standard
//! engine), `search_intent: false`, and `count` (1–50; the tool has already
//! clamped it into 1–10); the reply's `search_result` items map
//! `title`/`link`/`content` onto [`SearchHit`]'s `title`/`url`/`snippet`,
//! and non-success replies carry `{"error": {"code", "message"}}`.
//!
//! The request runs under a 30 s timeout and races the Turn's
//! cancellation token; every failure names Zhipu so the model can tell
//! backends apart. Tests drive the adapter against the shared in-process
//! loopback server through the endpoint seam ([`ZhipuBackend::with_endpoint`])
//! — never the real API.

use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::{SearchBackend, SearchHit};
use crate::tools::USER_AGENT;

const ENDPOINT: &str = "https://open.bigmodel.cn/api/paas/v4/web_search";
/// Zhipu's own standard engine — the cheapest tier, and the one a coding
/// agent's ordinary lookups want (`search_pro` and the partner engines
/// are the upgrades).
const SEARCH_ENGINE: &str = "search_std";
/// Total budget for one query, request through decoded response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct ZhipuBackend {
    api_key: String,
    endpoint: String,
}

impl ZhipuBackend {
    pub(crate) fn new(api_key: String) -> Self {
        Self {
            api_key,
            endpoint: ENDPOINT.to_string(),
        }
    }

    /// The transport seam for the stubbed-HTTP tests: same request shape,
    /// retargeted at a loopback server.
    #[cfg(test)]
    pub(crate) fn with_endpoint(api_key: String, endpoint: String) -> Self {
        Self { api_key, endpoint }
    }

    /// One query under a total wall-clock budget — request through the
    /// last decoded body byte (the web_fetch precedent).
    async fn request(
        &self,
        query: &str,
        max_results: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchHit>, String> {
        tokio::time::timeout(timeout, self.exchange(query, max_results))
            .await
            .map_err(|_| format!("Zhipu search timed out after {:.0}s", timeout.as_secs_f64()))?
    }

    async fn exchange(&self, query: &str, max_results: usize) -> Result<Vec<SearchHit>, String> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| format!("Zhipu search could not start: {error}"))?;
        let response = client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&ZhipuRequest {
                search_query: query,
                search_engine: SEARCH_ENGINE,
                search_intent: false,
                count: max_results,
            })
            .send()
            .await
            .map_err(|error| format!("Zhipu search request failed: {error}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| format!("Zhipu search failed reading the response: {error}"))?;
        if !status.is_success() {
            // The API documents `{"error": {"code", "message"}}` on
            // failures; quote both when they are there, the bare status
            // when they are not.
            let detail = serde_json::from_slice::<ZhipuErrorBody>(&body)
                .ok()
                .map(|error| {
                    // The docs type the code as a string; decode it
                    // leniently so a numeric code cannot nuke the body
                    // (and drop the message with it). A string prints
                    // bare, anything else keeps its JSON rendering.
                    let code = error.error.code.map(|value| match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    });
                    match (code.as_deref(), error.error.message.is_empty()) {
                        (Some(""), false) | (None, false) => {
                            format!(": {}", error.error.message)
                        }
                        (Some(""), true) | (None, true) => String::new(),
                        (Some(code), false) => format!(" (code {code}): {}", error.error.message),
                        (Some(code), true) => format!(" (code {code})"),
                    }
                })
                .unwrap_or_default();
            return Err(format!("Zhipu search failed: HTTP {status}{detail}"));
        }
        let decoded: ZhipuResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("could not decode the Zhipu search response: {error}"))?;
        Ok(decoded
            .search_result
            .into_iter()
            .map(|hit| SearchHit {
                title: hit.title,
                url: hit.link,
                snippet: hit.content,
            })
            .collect())
    }
}

impl SearchBackend for ZhipuBackend {
    fn name(&self) -> &str {
        "Zhipu"
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        max_results: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>> {
        // The request carries its own timeout; dropping it on
        // cancellation aborts the in-flight HTTP call.
        Box::pin(async move {
            let request = self.request(query, max_results, REQUEST_TIMEOUT);
            tokio::select! {
                _ = cancel.cancelled() => Err("search cancelled".to_string()),
                result = request => result,
            }
        })
    }
}

#[derive(serde::Serialize)]
struct ZhipuRequest<'a> {
    search_query: &'a str,
    search_engine: &'static str,
    search_intent: bool,
    count: usize,
}

#[derive(serde::Deserialize)]
struct ZhipuResponse {
    /// Absent on an empty result page — that is a success, not an error.
    #[serde(default)]
    search_result: Vec<ZhipuResult>,
}

#[derive(serde::Deserialize)]
struct ZhipuResult {
    title: String,
    link: String,
    content: String,
}

#[derive(serde::Deserialize)]
struct ZhipuErrorBody {
    error: ZhipuError,
}

#[derive(serde::Deserialize)]
struct ZhipuError {
    /// The docs type it as a string, but a numeric code must not fail
    /// the decode (and drop the message with it) — a raw value accepts
    /// both.
    #[serde(default)]
    code: Option<serde_json::Value>,
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_http::{Server, response, serve};
    use serde_json::json;

    fn hits_page() -> Vec<u8> {
        // The documented response shape, verbatim field for field.
        json!({
            "created": 1748261757,
            "id": "csid-1",
            "request_id": "861091",
            "search_intent": [],
            "search_result": [
                {
                    "title": "Async Rust",
                    "link": "https://example.com/async",
                    "content": "Async in Rust, explained.",
                    "media": "example.com",
                    "icon": "https://example.com/favicon.ico",
                    "publish_date": "2026-01-02",
                    "refer": "ref_1"
                },
                {
                    "title": "Tokio",
                    "link": "https://tokio.rs",
                    "content": "The async runtime.",
                    "media": "tokio.rs",
                    "icon": "",
                    "publish_date": "",
                    "refer": "ref_2"
                }
            ]
        })
        .to_string()
        .into_bytes()
    }

    /// The adapter aimed at a loopback server serving the documented path.
    fn stub(server: &Server) -> ZhipuBackend {
        ZhipuBackend::with_endpoint(
            "sk-test-key".into(),
            format!("{}/api/paas/v4/web_search", server.base),
        )
    }

    #[tokio::test]
    async fn maps_results_and_pins_the_request_shape() {
        let server = serve(|_, _| response("200 OK", "application/json", &hits_page())).await;

        let hits = stub(&server)
            .request("rust async", 2, REQUEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            hits,
            vec![
                SearchHit {
                    title: "Async Rust".into(),
                    url: "https://example.com/async".into(),
                    snippet: "Async in Rust, explained.".into(),
                },
                SearchHit {
                    title: "Tokio".into(),
                    url: "https://tokio.rs".into(),
                    snippet: "The async runtime.".into(),
                },
            ]
        );

        let head = &server.requests()[0];
        assert!(
            head.starts_with("POST /api/paas/v4/web_search HTTP/1.1"),
            "unexpected: {head}"
        );
        assert!(
            head.contains("authorization: Bearer sk-test-key\r\n"),
            "missing bearer auth: {head}"
        );
        assert!(
            head.contains(&format!("user-agent: {USER_AGENT}\r\n")),
            "missing user agent: {head}"
        );
        let body: serde_json::Value = serde_json::from_slice(&server.bodies()[0]).unwrap();
        assert_eq!(
            body,
            json!({
                "search_query": "rust async",
                "search_engine": "search_std",
                "search_intent": false,
                "count": 2,
            })
        );
    }

    #[tokio::test]
    async fn an_empty_result_page_is_an_empty_success() {
        for body in [
            json!({ "search_result": [] }).to_string().into_bytes(),
            json!({ "created": 1748261757 }).to_string().into_bytes(),
        ] {
            let server =
                serve(move |_, _| response("200 OK", "application/json", &body.clone())).await;
            assert_eq!(
                stub(&server)
                    .request("void", 5, REQUEST_TIMEOUT)
                    .await
                    .unwrap(),
                Vec::new()
            );
        }
    }

    #[tokio::test]
    async fn an_api_error_names_zhipu_code_and_message() {
        // The docs type the code as a string; a numeric code must survive
        // the decode too (it cannot be allowed to drop the message).
        for body in [
            r#"{"error":{"code":"1301","message":"并发上限"}}"#,
            r#"{"error":{"code":1301,"message":"并发上限"}}"#,
        ] {
            let body = body.to_string();
            let server = serve(move |_, _| {
                response("429 Too Many Requests", "application/json", body.as_bytes())
            })
            .await;

            let error = stub(&server)
                .request("q", 5, REQUEST_TIMEOUT)
                .await
                .unwrap_err();
            assert!(
                error.contains("Zhipu search failed: HTTP 429"),
                "unexpected: {error}"
            );
            assert!(error.contains("code 1301"), "unexpected: {error}");
            assert!(error.contains("并发上限"), "unexpected: {error}");
        }
    }

    #[tokio::test]
    async fn an_unparseable_error_body_still_names_the_status() {
        let server =
            serve(|_, _| response("502 Bad Gateway", "text/html", b"<html>proxy</html>")).await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, "Zhipu search failed: HTTP 502 Bad Gateway");
    }

    #[tokio::test]
    async fn a_success_with_a_garbage_body_fails_to_decode() {
        let server = serve(|_, _| response("200 OK", "application/json", b"not json")).await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert!(
            error.contains("could not decode the Zhipu search response"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn a_hanging_request_times_out() {
        let server = serve(|_, _| Vec::new()).await;

        let error = stub(&server)
            .request("q", 5, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(
            error.contains("Zhipu search timed out after"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn cancellation_races_the_token_through_the_trait() {
        let server = serve(|_, _| Vec::new()).await;
        let backend = stub(&server);
        let cancel = CancellationToken::new();
        let task_token = cancel.clone();
        let task = tokio::spawn(async move { backend.search("slow", 5, task_token).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        assert_eq!(task.await.unwrap().unwrap_err(), "search cancelled");
    }

    #[test]
    fn the_backend_names_itself() {
        assert_eq!(ZhipuBackend::new("k".into()).name(), "Zhipu");
    }
}
