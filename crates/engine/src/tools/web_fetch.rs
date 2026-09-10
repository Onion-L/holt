//! The agent's `web_fetch` tool: retrieve one http(s) URL and hand back its
//! readable content (ADR-0023). HTML becomes Markdown via htmd, other text
//! content types pass through raw, and PDFs, images, and other binary
//! responses are reported as typed errors. Full text only — the content is
//! never summarized.
//!
//! Bounds follow the [`super::grep`] precedent: the download is capped against
//! both `Content-Length` and the streamed body, and the text handed back to the
//! model sits in a byte envelope with an in-band truncation notice (no
//! spill-to-disk). The request races the run's cancellation token, so a
//! cancelled Turn drops the in-flight request.

use std::{sync::Arc, time::Duration};

use futures::{StreamExt, future::BoxFuture};
use htmd::HtmlToMarkdown;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Byte envelope for the text handed back to the model.
const OUTPUT_BYTE_CAP: usize = 50 * 1024;
/// Hard download cap, checked against `Content-Length` and streamed bytes.
const DOWNLOAD_BYTE_CAP: u64 = 5 * 1024 * 1024;
/// Total budget for a fetch, headers through last body byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Redirect hops followed before the request fails (cross-host allowed).
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = concat!("holt/", env!("CARGO_PKG_VERSION"));

const DESCRIPTION: &str = "Fetch one HTTP(S) URL and return its content as text. HTML is \
converted to Markdown; other text content types (JSON, plain text, CSV, XML…) pass through \
unchanged; PDF, image, and other binary responses cannot be read and return an error naming \
the content type and size. Downloads are capped at 5 MB, the returned text at 50 KB (a \
truncation notice is included), and the request times out after 30s. Non-success HTTP statuses are errors. Redirects are followed up \
to 10 hops — cross-host allowed — and the final URL is reported in the output. http:// and \
https:// both work, with no host filtering: `http://localhost:3000` is a first-class target. \
Responses are not cached. The only parameter is `url`.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebFetchInput {
    url: String,
}

/// How a response body is interpreted, from its content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    /// Converted to Markdown.
    Html,
    /// Returned verbatim (decoded as UTF-8).
    Text,
    /// Rejected with a typed notice naming the content type and size.
    Binary,
}

fn classify(content_type: Option<&str>) -> BodyKind {
    let Some(content_type) = content_type else {
        // Many dev servers omit the header; pass the bytes through rather
        // than declaring them unreadable.
        return BodyKind::Text;
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime.is_empty() {
        return BodyKind::Text;
    }
    if mime == "text/html" || mime == "application/xhtml+xml" {
        return BodyKind::Html;
    }
    if mime.starts_with("text/") || mime.ends_with("+json") || mime.ends_with("+xml") {
        return BodyKind::Text;
    }
    match mime.as_str() {
        "application/json"
        | "application/xml"
        | "application/javascript"
        | "application/x-javascript"
        | "application/graphql"
        | "application/x-ndjson"
        | "application/yaml"
        | "application/x-yaml"
        | "application/x-www-form-urlencoded" => BodyKind::Text,
        _ => BodyKind::Binary,
    }
}

/// The typed failures of a fetch. Every variant carries enough context to
/// tell the model what happened without a second call.
#[derive(Debug)]
enum FetchError {
    /// The client itself could not be constructed (TLS backend, config).
    Client(String),
    /// The URL is missing, malformed, or uses a scheme other than http(s).
    Request(String),
    /// A response arrived with a non-success status.
    Status { status: u16, url: String },
    /// The body exceeds [`DOWNLOAD_BYTE_CAP`]; `declared` is the
    /// `Content-Length` when the cap was caught before streaming.
    TooLarge { declared: Option<u64> },
    /// More than [`MAX_REDIRECTS`] hops.
    TooManyRedirects,
    /// The content type is not text or HTML.
    Unsupported {
        content_type: Option<String>,
        bytes: u64,
    },
    /// The whole fetch did not finish in time.
    Timeout { timeout: Duration },
    /// The run's cancellation token fired while the request was in flight.
    Cancelled,
    /// HTML→Markdown failed — the parser, or the blocking pool it runs on.
    Convert(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Client(message) => {
                write!(formatter, "could not start the fetch: {message}")
            }
            FetchError::Request(message) => write!(formatter, "request failed: {message}"),
            FetchError::Status { status, url } => {
                write!(formatter, "GET {url} returned HTTP {status}")
            }
            FetchError::TooLarge { declared } => match declared {
                Some(bytes) => write!(
                    formatter,
                    "response is larger than the 5 MB download cap ({bytes} bytes declared by Content-Length)"
                ),
                None => write!(
                    formatter,
                    "response is larger than the 5 MB download cap (stopped after {DOWNLOAD_BYTE_CAP} bytes)"
                ),
            },
            FetchError::TooManyRedirects => write!(
                formatter,
                "too many redirects: stopped after {MAX_REDIRECTS} hops"
            ),
            FetchError::Unsupported {
                content_type,
                bytes,
            } => write!(
                formatter,
                "web_fetch cannot read {} content ({bytes} bytes); only HTML and text responses are converted",
                content_type.as_deref().unwrap_or("unknown")
            ),
            FetchError::Timeout { timeout } => write!(
                formatter,
                "request timed out after {:.1}s",
                timeout.as_secs_f64()
            ),
            FetchError::Cancelled => write!(formatter, "fetch cancelled"),
            FetchError::Convert(message) => {
                write!(formatter, "could not convert HTML to Markdown: {message}")
            }
        }
    }
}

/// What a successful fetch produced, before it is rendered for the model.
#[derive(Debug)]
struct FetchOutcome {
    requested_url: String,
    final_url: String,
    content_type: Option<String>,
    downloaded_bytes: u64,
    text: String,
}

impl FetchOutcome {
    fn into_result(self) -> AgentToolResult {
        let content_type = self.content_type.as_deref().unwrap_or("(none)");
        let header = format!(
            "URL: {}\nContent-Type: {}\nDownloaded: {} bytes\n\n",
            self.final_url, content_type, self.downloaded_bytes
        );
        let budget = OUTPUT_BYTE_CAP.saturating_sub(header.len());
        let (body, truncated) = clamp_utf8(&self.text, budget);
        let mut text = String::with_capacity(header.len() + body.len() + 128);
        text.push_str(&header);
        text.push_str(body);
        if truncated {
            text.push_str(&format!(
                "\n\n[truncated: output capped at 50 KB — {} bytes downloaded]",
                self.downloaded_bytes
            ));
        }
        let output_bytes = text.len();
        AgentToolResult {
            content: vec![BlockContent::Text(TextContent {
                text,
                ..Default::default()
            })],
            details: json!({
                "url": self.requested_url,
                "final_url": self.final_url,
                "content_type": self.content_type,
                "downloaded_bytes": self.downloaded_bytes,
                "output_bytes": output_bytes,
                "truncated": truncated,
            }),
            ..Default::default()
        }
    }
}

/// Trim `text` to at most `max_bytes`, never splitting a character.
fn clamp_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn build_client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|error| FetchError::Client(error.to_string()))
}

fn markdown(html: &str) -> Result<String, FetchError> {
    // htmd walks script/style children into the output, so both are skipped
    // explicitly; v1 does no readability pass beyond that.
    HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style"])
        .build()
        .convert(html)
        .map_err(|error| FetchError::Convert(error.to_string()))
}

fn map_reqwest_error(error: reqwest::Error) -> FetchError {
    if error.is_redirect() {
        FetchError::TooManyRedirects
    } else {
        FetchError::Request(error.to_string())
    }
}

/// Decode the downloaded bytes into the text handed to the model. HTML parsing
/// is CPU-bound, so the caller runs this on the blocking pool.
fn decode_body(kind: BodyKind, body: &[u8]) -> Result<String, FetchError> {
    let text = String::from_utf8_lossy(body);
    match kind {
        BodyKind::Html => markdown(&text),
        _ => Ok(text.into_owned()),
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<FetchOutcome, FetchError> {
    let response = client.get(url).send().await.map_err(map_reqwest_error)?;
    let status = response.status();
    let final_url = response.url().to_string();
    if !status.is_success() {
        return Err(FetchError::Status {
            status: status.as_u16(),
            url: final_url,
        });
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string());
    let kind = classify(content_type.as_deref());
    let declared = response.content_length();
    // A binary response is rejected from Content-Length alone, so an
    // oversized PDF or image still reports its content type rather than
    // the generic cap message.
    if kind == BodyKind::Binary
        && let Some(bytes) = declared
    {
        return Err(FetchError::Unsupported {
            content_type,
            bytes,
        });
    }
    if let Some(bytes) = declared
        && bytes > DOWNLOAD_BYTE_CAP
    {
        return Err(FetchError::TooLarge {
            declared: Some(bytes),
        });
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest_error)?;
        if body.len() as u64 + chunk.len() as u64 > DOWNLOAD_BYTE_CAP {
            return Err(FetchError::TooLarge { declared: None });
        }
        body.extend_from_slice(&chunk);
    }
    let downloaded_bytes = body.len() as u64;
    if kind == BodyKind::Binary {
        return Err(FetchError::Unsupported {
            content_type,
            bytes: downloaded_bytes,
        });
    }
    let text = tokio::task::spawn_blocking(move || decode_body(kind, &body))
        .await
        .map_err(|error| FetchError::Convert(format!("conversion task failed: {error}")))??;
    Ok(FetchOutcome {
        requested_url: url.to_string(),
        final_url,
        content_type,
        downloaded_bytes,
        text,
    })
}

/// [`fetch`] under a total wall-clock budget (headers through last body byte).
async fn fetch_bounded(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<FetchOutcome, FetchError> {
    match tokio::time::timeout(timeout, fetch(client, url)).await {
        Ok(result) => result,
        Err(_) => Err(FetchError::Timeout { timeout }),
    }
}

fn parameters_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "http:// or https:// URL to fetch"
            }
        },
        "required": ["url"],
        "additionalProperties": false
    })
}

pub(crate) fn create_web_fetch_tool() -> AgentTool {
    let execute = Arc::new(
        move |_tool_call_id: &str,
              params: &serde_json::Value,
              signal: Option<&CancellationToken>,
              _on_update: Option<&AgentToolUpdateCallback>| {
            let input = serde_json::from_value::<WebFetchInput>(params.clone())
                .map_err(|error| format!("invalid web_fetch parameters: {error}"));
            let cancel = signal.cloned().unwrap_or_default();
            Box::pin(async move {
                let input = input?;
                let client = build_client().map_err(|error| error.to_string())?;
                tokio::select! {
                    _ = cancel.cancelled() => Err(FetchError::Cancelled),
                    result = fetch_bounded(&client, &input.url, REQUEST_TIMEOUT) => {
                        result.map(FetchOutcome::into_result)
                    }
                }
                .map_err(|error| error.to_string())
            }) as BoxFuture<'static, Result<AgentToolResult, String>>
        },
    );
    AgentTool {
        name: "web_fetch".to_string(),
        label: "Web Fetch".to_string(),
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// An in-process loopback HTTP/1.1 server — the tests never touch the
    /// real network. Every connection is answered from `respond`, which sees
    /// the request target (path plus query) and returns the raw response
    /// bytes; an empty return parks the connection without answering, which
    /// keeps a request in flight for the timeout and cancellation tests.
    struct Server {
        base: String,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        /// The request heads received so far. Tests use this to pin the
        /// headers the tool sends.
        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve<F>(respond: F) -> Server
    where
        F: Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let respond = Arc::new(respond);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let respond = Arc::clone(&respond);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    let head_end = loop {
                        let read = match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break request.len(),
                            Ok(read) => read,
                        };
                        request.extend_from_slice(&chunk[..read]);
                        if let Some(end) = header_end(&request) {
                            break end;
                        }
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    recorded.lock().unwrap().push(head.to_string());
                    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let response = respond(&target);
                    if response.is_empty() {
                        std::future::pending::<()>().await;
                    }
                    let _ = socket.write_all(&response).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Server {
            base,
            requests,
            task,
        }
    }

    fn header_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
    }

    fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn redirect(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    async fn fetch_path(server: &Server, path: &str) -> Result<FetchOutcome, FetchError> {
        let client = build_client().unwrap();
        fetch(&client, &format!("{}{path}", server.base)).await
    }

    async fn run_tool(url: &str) -> Result<AgentToolResult, String> {
        let tool = create_web_fetch_tool();
        (tool.execute)("call-1", &json!({ "url": url }), None, None).await
    }

    fn text_of(result: &AgentToolResult) -> &str {
        match result.content.first().unwrap() {
            BlockContent::Text(text) => &text.text,
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn classification_covers_html_text_and_binary() {
        assert_eq!(classify(Some("text/html; charset=utf-8")), BodyKind::Html);
        assert_eq!(classify(Some("application/xhtml+xml")), BodyKind::Html);
        assert_eq!(classify(Some("text/plain")), BodyKind::Text);
        assert_eq!(classify(Some("application/json")), BodyKind::Text);
        assert_eq!(classify(Some("application/ld+json")), BodyKind::Text);
        assert_eq!(classify(Some("text/csv; charset=utf-8")), BodyKind::Text);
        assert_eq!(classify(None), BodyKind::Text);
        assert_eq!(classify(Some("application/pdf")), BodyKind::Binary);
        assert_eq!(classify(Some("image/png")), BodyKind::Binary);
        assert_eq!(classify(Some("application/octet-stream")), BodyKind::Binary);
    }

    #[tokio::test]
    async fn html_is_converted_to_markdown_with_scripts_stripped() {
        let server = serve(|_| {
            response(
                "200 OK",
                "text/html; charset=utf-8",
                b"<html><head><title>T</title><style>body{color:red}</style></head>\
                  <body><h1>Hi</h1><p>Hello <strong>world</strong></p>\
                  <script>alert(1)</script></body></html>",
            )
        })
        .await;

        let result = fetch_path(&server, "/page").await.unwrap().into_result();
        let text = text_of(&result);

        assert!(text.contains("# Hi"), "unexpected markdown: {text}");
        assert!(text.contains("**world**"), "unexpected markdown: {text}");
        assert!(!text.contains("alert(1)"), "script leaked: {text}");
        assert!(!text.contains("color:red"), "style leaked: {text}");
        assert!(text.starts_with(&format!("URL: {}/page\n", server.base)));
        assert_eq!(
            result.details["url"],
            json!(format!("{}/page", server.base))
        );
        assert_eq!(
            result.details["content_type"],
            json!("text/html; charset=utf-8")
        );
        assert_eq!(result.details["truncated"], json!(false));
    }

    #[tokio::test]
    async fn non_html_text_passes_through_raw() {
        let server = serve(|target| match target {
            "/data.json" => response("200 OK", "application/json", br#"{"ok":true}"#),
            "/data.csv" => response("200 OK", "text/csv", b"a,b\n1,2\n"),
            _ => response("200 OK", "text/plain", b"plain & raw\n"),
        })
        .await;

        let json = fetch_path(&server, "/data.json").await.unwrap();
        assert_eq!(json.text, r#"{"ok":true}"#);
        assert_eq!(json.content_type.as_deref(), Some("application/json"));

        let csv = fetch_path(&server, "/data.csv").await.unwrap();
        assert_eq!(csv.text, "a,b\n1,2\n");

        let plain = fetch_path(&server, "/plain").await.unwrap();
        assert_eq!(plain.text, "plain & raw\n");
        assert!(text_of(&plain.into_result()).contains("plain & raw"));
    }

    #[tokio::test]
    async fn binary_pdf_and_image_are_rejected_naming_type_and_size() {
        let cases: [(&str, &[u8]); 3] = [
            ("application/octet-stream", b"\x00\x01\x02\x03\x04"),
            ("application/pdf", b"%PDF-1.7\n..."),
            ("image/png", b"\x89PNG\r\n\x1a\n"),
        ];
        for (content_type, body) in cases {
            let server = serve(move |_| response("200 OK", content_type, body)).await;
            let error = fetch_path(&server, "/").await.unwrap_err();
            assert!(
                matches!(error, FetchError::Unsupported { .. }),
                "unexpected error for {content_type}: {error:?}"
            );
            let message = error.to_string();
            assert!(message.contains(content_type), "unexpected: {message}");
            assert!(
                message.contains(&body.len().to_string()),
                "size missing from: {message}"
            );
        }
    }

    #[tokio::test]
    async fn binary_without_content_length_is_rejected_after_streaming() {
        let server = serve(|_| {
            let mut out =
                b"HTTP/1.1 200 OK\r\nContent-Type: image/gif\r\nConnection: close\r\n\r\n".to_vec();
            out.extend_from_slice(b"GIF89a");
            out
        })
        .await;

        let error = fetch_path(&server, "/").await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("image/gif"), "unexpected: {message}");
        assert!(message.contains("6 bytes"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn declared_oversize_is_rejected_without_reading_the_body() {
        let server = serve(|_| {
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6000000\r\nConnection: close\r\n\r\n"
                .to_vec()
        })
        .await;

        let error = fetch_path(&server, "/").await.unwrap_err();
        assert!(
            matches!(
                error,
                FetchError::TooLarge {
                    declared: Some(6_000_000)
                }
            ),
            "unexpected: {error:?}"
        );
        assert!(error.to_string().contains("6000000"));
    }

    #[tokio::test]
    async fn an_oversized_binary_reports_its_type_rather_than_the_cap() {
        let server = serve(|_| {
            b"HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 6000000\r\nConnection: close\r\n\r\n"
                .to_vec()
        })
        .await;

        let error = fetch_path(&server, "/doc.pdf").await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("application/pdf"), "unexpected: {message}");
        assert!(message.contains("6000000"), "unexpected: {message}");
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn streamed_oversize_is_rejected() {
        let over = DOWNLOAD_BYTE_CAP + 1;
        let server = serve(move |_| {
            let mut out =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
                    .to_vec();
            out.resize(out.len() + over as usize, b'a');
            out
        })
        .await;

        let error = fetch_path(&server, "/").await.unwrap_err();
        assert!(
            matches!(error, FetchError::TooLarge { declared: None }),
            "unexpected: {error:?}"
        );
        assert!(error.to_string().contains("5242880"));
    }

    #[tokio::test]
    async fn output_over_the_envelope_truncates_with_a_notice() {
        let downloaded = 200 * 1024;
        let server = serve(move |_| {
            let mut body = b"start-".to_vec();
            body.resize(downloaded, b'x');
            response("200 OK", "text/plain", &body)
        })
        .await;

        let outcome = fetch_path(&server, "/big").await.unwrap();
        assert_eq!(outcome.downloaded_bytes, downloaded as u64);
        let result = outcome.into_result();
        let text = text_of(&result);

        assert_eq!(result.details["truncated"], json!(true));
        assert_eq!(result.details["downloaded_bytes"], json!(downloaded as u64));
        assert_eq!(result.details["output_bytes"], json!(text.len()));
        assert!(text.contains("start-"));
        assert!(
            text.ends_with("bytes downloaded]"),
            "missing notice: {}",
            &text[text.len() - 80..]
        );
        assert!(
            text.len() <= OUTPUT_BYTE_CAP + 128,
            "envelope exceeded: {} bytes",
            text.len()
        );
    }

    #[tokio::test]
    async fn redirects_are_followed_across_hosts_and_the_final_url_is_reported() {
        let target = serve(|_| response("200 OK", "text/plain", b"arrived")).await;
        let location = format!("{}/final", target.base);
        let source = serve(move |_| redirect(&location)).await;

        let outcome = fetch_path(&source, "/start").await.unwrap();
        assert_eq!(outcome.requested_url, format!("{}/start", source.base));
        assert_eq!(outcome.final_url, format!("{}/final", target.base));
        assert_eq!(outcome.text, "arrived");

        let result = outcome.into_result();
        let text = text_of(&result);
        assert!(
            text.starts_with(&format!("URL: {}/final\n", target.base)),
            "unexpected header: {text}"
        );
        assert_eq!(
            result.details["final_url"],
            json!(format!("{}/final", target.base))
        );
        assert_eq!(
            result.details["url"],
            json!(format!("{}/start", source.base))
        );
    }

    #[tokio::test]
    async fn a_redirect_loop_stops_at_the_hop_limit() {
        let server = serve(|_| redirect("/loop")).await;

        let error = fetch_path(&server, "/loop").await.unwrap_err();
        assert!(
            matches!(error, FetchError::TooManyRedirects),
            "unexpected: {error:?}"
        );
        assert!(error.to_string().contains("10"), "unexpected: {error}");
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let server = serve(|_| response("404 Not Found", "text/plain", b"nope")).await;

        let error = fetch_path(&server, "/missing").await.unwrap_err();
        assert!(
            matches!(error, FetchError::Status { status: 404, .. }),
            "unexpected: {error:?}"
        );
        assert!(error.to_string().contains("404"), "unexpected: {error}");
    }

    #[tokio::test]
    async fn a_hanging_request_times_out() {
        let server = serve(|_| Vec::new()).await;
        let client = build_client().unwrap();

        let error = fetch_bounded(
            &client,
            &format!("{}/hang", server.base),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, FetchError::Timeout { .. }),
            "unexpected: {error:?}"
        );
        assert!(
            error.to_string().contains("timed out"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn cancellation_races_the_run_token() {
        let server = serve(|_| Vec::new()).await;
        let tool = create_web_fetch_tool();
        let cancel = CancellationToken::new();
        let future = (tool.execute)(
            "call-1",
            &json!({ "url": format!("{}/hang", server.base) }),
            Some(&cancel),
            None,
        );
        let task = tokio::spawn(future);
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error, "fetch cancelled");
    }

    #[tokio::test]
    async fn tool_executes_end_to_end() {
        let server = serve(|_| response("200 OK", "text/html", b"<h1>Title</h1>")).await;

        let result = run_tool(&format!("{}/", server.base)).await.unwrap();
        let text = text_of(&result);
        assert!(text.contains("# Title"), "unexpected: {text}");
        assert!(text.starts_with(&format!("URL: {}/\n", server.base)));
        assert_eq!(
            result.details["final_url"],
            json!(format!("{}/", server.base))
        );
    }

    #[tokio::test]
    async fn parameters_are_url_only_and_errors_are_typed() {
        let tool = create_web_fetch_tool();
        let missing = (tool.execute)("call-1", &json!({}), None, None)
            .await
            .unwrap_err();
        assert!(
            missing.contains("invalid web_fetch parameters"),
            "unexpected: {missing}"
        );

        let extra = (tool.execute)(
            "call-1",
            &json!({ "url": "http://127.0.0.1:1/", "prompt": "summarize" }),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(extra.contains("unknown field"), "unexpected: {extra}");

        let malformed = (tool.execute)("call-1", &json!({ "url": "not a url" }), None, None)
            .await
            .unwrap_err();
        assert!(
            malformed.contains("request failed"),
            "unexpected: {malformed}"
        );
    }

    #[test]
    fn description_names_every_limit() {
        for expected in [
            "5 MB",
            "50 KB",
            "30s",
            "10 hops",
            "Markdown",
            "JSON",
            "PDF",
            "binary",
            "localhost",
            "not cached",
            "final URL",
            "Non-success HTTP",
        ] {
            assert!(
                DESCRIPTION.contains(expected),
                "description is missing {expected:?}"
            );
        }
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn requests_carry_the_holt_user_agent() {
        let server = serve(|_| response("200 OK", "text/plain", b"ok")).await;
        fetch_path(&server, "/ua").await.unwrap();

        let request = &server.requests()[0];
        assert!(
            request.contains(&format!(
                "user-agent: holt/{}\r\n",
                env!("CARGO_PKG_VERSION")
            )),
            "unexpected request: {request}"
        );
    }

    #[test]
    fn execution_tools_mount_web_fetch() {
        let tools = crate::tools::execution_tools(".");
        let tool = tools
            .iter()
            .find(|tool| tool.name == "web_fetch")
            .expect("web_fetch is mounted");
        assert_eq!(tool.label, "Web Fetch");
        assert_eq!(tool.parameters["required"], json!(["url"]));
    }
}
