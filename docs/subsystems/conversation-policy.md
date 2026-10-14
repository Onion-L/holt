# Conversation policy

The user-facing rules of a chat's queue and its side tasks: how work is admitted (FIFO with priority and attended-send exceptions), what steer and edit-last do, when compaction runs, the one-shot title task, and the agent's question card. These are the policies a user feels; the loop's streaming and retry mechanics they ride belong to run-loop, the durable records to persistence.

Not here: the agent loop and Turn execution pipeline (see run-loop); the permission gate that pauses a Turn mid-run (see permission-gate); goals and routines, which enqueue through these same rules (see goals-routines).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Queue semantics | One FIFO consumer and one execution lock per chat, held through preparation, execution, History repair, transcript settlement, and queue completion. Execution failures pause remaining work; admission failures (missing credentials, an unresolved skill) retain the head with an error and create no Turn; an interrupt cancels and pauses, and the channel stays occupied until cleanup completes | [`crates/engine/src/queue.rs`](../../crates/engine/src/queue.rs) |
| Admission checkpoint | Pending→started is atomically replaced and synced before any model or tool work; mutations serialize with admission on the queue lock, so an edit landing between the consumer's pick and the checkpoint wins and a delete simply removes the item. After the checkpoint the item is execution's property — mutations fail with an already-executing error and the run proceeds exactly once | `queue.rs` |
| Steer | The explicit priority action: with a message id it promotes that pending item; without, it enqueues the new prompt ahead of parked work; either way it cancels the running Turn, waits for cleanup, unpauses, and starts. Pending steer requests keep their submission order; Run-now on a paused queue authorizes only the selected item | [`crates/engine/src/rpc/run.rs`](../../crates/engine/src/rpc/run.rs) |
| Attended send | The one exception to pause: a first acceptance arriving while the channel is settled and the queue paused is admitted at once through a one-shot, persisted grant bound to its message id — it runs alone ahead of parked work, and the queue returns to its pause when it settles. Continue lifts the single-run scope; deleting the last pending item (nothing executing, no queue error) clears the pause | `queue.rs` (`attended_grant`) |
| Edit-last | Applies only to the latest user transcript entry: cancels the active Turn, prunes that Turn's later transcript and History, preserves the message identity, and queues a replacement ahead of remaining work using the chat's current settings. Existing workspace side effects and usage remain; a stale message id is rejected | `rpc/run.rs` (`edit_last_message`) |
| Compaction | Shrinks the History into a model-written summary plus a verbatim recent tail. Three triggers: automatically before a Turn and between tool rounds, manually via `/compact` (a typed queue item sharing the channel — never a Turn), and on context overflow (compact and continue the same Turn, once; a second overflow flags the chat and the next Turn compacts unconditionally first). Upstream defaults: a context reserve, a recent-tokens tail, an estimator anchored on the last real usage, and a cut point that never separates a tool call from its result | [`crates/engine/src/compaction.rs`](../../crates/engine/src/compaction.rs) |
| Title task | One independent background request on a chat's first accepted prompt — outside the Turn lifecycle: its own token (only chat deletion cancels), no Session status, no History/transcript footprint, no retries, billed immediately as `title`. Every failure is silent and keeps the first-line fallback; the prompt frames the first message as material to name, never a message to answer | [`crates/engine/src/title_task.rs`](../../crates/engine/src/title_task.rs) |
| Question card | `ask_user` never waits: it lands a `QuestionCard` part and returns, and the model stops its Turn. `SettleQuestion` stamps the card under the transcript lock and enqueues each question restated with its answer as an ordinary user message (a failed enqueue rolls the stamp back); `DismissQuestion` stamps it superseded, persistently — a restart does not resurrect it. Any new Turn retires still-pending cards; Plan and Provider Turns mount no `ask_user` | [`crates/engine/src/tools/ask_user.rs`](../../crates/engine/src/tools/ask_user.rs) |
| UI cutaways | The composer's queue rows (edit/delete/run-now per kind), the approval bar's question pages with their `1/N` pager, the compacted-transcript dividers | [`crates/ui/src/composer/queue.rs`](../../crates/ui/src/composer/queue.rs), [`approval_bar.rs`](../../crates/ui/src/composer/approval_bar.rs) |

## Runtime path

1. **Accept.** A submission lands in the durable queue (model and reasoning captured at submission) and the watch frame carries the snapshot.
2. **Admit.** The consumer picks the head, writes the checkpoint, snapshots permission mode and live checkout identity, and refreshes the Turn's diff baseline — then the run loop owns the Turn.
3. **Settle.** History repair, transcript settlement, the usage batch, queue completion, then the terminal event — in that order, each durable before the next.
4. **Restart.** A started Turn repairs as interrupted (never requeued); a started manual Compaction settles with no record; unstarted work restores paused, with the attended-send grant intact.

## Constraints, state, and failure behavior

- **The queue is durable truth.** Accepted identities dedupe delivery retries; an unreadable queue is retained and blocked, never overwritten; a failed conversation write pauses consumption until repair and reopen.
- **Pause is the user's hand on the wheel**, with exactly two self-clearing paths: the attended-send grant and deleting the last pending item. A queue that settles empty with no queue-level error comes back clean on reload.
- **Compaction never loses context silently**: dividers and notices mark what happened; a compaction summary newer than a goal's stated-objective stamp forces the next continuation to restate it.
- **The transcript never shrinks** except edit-last's forward prune; History and transcript share the chat's persistence lock.
- **A question card is restart-safe by construction** — nothing waits on an RPC; the answer is just the next user message.
- Nothing to compact completes the `/compact` command with a transcript notice and advances the queue without a model request.

## Verification

```sh
cargo test -p holt-engine queue
cargo test -p holt-engine compaction
cargo test -p holt-engine title
cargo test -p holt-engine ask_user
cargo test -p holt-engine --test all queue
```

The suites cover admission checkpointing and the edit/delete race window, steer promotion and priority enqueue, the attended-send grant's one-shot scope across a simulated restart, edit-last pruning and identity preservation, compaction triggers (including the overflow retry-once path), the title task's silent failures and answer-rejection, question-card settling and rollback, and restart repair of every started shape.
