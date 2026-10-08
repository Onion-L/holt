//! HTTP transport abstraction: the Rust counterpart of the TypeScript
//! `FetchFunction` option (`globalThis.fetch`).
//!
//! Provider adapters issue requests through this trait so tests can inject
//! canned responses and applications can proxy or instrument traffic. The
//! default implementation (`default_fetch`) uses `reqwest`; WebSocket
//! transports are separate and not covered by this trait.

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;

/// An outbound provider HTTP request.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: HttpBody,
    /// The abort signal riding with the request (the `init.signal` of the
    /// TypeScript `fetch` call). Transports observe it to cancel in-flight
    /// work; `None` when the caller did not provide one.
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// Supported request methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Patch => "PATCH",
        }
    }
}

/// A request body.
#[derive(Clone, Debug, Default)]
pub enum HttpBody {
    #[default]
    Empty,
    Bytes(Bytes),
    Text(String),
    Json(serde_json::Value),
}

/// A provider HTTP response with a streaming body.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: BoxStream<'static, Result<Bytes, HttpFetchError>>,
}

impl HttpResponse {
    /// Collects the full response body.
    pub async fn bytes(mut self) -> Result<Bytes, HttpFetchError> {
        let mut buf = Vec::new();
        use futures::StreamExt;
        while let Some(chunk) = self.body.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(Bytes::from(buf))
    }

    /// Collects the full response body as UTF-8 text.
    pub async fn text(self) -> Result<String, HttpFetchError> {
        let bytes = self.bytes().await?;
        String::from_utf8(bytes.to_vec()).map_err(|error| {
            HttpFetchError::Body(format!("invalid utf-8 in response body: {error}"))
        })
    }
}

/// Collects the response body as text (lossy on invalid UTF-8).
pub async fn collect_text(response: HttpResponse) -> String {
    String::from_utf8_lossy(&response.bytes().await.unwrap_or_default()).to_string()
}

/// Also polls cancellation while a server is not producing body chunks.
pub(crate) fn abortable_body(
    body: BoxStream<'static, Result<Bytes, HttpFetchError>>,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> BoxStream<'static, Result<Bytes, HttpFetchError>> {
    use futures::StreamExt;
    let Some(signal) = signal else { return body };
    Box::pin(futures::stream::unfold(
        Some((body, signal)),
        |state| async {
            let (mut body, signal) = state?;
            tokio::select! {
                biased;
                () = signal.cancelled() => Some((Err(HttpFetchError::Cancelled), None)),
                chunk = body.next() => chunk.map(|chunk| (chunk, Some((body, signal)))),
            }
        },
    ))
}

/// Buffers only an incomplete frame. Decode UTF-8 after finding a complete
/// ASCII separator so split code points survive arbitrary chunk boundaries.
pub(crate) fn text_frames(
    body: BoxStream<'static, Result<Bytes, HttpFetchError>>,
    separators: &'static [&'static str],
) -> BoxStream<'static, Result<String, HttpFetchError>> {
    use futures::StreamExt;
    Box::pin(futures::stream::unfold(
        (body, Vec::<u8>::new(), false),
        move |(mut body, mut buffer, mut done)| async move {
            loop {
                let boundary = separators
                    .iter()
                    .enumerate()
                    .filter_map(|(order, separator)| {
                        buffer
                            .windows(separator.len())
                            .position(|bytes| bytes == separator.as_bytes())
                            .map(|index| (index, order, separator.len()))
                    })
                    .min();
                if let Some((index, _, length)) = boundary {
                    let frame = String::from_utf8_lossy(&buffer[..index]).into_owned();
                    buffer.drain(..index + length);
                    return Some((Ok(frame), (body, buffer, done)));
                }
                if done {
                    if buffer.is_empty() {
                        return None;
                    }
                    let frame = String::from_utf8_lossy(&buffer).into_owned();
                    return Some((Ok(frame), (body, Vec::new(), true)));
                }
                match body.next().await {
                    Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                    Some(Err(error)) => return Some((Err(error), (body, Vec::new(), true))),
                    None => done = true,
                }
            }
        },
    ))
}

/// Errors surfaced by the HTTP transport.
#[derive(Debug)]
pub enum HttpFetchError {
    /// The request failed before a response arrived (connection, DNS, TLS).
    Request(String),
    /// The response body failed mid-stream.
    Body(String),
    /// The operation was cancelled.
    Cancelled,
}

impl std::fmt::Display for HttpFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpFetchError::Request(message) => write!(f, "{message}"),
            HttpFetchError::Body(message) => write!(f, "{message}"),
            HttpFetchError::Cancelled => write!(f, "The operation was aborted"),
        }
    }
}

impl std::error::Error for HttpFetchError {}

/// Port of `FetchFunction`: an injectable HTTP transport.
pub trait HttpFetch: Send + Sync {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpFetchError>>;
}
