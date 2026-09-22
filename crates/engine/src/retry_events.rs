//! The provider-retry notice dispatcher: fire-and-forget fan-out of live
//! provider-retry backoff signals to `WatchTurnRetry` subscribers. The same
//! contract as `turn_events` (ADR-0019) one level down: publishing never
//! awaits consumer work and never fails the run — a closed or lagging
//! consumer is the consumer's failure. Notices are live-only: nothing is
//! persisted or replayed across restart.

use holt_rpc::retries::TurnRetryNotice;
use tokio::sync::broadcast;

#[derive(Clone)]
pub(crate) struct RetryEvents {
    tx: broadcast::Sender<TurnRetryNotice>,
}

impl RetryEvents {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }

    /// Fan out one scheduled retry. A send only fails when nobody is
    /// listening, which changes nothing about the pending retry.
    pub(crate) fn publish(&self, notice: TurnRetryNotice) {
        let _ = self.tx.send(notice);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<TurnRetryNotice> {
        self.tx.subscribe()
    }
}
