//! Turn change sets (ADR-0024, ticket 01): the net Git change between a
//! main-chat Turn's admission baseline and its live or final working tree.
//!
//! In-memory: a chat's record is replaced when its next Turn is admitted,
//! and an engine restart drops every record. Persisting settled Turns and
//! replaying their per-file content is ticket 02's slice.
//!
//! Two records per chat are kept so the final result survives an
//! auto-advanced next Turn: the current Turn, plus the most recently settled
//! Turn, addressable by its message id. That is a bounded in-memory handoff,
//! not history.
//!
//! Subagent edits need no special handling: a child shares the parent Turn's
//! working directory, so the parent's baseline already covers them.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use holt_proto::{TurnChangeSet, TurnChangeSetPhase};

use crate::git::{Git, GitFault, TurnBaseline, TurnChangeCapture};

/// The chat's current Turn: its identity, working directory, admission
/// baseline, and — once the Turn settled — its frozen final change set.
struct TurnRecord {
    message_id: String,
    cwd: String,
    baseline: TurnBaseline,
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
    pub settled: bool,
    pub final_change: Option<TurnChangeCapture>,
}

/// Every chat's current Turn change set.
#[derive(Default)]
pub(crate) struct TurnChanges {
    inner: Mutex<Registry>,
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
    /// [`TurnChanges::finish`] or the first read.
    pub(crate) fn settle(&self, chat_id: &str, message_id: &str) {
        self.update(chat_id, message_id, |record| record.settled = true);
    }

    /// Freeze the settled Turn's final change set (ADR-0024): capture from
    /// the still-current baseline and store it immutably, only while the
    /// record still belongs to `message_id`. Runs before the Turn terminal
    /// event is published, so the final set is what consumers see with the
    /// Turn result. A capture failure never fails the Turn — the first
    /// successful read freezes instead.
    pub(crate) async fn finish(&self, git: &Git, device_id: &str, chat_id: &str, message_id: &str) {
        let Some(snapshot) = self.snapshot_message(chat_id, message_id) else {
            return;
        };
        let Ok(capture) = git
            .turn_change_capture(&snapshot.cwd, device_id, &snapshot.baseline)
            .await
        else {
            return;
        };
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
                let capture = git
                    .turn_change_capture(&snapshot.cwd, device_id, &snapshot.baseline)
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

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl TurnSnapshot {
    fn of(record: &TurnRecord) -> Self {
        Self {
            message_id: record.message_id.clone(),
            cwd: record.cwd.clone(),
            baseline: record.baseline.clone(),
            settled: record.settled,
            final_change: record.final_change.clone(),
        }
    }
}

/// Compose the wire change set for one capture.
pub(crate) fn change_set(
    chat_id: &str,
    message_id: &str,
    phase: TurnChangeSetPhase,
    capture: &TurnChangeCapture,
) -> TurnChangeSet {
    TurnChangeSet {
        chat_id: chat_id.to_string(),
        message_id: message_id.to_string(),
        phase,
        files: capture.files.clone(),
        additions: capture.additions,
        deletions: capture.deletions,
        truncated: capture.truncated,
        updated_at: chrono::Utc::now(),
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
