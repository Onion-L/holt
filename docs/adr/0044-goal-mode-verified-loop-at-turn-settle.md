# Goal mode: a chat-scoped loop with an evidence-based verifier at Turn settle

## Context

Agent harnesses converge on a "goal mode": the user states an objective, the
agent loops on it, and an independent per-Turn check decides whether the work
verifiably holds. The consensus skeleton: a durable chat-scoped objective, an
evidence-only evaluation after each Turn, a small state machine
(active/paused/blocked), hard iteration caps, and interrupt-means-pause.

Two Holt specifics shape the port. First, the queue is durable, visible, and
user-editable (ADR-0021), so a continuation that rides a hidden channel would
be an alien mechanism — the user could see Turns starting with no row to
inspect or delete. Second, ADR-0024 already freezes each Turn's change set at
settle, so the verifier gets real workspace evidence (file status and line
counts) instead of judging from conversation text alone, which is what every
other harness feeds its evaluator.

Review surfaced three traps the design must close: the evaluation must not
reorder the settled Turn's card frame against its verdict marker (the
`final_signal` grace exists for exactly this ordering); the restart invariant
is NOT "the queue reloads paused" — `Queue::load` clears the pause of an
empty queue — but "a continuation exists only because a Turn settle enqueued
it"; and the usage batch settles BEFORE the terminal event publishes, so a
verifier booking through the Turn's capture buffer would silently lose its
record on the chat's last Turn.

## Decisions

- **The goal is chat-scoped durable state on the chat row** (`Chat::goal`,
  following `plan_mode`'s precedent): the objective text, a status
  (`active`/`paused`/`blocked`), the iteration and no-progress counters, a
  consecutive evaluation-failure counter, `startedAt`, and the last verdict's
  reason. There is no verdict history store — terminal transitions land as
  Transcript `Notice` rows (the reader-facing housekeeping row, ADR-0010),
  so history survives restarts through the record that already persists.
- **The verifier is one model pass at Turn settle, after
  `clear_final_signal`, inside the same queue-driver iteration** — before
  the next pick, so the continuation it may enqueue cannot race the next
  admission, and after the settled card's frame grace, so the verdict's
  Notice can never overtake the Turn's own card. The pass rides the chat's
  own model and transport (the auto-review `ReviewTransport` shape: one
  completion, no tools, a small max-tokens cap, silent provider retries).
  Its input is the goal text, a byte-capped Transcript tail, the Turn's tool
  calls, and the settled Turn's frozen change-set summary. The rubric
  admits only concrete evidence — files changed, command output, test
  results; plans and intentions do not count. It answers exactly one line:
  `COMPLETE: <evidence>`, `CONTINUE: <the missing piece>`, or
  `BLOCKED: <reason>`; a bare `COMPLETE` without evidence is no verdict at
  all — clearing the objective is the one destructive transition, so it
  must cite what settled it.
- **Evaluation happens only when the queue holds no pending item at
  settle.** Any pending row — user-typed or an orphaned continuation — means
  the next step is already spoken for: the user queueing work is the user
  taking the wheel, and the loop resumes judging at the first settle that
  finds the queue empty. A Turn that ends on an unanswered question card
  (`has_pending_question`, the Routine `Waiting` precedent) is not
  evaluated either; the settle after the answer is. Open approvals need no
  check: the gate blocks the loop, so a Succeeded Turn cannot carry one.
- **A `CONTINUE` verdict enqueues an ordinary, visible queue item** — the
  plan follow-up's path — carrying the goal text and the verifier's reason,
  flagged `goalContinuation` on the `PendingMessage` (additive,
  serde-defaulted). Deleting that item pauses the goal; editing it keeps
  the flag. `ClearGoal` or pausing sweeps every goal-flagged pending row,
  so "off" never leaves a last Turn to run. A continuation landing in a
  paused queue (ADR-0021's attended-send re-pause included) flips the goal
  to paused — the queue's pause wins.
- **Caps: `MAX_ITERATIONS = 20` is the primary backstop; no-progress is the
  secondary one.** A Succeeded Turn with zero tool calls and an empty
  change set increments `noProgress` (anything else resets it); at 3 the
  goal pauses. Zero-tool-call is deliberately weak — a research goal reads
  files for many Turns legitimately, and a wrong pause costs more than a
  few extra iterations under the hard cap.
- **Failure taxonomy.** Turn interrupted → goal paused (the user stopped
  it; resumption is explicit). Turn failed — a driver-iteration panic
  included — → goal paused, kept. A Message
  item's admission failure (the model or credential is gone) → goal
  cleared with a Notice — retrying cannot fix a missing model; a manual
  Compaction's failure on the same `DriverOutcome::Settled` path never
  touches the goal. A garbled verdict and a failed evaluation call are the
  SAME failure: `evalFailures` increments (a successful verdict resets it)
  and the loop keeps moving — the next step is queued so the following
  settle re-verifies, never leaving an `active` goal with nothing queued;
  at 3 the goal pauses with a Notice.
- **The evaluation is bounded and not user-interruptible.** The Turn's
  cancel token is already cleared at settle, and Stop between Turns pauses
  the queue only. The verifier rides its own `CancellationToken` (stored on
  the `ChatRuntime` like the Title task's), cancelled by `ClearGoal`,
  pausing, `SetGoal` replacing the objective, chat deletion, shutdown, and
  a new Turn's admission superseding an in-flight check. Inline placement
  is chosen for ordering determinism; `catch_unwind` covers a panic; the
  token-and-retry caps bound the wait.
- **Restart reconciliation is lazy, at runtime open** (`AgentRuntime::chat`
  — before then nothing can run the queue anyway): an `active` goal whose
  queue holds no goal-flagged pending or started item flips to `paused`.
  The durable invariant is that continuations are born only at a Turn
  settle, never revived.
- **Mutual exclusion is rejection, both ways** — a deliberate break from
  Provider Mode's "entering exits the other": a goal is durable state with
  a queue footprint, and silently clearing it costs the objective, the
  counters, and any queued continuation. `SetGoal` rejects a planning,
  provider-mode, or routine-run chat; entering Plan or Provider Mode
  rejects a chat with any goal state.
- **Usage books through `record_immediate` under a new `goal-check`
  kind** — never the Turn's capture buffer, whose batch settled before the
  verifier ran (the Title task's precedent).
- **The UI is a slash entry plus a composer chip, not a mode.** `/goal`
  joins `ListCommands` with an input hint. `/goal <objective>` arms the
  goal and sends the objective as the first Turn's message — the
  objective IS the task, and `SetGoal` rides after createChat, before the
  queue (the plan enter's shape), so the new-chat canvas works and a
  failed `SetGoal` aborts the send with the directive restored. `/goal
  off`, `/goal pause`, and `/goal resume` drive the loop on an existing
  chat. The chip rides `WatchChats` like Plan
  Mode's, shows the iteration count, and its ×
  clears. Resume from `paused` or `blocked` sets `active`, grants a fresh
  budget (the counters reset), and enqueues a continuation when the chat
  is idle, so the loop visibly restarts. Goal
  mode changes no tool gating, no system prompt, and no input behavior —
  it is a process attached to the chat, not a state the user is in.

## Consequences

The loop runs with no new transport, store, or wire type beyond the
chat-row field and the queue flag: state rides `WatchChats`, verdicts ride
`Notice` rows old builds already render, and the continuation is a queue
row the user can inspect, edit, or delete. The verifier's one pass per
evaluated Turn is billed visibly under its own usage kind. A goal never
survives what should kill it (a missing model clears it; an interrupt
pauses it), and never spins past the iteration cap even if the verifier
keeps finding "one more thing". Adding a `MessagePart` variant was
considered for structured verdicts and rejected: it would touch parts,
schema, salvage, and every renderer for a row `Notice` already expresses,
and old builds would read the new kind through their unknown-kind fallback.
A configurable verifier model (the Title-settings pattern) and a goal queue
(`/goal next`) are deliberate follow-ups, not part of this ADR.
