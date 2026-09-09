//! The Turn terminal event dispatcher (ADR-0019): fire-and-forget fan-out of
//! durably settled main-chat Turn outcomes to `WatchTurnTerminalEvents`
//! subscribers. Publishing never awaits consumer work and never fails the
//! Turn — a closed or lagging consumer is the consumer's failure. Events are
//! live-only: nothing is persisted or replayed across restart.

use holt_rpc::turns::TurnTerminalEvent;
use tokio::sync::broadcast;

#[derive(Clone)]
pub(crate) struct TurnEvents {
    tx: broadcast::Sender<TurnTerminalEvent>,
}

impl TurnEvents {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }

    /// Fan out one settled Turn's event. A send only fails when nobody is
    /// listening, which changes nothing about the finished Turn.
    pub(crate) fn publish(&self, event: TurnTerminalEvent) {
        let _ = self.tx.send(event);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<TurnTerminalEvent> {
        self.tx.subscribe()
    }
}
