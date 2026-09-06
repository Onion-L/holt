---
status: accepted
---

# Steer interrupts the current Turn and starts a new Turn

Steer gives a user message priority by interrupting the current Turn,
waiting for its cleanup, then starting a new Turn before the remaining
queued work. This chooses explicit interruption over upstream pi-core-rs
steering, which injects messages at loop boundaries: the new instruction
can stop ongoing work, while Holt keeps its existing per-Turn Transcript,
History repair, and Turn diff semantics. The old Turn retains its
interrupted record and is never automatically requeued; the pending items
keep their relative order behind the new Turn.

Multiple Steer requests received while the old work is ending keep their
submission order ahead of ordinary pending items. Run now on a paused queue
executes the selected message without resuming unrelated pending work.

The feature design was confirmed on 2026-09-06. Ordinary-message Steer and
Run now shipped in ticket 03; ticket 04 extended the same promotion to
queued skill invocations. See
[the spec](../../.scratch/message-queue-and-steer/spec.md).
