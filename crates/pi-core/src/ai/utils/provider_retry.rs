//! Port of `pi-core/ai/src/utils/provider-retry.ts`: reproduces the retry
//! behavior of the OpenAI and Anthropic SDKs while making the backoff sleep
//! interruptible.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ai::utils::http::{HttpFetch, HttpRequest, HttpResponse};

/// Port of the SDK-shaped provider error: status, response headers, and
/// message. Provider request layers construct this for HTTP failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderHttpError {
    pub message: String,
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    /// The response body of an HTTP failure; `None` for transport failures.
    pub body: Option<String>,
}

impl ProviderHttpError {
    pub fn new(
        message: impl Into<String>,
        status: Option<u16>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            message: message.into(),
            status,
            headers,
            body: None,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProviderHttpError {}

/// Port of `ProviderRetryOptions`.
#[derive(Default)]
pub struct ProviderRetryOptions {
    /// Invoked before each retry's backoff sleep; `None` disables notification.
    pub on_retry: Option<crate::ai::types::OnRetryCallback>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub signal: Option<CancellationToken>,
}

const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// Mirrors the pinned OpenAI/Anthropic SDK retry policy; review when either
/// SDK is upgraded.
/// Quota/billing exhaustion (often sent as a 429) never retries: waiting
/// does not refill a budget.
fn is_retryable_provider_error(error: &ProviderHttpError) -> bool {
    if error
        .body
        .as_deref()
        .is_some_and(crate::ai::utils::retry::is_provider_limit_error)
    {
        return false;
    }
    match error.header("x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match error.status {
        None => true,
        Some(status) => status == 408 || status == 409 || status == 429 || status >= 500,
    }
}

fn validate_server_retry_delay_ms(
    delay_ms: f64,
    max_retry_delay_ms: Option<u64>,
    provider_error_message: &str,
) -> Result<f64, String> {
    let max_delay_ms = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_delay_ms > 0 && delay_ms > max_delay_ms as f64 {
        return Err(format!(
            "Server requested {}s retry delay (max: {}s). {provider_error_message}",
            (delay_ms / 1000.0).ceil(),
            (max_delay_ms as f64 / 1000.0).ceil(),
        ));
    }
    Ok(delay_ms)
}

fn get_retry_delay_ms(
    error: &ProviderHttpError,
    retry_index: u32,
    max_retry_delay_ms: Option<u64>,
    now_ms: impl Fn() -> i64,
) -> Result<f64, String> {
    if let Some(retry_after_ms) = error.header("retry-after-ms")
        && let Ok(value) = retry_after_ms.parse::<f64>()
    {
        return validate_server_retry_delay_ms(value, max_retry_delay_ms, &error.message);
    }

    if let Some(retry_after) = error.header("retry-after") {
        let delay_ms = match retry_after.parse::<f64>() {
            Ok(seconds) => seconds * 1000.0,
            Err(_) => {
                // HTTP-date form.
                let parsed = chrono_http_date_to_epoch_ms(retry_after)
                    .map_or(0.0, |date_ms| (date_ms - now_ms()) as f64);
                if parsed.is_nan() { f64::NAN } else { parsed }
            }
        };
        return validate_server_retry_delay_ms(delay_ms, max_retry_delay_ms, &error.message);
    }

    let exponential_delay = (0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0;
    // JS applies a uniform jitter of up to 25% via Math.random; the Rust port
    // uses the system RNG.
    let jitter_factor = 1.0 - rand_jitter() * 0.25;
    Ok(exponential_delay * jitter_factor)
}

/// Parses an HTTP-date header value to epoch milliseconds; `None` when
/// unparseable (mirroring `Number.isNaN(Date.parse(...))` handling).
fn chrono_http_date_to_epoch_ms(value: &str) -> Option<i64> {
    httpdate::parse_http_date(value).ok().map(|system_time| {
        system_time
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default()
    })
}

fn rand_jitter() -> f64 {
    // Cheap uniform in [0, 1) from the system RNG.
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("system RNG is always available");
    f64::from(u32::from_le_bytes(bytes)) / (u32::MAX as f64 + 1.0)
}

fn create_abort_error() -> ProviderHttpError {
    ProviderHttpError::new("Request aborted", None, Vec::new())
}

/// Port of `retryProviderRequest`: retries transient provider errors with
/// SDK-equivalent backoff, honoring `retry-after-ms`/`retry-after` headers,
/// failing immediately on server delays above `max_retry_delay_ms`, and
/// surfacing cancellation as an abort error.
pub async fn retry_provider_request<T, P, F>(
    mut request: P,
    options: ProviderRetryOptions,
) -> Result<T, ProviderHttpError>
where
    P: FnMut() -> F,
    F: Future<Output = Result<T, ProviderHttpError>>,
{
    let max_retries = options.max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries as i64;
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default()
    };

    loop {
        let result = request().await;
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        if options
            .signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(create_abort_error());
        }
        if retries_remaining <= 0 || !is_retryable_provider_error(&error) {
            return Err(error);
        }

        let retry_index = (max_retries as i64 - retries_remaining) as u32;
        retries_remaining -= 1;
        let delay_ms = get_retry_delay_ms(&error, retry_index, options.max_retry_delay_ms, now_ms)
            .map_err(|message| ProviderHttpError::new(message, None, Vec::new()))?;

        if let Some(on_retry) = options.on_retry.as_ref() {
            on_retry(
                retry_index + 1,
                max_retries,
                delay_ms.max(0.0) as u64,
                &error.message,
            );
        }

        abortable_sleep_provider(delay_ms.max(0.0) as u64, options.signal.as_ref()).await?;
    }
}

/// Sends `request` through [`retry_provider_request`] with SDK semantics: a
/// non-2xx response is a failed attempt, classified by status,
/// `x-should-retry`, `retry-after`, and body. The final response comes back
/// as-is (a non-2xx one with its collected body re-attached), so each
/// provider keeps its own error formatting; only transport failures, aborts,
/// and over-cap server delays surface as `Err`.
pub async fn retry_http_request(
    fetch: &Arc<dyn HttpFetch>,
    request: &HttpRequest,
    options: ProviderRetryOptions,
) -> Result<HttpResponse, ProviderHttpError> {
    let result = retry_provider_request(
        || {
            let fetch = Arc::clone(fetch);
            let request = request.clone();
            async move {
                let response = fetch
                    .fetch(request)
                    .await
                    .map_err(|error| ProviderHttpError::new(error.to_string(), None, Vec::new()))?;
                if (200..300).contains(&response.status) {
                    return Ok(response);
                }
                let status = response.status;
                let headers = response.headers.clone();
                let body = crate::ai::utils::http::collect_text(response).await;
                let excerpt = crate::ai::utils::error_body::truncate_error_text(
                    body.trim(),
                    crate::ai::utils::error_body::MAX_PROVIDER_ERROR_BODY_CHARS,
                );
                let message = if excerpt.is_empty() {
                    format!("{status} status code")
                } else {
                    format!("{status} {excerpt}")
                };
                Err(ProviderHttpError {
                    message,
                    status: Some(status),
                    headers,
                    body: Some(body),
                })
            }
        },
        options,
    )
    .await;
    match result {
        Err(ProviderHttpError {
            status: Some(status),
            headers,
            body,
            ..
        }) => Ok(HttpResponse {
            status,
            headers,
            body: Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(
                body.unwrap_or_default(),
            ))])),
        }),
        other => other,
    }
}

/// The backoff sleep aborts the whole request when cancellation fires,
/// mirroring the TypeScript abortableSleep rejection.
async fn abortable_sleep_provider(
    ms: u64,
    signal: Option<&CancellationToken>,
) -> Result<(), ProviderHttpError> {
    let sleep = tokio::time::sleep(Duration::from_millis(ms));
    match signal {
        Some(signal) => {
            tokio::select! {
                () = sleep => Ok(()),
                _ = signal.cancelled() => Err(create_abort_error()),
            }
        }
        None => {
            sleep.await;
            Ok(())
        }
    }
}
