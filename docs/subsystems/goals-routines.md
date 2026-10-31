# Goals and routines

The two loops that keep a chat working when nobody presses send. A **goal** is a chat-scoped objective the queue driver drives: after a settled Turn, one verifier pass judges the objective from evidence and enqueues the next round as an ordinary, visible queue item. A **routine** is a user- or agent-defined schedule (cron or one-shot) that creates its own chat and queues a prompt through the normal run path.

Not here: the queue mechanics both ride (see conversation-policy — a goal continuation is just a flagged queue item); the modes' mutual exclusion (see modes — rejection, both ways); the usage ledger the verifier books into (`goal-check`, recorded immediately).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Goal state | The chat row's `goal`: objective (capped at 4,000 chars), status `active`/`paused`/`blocked`, iteration / no-progress / eval-failure counters, and the verifier-given `name`/`steps` the card renders. `SetGoal` / `ClearGoal` / `SetGoalPaused` are the whole surface; state rides `WatchChats` | [`crates/engine/src/goal.rs`](../../crates/engine/src/goal.rs) |
| The verifier | One model pass after a settled Turn's card frame is on its way — after `clear_final_signal`, inside the same driver iteration, so a continuation it enqueues can never race the next admission. Judged from evidence only: the History tail's tool calls and results (byte-capped) plus the frozen change-set summary. Answers one line — `COMPLETE: <evidence>` / `CONTINUE: <missing>` / `BLOCKED: <reason>` — plus the card-facing `NAME:`/`STEPS:` block; a bare `COMPLETE` without evidence is no verdict at all | `goal.rs` |
| Backstops | `MAX_ITERATIONS` 20 is the primary anti-spin cap; no-progress (a Succeeded Turn with no tool call and no change set) pauses at 3; consecutive verifier failures pause at 3; one pass is wall-clock capped at 120 s — the HTTP layer has no total timeout and the pass runs inline in the driver iteration, so without it a hung stream would stall the chat's queue | `goal.rs` (the caps are named constants) |
| Continuations | A `CONTINUE` verdict enqueues an ordinary, visible queue item flagged `goalContinuation` (the round rides as data; the transcript draws a loop row from it, never from prompt text). Deleting the item pauses the goal; `ClearGoal` and pausing sweep every flagged row; a continuation landing in a paused queue flips the goal to paused — the queue's pause wins | `goal.rs`, [`crates/engine/src/queue.rs`](../../crates/engine/src/queue.rs) |
| Skip and failure rules | Evaluation is skipped while user-queued items pend or a question card is unanswered; an interrupted or failed Turn pauses the goal; an admission failure (a gone model) clears it — retrying cannot fix a missing model; a continuation that cannot be enqueued at all pauses with a Notice | `goal.rs` |
| Restart reconciliation | At boot, over every chat: an active goal whose queue holds no goal-flagged item and no started Turn that will resume flips to paused. The durable invariant is that continuations are born only at a Turn settle, never revived; a resumed Turn settles again and feeds the verifier | `goal.rs` |
| Verifier settings | `goal-settings.json` (the settings-record pattern): a configured verifier model rides at the pinned lowest reasoning level — ideally cross-provider, so the judge does not share the worker's blind spots — with a silent fallback to the chat's own model. Saving rejects unresolvable models; missing credentials are a warning, never an error | [`crates/engine/src/goal_settings.rs`](../../crates/engine/src/goal_settings.rs) |
| Routine store | `routines.json`: every Routine with its newest 200 run records, written atomically, persisted before the in-memory list moves; a malformed file fails startup loudly. `Routines::create` shares validation between RPC and agent creation, and deterministic agent ids return the first committed Routine on retry. Records still running or waiting at launch become interrupted | [`crates/engine/src/routines.rs`](../../crates/engine/src/routines.rs) |
| Agent creation | `schedule_routine` mounts on main-chat Turns only. The agent supplies a self-contained prompt and exactly one of `delayMinutes`, `at`, or `cron`; the current Chat supplies the model, permission mode, and Space. Relative delays use the engine clock and persist in UTC, creation holds the registry lock against Space deletion, and the structured `ScheduleRoutine` chip preserves the approval subject. The tool description states the static device zone and points at the `current_time` tool for the clock (an embedded time would re-key the provider's prefix cache every Turn), so `at`/`cron` requests resolve dates and hours in the user's zone | [`crates/engine/src/tools/schedule_routine.rs`](../../crates/engine/src/tools/schedule_routine.rs) |
| Scheduler | One long-lived task sleeping until the earliest next fire among active routines, re-planning on every list change. Fires missed while Holt was quit or the device slept are found at startup or on a wall-clock jump and coalesce into one Catch-up run whose `missedFires` counts them; pausing moves the anchor to now, so missed fires are never made up. An edit that changes the schedule moves the anchor too — an edit never triggers a Catch-up | [`crates/engine/src/scheduler.rs`](../../crates/engine/src/scheduler.rs) |
| Runs | A fire creates its chat engine-side (the routine's name as a user-owned title, its `ChatConfig` — permission mode included, bypassing the sticky default — the `routine_run` marker, the worktree intent) and queues the prompt verbatim through the normal run path. A fire whose model is gone from the catalog or whose provider has no key records a failed run with the reason and no chat. While a run is running or waiting, further fires record as skipped; the gate moves a run waiting on an Approval and back, and a Turn that succeeds with an unanswered question card settles it waiting | [`crates/engine/src/rpc/routines.rs`](../../crates/engine/src/rpc/routines.rs) |
| UI cutaways | The routines page (create/edit/pause/run-now, schedule preview); the top-right chat-state card's goal form (verifier-given name, the steps as a compact verified indicator, live loop phase, the Turn's todo list as an embedded section — the card's one checklist, pause/clear); the composer's `/goal` directive chip | [`crates/ui/src/scheduled.rs`](../../crates/ui/src/scheduled.rs), [`shell/state_card.rs`](../../crates/ui/src/shell/state_card.rs) |

## Runtime path

1. **Arm.** `/goal <objective>` arms the goal and sends the objective as the first Turn's message (`SetGoal` rides after chat creation, before the queue — the same shape as a plan enter); a card offers pause/clear on an armed chat.
2. **Settle.** After the settled Turn's frame is on its way, the verifier reads the goal, the evidence tail, and the frozen change set, and answers one line.
3. **Verdict.** Complete clears the goal; blocked parks it; continue enqueues the visible continuation. The objective is stated in the History once — the first continuation spells it out, later ones point back, and a compaction newer than the objective's stamp forces a restate. A resume never replays the pre-pause verdict.
4. **Agent create.** `schedule_routine` validates one schedule shape, inherits the current Chat's execution config, persists through `Routines::create`, and returns the real `nextFireAt`. The write is gated like other mutations; a retry of the same tool call returns the original Routine rather than duplicating it.
5. **Routine fire.** The scheduler wakes, records the fire, creates the run chat, and queues the prompt; the run record's outcome follows its first Turn, settled where the terminal event publishes.
6. **Mutual exclusion.** `SetGoal` rejects a planning, provider-mode, or routine-run chat; entering either mode rejects a chat with goal state — nothing is silently cleared.

## Constraints, state, and failure behavior

- **The loop is visible or it does not run.** A continuation is a queue row the user can inspect, edit, or delete; an `active` goal with nothing queued can never happen (an un-enqueueable continuation pauses the goal with a Notice).
- **The verifier never destroys on its own say-so.** Clearing the objective — the one destructive transition — requires cited evidence; everything else pauses and waits for the user.
- **Verifier usage books immediately** under `goal-check`, never into the Turn's settled batch — which settles before the verifier runs.
- **The queue's pause always wins** over the goal's loop.
- **Removing a space pauses its routines** (reason `space-removed`, no run recorded); only an `UpdateRoutine` onto an existing space resumes them; deleting a run chat clears the record's `chatId` ("chat deleted").

## Verification

```sh
cargo test -p holt-engine goal
cargo test -p holt-engine routines
cargo test -p holt-engine schedule_routine
cargo test -p holt-engine --test all routines
cargo test -p holt-ui scheduled
```

The suites cover verdict parsing and the evidence gate, the caps and failure taxonomy, continuation sweeping and pause-wins, restart reconciliation, cron/`at` scheduling and the coalesced Catch-up, run lifecycle (waiting on gate and question, skipped fires, interrupted-at-launch), agent-created schedules (approval, idempotent retry, DST-safe relative delays, and actual fire), and the settings records' tolerant loads.
