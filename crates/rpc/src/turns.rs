//! The Turn terminal event contract (ADR-0019): the typed, engine-owned end
//! of one real main-chat Turn, published over `WatchTurnTerminalEvents` only
//! after the Turn's Transcript, History, and queue completion are durably
//! settled. Live-only: events are never persisted or replayed across restart.

use serde::{Deserialize, Serialize};

/// How a main-chat Turn ended. `Interrupted` is the user's own cancellation
/// (Stop / Steer); `Failed` covers provider, model, context-overflow, and
/// every other execution failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

/// One main-chat Turn's durable end. `eventId` is stable per event so
/// consumers can deduplicate duplicate transport delivery; consumers must
/// never derive completions from Session snapshots instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTerminalEvent {
    /// Stable per-event identity (a fresh UUID per settled Turn).
    pub event_id: String,
    pub chat_id: String,
    /// The queued message whose Turn this was.
    pub message_id: String,
    pub outcome: TurnOutcome,
    /// Completion timestamp (epoch milliseconds), stamped after queue
    /// completion was durably recorded.
    pub finished_at: i64,
    /// Engine-internal failure diagnostics. Never user-facing: notification
    /// and hook consumers must not display it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_reason: Option<String>,
}
