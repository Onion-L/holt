//! The Bocha search backend adapter (ADR-0023) — the second built-in
//! [`SearchBackend`]. Endpoint and shapes are pinned to Bocha's own
//! example request/response (open.bochaai.com, the Bing-compatible Web
//! Search API; full reference on their Feishu wiki, checked 2026-09-10):
//! `POST https://api.bochaai.com/v1/web-search` with
//! `Authorization: Bearer <key>`; the request carries `query` (required)
//! and `count` (1–50, default 10; the tool has already clamped it into
//! 1–10 — `freshness`, `summary`, and the domain filters are all
//! optional and stay at their defaults); the reply nests
//! `webPages.value[]` whose `name`/`url`/`snippet` map straight onto
//! [`SearchHit`] — an absent `webPages` is the empty result page, the
//! Bing convention. Bocha does not document an error body, so failures
//! surface the HTTP status and quote a flat `{"code", "message"}` body
//! when one is there.
//!
//! The request runs under a 30 s budget covering the whole exchange and
//! races the Turn's cancellation token; every failure names Bocha so the
//! model can tell backends apart. Tests drive the adapter against the
//! shared in-process loopback server through the endpoint seam
//! ([`BochaBackend::with_endpoint`]) — never the real API.

use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::{SearchBackend, SearchHit, transport};

const NAME: &str = "Bocha";
const ENDPOINT: &str = "https://api.bochaai.com/v1/web-search";
/// Total budget for one query, request through the last decoded body
/// byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct BochaBackend {
    api_key: String,
    endpoint: String,
}

impl BochaBackend {
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

    async fn request(
        &self,
        query: &str,
        max_results: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchHit>, String> {
        let (status, body) = transport::send_bounded(NAME, timeout, |client| {
            client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&BochaRequest {
                    query,
                    count: max_results,
                })
        })
        .await?;
        if !status.is_success() {
            let (code, message) = flat_error_pieces(&body);
            return Err(format!(
                "{NAME} search failed: HTTP {status}{}",
                transport::error_detail(code, message)
            ));
        }
        let decoded: BochaResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("could not decode the Bocha search response: {error}"))?;
        Ok(decoded
            .web_pages
            .unwrap_or_default()
            .value
            .into_iter()
            .map(|hit| SearchHit {
                title: hit.name,
                url: hit.url,
                snippet: hit.snippet,
            })
            .collect())
    }
}

impl SearchBackend for BochaBackend {
    fn name(&self) -> &str {
        NAME
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        max_results: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>> {
        Box::pin(transport::race_cancel(
            self.request(query, max_results, REQUEST_TIMEOUT),
            cancel,
        ))
    }
}

/// Pull the `code`/`message` pieces out of an undocumented-but-common
/// flat error body; anything else contributes nothing and the status
/// stands alone. A string code prints bare; anything else (a number,
/// say) keeps its JSON rendering.
fn flat_error_pieces(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let message = parsed
        .get("message")
        .and_then(|message| message.as_str())
        .map(str::to_string);
    let code = parsed.get("code").map(|code| match code {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    (code, message)
}

#[derive(serde::Serialize)]
struct BochaRequest<'a> {
    query: &'a str,
    count: usize,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BochaResponse {
    /// Absent on an empty result page (the Bing convention) — that is a
    /// success, not an error.
    web_pages: Option<WebPages>,
}

#[derive(serde::Deserialize, Default)]
struct WebPages {
    #[serde(default)]
    value: Vec<WebPage>,
}

#[derive(serde::Deserialize)]
struct WebPage {
    name: String,
    url: String,
    snippet: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::USER_AGENT;
    use crate::tools::test_http::{Server, response, serve};
    use serde_json::json;

    fn results_page() -> Vec<u8> {
        // The documented response shape, verbatim field for field.
        json!({
            "_type": "SearchResponse",
            "queryContext": { "originalQuery": "rust async" },
            "webPages": {
                "webSearchUrl": "https://bocha.cn/search?q=rust+async",
                "totalEstimatedMatches": 2380000,
                "value": [
                    {
                        "id": "https://example.com/async",
                        "name": "Async Rust",
                        "url": "https://example.com/async",
                        "siteName": "example.com",
                        "siteIcon": "https://example.com/favicon.ico",
                        "snippet": "Async in Rust, explained.",
                        "summary": "A longer machine summary when summary=true.",
                        "datePublished": "2026-01-02T00:00:00+08:00"
                    },
                    {
                        "id": "https://tokio.rs",
                        "name": "Tokio",
                        "url": "https://tokio.rs",
                        "siteName": "tokio.rs",
                        "siteIcon": "",
                        "snippet": "The async runtime.",
                        "summary": "",
                        "datePublished": ""
                    }
                ]
            }
        })
        .to_string()
        .into_bytes()
    }

    /// The adapter aimed at a loopback server serving the documented path.
    fn stub(server: &Server) -> BochaBackend {
        BochaBackend::with_endpoint(
            "sk-test-key".into(),
            format!("{}/v1/web-search", server.base),
        )
    }

    #[tokio::test]
    async fn maps_results_and_pins_the_request_shape() {
        let server = serve(|_, _| response("200 OK", "application/json", &results_page())).await;

        let hits = stub(&server)
            .request("rust async", 3, REQUEST_TIMEOUT)
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
            head.starts_with("POST /v1/web-search HTTP/1.1"),
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
        assert_eq!(body, json!({ "query": "rust async", "count": 3 }));
    }

    #[tokio::test]
    async fn an_empty_result_page_is_an_empty_success() {
        for body in [
            json!({ "_type": "SearchResponse", "webPages": { "value": [] } }),
            json!({ "_type": "SearchResponse" }),
        ] {
            let body = body.to_string().into_bytes();
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
    async fn an_error_body_names_bocha_code_and_message() {
        // The error body is undocumented; the adapter quotes the common
        // flat shape and tolerates string or numeric codes.
        for body in [
            r#"{"code":429,"message":"Too Many Requests"}"#,
            r#"{"code":"429","message":"Too Many Requests"}"#,
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
                error.contains("HTTP 429 Too Many Requests (code 429): Too Many Requests"),
                "unexpected: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_code_without_a_message_still_quotes_the_code() {
        let server =
            serve(|_, _| response("401 Unauthorized", "application/json", br#"{"code":401}"#))
                .await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Bocha search failed: HTTP 401 Unauthorized (code 401)"
        );
    }

    #[tokio::test]
    async fn an_unparseable_error_body_still_names_the_status() {
        let server =
            serve(|_, _| response("502 Bad Gateway", "text/html", b"<html>proxy</html>")).await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, "Bocha search failed: HTTP 502 Bad Gateway");
    }

    #[tokio::test]
    async fn a_success_with_a_garbage_body_fails_to_decode() {
        let server = serve(|_, _| response("200 OK", "application/json", b"not json")).await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert!(
            error.contains("could not decode the Bocha search response"),
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
            error.contains("Bocha search timed out after"),
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
}
