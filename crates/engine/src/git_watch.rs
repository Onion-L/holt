//! Live checkout-diff awareness for git spaces: the `WatchCheckoutDiffs`
//! stream (spec: git-capability issue 03).
//!
//! One shared watcher per watched checkout roots its `notify` watcher at the
//! space root (which covers `.git/HEAD`) with debouncing. Arming — both
//! creating the watcher and attaching a root — blocks its caller (the macOS
//! backend waits for the watcher's private runloop thread, which under load
//! can take seconds), so every notify call runs on the blocking pool and the
//! watch loop keeps ticking meanwhile; roots poll at 2 s until their attach
//! lands. Roots whose attach fails keep the polling fallback. Watching starts
//! with the first subscriber and stops with the last. A subscribe first
//! emits a full snapshot of every watched checkout, then one frame per
//! checkout change — and only when that checkout's checksum changed, so the
//! UI's checksum-keyed fold stays idempotent.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures::StreamExt;
use holt_proto::{CheckoutDiff, Space};
use notify::Watcher as _;
use tokio::sync::broadcast;

use crate::git::Git;

/// Quiet period a watched checkout must stay silent for before its change
/// fires — long enough to swallow an editor's write-temp-then-rename churn,
/// short enough to feel live.
const DEBOUNCE_QUIET: Duration = Duration::from_millis(200);

/// Poll cadence for roots without a working fs watcher.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Debounce for one watched checkout: events keep pushing the deadline out;
/// the change fires on the first tick past the deadline. Pure — all time is
/// injected, so the state machine is unit-testable without sleeps.
#[derive(Debug, Default)]
pub(crate) struct Debounce {
    deadline: Option<Instant>,
}

impl Debounce {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// An fs event arrived: (re)arm the quiet deadline.
    pub(crate) fn event(&mut self, now: Instant) {
        self.deadline = Some(now + DEBOUNCE_QUIET);
    }

    /// Clock tick: fires `true` exactly once, when the quiet period has
    /// fully elapsed since the last event.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }
}

struct WatchState {
    subscribers: usize,
    cancel: Option<tokio_util::sync::CancellationToken>,
    frames: broadcast::Sender<serde_json::Value>,
}

/// Shared hub behind `WatchCheckoutDiffs`. Cloned into every subscriber
/// stream; the watch task runs while any subscriber lives.
pub(crate) struct WatchHub {
    git: Git,
    device_id: String,
    spaces: Arc<std::sync::RwLock<Vec<Space>>>,
    spaces_tx: tokio::sync::watch::Receiver<serde_json::Value>,
    state: Mutex<WatchState>,
}

/// Decrements the subscriber count on drop; the last one out stops the
/// watch task.
struct SubscriberGuard {
    hub: Arc<WatchHub>,
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        let mut state = self.hub.state.lock().unwrap_or_else(|e| e.into_inner());
        state.subscribers = state.subscribers.saturating_sub(1);
        if state.subscribers == 0
            && let Some(cancel) = state.cancel.take()
        {
            cancel.cancel();
        }
    }
}

/// One watched checkout.
struct Watched {
    root: PathBuf,
    debounce: Debounce,
    /// Poll deadline while no fs watcher covers this root — from insert
    /// until the root's attach lands (or forever, when it fails).
    poll_due: Option<Instant>,
    /// The fs watcher covers this root; only a plan pass re-arms it.
    watching: bool,
    /// Last emitted checksum — frames fire only when it changes.
    last_checksum: Option<String>,
}

/// The attach work one sync batch performs, planned on the loop and executed
/// off-thread: drop these roots, attach those.
#[derive(Clone)]
struct SyncBatch {
    removed: Vec<PathBuf>,
    fresh: Vec<PathBuf>,
}

impl WatchHub {
    pub(crate) fn new(
        git: Git,
        device_id: String,
        spaces: Arc<std::sync::RwLock<Vec<Space>>>,
        spaces_tx: tokio::sync::watch::Receiver<serde_json::Value>,
    ) -> Self {
        let (frames, _) = broadcast::channel(64);
        Self {
            git,
            device_id,
            spaces,
            spaces_tx,
            state: Mutex::new(WatchState {
                subscribers: 0,
                cancel: None,
                frames,
            }),
        }
    }

    /// The `WatchCheckoutDiffs` reply: first a full snapshot of every
    /// watched checkout, then one frame per change.
    pub(crate) fn subscribe(self: &Arc<Self>) -> holt_rpc::RpcReply {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.subscribers += 1;
            if state.subscribers == 1 {
                let cancel = tokio_util::sync::CancellationToken::new();
                state.cancel = Some(cancel.clone());
                tokio::spawn({
                    let hub = self.clone();
                    async move { hub.run(cancel).await }
                });
            }
        }
        let hub = self.clone();
        let guard = SubscriberGuard { hub: self.clone() };
        let frames = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.frames.subscribe()
        };
        // The receiver rides the unfold state (it is not cloneable here), so
        // each poll reuses the same subscription; the guard dies with the
        // stream.
        let stream = futures::stream::unfold(
            (hub, frames, guard, true),
            |(hub, mut frames, guard, first)| async move {
                if first {
                    // Opening snapshot of all watched checkouts (list frame;
                    // the UI folds a full list as a replace-all).
                    let snapshot = hub.snapshot_all().await;
                    let value = serde_json::to_value(&snapshot).ok()?;
                    return Some((value, (hub, frames, guard, false)));
                }
                loop {
                    match frames.recv().await {
                        Ok(value) => return Some((value, (hub, frames, guard, false))),
                        // A slow consumer missed frames: keep the stream
                        // alive — the UI's upsert fold heals on the next
                        // live frame. Only a closed channel ends the stream.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            },
        );
        holt_rpc::RpcReply::Stream(stream.boxed())
    }

    /// Fresh captures for every git-detected space (failed reads skipped —
    /// a vanished folder must not kill the stream).
    async fn snapshot_all(&self) -> Vec<CheckoutDiff> {
        let mut out = Vec::new();
        for space in git_spaces(&self.spaces) {
            if let Ok(diff) = self.git.working_tree(&space.path, &self.device_id).await {
                out.push(diff);
            }
        }
        out
    }

    /// The watch loop: own the notify watcher, debounce events, poll
    /// uncovered roots, follow space changes, emit checksum-gated frames.
    async fn run(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        let mut spaces_rx = self.spaces_tx.clone();
        let frames = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .frames
            .clone();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<notify::Event>(256);
        // Creating the watcher is cheap but still blocking-flavored; keep it
        // off the runtime's thread like every other notify call.
        let watcher = tokio::task::spawn_blocking(move || {
            notify::recommended_watcher(move |result: Result<notify::Event, _>| {
                if let Ok(event) = result {
                    // The callback runs on the watcher's own thread.
                    let _ = event_tx.blocking_send(event);
                }
            })
            .ok()
        })
        .await
        .ok()
        .flatten();
        let watcher = Arc::new(tokio::sync::Mutex::new(watcher));
        // A watcher that cannot be created at all leaves every root on the
        // polling fallback (attach batches return no outcomes then).
        // Every git-detected space starts on the polling fallback: its
        // attach runs in a background batch (the handshake can take
        // seconds), and until the batch lands the polls must not miss.
        let now = Instant::now();
        let mut watched: HashMap<PathBuf, Watched> = git_spaces(&self.spaces)
            .into_iter()
            .map(|space| {
                let root = PathBuf::from(&space.path);
                let entry = Watched {
                    root: root.clone(),
                    debounce: Debounce::new(),
                    poll_due: Some(now + POLL_INTERVAL),
                    watching: false,
                    last_checksum: None,
                };
                (root, entry)
            })
            .collect();
        // Seed each checkout's last-known checksum so the loop only emits
        // on real changes; the first subscriber's opening list carries the
        // same data, so nothing is lost by not emitting here.
        for entry in watched.values_mut() {
            if let Ok(diff) = self
                .git
                .working_tree(&entry.root.display().to_string(), &self.device_id)
                .await
            {
                entry.last_checksum = Some(diff.checksum);
            }
        }

        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // One attach batch at a time; `sync_needed` requests the next one.
        let mut sync_needed = true;
        let mut syncing: Option<(SyncBatch, tokio::task::JoinHandle<HashMap<PathBuf, bool>>)> =
            None;
        // Set when a sync batch lands: one recompute pass heals anything the
        // pre-attach polls could not have seen.
        let mut catch_up = false;
        loop {
            // Harvest a finished attach batch without ever waiting on it in
            // the select arms.
            if syncing.as_ref().is_some_and(|(_, job)| job.is_finished()) {
                let (batch, job) = syncing.take().unwrap();
                let outcomes = job.await.unwrap_or_default();
                apply_sync(&mut watched, &batch, &outcomes);
                catch_up = true;
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = event_rx.recv() => {
                    let Some(event) = event else { break };
                    let now = Instant::now();
                    for root in roots_for_event(&watched, &event) {
                        if let Some(entry) = watched.get_mut(&root) {
                            entry.debounce.event(now);
                        }
                    }
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    if syncing.is_none() && std::mem::take(&mut sync_needed) {
                        let batch = plan_sync(&watched, &self.spaces);
                        syncing = Some((batch.clone(), spawn_sync(watcher.clone(), batch)));
                    }
                    let recompute_all = std::mem::take(&mut catch_up);
                    for entry in watched.values_mut() {
                        let due = recompute_all
                            || entry.debounce.tick(now)
                            || entry.poll_due.is_some_and(|due| now >= due);
                        if !due {
                            continue;
                        }
                        if entry.poll_due.is_some() {
                            entry.poll_due = Some(now + POLL_INTERVAL);
                        }
                        if let Ok(diff) = self
                            .git
                            .working_tree(&entry.root.display().to_string(), &self.device_id)
                            .await
                            && entry.last_checksum.as_deref() != Some(diff.checksum.as_str())
                        {
                            entry.last_checksum = Some(diff.checksum.clone());
                            if let Ok(value) = serde_json::to_value(&diff) {
                                // No subscribers means Err — nothing to do.
                                let _ = frames.send(value);
                            }
                        }
                    }
                }
                _ = spaces_rx.changed() => {
                    sync_needed = true;
                }
            }
        }
    }
}

/// Plan one attach batch against the current git-detected spaces: roots the
/// map lost, and map entries (existing or not yet present) the fs watcher
/// does not cover yet. Pure map/spaces reading — safe on the loop.
fn plan_sync(
    watched: &HashMap<PathBuf, Watched>,
    spaces: &Arc<std::sync::RwLock<Vec<Space>>>,
) -> SyncBatch {
    let roots: Vec<PathBuf> = git_spaces(spaces)
        .iter()
        .map(|space| PathBuf::from(&space.path))
        .collect();
    let removed: Vec<PathBuf> = watched
        .keys()
        .filter(|root| !roots.contains(root))
        .cloned()
        .collect();
    let fresh: Vec<PathBuf> = roots
        .into_iter()
        .filter(|root| !watched.get(root).is_some_and(|entry| entry.watching))
        .collect();
    SyncBatch { removed, fresh }
}

/// Run one attach batch entirely off the runtime's thread: the shared
/// watcher is taken out of the guard, its `unwatch`/`watch` calls (each a
/// potential blocking handshake on macOS) run on the blocking pool, and the
/// watcher is put back. Returns the attach outcome per fresh root — empty
/// when the watcher is gone entirely, which leaves every root polling.
fn spawn_sync(
    watcher: Arc<tokio::sync::Mutex<Option<notify::RecommendedWatcher>>>,
    batch: SyncBatch,
) -> tokio::task::JoinHandle<HashMap<PathBuf, bool>> {
    tokio::spawn(async move {
        let mut guard = watcher.lock().await;
        let Some(current) = guard.take() else {
            return HashMap::new();
        };
        let job = tokio::task::spawn_blocking(move || {
            let mut current = current;
            for root in &batch.removed {
                let _ = current.unwatch(root);
            }
            let outcomes = batch
                .fresh
                .iter()
                .map(|root| {
                    (
                        root.clone(),
                        current
                            .watch(root, notify::RecursiveMode::Recursive)
                            .is_ok(),
                    )
                })
                .collect::<HashMap<PathBuf, bool>>();
            (current, outcomes)
        });
        match job.await {
            Ok((current, outcomes)) => {
                *guard = Some(current);
                outcomes
            }
            // The batch panicked and took the watcher with it; the guard
            // stays empty and every root keeps polling.
            Err(_) => HashMap::new(),
        }
    })
}

/// Fold a finished batch's outcomes into the map: removed roots drop,
/// fresh roots gain (or lose) fs coverage.
fn apply_sync(
    watched: &mut HashMap<PathBuf, Watched>,
    batch: &SyncBatch,
    outcomes: &HashMap<PathBuf, bool>,
) {
    for root in &batch.removed {
        watched.remove(root);
    }
    for root in &batch.fresh {
        let watching = outcomes.get(root).copied().unwrap_or(false);
        let poll_due = (!watching).then(|| Instant::now() + POLL_INTERVAL);
        match watched.get_mut(root) {
            Some(entry) => {
                entry.watching = watching;
                entry.poll_due = poll_due;
            }
            None => {
                watched.insert(
                    root.clone(),
                    Watched {
                        root: root.clone(),
                        debounce: Debounce::new(),
                        poll_due,
                        watching,
                        last_checksum: None,
                    },
                );
            }
        }
    }
}

fn git_spaces(spaces: &Arc<std::sync::RwLock<Vec<Space>>>) -> Vec<Space> {
    spaces
        .read()
        .map(|spaces| {
            spaces
                .iter()
                .filter(|space| space.git_detected)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Roots affected by one fs event. Path normalization differences (macOS
/// `/var` ↔ `/private/var`) make exact prefixes unreliable, so an event
/// that matches no root arms every root — a cheap recompute that only
/// emits when a checksum actually moved.
fn roots_for_event(roots: &HashMap<PathBuf, Watched>, event: &notify::Event) -> Vec<PathBuf> {
    let touched: Vec<PathBuf> = roots
        .keys()
        .filter(|root| {
            event
                .paths
                .iter()
                .any(|path| path == *root || path.starts_with(root))
        })
        .cloned()
        .collect();
    if touched.is_empty() {
        roots.keys().cloned().collect()
    } else {
        touched
    }
}

#[cfg(test)]
mod tests {
    use super::Debounce;
    use std::time::{Duration, Instant};

    #[test]
    fn debounce_fires_only_after_the_quiet_period() {
        let start = Instant::now();
        let mut debounce = Debounce::new();
        debounce.event(start);
        // Immediately after the event: still quiet.
        assert!(!debounce.tick(start + Duration::from_millis(50)));
        // Inside the quiet window a fresh event re-arms the deadline.
        debounce.event(start + Duration::from_millis(100));
        assert!(!debounce.tick(start + Duration::from_millis(250)));
        // Past the re-armed deadline it fires exactly once.
        assert!(debounce.tick(start + Duration::from_millis(400)));
        assert!(!debounce.tick(start + Duration::from_millis(500)));
    }

    #[test]
    fn debounce_without_events_never_fires() {
        let start = Instant::now();
        let mut debounce = Debounce::new();
        assert!(!debounce.tick(start + Duration::from_secs(10)));
    }
}
