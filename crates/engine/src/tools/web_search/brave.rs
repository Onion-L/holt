//! The Brave search backend adapter (ADR-0023) — the third built-in
//! [`SearchBackend`], the international/pi-ecosystem option (the picker
//! notes it needs international access). Endpoint and shapes are pinned
//! to Brave's Search API reference (api-dashboard.search.brave.com,
//! Web Search endpoint, checked 2026-09-10):
//! `GET https://api.search.brave.com/res/v1/web/search` with the
//! `X-Subscription-Token` header and `Accept: application/json`; the
//! query string carries `q` and `count` (max 20 — the tool has already
//! clamped it into 1–10); the reply's `web.results[]` maps
//! `title`/`url`/`description` onto [`SearchHit`]'s title/url/snippet —
//! an absent `web` is the empty result page. Documented failures (404,
//! 422, 429) carry `{"error": {"code", "detail"?}}`.
//!
//! The request runs under a 30 s budget covering the whole exchange and
//! races the Turn's cancellation token; every failure names Brave so the
//! model can tell backends apart. Tests drive the adapter against the
//! shared in-process loopback server through the endpoint seam
//! ([`BraveBackend::with_endpoint`]) — never the real API.

use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::{SearchBackend, SearchHit, transport};

const NAME: &str = "Brave";
const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// Total budget for one query, request through the last decoded body
/// byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct BraveBackend {
    api_key: String,
    endpoint: String,
}

impl BraveBackend {
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
                .get(&self.endpoint)
                .header("X-Subscription-Token", &self.api_key)
                .header(reqwest::header::ACCEPT, "application/json")
                .query(&[("q", query)])
                .query(&[("count", max_results)])
        })
        .await?;
        if !status.is_success() {
            // Brave documents `{"error": {"code", "detail"?}}` on 404/422/
            // 429 — `detail` is the human-readable field (there is no
            // `message`).
            let detail = serde_json::from_slice::<BraveErrorBody>(&body)
                .ok()
                .map(|error| transport::error_detail(Some(error.error.code), error.error.detail))
                .unwrap_or_default();
            return Err(format!("{NAME} search failed: HTTP {status}{detail}"));
        }
        let decoded: BraveResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("could not decode the Brave search response: {error}"))?;
        Ok(decoded
            .web
            .map(|web| web.results)
            .unwrap_or_default()
            .into_iter()
            .map(|result| SearchHit {
                title: result.title,
                url: result.url,
                snippet: result.description.unwrap_or_default(),
            })
            .collect())
    }
}

impl SearchBackend for BraveBackend {
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

#[derive(serde::Deserialize)]
struct BraveResponse {
    /// Absent (or null) on an empty result page — that is a success, not
    /// an error.
    web: Option<BraveWeb>,
}

#[derive(serde::Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(serde::Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    /// The documented default is an empty string; a missing or null
    /// field maps to an empty snippet rather than failing the page.
    #[serde(default)]
    description: Option<String>,
}

#[derive(serde::Deserialize)]
struct BraveErrorBody {
    error: BraveError,
}

#[derive(serde::Deserialize)]
struct BraveError {
    /// An application-specific code, typed as a string by the docs; a
    /// missing code contributes nothing rather than failing the body.
    #[serde(default)]
    code: String,
    #[serde(default)]
    detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::USER_AGENT;
    use crate::tools::test_http::{Server, response, serve};
    use serde_json::json;

    fn results_page() -> Vec<u8> {
        // The documented response shape's load-bearing fields, verbatim.
        json!({
            "type": "search",
            "query": {
                "original": "rust async",
                "more_results_available": true
            },
            "web": {
                "type": "search",
                "results": [
                    {
                        "title": "Async Rust",
                        "url": "https://example.com/async",
                        "description": "Async in Rust, explained.",
                        "page_age": "2026-01-02",
                        "language": "en",
                        "family_friendly": true,
                        "type": "search_result"
                    },
                    {
                        "title": "Tokio",
                        "url": "https://tokio.rs",
                        "description": "The async runtime.",
                        "meta_url": null,
                        "age": null
                    }
                ]
            }
        })
        .to_string()
        .into_bytes()
    }

    /// The adapter aimed at a loopback server serving the documented path.
    fn stub(server: &Server) -> BraveBackend {
        BraveBackend::with_endpoint(
            "sk-test-key".into(),
            format!("{}/res/v1/web/search", server.base),
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
            head.starts_with("GET /res/v1/web/search?q=rust+async&count=3 HTTP/1.1"),
            "unexpected: {head}"
        );
        assert!(
            head.contains("x-subscription-token: sk-test-key\r\n"),
            "missing subscription token: {head}"
        );
        assert!(
            head.contains("accept: application/json\r\n"),
            "missing accept header: {head}"
        );
        assert!(
            head.contains(&format!("user-agent: {USER_AGENT}\r\n")),
            "missing user agent: {head}"
        );
    }

    #[tokio::test]
    async fn an_empty_result_page_is_an_empty_success() {
        for body in [
            json!({ "type": "search", "web": { "results": [] } }),
            json!({ "type": "search" }),
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
    async fn a_result_without_a_description_maps_to_an_empty_snippet() {
        let server = serve(|_, _| {
            response(
                "200 OK",
                "application/json",
                br#"{"web":{"results":[{"title":"Bare","url":"https://bare"}]}}"#,
            )
        })
        .await;

        let hits = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            hits,
            vec![SearchHit {
                title: "Bare".into(),
                url: "https://bare".into(),
                snippet: String::new(),
            }]
        );
    }

    #[tokio::test]
    async fn a_documented_error_body_names_code_and_detail() {
        let body = r#"{
            "type": "ErrorResponse",
            "error": {
                "id": "6f4c1a9e",
                "status": 429,
                "code": "RATE_LIMITED",
                "detail": "Request rate limit exceeded."
            },
            "time": 1748261757
        }"#;
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
            error.contains(
                "HTTP 429 Too Many Requests (code RATE_LIMITED): Request rate limit exceeded."
            ),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn a_code_without_detail_still_quotes_the_code() {
        let server = serve(|_, _| {
            response(
                "422 Unprocessable Entity",
                "application/json",
                br#"{"error":{"code":"VALIDATION","detail":null}}"#,
            )
        })
        .await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Brave search failed: HTTP 422 Unprocessable Entity (code VALIDATION)"
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
        assert_eq!(error, "Brave search failed: HTTP 502 Bad Gateway");
    }

    #[tokio::test]
    async fn a_success_with_a_garbage_body_fails_to_decode() {
        let server = serve(|_, _| response("200 OK", "application/json", b"not json")).await;

        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert!(
            error.contains("could not decode the Brave search response"),
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
            error.contains("Brave search timed out after"),
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
