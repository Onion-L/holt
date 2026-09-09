//! Live filesystem awareness for the File sidebar: the
//! `WatchWorkspaceEntries` stream. One recursive notify watcher per
//! subscription root (the macOS platform watcher carries recursion
//! natively), events debounced with the same 200ms quiet window the git
//! watch uses, and each frame carries the absolute paths that changed so
//! the UI can re-list the affected directories and re-read clean open
//! files. Dirty buffers are the UI's business — this stream only reports
//! what moved on disk. Dropping the stream ends the watch.

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use notify::Watcher as _;

/// Quiet period a watched root must stay silent for before a frame fires.
const DEBOUNCE_QUIET: Duration = Duration::from_millis(200);

/// Paths accumulated between frames are bounded — a runaway generator that
/// rewrites thousands of files collapses into one frame with a cap.
const FRAME_PATH_CAP: usize = 512;

/// One watched root: the pending changed-path set plus its quiet deadline.
struct WatchedRoot {
    pending: Vec<String>,
    deadline: Option<Instant>,
}

/// A subscription stream over one root: emits a `WorkspaceWatchFrame` per
/// debounce window carrying the changed paths under that root. The task
/// ends when the receiver drops (the send fails) or the watcher dies.
pub(crate) fn subscribe(
    root: PathBuf,
) -> Result<futures::channel::mpsc::Receiver<serde_json::Value>, String> {
    let (mut out_tx, out_rx) = futures::channel::mpsc::channel::<serde_json::Value>(16);
    tokio::spawn(async move {
        let mut watched: HashMap<PathBuf, WatchedRoot> = HashMap::new();
        watched.insert(
            root.clone(),
            WatchedRoot {
                pending: Vec::new(),
                deadline: None,
            },
        );
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<notify::Event>(256);
        let mut watcher = notify::recommended_watcher(move |result: Result<notify::Event, _>| {
            if let Ok(event) = result {
                let _ = event_tx.blocking_send(event);
            }
        })
        .ok();
        // A root that cannot be watched still streams — it simply stays
        // quiet rather than failing the whole subscription.
        if let Some(watcher) = watcher.as_mut()
            && let Err(error) = watcher.watch(&root, notify::RecursiveMode::Recursive)
        {
            tracing::warn!(%error, root = %root.display(), "workspace watch unavailable");
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pending_frame: Vec<String> = Vec::new();
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else { break };
                    let now = Instant::now();
                    if let Some(entry) = watched.get_mut(&root) {
                        for path in &event.paths {
                            let display = path.display().to_string();
                            if !entry.pending.contains(&display)
                                && entry.pending.len() < FRAME_PATH_CAP
                            {
                                entry.pending.push(display);
                            }
                        }
                        entry.deadline = Some(now + DEBOUNCE_QUIET);
                    }
                }
                _ = ticker.tick() => {
                    // The subscription is gone even on a quiet tree — end
                    // the watch instead of ticking forever.
                    if out_tx.is_closed() {
                        break;
                    }
                    let now = Instant::now();
                    let due = watched.get(&root).is_some_and(|entry| {
                        entry.deadline.is_some_and(|deadline| now >= deadline)
                    });
                    if !due {
                        continue;
                    }
                    if let Some(entry) = watched.get_mut(&root) {
                        entry.deadline = None;
                        pending_frame.append(&mut entry.pending);
                    }
                    if pending_frame.is_empty() {
                        continue;
                    }
                    let frame = holt_proto::WorkspaceWatchFrame {
                        paths: std::mem::take(&mut pending_frame),
                    };
                    // A failed send means the receiver dropped: end the
                    // watch with it.
                    if let Ok(value) = serde_json::to_value(&frame)
                        && out_tx.try_send(value).is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    Ok(out_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    /// End-to-end within the crate: writes after the watcher arms arrive
    /// as coalesced frames.
    #[tokio::test]
    async fn writes_arrive_as_one_debounced_frame() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().to_path_buf();
        // The RPC layer canonicalizes before subscribing — same spelling.
        let root = raw.canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), b"1").unwrap();
        let mut stream = subscribe(root.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        // One burst; the frame(s) that follow must mention the written file.
        std::fs::write(root.join("a.txt"), b"2").unwrap();
        std::fs::write(root.join("b.txt"), b"new").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut saw_a = false;
        let mut saw_b = false;
        while !(saw_a && saw_b) {
            let frame = tokio::time::timeout_at(deadline, stream.next())
                .await
                .expect("frame within deadline")
                .expect("stream stays open");
            saw_a |= frame.to_string().contains("a.txt");
            saw_b |= frame.to_string().contains("b.txt");
        }
    }
}
