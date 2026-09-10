---
status: accepted
---

# An attended send runs through a paused queue

A failed Turn pauses the chat's message queue, and that pause used to be
absolute: every later submission parked behind it. Run now deliberately
re-pauses after running its selection, and the one call that fully resumes
the queue had no UI surface — so a single transient failure left a chat
needing one manual click per message, indefinitely.

A message the user submits while the chat's execution channel is settled —
no Turn or manual Compaction is executing or being prepared — is admitted
immediately even when the queue is paused (an **attended send**). The
admission grant is durable, bound to the new message's identity, and
single-run: it orders that message ahead of parked and priority items,
runs it alone, and is consumed at admission. Accepting the message and
granting the admission are one persisted change, granted only on first
acceptance — a redelivered duplicate never re-authorizes a run. Continue
during the attended run lifts the solo scope and resumes the whole queue;
a later failure pauses again. Stop and restart pauses follow the same
exception, covering only the user's next submission, never the parked
backlog. When a queue settles with no pending items, nothing executing,
and no queue-level error, the pause clears — on restore too, so a stale
flag in a saved queue file cannot resurrect.

Scope: the rule lives in the engine's queue, not the composer — every
client path benefits and there is no second send path. Manual Compaction
keeps its strict submission order (ADR-0011): an attended /compact parks
like any queued item, and Continue is its way forward. An unreadable queue
file is never bypassed — the admission checkpoint itself is unusable.
Failures of any kind still pause remaining work; ADR-0015's "interrupted
Turns are never automatically requeued" stands. Error classification and
automatic retries remain out of scope.

The design was settled in a grilling session on 2026-09-10; it narrows the
glossary's Paused queue entry, whose recorded exception is the attended
send.
