//! Live Turn change-set delivery (ADR-0024, ticket 01): the
//! `WatchTurnChangeSet` subscription.
//!
//! One `notify` watcher per chat working directory — the working tree and
//! its `.git`, so staging and branch moves count too — with the git watch's
//! 200 ms quiet window; a root the watcher cannot cover degrades to 2 s
//! polling. A frame fires only when the change set actually moved, so the
//! UI's fold stays idempotent. The stream stays open across Turns — the
//! `final` phase is a frame, not the end — and ends when the UI drops it.
//!
//! The Turn terminal event drives the final frame, read by the event's
//! message id: an auto-advanced next Turn can begin before this task runs,
//! and the settled Turn's frozen result must survive that.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use holt_proto::{TurnChangeSet, TurnChangeSetPhase, TurnChangeSetReply, TurnFileChange};
use notify::Watcher as _;
use tokio::sync::broadcast;

use crate::git::Git;
use crate::git_watch::Debounce;
use crate::turn_changes::{PendingFinals, TurnChanges};
use crate::turn_events::TurnEvents;

/// Poll cadence for roots without a working fs watcher.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What a frame changed by: two captures are the same frame when the phase,
/// files, message identity, and truncation agree.
type FrameKey = (String, TurnChangeSetPhase, Vec<TurnFileChange>, bool);

fn frame_key(change_set: &TurnChangeSet) -> FrameKey {
    (
        change_set.message_id.clone(),
        change_set.phase,
        change_set.files.clone(),
        change_set.truncated,
    )
}

/// A subscription over one chat: the current change set, then a fresh frame
/// each time it moves. The task ends when the receiver drops or the watcher
/// dies.
pub(crate) fn subscribe(
    root: PathBuf,
    chat_id: String,
    git: Git,
    device_id: String,
    changes: Arc<TurnChanges>,
    turn_events: TurnEvents,
) -> Result<mpsc::Receiver<serde_json::Value>, String> {
    let (mut out_tx, out_rx) = mpsc::channel::<serde_json::Value>(16);
    let mut events = turn_events.subscribe();
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
                    tracing::warn!(%error, root = %root.display(), "turn change watch unavailable");
                    Some(Instant::now() + POLL_INTERVAL)
                }
            },
            None => Some(Instant::now() + POLL_INTERVAL),
        };
        let mut debounce = Debounce::new();
        let mut pending_finals = PendingFinals::default();
        let mut last: Option<FrameKey> = None;
        let mut last_message = None;

        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                    debounce.event(Instant::now());
                }
                event = events.recv() => match event {
                    Ok(event) if event.chat_id == chat_id => {
                        // The settled Turn's final change set is already
                        // stored before the event is published; reading it
                        // by message id survives the next Turn's admission.
                        pending_finals.push(event.message_id);
                        debounce.event(Instant::now());
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        debounce.event(Instant::now());
                    }
                    Err(broadcast::error::RecvError::Closed) => {}
                },
                _ = ticker.tick() => {
                    // The subscription is gone even on a quiet tree — end
                    // the watch instead of ticking forever.
                    if out_tx.is_closed() {
                        break;
                    }
                    let now = Instant::now();
                    if let Some(message_id) = pending_finals.pop() {
                        // The final frame outranks the current Turn's live
                        // one for this tick.
                        if let Ok(Some(change_set)) = changes
                            .read_message(&git, &device_id, &chat_id, &message_id)
                            .await
                            && emit(&mut out_tx, &mut last, change_set).is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    // A new Turn's identity is a change by itself, and the
                    // opening frame must not wait for filesystem activity.
                    let message = changes.current_message(&chat_id);
                    let turned = message != last_message;
                    let quiet = debounce.tick(now);
                    // `last` is None only before the opening frame: fire on
                    // the first tick so an already-settled Turn shows at once.
                    let due = last.is_none()
                        || turned
                        || quiet
                        || poll_due.is_some_and(|due| now >= due);
                    if !due {
                        continue;
                    }
                    last_message = message;
                    if poll_due.is_some() {
                        poll_due = Some(now + POLL_INTERVAL);
                    }
                    let Ok(Some(change_set)) = changes.read(&git, &device_id, &chat_id).await
                    else {
                        continue;
                    };
                    if emit(&mut out_tx, &mut last, change_set).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(out_rx)
}

/// Send `change_set` unless it repeats the last frame. `Err` means the
/// subscriber is gone, not a malformed value.
fn emit(
    out: &mut mpsc::Sender<serde_json::Value>,
    last: &mut Option<FrameKey>,
    change_set: TurnChangeSet,
) -> Result<(), ()> {
    let key = frame_key(&change_set);
    if last.as_ref() == Some(&key) {
        return Ok(());
    }
    *last = Some(key);
    let value = serde_json::to_value(TurnChangeSetReply::Captured(change_set)).map_err(|_| ())?;
    out.try_send(value).map_err(|_| ())
}
