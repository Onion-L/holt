# ADR-0032: The transcript is an append-only log; each chat persists behind its own lock

- Status: Accepted
- Date: 2026-09-12
- Scope: `crates/engine` persistence (`store.rs`, `agent.rs`, `rpc.rs`)

## Context

Through ADR-0031 the transcript persisted as a whole-file snapshot:
`publish()` deep-copied the entire `Vec<SessionMessageEntry>`, serialized it
pretty, and rewrote `transcripts/<chatId>.json` (tmp + rename) — on a 120 ms
tick while a run streamed, unthrottled on message/tool boundaries, and on
every later chip settle. One publish cost O(session); a session accumulated
O(N²) writes (≈40 MB/s sustained on a 5 MB transcript). Every such write
held the runtime-wide `persistence` std Mutex across synchronous disk IO, so
one streaming chat serialized every other chat's `start_turn`, history
appends, and queue settlement — the lock protected files those chats never
touch.

The History had already solved the same problem (ADR-0010): an append-only
JSONL file with a version header, one record per completed message, and a
tolerant reader that treats a truncated trailing line as absent.

## Decision

1. **The transcript log is append-only JSONL** (`transcripts/<chatId>.jsonl`):
   a version header, then one entry per line. An entry line is a full
   `SessionMessageEntry`; on replay a line whose id already exists replaces
   that earlier line (an upsert), otherwise it appends.

2. **Entries land at completion boundaries, never per stream tick.** A run's
   entry appends when: the admitted user entry is built, each assistant
   message ends, each tool call resolves, a gate stamps (an approval pause
   can outlive any tick — the pending chip must be durable), the run
   settles terminally, and when a post-run settle mutates an already-logged
   entry (plan cards, unresolved-tool sweeps, a subagent's first live-child
   stamp). Stream deltas update the watch only. This mirrors ADR-0010's
   "a crash mid-Turn loses nothing that had completed": the loss window is
   the in-flight message, exactly what the old 120 ms tick also failed to
   capture.

3. **`publish()` is pure broadcast.** Disk writes go through the explicit
   `ChatRuntime::persist_entry(entry_id)`; a raw publish can no longer
   accidentally become a whole-file rewrite.

4. **Each `ChatRuntime` owns its persistence lock.** Transcript and History
   appends of one chat serialize on that chat's lock; chats persist disjoint
   files, so nothing serializes chats against each other. `remove_chat`
   spans the removed flag and the record deletes with the chat's own lock so
   a racing append cannot resurrect a deleted file. The runtime keeps a
   separate `chats_store` lock for `chats.json` writes (`persist_chats_locked`),
   and `start_turn` no longer holds any lock across admission — the registry
   write takes `chats_store`, the user entry takes the chat's lock, and the
   per-chat execution mutex already orders that chat's runs.

5. **Legacy snapshots replay as the base.** A pre-0032
   `transcripts/<chatId>.json` is read first and the log applies on top of
   it (the same upsert rule), so migration is implicit and lazy; the legacy
   file is never written again and dies with the chat.

## Consequences

- A streaming run's disk traffic drops from O(session) per 120 ms to one
  entry line per completed unit — bounded by the current turn's size, not
  the session's.
- Concurrent chats (parallel subagents, a second chat while one streams) no
  longer contend on any shared persistence lock.
- Crash semantics: the replayed transcript keeps every completed round of an
  interrupted run, with the run entry settling as aborted on load; an
  in-flight partial message is lost (as before, modulo the tick window).
- A corrupted transcript line (beyond a truncated tail) is skipped on
  replay rather than quarantined: the transcript is a display record, never
  a correctness boundary like the History.
- The UI watch's whole-snapshot broadcast (`send_replace` of a full clone)
  is unchanged — consumer-side rebuild costs are a separate concern.
