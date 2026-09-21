//! Turn change sets (ADR-0024, ticket 01): the net Git change between a
//! main-chat Turn's admission baseline and its live or final working tree.
//!
//! This module is the in-memory handoff: a chat's record is replaced when
//! its next Turn is admitted, and an engine restart drops every record.
//! Settled Turns persist through `turn_change_store` (ticket 02), whose
//! records answer by message id once these are gone.
//!
//! Two records per chat are kept so the final result survives an
//! auto-advanced next Turn: the current Turn, plus the most recently settled
//! Turn, addressable by its message id. That is a bounded in-memory handoff,
//! not history.
//!
//! Subagent edits need no special handling: a child shares the parent Turn's
//! working directory, so the parent's baseline already covers them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use holt_proto::{TurnChangeSet, TurnChangeSetPhase};
use tokio::sync::Notify;

use crate::git::{Git, GitFault, TurnBaseline, TurnChangeCapture};

/// One live Turn's attributed write paths (repo-root-relative): the paths
/// the Turn's own tools mutated — write/edit targets plus each bash
/// command's before/after worktree delta. The change set keeps only files
/// in this set, so another chat's concurrent work in the same working
/// tree can never land in this Turn's card. Subagents record into the
/// parent Turn's set: they share its working directory, like its baseline.
#[derive(Default)]
pub(crate) struct Attribution {
    paths: Mutex<HashSet<String>>,
}

impl Attribution {
    pub(crate) fn record<I: IntoIterator<Item = String>>(&self, paths: I) {
        let mut guard = self.paths.lock().unwrap_or_else(|error| error.into_inner());
        guard.extend(paths);
    }

    pub(crate) fn snapshot(&self) -> HashSet<String> {
        self.paths
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// The chat's current Turn: its identity, working directory, admission
/// baseline, and — once the Turn settled — its frozen final change set.
struct TurnRecord {
    message_id: String,
    cwd: String,
    baseline: TurnBaseline,
    attribution: Arc<Attribution>,
    /// The Turn is over (succeeded, failed, or interrupted): the change set
    /// no longer moves.
    settled: bool,
    /// The frozen final change set. `None` after a failed settle-time
    /// capture — the first successful read freezes it instead.
    final_change: Option<TurnChangeCapture>,
}

/// The chat's current Turn and its most recently settled predecessor.
#[derive(Default)]
struct Registry {
    current: HashMap<String, TurnRecord>,
    settled: HashMap<String, TurnRecord>,
}

/// One Turn cloned out of the registry, so a reader never holds the lock
/// across a Git capture. The baseline patch can reach the shared 3 MiB cap,
/// so reads are not on a hot path.
pub(crate) struct TurnSnapshot {
    pub message_id: String,
    pub cwd: String,
    pub baseline: TurnBaseline,
    pub attribution: Arc<Attribution>,
    pub settled: bool,
    pub final_change: Option<TurnChangeCapture>,
}

/// Every chat's current Turn change set.
#[derive(Default)]
pub(crate) struct TurnChanges {
    inner: Mutex<Registry>,
    /// Per settled Turn, keyed `(chat_id, message_id)`: the one-shot signal
    /// a change-set watcher fires once the Turn's final frame is out to its
    /// subscriber. The queue driver parks the next queued Turn on it, so
    /// the UI's card lands before the queued message's own doc frames
    /// (user-visible order). Armed only while the chat has a live watcher
    /// ([`WatcherClaim`]): with nobody to order against there is nothing
    /// to hold, and the queue must not pay the grace.
    final_emitted: Mutex<HashMap<(String, String), Arc<Notify>>>,
    /// Per chat, how many WatchTurnChangeSet subscriptions are live.
    watchers: Mutex<HashMap<String, usize>>,
}

impl TurnChanges {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the Turn baseline at admission (ADR-0024), before execution
    /// begins. A settled predecessor stays addressable by its message id
    /// until the next Turn replaces it.
    pub(crate) fn begin(&self, chat_id: &str, message_id: &str, cwd: &str, baseline: TurnBaseline) {
        let mut registry = self.registry();
        let previous = registry.current.insert(
            chat_id.to_string(),
            TurnRecord {
                message_id: message_id.to_string(),
                cwd: cwd.to_string(),
                baseline,
                attribution: Arc::new(Attribution::default()),
                settled: false,
                final_change: None,
            },
        );
        match previous {
            Some(previous) if previous.settled => {
                registry.settled.insert(chat_id.to_string(), previous);
            }
            Some(_) => {
                // A Turn replaced while still live never happened; keep no
                // record of it.
                registry.settled.remove(chat_id);
            }
            None => {}
        }
    }

    /// The chat's current Turn, if one was admitted.
    pub(crate) fn snapshot(&self, chat_id: &str) -> Option<TurnSnapshot> {
        self.registry().current.get(chat_id).map(TurnSnapshot::of)
    }

    /// A specific Turn of the chat — its current one, or the most recently
    /// settled one — so a final frame survives the next Turn's admission.
    pub(crate) fn snapshot_message(&self, chat_id: &str, message_id: &str) -> Option<TurnSnapshot> {
        let registry = self.registry();
        registry
            .current
            .get(chat_id)
            .filter(|record| record.message_id == message_id)
            .or_else(|| {
                registry
                    .settled
                    .get(chat_id)
                    .filter(|record| record.message_id == message_id)
            })
            .map(TurnSnapshot::of)
    }

    /// The current Turn's identity — the cheap poll the live watch uses to
    /// notice a new Turn without cloning its baseline.
    pub(crate) fn current_message(&self, chat_id: &str) -> Option<String> {
        self.registry()
            .current
            .get(chat_id)
            .map(|record| record.message_id.clone())
    }

    /// The named Turn's attribution recorder. Subagents resolve this against
    /// their PARENT chat at spawn — they share the parent Turn's write set.
    pub(crate) fn attribution(&self, chat_id: &str, message_id: &str) -> Option<Arc<Attribution>> {
        self.registry()
            .current
            .get(chat_id)
            .filter(|record| record.message_id == message_id)
            .map(|record| Arc::clone(&record.attribution))
    }

    /// Read the chat's current Turn change set: the frozen final once the
    /// Turn settled, a fresh live capture while it runs. `None` when no
    /// Turn was admitted.
    pub(crate) async fn read(
        &self,
        git: &Git,
        device_id: &str,
        chat_id: &str,
    ) -> Result<Option<TurnChangeSet>, GitFault> {
        let Some(snapshot) = self.snapshot(chat_id) else {
            return Ok(None);
        };
        self.read_snapshot(git, device_id, chat_id, &snapshot).await
    }

    /// Read one specific Turn by message id — the settled final of the
    /// previous Turn stays readable after the next Turn begins.
    pub(crate) async fn read_message(
        &self,
        git: &Git,
        device_id: &str,
        chat_id: &str,
        message_id: &str,
    ) -> Result<Option<TurnChangeSet>, GitFault> {
        let Some(snapshot) = self.snapshot_message(chat_id, message_id) else {
            return Ok(None);
        };
        self.read_snapshot(git, device_id, chat_id, &snapshot).await
    }

    /// Mark the chat's current Turn settled (ADR-0024) the instant its
    /// durable records do, so a reader can never see a settled queue beside
    /// a still-live change set. The frozen capture lands with
    /// [`TurnChanges::freeze`] or the first read.
    pub(crate) fn settle(&self, chat_id: &str, message_id: &str) {
        self.update(chat_id, message_id, |record| record.settled = true);
    }

    /// Arm the final-frame emission signal for a settled Turn. Call BEFORE
    /// the terminal event publishes — the watcher may fire within the same
    /// scheduler tick. `None` when no watcher holds the chat: there is no
    /// subscriber to order the next Turn against, so the driver skips the
    /// wait entirely instead of burning the grace.
    pub(crate) fn arm_final_signal(&self, chat_id: &str, message_id: &str) -> Option<Arc<Notify>> {
        if !self
            .watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(chat_id)
        {
            return None;
        }
        let signal = Arc::new(Notify::new());
        self.final_emitted
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                (chat_id.to_string(), message_id.to_string()),
                signal.clone(),
            );
        Some(signal)
    }

    /// The watcher's acknowledgement: the final frame left for the
    /// subscriber. Releases the queue driver's bounded wait.
    pub(crate) fn fire_final_signal(&self, chat_id: &str, message_id: &str) {
        let signal = self
            .final_emitted
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(chat_id.to_string(), message_id.to_string()));
        if let Some(signal) = signal {
            // `notify_one`, not `notify_waiters`: the driver may not have
            // reached its wait yet, and a stored permit is consumed on its
            // first poll instead of being lost.
            signal.notify_one();
        }
    }

    /// Drop an unfired signal once the queue driver's bounded wait expired.
    /// A late watcher fire re-arms nothing: the card then lands late, as it
    /// did before this ordering existed.
    pub(crate) fn clear_final_signal(&self, chat_id: &str, message_id: &str) {
        self.final_emitted
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(chat_id.to_string(), message_id.to_string()));
    }

    /// Freeze the settled Turn's final change set (ADR-0024): store an
    /// already-captured change set immutably, only while the record still
    /// belongs to `message_id`. The caller captures once — summary and
    /// content in one Git pass (`Git::turn_change_freeze`) — then freezes
    /// and persists; the freeze runs before the Turn terminal event is
    /// published, so the final set is what consumers see with the Turn
    /// result. A capture failure never fails the Turn — the first
    /// successful read freezes instead.
    pub(crate) fn freeze(&self, chat_id: &str, message_id: &str, capture: TurnChangeCapture) {
        self.update(chat_id, message_id, |record| {
            record.settled = true;
            if record.final_change.is_none() {
                record.final_change = Some(capture);
            }
        });
    }

    async fn read_snapshot(
        &self,
        git: &Git,
        device_id: &str,
        chat_id: &str,
        snapshot: &TurnSnapshot,
    ) -> Result<Option<TurnChangeSet>, GitFault> {
        let capture = match &snapshot.final_change {
            Some(capture) => capture.clone(),
            None => {
                let attributed = snapshot.attribution.snapshot();
                let capture = git
                    .turn_change_capture(&snapshot.cwd, device_id, &snapshot.baseline, &attributed)
                    .await?;
                if snapshot.settled {
                    // The settle-time capture failed: freeze the first
                    // successful read so a settled Turn's content still
                    // stops moving.
                    let message_id = snapshot.message_id.clone();
                    self.update(chat_id, &message_id, |record| {
                        if record.final_change.is_none() {
                            record.final_change = Some(capture.clone());
                        }
                    });
                }
                capture
            }
        };
        let phase = if snapshot.settled {
            TurnChangeSetPhase::Final
        } else {
            TurnChangeSetPhase::Live
        };
        Ok(Some(change_set(
            chat_id,
            &snapshot.message_id,
            phase,
            &capture,
            chrono::Utc::now(),
        )))
    }

    /// Apply `update` to the record identified by `(chat_id, message_id)`,
    /// whether it is the chat's current Turn or its last settled one.
    fn update(&self, chat_id: &str, message_id: &str, update: impl FnOnce(&mut TurnRecord)) {
        let mut registry = self.registry();
        if let Some(record) = registry
            .current
            .get_mut(chat_id)
            .filter(|record| record.message_id == message_id)
        {
            update(record);
            return;
        }
        if let Some(record) = registry
            .settled
            .get_mut(chat_id)
            .filter(|record| record.message_id == message_id)
        {
            update(record);
        }
    }

    /// A live WatchTurnChangeSet subscription's claim on the chat: while at
    /// least one is held, [`TurnChanges::arm_final_signal`] arms. Drop —
    /// any task exit path, panic unwind included — releases the claim.
    pub(crate) fn watcher_claim(self: &Arc<Self>, chat_id: &str) -> WatcherClaim {
        *self
            .watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(chat_id.to_string())
            .or_insert(0) += 1;
        WatcherClaim {
            changes: self.clone(),
            chat_id: chat_id.to_string(),
        }
    }

    fn release_watcher(&self, chat_id: &str) {
        let mut watchers = self
            .watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(count) = watchers.get_mut(chat_id)
            && *count > 0
        {
            *count -= 1;
            if *count == 0 {
                watchers.remove(chat_id);
            }
        }
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// The [`TurnChanges::watcher_claim`] ticket. See its doc comment.
pub(crate) struct WatcherClaim {
    changes: Arc<TurnChanges>,
    chat_id: String,
}

impl Drop for WatcherClaim {
    fn drop(&mut self) {
        self.changes.release_watcher(&self.chat_id);
    }
}

impl TurnSnapshot {
    fn of(record: &TurnRecord) -> Self {
        Self {
            message_id: record.message_id.clone(),
            cwd: record.cwd.clone(),
            baseline: record.baseline.clone(),
            attribution: Arc::clone(&record.attribution),
            settled: record.settled,
            final_change: record.final_change.clone(),
        }
    }
}

/// Compose the wire change set for one capture. `updated_at` is the read
/// time for a live capture, or the persisted settle time for a restored
/// record.
pub(crate) fn change_set(
    chat_id: &str,
    message_id: &str,
    phase: TurnChangeSetPhase,
    capture: &TurnChangeCapture,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> TurnChangeSet {
    TurnChangeSet {
        chat_id: chat_id.to_string(),
        message_id: message_id.to_string(),
        phase,
        files: capture.files.clone(),
        additions: capture.additions,
        deletions: capture.deletions,
        truncated: capture.truncated,
        updated_at,
    }
}

/// How many settled Turns a chat may keep queued for the watch's final
/// frames. `begin` retires at most one per Turn; a burst larger than this
/// would only drop frames the UI would have replaced anyway.
pub(crate) const PENDING_FINALS_CAP: usize = 16;

/// A bounded FIFO of message ids whose terminal event the watch still has to
/// turn into a final frame.
#[derive(Default)]
pub(crate) struct PendingFinals(VecDeque<String>);

impl PendingFinals {
    pub(crate) fn push(&mut self, message_id: String) {
        if self.0.contains(&message_id) {
            return;
        }
        if self.0.len() == PENDING_FINALS_CAP {
            self.0.pop_front();
        }
        self.0.push_back(message_id);
    }

    pub(crate) fn pop(&mut self) -> Option<String> {
        self.0.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn attribution_records_and_snapshots_paths() {
        let attribution = Attribution::default();
        assert!(attribution.snapshot().is_empty());
        attribution.record(["a.txt".to_string(), "b.txt".to_string()]);
        attribution.record(Vec::<String>::new());
        let snapshot = attribution.snapshot();
        assert!(snapshot.contains("a.txt") && snapshot.contains("b.txt"));
        assert_eq!(snapshot.len(), 2, "an empty record adds nothing");
    }

    #[tokio::test]
    async fn final_signal_releases_a_late_waiter_and_clears() {
        let changes = Arc::new(TurnChanges::new());
        // With no watcher claim nothing arms: the queue driver must not
        // wait on a chat nobody watches.
        assert!(changes.arm_final_signal("chat", "m-1").is_none());
        let _claim = changes.watcher_claim("chat");
        let signal = changes
            .arm_final_signal("chat", "m-1")
            .expect("armed while a watcher holds the chat");
        // The watcher fires before the driver reaches its wait: the stored
        // permit must still release it (`notify_one`, not `notify_waiters`).
        changes.fire_final_signal("chat", "m-1");
        tokio::time::timeout(Duration::from_millis(10), signal.notified())
            .await
            .expect("a fired signal releases the waiter");
        // The fire consumed the registry entry; a second fire is inert.
        changes.fire_final_signal("chat", "m-1");

        // A cleared (expired) signal never fires, and re-arming hands out a
        // fresh one with no stale permit.
        let signal = changes
            .arm_final_signal("chat", "m-1")
            .expect("re-armed while still watched");
        changes.clear_final_signal("chat", "m-1");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), signal.notified())
                .await
                .is_err(),
            "a cleared signal never fires"
        );

        // The claim gone (chat closed), arming is refused again.
        drop(_claim);
        assert!(changes.arm_final_signal("chat", "m-1").is_none());
    }

    #[tokio::test]
    async fn watcher_claims_count_and_release() {
        let changes = Arc::new(TurnChanges::new());
        let first = changes.watcher_claim("chat");
        let _second = changes.watcher_claim("chat");
        let signal = changes
            .arm_final_signal("chat", "m-1")
            .expect("armed while a watcher holds the chat");
        drop(first);
        // Two claims overlapped: one dropping must not release the last.
        changes.fire_final_signal("chat", "m-1");
        tokio::time::timeout(Duration::from_millis(10), signal.notified())
            .await
            .expect("the overlapped claim keeps the signal live");
        drop(_second);
        assert!(changes.arm_final_signal("chat", "m-1").is_none());
    }
}
