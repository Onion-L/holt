//! One-key-at-a-time gate for X11 XIM.
//!
//! The XIM client used to forward every key in a batch, then apply replies
//! whenever they arrived. Under UI lag the input method answers a later
//! Enter or the next field's keys before earlier characters, so text is
//! dropped, reordered, or submitted truncated (Holt issue #85).
//!
//! A forwarded key is marked synchronous. Preedit, commit, and a forwarded
//! copy of the key are text for that key; they do not release the next one.
//! `XIM_SYNC_REPLY` does. A stalled reply is delivered locally and later
//! callbacks stay ignored until that reply's sync arrives, the input
//! context is replaced, or the connection drops. An empty timeout does not
//! clear that ignore count. A timeout that already showed a preedit keeps
//! later keys local until the context is reset, so the old composition
//! cannot join the next commit.

use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) struct XimKeyQueue<T> {
    inflight: Option<T>,
    deferred: VecDeque<T>,
    /// The in-flight key already inserted text (commit or a forwarded key).
    /// A timeout must not insert it a second time.
    content_applied: bool,
    /// The in-flight key drew a preedit. A timeout then owes an input-context
    /// reset before another key may be forwarded.
    preedit_seen: bool,
    /// Keys we already delivered locally whose sync reply has not arrived.
    /// Callbacks are ignored until each owed reply shows up. This is not
    /// cleared by a timeout that finds nothing held.
    suppress: usize,
    /// A preedit was abandoned locally. Keys stay local until [`XimKeyQueue::note_reset`].
    reset_owed: bool,
}

impl<T> Default for XimKeyQueue<T> {
    fn default() -> Self {
        Self {
            inflight: None,
            deferred: VecDeque::new(),
            content_applied: false,
            preedit_seen: false,
            suppress: 0,
            reset_owed: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyArrival<T> {
    /// Send this key to the input method and wait for its sync reply.
    Forward(T),
    /// A key is already in flight. Keep this one until that reply.
    Deferred,
    /// A previous reply is still being ignored. Insert locally.
    Local(T),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SyncPlan<T> {
    /// The reply is late, or nothing is in flight. Do not release a key.
    Ignore,
    /// The in-flight key is done. Events are in arrival order.
    Released(Vec<T>),
}

/// Where a released batch goes. Non-keys that preceded the next key are
/// dispatched first, so a click cannot land in front of the character it
/// followed.
pub(crate) struct AfterKey<T> {
    pub before: Vec<T>,
    pub forward: Option<T>,
    pub rest: Vec<T>,
}

pub(crate) fn split_at_first_key<T>(events: Vec<T>, is_key: impl Fn(&T) -> bool) -> AfterKey<T> {
    let mut before = Vec::new();
    let mut rest = events;
    while rest.first().is_some_and(|event| !is_key(event)) {
        before.push(rest.remove(0));
    }
    let forward = if rest.first().is_some_and(|event| is_key(event)) {
        Some(rest.remove(0))
    } else {
        None
    };
    AfterKey {
        before,
        forward,
        rest,
    }
}

impl<T: Clone> XimKeyQueue<T> {
    pub(crate) fn has_inflight(&self) -> bool {
        self.inflight.is_some()
    }

    /// Only an unanswered forwarded key needs the stall timer. The ignore
    /// count must not schedule another timeout: that timeout used to clear
    /// it and re-apply a late commit.
    pub(crate) fn needs_timeout(&self) -> bool {
        self.inflight.is_some()
    }

    /// Queue `key`. `Deferred` means the caller keeps no copy; the queue owns it.
    pub(crate) fn owes_reset(&self) -> bool {
        self.reset_owed
    }

    /// The input context no longer holds the composition this queue abandoned.
    pub(crate) fn note_reset(&mut self) {
        self.reset_owed = false;
    }

    pub(crate) fn push(&mut self, key: T) -> KeyArrival<T> {
        if self.suppress > 0 || self.reset_owed {
            return KeyArrival::Local(key);
        }
        if self.inflight.is_some() {
            self.deferred.push_back(key);
            KeyArrival::Deferred
        } else {
            self.inflight = Some(key.clone());
            self.content_applied = false;
            self.preedit_seen = false;
            KeyArrival::Forward(key)
        }
    }

    /// An event arrived while one key was in flight. Kept in order behind it.
    pub(crate) fn defer(&mut self, event: T) {
        self.deferred.push_back(event);
    }

    /// Text or a forwarded copy of the in-flight key. `inserts` is false for
    /// preedit, which does not put a committed character in the field.
    /// Returns whether the caller should apply it.
    pub(crate) fn on_content(&mut self, inserts: bool) -> bool {
        if self.suppress > 0 || self.inflight.is_none() {
            return false;
        }
        if inserts {
            self.content_applied = true;
        } else {
            self.preedit_seen = true;
        }
        true
    }

    /// The input method finished the in-flight key.
    pub(crate) fn on_sync(&mut self) -> SyncPlan<T> {
        if self.suppress > 0 {
            self.suppress -= 1;
            return SyncPlan::Ignore;
        }
        if self.inflight.is_none() {
            return SyncPlan::Ignore;
        }
        self.inflight = None;
        self.content_applied = false;
        self.preedit_seen = false;
        SyncPlan::Released(self.deferred.drain(..).collect())
    }

    /// Arm the next key after a sync reply and keep what arrived behind it.
    pub(crate) fn forward_next(&mut self, key: T, rest: Vec<T>) {
        self.inflight = Some(key);
        self.content_applied = false;
        self.preedit_seen = false;
        self.deferred.extend(rest);
    }

    /// The in-flight reply did not arrive. Deliver held input locally.
    /// A key whose text was already inserted is not delivered again.
    /// One owed sync reply stays ignored until it arrives; a later timeout
    /// with nothing held does not forget it.
    pub(crate) fn on_timeout(&mut self) -> Vec<T> {
        if self.inflight.is_none() && self.deferred.is_empty() {
            return Vec::new();
        }
        let mut local = Vec::new();
        if let Some(key) = self.inflight.take() {
            self.suppress += 1;
            if !self.content_applied {
                local.push(key);
            }
        }
        if self.preedit_seen {
            self.reset_owed = true;
        }
        self.content_applied = false;
        self.preedit_seen = false;
        local.extend(self.deferred.drain(..));
        local
    }

    /// The input method connection died. Hand back held input and forget the
    /// ignore count; later keys go through the local path until XIM returns.
    pub(crate) fn reset(&mut self) -> Vec<T> {
        let mut local = Vec::new();
        if let Some(key) = self.inflight.take() {
            if !self.content_applied {
                local.push(key);
            }
        }
        self.content_applied = false;
        self.preedit_seen = false;
        self.reset_owed = false;
        local.extend(self.deferred.drain(..));
        self.suppress = 0;
        local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Ev {
        Key(u8),
        Click(u8),
    }

    #[test]
    fn a_burst_waits_for_sync_before_the_next_key() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert_eq!(queue.push(2), KeyArrival::Deferred);
        assert_eq!(queue.push(3), KeyArrival::Deferred);
        assert!(queue.on_content(false));
        assert!(queue.has_inflight());
        assert_eq!(queue.on_sync(), SyncPlan::Released(vec![2, 3]));
        assert!(!queue.has_inflight());
        assert_eq!(queue.on_sync(), SyncPlan::Ignore);
    }

    #[test]
    fn content_with_nothing_in_flight_is_not_applied() {
        let mut queue = XimKeyQueue::<u8>::default();
        assert!(!queue.on_content(true));
        assert_eq!(queue.on_sync(), SyncPlan::Ignore);
    }

    #[test]
    fn a_click_queued_behind_a_key_stays_behind_it() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(Ev::Key(1)), KeyArrival::Forward(Ev::Key(1)));
        queue.defer(Ev::Click(9));
        queue.defer(Ev::Key(2));
        let SyncPlan::Released(events) = queue.on_sync() else {
            panic!("sync should release");
        };
        let split = split_at_first_key(events, |event| matches!(event, Ev::Key(_)));
        assert_eq!(split.before, vec![Ev::Click(9)]);
        assert_eq!(split.forward, Some(Ev::Key(2)));
        assert!(split.rest.is_empty());
    }

    #[test]
    fn a_stalled_reply_is_delivered_locally_and_a_late_commit_is_not() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert_eq!(queue.push(2), KeyArrival::Deferred);
        assert_eq!(queue.on_timeout(), vec![1, 2]);
        assert!(!queue.needs_timeout());
        assert!(queue.on_timeout().is_empty());
        assert!(!queue.on_content(true));
        assert_eq!(queue.push(3), KeyArrival::Local(3));
        assert_eq!(queue.on_sync(), SyncPlan::Ignore);
        assert_eq!(queue.push(4), KeyArrival::Forward(4));
        assert!(queue.on_content(true));
        assert_eq!(queue.on_sync(), SyncPlan::Released(vec![]));
    }

    #[test]
    fn timeout_does_not_reinsert_a_key_whose_text_arrived() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        queue.defer(2);
        assert!(queue.on_content(true));
        assert_eq!(queue.on_timeout(), vec![2]);
        assert!(!queue.on_content(true));
    }

    #[test]
    fn a_preedit_timeout_stays_local_until_the_context_is_reset() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert!(queue.on_content(false));
        assert_eq!(queue.on_timeout(), vec![1]);
        assert!(queue.owes_reset());
        // The late sync retires the key. It does not clear the composition.
        assert_eq!(queue.on_sync(), SyncPlan::Ignore);
        assert_eq!(queue.push(2), KeyArrival::Local(2));
        queue.note_reset();
        assert_eq!(queue.push(3), KeyArrival::Forward(3));
    }

    #[test]
    fn replacing_the_input_context_clears_an_owed_sync_and_reset() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert!(queue.on_content(false));
        assert_eq!(queue.on_timeout(), vec![1]);
        assert_eq!(queue.push(2), KeyArrival::Local(2));
        // The old sync will never be accepted: the context id changed.
        assert!(queue.reset().is_empty());
        assert!(!queue.owes_reset());
        assert_eq!(queue.push(3), KeyArrival::Forward(3));
    }

    #[test]
    fn resetting_the_connection_returns_held_keys_and_clears_suppress() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        queue.defer(2);
        assert_eq!(queue.on_timeout(), vec![1, 2]);
        assert_eq!(queue.push(3), KeyArrival::Local(3));
        assert!(queue.reset().is_empty());
        assert_eq!(queue.push(4), KeyArrival::Forward(4));
    }
}
