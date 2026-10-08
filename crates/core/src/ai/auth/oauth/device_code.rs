//! Port of `pi-core/ai/src/auth/oauth/device-code.ts`: RFC 8628 device-code
//! polling.
//!
//! The TypeScript version reads wall-clock time (`Date.now()`) and stubs
//! timers in tests; the Rust port uses `tokio::time`, so paused-time tests
//! (`#[tokio::test(start_paused = true)]`) drive the same scheduling
//! deterministically.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

const CANCEL_MESSAGE: &str = "Login cancelled";
const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: u64 = 1_000;
/// RFC 8628 section 3.2: if the authorization server omits `interval`, the
/// client must use 5 seconds.
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 section 3.5: `slow_down` means the polling interval must increase
/// by 5 seconds.
const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5_000;

/// One poll outcome. Port of `OAuthDeviceCodePollResult<T>`.
pub enum OAuthDeviceCodePollResult<T> {
    Pending,
    /// The server asked to slow down; `interval_seconds` carries a
    /// server-provided minimum when present.
    SlowDown {
        interval_seconds: Option<f64>,
    },
    Failed {
        message: String,
    },
    Complete(T),
}

/// Port of `OAuthDeviceCodePollOptions<T>`.
pub struct OAuthDeviceCodePollOptions<F> {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<f64>,
    pub wait_before_first_poll: bool,
    pub signal: CancellationToken,
    pub poll: F,
}

/// Port of `abortableSleep`: sleeps, or rejects with the cancel message when
/// the signal fires (including when already cancelled).
pub async fn abortable_sleep(
    ms: u64,
    signal: &CancellationToken,
    cancel_message: &str,
) -> Result<(), String> {
    if signal.is_cancelled() {
        return Err(cancel_message.to_string());
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
        _ = signal.cancelled() => Err(cancel_message.to_string()),
    }
}

/// Port of `pollOAuthDeviceCodeFlow`: polls until complete, failed, expired,
/// or cancelled.
pub async fn poll_oauth_device_code_flow<T, F, Fut>(
    options: OAuthDeviceCodePollOptions<F>,
) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = OAuthDeviceCodePollResult<T>>,
{
    let deadline = options
        .expires_in_seconds
        .map(|seconds| tokio::time::Instant::now() + Duration::from_secs_f64(seconds.max(0.0)));
    let mut interval_ms = ((options
        .interval_seconds
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
        * 1000.0)
        .floor() as u64)
        .max(MINIMUM_INTERVAL_MS);

    let mut slow_down_responses = 0u32;
    if options.wait_before_first_poll
        && let Some(deadline) = deadline
    {
        let remaining_ms = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !remaining_ms.is_zero() {
            abortable_sleep(
                remaining_ms
                    .min(Duration::from_millis(interval_ms))
                    .as_millis() as u64,
                &options.signal,
                CANCEL_MESSAGE,
            )
            .await?;
        }
    }

    loop {
        if deadline.is_none_or(|deadline| tokio::time::Instant::now() < deadline) {
            if options.signal.is_cancelled() {
                return Err(CANCEL_MESSAGE.to_string());
            }

            match (options.poll)().await {
                OAuthDeviceCodePollResult::Complete(value) => return Ok(value),
                OAuthDeviceCodePollResult::Failed { message } => return Err(message),
                OAuthDeviceCodePollResult::SlowDown { interval_seconds } => {
                    slow_down_responses += 1;
                    // Use the server-provided interval when given (GitHub
                    // reports the new required minimum in `interval`);
                    // trusting only a client-tracked value risks polling
                    // early forever under WSL/VM clock drift. Otherwise apply
                    // RFC 8628 section 3.5: increase by 5 seconds.
                    interval_ms = match interval_seconds {
                        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
                            ((seconds * 1000.0).floor() as u64).max(MINIMUM_INTERVAL_MS)
                        }
                        _ => {
                            (interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS).max(MINIMUM_INTERVAL_MS)
                        }
                    };
                }
                OAuthDeviceCodePollResult::Pending => {}
            }

            if let Some(deadline) = deadline {
                let remaining_ms = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining_ms.is_zero() {
                    break;
                }
                abortable_sleep(
                    remaining_ms
                        .min(Duration::from_millis(interval_ms))
                        .as_millis() as u64,
                    &options.signal,
                    CANCEL_MESSAGE,
                )
                .await?;
            } else {
                abortable_sleep(interval_ms, &options.signal, CANCEL_MESSAGE).await?;
            }
        } else {
            break;
        }
    }

    Err(if slow_down_responses > 0 {
        SLOW_DOWN_TIMEOUT_MESSAGE.to_string()
    } else {
        TIMEOUT_MESSAGE.to_string()
    })
}
