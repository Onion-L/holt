//! Port of `pi-core/ai/src/utils/retry.ts`: bounded retries with exponential
//! backoff for assistant-producing calls, plus transient-error
//! classification.

use futures::future::BoxFuture;
use regex::Regex;
use std::sync::OnceLock;

use crate::ai::types::{AssistantMessage, StopReason};
use crate::ai::utils::abort::abortable_sleep;
use tokio_util::sync::CancellationToken;

/// Port of `RetryPolicy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries). The initial call never counts.
    pub max_retries: u32,
    /// Base delay in ms. Per-attempt delay is `baseDelayMs * 2^(attempt-1)`.
    pub base_delay_ms: u64,
}

/// Port of `RetryCallbacks`.
#[derive(Default)]
pub struct RetryCallbacks {
    /// Emitted before the backoff sleep of each retry attempt (1-indexed).
    pub on_retry_scheduled: Option<OnRetryScheduled>,
    /// Emitted after the backoff sleep, immediately before the retried call.
    pub on_retry_attempt_start: Option<Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>,
    /// Emitted once when the loop ends: success if a later call completed.
    pub on_retry_finished: Option<OnRetryFinished>,
}

/// Arguments of the `onRetryScheduled` callback: attempt (1-indexed), max
/// attempts, delay in ms, and the previous error message.
pub type OnRetryScheduled =
    Box<dyn Fn(u32, u32, u64, &str) -> BoxFuture<'static, ()> + Send + Sync>;

/// Arguments of `onRetryFinished`: success, attempt, and the final error
/// message when unsuccessful.
pub type OnRetryFinished =
    Box<dyn Fn(bool, u32, Option<&str>) -> BoxFuture<'static, ()> + Send + Sync>;

fn non_retryable_provider_limit_error_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(&format!(
            "(?i){}",
            [
                // OpenCode Go/free-tier limits returned as 429 JSON error types.
                "GoUsageLimitError",
                "FreeUsageLimitError",
                // OpenCode Go subscription-limit text.
                "Monthly usage limit reached",
                "available balance",
                // Generic quota/budget/billing exhaustion.
                "insufficient_quota",
                "out of budget",
                "quota exceeded",
                "billing",
            ]
            .join("|")
        ))
        .expect("non-retryable pattern compiles")
    })
}

fn retryable_provider_error_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(&format!(
            "(?i){}",
            [
                // Generic provider load, HTTP status, server-side transients.
                "overloaded",
                "rate.?limit",
                "too many requests",
                "429",
                "500",
                "502",
                "503",
                "504",
                "524",
                "service.?unavailable",
                "server.?error",
                "internal.?error",
                // Wrapper/provider text for transient upstream failures.
                "provider.?returned.?error",
                "exceeded request buffer limit while retrying upstream",
                // Network, proxy, and fetch transport failures.
                "network.?error",
                "connection.?error",
                "connection.?refused",
                "connection.?lost",
                "other side closed",
                "fetch failed",
                "getaddrinfo",
                "ENOTFOUND",
                "EAI_AGAIN",
                "upstream.?connect",
                "reset before headers",
                "socket hang up",
                "socket connection was closed",
                "timed? out",
                "timeout",
                "terminated",
                // WebSocket close/error text.
                "websocket.?closed",
                "websocket.?error",
                // Premature stream endings.
                "ended without",
                "stream ended before message_stop",
                "stream ended before a terminal response event",
                "http2 request did not get a response",
                // Provider-requested retry delay cap failures.
                "retry delay",
                // Explicit retry guidance emitted mid-stream.
                "you can retry your request",
                "try your request again",
                "please retry your request",
                // gRPC based providers (e.g. NVIDIA NIM).
                "ResourceExhausted",
            ]
            .join("|")
        ))
        .expect("retryable pattern compiles")
    })
}

/// Whether provider error text reports quota/budget/billing exhaustion,
/// which no retry can fix.
pub(crate) fn is_provider_limit_error(text: &str) -> bool {
    non_retryable_provider_limit_error_pattern().is_match(text)
}

/// Port of `isRetryableAssistantError`.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    if non_retryable_provider_limit_error_pattern().is_match(error_message) {
        return false;
    }
    retryable_provider_error_pattern().is_match(error_message)
}

fn sleep_ms(
    ms: u64,
    signal: Option<&CancellationToken>,
) -> BoxFuture<'_, Result<(), crate::ai::utils::abort::AbortRace>> {
    Box::pin(abortable_sleep(ms, signal))
}

/// Port of `retryAssistantCall`: runs a single assistant-producing call with
/// bounded retry on transient errors.
///
/// Behavior (matching the TypeScript loop):
/// - A successful response returns immediately; aborted messages are terminal
///   and never retried.
/// - A non-retryable error (including quota/billing exhaustion) returns
///   immediately.
/// - Otherwise retries up to `maxRetries` with exponential backoff, emitting
///   the callbacks around each attempt, and normalizes aborts during backoff
///   to an aborted `AssistantMessage`.
pub async fn retry_assistant_call<P, F>(
    mut produce: P,
    policy: Option<&RetryPolicy>,
    signal: Option<&CancellationToken>,
    callbacks: Option<&RetryCallbacks>,
) -> AssistantMessage
where
    P: FnMut() -> F,
    F: Future<Output = AssistantMessage>,
{
    let max_attempts = policy
        .filter(|policy| policy.enabled)
        .map_or(0, |policy| policy.max_retries);

    let mut attempt = 0u32;
    let mut last_retry: Option<(u32, String)> = None;
    loop {
        let response = produce().await;

        // Abort: terminal but not successful. Never retry an aborted message.
        if response.stop_reason == StopReason::Aborted {
            if let Some((attempt, _)) = last_retry
                && let Some(callback) = callbacks.and_then(|c| c.on_retry_finished.as_ref())
            {
                callback(false, attempt, None).await;
            }
            return response;
        }

        // Success: non-error, non-abort responses return as-is.
        if response.stop_reason != StopReason::Error {
            if let Some((attempt, _)) = last_retry
                && let Some(callback) = callbacks.and_then(|c| c.on_retry_finished.as_ref())
            {
                callback(true, attempt, None).await;
            }
            return response;
        }

        // Non-retryable, or budget exhausted: return the final error message.
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((attempt, _)) = last_retry
                && let Some(callback) = callbacks.and_then(|c| c.on_retry_finished.as_ref())
            {
                callback(false, attempt, response.error_message.as_deref()).await;
            }
            return response;
        }

        attempt += 1;
        let error_message = response.error_message.clone().unwrap_or_default();
        let retry_error_message = if error_message.is_empty() {
            "Unknown error".to_string()
        } else {
            error_message
        };
        let base_delay_ms = policy.map_or(0, |policy| policy.base_delay_ms);
        let delay_ms = base_delay_ms * 2u64.pow(attempt - 1);
        if let Some(callback) = callbacks.and_then(|c| c.on_retry_scheduled.as_ref()) {
            callback(attempt, max_attempts, delay_ms, &retry_error_message).await;
        }
        last_retry = Some((attempt, retry_error_message.clone()));

        // Aborts during retry backoff normalize to the aborted message shape.
        match sleep_ms(delay_ms, signal).await {
            Ok(()) => {}
            Err(_) => {
                if let Some(callback) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
                    callback(false, attempt, Some(retry_error_message.as_str())).await;
                }
                let mut aborted = response.clone();
                aborted.stop_reason = StopReason::Aborted;
                aborted.error_message = None;
                return aborted;
            }
        }
        if let Some(callback) = callbacks.and_then(|c| c.on_retry_attempt_start.as_ref()) {
            callback().await;
        }
    }
}
