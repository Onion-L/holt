//! Live working-tree Git status for the File sidebar's decorations
//! (ticket 10): the `WatchWorkspaceGitStatus` stream. One recursive notify
//! watcher per subscription root — covering the working tree AND `.git`
//! (staging moves, HEAD switches), which the checkout-diff watch's checksum
//! gate would miss — with the git watch's 200ms quiet window. Roots the fs
//! watcher cannot cover degrade to 2s polling. Every recompute runs through
//! the engine's `Git` capability under the per-checkout lock, and a frame
//! is emitted only when the snapshot actually changed, so the UI's fold
//! stays idempotent. A non-Git root streams `workdir: null` and turns live
//! the moment a repository appears under it. Dropping the stream ends the
//! watch.

use std::{path::PathBuf, time::Instant};

use notify::Watcher as _;

use crate::git::Git;
use crate::git_watch::Debounce;

/// Poll cadence for roots without a working fs watcher.
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
        let mut watcher = notify::recommended_watcher(move |result: Result<notify::Event, _>| {
            if let Ok(event) = result {
                let _ = event_tx.blocking_send(event);
            }
        })
        .ok();
        // A root that cannot be watched still streams — it polls instead of
        // failing the whole subscription.
        let mut poll_due = match watcher.as_mut() {
            Some(watcher) => match watcher.watch(&root, notify::RecursiveMode::Recursive) {
                Ok(()) => None,
                Err(error) => {
                    tracing::warn!(%error, root = %root.display(), "git status watch unavailable");
                    Some(Instant::now() + POLL_INTERVAL)
                }
            },
            None => Some(Instant::now() + POLL_INTERVAL),
        };
        let mut debounce = Debounce::new();
        let repo_path = root.display().to_string();
        let mut last: Option<holt_proto::WorkspaceGitStatus> = None;

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
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
