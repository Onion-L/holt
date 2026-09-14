//! Live working-tree Git status for the File sidebar's decorations
//! (ticket 10): the `WatchWorkspaceGitStatus` stream. One recursive notify
//! watcher per subscription root — covering the working tree AND `.git`
//! (staging moves, HEAD switches), which the checkout-diff watch's checksum
//! gate would miss — with the git watch's 200ms quiet window. Arming the
//! watcher blocks its caller (the macOS backend waits for the watcher's
//! private runloop thread, which under load can take seconds), so it runs
//! on the blocking pool; until it lands — and for roots the fs watcher
//! cannot cover at all — the stream polls at `POLL_INTERVAL`. Every
//! recompute runs through the engine's `Git` capability under the
//! per-checkout lock, and a frame is emitted only when the snapshot
//! actually changed, so the UI's fold stays idempotent. A non-Git root
//! streams `workdir: null` and turns live the moment a repository appears
//! under it. Dropping the stream ends the watch.

use std::{path::PathBuf, time::Instant};

use notify::Watcher as _;

use crate::git::Git;
use crate::git_watch::Debounce;

/// Poll cadence for roots without a working fs watcher — and for the arming
/// window before one lands.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// A subscription stream over one root: emits a `WorkspaceGitStatus`
/// snapshot immediately, then a fresh one each time the quiet window lapses
/// after filesystem activity — but only when the status actually moved.
/// The task ends when the receiver drops (the send fails) or the watcher
/// dies.
pub(crate) fn subscribe(
    root: PathBuf,
    git: Git,
) -> Result<futures::channel::mpsc::Receiver<serde_json::Value>, String> {
    let (mut out_tx, out_rx) = futures::channel::mpsc::channel::<serde_json::Value>(16);
    tokio::spawn(async move {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<notify::Event>(256);
        // Arming never runs on the runtime's thread (see module docs); the
        // stream opens and polls while the handshake runs, and the result —
        // the armed watcher or the reason it could not — lands by harvest.
        let mut arming = Some(tokio::task::spawn_blocking({
            let root = root.clone();
            move || {
                let mut watcher =
                    notify::recommended_watcher(move |result: Result<notify::Event, _>| {
                        if let Ok(event) = result {
                            let _ = event_tx.blocking_send(event);
                        }
                    })
                    .ok();
                let error = watcher.as_mut().and_then(|watcher| {
                    watcher
                        .watch(&root, notify::RecursiveMode::Recursive)
                        .err()
                        .map(|error| error.to_string())
                });
                (watcher, error)
            }
        }));
        // Holds the armed watcher for the task's lifetime — dropping it
        // ends the fs watch. (Underscore name: never read, only kept.)
        let mut _armed_watcher: Option<notify::RecommendedWatcher> = None;
        // A root that cannot be watched (yet) still streams — it polls
        // instead of failing the whole subscription.
        let mut poll_due = Some(Instant::now() + POLL_INTERVAL);
        // Set once when the arming lands: one recompute heals anything the
        // pre-arming window's polls could not have seen.
        let mut catch_up = false;
        let mut debounce = Debounce::new();
        let repo_path = root.display().to_string();
        let mut last: Option<holt_proto::WorkspaceGitStatus> = None;

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // Harvest the arming result without ever waiting on it in the
            // loop's arms — the handshake can take seconds.
            if arming.as_ref().is_some_and(|job| job.is_finished()) {
                let (armed, error) = arming
                    .take()
                    .unwrap()
                    .await
                    .unwrap_or((None, Some("arming task panicked".to_string())));
                let armed_ok = armed.is_some() && error.is_none();
                if let Some(error) = error {
                    tracing::warn!(
                        %error,
                        root = %root.display(),
                        "git status watch unavailable"
                    );
                }
                _armed_watcher = armed;
                if armed_ok {
                    poll_due = None;
                }
                catch_up = true;
            }
            tokio::select! {
                event = event_rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                    debounce.event(Instant::now());
                }
                _ = ticker.tick() => {
                    // The subscription is gone even on a quiet tree — end
                    // the watch instead of ticking forever.
                    if out_tx.is_closed() {
                        break;
                    }
                    let now = Instant::now();
                    // `last` is None only before the opening snapshot: fire
                    // on the first tick instead of waiting for an event.
                    let due = last.is_none()
                        || std::mem::take(&mut catch_up)
                        || debounce.tick(now)
                        || poll_due.is_some_and(|due| now >= due);
                    if !due {
                        continue;
                    }
                    if poll_due.is_some() {
                        poll_due = Some(now + POLL_INTERVAL);
                    }
                    let snapshot = git.workspace_status(&repo_path).await;
                    if last.as_ref() != Some(&snapshot) {
                        last = Some(snapshot.clone());
                        if let Ok(value) = serde_json::to_value(&snapshot)
                            && out_tx.try_send(value).is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(out_rx)
}
