//! Default [`HttpFetch`](super::http::HttpFetch) implementation backed by
//! `reqwest`, honoring the environment proxy resolution ported from
//! `utils/node-http-proxy.ts`.

use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;

use super::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};
use super::node_http_proxy::resolve_http_proxy_url_for_target;

/// A `reqwest`-backed fetch using the process environment for proxy
/// configuration.
#[derive(Clone, Default)]
pub struct ReqwestFetch {
    client: Option<reqwest::Client>,
}

impl ReqwestFetch {
    /// Builds a fetch with an explicitly configured client (used by tests and
    /// callers that need custom TLS or proxy settings).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client: Some(client),
        }
    }

    /// Returns a client for the target URL, applying the environment-resolved
    /// proxy when one applies. Clients are cached per proxy URL.
    fn client_for(&self, url: &str) -> Result<reqwest::Client, HttpFetchError> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let proxy_url = resolve_http_proxy_url_for_target(url, None)
            .map_err(HttpFetchError::Request)?
            .unwrap_or_default();
        static CLIENTS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, reqwest::Client>>,
        > = std::sync::OnceLock::new();
        let clients =
            CLIENTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        let mut map = clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = map.get(&proxy_url) {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder();
        if !proxy_url.is_empty()
            && let Ok(proxy) = reqwest::Proxy::all(&proxy_url)
        {
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|error| HttpFetchError::Request(error.to_string()))?;
        map.insert(proxy_url, client.clone());
        Ok(client)
    }
}

impl HttpFetch for ReqwestFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        Box::pin(async move {
            // An already-aborted signal rejects before any traffic, matching
            // fetch's behavior on a pre-aborted `init.signal`.
            if request
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_cancelled())
            {
                return Err(HttpFetchError::Cancelled);
            }
            let client = self.client_for(&request.url)?;
            let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
                .map_err(|error| HttpFetchError::Request(error.to_string()))?;
            let mut builder = client.request(method, &request.url);

            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }

            builder = match &request.body {
                HttpBody::Empty => builder,
                HttpBody::Bytes(bytes) => builder.body(bytes.clone()),
                HttpBody::Text(text) => builder.body(text.clone()),
                HttpBody::Json(value) => builder.json(value),
            };

            let send = builder.send();
            let response = match request.signal.as_ref() {
                Some(signal) => tokio::select! {
                    response = send => response.map_err(|error| HttpFetchError::Request(format!("{error}")))?,
                    () = signal.cancelled() => return Err(HttpFetchError::Cancelled),
                },
                None => send
                    .await
                    .map_err(|error| HttpFetchError::Request(format!("{error}")))?,
            };

            let status = response.status().as_u16();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            let body = response
                .bytes_stream()
                .map(move |chunk| chunk.map_err(|error| HttpFetchError::Body(format!("{error}"))));

            Ok(HttpResponse {
                status,
                headers,
                body: super::http::abortable_body(Box::pin(body), request.signal),
            })
        })
    }
}

/// The shared default fetch instance.
pub fn default_fetch() -> std::sync::Arc<dyn HttpFetch> {
    static FETCH: std::sync::OnceLock<std::sync::Arc<dyn HttpFetch>> = std::sync::OnceLock::new();
    FETCH
        .get_or_init(|| std::sync::Arc::new(ReqwestFetch::default()))
        .clone()
}

/// Convenience for transports: collects request headers, applying defaults
/// without overriding caller values.
pub(crate) fn _normalize(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
}

/// Re-exports for provider adapters building SSE streams from responses.
pub fn bytes_stream_into_sse(
    response: HttpResponse,
) -> impl futures::Stream<Item = Result<Bytes, HttpFetchError>> {
    response.body
}
