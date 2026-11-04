//! The bounded output replay both terminal sessions and background
//! processes serve: monotonic seqs, byte-bounded retention, and an honest
//! `Gap` when a resuming subscriber asks for evicted output.

use std::collections::VecDeque;

use base64::Engine as _;
use holt_proto::TerminalEvent;

/// 1 MiB of retained output, or 4096 events, whichever evicts first.
const RETAINED_BYTES: usize = 1024 * 1024;
const RETAINED_EVENTS: usize = 4096;

fn event_seq(event: &TerminalEvent) -> u64 {
    match event {
        TerminalEvent::Data { seq, .. }
        | TerminalEvent::Exit { seq, .. }
        | TerminalEvent::Gap { seq } => *seq,
    }
}

#[derive(Default)]
pub(crate) struct OutputLog {
    seq: u64,
    bytes: usize,
    events: VecDeque<(TerminalEvent, usize)>,
}

impl OutputLog {
    /// Append one output chunk. Evicts oldest events until the retention
    /// bounds hold again.
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.seq += 1;
        self.bytes += data.len();
        self.events.push_back((
            TerminalEvent::Data {
                seq: self.seq,
                data: base64::engine::general_purpose::STANDARD.encode(data),
            },
            data.len(),
        ));
        while self.bytes > RETAINED_BYTES || self.events.len() > RETAINED_EVENTS {
            if let Some((_, len)) = self.events.pop_front() {
                self.bytes -= len;
            }
        }
    }

    /// Append the terminal `Exit` event. Nothing may follow it. Returns
    /// the exit event's seq.
    pub(crate) fn push_exit(&mut self, exit_code: i32) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        self.events.push_back((
            TerminalEvent::Exit {
                seq,
                exit_code,
                signal: None,
            },
            0,
        ));
        seq
    }

    /// The last assigned seq — what a watcher publishes to wake followers.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// The retained events, oldest first (tail reports walk this).
    pub(crate) fn events_iter(&self) -> impl DoubleEndedIterator<Item = &TerminalEvent> {
        self.events.iter().map(|(event, _)| event)
    }

    /// The next event after `after`, or the `Gap` announcing that `after`'s
    /// successor was evicted. `None` when everything retained is at or
    /// before `after`.
    pub(crate) fn next(&self, after: u64) -> Option<TerminalEvent> {
        let first = self.events.front().map(|(event, _)| event_seq(event))?;
        if after.saturating_add(1) < first {
            return Some(TerminalEvent::Gap { seq: first - 1 });
        }
        self.events
            .iter()
            .find(|(e, _)| event_seq(e) > after)
            .map(|(e, _)| e.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_is_explicit_and_storage_is_bounded() {
        let mut log = OutputLog::default();
        for _ in 0..200 {
            log.push(&[b'x'; 16384]);
        }
        assert!(log.bytes <= RETAINED_BYTES);
        let gap = log.next(0).unwrap();
        assert!(matches!(gap, TerminalEvent::Gap { .. }));
        assert!(matches!(
            log.next(event_seq(&gap)),
            Some(TerminalEvent::Data { .. })
        ));
    }

    #[test]
    fn a_full_replay_needs_no_gap_and_ends_on_exit() {
        let mut log = OutputLog::default();
        log.push(b"ready");
        log.push_exit(0);
        let first = log.next(0).unwrap();
        let seq = event_seq(&first);
        assert!(matches!(first, TerminalEvent::Data { .. }));
        assert!(matches!(log.next(seq), Some(TerminalEvent::Exit { .. })));
        assert_eq!(log.next(seq + 1), None);
    }
}
