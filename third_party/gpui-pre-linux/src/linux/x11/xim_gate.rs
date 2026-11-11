//! One-key-at-a-time gate for X11 XIM.
//!
//! The XIM client used to forward every key in a batch, then apply replies
//! whenever they arrived. Under UI lag the input method answers a later
//! Enter or the next field's keys before earlier characters, so text is
//! dropped, reordered, or submitted truncated (Holt issue #85).
//!
//! This gate holds the next key until the forwarded one has produced a
//! callback. A stalled reply is delivered locally and its late callback is
//! ignored, so a dead input method cannot swallow the rest of the burst.

use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) struct XimKeyQueue<T> {
    inflight: Option<T>,
    deferred: VecDeque<T>,
    /// Late callbacks still owed by keys we already delivered locally.
    suppress: usize,
}

impl<T> Default for XimKeyQueue<T> {
    fn default() -> Self {
        Self {
            inflight: None,
            deferred: VecDeque::new(),
            suppress: 0,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyArrival<T> {
    /// Send this key to the input method and wait for its callback.
    Forward(T),
    /// A key is already in flight. Keep this one until that callback.
    Deferred,
    /// The input method's previous replies are being ignored. Insert locally.
    Local(T),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallbackPlan<T> {
    /// Apply the callback. Nothing is waiting behind it.
    Applied,
    /// Apply the callback, then forward this previously deferred key.
    AppliedThenForward(T),
    /// The callback belongs to a key already delivered by [`XimKeyQueue::on_timeout`].
    Ignore,
}

impl<T: Clone> XimKeyQueue<T> {
    pub(crate) fn has_inflight(&self) -> bool {
        self.inflight.is_some()
    }

    pub(crate) fn needs_timeout(&self) -> bool {
        self.inflight.is_some() || self.suppress > 0
    }

    /// Queue `key`. `Deferred` means the caller keeps no copy; the queue owns it.
    pub(crate) fn push(&mut self, key: T) -> KeyArrival<T> {
        if self.suppress > 0 {
            return KeyArrival::Local(key);
        }
        if self.inflight.is_some() {
            self.deferred.push_back(key);
            KeyArrival::Deferred
        } else {
            self.inflight = Some(key.clone());
            KeyArrival::Forward(key)
        }
    }

    /// A key arrived while one was already in flight, split out of the batch
    /// before that batch's replies are applied. See the client loop.
    pub(crate) fn defer(&mut self, key: T) {
        self.deferred.push_back(key);
    }

    pub(crate) fn on_callback(&mut self) -> CallbackPlan<T> {
        if self.suppress > 0 {
            self.suppress -= 1;
            return CallbackPlan::Ignore;
        }
        self.inflight = None;
        if let Some(next) = self.deferred.pop_front() {
            self.inflight = Some(next.clone());
            CallbackPlan::AppliedThenForward(next)
        } else {
            CallbackPlan::Applied
        }
    }

    /// The in-flight reply did not arrive. Deliver every held key locally and
    /// ignore that many late callbacks. A later timeout with nothing held
    /// clears a suppress count whose callbacks never came.
    pub(crate) fn on_timeout(&mut self) -> Vec<T> {
        if self.inflight.is_none() && self.deferred.is_empty() {
            self.suppress = 0;
            return Vec::new();
        }
        let mut local = Vec::new();
        if let Some(key) = self.inflight.take() {
            local.push(key);
        }
        local.extend(self.deferred.drain(..));
        self.suppress = local.len();
        local
    }

    /// The input method connection died. Hand back every held key and forget
    /// the suppress count; later keys go through the local path.
    pub(crate) fn reset(&mut self) -> Vec<T> {
        let mut local = Vec::new();
        if let Some(key) = self.inflight.take() {
            local.push(key);
        }
        local.extend(self.deferred.drain(..));
        self.suppress = 0;
        local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_forwarded_one_key_at_a_time() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert_eq!(queue.push(2), KeyArrival::Deferred);
        assert_eq!(queue.push(3), KeyArrival::Deferred);
        assert_eq!(queue.on_callback(), CallbackPlan::AppliedThenForward(2));
        assert!(queue.has_inflight());
        assert_eq!(queue.on_callback(), CallbackPlan::AppliedThenForward(3));
        assert_eq!(queue.on_callback(), CallbackPlan::Applied);
        assert!(!queue.has_inflight());
    }

    #[test]
    fn keys_arriving_before_a_reply_are_not_forwarded_twice() {
        // The socket batch is [key 2, reply to key 1]. The client defers key 2
        // before applying the reply, and the reply is what forwards it.
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        queue.defer(2);
        assert_eq!(queue.on_callback(), CallbackPlan::AppliedThenForward(2));
        assert_eq!(queue.on_callback(), CallbackPlan::Applied);
    }

    #[test]
    fn a_stalled_reply_is_delivered_locally_and_its_callback_ignored() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert_eq!(queue.push(2), KeyArrival::Deferred);
        assert_eq!(queue.on_timeout(), vec![1, 2]);
        assert_eq!(queue.on_callback(), CallbackPlan::Ignore);
        assert_eq!(queue.on_callback(), CallbackPlan::Ignore);
        assert_eq!(queue.push(3), KeyArrival::Forward(3));
        assert_eq!(queue.on_callback(), CallbackPlan::Applied);
    }

    #[test]
    fn suppress_does_not_last_after_the_callbacks_never_arrive() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        assert_eq!(queue.on_timeout(), vec![1]);
        assert!(queue.needs_timeout());
        assert!(queue.on_timeout().is_empty());
        assert!(!queue.needs_timeout());
        assert_eq!(queue.push(2), KeyArrival::Forward(2));
    }

    #[test]
    fn resetting_the_connection_returns_held_keys() {
        let mut queue = XimKeyQueue::default();
        assert_eq!(queue.push(1), KeyArrival::Forward(1));
        queue.defer(2);
        assert_eq!(queue.reset(), vec![1, 2]);
        assert_eq!(queue.push(3), KeyArrival::Forward(3));
    }
}
