//! The provider-retry notice contract: a live-only, transient signal that a
//! chat's provider request hit a retryable transport/HTTP failure and is
//! backing off before the next attempt. Published from inside the provider
//! retry loop (pi-core-rs `on_retry` callback) while the Turn is still
//! running — the transcript stays quiet during a retry, so this stream is
//! the only channel a UI can render "Retrying…" from. Live-only: notices are
//! never persisted or replayed across restart, and a UI must drop the chip
//! as soon as transcript frames (or the Turn's terminal event) resume.

use serde::{Deserialize, Serialize};

/// One scheduled provider retry for a chat's live Turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRetryNotice {
    pub chat_id: String,
    /// 1-based number of the retry about to run (1 = first retry).
    pub attempt: u32,
    /// The configured retry budget the attempt counts against.
    pub max_retries: u32,
    /// The backoff sleep before the retry fires (milliseconds).
    pub delay_ms: u64,
    /// Epoch ms at which the retry fires (engine-stamped; lets a UI render a
    /// countdown without its own timer semantics).
    pub retry_at_ms: i64,
    /// The provider error that triggered the retry.
    pub error: String,
}
