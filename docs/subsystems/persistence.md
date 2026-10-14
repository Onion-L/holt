# Persistence

How a conversation durably exists on disk: the transcript log (the UI's replay source), the History record (the model's memory), the queue file, and the usage ledger — all append-only JSONL or atomic-replace JSON under the data dir, all tolerant readers. The engine owns every read and write; the UI never touches these files.

Not here: what a Turn does between writes (see run-loop), when compaction runs (see conversation-policy — this page owns what a compaction leaves behind), and the registry files (see shell-registry).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Transcript log | `transcripts/<chatId>.jsonl`: a version header, then self-describing records. A full entry line is a first landing, a settle, or a post-run re-append — a later full line with the same id replaces its earlier one; a `parts` line appends an entry's new tail parts; a `part` line backfills one resolved tool call in place. The full line is the replay's self-heal anchor: incremental lines that no longer fit are dropped. A pre-log legacy snapshot is the replay base for old chats and is never written again | [`crates/engine/src/store.rs`](../../crates/engine/src/store.rs) |
| Event decoding | pi-core's tool calls and assistant messages decode into transcript parts; a completed result stamps onto its chip wherever it sits in the log | [`crates/engine/src/decode.rs`](../../crates/engine/src/decode.rs) |
| History record | `history/<chatId>.jsonl`: the model-facing message sequence as its own append-only log — a version header, then one entry per line in the upstream serde shape (the file is Holt-owned, so entry kinds can grow without binding to upstream session codecs). Reads are tolerant: unknown entry kinds are skipped, and a truncated trailing line — the crash-mid-append shape — is treated as absent | [`crates/engine/src/history.rs`](../../crates/engine/src/history.rs) |
| History repair | Interrupted Turns keep an honest interrupted record with synthetic error tool results; errored Turns keep the prompt and drop the failed answer. A damaged file is quarantined `.corrupt` (kept, never overwritten) with a one-time transcript notice — repair never blocks the chat | `history.rs`, `agent.rs` |
| Queue file | `queues/<chatId>.json`: accepted items rewritten whole with an fsync on every mutation, including the admission checkpoint that precedes any model or tool work | [`crates/engine/src/queue.rs`](../../crates/engine/src/queue.rs) |
| Usage ledger | `usage/<chatId>.jsonl`: one record per metered provider round-trip — every token field, the attribution kind (Turn, Subagent, Compaction, Auto-review, Title), provider, model, and the upstream cost stored verbatim (Holt computes no prices). Same durability shapes as the History record: version header, repaired-append atomicity, a tolerant reader that skips a crash-truncated trailing line. A damaged file is quarantined and totals restart from zero — bookkeeping never blocks the chat | [`crates/engine/src/usage.rs`](../../crates/engine/src/usage.rs) |
| Write choreography | A Turn's records accumulate in memory and land as one batch append at settlement, after queue completion; calls outside the Turn model (the Title task, manual Compaction) append immediately; subagent round-trips — their internal Compaction and auto-review calls included, one `subagent` kind — book into the parent chat's batch, and no child ledger file exists. Writes are fire-and-forget: a failed append costs only the record, and a crash before settlement loses the running Turn's batch, unrepaired, by design | `usage.rs` |
| Archive and aggregate | Deleting a chat appends its whole ledger segment to the grow-only `usage/archive.jsonl` (best-effort — a failed archive never blocks the delete). The device-level aggregate merges live ledgers and the archive into one unary reply: range metrics, per-(provider, model) series, by-model and by-project breakdowns, a fixed heatmap — strictly read-only and best-effort, a damaged ledger is skipped | `usage.rs`, [`usage_stats.rs`](../../crates/engine/src/usage_stats.rs) |
| Compaction residue | A compaction summary lands in the History as its own message kind; the transcript keeps dividers with before/after token counts and the trigger — the transcript never shrinks | `history.rs`, `decode.rs` |
| UI cutaways | Transcript replay renders the collapsed log; the usage widget and usage overview read the ledger and aggregate | [`crates/ui/src/transcript/model.rs`](../../crates/ui/src/transcript/model.rs), [`chat_usage.rs`](../../crates/ui/src/chat_usage.rs) |

## Runtime path

1. **During a run**, streaming completions land incremental lines under the chat's own persistence lock (transcript and History share it; one chat's writes never serialize against another's); stream deltas never touch disk.
2. **At settle**, unresolved tool calls settle, the Turn's History repairs per its outcome, the usage batch appends once, queue completion persists, and only then does the terminal event publish.
3. **On open**, the log replays into one entry per id (full lines replacing, incremental lines folding), the History replays into the model's memory, and usage sums warm from the ledger.
4. **On delete**, the ledger archives first, then every per-chat file and directory goes.

## Constraints, state, and failure behavior

- **Append-only where it can be, atomic where it cannot**: whole-file replaces (queue, registry) go tmp + rename + fsync; JSONL appends are crash-safe by tolerant replay, not by rewrite.
- **Every damaged file degrades, never blocks**: `.corrupt` quarantine for History and ledger; a retained-and-blocked unreadable queue; a transcript that opens behind whatever lines parse.
- **A record books when the provider reported** — an aborted or errored round-trip still books what its newest partial carried.
- **Chat ids are path-safety-checked** before any file path is built; an unsafe id loads empty and writes nothing.
- **One lock per chat**: the transcript and History share the chat's persistence lock, so their settlement order is the write order on disk.

## Verification

```sh
cargo test -p holt-engine store
cargo test -p holt-engine history
cargo test -p holt-engine usage
cargo test -p holt-engine --test all transcript
```

The suites cover log replay and self-heal (full-line replacement, incremental folding, legacy snapshot base), the crash-truncated trailing line, quarantine and the one-time notice, repair per outcome (interrupted synthetic results, errored prompt-kept), batch choreography and immediate records, the fire-and-forget write, archive-on-delete, and the aggregate's skip-damaged behavior.
