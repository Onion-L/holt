//! The Exa search backend adapter (ADR-0023) — the keyless default.
//! Exa's hosted MCP endpoint answers a stateless JSON-RPC `tools/call`
//! without an API key or an `initialize` handshake (checked 2026-09-30):
//! `POST https://mcp.exa.ai/mcp` with `Accept: application/json,
//! text/event-stream` (JSON alone is refused with 406) and
//! `{"method": "tools/call", "params": {"name": "web_search_exa",
//! "arguments": {"query", "numResults"}}}`. The reply is one SSE
//! `data:` line (or a plain JSON body) carrying a JSON-RPC response whose
//! `result.content[]` text blocks are already a readable result list —
//! passed to the model as [`SearchResults::Text`]. Failures arrive as a
//! JSON-RPC `error` or as `result.isError` with the reason as text.
//!
//! The request runs under a 30 s budget covering the whole exchange and
//! races the Turn's cancellation token. Tests drive the adapter against
//! the shared loopback server through [`ExaBackend::with_endpoint`].

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{SearchBackend, SearchResults, transport};

const NAME: &str = "Exa";
const ENDPOINT: &str = "https://mcp.exa.ai/mcp";
const TOOL: &str = "web_search_exa";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct ExaBackend {
    endpoint: String,
}

impl ExaBackend {
    pub(crate) fn new() -> Self {
        Self {
            endpoint: ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(endpoint: String) -> Self {
        Self { endpoint }
    }

    async fn request(
        &self,
        query: &str,
        max_results: usize,
        timeout: Duration,
    ) -> Result<String, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": TOOL,
                "arguments": { "query": query, "numResults": max_results },
            },
        });
        let (status, body) = transport::send_bounded(NAME, timeout, |client| {
            client
                .post(&self.endpoint)
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .json(&body)
        })
        .await?;
        if !status.is_success() {
            let detail = rpc_response(&body)
                .and_then(|reply| reply.error)
                .map(|error| transport::error_detail(None, Some(error.message)))
                .unwrap_or_default();
            return Err(format!("{NAME} search failed: HTTP {status}{detail}"));
        }
        let reply = rpc_response(&body)
            .ok_or_else(|| "could not decode the Exa search response".to_string())?;
        if let Some(error) = reply.error {
            return Err(format!("{NAME} search failed: {}", error.message));
        }
        let result = reply
            .result
            .ok_or_else(|| "the Exa search response carried no result".to_string())?;
        let text = result
            .content
            .into_iter()
            .filter(|block| block.kind == "text")
            .map(|block| block.text)
            .collect::<Vec<_>>()
            .join("\n\n");
        if result.is_error {
            return Err(format!("{NAME} search failed: {text}"));
        }
        Ok(if text.trim().is_empty() {
            "No results.".to_string()
        } else {
            text
        })
    }
}

/// The JSON-RPC response in an SSE stream (the first `data:` line that
/// decodes) or a plain JSON body.
fn rpc_response(body: &[u8]) -> Option<RpcResponse> {
    let body = String::from_utf8_lossy(body);
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .find_map(|data| serde_json::from_str(data.trim()).ok())
        .or_else(|| serde_json::from_str(&body).ok())
}

impl SearchBackend for ExaBackend {
    fn name(&self) -> &str {
        NAME
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        max_results: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<SearchResults, String>> {
        Box::pin(async move {
            transport::race_cancel(self.request(query, max_results, REQUEST_TIMEOUT), cancel)
                .await
                .map(SearchResults::Text)
        })
    }
}

#[derive(serde::Deserialize)]
struct RpcResponse {
    result: Option<ToolResult>,
    error: Option<RpcError>,
}

#[derive(serde::Deserialize)]
struct RpcError {
    #[serde(default)]
    message: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolResult {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    is_error: bool,
}

#[derive(serde::Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_http::{Server, response, serve};

    fn stub(server: &Server) -> ExaBackend {
        ExaBackend::with_endpoint(format!("{}/mcp", server.base))
    }

    fn sse(payload: serde_json::Value) -> Vec<u8> {
        format!("event: message\ndata: {payload}\n\n").into_bytes()
    }

    #[tokio::test]
    async fn passes_the_text_through_and_pins_the_request_shape() {
        let server = serve(|_, _| {
            response(
                "200 OK",
                "text/event-stream",
                &sse(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": { "content": [
                        { "type": "text", "text": "Title: Tokio\nURL: https://tokio.rs" }
                    ]}
                })),
            )
        })
        .await;

        let text = stub(&server)
            .request("rust async", 3, REQUEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(text, "Title: Tokio\nURL: https://tokio.rs");

        let head = &server.requests()[0];
        assert!(head.starts_with("POST /mcp HTTP/1.1"), "unexpected: {head}");
        assert!(
            head.contains("accept: application/json, text/event-stream\r\n"),
            "missing accept header: {head}"
        );
        let body: serde_json::Value = serde_json::from_slice(&server.bodies()[0]).unwrap();
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], TOOL);
        assert_eq!(
            body["params"]["arguments"],
            json!({ "query": "rust async", "numResults": 3 })
        );
    }

    #[tokio::test]
    async fn a_plain_json_reply_decodes_too() {
        let server = serve(|_, _| {
            response(
                "200 OK",
                "application/json",
                br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hit"}]}}"#,
            )
        })
        .await;
        assert_eq!(
            stub(&server)
                .request("q", 5, REQUEST_TIMEOUT)
                .await
                .unwrap(),
            "hit"
        );
    }

    #[tokio::test]
    async fn tool_and_rpc_errors_name_exa() {
        let server = serve(|_, _| {
            response(
                "200 OK",
                "text/event-stream",
                &sse(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "content": [{ "type": "text", "text": "rate limited" }],
                        "isError": true
                    }
                })),
            )
        })
        .await;
        assert_eq!(
            stub(&server)
                .request("q", 5, REQUEST_TIMEOUT)
                .await
                .unwrap_err(),
            "Exa search failed: rate limited"
        );

        let server = serve(|_, _| {
            response(
                "200 OK",
                "text/event-stream",
                &sse(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": { "code": -32602, "message": "bad params" }
                })),
            )
        })
        .await;
        assert_eq!(
            stub(&server)
                .request("q", 5, REQUEST_TIMEOUT)
                .await
                .unwrap_err(),
            "Exa search failed: bad params"
        );
    }

    #[tokio::test]
    async fn an_http_failure_names_the_status() {
        let server =
            serve(|_, _| response("502 Bad Gateway", "text/html", b"<html>proxy</html>")).await;
        assert_eq!(
            stub(&server)
                .request("q", 5, REQUEST_TIMEOUT)
                .await
                .unwrap_err(),
            "Exa search failed: HTTP 502 Bad Gateway"
        );
    }

    #[tokio::test]
    async fn a_garbage_body_fails_to_decode() {
        let server = serve(|_, _| response("200 OK", "text/plain", b"not json")).await;
        let error = stub(&server)
            .request("q", 5, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert!(
            error.contains("could not decode the Exa search response"),
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
