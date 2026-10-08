//! Port of `pi-core/agent/src/harness/events.ts` and `result.ts`.
//!
//! `result.ts` collapses onto `std::result::Result` in the Rust port (the
//! `ok`/`err`/`isOk`/`isErr` helpers and the `TaggedError` factory have no
//! counterpart — the concrete error structs and `match` on their codes
//! serve the same purpose, documented in `MIGRATION.md`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The run outcome carried by `run_end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Aborted => "aborted",
            RunOutcome::Failed => "failed",
        }
    }
}

/// Port of `HarnessEventType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HarnessEventType {
    RunStart,
    RunEnd,
}

impl HarnessEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunStart => "run_start",
            Self::RunEnd => "run_end",
        }
    }
}

/// Port of `RunStartEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStartEvent {
    pub lane: String,
    pub run_id: String,
}

/// Port of `RunEndEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEndEvent {
    pub lane: String,
    pub run_id: String,
    pub outcome: RunOutcome,
    pub leaf_id: String,
}

/// Port of `HarnessEvent`.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessEvent {
    RunStart {
        lane: String,
        run_id: String,
    },
    RunEnd {
        lane: String,
        run_id: String,
        outcome: RunOutcome,
        leaf_id: String,
    },
}

impl HarnessEvent {
    /// The event discriminant.
    pub fn event_type(&self) -> HarnessEventType {
        match self {
            HarnessEvent::RunStart { .. } => HarnessEventType::RunStart,
            HarnessEvent::RunEnd { .. } => HarnessEventType::RunEnd,
        }
    }
}

/// Port of `HarnessEventListener` (sync listeners; async listeners use the
/// boxed-future form).
pub type HarnessEventListener = Arc<dyn Fn(&HarnessEvent) + Send + Sync>;

/// Port of `HarnessEventBus`.
#[derive(Default)]
pub struct HarnessEventBus {
    listeners: Mutex<HashMap<HarnessEventType, Vec<HarnessEventListener>>>,
    watch_listeners: Mutex<Vec<Arc<Mutex<WatchEntry>>>>,
}

struct WatchEntry {
    listener: Option<HarnessEventListener>,
    buffered: Vec<HarnessEvent>,
}

impl HarnessEventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a listener for future events of one type; returns an
    /// unsubscribe token id. Earlier events are not replayed.
    pub fn on(&self, event_type: HarnessEventType, listener: HarnessEventListener) -> usize {
        let mut listeners = lock(&self.listeners);
        let entry = listeners.entry(event_type).or_default();
        entry.push(listener);
        // The unsubscribe id is the listener's Arc pointer.
        Arc::as_ptr(entry.last().expect("just pushed")) as *const () as usize
    }

    /// Remove a listener registered via [`HarnessEventBus::on`].
    pub fn off(&self, event_type: HarnessEventType, listener_id: usize) {
        let mut listeners = lock(&self.listeners);
        if let Some(entry) = listeners.get_mut(&event_type) {
            entry.retain(|listener| (Arc::as_ptr(listener) as *const () as usize) != listener_id);
            if entry.is_empty() {
                listeners.remove(&event_type);
            }
        }
    }

    /// Publish an event to current event subscriptions and watch
    /// subscriptions (fire-and-forget; async results are not awaited,
    /// like the synchronous TypeScript `emit`).
    pub fn emit(&self, event: HarnessEvent) {
        let event_type = event.event_type();
        let direct = lock(&self.listeners)
            .get(&event_type)
            .cloned()
            .unwrap_or_default();
        for listener in direct {
            listener(&event);
        }

        let watchers = lock(&self.watch_listeners).clone();
        for entry in watchers {
            let mut entry = lock(&entry);
            match &entry.listener {
                Some(listener) => listener(&event),
                None => entry.buffered.push(event.clone()),
            }
        }
    }

    /// Port of `watch`: capture a snapshot, buffer events until `start`.
    pub fn watch<T>(&self, capture_snapshot: impl FnOnce() -> T) -> WatchHandle<T> {
        let entry = Arc::new(Mutex::new(WatchEntry {
            listener: None,
            buffered: Vec::new(),
        }));
        lock(&self.watch_listeners).push(Arc::clone(&entry));
        WatchHandle {
            snapshot: capture_snapshot(),
            entry,
        }
    }
}

/// Port of `WatchHandle`.
pub struct WatchHandle<T> {
    pub snapshot: T,
    entry: Arc<Mutex<WatchEntry>>,
}

impl<T> WatchHandle<T> {
    /// Start delivering buffered and future events to the listener.
    pub fn start(&self, listener: HarnessEventListener) {
        let mut entry = lock(&self.entry);
        // Flush buffered events while still buffering so reentrant
        // emissions preserve order.
        loop {
            let pending = std::mem::take(&mut entry.buffered);
            if pending.is_empty() {
                break;
            }
            for event in pending {
                listener(&event);
            }
        }
        entry.listener = Some(listener);
    }

    /// Stop watching and drop buffered events.
    pub fn unsubscribe(&self) {
        let mut entry = lock(&self.entry);
        entry.listener = None;
        entry.buffered.clear();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
